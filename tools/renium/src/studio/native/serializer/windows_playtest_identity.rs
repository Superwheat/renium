//! Bind the local-test bootstrap's JSON fields before PlayerConfigurer creates
//! the player. No player properties, allocator counters or security are changed.
use super::*;
use crate::system::LockRecover;
use iced_x86::{
    Decoder, DecoderOptions, FlowControl, InstructionInfoFactory, Mnemonic, OpAccess, OpKind,
    Register,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    Stack(i64),
    Key(usize),
    Number(u64),
}

// The value an instruction leaves in its destination, as far as the walk
// tracks it: a key address, a stack slot, a copied slot run or an immediate.
fn tracked_value(
    i: &iced_x86::Instruction,
    address: Option<i64>,
    registers: &HashMap<Register, Vec<Origin>>,
    stack: &HashMap<i64, Origin>,
    keys: &HashMap<usize, usize>,
) -> Option<Vec<Origin>> {
    match i.mnemonic() {
        Mnemonic::Lea if i.is_ip_rel_memory_operand() => keys
            .get(&(i.ip_rel_memory_address() as usize))
            .map(|key| vec![Origin::Key(*key)]),
        Mnemonic::Lea => address.map(|at| vec![Origin::Stack(at)]),
        Mnemonic::Mov
        | Mnemonic::Movaps
        | Mnemonic::Movups
        | Mnemonic::Movdqa
        | Mnemonic::Movdqu => match i.op1_kind() {
            OpKind::Register => registers.get(&i.op1_register().full_register()).cloned(),
            OpKind::Memory => address.and_then(|at| {
                (0..i.memory_size().size())
                    .step_by(8)
                    .map(|n| stack.get(&(at + n as i64)).copied())
                    .collect::<Option<Vec<_>>>()
            }),
            OpKind::Immediate32 | OpKind::Immediate32to64 | OpKind::Immediate64 => {
                Some(vec![Origin::Number(i.immediate(1))])
            }
            _ => None,
        },
        Mnemonic::Add | Mnemonic::Sub
            if i.op0_kind() == OpKind::Register
                && matches!(
                    i.op1_kind(),
                    OpKind::Immediate8to64 | OpKind::Immediate32to64
                ) =>
        {
            match registers
                .get(&i.op0_register().full_register())
                .and_then(|v| v.first())
            {
                Some(Origin::Stack(at)) => Some(vec![Origin::Stack(
                    at + i.immediate(1) as i64 * if i.mnemonic() == Mnemonic::Sub { -1 } else { 1 },
                )]),
                _ => None,
            }
        }
        _ => None,
    }
}

