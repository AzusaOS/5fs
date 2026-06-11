//! Block device access (file-backed; a real device node works the same way).

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;

pub struct Device {
    file: File,
    pub size: u64,
    pub writable: bool,
}

impl Device {
    pub fn open(path: &Path, writable: bool) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(writable)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        let size = file.metadata()?.len();
        Ok(Device { file, size, writable })
    }

    pub fn create(path: &Path, size: u64) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("create {}", path.display()))?;
        if file.metadata()?.len() != size {
            file.set_len(size)?;
        }
        Ok(Device { file, size, writable: true })
    }

    pub fn pread(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        self.file
            .read_exact_at(buf, offset)
            .with_context(|| format!("read {} bytes at {offset}", buf.len()))
    }

    pub fn pwrite(&self, buf: &[u8], offset: u64) -> Result<()> {
        self.file
            .write_all_at(buf, offset)
            .with_context(|| format!("write {} bytes at {offset}", buf.len()))
    }

    pub fn sync(&self) -> Result<()> {
        self.file.sync_all().context("sync")
    }

    /// Resize a file-backed device (grow/shrink).
    pub fn set_len(&mut self, size: u64) -> Result<()> {
        self.file.set_len(size).context("set_len")?;
        self.size = size;
        Ok(())
    }
}
