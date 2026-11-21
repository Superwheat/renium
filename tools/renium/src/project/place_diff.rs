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
    rbx_serialized_property_name_for_logical,
};
use crate::rbx::{
    decode::{native_property_filter, rbx_variant_to_settings_json},
    model::BytecodeModelImportRefs,
};
use crate::settings::tree::{DUPLICATE_REPAIR_GROUP_LIMIT, settings_children_by_parent};
use crate::settings::{
    bytecode::{SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance},
    equivalence::{
        align_settings_ids_by_place, align_settings_ids_to_reference,
        reconciliation_property_is_derived, stabilize_settings_reference_ids,
    },
};
use crate::snapshot::refs::{remap_record_reference_ids, stabilize_record_references};

const UNIFIED_LIGHTING_MARKER: &str = "RBX_LightingTechnologyUnifiedMigration";
const ENGINE_INTERNAL_SERVICES: [&str; 2] = ["ConfigureServerService", "FilteredSelection"];

/// The rows a whole document reads of a DOM a turn at a time. A test reads
/// few, so that its places take several turns.
const WHOLE_ROWS_AT_ONCE: usize = if cfg!(test) {
    16
} else {
    super::version_control::VIEW_ROWS_AT_ONCE
};

/// Studio session state saved with a place; two saves of unchanged content
/// disagree on it.
pub(super) fn is_engine_internal_service(name: &str) -> bool {
    ENGINE_INTERNAL_SERVICES.contains(&name)
}

enum Skipped {
    EngineManaged,
    NotCaptured,
}

/// Studio recomputes these from other saved data or from loaded assets, so two
/// saves of one place can disagree on them without any edit in between.
fn engine_managed_property(class_name: &str, name: &str, value: &Value) -> bool {
    reconciliation_property_is_derived(name)
        || matches!(
            name,
            "PhysicalConfigData"
                | "CollisionGroupId"
                | "ClockTime"
                | "TexturePack"
                | "TriangleCount"
                | "InitialSize"
                | "MeshSize"
                | "FluidFidelityInternal"
                | "CFrame1"
                | "Part0Internal"
                | "Part1Internal"
        )
        || class_name == "WeldConstraint" && name == "State"
        // Studio regenerates a rig's tables whenever it loads them; a table it
        // has not filled yet is saved as an empty string.
        || class_name == "AnimationRigData"
            && (value.get("_type").and_then(Value::as_str) == Some("BinaryString")
                || value.as_str() == Some(""))
}

/// A `CSGPHS ... BLOCK` PhysicalConfigData is a placeholder for a collision
/// hull Studio has not computed; saved, it turns the part into a solid box.
/// It is reported, where a computed hull is engine state and is not.
fn is_block_physics_placeholder_json(value: &Value) -> bool {
    crate::settings::held::resolve(value)
        .get("base64")
        .and_then(Value::as_str)
        .and_then(|encoded| base64::decode(encoded).ok())
        .is_some_and(|bytes| {
            bytes.len() <= 32 && bytes.starts_with(b"CSGPHS") && bytes.ends_with(b"BLOCK")
        })
}

/// Properties named in `keep` stay even when the engine manages them, so an
/// explicit `--props PhysicalConfigData` compares the cached physics data.
#[cfg(test)]
pub(super) fn omit_engine_managed_properties(
    document: &mut SettingsBytecode,
    keep: &[String],
) -> BTreeMap<String, usize> {
    let mut omitted = BTreeMap::new();
    for instance in &mut document.instances {
        let class_name = &instance.class_name;
        instance.properties.retain(|name, value| {
            let managed = !keep.iter().any(|kept| kept == name)
                && engine_managed_property(class_name, name, value)
                && !(name == "PhysicalConfigData" && is_block_physics_placeholder_json(value));
            if managed {
                *omitted.entry(name.clone()).or_default() += 1;
            }
            !managed
        });
    }
    omitted
}

struct ClassMembers {
    instances: usize,
    carried: HashMap<String, usize>,
}

fn class_members(document: &SettingsBytecode) -> HashMap<String, ClassMembers> {
    let mut classes = HashMap::<String, ClassMembers>::new();
    add_class_members(&mut classes, &document.instances);
    classes
}

fn add_class_members(
    classes: &mut HashMap<String, ClassMembers>,
    instances: &[SettingsBytecodeInstance],
) {
    for instance in instances {
        let members = classes
            .entry(instance.class_name.clone())
            .or_insert_with(|| ClassMembers {
                instances: 0,
                carried: HashMap::new(),
            });
        members.instances += 1;
        for name in instance.properties.keys() {
            *members.carried.entry(name.clone()).or_default() += 1;
        }
    }
}

/// Studio saves a member on every instance of a class or on none, so a member
/// without a reflection default (elision cannot have removed it) that every
/// instance of a class carries on one side and none carries on the other came
/// or went with a Studio version, not with an edit. Dropped before identities
/// are matched so the instances still pair by content. Services stay: their
/// single record cannot tell a version from an edit.
fn omit_one_sided_class_properties(
    before: &mut SettingsBytecode,
    after: &mut SettingsBytecode,
    database: &rbx_reflection::ReflectionDatabase<'_>,
) -> BTreeMap<String, usize> {
    let members = [class_members(before), class_members(after)];
    omit_one_sided_members(before, after, &members, database)
}

