use std::{
    error::Error,
    fmt,
    fs::Metadata,
    io,
    ops::AsyncFnOnce,
    os::windows::io::OwnedHandle,
    path::{Path, PathBuf},
};

use bitflags::bitflags;

use super::win32::{
    self, RawOperationKind, RawOplockBreak, RawOplockFile, RawRuntime, RawStartError, RawStartStage,
};

bitflags! {
    /// Windows sharing permissions used when a path is opened.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct ShareMode: u32 {
        /// Permit other handles to read the target.
        const READ = 0x0000_0001;
        /// Permit other handles to write the target.
        const WRITE = 0x0000_0002;
        /// Permit other handles to delete or rename the target.
        const DELETE = 0x0000_0004;
    }
}

impl Default for ShareMode {
    fn default() -> Self {
        Self::all()
    }
}

bitflags! {
    /// Flags returned with an oplock break notification.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct OplockBreakFlags: u32 {
        /// Windows requires an acknowledgement before the break completes.
        const ACK_REQUIRED = 0x0000_0001;
        /// The output contains access and sharing modes for the breaking open.
        const MODES_PROVIDED = 0x0000_0002;
        /// A writable mapped section prevented Windows from granting the requested level.
        const WRITABLE_SECTION_PRESENT = 0x0000_0004;
    }
}

/// A modern Windows 7 oplock request level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OplockLevel {
    /// Cache reads (R).
    Read,
    /// Cache reads and handles (RH).
    ReadHandle,
    /// Cache reads and writes (RW).
    ReadWrite,
    /// Cache reads, writes, and handles (RWH).
    ReadWriteHandle,
}

impl OplockLevel {
    pub(super) const fn bits(self) -> u32 {
        match self {
            Self::Read => win32::CACHE_READ,
            Self::ReadHandle => win32::CACHE_READ | win32::CACHE_HANDLE,
            Self::ReadWrite => win32::CACHE_READ | win32::CACHE_WRITE,
            Self::ReadWriteHandle => win32::CACHE_READ | win32::CACHE_WRITE | win32::CACHE_HANDLE,
        }
    }

    pub(super) const fn permits_writes(self) -> bool {
        matches!(self, Self::ReadWrite | Self::ReadWriteHandle)
    }

    fn from_bits(bits: u32) -> Option<Self> {
        match bits {
            win32::CACHE_READ => Some(Self::Read),
            value if value == win32::CACHE_READ | win32::CACHE_HANDLE => Some(Self::ReadHandle),
            value if value == win32::CACHE_READ | win32::CACHE_WRITE => Some(Self::ReadWrite),
            value if value == win32::CACHE_READ | win32::CACHE_WRITE | win32::CACHE_HANDLE => {
                Some(Self::ReadWriteHandle)
            }
            _ => None,
        }
    }
}

/// The kind of filesystem object being opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OplockTarget {
    /// A regular file.
    File,
    /// A directory. Windows only permits R and RH oplocks on directories.
    Directory,
}

/// Options for opening a target and requesting its oplock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OplockOptions {
    target: OplockTarget,
    level: OplockLevel,
    share_mode: ShareMode,
}

impl OplockOptions {
    /// Creates options for a target and oplock level.
    pub const fn new(target: OplockTarget, level: OplockLevel) -> Self {
        Self {
            target,
            level,
            share_mode: ShareMode::all(),
        }
    }

    /// Replaces the Windows sharing mode used by path-based opens.
    pub const fn with_share_mode(mut self, share_mode: ShareMode) -> Self {
        self.share_mode = share_mode;
        self
    }

    /// Returns the target kind.
    pub const fn target(self) -> OplockTarget {
        self.target
    }

    /// Returns the requested oplock level.
    pub const fn level(self) -> OplockLevel {
        self.level
    }

    /// Returns the path-open sharing mode.
    pub const fn share_mode(self) -> ShareMode {
        self.share_mode
    }

