use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::app::output::{drop_empty, print_json_output};
use crate::bytecode::acquire_settings_file_lock;
use crate::cli::{LinkApplyArgs, LinkPackArgs, ProjectSourceArgs};
use crate::editor::document::ensure_editor_source_target_in_bytecode;
use crate::editor::paths::{
    build_editor_source_paths_by_index, infer_editor_source_path_spec_in_service,
};
use crate::project::config;
use crate::project::experience::{
    EXPERIENCE_FILE, ExperienceLayout, ExperiencePlace, SharedLinkRecord, find_experience_root,
    load_experience, write_experience_member,
};
use crate::settings::bytecode::{
    SettingsBytecode, decode_settings_bytecode, encode_settings_bytecode,
};
use crate::settings::tree::settings_children_by_parent;
use crate::system::files::{
    absolutize_under, canonical_path, path_key, validate_filesystem_instance_name,
    write_bytes_if_changed,
};

use super::commands::{
    apply_project_links, link_target_output_ordinals, load_link_project, pack_subtree_to_bytecode,
    parse_link_target, resolve_editor_instance_by_path_ordinals,
};
use super::{
    LinkEntry, LinkManifest, LinkSource, LinkTargetRef, RENIUM_STORE_EXTENSION,
    canonicalize_loaded_settings_documents, inline_target_sources, is_package_path,
    link_manifest_path, link_slug, link_target_document_selector_parts, link_target_ordinals,
    link_target_ref_key, link_target_segments, package_document_fingerprint,
    package_target_difference, package_target_matches, read_link_manifest,
    resolve_link_target_storage, selector_starts_with, stray_target_files,
    target_has_inline_scripts, validate_link_target_ref, write_link_manifest,
};

const SHARED_LINKS_KEY: &str = "sharedLinks";
const SHARED_PACKAGE_DIR: &str = "links";

struct PlaceLinks {
    root: PathBuf,
    src_root: PathBuf,
    manifest_path: PathBuf,
    manifest: LinkManifest,
}

struct PackedShare {
    bytes: Vec<u8>,
    package: SettingsBytecode,
    fingerprint: String,
    root_name: String,
    stripped: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ConsumerState {
    Current,
    Drift,
    Missing,
}

impl ConsumerState {
    fn name(self) -> &'static str {
        match self {
            ConsumerState::Current => "ok",
            ConsumerState::Drift => "drift",
            ConsumerState::Missing => "missing",
        }
    }
}

struct LinkedTarget {
    document: SettingsBytecode,
    service: String,
    service_dir: PathBuf,
    segments: Vec<String>,
    ordinals: Vec<usize>,
}

impl LinkedTarget {
    fn drift_detail(&self, package: &SettingsBytecode) -> Result<Option<String>> {
        let (service, segments, ordinals) = (&self.service, &self.segments, &self.ordinals);
        let inlined = inline_target_sources(
            &self.document,
            service,
            &self.service_dir,
            segments,
            ordinals,
        )?;
        if target_has_inline_scripts(&self.document, service, segments, ordinals) {
            return Ok(Some(
                "a script keeps its source inside the store".to_string(),
            ));
        }
        let stray = stray_target_files(
            &self.document,
            service,
            &self.service_dir,
            segments,
            ordinals,
        )?;
        if let Some(path) = stray.first() {
            return Ok(Some(format!("stray file {}", path.display())));
        }
        package_target_difference(&inlined, service, segments, ordinals, package)
    }

    fn state(&self, package: &SettingsBytecode, fingerprint: &str) -> Result<ConsumerState> {
        let (service, segments, ordinals) = (&self.service, &self.segments, &self.ordinals);
        let inlined = inline_target_sources(
            &self.document,
            service,
            &self.service_dir,
            segments,
            ordinals,
        )?;
        let current =
            package_target_matches(&inlined, service, segments, ordinals, package, fingerprint)?
                && !target_has_inline_scripts(&self.document, service, segments, ordinals)
                && stray_target_files(
                    &self.document,
                    service,
                    &self.service_dir,
                    segments,
                    ordinals,
                )?
                .is_empty();
        Ok(if current {
            ConsumerState::Current
        } else {
            ConsumerState::Drift
        })
    }
}

