//! File data mapping: inline extents and extent refinement trees
//! (doc/4-extents.md).
//!
//! TREE root lives in the inode payload: 6 children ("reduced-width node"),
//! each covering 64^level blocks. Node blocks ("5FST") have 64 children.
//! A child is FREE (hole), FULL (a contiguous run of `blocks` blocks at
//! `addr`, the rest of the child range being a hole), or REFINED (a child
//! node one level down).
//!
//! Truncation frees whole children beyond the cutoff and trims straddling
//! runs at allocation-granule boundaries; emptied nodes coarsen away.
//! Requires blocksize >= 2048 for node blocks.

use crate::fmt::*;
use crate::fs::Gofs;
use crate::journal::Txn;
use anyhow::{anyhow, bail, Result};

const NODE_HDR: usize = 24; // magic(4) csum(4) gen(8) level(1) pad(7)
const NODE_STATES: usize = NODE_HDR; // 64 x 2 bits = 16 bytes
const NODE_CHILDREN: usize = NODE_STATES + 16;
const CHILD_REC: usize = 16; // blocks u32, pad u32, addr u64

/// Blocks covered by one child at `level` (child of a node whose children
/// are level `level`): 64^level.
pub fn cov(level: u8) -> u64 {
    NODE_FANOUT.pow(level as u32)
}

/// A mapping result starting at some file block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Map {
    /// Hole for at least `0` blocks (length given).
    Hole(u64),
    /// Contiguous run.
    Run { blk: u64, len: u64 },
}

// --- TREE root (inode payload) ----------------------------------------------------
// [0] level, [1] pad, [2..4] states (6 x 2 bits), child i at 8 + 16*i.

pub fn root_level(p: &[u8]) -> u8 {
    p[0]
}
pub fn root_state(p: &[u8], i: usize) -> u8 {
    cell_get(&p[2..4], i as u64)
}
pub fn root_child(p: &[u8], i: usize) -> (u32, u64) {
    (get_u32(p, 8 + 16 * i), get_u64(p, 8 + 16 * i + 8))
}
fn root_state_set(p: &mut [u8], i: usize, st: u8) {
    cell_set(&mut p[2..4], i as u64, st);
}
fn root_child_set(p: &mut [u8], i: usize, blocks: u32, addr: u64) {
    put_u32(p, 8 + 16 * i, blocks);
    put_u64(p, 8 + 16 * i + 8, addr);
}

fn root_init(p: &mut [u8; INODE_PAYLOAD], level: u8) {
    p.fill(0);
    p[0] = level;
}

// --- node blocks --------------------------------------------------------------------

pub fn node_level(b: &[u8]) -> u8 {
    b[16]
}
pub fn node_state(b: &[u8], i: usize) -> u8 {
    cell_get(&b[NODE_STATES..NODE_CHILDREN], i as u64)
}
pub fn node_child(b: &[u8], i: usize) -> (u32, u64) {
    let off = NODE_CHILDREN + CHILD_REC * i;
    (get_u32(b, off), get_u64(b, off + 8))
}
fn node_state_set(b: &mut [u8], i: usize, st: u8) {
    cell_set(&mut b[NODE_STATES..NODE_CHILDREN], i as u64, st);
}
fn node_child_set(b: &mut [u8], i: usize, blocks: u32, addr: u64) {
    let off = NODE_CHILDREN + CHILD_REC * i;
    put_u32(b, off, blocks);
    put_u64(b, off + 8, addr);
}

impl Gofs {
    pub fn node_read(&self, t: &Txn, addr: u64) -> Result<Vec<u8>> {
        let buf = self.txn_read(t, addr)?;
        if buf[0..4] != TREE_MAGIC {
            bail!("block {addr:#x}: not an extent tree node");
        }
        if get_u32(&buf, 4) != csum(&buf, 4) {
            bail!("block {addr:#x}: tree node checksum mismatch");
        }
        Ok(buf)
    }

