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

#[cfg(test)]
#[path = "import_memory_tests.rs"]
mod memory_tests;

// Each engine read finalizes the loaded scene. Hundreds of tiny reads repeat
// that work over an ever larger place; retain bounded chunks without doing a
// whole engine load for every small subtree.
pub(super) const NATIVE_IMPORT_BATCH_INSTANCES: usize = 32_768;

// Fixed-width tree rows avoid repeating full paths for every descendant.
// Each row is (parent row, sibling/root ordinal, name, class, settings id).
// Parent rows and ordinals are one-based; strings are zero-based. Parent zero
// selects a root through incomingByPayloadIndex. Retained stubs carry no tree.
pub(super) fn instance_identities(
    dom: &RbxWeakDom,
    roots: &[RbxRef],
    ids: &HashMap<RbxRef, String>,
    omitted: &HashSet<usize>,
) -> Result<Option<crate::editor::types::EditorBinaryStructure>> {
    if ids.is_empty() {
        return Ok(None);
    }
    let tree_profile = crate::app::profile::span("native import identity tree rows");
    let mut strings = Vec::with_capacity(roots.len().saturating_mul(3).max(64));
    let mut indices = AHashMap::with_capacity(roots.len().saturating_mul(3).max(64));
    let mut nodes = Vec::with_capacity(roots.len().saturating_mul(20));
    let mut ordinals: AHashMap<&str, usize> = AHashMap::new();
    let mut children: Vec<(RbxRef, u32, usize)> = Vec::new();
    let mut pending = roots
        .iter()
        .enumerate()
        .filter(|(index, _)| !omitted.contains(&(index + 1)))
        .rev()
        .map(|(index, root)| (*root, 0u32, index + 1))
        .collect::<Vec<_>>();
    while let Some((referent, parent, ordinal)) = pending.pop() {
        let instance = dom
            .get_by_ref(referent)
            .context("Import identity lost an instance")?;
        let row = u32::try_from(nodes.len() / 20 + 1)?;
        nodes.extend_from_slice(&parent.to_le_bytes());
        nodes.extend_from_slice(&u32::try_from(ordinal)?.to_le_bytes());
        for value in [
            instance.name.as_str(),
            instance.class.as_str(),
            ids.get(&referent).map_or("", String::as_str),
        ] {
            let index = *indices.entry(value).or_insert_with(|| {
                let index = strings.len() as u32;
                strings.push(value.to_owned());
                index
            });
            nodes.extend_from_slice(&index.to_le_bytes());
        }
        let child_refs = instance.children();
        if !child_refs.is_empty() {
            ordinals.clear();
            children.clear();
            for child in child_refs {
                let node = dom
                    .get_by_ref(*child)
                    .context("Import identity lost a child")?;
                let ordinal = ordinals.entry(node.name.as_str()).or_insert(0usize);
                *ordinal += 1;
                children.push((*child, row, *ordinal));
            }
            pending.extend(children.drain(..).rev());
        }
    }
    drop(tree_profile);
    let _encode = crate::app::profile::span("native import identity base64 encoding");
    Ok(
        (!nodes.is_empty()).then(|| crate::editor::types::EditorBinaryStructure {
            strings,
            nodes: base64::encode(nodes),
        }),
    )
}

