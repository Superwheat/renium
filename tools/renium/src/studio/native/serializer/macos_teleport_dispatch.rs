//! libc++ teleport dispatch into the existing server-owned plugin relay.
use super::*;

const INSTANCE: &str =
    "N3RBX10Reflection5TTypeINSt3__110shared_ptrINS_8InstanceEEELNS0_15TTypeStringModeE0EEE";
const TABLE: &str =
    "N3RBX10Reflection5TTypeINSt3__110shared_ptrIKNS0_10ValueTableEEELNS0_15TTypeStringModeE0EEE";
const STRING: &str = "N3RBX10Reflection5TTypeINSt3__112basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEELNS0_15TTypeStringModeE0EEE";
const RESULT: &str = "N3RBX19NonCreatableCreatorINS_19TeleportAsyncResultEEE";

pub(crate) struct NativeMacTeleportDispatch {
    memory: Mutex<Memory>,
    request: Vec<u8>,
    root: Option<Vec<u8>>,
}

impl NativeMacTeleportDispatch {
    pub(crate) fn arm(pid: u32, title: &str, path: &[String]) -> Result<Self> {
        Self::install(pid, title, Some(path), None)
    }

    pub(crate) fn before_play(pid: u32, title: &str, nonce: &str) -> Result<Self> {
        Self::install(pid, title, None, Some(nonce))
    }

