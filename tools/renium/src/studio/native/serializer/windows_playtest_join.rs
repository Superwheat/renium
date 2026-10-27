//! Native arrival fields derived from the reflected getters, never build offsets.
use super::*;
use iced_x86::{
    Decoder, DecoderOptions, FlowControl, Instruction, InstructionInfoFactory, Mnemonic, OpAccess,
    OpKind, Register,
};

const EXECUTE: &str = ".?AV?$BoundYieldFuncDesc@VStudioTestService@Studio@RBX@@$$A6A?AVVariant@Reflection@3@V453@@Z$0A@$00@Reflection@RBX@@";
const ARGS: &str = ".?AV?$BoundFuncDesc@VStudioTestService@Studio@RBX@@$$A6A?AVVariant@Reflection@3@XZ$0A@$0A@@Reflection@RBX@@";
const JOIN: &str = ".?AV?$CustomBoundFuncDesc@VPlayer@RBX@@$$A6A?AV?$shared_ptr@$$CBVValueTable@Reflection@RBX@@@std@@XZ$0A@$0A@@Reflection@RBX@@";
const LOCAL: &str = ".?AV?$BoundFuncDesc@VTeleportService@RBX@@$$A6A?AVVariant@Reflection@2@XZ$0A@$0A@@Reflection@RBX@@";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    Receiver,
    Output,
    Stack,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Symbol {
    Pointer(Origin, i64),
    Field(i64),
}
#[derive(Clone)]
struct Flow {
    registers: HashMap<Register, Symbol>,
    stack: HashMap<i64, Symbol>,
    output: HashMap<i64, Symbol>,
    copied: Option<i64>,
}
impl Flow {
    fn new() -> Self {
        Self {
            registers: HashMap::from([
                (Register::RCX, Symbol::Pointer(Origin::Receiver, 0)),
                (Register::RDX, Symbol::Pointer(Origin::Output, 0)),
                (Register::RSP, Symbol::Pointer(Origin::Stack, 0)),
            ]),
            stack: HashMap::new(),
            output: HashMap::new(),
            copied: None,
        }
    }
    fn reg(&self, reg: Register) -> Option<Symbol> {
        self.registers.get(&reg.full_register()).copied()
    }
    fn address(&self, i: &Instruction) -> Option<(Origin, i64)> {
        if i.memory_index() != Register::None {
            return None;
        }
        let Symbol::Pointer(base, offset) = self.reg(i.memory_base())? else {
            return None;
        };
        Some((base, offset.checked_add(i.memory_displacement64() as i64)?))
    }
    fn value(&self, i: &Instruction, op: u32) -> Option<Symbol> {
        match i.op_kind(op) {
            OpKind::Register if i.op_register(op).size() == 8 => self.reg(i.op_register(op)),
            OpKind::Memory if i.memory_size().size() == 8 => match self.address(i)? {
                (Origin::Receiver, at) => Some(Symbol::Field(at)),
                (Origin::Stack, at) => self.stack.get(&at).copied(),
                (Origin::Output, at) => self.output.get(&at).copied(),
            },
            _ => None,
        }
    }
    fn step(&mut self, i: &Instruction) {
        let address = self.address(i);
        let value = match i.mnemonic() {
            Mnemonic::Mov => self.value(i, 1),
            Mnemonic::Lea => address.map(|(base, at)| Symbol::Pointer(base, at)),
            Mnemonic::Add | Mnemonic::Sub if i.op0_kind() == OpKind::Register => {
                match self.reg(i.op0_register()) {
                    Some(Symbol::Pointer(base, at))
                        if matches!(
                            i.op1_kind(),
                            OpKind::Immediate8to64 | OpKind::Immediate32to64
                        ) =>
                    {
                        let delta = if i.op1_kind() == OpKind::Immediate8to64 {
                            i.immediate8to64()
                        } else {
                            i.immediate32to64()
                        };
                        at.checked_add(if i.mnemonic() == Mnemonic::Sub {
                            -delta
                        } else {
                            delta
                        })
                        .map(|at| Symbol::Pointer(base, at))
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let stack = self.reg(Register::RSP);
        let mut info = InstructionInfoFactory::new();
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
        if i.op0_kind() == OpKind::Register {
            if let Some(value) = value {
                self.registers
                    .insert(i.op0_register().full_register(), value);
            }
        } else if i.op0_kind() == OpKind::Memory
            && i.mnemonic() == Mnemonic::Mov
            && let Some((base, at)) = address
        {
            let slots = match base {
                Origin::Stack => Some(&mut self.stack),
                Origin::Output => Some(&mut self.output),
                _ => None,
            };
            if let Some(slots) = slots {
                slots.retain(|offset, _| {
                    *offset + 8 <= at || *offset >= at + i.memory_size().size() as i64
                });
                if let Some(value) = value {
                    slots.insert(at, value);
                }
            }
        }
        if matches!(i.mnemonic(), Mnemonic::Push | Mnemonic::Pop)
            && let Some(Symbol::Pointer(Origin::Stack, at)) = stack
        {
            self.registers.insert(
                Register::RSP,
                Symbol::Pointer(
                    Origin::Stack,
                    at + if i.mnemonic() == Mnemonic::Push {
                        -8
                    } else {
                        8
                    },
                ),
            );
        }
    }
}

fn join_field(code: &[u8]) -> Option<usize> {
    let mut flow = Flow::new();
    for i in Decoder::new(64, code, DecoderOptions::NONE) {
        if i.is_invalid() {
            return None;
        }
        if i.flow_control() == FlowControl::Call {
            return match (flow.reg(Register::RCX), flow.reg(Register::RDX)) {
                (
                    Some(Symbol::Pointer(Origin::Output, 0)),
                    Some(Symbol::Pointer(Origin::Receiver, field)),
                ) if (16..65536).contains(&field) && field % 8 == 0 => Some(field as usize),
                _ => None,
            };
        }
        if i.flow_control() != FlowControl::Next {
            return None;
        }
        flow.step(&i);
    }
    None
}

fn variant_field(code: &[u8]) -> Option<usize> {
    let instructions = Decoder::new(64, code, DecoderOptions::NONE)
        .into_iter()
        .map(|i| (i.ip(), i))
        .collect::<HashMap<_, _>>();
    let mut pending = vec![(0, Flow::new())];
    let mut fields = HashSet::new();
    let mut copied = false;
    let mut budget = 1024usize;
    while let Some((mut pc, mut flow)) = pending.pop() {
        loop {
            budget = budget.checked_sub(1)?;
            let i = instructions.get(&pc)?;
            if i.is_invalid() {
                return None;
            }
            match i.flow_control() {
                FlowControl::Return => {
                    let Symbol::Field(field) = *flow.output.get(&0)? else {
                        return None;
                    };
                    if !(16..65536).contains(&field)
                        || field % 8 != 0
                        || flow.output.get(&8) != Some(&Symbol::Field(field + 8))
                        || flow.copied.is_some_and(|source| source != field)
                    {
                        return None;
                    }
                    copied |= flow.copied.is_some();
                    fields.insert(field as usize);
                    break;
                }
                FlowControl::ConditionalBranch => {
                    pending.push((i.near_branch_target(), flow.clone()))
                }
                FlowControl::UnconditionalBranch => {
                    pc = i.near_branch_target();
                    continue;
                }
                FlowControl::IndirectCall
                    if i.op0_kind() == OpKind::Memory
                        && i.memory_displacement64() == 0
                        && i.memory_index() == Register::None =>
                {
                    let Symbol::Field(manager) = flow.reg(i.memory_base())? else {
                        return None;
                    };
                    if flow.reg(Register::RCX)
                        != Some(Symbol::Pointer(Origin::Receiver, manager + 8))
                        || flow.reg(Register::RDX) != Some(Symbol::Pointer(Origin::Output, 16))
                    {
                        return None;
                    }
                    flow.copied = Some(manager - 8);
                    for reg in [
                        Register::RAX,
                        Register::RCX,
                        Register::RDX,
                        Register::R8,
                        Register::R9,
                        Register::R10,
                        Register::R11,
                    ] {
                        flow.registers.remove(&reg);
                    }
                }
                FlowControl::Next => flow.step(i),
                _ => return None,
            }
            pc = i.next_ip();
        }
    }
    (copied && fields.len() == 1).then(|| *fields.iter().next().unwrap())
}

fn bindings(memory: &ProcessMemory, studio: &ModuleEntry) -> Result<Vec<(usize, usize)>> {
    let layout = package_layout(&studio.path)?;
    let args = types::member(memory, studio, ARGS, "GetTestArgs", false)?;
    let join = types::member(memory, studio, JOIN, "GetJoinData", true)?;
    let local = types::member(memory, studio, LOCAL, "GetLocalPlayerTeleportData", false)?;
    let join_field = join_field(&properties::dispatch_code(memory, studio, &layout, join.1)?)
        .context("GetJoinData no longer forwards its native table to Lua")?;
    let local_field = variant_field(&properties::dispatch_code(
        memory, studio, &layout, local.1,
    )?)
    .context("GetLocalPlayerTeleportData no longer copies its native Variant")?;
    Ok(vec![
        (88, args.1),
        (96, join.0),
        (104, join.1),
        (112, join_field),
        (120, local.0),
        (128, local.1),
        (136, local_field),
        (
            184,
            types::singleton(memory, studio, dispatch::TABLE, None)?,
        ),
    ])
}

pub(in super::super) fn probe(memory: &ProcessMemory, studio: &ModuleEntry) -> Result<Value> {
    Ok(json!({"bindings": bindings(memory, studio)?}))
}

pub(crate) struct NativePlayJoin {
    pid: u32,
    memory: ProcessMemory,
    entrypoint: usize,
    request: Vec<u8>,
    armed: bool,
}

/// Each separate process must create its own native arrival values before its
/// player callbacks run. Existing server players keep their original records.
pub(crate) struct NativeTestJoin {
    memory: ProcessMemory,
    entry: usize,
    request: Vec<u8>,
}
impl NativeTestJoin {
    pub(crate) fn arm(
        pid: u32,
        runtime: &str,
        path: &[String],
        place: i64,
        universe: i64,
        client: bool,
        identity: &Value,
    ) -> Result<Self> {
        anyhow::ensure!(
            path == ["CoreGui", &format!("__ReniumRouteJoin_{runtime}")],
            "Invalid routed arrival relay"
        );
        let loaded = modules(pid)?;
        let studio = loaded.first().context("Routed arrival process ended")?;
        let title = crate::studio::input::studio_window_title(pid)?;
        let memory = ProcessMemory::open(pid)?;
        memory.authorize(NativeOperation::PlayTeleportJoin, &title)?;
        let layout = package_layout(&studio.path)?;
        let model =
            active_data_model_for_path(pid, &memory, studio, layout.data, &title, Some(path))?;
        let relays = properties::resolve_path(&memory, &model, path, &[1, 1])?;
        let relay = relays.last().context("Routed arrival relay ended")?;
        let fields = owner::task_binding(&memory, studio, &model)?.0;
        let native = bindings(&memory, studio)?
            .into_iter()
            .collect::<HashMap<_, _>>();
        let mut request = vec![0; 760];
        put_u32(&mut request, 0, 2);
        put_u32(&mut request, 4, 760);
        put_u32(&mut request, 12, u32::from(client));
        let mut token = [0; 8];
        getrandom::fill(&mut token)
            .map_err(|error| anyhow::anyhow!("Cannot identify routed arrival: {error}"))?;
        put_u64(&mut request, 16, u64::from_le_bytes(token).max(1) as usize);
        for (index, field) in fields.into_iter().enumerate() {
            put_u64(&mut request, 24 + index * 8, field);
        }
        const INVOKE: &str = ".?AV?$BoundYieldFuncDesc@VBindableFunction@RBX@@$$A6A?AV?$shared_ptr@$$CBUTuple@Reflection@RBX@@@std@@V34@@Z$0A@$00@Reflection@RBX@@";
        for (index, value) in [
            relay.instance,
            relay.owner,
            types::yield_member(&memory, studio, INVOKE, "Invoke", 1)?.function,
            native[&96],
            native[&104],
            native[&112],
            native[&120],
            native[&128],
            native[&136],
            place as usize,
            universe as usize,
            crate::app::update::process_start_identity(std::process::id())
                .context("Arrival host ended")?
                .parse()?,
        ]
        .into_iter()
        .enumerate()
        {
            put_u64(&mut request, 152 + index * 8, value);
        }
        put_u32(&mut request, 248, std::process::id());
        if client {
            let user_id = identity["userId"]
                .as_i64()
                .filter(|id| *id < 0)
                .context("Invalid original test-player UserId")?;
            put_u64(&mut request, 256, user_id as usize);
            for (i, (at, writer)) in super::identity::bindings(&memory, studio)?
                .into_iter()
                .enumerate()
            {
                put_u64(&mut request, 264 + i * 8, at);
                put_u64(&mut request, 288 + i * 8, writer);
            }
            for (name, offset, size) in [("name", 312, 64), ("displayName", 376, 128)] {
                let text = identity[name]
                    .as_str()
                    .context("Missing original test-player name")?;
                anyhow::ensure!(
                    !text.is_empty() && text.len() < size && !text.contains('\0'),
                    "Invalid original test-player name"
                );
                request[offset..offset + text.len()].copy_from_slice(text.as_bytes());
            }
        }
        let helper = ensure_play_helper_loaded(pid, &memory, &loaded)?;
        let mut lease = Self {
            memory,
            entry: helper + helper_export_rva("ReniumTestJoin")?,
            request,
        };
        lease.invoke(1)?;
        Ok(lease)
    }
    fn invoke(&mut self, action: u32) -> Result<()> {
        put_u32(&mut self.request, 8, action);
        self.request[504..].fill(0);
        let mut input = self.memory.allocate(self.request.len())?;
        self.memory.write(input.address, &self.request)?;
        let exit = input.run(self.entry, 5000)?;
        self.memory.read(input.address, &mut self.request)?;
        let error = error_text_at(&self.request, 504);
        anyhow::ensure!(exit == 0 && error.is_empty(), "Routed arrival: {error}");
        Ok(())
    }
}
impl Drop for NativeTestJoin {
    fn drop(&mut self) {
        let _ = self.invoke(2);
    }
}
impl NativePlayJoin {
    pub(crate) fn prepare(pid: u32, title: &str, shared_launch: bool) -> Result<Self> {
        let loaded = modules(pid)?;
        let studio = loaded.first().context("Studio process ended")?;
        let memory = ProcessMemory::open(pid)?;
        memory.authorize(NativeOperation::PlayTeleportJoin, title)?;
        let mut request = vec![0; 608];
        put_u32(&mut request, 0, 2);
        put_u32(&mut request, 4, 608);
        for (at, value) in bindings(&memory, studio)? {
            put_u64(&mut request, at, value);
        }
        if !shared_launch {
            let execute = types::yield_member(&memory, studio, EXECUTE, "ExecutePlayModeAsync", 1)?;
            put_u64(&mut request, 72, execute.slot);
            put_u64(&mut request, 80, execute.function);
        }
        let mut token = [0; 8];
        getrandom::fill(&mut token)
            .map_err(|e| anyhow::anyhow!("Cannot create arrival identity: {e}"))?;
        put_u64(&mut request, 16, u64::from_le_bytes(token).max(1) as usize);
        let host = std::process::id();
        let created = crate::app::update::process_start_identity(host)
            .context("Arrival host ended")?
            .parse::<usize>()?;
        put_u64(&mut request, 208, created);
        put_u32(&mut request, 216, host);
        put_u32(&mut request, 220, 90000);
        let helper = ensure_play_helper_loaded(pid, &memory, &loaded)?;
        Ok(Self {
            pid,
            memory,
            entrypoint: helper + helper_export_rva("ReniumPlayJoin")?,
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
        root: Option<&dispatch::NativeTeleportDispatch>,
    ) -> Result<()> {
        anyhow::ensure!(
            !nonce.is_empty() && nonce.len() < 128,
            "Invalid native arrival launch identity"
        );
        let loaded = modules(self.pid)?;
        let studio = loaded.first().context("Studio process ended")?;
        let title = crate::studio::input::studio_window_title(self.pid)?;
        let model = if let Some(root) = root {
            root.root_model(studio)?
        } else {
            active_data_model(
                self.pid,
                &self.memory,
                studio,
                package_layout(&studio.path)?.data,
                &title,
            )?
        };
        let instance = model.outer + model.layout.data_model_instance;
        let place =
            integer_property_layout(&self.memory, studio, instance, model.layout, "PlaceId")?;
        let universe =
            integer_property_layout(&self.memory, studio, instance, model.layout, "GameId")?;
        anyhow::ensure!(
            place.width == 8
                && universe.width == 8
                && read_integer_property(&self.memory, instance, place)? == source.0
                && read_integer_property(&self.memory, instance, universe)? == source.1,
            "Source Edit changed before native arrival arming"
        );
        let test = model
            .roots
            .iter()
            .find(|entry| {
                read_instance_class(&self.memory, entry.instance, model.layout).as_deref()
                    == Some("StudioTestService")
            })
            .context("Source Edit has no test controller")?;
        let check = loader::nonce_bindings(&self.memory, studio, &model)?;
        for (at, value) in [
            (24, dispatch as usize),
            (32, attempt as usize),
            (40, destination as usize),
            (48, source.1 as usize),
            (56, instance),
            (64, test.instance),
            (144, properties::parent_offset(&self.memory, &model)?),
            (152, model.layout.children),
            (160, model.layout.class_descriptor),
            (168, place.offset),
            (176, universe.offset),
            (192, check[0]),
            (200, check[1]),
        ] {
            put_u64(&mut self.request, at, value);
        }
        self.request[224..352].fill(0);
        self.request[224..224 + nonce.len()].copy_from_slice(nonce.as_bytes());
        self.invoke(1)?;
        self.armed = true;
        Ok(())
    }
    fn invoke(&mut self, action: u32) -> Result<()> {
        put_u32(&mut self.request, 8, action);
        put_u32(&mut self.request, 12, 0);
        self.request[352..608].fill(0);
        let mut input = self.memory.allocate(self.request.len())?;
        self.memory.write(input.address, &self.request)?;
        let exit = input.run(self.entrypoint, 3000)?;
        self.memory.read(input.address, &mut self.request)?;
        let error = error_text_at(&self.request, 352);
        anyhow::ensure!(exit == 0 && error.is_empty(), "Native arrival: {error}");
        Ok(())
    }

    pub(crate) fn receipt(&mut self) -> Result<u32> {
        self.invoke(3)?;
        read_u32(&self.request, 12)
    }
}
impl Drop for NativePlayJoin {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.invoke(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn join_field_tracks_receiver_and_lua_state() {
        for field in [0x100i32, 0x420, 0x5000] {
            let mut code = vec![0x48, 0x89, 0xd7, 0x48, 0x8d, 0x91];
            code.extend(field.to_le_bytes());
            code.extend([0x48, 0x89, 0xf9, 0xe8, 0, 0, 0, 0]);
            assert_eq!(join_field(&code), Some(field as usize));
            code[5] = 0x92; // Receiver replaced by the Lua state.
            assert_eq!(join_field(&code), None);
        }
    }
    #[test]
    fn variant_getter_requires_engine_copy_and_both_output_words() {
        for field in [0x100i32, 0x2b0, 0x5000] {
            let mut code = vec![0x48, 0x89, 0xcf, 0x48, 0x89, 0xd3, 0x48, 0x8b, 0x81];
            code.extend(field.to_le_bytes());
            code.extend([0x48, 0x89, 0x02, 0x48, 0x8b, 0x81]);
            code.extend((field + 8).to_le_bytes());
            code.extend([0x48, 0x83, 0xc2, 16, 0x48, 0x81, 0xc1]);
            code.extend((field + 16).to_le_bytes());
            code.extend([0xff, 0x10, 0x48, 0x8b, 0x87]);
            code.extend((field + 8).to_le_bytes());
            code.extend([0x48, 0x89, 0x43, 8, 0xc3]);
            assert_eq!(variant_field(&code), Some(field as usize));
            let last = code.len() - 2;
            code[last] = 16;
            assert_eq!(variant_field(&code), None);
        }
    }
    #[test]
    #[ignore = "requires the explicitly owned Studio process"]
    fn owned_native_join_bindings() -> Result<()> {
        let pid = std::env::var("RENIUM_TELEPORT_TEST_PID")?.parse()?;
        let memory = ProcessMemory::open_read_only(pid)?;
        let loaded = modules(pid)?;
        text_bindings(probe(&memory, &loaded[0])?);
        Ok(())
    }
    fn text_bindings(value: Value) {
        println!("{value}");
    }
}
