//! Refinement-tree allocator (doc/2-allocation.md).
//!
//! Hierarchy at default sizes: L0 cell = 256 blocks (1 MiB), refined by 16
//! into L1 cells (16 blocks), refined by 16 into L2 cells (one block).
//! States live in the per-AG root table (L0) and in 128-byte records inside
//! "5FSA" table blocks (L1/L2). Table blocks come from arena cells: FREE L0
//! cells claimed as RSVD.
//!
//! Allocation granularity: n <= 16 blocks -> exact n L2 blocks;
//! 17..=255 -> multiples of 16 (L1 cells); >= 256 -> whole L0 cells.
//! free_extent() rediscovers granularity from the on-disk states, and
//! coarsens: a record whose children all return to FREE is released and the
//! parent state collapses (the buddy merge).
//!
//! Inodes (v1): inode blocks are ordinary single-block allocations; free
//! slots are recognized by being zeroed, and `ag_ino_hint` points at the
//! block currently used for new inodes. (The spec's L3 slot refinement is
//! not implemented yet.)

use crate::fmt::*;
use crate::fs::Gofs;
use crate::journal::Txn;
use anyhow::{anyhow, bail, Result};

pub const REC_SIZE: usize = 128;
// table block header: magic(4) csum(4) gen(8) used(4); records start at 128

/// Records per table block.
pub fn recs_per_block(bs: u32) -> u16 {
    (bs as usize / REC_SIZE - 1) as u16
}

// --- pure record helpers ------------------------------------------------------
// Record: states 4B (16 x 2 bits), then 16 refs of 6B (block u32, idx u16).

pub fn rec_state(rec: &[u8], i: u32) -> u8 {
    cell_get(&rec[0..4], i as u64)
}
pub fn rec_state_set(rec: &mut [u8], i: u32, st: u8) {
    cell_set(&mut rec[0..4], i as u64, st);
}
pub fn rec_ref(rec: &[u8], i: u32) -> (u32, u16) {
    let off = 4 + i as usize * 6;
    (get_u32(rec, off), get_u16(rec, off + 4))
}
pub fn rec_ref_set(rec: &mut [u8], i: u32, r: (u32, u16)) {
    let off = 4 + i as usize * 6;
    put_u32(rec, off, r.0);
    put_u16(rec, off + 4, r.1);
}
pub fn rec_all(rec: &[u8], st: u8) -> bool {
    (0..ALLOC_FANOUT).all(|i| rec_state(rec, i) == st)
}

fn rec_off(idx: u16) -> usize {
    REC_SIZE * (1 + idx as usize)
}

impl Gofs {
    // --- root table access ------------------------------------------------------

    fn rt_byte(&self, t: &Txn, hdr: &AgHeader, byte_off: u64) -> Result<(u64, usize, Vec<u8>)> {
        let bs = self.sb.blocksize as u64;
        let blk = blk_addr(hdr.ag_num, hdr.alloc_root + (byte_off / bs) as u32);
        let buf = self.txn_read(t, blk)?;
        Ok((blk, (byte_off % bs) as usize, buf))
    }

    pub fn rt_state(&self, t: &Txn, hdr: &AgHeader, c: u64) -> Result<u8> {
        let (_, off, buf) = self.rt_byte(t, hdr, c / 4)?;
        Ok((buf[off] >> ((c % 4) * 2)) & 3)
    }

    pub fn rt_state_set(&self, t: &mut Txn, hdr: &AgHeader, c: u64, st: u8) -> Result<()> {
        let (blk, off, mut buf) = self.rt_byte(t, hdr, c / 4)?;
        let sh = (c % 4) * 2;
        buf[off] = (buf[off] & !(3 << sh)) | ((st & 3) << sh);
        self.txn_write(t, blk, buf);
        Ok(())
    }

    pub fn rt_ref(&self, t: &Txn, hdr: &AgHeader, c: u64) -> Result<(u32, u16)> {
        let (cells, _) = rt_geometry(hdr.length, self.sb.blocksize);
        let (_, off, buf) = self.rt_byte(t, hdr, rt_ref_off(cells, c))?;
        Ok((get_u32(&buf, off), get_u16(&buf, off + 4)))
    }

