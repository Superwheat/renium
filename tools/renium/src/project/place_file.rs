use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use rbx_dom_weak::types::{Ref as RbxRef, Variant as RbxVariant};
use rbx_dom_weak::{Instance as RbxInstance, Ustr, WeakDom as RbxWeakDom};
use rbx_reflection::{DataType, PropertyKind, PropertySerialization, ReflectionDatabase};
use serde_json::{Map, Value, json};

use crate::app::output::print_json_output;
use crate::bytecode::explorer::compact_path_string;
use crate::cli::{ComparePlaceArgs, QueryPlaceArgs};
use crate::editor::sync::is_lua_source_class;
use crate::project::config;
use crate::rbx::decode::rbx_variant_to_settings_json;
use crate::rbx::encode::{
    enum_item_name_by_value, rbx_logical_property_name, rbx_property_descriptor, strip_enum_prefix,
};
use crate::rbx::model::{
    BytecodeModelImportRefs, RbxPlaceFormat, build_rbx_place, is_service_class,
    rbx_dom_instance_path_parts,
};
use crate::system::text::normalized_source_bytes;

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct ScriptRecord {
    class_name: String,
    source: Vec<u8>,
}

type ScriptGroups = BTreeMap<Vec<String>, Vec<ScriptRecord>>;

struct Descendants<'a> {
    dom: &'a RbxWeakDom,
    pending: Vec<RbxRef>,
}

impl Iterator for Descendants<'_> {
    type Item = RbxRef;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(referent) = self.pending.pop() {
            let Some(instance) = self.dom.get_by_ref(referent) else {
                continue;
            };
            self.pending.extend(instance.children().iter().copied());
            return Some(referent);
        }
        None
    }
}

pub(super) fn place_dom(path: &Path) -> Result<RbxWeakDom> {
    if !path.is_file() {
        bail!("Place file does not exist: {}", path.display());
    }
    RbxPlaceFormat::from_path(path)?.read(path)
}

/// `place_dom` as `RbxPlaceFormat::read_sparse` reads it.
pub(super) fn sparse_place_dom(path: &Path) -> Result<rbx_binary::SparseDom> {
    if !path.is_file() {
        bail!("Place file does not exist: {}", path.display());
    }
    RbxPlaceFormat::from_path(path)?.read_sparse(path)
}

fn descendants(dom: &RbxWeakDom) -> Descendants<'_> {
    Descendants {
        dom,
        pending: dom.root().children().to_vec(),
    }
}

fn script_source(dom: &RbxWeakDom, referent: RbxRef) -> Option<&str> {
    let instance = dom.get_by_ref(referent)?;
    if !is_lua_source_class(instance.class.as_str()) {
        return None;
    }
    match instance
        .properties
        .iter()
        .find(|(name, _)| name.as_str() == "Source")
        .map(|(_, value)| value)
    {
        Some(RbxVariant::String(source)) => Some(source),
        None => Some(""),
        _ => None,
    }
}

fn query_result(dom: &RbxWeakDom, referent: RbxRef) -> Value {
    let instance = dom
        .get_by_ref(referent)
        .expect("query results contain live referents");
    let (path, ordinals) = rbx_dom_instance_path_parts(dom, referent);
    let mut result = Map::from_iter([
        ("path".to_string(), json!(path)),
        ("name".to_string(), json!(instance.name)),
        ("className".to_string(), json!(instance.class.as_str())),
    ]);
    if ordinals.iter().any(|ordinal| *ordinal != 1) {
        result.insert("ordinals".to_string(), json!(ordinals));
    }
    Value::Object(result)
}

enum PropertyRequest<'a> {
    All,
    Named(Vec<&'a str>),
}

impl<'a> PropertyRequest<'a> {
    fn parse(raw: &'a [String]) -> Result<Option<Self>> {
        if raw.is_empty() {
            return Ok(None);
        }
        let names = raw
            .iter()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        if names.is_empty() {
            bail!("--props needs property names or all, for example --props Anchored,Size");
        }
        if names.iter().any(|name| name.eq_ignore_ascii_case("all")) {
            return Ok(Some(Self::All));
        }
        Ok(Some(Self::Named(names)))
    }
}