    fn validate(self) -> Result<(), OplockError> {
        if self.share_mode.bits() & !ShareMode::all().bits() != 0 {
            return Err(OplockError::InvalidOptions {
                reason: "share mode contains unsupported Windows flags",
            });
        }
        if self.target == OplockTarget::Directory && self.level.permits_writes() {
            return Err(OplockError::InvalidOptions {
                reason: "directories support only R and RH oplocks",
            });
        }
        Ok(())
    }
}

/// Identifies an operation in a structured error or shutdown issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperationKind {
    /// Request or wait for an oplock.
    Oplock,
    /// Read file data.
    Read,
    /// Write file data.
    Write,
    /// Read file metadata.
    Metadata,
    /// Flush file data.
    SyncData,
    /// Flush file data and metadata.
    SyncAll,
    /// Run or stop the completion dispatcher.
    Dispatcher,
    /// Close, cancel, or otherwise manage a Windows handle.
    Handle,
}

/// An error produced by an oplock runtime or protected operation.
#[derive(Debug)]
pub enum OplockError {
    /// The requested option combination is not supported by Windows.
    InvalidOptions {
        /// A stable explanation suitable for logs.
        reason: &'static str,
    },
    /// The runtime is shutting down and accepts no new work.
    RuntimeClosed,
    /// Opening a path failed.
    Open {
        /// The Windows or standard-library error.
        source: io::Error,
    },
    /// Associating the handle with the completion port failed.
    Associate {
        /// The Windows error.
        source: io::Error,
    },
    /// Submitting the oplock request failed.
    Request {
        /// The Windows error.
        source: io::Error,
    },
    /// A protected file operation failed.
    Operation {
        /// The operation that failed.
        kind: OperationKind,
        /// The Windows or standard-library error.
        source: io::Error,
    },
    /// Cancelling an in-flight operation failed.
    Cancel {
        /// The operation being cancelled.
        kind: OperationKind,
        /// The Windows error.
        source: io::Error,
    },
    /// Closing the protected handle failed.
    Close {
        /// The Windows error.
        source: io::Error,
    },
    /// The completion dispatcher failed.
    Dispatcher {
        /// The dispatcher error.
        source: io::Error,
    },
    /// An unclassified I/O failure.
    Other {
        /// The underlying error.
        source: io::Error,
    },
}

impl fmt::Display for OplockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOptions { reason } => {
                write!(formatter, "invalid oplock options: {reason}")
            }
            Self::RuntimeClosed => formatter.write_str("oplock runtime is shutting down"),
            Self::Open { source } => write!(formatter, "failed to open oplock target: {source}"),
            Self::Associate { source } => {
                write!(formatter, "failed to associate oplock handle: {source}")
            }
            Self::Request { source } => write!(formatter, "failed to request oplock: {source}"),
            Self::Operation { kind, source } => {
                write!(formatter, "{kind:?} operation failed: {source}")
            }
            Self::Cancel { kind, source } => {
                write!(formatter, "failed to cancel {kind:?}: {source}")
            }
            Self::Close { source } => write!(formatter, "failed to close oplock handle: {source}"),
            Self::Dispatcher { source } => write!(formatter, "oplock dispatcher failed: {source}"),
            Self::Other { source } => source.fmt(formatter),
        }
    }
}

impl Error for OplockError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidOptions { .. } | Self::RuntimeClosed => None,
            Self::Open { source }
            | Self::Associate { source }
            | Self::Request { source }
            | Self::Operation { source, .. }
            | Self::Cancel { source, .. }
            | Self::Close { source }
            | Self::Dispatcher { source }
            | Self::Other { source } => Some(source),
        }
    }
}

impl From<io::Error> for OplockError {
    fn from(source: io::Error) -> Self {
        Self::Other { source }
    }
}

impl From<OplockError> for io::Error {
    fn from(error: OplockError) -> Self {
        Self::other(error)
    }
}

/// A non-fatal problem observed while draining a runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeIssue {
    /// The operation that produced the issue.
    pub operation: OperationKind,
    /// The Windows error code, when one was available.
    pub raw_os_error: Option<i32>,
    /// A human-readable copy of the error.
    pub message: String,
}

