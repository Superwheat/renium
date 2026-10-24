//! Reuse the owned client document through PlaceOpener's native replacement.
use super::*;
use crate::system::LockRecover;

const OPENER: &str = ".?AVPlaceOpener@Studio@RBX@@";
const INVOKE: &str = ".?AV?$BoundYieldFuncDesc@VBindableFunction@RBX@@$$A6A?AV?$shared_ptr@$$CBUTuple@Reflection@RBX@@@std@@V34@@Z$0A@$00@Reflection@RBX@@";

#[derive(Clone, Debug, serde::Serialize)]
struct Binding {
    table: usize,
    bases: Vec<(usize, usize)>,
    parse: usize,
    replace: usize,
    start: usize,
    open: usize,
    close: usize,
    receiver: usize,
    capture_self: usize,
    capture_config: usize,
    destroy: usize,
    future_shared: usize,
    calls: Vec<usize>,
    #[serde(skip)]
    code: Vec<(usize, Vec<u8>)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Origin {
    Input(u8, i64),
    Stack(i64),
}
impl Origin {
    fn offset(self, by: i64) -> Self {
        match self {
            Self::Input(i, at) => Self::Input(i, at + by),
            Self::Stack(at) => Self::Stack(at + by),
        }
    }
}

// Follow the public wrapper's input/capture flow to its asynchronous scheduler.
// Calling the synchronous wrapper from an external thread is invalid in Tasks.
fn scheduler(bytes: &[u8], at: usize) -> Result<(usize, usize, usize, usize, usize, usize)> {
    use iced_x86::{Decoder, DecoderOptions, FlowControl, Mnemonic, OpKind, Register};
    let mut regs = HashMap::from([
        (Register::RCX, Origin::Input(0, 0)),
        (Register::RDX, Origin::Input(1, 0)),
        (Register::R8, Origin::Input(2, 0)),
        (Register::R9, Origin::Input(3, 0)),
        (Register::RSP, Origin::Stack(0)),
    ]);
    let mut stack = HashMap::new();
    let mut copied = None;
    let mut scheduled = None;
    let mut destroy = HashSet::new();
    let mut future_shared = None;
    for i in Decoder::with_ip(64, bytes, at as u64, DecoderOptions::NONE) {
        let address = if i.is_ip_rel_memory_operand() || i.memory_index() != Register::None {
            None
        } else {
            regs.get(&i.memory_base().full_register())
                .map(|v| v.offset(i.memory_displacement64() as i64))
        };
        if i.flow_control() == FlowControl::Call {
            let target = i.near_branch_target() as usize;
            if regs.get(&Register::RDX) == Some(&Origin::Input(2, 0))
                && let Some(Origin::Stack(to)) = regs.get(&Register::RCX)
            {
                copied = Some(*to);
            }
            if regs.get(&Register::RCX) == Some(&Origin::Input(2, 0)) {
                destroy.insert(target);
            }
            if let (
                Some(Origin::Input(0, receiver)),
                Some(Origin::Stack(result)),
                Some(Origin::Stack(_)),
                Some(Origin::Stack(capture)),
            ) = (
                regs.get(&Register::RCX),
                regs.get(&Register::RDX),
                regs.get(&Register::R8),
                regs.get(&Register::R9),
            ) && let Some(config) = copied
            {
                let self_fields = stack
                    .iter()
                    .filter(|(offset, value)| {
                        **offset >= *capture && **offset < config && **value == Origin::Input(0, 0)
                    })
                    .map(|(offset, _)| *offset - capture)
                    .collect::<Vec<_>>();
                anyhow::ensure!(
                    self_fields.len() == 1 && scheduled.is_none(),
                    "Ambiguous client replacement capture"
                );
                scheduled = Some((target, *receiver, self_fields[0], config - capture, *result));
            }
            if let Some((_, _, _, _, result)) = scheduled
                && let Some(Origin::Stack(offset)) = regs.get(&Register::RCX)
                && regs.get(&Register::RDX) == Some(&Origin::Input(1, 0))
            {
                future_shared = Some(offset - result);
            }
        }
        if matches!(
            i.flow_control(),
            FlowControl::Call | FlowControl::IndirectCall
        ) {
            for reg in [
                Register::RAX,
                Register::RCX,
                Register::RDX,
                Register::R8,
                Register::R9,
                Register::R10,
                Register::R11,
            ] {
                regs.remove(&reg);
            }
            continue;
        }
        if i.mnemonic() == Mnemonic::Push {
            if let Some(sp) = regs.get_mut(&Register::RSP) {
                *sp = sp.offset(-8);
            }
            continue;
        }
        let value = match i.mnemonic() {
            Mnemonic::Lea => address,
            Mnemonic::Mov | Mnemonic::Movzx if i.op1_kind() == OpKind::Register => {
                regs.get(&i.op1_register().full_register()).copied()
            }
            Mnemonic::Mov if i.op1_kind() == OpKind::Memory => match address {
                Some(Origin::Stack(offset)) => stack.get(&offset).copied(),
                _ => None,
            },
            Mnemonic::Add | Mnemonic::Sub
                if i.op0_kind() == OpKind::Register
                    && matches!(
                        i.op1_kind(),
                        OpKind::Immediate8to64 | OpKind::Immediate32to64
                    ) =>
            {
                regs.get(&i.op0_register().full_register()).map(|v| {
                    v.offset(
                        (if i.mnemonic() == Mnemonic::Sub { -1 } else { 1 })
                            * i.immediate(1) as i64,
                    )
                })
            }
            _ => None,
        };
        if i.op0_kind() == OpKind::Memory
            && let Some(Origin::Stack(offset)) = address
        {
            if let Some(value) = value {
                stack.insert(offset, value);
            } else {
                stack.remove(&offset);
            }
        }
        if i.op0_kind() == OpKind::Register
            && !matches!(i.mnemonic(), Mnemonic::Cmp | Mnemonic::Test)
        {
            let reg = i.op0_register().full_register();
            if let Some(value) = value {
                regs.insert(reg, value);
            } else {
                regs.remove(&reg);
            }
        }
    }
    let (target, receiver, capture_self, capture_config, _) =
        scheduled.context("Missing native client scheduler")?;
    let shared = future_shared.context("Missing client future ownership")?;
    anyhow::ensure!(
        destroy.len() == 1
            && [receiver, capture_self, capture_config, shared]
                .iter()
                .all(|at| (0..65536).contains(at)),
        "Invalid native client scheduler layout"
    );
    Ok((
        target,
        receiver as usize,
        capture_self as usize,
        capture_config as usize,
        *destroy.iter().next().unwrap(),
        shared as usize,
    ))
}

fn discover_binding(path: &std::path::Path) -> Result<Binding> {
    let mapped = observation::ImageView::open(path)?;
    let pe = PeImage::parse(mapped.bytes())?;
    let anchors = image::anchors(
        &pe,
        &[
            OPENER.into(),
            "PlaceOpener/startClient".into(),
            "PlaceOpener/replacePlace".into(),
            "PlaceOpener/openPlace".into(),
            "Invalid Place Config, expected a Lua table".into(),
        ],
    )?;
    let all_tables = image::rtti(&pe, &anchors)?;
    let bases = all_tables
        .iter()
        .filter(|row| {
            row["type"] == OPENER && row["offsetToTop"].as_u64().is_some_and(|offset| offset > 0)
        })
        .map(|row| {
            (
                row["offsetToTop"].as_u64().unwrap() as usize,
                row["vtableRva"].as_u64().unwrap() as usize,
            )
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        !bases.is_empty(),
        "PlaceOpener has no component ownership bases"
    );
    let tables = all_tables
        .into_iter()
        .filter(|row| row["type"] == OPENER && row["offsetToTop"] == 0)
        .collect::<Vec<_>>();
    anyhow::ensure!(
        tables.len() == 1,
        "Client PlaceOpener has no unique complete type"
    );
    let table = &tables[0];
    let index = image::index(&pe, &anchors.iter().map(|a| a.rva).collect())?;
    let anchored = |text: &str, member: bool| -> Result<usize> {
        let mut candidates = anchors
            .iter()
            .filter(|a| a.text == text)
            .flat_map(|a| {
                index
                    .references
                    .get(&a.rva)
                    .into_iter()
                    .flatten()
                    .map(|e| e.target)
            })
            .filter(|entry| {
                !member
                    || table["slots"].as_array().is_some_and(|slots| {
                        slots
                            .iter()
                            .any(|slot| slot["targetRva"].as_u64() == Some(*entry as u64))
                    })
            })
            .collect::<Vec<_>>();
        candidates.sort_unstable();
        candidates.dedup();
        anyhow::ensure!(
            candidates.len() == 1,
            "Client {text} resolved {} native entries",
            candidates.len()
        );
        Ok(candidates[0])
    };
    let parse = anchored("Invalid Place Config, expected a Lua table", false)?;
    let replace = anchored("PlaceOpener/replacePlace", true)?;
    let start = anchored("PlaceOpener/startClient", true)?;
    let open = anchored("PlaceOpener/openPlace", true)?;
    let callers = index
        .callers
        .get(&open)
        .context("Client replacement has no open continuation")?;
    let continuations: HashSet<_> = callers.iter().map(|edge| edge.target).collect();
    anyhow::ensure!(
        continuations.len() == 1 && (1..=8).contains(&callers.len()),
        "Client replacement has ambiguous native open continuations"
    );
    let mut calls = Vec::new();
    for edge in callers {
        let offset = pe.rva_to_offset(edge.at)?;
        anyhow::ensure!(
            pe.bytes[offset] == 0xe8,
            "Client continuation is not a direct native call"
        );
        calls.push(edge.at);
    }
    let close = image::imports(&pe)?
        .as_array()
        .context("Missing Qt imports")?
        .iter()
        .find(|row| {
            row["module"] == "Qt5Widgets.dll" && row["symbol"] == "?close@QWidget@@QEAA_NXZ"
        })
        .and_then(|row| row["iatRva"].as_u64())
        .context("Missing client window close import")? as usize;
    // Verify the complete callable bodies and the replacement continuation,
    // including the engine's configuration converter. No layout is reproduced.
    let mut code = Vec::new();
    for entry in [parse, replace, start, open]
        .into_iter()
        .chain(continuations)
    {
        let (begin, end) = image::code_range(&pe, &index, entry)?;
        anyhow::ensure!(
            begin == entry && !index.invalid.contains(&entry),
            "Invalid native client function boundary"
        );
        code.push((
            begin,
            slice(pe.bytes, pe.rva_to_offset(begin)?, end - begin)?.to_vec(),
        ));
    }
    let public = code.iter().find(|(at, _)| *at == replace).unwrap();
    let (replace, receiver, capture_self, capture_config, destroy, future_shared) =
        scheduler(&public.1, replace)?;
    for entry in [replace, destroy] {
        let (begin, end) = image::code_range(&pe, &index, entry)?;
        anyhow::ensure!(
            begin == entry && !index.invalid.contains(&entry),
            "Invalid client scheduler function"
        );
        code.push((
            begin,
            slice(pe.bytes, pe.rva_to_offset(begin)?, end - begin)?.to_vec(),
        ));
    }
    Ok(Binding {
        table: table["vtableRva"]
            .as_u64()
            .context("Missing client vtable")? as usize,
        bases,
        parse,
        replace,
        start,
        open,
        close,
        receiver,
        capture_self,
        capture_config,
        destroy,
        future_shared,
        calls,
        code,
    })
}

fn opener(memory: &ProcessMemory, studio: &ModuleEntry, binding: &Binding) -> Result<usize> {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_PRIVATE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READWRITE,
        PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_READWRITE, PAGE_WRITECOPY, VirtualQueryEx,
    };
    static CACHE: OnceLock<Mutex<HashMap<(u32, String), usize>>> = OnceLock::new();
    let table = studio.base + binding.table;
    let owns = |at: usize| {
        binding.bases.iter().all(|(offset, table)| {
            memory.read_u64(at + offset).ok() == Some((studio.base + table) as u64)
        })
    };
    let key = (
        memory.pid,
        crate::app::update::process_start_identity(memory.pid).context("Client ended")?,
    );
    if let Some(at) = CACHE
        .get_or_init(Default::default)
        .lock_recover()
        .get(&key)
        .copied()
        && memory.read_u64(at).ok() == Some(table as u64)
        && owns(at)
        && properties::complete_reflection_type(memory, studio, at).as_deref() == Some(OPENER)
    {
        return Ok(at);
    }
    let mut at = 0usize;
    let mut found = Vec::new();
    let mut budget = 4usize * 1024 * 1024 * 1024;
    loop {
        let mut region: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe {
            VirtualQueryEx(
                memory.handle,
                at as *const _,
                &mut region,
                std::mem::size_of_val(&region),
            )
        } == 0
        {
            break;
        }
        let base = region.BaseAddress as usize;
        let end = base
            .checked_add(region.RegionSize)
            .context("Client memory region overflow")?;
        if region.State == MEM_COMMIT
            && region.Type == MEM_PRIVATE
            && region.Protect & PAGE_GUARD == 0
            && region.Protect
                & (PAGE_READWRITE
                    | PAGE_WRITECOPY
                    | PAGE_EXECUTE_READWRITE
                    | PAGE_EXECUTE_WRITECOPY)
                != 0
        {
            budget = budget
                .checked_sub(region.RegionSize)
                .context("Client owner scan exceeds its memory budget")?;
            for chunk in (base..end).step_by(4 * 1024 * 1024) {
                if let Ok(bytes) = memory.read_vec(chunk, (end - chunk).min(4 * 1024 * 1024)) {
                    for offset in (0..bytes.len().saturating_sub(7)).step_by(8) {
                        if read_u64(&bytes, offset)? as usize == table && owns(chunk + offset) {
                            found.push(chunk + offset);
                        }
                    }
                }
            }
        }
        anyhow::ensure!(end > at, "Client memory region did not advance");
        at = end;
    }
    anyhow::ensure!(
        found.len() == 1,
        "Client has {} PlaceOpener owners",
        found.len()
    );
    anyhow::ensure!(
        properties::complete_reflection_type(memory, studio, found[0]).as_deref() == Some(OPENER),
        "Client document owner changed"
    );
    let mut cache = CACHE.get().unwrap().lock_recover();
    if cache.len() > 32 {
        cache.clear();
    }
    cache.insert(key, found[0]);
    Ok(found[0])
}

fn binding(memory: &ProcessMemory, studio: &ModuleEntry) -> Result<Binding> {
    let layout = package_layout(&studio.path)?;
    static CACHE: OnceLock<Mutex<HashMap<[u32; 3], Binding>>> = OnceLock::new();
    let cached = CACHE
        .get_or_init(Default::default)
        .lock_recover()
        .get(&layout.image_stamp)
        .cloned();
    let binding = if let Some(binding) = cached {
        binding
    } else {
        let binding = discover_binding(&studio.path)?;
        CACHE
            .get()
            .unwrap()
            .lock_recover()
            .insert(layout.image_stamp, binding.clone());
        binding
    };
    for (rva, bytes) in &binding.code {
        anyhow::ensure!(
            memory.read_vec(studio.base + rva, bytes.len())? == *bytes,
            "Client replacement code changed after discovery"
        );
    }
    Ok(binding)
}

/// Qualify the existing client while its destination server starts. This only
/// reads memory; the same ownership and code checks run again at handoff.
pub(crate) fn prepare_test_client(pid: u32) -> Result<()> {
    let loaded = modules(pid)?;
    let studio = loaded.first().context("Teleport client ended")?;
    let memory = ProcessMemory::open_read_only(pid)?;
    let binding = binding(&memory, studio)?;
    opener(&memory, studio, &binding)?;
    Ok(())
}

pub(crate) fn rejoin_test_client(pid: u32, runtime: &str, path: &[String]) -> Result<()> {
    anyhow::ensure!(
        path == ["CoreGui", &format!("__ReniumTeleportRoute_{runtime}")]
            || cfg!(test)
                && path
                    == [
                        "ReplicatedStorage",
                        &format!("__ReniumTeleportRoute_{runtime}")
                    ],
        "Invalid client route relay"
    );
    let loaded = modules(pid)?;
    let studio = loaded.first().context("Teleport client ended")?;
    let title = crate::studio::input::studio_window_title(pid)?;
    let memory = ProcessMemory::open(pid)?;
    memory.authorize(NativeOperation::PlayTeleportGui, &title)?;
    let layout = package_layout(&studio.path)?;
    let binding = binding(&memory, studio)?;
    let model = active_data_model_for_path(pid, &memory, studio, layout.data, &title, Some(path))?;
    let entries = properties::resolve_path(&memory, &model, path, &[1, 1])?;
    let relay = entries.last().context("Client route relay ended")?;
    anyhow::ensure!(
        read_instance_class(&memory, relay.instance, model.layout).as_deref()
            == Some("BindableFunction"),
        "Client route relay changed"
    );
    let (fields, _, _) = owner::task_binding(&memory, studio, &model)?;
    let table = studio.base + binding.table;
    let opener = opener(&memory, studio, &binding)?;
    let helper = ensure_helper_loaded(pid, &memory, &loaded)?;
    let mut request = vec![0; 600];
    put_u32(&mut request, 0, 2);
    put_u32(&mut request, 4, 600);
    for (index, field) in fields.into_iter().enumerate() {
        put_u64(&mut request, 8 + index * 8, field);
    }
    let window = crate::studio::input::teleport_caption_window(pid)?.0 as usize;
    for (index, value) in [
        relay.instance,
        relay.owner,
        types::member(&memory, studio, INVOKE, "Invoke", false)?.1,
        types::singleton(&memory, studio, dispatch::TABLE, None)?,
        window,
        opener,
        table,
        studio.base + binding.parse,
        studio.base + binding.replace,
        studio.base + binding.start,
        studio.base + binding.open,
        studio.base + binding.close,
    ]
    .into_iter()
    .enumerate()
    {
        put_u64(&mut request, 136 + index * 8, value);
    }
    for (index, at) in binding.calls.iter().enumerate() {
        put_u64(&mut request, 232 + index * 8, studio.base + at);
    }
    put_u32(&mut request, 296, binding.calls.len() as u32);
    for (index, value) in [
        binding.receiver,
        binding.capture_self,
        binding.capture_config,
        studio.base + binding.destroy,
        binding.future_shared,
    ]
    .into_iter()
    .enumerate()
    {
        put_u64(&mut request, 304 + index * 8, value);
    }
    let mut input = memory.allocate(request.len())?;
    memory.write(input.address, &request)?;
    let exit = input.run(helper + helper_export_rva("ReniumTestClient")?, 30000)?;
    memory.read(input.address, &mut request)?;
    let error = error_text_at(&request, 344);
    anyhow::ensure!(
        exit == 0 && error.is_empty(),
        "Same-window client travel: {error}"
    );
    let mut actual_pid = 0;
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
            window as _,
            &mut actual_pid,
        )
    };
    anyhow::ensure!(actual_pid == pid, "Client window changed during travel");
    Ok(())
}

