//! Offline comparison, using the same identity matcher as reconciliation but
//! comparing every decoded value (not reconciliation's ignored-field policy).
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use rbx_dom_weak::{
    WeakDom,
    types::{ContentType, Ref, Variant},
};
use serde_json::{Map, Value, json};

use crate::app::output::log_global;
use crate::cli::ComparePlaceArgs;
use crate::rbx::encode::{
    rbx_logical_property_name, rbx_model_property_descriptor, rbx_property_descriptor,
};
use crate::rbx::{decode::rbx_variant_to_settings_json, model::BytecodeModelImportRefs};
use crate::settings::{
    bytecode::{SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance},
    equivalence::{align_settings_ids_to_reference, stabilize_settings_reference_ids},
};
use crate::system::text::normalized_source_bytes;

#[cfg(test)]
#[path = "place_diff_tests.rs"]
mod tests;

/// The project DOM also carries native-setter properties that have no RBXL
/// representation. Report that boundary instead of treating them as deletions.
pub(super) fn omit_unsaved_project_properties(
    dom: &mut WeakDom,
) -> Result<BTreeMap<String, usize>> {
    let database = rbx_reflection_database::get()?;
    let mut omitted = BTreeMap::new();
    let refs = dom
        .descendants()
        .map(|node| node.referent())
        .collect::<Vec<_>>();
    for id in refs {
        let node = dom.get_by_ref_mut(id).unwrap();
        node.properties.retain(|name, _| {
            let unsaved = rbx_property_descriptor(database, node.class.as_str(), name.as_str())
                .is_some_and(|property| {
                    matches!(
                        property.kind,
                        rbx_reflection::PropertyKind::Canonical {
                            serialization: rbx_reflection::PropertySerialization::DoesNotSerialize
                        }
                    )
                });
            if unsaved {
                *omitted.entry(name.to_string()).or_default() += 1;
            }
            !unsaved
        });
    }
    Ok(omitted)
}

