use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct StructuralPart {
    pub(crate) name: String,
    pub(crate) class_name: String,
    pub(crate) ordinal: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PairingPart {
    pub(crate) name: String,
    pub(crate) ordinal: usize,
}

pub(crate) type PathId = usize;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PathNode<P> {
    pub(crate) parent: Option<PathId>,
    pub(crate) part: P,
}

pub(crate) struct PathInterner<P> {
    pub(crate) nodes: Vec<PathNode<P>>,
    pub(crate) ids: HashMap<PathNode<P>, PathId>,
}

impl<P> PathInterner<P>
where
    P: Clone + Eq + std::hash::Hash,
{
    pub(crate) fn new() -> Self {
        Self {
            nodes: Vec::new(),
            ids: HashMap::new(),
        }
    }

    pub(crate) fn intern(&mut self, parent: Option<PathId>, part: P) -> PathId {
        let node = PathNode { parent, part };
        if let Some(id) = self.ids.get(&node) {
            return *id;
        }
        let id = self.nodes.len();
        self.nodes.push(node.clone());
        self.ids.insert(node, id);
        id
    }

    pub(crate) fn find(&self, parent: Option<PathId>, part: &P) -> Option<PathId> {
        self.ids
            .get(&PathNode {
                parent,
                part: part.clone(),
            })
            .copied()
    }

    pub(crate) fn node(&self, id: PathId) -> &PathNode<P> {
        &self.nodes[id]
    }
}

pub(crate) fn intern_paths<P>(
    document: &SettingsBytecode,
    ordinals: &[usize],
    interner: &mut PathInterner<P>,
    make_part: impl Fn(&SettingsBytecodeInstance, usize) -> P,
) -> Vec<PathId>
where
    P: Clone + Eq + std::hash::Hash,
{
    let mut ids = vec![None; document.instances.len()];
    let mut chain = Vec::new();
    for start in 0..document.instances.len() {
        if ids[start].is_some() {
            continue;
        }
        chain.clear();
        let mut current = start;
        let mut parent_id = loop {
            if let Some(id) = ids[current] {
                break Some(id);
            }
            chain.push(current);
            let Some(parent) = document.instances[current].parent_index else {
                break None;
            };
            current = parent;
        };
        while let Some(index) = chain.pop() {
            let id = interner.intern(
                parent_id,
                make_part(&document.instances[index], ordinals[index]),
            );
            ids[index] = Some(id);
            parent_id = Some(id);
        }
    }
    ids.into_iter().map(Option::unwrap).collect()
}

pub(crate) fn structural_path_ids(
    document: &SettingsBytecode,
    interner: &mut PathInterner<StructuralPart>,
) -> Vec<PathId> {
    let mut ordinals = Vec::with_capacity(document.instances.len());
    let mut counts = HashMap::<(Option<usize>, &str, &str), usize>::new();
    for instance in &document.instances {
        let count = counts
            .entry((
                instance.parent_index,
                instance.name.as_str(),
                instance.class_name.as_str(),
            ))
            .and_modify(|value| *value += 1)
            .or_insert(1);
        ordinals.push(*count);
    }
    intern_paths(document, &ordinals, interner, |instance, ordinal| {
        StructuralPart {
            name: instance.name.clone(),
            class_name: instance.class_name.clone(),
            ordinal,
        }
    })
}

pub(crate) fn pairing_path_ids(
    document: &SettingsBytecode,
    interner: &mut PathInterner<PairingPart>,
) -> Vec<PathId> {
    let mut ordinals = Vec::with_capacity(document.instances.len());
    let mut counts = HashMap::<(Option<usize>, &str), usize>::new();
    for instance in &document.instances {
        let ordinal = counts
            .entry((instance.parent_index, instance.name.as_str()))
            .and_modify(|value| *value += 1)
            .or_insert(1);
        ordinals.push(*ordinal);
    }
    intern_paths(document, &ordinals, interner, |instance, ordinal| {
        PairingPart {
            name: instance.name.clone(),
            ordinal,
        }
    })
}

pub(crate) fn align_observation_ids_to_baseline(
    baseline: &SettingsBytecode,
    observed: &mut SettingsBytecode,
) {
    align_settings_ids_to_reference(baseline, observed);
}

pub(crate) fn align_new_instance_ids(
    baseline: &SettingsBytecode,
    editor: &SettingsBytecode,
    studio: &mut SettingsBytecode,
) -> Result<()> {
    let baseline_ids = baseline
        .instances
        .iter()
        .map(|instance| instance.settings_id.as_str())
        .collect::<HashSet<_>>();
    let mut interner = PathInterner::new();
    let editor_keys = structural_path_ids(editor, &mut interner);
    let studio_keys = structural_path_ids(studio, &mut interner);
    let editor_by_key = editor_keys
        .iter()
        .copied()
        .enumerate()
        .map(|(index, key)| (key, index))
        .collect::<HashMap<_, _>>();
    let studio_by_key = studio_keys
        .iter()
        .copied()
        .enumerate()
        .map(|(index, key)| (key, index))
        .collect::<HashMap<_, _>>();
    let editor_persistent = persistent_identity_index(editor);
    let studio_persistent = persistent_identity_index(studio);
    let persistent_pairs = studio
        .instances
        .iter()
        .enumerate()
        .filter_map(|(index, instance)| {
            let id = persistent_identity(instance)?;
            if studio_persistent.get(id) != Some(&Some(index)) {
                return None;
            }
            let target = editor_persistent.get(id).copied().flatten()?;
            (editor.instances[target].class_name == instance.class_name).then_some((index, target))
        })
        .collect::<HashMap<_, _>>();
    let persistent_targets = persistent_pairs.values().copied().collect::<HashSet<_>>();
    let editor_index_by_id = editor
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    // Ordinal pairing alone resolves references inside the values compared below.
    let provisional = studio_keys
        .iter()
        .copied()
        .enumerate()
        .filter_map(|(studio_index, key)| {
            let editor_index = persistent_pairs.get(&studio_index).copied().or_else(|| {
                editor_by_key
                    .get(&key)
                    .copied()
                    .filter(|index| !persistent_targets.contains(index))
            })?;
            let editor_id = editor.instances[editor_index].settings_id.as_str();
            let studio_id = studio.instances[studio_index].settings_id.as_str();
            (!baseline_ids.contains(editor_id)
                && !baseline_ids.contains(studio_id)
                && editor_id != studio_id)
                .then(|| (studio_id.to_string(), editor_id.to_string()))
        })
        .collect::<HashMap<_, _>>();
    let observed_record = |studio_index: usize| {
        let observed = &studio.instances[studio_index];
        let mut properties = observed.properties.clone();
        let mut attributes = observed.attributes.clone();
        remap_record_reference_ids(&mut properties, &provisional);
        remap_record_reference_ids(&mut attributes, &provisional);
        (properties, attributes)
    };
    let records_equal = |editor_index: usize, record: &(Map<String, Value>, Map<String, Value>)| {
        let desired = &editor.instances[editor_index];
        reconciliation_maps_equal(&desired.class_name, &desired.properties, &record.0)
            && reconciliation_values_map_equal(&desired.attributes, &record.1)
    };
    fn slot_available(
        editor: &SettingsBytecode,
        editor_index: usize,
        class_name: &str,
        persistent_targets: &HashSet<usize>,
        baseline_ids: &HashSet<&str>,
        claimed: &HashSet<String>,
    ) -> bool {
        let desired = &editor.instances[editor_index];
        !persistent_targets.contains(&editor_index)
            && !baseline_ids.contains(desired.settings_id.as_str())
            && !claimed.contains(&desired.settings_id)
            && desired.class_name == class_name
    }
    // Parents pair before their children, so a subtree follows a parent that
    // matched a differently ordered sibling instead of the slot at its ordinal.
    let mut depth = vec![0usize; studio.instances.len()];
    for (index, slot) in depth.iter_mut().enumerate() {
        let mut current = index;
        let mut level = 0;
        while let Some(parent) = studio.instances[current].parent_index {
            level += 1;
            current = parent;
            if level > studio.instances.len() {
                break;
            }
        }
        *slot = level;
    }
    let mut order = (0..studio.instances.len()).collect::<Vec<_>>();
    order.sort_by_key(|index| depth[*index]);
    let mut editor_key_of = HashMap::<PathId, PathId>::new();
    let mut remap = HashMap::<String, String>::new();
    let mut claimed = HashSet::<String>::new();
    let mut unmatched = Vec::new();
    for studio_index in order {
        let key = studio_keys[studio_index];
        let node = interner.node(key);
        let mapped_parent = node
            .parent
            .map(|parent| editor_key_of.get(&parent).copied().unwrap_or(parent));
        let observed = &studio.instances[studio_index];
        let studio_id = observed.settings_id.as_str();
        if baseline_ids.contains(studio_id) {
            if let Some(editor_index) = editor_index_by_id.get(studio_id).copied() {
                editor_key_of.insert(key, editor_keys[editor_index]);
            }
            continue;
        }
        // A unique engine identity remains valid across source edits and sibling
        // reordering. Only structural fallback candidates need value evidence.
        if let Some(editor_index) = persistent_pairs.get(&studio_index).copied() {
            let editor_id = editor.instances[editor_index].settings_id.as_str();
            editor_key_of.insert(key, editor_keys[editor_index]);
            if !baseline_ids.contains(editor_id) && editor_id != studio_id {
                claimed.insert(editor_id.to_string());
                remap.insert(studio_id.to_string(), editor_id.to_string());
            }
            continue;
        }
        let ordinal_partner = interner
            .find(mapped_parent, &node.part)
            .and_then(|sibling| editor_by_key.get(&sibling).copied());
        if let Some(editor_index) = ordinal_partner
            && editor.instances[editor_index].settings_id == studio_id
        {
            editor_key_of.insert(key, editor_keys[editor_index]);
            continue;
        }
        let mut second = node.part.clone();
        second.ordinal = 2;
        // New identities are paired by their unique structural location, not by
        // value. Duplicate-name slots still require equivalent data: their
        // ordinal alone is not identity, and the same new duplicates can sit in
        // a different sibling order on each side.
        let duplicate_slot = node.part.ordinal > 1
            || interner
                .find(mapped_parent, &second)
                .is_some_and(|sibling| editor_by_key.contains_key(&sibling))
            || interner
                .find(node.parent, &second)
                .is_some_and(|sibling| studio_by_key.contains_key(&sibling));
        let record = observed_record(studio_index);
        let mut partner = ordinal_partner.filter(|editor_index| {
            slot_available(
                editor,
                *editor_index,
                &observed.class_name,
                &persistent_targets,
                &baseline_ids,
                &claimed,
            )
        });
        if duplicate_slot
            && partner.is_some_and(|editor_index| !records_equal(editor_index, &record))
        {
            partner = None;
        }
        if partner.is_none() && duplicate_slot {
            let mut probe = node.part.clone();
            for ordinal in 1.. {
                probe.ordinal = ordinal;
                let Some(sibling) = interner.find(mapped_parent, &probe) else {
                    break;
                };
                let Some(editor_index) = editor_by_key.get(&sibling).copied() else {
                    continue;
                };
                if slot_available(
                    editor,
                    editor_index,
                    &observed.class_name,
                    &persistent_targets,
                    &baseline_ids,
                    &claimed,
                ) && records_equal(editor_index, &record)
                {
                    partner = Some(editor_index);
                    break;
                }
            }
        }
        match partner {
            Some(editor_index) => {
                let editor_id = editor.instances[editor_index].settings_id.clone();
                editor_key_of.insert(key, editor_keys[editor_index]);
                claimed.insert(editor_id.clone());
                remap.insert(studio_id.to_string(), editor_id);
            }
            None if duplicate_slot => unmatched.push((studio_index, key, mapped_parent, record)),
            None => {}
        }
    }
    // Slots left without a partner are additions of their own side, unless both
    // sides keep unmatched siblings: then nothing tells an edit apart from a
    // new instance.
    for (studio_index, key, mapped_parent, record) in unmatched {
        let observed = &studio.instances[studio_index];
        let node = interner.node(key);
        let mut probe = node.part.clone();
        for ordinal in 1.. {
            probe.ordinal = ordinal;
            let Some(sibling) = interner.find(mapped_parent, &probe) else {
                break;
            };
            let Some(editor_index) = editor_by_key.get(&sibling).copied() else {
                continue;
            };
            if slot_available(
                editor,
                editor_index,
                &observed.class_name,
                &persistent_targets,
                &baseline_ids,
                &claimed,
            ) {
                let desired = &editor.instances[editor_index];
                let difference = first_record_difference(
                    &desired.class_name,
                    (&record.0, &record.1),
                    (&desired.properties, &desired.attributes),
                )
                .map(|difference| format!(" ({difference})"))
                .unwrap_or_default();
                bail!(
                    "Ambiguous new duplicate instances at {}; Studio was not changed{difference}",
                    render_structural_key(&interner, key)
                );
            }
        }
    }
    let targets = remap.values().cloned().collect::<HashSet<_>>();
    let mut all_ids = editor
        .instances
        .iter()
        .chain(&studio.instances)
        .map(|instance| instance.settings_id.clone())
        .collect::<HashSet<_>>();
    let mut seed = all_ids.len();
    for instance in &studio.instances {
        let id = &instance.settings_id;
        if targets.contains(id) && !remap.contains_key(id) {
            remap.insert(
                id.clone(),
                crate::bytecode::edit::next_editor_settings_id_fast(&mut all_ids, &mut seed),
            );
        }
    }
    for instance in &mut studio.instances {
        if let Some(id) = remap.get(&instance.settings_id) {
            instance.settings_id.clone_from(id);
        }
        remap_record_reference_ids(&mut instance.properties, &remap);
        remap_record_reference_ids(&mut instance.attributes, &remap);
    }
    Ok(())
}

pub(crate) fn align_first_pairing(
    path: &Path,
    editor: &mut SettingsBytecode,
    studio: &mut SettingsBytecode,
    preference: ConflictPreference,
    conflicts: &mut Vec<String>,
) {
    align_settings_ids_to_reference(editor, studio);
    let mut structural_interner = PathInterner::new();
    let editor_keys = structural_path_ids(editor, &mut structural_interner);
    let mut studio_keys = structural_path_ids(studio, &mut structural_interner);
    // Sibling order is not identity. A proven engine identity can match a
    // reordered duplicate even when its authored values have changed.
    let editor_persistent = persistent_identity_index(editor);
    let studio_persistent = persistent_identity_index(studio);
    let mut identity_keys = studio_keys.clone();
    for (index, instance) in studio.instances.iter().enumerate() {
        let Some(id) = persistent_identity(instance) else {
            continue;
        };
        if studio_persistent.get(id) != Some(&Some(index)) {
            continue;
        }
        let Some(editor_index) = editor_persistent.get(id).copied().flatten() else {
            continue;
        };
        let desired = structural_interner.node(editor_keys[editor_index]);
        let observed = structural_interner.node(studio_keys[index]);
        if desired.parent == observed.parent
            && desired.part.name == observed.part.name
            && desired.part.class_name == observed.part.class_name
        {
            identity_keys[index] = editor_keys[editor_index];
        }
    }
    if identity_keys.iter().copied().collect::<HashSet<_>>().len() == identity_keys.len() {
        studio_keys = identity_keys;
    }
    let mut pairing_interner = PathInterner::new();
    let editor_pairing_keys = pairing_path_ids(editor, &mut pairing_interner);
    let studio_pairing_keys = pairing_path_ids(studio, &mut pairing_interner);
    let editor_by_key = editor_keys
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, key)| (key, index))
        .collect::<HashMap<_, _>>();
    let studio_by_key = studio_keys
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, key)| (key, index))
        .collect::<HashMap<_, _>>();
    let stable_pairs = editor_by_key
        .iter()
        .filter_map(|(key, editor_index)| {
            studio_by_key.get(key).and_then(|studio_index| {
                (editor.instances[*editor_index].settings_id
                    == studio.instances[*studio_index].settings_id)
                    .then_some(*key)
            })
        })
        .collect::<HashSet<_>>();
    let editor_by_pairing_key = editor_pairing_keys
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, key)| (key, index))
        .collect::<HashMap<_, _>>();
    for (studio_index, key) in studio_pairing_keys.iter().enumerate() {
        if let Some(editor_index) = editor_by_pairing_key.get(key).copied()
            && editor.instances[editor_index].class_name
                != studio.instances[studio_index].class_name
        {
            conflicts.push(format!(
                "{} has different classes for {}",
                path.display(),
                render_pairing_key(&pairing_interner, *key)
            ));
        }
    }
    let editor_key_by_id = editor
        .instances
        .iter()
        .zip(&editor_keys)
        .map(|(instance, key)| (instance.settings_id.clone(), key))
        .collect::<HashMap<_, _>>();
    for (instance, key) in studio.instances.iter().zip(&studio_keys) {
        if editor_key_by_id
            .get(&instance.settings_id)
            .is_some_and(|editor_key| *editor_key != key)
        {
            conflicts.push(format!(
                "{} has the same instance identity at different paths",
                path.display()
            ));
        }
    }

    let id_remap = editor_by_key
        .iter()
        .filter_map(|(key, editor_index)| {
            studio_by_key.get(key).map(|studio_index| {
                (
                    studio.instances[*studio_index].settings_id.clone(),
                    editor.instances[*editor_index].settings_id.clone(),
                )
            })
        })
        .collect::<HashMap<_, _>>();
    for instance in &mut studio.instances {
        if let Some(id) = id_remap.get(&instance.settings_id) {
            instance.settings_id.clone_from(id);
        }
        remap_record_reference_ids(&mut instance.properties, &id_remap);
        remap_record_reference_ids(&mut instance.attributes, &id_remap);
    }
    crate::settings::equivalence::inherit_workspace_viewport_reference(editor, studio);
    align_reconciliation_protected_workspace_cameras(editor, studio);
    align_equivalent_values(editor, studio);
    protect_package_links(path, None, editor, studio, conflicts);

    for (key, editor_index) in &editor_by_key {
        let Some(studio_index) = studio_by_key.get(key).copied() else {
            continue;
        };
        if settings_instances_equal(editor, *editor_index, studio, studio_index) {
            continue;
        }
        let node = structural_interner.node(*key);
        let duplicate_slot = if node.part.ordinal > 1 {
            true
        } else {
            let mut second = node.part.clone();
            second.ordinal = 2;
            structural_interner
                .find(node.parent, &second)
                .is_some_and(|second| {
                    editor_by_key.contains_key(&second) || studio_by_key.contains_key(&second)
                })
        };
        if duplicate_slot && !stable_pairs.contains(key) {
            conflicts.push(format!(
                "{} has ambiguous duplicate instances at {}",
                path.display(),
                render_structural_key(&structural_interner, *key)
            ));
            continue;
        }
        match preference {
            ConflictPreference::None => conflicts.push(format!(
                "{} has different values for {}",
                path.display(),
                render_structural_key(&structural_interner, *key)
            )),
            ConflictPreference::Editor => {
                copy_settings_instance(editor, *editor_index, studio, studio_index)
            }
            ConflictPreference::Studio => {
                copy_settings_instance(studio, studio_index, editor, *editor_index)
            }
        }
    }
}

