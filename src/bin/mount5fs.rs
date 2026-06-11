//! Read-only FUSE mount for 5FS images. Requires macFUSE on macOS or fuse
//! (fusermount) on Linux at runtime.

use anyhow::Result;
use clap::Parser;
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
    Request,
};
use gofs::fmt::*;
use gofs::fs::Gofs;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

/// Mount a 5FS filesystem (read-only).
#[derive(Parser)]
#[command(name = "mount.5fs", version)]
struct Args {
    /// Image file or block device
    device: PathBuf,
    /// Mount point
    mountpoint: PathBuf,
    /// Mount options (accepted for mount(8) compatibility; only ro supported)
    #[arg(short)]
    options: Option<String>,
}

const TTL: Duration = Duration::from_secs(1);
const FUSE_ROOT: u64 = 1;

struct GofsFuse {
    fs: Gofs,
}

impl GofsFuse {
    fn real_ino(&self, fuse_ino: u64) -> u64 {
        if fuse_ino == FUSE_ROOT {
            self.fs.sb.root_ino
        } else {
            fuse_ino
        }
    }

    fn attr(&self, fuse_ino: u64, i: &Inode) -> FileAttr {
        let ts = |t: Ts| {
            if t.sec >= 0 {
                UNIX_EPOCH + Duration::new(t.sec as u64, t.nsec)
            } else {
                UNIX_EPOCH
            }
        };
        let kind = match i.mode & 0o170000 {
            0o040000 => FileType::Directory,
            0o120000 => FileType::Symlink,
            _ => FileType::RegularFile,
        };
        FileAttr {
            ino: fuse_ino,
            size: i.size,
            blocks: i.size.div_ceil(512),
            atime: ts(i.atime),
            mtime: ts(i.mtime),
            ctime: ts(i.ctime),
            crtime: ts(i.btime),
            kind,
            perm: i.mode & 0o7777,
            nlink: i.nlink,
            uid: i.uid,
            gid: i.gid,
            rdev: 0,
            blksize: self.fs.sb.blocksize,
            flags: 0,
        }
    }
}

impl Filesystem for GofsFuse {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let parent = self.real_ino(parent);
        let Some(name) = name.to_str() else {
            reply.error(libc::ENOENT);
            return;
        };
        match self.fs.dir_lookup(parent, name) {
            Ok(Some(ino)) => match self.fs.read_inode(ino) {
                Ok(i) => reply.entry(&TTL, &self.attr(ino, &i), 0),
                Err(_) => reply.error(libc::EIO),
            },
            Ok(None) => reply.error(libc::ENOENT),
            Err(_) => reply.error(libc::ENOTDIR),
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match self.fs.read_inode(self.real_ino(ino)) {
            Ok(i) => reply.attr(&TTL, &self.attr(ino, &i)),
            Err(_) => reply.error(libc::ENOENT),
        }
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        match self.fs.read_file(self.real_ino(ino)) {
            Ok(data) => {
                let start = (offset as usize).min(data.len());
                let end = (start + size as usize).min(data.len());
                reply.data(&data[start..end]);
            }
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let real = self.real_ino(ino);
        let entries = match self.fs.dir_entries(real) {
            Ok(e) => e,
            Err(_) => {
                reply.error(libc::ENOTDIR);
                return;
            }
        };
        let mut all: Vec<(u64, FileType, String)> = vec![
            (ino, FileType::Directory, ".".into()),
            (FUSE_ROOT, FileType::Directory, "..".into()),
        ];
        for e in entries {
            let ft = if e.ftype == DT_DIR { FileType::Directory } else { FileType::RegularFile };
            all.push((e.ino, ft, e.name));
        }
        for (i, (eino, ft, name)) in all.into_iter().enumerate().skip(offset as usize) {
            if reply.add(eino, (i + 1) as i64, ft, name) {
                break;
            }
        }
        reply.ok();
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let fs = Gofs::open(&args.device, false)?;
    let label = fs.sb.label();
    let options = vec![
        MountOption::RO,
        MountOption::FSName("5fs".into()),
        MountOption::Subtype(if label.is_empty() { "5fs".into() } else { label }),
    ];
    eprintln!(
        "mount.5fs: serving {} on {} (read-only); unmount to stop",
        args.device.display(),
        args.mountpoint.display()
    );
    fuser::mount2(GofsFuse { fs }, &args.mountpoint, &options)?;
    Ok(())
}
