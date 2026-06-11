//! 5FS v2 on-disk format. All integers big-endian. See doc/0-principles.md.

pub const VERSION: u32 = 2;

pub const SB_MAGIC: [u8; 4] = *b"5FSB";
pub const AG_MAGIC: [u8; 4] = *b"5FSH";
pub const AGMAP_MAGIC: [u8; 4] = *b"5FSM";
pub const INODE_MAGIC: u16 = 0x494e; // "IN"

pub const DEFAULT_BLOCKSIZE: u32 = 4096;
pub const DEFAULT_INODESIZE: u16 = 256;
/// Level-0 allocation cell, in blocks (1 MiB at 4 KiB blocks).
pub const CELL_BLOCKS: u32 = 256;

pub const SB_SIZE: usize = 512;
pub const AGHDR_SIZE: usize = 64;
pub const INODE_SIZE: usize = 256;
pub const INODE_PAYLOAD: usize = 128;

// Cell states (2 bits), doc/0-principles.md
pub const CELL_FREE: u8 = 0;
pub const CELL_RSVD: u8 = 1;
pub const CELL_FULL: u8 = 2;
pub const CELL_REFINED: u8 = 3;

// Inode payload formats, doc/3-inodes.md
pub const FMT_EMPTY: u8 = 1;
pub const FMT_EMBED: u8 = 2;
pub const FMT_EXTENT: u8 = 3;
pub const FMT_TREE: u8 = 4;

// AG map entry flags, doc/1-layout.md
pub const AGF_PRESENT: u16 = 1;
pub const AGF_IMMOVABLE: u16 = 2;
pub const AGF_RETIRED: u16 = 4;

pub fn blk_addr(ag: u32, local: u32) -> u64 {
    ((ag as u64) << 32) | local as u64
}
pub fn blk_split(addr: u64) -> (u32, u32) {
    ((addr >> 32) as u32, addr as u32)
}
pub fn ino_addr(ag: u32, slot: u32) -> u64 {
    blk_addr(ag, slot)
}

// --- byte helpers -----------------------------------------------------------

pub fn put_u16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_be_bytes());
}
pub fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_be_bytes());
}
pub fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_be_bytes());
}
pub fn put_i64(b: &mut [u8], off: usize, v: i64) {
    b[off..off + 8].copy_from_slice(&v.to_be_bytes());
}
pub fn get_u16(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes(b[off..off + 2].try_into().unwrap())
}
pub fn get_u32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes(b[off..off + 4].try_into().unwrap())
}
pub fn get_u64(b: &[u8], off: usize) -> u64 {
    u64::from_be_bytes(b[off..off + 8].try_into().unwrap())
}
pub fn get_i64(b: &[u8], off: usize) -> i64 {
    i64::from_be_bytes(b[off..off + 8].try_into().unwrap())
}

/// CRC32C with the 4-byte checksum field at `csum_off` treated as zero.
pub fn csum(buf: &[u8], csum_off: usize) -> u32 {
    let c = crc32c::crc32c(&buf[..csum_off]);
    let c = crc32c::crc32c_append(c, &[0, 0, 0, 0]);
    crc32c::crc32c_append(c, &buf[csum_off + 4..])
}

// --- 2-bit state arrays -----------------------------------------------------

pub fn cell_get(buf: &[u8], idx: u64) -> u8 {
    let byte = buf[(idx / 4) as usize];
    (byte >> ((idx % 4) * 2)) & 3
}
pub fn cell_set(buf: &mut [u8], idx: u64, state: u8) {
    let i = (idx / 4) as usize;
    let sh = (idx % 4) * 2;
    buf[i] = (buf[i] & !(3 << sh)) | ((state & 3) << sh);
}

// --- superblock -------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct Superblock {
    pub compat: u64,
    pub ro_compat: u64,
    pub incompat: u64,
    pub blocksize: u32,
    pub inodesize: u16,
    pub uuid: [u8; 16],
    pub disk_name: [u16; 16],
    pub root_ino: u64,
    pub next_ag: u32,
    pub agmap_offset: u64, // physical bytes
    pub agmap_length: u64, // bytes per copy
    pub journal_start: u64, // block address
    pub journal_length: u64, // blocks
    pub kernel_offset: u64, // physical bytes, 0 = none
    pub kernel_end: u64,
    pub free_blocks: u64,
    pub rsvd_blocks: u64,
    pub full_blocks: u64,
    pub gen: u64,
}

