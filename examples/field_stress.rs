#[cfg(not(windows))]
fn main() {
    eprintln!("This example requires Windows.");
}

#[cfg(windows)]
mod windows_app {
    use std::{
        collections::HashSet,
        env,
        ffi::OsString,
        future::pending,
        io,
        os::windows::ffi::OsStrExt,
        path::{Path, PathBuf},
        process::{Command, Output, Stdio},
        ptr::{null, null_mut},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use tokio::{sync::oneshot, task::JoinSet, time::timeout};
    use tokio_oplock::{OplockLevel, OplockOptions, OplockOutcome, OplockRuntime, OplockTarget};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            CREATE_ALWAYS, CreateFileW, DELETE, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_RANDOM_ACCESS,
            FILE_FLAG_SEQUENTIAL_SCAN, FILE_FLAG_WRITE_THROUGH, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, OPEN_ALWAYS, OPEN_EXISTING, ReadFile, TRUNCATE_EXISTING, WriteFile,
        },
    };

    const OPERATION_TIMEOUT: Duration = Duration::from_secs(10);
    const CANCEL_BATCH: usize = 64;

    fn read_handle() -> OplockOptions {
        OplockOptions::new(OplockTarget::File, OplockLevel::ReadHandle)
    }

    #[derive(Clone)]
    struct OpenSpec {
        access_name: &'static str,
        access: u32,
        share: u32,
        disposition_name: &'static str,
        disposition: u32,
        flags_name: &'static str,
        flags: u32,
    }

    impl OpenSpec {
        fn label(&self) -> String {
            format!(
                "access={} share={:#x} disposition={} flags={}",
                self.access_name, self.share, self.disposition_name, self.flags_name
            )
        }
    }

    #[derive(Default)]
    struct Stats {
        external_opens: u64,
        oplock_breaks: u64,
        opens_without_break: u64,
        completed_cancellations: u64,
        aborted_cancellations: u64,
        self_opens: u64,
        max_external_open_us: u128,
        covered_specs: HashSet<usize>,
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> io::Result<Self> {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = env::temp_dir().join(format!(
                "tokio-oplock-field-stress-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir(&path)?;
            Ok(Self(path))
        }

        fn path(&self, name: impl AsRef<Path>) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub async fn run() -> io::Result<()> {
        let mut args = env::args_os();
        let _executable = args.next();
        match args.next() {
            Some(mode) if mode == "--child-open" => child_open(args.collect()),
            Some(mode) if mode == "--child-hold" => child_hold(args.collect()),
            Some(mode) if mode == "--runtime-shutdown-child" => {
                runtime_shutdown_child(args.collect()).await
            }
            Some(mode) if mode == "--probe-runtime-shutdown" => {
                probe_runtime_shutdown(args.collect()).await
            }
            Some(mode) if mode == "--probe-preexisting" => probe_preexisting(args.collect()).await,
            first => run_parent(first, args.collect()).await,
        }
    }

    async fn run_parent(first: Option<OsString>, rest: Vec<OsString>) -> io::Result<()> {
        let requested = parse_duration(first, rest)?;
        let directory = TestDirectory::new()?;
        let contention_path = directory.path("contention.bin");
        let self_path = directory.path("self-open.bin");
        let cancel_paths = (0..CANCEL_BATCH)
            .map(|index| directory.path(format!("cancel-{index}.bin")))
            .collect::<Vec<_>>();

        std::fs::write(&contention_path, b"field stress baseline")?;
        std::fs::write(&self_path, b"self-open baseline")?;
        for path in &cancel_paths {
            std::fs::write(path, b"cancel baseline")?;
        }

        let specs = open_specs();
        let executable = env::current_exe()?;
        let runtime = OplockRuntime::new()?;
        let started = Instant::now();
        let deadline = started + requested;
        let mut last_progress = started;
        let mut stats = Stats::default();
        let mut sequence = 0usize;

        println!(
            "START duration_secs={} matrix_specs={} cancel_batch={} pid={}",
            requested.as_secs(),
            specs.len(),
            CANCEL_BATCH,
            std::process::id()
        );

        while Instant::now() < deadline {
            let spec_index = sequence % specs.len();
            run_external_open(
                &runtime,
                &executable,
                &contention_path,
                &specs[spec_index],
                &mut stats,
            )
            .await
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("{}: {error}", specs[spec_index].label()),
                )
            })?;
            stats.covered_specs.insert(spec_index);
            sequence += 1;

            if sequence % 16 == 0 {
                run_self_open(&runtime, &self_path, &mut stats).await?;
            }
            if sequence % 32 == 0 {
                run_completed_cancellation_batch(&runtime, &cancel_paths, &mut stats).await?;
            }
            if sequence % 64 == 0 {
                run_aborted_cancellation_batch(&runtime, &cancel_paths, &mut stats).await?;
            }

            if last_progress.elapsed() >= Duration::from_secs(15) {
                println!(
                    "PROGRESS elapsed_secs={} external_opens={} breaks={} no_break={} cancel_complete={} cancel_abort={} self_opens={} covered={}/{} max_open_us={}",
                    started.elapsed().as_secs(),
                    stats.external_opens,
                    stats.oplock_breaks,
                    stats.opens_without_break,
                    stats.completed_cancellations,
                    stats.aborted_cancellations,
                    stats.self_opens,
                    stats.covered_specs.len(),
                    specs.len(),
                    stats.max_external_open_us,
                );
                last_progress = Instant::now();
            }
        }

        let elapsed = started.elapsed();
        println!(
            "PASS elapsed_ms={} external_opens={} breaks={} no_break={} cancel_complete={} cancel_abort={} self_opens={} covered={}/{} max_open_us={}",
            elapsed.as_millis(),
            stats.external_opens,
            stats.oplock_breaks,
            stats.opens_without_break,
            stats.completed_cancellations,
            stats.aborted_cancellations,
            stats.self_opens,
            stats.covered_specs.len(),
            specs.len(),
            stats.max_external_open_us,
        );
        runtime.shutdown().await?;
        Ok(())
    }

    fn parse_duration(first: Option<OsString>, rest: Vec<OsString>) -> io::Result<Duration> {
        let args = first.into_iter().chain(rest).collect::<Vec<_>>();
        let mut duration_secs = 600u64;
        let mut index = 0;
        while index < args.len() {
            if args[index] == "--duration-secs" {
                let value = args.get(index + 1).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "missing --duration-secs value")
                })?;
                duration_secs = parse_number(value, "duration")?;
                index += 2;
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument {}", args[index].to_string_lossy()),
                ));
            }
        }
        Ok(Duration::from_secs(duration_secs))
    }

    fn open_specs() -> Vec<OpenSpec> {
        let accesses = [
            ("metadata", 0),
            ("read", GENERIC_READ),
            ("write", GENERIC_WRITE),
            ("read_write", GENERIC_READ | GENERIC_WRITE),
            ("delete", DELETE),
        ];
        let flags = [
            ("normal", FILE_ATTRIBUTE_NORMAL),
            (
                "sequential",
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_SEQUENTIAL_SCAN,
            ),
            ("random", FILE_ATTRIBUTE_NORMAL | FILE_FLAG_RANDOM_ACCESS),
            (
                "write_through",
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_WRITE_THROUGH,
            ),
        ];
        let mut specs = Vec::new();

        for (access_name, access) in accesses {
            let mut dispositions = vec![("open_existing", OPEN_EXISTING)];
            if access & GENERIC_WRITE != 0 {
                dispositions.extend([
                    ("open_always", OPEN_ALWAYS),
                    ("create_always", CREATE_ALWAYS),
                    ("truncate_existing", TRUNCATE_EXISTING),
                ]);
            }
            for share in 0..=(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE) {
                for &(disposition_name, disposition) in &dispositions {
                    for &(flags_name, open_flags) in &flags {
                        specs.push(OpenSpec {
                            access_name,
                            access,
                            share,
                            disposition_name,
                            disposition,
                            flags_name,
                            flags: open_flags,
                        });
                    }
                }
            }
        }
        specs
    }

    async fn run_external_open(
        runtime: &OplockRuntime,
        executable: &Path,
        path: &Path,
        spec: &OpenSpec,
        stats: &mut Stats,
    ) -> io::Result<()> {
        std::fs::write(path, b"field stress baseline")?;
        let (ready_tx, ready_rx) = oneshot::channel();
        let oplock_path = path.to_path_buf();
        let runtime = runtime.clone();
        let mut oplock = tokio::spawn(async move {
            runtime
                .run(oplock_path, read_handle(), async |_file| {
                    let _ = ready_tx.send(());
                    pending::<io::Result<()>>().await
                })
                .await
        });

        timeout(OPERATION_TIMEOUT, ready_rx)
            .await
            .map_err(|_| io::Error::other("oplock request did not become ready"))?
            .map_err(|_| io::Error::other("oplock request ended before becoming ready"))?;

        let mut command = Command::new(executable);
        command
            .arg("--child-open")
            .arg(path)
            .arg(spec.access.to_string())
            .arg(spec.share.to_string())
            .arg(spec.disposition.to_string())
            .arg(spec.flags.to_string())
            .arg("2")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child_started = Instant::now();
        let mut child = tokio::task::spawn_blocking(move || command.output());

        let output = tokio::select! {
            oplock_result = &mut oplock => {
                match oplock_result.map_err(io::Error::other)?? {
                    OplockOutcome::Broken { guard, .. } => {
                        stats.oplock_breaks += 1;
                        tokio::time::sleep(Duration::from_millis(2)).await;
                        guard.close().await?;
                    }
                    OplockOutcome::Completed(()) => {
                        return Err(io::Error::other("pending work completed unexpectedly"));
                    }
                }
                timeout(OPERATION_TIMEOUT, &mut child)
                    .await
                    .map_err(|_| io::Error::other("external application stayed blocked after guard drop"))?
                    .map_err(io::Error::other)??
            }
            child_result = &mut child => {
                let output = child_result.map_err(io::Error::other)??;
                stats.opens_without_break += 1;
                oplock.abort();
                let _ = oplock.await;
                output
            }
            _ = tokio::time::sleep(OPERATION_TIMEOUT) => {
                oplock.abort();
                return Err(io::Error::other("external application/oplock interaction timed out"));
            }
        };

        ensure_child_success(&output)?;
        stats.external_opens += 1;
        stats.max_external_open_us = stats
            .max_external_open_us
            .max(child_started.elapsed().as_micros());
        Ok(())
    }

    async fn run_completed_cancellation_batch(
        runtime: &OplockRuntime,
        paths: &[PathBuf],
        stats: &mut Stats,
    ) -> io::Result<()> {
        let mut tasks = JoinSet::new();
        for path in paths.iter().cloned() {
            let runtime = runtime.clone();
            tasks.spawn(async move {
                runtime
                    .run(path, read_handle(), async |_file| Ok::<(), io::Error>(()))
                    .await
            });
        }

        timeout(OPERATION_TIMEOUT, async {
            while let Some(result) = tasks.join_next().await {
                match result.map_err(io::Error::other)?? {
                    OplockOutcome::Completed(()) => stats.completed_cancellations += 1,
                    OplockOutcome::Broken { guard, .. } => {
                        guard.close().await?;
                        return Err(io::Error::other(
                            "cancellation-only oplock unexpectedly broke",
                        ));
                    }
                }
            }
            Ok::<(), io::Error>(())
        })
        .await
        .map_err(|_| io::Error::other("completed cancellation batch timed out"))?
    }

    async fn run_aborted_cancellation_batch(
        runtime: &OplockRuntime,
        paths: &[PathBuf],
        stats: &mut Stats,
    ) -> io::Result<()> {
        let mut tasks = Vec::with_capacity(paths.len());
        let mut ready = Vec::with_capacity(paths.len());
        for path in paths.iter().cloned() {
            let runtime = runtime.clone();
            let (ready_tx, ready_rx) = oneshot::channel();
            ready.push(ready_rx);
            tasks.push(tokio::spawn(async move {
                runtime
                    .run(path, read_handle(), async |_file| {
                        let _ = ready_tx.send(());
                        pending::<io::Result<()>>().await
                    })
                    .await
            }));
        }

        timeout(OPERATION_TIMEOUT, async {
            for receiver in ready {
                receiver
                    .await
                    .map_err(|_| io::Error::other("aborted oplock was not ready"))?;
            }
            Ok::<(), io::Error>(())
        })
        .await
        .map_err(|_| io::Error::other("aborted cancellation setup timed out"))??;

        for task in tasks {
            task.abort();
            let _ = task.await;
            stats.aborted_cancellations += 1;
        }
        Ok(())
    }

    async fn run_self_open(
        runtime: &OplockRuntime,
        path: &Path,
        stats: &mut Stats,
    ) -> io::Result<()> {
        std::fs::write(path, b"self-open baseline")?;
        let work_path = path.to_path_buf();
        let outcome = timeout(
            OPERATION_TIMEOUT,
            runtime.run(path, read_handle(), async |_file| {
                let file = tokio::fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(work_path)
                    .await?;
                drop(file);
                Ok::<(), io::Error>(())
            }),
        )
        .await
        .map_err(|_| io::Error::other("opening the oplocked file from its own work timed out"))??;

        if let OplockOutcome::Broken { guard, .. } = outcome {
            guard.close().await?;
        }
        stats.self_opens += 1;
        Ok(())
    }

    fn child_open(args: Vec<OsString>) -> io::Result<()> {
        if args.len() != 6 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "child-open expects path, access, share, disposition, flags, hold-ms",
            ));
        }
        let path = PathBuf::from(&args[0]);
        let access = parse_number::<u32>(&args[1], "access")?;
        let share = parse_number::<u32>(&args[2], "share")?;
        let disposition = parse_number::<u32>(&args[3], "disposition")?;
        let flags = parse_number::<u32>(&args[4], "flags")?;
        let hold_ms = parse_number::<u64>(&args[5], "hold-ms")?;
        let wide_path = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let started = Instant::now();

        // SAFETY: the path is NUL-terminated and all remaining values are passed
        // straight through from the parent process's valid scenario matrix.
        let handle = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                access,
                share,
                null(),
                disposition,
                flags,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let open_us = started.elapsed().as_micros();
        let result = exercise_handle(handle, access);
        std::thread::sleep(Duration::from_millis(hold_ms));
        // SAFETY: CreateFileW returned this owned, valid handle and it is closed once.
        let closed = unsafe { CloseHandle(handle) };
        result?;
        if closed == 0 {
            return Err(io::Error::last_os_error());
        }
        println!("OPENED open_us={open_us}");
        Ok(())
    }

    fn exercise_handle(handle: HANDLE, access: u32) -> io::Result<()> {
        if access & GENERIC_READ != 0 {
            let mut byte = [0u8; 1];
            let mut read = 0u32;
            // SAFETY: this is a synchronous handle and the one-byte output buffer is valid.
            let ok = unsafe {
                ReadFile(
                    handle,
                    byte.as_mut_ptr(),
                    byte.len() as u32,
                    &mut read,
                    null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        if access & GENERIC_WRITE != 0 {
            let byte = [b'X'];
            let mut written = 0u32;
            // SAFETY: this is a synchronous handle and the one-byte input buffer is valid.
            let ok = unsafe {
                WriteFile(
                    handle,
                    byte.as_ptr(),
                    byte.len() as u32,
                    &mut written,
                    null_mut(),
                )
            };
            if ok == 0 || written != 1 {
                return Err(if ok == 0 {
                    io::Error::last_os_error()
                } else {
                    io::Error::other("short child write")
                });
            }
        }
        Ok(())
    }

    fn child_hold(args: Vec<OsString>) -> io::Result<()> {
        if args.len() != 5 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "child-hold expects path, access, share, hold-ms, ready-path",
            ));
        }
        let path = PathBuf::from(&args[0]);
        let access = parse_number::<u32>(&args[1], "access")?;
        let share = parse_number::<u32>(&args[2], "share")?;
        let hold_ms = parse_number::<u64>(&args[3], "hold-ms")?;
        let ready_path = PathBuf::from(&args[4]);
        let wide_path = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();

        // SAFETY: the path is NUL-terminated and the scenario uses a normal,
        // synchronous OPEN_EXISTING request.
        let handle = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                access,
                share,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        exercise_handle(handle, access)?;
        std::fs::write(&ready_path, b"ready")?;
        std::thread::sleep(Duration::from_millis(hold_ms));
        let result = exercise_handle(handle, access);
        // SAFETY: CreateFileW returned this owned, valid handle and it is closed once.
        let closed = unsafe { CloseHandle(handle) };
        result?;
        if closed == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    async fn runtime_shutdown_child(args: Vec<OsString>) -> io::Result<()> {
        let path = args
            .first()
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing probe path"))?;
        std::fs::write(&path, b"runtime shutdown probe")?;
        let runtime = OplockRuntime::new()?;
        let (ready_tx, ready_rx) = oneshot::channel();
        let runner = runtime.clone();
        let task = tokio::spawn(async move {
            runner
                .run(path, read_handle(), async |_file| {
                    let _ = ready_tx.send(());
                    pending::<io::Result<()>>().await
                })
                .await
        });
        ready_rx
            .await
            .map_err(|_| io::Error::other("runtime shutdown probe was not ready"))?;

        // Explicit shutdown must cancel and drain the pending request without
        // relying on the task to release its runtime clone first.
        runtime.shutdown().await?;
        let _ = task.await;
        Ok(())
    }

    async fn probe_runtime_shutdown(args: Vec<OsString>) -> io::Result<()> {
        let timeout_secs = args
            .first()
            .map(|value| parse_number::<u64>(value, "probe timeout"))
            .transpose()?
            .unwrap_or(3);
        let directory = TestDirectory::new()?;
        let path = directory.path("runtime-shutdown.bin");
        let mut child = Command::new(env::current_exe()?)
            .arg("--runtime-shutdown-child")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);

        while Instant::now() < deadline {
            if let Some(status) = child.try_wait()? {
                if status.success() {
                    println!("PASS runtime_shutdown_pending_oplock");
                    return Ok(());
                }
                return Err(io::Error::other(format!(
                    "runtime shutdown child failed with {status}"
                )));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        child.kill()?;
        let _ = child.wait();
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "runtime shutdown deadlocked for at least {timeout_secs}s while an oplock was pending"
            ),
        ))
    }

    async fn probe_preexisting(args: Vec<OsString>) -> io::Result<()> {
        if !args.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "probe-preexisting takes no arguments",
            ));
        }
        let directory = TestDirectory::new()?;
        let path = directory.path("preexisting.bin");
        let executable = env::current_exe()?;
        let runtime = OplockRuntime::new()?;
        let mut library_successes = 0u32;
        let mut library_open_errors = 0u32;

        for share in 0..=(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE) {
            std::fs::write(&path, b"preexisting application baseline")?;
            let ready_path = directory.path(format!("preexisting-{share}.ready"));
            let mut child = Command::new(&executable)
                .arg("--child-hold")
                .arg(&path)
                .arg((GENERIC_READ | GENERIC_WRITE).to_string())
                .arg(share.to_string())
                .arg("100")
                .arg(&ready_path)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;

            timeout(Duration::from_secs(2), async {
                loop {
                    if ready_path.exists() {
                        break Ok::<(), io::Error>(());
                    }
                    if let Some(status) = child.try_wait()? {
                        break Err(io::Error::other(format!(
                            "preexisting child exited before ready with {status}"
                        )));
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .map_err(|_| io::Error::other("preexisting child did not become ready"))??;

            match timeout(
                Duration::from_secs(2),
                runtime.run(&path, read_handle(), async |_file| Ok::<(), io::Error>(())),
            )
            .await
            .map_err(|_| io::Error::other("library open blocked behind preexisting application"))?
            {
                Ok(OplockOutcome::Completed(())) => library_successes += 1,
                Ok(OplockOutcome::Broken { guard, .. }) => {
                    guard.close().await?;
                    library_successes += 1;
                }
                Err(_) => library_open_errors += 1,
            }

            let output = tokio::task::spawn_blocking(move || child.wait_with_output())
                .await
                .map_err(io::Error::other)??;
            ensure_child_success(&output)?;
        }

        runtime.shutdown().await?;
        println!(
            "PASS preexisting_application share_masks=8 child_failures=0 library_successes={} library_open_errors={}",
            library_successes, library_open_errors
        );
        Ok(())
    }

    fn ensure_child_success(output: &Output) -> io::Result<()> {
        if output.status.success() {
            return Ok(());
        }
        Err(io::Error::other(format!(
            "external application failed with {}; stdout={}; stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim(),
        )))
    }

    fn parse_number<T>(value: &OsString, name: &str) -> io::Result<T>
    where
        T: std::str::FromStr,
        T::Err: std::fmt::Display,
    {
        value.to_string_lossy().parse().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid {name}: {error}"),
            )
        })
    }
}

#[cfg(windows)]
#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> std::io::Result<()> {
    windows_app::run().await
}
