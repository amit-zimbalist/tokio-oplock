use std::{
    collections::HashMap,
    fs::{File, Metadata, OpenOptions},
    io,
    mem::{size_of, zeroed},
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsHandle, AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle},
    },
    path::Path,
    ptr::null_mut,
    sync::{
        Arc, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
};

use tokio::sync::{Notify, oneshot};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_IO_PENDING, ERROR_NOT_FOUND, ERROR_OPERATION_ABORTED, HANDLE,
        INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
    },
    Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, ReadFile, WriteFile},
    System::{
        IO::{
            CancelIoEx, CreateIoCompletionPort, DeviceIoControl, GetQueuedCompletionStatus,
            OVERLAPPED, PostQueuedCompletionStatus,
        },
        Ioctl::{
            FSCTL_REQUEST_OPLOCK, OPLOCK_LEVEL_CACHE_HANDLE, OPLOCK_LEVEL_CACHE_READ,
            OPLOCK_LEVEL_CACHE_WRITE, REQUEST_OPLOCK_CURRENT_VERSION, REQUEST_OPLOCK_INPUT_BUFFER,
            REQUEST_OPLOCK_INPUT_FLAG_REQUEST, REQUEST_OPLOCK_OUTPUT_BUFFER,
            REQUEST_OPLOCK_OUTPUT_FLAG_ACK_REQUIRED,
        },
        WindowsProgramming::FILE_FLAG_OPEN_REQUIRING_OPLOCK,
    },
};

use super::request::{OplockOptions, OplockTarget};

pub(super) const CACHE_READ: u32 = OPLOCK_LEVEL_CACHE_READ;
pub(super) const CACHE_HANDLE: u32 = OPLOCK_LEVEL_CACHE_HANDLE;
pub(super) const CACHE_WRITE: u32 = OPLOCK_LEVEL_CACHE_WRITE;

const SHUTDOWN_KEY: usize = usize::MAX;
const COMPLETION_POLL_MS: u32 = 1_000;

#[derive(Debug, Clone, Copy)]
pub(super) enum RawOperationKind {
    Oplock,
    Read,
    Write,
    Dispatcher,
    Handle,
}

#[derive(Debug, Clone)]
pub(super) struct RawRuntimeIssue {
    pub(super) operation: RawOperationKind,
    pub(super) raw_os_error: Option<i32>,
    pub(super) message: String,
}

impl RawRuntimeIssue {
    fn new(operation: RawOperationKind, error: &io::Error) -> Self {
        Self {
            operation,
            raw_os_error: error.raw_os_error(),
            message: error.to_string(),
        }
    }

    fn to_io_error(&self) -> io::Error {
        self.raw_os_error
            .map(io::Error::from_raw_os_error)
            .unwrap_or_else(|| io::Error::other(self.message.clone()))
    }
}

pub(super) struct RawOplockBreak {
    pub(super) original_level: u32,
    pub(super) new_level: u32,
    pub(super) flags: u32,
    pub(super) ack_required: bool,
    pub(super) access_mode: u32,
    pub(super) share_mode: u32,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum RawStartStage {
    Runtime,
    Associate,
    Request,
}

#[derive(Debug)]
pub(super) struct RawStartError {
    pub(super) stage: RawStartStage,
    pub(super) source: io::Error,
}

struct CompletionPort {
    handle: OwnedHandle,
    shutdown_posted: AtomicBool,
}

impl CompletionPort {
    fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle() as HANDLE
    }

