use super::*;

pub(crate) struct NativeMacStudioAction {
    memory: Memory,
    request: Vec<u8>,
}

impl NativeMacStudioAction {
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

    fn window(pid: u32) -> Result<(isize, String)> {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an explicitly owned Studio PID; invokes its real Play/Stop control"]
    fn installed_mac_studio_action() -> Result<()> {
        let pid = std::env::var("RENIUM_STUDIO_ACTION_TEST_PID")?.parse()?;
        let action = std::env::var("RENIUM_STUDIO_ACTION_TEST_ACTION")?;
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
        let result = NativeMacStudioAction::trigger_action(pid, &action)?;
        println!(
            "Studio control {action}: {} matching action(s)",
            result.found
        );
        Ok(())
    }
}
