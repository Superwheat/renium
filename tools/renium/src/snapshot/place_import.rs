use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Cursor};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rbx_binary::{FlatDom, FlatInstance};
use rbx_dom_weak::types::Ref as RbxRef;
use rbx_reflection::ReflectionDatabase;
use serde_json::{Value, json};

use crate::app::output::print_json_output;
use crate::app::timing::set_quiet_timings;
use crate::cli::ImportPlaceArgs;
use crate::project::config;
use crate::project::layout::apply_configured_project_layout;
use crate::rbx::decode::{
    NativePropertyFilter, native_property_filter, rbx_properties_to_native_settings_records,
};
use crate::rbx::model::{BytecodeModelImportRefs, RbxPlaceFormat};
use crate::snapshot::export::{
    ExportExecutionSetup, finish_export_import, finish_export_publication, prepare_export_execution,
};
use crate::snapshot::import::parse_services;
use crate::snapshot::types::{
    ExportedSnapshotParts, NativeSettingsProperty, NativeSettingsValue, SnapshotInstance,
};
use crate::system::files::resolve_existing_project_root;

/// Imports a saved place file into the project the same way a pull imports
/// Studio's binary export, without a Studio session.
pub(crate) fn import_place_file(mut args: ImportPlaceArgs) -> Result<()> {
    crate::project::layout::ensure_explicit_project_root(&args.project_root)?;
    apply_configured_project_layout(&mut args.project_root, &mut args.src_dir)?;
    set_quiet_timings(true);
    let total_started = Instant::now();
    let project_root = resolve_existing_project_root(&args.project_root)?;
    config::validate_relative_portable_path(&args.src_dir, "srcDir")?;
    let services = parse_services(&args.services)?;
    let flat = read_place(&args.input, &services)?;
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let paths = PlacePaths::new(&flat);
    let subtrees = service_subtrees(&flat);
    let ExportExecutionSetup {
        project_stage,
        sourcemap_writer,
        direct_import_dispatcher,
        export_services,
    } = prepare_export_execution(&args.src_dir, &project_root, &services, total_started)?;
    let mut filters = HashMap::new();
    let mut imported = 0usize;
    for service in &export_services {
        direct_import_dispatcher.check_error()?;
        let parts = match subtrees.get(service) {
            Some(indices) => {
                imported += indices.len() - 1;
                convert_service(
                    service,
                    &flat.instances,
                    indices,
                    &paths,
                    database,
                    &mut filters,
                )?
            }
            None => empty_service_parts(service),
        };
        direct_import_dispatcher.enqueue_parts(service, parts)?;
    }
    finish_export_import(direct_import_dispatcher, sourcemap_writer)?;
    let (published, _) = finish_export_publication(
        Some(project_stage),
        &project_root,
        false,
        || Ok(()),
        || Ok(()),
    )?;
    print_json_output(
        &json!({
            "ok": true,
            "services": export_services.len(),
            "instances": imported,
            "changedPaths": published
                .changed_roots
                .iter()
                .map(|path| path.to_string_lossy().replace('\\', "/"))
                .collect::<Vec<_>>(),
        }),
        false,
    )
}

fn read_place(path: &Path, services: &[String]) -> Result<FlatDom> {
    let format = RbxPlaceFormat::from_path(path)?;
    let bytes = match format {
        RbxPlaceFormat::Binary => {
            std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?
        }
        RbxPlaceFormat::Xml => {
            let file =
                File::open(path).with_context(|| format!("Failed to read {}", path.display()))?;
            let dom = rbx_xml::from_reader(
                BufReader::new(file),
                rbx_xml::DecodeOptions::new()
                    .property_behavior(rbx_xml::DecodePropertyBehavior::ReadUnknown),
            )
            .with_context(|| format!("Failed to read {}", path.display()))?;
            let mut bytes = Vec::new();
            rbx_binary::to_writer(&mut bytes, &dom, dom.root().children())
                .with_context(|| format!("Failed to convert {}", path.display()))?;
            bytes
        }
    };
    let flat = rbx_binary::Deserializer::new()
        .elide_defaults(true)
        .retain_defaults_for_classes(services.iter().cloned().collect())
        .deserialize_flat(Cursor::new(bytes))
        .with_context(|| format!("{} is not a valid place file", path.display()))?;
    if flat.root_indices.is_empty() {
        bail!("{} contains no services", path.display());
    }
    Ok(flat)
}