impl Superblock {
    pub fn to_bytes(&self) -> [u8; SB_SIZE] {
        let mut b = [0u8; SB_SIZE];
        b[0..4].copy_from_slice(&SB_MAGIC);
        // csum at 4, filled last
        put_u32(&mut b, 8, VERSION);
        put_u64(&mut b, 12, self.compat);
        put_u64(&mut b, 20, self.ro_compat);
        put_u64(&mut b, 28, self.incompat);
        put_u32(&mut b, 36, self.blocksize);
        put_u16(&mut b, 40, self.inodesize);
        b[42..58].copy_from_slice(&self.uuid);
        for (i, u) in self.disk_name.iter().enumerate() {
            put_u16(&mut b, 58 + i * 2, *u);
        }
        put_u64(&mut b, 90, self.root_ino);
        put_u32(&mut b, 98, self.next_ag);
        put_u64(&mut b, 102, self.agmap_offset);
        put_u64(&mut b, 110, self.agmap_length);
        put_u64(&mut b, 118, self.journal_start);
        put_u64(&mut b, 126, self.journal_length);
        put_u64(&mut b, 134, self.kernel_offset);
        put_u64(&mut b, 142, self.kernel_end);
        put_u64(&mut b, 150, self.free_blocks);
        put_u64(&mut b, 158, self.rsvd_blocks);
        put_u64(&mut b, 166, self.full_blocks);
        put_u64(&mut b, 174, self.gen);
        let c = csum(&b, 4);
        put_u32(&mut b, 4, c);
        b
    }

    pub fn parse(b: &[u8]) -> Result<Self, String> {
        if b.len() < SB_SIZE {
            return Err("superblock: short read".into());
        }
        let b = &b[..SB_SIZE];
        if b[0..4] != SB_MAGIC {
            return Err("superblock: bad magic".into());
        }
        if get_u32(b, 4) != csum(b, 4) {
            return Err("superblock: bad checksum".into());
        }
        let ver = get_u32(b, 8);
        if ver != VERSION {
            return Err(format!("superblock: unsupported version {ver}"));
        }
        let mut sb = Superblock {
            compat: get_u64(b, 12),
            ro_compat: get_u64(b, 20),
            incompat: get_u64(b, 28),
            blocksize: get_u32(b, 36),
            inodesize: get_u16(b, 40),
            ..Default::default()
        };
        sb.uuid.copy_from_slice(&b[42..58]);
        for i in 0..16 {
            sb.disk_name[i] = get_u16(b, 58 + i * 2);
        }
        sb.root_ino = get_u64(b, 90);
        sb.next_ag = get_u32(b, 98);
        sb.agmap_offset = get_u64(b, 102);
        sb.agmap_length = get_u64(b, 110);
        sb.journal_start = get_u64(b, 118);
        sb.journal_length = get_u64(b, 126);
        sb.kernel_offset = get_u64(b, 134);
        sb.kernel_end = get_u64(b, 142);
        sb.free_blocks = get_u64(b, 150);
        sb.rsvd_blocks = get_u64(b, 158);
        sb.full_blocks = get_u64(b, 166);
        sb.gen = get_u64(b, 174);
        if !sb.blocksize.is_power_of_two() || sb.blocksize < 512 {
            return Err("superblock: bad blocksize".into());
        }
        Ok(sb)
    }

    pub fn label(&self) -> String {
        let units: Vec<u16> = self.disk_name.iter().copied().take_while(|&u| u != 0).collect();
        String::from_utf16_lossy(&units)
    }
}

// --- AG header ---------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct AgHeader {
    pub ag_num: u32,
    pub gen: u64,
    pub length: u32, // blocks
    pub free_blocks: u32,
    pub rsvd_blocks: u32,
    pub full_blocks: u32,
    pub alloc_root: u32, // block offset within AG
    pub ino_hint: u32,
    pub data_hint: u32,
}

impl AgHeader {
    pub fn to_bytes(&self) -> [u8; AGHDR_SIZE] {
        let mut b = [0u8; AGHDR_SIZE];
        b[0..4].copy_from_slice(&AG_MAGIC);
        put_u32(&mut b, 8, self.ag_num);
        put_u64(&mut b, 12, self.gen);
        put_u32(&mut b, 20, self.length);
        put_u32(&mut b, 24, self.free_blocks);
        put_u32(&mut b, 28, self.rsvd_blocks);
        put_u32(&mut b, 32, self.full_blocks);
        put_u32(&mut b, 36, self.alloc_root);
        put_u32(&mut b, 40, self.ino_hint);
        put_u32(&mut b, 44, self.data_hint);
        let c = csum(&b, 4);
        put_u32(&mut b, 4, c);
        b
    }