pub(crate) fn protect_package_links(
    path: &Path,
    base: Option<&SettingsBytecode>,
    editor: &mut SettingsBytecode,
    studio: &mut SettingsBytecode,
    conflicts: &mut Vec<String>,
) {
    let base_by_id = base
        .into_iter()
        .flat_map(|document| document.instances.iter().enumerate())
        .filter(|(_, instance)| instance.class_name == "PackageLink")
        .map(|(index, instance)| (instance.settings_id.clone(), (index, instance)))
        .collect::<HashMap<_, _>>();
    let studio_by_id = studio
        .instances
        .iter()
        .enumerate()
        .filter(|(_, instance)| instance.class_name == "PackageLink")
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    let editor_by_id = editor
        .instances
        .iter()
        .enumerate()
        .filter(|(_, instance)| instance.class_name == "PackageLink")
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();

    for (id, (base_index, base_instance)) in &base_by_id {
        if !studio_by_id.contains_key(id)
            && !base
                .is_some_and(|document| package_parent_is_missing(document, *base_index, studio))
        {
            let parent = base
                .and_then(|document| settings_parent_id(document, *base_index))
                .unwrap_or("unknown");
            conflicts.push(format!(
                "{} removes PackageLink {} ({id}) directly while parent {parent} remains in Studio",
                path.display(),
                base_instance.name
            ));
        }
        if !editor_by_id.contains_key(id)
            && studio_by_id.contains_key(id)
            && !base
                .is_some_and(|document| package_parent_is_missing(document, *base_index, editor))
        {
            conflicts.push(format!(
                "{} omits PackageLink {}; ordinary sync preserves package relationships",
                path.display(),
                base_instance.name
            ));
        }
    }

    for (id, editor_index) in editor_by_id {
        let Some(studio_index) = studio_by_id.get(&id).copied() else {
            if !base_by_id.contains_key(&id) {
                conflicts.push(format!(
                    "{} cannot create a PackageLink from project files",
                    path.display()
                ));
            }
            continue;
        };
        let editor_parent = settings_parent_id(editor, editor_index).map(str::to_string);
        let studio_parent = settings_parent_id(studio, studio_index).map(str::to_string);
        let editor_instance = &editor.instances[editor_index];
        let studio_instance = &studio.instances[studio_index];
        if editor_instance.name != studio_instance.name
            || editor_instance.class_name != studio_instance.class_name
            || editor_parent != studio_parent
        {
            conflicts.push(format!(
                "{} directly renames or reparents PackageLink {}",
                path.display(),
                studio_instance.name
            ));
            continue;
        }
        editor.instances[editor_index]
            .properties
            .clone_from(&studio_instance.properties);
        editor.instances[editor_index]
            .attributes
            .clone_from(&studio_instance.attributes);
    }
}

