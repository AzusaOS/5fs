//! End-to-end tests: mkfs, file I/O across all formats, directories through
//! hash conversion and splits, namespace ops, space reclamation, journal
//! replay, and resize. fsck must come back clean after every phase.

use gofs::fs::Gofs;
use gofs::mkfs::{mkfs, MkfsOpts};
use std::path::Path;

fn fresh(dir: &tempfile::TempDir, name: &str, mb: u64) -> std::path::PathBuf {
    let img = dir.path().join(name);
    let opts = MkfsOpts { size: Some(mb << 20), label: "test".into(), ..Default::default() };
    mkfs(&img, &opts).unwrap();
    img
}

fn assert_clean(img: &Path, phase: &str) {
    let r = gofs::fsck::check(img).unwrap();
    assert!(r.errors.is_empty(), "fsck errors after {phase}: {:#?}", r.errors);
    assert!(r.warnings.is_empty(), "fsck warnings after {phase}: {:#?}", r.warnings);
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u64 * 31 + seed as u64) as u8).collect()
}

#[test]
fn files_all_formats() {
    let dir = tempfile::tempdir().unwrap();
    let img = fresh(&dir, "f.img", 64);
    assert_clean(&img, "mkfs");

    let small = pattern(100, 1); // EMBED
    let medium = pattern(200_000, 2); // EXTENT
    let large = pattern(5 << 20, 3); // spans multiple cells
    {
        let mut fs = Gofs::open(&img, true).unwrap();
        fs.import("small", &small, 0o644).unwrap();
        fs.import("medium", &medium, 0o644).unwrap();
        fs.import("large", &large, 0o644).unwrap();
        assert!(fs.import("small", &small, 0o644).is_err(), "duplicate must fail");
    }
    assert_clean(&img, "imports");
    {
        let fs = Gofs::open(&img, false).unwrap();
        for (name, data) in [("small", &small), ("medium", &medium), ("large", &large)] {
            let ino = fs.lookup_path(name).unwrap();
            assert_eq!(fs.read_file(ino).unwrap(), **data, "{name} roundtrip");
        }
    }
}

#[test]
fn sparse_and_tree() {
    let dir = tempfile::tempdir().unwrap();
    let img = fresh(&dir, "s.img", 64);
    let chunk = pattern(4096, 7);
    let offsets: Vec<u64> = (0..10).map(|i| i * (1 << 20) + 12345).collect();
    {
        let mut fs = Gofs::open(&img, true).unwrap();
        let ino = fs.create("sparse", 0o644).unwrap();
        // scattered single-block writes force > 6 extents -> TREE
        for off in &offsets {
            fs.write(ino, *off, &chunk).unwrap();
        }
        let inode = fs.read_inode(ino).unwrap();
        assert_eq!(inode.format, gofs::fmt::FMT_TREE, "expected TREE format");
        assert_eq!(inode.size, offsets.last().unwrap() + 4096);
    }
    assert_clean(&img, "sparse writes");
    {
        let fs = Gofs::open(&img, false).unwrap();
        let ino = fs.lookup_path("sparse").unwrap();
        for off in &offsets {
            assert_eq!(fs.read(ino, *off, 4096).unwrap(), chunk, "chunk at {off}");
        }
        // holes read zero
        let hole = fs.read(ino, 4096 + 12345, 4096).unwrap();
        assert!(hole.iter().all(|&b| b == 0), "hole must read zeros");
    }
}

#[test]
fn directory_hashing() {
    let dir = tempfile::tempdir().unwrap();
    let img = fresh(&dir, "d.img", 64);
    let n = 500;
    {
        let mut fs = Gofs::open(&img, true).unwrap();
        fs.mkdir("docs", 0o755).unwrap();
        for i in 0..n {
            let data = pattern(64 + i, (i % 251) as u8);
            fs.import(&format!("docs/file-{i:04}.txt"), &data, 0o644).unwrap();
        }
    }
    assert_clean(&img, "500 creates");
    {
        let fs = Gofs::open(&img, false).unwrap();
        let docs = fs.lookup_path("docs").unwrap();
        let inode = fs.read_inode(docs).unwrap();
        assert!(
            matches!(inode.format, gofs::fmt::FMT_EXTENT | gofs::fmt::FMT_TREE),
            "directory should have converted to hashed (format {})",
            inode.format
        );
        assert_eq!(fs.dir_entries(docs).unwrap().len(), n);
        for i in (0..n).step_by(37) {
            let ino = fs.lookup_path(&format!("docs/file-{i:04}.txt")).unwrap();
            let data = fs.read_file(ino).unwrap();
            assert_eq!(data.len(), 64 + i);
        }
    }
    // remove everything: the directory must collapse and space must return
    {
        let mut fs = Gofs::open(&img, true).unwrap();
        for i in 0..n {
            fs.unlink(&format!("docs/file-{i:04}.txt")).unwrap();
        }
        let docs = fs.lookup_path("docs").unwrap();
        assert_eq!(fs.dir_entries(docs).unwrap().len(), 0);
        let inode = fs.read_inode(docs).unwrap();
        assert_eq!(inode.format, gofs::fmt::FMT_EMPTY, "emptied dir should collapse");
        fs.rmdir("docs").unwrap();
    }
    assert_clean(&img, "remove all");
}

