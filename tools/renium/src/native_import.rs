use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::BufReader;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use rbx_dom_weak::types::{Ref as RbxRef, Variant as RbxVariant};
use rbx_dom_weak::{InstanceBuilder as RbxInstanceBuilder, WeakDom as RbxWeakDom};
use rbx_reflection::{ReflectionDatabase, Scriptability as RbxScriptability};
use serde_json::{Map, Value, json};

use super::bridge_server::BridgeServer;
use super::bytecode_edit::instance_path_parts_key;
use super::bytecode_explorer::explorer_daemon_services;
use super::command_line::PushEditorChangesArgs;
use super::editor_diff::{NativeEditorPropertyRules, append_native_editor_full_property_changes};
use super::editor_review::{studio_pid_for_bridge, studio_title_for_bridge};
use super::editor_types::{
    EditorBinaryImport, EditorBinaryPackageRoot, EditorBinaryRetainedRoot, EditorBinaryRootPath,
    EditorBinaryServiceGroup, EditorChangeSet, EditorInstanceChange, EditorInstancePath,
    EditorSettingsWrite,
};
use super::file_io::{absolutize_under, resolve_project_root_if_present, service_settings_path};
use super::native_editor::{
    EditorBinaryExportFinishGuard, begin_editor_binary_export, rbx_variant_referent,
};
use super::property_schema::{PropertySchemaMap, load_rbx_dom_property_schema};
use super::rbx_decode::{
    NativeOverlayRequest, RbxSettingsConversionOptions, fetch_native_overlay_batches,
    native_property_filter, overlay_property_names_value, rbx_properties_to_settings_records,
};
use super::rbx_encode::{collect_rbx_subtree_preorder, rbx_property_descriptor};
use super::rbx_model::{
    BytecodeModelImportRefs, build_rbx_place, rbx_dom_instance_by_path_unique,
    rbx_dom_instance_path_parts, rbx_dom_path_import_refs,
};
use super::services::explorer_service_order;
use super::settings_bytecode::SettingsBytecode;
use super::studio_native_serializer;
use super::timing::{current_millis, log_timing, verbose_timing_logs};

struct PendingEditorBinaryGroup {
    service: String,
    target_path: Vec<String>,
    roots: Vec<RbxRef>,
}

struct EditorPackageGroupPlan {
    retained_roots: Vec<EditorBinaryRetainedRoot>,
    package_roots: Vec<EditorBinaryPackageRoot>,
    strip_package_payloads: Vec<usize>,
    change_generation: Option<u64>,
}

#[cfg(any(windows, target_os = "macos"))]
fn native_package_preflight_dom(bridge: &BridgeServer) -> Result<RbxWeakDom> {
    let pid = studio_pid_for_bridge(bridge)?;
    let title = studio_title_for_bridge(bridge, pid)?;
    let path = std::env::temp_dir().join("renium-native").join(format!(
        ".renium-package-preflight-{pid}-{}.rbxl",
        current_millis()
    ));
    let result = (|| -> Result<RbxWeakDom> {
        studio_native_serializer::write_live_place(pid, &title, &path)
            .context("Could not capture a read-only Studio package snapshot")?;
        let input =
            File::open(&path).with_context(|| format!("Failed to read {}", path.display()))?;
        rbx_binary::from_reader(BufReader::new(input))
            .context("Studio returned an invalid native package snapshot")
    })();
    let _ = fs::remove_file(&path);
    result
}

#[cfg(not(any(windows, target_os = "macos")))]
fn native_package_preflight_dom(_bridge: &BridgeServer) -> Result<RbxWeakDom> {
    bail!("Read-only Studio package snapshots are supported on Windows and macOS")
}

fn rbx_subtree_contains_class(dom: &RbxWeakDom, root: RbxRef, class_name: &str) -> bool {
    let mut subtree = Vec::new();
    collect_rbx_subtree_preorder(dom, root, &mut subtree);
    subtree.into_iter().any(|referent| {
        dom.get_by_ref(referent)
            .is_some_and(|instance| instance.class.as_str() == class_name)
    })
}

fn ensure_package_fingerprint_references_resolve<'a>(
    instance: &rbx_dom_weak::Instance,
    properties: impl Iterator<Item = (&'a rbx_dom_weak::Ustr, &'a RbxVariant)>,
    refs: &BytecodeModelImportRefs,
) -> Result<()> {
    for (name, value) in properties {
        let Some(referent) = rbx_variant_referent(value) else {
            continue;
        };
        if !referent.is_none() && !refs.path_segments_by_ref.contains_key(&referent) {
            bail!(
                "Package preflight cannot resolve {}.{}",
                instance.class,
                name
            );
        }
    }
    Ok(())
}

struct CanonicalRbxSubtree<'a> {
    dom: &'a RbxWeakDom,
    root: RbxRef,
    refs: &'a BytecodeModelImportRefs,
    logical_properties: Option<&'a HashMap<RbxRef, HashMap<rbx_dom_weak::Ustr, RbxVariant>>>,
    json_properties: Option<&'a HashMap<RbxRef, Map<String, Value>>>,
}

struct CanonicalRbxSubtreeEntry {
    path: Vec<(String, usize)>,
    referent: RbxRef,
}

