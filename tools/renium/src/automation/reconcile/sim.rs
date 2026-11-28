//! Seeded simulation of the three-way merge between the files and Studio.
//!
//! Each seed builds a small service, lets both sides edit it, merges, and
//! compares the result with a model: the same edits applied to one tree.
//! Rounds continue from the merged state, through the stops a real sync can
//! meet between its steps: after the files were written, after Studio was
//! written, or before the shared state was recorded. Studio's side is always
//! captured with its siblings in another order.
//!
//! `crate::tests::support::run_seeds` names the variables that run more
//! seeds or replay one.

use super::*;
use crate::tests::support::{SimRng as Rng, run_seeds};

const SERVICE: &str = "ReplicatedStorage";
const ROOT: &str = "root";
const DEFAULT_SEEDS: u64 = 200;
const ROUNDS: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
    name: String,
    class: String,
    parent: Option<String>,
    value: Option<String>,
    attributes: BTreeMap<String, String>,
    source: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Tree {
    nodes: BTreeMap<String, Node>,
}

impl Tree {
    fn children(&self, id: &str) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|(_, node)| node.parent.as_deref() == Some(id))
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn subtree(&self, id: &str) -> Vec<String> {
        let mut found = vec![id.to_string()];
        let mut index = 0;
        while index < found.len() {
            let children = self.children(&found[index]);
            found.extend(children);
            index += 1;
        }
        found
    }

    fn ancestors(&self, id: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut current = self.nodes.get(id).and_then(|node| node.parent.clone());
        while let Some(id) = current {
            current = self.nodes.get(&id).and_then(|node| node.parent.clone());
            found.push(id);
        }
        found
    }

    /// The service a node is in: the name of its root.
    fn service_of(&self, id: &str) -> Option<&str> {
        let root = self
            .ancestors(id)
            .last()
            .cloned()
            .unwrap_or_else(|| id.to_string());
        self.nodes.get(&root).map(|node| node.name.as_str())
    }

    /// The names from a node's service down to it.
    fn name_path(&self, id: &str) -> Vec<String> {
        let mut names = self
            .ancestors(id)
            .iter()
            .rev()
            .map(|id| self.nodes[id].name.clone())
            .collect::<Vec<_>>();
        names.push(self.nodes[id].name.clone());
        names
    }

    /// An ObjectValue's reference: an id in its own store, or the path and
    /// id of an instance of another service.
    fn reference(&self, holder: &str, target: &str) -> Value {
        if self.nodes.contains_key(target) && self.service_of(target) != self.service_of(holder) {
            let segments = self.name_path(target);
            json!({
                "_type": "Ref",
                "settingsId": target,
                "pathOrdinals": vec![1; segments.len()],
                "pathSegments": segments,
            })
        } else {
            json!({"_type": "Ref", "settingsId": target})
        }
    }

    /// Every service the tree holds: each root is one, with one store.
    fn snapshot(&self, mut noise: Option<&mut Rng>) -> ProjectSnapshot {
        let mut entries = BTreeMap::new();
        for (root, root_node) in self.nodes.iter().filter(|(_, node)| node.parent.is_none()) {
            self.service_snapshot(root, &root_node.name, noise.as_deref_mut(), &mut entries);
        }
        ProjectSnapshot { entries }
    }

    fn service_snapshot(
        &self,
        root: &str,
        service: &str,
        mut noise: Option<&mut Rng>,
        entries: &mut BTreeMap<PathBuf, SnapshotEntry>,
    ) {
        let mut order = vec![root.to_string()];
        let mut index = 0;
        while index < order.len() {
            let mut children = self.children(&order[index]);
            if let Some(rng) = noise.as_deref_mut() {
                rng.shuffle(&mut children);
            }
            order.extend(children);
            index += 1;
        }
        let positions = order
            .iter()
            .enumerate()
            .map(|(index, id)| (id.as_str(), index))
            .collect::<HashMap<_, _>>();
        let instances = order
            .iter()
            .map(|id| {
                let node = &self.nodes[id];
                let mut instance = SettingsBytecodeInstance::new(
                    id.clone(),
                    node.name.clone(),
                    node.class.clone(),
                    node.parent.as_deref().map(|parent| positions[parent]),
                );
                match (&node.value, node.class.as_str()) {
                    (Some(target), "ObjectValue") => {
                        instance
                            .properties
                            .insert("Value".into(), self.reference(id, target));
                    }
                    (Some(value), _) => {
                        instance.properties.insert("Value".into(), json!(value));
                    }
                    (None, _) => {}
                }
                for (key, value) in &node.attributes {
                    instance.attributes.insert(key.clone(), json!(value));
                }
                instance
            })
            .collect();
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        };
        let service_dir = Path::new("src").join(service);
        entries.insert(service_dir.clone(), SnapshotEntry::Directory);
        let paths = build_editor_source_paths_by_index(&document, service, &service_dir);
        for (instance, path) in document.instances.iter().zip(paths) {
            let Some(path) = path else { continue };
            for ancestor in path.ancestors().skip(1) {
                if ancestor.starts_with(&service_dir) {
                    entries.insert(ancestor.to_path_buf(), SnapshotEntry::Directory);
                }
            }
            let source = self.nodes[&instance.settings_id]
                .source
                .clone()
                .unwrap_or_default();
            entries.insert(path, SnapshotEntry::File(source.into_bytes()));
        }
        entries.insert(
            service_settings_path(&service_dir),
            SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
        );
    }

    /// Reads a snapshot back into the model, refusing one that is not a
    /// well-formed project: an identity twice, a script without its file, a
    /// script file no script owns.
    fn read(snapshot: &ProjectSnapshot) -> Result<Tree, String> {
        let mut tree = Tree::default();
        let mut owned = HashSet::new();
        let stores = snapshot.entries.keys().filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_service_settings_file_name)
        });
        for settings_path in stores {
            tree.read_store(snapshot, settings_path, &mut owned)?;
        }
        for (path, entry) in &snapshot.entries {
            if matches!(entry, SnapshotEntry::File(_))
                && is_source_path(path)
                && !owned.contains(path)
            {
                return Err(format!("no script owns the file {}", path.display()));
            }
        }
        Ok(tree)
    }

    fn read_store(
        &mut self,
        snapshot: &ProjectSnapshot,
        settings_path: &Path,
        owned: &mut HashSet<PathBuf>,
    ) -> Result<(), String> {
        let document = settings_document(snapshot.entries.get(settings_path))
            .map_err(|error| format!("the store does not decode: {error:#}"))?;
        let paths = source_paths_by_settings_index(&document, settings_path)
            .map_err(|error| format!("script paths: {error:#}"))?;
        for (instance, path) in document.instances.iter().zip(paths) {
            let source = match path {
                Some(path) => match snapshot.entries.get(&path) {
                    Some(SnapshotEntry::File(bytes)) => {
                        let source = String::from_utf8_lossy(bytes).into_owned();
                        owned.insert(path);
                        Some(source)
                    }
                    _ => {
                        return Err(format!(
                            "script {} has no file at {}",
                            instance.settings_id,
                            path.display()
                        ));
                    }
                },
                None => None,
            };
            let value = instance.properties.get("Value");
            let node = Node {
                name: instance.name.clone(),
                class: instance.class_name.clone(),
                parent: instance
                    .parent_index
                    .map(|parent| document.instances[parent].settings_id.clone()),
                value: value
                    .and_then(|value| {
                        (value.get("settingsId"))
                            .or_else(|| value.get("value"))
                            .or(Some(value))
                    })
                    .and_then(Value::as_str)
                    .map(str::to_string),
                attributes: instance
                    .attributes
                    .iter()
                    .map(|(key, value)| (key.clone(), value.as_str().unwrap_or("?").to_string()))
                    .collect(),
                source,
            };
            if self
                .nodes
                .insert(instance.settings_id.clone(), node)
                .is_some()
            {
                return Err(format!("identity {} twice", instance.settings_id));
            }
        }
        Ok(())
    }

    fn difference(&self, expected: &Tree) -> Option<String> {
        for (id, node) in &expected.nodes {
            match self.nodes.get(id) {
                None => return Some(format!("{id} ({}) is missing", node.name)),
                Some(found) if found != node => {
                    return Some(format!("{id} is {found:?}, expected {node:?}"));
                }
                Some(_) => {}
            }
        }
        self.nodes
            .keys()
            .find(|id| !expected.nodes.contains_key(*id))
            .map(|id| format!("{id} ({:?}) should not exist", self.nodes[id]))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Editor,
    Studio,
}

impl Side {
    fn index(self) -> usize {
        self as usize
    }

    fn other(self) -> Side {
        match self {
            Side::Editor => Side::Studio,
            Side::Studio => Side::Editor,
        }
    }
}

#[derive(Clone, Debug)]
enum Op {
    Value(String, String),
    Attribute(String, String, Option<String>),
    Source(String, String),
    Rename(String, String),
    Move(String, String),
    Delete(String),
    Add(String, Node),
}

impl Op {
    fn apply(&self, tree: &mut Tree) {
        match self {
            Op::Value(id, value) => tree.nodes.get_mut(id).unwrap().value = Some(value.clone()),
            Op::Attribute(id, key, value) => {
                let attributes = &mut tree.nodes.get_mut(id).unwrap().attributes;
                match value {
                    Some(value) => attributes.insert(key.clone(), value.clone()),
                    None => attributes.remove(key),
                };
            }
            Op::Source(id, source) => {
                tree.nodes.get_mut(id).unwrap().source = Some(source.clone());
            }
            Op::Rename(id, name) => tree.nodes.get_mut(id).unwrap().name.clone_from(name),
            Op::Move(id, parent) => {
                tree.nodes.get_mut(id).unwrap().parent = Some(parent.clone());
            }
            Op::Delete(id) => {
                for id in tree.subtree(id) {
                    tree.nodes.remove(&id);
                }
            }
            Op::Add(id, node) => {
                tree.nodes.insert(id.clone(), node.clone());
            }
        }
    }
}

/// One round of edits on both sides, with the tree each preference must end
/// in. An edit is taken only where it is independent of the other side's
/// edits, or, for a value, where it contests one of them on purpose.
struct Edits {
    sides: [Tree; 2],
    expected: [Tree; 2],
    fields: HashMap<String, HashMap<String, Side>>,
    contested: HashSet<(String, String)>,
    needed: HashMap<String, [bool; 2]>,
    /// Instances renamed to make room for a new one under their old name.
    /// Removing one afterwards leaves a new instance where an old one of the
    /// same name stood, which a merge may rightly read as that old instance
    /// changed in place, under the old identity.
    renamed_aside: HashSet<String>,
    log: Vec<String>,
}

impl Edits {
    fn new(shared: &Tree) -> Self {
        Self {
            sides: [shared.clone(), shared.clone()],
            expected: [shared.clone(), shared.clone()],
            fields: HashMap::new(),
            contested: HashSet::new(),
            needed: HashMap::new(),
            renamed_aside: HashSet::new(),
            log: Vec::new(),
        }
    }

    fn touched_by(&self, side: Side, id: &str) -> bool {
        self.needed
            .get(id)
            .is_some_and(|needed| needed[side.index()])
            || self
                .fields
                .get(id)
                .is_some_and(|fields| fields.values().any(|owner| *owner == side))
    }

    fn need(&mut self, side: Side, id: &str) {
        let mut chain = vec![id.to_string()];
        chain.extend(self.sides[side.index()].ancestors(id));
        chain.extend(self.expected[0].ancestors(id));
        for id in chain {
            self.needed.entry(id).or_default()[side.index()] = true;
        }
    }

    /// Whether `side` may write the field: yes when the other side left it
    /// alone, and, for a contest, when the caller allows one.
    fn claim(&mut self, side: Side, id: &str, field: &str, contest: bool) -> Option<bool> {
        let key = (id.to_string(), field.to_string());
        let owner = self.fields.get(id).and_then(|fields| fields.get(field));
        let contested = match owner {
            Some(owner) if *owner != side => {
                if !contest {
                    return None;
                }
                self.contested.insert(key.clone());
                true
            }
            _ => self.contested.contains(&key),
        };
        self.fields
            .entry(key.0)
            .or_default()
            .entry(key.1)
            .or_insert(side);
        self.need(side, id);
        Some(contested)
    }

    fn take(&mut self, side: Side, op: Op, contested: bool) {
        self.log.push(format!("{side:?}: {op:?}"));
        op.apply(&mut self.sides[side.index()]);
        if contested {
            op.apply(&mut self.expected[side.index()]);
        } else {
            op.apply(&mut self.expected[0]);
            op.apply(&mut self.expected[1]);
        }
    }

