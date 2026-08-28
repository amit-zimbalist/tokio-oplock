mod request;
mod win32;

pub use request::{
    BufferResult, OperationKind, OplockBreak, OplockBreakFlags, OplockBreakGuard, OplockError,
    OplockFile, OplockLevel, OplockOptions, OplockOutcome, OplockRuntime, OplockTarget,
    RuntimeIssue, ShareMode, ShutdownError,
};
