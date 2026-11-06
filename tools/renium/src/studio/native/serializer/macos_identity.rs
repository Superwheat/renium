//! Capture persistent engine identities on the same DataModel queue used by
//! protected properties when its structural locator is available.
use super::*;
use crate::studio::native::serializer::macos_identity_rows::decode_identities;
use rbx_dom_weak::types::UniqueId;

#[derive(Clone, Copy, Debug, PartialEq)]
enum MethodArgument {
    Descriptor,
    Instance,
    Field(usize),
}

fn method_call(code: &[u8], address: u64) -> Option<(usize, u64)> {
    use MethodArgument::*;
    let mut registers = [None; 32];
    registers[0] = Some(Descriptor);
    registers[1] = Some(Instance);
    for (index, bytes) in code.chunks_exact(4).enumerate() {
        let word = u32::from_le_bytes(bytes.try_into().ok()?);
        let dst = (word & 31) as usize;
        let base = ((word >> 5) & 31) as usize;
        if word & 0xffe0ffe0 == 0xaa0003e0 {
            registers[dst] = registers[((word >> 16) & 31) as usize];
        } else if word & 0xffc00000 == 0xa9400000 && registers[base] == Some(Descriptor) {
            let offset = (((word >> 15) & 127) as i8) << 1 >> 1;
            if offset < 0 {
                return None;
            }
            let field = offset as usize * 8;
            registers[dst] = Some(Field(field));
            registers[((word >> 10) & 31) as usize] = Some(Field(field + 8));
        } else if word & 0xfc000000 == 0x94000000 {
            if let (Some(Instance), Some(Field(field)), Some(Field(high))) =
                (registers[0], registers[1], registers[2])
                && high == field + 8
                && (64..256).contains(&field)
            {
                let displacement = ((word << 6) as i32 >> 4) as i64;
                return Some((
                    field,
                    (address as i64 + index as i64 * 4 + displacement) as u64,
                ));
            }
            registers[..19].fill(None);
        } else if word == 0xd65f03c0 || word & 0xfc000000 == 0x14000000 {
            return None;
        } else if word & 0xff000010 != 0x54000000
            && word & 0x7e000000 != 0x34000000
            && word & 0x7e000000 != 0x36000000
            && word & 0x3b000000 != 0x29000000
            && word & 0xffc00000 != 0xf9000000
        {
            registers[dst] = None;
        }
    }
    None
}

fn validate_member_caller(code: &[u8]) -> bool {
    // Itanium ARM64 member pointer: low bit of adjustment selects a virtual
    // member; arithmetic shift supplies this adjustment. The int argument is
    // loaded into w1 and libc++ string return storage is passed in x8.
    let sequence = [
        0xaa0103e9, 0x8b820400, 0x36000062, 0xf9400008, 0xf8694909, 0xb9400081, 0x910023e8,
        0xd63f0120,
    ];
    code.windows(sequence.len() * 4).step_by(4).any(|bytes| {
        bytes
            .chunks_exact(4)
            .zip(sequence)
            .all(|(bytes, word)| u32::from_le_bytes(bytes.try_into().unwrap()) == word)
    })
}

fn debug_binding(memory: &Memory, instance: u64, class_offset: u64) -> Result<[u64; 6]> {
    let descriptor = memory.member(instance, class_offset, "GetDebugId")?;
    anyhow::ensure!(
        memory.rtti(descriptor)?
            == concat!(
                "N3RBX10Reflection13BoundFuncDescINS_8InstanceEFNSt3__112basic_stringIcNS3_",
                "11char_traitsIcEENS3_9allocatorIcEEEEiELb0ELi1EEE"
            ),
        "Studio GetDebugId signature is unsupported"
    );
    let table = memory.pointer(descriptor)?;
    let mut candidates = Vec::new();
    for slot in (0..32).step_by(8) {
        let invoker = memory.pointer(table + slot)?;
        let code = memory.code(invoker, 256)?;
        let Some((field, caller)) = method_call(&code, invoker) else {
            continue;
        };
        anyhow::ensure!(
            validate_member_caller(&memory.code(caller, 128)?),
            "Studio GetDebugId member call ABI changed"
        );
        let pair = memory.read(descriptor + field as u64, 16)?;
        anyhow::ensure!(
            read_u64(&pair, 8) == Some(0),
            "Studio GetDebugId requires an unsupported Instance adjustment"
        );
        let function = read_u64(&pair, 0).unwrap();
        let entry = memory.code(function, 8)?;
        let adjustment = read_u32(&entry, 0).unwrap();
        let branch = read_u32(&entry, 4).unwrap();
        anyhow::ensure!(
            adjustment & 0xffc003ff == 0x91000000 && branch & 0xfc000000 == 0x14000000,
            "Studio GetDebugId is not a supported direct formatter entry"
        );
        let destination = (function as i64 + 4 + ((branch << 6) as i32 >> 4) as i64) as u64;
        memory.code(destination, 64)?;
        anyhow::ensure!(
            memory.pointer(descriptor)? == table
                && memory.pointer(table + slot)? == invoker
                && memory.read(descriptor + field as u64, 16)? == pair,
            "Studio GetDebugId binding changed during discovery"
        );
        candidates.push([descriptor, table, slot, invoker, field as u64, function]);
    }
    anyhow::ensure!(
        candidates.len() == 1,
        "Studio GetDebugId binding is ambiguous"
    );
    Ok(candidates[0])
}