    fn edit(&mut self, rng: &mut Rng, counter: &mut u64, side: Side, contests: bool) {
        let own = self.sides[side.index()].clone();
        let present = own
            .nodes
            .keys()
            .filter(|id| self.expected[0].nodes.contains_key(*id))
            .cloned()
            .collect::<Vec<_>>();
        let of_class = |class: &str| {
            present
                .iter()
                .filter(|id| own.nodes[*id].class == class)
                .cloned()
                .collect::<Vec<_>>()
        };
        let movable = present
            .iter()
            .filter(|id| *id != ROOT)
            .cloned()
            .collect::<Vec<_>>();
        *counter += 1;
        let fresh = *counter;
        let contest = contests && rng.below(3) == 0;
        match rng.below(100) {
            0..=24 => {
                let Some(id) = rng.pick(&of_class("StringValue")) else {
                    return;
                };
                if let Some(contested) = self.claim(side, &id, "value", contest) {
                    self.take(side, Op::Value(id, format!("v{fresh}")), contested);
                }
            }
            25..=39 => {
                let Some(id) = rng.pick(&movable) else { return };
                let key = format!("K{}", rng.below(3));
                let value = (rng.below(4) != 0).then(|| format!("a{fresh}"));
                if value.is_none() && !own.nodes[&id].attributes.contains_key(&key) {
                    return;
                }
                let field = format!("attribute {key}");
                if let Some(contested) = self.claim(side, &id, &field, contest) {
                    self.take(side, Op::Attribute(id, key, value), contested);
                }
            }
            40..=54 => {
                let Some(id) = rng.pick(&of_class("ModuleScript")) else {
                    return;
                };
                if let Some(contested) = self.claim(side, &id, "source", contest) {
                    self.take(side, Op::Source(id, format!("return {fresh}\n")), contested);
                }
            }
            55..=64 => {
                let Some(id) = rng.pick(&movable) else { return };
                if self.claim(side, &id, "name", false) == Some(false) {
                    self.take(side, Op::Rename(id, format!("N{fresh}")), false);
                }
            }
            65..=76 => {
                let (Some(id), Some(parent)) = (rng.pick(&movable), rng.pick(&present)) else {
                    return;
                };
                let inside = |tree: &Tree| tree.subtree(&id).contains(&parent);
                if inside(&own)
                    || inside(&self.expected[0])
                    || own.nodes[&id].parent.as_ref() == Some(&parent)
                {
                    return;
                }
                if self.claim(side, &id, "parent", false) == Some(false) {
                    self.need(side, &parent);
                    self.take(side, Op::Move(id, parent), false);
                }
            }
            77..=86 => {
                let Some(id) = rng.pick(&movable) else { return };
                let mut subtree = own.subtree(&id);
                subtree.extend(self.expected[0].subtree(&id));
                if subtree
                    .iter()
                    .any(|id| self.touched_by(side.other(), id) || self.renamed_aside.contains(id))
                {
                    return;
                }
                for id in &subtree {
                    self.fields
                        .entry(id.clone())
                        .or_default()
                        .entry("exists".into())
                        .or_insert(side);
                }
                self.take(side, Op::Delete(id), false);
            }
            87..=92 => {
                // A rename, and a new instance under the name that is free now.
                let Some(id) = rng.pick(&movable) else { return };
                let node = &own.nodes[&id];
                let (name, parent) = (node.name.clone(), node.parent.clone().unwrap());
                if !self.expected[0].nodes.contains_key(&parent)
                    || self.claim(side, &id, "name", false) != Some(false)
                {
                    return;
                }
                self.renamed_aside.insert(id.clone());
                self.take(side, Op::Rename(id, format!("N{fresh}")), false);
                let class = ["Folder", "StringValue", "ModuleScript"][rng.below(3)];
                let id = format!("{}{fresh}", if side == Side::Editor { "e" } else { "s" });
                let node = Node {
                    name,
                    class: class.to_string(),
                    parent: Some(parent.clone()),
                    value: (class == "StringValue").then(|| format!("v{fresh}")),
                    attributes: BTreeMap::new(),
                    source: (class == "ModuleScript").then(|| format!("return {fresh}\n")),
                };
                self.need(side, &parent);
                self.fields
                    .entry(id.clone())
                    .or_default()
                    .insert("exists".into(), side);
                self.take(side, Op::Add(id, node), false);
            }
            _ => {
                let Some(parent) = rng.pick(&present) else {
                    return;
                };
                let class = ["Folder", "StringValue", "ModuleScript"][rng.below(3)];
                let id = format!("{}{fresh}", if side == Side::Editor { "e" } else { "s" });
                let node = Node {
                    name: format!("N{fresh}"),
                    class: class.to_string(),
                    parent: Some(parent.clone()),
                    value: (class == "StringValue").then(|| format!("v{fresh}")),
                    attributes: BTreeMap::new(),
                    source: (class == "ModuleScript").then(|| format!("return {fresh}\n")),
                };
                self.need(side, &parent);
                self.fields
                    .entry(id.clone())
                    .or_default()
                    .insert("exists".into(), side);
                self.take(side, Op::Add(id, node), false);
            }
        }
    }
}

fn first_tree(rng: &mut Rng, counter: &mut u64) -> Tree {
    let mut tree = Tree::default();
    tree.nodes.insert(
        ROOT.to_string(),
        Node {
            name: SERVICE.to_string(),
            class: SERVICE.to_string(),
            parent: None,
            value: None,
            attributes: BTreeMap::new(),
            source: None,
        },
    );
    for _ in 0..6 + rng.below(10) {
        let present = tree.nodes.keys().cloned().collect::<Vec<_>>();
        let parent = rng.pick(&present).unwrap();
        *counter += 1;
        let class = ["Folder", "StringValue", "ModuleScript"][rng.below(3)];
        let node = Node {
            name: format!("N{counter}"),
            class: class.to_string(),
            parent: Some(parent),
            value: (class == "StringValue").then(|| format!("v{counter}")),
            attributes: BTreeMap::new(),
            source: (class == "ModuleScript").then(|| format!("return {counter}\n")),
        };
        tree.nodes.insert(format!("b{counter}"), node);
    }
    tree
}

struct Merged {
    tree: Result<Tree, String>,
    paths: BTreeSet<PathBuf>,
    conflicts: Vec<Conflict>,
    changes: MergeChanges,
}

fn script_paths(tree: &Tree) -> HashMap<String, PathBuf> {
    let snapshot = tree.snapshot(None);
    tree.nodes
        .values()
        .filter(|node| node.parent.is_none())
        .flat_map(|root| {
            let settings_path = service_settings_path(&Path::new("src").join(&root.name));
            script_paths_by_settings_id(&snapshot.entries, &settings_path).unwrap()
        })
        .collect()
}

/// Whether one side changed a script's source while the other changed the
/// path of its file: a rename or a move of the script or of something above
/// it, or a first or last child.
fn path_contest(shared: &Tree, editor: &Tree, studio: &Tree) -> bool {
    let shared_paths = script_paths(shared);
    let sides = [
        (script_paths(editor), editor),
        (script_paths(studio), studio),
    ];
    shared_paths.iter().any(|(id, shared_path)| {
        [(0, 1), (1, 0)].into_iter().any(|(mover, writer)| {
            sides[mover]
                .0
                .get(id)
                .is_some_and(|path| path != shared_path)
                && sides[writer]
                    .1
                    .nodes
                    .get(id)
                    .is_some_and(|node| node.source != shared.nodes[id].source)
        })
    })
}

fn merge(
    shared: &Tree,
    editor: &Tree,
    studio: &Tree,
    preference: impl Into<ConflictDecisions>,
    rng: &mut Rng,
) -> Result<Merged, String> {
    merge_from(Some(shared), editor, studio, preference, rng)
}

/// A merge against the shared tree, or a first pairing without one.
fn merge_from(
    shared: Option<&Tree>,
    editor: &Tree,
    studio: &Tree,
    preference: impl Into<ConflictDecisions>,
    rng: &mut Rng,
) -> Result<Merged, String> {
    // Without a shared tree the pairing reads Studio's order, which a real
    // capture keeps from one comparison to the next.
    let noise = shared.is_some().then_some(&mut *rng);
    let (editor, studio) = (editor.snapshot(None), studio.snapshot(noise));
    // A sync merges only the paths its two sides hold differently.
    let differences = (rng.below(2) == 0)
        .then(|| snapshot_differences(&editor, &studio))
        .transpose()
        .map_err(|error| format!("comparing the sides failed: {error:#}"))?;
    let (snapshot, conflicts, changes) = merge_snapshots_with_changes(
        shared.map(|shared| shared.snapshot(None)).as_ref(),
        &editor,
        &studio,
        preference,
        differences.as_ref(),
    )
    .map_err(|error| format!("the merge failed: {error:#}"))?;
    Ok(Merged {
        tree: Tree::read(&snapshot),
        paths: snapshot.entries.into_keys().collect(),
        conflicts,
        changes,
    })
}

fn expect_tree(what: &str, found: &Merged, expected: &Tree) -> Result<(), String> {
    if !found.conflicts.is_empty() {
        return Err(format!(
            "{what}: unexpected conflicts {:?}",
            found.conflicts
        ));
    }
    let tree = found
        .tree
        .as_ref()
        .map_err(|error| format!("{what}: {error}"))?;
    if let Some(difference) = tree.difference(expected) {
        return Err(format!("{what}: {difference}"));
    }
    // The files and folders too: a folder left behind, or one that is
    // missing, is a difference the next readback of Studio reports.
    let expected_paths = expected
        .snapshot(None)
        .entries
        .into_keys()
        .collect::<BTreeSet<_>>();
    match found.paths.symmetric_difference(&expected_paths).next() {
        Some(path) if found.paths.contains(path) => {
            Err(format!("{what}: {} should not exist", path.display()))
        }
        Some(path) => Err(format!("{what}: {} is missing", path.display())),
        None => Ok(()),
    }
}

fn run_seed(seed: u64, log: &mut Vec<String>) -> Result<(), String> {
    let mut rng = Rng(seed);
    let mut counter = 0;
    let mut shared = first_tree(&mut rng, &mut counter);
    for round in 0..ROUNDS {
        let contests = rng.below(3) == 0;
        let mut edits = Edits::new(&shared);
        for _ in 0..rng.below(9) {
            let side = if rng.below(2) == 0 {
                Side::Editor
            } else {
                Side::Studio
            };
            edits.edit(&mut rng, &mut counter, side, contests);
        }
        log.push(format!(
            "round {round}: {}",
            shared
                .nodes
                .iter()
                .map(|(id, node)| format!(
                    "{id}={}:{}<{}",
                    node.name,
                    &node.class[..1],
                    node.parent.as_deref().unwrap_or("-")
                ))
                .collect::<Vec<_>>()
                .join(" ")
        ));
        log.append(&mut edits.log);
        let [editor, studio] = &edits.sides;
        let contested = !edits.contested.is_empty();
        let path_contested = path_contest(&shared, editor, studio);

        let open = merge(&shared, editor, studio, ConflictPreference::None, &mut rng)?;
        if contested {
            if open.conflicts.is_empty() {
                return Err(format!(
                    "round {round}: both sides changed {:?} and no conflict was reported",
                    edits.contested
                ));
            }
        } else if !path_contested || open.conflicts.is_empty() {
            expect_tree(&format!("round {round}"), &open, &edits.expected[0])?;
        }
        let (preference, winner) = if rng.below(2) == 0 {
            (ConflictPreference::Editor, Side::Editor)
        } else {
            (ConflictPreference::Studio, Side::Studio)
        };
        let expected = &edits.expected[winner.index()];
        let settled = merge(&shared, editor, studio, preference, &mut rng)?;
        expect_tree(
            &format!(
                "round {round}, {winner:?} preferred{}",
                if path_contested {
                    " (path contest)"
                } else {
                    ""
                }
            ),
            &settled,
            expected,
        )?;
        let settled_tree = settled.tree.as_ref().unwrap();

        // A stop between the steps of a sync leaves one side, or both, already
        // holding the result while the shared state is the old one. The next
        // sync must end in the same place.
        let stops: [(&str, &Tree, &Tree); 3] = [
            (
                "the files were written and Studio was not",
                settled_tree,
                studio,
            ),
            (
                "Studio was written and the files were not",
                editor,
                settled_tree,
            ),
            (
                "both were written and the shared state was not",
                settled_tree,
                settled_tree,
            ),
        ];
        let (stop, editor_then, studio_then) = stops[rng.below(3)];
        let again = merge(&shared, editor_then, studio_then, preference, &mut rng)?;
        expect_tree(
            &format!("round {round}, after a stop when {stop}"),
            &again,
            expected,
        )?;

        let rest = merge(
            expected,
            expected,
            expected,
            ConflictPreference::None,
            &mut rng,
        )?;
        expect_tree(&format!("round {round}, a second sync"), &rest, expected)?;
        if !rest.changes.editor.is_empty() || !rest.changes.studio.is_empty() {
            return Err(format!(
                "round {round}: a second sync still changes {:?} in the files and {:?} in Studio",
                rest.changes.editor, rest.changes.studio
            ));
        }
        shared = expected.clone();
    }
    Ok(())
}

#[test]
fn seeded_edits_on_both_sides_merge_to_the_modeled_tree() {
    run_seeds(DEFAULT_SEEDS, run_seed);
}

/// A new node of `class` under `parent`; an ObjectValue refers to `target`.
fn new_node(class: &str, parent: String, fresh: u64, target: Option<String>) -> Node {
    Node {
        name: format!("N{fresh}"),
        class: class.to_string(),
        parent: Some(parent),
        value: match class {
            "StringValue" => Some(format!("v{fresh}")),
            "ObjectValue" => target,
            _ => None,
        },
        attributes: BTreeMap::new(),
        source: (class == "ModuleScript").then(|| format!("return {fresh}\n")),
    }
}

/// A second service whose ObjectValues refer into the first, which gets
/// some of its own.
fn add_second_service(tree: &mut Tree, rng: &mut Rng, counter: &mut u64) {
    tree.nodes.insert(
        "root2".to_string(),
        Node {
            name: "ServerStorage".to_string(),
            class: "ServerStorage".to_string(),
            parent: None,
            value: None,
            attributes: BTreeMap::new(),
            source: None,
        },
    );
    let first = tree.subtree(ROOT)[1..].to_vec();
    for _ in 0..3 + rng.below(4) {
        *counter += 1;
        let (parent, target) = if rng.below(3) == 0 {
            (rng.pick(&tree.subtree(ROOT)), rng.pick(&first))
        } else {
            (rng.pick(&tree.subtree("root2")), rng.pick(&first))
        };
        let class = ["Folder", "ObjectValue", "ObjectValue"][rng.below(3)];
        let node = new_node(class, parent.unwrap(), *counter, target);
        tree.nodes.insert(format!("b{counter}"), node);
    }
}

