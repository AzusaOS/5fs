//! Directories (doc/5-directories.md).
//!
//! Tiny directories are EMBED (entries inline in the inode payload, see
//! fmt::dir_parse). On overflow they convert to extendible hashing: the
//! directory's file space holds a header block (file block 0, "5FSD") and
//! buckets (file block 1+n, "5FSb"). Hashing is SipHash-2-4 keyed by the
//! volume UUID over the name's UTF-16BE bytes; a bucket is selected by the
//! top `global_depth` bits.
//!
//! Coarsening: removals trigger buddy-merge — two buckets that differ only
//! in their deepest hash bit and fit one block re-merge, the table halves
//! when every pair collapses, and freed bucket numbers go on a freelist in
//! the header (reused by later splits). A directory that empties completely
//! collapses back to FMT_EMPTY and frees its blocks.

use crate::fmt::*;
use crate::fs::Gofs;
use crate::journal::Txn;
use anyhow::{anyhow, bail, Result};
use std::hash::Hasher;

/// Maximum global depth (table must fit the header block at 4 KiB).
pub const MAX_DEPTH: u8 = 9;

const DH_HDR: usize = 24; // magic csum gen depth pad nbuckets
const BK_HDR: usize = 24; // magic csum gen local_depth pad count pad

pub fn hash_name(uuid: &[u8; 16], name: &str) -> u64 {
    let k0 = u64::from_le_bytes(uuid[0..8].try_into().unwrap());
    let k1 = u64::from_le_bytes(uuid[8..16].try_into().unwrap());
    let mut h = siphasher::sip::SipHasher24::new_with_keys(k0, k1);
    for u in name.encode_utf16() {
        h.write(&u.to_be_bytes());
    }
    h.finish()
}

fn bucket_index(hash: u64, depth: u8) -> usize {
    if depth == 0 {
        0
    } else {
        (hash >> (64 - depth)) as usize
    }
}

fn entry_size(name_units: usize) -> usize {
    (20 + 2 * name_units + 3) & !3
}

#[derive(Debug, Clone)]
struct HEntry {
    ino: u64,
    hash: u64,
    ftype: u8,
    name: String,
}

fn bucket_entries(buf: &[u8]) -> Vec<HEntry> {
    let count = get_u16(buf, 18) as usize;
    let mut v = Vec::with_capacity(count);
    let mut off = BK_HDR;
    for _ in 0..count {
        let ino = get_u64(buf, off);
        let hash = get_u64(buf, off + 8);
        let ftype = buf[off + 16];
        let nlen = get_u16(buf, off + 18) as usize;
        let units: Vec<u16> =
            (0..nlen).map(|i| get_u16(buf, off + 20 + i * 2)).collect();
        v.push(HEntry { ino, hash, ftype, name: String::from_utf16_lossy(&units) });
        off += entry_size(nlen);
    }
    v
}

fn bucket_build(bs: usize, local_depth: u8, entries: &[HEntry]) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; bs];
    buf[0..4].copy_from_slice(&BUCKET_MAGIC);
    buf[16] = local_depth;
    put_u16(&mut buf, 18, entries.len() as u16);
    let mut off = BK_HDR;
    for e in entries {
        let units: Vec<u16> = e.name.encode_utf16().collect();
        if off + entry_size(units.len()) > bs {
            bail!("bucket overflow");
        }
        put_u64(&mut buf, off, e.ino);
        put_u64(&mut buf, off + 8, e.hash);
        buf[off + 16] = e.ftype;
        put_u16(&mut buf, off + 18, units.len() as u16);
        for (i, u) in units.iter().enumerate() {
            put_u16(&mut buf, off + 20 + i * 2, *u);
        }
        off += entry_size(units.len());
    }
    Ok(buf)
}

fn bucket_fits(bs: usize, entries: &[HEntry], add: &HEntry) -> bool {
    let used: usize =
        entries.iter().map(|e| entry_size(e.name.encode_utf16().count())).sum();
    BK_HDR + used + entry_size(add.name.encode_utf16().count()) <= bs
}

impl Gofs {
    // --- directory file block access ------------------------------------------------

