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
use crate::daemon::memory::release_between_phases;
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
        align_settings_ids_by_place, align_settings_ids_to_reference, identity_value_is_stable,
        reconciliation_property_is_derived, reconciliation_value_contains_reference,
        stabilize_settings_reference_ids, take_settings_ids,
    },
};
use crate::snapshot::refs::{remap_record_reference_ids, stabilize_record_references};

const UNIFIED_LIGHTING_MARKER: &str = "RBX_LightingTechnologyUnifiedMigration";
const ENGINE_INTERNAL_SERVICES: [&str; 2] = ["ConfigureServerService", "FilteredSelection"];

/// The rows a comparison reads of a DOM a turn at a time. A test reads few,
/// so that its places take several turns.
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
        // A name is copied the first time its class carries it, not for
        // every instance.
        if !classes.contains_key(&instance.class_name) {
            let members = ClassMembers {
                instances: 0,
                carried: HashMap::new(),
            };
            classes.insert(instance.class_name.clone(), members);
        }
        let members = classes.get_mut(&instance.class_name).unwrap();
        members.instances += 1;
        for name in instance.properties.keys() {
            match members.carried.get_mut(name) {
                Some(count) => *count += 1,
                None => drop(members.carried.insert(name.clone(), 1)),
            }
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
                    && has_no_default(database, class, class_name, name);
                if one_sided {
                    *omitted.entry(name.clone()).or_default() += 1;
                }
                !one_sided
            });
        }
    }
    omitted
}

/// Whether the reflection gives a class no default for a property, under
/// any of its names: elision cannot have removed it from an instance.
fn has_no_default(
    database: &rbx_reflection::ReflectionDatabase<'_>,
    class: &rbx_reflection::ClassDescriptor<'_>,
    class_name: &str,
    name: &str,
) -> bool {
    database.find_default_property(class, name).is_none()
        && rbx_logical_property_name(database, class_name, name)
            .is_none_or(|logical| database.find_default_property(class, logical).is_none())
        && rbx_serialized_property_name_for_logical(database, class_name, name)
            .is_none_or(|serialized| database.find_default_property(class, serialized).is_none())
}

