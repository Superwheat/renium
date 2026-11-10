use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rbx_dom_weak::types::Ref as RbxRef;
use serde_json::{Map, Value, json};

use crate::app::output::print_json_output;
use crate::bytecode::query::{bytecode_parent_index, parse_property_assignments};
use crate::bytecode::references::{MissingTarget, ReferencedStores};
use crate::bytecode::{
    acquire_settings_file_lock, adopt_single_positional_target, apply_file_mutations,
    collect_source_path_updates, ensure_service_store_exists, file_mutation_paths,
    lock_existing_service_store, preserve_source_path_extension,
    resolve_bytecode_cli_settings_file, resolve_bytecode_selector,
};
use crate::cli::{
    BytecodeAddInstanceArgs, BytecodeCloneInstanceArgs, BytecodeDesyncPackageLinkArgs,
    BytecodeRemoveInstanceArgs,
};
use crate::editor::document::is_protected_engine_container;
use crate::editor::paths::{
    build_editor_instance_path_parts, build_editor_instance_paths,
    build_editor_source_paths_by_index, script_file_names,
};
use crate::editor::sync::is_lua_source_class;
use crate::settings::EXTERNAL_SOURCE_MARKER;
use crate::settings::bytecode::{
    SETTINGS_REFERENCE_SELECTOR_KEYS, SettingsBytecode, SettingsBytecodeInstance,
    encode_settings_bytecode, settings_reference_index, strict_reference_path,
};
use crate::settings::instance::{self as instance_api, AddInstanceSpec};
use crate::settings::tree::settings_children_by_parent;
use crate::system::files::{ensure_existing_ancestor_inside, exact_path_key, path_key};

/// Studio cannot instantiate a class tagged NotCreatable (Player, Terrain, a
/// service); a store holding one makes every push of that service fail, and
/// an imported Player leaves the Studio window unusable. Unknown class names
/// stay allowed because the reflection database can lag behind Studio.
fn ensure_creatable_class(class_name: &str) -> Result<()> {
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    // The reflection database does not tag Player or Terrain; both are
    // engine-owned and break the push that carries them.
    let engine_owned = matches!(class_name, "Player" | "Terrain" | "DataModel");
    if engine_owned
        || database.classes.get(class_name).is_some_and(|class| {
            class.tags.contains(&rbx_reflection::ClassTag::NotCreatable)
                || class.tags.contains(&rbx_reflection::ClassTag::Service)
        })
    {
        bail!(
            "{class_name} cannot be created in Studio (the class is not creatable); choose a creatable class such as Folder or Part"
        );
    }
    Ok(())
}

/// `-p NAME=JSON` names must be real properties of the class, stored under
/// their canonical spelling, or every later consumer misses them.
fn canonical_new_instance_properties(
    class_name: &str,
    properties: Map<String, Value>,
) -> Result<Map<String, Value>> {
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    if !database.classes.contains_key(class_name) {
        return Ok(properties);
    }
    let mut out = Map::new();
    let mut position = None;
    let mut orientation = None;
    for (name, value) in properties {
        if name.eq_ignore_ascii_case("Source") {
            out.insert("Source".to_string(), value);
            continue;
        }
        if name.eq_ignore_ascii_case("Position") && has_cframe(class_name) {
            position = Some(value);
            continue;
        }
        if name.eq_ignore_ascii_case("Orientation") && has_cframe(class_name) {
            orientation = Some(value);
            continue;
        }
        let Some(logical) =
            crate::rbx::encode::rbx_logical_property_name(database, class_name, &name)
        else {
            bail!("Property {name} does not exist on {class_name}; use -a for an attribute");
        };
        let value = crate::rbx::encode::typed_store_property_value(class_name, logical, value)?;
        out.insert(logical.to_string(), value);
    }
    if position.is_some() || orientation.is_some() {
        let cframe = cframe_from_parts(out.get("CFrame"), position.as_ref(), orientation.as_ref())?;
        out.insert("CFrame".to_string(), cframe);
    }
    Ok(out)
}

/// Whether instances of the class carry a CFrame, so Position and
/// Orientation (which Studio never saves) fold into it.
pub(crate) fn has_cframe(class_name: &str) -> bool {
    rbx_reflection_database::get().is_ok_and(|database| {
        crate::rbx::encode::rbx_property_descriptor(database, class_name, "CFrame").is_some()
    })
}

fn vector3_components(value: &Value) -> Option<[f64; 3]> {
    if let Some(items) = value.as_array() {
        let mut out = [0.0; 3];
        for (slot, item) in out.iter_mut().zip(items.iter().take(3)) {
            *slot = item.as_f64()?;
        }
        return (items.len() >= 3).then_some(out);
    }
    let object = value.as_object()?;
    Some([
        object.get("x").or_else(|| object.get("X"))?.as_f64()?,
        object.get("y").or_else(|| object.get("Y"))?.as_f64()?,
        object.get("z").or_else(|| object.get("Z"))?.as_f64()?,
    ])
}

/// A part's Position is not stored: it is the translation of its CFrame. The
/// CFrame with that translation keeps the rotation the part already has, or
/// none for a new part.
/// Lighting keeps the time of day as the string `TimeOfDay`; `ClockTime` is
/// the same value in hours and is never saved, so a stored ClockTime would
/// fail every push. This turns hours (18.25) into the saved form (18:15:00).
pub(crate) fn time_of_day_from_hours(value: &Value) -> Result<Value> {
    let hours = value
        .as_f64()
        .with_context(|| format!("ClockTime needs a number of hours, got {value}"))?;
    if !(0.0..=24.0).contains(&hours) {
        bail!("ClockTime must be between 0 and 24 hours, got {hours}");
    }
    let total = (hours * 3600.0).round() as u64 % 86_400;
    Ok(json!(format!(
        "{:02}:{:02}:{:02}",
        total / 3600,
        total / 60 % 60,
        total % 60
    )))
}

/// The stored CFrame with its position and/or rotation replaced: `position`
/// is a Vector3, `orientation` the Roblox Orientation (degrees about X, Y
/// and Z, applied as Studio does, Y then X then Z).
pub(crate) fn cframe_from_parts(
    current: Option<&Value>,
    position: Option<&Value>,
    orientation: Option<&Value>,
) -> Result<Value> {
    let mut components = current
        .and_then(|value| value.get("components"))
        .and_then(Value::as_array)
        .filter(|items| items.len() == 12)
        .map(|items| {
            let mut out = [0.0; 12];
            for (slot, item) in out.iter_mut().zip(items) {
                *slot = item.as_f64().unwrap_or(0.0);
            }
            out
        })
        .unwrap_or([0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]);
    if let Some(position) = position {
        let [x, y, z] = vector3_components(position).context(
            "Position needs x, y and z numbers, as {\"x\":..,\"y\":..,\"z\":..} or [x,y,z]",
        )?;
        components[0] = x;
        components[1] = y;
        components[2] = z;
    }
    if let Some(orientation) = orientation {
        let degrees = vector3_components(orientation)
            .context("Orientation needs x, y and z degrees, as [rx,ry,rz]")?;
        let rows = rotation_rows_from_orientation(degrees);
        for (slot, value) in components[3..].iter_mut().zip(rows.iter().flatten()) {
            *slot = *value;
        }
    }
    Ok(json!({ "_type": "CFrame", "components": components }))
}

/// Studio's Orientation is Euler angles in degrees applied Y, then X, then
/// Z (`CFrame.fromOrientation`); the rows of the resulting rotation matrix.
pub(crate) fn rotation_rows_from_orientation([rx, ry, rz]: [f64; 3]) -> [[f64; 3]; 3] {
    let (sx, cx) = rx.to_radians().sin_cos();
    let (sy, cy) = ry.to_radians().sin_cos();
    let (sz, cz) = rz.to_radians().sin_cos();
    let y = [[cy, 0.0, sy], [0.0, 1.0, 0.0], [-sy, 0.0, cy]];
    let x = [[1.0, 0.0, 0.0], [0.0, cx, -sx], [0.0, sx, cx]];
    let z = [[cz, -sz, 0.0], [sz, cz, 0.0], [0.0, 0.0, 1.0]];
    let multiply = |a: [[f64; 3]; 3], b: [[f64; 3]; 3]| {
        let mut out = [[0.0; 3]; 3];
        for (i, row) in out.iter_mut().enumerate() {
            for (j, cell) in row.iter_mut().enumerate() {
                *cell = (0..3).map(|k| a[i][k] * b[k][j]).sum();
            }
        }
        out
    };
    let mut rows = multiply(multiply(y, x), z);
    for cell in rows.iter_mut().flatten() {
        if cell.abs() < 1e-9 {
            *cell = 0.0;
        }
    }
    rows
}

/// Studio's Orientation of a rotation matrix given by its rows, in degrees
/// (`CFrame:ToOrientation`): the inverse of `rotation_rows_from_orientation`.
/// Looking straight up or down leaves Y and Z one shared turn, reported as Y.
pub(crate) fn orientation_from_rotation_rows(rows: [[f64; 3]; 3]) -> [f64; 3] {
    let rx = (-rows[1][2]).clamp(-1.0, 1.0).asin();
    let (ry, rz) = if rows[1][2].abs() > 0.999_999 {
        ((-rows[2][0]).atan2(rows[0][0]), 0.0)
    } else {
        (rows[0][2].atan2(rows[2][2]), rows[1][0].atan2(rows[1][1]))
    };
    [rx.to_degrees(), ry.to_degrees(), rz.to_degrees()]
}

