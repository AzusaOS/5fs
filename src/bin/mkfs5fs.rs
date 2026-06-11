use anyhow::{bail, Result};
use clap::Parser;
use gofs::mkfs::{mkfs, MkfsOpts};
use std::path::PathBuf;

/// Create a 5FS filesystem.
#[derive(Parser)]
#[command(name = "mkfs.5fs", version)]
struct Args {
    /// Image file or block device
    device: PathBuf,
    /// Size to create the image with (e.g. 256M, 8G); default: existing size
    #[arg(short, long, value_parser = parse_size)]
    size: Option<u64>,
    /// Volume label (up to 16 UTF-16 units)
    #[arg(short = 'L', long, default_value = "")]
    label: String,
    /// Block size in bytes
    #[arg(short, long, default_value_t = 4096)]
    blocksize: u32,
    /// Journal size (e.g. 16M); default: 128M capped to 1/16 of the device
    #[arg(short, long, value_parser = parse_size)]
    journal_size: Option<u64>,
    /// Kernel image to store contiguously for the bootloader (becomes /kernel.bin)
    #[arg(short, long)]
    kernel: Option<PathBuf>,
    /// Overwrite an existing filesystem
    #[arg(short, long)]
    force: bool,
}

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M' | 'm') => (&s[..s.len() - 1], 1 << 20),
        Some('G' | 'g') => (&s[..s.len() - 1], 1 << 30),
        Some('T' | 't') => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    num.parse::<u64>().map(|n| n * mult).map_err(|e| e.to_string())
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.device.exists() && args.size.is_none() && !args.force {
        // refuse to clobber something that already looks like a filesystem
        let mut head = [0u8; 4];
        if let Ok(dev) = gofs::device::Device::open(&args.device, false) {
            if dev.pread(&mut head, 0).is_ok() && head != [0u8; 4] {
                bail!(
                    "{} contains data (starts with {:02x?}); use --force",
                    args.device.display(),
                    head
                );
            }
        }
    }
    let kernel = match &args.kernel {
        Some(p) => Some(std::fs::read(p)?),
        None => None,
    };
    let opts = MkfsOpts {
        size: args.size,
        blocksize: args.blocksize,
        journal: args.journal_size,
        label: args.label,
        kernel,
        ..Default::default()
    };
    let s = mkfs(&args.device, &opts)?;
    println!("5FS v2 created on {}", args.device.display());
    println!("  size:    {} bytes ({} blocks)", s.size, s.blocks);
    println!("  AGs:     {}", s.ags);
    println!("  journal: {} blocks", s.journal_blocks);
    println!("  root:    inode {:#x}", s.root_ino);
    println!("  free:    {} blocks", s.free_blocks);
    if s.kernel_offset != 0 {
        println!("  kernel:  phys {:#x} (/kernel.bin, contiguous)", s.kernel_offset);
    }
    Ok(())
}