fn reference_value(dom: &RbxWeakDom, target: RbxRef) -> Value {
    if target == dom.root_ref() || dom.get_by_ref(target).is_none() {
        return Value::Null;
    }
    let (path, ordinals) = rbx_dom_instance_path_parts(dom, target);
    compact_path_string(&path, Some(&ordinals)).map_or_else(
        || json!({"path": path, "ordinals": ordinals}),
        Value::String,
    )
}

/// Values print in the settings-store shape, except enums (`Enum.Type.Item`)
/// and references (the target's path), which read better by name.
fn property_value(
    dom: &RbxWeakDom,
    class_name: &str,
    property_name: &str,
    value: &RbxVariant,
    database: &ReflectionDatabase<'_>,
) -> Value {
    let descriptor = rbx_property_descriptor(database, class_name, property_name);
    let enum_item = match value {
        RbxVariant::Enum(item) => descriptor
            .and_then(|descriptor| match &descriptor.data_type {
                DataType::Enum(enum_name) => Some(*enum_name),
                _ => None,
            })
            .map(|enum_name| (enum_name, item.to_u32())),
        RbxVariant::EnumItem(item) => Some((item.ty.as_str(), item.value)),
        RbxVariant::Ref(target) => return reference_value(dom, *target),
        _ => None,
    };
    if let Some((enum_name, item_value)) = enum_item
        && let Some(item_name) = enum_item_name_by_value(database, enum_name, item_value)
    {
        return json!(format!("Enum.{}.{item_name}", strip_enum_prefix(enum_name)));
    }
    rbx_variant_to_settings_json(
        value,
        descriptor,
        database,
        &BytecodeModelImportRefs::default(),
    )
    .or_else(|| serde_json::to_value(value).ok())
    .unwrap_or(Value::Null)
}

fn saved_property<'a>(
    instance: &'a RbxInstance,
    logical: Option<&'a str>,
    requested: &'a str,
) -> Option<(&'a str, &'a RbxVariant)> {
    [logical, Some(requested)]
        .into_iter()
        .flatten()
        .find_map(|name| {
            instance
                .properties
                .get(&Ustr::from(name))
                .map(|value| (logical.unwrap_or(name), value))
        })
        .or_else(|| {
            instance
                .properties
                .iter()
                .find(|(name, _)| name.as_str().eq_ignore_ascii_case(requested))
                .map(|(name, value)| (name.as_str(), value))
        })
}

fn migrated_property<'a>(
    instance: &'a RbxInstance,
    class_name: &str,
    logical: &str,
    database: &'a ReflectionDatabase<'a>,
) -> Option<(&'a str, &'a RbxVariant)> {
    let PropertyKind::Canonical {
        serialization: PropertySerialization::Migrate(migration),
    } = &rbx_property_descriptor(database, class_name, logical)?.kind
    else {
        return None;
    };
    migration.new_property_names().iter().find_map(|name| {
        instance
            .properties
            .get(&Ustr::from(name))
            .map(|value| (*name, value))
    })
}