pub(crate) struct SharedPropagation {
    pub(crate) summary: Option<String>,
    pub(crate) failure: Option<String>,
}

fn shared_target(link: &SharedLinkRecord) -> LinkTargetRef {
    LinkTargetRef {
        service: link.service.clone(),
        path: link.path.clone(),
        ords: link.ords.clone(),
    }
}

fn target_label(target: &LinkTargetRef) -> String {
    std::iter::once(target.service.clone())
        .chain(link_target_segments(target))
        .collect::<Vec<_>>()
        .join(".")
}

fn shared_package_path(experience_root: &Path, id: &str) -> PathBuf {
    experience_root
        .join(SHARED_PACKAGE_DIR)
        .join(format!("{id}.{RENIUM_STORE_EXTENSION}"))
}

fn consumer_source_path(experience_root: &Path, place_root: &Path, id: &str) -> String {
    let depth = place_root
        .strip_prefix(experience_root)
        .map_or(0, |relative| relative.components().count());
    format!(
        "{}{SHARED_PACKAGE_DIR}/{id}.{RENIUM_STORE_EXTENSION}",
        "../".repeat(depth)
    )
}

fn lexical_path(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() {
                    output.push("..");
                }
            }
            other => output.push(other.as_os_str()),
        }
    }
    output
}

/// A link source that resolves into the experience's shared package folder
/// belongs to every place, so per-place commands must not rewrite or delete it.
pub(super) fn experience_package_path(project_root: &Path, raw: &str) -> Option<PathBuf> {
    if !is_package_path(Path::new(raw)) {
        return None;
    }
    let project_root = canonical_path(project_root).ok()?;
    let candidate = lexical_path(&absolutize_under(&project_root, Path::new(raw)));
    let experience = find_experience_root(&project_root).ok().flatten()?;
    let folder = path_key(&experience.join(SHARED_PACKAGE_DIR));
    candidate
        .parent()
        .is_some_and(|parent| path_key(parent) == folder)
        .then_some(candidate)
}

fn load_shared_experience(start: &Path) -> Result<Option<ExperienceLayout>> {
    let Some(layout) = load_experience(start)? else {
        return Ok(None);
    };
    let mut ids = HashSet::new();
    for link in &layout.shared_links {
        validate_filesystem_instance_name(&link.id, "shared link id")?;
        if !ids.insert(link.id.as_str()) {
            bail!(
                "Shared link {} appears more than once in {EXPERIENCE_FILE}",
                link.id
            );
        }
        validate_link_target_ref(&shared_target(link))
            .with_context(|| format!("Invalid shared link {} in {EXPERIENCE_FILE}", link.id))?;
    }
    Ok(Some(layout))
}

fn load_place_links(place_root: &Path) -> Result<PlaceLinks> {
    let root = canonical_path(place_root)
        .with_context(|| format!("Failed to resolve place {}", place_root.display()))?;
    let source_root = config::try_load_project(None, Some(&root))?
        .filter(|loaded| canonical_path(&loaded.root).is_ok_and(|loaded| loaded == root))
        .map_or_else(|| PathBuf::from("src"), |loaded| loaded.project.source_root);
    let manifest_path = link_manifest_path(&root, Path::new("renium-link.json"));
    let manifest = read_link_manifest(&manifest_path)?;
    Ok(PlaceLinks {
        src_root: absolutize_under(&root, &source_root),
        root,
        manifest_path,
        manifest,
    })
}

fn overlapping_link<'a>(manifest: &'a LinkManifest, target: &LinkTargetRef) -> Option<&'a str> {
    let broken = manifest
        .broken
        .iter()
        .map(link_target_ref_key)
        .collect::<HashSet<_>>();
    let segments = link_target_segments(target);
    let ordinals = link_target_ordinals(target);
    manifest
        .links
        .iter()
        .find(|link| {
            link.targets.iter().any(|existing| {
                if existing.service != target.service
                    || broken.contains(&link_target_ref_key(existing))
                {
                    return false;
                }
                let existing_segments = link_target_segments(existing);
                let existing_ordinals = link_target_ordinals(existing);
                selector_starts_with(&segments, &ordinals, &existing_segments, &existing_ordinals)
                    || selector_starts_with(
                        &existing_segments,
                        &existing_ordinals,
                        &segments,
                        &ordinals,
                    )
            })
        })
        .map(|link| link.id.as_str())
}

