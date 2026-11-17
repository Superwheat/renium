//! A sync conflict as a record, and the decisions that settle conflicts one
//! by one.
//!
//! A conflict carries an id that stays the same for the same thing across
//! comparisons, what it is about, and what the last sync, the files and
//! Studio hold for it. A caller decides it by that id; a comparison asks the
//! decisions for every conflict it meets before it records one.

use super::*;

/// The lines a text excerpt or diff keeps.
const EXCERPT_LINES: usize = 60;
/// The characters a shown value or line keeps.
const VALUE_CHARACTERS: usize = 2_000;

/// What a conflict is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ConflictKind {
    /// A script both sides edited.
    Script,
    /// Another file both sides changed.
    File,
    Property,
    Attribute,
    Name,
    Class,
    Parent,
    /// One side removed what the other edited.
    Removal,
    /// A reference to something the other side removed.
    Reference,
    /// A PackageLink edit, which sync never settles.
    Package,
    /// Instances a first pairing cannot match between the two sides.
    Pairing,
    /// A store file missing on one side.
    Store,
}

impl ConflictKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::File => "file",
            Self::Property => "property",
            Self::Attribute => "attribute",
            Self::Name => "name",
            Self::Class => "class",
            Self::Parent => "parent",
            Self::Removal => "removal",
            Self::Reference => "reference",
            Self::Package => "package",
            Self::Pairing => "pairing",
            Self::Store => "store",
        }
    }
}

/// The side a decision keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ConflictSide {
    Studio,
    Editor,
}

impl ConflictSide {
    /// `studio`, or `editor` (also spelled `files`).
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "studio" => Some(Self::Studio),
            "editor" | "files" => Some(Self::Editor),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Studio => "studio",
            Self::Editor => "editor",
        }
    }
}

impl ConflictPreference {
    pub(crate) fn side(self) -> Option<ConflictSide> {
        match self {
            Self::None => None,
            Self::Studio => Some(ConflictSide::Studio),
            Self::Editor => Some(ConflictSide::Editor),
        }
    }
}

/// How one comparison settles conflicts: a side for each decided id, and
/// the side every other conflict takes.
#[derive(Clone, Debug)]
pub(crate) struct ConflictDecisions {
    pub(crate) preference: ConflictPreference,
    pub(crate) by_id: BTreeMap<String, ConflictSide>,
}

impl From<ConflictPreference> for ConflictDecisions {
    fn from(preference: ConflictPreference) -> Self {
        Self {
            preference,
            by_id: BTreeMap::new(),
        }
    }
}

/// Reads one decision, `ID=studio|editor`.
pub(crate) fn parse_conflict_decision(text: &str) -> Result<(String, ConflictSide), String> {
    let (id, side) = text
        .split_once('=')
        .ok_or_else(|| format!("a decision is ID=studio|editor, got `{text}`"))?;
    let id = id.trim();
    if id.len() != 8
        || !id
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(format!(
            "a conflict id is 8 lowercase hex characters, got `{id}`"
        ));
    }
    let side = ConflictSide::parse(side.trim())
        .ok_or_else(|| format!("a decision keeps studio or editor, got `{}`", side.trim()))?;
    Ok((id.to_string(), side))
}

/// Reads a list of decisions; one id decided two ways is refused.
pub(crate) fn parse_conflict_decisions<'a>(
    texts: impl IntoIterator<Item = &'a str>,
) -> Result<BTreeMap<String, ConflictSide>> {
    let mut decisions = BTreeMap::new();
    for text in texts {
        let (id, side) = parse_conflict_decision(text).map_err(anyhow::Error::msg)?;
        if decisions
            .insert(id.clone(), side)
            .is_some_and(|other| other != side)
        {
            bail!("Conflict {id} is decided for both sides");
        }
    }
    Ok(decisions)
}

/// The value parser of `--resolve`: the text, once it reads as a decision.
pub(crate) fn conflict_decision_argument(text: &str) -> Result<String, String> {
    parse_conflict_decision(text).map(|_| text.to_string())
}

/// The instance a conflict is about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ConflictInstance {
    /// Its settings id in the store.
    pub(crate) id: String,
    pub(crate) path: String,
}