    pub fn rt_ref_set(&self, t: &mut Txn, hdr: &AgHeader, c: u64, r: (u32, u16)) -> Result<()> {
        let (cells, _) = rt_geometry(hdr.length, self.sb.blocksize);
        // a ref never straddles a block: 6-byte refs vs 4096 blocks can
        // misalign, so handle byte-wise
        let mut six = [0u8; 6];
        put_u32(&mut six, 0, r.0);
        put_u16(&mut six, 4, r.1);
        for (i, byte) in six.iter().enumerate() {
            let (blk, off, mut buf) = self.rt_byte(t, hdr, rt_ref_off(cells, c) + i as u64)?;
            buf[off] = *byte;
            self.txn_write(t, blk, buf);
        }
        Ok(())
    }

    // --- table blocks -------------------------------------------------------------

    fn tbl_read(&self, t: &Txn, ag: u32, local: u32) -> Result<Vec<u8>> {
        let buf = self.txn_read(t, blk_addr(ag, local))?;
        if buf[0..4] != ALLOC_MAGIC {
            bail!("AG {ag} block {local}: not a table block");
        }
        if get_u32(&buf, 4) != csum(&buf, 4) {
            bail!("AG {ag} block {local}: table block checksum mismatch");
        }
        Ok(buf)
    }

    fn tbl_write(&self, t: &mut Txn, ag: u32, local: u32, mut buf: Vec<u8>) {
        let gen = get_u64(&buf, 8);
        put_u64(&mut buf, 8, gen + 1);
        let c = csum(&buf, 4);
        put_u32(&mut buf, 4, c);
        self.txn_write(t, blk_addr(ag, local), buf);
    }

    pub fn rec_read(&self, t: &Txn, ag: u32, r: (u32, u16)) -> Result<[u8; REC_SIZE]> {
        let buf = self.tbl_read(t, ag, r.0)?;
        let off = rec_off(r.1);
        let mut rec = [0u8; REC_SIZE];
        rec.copy_from_slice(&buf[off..off + REC_SIZE]);
        Ok(rec)
    }

    pub fn rec_write(&self, t: &mut Txn, ag: u32, r: (u32, u16), rec: &[u8; REC_SIZE]) -> Result<()> {
        let mut buf = self.tbl_read(t, ag, r.0)?;
        let off = rec_off(r.1);
        buf[off..off + REC_SIZE].copy_from_slice(rec);
        self.tbl_write(t, ag, r.0, buf);
        Ok(())
    }

    /// Claim a free record slot, growing the arena if needed.
    fn tbl_alloc_rec(&mut self, t: &mut Txn, hdr: &mut AgHeader) -> Result<(u32, u16)> {
        let nrec = recs_per_block(self.sb.blocksize);
        if hdr.tbl_arena != 0 {
            for b in hdr.tbl_arena..hdr.tbl_arena + CELL_BLOCKS {
                let blk = blk_addr(hdr.ag_num, b);
                let raw = self.txn_read(t, blk)?;
                let mut buf = if raw[0..4] == ALLOC_MAGIC {
                    self.tbl_read(t, hdr.ag_num, b)?
                } else {
                    // fresh arena block: initialize
                    let mut nb = vec![0u8; self.sb.blocksize as usize];
                    nb[0..4].copy_from_slice(&ALLOC_MAGIC);
                    nb
                };
                let used = get_u32(&buf, 16);
                if used.count_ones() as u16 >= nrec {
                    continue;
                }
                let idx = (0..nrec).find(|i| used & (1 << i) == 0).unwrap();
                put_u32(&mut buf, 16, used | (1 << idx));
                let off = rec_off(idx);
                buf[off..off + REC_SIZE].fill(0);
                self.tbl_write(t, hdr.ag_num, b, buf);
                return Ok((b, idx));
            }
        }
        // claim a FREE L0 cell as a new arena
        let (cells, _) = rt_geometry(hdr.length, self.sb.blocksize);
        let c = (0..cells)
            .find(|&c| self.rt_state(t, hdr, c).map(|s| s == CELL_FREE).unwrap_or(false))
            .ok_or_else(|| anyhow!("AG {}: no free cell for table arena", hdr.ag_num))?;
        self.rt_state_set(t, hdr, c, CELL_RSVD)?;
        hdr.tbl_arena = (c * CELL_BLOCKS as u64) as u32;
        hdr.free_blocks -= CELL_BLOCKS;
        hdr.rsvd_blocks += CELL_BLOCKS;
        self.sb.free_blocks -= CELL_BLOCKS as u64;
        self.sb.rsvd_blocks += CELL_BLOCKS as u64;
        let mut buf = vec![0u8; self.sb.blocksize as usize];
        buf[0..4].copy_from_slice(&ALLOC_MAGIC);
        put_u32(&mut buf, 16, 1);
        self.tbl_write(t, hdr.ag_num, hdr.tbl_arena, buf);
        Ok((hdr.tbl_arena, 0))
    }

