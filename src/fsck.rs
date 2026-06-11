//! Structural filesystem check (read-only).

use crate::device::Device;
use crate::fmt::*;
use crate::fs::Gofs;
use anyhow::{anyhow, Result};
use std::path::Path;

#[derive(Default)]
pub struct Report {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub info: Vec<String>,
}

impl Report {
    fn err(&mut self, m: impl Into<String>) {
        self.errors.push(m.into());
    }
    fn warn(&mut self, m: impl Into<String>) {
        self.warnings.push(m.into());
    }
    pub fn clean(&self) -> bool {
        self.errors.is_empty()
    }
}

pub fn check(path: &Path) -> Result<Report> {
    let mut r = Report::default();
    let dev = Device::open(path, false)?;

    // superblock
    let mut sbbuf = [0u8; SB_SIZE];
    dev.pread(&mut sbbuf, 0)?;
    let sb = Superblock::parse(&sbbuf).map_err(|e| anyhow!(e))?;
    r.info.push(format!(
        "superblock: v2, blocksize {}, inodesize {}, label \"{}\", gen {}",
        sb.blocksize,
        sb.inodesize,
        sb.label(),
        sb.gen
    ));
    if sb.incompat != 0 {
        r.err(format!("unknown incompat feature flags {:#x}", sb.incompat));
        return Ok(r);
    }

    // AG map copies
    let (map, status) = Gofs::load_agmap(&dev, &sb)?;
    for (i, s) in status.iter().enumerate() {
        match s {
            Ok(gen) => r.info.push(format!("ag map copy {i}: valid, gen {gen}")),
            Err(e) => r.warn(format!("ag map copy {i}: {e}")),
        }
    }
    if map.entries.len() != sb.next_ag as usize {
        r.warn(format!(
            "ag map has {} entries, superblock next_ag is {}",
            map.entries.len(),
            sb.next_ag
        ));
    }

    // backup superblock (last block of AG 0's first segment)
    if let Some(s) = map.entries.first().and_then(|e| e.segs.first()) {
        let off = s.dev_offset + (s.blocks as u64 - 1) * sb.blocksize as u64;
        let mut bak = [0u8; SB_SIZE];
        if dev.pread(&mut bak, off).is_err() {
            r.err("backup superblock: unreadable");
        } else {
            match Superblock::parse(&bak) {
                Ok(b) if b.uuid != sb.uuid => r.err("backup superblock: UUID mismatch"),
                Ok(b) if b.gen != sb.gen => {
                    r.warn(format!("backup superblock gen {} != primary {}", b.gen, sb.gen))
                }
                Ok(_) => {}
                Err(e) => r.err(format!("backup superblock: {e}")),
            }
        }
    }

    let gofs = Gofs { dev, sb, map };
    let sb = &gofs.sb;

    // per-AG checks
    let mut total = (0u64, 0u64, 0u64); // free, rsvd, full
    for (agi, e) in gofs.map.entries.iter().enumerate() {
        let ag = agi as u32;
        if e.flags & AGF_RETIRED != 0 {
            continue;
        }
        if e.flags & AGF_PRESENT == 0 {
            r.warn(format!("AG {ag}: not present and not retired"));
            continue;
        }
        let mapped: u64 = e.segs.iter().map(|s| s.blocks as u64).sum();
        if mapped != e.length as u64 {
            r.err(format!("AG {ag}: map covers {mapped} of {} blocks", e.length));
            continue;
        }
        let hdr = match gofs.read_ag_header(ag) {
            Ok(h) => h,
            Err(e) => {
                r.err(format!("AG {ag}: {e}"));
                continue;
            }
        };
        if hdr.ag_num != ag {
            r.err(format!("AG {ag}: header claims ag_num {}", hdr.ag_num));
        }
        if hdr.length != e.length {
            r.err(format!("AG {ag}: header length {} != map length {}", hdr.length, e.length));
        }
        // allocator state tally vs counters
        let (cells, _) = rt_geometry(hdr.length, sb.blocksize);
        match gofs.read_root_table(&hdr) {
            Ok(table) => {
                let mut n = [0u64; 4];
                for c in 0..cells {
                    n[cell_get(&table, c) as usize] += 1;
                }
                if n[CELL_REFINED as usize] > 0 {
                    r.warn(format!(
                        "AG {ag}: {} refined cells (not validated by this version)",
                        n[CELL_REFINED as usize]
                    ));
                }
                let free = n[CELL_FREE as usize] * CELL_BLOCKS as u64;
                let full = n[CELL_FULL as usize] * CELL_BLOCKS as u64;
                if free != hdr.free_blocks as u64 {
                    r.err(format!(
                        "AG {ag}: table shows {free} free blocks, header says {}",
                        hdr.free_blocks
                    ));
                }
                if full != hdr.full_blocks as u64 {
                    r.err(format!(
                        "AG {ag}: table shows {full} full blocks, header says {}",
                        hdr.full_blocks
                    ));
                }
                total.0 += hdr.free_blocks as u64;
                total.1 += hdr.rsvd_blocks as u64;
                total.2 += hdr.full_blocks as u64;
            }
            Err(e) => r.err(format!("AG {ag}: root table: {e}")),
        }
    }
    if total.0 != sb.free_blocks {
        r.warn(format!(
            "superblock free_blocks {} != sum of AG counters {} (advisory field)",
            sb.free_blocks, total.0
        ));
    }

    // root inode and directory tree (v0: root EMBED dir, one level)
    match gofs.read_inode(sb.root_ino) {
        Ok(root) if !root.is_dir() => r.err("root inode is not a directory"),
        Ok(_) => match gofs.dir_entries(sb.root_ino) {
            Ok(entries) => {
                r.info.push(format!("root directory: {} entries", entries.len()));
                for ent in entries {
                    match gofs.read_inode(ent.ino) {
                        Ok(i) => {
                            for x in extents_check(&gofs, &i) {
                                r.err(format!("{}: {x}", ent.name));
                            }
                        }
                        Err(e) => r.err(format!("{}: {e}", ent.name)),
                    }
                }
            }
            Err(e) => r.err(format!("root directory: {e}")),
        },
        Err(e) => r.err(format!("root inode: {e}")),
    }

    Ok(r)
}

fn extents_check(gofs: &Gofs, inode: &Inode) -> Vec<String> {
    let mut errs = Vec::new();
    if inode.format == FMT_EXTENT {
        let bs = gofs.sb.blocksize as u64;
        let mut covered = 0u64;
        for e in extents_parse(&inode.payload) {
            if gofs.map.resolve(e.blk, gofs.sb.blocksize).is_none()
                || gofs.map.resolve(e.blk + (e.blocks - 1) as u64, gofs.sb.blocksize).is_none()
            {
                errs.push(format!("extent at {:#x} not fully mapped", e.blk));
            }
            covered += e.blocks as u64;
        }
        if covered < inode.size.div_ceil(bs) {
            errs.push(format!(
                "extents cover {covered} blocks, size needs {}",
                inode.size.div_ceil(bs)
            ));
        }
    }
    errs
}
