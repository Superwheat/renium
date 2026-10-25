use super::*;

pub(crate) struct NativeMacPlayCaption {
    memory: Memory,
    request: Vec<u8>,
}

fn text(request: &mut [u8], at: usize, value: &str) -> Result<()> {
    anyhow::ensure!(
        value.len() < 1024 && !value.contains('\0'),
        "Studio caption exceeds its UTF-8 bound"
    );
    request[at..at + 1024].fill(0);
    request[at..at + value.len()].copy_from_slice(value.as_bytes());
    Ok(())
}

impl NativeMacPlayCaption {
    pub(crate) fn trigger_action(
        pid: u32,
        action: &str,
    ) -> Result<crate::studio::native::serializer::StudioActionOutcome> {
        anyhow::ensure!(
            !action.is_empty() && action.len() < 96 && action.is_ascii() && !action.contains('\0'),
            "Studio action names are short ASCII identifiers"
        );
        let (window, window_title) = Self::window(pid)?;
        let memory = Memory::for_process(pid, Duration::from_secs(10))?;
        let mut request = vec![0; 112];
        put32(&mut request, 0, 1);
        put32(&mut request, 4, 112);
        put32(&mut request, 8, u32::try_from(window)?);
        request[16..16 + action.len()].copy_from_slice(action.as_bytes());
        let response = memory.request(15, &request, "")?;
        anyhow::ensure!(
            response.len() == request.len(),
            "Invalid Studio action receipt"
        );
        Ok(crate::studio::native::serializer::StudioActionOutcome {
            window_title,
            found: read_u32(&response, 12).unwrap(),
        })
    }
    fn request(pid: u32) -> Result<Self> {
        let memory = Memory::for_process(pid, Duration::from_secs(5))?;
        let mut request = vec![0; 2096];
        put32(&mut request, 0, 1);
        put32(&mut request, 4, 2096);
        Ok(Self { memory, request })
    }

    fn invoke(&mut self, action: u32) -> Result<Vec<u8>> {
        self.memory.deadline = Instant::now() + Duration::from_secs(3);
        put32(&mut self.request, 8, action);
        let response = self.memory.request(11, &self.request, "")?;
        anyhow::ensure!(
            response.len() == self.request.len(),
            "Invalid Mac caption receipt"
        );
        Ok(response)
    }

    pub(crate) fn window(pid: u32) -> Result<(isize, String)> {
        let mut query = Self::request(pid)?;
        let response = query.invoke(1)?;
        let title = &response[48..1072];
        let length = title
            .iter()
            .position(|b| *b == 0)
            .context("Unterminated Mac caption")?;
        Ok((
            read_u32(&response, 12).unwrap() as isize,
            std::str::from_utf8(&title[..length])?.to_owned(),
        ))
    }

    pub(crate) fn arm(pid: u32, ordinary: &str, window: isize) -> Result<Self> {
        let mut lease = Self::request(pid)?;
        let mut token = [0; 8];
        getrandom::fill(&mut token).map_err(|e| anyhow::anyhow!("Cannot identify caption: {e}"))?;
        put64(&mut lease.request, 16, u64::from_le_bytes(token).max(1));
        put32(&mut lease.request, 12, u32::try_from(window)?);
        put32(&mut lease.request, 24, std::process::id());
        let (seconds, micros) = host_identity()?;
        put64(&mut lease.request, 32, seconds);
        put64(&mut lease.request, 40, micros);
        text(&mut lease.request, 48, ordinary)?;
        lease.invoke(2)?;
        Ok(lease)
    }

    pub(crate) fn update(&mut self, ordinary: &str, next: Option<&str>) -> Result<()> {
        text(&mut self.request, 48, ordinary)?;
        text(&mut self.request, 1072, next.unwrap_or_default())?;
        self.invoke(3).map(|_| ())
    }
}

impl Drop for NativeMacPlayCaption {
    fn drop(&mut self) {
        if read_u64(&self.request, 16).is_some_and(|token| token != 0) {
            let _ = self.invoke(4);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an explicitly owned Studio PID; invokes its real Play/Stop control"]
    fn installed_mac_studio_action() -> Result<()> {
        let pid = std::env::var("RENIUM_TELEPORT_TEST_PID")?.parse()?;
        let action = std::env::var("RENIUM_TELEPORT_TEST_ACTION")?;
        anyhow::ensure!(
            matches!(
                action.as_str(),
                "simulationPlayAction"
                    | "simulationResetAction"
                    | "startServerAction"
                    | "addClientsAction"
                    | "cleanupServersAndPlayersAction"
            ),
            "Not a playtest control"
        );
        let result = NativeMacPlayCaption::trigger_action(pid, &action)?;
        println!(
            "Studio control {action}: {} matching action(s)",
            result.found
        );
        Ok(())
    }
}