    pub fn parse(b: &[u8]) -> Result<Self, String> {
        if b.len() < AGHDR_SIZE {
            return Err("ag header: short read".into());
        }
        let b = &b[..AGHDR_SIZE];
        if b[0..4] != AG_MAGIC {
            return Err("ag header: bad magic".into());
        }
        if get_u32(b, 4) != csum(b, 4) {
            return Err("ag header: bad checksum".into());
        }
        Ok(AgHeader {
            ag_num: get_u32(b, 8),
            gen: get_u64(b, 12),
            length: get_u32(b, 20),
            free_blocks: get_u32(b, 24),
            rsvd_blocks: get_u32(b, 28),
            full_blocks: get_u32(b, 32),
            alloc_root: get_u32(b, 36),
            ino_hint: get_u32(b, 40),
            data_hint: get_u32(b, 44),
        })
    }
}

/// Allocator root table geometry: (level-0 cells, table size in blocks).
pub fn rt_geometry(ag_blocks: u32, blocksize: u32) -> (u64, u32) {
    let cells = (ag_blocks as u64).div_ceil(CELL_BLOCKS as u64);
    let bytes = cells.div_ceil(4);
    (cells, bytes.div_ceil(blocksize as u64) as u32)
}

// --- AG map -------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AgSegment {
    pub ag_block: u32,   // first AG-space block this segment maps
    pub blocks: u32,     // length in blocks
    pub dev_offset: u64, // physical byte offset
}

#[derive(Debug, Clone)]
pub struct AgEntry {
    pub flags: u16,
    pub length: u32, // AG length in blocks
    pub segs: Vec<AgSegment>,
}

#[derive(Debug, Clone, Default)]
pub struct AgMap {
    pub gen: u64,
    pub entries: Vec<AgEntry>,
}

const AGMAP_HDR: usize = 24; // magic(4) csum(4) gen(8) entry_count(4) used_len(4)

impl AgMap {
    pub fn to_bytes(&self, capacity: usize) -> Result<Vec<u8>, String> {
        let mut b = vec![0u8; capacity];
        b[0..4].copy_from_slice(&AGMAP_MAGIC);
        put_u64(&mut b, 8, self.gen);
        put_u32(&mut b, 16, self.entries.len() as u32);
        let mut off = AGMAP_HDR;
        for e in &self.entries {
            if off + 8 + e.segs.len() * 16 > capacity {
                return Err("ag map: capacity exceeded".into());
            }
            put_u16(&mut b, off, e.flags);
            put_u16(&mut b, off + 2, e.segs.len() as u16);
            put_u32(&mut b, off + 4, e.length);
            off += 8;
            for s in &e.segs {
                put_u32(&mut b, off, s.ag_block);
                put_u32(&mut b, off + 4, s.blocks);
                put_u64(&mut b, off + 8, s.dev_offset);
                off += 16;
            }
        }
        put_u32(&mut b, 20, off as u32);
        let c = csum(&b[..off], 4);
        put_u32(&mut b, 4, c);
        Ok(b)
    }

    pub fn parse(b: &[u8]) -> Result<Self, String> {
        if b.len() < AGMAP_HDR || b[0..4] != AGMAP_MAGIC {
            return Err("ag map: bad magic".into());
        }
        let used = get_u32(b, 20) as usize;
        if used < AGMAP_HDR || used > b.len() {
            return Err("ag map: bad length".into());
        }
        if get_u32(b, 4) != csum(&b[..used], 4) {
            return Err("ag map: bad checksum".into());
        }
        let count = get_u32(b, 16) as usize;
        let mut entries = Vec::with_capacity(count);
        let mut off = AGMAP_HDR;
        for _ in 0..count {
            if off + 8 > used {
                return Err("ag map: truncated entry".into());
            }
            let flags = get_u16(b, off);
            let nseg = get_u16(b, off + 2) as usize;
            let length = get_u32(b, off + 4);
            off += 8;
            let mut segs = Vec::with_capacity(nseg);
            for _ in 0..nseg {
                if off + 16 > used {
                    return Err("ag map: truncated segment".into());
                }
                segs.push(AgSegment {
                    ag_block: get_u32(b, off),
                    blocks: get_u32(b, off + 4),
                    dev_offset: get_u64(b, off + 8),
                });
                off += 16;
            }
            entries.push(AgEntry { flags, length, segs });
        }
        Ok(AgMap { gen: get_u64(b, 8), entries })
    }