    fn request_shutdown(&self) -> io::Result<()> {
        if self.shutdown_posted.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        // SAFETY: the IOCP is live and the reserved key identifies our sentinel.
        let posted = unsafe { PostQueuedCompletionStatus(self.raw(), 0, SHUTDOWN_KEY, null_mut()) };
        if posted == 0 {
            self.shutdown_posted.store(false, Ordering::Release);
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

// SAFETY: completion-port handles support concurrent association, posting, and
// queue access. OwnedHandle closes the port after the final Arc is released.
unsafe impl Send for CompletionPort {}
unsafe impl Sync for CompletionPort {}

struct RuntimeState {
    accepting: bool,
    shutdown_started: bool,
    worker_stopped: bool,
    active_operations: usize,
    sessions: HashMap<u64, Weak<FileState>>,
    issues: Vec<RawRuntimeIssue>,
}

struct Dispatcher {
    port: Arc<CompletionPort>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    state: Mutex<RuntimeState>,
    stopped: Notify,
    next_session: AtomicU64,
}

// SAFETY: the IOCP is concurrency-safe; all other shared dispatcher state is
// atomic, mutex-protected, or used only by the completion thread.
unsafe impl Send for Dispatcher {}
unsafe impl Sync for Dispatcher {}

struct RuntimeOwner {
    dispatcher: Arc<Dispatcher>,
}

impl Drop for RuntimeOwner {
    fn drop(&mut self) {
        self.dispatcher.initiate_shutdown();
    }
}

#[derive(Clone)]
pub(super) struct RawRuntime {
    owner: Arc<RuntimeOwner>,
}

impl RawRuntime {
    pub(super) fn new() -> io::Result<Self> {
        // SAFETY: this documented argument combination creates a new IOCP.
        let raw_port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, null_mut(), 0, 1) };
        if raw_port.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateIoCompletionPort returned a fresh owned handle.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw_port as _) };
        let dispatcher = Arc::new(Dispatcher {
            port: Arc::new(CompletionPort {
                handle,
                shutdown_posted: AtomicBool::new(false),
            }),
            worker: Mutex::new(None),
            state: Mutex::new(RuntimeState {
                accepting: true,
                shutdown_started: false,
                worker_stopped: false,
                active_operations: 0,
                sessions: HashMap::new(),
                issues: Vec::new(),
            }),
            stopped: Notify::new(),
            next_session: AtomicU64::new(1),
        });

        let worker_dispatcher = Arc::clone(&dispatcher);
        let worker = thread::Builder::new()
            .name("tokio-oplock-iocp".into())
            .spawn(move || completion_loop(worker_dispatcher))?;
        *lock(&dispatcher.worker) = Some(worker);

        Ok(Self {
            owner: Arc::new(RuntimeOwner { dispatcher }),
        })
    }

    pub(super) fn request(
        &self,
        file: File,
        requested_level: u32,
        permits_writes: bool,
    ) -> Result<(RawOplockFile, RawOplockRequest), RawStartError> {
        let dispatcher = &self.owner.dispatcher;
        let raw_file = file.as_raw_handle() as HANDLE;
        // SAFETY: both handles are live. A file can be associated with one IOCP
        // once; association occurs before any operation is submitted.
        let associated = unsafe { CreateIoCompletionPort(raw_file, dispatcher.port.raw(), 0, 0) };
        if associated.is_null() {
            return Err(RawStartError {
                stage: RawStartStage::Associate,
                source: io::Error::last_os_error(),
            });
        }

        let id = dispatcher.next_session.fetch_add(1, Ordering::Relaxed);
        let state = Arc::new(FileState {
            id,
            dispatcher: Arc::clone(dispatcher),
            inner: Mutex::new(FileInner {
                file: Some(file),
                closing: false,
                pending: 0,
                close_issue: None,
            }),
            closed: Notify::new(),
            permits_writes,
        });

        {
            let mut runtime = lock(&dispatcher.state);
            if !runtime.accepting {
                drop(runtime);
                state.initiate_close();
                return Err(RawStartError {
                    stage: RawStartStage::Runtime,
                    source: runtime_closed_error(),
                });
            }
            runtime.sessions.insert(id, Arc::downgrade(&state));
        }

        let request = match submit_oplock(Arc::clone(&state), requested_level) {
            Ok(request) => request,
            Err(source) => {
                state.initiate_close();
                let stage = if source.kind() == io::ErrorKind::NotConnected {
                    RawStartStage::Runtime
                } else {
                    RawStartStage::Request
                };
                return Err(RawStartError { stage, source });
            }
        };

        Ok((RawOplockFile { state }, request))
    }

    pub(super) async fn shutdown(self) -> Vec<RawRuntimeIssue> {
        let dispatcher = Arc::clone(&self.owner.dispatcher);
        dispatcher.initiate_shutdown();
        dispatcher.wait_stopped().await;

        let worker = { lock(&dispatcher.worker).take() };
        if let Some(worker) = worker {
            match tokio::task::spawn_blocking(move || worker.join()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => dispatcher.record_message(
                    RawOperationKind::Dispatcher,
                    "completion dispatcher thread panicked",
                ),
                Err(error) => dispatcher.record_message(
                    RawOperationKind::Dispatcher,
                    format!("failed to join completion dispatcher: {error}"),
                ),
            }
        }

        lock(&dispatcher.state).issues.clone()
    }
}

