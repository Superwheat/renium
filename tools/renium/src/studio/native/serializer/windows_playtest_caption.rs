//! Caption cleanup owned by the same Studio window and daemon process lifetime.
use super::*;

pub(crate) struct NativePlayCaption {
    memory: ProcessMemory,
    entrypoint: usize,
    request: Vec<u8>,
}
// Like the dispatch lease, this exclusively owns its process handle and buffer.
unsafe impl Send for NativePlayCaption {}

impl NativePlayCaption {
    pub(crate) fn arm(pid: u32, title: &str, hwnd: isize) -> Result<Self> {
        let loaded = modules(pid)?;
        let memory = ProcessMemory::open(pid)?;
        memory.authorize(NativeOperation::Input, title)?;
        let helper = ensure_helper_loaded(pid, &memory, &loaded)?;
        let mut request = vec![0; 8488];
        put_u32(&mut request, 0, 1);
        put_u32(&mut request, 4, 8488);
        let host = std::process::id();
        put_u32(&mut request, 12, host);
        let mut token = [0; 8];
        getrandom::fill(&mut token)
            .map_err(|e| anyhow::anyhow!("Cannot create caption owner: {e}"))?;
        put_u64(&mut request, 16, u64::from_le_bytes(token).max(1) as usize);
        put_u64(&mut request, 24, hwnd as usize);
        put_u64(
            &mut request,
            32,
            crate::app::update::process_start_identity(host)
                .context("Caption host ended")?
                .parse()?,
        );
        let mut lease = Self {
            memory,
            entrypoint: helper + helper_export_rva("ReniumPlayCaption")?,
            request,
        };
        lease.set_text(40, title)?;
        lease.invoke(1)?;
        Ok(lease)
    }

    fn set_text(&mut self, at: usize, text: &str) -> Result<()> {
        let wide = text.encode_utf16().collect::<Vec<_>>();
        anyhow::ensure!(
            wide.len() < 2048 && !wide.contains(&0),
            "Teleport caption is too long or contains a null"
        );
        self.request[at..at + 4096].fill(0);
        for (index, character) in wide.into_iter().enumerate() {
            self.request[at + index * 2..at + index * 2 + 2]
                .copy_from_slice(&character.to_le_bytes());
        }
        Ok(())
    }

    pub(crate) fn update(&mut self, ordinary: &str, written: Option<&str>) -> Result<()> {
        self.set_text(40, ordinary)?;
        self.set_text(4136, written.unwrap_or_default())?;
        self.invoke(2)
    }

    fn invoke(&mut self, action: u32) -> Result<()> {
        put_u32(&mut self.request, 8, action);
        self.request[8232..].fill(0);
        let mut input = self.memory.allocate(self.request.len())?;
        self.memory.write(input.address, &self.request)?;
        let exit = input.run(self.entrypoint, 3000)?;
        self.memory.read(input.address, &mut self.request)?;
        let error = error_text_at(&self.request, 8232);
        anyhow::ensure!(exit == 0 && error.is_empty(), "Teleport caption: {error}");
        Ok(())
    }
}
impl Drop for NativePlayCaption {
    fn drop(&mut self) {
        let _ = self.invoke(3);
    }
}