    fn dir_block_read(&self, t: &Txn, inode: &Inode, fb: u64) -> Result<Vec<u8>> {
        match self.map_lookup(t, inode, fb)? {
            crate::extent::Map::Run { blk, .. } => self.txn_read(t, blk),
            crate::extent::Map::Hole(_) => bail!("directory block {fb} is a hole"),
        }
    }

    fn dir_block_addr(&self, t: &Txn, inode: &Inode, fb: u64) -> Result<u64> {
        match self.map_lookup(t, inode, fb)? {
            crate::extent::Map::Run { blk, .. } => Ok(blk),
            crate::extent::Map::Hole(_) => bail!("directory block {fb} is a hole"),
        }
    }

    fn dir_block_ensure(
        &mut self,
        t: &mut Txn,
        ag: u32,
        inode: &mut Inode,
        fb: u64,
    ) -> Result<u64> {
        let runs = self.map_ensure(t, ag, inode, fb, 1)?;
        Ok(runs[0].1)
    }

    /// (depth, nbuckets, table, header block, freelist). The freelist of
    /// reusable bucket numbers grows down from the end of the header block;
    /// its count lives at offset 18.
    #[allow(clippy::type_complexity)]
    fn dhdr_read(&self, t: &Txn, inode: &Inode) -> Result<(u8, u32, Vec<u32>, u64, Vec<u32>)> {
        let blk = self.dir_block_addr(t, inode, 0)?;
        let buf = self.dir_block_read(t, inode, 0)?;
        if buf[0..4] != DIRH_MAGIC {
            bail!("bad directory header magic");
        }
        if get_u32(&buf, 4) != csum(&buf, 4) {
            bail!("directory header checksum mismatch");
        }
        let depth = buf[16];
        let nbuckets = get_u32(&buf, 20);
        let table: Vec<u32> =
            (0..1usize << depth).map(|i| get_u32(&buf, DH_HDR + i * 4)).collect();
        let nfree = get_u16(&buf, 18) as usize;
        let bs = buf.len();
        let freelist: Vec<u32> = (0..nfree).map(|k| get_u32(&buf, bs - 4 * (k + 1))).collect();
        Ok((depth, nbuckets, table, blk, freelist))
    }

    fn dhdr_write(
        &self,
        t: &mut Txn,
        blk: u64,
        depth: u8,
        nbuckets: u32,
        table: &[u32],
        freelist: &[u32],
    ) -> Result<()> {
        let bs = self.sb.blocksize as usize;
        let mut buf = vec![0u8; bs];
        buf[0..4].copy_from_slice(&DIRH_MAGIC);
        let old = self.txn_read(t, blk).map(|b| get_u64(&b, 8)).unwrap_or(0);
        put_u64(&mut buf, 8, old + 1);
        buf[16] = depth;
        put_u16(&mut buf, 18, freelist.len() as u16);
        put_u32(&mut buf, 20, nbuckets);
        if DH_HDR + table.len() * 4 + freelist.len() * 4 > bs {
            bail!("directory table exceeds header block");
        }
        for (i, b) in table.iter().enumerate() {
            put_u32(&mut buf, DH_HDR + i * 4, *b);
        }
        for (k, b) in freelist.iter().enumerate() {
            put_u32(&mut buf, bs - 4 * (k + 1), *b);
        }
        let c = csum(&buf, 4);
        put_u32(&mut buf, 4, c);
        self.txn_write(t, blk, buf);
        Ok(())
    }

    fn bucket_read(&self, t: &Txn, inode: &Inode, bno: u32) -> Result<(Vec<HEntry>, u8, u64)> {
        let blk = self.dir_block_addr(t, inode, 1 + bno as u64)?;
        let buf = self.dir_block_read(t, inode, 1 + bno as u64)?;
        if buf[0..4] != BUCKET_MAGIC {
            bail!("bucket {bno}: bad magic");
        }
        if get_u32(&buf, 4) != csum(&buf, 4) {
            bail!("bucket {bno}: checksum mismatch");
        }
        Ok((bucket_entries(&buf), buf[16], blk))
    }

