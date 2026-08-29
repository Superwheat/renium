use std::collections::{HashMap, HashSet};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use rbx_dom_weak::types::{Ref as RbxRef, Variant as RbxVariant};
use rbx_dom_weak::{InstanceBuilder as RbxInstanceBuilder, WeakDom as RbxWeakDom};
use rbx_reflection::{ReflectionDatabase, Scriptability as RbxScriptability};
use serde_json::{Map, Value, json};

use crate::app::timing::{log_timing, verbose_timing_logs};
use crate::bytecode::edit::instance_path_parts_key;
use crate::bytecode::explorer::explorer_daemon_services;
use crate::cli::PushEditorChangesArgs;
use crate::editor::diff::{NativeEditorPropertyRules, append_native_editor_full_property_changes};
use crate::editor::types::{
    EditorBinaryExport, EditorBinaryImport, EditorBinaryImportGroup, EditorBinaryPackageRoot,
    EditorBinaryRetainedRoot, EditorBinaryRootPath, EditorChangeSet, EditorInstanceChange,
    EditorInstancePath, EditorSettingsWrite,
};
use crate::rbx::decode::{
    NativeOverlayRequest, NativePropertyFilter, RbxSettingsConversionOptions,
    fetch_native_overlay_batches, native_property_filter, overlay_property_names_value,
    rbx_model_primary_part_is_set, rbx_properties_to_settings_records,
};
use crate::rbx::encode::{collect_rbx_subtree_preorder, rbx_property_descriptor};
use crate::rbx::model::{
    BytecodeModelImportRefs, build_rbx_place, rbx_dom_instance_by_path_unique,
    rbx_dom_instance_path_parts, rbx_dom_path_import_refs,
};
use crate::roblox::schema::{PropertySchemaMap, load_rbx_dom_property_schema};
use crate::roblox::services::explorer_service_order;
use crate::settings::bytecode::{
    SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance,
};
use crate::settings::equivalence::{
    align_settings_ids_to_reference, settings_documents_equivalent,
    stabilize_settings_reference_ids,
};
use crate::studio::bridge::BridgeServer;
use crate::studio::native::editor::{
    EditorBinaryExportFinishGuard, begin_editor_binary_export, rbx_variant_referent,
    receive_editor_binary_export_bytes,
};
use crate::system::files::{
    absolutize_under, resolve_project_root_if_present, service_settings_path,
};

struct PendingEditorBinaryGroup {
    service: String,
    target_path: Vec<String>,
    roots: Vec<RbxRef>,
}

struct EditorPackageGroupPlan {
    retained_roots: Vec<EditorBinaryRetainedRoot>,
    package_roots: Vec<EditorBinaryPackageRoot>,
    change_generation: Option<u64>,
}

