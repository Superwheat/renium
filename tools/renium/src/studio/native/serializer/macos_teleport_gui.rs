//! Invoke Roblox's installed loading CoreScript on the client's own scheduler.
use super::*;

const ATTRIBUTE: &str =
    "N3RBX10Reflection13BoundFuncDescINS_8InstanceEKFNS0_7VariantENS_10StringViewEELb0ELi1EEE";
const ADD: &str = "N3RBX10Reflection13BoundFuncDescINS_13ScriptContextEFvNSt3__112basic_stringIcNS3_11char_traitsIcEENS3_9allocatorIcEEEENS3_10shared_ptrINS_8InstanceEEEELb0ELi2EEE";
const READY: &str = "N3RBX10Reflection13BoundFuncDescINS_15ReplicatedFirstEFbvELb0ELi0EEE";
const REMOVE: &str = "N3RBX10Reflection13BoundFuncDescINS_15ReplicatedFirstEFvvELb0ELi0EEE";

pub(crate) struct NativeMacTeleportGui {
    memory: Memory,
    request: Vec<u8>,
}

impl NativeMacTeleportGui {
    pub(crate) fn arm(
        pid: u32,
        runtime: &str,
        model_name: &str,
        path: &[String],
        show: bool,
    ) -> Result<Self> {
        anyhow::ensure!(
            path == ["CoreGui", &format!("__ReniumTeleportGui_{runtime}")],
            "Invalid Mac teleport GUI relay"
        );
        let mut memory = Memory::for_process(pid, Duration::from_secs(15))?;
        // The helper discovers Name storage from the actual DataModel name,
        // which can differ from its file/window title. An empty selector leaves
        // the name layout uninitialized on a freshly started Studio process.
        let contexts = memory.request(5, &[0], model_name)?;
        memory.base = read_u64(&contexts, 0).context("Missing Studio image base")?;
        let mut failures = Vec::new();
        let matches = contexts
            .chunks_exact(64)
            .filter_map(|context| match resolve_path(&memory, context, path, &[]) {
                Ok(path) => Some((context, path)),
                Err(error) => {
                    failures.push(format!("{error:#}"));
                    None
                }
            })
            .collect::<Vec<_>>();
        anyhow::ensure!(
            matches.len() == 1,
            "Teleport GUI relay resolves {} of {} models: {}",
            matches.len(),
            contexts.len() / 64,
            failures.join("; ")
        );
        let (context, relay_path) = &matches[0];
        let relay = *relay_path.last().context("Teleport GUI relay ended")?;
        let catalog = Catalog::open(&memory)?;
        let (attribute_slot, attribute) = catalog.member(&memory, ATTRIBUTE, "GetAttribute")?;
        let (_, add) = catalog.member(&memory, ADD, "AddCoreScriptLocal")?;
        let (_, remove) = catalog.member(&memory, REMOVE, "RemoveDefaultLoadingScreen")?;
        let ready = if show {
            [
                catalog.member(&memory, READY, "IsFinishedReplicating")?,
                catalog.member(&memory, READY, "IsDefaultLoadingGuiRemoved")?,
            ]
        } else {
            [(0, 0); 2]
        };
        let service = |name: &str| -> Result<u64> {
            Ok(resolve_path(&memory, context, &[name.into()], &[])?[0].0)
        };
        let (seconds, micros) = host_identity()?;
        let mut token = [0; 8];
        getrandom::fill(&mut token)
            .map_err(|e| anyhow::anyhow!("Cannot identify teleport GUI: {e}"))?;
        let mut request = vec![0; 192];
        put32(&mut request, 0, 1);
        put32(&mut request, 4, 192);
        put32(&mut request, 168, u32::from(show));
        put32(&mut request, 172, std::process::id());
        for (offset, value) in [
            (16, u64::from_le_bytes(token).max(1)),
            (24, read_u64(context, 8).unwrap()),
            (32, read_u64(context, 16).unwrap()),
            (40, relay.0),
            (48, relay.1),
            (56, service("ScriptContext")?),
            (64, service("CoreGui")?),
            (72, service("ReplicatedFirst")?),
            (80, attribute_slot),
            (88, attribute),
            (96, add),
            (104, remove),
            (112, ready[0].0),
            (120, ready[0].1),
            (128, ready[1].0),
            (136, ready[1].1),
            (144, parent_offset(&memory, context)?),
            (152, read_u64(context, 40).unwrap()),
            (160, read_u64(context, 48).unwrap()),
            (176, seconds),
            (184, micros),
        ] {
            put64(&mut request, offset, value);
        }
        let mut lease = Self { memory, request };
        lease.invoke(1)?;
        Ok(lease)
    }

    fn invoke(&mut self, action: u32) -> Result<u32> {
        self.memory.deadline = Instant::now() + Duration::from_secs(3);
        put32(&mut self.request, 8, action);
        let response = self.memory.request(10, &self.request, "")?;
        anyhow::ensure!(
            response.len() == self.request.len(),
            "Invalid Mac teleport GUI receipt"
        );
        read_u32(&response, 12).context("Missing Mac teleport GUI receipt")
    }

    pub(crate) fn complete(&mut self) -> Result<()> {
        anyhow::ensure!(
            self.invoke(3)? == 2,
            "The client did not invoke its teleport GUI notification"
        );
        Ok(())
    }
}

impl Drop for NativeMacTeleportGui {
    fn drop(&mut self) {
        let _ = self.invoke(2);
    }
}