/// Errors collected while explicitly shutting down a runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownError {
    issues: Vec<RuntimeIssue>,
}

impl ShutdownError {
    /// Returns all issues observed while cancelling, closing, and joining.
    pub fn issues(&self) -> &[RuntimeIssue] {
        &self.issues
    }
}

impl fmt::Display for ShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "oplock runtime shutdown reported {} issue(s)",
            self.issues.len()
        )
    }
}

impl Error for ShutdownError {}

impl From<ShutdownError> for io::Error {
    fn from(error: ShutdownError) -> Self {
        Self::other(error)
    }
}

/// Details reported when Windows breaks an oplock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OplockBreak {
    /// Raw original-level bits returned by Windows.
    pub original_level_raw: u32,
    /// The level originally granted by Windows, if recognized.
    pub original_level: Option<OplockLevel>,
    /// Raw replacement-level bits returned by Windows.
    pub new_level_raw: u32,
    /// The level retained after the break, if recognized. `None` also covers no caching.
    pub new_level: Option<OplockLevel>,
    /// Flags returned by `FSCTL_REQUEST_OPLOCK`.
    pub flags: OplockBreakFlags,
    /// Whether Windows requested an acknowledgement.
    pub ack_required: bool,
    /// Desired access of the breaking open, when Windows supplied it.
    pub access_mode: Option<u32>,
    /// Sharing mode of the breaking open, when Windows supplied it.
    pub share_mode: Option<ShareMode>,
}

/// The result shape returned by owned-buffer read and write operations.
pub type BufferResult = (Result<usize, OplockError>, Vec<u8>);

/// The sole capability for operating on a protected handle.
///
/// The type is not cloneable. Data I/O uses owned buffers so cancellation
/// cannot outlive caller-provided memory. Use
/// [`try_clone_handle`](Self::try_clone_handle) when another component needs an
/// independently owned Windows handle and can uphold its safety contract.
pub struct OplockFile {
    inner: RawOplockFile,
}

impl fmt::Debug for OplockFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("OplockFile").finish_non_exhaustive()
    }
}

impl OplockFile {
    /// Duplicates the protected handle into the current process.
    ///
    /// The returned handle has the same access rights as the protected handle
    /// and is owned by the caller. It is not tracked or closed by the oplock
    /// runtime. It can be inherited by a child process or duplicated into
    /// another process with the Windows `DuplicateHandle` API.
    ///
    /// A duplicate refers to the same underlying file object. Keeping it open,
    /// including in another process, extends the lifetime of that object and
    /// its completion-port association beyond this callback and any
    /// [`OplockBreakGuard`].
    ///
    /// # Safety
    ///
    /// The protected handle is associated with this runtime's I/O completion
    /// port, and that association is shared by duplicates. The caller must
    /// ensure that operations through a duplicate cannot enqueue completion
    /// packets that were not created by this crate. It is sound to transfer,
    /// retain, and close the duplicate without issuing I/O through it.
    pub unsafe fn try_clone_handle(&self) -> Result<OwnedHandle, OplockError> {
        self.inner
            .try_clone_handle()
            .map_err(|source| OplockError::Operation {
                kind: OperationKind::Handle,
                source,
            })
    }

    /// Reads metadata from the protected handle.
    pub async fn metadata(&self) -> Result<Metadata, OplockError> {
        self.inner
            .metadata()
            .await
            .map_err(|source| OplockError::Operation {
                kind: OperationKind::Metadata,
                source,
            })
    }

    /// Reads at `offset`, returning ownership of `buffer` with the operation result.
    pub async fn read_at(&self, buffer: Vec<u8>, offset: u64) -> BufferResult {
        let (result, buffer) = self.inner.read_at(buffer, offset).await;
        (
            result.map_err(|source| OplockError::Operation {
                kind: OperationKind::Read,
                source,
            }),
            buffer,
        )
    }

