# tokio-oplock

Safe, runtime-scoped asynchronous Windows oplocks for Tokio applications.

`tokio-oplock` lets an async task operate on a file while Windows watches for
conflicting access. The crate races the task against the oplock notification,
cancels in-flight work safely, and retains ownership of the protected handle
until the caller explicitly releases a break guard.

## Status and platform scope

Version `0.2` is a breaking, production-oriented redesign. It uses only the
modern `FSCTL_REQUEST_OPLOCK` protocol introduced in Windows 7 and supports all
four modern caching combinations:

| Rust level | Windows level | Files | Directories |
| --- | --- | --- | --- |
| `Read` | R | yes | yes |
| `ReadHandle` | RH | yes | yes |
| `ReadWrite` | RW | yes | no |
| `ReadWriteHandle` | RWH | yes | no |

“Windows 7 oplocks” describes the protocol, not the minimum operating-system
version. The crate deliberately does not use the legacy Windows Vista/XP
`FSCTL_REQUEST_OPLOCK_LEVEL_*` control codes. Current Rust Windows targets
require Windows 10 or Windows Server 2016 or newer.

The implementation has stress coverage for local Windows files, IOCP
completion and cancellation races, simultaneous runtimes, explicit shutdown,
external conflicting opens, every sharing mask, directories, and positional
data I/O. Remote shares and unusual filesystem drivers still require
environment-specific qualification before deployment.

## Installation

Until `0.2` is published, depend on the repository directly:

```toml
[target.'cfg(windows)'.dependencies]
tokio-oplock = { git = "https://github.com/amit-zimbalist/tokio-oplock.git" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

The Cargo package is `tokio-oplock`; Rust code imports it as `tokio_oplock`.

## Quick start

```rust
use tokio_oplock::{
    OplockError, OplockLevel, OplockOptions, OplockOutcome,
    OplockRuntime, OplockTarget,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = OplockRuntime::new()?;
    let options = OplockOptions::new(
        OplockTarget::File,
        OplockLevel::ReadHandle,
    );

    let outcome = runtime
        .run(r"C:\data\sample.bin", options, async |file| {
            let metadata = file.metadata().await?;
            Ok::<_, OplockError>(metadata.len())
        })
        .await?;

    match outcome {
        OplockOutcome::Completed(length) => {
            println!("Stable scan completed: {length} bytes");
        }
        OplockOutcome::Broken { info, guard } => {
            eprintln!(
                "Oplock broke from {:?} to {:?}; flags={:?}",
                info.original_level,
                info.new_level,
                info.flags,
            );
            guard.close().await?;
        }
    }

    runtime.shutdown().await?;
    Ok(())
}
```

Async closures (`async |file| { ... }`) can borrow `OplockFile` across awaits
without making `OplockFile` itself cloneable.

## The 0.2 design

1. **Modern, explicit oplock requests.** `OplockLevel` represents R, RH, RW,
   and RWH directly. `OplockTarget` makes file-versus-directory behavior
   explicit, and invalid directory write-caching requests fail before opening a
   handle. `ShareMode` controls path-open sharing.

2. **Runtime ownership instead of global state.** Every `OplockRuntime::new()`
   creates an independent completion port and dispatcher thread. Runtimes can
   coexist. `shutdown().await` stops new work, cancels and closes every session,
   drains queued completions, joins the dispatcher, and reports collected
   `RuntimeIssue` values through `ShutdownError`.

3. **A capability-safe protected handle.** Work receives a borrowed,
   non-cloneable `OplockFile`. It exposes metadata, flush, and positional
   `read_at`/`write_at` operations. An explicit unsafe `try_clone_handle` escape
   hatch returns an independently owned Windows handle for inheritance or
   cross-process duplication. Data operations own their `Vec<u8>` and return
   it on completion, so caller memory cannot be invalidated by async
   cancellation. Writes require an RW or RWH request.

4. **Defined break and cancellation behavior.** Work is raced against the
   oplock request. If work finishes first, the request is specifically
   cancelled and its IOCP completion is drained before the handle closes. If a
   break wins—or both futures become ready together—the work future is dropped and a close-only
   `OplockBreakGuard` is returned. Calling `guard.close().await` releases any
   required acknowledgement by closing the handle. Dropping a guard or runtime
   starts best-effort cleanup, but explicit close and shutdown are preferred
   when errors matter.

5. **Operational diagnostics and release hygiene.** Open, association,
   request, operation, cancellation, close, and dispatcher failures have
   structured `OplockError` variants. The package declares its Rust version,
   Windows documentation target, repository metadata, and MIT license. Tests
   cover real Windows behavior rather than only type-level mocks.

## Opening targets

`OplockRuntime::run` is the preferred entry point. It opens the path with
`FILE_FLAG_OVERLAPPED | FILE_FLAG_OPEN_REQUIRING_OPLOCK`, so a conflicting open
cannot slip between the crate's open and oplock request.

`OplockRuntime::run_file` takes ownership of a `std::fs::File` for integration
with code that already opens handles. That handle must have been opened for
overlapped I/O. This entry point cannot retroactively eliminate the race between
the caller's open and the oplock request.

Directories require `OplockTarget::Directory`; the crate adds
`FILE_FLAG_BACKUP_SEMANTICS` and accepts only R or RH levels.

## Protected I/O

Reads and writes are positional and return both their result and the original
owned buffer:

```rust
let (result, buffer) = file.read_at(vec![0; 4096], 0).await;
let bytes_read = result?;
process(&buffer[..bytes_read]);
```

For writes, select `ReadWrite` or `ReadWriteHandle`:

```rust
let (result, buffer) = file.write_at(payload, offset).await;
result?;
file.sync_data().await?;
drop(buffer);
```

The work future must still be cancellation-safe: do not rely on statements
after an `.await` always executing. Borrowed buffers cannot escape, but the API
cannot make unrelated external side effects transactional.

## Duplicating the protected handle

`OplockFile::try_clone_handle` returns an `OwnedHandle` in the current process:

```rust
// SAFETY: the recipient only retains and closes the duplicate. It does not
// issue I/O through the IOCP-associated handle.
let duplicate = unsafe { file.try_clone_handle()? };
```

The caller owns the duplicate and may arrange for a child process to inherit it
or use the Windows `DuplicateHandle` API to copy it into another process. The
oplock runtime does not track or close duplicates. Because they refer to the
same underlying file object, duplicates can keep that object and its IOCP
association alive after the callback or break guard is gone. The caller must
close every duplicate, including copies in other processes.

The method is unsafe because IOCP association is shared by duplicated file
handles. The caller must ensure that no operation through a duplicate can queue
a completion packet not created by this crate. Transferring, retaining, and
closing the duplicate without issuing I/O through it satisfies this contract.

## Break information

`OplockBreak` reports:

- raw and recognized original and replacement `OplockLevel` values;
- retained `OplockBreakFlags`, including acknowledgement and mode flags;
- `ack_required` for convenient policy checks;
- breaking access and share modes when Windows provides them.

This crate intentionally uses a close-only break policy. It does not issue a
downgrade acknowledgement and then resume work under a weaker caching level.

## Examples

```powershell
cargo run --example basic -- C:\path\to\file
cargo run --example metadata_scan -- C:\path\to\file
cargo run --example concurrent_metadata -- C:\first C:\second
cargo run --example retry_on_break -- C:\path\to\file
```

- `basic` waits for a conflicting open and demonstrates delayed guard release.
- `metadata_scan` is the smallest useful protected operation.
- `concurrent_metadata` uses one runtime for simultaneous independent files.
- `retry_on_break` demonstrates cancellation-safe bounded retries.
- `field_stress` drives cross-process open/share matrices and shutdown probes.

## Safety and limitations

- Windows only; the public oplock module is absent on other targets.
- A conflicting application may remain blocked until work cancellation drains
  and the break guard closes. That delay is fundamental to oplocks.
- An existing handle that denies this crate's requested sharing can make the
  atomic open fail with a sharing violation. The existing application is not
  disturbed.
- Oplock availability and semantics can vary on SMB shares and third-party
  filesystems. Qualify the exact storage stack used in production.
- `run_file` trusts the caller to provide an overlapped handle and cannot offer
  the atomic-open guarantee of `run`.
- Unexpected I/O errors are operational failures. Retry only when the
  application can safely repeat the complete protected operation.

## Validation

Run the normal release checks on Windows:

```powershell
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo doc --locked --no-deps
```

Run cross-process probes and the continuous scenario matrix:

```powershell
cargo run --release --example field_stress -- --probe-runtime-shutdown 3
cargo run --release --example field_stress -- --probe-preexisting
cargo run --release --example field_stress -- --duration-secs 600
```

For release qualification, run the release harness under Application Verifier
Basics and Full Page Heap. Repeat it against every supported Windows release,
filesystem, antivirus/minifilter configuration, and remote-share topology that
the application will deploy.

## License

Licensed under the [MIT License](LICENSE).