/// Edits one side without regard for the other: a rename, a move or a
/// removal can meet the other side's edit of the same instance, which is
/// where the structural conflicts come from.
fn edit_freely(tree: &mut Tree, rng: &mut Rng, counter: &mut u64, side: Side) -> Option<Op> {
    *counter += 1;
    let fresh = *counter;
    let movable = tree
        .nodes
        .iter()
        .filter(|(_, node)| node.parent.is_some())
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let of_class = |class: &str| {
        movable
            .iter()
            .filter(|id| tree.nodes[*id].class == class)
            .cloned()
            .collect::<Vec<_>>()
    };
    // Half the edits of a field go to the same few instances, so the two
    // sides meet on one instance often.
    let hot = movable.iter().take(3).cloned().collect::<HashSet<_>>();
    let choose = |rng: &mut Rng, candidates: Vec<String>| {
        let focused = candidates
            .iter()
            .filter(|id| hot.contains(*id))
            .cloned()
            .collect::<Vec<_>>();
        if !focused.is_empty() && rng.below(2) == 0 {
            rng.pick(&focused)
        } else {
            rng.pick(&candidates)
        }
    };
    let op = match rng.below(11) {
        0 | 1 => Op::Value(choose(rng, of_class("StringValue"))?, format!("v{fresh}")),
        10 => Op::Value(choose(rng, of_class("ObjectValue"))?, rng.pick(&movable)?),
        2 => {
            let id = choose(rng, movable.clone())?;
            let value = (rng.below(3) != 0).then(|| format!("a{fresh}"));
            Op::Attribute(id, format!("K{}", rng.below(2)), value)
        }
        3 => Op::Source(
            choose(rng, of_class("ModuleScript"))?,
            format!("return {fresh}\n"),
        ),
        4 => Op::Rename(choose(rng, movable.clone())?, format!("N{fresh}")),
        5 => {
            let id = rng.pick(&movable)?;
            let parent = rng.pick(&tree.nodes.keys().cloned().collect::<Vec<_>>())?;
            if tree.subtree(&id).contains(&parent)
                || tree.service_of(&parent) != tree.service_of(&id)
            {
                return None;
            }
            Op::Move(id, parent)
        }
        6 => Op::Delete(rng.pick(&movable)?),
        _ => {
            let parent = rng.pick(&tree.nodes.keys().cloned().collect::<Vec<_>>())?;
            let class = ["Folder", "StringValue", "ModuleScript", "ObjectValue"][rng.below(4)];
            let id = format!("{}{fresh}", if side == Side::Editor { "e" } else { "s" });
            Op::Add(id, new_node(class, parent, fresh, rng.pick(&movable)))
        }
    };
    op.apply(tree);
    Some(op)
}

fn other_side(side: ConflictSide) -> ConflictSide {
    match side {
        ConflictSide::Studio => ConflictSide::Editor,
        ConflictSide::Editor => ConflictSide::Studio,
    }
}

fn decisions(by_id: &BTreeMap<String, ConflictSide>) -> ConflictDecisions {
    ConflictDecisions {
        preference: ConflictPreference::None,
        by_id: by_id.clone(),
    }
}

/// What tells a conflict apart: its kind, file, instance, field.
fn conflict_identity(conflict: &Conflict) -> String {
    format!(
        "{:?} {} {:?} {:?} {}",
        conflict.kind,
        conflict.path,
        conflict.instance.as_ref().map(|instance| &instance.id),
        conflict.field,
        conflict.discriminator
    )
}

/// The ids of a run's open conflicts with what each is about; an id two
/// different conflicts share is a failure.
fn identities(merged: &Merged) -> Result<BTreeMap<String, String>, String> {
    let mut found = BTreeMap::new();
    for conflict in &merged.conflicts {
        let identity = conflict_identity(conflict);
        if let Some(other) = found.insert(conflict.id.clone(), identity.clone())
            && other != identity
        {
            return Err(format!(
                "two conflicts share the id {}: {other} and {identity}",
                conflict.id
            ));
        }
    }
    Ok(found)
}

