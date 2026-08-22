use std::{
    io,
    mem::{size_of, zeroed},
    os::windows::io::AsRawHandle,
    ptr::null_mut,
    sync::{Arc, Condvar, Mutex, MutexGuard, Weak},
    thread,
};

use tokio::sync::oneshot;
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_IO_PENDING, HANDLE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::FILE_FLAG_OVERLAPPED,
    System::{
        IO::{
            CancelIoEx, CreateIoCompletionPort, DeviceIoControl, GetQueuedCompletionStatus,
            OVERLAPPED, PostQueuedCompletionStatus,
        },
        Ioctl::{
            FSCTL_REQUEST_OPLOCK, OPLOCK_LEVEL_CACHE_HANDLE, OPLOCK_LEVEL_CACHE_READ,
            REQUEST_OPLOCK_CURRENT_VERSION, REQUEST_OPLOCK_INPUT_BUFFER,
            REQUEST_OPLOCK_INPUT_FLAG_REQUEST, REQUEST_OPLOCK_OUTPUT_BUFFER,
            REQUEST_OPLOCK_OUTPUT_FLAG_ACK_REQUIRED,
        },
    },
};

pub(super) const OVERLAPPED_FILE_FLAG: u32 = FILE_FLAG_OVERLAPPED;
const SHUTDOWN_KEY: usize = usize::MAX;

pub(super) struct RawOplockBreak {
    pub(super) original_level: u32,
    pub(super) new_level: u32,
    pub(super) flags: u32,
    pub(super) ack_required: bool,
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: this is a valid owned handle, closed only here.
        unsafe { CloseHandle(self.0) };
    }
}

struct RuntimeState {
    accepting: bool,
    active: usize,
}

struct Dispatcher {
    port: OwnedHandle,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    state: Mutex<RuntimeState>,
    idle: Condvar,
}

// SAFETY: IOCP handles support concurrent association and posting. Queue reads
// are confined to the worker thread and other state is mutex-protected.
unsafe impl Send for Dispatcher {}
unsafe impl Sync for Dispatcher {}

pub(super) struct RawRuntime {
    dispatcher: Arc<Dispatcher>,
}

static RUNTIME: Mutex<Option<Weak<Dispatcher>>> = Mutex::new(None);

impl RawRuntime {
    pub(super) fn new() -> io::Result<Self> {
        let mut runtime = lock(&RUNTIME);
        if runtime.as_ref().and_then(Weak::upgrade).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "an oplock runtime is already running",
            ));
        }

        // SAFETY: this argument combination creates a new IOCP.
        let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, null_mut(), 0, 1) };
        if port.is_null() {
            return Err(io::Error::last_os_error());
        }

        let port_bits = port as usize;
        let worker = match thread::Builder::new()
            .name("oplock-iocp".into())
            .spawn(move || completion_loop(port_bits as HANDLE))
        {
            Ok(worker) => worker,
            Err(error) => {
                // SAFETY: no thread was created, so this remains our handle.
                unsafe { CloseHandle(port) };
                return Err(error);
            }
        };

        let dispatcher = Arc::new(Dispatcher {
            port: OwnedHandle(port),
            worker: Mutex::new(Some(worker)),
            state: Mutex::new(RuntimeState {
                accepting: true,
                active: 0,
            }),
            idle: Condvar::new(),
        });
        *runtime = Some(Arc::downgrade(&dispatcher));
        Ok(Self { dispatcher })
    }
}

impl Drop for RawRuntime {
    fn drop(&mut self) {
        self.dispatcher.shutdown();
    }
}

impl Dispatcher {
    fn shutdown(&self) {
        let mut state = lock(&self.state);
        state.accepting = false;
        while state.active != 0 {
            state = self
                .idle
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        drop(state);

        // SAFETY: the port is live and the reserved key identifies our sentinel.
        let posted =
            unsafe { PostQueuedCompletionStatus(self.port.0, 0, SHUTDOWN_KEY, null_mut()) };
        if posted == 0 {
            return;
        }

        let worker = lock(&self.worker).take();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }
}

/// IOCP returns a pointer to the first field, allowing reconstruction of the box.
#[repr(C)]
struct Operation {
    overlapped: OVERLAPPED,
    output: REQUEST_OPLOCK_OUTPUT_BUFFER,
    _file: Arc<tokio::fs::File>,
    cancellation: Arc<Mutex<CancelState>>,
    completion: oneshot::Sender<io::Result<RawOplockBreak>>,
    dispatcher: Arc<Dispatcher>,
}

pub(super) struct RawOplockGuard {
    file: Arc<tokio::fs::File>,
    cancellation: Arc<Mutex<CancelState>>,
}

pub(super) struct RawBreakReceiver {
    completion: oneshot::Receiver<io::Result<RawOplockBreak>>,
}

struct CancelState {
    overlapped: *mut OVERLAPPED,
    active: bool,
}

// SAFETY: the pointer is accessed only under the mutex and remains valid until
// the dispatcher marks the state inactive.
unsafe impl Send for CancelState {}

pub(super) fn request(file: tokio::fs::File) -> io::Result<(RawOplockGuard, RawBreakReceiver)> {
    let dispatcher = lock(&RUNTIME)
        .as_ref()
        .and_then(Weak::upgrade)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no oplock runtime"))?;
    {
        let mut state = lock(&dispatcher.state);
        if !state.accepting {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "oplock runtime is shutting down",
            ));
        }
        state.active += 1;
    }

    let result = submit_request(Arc::clone(&dispatcher), file);
    if result.is_err() {
        operation_finished(&dispatcher);
    }
    result
}

