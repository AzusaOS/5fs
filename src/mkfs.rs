//! Create a 5FS v2 filesystem. Layout per doc/1-layout.md.
//!
//! AG 0 reserved head: superblock, AG header, allocator root table, AG map
//! copies, journal, optional kernel region. The first two cells after the
//! head are the initial allocator table arena and the inode cell, whose
//! first block is slot-refined (L0 -> L1 -> L3 records built by hand here)
//! and holds the root inode in slot 0 (and the kernel inode in slot 1).

use crate::alloc::REC_SIZE;
use crate::device::Device;
use crate::fmt::*;
use anyhow::{bail, Result};
use std::path::Path;

pub struct MkfsOpts {
    /// Image size; None = use existing file/device size.
    pub size: Option<u64>,
    pub blocksize: u32,
    pub inodesize: u16,
    /// Journal size in bytes; None = default (128 MiB capped to size/16, min 4 MiB).
    pub journal: Option<u64>,
    pub label: String,
    /// Kernel image to store contiguously for the bootloader (doc/1-layout.md).
    pub kernel: Option<Vec<u8>>,
}

impl Default for MkfsOpts {
    fn default() -> Self {
        MkfsOpts {
            size: None,
            blocksize: DEFAULT_BLOCKSIZE,
            inodesize: DEFAULT_INODESIZE,
            journal: None,
            label: String::new(),
            kernel: None,
        }
    }
}

pub struct MkfsSummary {
    pub size: u64,
    pub blocks: u64,
    pub ags: u32,
    pub journal_blocks: u64,
    pub root_ino: u64,
    pub free_blocks: u64,
    pub kernel_offset: u64,
}

/// Maximum AG size we create: 64 GiB (doc/1-layout.md recommended cap).
const MAX_AG_BYTES: u64 = 64 << 30;
/// Reserved bytes for each AG map copy.
const AGMAP_BYTES: u64 = 64 << 10;