/// A serialized property missing from the file loads as the class default
/// (`defaulted`, e.g. a place saved before the property existed). Properties
/// Roblox derives from others instead of saving (Position, BrickColor) are
/// `notSaved`; names the class does not have are `unknown`.
fn insert_property_values(
    result: &mut Value,
    dom: &RbxWeakDom,
    referent: RbxRef,
    request: &PropertyRequest<'_>,
    database: &ReflectionDatabase<'_>,
) {
    let Some(instance) = dom.get_by_ref(referent) else {
        return;
    };
    let class_name = instance.class.as_str();
    let mut values = Map::new();
    let mut defaulted = Vec::new();
    let mut not_saved = Vec::new();
    let mut unknown = Vec::new();
    match request {
        PropertyRequest::All => {
            let mut properties = instance.properties.iter().collect::<Vec<_>>();
            properties.sort_unstable_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));
            for (name, value) in properties {
                values.insert(
                    name.to_string(),
                    property_value(dom, class_name, name.as_str(), value, database),
                );
            }
        }
        PropertyRequest::Named(names) => {
            for requested in names {
                let logical = rbx_logical_property_name(database, class_name, requested);
                let intrinsic = match logical.unwrap_or(requested) {
                    "Name" => Some(json!(instance.name)),
                    "ClassName" => Some(json!(class_name)),
                    "Parent" => Some(reference_value(dom, instance.parent())),
                    _ => None,
                };
                if let Some(value) = intrinsic {
                    values.insert(logical.unwrap_or(requested).to_string(), value);
                    continue;
                }
                if let Some((name, value)) = saved_property(instance, logical, requested) {
                    values.insert(
                        name.to_string(),
                        property_value(dom, class_name, name, value, database),
                    );
                    continue;
                }
                let Some(logical) = logical else {
                    unknown.push(*requested);
                    continue;
                };
                // Studio saves a migrated property under its successor's name
                // (ImageButton.Image as ImageContent); the value answers for
                // the name that was asked.
                if let Some((name, value)) =
                    migrated_property(instance, class_name, logical, database)
                {
                    values.insert(
                        logical.to_string(),
                        property_value(dom, class_name, name, value, database),
                    );
                    continue;
                }
                let serialized = !matches!(
                    rbx_property_descriptor(database, class_name, logical)
                        .map(|property| &property.kind),
                    Some(PropertyKind::Canonical {
                        serialization: PropertySerialization::DoesNotSerialize
                    })
                );
                match database
                    .classes
                    .get(class_name)
                    .filter(|_| serialized)
                    .and_then(|class| database.find_default_property(class, logical))
                {
                    Some(value) => {
                        values.insert(
                            logical.to_string(),
                            property_value(dom, class_name, logical, value, database),
                        );
                        defaulted.push(logical);
                    }
                    None => not_saved.push(logical),
                }
            }
        }
    }
    result["props"] = Value::Object(values);
    for (key, names) in [
        ("defaulted", defaulted),
        ("notSaved", not_saved),
        ("unknown", unknown),
    ] {
        if !names.is_empty() {
            result[key] = json!(names);
        }
    }
}

pub(crate) fn query_place(args: QueryPlaceArgs) -> Result<()> {
    let query = args
        .query
        .as_deref()
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(str::to_ascii_lowercase);
    // Repeated selectors of one kind match any of their values, so one decode
    // of the place answers several lookups.
    let names = non_empty(&args.name);
    let class_names = non_empty(&args.class_name);
    let sources = non_empty(&args.source);
    if query.is_none() && names.is_empty() && class_names.is_empty() && sources.is_empty() {
        bail!("Provide a query, --name, --class, or --source");
    }
    let properties = PropertyRequest::parse(&args.props)?;
    let database = match properties {
        Some(_) => {
            Some(rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?)
        }
        None => None,
    };

    let dom = place_dom(&args.input)?;
    let limit = if args.all {
        usize::MAX
    } else {
        args.limit.get()
    };
    let mut matches = Vec::new();
    let mut truncated = false;
    let mut searched = 0usize;
    for referent in descendants(&dom) {
        let Some(instance) = dom.get_by_ref(referent) else {
            continue;
        };
        searched += 1;
        if !names.is_empty() && !names.iter().any(|name| instance.name == *name)
            || !class_names.is_empty()
                && !class_names
                    .iter()
                    .any(|class_name| instance.class.as_str() == *class_name)
            || !sources.is_empty()
                && script_source(&dom, referent).is_none_or(|candidate| {
                    !sources.iter().any(|source| candidate.contains(source))
                })
            || query.as_ref().is_some_and(|query| {
                !instance.name.to_ascii_lowercase().contains(query)
                    && !instance.class.as_str().to_ascii_lowercase().contains(query)
            })
        {
            continue;
        }
        if matches.len() == limit {
            truncated = true;
            break;
        }
        let mut result = query_result(&dom, referent);
        if let (Some(properties), Some(database)) = (&properties, database) {
            insert_property_values(&mut result, &dom, referent, properties, database);
        }
        matches.push(result);
    }

    let mut result = json!({
        "ok": true,
        "input": args.input,
        "matches": matches,
        "searched": searched,
        "truncated": truncated,
    });
    if result["matches"].as_array().is_some_and(Vec::is_empty) && sources.is_empty() {
        result["hint"] = json!(
            "A query matches instance names and classes only; use -s TEXT to search script source"
        );
    }
    if crate::app::output::compact_text_output() {
        compact_query_text(&mut result);
    }
    crate::app::output::drop_false(&mut result, &["truncated"]);
    print_json_output(&result, args.pretty)
}