struct PlacePaths {
    segments: Arc<HashMap<RbxRef, Vec<String>>>,
    ordinals: Arc<HashMap<RbxRef, Vec<usize>>>,
}

fn children_by_index(flat: &FlatDom) -> Vec<Vec<usize>> {
    let mut children = vec![Vec::new(); flat.instances.len()];
    for (index, instance) in flat.instances.iter().enumerate() {
        if let Some(parent) = instance.parent_index {
            children[parent].push(index);
        }
    }
    children
}

impl PlacePaths {
    fn new(flat: &FlatDom) -> Self {
        let children = children_by_index(flat);
        let mut segments = HashMap::with_capacity(flat.instances.len());
        let mut ordinals = HashMap::with_capacity(flat.instances.len());
        let mut stack = flat
            .root_indices
            .iter()
            .rev()
            .map(|&index| (index, Vec::new(), Vec::new(), 1usize))
            .collect::<Vec<_>>();
        while let Some((index, parent_segments, parent_ordinals, ordinal)) = stack.pop() {
            let instance = &flat.instances[index];
            let mut path = parent_segments;
            path.push(instance.name.clone());
            let mut ords = parent_ordinals;
            ords.push(ordinal);
            let mut counts = HashMap::<&str, usize>::new();
            let mut ordered = Vec::with_capacity(children[index].len());
            for &child in &children[index] {
                let count = counts
                    .entry(flat.instances[child].name.as_str())
                    .or_default();
                *count += 1;
                ordered.push((child, path.clone(), ords.clone(), *count));
            }
            stack.extend(ordered.into_iter().rev());
            segments.insert(instance.referent, path);
            ordinals.insert(instance.referent, ords);
        }
        Self {
            segments: Arc::new(segments),
            ordinals: Arc::new(ordinals),
        }
    }
}

/// Each service's instances in parent-before-child order, starting with the
/// service itself.
fn service_subtrees(flat: &FlatDom) -> HashMap<String, Vec<usize>> {
    let children = children_by_index(flat);
    let mut subtrees = HashMap::new();
    for &root in &flat.root_indices {
        let mut order = Vec::new();
        let mut stack = vec![root];
        while let Some(index) = stack.pop() {
            order.push(index);
            stack.extend(children[index].iter().rev());
        }
        subtrees.insert(flat.instances[root].class.to_string(), order);
    }
    subtrees
}

fn clock_time_from_time_of_day(text: &str) -> Option<f64> {
    let mut parts = text.split(':').map(|part| part.trim().parse::<f64>().ok());
    let hours = parts.next()??;
    let minutes = parts.next().flatten().unwrap_or(0.0);
    let seconds = parts.next().flatten().unwrap_or(0.0);
    if parts.next().is_some() {
        return None;
    }
    let clock = (hours + minutes / 60.0 + seconds / 3600.0) as f32;
    clock.is_finite().then_some(f64::from(clock))
}

fn empty_service_parts(service: &str) -> ExportedSnapshotParts {
    ExportedSnapshotParts {
        class_defaults: Value::Object(Default::default()),
        instances: vec![SnapshotInstance {
            name: service.to_string(),
            class_name: service.into(),
            instance_index: Some(1),
            ..Default::default()
        }],
        native_properties_by_instance: Some(vec![Vec::new()]),
    }
}