fn read_linked_target(links: &PlaceLinks, target: &LinkTargetRef) -> Result<Option<LinkedTarget>> {
    let storage = resolve_link_target_storage(&links.root, &links.src_root, target, false, true)?;
    let Some(settings_file) = storage.settings_file.as_ref().filter(|path| path.is_file()) else {
        return Ok(None);
    };
    let mut documents = HashMap::from([(
        settings_file.clone(),
        SettingsBytecode::read_file(settings_file)?,
    )]);
    canonicalize_loaded_settings_documents(&mut documents)?;
    let document = documents
        .remove(settings_file)
        .context("Settings store vanished while loading")?;
    let (service, segments, ordinals) = link_target_document_selector_parts(
        &target.service,
        &link_target_segments(target),
        &link_target_ordinals(target),
        &storage,
        &document,
    )?;
    if resolve_editor_instance_by_path_ordinals(&document, &service, &segments, &ordinals).is_none()
    {
        return Ok(None);
    }
    Ok(Some(LinkedTarget {
        document,
        service,
        service_dir: storage.source_root,
        segments,
        ordinals,
    }))
}

fn pack_shared_subtree(
    place: &PlaceLinks,
    target: &LinkTargetRef,
    enforce_ownership: bool,
) -> Result<PackedShare> {
    let label = target_label(target);
    let storage = resolve_link_target_storage(
        &place.root,
        &place.src_root,
        target,
        enforce_ownership,
        false,
    )?;
    let settings_file = storage
        .settings_file
        .as_ref()
        .with_context(|| format!("{label} has no Renium store"))?;
    let guard = acquire_settings_file_lock(settings_file)?;
    let mut document = SettingsBytecode::read_file(settings_file)?;
    let (service, segments, ordinals) = link_target_document_selector_parts(
        &target.service,
        &link_target_segments(target),
        &link_target_ordinals(target),
        &storage,
        &document,
    )?;
    let root = resolve_editor_instance_by_path_ordinals(&document, &service, &segments, &ordinals)
        .with_context(|| format!("{label} was not found in {}", place.root.display()))?;
    for file in stray_target_files(
        &document,
        &service,
        &storage.source_root,
        &segments,
        &ordinals,
    )? {
        if let Some(spec) =
            infer_editor_source_path_spec_in_service(&storage.source_root, &service, &file)
        {
            ensure_editor_source_target_in_bytecode(&mut document, &spec)?;
        }
    }
    let package_links = settings_children_by_parent(&document)
        .get(root)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .copied()
        .filter(|child| document.instances[*child].class_name == "PackageLink")
        .collect::<Vec<_>>();
    let source_paths =
        build_editor_source_paths_by_index(&document, &service, &storage.source_root);
    let (package, _) = pack_subtree_to_bytecode(&document, root, &source_paths, &package_links)?;
    drop(guard);
    let bytes = encode_settings_bytecode(&package)?;
    let package = decode_settings_bytecode(&bytes)?;
    Ok(PackedShare {
        fingerprint: package_document_fingerprint(&package)?,
        root_name: document.instances[root].name.clone(),
        stripped: package_links
            .iter()
            .map(|index| format!("{label}.{}", document.instances[*index].name))
            .collect(),
        bytes,
        package,
    })
}

fn stored_package(path: &Path) -> Option<(SettingsBytecode, String)> {
    let package = SettingsBytecode::read_file(path).ok()?;
    let fingerprint = package_document_fingerprint(&package).ok()?;
    Some((package, fingerprint))
}