// Follow the string_view key through scalar/SIMD copies to the writer's RDX.
// This does not depend on registers, stack offsets or instruction distances.
fn writers(code: &[u8], entry: usize, keys: &HashMap<usize, usize>) -> Result<[(usize, usize); 3]> {
    let mut registers = HashMap::from([(Register::RSP, vec![Origin::Stack(0)])]);
    let mut stack = HashMap::new();
    let mut found = [Vec::new(), Vec::new(), Vec::new()];
    let mut factory = InstructionInfoFactory::new();
    for i in Decoder::with_ip(64, code, entry as u64, DecoderOptions::NONE) {
        let address = if i.memory_index() == Register::None {
            match registers
                .get(&i.memory_base().full_register())
                .and_then(|v| v.first())
            {
                Some(Origin::Stack(at)) => Some(*at + i.memory_displacement64() as i64),
                _ => None,
            }
        } else {
            None
        };
        if matches!(
            i.flow_control(),
            FlowControl::Call | FlowControl::IndirectCall
        ) {
            if i.flow_control() == FlowControl::Call
                && let Some(Origin::Stack(at)) =
                    registers.get(&Register::RDX).and_then(|v| v.first())
                && let (Some(Origin::Key(key)), Some(Origin::Number(length))) =
                    (stack.get(at), stack.get(&(at + 8)))
                && *length == [6, 8, 11][*key]
            {
                found[*key].push((i.ip() as usize, i.near_branch_target() as usize));
            }
            for register in [
                Register::RAX,
                Register::RCX,
                Register::RDX,
                Register::R8,
                Register::R9,
                Register::R10,
                Register::R11,
                Register::ZMM0,
                Register::ZMM1,
                Register::ZMM2,
                Register::ZMM3,
                Register::ZMM4,
                Register::ZMM5,
            ] {
                registers.remove(&register);
            }
            continue;
        }
        let value = tracked_value(&i, address, &registers, &stack, keys);
        let sp = registers.get(&Register::RSP).cloned();
        for register in factory.info(&i).used_registers() {
            if matches!(
                register.access(),
                OpAccess::Write
                    | OpAccess::CondWrite
                    | OpAccess::ReadWrite
                    | OpAccess::ReadCondWrite
            ) {
                registers.remove(&register.register().full_register());
            }
        }
        if i.op0_kind() == OpKind::Register {
            if let Some(value) = value {
                registers.insert(i.op0_register().full_register(), value);
            }
        } else if i.op0_kind() == OpKind::Memory
            && let Some(at) = address
            && factory
                .info(&i)
                .used_memory()
                .iter()
                .any(|m| matches!(m.access(), OpAccess::Write | OpAccess::ReadWrite))
        {
            let size = i.memory_size().size() as i64;
            stack.retain(|offset, _| *offset + 8 <= at || *offset >= at + size);
            if let Some(value) = value {
                for (n, v) in value.into_iter().enumerate() {
                    stack.insert(at + n as i64 * 8, v);
                }
            }
        }
        if matches!(i.mnemonic(), Mnemonic::Push | Mnemonic::Pop)
            && let Some(Origin::Stack(at)) = sp.as_ref().and_then(|v| v.first())
        {
            registers.insert(
                Register::RSP,
                vec![Origin::Stack(
                    at + if i.mnemonic() == Mnemonic::Push {
                        -8
                    } else {
                        8
                    },
                )],
            );
        }
    }
    anyhow::ensure!(
        found.iter().all(|v| v.len() == 1),
        "Local-test identity writers are ambiguous or missing"
    );
    let result = [found[0][0], found[1][0], found[2][0]];
    anyhow::ensure!(
        result[1].1 == result[2].1 && result[0].1 != result[1].1,
        "Local-test identity writers changed types"
    );
    Ok(result)
}

#[derive(Clone)]
struct Binding {
    calls: [(usize, usize); 3],
    code: Vec<(usize, Vec<u8>)>,
}