    fn node_write(&self, t: &mut Txn, addr: u64, mut buf: Vec<u8>) {
        let gen = get_u64(&buf, 8);
        put_u64(&mut buf, 8, gen + 1);
        let c = csum(&buf, 4);
        put_u32(&mut buf, 4, c);
        self.txn_write(t, addr, buf);
    }

    fn node_alloc(&mut self, t: &mut Txn, ag: u32, level: u8) -> Result<u64> {
        let addr = self.alloc_block(t, ag)?;
        let mut buf = vec![0u8; self.sb.blocksize as usize];
        buf[0..4].copy_from_slice(&TREE_MAGIC);
        buf[16] = level;
        self.node_write(t, addr, buf);
        Ok(addr)
    }

    // --- lookup ------------------------------------------------------------------------

    /// Resolve `fblock` in a file's mapping. Run/hole lengths never cross a
    /// child boundary (callers loop).
    pub fn map_lookup(&self, t: &Txn, inode: &Inode, fblock: u64) -> Result<Map> {
        match inode.format {
            FMT_EMPTY | FMT_EMBED => Ok(Map::Hole(u64::MAX)),
            FMT_EXTENT => {
                let mut next_start = u64::MAX;
                for e in extents_parse(&inode.payload) {
                    if fblock >= e.file_block && fblock < e.file_block + e.blocks as u64 {
                        let off = fblock - e.file_block;
                        return Ok(Map::Run { blk: e.blk + off, len: e.blocks as u64 - off });
                    }
                    if e.file_block > fblock {
                        next_start = next_start.min(e.file_block);
                    }
                }
                Ok(Map::Hole(next_start - fblock))
            }
            FMT_TREE => {
                let level = root_level(&inode.payload);
                let c = cov(level);
                let i = (fblock / c) as usize;
                if i >= ROOT_FANOUT {
                    return Ok(Map::Hole(u64::MAX));
                }
                let rel = fblock % c;
                match root_state(&inode.payload, i) {
                    CELL_FREE => Ok(Map::Hole(c - rel)),
                    CELL_FULL => {
                        let (blocks, addr) = root_child(&inode.payload, i);
                        run_in_child(blocks as u64, addr, rel, c)
                    }
                    CELL_REFINED => {
                        let (_, addr) = root_child(&inode.payload, i);
                        self.node_lookup(t, addr, rel)
                    }
                    s => bail!("bad root child state {s}"),
                }
            }
            f => bail!("format {f}: no block mapping"),
        }
    }

    fn node_lookup(&self, t: &Txn, addr: u64, rel: u64) -> Result<Map> {
        let buf = self.node_read(t, addr)?;
        let level = node_level(&buf);
        let c = cov(level);
        let i = (rel / c) as usize;
        let sub = rel % c;
        match node_state(&buf, i) {
            CELL_FREE => Ok(Map::Hole(c - sub)),
            CELL_FULL => {
                let (blocks, a) = node_child(&buf, i);
                run_in_child(blocks as u64, a, sub, c)
            }
            CELL_REFINED => {
                let (_, a) = node_child(&buf, i);
                self.node_lookup(t, a, sub)
            }
            s => bail!("bad node child state {s}"),
        }
    }

    // --- ensure (allocate for writes) -----------------------------------------------------

