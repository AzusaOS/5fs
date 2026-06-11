//! Filesystem access: open/replay, address resolution, inode I/O, file
//! read/write, and the VFS-level operations (create/unlink/rename/...).
//! All metadata mutation goes through journal transactions (journal.rs);
//! file data is written in place before the transaction commits (ordered
//! mode, doc/6-journal.md).

use crate::device::Device;
use crate::extent::Map;
use crate::fmt::*;
use crate::journal::Txn;
use anyhow::{anyhow, bail, Result};
use std::path::Path;

pub struct Gofs {
    pub dev: Device,
    pub sb: Superblock,
    pub map: AgMap,
    /// In-memory allocator scan hints per AG: (arena block, L0 cell). Pure
    /// optimization — scans wrap around, so a stale hint only costs time.
    pub hints: std::collections::HashMap<u32, (u32, u64)>,
}

impl Gofs {
    pub fn open(path: &Path, writable: bool) -> Result<Self> {
        let dev = Device::open(path, writable)?;
        let (sb, from_backup) = Self::find_superblock(&dev)?;
        let map = Self::load_agmap(&dev, &sb)?.0;
        let mut fs = Gofs { dev, sb, map, hints: Default::default() };
        if writable {
            if from_backup {
                eprintln!("5fs: primary superblock invalid; recovered from backup");
                fs.write_superblock()?; // self-heal both copies
            } else if !fs.backup_superblock_ok() {
                fs.write_superblock()?; // heal a torn/stale backup
            }
            let n = fs.replay()?;
            if n > 0 {
                eprintln!("5fs: replayed {n} journal transaction(s)");
            }
        }
        Ok(fs)
    }

    /// Read the primary superblock, falling back to the backup copy. The
    /// backup lives in the last block of AG 0, found by probing the AG 0
    /// header (block 1) at each candidate block size.
    pub fn find_superblock(dev: &Device) -> Result<(Superblock, bool)> {
        let mut sbbuf = [0u8; SB_SIZE];
        dev.pread(&mut sbbuf, 0)?;
        match Superblock::parse(&sbbuf) {
            Ok(sb) => Ok((sb, false)),
            Err(primary_err) => {
                for bs in [2048u64, 4096, 8192, 16384, 32768, 65536] {
                    let mut h = vec![0u8; AGHDR_SIZE];
                    if dev.pread(&mut h, bs).is_err() {
                        continue;
                    }
                    let Ok(hdr) = AgHeader::parse(&h) else { continue };
                    if hdr.ag_num != 0 {
                        continue;
                    }
                    let off = (hdr.length as u64 - 1) * bs;
                    let mut bak = [0u8; SB_SIZE];
                    if dev.pread(&mut bak, off).is_err() {
                        continue;
                    }
                    if let Ok(sb) = Superblock::parse(&bak) {
                        if sb.blocksize as u64 == bs {
                            return Ok((sb, true));
                        }
                    }
                }
                Err(anyhow!("{primary_err}; backup superblock not found either"))
            }
        }
    }

