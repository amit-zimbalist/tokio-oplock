use std::{future::Future, io, path::Path, sync::Arc};

use super::win32::{self, RawBreakReceiver, RawOplockGuard, RawRuntime};

#[derive(Debug, Clone, Copy)]
pub struct OplockBreak {
    pub original_level: u32,
    pub new_level: u32,
    pub flags: u32,
    pub ack_required: bool,
}

pub struct OplockGuard {
    inner: RawOplockGuard,
}

impl OplockGuard {
    fn file(&self) -> Arc<tokio::fs::File> {
        self.inner.file()
    }
}

struct OplockBreakReceiver {
    inner: RawBreakReceiver,
}

impl OplockBreakReceiver {
    async fn wait(self) -> io::Result<OplockBreak> {
        let output = self.inner.wait_for_break().await?;
        Ok(OplockBreak {
            original_level: output.original_level,
            new_level: output.new_level,
            flags: output.flags,
            ack_required: output.ack_required,
        })
    }
}

pub enum OplockOutcome<T> {
    Completed(T),
    Broken {
        info: OplockBreak,
        guard: OplockGuard,
    },
}

pub struct OplockRuntime {
    _inner: RawRuntime,
}

impl OplockRuntime {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            _inner: RawRuntime::new()?,
        })
    }
}

pub struct Oplock;

impl Oplock {
    async fn request(path: impl AsRef<Path>) -> io::Result<(OplockGuard, OplockBreakReceiver)> {
        let mut options = tokio::fs::OpenOptions::new();
        options.read(true).custom_flags(win32::OVERLAPPED_FILE_FLAG);
        let file = options.open(path).await?;
        let (guard, breaks) = win32::request(file)?;

        Ok((
            OplockGuard { inner: guard },
            OplockBreakReceiver { inner: breaks },
        ))
    }

    pub async fn run<T, F, Fut>(path: impl AsRef<Path>, work: F) -> io::Result<OplockOutcome<T>>
    where
        F: FnOnce(Arc<tokio::fs::File>) -> Fut,
        Fut: Future<Output = io::Result<T>>,
    {
        let (guard, breaks) = Self::request(path).await?;
        let work = work(guard.file());
        tokio::pin!(work);

        tokio::select! {
            result = &mut work => Ok(OplockOutcome::Completed(result?)),
            broken = breaks.wait() => Ok(OplockOutcome::Broken {
                info: broken?,
                guard,
            }),
        }
    }
}
