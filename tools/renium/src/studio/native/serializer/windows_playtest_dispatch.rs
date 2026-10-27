//! Owned server TeleportAsync dispatch into the plugin's validated request path.
use super::*;

const TELEPORT: &str = ".?AV?$BoundYieldFuncDesc@VTeleportService@RBX@@$$A6A?AV?$shared_ptr@VInstance@RBX@@@std@@_JV?$shared_ptr@$$CBV?$vector@V?$shared_ptr@VInstance@RBX@@@std@@V?$allocator@V?$shared_ptr@VInstance@RBX@@@std@@@2@@std@@@4@V34@@Z$0A@$02@Reflection@RBX@@";
pub(super) const INVOKE: &str = ".?AV?$BoundYieldFuncDesc@VBindableFunction@RBX@@$$A6A?AV?$shared_ptr@$$CBUTuple@Reflection@RBX@@@std@@V34@@Z$0A@$00@Reflection@RBX@@";
pub(super) const INSTANCE: &str =
    ".?AV?$TType@V?$shared_ptr@VInstance@RBX@@@std@@$0A@@Reflection@RBX@@";
const RESULT_CREATOR: &str = ".?AV?$NonCreatableCreator@VTeleportAsyncResult@RBX@@@RBX@@";
pub(super) const TABLE: &str =
    ".?AV?$TType@V?$shared_ptr@$$CBVValueTable@Reflection@RBX@@@std@@$0A@@Reflection@RBX@@";

fn result_factory(memory: &ProcessMemory, studio: &ModuleEntry) -> Result<(usize, usize)> {
    // The class is deliberately absent from the user-creatable registry. Its
    // complete NonCreatableCreator has the same engine creation interface
    // qualified by the native reader, and owns construction/refcount setup.
    super::super::loader::verify_loaded(memory, studio)?;
    let factory = types::singleton(memory, studio, RESULT_CREATOR, None)?;
    let table = memory.read_u64(factory)? as usize;
    let create = memory.read_u64(table)? as usize;
    properties::dispatch_code(memory, studio, &package_layout(&studio.path)?, create)?;
    Ok((factory, create))
}

pub(in super::super) fn probe(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    model: &ActiveDataModel,
) -> Result<Value> {
    let abi = types::yield_abi(memory, studio, TELEPORT, "TeleportAsync", 3)?;
    anyhow::ensure!(abi == types::yield_abi(memory, studio, INVOKE, "Invoke", 1)?, "Native teleport continuation ABIs disagree");
    let (slot, function) = types::yield_member(memory, studio, TELEPORT, "TeleportAsync", 3)?;
    let (_, invoke) = types::yield_member(memory, studio, INVOKE, "Invoke", 1)?;
    let launch = loader::nonce_bindings(memory, studio, model)?;
    let (factory, create) = result_factory(memory, studio)?;

    Ok(
        json!({"continuationAbi":abi,"slot":slot,"function":function,"instanceType":types::singleton(memory, studio, INSTANCE, None)?, "tableType":types::singleton(memory, studio, TABLE, None)?, "invoke":invoke, "launch":launch,
            "resultFactory":factory,"resultCreate":create}),
    )
}

pub(crate) struct NativeTeleportDispatch {
    memory: ProcessMemory,
    entrypoint: usize,
    request: Vec<u8>,
    root: Option<CachedDataModel>,
}

// The Windows process handle belongs to this lease and is valid on any host
// thread. Its Cell and request buffer remain exclusively owned after transfer.
unsafe impl Send for NativeTeleportDispatch {}

impl NativeTeleportDispatch {
    pub(crate) fn arm(pid: u32, title: &str, path: &[String]) -> Result<Self> {
        Self::install(pid, title, Some(path), None)
    }

    pub(crate) fn before_play(pid: u32, title: &str, nonce: &str) -> Result<Self> {
        anyhow::ensure!(
            !nonce.is_empty()
                && nonce.len() < 128
                && nonce
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "Invalid native launch nonce"
        );
        Self::install(pid, title, None, Some(nonce))
    }

