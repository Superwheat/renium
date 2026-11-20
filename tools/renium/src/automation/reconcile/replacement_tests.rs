use super::*;
use crate::automation::reconcile::push_flow::plan_from_whole_stores;

/// A writer that digests what is written to it.
struct Hashed(Sha256);

impl io::Write for Hashed {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Writes the instances of a document into `hashed`, each in its order, an
/// instance that stands for its unchanged counterpart as such.
fn hash_instances(hashed: &mut Hashed, document: &SettingsBytecode) -> Result<()> {
    use crate::settings::held::{AsPayloads, stands_unchanged};
    serde_json::to_writer(&mut *hashed, &document.version)?;
    for instance in &document.instances {
        serde_json::to_writer(&mut *hashed, &instance.bare())?;
        if stands_unchanged(&instance.properties) {
            hashed.0.update(b"unchanged");
        } else {
            let properties = instance.properties.iter();
            let properties = properties.map(|(name, value)| (name, AsPayloads(value)));
            serde_json::to_writer(&mut *hashed, &properties.collect::<Vec<_>>())?;
        }
        serde_json::to_writer(&mut *hashed, &instance.attributes)?;
    }
    Ok(())
}

/// The digest of what a push sends and verifies: the prepared documents, the
/// stores it imports anew and its plan.
fn fingerprint(
    prepared: &HashMap<PathBuf, PreparedEditorSettingsChange>,
    imported: &HashSet<PathBuf>,
    plan: &ReconcilePushPlan,
) -> Result<String> {
    let mut hashed = Hashed(Sha256::new());
    let mut paths = prepared.keys().collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        let change = &prepared[path];
        serde_json::to_writer(&mut hashed, path)?;
        hash_instances(&mut hashed, &change.previous)?;
        hash_instances(&mut hashed, &change.current)?;
    }
    let sorted = |items: &mut dyn Iterator<Item = String>| {
        let mut items = items.collect::<Vec<_>>();
        items.sort();
        items
    };
    let mut imported = imported.iter().collect::<Vec<_>>();
    imported.sort();
    serde_json::to_writer(
        &mut hashed,
        &(
            imported,
            &plan.changed_paths,
            &plan.target_settings_ids,
            sorted(&mut plan.recreated_settings_ids.iter().cloned()),
            sorted(&mut plan.initial_native_services.iter().cloned()),
            sorted(
                &mut plan
                    .attribute_only_instances
                    .iter()
                    .map(|(service, id)| format!("{service}/{id}")),
            ),
        ),
    )?;
    Ok(format!("{:x}", hashed.0.finalize()))
}

/// Studio's copy of a store: every part holds another Transparency, and with
/// `apart` two leaves under one parent trade places.
fn studio_copy(files: &[u8], apart: bool) -> Result<Vec<u8>> {
    let database = rbx_reflection_database::get()?;
    let mut document = decode_settings_bytecode(files)?;
    for instance in &mut document.instances {
        if crate::rbx::decode::rbx_reflection_class_is_a(database, &instance.class_name, "BasePart")
        {
            let now = match instance.properties.get("Transparency") {
                Some(value) if value == &json!(0.5) => 0.25,
                _ => 0.5,
            };
            instance
                .properties
                .insert("Transparency".into(), json!(now));
        }
    }
    if apart {
        let count = document.instances.len();
        let mut parents = vec![false; count];
        for instance in &document.instances {
            if let Some(parent) = instance.parent_index {
                parents[parent] = true;
            }
        }
        let at = (1..count)
            .find(|&index| {
                let (left, right) = (&document.instances[index - 1], &document.instances[index]);
                !parents[index - 1]
                    && !parents[index]
                    && left.parent_index.is_some()
                    && left.parent_index == right.parent_index
                    && left.name != right.name
            })
            .context("No two leaves under one parent")?;
        document.instances.swap(at - 1, at);
        let to = (0..count)
            .map(|index| {
                Some(match index {
                    index if index == at => at - 1,
                    index if index == at - 1 => at,
                    index => index,
                })
            })
            .collect::<Vec<_>>();
        for instance in &mut document.instances {
            let values = instance.properties.values_mut();
            for value in values.chain(instance.attributes.values_mut()) {
                crate::settings::instance::remap_ref_indices_in_value(value, &to);
            }
        }
    }
    encode_settings_bytecode(&document)
}