#[test]
fn namespace_ops() {
    let dir = tempfile::tempdir().unwrap();
    let img = fresh(&dir, "n.img", 64);
    {
        let mut fs = Gofs::open(&img, true).unwrap();
        fs.mkdir("a", 0o755).unwrap();
        fs.mkdir("a/b", 0o755).unwrap();
        fs.mkdir("c", 0o755).unwrap();
        fs.import("a/b/data", &pattern(1000, 9), 0o644).unwrap();
        // rename a file across directories
        fs.rename("a/b/data", "c/data2").unwrap();
        assert!(fs.lookup_path("a/b/data").is_err());
        assert_eq!(fs.read_file(fs.lookup_path("c/data2").unwrap()).unwrap(), pattern(1000, 9));
        // move a directory: nlink bookkeeping
        fs.rename("a/b", "c/b").unwrap();
        assert_eq!(fs.read_inode(fs.lookup_path("a").unwrap()).unwrap().nlink, 2);
        assert_eq!(fs.read_inode(fs.lookup_path("c").unwrap()).unwrap().nlink, 3);
        // hard link
        let f = fs.lookup_path("c/data2").unwrap();
        let (p, _) = fs.resolve_parent("c/link").unwrap();
        fs.link_at(f, p, "link").unwrap();
        assert_eq!(fs.read_inode(f).unwrap().nlink, 2);
        fs.unlink("c/data2").unwrap();
        assert_eq!(fs.read_inode(f).unwrap().nlink, 1);
        assert_eq!(fs.read_file(fs.lookup_path("c/link").unwrap()).unwrap(), pattern(1000, 9));
        // symlink
        let (p, n) = fs.resolve_parent("c/sym").unwrap();
        let s = fs.symlink_at(p, &n, "link").unwrap();
        assert_eq!(fs.readlink(s).unwrap(), "link");
        // rmdir refuses non-empty, accepts empty
        assert!(fs.rmdir("c").is_err());
        fs.rmdir("c/b").unwrap();
    }
    assert_clean(&img, "namespace ops");
}

#[test]
fn truncate_and_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let img = fresh(&dir, "t.img", 64);
    {
        let mut fs = Gofs::open(&img, true).unwrap();
        let data = pattern(300_000, 5);
        let ino = fs.import("f", &data, 0o644).unwrap();
        // shrink, check tail is gone and zeros come back on regrow
        fs.truncate(ino, 100_000).unwrap();
        assert_eq!(fs.read_file(ino).unwrap(), data[..100_000]);
        fs.truncate(ino, 200_000).unwrap();
        let back = fs.read_file(ino).unwrap();
        assert_eq!(&back[..100_000], &data[..100_000]);
        assert!(back[100_000..].iter().all(|&b| b == 0), "regrown region must be zeros");
        // overwrite in the middle
        let patch = pattern(5000, 6);
        fs.write(ino, 50_000, &patch).unwrap();
        let back = fs.read_file(ino).unwrap();
        assert_eq!(&back[50_000..55_000], &patch[..]);
        assert_eq!(&back[..50_000], &data[..50_000]);
        // truncate to zero frees everything
        fs.truncate(ino, 0).unwrap();
        assert_eq!(fs.read_inode(ino).unwrap().format, gofs::fmt::FMT_EMPTY);
    }
    assert_clean(&img, "truncate ops");
}