#[test]
fn import_identity_tree_preserves_duplicate_ordinals_and_omits_retained_payloads() -> Result<()> {
    let mut dom = RbxWeakDom::new(RbxInstanceBuilder::new("DataModel"));
    let retained = dom.insert(
        dom.root_ref(),
        RbxInstanceBuilder::new("Folder").with_name("Retained"),
    );
    let root = dom.insert(
        dom.root_ref(),
        RbxInstanceBuilder::new("Folder").with_name("TreeRoot"),
    );
    let child = dom.insert(
        root,
        RbxInstanceBuilder::new("Folder").with_name("TreeChild"),
    );
    let duplicate = dom.insert(root, RbxInstanceBuilder::new("Part").with_name("TreeChild"));
    let leaf = dom.insert(
        child,
        RbxInstanceBuilder::new("ObjectValue").with_name("Leaf"),
    );
    let ids = [
        (root, "editor:6c72"),
        (child, "editor:6c73"),
        (duplicate, "editor:6c74"),
        (leaf, "editor:6c75"),
    ]
    .into_iter()
    .map(|(node, id)| (node, id.to_owned()))
    .collect();
    let receipt = instance_identities(&dom, &[retained, root], &ids, &HashSet::from([1]))?.unwrap();
    let rows = base64::decode(&receipt.nodes)?;
    let values = rows
        .chunks_exact(20)
        .map(|row| {
            let word =
                |offset| u32::from_le_bytes(row[offset..offset + 4].try_into().unwrap()) as usize;
            (
                word(0),
                word(4),
                receipt.strings[word(8)].as_str(),
                receipt.strings[word(12)].as_str(),
                receipt.strings[word(16)].as_str(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        values,
        vec![
            (0, 2, "TreeRoot", "Folder", "editor:6c72"),
            (1, 1, "TreeChild", "Folder", "editor:6c73"),
            (2, 1, "Leaf", "ObjectValue", "editor:6c75"),
            (1, 2, "TreeChild", "Part", "editor:6c74"),
        ]
    );
    assert!(instance_identities(&dom, &[root], &HashMap::new(), &HashSet::new())?.is_none());
    Ok(())
}

#[test]
fn import_identity_metadata_is_linear_for_deep_place_sized_trees() -> Result<()> {
    let mut dom = RbxWeakDom::new(RbxInstanceBuilder::new("Folder").with_name("Node"));
    let mut ids = HashMap::new();
    let mut parent = dom.root_ref();
    for index in 0..20_000 {
        ids.insert(parent, format!("editor:{index:x}"));
        if index != 19_999 {
            parent = dom.insert(parent, RbxInstanceBuilder::new("Folder").with_name("Node"));
        }
    }
    let receipt = instance_identities(&dom, &[dom.root_ref()], &ids, &HashSet::new())?.unwrap();
    assert_eq!(base64::decode(&receipt.nodes)?.len(), 20 * 20_000);
    assert_eq!(receipt.strings.len(), 20_002);
    assert!(serde_json::to_vec(&receipt)?.len() < 900_000);
    Ok(())
}

#[test]
fn native_import_paths_preserve_ordinals_in_wide_mixed_class_siblings() -> Result<()> {
    let mut dom = RbxWeakDom::new(RbxInstanceBuilder::new("DataModel"));
    let service = dom.insert(
        dom.root_ref(),
        RbxInstanceBuilder::new("Workspace").with_name("Renamed service"),
    );
    let count = 4096;
    for index in 0..count {
        dom.insert(
            service,
            RbxInstanceBuilder::new(if index % 2 == 0 { "Folder" } else { "Part" })
                .with_name(if index % 3 == 0 { "Other" } else { "Duplicate" }),
        );
    }
    let services = vec![("Workspace".to_string(), service)];
    let groups = pending_editor_binary_groups(&dom, &services)?;
    let plan = EditorPackageGroupPlan {
        retained_roots: Vec::new(),
        package_roots: Vec::new(),
        mutation_package_roots: Vec::new(),
        change_generation: None,
    };
    let (_, groups, _) = encode_services(
        &mut dom,
        &services,
        &HashMap::new(),
        groups,
        vec![plan],
        &mut HashMap::new(),
    )?;
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].root_paths.len(), count);
    let mut duplicate = 0;
    let mut other = 0;
    for (index, path) in groups[0].root_paths.iter().enumerate() {
        let (name, ordinal) = if index % 3 == 0 {
            other += 1;
            ("Other", other)
        } else {
            duplicate += 1;
            ("Duplicate", duplicate)
        };
        assert_eq!(path.path_segments, ["Workspace", name]);
        assert_eq!(path.path_ordinals, [1, ordinal]);
    }
    Ok(())
}

#[test]
fn native_import_parallel_metadata_preserves_service_order_and_identity_scopes() -> Result<()> {
    let mut dom = RbxWeakDom::new(RbxInstanceBuilder::new("DataModel"));
    let names = ["ServerStorage", "Workspace", "ReplicatedStorage"];
    let mut ids = HashMap::new();
    let services = names
        .iter()
        .map(|name| {
            let service = dom.insert(dom.root_ref(), RbxInstanceBuilder::new(*name));
            let root = dom.insert(service, RbxInstanceBuilder::new("Folder").with_name("Root"));
            ids.insert(root, format!("editor:{name}"));
            (name.to_string(), service)
        })
        .collect::<Vec<_>>();
    let groups = pending_editor_binary_groups(&dom, &services)?;
    let plans = names
        .iter()
        .map(|_| EditorPackageGroupPlan {
            retained_roots: Vec::new(),
            package_roots: Vec::new(),
            mutation_package_roots: Vec::new(),
            change_generation: None,
        })
        .collect();
    let (_, groups, _) = encode_services(
        &mut dom,
        &services,
        &ids,
        groups,
        plans,
        &mut HashMap::new(),
    )?;
    assert_eq!(
        groups
            .iter()
            .map(|group| group.service.as_str())
            .collect::<Vec<_>>(),
        names
    );
    for group in groups {
        assert_eq!(
            group.root_paths[0].path_segments,
            [group.service.as_str(), "Root"]
        );
        let identities = group.instance_identities.unwrap();
        assert_eq!(base64::decode(&identities.nodes)?.len(), 20);
        for name in names {
            assert_eq!(
                identities.strings.contains(&format!("editor:{name}")),
                group.service == name
            );
        }
    }
    Ok(())
}

#[test]
fn import_identity_tree_covers_a_subtree_across_native_batches() -> Result<()> {
    let mut dom = RbxWeakDom::new(RbxInstanceBuilder::new("DataModel"));
    let service = dom.insert(dom.root_ref(), RbxInstanceBuilder::new("Workspace"));
    let root = dom.insert(service, RbxInstanceBuilder::new("Folder").with_name("Bulk"));
    assert_eq!(NATIVE_IMPORT_BATCH_INSTANCES, 32_768);
    let count = NATIVE_IMPORT_BATCH_INSTANCES + 100;
    let names = (0..count)
        .map(|index| format!("Part{index:05}{}", "x".repeat(91)))
        .collect::<Vec<_>>();
    let mut ids = HashMap::from([(root, "editor:bulk".to_string())]);
    for (index, name) in names.iter().enumerate() {
        let part = dom.insert(root, RbxInstanceBuilder::new("Part").with_name(name));
        ids.insert(part, format!("editor:part{index}"));
    }
    let services = vec![("Workspace".to_string(), service)];
    let groups = pending_editor_binary_groups(&dom, &services)?;
    let plans = vec![EditorPackageGroupPlan {
        retained_roots: Vec::new(),
        package_roots: Vec::new(),
        mutation_package_roots: Vec::new(),
        change_generation: None,
    }];
    let (bytes, groups, replacement) = encode_services(
        &mut dom,
        &services,
        &ids,
        groups,
        plans,
        &mut HashMap::new(),
    )?;
    assert!(replacement.batches.len() > 1);
    assert_eq!(
        replacement.plan.aliases.len(),
        replacement.batches.len() - 1
    );
    for alias in &replacement.plan.aliases {
        assert_eq!(
            replacement.plan.classes[alias.class_index as usize].name,
            "Folder"
        );
        assert_eq!(alias.source_ordinal, 0);
    }
    let serialized_count = replacement
        .batches
        .iter()
        .map(|batch| {
            u32::from_le_bytes(
                bytes[batch.bytes.start + 20..batch.bytes.start + 24]
                    .try_into()
                    .unwrap(),
            ) as usize
        })
        .sum::<usize>();
    // Each batch references Workspace and (after the first) the same Bulk.
    assert_eq!(
        serialized_count,
        ids.len() + replacement.batches.len() * 2 - 1
    );
    assert_eq!(
        replacement
            .plan
            .classes
            .iter()
            .find(|class| class.name == "Part")
            .unwrap()
            .count as usize,
        count
    );
    assert_eq!(groups.len(), 1);
    let identities = groups[0].instance_identities.as_ref().unwrap();
    let rows = base64::decode(&identities.nodes)?;
    assert_eq!(rows.len(), ids.len() * 20);
    for (index, row) in rows.chunks_exact(20).enumerate() {
        let word =
            |offset| u32::from_le_bytes(row[offset..offset + 4].try_into().unwrap()) as usize;
        let (name, class, id) = if index == 0 {
            ("Bulk", "Folder", "editor:bulk".to_string())
        } else {
            (
                names[index - 1].as_str(),
                "Part",
                format!("editor:part{}", index - 1),
            )
        };
        assert_eq!(word(0), usize::from(index != 0));
        assert_eq!(word(4), 1);
        assert_eq!(identities.strings[word(8)], name);
        assert_eq!(identities.strings[word(12)], class);
        assert_eq!(identities.strings[word(16)], id);
    }
    Ok(())
}

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

fn record_cross_batch_properties(
    dom: &RbxWeakDom,
    fields: impl Iterator<Item = (RbxRef, rbx_dom_weak::Ustr)>,
    post_apply: &mut HashMap<String, HashSet<String>>,
    database: &ReflectionDatabase<'_>,
) {
    let mut paths = crate::rbx::model::RbxDomPaths::new(dom);
    for (referent, name) in fields {
        let (segments, ordinals) = paths.parts(referent);
        let class = dom.get_by_ref(referent).unwrap().class.as_str();
        let logical =
            rbx_logical_property_name(database, class, name.as_str()).unwrap_or(name.as_str());
        post_apply
            .entry(instance_path_parts_key(&segments, &ordinals))
            .or_default()
            .insert(logical.to_string());
    }
}

fn collect_requested_bindings(
    dom: &RbxWeakDom,
    service_roots: &[(String, RbxRef)],
    groups: &[PendingEditorBinaryGroup],
) -> Result<(HashMap<RbxRef, InstanceBindingMode>, HashSet<RbxRef>)> {
    let mut requested = HashMap::new();
    let mut included = service_roots
        .iter()
        .map(|(_, root)| *root)
        .collect::<HashSet<_>>();
    for (_, root) in service_roots {
        requested.insert(*root, InstanceBindingMode::ReferenceOnly);
    }
    for group in groups {
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
    Ok((requested, included))
}

pub(super) fn encode_services(
    dom: &mut RbxWeakDom,
    service_roots: &[(String, RbxRef)],
    settings_ids: &HashMap<RbxRef, String>,
    groups: Vec<PendingEditorBinaryGroup>,
    package_plans: Vec<EditorPackageGroupPlan>,
    post_apply: &mut HashMap<String, HashSet<String>>,
) -> Result<(
    Vec<u8>,
    Vec<EditorBinaryImportGroup>,
    EditorNativeReplacement,
)> {
    let trim_profile =
        crate::app::profile::span("native encode retained bindings and excluded children");
    anyhow::ensure!(
        groups.len() == package_plans.len(),
        "Native import plans differ"
    );
    let (requested, included) = collect_requested_bindings(dom, service_roots, &groups)?;
    remove_excluded_children(dom, &requested, &included)?;
    let database = rbx_reflection_database::get()?;
    filter_retained_properties(dom, &requested, post_apply, database)?;
    drop(trim_profile);
    let tree_profile = crate::app::profile::span("native encode subtree sizes and traversal order");
    let mut preorder = Vec::new();
    let mut postorder_nodes = Vec::new();
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
                postorder_nodes.push(referent);
            } else {
                preorder.push(referent);
                stack.push((referent, true));
                stack.extend(node.children().iter().rev().map(|child| (*child, false)));
            }
        }
    }
    drop(tree_profile);
    let limit = NATIVE_IMPORT_BATCH_INSTANCES;
    let single_batch = preorder.len() <= limit;
    let (partitions, primary, repeated, schema) = if single_batch {
        let partitions = if postorder_nodes.is_empty() {
            Vec::new()
        } else {
            vec![postorder_nodes]
        };
        (
            partitions,
            AHashMap::<RbxRef, usize>::new(),
            HashSet::<RbxRef>::new(),
            HashMap::<rbx_dom_weak::Ustr, HashMap<rbx_dom_weak::Ustr, &RbxVariant>>::new(),
        )
    } else {
        for (rank, referent) in postorder_nodes.into_iter().enumerate() {
            postorder.insert(referent, rank);
        }
        let partition_profile =
            crate::app::profile::span("native encode partitions and primary ownership");
        let mut partitions = Vec::<Vec<RbxRef>>::new();
        let mut members = Vec::new();
        let mut cursor = 0;
        while cursor < preorder.len() {
            let size = subtree_sizes[&preorder[cursor]];
            let take = if size <= limit { size } else { 1 };
            if !members.is_empty() && members.len() + take > limit {
                partitions.push(std::mem::take(&mut members));
            }
            members.extend_from_slice(&preorder[cursor..cursor + take]);
            cursor += take;
        }
        if !members.is_empty() {
            partitions.push(members);
        }
        let primary = partitions
            .iter()
            .enumerate()
            .flat_map(|(index, members)| members.iter().map(move |referent| (*referent, index)))
            .collect::<AHashMap<_, _>>();
        drop(partition_profile);
        let reference_profile =
            crate::app::profile::span("native encode ancestor anchors and crossing references");
        let batch_references = partitions
            .par_iter_mut()
            .enumerate()
            .map(|(index, members)| {
                let mut repeated = Vec::new();
                let mut selected = members.iter().copied().collect::<AHashSet<_>>();
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
                let crossing = if members.len() >= 8192 {
                    members
                        .par_chunks(4096)
                        .map(|chunk| {
                            let mut found = Vec::new();
                            for referent in chunk {
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
                                        found.push((*referent, *name));
                                    }
                                }
                            }
                            Ok(found)
                        })
                        .collect::<Result<Vec<_>>>()?
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                } else {
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
                    crossing
                };
                Ok((repeated, crossing))
            })
            .collect::<Result<Vec<_>>>()?;
        record_cross_batch_properties(
            dom,
            batch_references
                .iter()
                .flat_map(|(_, crossing)| crossing.iter().copied()),
            post_apply,
            database,
        );
        let mut repeated = HashSet::new();
        for (anchors, crossing) in batch_references {
            repeated.extend(anchors);
            for (referent, name) in crossing {
                dom.get_by_ref_mut(referent)
                    .unwrap()
                    .properties
                    .remove(&name);
            }
        }
        drop(reference_profile);
        let schema_profile = crate::app::profile::span("native encode shared class schema");
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
        let mut schema =
            HashMap::<rbx_dom_weak::Ustr, HashMap<rbx_dom_weak::Ustr, &RbxVariant>>::new();
        for chunk in schemas {
            for (class, properties) in chunk {
                let target = schema.entry(class).or_default();
                for (name, value) in properties {
                    target.entry(name).or_insert(value);
                }
            }
        }
        drop(schema_profile);
        (partitions, primary, repeated, schema)
    };
    let workers_profile = crate::app::profile::span("native encode parallel batches");
    let encoded = partitions
        .par_iter()
        .enumerate()
        .map(|(index, members)| {
            let _batch = crate::app::profile::span(&format!(
                "native encode batch {index}: {} instances",
                members.len()
            ));
            let bindings_profile = crate::app::profile::span(&format!(
                "native encode batch {index}: binding selection"
            ));
            let bindings = if single_batch {
                requested.clone()
            } else {
                members
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
                    .collect::<HashMap<_, _>>()
            };
            drop(bindings_profile);
            let serialize_profile = crate::app::profile::span(&format!(
                "native encode batch {index}: binary serialization"
            ));
            let mut bytes = Vec::new();
            let receipts = rbx_binary::Serializer::new()
                .serialize_selection_with_bindings(&mut bytes, dom, members, &bindings, &schema)?;
            drop(serialize_profile);
            let _receipt_profile = crate::app::profile::span(&format!(
                "native encode batch {index}: receipt and class counts ({} bytes)",
                bytes.len()
            ));
            let receipts = receipts
                .into_iter()
                .filter(|receipt| {
                    !database.classes[receipt.class_name.as_str()]
                        .tags
                        .contains(&rbx_reflection::ClassTag::Service)
                })
                .collect::<Vec<_>>();
            let tags_key = rbx_dom_weak::Ustr::from("Tags");
            let mut counts = AHashMap::<rbx_dom_weak::Ustr, (u32, bool)>::new();
            for referent in members {
                let instance = dom.get_by_ref(*referent).unwrap();
                let entry = counts.entry(instance.class).or_insert((0, false));
                entry.0 += 1;
                if single_batch
                    && !entry.1
                    && requested.get(referent) != Some(&InstanceBindingMode::ReferenceOnly)
                    && instance.properties.contains_key(&tags_key)
                {
                    entry.1 = true;
                }
            }
            let mut classes = Vec::with_capacity(counts.len());
            for (class_ustr, (count, has_tags)) in counts {
                let class = database
                    .classes
                    .get(class_ustr.as_str())
                    .context("Native reader class is absent from reflection metadata")?;
                if !class.tags.contains(&rbx_reflection::ClassTag::Service) {
                    classes.push(EditorNativeClass {
                        name: class_ustr.to_string(),
                        count,
                        tags_absent: !has_tags,
                    });
                }
            }
            classes.sort_unstable_by(|a, b| a.name.cmp(&b.name));
            Ok((bytes, receipts, classes))
        })
        .collect::<Result<Vec<_>>>()?;
    drop(workers_profile);
    let _combine =
        crate::app::profile::span("native encode join payloads, identities and receipt plans");
    let combine_classes = crate::app::profile::span("native encode merge class counts");
    let mut counts = std::collections::BTreeMap::<String, (u32, bool)>::new();
    for (_, _, classes) in &encoded {
        for class in classes {
            let entry = counts.entry(class.name.clone()).or_insert((0, true));
            entry.0 += class.count;
            entry.1 &= class.tags_absent;
        }
    }
    let classes = counts
        .into_iter()
        .map(|(name, (count, batch_tags_absent))| {
            let tags_absent = if single_batch {
                batch_tags_absent
            } else {
                schema
                    .get(&rbx_dom_weak::Ustr::from(name.as_str()))
                    .is_none_or(|properties| {
                        !properties.contains_key(&rbx_dom_weak::Ustr::from("Tags"))
                    })
            };
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
    let mut bytes = Vec::with_capacity(encoded.iter().map(|(bytes, _, _)| bytes.len()).sum());
    let mut batches = Vec::with_capacity(encoded.len());
    let mut bindings = Vec::new();
    let mut aliases = Vec::new();
    let mut first_ordinals = HashMap::new();
    let mut referent_offset = 0;
    // All hierarchy edits are complete. Index siblings once while preparing
    // paths; rescanning them for every imported root is quadratic for wide trees.
    let mut paths = crate::rbx::model::RbxDomPaths::new(dom);
    drop(combine_classes);
    let combine_payloads =
        crate::app::profile::span("native encode concatenate payloads and bind receipts");
    for (payload, receipts, batch_classes) in encoded {
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
                // Only repeated ancestor anchors can occur in another batch.
                // Do not allocate a lookup entry for every created instance.
                if repeated.contains(&receipt.referent) {
                    first_ordinals.insert(receipt.referent, ordinal);
                }
                if let Some(mode) = requested.get(&receipt.referent) {
                    let (path_segments, path_ordinals) = paths.parts(receipt.referent);
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
        });
        referent_offset += instance_count;
    }
    drop(combine_payloads);
    let _groups = crate::app::profile::span("native encode group paths and identity rows");
    let groups = groups
        .into_par_iter()
        .zip(package_plans)
        .map(|(group, plan)| {
            let mut paths = crate::rbx::model::RbxDomPaths::new(dom);
            let _service = crate::app::profile::span(&format!(
                "{}: native import paths and identities ({} roots)",
                group.service,
                group.roots.len()
            ));
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
                    let (path_segments, path_ordinals) = paths.parts(*root);
                    EditorBinaryRootPath {
                        path_segments,
                        path_ordinals,
                    }
                })
                .collect();
            Ok(EditorBinaryImportGroup {
                instance_identities: instance_identities(
                    dom,
                    &group.roots,
                    settings_ids,
                    &HashSet::new(),
                )?,
                service: group.service,
                additive: group.additive,
                target_path: group.target_path,
                count: group.roots.len(),
                payload_root_name: String::new(),
                expected_structure: None,
                root_paths,
                root_settings_ids: group.root_settings_ids,
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