    fn tbl_free_rec(&self, t: &mut Txn, ag: u32, r: (u32, u16)) -> Result<()> {
        let mut buf = self.tbl_read(t, ag, r.0)?;
        let used = get_u32(&buf, 16) & !(1u32 << r.1);
        put_u32(&mut buf, 16, used);
        let off = rec_off(r.1);
        buf[off..off + REC_SIZE].fill(0);
        self.tbl_write(t, ag, r.0, buf);
        Ok(())
    }

    pub fn put_ag_header(&self, t: &mut Txn, hdr: &mut AgHeader) -> Result<()> {
        hdr.gen += 1;
        let mut buf = vec![0u8; self.sb.blocksize as usize];
        buf[..AGHDR_SIZE].copy_from_slice(&hdr.to_bytes());
        self.txn_write(t, blk_addr(hdr.ag_num, Self::ag_header_block(hdr.ag_num)), buf);
        Ok(())
    }

    // --- refinement ------------------------------------------------------------------

    /// Refine a FREE L0 cell: all 16 L1 children start FREE.
    fn refine_l0(&mut self, t: &mut Txn, hdr: &mut AgHeader, c: u64) -> Result<(u32, u16)> {
        // flip the state before allocating the record: tbl_alloc_rec scans
        // for FREE cells when growing the arena and must not pick this one
        self.rt_state_set(t, hdr, c, CELL_REFINED)?;
        let r = self.tbl_alloc_rec(t, hdr)?;
        self.rt_ref_set(t, hdr, c, r)?;
        Ok(r)
    }

    /// Refine a FREE L1 child of an L0 record.
    fn refine_l1(
        &mut self,
        t: &mut Txn,
        hdr: &mut AgHeader,
        l0: (u32, u16),
        i: u32,
    ) -> Result<(u32, u16)> {
        let r = self.tbl_alloc_rec(t, hdr)?;
        let mut rec = self.rec_read(t, hdr.ag_num, l0)?;
        rec_state_set(&mut rec, i, CELL_REFINED);
        rec_ref_set(&mut rec, i, r);
        self.rec_write(t, hdr.ag_num, l0, &rec)?;
        Ok(r)
    }

    // --- allocation --------------------------------------------------------------------

    /// Allocate `n` contiguous blocks in `ag`. Returns the block address.
    /// Granularity: see module docs.
    pub fn alloc_extent(&mut self, t: &mut Txn, ag: u32, n: u64) -> Result<u64> {
        let mut hdr = self.read_ag_header_txn(t, ag)?;
        let blk = if n >= CELL_BLOCKS as u64 {
            self.alloc_l0(t, &mut hdr, n.div_ceil(CELL_BLOCKS as u64))?
        } else if n > ALLOC_FANOUT as u64 {
            self.alloc_l1(t, &mut hdr, n.div_ceil(ALLOC_FANOUT as u64) as u32)?
        } else {
            self.alloc_l2(t, &mut hdr, n as u32)?
        };
        self.put_ag_header(t, &mut hdr)?;
        Ok(blk)
    }

    pub fn alloc_block(&mut self, t: &mut Txn, ag: u32) -> Result<u64> {
        self.alloc_extent(t, ag, 1)
    }

    fn account_alloc(&mut self, hdr: &mut AgHeader, blocks: u32) {
        hdr.free_blocks -= blocks;
        hdr.full_blocks += blocks;
        self.sb.free_blocks -= blocks as u64;
        self.sb.full_blocks += blocks as u64;
    }