fn external_targets<T: Clone>(
    map: &HashMap<RbxRef, T>,
    local: &HashMap<RbxRef, usize>,
) -> HashMap<RbxRef, T> {
    map.iter()
        .filter(|(referent, _)| !local.contains_key(*referent))
        .map(|(referent, value)| (*referent, value.clone()))
        .collect()
}

fn convert_service(
    service: &str,
    instances: &[FlatInstance],
    indices: &[usize],
    paths: &PlacePaths,
    database: &ReflectionDatabase<'_>,
    filters: &mut HashMap<String, NativePropertyFilter>,
) -> Result<ExportedSnapshotParts> {
    let local_by_global = indices
        .iter()
        .enumerate()
        .map(|(local, &global)| (global, local))
        .collect::<HashMap<_, _>>();
    let new_index_by_ref = indices
        .iter()
        .enumerate()
        .map(|(local, &global)| (instances[global].referent, local))
        .collect::<HashMap<_, _>>();
    let refs = BytecodeModelImportRefs {
        path_segments_by_ref: Arc::new(external_targets(&paths.segments, &new_index_by_ref)),
        path_ordinals_by_ref: Arc::new(external_targets(&paths.ordinals, &new_index_by_ref)),
        new_index_by_ref,
        ..Default::default()
    };
    let mut converted = Vec::with_capacity(indices.len());
    let mut native_by_instance = Vec::with_capacity(indices.len());
    for (local, &global) in indices.iter().enumerate() {
        let instance = &instances[global];
        let class_name = instance.class.as_str();
        let filter = filters
            .entry(class_name.to_string())
            .or_insert_with(|| native_property_filter(database, class_name));
        let properties = instance
            .properties
            .iter()
            .map(|(name, value)| (name, value));
        let (mut native_properties, mut properties, mut attributes, source) =
            rbx_properties_to_native_settings_records(
                class_name,
                properties,
                database,
                &refs,
                Some(filter),
            );
        if filter.reconstruct_decal_color_map {
            let value = properties
                .get("TextureContent")
                .cloned()
                .unwrap_or_else(|| Value::String(String::new()));
            properties.insert("ColorMapContent".to_string(), value);
        }
        if filter.reconstruct_weld_enabled
            && let Some(raw_state) = properties.remove("State")
        {
            let state = raw_state
                .as_i64()
                .context("WeldConstraint.State was not an integer")?;
            native_properties.push(NativeSettingsProperty {
                name: "State".to_string(),
                value: NativeSettingsValue::Int(state),
            });
            if state == 0 {
                properties.insert("Enabled".to_string(), Value::Bool(false));
            }
        }
        if let Some(source) = source {
            properties.insert("Source".to_string(), Value::String(source));
        }
        properties.retain(|name, _| {
            !crate::editor::review::is_engine_managed_editor_property(class_name, name, database)
        });
        if local == 0 {
            properties.retain(|name, _| {
                !crate::rbx::decode::is_unexposed_service_property(database, class_name, name)
            });
            if class_name == "Lighting"
                && !properties.contains_key("ClockTime")
                && let Some(clock_time) = properties
                    .get("TimeOfDay")
                    .and_then(Value::as_str)
                    .and_then(clock_time_from_time_of_day)
            {
                properties.insert("ClockTime".to_string(), json!(clock_time));
            }
            native_properties.retain(|property| {
                !crate::rbx::decode::is_unexposed_service_property(
                    database,
                    class_name,
                    &property.name,
                )
            });
            properties.insert("Archivable".to_string(), Value::Bool(true));
            attributes.retain(|name, _| !name.starts_with("RBX"));
        }
        let parent_index = instance
            .parent_index
            .and_then(|parent| local_by_global.get(&parent))
            .map(|parent| parent + 1);
        converted.push(SnapshotInstance {
            name: if local == 0 {
                service.to_string()
            } else {
                instance.name.clone()
            },
            class_name: instance.class,
            properties,
            attributes,
            instance_index: Some(local + 1),
            parent_index,
            ..Default::default()
        });
        native_by_instance.push(native_properties);
    }
    Ok(ExportedSnapshotParts {
        class_defaults: Value::Object(Default::default()),
        instances: converted,
        native_properties_by_instance: Some(native_by_instance),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rbx_dom_weak::{InstanceBuilder, WeakDom};

    use super::*;
    use crate::app::output::capture_json_output;
    use crate::settings::bytecode::SettingsBytecode;
    use crate::system::files::create_unique_directory;

    fn run(root: &Path) -> Value {
        capture_json_output(|| {
            import_place_file(ImportPlaceArgs {
                input: root.join("place.rbxl"),
                project_root: root.to_path_buf(),
                src_dir: PathBuf::from("src"),
                services: String::new(),
            })
        })
        .expect("place import")
    }

    #[test]
    fn clock_time_derives_from_time_of_day() {
        assert_eq!(
            clock_time_from_time_of_day("10:36:00"),
            Some(f64::from(10.6f32))
        );
        assert_eq!(clock_time_from_time_of_day("14:00:00"), Some(14.0));
        assert_eq!(clock_time_from_time_of_day("00:30"), Some(0.5));
        assert_eq!(clock_time_from_time_of_day("noon"), None);
    }

    #[test]
    fn place_import_writes_scripts_stores_and_references_once() {
        let root = create_unique_directory(&std::env::temp_dir(), "renium-place-import-")
            .expect("temp project");
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let workspace = dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("Workspace").with_property("SourceAssetId", 5i64),
        );
        let door = dom.insert(
            workspace,
            InstanceBuilder::new("Part")
                .with_name("Door")
                .with_property("Anchored", true),
        );
        dom.insert(
            workspace,
            InstanceBuilder::new("ObjectValue")
                .with_name("Target")
                .with_property("Value", door),
        );
        let storage = dom.insert(dom.root_ref(), InstanceBuilder::new("ReplicatedStorage"));
        dom.insert(
            storage,
            InstanceBuilder::new("ModuleScript")
                .with_name("Config")
                .with_property("Source", "return 1"),
        );
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, &dom, dom.root().children()).expect("place bytes");
        std::fs::write(root.join("place.rbxl"), bytes).expect("place file");
        std::fs::write(
            root.join("renium.project.jsonc"),
            "{
  \"schemaVersion\": 1
}
",
        )
        .expect("project file");

        let first = run(&root);
        assert_eq!(first["ok"], true);
        assert_eq!(first["instances"], 3);
        let script = walkdir::WalkDir::new(root.join("src"))
            .into_iter()
            .filter_map(Result::ok)
            .find(|entry| entry.path().extension().is_some_and(|ext| ext == "luau"))
            .expect("a script file");
        assert_eq!(
            std::fs::read_to_string(script.path()).expect("script source"),
            "return 1"
        );
        let workspace =
            SettingsBytecode::read_file(&root.join("instances").join("Workspace.renium"))
                .expect("workspace store");
        let door = workspace
            .instances
            .iter()
            .find(|instance| instance.name == "Door")
            .expect("door");
        assert_eq!(door.properties["Anchored"], true);
        let workspace_root = workspace
            .instances
            .iter()
            .find(|instance| instance.parent_index.is_none())
            .expect("workspace root");
        assert!(!workspace_root.properties.contains_key("SourceAssetId"));
        assert_eq!(workspace_root.properties["Archivable"], true);
        let door_index = workspace
            .instances
            .iter()
            .position(|instance| instance.name == "Door")
            .expect("door index");
        let target = workspace
            .instances
            .iter()
            .find(|instance| instance.name == "Target")
            .expect("target");
        assert_eq!(target.properties["Value"]["_type"], "Ref");
        assert_eq!(
            target.properties["Value"]["instanceIndex"],
            json!(door_index + 1)
        );

        let second = run(&root);
        assert_eq!(second["changedPaths"].as_array().map(Vec::len), Some(0));
        let _ = std::fs::remove_dir_all(&root);
    }
}
