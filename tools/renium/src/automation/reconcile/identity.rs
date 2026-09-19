use super::*;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PairIdentity {
    pub(crate) experience: String,
    pub(crate) project: String,
    pub(crate) fingerprint: String,
    pub(crate) game_id: Option<i64>,
    pub(crate) place_id: Option<i64>,
    #[serde(default)]
    pub(crate) local_file: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LocalFileStamp {
    pub(crate) length: u64,
    pub(crate) modified_seconds: u64,
    pub(crate) modified_nanos: u32,
}

pub(crate) fn local_file_stamp(path: Option<&str>) -> Result<Option<LocalFileStamp>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to inspect local place file {path}"));
        }
    };
    let modified = metadata
        .modified()
        .with_context(|| format!("Failed to read local place timestamp for {path}"))?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Ok(Some(LocalFileStamp {
        length: metadata.len(),
        modified_seconds: modified.as_secs(),
        modified_nanos: modified.subsec_nanos(),
    }))
}

pub(crate) fn local_file_digest(path: Option<&str>) -> Result<Option<String>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to open local place file {path}"));
        }
    };
    let mut digest = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("Failed to read local place file {path}"))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(Some(format!("{:x}", digest.finalize())))
}

pub(crate) fn should_bootstrap_studio_from_editor(
    mode: PairMode,
    previous_runtime_id: Option<&str>,
    current_runtime_id: Option<&str>,
    previous_local_file_digest: Option<&str>,
    current_local_file_digest: Option<&str>,
) -> bool {
    mode == PairMode::Reconcile
        && previous_runtime_id.is_some()
        && previous_runtime_id != current_runtime_id
        && previous_local_file_digest.is_some()
        && previous_local_file_digest == current_local_file_digest
}

impl PairIdentity {
    pub(crate) fn from_context(context: &BoundContext, bridge: &BridgeServer) -> Result<Self> {
        let published = context.game_id.is_some_and(|value| value > 0)
            && context.place_id.is_some_and(|value| value > 0);
        Ok(Self {
            experience: canonical_string(Path::new(&context.experience))?,
            project: canonical_string(Path::new(&context.root))?,
            fingerprint: context.fingerprint.clone(),
            game_id: context.game_id.filter(|value| *value > 0),
            place_id: context.place_id.filter(|value| *value > 0),
            local_file: if published {
                None
            } else {
                context
                    .runtime_id
                    .as_deref()
                    .and_then(|runtime_id| local_place_path_for_runtime(bridge, runtime_id))
                    .or(saved_local_file_for_context(context)?)
                    .map(|path| canonical_string(&path))
                    .transpose()?
            },
        })
    }

    pub(crate) fn pair_key(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(self.experience.as_bytes());
        hash.update([0]);
        hash.update(self.project.as_bytes());
        hash.update([0]);
        hash.update(self.game_id.unwrap_or_default().to_le_bytes());
        hash.update(self.place_id.unwrap_or_default().to_le_bytes());
        if let Some(local_file) = &self.local_file {
            hash.update([0]);
            hash.update(local_file.as_bytes());
        }
        format!("{:x}", hash.finalize())
    }

    pub(crate) fn same_pair(&self, other: &Self) -> bool {
        self.experience == other.experience
            && self.project == other.project
            && self.game_id == other.game_id
            && self.place_id == other.place_id
            && self.local_file == other.local_file
    }