/// Whether the trees of two runs differ only where the conflict decides:
/// its instance's field, its script's text, the instance a removal is about
/// with what hangs under it, and what those bring along: the parents they
/// stand under and the instances they refer to.
fn outside_scope(
    conflict: &Conflict,
    inputs: [&Tree; 2],
    decided: &Tree,
    flipped: &Tree,
) -> Option<String> {
    let ids = decided
        .nodes
        .keys()
        .chain(flipped.nodes.keys())
        .filter(|id| decided.nodes.get(*id) != flipped.nodes.get(*id))
        .cloned()
        .collect::<BTreeSet<_>>();
    let only = |id: &str, clear: &dyn Fn(&mut Node)| {
        let (Some(left), Some(right)) = (decided.nodes.get(id), flipped.nodes.get(id)) else {
            return false;
        };
        let (mut left, mut right) = (left.clone(), right.clone());
        clear(&mut left);
        clear(&mut right);
        left == right
    };
    // A reference to an instance one run holds and the other does not keeps
    // or loses its target with it.
    let gone = |id: &String| decided.nodes.contains_key(id) != flipped.nodes.contains_key(id);
    let follows_target = |id: &String| {
        only(id, &|node: &mut Node| node.value = None)
            && [decided, flipped]
                .into_iter()
                .any(|tree| tree.nodes[id].value.as_ref().is_some_and(gone))
    };
    let ids = ids
        .iter()
        .filter(|id| !follows_target(id))
        .cloned()
        .collect::<BTreeSet<_>>();
    let instance = conflict
        .instance
        .as_ref()
        .map(|instance| instance.id.as_str());
    let field = conflict.field.clone().unwrap_or_default();
    let trees = [inputs[0], inputs[1], decided, flipped];
    // What a decision about some instances brings along: the parents they
    // stand under and what they refer to, with theirs in turn.
    let reach = |seeds: BTreeSet<String>| {
        let mut reached = seeds;
        loop {
            let before = reached.len();
            for id in reached.clone() {
                for tree in trees {
                    reached.extend(tree.ancestors(&id));
                    if let Some(node) = tree.nodes.get(&id)
                        && node.class == "ObjectValue"
                        && let Some(target) = &node.value
                    {
                        reached.insert(target.clone());
                    }
                }
            }
            if reached.len() == before {
                return reached;
            }
        }
    };
    // The subject differs only in what the conflict is about; anything else
    // that differs is what the subject brings along.
    let subject_and_reach = |instance: &str, clear: &dyn Fn(&mut Node)| {
        let reached = reach(BTreeSet::from([instance.to_string()]));
        ids.iter().all(|id| {
            if id == instance {
                only(id, clear)
            } else {
                reached.contains(id)
            }
        })
    };
    let within = match (conflict.kind, instance) {
        // A cycle's members take one side's parents, whose chains come back
        // with them.
        (ConflictKind::Parent, Some(_)) if conflict.discriminator.starts_with("cycle") => {
            let (members, others): (Vec<_>, Vec<_>) = ids
                .iter()
                .partition(|id| only(id, &|node: &mut Node| node.parent = None));
            let reached = reach(members.into_iter().cloned().collect());
            others.iter().all(|id| reached.contains(*id))
        }
        (ConflictKind::Parent, Some(instance)) => {
            subject_and_reach(instance, &|node: &mut Node| node.parent = None)
        }
        (ConflictKind::Property | ConflictKind::Reference, Some(instance)) => {
            subject_and_reach(instance, &|node: &mut Node| node.value = None)
        }
        (ConflictKind::Attribute, Some(instance)) => {
            subject_and_reach(instance, &|node: &mut Node| {
                node.attributes.remove(&field);
            })
        }
        (ConflictKind::Name, Some(instance)) => {
            subject_and_reach(instance, &|node: &mut Node| node.name.clear())
        }
        (ConflictKind::Script, Some(instance)) => {
            subject_and_reach(instance, &|node: &mut Node| node.source = None)
        }
        // An instance kept against a removal comes back with what hangs
        // under it and what it brings along.
        (ConflictKind::Removal, Some(instance)) => {
            let subtree = [decided, flipped]
                .into_iter()
                .flat_map(|tree| tree.subtree(instance))
                .collect();
            ids.is_subset(&reach(subtree))
        }
        (_, None) => {
            let owners = [decided, flipped]
                .into_iter()
                .flat_map(|tree| {
                    script_paths(tree)
                        .into_iter()
                        .filter(|(_, path)| conflict_path(path) == conflict.path)
                        .map(|(id, _)| id)
                })
                .collect::<BTreeSet<_>>();
            ids.iter()
                .all(|id| owners.contains(id) && only(id, &|node: &mut Node| node.source = None))
        }
        _ => false,
    };
    (!within).then(|| {
        ids.iter()
            .map(|id| {
                format!(
                    "{id}: {:?} against {:?}",
                    decided.nodes.get(id),
                    flipped.nodes.get(id)
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    })
}

/// The fields both sides changed, each to its own value, on an instance all
/// three trees hold: (instance, kind, field).
fn contested_fields(
    shared: &Tree,
    editor: &Tree,
    studio: &Tree,
) -> Vec<(String, ConflictKind, Option<String>)> {
    let mut contested = Vec::new();
    for (id, base) in &shared.nodes {
        let (Some(files), Some(live)) = (editor.nodes.get(id), studio.nodes.get(id)) else {
            continue;
        };
        let both = |pick: &dyn Fn(&Node) -> Option<String>| {
            let (base, files, live) = (pick(base), pick(files), pick(live));
            files != base && live != base && files != live
        };
        if both(&|node| node.value.clone()) {
            contested.push((
                id.clone(),
                ConflictKind::Property,
                Some("Value".to_string()),
            ));
        }
        if both(&|node| Some(node.name.clone())) {
            contested.push((id.clone(), ConflictKind::Name, None));
        }
        let keys = [base, files, live]
            .into_iter()
            .flat_map(|node| node.attributes.keys())
            .collect::<BTreeSet<_>>();
        for key in keys {
            if both(&|node| node.attributes.get(key).cloned()) {
                contested.push((id.clone(), ConflictKind::Attribute, Some(key.clone())));
            }
        }
    }
    contested
}

fn same_outcome(what: &str, left: &Merged, right: &Merged) -> Result<(), String> {
    if left.tree != right.tree {
        return Err(format!(
            "{what}: the trees differ: {:?} against {:?}",
            left.tree, right.tree
        ));
    }
    if left.paths != right.paths {
        return Err(format!(
            "{what}: the files differ: {:?}",
            left.paths
                .symmetric_difference(&right.paths)
                .collect::<Vec<_>>()
        ));
    }
    for (side, left, right) in [
        ("files", &left.changes.editor, &right.changes.editor),
        ("Studio", &left.changes.studio, &right.changes.studio),
    ] {
        if left != right {
            return Err(format!(
                "{what}: the changes to {side} differ: {left:?} against {right:?}"
            ));
        }
    }
    Ok(())
}

/// A run with every conflict it met decided toward one side.
struct AllDecided {
    decided: BTreeMap<String, ConflictSide>,
    records: BTreeMap<String, Conflict>,
    run: Merged,
}

/// Decides every conflict a run meets toward `side`, run after run, until
/// none is open. `None` when a conflict no side settles stays open.
fn decide_all(
    base: Option<&Tree>,
    sides: [&Tree; 2],
    side: ConflictSide,
    rng: &mut Rng,
) -> Result<Option<AllDecided>, String> {
    let mut decided = BTreeMap::new();
    let mut records = BTreeMap::new();
    for _ in 0..12 {
        let run = merge_from(base, sides[0], sides[1], decisions(&decided), rng)
            .map_err(|error| format!("deciding {decided:?}: {error}"))?;
        if run.conflicts.is_empty() {
            return Ok(Some(AllDecided {
                decided,
                records,
                run,
            }));
        }
        for conflict in &run.conflicts {
            if !conflict.decidable {
                return Ok(None);
            }
            if decided.contains_key(&conflict.id) {
                return Err(format!(
                    "the decided conflict {} stays open: {}",
                    conflict.id, conflict.detail
                ));
            }
            decided.insert(conflict.id.clone(), side);
            records.insert(conflict.id.clone(), conflict.clone());
        }
    }
    Err("deciding every conflict met never ends".to_string())
}

/// Both sides' free edits of one shared tree of two services, Studio
/// answering some of the files' field edits with its own value.
fn edited_sides(
    shared: &Tree,
    rng: &mut Rng,
    counter: &mut u64,
    log: &mut Vec<String>,
) -> [Tree; 2] {
    let mut sides = [shared.clone(), shared.clone()];
    let mut files_ops = Vec::new();
    for _ in 0..1 + rng.below(6) {
        if let Some(op) = edit_freely(&mut sides[0], rng, counter, Side::Editor) {
            log.push(format!("Editor: {op:?}"));
            files_ops.push(op);
        }
    }
    for op in files_ops {
        *counter += 1;
        let fresh = *counter;
        let present = sides[1].nodes.keys().cloned().collect::<Vec<_>>();
        let answer = match op {
            Op::Value(id, _)
                if sides[1]
                    .nodes
                    .get(&id)
                    .is_some_and(|node| node.class == "ObjectValue") =>
            {
                let Some(target) = rng.pick(&present) else {
                    continue;
                };
                Op::Value(id, target)
            }
            Op::Value(id, _) => Op::Value(id, format!("v{fresh}")),
            Op::Attribute(id, key, _) => Op::Attribute(id, key, Some(format!("a{fresh}"))),
            Op::Source(id, _) => Op::Source(id, format!("return {fresh}\n")),
            Op::Rename(id, _) => Op::Rename(id, format!("N{fresh}")),
            _ => continue,
        };
        let id = match &answer {
            Op::Value(id, _) | Op::Attribute(id, ..) | Op::Source(id, _) | Op::Rename(id, _) => id,
            _ => continue,
        };
        if rng.below(2) == 0 && sides[1].nodes.contains_key(id) {
            log.push(format!("Studio: {answer:?}"));
            answer.apply(&mut sides[1]);
        }
    }
    for _ in 0..1 + rng.below(6) {
        if let Some(op) = edit_freely(&mut sides[1], rng, counter, Side::Studio) {
            log.push(format!("Studio: {op:?}"));
        }
    }
    sides
}

/// Leaving one conflict undecided applies nothing for it: it stays open,
/// unless the other decisions made it moot, and then neither of its own
/// decisions changes the run.
fn undecided_stays_open(
    base: Option<&Tree>,
    sides: [&Tree; 2],
    open: &[Conflict],
    rng: &mut Rng,
) -> Result<(), String> {
    let decidable = open
        .iter()
        .filter(|conflict| conflict.decidable)
        .collect::<Vec<_>>();
    let mut picks = decidable.clone();
    rng.shuffle(&mut picks);
    for undecided in picks.into_iter().take(3) {
        let others = decidable
            .iter()
            .filter(|conflict| conflict.id != undecided.id)
            .map(|conflict| (conflict.id.clone(), ConflictSide::Studio))
            .collect::<BTreeMap<_, _>>();
        let what = format!("leaving {} ({}) undecided", undecided.id, undecided.detail);
        let partial = merge_from(base, sides[0], sides[1], decisions(&others), rng)
            .map_err(|error| format!("{what}: {error}"))?;
        if partial.conflicts.iter().any(|open| open.id == undecided.id) {
            continue;
        }
        for side in [ConflictSide::Studio, ConflictSide::Editor] {
            let mut with = others.clone();
            with.insert(undecided.id.clone(), side);
            let decided = merge_from(base, sides[0], sides[1], decisions(&with), rng)
                .map_err(|error| format!("{what}, then {side:?}: {error}"))?;
            same_outcome(
                &format!("{what}: it was settled without its own decision, as {side:?} shows"),
                &partial,
                &decided,
            )?;
        }
    }
    Ok(())
}

/// The laws of deciding conflicts one by one, over edits both sides made
/// freely to one shared tree of two services, or to no shared tree at all.
fn run_decision_seed(seed: u64, log: &mut Vec<String>) -> Result<(), String> {
    let mut rng = Rng(seed);
    let mut counter = 0;
    let mut shared = first_tree(&mut rng, &mut counter);
    add_second_service(&mut shared, &mut rng, &mut counter);
    let first_pairing = rng.below(4) == 0;
    log.push(format!("first pairing: {first_pairing}"));
    let base = (!first_pairing).then_some(&shared);
    let sides = edited_sides(&shared, &mut rng, &mut counter, log);
    let [editor, studio] = &sides;

    // Ids are unique and stable: a second run, with Studio's siblings in
    // another order, gives every conflict the id it had.
    let open = merge_from(base, editor, studio, ConflictPreference::None, &mut rng)?;
    let again = merge_from(base, editor, studio, ConflictPreference::None, &mut rng)?;
    let ids = identities(&open)?;
    if identities(&again)? != ids {
        return Err(format!(
            "the ids changed between two runs: {ids:?} against {:?}",
            identities(&again)?
        ));
    }

    // A decision for an id no conflict of the run has is refused, alone or
    // among decisions for real ones, and it names only the stale one.
    let stale = (0u32..)
        .map(|value| format!("{value:08x}"))
        .find(|id| !ids.contains_key(id))
        .unwrap();
    let mut mixed = open
        .conflicts
        .iter()
        .filter(|conflict| conflict.decidable)
        .map(|conflict| (conflict.id.clone(), ConflictSide::Editor))
        .collect::<BTreeMap<_, _>>();
    mixed.insert(stale.clone(), ConflictSide::Studio);
    for by_id in [
        BTreeMap::from([(stale.clone(), ConflictSide::Studio)]),
        mixed,
    ] {
        match merge_from(base, editor, studio, decisions(&by_id), &mut rng) {
            Err(error) if error.contains(&stale) => {
                if let Some(named) = by_id
                    .keys()
                    .find(|id| **id != stale && error.contains(id.as_str()))
                {
                    return Err(format!("a stale decision's error names {named}: {error}"));
                }
            }
            Err(error) => return Err(format!("a stale decision failed without its id: {error}")),
            Ok(_) => return Err(format!("the stale decision {stale} was taken")),
        }
    }

    // A field both sides changed is an open conflict while nothing decides
    // it, so the run applies nothing.
    if !first_pairing {
        for (id, kind, field) in contested_fields(&shared, editor, studio) {
            if !open.conflicts.iter().any(|conflict| {
                conflict.kind == kind
                    && conflict.field == field
                    && conflict
                        .instance
                        .as_ref()
                        .is_some_and(|instance| instance.id == id)
            }) {
                return Err(format!(
                    "both sides changed {kind:?} {field:?} of {id} and no conflict is open"
                ));
            }
        }
    }
    if open.conflicts.is_empty() {
        return Ok(());
    }
    undecided_stays_open(base, [editor, studio], &open.conflicts, &mut rng)?;

    // A run with every conflict decided toward one side equals the run
    // that prefers that side.
    let side = if rng.below(2) == 0 {
        ConflictSide::Studio
    } else {
        ConflictSide::Editor
    };
    let Some(AllDecided {
        decided,
        records,
        run: all,
    }) = decide_all(base, [editor, studio], side, &mut rng)?
    else {
        return Ok(());
    };
    let preference = match side {
        ConflictSide::Studio => ConflictPreference::Studio,
        ConflictSide::Editor => ConflictPreference::Editor,
    };
    let preferred = merge_from(base, editor, studio, preference, &mut rng)?;
    if !preferred.conflicts.is_empty() {
        return Ok(());
    }
    same_outcome(
        &format!("every conflict decided for {side:?}"),
        &all,
        &preferred,
    )?;

    // Deciding one conflict for the other side changes only what that
    // conflict is about.
    let Ok(all_tree) = &all.tree else {
        return Ok(());
    };
    let mut flips = decided.keys().cloned().collect::<Vec<_>>();
    rng.shuffle(&mut flips);
    for id in flips.into_iter().take(8) {
        let mut flipped = decided.clone();
        flipped.insert(id.clone(), other_side(side));
        // A decision whose conflict only the flipped one brought about is
        // stale now, and the run refuses it: nothing to compare.
        let run = match merge_from(base, editor, studio, decisions(&flipped), &mut rng) {
            Err(error) if error.contains("No open conflict has the id") => continue,
            run => run.map_err(|error| format!("{id} flipped: {error}"))?,
        };
        if !run.conflicts.is_empty() {
            continue;
        }
        let conflict = &records[&id];
        let tree = run
            .tree
            .as_ref()
            .map_err(|error| format!("{id} flipped: {error}"))?;
        if let Some(outside) = outside_scope(conflict, [editor, studio], all_tree, tree) {
            return Err(format!(
                "deciding {id} ({}, {}) the other way changed more than it: {outside}",
                conflict.detail,
                conflict_identity(conflict)
            ));
        }
    }
    Ok(())
}

#[test]
fn seeded_decisions_by_id_keep_the_laws_of_a_conflict_record() {
    run_seeds(300, run_decision_seed);
}

/// The files the daemon holds as what Studio holds, at the Studio version
/// its proof was taken at: every change Studio takes, its own pushes
/// included, moves the version, as it retires a proof.
#[cfg(any(windows, target_os = "macos"))]
struct Held {
    project: ProjectSnapshot,
    version: u64,
}

#[cfg(any(windows, target_os = "macos"))]
static BASELINE_OUTCOMES: [std::sync::atomic::AtomicUsize; 4] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; 4];

/// One edit of the kind either side makes: a value set or cleared, an
/// attribute set or removed, a script's source, or a rename.
#[cfg(any(windows, target_os = "macos"))]
fn edit_one(tree: &mut Tree, rng: &mut Rng, counter: &mut u64) -> String {
    *counter += 1;
    let ids = tree
        .nodes
        .keys()
        .filter(|id| *id != ROOT)
        .cloned()
        .collect::<Vec<_>>();
    let id = rng.pick(&ids).unwrap();
    let node = tree.nodes.get_mut(&id).unwrap();
    let kind = match rng.below(6) {
        0 if node.class == "StringValue" => {
            node.value = Some(format!("v{counter}"));
            "value"
        }
        1 if node.class == "StringValue" => {
            node.value = None;
            "value cleared"
        }
        2 if node.class == "ModuleScript" => {
            node.source = Some(format!("return {counter}\n"));
            "source"
        }
        3 => {
            node.attributes.clear();
            "attributes cleared"
        }
        4 if rng.below(3) == 0 => {
            node.name = format!("R{counter}");
            "rename"
        }
        _ => {
            node.attributes
                .insert(format!("A{}", rng.below(3)), format!("a{counter}"));
            "attribute"
        }
    };
    format!("{kind} of {id}")
}

/// What a push of chosen instances or scripts writes in Studio: each value
/// the files' row holds, the whole attribute map, and the named scripts'
/// sources. It resets no value and moves or renames nothing.
#[cfg(any(windows, target_os = "macos"))]
fn push_chosen(studio: &mut Tree, files: &Tree, chosen: &[String], scripts: &[String]) {
    for id in chosen {
        let (Some(node), Some(wanted)) = (studio.nodes.get_mut(id), files.nodes.get(id)) else {
            continue;
        };
        if wanted.value.is_some() {
            node.value.clone_from(&wanted.value);
        }
        node.attributes.clone_from(&wanted.attributes);
    }
    for id in scripts {
        if let (Some(node), Some(wanted)) = (studio.nodes.get_mut(id), files.nodes.get(id)) {
            node.source.clone_from(&wanted.source);
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn settled_of(snapshot: &ProjectSnapshot) -> BTreeMap<PathBuf, PublishEntryState> {
    snapshot
        .entries
        .iter()
        .map(|(path, entry)| {
            let state = match entry {
                SnapshotEntry::Directory => PublishEntryState::Directory,
                SnapshotEntry::File(bytes) => crate::snapshot::export::file_publish_state(bytes),
                SnapshotEntry::Symlink { target, .. } => PublishEntryState::Symlink(target.clone()),
            };
            (path.clone(), state)
        })
        .collect()
}

/// A targeted push: `bs`/`ba` (instances with their scripts) or `ps FILE`
/// (scripts alone), under the held baseline when its proof still holds.
#[cfg(any(windows, target_os = "macos"))]
fn targeted_step(
    rng: &mut Rng,
    (studio, files, version): (&mut Tree, &Tree, &mut u64),
    held: Option<Held>,
    log: &mut Vec<String>,
) -> Option<Held> {
    let scripts_by_id = script_paths(files);
    let ids = files
        .nodes
        .keys()
        .filter(|id| *id != ROOT)
        .cloned()
        .collect::<Vec<_>>();
    let store = service_settings_path(&Path::new("src").join(SERVICE));
    let exact = rng.below(2) == 0;
    let (targets, scripts) = if rng.below(3) == 0 {
        let scripts = scripts_by_id.keys().cloned().collect::<Vec<_>>();
        (
            Vec::new(),
            rng.pick(&scripts).into_iter().collect::<Vec<_>>(),
        )
    } else {
        let mut targets = (0..1 + rng.below(2))
            .filter_map(|_| rng.pick(&ids))
            .collect::<Vec<_>>();
        targets.sort();
        targets.dedup();
        let scripts = targets
            .iter()
            .filter(|id| scripts_by_id.contains_key(*id))
            .cloned()
            .collect::<Vec<_>>();
        (targets, scripts)
    };
    let chosen = if exact {
        targets.clone()
    } else {
        targets.iter().flat_map(|id| files.subtree(id)).collect()
    };
    let mut paths = scripts
        .iter()
        .map(|id| scripts_by_id[id].clone())
        .collect::<HashSet<_>>();
    if !targets.is_empty() {
        paths.insert(store);
    }
    let held = held.filter(|held| held.version == *version);
    let before = files.snapshot(None);
    push_chosen(studio, files, &chosen, &scripts);
    *version += 1;
    let concurrent = rng.below(5) == 0;
    log.push(format!(
        "targeted push of {targets:?} exact={exact} scripts={scripts:?} held={} concurrent={concurrent}",
        held.is_some()
    ));
    let held = held?;
    let mut summary = Map::new();
    summary.insert("ok".into(), json!(true));
    summary.insert("sourceVerified".into(), json!(scripts.len()));
    summary.insert("sourceVerifyFailed".into(), json!(0));
    if !targets.is_empty() {
        summary.insert("fieldVerifiedServices".into(), json!([SERVICE]));
    }
    let pushed = TargetedPush {
        paths,
        ids: targets.into_iter().collect(),
        previous_ids: HashSet::new(),
        exact,
        whole_rows: true,
    };
    let kept = targeted_baseline(&held.project, (&before, &before), &summary, &pushed, |_| {
        Some(SERVICE.to_string())
    });
    BASELINE_OUTCOMES[usize::from(kept.is_ok())].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    log.push(format!("  baseline: {:?}", kept.as_ref().err()));
    if concurrent {
        // Studio's generation fence turns the lease down.
        *version += 1;
        return None;
    }
    kept.ok().map(|project| Held {
        project,
        version: *version,
    })
}

/// A pull: answered from the baseline, or a capture that keeps one where
/// the files are still what it wrote and Studio did not change meanwhile.
#[cfg(any(windows, target_os = "macos"))]
fn pull_step(
    rng: &mut Rng,
    (studio, files, version): (&mut Tree, &mut Tree, &mut u64),
    held: Option<Held>,
    (counter, log): (&mut u64, &mut Vec<String>),
) -> Result<Option<Held>, String> {
    let current = files.snapshot(None);
    if let Some(held) = held.filter(|held| held.version == *version && held.project == current) {
        log.push("pull answered unchanged".into());
        if let Some(difference) = files.difference(studio) {
            return Err(format!(
                "a pull answered unchanged where a capture writes: {difference}"
            ));
        }
        return Ok(Some(held));
    }
    let proof = *version;
    *files = studio.clone();
    let settled = settled_of(&files.snapshot(None));
    if rng.below(4) == 0 {
        let edit = edit_one(files, rng, counter);
        log.push(format!("pull, then a file edit: {edit}"));
    }
    if rng.below(4) == 0 {
        let edit = edit_one(studio, rng, counter);
        *version += 1;
        log.push(format!("pull, with a Studio edit: {edit}"));
    }
    let project = files.snapshot(None);
    let kept = publish_settled(&project, &settled) && proof == *version;
    BASELINE_OUTCOMES[2 + usize::from(kept)].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(kept.then_some(Held {
        project,
        version: *version,
    }))
}

#[cfg(any(windows, target_os = "macos"))]
fn run_baseline_seed(seed: u64, log: &mut Vec<String>) -> Result<(), String> {
    let mut rng = Rng(seed);
    let mut counter = 0;
    let mut studio = first_tree(&mut rng, &mut counter);
    let mut files = studio.clone();
    let mut version = 0;
    let mut held = None;
    for step in 0..40 {
        match rng.below(6) {
            0 => {
                let edit = edit_one(&mut files, &mut rng, &mut counter);
                log.push(format!("file edit: {edit}"));
            }
            1 => {
                let edit = edit_one(&mut studio, &mut rng, &mut counter);
                version += 1;
                log.push(format!("Studio edit: {edit}"));
            }
            2 => {
                held = pull_step(
                    &mut rng,
                    (&mut studio, &mut files, &mut version),
                    held,
                    (&mut counter, log),
                )?;
            }
            3 => {
                let current = files.snapshot(None);
                if held
                    .as_ref()
                    .is_some_and(|held: &Held| held.version == version && held.project == current)
                {
                    log.push("full push answered unchanged".into());
                    if let Some(difference) = files.difference(&studio) {
                        return Err(format!(
                            "a full push answered unchanged where it writes: {difference}"
                        ));
                    }
                } else {
                    log.push("full push".into());
                    studio = files.clone();
                    version += 1;
                    held = Some(Held {
                        project: current,
                        version,
                    });
                }
            }
            _ => {
                held = targeted_step(
                    &mut rng,
                    (&mut studio, &files, &mut version),
                    held.take(),
                    log,
                );
            }
        }
        if let Some(held) = held.as_ref().filter(|held| held.version == version) {
            let tree = Tree::read(&held.project)?;
            if let Some(difference) = tree.difference(&studio) {
                return Err(format!(
                    "step {step}: the held baseline is not what Studio holds: {difference}"
                ));
            }
        }
    }
    Ok(())
}

/// The services of the scoped-pull model, each one root with one store:
/// `root`, `root2` and `root3`, which holds Terrain.
#[cfg(any(windows, target_os = "macos"))]
const SCOPED_SERVICES: [&str; 3] = ["ReplicatedStorage", "ServerStorage", "Workspace"];

#[cfg(any(windows, target_os = "macos"))]
const SCOPED_OUTCOMES: [&str; 8] = [
    "scoped pull kept a baseline",
    "scoped pull published and kept none",
    "scoped pull fell back to a whole pull",
    "pull not scoped: Studio could not say where it moved",
    "scope took a store referring into a moved service",
    "pull not scoped: it covers every service",
    "a scoped pull's baseline answered a later pull",
    "scope left out a store referring into a service moved only in values",
];

#[cfg(any(windows, target_os = "macos"))]
static SCOPED_COUNTS: [std::sync::atomic::AtomicUsize; 8] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; 8];

#[cfg(any(windows, target_os = "macos"))]
fn scoped_outcome(index: usize) {
    SCOPED_COUNTS[index].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(any(windows, target_os = "macos"))]
const INSTANCE_OUTCOMES: [&str; 10] = [
    "a pull of instances wrote",
    "a pull of instances found nothing to write",
    "a pull of instances met a difference Studio did not name",
    "a pull of instances met a named reference",
    "a pull of instances met a change outside its capture",
    "a pull of instances wrote several",
    "a pull of instances wrote one touched under another",
    "a pull of instances wrote with a change since its proof",
    "a pull of instances wrote over a store a pull aligned",
    "a joint carried a change to an instance Studio named nothing of",
];

#[cfg(any(windows, target_os = "macos"))]
static INSTANCE_COUNTS: [std::sync::atomic::AtomicUsize; 10] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; 10];

#[cfg(any(windows, target_os = "macos"))]
fn instance_outcome(index: usize) {
    INSTANCE_COUNTS[index].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// What the plugin lays one change to: each service it touched with how
/// (`values`, `children`, `subtree`, or `whole`) and, for values it names,
/// the instance and what of it changed; or `None` where no service answers
/// for it.
#[cfg(any(windows, target_os = "macos"))]
type Laid = Option<Vec<(String, &'static str, Option<(String, &'static str)>)>>;

/// What Studio touched since a proof in each service it moved in.
#[cfg(any(windows, target_os = "macos"))]
type Touched = BTreeMap<String, Option<Vec<TouchedInstance>>>;

/// Removed subtrees by when they left, which an undo brings back with their
/// identities.
#[cfg(any(windows, target_os = "macos"))]
type Removed = Vec<(u64, Vec<(String, Node)>)>;

/// Studio in the scoped-pull model, with what the plugin lays each change
/// to, by the version the change made.
#[cfg(any(windows, target_os = "macos"))]
struct ScopedStudio {
    tree: Tree,
    changes: Vec<Laid>,
    removed: Removed,
    /// When Studio took its latest proof; the plugin lays an instance that
    /// arrives in a service by it.
    proof: u64,
}

/// When the model made an instance: its id carries the counter then.
#[cfg(any(windows, target_os = "macos"))]
fn created(id: &str) -> u64 {
    id.trim_start_matches(|character: char| !character.is_ascii_digit())
        .parse()
        .unwrap_or(0)
}

#[cfg(any(windows, target_os = "macos"))]
impl ScopedStudio {
    fn version(&self) -> u64 {
        self.changes.len() as u64
    }

    /// The plugin's answer for the proof taken at `proof`, as it sends it:
    /// an instance whose values it named stands where it stands now, and
    /// one that left its service since is left out.
    fn answer_since(&self, proof: u64) -> Value {
        let mut kinds = BTreeMap::<String, Vec<&str>>::new();
        let mut named = BTreeMap::<(String, String), BTreeSet<&str>>::new();
        for change in &self.changes[proof as usize..] {
            let Some(touched) = change else {
                return json!({"unknown": "no service answers for a change"});
            };
            for (service, kind, instance) in touched {
                match instance {
                    Some((id, field)) => {
                        named
                            .entry((service.clone(), id.clone()))
                            .or_default()
                            .insert(*field);
                    }
                    None => kinds.entry(service.clone()).or_default().push(*kind),
                }
            }
        }
        let services = (kinds.keys())
            .chain(named.keys().map(|(service, _)| service))
            .cloned()
            .collect::<BTreeSet<_>>();
        let touched = services
            .iter()
            .map(|service| {
                let unnamed = kinds.get(service).map(Vec::as_slice).unwrap_or_default();
                let mut entries = (unnamed.iter())
                    .map(|kind| json!({"pathSegments": [service], "pathOrdinals": [1], "kind": kind}))
                    .collect::<Vec<_>>();
                for ((at, id), fields) in &named {
                    if at != service || self.tree.service_of(id) != Some(service.as_str()) {
                        continue;
                    }
                    let segments = self.tree.name_path(id);
                    entries.push(json!({
                        "pathSegments": segments, "pathOrdinals": vec![1; segments.len()],
                        "kind": "values", "debugId": id,
                        "properties": fields.iter().filter(|field| !field.starts_with('@')).collect::<Vec<_>>(),
                        "attributes": fields.contains("@attributes"), "tags": fields.contains("@tags"),
                    }));
                }
                let entries = if unnamed.contains(&"whole") || entries.is_empty() {
                    json!("whole")
                } else {
                    Value::Array(entries)
                };
                (service.clone(), entries)
            })
            .collect::<Map<_, _>>();
        json!({"services": services, "touched": touched})
    }

    fn edit(&mut self, rng: &mut Rng, counter: &mut u64, log: &mut Vec<String>) {
        if rng.below(12) == 0 {
            log.push("Studio: the observation ended".into());
            self.changes.push(None);
            return;
        }
        let (what, touched) = scoped_edit(
            &mut self.tree,
            (&mut self.removed, self.proof),
            rng,
            counter,
        );
        log.push(format!("Studio: {what}, laid to {touched:?}"));
        self.changes.push(touched);
    }

    fn take_proof(&mut self, counter: u64) -> u64 {
        self.proof = counter;
        self.version()
    }
}

/// Workspace with its Terrain, and ObjectValues between it and the other
/// services.
#[cfg(any(windows, target_os = "macos"))]
fn add_workspace(tree: &mut Tree, rng: &mut Rng, counter: &mut u64) {
    for (id, name, parent, value) in [
        ("root3", "Workspace", None, None),
        ("terrain", "Terrain", Some("root3"), Some("voxels")),
    ] {
        tree.nodes.insert(
            id.to_string(),
            Node {
                name: name.to_string(),
                class: name.to_string(),
                parent: parent.map(str::to_string),
                value: value.map(str::to_string),
                attributes: BTreeMap::new(),
                source: None,
            },
        );
    }
    let elsewhere = tree
        .nodes
        .keys()
        .filter(|id| !id.starts_with("root") && *id != "terrain")
        .cloned()
        .collect::<Vec<_>>();
    for _ in 0..3 + rng.below(4) {
        *counter += 1;
        let parent = if rng.below(3) == 0 {
            rng.pick(&elsewhere)
        } else {
            rng.pick(&tree.subtree("root3"))
                .filter(|id| id.as_str() != "terrain")
        };
        let target = rng.pick(&tree.nodes.keys().cloned().collect::<Vec<_>>());
        let class = ["Folder", "ObjectValue", "ObjectValue", "StringValue"][rng.below(4)];
        let node = new_node(
            class,
            parent.unwrap_or_else(|| "root3".to_string()),
            *counter,
            target,
        );
        tree.nodes.insert(format!("w{counter}"), node);
    }
}

/// One edit of the scoped model, with what the plugin lays it to under the
/// proof taken at `proof`.
#[cfg(any(windows, target_os = "macos"))]
fn scoped_edit(
    tree: &mut Tree,
    removed: (&mut Removed, u64),
    rng: &mut Rng,
    counter: &mut u64,
) -> (String, Laid) {
    loop {
        if let Some(edit) = try_scoped_edit(tree, (&mut *removed.0, removed.1), rng, counter) {
            return edit;
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn try_scoped_edit(
    tree: &mut Tree,
    (removed, proof): (&mut Removed, u64),
    rng: &mut Rng,
    counter: &mut u64,
) -> Option<(String, Laid)> {
    *counter += 1;
    let fresh = *counter;
    let movable = tree
        .nodes
        .iter()
        .filter(|(_, node)| node.parent.is_some() && node.class != "Terrain")
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let all = tree.nodes.keys().cloned().collect::<Vec<_>>();
    let laid = |tree: &Tree, id: &str, kind: &'static str| {
        Some(vec![(tree.service_of(id)?.to_string(), kind, None)])
    };
    // Values the plugin names: of the instance, or, as a model of a change
    // it misses, of the instance's parent. A capture holds no reference, so
    // a pull of instances counts on Studio naming each.
    let misnamed = rng.below(6) == 0;
    let named = |tree: &Tree, id: &str, field: &'static str, misnamed: bool| {
        let parent = tree.nodes[id].parent.clone().filter(|parent| {
            misnamed && tree.nodes[parent].parent.is_some() && tree.nodes[parent].class != "Terrain"
        });
        let id = parent.unwrap_or_else(|| id.to_string());
        Some(vec![(
            tree.service_of(&id)?.to_string(),
            "values",
            Some((id, field)),
        )])
    };
    let edit = match rng.below(11) {
        0..=4 => {
            let id = rng.pick(&movable)?;
            let class = tree.nodes[&id].class.clone();
            let node = tree.nodes.get_mut(&id)?;
            let (what, field) = match (rng.below(5), class.as_str()) {
                (0, "StringValue") => {
                    node.value = Some(format!("v{fresh}"));
                    // A joint carries the change to another instance of the
                    // service with no event of its own; the plugin names
                    // nothing of an instance with a joint.
                    let subtree = tree.subtree(&id);
                    let service = tree.service_of(&id).map(str::to_string);
                    let beside = (tree.nodes.iter())
                        .filter(|(other, node)| {
                            node.class == "StringValue"
                                && !subtree.contains(other)
                                && tree.service_of(other).map(str::to_string) == service
                        })
                        .map(|(other, _)| other.clone())
                        .collect::<Vec<_>>();
                    if let Some(other) = rng.pick(&beside).filter(|_| rng.below(4) == 0) {
                        tree.nodes.get_mut(&other)?.value = Some(format!("j{fresh}"));
                        instance_outcome(9);
                        let touched = laid(tree, &id, "values");
                        return Some((format!("value of {id}, carried to {other}"), touched));
                    }
                    ("value", Some("Value"))
                }
                (0, "ObjectValue") => {
                    node.value = rng.pick(&all);
                    ("reference", Some("Value"))
                }
                (1, "ModuleScript") => {
                    node.source = Some(format!("return {fresh}\n"));
                    ("source", None)
                }
                (2, _) => {
                    node.name = format!("R{fresh}");
                    let touched = laid(tree, &id, "children");
                    return Some((format!("rename of {id}"), touched));
                }
                (3, _) => {
                    let key = format!("A{}", rng.below(2));
                    if rng.below(3) == 0 {
                        node.attributes.remove(&key);
                    } else {
                        node.attributes.insert(key, format!("a{fresh}"));
                    }
                    ("attribute", Some("@attributes"))
                }
                (4, _) => {
                    // A tag is an attribute here; the first or last use of a
                    // tag anywhere is the tag's creation or retirement.
                    let key = format!("Tag{}", rng.below(2));
                    if node.attributes.remove(&key).is_none() {
                        node.attributes.insert(key.clone(), String::new());
                    }
                    let uses = tree
                        .nodes
                        .values()
                        .filter(|node| node.attributes.contains_key(&key))
                        .count();
                    let first_or_last =
                        uses == 0 || (uses == 1 && tree.nodes[&id].attributes.contains_key(&key));
                    let touched = if first_or_last {
                        None
                    } else {
                        named(tree, &id, "@attributes", misnamed)
                    };
                    return Some((format!("tag {key} of {id}"), touched));
                }
                _ => return None,
            };
            let touched = match field {
                Some(field) => named(tree, &id, field, misnamed && what != "reference"),
                None => laid(tree, &id, "values"),
            };
            (format!("{what} of {id}"), touched)
        }
        5 => {
            tree.nodes.get_mut("terrain")?.value = Some(format!("voxels{fresh}"));
            (
                "Terrain voxels".to_string(),
                laid(tree, "terrain", "values"),
            )
        }
        6 | 7 => {
            // A move within a service has no old parent to name.
            let id = rng.pick(&movable)?;
            let parent = rng.pick(&all)?;
            if parent == "terrain" || tree.subtree(&id).contains(&parent) {
                return None;
            }
            let from = tree.service_of(&id)?.to_string();
            Op::Move(id.clone(), parent.clone()).apply(tree);
            let to = tree.service_of(&id)?.to_string();
            let touched = if from == to {
                vec![(to, "whole", None)]
            } else {
                vec![(from, "children", None), (to, "subtree", None)]
            };
            (format!("move of {id} into {parent}"), Some(touched))
        }
        8 => {
            let id = rng.pick(&movable)?;
            let touched = laid(tree, &id, "children");
            removed.push((
                fresh,
                tree.subtree(&id)
                    .into_iter()
                    .map(|id| {
                        let node = tree.nodes[&id].clone();
                        (id, node)
                    })
                    .collect(),
            ));
            Op::Delete(id.clone()).apply(tree);
            (format!("removal of {id}"), touched)
        }
        9 => {
            let parent = rng.pick(&all)?;
            if parent == "terrain" {
                return None;
            }
            let class = ["Folder", "StringValue", "ModuleScript", "ObjectValue"][rng.below(4)];
            let node = new_node(class, parent.clone(), fresh, rng.pick(&all));
            tree.nodes.insert(format!("s{fresh}"), node);
            let touched = if fresh > proof {
                laid(tree, &parent, "subtree")
            } else {
                None
            };
            (format!("new {class} in {parent}"), touched)
        }
        _ => {
            // An undo brings a removed subtree back with its identities, so
            // a reference another store held to it while it was away is
            // written again. The plugin lays it to its service only where
            // it left since the proof, or was made since and so was named
            // by no store before.
            let index = rng.below(removed.len().max(1));
            let (_, subtree) = removed.get(index)?;
            let parent = subtree[0].1.parent.clone()?;
            if !tree.nodes.contains_key(&parent)
                || subtree.iter().any(|(id, _)| tree.nodes.contains_key(id))
            {
                return None;
            }
            let (left, subtree) = removed.remove(index);
            let what = format!("undo of the removal of {}", subtree[0].0);
            let placed = left > proof || subtree.iter().all(|(id, _)| created(id) > proof);
            tree.nodes.extend(subtree);
            let touched = if placed {
                laid(tree, &parent, "subtree")
            } else {
                None
            };
            (what, touched)
        }
    };
    Some(edit)
}

/// A whole pull of the scoped model: the files become Studio, and keep a
/// baseline where neither side changed while it wrote.
#[cfg(any(windows, target_os = "macos"))]
fn scoped_whole_pull(
    rng: &mut Rng,
    (studio, files): (&mut ScopedStudio, &mut Tree),
    (counter, log): (&mut u64, &mut Vec<String>),
) -> Option<Held> {
    log.push("whole pull".into());
    let proof = studio.take_proof(*counter);
    *files = studio.tree.clone();
    let settled = settled_of(&files.snapshot(None));
    if rng.below(5) == 0 {
        let (what, _) = scoped_edit(files, (&mut Vec::new(), 0), rng, counter);
        log.push(format!("  then a file edit: {what}"));
    }
    if rng.below(5) == 0 {
        studio.edit(rng, counter, log);
    }
    let project = files.snapshot(None);
    (publish_settled(&project, &settled) && proof == studio.version()).then_some(Held {
        project,
        version: proof,
    })
}

/// A pull of the scoped model. Under a held baseline whose files are
/// unchanged it captures only the services Studio moved in since and the
/// stores referring into them, decided by the code a pull runs, and its
/// files must be byte for byte those of a whole pull at its capture.
#[cfg(any(windows, target_os = "macos"))]
fn scoped_pull_step(
    rng: &mut Rng,
    (studio, files): (&mut ScopedStudio, &mut Tree),
    held: Option<Held>,
    (counter, from_scope, log): (&mut u64, &mut bool, &mut Vec<String>),
) -> Result<Option<Held>, String> {
    let current = files.snapshot(None);
    let Some(held) = held.filter(|held| held.project == current) else {
        *from_scope = false;
        return Ok(scoped_whole_pull(rng, (studio, files), (counter, log)));
    };
    if held.version == studio.version() {
        log.push("pull answered unchanged".into());
        if let Some(difference) = files.difference(&studio.tree) {
            return Err(format!(
                "a pull answered unchanged where a capture writes: {difference}"
            ));
        }
        if *from_scope {
            scoped_outcome(6);
        }
        return Ok(Some(held));
    }
    *from_scope = false;
    let stores = SCOPED_SERVICES
        .iter()
        .map(|service| {
            let store = service_settings_path(&Path::new("src").join(service));
            (service.to_string(), store)
        })
        .collect::<BTreeMap<_, _>>();
    let (moved, paths_moved, touched) = match ProofScope::parse(&studio.answer_since(held.version))
    {
        ProofScope::Unknown(reason) => {
            log.push(format!("pull not scoped: {reason}"));
            scoped_outcome(3);
            return Ok(scoped_whole_pull(rng, (studio, files), (counter, log)));
        }
        ProofScope::Services {
            services,
            touched,
            values_only,
        } => {
            let paths_moved = paths_moved_in_with(&services, &touched, &values_only);
            (services, paths_moved, touched)
        }
    };
    let referring = referring_services_for_changes(
        &held.project,
        &stores,
        &structural_changes_from_touched(&paths_moved, &touched),
    )
    .map_err(|error| format!("{error:#}"))?;
    if !referring.is_empty() {
        scoped_outcome(4);
    }
    let scope = moved.union(&referring).cloned().collect::<BTreeSet<_>>();
    let whole_moved: StructuralChanges = moved
        .iter()
        .map(|service| (service.clone(), vec![StructuralPathChange::WholeService]))
        .collect();
    if !referring_services_for_changes(&held.project, &stores, &whole_moved)
        .map_err(|error| format!("{error:#}"))?
        .is_subset(&scope)
    {
        scoped_outcome(7);
    }
    log.push(format!(
        "pull scoped to {scope:?}: Studio moved {moved:?}, structure in {paths_moved:?}, stores referring to it {referring:?}"
    ));
    if scope.len() >= SCOPED_SERVICES.len() {
        scoped_outcome(5);
        return Ok(scoped_whole_pull(rng, (studio, files), (counter, log)));
    }
    let proof = studio.take_proof(*counter);
    let at_proof = studio.tree.snapshot(None);
    if rng.below(4) == 0 {
        log.push("  before the capture:".into());
        studio.edit(rng, counter, log);
    }
    let whole = studio.tree.snapshot(None);
    let paths = scope
        .iter()
        .map(|service| Path::new("src").join(service))
        .collect::<Vec<_>>();
    let in_scope = ScopeSet::new(&paths);
    let under_scope = |snapshot: &ProjectSnapshot| ProjectSnapshot {
        entries: snapshot
            .entries
            .iter()
            .filter(|(path, _)| in_scope.contains(path))
            .map(|(path, entry)| (path.clone(), entry.clone()))
            .collect(),
    };
    let captured = under_scope(&whole);
    if rng.below(4) == 0 {
        log.push("  after the capture:".into());
        studio.edit(rng, counter, log);
    }
    // An attribute of an instance Studio named, changed after the capture.
    let named = (touched.values().flatten().flatten())
        .filter_map(|entry| Some(entry.fields.as_ref()?.debug_id.clone()))
        .collect::<Vec<_>>();
    if let Some(id) = rng.pick(&named).filter(|_| rng.below(3) == 0)
        && let Some(service) = studio.tree.service_of(&id).map(str::to_string)
    {
        *counter += 1;
        let value = format!("a{counter}");
        studio
            .tree
            .nodes
            .get_mut(&id)
            .unwrap()
            .attributes
            .insert("A0".into(), value);
        log.push(format!("  after the capture: attribute of {id}"));
        (studio.changes).push(Some(vec![(service, "values", Some((id, "@attributes")))]));
    }
    let premise = if studio.version() == proof {
        ScopedPremise::Held
    } else {
        let now = ProofScope::parse(&studio.answer_since(proof));
        scoped_premise(&now, &scope, &held.project, &stores)
            .map_err(|error| format!("{error:#}"))?
    };
    log.push(format!("  premise: {premise:?}"));
    let since = (studio.version() != proof).then(|| ProofScope::parse(&studio.answer_since(proof)));
    let (aligned, kept) = aligned_stores(&held.project, (&scope, &stores), rng);
    match instance_pull(
        &aligned,
        (&scope, &touched, &stores),
        &whole,
        since.as_ref(),
    ) {
        Ok((project, roots, nested)) => {
            if project != whole {
                let difference = Tree::read(&project)?.difference(&Tree::read(&whole)?);
                if !kept || difference.is_some() || !stores_a_pull_leaves(&project, &whole)? {
                    return Err(format!(
                        "a pull of instances wrote other files than a whole pull: {difference:?}"
                    ));
                }
            }
            let wrote = project != aligned;
            log.push(format!(
                "  instance pull of {roots} instances, written: {wrote}, over a store a pull aligned: {kept}"
            ));
            instance_outcome(usize::from(!wrote));
            for (index, reached) in [(5, roots > 1), (6, nested), (7, since.is_some()), (8, kept)] {
                if wrote && reached {
                    instance_outcome(index);
                }
            }
        }
        Err(reason) => {
            log.push(format!("  instance pull not taken: {reason}"));
            for (index, cause) in [
                (2, "where Studio named no change"),
                (3, "which the capture does not hold"),
                (4, "Studio moved outside the captured instances"),
            ] {
                if reason.contains(cause) {
                    instance_outcome(index);
                }
            }
        }
    }
    if premise == ScopedPremise::False {
        scoped_outcome(2);
        return Ok(scoped_whole_pull(rng, (studio, files), (counter, log)));
    }
    let mut published = current.clone();
    rescope(&mut published, captured.clone(), &paths);
    if published != whole {
        let difference = Tree::read(&published)?.difference(&Tree::read(&whole)?);
        return Err(format!(
            "a pull scoped to {scope:?} wrote other files than a whole pull: {difference:?}"
        ));
    }
    *files = Tree::read(&published)?;
    if rng.below(5) == 0 {
        let (what, _) = scoped_edit(files, (&mut Vec::new(), 0), rng, counter);
        log.push(format!("  then a file edit: {what}"));
    }
    let written = under_scope(&files.snapshot(None));
    match scoped_baseline(
        held.project.clone(),
        written,
        (&paths, &settled_of(&captured)),
        premise,
    ) {
        Ok(project) => {
            if project != at_proof {
                let difference = Tree::read(&project)?.difference(&Tree::read(&at_proof)?);
                return Err(format!(
                    "a scoped pull kept a baseline other than Studio at its proof: {difference:?}"
                ));
            }
            scoped_outcome(0);
            *from_scope = true;
            Ok(Some(Held {
                project,
                version: proof,
            }))
        }
        Err(reason) => {
            log.push(format!("  no baseline: {reason}"));
            scoped_outcome(1);
            Ok(None)
        }
    }
}

/// The files a pull of the instances Studio named in `scope` leaves, by the
/// code a pull runs, with what it captured, and whether one touched instance
/// stood under another; or why it falls back. Studio is as `whole` holds it
/// at the capture, and `since` says where it moved since the proof.
#[cfg(any(windows, target_os = "macos"))]
fn instance_pull(
    held: &ProjectSnapshot,
    (scope, touched, stores): (&BTreeSet<String>, &Touched, &BTreeMap<String, PathBuf>),
    whole: &ProjectSnapshot,
    since: Option<&ProofScope>,
) -> Result<(ProjectSnapshot, usize, bool), String> {
    let mut plans = BTreeMap::new();
    let mut nested = false;
    for service in scope {
        let Some(Some(entries)) = touched.get(service) else {
            return Err("a service moved as a whole".into());
        };
        if entries.is_empty()
            || (entries.iter()).any(|entry| entry.kind != "values" || entry.fields.is_none())
        {
            return Err("not every touched instance is values Studio named".into());
        }
        let Some(SnapshotEntry::File(bytes)) = held.entries.get(&stores[service]) else {
            return Err("the baseline holds no store of a service".into());
        };
        let plan = StoreInstancePull::plan(service, bytes, entries)?;
        nested |= plan.roots.len() < entries.len();
        plans.insert(service.clone(), plan);
    }
    if let Some(since) = since {
        let pulls = (plans.iter())
            .map(|(service, plan)| (service.clone(), plan))
            .collect();
        if instance_premise(since, &pulls) == ScopedPremise::False {
            return Err("Studio moved outside the captured instances".into());
        }
        if let ProofScope::Services { touched, .. } = since {
            for (service, plan) in &mut plans {
                if let Some(Some(entries)) = touched.get(service) {
                    plan.name_since(entries)?;
                }
            }
        }
    }
    let mut project = held.clone();
    for (service, plan) in &mut plans {
        let captured = captured_from(whole, &stores[service], &plan.roots)?;
        if let Some(bytes) = plan.apply(&captured)? {
            (project.entries).insert(stores[service].clone(), SnapshotEntry::File(bytes));
        }
    }
    let roots = plans.values().map(|plan| plan.roots.len()).sum();
    Ok((project, roots, nested))
}

/// The held files as a pull that aligned the stores of `scope` may leave
/// them: some values in the typed form an older store kept, which a pull
/// counts as the value itself; and whether any is.
#[cfg(any(windows, target_os = "macos"))]
fn aligned_stores(
    project: &ProjectSnapshot,
    (scope, stores): (&BTreeSet<String>, &BTreeMap<String, PathBuf>),
    rng: &mut Rng,
) -> (ProjectSnapshot, bool) {
    let mut project = project.clone();
    let mut kept = false;
    for service in scope {
        let Some(SnapshotEntry::File(bytes)) = project.entries.get(&stores[service]) else {
            continue;
        };
        let Ok(mut document) = decode_settings_bytecode(bytes) else {
            continue;
        };
        for instance in &mut document.instances {
            let text = instance.properties.get("Value").and_then(Value::as_str);
            if let Some(text) = text.map(str::to_string)
                && rng.below(3) == 0
            {
                let typed = json!({"_type": "Content", "value": text});
                instance.properties.insert("Value".into(), typed);
                kept = true;
            }
        }
        let bytes = encode_settings_bytecode(&document).unwrap();
        (project.entries).insert(stores[service].clone(), SnapshotEntry::File(bytes));
    }
    (project, kept)
}

/// Whether a whole pull of Studio as `whole` leaves each store of `project`
/// as it is.
#[cfg(any(windows, target_os = "macos"))]
fn stores_a_pull_leaves(
    project: &ProjectSnapshot,
    whole: &ProjectSnapshot,
) -> Result<bool, String> {
    use crate::settings::equivalence::{
        SettingsAlignment, align_pulled_settings_bytes_to_reference,
    };
    for (path, entry) in &project.entries {
        let (SnapshotEntry::File(stored), Some(SnapshotEntry::File(captured))) =
            (entry, whole.entries.get(path))
        else {
            continue;
        };
        let store = (path.file_name().and_then(|name| name.to_str()))
            .is_some_and(is_service_settings_file_name);
        if stored == captured || !store {
            continue;
        }
        match align_pulled_settings_bytes_to_reference(stored, captured) {
            Ok(SettingsAlignment::Equivalent) => {}
            Ok(SettingsAlignment::Changed(_)) => return Ok(false),
            Err(error) => return Err(format!("{error:#}")),
        }
    }
    Ok(true)
}

/// What a native capture of `roots` holds where Studio is as `whole` holds
/// it: each root, found by its debug id where its names lead, with the
/// instances under it breadth first, without the references a native
/// capture leaves out, and the plugin reading every ObjectValue's.
#[cfg(any(windows, target_os = "macos"))]
fn captured_from(
    whole: &ProjectSnapshot,
    store: &Path,
    roots: &[InstanceRoot],
) -> Result<Vec<Vec<CapturedInstance>>, String> {
    let Some(SnapshotEntry::File(bytes)) = whole.entries.get(store) else {
        return Err("a touched instance is gone".into());
    };
    let document = decode_settings_bytecode(bytes).map_err(|error| format!("{error:#}"))?;
    let database = rbx_reflection_database::get().map_err(|error| format!("{error:#}"))?;
    let schemas = crate::roblox::schema::PropertySchemaMap::from([(
        "ObjectValue".to_string(),
        vec![crate::roblox::schema::PropertySchemaEntry {
            name: "Value".into(),
            type_id: crate::roblox::schema::TYPE_ID_REF,
            enum_type: None,
        }],
    )]);
    let rows = &document.instances;
    let conversions = class_conversions(
        database,
        &schemas,
        rows.iter().map(|row| row.class_name.as_str()),
    );
    let names = |mut row: usize| {
        let mut names = vec![rows[row].name.clone()];
        while let Some(parent) = rows[row].parent_index {
            names.insert(0, rows[parent].name.clone());
            row = parent;
        }
        names
    };
    let mut captured = Vec::new();
    for root in roots {
        let top = (0..rows.len())
            .find(|row| rows[*row].settings_id == root.debug_id && names(*row) == root.segments)
            .ok_or("a touched instance is gone or stands elsewhere")?;
        let mut order = vec![(top, None)];
        let mut at = 0;
        while let Some(&(row, _)) = order.get(at) {
            let children = (0..rows.len()).filter(|child| rows[*child].parent_index == Some(row));
            order.extend(children.map(|child| (child, Some(at))));
            at += 1;
        }
        captured.push(
            (order.into_iter())
                .map(|(row, parent)| {
                    let row = &rows[row];
                    let mut properties = row.properties.clone();
                    properties.retain(|_, value| {
                        !crate::settings::equivalence::reconciliation_value_contains_reference(
                            value,
                        )
                    });
                    CapturedInstance {
                        name: row.name.clone(),
                        class_name: row.class_name.clone(),
                        parent,
                        values: CapturedValues {
                            properties,
                            attributes: row.attributes.clone(),
                            overlaid: conversions[&row.class_name].overlaid.clone(),
                        },
                    }
                })
                .collect(),
        );
    }
    Ok(captured)
}

#[cfg(any(windows, target_os = "macos"))]
fn run_scoped_pull_seed(seed: u64, log: &mut Vec<String>) -> Result<(), String> {
    let mut rng = Rng(seed);
    let mut counter = 0;
    let mut tree = first_tree(&mut rng, &mut counter);
    add_second_service(&mut tree, &mut rng, &mut counter);
    add_workspace(&mut tree, &mut rng, &mut counter);
    let mut studio = ScopedStudio {
        tree,
        changes: Vec::new(),
        removed: Vec::new(),
        proof: counter,
    };
    let mut files = studio.tree.clone();
    let mut held = Some(Held {
        project: files.snapshot(None),
        version: 0,
    });
    let mut from_scope = false;
    for step in 0..40 {
        match rng.below(8) {
            0 => {
                let (what, _) =
                    scoped_edit(&mut files, (&mut Vec::new(), 0), &mut rng, &mut counter);
                log.push(format!("file edit: {what}"));
            }
            1..=3 => {
                for _ in 0..1 + rng.below(2) {
                    studio.edit(&mut rng, &mut counter, log);
                }
            }
            4 => {
                log.push("full push".into());
                studio.tree = files.clone();
                studio.changes.push(None);
                from_scope = false;
                held = Some(Held {
                    project: files.snapshot(None),
                    version: studio.take_proof(counter),
                });
            }
            _ => {
                held = scoped_pull_step(
                    &mut rng,
                    (&mut studio, &mut files),
                    held,
                    (&mut counter, &mut from_scope, log),
                )?;
            }
        }
        if let Some(held) = held
            .as_ref()
            .filter(|held| held.version == studio.version())
            && held.project != studio.tree.snapshot(None)
        {
            let difference = Tree::read(&held.project)?.difference(&studio.tree);
            return Err(format!(
                "step {step}: the held baseline is not what Studio holds: {difference:?}"
            ));
        }
    }
    Ok(())
}

/// Every baseline a pull or a push keeps is what Studio holds, and an answer
/// from one is the answer of a capture: random file and Studio edits, pulls
/// (some with a file or Studio edit while they write), full pushes and
/// pushes of chosen instances or scripts (some with a Studio edit meanwhile).
/// A pull scoped to the services Studio moved in writes what a whole pull
/// writes, with edits in three services: values, attributes, tags, sources,
/// renames, removals and their undo, moves between services, references
/// across them and Terrain, some made before or after the pull's capture.
/// So does a pull of the instances Studio named values of, where it writes:
/// some changes are laid to the instance's parent, as Studio missing one.
#[cfg(any(windows, target_os = "macos"))]
#[test]
fn seeded_baselines_answer_as_captures_do() {
    run_seeds(300, run_baseline_seed);
    for (index, what) in [
        "targeted push dropped",
        "targeted push kept",
        "pull dropped",
        "pull kept",
    ]
    .iter()
    .enumerate()
    {
        assert!(
            BASELINE_OUTCOMES[index].load(std::sync::atomic::Ordering::Relaxed) > 0,
            "no seed reached: {what}"
        );
    }
    run_seeds(1000, run_scoped_pull_seed);
    for (count, what) in (SCOPED_COUNTS.iter().zip(SCOPED_OUTCOMES))
        .chain(INSTANCE_COUNTS.iter().zip(INSTANCE_OUTCOMES))
    {
        assert!(
            count.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "no seed reached: {what}"
        );
    }
}

#[cfg(any(windows, target_os = "macos"))]
const LIVE_START_OUTCOMES: [&str; 5] = [
    "started from the baseline",
    "captured the services Studio moved in",
    "captured a store referring into a moved service",
    "captured every service: Studio could not say where it moved",
    "captured every service: the files changed since the pull",
];

#[cfg(any(windows, target_os = "macos"))]
static LIVE_START_COUNTS: [std::sync::atomic::AtomicUsize; 5] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; 5];

/// A Live Sync start of the scoped model under the baseline a pull keeps,
/// decided by the code a start runs. Where it starts from the baseline,
/// Studio and the files are that baseline; where it captures the services
/// Studio moved in, its Studio side is byte for byte a whole capture's.
#[cfg(any(windows, target_os = "macos"))]
fn live_start_step(
    studio: &ScopedStudio,
    files: &Tree,
    held: Option<&Held>,
    log: &mut Vec<String>,
) -> Result<(), String> {
    let outcome = |index: usize| {
        LIVE_START_COUNTS[index].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    };
    let whole = studio.tree.snapshot(None);
    let Some(held) = held.filter(|held| held.project == files.snapshot(None)) else {
        log.push("live start captures every service".into());
        outcome(4);
        return Ok(());
    };
    if held.version == studio.version() {
        log.push("live start from the baseline".into());
        if let Some(difference) = files.difference(&studio.tree) {
            return Err(format!(
                "a live start from the baseline compared other Studio than a capture: {difference}"
            ));
        }
        outcome(0);
        return Ok(());
    }
    let stores = SCOPED_SERVICES
        .iter()
        .map(|service| {
            let store = service_settings_path(&Path::new("src").join(service));
            (service.to_string(), store)
        })
        .collect::<BTreeMap<_, _>>();
    let scope = ProofScope::parse(&studio.answer_since(held.version));
    let services = match moved_services(&scope, &held.project, &stores)
        .map_err(|error| format!("{error:#}"))?
    {
        Ok(services) => services,
        Err(reason) => {
            log.push(format!("live start captures every service: {reason}"));
            outcome(3);
            return Ok(());
        }
    };
    log.push(format!("live start captures {services:?}"));
    let ProofScope::Services {
        services: moved, ..
    } = scope
    else {
        return Err("a start captured services Studio could not name".into());
    };
    let scopes = services
        .iter()
        .flat_map(|service| {
            let directory = Path::new("src").join(service);
            [service_settings_path(&directory), directory]
        })
        .collect::<Vec<_>>();
    let scope_set = ScopeSet::new(&scopes);
    let mut side = held.project.clone();
    side.entries.retain(|path, _| !scope_set.contains(path));
    side.entries.extend(
        whole
            .entries
            .iter()
            .filter(|(path, _)| scope_set.contains(path))
            .map(|(path, entry)| (path.clone(), entry.clone())),
    );
    if side != whole {
        let difference = Tree::read(&side)?.difference(&studio.tree);
        return Err(format!(
            "a live start capturing {services:?} compared other Studio than a capture: {difference:?}"
        ));
    }
    outcome(if services == moved { 1 } else { 2 });
    Ok(())
}

#[cfg(any(windows, target_os = "macos"))]
fn run_live_start_seed(seed: u64, log: &mut Vec<String>) -> Result<(), String> {
    let mut rng = Rng(seed);
    let mut counter = 0;
    let mut tree = first_tree(&mut rng, &mut counter);
    add_second_service(&mut tree, &mut rng, &mut counter);
    add_workspace(&mut tree, &mut rng, &mut counter);
    let mut studio = ScopedStudio {
        tree,
        changes: Vec::new(),
        removed: Vec::new(),
        proof: counter,
    };
    let mut files = studio.tree.clone();
    let mut held = Some(Held {
        project: files.snapshot(None),
        version: 0,
    });
    let mut from_scope = false;
    for _ in 0..40 {
        match rng.below(9) {
            0 => {
                let (what, _) =
                    scoped_edit(&mut files, (&mut Vec::new(), 0), &mut rng, &mut counter);
                log.push(format!("file edit: {what}"));
            }
            1..=3 => {
                for _ in 0..1 + rng.below(3) {
                    studio.edit(&mut rng, &mut counter, log);
                }
            }
            4 => {
                // A push keeps a baseline of what it sent, which a start
                // does not begin from.
                log.push("full push".into());
                studio.tree = files.clone();
                studio.changes.push(None);
                studio.take_proof(counter);
                held = None;
            }
            5 | 6 => {
                held = scoped_pull_step(
                    &mut rng,
                    (&mut studio, &mut files),
                    held,
                    (&mut counter, &mut from_scope, log),
                )?;
            }
            _ => live_start_step(&studio, &files, held.as_ref(), log)?,
        }
    }
    Ok(())
}

/// A Live Sync start under a pull's baseline compares what a start that
/// captures every service compares: random file and Studio edits, pushes,
/// whole and scoped pulls, and starts where Studio moved in some services
/// or where it cannot say.
#[cfg(any(windows, target_os = "macos"))]
#[test]
fn seeded_live_starts_compare_what_a_capture_compares() {
    run_seeds(1000, run_live_start_seed);
    for (count, what) in LIVE_START_COUNTS.iter().zip(LIVE_START_OUTCOMES) {
        assert!(
            count.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "no seed reached: {what}"
        );
    }
}

/// A store outside a pull's scope that refers into a service the pull
/// captures is written from the capture's paths: a change there during the
/// pull is outside the scope, one in a service it does not refer to is not,
/// and one that leaves the paths it refers to in place moves none of them.
#[cfg(any(windows, target_os = "macos"))]
#[test]
fn a_change_a_store_outside_the_scope_refers_to_falsifies_the_premise() {
    let mut tree = Tree::default();
    for (id, service) in [
        ("root", "ReplicatedStorage"),
        ("root2", "ServerStorage"),
        ("root3", "Workspace"),
    ] {
        let mut node = new_node(service, String::new(), 0, None);
        node.name = service.to_string();
        node.parent = None;
        tree.nodes.insert(id.to_string(), node);
    }
    tree.nodes
        .insert("b1".into(), new_node("Folder", "root2".into(), 1, None));
    tree.nodes.insert(
        "w2".into(),
        new_node("ObjectValue", "root3".into(), 2, Some("b1".into())),
    );
    let stores = SCOPED_SERVICES
        .iter()
        .map(|service| {
            let store = service_settings_path(&Path::new("src").join(service));
            (service.to_string(), store)
        })
        .collect::<BTreeMap<_, _>>();
    let scope = BTreeSet::from(["ReplicatedStorage", "ServerStorage"].map(String::from));
    let premise = |moved: &str| {
        let now = ProofScope::Services {
            services: BTreeSet::from([moved.to_string()]),
            touched: BTreeMap::new(),
            values_only: BTreeSet::new(),
        };
        scoped_premise(&now, &scope, &tree.snapshot(None), &stores).unwrap()
    };
    assert_eq!(premise("ServerStorage"), ScopedPremise::False);
    assert_eq!(premise("ReplicatedStorage"), ScopedPremise::Inside);
    assert_eq!(premise("Workspace"), ScopedPremise::False);
    // Studio moved only values in a service another store refers into:
    // that store writes its references as before. So it does where the
    // children of the instance it names changed, or a session lock's marker
    // beside it: neither moves that instance's path. The instance's own
    // subtree, its parent's children and the whole service do.
    let scope = BTreeSet::from(["ServerStorage".to_string()]);
    let premise = |touched: Option<(&[&str], &str)>| {
        let touched = touched.map(|(below, kind)| {
            let mut path_segments = vec!["ServerStorage".to_string()];
            path_segments.extend(below.iter().map(|name| name.to_string()));
            vec![TouchedInstance {
                path_ordinals: vec![1; path_segments.len()],
                path_segments,
                kind: kind.into(),
                fields: None,
            }]
        });
        let now = ProofScope::Services {
            services: scope.clone(),
            touched: BTreeMap::from([("ServerStorage".to_string(), touched)]),
            values_only: BTreeSet::new(),
        };
        scoped_premise(&now, &scope, &tree.snapshot(None), &stores).unwrap()
    };
    assert_eq!(premise(Some((&["N1"], "values"))), ScopedPremise::Inside);
    assert_eq!(premise(Some((&["N1"], "children"))), ScopedPremise::Inside);
    assert_eq!(
        premise(Some((&["__Renium_SessionLock"], "values"))),
        ScopedPremise::Inside
    );
    assert_eq!(premise(Some((&["N1"], "subtree"))), ScopedPremise::False);
    assert_eq!(premise(Some((&[], "children"))), ScopedPremise::False);
    assert_eq!(premise(None), ScopedPremise::False);
    let unnamed = BTreeMap::from([("ServerStorage".to_string(), Some(Vec::new()))]);
    assert_eq!(
        paths_moved_in(&scope, &unnamed),
        scope,
        "a service with no instance named moved as a whole"
    );
}

/// A push of chosen instances keeps a baseline only where it sends every
/// value that differs: it sends no PackageLink, not a camera, and a script's
/// RunContext names its file.
#[cfg(any(windows, target_os = "macos"))]
#[test]
fn a_targeted_baseline_holds_only_rows_a_push_sends() {
    let store = |class: &str, property: &str, value: Value| {
        let mut row =
            SettingsBytecodeInstance::new("row".into(), "Row".into(), class.into(), Some(0));
        row.properties.insert(property.into(), value);
        encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "root".into(),
                    "Workspace".into(),
                    "Workspace".into(),
                    None,
                ),
                row,
            ],
        })
        .unwrap()
    };
    let ids = HashSet::from(["row".to_string()]);
    let empty = HashSet::new();
    let kept = |class: &str, property: &str, was: Value, now: Value| {
        store_rows_differ_only_at(
            &store(class, property, was),
            &store(class, property, now),
            &ids,
            true,
            &empty,
        )
        .unwrap()
    };
    assert!(kept("Part", "Transparency", json!(0.0), json!(0.5)));
    assert!(!kept("Camera", "FieldOfView", json!(70.0), json!(40.0)));
    assert!(!kept(
        "PackageLink",
        "AutoUpdate",
        json!(false),
        json!(true)
    ));
    assert!(!kept(
        "Script",
        "RunContext",
        json!("Legacy"),
        json!("Client")
    ));
    assert!(kept("Script", "Disabled", json!(false), json!(true)));

    assert!(push_changed_structure(
        json!({"ok": true, "instanceCreated": 1.0})
            .as_object()
            .unwrap()
    ));
    assert!(!push_changed_structure(
        json!({"ok": true, "instanceCreated": 0.0, "propertyUpdated": 2.0})
            .as_object()
            .unwrap()
    ));

    let decal = |texture: &str, color_map: &str| {
        let mut row =
            SettingsBytecodeInstance::new("row".into(), "Row".into(), "Decal".into(), None);
        row.properties
            .insert("TextureContent".into(), json!(texture));
        row.properties
            .insert("ColorMapContent".into(), json!(color_map));
        encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![row],
        })
        .unwrap()
    };
    let held = decal("rbxassetid://1", "rbxassetid://1");
    let kept = |now: &[u8]| store_rows_differ_only_at(&held, now, &ids, true, &empty).unwrap();
    assert!(kept(&decal("rbxassetid://2", "rbxassetid://2")));
    assert!(!kept(&decal("rbxassetid://1", "rbxassetid://2")));
    assert!(!kept(&decal("rbxassetid://2", "rbxassetid://1")));
}

