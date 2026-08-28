#[cfg(not(windows))]
fn main() {
    eprintln!("This example requires Windows.");
}

#[cfg(windows)]
use std::{env, io, path::PathBuf};

#[cfg(windows)]
use tokio::task::JoinSet;
#[cfg(windows)]
use tokio_oplock::{
    OplockRuntime,
    oplock::{Oplock, OplockOutcome},
};

#[cfg(windows)]
#[tokio::main(flavor = "multi_thread")]
async fn main() -> io::Result<()> {
    let paths = env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: concurrent_metadata <path> [path ...]",
        ));
    }

    let _runtime = OplockRuntime::new()?;
    let mut scans = JoinSet::new();

    for path in paths {
        scans.spawn(async move {
            let display_path = path.clone();
            let outcome = Oplock::run(path, |file| async move {
                let metadata = file.metadata().await?;
                Ok(metadata.len())
            })
            .await?;
            Ok::<_, io::Error>((display_path, outcome))
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
                    "{} interrupted: {:#x} -> {:#x}",
                    path.display(),
                    info.original_level,
                    info.new_level
                );
                drop(guard);
            }
        }
    }

    Ok(())
}