    pub(crate) fn target_key(&self) -> String {
        match (self.game_id, self.place_id) {
            (Some(game_id), Some(place_id)) => format!("published:{game_id}:{place_id}"),
            _ => self.local_file.as_ref().map_or_else(
                || format!("local-unresolved:{}", self.project),
                |path| format!("local-file:{path}"),
            ),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RecoveryHead {
    pub(crate) editor_before: String,
    pub(crate) studio_before: String,
    pub(crate) intended: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StudioCheckpoint {
    pub(crate) runtime_id: String,
    pub(crate) change_tracker_version: u64,
    pub(crate) seq: u64,
    pub(crate) service_generations: BTreeMap<String, u64>,
}

pub(crate) fn checkpoint_generations(state: &Value) -> Option<&Map<String, Value>> {
    state["checkpointGenerations"]
        .as_object()
        .or_else(|| state["serviceGenerations"].as_object())
}

impl StudioCheckpoint {
    pub(crate) fn from_state(context: &BoundContext, state: &Value) -> Option<Self> {
        let services = sync_services();
        if state["tracking"].as_bool() != Some(true)
            || state["trackedServices"].as_u64() != u64::try_from(services.len()).ok()
            || !state["dirtyServices"].as_array().is_some_and(Vec::is_empty)
            || !state["fullSyncServices"]
                .as_array()
                .is_some_and(Vec::is_empty)
        {
            return None;
        }
        let runtime_id = state["runtimeId"].as_str()?;
        if context.runtime_id.as_deref() != Some(runtime_id) {
            return None;
        }
        let generations = checkpoint_generations(state)?;
        if generations.len() != services.len() {
            return None;
        }
        let service_generations = services
            .into_iter()
            .map(|service| Some((service.clone(), generations.get(&service)?.as_u64()?)))
            .collect::<Option<_>>()?;
        Some(Self {
            runtime_id: runtime_id.to_string(),
            change_tracker_version: state["changeTrackerVersion"].as_u64()?,
            seq: state["seq"].as_u64()?,
            service_generations,
        })
    }

    pub(crate) fn matches_state(&self, context: &BoundContext, state: &Value) -> bool {
        Self::from_state(context, state).as_ref() == Some(self)
    }

    pub(crate) fn changed_services(
        &self,
        context: &BoundContext,
        state: &Value,
    ) -> Option<Vec<String>> {
        let services = sync_services();
        if state["tracking"].as_bool() != Some(true)
            || state["trackedServices"].as_u64() != u64::try_from(services.len()).ok()
            || state["runtimeId"].as_str()? != self.runtime_id
            || context.runtime_id.as_deref() != Some(self.runtime_id.as_str())
            || state["changeTrackerVersion"].as_u64()? != self.change_tracker_version
            || state["seq"].as_u64()? < self.seq
            || state["referencePathsMayChange"].as_bool() == Some(true)
        {
            return None;
        }
        let generations = checkpoint_generations(state)?;
        if generations.len() != services.len() {
            return None;
        }
        let allowed = services.iter().map(String::as_str).collect::<HashSet<_>>();
        let mut changed = BTreeSet::new();
        for key in ["dirtyServices", "fullSyncServices"] {
            for service in state[key].as_array()?.iter().map(Value::as_str) {
                let service = service?;
                if !allowed.contains(service) {
                    return None;
                }
                changed.insert(service.to_string());
            }
        }
        for service in &services {
            let current = generations.get(service)?.as_u64()?;
            if self.service_generations.get(service) != Some(&current) {
                changed.insert(service.clone());
            }
        }
        if state["seq"].as_u64()? != self.seq && changed.is_empty() {
            return None;
        }
        Some(changed.into_iter().collect())
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct PairRecord {
    pub(crate) version: u8,
    pub(crate) identity: PairIdentity,
    pub(crate) mode: PairMode,
    pub(crate) conflict_preference: ConflictPreference,
    pub(crate) runtime_settings: Map<String, Value>,
    #[serde(default)]
    pub(crate) baseline: Option<StoredSnapshot>,
    #[serde(default)]
    pub(crate) head: Option<RecoveryHead>,
    #[serde(default)]
    pub(crate) conflicts: Vec<String>,
    #[serde(default)]
    pub(crate) resolution_required: bool,
    #[serde(default)]
    pub(crate) last_runtime_id: Option<String>,
    #[serde(default)]
    pub(crate) local_file_stamp: Option<LocalFileStamp>,
    #[serde(default)]
    pub(crate) local_file_digest: Option<String>,
    #[serde(default)]
    pub(crate) studio_checkpoint: Option<StudioCheckpoint>,
}

pub(crate) fn saved_pair_identities(root: &Path, experience: &Path) -> Result<Vec<PairIdentity>> {
    let record_dir = root.join(".renium").join(RECORD_DIR);
    let entries = match fs::read_dir(&record_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to inspect {}", record_dir.display()));
        }
    };
    let project = canonical_string(root)?;
    let experience = canonical_string(experience)?;
    let mut identities = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry
            .path()
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("rmp")
        {
            continue;
        }
        let bytes = fs::read(entry.path())?;
        let Ok(record) = rmp_serde::from_slice::<PairRecord>(&bytes) else {
            continue;
        };
        if record.version != RECORD_VERSION
            || record.identity.project != project
            || record.identity.experience != experience
        {
            continue;
        }
        identities.push(record.identity);
    }
    Ok(identities)
}

pub(crate) fn saved_studio_target_for_root(
    root: &Path,
    experience: &Path,
) -> Result<Option<crate::automation::StudioReopenTarget>> {
    let mut targets = Vec::new();
    for identity in saved_pair_identities(root, experience)? {
        let file = identity
            .local_file
            .map(PathBuf::from)
            .filter(|path| path.is_file());
        let game_id = identity.game_id.filter(|id| *id > 0);
        let place_id = identity.place_id.filter(|id| *id > 0);
        if file.is_none() && place_id.is_none() {
            continue;
        }
        let target = crate::automation::StudioReopenTarget {
            file,
            game_id,
            place_id,
        };
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    Ok((targets.len() == 1).then(|| targets.pop().unwrap()))
}

pub(crate) fn saved_local_file_for_context(context: &BoundContext) -> Result<Option<PathBuf>> {
    let files = saved_pair_identities(Path::new(&context.root), Path::new(&context.experience))?
        .into_iter()
        .filter_map(|identity| identity.local_file.map(PathBuf::from))
        .filter(|file| file.is_file())
        .collect::<HashSet<_>>();
    if files.len() > 1 {
        bail!(
            "More than one local Studio file is paired with this project; pass the file to rbx ro"
        );
    }
    Ok(files.into_iter().next())
}

pub(crate) fn canonical_string(path: &Path) -> Result<String> {
    Ok(canonical_path(path)?.to_string_lossy().into_owned())
}

pub(crate) fn record_path(context: &BoundContext, key: &str) -> PathBuf {
    Path::new(&context.root)
        .join(".renium")
        .join(RECORD_DIR)
        .join(format!("{key}.rmp"))
}

pub(crate) fn load_record(context: &BoundContext, key: &str) -> Result<Option<PairRecord>> {
    let path = record_path(context, key);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read {}", path.display()));
        }
    };
    let mut record: PairRecord = rmp_serde::from_slice(&bytes)
        .with_context(|| format!("Failed to decode {}", path.display()))?;
    if let Some(baseline) = record.baseline.as_mut()
        && baseline.migrate_store_paths(Path::new(&context.root))?
    {
        record.studio_checkpoint = None;
        record.local_file_stamp = None;
        record.local_file_digest = None;
        write_record(context, key, &record)?;
    }
    Ok(Some(record))
}

pub(crate) fn write_record(context: &BoundContext, key: &str, record: &PairRecord) -> Result<()> {
    let path = record_path(context, key);
    let encoded = rmp_serde::to_vec(record).context("Failed to encode reconciliation state")?;
    atomic_write_file(&path, &encoded)?;
    if let Some(baseline) = &record.baseline {
        let _ = baseline.prune(Path::new(&context.root), key);
    } else {
        let _ = StoredSnapshot::clear(Path::new(&context.root), key);
    }
    Ok(())
}