    fn bucket_write(
        &self,
        t: &mut Txn,
        blk: u64,
        local_depth: u8,
        entries: &[HEntry],
    ) -> Result<()> {
        let mut buf = bucket_build(self.sb.blocksize as usize, local_depth, entries)?;
        let old = self.txn_read(t, blk).map(|b| get_u64(&b, 8)).unwrap_or(0);
        put_u64(&mut buf, 8, old + 1);
        let c = csum(&buf, 4);
        put_u32(&mut buf, 4, c);
        self.txn_write(t, blk, buf);
        Ok(())
    }

    // --- generic directory API ----------------------------------------------------------

    /// All entries of a directory, any format.
    pub fn dir_list(&self, t: &Txn, inode: &Inode) -> Result<Vec<DirEntry>> {
        if !inode.is_dir() {
            bail!("not a directory");
        }
        match inode.format {
            FMT_EMPTY => Ok(Vec::new()),
            FMT_EMBED => Ok(dir_parse(&inode.payload)),
            FMT_EXTENT | FMT_TREE => {
                let (_, nbuckets, _, _, freelist) = self.dhdr_read(t, inode)?;
                let mut out = Vec::new();
                for b in (0..nbuckets).filter(|b| !freelist.contains(b)) {
                    let (entries, _, _) = self.bucket_read(t, inode, b)?;
                    out.extend(entries.into_iter().map(|e| DirEntry {
                        ino: e.ino,
                        ftype: e.ftype,
                        name: e.name,
                    }));
                }
                out.sort_by(|a, b| a.name.cmp(&b.name));
                Ok(out)
            }
            f => bail!("directory format {f} unsupported"),
        }
    }

    pub fn dir_find(&self, t: &Txn, inode: &Inode, name: &str) -> Result<Option<DirEntry>> {
        if !inode.is_dir() {
            bail!("not a directory");
        }
        match inode.format {
            FMT_EMPTY => Ok(None),
            FMT_EMBED => Ok(dir_parse(&inode.payload).into_iter().find(|e| e.name == name)),
            FMT_EXTENT | FMT_TREE => {
                let h = hash_name(&self.sb.uuid, name);
                let (depth, _, table, _, _) = self.dhdr_read(t, inode)?;
                let bno = table[bucket_index(h, depth)];
                let (entries, _, _) = self.bucket_read(t, inode, bno)?;
                Ok(entries
                    .into_iter()
                    .find(|e| e.hash == h && e.name == name)
                    .map(|e| DirEntry { ino: e.ino, ftype: e.ftype, name: e.name }))
            }
            f => bail!("directory format {f} unsupported"),
        }
    }

    pub fn dir_count(&self, t: &Txn, inode: &Inode) -> Result<usize> {
        Ok(self.dir_list(t, inode)?.len())
    }

    /// Insert an entry, converting EMBED -> hashed on overflow. `dir_ino`
    /// determines the AG for new blocks. The caller persists the inode.
    pub fn dir_insert(
        &mut self,
        t: &mut Txn,
        dir_ino: u64,
        inode: &mut Inode,
        entry: &DirEntry,
    ) -> Result<()> {
        if self.dir_find(t, inode, &entry.name)?.is_some() {
            bail!("{}: already exists", entry.name);
        }
        match inode.format {
            FMT_EMPTY | FMT_EMBED => {
                if inode.format == FMT_EMPTY {
                    inode.format = FMT_EMBED;
                    inode.payload.fill(0);
                }
                let mut p = inode.payload;
                if dir_append(&mut p, entry).is_ok() {
                    inode.payload = p;
                    return Ok(());
                }
                // overflow: convert to hashed, then insert there
                self.dir_to_hashed(t, dir_ino, inode)?;
                self.dir_insert(t, dir_ino, inode, entry)
            }
            FMT_EXTENT | FMT_TREE => self.hashed_insert(t, dir_ino, inode, entry),
            f => bail!("directory format {f} unsupported"),
        }
    }

    fn dir_to_hashed(&mut self, t: &mut Txn, dir_ino: u64, inode: &mut Inode) -> Result<()> {
        let old = dir_parse(&inode.payload);
        let (ag, _) = blk_split(dir_ino);
        inode.format = FMT_EMPTY;
        inode.payload.fill(0);
        let hblk = self.dir_block_ensure(t, ag, inode, 0)?;
        let bblk = self.dir_block_ensure(t, ag, inode, 1)?;
        self.dhdr_write(t, hblk, 0, 1, &[0], &[])?;
        self.bucket_write(t, bblk, 0, &[])?;
        inode.size = 2 * self.sb.blocksize as u64;
        for e in old {
            self.hashed_insert(t, dir_ino, inode, &e)?;
        }
        Ok(())
    }