fn pending_editor_binary_groups(
    dom: &RbxWeakDom,
    service_roots: &[(String, RbxRef)],
) -> Result<Vec<PendingEditorBinaryGroup>> {
    let mut groups = Vec::with_capacity(service_roots.len());
    for (service, root_ref) in service_roots {
        let root = dom
            .get_by_ref(*root_ref)
            .context("Export service root is missing")?;
        let mut children = Vec::new();
        let mut nested_groups = Vec::new();
        for referent in root.children().iter().copied() {
            let Some(instance) = dom.get_by_ref(referent) else {
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
        groups.push(PendingEditorBinaryGroup {
            service: service.clone(),
            target_path: vec![service.clone()],
            roots: children,
        });
        for (target_name, nested_children) in nested_groups {
            groups.push(PendingEditorBinaryGroup {
                service: service.clone(),
                target_path: vec![service.clone(), target_name],
                roots: nested_children,
            });
        }
    }
    Ok(groups)
}

fn rbx_subtree_contains_class(dom: &RbxWeakDom, root: RbxRef, class_name: &str) -> bool {
    let mut pending = vec![root];
    while let Some(referent) = pending.pop() {
        let Some(instance) = dom.get_by_ref(referent) else {
            continue;
        };
        if instance.class.as_str() == class_name {
            return true;
        }
        pending.extend(instance.children().iter().rev().copied());
    }
    false
}

fn package_roots_for_groups(
    dom: &RbxWeakDom,
    groups: &[PendingEditorBinaryGroup],
) -> HashSet<RbxRef> {
    groups
        .iter()
        .flat_map(|group| group.roots.iter().copied())
        .collect::<Vec<_>>()
        .into_par_iter()
        .filter(|root| rbx_subtree_contains_class(dom, *root, "PackageLink"))
        .collect()
}

fn desired_package_reference_services(
    dom: &RbxWeakDom,
    package_roots: &HashSet<RbxRef>,
    logical_properties_by_ref: &HashMap<RbxRef, HashMap<rbx_dom_weak::Ustr, RbxVariant>>,
    refs: &BytecodeModelImportRefs,
) -> Option<HashSet<String>> {
    let mut services = HashSet::new();
    for root in package_roots.iter().copied() {
        let mut subtree = Vec::new();
        collect_rbx_subtree_preorder(dom, root, &mut subtree);
        for referent in subtree {
            let instance = dom.get_by_ref(referent)?;
            for value in instance.properties.values().chain(
                logical_properties_by_ref
                    .get(&referent)
                    .into_iter()
                    .flat_map(|properties| properties.values()),
            ) {
                let Some(target) = rbx_variant_referent(value) else {
                    continue;
                };
                if target.is_none() {
                    continue;
                }
                let service = refs.path_segments_by_ref.get(&target)?.first()?.clone();
                services.insert(service);
            }
        }
    }
    Some(services)
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

#[derive(Clone, Copy)]
struct CanonicalRbxSubtree<'a> {
    dom: &'a RbxWeakDom,
    root: RbxRef,
    refs: &'a BytecodeModelImportRefs,
    logical_properties: Option<&'a HashMap<RbxRef, HashMap<rbx_dom_weak::Ustr, RbxVariant>>>,
    json_properties: Option<&'a HashMap<RbxRef, Map<String, Value>>>,
}

struct CanonicalRbxSubtreePair {
    desired_root: RbxRef,
    entries: Vec<(RbxRef, RbxRef)>,
}

fn canonical_rbx_import_refs(dom: &RbxWeakDom) -> BytecodeModelImportRefs {
    let mut refs = rbx_dom_path_import_refs(dom, true);
    refs.settings_id_by_ref = refs
        .new_index_by_ref
        .iter()
        .map(|(referent, index)| (*referent, format!("debug:native:{index}")))
        .collect();
    refs
}

fn canonical_rbx_subtree_pair(
    desired: &CanonicalRbxSubtree<'_>,
    live: &CanonicalRbxSubtree<'_>,
) -> Result<Option<CanonicalRbxSubtreePair>> {
    let mut pending = vec![(desired.root, live.root)];
    let mut entries = Vec::new();
    while let Some((desired_referent, live_referent)) = pending.pop() {
        let desired_instance = desired
            .dom
            .get_by_ref(desired_referent)
            .context("Package preflight subtree contains a missing project instance")?;
        let live_instance = live
            .dom
            .get_by_ref(live_referent)
            .context("Package preflight subtree contains a missing Studio instance")?;
        if desired_instance.class != live_instance.class
            || desired_instance.children().len() != live_instance.children().len()
        {
            return Ok(None);
        }
        entries.push((desired_referent, live_referent));
        if desired_instance.children().is_empty() {
            continue;
        }
        let paired_in_order = desired_instance
            .children()
            .iter()
            .zip(live_instance.children())
            .all(|(desired_child, live_child)| {
                desired
                    .dom
                    .get_by_ref(*desired_child)
                    .map(|child| &child.name)
                    == live.dom.get_by_ref(*live_child).map(|child| &child.name)
            });
        if paired_in_order {
            pending.extend(
                desired_instance
                    .children()
                    .iter()
                    .copied()
                    .zip(live_instance.children().iter().copied()),
            );
            continue;
        }
        let mut live_ordinals = HashMap::<&str, usize>::new();
        let mut live_by_name_ordinal = HashMap::with_capacity(live_instance.children().len());
        for child_ref in live_instance.children().iter().copied() {
            let child = live
                .dom
                .get_by_ref(child_ref)
                .context("Package preflight subtree contains a missing Studio child")?;
            let ordinal = live_ordinals.entry(child.name.as_str()).or_default();
            *ordinal += 1;
            live_by_name_ordinal.insert((child.name.as_str(), *ordinal), child_ref);
        }
        let mut desired_ordinals = HashMap::<&str, usize>::new();
        for child_ref in desired_instance.children().iter().copied() {
            let child = desired
                .dom
                .get_by_ref(child_ref)
                .context("Package preflight subtree contains a missing project child")?;
            let ordinal = desired_ordinals.entry(child.name.as_str()).or_default();
            *ordinal += 1;
            let Some(live_child_ref) =
                live_by_name_ordinal.remove(&(child.name.as_str(), *ordinal))
            else {
                return Ok(None);
            };
            pending.push((child_ref, live_child_ref));
        }
    }
    Ok(Some(CanonicalRbxSubtreePair {
        desired_root: desired.root,
        entries,
    }))
}

struct CanonicalRbxInstanceRecord {
    properties: Map<String, Value>,
    attributes: Map<String, Value>,
    source: Option<String>,
}

fn canonical_rbx_instance_record(
    subtree: &CanonicalRbxSubtree<'_>,
    referent: RbxRef,
    database: &ReflectionDatabase<'_>,
    property_filters: &HashMap<String, NativePropertyFilter>,
) -> Result<CanonicalRbxInstanceRecord> {
    let instance = subtree
        .dom
        .get_by_ref(referent)
        .context("Package preflight subtree contains a missing instance")?;
    if instance.class.as_str() == "PackageLink" {
        return Ok(CanonicalRbxInstanceRecord {
            properties: Map::new(),
            attributes: Map::new(),
            source: None,
        });
    }
    let property_filter = property_filters
        .get(instance.class.as_str())
        .context("Package preflight property filter is missing")?;
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
            native_properties_pre_filtered: true,
            native_filter: Some(property_filter),
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
                native_properties_pre_filtered: true,
                native_filter: Some(property_filter),
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
    if rbx_model_primary_part_is_set(
        database,
        instance.class.as_str(),
        instance.properties.iter(),
    ) {
        properties.remove("WorldPivot");
    }
    Ok(CanonicalRbxInstanceRecord {
        properties,
        attributes,
        source,
    })
}

fn canonical_rbx_subtrees_equal(
    left: CanonicalRbxSubtree<'_>,
    right: CanonicalRbxSubtree<'_>,
    _entries: &[(RbxRef, RbxRef)],
    database: &ReflectionDatabase<'_>,
    property_filters: &HashMap<String, NativePropertyFilter>,
) -> Result<bool> {
    let left = canonical_rbx_subtree_document(left, database, property_filters, "desired")?;
    let mut right = canonical_rbx_subtree_document(right, database, property_filters, "live")?;
    stabilize_settings_reference_ids(&mut right);
    align_settings_ids_to_reference(&left, &mut right);
    Ok(settings_documents_equivalent(&left, &right))
}

fn canonical_rbx_subtree_document(
    subtree: CanonicalRbxSubtree<'_>,
    database: &ReflectionDatabase<'_>,
    property_filters: &HashMap<String, NativePropertyFilter>,
    id_prefix: &str,
) -> Result<SettingsBytecode> {
    let mut pending = vec![(subtree.root, None)];
    let mut instances = Vec::new();
    while let Some((referent, parent_index)) = pending.pop() {
        let instance = subtree
            .dom
            .get_by_ref(referent)
            .context("Package preflight subtree contains a missing instance")?;
        let mut record =
            canonical_rbx_instance_record(&subtree, referent, database, property_filters)?;
        if let Some(source) = record.source {
            record
                .properties
                .insert("Source".to_string(), Value::String(source));
        }
        let index = instances.len();
        instances.push(SettingsBytecodeInstance {
            settings_id: subtree
                .refs
                .settings_id_by_ref
                .get(&referent)
                .cloned()
                .unwrap_or_else(|| format!("debug:{id_prefix}:{index}")),
            name: instance.name.clone(),
            class_name: instance.class.to_string(),
            parent_index,
            properties: record.properties,
            attributes: record.attributes,
        });
        pending.extend(
            instance
                .children()
                .iter()
                .rev()
                .copied()
                .map(|child| (child, Some(index))),
        );
    }
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    };
    stabilize_settings_reference_ids(&mut document);
    Ok(document)
}

// These canonicalization tests stay beside the comparison helpers they characterize.
#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod canonical_tests {
    use super::*;

    #[test]
    fn package_link_runtime_state_does_not_change_package_content_equality() {
        let mut desired = RbxWeakDom::new(RbxInstanceBuilder::new("DataModel"));
        let desired_root = desired.insert(
            desired.root_ref(),
            RbxInstanceBuilder::new("PackageLink")
                .with_property("ModifiedState", RbxVariant::Int32(-1)),
        );
        let mut live = RbxWeakDom::new(RbxInstanceBuilder::new("DataModel"));
        let live_root = live.insert(
            live.root_ref(),
            RbxInstanceBuilder::new("PackageLink")
                .with_property("ModifiedState", RbxVariant::Int32(1)),
        );
        let desired_refs = rbx_dom_path_import_refs(&desired, false);
        let live_refs = rbx_dom_path_import_refs(&live, false);
        let desired_subtree = CanonicalRbxSubtree {
            dom: &desired,
            root: desired_root,
            refs: &desired_refs,
            logical_properties: None,
            json_properties: None,
        };
        let live_subtree = CanonicalRbxSubtree {
            dom: &live,
            root: live_root,
            refs: &live_refs,
            logical_properties: None,
            json_properties: None,
        };
        let pair = canonical_rbx_subtree_pair(&desired_subtree, &live_subtree)
            .unwrap()
            .unwrap();
        assert!(
            canonical_rbx_subtrees_equal(
                desired_subtree,
                live_subtree,
                &pair.entries,
                rbx_reflection_database::get().unwrap(),
                &HashMap::new(),
            )
            .unwrap()
        );
    }

    #[test]
    fn package_comparison_matches_reordered_duplicate_siblings_by_content() {
        let mut desired = RbxWeakDom::new(RbxInstanceBuilder::new("DataModel"));
        let desired_root = desired.insert(
            desired.root_ref(),
            RbxInstanceBuilder::new("Model").with_name("Package"),
        );
        for value in ["first", "second"] {
            desired.insert(
                desired_root,
                RbxInstanceBuilder::new("StringValue")
                    .with_name("Node")
                    .with_property("Value", RbxVariant::String(value.to_string())),
            );
        }
        let mut live = RbxWeakDom::new(RbxInstanceBuilder::new("DataModel"));
        let live_root = live.insert(
            live.root_ref(),
            RbxInstanceBuilder::new("Model").with_name("Package"),
        );
        for value in ["second", "first"] {
            live.insert(
                live_root,
                RbxInstanceBuilder::new("StringValue")
                    .with_name("Node")
                    .with_property("Value", RbxVariant::String(value.to_string())),
            );
        }
        let desired_refs = canonical_rbx_import_refs(&desired);
        let live_refs = canonical_rbx_import_refs(&live);
        let desired_subtree = CanonicalRbxSubtree {
            dom: &desired,
            root: desired_root,
            refs: &desired_refs,
            logical_properties: None,
            json_properties: None,
        };
        let live_subtree = CanonicalRbxSubtree {
            dom: &live,
            root: live_root,
            refs: &live_refs,
            logical_properties: None,
            json_properties: None,
        };
        let pair = canonical_rbx_subtree_pair(&desired_subtree, &live_subtree)
            .unwrap()
            .unwrap();
        let database = rbx_reflection_database::get().unwrap();
        let property_filters = ["Model", "StringValue"]
            .into_iter()
            .map(|class_name| {
                (
                    class_name.to_string(),
                    native_property_filter(database, class_name),
                )
            })
            .collect();
        assert!(
            canonical_rbx_subtrees_equal(
                desired_subtree,
                live_subtree,
                &pair.entries,
                database,
                &property_filters,
            )
            .unwrap()
        );
    }

    #[test]
    fn package_comparison_remaps_internal_references_from_global_dom_ids() {
        let build = |leading_folders: usize| {
            let mut dom = RbxWeakDom::new(RbxInstanceBuilder::new("DataModel"));
            for index in 0..leading_folders {
                let root = dom.root_ref();
                dom.insert(
                    root,
                    RbxInstanceBuilder::new("Folder").with_name(format!("Before{index}")),
                );
            }
            let root = dom.root_ref();
            let package = dom.insert(root, RbxInstanceBuilder::new("Model").with_name("Package"));
            let card = dom.insert(
                package,
                RbxInstanceBuilder::new("Model").with_name("CardRig"),
            );
            let primary = dom.insert(card, RbxInstanceBuilder::new("Part").with_name("RootPart"));
            dom.get_by_ref_mut(card)
                .unwrap()
                .properties
                .insert("PrimaryPart".into(), RbxVariant::Ref(primary));
            (dom, package)
        };
        let (desired, desired_root) = build(2);
        let (live, live_root) = build(5);
        let desired_refs = canonical_rbx_import_refs(&desired);
        let live_refs = canonical_rbx_import_refs(&live);
        let desired_subtree = CanonicalRbxSubtree {
            dom: &desired,
            root: desired_root,
            refs: &desired_refs,
            logical_properties: None,
            json_properties: None,
        };
        let live_subtree = CanonicalRbxSubtree {
            dom: &live,
            root: live_root,
            refs: &live_refs,
            logical_properties: None,
            json_properties: None,
        };
        let pair = canonical_rbx_subtree_pair(&desired_subtree, &live_subtree)
            .unwrap()
            .unwrap();
        let database = rbx_reflection_database::get().unwrap();
        let property_filters = ["Model", "Part"]
            .into_iter()
            .map(|class_name| {
                (
                    class_name.to_string(),
                    native_property_filter(database, class_name),
                )
            })
            .collect();
        assert!(
            canonical_rbx_subtrees_equal(
                desired_subtree,
                live_subtree,
                &pair.entries,
                database,
                &property_filters,
            )
            .unwrap()
        );
    }
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
    captured_services: &HashSet<String>,
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
        let live_target = match rbx_dom_instance_by_path_unique(
            live_dom,
            &group.target_path,
            &vec![1; group.target_path.len()],
        ) {
            Ok(target) => target,
            Err(_) if !captured_services.contains(&group.service) => continue,
            Err(error) => return Err(error),
        };
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
    live_dom: &RbxWeakDom,
    package_pairs: &[CanonicalRbxSubtreePair],
    logical_properties_by_ref: &HashMap<RbxRef, HashMap<rbx_dom_weak::Ustr, RbxVariant>>,
    desired_refs: &BytecodeModelImportRefs,
    database: &ReflectionDatabase<'_>,
    property_filters: &HashMap<String, NativePropertyFilter>,
) -> Result<HashMap<String, HashSet<String>>> {
    package_pairs
        .par_iter()
        .map(|pair| {
            pair.entries
                .par_iter()
                .try_fold(
                    HashMap::new,
                    |mut requested: HashMap<String, HashSet<String>>,
                     (desired_referent, live_referent)| {
                        let instance = desired_dom
                            .get_by_ref(*desired_referent)
                            .context("Package preflight project instance is missing")?;
                        let live_instance = live_dom
                            .get_by_ref(*live_referent)
                            .context("Package preflight Studio instance is missing")?;
                        let class_name = instance.class.to_string();
                        let filter = property_filters
                            .get(instance.class.as_str())
                            .context("Package preflight property filter is missing")?;
                        let logical = logical_properties_by_ref.get(desired_referent);
                        let represented = instance
                            .properties
                            .keys()
                            .map(|name| {
                                filter
                                    .renamed
                                    .get(name.as_str())
                                    .map_or(name.as_str(), String::as_str)
                                    .to_string()
                            })
                            .collect::<HashSet<_>>();
                        let live_represented = live_instance
                            .properties
                            .keys()
                            .map(|name| {
                                filter
                                    .renamed
                                    .get(name.as_str())
                                    .map_or(name.as_str(), String::as_str)
                            })
                            .collect::<HashSet<_>>();
                        for (name, value) in &instance.properties {
                            if matches!(value, RbxVariant::UniqueId(_)) {
                                continue;
                            }
                            let output_name = filter
                                .renamed
                                .get(name.as_str())
                                .map_or(name.as_str(), String::as_str);
                            if live_represented.contains(output_name) {
                                continue;
                            }
                            let (properties, _, _) = rbx_properties_to_settings_records(
                                instance.class.as_str(),
                                std::iter::once((name, value)),
                                database,
                                desired_refs,
                                RbxSettingsConversionOptions {
                                    elide_defaults: true,
                                    defaults_already_elided: false,
                                    native_properties_pre_filtered: true,
                                    native_filter: Some(filter),
                                },
                            );
                            requested
                                .entry(class_name.clone())
                                .or_default()
                                .extend(properties.into_iter().map(|(name, _)| name));
                        }
                        if let Some(logical) = logical {
                            for (name, value) in logical {
                                if !matches!(value, RbxVariant::UniqueId(_))
                                    && !represented.contains(name.as_str())
                                {
                                    requested
                                        .entry(class_name.clone())
                                        .or_default()
                                        .insert(name.as_str().to_string());
                                }
                            }
                        }
                        Ok(requested)
                    },
                )
                .try_reduce(HashMap::new, |mut requested, next| {
                    for (class_name, names) in next {
                        requested.entry(class_name).or_default().extend(names);
                    }
                    Ok(requested)
                })
        })
        .try_reduce(HashMap::new, |mut requested, next| {
            for (class_name, names) in next {
                requested.entry(class_name).or_default().extend(names);
            }
            Ok(requested)
        })
}