pub fn mkfs(path: &Path, opts: &MkfsOpts) -> Result<MkfsSummary> {
    let bs = opts.blocksize as u64;
    if !opts.blocksize.is_power_of_two() || opts.blocksize < 2048 {
        bail!("blocksize must be a power of two >= 2048");
    }
    if !u32::from(opts.inodesize).is_power_of_two()
        || opts.inodesize < 128
        || u32::from(opts.inodesize) > opts.blocksize
    {
        bail!("inodesize must be a power of two, 128 <= inodesize <= blocksize");
    }
    let slots = opts.blocksize / opts.inodesize as u32;
    if slots > ALLOC_FANOUT {
        bail!("blocksize/inodesize must be <= {ALLOC_FANOUT} (slot record limit)");
    }

    let dev = match opts.size {
        Some(s) => Device::create(path, s - s % bs)?,
        None => Device::open(path, true)?,
    };
    let size = dev.size - dev.size % bs;
    if size < 8 << 20 {
        bail!("device too small ({size} bytes); minimum 8 MiB");
    }

    let total_blocks = size / bs;
    let max_ag_blocks = MAX_AG_BYTES / bs;
    let journal_blocks = opts
        .journal
        .unwrap_or_else(|| (128 << 20).min((size / 16).max(4 << 20)))
        .div_ceil(bs);
    let agmap_blocks = AGMAP_BYTES.div_ceil(bs);
    let kernel_len = opts.kernel.as_ref().map(|k| k.len() as u64).unwrap_or(0);
    let kernel_blocks = kernel_len.div_ceil(bs);

    // Split the device into contiguous AGs.
    let mut ag_lengths = Vec::new();
    let mut remaining = total_blocks;
    while remaining > 0 {
        let len = remaining.min(max_ag_blocks);
        // avoid a runt AG that can't hold its own metadata
        if remaining - len > 0 && remaining - len < 1024 {
            ag_lengths.push(remaining as u32);
            break;
        }
        ag_lengths.push(len as u32);
        remaining -= len;
    }
    let ag_count = ag_lengths.len() as u32;

    let zero_block = vec![0u8; bs as usize];
    let mut map = AgMap { gen: 1, entries: Vec::new() };
    let mut dev_off = 0u64;
    let mut total_free = 0u64;
    let mut total_rsvd = 0u64;
    let mut total_full = 0u64;
    let mut root_ino = 0u64;
    let mut sb_proto = Superblock {
        blocksize: opts.blocksize,
        inodesize: opts.inodesize,
        next_ag: ag_count,
        agmap_length: AGMAP_BYTES,
        journal_length: journal_blocks,
        gen: 1,
        journal_seq: 1,
        journal_head: 0,
        ..Default::default()
    };
    sb_proto.uuid = *uuid::Uuid::new_v4().as_bytes();
    for (i, u) in opts.label.encode_utf16().take(16).enumerate() {
        sb_proto.disk_name[i] = u;
    }

    for (agi, &ag_len) in ag_lengths.iter().enumerate() {
        let ag = agi as u32;
        let (cells, rt_blocks) = rt_geometry(ag_len, opts.blocksize);
        let hdr_block = if ag == 0 { 1u32 } else { 0u32 };
        let alloc_root = hdr_block + 1;
        let mut next = alloc_root + rt_blocks; // first block past fixed metadata

        let mut kernel_start = 0u32;
        if ag == 0 {
            // AG map copies, block aligned
            sb_proto.agmap_offset = dev_off + next as u64 * bs;
            next += (2 * agmap_blocks) as u32;
            // journal
            sb_proto.journal_start = blk_addr(0, next);
            next += journal_blocks as u32;
            // contiguous kernel region
            if kernel_blocks > 0 {
                kernel_start = next;
                sb_proto.kernel_offset = next as u64 * bs;
                sb_proto.kernel_end = sb_proto.kernel_offset + kernel_len;
                next += kernel_blocks as u32;
            }
            if (next as u64) + 3 * CELL_BLOCKS as u64 >= ag_len as u64 {
                bail!("device too small for journal/kernel + metadata; use --journal-size");
            }
        }

        // Reserve [0, next) plus the backup-superblock block (AG 0) and the
        // partial tail cell, cell-granular.
        let mut table = vec![0u8; (rt_blocks as u64 * bs) as usize];
        let rsvd_head_cells = (next as u64).div_ceil(CELL_BLOCKS as u64);
        for c in 0..rsvd_head_cells {
            cell_set(&mut table, c, CELL_RSVD);
        }
        let mut rsvd_cells = rsvd_head_cells;
        if ag_len as u64 % CELL_BLOCKS as u64 != 0 {
            // partial tail cell unusable by the cell allocator
            if cell_get(&table, cells - 1) == CELL_FREE {
                cell_set(&mut table, cells - 1, CELL_RSVD);
                rsvd_cells += 1;
            }
        }
        if ag == 0 && cell_get(&table, cells - 1) == CELL_FREE {
            // backup superblock lives in the last block
            cell_set(&mut table, cells - 1, CELL_RSVD);
            rsvd_cells += 1;
        }

        let mut ino_hint = 0u32;
        let mut tbl_arena = 0u32;
        let mut full_blocks = 0u64;
        if ag == 0 {
            // arena cell + inode cell with the slot-refined inode block
            let c_arena = rsvd_head_cells;
            let c_ino = c_arena + 1;
            if c_ino + 1 >= cells {
                bail!("AG 0 too small for inode metadata");
            }
            cell_set(&mut table, c_arena, CELL_RSVD);
            rsvd_cells += 1;
            cell_set(&mut table, c_ino, CELL_REFINED);
            tbl_arena = (c_arena * CELL_BLOCKS as u64) as u32;
            ino_hint = (c_ino * CELL_BLOCKS as u64) as u32;
            // root table ref for c_ino -> record 0 of the first arena block
            let roff = rt_ref_off(cells, c_ino) as usize;
            put_u32(&mut table, roff, tbl_arena);
            put_u16(&mut table, roff + 4, 0);
            full_blocks = 1; // the slot block itself

            // arena table block: rec0 = L0(c_ino), rec1 = L1, rec2 = slots
            let mut tbl = zero_block.clone();
            tbl[0..4].copy_from_slice(&ALLOC_MAGIC);
            put_u32(&mut tbl, 16, 0b111); // records 0..2 used
            let r0 = REC_SIZE;
            cell_set(&mut tbl[r0..r0 + 4], 0, CELL_REFINED);
            put_u32(&mut tbl, r0 + 4, tbl_arena);
            put_u16(&mut tbl, r0 + 8, 1);
            let r1 = 2 * REC_SIZE;
            cell_set(&mut tbl[r1..r1 + 4], 0, CELL_REFINED);
            put_u32(&mut tbl, r1 + 4, tbl_arena);
            put_u16(&mut tbl, r1 + 8, 2);
            let r2 = 3 * REC_SIZE;
            cell_set(&mut tbl[r2..r2 + 4], 0, CELL_FULL); // root inode
            if kernel_blocks > 0 {
                cell_set(&mut tbl[r2..r2 + 4], 1, CELL_FULL); // kernel inode
            }
            let c = csum(&tbl, 4);
            put_u32(&mut tbl, 4, c);
            dev.pwrite(&tbl, dev_off + tbl_arena as u64 * bs)?;
        }

        let free_cells = cells - rsvd_cells - if ag == 0 { 1 } else { 0 };
        let mut free_blocks = free_cells * CELL_BLOCKS as u64;
        if ag == 0 {
            free_blocks += CELL_BLOCKS as u64 - 1; // inode cell minus slot block
        }
        let rsvd_blocks = ag_len as u64 - free_blocks - full_blocks;

        // write metadata blocks (zero first, then content)
        for b in 0..next {
            dev.pwrite(&zero_block, dev_off + b as u64 * bs)?;
        }
        let hdr = AgHeader {
            ag_num: ag,
            gen: 1,
            length: ag_len,
            free_blocks: free_blocks as u32,
            rsvd_blocks: rsvd_blocks as u32,
            full_blocks: full_blocks as u32,
            alloc_root,
            ino_hint,
            data_hint: (rsvd_head_cells * CELL_BLOCKS as u64) as u32,
            tbl_arena,
        };
        let mut hdrblk = zero_block.clone();
        hdrblk[..AGHDR_SIZE].copy_from_slice(&hdr.to_bytes());
        dev.pwrite(&hdrblk, dev_off + hdr_block as u64 * bs)?;
        for (i, chunk) in table.chunks(bs as usize).enumerate() {
            let mut blk = zero_block.clone();
            blk[..chunk.len()].copy_from_slice(chunk);
            dev.pwrite(&blk, dev_off + (alloc_root + i as u32) as u64 * bs)?;
        }

        if ag == 0 {
            let now = Ts::now();
            root_ino = ino_addr(0, ino_hint * slots);
            let mut root = Inode {
                format: FMT_EMPTY,
                mode: 0o040755,
                nlink: 2,
                atime: now,
                mtime: now,
                ctime: now,
                btime: now,
                ..Default::default()
            };
            dev.pwrite(&zero_block, dev_off + ino_hint as u64 * bs)?;
            if let Some(kernel) = &opts.kernel {
                // kernel data, zero-padding the tail block
                let mut padded = kernel.clone();
                padded.resize((kernel_blocks * bs) as usize, 0);
                dev.pwrite(&padded, dev_off + kernel_start as u64 * bs)?;
                // immutable kernel.bin inode in slot 1
                let kino = root_ino + 1;
                let mut ki = Inode {
                    format: FMT_EXTENT,
                    mode: 0o100644,
                    nlink: 1,
                    flags: INODE_FLAG_IMMUTABLE,
                    size: kernel_len,
                    nblocks: kernel_blocks,
                    atime: now,
                    mtime: now,
                    ctime: now,
                    btime: now,
                    ..Default::default()
                };
                extents_store(
                    &mut ki.payload,
                    &[Extent {
                        file_block: 0,
                        blk: blk_addr(0, kernel_start),
                        blocks: kernel_blocks as u32,
                    }],
                )
                .map_err(anyhow::Error::msg)?;
                dev.pwrite(
                    &ki.to_bytes(),
                    dev_off + ino_hint as u64 * bs + opts.inodesize as u64,
                )?;
                root.format = FMT_EMBED;
                dir_append(
                    &mut root.payload,
                    &DirEntry { ino: kino, ftype: DT_FILE, name: "kernel.bin".into() },
                )
                .map_err(anyhow::Error::msg)?;
            }
            dev.pwrite(&root.to_bytes(), dev_off + ino_hint as u64 * bs)?;
        }

        map.entries.push(AgEntry {
            flags: AGF_PRESENT | if ag == 0 { AGF_IMMOVABLE } else { 0 },
            length: ag_len,
            segs: vec![AgSegment { ag_block: 0, blocks: ag_len, dev_offset: dev_off }],
        });
        total_free += free_blocks;
        total_rsvd += rsvd_blocks;
        total_full += full_blocks;
        dev_off += ag_len as u64 * bs;
    }

    // AG map copies (identical at mkfs)
    let map_bytes = map.to_bytes(AGMAP_BYTES as usize).map_err(anyhow::Error::msg)?;
    dev.pwrite(&map_bytes, sb_proto.agmap_offset)?;
    dev.pwrite(&map_bytes, sb_proto.agmap_offset + AGMAP_BYTES)?;

    // journal: zeroed = empty
    let jstart = sb_proto.journal_start & 0xffff_ffff;
    for b in 0..journal_blocks {
        dev.pwrite(&zero_block, (jstart + b) * bs)?;
    }

    sb_proto.root_ino = root_ino;
    sb_proto.free_blocks = total_free;
    sb_proto.rsvd_blocks = total_rsvd;
    sb_proto.full_blocks = total_full;
    let sb_bytes = sb_proto.to_bytes();
    dev.pwrite(&sb_bytes, 0)?;
    // backup superblock at the last block of AG 0
    dev.pwrite(&sb_bytes, (ag_lengths[0] as u64 - 1) * bs)?;
    dev.sync()?;

    Ok(MkfsSummary {
        size,
        blocks: total_blocks,
        ags: ag_count,
        journal_blocks,
        root_ino,
        free_blocks: total_free,
        kernel_offset: sb_proto.kernel_offset,
    })
}
