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
    /// The most slots one uid may hold at once, when fairness is configured.
    ///
    /// The host limit stops the machine being overloaded; it does nothing about
    /// *whose* work is on it. One developer's loop could take every slot and
    /// everyone else would see `Full` until it finished, which is the gap
    /// Codex's review asked about.
    per_user: Option<NonZeroU16>,
}

/// A held slot. Dropping it releases the slot.
#[derive(Debug)]
pub struct SlotLease {
    index: u16,
    // Held for its lock. Dropping unlocks explicitly; see `Drop`.
    file: File,
    /// The caller's own per-user slot, held for exactly as long as the host one.
    /// Taken first, so a caller at their own cap never occupies a host slot
    /// while being turned away.
    user: Option<(u16, File)>,
}

/// Why a slot could not be acquired.
#[derive(Debug)]
pub enum SlotError {
    /// Every slot is held.
    Full,
    /// The caller already holds as many slots as one uid may hold. Distinct
    /// from `Full` because the two mean different things to whoever reads them:
    /// the host is busy, or you are.
    YoursFull { uid: u32, cap: u16 },
    /// A slot file is missing in [`SlotMode::Provisioned`].
    Missing(PathBuf),
    /// Any other I/O failure.
    Io(PathBuf, io::Error),
}

impl std::fmt::Display for SlotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SlotError::Full => write!(f, "all worker slots are in use"),
            SlotError::YoursFull { uid, cap } => write!(
                f,
                "uid {uid} already holds {cap} worker slot(s), which is the limit for \
                 one user on this host; other slots may be free"
            ),
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
            per_user: None,
        }
    }

    /// Caps how many slots any one uid may hold at once.
    ///
    /// A cap at or above the host limit changes nothing, and is allowed rather
    /// than rejected: an operator sizing a host should not have to reason about
    /// which of two numbers binds.
    pub fn per_user(mut self, cap: NonZeroU16) -> Self {
        self.per_user = Some(cap);
        self
    }

    pub fn per_user_limit(&self) -> Option<NonZeroU16> {
        self.per_user
    }

    /// Where one uid's own slots live.
    pub fn user_dir(&self, uid: u32) -> PathBuf {
        self.dir.join(format!("u{uid}"))
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
    /// Returns [`SlotError::Full`] immediately when every slot is held, and
    /// [`SlotError::YoursFull`] when the caller is already at their own cap.
    pub fn try_acquire(&self) -> Result<SlotLease, SlotError> {
        self.try_acquire_as(current_uid())
    }

    /// The same, for a stated uid. Separated so fairness can be tested without
    /// running as several users.
    pub fn try_acquire_as(&self, uid: u32) -> Result<SlotLease, SlotError> {
        if self.mode == SlotMode::CreateMissing {
            self.create_dir()?;
        }
        // The caller's own slot first. Taking a host slot and then discovering
        // the caller is at their cap would occupy a slot for the length of the
        // refusal, which is the opposite of fairness.
        let user = match self.per_user {
            Some(cap) => Some(self.take_user_slot(uid, cap)?),
            None => None,
        };
        for index in 0..self.limit.get() {
            let path = self.slot_path(index);
            let file = self.open_slot(&path)?;
            match file.try_lock() {
                Ok(()) => return Ok(SlotLease { index, file, user }),
                Err(TryLockError::WouldBlock) => continue,
                Err(TryLockError::Error(error)) => return Err(SlotError::Io(path, error)),
            }
        }
        Err(SlotError::Full)
    }

    /// Takes one of a single uid's own slots.
    ///
    /// The same file-lock primitive as the host pool, in a directory of the
    /// caller's own, which is what makes the count exact without any process
    /// having to read another's state: the locks *are* the accounting.
    ///
    /// The directory is created on demand even on a provisioned host. An
    /// operator cannot provision directories for uids they do not know yet, and
    /// a user creating the files that limit only themselves is not a privilege.
    /// **The honest consequence: a user who deletes their own cap files can
    /// exceed their cap.** They still cannot exceed the host limit, which is
    /// provisioned and not theirs to touch. This is fairness against runaway
    /// work, not a boundary against someone determined to take more.
    fn take_user_slot(&self, uid: u32, cap: NonZeroU16) -> Result<(u16, File), SlotError> {
        let dir = self.user_dir(uid);
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        if !dir.exists() {
            builder
                .create(&dir)
                .map_err(|error| SlotError::Io(dir.clone(), error))?;
        }
        let mine = SlotPool::new(&dir, cap, SlotMode::CreateMissing);
        for index in 0..cap.get() {
            let path = mine.slot_path(index);
            let file = mine.open_slot(&path)?;
            match file.try_lock() {
                Ok(()) => return Ok((index, file)),
                Err(TryLockError::WouldBlock) => continue,
                Err(TryLockError::Error(error)) => return Err(SlotError::Io(path, error)),
            }
        }
        Err(SlotError::YoursFull {
            uid,
            cap: cap.get(),
        })
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

/// The uid this process runs as.
fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: getuid cannot fail and touches no memory we own.
        unsafe { libc::getuid() }
    }
}

