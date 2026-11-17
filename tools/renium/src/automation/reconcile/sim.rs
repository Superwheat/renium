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
                    .and_then(|value| value.get("settingsId").or(Some(value)))
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
