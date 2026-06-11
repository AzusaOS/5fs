//! Block device access (file-backed; a real device node works the same way).
//!
//! Test hooks: `set_nosync` skips physical syncs (stress tests), and the
//! write recorder captures every pwrite tagged with a sync epoch so crash
//! tests can reconstruct any "power was cut here" state — all writes of
//! finished epochs plus an arbitrary subset of the current one, since
//! nothing orders writes between two syncs.

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Mutex;

/// One recorded write: (sync epoch, byte offset, data).
pub type WriteLog = Vec<(u32, u64, Vec<u8>)>;

pub struct Device {
    file: File,
    pub size: u64,
    pub writable: bool,
    nosync: bool,
    recorder: Mutex<Option<(u32, WriteLog)>>,
}

impl Device {
    fn new(file: File, size: u64, writable: bool) -> Self {
        Device { file, size, writable, nosync: false, recorder: Mutex::new(None) }
    }

    pub fn open(path: &Path, writable: bool) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(writable)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        let size = file.metadata()?.len();
        Ok(Self::new(file, size, writable))
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
        Ok(Self::new(file, size, true))
    }

    pub fn pread(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        self.file
            .read_exact_at(buf, offset)
            .with_context(|| format!("read {} bytes at {offset}", buf.len()))
    }

    pub fn pwrite(&self, buf: &[u8], offset: u64) -> Result<()> {
        if let Some((epoch, log)) = self.recorder.lock().unwrap().as_mut() {
            log.push((*epoch, offset, buf.to_vec()));
        }
        self.file
            .write_all_at(buf, offset)
            .with_context(|| format!("write {} bytes at {offset}", buf.len()))
    }

    pub fn sync(&self) -> Result<()> {
        if let Some((epoch, _)) = self.recorder.lock().unwrap().as_mut() {
            *epoch += 1;
        }
        if self.nosync {
            return Ok(());
        }
        self.file.sync_all().context("sync")
    }

    /// Resize a file-backed device (grow/shrink).
    pub fn set_len(&mut self, size: u64) -> Result<()> {
        self.file.set_len(size).context("set_len")?;
        self.size = size;
        Ok(())
    }

    /// Skip physical syncs (testing only — durability is the caller's
    /// problem). Sync epochs still advance for the recorder.
    pub fn set_nosync(&mut self, v: bool) {
        self.nosync = v;
    }

    /// Start capturing writes (epoch 0 begins now).
    pub fn record_start(&self) {
        *self.recorder.lock().unwrap() = Some((0, Vec::new()));
    }

    /// Stop capturing and return the log.
    pub fn record_take(&self) -> WriteLog {
        self.recorder.lock().unwrap().take().map(|(_, l)| l).unwrap_or_default()
    }
}
