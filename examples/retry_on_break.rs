#[cfg(not(windows))]
fn main() {
    eprintln!("This example requires Windows.");
}

#[cfg(windows)]
use std::{env, io, path::PathBuf, time::Duration};

#[cfg(windows)]
use tokio::time::sleep;
#[cfg(windows)]
use tokio_oplock::{
    OplockError, OplockFile, OplockLevel, OplockOptions, OplockOutcome, OplockRuntime, OplockTarget,
};

#[cfg(windows)]
const MAX_ATTEMPTS: u32 = 5;

#[cfg(windows)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = argument_path("usage: retry_on_break <path>")?;
    let runtime = OplockRuntime::new()?;
    let options = OplockOptions::new(OplockTarget::File, OplockLevel::ReadHandle);

    for attempt in 1..=MAX_ATTEMPTS {
        match runtime.run(&path, options, scan).await? {
            OplockOutcome::Completed(bytes) => {
                println!("stable metadata scan completed on attempt {attempt}: {bytes} bytes");
                runtime.shutdown().await?;
                return Ok(());
            }
            OplockOutcome::Broken { info, guard } => {
                eprintln!(
                    "attempt {attempt} interrupted: {:?} -> {:?}",
                    info.original_level, info.new_level
                );
                guard.close().await?;
                if attempt < MAX_ATTEMPTS {
                    sleep(Duration::from_millis(u64::from(attempt) * 50)).await;
                }
            }
        }
    }

    runtime.shutdown().await?;
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        format!("file changed during all {MAX_ATTEMPTS} scan attempts"),
    )
    .into())
}

#[cfg(windows)]
async fn scan(file: &OplockFile) -> Result<u64, OplockError> {
    let original = file.metadata().await?;
    for _ in 0..20 {
        // Simulate asynchronous analysis while remaining cancellation-safe.
        sleep(Duration::from_millis(10)).await;
        let current = file.metadata().await?;
        if current.len() != original.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file length changed during metadata scan",
            )
            .into());
        }
    }
    Ok(original.len())
}

#[cfg(windows)]
fn argument_path(usage: &'static str) -> io::Result<PathBuf> {
    env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, usage))
}