/// One conflict between the files and Studio.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Conflict {
    pub(crate) id: String,
    pub(crate) kind: ConflictKind,
    /// The project path of the file or store, with forward slashes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) instance: Option<ConflictInstance>,
    /// The property, attribute or referring field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) base: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) files: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) studio: Option<Value>,
    /// The places that do not hold the thing at all: `base` (the last
    /// sync), `files`, `studio`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) absent: Vec<String>,
    /// For a text: the lines the files changed since the last sync.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) files_diff: Vec<String>,
    /// For a text: the lines Studio changed since the last sync.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) studio_diff: Vec<String>,
    #[serde(default = "decidable_by_default", skip_serializing_if = "is_true")]
    pub(crate) decidable: bool,
    /// The one side a decision may keep, when the other is not allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) only: Option<ConflictSide>,
    pub(crate) detail: String,
    /// When the file was last written on disk, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) files_changed_at: Option<String>,
    /// Whether Studio's change tracker saw the store's service change since
    /// the last sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) studio_changed: Option<bool>,
    /// Tells apart, in the id, two conflicts about the same field.
    #[serde(skip)]
    pub(crate) discriminator: String,
    /// A digest of the whole values each place holds, uncut: an id names
    /// the conflict as shown, with these values, and nothing else.
    #[serde(skip)]
    held: String,
}

fn decidable_by_default() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

/// A project path as a conflict names it.
pub(crate) fn conflict_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

impl Conflict {
    pub(crate) fn new(kind: ConflictKind, path: &Path, detail: String) -> Self {
        Self {
            id: String::new(),
            kind,
            path: conflict_path(path),
            instance: None,
            field: None,
            base: None,
            files: None,
            studio: None,
            absent: Vec::new(),
            files_diff: Vec::new(),
            studio_diff: Vec::new(),
            decidable: true,
            only: None,
            detail,
            files_changed_at: None,
            studio_changed: None,
            discriminator: String::new(),
            held: String::new(),
        }
    }

    /// A conflict a record written before conflicts were records holds as
    /// a sentence: no side can be chosen for it, and the next comparison
    /// replaces it.
    pub(crate) fn legacy(detail: String) -> Self {
        let mut conflict = Self::new(ConflictKind::File, Path::new(""), detail).undecidable();
        conflict.id = short_hash(&format!("legacy\0{}", conflict.detail));
        conflict
    }

    pub(crate) fn instance(mut self, id: &str, path: String) -> Self {
        self.instance = Some(ConflictInstance {
            id: id.to_string(),
            path,
        });
        self
    }

    pub(crate) fn field(mut self, field: &str) -> Self {
        self.field = Some(field.to_string());
        self
    }

    pub(crate) fn undecidable(mut self) -> Self {
        self.decidable = false;
        self
    }

    pub(crate) fn only(mut self, side: ConflictSide) -> Self {
        self.only = Some(side);
        self
    }

    pub(crate) fn discriminated(mut self, discriminator: impl Into<String>) -> Self {
        self.discriminator = discriminator.into();
        self
    }