    fn alloc_l0(&mut self, t: &mut Txn, hdr: &mut AgHeader, k: u64) -> Result<u64> {
        let (cells, _) = rt_geometry(hdr.length, self.sb.blocksize);
        let mut run = 0u64;
        let mut start = 0u64;
        for c in 0..cells {
            if self.rt_state(t, hdr, c)? == CELL_FREE {
                if run == 0 {
                    start = c;
                }
                run += 1;
                if run == k {
                    for cc in start..start + k {
                        self.rt_state_set(t, hdr, cc, CELL_FULL)?;
                    }
                    self.account_alloc(hdr, (k * CELL_BLOCKS as u64) as u32);
                    return Ok(blk_addr(hdr.ag_num, (start * CELL_BLOCKS as u64) as u32));
                }
            } else {
                run = 0;
            }
        }
        bail!("AG {}: no room for {k} cells", hdr.ag_num)
    }

    /// Allocate `m` consecutive L1 cells (16 blocks each) inside one L0 cell.
    fn alloc_l1(&mut self, t: &mut Txn, hdr: &mut AgHeader, m: u32) -> Result<u64> {
        let (cells, _) = rt_geometry(hdr.length, self.sb.blocksize);
        // prefer an already-refined L0 with room (clustered refinement)
        for c in 0..cells {
            if self.rt_state(t, hdr, c)? != CELL_REFINED {
                continue;
            }
            let r = self.rt_ref(t, hdr, c)?;
            let mut rec = self.rec_read(t, hdr.ag_num, r)?;
            if let Some(start) = find_run(&rec, m) {
                for i in start..start + m {
                    rec_state_set(&mut rec, i, CELL_FULL);
                }
                self.rec_write(t, hdr.ag_num, r, &rec)?;
                self.account_alloc(hdr, m * ALLOC_FANOUT);
                return Ok(blk_addr(
                    hdr.ag_num,
                    (c * CELL_BLOCKS as u64) as u32 + start * ALLOC_FANOUT,
                ));
            }
        }
        // refine a fresh FREE L0 cell
        let c = (0..cells)
            .find(|&c| self.rt_state(t, hdr, c).map(|s| s == CELL_FREE).unwrap_or(false))
            .ok_or_else(|| anyhow!("AG {}: out of space", hdr.ag_num))?;
        let r = self.refine_l0(t, hdr, c)?;
        let mut rec = self.rec_read(t, hdr.ag_num, r)?;
        for i in 0..m {
            rec_state_set(&mut rec, i, CELL_FULL);
        }
        self.rec_write(t, hdr.ag_num, r, &rec)?;
        self.account_alloc(hdr, m * ALLOC_FANOUT);
        Ok(blk_addr(hdr.ag_num, (c * CELL_BLOCKS as u64) as u32))
    }

    /// Allocate `n` (1..=16) consecutive single blocks inside one L1 cell.
    fn alloc_l2(&mut self, t: &mut Txn, hdr: &mut AgHeader, n: u32) -> Result<u64> {
        let (cells, _) = rt_geometry(hdr.length, self.sb.blocksize);
        // pass 1: an existing L2 record with a free run
        for c in 0..cells {
            if self.rt_state(t, hdr, c)? != CELL_REFINED {
                continue;
            }
            let l0 = self.rt_ref(t, hdr, c)?;
            let l0rec = self.rec_read(t, hdr.ag_num, l0)?;
            for i in 0..ALLOC_FANOUT {
                if rec_state(&l0rec, i) != CELL_REFINED {
                    continue;
                }
                let l1 = rec_ref(&l0rec, i);
                let mut l1rec = self.rec_read(t, hdr.ag_num, l1)?;
                if let Some(start) = find_run(&l1rec, n) {
                    for j in start..start + n {
                        rec_state_set(&mut l1rec, j, CELL_FULL);
                    }
                    self.rec_write(t, hdr.ag_num, l1, &l1rec)?;
                    self.account_alloc(hdr, n);
                    return Ok(blk_addr(
                        hdr.ag_num,
                        (c * CELL_BLOCKS as u64) as u32 + i * ALLOC_FANOUT + start,
                    ));
                }
            }
        }
        // pass 2: refine a FREE L1 inside an existing refined L0
        for c in 0..cells {
            if self.rt_state(t, hdr, c)? != CELL_REFINED {
                continue;
            }
            let l0 = self.rt_ref(t, hdr, c)?;
            let l0rec = self.rec_read(t, hdr.ag_num, l0)?;
            if let Some(i) = (0..ALLOC_FANOUT).find(|&i| rec_state(&l0rec, i) == CELL_FREE) {
                let l1 = self.refine_l1(t, hdr, l0, i)?;
                let mut l1rec = self.rec_read(t, hdr.ag_num, l1)?;
                for j in 0..n {
                    rec_state_set(&mut l1rec, j, CELL_FULL);
                }
                self.rec_write(t, hdr.ag_num, l1, &l1rec)?;
                self.account_alloc(hdr, n);
                return Ok(blk_addr(
                    hdr.ag_num,
                    (c * CELL_BLOCKS as u64) as u32 + i * ALLOC_FANOUT,
                ));
            }
        }
        // pass 3: refine a fresh FREE L0, then its first L1
        let c = (0..cells)
            .find(|&c| self.rt_state(t, hdr, c).map(|s| s == CELL_FREE).unwrap_or(false))
            .ok_or_else(|| anyhow!("AG {}: out of space", hdr.ag_num))?;
        let l0 = self.refine_l0(t, hdr, c)?;
        let l1 = self.refine_l1(t, hdr, l0, 0)?;
        let mut l1rec = self.rec_read(t, hdr.ag_num, l1)?;
        for j in 0..n {
            rec_state_set(&mut l1rec, j, CELL_FULL);
        }
        self.rec_write(t, hdr.ag_num, l1, &l1rec)?;
        self.account_alloc(hdr, n);
        Ok(blk_addr(hdr.ag_num, (c * CELL_BLOCKS as u64) as u32))
    }