    fn hashed_insert(
        &mut self,
        t: &mut Txn,
        dir_ino: u64,
        inode: &mut Inode,
        entry: &DirEntry,
    ) -> Result<()> {
        let (ag, _) = blk_split(dir_ino);
        let h = hash_name(&self.sb.uuid, &entry.name);
        let new = HEntry { ino: entry.ino, hash: h, ftype: entry.ftype, name: entry.name.clone() };
        let bs = self.sb.blocksize as usize;
        loop {
            let (depth, nbuckets, mut table, hblk, mut freelist) = self.dhdr_read(t, inode)?;
            let bno = table[bucket_index(h, depth)];
            let (mut entries, ld, bblk) = self.bucket_read(t, inode, bno)?;
            if bucket_fits(bs, &entries, &new) {
                entries.push(new);
                self.bucket_write(t, bblk, ld, &entries)?;
                return Ok(());
            }
            // split the bucket
            let mut depth = depth;
            if ld == depth {
                if depth >= MAX_DEPTH {
                    bail!("directory exceeds maximum depth (v1 limit)");
                }
                depth += 1;
                let mut bigger = Vec::with_capacity(table.len() * 2);
                for b in &table {
                    bigger.push(*b);
                    bigger.push(*b);
                }
                table = bigger;
            }
            // reuse a freed bucket number before growing the file
            let (new_bno, new_nbuckets) = match freelist.pop() {
                Some(b) => (b, nbuckets),
                None => (nbuckets, nbuckets + 1),
            };
            let nblk = self.dir_block_ensure(t, ag, inode, 1 + new_bno as u64)?;
            inode.size = inode.size.max((2 + new_bno as u64) * bs as u64);
            let bit = 63 - ld; // the bit that distinguishes the buddies
            let (stay, go): (Vec<HEntry>, Vec<HEntry>) =
                entries.into_iter().partition(|e| e.hash & (1 << bit) == 0);
            self.bucket_write(t, bblk, ld + 1, &stay)?;
            self.bucket_write(t, nblk, ld + 1, &go)?;
            for (idx, b) in table.iter_mut().enumerate() {
                if *b == bno && (idx >> (depth - 1 - ld)) & 1 == 1 {
                    *b = new_bno;
                }
            }
            self.dhdr_write(t, hblk, depth, new_nbuckets, &table, &freelist)?;
            // loop: retry the insert against the new layout
        }
    }

    /// Structural directory validation for fsck: bucket placement, depth
    /// invariants. Returns problem descriptions.
    pub fn dir_check(&self, t: &Txn, inode: &Inode) -> Result<Vec<String>> {
        let mut errs = Vec::new();
        if !matches!(inode.format, FMT_EXTENT | FMT_TREE) {
            return Ok(errs); // EMBED/EMPTY have nothing structural to check
        }
        let (depth, nbuckets, table, _, freelist) = self.dhdr_read(t, inode)?;
        for (idx, b) in table.iter().enumerate() {
            if *b >= nbuckets {
                errs.push(format!("table[{idx}] points at bucket {b} >= {nbuckets}"));
            }
            if freelist.contains(b) {
                errs.push(format!("table[{idx}] points at freed bucket {b}"));
            }
        }
        for b in &freelist {
            if *b >= nbuckets {
                errs.push(format!("freelist entry {b} >= {nbuckets}"));
            }
        }
        for b in (0..nbuckets).filter(|b| !freelist.contains(b)) {
            match self.bucket_read(t, inode, b) {
                Ok((entries, ld, _)) => {
                    if ld > depth {
                        errs.push(format!("bucket {b}: local depth {ld} > global {depth}"));
                    }
                    for e in &entries {
                        let h = hash_name(&self.sb.uuid, &e.name);
                        if h != e.hash {
                            errs.push(format!("\"{}\": stored hash mismatch", e.name));
                        }
                        if table[bucket_index(h, depth)] != b {
                            errs.push(format!("\"{}\": entry in wrong bucket {b}", e.name));
                        }
                    }
                }
                Err(e) => errs.push(format!("bucket {b}: {e}")),
            }
        }
        Ok(errs)
    }

