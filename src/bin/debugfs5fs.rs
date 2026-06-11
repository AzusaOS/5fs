use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use gofs::fmt::*;
use gofs::fs::Gofs;
use std::path::PathBuf;

/// Inspect and manipulate a 5FS image.
#[derive(Parser)]
#[command(name = "debugfs.5fs", version)]
struct Args {
    /// Image file or block device
    device: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the superblock
    Sb,
    /// Print the AG map
    Agmap,
    /// Print an AG header and allocator summary
    Ag { id: u32 },
    /// Print an inode (decimal or 0x-hex)
    Inode { ino: String },
    /// List a directory (default /)
    Ls {
        #[arg(default_value = "/")]
        path: String,
    },
    /// Print a file's contents to stdout
    Cat { path: String },
    /// Copy a host file into the filesystem
    Import { host: PathBuf, path: String },
    /// Create a directory
    Mkdir { path: String },
    /// Remove a file or symlink
    Rm { path: String },
    /// Remove an empty directory
    Rmdir { path: String },
    /// Rename/move
    Mv { from: String, to: String },
    /// Create a symlink at `path` pointing to `target`
    Symlink { path: String, target: String },
    /// Scan all mapped blocks for inode signatures
    Scan,
    /// Show journal state
    Journal,
    /// Grow the filesystem to SIZE (e.g. 512M)
    Grow { size: String },
    /// Shrink the filesystem to SIZE (relocating/retiring tail AGs)
    Shrink { size: String },
    /// Move an AG wholesale to a new physical byte offset
    Relocate { ag: u32, offset: u64 },
    /// Retire an empty AG
    Retire { ag: u32 },
}

fn parse_ino(s: &str) -> Result<u64> {
    if let Some(h) = s.strip_prefix("0x") {
        Ok(u64::from_str_radix(h, 16)?)
    } else {
        Ok(s.parse()?)
    }
}

fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M' | 'm') => (&s[..s.len() - 1], 1 << 20),
        Some('G' | 'g') => (&s[..s.len() - 1], 1 << 30),
        Some('T' | 't') => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    Ok(num.parse::<u64>()? * mult)
}

fn show_ts(t: Ts) -> String {
    format!("{}.{:09}", t.sec, t.nsec)
}