impl Dispatcher {
    fn initiate_shutdown(&self) {
        let (sessions, post_now) = {
            let mut state = lock(&self.state);
            if !state.shutdown_started {
                state.accepting = false;
                state.shutdown_started = true;
            }
            let sessions = state
                .sessions
                .values()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>();
            (sessions, state.active_operations == 0)
        };

        for session in sessions {
            session.initiate_close();
        }
        if post_now {
            self.post_shutdown();
        }
    }

    fn post_shutdown(&self) {
        if let Err(error) = self.port.request_shutdown() {
            self.record_issue(RawOperationKind::Dispatcher, &error);
        }
    }

    async fn wait_stopped(&self) {
        loop {
            let notified = self.stopped.notified();
            if lock(&self.state).worker_stopped {
                return;
            }
            notified.await;
        }
    }

    fn record_issue(&self, operation: RawOperationKind, error: &io::Error) {
        lock(&self.state)
            .issues
            .push(RawRuntimeIssue::new(operation, error));
    }

    fn record_message(&self, operation: RawOperationKind, message: impl Into<String>) {
        lock(&self.state).issues.push(RawRuntimeIssue {
            operation,
            raw_os_error: None,
            message: message.into(),
        });
    }
}

struct FileInner {
    file: Option<File>,
    closing: bool,
    pending: usize,
    close_issue: Option<RawRuntimeIssue>,
}

struct FileState {
    id: u64,
    dispatcher: Arc<Dispatcher>,
    inner: Mutex<FileInner>,
    closed: Notify,
    permits_writes: bool,
}

impl FileState {
    fn initiate_close(&self) {
        let (file_to_close, notify) = {
            let mut runtime = lock(&self.dispatcher.state);
            let mut inner = lock(&self.inner);
            if inner.closing {
                return;
            }
            inner.closing = true;

            if inner.pending == 0 {
                runtime.sessions.remove(&self.id);
                (inner.file.take(), true)
            } else {
                let cancel_result = inner.file.as_ref().map(|file| {
                    // SAFETY: the handle remains owned under the file lock and
                    // pending operations keep it open until completion.
                    unsafe { CancelIoEx(file.as_raw_handle() as HANDLE, null_mut()) }
                });
                match cancel_result {
                    Some(0) => {
                        let error = io::Error::last_os_error();
                        if error.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
                            (None, false)
                        } else {
                            let issue = RawRuntimeIssue::new(RawOperationKind::Handle, &error);
                            runtime.issues.push(issue.clone());
                            inner.close_issue.get_or_insert(issue);
                            runtime.sessions.remove(&self.id);
                            (inner.file.take(), false)
                        }
                    }
                    _ => (None, false),
                }
            }
        };

