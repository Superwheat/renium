//! Offline MicroProfiler analysis. Only a pinned Roblox parser runs in the
//! isolated Luau VM; dump bytes are data, with no filesystem or network access.
use super::*;
use mlua::{Lua, LuaSerdeExt};

const LIBMP_SHA256: &str = "ab9579e592e8751386a01537152f2b739cc7942ce565d3c11337cddaa250d231";
// An asset ID does not follow the mutable `latest` tag when Roblox updates it.
const LIBMP_URL: &str = "https://api.github.com/repos/Roblox/libmp/releases/assets/456813947";
const MAX_CAPTURE_BYTES: u64 = 128 * 1024 * 1024;

fn metadata_path(capture: &Path) -> PathBuf {
    let mut name = capture.as_os_str().to_owned();
    name.push(".json");
    PathBuf::from(name)
}

fn read_metadata(capture: &Path, bytes: &[u8]) -> Result<Option<Value>> {
    let file = match fs::File::open(metadata_path(capture)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Could not read capture metadata"),
    };
    let mut metadata = Vec::new();
    file.take(65537).read_to_end(&mut metadata)?;
    if metadata.len() > 65536 {
        bail!("Capture metadata exceeds 64 KiB");
    }
    let metadata: Value = serde_json::from_slice(&metadata).context("Invalid capture metadata")?;
    if metadata["sha256"].as_str() != Some(&crate::system::files::sha256_hex(bytes)) {
        bail!(
            "Capture metadata belongs to different bytes; remove the mismatched .gprx.json sidecar to analyze the raw dump"
        );
    }
    Ok(Some(metadata))
}

pub(super) fn save_capture(path: &Path, result: &Value) -> Result<Value> {
    let encoded = result["data"]
        .as_str()
        .context("MicroProfiler response omitted capture bytes")?;
    if encoded.len() as u64 > MAX_CAPTURE_BYTES.div_ceil(3) * 4 {
        bail!("MicroProfiler capture exceeded 128 MiB; no file was written");
    }
    let bytes = base64::decode(encoded).context("Invalid MicroProfiler capture encoding")?;
    if result["bytes"].as_u64() != Some(bytes.len() as u64)
        || bytes.is_empty()
        || bytes.len() as u64 > MAX_CAPTURE_BYTES
    {
        bail!("MicroProfiler capture was incomplete or oversized; no file was written");
    }
    let path = std::path::absolute(path)?;
    let sidecar = metadata_path(&path);
    let mut metadata = result["metadata"]
        .as_object()
        .context("Capture metadata is missing; install the matching Studio plugin")?
        .clone();
    metadata.insert(
        "sha256".into(),
        json!(crate::system::files::sha256_hex(&bytes)),
    );
    metadata.insert("pid".into(), result["pid"].clone());
    metadata.insert("scope".into(), json!("studio-process"));
    crate::system::files::atomic_write_file(&path, &bytes)?;
    crate::system::files::atomic_write_file(&sidecar, &serde_json::to_vec(&metadata)?)
        .with_context(|| {
            format!(
                "Capture saved to {}, but its metadata could not be saved",
                path.display()
            )
        })?;
    Ok(
        json!({"path":path,"metadata":sidecar,"bytes":bytes.len(),"runtimeId":result["runtimeId"],"format":"gprx"}),
    )
}

fn future_path(path: &Path) -> Result<PathBuf> {
    use std::path::Component;
    let mut resolved = PathBuf::new();
    for component in std::path::absolute(path)?.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            component => {
                resolved.push(component);
                match fs::canonicalize(&resolved) {
                    Ok(path) => resolved = path,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).context("Cannot resolve the analysis output path");
                    }
                }
            }
        }
    }
    Ok(resolved)
}

fn validate_output(capture: &Path, output: &Path) -> Result<()> {
    let output_key = crate::system::files::path_key(&future_path(output)?);
    for protected in [capture.to_path_buf(), metadata_path(capture)] {
        if crate::system::files::path_key(&future_path(&protected)?) == output_key {
            bail!("The analysis output must not overwrite the capture or its metadata");
        }
        match same_file::is_same_file(&protected, output) {
            Ok(true) => bail!("The analysis output must not overwrite the capture or its metadata"),
            Ok(false) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Cannot verify the analysis output path"),
        }
    }
    Ok(())
}