    /// Writes `buffer` at `offset`, returning it with the operation result.
    ///
    /// Writes require an RW or RWH oplock.
    pub async fn write_at(&self, buffer: Vec<u8>, offset: u64) -> BufferResult {
        let (result, buffer) = self.inner.write_at(buffer, offset).await;
        (
            result.map_err(|source| OplockError::Operation {
                kind: OperationKind::Write,
                source,
            }),
            buffer,
        )
    }

    /// Flushes file data to the storage device.
    pub async fn sync_data(&self) -> Result<(), OplockError> {
        self.inner
            .sync_data()
            .await
            .map_err(|source| OplockError::Operation {
                kind: OperationKind::SyncData,
                source,
            })
    }

    /// Flushes file data and metadata to the storage device.
    pub async fn sync_all(&self) -> Result<(), OplockError> {
        self.inner
            .sync_all()
            .await
            .map_err(|source| OplockError::Operation {
                kind: OperationKind::SyncAll,
                source,
            })
    }

    async fn close(self) -> Result<(), OplockError> {
        self.inner
            .close()
            .await
            .map_err(|source| OplockError::Close { source })
    }
}

/// Ownership of a broken oplock's still-open handle.
///
/// Dropping the guard starts best-effort cancellation and close. Prefer
/// [`close`](Self::close) when shutdown errors must be observed.
pub struct OplockBreakGuard {
    file: Option<OplockFile>,
}

impl fmt::Debug for OplockBreakGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OplockBreakGuard")
            .finish_non_exhaustive()
    }
}

impl OplockBreakGuard {
    /// Cancels pending operations, closes the handle, and waits for completion.
    pub async fn close(mut self) -> Result<(), OplockError> {
        self.file
            .take()
            .expect("oplock break guard is open")
            .close()
            .await
    }
}

/// The outcome of racing protected work against an oplock break.
#[derive(Debug)]
pub enum OplockOutcome<T> {
    /// The work completed before any oplock break.
    Completed(T),
    /// Windows broke the oplock before the work completed.
    Broken {
        /// Information returned by Windows.
        info: OplockBreak,
        /// The close-only guard retaining the protected handle.
        guard: OplockBreakGuard,
    },
}

/// An independent IOCP-backed oplock runtime.
///
/// Clones refer to the same runtime. Independent calls to [`new`](Self::new)
/// create independent completion ports and dispatcher threads.
#[derive(Clone)]
pub struct OplockRuntime {
    inner: RawRuntime,
}

impl fmt::Debug for OplockRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OplockRuntime")
            .finish_non_exhaustive()
    }
}

impl OplockRuntime {
    /// Creates an independent runtime and its completion dispatcher.
    pub fn new() -> Result<Self, OplockError> {
        RawRuntime::new()
            .map(|inner| Self { inner })
            .map_err(|source| OplockError::Dispatcher { source })
    }