    /// Buddy-merge pass: while a bucket and its buddy (same local depth,
    /// differing in the deepest distinguishing hash bit) fit one block,
    /// merge them; then halve the table while every slot pair is identical.
    /// Freed bucket numbers go on the header freelist for later splits.
    fn try_merge(&mut self, t: &mut Txn, inode: &mut Inode) -> Result<()> {
        let bs = self.sb.blocksize as usize;
        loop {
            let (depth, nbuckets, mut table, hblk, mut freelist) = self.dhdr_read(t, inode)?;
            if depth == 0 {
                return Ok(());
            }
            let mut acted = false;
            for idx in 0..table.len() {
                let bno = table[idx];
                let (entries, ld, bblk) = self.bucket_read(t, inode, bno)?;
                if ld == 0 {
                    continue;
                }
                let buddy_idx = idx ^ (1usize << (depth - ld));
                let bno2 = table[buddy_idx];
                if bno2 == bno {
                    continue;
                }
                let (entries2, ld2, bblk2) = self.bucket_read(t, inode, bno2)?;
                if ld2 != ld {
                    continue;
                }
                let used: usize = entries
                    .iter()
                    .chain(entries2.iter())
                    .map(|e| entry_size(e.name.encode_utf16().count()))
                    .sum();
                if BK_HDR + used > bs {
                    continue;
                }
                let mut all = entries;
                all.extend(entries2);
                self.bucket_write(t, bblk, ld - 1, &all)?;
                self.bucket_write(t, bblk2, 0, &[])?; // freed bucket left empty
                for b in table.iter_mut() {
                    if *b == bno2 {
                        *b = bno;
                    }
                }
                freelist.push(bno2);
                self.dhdr_write(t, hblk, depth, nbuckets, &table, &freelist)?;
                acted = true;
                break;
            }
            if acted {
                continue;
            }
            // halve the table when every adjacent pair points the same way
            if table.chunks(2).all(|p| p[0] == p[1]) {
                let halved: Vec<u32> = table.chunks(2).map(|p| p[0]).collect();
                self.dhdr_write(t, hblk, depth - 1, nbuckets, &halved, &freelist)?;
                continue;
            }
            return Ok(());
        }
    }

    /// Remove an entry by name. Returns it. An EMBED directory is repacked;
    /// a hashed directory that becomes completely empty collapses to EMPTY.
    pub fn dir_remove(&mut self, t: &mut Txn, inode: &mut Inode, name: &str) -> Result<DirEntry> {
        match inode.format {
            FMT_EMBED => {
                let mut entries = dir_parse(&inode.payload);
                let i = entries
                    .iter()
                    .position(|e| e.name == name)
                    .ok_or_else(|| anyhow!("{name}: not found"))?;
                let removed = entries.remove(i);
                let mut p = [0u8; INODE_PAYLOAD];
                for e in &entries {
                    dir_append(&mut p, e).map_err(|e| anyhow!(e))?;
                }
                inode.payload = p;
                if entries.is_empty() {
                    inode.format = FMT_EMPTY;
                }
                Ok(removed)
            }
            FMT_EXTENT | FMT_TREE => {
                let h = hash_name(&self.sb.uuid, name);
                let (depth, _, table, _, _) = self.dhdr_read(t, inode)?;
                let bno = table[bucket_index(h, depth)];
                let (mut entries, ld, bblk) = self.bucket_read(t, inode, bno)?;
                let i = entries
                    .iter()
                    .position(|e| e.hash == h && e.name == name)
                    .ok_or_else(|| anyhow!("{name}: not found"))?;
                let removed = entries.remove(i);
                self.bucket_write(t, bblk, ld, &entries)?;
                self.try_merge(t, inode)?;
                if self.dir_count(t, inode)? == 0 {
                    // collapse: free the directory's blocks entirely
                    self.map_free_all(t, inode)?;
                }
                Ok(DirEntry { ino: removed.ino, ftype: removed.ftype, name: removed.name })
            }
            FMT_EMPTY => bail!("{name}: not found"),
            f => bail!("directory format {f} unsupported"),
        }
    }
}
