//! Model-based randomized testing: a seeded stream of namespace and file
//! operations is applied to the filesystem and to a trivial in-memory model
//! in lockstep. Each op must succeed/fail identically on both sides; at the
//! end the full tree and every file's contents must match, and fsck must be
//! clean. Failures are reproducible from the printed seed.

use gofs::fs::Gofs;
use gofs::mkfs::{mkfs, MkfsOpts};
use std::collections::BTreeMap;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[derive(Default)]
struct Model {
    /// path -> file contents
    files: BTreeMap<String, Vec<u8>>,
    /// directory paths ("" is the root)
    dirs: Vec<String>,
}

impl Model {
    fn new() -> Self {
        Model { files: BTreeMap::new(), dirs: vec![String::new()] }
    }
    fn parent_exists(&self, path: &str) -> bool {
        let parent = match path.rfind('/') {
            Some(i) => &path[..i],
            None => "",
        };
        self.dirs.iter().any(|d| d == parent)
    }
    fn is_dir(&self, path: &str) -> bool {
        self.dirs.iter().any(|d| d == path)
    }
    fn exists(&self, path: &str) -> bool {
        self.is_dir(path) || self.files.contains_key(path)
    }
    fn dir_empty(&self, path: &str) -> bool {
        let pfx = format!("{path}/");
        !self.files.keys().any(|f| f.starts_with(&pfx))
            && !self.dirs.iter().any(|d| d.starts_with(&pfx))
    }
}

fn pattern(len: usize, seed: u64) -> Vec<u8> {
    (0..len).map(|i| (i as u64 * 31 + seed) as u8).collect()
}

/// Random path: 1-3 components from a small alphabet, so collisions and
/// nesting happen constantly.
fn rand_path(rng: &mut Rng) -> String {
    let depth = 1 + rng.below(3);
    let mut p = String::new();
    for i in 0..depth {
        if i > 0 {
            p.push('/');
        }
        p.push(char::from(b'a' + rng.below(6) as u8));
    }
    p
}