    /// The three values, each shown whole up to its cut; a missing one is
    /// named in `absent`.
    pub(crate) fn values(
        mut self,
        base: Option<&Value>,
        files: Option<&Value>,
        studio: Option<&Value>,
    ) -> Self {
        self.base = base.map(shown_value);
        self.files = files.map(shown_value);
        self.studio = studio.map(shown_value);
        self.absent = [("base", base), ("files", files), ("studio", studio)]
            .into_iter()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string())
            .collect();
        self.held = digest_of(
            [base, files, studio].map(|value| value.map(|value| value.to_string().into_bytes())),
        );
        self
    }

    /// The three versions of a file: each side's changes as a diff against
    /// the last sync's text, or, without one, the first lines of each side.
    pub(crate) fn texts(
        mut self,
        base: Option<&SnapshotEntry>,
        files: Option<&SnapshotEntry>,
        studio: Option<&SnapshotEntry>,
    ) -> Self {
        let text = |entry: &SnapshotEntry| match entry {
            SnapshotEntry::File(bytes) => std::str::from_utf8(bytes)
                .ok()
                .map(|text| text.replace("\r\n", "\n")),
            _ => None,
        };
        let shown = |entry: &SnapshotEntry| match text(entry) {
            Some(body) => Value::Array(text_head(&body).into_iter().map(Value::String).collect()),
            None => json!(entry_description(entry)),
        };
        match base.and_then(text) {
            // Both diffs read against the last sync's text, which is not
            // repeated.
            Some(base_body) => {
                for (entry, diff, value) in [
                    (files, &mut self.files_diff, &mut self.files),
                    (studio, &mut self.studio_diff, &mut self.studio),
                ] {
                    match entry.and_then(text) {
                        Some(side) => *diff = text_diff(&base_body, &side),
                        None => *value = entry.map(shown),
                    }
                }
            }
            None => {
                self.base = base.map(shown);
                self.files = files.map(shown);
                self.studio = studio.map(shown);
            }
        }
        self.absent = [("base", base), ("files", files), ("studio", studio)]
            .into_iter()
            .filter(|(_, entry)| entry.is_none())
            .map(|(name, _)| name.to_string())
            .collect();
        self.held = digest_of([base, files, studio].map(|entry| {
            entry.map(|entry| match entry {
                SnapshotEntry::File(bytes) => [b"file\0".as_slice(), bytes].concat(),
                other => entry_description(other).into_bytes(),
            })
        }));
        self
    }

    /// What tells this conflict apart from every other of a comparison:
    /// what it is about, and the values each place holds.
    fn identity(&self) -> String {
        format!(
            "{}\0{}\0{}\0{}\0{}\0{}",
            self.kind.name(),
            self.path,
            self.instance
                .as_ref()
                .map_or("", |instance| instance.id.as_str()),
            self.field.as_deref().unwrap_or(""),
            self.discriminator,
            self.held
        )
    }

    /// Whether two records are about the same thing, whatever values each
    /// shows.
    pub(crate) fn same_subject(&self, other: &Conflict) -> bool {
        self.kind == other.kind
            && self.path == other.path
            && self.field == other.field
            && self.instance.as_ref().map(|instance| &instance.id)
                == other.instance.as_ref().map(|instance| &instance.id)
    }

    /// Whether a scoped push or pull of these project paths settles it.
    pub(crate) fn covered_by_paths(&self, paths: &[PathBuf]) -> bool {
        if self.path.is_empty() {
            return conflict_covered_by_paths(&self.detail, paths);
        }
        conflict_covered_by_paths(&self.path, paths)
    }
}

/// Whether a text that starts with a project path (`src/A.luau changed on
/// both sides`, `push: src/A.luau`) names a path a push of `paths`, or of a
/// directory above it, covers.
pub(crate) fn conflict_covered_by_paths(text: &str, paths: &[PathBuf]) -> bool {
    let text = text.replace('\\', "/");
    let text = text
        .strip_prefix("push: ")
        .or_else(|| text.strip_prefix("pull: "))
        .unwrap_or(&text);
    paths.iter().any(|path| {
        let path = path.to_string_lossy().replace('\\', "/");
        let path = path.trim_end_matches('/');
        !path.is_empty()
            && text.starts_with(path)
            && text[path.len()..]
                .chars()
                .next()
                .is_none_or(|next| matches!(next, ' ' | ':' | '/'))
    })
}

/// A digest of the last sync's, the files' and Studio's whole values, each
/// told apart from a missing one.
fn digest_of(values: [Option<Vec<u8>>; 3]) -> String {
    let mut digest = Sha256::new();
    for value in values {
        match value {
            Some(bytes) => {
                digest.update([1]);
                digest.update((bytes.len() as u64).to_le_bytes());
                digest.update(bytes);
            }
            None => digest.update([0]),
        }
    }
    format!("{:x}", digest.finalize())
}