    /// Make `[fblock, fblock+n)` fully mapped, allocating where needed.
    /// Returns the runs covering the range as (file_block, blk, len, fresh);
    /// `fresh` runs were newly allocated this call.
    pub fn map_ensure(
        &mut self,
        t: &mut Txn,
        ag: u32,
        inode: &mut Inode,
        fblock: u64,
        n: u64,
    ) -> Result<Vec<(u64, u64, u64, bool)>> {
        if inode.format == FMT_EXTENT {
            self.extent_ensure(t, ag, inode, fblock, n)?;
        }
        if inode.format == FMT_EXTENT {
            // still inline after ensure: collect runs from the records
            return self.collect_runs(t, inode, fblock, n, &[]);
        }
        if inode.format != FMT_TREE {
            // EMPTY (or EMBED already lifted by the caller): start inline
            inode.format = FMT_EXTENT;
            extents_store(&mut inode.payload, &[]).map_err(|e| anyhow!(e))?;
            return self.map_ensure(t, ag, inode, fblock, n);
        }
        // TREE: grow the root until the range fits
        while fblock + n > ROOT_FANOUT as u64 * cov(root_level(&inode.payload)) {
            self.root_grow(t, ag, inode)?;
        }
        let mut fresh = Vec::new();
        let level = root_level(&inode.payload);
        let c = cov(level);
        let mut pos = fblock;
        let end = fblock + n;
        while pos < end {
            let i = (pos / c) as usize;
            let child_start = i as u64 * c;
            let want_end = end.min(child_start + c);
            let mut payload = inode.payload;
            self.child_ensure(
                t,
                ag,
                RootOrNode::Root(&mut payload),
                i,
                level,
                child_start,
                pos,
                want_end,
                &mut fresh,
            )?;
            inode.payload = payload;
            pos = want_end;
        }
        self.collect_runs(t, inode, fblock, n, &fresh)
    }

    /// Ensure within one child of a root or node.
    #[allow(clippy::too_many_arguments)]
    fn child_ensure(
        &mut self,
        t: &mut Txn,
        ag: u32,
        mut parent: RootOrNode,
        i: usize,
        level: u8,
        child_start: u64,
        from: u64,
        to: u64,
        fresh: &mut Vec<(u64, u64)>,
    ) -> Result<()> {
        let (state, (blocks, addr)) = parent.get(self, t, i)?;
        match state {
            CELL_FULL if to - child_start <= blocks as u64 => Ok(()), // already mapped
            CELL_FREE if from == child_start => {
                // allocate a run from the start of the child range
                let want = to - child_start;
                match self.alloc_extent(t, ag, want) {
                    Ok(blk) => {
                        parent.set(self, t, i, CELL_FULL, want as u32, blk)?;
                        fresh.push((child_start, want));
                        Ok(())
                    }
                    Err(_) if level > 0 => {
                        // fragmented: refine and let smaller children try
                        let node = self.node_alloc(t, ag, level - 1)?;
                        parent.set(self, t, i, CELL_REFINED, 0, node)?;
                        self.descend_ensure(t, ag, node, child_start, from, to, fresh)
                    }
                    Err(e) => Err(e),
                }
            }
            CELL_FREE | CELL_FULL => {
                if level == 0 {
                    bail!("level-0 child cannot be refined");
                }
                // refine: redistribute any existing run, then ensure below
                let node = self.node_alloc(t, ag, level - 1)?;
                if state == CELL_FULL {
                    self.redistribute(t, node, blocks as u64, addr)?;
                }
                parent.set(self, t, i, CELL_REFINED, 0, node)?;
                self.descend_ensure(t, ag, node, child_start, from, to, fresh)
            }
            CELL_REFINED => self.descend_ensure(t, ag, addr, child_start, from, to, fresh),
            s => bail!("bad child state {s}"),
        }
    }

    fn descend_ensure(
        &mut self,
        t: &mut Txn,
        ag: u32,
        node: u64,
        node_start: u64,
        from: u64,
        to: u64,
        fresh: &mut Vec<(u64, u64)>,
    ) -> Result<()> {
        let buf = self.node_read(t, node)?;
        let level = node_level(&buf);
        let c = cov(level);
        let mut pos = from;
        while pos < to {
            let rel = pos - node_start;
            let i = (rel / c) as usize;
            let child_start = node_start + i as u64 * c;
            let want_end = to.min(child_start + c);
            self.child_ensure(
                t,
                ag,
                RootOrNode::Node(node),
                i,
                level,
                child_start,
                pos,
                want_end,
                fresh,
            )?;
            pos = want_end;
        }
        Ok(())
    }

    /// Spread an existing run of `blocks` at `addr` over a fresh node's
    /// children (it covered the parent child range from its start).
    fn redistribute(&mut self, t: &mut Txn, node: u64, blocks: u64, addr: u64) -> Result<()> {
        let mut buf = self.node_read(t, node)?;
        let c = cov(node_level(&buf));
        let mut done = 0u64;
        let mut i = 0;
        while done < blocks {
            let take = c.min(blocks - done);
            node_state_set(&mut buf, i, CELL_FULL);
            node_child_set(&mut buf, i, take as u32, addr + done);
            done += take;
            i += 1;
        }
        self.node_write(t, node, buf);
        Ok(())
    }

