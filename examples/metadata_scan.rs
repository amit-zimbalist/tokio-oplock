#[cfg(not(windows))]
fn main() {
    eprintln!("This example requires Windows.");
}

#[cfg(windows)]
use std::{env, io, path::PathBuf, time::SystemTime};

#[cfg(windows)]
use tokio_oplock::{OplockLevel, OplockOptions, OplockOutcome, OplockRuntime, OplockTarget};

#[cfg(windows)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = argument_path("usage: metadata_scan <path>")?;
    let runtime = OplockRuntime::new()?;

    let outcome = runtime
        .run(
            &path,
            OplockOptions::new(OplockTarget::File, OplockLevel::ReadHandle),
            async |file| {
                let metadata = file.metadata().await?;
                Ok::<_, tokio_oplock::OplockError>((metadata.len(), metadata.modified().ok()))
            },
        )
        .await?;

    match outcome {
        OplockOutcome::Completed((length, modified)) => {
            println!("length={length} bytes modified={}", display_time(modified));
        }
        OplockOutcome::Broken { info, guard } => {
            eprintln!(
                "metadata scan cancelled by oplock break: {:?} -> {:?}",
                info.original_level, info.new_level
            );
            guard.close().await?;
        }
    }

    runtime.shutdown().await?;
    Ok(())
}

#[cfg(windows)]
fn argument_path(usage: &'static str) -> io::Result<PathBuf> {
    env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, usage))
}

#[cfg(windows)]
fn display_time(value: Option<SystemTime>) -> String {
    value
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| format!("{}s-since-epoch", duration.as_secs()))
        .unwrap_or_else(|| "unknown".into())
}
