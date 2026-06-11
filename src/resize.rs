//! Online resize primitives (doc/7-resize.md): grow, wholesale AG
//! relocation, retiring empty AGs, and tail shrink. All of these touch only
//! the AG map and superblock — never file metadata.

use crate::fmt::*;
use crate::fs::Gofs;
use anyhow::{bail, Result};

impl Gofs {
    /// Add an AG covering [dev_offset, dev_offset + bytes). The region must
    /// be unmapped. Returns the new AG id.
    pub fn add_ag(&mut self, dev_offset: u64, bytes: u64) -> Result<u32> {
        let bs = self.sb.blocksize as u64;
        if dev_offset % bs != 0 {
            bail!("AG offset must be block aligned");
        }
        let ag_len = (bytes / bs) as u32;
        if ag_len < 1024 {
            bail!("AG too small ({ag_len} blocks; minimum 1024)");
        }
        if self.overlaps(dev_offset, bytes) {
            bail!("region overlaps an existing AG");
        }
        let ag = self.sb.next_ag;
        let (cells, rt_blocks) = rt_geometry(ag_len, self.sb.blocksize);
        let alloc_root = 1u32;
        let meta_blocks = alloc_root + rt_blocks;

        // metadata-covering cells + partial tail cell are reserved
        let mut table = vec![0u8; (rt_blocks as u64 * bs) as usize];
        let head_cells = (meta_blocks as u64).div_ceil(CELL_BLOCKS as u64);
        for c in 0..head_cells {
            cell_set(&mut table, c, CELL_RSVD);
        }
        let mut rsvd_cells = head_cells;
        if ag_len as u64 % CELL_BLOCKS as u64 != 0 && cell_get(&table, cells - 1) == CELL_FREE {
            cell_set(&mut table, cells - 1, CELL_RSVD);
            rsvd_cells += 1;
        }
        let free_blocks = (cells - rsvd_cells) * CELL_BLOCKS as u64;

        let hdr = AgHeader {
            ag_num: ag,
            gen: 1,
            length: ag_len,
            free_blocks: free_blocks as u32,
            rsvd_blocks: ag_len - free_blocks as u32,
            full_blocks: 0,
            alloc_root,
            ino_hint: 0,
            data_hint: (head_cells * CELL_BLOCKS as u64) as u32,
            tbl_arena: 0,
        };
        // write the AG body directly: it is unreachable until the map commits
        let mut hdrblk = vec![0u8; bs as usize];
        hdrblk[..AGHDR_SIZE].copy_from_slice(&hdr.to_bytes());
        self.dev.pwrite(&hdrblk, dev_offset)?;
        self.dev.pwrite(&table, dev_offset + alloc_root as u64 * bs)?;
        self.dev.sync()?;

        self.map.entries.push(AgEntry {
            flags: AGF_PRESENT,
            length: ag_len,
            segs: vec![AgSegment { ag_block: 0, blocks: ag_len, dev_offset }],
        });
        self.sb.next_ag = ag + 1;
        self.sb.free_blocks += free_blocks;
        self.sb.rsvd_blocks += (ag_len as u64) - free_blocks;
        self.write_agmap()?;
        self.write_superblock()?;
        self.dev.sync()?;
        Ok(ag)
    }

    /// Grow the filesystem (and a file-backed device) to `new_size` bytes.
    pub fn grow(&mut self, new_size: u64) -> Result<u32> {
        let end = self.mapped_end();
        if new_size <= end {
            bail!("new size {new_size} does not extend past current end {end}");
        }
        if new_size > self.dev.size {
            self.dev.set_len(new_size)?;
        }
        self.add_ag(end, new_size - end)
    }

    /// Move an entire AG to `dest` (unmapped, sized region). Only the AG map
    /// changes; every block address and inode number stays valid.
    pub fn relocate(&mut self, ag: u32, dest: u64) -> Result<()> {
        let bs = self.sb.blocksize as u64;
        if dest % bs != 0 {
            bail!("destination must be block aligned");
        }
        let e = self
            .map
            .entries
            .get(ag as usize)
            .ok_or_else(|| anyhow::anyhow!("no such AG"))?
            .clone();
        if e.flags & AGF_PRESENT == 0 {
            bail!("AG {ag} is not present");
        }
        if e.flags & AGF_IMMOVABLE != 0 {
            bail!("AG {ag} is immovable");
        }
        let bytes = e.length as u64 * bs;
        if dest + bytes > self.dev.size {
            bail!("destination past end of device");
        }
        if self.overlaps_excluding(dest, bytes, ag) {
            bail!("destination overlaps a mapped region");
        }
        // copy each segment's data to its place in the new contiguous home
        let mut buf = vec![0u8; (CELL_BLOCKS as u64 * bs) as usize];
        for s in &e.segs {
            let mut done = 0u64;
            let total = s.blocks as u64 * bs;
            while done < total {
                let n = buf.len().min((total - done) as usize);
                self.dev.pread(&mut buf[..n], s.dev_offset + done)?;
                self.dev
                    .pwrite(&buf[..n], dest + s.ag_block as u64 * bs + done)?;
                done += n as u64;
            }
        }
        self.dev.sync()?;
        self.map.entries[ag as usize].segs =
            vec![AgSegment { ag_block: 0, blocks: e.length, dev_offset: dest }];
        self.write_agmap()?;
        self.write_superblock()?;
        self.dev.sync()?;
        Ok(())
    }