fn package_preflight_overlay_schema(
    requested: HashMap<String, HashSet<String>>,
    available: &PropertySchemaMap,
) -> Result<PropertySchemaMap> {
    let mut schema = PropertySchemaMap::new();
    let mut unavailable = Vec::new();
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
        unavailable.extend(
            names
                .difference(&found)
                .map(|name| format!("{class_name}.{name}")),
        );
    }
    if verbose_timing_logs() && !unavailable.is_empty() {
        unavailable.sort();
        unavailable.truncate(12);
        eprintln!(
            "[renium] package preflight will replace roots with unreadable properties: {}",
            unavailable.join(", ")
        );
    }
    Ok(schema)
}

fn fetch_package_preflight_overlay_properties(
    bridge: &BridgeServer,
    export: &EditorBinaryExport,
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

pub(crate) fn editor_services_have_package_links(
    bridge: &BridgeServer,
    services: &[String],
) -> Result<bool> {
    Ok(editor_service_change_generations(bridge, services)?
        .has_package_links
        .is_none_or(|services| services.into_values().any(|present| present)))
}

struct EditorPackagePreflightLive<'a> {
    dom: Option<RbxWeakDom>,
    generations: HashMap<String, u64>,
    captured_services: HashSet<String>,
    export: Option<EditorBinaryExport>,
    finish_guard: Option<EditorBinaryExportFinishGuard<'a>>,
}

