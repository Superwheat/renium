// Included only by windows_tests.rs; never part of the shipped serializer API.
use super::*;
use sha2::{Digest, Sha256};
use std::io::Write;

const HISTORY_IMAGE: &str = "9004f0bd48a99c09ad1932cd46e21405fa35972bb5ae3a44dcb8db67c03add74";

use super::super::observation::signal as observation;

fn history_export(bytes: &[u8]) -> Result<usize> {
    let image = PeImage::parse(bytes)?;
    let optional = read_u32(bytes, 0x3c)? as usize + 24;
    let exports = image.rva_to_offset(read_u32(bytes, optional + 112)? as usize)?;
    let names = image.rva_to_offset(read_u32(bytes, exports + 32)? as usize)?;
    let ordinals = image.rva_to_offset(read_u32(bytes, exports + 36)? as usize)?;
    let functions = image.rva_to_offset(read_u32(bytes, exports + 28)? as usize)?;
    let mut matches = Vec::new();
    for index in 0..read_u32(bytes, exports + 24)? as usize {
        let name = image.rva_to_offset(read_u32(bytes, names + index * 4)? as usize)?;
        let symbol = if std::env::var("RENIUM_HISTORY_MODE").as_deref() == Ok("observe") {
            b"ReniumAttributeObserve\0".as_slice()
        } else { b"ReniumHistoryPrivate\0".as_slice() };
        if bytes.get(name..).is_some_and(|tail| tail.starts_with(symbol)) {
            let ordinal = read_u16(bytes, ordinals + index * 2)? as usize;
            matches.push(read_u32(bytes, functions + ordinal * 4)? as usize);
        }
    }
    anyhow::ensure!(matches.len() == 1, "History export missing/ambiguous");
    Ok(matches[0])
}

