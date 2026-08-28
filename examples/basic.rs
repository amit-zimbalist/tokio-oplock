#[cfg(not(windows))]
fn main() {
    eprintln!("This example uses Windows oplocks and can only run on Windows.");
}

#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use tokio_oplock::{
    OplockRuntime,
    oplock::{Oplock, OplockOutcome},
};

#[cfg(windows)]
#[tokio::main]
async fn main() -> std::io::Result<()> {
    use std::{env, io, path::PathBuf};

    let path = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "usage: basic <path>"))?;

    let _runtime = OplockRuntime::new()?;
    let outcome = Oplock::run(&path, |_file| async {
        println!(
            "Oplock held on {}. Waiting for a conflicting open...",
            path.display()
        );
        tokio::signal::ctrl_c().await?;
        Ok(())
    })
    .await?;

    match outcome {
        OplockOutcome::Broken { info, guard } => {
            println!(
                "Oplock broken: original={:#x}, new={:#x}, flags={:#x}",
                info.original_level, info.new_level, info.flags
            );
            if info.ack_required {
                println!("An acknowledgement was required; closing the file handle releases it.");
            }

            println!("Holding the file handle for 5 more seconds before releasing the oplock...");
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(guard);
            println!("File handle released.");
        }
        OplockOutcome::Completed(()) => println!("Ctrl-C received; exiting."),
    }

    Ok(())
}