    fn install(
        pid: u32,
        title: &str,
        path: Option<&[String]>,
        launch_nonce: Option<&str>,
    ) -> Result<Self> {
        let loaded = modules(pid)?;
        let studio = loaded.first().context("Studio process ended")?;
        let memory = ProcessMemory::open(pid)?;
        memory.authorize(
            if launch_nonce.is_some() {
                NativeOperation::PlayTeleportLaunch
            } else {
                NativeOperation::PlayTeleportDispatch
            },
            title,
        )?;
        let layout = package_layout(&studio.path)?;
        let model = active_data_model(pid, &memory, studio, layout.data, title)?;
        let (slot, function) = types::yield_member(&memory, studio, TELEPORT, "TeleportAsync", 3)?;
        let (_, invoke) = types::yield_member(&memory, studio, INVOKE, "Invoke", 1)?;
        let runtime = path.and_then(|path| {
            path.last()
                .and_then(|name| name.strip_prefix("__ReniumTeleportNative_"))
                .filter(|runtime| !runtime.is_empty() && runtime.len() < 64)
        });
        anyhow::ensure!(
            runtime.is_some() || launch_nonce.is_some(),
            "Invalid native teleport runtime identity"
        );
        let nonce = loader::nonce_bindings(&memory, studio, &model)?;
        let instance_type = types::singleton(&memory, studio, INSTANCE, None)?;
        let (factory, create) = result_factory(&memory, studio)?;
        let mut request = vec![0; 752];
        put_u32(&mut request, 0, 6);
        put_u32(&mut request, 4, 752);
        if let Some(runtime) = runtime {
            request[376..376 + runtime.len()].copy_from_slice(runtime.as_bytes());
        }
        if let Some(nonce) = launch_nonce {
            let owner = owner::resolve(&memory, studio, &model)?;
            for (index, value) in owner.fields.into_iter().enumerate() {
                put_u64(&mut request, 464 + index * 8, value);
            }
            for (at, value) in [
                (592, owner.execute.0),
                (600, owner.execute.1),
                (608, owner.place as usize),
                (616, owner.universe as usize),
            ] {
                put_u64(&mut request, at, value);
            }
            request[624..624 + nonce.len()].copy_from_slice(nonce.as_bytes());
        }
        let mut token = [0; 8];
        getrandom::fill(&mut token)
            .map_err(|error| anyhow::anyhow!("Cannot create teleport dispatch lease: {error}"))?;
        let host = std::process::id();
        let creation = crate::app::update::process_start_identity(host)
            .context("Teleport host ended")?
            .parse::<usize>()?;
        for (offset, value) in [
            (16, u64::from_le_bytes(token).max(1) as usize),
            (24, slot),
            (32, function),
            (40, properties::parent_offset(&memory, &model)?),
            (48, model.layout.children),
            (56, model.layout.class_descriptor),
            (64, 0),
            (72, invoke),
            (80, nonce[0]),
            (88, instance_type),
            (96, nonce[1]),
            (104, creation),
            (440, types::singleton(&memory, studio, TABLE, None)?),
            (448, factory),
            (456, create),
        ] {
            put_u64(&mut request, offset, value);
        }
        put_u32(&mut request, 112, host);
        put_u32(&mut request, 116, 180000);
        let entry = helper_export_rva("ReniumPlayTeleport")?;
        let helper = ensure_play_helper_loaded(pid, &memory, &loaded)?;
        let mut lease = Self {
            memory,
            entrypoint: helper + entry,
            request,
            root: launch_nonce.map(|_| CachedDataModel {
                title: title.to_owned(),
                outer: model.outer,
                owner: model.owner,
                layout: model.layout,
            }),
        };
        lease.invoke(1)?;
        Ok(lease)
    }

    fn invoke(&mut self, action: u32) -> Result<()> {
        put_u32(&mut self.request, 8, action);
        self.request[120..376].fill(0);
        let mut input = self.memory.allocate(self.request.len())?;
        self.memory.write(input.address, &self.request)?;
        let exit = input.run(self.entrypoint, 3000)?;
        self.memory.read(input.address, &mut self.request)?;
        let error = error_text_at(&self.request, 120);
        anyhow::ensure!(exit == 0 && error.is_empty(), "Teleport dispatch: {error}");
        Ok(())
    }

