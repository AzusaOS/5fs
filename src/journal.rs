//! Metadata journal: whole-block physical transactions (doc/6-journal.md).
//!
//! A transaction is journaled (descriptor + data blocks + commit), synced,
//! then applied in place and the superblock's head/seq advance. Replay on
//! writable open re-applies any committed-but-unapplied transactions;
//! replay is idempotent.

use crate::fmt::*;
use crate::fs::Gofs;
use anyhow::{bail, Result};
use std::collections::BTreeMap;

/// An in-flight transaction: new contents for metadata blocks, keyed by
/// block address. Data blocks are NOT journaled (ordered mode): write them
/// in place before committing the metadata that references them.
#[derive(Default)]
pub struct Txn {
    pub blocks: BTreeMap<u64, Vec<u8>>,
}

impl Txn {
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

const JHDR: usize = 24; // magic(4) csum(4) seq(8) count/txn_csum(4) pad(4)

fn desc_block(bs: usize, seq: u64, targets: &[u64]) -> Vec<u8> {
    let mut b = vec![0u8; bs];
    b[0..4].copy_from_slice(&JDESC_MAGIC);
    put_u64(&mut b, 8, seq);
    put_u32(&mut b, 16, targets.len() as u32);
    for (i, t) in targets.iter().enumerate() {
        put_u64(&mut b, JHDR + i * 8, *t);
    }
    let c = csum(&b, 4);
    put_u32(&mut b, 4, c);
    b
}

fn commit_block(bs: usize, seq: u64, txn_csum: u32) -> Vec<u8> {
    let mut b = vec![0u8; bs];
    b[0..4].copy_from_slice(&JCOMMIT_MAGIC);
    put_u64(&mut b, 8, seq);
    put_u32(&mut b, 16, txn_csum);
    let c = csum(&b, 4);
    put_u32(&mut b, 4, c);
    b
}

/// Maximum targets per transaction for a given block size.
pub fn max_targets(bs: usize) -> usize {
    (bs - JHDR) / 8
}

impl Gofs {
    pub fn txn(&self) -> Txn {
        Txn::default()
    }

    /// Read a block through the transaction (sees uncommitted writes).
    pub fn txn_read(&self, t: &Txn, blk: u64) -> Result<Vec<u8>> {
        if let Some(b) = t.blocks.get(&blk) {
            return Ok(b.clone());
        }
        self.read_block(blk)
    }

    pub fn txn_write(&self, t: &mut Txn, blk: u64, data: Vec<u8>) {
        debug_assert_eq!(data.len(), self.sb.blocksize as usize);
        t.blocks.insert(blk, data);
    }

    fn journal_phys(&self, idx: u64) -> Result<u64> {
        if idx >= self.sb.journal_length {
            bail!("journal index {idx} out of range");
        }
        self.resolve(self.sb.journal_start + idx)
    }

    /// Journal and apply a transaction.
    pub fn commit(&mut self, t: Txn) -> Result<()> {
        if t.is_empty() {
            return Ok(());
        }
        let bs = self.sb.blocksize as usize;
        if t.blocks.len() > max_targets(bs) {
            // Callers keep transactions small; this is a structural limit.
            bail!("transaction too large ({} blocks)", t.blocks.len());
        }
        let needed = t.blocks.len() as u64 + 2;
        if needed > self.sb.journal_length {
            bail!("transaction larger than journal");
        }
        let mut head = self.sb.journal_head;
        if head + needed > self.sb.journal_length {
            head = 0; // wrap: transactions never split across the end
        }
        let seq = self.sb.journal_seq;
        let targets: Vec<u64> = t.blocks.keys().copied().collect();
        let desc = desc_block(bs, seq, &targets);
        let mut tc = crc32c::crc32c(&desc);
        for data in t.blocks.values() {
            tc = crc32c::crc32c_append(tc, data);
        }
        self.dev.pwrite(&desc, self.journal_phys(head)?)?;
        for (i, data) in t.blocks.values().enumerate() {
            self.dev.pwrite(data, self.journal_phys(head + 1 + i as u64)?)?;
        }
        self.dev.pwrite(&commit_block(bs, seq, tc), self.journal_phys(head + needed - 1)?)?;
        self.dev.sync()?;

        // apply in place
        for (blk, data) in &t.blocks {
            self.write_block(*blk, data)?;
        }
        self.dev.sync()?;

        // checkpoint
        self.sb.journal_head = head + needed;
        if self.sb.journal_head >= self.sb.journal_length {
            self.sb.journal_head = 0;
        }
        self.sb.journal_seq = seq + 1;
        self.write_superblock()?;
        Ok(())
    }

