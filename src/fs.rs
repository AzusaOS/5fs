//! Mounted-filesystem access: address resolution, inodes, the v0 allocator
//! (level-0 cells only), embedded directories, file read, and offline import.

use crate::device::Device;
use crate::fmt::*;
use anyhow::{anyhow, bail, Result};
use std::path::Path;

pub struct Gofs {
    pub dev: Device,
    pub sb: Superblock,
    pub map: AgMap,
}

impl Gofs {
    pub fn open(path: &Path, writable: bool) -> Result<Self> {
        let dev = Device::open(path, writable)?;
        let mut sbbuf = [0u8; SB_SIZE];
        dev.pread(&mut sbbuf, 0)?;
        let sb = Superblock::parse(&sbbuf).map_err(|e| anyhow!(e))?;
        let map = Self::load_agmap(&dev, &sb)?.0;
        Ok(Gofs { dev, sb, map })
    }

    /// Load both AG map copies; return the one with the highest valid
    /// generation plus per-copy status for fsck.
    pub fn load_agmap(dev: &Device, sb: &Superblock) -> Result<(AgMap, [Result<u64, String>; 2])> {
        let mut best: Option<AgMap> = None;
        let mut status: [Result<u64, String>; 2] =
            [Err("unread".into()), Err("unread".into())];
        for copy in 0..2 {
            let off = sb.agmap_offset + copy as u64 * sb.agmap_length;
            let mut buf = vec![0u8; sb.agmap_length as usize];
            if let Err(e) = dev.pread(&mut buf, off) {
                status[copy] = Err(e.to_string());
                continue;
            }
            match AgMap::parse(&buf) {
                Ok(m) => {
                    status[copy] = Ok(m.gen);
                    if best.as_ref().map_or(true, |b| m.gen > b.gen) {
                        best = Some(m);
                    }
                }
                Err(e) => status[copy] = Err(e),
            }
        }
        Ok((best.ok_or_else(|| anyhow!("no valid AG map copy"))?, status))
    }

    pub fn blocksize(&self) -> u32 {
        self.sb.blocksize
    }

    pub fn resolve(&self, blk: u64) -> Result<u64> {
        self.map
            .resolve(blk, self.sb.blocksize)
            .ok_or_else(|| anyhow!("unmapped block address {blk:#x}"))
    }