pub(super) fn document(
    dom: &WeakDom,
    prefix: &str,
    services: Option<&BTreeSet<String>>,
    elide_defaults: bool,
) -> Result<SettingsBytecode> {
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let mut refs = Vec::new();
    let mut pending = dom
        .root()
        .children()
        .iter()
        .rev()
        .copied()
        .filter(|id| {
            services.is_none_or(|services| services.contains(&dom.get_by_ref(*id).unwrap().name))
        })
        .collect::<Vec<_>>();
    while let Some(id) = pending.pop() {
        refs.push(id);
        pending.extend(dom.get_by_ref(id).unwrap().children().iter().rev().copied());
    }
    let indices = refs
        .iter()
        .enumerate()
        .map(|(index, id)| (*id, index))
        .collect::<HashMap<_, _>>();
    let mut import_refs = BytecodeModelImportRefs {
        new_index_by_ref: indices.clone(),
        ..Default::default()
    };
    for node in dom.descendants() {
        if !indices.contains_key(&node.referent()) {
            let (path, ordinals) =
                crate::rbx::model::rbx_dom_instance_path_parts(dom, node.referent());
            std::sync::Arc::make_mut(&mut import_refs.path_segments_by_ref)
                .insert(node.referent(), path);
            std::sync::Arc::make_mut(&mut import_refs.path_ordinals_by_ref)
                .insert(node.referent(), ordinals);
        }
    }
    // Indexed collection keeps file traversal order while independent records
    // convert in parallel. Each worker reuses its reflection/default metadata.
    let instances = refs
        .par_iter()
        .enumerate()
        .map_init(
            HashMap::new,
            |property_metadata, (index, id)| -> Result<_> {
                let node = dom.get_by_ref(*id).unwrap();
                let mut record = SettingsBytecodeInstance::new(
                    format!("{prefix}:{index}"),
                    node.name.clone(),
                    node.class.to_string(),
                    indices.get(&node.parent()).copied(),
                );
                for (key, value) in &node.properties {
                    let metadata_key = (node.class, *key);
                    let key = key.as_str();
                    // Serialized identity/history are not editable content. File-local
                    // referents are resolved below, never compared by their raw numbers.
                    if elide_defaults && matches!(key, "UniqueId" | "HistoryId") {
                        continue;
                    }
                    if let Variant::Attributes(attributes) = value {
                        for (name, value) in attributes {
                            // Attribute strings share one byte encoding on disk.
                            let value = match value {
                                Variant::BinaryString(bytes) if elide_defaults => {
                                    match std::str::from_utf8(bytes.as_ref()) {
                                        Ok(text) => json!(text),
                                        Err(_) => {
                                            variant_value(value, None, database, &import_refs)?
                                        }
                                    }
                                }
                                _ => variant_value(value, None, database, &import_refs)?,
                            };
                            record.attributes.insert(name.clone(), value);
                        }
                        continue;
                    }
                    let (canonical, descriptor, raw_default, default) = match property_metadata
                        .entry(metadata_key)
                    {
                        std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            let canonical =
                                rbx_logical_property_name(database, node.class.as_str(), key)
                                    .unwrap_or(key);
                            let descriptor =
                                rbx_model_property_descriptor(database, node.class.as_str(), key)
                                    .or_else(|| {
                                        rbx_property_descriptor(
                                            database,
                                            node.class.as_str(),
                                            canonical,
                                        )
                                    });
                            let raw_default = if elide_defaults {
                                database.classes.get(node.class.as_str()).and_then(|class| {
                                    database
                                        .find_default_property(class, canonical)
                                        .or_else(|| database.find_default_property(class, key))
                                })
                            } else {
                                None
                            };
                            let default = raw_default
                                .map(|value| {
                                    variant_value(value, descriptor, database, &import_refs)
                                })
                                .transpose()?;
                            entry.insert((canonical.to_string(), descriptor, raw_default, default))
                        }
                    };
                    let descriptor = *descriptor;
                    // Most decoded properties are defaults. Avoid allocating their JSON
                    // only when both sides take the identical conversion path; source,
                    // tags and unknown-type normalization retain their full comparison.
                    if descriptor.is_some()
                        && key != "Source"
                        && !matches!(value, Variant::Tags(_))
                        && *raw_default == Some(value)
                    {
                        continue;
                    }
                    if elide_defaults
                        && descriptor.is_some()
                        && matches!(value, Variant::Ref(target) if *target == Ref::none())
                    {
                        continue;
                    }
                    let value = if key == "Source" {
                        match value {
                            Variant::String(source) if elide_defaults => json!(String::from_utf8(
                                normalized_source_bytes(source.as_bytes()).collect()
                            )?),
                            Variant::String(source) => json!(source),
                            _ => variant_value(value, descriptor, database, &import_refs)?,
                        }
                    } else if elide_defaults && let Variant::Tags(tags) = value {
                        json!(tags.iter().collect::<BTreeSet<_>>())
                    } else if elide_defaults
                        && descriptor.is_none()
                        && let Variant::BinaryString(bytes) = value
                        && let Ok(text) = std::str::from_utf8(bytes.as_ref())
                    {
                        // An unknown RBXL string has no text/binary type metadata.
                        json!(text)
                    } else if elide_defaults
                        && descriptor.is_none()
                        && let Variant::EnumItem(item) = value
                    {
                        // Unknown property enums carry only their number in both file formats.
                        variant_value(
                            &Variant::Enum(rbx_dom_weak::types::Enum::from_u32(item.value)),
                            None,
                            database,
                            &import_refs,
                        )?
                    } else {
                        variant_value(value, descriptor, database, &import_refs)?
                    };
                    if default.as_ref() == Some(&value) {
                        continue;
                    }
                    let output_name = if elide_defaults {
                        canonical.as_str()
                    } else {
                        key
                    };
                    match record.properties.entry(output_name.to_string()) {
                        serde_json::map::Entry::Vacant(entry) => {
                            entry.insert(value);
                        }
                        serde_json::map::Entry::Occupied(entry) if entry.get() != &value => {
                            bail!(
                                "{} contains conflicting aliases for property {output_name}",
                                node.name
                            );
                        }
                        _ => {}
                    }
                }
                Ok(record)
            },
        )
        .collect::<Result<Vec<_>>>()?;
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    };
    stabilize_settings_reference_ids(&mut document);
    Ok(document)
}

fn variant_value(
    value: &Variant,
    descriptor: Option<&rbx_reflection::PropertyDescriptor<'_>>,
    database: &rbx_reflection::ReflectionDatabase<'_>,
    refs: &BytecodeModelImportRefs,
) -> Result<Value> {
    let target = match value {
        Variant::Ref(target) => Some(*target),
        Variant::Content(content) => match content.value() {
            ContentType::Object(target) => Some(*target),
            _ => None,
        },
        _ => None,
    };
    if let Some(target) = target {
        return Ok(if target == Ref::none() {
            Value::Null
        } else if let Some(index) = refs.new_index_by_ref.get(&target) {
            json!({"_type":"Ref", "instanceIndex": index + 1})
        } else {
            // A reference outside the compared service scope is not a null ref.
            json!({"_type":"ExternalRef", "path": refs.path_segments_by_ref.get(&target).context("Dangling reference in place file")?, "ordinals": refs.path_ordinals_by_ref.get(&target)})
        });
    }
    match rbx_variant_to_settings_json(value, descriptor, database, refs) {
        Some(value) => Ok(value),
        None => serde_json::to_value(value).context("Cannot represent saved property value"),
    }
}

fn location(document: &SettingsBytecode, index: usize) -> Value {
    let mut path = Vec::new();
    let mut current = Some(index);
    while let Some(index) = current {
        let instance = &document.instances[index];
        path.push(instance.name.clone());
        current = instance.parent_index;
    }
    path.reverse();
    json!({"path":path, "className":document.instances[index].class_name, "index":index + 1})
}