/// What a new store instance needs; the CLI and script imports both build it.
pub(crate) struct StoreInstanceRequest {
    pub(crate) settings_file: PathBuf,
    pub(crate) service_hint: String,
    pub(crate) name: String,
    pub(crate) class_name: String,
    pub(crate) settings_id: Option<String>,
    pub(crate) parent: StoreParentSelector,
    pub(crate) properties: Vec<String>,
    pub(crate) attributes: Vec<String>,
    pub(crate) override_packages: bool,
}

#[derive(Default)]
pub(crate) struct StoreParentSelector {
    pub(crate) no_parent: bool,
    pub(crate) parent_index: Option<usize>,
    pub(crate) parent_settings_id: Option<String>,
    pub(crate) parent_name: Option<String>,
    pub(crate) parent_class_name: Option<String>,
}

pub(crate) fn bytecode_add_instance(args: BytecodeAddInstanceArgs) -> Result<()> {
    if let Some(batch) = args.batch.as_deref() {
        return crate::bytecode::add_batch::bytecode_add_batch(
            batch,
            args.pretty,
            args.push,
            args.override_packages,
        );
    }
    let (Some(name), Some(class_name)) = (args.name, args.class_name) else {
        bail!("Give -n NAME and -c CLASS, or --batch with JSON ops");
    };
    let (settings_file, service_hint) = resolve_bytecode_cli_settings_file(
        args.input.settings_file.as_deref(),
        args.input.service_or_file.as_deref(),
        args.service_hint.as_deref(),
    )?;
    let names = |items: &[String]| {
        items
            .iter()
            .map(|item| {
                item.split_once('=')
                    .map_or(item.as_str(), |(name, _)| name)
                    .trim()
                    .to_string()
            })
            .collect::<Vec<_>>()
    };
    let (property_names, attribute_names) = (names(&args.properties), names(&args.attributes));
    let mut result = add_store_instance(StoreInstanceRequest {
        settings_file,
        service_hint,
        name,
        class_name,
        settings_id: args.settings_id,
        parent: StoreParentSelector {
            no_parent: args.parent.no_parent,
            parent_index: args.parent.parent_index,
            parent_settings_id: args.parent.parent_settings_id,
            parent_name: args.parent.parent_name,
            parent_class_name: args.parent.parent_class_name,
        },
        properties: args.properties,
        attributes: args.attributes,
        override_packages: args.override_packages,
    })?;
    if args.push {
        push_stored_instance_reading(&mut result, &property_names, &attribute_names)?;
    }
    print_json_output(&result, args.pretty)
}

/// Pushes the instance a store edit just wrote (its `settingsFile` and
/// `settingsId`) to the open place, scoped to that instance and without a
/// review, and records the push summary under `push`.
/// Pushes the edited instance and then reads it back from the open place:
/// `studio` carries its live path, class and the named properties and
/// attributes, so the caller needs no separate read to confirm the edit.
pub(crate) fn push_stored_instance_reading(
    result: &mut Value,
    properties: &[String],
    attributes: &[String],
) -> Result<()> {
    let settings_file = result
        .get("settingsFile")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .context("The store edit did not name its file")?;
    let settings_id = result
        .get("settingsId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .context("The store edit did not name its instance; push the store file with `rbx ps`")?;
    result["push"] = push_stored_instances(settings_file.clone(), vec![settings_id.clone()])?;
    let document = SettingsBytecode::read_file(&settings_file)?;
    let Some(index) = document
        .instances
        .iter()
        .position(|instance| instance.settings_id == settings_id)
    else {
        return Ok(());
    };
    let service = bytecode_service_name(&document, &settings_file, "");
    let (segments, _) = build_editor_instance_path_parts(&document, &service);
    if let Some(path) = segments.get(index).cloned().flatten()
        && let Ok(studio) = read_back_from_studio(&path, properties, attributes)
    {
        if let Some(verified) = read_back_verifies(
            &studio,
            &path,
            &document.instances[index],
            properties,
            attributes,
        ) {
            result["verified"] = json!(verified);
            // One verdict: the read-back supersedes the push's own flag.
            if let Some(push) = result.get_mut("push").and_then(Value::as_object_mut) {
                push.remove("verified");
            }
        }
        result["studio"] = studio;
    }
    Ok(())
}

/// Whether a read-back row shows the pushed instance: `Some(true)` when the
/// instance is at `path` with the stored class and every requested property
/// and attribute equal to the store's value, `Some(false)` when it is missing
/// or a value differs, `None` when a live value's shape cannot be compared.
pub(crate) fn read_back_verifies(
    row: &Value,
    path: &[String],
    stored: &SettingsBytecodeInstance,
    properties: &[String],
    attributes: &[String],
) -> Option<bool> {
    if row.get("missing").and_then(Value::as_bool) == Some(true)
        || row.get("path").and_then(Value::as_str) != Some(path.join(".").as_str())
        || row.get("class").and_then(Value::as_str) != Some(stored.class_name.as_str())
    {
        return Some(false);
    }
    let mut verified = Some(true);
    let mut compare = |expected: Option<&Value>, live: Option<&Value>| {
        let Some(expected) = expected else {
            return;
        };
        match live.and_then(|live| live_value_agrees(expected, live)) {
            Some(true) => {}
            Some(false) => verified = Some(false),
            None => {
                if verified == Some(true) {
                    verified = None;
                }
            }
        }
    };
    for name in properties {
        compare(stored.properties.get(name), row.get(name));
    }
    for name in attributes {
        compare(
            stored.attributes.get(name),
            row.get(format!("@{name}").as_str()),
        );
    }
    verified
}

/// Compares a stored value with the shape Studio returns for it: numbers
/// within float tolerance, enum items by name whichever field names them,
/// typed objects field by field; `None` when the shapes do not line up.
fn live_value_agrees(expected: &Value, live: &Value) -> Option<bool> {
    match (expected, live) {
        (Value::Number(a), Value::Number(b)) => {
            let (a, b) = (a.as_f64()?, b.as_f64()?);
            Some((a - b).abs() <= 1e-3_f64.max(a.abs().max(b.abs()) * 1e-4))
        }
        (Value::Bool(a), Value::Bool(b)) => Some(a == b),
        (Value::String(a), Value::String(b)) => Some(a == b),
        (Value::Null, Value::Null) => Some(true),
        (Value::Object(a), Value::Object(b)) => {
            if a.get("_type") != b.get("_type") {
                return None;
            }
            if a.get("_type").and_then(Value::as_str) == Some("EnumItem") {
                let expected = a.get("name").and_then(Value::as_str)?;
                let live = b.get("name").and_then(Value::as_str).or_else(|| {
                    b.get("value")
                        .and_then(Value::as_str)
                        .and_then(|value| value.rsplit('.').next())
                })?;
                return Some(expected == live);
            }
            let mut agree = Some(true);
            for (key, value) in a {
                if key == "_type" || key == "enumType" {
                    continue;
                }
                match live_value_agrees(value, b.get(key)?) {
                    Some(true) => {}
                    Some(false) => return Some(false),
                    None => agree = None,
                }
            }
            agree
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                return Some(false);
            }
            let mut agree = Some(true);
            for (value, other) in a.iter().zip(b) {
                match live_value_agrees(value, other) {
                    Some(true) => {}
                    Some(false) => return Some(false),
                    None => agree = None,
                }
            }
            agree
        }
        _ => None,
    }
}

/// The full path (service first) of a stored instance, by settings id.
pub(crate) fn stored_path_segments(
    settings_file: &Path,
    settings_id: Option<&str>,
) -> Result<Option<Vec<String>>> {
    let Some(settings_id) = settings_id else {
        return Ok(None);
    };
    let document = SettingsBytecode::read_file(settings_file)?;
    let service = bytecode_service_name(&document, settings_file, "");
    let Some(index) = document
        .instances
        .iter()
        .position(|instance| instance.settings_id == settings_id)
    else {
        return Ok(None);
    };
    let (segments, _) = build_editor_instance_path_parts(&document, &service);
    Ok(segments.get(index).cloned().flatten())
}

