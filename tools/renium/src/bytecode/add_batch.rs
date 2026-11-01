use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::app::output::print_json_output;
use crate::bytecode::edit::{
    NewInstanceValues, bytecode_service_name, new_script_source, prepare_new_instance_values,
};
use crate::bytecode::explorer::compact_path_string;
use crate::bytecode::query::bytecode_parent_index;
use crate::bytecode::{
    SettingsFileLock, acquire_settings_file_lock, apply_file_mutations,
    collect_source_path_updates, file_mutation_paths, high_level_split_path,
    resolve_bytecode_cli_settings_file,
};
use crate::editor::paths::{build_editor_instance_paths, build_editor_source_paths_by_index};
use crate::editor::sync::is_lua_source_class;
use crate::settings::bytecode::{
    SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance, encode_settings_bytecode,
};
use crate::settings::instance::{self as instance_api, AddInstanceSpec};
use crate::system::files::path_key;

/// One instance of a `ba --batch` request.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BatchAddItem {
    pub(crate) parent: String,
    #[serde(default)]
    pub(crate) service: Option<String>,
    pub(crate) name: String,
    #[serde(alias = "className", alias = "class_name", alias = "c")]
    pub(crate) class: String,
    #[serde(default, alias = "settingsId", alias = "settings_id")]
    pub(crate) id: Option<String>,
    #[serde(default, rename = "ref")]
    pub(crate) reference: Option<String>,
    #[serde(default)]
    pub(crate) source: Option<String>,
    #[serde(default, alias = "properties")]
    pub(crate) props: Map<String, Value>,
    #[serde(default, alias = "attributes")]
    pub(crate) attrs: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchAddRequest {
    ops: Vec<BatchAddItem>,
}

struct CreatedItem {
    item: usize,
    index: usize,
    reference: Option<String>,
    given_source: Option<String>,
}

struct StoreState {
    settings_file: PathBuf,
    service: String,
    service_dir: PathBuf,
    _lock: SettingsFileLock,
    before: SettingsBytecode,
    document: SettingsBytecode,
    source_paths_before: Vec<Option<PathBuf>>,
    created: Vec<CreatedItem>,
}

struct StorePlan {
    writes: BTreeMap<PathBuf, Vec<u8>>,
    removals: Vec<PathBuf>,
    receipts: Vec<Value>,
}

/// Resolves a service name to its store file and service hint.
pub(crate) type StoreResolver<'a> = dyn Fn(&str) -> Result<(PathBuf, String)> + 'a;

pub(crate) fn bytecode_add_batch(input: &Path, pretty: bool) -> Result<()> {
    let items = parse_batch_add_items(&read_batch_input(input)?)?;
    let result = add_store_instances_batch(items, &|service| {
        resolve_bytecode_cli_settings_file(None, Some(service), None)
    })?;
    print_json_output(&result, pretty)
}

fn read_batch_input(input: &Path) -> Result<String> {
    if input == Path::new("-") {
        let mut raw = String::new();
        io::stdin().read_to_string(&mut raw)?;
        return Ok(raw);
    }
    if let Some(inline) = input
        .to_str()
        .map(str::trim_start)
        .filter(|text| text.starts_with(['{', '[']))
    {
        return Ok(inline.to_string());
    }
    fs::read_to_string(input).with_context(|| format!("Failed to read {}", input.display()))
}

pub(crate) fn parse_batch_add_items(raw: &str) -> Result<Vec<BatchAddItem>> {
    let raw = raw.trim_start_matches('\u{feff}');
    let items = match raw.trim_start().chars().next() {
        Some('{') => serde_json::from_str::<BatchAddRequest>(raw).map(|request| request.ops),
        Some('[') => serde_json::from_str::<Vec<BatchAddItem>>(raw),
        _ => bail!("Batch input must be {{\"ops\":[...]}} or a JSON array of items"),
    }
    .context("Invalid batch input")?;
    if items.is_empty() {
        bail!("Batch input has no items");
    }
    Ok(items)
}

