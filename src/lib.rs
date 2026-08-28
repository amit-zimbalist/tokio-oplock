#![deny(missing_docs)]

//! Runtime-scoped asynchronous Windows 7 oplocks.
//!
//! This crate uses the modern `FSCTL_REQUEST_OPLOCK` protocol introduced in
//! Windows 7. It does not use the legacy Windows Vista/XP oplock control codes.

#[cfg(windows)]
/// Windows oplock types, runtime, and protected file operations.
pub mod oplock;

#[cfg(windows)]
pub use oplock::{
    BufferResult, OperationKind, OplockBreak, OplockBreakFlags, OplockBreakGuard, OplockError,
    OplockFile, OplockLevel, OplockOptions, OplockOutcome, OplockRuntime, OplockTarget,
    RuntimeIssue, ShareMode, ShutdownError,
};
