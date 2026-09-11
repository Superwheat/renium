//! Native reader invocation for an already-staged editor transaction. The
//! plugin holds its original objects; this boundary checks those exact identities
//! again under Studio's DataModel lock before the engine reads the payload.
use super::*;
use crate::editor::types::EditorNativeReplacement;
use serde::Deserialize;
use serde_json::Value;

const HEADER: usize = 1480;
const CLASS: usize = 264;
const TARGET: usize = 80;
const RESPONSE: usize = 312;
const READER_TIMING: usize = 136;
pub(crate) const CREATED_ROW: usize = 56;

pub(crate) struct NativeReadReceipt {
    pub status: u32,
    pub state: u32,
    pub error: String,
    pub created: Vec<u8>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Target {
    path_segments: Vec<String>,
    path_ordinals: Vec<usize>,
    class_name: String,
    debug_id: String,
    ordinal: Option<u32>,
    class_count: Option<u32>,
    binary_referent: Option<i32>,
    reference_only: Option<bool>,
}

pub(crate) fn read_service_payload(
    pid: u32,
    title: &str,
    bytes: &[u8],
    replacement: &EditorNativeReplacement,
    held: &Value,
    timeout: Duration,
    invoked: &mut bool,
) -> Result<NativeReadReceipt> {
    use crate::app::timing::{trace_profile, trace_scope};
    let started = Instant::now();
    let preparation = trace_scope("native.import", "prepare native service reader");
    let mut stages = crate::app::timing::trace_stages(
        "native.host",
        "validate payload boundaries and open Studio process",
    );
    anyhow::ensure!(
        (32..=512 * 1024 * 1024).contains(&bytes.len()),
        "Invalid native import payload size"
    );
    let plan = &replacement.plan;
    anyhow::ensure!(
        (1..=4096).contains(&replacement.batches.len()),
        "Invalid native import batch count"
    );
    let mut end = 0;
    for batch in &replacement.batches {
        anyhow::ensure!(
            batch.bytes.start == end && batch.bytes.end <= bytes.len() && batch.bytes.len() >= 32,
            "Invalid native import batch boundary"
        );
        end = batch.bytes.end;
    }
    anyhow::ensure!(
        end == bytes.len(),
        "Native import batches do not cover the payload"
    );
    let memory = ProcessMemory::open(pid)?;
    let current_modules = modules(pid)?;
    let studio = current_modules
        .first()
        .context("Studio process has no main module")?;
    anyhow::ensure!(
        studio.name.eq_ignore_ascii_case("RobloxStudioBeta.exe"),
        "Native import target is not Studio"
    );
    stages.next("resolve window DataModel and validate loaded engine code");
    let window = capture_window(pid, title)?;
    let layout = package_layout(&studio.path)?;
    verify_loaded_image(&memory, studio, layout.image_stamp)?;
    let model = active_data_model(pid, &memory, studio, layout.data, title)?;
    let (loader, history) = loader::verify_loaded(&memory, studio)?;
    anyhow::ensure!(
        loader.instance_offset == model.layout.data_model_instance,
        "Native reader and DataModel discovery disagree"
    );
    let (_, serializer, _) = studio_layout(&studio.path)?;
    for rva in [layout.submit_task, serializer.deallocator] {
        properties::verified_code(&memory, studio, &layout, studio.base + rva, 64)?;
    }
    let instance = model.outer + model.layout.data_model_instance;
    let debug_id = properties::debug_id_function(&memory, studio, &layout, &model, instance)?;
    stages.next("encode and validate native classes targets and anchors");
    let class_index = plan
        .classes
        .iter()
        .enumerate()
        .map(|(index, class)| (class.name.as_str(), (index, class.count)))
        .collect::<HashMap<_, _>>();
    anyhow::ensure!(
        class_index.len() == plan.classes.len() && plan.classes.len() <= 4096,
        "Native import class plan is duplicated or oversized"
    );
    let expected_created = plan
        .classes
        .iter()
        .try_fold(0usize, |count, class| {
            count
                .checked_add(class.count as usize)
                .context("Native import class count overflowed")
        })?
        .checked_sub(plan.bindings.len())
        .and_then(|count| count.checked_sub(plan.aliases.len()))
        .context("Native import bindings exceed class count")?;
    anyhow::ensure!(
        expected_created <= CAPTURE_MAX_ROWS,
        "Native import class count is oversized"
    );
    let mut parameters = vec![0; HEADER + plan.classes.len() * CLASS];
    let database = rbx_reflection_database::get()?;
    for (index, class) in plan.classes.iter().enumerate() {
        anyhow::ensure!(
            !class.name.is_empty()
                && class.name.len() < 256
                && !class.name.contains('\0')
                && class.count > 0,
            "Invalid native import class"
        );
        let offset = HEADER + index * CLASS;
        parameters[offset..offset + class.name.len()].copy_from_slice(class.name.as_bytes());
        put_u32(&mut parameters, offset + 256, class.count);
        // Forward script insertions: Studio's callback also initializes ScriptGuid.
        let script = !database.classes.contains_key(class.name.as_str())
            || crate::rbx::decode::rbx_reflection_class_is_a(
                database,
                &class.name,
                "LuaSourceContainer",
            );
        put_u32(&mut parameters, offset + 260, u32::from(script));
    }
    let mut indices = HashMap::<(usize, usize), usize>::new();
    let mut targets: Vec<[u8; TARGET]> = Vec::new();
    let mut bound = HashSet::new();
    for (key, binding) in [("containers", false), ("bindings", true)] {
        let requested: Vec<Target> = serde_json::from_value(
            held.get(key)
                .cloned()
                .with_context(|| format!("Native import response is missing {key}"))?,
        )?;
        for requested in requested {
            anyhow::ensure!(
                !requested.debug_id.is_empty()
                    && requested.debug_id.len() < 48
                    && !requested.debug_id.contains('\0'),
                "Invalid native import target identity"
            );
            let ancestors = properties::resolve_path(
                &memory,
                &model,
                &requested.path_segments,
                &requested.path_ordinals,
            )?;
            let leaf = *ancestors
                .last()
                .context("Native import target has no path")?;
            anyhow::ensure!(
                read_instance_class(&memory, leaf.instance, model.layout).as_deref()
                    == Some(&requested.class_name),
                "Native import target changed class"
            );
            let mut parent = u32::MAX;
            for ancestor in ancestors {
                let index = *indices
                    .entry((ancestor.instance, ancestor.owner))
                    .or_insert_with(|| {
                        let index = targets.len();
                        let mut row = [0; TARGET];
                        put_u64(&mut row, 0, ancestor.instance);
                        put_u64(&mut row, 8, ancestor.owner);
                        put_u32(&mut row, 16, parent);
                        put_u32(&mut row, 20, u32::MAX);
                        targets.push(row);
                        index
                    });
                anyhow::ensure!(
                    read_u32(&targets[index], 16)? == parent,
                    "Native import ancestry changed"
                );
                parent = u32::try_from(index)?;
            }
            let row = &mut targets[parent as usize];
            if key == "containers" {
                put_u32(row, 76, 1);
            }
            let expected = requested.debug_id.as_bytes();
            anyhow::ensure!(
                row[24] == 0
                    || row[24 + expected.len()] == 0 && row[24..24 + expected.len()] == *expected,
                "Native import target has conflicting identities"
            );
            row[24..24 + 48].fill(0);
            row[24..24 + expected.len()].copy_from_slice(expected);
            if binding {
                let binary_id = requested
                    .binary_referent
                    .context("Native binding is missing its serialized identity")?;
                let source = plan
                    .bindings
                    .iter()
                    .find(|source| source.binary_referent == binary_id)
                    .context("Studio returned an unrequested native binding")?;
                anyhow::ensure!(
                    source.class_name == requested.class_name
                        && Some(source.ordinal) == requested.ordinal
                        && Some(source.class_count) == requested.class_count
                        && Some(source.reference_only) == requested.reference_only
                        && bound.insert(binary_id),
                    "Studio changed the native binding plan"
                );
                let (class, count) = class_index
                    .get(requested.class_name.as_str())
                    .context("Native binding class is absent from the payload")?;
                anyhow::ensure!(
                    *count == source.class_count,
                    "Native binding class counts differ"
                );
                put_u32(row, 20, u32::try_from(*class)?);
                put_u32(row, 72, source.ordinal);
            }
        }
    }
    anyhow::ensure!(
        bound.len() == plan.bindings.len() && !targets.is_empty() && targets.len() <= 2_000_000,
        "Native import target plan is incomplete or oversized"
    );
    let history_object = model
        .roots
        .iter()
        .find(|root| {
            read_instance_class(&memory, root.instance, model.layout).as_deref()
                == Some("ChangeHistoryService")
        })
        .context("Native import has no ChangeHistoryService")?;
    anyhow::ensure!(
        memory.read_u64(history_object.instance)? as usize == studio.base + history.table,
        "Studio history complete-object identity changed"
    );
    let history_index = targets.len();
    let mut history_target = [0; TARGET];
    put_u64(&mut history_target, 0, history_object.instance);
    put_u64(&mut history_target, 8, history_object.owner);
    put_u32(&mut history_target, 16, u32::MAX);
    put_u32(&mut history_target, 20, u32::MAX);
    targets.push(history_target);
    for row in &targets {
        parameters.extend_from_slice(row);
    }
    let bound_ordinals = plan
        .bindings
        .iter()
        .map(|binding| {
            (
                class_index[binding.class_name.as_str()].0 as u32,
                binding.ordinal,
            )
        })
        .collect::<HashSet<_>>();
    let mut alias_ordinals = HashSet::new();
    for alias in &plan.aliases {
        anyhow::ensure!(
            plan.classes
                .get(alias.class_index as usize)
                .is_some_and(|class| alias.ordinal < class.count)
                && alias.source_ordinal < alias.ordinal
                && !bound_ordinals.contains(&(alias.class_index, alias.ordinal))
                && alias_ordinals.insert((alias.class_index, alias.ordinal)),
            "Invalid native batch anchor"
        );
        for value in [alias.class_index, alias.ordinal, alias.source_ordinal] {
            parameters.extend_from_slice(&value.to_le_bytes());
        }
    }
    anyhow::ensure!(
        plan.aliases
            .iter()
            .all(|alias| !alias_ordinals.contains(&(alias.class_index, alias.source_ordinal))),
        "Native batch anchors cannot form alias chains"
    );
    put_u32(&mut parameters, 1472, u32::try_from(plan.aliases.len())?);
    let pause = std::env::var("RENIUM_NATIVE_IMPORT_BATCH_PAUSE_MS")
        .ok()
        .map(|value| value.parse::<u32>())
        .transpose()?
        .unwrap_or(1);
    anyhow::ensure!(pause <= 16, "Native batch pause exceeds 16 ms");
    put_u32(&mut parameters, 1476, pause);
    for batch in &replacement.batches {
        parameters.extend_from_slice(&(batch.bytes.len() as u64).to_le_bytes());
    }
    parameters.extend_from_slice(bytes);
    for (offset, value) in [
        (0, instance),
        (8, model.owner),
        (
            16,
            data_model_task_context(&memory, studio, &layout, &model)?,
        ),
        (24, studio.base + layout.submit_task),
        (32, studio.base + loader.loader),
        (40, studio.base + loader.factory.lookup),
        (48, studio.base + loader.factory.intern_name),
        (56, studio.base + loader.factory.origin),
        (72, debug_id),
        (80, studio.base + serializer.deallocator),
        (104, bytes.len()),
        (1440, studio.base + history.table),
    ] {
        put_u64(&mut parameters, offset, value);
    }
    for (offset, value) in [
        (64, loader.factory.context_bytes),
        (68, model.layout.class_descriptor),
        (88, model.layout.children),
        (92, model.layout.self_pointer),
        (96, targets.len()),
        (100, plan.classes.len()),
        (1432, replacement.batches.len()),
        (1448, history_index),
        (1452, history.slots),
        (1456, history.insertion),
        (1460, history.record),
        (1464, history.pending),
        (1468, history.playback),
    ] {
        put_u32(&mut parameters, offset, u32::try_from(value)?);
    }
    stages.next("load or validate native helper module");
    let helper = ensure_helper_loaded_with_timeout(
        pid,
        &memory,
        &current_modules,
        capture_remaining_ms(started, timeout)?,
    )?;
    let entry = helper
        .checked_add(helper_export_rva("ReniumReadServices")?)
        .context("Native reader helper address overflowed")?;
    anyhow::ensure!(
        capture_window(pid, title)? == window,
        "Studio target changed during native import preparation"
    );
    stages.next("create private result transport");
    let mut nonce = [0; 16];
    getrandom::fill(&mut nonce)
        .map_err(|error| anyhow::anyhow!("Cannot create native import nonce: {error}"))?;
    let directory = std::env::temp_dir().join("renium-native");
    fs::create_dir_all(&directory)?;
    let path = directory.join(format!(
        "import-{pid}-{:032x}.tmp",
        u128::from_le_bytes(nonce)
    ));
    let mut transport = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .share_mode(0x7)
        .custom_flags(0x04000000)
        .open(&path)
        .context("Cannot create native import response transport")?;
    let path = wide(path.as_os_str());
    anyhow::ensure!(path.len() <= 520, "Native import response path is too long");
    for (index, unit) in path.iter().enumerate() {
        parameters[392 + index * 2..394 + index * 2].copy_from_slice(&unit.to_le_bytes());
    }
    put_u32(
        &mut parameters,
        112,
        capture_remaining_ms(started, timeout)?.min(20_000),
    );
    put_u32(
        &mut parameters,
        1436,
        u32::from(crate::app::output::global_log_enabled(5)),
    );
    stages.next("allocate and copy payload into Studio");
    let remote = memory.allocate(parameters.len())?;
    memory.write(remote.address, &parameters)?;
    drop(preparation);
    let operation = trace_scope("native.import", "native service reader");
    stages.next("execute native helper and wait for completion");
    let helper_started = Instant::now();
    *invoked = true;
    let exit = remote.run_owned(entry, capture_remaining_ms(started, timeout)?)?;
    let helper_finished = Instant::now();
    let helper_wall_ms = helper_finished.duration_since(helper_started).as_secs_f64() * 1000.0;
    stages.next("read and validate native result header");
    let mut response = [0; RESPONSE];
    transport
        .read_exact(&mut response)
        .context("Native import outcome is unavailable; do not retry the mutation")?;
    drop(operation);
    anyhow::ensure!(
        read_u32(&response, 0)? == 0x52494E52 && read_u32(&response, 4)? == 6,
        "Native import receipt header is invalid"
    );
    let status = read_u32(&response, 8)?;
    let state = read_u32(&response, 48)?;
    anyhow::ensure!(
        state <= 15
            && read_u32(&response, 52)? as usize == replacement.batches.len()
            && (status != 4 || state == 14),
        "Native import receipt has an inconsistent execution state"
    );
    let count = read_u32(&response, 12)? as usize;
    let history_skipped = usize::try_from(read_u64(&response, 40)?)?;
    anyhow::ensure!(
        count <= expected_created
            && (status != 4 || count == expected_created)
            && history_skipped <= count
            && transport.metadata()?.len()
                == (RESPONSE + count * CREATED_ROW + replacement.batches.len() * 8 + READER_TIMING)
                    as u64,
        "Native import creation receipt is incomplete"
    );
    anyhow::ensure!(
        (status == 4 && exit == 0) || status == exit,
        "Native import response disagrees with helper outcome"
    );
    stages.next("read creation receipts batch timings and native accounting");
    let mut created = vec![0; count * CREATED_ROW];
    transport
        .read_exact(&mut created)
        .context("Native import identity receipt is truncated")?;
    let mut batch_timings = vec![0; replacement.batches.len() * 8];
    transport
        .read_exact(&mut batch_timings)
        .context("Native import batch timings are truncated")?;
    let mut accounting = [0; READER_TIMING];
    transport
        .read_exact(&mut accounting)
        .context("Native import accounting is truncated")?;
    let frequency = read_u64(&accounting, 0)?;
    let total = read_u64(&accounting, 8)?;
    anyhow::ensure!(frequency > 0, "Native import timing clock is invalid");
    let names = [
        "request setup and payload ownership",
        "task dispatch and queue wait",
        "target validation and patch setup",
        "Studio native deserialization",
        "reader verification and patch restoration",
        "identity receipt and ownership release",
        "receipt encoding and transport write",
        "worker completion handoff",
        "deliberate batch pacing",
        "producer loop bookkeeping",
    ];
    let mut phases = serde_json::Map::new();
    let mut measured = 0u64;
    for (index, name) in names.iter().enumerate() {
        let ticks = read_u64(&accounting, 16 + index * 8)?;
        measured = measured
            .checked_add(ticks)
            .context("Native timing total overflowed")?;
        phases.insert(
            (*name).to_string(),
            serde_json::json!(ticks as f64 * 1000.0 / frequency as f64),
        );
    }
    anyhow::ensure!(
        measured == total,
        "Native timing phases do not cover the helper timeline"
    );
    let accounted_ms = total as f64 * 1000.0 / frequency as f64;
    let factory_ms = read_u64(&accounting, 96)? as f64 * 1000.0 / frequency as f64;
    let constructor_ms = read_u64(&accounting, 104)? as f64 * 1000.0 / frequency as f64;
    trace_profile(
        "native.import.accounting",
        &serde_json::json!({
            "wallMs": helper_wall_ms, "helperMeasuredMs": accounted_ms,
            "invocationAndReturnMs": helper_wall_ms - accounted_ms,
            "hostInterval": crate::app::timing::trace_range(helper_started, helper_finished),
            "phasesMs": phases,
            "factoryMs": factory_ms, "constructorMs": constructor_ms,
            "framesDelivered": read_u64(&accounting, 112)?,
            "frameWaits": read_u64(&accounting, 120)?,
            "frameTimeouts": read_u64(&accounting, 128)?,
            "basis": "Exclusive wall-clock phases; deserialization includes engine and synchronous callbacks. Invocation/return includes remote-thread startup, trailing accounting write and helper destruction.",
        }),
    );
    stages.next("emit per-batch diagnostic records");
    for (index, batch) in replacement.batches.iter().enumerate() {
        trace_profile(
            "native.import.batch",
            &serde_json::json!({
                "index": index + 1, "batches": replacement.batches.len(),
                "services": batch.services, "bytes": batch.bytes.len(),
                "readMs": read_u64(&batch_timings, index * 8)? as f64 / 1000.0,
            }),
        );
    }
    stages.next("validate creation receipt identities and ordinals");
    let mut ordinals = HashSet::with_capacity(count);
    let mut identities = HashSet::with_capacity(count);
    for row in created.chunks_exact(CREATED_ROW) {
        let class = read_u32(row, 0)?;
        let ordinal = read_u32(row, 4)?;
        let end = row[8..]
            .iter()
            .position(|byte| *byte == 0)
            .context("Native import identity is unterminated")?;
        anyhow::ensure!(
            end > 0
                && plan
                    .classes
                    .get(class as usize)
                    .is_some_and(|entry| ordinal < entry.count)
                && !bound_ordinals.contains(&(class, ordinal))
                && !alias_ordinals.contains(&(class, ordinal))
                && ordinals.insert((class, ordinal))
                && identities.insert(&row[8..8 + end]),
            "Native import creation identities are invalid or duplicated"
        );
    }
    stages.next("emit reader summary and revalidate target window");
    trace_profile(
        "native.import.reader",
        &serde_json::json!({
            "queueMs": read_u64(&response, 16)? as f64 / 1000.0,
            "readMs": read_u64(&response, 24)? as f64 / 1000.0,
            "identityMs": read_u64(&response, 32)? as f64 / 1000.0,
            "created": count,
            "historySkipped": history_skipped,
            "bytes": bytes.len(), "targets": targets.len(),
        }),
    );
    let error = &response[56..312];
    let error = String::from_utf8_lossy(
        &error[..error
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(error.len())],
    );
    anyhow::ensure!(
        capture_window(pid, title)? == window,
        "Studio target changed during native import"
    );
    Ok(NativeReadReceipt {
        status,
        state,
        error: error.into_owned(),
        created,
    })
}