/// What a pull writes of Studio, as `studio` holds it, over the files
/// `files`: each store aligned to the files' copy, or left as the files hold
/// it where Studio holds it alike, and every other path as Studio has it.
#[cfg(any(windows, target_os = "macos"))]
pub(super) fn pulled_over(
    files: &ProjectSnapshot,
    studio: &ProjectSnapshot,
) -> Result<ProjectSnapshot> {
    use crate::settings::equivalence::{
        SettingsAlignment, align_pulled_settings_bytes_to_reference,
    };
    let mut pulled = studio.clone();
    for (path, entry) in &mut pulled.entries {
        let (Some(SnapshotEntry::File(held)), SnapshotEntry::File(observed)) =
            (files.entries.get(path), &*entry)
        else {
            continue;
        };
        if !(path.file_name().and_then(|name| name.to_str()))
            .is_some_and(is_service_settings_file_name)
        {
            continue;
        }
        let aligned = match align_pulled_settings_bytes_to_reference(held, observed)
            .with_context(|| format!("{} does not align", path.display()))?
        {
            SettingsAlignment::Equivalent => held.clone(),
            SettingsAlignment::Changed(aligned) => aligned,
        };
        *entry = SnapshotEntry::File(aligned);
    }
    Ok(pulled)
}

/// Why a pull of Studio as `studio` holds it would write over `files`, or a
/// push of them send anything; `None` where neither does.
#[cfg(any(windows, target_os = "macos"))]
pub(super) fn unsettled(
    files: &ProjectSnapshot,
    studio: &ProjectSnapshot,
) -> Result<Option<String>> {
    let file = |entry: Option<&SnapshotEntry>| matches!(entry, Some(SnapshotEntry::File(_)));
    let pulled = pulled_over(files, studio)?;
    let written = (files.entries.keys().chain(pulled.entries.keys())).find(|path| {
        let (held, taken) = (files.entries.get(*path), pulled.entries.get(*path));
        held != taken && (file(held) || file(taken))
    });
    if let Some(path) = written {
        return Ok(Some(format!(
            "a pull after the start writes {}",
            path.display()
        )));
    }
    let mut sent = super::replacement::snapshot_differences_prepared(files, studio, None, true)?
        .into_iter()
        .filter(|path| file(files.entries.get(path)) || file(studio.entries.get(path)))
        .collect::<Vec<_>>();
    sent.sort();
    Ok((!sent.is_empty()).then(|| format!("a push after the start sends {sent:?}")))
}