fn consumer_link<'a>(
    layout: &ExperienceLayout,
    place: &ExperiencePlace,
    manifest: &'a LinkManifest,
    id: &str,
) -> Option<&'a LinkEntry> {
    let expected = consumer_source_path(&layout.root, &place.root, id);
    manifest.links.iter().find(|link| {
        link.id == id
            && matches!(&link.source, LinkSource::Local { path } if path.replace('\\', "/") == expected)
    })
}

fn consumer_state(
    layout: &ExperienceLayout,
    place: &ExperiencePlace,
    id: &str,
    package: &SettingsBytecode,
    fingerprint: &str,
) -> Result<Option<(PlaceLinks, ConsumerState)>> {
    let links = load_place_links(&place.root)?;
    let Some(link) = consumer_link(layout, place, &links.manifest, id) else {
        return Ok(None);
    };
    let broken = links
        .manifest
        .broken
        .iter()
        .map(link_target_ref_key)
        .collect::<HashSet<_>>();
    let targets = link
        .targets
        .iter()
        .filter(|target| !broken.contains(&link_target_ref_key(target)))
        .cloned()
        .collect::<Vec<_>>();
    let mut state = None;
    for target in &targets {
        let target_state = match read_linked_target(&links, target)? {
            None => ConsumerState::Missing,
            Some(linked) => linked.state(package, fingerprint)?,
        };
        state = Some(state.map_or(target_state, |state: ConsumerState| state.max(target_state)));
    }
    Ok(state.map(|state| (links, state)))
}

/// What the status compares when it reports drift: the first differing
/// instance of the first drifting target, computed on the same inlined copy.
fn consumer_drift_detail(
    links: &PlaceLinks,
    layout: &ExperienceLayout,
    place: &ExperiencePlace,
    id: &str,
    package: &SettingsBytecode,
) -> Result<Option<String>> {
    let Some(link) = consumer_link(layout, place, &links.manifest, id) else {
        return Ok(None);
    };
    for target in &link.targets {
        let Some(linked) = read_linked_target(links, target)? else {
            return Ok(Some("target is missing".to_string()));
        };
        if let Some(detail) = linked.drift_detail(package)? {
            return Ok(Some(detail));
        }
    }
    Ok(None)
}

fn consumer_apply_args(place_root: &Path, id: &str) -> LinkApplyArgs {
    LinkApplyArgs {
        project: ProjectSourceArgs {
            project_root: place_root.to_path_buf(),
            src_root: PathBuf::from("src"),
        },
        manifest: PathBuf::from("renium-link.json"),
        link: Some(id.to_string()),
        check: false,
        force_targets: false,
        force_target: Vec::new(),
        offline: true,
        strict: false,
        git_path: "git".to_string(),
        wally_path: "wally".to_string(),
        cache_dir: None,
        experience: false,
        shared_consumer: true,
        pretty: false,
    }
}

struct ConsumerUpdate {
    state: &'static str,
    replaced: usize,
}

fn apply_consumer(links: PlaceLinks, id: &str) -> Result<ConsumerUpdate> {
    let args = consumer_apply_args(&links.root, id);
    let (result, _) = apply_project_links(
        &args,
        &links.root,
        &links.src_root,
        &links.manifest_path,
        links.manifest,
    )?;
    let warnings = result
        .get("warnings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    if !warnings.is_empty() {
        bail!("{}", warnings.join("; "));
    }
    let detached = result
        .get("links")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|link| link.get("deletedTarget") == Some(&Value::Bool(true)));
    Ok(ConsumerUpdate {
        state: if detached { "detached" } else { "updated" },
        replaced: result
            .get("replacedFiles")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
    })
}

fn update_consumer(
    layout: &ExperienceLayout,
    place: &ExperiencePlace,
    id: &str,
    packed: &PackedShare,
) -> Result<Option<ConsumerUpdate>> {
    let Some((links, state)) =
        consumer_state(layout, place, id, &packed.package, &packed.fingerprint)?
    else {
        return Ok(None);
    };
    if state == ConsumerState::Current {
        return Ok(Some(ConsumerUpdate {
            state: state.name(),
            replaced: 0,
        }));
    }
    apply_consumer(links, id).map(Some)
}