pub(crate) fn luau_string_list(items: &[String]) -> String {
    let items = items
        .iter()
        .map(|item| format!("{item:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{{{items}}}")
}

/// One live read of the instance at `path` in the Edit window: its full
/// name, class, the given properties and the given attributes.
fn read_back_from_studio(
    path: &[String],
    properties: &[String],
    attributes: &[String],
) -> Result<Value> {
    let rows =
        read_back_many_from_studio(std::slice::from_ref(&path.to_vec()), properties, attributes)?;
    Ok(rows
        .as_array()
        .and_then(|rows| rows.first())
        .cloned()
        .unwrap_or(Value::Null))
}

/// One live read of several instances in the Edit window, as an array of rows
/// in the order of `paths`: each row has the full name, class, the given
/// properties and attributes, or `missing` with the path that was not found.
pub(crate) fn read_back_many_from_studio(
    paths: &[Vec<String>],
    properties: &[String],
    attributes: &[String],
) -> Result<Value> {
    let paths = paths
        .iter()
        .map(|path| luau_string_list(path))
        .collect::<Vec<_>>()
        .join(", ");
    let code = format!(
        "local function at(segments) local inst = game for _, name in segments do inst = inst:FindFirstChild(name) if not inst then return nil end end return inst end local rows = {{}} for _, segments in {{{paths}}} do local inst = at(segments) if not inst then table.insert(rows, {{ missing = true, path = table.concat(segments, '.') }}) else local row = {{ path = inst:GetFullName(), class = inst.ClassName }} for _, name in {} do if name == 'Tags' then row[name] = inst:GetTags() else local ok, value = pcall(function() return inst[name] end) if ok then row[name] = value end end end for _, name in {} do row['@' .. name] = inst:GetAttribute(name) end table.insert(rows, row) end end return rows",
        luau_string_list(properties),
        luau_string_list(attributes),
    );
    let result = crate::daemon::daemon_control_request(
        crate::automation::op::LUAU,
        None,
        json!({ "code": code, "edit": true, "bridgeWaitSeconds": 8.0, "bridgePorts": "8781,8782" }),
        false,
    )?;
    Ok(result
        .get("results")
        .and_then(Value::as_array)
        .and_then(|results| results.first())
        .cloned()
        .unwrap_or_else(|| json!([])))
}

/// One scoped push of the given instances of one store, without a review;
/// the summary is the push result.
pub(crate) fn push_stored_instances(
    settings_file: PathBuf,
    settings_ids: Vec<String>,
) -> Result<Value> {
    let mut args = crate::cli::PushEditorChangesArgs::new(
        crate::cli::ProjectSourceArgs::current(),
        crate::cli::BridgeConnectionArgs::default_connection(),
    );
    // A scoped push creates the instances from the store; a script's text
    // travels as its file, so the files of the pushed scripts go along.
    let mut changed_paths = vec![settings_file.clone()];
    if let Ok(document) = SettingsBytecode::read_file(&settings_file) {
        let service = bytecode_service_name(&document, &settings_file, "");
        let service_dir = crate::project::storage::source_directory(&settings_file);
        let source_paths = crate::editor::paths::build_editor_source_paths_by_index(
            &document,
            &service,
            &service_dir,
        );
        for (index, instance) in document.instances.iter().enumerate() {
            if settings_ids.contains(&instance.settings_id)
                && let Some(Some(path)) = source_paths.get(index)
                && path.is_file()
            {
                changed_paths.push(path.clone());
            }
        }
    }
    args.changed_paths = changed_paths;
    args.target_settings_ids = settings_ids;
    args.yes = true;
    args.verify_sources = true;
    crate::editor::sync::push_editor_changes_result(args)
        .context("The store was written but the push failed; push it with `rbx ps`")
}

/// A script file imported with `--path-json` into a project whose service is
/// store-backed gets its store instance too, so builds and pushes see it.
pub(crate) fn register_imported_script(
    loaded: &crate::project::config::LoadedProject,
    path_json: &str,
    destination: &Path,
) -> Result<Option<Value>> {
    let segments: Vec<String> =
        serde_json::from_str(path_json).context("--path-json must be a JSON string array")?;
    if segments.len() < 2 {
        return Ok(None);
    }
    let src_root = loaded.root.join(&loaded.project.source_root);
    let (settings_file, service_hint) = crate::bytecode::resolve_bytecode_settings_file(
        None,
        Some(&segments[0]),
        None,
        &loaded.root,
        &src_root,
    )?;
    if !settings_file.is_file() {
        return Ok(None);
    }
    crate::project::config::cache_script_naming(&loaded.root, &loaded.project);
    let naming = crate::project::config::project_script_naming(&loaded.project);
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .context("Import destination has no file name")?;
    let Some((class_name, stem, run_context)) =
        crate::editor::paths::infer_source_script(file_name, &naming)
    else {
        return Ok(None);
    };
    if stem.is_none() {
        bail!(
            "{file_name} names its parent instance; register that instance with `rbx ba` and its class, then import the file"
        );
    }
    let document = SettingsBytecode::read_file(&settings_file)?;
    let parent_segments = &segments[1..segments.len() - 1];
    let parent_index = store_index_at_segments(&document, parent_segments)?.with_context(|| {
        format!(
            "{} is not in the {} store",
            parent_segments.join("."),
            segments[0]
        )
    })?;
    let name = &segments[segments.len() - 1];
    let existing = document
        .instances
        .iter()
        .filter(|instance| instance.parent_index == Some(parent_index) && instance.name == *name)
        .collect::<Vec<_>>();
    match existing.as_slice() {
        [] => {}
        [instance] if is_lua_source_class(&instance.class_name) => return Ok(None),
        [instance] => bail!(
            "{} already exists in the store as a {}; remove it with `rbx br` or choose another name",
            segments.join("."),
            instance.class_name
        ),
        _ => bail!("{} names several store instances", segments.join(".")),
    }
    let mut properties = Vec::new();
    if class_name == "Script"
        && let Some(context) = run_context
        && context != "Legacy"
    {
        properties.push(format!("RunContext=\"{context}\""));
    }
    add_store_instance(StoreInstanceRequest {
        settings_file,
        service_hint,
        name: name.clone(),
        class_name: class_name.to_string(),
        settings_id: None,
        parent: StoreParentSelector {
            parent_index: Some(parent_index),
            ..StoreParentSelector::default()
        },
        properties,
        attributes: Vec::new(),
        // The import has written the script file by now; read-only links do
        // not guard `ip`.
        override_packages: true,
    })
    .map(Some)
}

/// The store instance a path below the service root names, by exact name at
/// each level; a level with several such instances is refused.
fn store_index_at_segments(
    document: &SettingsBytecode,
    segments: &[String],
) -> Result<Option<usize>> {
    let mut current = document
        .instances
        .iter()
        .position(|instance| instance.parent_index.is_none())
        .context("Store has no root")?;
    for segment in segments {
        let matches = document
            .instances
            .iter()
            .enumerate()
            .filter(|(_, instance)| {
                instance.parent_index == Some(current) && instance.name == *segment
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        current = match matches.as_slice() {
            [] => return Ok(None),
            [one] => *one,
            _ => bail!(
                "{segment} names {} instances in the store; register with `rbx ba -x INDEX`",
                matches.len()
            ),
        };
    }
    Ok(Some(current))
}

/// The property and attribute maps a new instance is stored with, and the
/// script source its file receives when one was given.
pub(crate) struct NewInstanceValues {
    pub(crate) properties: Map<String, Value>,
    pub(crate) attributes: Map<String, Value>,
    pub(crate) given_source: Option<String>,
}

/// Validates and canonicalizes the values a new instance is created with.
/// A script stores the external-source marker and keeps the given source for
/// its file. References are left as written; the caller completes them
/// against the stores it works on.
pub(crate) fn prepare_new_instance_values(
    class_name: &str,
    properties: Map<String, Value>,
    attributes: Map<String, Value>,
) -> Result<NewInstanceValues> {
    if class_name == "PackageLink" {
        bail!("PackageLink instances can only be created by Roblox package workflows");
    }
    ensure_creatable_class(class_name)?;
    let mut properties = canonical_new_instance_properties(class_name, properties)?;
    for name in attributes.keys() {
        super::ensure_settable_attribute_name(name)?;
    }
    let given_source = if is_lua_source_class(class_name) {
        match properties.get("Source") {
            Some(Value::String(source)) => Some(source.clone()),
            Some(_) => bail!("Source must be a string"),
            None => None,
        }
    } else {
        None
    };
    if is_lua_source_class(class_name) {
        properties.insert(
            "Source".to_string(),
            Value::String(EXTERNAL_SOURCE_MARKER.to_string()),
        );
    }
    Ok(NewInstanceValues {
        properties,
        attributes,
        given_source,
    })
}

pub(crate) fn add_store_instance(request: StoreInstanceRequest) -> Result<Value> {
    let StoreInstanceRequest {
        settings_file,
        service_hint,
        name,
        class_name,
        settings_id,
        parent,
        properties: raw_properties,
        attributes: raw_attributes,
        override_packages,
    } = request;
    ensure_service_store_exists(&settings_file, &service_hint)?;
    let _lock = acquire_settings_file_lock(&settings_file)?;
    let mut document = SettingsBytecode::read_file(&settings_file)?;
    let before_document = document.clone();
    let service = bytecode_service_name(&document, &settings_file, &service_hint);
    let service_directory = crate::project::storage::source_directory(&settings_file);
    let service_dir = service_directory.as_path();
    let source_paths_before = build_editor_source_paths_by_index(&document, &service, service_dir);
    let parent_index = bytecode_parent_index(
        &document,
        parent.no_parent,
        parent.parent_index,
        parent.parent_settings_id.as_deref(),
        parent.parent_name.as_deref(),
        parent.parent_class_name.as_deref(),
    )?;
    if let Some(parent_index) = parent_index {
        let links = super::store_link_enforcement(&settings_file, override_packages)?;
        super::reject_read_only_link_edit(&links, &document, &service, parent_index)?;
    }
    let NewInstanceValues {
        mut properties,
        mut attributes,
        given_source,
    } = prepare_new_instance_values(
        &class_name,
        parse_property_assignments(&raw_properties)?,
        parse_property_assignments(&raw_attributes)?,
    )?;
    let mut referenced = ReferencedStores::default();
    let mut to_itself = Vec::new();
    for (name, value) in properties.iter_mut() {
        if referenced.qualify(value, &settings_file, &document, MissingTarget::Defer)? {
            to_itself.push(name.clone());
        }
    }
    for value in attributes.values_mut() {
        referenced.qualify(value, &settings_file, &document, MissingTarget::Refuse)?;
    }
    let added = instance_api::add_instance(
        &mut document,
        AddInstanceSpec {
            settings_id,
            name,
            class_name,
            parent_index,
            properties,
            attributes,
        },
    )?;
    // A path that named nothing may name the new instance itself.
    for name in to_itself {
        let Some(mut value) = document.instances[added.index]
            .properties
            .get(&name)
            .cloned()
        else {
            continue;
        };
        referenced.qualify(&mut value, &settings_file, &document, MissingTarget::Refuse)?;
        document.instances[added.index]
            .properties
            .insert(name, value);
    }
    let mut writes = BTreeMap::new();
    let mut removals = Vec::new();
    let source_paths = collect_source_path_updates(
        &before_document,
        &source_paths_before,
        &document,
        &service,
        service_dir,
        &mut writes,
        &mut removals,
    )?;
    writes.insert(settings_file.clone(), encode_settings_bytecode(&document)?);
    let mut adopted_source = None;
    if is_lua_source_class(&document.instances[added.index].class_name)
        && let Some(Some(source_path)) = source_paths.get(added.index)
    {
        match new_script_source(source_path, given_source.as_deref().map(str::as_bytes))? {
            Some(bytes) => {
                writes.insert(source_path.clone(), bytes);
            }
            None => adopted_source = Some(source_path.clone()),
        }
    }
    let changed_paths = file_mutation_paths(&writes, &removals);
    let source_writes = changed_paths
        .iter()
        .filter(|path| *path != &settings_file)
        .cloned()
        .collect::<Vec<_>>();
    apply_file_mutations(&writes, &removals)?;
    let (path_segments_by_index, path_ordinals_by_index) =
        build_editor_instance_path_parts(&document, &service);
    let mut result = json!({
        "ok": true,
        "settingsFile": settings_file,
        "index": added.index,
        "settingsId": added.settings_id,
        "pathSegments": path_segments_by_index.get(added.index).and_then(std::clone::Clone::clone),
        "pathOrdinals": path_ordinals_by_index.get(added.index).and_then(std::clone::Clone::clone),
        "changedPaths": changed_paths,
        "sourceWrites": source_writes,
        "adoptedSource": adopted_source,
    });
    crate::app::output::drop_empty(&mut result, &["sourceWrites", "adoptedSource"]);
    Ok(result)
}

/// The bytes a new script's file receives, or None when a file already at
/// its path is adopted as the source. A file that exists is never replaced
/// by an empty source, and a given source that differs from it is refused.
pub(crate) fn new_script_source(
    source_path: &Path,
    given: Option<&[u8]>,
) -> Result<Option<Vec<u8>>> {
    let existing = source_path
        .is_file()
        .then(|| fs::read(source_path))
        .transpose()
        .with_context(|| format!("Failed to read {}", source_path.display()))?;
    match (existing, given) {
        (None, given) => Ok(Some(given.unwrap_or_default().to_vec())),
        (Some(_), None) => Ok(None),
        (Some(existing), Some(given)) if existing == given => Ok(None),
        (Some(_), Some(_)) => bail!(
            "{} already exists with different contents; omit the Source property to adopt the file, or change the file first",
            source_path.display()
        ),
    }
}

pub(crate) fn bytecode_clone_instance(args: BytecodeCloneInstanceArgs) -> Result<()> {
    let pretty = args.pretty;
    let result = bytecode_clone_instance_result(args)?;
    print_json_output(&result, pretty)
}

/// The clone as a result: `rootSettingsId` names the copy, `settingsIds`
/// every copied instance.
pub(crate) fn bytecode_clone_instance_result(mut args: BytecodeCloneInstanceArgs) -> Result<Value> {
    adopt_single_positional_target(
        &mut args.input,
        Some(args.service.as_str()),
        &mut args.selector,
    )?;
    let (settings_file, service_hint) = resolve_bytecode_cli_settings_file(
        args.input.settings_file.as_deref(),
        args.input.service_or_file.as_deref(),
        Some(args.service.as_str()),
    )?;
    let _lock = lock_existing_service_store(&settings_file)?;
    let mut document = SettingsBytecode::read_file(&settings_file)?;
    let before_document = document.clone();
    let service = bytecode_service_name(&document, &settings_file, &service_hint);
    let source_index = resolve_bytecode_selector(
        &document,
        &service,
        &args.selector,
        "Source instance was not found",
    )?
    .index;
    if document.instances[source_index].parent_index.is_none() {
        bail!("Service roots cannot be copied");
    }

    let parent_selector_specified = args.parent_index.is_some()
        || args
            .parent_settings_id
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        || args
            .parent_name
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        || args
            .parent_class_name
            .as_deref()
            .is_some_and(|value| !value.is_empty());
    let target_parent_index = if parent_selector_specified {
        bytecode_parent_index(
            &document,
            false,
            args.parent_index,
            args.parent_settings_id.as_deref(),
            args.parent_name.as_deref(),
            args.parent_class_name.as_deref(),
        )?
        .ok_or_else(|| anyhow::anyhow!("Parent instance was not found"))?
    } else {
        document.instances[source_index]
            .parent_index
            .ok_or_else(|| anyhow::anyhow!("Source instance has no parent"))?
    };
    let links = super::store_link_enforcement(&settings_file, args.override_packages)?;
    super::reject_read_only_link_edit(&links, &document, &service, target_parent_index)?;

    let service_dir = crate::project::storage::source_directory(&settings_file);
    let source_paths_before = build_editor_source_paths_by_index(&document, &service, &service_dir);
    let cloned = clone_store_subtree(
        &mut document,
        &service,
        source_index,
        target_parent_index,
        None,
        &HashSet::new(),
    )?;
    let cloned_settings_ids = cloned
        .copies
        .iter()
        .map(|&(_, new_index)| document.instances[new_index].settings_id.clone())
        .collect::<Vec<_>>();
    let root_settings_id = document.instances[cloned.root_index].settings_id.clone();

    let mut writes = BTreeMap::new();
    let mut removals = Vec::new();
    let mut source_paths_after = collect_source_path_updates(
        &before_document,
        &source_paths_before,
        &document,
        &service,
        &service_dir,
        &mut writes,
        &mut removals,
    )?;
    let mut source_copies = Vec::new();
    for &(old_index, new_index) in &cloned.copies {
        let Some(instance) = document.instances.get(old_index) else {
            continue;
        };
        if script_file_names(&instance.class_name).is_none() {
            continue;
        }
        let Some(Some(from)) = source_paths_before.get(old_index) else {
            continue;
        };
        let Some(Some(to)) = source_paths_after.get_mut(new_index) else {
            continue;
        };
        preserve_source_path_extension(from, to);
        if from == to || !from.exists() {
            continue;
        }
        let bytes = fs::read(from).with_context(|| format!("Failed to read {}", from.display()))?;
        super::check_new_source_file(to, &bytes)?;
        writes.insert(to.clone(), bytes);
        source_copies.push(json!({
            "from": from,
            "to": to,
        }));
    }
    writes.insert(settings_file.clone(), encode_settings_bytecode(&document)?);
    let changed_paths = file_mutation_paths(&writes, &removals);
    apply_file_mutations(&writes, &removals)?;
    let root_index = cloned.root_index;
    let root = &document.instances[root_index];
    let (path_segments_by_index, path_ordinals_by_index) =
        build_editor_instance_path_parts(&document, &service);

    Ok(json!({
        "ok": true,
        "settingsFile": settings_file,
        "service": service,
        "rootSettingsId": root_settings_id,
        "name": root.name,
        "className": root.class_name,
        "pathSegments": path_segments_by_index.get(root_index).and_then(std::clone::Clone::clone),
        "pathOrdinals": path_ordinals_by_index.get(root_index).and_then(std::clone::Clone::clone),
        "settingsIds": cloned_settings_ids,
        "sourceCopies": source_copies,
        "changedPaths": changed_paths,
    }))
}

/// A subtree copied inside its store: the copy's root and, in subtree
/// order, each original's index paired with its copy's.
pub(crate) struct ClonedSubtree {
    pub(crate) root_index: usize,
    pub(crate) copies: Vec<(usize, usize)>,
}

/// Copies the subtree at `source_index` under `parent_index` in memory. The
/// copy's root takes `name`, or a name unique among its new siblings; every
/// copy gets a fresh id that is not in `reserved`, scripts drop their stored
/// source, and references inside the subtree point at the copies.
pub(crate) fn clone_store_subtree(
    document: &mut SettingsBytecode,
    service: &str,
    source_index: usize,
    parent_index: usize,
    name: Option<&str>,
    reserved: &HashSet<String>,
) -> Result<ClonedSubtree> {
    let children_before = settings_children_by_parent(document);
    let mut source_subtree = Vec::new();
    collect_settings_subtree_preorder(&children_before, source_index, &mut source_subtree);
    reject_package_link_subtree_mutation(document, &source_subtree, "copied")?;
    let source_set = source_subtree.iter().copied().collect::<HashSet<_>>();
    if source_set.contains(&parent_index) {
        bail!("Cannot copy an instance into itself or one of its descendants");
    }
    let (path_segments_before, path_ordinals_before) =
        build_editor_instance_path_parts(document, service);
    let root_name = name.map_or_else(
        || {
            unique_editor_child_name(
                document,
                children_before
                    .get(parent_index)
                    .map_or(&[][..], Vec::as_slice),
                &document.instances[source_index].name,
            )
        },
        str::to_string,
    );
    let mut existing_settings_ids = document
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .chain(reserved.iter().cloned())
        .collect::<HashSet<_>>();
    let mut next_settings_id_seed = document.instances.len();
    let mut old_to_new_index = HashMap::<usize, usize>::with_capacity(source_subtree.len());
    let mut copies = Vec::with_capacity(source_subtree.len());
    for old_index in source_subtree.iter().copied() {
        let old_instance = document.instances[old_index].clone();
        let new_parent_index = if old_index == source_index {
            Some(parent_index)
        } else {
            Some(
                old_instance
                    .parent_index
                    .and_then(|parent_index| old_to_new_index.get(&parent_index).copied())
                    .ok_or_else(|| {
                        anyhow::anyhow!("Clone source subtree is missing a parent mapping")
                    })?,
            )
        };
        let settings_id =
            next_editor_settings_id_fast(&mut existing_settings_ids, &mut next_settings_id_seed);
        let new_index = document.instances.len();
        let name = if old_index == source_index {
            root_name.clone()
        } else {
            old_instance.name
        };
        let mut properties = old_instance.properties;
        properties.remove("Source");
        properties.remove("LinkedSource");
        document.instances.push(SettingsBytecodeInstance {
            settings_id,
            name,
            class_name: old_instance.class_name,
            parent_index: new_parent_index,
            properties,
            attributes: old_instance.attributes,
        });
        old_to_new_index.insert(old_index, new_index);
        copies.push((old_index, new_index));
    }
    let mut ref_map = build_clone_ref_map(
        document,
        CloneRefMapInput {
            source_subtree: &source_subtree,
            old_to_new_index: &old_to_new_index,
            path_segments_before: &path_segments_before,
            path_ordinals_before: &path_ordinals_before,
        },
    );
    ref_map.store_service = Some(service.to_string());
    for new_index in old_to_new_index.values().copied() {
        if let Some(instance) = document.instances.get_mut(new_index) {
            remap_internal_clone_refs_in_record(&mut instance.properties, &ref_map);
            remap_internal_clone_refs_in_record(&mut instance.attributes, &ref_map);
        }
    }
    Ok(ClonedSubtree {
        root_index: old_to_new_index[&source_index],
        copies,
    })
}

pub(crate) struct BytecodeCloneRefMap {
    /// The service of the store the instances are copied in. With it, a
    /// reference whose path leads into another service is not one to a
    /// copied instance, even when its settings id matches one: ids are
    /// unique within a store only.
    pub(crate) store_service: Option<String>,
    pub(crate) new_index_by_old: HashMap<usize, usize>,
    pub(crate) old_index_by_settings_id: HashMap<String, usize>,
    pub(crate) old_index_by_debug_id: HashMap<String, usize>,
    pub(crate) old_index_by_path_key: HashMap<String, Option<usize>>,
    pub(crate) old_index_by_path_parts_key: HashMap<String, usize>,
}

pub(crate) fn collect_settings_subtree_preorder(
    children_by_parent: &[Vec<usize>],
    index: usize,
    out: &mut Vec<usize>,
) {
    let mut stack = vec![index];
    let mut visited = HashSet::new();
    while let Some(current) = stack.pop() {
        if !visited.insert(current) {
            continue;
        }
        out.push(current);
        if let Some(children) = children_by_parent.get(current) {
            stack.extend(children.iter().rev().copied());
        }
    }
}

pub(crate) fn bytecode_service_name(
    document: &SettingsBytecode,
    settings_file: &Path,
    service: &str,
) -> String {
    if !service.is_empty() {
        return service.to_string();
    }
    document
        .instances
        .iter()
        .find(|instance| instance.parent_index.is_none())
        .map(|instance| instance.name.clone())
        .or_else(|| crate::project::storage::store_service_name(settings_file))
        .unwrap_or_default()
}

pub(crate) fn reject_package_link_instance_mutation(
    document: &SettingsBytecode,
    index: usize,
    action: &str,
) -> Result<()> {
    let instance = document
        .instances
        .get(index)
        .with_context(|| format!("Invalid instance index {index}"))?;
    if instance.class_name == "PackageLink" {
        bail!(
            "PackageLink instances cannot be {action}; use upl on the package root to remove its link"
        );
    }
    Ok(())
}

pub(crate) fn reject_package_link_subtree_mutation(
    document: &SettingsBytecode,
    indices: &[usize],
    action: &str,
) -> Result<()> {
    if indices.iter().any(|index| {
        document
            .instances
            .get(*index)
            .is_some_and(|instance| instance.class_name == "PackageLink")
    }) {
        bail!("Package-bearing instances cannot be {action}; use upl on the package root first");
    }
    Ok(())
}

pub(crate) fn unique_editor_child_name(
    document: &SettingsBytecode,
    child_indices: &[usize],
    requested: &str,
) -> String {
    let existing = child_indices
        .iter()
        .filter_map(|index| document.instances.get(*index))
        .map(|instance| instance.name.as_str())
        .collect::<HashSet<_>>();
    let base = if requested.trim().is_empty() {
        "Instance"
    } else {
        requested.trim()
    };
    if !existing.contains(base) {
        return base.to_string();
    }
    let mut suffix = 2usize;
    let mut candidate = format!("{base} Copy");
    while existing.contains(candidate.as_str()) {
        candidate = format!("{base} Copy {suffix}");
        suffix += 1;
    }
    candidate
}

pub(crate) fn next_editor_settings_id_fast(
    existing: &mut HashSet<String>,
    next_seed: &mut usize,
) -> String {
    loop {
        let candidate = format!("editor:{:x}", *next_seed);
        *next_seed += 1;
        if existing.insert(candidate.clone()) {
            return candidate;
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CloneRefMapInput<'a> {
    pub(crate) source_subtree: &'a [usize],
    pub(crate) old_to_new_index: &'a HashMap<usize, usize>,
    pub(crate) path_segments_before: &'a [Option<Vec<String>>],
    pub(crate) path_ordinals_before: &'a [Option<Vec<usize>>],
}

pub(crate) fn build_clone_ref_map(
    document: &SettingsBytecode,
    input: CloneRefMapInput<'_>,
) -> BytecodeCloneRefMap {
    let mut old_index_by_settings_id = HashMap::with_capacity(input.source_subtree.len());
    let mut old_index_by_debug_id = HashMap::new();
    let mut old_index_by_path_key = HashMap::with_capacity(input.source_subtree.len());
    let mut old_index_by_path_parts_key = HashMap::with_capacity(input.source_subtree.len());
    for old_index in input.source_subtree.iter().copied() {
        if let Some(instance) = document.instances.get(old_index) {
            old_index_by_settings_id.insert(instance.settings_id.clone(), old_index);
            if let Some(debug_id) = instance.settings_id.strip_prefix("debug:") {
                old_index_by_debug_id.insert(debug_id.to_string(), old_index);
            }
        }
        if let Some(Some(path_segments)) = input.path_segments_before.get(old_index) {
            let path_key = instance_path_key(path_segments);
            old_index_by_path_key
                .entry(path_key)
                .and_modify(|existing| *existing = None)
                .or_insert(Some(old_index));
            if let Some(Some(path_ordinals)) = input.path_ordinals_before.get(old_index) {
                old_index_by_path_parts_key.insert(
                    instance_path_parts_key(path_segments, path_ordinals),
                    old_index,
                );
            }
        }
    }
    BytecodeCloneRefMap {
        store_service: None,
        new_index_by_old: input.old_to_new_index.clone(),
        old_index_by_settings_id,
        old_index_by_debug_id,
        old_index_by_path_key,
        old_index_by_path_parts_key,
    }
}

pub(crate) fn remap_internal_clone_refs_in_record(
    record: &mut Map<String, Value>,
    refs: &BytecodeCloneRefMap,
) {
    for value in record.values_mut() {
        remap_internal_clone_refs(value, refs);
    }
}

fn remap_internal_clone_refs(value: &mut Value, refs: &BytecodeCloneRefMap) -> bool {
    match value {
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed = remap_internal_clone_refs(item, refs) || changed;
            }
            changed
        }
        Value::Object(object) => {
            if object.get("_type").and_then(Value::as_str) == Some("Ref") {
                return remap_ref_object(object, refs);
            }
            let mut changed = false;
            if let Some(Value::Object(ref_object)) = object.get_mut("Ref") {
                changed = remap_ref_object(ref_object, refs) || changed;
            }
            for nested in object.values_mut() {
                changed = remap_internal_clone_refs(nested, refs) || changed;
            }
            changed
        }
        _ => false,
    }
}

fn remap_ref_object(object: &mut Map<String, Value>, refs: &BytecodeCloneRefMap) -> bool {
    let Some(old_index) = ref_old_index(object, refs) else {
        return false;
    };
    let Some(new_index) = refs.new_index_by_old.get(&old_index).copied() else {
        return false;
    };
    for selector in SETTINGS_REFERENCE_SELECTOR_KEYS {
        object.remove(selector);
    }
    object.insert("instanceIndex".to_string(), json!((new_index + 1) as u64));
    true
}

pub(crate) fn ref_old_index(
    object: &Map<String, Value>,
    refs: &BytecodeCloneRefMap,
) -> Option<usize> {
    let path_service = object
        .get("pathSegments")
        .and_then(Value::as_array)
        .and_then(|segments| segments.first())
        .and_then(Value::as_str);
    if let (Some(path_service), Some(store_service)) = (path_service, &refs.store_service)
        && path_service != store_service
    {
        return None;
    }
    object
        .get("settingsId")
        .or_else(|| object.get("instanceId"))
        .and_then(Value::as_str)
        .and_then(|settings_id| refs.old_index_by_settings_id.get(settings_id).copied())
        .or_else(|| {
            object
                .get("instanceIndex")
                .and_then(settings_reference_index)
                .and_then(|index| refs.new_index_by_old.contains_key(&index).then_some(index))
        })
        .or_else(|| {
            object
                .get("debugId")
                .and_then(Value::as_str)
                .and_then(|debug_id| refs.old_index_by_debug_id.get(debug_id).copied())
        })
        .or_else(|| {
            let segments = object
                .get("pathSegments")
                .and_then(path_segments_from_value)?;
            if let Some(ordinals) = object
                .get("pathOrdinals")
                .and_then(path_ordinals_from_value)
            {
                return refs
                    .old_index_by_path_parts_key
                    .get(&instance_path_parts_key(&segments, &ordinals))
                    .copied();
            }
            refs.old_index_by_path_key
                .get(&instance_path_key(&segments))
                .copied()
                .flatten()
        })
}

pub(crate) fn strict_ref_old_index(
    object: &Map<String, Value>,
    refs: &BytecodeCloneRefMap,
) -> Result<Option<usize>> {
    let mut resolved = None;
    let mut accept = |selector: &str, candidate: Option<usize>| -> Result<()> {
        let candidate =
            candidate.with_context(|| format!("Ref {selector} does not identify an instance"))?;
        if let Some(existing) = resolved
            && existing != candidate
        {
            bail!("Ref selectors disagree at {selector}");
        }
        resolved = Some(candidate);
        Ok(())
    };

    if let Some(value) = object.get("instanceIndex") {
        accept(
            "instanceIndex",
            settings_reference_index(value)
                .filter(|index| refs.new_index_by_old.contains_key(index)),
        )?;
    }
    for selector in ["settingsId", "instanceId", "referent", "ref"] {
        if let Some(value) = object.get(selector) {
            let id = value
                .as_str()
                .with_context(|| format!("Ref {selector} must be a string"))?;
            accept(selector, refs.old_index_by_settings_id.get(id).copied())?;
        }
    }
    if let Some(value) = object.get("debugId") {
        let debug_id = value.as_str().context("Ref debugId must be a string")?;
        accept("debugId", refs.old_index_by_debug_id.get(debug_id).copied())?;
    }
    if let Some((segments, ordinals)) = strict_reference_path(object)? {
        let candidate = if let Some(ordinals) = ordinals {
            refs.old_index_by_path_parts_key
                .get(&instance_path_parts_key(&segments, &ordinals))
                .copied()
        } else {
            refs.old_index_by_path_key
                .get(&instance_path_key(&segments))
                .copied()
                .flatten()
        };
        accept("pathSegments", candidate)?;
    }
    if object.contains_key("path") {
        bail!("Ref path is unsupported; use pathSegments and pathOrdinals");
    }
    Ok(resolved)
}

pub(crate) fn validate_settings_model_internal_references(
    document: &SettingsBytecode,
    owner: &str,
) -> Result<()> {
    fn visit(value: &Value, refs: &BytecodeCloneRefMap, owner: &str, location: &str) -> Result<()> {
        match value {
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    visit(item, refs, owner, &format!("{location}[{}]", index + 1))?;
                }
            }
            Value::Object(object) => {
                if object.get("_type").and_then(Value::as_str) == Some("Ref") {
                    strict_ref_old_index(object, refs).with_context(|| {
                        format!(
                            "Model owner {owner} has an external or invalid reference at {location}"
                        )
                    })?;
                    return Ok(());
                }
                if let Some(reference) = object.get("Ref").and_then(Value::as_object) {
                    strict_ref_old_index(reference, refs).with_context(|| {
                        format!(
                            "Model owner {owner} has an external or invalid reference at {location}.Ref"
                        )
                    })?;
                }
                for (key, nested) in object {
                    if key == "Ref" {
                        continue;
                    }
                    visit(nested, refs, owner, &format!("{location}.{key}"))?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    let service = document
        .instances
        .iter()
        .find(|instance| instance.parent_index.is_none())
        .map_or("", |instance| instance.name.as_str());
    let (path_segments, path_ordinals) = build_editor_instance_path_parts(document, service);
    let indexes = (0..document.instances.len()).collect::<Vec<_>>();
    let identity = indexes
        .iter()
        .copied()
        .map(|index| (index, index))
        .collect::<HashMap<_, _>>();
    let refs = build_clone_ref_map(
        document,
        CloneRefMapInput {
            source_subtree: &indexes,
            old_to_new_index: &identity,
            path_segments_before: &path_segments,
            path_ordinals_before: &path_ordinals,
        },
    );
    for instance in &document.instances {
        for (name, value) in &instance.properties {
            visit(
                value,
                &refs,
                owner,
                &format!(
                    "{} ({}) property {name}",
                    instance.name, instance.class_name
                ),
            )?;
        }
        for (name, value) in &instance.attributes {
            visit(
                value,
                &refs,
                owner,
                &format!(
                    "{} ({}) attribute {name}",
                    instance.name, instance.class_name
                ),
            )?;
        }
    }
    Ok(())
}

pub(crate) fn path_segments_from_value(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|items| {
        items
            .iter()
            .filter_map(|item| item.as_str().map(ToString::to_string))
            .collect::<Vec<_>>()
    })
}

pub(crate) fn instance_path_key(path_segments: &[String]) -> String {
    path_segments.join("\0")
}

pub(crate) fn instance_path_parts_key(path_segments: &[String], path_ordinals: &[usize]) -> String {
    let mut key = instance_path_key(path_segments);
    key.push('\u{1}');
    for ordinal in path_ordinals {
        key.push_str(&ordinal.to_string());
        key.push('\0');
    }
    key
}

pub(crate) fn insert_unique_rbx_path(
    paths: &mut HashMap<String, Option<RbxRef>>,
    key: String,
    referent: RbxRef,
) {
    match paths.entry(key) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(Some(referent));
        }
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            if entry.get().is_some_and(|existing| existing != referent) {
                entry.insert(None);
            }
        }
    }
}

pub(crate) fn path_ordinals_from_value(value: &Value) -> Option<Vec<usize>> {
    value
        .as_array()?
        .iter()
        .map(|item| {
            item.as_u64()
                .filter(|ordinal| *ordinal > 0)
                .and_then(|ordinal| usize::try_from(ordinal).ok())
        })
        .collect()
}

pub(crate) fn prune_empty_source_dirs(service_dir: &Path, start: &Path) -> Result<()> {
    let mut current = start.to_path_buf();
    while current != service_dir && current.starts_with(service_dir) {
        let is_empty = match fs::read_dir(&current) {
            Ok(mut entries) => entries.next().is_none(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to inspect {}", current.display()));
            }
        };
        if !is_empty {
            break;
        }
        match fs::remove_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to remove empty {}", current.display()));
            }
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.to_path_buf();
    }
    Ok(())
}

