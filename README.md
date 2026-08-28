# tokio-oplock

Safe asynchronous Windows oplock coordination for Tokio applications.

`tokio-oplock` lets a task perform work against a file while Windows watches for a conflicting open. If another application needs incompatible access, the work future is cancelled and the caller receives the oplock-break information plus the file guard that must be released promptly.

## Status

This is a Windows-only `0.1` crate. Its local-file behavior has been stress-tested for ten continuous minutes under Windows Application Verifier Full Page Heap, including external processes, every Windows share-mask combination, cancellation races, self-opens, and runtime shutdown. See [Safety and limitations](#safety-and-limitations) before deploying it.

## Installation

Until you choose to publish a release, depend on the repository directly:

```toml
[target.'cfg(windows)'.dependencies]
tokio-oplock = { git = "https://github.com/amit-zimbalist/tokio-oplock.git" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

The Cargo package name is `tokio-oplock`; Rust code imports it as `tokio_oplock`.

## Quick start

```rust
use std::{io, path::PathBuf};

use tokio_oplock::{
    OplockRuntime,
    oplock::{Oplock, OplockOutcome},
};

#[tokio::main]
async fn main() -> io::Result<()> {
    let path = PathBuf::from(r"C:\data\sample.bin");
    let _runtime = OplockRuntime::new()?;

    let outcome = Oplock::run(&path, |file| async move {
        let metadata = file.metadata().await?;
        Ok(metadata.len())
    })
    .await?;

    match outcome {
        OplockOutcome::Completed(length) => {
            println!("Stable scan completed; file length is {length} bytes");
        }
        OplockOutcome::Broken { info, guard } => {
            eprintln!(
                "Scan cancelled by oplock break: {:#x} -> {:#x}",
                info.original_level, info.new_level
            );
            drop(guard); // Release the file handle so the other application can continue.
        }
    }

    Ok(())
}
```

Keep `OplockRuntime` alive while requests are being created. Only one runtime accepts requests at a time; after shutdown begins, a replacement runtime can start while already-submitted completions drain safely.

## Break semantics

`Oplock::run` requests a read/handle-caching oplock and races two futures:

- The supplied work future. If it wins, the result is `OplockOutcome::Completed(T)` and the pending oplock request is cancelled.
- The Windows oplock completion. If it wins, the work future is dropped and the result is `OplockOutcome::Broken { info, guard }`.

The work future must therefore be cancellation-safe: do not rely on code after an `.await` always running, and avoid detached or non-cancellable work that continues using the file after the future is dropped.

On a break, release `guard` as soon as your cleanup is complete. Holding it can intentionally delay the conflicting application. The `ack_required` field reports whether Windows requested acknowledgement; this crate releases the request by closing the guarded handle.

## Examples

```powershell
cargo run --example basic -- C:\path\to\file
cargo run --example metadata_scan -- C:\path\to\file
cargo run --example concurrent_metadata -- C:\path\to\first C:\path\to\second
cargo run --example retry_on_break -- C:\path\to\file
```

- `basic` waits for a conflict and deliberately holds the returned guard for five seconds to demonstrate the external application's delay.
- `metadata_scan` shows the smallest useful protected operation.
- `concurrent_metadata` protects metadata work on several files through one process-wide runtime.
- `retry_on_break` shows cancellation-safe metadata work and bounded retries after conflicts.
- `field_stress` is the diagnostic harness used for the cross-process share/open matrix and shutdown probes.

## Safety and limitations

- Windows only. Validation was performed on local files; remote shares and uncommon filesystem drivers can implement or reject oplocks differently.
- The crate's own handle requests read access and broad sharing. If an application already holds the file without `FILE_SHARE_READ`, Windows must reject the crate's open with a sharing violation; the existing application is not disturbed.
- A conflicting application may be delayed until your work is cancelled and the returned guard is dropped. This timing effect is fundamental to oplocks.
- The file passed to the work closure is an overlapped handle intended for the oplock request and handle/metadata operations. Tokio's regular-file `AsyncReadExt` currently performs synchronous reads internally and returns `ERROR_INVALID_PARAMETER` on this handle. Do not submit unrelated overlapped I/O on it either, because the handle is associated with the crate's private IOCP. General content scanning needs a dedicated safe read API that this `0.1` interface does not yet provide.
- Dropping `OplockRuntime` stops that dispatcher from accepting requests but does not block on active requests. Their IOCP completions drain in the background.
- The implementation retains every `OVERLAPPED` allocation through its sole IOCP completion, including immediate-success and cancellation paths.
- Treat unexpected I/O errors as normal operational failures and retry only when that is appropriate for your application.

## Validation

Run the normal suite:

```powershell
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

Run the field probes and continuous matrix:

```powershell
cargo run --release --example field_stress -- --probe-runtime-drop 3
cargo run --release --example field_stress -- --probe-preexisting
cargo run --release --example field_stress -- --duration-secs 600
```

For release qualification, enable Application Verifier Basics with Full Heaps for `field_stress.exe` before running the continuous test.

## License

MIT