pub(crate) fn settings_parent_id(document: &SettingsBytecode, index: usize) -> Option<&str> {
    document.instances[index]
        .parent_index
        .and_then(|parent| document.instances.get(parent))
        .map(|parent| parent.settings_id.as_str())
}

pub(crate) fn package_parent_is_missing(
    source: &SettingsBytecode,
    package_link_index: usize,
    target: &SettingsBytecode,
) -> bool {
    settings_parent_id(source, package_link_index).is_some_and(|parent_id| {
        !target
            .instances
            .iter()
            .any(|instance| instance.settings_id == parent_id)
    })
}

pub(crate) fn settings_instances_equal(
    left: &SettingsBytecode,
    left_index: usize,
    right: &SettingsBytecode,
    right_index: usize,
) -> bool {
    let left_instance = &left.instances[left_index];
    let right_instance = &right.instances[right_index];
    left_instance.name == right_instance.name
        && left_instance.class_name == right_instance.class_name
        && settings_parent_id(left, left_index) == settings_parent_id(right, right_index)
        && reconciliation_maps_equal(
            &left_instance.class_name,
            &left_instance.properties,
            &right_instance.properties,
        )
        && reconciliation_values_map_equal(&left_instance.attributes, &right_instance.attributes)
}

pub(crate) fn copy_settings_instance(
    source: &SettingsBytecode,
    source_index: usize,
    target: &mut SettingsBytecode,
    target_index: usize,
) {
    let source = &source.instances[source_index];
    let target = &mut target.instances[target_index];
    target.name.clone_from(&source.name);
    target.class_name.clone_from(&source.class_name);
    target.properties.clone_from(&source.properties);
    target.attributes.clone_from(&source.attributes);
}

