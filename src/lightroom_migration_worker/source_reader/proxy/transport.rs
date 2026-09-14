//! Direct ownership remains explicit for the qualified core fixtures. Managed
//! migration constructors require a G token client and never fall back to spawn.
use super::*;
use crate::lightroom_migration_worker::source_reader::relay::client::Remote;
pub(super) enum Transport {
    Direct(Process<Reply>),
    Managed(Remote),
}
impl From<Process<Reply>> for Transport {
    fn from(value: Process<Reply>) -> Self {
        Self::Direct(value)
    }
}
impl From<Remote> for Transport {
    fn from(value: Remote) -> Self {
        Self::Managed(value)
    }
}
impl Transport {
    pub(super) fn admit_core(&self, bytes: usize) -> Result<()> {
        match self {
            Self::Direct(_) => Ok(()),
            Self::Managed(remote) => remote.admit_core(bytes),
        }
    }

    pub(super) fn admit_producer(&self, bytes: usize) -> Result<()> {
        match self {
            Self::Direct(_) => Ok(()),
            Self::Managed(remote) => remote.admit_producer(bytes),
        }
    }
    pub(super) fn admit_result(&self, transient: usize, graph: usize) -> Result<()> {
        match self {
            // Legacy direct ownership has its separately qualified path-grant
            // contract; managed aggregate admission never falls back to it.
            Self::Direct(_) => Ok(()),
            Self::Managed(remote) => remote.admit_result(transient, graph),
        }
    }
    pub(super) fn try_send(&self, request: Request) -> Result<Option<Request>> {
        match self {
            Self::Direct(p) => p.try_send(request),
            Self::Managed(p) => p.try_send(request),
        }
    }
    pub(super) fn try_receive(&self) -> Result<Output<Reply>> {
        match self {
            Self::Direct(p) => p.try_receive(),
            Self::Managed(p) => p.try_receive(),
        }
    }
    pub(super) fn try_reap(&mut self) -> Result<Option<()>> {
        match self {
            Self::Direct(p) => Ok(p.try_reap()?.map(|_| ())),
            Self::Managed(p) => Ok(p.reaped()?.then_some(())),
        }
    }
    pub(super) fn terminate(&mut self) -> Result<()> {
        match self {
            Self::Direct(p) => p.drain_checked(),
            Self::Managed(p) => p.terminate(),
        }
    }
    #[cfg(test)]
    pub(super) fn pid(&self) -> u32 {
        match self {
            Self::Direct(p) => p.pid(),
            Self::Managed(_) => panic!("G owns managed Source PIDs; inspect the test broker owner"),
        }
    }
}