#[test]
#[ignore = "Owned Edit fixture: forwards native CHS callbacks to measure attribute coverage"]
#[expect(clippy::cognitive_complexity, reason = "Opt-in pinned-ABI diagnostic keeps its independent probe modes and safety checks together")]
fn native_attribute_observer_proof() -> Result<()> {
    let started = Instant::now();
    let timeout = Duration::from_secs(18);
    anyhow::ensure!(std::env::var("RENIUM_HISTORY_MODE").as_deref() == Ok("observe"), "Explicit diagnostic opt-in required");
    let pid: u32 = std::env::var("RENIUM_HISTORY_PID")?.parse()?;
    let title = "ReniumPullTrace.rbxl";
    let project = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize()?;
    let memory = ProcessMemory::open(pid)?;
    let current_modules = modules(pid)?;
    let studio = current_modules.first().context("Missing Studio module")?;
    anyhow::ensure!(studio.name.eq_ignore_ascii_case("RobloxStudioBeta.exe") && format!("{:x}", Sha256::digest(fs::read(&studio.path)?)) == HISTORY_IMAGE, "Attribute audit image changed");
    let window = capture_window(pid, title)?;
    let (data, trace, stamp) = studio_layout(&studio.path)?;
    verify_loaded_image(&memory, studio, stamp)?;
    let mut model = active_data_model(pid, &memory, studio, data, title)?;
    let layout = package_layout(&studio.path)?;
    let task_context = data_model_task_context(&memory, studio, &layout, &model)?;
    let history = model.roots.iter().find(|e| read_instance_class(&memory, e.instance, model.layout).as_deref() == Some("ChangeHistoryService")).copied().context("CHS unavailable")?;
    let targets = [vec!["ServerStorage".into(),"ReniumAttributeObserverProof".into()],
        vec!["ServerStorage".into(),"ReniumAttributeObserverProof".into(),"Child".into()]]
        .iter().map(|segments| properties::resolve_path(&memory, &model, segments, &[]).and_then(|v| v.last().copied().context("Missing fixture"))).collect::<Result<Vec<_>>>()?;
    model.roots = targets.clone();
    let case = std::env::var("RENIUM_OBSERVER_CASE").unwrap_or_else(|_| "normal".into());
    if matches!(case.as_str(), "guard-clean" | "guard-dirty" | "guard-expiry" | "guard-drop") {
        let mut guard = begin_attribute_guard(pid,title,&["ServerStorage".into()],Duration::from_secs(if case == "guard-expiry" {1} else {120}))?;
        println!("ATTRIBUTES ARMED");
        std::io::stdout().flush()?;
        std::thread::sleep(Duration::from_secs(6));
        if case == "guard-drop" {
            drop(guard);
            begin_attribute_guard(pid,title,&["ServerStorage".into()],Duration::from_secs(120))?.finish()?;
        } else {
            let result = guard.finish();
            println!("Native guard completion: {result:?}");
            anyhow::ensure!(result.is_err() == matches!(case.as_str(), "guard-dirty" | "guard-expiry"), "Unexpected guard completion");
        }
        return Ok(());
    }
    if case == "discover" {
        let descriptor = find_class_member_descriptor(&memory, history.instance, model.layout, "SetEnabled")?;
        let kind = read_rtti_type(&memory, descriptor, studio.base, studio.size).context("Missing SetEnabled type")?;
        println!("SetEnabled: {kind}");
        anyhow::ensure!(kind.contains("BoundFuncDesc") && kind.contains("ChangeHistoryService"), "SetEnabled reflection changed");
        let bytes = fs::read(&studio.path)?;
        let image = PeImage::parse(&bytes)?;
        let discovery_started = Instant::now();
        let mut candidates = Vec::new();
        for offset in (0x50..0xa0).step_by(8) {
            let address = memory.read_u64(descriptor + offset)? as usize;
            let Some(method) = address.checked_sub(studio.base) else { continue };
            if image.require_executable_rva(method).is_err() { continue }
            match observation::discover(&image,method) {
                Ok(trace) => candidates.push((offset,trace)),
                Err(error) => println!("candidate offset{offset:x} method{method:x}: {error:#}"),
            }
        }
        anyhow::ensure!(candidates.len()==1,"SetEnabled observer discovery found {} candidates",candidates.len());
        let (offset,result) = candidates.remove(0);
        anyhow::ensure!(memory.read_u32(descriptor+offset+8)?==0,"SetEnabled this-adjustment changed");
        println!("SetEnabled field{offset:x}");
        for &rva in &result.functions {
            let offset = image.rva_to_offset(rva)?;
            let (_,end) = image.function_bounds(offset)?;
            properties::verified_code(&memory,studio,&layout,studio.base+rva,end-offset)?;
        }
        println!("{result:?}, elapsed {:?}",discovery_started.elapsed());
        anyhow::ensure!(result.signal_offset==0x6e8 && result.ensure==0x7699bf0 && result.allocate==0x76985e0 && result.append==0x7699a90 && result.disconnect==0x7699280 && result.assign==0x7698d10,"Discovery disagrees with the independently audited contract");
        return Ok(());
    }
    anyhow::ensure!(matches!(case.as_str(), "normal" | "disabled" | "detached" | "independent" | "independent-disabled"), "Unknown observer case");
    let path = project.join(format!("audit/current-full-pull/attribute-observer-{case}.transport"));
    let mut transport = fs::OpenOptions::new().read(true).write(true).create_new(true).share_mode(7).custom_flags(0x04000000).open(&path)?;
    let mut params = build_parameters(studio.base, trace, &model, &path, true)?;
    params.resize(5920,0);
    for (offset,value) in [(PARAM_TASK_CONTEXT,task_context),(PARAM_SUBMIT_TASK,studio.base+layout.submit_task),
        (PARAM_WINDOW,window.0),(5904,history.instance),(5912,history.owner)] { put_u64(&mut params,offset,value); }
    put_u32(&mut params,PARAM_CAPTURE_MODE,1);
    put_u32(&mut params,PARAM_PROCESS_ID,pid);
    if case.starts_with("independent") {
        put_u32(&mut params,136,2);
        for (raw,size) in [(0x4519a00,1479),(0x7698ff0,243),(0x76979e0,605),(0x7698e90,346),(0x7698680,355),(0x7698110,70),(0x2596420,145)] {
            properties::verified_code(&memory,studio,&layout,studio.base+raw+0xc00,size)?;
        }
    }
    properties::verified_code(&memory,studio,&layout,studio.base+0x451df70,1070)?;
    let dll = project.join("audit/native-attribute-observer3.dll");
    let entry = history_export(&fs::read(&dll)?)?;
    let kernel=current_modules.iter().find(|m|m.name.eq_ignore_ascii_case("kernel32.dll")).context("Missing kernel32")?;
    let local=unsafe {GetModuleHandleW(wide("kernel32.dll").as_ptr())};
    anyhow::ensure!(!local.is_null(),"Missing local kernel32");
    let load=unsafe {GetProcAddress(local,c"LoadLibraryW".as_ptr().cast())}.context("Missing loader")?;
    let load=kernel.base+(load as usize).checked_sub(local as usize).context("Loader outside kernel32")?;
    let path_bytes=wide(dll.as_os_str()).iter().flat_map(|v|v.to_le_bytes()).collect::<Vec<_>>();
    let mut remote_path=memory.allocate(path_bytes.len())?;
    memory.write(remote_path.address,&path_bytes)?;
    remote_path.run(load,2000)?;
    let helper=modules(pid)?.into_iter().find(|m|module_path_matches(m,&dll)).context("Observer DLL not loaded")?;
    anyhow::ensure!(capture_window(pid,title)?==window,"Target changed");
    let remote=memory.allocate(params.len())?;
    memory.write(remote.address,&params)?;
    let exit=remote.run_owned(helper.base+entry,capture_remaining_ms(started,timeout)?)?;
    let mut bytes=Vec::new(); transport.read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len()>=320,"Incomplete observer result");
    anyhow::ensure!(exit==0 && read_u32(&bytes,8)?==4,"Observer failed: exit={exit}, {}",String::from_utf8_lossy(&bytes[64..320]));
    let rows=bytes[320..].chunks_exact(32).map(|r|Ok(serde_json::json!({
        "target":targets.iter().position(|t|t.instance==read_u64(r,0).unwrap() as usize),
        "descriptorRva":format!("{:x}",read_u64(r,8)? as usize-studio.base),"tick":read_u64(r,16)?,
        "enabled":read_u32(r,24)?,"playback":read_u32(r,28)?}))).collect::<Result<Vec<_>>>()?;
    let result=serde_json::json!({"events":read_u64(&bytes,16)?,"rows":rows,"connectionCleared":read_u64(&bytes,32)?==1,"restored":memory.read_u64(history.instance)? as usize==studio.base+0xa102e00});
    println!("{result}");
    fs::OpenOptions::new().write(true).create_new(true).open(project.join(format!("audit/current-full-pull/attribute-observer-{case}.json")))?.write_all(&serde_json::to_vec_pretty(&result)?)?;
    Ok(())
}