        if let Some(file) = file_to_close {
            self.close_file(file);
        }
        if notify {
            self.closed.notify_waiters();
        }
    }

    fn close_file(&self, file: File) {
        if let Err(error) = close_owned_file(file) {
            let issue = RawRuntimeIssue::new(RawOperationKind::Handle, &error);
            lock(&self.dispatcher.state).issues.push(issue.clone());
            lock(&self.inner).close_issue.get_or_insert(issue);
        }
    }

    async fn close(self: Arc<Self>) -> io::Result<()> {
        self.initiate_close();
        loop {
            let notified = self.closed.notified();
            let issue = {
                let inner = lock(&self.inner);
                if inner.pending == 0 && inner.file.is_none() {
                    Some(inner.close_issue.clone())
                } else {
                    None
                }
            };
            if let Some(issue) = issue {
                return issue.map_or(Ok(()), |issue| Err(issue.to_io_error()));
            }
            notified.await;
        }
    }

    fn finish_operation(&self) {
        let (file_to_close, post_shutdown) = {
            let mut runtime = lock(&self.dispatcher.state);
            let mut inner = lock(&self.inner);
            debug_assert!(inner.pending > 0);
            debug_assert!(runtime.active_operations > 0);
            inner.pending -= 1;
            runtime.active_operations -= 1;

            let file = if inner.pending == 0 && inner.closing {
                runtime.sessions.remove(&self.id);
                inner.file.take()
            } else {
                None
            };
            let post = runtime.shutdown_started && runtime.active_operations == 0;
            (file, post)
        };

        if let Some(file) = file_to_close {
            self.close_file(file);
        }
        self.closed.notify_waiters();
        if post_shutdown {
            self.dispatcher.post_shutdown();
        }
    }

    fn cancel_specific(&self, cancellation: &Arc<Mutex<CancelState>>) -> io::Result<()> {
        let cancellation = lock(cancellation);
        if !cancellation.active {
            return Ok(());
        }
        let inner = lock(&self.inner);
        let Some(file) = inner.file.as_ref() else {
            return Ok(());
        };
        // SAFETY: the cancellation mutex guarantees that OVERLAPPED remains
        // allocated, and the file lock guarantees that the handle remains open.
        let cancelled =
            unsafe { CancelIoEx(file.as_raw_handle() as HANDLE, cancellation.overlapped) };
        if cancelled != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
            Ok(())
        } else {
            Err(error)
        }
    }

    fn begin_blocking(&self) -> io::Result<()> {
        let mut runtime = lock(&self.dispatcher.state);
        let mut inner = lock(&self.inner);
        if !runtime.accepting || inner.closing || inner.file.is_none() {
            return Err(runtime_closed_error());
        }
        runtime.active_operations += 1;
        inner.pending += 1;
        Ok(())
    }

    fn with_file<T>(&self, operation: impl FnOnce(&File) -> io::Result<T>) -> io::Result<T> {
        let inner = lock(&self.inner);
        let file = inner.file.as_ref().ok_or_else(runtime_closed_error)?;
        operation(file)
    }
}

pub(super) struct RawOplockFile {
    state: Arc<FileState>,
}

impl RawOplockFile {
    pub(super) fn try_clone_handle(&self) -> io::Result<OwnedHandle> {
        let _operation = BlockingOperation::start(Arc::clone(&self.state))?;
        self.state
            .with_file(|file| file.as_handle().try_clone_to_owned())
    }

    pub(super) async fn metadata(&self) -> io::Result<Metadata> {
        let operation = BlockingOperation::start(Arc::clone(&self.state))?;
        tokio::task::spawn_blocking(move || operation.state.with_file(File::metadata))
            .await
            .map_err(|error| io::Error::other(format!("metadata task failed: {error}")))?
    }

    pub(super) async fn sync_data(&self) -> io::Result<()> {
        let operation = BlockingOperation::start(Arc::clone(&self.state))?;
        tokio::task::spawn_blocking(move || operation.state.with_file(File::sync_data))
            .await
            .map_err(|error| io::Error::other(format!("sync-data task failed: {error}")))?
    }

    pub(super) async fn sync_all(&self) -> io::Result<()> {
        let operation = BlockingOperation::start(Arc::clone(&self.state))?;
        tokio::task::spawn_blocking(move || operation.state.with_file(File::sync_all))
            .await
            .map_err(|error| io::Error::other(format!("sync-all task failed: {error}")))?
    }

