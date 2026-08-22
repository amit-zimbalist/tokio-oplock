#![cfg(windows)]

use std::{
    future::pending,
    io,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use oplocks::oplock::{Oplock, OplockOutcome, OplockRuntime};
use tokio::{
    sync::{Mutex, MutexGuard, oneshot},
    task::JoinSet,
    time::timeout,
};

const CASE_TIMEOUT: Duration = Duration::from_secs(30);
const CONCURRENT_OPLOCKS: usize = 64;
const RESTART_CYCLES: usize = 16;

// Only one process-wide oplock runtime may exist. Cargo can execute tests in
// this binary concurrently, so serialize the cases explicitly.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_oplocks_break_through_one_dispatcher() -> io::Result<()> {
    let _serial = test_lock().await;
    timeout(CASE_TIMEOUT, async {
        let directory = TestDirectory::new("breaks")?;
        let paths = directory.files(CONCURRENT_OPLOCKS)?;
        let _runtime = OplockRuntime::new()?;
        let mut oplocks = JoinSet::new();
        let mut ready = Vec::with_capacity(paths.len());

        for path in paths.iter().cloned() {
            let (ready_tx, ready_rx) = oneshot::channel();
            ready.push(ready_rx);
            oplocks.spawn(async move {
                Oplock::run(path, |_file| async move {
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
                    drop(guard);
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
        let _runtime = OplockRuntime::new()?;
        let mut tasks = JoinSet::new();

        for path in paths {
            tasks.spawn(async move { Oplock::run(path, |_file| async { Ok(42_u32) }).await });
        }

        let mut completed = 0;
        while let Some(result) = tasks.join_next().await {
            match result.map_err(io::Error::other)?? {
                OplockOutcome::Completed(42) => completed += 1,
                OplockOutcome::Completed(value) => {
                    return Err(io::Error::other(format!("unexpected value {value}")));
                }
                OplockOutcome::Broken { .. } => {
                    return Err(io::Error::other("oplock unexpectedly broke"));
                }
            }
        }
        assert_eq!(completed, CONCURRENT_OPLOCKS);
        Ok(())
    })
    .await
    .map_err(|_| io::Error::other("cancellation stress test timed out"))?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_repeatedly_starts_and_joins() -> io::Result<()> {
    let _serial = test_lock().await;
    timeout(CASE_TIMEOUT, async {
        let directory = TestDirectory::new("restart")?;
        let path = directory.files(1)?.remove(0);

        for expected in 0..RESTART_CYCLES {
            let runtime = OplockRuntime::new()?;
            let outcome = Oplock::run(&path, |_file| async move { Ok(expected) }).await?;
            match outcome {
                OplockOutcome::Completed(actual) => assert_eq!(actual, expected),
                OplockOutcome::Broken { .. } => {
                    return Err(io::Error::other("oplock unexpectedly broke"));
                }
            }
            drop(runtime);
        }
        Ok(())
    })
    .await
    .map_err(|_| io::Error::other("runtime restart stress test timed out"))?
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