    /// Replay committed-but-unapplied transactions. Called on writable open.
    /// Returns the number of transactions applied.
    pub fn replay(&mut self) -> Result<u32> {
        let bs = self.sb.blocksize as usize;
        let mut head = self.sb.journal_head;
        let mut seq = self.sb.journal_seq;
        let mut applied = 0u32;
        loop {
            let txn = match self.try_read_txn(head, seq, bs)? {
                Some(t) => Some((head, t)),
                // the writer may have wrapped without recording it
                None if head != 0 => self.try_read_txn(0, seq, bs)?.map(|t| (0, t)),
                None => None,
            };
            let Some((pos, (targets, blocks))) = txn else { break };
            for (blk, data) in targets.iter().zip(blocks.iter()) {
                self.write_block(*blk, data)?;
            }
            head = pos + targets.len() as u64 + 2;
            if head >= self.sb.journal_length {
                head = 0;
            }
            seq += 1;
            applied += 1;
        }
        if applied > 0 {
            self.dev.sync()?;
            self.sb.journal_head = head;
            self.sb.journal_seq = seq;
            self.write_superblock()?;
        }
        Ok(applied)
    }

    /// Count committed-but-unapplied transactions without applying them
    /// (read-only fsck).
    pub fn journal_pending(&self) -> Result<u32> {
        let bs = self.sb.blocksize as usize;
        let mut head = self.sb.journal_head;
        let mut seq = self.sb.journal_seq;
        let mut n = 0u32;
        loop {
            let t = match self.try_read_txn(head, seq, bs)? {
                Some(t) => Some((head, t)),
                None if head != 0 => self.try_read_txn(0, seq, bs)?.map(|t| (0, t)),
                None => None,
            };
            let Some((pos, (targets, _))) = t else { break };
            head = pos + targets.len() as u64 + 2;
            if head >= self.sb.journal_length {
                head = 0;
            }
            seq += 1;
            n += 1;
        }
        Ok(n)
    }

    /// Validate a transaction at journal index `pos` with expected sequence.
    #[allow(clippy::type_complexity)]
    fn try_read_txn(
        &self,
        pos: u64,
        seq: u64,
        bs: usize,
    ) -> Result<Option<(Vec<u64>, Vec<Vec<u8>>)>> {
        if pos + 2 > self.sb.journal_length {
            return Ok(None);
        }
        let mut desc = vec![0u8; bs];
        self.dev.pread(&mut desc, self.journal_phys(pos)?)?;
        if desc[0..4] != JDESC_MAGIC
            || get_u32(&desc, 4) != csum(&desc, 4)
            || get_u64(&desc, 8) != seq
        {
            return Ok(None);
        }
        let count = get_u32(&desc, 16) as u64;
        if count == 0 || count as usize > max_targets(bs) || pos + count + 2 > self.sb.journal_length
        {
            return Ok(None);
        }
        let mut commit = vec![0u8; bs];
        self.dev.pread(&mut commit, self.journal_phys(pos + count + 1)?)?;
        if commit[0..4] != JCOMMIT_MAGIC
            || get_u32(&commit, 4) != csum(&commit, 4)
            || get_u64(&commit, 8) != seq
        {
            return Ok(None);
        }
        let mut targets = Vec::with_capacity(count as usize);
        let mut blocks = Vec::with_capacity(count as usize);
        let mut tc = crc32c::crc32c(&desc);
        for i in 0..count {
            targets.push(get_u64(&desc, JHDR + i as usize * 8));
            let mut data = vec![0u8; bs];
            self.dev.pread(&mut data, self.journal_phys(pos + 1 + i)?)?;
            tc = crc32c::crc32c_append(tc, &data);
            blocks.push(data);
        }
        if tc != get_u32(&commit, 16) {
            return Ok(None); // torn transaction
        }
        Ok(Some((targets, blocks)))
    }
}