/// Creates every item in memory first, then writes each affected store and
/// its script files once. A failing item leaves every store untouched.
pub(crate) fn add_store_instances_batch(
    items: Vec<BatchAddItem>,
    resolve_store: &StoreResolver<'_>,
) -> Result<Value> {
    let mut stores: Vec<StoreState> = Vec::new();
    let mut store_positions = HashMap::new();
    let mut refs = HashMap::new();
    for (position, item) in items.into_iter().enumerate() {
        let label = match item.reference.as_deref() {
            Some(reference) => format!("item {position} (#{reference})"),
            None => format!("item {position}"),
        };
        add_batch_item(
            position,
            item,
            &mut stores,
            &mut store_positions,
            &mut refs,
            resolve_store,
        )
        .with_context(|| format!("Batch {label}"))?;
    }
    let plans = stores
        .iter()
        .map(plan_store_writes)
        .collect::<Result<Vec<_>>>()?;
    let mut created = Vec::new();
    let mut changed_paths = Vec::new();
    let mut written = Vec::new();
    for (store, plan) in stores.iter().zip(plans) {
        let store_changed = file_mutation_paths(&plan.writes, &plan.removals);
        apply_file_mutations(&plan.writes, &plan.removals).with_context(|| {
            let partial = if written.is_empty() {
                String::new()
            } else {
                format!("; stores already written: {}", written.join(", "))
            };
            format!(
                "Failed to write the {} store{partial}",
                store.service_label()
            )
        })?;
        written.push(store.service_label());
        changed_paths.extend(store_changed);
        created.extend(plan.receipts);
    }
    created.sort_by_key(|receipt| receipt["item"].as_u64());
    Ok(json!({
        "ok": true,
        "created": created,
        "stores": stores.len(),
        "changedPaths": changed_paths,
    }))
}

impl StoreState {
    fn service_label(&self) -> String {
        if self.service.is_empty() {
            self.settings_file.display().to_string()
        } else {
            self.service.clone()
        }
    }
}

fn add_batch_item(
    position: usize,
    item: BatchAddItem,
    stores: &mut Vec<StoreState>,
    store_positions: &mut HashMap<String, usize>,
    refs: &mut HashMap<String, (usize, usize)>,
    resolve_store: &StoreResolver<'_>,
) -> Result<()> {
    if let Some(reference) = item.reference.as_deref() {
        if reference.is_empty() || reference.starts_with('#') {
            bail!("ref must be a non-empty name without a leading #");
        }
        if refs.contains_key(reference) {
            bail!("ref #{reference} is already used by an earlier item");
        }
    }
    let (store_position, parent_index) = if let Some(reference) = item.parent.strip_prefix('#') {
        let (store_position, index) = refs
            .get(reference)
            .copied()
            .with_context(|| format!("parent #{reference} does not name an earlier item"))?;
        if let Some(service) = item.service.as_deref().filter(|value| !value.is_empty())
            && service != stores[store_position].service
        {
            bail!(
                "parent #{reference} lives in {}, not {service}",
                stores[store_position].service
            );
        }
        (store_position, Some(index))
    } else {
        let service = batch_item_service(&item)?;
        let store_position = load_batch_store(&service, stores, store_positions, resolve_store)?;
        let parent_index = bytecode_parent_index(
            &stores[store_position].document,
            false,
            None,
            Some(&item.parent),
            None,
            None,
        )?;
        (store_position, parent_index)
    };
    let store = &mut stores[store_position];
    let mut properties = item.props;
    if let Some(source) = item.source {
        if !is_lua_source_class(&item.class) {
            bail!("source is only accepted for Script, LocalScript and ModuleScript");
        }
        if properties.contains_key("Source") {
            bail!("give source or props.Source, not both");
        }
        properties.insert("Source".to_string(), Value::String(source));
    }
    let NewInstanceValues {
        properties,
        attributes,
        given_source,
    } = prepare_new_instance_values(
        &store.document,
        &store.settings_file,
        &item.class,
        properties,
        item.attrs,
    )?;
    let added = instance_api::add_instance(
        &mut store.document,
        AddInstanceSpec {
            settings_id: item.id,
            name: item.name,
            class_name: item.class,
            parent_index,
            properties,
            attributes,
        },
    )?;
    if let Some(reference) = item.reference.clone() {
        refs.insert(reference, (store_position, added.index));
    }
    store.created.push(CreatedItem {
        item: position,
        index: added.index,
        reference: item.reference,
        given_source,
    });
    Ok(())
}

/// The service an item's parent belongs to: its `service` field, or the
/// first segment of the dotted parent path.
fn batch_item_service(item: &BatchAddItem) -> Result<String> {
    if let Some(service) = item
        .service
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(service.to_string());
    }
    if item.parent.contains(':') {
        bail!(
            "parent {} is a settings id; add \"service\" to say which store it is in",
            item.parent
        );
    }
    high_level_split_path(&item.parent)
        .into_iter()
        .next()
        .with_context(|| format!("parent {:?} names no service", item.parent))
}

