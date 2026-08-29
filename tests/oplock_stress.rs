#![cfg(windows)]

use std::{
    future::pending,
    io,
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::{
    sync::{Mutex, MutexGuard, oneshot},
    task::JoinSet,
    time::timeout,
};
use tokio_oplock::{
    OplockError, OplockLevel, OplockOptions, OplockOutcome, OplockRuntime, OplockTarget, ShareMode,
};
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

const CASE_TIMEOUT: Duration = Duration::from_secs(30);
const CONCURRENT_OPLOCKS: usize = 64;
const RESTART_CYCLES: usize = 16;

static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(case: &str) -> io::Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("oplocks-{case}-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn files(&self, count: usize) -> io::Result<Vec<PathBuf>> {
        (0..count)
            .map(|index| {
                let path = self.0.join(format!("file-{index}.txt"));
                std::fs::write(&path, b"oplock stress test")?;
                Ok(path)
            })
            .collect()
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn options(level: OplockLevel) -> OplockOptions {
    OplockOptions::new(OplockTarget::File, level)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_oplocks_break_through_one_dispatcher() -> io::Result<()> {
    let _serial = test_lock().await;
    timeout(CASE_TIMEOUT, async {
        let directory = TestDirectory::new("breaks")?;
        let paths = directory.files(CONCURRENT_OPLOCKS)?;
        let runtime = OplockRuntime::new()?;
        let mut oplocks = JoinSet::new();
        let mut ready = Vec::with_capacity(paths.len());

        for path in paths.iter().cloned() {
            let runtime = runtime.clone();
            let (ready_tx, ready_rx) = oneshot::channel();
            ready.push(ready_rx);
            oplocks.spawn(async move {
                runtime
                    .run(path, options(OplockLevel::ReadHandle), async |_file| {
                        let _ = ready_tx.send(());
                        pending::<io::Result<()>>().await
                    })
                    .await
            });
        }

        for receiver in ready {
            receiver
                .await
                .map_err(|_| io::Error::other("oplock did not become ready"))?;
        }

        let mut writers = JoinSet::new();
        for path in paths {
            writers.spawn(open_for_write(path));
        }

        let mut breaks = 0;
        while let Some(result) = oplocks.join_next().await {
            match result.map_err(io::Error::other)?? {
                OplockOutcome::Broken { guard, .. } => {
                    breaks += 1;
                    guard.close().await?;
                }
                OplockOutcome::Completed(()) => {
                    return Err(io::Error::other("work completed without an oplock break"));
                }
            }
        }

        while let Some(result) = writers.join_next().await {
            result.map_err(io::Error::other)??;
        }
        assert_eq!(breaks, CONCURRENT_OPLOCKS);
        runtime.shutdown().await?;
        Ok(())
    })
    .await
    .map_err(|_| io::Error::other("concurrent break stress test timed out"))?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completed_work_cancels_all_pending_oplocks() -> io::Result<()> {
    let _serial = test_lock().await;
    timeout(CASE_TIMEOUT, async {
        let directory = TestDirectory::new("cancellation")?;
        let paths = directory.files(CONCURRENT_OPLOCKS)?;
        let runtime = OplockRuntime::new()?;
        let mut tasks = JoinSet::new();

        for path in paths {
            let runtime = runtime.clone();
            tasks.spawn(async move {
                runtime
                    .run(path, options(OplockLevel::ReadHandle), async |_file| {
                        Ok::<_, io::Error>(42_u32)
                    })
                    .await
            });
        }

        let mut completed = 0;
        while let Some(result) = tasks.join_next().await {
            match result.map_err(io::Error::other)?? {
                OplockOutcome::Completed(42) => completed += 1,
                OplockOutcome::Completed(value) => {
                    return Err(io::Error::other(format!("unexpected value {value}")));
                }
                OplockOutcome::Broken { guard, .. } => {
                    guard.close().await?;
                    return Err(io::Error::other("oplock unexpectedly broke"));
                }
            }
        }
        assert_eq!(completed, CONCURRENT_OPLOCKS);
        runtime.shutdown().await?;
        Ok(())
    })
    .await
    .map_err(|_| io::Error::other("cancellation stress test timed out"))?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_windows_7_levels_complete() -> io::Result<()> {
    let _serial = test_lock().await;
    let directory = TestDirectory::new("levels")?;
    let path = directory.files(1)?.remove(0);
    let runtime = OplockRuntime::new()?;

    for level in [
        OplockLevel::Read,
        OplockLevel::ReadHandle,
        OplockLevel::ReadWrite,
        OplockLevel::ReadWriteHandle,
    ] {
        let outcome = runtime
            .run(&path, options(level), async |_file| {
                Ok::<_, io::Error>(level)
            })
            .await?;
        assert!(matches!(outcome, OplockOutcome::Completed(actual) if actual == level));
    }

    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owned_positional_io_round_trips() -> io::Result<()> {
    let _serial = test_lock().await;
    let directory = TestDirectory::new("owned-io")?;
    let path = directory.files(1)?.remove(0);
    let runtime = OplockRuntime::new()?;
    let expected = b"safe owned buffer".to_vec();

    let outcome = runtime
        .run(&path, options(OplockLevel::ReadWrite), async |file| {
            let (written, write_buffer) = file.write_at(expected.clone(), 7).await;
            assert_eq!(written?, write_buffer.len());
            file.sync_data().await?;

            let (read, read_buffer) = file.read_at(vec![0; write_buffer.len()], 7).await;
            let read = read?;
            Ok::<_, OplockError>(read_buffer[..read].to_vec())
        })
        .await?;

    assert!(matches!(outcome, OplockOutcome::Completed(actual) if actual == expected));
    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_level_rejects_writes_without_losing_the_buffer() -> io::Result<()> {
    let _serial = test_lock().await;
    let directory = TestDirectory::new("read-only-write")?;
    let path = directory.files(1)?.remove(0);
    let runtime = OplockRuntime::new()?;
    let payload = b"must be returned".to_vec();

    let outcome = runtime
        .run(path, options(OplockLevel::ReadHandle), async |file| {
            let (result, returned) = file.write_at(payload.clone(), 0).await;
            let error = result.expect_err("RH must reject protected writes");
            assert!(matches!(
                error,
                OplockError::Operation {
                    kind: tokio_oplock::OperationKind::Write,
                    ..
                }
            ));
            Ok::<_, OplockError>(returned)
        })
        .await?;
    assert!(matches!(outcome, OplockOutcome::Completed(returned) if returned == payload));

    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn directory_validation_and_supported_levels() -> io::Result<()> {
    let _serial = test_lock().await;
    let directory = TestDirectory::new("directory")?;
    let runtime = OplockRuntime::new()?;

    for level in [OplockLevel::Read, OplockLevel::ReadHandle] {
        let outcome = runtime
            .run(
                &directory.0,
                OplockOptions::new(OplockTarget::Directory, level),
                async |file| Ok::<_, OplockError>(file.metadata().await?.is_dir()),
            )
            .await?;
        assert!(matches!(outcome, OplockOutcome::Completed(true)));
    }

    let error = runtime
        .run(
            &directory.0,
            OplockOptions::new(OplockTarget::Directory, OplockLevel::ReadWrite),
            async |_file| Ok::<_, io::Error>(()),
        )
        .await
        .expect_err("directory RW oplock must be rejected before opening");
    assert!(matches!(error, OplockError::InvalidOptions { .. }));

    let error = runtime
        .run(
            directory.0.join("not-opened"),
            options(OplockLevel::Read).with_share_mode(ShareMode::from_bits_retain(0x8000_0000)),
            async |_file| Ok::<_, io::Error>(()),
        )
        .await
        .expect_err("unknown share flags must be rejected before opening");
    assert!(matches!(error, OplockError::InvalidOptions { .. }));

    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owned_std_file_entry_point_works() -> io::Result<()> {
    let _serial = test_lock().await;
    let directory = TestDirectory::new("owned-file")?;
    let path = directory.files(1)?.remove(0);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(ShareMode::all().bits())
        .custom_flags(FILE_FLAG_OVERLAPPED)
        .open(path)?;
    let runtime = OplockRuntime::new()?;

    let outcome = runtime
        .run_file(file, options(OplockLevel::ReadHandle), async |file| {
            Ok::<_, OplockError>(file.metadata().await?.len())
        })
        .await?;
    assert!(matches!(outcome, OplockOutcome::Completed(length) if length > 0));

    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protected_handle_can_be_duplicated() -> io::Result<()> {
    let _serial = test_lock().await;
    let directory = TestDirectory::new("duplicate-handle")?;
    let path = directory.files(1)?.remove(0);
    let runtime = OplockRuntime::new()?;

    let outcome = runtime
        .run(&path, options(OplockLevel::ReadHandle), async |file| {
            // SAFETY: the duplicate is only queried for synchronous metadata
            // and closed; it is never used to submit overlapped I/O.
            unsafe { file.try_clone_handle() }
        })
        .await?;

    let duplicate = match outcome {
        OplockOutcome::Completed(handle) => std::fs::File::from(handle),
        OplockOutcome::Broken { guard, .. } => {
            guard.close().await?;
            return Err(io::Error::other("oplock unexpectedly broke"));
        }
    };
    assert!(duplicate.metadata()?.len() > 0);

    drop(duplicate);
    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn independent_runtimes_can_coexist() -> io::Result<()> {
    let _serial = test_lock().await;
    let directory = TestDirectory::new("independent")?;
    let paths = directory.files(2)?;
    let first = OplockRuntime::new()?;
    let second = OplockRuntime::new()?;

    let (left, right) = tokio::join!(
        first.run(&paths[0], options(OplockLevel::ReadHandle), async |_file| {
            Ok::<_, io::Error>(1)
        },),
        second.run(&paths[1], options(OplockLevel::ReadHandle), async |_file| {
            Ok::<_, io::Error>(2)
        },),
    );
    assert!(matches!(left?, OplockOutcome::Completed(1)));
    assert!(matches!(right?, OplockOutcome::Completed(2)));

    first.shutdown().await?;
    second.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_repeatedly_starts_and_joins() -> io::Result<()> {
    let _serial = test_lock().await;
    timeout(CASE_TIMEOUT, async {
        let directory = TestDirectory::new("restart")?;
        let path = directory.files(1)?.remove(0);

        for expected in 0..RESTART_CYCLES {
            let runtime = OplockRuntime::new()?;
            let outcome = runtime
                .run(&path, options(OplockLevel::ReadHandle), async |_file| {
                    Ok::<_, io::Error>(expected)
                })
                .await?;
            assert!(matches!(outcome, OplockOutcome::Completed(actual) if actual == expected));
            runtime.shutdown().await?;
        }
        Ok(())
    })
    .await
    .map_err(|_| io::Error::other("runtime restart stress test timed out"))?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_shutdown_drains_a_pending_oplock() -> io::Result<()> {
    let _serial = test_lock().await;
    timeout(CASE_TIMEOUT, async {
        let directory = TestDirectory::new("pending-shutdown")?;
        let path = directory.files(1)?.remove(0);
        let runtime = OplockRuntime::new()?;
        let runner = runtime.clone();
        let (ready_tx, ready_rx) = oneshot::channel();
        let oplock = tokio::spawn(async move {
            runner
                .run(path, options(OplockLevel::ReadHandle), async |_file| {
                    let _ = ready_tx.send(());
                    pending::<io::Result<()>>().await
                })
                .await
        });

        ready_rx
            .await
            .map_err(|_| io::Error::other("pending oplock did not become ready"))?;
        runtime.shutdown().await?;
        assert!(oplock.await.map_err(io::Error::other)?.is_err());
        Ok(())
    })
    .await
    .map_err(|_| io::Error::other("pending runtime-shutdown test timed out"))?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborting_a_run_future_cancels_and_drains() -> io::Result<()> {
    let _serial = test_lock().await;
    timeout(CASE_TIMEOUT, async {
        let directory = TestDirectory::new("aborted-run")?;
        let path = directory.files(1)?.remove(0);
        let runtime = OplockRuntime::new()?;
        let runner = runtime.clone();
        let (ready_tx, ready_rx) = oneshot::channel();
        let oplock = tokio::spawn(async move {
            runner
                .run(path, options(OplockLevel::ReadHandle), async |_file| {
                    let _ = ready_tx.send(());
                    pending::<io::Result<()>>().await
                })
                .await
        });

        ready_rx
            .await
            .map_err(|_| io::Error::other("aborted oplock did not become ready"))?;
        oplock.abort();
        let _ = oplock.await;
        runtime.shutdown().await?;
        Ok(())
    })
    .await
    .map_err(|_| io::Error::other("aborted run did not drain"))?
}

async fn open_for_write(path: impl AsRef<Path>) -> io::Result<()> {
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .await?;
    drop(file);
    Ok(())
}

async fn test_lock() -> MutexGuard<'static, ()> {
    TEST_LOCK.lock().await
}