    pub(super) async fn read_at(&self, buffer: Vec<u8>, offset: u64) -> RawBufferResult {
        if buffer.is_empty() {
            return (Ok(0), buffer);
        }
        match submit_read(Arc::clone(&self.state), buffer, offset) {
            Ok(request) => request.wait().await,
            Err((error, buffer)) => (Err(error), buffer),
        }
    }

    pub(super) async fn write_at(&self, buffer: Vec<u8>, offset: u64) -> RawBufferResult {
        if buffer.is_empty() {
            return (Ok(0), buffer);
        }
        if !self.state.permits_writes {
            return (
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "writes require an RW or RWH oplock",
                )),
                buffer,
            );
        }
        match submit_write(Arc::clone(&self.state), buffer, offset) {
            Ok(request) => request.wait().await,
            Err((error, buffer)) => (Err(error), buffer),
        }
    }

    pub(super) async fn close(self) -> io::Result<()> {
        Arc::clone(&self.state).close().await
    }
}

impl Drop for RawOplockFile {
    fn drop(&mut self) {
        self.state.initiate_close();
    }
}

struct BlockingOperation {
    state: Arc<FileState>,
}

impl BlockingOperation {
    fn start(state: Arc<FileState>) -> io::Result<Self> {
        state.begin_blocking()?;
        Ok(Self { state })
    }
}

impl Drop for BlockingOperation {
    fn drop(&mut self) {
        self.state.finish_operation();
    }
}

#[repr(C)]
struct Operation {
    overlapped: OVERLAPPED,
    submission_complete: AtomicBool,
    payload: OperationPayload,
    file: Arc<FileState>,
    cancellation: Arc<Mutex<CancelState>>,
}

enum OperationPayload {
    Oplock {
        output: REQUEST_OPLOCK_OUTPUT_BUFFER,
        completion: oneshot::Sender<io::Result<RawOplockBreak>>,
    },
    Read {
        buffer: Vec<u8>,
        completion: oneshot::Sender<RawBufferResult>,
    },
    Write {
        buffer: Vec<u8>,
        completion: oneshot::Sender<RawBufferResult>,
    },
}

struct CancelState {
    overlapped: *mut OVERLAPPED,
    active: bool,
}

// SAFETY: the pointer is accessed only under the mutex and remains valid until
// the completion dispatcher marks this state inactive.
unsafe impl Send for CancelState {}

pub(super) struct RawOplockRequest {
    state: Arc<FileState>,
    cancellation: Arc<Mutex<CancelState>>,
    completion: oneshot::Receiver<io::Result<RawOplockBreak>>,
}

impl RawOplockRequest {
    pub(super) async fn wait(&mut self) -> io::Result<RawOplockBreak> {
        (&mut self.completion)
            .await
            .map_err(|_| io::Error::other("oplock completion dispatcher stopped"))?
    }

