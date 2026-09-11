//! Capture persistent engine identities on the same DataModel queue used by
//! protected properties. Debug IDs remain transport keys, never disk identity.
use super::*;
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
) -> Result<HashMap<String, UniqueId>> {
    anyhow::ensure!(
        !services.is_empty() && services.len() <= 64,
        "Native identity capture needs 1–64 services"
    );
    // Operation 4 validates and retains the current service graph before reading
    // identities. It needs no preceding identity grant (which costs another
    // DataModel queue turn while Studio is busy processing a large insertion).
    let mut prepared = discover_property(
        pid,
        title,
        &[services[0].clone()],
        &[],
        "Name",
        timeout,
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
    let _trace = crate::app::timing::trace_scope("native.export", "capture native identity graph");
    decode_identities(&prepared.invoke(4)?[16..])
}

fn decode_identities(rows: &[u8]) -> Result<HashMap<String, UniqueId>> {
    anyhow::ensure!(
        rows.len().is_multiple_of(64),
        "Invalid native identity rows"
    );
    let mut result = HashMap::with_capacity(rows.len() / 64);
    let mut seen = HashSet::with_capacity(result.capacity());
    for row in rows.chunks_exact(64) {
        let word = |at| read_u32(row, at).unwrap();
        let id = UniqueId::new(
            word(12),
            word(8),
            ((u64::from(word(0)) << 32) | u64::from(word(4))) as i64,
        );
        let text = &row[16..];
        let end = text
            .iter()
            .position(|b| *b == 0)
            .context("Unterminated native debug ID")?;
        let key = std::str::from_utf8(&text[..end])?;
        anyhow::ensure!(
            end > 0
                && text[end..].iter().all(|b| *b == 0)
                && !id.is_nil()
                && seen.insert(id)
                && result.insert(key.into(), id).is_none(),
            "Native identity capture contains an invalid or duplicate identity"
        );
    }
    Ok(result)
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

#[test]
fn identity_rows_reject_collisions_and_preserve_native_word_order() {
    let mut row = [0; 64];
    for (offset, word) in [(0, 0x12345678u32), (4, 0x87654321), (8, 10), (12, 20)] {
        row[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
    }
    row[16..20].copy_from_slice(b"0_42");
    let parsed = decode_identities(&row).unwrap();
    assert_eq!(parsed["0_42"], UniqueId::new(20, 10, 0x1234567887654321));
    assert!(decode_identities(&[row, row].concat()).is_err());
    row[63] = 1;
    assert!(decode_identities(&row).is_err());
}

#[test]
#[ignore = "Read-only bulk identity capture in the owned Mac properties fixture"]
fn identity_capture_live_fixture() -> Result<()> {
    let pid = std::env::var("RENIUM_INSPECT_FIXTURE_PID")?.parse()?;
    let title = std::env::var("RENIUM_INSPECT_FIXTURE_TITLE")
        .unwrap_or_else(|_| "ReniumPropertyPackageTest.rbxl".into());
    anyhow::ensure!(
        title == "ReniumPropertyPackageTest.rbxl" || title.starts_with("ReniumBench-"),
        "Expected an owned identity fixture"
    );
    let mut timings = Vec::new();
    let mut values = HashMap::new();
    for _ in 0..3 {
        let started = Instant::now();
        let captured = capture_identities(
            pid,
            &title,
            &[
                "Workspace".into(),
                "ReplicatedStorage".into(),
                "ServerStorage".into(),
            ],
            Duration::from_secs(3),
        )?;
        timings.push(started.elapsed().as_secs_f64() * 1000.0);
        anyhow::ensure!(
            values.is_empty() || values == captured,
            "Native identities changed without an edit"
        );
        values = captured;
    }
    let output = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../audit/release-readiness/native-identity-capture.json");
    fs::write(
        output,
        serde_json::to_vec_pretty(&serde_json::json!({
            "ms": timings,
            "identities": values.iter().map(|(debug, id)| (debug, id.to_string())).collect::<HashMap<_, _>>()
        }))?,
    )?;
    Ok(())
}