fn short_hash(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn cut_characters(text: &str) -> Option<String> {
    let length = text.chars().count();
    (length > VALUE_CHARACTERS).then(|| {
        format!(
            "{}… ({length} characters, cut)",
            text.chars().take(VALUE_CHARACTERS).collect::<String>()
        )
    })
}

/// A value as a conflict shows it: whole, or, past 2,000 characters, as its
/// start with its full length.
pub(crate) fn shown_value(value: &Value) -> Value {
    let text = value.to_string();
    match cut_characters(&text) {
        Some(cut) => Value::String(cut),
        None => value.clone(),
    }
}

fn shown_line(line: &str) -> String {
    cut_characters(line).unwrap_or_else(|| line.to_string())
}

fn capped(mut lines: Vec<String>) -> Vec<String> {
    if lines.len() > EXCERPT_LINES {
        let cut = lines.len() - EXCERPT_LINES;
        lines.truncate(EXCERPT_LINES);
        lines.push(format!("… {cut} more lines"));
    }
    lines
}

/// The changes from `base` to `side` as unified diff lines with one line of
/// context, at most 60 of them.
pub(crate) fn text_diff(base: &str, side: &str) -> Vec<String> {
    let diff = similar::TextDiff::from_lines(base, side);
    let mut lines = Vec::new();
    for hunk in diff.unified_diff().context_radius(1).iter_hunks() {
        lines.push(hunk.header().to_string());
        for change in hunk.iter_changes() {
            let sign = match change.tag() {
                similar::ChangeTag::Delete => '-',
                similar::ChangeTag::Insert => '+',
                similar::ChangeTag::Equal => ' ',
            };
            let text = change.value().trim_end_matches('\n');
            lines.push(format!("{sign}{}", shown_line(text)));
        }
    }
    capped(lines)
}

/// The first 60 lines of a text.
pub(crate) fn text_head(text: &str) -> Vec<String> {
    capped(text.lines().map(shown_line).collect())
}

fn entry_description(entry: &SnapshotEntry) -> String {
    match entry {
        SnapshotEntry::Directory => "a folder".to_string(),
        SnapshotEntry::Symlink { target, .. } => format!("a link to {}", target.display()),
        SnapshotEntry::File(bytes) => format!("binary, {} bytes", bytes.len()),
    }
}

/// A value as a sentence quotes it: short, `no value` for none.
pub(crate) fn quoted_value(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return "no value".to_string();
    };
    let text = match value {
        Value::String(text) => format!("{text:?}"),
        other => other.to_string(),
    };
    if text.chars().count() > 60 {
        format!("{}…", text.chars().take(59).collect::<String>())
    } else {
        text
    }
}

/// The sentence of a value conflict: what each place holds.
pub(crate) fn values_sentence(
    subject: &str,
    base: Option<Option<&Value>>,
    files: Option<&Value>,
    studio: Option<&Value>,
) -> String {
    let mut sentence = format!(
        "{subject}: Studio has {}, the files have {}",
        quoted_value(studio),
        quoted_value(files)
    );
    if let Some(base) = base {
        sentence.push_str(&format!(", the last sync had {}", quoted_value(base)));
    }
    sentence
}

/// The conflicts of one comparison, and the decisions it settles them by.
pub(crate) struct ConflictLog {
    decisions: ConflictDecisions,
    open: Vec<Conflict>,
    /// Every id given out, with the identity of its conflict.
    ids: HashMap<String, String>,
    /// The ids whose decision a comparison applied.
    applied: BTreeSet<String>,
    /// Decisions a comparison could not apply, with the reason.
    refused: BTreeMap<String, String>,
    /// The ids the same comparison meets on the way to its decisions: a
    /// decision another decision made moot still names a conflict of it.
    found: HashSet<String>,
    /// The removals decisions took, by store and instance, with each
    /// decision's id.
    removed: HashMap<(String, String), String>,
    /// Decisions that had no effect, with the decisions that made them moot.
    moot: BTreeMap<String, BTreeSet<String>>,
}

/// A decision that had no effect: another decision made its conflict moot
/// or brought back what it removed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct MootDecision {
    pub(crate) id: String,
    pub(crate) decision: ConflictSide,
    /// The decisions that made it moot.
    pub(crate) by: Vec<String>,
}

/// Decisions a comparison could not take; it applies none of its
/// decisions.
#[derive(Debug)]
pub(crate) struct RefusedDecisions {
    /// Why each decision for a conflict met but not decidable that way was
    /// refused.
    pub(crate) reasons: Vec<String>,
    /// The ids no conflict of the comparison has.
    pub(crate) unknown: Vec<String>,
    /// The conflicts open now, none of them decided.
    pub(crate) open: Vec<Conflict>,
}

impl std::fmt::Display for RefusedDecisions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = self.reasons.clone();
        if !self.unknown.is_empty() {
            let ids = self.unknown.iter().map(String::as_str).collect::<Vec<_>>();
            parts.push(unknown_decisions_message(&ids));
            if !self.open.is_empty() {
                let mut shown = self
                    .open
                    .iter()
                    .take(3)
                    .map(|conflict| format!("{} {}", conflict.id, conflict.detail))
                    .collect::<Vec<_>>();
                if self.open.len() > 3 {
                    shown.push(format!("and {} more", self.open.len() - 3));
                }
                parts.push(format!("open now: {}", shown.join("; ")));
            }
        }
        write!(formatter, "{}; nothing was applied", parts.join("; "))
    }
}