fn capture_editor_package_preflight_live<'a>(
    bridge: &'a BridgeServer,
    service_names: &[String],
    required_reference_services: Option<&HashSet<String>>,
    force_full_snapshot: bool,
) -> Result<EditorPackagePreflightLive<'a>> {
    capture_editor_package_preflight_live_attempt(
        bridge,
        service_names,
        required_reference_services,
        force_full_snapshot,
        0,
    )
}

fn capture_editor_package_preflight_live_attempt<'a>(
    bridge: &'a BridgeServer,
    service_names: &[String],
    required_reference_services: Option<&HashSet<String>>,
    force_full_snapshot: bool,
    attempt: u8,
) -> Result<EditorPackagePreflightLive<'a>> {
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
            captured_services: HashSet::new(),
            export: None,
            finish_guard: None,
        });
    }
    let service_filter = (!force_full_snapshot)
        .then_some(first.has_package_links.as_ref())
        .flatten()
        .and_then(|states| {
            let filtered = service_names
                .iter()
                .filter(|service| {
                    states.get(*service).copied().unwrap_or(false)
                        || required_reference_services
                            .is_some_and(|required| required.contains(*service))
                })
                .cloned()
                .collect::<Vec<_>>();
            (filtered.len() < service_names.len()).then_some(filtered)
        });
    let started = Instant::now();
    let export = begin_editor_binary_export(bridge, false, None, service_filter.as_deref(), false)?;
    let mut finish_guard = EditorBinaryExportFinishGuard {
        bridge,
        export_id: export.export_id.clone(),
    };
    let bytes = receive_editor_binary_export_bytes(
        bridge,
        export
            .export_id
            .as_deref()
            .context("Package preflight export id is missing")?,
        None,
        None,
    )?;
    let mut dom = rbx_binary::from_reader(std::io::Cursor::new(bytes))
        .context("Studio returned an invalid package snapshot")?;
    let roots = dom.root().children().to_vec();
    let expected_roots = export
        .groups
        .iter()
        .map(|group| group.count + 1)
        .sum::<usize>();
    if roots.len() != expected_roots {
        bail!("Studio package snapshot has the wrong root count");
    }
    let mut cursor = 0;
    for group in &export.groups {
        let marker_ref = roots[cursor];
        cursor += 1;
        let child_refs = roots[cursor..cursor + group.count].to_vec();
        cursor += group.count;
        let marker = dom
            .get_by_ref_mut(marker_ref)
            .context("Studio package snapshot lost a service marker")?;
        marker.class = group.service.as_str().into();
        marker.name.clone_from(&group.service);
        for child_ref in child_refs {
            dom.transfer_within(child_ref, marker_ref);
        }
    }
    let mismatch = export.groups.iter().find_map(|group| {
        let marker =
            rbx_dom_instance_by_path_unique(&dom, std::slice::from_ref(&group.service), &[1])
                .ok()?;
        let mut preorder = Vec::new();
        collect_rbx_subtree_preorder(&dom, marker, &mut preorder);
        (preorder.len() != group.instance_count).then(|| {
            format!(
                "Studio package preflight index for {} contains {} instances; expected {}",
                group.service,
                preorder.len(),
                group.instance_count
            )
        })
    });
    if let Some(message) = mismatch {
        finish_guard.finish(false)?;
        if attempt < 2 {
            return capture_editor_package_preflight_live_attempt(
                bridge,
                service_names,
                required_reference_services,
                force_full_snapshot,
                attempt + 1,
            );
        }
        bail!(message);
    }
    log_timing("package preflight plugin snapshot", started);
    let captured_services = export
        .groups
        .iter()
        .map(|group| group.service.clone())
        .collect();
    Ok(EditorPackagePreflightLive {
        dom: Some(dom),
        generations: first.generations,
        captured_services,
        export: Some(export),
        finish_guard: Some(finish_guard),
    })
}

