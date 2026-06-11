// Debug harness: replays a model seed, verifying every model file after
// every op, and prints the first diverging operation.
use gofs::fs::Gofs;
use gofs::mkfs::{mkfs, MkfsOpts};
use std::collections::BTreeMap;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    fn below(&mut self, n: u64) -> u64 { self.next() % n }
}
fn pattern(len: usize, seed: u64) -> Vec<u8> {
    (0..len).map(|i| (i as u64 * 31 + seed) as u8).collect()
}
fn rand_path(rng: &mut Rng) -> String {
    let depth = 1 + rng.below(3);
    let mut p = String::new();
    for i in 0..depth {
        if i > 0 { p.push('/'); }
        p.push(char::from(b'a' + rng.below(6) as u8));
    }
    p
}

fn main() {
    let seed: u64 = std::env::args().nth(1).unwrap().parse().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("dbg.img");
    mkfs(&img, &MkfsOpts { size: Some(64 << 20), label: "dbg".into(), ..Default::default() }).unwrap();
    let mut fs = Gofs::open(&img, true).unwrap();
    fs.dev.set_nosync(true);
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut dirs: Vec<String> = vec![String::new()];
    let mut rng = Rng(seed);
    let parent_exists = |dirs: &Vec<String>, p: &str| {
        let parent = match p.rfind('/') { Some(i) => &p[..i], None => "" };
        dirs.iter().any(|d| d == parent)
    };
    for op in 0..1500u64 {
        let path = rand_path(&mut rng);
        #[allow(unused_assignments)]
        let mut desc = String::new();
        match rng.below(100) {
            0..=24 => {
                let data = pattern(rng.below(200_000) as usize, op);
                desc = format!("import {path} len {}", data.len());
                if fs.import(&path, &data, 0o644).is_ok() { files.insert(path.clone(), data); }
            }
            25..=44 => {
                let data = pattern(1 + rng.below(64_000) as usize, op + 7);
                let off = rng.below(100_000);
                desc = format!("write {path} off {off} len {}", data.len());
                if let Ok(ino) = fs.lookup_path(&path) {
                    if files.contains_key(&path) && fs.write(ino, off, &data).is_ok() {
                        let f = files.get_mut(&path).unwrap();
                        let end = off as usize + data.len();
                        if f.len() < end { f.resize(end, 0); }
                        f[off as usize..end].copy_from_slice(&data);
                    }
                }
            }
            45..=54 => {
                let sz = rng.below(150_000);
                desc = format!("truncate {path} to {sz}");
                if let Ok(ino) = fs.lookup_path(&path) {
                    if files.contains_key(&path) && fs.truncate(ino, sz).is_ok() {
                        files.get_mut(&path).unwrap().resize(sz as usize, 0);
                    }
                }
            }
            55..=69 => {
                desc = format!("unlink {path}");
                if fs.unlink(&path).is_ok() { files.remove(&path); }
            }
            70..=84 => {
                desc = format!("mkdir {path}");
                if fs.mkdir(&path, 0o755).is_ok() { dirs.push(path.clone()); }
            }
            85..=92 => {
                desc = format!("rmdir {path}");
                if fs.rmdir(&path).is_ok() { dirs.retain(|d| d != &path); }
            }
            _ => {
                let to = rand_path(&mut rng);
                desc = format!("rename {path} -> {to}");
                if to == path || to.starts_with(&format!("{path}/")) { continue; }
                if fs.rename(&path, &to).is_ok() {
                    let src_file = files.contains_key(&path);
                    if dirs.iter().any(|d| d == &to) { dirs.retain(|d| d != &to); }
                    files.remove(&to);
                    if src_file {
                        let d = files.remove(&path).unwrap();
                        files.insert(to.clone(), d);
                    } else {
                        let pfx = format!("{path}/");
                        let moved: Vec<String> = dirs.iter().filter(|d| **d == path || d.starts_with(&pfx)).cloned().collect();
                        for d in moved { dirs.retain(|x| x != &d); dirs.push(format!("{to}{}", &d[path.len()..])); }
                        let fl: Vec<String> = files.keys().filter(|f| f.starts_with(&pfx)).cloned().collect();
                        for f in fl { let data = files.remove(&f).unwrap(); files.insert(format!("{to}{}", &f[path.len()..]), data); }
                    }
                }
            }
        }
        if path == "f" || desc.contains(" f ") || desc.ends_with("-> f") || desc.contains("f ->") {
            println!("op {op}: {desc}");
        }
        let _ = parent_exists(&dirs, "x");
        // verify all
        for (p, data) in &files {
            let ino = match fs.lookup_path(p) { Ok(i) => i, Err(e) => { println!("op {op} [{desc}]: {p} missing: {e}"); return; } };
            let got = fs.read_file(ino).unwrap();
            if &got != data {
                println!("op {op} [{desc}]: DIVERGED at {p}: fs len {} model len {}", got.len(), data.len());
                for (i, (a, b)) in got.iter().zip(data.iter()).enumerate() {
                    if a != b { println!("  first diff at byte {i}: fs {a} model {b}"); break; }
                }
                if got.len() != data.len() { println!("  length differs"); }
                return;
            }
        }
    }
    println!("no divergence in 1500 ops");
}
