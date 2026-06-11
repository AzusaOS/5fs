//! FUSE mount for 5FS images (read-write by default, `-o ro` for
//! read-only). Requires macFUSE on macOS or fuse (fusermount) on Linux at
//! runtime.

use anyhow::Result;
use clap::Parser;
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyWrite, Request, TimeOrNow,
};
use gofs::fmt::*;
use gofs::fs::Gofs;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Mount a 5FS filesystem.
#[derive(Parser)]
#[command(name = "mount.5fs", version)]
struct Args {
    /// Image file or block device
    device: PathBuf,
    /// Mount point
    mountpoint: PathBuf,
    /// Mount options (ro supported)
    #[arg(short)]
    options: Option<String>,
}

const TTL: Duration = Duration::from_secs(1);
const FUSE_ROOT: u64 = 1;

struct GofsFuse {
    fs: Gofs,
    rw: bool,
}

fn errno(e: &anyhow::Error) -> i32 {
    let m = e.to_string();
    if m.contains("no such file") {
        libc::ENOENT
    } else if m.contains("already exists") {
        libc::EEXIST
    } else if m.contains("not empty") {
        libc::ENOTEMPTY
    } else if m.contains("not a directory") {
        libc::ENOTDIR
    } else if m.contains("is a directory") {
        libc::EISDIR
    } else if m.contains("out of space") || m.contains("no room") {
        libc::ENOSPC
    } else if m.contains("name too long") {
        libc::ENAMETOOLONG
    } else {
        libc::EIO
    }
}

fn to_ts(t: TimeOrNow) -> Ts {
    match t {
        TimeOrNow::Now => Ts::now(),
        TimeOrNow::SpecificTime(st) => match st.duration_since(UNIX_EPOCH) {
            Ok(d) => Ts { sec: d.as_secs() as i64, nsec: d.subsec_nanos() },
            Err(_) => Ts::default(),
        },
    }
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
        let ts = |t: Ts| -> SystemTime {
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

    fn reply_entry(&self, ino: u64, reply: ReplyEntry) {
        match self.fs.read_inode(ino) {
            Ok(i) => reply.entry(&TTL, &self.attr(ino, &i), 0),
            Err(_) => reply.error(libc::EIO),
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
            Ok(Some(ino)) => self.reply_entry(ino, reply),
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

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        if !self.rw {
            reply.error(libc::EROFS);
            return;
        }
        let real = self.real_ino(ino);
        match self.fs.setattr(
            real,
            mode.map(|m| m as u16),
            uid,
            gid,
            size,
            atime.map(to_ts),
            mtime.map(to_ts),
        ) {
            Ok(i) => reply.attr(&TTL, &self.attr(ino, &i)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn readlink(&mut self, _req: &Request, ino: u64, reply: ReplyData) {
        match self.fs.readlink(self.real_ino(ino)) {
            Ok(t) => reply.data(t.as_bytes()),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn mkdir(&mut self, _req: &Request, parent: u64, name: &OsStr, mode: u32, _umask: u32, reply: ReplyEntry) {
        if !self.rw {
            reply.error(libc::EROFS);
            return;
        }
        let parent = self.real_ino(parent);
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.fs.mkdir_at(parent, name, mode as u16) {
            Ok(ino) => self.reply_entry(ino, reply),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn create(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        if !self.rw {
            reply.error(libc::EROFS);
            return;
        }
        let parent = self.real_ino(parent);
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.fs.create_at(parent, name, mode as u16) {
            Ok(ino) => match self.fs.read_inode(ino) {
                Ok(i) => reply.created(&TTL, &self.attr(ino, &i), 0, 0, 0),
                Err(_) => reply.error(libc::EIO),
            },
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        if !self.rw {
            reply.error(libc::EROFS);
            return;
        }
        let parent = self.real_ino(parent);
        let Some(name) = name.to_str() else {
            reply.error(libc::ENOENT);
            return;
        };
        match self.fs.unlink_at(parent, name) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        if !self.rw {
            reply.error(libc::EROFS);
            return;
        }
        let parent = self.real_ino(parent);
        let Some(name) = name.to_str() else {
            reply.error(libc::ENOENT);
            return;
        };
        match self.fs.rmdir_at(parent, name) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn symlink(
        &mut self,
        _req: &Request,
        parent: u64,
        link_name: &OsStr,
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        if !self.rw {
            reply.error(libc::EROFS);
            return;
        }
        let parent = self.real_ino(parent);
        let (Some(name), Some(target)) = (link_name.to_str(), target.to_str()) else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.fs.symlink_at(parent, name, target) {
            Ok(ino) => self.reply_entry(ino, reply),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        if !self.rw {
            reply.error(libc::EROFS);
            return;
        }
        let p1 = self.real_ino(parent);
        let p2 = self.real_ino(newparent);
        let (Some(n1), Some(n2)) = (name.to_str(), newname.to_str()) else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.fs.rename_at(p1, n1, p2, n2) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn link(&mut self, _req: &Request, ino: u64, newparent: u64, newname: &OsStr, reply: ReplyEntry) {
        if !self.rw {
            reply.error(libc::EROFS);
            return;
        }
        let real = self.real_ino(ino);
        let parent = self.real_ino(newparent);
        let Some(name) = newname.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.fs.link_at(real, parent, name) {
            Ok(()) => self.reply_entry(real, reply),
            Err(e) => reply.error(errno(&e)),
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
        match self.fs.read(self.real_ino(ino), offset.max(0) as u64, size as u64) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        if !self.rw {
            reply.error(libc::EROFS);
            return;
        }
        match self.fs.write(self.real_ino(ino), offset.max(0) as u64, data) {
            Ok(n) => reply.written(n as u32),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn fsync(&mut self, _req: &Request, _ino: u64, _fh: u64, _datasync: bool, reply: ReplyEmpty) {
        match self.fs.dev.sync() {
            Ok(()) => reply.ok(),
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
    let ro = args
        .options
        .as_deref()
        .map(|o| o.split(',').any(|t| t == "ro"))
        .unwrap_or(false);
    let fs = Gofs::open(&args.device, !ro)?;
    let label = fs.sb.label();
    let mut options = vec![
        MountOption::FSName("5fs".into()),
        MountOption::Subtype(if label.is_empty() { "5fs".into() } else { label }),
        MountOption::DefaultPermissions,
    ];
    if ro {
        options.push(MountOption::RO);
    } else {
        options.push(MountOption::RW);
    }
    eprintln!(
        "mount.5fs: serving {} on {} ({}); unmount to stop",
        args.device.display(),
        args.mountpoint.display(),
        if ro { "read-only" } else { "read-write" }
    );
    fuser::mount2(GofsFuse { fs, rw: !ro }, &args.mountpoint, &options)?;
    Ok(())
}