    // --- free + coarsen --------------------------------------------------------------------

    /// Free `n` blocks starting at `blk`. Granularity is rediscovered from
    /// the on-disk states; empty records coarsen away.
    pub fn free_extent(&mut self, t: &mut Txn, blk: u64, n: u64) -> Result<()> {
        let (ag, local0) = blk_split(blk);
        let mut hdr = self.read_ag_header_txn(t, ag)?;
        let mut cur = 0u64;
        while cur < n {
            let local = local0 + cur as u32;
            let c = (local / CELL_BLOCKS) as u64;
            match self.rt_state(t, &hdr, c)? {
                CELL_FULL => {
                    if local % CELL_BLOCKS != 0 {
                        bail!("free_extent: misaligned free inside FULL cell");
                    }
                    self.rt_state_set(t, &mut hdr, c, CELL_FREE)?;
                    self.account_free(&mut hdr, CELL_BLOCKS);
                    cur += CELL_BLOCKS as u64;
                }
                CELL_REFINED => {
                    let l0 = self.rt_ref(t, &hdr, c)?;
                    let mut l0rec = self.rec_read(t, ag, l0)?;
                    let i = (local % CELL_BLOCKS) / ALLOC_FANOUT;
                    match rec_state(&l0rec, i) {
                        CELL_FULL => {
                            if local % ALLOC_FANOUT != 0 {
                                bail!("free_extent: misaligned free inside FULL L1 cell");
                            }
                            rec_state_set(&mut l0rec, i, CELL_FREE);
                            self.rec_write(t, ag, l0, &l0rec)?;
                            self.account_free(&mut hdr, ALLOC_FANOUT);
                            cur += ALLOC_FANOUT as u64;
                        }
                        CELL_REFINED => {
                            let l1 = rec_ref(&l0rec, i);
                            let mut l1rec = self.rec_read(t, ag, l1)?;
                            let j = local % ALLOC_FANOUT;
                            if rec_state(&l1rec, j) != CELL_FULL {
                                bail!("free_extent: block {local} in AG {ag} not allocated");
                            }
                            rec_state_set(&mut l1rec, j, CELL_FREE);
                            self.account_free(&mut hdr, 1);
                            cur += 1;
                            if rec_all(&l1rec, CELL_FREE) {
                                // coarsen: L1 cell back to FREE
                                self.tbl_free_rec(t, ag, l1)?;
                                rec_state_set(&mut l0rec, i, CELL_FREE);
                                rec_ref_set(&mut l0rec, i, (0, 0));
                                self.rec_write(t, ag, l0, &l0rec)?;
                            } else {
                                self.rec_write(t, ag, l1, &l1rec)?;
                            }
                        }
                        s => bail!("free_extent: L1 cell in state {s}"),
                    }
                    // coarsen: L0 cell back to FREE
                    let l0rec = self.rec_read(t, ag, l0)?;
                    if rec_all(&l0rec, CELL_FREE) {
                        self.tbl_free_rec(t, ag, l0)?;
                        self.rt_state_set(t, &mut hdr, c, CELL_FREE)?;
                        self.rt_ref_set(t, &mut hdr, c, (0, 0))?;
                    }
                }
                s => bail!("free_extent: AG {ag} cell {c} in state {s}"),
            }
        }
        self.put_ag_header(t, &mut hdr)?;
        Ok(())
    }