    pub(super) async fn cancel_and_wait(mut self) -> io::Result<()> {
        let cancel_error = self.state.cancel_specific(&self.cancellation).err();
        if let Some(error) = cancel_error.as_ref() {
            self.state
                .dispatcher
                .record_issue(RawOperationKind::Oplock, error);
            self.state.initiate_close();
        }

        let completion = (&mut self.completion)
            .await
            .map_err(|_| io::Error::other("oplock completion dispatcher stopped"))?;
        if let Some(error) = cancel_error {
            return Err(error);
        }
        match completion {
            Ok(_) => Ok(()),
            Err(error) if error.raw_os_error() == Some(ERROR_OPERATION_ABORTED as i32) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl Drop for RawOplockRequest {
    fn drop(&mut self) {
        if let Err(error) = self.state.cancel_specific(&self.cancellation) {
            self.state
                .dispatcher
                .record_issue(RawOperationKind::Oplock, &error);
            self.state.initiate_close();
        }
    }
}

type RawBufferResult = (io::Result<usize>, Vec<u8>);

struct RawBufferRequest {
    state: Arc<FileState>,
    kind: RawOperationKind,
    cancellation: Arc<Mutex<CancelState>>,
    completion: oneshot::Receiver<RawBufferResult>,
}

impl RawBufferRequest {
    async fn wait(mut self) -> RawBufferResult {
        (&mut self.completion).await.unwrap_or_else(|_| {
            (
                Err(io::Error::other("file I/O completion dispatcher stopped")),
                Vec::new(),
            )
        })
    }
}

impl Drop for RawBufferRequest {
    fn drop(&mut self) {
        if let Err(error) = self.state.cancel_specific(&self.cancellation) {
            self.state.dispatcher.record_issue(self.kind, &error);
            self.state.initiate_close();
        }
    }
}

fn submit_oplock(state: Arc<FileState>, level: u32) -> io::Result<RawOplockRequest> {
    let (sender, receiver) = oneshot::channel();
    let cancellation = Arc::new(Mutex::new(CancelState {
        overlapped: null_mut(),
        active: true,
    }));
    let mut operation = Box::new(Operation {
        // SAFETY: the documented initial state of OVERLAPPED is all zeroes.
        overlapped: unsafe { zeroed() },
        submission_complete: AtomicBool::new(false),
        payload: OperationPayload::Oplock {
            output: REQUEST_OPLOCK_OUTPUT_BUFFER::default(),
            completion: sender,
        },
        file: Arc::clone(&state),
        cancellation: Arc::clone(&cancellation),
    });
    lock(&cancellation).overlapped = &mut operation.overlapped;

    let input = REQUEST_OPLOCK_INPUT_BUFFER {
        StructureVersion: REQUEST_OPLOCK_CURRENT_VERSION as u16,
        StructureLength: size_of::<REQUEST_OPLOCK_INPUT_BUFFER>() as u16,
        RequestedOplockLevel: level,
        Flags: REQUEST_OPLOCK_INPUT_FLAG_REQUEST,
    };

    submit_operation(state.as_ref(), operation, |file, operation| {
        let output = match &mut operation.payload {
            OperationPayload::Oplock { output, .. } => output,
            _ => unreachable!(),
        };
        // SAFETY: the boxed operation has stable storage, its output lives
        // through completion, and the input is consumed during this call.
        unsafe {
            DeviceIoControl(
                file,
                FSCTL_REQUEST_OPLOCK,
                &input as *const _ as *const _,
                size_of::<REQUEST_OPLOCK_INPUT_BUFFER>() as u32,
                output as *mut _ as *mut _,
                size_of::<REQUEST_OPLOCK_OUTPUT_BUFFER>() as u32,
                null_mut(),
                &mut operation.overlapped,
            )
        }
    })
    .map_err(|failure| failure.error)?;

    Ok(RawOplockRequest {
        state,
        cancellation,
        completion: receiver,
    })
}

fn submit_read(
    state: Arc<FileState>,
    buffer: Vec<u8>,
    offset: u64,
) -> Result<RawBufferRequest, (io::Error, Vec<u8>)> {
    if buffer.len() > u32::MAX as usize {
        return Err((
            io::Error::new(io::ErrorKind::InvalidInput, "read buffer exceeds u32::MAX"),
            buffer,
        ));
    }
    let (sender, receiver) = oneshot::channel();
    let cancellation = Arc::new(Mutex::new(CancelState {
        overlapped: null_mut(),
        active: true,
    }));
    let mut operation = Box::new(Operation {
        // SAFETY: the documented initial state of OVERLAPPED is all zeroes.
        overlapped: unsafe { zeroed() },
        submission_complete: AtomicBool::new(false),
        payload: OperationPayload::Read {
            buffer,
            completion: sender,
        },
        file: Arc::clone(&state),
        cancellation: Arc::clone(&cancellation),
    });
    set_offset(&mut operation.overlapped, offset);
    lock(&cancellation).overlapped = &mut operation.overlapped;

    match submit_operation(Arc::as_ref(&state), operation, |file, operation| {
        let buffer = match &mut operation.payload {
            OperationPayload::Read { buffer, .. } => buffer,
            _ => unreachable!(),
        };
        // SAFETY: the owned buffer and OVERLAPPED remain stable through IOCP
        // completion, and an overlapped call requires a null byte-count pointer.
        unsafe {
            ReadFile(
                file,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                null_mut(),
                &mut operation.overlapped,
            )
        }
    }) {
        Ok(()) => Ok(RawBufferRequest {
            state,
            kind: RawOperationKind::Read,
            cancellation,
            completion: receiver,
        }),
        Err(operation_error) => {
            let buffer = match operation_error.operation.payload {
                OperationPayload::Read { buffer, .. } => buffer,
                _ => unreachable!(),
            };
            Err((operation_error.error, buffer))
        }
    }
}

fn submit_write(
    state: Arc<FileState>,
    buffer: Vec<u8>,
    offset: u64,
) -> Result<RawBufferRequest, (io::Error, Vec<u8>)> {
    if buffer.len() > u32::MAX as usize {
        return Err((
            io::Error::new(io::ErrorKind::InvalidInput, "write buffer exceeds u32::MAX"),
            buffer,
        ));
    }
    let (sender, receiver) = oneshot::channel();
    let cancellation = Arc::new(Mutex::new(CancelState {
        overlapped: null_mut(),
        active: true,
    }));
    let mut operation = Box::new(Operation {
        // SAFETY: the documented initial state of OVERLAPPED is all zeroes.
        overlapped: unsafe { zeroed() },
        submission_complete: AtomicBool::new(false),
        payload: OperationPayload::Write {
            buffer,
            completion: sender,
        },
        file: Arc::clone(&state),
        cancellation: Arc::clone(&cancellation),
    });
    set_offset(&mut operation.overlapped, offset);
    lock(&cancellation).overlapped = &mut operation.overlapped;

    match submit_operation(Arc::as_ref(&state), operation, |file, operation| {
        let buffer = match &operation.payload {
            OperationPayload::Write { buffer, .. } => buffer,
            _ => unreachable!(),
        };
        // SAFETY: the owned buffer and OVERLAPPED remain stable through IOCP
        // completion, and an overlapped call requires a null byte-count pointer.
        unsafe {
            WriteFile(
                file,
                buffer.as_ptr(),
                buffer.len() as u32,
                null_mut(),
                &mut operation.overlapped,
            )
        }
    }) {
        Ok(()) => Ok(RawBufferRequest {
            state,
            kind: RawOperationKind::Write,
            cancellation,
            completion: receiver,
        }),
        Err(operation_error) => {
            let buffer = match operation_error.operation.payload {
                OperationPayload::Write { buffer, .. } => buffer,
                _ => unreachable!(),
            };
            Err((operation_error.error, buffer))
        }
    }
}

struct SubmitError {
    error: io::Error,
    operation: Box<Operation>,
}

fn submit_operation(
    state: &FileState,
    operation: Box<Operation>,
    submit: impl FnOnce(HANDLE, &mut Operation) -> i32,
) -> Result<(), SubmitError> {
    let mut runtime = lock(&state.dispatcher.state);
    let mut inner = lock(&state.inner);
    if !runtime.accepting || inner.closing {
        return Err(SubmitError {
            error: runtime_closed_error(),
            operation,
        });
    }
    let Some(raw_file) = inner
        .file
        .as_ref()
        .map(|file| file.as_raw_handle() as HANDLE)
    else {
        return Err(SubmitError {
            error: runtime_closed_error(),
            operation,
        });
    };

    runtime.active_operations += 1;
    inner.pending += 1;

    // Transfer ownership before the syscall: an immediately successful I/O can
    // be dequeued and reclaimed by the worker before the syscall returns.
    let raw_operation = Box::into_raw(operation);
    // SAFETY: raw_operation remains owned by either the IOCP completion path or
    // the synchronous-failure path below.
    let submitted = submit(raw_file, unsafe { &mut *raw_operation });
    // A successful I/O may be dequeued before its submission call returns. The
    // worker waits for this hand-off before reconstructing the Box, so no Rust
    // reference into the allocation can overlap its destruction.
    unsafe {
        (*raw_operation)
            .submission_complete
            .store(true, Ordering::Release);
    }
    if submitted != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
        return Ok(());
    }

    runtime.active_operations -= 1;
    inner.pending -= 1;
    lock(unsafe { &(*raw_operation).cancellation }).active = false;
    // SAFETY: a synchronous non-pending failure never queues a completion, so
    // ownership was not transferred to the dispatcher.
    let operation = unsafe { Box::from_raw(raw_operation) };
    Err(SubmitError { error, operation })
}

fn completion_loop(dispatcher: Arc<Dispatcher>) {
    loop {
        let (mut bytes, mut key, mut overlapped) = (0, 0, null_mut());
        // SAFETY: the completion port is live and outputs are writable locals.
        let ok = unsafe {
            GetQueuedCompletionStatus(
                dispatcher.port.raw(),
                &mut bytes,
                &mut key,
                &mut overlapped,
                COMPLETION_POLL_MS,
            )
        };
        if overlapped.is_null() {
            if key == SHUTDOWN_KEY {
                break;
            }
            if ok == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(WAIT_TIMEOUT as i32) {
                    let state = lock(&dispatcher.state);
                    if state.shutdown_started && state.active_operations == 0 {
                        break;
                    }
                    continue;
                }
                dispatcher.record_issue(RawOperationKind::Dispatcher, &error);
            }
            continue;
        }

        let error = (ok == 0).then(io::Error::last_os_error);
        let raw_operation = overlapped.cast::<Operation>();
        // SAFETY: packets on this private port originate from boxed Operations
        // with OVERLAPPED as their first field. The allocation remains live
        // until the completion path reconstructs it below.
        while unsafe { !(*raw_operation).submission_complete.load(Ordering::Acquire) } {
            thread::yield_now();
        }
        // SAFETY: submission transferred sole ownership to this completion.
        let operation = unsafe { Box::from_raw(raw_operation) };
        lock(&operation.cancellation).active = false;
        let file = Arc::clone(&operation.file);

        match operation.payload {
            OperationPayload::Oplock { output, completion } => {
                let result = error.map_or_else(
                    || {
                        Ok(RawOplockBreak {
                            original_level: output.OriginalOplockLevel,
                            new_level: output.NewOplockLevel,
                            flags: output.Flags,
                            ack_required: output.Flags & REQUEST_OPLOCK_OUTPUT_FLAG_ACK_REQUIRED
                                != 0,
                            access_mode: output.AccessMode,
                            share_mode: u32::from(output.ShareMode),
                        })
                    },
                    Err,
                );
                let _ = completion.send(result);
            }
            OperationPayload::Read { buffer, completion }
            | OperationPayload::Write { buffer, completion } => {
                let result = error.map_or(Ok(bytes as usize), Err);
                let _ = completion.send((result, buffer));
            }
        }
        file.finish_operation();
    }

    {
        let mut state = lock(&dispatcher.state);
        state.worker_stopped = true;
    }
    dispatcher.stopped.notify_waiters();
}

fn set_offset(overlapped: &mut OVERLAPPED, offset: u64) {
    overlapped.Anonymous.Anonymous.Offset = offset as u32;
    overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
}

fn close_owned_file(file: File) -> io::Result<()> {
    let raw = file.into_raw_handle() as HANDLE;
    // SAFETY: into_raw_handle transfers the sole owned handle to this call.
    if unsafe { CloseHandle(raw) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn runtime_closed_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        "oplock runtime or handle is closing",
    )
}

pub(super) fn open_path(path: &Path, options: OplockOptions) -> io::Result<File> {
    let mut open = OpenOptions::new();
    open.read(true)
        .write(options.level().permits_writes())
        .share_mode(options.share_mode().bits());

    let mut flags = FILE_FLAG_OVERLAPPED | FILE_FLAG_OPEN_REQUIRING_OPLOCK;
    if options.target() == OplockTarget::Directory {
        flags |= FILE_FLAG_BACKUP_SEMANTICS;
    }
    open.custom_flags(flags).open(path)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