fn run_seed(seed: u64, ops: usize) {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join(format!("m{seed}.img"));
    mkfs(&img, &MkfsOpts { size: Some(64 << 20), label: "model".into(), ..Default::default() })
        .unwrap();
    let mut fs = Gofs::open(&img, true).unwrap();
    fs.dev.set_nosync(true);
    let mut model = Model::new();
    let mut rng = Rng(seed);

    for op in 0..ops {
        let path = rand_path(&mut rng);
        let ctx = format!("seed {seed} op {op}");
        match rng.below(100) {
            // create file with content
            0..=24 => {
                let data = pattern(rng.below(200_000) as usize, op as u64);
                let ok = fs.import(&path, &data, 0o644).is_ok();
                let mok = model.parent_exists(&path) && !model.exists(&path);
                assert_eq!(ok, mok, "{ctx}: import {path}");
                if ok {
                    model.files.insert(path, data);
                }
            }
            // overwrite a range of an existing file
            25..=44 => {
                let data = pattern(1 + rng.below(64_000) as usize, op as u64 + 7);
                let off = rng.below(100_000);
                let ok = match fs.lookup_path(&path) {
                    Ok(ino) => fs.write(ino, off, &data).is_ok(),
                    Err(_) => false,
                };
                let mok = model.files.contains_key(&path);
                assert_eq!(ok, mok, "{ctx}: write {path}");
                if ok {
                    let f = model.files.get_mut(&path).unwrap();
                    let end = off as usize + data.len();
                    if f.len() < end {
                        f.resize(end, 0);
                    }
                    f[off as usize..end].copy_from_slice(&data);
                }
            }
            // truncate
            45..=54 => {
                let sz = rng.below(150_000);
                let ok = match fs.lookup_path(&path) {
                    Ok(ino) if model.files.contains_key(&path) => fs.truncate(ino, sz).is_ok(),
                    _ => false,
                };
                let mok = model.files.contains_key(&path);
                assert_eq!(ok, mok, "{ctx}: truncate {path}");
                if ok {
                    model.files.get_mut(&path).unwrap().resize(sz as usize, 0);
                }
            }
            // unlink
            55..=69 => {
                let ok = fs.unlink(&path).is_ok();
                let mok = model.files.contains_key(&path);
                assert_eq!(ok, mok, "{ctx}: unlink {path}");
                if ok {
                    model.files.remove(&path);
                }
            }
            // mkdir
            70..=84 => {
                let ok = fs.mkdir(&path, 0o755).is_ok();
                let mok = model.parent_exists(&path) && !model.exists(&path);
                assert_eq!(ok, mok, "{ctx}: mkdir {path}");
                if ok {
                    model.dirs.push(path);
                }
            }
            // rmdir
            85..=92 => {
                let ok = fs.rmdir(&path).is_ok();
                let mok = model.is_dir(&path) && model.dir_empty(&path);
                assert_eq!(ok, mok, "{ctx}: rmdir {path}");
                if ok {
                    model.dirs.retain(|d| d != &path);
                }
            }
            // rename (files and dirs)
            _ => {
                let to = rand_path(&mut rng);
                if to == path || to.starts_with(&format!("{path}/")) {
                    continue;
                }
                let ok = fs.rename(&path, &to).is_ok();
                let src_file = model.files.contains_key(&path);
                let src_dir = model.is_dir(&path);
                let tgt_replaceable = if model.is_dir(&to) {
                    model.dir_empty(&to) && src_dir
                } else if model.files.contains_key(&to) {
                    src_file
                } else {
                    true
                };
                let mok = (src_file || src_dir)
                    && model.parent_exists(&to)
                    && tgt_replaceable
                    && !to.starts_with(&format!("{path}/"));
                assert_eq!(ok, mok, "{ctx}: rename {path} -> {to}");
                if ok {
                    if model.is_dir(&to) {
                        model.dirs.retain(|d| d != &to);
                    }
                    model.files.remove(&to);
                    if src_file {
                        let data = model.files.remove(&path).unwrap();
                        model.files.insert(to, data);
                    } else {
                        let pfx = format!("{path}/");
                        let moved: Vec<String> = model
                            .dirs
                            .iter()
                            .filter(|d| **d == path || d.starts_with(&pfx))
                            .cloned()
                            .collect();
                        for d in moved {
                            model.dirs.retain(|x| x != &d);
                            model.dirs.push(format!("{to}{}", &d[path.len()..]));
                        }
                        let files: Vec<String> =
                            model.files.keys().filter(|f| f.starts_with(&pfx)).cloned().collect();
                        for f in files {
                            let data = model.files.remove(&f).unwrap();
                            model.files.insert(format!("{to}{}", &f[path.len()..]), data);
                        }
                    }
                }
            }
        }
    }

    // full comparison: every model file must exist with identical bytes,
    // every model dir must exist, and the fs must contain nothing else
    for (path, data) in &model.files {
        let ino = fs.lookup_path(path).unwrap_or_else(|e| panic!("seed {seed}: {path}: {e}"));
        let got = fs.read_file(ino).unwrap();
        assert_eq!(&got, data, "seed {seed}: content mismatch at {path}");
    }
    let mut fs_files = 0usize;
    let mut fs_dirs = 0usize;
    let mut stack = vec![(String::new(), fs.sb.root_ino)];
    while let Some((path, ino)) = stack.pop() {
        fs_dirs += 1;
        for e in fs.dir_entries(ino).unwrap() {
            let p = if path.is_empty() { e.name.clone() } else { format!("{path}/{}", e.name) };
            let i = fs.read_inode(e.ino).unwrap();
            if i.is_dir() {
                assert!(model.is_dir(&p), "seed {seed}: unexpected dir {p}");
                stack.push((p, e.ino));
            } else {
                assert!(model.files.contains_key(&p), "seed {seed}: unexpected file {p}");
                fs_files += 1;
            }
        }
    }
    assert_eq!(fs_files, model.files.len(), "seed {seed}: file count");
    assert_eq!(fs_dirs, model.dirs.len(), "seed {seed}: dir count");
    drop(fs);

    let r = gofs::fsck::check(&img).unwrap();
    assert!(r.errors.is_empty(), "seed {seed}: fsck errors {:#?}", r.errors);
    assert!(r.warnings.is_empty(), "seed {seed}: fsck warnings {:#?}", r.warnings);
}

#[test]
fn model_seed_1() {
    run_seed(1, 1500);
}

#[test]
fn model_seed_2() {
    run_seed(0xdeadbeef, 1500);
}

#[test]
fn model_seed_3() {
    run_seed(0x5f5f5f5f, 1500);
}

/// Long run for `make stress`.
#[test]
#[ignore = "stress: run with make stress"]
fn model_long() {
    for seed in [7, 8, 9, 10] {
        run_seed(seed, 20_000);
    }
}
