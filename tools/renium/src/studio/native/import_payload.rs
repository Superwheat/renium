//! Native insertion uses the existing services as anchors, not Folder wrappers.
//! Serializable container settings load in place; protected setters stay separate.
use super::*;
use crate::editor::review::{
    is_engine_managed_editor_property, is_externally_managed_editor_property,
};
use crate::editor::types::{
    EditorNativeAlias, EditorNativeBatch, EditorNativeBinding, EditorNativeClass, EditorNativePlan,
    EditorNativeReplacement,
};
use crate::rbx::encode::rbx_logical_property_name;
use ahash::{AHashMap, AHashSet};
use rbx_binary::InstanceBindingMode;

pub(super) fn expected_structure(
    dom: &RbxWeakDom,
    root: RbxRef,
) -> Result<Option<crate::editor::types::EditorBinaryStructure>> {
    let mut strings = Vec::new();
    let mut indices = AHashMap::new();
    let mut nodes = Vec::new();
    let mut pending = vec![root];
    while let Some(referent) = pending.pop() {
        let instance = dom
            .get_by_ref(referent)
            .context("Import receipt lost an instance")?;
        // Group selection excludes retained packages. Fresh PackageLinks are
        // loaded by the same native binary reader as their complete contents.
        if instance.class.as_str() == "Terrain" {
            return Ok(None);
        }
        for value in [instance.class.as_str(), instance.name.as_str()] {
            let index = *indices.entry(value).or_insert_with(|| {
                let index = strings.len() as u32;
                strings.push(value.to_string());
                index
            });
            nodes.extend_from_slice(&index.to_le_bytes());
        }
        let children = u32::try_from(instance.children().len())?;
        nodes.extend_from_slice(&children.to_le_bytes());
        pending.extend(instance.children().iter().rev().copied());
    }
    Ok(Some(crate::editor::types::EditorBinaryStructure {
        strings,
        nodes: base64::encode(nodes),
    }))
}