fn load_batch_store(
    service: &str,
    stores: &mut Vec<StoreState>,
    store_positions: &mut HashMap<String, usize>,
    resolve_store: &StoreResolver<'_>,
) -> Result<usize> {
    let (settings_file, service_hint) = resolve_store(service)?;
    let key = path_key(&settings_file);
    if let Some(position) = store_positions.get(&key) {
        return Ok(*position);
    }
    let lock = acquire_settings_file_lock(&settings_file)?;
    let document = if settings_file.exists() {
        SettingsBytecode::read_file(&settings_file)?
    } else {
        new_service_document(&service_hint)
    };
    let service = bytecode_service_name(&document, &settings_file, &service_hint);
    let service_dir = crate::project::storage::source_directory(&settings_file);
    let source_paths_before = build_editor_source_paths_by_index(&document, &service, &service_dir);
    stores.push(StoreState {
        settings_file,
        service,
        service_dir,
        _lock: lock,
        before: document.clone(),
        document,
        source_paths_before,
        created: Vec::new(),
    });
    store_positions.insert(key, stores.len() - 1);
    Ok(stores.len() - 1)
}

fn new_service_document(service: &str) -> SettingsBytecode {
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: Vec::new(),
    };
    let service = service.trim();
    if !service.is_empty() {
        document.instances.push(SettingsBytecodeInstance::new(
            "editor:0".to_string(),
            service.to_string(),
            service.to_string(),
            None,
        ));
    }
    document
}