pub(crate) fn render_structural_key(interner: &PathInterner<StructuralPart>, id: PathId) -> String {
    render_path(interner, id, |part| {
        if part.ordinal == 1 {
            part.name.clone()
        } else {
            format!("{}[{}]", part.name, part.ordinal)
        }
    })
}

pub(crate) fn render_pairing_key(interner: &PathInterner<PairingPart>, id: PathId) -> String {
    render_path(interner, id, |part| {
        if part.ordinal == 1 {
            part.name.clone()
        } else {
            format!("{}[{}]", part.name, part.ordinal)
        }
    })
}

pub(crate) fn render_path<P>(
    interner: &PathInterner<P>,
    mut id: PathId,
    render: impl Fn(&P) -> String,
) -> String
where
    P: Clone + Eq + std::hash::Hash,
{
    let mut parts = Vec::new();
    loop {
        let node = interner.node(id);
        parts.push(render(&node.part));
        let Some(parent) = node.parent else {
            break;
        };
        id = parent;
    }
    parts.reverse();
    parts.join(".")
}

pub(crate) fn align_transient_script_guids(
    base: &mut SettingsBytecode,
    editor: &mut SettingsBytecode,
    studio: &mut SettingsBytecode,
) {
    let editor_by_id = editor
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    let studio_by_id = studio
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    for base_instance in &mut base.instances {
        let Some(editor_index) = editor_by_id.get(&base_instance.settings_id).copied() else {
            continue;
        };
        let Some(studio_index) = studio_by_id.get(&base_instance.settings_id).copied() else {
            continue;
        };
        let value = editor.instances[editor_index]
            .properties
            .get("ScriptGuid")
            .cloned()
            .or_else(|| {
                studio.instances[studio_index]
                    .properties
                    .get("ScriptGuid")
                    .cloned()
            });
        if let Some(value) = value {
            base_instance
                .properties
                .insert("ScriptGuid".to_string(), value.clone());
            editor.instances[editor_index]
                .properties
                .insert("ScriptGuid".to_string(), value.clone());
            studio.instances[studio_index]
                .properties
                .insert("ScriptGuid".to_string(), value);
        }
    }
}
