//! Protected reflection calls. Discovery and authorization stay in Rust; the
//! helper only owns C++ objects and calls engine methods on the DataModel queue.
use super::*;
use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction, Mnemonic, OpKind, Register};
use iced_x86::{InstructionInfoFactory, OpAccess};
use std::io::{Read, Seek, SeekFrom};

#[path = "windows_history.rs"]
mod history;

#[path = "windows_terrain.rs"]
mod terrain;
pub(crate) use terrain::prepare_terrain;
#[path = "windows_terrain_observation.rs"]
mod terrain_observation;
pub(crate) use terrain_observation::observe_terrain;

pub(crate) fn register_history(pid: u32, title: &str, token: &str) -> Result<()> {
    anyhow::ensure!(
        !token.is_empty() && token.len() <= 256,
        "Invalid Studio recording token"
    );
    let mut prepared = prepare_property(
        pid,
        title,
        &["ChangeHistoryService".into()],
        &[],
        "Name",
        Duration::from_secs(3),
    )?;
    prepared.invoke(6)?;
    let mut binding = prepared.parameters[96..176].to_vec();
    if binding.iter().all(|byte| *byte == 0) {
        binding = history::binding(&prepared, pid, title)?;
    }
    binding.extend_from_slice(token.as_bytes());
    put_u32(&mut prepared.parameters, 65916, binding.len() as u32);
    prepared.parameters[INPUT..INPUT + binding.len()].copy_from_slice(&binding);
    prepared.invoke(6)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Operand {
    Descriptor,
    Output,
    Instance,
    Scratch,
    Input,
    DescriptorField,
    Binding(usize),
    Vtable(usize),
}

// Track ABI argument relationships, not the compiler's choice of scratch
// register or descriptor field offset. Decode instructions so byte sequences
// inside an immediate/displacement cannot masquerade as a call.
#[derive(Clone, Copy, Eq, PartialEq)]
enum GetterAbi {
    Text,
    Identity,
    Variant,
}

fn setter_operand(
    i: &Instruction,
    registers: &HashMap<Register, Operand>,
    bindings: &[usize],
) -> Option<Operand> {
    use Operand::*;
    if i.op1_kind() == OpKind::Register {
        return registers.get(&i.op1_register().full_register()).copied();
    }
    if i.op1_kind() != OpKind::Memory || i.memory_index() != Register::None {
        return None;
    }
    let base = i.memory_base().full_register();
    let offset = i.memory_displacement64() as usize;
    match (registers.get(&base), i.mnemonic()) {
        (Some(Descriptor), Mnemonic::Mov) if bindings.contains(&offset) => Some(Binding(offset)),
        (Some(Descriptor), Mnemonic::Mov) => Some(DescriptorField),
        (Some(Binding(field)), Mnemonic::Mov) if offset == 0 => Some(Vtable(*field)),
        (Some(Input), Mnemonic::Mov) if offset == 0 => Some(Input),
        (Some(Instance), Mnemonic::Lea) => Some(Instance),
        (_, Mnemonic::Lea) if matches!(base, Register::RSP | Register::RBP) => Some(Scratch),
        _ => None,
    }
}

#[derive(Clone)]
struct SetterPath {
    pc: u64,
    registers: HashMap<Register, Operand>,
    parsed_input: bool,
}

fn setter_instruction(
    i: &Instruction,
    state: &mut SetterPath,
    bindings: &[usize],
) -> Option<(usize, usize)> {
    use Operand::*;
    let r = &mut state.registers;
    let destination = i.op0_register().full_register();
    match i.mnemonic() {
        Mnemonic::Mov | Mnemonic::Movzx | Mnemonic::Lea if i.op0_kind() == OpKind::Register => {
            let value = setter_operand(i, r, bindings);
            r.remove(&destination);
            if let Some(value) = value {
                r.insert(destination, value);
            }
        }
        Mnemonic::Call => {
            let mut binding = None;
            if i.op0_kind() == OpKind::Memory && i.memory_index() == Register::None {
                if let Some(Vtable(field)) = r.get(&i.memory_base().full_register())
                    && r.get(&Register::RCX) == Some(&Binding(*field))
                    && r.get(&Register::RDX) == Some(&Instance)
                    && r.get(&Register::R8) == Some(&Scratch)
                    && state.parsed_input
                {
                    binding = Some((*field, i.memory_displacement64() as usize));
                }
            } else if i.op0_kind() == OpKind::NearBranch64 {
                state.parsed_input |= r.get(&Register::RCX) == Some(&Input)
                    && r.get(&Register::RDX) == Some(&Scratch)
                    || r.get(&Register::RCX) == Some(&DescriptorField)
                        && r.get(&Register::RDX) == Some(&Input)
                        && r.get(&Register::R8) == Some(&Scratch);
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
                r.remove(&register);
            }
            return binding;
        }
        Mnemonic::Push | Mnemonic::Cmp | Mnemonic::Test | Mnemonic::Nop => {}
        _ => {
            r.remove(&destination);
        }
    }
    None
}

// Follow both arms of conversion guards: bool setters can return false before
// their successful write path. Branch targets must be decoded boundaries, not
// incidental bytes inside an immediate. Unknown/looping layouts fail bounded.
fn setter_call(code: &[u8], bindings: &[usize]) -> Option<(usize, usize)> {
    let instructions = Decoder::new(64, code, DecoderOptions::NONE)
        .into_iter()
        .map(|i| (i.ip(), i))
        .collect::<HashMap<_, _>>();
    let mut pending = vec![SetterPath {
        pc: 0,
        parsed_input: false,
        registers: HashMap::from([
            (Register::RCX, Operand::Descriptor),
            (Register::RDX, Operand::Instance),
            (Register::R8, Operand::Input),
        ]),
    }];
    let mut calls = std::collections::HashSet::new();
    let mut steps = 0;
    while let Some(mut state) = pending.pop() {
        loop {
            steps += 1;
            if steps > 4096 || pending.len() > 64 {
                return None;
            }
            let i = instructions.get(&state.pc)?;
            if i.is_invalid() {
                return None;
            }
            match i.flow_control() {
                FlowControl::Return | FlowControl::Exception => break,
                FlowControl::ConditionalBranch => {
                    let mut branch = state.clone();
                    branch.pc = i.near_branch_target();
                    if branch.pc <= state.pc {
                        return None;
                    }
                    pending.push(branch);
                }
                FlowControl::UnconditionalBranch => {
                    let target = i.near_branch_target();
                    if target <= state.pc {
                        return None;
                    }
                    state.pc = target;
                    continue;
                }
                FlowControl::IndirectBranch => return None,
                _ => {
                    if let Some(call) = setter_instruction(i, &mut state, bindings) {
                        calls.insert(call);
                    }
                }
            }
            state.pc = i.next_ip();
        }
    }
    (calls.len() == 1).then(|| *calls.iter().next().unwrap())
}

fn getter_call(code: &[u8], bindings: &[usize], abi: GetterAbi) -> Option<(usize, usize)> {
    use Operand::*;
    let direct_binding = abi == GetterAbi::Identity;
    let mut registers = HashMap::from([
        (
            Register::RCX,
            if direct_binding {
                Binding(0)
            } else {
                Descriptor
            },
        ),
        (
            Register::RDX,
            if abi == GetterAbi::Variant {
                Instance
            } else {
                Output
            },
        ),
        (
            Register::R8,
            if abi == GetterAbi::Variant {
                Output
            } else {
                Instance
            },
        ),
    ]);
    let mut call = None;
    let mut returned_output = false;
    for instruction in Decoder::new(64, code, DecoderOptions::NONE) {
        if instruction.is_invalid() {
            return None;
        }
        let destination = instruction.op0_register().full_register();
        let base = instruction.memory_base().full_register();
        let displacement = instruction.memory_displacement64() as usize;
        match instruction.mnemonic() {
            Mnemonic::Ret => {
                return (abi == GetterAbi::Variant
                    || returned_output
                    || registers.get(&Register::RAX) == Some(&Output))
                .then_some(call)
                .flatten();
            }
            Mnemonic::Int3 => {} // abort arm of an allocator/security check, not the normal epilogue
            Mnemonic::Mov | Mnemonic::Lea if instruction.op0_kind() == OpKind::Register => {
                let value = if instruction.op1_kind() == OpKind::Register {
                    registers
                        .get(&instruction.op1_register().full_register())
                        .copied()
                } else if instruction.op1_kind() == OpKind::Memory
                    && instruction.memory_index() == Register::None
                {
                    match (registers.get(&base), instruction.mnemonic()) {
                        (Some(Descriptor), Mnemonic::Mov) if bindings.contains(&displacement) => {
                            Some(Binding(displacement))
                        }
                        (Some(Binding(offset)), Mnemonic::Mov) if displacement == 0 => {
                            Some(Vtable(*offset))
                        }
                        (Some(Instance), Mnemonic::Lea) => Some(Instance),
                        (_, Mnemonic::Lea) if matches!(base, Register::RSP | Register::RBP) => {
                            Some(Scratch)
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                registers.remove(&destination);
                if destination == Register::RAX {
                    returned_output = value == Some(Output) && call.is_some();
                }
                if let Some(value) = value {
                    registers.insert(destination, value);
                }
            }
            Mnemonic::Call => {
                if instruction.op0_kind() == OpKind::Memory
                    && instruction.memory_index() == Register::None
                {
                    let target = registers.get(&base).copied();
                    let receiver = registers.get(&Register::RCX);
                    let instance = (registers.get(&Register::R8) == Some(&Instance)
                        && matches!(registers.get(&Register::RDX), Some(Output | Scratch)))
                        || (abi != GetterAbi::Variant
                            && registers.get(&Register::RDX) == Some(&Instance));
                    if let Some(Vtable(offset)) = target {
                        if receiver == Some(&Binding(offset))
                            && instance
                            && displacement.is_multiple_of(8)
                        {
                            call = Some((offset, displacement));
                        }
                    } else if direct_binding
                        && target == Some(Binding(0))
                        && receiver == Some(&Instance)
                        && registers.get(&Register::RDX) == Some(&Output)
                    {
                        call = Some((0, displacement));
                    }
                }
                for reg in [
                    Register::RAX,
                    Register::RCX,
                    Register::RDX,
                    Register::R8,
                    Register::R9,
                    Register::R10,
                    Register::R11,
                ] {
                    registers.remove(&reg);
                }
            }
            Mnemonic::Cmove => {} // nullable-this adjustment; the instance is known non-null
            Mnemonic::Push | Mnemonic::Pop | Mnemonic::Cmp | Mnemonic::Test | Mnemonic::Nop => {}
            _ => {
                registers.remove(&destination);
                if destination == Register::RAX {
                    returned_output = false;
                }
            }
        }
    }
    None
}

const SIZE: usize = 132048;
const IDENTITY: usize = 65920;
const EXPECTED_IDENTITY: usize = 65936;
const INPUT: usize = 65952;
const LIMIT: usize = 65536;

#[derive(Clone)]
struct AbiCacheEntry {
    descriptor_binding_offset: Option<usize>,
    setter_supported: bool,
    functions: Vec<(usize, usize, Vec<u8>)>, // vtable byte offset, RVA, verified code
}

type AbiCacheKey = (PathBuf, [u32; 3], usize);
static TEXT_ABIS: OnceLock<Mutex<HashMap<AbiCacheKey, AbiCacheEntry>>> = OnceLock::new();
static IDENTITY_ABIS: OnceLock<Mutex<HashMap<AbiCacheKey, AbiCacheEntry>>> = OnceLock::new();

fn cached_abi(
    cache: &OnceLock<Mutex<HashMap<AbiCacheKey, AbiCacheEntry>>>,
    key: &AbiCacheKey,
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    descriptor: usize,
) -> Option<AbiCacheEntry> {
    let entry = cache
        .get()?
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(key)?
        .clone();
    let object = match entry.descriptor_binding_offset {
        Some(offset) => memory.read_u64(descriptor + offset).ok()? as usize,
        None => descriptor,
    };
    let vtable = memory.read_u64(object).ok()? as usize;
    if entry.functions.iter().all(|(offset, rva, code)| {
        memory.read_u64(vtable + offset).ok() == Some((studio.base + rva) as u64)
            && memory.read_vec(studio.base + rva, code.len()).ok().as_ref() == Some(code)
    }) {
        Some(entry)
    } else {
        cache
            .get()?
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key);
        None
    }
}

pub(crate) struct NativeProperty {
    pub(crate) class_name: String,
    pub(crate) instance_id: String,
    property: String,
    memory: ProcessMemory,
    parameters: Vec<u8>,
    entrypoint: usize,
    deadline: Instant,
}

impl NativeProperty {
    pub(crate) fn ensure_writable(&self) -> Result<()> {
        anyhow::ensure!(
            read_u64(&self.parameters, 65904)? != 0,
            "This engine property has no supported setter"
        );
        Ok(())
    }
    pub(crate) fn remaining(&self) -> Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .context("Protected property operation exceeded its deadline")
    }

    fn invoke(&mut self, operation: u32) -> Result<()> {
        let remaining = self.remaining()?;
        let timeout = u32::try_from(remaining.as_millis().clamp(1, 3000))?;
        put_u32(&mut self.parameters, 80, timeout);
        put_u32(&mut self.parameters, 84, 0);
        put_u32(&mut self.parameters, 65912, operation);
        let mut remote = self.memory.allocate(self.parameters.len())?;
        if operation == 7 {
            put_u64(&mut self.parameters, 132032, remote.address + SIZE);
            let length = self.parameters.len() - SIZE;
            put_u64(&mut self.parameters, 132040, length);
        }
        self.memory.write(remote.address, &self.parameters)?;
        let exit = remote.run(self.entrypoint, timeout + 500)?;
        self.memory
            .read(remote.address, &mut self.parameters[..SIZE])?;
        let status = read_u32(&self.parameters, 84)?;
        if exit != 0 || status != 4 {
            bail!(
                "Studio protected property call failed (0x{exit:X}/0x{status:X}): {}",
                error_text_at(&self.parameters[..65888], 65632)
            );
        }
        Ok(())
    }

    pub(crate) fn read(&mut self) -> Result<String> {
        self.invoke(1)?;
        self.value()
    }

    pub(crate) fn write(&mut self, text: &str) -> Result<String> {
        self.ensure_writable()?;
        if text.len() > LIMIT {
            bail!("Protected property input exceeds 64 KiB");
        }
        put_u32(&mut self.parameters, 65916, text.len() as u32);
        self.parameters[INPUT..INPUT + text.len()].copy_from_slice(text.as_bytes());
        self.invoke(2)?;
        let mut actual = self.value()?;
        let mut pause_ms = 0;
        // Some engine setters (mesh collision cooking, for example) finish on
        // another task. Never repeat the mutation or block Studio's queue while
        // waiting for it. A setter acknowledgement alone is not success.
        while !crate::automation::property_access::property_text_matches(
            &self.class_name,
            &self.property,
            text,
            &actual,
        ) {
            if Instant::now() + Duration::from_millis(pause_ms) >= self.deadline {
                bail!(
                    "Studio accepted the write but its value was not verified before the deadline; it may still finish. Read the property before retrying"
                );
            }
            if pause_ms != 0 {
                std::thread::sleep(Duration::from_millis(pause_ms));
            }
            actual = self.read()?;
            pause_ms = (pause_ms * 2).clamp(1, 8);
        }
        Ok(actual)
    }

    fn value(&self) -> Result<String> {
        let len = read_u32(&self.parameters, 88)? as usize;
        if len > LIMIT {
            bail!("Studio returned an invalid property length");
        }
        String::from_utf8(self.parameters[96..96 + len].to_vec())
            .context("Property is not UTF-8 text; it requires a binary value codec")
    }
}

// Read only the relevant executable bytes, not the whole Studio image on every
// lookup. The loaded image and the actual code both have to match the file.
pub(super) fn verified_code(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    address: usize,
    len: usize,
) -> Result<Vec<u8>> {
    let offset = address
        .checked_sub(studio.base + layout.text.virtual_address)
        .filter(|offset| {
            offset
                .checked_add(len)
                .is_some_and(|end| end <= layout.text.raw_size)
        })
        .context("Property function is outside Studio's executable section")?;
    let mut file = fs::File::open(&studio.path)?;
    file.seek(SeekFrom::Start((layout.text.raw_offset + offset) as u64))?;
    let mut expected = vec![0; len];
    file.read_exact(&mut expected)?;
    if memory.read_vec(address, len)? != expected {
        bail!("Studio property function differs from its executable; refusing an unverified call");
    }
    Ok(expected)
}

fn true_return(mut code: &[u8]) -> bool {
    if code.starts_with(&[0x40]) {
        code = &code[1..];
    }
    code.starts_with(&[0xb0, 1, 0xc3]) || code.starts_with(&[0xb8, 1, 0, 0, 0, 0xc3])
}

fn bindings(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    descriptor: usize,
) -> Result<Vec<(usize, usize)>> {
    let bytes = memory.read_vec(descriptor, 0x200)?;
    let mut result = Vec::new();
    for offset in (0x40..0x200).step_by(8) {
        let pointer = read_u64(&bytes, offset)? as usize;
        if !likely_pointer(pointer) {
            continue;
        }
        if read_rtti_type(memory, pointer, studio.base, studio.size).is_some_and(|kind| {
            (kind.starts_with(".?AV?$GetSetImpl@") || kind.starts_with(".?AV?$GetImpl@"))
                && kind.contains("@?$PropDescriptor@")
        }) {
            result.push((offset, pointer));
        }
    }
    Ok(result)
}

fn reflection_functions(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    descriptor: usize,
) -> Result<(usize, usize)> {
    let kind = read_rtti_type(memory, descriptor, studio.base, studio.size)
        .context("Property descriptor has no validated RTTI")?;
    if !kind.starts_with(".?AV?$PropDescriptor@") && !kind.starts_with(".?AV?$EnumPropDescriptor@")
    {
        bail!("Unsupported protected-property descriptor: {kind}");
    }
    let vtable = memory.read_u64(descriptor)? as usize;
    let key = (
        studio.path.clone(),
        layout.image_stamp,
        vtable
            .checked_sub(studio.base)
            .context("Property vtable is outside Studio")?,
    );
    if let Some(entry) = cached_abi(&TEXT_ABIS, &key, memory, studio, descriptor) {
        return Ok((
            studio.base + entry.functions[0].1,
            if entry.setter_supported {
                studio.base + entry.functions[1].1
            } else {
                0
            },
        ));
    }
    let bindings = bindings(memory, studio, descriptor)?;
    let offsets = bindings
        .iter()
        .map(|(offset, _)| *offset)
        .collect::<Vec<_>>();
    // Find the string-capability/get/set sequence by behavior rather than an
    // absolute address or a blindly assumed vtable index. Unknown layouts stop.
    let mut candidates = Vec::new();
    for slot in 16..30 {
        let capability = memory.read_u64(vtable + slot * 8)? as usize;
        let Ok(code) = verified_code(memory, studio, layout, capability, 8) else {
            continue;
        };
        if !true_return(&code) {
            continue;
        }
        let getter = memory.read_u64(vtable + (slot + 1) * 8)? as usize;
        let code = verified_code(memory, studio, layout, getter, 256)?;
        let Some((binding_offset, getter_slot)) = getter_call(&code, &offsets, GetterAbi::Text)
        else {
            continue;
        };
        let binding = bindings
            .iter()
            .find(|(offset, _)| *offset == binding_offset)
            .unwrap()
            .1;
        let binding_vtable = memory.read_u64(binding)? as usize;
        let bound_getter = memory.read_u64(binding_vtable + getter_slot)? as usize;
        verified_code(memory, studio, layout, bound_getter, 64)?;
        let setter = memory.read_u64(vtable + (slot + 2) * 8)? as usize;
        let setter_code = verified_code(memory, studio, layout, setter, 512)?;
        let setter_supported = setter_call(&setter_code, &offsets)
            .is_some_and(|(field, slot)| field == binding_offset && slot.is_multiple_of(8));
        candidates.push(AbiCacheEntry {
            descriptor_binding_offset: None,
            setter_supported,
            functions: vec![
                ((slot + 1) * 8, getter - studio.base, code),
                ((slot + 2) * 8, setter - studio.base, setter_code),
            ],
        });
    }
    if candidates.len() != 1 {
        bail!(
            "Property has no uniquely validated text codec on this Studio build ({} matches)",
            candidates.len()
        );
    }
    let entry = candidates.remove(0);
    let functions = (
        studio.base + entry.functions[0].1,
        if entry.setter_supported {
            studio.base + entry.functions[1].1
        } else {
            0
        },
    );
    TEXT_ABIS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, entry);
    Ok(functions)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DebugIdOperand {
    Descriptor,
    Instance,
    Lua,
    Stack(i64),
    Field(usize, usize), // descriptor byte offset and loaded width
    Adjustment(usize),   // sign-extended int32, not a pointer-sized adjustment
    AdjustedInstance(usize),
    CallResult,
    Integer32,
}

#[derive(Clone)]
struct DebugIdPath {
    pc: u64,
    registers: HashMap<Register, DebugIdOperand>,
    member: Option<(usize, i64)>, // member field and result string's stack address
    consumed: bool,
}

impl DebugIdPath {
    fn register(&self, register: Register) -> Option<DebugIdOperand> {
        self.registers.get(&register.full_register()).copied()
    }

    fn value(
        &self,
        instruction: &Instruction,
        operand: u32,
        width: usize,
    ) -> Option<DebugIdOperand> {
        use DebugIdOperand::*;
        let value = match instruction.op_kind(operand) {
            OpKind::Register => self.register(instruction.op_register(operand))?,
            OpKind::Memory if instruction.memory_index() == Register::None => {
                let Descriptor = self.register(instruction.memory_base())? else {
                    return None;
                };
                let offset = usize::try_from(instruction.memory_displacement64()).ok()?;
                if offset.checked_add(width)? > 0x200 {
                    return None;
                }
                Field(offset, width)
            }
            _ => return None,
        };
        match (value, width) {
            (Field(offset, available), _) if width <= available => Some(Field(offset, width)),
            (CallResult | Integer32, 4) => Some(Integer32),
            (Descriptor | Instance | Lua | Stack(_) | Adjustment(_) | AdjustedInstance(_), 8) => {
                Some(value)
            }
            _ => None,
        }
    }

    fn clobber_call(&mut self) {
        for register in [
            Register::RAX,
            Register::RCX,
            Register::RDX,
            Register::R8,
            Register::R9,
            Register::R10,
            Register::R11,
            Register::XMM0,
            Register::XMM1,
            Register::XMM2,
            Register::XMM3,
            Register::XMM4,
            Register::XMM5,
        ] {
            self.registers.remove(&register.full_register());
        }
    }

    fn instruction(&mut self, i: &Instruction, info: &mut InstructionInfoFactory) -> Option<()> {
        use DebugIdOperand::*;
        let destination = i.op0_register();
        let result = match i.mnemonic() {
            Mnemonic::Mov
            | Mnemonic::Movups
            | Mnemonic::Movaps
            | Mnemonic::Movdqu
            | Mnemonic::Movdqa
            | Mnemonic::Movd
            | Mnemonic::Movq
                if i.op0_kind() == OpKind::Register =>
            {
                let width = match i.mnemonic() {
                    Mnemonic::Movd => 4,
                    Mnemonic::Movq => 8,
                    _ => destination.size(),
                };
                self.value(i, 1, width)
            }
            Mnemonic::Movsxd if destination.size() == 8 => match self.value(i, 1, 4) {
                Some(Field(offset, 4)) => Some(Adjustment(offset)),
                _ => None,
            },
            Mnemonic::Psrldq if i.immediate8() == 8 => match self.register(destination) {
                Some(Field(offset, 16)) => Some(Field(offset + 8, 8)),
                _ => None,
            },
            Mnemonic::Lea if destination.size() == 8 && i.memory_index() == Register::None => {
                match self.register(i.memory_base()) {
                    Some(Stack(offset)) => offset
                        .checked_add(i.memory_displacement64() as i64)
                        .map(Stack),
                    _ => None,
                }
            }
            Mnemonic::Add | Mnemonic::Sub if destination.size() == 8 => {
                match (self.register(destination), i.op1_kind()) {
                    (Some(Stack(offset)), OpKind::Immediate8to64 | OpKind::Immediate32to64) => {
                        let delta = i.immediate(1) as i64;
                        if i.mnemonic() == Mnemonic::Add {
                            offset.checked_add(delta).map(Stack)
                        } else {
                            offset.checked_sub(delta).map(Stack)
                        }
                    }
                    (Some(Adjustment(offset)), OpKind::Register)
                        if i.mnemonic() == Mnemonic::Add
                            && self.register(i.op1_register()) == Some(Instance) =>
                    {
                        Some(AdjustedInstance(offset))
                    }
                    (Some(Instance), OpKind::Register) if i.mnemonic() == Mnemonic::Add => {
                        match self.register(i.op1_register()) {
                            Some(Adjustment(offset)) => Some(AdjustedInstance(offset)),
                            _ => None,
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let stack = match (i.mnemonic(), self.register(Register::RSP)) {
            (Mnemonic::Push, Some(Stack(offset))) => Some(Stack(offset.checked_sub(8)?)),
            (Mnemonic::Pop, Some(Stack(offset))) => Some(Stack(offset.checked_add(8)?)),
            _ => None,
        };
        // Unknown writes must kill provenance, including partial registers and
        // implicit destinations. A matching instruction suffix is not a proof.
        for used in info.info(i).used_registers() {
            if matches!(
                used.access(),
                OpAccess::Write
                    | OpAccess::CondWrite
                    | OpAccess::ReadWrite
                    | OpAccess::ReadCondWrite
            ) {
                self.registers.remove(&used.register().full_register());
            }
        }
        if let Some(value) = result {
            self.registers.insert(destination.full_register(), value);
        }
        if let Some(value) = stack {
            self.registers.insert(Register::RSP, value);
        }
        Some(())
    }
}

// BoundFuncDesc<Instance, string(int)> receives descriptor/Instance/Lua/index.
// Follow all forward paths through its argument conversion and member-pointer
// unpacking. The string must subsequently reach Lua with the same stack address.
// No descriptor offset, scratch register, or native RVA is a signature.
fn debug_id_member_field(code: &[u8]) -> Option<usize> {
    use DebugIdOperand::*;
    if code.len() > 1024 {
        return None;
    }
    let instructions = Decoder::new(64, code, DecoderOptions::NONE)
        .into_iter()
        .map(|i| (i.ip(), i))
        .collect::<HashMap<_, _>>();
    let mut pending = vec![DebugIdPath {
        pc: 0,
        registers: HashMap::from([
            (Register::RCX, Descriptor),
            (Register::RDX, Instance),
            (Register::R8, Lua),
            (Register::RSP, Stack(0)),
        ]),
        member: None,
        consumed: false,
    }];
    let mut info = InstructionInfoFactory::new();
    let mut fields = HashSet::new();
    let mut steps = 0;
    while let Some(mut state) = pending.pop() {
        loop {
            steps += 1;
            if steps > 8192 || pending.len() > 64 {
                return None;
            }
            let i = instructions.get(&state.pc)?;
            if i.is_invalid() {
                return None;
            }
            match i.flow_control() {
                FlowControl::Return => {
                    let (field, _) = state.member?;
                    if !state.consumed {
                        return None;
                    }
                    fields.insert(field);
                    break;
                }
                FlowControl::Exception => {
                    if !state.consumed {
                        return None;
                    }
                    break;
                }
                // MSVC's invalid-allocation cleanup ends in INT3. iced-x86
                // classifies it as Interrupt, not Exception. Only terminate
                // this abort arm after the native string reached Lua; normal
                // return paths must still establish one unambiguous binding.
                FlowControl::Interrupt if i.mnemonic() == Mnemonic::Int3 && state.consumed => break,
                FlowControl::ConditionalBranch | FlowControl::UnconditionalBranch => {
                    let target = i.near_branch_target();
                    if target <= state.pc || !instructions.contains_key(&target) {
                        return None;
                    }
                    if i.flow_control() == FlowControl::UnconditionalBranch {
                        state.pc = target;
                        continue;
                    }
                    state.instruction(i, &mut info)?;
                    let mut branch = state.clone();
                    branch.pc = target;
                    pending.push(branch);
                }
                FlowControl::Call | FlowControl::IndirectCall => {
                    if i.op0_kind() == OpKind::Register {
                        let Field(field, 8) = state.register(i.op0_register())? else {
                            return None;
                        };
                        let Some(Stack(output)) = state.register(Register::RDX) else {
                            return None;
                        };
                        if !(0x40..=0x1f0).contains(&field)
                            || !field.is_multiple_of(8)
                            || state.register(Register::RCX) != Some(AdjustedInstance(field + 8))
                            || state.register(Register::R8) != Some(Integer32)
                            || state.member.is_some()
                        {
                            return None;
                        }
                        state.member = Some((field, output));
                    } else if i.op0_kind() == OpKind::NearBranch64 {
                        if let Some((_, output)) = state.member
                            && state.register(Register::RCX) == Some(Lua)
                            && state.register(Register::RDX) == Some(Stack(output))
                        {
                            state.consumed = true;
                        }
                    } else if !state.consumed {
                        return None;
                    }
                    state.clobber_call();
                    if i.op0_kind() == OpKind::NearBranch64 {
                        state.registers.insert(Register::RAX, CallResult);
                    }
                }
                FlowControl::Next => state.instruction(i, &mut info)?,
                _ => return None,
            }
            state.pc = i.next_ip();
        }
    }
    (fields.len() == 1).then(|| *fields.iter().next().unwrap())
}

fn debug_id_invoker(codes: &[(usize, Vec<u8>)]) -> Result<(usize, usize)> {
    let candidates = codes
        .iter()
        .filter_map(|(slot, code)| debug_id_member_field(code).map(|field| (*slot, field)))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        candidates.len() == 1,
        "Studio GetDebugId invoker resolved {} candidates",
        candidates.len()
    );
    Ok(candidates[0])
}

// The observed direct implementation calls its formatter and returns. Do not
// accept a virtual-dispatch thunk, an indirect call, or an unbounded jump chain.
fn debug_id_direct_target(code: &[u8], address: usize) -> Option<usize> {
    let mut target = None;
    for i in Decoder::with_ip(64, code, address as u64, DecoderOptions::NONE) {
        if i.is_invalid() {
            return None;
        }
        match i.flow_control() {
            FlowControl::Return => return target,
            FlowControl::Call if i.op0_kind() == OpKind::NearBranch64 && target.is_none() => {
                target = Some(usize::try_from(i.near_branch_target()).ok()?);
            }
            FlowControl::Next => {}
            _ => return None,
        }
    }
    None
}

fn debug_id_direct_member(pair: &[u8]) -> Result<usize> {
    // MSVC stores an int32 this-adjustment after the function pointer. The
    // remaining four bytes in a SIMD-loaded pair are padding, not adjustment.
    anyhow::ensure!(
        read_u32(pair, 8)? as i32 == 0,
        "Studio GetDebugId requires an unsupported Instance adjustment"
    );
    Ok(read_u64(pair, 0)? as usize)
}

/// Read-only discovery. Call once per native capture, not once per Instance.
/// Returns the direct MSVC member entry: RCX=Instance, RDX=uninitialized engine
/// string storage, R8D=int32 scope; RAX returns that storage. The caller owns the
/// Instance lifetime and must destroy the engine string with its matching ABI.
pub(super) fn debug_id_function(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    model: &ActiveDataModel,
    instance: usize,
) -> Result<usize> {
    verify_loaded_image(memory, studio, layout.image_stamp)?;
    anyhow::ensure!(
        layout.text.characteristics & 0x2000_0000 != 0,
        "Studio GetDebugId discovery requires an executable text section"
    );
    let descriptor = find_class_member_descriptor(memory, instance, model.layout, "GetDebugId")?;
    anyhow::ensure!(
        read_rtti_type(memory, descriptor, studio.base, studio.size).as_deref()
            == Some(concat!(
                ".?AV?$BoundFuncDesc@VInstance@RBX@@$$A6A?AV?$basic_string@DU?$char_traits@D@std@@",
                "V?$allocator@D@2@@std@@H@Z$0A@$00@Reflection@RBX@@"
            )),
        "Studio GetDebugId has an unsupported method signature"
    );
    let vtable = memory.read_u64(descriptor)? as usize;
    let locator = memory.read_u64(vtable - 8)? as usize;
    anyhow::ensure!(
        memory.read_u32(locator + 4)? == 0
            && studio
                .base
                .checked_add(memory.read_u32(locator + 20)? as usize)
                == Some(locator),
        "Studio GetDebugId descriptor is not a complete reflection object"
    );
    let mut codes = Vec::new();
    let mut functions = Vec::new();
    for slot in (0..64).step_by(8) {
        let function = memory.read_u64(vtable + slot)? as usize;
        if function
            .checked_sub(studio.base + layout.text.virtual_address)
            .is_none_or(|offset| offset >= layout.text.raw_size)
        {
            continue;
        }
        codes.push((slot, verified_code(memory, studio, layout, function, 1024)?));
        functions.push(function);
    }
    let (slot, field) = debug_id_invoker(&codes)?;
    let invoker_index = codes
        .iter()
        .position(|(candidate, _)| *candidate == slot)
        .unwrap();
    let invoker = functions[invoker_index];
    let pair_address = descriptor
        .checked_add(field)
        .context("GetDebugId field overflow")?;
    let pair = memory.read_vec(pair_address, 12)?;
    let function = debug_id_direct_member(&pair)?;
    let code = verified_code(memory, studio, layout, function, 64)?;
    let formatter = debug_id_direct_target(&code, function)
        .context("Studio GetDebugId is not the supported direct native entry")?;
    verified_code(memory, studio, layout, formatter, 64)?;
    anyhow::ensure!(
        memory.read_u64(descriptor)? as usize == vtable
            && memory.read_vec(pair_address, 12)? == pair
            && memory.read_u64(vtable + slot)? as usize == invoker
            && memory.read_vec(invoker, codes[invoker_index].1.len())? == codes[invoker_index].1,
        "Studio GetDebugId binding changed during discovery"
    );
    Ok(function)
}

pub(super) fn identity_binding(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    model: &ActiveDataModel,
    instance: usize,
) -> Result<(usize, usize)> {
    let descriptor = find_class_member_descriptor(memory, instance, model.layout, "UniqueId")?;
    if read_rtti_type(memory, descriptor, studio.base, studio.size).as_deref()
        != Some(".?AV?$PropDescriptor@VInstance@RBX@@VUniqueId@2@@Reflection@RBX@@")
    {
        bail!("Studio instance identity descriptor is unsupported");
    }
    let descriptor_vtable = memory.read_u64(descriptor)? as usize;
    let key = (
        studio.path.clone(),
        layout.image_stamp,
        descriptor_vtable
            .checked_sub(studio.base)
            .context("Identity vtable is outside Studio")?,
    );
    if let Some(entry) = cached_abi(&IDENTITY_ABIS, &key, memory, studio, descriptor) {
        let binding =
            memory.read_u64(descriptor + entry.descriptor_binding_offset.unwrap())? as usize;
        return Ok((binding, studio.base + entry.functions[0].1));
    }
    let bindings = bindings(memory, studio, descriptor)?;
    let offsets = bindings
        .iter()
        .map(|(offset, _)| *offset)
        .collect::<Vec<_>>();
    let mut used = HashSet::new();
    for slot in 12..24 {
        let function = memory.read_u64(descriptor_vtable + slot * 8)? as usize;
        let Ok(code) = verified_code(memory, studio, layout, function, 256) else {
            continue;
        };
        if let Some((offset, _)) = getter_call(&code, &offsets, GetterAbi::Variant) {
            used.insert(offset);
        }
    }
    if used.len() != 1 {
        bail!(
            "Studio identity binding resolved {} active candidates",
            used.len()
        );
    }
    let offset = *used.iter().next().unwrap();
    let binding = bindings
        .iter()
        .find(|(candidate, _)| *candidate == offset)
        .unwrap()
        .1;
    let kind = read_rtti_type(memory, binding, studio.base, studio.size).unwrap_or_default();
    if !kind.starts_with(".?AV?$GetSetImpl@P8InstanceProp@RBX@@EBA?AVUniqueId@") {
        bail!("Studio instance identity binding is unsupported");
    }
    let vtable = memory.read_u64(binding)? as usize;
    let mut candidates = Vec::new();
    for slot in 0..12 {
        let getter = memory.read_u64(vtable + slot * 8)? as usize;
        let Ok(code) = verified_code(memory, studio, layout, getter, 128) else {
            continue;
        };
        if let Some((_, member)) = getter_call(&code, &[], GetterAbi::Identity) {
            let function = memory.read_u64(binding + member)? as usize;
            verified_code(memory, studio, layout, function, 48)?;
            candidates.push((slot * 8, getter - studio.base, code));
        }
    }
    if candidates.len() != 1 {
        bail!(
            "Studio identity return-buffer ABI resolved {} candidates",
            candidates.len()
        );
    }
    let function = candidates.remove(0);
    let getter = studio.base + function.1;
    IDENTITY_ABIS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            key,
            AbiCacheEntry {
                descriptor_binding_offset: Some(offset),
                setter_supported: false,
                functions: vec![function],
            },
        );
    Ok((binding, getter))
}

pub(super) fn resolve_path(
    memory: &ProcessMemory,
    model: &ActiveDataModel,
    segments: &[String],
    ordinals: &[usize],
) -> Result<Vec<SharedEntry>> {
    if segments.is_empty()
        || segments.len() > 64
        || segments.iter().any(String::is_empty)
        || (!ordinals.is_empty() && ordinals.len() != segments.len())
        || ordinals.contains(&0)
    {
        bail!("Property target needs 1-64 nonempty path segments and matching positive ordinals");
    }
    let mut children = model.roots.clone();
    let mut ancestors = Vec::with_capacity(segments.len());
    for (index, name) in segments.iter().enumerate() {
        let matches = children
            .iter()
            .copied()
            .filter(|entry| {
                read_instance_name(memory, entry.instance, model.layout).as_deref() == Some(name)
            })
            .collect::<Vec<_>>();
        let entry = match ordinals.get(index) {
            Some(ordinal) => matches.get(ordinal - 1).copied(),
            None if matches.len() == 1 => Some(matches[0]),
            _ => None,
        }
        .with_context(|| {
            format!(
                "Property path segment '{name}' has {} matches; use --ords for duplicates",
                matches.len()
            )
        })?;
        ancestors.push(entry);
        if index + 1 < segments.len() {
            children = read_children(memory, entry.instance, model.layout)
                .context("Property hierarchy changed during lookup")?;
        }
    }
    Ok(ancestors)
}

pub(super) fn parent_offset(memory: &ProcessMemory, model: &ActiveDataModel) -> Result<usize> {
    let instance = model.outer + model.layout.data_model_instance;
    let samples = model
        .roots
        .iter()
        .take(3)
        .map(|entry| memory.read_vec(entry.instance, 0x208))
        .collect::<Result<Vec<_>>>()?;
    if samples.len() < 3 {
        bail!("Not enough roots to validate Studio parent layout");
    }
    let candidates = (0..=0x200)
        .step_by(8)
        .filter(|offset| {
            samples
                .iter()
                .all(|bytes| read_u64(bytes, *offset).ok() == Some(instance as u64))
        })
        .collect::<Vec<_>>();
    if candidates.len() != 1 {
        bail!("Studio parent layout is ambiguous");
    }
    Ok(candidates[0])
}

pub(crate) fn prepare_property(
    pid: u32,
    title: &str,
    segments: &[String],
    ordinals: &[usize],
    property: &str,
    timeout: Duration,
) -> Result<NativeProperty> {
    prepare(pid, title, segments, ordinals, property, timeout, None).map(|(prepared, _)| prepared)
}

/// A trusted snapshot read captures identity and value on the same engine task.
/// It does not create a write grant or change the ordinary approval flow.
pub(crate) fn read_property(
    pid: u32,
    title: &str,
    segments: &[String],
    ordinals: &[usize],
    class: &str,
    property: &str,
    timeout: Duration,
) -> Result<String> {
    prepare(
        pid,
        title,
        segments,
        ordinals,
        property,
        timeout,
        Some(class),
    )
    .map(|(_, value)| value)
}

fn prepare(
    pid: u32,
    title: &str,
    segments: &[String],
    ordinals: &[usize],
    property: &str,
    timeout: Duration,
    read_class: Option<&str>,
) -> Result<(NativeProperty, String)> {
    let _trace = crate::app::timing::trace_scope("native.property", "prepare reflected property");
    let deadline = Instant::now() + timeout;
    let phase =
        crate::app::timing::trace_scope("native.property", "open process and validate image");
    let current_modules = modules(pid)?;
    let studio = current_modules
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"))
        .context("Roblox Studio module was not found")?;
    let layout = package_layout(&studio.path)?;
    let memory = ProcessMemory::open(pid)?;
    verify_loaded_image(&memory, studio, layout.image_stamp)?;
    drop(phase);
    let phase = crate::app::timing::trace_scope("native.property", "locate DataModel context");
    let model = active_data_model(pid, &memory, studio, layout.data, title)?;
    let model_instance = model.outer + model.layout.data_model_instance;
    let name = read_instance_name(&memory, model_instance, model.layout)
        .context("Studio DataModel name is unavailable")?;
    if !expected_data_model_names(title)
        .iter()
        .any(|expected| expected == &name)
    {
        bail!("Native property target did not match the selected Studio place");
    }
    drop(phase);
    let phase =
        crate::app::timing::trace_scope("native.property", "resolve instance path and class");
    let ancestors = resolve_path(&memory, &model, segments, ordinals)?;
    let entry = *ancestors.last().expect("path validated above");
    let class_name = read_instance_class(&memory, entry.instance, model.layout)
        .context("Property target class is unavailable")?;
    anyhow::ensure!(
        read_class.is_none_or(|expected| class_name == expected),
        "Native snapshot target changed class"
    );
    drop(phase);
    let phase = crate::app::timing::trace_scope("native.property", "resolve property descriptor");
    let descriptor = find_class_member_descriptor(&memory, entry.instance, model.layout, property)?;
    drop(phase);
    let phase = crate::app::timing::trace_scope(
        "native.property",
        "resolve property codec and identity getter",
    );
    let (getter, setter) = reflection_functions(&memory, studio, &layout, descriptor)?;
    let (binding, identity_getter) =
        identity_binding(&memory, studio, &layout, &model, entry.instance)?;
    drop(phase);
    let phase = crate::app::timing::trace_scope("native.property", "prepare ABI parameters");
    let mut parameters = vec![0; SIZE];
    for (offset, value) in [
        (
            0,
            data_model_task_context(&memory, studio, &layout, &model)?,
        ),
        (8, studio.base + layout.submit_task),
        (16, entry.instance),
        (24, entry.owner),
        (32, model.owner),
        (40, descriptor),
        (48, getter),
        (
            56,
            memory.read_u64(entry.instance + model.layout.class_descriptor)? as usize,
        ),
        (64, memory.read_u64(descriptor)? as usize),
        (72, model.layout.class_descriptor),
        (65888, binding),
        (65896, identity_getter),
        (65904, setter),
        (131488, parent_offset(&memory, &model)?),
        (132024, model.layout.self_pointer),
    ] {
        put_u64(&mut parameters, offset, value);
    }
    put_u32(&mut parameters, 131496, ancestors.len() as u32 + 1);
    for (index, entry) in ancestors.iter().rev().enumerate() {
        put_u64(&mut parameters, 131504 + index * 8, entry.instance);
    }
    put_u64(
        &mut parameters,
        131504 + ancestors.len() * 8,
        model_instance,
    );
    drop(phase);
    let phase = crate::app::timing::trace_scope("native.property", "ensure native helper");
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .context("Protected property discovery exceeded its deadline")?;
    let helper = ensure_helper_loaded_with_timeout(
        pid,
        &memory,
        &current_modules,
        remaining.as_millis().clamp(1, 3000) as u32,
    )?;
    let mut prepared = NativeProperty {
        class_name,
        property: property.into(),
        instance_id: String::new(),
        memory,
        parameters,
        entrypoint: helper + helper_export_rva("ReniumReadProperty")?,
        deadline,
    };
    drop(phase);
    let phase = crate::app::timing::trace_scope(
        "native.property",
        "invoke identity-checked property operation",
    );
    prepared.invoke(if read_class.is_some() { 3 } else { 0 })?;
    drop(phase);
    let identity: [u8; 16] = prepared.parameters[IDENTITY..IDENTITY + 16].try_into()?;
    if identity == [0; 16] {
        bail!("Studio target has no stable instance identity");
    }
    prepared.instance_id = identity.iter().map(|byte| format!("{byte:02x}")).collect();
    prepared.parameters[EXPECTED_IDENTITY..EXPECTED_IDENTITY + 16].copy_from_slice(&identity);
    let value = if read_class.is_some() {
        prepared.value()?
    } else {
        String::new()
    };
    Ok((prepared, value))
}

#[cfg(test)]
pub(super) mod typed_audit {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/studio/native/serializer/fixtures/native-typed-finder.rs"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "Read-only history ABI inspection in the owned Windows Terrain fixture"]
    fn terrain_history_descriptor_live_fixture() -> Result<()> {
        let pid = std::env::var("RENIUM_INSPECT_FIXTURE_PID")?.parse()?;
        let title = "ReniumTerrainTest.rbxl";
        let prepared = prepare_property(
            pid,
            title,
            &["ChangeHistoryService".into()],
            &[],
            "Name",
            Duration::from_secs(10),
        )?;
        let current_modules = modules(pid)?;
        let studio = current_modules
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"))
            .context("Missing Studio")?;
        let layout = package_layout(&studio.path)?;
        let model = active_data_model(pid, &prepared.memory, studio, layout.data, title)?;
        let instance = read_u64(&prepared.parameters, 16)? as usize;
        let descriptor = find_class_member_descriptor(
            &prepared.memory,
            instance,
            model.layout,
            "FinishRecording",
        )?;
        let table = prepared.memory.read_u64(descriptor)? as usize;
        let fields = prepared.memory.read_vec(descriptor, 256)?;
        let mut functions = Vec::new();
        for (kind, address) in [("slot", table), ("field", descriptor)] {
            for offset in (0..256).step_by(8) {
                let function = prepared.memory.read_u64(address + offset)? as usize;
                if let Ok(code) = verified_code(&prepared.memory, studio, &layout, function, 1024) {
                    functions.push(serde_json::json!({"kind":kind,"offset":offset,"rva":function-studio.base,"code":base64::encode(code)}));
                }
            }
        }
        let result = serde_json::json!({"base":studio.base,"descriptor":descriptor,"table":table,"kind":read_rtti_type(&prepared.memory,descriptor,studio.base,studio.size),"fields":base64::encode(fields),"functions":functions});
        fs::write(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../audit/release-readiness/history-finish-windows.json"),
            serde_json::to_vec_pretty(&result)?,
        )?;
        Ok(())
    }

    #[test]
    #[ignore = "Read-only binary Terrain binding inspection in the owned Windows fixture"]
    fn terrain_binary_descriptor_live_fixture() -> Result<()> {
        let pid = std::env::var("RENIUM_INSPECT_FIXTURE_PID")?.parse()?;
        let prepared = prepare_property(
            pid,
            "ReniumTerrainTest.rbxl",
            &["Workspace".into(), "Terrain".into()],
            &[],
            "Name",
            Duration::from_secs(10),
        )?;
        let current_modules = modules(pid)?;
        let studio = current_modules
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"))
            .context("Missing Studio")?;
        let layout = package_layout(&studio.path)?;
        let model = active_data_model(
            pid,
            &prepared.memory,
            studio,
            layout.data,
            "ReniumTerrainTest.rbxl",
        )?;
        let instance = read_u64(&prepared.parameters, 16)? as usize;
        let mut rows = Vec::new();
        for name in ["SmoothGrid", "PhysicsGrid", "Clear"] {
            let descriptor =
                find_class_member_descriptor(&prepared.memory, instance, model.layout, name)?;
            let mut bindings = Vec::new();
            for offset in (64..256).step_by(8) {
                let binding = prepared.memory.read_u64(descriptor + offset)? as usize;
                let Some(kind) =
                    read_rtti_type(&prepared.memory, binding, studio.base, studio.size)
                else {
                    continue;
                };
                if !kind.contains("GetSetImpl") {
                    continue;
                }
                let table = prepared.memory.read_u64(binding)? as usize;
                let mut methods = Vec::new();
                for slot in (0..64).step_by(8) {
                    let target = prepared.memory.read_u64(table + slot)? as usize;
                    if let Ok(code) = verified_code(&prepared.memory, studio, &layout, target, 512)
                    {
                        methods.push(serde_json::json!({"slot":slot,"rva":target-studio.base,"code":base64::encode(code)}));
                    }
                }
                bindings.push(serde_json::json!({"offset":offset,"kind":kind,"fields":base64::encode(prepared.memory.read_vec(binding,64)?),"methods":methods}));
            }
            let table = prepared.memory.read_u64(descriptor)? as usize;
            let mut functions = Vec::new();
            for slot in (16..256).step_by(8) {
                let target = prepared.memory.read_u64(table + slot)? as usize;
                if let Ok(code) = verified_code(&prepared.memory, studio, &layout, target, 512) {
                    functions.push(serde_json::json!({"slot":slot,"rva":target-studio.base,"code":base64::encode(code)}));
                }
            }
            rows.push(serde_json::json!({"name":name,"base":studio.base,"kind":read_rtti_type(&prepared.memory,descriptor,studio.base,studio.size),"bindings":bindings,"functions":functions,"fields":base64::encode(prepared.memory.read_vec(descriptor,256)?)}));
        }
        fs::write(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../audit/release-readiness/terrain-binary-windows.json"),
            serde_json::to_vec_pretty(&rows)?,
        )?;
        Ok(())
    }

    #[test]
    #[ignore = "Opt-in history hook qualification in the owned Windows Terrain fixture"]
    fn terrain_history_hook_live_fixture() -> Result<()> {
        anyhow::ensure!(
            std::env::var("RENIUM_TERRAIN_WRITE_PROBE").as_deref() == Ok("1"),
            "Explicit write probe opt-in required"
        );
        let pid = std::env::var("RENIUM_INSPECT_FIXTURE_PID")?.parse()?;
        let token = std::env::var("RENIUM_HISTORY_PROBE_TOKEN")?;
        register_history(pid, "ReniumTerrainTest.rbxl", &token)
    }

    fn debug_id_fixture(field: i32, variant: usize) -> Vec<u8> {
        // Move the saved descriptor/Instance/Lua, SSE pair, signed adjustment,
        // and indirect-call register independently; none is a fixed signature.
        let (descriptor, instance, lua, pair, high, adjust, method) = match variant {
            0 => (5, 6, 14, 1, 0, 1, 0),
            1 => (3, 7, 15, 4, 3, 10, 11),
            _ => (12, 13, 14, 6, 7, 9, 10),
        };
        fn rr(code: &mut Vec<u8>, opcode: u8, dest: u8, src: u8, wide: bool) {
            code.extend_from_slice(&[
                0x40 | (u8::from(wide) * 8) | ((dest >> 3) * 4) | (src >> 3),
                opcode,
                0xc0 | ((dest & 7) * 8) | (src & 7),
            ]);
        }
        let mut code = Vec::new();
        rr(&mut code, 0x8b, descriptor, 1, true);
        rr(&mut code, 0x8b, instance, 2, true);
        rr(&mut code, 0x8b, lua, 8, true);
        code.extend_from_slice(&[0xe8, 0, 0, 0, 0]); // argument conversion, int32 in EAX
        rr(&mut code, 0x8b, 8, 0, false);
        code.extend_from_slice(&[
            0x40 | (descriptor >> 3),
            0x0f,
            0x10,
            0x80 | (pair * 8) | (descriptor & 7),
        ]);
        // R12 needs a SIB byte even with no index.
        if descriptor & 7 == 4 {
            code.push(0x24);
        }
        code.extend_from_slice(&field.to_le_bytes());
        code.extend_from_slice(&[
            0x66,
            0x0f,
            0x6f,
            0xc0 | (high * 8) | pair, // copy the pair
            0x66,
            0x0f,
            0x73,
            0xd8 | high,
            8, // high half contains the adjustment
            0x66,
            0x0f,
            0x7e,
            0xc2 | (high * 8), // movd EDX,XMMhigh
        ]);
        rr(&mut code, 0x63, adjust, 2, true); // signed int32 -> 64
        rr(&mut code, 0x03, adjust, instance, true);
        rr(&mut code, 0x8b, 1, adjust, true);
        code.extend_from_slice(&[
            0x48,
            0x8d,
            0x54,
            0x24,
            0x20, // RDX = string storage
            0x66,
            0x48 | (method >> 3),
            0x0f,
            0x7e,
            0xc0 | (pair * 8) | (method & 7),
            0x40 | (method >> 3),
            0xff,
            0xd0 | (method & 7),
            0x48,
            0x8d,
            0x54,
            0x24,
            0x20,
        ]);
        rr(&mut code, 0x8b, 1, lua, true);
        code.extend_from_slice(&[0xe8, 0, 0, 0, 0, 0xc3]); // consume string in Lua
        code
    }

    fn hex_bytes(text: &str) -> Vec<u8> {
        text.as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    #[test]
    fn debug_id_discovery_accepts_observed_invoker_with_all_conversion_and_cleanup_paths() {
        // Complete 0.737 invoker: SSO name branches, scope conversion, SSE member
        // unpacking, Lua string consumption, allocation cleanup and cookie check.
        let code = hex_bytes(concat!(
            "4053555657415641574883ec68488b056432480b4833c44889442450458bf94d8bf0488bf2488be9",
            "488b790848837f18107203488b3f488b4218488b580848837b18107203488b1b498bcee8302d8c05",
            "448bc8452bcf488b859000000048897c243048895c2438488d4c243048894c2428488944242041b8",
            "01000000418bd7498bcee8790526020f108d80000000660f6fc1660f73d808660f7ec24863ca4803ce",
            "448bc0488d54243066480f7ec8ffd090488d542430498bcee822e326028bd8488b5424484883fa107235",
            "48ffc2488b4c2430488bc14881fa00100000721c4883c227488b49f8482bc14883c0f84883f81f7607",
            "ff1536e2d206cce8a05c15068bc3488b4c24504833cce891514c064883c468415f415e5f5e5d5bc3"
        ));
        assert_eq!(debug_id_member_field(&code), Some(0x80));
    }

    #[test]
    fn debug_id_discovery_follows_relocated_member_fields_and_registers() {
        for field in [0x48, 0x80, 0xd8, 0x180, 0x1f0] {
            for variant in 0..3 {
                assert_eq!(
                    debug_id_member_field(&debug_id_fixture(field, variant)),
                    Some(field as usize)
                );
            }
        }
        for field in [0x20, 0x81, 0x1f8, 0x200, -8] {
            assert_eq!(debug_id_member_field(&debug_id_fixture(field, 0)), None);
        }
    }

    #[test]
    fn debug_id_discovery_rejects_wrong_abi_and_lost_provenance() {
        let original = debug_id_fixture(0xb0, 0);
        for (needle, replacement) in [
            (vec![0x44, 0x8b, 0xc0], vec![0x4c, 0x8b, 0xc0]), // R8, not R8D
            (vec![0x48, 0x63, 0xca], vec![0x48, 0x8b, 0xca]), // no signed adjustment
            (vec![0x48, 0x03, 0xce], vec![0x48, 0x03, 0xcd]), // descriptor, not Instance
            // sret in RCX instead of RDX
            (
                vec![0x48, 0x8d, 0x54, 0x24, 0x20],
                vec![0x48, 0x8d, 0x4c, 0x24, 0x20],
            ),
            (
                vec![0x66, 0x0f, 0x73, 0xd8, 8],
                vec![0x66, 0x0f, 0x73, 0xd8, 4],
            ),
        ] {
            let offset = original
                .windows(needle.len())
                .position(|bytes| bytes == needle)
                .unwrap();
            let mut code = original.clone();
            code[offset..offset + needle.len()].copy_from_slice(&replacement);
            assert_eq!(debug_id_member_field(&code), None);
        }
        let call = original
            .windows(3)
            .position(|bytes| bytes == [0x40, 0xff, 0xd0])
            .unwrap();
        for clobber in [
            vec![0x31, 0xc9],
            vec![0x41, 0xb0, 0],
            vec![0x66, 0x0f, 0xef, 0xc9],
        ] {
            let mut code = original.clone();
            // Clear the pair before it is read into RAX; clear RCX/R8 immediately before the call.
            let offset = if clobber.len() == 4 { call - 5 } else { call };
            code.splice(offset..offset, clobber);
            assert_eq!(debug_id_member_field(&code), None);
        }
        let mut wrong_result = original.clone();
        let result = wrong_result
            .windows(5)
            .rposition(|bytes| bytes == [0x48, 0x8d, 0x54, 0x24, 0x20])
            .unwrap();
        wrong_result[result + 4] = 0x28;
        assert_eq!(debug_id_member_field(&wrong_result), None);
    }

    #[test]
    fn debug_id_discovery_rejects_ambiguity_and_unbounded_or_invalid_paths() {
        let code = debug_id_fixture(0xa0, 1);
        assert_eq!(
            debug_id_invoker(&[(0x28, code.clone())]).unwrap(),
            (0x28, 0xa0)
        );
        assert!(debug_id_invoker(&[]).is_err());
        assert!(debug_id_invoker(&[(0x10, code.clone()), (0x18, code.clone())]).is_err());
        for field in [0xa0, 0xb0] {
            let mut branched = vec![0x0f, 0x84];
            branched.extend_from_slice(&(code.len() as i32).to_le_bytes());
            branched.extend_from_slice(&code);
            branched.extend_from_slice(&debug_id_fixture(field, 2));
            assert_eq!(
                debug_id_member_field(&branched),
                (field == 0xa0).then_some(0xa0)
            );
        }
        for prefix in [&[0xeb, 0xfe][..], &[0x75, 1], &[0xc3]] {
            let mut invalid = prefix.to_vec();
            invalid.extend_from_slice(&code);
            assert_eq!(debug_id_member_field(&invalid), None);
        }
        let mut twice = code.clone();
        twice.pop();
        twice.extend_from_slice(&debug_id_fixture(0xb0, 0));
        assert_eq!(debug_id_member_field(&twice), None);
        assert_eq!(debug_id_member_field(&[0x90; 1025]), None);
        assert_eq!(debug_id_member_field(&code[..code.len() - 1]), None);
    }

    #[test]
    fn debug_id_discovery_accepts_only_post_result_int3_abort_paths() {
        let mut code = debug_id_fixture(0xb0, 1);
        code.pop();
        // One normal return plus the observed allocator-failure abort arm.
        code.extend_from_slice(&[0x74, 1, 0xc3, 0xcc]);
        assert_eq!(debug_id_member_field(&code), Some(0xb0));

        code.pop();
        code.extend_from_slice(&[0xcd, 0x80]); // Other interrupts remain unsupported.
        assert_eq!(debug_id_member_field(&code), None);

        let mut early_abort = vec![0x75, 1, 0xcc];
        early_abort.extend_from_slice(&debug_id_fixture(0xb0, 1));
        assert_eq!(debug_id_member_field(&early_abort), None);

        let mut no_return = debug_id_fixture(0xb0, 1);
        *no_return.last_mut().unwrap() = 0xcc;
        assert_eq!(debug_id_member_field(&no_return), None);
    }

    #[test]
    fn debug_id_direct_entry_rejects_virtual_thunks_and_jump_chains() {
        let code = hex_bytes("40534883ec204883c130488bdae8ce9f8705488bc34883c4205bc3");
        assert_eq!(debug_id_direct_target(&code, 0x1538680), Some(0x6db2660));
        for code in [
            &[0x48, 0x8b, 0x01, 0xff, 0x20][..],
            &[0xe9, 0, 0, 0, 0],
            &[0xff, 0xd0, 0xc3],
            &[0xc3],
            &[0xe8, 0, 0, 0, 0, 0xe8, 0, 0, 0, 0, 0xc3],
        ] {
            assert_eq!(debug_id_direct_target(code, 0x10000), None);
        }
    }

    #[test]
    fn debug_id_member_adjustment_is_signed32_and_ignores_upper_padding() {
        let function = 0x140123450_u64;
        let mut pair = [0; 16];
        pair[..8].copy_from_slice(&function.to_le_bytes());
        for padding in [0_u32, 1, 0xdeadbeef, u32::MAX] {
            pair[12..].copy_from_slice(&padding.to_le_bytes());
            assert_eq!(debug_id_direct_member(&pair).unwrap(), function as usize);
            assert_eq!(
                debug_id_direct_member(&pair[..12]).unwrap(),
                function as usize
            );
            for adjustment in [1_i32, -1, i32::MIN, i32::MAX] {
                pair[8..12].copy_from_slice(&adjustment.to_le_bytes());
                assert!(debug_id_direct_member(&pair).is_err());
            }
            pair[8..12].fill(0);
        }
        assert!(debug_id_direct_member(&pair[..11]).is_err());
    }

    #[test]
    fn setter_discovery_follows_conversion_branches_and_relocated_binding_slots() {
        for field in [0x70_i32, 0x98, 0x148] {
            for slot in [0x18, 0x20, 0x38] {
                let mut code = vec![
                    0x48, 0x8b, 0xfa, // instance -> RDI
                    0x48, 0x8b, 0xd9, // descriptor -> RBX
                    0x48, 0x8d, 0x54, 0x24, 0x20, // parser output
                    0x49, 0x8b, 0xc8, // input -> RCX
                    0xe8, 0, 0, 0, 0, // conversion call
                    0x84, 0xc0, 0x75, 1, 0xc3, // early conversion-failure return
                    0x48, 0x8b, 0x8b,
                ];
                code.extend_from_slice(&field.to_le_bytes());
                code.extend_from_slice(&[
                    0x48, 0x8b, 0xd7, 0x4c, 0x8d, 0x44, 0x24, 0x20, 0x48, 0x8b, 0x01, 0xff, 0x50,
                    slot, 0xb0, 1, 0xc3,
                ]);
                assert_eq!(
                    setter_call(&code, &[field as usize]),
                    Some((field as usize, slot as usize))
                );
                assert_eq!(setter_call(&code, &[]), None);
                let mut invalid = code.clone();
                invalid[13] = 0xc9; // R9 is not the string input.
                assert_eq!(setter_call(&invalid, &[field as usize]), None);
                invalid = code;
                invalid[22] = 2; // Not a decoded branch boundary.
                assert_eq!(setter_call(&invalid, &[field as usize]), None);
            }
        }
    }
    #[test]
    fn capability_check_accepts_only_an_unconditional_true_return() {
        assert!(true_return(&[0xb0, 1, 0xc3]));
        assert!(true_return(&[0xb8, 1, 0, 0, 0, 0xc3]));
        assert!(!true_return(&[0xb0, 0, 0xc3]));
        assert!(!true_return(&[0xb0, 1, 0x90]));
    }

    #[test]
    fn getter_discovery_follows_relocated_fields_slots_and_registers() {
        for (save, restore) in [(0xda, 0xc3), (0xfa, 0xc7), (0xf2, 0xc6)] {
            for offset in [0x80_i32, 0x90, 0x98, 0x150] {
                for slot in [0x18, 0x20, 0x28] {
                    let mut code = vec![0x48, 0x8b, save, 0x48, 0x8b, 0x89];
                    code.extend_from_slice(&offset.to_le_bytes());
                    code.extend_from_slice(&[
                        0x49, 0x8b, 0xd0, 0x48, 0x8b, 0x01, 0xff, 0x50, slot, 0x48, 0x8b, restore,
                        0xc3,
                    ]);
                    assert_eq!(
                        getter_call(&code, &[offset as usize], GetterAbi::Text),
                        Some((offset as usize, slot as usize))
                    );
                    assert_eq!(
                        getter_call(&code, &[offset as usize + 8], GetterAbi::Text),
                        None
                    );
                    // Wrong instance argument must not validate merely because the byte pattern matches.
                    code[12] = 0xd1;
                    assert_eq!(
                        getter_call(&code, &[offset as usize], GetterAbi::Text),
                        None
                    );
                }
            }
        }
    }
}