    /// Load both AG map copies; return the one with the highest valid
    /// generation plus per-copy status for fsck.
    pub fn load_agmap(dev: &Device, sb: &Superblock) -> Result<(AgMap, [Result<u64, String>; 2])> {
        let mut best: Option<AgMap> = None;
        let mut status: [Result<u64, String>; 2] = [Err("unread".into()), Err("unread".into())];
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

    /// Persist the AG map (both copies) and the superblock.
    pub fn write_agmap(&mut self) -> Result<()> {
        self.map.gen += 1;
        let bytes =
            self.map.to_bytes(self.sb.agmap_length as usize).map_err(anyhow::Error::msg)?;
        self.dev.pwrite(&bytes, self.sb.agmap_offset)?;
        self.dev.pwrite(&bytes, self.sb.agmap_offset + self.sb.agmap_length)?;
        Ok(())
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

    // --- AG headers -------------------------------------------------------------

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

    /// Non-txn root table read (fsck/debugfs).
    pub fn read_root_table(&self, hdr: &AgHeader) -> Result<Vec<u8>> {
        let (_cells, rt_blocks) = rt_geometry(hdr.length, self.sb.blocksize);
        let mut buf = Vec::with_capacity((rt_blocks * self.sb.blocksize) as usize);
        for i in 0..rt_blocks {
            buf.extend_from_slice(&self.read_block(blk_addr(hdr.ag_num, hdr.alloc_root + i))?);
        }
        Ok(buf)
    }

    /// True if the backup superblock parses and matches the primary's
    /// generation.
    fn backup_superblock_ok(&self) -> bool {
        let Some(s) = self.map.entries.first().and_then(|e| e.segs.first()) else {
            return true;
        };
        let off = s.dev_offset + (s.blocks as u64 - 1) * self.sb.blocksize as u64;
        let mut bak = [0u8; SB_SIZE];
        if self.dev.pread(&mut bak, off).is_err() {
            return false;
        }
        matches!(Superblock::parse(&bak), Ok(b) if b.uuid == self.sb.uuid && b.gen == self.sb.gen)
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

    // --- inodes -------------------------------------------------------------------

    pub fn inode_phys(&self, ino: u64) -> Result<u64> {
        let (ag, slot) = blk_split(ino);
        let byte_off = slot as u64 * self.sb.inodesize as u64;
        let bs = self.sb.blocksize as u64;
        let blk = blk_addr(ag, (byte_off / bs) as u32);
        Ok(self.resolve(blk)? + byte_off % bs)
    }

    fn inode_block_off(&self, ino: u64) -> (u64, usize) {
        let (ag, slot) = blk_split(ino);
        let isz = self.sb.inodesize as u32;
        let slots = self.sb.blocksize / isz;
        (blk_addr(ag, slot / slots), ((slot % slots) * isz) as usize)
    }

    pub fn read_inode(&self, ino: u64) -> Result<Inode> {
        let mut buf = [0u8; INODE_SIZE];
        self.dev.pread(&mut buf, self.inode_phys(ino)?)?;
        Inode::parse(&buf).map_err(|e| anyhow!("inode {ino:#x}: {e}"))
    }

    pub fn read_inode_txn(&self, t: &Txn, ino: u64) -> Result<Inode> {
        let (blk, off) = self.inode_block_off(ino);
        let buf = self.txn_read(t, blk)?;
        Inode::parse(&buf[off..off + INODE_SIZE]).map_err(|e| anyhow!("inode {ino:#x}: {e}"))
    }

    pub fn write_inode_txn(&self, t: &mut Txn, ino: u64, inode: &Inode) -> Result<()> {
        let (blk, off) = self.inode_block_off(ino);
        let mut buf = self.txn_read(t, blk)?;
        buf[off..off + INODE_SIZE].copy_from_slice(&inode.to_bytes());
        self.txn_write(t, blk, buf);
        Ok(())
    }

    // --- directory convenience (read-only paths) -------------------------------------

    pub fn dir_entries(&self, ino: u64) -> Result<Vec<DirEntry>> {
        let t = self.txn();
        let inode = self.read_inode(ino)?;
        self.dir_list(&t, &inode)
    }

    pub fn dir_lookup(&self, dir: u64, name: &str) -> Result<Option<u64>> {
        let t = self.txn();
        let inode = self.read_inode(dir)?;
        Ok(self.dir_find(&t, &inode, name)?.map(|e| e.ino))
    }

    // --- path resolution ----------------------------------------------------------------

    pub fn lookup_path(&self, path: &str) -> Result<u64> {
        let mut ino = self.sb.root_ino;
        for comp in path.split('/').filter(|c| !c.is_empty() && *c != ".") {
            ino = self
                .dir_lookup(ino, comp)?
                .ok_or_else(|| anyhow!("{comp}: no such file or directory"))?;
        }
        Ok(ino)
    }

    /// Split a path into (parent inode, final component).
    pub fn resolve_parent(&self, path: &str) -> Result<(u64, String)> {
        let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
        let Some((name, dirs)) = comps.split_last() else {
            bail!("cannot operate on the root directory");
        };
        let mut ino = self.sb.root_ino;
        for comp in dirs {
            ino = self
                .dir_lookup(ino, comp)?
                .ok_or_else(|| anyhow!("{comp}: no such file or directory"))?;
        }
        Ok((ino, name.to_string()))
    }

    // --- file read ---------------------------------------------------------------------------

    pub fn read(&self, ino: u64, offset: u64, len: u64) -> Result<Vec<u8>> {
        let inode = self.read_inode(ino)?;
        self.read_inode_data(&inode, offset, len)
    }

    pub fn read_inode_data(&self, inode: &Inode, offset: u64, len: u64) -> Result<Vec<u8>> {
        if offset >= inode.size {
            return Ok(Vec::new());
        }
        let len = len.min(inode.size - offset);
        match inode.format {
            FMT_EMPTY => Ok(vec![0u8; len as usize]),
            FMT_EMBED => Ok(inode.payload[offset as usize..(offset + len) as usize].to_vec()),
            FMT_EXTENT | FMT_TREE => {
                let bs = self.sb.blocksize as u64;
                let t = self.txn();
                let mut out = vec![0u8; len as usize];
                let mut pos = offset;
                let end = offset + len;
                while pos < end {
                    let fb = pos / bs;
                    let in_blk = pos % bs;
                    match self.map_lookup(&t, inode, fb)? {
                        Map::Run { blk, len: rl } => {
                            let avail = rl * bs - in_blk;
                            let take = avail.min(end - pos);
                            let phys = self.resolve(blk)? + in_blk;
                            let o = (pos - offset) as usize;
                            self.dev.pread(&mut out[o..o + take as usize], phys)?;
                            pos += take;
                        }
                        Map::Hole(hl) => {
                            let skip = (hl.saturating_mul(bs).saturating_sub(in_blk))
                                .min(end - pos)
                                .max(1);
                            pos += skip; // zeros already in the buffer
                        }
                    }
                }
                Ok(out)
            }
            f => bail!("file format {f} unsupported"),
        }
    }

    pub fn read_file(&self, ino: u64) -> Result<Vec<u8>> {
        let inode = self.read_inode(ino)?;
        self.read_inode_data(&inode, 0, inode.size)
    }

    // --- file write -------------------------------------------------------------------------------

    /// Write `data` at `offset`. Metadata is journaled; data lands in place
    /// before the commit.
    pub fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let mut t = self.txn();
        let mut inode = self.read_inode_txn(&t, ino)?;
        if inode.is_dir() {
            bail!("is a directory");
        }
        if inode.flags & INODE_FLAG_IMMUTABLE != 0 {
            bail!("immutable file");
        }
        let end = offset + data.len() as u64;
        if matches!(inode.format, FMT_EMPTY | FMT_EMBED) && end <= INODE_PAYLOAD as u64 {
            inode.format = FMT_EMBED;
            inode.payload[offset as usize..end as usize].copy_from_slice(data);
            inode.size = inode.size.max(end);
            self.touch(&mut inode, false);
            self.write_inode_txn(&mut t, ino, &inode)?;
            self.commit(t)?;
            return Ok(data.len());
        }
        if inode.format == FMT_EMBED {
            self.lift_embed(&mut t, ino, &mut inode)?;
        }
        let bs = self.sb.blocksize as u64;
        let (ag, _) = blk_split(ino);
        let fb0 = offset / bs;
        let fbn = end.div_ceil(bs) - fb0;
        let runs = self.map_ensure(&mut t, ag, &mut inode, fb0, fbn)?;
        for (rfb, rblk, rlen, fresh) in runs {
            for b in 0..rlen {
                let fb = rfb + b;
                let blk_start = fb * bs;
                let a = offset.max(blk_start);
                let z = end.min(blk_start + bs);
                if a >= z {
                    continue;
                }
                let phys = self.resolve(rblk + b)?;
                let full_block = a == blk_start && z == blk_start + bs;
                if full_block {
                    self.dev.pwrite(&data[(a - offset) as usize..(z - offset) as usize], phys)?;
                } else {
                    let mut buf = vec![0u8; bs as usize];
                    if !fresh {
                        self.dev.pread(&mut buf, phys)?;
                    }
                    buf[(a - blk_start) as usize..(z - blk_start) as usize]
                        .copy_from_slice(&data[(a - offset) as usize..(z - offset) as usize]);
                    self.dev.pwrite(&buf, phys)?;
                }
                if fresh {
                    inode.nblocks += 1;
                }
            }
        }
        inode.size = inode.size.max(end);
        self.touch(&mut inode, false);
        self.write_inode_txn(&mut t, ino, &inode)?;
        self.commit(t)?;
        Ok(data.len())
    }

    /// Move EMBED payload data out to a block so the mapping formats apply.
    fn lift_embed(&mut self, t: &mut Txn, ino: u64, inode: &mut Inode) -> Result<()> {
        let size = inode.size as usize;
        let data: Vec<u8> = inode.payload[..size].to_vec();
        inode.format = FMT_EMPTY;
        inode.payload.fill(0);
        if size == 0 {
            return Ok(());
        }
        let (ag, _) = blk_split(ino);
        let runs = self.map_ensure(t, ag, inode, 0, 1)?;
        let mut buf = vec![0u8; self.sb.blocksize as usize];
        buf[..size].copy_from_slice(&data);
        self.dev.pwrite(&buf, self.resolve(runs[0].1)?)?;
        inode.nblocks += 1;
        Ok(())
    }

    fn touch(&self, inode: &mut Inode, ctime_only: bool) {
        let now = Ts::now();
        inode.ctime = now;
        if !ctime_only {
            inode.mtime = now;
        }
    }

    // --- truncate ---------------------------------------------------------------------------------

    /// Truncate to `size`. Shrinking frees whole mapping children beyond the
    /// cutoff (v1; partially covered runs keep their allocation) and zeroes
    /// kept blocks beyond EOF so a later extension reads zeros.
    pub fn truncate(&mut self, ino: u64, size: u64) -> Result<()> {
        let mut t = self.txn();
        let mut inode = self.read_inode_txn(&t, ino)?;
        if inode.is_dir() {
            bail!("is a directory");
        }
        if inode.flags & INODE_FLAG_IMMUTABLE != 0 {
            bail!("immutable file");
        }
        let old = inode.size;
        if size == old {
            return Ok(());
        }
        let bs = self.sb.blocksize as u64;
        if size < old {
            match inode.format {
                FMT_EMPTY => {}
                FMT_EMBED => {
                    inode.payload[size as usize..].fill(0);
                }
                FMT_EXTENT | FMT_TREE if size == 0 => {
                    self.map_free_all(&mut t, &mut inode)?;
                }
                FMT_EXTENT | FMT_TREE => {
                    let cutoff = size.div_ceil(bs);
                    let freed = self.map_truncate(&mut t, &mut inode, cutoff)?;
                    inode.nblocks = inode.nblocks.saturating_sub(freed);
                    self.zero_tail(&t, &inode, size, old)?;
                }
                f => bail!("file format {f} unsupported"),
            }
        }
        inode.size = size;
        self.touch(&mut inode, false);
        self.write_inode_txn(&mut t, ino, &inode)?;
        self.commit(t)
    }

    /// Zero mapped bytes in [from, to) so stale data can't reappear when the
    /// file grows back over a kept allocation.
    fn zero_tail(&self, t: &Txn, inode: &Inode, from: u64, to: u64) -> Result<()> {
        let bs = self.sb.blocksize as u64;
        let zero = vec![0u8; bs as usize];
        let mut pos = from;
        while pos < to {
            let fb = pos / bs;
            let in_blk = pos % bs;
            match self.map_lookup(t, inode, fb)? {
                Map::Run { blk, .. } => {
                    let phys = self.resolve(blk)? + in_blk;
                    let n = (bs - in_blk).min(to - pos);
                    self.dev.pwrite(&zero[..n as usize], phys)?;
                    pos += n;
                }
                Map::Hole(hl) => {
                    pos = pos.saturating_add(hl.saturating_mul(bs).saturating_sub(in_blk).max(1));
                }
            }
        }
        Ok(())
    }

    // --- namespace operations -------------------------------------------------------------------------

    fn new_inode(&self, mode: u16, nlink: u32) -> Inode {
        let now = Ts::now();
        Inode {
            format: FMT_EMPTY,
            mode,
            nlink,
            atime: now,
            mtime: now,
            ctime: now,
            btime: now,
            ..Default::default()
        }
    }

    /// Create a regular file. Returns the new inode number.
    pub fn create_at(&mut self, parent: u64, name: &str, mode: u16) -> Result<u64> {
        self.mknod(parent, name, 0o100000 | (mode & 0o7777), DT_FILE, 1, None)
    }

    pub fn mkdir_at(&mut self, parent: u64, name: &str, mode: u16) -> Result<u64> {
        self.mknod(parent, name, 0o040000 | (mode & 0o7777), DT_DIR, 2, None)
    }

    pub fn symlink_at(&mut self, parent: u64, name: &str, target: &str) -> Result<u64> {
        self.mknod(parent, name, 0o120777, DT_FILE, 1, Some(target.as_bytes()))
    }

    fn mknod(
        &mut self,
        parent: u64,
        name: &str,
        mode: u16,
        ftype: u8,
        nlink: u32,
        data: Option<&[u8]>,
    ) -> Result<u64> {
        check_name(name)?;
        let mut t = self.txn();
        let mut pinode = self.read_inode_txn(&t, parent)?;
        if !pinode.is_dir() {
            bail!("parent is not a directory");
        }
        if self.dir_find(&t, &pinode, name)?.is_some() {
            bail!("{name}: already exists");
        }
        let (ag, _) = blk_split(parent);
        let ino = self.alloc_inode(&mut t, ag)?;
        let mut inode = self.new_inode(mode, nlink);
        if let Some(d) = data {
            if d.len() > INODE_PAYLOAD {
                bail!("symlink target too long (v1 limit {INODE_PAYLOAD})");
            }
            inode.format = FMT_EMBED;
            inode.payload[..d.len()].copy_from_slice(d);
            inode.size = d.len() as u64;
        }
        self.write_inode_txn(&mut t, ino, &inode)?;
        self.dir_insert(&mut t, parent, &mut pinode, &DirEntry { ino, ftype, name: name.into() })?;
        if ftype == DT_DIR {
            pinode.nlink += 1;
        }
        self.touch(&mut pinode, false);
        self.write_inode_txn(&mut t, parent, &pinode)?;
        self.commit(t)?;
        Ok(ino)
    }

    pub fn link_at(&mut self, ino: u64, parent: u64, name: &str) -> Result<()> {
        check_name(name)?;
        let mut t = self.txn();
        let mut inode = self.read_inode_txn(&t, ino)?;
        if inode.is_dir() {
            bail!("hard links to directories are not allowed");
        }
        let mut pinode = self.read_inode_txn(&t, parent)?;
        self.dir_insert(
            &mut t,
            parent,
            &mut pinode,
            &DirEntry { ino, ftype: DT_FILE, name: name.into() },
        )?;
        inode.nlink += 1;
        self.touch(&mut inode, true);
        self.touch(&mut pinode, false);
        self.write_inode_txn(&mut t, ino, &inode)?;
        self.write_inode_txn(&mut t, parent, &pinode)?;
        self.commit(t)
    }

    pub fn unlink_at(&mut self, parent: u64, name: &str) -> Result<()> {
        self.remove_common(parent, name, false)
    }

    pub fn rmdir_at(&mut self, parent: u64, name: &str) -> Result<()> {
        self.remove_common(parent, name, true)
    }

    fn remove_common(&mut self, parent: u64, name: &str, want_dir: bool) -> Result<()> {
        let mut t = self.txn();
        let mut pinode = self.read_inode_txn(&t, parent)?;
        let ent = self
            .dir_find(&t, &pinode, name)?
            .ok_or_else(|| anyhow!("{name}: no such file or directory"))?;
        let mut inode = self.read_inode_txn(&t, ent.ino)?;
        if want_dir != inode.is_dir() {
            bail!("{name}: {}", if want_dir { "not a directory" } else { "is a directory" });
        }
        if inode.flags & INODE_FLAG_IMMUTABLE != 0 {
            bail!("{name}: immutable file");
        }
        if want_dir {
            if self.dir_count(&t, &inode)? != 0 {
                bail!("{name}: directory not empty");
            }
            self.map_free_all(&mut t, &mut inode)?;
            self.free_inode(&mut t, ent.ino)?;
            pinode.nlink = pinode.nlink.saturating_sub(1);
        } else {
            inode.nlink = inode.nlink.saturating_sub(1);
            if inode.nlink == 0 {
                self.map_free_all(&mut t, &mut inode)?;
                self.free_inode(&mut t, ent.ino)?;
            } else {
                self.touch(&mut inode, true);
                self.write_inode_txn(&mut t, ent.ino, &inode)?;
            }
        }
        self.dir_remove(&mut t, &mut pinode, name)?;
        self.touch(&mut pinode, false);
        self.write_inode_txn(&mut t, parent, &pinode)?;
        self.commit(t)
    }

    pub fn rename_at(&mut self, p1: u64, n1: &str, p2: u64, n2: &str) -> Result<()> {
        check_name(n2)?;
        let mut t = self.txn();
        let mut src = self.read_inode_txn(&t, p1)?;
        let ent = self
            .dir_find(&t, &src, n1)?
            .ok_or_else(|| anyhow!("{n1}: no such file or directory"))?;
        // replace target if it exists (POSIX)
        let mut dst = if p1 == p2 { src.clone() } else { self.read_inode_txn(&t, p2)? };
        if let Some(existing) = self.dir_find(&t, &dst, n2)? {
            if existing.ino != ent.ino {
                let mut ei = self.read_inode_txn(&t, existing.ino)?;
                if ei.flags & INODE_FLAG_IMMUTABLE != 0 {
                    bail!("{n2}: immutable file");
                }
                // POSIX: a directory may only replace a directory, a
                // non-directory only a non-directory
                if ei.is_dir() != (ent.ftype == DT_DIR) {
                    bail!(
                        "{n2}: {}",
                        if ei.is_dir() { "is a directory" } else { "not a directory" }
                    );
                }
                if ei.is_dir() {
                    if self.dir_count(&t, &ei)? != 0 {
                        bail!("{n2}: directory not empty");
                    }
                    self.map_free_all(&mut t, &mut ei)?;
                    self.free_inode(&mut t, existing.ino)?;
                    dst.nlink = dst.nlink.saturating_sub(1);
                } else {
                    ei.nlink = ei.nlink.saturating_sub(1);
                    if ei.nlink == 0 {
                        self.map_free_all(&mut t, &mut ei)?;
                        self.free_inode(&mut t, existing.ino)?;
                    } else {
                        self.write_inode_txn(&mut t, existing.ino, &ei)?;
                    }
                }
                self.dir_remove(&mut t, &mut dst, n2)?;
            } else {
                return Ok(()); // same file
            }
        }
        if p1 == p2 {
            self.dir_remove(&mut t, &mut dst, n1)?;
            self.dir_insert(
                &mut t,
                p2,
                &mut dst,
                &DirEntry { ino: ent.ino, ftype: ent.ftype, name: n2.into() },
            )?;
            self.touch(&mut dst, false);
            self.write_inode_txn(&mut t, p2, &dst)?;
        } else {
            self.dir_remove(&mut t, &mut src, n1)?;
            self.dir_insert(
                &mut t,
                p2,
                &mut dst,
                &DirEntry { ino: ent.ino, ftype: ent.ftype, name: n2.into() },
            )?;
            if ent.ftype == DT_DIR {
                src.nlink = src.nlink.saturating_sub(1);
                dst.nlink += 1;
            }
            self.touch(&mut src, false);
            self.touch(&mut dst, false);
            self.write_inode_txn(&mut t, p1, &src)?;
            self.write_inode_txn(&mut t, p2, &dst)?;
        }
        self.commit(t)
    }

    pub fn readlink(&self, ino: u64) -> Result<String> {
        let inode = self.read_inode(ino)?;
        if inode.mode & 0o170000 != 0o120000 {
            bail!("not a symlink");
        }
        let data = self.read_inode_data(&inode, 0, inode.size)?;
        String::from_utf8(data).map_err(|_| anyhow!("invalid symlink target"))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn setattr(
        &mut self,
        ino: u64,
        mode: Option<u16>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<Ts>,
        mtime: Option<Ts>,
    ) -> Result<Inode> {
        if let Some(s) = size {
            let inode = self.read_inode(ino)?;
            if !inode.is_dir() && inode.size != s {
                self.truncate(ino, s)?;
            }
        }
        let mut t = self.txn();
        let mut inode = self.read_inode_txn(&t, ino)?;
        if let Some(m) = mode {
            inode.mode = (inode.mode & 0o170000) | (m & 0o7777);
        }
        if let Some(u) = uid {
            inode.uid = u;
        }
        if let Some(g) = gid {
            inode.gid = g;
        }
        if let Some(a) = atime {
            inode.atime = a;
        }
        if let Some(m) = mtime {
            inode.mtime = m;
        }
        inode.ctime = Ts::now();
        self.write_inode_txn(&mut t, ino, &inode)?;
        self.commit(t)?;
        Ok(inode)
    }

    // --- path-based wrappers -------------------------------------------------------------------------

    pub fn create(&mut self, path: &str, mode: u16) -> Result<u64> {
        let (p, n) = self.resolve_parent(path)?;
        self.create_at(p, &n, mode)
    }
    pub fn mkdir(&mut self, path: &str, mode: u16) -> Result<u64> {
        let (p, n) = self.resolve_parent(path)?;
        self.mkdir_at(p, &n, mode)
    }
    pub fn unlink(&mut self, path: &str) -> Result<()> {
        let (p, n) = self.resolve_parent(path)?;
        self.unlink_at(p, &n)
    }
    pub fn rmdir(&mut self, path: &str) -> Result<()> {
        let (p, n) = self.resolve_parent(path)?;
        self.rmdir_at(p, &n)
    }
    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let f = from.trim_matches('/');
        let t = to.trim_matches('/');
        if t == f || t.starts_with(&format!("{f}/")) {
            bail!("cannot move a directory into itself");
        }
        let (p1, n1) = self.resolve_parent(from)?;
        let (p2, n2) = self.resolve_parent(to)?;
        self.rename_at(p1, &n1, p2, &n2)
    }

    /// Create (or refuse to overwrite) a file at `path` with `data`.
    pub fn import(&mut self, path: &str, data: &[u8], mode: u16) -> Result<u64> {
        let ino = self.create(path, mode)?;
        self.write(ino, 0, data)?;
        Ok(ino)
    }

    /// Replace the boot kernel in place. The new image must fit the region
    /// reserved at mkfs (the kernel.bin extent); the inode stays immutable
    /// to every other write path.
    pub fn kernel_update(&mut self, data: &[u8]) -> Result<()> {
        if self.sb.kernel_offset == 0 {
            bail!("no kernel region (create one with mkfs --kernel)");
        }
        let ino = self
            .dir_lookup(self.sb.root_ino, "kernel.bin")?
            .ok_or_else(|| anyhow!("kernel.bin missing from the root directory"))?;
        let mut t = self.txn();
        let mut inode = self.read_inode_txn(&t, ino)?;
        let extents = extents_parse(&inode.payload);
        let capacity = extents.first().map(|e| e.blocks as u64).unwrap_or(0)
            * self.sb.blocksize as u64;
        if data.len() as u64 > capacity {
            bail!("kernel ({} bytes) exceeds the reserved region ({capacity} bytes)", data.len());
        }
        // ordered: kernel bytes land (zero-padded to the region) before the
        // metadata that describes them commits
        let mut padded = data.to_vec();
        padded.resize(capacity as usize, 0);
        self.dev.pwrite(&padded, self.sb.kernel_offset)?;
        self.dev.sync()?;
        inode.size = data.len() as u64;
        self.touch(&mut inode, false);
        self.write_inode_txn(&mut t, ino, &inode)?;
        self.sb.kernel_end = self.sb.kernel_offset + data.len() as u64;
        self.commit(t)
    }
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        bail!("invalid name {name:?}");
    }
    if name.encode_utf16().count() > 255 {
        bail!("name too long");
    }
    Ok(())
}
