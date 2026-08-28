#[cfg(not(windows))]
fn main() {
    eprintln!("This example requires Windows.");
}

#[cfg(windows)]
use std::{env, io, path::PathBuf};

#[cfg(windows)]
use tokio::task::JoinSet;
#[cfg(windows)]
use tokio_oplock::{OplockLevel, OplockOptions, OplockOutcome, OplockRuntime, OplockTarget};

#[cfg(windows)]
#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let paths = env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: concurrent_metadata <path> [path ...]",
        )
        .into());
    }

    let runtime = OplockRuntime::new()?;
    let mut scans = JoinSet::new();

    for path in paths {
        let runtime = runtime.clone();
        scans.spawn(async move {
            let display_path = path.clone();
            let outcome = runtime
                .run(
                    path,
                    OplockOptions::new(OplockTarget::File, OplockLevel::ReadHandle),
                    async |file| Ok::<_, tokio_oplock::OplockError>(file.metadata().await?.len()),
                )
                .await?;
            Ok::<_, tokio_oplock::OplockError>((display_path, outcome))
        });
    }

    while let Some(result) = scans.join_next().await {
        let (path, outcome) = result.map_err(io::Error::other)??;
        match outcome {
            OplockOutcome::Completed(length) => {
                println!("{}: {length} bytes", path.display());
            }
            OplockOutcome::Broken { info, guard } => {
                eprintln!(
                    "{} interrupted: {:?} -> {:?}",
                    path.display(),
                    info.original_level,
                    info.new_level
                );
                guard.close().await?;
            }
        }
    }

    runtime.shutdown().await?;
    Ok(())
}