fn changed_fields(
    before: &Map<String, Value>,
    after: &Map<String, Value>,
    values: bool,
) -> Vec<Value> {
    if before == after {
        return Vec::new();
    }
    let mut names = before
        .iter()
        .filter_map(|(name, value)| (after.get(name) != Some(value)).then_some(name))
        .collect::<Vec<_>>();
    names.extend(after.keys().filter(|name| !before.contains_key(*name)));
    names.sort_unstable();
    names.into_iter().map(|name| {
        if values {
            json!({"name":name,"before":before.get(name),"after":after.get(name),"beforePresent":before.contains_key(name),"afterPresent":after.contains_key(name)})
        } else {
            json!({"name":name})
        }
    }).collect()
}

pub(super) fn compare(
    before: &WeakDom,
    after: &WeakDom,
    services: &BTreeSet<String>,
    args: &ComparePlaceArgs,
) -> Result<Value> {
    let started = Instant::now();
    let (before, after) = rayon::join(
        || document(before, "before", Some(services), true),
        || document(after, "after", Some(services), true),
    );
    let before = before?;
    let mut after = after?;
    log_global(
        4,
        format_args!(
            "[renium] place comparison documents: {:.1}ms",
            started.elapsed().as_secs_f64() * 1000.0
        ),
    );
    let started = Instant::now();
    if !align_settings_ids_to_reference(&before, &mut after) {
        bail!(
            "Cannot match duplicate instance identities safely; inspect the ambiguous subtrees with `rbx v FILE --json`"
        );
    }
    log_global(
        4,
        format_args!(
            "[renium] place comparison identity: {:.1}ms",
            started.elapsed().as_secs_f64() * 1000.0
        ),
    );
    let started = Instant::now();
    let before_ids = before
        .instances
        .iter()
        .enumerate()
        .map(|(i, instance)| (instance.settings_id.as_str(), i))
        .collect::<HashMap<_, _>>();
    let mut matched = vec![false; before.instances.len()];
    let mut differences = Vec::new();
    let (mut added, mut removed, mut changed, mut unchanged) = (0, 0, 0, 0);
    let limit = if args.all {
        usize::MAX
    } else {
        args.limit.get()
    };
    // Compare records independently; emit in file order so limits and reports
    // remain deterministic, including duplicate sibling identities.
    let compare_record = |instance: &SettingsBytecodeInstance| {
        before_ids
            .get(instance.settings_id.as_str())
            .map(|&previous| {
                let old = &before.instances[previous];
                (
                    previous,
                    changed_fields(&old.properties, &instance.properties, args.values),
                    changed_fields(&old.attributes, &instance.attributes, args.values),
                )
            })
    };
    let records = if after.instances.len() >= 2_048 {
        after
            .instances
            .par_iter()
            .map(compare_record)
            .collect::<Vec<_>>()
    } else {
        after
            .instances
            .iter()
            .map(compare_record)
            .collect::<Vec<_>>()
    };
    for (index, (instance, record)) in after.instances.iter().zip(records).enumerate() {
        let Some((previous, properties, attributes)) = record else {
            added += 1;
            if differences.len() < limit {
                let mut entry = json!({"kind":"added","after":location(&after, index)});
                if args.values {
                    entry["properties"] = json!(instance.properties);
                    entry["attributes"] = json!(instance.attributes);
                }
                differences.push(entry);
            }
            continue;
        };
        matched[previous] = true;
        if properties.is_empty() && attributes.is_empty() {
            unchanged += 1;
        } else {
            changed += 1;
            if differences.len() < limit {
                differences.push(json!({"kind":"changed","before":location(&before, previous),"after":location(&after, index),"properties":properties,"attributes":attributes}));
            }
        }
    }
    for (index, was_matched) in matched.into_iter().enumerate() {
        if !was_matched {
            removed += 1;
            if differences.len() < limit {
                let mut entry = json!({"kind":"removed","before":location(&before,index)});
                if args.values {
                    entry["properties"] = json!(before.instances[index].properties);
                    entry["attributes"] = json!(before.instances[index].attributes);
                }
                differences.push(entry);
            }
        }
    }
    log_global(
        4,
        format_args!(
            "[renium] place comparison differences: {:.1}ms",
            started.elapsed().as_secs_f64() * 1000.0
        ),
    );
    let result = json!({"ok":true,"scope":"full","direction":"input -> target","services":services,"matches":added+removed+changed == 0,"beforeInstances":before.instances.len(),"afterInstances":after.instances.len(),"added":added,"removed":removed,"changed":changed,"unchanged":unchanged,"differenceCount":added+removed+changed,"truncated":added+removed+changed > differences.len(),"differences":differences});
    drop(before_ids);
    let started = Instant::now();
    before
        .instances
        .into_par_iter()
        .chain(after.instances)
        .for_each(drop);
    log_global(
        4,
        format_args!(
            "[renium] place comparison document release: {:.1}ms",
            started.elapsed().as_secs_f64() * 1000.0
        ),
    );
    Ok(result)
}