impl std::error::Error for RefusedDecisions {}

impl ConflictLog {
    pub(crate) fn new(decisions: impl Into<ConflictDecisions>) -> Self {
        Self {
            decisions: decisions.into(),
            open: Vec::new(),
            ids: HashMap::new(),
            applied: BTreeSet::new(),
            refused: BTreeMap::new(),
            found: HashSet::new(),
            removed: HashMap::new(),
            moot: BTreeMap::new(),
        }
    }

    /// The ids whose decision this comparison applied, in order.
    pub(crate) fn applied(&self) -> &BTreeSet<String> {
        &self.applied
    }

    /// The ids of every conflict this comparison met, settled or open.
    pub(crate) fn met(&self) -> HashSet<String> {
        self.ids.keys().cloned().collect()
    }

    pub(crate) fn has_met(&self, id: &str) -> bool {
        self.ids.contains_key(id)
    }

    /// Takes the ids the comparison meets on the way to its decisions.
    pub(crate) fn found_before_decisions(&mut self, ids: HashSet<String>) {
        self.found = ids;
    }

    /// Gives a conflict its id. A conflict about the same thing met again,
    /// even by another step of the merge, gets the same id; another one
    /// whose identity hashes alike gets the next free one.
    fn assign_id(&mut self, conflict: &mut Conflict) {
        let identity = conflict.identity();
        let mut input = identity.clone();
        for round in 1.. {
            let id = short_hash(&input);
            match self.ids.get(&id) {
                None => {
                    self.ids.insert(id.clone(), identity);
                    conflict.id = id;
                    return;
                }
                Some(known) if *known == identity => {
                    conflict.id = id;
                    return;
                }
                Some(_) => input = format!("{identity}\0{round}"),
            }
        }
    }

    /// The side a conflict takes: the decision for its id; without one,
    /// under a preference, `follows` (the side an earlier decision it hangs
    /// on took) or the preference; else `None`, which leaves it open. The
    /// conflict gets its id; nothing is recorded.
    pub(crate) fn consult(
        &mut self,
        conflict: &mut Conflict,
        follows: Option<ConflictSide>,
    ) -> Option<ConflictSide> {
        self.assign_id(conflict);
        let allowed = |side: ConflictSide| conflict.only.is_none_or(|only| only == side);
        if let Some(side) = self.decisions.by_id.get(&conflict.id).copied() {
            if !conflict.decidable {
                self.refused.insert(
                    conflict.id.clone(),
                    format!(
                        "Conflict {} cannot be settled by choosing a side: {}",
                        conflict.id, conflict.detail
                    ),
                );
                return None;
            }
            if !allowed(side) {
                self.refused.insert(
                    conflict.id.clone(),
                    format!(
                        "Conflict {} can only be decided for {}: {}",
                        conflict.id,
                        conflict.only.map_or("", ConflictSide::name),
                        conflict.detail
                    ),
                );
                return None;
            }
            self.applied.insert(conflict.id.clone());
            let removes = match side {
                ConflictSide::Studio => "studio",
                ConflictSide::Editor => "files",
            };
            if conflict.kind == ConflictKind::Removal
                && conflict.discriminator.is_empty()
                && conflict.absent.iter().any(|place| place == removes)
                && let Some(instance) = &conflict.instance
            {
                self.removed.insert(
                    (conflict.path.clone(), instance.id.clone()),
                    conflict.id.clone(),
                );
            }
            return Some(side);
        }
        if !conflict.decidable {
            return None;
        }
        // A decision by id settles only its own conflict; what hangs on it
        // follows it only where a preference settles everything anyway.
        let preference = self.decisions.preference.side()?;
        Some(follows.unwrap_or(preference)).filter(|side| allowed(*side))
    }

    /// Records a conflict nothing settled.
    pub(crate) fn record(&mut self, mut conflict: Conflict) {
        self.assign_id(&mut conflict);
        self.open.push(conflict);
    }

    /// The side a conflict takes, or `None` after recording it open.
    pub(crate) fn settle(&mut self, mut conflict: Conflict) -> Option<ConflictSide> {
        let side = self.consult(&mut conflict, None);
        if side.is_none() {
            self.open.push(conflict);
        }
        side
    }

