//! Notify the installed Roblox loading CoreScripts through their native event.
use super::*;

const INVOKE: &str = ".?AV?$BoundYieldFuncDesc@VBindableFunction@RBX@@$$A6A?AV?$shared_ptr@$$CBUTuple@Reflection@RBX@@@std@@V34@@Z$0A@$00@Reflection@RBX@@";
const ADD: &str = ".?AV?$BoundFuncDesc@VScriptContext@RBX@@$$A6AXV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@V?$shared_ptr@VInstance@RBX@@@4@@Z$0A@$01@Reflection@RBX@@";
const READY: &str = ".?AV?$BoundFuncDesc@VReplicatedFirst@RBX@@$$A6A_NXZ$0A@$0A@@Reflection@RBX@@";
const REMOVE: &str = ".?AV?$BoundFuncDesc@VReplicatedFirst@RBX@@$$A6AXXZ$0A@$0A@@Reflection@RBX@@";

fn bindings(memory: &ProcessMemory, studio: &ModuleEntry) -> Result<[usize; 3]> {
    Ok([
        types::yield_member(memory, studio, INVOKE, "Invoke", 1)?.function,
        types::member(memory, studio, ADD, "AddCoreScriptLocal", false)?.1,
        types::member(memory, studio, REMOVE, "RemoveDefaultLoadingScreen", false)?.1,
    ])
}

pub(in super::super) fn probe(memory: &ProcessMemory, studio: &ModuleEntry) -> Result<Value> {
    Ok(json!({"loadingScript":bindings(memory, studio)?}))
}

pub(crate) fn notify_teleport_gui(
    pid: u32,
    title: &str,
    runtime: &str,
    path: &[String],
    show: bool,
) -> Result<()> {
    anyhow::ensure!(
        path == ["CoreGui", &format!("__ReniumTeleportGui_{runtime}")],
        "Invalid teleport GUI relay"
    );
    let loaded = modules(pid)?;
    let studio = loaded.first().context("Teleport client ended")?;
    let memory = ProcessMemory::open(pid)?;
    memory.authorize(NativeOperation::PlayTeleportGui, title)?;
    let layout = package_layout(&studio.path)?;
    let model = active_data_model_for_path(pid, &memory, studio, layout.data, title, Some(path))?;
    let entries = properties::resolve_path(&memory, &model, path, &[1, 1])?;
    let relay = entries.last().context("Teleport GUI relay ended")?;
    anyhow::ensure!(
        read_instance_class(&memory, relay.instance, model.layout).as_deref()
            == Some("BindableFunction"),
        "Teleport GUI relay changed"
    );
    let (fields, _, _) = owner::task_binding(&memory, studio, &model)?;
    let bindings = bindings(&memory, studio)?;
    let ready = if show {
        [
            types::member(&memory, studio, READY, "IsFinishedReplicating", false)?,
            types::member(&memory, studio, READY, "IsDefaultLoadingGuiRemoved", false)?,
        ]
    } else {
        [(0, 0); 2]
    };
    let mut request = vec![0; 488];
    put_u32(&mut request, 0, 6);
    put_u32(&mut request, 4, 488);
    for (index, value) in fields.into_iter().enumerate() {
        put_u64(&mut request, 8 + index * 8, value);
    }
    for (index, value) in [
        relay.instance,
        relay.owner,
        bindings[0],
        types::singleton(&memory, studio, dispatch::INSTANCE, None)?,
        bindings[1],
        bindings[2],
        ready[0].0,
        ready[0].1,
        ready[1].0,
        ready[1].1,
    ]
    .into_iter()
    .enumerate()
    {
        put_u64(&mut request, 136 + index * 8, value);
    }
    put_u32(&mut request, 216, u32::from(show));
    let host = std::process::id();
    put_u32(&mut request, 220, host);
    let creation = crate::app::update::process_start_identity(host)
        .context("Teleport GUI host ended")?
        .parse::<usize>()?;
    put_u64(&mut request, 224, creation);
    let helper = ensure_play_helper_loaded(pid, &memory, &loaded)?;
    let mut input = memory.allocate(request.len())?;
    memory.write(input.address, &request)?;
    let exit = input.run(helper + helper_export_rva("ReniumTeleportGui")?, 5000)?;
    memory.read(input.address, &mut request)?;
    let error = error_text_at(&request, 232);
    anyhow::ensure!(
        exit == 0 && error.is_empty(),
        "Roblox teleport GUI: {error}"
    );
    Ok(())
}