pub(crate) fn prune_removed_source_dirs(root: &Path, removals: &[PathBuf]) {
    let mut directories = removals
        .iter()
        .filter_map(|path| path.parent().filter(|path| path.starts_with(root)))
        .collect::<Vec<_>>();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    directories.dedup();
    for directory in directories {
        let _ = prune_empty_source_dirs(root, directory);
    }
}

pub(crate) fn plan_editor_source_file_removals(
    service_dir: &Path,
    source_paths_by_index: &[Option<PathBuf>],
    removed_indexes: &[usize],
) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for index in removed_indexes {
        let Some(Some(source_path)) = source_paths_by_index.get(*index) else {
            continue;
        };
        ensure_existing_ancestor_inside(service_dir, source_path, "removed source file")?;
        if fs::symlink_metadata(source_path).is_ok() && !source_path.is_dir() {
            paths.push(source_path.clone());
        }
    }
    paths.sort_by_key(|path| exact_path_key(path));
    paths.dedup_by(|left, right| exact_path_key(left) == exact_path_key(right));
    Ok(paths)
}

pub(crate) fn bytecode_remove_instance(mut args: BytecodeRemoveInstanceArgs) -> Result<()> {
    adopt_single_positional_target(&mut args.input, None, &mut args.selector)?;
    let (settings_file, service_hint) = resolve_bytecode_cli_settings_file(
        args.input.settings_file.as_deref(),
        args.input.service_or_file.as_deref(),
        None,
    )?;
    let store_files = super::project_store_files(&settings_file)?;
    let locks = super::lock_existing_service_stores(store_files.iter().map(PathBuf::as_path))?;
    let mut document = SettingsBytecode::read_file(&settings_file)?;
    let before_document = document.clone();
    let service = bytecode_service_name(&document, &settings_file, &service_hint);
    let source_paths_by_index = build_editor_source_paths_by_index(
        &document,
        &service,
        &crate::project::storage::source_directory(&settings_file),
    );
    let (segments_before, _) = build_editor_instance_path_parts(&document, &service);
    let path_of = |index: usize| {
        segments_before
            .get(index)
            .cloned()
            .flatten()
            .unwrap_or_else(|| vec![document.instances[index].name.clone()])
    };
    let indices = if let Some(query) = args.where_query.as_deref() {
        let scope = super::resolve_bytecode_selector_or_service_root(
            &document,
            &service,
            &args.selector,
            "No matching instance",
        )?;
        let matched = super::where_matches(&document, &service, scope.index, query)?;
        if matched.is_empty() {
            bail!(
                "No instance under {} matches '{query}'",
                path_of(scope.index).join(".")
            );
        }
        if matched.len() > 500 {
            bail!(
                "{} instances under {} match '{query}'; narrow the query (500 at most per call)",
                matched.len(),
                path_of(scope.index).join(".")
            );
        }
        matched
    } else {
        let mut indices = vec![
            resolve_bytecode_selector(&document, &service, &args.selector, "No matching instance")?
                .index,
        ];
        for target in &args.more_targets {
            let selector = crate::cli::BytecodeInstanceSelectorArgs {
                target: Some(target.clone()),
                ..Default::default()
            };
            let index =
                resolve_bytecode_selector(&document, &service, &selector, "No matching instance")?
                    .index;
            if !indices.contains(&index) {
                indices.push(index);
            }
        }
        indices
    };
    let links = super::store_link_enforcement(&settings_file, args.override_packages)?;
    let children_by_parent = settings_children_by_parent(&document);
    for &index in &indices {
        super::reject_read_only_link_subtree_edit(&links, &document, &service, index)?;
        let mut subtree = Vec::new();
        collect_settings_subtree_preorder(&children_by_parent, index, &mut subtree);
        reject_package_link_subtree_mutation(&document, &subtree, "removed")?;
        if is_protected_engine_container(&document, index) {
            bail!("{} cannot be removed", document.instances[index].name);
        }
    }
    let removed_instance_paths = indices
        .iter()
        .map(|&index| path_of(index))
        .collect::<Vec<_>>();
    let removed =
        instance_api::remove_instances_at_indices(&mut document, &indices, !args.no_recursive)?;
    let removed_settings_ids = removed
        .iter()
        .filter_map(|index| before_document.instances.get(*index))
        .map(|instance| instance.settings_id.clone())
        .collect::<Vec<_>>();
    let removed_paths = removed
        .iter()
        .filter_map(|index| source_paths_by_index.get(*index).and_then(Option::as_ref))
        .filter(|path| path.is_file())
        .cloned()
        .collect::<Vec<_>>();
    let service_directory = crate::project::storage::source_directory(&settings_file);
    let service_dir = service_directory.as_path();
    let mut writes = BTreeMap::new();
    let mut removals = Vec::new();
    let other_stores = store_files
        .iter()
        .filter(|path| **path != settings_file)
        .cloned()
        .collect::<Vec<_>>();
    document = clear_references_to_removed(
        &settings_file,
        &before_document,
        document,
        &other_stores,
        &removed_instance_paths,
        &mut writes,
    )?;
    collect_source_path_updates(
        &before_document,
        &source_paths_by_index,
        &document,
        &service,
        service_dir,
        &mut writes,
        &mut removals,
    )?;
    removals.extend(removed_paths);
    removals.retain(|path| !writes.keys().any(|write| path_key(write) == path_key(path)));
    removals.sort_by_key(|path| path_key(path));
    removals.dedup_by(|left, right| path_key(left) == path_key(right));
    let removed_source_paths = removals.clone();
    let removed_settings_file = document.instances.is_empty();
    if removed_settings_file {
        removals.push(settings_file.clone());
    } else {
        writes.insert(settings_file.clone(), encode_settings_bytecode(&document)?);
    }
    let changed_paths = file_mutation_paths(&writes, &removals);
    apply_file_mutations(&writes, &removals)?;
    drop(locks);
    let removed_source_paths_json = removed_source_paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    {
        prune_removed_source_dirs(
            &crate::project::storage::source_directory(&settings_file),
            &removed_source_paths,
        );
        if removed_settings_file && let Some(source_root) = service_dir.parent() {
            prune_empty_source_dirs(source_root, service_dir)?;
        }
    }
    let mut result = json!({
        "ok": true,
        "settingsFile": settings_file,
        "storeRemoved": removed_settings_file,
        "removed": removed_instance_paths.iter().map(|path| path.join(".")).collect::<Vec<_>>(),
        "removedSettingsIds": removed_settings_ids,
        "removedSourcePaths": removed_source_paths_json,
        "changedPaths": changed_paths,
    });
    if let Some(query) = args.where_query.as_deref() {
        result["where"] = json!(query);
        result["matched"] = json!(removed_instance_paths.len());
    }
    if args.push {
        let studio = destroy_in_studio(&removed_instance_paths)?;
        let destroyed = studio.get("destroyed").and_then(Value::as_u64).unwrap_or(0);
        let missing = studio
            .get("missing")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty);
        result["verified"] = json!(destroyed as usize == removed_instance_paths.len() && missing);
        result["studio"] = studio;
    }
    crate::app::output::drop_empty(&mut result, &["removedSourcePaths"]);
    print_json_output(&result, args.pretty)
}