pub(crate) fn test_client_configuration(
    command: &str,
    edit: u32,
    server: u32,
    universe: i64,
    place: i64,
) -> Result<Value> {
    let args = server::arguments(command)?;
    let value = |name: &str| -> Result<String> {
        args.windows(2)
            .find(|pair| {
                pair[0]
                    .to_str()
                    .is_some_and(|v| v.eq_ignore_ascii_case(name))
            })
            .and_then(|pair| pair[1].to_str())
            .map(str::to_owned)
            .with_context(|| format!("Native client omitted {name}"))
    };
    anyhow::ensure!(
        value("-task")? == "StartClient"
            && value("-editpid")? == edit.to_string()
            && value("-parentpid")? == server.to_string()
            && value("-placeid")? == place.to_string()
            && value("-universeid")? == universe.to_string(),
        "Destination client no longer belongs to this test"
    );
    Ok(json!({"LaunchTask":"StartClient",
        "PlaceLaunchInfo":{"PlaceId":place,"UniverseId":universe},
        "PlayTestConfig":{"ServerPort":value("-port")?.parse::<u16>()?,
            "ServerName":value("-playtestsessionguid")?,
            "RbxTransportPort":value("-rbxtransportport")?.parse::<u16>()?,
            "RbxTransportToken":value("-rbxtransporttoken")?,
            "NumTestServerPlayersOnStartup":value("-numtestserverplayersuponstartup")?.parse::<u8>()?,
            "ExecuteTestService":true,"IsInStudioTestServiceMode":true,
            "PlayerEmulatorSerializedValues":value("-playeremulatorserializedvalues")?}}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scheduler_follows_relocated_captures_and_rejects_lost_ownership() {
        fn call(bytes: &mut Vec<u8>, target: i32) {
            let displacement = target - (0x1000 + bytes.len() as i32 + 5);
            bytes.push(0xe8);
            bytes.extend(displacement.to_le_bytes());
        }
        for (receiver, self_at, config, shared) in [(0x30, 0x20, 0x28, 8), (0x48, 0x28, 0x38, 16)] {
            let mut code = hex("415656574883ec784989ce4889d64c89c7").unwrap();
            code.extend([
                0x4c, 0x89, 0x74, 0x24, self_at, 0x48, 0x89, 0xfa, 0x48, 0x8d, 0x4c, 0x24, config,
            ]);
            call(&mut code, 0x4000);
            code.extend([
                0x49, 0x8d, 0x4e, receiver, 0x48, 0x8d, 0x54, 0x24, 0x50, 0x4c, 0x8d, 0x44, 0x24,
                0x68, 0x4c, 0x8d, 0x4c, 0x24, 0x20,
            ]);
            call(&mut code, 0x5000);
            code.extend([0x48, 0x8d, 0x4c, 0x24, 0x50 + shared, 0x48, 0x89, 0xf2]);
            call(&mut code, 0x6000);
            code.extend([0x48, 0x89, 0xf9]);
            call(&mut code, 0x7000);
            code.push(0xc3);
            assert_eq!(
                scheduler(&code, 0x1000).unwrap(),
                (
                    0x5000,
                    receiver as usize,
                    self_at as usize - 0x20,
                    config as usize - 0x20,
                    0x7000,
                    shared as usize
                )
            );
            // A real intervening register write must invalidate the destructor
            // receiver, rather than retain an incidental former value.
            let at = code.len() - 9;
            code.splice(at..at, [0x31, 0xff]);
            assert!(scheduler(&code, 0x1000).is_err());
        }
    }
    #[test]
    #[ignore = "requires an explicitly supplied installed Studio image"]
    fn installed_client_binding() -> Result<()> {
        let path = std::env::var("RENIUM_TELEPORT_TEST_IMAGE")?;
        println!(
            "{}",
            serde_json::to_string(&discover_binding(std::path::Path::new(&path))?)?
        );
        Ok(())
    }
    #[test]
    #[ignore = "requires the explicitly owned client and its prepared route relay"]
    fn owned_client_rejoin() -> Result<()> {
        let pid = std::env::var("RENIUM_TELEPORT_TEST_CLIENT_PID")?.parse()?;
        let runtime = std::env::var("RENIUM_TELEPORT_TEST_CLIENT_RUNTIME")?;
        rejoin_test_client(
            pid,
            &runtime,
            &[
                "ReplicatedStorage".into(),
                format!("__ReniumTeleportRoute_{runtime}"),
            ],
        )
    }
}
