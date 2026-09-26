// Adapted from Prodex (https://github.com/christiandoxa/prodex),
// crates/prodex-app/src/runtime_tools/sub_agents.rs, Apache-2.0.
// Modified: slot files are stable and never deleted, the pool can run in an
// operator-provisioned mode for shared hosts, and locking uses std file locks.

//! Host-wide concurrency slots backed by exclusively locked files.
//!
//! A slot is a file named `slot-NNN.lock` in the slot directory. Holding an
//! exclusive lock on the file means holding the slot. The lock is released when
//! the [`SlotLease`] is dropped or when the owning process exits, so a crashed
//! run cannot leak a slot.
//!
//! Slot files are never deleted or recreated by Timon. Deleting a lock file
//! while another process holds a lock on it would let a third process lock a
//! fresh file under the same name and exceed the limit.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};

/// How missing slot files are handled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlotMode {
    /// Slot files must already exist. Used on shared hosts, where an operator
    /// provisions the directory and files with the right owner, group and mode.
    Provisioned,
    /// The directory (mode 0700) and missing slot files (mode 0600) are
    /// created. Used for single-user development.
    CreateMissing,
}

/// A fixed-size pool of slots in one directory.
#[derive(Clone, Debug)]
pub struct SlotPool {
    dir: PathBuf,
    limit: NonZeroU16,
    mode: SlotMode,
}

/// A held slot. Dropping it releases the slot.
#[derive(Debug)]
pub struct SlotLease {
    index: u16,
    // Held only for its lock; the lock is released when the file is closed.
    _file: File,
}

/// Why a slot could not be acquired.
#[derive(Debug)]
pub enum SlotError {
    /// Every slot is held.
    Full,
    /// A slot file is missing in [`SlotMode::Provisioned`].
    Missing(PathBuf),
    /// Any other I/O failure.
    Io(PathBuf, io::Error),
}

impl std::fmt::Display for SlotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SlotError::Full => write!(f, "all worker slots are in use"),
            SlotError::Missing(path) => {
                write!(
                    f,
                    "slot file {} is missing; it must be provisioned",
                    path.display()
                )
            }
            SlotError::Io(path, _) => write!(f, "cannot use slot file {}", path.display()),
        }
    }
}

impl std::error::Error for SlotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SlotError::Io(_, error) => Some(error),
            _ => None,
        }
    }
}

impl SlotPool {
    pub fn new(dir: impl Into<PathBuf>, limit: NonZeroU16, mode: SlotMode) -> Self {
        Self {
            dir: dir.into(),
            limit,
            mode,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn limit(&self) -> NonZeroU16 {
        self.limit
    }

    /// Path of the slot file with the given index.
    pub fn slot_path(&self, index: u16) -> PathBuf {
        self.dir.join(format!("slot-{index:03}.lock"))
    }

    /// Tries to take a free slot without waiting.
    ///
    /// Returns [`SlotError::Full`] immediately when every slot is held.
    pub fn try_acquire(&self) -> Result<SlotLease, SlotError> {
        if self.mode == SlotMode::CreateMissing {
            self.create_dir()?;
        }
        for index in 0..self.limit.get() {
            let path = self.slot_path(index);
            let file = self.open_slot(&path)?;
            match file.try_lock() {
                Ok(()) => return Ok(SlotLease { index, _file: file }),
                Err(TryLockError::WouldBlock) => continue,
                Err(TryLockError::Error(error)) => return Err(SlotError::Io(path, error)),
            }
        }
        Err(SlotError::Full)
    }

    fn create_dir(&self) -> Result<(), SlotError> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&self.dir)
            .map_err(|error| SlotError::Io(self.dir.clone(), error))
    }

    fn open_slot(&self, path: &Path) -> Result<File, SlotError> {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        match self.mode {
            SlotMode::Provisioned => options.open(path).map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    SlotError::Missing(path.to_path_buf())
                } else {
                    SlotError::Io(path.to_path_buf(), error)
                }
            }),
            SlotMode::CreateMissing => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                options
                    .create(true)
                    .truncate(false)
                    .open(path)
                    .map_err(|error| SlotError::Io(path.to_path_buf(), error))
            }
        }
    }
}

impl SlotLease {
    pub fn index(&self) -> u16 {
        self.index
    }
}