struct Propagation<'a> {
    skip: Option<&'a str>,
    consumers_when_current: bool,
}

fn propagate_link(
    layout: &ExperienceLayout,
    link: &SharedLinkRecord,
    propagation: &Propagation<'_>,
) -> Value {
    let mut entry = Map::new();
    entry.insert("id".to_string(), json!(link.id));
    entry.insert("src".to_string(), json!(link.source));
    if let Err(error) = propagate_link_into(layout, link, propagation, &mut entry) {
        entry.insert("error".to_string(), json!(format!("{error:#}")));
    }
    Value::Object(entry)
}

fn propagate_link_into(
    layout: &ExperienceLayout,
    link: &SharedLinkRecord,
    propagation: &Propagation<'_>,
    entry: &mut Map<String, Value>,
) -> Result<()> {
    let source = layout.place(&link.source).with_context(|| {
        format!(
            "Source place {} is not in {EXPERIENCE_FILE}; share it again from its place",
            link.source
        )
    })?;
    let packed = pack_shared_subtree(
        &load_place_links(&source.root)?,
        &shared_target(link),
        false,
    )?;
    let package_path = shared_package_path(&layout.root, &link.id);
    let repacked = stored_package(&package_path)
        .is_none_or(|(_, fingerprint)| fingerprint != packed.fingerprint);
    if repacked {
        write_bytes_if_changed(&package_path, &packed.bytes)?;
        entry.insert("repacked".to_string(), Value::Bool(true));
    }
    if !repacked && !propagation.consumers_when_current {
        return Ok(());
    }
    let mut places = Map::new();
    let mut replaced = Map::new();
    let mut errors = Map::new();
    for place in &layout.places {
        if place.alias == link.source || propagation.skip == Some(place.alias.as_str()) {
            continue;
        }
        match update_consumer(layout, place, &link.id, &packed) {
            Ok(Some(update)) => {
                places.insert(place.alias.clone(), json!(update.state));
                if update.replaced > 0 {
                    replaced.insert(place.alias.clone(), json!(update.replaced));
                }
            }
            Ok(None) => {}
            Err(error) => {
                errors.insert(place.alias.clone(), json!(format!("{error:#}")));
            }
        }
    }
    for (key, map) in [
        ("places", places),
        ("replaced", replaced),
        ("errors", errors),
    ] {
        if !map.is_empty() {
            entry.insert(key.to_string(), Value::Object(map));
        }
    }
    Ok(())
}

fn status_entry(layout: &ExperienceLayout, link: &SharedLinkRecord, skip: Option<&str>) -> Value {
    let target = shared_target(link);
    let mut entry = Map::new();
    entry.insert("id".to_string(), json!(link.id));
    entry.insert("src".to_string(), json!(link.source));
    entry.insert("service".to_string(), json!(link.service));
    entry.insert("path".to_string(), json!(link_target_segments(&target)));
    let stored = stored_package(&shared_package_path(&layout.root, &link.id));
    let packed = layout
        .place(&link.source)
        .with_context(|| format!("Source place {} is not in {EXPERIENCE_FILE}", link.source))
        .and_then(|source| pack_shared_subtree(&load_place_links(&source.root)?, &target, false));
    match packed {
        Ok(packed) => {
            let stale = stored
                .as_ref()
                .is_none_or(|(_, fingerprint)| *fingerprint != packed.fingerprint);
            entry.insert("stale".to_string(), Value::Bool(stale));
        }
        Err(error) => {
            entry.insert("error".to_string(), json!(format!("{error:#}")));
        }
    }
    let Some((package, fingerprint)) = stored else {
        entry.insert("package".to_string(), json!("missing"));
        return Value::Object(entry);
    };
    let mut places = Map::new();
    let mut errors = Map::new();
    let mut drift = Map::new();
    for place in &layout.places {
        if place.alias == link.source || skip == Some(place.alias.as_str()) {
            continue;
        }
        match consumer_state(layout, place, &link.id, &package, &fingerprint) {
            Ok(Some((links, state))) => {
                places.insert(place.alias.clone(), json!(state.name()));
                if state == ConsumerState::Drift
                    && let Ok(Some(detail)) =
                        consumer_drift_detail(&links, layout, place, &link.id, &package)
                {
                    drift.insert(place.alias.clone(), json!(detail));
                }
            }
            Ok(None) => {}
            Err(error) => {
                errors.insert(place.alias.clone(), json!(format!("{error:#}")));
            }
        }
    }
    entry.insert("places".to_string(), Value::Object(places));
    if !drift.is_empty() {
        entry.insert("drift".to_string(), Value::Object(drift));
    }
    if !errors.is_empty() {
        entry.insert("errors".to_string(), Value::Object(errors));
    }
    Value::Object(entry)
}