fn canonical_rbx_subtree_entries(
    subtree_view: &CanonicalRbxSubtree<'_>,
) -> Result<Vec<CanonicalRbxSubtreeEntry>> {
    let mut subtree = Vec::new();
    collect_rbx_subtree_preorder(subtree_view.dom, subtree_view.root, &mut subtree);
    let mut entries = Vec::with_capacity(subtree.len());
    for referent in subtree {
        let path_segments = subtree_view
            .refs
            .path_segments_by_ref
            .get(&referent)
            .context("Package preflight instance path is missing")?;
        let path_ordinals = subtree_view
            .refs
            .path_ordinals_by_ref
            .get(&referent)
            .context("Package preflight instance ordinals are missing")?;
        entries.push(CanonicalRbxSubtreeEntry {
            path: path_segments
                .iter()
                .cloned()
                .zip(path_ordinals.iter().copied())
                .collect(),
            referent,
        });
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

fn canonical_rbx_instance_record(
    subtree: &CanonicalRbxSubtree<'_>,
    referent: RbxRef,
    database: &ReflectionDatabase<'_>,
) -> Result<Value> {
    let instance = subtree
        .dom
        .get_by_ref(referent)
        .context("Package preflight subtree contains a missing instance")?;
    let path_segments = subtree
        .refs
        .path_segments_by_ref
        .get(&referent)
        .context("Package preflight instance path is missing")?;
    let path_ordinals = subtree
        .refs
        .path_ordinals_by_ref
        .get(&referent)
        .context("Package preflight instance ordinals are missing")?;
    let mut property_filter = native_property_filter(database, instance.class.as_str());
    property_filter.allowed.extend(
        instance
            .properties
            .keys()
            .map(|name| name.as_str().to_string()),
    );
    if let Some(logical_properties) = subtree
        .logical_properties
        .and_then(|properties_by_ref| properties_by_ref.get(&referent))
    {
        property_filter.allowed.extend(
            logical_properties
                .keys()
                .map(|name| name.as_str().to_string()),
        );
    }
    ensure_package_fingerprint_references_resolve(
        instance,
        instance.properties.iter(),
        subtree.refs,
    )?;
    let (mut properties, attributes, source) = rbx_properties_to_settings_records(
        instance.class.as_str(),
        instance
            .properties
            .iter()
            .filter(|(_, value)| !matches!(value, RbxVariant::UniqueId(_))),
        database,
        subtree.refs,
        RbxSettingsConversionOptions {
            elide_defaults: true,
            defaults_already_elided: false,
            native_properties_pre_filtered: false,
            native_filter: Some(&property_filter),
        },
    );
    if let Some(logical_properties) = subtree
        .logical_properties
        .and_then(|properties_by_ref| properties_by_ref.get(&referent))
    {
        ensure_package_fingerprint_references_resolve(
            instance,
            logical_properties.iter(),
            subtree.refs,
        )?;
        let (logical, _, _) = rbx_properties_to_settings_records(
            instance.class.as_str(),
            logical_properties.iter(),
            database,
            subtree.refs,
            RbxSettingsConversionOptions {
                elide_defaults: true,
                defaults_already_elided: false,
                native_properties_pre_filtered: false,
                native_filter: Some(&property_filter),
            },
        );
        properties.extend(logical);
    }
    if let Some(logical) = subtree
        .json_properties
        .and_then(|properties_by_ref| properties_by_ref.get(&referent))
    {
        properties.extend(logical.clone());
    }
    Ok(json!({
        "name": instance.name,
        "className": instance.class.as_str(),
        "pathSegments": path_segments,
        "pathOrdinals": path_ordinals,
        "childCount": instance.children().len(),
        "properties": properties,
        "attributes": attributes,
        "source": source,
    }))
}

fn canonical_rbx_subtrees_equal(
    left: CanonicalRbxSubtree<'_>,
    right: CanonicalRbxSubtree<'_>,
    database: &ReflectionDatabase<'_>,
) -> Result<bool> {
    let left_entries = canonical_rbx_subtree_entries(&left)?;
    let right_entries = canonical_rbx_subtree_entries(&right)?;
    if left_entries.len() != right_entries.len() {
        return Ok(false);
    }
    for (left_entry, right_entry) in left_entries.into_iter().zip(right_entries) {
        if left_entry.path != right_entry.path
            || canonical_rbx_instance_record(&left, left_entry.referent, database)?
                != canonical_rbx_instance_record(&right, right_entry.referent, database)?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn collect_rbx_subtree_set(
    dom: &RbxWeakDom,
    roots: impl IntoIterator<Item = RbxRef>,
) -> HashSet<RbxRef> {
    let mut result = HashSet::new();
    for root in roots {
        let mut subtree = Vec::new();
        collect_rbx_subtree_preorder(dom, root, &mut subtree);
        result.extend(subtree);
    }
    result
}

fn package_reference_is_writable(
    database: &ReflectionDatabase<'_>,
    class_name: &str,
    property_name: &str,
) -> bool {
    rbx_property_descriptor(database, class_name, property_name).is_some_and(|descriptor| {
        matches!(
            descriptor.scriptability,
            RbxScriptability::ReadWrite | RbxScriptability::Custom
        )
    })
}

fn reject_unsafe_package_reference_crossings(
    desired_dom: &RbxWeakDom,
    desired_logical_properties_by_ref: &HashMap<RbxRef, HashMap<rbx_dom_weak::Ustr, RbxVariant>>,
    live_dom: &RbxWeakDom,
    groups: &[PendingEditorBinaryGroup],
    plans: &[EditorPackageGroupPlan],
    database: &ReflectionDatabase<'_>,
) -> Result<()> {
    if plans.iter().all(|plan| plan.retained_roots.is_empty()) {
        return Ok(());
    }
    let mut desired_retained_roots = Vec::new();
    let mut desired_swapped_roots = Vec::new();
    let mut live_retained_roots = Vec::new();
    let mut live_swapped_roots = Vec::new();
    for (group, plan) in groups.iter().zip(plans) {
        let retained_indexes = plan
            .retained_roots
            .iter()
            .map(|root| root.payload_index)
            .collect::<HashSet<_>>();
        for (index, root) in group.roots.iter().copied().enumerate() {
            if retained_indexes.contains(&(index + 1)) {
                desired_retained_roots.push(root);
            } else {
                desired_swapped_roots.push(root);
            }
        }
        for descriptor in &plan.retained_roots {
            live_retained_roots.push(rbx_dom_instance_by_path_unique(
                live_dom,
                &descriptor.path_segments,
                &descriptor.path_ordinals,
            )?);
        }
        let retained_live = live_retained_roots.iter().copied().collect::<HashSet<_>>();
        let live_target = rbx_dom_instance_by_path_unique(
            live_dom,
            &group.target_path,
            &vec![1; group.target_path.len()],
        )?;
        let target = live_dom
            .get_by_ref(live_target)
            .context("Studio native import target is missing")?;
        for root in target.children().iter().copied() {
            let Some(instance) = live_dom.get_by_ref(root) else {
                continue;
            };
            if editor_binary_group_includes_root(&group.service, &group.target_path, instance)
                && !retained_live.contains(&root)
            {
                live_swapped_roots.push(root);
            }
        }
    }
    let desired_retained = collect_rbx_subtree_set(desired_dom, desired_retained_roots);
    let desired_swapped = collect_rbx_subtree_set(desired_dom, desired_swapped_roots);
    let live_swapped = collect_rbx_subtree_set(live_dom, live_swapped_roots);
    for source in desired_swapped {
        let Some(instance) = desired_dom.get_by_ref(source) else {
            continue;
        };
        let properties = instance.properties.iter().chain(
            desired_logical_properties_by_ref
                .get(&source)
                .into_iter()
                .flat_map(|properties| properties.iter()),
        );
        for (name, value) in properties {
            let Some(target) = rbx_variant_referent(value) else {
                continue;
            };
            if !target.is_none()
                && desired_retained.contains(&target)
                && !package_reference_is_writable(database, instance.class.as_str(), name.as_str())
            {
                bail!(
                    "Native import cannot retarget {}.{} into a retained package root",
                    instance.class,
                    name
                );
            }
        }
    }
    for source in live_dom.descendants() {
        let source_ref = source.referent();
        if live_swapped.contains(&source_ref) {
            continue;
        }
        for (name, value) in &source.properties {
            let Some(target) = rbx_variant_referent(value) else {
                continue;
            };
            if !target.is_none()
                && live_swapped.contains(&target)
                && !package_reference_is_writable(database, source.class.as_str(), name.as_str())
            {
                bail!(
                    "Native import cannot retarget {}.{} away from a replaced root",
                    source.class,
                    name
                );
            }
        }
    }
    Ok(())
}

fn editor_binary_group_includes_root(
    service: &str,
    target_path: &[String],
    instance: &rbx_dom_weak::Instance,
) -> bool {
    if target_path.len() != 1 {
        return true;
    }
    instance.class.as_str() != "Terrain"
        && !(service == "StarterPlayer"
            && matches!(
                instance.class.as_str(),
                "StarterPlayerScripts" | "StarterCharacterScripts"
            ))
        && !(service == "Workspace"
            && instance.class.as_str() == "Camera"
            && matches!(instance.name.as_str(), "Camera" | "CurrentCamera"))
}

fn package_preflight_overlay_property_requests(
    desired_dom: &RbxWeakDom,
    groups: &[PendingEditorBinaryGroup],
    overlay_candidate_roots: &HashSet<RbxRef>,
    logical_properties_by_ref: &HashMap<RbxRef, HashMap<rbx_dom_weak::Ustr, RbxVariant>>,
    database: &ReflectionDatabase<'_>,
) -> HashMap<String, HashSet<String>> {
    let mut requested = HashMap::<String, HashSet<String>>::new();
    for group in groups {
        if group.service == "Workspace" {
            continue;
        }
        for root in &group.roots {
            if !overlay_candidate_roots.contains(root) {
                continue;
            }
            let mut subtree = Vec::new();
            collect_rbx_subtree_preorder(desired_dom, *root, &mut subtree);
            for referent in subtree {
                let Some(instance) = desired_dom.get_by_ref(referent) else {
                    continue;
                };
                let Some(logical) = logical_properties_by_ref.get(&referent) else {
                    continue;
                };
                let filter = native_property_filter(database, instance.class.as_str());
                let represented = instance
                    .properties
                    .keys()
                    .map(|name| {
                        filter
                            .renamed
                            .get(name.as_str())
                            .map(String::as_str)
                            .unwrap_or(name.as_str())
                            .to_string()
                    })
                    .collect::<HashSet<_>>();
                for (name, value) in logical {
                    if !matches!(value, RbxVariant::UniqueId(_))
                        && !represented.contains(name.as_str())
                    {
                        requested
                            .entry(instance.class.to_string())
                            .or_default()
                            .insert(name.as_str().to_string());
                    }
                }
            }
        }
    }
    requested
}

fn package_preflight_overlay_schema(
    requested: HashMap<String, HashSet<String>>,
    available: &PropertySchemaMap,
) -> Result<PropertySchemaMap> {
    let mut schema = PropertySchemaMap::new();
    let mut unresolved = Vec::new();
    for (class_name, names) in requested {
        let mut found = HashSet::new();
        if let Some(entries) = available.get(&class_name) {
            for entry in entries {
                if names.contains(&entry.name) {
                    found.insert(entry.name.clone());
                    schema
                        .entry(class_name.clone())
                        .or_default()
                        .push(entry.clone());
                }
            }
        }
        unresolved.extend(
            names
                .difference(&found)
                .map(|name| format!("{class_name}.{name}")),
        );
    }
    if !unresolved.is_empty() {
        unresolved.sort();
        unresolved.truncate(12);
        bail!(
            "Studio cannot read package preflight properties: {}",
            unresolved.join(", ")
        );
    }
    Ok(schema)
}

fn fetch_package_preflight_overlay_properties(
    bridge: &BridgeServer,
    export: &EditorBinaryImport,
    live_dom: &RbxWeakDom,
    schema: &PropertySchemaMap,
    package_services: &HashSet<String>,
) -> Result<HashMap<RbxRef, Map<String, Value>>> {
    if schema.is_empty() {
        return Ok(HashMap::new());
    }
    let export_id = export
        .export_id
        .as_deref()
        .context("Package preflight export id is missing")?;
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let filters = schema
        .keys()
        .map(|class_name| {
            (
                class_name.clone(),
                native_property_filter(database, class_name),
            )
        })
        .collect::<HashMap<_, _>>();
    let names = overlay_property_names_value(schema, &filters);
    let mut properties_by_ref = HashMap::new();
    for group in &export.groups {
        if !package_services.contains(&group.service)
            || !group
                .class_names
                .iter()
                .any(|class_name| schema.contains_key(class_name))
        {
            continue;
        }
        let marker =
            rbx_dom_instance_by_path_unique(live_dom, std::slice::from_ref(&group.service), &[1])?;
        let mut preorder = Vec::new();
        collect_rbx_subtree_preorder(live_dom, marker, &mut preorder);
        if preorder.len() != group.instance_count {
            bail!(
                "Studio package preflight index for {} contains {} instances; expected {}",
                group.service,
                preorder.len(),
                group.instance_count
            );
        }
        let overlay = fetch_native_overlay_batches(
            bridge,
            NativeOverlayRequest {
                service: &group.service,
                start_index: 1,
                take_count: group.instance_count,
                instance_count: group.instance_count,
                overlay_id: export_id,
                overlay_variant: "package-preflight-defaults",
                include_debug_ids: false,
                overlay_names: &names,
                overlay_schema: schema,
                enum_value_names_by_type: &export.enum_value_names_by_type,
                class_names: &group.class_names,
            },
        )?;
        for item in overlay.items {
            if item.properties.is_empty() {
                continue;
            }
            let referent = item
                .instance_index
                .checked_sub(1)
                .and_then(|index| preorder.get(index))
                .copied()
                .context("Studio package preflight overlay index is invalid")?;
            properties_by_ref
                .entry(referent)
                .or_insert_with(Map::new)
                .extend(item.properties);
        }
    }
    Ok(properties_by_ref)
}

struct EditorServiceChangeGenerations {
    generations: HashMap<String, u64>,
    has_package_links: Option<HashMap<String, bool>>,
}

fn editor_service_change_generations(
    bridge: &BridgeServer,
    services: &[String],
) -> Result<EditorServiceChangeGenerations> {
    let result = bridge.call(
        "getEditorServiceChangeGenerations",
        json!({ "services": services }),
    )?;
    let values = result
        .get("generations")
        .and_then(Value::as_object)
        .context("Studio returned invalid service change generations")?;
    let generations = services
        .iter()
        .map(|service| {
            let generation = values
                .get(service)
                .and_then(Value::as_u64)
                .with_context(|| format!("Studio omitted the {service} change generation"))?;
            Ok((service.clone(), generation))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let has_package_links = result
        .get("hasPackageLinks")
        .and_then(Value::as_object)
        .map(|package_values| {
            services
                .iter()
                .map(|service| {
                    let has_package_link = package_values
                        .get(service)
                        .and_then(Value::as_bool)
                        .with_context(|| format!("Studio omitted the {service} package state"))?;
                    Ok((service.clone(), has_package_link))
                })
                .collect::<Result<HashMap<_, _>>>()
        })
        .transpose()?;
    Ok(EditorServiceChangeGenerations {
        generations,
        has_package_links,
    })
}

struct EditorPackagePreflightLive {
    dom: Option<RbxWeakDom>,
    generations: HashMap<String, u64>,
}

fn capture_editor_package_preflight_live(
    bridge: &BridgeServer,
    service_names: &[String],
) -> Result<EditorPackagePreflightLive> {
    let started = Instant::now();
    let first = editor_service_change_generations(bridge, service_names)?;
    log_timing("package preflight generation read", started);
    if first
        .has_package_links
        .as_ref()
        .is_some_and(|states| !states.values().any(|has_package_link| *has_package_link))
    {
        let generations = editor_service_change_generations(bridge, service_names)?.generations;
        if generations != first.generations {
            bail!("Studio changed while Renium checked package state; retry the sync");
        }
        return Ok(EditorPackagePreflightLive {
            dom: None,
            generations,
        });
    }
    let started = Instant::now();
    let dom = native_package_preflight_dom(bridge)?;
    log_timing("package preflight native snapshot", started);
    let generations = editor_service_change_generations(bridge, service_names)?.generations;
    if generations != first.generations {
        bail!("Studio changed while Renium captured the package snapshot; retry the sync");
    }
    Ok(EditorPackagePreflightLive {
        dom: Some(dom),
        generations,
    })
}

fn plan_editor_package_root_retention(
    bridge: &BridgeServer,
    desired_dom: &RbxWeakDom,
    groups: &[PendingEditorBinaryGroup],
    logical_properties_by_ref: &HashMap<RbxRef, HashMap<rbx_dom_weak::Ustr, RbxVariant>>,
    live: EditorPackagePreflightLive,
) -> Result<Vec<EditorPackageGroupPlan>> {
    let total_started = Instant::now();
    let EditorPackagePreflightLive {
        dom: live_dom,
        generations: live_generations,
    } = live;
    let Some(live_dom) = live_dom else {
        return groups
            .iter()
            .map(|group| {
                let strip_package_payloads = group
                    .roots
                    .iter()
                    .enumerate()
                    .filter_map(|(index, root)| {
                        rbx_subtree_contains_class(desired_dom, *root, "PackageLink")
                            .then_some(index + 1)
                    })
                    .collect();
                let change_generation = live_generations
                    .get(&group.service)
                    .copied()
                    .with_context(|| {
                        format!(
                            "Studio cannot guard native import target {} against concurrent edits",
                            group.target_path.join(".")
                        )
                    })?;
                Ok(EditorPackageGroupPlan {
                    retained_roots: Vec::new(),
                    package_roots: Vec::new(),
                    strip_package_payloads,
                    change_generation: Some(change_generation),
                })
            })
            .collect();
    };
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let mut package_pairs = Vec::new();
    for group in groups {
        if group.service == "Workspace" {
            continue;
        }
        for desired_root in &group.roots {
            if !rbx_subtree_contains_class(desired_dom, *desired_root, "PackageLink") {
                continue;
            }
            let (path_segments, path_ordinals) =
                rbx_dom_instance_path_parts(desired_dom, *desired_root);
            let Ok(live_root) =
                rbx_dom_instance_by_path_unique(&live_dom, &path_segments, &path_ordinals)
            else {
                continue;
            };
            if !rbx_subtree_contains_class(&live_dom, live_root, "PackageLink") {
                continue;
            }
            package_pairs.push((*desired_root, live_root));
        }
    }
    let desired_refs = if package_pairs.is_empty() {
        None
    } else {
        let started = Instant::now();
        let refs = rbx_dom_path_import_refs(desired_dom, false);
        log_timing("package preflight desired canonical paths", started);
        Some(refs)
    };
    let live_refs = if package_pairs.is_empty() {
        None
    } else {
        let started = Instant::now();
        let refs = rbx_dom_path_import_refs(&live_dom, false);
        log_timing("package preflight live canonical paths", started);
        Some(refs)
    };
    let started = Instant::now();
    let overlay_candidate_roots = package_pairs
        .par_iter()
        .map(|(desired_root, live_root)| {
            let unchanged = canonical_rbx_subtrees_equal(
                CanonicalRbxSubtree {
                    dom: desired_dom,
                    root: *desired_root,
                    refs: desired_refs
                        .as_ref()
                        .context("Desired package paths were not prepared")?,
                    logical_properties: None,
                    json_properties: None,
                },
                CanonicalRbxSubtree {
                    dom: &live_dom,
                    root: *live_root,
                    refs: live_refs
                        .as_ref()
                        .context("Live package paths were not prepared")?,
                    logical_properties: None,
                    json_properties: None,
                },
                database,
            )?;
            Ok(unchanged.then_some(*desired_root))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<HashSet<_>>();
    log_timing("package preflight native package comparison", started);
    let overlay_requests = package_preflight_overlay_property_requests(
        desired_dom,
        groups,
        &overlay_candidate_roots,
        logical_properties_by_ref,
        database,
    );
    let mut finish_guard = None;
    let live_export = if overlay_requests.is_empty() {
        None
    } else {
        let started = Instant::now();
        let export = begin_editor_binary_export(bridge, false, None, true)?;
        log_timing("package preflight metadata export begin", started);
        finish_guard = Some(EditorBinaryExportFinishGuard {
            bridge,
            export_id: export.export_id.clone(),
        });
        for group in &export.groups {
            let expected = live_generations.get(&group.service).with_context(|| {
                format!("Studio returned an unexpected {} export", group.service)
            })?;
            if group.change_generation != Some(*expected) {
                bail!("Studio changed after Renium captured the package snapshot; retry the sync");
            }
        }
        Some(export)
    };
    let empty_schema = PropertySchemaMap::new();
    let started = Instant::now();
    let overlay_schema = package_preflight_overlay_schema(
        overlay_requests,
        live_export
            .as_ref()
            .map(|export| &export.property_schema_by_class)
            .unwrap_or(&empty_schema),
    )?;
    if verbose_timing_logs() {
        let mut properties = overlay_schema
            .iter()
            .flat_map(|(class_name, entries)| {
                entries
                    .iter()
                    .map(move |entry| format!("{class_name}.{}", entry.name))
            })
            .collect::<Vec<_>>();
        properties.sort();
        eprintln!(
            "[renium] package preflight overlay properties: {}",
            properties.join(",")
        );
    }
    log_timing("package preflight overlay schema", started);
    let started = Instant::now();
    let package_services = groups
        .iter()
        .filter(|group| {
            group.service != "Workspace"
                && group
                    .roots
                    .iter()
                    .any(|root| overlay_candidate_roots.contains(root))
        })
        .map(|group| group.service.clone())
        .collect::<HashSet<_>>();
    let live_properties_by_ref = if overlay_schema.is_empty() {
        HashMap::new()
    } else {
        fetch_package_preflight_overlay_properties(
            bridge,
            live_export
                .as_ref()
                .context("Package preflight export was not prepared")?,
            &live_dom,
            &overlay_schema,
            &package_services,
        )?
    };
    log_timing("package preflight overlay fetch", started);
    if let Some(guard) = finish_guard.as_mut() {
        guard.finish(false)?;
    }
    let started = Instant::now();
    let mut plans = Vec::with_capacity(groups.len());
    for group in groups {
        let live_target = rbx_dom_instance_by_path_unique(
            &live_dom,
            &group.target_path,
            &vec![1; group.target_path.len()],
        )
        .with_context(|| {
            format!(
                "Studio is missing the native import target {}",
                group.target_path.join(".")
            )
        })?;
        let live_target_instance = live_dom
            .get_by_ref(live_target)
            .context("Studio native import target is missing")?;
        let mut package_roots = Vec::new();
        for live_root in live_target_instance.children().iter().copied() {
            let Some(instance) = live_dom.get_by_ref(live_root) else {
                continue;
            };
            if !editor_binary_group_includes_root(&group.service, &group.target_path, instance)
                || !rbx_subtree_contains_class(&live_dom, live_root, "PackageLink")
            {
                continue;
            }
            let (path_segments, path_ordinals) = rbx_dom_instance_path_parts(&live_dom, live_root);
            package_roots.push(EditorBinaryPackageRoot {
                path_segments,
                path_ordinals,
                class_name: instance.class.to_string(),
            });
        }
        let mut retained_roots = Vec::new();
        let mut strip_package_payloads = Vec::new();
        for (payload_index, desired_root) in group.roots.iter().copied().enumerate() {
            let (path_segments, path_ordinals) =
                rbx_dom_instance_path_parts(desired_dom, desired_root);
            let desired_has_package =
                rbx_subtree_contains_class(desired_dom, desired_root, "PackageLink");
            let live_root =
                rbx_dom_instance_by_path_unique(&live_dom, &path_segments, &path_ordinals).ok();
            let live_has_package = live_root
                .is_some_and(|root| rbx_subtree_contains_class(&live_dom, root, "PackageLink"));
            if !desired_has_package && !live_has_package {
                continue;
            }
            let mut unchanged = false;
            if group.service != "Workspace"
                && desired_has_package
                && live_has_package
                && overlay_candidate_roots.contains(&desired_root)
            {
                let live_root = live_root.context("Package-bearing Studio root is missing")?;
                let desired_instance = desired_dom
                    .get_by_ref(desired_root)
                    .context("Package-bearing project root is missing")?;
                let desired_matches = group
                    .roots
                    .iter()
                    .filter_map(|referent| desired_dom.get_by_ref(*referent))
                    .filter(|instance| {
                        instance.name == desired_instance.name
                            && instance.class == desired_instance.class
                    })
                    .count();
                let live_matches = live_target_instance
                    .children()
                    .iter()
                    .filter_map(|referent| live_dom.get_by_ref(*referent))
                    .filter(|instance| {
                        editor_binary_group_includes_root(
                            &group.service,
                            &group.target_path,
                            instance,
                        ) && instance.name == desired_instance.name
                            && instance.class == desired_instance.class
                    })
                    .count();
                if desired_matches != 1 || live_matches != 1 {
                    bail!(
                        "Package root {} cannot be matched uniquely between the project and Studio",
                        path_segments.join(".")
                    );
                }
                unchanged = canonical_rbx_subtrees_equal(
                    CanonicalRbxSubtree {
                        dom: desired_dom,
                        root: desired_root,
                        refs: desired_refs
                            .as_ref()
                            .context("Desired package paths were not prepared")?,
                        logical_properties: Some(logical_properties_by_ref),
                        json_properties: None,
                    },
                    CanonicalRbxSubtree {
                        dom: &live_dom,
                        root: live_root,
                        refs: live_refs
                            .as_ref()
                            .context("Live package paths were not prepared")?,
                        logical_properties: None,
                        json_properties: Some(&live_properties_by_ref),
                    },
                    database,
                )?;
                if unchanged {
                    let mut subtree = Vec::new();
                    collect_rbx_subtree_preorder(desired_dom, desired_root, &mut subtree);
                    let instance = desired_dom
                        .get_by_ref(desired_root)
                        .context("Package-bearing import root is missing")?;
                    let (live_path_segments, live_path_ordinals) =
                        rbx_dom_instance_path_parts(&live_dom, live_root);
                    retained_roots.push(EditorBinaryRetainedRoot {
                        path_segments: live_path_segments,
                        path_ordinals: live_path_ordinals,
                        class_name: instance.class.to_string(),
                        payload_index: payload_index + 1,
                        instance_count: subtree.len(),
                    });
                }
            }
            if unchanged {
                continue;
            }
            if desired_has_package {
                strip_package_payloads.push(payload_index + 1);
            }
        }
        let change_generation = Some(live_generations.get(&group.service).copied().with_context(
            || {
                format!(
                    "Studio cannot guard native import target {} against concurrent edits",
                    group.target_path.join(".")
                )
            },
        )?);
        plans.push(EditorPackageGroupPlan {
            retained_roots,
            package_roots,
            strip_package_payloads,
            change_generation,
        });
    }
    log_timing("package preflight package comparison", started);
    let started = Instant::now();
    reject_unsafe_package_reference_crossings(
        desired_dom,
        logical_properties_by_ref,
        &live_dom,
        groups,
        &plans,
        database,
    )?;
    log_timing("package preflight reference validation", started);
    log_timing("package preflight total", total_started);
    Ok(plans)
}

struct PreparedEditorBinaryImport {
    binary_import: EditorBinaryImport,
    documents_by_service: HashMap<String, SettingsBytecode>,
    paths_by_service: HashMap<String, Vec<Option<EditorInstancePath>>>,
    settings_writes: Vec<EditorSettingsWrite>,
}

pub(super) fn build_editor_binary_import(
    args: &PushEditorChangesArgs,
    changes: &EditorChangeSet,
    bridge: &BridgeServer,
) -> Result<Option<EditorBinaryImport>> {
    if changes.files_to_studio_filters_active
        || changes.instance_changes.is_empty()
        || changes
            .instance_changes
            .iter()
            .any(|change| !change.preserve_instances.is_empty())
    {
        return Ok(None);
    }
    let services = changes
        .instance_changes
        .iter()
        .filter(|change| change.mode == "reconcileService" && change.allow_deletes)
        .map(|change| change.service.clone())
        .collect::<HashSet<_>>();
    if services.is_empty()
        || changes
            .instance_changes
            .iter()
            .any(|change| !services.contains(&change.service))
    {
        return Ok(None);
    }
    if changes
        .source_changes
        .iter()
        .any(|change| !services.contains(&change.service))
        || changes
            .property_changes
            .iter()
            .any(|change| !services.contains(&change.service))
    {
        return Ok(None);
    }
    let document_overrides = changes
        .settings_writes
        .iter()
        .filter_map(|write| {
            let service = write.path.parent()?.file_name()?.to_str()?.to_string();
            Some((service, write.document.clone()))
        })
        .collect::<HashMap<_, _>>();
    Ok(build_editor_binary_import_for_services(
        args,
        services,
        Some(&document_overrides),
        bridge,
        false,
    )?
    .map(|prepared| prepared.binary_import))
}

fn build_editor_binary_import_for_services(
    args: &PushEditorChangesArgs,
    services: HashSet<String>,
    document_overrides: Option<&HashMap<String, SettingsBytecode>>,
    bridge: &BridgeServer,
    merge_source_files: bool,
) -> Result<Option<PreparedEditorBinaryImport>> {
    let project_root = resolve_project_root_if_present(&args.project_root)?;
    let src_root = absolutize_under(&project_root, &args.src_dir);
    let mut ordered_services = services.iter().cloned().collect::<Vec<_>>();
    ordered_services.sort_by(|a, b| {
        explorer_service_order(a)
            .unwrap_or(usize::MAX)
            .cmp(&explorer_service_order(b).unwrap_or(usize::MAX))
            .then_with(|| a.cmp(b))
    });
    let build_services = ordered_services.clone();
    let (build, live_preflight) = rayon::join(
        || {
            let started = Instant::now();
            let result = build_rbx_place(
                &src_root,
                build_services,
                document_overrides,
                true,
                true,
                merge_source_files,
            );
            log_timing("native editor import place build", started);
            result
        },
        || capture_editor_package_preflight_live(bridge, &ordered_services),
    );
    let mut build = build?;
    let live_preflight = live_preflight?;
    if build.service_roots.len() != services.len() {
        return Ok(None);
    }
    let phase_started = Instant::now();
    let mut pending_groups = Vec::with_capacity(build.service_roots.len());
    for (service, root_ref) in &build.service_roots {
        let root = build
            .dom
            .get_by_ref(*root_ref)
            .ok_or_else(|| anyhow::anyhow!("Export service root is missing"))?;
        let mut children = Vec::new();
        let mut nested_groups = Vec::new();
        for referent in root.children().iter().copied() {
            let Some(instance) = build.dom.get_by_ref(referent) else {
                continue;
            };
            if instance.class.as_str() == "Terrain"
                || (service == "StarterPlayer"
                    && matches!(
                        instance.class.as_str(),
                        "StarterPlayerScripts" | "StarterCharacterScripts"
                    ))
                || (service == "Workspace"
                    && instance.class.as_str() == "Camera"
                    && matches!(instance.name.as_str(), "Camera" | "CurrentCamera"))
            {
                nested_groups.push((instance.name.clone(), instance.children().to_vec()));
            } else {
                children.push(referent);
            }
        }
        if service == "Workspace" {
            children.reverse();
        }
        pending_groups.push(PendingEditorBinaryGroup {
            service: service.clone(),
            target_path: vec![service.clone()],
            roots: children,
        });
        for (target_name, nested_children) in nested_groups {
            pending_groups.push(PendingEditorBinaryGroup {
                service: service.clone(),
                target_path: vec![service.clone(), target_name],
                roots: nested_children,
            });
        }
    }
    let mut imported_refs = HashSet::new();
    for group in &pending_groups {
        for root in &group.roots {
            let mut subtree = Vec::new();
            collect_rbx_subtree_preorder(&build.dom, *root, &mut subtree);
            imported_refs.extend(subtree);
        }
    }
    let mut external_reference_properties = Vec::new();
    let mut post_apply_properties_by_path = HashMap::<String, HashSet<String>>::new();
    for referent in imported_refs.iter().copied() {
        let Some(instance) = build.dom.get_by_ref(referent) else {
            continue;
        };
        let external_names = instance
            .properties
            .iter()
            .filter_map(|(name, value)| {
                let target = rbx_variant_referent(value)?;
                (!target.is_none() && !imported_refs.contains(&target))
                    .then(|| name.as_str().to_string())
            })
            .collect::<Vec<_>>();
        if external_names.is_empty() {
            continue;
        }
        let (segments, ordinals) = rbx_dom_instance_path_parts(&build.dom, referent);
        let path_key = instance_path_parts_key(&segments, &ordinals);
        post_apply_properties_by_path
            .entry(path_key)
            .or_default()
            .extend(external_names.iter().cloned());
        external_reference_properties
            .extend(external_names.into_iter().map(|name| (referent, name)));
    }
    for (referent, name) in external_reference_properties {
        if let Some(instance) = build.dom.get_by_ref_mut(referent) {
            instance
                .properties
                .remove(&rbx_dom_weak::Ustr::from(name.as_str()));
        }
    }
    log_timing("native editor import group preparation", phase_started);
    let package_plans = plan_editor_package_root_retention(
        bridge,
        &build.dom,
        &pending_groups,
        &build.logical_properties_by_ref,
        live_preflight,
    )?;
    let phase_started = Instant::now();
    let mut groups = Vec::with_capacity(pending_groups.len());
    let mut top_level_refs = Vec::with_capacity(pending_groups.len());
    let mut instance_count = 0usize;
    for (pending, package_plan) in pending_groups.into_iter().zip(package_plans) {
        let group_index = groups.len() + 1;
        let payload_root_name = format!("__ReniumImportGroup_{group_index}");
        let root_paths = pending
            .roots
            .iter()
            .copied()
            .map(|referent| {
                let (path_segments, path_ordinals) =
                    rbx_dom_instance_path_parts(&build.dom, referent);
                EditorBinaryRootPath {
                    path_segments,
                    path_ordinals,
                }
            })
            .collect::<Vec<_>>();
        let payload_root_ref = build.dom.insert(
            build.dom.root_ref(),
            RbxInstanceBuilder::new("Folder").with_name(payload_root_name.as_str()),
        );
        for referent in &pending.roots {
            let mut subtree = Vec::new();
            collect_rbx_subtree_preorder(&build.dom, *referent, &mut subtree);
            instance_count += subtree.len();
            build.dom.transfer_within(*referent, payload_root_ref);
        }
        top_level_refs.push(payload_root_ref);
        groups.push(EditorBinaryServiceGroup {
            service: pending.service,
            target_path: pending.target_path,
            count: pending.roots.len(),
            payload_root_name: Some(payload_root_name),
            payload_root_names: Vec::new(),
            root_paths,
            instance_count: 0,
            class_names: Vec::new(),
            root_properties: Map::new(),
            retained_roots: package_plan.retained_roots,
            package_roots: package_plan.package_roots,
            strip_package_payloads: package_plan.strip_package_payloads,
            change_generation: package_plan.change_generation,
        });
    }
    log_timing("native editor import payload grouping", phase_started);
    let phase_started = Instant::now();
    let mut bytes = Vec::new();
    rbx_binary::to_writer(&mut bytes, &build.dom, &top_level_refs)
        .context("Failed to encode native Studio import")?;
    log_timing("native editor import binary encode", phase_started);
    Ok(Some(PreparedEditorBinaryImport {
        documents_by_service: build.documents_by_service,
        paths_by_service: build.paths_by_service,
        settings_writes: build.settings_writes,
        binary_import: EditorBinaryImport {
            bytes,
            groups,
            serialization_batches: Vec::new(),
            instance_count,
            export_id: None,
            property_schema_by_class: HashMap::new(),
            enum_value_names_by_type: HashMap::new(),
            post_apply_properties_by_class: build.omitted_properties_by_class,
            post_apply_properties_by_path,
            external_references_post_applied: true,
        },
    }))
}

pub(super) fn prepare_native_editor_full_push(
    args: &PushEditorChangesArgs,
    bridge: &BridgeServer,
) -> Result<(EditorChangeSet, EditorBinaryImport)> {
    let started = Instant::now();
    let project_root = resolve_project_root_if_present(&args.project_root)?;
    let src_root = absolutize_under(&project_root, &args.src_dir);
    let services = explorer_daemon_services(&src_root, "")?
        .into_iter()
        .filter(|service| {
            let service_dir = src_root.join(service);
            service_dir.is_dir() || service_settings_path(&service_dir).is_file()
        })
        .collect::<HashSet<_>>();
    let PreparedEditorBinaryImport {
        binary_import,
        documents_by_service,
        paths_by_service,
        settings_writes,
    } = build_editor_binary_import_for_services(args, services, None, bridge, true)?
        .context("The project could not be represented as a native full sync")?;

    let property_schema_by_class = load_rbx_dom_property_schema(&project_root)?.unwrap_or_default();
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let mut changes = EditorChangeSet {
        settings_writes,
        ..EditorChangeSet::default()
    };
    let mut ordered_services = documents_by_service.keys().cloned().collect::<Vec<_>>();
    ordered_services.sort_by(|a, b| {
        explorer_service_order(a)
            .unwrap_or(usize::MAX)
            .cmp(&explorer_service_order(b).unwrap_or(usize::MAX))
            .then_with(|| a.cmp(b))
    });
    for service in ordered_services {
        changes.instance_changes.push(EditorInstanceChange {
            mode: "reconcileService".to_string(),
            service: service.clone(),
            allow_deletes: true,
            instances: Vec::new(),
            preserve_instances: Vec::new(),
        });
        append_native_editor_full_property_changes(
            &mut changes,
            &documents_by_service[&service],
            &paths_by_service[&service],
            &service,
            NativeEditorPropertyRules {
                property_schema_by_class: &property_schema_by_class,
                post_apply_properties_by_class: &binary_import.post_apply_properties_by_class,
                post_apply_properties_by_path: &binary_import.post_apply_properties_by_path,
                database,
            },
        );
    }
    if verbose_timing_logs() {
        let property_count = changes
            .property_changes
            .iter()
            .map(|change| change.properties.len())
            .sum::<usize>();
        eprintln!(
            "[renium] native editor post-apply: instances={}, properties={}, external_refs={}",
            changes.property_changes.len(),
            property_count,
            binary_import.post_apply_properties_by_path.len()
        );
    }
    log_timing("native editor full push preparation", started);
    Ok((changes, binary_import))
}