    pub fn read_block(&self, blk: u64) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; self.sb.blocksize as usize];
        let off = self.resolve(blk)?;
        self.dev.pread(&mut buf, off)?;
        Ok(buf)
    }

    pub fn write_block(&self, blk: u64, data: &[u8]) -> Result<()> {
        let off = self.resolve(blk)?;
        self.dev.pwrite(data, off)
    }

    // --- AG headers -----------------------------------------------------------

    /// AG 0's header sits at local block 1 (block 0 is the superblock).
    pub fn ag_header_block(ag: u32) -> u32 {
        if ag == 0 {
            1
        } else {
            0
        }
    }

    pub fn read_ag_header(&self, ag: u32) -> Result<AgHeader> {
        let blk = blk_addr(ag, Self::ag_header_block(ag));
        let buf = self.read_block(blk)?;
        AgHeader::parse(&buf).map_err(|e| anyhow!("AG {ag}: {e}"))
    }

    pub fn write_ag_header(&self, hdr: &AgHeader) -> Result<()> {
        let mut buf = vec![0u8; self.sb.blocksize as usize];
        buf[..AGHDR_SIZE].copy_from_slice(&hdr.to_bytes());
        self.write_block(blk_addr(hdr.ag_num, Self::ag_header_block(hdr.ag_num)), &buf)
    }

    // --- allocator root table ---------------------------------------------------

    pub fn read_root_table(&self, hdr: &AgHeader) -> Result<Vec<u8>> {
        let (_cells, rt_blocks) = rt_geometry(hdr.length, self.sb.blocksize);
        let mut buf = Vec::with_capacity((rt_blocks * self.sb.blocksize) as usize);
        for i in 0..rt_blocks {
            buf.extend_from_slice(&self.read_block(blk_addr(hdr.ag_num, hdr.alloc_root + i))?);
        }
        Ok(buf)
    }

    pub fn write_root_table(&self, hdr: &AgHeader, table: &[u8]) -> Result<()> {
        let bs = self.sb.blocksize as usize;
        for (i, chunk) in table.chunks(bs).enumerate() {
            let mut blkbuf = vec![0u8; bs];
            blkbuf[..chunk.len()].copy_from_slice(chunk);
            self.write_block(blk_addr(hdr.ag_num, hdr.alloc_root + i as u32), &blkbuf)?;
        }
        Ok(())
    }

    /// Allocate `count` consecutive FREE level-0 cells in `ag`, marking them
    /// FULL. Returns the block address of the first block. v0: no refinement,
    /// so the allocation unit is a whole cell.
    pub fn alloc_cells(&mut self, ag: u32, count: u64) -> Result<u64> {
        let mut hdr = self.read_ag_header(ag)?;
        let (cells, _) = rt_geometry(hdr.length, self.sb.blocksize);
        let mut table = self.read_root_table(&hdr)?;
        let mut run_start = 0u64;
        let mut run = 0u64;
        let mut found = None;
        for c in 0..cells {
            if cell_get(&table, c) == CELL_FREE {
                if run == 0 {
                    run_start = c;
                }
                run += 1;
                if run == count {
                    found = Some(run_start);
                    break;
                }
            } else {
                run = 0;
            }
        }
        let start = found.ok_or_else(|| anyhow!("AG {ag}: no room for {count} cells"))?;
        for c in start..start + count {
            cell_set(&mut table, c, CELL_FULL);
        }
        let blocks = (count * CELL_BLOCKS as u64) as u32;
        hdr.free_blocks -= blocks;
        hdr.full_blocks += blocks;
        hdr.gen += 1;
        self.write_root_table(&hdr, &table)?;
        self.write_ag_header(&hdr)?;
        self.sb.free_blocks -= blocks as u64;
        self.sb.full_blocks += blocks as u64;
        self.write_superblock()?;
        Ok(blk_addr(ag, (start * CELL_BLOCKS as u64) as u32))
    }

    pub fn write_superblock(&mut self) -> Result<()> {
        self.sb.gen += 1;
        let bytes = self.sb.to_bytes();
        self.dev.pwrite(&bytes, 0)?;
        // backup at the last block of AG 0's first segment
        if let Some(e) = self.map.entries.first() {
            if let Some(s) = e.segs.first() {
                let off = s.dev_offset + (s.blocks as u64 - 1) * self.sb.blocksize as u64;
                self.dev.pwrite(&bytes, off)?;
            }
        }
        Ok(())
    }

    // --- inodes ----------------------------------------------------------------

    pub fn inode_phys(&self, ino: u64) -> Result<u64> {
        let (ag, slot) = blk_split(ino);
        let byte_off = slot as u64 * self.sb.inodesize as u64;
        let bs = self.sb.blocksize as u64;
        let blk = blk_addr(ag, (byte_off / bs) as u32);
        Ok(self.resolve(blk)? + byte_off % bs)
    }

    pub fn read_inode(&self, ino: u64) -> Result<Inode> {
        let mut buf = [0u8; INODE_SIZE];
        self.dev.pread(&mut buf, self.inode_phys(ino)?)?;
        Inode::parse(&buf).map_err(|e| anyhow!("inode {ino:#x}: {e}"))
    }

    pub fn write_inode(&self, ino: u64, inode: &Inode) -> Result<()> {
        self.dev.pwrite(&inode.to_bytes(), self.inode_phys(ino)?)
    }

    /// v0: inodes live in the reserved inode block that also holds the root
    /// inode. Find a free (zeroed) slot there.
    pub fn alloc_inode_slot(&self) -> Result<u64> {
        let (ag, root_slot) = blk_split(self.sb.root_ino);
        let slots_per_block = (self.sb.blocksize / self.sb.inodesize as u32) as u32;
        let base = root_slot - root_slot % slots_per_block;
        for s in base..base + slots_per_block {
            let ino = ino_addr(ag, s);
            let mut buf = [0u8; INODE_SIZE];
            self.dev.pread(&mut buf, self.inode_phys(ino)?)?;
            if buf.iter().all(|&b| b == 0) {
                return Ok(ino);
            }
        }
        bail!("no free inode slot (v0 single inode block)")
    }

    // --- directories (EMBED only in v0) ------------------------------------------

    pub fn dir_entries(&self, ino: u64) -> Result<Vec<DirEntry>> {
        let inode = self.read_inode(ino)?;
        if !inode.is_dir() {
            bail!("inode {ino:#x} is not a directory");
        }
        match inode.format {
            FMT_EMPTY => Ok(Vec::new()),
            FMT_EMBED => Ok(dir_parse(&inode.payload)),
            f => bail!("directory format {f} not supported yet"),
        }
    }

    pub fn dir_lookup(&self, dir: u64, name: &str) -> Result<Option<u64>> {
        Ok(self.dir_entries(dir)?.into_iter().find(|e| e.name == name).map(|e| e.ino))
    }

    // --- file data -----------------------------------------------------------------

    pub fn read_file(&self, ino: u64) -> Result<Vec<u8>> {
        let inode = self.read_inode(ino)?;
        match inode.format {
            FMT_EMPTY => Ok(Vec::new()),
            FMT_EMBED => Ok(inode.payload[..inode.size as usize].to_vec()),
            FMT_EXTENT => {
                let bs = self.sb.blocksize as u64;
                let mut data = vec![0u8; (inode.size.div_ceil(bs) * bs) as usize];
                for e in extents_parse(&inode.payload) {
                    let (ag, local) = blk_split(e.blk);
                    for i in 0..e.blocks {
                        let blk = self.read_block(blk_addr(ag, local + i))?;
                        let off = ((e.file_block + i as u64) * bs) as usize;
                        data[off..off + bs as usize].copy_from_slice(&blk);
                    }
                }
                data.truncate(inode.size as usize);
                Ok(data)
            }
            f => bail!("file format {f} not supported yet"),
        }
    }

    /// Offline import: store `data` as a new file named `name` in the root
    /// directory. EMBED if it fits the payload, otherwise inline extents over
    /// whole level-0 cells (v0: no refinement, so small files round up to a
    /// cell — see doc/2-allocation.md for what replaces this).
    pub fn import(&mut self, name: &str, data: &[u8], mode: u16) -> Result<u64> {
        if !self.dev.writable {
            bail!("image opened read-only");
        }
        if self.dir_lookup(self.sb.root_ino, name)?.is_some() {
            bail!("{name}: already exists");
        }
        let now = Ts::now();
        let ino = self.alloc_inode_slot()?;
        let mut inode = Inode {
            format: FMT_EMBED,
            mode: 0o100000 | (mode & 0o7777),
            nlink: 1,
            size: data.len() as u64,
            atime: now,
            mtime: now,
            ctime: now,
            btime: now,
            ..Default::default()
        };
        if data.is_empty() {
            inode.format = FMT_EMPTY;
        } else if data.len() <= INODE_PAYLOAD {
            inode.payload[..data.len()].copy_from_slice(data);
        } else {
            let bs = self.sb.blocksize as u64;
            let cell_bytes = CELL_BLOCKS as u64 * bs;
            let cells = (data.len() as u64).div_ceil(cell_bytes);
            let (ag, _) = blk_split(ino);
            let blk = self.alloc_cells(ag, cells)?;
            // write data, zero-padding the tail block
            let nblocks = (data.len() as u64).div_ceil(bs);
            let mut padded = data.to_vec();
            padded.resize((nblocks * bs) as usize, 0);
            self.dev.pwrite(&padded, self.resolve(blk)?)?;
            inode.format = FMT_EXTENT;
            inode.nblocks = cells * CELL_BLOCKS as u64;
            extents_store(&mut inode.payload, &[Extent {
                file_block: 0,
                blk,
                blocks: nblocks as u32,
            }])
            .map_err(|e| anyhow!(e))?;
        }
        self.write_inode(ino, &inode)?;

        let mut root = self.read_inode(self.sb.root_ino)?;
        if root.format == FMT_EMPTY {
            root.format = FMT_EMBED;
            root.payload.fill(0);
        }
        dir_append(&mut root.payload, &DirEntry { ino, ftype: DT_FILE, name: name.into() })
            .map_err(|e| anyhow!(e))?;
        root.mtime = now;
        root.ctime = now;
        self.write_inode(self.sb.root_ino, &root)?;
        self.dev.sync()?;
        Ok(ino)
    }
}
