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

    fn snapshot(&self, mut noise: Option<&mut Rng>) -> ProjectSnapshot {
        let mut order = vec![ROOT.to_string()];
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
                if let Some(value) = &node.value {
                    instance.properties.insert("Value".into(), json!(value));
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
        let service_dir = Path::new("src").join(SERVICE);
        let mut entries = BTreeMap::new();
        entries.insert(service_dir.clone(), SnapshotEntry::Directory);
        let paths = build_editor_source_paths_by_index(&document, SERVICE, &service_dir);
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
        ProjectSnapshot { entries }
    }

    /// Reads a snapshot back into the model, refusing one that is not a
    /// well-formed project: an identity twice, a script without its file, a
    /// script file no script owns.
    fn read(snapshot: &ProjectSnapshot) -> Result<Tree, String> {
        let settings_path = service_settings_path(&Path::new("src").join(SERVICE));
        let document = settings_document(snapshot.entries.get(&settings_path))
            .map_err(|error| format!("the store does not decode: {error:#}"))?;
        let paths = source_paths_by_settings_index(&document, &settings_path)
            .map_err(|error| format!("script paths: {error:#}"))?;
        let mut tree = Tree::default();
        let mut owned = HashSet::new();
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
            let node = Node {
                name: instance.name.clone(),
                class: instance.class_name.clone(),
                parent: instance
                    .parent_index
                    .map(|parent| document.instances[parent].settings_id.clone()),
                value: instance
                    .properties
                    .get("Value")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                attributes: instance
                    .attributes
                    .iter()
                    .map(|(key, value)| (key.clone(), value.as_str().unwrap_or("?").to_string()))
                    .collect(),
                source,
            };
            if tree
                .nodes
                .insert(instance.settings_id.clone(), node)
                .is_some()
            {
                return Err(format!("identity {} twice", instance.settings_id));
            }
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
    conflicts: Vec<String>,
    changes: MergeChanges,
}

fn script_paths(tree: &Tree) -> HashMap<String, PathBuf> {
    let settings_path = service_settings_path(&Path::new("src").join(SERVICE));
    script_paths_by_settings_id(&tree.snapshot(None).entries, &settings_path).unwrap()
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
    preference: ConflictPreference,
    rng: &mut Rng,
) -> Result<Merged, String> {
    let (editor, studio) = (editor.snapshot(None), studio.snapshot(Some(rng)));
    // A sync merges only the paths its two sides hold differently.
    let differences = (rng.below(2) == 0)
        .then(|| snapshot_differences(&editor, &studio))
        .transpose()
        .map_err(|error| format!("comparing the sides failed: {error:#}"))?;
    let (snapshot, conflicts, changes) = merge_snapshots_with_changes(
        Some(&shared.snapshot(None)),
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