impl SlotLease {
    pub fn index(&self) -> u16 {
        self.index
    }

    /// Which of the holder's own slots this lease occupies, when a per-user cap
    /// is configured.
    pub fn user_index(&self) -> Option<u16> {
        self.user.as_ref().map(|(index, _)| *index)
    }

    /// The locked slot file. Exposed so tests can duplicate the descriptor and
    /// reproduce the shared-open-file-description case.
    #[doc(hidden)]
    pub fn file_for_test(&self) -> &File {
        &self.file
    }
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        // Release explicitly rather than relying on close(2).
        //
        // The lock belongs to the open file description, not to our descriptor.
        // A child forked by another thread while this slot was held shares that
        // description until it execs and FD_CLOEXEC closes the copy. For as long
        // as such a child exists, closing our own descriptor leaves the lock in
        // place and the next caller sees a free slot as busy, so `try_acquire`
        // reports `Full` while capacity is free. Timon forks and reaps children
        // continuously, so that window is the normal case, not a rare one.
        //
        // Unlocking the description first releases the lock even when a copy of
        // the descriptor is still open elsewhere. Errors are not actionable here:
        // the descriptor is closed immediately afterwards either way.
        #[cfg(unix)]
        unsafe {
            libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.file), libc::LOCK_UN);
            // The per-user slot goes back at the same moment, for the same
            // reason. Releasing one and not the other would let a caller sit at
            // their cap holding nothing.
            if let Some((_, file)) = &self.user {
                libc::flock(std::os::fd::AsRawFd::as_raw_fd(file), libc::LOCK_UN);
            }
        }
    }
}

/// One slot file after provisioning.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ProvisionedSlot {
    pub path: PathBuf,
    /// Identifies the file itself. It must not change once anyone can hold a
    /// lock on it: replacing the file would let a second holder lock the new
    /// inode while the first still holds the old one, and the limit would be
    /// quietly doubled.
    pub inode: u64,
    /// This run created it. A second run reports `false` for the same slot.
    pub created: bool,
    pub mode: u32,
    pub gid: u32,
}

/// What an operator provisioned.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ProvisionReport {
    pub dir: PathBuf,
    pub dir_mode: u32,
    pub dir_gid: u32,
    pub slots: Vec<ProvisionedSlot>,
    /// Slot files present beyond the requested count. They are left alone:
    /// removing one while a launcher holds its lock would release a slot that is
    /// still in use.
    pub extra_left_in_place: Vec<PathBuf>,
}