/// `omit_one_sided_class_properties` with the members each side carries,
/// counted of all its instances.
fn omit_one_sided_members(
    before: &mut SettingsBytecode,
    after: &mut SettingsBytecode,
    [before_members, after_members]: &[HashMap<String, ClassMembers>; 2],
    database: &rbx_reflection::ReflectionDatabase<'_>,
) -> BTreeMap<String, usize> {
    let mut omitted = BTreeMap::new();
    for (document, own, other) in [
        (before, before_members, after_members),
        (after, after_members, before_members),
    ] {
        for instance in &mut document.instances {
            let class_name = instance.class_name.as_str();
            let (Some(own), Some(other)) = (own.get(class_name), other.get(class_name)) else {
                continue;
            };
            let Some(class) = database.classes.get(class_name) else {
                continue;
            };
            if class.tags.contains(&rbx_reflection::ClassTag::Service) {
                continue;
            }
            instance.properties.retain(|name, _| {
                let one_sided = own.carried.get(name) == Some(&own.instances)
                    && !other.carried.contains_key(name)
                    && database.find_default_property(class, name).is_none()
                    && rbx_logical_property_name(database, class_name, name).is_none_or(
                        |logical| database.find_default_property(class, logical).is_none(),
                    )
                    && rbx_serialized_property_name_for_logical(database, class_name, name)
                        .is_none_or(|serialized| {
                            database.find_default_property(class, serialized).is_none()
                        });
                if one_sided {
                    *omitted.entry(name.clone()).or_default() += 1;
                }
                !one_sided
            });
        }
    }
    omitted
}

/// A saved place never contains instances with Archivable off, so a project
/// that carries them (Studio-only helpers, plugin state) cannot be compared on
/// them.
pub(super) fn omit_unsaved_project_instances(dom: &mut WeakDom) -> usize {
    let archivable = rbx_dom_weak::Ustr::from("Archivable");
    let unsaved = dom
        .descendants()
        .filter(|node| matches!(node.properties.get(&archivable), Some(Variant::Bool(false))))
        .map(|node| node.referent())
        .collect::<Vec<_>>();
    let mut omitted = 0;
    for id in unsaved {
        if dom.get_by_ref(id).is_some() {
            omitted += dom.descendants_of(id).count();
            dom.destroy(id);
        }
    }
    omitted
}
use crate::system::text::normalized_source_bytes;

#[cfg(test)]
#[path = "place_diff_tests.rs"]
mod tests;

/// Leaves the project DOM with the properties a place file written from it
/// holds. Native-setter properties with no RBXL representation go, and are
/// counted so the report names that boundary instead of calling them
/// deletions. A property Roblox replaced moves to its successor as the
/// writer moves it (a Sound's SoundId to AudioContent, BrickColor to Color);
/// a successor the instance already carries wins.
pub(super) fn project_properties_as_saved(dom: &mut WeakDom) -> Result<BTreeMap<String, usize>> {
    use rbx_reflection::{PropertyKind, PropertySerialization};
    let database = rbx_reflection_database::get()?;
    let mut omitted = BTreeMap::new();
    let refs = dom
        .descendants()
        .map(|node| node.referent())
        .collect::<Vec<_>>();
    for id in refs {
        let node = dom.get_by_ref_mut(id).unwrap();
        let mut migrated = Vec::new();
        node.properties.retain(|name, value| {
            let Some(PropertyKind::Canonical { serialization }) =
                rbx_property_descriptor(database, node.class.as_str(), name.as_str())
                    .map(|property| &property.kind)
            else {
                return true;
            };
            match serialization {
                PropertySerialization::DoesNotSerialize => {
                    *omitted.entry(name.to_string()).or_default() += 1;
                    false
                }
                PropertySerialization::Migrate(migration) => match migration.perform(value) {
                    Ok(value) => {
                        migrated.push((migration.new_property_names(), value));
                        false
                    }
                    // A value the migration rejects stays under its own name.
                    Err(_) => true,
                },
                _ => true,
            }
        });
        for (names, value) in migrated {
            for name in names {
                node.properties
                    .entry(rbx_dom_weak::Ustr::from(name))
                    .or_insert_with(|| value.clone());
            }
        }
    }
    Ok(omitted)
}

/// Fields of a service root that `rbx pi` leaves out of a project: Studio's
/// own saved state the reflection database does not describe (Workspace's
/// CSGAsyncDynamicCollision, StarterPlayer's avatar settings) and the pivot
/// override Studio always writes. A saved place carries them, so comparing a
/// project with one drops them from the place first and reports the count.
pub(super) fn omit_unsaved_service_properties(
    dom: &mut WeakDom,
) -> Result<BTreeMap<String, usize>> {
    let database = rbx_reflection_database::get()?;
    let mut omitted = BTreeMap::new();
    let services = dom.root().children().to_vec();
    for id in services {
        let Some(node) = dom.get_by_ref_mut(id) else {
            continue;
        };
        let class_name = node.class.to_string();
        node.properties.retain(|name, _| {
            let unsaved = crate::rbx::decode::is_unsaved_service_property(
                database,
                &class_name,
                name.as_str(),
            ) || class_name == "Workspace" && name.as_str() == "WorldPivotData";
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
    prefix: &'static str,
    services: Option<&BTreeSet<String>>,
    elide_defaults: bool,
) -> Result<SettingsBytecode> {
    let rows = dom_rows(dom, prefix, services, elide_defaults)?;
    let every = (0..rows.plan.refs.len()).collect::<Vec<_>>();
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: records(dom, &rows.plan, &every, None)?.0,
    };
    stabilize_settings_reference_ids(&mut document);
    Ok(document)
}

/// A DOM and what `document` reads of it before it converts an instance.
pub(super) struct DomRows<'a> {
    dom: &'a WeakDom,
    pub(super) plan: RowPlan,
}

/// The instances of a DOM in the order of its document, where each stands
/// there, and how a reference names one.
pub(super) struct RowPlan {
    prefix: &'static str,
    elide_defaults: bool,
    database: &'static rbx_reflection::ReflectionDatabase<'static>,
    refs: Vec<Ref>,
    indices: HashMap<Ref, usize>,
    current_camera: Option<Ref>,
    import_refs: BytecodeModelImportRefs,
    /// The properties a sparse read left out of each instance of the DOM
    /// (`RbxPlaceFormat::read_sparse`): a row holds each at its reflection
    /// default.
    pub(super) left_out: HashMap<Ref, Vec<rbx_dom_weak::Ustr>>,
    /// Whether a row holds a payload its instances share as a handle
    /// (`held::held`), as a comparison of whole documents does.
    pub(super) holds_payloads: bool,
}

impl DomRows<'_> {
    /// The instances of `document` without their values.
    pub(super) fn structure(&self) -> SettingsBytecode {
        let bare = |(index, id): (usize, &Ref)| {
            let node = self.dom.get_by_ref(*id).unwrap();
            SettingsBytecodeInstance::new(
                format!("{}:{index}", self.plan.prefix),
                node.name.clone(),
                node.class.to_string(),
                self.plan.indices.get(&node.parent()).copied(),
            )
        };
        SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: self.plan.refs.iter().enumerate().map(bare).collect(),
        }
    }

    /// `RowReader::walk_turns` of `structure`: each row of a turn holds the
    /// values `document` gives it while its turn is visited.
    pub(super) fn walk_turns(
        &self,
        document: &mut SettingsBytecode,
        rows: &[usize],
        at_once: usize,
        mut visit: impl FnMut(&SettingsBytecode, &[usize]) -> Result<()>,
    ) -> Result<()> {
        let ids = (document.instances.iter())
            .map(|instance| instance.settings_id.clone())
            .collect::<Vec<_>>();
        for turn in rows.chunks(at_once.max(1)) {
            let (mut read, _) = records(self.dom, &self.plan, turn, None)?;
            read.par_iter_mut().for_each(|record| {
                stabilize_record_references(&mut record.properties, &ids);
                stabilize_record_references(&mut record.attributes, &ids);
            });
            for (row, record) in turn.iter().zip(read) {
                document.instances[*row] = record;
            }
            let visited = visit(document, turn);
            for row in turn {
                document.instances[*row] = document.instances[*row].bare();
            }
            visited?;
        }
        Ok(())
    }
}

