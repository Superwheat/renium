//! Bind the source serialization callback by its loader, RTTI and identity flow.
//! No Studio addresses, lambda hashes or ServerLoadInfo field offsets are fixed.
use super::*;
use iced_x86::{
    Decoder, DecoderOptions, FlowControl, Instruction, InstructionInfoFactory, Mnemonic, OpAccess,
    OpKind, Register,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Origin {
    Capture(i64),
    ModelReference,
    Model(i64),
    Config(i64, i64),
    Identity(usize),
    Stack(i64),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Flow {
    registers: HashMap<Register, Origin>,
    stack: BTreeMap<i64, (usize, Origin)>,
}

impl Flow {
    fn register(&self, register: Register) -> Option<Origin> {
        self.registers.get(&register.full_register()).copied()
    }

    fn address(&self, i: &Instruction) -> Option<Origin> {
        if i.memory_index() != Register::None || i.is_ip_rel_memory_operand() {
            return None;
        }
        Self::offset(
            self.register(i.memory_base())?,
            i.memory_displacement64() as i64,
        )
    }

    fn offset(origin: Origin, offset: i64) -> Option<Origin> {
        Some(match origin {
            Origin::Capture(base) => Origin::Capture(base.checked_add(offset)?),
            Origin::Model(base) => Origin::Model(base.checked_add(offset)?),
            Origin::Config(field, base) => Origin::Config(field, base.checked_add(offset)?),
            Origin::Stack(base) => Origin::Stack(base.checked_add(offset)?),
            other if offset == 0 => other,
            _ => return None,
        })
    }

    fn read(&self, i: &Instruction, fields: [usize; 2]) -> Option<Origin> {
        match i.op1_kind() {
            OpKind::Register => self.register(i.op1_register()),
            OpKind::Memory => {
                let address = self.address(i)?;
                if i.mnemonic() == Mnemonic::Lea {
                    return Some(address);
                }
                let width = i.memory_size().size();
                match address {
                    Origin::ModelReference if width == 8 => Some(Origin::Model(0)),
                    Origin::Capture(field) if width == 8 => Some(Origin::Config(field, 0)),
                    Origin::Model(field) if width == 8 => fields
                        .iter()
                        .position(|known| *known as i64 == field)
                        .map(Origin::Identity),
                    Origin::Stack(offset) => self
                        .stack
                        .get(&offset)
                        .filter(|(size, _)| *size == width)
                        .map(|(_, value)| *value),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn store_stack(&mut self, offset: i64, size: usize, value: Option<Origin>) {
        self.stack
            .retain(|at, (width, _)| *at + *width as i64 <= offset || *at >= offset + size as i64);
        if let Some(value) = value {
            self.stack.insert(offset, (size, value));
        }
    }

    fn instruction(
        &mut self,
        i: &Instruction,
        identity: [usize; 2],
        stores: &mut [HashSet<(i64, i64)>; 2],
        info: &mut InstructionInfoFactory,
    ) {
        let destination = i.op0_register().full_register();
        let value = match i.mnemonic() {
            Mnemonic::Mov | Mnemonic::Lea => self.read(i, identity),
            Mnemonic::Add | Mnemonic::Sub
                if i.op1_kind() != OpKind::Register && i.op1_kind() != OpKind::Memory =>
            {
                let offset = i.immediate(1) as i64;
                self.register(destination).and_then(|origin| {
                    Flow::offset(
                        origin,
                        if i.mnemonic() == Mnemonic::Sub {
                            -offset
                        } else {
                            offset
                        },
                    )
                })
            }
            Mnemonic::Pop => match self.register(Register::RSP) {
                Some(Origin::Stack(offset)) => self.stack.get(&offset).map(|(_, value)| *value),
                _ => None,
            },
            _ => None,
        };
        if i.op0_kind() == OpKind::Memory {
            let width = i.memory_size().size();
            match self.address(i) {
                Some(Origin::Stack(offset)) => self.store_stack(offset, width, value),
                Some(Origin::Config(field, offset)) if width == 8 => {
                    if let Some(Origin::Identity(index)) = value {
                        stores[index].insert((field, offset));
                    }
                }
                _ => {}
            }
        }
        let stack = match (i.mnemonic(), self.register(Register::RSP)) {
            (Mnemonic::Push, Some(Origin::Stack(offset))) => Some(Origin::Stack(offset - 8)),
            (Mnemonic::Pop, Some(Origin::Stack(offset))) => Some(Origin::Stack(offset + 8)),
            _ => None,
        };
        for reg in info.info(i).used_registers() {
            if matches!(
                reg.access(),
                OpAccess::Write
                    | OpAccess::CondWrite
                    | OpAccess::ReadWrite
                    | OpAccess::ReadCondWrite
            ) {
                self.registers.remove(&reg.register().full_register());
            }
        }
        if i.op0_kind() == OpKind::Register
            && let Some(value) = value
        {
            self.registers.insert(destination, value);
        }
        if let Some(stack) = stack {
            self.registers.insert(Register::RSP, stack);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(in super::super) struct Fields {
    pub(in super::super) capture: usize,
    pub(in super::super) string: usize,
    pub(in super::super) place: usize,
    pub(in super::super) universe: usize,
}

/// The two identity inputs come from the reflected DataModel property bindings.
/// Follow both control-flow arms and tail helpers; unknown writes lose provenance.
fn producer_fields(
    start: usize,
    identity: [usize; 2],
    instance: usize,
    direct_model: bool,
    mut code: impl FnMut(usize) -> Option<Vec<u8>>,
) -> Option<Fields> {
    let mut pending = vec![(
        start,
        Flow {
            registers: HashMap::from([
                (Register::RCX, Origin::Capture(0)),
                (
                    Register::RDX,
                    if direct_model {
                        Origin::Model(0)
                    } else {
                        Origin::ModelReference
                    },
                ),
                (Register::RSP, Origin::Stack(0)),
            ]),
            ..Flow::default()
        },
    )];
    let mut decoded = HashMap::<usize, Instruction>::new();
    let mut loaded = HashSet::new();
    let mut visited = HashMap::<usize, Vec<Flow>>::new();
    let mut stores = [HashSet::new(), HashSet::new()];
    let mut serializations = HashSet::new();
    let mut info = InstructionInfoFactory::new();
    let mut budget = 16384;
    while let Some((mut pc, mut state)) = pending.pop() {
        loop {
            budget -= 1;
            if budget == 0 || pending.len() > 256 || loaded.len() > 16 {
                return None;
            }
            if !decoded.contains_key(&pc) {
                if !loaded.insert(pc) {
                    return None;
                }
                let bytes = code(pc)?;
                for i in Decoder::with_ip(64, &bytes, pc as u64, DecoderOptions::NONE) {
                    if i.is_invalid() {
                        return None;
                    }
                    decoded.insert(i.ip() as usize, i);
                }
            }
            let seen = visited.entry(pc).or_default();
            if seen.contains(&state) {
                break;
            }
            if seen.len() >= 32 {
                return None;
            }
            seen.push(state.clone());
            let i = decoded.get(&pc)?;
            let next = i.next_ip() as usize;
            match i.flow_control() {
                FlowControl::Return | FlowControl::Exception | FlowControl::Interrupt => break,
                FlowControl::UnconditionalBranch => {
                    if i.op0_kind() != OpKind::NearBranch64 {
                        return None;
                    }
                    pc = i.near_branch_target() as usize;
                    continue;
                }
                FlowControl::ConditionalBranch => {
                    let target = i.near_branch_target() as usize;
                    // Backward edges are allowed only through the visited-state budget.
                    pending.push((target, state.clone()));
                    pc = next;
                    continue;
                }
                FlowControl::Call | FlowControl::IndirectCall => {
                    if let (Some(Origin::Config(field, offset)), Some(Origin::Model(model))) =
                        (state.register(Register::RCX), state.register(Register::RDX))
                        && model == instance as i64
                        && i.flow_control() == FlowControl::Call
                    {
                        serializations.insert((field, offset));
                    }
                    for register in [
                        Register::RAX,
                        Register::RCX,
                        Register::RDX,
                        Register::R8,
                        Register::R9,
                        Register::R10,
                        Register::R11,
                    ] {
                        state.registers.remove(&register);
                    }
                    pc = next;
                    continue;
                }
                FlowControl::Next => {}
                _ => return None,
            }
            state.instruction(i, identity, &mut stores, &mut info);
            pc = next;
        }
    }
    if stores.iter().any(|set| set.len() != 1) || serializations.len() != 1 {
        return None;
    }
    let &(capture, place) = stores[0].iter().next()?;
    let &(other, universe) = stores[1].iter().next()?;
    let &(serialized, string) = serializations.iter().next()?;
    if capture != other
        || capture != serialized
        || capture < 0
        || string < 0
        || place < string + 32
        || universe < string + 32
        || place == universe
    {
        return None;
    }
    Some(Fields {
        capture: capture.try_into().ok()?,
        string: string.try_into().ok()?,
        place: place.try_into().ok()?,
        universe: universe.try_into().ok()?,
    })
}

#[derive(Clone)]
pub(in super::super) struct Binding {
    pub(in super::super) slot: usize,
    pub(in super::super) function: usize,
    pub(in super::super) fields: Fields,
    pub(in super::super) direct_model: bool,
    code: Vec<(usize, Vec<u8>)>,
}

type BindingKey = ([u32; 3], [usize; 2], usize);
static BINDINGS: OnceLock<Mutex<HashMap<BindingKey, Binding>>> = OnceLock::new();

fn verify(memory: &ProcessMemory, studio: &ModuleEntry, binding: &Binding) -> Result<()> {
    for (rva, bytes) in &binding.code {
        anyhow::ensure!(
            memory.read_vec(studio.base + rva, bytes.len())? == *bytes,
            "Solo loader code changed after discovery"
        );
    }
    anyhow::ensure!(
        memory.read_u64(studio.base + binding.slot)? as usize == studio.base + binding.function,
        "Solo loader callback was replaced"
    );
    Ok(())
}

pub(in super::super) fn resolve(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    model: &ActiveDataModel,
) -> Result<Binding> {
    let native = model.outer + model.layout.data_model_instance;
    let mut identity = [0; 2];
    for (index, name) in ["PlaceId", "GameId"].iter().enumerate() {
        let field = integer_property_layout(memory, studio, native, model.layout, name)?;
        anyhow::ensure!(
            field.width == 8,
            "Play identity property changed representation"
        );
        identity[index] = model.layout.data_model_instance + field.offset;
    }
    let mapped = observation::ImageView::open(&studio.path)?;
    let pe = PeImage::parse(mapped.bytes())?;
    let key = (pe.image_stamp, identity, model.layout.data_model_instance);
    let cached = BINDINGS
        .get_or_init(Default::default)
        .lock_recover()
        .get(&key)
        .cloned();
    if let Some(binding) = cached {
        verify(memory, studio, &binding)?;
        return Ok(binding);
    }
    let anchors = image::anchors(
        &pe,
        &[
            "DataModelLoader::loadPlaySoloServer".into(),
            "XPEAVDataModel@RBX@@@std@@".into(),
        ],
    )?;
    let tables = image::rtti(&pe, &anchors)?;
    let targets = anchors
        .iter()
        .map(|a| a.rva)
        .chain(
            tables
                .iter()
                .filter_map(|table| table["vtableRva"].as_u64().map(|v| v as usize)),
        )
        .collect::<BTreeSet<_>>();
    let index = image::index(&pe, &targets)?;
    let loaders = anchors
        .iter()
        .filter(|a| a.text == "DataModelLoader::loadPlaySoloServer")
        .flat_map(|a| {
            index
                .references
                .get(&a.rva)
                .into_iter()
                .flatten()
                .map(|edge| edge.target)
        })
        .collect::<HashSet<_>>();
    anyhow::ensure!(
        loaders.len() == 1,
        "Solo loader has no unique structural owner"
    );
    let mut candidates = Vec::new();
    let mut owner_code = Vec::new();
    for &owner in &loaders {
        // The compact callable has no RTTI. Follow its actual construction
        // and third-argument use in the source loader, then qualify the
        // invoker's serialization and identity stores exactly as below.
        let tables = properties::inline_callback_tables(
            owner as u64,
            |at| {
                let at = usize::try_from(at).ok()?;
                let (begin, end) = image::code_range(&pe, &index, at).ok()?;
                if begin != at {
                    return None;
                }
                let bytes = slice(pe.bytes, pe.rva_to_offset(begin).ok()?, end - begin)
                    .ok()?
                    .to_vec();
                owner_code.push((begin, bytes.clone()));
                Some(bytes)
            },
            2,
        )
        .unwrap_or_default();
        for table in tables {
            let function = read_u64(pe.bytes, pe.rva_to_offset(table)?)? as usize;
            if let Some(function) = function.checked_sub(pe.image_base) {
                candidates.push((table, function, true));
            }
        }
    }
    for table in tables {
        let kind = table["type"].as_str().unwrap_or_default();
        let rva = table["vtableRva"]
            .as_u64()
            .context("Missing callback vtable")? as usize;
        if !kind.starts_with(".?AV?$_Func_impl_no_alloc@")
            || !kind.ends_with("@@XPEAVDataModel@RBX@@@std@@")
            || table["offsetToTop"] != 0
            || !index
                .references
                .get(&rva)
                .is_some_and(|refs| refs.iter().any(|edge| loaders.contains(&edge.target)))
        {
            continue;
        }
        for slot in table["slots"]
            .as_array()
            .context("Missing callback methods")?
        {
            let function = slot["targetRva"]
                .as_u64()
                .context("Missing callback method")? as usize;
            candidates.push((
                rva + slot["byteOffset"]
                    .as_u64()
                    .context("Missing callback slot")? as usize,
                function,
                false,
            ));
        }
    }
    let mut found = Vec::new();
    for (slot, function, direct_model) in candidates {
        let mut evidence = Vec::new();
        if let Some(fields) = producer_fields(
            function,
            identity,
            model.layout.data_model_instance,
            direct_model,
            |address| {
                let (begin, end) = image::code_range(&pe, &index, address).ok()?;
                if begin != address {
                    return None;
                }
                let bytes = slice(pe.bytes, pe.rva_to_offset(begin).ok()?, end - begin)
                    .ok()?
                    .to_vec();
                evidence.push((address, bytes.clone()));
                Some(bytes)
            },
        ) {
            evidence.extend(owner_code.clone());
            found.push(Binding {
                slot,
                function,
                fields,
                direct_model,
                code: evidence,
            });
        }
    }
    anyhow::ensure!(
        found.len() == 1,
        "Solo source serialization resolved {} callbacks",
        found.len()
    );
    let binding = found.remove(0);
    verify(memory, studio, &binding)?;
    let mut cache = BINDINGS.get().unwrap().lock_recover();
    if cache.len() >= 16 {
        cache.clear();
    }
    cache.insert(key, binding.clone());
    Ok(binding)
}

/// Process-pinned lease for one alternate input. Dropping a lease cancels it;
/// the helper also cancels on host death, timeout and late cancellation.
pub(in super::super) fn nonce_bindings(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    model: &ActiveDataModel,
) -> Result<[usize; 4]> {
    let layout = package_layout(&studio.path)?;
    let instance = model.outer + model.layout.data_model_instance;
    let attribute = find_class_member_descriptor(memory, instance, model.layout, "GetAttribute")?;
    let attribute_type = properties::complete_reflection_type(memory, studio, attribute)
        .context("GetAttribute reflection identity changed")?;
    anyhow::ensure!(
        attribute_type
            == ".?AV?$BoundFuncDesc@VInstance@RBX@@$$A8@@EBA?AVVariant@Reflection@2@VStringView@2@@Z$0A@$00@Reflection@RBX@@",
        "GetAttribute's native launch-check ABI changed: {attribute_type}"
    );
    let (_, attribute) = properties::member_binding(memory, studio, &layout, attribute)
        .context("Cannot bind the owned launch attribute getter")?;
    const STRING_TYPE: &str = ".?AV?$TType@V?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@$0A@@Reflection@RBX@@";
    let string_type = types::singleton(memory, studio, STRING_TYPE, None)?;
    let table = memory.read_u64(string_type)? as usize;
    Ok([attribute, table, 0, instance])
}

pub(crate) struct NativePlayLoad {
    memory: ProcessMemory,
    entrypoint: usize,
    request: Vec<u8>,
}

impl NativePlayLoad {
    pub(crate) fn arm(
        pid: u32,
        title: &str,
        source: (i64, i64),
        destination: (i64, i64),
        bytes: &[u8],
        launch_nonce: &str,
        root: Option<&dispatch::NativeTeleportDispatch>,
    ) -> Result<Self> {
        anyhow::ensure!(
            source.0 > 0 && source.1 > 0 && destination.0 > 0 && destination.1 == source.1,
            "Native Play input requires a readable place in the source universe"
        );
        anyhow::ensure!(
            !launch_nonce.is_empty()
                && launch_nonce.len() < 128
                && launch_nonce
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "Invalid owned Play launch nonce"
        );
        let loaded = modules(pid)?;
        let studio = loaded.first().context("Studio process ended")?;
        let memory = ProcessMemory::open(pid)?;
        memory.authorize(NativeOperation::PlayTeleportLoad, title)?;
        let layout = package_layout(&studio.path)?;
        let model = if let Some(root) = root {
            root.root_model(studio)?
        } else {
            active_data_model(pid, &memory, studio, layout.data, title)?
        };
        for (name, expected) in [("PlaceId", source.0), ("GameId", source.1)] {
            let field = integer_property_layout(
                &memory,
                studio,
                model.outer + model.layout.data_model_instance,
                model.layout,
                name,
            )?;
            anyhow::ensure!(
                read_integer_property(
                    &memory,
                    model.outer + model.layout.data_model_instance,
                    field
                )? == expected,
                "The source Edit identity changed before Play input preparation"
            );
        }
        let binding = resolve(&memory, studio, &model)?;
        let nonce_bindings = nonce_bindings(&memory, studio, &model)?;
        let helper = ensure_play_helper_loaded(pid, &memory, &loaded)?;
        let entrypoint = helper + helper_export_rva("ReniumPlayLoader")?;
        let mut nonce = [0; 8];
        getrandom::fill(&mut nonce)
            .map_err(|error| anyhow::anyhow!("Cannot create Play loader lease: {error}"))?;
        let mut request = vec![0; 560];
        put_u32(&mut request, 0, 4);
        put_u32(&mut request, 4, 560);
        for (index, value) in nonce_bindings.into_iter().enumerate() {
            put_u64(&mut request, 400 + index * 8, value);
        }
        put_u64(&mut request, 416, usize::from(binding.direct_model));
        request[432..432 + launch_nonce.len()].copy_from_slice(launch_nonce.as_bytes());
        for (offset, value) in [
            (16, u64::from_le_bytes(nonce).max(1) as usize),
            (24, studio.base + binding.slot),
            (32, studio.base + binding.function),
            (40, model.outer),
            (48, source.0 as usize),
            (56, source.1 as usize),
            (64, destination.0 as usize),
            (72, destination.1 as usize),
            (80, binding.fields.capture),
            (88, binding.fields.string),
            (96, binding.fields.place),
            (104, binding.fields.universe),
            (120, bytes.len()),
        ] {
            put_u64(&mut request, offset, value);
        }
        let host = std::process::id();
        let creation = crate::app::update::process_start_identity(host)
            .context("Play loader host process ended")?
            .parse::<usize>()?;
        put_u64(&mut request, 128, creation);
        put_u32(&mut request, 136, host);
        put_u32(&mut request, 140, 30000);
        let mut lease = Self {
            memory,
            entrypoint,
            request,
        };
        let payload = lease.memory.allocate(bytes.len())?;
        lease.memory.write(payload.address, bytes)?;
        put_u64(&mut lease.request, 112, payload.address);
        let result = Self::invoke_request(&lease.memory, lease.entrypoint, &mut lease.request, 1);
        drop(payload);
        result?;
        Ok(lease)
    }

    fn invoke(&mut self, action: u32) -> Result<u32> {
        Self::invoke_request(&self.memory, self.entrypoint, &mut self.request, action)
    }

    fn invoke_request(
        memory: &ProcessMemory,
        entrypoint: usize,
        request: &mut [u8],
        action: u32,
    ) -> Result<u32> {
        put_u32(request, 8, action);
        request[144..400].fill(0);
        let mut input = memory.allocate(request.len())?;
        memory.write(input.address, request)?;
        let exit = input.run(entrypoint, 3000)?;
        memory.read(input.address, request)?;
        let error = error_text_at(request, 144);
        anyhow::ensure!(exit == 0 && error.is_empty(), "Play loader: {error}");
        read_u32(request, 12)
    }

    pub(crate) fn consumed(&mut self) -> Result<bool> {
        let phase = self.invoke(3)?;
        anyhow::ensure!(
            phase != 4 && phase != 5,
            "Play loader was cancelled before destination consumption"
        );
        Ok(phase == 3)
    }

    pub(crate) fn cancel(&mut self) -> Result<()> {
        self.invoke(2).map(|_| ())
    }
}

impl Drop for NativePlayLoad {
    fn drop(&mut self) {
        if let Err(error) = self.cancel() {
            crate::app::output::log_global(
                3,
                format_args!("[renium] Play loader cleanup: {error:#}"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an explicitly owned Studio process"]
    fn native_launch_nonce_bindings() -> Result<()> {
        let pid = std::env::var("RENIUM_PLAY_LOADER_TEST_PID")?.parse()?;
        let loaded = modules(pid)?;
        let studio = loaded.first().context("Studio ended")?;
        let memory = ProcessMemory::open(pid)?;
        let layout = package_layout(&studio.path)?;
        let title = crate::studio::input::studio_window_title(pid)?;
        let model = active_data_model(pid, &memory, studio, layout.data, &title)?;
        nonce_bindings(&memory, studio, &model)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires an explicitly owned stopped Studio and saved destination"]
    fn native_play_loader_owned_process() -> Result<()> {
        let pid = std::env::var("RENIUM_PLAY_LOADER_TEST_PID")?.parse()?;
        let source = std::env::var("RENIUM_PLAY_LOADER_TEST_SOURCE")?.parse()?;
        let destination = std::env::var("RENIUM_PLAY_LOADER_TEST_DESTINATION")?.parse()?;
        let universe = std::env::var("RENIUM_PLAY_LOADER_TEST_UNIVERSE")?.parse()?;
        let path = std::env::var_os("RENIUM_PLAY_LOADER_TEST_FILE")
            .context("Missing saved destination")?;
        let bytes = std::fs::read(path)?;
        let nonce = std::env::var("RENIUM_PLAY_LOADER_TEST_NONCE")?;
        let title = crate::studio::input::studio_window_title(pid)?;
        let mut lease = NativePlayLoad::arm(
            pid,
            &title,
            (source, universe),
            (destination, universe),
            &bytes,
            &nonce,
            None,
        )?;
        println!("PLAY_LOADER_ARMED pid={pid}");
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            if lease.consumed()? {
                break;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "Owned test did not start before the loader lease expired"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        println!("PLAY_LOADER_CONSUMED pid={pid}");
        Ok(())
    }

    fn fixture(capture: u8, place: i32, universe: i32, instance: i32, fields: [i32; 2]) -> Vec<u8> {
        let mut bytes = vec![0x53, 0x56, 0x57]; // save RBX, RSI, RDI
        bytes.extend([0x48, 0x89, 0xcb]); // RBX = callback
        bytes.extend([0x48, 0x8b, 0x32]); // RSI = *model argument
        bytes.extend([0x48, 0x8b, 0x7b, capture]); // RDI = captured configuration
        bytes.extend([0x48, 0x89, 0xf9, 0x48, 0x8d, 0x96]); // RCX=config, RDX=model instance
        bytes.extend(instance.to_le_bytes());
        bytes.extend([0xe8, 0, 0, 0, 0]); // serialize config string
        for (source, destination) in [place, universe].into_iter().zip(fields) {
            bytes.extend([0x48, 0x8b, 0x86]);
            bytes.extend(source.to_le_bytes());
            bytes.extend([0x48, 0x89, 0x87]);
            bytes.extend(destination.to_le_bytes());
        }
        bytes.extend([0x5f, 0x5e, 0x5b, 0xc3]);
        bytes
    }

    #[test]
    fn loader_follows_relocated_capture_and_native_identity_fields() {
        for capture in [8, 16, 80] {
            for place in [0x398, 0x550] {
                let universe = place + 0x20;
                for fields in [[32, 40], [88, 64]] {
                    let code = fixture(capture, place, universe, 0x1f0, fields);
                    let actual = producer_fields(
                        0,
                        [place as usize, universe as usize],
                        0x1f0,
                        false,
                        |at| (at == 0).then(|| code.clone()),
                    );
                    assert_eq!(
                        actual,
                        Some(Fields {
                            capture: capture as usize,
                            string: 0,
                            place: fields[0] as usize,
                            universe: fields[1] as usize
                        })
                    );
                }
            }
        }
    }

    #[test]
    fn loader_rejects_wrong_receiver_clobbered_fields_and_ambiguous_outputs() {
        let code = fixture(16, 0x398, 0x390, 0x1f0, [32, 40]);
        assert!(
            producer_fields(0, [0x398, 0x390], 0x200, false, |at| (at == 0)
                .then(|| code.clone()))
            .is_none()
        );
        assert!(
            producer_fields(0, [0x398, 0x400], 0x1f0, false, |at| (at == 0)
                .then(|| code.clone()))
            .is_none()
        );
        let duplicate = fixture(16, 0x398, 0x390, 0x1f0, [32, 32]);
        assert!(
            producer_fields(0, [0x398, 0x390], 0x1f0, false, |at| (at == 0)
                .then(|| duplicate.clone()))
            .is_none()
        );
        let mut clobbered = code.clone();
        clobbered.splice(28..28, [0x31, 0xf6]); // lose the model receiver after serialization
        assert!(
            producer_fields(0, [0x398, 0x390], 0x1f0, false, |at| (at == 0)
                .then(|| clobbered.clone()))
            .is_none()
        );
    }

    #[test]
    fn loader_distinguishes_pointer_and_reference_callback_arguments() {
        let reference = fixture(8, 0x398, 0x390, 0x1f0, [32, 40]);
        let mut pointer = reference.clone();
        pointer[6..9].copy_from_slice(&[0x48, 0x89, 0xd6]); // RSI = model, not *model
        for (bytes, direct) in [(&reference, false), (&pointer, true)] {
            assert!(
                producer_fields(0, [0x398, 0x390], 0x1f0, direct, |at| (at == 0)
                    .then(|| bytes.clone()))
                .is_some()
            );
            assert!(
                producer_fields(0, [0x398, 0x390], 0x1f0, !direct, |at| (at == 0)
                    .then(|| bytes.clone()))
                .is_none()
            );
        }
    }

    #[test]
    fn compact_loader_producer_proves_the_model_argument_and_serialized_identity() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../native/tests/windows-loader-dispatch.json"
        ))
        .unwrap();
        let rows = fixture["producer"].as_array().unwrap();
        for slide in [0, 0x40000000] {
            let entry = rows[0]["entry"].as_u64().unwrap() as usize + slide;
            for direct in [false, true] {
                let actual = producer_fields(entry, [0x398, 0x390], 0x1f0, direct, |at| {
                    rows.iter().find_map(|row| {
                        (row["entry"].as_u64()? as usize + slide == at)
                            .then(|| hex(row["hex"].as_str().unwrap()).unwrap())
                    })
                });
                assert_eq!(
                    actual,
                    direct.then_some(Fields {
                        capture: 8,
                        string: 0,
                        place: 32,
                        universe: 40
                    })
                );
            }
        }
    }
}