fn plan_store_writes(store: &StoreState) -> Result<StorePlan> {
    let mut writes = BTreeMap::new();
    let mut removals = Vec::new();
    let source_paths = collect_source_path_updates(
        &store.before,
        &store.source_paths_before,
        &store.document,
        &store.service,
        &store.service_dir,
        &mut writes,
        &mut removals,
    )?;
    writes.insert(
        store.settings_file.clone(),
        encode_settings_bytecode(&store.document)?,
    );
    let paths = build_editor_instance_paths(&store.document, &store.service);
    let mut receipts = Vec::with_capacity(store.created.len());
    for created in &store.created {
        let instance = &store.document.instances[created.index];
        let mut receipt = Map::new();
        receipt.insert("item".into(), json!(created.item));
        if let Some(reference) = &created.reference {
            receipt.insert("ref".into(), json!(reference));
        }
        receipt.insert("id".into(), json!(instance.settings_id));
        receipt.insert("index".into(), json!(created.index));
        receipt.insert("service".into(), json!(store.service));
        if let Some(Some(path)) = paths.get(created.index) {
            let compact = compact_path_string(&path.path_segments, Some(&path.path_ordinals));
            receipt.insert(
                "path".into(),
                compact.map_or_else(|| json!(path.path_segments), Value::String),
            );
        }
        if is_lua_source_class(&instance.class_name)
            && let Some(Some(source_path)) = source_paths.get(created.index)
        {
            match new_script_source(source_path, created.given_source.as_deref())
                .with_context(|| format!("Batch item {}", created.item))?
            {
                Some(bytes) => {
                    writes.insert(source_path.clone(), bytes);
                }
                None => {
                    receipt.insert("adoptedSource".into(), json!(true));
                }
            }
            receipt.insert("sourcePath".into(), json!(source_path));
        }
        receipts.push(Value::Object(receipt));
    }
    Ok(StorePlan {
        writes,
        removals,
        receipts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver(root: &Path) -> impl Fn(&str) -> Result<(PathBuf, String)> + '_ {
        move |service| {
            Ok((
                root.join("instances").join(format!("{service}.renium")),
                service.to_string(),
            ))
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        crate::system::files::create_unique_directory(&std::env::temp_dir(), tag).unwrap()
    }

    fn run(root: &Path, raw: &str) -> Result<Value> {
        add_store_instances_batch(parse_batch_add_items(raw)?, &resolver(root))
    }

    fn names(document: &SettingsBytecode) -> Vec<String> {
        document
            .instances
            .iter()
            .map(|instance| instance.name.clone())
            .collect()
    }

    #[test]
    fn batch_creates_across_two_services_with_ref_parents() {
        let root = scratch("renium-ba-batch-");
        let result = run(
            &root,
            r##"{"ops":[
                {"parent":"ServerScriptService","name":"Battle","class":"Folder","ref":"battle"},
                {"parent":"#battle","name":"Abilities","class":"Folder","ref":"abilities"},
                {"parent":"#abilities","name":"CarMods","class":"ModuleScript","source":"return {}"},
                {"parent":"ReplicatedStorage","name":"Shared","class":"Folder","ref":"shared"},
                {"parent":"#shared","name":"Config","class":"ModuleScript","attrs":{"Version":3}},
                {"parent":"ServerScriptService.Battle","name":"Main","class":"Script"},
                {"parent":"#battle","name":"Mode","class":"StringValue","props":{"Value":"ranked"}}
            ]}"##,
        )
        .unwrap();
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["stores"], 2, "{result}");
        let created = result["created"].as_array().unwrap();
        assert_eq!(created.len(), 7, "{result}");
        assert_eq!(created[1]["ref"], "abilities");
        assert_eq!(
            created[2]["path"],
            "ServerScriptService.Battle.Abilities.CarMods"
        );
        assert_eq!(created[4]["path"], "ReplicatedStorage.Shared.Config");
        assert_eq!(created[5]["path"], "ServerScriptService.Battle.Main");
        assert!(created[0].get("sourcePath").is_none(), "{result}");
        let car_mods = PathBuf::from(created[2]["sourcePath"].as_str().unwrap());
        assert_eq!(fs::read_to_string(&car_mods).unwrap(), "return {}");
        let config = PathBuf::from(created[4]["sourcePath"].as_str().unwrap());
        assert_eq!(fs::read_to_string(&config).unwrap(), "");
        let server =
            SettingsBytecode::read_file(&root.join("instances").join("ServerScriptService.renium"))
                .unwrap();
        assert_eq!(
            names(&server),
            [
                "ServerScriptService",
                "Battle",
                "Abilities",
                "CarMods",
                "Main",
                "Mode"
            ]
        );
        assert_eq!(server.instances[3].parent_index, Some(2));
        assert_eq!(server.instances[4].parent_index, Some(1));
        assert_eq!(server.instances[5].parent_index, Some(1));
        assert_eq!(server.instances[5].properties["Value"], "ranked");
        let shared =
            SettingsBytecode::read_file(&root.join("instances").join("ReplicatedStorage.renium"))
                .unwrap();
        assert_eq!(names(&shared), ["ReplicatedStorage", "Shared", "Config"]);
        assert_eq!(shared.instances[2].attributes["Version"], 3);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn failing_item_leaves_the_store_untouched() {
        let root = scratch("renium-ba-batch-fail-");
        let settings_file = root.join("instances").join("ServerScriptService.renium");
        run(
            &root,
            r##"[{"parent":"ServerScriptService","name":"Existing","class":"Folder"}]"##,
        )
        .unwrap();
        let before = fs::read(&settings_file).unwrap();
        let error = run(
            &root,
            r##"[
                {"parent":"ServerScriptService","name":"Fresh","class":"Folder","ref":"fresh"},
                {"parent":"#missing","name":"Orphan","class":"ModuleScript","source":"return 1"}
            ]"##,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("item 1") && message.contains("#missing"),
            "{message}"
        );
        assert_eq!(fs::read(&settings_file).unwrap(), before);
        let service_dir = crate::project::storage::source_directory(&settings_file);
        assert!(!service_dir.join("Orphan.luau").exists());
        let bad_class = run(
            &root,
            r##"[
                {"parent":"ServerScriptService","name":"Fresh","class":"Folder"},
                {"parent":"ServerScriptService","name":"Loose","class":"Folder","source":"return 1"}
            ]"##,
        )
        .unwrap_err();
        assert!(format!("{bad_class:#}").contains("item 1"), "{bad_class:#}");
        assert_eq!(fs::read(&settings_file).unwrap(), before);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn script_item_writes_its_source_file_once_per_store() {
        let root = scratch("renium-ba-batch-script-");
        let settings_file = root.join("instances").join("ServerScriptService.renium");
        let result = run(
            &root,
            r##"[
                {"parent":"ServerScriptService","name":"Rounds","class":"ModuleScript","source":"return { rounds = 3 }\n"},
                {"parent":"ServerScriptService","name":"Main","class":"Script","source":"print('hi')"}
            ]"##,
        )
        .unwrap();
        assert_eq!(result["stores"], 1, "{result}");
        let changed = result["changedPaths"].as_array().unwrap();
        assert_eq!(changed.len(), 3, "{result}");
        let created = result["created"].as_array().unwrap();
        let rounds = PathBuf::from(created[0]["sourcePath"].as_str().unwrap());
        assert!(rounds.ends_with("Rounds.luau"), "{result}");
        assert_eq!(
            fs::read_to_string(&rounds).unwrap(),
            "return { rounds = 3 }\n"
        );
        let main = PathBuf::from(created[1]["sourcePath"].as_str().unwrap());
        assert_eq!(fs::read_to_string(&main).unwrap(), "print('hi')");
        let document = SettingsBytecode::read_file(&settings_file).unwrap();
        assert_eq!(
            document.instances[1].properties["Source"],
            crate::settings::EXTERNAL_SOURCE_MARKER
        );
        let _ = fs::remove_dir_all(&root);
    }
}