/// The members `omit_one_sided_members` takes from documents whose classes
/// carry `members`, counted by name: each from every instance of its class.
fn one_sided_counts(
    [before, after]: &[HashMap<String, ClassMembers>; 2],
    database: &rbx_reflection::ReflectionDatabase<'_>,
) -> BTreeMap<String, usize> {
    let mut omitted = BTreeMap::new();
    for (own, other) in [(before, after), (after, before)] {
        for (class_name, own) in own {
            let (Some(other), Some(class)) = (
                other.get(class_name),
                database.classes.get(class_name.as_str()),
            ) else {
                continue;
            };
            if class.tags.contains(&rbx_reflection::ClassTag::Service) {
                continue;
            }
            for (name, count) in &own.carried {
                if *count == own.instances
                    && !other.carried.contains_key(name)
                    && has_no_default(database, class, class_name, name)
                {
                    *omitted.entry(name.clone()).or_default() += count;
                }
            }
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
    let mut omitted = HashMap::<rbx_dom_weak::Ustr, usize>::new();
    // How a class saves a name is looked up once: a place holds each many
    // times over.
    let mut saved = HashMap::new();
    let refs = dom
        .descendants()
        .map(|node| node.referent())
        .collect::<Vec<_>>();
    for id in refs {
        let node = dom.get_by_ref_mut(id).unwrap();
        let class = node.class;
        let mut migrated = Vec::new();
        node.properties.retain(|name, value| {
            let serialization = *saved.entry((class, *name)).or_insert_with(|| {
                match rbx_property_descriptor(database, class.as_str(), name.as_str())
                    .map(|property| &property.kind)
                {
                    Some(PropertyKind::Canonical { serialization }) => Some(serialization),
                    _ => None,
                }
            });
            match serialization {
                Some(PropertySerialization::DoesNotSerialize) => {
                    *omitted.entry(*name).or_default() += 1;
                    false
                }
                Some(PropertySerialization::Migrate(migration)) => {
                    match migration.perform(value) {
                        Ok(value) => {
                            migrated.push((migration.new_property_names(), value));
                            false
                        }
                        // A value the migration rejects stays under its own name.
                        Err(_) => true,
                    }
                }
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
    Ok((omitted.into_iter())
        .map(|(name, count)| (name.to_string(), count))
        .collect())
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
            instances: (self.plan.refs.par_iter().enumerate()).map(bare).collect(),
        }
    }

    /// Whether the instances stand as those of `document` do: `structure`
    /// would hold the same names, classes and parents.
    fn stand_as(&self, document: &SettingsBytecode) -> bool {
        self.plan.refs.len() == document.instances.len()
            && (self.plan.refs.iter().zip(&document.instances)).all(|(id, instance)| {
                let node = self.dom.get_by_ref(*id).unwrap();
                node.name == instance.name
                    && node.class.as_str() == instance.class_name
                    && self.plan.indices.get(&node.parent()).copied() == instance.parent_index
            })
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
    let name = |id: &Ref| &dom.get_by_ref(*id).unwrap().name;
    let mut pending = (dom.root().children().iter().rev().copied())
        .filter(|id| services.is_none_or(|services| services.contains(name(id))))
        .collect::<Vec<_>>();
    if services.is_some() {
        // The services of two places compared stand by their names, then
        // their classes: a place file gives them in the order Studio made
        // them, a project in its own, and services that stand otherwise
        // would be instances at other places. Namesakes of one class keep
        // their order.
        let named = |id: &Ref| (name(id), dom.get_by_ref(*id).unwrap().class.as_str());
        pending.reverse();
        pending.sort_by(|left, right| named(left).cmp(&named(right)));
        pending.reverse();
    }
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
    let mut paths = crate::rbx::model::RbxDomPaths::new(dom);
    for node in dom.descendants() {
        if !indices.contains_key(&node.referent()) {
            let (path, ordinals) = paths.parts(node.referent());
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
    // Nearly every group of twins holds what its counterparts hold. Which do
    // not is found on every thread, and only those are re-paired in turn.
    let unsettled = |children: &Vec<usize>| {
        let mut groups: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
        for &child in children {
            let instance = &after.instances[child];
            let key = (instance.name.as_str(), instance.class_name.as_str());
            groups.entry(key).or_default().push(child);
        }
        groups.into_values().any(|members| {
            (2..=DUPLICATE_REPAIR_GROUP_LIMIT).contains(&members.len())
                && members.iter().any(|&member| {
                    let id = after.instances[member].settings_id.as_str();
                    before_by_id.get(id).is_some_and(|&candidate| {
                        !subtree_matches(
                            (before, &before_children, candidate),
                            (&*after, &after_children, member),
                        )
                    })
                })
        })
    };
    let unsettled = if after_children.len() >= 2_048 {
        after_children.par_iter().map(unsettled).collect::<Vec<_>>()
    } else {
        after_children.iter().map(unsettled).collect::<Vec<_>>()
    };
    for (parent, children) in after_children.iter().enumerate() {
        // A re-paired ancestor already carries its descendants with it.
        if !unsettled[parent] || remap[parent].is_some() {
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

/// The comparison of two places, each read when it is asked for, so that the
/// DOM of one never stands beside the DOM of the other. By their rows where
/// it can be (`compare_by_rows`): the first is read for what tells its rows
/// apart and let go, the second read beside that, and the first read once
/// more for the rows that differ. Else by their whole documents, each made
/// of its DOM in turn. Without `services`, each place gives those it holds.
pub(super) fn compare(
    before: &mut dyn FnMut() -> Result<WeakDom>,
    after: &mut dyn FnMut() -> Result<WeakDom>,
    services: Option<&BTreeSet<String>>,
    args: &ComparePlaceArgs,
) -> Result<Value> {
    let started = Instant::now();
    let keep = &args.props;
    let reading = Reading::of(args);
    let scope = |dom: &WeakDom| match services {
        Some(services) => services.clone(),
        None => (dom.root().children().iter())
            .filter_map(|id| Some(dom.get_by_ref(*id)?.name.clone()))
            .filter(|name| !is_engine_internal_service(name))
            .collect(),
    };
    let first = before()?;
    let of_first = scope(&first);
    let mut kept = keep_rows(&first, "before", &of_first, reading);
    // What a place gave up is handed back before the next is read: the
    // allocator would keep it beside what the next one takes.
    drop(first);
    if let Ok((kept, _)) = &mut kept {
        kept.settle();
    }
    release_between_phases();
    let second = after()?;
    let of_second = scope(&second);
    let all = &of_first.union(&of_second).cloned().collect::<BTreeSet<_>>();
    // A comparison by rows that fails leaves the error, as every other
    // answer, to the whole documents.
    let paired = kept.and_then(|(kept, _)| pair_rows(kept, &second, &of_second, reading));
    let second = match paired {
        Ok(Some(mut paired)) => {
            // The DOM goes while the first place is read again and the
            // answer is made.
            let released = std::thread::spawn(move || {
                drop(second);
                release_between_phases();
            });
            let by_rows = (|| -> Result<Option<Value>> {
                if !fill_rows(&mut paired, before, &of_first, reading)? {
                    return Ok(None);
                }
                rows_report(paired, started, all, args)
            })();
            if let Ok(Some(result)) = by_rows {
                return Ok(result);
            }
            let _ = released.join();
            after()?
        }
        // The rows were read of the second DOM and it stands as it was read.
        Ok(None) | Err(_) => second,
    };
    let (after, mut engine_managed) = compared_document(second, "after", all, keep)?;
    release_between_phases();
    let (before, omitted) = compared_document(before()?, "before", all, keep)?;
    release_between_phases();
    for (name, count) in omitted {
        *engine_managed.entry(name).or_default() += count;
    }
    compare_documents([before, after], engine_managed, started, all, args)
}

/// The DOM a comparison was given or lent.
fn lent(dom: &impl ComparedDom) -> &WeakDom {
    dom.borrow()
}

/// The rows of a DOM as a comparison by rows reads them, with its instances
/// without their values and the id of each. Every row is read and few are
/// kept: a payload held once would stay held for the process.
fn compared_rows(
    dom: &WeakDom,
    prefix: &'static str,
    services: &BTreeSet<String>,
) -> Result<(SettingsBytecode, Vec<String>, RowPlan)> {
    let mut rows = dom_rows(dom, prefix, Some(services), true)?;
    rows.plan.holds_payloads = false;
    let document = rows.structure();
    let ids = (document.instances.iter())
        .map(|instance| instance.settings_id.clone())
        .collect();
    Ok((document, ids, rows.plan))
}

/// Whether two documents hold the same instances at the same places.
fn same_places(left: &SettingsBytecode, right: &SettingsBytecode) -> bool {
    left.instances.len() == right.instances.len()
        && (left.instances.iter().zip(&right.instances)).all(|(left, right)| {
            (&left.name, &left.class_name, left.parent_index)
                == (&right.name, &right.class_name, right.parent_index)
        })
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

/// A digest two rows share only where what it is taken of is equal.
type RowDigest = [u8; 16];

fn row_digest(digest: sha2::Sha256) -> RowDigest {
    use sha2::Digest as _;
    let mut short = RowDigest::default();
    short.copy_from_slice(&digest.finalize()[..16]);
    short
}

/// The property of a kept row that holds the digest of its values, which no
/// instance holds a property named as.
const KEPT_VALUES: &str = "\u{0}values";

/// The property of a kept row that holds a number of that digest: among many
/// namesakes the matching looks up by a number who may hold the same.
const KEPT_NEAR: &str = "\u{0}near";

/// How a comparison reads its rows: the properties it keeps though the
/// engine manages them, and whether the second place is a project.
#[derive(Clone, Copy)]
pub(super) struct Reading<'a> {
    keep: &'a [String],
    project: bool,
}

impl<'a> Reading<'a> {
    pub(super) fn of(args: &'a ComparePlaceArgs) -> Self {
        Self {
            keep: &args.props,
            project: args.against.is_none(),
        }
    }
}

/// What the answer of a comparison may leave out of a property of a class.
#[derive(Clone, Copy)]
struct LeftOut {
    /// `omit_one_sided_members` may take it from the instances of the class.
    one_sided: bool,
    /// The class has it and a project never captures it: against a project
    /// that lacks it, it says nothing (`report`).
    never_captured: bool,
}

thread_local! {
    /// `left_out` of the properties this thread asked of so far, and the
    /// properties a project captures of their classes.
    static LEFT_OUT: std::cell::RefCell<HashMap<String, HashMap<String, LeftOut>>> =
        Default::default();
    static CAPTURED: std::cell::RefCell<HashMap<String, std::collections::HashSet<String>>> =
        Default::default();
}

fn left_out(
    database: &rbx_reflection::ReflectionDatabase<'_>,
    class_name: &str,
    name: &str,
) -> LeftOut {
    let known = LEFT_OUT.with_borrow(|known| known.get(class_name)?.get(name).copied());
    if let Some(known) = known {
        return known;
    }
    let one_sided = database.classes.get(class_name).is_some_and(|class| {
        !class.tags.contains(&rbx_reflection::ClassTag::Service)
            && has_no_default(database, class, class_name, name)
    });
    let never_captured = name != "PhysicalConfigData"
        && rbx_property_descriptor(database, class_name, name).is_some()
        && CAPTURED.with_borrow_mut(|captured| {
            if !captured.contains_key(class_name) {
                let of_class = native_property_filter(database, class_name).allowed;
                captured.insert(class_name.to_string(), of_class);
            }
            !captured[class_name].contains(name)
        });
    let asked = LeftOut {
        one_sided,
        never_captured,
    };
    LEFT_OUT.with_borrow_mut(|known| {
        let names = known.entry(class_name.to_string()).or_default();
        names.insert(name.to_string(), asked);
    });
    asked
}

/// What the pairing of two documents reads of an instance
/// (`align_settings_ids_to_reference`): its name, class and parent, the
/// values that name other instances, and of its other values no more than
/// whether another instance holds the same. This is what a comparison keeps
/// of an instance to pair it: the values that name instances as they are,
/// and of the others one digest, so two documents of kept rows (`row`) pair
/// as the whole ones do where equal values are the same values. A property
/// a class may carry on one side alone is kept as it is: it goes before the
/// pairing. So is, against a project, one a project never captures: where
/// the project lacks it, it says nothing. `rest` is the digest of the values
/// the pairing does not read.
/// Nothing is kept of an instance with a property named as a kept row names
/// its digest.
#[derive(Clone)]
struct Kept {
    values: RowDigest,
    rest: RowDigest,
    properties: Map<String, Value>,
    attributes: Map<String, Value>,
}

impl Kept {
    fn of(
        record: &SettingsBytecodeInstance,
        database: &rbx_reflection::ReflectionDatabase<'_>,
        project: bool,
    ) -> Result<Self> {
        use sha2::Digest as _;
        let (mut properties, mut attributes) = (Map::new(), Map::new());
        let (mut values, mut rest) = (sha2::Sha256::new(), sha2::Sha256::new());
        for (name, value) in &record.properties {
            if name == KEPT_VALUES || name == KEPT_NEAR {
                bail!("A property is named as the digest of a kept row");
            }
            let apart = left_out(database, &record.class_name, name);
            if reconciliation_value_contains_reference(value) {
                properties.insert(name.clone(), value.clone());
            } else if apart.one_sided || project && apart.never_captured {
                properties.insert(name.clone(), crate::settings::held::held(value.clone()));
            } else if identity_value_is_stable(name, value, true) {
                serde_json::to_writer(&mut values, &(name, value))?;
            } else {
                serde_json::to_writer(&mut rest, &(name, value))?;
            }
        }
        values.update(b"|");
        for (name, value) in &record.attributes {
            if reconciliation_value_contains_reference(value) {
                attributes.insert(name.clone(), value.clone());
            } else {
                serde_json::to_writer(&mut values, &(name, value))?;
            }
        }
        Ok(Self {
            values: row_digest(values),
            rest: row_digest(rest),
            properties,
            attributes,
        })
    }

    /// The digest of all the record held.
    fn all(&self) -> Result<RowDigest> {
        use sha2::Digest as _;
        let mut all = sha2::Sha256::new();
        all.update(self.values);
        all.update(self.rest);
        serde_json::to_writer(&mut all, &(&self.properties, &self.attributes))?;
        Ok(row_digest(all))
    }

    /// Makes of the instance whose record this was kept of, which holds no
    /// value, its kept row: what was kept as it is goes to the row.
    fn put(&mut self, row: &mut SettingsBytecodeInstance) {
        row.properties = std::mem::take(&mut self.properties);
        row.attributes = std::mem::take(&mut self.attributes);
        let near = u64::from(self.values[0])
            | u64::from(self.values[1]) << 8
            | u64::from(self.values[2] & 7) << 16;
        let mut text = String::with_capacity(2 * self.values.len());
        for byte in self.values {
            use std::fmt::Write as _;
            let _ = write!(text, "{byte:02x}");
        }
        row.properties.insert(KEPT_NEAR.to_string(), json!(near));
        row.properties.insert(KEPT_VALUES.to_string(), json!(text));
    }
}

/// What two kept rows of one class hold as it is, as the comparison finds
/// it (`changed_fields_of`): whether nothing differs, and whether nothing
/// does that its answer names. Against a project (`project`) the answer
/// leaves out what a project never captures where the project, the second
/// of the two, lacks it. The instances the values of both name are the same
/// for either.
fn kept_alike(
    one: &SettingsBytecodeInstance,
    other: &SettingsBytecodeInstance,
    project: bool,
) -> [bool; 2] {
    if one.properties == other.properties && one.attributes == other.attributes {
        return [true; 2];
    }
    let part = |fields: &Map<String, Value>, names: bool| {
        (fields.iter())
            .filter(|(_, value)| reconciliation_value_contains_reference(value) == names)
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Map<_, _>>()
    };
    let attributes = [part(&one.attributes, false), part(&other.attributes, false)];
    if part(&one.properties, true) != part(&other.properties, true)
        || part(&one.attributes, true) != part(&other.attributes, true)
        || !changed_fields(&attributes[0], &attributes[1], false).is_empty()
    {
        return [false; 2];
    }
    let changed = changed_fields_of(
        Some(&other.class_name),
        &part(&one.properties, false),
        &part(&other.properties, false),
        false,
    );
    let database = rbx_reflection_database::get().ok();
    let says_nothing = |field: &Value| {
        let name = field["name"].as_str().unwrap_or_default();
        project
            && !other.properties.contains_key(name)
            && database
                .is_some_and(|database| left_out(database, &other.class_name, name).never_captured)
    };
    [changed.is_empty(), changed.iter().all(says_nothing)]
}

/// Whether the comparison surely finds a difference between two kept rows,
/// whatever the values their digests are of: in what both hold as it is,
/// beside the instances it names. A row that is not kept says nothing.
fn kept_surely_differ(one: &SettingsBytecodeInstance, other: &SettingsBytecodeInstance) -> bool {
    if one.name != other.name || one.class_name != other.class_name {
        return true;
    }
    let kept = |row: &SettingsBytecodeInstance| row.properties.contains_key(KEPT_VALUES);
    if !kept(one) || !kept(other) {
        return false;
    }
    let part = |fields: &Map<String, Value>| {
        (fields.iter())
            .filter(|(name, value)| {
                *name != KEPT_VALUES
                    && *name != KEPT_NEAR
                    && !reconciliation_value_contains_reference(value)
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Map<_, _>>()
    };
    let (one, other) = (
        part(&one.properties),
        (&other.class_name, part(&other.properties)),
    );
    !changed_fields_of(Some(other.0), &one, &other.1, false).is_empty()
}

/// Whether two subtrees of kept rows surely do not match
/// (`subtree_matches`): a row of one surely differs from the row at its
/// place in the other, or holds other rows.
fn subtrees_surely_differ(
    (first, first_children, at): (&SettingsBytecode, &[Vec<usize>], usize),
    (second, second_children, row): (&SettingsBytecode, &[Vec<usize>], usize),
) -> bool {
    let mut pending = vec![(at, row)];
    while let Some((at, row)) = pending.pop() {
        if first_children[at].len() != second_children[row].len()
            || kept_surely_differ(&first.instances[at], &second.instances[row])
        {
            return true;
        }
        let under = first_children[at].iter().copied();
        pending.extend(under.zip(second_children[row].iter().copied()));
    }
    false
}

/// `add_class_members` of many instances, counted on every thread.
fn count_class_members(
    classes: &mut HashMap<String, ClassMembers>,
    instances: &[SettingsBytecodeInstance],
) {
    let merge = |classes: &mut HashMap<String, ClassMembers>,
                 more: HashMap<String, ClassMembers>| {
        for (class, members) in more {
            match classes.get_mut(&class) {
                Some(known) => {
                    known.instances += members.instances;
                    for (name, count) in members.carried {
                        *known.carried.entry(name).or_default() += count;
                    }
                }
                None => drop(classes.insert(class, members)),
            }
        }
    };
    let counted = (instances.par_chunks(1_024))
        .map(|instances| {
            let mut classes = HashMap::new();
            add_class_members(&mut classes, instances);
            classes
        })
        .reduce(HashMap::new, |mut classes, more| {
            merge(&mut classes, more);
            classes
        });
    merge(classes, counted);
}

/// What is kept of records as `records` gives them, each with the digest of
/// all it held. With `ids`, the references kept are stabilized under them.
fn keep_records(
    records: &[SettingsBytecodeInstance],
    ids: Option<&[String]>,
    project: bool,
) -> Result<Vec<(Kept, RowDigest)>> {
    let database = rbx_reflection_database::get()?;
    (records.par_iter())
        .map(|record| {
            let mut kept = Kept::of(record, database, project)?;
            let all = kept.all()?;
            if let Some(ids) = ids {
                stabilize_record_references(&mut kept.properties, ids);
                stabilize_record_references(&mut kept.attributes, ids);
            }
            Ok((kept, all))
        })
        .collect()
}

/// The kept rows of a whole document.
fn kept_document(document: &SettingsBytecode, project: bool) -> Result<SettingsBytecode> {
    let database = rbx_reflection_database::get()?;
    let instances = (document.instances.par_iter())
        .map(|instance| {
            let mut row = instance.bare();
            Kept::of(instance, database, project)?.put(&mut row);
            Ok(row)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(SettingsBytecode {
        version: document.version,
        instances,
    })
}

/// Where the rows under each row of a document end: its rows stand in the
/// order of its tree.
fn subtree_ends(document: &SettingsBytecode) -> Vec<usize> {
    let count = document.instances.len();
    let mut ends = (1..=count).collect::<Vec<_>>();
    for row in (0..count).rev() {
        if let Some(parent) = document.instances[row].parent_index {
            ends[parent] = ends[parent].max(ends[row]);
        }
    }
    ends
}

/// Of each row of a document a key of the names and classes of the row and
/// of the rows under it, in their order. Rows of other keys never match
/// (`subtree_matches`).
fn subtree_shapes(document: &SettingsBytecode, children: &[Vec<usize>]) -> Vec<u64> {
    use std::hash::{Hash, Hasher};
    let mut shapes = vec![0; document.instances.len()];
    for (row, instance) in document.instances.iter().enumerate().rev() {
        let mut shape = std::collections::hash_map::DefaultHasher::new();
        instance.name.hash(&mut shape);
        instance.class_name.hash(&mut shape);
        for child in &children[row] {
            shapes[*child].hash(&mut shape);
        }
        shapes[row] = shape.finish();
    }
    shapes
}

/// `compare` of two DOMs by their rows. The rows are paired as the whole
/// documents are, by what is kept of each (`Kept`). A pair that holds the
/// same holds no value in either document, and two rows without values are
/// equal wherever the comparison looks, so the documents hold whole only the
/// services, the rows one place alone holds, the pairs that differ and the
/// twins `pair_duplicate_siblings_by_content` weighs anew. `None` where the
/// whole documents have to say: the rows do not pair, twins swapped what
/// they hold, or the first place changed while the second was read.
#[cfg(test)]
pub(super) fn compare_by_rows(
    before: &WeakDom,
    after: &WeakDom,
    services: &BTreeSet<String>,
    args: &ComparePlaceArgs,
) -> Result<Option<Value>> {
    let started = Instant::now();
    let reading = Reading::of(args);
    let (kept, _) = keep_rows(before, "before", services, reading)?;
    let Some(mut paired) = pair_rows(kept, after, services, reading)? else {
        return Ok(None);
    };
    if !fill_rows(&mut paired, &mut || Ok(before), services, reading)? {
        return Ok(None);
    }
    rows_report(paired, started, services, args)
}

/// What a comparison by rows keeps of a DOM once it lets go of it: its
/// instances without values, the id each stands under, what is kept of each
/// (`Kept`) with the digest of all it held, its services whole, the members
/// its classes carry and the values the engine manages, counted.
struct RowsKept {
    document: SettingsBytecode,
    ids: Vec<String>,
    rows: Vec<Kept>,
    all: Vec<RowDigest>,
    services: Vec<(usize, SettingsBytecodeInstance)>,
    members: HashMap<String, ClassMembers>,
    engine_managed: BTreeMap<String, usize>,
}

impl RowsKept {
    /// Makes kept rows of the rows of the document `rows` names, of every
    /// one without it, but those that are already.
    fn put_kept(&mut self, rows: Option<&[bool]>) {
        (self.document.instances.par_iter_mut())
            .zip(self.rows.par_iter_mut())
            .enumerate()
            .for_each(|(row, (instance, kept))| {
                if rows.is_none_or(|rows| rows[row])
                    && !instance.properties.contains_key(KEPT_VALUES)
                {
                    kept.put(instance);
                }
            });
    }

    /// Leaves the rows of the document without values again, each under its
    /// own id.
    fn bare(&mut self) {
        let rows = self.document.instances.par_iter_mut().zip(&self.ids);
        rows.for_each(|(instance, id)| {
            instance.settings_id.clone_from(id);
            instance.properties = Map::new();
            instance.attributes = Map::new();
        });
    }

    /// Moves what is kept to memory of its own. It was taken between the
    /// values of a DOM, whose memory goes back to the system only where
    /// nothing else stands in it.
    fn settle(&mut self) {
        self.rows = self.rows.clone();
    }

    /// The document a comparison by rows compares, before the rows it reads
    /// whole are put in it: the instances without values and the services
    /// whole.
    fn emptied(self) -> (SettingsBytecode, Vec<String>, Vec<RowDigest>) {
        let Self {
            mut document,
            ids,
            all,
            services,
            ..
        } = self;
        for (row, record) in services {
            put_record(&mut document, &ids, row, record);
        }
        (document, ids, all)
    }
}

/// The documents of two DOMs whose rows are paired, the second read: each
/// holds whole its services and the rows `rows_read_whole` names, but the
/// first owes those of `owed`, of which `all` tells what its DOM held. A row
/// of the first that is not read holds what `counted_unnamed` left it.
struct RowsPaired {
    documents: [SettingsBytecode; 2],
    ids: Vec<String>,
    all: Vec<RowDigest>,
    members: [HashMap<String, ClassMembers>; 2],
    engine_managed: BTreeMap<String, usize>,
    one_sided: BTreeMap<String, usize>,
    owed: Vec<usize>,
}

/// Gives up the records of a turn on the threads that read them: one thread
/// freeing every value of a large place takes a noticeable part of a second.
fn give_up(records: Vec<SettingsBytecodeInstance>) {
    records.into_par_iter().for_each(drop);
}

/// Where the time of a pass over the rows of a DOM went.
#[derive(Default)]
struct RowsSpent {
    reading: std::time::Duration,
    digesting: std::time::Duration,
    counting: std::time::Duration,
}

impl RowsSpent {
    fn timed<T>(spent: &mut std::time::Duration, work: impl FnOnce() -> T) -> T {
        let started = Instant::now();
        let done = work();
        *spent += started.elapsed();
        done
    }
    fn reading<T>(&mut self, work: impl FnOnce() -> T) -> T {
        Self::timed(&mut self.reading, work)
    }
    fn digesting<T>(&mut self, work: impl FnOnce() -> T) -> T {
        Self::timed(&mut self.digesting, work)
    }
    fn counting<T>(&mut self, work: impl FnOnce() -> T) -> T {
        Self::timed(&mut self.counting, work)
    }
    fn log(&self, side: &str) {
        let ms = |spent: std::time::Duration| spent.as_secs_f64() * 1000.0;
        log_global(
            4,
            format_args!(
                "[renium] place comparison {side} rows: read {:.1}ms, digested {:.1}ms, counted and freed {:.1}ms",
                ms(self.reading),
                ms(self.digesting),
                ms(self.counting)
            ),
        );
    }
}

/// Reads the rows of a DOM a turn at a time for what the comparison keeps
/// of them, and gives how it read them. Every row is read and its values
/// let go: the DOM stands as it was read.
fn keep_rows(
    dom: &WeakDom,
    prefix: &'static str,
    services: &BTreeSet<String>,
    reading: Reading<'_>,
) -> Result<(RowsKept, RowPlan)> {
    let (document, ids, plan) = compared_rows(dom, prefix, services)?;
    let mut kept = RowsKept {
        document,
        rows: Vec::with_capacity(ids.len()),
        all: Vec::with_capacity(ids.len()),
        ids,
        services: Vec::new(),
        members: HashMap::new(),
        engine_managed: BTreeMap::new(),
    };
    let every = (0..kept.ids.len()).collect::<Vec<_>>();
    let mut spent = RowsSpent::default();
    for turn in every.chunks(WHOLE_ROWS_AT_ONCE) {
        let (read, omitted) = spent.reading(|| records(dom, &plan, turn, Some(reading.keep)))?;
        for (name, count) in omitted {
            *kept.engine_managed.entry(name).or_default() += count;
        }
        spent.counting(|| count_class_members(&mut kept.members, &read));
        let rows = spent.digesting(|| keep_records(&read, Some(&kept.ids), reading.project))?;
        let mut spare = Vec::with_capacity(read.len());
        for ((row, record), (of_row, all)) in turn.iter().zip(read).zip(rows) {
            kept.rows.push(of_row);
            kept.all.push(all);
            if record.parent_index.is_none() {
                kept.services.push((*row, record));
            } else {
                spare.push(record);
            }
        }
        spent.counting(|| give_up(spare));
    }
    spent.log(prefix);
    Ok((kept, plan))
}

/// The row of the first of two documents each row of the second is, and the
/// reverse, with whether the comparison finds no difference between the two
/// (`same`) and whether it finds none its answer names (`alike`).
struct Pairing {
    in_first: Vec<Option<usize>>,
    in_second: Vec<Option<usize>>,
    /// Of each row of the second.
    same: Vec<bool>,
    alike: Vec<bool>,
    /// The id each row of the second takes.
    ids: Vec<String>,
}

impl Pairing {
    /// The pairing of two documents of kept rows of which the second took
    /// the ids of the first, with `rest` of each row of the two. A row
    /// `differs` does not name holds what its counterpart holds. The second
    /// is a project where `project`.
    fn of(
        [first, second]: [&SettingsBytecode; 2],
        rest: [&[Kept]; 2],
        differs: Option<&[bool]>,
        project: bool,
    ) -> Self {
        let rows = (first.instances.iter().enumerate())
            .map(|(row, instance)| (instance.settings_id.as_str(), row))
            .collect::<ahash::AHashMap<_, _>>();
        let in_first = (second.instances.iter())
            .map(|instance| rows.get(instance.settings_id.as_str()).copied())
            .collect::<Vec<_>>();
        let mut in_second = vec![None; first.instances.len()];
        for (row, at) in in_first.iter().enumerate() {
            if let Some(at) = at {
                in_second[*at] = Some(row);
            }
        }
        let weighed = |row: usize| {
            let Some(at) = in_first[row] else {
                return [false; 2];
            };
            let (one, other) = (&first.instances[at], &second.instances[row]);
            if one.name != other.name || one.class_name != other.class_name {
                [false; 2]
            } else if differs.is_some_and(|differs| !differs[row]) {
                [true; 2]
            } else if rest[0][at].rest != rest[1][row].rest {
                [false; 2]
            } else {
                kept_alike(one, other, project)
            }
        };
        let (same, alike) = (0..second.instances.len())
            .into_par_iter()
            .map(|row| {
                let [same, alike] = weighed(row);
                (same, alike)
            })
            .unzip();
        let ids = (second.instances.iter())
            .map(|instance| instance.settings_id.clone())
            .collect();
        Self {
            in_first,
            in_second,
            same,
            alike,
            ids,
        }
    }
}

/// Pairs the rows of two DOMs by what is kept of them, as
/// `compare_documents` pairs the whole documents. Rows that stand at the
/// same places are paired by those wherever the pairing leaves each at its
/// place (`align_settings_ids_by_place`), which rows that hold the same at
/// one place say as well without what they hold: only the rows that differ
/// there are made. `None` where the rows do not pair. The kept rows it made
/// are left in the documents.
fn paired(
    first: &mut RowsKept,
    second: &mut RowsKept,
    members: &[HashMap<String, ClassMembers>; 2],
    (database, project): (&rbx_reflection::ReflectionDatabase<'_>, bool),
) -> Option<Pairing> {
    if same_places(&first.document, &second.document) {
        let differs = (first.all.iter().zip(&second.all))
            .zip(&first.document.instances)
            .map(|((one, other), instance)| one != other || instance.parent_index.is_none())
            .collect::<Vec<_>>();
        first.put_kept(Some(&differs));
        second.put_kept(Some(&differs));
        omit_one_sided_members(&mut first.document, &mut second.document, members, database);
        if align_settings_ids_by_place(&first.document, &mut second.document) {
            let rest = [&first.rows[..], &second.rows[..]];
            let documents = [&first.document, &second.document];
            return Some(Pairing::of(documents, rest, Some(&differs), project));
        }
    }
    first.put_kept(None);
    second.put_kept(None);
    omit_one_sided_members(&mut first.document, &mut second.document, members, database);
    align_settings_ids_to_reference(&first.document, &mut second.document).then(|| {
        let rest = [&first.rows[..], &second.rows[..]];
        Pairing::of([&first.document, &second.document], rest, None, project)
    })
}

/// The rows of each of two paired documents a comparison by rows reads
/// whole: a service, a row its place alone holds, a pair that does not hold
/// the same, and the twins `pair_duplicate_siblings_by_content` weighs
/// against each other's counterparts, with every row under them and under
/// those counterparts. It weighs the twins of a group where one of them does
/// not match its counterpart, and one whose rows are each the counterpart of
/// the row at its place there, and hold the same, does match it. It pairs
/// them anew only where each matches one of the counterparts, which rows of
/// other names, classes or numbers under them never do (`subtree_shapes`),
/// nor rows that surely differ in what they hold as it is
/// (`subtrees_surely_differ`): such a group stays as it is paired, unread.
/// The documents hold the kept rows the pairing made.
fn rows_read_whole([first, second]: [&SettingsBytecode; 2], pairing: &Pairing) -> [Vec<bool>; 2] {
    let (one, other) = (&first.instances, &second.instances);
    let children = [
        settings_children_by_parent(first),
        settings_children_by_parent(second),
    ];
    // A row and every row under it matches its counterpart, row for row.
    let mut matches = pairing.same.clone();
    for row in (0..other.len()).rev() {
        matches[row] = matches[row]
            && pairing.in_first[row].is_some_and(|at| {
                children[0][at].len() == children[1][row].len()
                    && (children[0][at].iter().zip(&children[1][row])).all(|(under, beside)| {
                        pairing.in_first[*beside] == Some(*under) && matches[*beside]
                    })
            });
    }
    let mut whole = [
        (0..one.len())
            .map(|row| pairing.in_second[row].is_none() || one[row].parent_index.is_none())
            .collect::<Vec<_>>(),
        (0..other.len())
            .map(|row| !pairing.alike[row] || other[row].parent_index.is_none())
            .collect::<Vec<_>>(),
    ];
    let ends = [subtree_ends(first), subtree_ends(second)];
    let shapes = std::cell::OnceCell::new();
    let mut weighed = 0;
    for siblings in children[1].iter().filter(|siblings| siblings.len() > 1) {
        let mut groups = ahash::AHashMap::<(&str, &str), Vec<usize>>::new();
        for sibling in siblings {
            let instance = &other[*sibling];
            let key = (instance.name.as_str(), instance.class_name.as_str());
            groups.entry(key).or_default().push(*sibling);
        }
        for twins in groups.into_values() {
            if !(2..=DUPLICATE_REPAIR_GROUP_LIMIT).contains(&twins.len())
                || twins.iter().all(|twin| matches[*twin])
            {
                continue;
            }
            let counterparts =
                (twins.iter().map(|twin| pairing.in_first[*twin])).collect::<Option<Vec<_>>>();
            // Twins of which one has no counterpart stay as they are paired.
            let Some(counterparts) = counterparts else {
                continue;
            };
            let [of_first, of_second] = shapes.get_or_init(|| {
                [
                    subtree_shapes(first, &children[0]),
                    subtree_shapes(second, &children[1]),
                ]
            });
            let mut shaped = [
                counterparts
                    .iter()
                    .map(|at| of_first[*at])
                    .collect::<Vec<_>>(),
                twins
                    .iter()
                    .map(|twin| of_second[*twin])
                    .collect::<Vec<_>>(),
            ];
            shaped[0].sort_unstable();
            shaped[1].sort_unstable();
            if shaped[0] != shaped[1] {
                continue;
            }
            let matches_none = |twin: &usize| {
                counterparts.iter().all(|at| {
                    subtrees_surely_differ(
                        (first, &children[0], *at),
                        (second, &children[1], *twin),
                    )
                })
            };
            let mut unmatched = twins.iter().filter(|twin| !matches[**twin]).take(3);
            if unmatched.any(matches_none) {
                continue;
            }
            weighed += 1;
            for twin in twins {
                whole[1][twin..ends[1][twin]].fill(true);
            }
            for at in counterparts {
                whole[0][at..ends[0][at]].fill(true);
            }
        }
    }
    // A row read whole is compared with its counterpart, which is read too.
    for (row, at) in pairing.in_first.iter().enumerate() {
        if let Some(at) = at {
            let either = whole[0][*at] || whole[1][row];
            whole[0][*at] = either;
            whole[1][row] = either;
        }
    }
    log_global(
        4,
        format_args!("[renium] place comparison twins: {weighed} groups read whole"),
    );
    whole
}

/// Of each pair of two paired documents of kept rows that is not read whole
/// though its rows differ (`whole` of the second, `Pairing::alike`): the row
/// of the first and what it holds as it is that the second lacks. The answer
/// counts it without naming the pair, so it stays in the row.
fn counted_unnamed(
    [first, second]: [&SettingsBytecode; 2],
    pairing: &Pairing,
    whole: &[bool],
) -> Vec<(usize, Map<String, Value>)> {
    (0..second.instances.len())
        .into_par_iter()
        .filter(|row| pairing.alike[*row] && !pairing.same[*row] && !whole[*row])
        .filter_map(|row| {
            let at = pairing.in_first[row]?;
            let (one, other) = (&first.instances[at], &second.instances[row]);
            let unnamed = (one.properties.iter())
                .filter(|(name, value)| {
                    !other.properties.contains_key(*name)
                        && !reconciliation_value_contains_reference(value)
                })
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect::<Map<_, _>>();
            Some((at, unnamed))
        })
        .collect()
}

/// Reads the rows of the second DOM beside what the first left, pairs the
/// rows of the two and puts in the second document the rows it reads whole.
/// `None` where the rows do not pair.
fn pair_rows(
    mut first: RowsKept,
    after: &WeakDom,
    services: &BTreeSet<String>,
    reading: Reading<'_>,
) -> Result<Option<RowsPaired>> {
    let (mut second, plan) = keep_rows(after, "after", services, reading)?;
    let members = [
        std::mem::take(&mut first.members),
        std::mem::take(&mut second.members),
    ];
    let started = Instant::now();
    let asked = (plan.database, reading.project);
    let pairing = paired(&mut first, &mut second, &members, asked);
    log_global(
        4,
        format_args!(
            "[renium] place comparison identity: {:.1}ms",
            started.elapsed().as_secs_f64() * 1000.0
        ),
    );
    let whole = (pairing.as_ref())
        .map(|pairing| rows_read_whole([&first.document, &second.document], pairing));
    let unnamed = match (&pairing, &whole) {
        (Some(pairing), Some(whole)) => {
            counted_unnamed([&first.document, &second.document], pairing, &whole[1])
        }
        _ => Vec::new(),
    };
    first.bare();
    second.bare();
    for (row, held) in unnamed {
        first.document.instances[row].properties = held;
    }
    let (Some(pairing), Some([whole_first, whole_second])) = (pairing, whole) else {
        return Ok(None);
    };
    let mut engine_managed = std::mem::take(&mut first.engine_managed);
    for (name, count) in std::mem::take(&mut second.engine_managed) {
        *engine_managed.entry(name).or_default() += count;
    }
    let (before, first_ids, all) = first.emptied();
    let (mut document, ids, _) = second.emptied();
    let read = (0..ids.len())
        .filter(|row| whole_second[*row] && document.instances[*row].parent_index.is_some())
        .collect::<Vec<_>>();
    for turn in read.chunks(WHOLE_ROWS_AT_ONCE) {
        let (records, _) = records(after, &plan, turn, Some(reading.keep))?;
        for (row, record) in turn.iter().zip(records) {
            put_record(&mut document, &ids, *row, record);
        }
    }
    take_settings_ids(&mut document, pairing.ids);
    let owed = (0..first_ids.len())
        .filter(|row| whole_first[*row] && before.instances[*row].parent_index.is_some())
        .collect::<Vec<_>>();
    // Of few rows, which classes: a row that differs in every comparison of
    // a place is worth knowing.
    let classes = (owed.iter().take(9))
        .map(|row| before.instances[*row].class_name.as_str())
        .collect::<BTreeSet<_>>();
    log_global(
        4,
        format_args!(
            "[renium] place comparison rows: {} of {} and {} of {} read whole{}",
            owed.len(),
            first_ids.len(),
            read.len(),
            ids.len(),
            if (1..9).contains(&owed.len()) {
                format!(", of {}", classes.into_iter().collect::<Vec<_>>().join(" "))
            } else {
                String::new()
            }
        ),
    );
    Ok(Some(RowsPaired {
        documents: [before, document],
        ids: first_ids,
        all,
        one_sided: one_sided_counts(&members, plan.database),
        members,
        engine_managed,
        owed,
    }))
}

/// Puts in the first document the rows it owes, of its DOM read once more
/// where it owes any. False where that DOM no longer holds what it held: a
/// place file written between the two reads.
fn fill_rows<D: ComparedDom>(
    paired: &mut RowsPaired,
    before: &mut dyn FnMut() -> Result<D>,
    services: &BTreeSet<String>,
    reading: Reading<'_>,
) -> Result<bool> {
    if paired.owed.is_empty() {
        return Ok(true);
    }
    let mut before = before()?;
    let mut rows = dom_rows(lent(&before), "before", Some(services), true)?;
    rows.plan.holds_payloads = false;
    if !rows.stand_as(&paired.documents[0]) {
        return Ok(false);
    }
    let plan = rows.plan;
    for turn in paired.owed.chunks(WHOLE_ROWS_AT_ONCE) {
        let (read, _) = records(lent(&before), &plan, turn, Some(reading.keep))?;
        let held = keep_records(&read, None, reading.project)?;
        if (held.iter().zip(turn)).any(|((_, all), row)| *all != paired.all[*row]) {
            return Ok(false);
        }
        for (row, record) in turn.iter().zip(read) {
            put_record(&mut paired.documents[0], &paired.ids, *row, record);
        }
        before.release(&turn.iter().map(|row| plan.refs[*row]).collect::<Vec<_>>());
    }
    Ok(true)
}

/// The answer of two documents `pair_rows` and `fill_rows` made, or `None`
/// where the whole documents have to say: twins swapped what they hold, and
/// a row left unread may name one of them.
fn rows_report(
    paired: RowsPaired,
    started: Instant,
    services: &BTreeSet<String>,
    args: &ComparePlaceArgs,
) -> Result<Option<Value>> {
    let [mut before, mut after] = paired.documents;
    if !paired.one_sided.is_empty() {
        let database = rbx_reflection_database::get()?;
        omit_one_sided_members(&mut before, &mut after, &paired.members, database);
    }
    log_global(
        4,
        format_args!(
            "[renium] place comparison rows: {:.1}ms",
            started.elapsed().as_secs_f64() * 1000.0
        ),
    );
    if pair_duplicate_siblings_by_content(&before, &mut after) > 0 {
        return Ok(None);
    }
    let set_aside = (paired.engine_managed, paired.one_sided, 0);
    report(before, after, set_aside, services, args).map(Some)
}

/// Puts a record in its row of a document, its references stabilized under
/// the document's `ids`, as `document` holds it.
fn put_record(
    document: &mut SettingsBytecode,
    ids: &[String],
    row: usize,
    mut record: SettingsBytecodeInstance,
) {
    stabilize_record_references(&mut record.properties, ids);
    stabilize_record_references(&mut record.attributes, ids);
    document.instances[row] = record;
}

/// `compare` of the whole documents of two DOMs. A DOM given whole is
/// released as its document is read, the first before the second is
/// converted, so the two documents never stand beside both DOMs.
#[cfg(test)]
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
/// values the engine manages that `engine_managed` counts. The instances are
/// paired by what is kept of them (`Kept`), as a comparison by rows pairs
/// them, or by all they hold where nothing is kept of one.
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
    let project = Reading::of(args).project;
    let aligned = match (
        kept_document(&before, project),
        kept_document(&after, project),
    ) {
        (Ok(kept), Ok(mut paired)) => {
            let aligned = align_settings_ids_to_reference(&kept, &mut paired);
            if aligned {
                let ids = (paired.instances.into_iter())
                    .map(|instance| instance.settings_id)
                    .collect();
                take_settings_ids(&mut after, ids);
            }
            aligned
        }
        _ => align_settings_ids_to_reference(&before, &mut after),
    };
    if !aligned {
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