/// Creates the shared slot directory and lock files for a host.
///
/// Run once by an operator, as root. Every official launcher on the host then
/// points at this directory in [`SlotMode::Provisioned`], so one limit covers
/// every account instead of each getting its own.
///
/// Idempotent, and deliberately conservative about what it touches: a missing
/// file is created, an existing one is left exactly as it is. Re-provisioning
/// must not disturb a file somebody is holding, which rules out truncating,
/// replacing or re-creating, and is why this does not reuse the per-user
/// initialisation that does create files on demand.
///
/// The directory is not group-writable, so members can open and lock the files
/// but cannot unlink, replace or add slots.
pub fn provision(
    dir: &Path,
    slots: NonZeroU16,
    group: Option<&str>,
    dir_mode: u32,
    file_mode: u32,
) -> Result<ProvisionReport, SlotError> {
    let gid = match group {
        Some(name) => Some(group_id(name)?),
        None => None,
    };

    if !dir.exists() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(dir_mode);
        }
        builder
            .create(dir)
            .map_err(|error| SlotError::Io(dir.to_path_buf(), error))?;
    }
    set_mode(dir, dir_mode)?;
    if let Some(gid) = gid {
        set_group(dir, gid)?;
    }

    let pool = SlotPool::new(dir, slots, SlotMode::Provisioned);
    let mut provisioned = Vec::new();
    for index in 0..slots.get() {
        let path = pool.slot_path(index);
        let created = !path.exists();
        if created {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(file_mode);
            }
            options
                .open(&path)
                .map_err(|error| SlotError::Io(path.clone(), error))?;
        }
        // Mode and group are corrected either way, so a half-finished earlier run
        // does not leave a slot nobody can lock. The file itself is untouched.
        set_mode(&path, file_mode)?;
        if let Some(gid) = gid {
            set_group(&path, gid)?;
        }
        let meta = std::fs::metadata(&path).map_err(|error| SlotError::Io(path.clone(), error))?;
        provisioned.push(ProvisionedSlot {
            path,
            inode: inode_of(&meta),
            created,
            mode: mode_of(&meta),
            gid: gid_of(&meta),
        });
    }

    // A shrink is not applied. Whoever asked for fewer slots can remove the
    // files deliberately when nothing is running; doing it here would release a
    // slot out from under a live launcher.
    let mut extra = Vec::new();
    for index in slots.get()..=u16::MAX {
        let path = pool.slot_path(index);
        if !path.exists() {
            break;
        }
        extra.push(path);
    }

    let dir_meta =
        std::fs::metadata(dir).map_err(|error| SlotError::Io(dir.to_path_buf(), error))?;
    Ok(ProvisionReport {
        dir: dir.to_path_buf(),
        dir_mode: mode_of(&dir_meta),
        dir_gid: gid_of(&dir_meta),
        slots: provisioned,
        extra_left_in_place: extra,
    })
}

fn set_mode(path: &Path, mode: u32) -> Result<(), SlotError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|error| SlotError::Io(path.to_path_buf(), error))?;
    }
    Ok(())
}

/// Sets the group, leaving the owner alone.
fn set_group(path: &Path, gid: u32) -> Result<(), SlotError> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| SlotError::Io(path.to_path_buf(), io::Error::other("path contains a NUL")))?;
    // SAFETY: the path is a valid NUL-terminated C string for the duration of
    // the call. `-1` as a uid means "leave the owner unchanged".
    let result = unsafe { libc::chown(c_path.as_ptr(), u32::MAX, gid) };
    if result != 0 {
        return Err(SlotError::Io(
            path.to_path_buf(),
            io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn group_id(name: &str) -> Result<u32, SlotError> {
    let c_name = std::ffi::CString::new(name).map_err(|_| {
        SlotError::Io(
            PathBuf::from(name),
            io::Error::other("group name contains a NUL"),
        )
    })?;
    // SAFETY: getgrnam returns a pointer into library-owned storage valid until
    // the next call from this thread; the gid is copied out immediately.
    let gid = unsafe {
        let entry = libc::getgrnam(c_name.as_ptr());
        if entry.is_null() {
            return Err(SlotError::Io(
                PathBuf::from(name),
                io::Error::other(format!("no such group: {name}")),
            ));
        }
        (*entry).gr_gid
    };
    Ok(gid)
}

fn inode_of(meta: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        std::os::unix::fs::MetadataExt::ino(meta)
    }
}

fn mode_of(meta: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o7777
    }
}

fn gid_of(meta: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        std::os::unix::fs::MetadataExt::gid(meta)
    }
}
