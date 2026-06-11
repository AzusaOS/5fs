//! Structural filesystem check (read-only).
//!
//! Validates: superblock + backup, both AG map copies, journal state, AG
//! headers, the full allocator refinement tree (states, table blocks,
//! per-block tallies vs counters), and the namespace from the root: inode
//! checksums, extent/tree mappings, directory structure (extendible-hashing
//! invariants), and link counts.

use crate::extent::{cov, node_child, node_level, node_state, root_child, root_level, root_state};
use crate::fmt::*;
use crate::fs::Gofs;
use anyhow::Result;
use std::collections::HashMap;
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
    let gofs = Gofs::open(path, false)?;
    let sb = &gofs.sb;
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
    let (_, status) = Gofs::load_agmap(&gofs.dev, sb)?;
    for (i, s) in status.iter().enumerate() {
        match s {
            Ok(gen) => r.info.push(format!("ag map copy {i}: valid, gen {gen}")),
            Err(e) => r.warn(format!("ag map copy {i}: {e}")),
        }
    }
    if gofs.map.entries.len() != sb.next_ag as usize {
        r.warn(format!(
            "ag map has {} entries, superblock next_ag is {}",
            gofs.map.entries.len(),
            sb.next_ag
        ));
    }

    // backup superblock
    if let Some(s) = gofs.map.entries.first().and_then(|e| e.segs.first()) {
        let off = s.dev_offset + (s.blocks as u64 - 1) * sb.blocksize as u64;
        let mut bak = [0u8; SB_SIZE];
        if gofs.dev.pread(&mut bak, off).is_err() {
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

    // journal
    match gofs.journal_pending() {
        Ok(0) => r.info.push(format!("journal: clean (seq {}, head {})", sb.journal_seq, sb.journal_head)),
        Ok(n) => r.warn(format!("journal: {n} transaction(s) pending replay (mount rw to apply)")),
        Err(e) => r.err(format!("journal: {e}")),
    }

    check_allocator(&gofs, &mut r);
    check_namespace(&gofs, &mut r);
    check_kernel(&gofs, &mut r);
    Ok(r)
}

fn check_allocator(gofs: &Gofs, r: &mut Report) {
    let sb = &gofs.sb;
    let mut totals = (0u64, 0u64, 0u64); // free, rsvd, full
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
        match tally_ag(gofs, &hdr) {
            Ok((free, rsvd, full, errs)) => {
                for m in errs {
                    r.err(format!("AG {ag}: {m}"));
                }
                if free != hdr.free_blocks as u64 {
                    r.err(format!(
                        "AG {ag}: allocator shows {free} free blocks, header says {}",
                        hdr.free_blocks
                    ));
                }
                if full != hdr.full_blocks as u64 {
                    r.err(format!(
                        "AG {ag}: allocator shows {full} full blocks, header says {}",
                        hdr.full_blocks
                    ));
                }
                if rsvd != hdr.rsvd_blocks as u64 {
                    r.warn(format!(
                        "AG {ag}: allocator shows {rsvd} reserved blocks, header says {}",
                        hdr.rsvd_blocks
                    ));
                }
                totals.0 += free;
                totals.1 += rsvd;
                totals.2 += full;
            }
            Err(e) => r.err(format!("AG {ag}: allocator: {e}")),
        }
    }
    if totals.0 != sb.free_blocks {
        r.warn(format!(
            "superblock free_blocks {} != allocator total {} (advisory field)",
            sb.free_blocks, totals.0
        ));
    }
}