/// After instances were removed from the store at `settings_file`, clears the
/// references the project's other stores hold to them and renumbers those to
/// same-named siblings whose ordinal the removal shifted. `before` and
/// `after` are the store's document around the removal; `after` is returned
/// with its own references brought up to date. A reference carries its
/// target's path, so only the stores that hold every name of a removed path
/// are read, and those that change are added to `writes`.
fn clear_references_to_removed(
    settings_file: &Path,
    before: &SettingsBytecode,
    after: SettingsBytecode,
    other_stores: &[PathBuf],
    removed_paths: &[Vec<String>],
    writes: &mut BTreeMap<PathBuf, Vec<u8>>,
) -> Result<SettingsBytecode> {
    let mut files = BTreeMap::new();
    let mut documents = BTreeMap::new();
    for path in other_stores {
        if let Some(document) =
            SettingsBytecode::read_file_if_contains_any_string_set(path, removed_paths)?
        {
            let service = crate::project::storage::store_service_name(path)
                .context("Service settings path has no service name")?;
            files.insert(service.clone(), path);
            documents.insert(service, document);
        }
    }
    if documents.is_empty() {
        return Ok(after);
    }
    let own_service = crate::project::storage::store_service_name(settings_file)
        .context("Service settings path has no service name")?;
    documents.insert(own_service.clone(), after);
    let changed = super::references::follow_edited_references(
        &BTreeMap::from([(own_service.clone(), before)]),
        &mut documents,
    );
    for service in changed {
        if let Some(path) = files.get(&service) {
            writes.insert(
                (*path).clone(),
                encode_settings_bytecode(&documents[&service])?,
            );
        }
    }
    documents
        .remove(&own_service)
        .context("The edited store left the reference update")
}