fn submit_request(
    dispatcher: Arc<Dispatcher>,
    file: tokio::fs::File,
) -> io::Result<(RawOplockGuard, RawBreakReceiver)> {
    let file = Arc::new(file);
    let raw_file = file.as_raw_handle() as HANDLE;
    // SAFETY: both handles are live and valid for IOCP association.
    let associated = unsafe { CreateIoCompletionPort(raw_file, dispatcher.port.0, 0, 0) };
    if associated.is_null() {
        return Err(io::Error::last_os_error());
    }

    let (sender, receiver) = oneshot::channel();
    let cancellation = Arc::new(Mutex::new(CancelState {
        overlapped: null_mut(),
        active: true,
    }));
    let mut operation = Box::new(Operation {
        // SAFETY: the documented initial state of OVERLAPPED is zeroed.
        overlapped: unsafe { zeroed() },
        output: REQUEST_OPLOCK_OUTPUT_BUFFER::default(),
        _file: Arc::clone(&file),
        cancellation: Arc::clone(&cancellation),
        completion: sender,
        dispatcher,
    });
    lock(&cancellation).overlapped = &mut operation.overlapped;
    submit(operation, raw_file)?;

    Ok((
        RawOplockGuard { file, cancellation },
        RawBreakReceiver {
            completion: receiver,
        },
    ))
}

impl RawBreakReceiver {
    pub(super) async fn wait_for_break(self) -> io::Result<RawOplockBreak> {
        self.completion
            .await
            .map_err(|_| io::Error::other("oplock IOCP dispatcher stopped"))?
    }
}

impl RawOplockGuard {
    pub(super) fn file(&self) -> Arc<tokio::fs::File> {
        Arc::clone(&self.file)
    }
}

impl Drop for RawOplockGuard {
    fn drop(&mut self) {
        let mut cancellation = lock(&self.cancellation);
        if cancellation.active {
            // SAFETY: file and operation are alive; the mutex prevents a race
            // with the dispatcher reclaiming the operation.
            unsafe { CancelIoEx(self.file.as_raw_handle() as HANDLE, cancellation.overlapped) };
            cancellation.active = false;
        }
    }
}

fn submit(mut operation: Box<Operation>, file: HANDLE) -> io::Result<()> {
    let input = REQUEST_OPLOCK_INPUT_BUFFER {
        StructureVersion: REQUEST_OPLOCK_CURRENT_VERSION as u16,
        StructureLength: size_of::<REQUEST_OPLOCK_INPUT_BUFFER>() as u16,
        RequestedOplockLevel: OPLOCK_LEVEL_CACHE_READ | OPLOCK_LEVEL_CACHE_HANDLE,
        Flags: REQUEST_OPLOCK_INPUT_FLAG_REQUEST,
    };
    // SAFETY: the file remains alive and boxed buffers remain stable through IOCP.
    let submitted = unsafe {
        DeviceIoControl(
            file,
            FSCTL_REQUEST_OPLOCK,
            &input as *const _ as *const _,
            size_of::<REQUEST_OPLOCK_INPUT_BUFFER>() as u32,
            &mut operation.output as *mut _ as *mut _,
            size_of::<REQUEST_OPLOCK_OUTPUT_BUFFER>() as u32,
            null_mut(),
            &mut operation.overlapped,
        )
    };
    if submitted == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
            return Err(error);
        }
    }
    let _ = Box::into_raw(operation);
    Ok(())
}

fn completion_loop(port: HANDLE) {
    loop {
        let (mut bytes, mut key, mut overlapped) = (0, 0, null_mut());
        // SAFETY: port is owned by the runtime and outputs are writable locals.
        let ok = unsafe {
            GetQueuedCompletionStatus(port, &mut bytes, &mut key, &mut overlapped, u32::MAX)
        };
        if overlapped.is_null() {
            if key == SHUTDOWN_KEY {
                break;
            }
            if ok == 0 {
                // The port was closed without an explicit shutdown packet.
                break;
            }
            continue;
        }

        // SAFETY: packets on this private port originate from boxed Operations
        // with OVERLAPPED as their first field.
        let operation = unsafe { Box::from_raw(overlapped.cast::<Operation>()) };
        lock(&operation.cancellation).active = false;
        let result = if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            let output = operation.output;
            Ok(RawOplockBreak {
                original_level: output.OriginalOplockLevel,
                new_level: output.NewOplockLevel,
                flags: output.Flags,
                ack_required: output.Flags & REQUEST_OPLOCK_OUTPUT_FLAG_ACK_REQUIRED != 0,
            })
        };
        let dispatcher = Arc::clone(&operation.dispatcher);
        let _ = operation.completion.send(result);
        operation_finished(&dispatcher);
    }
}

fn operation_finished(dispatcher: &Dispatcher) {
    let became_idle = {
        let mut state = lock(&dispatcher.state);
        state.active -= 1;
        state.active == 0
    };
    if became_idle {
        dispatcher.idle.notify_all();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