/// A PackageLink under an instance of `tree` that is not one.
#[cfg(any(windows, target_os = "macos"))]
fn add_package_link(tree: &mut Tree, rng: &mut Rng, counter: &mut u64, side: Side) -> Option<Op> {
    *counter += 1;
    let parents = (tree.nodes.iter())
        .filter(|(_, node)| node.class != "PackageLink")
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let id = format!("{}{counter}", if side == Side::Editor { "e" } else { "s" });
    let op = Op::Add(
        id,
        new_node("PackageLink", rng.pick(&parents)?, *counter, None),
    );
    op.apply(tree);
    Some(op)
}

/// A rename, a removal or a move of a PackageLink of `tree`, which a merge
/// refuses.
#[cfg(any(windows, target_os = "macos"))]
fn edit_package_link(tree: &mut Tree, rng: &mut Rng, counter: &mut u64) -> Option<Op> {
    *counter += 1;
    let of_class = |link: bool| {
        (tree.nodes.iter())
            .filter(|(_, node)| (node.class == "PackageLink") == link)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>()
    };
    let link = rng.pick(&of_class(true))?;
    let op = match rng.below(3) {
        0 => Op::Rename(link, format!("N{counter}")),
        1 => Op::Delete(link),
        _ => {
            let parent = rng.pick(&of_class(false))?;
            if parent == tree.nodes[&link].parent.clone()?
                || tree.subtree(&link).contains(&parent)
                || tree.service_of(&parent) != tree.service_of(&link)
            {
                return None;
            }
            Op::Move(link, parent)
        }
    };
    op.apply(tree);
    Some(op)
}