fn shared_entries(
    layout: &ExperienceLayout,
    only: Option<&str>,
    check: bool,
    skip: Option<&str>,
) -> Vec<Value> {
    let propagation = Propagation {
        skip,
        consumers_when_current: true,
    };
    layout
        .shared_links
        .iter()
        .filter(|link| only.is_none_or(|id| id == link.id))
        .map(|link| {
            if check {
                status_entry(layout, link, skip)
            } else {
                propagate_link(layout, link, &propagation)
            }
        })
        .collect()
}

fn entries_ok(entries: &[Value]) -> bool {
    entries
        .iter()
        .all(|entry| entry.get("error").is_none() && entry.get("errors").is_none())
}

pub(super) fn apply_place_shared_links(
    project_root: &Path,
    only: Option<&str>,
    check: bool,
) -> Result<Option<Value>> {
    let Some(layout) = load_shared_experience(project_root)? else {
        return Ok(None);
    };
    let skip = layout
        .place_containing(project_root)
        .map(|place| place.alias.clone());
    let entries = shared_entries(&layout, only, check, skip.as_deref());
    Ok((!entries.is_empty()).then_some(Value::Array(entries)))
}

pub(super) fn experience_shared_links(
    start: &Path,
    only: Option<&str>,
    check: bool,
) -> Result<Value> {
    let layout = load_shared_experience(start)?
        .with_context(|| format!("No {EXPERIENCE_FILE} was found above {}", start.display()))?;
    let entries = shared_entries(&layout, only, check, None);
    Ok(json!({ "ok": entries_ok(&entries), "shared": entries }))
}

pub(super) fn shared_link_status(start: &Path) -> Result<Option<Value>> {
    let Some(layout) = load_shared_experience(start)? else {
        return Ok(None);
    };
    let entries = shared_entries(&layout, None, true, None);
    Ok((!entries.is_empty()).then_some(Value::Array(entries)))
}