#[test]
fn space_reclamation() {
    let dir = tempfile::tempdir().unwrap();
    let img = fresh(&dir, "r.img", 64);
    let free0;
    {
        let fs = Gofs::open(&img, false).unwrap();
        free0 = fs.sb.free_blocks;
    }
    {
        let mut fs = Gofs::open(&img, true).unwrap();
        fs.mkdir("tmp", 0o755).unwrap();
        for i in 0..50 {
            fs.import(&format!("tmp/f{i}"), &pattern(100_000 + i, i as u8), 0o644).unwrap();
        }
        for i in 0..50 {
            fs.unlink(&format!("tmp/f{i}")).unwrap();
        }
        fs.rmdir("tmp").unwrap();
    }
    assert_clean(&img, "create+delete cycle");
    {
        let fs = Gofs::open(&img, false).unwrap();
        // everything except possible table-arena reservation must be back
        let kept = free0 - fs.sb.free_blocks;
        assert!(
            kept <= gofs::fmt::CELL_BLOCKS as u64,
            "leaked {kept} blocks (more than one arena cell)"
        );
    }
}

#[test]
fn journal_replay() {
    let dir = tempfile::tempdir().unwrap();
    let img = fresh(&dir, "j.img", 64);
    {
        let mut fs = Gofs::open(&img, true).unwrap();
        let (head, seq) = (fs.sb.journal_head, fs.sb.journal_seq);
        fs.import("file", &pattern(100, 1), 0o644).unwrap();
        // simulate a crash between journal write and checkpoint: rewind the
        // superblock journal pointers so the committed txns look unapplied
        fs.sb.journal_head = head;
        fs.sb.journal_seq = seq;
        fs.write_superblock().unwrap();
        assert!(fs.journal_pending().unwrap() > 0, "txns should look pending");
    }
    {
        // writable open must replay (idempotently)
        let fs = Gofs::open(&img, true).unwrap();
        assert_eq!(fs.journal_pending().unwrap(), 0, "replay must clear the journal");
        assert_eq!(fs.read_file(fs.lookup_path("file").unwrap()).unwrap(), pattern(100, 1));
    }
    assert_clean(&img, "journal replay");
}

#[test]
fn resize_grow_relocate_shrink() {
    let dir = tempfile::tempdir().unwrap();
    let img = fresh(&dir, "g.img", 64);
    {
        let mut fs = Gofs::open(&img, true).unwrap();
        let ag1 = fs.grow(96 << 20).unwrap();
        assert_eq!(ag1, 1);
        let ag2 = fs.grow(128 << 20).unwrap();
        assert_eq!(ag2, 2);
    }
    assert_clean(&img, "grow x2");
    {
        let mut fs = Gofs::open(&img, true).unwrap();
        // retire empty AG 1, leaving a hole at 64..96 MiB
        fs.retire(1).unwrap();
        // move AG 2 wholesale into the hole; addresses must stay valid
        fs.relocate(2, 64 << 20).unwrap();
        assert_eq!(fs.map.entries[2].segs[0].dev_offset, 64 << 20);
        // and shrink the device back
        fs.shrink(96 << 20).unwrap();
    }
    assert_clean(&img, "retire+relocate+shrink");
    {
        let fs = Gofs::open(&img, false).unwrap();
        assert_eq!(fs.dev.size, 96 << 20);
        assert_eq!(fs.sb.next_ag, 3, "AG ids are never reused");
    }
}

#[test]
fn corruption_detected() {
    let dir = tempfile::tempdir().unwrap();
    let img = fresh(&dir, "c.img", 32);
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(&img).unwrap();
    let mut b = [0u8; 1];
    f.seek(SeekFrom::Start(100)).unwrap();
    f.read_exact(&mut b).unwrap();
    b[0] ^= 0xff;
    f.seek(SeekFrom::Start(100)).unwrap();
    f.write_all(&b).unwrap();
    drop(f);
    assert!(gofs::fsck::check(&img).is_err(), "corrupt superblock must not parse");
}