/// Renames the instances at the given paths in the open place as one undo
/// step, so the push that follows pairs them under their new names;
/// `renamed` counts them and `missing` lists the paths Studio did not have.
pub(crate) fn rename_in_studio(pairs: &[(Vec<String>, String)]) -> Result<Value> {
    let pairs = pairs
        .iter()
        .map(|(path, name)| format!("{{ {}, {name:?} }}", luau_string_list(path)))
        .collect::<Vec<_>>()
        .join(", ");
    let code = format!(
        "local function at(segments) local inst = game for _, name in segments do inst = inst:FindFirstChild(name) if not inst then return nil end end return inst end local renamed, missing = 0, {{}} for _, pair in {{{pairs}}} do local inst = at(pair[1]) if inst then if inst.Name ~= pair[2] then inst.Name = pair[2] end renamed += 1 else table.insert(missing, table.concat(pair[1], '.')) end end return {{ renamed = renamed, missing = missing }}"
    );
    let result = crate::daemon::daemon_control_request(
        crate::automation::op::LUAU,
        None,
        json!({ "code": code, "edit": true, "bridgeWaitSeconds": 8.0, "bridgePorts": "8781,8782" }),
        false,
    )
    .context("The store was renamed but Studio could not rename the instance; push the store with `rbx ps`")?;
    Ok(result
        .get("results")
        .and_then(Value::as_array)
        .and_then(|results| results.first())
        .cloned()
        .unwrap_or(Value::Null))
}