fn plan_editor_package_root_retention(
    bridge: &BridgeServer,
    desired_dom: &RbxWeakDom,
    groups: &[PendingEditorBinaryGroup],
    desired_package_roots: &HashSet<RbxRef>,
    logical_properties_by_ref: &HashMap<RbxRef, HashMap<rbx_dom_weak::Ustr, RbxVariant>>,
    live: EditorPackagePreflightLive<'_>,
    prepared_desired_refs: Option<BytecodeModelImportRefs>,
) -> Result<Vec<EditorPackageGroupPlan>> {
    let total_started = Instant::now();
    let EditorPackagePreflightLive {
        dom: live_dom,
        generations: live_generations,
        captured_services,
        export: live_export,
        mut finish_guard,
    } = live;
    let Some(live_dom) = live_dom else {
        return groups
            .iter()
            .map(|group| {
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
                    change_generation: Some(change_generation),
                })
            })
            .collect();
    };
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let mut live_root_candidates = Vec::new();
    for group in groups {
        let Ok(live_target) = rbx_dom_instance_by_path_unique(
            &live_dom,
            &group.target_path,
            &vec![1; group.target_path.len()],
        ) else {
            continue;
        };
        let target = live_dom
            .get_by_ref(live_target)
            .context("Studio native import target is missing")?;
        live_root_candidates.extend(target.children().iter().copied().filter(|root| {
            live_dom.get_by_ref(*root).is_some_and(|instance| {
                editor_binary_group_includes_root(&group.service, &group.target_path, instance)
            })
        }));
    }
    let live_package_roots = live_root_candidates
        .into_par_iter()
        .filter(|root| rbx_subtree_contains_class(&live_dom, *root, "PackageLink"))
        .collect::<HashSet<_>>();
    let mut package_pairs = Vec::new();
    for group in groups {
        for desired_root in &group.roots {
            if !desired_package_roots.contains(desired_root) {
                continue;
            }
            let (path_segments, path_ordinals) =
                rbx_dom_instance_path_parts(desired_dom, *desired_root);
            let Ok(live_root) =
                rbx_dom_instance_by_path_unique(&live_dom, &path_segments, &path_ordinals)
            else {
                continue;
            };
            if !live_package_roots.contains(&live_root) {
                continue;
            }
            let desired_instance = desired_dom
                .get_by_ref(*desired_root)
                .context("Package-bearing project root is missing")?;
            let live_instance = live_dom
                .get_by_ref(live_root)
                .context("Package-bearing Studio root is missing")?;
            if desired_instance.class != live_instance.class {
                continue;
            }
            package_pairs.push((*desired_root, live_root));
        }
    }
    let (desired_refs, live_refs) = if package_pairs.is_empty() {
        (None, None)
    } else {
        let started = Instant::now();
        let desired_refs =
            prepared_desired_refs.unwrap_or_else(|| rbx_dom_path_import_refs(desired_dom, false));
        let live_refs = canonical_rbx_import_refs(&live_dom);
        log_timing("package preflight canonical paths", started);
        (Some(desired_refs), Some(live_refs))
    };
    let package_pairs_by_root = package_pairs.iter().copied().collect::<HashMap<_, _>>();
    let started = Instant::now();
    let canonical_package_pairs = package_pairs
        .par_iter()
        .map(|(desired_root, live_root)| {
            canonical_rbx_subtree_pair(
                &CanonicalRbxSubtree {
                    dom: desired_dom,
                    root: *desired_root,
                    refs: desired_refs
                        .as_ref()
                        .context("Desired package paths were not prepared")?,
                    logical_properties: None,
                    json_properties: None,
                },
                &CanonicalRbxSubtree {
                    dom: &live_dom,
                    root: *live_root,
                    refs: live_refs
                        .as_ref()
                        .context("Live package paths were not prepared")?,
                    logical_properties: None,
                    json_properties: None,
                },
            )
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    log_timing("package preflight native package comparison", started);
    let canonical_package_pairs_by_root = canonical_package_pairs
        .iter()
        .map(|pair| (pair.desired_root, pair))
        .collect::<HashMap<_, _>>();
    let mut package_property_filters = HashMap::new();
    if !canonical_package_pairs.is_empty() {
        for class_name in live_export
            .as_ref()
            .context("Package preflight export was not prepared")?
            .groups
            .iter()
            .flat_map(|group| &group.class_names)
        {
            package_property_filters
                .entry(class_name.clone())
                .or_insert_with(|| native_property_filter(database, class_name));
        }
    }
    let started = Instant::now();
    let overlay_requests = package_preflight_overlay_property_requests(
        desired_dom,
        &live_dom,
        &canonical_package_pairs,
        logical_properties_by_ref,
        desired_refs
            .as_ref()
            .context("Desired package paths were not prepared")?,
        database,
        &package_property_filters,
    )?;
    log_timing("package preflight overlay property requests", started);
    let empty_schema = PropertySchemaMap::new();
    let started = Instant::now();
    let overlay_schema = package_preflight_overlay_schema(
        overlay_requests,
        live_export
            .as_ref()
            .map_or(&empty_schema, |export| &export.property_schema_by_class),
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
            group
                .roots
                .iter()
                .any(|root| canonical_package_pairs_by_root.contains_key(root))
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
    let services = live_generations.keys().cloned().collect::<Vec<_>>();
    let generations = editor_service_change_generations(bridge, &services)?.generations;
    if generations != live_generations {
        bail!("Studio changed while Renium captured the package snapshot; retry the sync");
    }
    let started = Instant::now();
    let mut plans = Vec::with_capacity(groups.len());
    for group in groups {
        let live_target = match rbx_dom_instance_by_path_unique(
            &live_dom,
            &group.target_path,
            &vec![1; group.target_path.len()],
        ) {
            Ok(target) => target,
            Err(_) if !captured_services.contains(&group.service) => {
                let change_generation = live_generations
                    .get(&group.service)
                    .copied()
                    .with_context(|| {
                        format!(
                            "Studio cannot guard native import target {} against concurrent edits",
                            group.target_path.join(".")
                        )
                    })?;
                plans.push(EditorPackageGroupPlan {
                    retained_roots: Vec::new(),
                    package_roots: Vec::new(),
                    change_generation: Some(change_generation),
                });
                continue;
            }
            Err(_) => {
                bail!(
                    "Studio is missing the native import target {}",
                    group.target_path.join(".")
                )
            }
        };
        let live_target_instance = live_dom
            .get_by_ref(live_target)
            .context("Studio native import target is missing")?;
        let mut package_roots = Vec::new();
        for live_root in live_target_instance.children().iter().copied() {
            let Some(instance) = live_dom.get_by_ref(live_root) else {
                continue;
            };
            if !editor_binary_group_includes_root(&group.service, &group.target_path, instance)
                || !live_package_roots.contains(&live_root)
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
        let retained_root = |desired_root: RbxRef,
                             live_root: RbxRef,
                             payload_index: usize,
                             payload_omitted: bool|
         -> Result<EditorBinaryRetainedRoot> {
            let desired_instance = desired_dom
                .get_by_ref(desired_root)
                .context("Package-bearing project root is missing")?;
            let (path_segments, _) = rbx_dom_instance_path_parts(desired_dom, desired_root);
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
                    editor_binary_group_includes_root(&group.service, &group.target_path, instance)
                        && instance.name == desired_instance.name
                        && instance.class == desired_instance.class
                })
                .count();
            if desired_matches != 1 || live_matches != 1 {
                bail!(
                    "Package root {} cannot be matched uniquely between the project and Studio",
                    path_segments.join(".")
                );
            }
            let mut subtree = Vec::new();
            collect_rbx_subtree_preorder(desired_dom, desired_root, &mut subtree);
            let (live_path_segments, live_path_ordinals) =
                rbx_dom_instance_path_parts(&live_dom, live_root);
            Ok(EditorBinaryRetainedRoot {
                path_segments: live_path_segments,
                path_ordinals: live_path_ordinals,
                class_name: desired_instance.class.to_string(),
                payload_index: payload_index + 1,
                instance_count: subtree.len(),
                payload_omitted,
            })
        };
        for (payload_index, desired_root) in group.roots.iter().copied().enumerate() {
            let (path_segments, path_ordinals) =
                rbx_dom_instance_path_parts(desired_dom, desired_root);
            let desired_has_package = desired_package_roots.contains(&desired_root);
            let live_root =
                rbx_dom_instance_by_path_unique(&live_dom, &path_segments, &path_ordinals).ok();
            let live_has_package = live_root.is_some_and(|root| live_package_roots.contains(&root));
            if !desired_has_package && !live_has_package {
                continue;
            }
            if live_has_package && !desired_has_package {
                let live_root = live_root.context("Studio package root is missing")?;
                let desired_instance = desired_dom
                    .get_by_ref(desired_root)
                    .context("Project package root is missing")?;
                let live_instance = live_dom
                    .get_by_ref(live_root)
                    .context("Studio package root is missing")?;
                if desired_instance.class == live_instance.class {
                    retained_roots.push(retained_root(
                        desired_root,
                        live_root,
                        payload_index,
                        false,
                    )?);
                }
                continue;
            }
            if desired_has_package && !live_has_package {
                bail!(
                    "Project package root {} has no matching package in Studio; ordinary push cannot create or replace package relationships",
                    path_segments.join(".")
                );
            }
            if desired_has_package && live_has_package {
                let live_root = package_pairs_by_root
                    .get(&desired_root)
                    .copied()
                    .with_context(|| {
                        format!(
                            "Package root {} cannot be preserved safely",
                            path_segments.join(".")
                        )
                    })?;
                let unchanged =
                    if let Some(pair) = canonical_package_pairs_by_root.get(&desired_root) {
                        canonical_rbx_subtrees_equal(
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
                            &pair.entries,
                            database,
                            &package_property_filters,
                        )?
                    } else {
                        false
                    };
                retained_roots.push(retained_root(
                    desired_root,
                    live_root,
                    payload_index,
                    unchanged,
                )?);
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
        &captured_services,
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

pub(crate) fn build_editor_binary_import(
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
    let document_overrides = changes
        .settings_writes
        .iter()
        .filter_map(|write| {
            let service = write.path.parent()?.file_name()?.to_str()?.to_string();
            Some((service, &write.document))
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
    document_overrides: Option<&HashMap<String, &SettingsBytecode>>,
    bridge: &BridgeServer,
    merge_source_files: bool,
) -> Result<Option<PreparedEditorBinaryImport>> {
    let project_root = resolve_project_root_if_present(&args.project.project_root)?;
    let src_root = absolutize_under(&project_root, &args.project.src_root);
    let service_count = services.len();
    let mut ordered_services = services.into_iter().collect::<Vec<_>>();
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
            result.and_then(|build| {
                let pending_groups =
                    pending_editor_binary_groups(&build.dom, &build.service_roots)?;
                let desired_package_roots = package_roots_for_groups(&build.dom, &pending_groups);
                let desired_refs = if build.has_package_links {
                    let started = Instant::now();
                    let refs = canonical_rbx_import_refs(&build.dom);
                    log_timing("package preflight desired canonical paths", started);
                    Some(refs)
                } else {
                    None
                };
                let desired_reference_services = desired_refs.as_ref().and_then(|refs| {
                    desired_package_reference_services(
                        &build.dom,
                        &desired_package_roots,
                        &build.logical_properties_by_ref,
                        refs,
                    )
                });
                Ok((
                    build,
                    desired_refs,
                    pending_groups,
                    desired_package_roots,
                    desired_reference_services,
                ))
            })
        },
        || capture_editor_package_preflight_live(bridge, &ordered_services, None, false),
    );
    let (
        mut build,
        desired_refs,
        pending_groups,
        desired_package_roots,
        desired_reference_services,
    ) = build?;
    let mut live_preflight = live_preflight?;
    if build.service_roots.len() != service_count {
        return Ok(None);
    }
    let phase_started = Instant::now();
    let unresolved_desired_references =
        desired_refs.is_some() && desired_reference_services.is_none();
    let missing_reference_service = desired_reference_services
        .as_ref()
        .is_some_and(|required| !required.is_subset(&live_preflight.captured_services));
    if live_preflight.dom.is_some() && (unresolved_desired_references || missing_reference_service)
    {
        if let Some(guard) = live_preflight.finish_guard.as_mut() {
            guard.finish(false)?;
        }
        let known_services = ordered_services.iter().cloned().collect::<HashSet<_>>();
        let force_full_snapshot = unresolved_desired_references
            || desired_reference_services
                .as_ref()
                .is_some_and(|required| !required.is_subset(&known_services));
        live_preflight = capture_editor_package_preflight_live(
            bridge,
            &ordered_services,
            desired_reference_services.as_ref(),
            force_full_snapshot,
        )?;
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
        let unresolved_names = build.unresolved_reference_properties_by_ref.get(&referent);
        if external_names.is_empty() && unresolved_names.is_none() {
            continue;
        }
        let (segments, ordinals) = rbx_dom_instance_path_parts(&build.dom, referent);
        let path_key = instance_path_parts_key(&segments, &ordinals);
        let post_apply_names = post_apply_properties_by_path.entry(path_key).or_default();
        post_apply_names.extend(external_names.iter().cloned());
        if let Some(names) = unresolved_names {
            post_apply_names.extend(names.iter().map(|name| name.as_str().to_string()));
        }
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
    build
        .omitted_properties_by_class
        .entry("Model".to_string())
        .or_default()
        .insert("WorldPivot".to_string());
    log_timing("native editor import group preparation", phase_started);
    let package_plans = plan_editor_package_root_retention(
        bridge,
        &build.dom,
        &pending_groups,
        &desired_package_roots,
        &build.logical_properties_by_ref,
        live_preflight,
        desired_refs,
    )?;
    let retained_refs = pending_groups
        .iter()
        .zip(&package_plans)
        .flat_map(|(group, plan)| {
            plan.retained_roots
                .iter()
                .map(|root| group.roots[root.payload_index - 1])
        })
        .flat_map(|root| {
            let mut subtree = Vec::new();
            collect_rbx_subtree_preorder(&build.dom, root, &mut subtree);
            subtree
        })
        .collect::<HashSet<_>>();
    if !retained_refs.is_empty() {
        for referent in retained_refs.iter().copied() {
            let Some(instance) = build.dom.get_by_ref(referent) else {
                continue;
            };
            let names = instance
                .properties
                .iter()
                .chain(
                    build
                        .logical_properties_by_ref
                        .get(&referent)
                        .into_iter()
                        .flat_map(|properties| properties.iter()),
                )
                .filter(|(_, value)| {
                    rbx_variant_referent(value).is_some_and(|target| {
                        imported_refs.contains(&target) && !retained_refs.contains(&target)
                    })
                })
                .map(|(name, _)| name.as_str().to_string())
                .collect::<Vec<_>>();
            if names.is_empty() {
                continue;
            }
            let (segments, ordinals) = rbx_dom_instance_path_parts(&build.dom, referent);
            post_apply_properties_by_path
                .entry(instance_path_parts_key(&segments, &ordinals))
                .or_default()
                .extend(names);
        }
        let mut retained_reference_properties = Vec::new();
        for referent in imported_refs.difference(&retained_refs).copied() {
            let Some(instance) = build.dom.get_by_ref(referent) else {
                continue;
            };
            let names = instance
                .properties
                .iter()
                .filter(|(_, value)| {
                    rbx_variant_referent(value)
                        .is_some_and(|target| retained_refs.contains(&target))
                })
                .map(|(name, _)| name.as_str().to_string())
                .collect::<Vec<_>>();
            if names.is_empty() {
                continue;
            }
            let (segments, ordinals) = rbx_dom_instance_path_parts(&build.dom, referent);
            post_apply_properties_by_path
                .entry(instance_path_parts_key(&segments, &ordinals))
                .or_default()
                .extend(names.iter().cloned());
            retained_reference_properties.extend(names.into_iter().map(|name| (referent, name)));
        }
        for (referent, name) in retained_reference_properties {
            if let Some(instance) = build.dom.get_by_ref_mut(referent) {
                instance
                    .properties
                    .remove(&rbx_dom_weak::Ustr::from(name.as_str()));
            }
        }
    }
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
        let omitted_payloads = package_plan
            .retained_roots
            .iter()
            .filter(|root| root.payload_omitted)
            .map(|root| root.payload_index)
            .collect::<HashSet<_>>();
        for (index, referent) in pending.roots.iter().enumerate() {
            let mut subtree = Vec::new();
            collect_rbx_subtree_preorder(&build.dom, *referent, &mut subtree);
            instance_count += subtree.len();
            let payload_name = format!("__ReniumImportRoot_{}", index + 1);
            if omitted_payloads.contains(&(index + 1)) {
                let instance = build
                    .dom
                    .get_by_ref(*referent)
                    .context("Retained package payload root is missing")?;
                let class_name = instance.class;
                build.dom.insert(
                    payload_root_ref,
                    RbxInstanceBuilder::new(class_name).with_name(payload_name),
                );
            } else {
                build
                    .dom
                    .get_by_ref_mut(*referent)
                    .context("Native import payload root is missing")?
                    .name = payload_name;
                build.dom.transfer_within(*referent, payload_root_ref);
            }
        }
        top_level_refs.push(payload_root_ref);
        groups.push(EditorBinaryImportGroup {
            service: pending.service,
            target_path: pending.target_path,
            count: pending.roots.len(),
            payload_root_name,
            root_paths,
            retained_roots: package_plan.retained_roots,
            package_roots: package_plan.package_roots,
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
            instance_count,
            post_apply_properties_by_class: build.omitted_properties_by_class,
            post_apply_properties_by_path,
            external_references_post_applied: true,
        },
    }))
}

pub(crate) fn prepare_native_editor_full_push(
    args: &PushEditorChangesArgs,
    bridge: &BridgeServer,
) -> Result<(EditorChangeSet, EditorBinaryImport)> {
    let started = Instant::now();
    let project_root = resolve_project_root_if_present(&args.project.project_root)?;
    let services = native_editor_full_push_services(args)?;
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
    let phase_started = Instant::now();
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
            &binary_import,
            NativeEditorPropertyRules {
                property_schema_by_class: &property_schema_by_class,
                post_apply_properties_by_class: &binary_import.post_apply_properties_by_class,
                post_apply_properties_by_path: &binary_import.post_apply_properties_by_path,
                database,
            },
        );
    }
    log_timing("native editor post-apply preparation", phase_started);
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

pub(crate) fn native_editor_full_push_services(
    args: &PushEditorChangesArgs,
) -> Result<HashSet<String>> {
    let project_root = resolve_project_root_if_present(&args.project.project_root)?;
    let src_root = absolutize_under(&project_root, &args.project.src_root);
    Ok(explorer_daemon_services(&src_root, "")?
        .into_iter()
        .filter(|service| {
            let service_dir = src_root.join(service);
            service_dir.is_dir() || service_settings_path(&service_dir).is_file()
        })
        .collect())
}