    /// Retire an empty AG: its id is never reused, its space is released.
    pub fn retire(&mut self, ag: u32) -> Result<()> {
        if ag == 0 {
            bail!("AG 0 cannot be retired");
        }
        let hdr = self.read_ag_header(ag)?;
        if hdr.full_blocks != 0 {
            bail!("AG {ag} still has {} allocated blocks", hdr.full_blocks);
        }
        let e = &mut self.map.entries[ag as usize];
        if e.flags & AGF_PRESENT == 0 {
            bail!("AG {ag} is not present");
        }
        e.flags = AGF_RETIRED;
        e.segs.clear();
        self.sb.free_blocks -= hdr.free_blocks as u64;
        self.sb.rsvd_blocks -= hdr.rsvd_blocks as u64;
        self.write_agmap()?;
        self.write_superblock()?;
        self.dev.sync()?;
        Ok(())
    }

    /// Shrink the device to `new_size`: every AG beyond it is retired (if
    /// empty) or relocated into a gap below, then the file is truncated.
    pub fn shrink(&mut self, new_size: u64) -> Result<()> {
        let bs = self.sb.blocksize as u64;
        loop {
            let mut moved = false;
            for ag in 0..self.map.entries.len() as u32 {
                let e = self.map.entries[ag as usize].clone();
                if e.flags & AGF_PRESENT == 0 {
                    continue;
                }
                let beyond =
                    e.segs.iter().any(|s| s.dev_offset + s.blocks as u64 * bs > new_size);
                if !beyond {
                    continue;
                }
                if ag == 0 {
                    bail!("cannot shrink into AG 0");
                }
                let hdr = self.read_ag_header(ag)?;
                if hdr.full_blocks == 0 {
                    self.retire(ag)?;
                } else {
                    let bytes = e.length as u64 * bs;
                    let dest = self
                        .find_gap(bytes, new_size)
                        .ok_or_else(|| anyhow::anyhow!("no room below {new_size} for AG {ag}"))?;
                    self.relocate(ag, dest)?;
                }
                moved = true;
                break;
            }
            if !moved {
                break;
            }
        }
        self.dev.set_len(new_size)?;
        Ok(())
    }

    fn mapped_end(&self) -> u64 {
        let bs = self.sb.blocksize as u64;
        self.map
            .entries
            .iter()
            .flat_map(|e| e.segs.iter())
            .map(|s| s.dev_offset + s.blocks as u64 * bs)
            .max()
            .unwrap_or(0)
    }

    fn overlaps(&self, off: u64, len: u64) -> bool {
        self.overlaps_excluding(off, len, u32::MAX)
    }

    fn overlaps_excluding(&self, off: u64, len: u64, skip_ag: u32) -> bool {
        let bs = self.sb.blocksize as u64;
        self.map.entries.iter().enumerate().any(|(i, e)| {
            i as u32 != skip_ag
                && e.segs.iter().any(|s| {
                    let s0 = s.dev_offset;
                    let s1 = s.dev_offset + s.blocks as u64 * bs;
                    off < s1 && s0 < off + len
                })
        })
    }

    /// Find a block-aligned unmapped gap of `bytes` ending at or before `limit`.
    pub fn find_gap(&self, bytes: u64, limit: u64) -> Option<u64> {
        let bs = self.sb.blocksize as u64;
        let mut segs: Vec<(u64, u64)> = self
            .map
            .entries
            .iter()
            .flat_map(|e| e.segs.iter())
            .map(|s| (s.dev_offset, s.dev_offset + s.blocks as u64 * bs))
            .collect();
        segs.sort_unstable();
        let mut cursor = 0u64;
        for (s0, s1) in segs {
            if s0 > cursor && s0.min(limit).saturating_sub(cursor) >= bytes {
                return Some(cursor);
            }
            cursor = cursor.max(s1);
        }
        if limit.saturating_sub(cursor) >= bytes {
            return Some(cursor);
        }
        None
    }
}
