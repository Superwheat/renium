//! Populate the actual engine arrival fields before game callbacks read them.
use super::*;

const JOIN: &str = "N3RBX10Reflection19CustomBoundFuncDescINS_6PlayerEFNSt3__110shared_ptrIKNS0_10ValueTableEEEvELb0ELi0EEE";
const LOCAL: &str =
    "N3RBX10Reflection13BoundFuncDescINS_15TeleportServiceEFNS0_7VariantEvELb0ELi0EEE";

pub(crate) struct NativeMacPlayJoin {
    memory: Memory,
    request: Vec<u8>,
    armed: bool,
}

impl NativeMacPlayJoin {
    pub(crate) fn prepare(pid: u32, title: &str, shared_launch: bool) -> Result<Self> {
        anyhow::ensure!(
            shared_launch,
            "Mac arrival requires the retained native launch"
        );
        let mut memory = Memory::for_process(pid, Duration::from_secs(30))?;
        let contexts = memory.request(5, &[0], title)?;
        memory.base = read_u64(&contexts, 0).context("Missing Studio image base")?;
        let catalog = Catalog::open(&memory)?;
        let (join_slot, join) = catalog.member(&memory, JOIN, "GetJoinData")?;
        let (local_slot, local) = catalog.member(&memory, LOCAL, "GetLocalPlayerTeleportData")?;
        let join_field = arm64_playtest::arrival_field(&memory.dispatch_code(join)?, false)
            .context("GetJoinData no longer forwards a unique receiver table to Lua")?;
        let local_field = arm64_playtest::arrival_field(&memory.dispatch_code(local)?, true)
            .context("GetLocalPlayerTeleportData no longer copies a complete receiver Variant")?;
        let mut token = [0; 8];
        getrandom::fill(&mut token)
            .map_err(|e| anyhow::anyhow!("Cannot identify Mac arrival: {e}"))?;
        let mut request = vec![0; 232];
        put32(&mut request, 0, 1);
        put32(&mut request, 4, 232);
        for (at, value) in [
            (16, u64::from_le_bytes(token).max(1)),
            (56, join_slot),
            (64, join),
            (72, join_field as u64),
            (80, local_slot),
            (88, local),
            (96, local_field as u64),
        ] {
            put64(&mut request, at, value);
        }
        Ok(Self {
            memory,
            request,
            armed: false,
        })
    }

    pub(crate) fn arm(
        &mut self,
        source: (i64, i64),
        destination: i64,
        nonce: &str,
        dispatch: u64,
        attempt: u64,
        root: Option<&NativeMacTeleportDispatch>,
    ) -> Result<()> {
        anyhow::ensure!(
            source.0 > 0
                && source.1 > 0
                && destination > 0
                && attempt > 0
                && root.is_some_and(|root| root.has_root() && root.token() == dispatch)
                && !nonce.is_empty()
                && nonce.len() < 128
                && !nonce.contains('\0'),
            "Mac arrival has no matching retained Edit launch"
        );
        for (at, value) in [
            (24, dispatch),
            (32, attempt),
            (40, destination as u64),
            (48, source.1 as u64),
        ] {
            put64(&mut self.request, at, value);
        }
        self.request[104..].fill(0);
        self.request[104..104 + nonce.len()].copy_from_slice(nonce.as_bytes());
        self.invoke(1)?;
        self.armed = true;
        Ok(())
    }

    fn invoke(&mut self, action: u32) -> Result<u32> {
        self.memory.deadline = Instant::now() + Duration::from_secs(3);
        put32(&mut self.request, 8, action);
        let result = self.memory.request(9, &self.request, "")?;
        anyhow::ensure!(
            result.len() == self.request.len(),
            "Invalid Mac arrival receipt"
        );
        read_u32(&result, 12).context("Missing Mac arrival receipt")
    }

    pub(crate) fn receipt(&mut self) -> Result<u32> {
        self.invoke(3)
    }
}

impl Drop for NativeMacPlayJoin {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.invoke(2);
        }
    }
}