fn parser_source() -> Result<Vec<u8>> {
    let path = crate::app::update::user_data_dir()?
        .join("cache/microprofiler")
        .join(format!("{LIBMP_SHA256}.luau"));
    match fs::File::open(&path) {
        Ok(file) => {
            let mut source = Vec::new();
            file.take(6 * 1024 * 1024 + 1).read_to_end(&mut source)?;
            if crate::system::files::sha256_hex(&source) != LIBMP_SHA256 {
                bail!(
                    "MicroProfiler parser cache failed integrity verification: {}",
                    path.display()
                );
            }
            return Ok(source);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("Could not open the MicroProfiler parser cache"),
    }
    let response = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .build()
        .get(LIBMP_URL)
        .set("Accept", "application/octet-stream")
        .set("User-Agent", "Renium/MicroProfiler")
        .call()
        .context("Could not download Roblox's MicroProfiler parser")?;
    let mut source = Vec::new();
    response
        .into_reader()
        .take(6 * 1024 * 1024)
        .read_to_end(&mut source)?;
    if crate::system::files::sha256_hex(&source) != LIBMP_SHA256 {
        bail!("Roblox's MicroProfiler parser changed; update Renium before using the new parser");
    }
    fs::create_dir_all(path.parent().context("Parser cache has no parent")?)?;
    crate::system::files::atomic_write_file(&path, &source)?;
    Ok(source)
}

pub(super) fn analyze(
    path: &Path,
    frame: Option<u32>,
    top: usize,
    filters: [Option<&str>; 3],
    output: Option<&Path>,
) -> Result<Value> {
    if !(1..=50).contains(&top) || frame == Some(0) {
        bail!("--top must be 1–50; --frame must be a positive capture frame ID");
    }
    if filters
        .iter()
        .flatten()
        .any(|value| value.is_empty() || value.len() > 256)
    {
        bail!("Profiler filters must contain 1–256 bytes");
    }
    let file = fs::File::open(path).with_context(|| format!("Cannot open {}", path.display()))?;
    if let Some(output) = output {
        validate_output(path, output)?;
    }
    let mut bytes = Vec::new();
    file.take(MAX_CAPTURE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_CAPTURE_BYTES {
        bail!("MicroProfiler captures must be nonempty and at most 128 MiB");
    }
    let metadata = read_metadata(path, &bytes)?;
    let source = parser_source()?;
    let started = Instant::now();
    let lua = Lua::new();
    lua.set_memory_limit(768 * 1024 * 1024)?;
    lua.set_compiler(mlua::Compiler::new().set_optimization_level(2));
    lua.set_interrupt(move |_| {
        if started.elapsed() >= Duration::from_secs(20) {
            return Err(mlua::Error::RuntimeError(
                "MicroProfiler analysis exceeded 20 seconds; select one frame with --frame".into(),
            ));
        }
        Ok(mlua::VmState::Continue)
    });
    // LibMP also supports Lute file I/O; this host intentionally exposes none.
    lua.globals().set(
        "require",
        lua.create_function(|lua, name: String| {
            if matches!(name.as_str(), "@std/fs" | "@lute/process") {
                lua.create_table()
            } else {
                Err(mlua::Error::RuntimeError(
                    "Module access is disabled in the profiler parser".into(),
                ))
            }
        })?,
    )?;
    lua.sandbox(true)?;
    let library: mlua::Table = lua.load(source).set_name("Roblox/LibMP").eval()?;
    let analyze: mlua::Function = lua
        .load(include_str!("microprofiler_analysis.luau"))
        .set_name("Renium/MicroProfilerAnalysis")
        .eval()?;
    let result: mlua::Value = analyze.call((
        library,
        lua.create_buffer(&bytes)?,
        frame,
        top,
        filters[0],
        filters[1],
        filters[2],
    ))?;
    let mut result: Value = lua.from_value(result)?;
    // Empty Luau tables otherwise become JSON objects instead of empty arrays.
    for key in ["frames", "counters", "segments"] {
        if result[key].as_object().is_some_and(Map::is_empty) {
            result[key] = json!([]);
        }
    }
    if let Some(frames) = result["frames"].as_array_mut() {
        for frame in frames {
            if frame["scopes"].as_object().is_some_and(Map::is_empty) {
                frame["scopes"] = json!([]);
            }
        }
    }
    result["capture"] = json!(std::path::absolute(path)?);
    result["analysisMs"] = json!(started.elapsed().as_secs_f64() * 1000.0);
    if let Some(metadata) = metadata {
        result["metadata"] = metadata;
    }
    if let Some(output) = output {
        validate_output(path, output)?;
        crate::system::files::atomic_write_file(output, &serde_json::to_vec(&result)?)?;
        return Ok(
            json!({"path":std::path::absolute(output)?,"capture":result["capture"],"frameCount":result["frameCount"],"analysisMs":result["analysisMs"]}),
        );
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_preserves_existing_captures_on_incomplete_data_and_binds_metadata() -> Result<()> {
        let dir = std::env::temp_dir().join(format!(
            "renium-profiler-save-{}",
            crate::automation::authorization::random_id()?
        ));
        fs::create_dir_all(&dir)?;
        let path = dir.join("capture.gprx");
        let checks = (|| -> Result<()> {
            let value = json!({"data":base64::encode(b"fixture"),"bytes":7,"runtimeId":"test-runtime","pid":12,"metadata":{"studioVersion":"fixture","capturedAt":"test-time","runtimeId":"test-runtime"}});
            save_capture(&path, &value)?;
            assert_eq!(fs::read(&path)?, b"fixture");
            assert_eq!(read_metadata(&path, b"fixture")?.unwrap()["pid"], 12);
            assert!(read_metadata(&path, b"other capture").is_err());
            for (key, invalid) in [
                ("data", json!("!")),
                ("bytes", json!(9)),
                ("metadata", Value::Null),
            ] {
                let mut request = value.clone();
                request[key] = invalid;
                assert!(save_capture(&path, &request).is_err());
                assert_eq!(
                    fs::read(&path)?,
                    b"fixture",
                    "failed export must not replace existing capture"
                );
            }
            fs::remove_file(metadata_path(&path))?;
            assert!(read_metadata(&path, b"fixture")?.is_none());
            Ok(())
        })();
        fs::remove_dir_all(dir)?;
        checks
    }

    #[test]
    fn analysis_cannot_replace_its_capture_through_path_or_hard_link_aliases() -> Result<()> {
        let dir = std::env::temp_dir().join(format!(
            "renium-profiler-output-{}",
            crate::automation::authorization::random_id()?
        ));
        fs::create_dir_all(&dir)?;
        let capture = dir.join("capture.gprx");
        let alias = dir.join("alias.gprx");
        fs::write(&capture, b"capture")?;
        fs::hard_link(&capture, &alias)?;
        let checks = (|| -> Result<()> {
            assert!(validate_output(&capture, &capture).is_err());
            assert!(validate_output(&capture, &dir.join("./capture.gprx")).is_err());
            assert!(validate_output(&capture, &alias).is_err());
            let metadata = metadata_path(&capture);
            assert!(validate_output(&capture, &metadata).is_err());
            assert!(
                validate_output(&capture, &dir.join("not-created/../capture.gprx.json")).is_err()
            );
            #[cfg(unix)]
            {
                let link = dir.join("alias-dir");
                std::os::unix::fs::symlink(&dir, &link)?;
                assert!(
                    validate_output(&capture, &link.join("not-created/../capture.gprx.json"))
                        .is_err()
                );
            }
            fs::write(&metadata, b"metadata")?;
            let metadata_alias = dir.join("metadata-alias.json");
            fs::hard_link(&metadata, &metadata_alias)?;
            assert!(validate_output(&capture, &metadata).is_err());
            assert!(validate_output(&capture, &metadata_alias).is_err());
            assert!(validate_output(&capture, &dir.join("new.json")).is_ok());
            fs::write(dir.join("existing.json"), b"old analysis")?;
            assert!(validate_output(&capture, &dir.join("existing.json")).is_ok());
            Ok(())
        })();
        fs::remove_dir_all(dir)?;
        checks
    }
}