/// Walk one AG's refinement tree; returns (free, rsvd, full, problems) in blocks.
fn tally_ag(gofs: &Gofs, hdr: &AgHeader) -> Result<(u64, u64, u64, Vec<String>)> {
    let (cells, _) = rt_geometry(hdr.length, gofs.sb.blocksize);
    let table = gofs.read_root_table(hdr)?;
    let mut errs = Vec::new();
    let (mut free, mut rsvd, mut full) = (0u64, 0u64, 0u64);
    // validated table blocks: local block -> used bitmap
    let mut tbl_used: HashMap<u32, u32> = HashMap::new();
    let mut tbl_referenced: HashMap<u32, u32> = HashMap::new();
    let mut read_rec = |gofs: &Gofs, rf: (u32, u16), errs: &mut Vec<String>| -> Option<[u8; 128]> {
        let buf = match gofs.read_block(blk_addr(hdr.ag_num, rf.0)) {
            Ok(b) => b,
            Err(e) => {
                errs.push(format!("table block {}: {e}", rf.0));
                return None;
            }
        };
        if buf[0..4] != ALLOC_MAGIC {
            errs.push(format!("table block {}: bad magic", rf.0));
            return None;
        }
        if get_u32(&buf, 4) != csum(&buf, 4) {
            errs.push(format!("table block {}: bad checksum", rf.0));
            return None;
        }
        let used = get_u32(&buf, 16);
        tbl_used.insert(rf.0, used);
        *tbl_referenced.entry(rf.0).or_insert(0) |= 1 << rf.1;
        if used & (1 << rf.1) == 0 {
            errs.push(format!("table block {} record {}: referenced but not marked used", rf.0, rf.1));
        }
        let off = 128 * (1 + rf.1 as usize);
        let mut rec = [0u8; 128];
        rec.copy_from_slice(&buf[off..off + 128]);
        Some(rec)
    };
    for c in 0..cells {
        let tail = (hdr.length as u64).saturating_sub(c * CELL_BLOCKS as u64).min(CELL_BLOCKS as u64);
        match cell_get(&table, c) {
            CELL_FREE => free += CELL_BLOCKS as u64,
            CELL_RSVD => rsvd += tail,
            CELL_FULL => full += CELL_BLOCKS as u64,
            _refined => {
                let rf = (
                    get_u32(&table, rt_ref_off(cells, c) as usize),
                    get_u16(&table, rt_ref_off(cells, c) as usize + 4),
                );
                let Some(l0rec) = read_rec(gofs, rf, &mut errs) else { continue };
                for i in 0..ALLOC_FANOUT {
                    match crate::alloc::rec_state(&l0rec, i) {
                        CELL_FREE => free += ALLOC_FANOUT as u64,
                        CELL_RSVD => rsvd += ALLOC_FANOUT as u64,
                        CELL_FULL => full += ALLOC_FANOUT as u64,
                        _ => {
                            let rf1 = crate::alloc::rec_ref(&l0rec, i);
                            let Some(l1rec) = read_rec(gofs, rf1, &mut errs) else { continue };
                            for j in 0..ALLOC_FANOUT {
                                match crate::alloc::rec_state(&l1rec, j) {
                                    CELL_FREE => free += 1,
                                    CELL_RSVD => rsvd += 1,
                                    CELL_FULL => full += 1,
                                    _ => {
                                        // L3: an inode slot block
                                        full += 1;
                                        let rf2 = crate::alloc::rec_ref(&l1rec, j);
                                        let Some(l3rec) = read_rec(gofs, rf2, &mut errs) else {
                                            continue;
                                        };
                                        let local = (c * CELL_BLOCKS as u64) as u32
                                            + i * ALLOC_FANOUT
                                            + j;
                                        check_slot_block(gofs, hdr.ag_num, local, &l3rec, &mut errs);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    for (blk, used) in tbl_used {
        let referenced = tbl_referenced.get(&blk).copied().unwrap_or(0);
        if used & !referenced != 0 {
            errs.push(format!(
                "table block {blk}: records {:#x} marked used but unreferenced (leak)",
                used & !referenced
            ));
        }
    }
    Ok((free, rsvd, full, errs))
}

/// L3 slot record vs the actual block: FULL slots must hold valid inodes,
/// FREE slots must be zeroed.
fn check_slot_block(gofs: &Gofs, ag: u32, local: u32, l3rec: &[u8; 128], errs: &mut Vec<String>) {
    let isz = gofs.sb.inodesize as usize;
    let slots = gofs.sb.blocksize as usize / isz;
    let buf = match gofs.read_block(blk_addr(ag, local)) {
        Ok(b) => b,
        Err(e) => {
            errs.push(format!("slot block {local}: {e}"));
            return;
        }
    };
    for s in 0..slots {
        let chunk = &buf[s * isz..(s + 1) * isz];
        match crate::alloc::rec_state(l3rec, s as u32) {
            CELL_FULL => {
                if let Err(e) = Inode::parse(chunk) {
                    errs.push(format!("slot block {local} slot {s}: marked used but {e}"));
                }
            }
            CELL_FREE => {
                if !chunk.iter().all(|&b| b == 0) {
                    errs.push(format!("slot block {local} slot {s}: marked free but not zeroed"));
                }
            }
            st => errs.push(format!("slot block {local} slot {s}: bad state {st}")),
        }
    }
}

fn check_kernel(gofs: &Gofs, r: &mut Report) {
    let sb = &gofs.sb;
    if sb.kernel_offset == 0 {
        return;
    }
    let len = sb.kernel_end.saturating_sub(sb.kernel_offset);
    r.info.push(format!("kernel: {len} bytes at phys {:#x}", sb.kernel_offset));
    let Some(seg) = gofs.map.entries.first().and_then(|e| e.segs.first()) else { return };
    let ag0_end = seg.dev_offset + seg.blocks as u64 * sb.blocksize as u64;
    if sb.kernel_offset < seg.dev_offset || sb.kernel_end > ag0_end {
        r.err("kernel region lies outside AG 0's first segment");
    }
    match gofs.dir_lookup(sb.root_ino, "kernel.bin") {
        Ok(Some(ino)) => match gofs.read_inode(ino) {
            Ok(i) => {
                if i.flags & INODE_FLAG_IMMUTABLE == 0 {
                    r.err("kernel.bin: missing immutable flag");
                }
                if i.size != len {
                    r.err(format!("kernel.bin: size {} != superblock kernel length {len}", i.size));
                }
                let ext = extents_parse(&i.payload);
                match ext.first().and_then(|e| gofs.map.resolve(e.blk, sb.blocksize)) {
                    Some(phys) if phys == sb.kernel_offset => {}
                    Some(phys) => r.err(format!(
                        "kernel.bin extent at phys {phys:#x} but superblock says {:#x}",
                        sb.kernel_offset
                    )),
                    None => r.err("kernel.bin: extent unmapped"),
                }
            }
            Err(e) => r.err(format!("kernel.bin: {e}")),
        },
        Ok(None) => r.err("kernel region exists but kernel.bin is missing from /"),
        Err(e) => r.err(format!("kernel.bin lookup: {e}")),
    }
}

fn check_namespace(gofs: &Gofs, r: &mut Report) {
    // expected nlink per inode from the directory walk
    let mut nlinks: HashMap<u64, u32> = HashMap::new();
    let mut files = 0u64;
    let mut dirs = 0u64;
    walk_dir(gofs, gofs.sb.root_ino, "/", 0, r, &mut nlinks, &mut files, &mut dirs);
    for (ino, expect) in nlinks {
        match gofs.read_inode(ino) {
            Ok(i) => {
                if i.nlink != expect {
                    r.err(format!("inode {ino:#x}: nlink {} but {expect} reference(s) found", i.nlink));
                }
            }
            Err(e) => r.err(format!("inode {ino:#x}: {e}")),
        }
    }
    r.info.push(format!("namespace: {dirs} directorie(s), {files} file(s)"));
}

#[allow(clippy::too_many_arguments)]
fn walk_dir(
    gofs: &Gofs,
    ino: u64,
    path: &str,
    depth: u32,
    r: &mut Report,
    nlinks: &mut HashMap<u64, u32>,
    files: &mut u64,
    dirs: &mut u64,
) {
    if depth > 64 {
        r.err(format!("{path}: directory tree too deep (cycle?)"));
        return;
    }
    let inode = match gofs.read_inode(ino) {
        Ok(i) => i,
        Err(e) => {
            r.err(format!("{path}: {e}"));
            return;
        }
    };
    if !inode.is_dir() {
        r.err(format!("{path}: not a directory"));
        return;
    }
    *dirs += 1;
    let t = gofs.txn();
    // 2 for the dir itself (self + parent's entry handled by caller side)
    let mut self_links = 2u32;
    match gofs.dir_check(&t, &inode) {
        Ok(errs) => {
            for m in errs {
                r.err(format!("{path}: {m}"));
            }
        }
        Err(e) => r.err(format!("{path}: {e}")),
    }
    match gofs.dir_list(&t, &inode) {
        Ok(entries) => {
            for ent in entries {
                let cpath = format!("{}{}", path, ent.name);
                let child = match gofs.read_inode(ent.ino) {
                    Ok(i) => i,
                    Err(e) => {
                        r.err(format!("{cpath}: {e}"));
                        continue;
                    }
                };
                if child.is_dir() {
                    if ent.ftype != DT_DIR {
                        r.warn(format!("{cpath}: entry type {} but inode is a directory", ent.ftype));
                    }
                    self_links += 1;
                    nlinks.insert(ent.ino, 2 + count_subdirs(gofs, &child));
                    walk_dir(gofs, ent.ino, &format!("{cpath}/"), depth + 1, r, nlinks, files, dirs);
                } else {
                    *files += 1;
                    *nlinks.entry(ent.ino).or_insert(0) += 1;
                    for m in check_mapping(gofs, &child) {
                        r.err(format!("{cpath}: {m}"));
                    }
                }
            }
        }
        Err(e) => r.err(format!("{path}: {e}")),
    }
    nlinks.entry(ino).or_insert(self_links);
}

fn count_subdirs(gofs: &Gofs, inode: &Inode) -> u32 {
    let t = gofs.txn();
    gofs.dir_list(&t, inode)
        .map(|v| v.iter().filter(|e| e.ftype == DT_DIR).count() as u32)
        .unwrap_or(0)
}

/// Validate a file's block mapping: tree node integrity, mapped addresses.
fn check_mapping(gofs: &Gofs, inode: &Inode) -> Vec<String> {
    let mut errs = Vec::new();
    let bs = gofs.sb.blocksize;
    match inode.format {
        FMT_EMPTY | FMT_EMBED => {}
        FMT_EXTENT => {
            for e in extents_parse(&inode.payload) {
                if gofs.map.resolve(e.blk, bs).is_none()
                    || gofs.map.resolve(e.blk + (e.blocks - 1) as u64, bs).is_none()
                {
                    errs.push(format!("extent at {:#x} not fully mapped", e.blk));
                }
            }
        }
        FMT_TREE => {
            let level = root_level(&inode.payload);
            for i in 0..ROOT_FANOUT {
                let st = root_state(&inode.payload, i);
                let (blocks, addr) = root_child(&inode.payload, i);
                check_tree_child(gofs, st, blocks as u64, addr, level, &mut errs);
            }
        }
        f => errs.push(format!("unknown format {f}")),
    }
    errs
}

fn check_tree_child(gofs: &Gofs, st: u8, blocks: u64, addr: u64, level: u8, errs: &mut Vec<String>) {
    let bs = gofs.sb.blocksize;
    match st {
        CELL_FREE => {}
        CELL_FULL => {
            if blocks == 0 || blocks > cov(level) {
                errs.push(format!("FULL child maps {blocks} blocks > coverage {}", cov(level)));
            } else if gofs.map.resolve(addr, bs).is_none()
                || gofs.map.resolve(addr + blocks - 1, bs).is_none()
            {
                errs.push(format!("run at {addr:#x} not fully mapped"));
            }
        }
        CELL_REFINED => {
            if level == 0 {
                errs.push("level-0 child marked refined".into());
                return;
            }
            let t = gofs.txn();
            match gofs.node_read(&t, addr) {
                Ok(buf) => {
                    if node_level(&buf) != level - 1 {
                        errs.push(format!(
                            "node {addr:#x}: level {} under a level-{level} child",
                            node_level(&buf)
                        ));
                    }
                    for i in 0..NODE_FANOUT as usize {
                        let cst = node_state(&buf, i);
                        let (b, a) = node_child(&buf, i);
                        check_tree_child(gofs, cst, b as u64, a, node_level(&buf), errs);
                    }
                }
                Err(e) => errs.push(format!("{e}")),
            }
        }
        s => errs.push(format!("bad child state {s}")),
    }
}