/// Times the preparation and the plan of a push that replaces a large
/// service, as `push_flow` makes them, on the stores of a project and
/// Studio's copy of its Workspace store, and prints the digest of what they
/// give.
#[test]
#[ignore = "timing probe: set RENIUM_PREP_PROJECT and RENIUM_PREP_STUDIO (written when missing; RENIUM_PREP_APART=1 moves two leaves)"]
fn whole_service_preparation_probe() -> Result<()> {
    use clap::Parser;
    let project_root =
        PathBuf::from(std::env::var_os("RENIUM_PREP_PROJECT").context("Set RENIUM_PREP_PROJECT")?);
    let studio_path =
        PathBuf::from(std::env::var_os("RENIUM_PREP_STUDIO").context("Set RENIUM_PREP_STUDIO")?);
    let mut project = ProjectSnapshot::default();
    for entry in fs::read_dir(project_root.join("instances"))? {
        let path = entry?.path();
        let relative = path.strip_prefix(&project_root)?.to_path_buf();
        project
            .entries
            .insert(relative, SnapshotEntry::File(fs::read(&path)?));
    }
    let workspace = PathBuf::from("instances").join("Workspace.renium");
    let Some(SnapshotEntry::File(files)) = project.entries.get(&workspace) else {
        bail!("The project holds no Workspace store");
    };
    if !studio_path.is_file() {
        let apart = std::env::var_os("RENIUM_PREP_APART").is_some();
        fs::write(&studio_path, studio_copy(files, apart)?)?;
    }
    let mut studio = project.clone();
    studio.entries.insert(
        workspace.clone(),
        SnapshotEntry::File(fs::read(&studio_path)?),
    );

    let cli = crate::cli::Cli::try_parse_from([
        "rbx",
        "--log-level",
        "trace",
        "--color",
        "never",
        "status",
    ])?;
    crate::app::output::configure(&cli);
    let started = Instant::now();
    let _request = crate::app::profile::request(started);

    let phase = Instant::now();
    let paths = HashSet::from([workspace.clone()]);
    let imported = ImportedStores::default();
    let mut prepared = HashMap::new();
    prepare_project_replacement_importing(
        &project,
        &studio,
        &paths,
        &mut prepared,
        StoresRead::ChangedOfBoth,
        Some(&imported),
    )?;
    let preparation = elapsed_ms(phase);
    let promoted = imported_services(
        ImportedStores::new(imported.lock_recover().clone()),
        &prepared,
    )?;
    let imported = imported.into_inner().unwrap_or_default();

    let phase = Instant::now();
    let differences = prepared.keys().cloned().collect::<HashSet<_>>();
    let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &studio,
        &project,
        &differences,
        &prepared,
        true,
        false,
    )?;
    let mut plan = plan_from_whole_stores(
        (StoresRead::ChangedOfBoth, true, plan),
        (&project, &studio),
        &differences,
        &mut prepared,
    )?;
    let first = elapsed_ms(phase);
    let phase = Instant::now();
    extend_replaced_service_references(&project, &prepared, &promoted, &mut plan)?;
    let extended = elapsed_ms(phase);

    let packs = studio_path.with_extension("packs");
    if packs.exists() {
        fs::remove_dir_all(&packs)?;
    }
    fs::create_dir_all(&packs)?;
    let phase = Instant::now();
    let stored = crate::automation::reconcile::store::StoredSnapshot::write_at(
        &packs,
        project.entries.iter(),
    )?;
    let written = elapsed_ms(phase);
    let phase = Instant::now();
    let again = crate::automation::reconcile::store::StoredSnapshot::write_at(
        &packs,
        project.entries.iter(),
    )?;
    let rewritten = elapsed_ms(phase);
    anyhow::ensure!(stored == again, "A second write stored another snapshot");

    eprintln!(
        "{}",
        crate::app::profile::report("whole service preparation", started).unwrap_or_default()
    );
    let instances = prepared
        .values()
        .map(|change| change.current.instances.len())
        .sum::<usize>();
    eprintln!(
        "preparation {preparation:.0} ms, plan {first:.0} ms, replaced references {extended:.0} ms; {} stores prepared ({instances} instances), {} imported, {} recreated, {} targets; digest {}",
        prepared.len(),
        imported.len(),
        plan.recreated_settings_ids.len(),
        plan.target_settings_ids.len(),
        fingerprint(&prepared, &imported, &plan)?
    );
    eprintln!(
        "last-sync copy written {written:.0} ms, again {rewritten:.0} ms; digest {}",
        crate::system::files::sha256_hex(&serde_json::to_vec(&stored)?)
    );
    Ok(())
}