    pub(crate) fn open(&self) -> &[Conflict] {
        &self.open
    }

    /// Hears that the decision of `by` brought back the instance `instance`
    /// of the store at `path`: a decision that removed it had no effect.
    pub(crate) fn restored(&mut self, path: &str, instance: &str, by: &mut Conflict) {
        let Some(id) = self
            .removed
            .get(&(path.to_string(), instance.to_string()))
            .cloned()
        else {
            return;
        };
        self.assign_id(by);
        if by.id != id {
            self.moot.entry(id).or_default().insert(by.id.clone());
        }
    }

    /// Notes a decision other decisions made moot.
    pub(crate) fn made_moot(&mut self, id: &str, by: impl IntoIterator<Item = String>) {
        self.moot.entry(id.to_string()).or_default().extend(by);
    }

    /// The decisions this comparison took without effect.
    pub(crate) fn moot(&self) -> Vec<MootDecision> {
        self.moot
            .iter()
            .filter_map(|(id, by)| {
                Some(MootDecision {
                    id: id.clone(),
                    decision: *self.decisions.by_id.get(id)?,
                    by: by.iter().cloned().collect(),
                })
            })
            .collect()
    }

    pub(crate) fn retain(&mut self, keep: impl FnMut(&Conflict) -> bool) {
        self.open.retain(keep);
    }

    /// The decisions that named no conflict this run met: ones another
    /// decision made moot, or stale ones, which only the conflicts met on
    /// the way to the decisions tell apart.
    pub(crate) fn unmatched_decisions(&self) -> Vec<String> {
        self.decisions
            .by_id
            .keys()
            .filter(|id| {
                !self.applied.contains(*id)
                    && !self.refused.contains_key(*id)
                    && !self
                        .open
                        .iter()
                        .any(|conflict| conflict.id == **id && !conflict.decidable)
            })
            .cloned()
            .collect()
    }

    /// The open conflicts in order, or [`RefusedDecisions`] for every
    /// decision that named no conflict of this comparison or could not be
    /// applied: a stale decision never settles something else. A decision
    /// whose conflict another decision made moot (a parent another one
    /// brought back) is no error.
    pub(crate) fn finish(mut self) -> Result<Vec<Conflict>> {
        let mut reasons = Vec::new();
        let mut unknown = Vec::new();
        for id in self.decisions.by_id.keys() {
            if self.applied.contains(id) {
                continue;
            }
            if let Some(reason) = self.refused.get(id) {
                reasons.push(reason.clone());
            } else if let Some(conflict) = self
                .open
                .iter()
                .find(|conflict| conflict.id == *id && !conflict.decidable)
            {
                reasons.push(format!(
                    "Conflict {id} cannot be settled by choosing a side: {}",
                    conflict.detail
                ));
            } else if !self.found.contains(id) {
                unknown.push(id.clone());
            }
        }
        self.order_open();
        if !reasons.is_empty() || !unknown.is_empty() {
            let decisions = &self.decisions;
            self.open
                .retain(|conflict| !decisions.by_id.contains_key(&conflict.id));
            return Err(RefusedDecisions {
                reasons,
                unknown,
                open: self.open,
            }
            .into());
        }
        Ok(self.open)
    }

    fn order_open(&mut self) {
        // One conflict two steps of the merge met alike is listed once.
        self.open.sort_by(|left, right| {
            (&left.path, &left.id, &left.detail).cmp(&(&right.path, &right.id, &right.detail))
        });
        self.open
            .dedup_by(|later, first| later.id == first.id && later.detail == first.detail);
        self.open.sort_by(|left, right| {
            let at = |conflict: &Conflict| {
                conflict
                    .instance
                    .as_ref()
                    .map(|instance| instance.path.clone())
            };
            (&left.path, at(left), &left.detail).cmp(&(&right.path, at(right), &right.detail))
        });
    }
}

pub(crate) fn unknown_decisions_message(ids: &[&str]) -> String {
    format!(
        "No open conflict has the id {}; decide only ids that `rbx lst` lists",
        ids.join(", ")
    )
}