/// An edit that leaves or takes away instances alike in parent, name and
/// class: a copy of an instance beside it, or the removal or a move to
/// another parent of one of two such siblings.
#[cfg(any(windows, target_os = "macos"))]
fn twin_edit(tree: &mut Tree, rng: &mut Rng, counter: &mut u64) -> Option<Op> {
    *counter += 1;
    let alike = |id: &String, other: &String| {
        let (left, right) = (&tree.nodes[id], &tree.nodes[other]);
        id != other
            && left.parent.is_some()
            && (left.parent.as_ref(), &left.name, &left.class)
                == (right.parent.as_ref(), &right.name, &right.class)
    };
    let twins = (tree.nodes.keys())
        .filter(|id| tree.nodes.keys().any(|other| alike(id, other)))
        .cloned()
        .collect::<Vec<_>>();
    let op = match rng.below(3) {
        0 => {
            let leaves = (tree.nodes.iter())
                .filter(|(id, node)| {
                    node.parent.is_some()
                        && node.class != "ModuleScript"
                        && tree.children(id).is_empty()
                })
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            let copied = rng.pick(&leaves)?;
            Op::Add(format!("e{counter}"), tree.nodes[&copied].clone())
        }
        1 => Op::Delete(rng.pick(&twins)?),
        _ => {
            let twin = rng.pick(&twins)?;
            let parents = (tree.nodes.keys())
                .filter(|id| {
                    **id != twin
                        && !tree.subtree(&twin).contains(*id)
                        && tree.nodes[&twin].parent.as_ref() != Some(*id)
                        && tree.service_of(id) == tree.service_of(&twin)
                })
                .cloned()
                .collect::<Vec<_>>();
            Op::Move(twin, rng.pick(&parents)?)
        }
    };
    op.apply(tree);
    Some(op)
}