pub(super) fn bindings(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
) -> Result<[(usize, usize); 3]> {
    static CACHE: OnceLock<Mutex<HashMap<[u32; 3], Binding>>> = OnceLock::new();
    let stamp = package_layout(&studio.path)?.image_stamp;
    let cached = CACHE
        .get_or_init(Default::default)
        .lock_recover()
        .get(&stamp)
        .cloned();
    let binding = if let Some(binding) = cached {
        binding
    } else {
        let mapped = observation::ImageView::open(&studio.path)?;
        let pe = PeImage::parse(mapped.bytes())?;
        let anchors = image::anchors(
            &pe,
            &[
                "GenerateTeleportJoin".into(),
                "UserId".into(),
                "UserName".into(),
                "DisplayName".into(),
            ],
        )?;
        let index = image::index(&pe, &anchors.iter().map(|a| a.rva).collect())?;
        let entries: HashSet<_> = anchors
            .iter()
            .filter(|a| a.text == "GenerateTeleportJoin")
            .flat_map(|a| {
                index
                    .references
                    .get(&a.rva)
                    .into_iter()
                    .flatten()
                    .map(|e| e.target)
            })
            .collect();
        let keys: HashMap<_, _> = anchors
            .iter()
            .filter_map(|a| {
                ["UserId", "UserName", "DisplayName"]
                    .iter()
                    .position(|s| *s == a.text)
                    .map(|n| (a.rva, n))
            })
            .collect();
        let mut candidates = Vec::new();
        for entry in entries {
            let (begin, end) = image::code_range(&pe, &index, entry)?;
            let bytes = slice(pe.bytes, pe.rva_to_offset(begin)?, end - begin)?.to_vec();
            if let Ok(calls) = writers(&bytes, begin, &keys) {
                candidates.push((calls, (begin, bytes)));
            }
        }
        anyhow::ensure!(candidates.len() == 1, "Local-test bootstrap is not unique");
        let (calls, body) = candidates.pop().unwrap();
        let mut code = vec![body];
        for entry in [calls[0].1, calls[1].1] {
            let (begin, end) = image::code_range(&pe, &index, entry)?;
            anyhow::ensure!(
                begin == entry && !index.invalid.contains(&entry),
                "Invalid local-test JSON writer"
            );
            code.push((
                begin,
                slice(pe.bytes, pe.rva_to_offset(begin)?, end - begin)?.to_vec(),
            ));
        }
        let binding = Binding { calls, code };
        CACHE
            .get()
            .unwrap()
            .lock_recover()
            .insert(stamp, binding.clone());
        binding
    };
    for (at, bytes) in &binding.code {
        anyhow::ensure!(
            memory.read_vec(studio.base + at, bytes.len())? == *bytes,
            "Local-test bootstrap changed after discovery"
        );
    }
    Ok(binding
        .calls
        .map(|(at, target)| (studio.base + at, studio.base + target)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_writers_follow_relocated_stack_and_simd_keys() {
        for (entry, stack_at, copy_at) in [(0x1000, 0x20u8, 0x50u8), (0x8000, 0x40, 0x70)] {
            let keys = HashMap::from([(entry + 0x500, 0), (entry + 0x510, 1), (entry + 0x520, 2)]);
            let mut code = vec![0x55, 0x48, 0x83, 0xec, 0x78, 0x48, 0x89, 0xe5];
            let mut expected = Vec::new();
            for (n, length) in [6u8, 8, 11].into_iter().enumerate() {
                code.extend([0x48, 0x8d, 0x05]);
                let displacement =
                    (entry + 0x500 + n * 0x10) as i32 - (entry + code.len() + 4) as i32;
                code.extend(displacement.to_le_bytes());
                code.extend([
                    0x48,
                    0x89,
                    0x45,
                    stack_at,
                    0x48,
                    0xc7,
                    0x45,
                    stack_at + 8,
                    length,
                    0,
                    0,
                    0,
                ]);
                code.extend([
                    0x0f, 0x10, 0x4d, stack_at, 0x0f, 0x11, 0x4d, copy_at, 0x48, 0x8d, 0x55,
                    copy_at,
                ]);
                let at = entry + code.len();
                let target = entry + if n == 0 { 0x900 } else { 0xa00 };
                code.push(0xe8);
                code.extend((target as i32 - at as i32 - 5).to_le_bytes());
                expected.push((at, target));
            }
            assert_eq!(writers(&code, entry, &keys).unwrap().as_slice(), expected);
            let call = expected[1].0 - entry;
            code.splice(call..call, [0x31, 0xd2]);
            assert!(
                writers(&code, entry, &keys).is_err(),
                "a clobbered key argument must fail qualification"
            );
        }
    }

    #[test]
    #[ignore = "requires an explicitly owned installed Studio process"]
    fn owned_test_identity_binding() -> Result<()> {
        let pid = std::env::var("RENIUM_TELEPORT_TEST_PID")?.parse()?;
        let memory = ProcessMemory::open_read_only(pid)?;
        let loaded = modules(pid)?;
        println!("{:?}", bindings(&memory, &loaded[0])?);
        Ok(())
    }
}
