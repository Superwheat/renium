// Audit only: included in the Windows test binary, never the shipped CLI.
use super::*;

pub(crate) const FIELD_NAMES: [&str; 10] = [
    "Anchored",
    "CanCollide",
    "CanQuery",
    "CanTouch",
    "CastShadow",
    "Locked",
    "Massless",
    "EnableFluidForces",
    "Transparency",
    "Reflectance",
];
pub(crate) const SPEC_SIZE: usize = 200;
const THUNK: &[u8; 24] = &[
    0x45, 0x33, 0xc0, 0x48, 0x8b, 0xc1, 0x48, 0x85, 0xd2, 0x48, 0x8d, 0x8a, 0xc0, 0, 0, 0, 0x49,
    0x0f, 0x44, 0xc8, 0x48, 0xff, 0x60, 8,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Kind {
    Boolean,
    Float32,
}

// Only the straight-line prefix is interpreted. The wrapper's allocating
// Variant tail is NOT called and its object layout is NOT assumed here.
fn typed_call(code: &[u8], fields: &[usize], kind: Kind) -> Option<(usize, usize)> {
    let mut registers = HashMap::from([
        (Register::RCX, Operand::Descriptor),
        (Register::RDX, Operand::Instance),
        (Register::R8, Operand::Output),
    ]);
    let mut decoder = Decoder::new(64, code, DecoderOptions::NONE);
    let mut info = InstructionInfoFactory::new();
    while decoder.can_decode() && decoder.position() < 64 {
        let i = decoder.decode();
        if i.is_invalid() {
            return None;
        }
        if i.mnemonic() == Mnemonic::Call {
            if i.op0_kind() != OpKind::Memory || i.memory_index() != Register::None {
                return None;
            }
            let Operand::Vtable(field) = *registers.get(&i.memory_base().full_register())? else {
                return None;
            };
            if registers.get(&Register::RCX) != Some(&Operand::Binding(field))
                || registers.get(&Register::RDX) != Some(&Operand::Instance)
                || registers.get(&Register::R8) != Some(&Operand::Output)
            {
                return None;
            }
            let slot = usize::try_from(i.memory_displacement64()).ok()?;
            if slot % 8 != 0 || slot >= 12 * 8 || !decoder.can_decode() {
                return None;
            }
            let result = decoder.decode();
            let valid = result.op0_kind() == OpKind::Register
                && result.op1_kind() == OpKind::Register
                && match kind {
                    Kind::Boolean => {
                        result.mnemonic() == Mnemonic::Movzx
                            && result.op1_register() == Register::AL
                            && result.op0_register().size() == 4
                    }
                    Kind::Float32 => {
                        result.mnemonic() == Mnemonic::Movaps
                            && result.op1_register() == Register::XMM0
                            && result.op0_register().size() == 16
                    }
                };
            return valid.then_some((field, slot));
        }
        if i.flow_control() != FlowControl::Next {
            return None;
        }
        // The observed prefix saves registers to its stack, never through an
        // Instance or descriptor. Do not silently accept a setter-like prefix.
        if i.op0_kind() == OpKind::Memory
            && (i.memory_base() != Register::RSP || i.memory_index() != Register::None)
        {
            return None;
        }
        let value = if i.mnemonic() == Mnemonic::Mov && i.op0_kind() == OpKind::Register {
            setter_operand(&i, &registers, fields)
        } else {
            None
        };
        for used in info.info(&i).used_registers() {
            if matches!(
                used.access(),
                OpAccess::Write
                    | OpAccess::CondWrite
                    | OpAccess::ReadWrite
                    | OpAccess::ReadCondWrite
            ) {
                registers.remove(&used.register().full_register());
            }
        }
        if let Some(value) = value {
            registers.insert(i.op0_register().full_register(), value);
        }
    }
    None
}

fn unique_call(candidates: &[(usize, usize, usize)]) -> Result<(usize, usize, usize)> {
    let unique = candidates.iter().copied().collect::<HashSet<_>>();
    anyhow::ensure!(
        unique.len() == 1,
        "Typed audit found {} invoker candidates",
        unique.len()
    );
    Ok(*unique.iter().next().unwrap())
}

pub(crate) fn discover(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    model: &ActiveDataModel,
    instance: usize,
    property: &str,
    kind: Kind,
) -> Result<Vec<u8>> {
    let descriptor = find_class_member_descriptor(memory, instance, model.layout, property)?;
    let (descriptor_kind, binding_kind) = match kind {
        Kind::Boolean => (
            ".?AV?$PropDescriptor@VBasePart@RBX@@_N@Reflection@RBX@@",
            ".?AV?$GetSetImpl@P8BasePartProp@RBX@@EBA_NXZP812@EAAX_N@Z@?$PropDescriptor@VBasePart@RBX@@_N@Reflection@RBX@@",
        ),
        Kind::Float32 => (
            ".?AV?$PropDescriptor@VBasePart@RBX@@M@Reflection@RBX@@",
            ".?AV?$GetSetImpl@P8BasePartProp@RBX@@EBAMXZP812@EAAXM@Z@?$PropDescriptor@VBasePart@RBX@@M@Reflection@RBX@@",
        ),
    };
    anyhow::ensure!(
        read_rtti_type(memory, descriptor, studio.base, studio.size).as_deref()
            == Some(descriptor_kind),
        "Unsupported typed descriptor for {property}"
    );
    let found = bindings(memory, studio, descriptor)?;
    let fields = found.iter().map(|(offset, _)| *offset).collect::<Vec<_>>();
    let descriptor_vtable = memory.read_u64(descriptor)? as usize;
    let mut candidates = Vec::new();
    for slot in 12..24 {
        let wrapper = memory.read_u64(descriptor_vtable + slot * 8)? as usize;
        let Ok(code) = verified_code(memory, studio, layout, wrapper, 128) else {
            continue;
        };
        if let Some((field, getter_slot)) = typed_call(&code, &fields, kind) {
            candidates.push((field, getter_slot, wrapper));
        }
    }
    let (field, getter_slot, wrapper) = unique_call(&candidates)?;
    let wrapper_slot = (12..24)
        .find(|slot| {
            memory
                .read_u64(descriptor_vtable + slot * 8)
                .is_ok_and(|value| value as usize == wrapper)
        })
        .context("Invoker disappeared")?
        * 8;
    let binding = found.iter().find(|(offset, _)| *offset == field).unwrap().1;
    anyhow::ensure!(
        read_rtti_type(memory, binding, studio.base, studio.size).as_deref() == Some(binding_kind),
        "Unsupported typed binding for {property}"
    );
    let binding_vtable = memory.read_u64(binding)? as usize;
    let getter = memory.read_u64(binding_vtable + getter_slot)? as usize;
    let getter_code = verified_code(memory, studio, layout, getter, THUNK.len())?;
    anyhow::ensure!(
        getter_code == THUNK,
        "Typed getter is not the audited two-argument thunk"
    );
    // This *validated thunk* tail-jumps through binding+8 after Instance+0xc0.
    // It is not a two-word MSVC method pair; no padding/adjust word is read.
    let member = memory.read_u64(binding + 8)? as usize;
    let member_code = verified_code(memory, studio, layout, member, 64)?;
    let wrapper_code = verified_code(memory, studio, layout, wrapper, 32)?;
    anyhow::ensure!(
        memory.read_u64(descriptor)? as usize == descriptor_vtable
            && memory.read_u64(descriptor + field)? as usize == binding
            && memory.read_u64(binding)? as usize == binding_vtable
            && memory.read_u64(binding + 8)? as usize == member
            && memory.read_u64(binding_vtable + getter_slot)? as usize == getter,
        "Typed binding changed during discovery"
    );
    let mut spec = vec![0; SPEC_SIZE];
    for (index, value) in [
        descriptor,
        descriptor_vtable,
        binding,
        binding_vtable,
        getter,
        member,
        wrapper,
    ]
    .into_iter()
    .enumerate()
    {
        put_u64(&mut spec, index * 8, value);
    }
    for (index, value) in [
        field,
        getter_slot,
        8,
        wrapper_slot,
        if kind == Kind::Boolean { 1 } else { 2 },
    ]
    .into_iter()
    .enumerate()
    {
        put_u32(&mut spec, 56 + index * 4, u32::try_from(value)?);
    }
    spec[80..104].copy_from_slice(&getter_code);
    spec[104..168].copy_from_slice(&member_code);
    spec[168..200].copy_from_slice(&wrapper_code);
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wrapper(field: u32, slot: u8, float: bool, scratch: bool) -> Vec<u8> {
        let mut code = vec![0x48, 0x8b, 0x89]; // mov rcx,[rcx+field]
        code.extend(field.to_le_bytes());
        code.extend([0x49, 0x8b, 0xd8]); // mov rbx,r8
        if scratch {
            code.extend([0x4c, 0x8b, 0x19, 0x41, 0xff, 0x53, slot]);
        } else {
            code.extend([0x48, 0x8b, 0x01, 0xff, 0x50, slot]);
        }
        if float {
            code.extend([0x0f, 0x28, 0xf0]);
        }
        // movaps xmm6,xmm0
        else {
            code.extend([0x0f, 0xb6, 0xf8]);
        } // movzx edi,al
        code
    }
    #[test]
    fn native_typed_observed_and_relocated_wrapper() {
        for field in [0x90, 0xb8] {
            for slot in [0x18, 0x28] {
                for scratch in [false, true] {
                    for kind in [Kind::Boolean, Kind::Float32] {
                        let code = wrapper(field, slot, kind == Kind::Float32, scratch);
                        assert_eq!(
                            typed_call(&code, &[field as usize], kind),
                            Some((field as usize, slot as usize))
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn native_typed_rejects_wrong_abi_and_return() {
        let bool_code = wrapper(0x90, 0x18, false, false);
        assert_eq!(typed_call(&bool_code, &[0x90], Kind::Float32), None);
        assert_eq!(typed_call(&bool_code, &[0x88], Kind::Boolean), None);
        for prefix in [&[0x48, 0x8b, 0xd1][..], &[0x48, 0x83, 0xc2, 8], &[0xeb, 0]] {
            let mut code = prefix.to_vec();
            code.extend(&bool_code);
            assert_eq!(typed_call(&code, &[0x90], Kind::Boolean), None);
        }
        for end in 0..bool_code.len() {
            assert_eq!(typed_call(&bool_code[..end], &[0x90], Kind::Boolean), None);
        }
    }
    #[test]
    fn native_typed_rejects_ambiguous_invokers() {
        assert!(unique_call(&[]).is_err());
        assert!(unique_call(&[(0x90, 0x18, 1), (0x90, 0x18, 2)]).is_err());
        assert_eq!(unique_call(&[(0x90, 0x18, 1); 2]).unwrap(), (0x90, 0x18, 1));
    }
}