    fn install(
        pid: u32,
        title: &str,
        path: Option<&[String]>,
        nonce: Option<&str>,
    ) -> Result<Self> {
        anyhow::ensure!(
            nonce.is_none_or(|value| !value.is_empty()
                && value.len() < 128
                && value
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')),
            "Invalid native launch nonce"
        );
        let runtime = path
            .and_then(|path| path.last())
            .and_then(|name| name.strip_prefix("__ReniumTeleportNative_"))
            .filter(|name| !name.is_empty() && name.len() < 64 && !name.bytes().any(|b| b == 0))
            .unwrap_or_default();
        anyhow::ensure!(
            nonce.is_some() || !runtime.is_empty(),
            "Invalid native teleport runtime identity"
        );
        let mut memory = Memory::for_process(pid, Duration::from_secs(30))?;
        let contexts = memory.request(5, &[0], title)?;
        anyhow::ensure!(
            !contexts.is_empty() && contexts.len().is_multiple_of(64),
            "Studio has no complete model context"
        );
        memory.base = read_u64(&contexts, 0).context("Missing Studio image base")?;
        let catalog = Catalog::open(&memory)?;
        let context = if let Some(path) = path {
            let matches = contexts
                .chunks_exact(64)
                .filter(|context| resolve_path(&memory, context, path, &[]).is_ok())
                .collect::<Vec<_>>();
            anyhow::ensure!(
                matches.len() == 1,
                "Teleport relay resolves {} models",
                matches.len()
            );
            matches[0].to_vec()
        } else {
            catalog.source_context(&memory, &contexts, None)?
        };
        let (slot, original) = catalog.member(&memory, TELEPORT, "TeleportAsync")?;
        let (invoke_slot, invoke) = catalog.member(&memory, INVOKE, "Invoke")?;
        let descriptor = memory.member(
            read_u64(&context, 8).unwrap(),
            read_u64(&context, 40).unwrap(),
            "GetAttribute",
        )?;
        anyhow::ensure!(
            memory.rtti(descriptor)?
                == "N3RBX10Reflection13BoundFuncDescINS_8InstanceEKFNS0_7VariantENS_10StringViewEELb0ELi1EEE",
            "The native teleport attribute getter changed ABI"
        );
        let (_, attribute) = memory.function_binding(descriptor, memory.pointer(descriptor)?)?;
        let factory = catalog.singleton(&memory, RESULT, None)?;
        let create = memory.pointer(memory.pointer(factory)?)?;
        memory.dispatch_code(create)?;
        let host = std::process::id();
        let (seconds, micros) = host_identity()?;
        let mut token = [0; 8];
        getrandom::fill(&mut token)
            .map_err(|error| anyhow::anyhow!("Cannot create teleport dispatch lease: {error}"))?;
        let mut request = vec![0; 448];
        put32(&mut request, 0, 2);
        put32(&mut request, 4, 448);
        put32(&mut request, 120, host);
        put32(&mut request, 124, 180000);
        request[160..160 + runtime.len()].copy_from_slice(runtime.as_bytes());
        for (at, value) in [
            (16, u64::from_le_bytes(token).max(1)),
            (24, slot),
            (32, original),
            (40, invoke),
            (48, attribute),
            (56, catalog.singleton(&memory, INSTANCE, None)?),
            (64, memory.base + catalog.table(STRING)?),
            (72, catalog.singleton(&memory, TABLE, None)?),
            (80, factory),
            (88, create),
            (96, parent_offset(&memory, &context)?),
            (104, read_u64(&context, 24).unwrap()),
            (112, read_u64(&context, 40).unwrap()),
            (128, seconds),
            (136, micros),
        ] {
            request[at..at + 8].copy_from_slice(&value.to_le_bytes());
        }
        if let Some(nonce) = nonce {
            let test = resolve_path(&memory, &context, &["StudioTestService".into()], &[])?[0].0;
            let (execute_slot, execute) =
                catalog.member(&memory, EXECUTE, "ExecutePlayModeAsync")?;
            let (_, args) = catalog.member(&memory, ARGS, "GetTestArgs")?;
            let place = catalog.identity(&memory, &context, "PlaceId")?;
            let universe = catalog.identity(&memory, &context, "GameId")?;
            for (at, value) in [
                (224, read_u64(&context, 8).unwrap()),
                (232, read_u64(&context, 16).unwrap()),
                (240, test),
                (248, args),
                (256, execute_slot),
                (264, execute),
                (272, invoke_slot),
                (280, read_u64(&context, 48).unwrap()),
                (288, place.0 as u64),
                (296, universe.0 as u64),
                (304, place.1),
                (312, universe.1),
            ] {
                put64(&mut request, at, value);
            }
            request[320..320 + nonce.len()].copy_from_slice(nonce.as_bytes());
        }
        let lease = Self {
            memory: Mutex::new(memory),
            request,
            root: nonce.map(|_| context),
        };
        lease.invoke(1, 0, 0)?;
        Ok(lease)
    }

    fn invoke(&self, action: u32, attempt: u64, place: u64) -> Result<u32> {
        let mut memory = self.memory.lock_recover();
        memory.deadline = Instant::now() + Duration::from_secs(3);
        let mut request = self.request.clone();
        put32(&mut request, 8, action);
        request[144..152].copy_from_slice(&attempt.to_le_bytes());
        request[152..160].copy_from_slice(&place.to_le_bytes());
        let response = memory.request(8, &request, "")?;
        anyhow::ensure!(
            response.len() == request.len(),
            "Invalid Mac teleport dispatch receipt"
        );
        read_u32(&response, 12).context("Missing teleport dispatch receipt")
    }

    pub(crate) fn renew(&mut self) -> Result<()> {
        self.invoke(3, 0, 0).map(|_| ())
    }

    pub(crate) fn accepted(&self, attempt: u64, place: i64) -> Result<bool> {
        Ok(self.invoke(4, attempt, place as u64)? == 1)
    }

    pub(crate) fn token(&self) -> u64 {
        read_u64(&self.request, 16).unwrap()
    }

    pub(crate) fn has_launch_scope(&self) -> bool {
        self.request[320] != 0
    }
    pub(crate) fn has_root(&self) -> bool {
        self.root.is_some()
    }

    pub(super) fn root_context(&self, memory: &Memory, contexts: &[u8]) -> Result<Vec<u8>> {
        let root = self
            .root
            .as_ref()
            .context("The teleport has no retained Edit controller")?;
        let model = read_u64(root, 8).unwrap();
        let owner = read_u64(root, 16).unwrap();
        let context = contexts
            .chunks_exact(64)
            .find(|context| {
                read_u64(context, 8) == Some(model) && read_u64(context, 16) == Some(owner)
            })
            .context("The original teleport Edit controller ended")?;
        anyhow::ensure!(
            memory.pointer(model + read_u64(context, 48).unwrap() + 8)? == owner,
            "The original teleport Edit ownership changed"
        );
        Ok(context.to_vec())
    }

    pub(crate) fn advance(&mut self, place: i64, nonce: &str) -> Result<()> {
        anyhow::ensure!(
            self.has_launch_scope()
                && place > 0
                && !nonce.is_empty()
                && nonce.len() < 128
                && nonce
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "Invalid destination launch scope"
        );
        put64(&mut self.request, 304, place as u64);
        self.request[320..448].fill(0);
        self.request[320..320 + nonce.len()].copy_from_slice(nonce.as_bytes());
        self.invoke(5, 0, 0).map(|_| ())
    }
}

impl Drop for NativeMacTeleportDispatch {
    fn drop(&mut self) {
        let _ = self.invoke(2, 0, 0);
    }
}