    pub(crate) fn renew(&mut self) -> Result<()> {
        self.invoke(3)
    }

    pub(crate) fn token(&self) -> u64 {
        u64::from_le_bytes(self.request[16..24].try_into().unwrap())
    }

    pub(crate) fn has_launch_scope(&self) -> bool {
        self.request[624] != 0
    }

    pub(crate) fn has_root(&self) -> bool {
        self.root.is_some()
    }

    pub(super) fn root_model(&self, studio: &ModuleEntry) -> Result<ActiveDataModel> {
        let root = self
            .root
            .as_ref()
            .context("The teleport has no retained Edit controller")?;
        refresh_active_data_model(&self.memory, studio, &root.title, root)
            .context("The original teleport Edit controller ended")
    }

    pub(crate) fn advance(&mut self, place: i64, nonce: &str) -> Result<()> {
        anyhow::ensure!(
            place > 0 && !nonce.is_empty() && nonce.len() < 128,
            "Invalid destination launch scope"
        );
        if !self.has_launch_scope() {
            let loaded = modules(self.memory.pid)?;
            let studio = loaded.first().context("Studio process ended")?;
            let title = crate::studio::input::studio_window_title(self.memory.pid)?;
            let model = active_data_model(
                self.memory.pid,
                &self.memory,
                studio,
                package_layout(&studio.path)?.data,
                &title,
            )?;
            let owner = owner::resolve(&self.memory, studio, &model)?;
            for (index, value) in owner.fields.into_iter().enumerate() {
                put_u64(&mut self.request, 464 + index * 8, value);
            }
            put_u64(&mut self.request, 592, owner.execute.0);
            put_u64(&mut self.request, 600, owner.execute.1);
            put_u64(&mut self.request, 616, owner.universe as usize);
            self.root = Some(CachedDataModel {
                title,
                outer: model.outer,
                owner: model.owner,
                layout: model.layout,
            });
        }
        put_u64(&mut self.request, 608, place as usize);
        self.request[624..752].fill(0);
        self.request[624..624 + nonce.len()].copy_from_slice(nonce.as_bytes());
        self.invoke(5)
    }

    pub(crate) fn accepted(&self, attempt: u64, place: i64) -> Result<bool> {
        let mut request = self.request.clone();
        put_u32(&mut request, 8, 4);
        put_u64(&mut request, 24, attempt as usize);
        put_u64(&mut request, 32, place as usize);
        request[120..376].fill(0);
        let mut input = self.memory.allocate(request.len())?;
        self.memory.write(input.address, &request)?;
        let exit = input.run(self.entrypoint, 3000)?;
        self.memory.read(input.address, &mut request)?;
        let error = error_text_at(&request, 120);
        anyhow::ensure!(
            exit == 0 && error.is_empty(),
            "Teleport acceptance: {error}"
        );
        Ok(read_u32(&request, 12)? == 1)
    }
}

impl Drop for NativeTeleportDispatch {
    fn drop(&mut self) {
        let _ = self.invoke(2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an explicitly owned active Play server and native relay"]
    fn live_teleport_dispatch_owned_process() -> Result<()> {
        let pid = std::env::var("RENIUM_TELEPORT_TEST_PID")?.parse()?;
        let runtime = std::env::var("RENIUM_TELEPORT_TEST_RUNTIME")?;
        let stop = std::path::PathBuf::from(std::env::var("RENIUM_TELEPORT_TEST_STOP")?);
        anyhow::ensure!(!stop.exists(), "Owned test stop signal already exists");
        let title = crate::studio::input::studio_window_title(pid)?;
        let mut dispatch = NativeTeleportDispatch::arm(
            pid,
            &title,
            &[
                "ServerStorage".into(),
                format!("__ReniumTeleportNative_{runtime}"),
            ],
        )?;
        println!("TELEPORT_DISPATCH_ARMED pid={pid}");
        let deadline = Instant::now() + Duration::from_secs(50);
        while Instant::now() < deadline && !stop.exists() {
            dispatch.renew()?;
            std::thread::sleep(Duration::from_millis(250));
        }
        Ok(())
    }
}