/// A PackageLink of Studio added, renamed, removed or moved, which the
/// plugin lays to its service as a whole.
#[cfg(any(windows, target_os = "macos"))]
fn studio_package_edit(
    studio: &mut ScopedStudio,
    rng: &mut Rng,
    counter: &mut u64,
    log: &mut Vec<String>,
) {
    let before = studio.tree.clone();
    let op = if rng.below(3) == 0 {
        add_package_link(&mut studio.tree, rng, counter, Side::Studio)
    } else {
        edit_package_link(&mut studio.tree, rng, counter)
    };
    let at = match &op {
        Some(Op::Add(_, node)) => node.parent.clone(),
        Some(Op::Rename(id, _) | Op::Delete(id) | Op::Move(id, _)) => Some(id.clone()),
        _ => None,
    };
    let Some(service) = at.and_then(|id| before.service_of(&id).map(str::to_string)) else {
        return;
    };
    log.push(format!("Studio: {op:?}, laid to {service}"));
    studio.changes.push(Some(vec![(service, "whole", None)]));
}

/// A Live Sync start the coordinator's simulation draws: what a pull wrote,
/// a sync older than it, and the files and Studio now, of which one moved
/// since the pull, with Studio's answer of where it moved since.
#[cfg(any(windows, target_os = "macos"))]
pub(super) struct DrawnStart {
    pub(super) older: ProjectSnapshot,
    pub(super) pulled: ProjectSnapshot,
    pub(super) files: ProjectSnapshot,
    pub(super) studio: ProjectSnapshot,
    pub(super) scope: Value,
}

/// Draws a start in two services, with twins and a PackageLink now and then:
/// the files move since the pull where `files_move`, by values, references,
/// attributes, sources, renames, moves, removals, additions, twins and
/// PackageLinks; else Studio does, by those and tags, moves between
/// services and the undo of a removal. Studio that moved holds its siblings
/// in another order than the files, and names no instance by its debug id,
/// so a pull takes a service it moved in whole.
#[cfg(any(windows, target_os = "macos"))]
pub(super) fn drawn_start(rng: &mut Rng, files_move: bool, log: &mut Vec<String>) -> DrawnStart {
    let mut counter = 0;
    let mut older = first_tree(rng, &mut counter);
    add_second_service(&mut older, rng, &mut counter);
    if rng.below(3) == 0 {
        let link = add_package_link(&mut older, rng, &mut counter, Side::Studio);
        log.push(format!("older sync: {link:?}"));
    }
    for _ in 0..rng.below(3) {
        let twin = twin_edit(&mut older, rng, &mut counter);
        log.push(format!("older sync: {twin:?}"));
    }
    let mut pulled = older.clone();
    for _ in 0..1 + rng.below(2) {
        let op = edit_freely(&mut pulled, rng, &mut counter, Side::Studio);
        log.push(format!("before the pull: {op:?}"));
    }
    let mut files = pulled.clone();
    let mut studio = ScopedStudio {
        tree: pulled.clone(),
        changes: Vec::new(),
        removed: Vec::new(),
        proof: counter,
    };
    for _ in 0..1 + rng.below(3) {
        if files_move {
            let op = match rng.below(10) {
                0 => add_package_link(&mut files, rng, &mut counter, Side::Editor),
                1 => edit_package_link(&mut files, rng, &mut counter),
                2 | 3 => twin_edit(&mut files, rng, &mut counter),
                _ => edit_freely(&mut files, rng, &mut counter, Side::Editor),
            };
            log.push(format!("file edit: {op:?}"));
        } else if rng.below(10) == 0 {
            studio_package_edit(&mut studio, rng, &mut counter, log);
        } else {
            studio.edit(rng, &mut counter, log);
        }
    }
    let mut scope = studio.answer_since(0);
    let touched = scope.get_mut("touched").and_then(Value::as_object_mut);
    for entries in touched.into_iter().flat_map(|touched| touched.values_mut()) {
        for entry in entries.as_array_mut().into_iter().flatten() {
            if let Some(entry) = entry.as_object_mut() {
                entry.remove("debugId");
            }
        }
    }
    DrawnStart {
        older: older.snapshot(None),
        pulled: pulled.snapshot(None),
        files: files.snapshot(None),
        studio: studio.tree.snapshot((!files_move).then_some(rng)),
        scope,
    }
}
