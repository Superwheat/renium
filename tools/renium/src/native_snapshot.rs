use std::collections::HashMap;
use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

#[derive(Debug)]
pub(crate) struct NativeSnapshot {
    pub instance_count: usize,
    pub output_size: u64,
    pub setup_ms: f64,
    pub trace_ms: f64,
    pub discover_ms: f64,
    pub helper_ms: f64,
    pub invoke_ms: f64,
    pub validate_ms: f64,
    pub context_ms: f64,
    pub collect_ms: f64,
    pub serialize_ms: f64,
    pub write_ms: f64,
    pub elapsed_ms: f64,
}

pub(crate) struct NativeSnapshotRoots<'a> {
    pub exact_service: Option<&'a str>,
    pub containing_service: Option<&'a str>,
}

pub(crate) fn temporary_output_path(output: &Path, pid: u32) -> Result<PathBuf> {
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("Could not create {}", parent.display()))?;
    let name = output
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("place.rbxl");
    Ok(parent.join(format!(".{name}.renium-native-{pid}-{}.rbxl", now_millis())))
}

pub(crate) fn validate_native_snapshot(
    path: &Path,
    expected_roots: NativeSnapshotRoots<'_>,
) -> Result<(usize, f64)> {
    let started = Instant::now();
    let file = File::open(path).with_context(|| format!("Could not read {}", path.display()))?;
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let filter = database
        .classes
        .keys()
        .map(|class| (class.to_string(), std::collections::HashSet::new()))
        .collect::<HashMap<_, _>>();
    let flat = rbx_binary::Deserializer::new()
        .flat_property_filter(std::sync::Arc::new(filter))
        .deserialize_flat(BufReader::new(file))
        .context("Studio native serializer returned an invalid RBXL")?;
    if expected_roots.exact_service.is_none()
        && flat
            .metadata
            .get("ExplicitAutoJoints")
            .is_some_and(|value| value == "true")
    {
        bail!("Studio native serializer returned an instance model instead of a place");
    }
    match expected_roots.exact_service {
        Some(service) => {
            if flat.root_indices.len() != 1
                || flat
                    .instances
                    .get(flat.root_indices[0])
                    .is_none_or(|instance| instance.class.as_str() != service)
            {
                bail!("Studio native RBXL did not contain exactly one {service} root");
            }
        }
        None => {
            let root_classes = flat
                .root_indices
                .iter()
                .filter_map(|index| flat.instances.get(*index))
                .map(|instance| instance.class.as_str())
                .collect::<Vec<_>>();
            if !["Workspace", "Players", "MaterialService"]
                .iter()
                .all(|required| root_classes.iter().any(|class| class == required))
            {
                bail!("Studio native RBXL omitted required service roots");
            }
            if let Some(service) = expected_roots.containing_service
                && !root_classes.iter().any(|class| class == &service)
            {
                bail!("Studio native RBXL omitted the {service} service root");
            }
        }
    }
    Ok((
        flat.instances.len(),
        started.elapsed().as_secs_f64() * 1000.0,
    ))
}

pub(crate) fn finalize_native_snapshot(
    temporary: &Path,
    output: &Path,
    reported_size: u64,
    expected_roots: NativeSnapshotRoots<'_>,
) -> Result<(usize, f64)> {
    let written_size = fs::metadata(temporary)
        .with_context(|| format!("Studio did not create {}", temporary.display()))?
        .len();
    if reported_size == 0 || written_size != reported_size {
        bail!("Studio native serializer reported {reported_size} bytes but wrote {written_size}");
    }
    let validated = validate_native_snapshot(temporary, expected_roots)?;
    fs::rename(temporary, output).with_context(|| {
        format!(
            "Could not move validated native snapshot from {} to {}",
            temporary.display(),
            output.display()
        )
    })?;
    Ok(validated)
}

pub(crate) fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
}