    /// Resolve a block address to a physical byte offset.
    pub fn resolve(&self, addr: u64, blocksize: u32) -> Option<u64> {
        let (ag, local) = blk_split(addr);
        let e = self.entries.get(ag as usize)?;
        if e.flags & AGF_PRESENT == 0 || local >= e.length {
            return None;
        }
        for s in &e.segs {
            if local >= s.ag_block && local < s.ag_block + s.blocks {
                return Some(s.dev_offset + (local - s.ag_block) as u64 * blocksize as u64);
            }
        }
        None
    }
}

// --- inode --------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Ts {
    pub sec: i64,
    pub nsec: u32,
}

impl Ts {
    pub fn now() -> Self {
        match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => Ts { sec: d.as_secs() as i64, nsec: d.subsec_nanos() },
            Err(_) => Ts::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Inode {
    pub format: u8,
    pub mode: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub flags: u32,
    pub gen: u32,
    pub size: u64,
    pub nblocks: u64,
    pub atime: Ts,
    pub mtime: Ts,
    pub ctime: Ts,
    pub btime: Ts,
    pub xattr: u64,
    pub payload: [u8; INODE_PAYLOAD],
}

impl Default for Inode {
    fn default() -> Self {
        Inode {
            format: FMT_EMPTY,
            mode: 0,
            nlink: 0,
            uid: 0,
            gid: 0,
            flags: 0,
            gen: 0,
            size: 0,
            nblocks: 0,
            atime: Ts::default(),
            mtime: Ts::default(),
            ctime: Ts::default(),
            btime: Ts::default(),
            xattr: 0,
            payload: [0; INODE_PAYLOAD],
        }
    }
}

fn put_ts(b: &mut [u8], off: usize, t: Ts) {
    put_i64(b, off, t.sec);
    put_u32(b, off + 8, t.nsec);
}
fn get_ts(b: &[u8], off: usize) -> Ts {
    Ts { sec: get_i64(b, off), nsec: get_u32(b, off + 8) }
}

impl Inode {
    pub fn to_bytes(&self) -> [u8; INODE_SIZE] {
        let mut b = [0u8; INODE_SIZE];
        put_u16(&mut b, 0, INODE_MAGIC);
        b[2] = 2; // version
        b[3] = self.format;
        put_u16(&mut b, 8, self.mode);
        put_u32(&mut b, 10, self.nlink);
        put_u32(&mut b, 14, self.uid);
        put_u32(&mut b, 18, self.gid);
        put_u32(&mut b, 22, self.flags);
        put_u32(&mut b, 26, self.gen);
        put_u64(&mut b, 30, self.size);
        put_u64(&mut b, 38, self.nblocks);
        put_ts(&mut b, 46, self.atime);
        put_ts(&mut b, 58, self.mtime);
        put_ts(&mut b, 70, self.ctime);
        put_ts(&mut b, 82, self.btime);
        put_u64(&mut b, 94, self.xattr);
        b[128..].copy_from_slice(&self.payload);
        let c = csum(&b, 4);
        put_u32(&mut b, 4, c);
        b
    }

    pub fn parse(b: &[u8]) -> Result<Self, String> {
        if b.len() < INODE_SIZE {
            return Err("inode: short read".into());
        }
        let b = &b[..INODE_SIZE];
        if get_u16(b, 0) != INODE_MAGIC {
            return Err("inode: bad magic".into());
        }
        if get_u32(b, 4) != csum(b, 4) {
            return Err("inode: bad checksum".into());
        }
        if b[2] != 2 {
            return Err(format!("inode: unsupported version {}", b[2]));
        }
        let mut ino = Inode {
            format: b[3],
            mode: get_u16(b, 8),
            nlink: get_u32(b, 10),
            uid: get_u32(b, 14),
            gid: get_u32(b, 18),
            flags: get_u32(b, 22),
            gen: get_u32(b, 26),
            size: get_u64(b, 30),
            nblocks: get_u64(b, 38),
            atime: get_ts(b, 46),
            mtime: get_ts(b, 58),
            ctime: get_ts(b, 70),
            btime: get_ts(b, 82),
            xattr: get_u64(b, 94),
            ..Default::default()
        };
        ino.payload.copy_from_slice(&b[128..]);
        Ok(ino)
    }

    pub fn is_dir(&self) -> bool {
        self.mode & 0o170000 == 0o040000
    }
    pub fn is_file(&self) -> bool {
        self.mode & 0o170000 == 0o100000
    }
}

/// One inline extent record (doc/4-extents.md), 20 bytes in the inode payload.
#[derive(Debug, Clone, Copy)]
pub struct Extent {
    pub file_block: u64,
    pub blk: u64,   // block address
    pub blocks: u32,
}

pub const EXTENT_REC: usize = 20;
pub const MAX_INLINE_EXTENTS: usize = INODE_PAYLOAD / EXTENT_REC;

pub fn extents_parse(payload: &[u8]) -> Vec<Extent> {
    let mut v = Vec::new();
    for i in 0..MAX_INLINE_EXTENTS {
        let off = i * EXTENT_REC;
        let blocks = get_u32(payload, off + 16);
        if blocks == 0 {
            break;
        }
        v.push(Extent {
            file_block: get_u64(payload, off),
            blk: get_u64(payload, off + 8),
            blocks,
        });
    }
    v
}

pub fn extents_store(payload: &mut [u8; INODE_PAYLOAD], extents: &[Extent]) -> Result<(), String> {
    if extents.len() > MAX_INLINE_EXTENTS {
        return Err("too many inline extents".into());
    }
    payload.fill(0);
    for (i, e) in extents.iter().enumerate() {
        let off = i * EXTENT_REC;
        put_u64(payload, off, e.file_block);
        put_u64(payload, off + 8, e.blk);
        put_u32(payload, off + 16, e.blocks);
    }
    Ok(())
}

// --- embedded directory entries ------------------------------------------------
// EMBED payload: repeated { ino u64, type u8, name_len u8, name UTF-16BE }.
// End of list at ino == 0 or end of payload.

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub ino: u64,
    pub ftype: u8, // 1 = file, 2 = dir (mode type bits >> 12)
    pub name: String,
}

pub const DT_FILE: u8 = 1;
pub const DT_DIR: u8 = 2;

pub fn dir_parse(payload: &[u8]) -> Vec<DirEntry> {
    let mut v = Vec::new();
    let mut off = 0;
    while off + 10 <= payload.len() {
        let ino = get_u64(payload, off);
        if ino == 0 {
            break;
        }
        let ftype = payload[off + 8];
        let nlen = payload[off + 9] as usize;
        off += 10;
        if off + nlen * 2 > payload.len() {
            break;
        }
        let units: Vec<u16> = (0..nlen).map(|i| get_u16(payload, off + i * 2)).collect();
        off += nlen * 2;
        v.push(DirEntry { ino, ftype, name: String::from_utf16_lossy(&units) });
    }
    v
}

pub fn dir_append(payload: &mut [u8; INODE_PAYLOAD], e: &DirEntry) -> Result<(), String> {
    let units: Vec<u16> = e.name.encode_utf16().collect();
    if units.len() > 255 {
        return Err("name too long".into());
    }
    // find end of existing entries
    let mut off = 0;
    for ent in dir_parse(payload) {
        off += 10 + ent.name.encode_utf16().count() * 2;
    }
    let need = 10 + units.len() * 2;
    if off + need > INODE_PAYLOAD {
        return Err("embedded directory full".into());
    }
    put_u64(payload, off, e.ino);
    payload[off + 8] = e.ftype;
    payload[off + 9] = units.len() as u8;
    for (i, u) in units.iter().enumerate() {
        put_u16(payload, off + 10 + i * 2, *u);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sb_roundtrip() {
        let mut sb = Superblock {
            blocksize: 4096,
            inodesize: 256,
            root_ino: 48,
            next_ag: 1,
            agmap_offset: 16384,
            agmap_length: 65536,
            journal_start: 36,
            journal_length: 1024,
            free_blocks: 2560,
            rsvd_blocks: 1536,
            gen: 1,
            ..Default::default()
        };
        sb.uuid = *b"0123456789abcdef";
        for (i, u) in "test".encode_utf16().enumerate() {
            sb.disk_name[i] = u;
        }
        let b = sb.to_bytes();
        let p = Superblock::parse(&b).unwrap();
        assert_eq!(p.root_ino, 48);
        assert_eq!(p.label(), "test");
        assert_eq!(p.journal_length, 1024);
        // corrupt one byte -> checksum failure
        let mut bad = b;
        bad[100] ^= 1;
        assert!(Superblock::parse(&bad).is_err());
    }

    #[test]
    fn aghdr_roundtrip() {
        let h = AgHeader {
            ag_num: 3,
            gen: 7,
            length: 16384,
            free_blocks: 16000,
            rsvd_blocks: 384,
            alloc_root: 1,
            ..Default::default()
        };
        let b = h.to_bytes();
        let p = AgHeader::parse(&b).unwrap();
        assert_eq!(p.ag_num, 3);
        assert_eq!(p.free_blocks, 16000);
    }

    #[test]
    fn agmap_roundtrip_resolve() {
        let map = AgMap {
            gen: 2,
            entries: vec![
                AgEntry {
                    flags: AGF_PRESENT | AGF_IMMOVABLE,
                    length: 4096,
                    segs: vec![AgSegment { ag_block: 0, blocks: 4096, dev_offset: 0 }],
                },
                AgEntry {
                    flags: AGF_PRESENT,
                    length: 8192,
                    segs: vec![
                        AgSegment { ag_block: 0, blocks: 4096, dev_offset: 4096 * 4096 },
                        AgSegment { ag_block: 4096, blocks: 4096, dev_offset: 10000 * 4096 },
                    ],
                },
            ],
        };
        let b = map.to_bytes(65536).unwrap();
        let p = AgMap::parse(&b).unwrap();
        assert_eq!(p.entries.len(), 2);
        assert_eq!(p.resolve(blk_addr(0, 5), 4096), Some(5 * 4096));
        assert_eq!(p.resolve(blk_addr(1, 4097), 4096), Some(10001 * 4096));
        assert_eq!(p.resolve(blk_addr(1, 9000), 4096), None);
        assert_eq!(p.resolve(blk_addr(2, 0), 4096), None);
    }

    #[test]
    fn inode_roundtrip() {
        let mut ino = Inode {
            format: FMT_EMBED,
            mode: 0o100644,
            nlink: 1,
            size: 5,
            mtime: Ts { sec: 1_750_000_000, nsec: 42 },
            ..Default::default()
        };
        ino.payload[..5].copy_from_slice(b"hello");
        let b = ino.to_bytes();
        let p = Inode::parse(&b).unwrap();
        assert!(p.is_file());
        assert_eq!(&p.payload[..5], b"hello");
        assert_eq!(p.mtime.sec, 1_750_000_000);
    }

    #[test]
    fn cells() {
        let mut buf = [0u8; 8];
        cell_set(&mut buf, 0, CELL_RSVD);
        cell_set(&mut buf, 5, CELL_FULL);
        cell_set(&mut buf, 31, CELL_REFINED);
        assert_eq!(cell_get(&buf, 0), CELL_RSVD);
        assert_eq!(cell_get(&buf, 5), CELL_FULL);
        assert_eq!(cell_get(&buf, 31), CELL_REFINED);
        assert_eq!(cell_get(&buf, 1), CELL_FREE);
        cell_set(&mut buf, 5, CELL_FREE);
        assert_eq!(cell_get(&buf, 5), CELL_FREE);
    }

    #[test]
    fn dir_embed() {
        let mut payload = [0u8; INODE_PAYLOAD];
        dir_append(&mut payload, &DirEntry { ino: 49, ftype: DT_FILE, name: "héllo.txt".into() })
            .unwrap();
        dir_append(&mut payload, &DirEntry { ino: 50, ftype: DT_DIR, name: "日本語".into() })
            .unwrap();
        let v = dir_parse(&payload);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "héllo.txt");
        assert_eq!(v[1].name, "日本語");
        assert_eq!(v[1].ino, 50);
    }

    #[test]
    fn extents() {
        let mut payload = [0u8; INODE_PAYLOAD];
        extents_store(
            &mut (payload),
            &[Extent { file_block: 0, blk: blk_addr(0, 1280), blocks: 2 }],
        )
        .unwrap();
        let v = extents_parse(&payload);
        assert_eq!(v.len(), 1);
        assert_eq!(blk_split(v[0].blk), (0, 1280));
    }

    #[test]
    fn rt_geom() {
        // 16 MiB AG at 4 KiB blocks = 4096 blocks = 16 cells -> 4 bytes -> 1 block
        assert_eq!(rt_geometry(4096, 4096), (16, 1));
        // 64 GiB AG = 16 Mi blocks = 65536 cells -> 16 KiB -> 4 blocks
        assert_eq!(rt_geometry(16 * 1024 * 1024, 4096), (65536, 4));
    }
}