fn non_empty(values: &[String]) -> Vec<&str> {
    values
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect()
}

/// Text mode prints each match as the dotted path every other command prints:
/// `name` repeats the last segment and the input path was the caller's own.
fn compact_query_text(result: &mut Value) {
    if let Some(map) = result.as_object_mut() {
        map.remove("input");
        map.remove("searched");
    }
    let Some(matches) = result.get_mut("matches").and_then(Value::as_array_mut) else {
        return;
    };
    for entry in matches {
        let Some(map) = entry.as_object_mut() else {
            continue;
        };
        let segments = map
            .get("path")
            .and_then(Value::as_array)
            .map(|path| {
                path.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let ordinals = map
            .get("ordinals")
            .and_then(Value::as_array)
            .map(|ordinals| {
                ordinals
                    .iter()
                    .filter_map(Value::as_u64)
                    .map(|ordinal| ordinal as usize)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if let Some(path) = crate::bytecode::explorer::compact_path_string(
            &segments,
            (!ordinals.is_empty()).then_some(ordinals.as_slice()),
        ) {
            map.insert("path".to_string(), Value::String(path));
            map.remove("ordinals");
        }
        map.remove("name");
    }
}

fn projected_services(root: &Path) -> Result<Vec<String>> {
    let mut services = Vec::new();
    for entry in super::storage::service_directories(root)? {
        if let Some(name) = entry.file_name().and_then(|name| name.to_str())
            && !name.starts_with('.')
        {
            services.push(name.to_string());
        }
    }
    services.sort();
    Ok(services)
}

/// The scripts of a place, each under the names from its service down to it,
/// and the names of the place's services.
pub(super) struct PlaceScripts {
    pub(super) services: Vec<String>,
    pub(super) scripts: Vec<(Vec<String>, ScriptRecord)>,
}

impl PlaceScripts {
    fn record(class_name: &str, source: &str) -> ScriptRecord {
        ScriptRecord {
            class_name: class_name.to_string(),
            source: normalized_source_bytes(source.as_bytes()).collect(),
        }
    }

    pub(super) fn of_dom(dom: &RbxWeakDom) -> Self {
        let services = (dom.root().children().iter())
            .filter_map(|id| dom.get_by_ref(*id).map(|node| node.name.clone()))
            .collect();
        let scripts = descendants(dom)
            .filter_map(|referent| {
                let source = script_source(dom, referent)?;
                let class_name = dom.get_by_ref(referent)?.class.as_str();
                let (path, _) = rbx_dom_instance_path_parts(dom, referent);
                Some((path, Self::record(class_name, source)))
            })
            .collect();
        Self { services, scripts }
    }

    /// Of a place file, as `of_dom` gives them of its DOM. A binary file is
    /// read for them alone: every instance by its name and class, and each
    /// script with its Source.
    pub(super) fn of_file(path: &Path) -> Result<Self> {
        if !path.is_file() {
            bail!("Place file does not exist: {}", path.display());
        }
        let database =
            rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
        let kept = (database.classes.keys())
            .map(|class| {
                let kept = is_lua_source_class(class).then(|| "Source".to_string());
                (class.to_string(), kept.into_iter().collect())
            })
            .collect();
        let Some(flat) = RbxPlaceFormat::from_path(path)?.read_flat(path, kept)? else {
            return Ok(Self::of_dom(&place_dom(path)?));
        };
        let segment = |index: usize| {
            let instance = &flat.instances[index];
            if instance.parent_index.is_none() && is_service_class(instance.class.as_str()) {
                instance.class.to_string()
            } else {
                instance.name.clone()
            }
        };
        let script = |(index, instance): (usize, &rbx_binary::FlatInstance)| {
            if !is_lua_source_class(instance.class.as_str()) {
                return None;
            }
            let mut saved = instance.properties.iter().rev();
            let saved = saved.find(|(name, _)| name.as_str() == "Source");
            let source = match saved {
                Some((_, RbxVariant::String(source))) => source.as_str(),
                None => "",
                _ => return None,
            };
            let mut path = vec![segment(index)];
            let mut parent = instance.parent_index;
            while let Some(index) = parent {
                path.push(segment(index));
                parent = flat.instances[index].parent_index;
            }
            path.reverse();
            Some((path, Self::record(instance.class.as_str(), source)))
        };
        Ok(Self {
            services: (flat.root_indices.iter())
                .map(|root| flat.instances[*root].name.clone())
                .collect(),
            scripts: (flat.instances.iter().enumerate())
                .filter_map(script)
                .collect(),
        })
    }

    /// The scripts under the services `services` names, by their paths.
    fn groups(self, services: &BTreeSet<String>) -> ScriptGroups {
        let mut groups = ScriptGroups::new();
        for (path, script) in self.scripts {
            if path.first().is_some_and(|name| services.contains(name)) {
                groups.entry(path).or_default().push(script);
            }
        }
        for scripts in groups.values_mut() {
            scripts.sort();
        }
        groups
    }
}

fn remove_exact_matches(project: &mut Vec<ScriptRecord>, place: &mut Vec<ScriptRecord>) -> usize {
    let mut matched = 0;
    let mut index = 0;
    while index < project.len() {
        if let Ok(place_index) = place.binary_search(&project[index]) {
            project.remove(index);
            place.remove(place_index);
            matched += 1;
        } else {
            index += 1;
        }
    }
    matched
}

fn take_same_class(scripts: &mut Vec<ScriptRecord>, class_name: &str) -> Option<ScriptRecord> {
    let index = scripts
        .iter()
        .position(|script| script.class_name == class_name)?;
    Some(scripts.remove(index))
}

fn difference_value(
    kind: &str,
    path: &[String],
    project: Option<&ScriptRecord>,
    place: Option<&ScriptRecord>,
) -> Value {
    let mut value = Map::from_iter([
        ("kind".to_string(), json!(kind)),
        ("path".to_string(), json!(path)),
    ]);
    match (project, place) {
        (Some(project), Some(place)) if project.class_name == place.class_name => {
            value.insert("class".to_string(), json!(project.class_name));
        }
        _ => {
            if let Some(project) = project {
                value.insert("projectClass".to_string(), json!(project.class_name));
            }
            if let Some(place) = place {
                value.insert("placeClass".to_string(), json!(place.class_name));
            }
        }
    }
    Value::Object(value)
}

pub(crate) fn compare_place(mut args: ComparePlaceArgs, project: Option<&Path>) -> Result<()> {
    let started = std::time::Instant::now();
    if args.against.is_none() {
        args.against = args.other.take();
    }
    if !args.full {
        return compare_scripts(&args, project, started);
    }
    // Full comparison already normalizes exact reflection defaults away.
    // Omit them during decoding instead of allocating then dropping them.
    let read = |path: &Path| -> Result<RbxWeakDom> {
        let read = std::time::Instant::now();
        let dom = RbxPlaceFormat::from_path(path)?.read_for_comparison(path)?;
        // The file's bytes and what they unpacked to go back to the system.
        crate::daemon::memory::release_between_phases();
        crate::app::output::log_global(
            4,
            format_args!(
                "[renium] place comparison input decoding: {:.1}ms",
                read.elapsed().as_secs_f64() * 1000.0
            ),
        );
        Ok(dom)
    };
    // What each place gives up as it is read; a place read twice gives up
    // the same.
    let internal = std::cell::RefCell::new(BTreeSet::new());
    let not_serialized = std::cell::RefCell::new(BTreeMap::new());
    let unsaved_service = std::cell::RefCell::new(BTreeMap::new());
    let unsaved_project_service = std::cell::RefCell::new(BTreeMap::new());
    let unsaved_instances = std::cell::Cell::new(0);
    let (mut result, project_path) = if let Some(against) = &args.against {
        let place = |path: &Path| -> Result<RbxWeakDom> {
            let dom = read(path)?;
            internal.borrow_mut().extend(
                (dom.root().children().iter())
                    .filter_map(|id| Some(dom.get_by_ref(*id)?.name.clone()))
                    .filter(|name| super::place_diff::is_engine_internal_service(name)),
            );
            Ok(dom)
        };
        let result = super::place_diff::compare(
            &mut || place(&args.input),
            &mut || place(against),
            None,
            &args,
        )?;
        (result, against.clone())
    } else {
        let place = || -> Result<RbxWeakDom> {
            let mut dom = read(&args.input)?;
            *unsaved_service.borrow_mut() =
                super::place_diff::omit_unsaved_service_properties(&mut dom)?;
            Ok(dom)
        };
        // The place is read before the project is looked at: a place that
        // cannot be read is what the command says first.
        let mut read_already = Some(place()?);
        let loaded = config::load_project(project, None)?;
        let projection = config::stage_project(&loaded)?;
        let services = projected_services(projection.root())?;
        let service_set = services.iter().cloned().collect::<BTreeSet<_>>();
        let result = super::place_diff::compare(
            &mut || read_already.take().map_or_else(&place, Ok),
            &mut || -> Result<RbxWeakDom> {
                let services = services.clone();
                let built = std::time::Instant::now();
                let mut dom =
                    build_rbx_place(projection.root(), services, None, false, false, false)?.dom;
                let saved = std::time::Instant::now();
                *not_serialized.borrow_mut() =
                    super::place_diff::project_properties_as_saved(&mut dom)?;
                crate::app::output::log_global(
                    4,
                    format_args!(
                        "[renium] place comparison project: built {:.1}ms, as saved {:.1}ms",
                        (saved - built).as_secs_f64() * 1000.0,
                        saved.elapsed().as_secs_f64() * 1000.0
                    ),
                );
                unsaved_instances.set(super::place_diff::omit_unsaved_project_instances(&mut dom));
                crate::daemon::memory::release_between_phases();
                // An imported project carries the pivot the place gave up.
                *unsaved_project_service.borrow_mut() =
                    super::place_diff::omit_unsaved_service_properties(&mut dom)?;
                Ok(dom)
            },
            Some(&service_set),
            &args,
        )?;
        (result, loaded.path)
    };
    let (internal, not_serialized, mut unsaved_service, unsaved_instances) = (
        internal.into_inner(),
        not_serialized.into_inner(),
        unsaved_service.into_inner(),
        unsaved_instances.get(),
    );
    for (name, count) in unsaved_project_service.into_inner() {
        *unsaved_service.entry(name).or_default() += count;
    }
    if !not_serialized.is_empty() {
        result["notSerializedProjectProperties"] = json!(not_serialized);
    }
    if !unsaved_service.is_empty() {
        result["unsavedServiceProperties"] = json!(unsaved_service);
    }
    if unsaved_instances > 0 {
        result["unsavedProjectInstances"] = json!(unsaved_instances);
    }
    if !internal.is_empty() {
        result["engineManagedServices"] = json!(internal);
    }
    result["input"] = json!(args.input);
    result["target"] = json!(project_path);
    result["targetKind"] = json!(if args.against.is_some() {
        "place"
    } else {
        "project"
    });
    print_json_output(&result, args.pretty)
}

/// `compare_place` of the scripts alone, which reads of each place nothing
/// but them.
fn compare_scripts(
    args: &ComparePlaceArgs,
    project: Option<&Path>,
    started: std::time::Instant,
) -> Result<()> {
    let (place, target, project_path, service_set) = if let Some(against) = &args.against {
        let (before, target) = rayon::join(
            || PlaceScripts::of_file(&args.input),
            || PlaceScripts::of_file(against),
        );
        let (before, target) = (before?, target?);
        let services = (before.services.iter().chain(&target.services))
            .filter(|name| !super::place_diff::is_engine_internal_service(name))
            .cloned()
            .collect::<BTreeSet<_>>();
        (before, target, against.clone(), services)
    } else {
        let before = PlaceScripts::of_file(&args.input)?;
        let loaded = config::load_project(project, None)?;
        let projection = config::stage_project(&loaded)?;
        let services = projected_services(projection.root())?;
        let service_set = services.iter().cloned().collect::<BTreeSet<_>>();
        let dom = build_rbx_place(projection.root(), services, None, false, false, false)?.dom;
        (before, PlaceScripts::of_dom(&dom), loaded.path, service_set)
    };
    crate::app::output::log_global(
        4,
        format_args!(
            "[renium] place comparison input decoding: {:.1}ms",
            started.elapsed().as_secs_f64() * 1000.0
        ),
    );
    let project_scripts = target.groups(&service_set);
    let place_scripts = place.groups(&service_set);
    let project_count = project_scripts.values().map(Vec::len).sum::<usize>();
    let place_count = place_scripts.values().map(Vec::len).sum::<usize>();

    let paths = project_scripts
        .keys()
        .chain(place_scripts.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut matching = 0;
    let mut changed = 0;
    let mut missing_from_place = 0;
    let mut extra_in_place = 0;
    let mut differences = Vec::new();
    let limit = if args.all {
        usize::MAX
    } else {
        args.limit.get()
    };

    let mut record_difference = |difference: Value| {
        if differences.len() < limit {
            differences.push(difference);
        }
    };
    for path in paths {
        let mut project = project_scripts.get(&path).cloned().unwrap_or_default();
        let mut place = place_scripts.get(&path).cloned().unwrap_or_default();
        matching += remove_exact_matches(&mut project, &mut place);

        let mut index = 0;
        while index < project.len() {
            let Some(place_script) = take_same_class(&mut place, &project[index].class_name) else {
                index += 1;
                continue;
            };
            let project_script = project.remove(index);
            changed += 1;
            record_difference(difference_value(
                "sourceChanged",
                &path,
                Some(&project_script),
                Some(&place_script),
            ));
        }

        while !project.is_empty() && !place.is_empty() {
            let project_script = project.remove(0);
            let place_script = place.remove(0);
            changed += 1;
            record_difference(difference_value(
                "classChanged",
                &path,
                Some(&project_script),
                Some(&place_script),
            ));
        }
        for project_script in project {
            missing_from_place += 1;
            record_difference(difference_value(
                "missingFromPlace",
                &path,
                Some(&project_script),
                None,
            ));
        }
        for place_script in place {
            extra_in_place += 1;
            record_difference(difference_value(
                "extraInPlace",
                &path,
                None,
                Some(&place_script),
            ));
        }
    }

    let difference_count = changed + missing_from_place + extra_in_place;
    let mut result = json!({
            "ok": true,
            "matches": difference_count == 0,
            "input": args.input,
            "project": project_path,
            "scope": "scripts",
            "targetKind": if args.against.is_some() { "place" } else { "project" },
            "services": service_set,
            "projectScripts": project_count,
            "placeScripts": place_count,
            "matchingScripts": matching,
            "changed": changed,
            "missingFromPlace": missing_from_place,
            "extraInPlace": extra_in_place,
            "differences": differences,
            "truncated": difference_count > differences.len(),
    });
    crate::app::output::drop_false(&mut result, &["truncated"]);
    print_json_output(&result, args.pretty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbx_dom_weak::InstanceBuilder;

    #[test]
    fn query_text_mode_prints_dotted_paths_without_the_duplicate_name() {
        let mut result = json!({
            "ok": true,
            "input": "C:/somewhere/place.rbxl",
            "matches": [
                {"className": "Part", "name": "Pad", "path": ["Workspace", "Map", "Pad"], "ordinals": [1, 1, 3]},
                {"className": "Folder", "name": "a.b", "path": ["Workspace", "a.b"]}
            ],
            "searched": 5
        });
        compact_query_text(&mut result);
        assert!(result.get("input").is_none());
        assert_eq!(
            result["matches"][0],
            json!({"className": "Part", "path": "Workspace.Map.Pad[3]"})
        );
        assert_eq!(
            result["matches"][1],
            json!({"className": "Folder", "path": ["Workspace", "a.b"]})
        );
    }

    #[test]
    fn place_query_prints_requested_saved_properties_from_both_formats() {
        use clap::Parser;
        let root = crate::system::files::create_unique_directory(
            &std::env::temp_dir(),
            "renium-place-query-",
        )
        .unwrap();
        let _cleanup = crate::system::files::OnDrop::new(|| {
            let _ = std::fs::remove_dir_all(&root);
        });
        let mut dom = RbxWeakDom::new(InstanceBuilder::new("DataModel"));
        let workspace = dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("Workspace")
                .with_property(
                    "PlayerCharacterDestroyBehavior",
                    rbx_dom_weak::types::Enum::from_u32(2),
                )
                .with_property("Gravity", 50.0f32),
        );
        let camera = dom.insert(workspace, InstanceBuilder::new("Camera"));
        dom.get_by_ref_mut(workspace)
            .unwrap()
            .properties
            .insert("CurrentCamera".into(), RbxVariant::Ref(camera));
        dom.insert(
            workspace,
            InstanceBuilder::new("Part")
                .with_name("Door")
                .with_property("Anchored", true),
        );
        let gui = dom.insert(dom.root_ref(), InstanceBuilder::new("StarterGui"));
        dom.insert(
            gui,
            InstanceBuilder::new("ImageButton")
                .with_name("Vote")
                .with_property(
                    "ImageContent",
                    rbx_dom_weak::types::Content::from_uri("rbxassetid://129956776090795"),
                ),
        );
        for extension in ["rbxl", "rbxlx"] {
            let file = root.join(format!("place.{extension}"));
            RbxPlaceFormat::from_path(&file)
                .unwrap()
                .write(&file, &dom, dom.root().children())
                .unwrap();
            let query = |arguments: &[&str]| {
                crate::app::output::capture_json_output(|| {
                    query_place(
                        QueryPlaceArgs::try_parse_from(
                            ["q", file.to_str().unwrap()].iter().chain(arguments),
                        )
                        .unwrap(),
                    )
                })
            };
            let result = query(&[
                "-c",
                "Workspace",
                "--props",
                "playerCharacterDestroyBehavior,Gravity,CurrentCamera,Name,Position,Bogus",
            ])
            .unwrap();
            let workspace = &result["matches"][0];
            assert_eq!(
                workspace["props"],
                json!({
                    "PlayerCharacterDestroyBehavior": "Enum.PlayerCharacterDestroyBehavior.Enabled",
                    "Gravity": 50.0,
                    "CurrentCamera": "Workspace.Camera",
                    "Name": "Workspace",
                }),
                "{extension}: {result}"
            );
            assert_eq!(workspace["unknown"], json!(["Position", "Bogus"]));
            let several = query(&[
                "-n",
                "Door",
                "-n",
                "Vote",
                "-c",
                "Part",
                "-c",
                "ImageButton",
            ])
            .unwrap();
            let mut names = several["matches"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["path"].as_array().unwrap().last().unwrap().clone())
                .collect::<Vec<_>>();
            names.sort_by_key(|name| name.as_str().unwrap_or_default().to_string());
            assert_eq!(
                names,
                json!(["Door", "Vote"]).as_array().unwrap().clone(),
                "{extension}: several selectors match any of their values"
            );
            let door = query(&[
                "-n",
                "Door",
                "--props",
                "Anchored,Position,StreamingEnabled",
            ])
            .unwrap();
            assert_eq!(door["matches"][0]["props"], json!({"Anchored": true}));
            assert_eq!(door["matches"][0]["notSaved"], json!(["Position"]));
            assert_eq!(door["matches"][0]["unknown"], json!(["StreamingEnabled"]));
            // A legacy name answers with the successor the file saved.
            let vote = query(&["-n", "Vote", "--props", "Image"]).unwrap();
            assert_eq!(
                vote["matches"][0]["props"],
                json!({"Image": "rbxassetid://129956776090795"}),
                "{extension}: {vote}"
            );
            assert!(vote["matches"][0].get("notSaved").is_none());
            let all = query(&["-n", "Door", "--props", "all"]).unwrap();
            assert_eq!(all["matches"][0]["props"]["Anchored"], true);
            let plain = query(&["-n", "Door"]).unwrap();
            assert!(plain["matches"][0].get("props").is_none());
            assert!(query(&["-n", "Door", "--props", ","]).is_err());
        }

        let mut old = RbxWeakDom::new(InstanceBuilder::new("DataModel"));
        let workspace = old.insert(old.root_ref(), InstanceBuilder::new("Workspace"));
        let database = rbx_reflection_database::get().unwrap();
        let mut result = json!({});
        insert_property_values(
            &mut result,
            &old,
            workspace,
            &PropertyRequest::Named(vec!["PlayerCharacterDestroyBehavior"]),
            database,
        );
        assert_eq!(
            result["props"]["PlayerCharacterDestroyBehavior"],
            "Enum.PlayerCharacterDestroyBehavior.Default"
        );
        assert_eq!(
            result["defaulted"],
            json!(["PlayerCharacterDestroyBehavior"])
        );
    }
}