    fn account_free(&mut self, hdr: &mut AgHeader, blocks: u32) {
        hdr.free_blocks += blocks;
        hdr.full_blocks -= blocks;
        self.sb.free_blocks += blocks as u64;
        self.sb.full_blocks -= blocks as u64;
    }

    // --- inodes ----------------------------------------------------------------------------

    /// Allocate an inode slot, journaled. New inode blocks are single-block
    /// allocations; free slots are zeroed.
    pub fn alloc_inode(&mut self, t: &mut Txn, ag: u32) -> Result<u64> {
        let mut hdr = self.read_ag_header_txn(t, ag)?;
        let slots = self.sb.blocksize / self.sb.inodesize as u32;
        if hdr.ino_hint != 0 {
            let blk = blk_addr(ag, hdr.ino_hint);
            let buf = self.txn_read(t, blk)?;
            for s in 0..slots {
                let off = (s * self.sb.inodesize as u32) as usize;
                if buf[off..off + self.sb.inodesize as usize].iter().all(|&b| b == 0) {
                    return Ok(ino_addr(ag, hdr.ino_hint * slots + s));
                }
            }
        }
        let blk = self.alloc_extent(t, ag, 1)?;
        let (_, local) = blk_split(blk);
        self.txn_write(t, blk, vec![0u8; self.sb.blocksize as usize]);
        hdr = self.read_ag_header_txn(t, ag)?; // alloc_extent rewrote it
        hdr.ino_hint = local;
        self.put_ag_header(t, &mut hdr)?;
        Ok(ino_addr(ag, local * slots))
    }

    /// Release an inode slot (zero it). A block whose slots are all free is
    /// returned to the allocator (except the mkfs seed block, which always
    /// holds the root inode and so never empties).
    pub fn free_inode(&mut self, t: &mut Txn, ino: u64) -> Result<()> {
        let (ag, slot) = blk_split(ino);
        let isz = self.sb.inodesize as u32;
        let slots = self.sb.blocksize / isz;
        let local = slot / slots;
        let blk = blk_addr(ag, local);
        let mut buf = self.txn_read(t, blk)?;
        let off = ((slot % slots) * isz) as usize;
        buf[off..off + isz as usize].fill(0);
        let empty = buf.iter().all(|&b| b == 0);
        self.txn_write(t, blk, buf);
        let mut hdr = self.read_ag_header_txn(t, ag)?;
        if empty {
            self.free_extent(t, blk, 1)?;
            hdr = self.read_ag_header_txn(t, ag)?; // free_extent rewrote it
            if hdr.ino_hint == local {
                hdr.ino_hint = 0;
                self.put_ag_header(t, &mut hdr)?;
            }
        } else if hdr.ino_hint != local {
            hdr.ino_hint = local;
            self.put_ag_header(t, &mut hdr)?;
        }
        Ok(())
    }

    pub fn read_ag_header_txn(&self, t: &Txn, ag: u32) -> Result<AgHeader> {
        let buf = self.txn_read(t, blk_addr(ag, Self::ag_header_block(ag)))?;
        AgHeader::parse(&buf).map_err(|e| anyhow!("AG {ag}: {e}"))
    }
}

/// Find a run of `m` consecutive FREE children in a record.
fn find_run(rec: &[u8], m: u32) -> Option<u32> {
    let mut run = 0;
    for i in 0..ALLOC_FANOUT {
        if rec_state(rec, i) == CELL_FREE {
            run += 1;
            if run == m {
                return Some(i + 1 - m);
            }
        } else {
            run = 0;
        }
    }
    None
}