    /// Atomically opens `path` with `FILE_FLAG_OPEN_REQUIRING_OPLOCK`, requests
    /// `options.level()`, and races `work` against the break notification.
    pub async fn run<T, F, E>(
        &self,
        path: impl AsRef<Path>,
        options: OplockOptions,
        work: F,
    ) -> Result<OplockOutcome<T>, OplockError>
    where
        F: for<'a> AsyncFnOnce(&'a OplockFile) -> Result<T, E>,
        OplockError: From<E>,
    {
        options.validate()?;
        let path = PathBuf::from(path.as_ref());
        let file = tokio::task::spawn_blocking(move || win32::open_path(&path, options))
            .await
            .map_err(|join| OplockError::Open {
                source: io::Error::other(format!("path-open task failed: {join}")),
            })?
            .map_err(|source| OplockError::Open { source })?;
        self.run_opened(file, options, work).await
    }

    /// Takes ownership of an already-open Windows file and requests an oplock.
    ///
    /// The file must have been opened for overlapped I/O. Path-based [`run`](Self::run)
    /// is preferred because it uses `FILE_FLAG_OPEN_REQUIRING_OPLOCK` and avoids
    /// a race between opening the handle and requesting the oplock.
    pub async fn run_file<T, F, E>(
        &self,
        file: std::fs::File,
        options: OplockOptions,
        work: F,
    ) -> Result<OplockOutcome<T>, OplockError>
    where
        F: for<'a> AsyncFnOnce(&'a OplockFile) -> Result<T, E>,
        OplockError: From<E>,
    {
        options.validate()?;
        self.run_opened(file, options, work).await
    }

    /// Stops accepting new work, cancels and closes all sessions, drains the
    /// completion port, and joins the dispatcher thread.
    pub async fn shutdown(self) -> Result<(), ShutdownError> {
        let issues = self.inner.shutdown().await;
        if issues.is_empty() {
            Ok(())
        } else {
            Err(ShutdownError {
                issues: issues
                    .into_iter()
                    .map(|issue| RuntimeIssue {
                        operation: map_kind(issue.operation),
                        raw_os_error: issue.raw_os_error,
                        message: issue.message,
                    })
                    .collect(),
            })
        }
    }

    async fn run_opened<T, F, E>(
        &self,
        file: std::fs::File,
        options: OplockOptions,
        work: F,
    ) -> Result<OplockOutcome<T>, OplockError>
    where
        F: for<'a> AsyncFnOnce(&'a OplockFile) -> Result<T, E>,
        OplockError: From<E>,
    {
        let (raw_file, mut request) = self
            .inner
            .request(file, options.level.bits(), options.level.permits_writes())
            .map_err(map_start_error)?;
        let file = OplockFile { inner: raw_file };
        let mut work = Box::pin(work(&file));

        enum Race<T, E> {
            Work(Result<T, E>),
            Break(io::Result<RawOplockBreak>),
        }

        let race = tokio::select! {
            biased;
            broken = request.wait() => Race::Break(broken),
            result = &mut work => Race::Work(result),
        };
        drop(work);

        match race {
            Race::Work(result) => {
                request
                    .cancel_and_wait()
                    .await
                    .map_err(|source| OplockError::Cancel {
                        kind: OperationKind::Oplock,
                        source,
                    })?;
                file.close().await?;
                result
                    .map(OplockOutcome::Completed)
                    .map_err(OplockError::from)
            }
            Race::Break(Ok(info)) => Ok(OplockOutcome::Broken {
                info: map_break(info),
                guard: OplockBreakGuard { file: Some(file) },
            }),
            Race::Break(Err(source)) => {
                let close = file.close().await;
                close?;
                Err(OplockError::Operation {
                    kind: OperationKind::Oplock,
                    source,
                })
            }
        }
    }
}

fn map_start_error(error: RawStartError) -> OplockError {
    match error.stage {
        RawStartStage::Runtime => OplockError::RuntimeClosed,
        RawStartStage::Associate => OplockError::Associate {
            source: error.source,
        },
        RawStartStage::Request => OplockError::Request {
            source: error.source,
        },
    }
}

fn map_break(info: RawOplockBreak) -> OplockBreak {
    let flags = OplockBreakFlags::from_bits_retain(info.flags);
    let modes_provided = flags.contains(OplockBreakFlags::MODES_PROVIDED);
    OplockBreak {
        original_level_raw: info.original_level,
        original_level: OplockLevel::from_bits(info.original_level),
        new_level_raw: info.new_level,
        new_level: OplockLevel::from_bits(info.new_level),
        flags,
        ack_required: info.ack_required,
        access_mode: modes_provided.then_some(info.access_mode),
        share_mode: modes_provided.then_some(ShareMode::from_bits_retain(info.share_mode)),
    }
}

fn map_kind(kind: RawOperationKind) -> OperationKind {
    match kind {
        RawOperationKind::Oplock => OperationKind::Oplock,
        RawOperationKind::Read => OperationKind::Read,
        RawOperationKind::Write => OperationKind::Write,
        RawOperationKind::Dispatcher => OperationKind::Dispatcher,
        RawOperationKind::Handle => OperationKind::Handle,
    }
}