pub(crate) fn capture_identities(
    pid: u32,
    title: &str,
    services: &[String],
    timeout: Duration,
) -> Result<Option<HashMap<String, UniqueId>>> {
    super::super::super::optional_native(
        read_identities(pid, title, services, timeout)
            .map_err(|error| macos_deadline::identity_capture_error(pid, error)),
    )
}

fn read_identities(
    pid: u32,
    title: &str,
    services: &[String],
    timeout: Duration,
) -> Result<HashMap<String, UniqueId>> {
    let started = Instant::now();
    anyhow::ensure!(
        !services.is_empty() && services.len() <= 64,
        "Native identity capture needs 1–64 services"
    );
    // The capture task itself validates the queue ABI, retains the DataModel,
    // and validates the service graph before reading identities. A separate
    // descriptor probe duplicates discovery and used to queue an empty task.
    let mut prepared = discover_property(
        pid,
        title,
        &[services[0].clone()],
        &[],
        "Name",
        timeout,
        None,
        None,
    )?;
    let memory = &prepared.memory;
    let context = memory.request(0, &[0], title)?;
    anyhow::ensure!(
        context.len() == 64 && read_u64(&context, 0) == Some(memory.base),
        "Native identity capture context changed"
    );
    let instance = read_u64(&prepared.parameters, 8).unwrap();
    let binding = debug_binding(memory, instance, read_u64(&context, 40).unwrap())?;
    let children_offset = read_u64(&context, 24).unwrap();
    let children = memory.children(read_u64(&context, 8).unwrap(), children_offset)?;
    let names = memory.instance_classes(&children, read_u64(&context, 40).unwrap())?;
    let wanted = services.iter().collect::<HashSet<_>>();
    anyhow::ensure!(
        wanted.len() == services.len(),
        "Native identity services repeat"
    );
    let roots = children
        .into_iter()
        .zip(names)
        .filter_map(|(root, name)| wanted.contains(&name).then_some(root))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        roots.len() == services.len(),
        "Native identity service roots changed"
    );
    let mut input = Vec::new();
    for value in binding
        .into_iter()
        .chain([children_offset, roots.len() as u64])
    {
        input.extend_from_slice(&value.to_le_bytes());
    }
    for (instance, owner) in roots {
        input.extend_from_slice(&instance.to_le_bytes());
        input.extend_from_slice(&owner.to_le_bytes());
    }
    put32(&mut prepared.parameters, 136, input.len() as u32);
    prepared.parameters[680..680 + input.len()].copy_from_slice(&input);
    let discovered = Instant::now();
    let rows = prepared.invoke(4)?;
    let captured = Instant::now();
    let identities = decode_identities(&rows[16..])?;
    super::super::super::probes::record(
        pid,
        NativeOperation::Capture,
        &Ok(serde_json::json!({
            "instances": identities.len(),
            "discoveryMs": discovered.duration_since(started).as_secs_f64() * 1000.0,
            "captureMs": captured.duration_since(discovered).as_secs_f64() * 1000.0,
            "decodeMs": captured.elapsed().as_secs_f64() * 1000.0,
            "responseBytes": rows.len(),
        })),
    );
    Ok(identities)
}

#[test]
fn debug_method_discovery_follows_descriptor_pair_through_argument_clobbers() {
    let words: [u32; 46] = [
        0xd10183ff, 0xa90167fa, 0xa9025ff8, 0xa90357f6, 0xa9044ff4, 0xa9057bfd, 0x910143fd,
        0xaa0303f5, 0xaa0203f3, 0xaa0103f4, 0xaa0003f7, 0xf9400416, 0x39c05ec8, 0x36f80048,
        0xf94002d6, 0xf9400e88, 0xf9400518, 0x39c05f08, 0x36f80048, 0xf9400318, 0xaa1303e0,
        0x9500bce4, 0x4b150003, 0xa9486af9, 0xf9404ae4, 0xaa1303e0, 0xaa1503e1, 0x52800022,
        0xaa1603e5, 0xaa1803e6, 0x942ac4a2, 0xb9000fe0, 0x910033e4, 0xaa1403e0, 0xaa1903e1,
        0xaa1a03e2, 0xaa1303e3, 0xd2800005, 0x94000008, 0xa9457bfd, 0xa9444ff4, 0xa94357f6,
        0xa9425ff8, 0xa94167fa, 0x910183ff, 0xd65f03c0,
    ];
    let bytes = |words: &[u32]| {
        words
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect::<Vec<_>>()
    };
    assert_eq!(method_call(&bytes(&words), 0x1000), Some((128, 0x10b8)));
    let mut wrong = words;
    wrong[35] = 0xaa0003e2; // The high member word no longer reaches x2.
    assert_eq!(method_call(&bytes(&wrong), 0x1000), None);
}
