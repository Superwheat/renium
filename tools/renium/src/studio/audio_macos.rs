use super::{Action, Status};
use anyhow::Result;

pub(super) struct Backend {
    pid: u32,
}

impl Backend {
    pub(super) fn new(pid: u32, _dir: &std::path::Path) -> Result<Self> {
        Ok(Self { pid })
    }

    pub(super) fn step(&mut self, action: Action) -> Result<Status> {
        crate::studio::native::serializer::studio_audio(self.pid, action)
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.step(Action::Off);
    }
}