#[test]
fn truncate_reclaims_space() {
    let dir = tempfile::tempdir().unwrap();
    let img = fresh(&dir, "tr.img", 64);
    {
        let mut fs = Gofs::open(&img, true).unwrap();
        let free0 = fs.sb.free_blocks;
        let data = pattern(8 << 20, 11); // 8 MiB = 8 L0 cells
        let ino = fs.import("big", &data, 0o644).unwrap();
        let after_import = fs.sb.free_blocks;
        assert!(free0 - after_import >= 2048, "import should consume ~2048 blocks");
        // truncate to 1 MiB + 10 bytes: everything past the first cell
        // boundary above the cutoff must come back
        fs.truncate(ino, (1 << 20) + 10).unwrap();
        let after_trunc = fs.sb.free_blocks;
        assert!(
            after_trunc >= after_import + 6 * 256,
            "truncate freed only {} blocks",
            after_trunc - after_import
        );
        assert_eq!(fs.read_file(ino).unwrap(), data[..(1 << 20) + 10]);
        // and a sparse TREE file trims too
        let s = fs.create("sparse", 0o644).unwrap();
        for i in 0..10u64 {
            fs.write(s, i * (1 << 20), &pattern(4096, 12)).unwrap();
        }
        let before = fs.sb.free_blocks;
        fs.truncate(s, 2 << 20).unwrap();
        assert!(fs.sb.free_blocks > before, "tree truncate must free blocks");
        assert_eq!(fs.read(s, 1 << 20, 4096).unwrap(), pattern(4096, 12));
    }
    assert_clean(&img, "truncate reclamation");
}

#[test]
fn directory_buddy_merge() {
    let dir = tempfile::tempdir().unwrap();
    let img = fresh(&dir, "bm.img", 64);
    let n = 400;
    {
        let mut fs = Gofs::open(&img, true).unwrap();
        fs.mkdir("d", 0o755).unwrap();
        for i in 0..n {
            fs.import(&format!("d/entry-{i:04}"), b"x", 0o644).unwrap();
        }
        // remove most entries: buckets must merge back and stay consistent
        for i in 0..n - 20 {
            fs.unlink(&format!("d/entry-{i:04}")).unwrap();
        }
        let d = fs.lookup_path("d").unwrap();
        assert_eq!(fs.dir_entries(d).unwrap().len(), 20);
        for i in n - 20..n {
            assert!(
                fs.lookup_path(&format!("d/entry-{i:04}")).is_ok(),
                "entry-{i:04} lost in merge"
            );
        }
    }
    assert_clean(&img, "buddy merge");
    {
        // churn after merging: freed buckets must be reusable
        let mut fs = Gofs::open(&img, true).unwrap();
        for i in 0..100 {
            fs.import(&format!("d/new-{i:03}"), b"y", 0o644).unwrap();
        }
        let d = fs.lookup_path("d").unwrap();
        assert_eq!(fs.dir_entries(d).unwrap().len(), 120);
    }
    assert_clean(&img, "post-merge reuse");
}

#[test]
fn kernel_region() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("k.img");
    let kernel = pattern(300_000, 42);
    let opts = MkfsOpts {
        size: Some(64 << 20),
        label: "boot".into(),
        kernel: Some(kernel.clone()),
        ..Default::default()
    };
    let s = mkfs(&img, &opts).unwrap();
    assert_ne!(s.kernel_offset, 0);
    assert_clean(&img, "mkfs --kernel");
    {
        let fs = Gofs::open(&img, false).unwrap();
        // raw read at the physical offset must be the kernel: that is the
        // contiguity guarantee the bootloader relies on
        let mut raw = vec![0u8; kernel.len()];
        fs.dev.pread(&mut raw, fs.sb.kernel_offset).unwrap();
        assert_eq!(raw, kernel, "kernel must be raw-readable at sb_kernel_offset");
        // and visible as a normal file
        let ino = fs.lookup_path("kernel.bin").unwrap();
        assert_eq!(fs.read_file(ino).unwrap(), kernel);
    }
    {
        let mut fs = Gofs::open(&img, true).unwrap();
        let ino = fs.lookup_path("kernel.bin").unwrap();
        // immutable to ordinary writes
        assert!(fs.write(ino, 0, b"boom").is_err(), "write must refuse");
        assert!(fs.truncate(ino, 0).is_err(), "truncate must refuse");
        assert!(fs.unlink("kernel.bin").is_err(), "unlink must refuse");
        // but replaceable through the dedicated path
        let newk = pattern(250_000, 43);
        fs.kernel_update(&newk).unwrap();
        assert_eq!(fs.sb.kernel_end - fs.sb.kernel_offset, 250_000);
        assert_eq!(fs.read_file(ino).unwrap(), newk);
        // too large for the reserved region must refuse
        assert!(fs.kernel_update(&pattern(1 << 20, 44)).is_err());
    }
    assert_clean(&img, "kernel update");
}
