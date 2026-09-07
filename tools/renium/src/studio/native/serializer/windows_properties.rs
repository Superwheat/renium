//! Protected reflection calls. Discovery and authorization stay in Rust; the
//! helper only owns C++ objects and calls engine methods on the DataModel queue.
use super::*;
use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction, Mnemonic, OpKind, Register};
use std::io::{Read, Seek, SeekFrom};

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

const SIZE: usize = 132032;
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
        let mut remote = self.memory.allocate(SIZE)?;
        self.memory.write(remote.address, &self.parameters)?;
        let exit = remote.run(self.entrypoint, timeout + 500)?;
        self.memory.read(remote.address, &mut self.parameters)?;
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
fn verified_code(
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

fn identity_binding(
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

fn resolve_path(
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

fn parent_offset(memory: &ProcessMemory, model: &ActiveDataModel) -> Result<usize> {
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
    let deadline = Instant::now() + timeout;
    let current_modules = modules(pid)?;
    let studio = current_modules
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"))
        .context("Roblox Studio module was not found")?;
    let layout = package_layout(&studio.path)?;
    let memory = ProcessMemory::open(pid)?;
    verify_loaded_image(&memory, studio, layout.image_stamp)?;
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
    let ancestors = resolve_path(&memory, &model, segments, ordinals)?;
    let entry = *ancestors.last().expect("path validated above");
    let class_name = read_instance_class(&memory, entry.instance, model.layout)
        .context("Property target class is unavailable")?;
    let descriptor = find_class_member_descriptor(&memory, entry.instance, model.layout, property)?;
    let (getter, setter) = reflection_functions(&memory, studio, &layout, descriptor)?;
    let (binding, identity_getter) =
        identity_binding(&memory, studio, &layout, &model, entry.instance)?;
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
    prepared.invoke(0)?;
    let identity: [u8; 16] = prepared.parameters[IDENTITY..IDENTITY + 16].try_into()?;
    if identity == [0; 16] {
        bail!("Studio target has no stable instance identity");
    }
    prepared.instance_id = identity.iter().map(|byte| format!("{byte:02x}")).collect();
    prepared.parameters[EXPECTED_IDENTITY..EXPECTED_IDENTITY + 16].copy_from_slice(&identity);
    Ok(prepared)
}

#[cfg(test)]
mod tests {
    use super::*;
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
