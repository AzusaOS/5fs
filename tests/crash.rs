//! Crash-recovery testing. A workload runs with the device write recorder
//! on; every pwrite is captured with its sync-epoch. A simulated crash
//! state is: all writes from finished epochs, plus an arbitrary subset of
//! the crash epoch (nothing orders writes between two syncs), optionally
//! with one in-flight write torn (replaced by garbage). For every sampled
//! crash state, a writable open (journal replay + superblock self-heal)
//! must succeed, fsck must report no errors, and the completed operations
//! must be an exact prefix of the workload.

use gofs::fs::Gofs;
use gofs::mkfs::{mkfs, MkfsOpts};
use std::path::Path;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    fn coin(&mut self) -> bool {
        self.next() & 1 == 1
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const OPS: usize = 25;

/// Run the workload once, returning (base image bytes, write log).
fn record_workload(dir: &Path) -> (Vec<u8>, gofs::device::WriteLog) {
    let img = dir.join("crash-src.img");
    mkfs(
        &img,
        &MkfsOpts {
            size: Some(16 << 20),
            journal: Some(2 << 20),
            label: "crash".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let base = std::fs::read(&img).unwrap();
    let mut fs = Gofs::open(&img, true).unwrap();
    fs.dev.set_nosync(true); // epochs still advance; no physical fsync
    fs.dev.record_start();
    for i in 0..OPS {
        // one transaction per op: a strict prefix is the invariant
        fs.mkdir(&format!("seq-{i:03}"), 0o755).unwrap();
    }
    let log = fs.dev.record_take();
    (base, log)
}

/// Verify one crash state: recover, fsck, and check the prefix property.
fn verify_state(dir: &Path, state: &[u8], what: &str) {
    let img = dir.join("crash-case.img");
    std::fs::write(&img, state).unwrap();
    {
        // writable open: replay + heal must succeed
        let fs = Gofs::open(&img, true).unwrap_or_else(|e| panic!("{what}: open: {e}"));
        let entries = fs.dir_entries(fs.sb.root_ino).unwrap();
        let mut seqs: Vec<usize> = entries
            .iter()
            .filter_map(|e| e.name.strip_prefix("seq-").and_then(|s| s.parse().ok()))
            .collect();
        seqs.sort_unstable();
        for (k, s) in seqs.iter().enumerate() {
            assert_eq!(*s, k, "{what}: completed ops are not a prefix: {seqs:?}");
        }
    }
    let r = gofs::fsck::check(&img).unwrap_or_else(|e| panic!("{what}: fsck: {e}"));
    assert!(r.errors.is_empty(), "{what}: fsck errors: {:#?}", r.errors);
}

#[test]
fn crash_recovery() {
    let tmp = tempfile::tempdir().unwrap();
    let (base, log) = record_workload(tmp.path());
    let max_epoch = log.last().map(|w| w.0).unwrap_or(0);
    assert!(max_epoch >= 2 * OPS as u32, "expected two epochs per op");
    let mut rng = Rng(0x5f5);

    let apply = |state: &mut Vec<u8>, off: u64, data: &[u8]| {
        let end = off as usize + data.len();
        if state.len() < end {
            state.resize(end, 0);
        }
        state[off as usize..end].copy_from_slice(data);
    };

    for crash_epoch in 0..=max_epoch + 1 {
        // variant 0: clean epoch boundary. 1-2: random subset of the crash
        // epoch. 3: subset plus one torn (garbled) in-flight write.
        for variant in 0..4 {
            let mut state = base.clone();
            let mut inflight: Vec<(u64, usize)> = Vec::new(); // (offset, len) applied this epoch
            for (epoch, off, data) in &log {
                if *epoch < crash_epoch {
                    apply(&mut state, *off, data);
                } else if *epoch == crash_epoch && variant > 0 && rng.coin() {
                    apply(&mut state, *off, data);
                    inflight.push((*off, data.len()));
                }
            }
            if variant == 3 {
                if let Some(&(off, len)) = inflight.last() {
                    // tear it: persisted garbage instead of the payload
                    let garbage: Vec<u8> =
                        (0..len).map(|i| (rng.next() as u8) ^ i as u8).collect();
                    apply(&mut state, off, &garbage);
                }
            }
            verify_state(tmp.path(), &state, &format!("epoch {crash_epoch} variant {variant}"));
        }
    }
}

/// Torn writes inside the journal area specifically: a transaction whose
/// commit record is garbage must be ignored wholesale on replay.
#[test]
fn torn_journal_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let (base, log) = record_workload(tmp.path());
    let mut rng = Rng(42);
    // crash right after each op's journal epoch, tearing one of that
    // epoch's writes (journal descriptor, data, or commit block)
    let max_epoch = log.last().map(|w| w.0).unwrap_or(0);
    for crash_epoch in (0..=max_epoch).step_by(3) {
        let epoch_writes: Vec<&(u32, u64, Vec<u8>)> =
            log.iter().filter(|w| w.0 == crash_epoch).collect();
        if epoch_writes.is_empty() {
            continue;
        }
        let mut state = base.clone();
        for (epoch, off, data) in &log {
            if *epoch <= crash_epoch {
                let end = *off as usize + data.len();
                if state.len() < end {
                    state.resize(end, 0);
                }
                state[*off as usize..end].copy_from_slice(data);
            }
        }
        // tear a random write of the crash epoch
        let victim = epoch_writes[rng.below(epoch_writes.len() as u64) as usize];
        let garbage: Vec<u8> = (0..victim.2.len()).map(|_| rng.next() as u8).collect();
        state[victim.1 as usize..victim.1 as usize + garbage.len()].copy_from_slice(&garbage);
        verify_state(tmp.path(), &state, &format!("torn epoch {crash_epoch}"));
    }
}