    /// Wrap the current root children into a node one level down and raise
    /// the root level.
    fn root_grow(&mut self, t: &mut Txn, ag: u32, inode: &mut Inode) -> Result<()> {
        let level = root_level(&inode.payload);
        let node = self.node_alloc(t, ag, level)?;
        let mut buf = self.node_read(t, node)?;
        let mut any = false;
        for i in 0..ROOT_FANOUT {
            let st = root_state(&inode.payload, i);
            if st != CELL_FREE {
                let (blocks, addr) = root_child(&inode.payload, i);
                node_state_set(&mut buf, i, st);
                node_child_set(&mut buf, i, blocks, addr);
                any = true;
            }
        }
        self.node_write(t, node, buf);
        let mut p = inode.payload;
        root_init(&mut p, level + 1);
        if any {
            root_state_set(&mut p, 0, CELL_REFINED);
            root_child_set(&mut p, 0, 0, node);
        } else {
            // nothing mapped yet: drop the node again
            self.free_extent(t, node, 1)?;
            root_init(&mut p, level + 1);
        }
        inode.payload = p;
        Ok(())
    }

    // --- inline extents ---------------------------------------------------------------------

    /// Ensure on the inline-extent format; converts to TREE on overflow.
    fn extent_ensure(
        &mut self,
        t: &mut Txn,
        ag: u32,
        inode: &mut Inode,
        fblock: u64,
        n: u64,
    ) -> Result<()> {
        let mut extents = extents_parse(&inode.payload);
        let mut pos = fblock;
        let end = fblock + n;
        while pos < end {
            // find covering or next extent
            let covering = extents
                .iter()
                .find(|e| pos >= e.file_block && pos < e.file_block + e.blocks as u64);
            if let Some(e) = covering {
                pos = e.file_block + e.blocks as u64;
                continue;
            }
            let next = extents
                .iter()
                .filter(|e| e.file_block > pos)
                .map(|e| e.file_block)
                .min()
                .unwrap_or(end);
            let want = next.min(end) - pos;
            // try to extend the extent ending exactly at pos, else add one
            match self.alloc_extent(t, ag, want) {
                Ok(blk) => {
                    let merged = extents.iter_mut().any(|e| {
                        if e.file_block + e.blocks as u64 == pos && e.blk + e.blocks as u64 == blk
                        {
                            e.blocks += want as u32;
                            true
                        } else {
                            false
                        }
                    });
                    if !merged {
                        extents.push(Extent { file_block: pos, blk, blocks: want as u32 });
                    }
                }
                Err(e) => {
                    if extents.is_empty() {
                        return Err(e);
                    }
                    // can't get it contiguously: go to tree and retry there
                    self.to_tree(t, ag, inode, &extents)?;
                    return Ok(());
                }
            }
            pos = next.min(end);
        }
        if extents.len() > MAX_INLINE_EXTENTS {
            self.to_tree(t, ag, inode, &extents)?;
        } else {
            extents.sort_by_key(|e| e.file_block);
            extents_store(&mut inode.payload, &extents).map_err(|e| anyhow!(e))?;
        }
        Ok(())
    }

    /// Convert inline extents to a TREE mapping.
    pub fn to_tree(&mut self, t: &mut Txn, ag: u32, inode: &mut Inode, extents: &[Extent]) -> Result<()> {
        let needed = extents.iter().map(|e| e.file_block + e.blocks as u64).max().unwrap_or(1);
        let mut level = 0u8;
        while ROOT_FANOUT as u64 * cov(level) < needed {
            level += 1;
        }
        let mut p = [0u8; INODE_PAYLOAD];
        root_init(&mut p, level);
        inode.format = FMT_TREE;
        inode.payload = p;
        for e in extents {
            self.insert_run(t, ag, inode, e.file_block, e.blk, e.blocks as u64)?;
        }
        Ok(())
    }