#[test]
#[ignore = "Coordinated owned Edit fixture: audit-only native history capture/move gate"]
fn native_history_scratch_proof() -> Result<()> {
    let started = Instant::now();
    let timeout = Duration::from_secs(18);
    let pid: u32 = std::env::var("RENIUM_HISTORY_PID")?.parse()?;
    let title = std::env::var("RENIUM_HISTORY_TITLE")?;
    let mode = std::env::var("RENIUM_HISTORY_MODE")?;
    anyhow::ensure!(matches!(mode.as_str(), "proof" | "timeout"), "Unknown history mode");
    anyhow::ensure!(title.starts_with("ReniumNativeHistory") && title.ends_with(".rbxl") && !title.contains(['/', '\\']), "Not an owned history title");
    let project = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize()?;
    let output = project.join(format!("audit/native-history-{}.{pid}.bin", title.trim_end_matches(".rbxl")));
    anyhow::ensure!(!output.exists(), "Do not overwrite history evidence");
    let memory = ProcessMemory::open(pid)?;
    let current_modules = modules(pid)?;
    let studio = current_modules.first().context("Missing Studio module")?;
    anyhow::ensure!(studio.name.eq_ignore_ascii_case("RobloxStudioBeta.exe") && format!("{:x}", Sha256::digest(fs::read(&studio.path)?)) == HISTORY_IMAGE, "History audit image changed");
    let window = capture_window(pid, &title)?;
    let (data, trace, stamp) = studio_layout(&studio.path)?;
    verify_loaded_image(&memory, studio, stamp)?;
    let mut model = active_data_model(pid, &memory, studio, data, &title)?;
    let layout = package_layout(&studio.path)?;
    let task_context = data_model_task_context(&memory, studio, &layout, &model)?;
    let parent_offset = properties::parent_offset(&memory, &model)?;
    let roots = model.roots.iter().map(|entry| Ok((*entry, read_instance_class(&memory, entry.instance, model.layout).context("Missing root class")?))).collect::<Result<Vec<_>>>()?;
    let service = roots.iter().find(|(_, name)| name == "SerializationService").context("Prepare SerializationService first")?.0.instance;
    let history = roots.iter().find(|(_, name)| name == "ChangeHistoryService").context("Prepare ChangeHistoryService first")?.0.instance;
    anyhow::ensure!(memory.read_u64(history + 0xd0)? as usize == model.outer, "CHS complete-object/DataModel seam invalid");
    let descriptor = find_class_member_descriptor(&memory, service, model.layout, "SerializeInstancesAsync")?;
    anyhow::ensure!(descriptor == studio.base + 0xdea0ca0, "History handoff descriptor changed");
    let method = studio.base + 0x4cc95b0;
    anyhow::ensure!(memory.read_u64(descriptor + 0x78)? as usize == method && memory.read_u32(descriptor + 0x80)? == 0, "History handoff ABI invalid");
    for name in ["native-typed-private-contract.json", "native-history-contract.json"] {
        let contract: serde_json::Value = serde_json::from_slice(&fs::read(project.join("audit").join(name))?)?;
        anyhow::ensure!(contract["sha"].as_str() == Some(HISTORY_IMAGE), "Unpinned contract");
        for check in contract["checks"].as_array().context("Missing code checks")? {
            properties::verified_code(&memory, studio, &layout, studio.base + check["rva"].as_u64().context("Missing RVA")? as usize, check["size"].as_u64().context("Missing size")? as usize)?;
        }
    }
    properties::verified_code(&memory, studio, &layout, studio.base + layout.submit_task, 64)?;
    let name = memory.read_u64(service + model.layout.name)? as usize;
    let name_text = [0, 8].into_iter().find(|offset| read_msvc_string(&memory, name + offset).as_deref() == Some("SerializationService")).context("Unknown Instance name layout")?;
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).map_err(|error| anyhow::anyhow!("History nonce: {error}"))?;
    let secret = std::env::var("RENIUM_HISTORY_SECRET").unwrap_or_else(|_| secret.iter().map(|byte| format!("{byte:02x}")).collect::<String>());
    anyhow::ensure!(secret.len() == 64 && secret.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()), "Invalid history audit secret");
    let nonce = format!("renium-history-proof:{secret}");
    let path = project.join(format!("audit/native-history-transport-{}.tmp", &nonce[21..]));
    let mut transport = fs::OpenOptions::new().read(true).write(true).create_new(true).share_mode(7).custom_flags(0x04000000).open(&path)?;
    let full = std::env::var("RENIUM_HISTORY_FULL").as_deref() == Ok("1");
    let names = if full { ["Lighting", "LocalizationService", "MaterialService", "Players", "ReplicatedFirst", "ReplicatedStorage", "ServerScriptService", "ServerStorage", "SoundService", "StarterGui", "StarterPack", "StarterPlayer", "Teams", "TestService", "TextChatService", "VoiceChatService", "VRService", "Workspace"].iter().map(|n| (*n).into()).collect::<Vec<_>>() } else { vec!["ServerStorage".into()] };
    model.roots = select_capture_roots(&roots, &names)?;
    let mut params = build_parameters(studio.base, trace, &model, &path, true)?;
    params.resize(6040, 0);
    let suppress = std::env::var("RENIUM_HISTORY_SUPPRESS").as_deref() == Ok("1");
    put_u32(&mut params, 5916, if full { if suppress { 4 } else { 3 } } else if suppress { 2 } else { 0 });
    for (offset, value) in [(PARAM_TASK_CONTEXT, task_context), (PARAM_SUBMIT_TASK, studio.base + layout.submit_task), (PARAM_WINDOW, window.0), (5904, history), (5920, descriptor), (5928, service)] {
        put_u64(&mut params, offset, value);
    }
    for (offset, value) in [(PARAM_CAPTURE_MODE, 1), (PARAM_REQUESTED_MXCSR, 0), (PARAM_PROCESS_ID, pid), (PARAM_PARENT_OFFSET, u32::try_from(parent_offset)?), (5912, u32::try_from(model.layout.class_descriptor)?), (5936, u32::try_from(model.layout.name)?), (5940, name_text as u32)] {
        put_u32(&mut params, offset, value);
    }
    params[5944..5944 + nonce.len()].copy_from_slice(nonce.as_bytes());
    // All loaded-image and complete-object contracts precede loading the DLL.
    let dll = project.join("audit/native-history-probe.dll");
    let entry_rva = history_export(&fs::read(&dll)?)?;
    let kernel = current_modules.iter().find(|m| m.name.eq_ignore_ascii_case("kernel32.dll")).context("Missing kernel32")?;
    let local = unsafe { GetModuleHandleW(wide("kernel32.dll").as_ptr()) };
    anyhow::ensure!(!local.is_null(), "Missing local kernel32");
    let load = unsafe { GetProcAddress(local, c"LoadLibraryW".as_ptr().cast()) }.context("Missing loader")?;
    let load = kernel.base + (load as usize).checked_sub(local as usize).context("Loader outside kernel32")?;
    let path_bytes = wide(dll.as_os_str()).iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>();
    let mut remote_path = memory.allocate(path_bytes.len())?;
    memory.write(remote_path.address, &path_bytes)?;
    remote_path.run(load, capture_remaining_ms(started, timeout)?.min(2000))?;
    let helper = modules(pid)?.into_iter().find(|m| module_path_matches(m, &dll)).context("History DLL not loaded")?;
    anyhow::ensure!(capture_window(pid, &title)? == window, "History target changed");
    put_u32(&mut params, PARAM_TIMEOUT, capture_remaining_ms(started, timeout)?.min(15000));
    let remote = memory.allocate(params.len())?;
    memory.write(remote.address, &params)?;
    fs::OpenOptions::new().write(true).create_new(true).open(output.with_extension("ready.json"))?.write_all(&serde_json::to_vec(&serde_json::json!({"pid":pid,"title":title,"nonce":nonce,"transport":path,"output":output,"mode":mode}))?)?;
    let exit = remote.run_owned(helper.base + entry_rva, capture_remaining_ms(started, timeout)?)?;
    let mut bytes = Vec::new();
    anyhow::ensure!(transport.metadata()?.len() == 320, "Invalid history transport length");
    transport.read_to_end(&mut bytes)?;
    fs::OpenOptions::new().write(true).create_new(true).open(&output)?.write_all(&bytes)?;
    anyhow::ensure!(read_u32(&bytes, 0)? == 0x50414352 && read_u32(&bytes, 4)? == 1, "Invalid history response header");
    let expected = if mode == "timeout" { 6 } else { 4 };
    anyhow::ensure!(read_u32(&bytes, 8)? == expected && exit == if expected == 4 { 0 } else { 6 }, "History native failure status={} exit={exit}: {}", read_u32(&bytes,8)?, String::from_utf8_lossy(&bytes[64..320]));
    anyhow::ensure!(memory.read_u64(descriptor + 0x78)? as usize == method && capture_window(pid, &title)? == window, "History method/window not restored");
    eprintln!("{}", serde_json::json!({"status":expected,"restored":true,"totalMs":started.elapsed().as_secs_f64()*1000.0,"image":HISTORY_IMAGE}));
    Ok(())
}
