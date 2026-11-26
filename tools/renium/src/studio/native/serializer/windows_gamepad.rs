//! Virtual gamepad control through a Play client's reflected VirtualInputManager.
use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::studio::native::serializer::functions as native_functions;

const CONNECT_RTTI: &str =
    ".?AV?$BoundFuncDesc@VVirtualInputManager@RBX@@$$A6AXH@Z$0A@$00@Reflection@RBX@@";
const BUTTON_RTTI: &str =
    ".?AV?$BoundFuncDesc@VVirtualInputManager@RBX@@$$A6AXHW4KeyCode@2@H@Z$0A@$02@Reflection@RBX@@";
const AXIS_RTTI: &str = ".?AV?$BoundFuncDesc@VVirtualInputManager@RBX@@$$A6AXHW4KeyCode@2@MMM@Z$0A@$04@Reflection@RBX@@";

// UserInputService maps deviceId -> UserInputType and assigns the first free
// Gamepad1..Gamepad8 slot; 12345 sits outside Windows XInput's 0..3 ids.
pub(crate) const VIRTUAL_DEVICE_ID: i32 = 12345;
const HELPER_TIMEOUT_MS: u32 = 3000;
const RELEASE_TIMEOUT_MS: u32 = 1000;
const CLEANUP_TIMEOUT_MS: u32 = 400;