fn path_within(path: &Path, prefix: &Path) -> bool {
    let path = path_key(path);
    let prefix = path_key(prefix);
    let prefix = prefix.trim_end_matches('/');
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

pub(crate) fn shared_link_sources_touched(project_root: &Path, paths: &[PathBuf]) -> Result<bool> {
    if paths.is_empty() {
        return Ok(false);
    }
    let Some(layout) = load_shared_experience(project_root)? else {
        return Ok(false);
    };
    let Some(place) = layout.place_containing(project_root) else {
        return Ok(false);
    };
    let shared = layout
        .shared_links
        .iter()
        .filter(|link| link.source == place.alias)
        .collect::<Vec<_>>();
    if shared.is_empty() {
        return Ok(false);
    }
    let links = load_place_links(&place.root)?;
    let changed = paths
        .iter()
        .map(|path| {
            path.strip_prefix(project_root)
                .map_or_else(|_| path.clone(), |relative| links.root.join(relative))
        })
        .collect::<Vec<_>>();
    for link in shared {
        let storage = resolve_link_target_storage(
            &links.root,
            &links.src_root,
            &shared_target(link),
            false,
            true,
        )?;
        let stores = [&storage.settings_file, &storage.settings_output_file]
            .into_iter()
            .flatten()
            .map(|path| path_key(path))
            .collect::<HashSet<_>>();
        if changed
            .iter()
            .any(|path| path_within(path, &storage.target_path) || stores.contains(&path_key(path)))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn propagate_shared_links_from(project_root: &Path) -> Result<SharedPropagation> {
    let mut outcome = SharedPropagation {
        summary: None,
        failure: None,
    };
    let Some(layout) = load_shared_experience(project_root)? else {
        return Ok(outcome);
    };
    let Some(source) = layout.place_containing(project_root) else {
        return Ok(outcome);
    };
    let propagation = Propagation {
        skip: None,
        consumers_when_current: false,
    };
    let mut summaries = Vec::new();
    let mut failures = Vec::new();
    for link in layout
        .shared_links
        .iter()
        .filter(|link| link.source == source.alias)
    {
        let entry = propagate_link(&layout, link, &propagation);
        if let Some(error) = entry.get("error").and_then(Value::as_str) {
            failures.push(format!("{}: {error}", link.id));
        }
        for (place, error) in entry
            .get("errors")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            failures.push(format!(
                "{} in {place}: {}",
                link.id,
                error.as_str().unwrap_or_default()
            ));
        }
        if entry.get("repacked") == Some(&Value::Bool(true)) {
            let updated = entry
                .get("places")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter(|(_, state)| state.as_str() != Some("ok"))
                .map(|(place, _)| place.as_str())
                .collect::<Vec<_>>();
            summaries.push(if updated.is_empty() {
                format!("{} repacked from {}", link.id, link.source)
            } else {
                format!(
                    "{} repacked from {}, applied to {}",
                    link.id,
                    link.source,
                    updated.join(", ")
                )
            });
        }
    }
    outcome.summary = (!summaries.is_empty()).then(|| summaries.join("; "));
    outcome.failure = (!failures.is_empty()).then(|| failures.join("; "));
    Ok(outcome)
}

fn upsert_shared_record(
    layout: &ExperienceLayout,
    records: &mut Vec<SharedLinkRecord>,
    record: &SharedLinkRecord,
) -> Result<bool> {
    let target = shared_target(record);
    let key = link_target_ref_key(&target);
    if let Some(other) = records.iter().find(|existing| {
        existing.id != record.id
            && existing.source == record.source
            && link_target_ref_key(&shared_target(existing)) == key
    }) {
        bail!(
            "{} is already shared as {}",
            target_label(&target),
            other.id
        );
    }
    let Some(existing) = records.iter_mut().find(|existing| existing.id == record.id) else {
        records.push(record.clone());
        return Ok(true);
    };
    let same_target = link_target_ref_key(&shared_target(existing)) == key;
    if !same_target || existing.source != record.source && layout.place(&existing.source).is_some()
    {
        bail!(
            "Shared link {} already shares {} from {}; choose another --id",
            existing.id,
            target_label(&shared_target(existing)),
            existing.source
        );
    }
    let changed = existing != record;
    *existing = record.clone();
    Ok(changed)
}

fn link_consumer_place(
    layout: &ExperienceLayout,
    place: &ExperiencePlace,
    record: &SharedLinkRecord,
    create: bool,
) -> Result<std::result::Result<usize, String>> {
    let target = shared_target(record);
    let label = target_label(&target);
    let key = link_target_ref_key(&target);
    let mut links = load_place_links(&place.root)?;
    let source_path = consumer_source_path(&layout.root, &place.root, &record.id);
    let existing = links
        .manifest
        .links
        .iter()
        .position(|link| link.id == record.id);
    if let Some(index) = existing
        && !matches!(
            &links.manifest.links[index].source,
            LinkSource::Local { path } if path.replace('\\', "/") == source_path
        )
    {
        return Ok(Err(format!(
            "link id {} is already used by another link there",
            record.id
        )));
    }
    if let Some(owner) =
        overlapping_link(&links.manifest, &target).filter(|owner| *owner != record.id)
    {
        return Ok(Err(format!("{label} overlaps its link {owner}")));
    }
    if !create && read_linked_target(&links, &target)?.is_none() {
        return Ok(Err(format!("no {label}")));
    }
    match existing {
        Some(index) => {
            let link = &mut links.manifest.links[index];
            if !link
                .targets
                .iter()
                .any(|candidate| link_target_ref_key(candidate) == key)
            {
                link.targets.push(target);
            }
        }
        None => links.manifest.links.push(LinkEntry {
            id: record.id.clone(),
            read_only: true,
            source: LinkSource::Local { path: source_path },
            targets: vec![target],
        }),
    }
    links
        .manifest
        .broken
        .retain(|broken| link_target_ref_key(broken) != key);
    write_link_manifest(&links.manifest_path, &links.manifest)?;
    Ok(Ok(apply_consumer(links, &record.id)?.replaced))
}

pub(super) fn share_link_pack(mut args: LinkPackArgs) -> Result<()> {
    if args.target.writable {
        bail!("Shared links are read-only in the other places; drop --writable");
    }
    let (project_root, src_root, manifest_path, manifest) =
        load_link_project(&mut args.project, &args.manifest)?;
    let layout = load_shared_experience(&project_root)?.with_context(|| {
        format!("--share needs a multi-place experience; no {EXPERIENCE_FILE} was found above this place")
    })?;
    let source = layout
        .place_containing(&project_root)
        .with_context(|| {
            format!(
                "{} is not one of the experience's places",
                project_root.display()
            )
        })?
        .alias
        .clone();
    let target = parse_link_target(
        &args.target.service,
        &args.target.path_segments_json,
        &args.target.path_ordinals_json,
    )?;
    let label = target_label(&target);
    if let Some(owner) = overlapping_link(&manifest, &target) {
        bail!(
            "{label} overlaps link \"{owner}\" in this place; only an editable subtree can be shared"
        );
    }
    let source_links = PlaceLinks {
        root: project_root,
        src_root,
        manifest_path,
        manifest,
    };
    let packed = pack_shared_subtree(&source_links, &target, true)?;
    let id = args
        .id
        .take()
        .unwrap_or_else(|| link_slug(&packed.root_name));
    validate_filesystem_instance_name(&id, "link id")?;
    let mut path = vec![target.service.clone()];
    path.extend(link_target_segments(&target));
    let record = SharedLinkRecord {
        id: id.clone(),
        source: source.clone(),
        service: target.service.clone(),
        path,
        ords: link_target_output_ordinals(link_target_ordinals(&target)),
    };
    let mut records = layout.shared_links.clone();
    let records_changed = upsert_shared_record(&layout, &mut records, &record)?;
    let package_path = shared_package_path(&layout.root, &id);
    write_bytes_if_changed(&package_path, &packed.bytes)?;
    if records_changed {
        write_experience_member(
            &layout.root,
            SHARED_LINKS_KEY,
            &serde_json::to_value(&records)?,
        )?;
    }
    let mut linked = Vec::new();
    let mut replaced = Map::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();
    for place in layout.places.iter().filter(|place| place.alias != source) {
        match link_consumer_place(&layout, place, &record, args.all_places) {
            Ok(Ok(count)) => {
                linked.push(place.alias.clone());
                if count > 0 {
                    replaced.insert(place.alias.clone(), json!(count));
                }
            }
            Ok(Err(reason)) => skipped.push(json!({ "place": place.alias, "reason": reason })),
            Err(error) => {
                failed.push(json!({ "place": place.alias, "error": format!("{error:#}") }));
            }
        }
    }
    let mut result = json!({
        "ok": failed.is_empty(),
        "id": id,
        "source": source,
        "service": target.service,
        "path": link_target_segments(&target),
        "package": package_path,
        "instances": packed.package.instances.len(),
        "linked": linked,
        "skipped": skipped,
        "failed": failed,
        "strippedPackageLinks": packed.stripped,
    });
    if !packed.stripped.is_empty() {
        result["next"] = json!(format!(
            "The Roblox PackageLink stays out of the package, so linked copies drop it. `rbx --place {source} upl {} -i {label}` unlinks the Roblox package in the source place; run it in any other place that still has it",
            target.service
        ));
    }
    if !replaced.is_empty() {
        result["replaced"] = Value::Object(replaced);
    }
    drop_empty(&mut result, &["skipped", "failed", "strippedPackageLinks"]);
    print_json_output(&result, args.pretty)
}
