#[cfg(windows)]
pub mod oplock;

#[cfg(windows)]
pub use oplock::OplockRuntime;
