//! Stress tests — ignored by default; run with `make stress`
//! (cargo test --release -- --ignored). Physical syncs are disabled, so
//! these measure structural integrity under volume, not durability.

use gofs::fs::Gofs;
use gofs::mkfs::{mkfs, MkfsOpts};
use std::path::Path;

fn fresh(dir: &Path, name: &str, mb: u64) -> std::path::PathBuf {
    let img = dir.join(name);
    let opts = MkfsOpts {
        size: Some(mb << 20),
        journal: Some(8 << 20),
        label: "stress".into(),
        ..Default::default()
    };
    mkfs(&img, &opts).unwrap();
    img
}

fn assert_clean(img: &Path, phase: &str) {
    let r = gofs::fsck::check(img).unwrap();
    assert!(r.errors.is_empty(), "fsck errors after {phase}: {:#?}", r.errors);
    assert!(r.warnings.is_empty(), "fsck warnings after {phase}: {:#?}", r.warnings);
}

/// 200,000 files across a three-level directory tree, then verify, then
/// tear a subtree down and check the space comes back.
#[test]
#[ignore = "stress: run with make stress"]
fn two_hundred_thousand_files() {
    let tmp = tempfile::tempdir().unwrap();
    let img = fresh(tmp.path(), "many.img", 512);
    const D: usize = 50; // top-level dirs
    const S: usize = 40; // subdirs each
    const F: usize = 100; // files each
    {
        let mut fs = Gofs::open(&img, true).unwrap();
        fs.dev.set_nosync(true);
        let free0 = fs.sb.free_blocks;
        for d in 0..D {
            fs.mkdir(&format!("d{d:02}"), 0o755).unwrap();
            for s in 0..S {
                fs.mkdir(&format!("d{d:02}/s{s:02}"), 0o755).unwrap();
                for f in 0..F {
                    let path = format!("d{d:02}/s{s:02}/f{f:03}");
                    let data = format!("{path}: file number {}", (d * S + s) * F + f);
                    fs.import(&path, data.as_bytes(), 0o644).unwrap();
                }
            }
            if d % 10 == 9 {
                eprintln!("  created {} files...", (d + 1) * S * F);
            }
        }
        // spot-check lookups across the tree
        for (d, s, f) in [(0, 0, 0), (24, 19, 50), (49, 39, 99), (13, 37, 7)] {
            let path = format!("d{d:02}/s{s:02}/f{f:03}");
            let ino = fs.lookup_path(&path).unwrap();
            let data = fs.read_file(ino).unwrap();
            assert!(String::from_utf8(data).unwrap().starts_with(&path));
        }
        // tear down one top-level dir (4,000 files) and verify reclamation
        let before = fs.sb.free_blocks;
        for s in 0..S {
            for f in 0..F {
                fs.unlink(&format!("d00/s{s:02}/f{f:03}")).unwrap();
            }
            fs.rmdir(&format!("d00/s{s:02}")).unwrap();
        }
        fs.rmdir("d00").unwrap();
        assert!(fs.sb.free_blocks > before, "teardown must free space");
        eprintln!(
            "  free blocks: initial {free0}, full {} after teardown {}",
            before, fs.sb.free_blocks
        );
    }
    eprintln!("  running fsck over {} files...", (D - 1) * S * F);
    let r = gofs::fsck::check(&img).unwrap();
    assert!(r.errors.is_empty(), "fsck errors: {:#?}", r.errors);
    assert!(r.warnings.is_empty(), "fsck warnings: {:#?}", r.warnings);
    let counted = r
        .info
        .iter()
        .find(|m| m.contains("namespace"))
        .cloned()
        .unwrap_or_default();
    assert!(
        counted.contains(&format!("{} file(s)", (D - 1) * S * F)),
        "unexpected namespace count: {counted}"
    );
}

/// One directory pushed toward the depth-9 limit: graceful behavior at the
/// boundary, full teardown after.
#[test]
#[ignore = "stress: run with make stress"]
fn directory_at_the_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let img = fresh(tmp.path(), "limit.img", 256);
    let mut created = Vec::new();
    {
        let mut fs = Gofs::open(&img, true).unwrap();
        fs.dev.set_nosync(true);
        fs.mkdir("big", 0o755).unwrap();
        for i in 0..60_000 {
            match fs.import(&format!("big/e{i:05}"), b"x", 0o644) {
                Ok(_) => created.push(i),
                Err(e) => {
                    // the only acceptable failure is the documented limit
                    assert!(e.to_string().contains("maximum depth"), "unexpected error: {e}");
                    eprintln!("  hit depth limit at {} entries (expected)", created.len());
                    break;
                }
            }
        }
        assert!(created.len() > 30_000, "should fit >30k entries before the limit");
        let d = fs.lookup_path("big").unwrap();
        assert_eq!(fs.dir_entries(d).unwrap().len(), created.len());
    }
    assert_clean(&img, "limit fill");
    {
        let mut fs = Gofs::open(&img, true).unwrap();
        fs.dev.set_nosync(true);
        for i in &created {
            fs.unlink(&format!("big/e{i:05}")).unwrap();
        }
        fs.rmdir("big").unwrap();
    }
    assert_clean(&img, "limit teardown");
}

/// Create/delete churn: free space must return to baseline every cycle
/// (modulo the one-time table arena) — catches slow leaks.
#[test]
#[ignore = "stress: run with make stress"]
fn churn_no_leaks() {
    let tmp = tempfile::tempdir().unwrap();
    let img = fresh(tmp.path(), "churn.img", 128);
    let mut fs = Gofs::open(&img, true).unwrap();
    fs.dev.set_nosync(true);
    let mut baseline = None;
    for cycle in 0..25 {
        fs.mkdir("work", 0o755).unwrap();
        for i in 0..1500 {
            let data = vec![(i % 251) as u8; 50 + (i % 9) * 8_000];
            fs.import(&format!("work/f{i}"), &data, 0o644).unwrap();
        }
        for i in 0..1500 {
            fs.unlink(&format!("work/f{i}")).unwrap();
        }
        fs.rmdir("work").unwrap();
        match baseline {
            None => baseline = Some(fs.sb.free_blocks),
            Some(b) => assert_eq!(
                fs.sb.free_blocks, b,
                "cycle {cycle}: free blocks drifted from {b}"
            ),
        }
    }
    drop(fs);
    assert_clean(&img, "25 churn cycles");
}

/// Huge sparse offsets: multi-level extent tree growth and lookup.
#[test]
#[ignore = "stress: run with make stress"]
fn huge_sparse_offsets() {
    let tmp = tempfile::tempdir().unwrap();
    let img = fresh(tmp.path(), "sparse.img", 64);
    let offsets: [u64; 5] = [0, 1 << 20, 10 << 30, (1 << 40) + 12345, 100 << 40];
    {
        let mut fs = Gofs::open(&img, true).unwrap();
        fs.dev.set_nosync(true);
        let ino = fs.create("vast", 0o644).unwrap();
        for (i, off) in offsets.iter().enumerate() {
            fs.write(ino, *off, format!("mark {i}").as_bytes()).unwrap();
        }
        for (i, off) in offsets.iter().enumerate() {
            let got = fs.read(ino, *off, 6).unwrap();
            assert_eq!(got, format!("mark {i}").as_bytes(), "marker {i}");
        }
        let inode = fs.read_inode(ino).unwrap();
        assert_eq!(inode.size, 100 << 40 | 6);
        assert!(inode.nblocks < 64, "sparse file must not allocate much");
    }
    assert_clean(&img, "huge sparse offsets");
}