/// Destroys the instances at `paths` in the open place as one undo step;
/// `destroyed` counts them and `missing` lists the paths Studio no longer had.
fn destroy_in_studio(paths: &[Vec<String>]) -> Result<Value> {
    let paths = paths
        .iter()
        .map(|path| luau_string_list(path))
        .collect::<Vec<_>>()
        .join(", ");
    let code = format!(
        "local function at(segments) local inst = game for _, name in segments do inst = inst:FindFirstChild(name) if not inst then return nil end end return inst end local destroyed, missing = 0, {{}} for _, segments in {{{paths}}} do local inst = at(segments) if inst then inst:Destroy() destroyed += 1 else table.insert(missing, table.concat(segments, '.')) end end return {{ destroyed = destroyed, missing = missing }}"
    );
    let result = crate::daemon::daemon_control_request(
        crate::automation::op::LUAU,
        None,
        json!({ "code": code, "edit": true, "bridgeWaitSeconds": 8.0, "bridgePorts": "8781,8782" }),
        false,
    )
    .context("The store was updated but Studio could not remove the instances; push the store with `rbx ps`")?;
    Ok(result
        .get("results")
        .and_then(Value::as_array)
        .and_then(|results| results.first())
        .cloned()
        .unwrap_or(Value::Null))
}