pub(super) fn dom_rows<'a>(
    dom: &'a WeakDom,
    prefix: &'static str,
    services: Option<&BTreeSet<String>>,
    elide_defaults: bool,
) -> Result<DomRows<'a>> {
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
    // The viewport camera's placement is editor state, not place content.
    let current_camera = dom
        .root()
        .children()
        .iter()
        .filter_map(|id| dom.get_by_ref(*id))
        .find(|instance| instance.class == "Workspace")
        .and_then(|workspace| {
            match workspace
                .properties
                .get(&rbx_dom_weak::Ustr::from("CurrentCamera"))
            {
                Some(Variant::Ref(camera)) => Some(*camera),
                _ => None,
            }
        });
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
    Ok(DomRows {
        dom,
        plan: RowPlan {
            prefix,
            elide_defaults,
            database,
            refs,
            indices,
            current_camera,
            import_refs,
            left_out: HashMap::new(),
            holds_payloads: elide_defaults,
        },
    })
}

/// The instances at `rows` of a DOM's rows, as `document` holds them before
/// it stabilizes their references. With `keep`, without the values the
/// engine manages but those `keep` names, which are counted.
fn records(
    dom: &WeakDom,
    plan: &RowPlan,
    rows: &[usize],
    keep: Option<&[String]>,
) -> Result<(Vec<SettingsBytecodeInstance>, BTreeMap<String, usize>)> {
    let &RowPlan {
        prefix,
        elide_defaults,
        database,
        ref refs,
        ref indices,
        current_camera,
        ref import_refs,
        ref left_out,
        holds_payloads,
    } = plan;
    // Indexed collection keeps file traversal order while independent records
    // convert in parallel. Each worker reuses its reflection/default metadata.
    let read = rows
        .par_iter()
        .map(|index| (*index, &refs[*index]))
        .map_init(
            HashMap::new,
            |property_metadata, (index, id)| -> Result<_> {
                let mut omitted = Map::new();
                let node = dom.get_by_ref(*id).unwrap();
                let mut record = SettingsBytecodeInstance::new(
                    format!("{prefix}:{index}"),
                    node.name.clone(),
                    node.class.to_string(),
                    indices.get(&node.parent()).copied(),
                );
                let class = database.classes.get(node.class.as_str());
                let restored = (left_out.get(id).into_iter().flatten())
                    .filter(|name| !node.properties.contains_key(*name))
                    .map(|name| {
                        let default = class
                            .and_then(|class| database.find_default_property(class, name.as_str()));
                        Ok((name, default.context("A property left out has no default")?))
                    })
                    .collect::<Result<Vec<_>>>()?;
                for (key, value) in node.properties.iter().chain(restored) {
                    let metadata_key = (node.class, *key);
                    let key = key.as_str();
                    // Serialized identity/history are not editable content. File-local
                    // referents are resolved below, never compared by their raw numbers.
                    if elide_defaults && matches!(key, "UniqueId" | "HistoryId" | "ScriptGuid") {
                        continue;
                    }
                    if elide_defaults
                        && current_camera == Some(*id)
                        && matches!(key, "CFrame" | "Focus")
                    {
                        continue;
                    }
                    if let Variant::Attributes(attributes) = value
                        && key == "Attributes"
                    {
                        collect_attribute_values(
                            attributes,
                            elide_defaults,
                            database,
                            import_refs,
                            &mut record,
                        )?;
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
                            let raw_default =
                                if elide_defaults {
                                    database.classes.get(node.class.as_str()).and_then(|class| {
                                        database
                                            .find_default_property(class, canonical)
                                            .or_else(|| database.find_default_property(class, key))
                                            .or_else(|| {
                                                rbx_serialized_property_name_for_logical(
                                                    database,
                                                    node.class.as_str(),
                                                    canonical,
                                                )
                                                .filter(|serialized| {
                                                    *serialized != canonical && *serialized != key
                                                })
                                                .and_then(|serialized| {
                                                    database
                                                        .find_default_property(class, serialized)
                                                })
                                            })
                                    })
                                } else {
                                    None
                                };
                            let default = raw_default
                                .map(|value| {
                                    variant_value(value, descriptor, database, import_refs)
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
                        && key != crate::rbx::encode::COLLISION_HULL_PROPERTY
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
                            _ => variant_value(value, descriptor, database, import_refs)?,
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
                            import_refs,
                        )?
                    } else {
                        variant_value(value, descriptor, database, import_refs)?
                    };
                    if key != crate::rbx::encode::COLLISION_HULL_PROPERTY
                        && default.as_ref() == Some(&value)
                    {
                        continue;
                    }
                    let output_name = if elide_defaults {
                        canonical.as_str()
                    } else {
                        key
                    };
                    let managed = keep.is_some_and(|keep| {
                        !keep.iter().any(|kept| kept == output_name)
                            && engine_managed_property(&record.class_name, output_name, &value)
                            && !(output_name == "PhysicalConfigData"
                                && is_block_physics_placeholder_json(&value))
                    });
                    // A comparison holds a payload its instances share once.
                    let value = if holds_payloads && !managed {
                        crate::settings::held::held(value)
                    } else {
                        value
                    };
                    let (into, beside) = if managed {
                        (&mut omitted, &record.properties)
                    } else {
                        (&mut record.properties, &omitted)
                    };
                    let conflicting = beside.contains_key(output_name)
                        || match into.entry(output_name.to_string()) {
                            serde_json::map::Entry::Vacant(entry) => {
                                entry.insert(value);
                                false
                            }
                            serde_json::map::Entry::Occupied(entry) => entry.get() != &value,
                        };
                    if conflicting {
                        bail!(
                            "{} contains conflicting aliases for property {output_name}",
                            node.name
                        );
                    }
                }
                Ok((
                    record,
                    omitted
                        .into_iter()
                        .map(|(name, _)| name)
                        .collect::<Vec<_>>(),
                ))
            },
        )
        .collect::<Result<Vec<_>>>()?;
    let mut omitted = BTreeMap::new();
    let records = (read.into_iter())
        .map(|(record, names)| {
            for name in names {
                *omitted.entry(name).or_default() += 1;
            }
            record
        })
        .collect();
    Ok((records, omitted))
}

fn collect_attribute_values(
    attributes: &rbx_dom_weak::types::Attributes,
    elide_defaults: bool,
    database: &rbx_reflection::ReflectionDatabase<'_>,
    refs: &BytecodeModelImportRefs,
    record: &mut SettingsBytecodeInstance,
) -> Result<()> {
    for (name, value) in attributes {
        // Attribute strings share one byte encoding on disk.
        let value = match value {
            Variant::BinaryString(bytes) if elide_defaults => {
                match std::str::from_utf8(bytes.as_ref()) {
                    Ok(text) => json!(text),
                    Err(_) => variant_value(value, None, database, refs)?,
                }
            }
            _ => variant_value(value, None, database, refs)?,
        };
        record.attributes.insert(name.clone(), value);
    }
    Ok(())
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

/// Text mode prints one line per difference: kind, class and dotted path,
/// then the changed property names and `@attribute` names.
fn difference_lines(result: &mut Value) {
    // The echoed scope and the instance totals repeat what the caller gave or
    // what the counts already say.
    if let Some(map) = result.as_object_mut() {
        for key in ["services", "beforeInstances", "afterInstances", "direction"] {
            map.remove(key);
        }
    }
    let Some(differences) = result.get_mut("differences").and_then(Value::as_array_mut) else {
        return;
    };
    for difference in differences.iter_mut() {
        let kind = difference["kind"].as_str().unwrap_or_default();
        let after = location_text(&difference["after"]);
        let before = location_text(&difference["before"]);
        let (class, path) = after
            .as_ref()
            .or(before.as_ref())
            .cloned()
            .unwrap_or_default();
        let mut line = format!("{kind} {class} {path}");
        if let (Some((before_class, before_path)), Some(_)) = (&before, &after) {
            if *before_path != path {
                line.push_str(&format!(" (from {before_path})"));
            }
            if *before_class != class {
                line.push_str(&format!(" (was {before_class})"));
            }
        }
        let names = difference["properties"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry["name"].as_str().map(str::to_string))
            .chain(
                difference["attributes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| entry["name"].as_str().map(|name| format!("@{name}"))),
            )
            .collect::<Vec<_>>();
        if !names.is_empty() {
            line.push_str(": ");
            line.push_str(&names.join(", "));
        }
        *difference = Value::String(line);
    }
}

fn location_text(value: &Value) -> Option<(String, String)> {
    let class = value.get("className")?.as_str()?.to_string();
    let segments = value
        .get("path")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    let path = crate::bytecode::explorer::compact_path_string(&segments, None)
        .unwrap_or_else(|| segments.join("."));
    Some((class, path))
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

/// Same-named siblings whose contents swapped places (a Boards folder of
/// `Part`s, two car models with one name) pair by position once their shapes
/// agree, so each twin reads as changed. When every twin has an unchanged
/// counterpart elsewhere in its group, pair them that way and count the
/// reorder instead. Float noise is tolerated as in the field comparison.
fn pair_duplicate_siblings_by_content(
    before: &SettingsBytecode,
    after: &mut SettingsBytecode,
) -> usize {
    let before_children = settings_children_by_parent(before);
    let after_children = settings_children_by_parent(after);
    let before_by_id = before
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut remap: Vec<Option<usize>> = vec![None; after.instances.len()];
    let mut reordered = 0;
    for (parent, children) in after_children.iter().enumerate() {
        // A re-paired ancestor already carries its descendants with it.
        if remap[parent].is_some() {
            continue;
        }
        let mut groups: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
        for &child in children {
            let instance = &after.instances[child];
            groups
                .entry((instance.name.as_str(), instance.class_name.as_str()))
                .or_default()
                .push(child);
        }
        for members in groups
            .into_values()
            .filter(|members| members.len() > 1 && members.len() <= DUPLICATE_REPAIR_GROUP_LIMIT)
        {
            let paired = members
                .iter()
                .map(|&member| {
                    before_by_id
                        .get(after.instances[member].settings_id.as_str())
                        .copied()
                })
                .collect::<Option<Vec<_>>>();
            let Some(paired) = paired else {
                continue;
            };
            let matches = |member: usize, candidate: usize| {
                subtree_matches(
                    (before, &before_children, candidate),
                    (after, &after_children, member),
                )
            };
            if members
                .iter()
                .zip(&paired)
                .all(|(&member, &candidate)| matches(member, candidate))
            {
                continue;
            }
            let mut available = paired.clone();
            let mut assignment = Vec::with_capacity(members.len());
            for (&member, &current) in members.iter().zip(&paired) {
                let position = available
                    .iter()
                    .position(|&candidate| candidate == current && matches(member, candidate))
                    .or_else(|| {
                        available
                            .iter()
                            .position(|&candidate| matches(member, candidate))
                    });
                let Some(position) = position else {
                    break;
                };
                assignment.push(available.swap_remove(position));
            }
            if assignment.len() != members.len() {
                continue;
            }
            for ((&member, &current), &target) in members.iter().zip(&paired).zip(&assignment) {
                if current == target {
                    continue;
                }
                reordered += 1;
                let mut stack = vec![(member, target)];
                while let Some((after_index, before_index)) = stack.pop() {
                    remap[after_index] = Some(before_index);
                    stack.extend(
                        after_children[after_index]
                            .iter()
                            .copied()
                            .zip(before_children[before_index].iter().copied()),
                    );
                }
            }
        }
    }
    if reordered == 0 {
        return 0;
    }
    let id_map = remap
        .iter()
        .enumerate()
        .filter_map(|(after_index, target)| {
            target.map(|before_index| {
                (
                    after.instances[after_index].settings_id.clone(),
                    before.instances[before_index].settings_id.clone(),
                )
            })
        })
        .collect::<HashMap<_, _>>();
    for (after_index, target) in remap.iter().enumerate() {
        if let Some(before_index) = target {
            after.instances[after_index]
                .settings_id
                .clone_from(&before.instances[*before_index].settings_id);
        }
    }
    // The document holds the compared services together under ids of its
    // own, so an id names its instance whatever service a path leads into.
    for instance in &mut after.instances {
        remap_record_reference_ids(&mut instance.properties, &id_map, None);
        remap_record_reference_ids(&mut instance.attributes, &id_map, None);
    }
    reordered
}

/// Two subtrees with the same shape and no changed fields at any node.
/// References are compared by presence only: an internal reference keeps
/// its old identity until the subtree remap below, and an external one is
/// reported by the ordinary comparison afterwards.
fn subtree_matches(
    (before, before_children, before_root): (&SettingsBytecode, &[Vec<usize>], usize),
    (after, after_children, after_root): (&SettingsBytecode, &[Vec<usize>], usize),
) -> bool {
    let mut stack = vec![(before_root, after_root)];
    while let Some((before_index, after_index)) = stack.pop() {
        let old = &before.instances[before_index];
        let new = &after.instances[after_index];
        if old.name != new.name
            || old.class_name != new.class_name
            || before_children[before_index].len() != after_children[after_index].len()
            || !changed_fields_of(
                Some(&old.class_name),
                &without_references(&old.properties),
                &without_references(&new.properties),
                false,
            )
            .is_empty()
            || !changed_fields(
                &without_references(&old.attributes),
                &without_references(&new.attributes),
                false,
            )
            .is_empty()
        {
            return false;
        }
        stack.extend(
            before_children[before_index]
                .iter()
                .copied()
                .zip(after_children[after_index].iter().copied()),
        );
    }
    true
}

fn without_references(fields: &Map<String, Value>) -> Map<String, Value> {
    fields
        .iter()
        .map(|(name, value)| {
            let is_reference = value.get("_type").and_then(Value::as_str) == Some("Ref");
            (
                name.clone(),
                if is_reference {
                    json!({"_type":"Ref"})
                } else {
                    value.clone()
                },
            )
        })
        .collect()
}

fn changed_fields(
    before: &Map<String, Value>,
    after: &Map<String, Value>,
    values: bool,
) -> Vec<Value> {
    changed_fields_of(None, before, after, values)
}

/// The fields that differ; with the instance's class, a property one side
/// leaves out at its reflection default is not one of them (a saved place
/// spells every service field, a project only the ones it sets), nor is an
/// asset named in two spellings.
fn changed_fields_of(
    class_name: Option<&str>,
    before: &Map<String, Value>,
    after: &Map<String, Value>,
    values: bool,
) -> Vec<Value> {
    if before == after {
        return Vec::new();
    }
    let noise =
        |name: &str, before: Option<&Value>, after: Option<&Value>| {
            field_noise(before, after)
                || class_name.is_some_and(|class_name| {
                    crate::settings::equivalence::is_asset_reference_property(class_name, name)
                        && asset_spelling_noise(before, after)
                })
                || class_name.is_some_and(|class_name| match (before, after) {
                    (Some(value), None) | (None, Some(value)) => name != "PhysicalConfigData"
                        && crate::settings::equivalence::reconciliation_property_value_is_default(
                            class_name, name, value,
                        ),
                    _ => false,
                })
        };
    let mut names = before
        .iter()
        .filter_map(|(name, value)| {
            let other = after.get(name);
            (other != Some(value) && !noise(name, Some(value), other)).then_some(name)
        })
        .collect::<Vec<_>>();
    names.extend(
        after
            .iter()
            .filter(|(name, value)| !before.contains_key(*name) && !noise(name, None, Some(value)))
            .map(|(name, _)| name),
    );
    names.sort_unstable();
    names.into_iter().map(|name| {
        if values {
            json!({"name":name,"before":before.get(name),"after":after.get(name),"beforePresent":before.contains_key(name),"afterPresent":after.contains_key(name)})
        } else {
            json!({"name":name})
        }
    }).collect()
}

/// The same asset named by a roblox.com URL on one side and an rbxassetid
/// URI on the other; Studio reads either back as the latter.
fn asset_spelling_noise(before: Option<&Value>, after: Option<&Value>) -> bool {
    let (Some(Value::String(before)), Some(Value::String(after))) = (before, after) else {
        return false;
    };
    let before = crate::settings::equivalence::canonical_asset_text(before);
    let after = crate::settings::equivalence::canonical_asset_text(after);
    before == after && before.starts_with("rbxassetid://")
}

/// Differences no edit produced: an empty blob against none, which Studio
/// versions save either way, and a CFrame moved by float noise, which a
/// rigid transform through float32 leaves behind.
fn field_noise(before: Option<&Value>, after: Option<&Value>) -> bool {
    match (before, after) {
        (None, Some(value)) | (Some(value), None) => {
            value.get("_type").and_then(Value::as_str) == Some("BinaryString")
                && value.get("base64").and_then(Value::as_str) == Some("")
        }
        (Some(before), Some(after)) => {
            let components = |value: &Value| {
                (value.get("_type").and_then(Value::as_str) == Some("CFrame"))
                    .then(|| value.get("components")?.as_array())
                    .flatten()
                    .filter(|components| components.len() == 12)
                    .map(|components| components.iter().map(Value::as_f64).collect::<Vec<_>>())
            };
            match (components(before), components(after)) {
                (Some(before), Some(after)) => {
                    before
                        .iter()
                        .zip(&after)
                        .enumerate()
                        .all(|(index, (a, b))| {
                            let tolerance = if index < 3 { 1e-3 } else { 2e-5 };
                            match (a, b) {
                                (Some(a), Some(b)) => (a - b).abs() <= tolerance,
                                _ => false,
                            }
                        })
                }
                _ => false,
            }
        }
        (None, None) => true,
    }
}

/// What a comparison set aside before it compared values: the values the
/// engine manages and the members one side alone carries, by name, and the
/// twins it paired by their content.
type SetAside = (BTreeMap<String, usize>, BTreeMap<String, usize>, usize);

/// A DOM a comparison reads: one given whole gives up the values of the
/// instances it has read.
pub(super) trait ComparedDom: std::borrow::Borrow<WeakDom> {
    fn release(&mut self, _ids: &[Ref]) {}
}

impl ComparedDom for &WeakDom {}

impl ComparedDom for WeakDom {
    fn release(&mut self, ids: &[Ref]) {
        for id in ids {
            if let Some(instance) = self.get_by_ref_mut(*id) {
                instance.properties = Default::default();
            }
        }
    }
}

/// The comparison of two DOMs, by their rows where it can be
/// (`compare_by_rows`) and else of their whole documents.
pub(super) fn compare(
    before: impl ComparedDom,
    after: impl ComparedDom,
    services: &BTreeSet<String>,
    args: &ComparePlaceArgs,
) -> Result<Value> {
    // A comparison by rows that fails leaves the error, as every other
    // answer, to the whole documents.
    if let Ok(Some(result)) = compare_by_rows(before.borrow(), after.borrow(), services, args) {
        return Ok(result);
    }
    compare_whole(before, after, services, args)
}

/// The records at `at` of a DOM's rows as a comparison holds them before
/// their references are stabilized, without the values the engine manages
/// but those `keep` names, which are counted.
fn compared_records(
    rows: &DomRows<'_>,
    at: &[usize],
    keep: &[String],
) -> Result<(Vec<SettingsBytecodeInstance>, BTreeMap<String, usize>)> {
    records(rows.dom, &rows.plan, at, Some(keep))
}

/// The whole document of a DOM as a comparison holds it, without the values
/// the engine manages but those `keep` names, which are counted. The DOM
/// gives up the values of each turn of rows once they are read.
fn compared_document(
    mut dom: impl ComparedDom,
    prefix: &'static str,
    services: &BTreeSet<String>,
    keep: &[String],
) -> Result<(SettingsBytecode, BTreeMap<String, usize>)> {
    let plan = dom_rows(dom.borrow(), prefix, Some(services), true)?.plan;
    let every = (0..plan.refs.len()).collect::<Vec<_>>();
    let mut instances = Vec::with_capacity(every.len());
    let mut omitted = BTreeMap::new();
    for rows in every.chunks(WHOLE_ROWS_AT_ONCE) {
        let (read, counted) = records(dom.borrow(), &plan, rows, Some(keep))?;
        instances.extend(read);
        for (name, count) in counted {
            *omitted.entry(name).or_default() += count;
        }
        dom.release(&plan.refs[rows[0]..][..rows.len()]);
    }
    drop(dom);
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    };
    stabilize_settings_reference_ids(&mut document);
    Ok((document, omitted))
}

/// Of what a record holds a digest that two records share only where they
/// hold equal values. A handle stands for one payload throughout a process.
fn record_digest(record: &SettingsBytecodeInstance) -> Result<[u8; 32]> {
    use sha2::Digest;
    let mut digest = sha2::Sha256::new();
    serde_json::to_writer(&mut digest, &(&record.properties, &record.attributes))?;
    Ok(digest.finalize().into())
}

/// The rows `pair_duplicate_siblings_by_content` reads where a row `whole`
/// marks stands under one of a group of twins: every row under each of them.
/// The rows of a document stand in the order of its tree.
fn rows_under_twins(document: &SettingsBytecode, whole: &[bool]) -> Vec<usize> {
    let count = document.instances.len();
    let mut ends = (1..=count).collect::<Vec<_>>();
    for row in (0..count).rev() {
        if let Some(parent) = document.instances[row].parent_index {
            ends[parent] = ends[parent].max(ends[row]);
        }
    }
    let mut marked = vec![0];
    for flag in whole {
        marked.push(marked[marked.len() - 1] + usize::from(*flag));
    }
    let mut read = vec![false; count];
    for children in settings_children_by_parent(document) {
        let mut groups: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
        for child in children {
            let instance = &document.instances[child];
            let key = (instance.name.as_str(), instance.class_name.as_str());
            groups.entry(key).or_default().push(child);
        }
        for twins in groups.into_values() {
            if (2..=DUPLICATE_REPAIR_GROUP_LIMIT).contains(&twins.len())
                && twins.iter().any(|twin| marked[ends[*twin]] > marked[*twin])
            {
                for twin in twins {
                    read[twin..ends[twin]].fill(true);
                }
            }
        }
    }
    (0..count).filter(|row| read[*row]).collect()
}

/// `compare` of two DOMs whose documents hold the same instances at the same
/// places, by their rows. A pair at one place whose records hold the same
/// values holds none of them in either document, and two rows without values
/// are equal wherever the comparison looks, so the documents hold whole only
/// the services, the pairs that differ and the twins
/// `pair_duplicate_siblings_by_content` compares beside those. `None` where
/// the whole documents have to say: the instances stand at other places, the
/// places do not tell them (`align_settings_ids_by_place`), or twins swapped
/// their contents.
pub(super) fn compare_by_rows(
    before: &WeakDom,
    after: &WeakDom,
    services: &BTreeSet<String>,
    args: &ComparePlaceArgs,
) -> Result<Option<Value>> {
    let started = Instant::now();
    let mut rows = [
        dom_rows(before, "before", Some(services), true)?,
        dom_rows(after, "after", Some(services), true)?,
    ];
    // Every row is read and few are kept: a payload held once would stay
    // held for the process.
    for side in &mut rows {
        side.plan.holds_payloads = false;
    }
    let mut documents = rows.each_ref().map(DomRows::structure);
    let [reference, observed] = &documents;
    if reference.instances.len() != observed.instances.len()
        || (reference.instances.iter().zip(&observed.instances)).any(|(left, right)| {
            (&left.name, &left.class_name, left.parent_index)
                != (&right.name, &right.class_name, right.parent_index)
        })
    {
        return Ok(None);
    }
    let ids = documents.each_ref().map(|document| {
        (document.instances.iter())
            .map(|instance| instance.settings_id.clone())
            .collect::<Vec<_>>()
    });
    let (engine_managed, members, whole) = read_pairs(&rows, &mut documents, &ids, &args.props)?;
    let twins = (rows_under_twins(&documents[1], &whole).into_iter())
        .filter(|row| !whole[*row])
        .collect::<Vec<_>>();
    let [left, right] = (rows.each_ref()).map(|side| compared_records(side, &twins, &args.props));
    for ((row, left), right) in twins.iter().zip(left?.0).zip(right?.0) {
        put_records(&mut documents, &ids, *row, [left, right]);
    }
    let [mut before, mut after] = documents;
    let database = rbx_reflection_database::get()?;
    let one_sided = omit_one_sided_members(&mut before, &mut after, &members, database);
    log_global(
        4,
        format_args!(
            "[renium] place comparison rows: {:.1}ms, {} of {} whole",
            started.elapsed().as_secs_f64() * 1000.0,
            whole.iter().filter(|whole| **whole).count() + twins.len(),
            whole.len()
        ),
    );
    if !align_settings_ids_by_place(&before, &mut after)
        || pair_duplicate_siblings_by_content(&before, &mut after) > 0
    {
        return Ok(None);
    }
    let set_aside = (engine_managed, one_sided, 0);
    report(before, after, set_aside, services, args).map(Some)
}

/// What `read_pairs` counts of the rows of two documents: the values the
/// engine manages, the members of each class each side carries, and the
/// rows it put whole.
type PairsRead = (
    BTreeMap<String, usize>,
    [HashMap<String, ClassMembers>; 2],
    Vec<bool>,
);

/// Reads the rows of two documents whose instances stand at the same places
/// a turn at a time. A row of a service, or whose records differ, is put
/// whole in both (`put_records`); any other is left without values.
fn read_pairs(
    rows: &[DomRows<'_>; 2],
    documents: &mut [SettingsBytecode; 2],
    ids: &[Vec<String>; 2],
    keep: &[String],
) -> Result<PairsRead> {
    let mut engine_managed = BTreeMap::new();
    let mut members = [HashMap::new(), HashMap::new()];
    let mut whole = vec![false; ids[0].len()];
    let every = (0..whole.len()).collect::<Vec<_>>();
    for turn in every.chunks(super::version_control::VIEW_ROWS_AT_ONCE) {
        let (left, right) = rayon::join(
            || compared_records(&rows[0], turn, keep),
            || compared_records(&rows[1], turn, keep),
        );
        let read = [left?, right?].map(|(records, omitted)| {
            for (name, count) in omitted {
                *engine_managed.entry(name).or_default() += count;
            }
            records
        });
        add_class_members(&mut members[0], &read[0]);
        add_class_members(&mut members[1], &read[1]);
        let same = (read[0].par_iter().zip(&read[1]))
            .map(|(left, right)| Ok(record_digest(left)? == record_digest(right)?))
            .collect::<Result<Vec<_>>>()?;
        let [left, right] = read;
        for (((row, left), right), same) in turn.iter().zip(left).zip(right).zip(same) {
            if !same || documents[0].instances[*row].parent_index.is_none() {
                whole[*row] = true;
                put_records(documents, ids, *row, [left, right]);
            }
        }
    }
    Ok((engine_managed, members, whole))
}

/// Puts the records of a row in both documents, their references stabilized
/// under the `ids` of each, as `document` holds them.
fn put_records(
    documents: &mut [SettingsBytecode; 2],
    ids: &[Vec<String>; 2],
    row: usize,
    records: [SettingsBytecodeInstance; 2],
) {
    for ((document, ids), mut record) in documents.iter_mut().zip(ids).zip(records) {
        stabilize_record_references(&mut record.properties, ids);
        stabilize_record_references(&mut record.attributes, ids);
        document.instances[row] = record;
    }
}

/// `compare` of the whole documents of two DOMs. A DOM given whole is
/// released as its document is read, the first before the second is
/// converted, so the two documents never stand beside both DOMs.
pub(super) fn compare_whole(
    before: impl ComparedDom,
    after: impl ComparedDom,
    services: &BTreeSet<String>,
    args: &ComparePlaceArgs,
) -> Result<Value> {
    let started = Instant::now();
    let (before, mut engine_managed) = compared_document(before, "before", services, &args.props)?;
    let (after, omitted) = compared_document(after, "after", services, &args.props)?;
    for (name, count) in omitted {
        *engine_managed.entry(name).or_default() += count;
    }
    compare_documents([before, after], engine_managed, started, services, args)
}

/// `compare_whole` of the documents it read since `started`, without the
/// values the engine manages that `engine_managed` counts.
pub(super) fn compare_documents(
    [mut before, mut after]: [SettingsBytecode; 2],
    engine_managed: BTreeMap<String, usize>,
    started: Instant,
    services: &BTreeSet<String>,
    args: &ComparePlaceArgs,
) -> Result<Value> {
    let database = rbx_reflection_database::get()?;
    let one_sided = omit_one_sided_class_properties(&mut before, &mut after, database);
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
    let reordered_siblings = pair_duplicate_siblings_by_content(&before, &mut after);
    log_global(
        4,
        format_args!(
            "[renium] place comparison identity: {:.1}ms",
            started.elapsed().as_secs_f64() * 1000.0
        ),
    );
    let set_aside = (engine_managed, one_sided, reordered_siblings);
    report(before, after, set_aside, services, args)
}

/// The answer of a comparison of the documents of the `services` of two
/// places, whose identities are aligned.
fn report(
    before: SettingsBytecode,
    after: SettingsBytecode,
    (mut engine_managed, one_sided, reordered_siblings): SetAside,
    services: &BTreeSet<String>,
    args: &ComparePlaceArgs,
) -> Result<Value> {
    let database = rbx_reflection_database::get()?;
    let mut not_captured = BTreeMap::new();
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
    // Against a project, a saved field the project never captures says nothing
    // when the project lacks it.
    let project_target = args.against.is_none();
    let captured_by_class = if project_target {
        before
            .instances
            .iter()
            .map(|instance| instance.class_name.as_str())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|class_name| {
                (
                    class_name.to_string(),
                    native_property_filter(database, class_name).allowed,
                )
            })
            .collect::<HashMap<_, _>>()
    } else {
        HashMap::new()
    };
    // Compare records independently; emit in file order so limits and reports
    // remain deterministic, including duplicate sibling identities.
    let compare_record = |instance: &SettingsBytecodeInstance| {
        before_ids
            .get(instance.settings_id.as_str())
            .map(|&previous| {
                let old = &before.instances[previous];
                let mut skipped = Vec::new();
                let mut properties = changed_fields_of(
                    Some(&instance.class_name),
                    &old.properties,
                    &instance.properties,
                    args.values,
                );
                let mut attributes =
                    changed_fields(&old.attributes, &instance.attributes, args.values);
                if !args.props.is_empty() {
                    properties.retain(|entry| {
                        entry["name"]
                            .as_str()
                            .is_some_and(|name| args.props.iter().any(|wanted| wanted == name))
                    });
                    attributes.clear();
                }
                // Once a place is on Unified lighting, Studio keeps the saved
                // Technology only for rollback and shows Unified regardless.
                let unified = old.attributes.contains_key(UNIFIED_LIGHTING_MARKER)
                    || instance.attributes.contains_key(UNIFIED_LIGHTING_MARKER);
                properties.retain(|entry| {
                    let name = entry["name"].as_str().unwrap_or_default();
                    if unified && instance.class_name == "Lighting" && name == "Technology" {
                        skipped.push((name.to_string(), Skipped::EngineManaged));
                        return false;
                    }
                    if project_target
                        && name != "PhysicalConfigData"
                        && !instance.properties.contains_key(name)
                        && captured_by_class
                            .get(instance.class_name.as_str())
                            .is_some_and(|captured| !captured.contains(name))
                        && rbx_property_descriptor(database, &instance.class_name, name).is_some()
                    {
                        skipped.push((name.to_string(), Skipped::NotCaptured));
                        return false;
                    }
                    true
                });
                attributes.retain(|entry| {
                    let name = entry["name"].as_str().unwrap_or_default();
                    if name.starts_with("RBX_") {
                        skipped.push((name.to_string(), Skipped::EngineManaged));
                        return false;
                    }
                    true
                });
                (previous, properties, attributes, skipped)
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
    let class_filter = args.class.as_deref();
    for (index, (instance, record)) in after.instances.iter().zip(records).enumerate() {
        if class_filter.is_some_and(|class| instance.class_name != class) {
            if let Some((previous, ..)) = &record {
                matched[*previous] = true;
            }
            continue;
        }
        let Some((previous, properties, attributes, skipped)) = record else {
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
        for (name, kind) in skipped {
            let bucket = match kind {
                Skipped::EngineManaged => &mut engine_managed,
                Skipped::NotCaptured => &mut not_captured,
            };
            *bucket.entry(name).or_default() += 1;
        }
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
            if class_filter.is_some_and(|class| before.instances[index].class_name != class) {
                continue;
            }
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
    let mut result = json!({"ok":true,"scope":"full","direction":"input -> target","services":services,"matches":added+removed+changed == 0,"beforeInstances":before.instances.len(),"afterInstances":after.instances.len(),"added":added,"removed":removed,"changed":changed,"unchanged":unchanged,"differenceCount":added+removed+changed,"truncated":added+removed+changed > differences.len(),"differences":differences});
    crate::app::output::drop_false(&mut result, &["truncated"]);
    if !args.values && crate::app::output::compact_text_output() {
        difference_lines(&mut result);
    }
    if !engine_managed.is_empty() {
        result["engineManagedProperties"] = json!(engine_managed);
    }
    if reordered_siblings > 0 {
        result["reorderedSiblings"] = json!(reordered_siblings);
    }
    if !one_sided.is_empty() {
        result["oneSidedProperties"] = json!(one_sided);
    }
    if !not_captured.is_empty() {
        result["notCapturedProjectProperties"] = json!(not_captured);
    }
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