#[test]
fn ordinary_payload_receipt_preserves_complete_preorder_and_keeps_special_classes_on_readback() {
    let mut dom = RbxWeakDom::new(RbxInstanceBuilder::new("Folder").with_name("Wrapper"));
    let left = dom.insert(
        dom.root_ref(),
        RbxInstanceBuilder::new("Folder").with_name("Duplicate"),
    );
    dom.insert(
        left,
        RbxInstanceBuilder::new("StringValue").with_name("Value"),
    );
    let right = dom.insert(
        dom.root_ref(),
        RbxInstanceBuilder::new("Folder").with_name("Duplicate"),
    );
    dom.insert(
        right,
        RbxInstanceBuilder::new("NumberValue").with_name("Value"),
    );
    let receipt = expected_structure(&dom, dom.root_ref()).unwrap().unwrap();
    let nodes = base64::decode(&receipt.nodes).unwrap();
    let actual = nodes
        .chunks_exact(12)
        .map(|row| {
            let index =
                |offset| u32::from_le_bytes(row[offset..offset + 4].try_into().unwrap()) as usize;
            (
                receipt.strings[index(0)].as_str(),
                receipt.strings[index(4)].as_str(),
                index(8),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        vec![
            ("Folder", "Wrapper", 2),
            ("Folder", "Duplicate", 1),
            ("StringValue", "Value", 0),
            ("Folder", "Duplicate", 1),
            ("NumberValue", "Value", 0),
        ]
    );
    let package = dom.insert(right, RbxInstanceBuilder::new("PackageLink"));
    assert!(expected_structure(&dom, dom.root_ref()).unwrap().is_some());
    dom.destroy(package);
    dom.insert(right, RbxInstanceBuilder::new("Terrain"));
    assert!(expected_structure(&dom, dom.root_ref()).unwrap().is_none());
}

pub(super) fn encode_services(
    dom: &mut RbxWeakDom,
    service_roots: &[(String, RbxRef)],
    groups: Vec<PendingEditorBinaryGroup>,
    package_plans: Vec<EditorPackageGroupPlan>,
    post_apply: &mut HashMap<String, HashSet<String>>,
) -> Result<(
    Vec<u8>,
    Vec<EditorBinaryImportGroup>,
    EditorNativeReplacement,
)> {
    let mut stages = crate::app::timing::trace_stages(
        "native.encode",
        "bind retained services containers and viewport",
    );
    anyhow::ensure!(
        groups.len() == package_plans.len(),
        "Native import plans differ"
    );
    let mut requested = HashMap::new();
    let mut included = service_roots
        .iter()
        .map(|(_, root)| *root)
        .collect::<HashSet<_>>();
    for (_, root) in service_roots {
        requested.insert(*root, InstanceBindingMode::ReferenceOnly);
    }
    for group in &groups {
        let target = rbx_dom_instance_by_path_unique(dom, &group.target_path, &[])?;
        included.insert(target);
        included.extend(group.roots.iter().copied());
        // Services are resolved by the engine's service provider. Its ordinary
        // factory path is used only for retained descendants such as Terrain.
        requested.insert(
            target,
            if group.additive {
                InstanceBindingMode::ReferenceOnly
            } else {
                InstanceBindingMode::Properties
            },
        );
        if let Some(camera) = &group.viewport_camera {
            let referent =
                rbx_dom_instance_by_path_unique(dom, &camera.path_segments, &camera.path_ordinals)?;
            anyhow::ensure!(
                dom.get_by_ref(referent)
                    .is_some_and(|node| node.class.as_str() == "Camera"),
                "Viewport role does not refer to a Camera"
            );
            // Bind the viewport's actual live identity without setting its name,
            // pose, field of view or parent. Other cameras remain ordinary data.
            requested.insert(referent, InstanceBindingMode::ReferenceOnly);
        }
    }
    remove_excluded_children(dom, &requested, &included)?;
    stages.next("filter non-writable retained properties and references");
    let database = rbx_reflection_database::get()?;
    filter_retained_properties(dom, &requested, post_apply, database)?;
    // Bound actual instance trees, not only transport bytes or service count.
    // Zero is an internal whole-payload A/B control.
    stages.next("index subtree sizes and native insertion order");
    let limit = std::env::var("RENIUM_NATIVE_IMPORT_BATCH_INSTANCES")
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(512);
    anyhow::ensure!(limit <= 1_000_000, "Native import batch limit is oversized");
    let mut preorder = Vec::new();
    let mut subtree_sizes = AHashMap::<RbxRef, usize>::new();
    let mut postorder = AHashMap::<RbxRef, usize>::new();
    for (_, root) in service_roots {
        let mut stack = vec![(*root, false)];
        while let Some((referent, exiting)) = stack.pop() {
            let node = dom
                .get_by_ref(referent)
                .context("Native subtree disappeared")?;
            if exiting {
                let size = 1 + node
                    .children()
                    .iter()
                    .map(|child| subtree_sizes[child])
                    .sum::<usize>();
                subtree_sizes.insert(referent, size);
                postorder.insert(referent, postorder.len());
            } else {
                preorder.push(referent);
                stack.push((referent, true));
                stack.extend(node.children().iter().rev().map(|child| (*child, false)));
            }
        }
    }
    stages.next("partition bounded subtrees");
    let mut partitions = Vec::<Vec<RbxRef>>::new();
    let mut members = Vec::new();
    let mut cursor = 0;
    while cursor < preorder.len() {
        let size = subtree_sizes[&preorder[cursor]];
        let take = if limit == 0 || size <= limit { size } else { 1 };
        if limit > 0 && !members.is_empty() && members.len() + take > limit {
            partitions.push(std::mem::take(&mut members));
        }
        members.extend_from_slice(&preorder[cursor..cursor + take]);
        cursor += take;
    }
    if !members.is_empty() {
        partitions.push(members);
    }
    stages.next("resolve repeated ancestors and cross-batch references");
    let primary = partitions
        .iter()
        .enumerate()
        .flat_map(|(index, members)| members.iter().map(move |referent| (*referent, index)))
        .collect::<AHashMap<_, _>>();
    let trace_context = crate::app::timing::trace_context();
    let batch_references = partitions
        .par_iter_mut()
        .enumerate()
        .map(|(index, members)| {
            let _context = crate::app::timing::enter_trace_context(trace_context);
            let _trace = crate::app::timing::trace_scope(
                "native.encode.worker",
                "resolve batch ancestors and references",
            );
            let mut repeated = Vec::new();
            let mut selected = members.iter().copied().collect::<AHashSet<_>>();
            // A repeated ancestor is only an identity anchor. Its properties load
            // once, in its first batch; children never duplicate their ancestors.
            for referent in members.clone() {
                let mut parent = dom.get_by_ref(referent).unwrap().parent();
                while parent != dom.root_ref() && selected.insert(parent) {
                    members.push(parent);
                    repeated.push(parent);
                    parent = dom
                        .get_by_ref(parent)
                        .context("Native batch ancestor disappeared")?
                        .parent();
                }
            }
            members.sort_unstable_by_key(|referent| postorder[referent]);
            let mut crossing = Vec::new();
            for referent in members.iter() {
                if primary[referent] != index {
                    continue;
                }
                let instance = dom
                    .get_by_ref(*referent)
                    .context("Native import member disappeared")?;
                for (name, value) in &instance.properties {
                    if rbx_variant_referent(value).is_some_and(|target| {
                        primary.contains_key(&target) && !selected.contains(&target)
                    }) {
                        crossing.push((*referent, *name));
                    }
                }
            }
            Ok((repeated, crossing))
        })
        .collect::<Result<Vec<_>>>()?;
    // Every worker reads the same DOM. Remove cross-batch properties only after
    // all selections are complete, then encode against one shared schema.
    let mut repeated = HashSet::new();
    for (anchors, crossing) in batch_references {
        repeated.extend(anchors);
        for (referent, name) in crossing {
            let (segments, ordinals) = rbx_dom_instance_path_parts(dom, referent);
            let class = dom.get_by_ref(referent).unwrap().class.as_str();
            let logical =
                rbx_logical_property_name(database, class, name.as_str()).unwrap_or(name.as_str());
            post_apply
                .entry(instance_path_parts_key(&segments, &ordinals))
                .or_default()
                .insert(logical.to_string());
            dom.get_by_ref_mut(referent)
                .unwrap()
                .properties
                .remove(&name);
        }
    }
    // A property present in only one batch must still emit its default in the
    // others. Studio constructor defaults can differ from serialized defaults.
    stages.next("collect shared binary property schema");
    // Merge class schemas, not all instance property maps, on the coordinator.
    // Workers retain references into the same immutable DOM.
    let schemas = preorder
        .par_chunks(4096)
        .map(|members| {
            let mut schema =
                HashMap::<rbx_dom_weak::Ustr, HashMap<rbx_dom_weak::Ustr, &RbxVariant>>::new();
            for referent in members {
                if requested.get(referent) == Some(&InstanceBindingMode::ReferenceOnly) {
                    continue;
                }
                let instance = dom.get_by_ref(*referent).unwrap();
                let properties = schema.entry(instance.class).or_default();
                for (name, value) in &instance.properties {
                    properties.entry(*name).or_insert(value);
                }
            }
            schema
        })
        .collect::<Vec<_>>();
    let mut schema = HashMap::<rbx_dom_weak::Ustr, HashMap<rbx_dom_weak::Ustr, &RbxVariant>>::new();
    for chunk in schemas {
        for (class, properties) in chunk {
            let target = schema.entry(class).or_default();
            for (name, value) in properties {
                target.entry(name).or_insert(value);
            }
        }
    }
    stages.next("join parallel binary batch encoding");
    let trace_context = crate::app::timing::trace_context();
    let encoded = partitions
        .par_iter()
        .enumerate()
        .map(|(index, members)| {
            let _context =
                trace_context.map(|context| crate::app::timing::enter_trace_context(Some(context)));
            let mut batch = crate::app::timing::trace_stages(
                "native.encode.worker",
                "prepare batch binding modes",
            );
            let bindings = members
                .iter()
                .filter_map(|referent| {
                    let mode = if primary[referent] != index {
                        Some(InstanceBindingMode::ReferenceOnly)
                    } else {
                        requested.get(referent).copied().or_else(|| {
                            repeated
                                .contains(referent)
                                .then_some(InstanceBindingMode::Replace)
                        })
                    };
                    mode.map(|mode| (*referent, mode))
                })
                .collect::<HashMap<_, _>>();
            let mut bytes = Vec::new();
            batch.next("serialize and compress instance columns");
            let receipts = rbx_binary::Serializer::new()
                .serialize_selection_with_bindings(&mut bytes, dom, members, &bindings, &schema)?;
            batch.next("collect class counts and retained identity receipts");
            let receipts = receipts
                .into_iter()
                .filter(|receipt| {
                    !database.classes[receipt.class_name.as_str()]
                        .tags
                        .contains(&rbx_reflection::ClassTag::Service)
                })
                .collect::<Vec<_>>();
            let mut counts = std::collections::BTreeMap::<String, u32>::new();
            for referent in members {
                let instance = dom.get_by_ref(*referent).unwrap();
                let class = database
                    .classes
                    .get(instance.class.as_str())
                    .context("Native reader class is absent from reflection metadata")?;
                if !class.tags.contains(&rbx_reflection::ClassTag::Service) {
                    *counts.entry(instance.class.to_string()).or_default() += 1;
                }
            }
            let classes = counts
                .into_iter()
                .map(|(name, count)| EditorNativeClass {
                    name,
                    count,
                    tags_absent: false,
                })
                .collect::<Vec<_>>();
            let services = service_roots
                .iter()
                .filter(|(_, root)| members.contains(root))
                .map(|(name, _)| name.clone())
                .collect();
            Ok((bytes, receipts, classes, services))
        })
        .collect::<Result<Vec<_>>>()?;
    stages.next("merge batch class ordinals and concatenate payloads");
    let mut counts = std::collections::BTreeMap::<String, u32>::new();
    for (_, _, classes, _) in &encoded {
        for class in classes {
            *counts.entry(class.name.clone()).or_default() += class.count;
        }
    }
    let classes = counts
        .into_iter()
        .map(|(name, count)| {
            // The shared schema includes every payload instance, not just a
            // representative. Absence therefore proves an empty initial tag set.
            let tags_absent = schema
                .get(&rbx_dom_weak::Ustr::from(name.as_str()))
                .is_none_or(|properties| {
                    !properties.contains_key(&rbx_dom_weak::Ustr::from("Tags"))
                });
            EditorNativeClass {
                name,
                count,
                tags_absent,
            }
        })
        .collect::<Vec<_>>();
    let class_indices = classes
        .iter()
        .enumerate()
        .map(|(index, class)| (class.name.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut offsets = vec![0u32; classes.len()];
    let mut bytes = Vec::with_capacity(encoded.iter().map(|(bytes, _, _, _)| bytes.len()).sum());
    let mut batches = Vec::with_capacity(encoded.len());
    let mut bindings = Vec::new();
    let mut aliases = Vec::new();
    let mut first_ordinals = HashMap::new();
    let mut referent_offset = 0;
    for (payload, receipts, batch_classes, services) in encoded {
        let start = bytes.len();
        bytes.extend_from_slice(&payload);
        for receipt in receipts {
            let index = class_indices[receipt.class_name.as_str()];
            let ordinal = receipt.ordinal + offsets[index];
            if let Some(source_ordinal) = first_ordinals.get(&receipt.referent) {
                aliases.push(EditorNativeAlias {
                    class_index: index as u32,
                    ordinal,
                    source_ordinal: *source_ordinal,
                });
            } else {
                first_ordinals.insert(receipt.referent, ordinal);
                if let Some(mode) = requested.get(&receipt.referent) {
                    let (path_segments, path_ordinals) =
                        rbx_dom_instance_path_parts(dom, receipt.referent);
                    bindings.push(EditorNativeBinding {
                        path_segments,
                        path_ordinals,
                        class_name: receipt.class_name,
                        binary_referent: receipt.binary_referent + referent_offset,
                        ordinal,
                        class_count: classes[index].count,
                        reference_only: *mode == InstanceBindingMode::ReferenceOnly,
                    });
                }
            }
        }
        for class in &batch_classes {
            offsets[class_indices[class.name.as_str()]] += class.count;
        }
        let instance_count = i32::try_from(u32::from_le_bytes(payload[20..24].try_into()?))?;
        batches.push(EditorNativeBatch {
            bytes: start..bytes.len(),
            services,
        });
        referent_offset += instance_count;
    }
    stages.next("assemble plugin target metadata and release encoding scratch data");
    let groups = groups
        .into_iter()
        .zip(package_plans)
        .map(|(group, plan)| {
            anyhow::ensure!(
                plan.retained_roots.is_empty()
                    && plan.package_roots.is_empty()
                    && plan.mutation_package_roots.is_empty(),
                "Linked-package replacement requires its existing retention pipeline"
            );
            let root_paths = group
                .roots
                .iter()
                .map(|root| {
                    let (path_segments, path_ordinals) = rbx_dom_instance_path_parts(dom, *root);
                    EditorBinaryRootPath {
                        path_segments,
                        path_ordinals,
                    }
                })
                .collect();
            Ok(EditorBinaryImportGroup {
                service: group.service,
                additive: group.additive,
                target_path: group.target_path,
                count: group.roots.len(),
                payload_root_name: String::new(),
                expected_structure: None,
                root_paths,
                viewport_camera: group.viewport_camera,
                retained_roots: plan.retained_roots,
                package_roots: plan.package_roots,
                mutation_package_roots: plan.mutation_package_roots,
                change_generation: plan.change_generation,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((
        bytes,
        groups,
        EditorNativeReplacement {
            plan: EditorNativePlan {
                bindings,
                classes,
                aliases,
            },
            batches,
        },
    ))
}

fn filter_retained_properties(
    dom: &mut RbxWeakDom,
    requested: &HashMap<RbxRef, InstanceBindingMode>,
    post_apply: &mut HashMap<String, HashSet<String>>,
    database: &ReflectionDatabase<'_>,
) -> Result<()> {
    for (referent, mode) in requested {
        if *mode != InstanceBindingMode::Properties {
            continue;
        }
        let (segments, ordinals) = rbx_dom_instance_path_parts(dom, *referent);
        let instance = dom
            .get_by_ref(*referent)
            .context("Native container disappeared")?;
        let class = instance.class.as_str().to_string();
        let service = &segments[0];
        let mut removed = Vec::new();
        let mut post_applied = Vec::new();
        for (name, value) in &instance.properties {
            let name = name.as_str();
            if matches!(
                name,
                "Attributes" | "AttributesSerialize" | "Tags" | "NeedsPivotMigration"
            ) {
                continue;
            }
            let logical = rbx_logical_property_name(database, &class, name).unwrap_or(name);
            if rbx_variant_referent(value).is_some() {
                removed.push(name.to_string());
                if !(class == "Workspace" && logical == "CurrentCamera") {
                    post_applied.push(logical.to_string());
                }
            } else if is_externally_managed_editor_property(service, &class, &segments, logical)
                || crate::editor::native_roots::is_property(&class, logical)
                || is_engine_managed_editor_property(&class, logical, database)
            {
                removed.push(name.to_string());
            }
        }
        if !post_applied.is_empty() {
            post_apply
                .entry(instance_path_parts_key(&segments, &ordinals))
                .or_default()
                .extend(post_applied);
        }
        let instance = dom.get_by_ref_mut(*referent).unwrap();
        for name in removed {
            instance
                .properties
                .remove(&rbx_dom_weak::Ustr::from(name.as_str()));
        }
    }
    Ok(())
}

fn remove_excluded_children(
    dom: &mut RbxWeakDom,
    requested: &HashMap<RbxRef, InstanceBindingMode>,
    included: &HashSet<RbxRef>,
) -> Result<()> {
    // The preflight can remove occupied additive roots from its plan. Do not
    // serialize those stale source siblings merely because the service is kept.
    for container in requested.keys().copied().collect::<Vec<_>>() {
        if dom
            .get_by_ref(container)
            .is_some_and(|node| node.class.as_str() == "Camera")
        {
            continue;
        }
        let children = dom
            .get_by_ref(container)
            .context("Native container disappeared")?
            .children()
            .to_vec();
        for child in children {
            if !included.contains(&child) {
                dom.destroy(child);
            }
        }
    }
    Ok(())
}