#[derive(Clone, Debug, Default)]
pub(crate) struct GamepadInitialState {
    pub(crate) player_name: String,
    pub(crate) nonce: String,
    pub(crate) existing_slot: Option<String>,
    pub(crate) held_buttons: Vec<(&'static str, i32)>,
    pub(crate) sticks: Vec<(&'static str, i32, f32, f32)>,
    pub(crate) triggers: Vec<(&'static str, i32, f32)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GamepadSessionReady {
    Connected(String),
    NeedsSlotDetection,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum GamepadAction {
    Button {
        name: &'static str,
        key_code: i32,
        pressed: bool,
    },
    Stick {
        name: &'static str,
        key_code: i32,
        x: f32,
        y: f32,
    },
    Trigger {
        name: &'static str,
        key_code: i32,
        value: f32,
    },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct GamepadStateReport {
    pub(crate) slot: String,
    pub(crate) held_buttons: Vec<&'static str>,
    pub(crate) sticks: BTreeMap<&'static str, [f32; 2]>,
    pub(crate) triggers: BTreeMap<&'static str, f32>,
}

#[derive(Clone, Copy)]
struct ReflectedBinding {
    descriptor: usize,
    table: usize,
    field: usize,
    function: usize,
}

#[derive(Clone, Copy)]
struct GamepadBindings {
    connect: ReflectedBinding,
    disconnect: ReflectedBinding,
    button: ReflectedBinding,
    axis: ReflectedBinding,
}

struct PlayClientLookup<'a> {
    player_name: &'a str,
    nonce: Option<&'a str>,
    hint_outer: Option<usize>,
}

struct GamepadSession {
    pid: u32,
    outer: usize,
    owner: usize,
    layout: InstanceLayout,
    vim: SharedEntry,
    parameters: Vec<u8>,
    entrypoint: usize,
    task_abi: u32,
    bindings: GamepadBindings,
    connected: bool,
    slot: Option<String>,
    held_buttons: BTreeMap<&'static str, i32>,
    sticks: BTreeMap<&'static str, (i32, f32, f32)>,
    triggers: BTreeMap<&'static str, (i32, f32)>,
}

impl GamepadSession {
    fn state_report(&self) -> GamepadStateReport {
        GamepadStateReport {
            slot: self.slot.clone().unwrap_or_default(),
            held_buttons: self.held_buttons.keys().copied().collect(),
            sticks: self
                .sticks
                .iter()
                .map(|(&name, &(_, x, y))| (name, [x, y]))
                .collect(),
            triggers: self
                .triggers
                .iter()
                .map(|(&name, &(_, value))| (name, value))
                .collect(),
        }
    }

    fn auth_key(&self) -> String {
        format!(
            "cached:{}:{:x}:{:x}",
            self.task_abi, self.outer, self.vim.instance
        )
    }
}

enum SessionSlot {
    Uninitialized,
    Active(Box<GamepadSession>),
    Closed,
}

static GAMEPAD_SESSIONS: OnceLock<Mutex<HashMap<String, Arc<Mutex<SessionSlot>>>>> =
    OnceLock::new();

fn sessions() -> &'static Mutex<HashMap<String, Arc<Mutex<SessionSlot>>>> {
    GAMEPAD_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn slot_for_runtime(runtime_id: &str) -> Arc<Mutex<SessionSlot>> {
    let mut map = sessions().lock_recover();
    Arc::clone(
        map.entry(runtime_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(SessionSlot::Uninitialized))),
    )
}

pub(crate) fn prune_gamepad_sessions(active_runtime_ids: &[String]) {
    let active: HashSet<&str> = active_runtime_ids.iter().map(String::as_str).collect();
    let mut map = sessions().lock_recover();
    map.retain(|runtime_id, slot| {
        if !active.contains(runtime_id.as_str()) {
            return false;
        }
        match slot.try_lock() {
            Ok(guard) => match &*guard {
                SessionSlot::Closed => false,
                SessionSlot::Active(session) => crate::daemon::is_process_alive(session.pid),
                SessionSlot::Uninitialized => true,
            },
            Err(_) => true,
        }
    });
}

pub(crate) fn has_live_gamepad_session(pid: u32, runtime_id: &str) -> Option<String> {
    let slot = {
        let map = sessions().lock_recover();
        map.get(runtime_id).cloned()
    }?;
    let guard = slot.lock_recover();
    let SessionSlot::Active(session) = &*guard else {
        return None;
    };
    let slot_name = session.slot.as_ref()?;
    if session.pid != pid || !session.connected {
        return None;
    }
    let memory = ProcessMemory::open_read_only(pid).ok()?;
    session_is_live(&memory, session).then(|| slot_name.clone())
}

pub(crate) fn connect_gamepad_session(
    pid: u32,
    runtime_id: &str,
    init: &GamepadInitialState,
) -> Result<GamepadSessionReady> {
    let slot = slot_for_runtime(runtime_id);
    let mut guard = slot.lock_recover();
    if matches!(*guard, SessionSlot::Closed) {
        bail!("Play client session is closing");
    }

    let memory = ProcessMemory::open(pid)?;
    if let SessionSlot::Active(session) = &*guard
        && session.pid == pid
        && session.connected
        && session_is_live(&memory, session)
        && let Some(slot_name) = session.slot.clone()
    {
        memory.authorize(NativeOperation::Gamepad, &session.auth_key())?;
        return Ok(GamepadSessionReady::Connected(slot_name));
    }

    let auth_key = format!("player:{}:{}", init.player_name, init.nonce);
    memory.authorize(NativeOperation::Gamepad, &auth_key)?;
    let mut session = discover_session(&memory, pid, init)?;
    let connect_binding = session.bindings.connect;
    let input = native_functions::gamepad_connection_input(true, VIRTUAL_DEVICE_ID);
    invoke_reflected(
        &memory,
        &mut session,
        connect_binding,
        input,
        HELPER_TIMEOUT_MS,
    )
    .context("Failed to connect virtual gamepad")?;
    session.connected = true;
    let ready = match session.slot.clone() {
        Some(slot_name) => GamepadSessionReady::Connected(slot_name),
        None => GamepadSessionReady::NeedsSlotDetection,
    };
    *guard = SessionSlot::Active(Box::new(session));
    Ok(ready)
}

pub(crate) fn set_gamepad_slot(pid: u32, runtime_id: &str, slot_name: String) {
    let slot = {
        let map = sessions().lock_recover();
        map.get(runtime_id).cloned()
    };
    let Some(slot) = slot else {
        return;
    };
    let mut guard = slot.lock_recover();
    if let SessionSlot::Active(session) = &mut *guard
        && session.pid == pid
    {
        session.slot = Some(slot_name);
    }
}

pub(crate) fn set_gamepad_connected(pid: u32, runtime_id: &str, connected: bool) -> Result<()> {
    let slot = {
        let map = sessions().lock_recover();
        map.get(runtime_id).cloned()
    }
    .context("Virtual gamepad session is not active")?;
    let mut guard = slot.lock_recover();
    let SessionSlot::Active(session) = &mut *guard else {
        bail!("Virtual gamepad session is not active");
    };
    anyhow::ensure!(session.pid == pid, "Virtual gamepad session PID changed");
    let memory = ProcessMemory::open(pid)?;
    anyhow::ensure!(
        session_is_live(&memory, session),
        "Virtual gamepad session expired before reconnecting"
    );
    memory.authorize(NativeOperation::Gamepad, &session.auth_key())?;
    let binding = if connected {
        session.bindings.connect
    } else {
        session.bindings.disconnect
    };
    let input = native_functions::gamepad_connection_input(connected, VIRTUAL_DEVICE_ID);
    invoke_reflected(&memory, session, binding, input, HELPER_TIMEOUT_MS)?;
    session.connected = connected;
    if !connected {
        session.slot = None;
        session.held_buttons.clear();
        session.sticks.clear();
        session.triggers.clear();
    }
    Ok(())
}

pub(crate) fn execute_gamepad_actions(
    pid: u32,
    runtime_id: &str,
    actions: &[(usize, GamepadAction)],
) -> std::result::Result<GamepadStateReport, (usize, anyhow::Error)> {
    let first_step = actions.first().map_or(0, |&(step, _)| step);
    let slot = slot_for_runtime(runtime_id);
    let mut guard = slot.lock_recover();
    if matches!(*guard, SessionSlot::Closed) {
        return Err((
            first_step,
            anyhow::anyhow!("Play client session is closing"),
        ));
    }
    let SessionSlot::Active(session) = &mut *guard else {
        return Err((
            first_step,
            anyhow::anyhow!("Virtual gamepad session is not connected; retry once Play settles"),
        ));
    };
    let memory = ProcessMemory::open(pid).map_err(|err| (first_step, err))?;
    if session.pid != pid || !session.connected || !session_is_live(&memory, session) {
        return Err((
            first_step,
            anyhow::anyhow!(
                "Virtual gamepad session expired before the action ran; retry once Play settles"
            ),
        ));
    }
    memory
        .authorize(NativeOperation::Gamepad, &session.auth_key())
        .map_err(|err| (first_step, err))?;

    for &(step_index, action) in actions {
        apply_gamepad_action(&memory, session, action).map_err(|err| (step_index, err))?;
    }
    Ok(session.state_report())
}

fn apply_gamepad_action(
    memory: &ProcessMemory,
    session: &mut GamepadSession,
    action: GamepadAction,
) -> Result<()> {
    let bindings = session.bindings;
    match action {
        GamepadAction::Button {
            name,
            key_code,
            pressed,
        } => {
            let input =
                native_functions::gamepad_button_input(VIRTUAL_DEVICE_ID, key_code, pressed);
            invoke_reflected(memory, session, bindings.button, input, HELPER_TIMEOUT_MS)?;
            if pressed {
                session.held_buttons.insert(name, key_code);
            } else {
                session.held_buttons.remove(name);
            }
        }
        GamepadAction::Stick {
            name,
            key_code,
            x,
            y,
        } => {
            let input =
                native_functions::gamepad_axis_input(VIRTUAL_DEVICE_ID, key_code, x, y, 0.0);
            invoke_reflected(memory, session, bindings.axis, input, HELPER_TIMEOUT_MS)?;
            if x == 0.0 && y == 0.0 {
                session.sticks.remove(name);
            } else {
                session.sticks.insert(name, (key_code, x, y));
            }
        }
        GamepadAction::Trigger {
            name,
            key_code,
            value,
        } => {
            let input =
                native_functions::gamepad_axis_input(VIRTUAL_DEVICE_ID, key_code, 0.0, 0.0, value);
            invoke_reflected(memory, session, bindings.axis, input, HELPER_TIMEOUT_MS)?;
            if value == 0.0 {
                session.triggers.remove(name);
            } else {
                session.triggers.insert(name, (key_code, value));
            }
        }
    }
    Ok(())
}

pub(crate) fn release_gamepad_inputs(pid: u32, runtime_id: &str) -> bool {
    let slot = {
        let map = sessions().lock_recover();
        map.get(runtime_id).cloned()
    };
    let Some(slot) = slot else {
        return false;
    };
    let mut guard = slot.lock_recover();
    let SessionSlot::Active(session) = &mut *guard else {
        return false;
    };
    if session.pid != pid || !session.connected {
        return false;
    }
    if session.held_buttons.is_empty() && session.sticks.is_empty() && session.triggers.is_empty() {
        return true;
    }
    let Ok(memory) = ProcessMemory::open(pid) else {
        return true;
    };
    if !session_is_live(&memory, session) {
        return false;
    }
    if memory
        .authorize(NativeOperation::Gamepad, &session.auth_key())
        .is_err()
    {
        return true;
    }
    release_tracked_inputs(&memory, session);
    true
}

fn release_tracked_inputs(memory: &ProcessMemory, session: &mut GamepadSession) {
    let bindings = session.bindings;
    let buttons: Vec<(&'static str, i32)> = session
        .held_buttons
        .iter()
        .map(|(&name, &code)| (name, code))
        .collect();
    for (name, key_code) in buttons {
        let input = native_functions::gamepad_button_input(VIRTUAL_DEVICE_ID, key_code, false);
        if invoke_reflected(memory, session, bindings.button, input, RELEASE_TIMEOUT_MS).is_err() {
            return;
        }
        session.held_buttons.remove(name);
    }
    let sticks: Vec<(&'static str, i32)> = session
        .sticks
        .iter()
        .map(|(&name, &(code, _, _))| (name, code))
        .collect();
    for (name, key_code) in sticks {
        let input =
            native_functions::gamepad_axis_input(VIRTUAL_DEVICE_ID, key_code, 0.0, 0.0, 0.0);
        if invoke_reflected(memory, session, bindings.axis, input, RELEASE_TIMEOUT_MS).is_err() {
            return;
        }
        session.sticks.remove(name);
    }
    let triggers: Vec<(&'static str, i32)> = session
        .triggers
        .iter()
        .map(|(&name, &(code, _))| (name, code))
        .collect();
    for (name, key_code) in triggers {
        let input =
            native_functions::gamepad_axis_input(VIRTUAL_DEVICE_ID, key_code, 0.0, 0.0, 0.0);
        if invoke_reflected(memory, session, bindings.axis, input, RELEASE_TIMEOUT_MS).is_err() {
            return;
        }
        session.triggers.remove(name);
    }
}

pub(crate) fn disconnect_gamepad_runtimes(runtime_ids: &[String]) {
    let slots: Vec<Arc<Mutex<SessionSlot>>> = {
        let mut map = sessions().lock_recover();
        runtime_ids
            .iter()
            .filter_map(|runtime_id| map.remove(runtime_id))
            .collect()
    };
    for slot in slots {
        let mut guard = slot.lock_recover();
        let previous = std::mem::replace(&mut *guard, SessionSlot::Closed);
        if let SessionSlot::Active(mut session) = previous {
            cleanup_session(&mut session);
        }
    }
}

fn cleanup_session(session: &mut GamepadSession) {
    if !session.connected {
        return;
    }
    let Ok(memory) = ProcessMemory::open(session.pid) else {
        return;
    };
    if !session_is_live(&memory, session)
        || memory
            .authorize(NativeOperation::Gamepad, &session.auth_key())
            .is_err()
    {
        return;
    }
    let disconnect_binding = session.bindings.disconnect;
    let input = native_functions::gamepad_connection_input(false, VIRTUAL_DEVICE_ID);
    let _ = invoke_reflected(
        &memory,
        session,
        disconnect_binding,
        input,
        CLEANUP_TIMEOUT_MS,
    );
    session.connected = false;
    session.held_buttons.clear();
    session.sticks.clear();
    session.triggers.clear();
}

pub(super) fn probe(
    memory: &ProcessMemory,
    pid: u32,
    title: &str,
    loaded: &[ModuleEntry],
) -> Result<Value> {
    let studio = loaded
        .iter()
        .find(|module| module.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"))
        .context("Roblox Studio module was not found")?;
    let layout = package_layout(&studio.path)?;
    verify_loaded_image(memory, studio, layout.image_stamp)?;

    if let Some(rest) = title.strip_prefix("cached:") {
        return probe_cached(memory, studio, &layout, rest);
    }

    let model = if let Some(rest) = title.strip_prefix("player:") {
        let (player_name, nonce) = match rest.split_once(':') {
            Some((name, nonce)) if !nonce.is_empty() => (name, Some(nonce)),
            _ => (rest, None),
        };
        let lookup = PlayClientLookup {
            player_name,
            nonce,
            hint_outer: None,
        };
        find_play_client_data_model(memory, studio, &layout, &lookup)?
    } else {
        active_data_model(pid, memory, studio, layout.data, title)?
    };
    let vim = find_virtual_input_manager(memory, &model)
        .context("VirtualInputManager is not initialized in this DataModel")?;
    let bindings = resolve_gamepad_bindings(memory, studio, &layout, &model, vim.instance)?;
    let task_context = data_model_task_context(memory, studio, &layout, &model)?;
    properties::verified_code(
        memory,
        studio,
        &layout,
        studio.base + layout.submit_task,
        64,
    )?;
    let task_abi = task_abi::resolve(memory, studio, &layout, task_context)? as u32;
    Ok(json!({
        "imageStamp": layout.image_stamp,
        "dataModel": model.outer,
        "virtualInputManager": vim.instance,
        "taskContext": task_context,
        "submitTaskRva": layout.submit_task,
        "taskContinuationAbi": task_abi,
        "bindings": {
            "connect": bindings.connect.function.wrapping_sub(studio.base),
            "disconnect": bindings.disconnect.function.wrapping_sub(studio.base),
            "button": bindings.button.function.wrapping_sub(studio.base),
            "axis": bindings.axis.function.wrapping_sub(studio.base),
        },
    }))
}

fn probe_cached(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    rest: &str,
) -> Result<Value> {
    let mut parts = rest.split(':');
    let abi: u32 = parts
        .next()
        .and_then(|value| value.parse().ok())
        .context("Invalid cached gamepad probe key")?;
    let outer = parts
        .next()
        .and_then(|value| usize::from_str_radix(value, 16).ok())
        .context("Invalid cached gamepad DataModel address")?;
    let vim = parts
        .next()
        .and_then(|value| usize::from_str_radix(value, 16).ok())
        .context("Invalid cached gamepad VirtualInputManager address")?;
    anyhow::ensure!(
        matches!(abi, 1 | 2)
            && read_rtti_type(memory, outer, studio.base, studio.size).as_deref()
                == Some(".?AVDataModel@RBX@@"),
        "Cached Play client DataModel is no longer live"
    );
    Ok(json!({
        "imageStamp": layout.image_stamp,
        "dataModel": outer,
        "virtualInputManager": vim,
        "taskContinuationAbi": abi,
        "cached": true,
    }))
}

fn session_is_live(memory: &ProcessMemory, session: &GamepadSession) -> bool {
    let Ok(loaded) = modules(session.pid) else {
        return false;
    };
    let Some(studio) = loaded
        .iter()
        .find(|module| module.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"))
    else {
        return false;
    };
    if read_rtti_type(memory, session.outer, studio.base, studio.size).as_deref()
        != Some(".?AVDataModel@RBX@@")
    {
        return false;
    }
    let Some(dm_instance) = session
        .outer
        .checked_add(session.layout.data_model_instance)
    else {
        return false;
    };
    if memory
        .read_u64(dm_instance + session.layout.self_pointer)
        .ok()
        .map(|value| value as usize)
        != Some(dm_instance)
        || memory
            .read_u64(dm_instance + session.layout.self_pointer + 8)
            .ok()
            .map(|value| value as usize)
            != Some(session.owner)
        || !valid_owner(memory, session.owner, studio.base, studio.size)
    {
        return false;
    }
    memory
        .read_u64(session.vim.instance + session.layout.self_pointer)
        .ok()
        .map(|value| value as usize)
        == Some(session.vim.instance)
        && memory
            .read_u64(session.vim.instance + session.layout.self_pointer + 8)
            .ok()
            .map(|value| value as usize)
            == Some(session.vim.owner)
        && valid_owner(memory, session.vim.owner, studio.base, studio.size)
}

fn cached_probe_data_model(pid: u32) -> Option<usize> {
    super::super::probes::cached(pid)
        .get("operations")
        .and_then(Value::as_array)
        .and_then(|ops| {
            ops.iter()
                .find(|entry| entry.get("operation").and_then(Value::as_str) == Some("gamepad"))
        })
        .and_then(|entry| entry.get("readings"))
        .and_then(|readings| readings.get("dataModel"))
        .and_then(Value::as_u64)
        .map(|addr| addr as usize)
}

fn discover_session(
    memory: &ProcessMemory,
    pid: u32,
    init: &GamepadInitialState,
) -> Result<GamepadSession> {
    let loaded = modules(pid)?;
    let studio = loaded
        .iter()
        .find(|module| module.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"))
        .context("Roblox Studio module was not found in the Play client process")?;
    let layout = package_layout(&studio.path)?;
    verify_loaded_image(memory, studio, layout.image_stamp)?;

    let lookup = PlayClientLookup {
        player_name: &init.player_name,
        nonce: (!init.nonce.is_empty()).then_some(init.nonce.as_str()),
        hint_outer: cached_probe_data_model(pid),
    };
    let model = find_play_client_data_model(memory, studio, &layout, &lookup)?;
    let vim = find_virtual_input_manager(memory, &model)?;
    let bindings = resolve_gamepad_bindings(memory, studio, &layout, &model, vim.instance)?;
    let task_context = data_model_task_context(memory, studio, &layout, &model)?;
    let task_abi = memory
        .task_abi
        .get()
        .context("Native gamepad task continuation ABI was not authorized")?;

    let helper = ensure_helper_loaded_with_timeout(pid, memory, &loaded, HELPER_TIMEOUT_MS)?;
    let entrypoint = helper + helper_export_rva("ReniumReadProperty")?;
    let parameters = build_helper_parameters(
        memory,
        studio,
        &layout,
        &model,
        vim,
        task_context,
        entrypoint,
    )?;

    let held_buttons = init.held_buttons.iter().copied().collect();
    let sticks = init
        .sticks
        .iter()
        .map(|&(name, code, x, y)| (name, (code, x, y)))
        .collect();
    let triggers = init
        .triggers
        .iter()
        .map(|&(name, code, value)| (name, (code, value)))
        .collect();

    Ok(GamepadSession {
        pid,
        outer: model.outer,
        owner: model.owner,
        layout: model.layout,
        vim,
        parameters,
        entrypoint,
        task_abi,
        bindings,
        connected: false,
        slot: init.existing_slot.clone(),
        held_buttons,
        sticks,
        triggers,
    })
}

fn build_helper_parameters(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    model: &ActiveDataModel,
    vim: SharedEntry,
    task_context: usize,
    entrypoint: usize,
) -> Result<Vec<u8>> {
    let parent_offset = properties::parent_offset(memory, model)?;
    let name_descriptor = find_class_member_descriptor(memory, vim.instance, model.layout, "Name")?;
    let name_table = memory.read_u64(name_descriptor)? as usize;
    let (name_getter, name_setter) =
        properties::reflection_functions(memory, studio, layout, name_descriptor)?;
    let (identity_binding, identity_getter) =
        properties::identity_binding(memory, studio, layout, model, vim.instance)?;
    let vim_class = memory.read_u64(vim.instance + model.layout.class_descriptor)? as usize;
    let dm_instance = model
        .outer
        .checked_add(model.layout.data_model_instance)
        .context("Play client DataModel instance address overflowed")?;

    let mut parameters = vec![0u8; properties::SIZE];
    for (offset, value) in [
        (0, task_context),
        (8, studio.base + layout.submit_task),
        (16, vim.instance),
        (24, vim.owner),
        (32, model.owner),
        (40, name_descriptor),
        (48, name_getter),
        (56, vim_class),
        (64, name_table),
        (72, model.layout.class_descriptor),
        (65888, identity_binding),
        (65896, identity_getter),
        (65904, name_setter),
        (131488, parent_offset),
        (131504, vim.instance),
        (131512, dm_instance),
        (132024, model.layout.self_pointer),
    ] {
        put_u64(&mut parameters, offset, value);
    }
    put_u32(&mut parameters, 131496, 2);

    invoke_helper(memory, entrypoint, &mut parameters, 0, HELPER_TIMEOUT_MS)
        .context("Failed to capture VirtualInputManager identity")?;
    let identity: [u8; 16] =
        parameters[properties::IDENTITY..properties::IDENTITY + 16].try_into()?;
    parameters[properties::EXPECTED_IDENTITY..properties::EXPECTED_IDENTITY + 16]
        .copy_from_slice(&identity);
    Ok(parameters)
}

fn find_virtual_input_manager(
    memory: &ProcessMemory,
    model: &ActiveDataModel,
) -> Result<SharedEntry> {
    let matches = model
        .roots
        .iter()
        .copied()
        .filter(|root| {
            read_instance_class(memory, root.instance, model.layout).as_deref()
                == Some("VirtualInputManager")
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        matches.len() == 1,
        "Play client DataModel has {} VirtualInputManager service roots (expected 1)",
        matches.len()
    );
    Ok(matches[0])
}

fn find_class_member_descriptors(
    memory: &ProcessMemory,
    instance: usize,
    layout: InstanceLayout,
    wanted: &[&'static str],
) -> Result<HashMap<&'static str, usize>> {
    let class_descriptor = memory.read_u64(instance + layout.class_descriptor)? as usize;
    let class_bytes = memory.read_vec(class_descriptor, 0x218)?;
    let mut matches: HashMap<&'static str, HashSet<usize>> = HashMap::new();
    for offset in (0..=0x200usize).step_by(8) {
        let entries = read_u64(&class_bytes, offset)? as usize;
        let count = read_u64(&class_bytes, offset + 8)? as usize;
        let capacity = read_u64(&class_bytes, offset + 16)? as usize;
        if count == 0
            || count > 512
            || capacity < count
            || capacity > 1024
            || !likely_pointer(entries)
        {
            continue;
        }
        let Ok(members) = memory.read_vec(entries, count * 16) else {
            continue;
        };
        for index in 0..count {
            let member = read_u64(&members, index * 16)? as usize;
            if !likely_pointer(member) {
                continue;
            }
            let Ok(name_object) = memory.read_u64(member + 8).map(|value| value as usize) else {
                continue;
            };
            let Some(name) = read_msvc_string(memory, name_object) else {
                continue;
            };
            for &target in wanted {
                if name == target {
                    matches.entry(target).or_default().insert(member);
                }
            }
        }
    }
    let mut resolved = HashMap::with_capacity(wanted.len());
    for &target in wanted {
        let set = matches.remove(target).unwrap_or_default();
        anyhow::ensure!(
            set.len() == 1,
            "Virtual gamepad is not supported on this Studio version: VirtualInputManager.{target} resolved to {} descriptors",
            set.len()
        );
        resolved.insert(target, set.into_iter().next().unwrap());
    }
    Ok(resolved)
}

fn resolve_gamepad_bindings(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    model: &ActiveDataModel,
    vim_instance: usize,
) -> Result<GamepadBindings> {
    let descriptors = find_class_member_descriptors(
        memory,
        vim_instance,
        model.layout,
        &[
            "HandleGamepadConnect",
            "HandleGamepadDisconnect",
            "HandleGamepadButtonInput",
            "HandleGamepadAxisInput",
        ],
    )?;
    Ok(GamepadBindings {
        connect: verify_member_binding(
            memory,
            studio,
            layout,
            descriptors["HandleGamepadConnect"],
            "HandleGamepadConnect",
            CONNECT_RTTI,
        )?,
        disconnect: verify_member_binding(
            memory,
            studio,
            layout,
            descriptors["HandleGamepadDisconnect"],
            "HandleGamepadDisconnect",
            CONNECT_RTTI,
        )?,
        button: verify_member_binding(
            memory,
            studio,
            layout,
            descriptors["HandleGamepadButtonInput"],
            "HandleGamepadButtonInput",
            BUTTON_RTTI,
        )?,
        axis: verify_member_binding(
            memory,
            studio,
            layout,
            descriptors["HandleGamepadAxisInput"],
            "HandleGamepadAxisInput",
            AXIS_RTTI,
        )?,
    })
}

fn verify_member_binding(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    layout: &PackageLayout,
    descriptor: usize,
    member_name: &str,
    expected_rtti: &str,
) -> Result<ReflectedBinding> {
    let signature = read_rtti_type(memory, descriptor, studio.base, studio.size).with_context(
        || {
            format!(
                "Virtual gamepad is not supported on this Studio version: VirtualInputManager.{member_name} has no RTTI"
            )
        },
    )?;
    anyhow::ensure!(
        signature == expected_rtti,
        "Virtual gamepad is not supported on this Studio version: VirtualInputManager.{member_name} signature mismatch ({signature})"
    );
    let table = memory.read_u64(descriptor)? as usize;
    let locator = memory.read_u64(table - 8)? as usize;
    anyhow::ensure!(
        memory.read_u32(locator + 4)? == 0
            && studio
                .base
                .checked_add(memory.read_u32(locator + 20)? as usize)
                == Some(locator),
        "Virtual gamepad is not supported on this Studio version: VirtualInputManager.{member_name} is not a complete reflection descriptor"
    );
    let (field, function) =
        properties::function_binding(memory, studio, layout, descriptor, table).with_context(
            || {
                format!(
                    "Virtual gamepad is not supported on this Studio version: VirtualInputManager.{member_name} dispatch binding could not be verified"
                )
            },
        )?;
    Ok(ReflectedBinding {
        descriptor,
        table,
        field,
        function,
    })
}

fn invoke_reflected(
    memory: &ProcessMemory,
    session: &mut GamepadSession,
    binding: ReflectedBinding,
    mut input: native_functions::FunctionInput,
    timeout_ms: u32,
) -> Result<()> {
    input.bytes[..8].copy_from_slice(&(binding.descriptor as u64).to_le_bytes());
    input.bytes[8..16].copy_from_slice(&(binding.table as u64).to_le_bytes());
    input.bytes[16..20].copy_from_slice(&(binding.field as u32).to_le_bytes());
    input.bytes[24..32].copy_from_slice(&(binding.function as u64).to_le_bytes());
    put_u32(&mut session.parameters, 65916, input.bytes.len() as u32);
    session.parameters[properties::INPUT..properties::INPUT + input.bytes.len()]
        .copy_from_slice(&input.bytes);
    invoke_helper(
        memory,
        session.entrypoint,
        &mut session.parameters,
        9,
        timeout_ms,
    )
}

fn invoke_helper(
    memory: &ProcessMemory,
    entrypoint: usize,
    parameters: &mut [u8],
    operation: u32,
    timeout_ms: u32,
) -> Result<()> {
    put_u32(parameters, 80, timeout_ms);
    put_u32(parameters, 84, 0);
    put_u32(parameters, properties::EXCEPTION_CODE, 0);
    put_u32(parameters, 65912, operation);
    let mut remote = memory.allocate(parameters.len())?;
    memory.write(remote.address, parameters)?;
    let exit = remote.run(entrypoint, timeout_ms + 500)?;
    memory.read(remote.address, parameters)?;
    properties::task_result(exit, parameters)
}

fn is_data_model_pointer(
    memory: &ProcessMemory,
    module: &ModuleEntry,
    expected_vtable: Option<usize>,
    outer: usize,
) -> bool {
    if let Some(expected) = expected_vtable
        && memory.read_u64(outer).ok().map(|value| value as usize) != Some(expected)
    {
        return false;
    }
    read_rtti_type(memory, outer, module.base, module.size).as_deref()
        == Some(".?AVDataModel@RBX@@")
}

fn find_play_client_data_model(
    memory: &ProcessMemory,
    module: &ModuleEntry,
    layout: &PackageLayout,
    lookup: &PlayClientLookup<'_>,
) -> Result<ActiveDataModel> {
    let player_path = [
        "Players".to_string(),
        lookup.player_name.to_string(),
        "PlayerScripts".to_string(),
    ];
    let nonce_path = lookup
        .nonce
        .map(|nonce| ["VirtualInputManager".to_string(), nonce.to_string()]);
    let nonce_slice = nonce_path.as_ref().map(|path| path.as_slice());
    let expected_vtable = layout.data_model_vtable.map(|rva| module.base + rva);

    if let Some(outer) = lookup.hint_outer
        && likely_pointer(outer)
        && is_data_model_pointer(memory, module, expected_vtable, outer)
    {
        let mut hint_layout = None;
        if let Some(model) = inspect_data_model(
            memory,
            module,
            outer,
            &mut hint_layout,
            &player_path,
            nonce_slice,
        ) {
            return Ok(model);
        }
    }

    let data_base = module
        .base
        .checked_add(layout.data.virtual_address)
        .context("Studio data section address overflowed")?;
    let bytes = memory
        .read_vec(data_base, layout.data.virtual_size)
        .context("Could not read Studio's data section")?;
    let image_end = module.base.saturating_add(module.size);
    let mut shared_candidates = Vec::new();
    let mut other_candidates = Vec::new();
    let mut seen_outers = HashSet::new();
    for offset in (0..=bytes.len().saturating_sub(8)).step_by(8) {
        let outer = u64::from_le_bytes(
            bytes[offset..offset + 8]
                .try_into()
                .expect("data-section word is eight bytes"),
        ) as usize;
        if !likely_pointer(outer)
            || (module.base..image_end).contains(&outer)
            || !seen_outers.insert(outer)
        {
            continue;
        }
        let next_is_heap = bytes
            .get(offset + 8..offset + 16)
            .map(|word| {
                u64::from_le_bytes(
                    word.try_into()
                        .expect("data-section next word is eight bytes"),
                ) as usize
            })
            .is_some_and(|next| likely_pointer(next) && !(module.base..image_end).contains(&next));
        if next_is_heap {
            shared_candidates.push(outer);
        } else {
            other_candidates.push(outer);
        }
    }

    let scan = CandidateScan {
        memory,
        module,
        expected_vtable,
        player_path: &player_path,
        nonce_path: nonce_slice,
    };
    let mut known_layout = None;
    let mut matches = Vec::new();
    scan_data_candidates_with(&scan, &shared_candidates, &mut known_layout, &mut matches);
    if matches.is_empty() {
        scan_data_candidates_with(&scan, &other_candidates, &mut known_layout, &mut matches);
    }

    anyhow::ensure!(
        matches.len() == 1,
        "Play client DataModel lookup for '{}' matched {} live DataModels in Studio data (expected 1)",
        lookup.player_name,
        matches.len()
    );
    Ok(matches.remove(0))
}

struct CandidateScan<'a> {
    memory: &'a ProcessMemory,
    module: &'a ModuleEntry,
    expected_vtable: Option<usize>,
    player_path: &'a [String],
    nonce_path: Option<&'a [String]>,
}

fn scan_data_candidates_with(
    scan: &CandidateScan<'_>,
    candidates: &[usize],
    known_layout: &mut Option<InstanceLayout>,
    matches: &mut Vec<ActiveDataModel>,
) {
    for &outer in candidates {
        if !is_data_model_pointer(scan.memory, scan.module, scan.expected_vtable, outer) {
            continue;
        }
        if let Some(model) = inspect_data_model(
            scan.memory,
            scan.module,
            outer,
            known_layout,
            scan.player_path,
            scan.nonce_path,
        ) {
            matches.push(model);
        }
    }
}

fn inspect_data_model(
    memory: &ProcessMemory,
    module: &ModuleEntry,
    outer: usize,
    known_layout: &mut Option<InstanceLayout>,
    player_path: &[String],
    nonce_path: Option<&[String]>,
) -> Option<ActiveDataModel> {
    let layouts: Vec<(InstanceLayout, Vec<SharedEntry>)> = if let Some(layout) = *known_layout {
        let instance = outer.checked_add(layout.data_model_instance)?;
        if memory
            .read_u64(instance + layout.self_pointer)
            .ok()
            .map(|value| value as usize)
            != Some(instance)
            || read_instance_class(memory, instance, layout).as_deref() != Some("DataModel")
        {
            return None;
        }
        let roots = read_children(memory, instance, layout)?;
        vec![(layout, roots)]
    } else {
        let discovered: Vec<(InstanceLayout, Vec<SharedEntry>)> =
            discover_instance_layout(memory, module, outer)
                .into_iter()
                .map(|(layout, _, roots)| (layout, roots))
                .collect();
        if let Some((layout, _)) = discovered.first() {
            *known_layout = Some(*layout);
        }
        discovered
    };

    for (layout, roots) in layouts {
        if roots.is_empty()
            || roots.len() > MAX_ROOTS
            || roots
                .iter()
                .any(|root| !valid_owner(memory, root.owner, module.base, module.size))
            || !has_required_data_model_roots(memory, &roots, layout)
        {
            continue;
        }
        let instance = outer.checked_add(layout.data_model_instance)?;
        let owner = memory.read_u64(instance + layout.self_pointer + 8).ok()? as usize;
        if !valid_owner(memory, owner, module.base, module.size) {
            continue;
        }
        let model = ActiveDataModel {
            outer,
            owner,
            roots,
            layout,
        };
        if properties::resolve_path(memory, &model, player_path, &[]).is_err() {
            continue;
        }
        if let Some(marker) = nonce_path
            && properties::resolve_path(memory, &model, marker, &[]).is_err()
        {
            continue;
        }
        return Some(model);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnect_closes_slot_and_pruning_evicts_inactive_runtimes() {
        let active_id = format!("test-active-{}", std::process::id());
        let stale_id = format!("test-stale-{}", std::process::id());
        let slot_active = slot_for_runtime(&active_id);
        let slot_stale = slot_for_runtime(&stale_id);
        assert!(matches!(
            *slot_active.lock_recover(),
            SessionSlot::Uninitialized
        ));
        assert!(matches!(
            *slot_stale.lock_recover(),
            SessionSlot::Uninitialized
        ));

        prune_gamepad_sessions(std::slice::from_ref(&active_id));
        {
            let map = sessions().lock_recover();
            assert!(map.contains_key(&active_id));
            assert!(!map.contains_key(&stale_id));
        }

        disconnect_gamepad_runtimes(std::slice::from_ref(&active_id));
        assert!(matches!(*slot_active.lock_recover(), SessionSlot::Closed));
        assert!(!sessions().lock_recover().contains_key(&active_id));
    }
}