pub(crate) fn bytecode_desync_package_link(mut args: BytecodeDesyncPackageLinkArgs) -> Result<()> {
    adopt_single_positional_target(
        &mut args.input,
        Some(args.service.as_str()),
        &mut args.selector,
    )?;
    let explicit_service = (!args.service.trim().is_empty()).then_some(args.service.as_str());
    let (settings_file, service_hint) = resolve_bytecode_cli_settings_file(
        args.input.settings_file.as_deref(),
        args.input.service_or_file.as_deref(),
        explicit_service,
    )?;
    let _lock = lock_existing_service_store(&settings_file)?;
    let mut document = SettingsBytecode::read_file(&settings_file)?;
    let before_document = document.clone();
    let service = bytecode_service_name(&document, &settings_file, &service_hint);
    let service_directory = crate::project::storage::source_directory(&settings_file);
    let service_dir = service_directory.as_path();
    let source_paths_before = build_editor_source_paths_by_index(&document, &service, service_dir);
    let target_index =
        resolve_bytecode_selector(&document, &service, &args.selector, "No matching instance")?
            .index;
    let path_segments_by_index = build_editor_instance_paths(&document, &service)
        .into_iter()
        .map(|path| path.map(|path| path.path_segments))
        .collect::<Vec<_>>();
    let children_by_parent = settings_children_by_parent(&document);
    let package_link_indices =
        package_link_indices_for_desync(&document, &children_by_parent, target_index)?;
    let removed_package_links = package_link_indices
        .iter()
        .map(|index| {
            let instance = &document.instances[*index];
            json!({
                "index": *index,
                "settingsId": instance.settings_id.clone(),
                "name": instance.name.clone(),
                "className": instance.class_name.clone(),
                "pathSegments": path_segments_by_index.get(*index).and_then(std::clone::Clone::clone),
            })
        })
        .collect::<Vec<_>>();
    let target_path = path_segments_by_index
        .get(target_index)
        .and_then(std::clone::Clone::clone);
    let removed =
        instance_api::remove_instances_at_indices(&mut document, &package_link_indices, true)?;
    let mut writes = BTreeMap::new();
    let mut removals = Vec::new();
    collect_source_path_updates(
        &before_document,
        &source_paths_before,
        &document,
        &service,
        service_dir,
        &mut writes,
        &mut removals,
    )?;
    writes.insert(settings_file.clone(), encode_settings_bytecode(&document)?);
    let changed_paths = file_mutation_paths(&writes, &removals);
    apply_file_mutations(&writes, &removals)?;
    prune_removed_source_dirs(service_dir, &removals);
    print_json_output(
        &json!({
            "ok": true,
            "settingsFile": settings_file,
            "service": service,
            "targetIndex": target_index,
            "targetPathSegments": target_path,
            "removedIndexes": removed,
            "removedPackageLinks": removed_package_links,
            "changedPaths": changed_paths,
        }),
        args.pretty,
    )
}

fn package_link_indices_for_desync(
    document: &SettingsBytecode,
    children_by_parent: &[Vec<usize>],
    target_index: usize,
) -> Result<Vec<usize>> {
    let target = document
        .instances
        .get(target_index)
        .ok_or_else(|| anyhow::anyhow!("Invalid target index {target_index}"))?;
    if target.class_name == "PackageLink" {
        return Ok(vec![target_index]);
    }
    let links = children_by_parent
        .get(target_index)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .copied()
        .filter(|index| {
            document
                .instances
                .get(*index)
                .is_some_and(|instance| instance.class_name == "PackageLink")
        })
        .collect::<Vec<_>>();
    if links.is_empty() {
        bail!(
            "{} has no direct PackageLink child",
            path_segments_for_error(document, target_index)
        );
    }
    Ok(links)
}

pub(crate) fn has_direct_package_link_child(
    document: &SettingsBytecode,
    children_by_parent: &[Vec<usize>],
    target_index: usize,
) -> bool {
    children_by_parent
        .get(target_index)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .any(|index| {
            document
                .instances
                .get(*index)
                .is_some_and(|instance| instance.class_name == "PackageLink")
        })
}

fn path_segments_for_error(document: &SettingsBytecode, index: usize) -> String {
    let Some(instance) = document.instances.get(index) else {
        return format!("#{index}");
    };
    let mut names = vec![instance.name.clone()];
    let mut current = instance.parent_index;
    while let Some(parent_index) = current {
        let Some(parent) = document.instances.get(parent_index) else {
            break;
        };
        names.push(parent.name.clone());
        current = parent.parent_index;
    }
    names.reverse();
    names.join(".")
}

#[cfg(test)]
mod read_back_tests {
    use super::*;

    fn stored() -> SettingsBytecodeInstance {
        let mut instance =
            SettingsBytecodeInstance::new("id".into(), "Pad".into(), "Part".into(), None);
        instance.properties.insert("Anchored".into(), json!(true));
        instance
            .properties
            .insert("Transparency".into(), json!(0.5));
        instance.properties.insert(
            "Color".into(),
            json!({"_type": "Color3", "r": 0.2, "g": 1.0, "b": 0.3}),
        );
        instance.properties.insert(
            "Material".into(),
            json!({"_type": "EnumItem", "enumType": "Material", "name": "Neon"}),
        );
        instance.properties.insert(
            "CFrame".into(),
            json!({"_type": "CFrame", "components": [30.0, 0.5, 0.0, 1, 0, 0, 0, 1, 0, 0, 0, 1]}),
        );
        instance.attributes.insert("MaxSpeed".into(), json!(16));
        instance
    }

    fn row(material: &str, transparency: f64) -> Value {
        json!({
            "path": "Workspace.Lobby.Pad",
            "class": "Part",
            "Anchored": true,
            "Transparency": transparency,
            "Color": {"_type": "Color3", "r": 0.2, "g": 1, "b": 0.30000001},
            "Material": {"_type": "EnumItem", "value": format!("Enum.Material.{material}")},
            "CFrame": {"_type": "CFrame", "position": [30, 0.5, 0]},
            "@MaxSpeed": 16,
        })
    }

    #[test]
    fn a_read_back_verifies_the_pushed_values_and_names_a_value_studio_did_not_take() {
        let path = ["Workspace", "Lobby", "Pad"].map(String::from);
        let names = ["Anchored", "Transparency", "Color", "Material"].map(String::from);
        let attributes = ["MaxSpeed".to_string()];
        let stored = stored();
        assert_eq!(
            read_back_verifies(&row("Neon", 0.5), &path, &stored, &names, &attributes),
            Some(true)
        );
        assert_eq!(
            read_back_verifies(&row("Plastic", 0.5), &path, &stored, &names, &attributes),
            Some(false),
            "an enum Studio kept at its old value is a mismatch"
        );
        assert_eq!(
            read_back_verifies(&row("Neon", 0.0), &path, &stored, &names, &attributes),
            Some(false)
        );
        assert_eq!(
            read_back_verifies(
                &row("Neon", 0.5),
                &path,
                &stored,
                &["CFrame".to_string()],
                &[]
            ),
            None,
            "a live shape that does not line up with the stored one is not judged"
        );
        assert_eq!(
            read_back_verifies(
                &row("Neon", 0.5),
                &path,
                &stored,
                &["Parent".to_string()],
                &[]
            ),
            Some(true),
            "a property the store does not hold is not compared"
        );
        let missing = json!({"missing": true, "path": "Workspace.Lobby.Pad"});
        assert_eq!(
            read_back_verifies(&missing, &path, &stored, &names, &attributes),
            Some(false)
        );
        let moved = json!({"path": "Workspace.Props.Pad", "class": "Part"});
        assert_eq!(
            read_back_verifies(&moved, &path, &stored, &[], &[]),
            Some(false)
        );
    }
}

#[cfg(test)]
mod add_instance_tests {
    use std::path::PathBuf;

    use super::*;
    use crate::cli::{BytecodeAddInstanceArgs, BytecodeFileArgs, BytecodeParentArgs};

    #[test]
    fn non_creatable_classes_are_refused_before_a_store_is_touched() {
        for class_name in ["Player", "Terrain", "Workspace", "Players"] {
            let error = ensure_creatable_class(class_name).unwrap_err().to_string();
            assert!(error.contains("cannot be created"), "{class_name}: {error}");
        }
        for class_name in ["Folder", "Part", "Script", "NotAKnownClassYet"] {
            ensure_creatable_class(class_name).unwrap();
        }
    }

    fn add(root: &Path, name: &str, properties: Vec<String>) -> Result<Value> {
        crate::app::output::capture_json_output(|| {
            bytecode_add_instance(BytecodeAddInstanceArgs {
                input: BytecodeFileArgs {
                    service_or_file: None,
                    settings_file: Some(root.join("instances").join("ServerScriptService.renium")),
                },
                service_hint: Some("ServerScriptService".into()),
                name: Some(name.into()),
                class_name: Some("ModuleScript".into()),
                settings_id: None,
                batch: None,
                push: false,
                parent: BytecodeParentArgs {
                    parent_index: None,
                    parent_settings_id: None,
                    parent_name: None,
                    parent_class_name: None,
                    no_parent: true,
                },
                properties,
                attributes: Vec::new(),
                override_packages: false,
                pretty: false,
            })
        })
    }

    #[test]
    fn adding_a_script_adopts_an_existing_source_file() {
        let root = crate::system::files::create_unique_directory(
            &std::env::temp_dir(),
            "renium-ba-adopt-",
        )
        .unwrap();
        let service_dir = crate::project::storage::source_directory(
            &root.join("instances").join("ServerScriptService.renium"),
        );
        fs::create_dir_all(&service_dir).unwrap();
        let file = service_dir.join("RaceRounds.luau");
        fs::write(&file, "return { rounds = 3 }\n").unwrap();
        let result = add(&root, "RaceRounds", Vec::new()).unwrap();
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "return { rounds = 3 }\n"
        );
        assert!(
            result["adoptedSource"]
                .as_str()
                .is_some_and(|path| path.ends_with("RaceRounds.luau")),
            "{result}"
        );
        let other = service_dir.join("Other.luau");
        fs::write(&other, "return 9").unwrap();
        let refused = add(&root, "Other", vec!["Source=\"return 1\"".into()]);
        assert!(refused.is_err() || refused.unwrap()["ok"] == false);
        assert_eq!(fs::read_to_string(&other).unwrap(), "return 9");
        let created = add(&root, "Fresh", vec!["Source=\"return 2\"".into()]).unwrap();
        assert_eq!(created["ok"], true, "{created}");
        assert_eq!(
            fs::read_to_string(service_dir.join("Fresh.luau")).unwrap(),
            "return 2"
        );
        let _ = fs::remove_dir_all(&root);
        let _ = PathBuf::new();
    }
}