    /// Record an already-allocated run in a TREE mapping, splitting at child
    /// boundaries and refining where the run does not start a child range.
    fn insert_run(
        &mut self,
        t: &mut Txn,
        ag: u32,
        inode: &mut Inode,
        fblock: u64,
        blk: u64,
        len: u64,
    ) -> Result<()> {
        let level = root_level(&inode.payload);
        let c = cov(level);
        let mut pos = 0u64;
        while pos < len {
            let f = fblock + pos;
            let i = (f / c) as usize;
            let child_start = i as u64 * c;
            let chunk = len.min(child_start + c - f + pos) - pos;
            let mut payload = inode.payload;
            self.insert_in_child(
                t,
                ag,
                RootOrNode::Root(&mut payload),
                i,
                level,
                child_start,
                f,
                blk + pos,
                chunk,
            )?;
            inode.payload = payload;
            pos += chunk;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_in_child(
        &mut self,
        t: &mut Txn,
        ag: u32,
        mut parent: RootOrNode,
        i: usize,
        level: u8,
        child_start: u64,
        f: u64,
        blk: u64,
        len: u64,
    ) -> Result<()> {
        let (state, (blocks, addr)) = parent.get(self, t, i)?;
        match state {
            CELL_FREE if f == child_start => {
                parent.set(self, t, i, CELL_FULL, len as u32, blk)?;
                Ok(())
            }
            CELL_FULL if f == child_start + blocks as u64 && blk == addr + blocks as u64 => {
                // physically contiguous append
                parent.set(self, t, i, CELL_FULL, blocks + len as u32, addr)?;
                Ok(())
            }
            CELL_FREE | CELL_FULL => {
                if level == 0 {
                    bail!("insert_run: level-0 collision");
                }
                let node = self.node_alloc(t, ag, level - 1)?;
                if state == CELL_FULL {
                    self.redistribute(t, node, blocks as u64, addr)?;
                }
                parent.set(self, t, i, CELL_REFINED, 0, node)?;
                self.insert_below(t, ag, node, child_start, f, blk, len)
            }
            CELL_REFINED => self.insert_below(t, ag, addr, child_start, f, blk, len),
            s => bail!("bad child state {s}"),
        }
    }

    fn insert_below(
        &mut self,
        t: &mut Txn,
        ag: u32,
        node: u64,
        node_start: u64,
        f: u64,
        blk: u64,
        len: u64,
    ) -> Result<()> {
        let buf = self.node_read(t, node)?;
        let level = node_level(&buf);
        let c = cov(level);
        let mut pos = 0u64;
        while pos < len {
            let ff = f + pos;
            let rel = ff - node_start;
            let i = (rel / c) as usize;
            let child_start = node_start + i as u64 * c;
            let chunk = len.min(child_start + c - ff + pos) - pos;
            self.insert_in_child(
                t,
                ag,
                RootOrNode::Node(node),
                i,
                level,
                child_start,
                ff,
                blk + pos,
                chunk,
            )?;
            pos += chunk;
        }
        Ok(())
    }

    // --- collect runs over a range (post-ensure) -----------------------------------------------

    fn collect_runs(
        &self,
        t: &Txn,
        inode: &Inode,
        fblock: u64,
        n: u64,
        fresh: &[(u64, u64)],
    ) -> Result<Vec<(u64, u64, u64, bool)>> {
        let mut out = Vec::new();
        let mut pos = fblock;
        let end = fblock + n;
        while pos < end {
            match self.map_lookup(t, inode, pos)? {
                Map::Run { blk, len } => {
                    let take = len.min(end - pos);
                    let is_fresh =
                        fresh.iter().any(|&(fs, fl)| pos >= fs && pos < fs + fl);
                    out.push((pos, blk, take, is_fresh));
                    pos += take;
                }
                Map::Hole(_) => bail!("collect_runs: hole at {pos} after ensure"),
            }
        }
        Ok(out)
    }

    // --- free ------------------------------------------------------------------------------------

    /// Release every block a mapping owns (delete/truncate-to-zero). Resets
    /// the inode to FMT_EMPTY.
    pub fn map_free_all(&mut self, t: &mut Txn, inode: &mut Inode) -> Result<()> {
        match inode.format {
            FMT_EMPTY | FMT_EMBED => {}
            FMT_EXTENT => {
                for e in extents_parse(&inode.payload) {
                    self.free_extent(t, e.blk, e.blocks as u64)?;
                }
            }
            FMT_TREE => {
                for i in 0..ROOT_FANOUT {
                    let st = root_state(&inode.payload, i);
                    let (blocks, addr) = root_child(&inode.payload, i);
                    self.free_child(t, st, blocks as u64, addr)?;
                }
            }
            f => bail!("format {f}: cannot free"),
        }
        inode.format = FMT_EMPTY;
        inode.payload.fill(0);
        inode.nblocks = 0;
        inode.size = 0;
        Ok(())
    }

    /// Returns the number of data blocks freed (node blocks not counted —
    /// they were never in `in_nblocks`).
    fn free_child(&mut self, t: &mut Txn, state: u8, blocks: u64, addr: u64) -> Result<u64> {
        match state {
            CELL_FREE => Ok(0),
            CELL_FULL => {
                self.free_extent(t, addr, blocks)?;
                Ok(blocks)
            }
            CELL_REFINED => {
                let buf = self.node_read(t, addr)?;
                let mut freed = 0;
                for i in 0..NODE_FANOUT as usize {
                    let st = node_state(&buf, i);
                    let (b, a) = node_child(&buf, i);
                    freed += self.free_child(t, st, b as u64, a)?;
                }
                self.free_extent(t, addr, 1)?; // the node block itself
                Ok(freed)
            }
            s => bail!("bad child state {s}"),
        }
    }

    // --- truncate (partial reclamation) ----------------------------------------------------

    /// Free the mapping beyond `cutoff` file blocks: whole children beyond
    /// the cutoff, and the granule-aligned suffix of a straddling run. Kept
    /// slack inside the last granule is zeroed by the caller. Returns data
    /// blocks freed.
    pub fn map_truncate(&mut self, t: &mut Txn, inode: &mut Inode, cutoff: u64) -> Result<u64> {
        let mut freed = 0u64;
        match inode.format {
            FMT_EMPTY | FMT_EMBED => {}
            FMT_EXTENT => {
                let mut keep = Vec::new();
                for e in extents_parse(&inode.payload) {
                    if e.file_block >= cutoff {
                        self.free_extent(t, e.blk, e.blocks as u64)?;
                        freed += e.blocks as u64;
                    } else if e.file_block + e.blocks as u64 > cutoff {
                        let kept = self.trim_run(t, e.blk, e.blocks as u64, cutoff - e.file_block, &mut freed)?;
                        keep.push(Extent { file_block: e.file_block, blk: e.blk, blocks: kept as u32 });
                    } else {
                        keep.push(e);
                    }
                }
                extents_store(&mut inode.payload, &keep).map_err(|e| anyhow!(e))?;
            }
            FMT_TREE => {
                let level = root_level(&inode.payload);
                let c = cov(level);
                let mut p = inode.payload;
                for i in 0..ROOT_FANOUT {
                    let st = root_state(&p, i);
                    let (blocks, addr) = root_child(&p, i);
                    if let Some((nst, nb, na)) =
                        self.trim_child(t, st, blocks as u64, addr, level, i as u64 * c, cutoff, &mut freed)?
                    {
                        root_state_set(&mut p, i, nst);
                        root_child_set(&mut p, i, nb as u32, na);
                    }
                }
                inode.payload = p;
            }
            f => bail!("format {f}: cannot truncate"),
        }
        Ok(freed)
    }

    /// Trim a FULL run to at least `keep` blocks, freeing the suffix at the
    /// allocation-granule boundary. Returns the kept length.
    fn trim_run(&mut self, t: &mut Txn, addr: u64, blocks: u64, keep: u64, freed: &mut u64) -> Result<u64> {
        if keep >= blocks {
            return Ok(blocks);
        }
        let g = self.granule_at(t, addr)?;
        let keep_r = (keep.div_ceil(g) * g).min(blocks);
        if keep_r < blocks {
            self.free_extent(t, addr + keep_r, blocks - keep_r)?;
            *freed += blocks - keep_r;
        }
        Ok(keep_r)
    }

    /// Returns Some(new child rec) if the child changed.
    #[allow(clippy::too_many_arguments)]
    fn trim_child(
        &mut self,
        t: &mut Txn,
        st: u8,
        blocks: u64,
        addr: u64,
        level: u8,
        child_start: u64,
        cutoff: u64,
        freed: &mut u64,
    ) -> Result<Option<(u8, u64, u64)>> {
        let c = cov(level);
        if child_start >= cutoff {
            if st == CELL_FREE {
                return Ok(None);
            }
            *freed += self.free_child(t, st, blocks, addr)?;
            return Ok(Some((CELL_FREE, 0, 0)));
        }
        if child_start + c <= cutoff {
            return Ok(None); // fully kept
        }
        match st {
            CELL_FREE => Ok(None),
            CELL_FULL => {
                let kept = self.trim_run(t, addr, blocks, cutoff - child_start, freed)?;
                if kept != blocks {
                    Ok(Some((CELL_FULL, kept, addr)))
                } else {
                    Ok(None)
                }
            }
            CELL_REFINED => {
                let mut buf = self.node_read(t, addr)?;
                let nl = node_level(&buf);
                let nc = cov(nl);
                let mut changed = false;
                for i in 0..NODE_FANOUT as usize {
                    let cst = node_state(&buf, i);
                    let (b, a) = node_child(&buf, i);
                    if let Some((nst, nb, na)) =
                        self.trim_child(t, cst, b as u64, a, nl, child_start + i as u64 * nc, cutoff, freed)?
                    {
                        node_state_set(&mut buf, i, nst);
                        node_child_set(&mut buf, i, nb as u32, na);
                        changed = true;
                    }
                }
                if (0..NODE_FANOUT as usize).all(|i| node_state(&buf, i) == CELL_FREE) {
                    // coarsen: the node emptied
                    self.free_extent(t, addr, 1)?;
                    return Ok(Some((CELL_FREE, 0, 0)));
                }
                if changed {
                    self.node_write(t, addr, buf);
                }
                Ok(None)
            }
            s => bail!("bad child state {s}"),
        }
    }
}

fn run_in_child(blocks: u64, addr: u64, rel: u64, c: u64) -> Result<Map> {
    if rel < blocks {
        Ok(Map::Run { blk: addr + rel, len: blocks - rel })
    } else {
        Ok(Map::Hole(c - rel))
    }
}

/// Uniform accessor over the inode-payload root and node blocks, so the
/// ensure/insert logic is written once.
enum RootOrNode<'a> {
    Root(&'a mut [u8; INODE_PAYLOAD]),
    Node(u64),
}

impl RootOrNode<'_> {
    fn get(&self, g: &Gofs, t: &Txn, i: usize) -> Result<(u8, (u32, u64))> {
        match self {
            RootOrNode::Root(p) => Ok((root_state(*p, i), root_child(*p, i))),
            RootOrNode::Node(addr) => {
                let buf = g.node_read(t, *addr)?;
                Ok((node_state(&buf, i), node_child(&buf, i)))
            }
        }
    }

    fn set(
        &mut self,
        g: &Gofs,
        t: &mut Txn,
        i: usize,
        st: u8,
        blocks: u32,
        addr: u64,
    ) -> Result<()> {
        match self {
            RootOrNode::Root(p) => {
                root_state_set(*p, i, st);
                root_child_set(*p, i, blocks, addr);
                Ok(())
            }
            RootOrNode::Node(n) => {
                let mut buf = g.node_read(t, *n)?;
                node_state_set(&mut buf, i, st);
                node_child_set(&mut buf, i, blocks, addr);
                g.node_write(t, *n, buf);
                Ok(())
            }
        }
    }
}