fn main() -> Result<()> {
    let args = Args::parse();
    let writable = matches!(
        args.cmd,
        Cmd::Import { .. }
            | Cmd::Mkdir { .. }
            | Cmd::Rm { .. }
            | Cmd::Rmdir { .. }
            | Cmd::Mv { .. }
            | Cmd::Symlink { .. }
            | Cmd::Grow { .. }
            | Cmd::Shrink { .. }
            | Cmd::Relocate { .. }
            | Cmd::Retire { .. }
    );
    let mut fs = Gofs::open(&args.device, writable)?;

    match args.cmd {
        Cmd::Sb => {
            let sb = &fs.sb;
            println!("magic      5FSB  version 2  gen {}", sb.gen);
            println!("label      \"{}\"", sb.label());
            println!("uuid       {}", uuid::Uuid::from_bytes(sb.uuid));
            println!("blocksize  {}   inodesize {}", sb.blocksize, sb.inodesize);
            println!("root_ino   {:#x}", sb.root_ino);
            println!("next_ag    {}", sb.next_ag);
            println!("agmap      phys {:#x} (+{:#x} per copy)", sb.agmap_offset, sb.agmap_length);
            println!(
                "journal    blk {:#x} len {} blocks (seq {}, head {})",
                sb.journal_start, sb.journal_length, sb.journal_seq, sb.journal_head
            );
            if sb.kernel_offset != 0 {
                println!("kernel     phys {:#x}..{:#x}", sb.kernel_offset, sb.kernel_end);
            } else {
                println!("kernel     none");
            }
            println!(
                "blocks     free {}  rsvd {}  full {}",
                sb.free_blocks, sb.rsvd_blocks, sb.full_blocks
            );
        }
        Cmd::Agmap => {
            println!("gen {}  {} entries", fs.map.gen, fs.map.entries.len());
            for (i, e) in fs.map.entries.iter().enumerate() {
                let mut fl = String::new();
                for (bit, name) in
                    [(AGF_PRESENT, "PRESENT"), (AGF_IMMOVABLE, "IMMOVABLE"), (AGF_RETIRED, "RETIRED")]
                {
                    if e.flags & bit != 0 {
                        fl.push_str(name);
                        fl.push(' ');
                    }
                }
                println!("AG {i}: {} blocks, {}", e.length, fl.trim_end());
                for s in &e.segs {
                    println!(
                        "    ag_block {:>10} +{:<10} -> phys {:#x}",
                        s.ag_block, s.blocks, s.dev_offset
                    );
                }
            }
        }
        Cmd::Ag { id } => {
            let h = fs.read_ag_header(id)?;
            println!(
                "AG {}  gen {}  length {} blocks  alloc_root blk {}  tbl_arena {}",
                h.ag_num, h.gen, h.length, h.alloc_root, h.tbl_arena
            );
            println!(
                "counters: free {}  rsvd {}  full {}  ino_hint blk {}",
                h.free_blocks, h.rsvd_blocks, h.full_blocks, h.ino_hint
            );
            let (cells, _) = rt_geometry(h.length, fs.sb.blocksize);
            let table = fs.read_root_table(&h)?;
            let mut n = [0u64; 4];
            let mut row = String::new();
            for c in 0..cells {
                let st = cell_get(&table, c);
                n[st as usize] += 1;
                row.push(['.', 'r', 'F', '+'][st as usize]);
                if row.len() == 64 || c == cells - 1 {
                    println!("  {row}");
                    row.clear();
                }
            }
            println!(
                "cells: {} free, {} rsvd, {} full, {} refined ('.': free 'r': rsvd 'F': full '+': refined)",
                n[0], n[1], n[2], n[3]
            );
        }
        Cmd::Inode { ino } => {
            let num = parse_ino(&ino)?;
            let i = fs.read_inode(num)?;
            let (ag, slot) = blk_split(num);
            println!("inode {:#x}  (AG {ag}, slot {slot})", num);
            println!(
                "format {}  mode {:o}  nlink {}  uid {} gid {}  gen {}",
                i.format, i.mode, i.nlink, i.uid, i.gid, i.gen
            );
            println!("size {}  nblocks {}  xattr {:#x}", i.size, i.nblocks, i.xattr);
            println!(
                "atime {}  mtime {}\nctime {}  btime {}",
                show_ts(i.atime),
                show_ts(i.mtime),
                show_ts(i.ctime),
                show_ts(i.btime)
            );
            match i.format {
                FMT_EMBED if i.is_dir() => {
                    for e in dir_parse(&i.payload) {
                        println!("  entry: {:#x} type {} \"{}\"", e.ino, e.ftype, e.name);
                    }
                }
                FMT_EXTENT => {
                    for e in extents_parse(&i.payload) {
                        println!(
                            "  extent: file_block {} -> blk {:#x} ({} blocks)",
                            e.file_block, e.blk, e.blocks
                        );
                    }
                }
                FMT_TREE => {
                    use gofs::extent::{cov, root_child, root_level, root_state};
                    let level = root_level(&i.payload);
                    println!("  tree root: level {level} (child coverage {} blocks)", cov(level));
                    for c in 0..ROOT_FANOUT {
                        let st = root_state(&i.payload, c);
                        let (blocks, addr) = root_child(&i.payload, c);
                        match st {
                            CELL_FREE => {}
                            CELL_FULL => println!("    child {c}: FULL {blocks} blocks @ {addr:#x}"),
                            CELL_REFINED => println!("    child {c}: REFINED -> node {addr:#x}"),
                            s => println!("    child {c}: bad state {s}"),
                        }
                    }
                }
                _ => {}
            }
        }
        Cmd::Ls { path } => {
            let dir = fs.lookup_path(&path)?;
            for e in fs.dir_entries(dir)? {
                let i = fs.read_inode(e.ino)?;
                println!("{:o} {:>10} {:#x}  {}", i.mode, i.size, e.ino, e.name);
            }
        }
        Cmd::Cat { path } => {
            let ino = match fs.lookup_path(&path) {
                Ok(i) => i,
                Err(_) => parse_ino(&path).map_err(|_| anyhow!("{path}: not found"))?,
            };
            use std::io::Write;
            std::io::stdout().write_all(&fs.read_file(ino)?)?;
        }
        Cmd::Import { host, path } => {
            let data = std::fs::read(&host)?;
            let ino = fs.import(&path, &data, 0o644)?;
            println!("imported {} bytes as \"{path}\" (inode {ino:#x})", data.len());
        }
        Cmd::Mkdir { path } => {
            let ino = fs.mkdir(&path, 0o755)?;
            println!("created directory \"{path}\" (inode {ino:#x})");
        }
        Cmd::Rm { path } => fs.unlink(&path)?,
        Cmd::Rmdir { path } => fs.rmdir(&path)?,
        Cmd::Mv { from, to } => fs.rename(&from, &to)?,
        Cmd::Symlink { path, target } => {
            let (p, n) = fs.resolve_parent(&path)?;
            let ino = fs.symlink_at(p, &n, &target)?;
            println!("created symlink \"{path}\" -> \"{target}\" (inode {ino:#x})");
        }
        Cmd::Scan => {
            let bs = fs.sb.blocksize as usize;
            let isz = fs.sb.inodesize as usize;
            let mut found = 0;
            for (agi, e) in fs.map.entries.iter().enumerate() {
                if e.flags & AGF_PRESENT == 0 {
                    continue;
                }
                for local in 0..e.length {
                    let Ok(buf) = fs.read_block(blk_addr(agi as u32, local)) else { continue };
                    for (si, slot) in buf.chunks(isz).enumerate() {
                        if get_u16(slot, 0) == INODE_MAGIC {
                            let ino = ino_addr(agi as u32, local * (bs / isz) as u32 + si as u32);
                            match Inode::parse(slot) {
                                Ok(i) => {
                                    println!(
                                        "{:#x}: mode {:o} size {} format {}",
                                        ino, i.mode, i.size, i.format
                                    );
                                    found += 1;
                                }
                                Err(e) => println!("{ino:#x}: signature but invalid: {e}"),
                            }
                        }
                    }
                }
            }
            println!("{found} inode(s)");
        }
        Cmd::Journal => {
            println!(
                "journal: blk {:#x}, {} blocks, next seq {}, head {}",
                fs.sb.journal_start, fs.sb.journal_length, fs.sb.journal_seq, fs.sb.journal_head
            );
            match fs.journal_pending()? {
                0 => println!("clean: no transactions pending replay"),
                n => println!("{n} transaction(s) pending replay"),
            }
        }
        Cmd::Grow { size } => {
            let ag = fs.grow(parse_size(&size)?)?;
            println!("grown: new AG {ag}");
        }
        Cmd::Shrink { size } => {
            fs.shrink(parse_size(&size)?)?;
            println!("shrunk to {size}");
        }
        Cmd::Relocate { ag, offset } => {
            fs.relocate(ag, offset)?;
            println!("AG {ag} relocated to {offset:#x}");
        }
        Cmd::Retire { ag } => {
            fs.retire(ag)?;
            println!("AG {ag} retired");
        }
    }
    Ok(())
}