/// The text Live Sync reports for open conflicts: the first three, then a
/// count of the rest by kind and field.
pub(crate) fn conflict_message(conflicts: &[Conflict]) -> String {
    let shown = conflicts
        .iter()
        .take(3)
        .map(|conflict| conflict.detail.as_str())
        .collect::<Vec<_>>();
    let remainder = &conflicts[shown.len()..];
    if remainder.is_empty() {
        return format!("Sync needs review: {}", shown.join("; "));
    }
    let mut counts = Vec::<(String, usize)>::new();
    for conflict in remainder {
        let subject = match &conflict.field {
            Some(field) => format!("{} {field}", conflict.kind.name()),
            None => conflict.kind.name().to_string(),
        };
        match counts.iter_mut().find(|(name, _)| *name == subject) {
            Some((_, count)) => *count += 1,
            None => counts.push((subject, 1)),
        }
    }
    let breakdown = counts
        .iter()
        .map(|(name, count)| format!("{name} x{count}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Sync needs review: {}; and {} more ({breakdown})",
        shown.join("; "),
        remainder.len()
    )
}

/// The conflicts of a pair record. Each is kept as a map, so a record
/// written while conflicts were sentences still loads: such a sentence
/// becomes a conflict no side can be chosen for, which keeps writes held
/// until the next comparison replaces it.
pub(crate) mod stored {
    use super::{Conflict, Value};
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _, ser::Error as _};

    pub(crate) fn serialize<S: Serializer>(
        conflicts: &[Conflict],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let values = conflicts
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(S::Error::custom)?;
        serializer.collect_seq(values)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<Conflict>, D::Error> {
        Vec::<Value>::deserialize(deserializer)?
            .into_iter()
            .map(|value| match value {
                Value::String(text) => Ok(Conflict::legacy(text)),
                value => serde_json::from_value(value).map_err(D::Error::custom),
            })
            .collect()
    }
}

/// Whether a sentence a record written while conflicts were sentences
/// holds is one of verify mode's difference lines.
pub(crate) fn is_difference_line(text: &str) -> bool {
    text == VERIFY_DIFFERENCE_HEADING
        || text.starts_with("push: ")
        || text.starts_with("pull: ")
        || text
            .strip_suffix(" more paths")
            .is_some_and(|count| count.parse::<usize>().is_ok())
}

pub(crate) const VERIFY_DIFFERENCE_HEADING: &str = "Studio and project files differ";

#[cfg(test)]
mod tests {
    use super::*;

    fn property(field: &str) -> Conflict {
        Conflict::new(
            ConflictKind::Property,
            Path::new(r"instances\Workspace.renium"),
            format!("Workspace/Door: property {field}"),
        )
        .instance("door", "Workspace/Door".into())
        .field(field)
    }

    fn id_of(mut conflict: Conflict) -> String {
        ConflictLog::new(ConflictPreference::None).consult(&mut conflict, None);
        conflict.id
    }

    #[test]
    fn an_id_is_eight_hex_characters_of_what_the_conflict_is_about() {
        let first = id_of(property("Transparency"));
        assert_eq!(first.len(), 8);
        assert!(
            first
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        );
        let mut reworded = property("Transparency");
        reworded.detail = "other words, other values".into();
        assert_eq!(id_of(reworded), first, "the sentence is not the identity");
        assert_ne!(id_of(property("Anchored")), first);
        assert_ne!(
            id_of(property("Transparency").discriminated("orphan")),
            first
        );
        let mut attribute = property("Transparency");
        attribute.kind = ConflictKind::Attribute;
        assert_ne!(id_of(attribute), first);
        assert_eq!(
            property("Transparency").path,
            "instances/Workspace.renium",
            "a path names the same file on every system"
        );
    }

    #[test]
    fn an_id_names_the_values_shown_so_a_decision_never_meets_other_ones() {
        let with = |files: &Value| {
            id_of(property("Transparency").values(Some(&json!(0)), Some(files), Some(&json!(0.2))))
        };
        assert_eq!(with(&json!(0.5)), with(&json!(0.5)));
        assert_ne!(with(&json!(0.5)), with(&json!(0.6)));
        // Values that differ only past the cut a record shows.
        let long = |tail: &str| json!(format!("{}{tail}", "x".repeat(3_000)));
        assert_ne!(with(&long("a")), with(&long("b")));
        let missing =
            id_of(property("Transparency").values(Some(&json!(0)), None, Some(&json!(0.2))));
        assert_ne!(missing, with(&Value::Null));
        let text = |files: &str| {
            id_of(
                Conflict::new(ConflictKind::Script, Path::new("src/A.luau"), "a".into()).texts(
                    Some(&SnapshotEntry::File(b"a\n".to_vec())),
                    Some(&SnapshotEntry::File(files.as_bytes().to_vec())),
                    Some(&SnapshotEntry::File(b"c\n".to_vec())),
                ),
            )
        };
        assert_eq!(text("b\n"), text("b\n"));
        assert_ne!(text("b\n"), text("b\r\n"));
    }

    #[test]
    fn identities_that_hash_alike_get_different_ids_in_a_fixed_order() {
        let conflict = property("Transparency");
        let plain = short_hash(&conflict.identity());
        let crowded = || {
            let mut log = ConflictLog::new(ConflictPreference::None);
            log.ids.insert(plain.clone(), "another identity".into());
            log
        };
        let mut log = crowded();
        let mut first = conflict.clone();
        log.consult(&mut first, None);
        assert_ne!(first.id, plain);
        assert_eq!(
            first.id,
            short_hash(&format!("{}\0{}", conflict.identity(), 1))
        );
        let mut again = conflict.clone();
        log.consult(&mut again, None);
        assert_eq!(again.id, first.id, "the same conflict met again");
        let mut replay = conflict.clone();
        crowded().consult(&mut replay, None);
        assert_eq!(replay.id, first.id, "the extension is the same every run");
    }

    #[test]
    fn a_decision_settles_only_its_own_conflict() {
        let id = id_of(property("Transparency"));
        let mut log = ConflictLog::new(ConflictDecisions {
            preference: ConflictPreference::None,
            by_id: BTreeMap::from([(id, ConflictSide::Editor)]),
        });
        assert_eq!(
            log.settle(property("Transparency")),
            Some(ConflictSide::Editor)
        );
        assert_eq!(log.settle(property("Anchored")), None);
        let open = log.finish().unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].field.as_deref(), Some("Anchored"));
    }

    #[test]
    fn a_decision_that_names_no_conflict_fails_the_comparison() {
        let decisions = ConflictDecisions {
            preference: ConflictPreference::Studio,
            by_id: BTreeMap::from([("0000beef".to_string(), ConflictSide::Studio)]),
        };
        let mut log = ConflictLog::new(decisions.clone());
        log.settle(property("Transparency"));
        let error = log.finish().unwrap_err().to_string();
        assert!(
            error.contains("0000beef") && error.ends_with("nothing was applied"),
            "{error}"
        );
        // One that another decision made moot names a conflict the
        // comparison met before any decision.
        let mut log = ConflictLog::new(decisions);
        log.found_before_decisions(HashSet::from(["0000beef".to_string()]));
        assert!(log.finish().unwrap().is_empty());
    }

    #[test]
    fn decisions_read_files_as_editor_and_refuse_anything_else() {
        assert_eq!(
            parse_conflict_decision("0a1b2c3d=files"),
            Ok(("0a1b2c3d".to_string(), ConflictSide::Editor))
        );
        assert_eq!(
            parse_conflict_decision(" 0a1b2c3d = studio "),
            Ok(("0a1b2c3d".to_string(), ConflictSide::Studio))
        );
        for refused in [
            "0a1b2c3d",
            "0a1b2c3=studio",
            "0a1b2c3g=studio",
            "0a1b2c3d=ours",
        ] {
            assert!(parse_conflict_decision(refused).is_err(), "{refused}");
        }
        assert!(parse_conflict_decisions(["0a1b2c3d=studio", "0a1b2c3d=editor"]).is_err());
        assert_eq!(
            parse_conflict_decisions(["0a1b2c3d=studio", "0a1b2c3d=studio"])
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn the_message_shows_three_conflicts_and_counts_the_rest_by_field() {
        let conflicts = ["A", "B", "C", "Transparency", "Transparency", "Anchored"]
            .into_iter()
            .map(property)
            .collect::<Vec<_>>();
        assert_eq!(
            conflict_message(&conflicts),
            "Sync needs review: Workspace/Door: property A; Workspace/Door: property B; Workspace/Door: property C; and 3 more (property Transparency x2, property Anchored x1)"
        );
        assert_eq!(
            conflict_message(&conflicts[..1]),
            "Sync needs review: Workspace/Door: property A"
        );
    }
}
