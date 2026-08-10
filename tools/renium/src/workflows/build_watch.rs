use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::Value;

use super::{BuildArgs, ToolPolicy, build_once, find_command, project_package_manager};
use crate::file_io::absolutize_for_daemon as absolute_path;
use crate::project_config::{self, LoadedProject};

fn roblox_ts_executable(loaded: &LoadedProject) -> Result<PathBuf> {
    let local = loaded.root.join(if cfg!(windows) {
        "node_modules/.bin/rbxtsc.cmd"
    } else {
        "node_modules/.bin/rbxtsc"
    });
    if local.is_file() {
        return Ok(local);
    }
    find_command("rbxtsc")
        .context("roblox-ts was requested but rbxtsc is not installed locally or on PATH")
}

fn roblox_ts_package_script(root: &Path, watch: bool) -> Result<Option<String>> {
    let package = root.join("package.json");
    let value: Value = serde_json::from_slice(&fs::read(&package)?)
        .with_context(|| format!("Invalid {}", package.display()))?;
    let Some(scripts) = value.get("scripts").and_then(Value::as_object) else {
        return Ok(None);
    };
    let preferred = if watch { "watch" } else { "build" };
    if scripts
        .get(preferred)
        .and_then(Value::as_str)
        .is_some_and(|command| {
            command
                .split_whitespace()
                .any(|part| part.contains("rbxtsc"))
        })
    {
        return Ok(Some(preferred.to_string()));
    }
    Ok(scripts.iter().find_map(|(name, command)| {
        let command = command.as_str()?;
        let has_rbxtsc = command
            .split_whitespace()
            .any(|part| part.contains("rbxtsc"));
        let is_watch = command
            .split_whitespace()
            .any(|part| matches!(part, "-w" | "--watch"));
        (has_rbxtsc && is_watch == watch).then(|| name.clone())
    }))
}

pub(super) fn roblox_ts_command(
    loaded: &LoadedProject,
    watch: bool,
) -> Result<(PathBuf, Vec<String>)> {
    if let Some(script) = roblox_ts_package_script(&loaded.root, watch)? {
        let manager = project_package_manager(&loaded.root)?;
        return Ok((manager, vec!["run".to_string(), script]));
    }
    if let Ok(executable) = roblox_ts_executable(loaded) {
        let args = watch.then(|| "--watch".to_string()).into_iter().collect();
        return Ok((executable, args));
    }
    let manager = project_package_manager(&loaded.root)?;
    let name = manager
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut args = match name.as_str() {
        "npm" => vec!["exec".to_string(), "--".to_string(), "rbxtsc".to_string()],
        "pnpm" | "yarn" => vec!["exec".to_string(), "rbxtsc".to_string()],
        "bun" => vec!["x".to_string(), "rbxtsc".to_string()],
        _ => bail!("Unsupported package manager {}", manager.display()),
    };
    if watch {
        args.push("--watch".to_string());
    }
    Ok((manager, args))
}

struct RobloxTsWatch {
    child: Child,
    output_threads: Vec<thread::JoinHandle<()>>,
    output: mpsc::Receiver<String>,
    output_dropped: Arc<AtomicBool>,
    incremental_error: bool,
    cycle_unreliable: bool,
    cycle_active: bool,
    projection_blocked: bool,
}

#[derive(Default)]
struct RobloxTsDrain {
    successful_cycle: bool,
    overflowed: bool,
}

fn spawn_tool_output_reader(
    stream: impl Read + Send + 'static,
    sender: mpsc::SyncSender<String>,
    output_dropped: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            crate::log_global(3, format_args!("{line}"));
            match sender.try_send(line) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(_)) => {
                    output_dropped.store(true, Ordering::Release);
                }
                Err(mpsc::TrySendError::Disconnected(_)) => break,
            }
        }
    })
}

fn roblox_ts_error_count(line: &str) -> Option<usize> {
    let found = line.find("found ")? + "found ".len();
    let digits = line[found..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    if digits.is_empty()
        || !line[found + digits.len()..]
            .trim_start()
            .starts_with("error")
    {
        return None;
    }
    digits.parse().ok()
}

fn roblox_ts_error_line(line: &str) -> bool {
    line.contains("error ts")
        || line.contains("[error]")
        || line.starts_with("error:")
        || line.contains(" compilation failed")
        || line.contains("failed to compile")
}

#[cfg(unix)]
fn configure_watched_process(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

#[cfg(windows)]
fn configure_watched_process(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    command.creation_flags(0x00000200 | 0x08000000);
}

#[cfg(unix)]
fn terminate_watched_process(child: &mut Child) {
    let pid = child.id() as i32;
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let group_alive = unsafe {
            libc::kill(-pid, 0) == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        };
        if !group_alive {
            let _ = child.wait();
            return;
        }
        let _ = child.try_wait();
        thread::sleep(Duration::from_millis(25));
    }
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.wait();
}

#[cfg(windows)]
fn terminate_watched_process(child: &mut Child) {
    let _ = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

impl RobloxTsWatch {
    fn start(loaded: &LoadedProject, args: &BuildArgs) -> Result<Option<Self>> {
        let package = loaded.root.join("package.json");
        let detected = package_uses_roblox_ts(&package)?;
        if !should_run_tool(args.typescript, detected) {
            return Ok(None);
        }
        if !package.is_file() {
            bail!("--ts always requires {}", package.display());
        }
        let (executable, command_args) = roblox_ts_command(loaded, true)?;
        let mut command = Command::new(executable);
        command
            .args(command_args)
            .current_dir(&loaded.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_watched_process(&mut command);
        let mut child = command.spawn().context("Failed to start roblox-ts watch")?;
        let (sender, output) = mpsc::sync_channel(1_024);
        let output_dropped = Arc::new(AtomicBool::new(false));
        let mut output_threads = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            output_threads.push(spawn_tool_output_reader(
                stdout,
                sender.clone(),
                Arc::clone(&output_dropped),
            ));
        }
        if let Some(stderr) = child.stderr.take() {
            output_threads.push(spawn_tool_output_reader(
                stderr,
                sender,
                Arc::clone(&output_dropped),
            ));
        }
        Ok(Some(Self {
            child,
            output_threads,
            output,
            output_dropped,
            incremental_error: false,
            cycle_unreliable: false,
            cycle_active: false,
            projection_blocked: false,
        }))
    }

    fn is_running(&mut self) -> Result<bool> {
        if let Some(status) = self.child.try_wait()? {
            crate::log_global(2, format_args!("roblox-ts watch exited with {status}"));
            return Ok(false);
        }
        Ok(true)
    }

    fn wait_initial(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut saw_error = false;
        loop {
            if !self.is_running()? {
                bail!("roblox-ts watch exited before its initial compilation completed");
            }
            if self.output_dropped.load(Ordering::Acquire) {
                bail!("roblox-ts watch produced too much output to verify its initial compilation");
            }
            match self.output.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => {
                    let line = line.to_ascii_lowercase();
                    if let Some(errors) = roblox_ts_error_count(&line) {
                        if errors == 0 {
                            return Ok(());
                        }
                        bail!("roblox-ts initial compilation reported {errors} error(s)");
                    }
                    saw_error |= roblox_ts_error_line(&line);
                    if (line.contains("compilation complete")
                        || line.contains("compiled successfully"))
                        && !saw_error
                    {
                        return Ok(());
                    }
                    if line.contains("watching for file changes") && saw_error {
                        bail!("roblox-ts initial compilation failed");
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("roblox-ts watch closed its output before becoming ready");
                }
            }
            if Instant::now() >= deadline {
                bail!("roblox-ts watch did not finish its initial compilation within 60 seconds");
            }
        }
    }

    fn drain_incremental(&mut self) -> RobloxTsDrain {
        let mut drain = RobloxTsDrain::default();
        if self.output_dropped.load(Ordering::Acquire) {
            self.incremental_error = true;
            self.cycle_unreliable = true;
            self.projection_blocked = true;
            drain.overflowed = true;
            crate::log_global(
                2,
                format_args!(
                    "roblox-ts produced too much output; the current compile cycle is unreliable"
                ),
            );
        }
        while let Ok(line) = self.output.try_recv() {
            let normalized = line.to_ascii_lowercase();
            if normalized.contains("starting compilation")
                || normalized.contains("starting incremental compilation")
                || normalized.contains("file change detected")
            {
                self.incremental_error = false;
                self.cycle_active = true;
                self.projection_blocked = true;
            }
            let mut surfaced = false;
            if let Some(errors) = roblox_ts_error_count(&normalized) {
                if errors > 0 {
                    self.incremental_error = true;
                    crate::log_global(
                        2,
                        format_args!("roblox-ts compilation reported {errors} error(s)"),
                    );
                    surfaced = true;
                }
                let succeeded = self.cycle_active
                    && errors == 0
                    && !self.incremental_error
                    && !self.cycle_unreliable;
                drain.successful_cycle |= succeeded;
                if self.cycle_active {
                    self.projection_blocked = !succeeded;
                    self.cycle_active = false;
                }
            }
            if roblox_ts_error_line(&normalized) {
                self.incremental_error = true;
                if !surfaced {
                    crate::log_global(2, format_args!("roblox-ts: {line}"));
                }
            }
            if normalized.contains("compilation complete")
                || normalized.contains("compiled successfully")
                || normalized.contains("watching for file changes")
            {
                let succeeded =
                    self.cycle_active && !self.incremental_error && !self.cycle_unreliable;
                if self.cycle_active && !succeeded {
                    crate::log_global(2, format_args!("roblox-ts compilation failed"));
                }
                drain.successful_cycle |= succeeded;
                if self.cycle_active {
                    self.projection_blocked = !succeeded;
                    self.cycle_active = false;
                }
            }
        }
        if self.output_dropped.load(Ordering::Acquire) {
            self.incremental_error = true;
            self.cycle_unreliable = true;
            self.projection_blocked = true;
            drain.overflowed = true;
            drain.successful_cycle = false;
        }
        drain
    }

    fn projection_blocked(&self) -> bool {
        self.projection_blocked
    }
}

fn roblox_ts_watch_desired(loaded: &LoadedProject, args: &BuildArgs) -> Result<bool> {
    Ok(should_run_tool(
        args.typescript,
        package_uses_roblox_ts(&loaded.root.join("package.json"))?,
    ))
}

fn start_ready_roblox_ts_watch(
    loaded: &LoadedProject,
    args: &BuildArgs,
) -> Result<Option<RobloxTsWatch>> {
    let Some(mut process) = RobloxTsWatch::start(loaded, args)? else {
        return Ok(None);
    };
    process.wait_initial()?;
    Ok(Some(process))
}

fn roblox_ts_retry_delay(failures: u32) -> Duration {
    Duration::from_millis(250_u64.saturating_mul(1_u64 << failures.min(4)))
}

struct RobloxTsConfigGraph {
    files: BTreeSet<PathBuf>,
    output_roots: BTreeSet<PathBuf>,
}

impl RobloxTsConfigGraph {
    fn fingerprint(&self) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        self.files
            .iter()
            .map(|path| (path.clone(), fs::read(path).ok()))
            .collect()
    }
}

fn resolve_tsconfig_candidate(path: PathBuf) -> Option<PathBuf> {
    let candidates = if path.extension().is_some() {
        vec![path]
    } else {
        vec![
            path.clone(),
            path.with_extension("json"),
            path.join("tsconfig.json"),
        ]
    };
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .map(|candidate| absolute_path(&candidate))
}

fn resolve_tsconfig_package(
    from: &Path,
    specifier: &str,
    files: &mut BTreeSet<PathBuf>,
) -> Result<Option<PathBuf>> {
    let segments = specifier.split('/').collect::<Vec<_>>();
    let package_parts = if specifier.starts_with('@') { 2 } else { 1 };
    if segments.len() < package_parts {
        return Ok(None);
    }
    let package_name = segments[..package_parts].join("/");
    let subpath = segments[package_parts..].join("/");
    for ancestor in from.ancestors() {
        let package_root = ancestor.join("node_modules").join(&package_name);
        if !package_root.is_dir() {
            continue;
        }
        if !subpath.is_empty() {
            return Ok(resolve_tsconfig_candidate(package_root.join(subpath)));
        }
        let package_json = package_root.join("package.json");
        if package_json.is_file() {
            files.insert(absolute_path(&package_json));
            let value: Value = serde_json::from_slice(&fs::read(&package_json)?)
                .with_context(|| format!("Invalid {}", package_json.display()))?;
            if let Some(config) = value.get("tsconfig").and_then(Value::as_str)
                && let Some(path) = resolve_tsconfig_candidate(package_root.join(config))
            {
                return Ok(Some(path));
            }
        }
        return Ok(resolve_tsconfig_candidate(
            package_root.join("tsconfig.json"),
        ));
    }
    Ok(None)
}

fn resolve_tsconfig_specifier(
    config: &Path,
    specifier: &str,
    files: &mut BTreeSet<PathBuf>,
) -> Result<PathBuf> {
    let parent = config
        .parent()
        .context("tsconfig has no parent directory")?;
    let path = Path::new(specifier);
    let resolved = if path.is_absolute() || specifier.starts_with('.') {
        resolve_tsconfig_candidate(parent.join(path))
    } else {
        resolve_tsconfig_package(parent, specifier, files)?
    };
    resolved.with_context(|| {
        format!(
            "Could not resolve tsconfig dependency '{specifier}' from {}",
            config.display()
        )
    })
}

fn roblox_ts_config_graph(root: &Path) -> Result<RobloxTsConfigGraph> {
    fn visit(
        path: PathBuf,
        graph: &mut RobloxTsConfigGraph,
        visiting: &mut BTreeSet<PathBuf>,
        outputs: &mut BTreeMap<PathBuf, Option<PathBuf>>,
    ) -> Result<Option<PathBuf>> {
        let path = absolute_path(&path);
        if let Some(output) = outputs.get(&path) {
            return Ok(output.clone());
        }
        if !visiting.insert(path.clone()) {
            bail!("tsconfig dependency cycle includes {}", path.display());
        }
        graph.files.insert(path.clone());
        let text = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let value = project_config::parse_jsonc_value(&text)
            .with_context(|| format!("Invalid {}", path.display()))?;
        let mut output = None;
        let extends = match value.get("extends") {
            Some(Value::String(value)) => vec![value.as_str()],
            Some(Value::Array(values)) => values.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        for specifier in extends {
            let dependency = resolve_tsconfig_specifier(&path, specifier, &mut graph.files)?;
            if let Some(inherited) = visit(dependency, graph, visiting, outputs)? {
                output = Some(inherited);
            }
        }
        if let Some(out_dir) = value
            .get("compilerOptions")
            .and_then(Value::as_object)
            .and_then(|options| options.get("outDir"))
            .and_then(Value::as_str)
        {
            output = Some(absolute_path(
                &path
                    .parent()
                    .context("tsconfig has no parent directory")?
                    .join(out_dir),
            ));
        }
        if let Some(references) = value.get("references").and_then(Value::as_array) {
            for reference in references {
                let Some(specifier) = reference.get("path").and_then(Value::as_str) else {
                    continue;
                };
                let dependency = resolve_tsconfig_candidate(
                    path.parent()
                        .context("tsconfig has no parent directory")?
                        .join(specifier),
                )
                .with_context(|| {
                    format!(
                        "Could not resolve tsconfig project reference '{specifier}' from {}",
                        path.display()
                    )
                })?;
                if let Some(reference_output) = visit(dependency, graph, visiting, outputs)? {
                    graph.output_roots.insert(reference_output);
                }
            }
        }
        visiting.remove(&path);
        if let Some(output) = output.as_ref() {
            graph.output_roots.insert(output.clone());
        }
        outputs.insert(path, output.clone());
        Ok(output)
    }

    let mut graph = RobloxTsConfigGraph {
        files: BTreeSet::from([absolute_path(&root.join("package.json"))]),
        output_roots: BTreeSet::new(),
    };
    let root_config = root.join("tsconfig.json");
    if root_config.is_file() {
        visit(
            root_config,
            &mut graph,
            &mut BTreeSet::new(),
            &mut BTreeMap::new(),
        )?;
    }
    Ok(graph)
}

fn roblox_ts_event_is_related(paths: &[PathBuf], output_roots: &[PathBuf]) -> bool {
    paths.iter().any(|path| {
        let path = absolute_path(path);
        path.extension()
            .and_then(OsStr::to_str)
            .is_some_and(|extension| {
                matches!(extension.to_ascii_lowercase().as_str(), "ts" | "tsx")
            })
            || output_roots
                .iter()
                .any(|root| path == *root || path.starts_with(root))
    })
}

fn build_after_roblox_ts(loaded: &LoadedProject, args: &BuildArgs, output: &Path) -> bool {
    let mut projection_args = args.clone();
    projection_args.typescript = ToolPolicy::Never;
    match build_once(loaded, &projection_args, output, true, None) {
        Ok(()) => true,
        Err(error) => {
            crate::log_global(
                2,
                format_args!("Build after roblox-ts compilation failed: {error:#}"),
            );
            false
        }
    }
}

impl Drop for RobloxTsWatch {
    fn drop(&mut self) {
        terminate_watched_process(&mut self.child);
        for handle in self.output_threads.drain(..) {
            let _ = handle.join();
        }
    }
}

pub(super) fn package_uses_roblox_ts(path: &Path) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let value: Value = serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("Invalid {}", path.display()))?;
    let dependency = ["dependencies", "devDependencies"]
        .into_iter()
        .any(|section| {
            value
                .get(section)
                .and_then(Value::as_object)
                .is_some_and(|dependencies| dependencies.contains_key("roblox-ts"))
        });
    let script = value
        .get("scripts")
        .and_then(Value::as_object)
        .is_some_and(|scripts| {
            scripts.values().filter_map(Value::as_str).any(|command| {
                command
                    .split_whitespace()
                    .any(|part| part.contains("rbxtsc"))
            })
        });
    Ok(dependency || script)
}

pub(super) fn should_run_tool(policy: ToolPolicy, detected: bool) -> bool {
    match policy {
        ToolPolicy::Auto => detected,
        ToolPolicy::Always => true,
        ToolPolicy::Never => false,
    }
}

#[derive(Default)]
struct ProjectWatchInputs {
    files: BTreeSet<PathBuf>,
    directories: BTreeSet<PathBuf>,
    ignored: BTreeSet<PathBuf>,
}

fn project_watch_inputs(loaded: &LoadedProject) -> Result<ProjectWatchInputs> {
    let mut inputs = ProjectWatchInputs::default();
    let mut visited = BTreeSet::new();
    project_watch_inputs_into(loaded, &mut inputs, &mut visited)?;
    Ok(inputs)
}

fn project_watch_inputs_into(
    loaded: &LoadedProject,
    inputs: &mut ProjectWatchInputs,
    visited: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let key = absolute_path(&loaded.path);
    if !visited.insert(key.clone()) {
        return Ok(());
    }
    inputs.files.insert(key);
    inputs.directories.insert(absolute_path(
        &loaded.root.join(&loaded.project.source_root),
    ));
    let mut nested = Vec::new();
    for (_, node) in project_config::project_tree_nodes(&loaded.project.tree) {
        if let Some(path) = node.path {
            let path = absolute_path(&loaded.root.join(path));
            if path.is_dir() || (!path.exists() && path.extension().is_none()) {
                inputs.directories.insert(path);
            } else {
                inputs.files.insert(path.clone());
                nested.push(path);
            }
        }
    }
    for mount in &loaded.project.mounts {
        let path = absolute_path(&loaded.root.join(&mount.source));
        if path.is_dir() || (!path.exists() && path.extension().is_none()) {
            inputs.directories.insert(path);
        } else {
            inputs.files.insert(path.clone());
            nested.push(path);
        }
    }
    for adapter in &loaded.project.adapters {
        let source = absolute_path(&loaded.root.join(&adapter.source));
        inputs.files.insert(source.clone());
        nested.push(source);
        if let Some(output) = adapter.output.as_deref() {
            inputs
                .ignored
                .insert(absolute_path(&loaded.root.join(output)));
        }
    }
    inputs.files.extend(
        ["wally.toml", "wally.lock"]
            .into_iter()
            .map(|path| absolute_path(&loaded.root.join(path))),
    );
    inputs
        .files
        .extend(roblox_ts_config_graph(&loaded.root)?.files);
    inputs
        .ignored
        .insert(absolute_path(&loaded.root.join("sourcemap.json")));
    for path in nested {
        let name = path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if (name.ends_with(".project.json") || name.ends_with(".project.jsonc")) && path.is_file() {
            let nested = project_config::load_project(Some(&path), None)?;
            project_watch_inputs_into(&nested, inputs, visited)?;
        }
    }
    Ok(())
}

fn configure_project_watcher(
    watcher: &mut RecommendedWatcher,
    previous: &mut BTreeMap<PathBuf, bool>,
    inputs: &ProjectWatchInputs,
) -> Result<()> {
    let mut roots = BTreeMap::new();
    for (root, recursive) in inputs
        .files
        .iter()
        .map(|path| (path, false))
        .chain(inputs.directories.iter().map(|path| (path, true)))
    {
        let mut watch_root = root.clone();
        while !watch_root.exists() {
            let Some(parent) = watch_root.parent() else {
                break;
            };
            watch_root = parent.to_path_buf();
        }
        if !watch_root.exists() {
            continue;
        }
        let recursive = recursive || watch_root != *root;
        roots
            .entry(watch_root)
            .and_modify(|existing| *existing |= recursive)
            .or_insert(recursive);
    }
    for (root, recursive) in previous.iter() {
        if roots.get(root) != Some(recursive) {
            let _ = watcher.unwatch(root);
        }
    }
    for (root, recursive) in &roots {
        if previous.get(root) == Some(recursive) {
            continue;
        }
        watcher
            .watch(
                root,
                if *recursive {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                },
            )
            .with_context(|| format!("Failed to watch {}", root.display()))?;
    }
    *previous = roots;
    Ok(())
}

fn build_watch_event_is_relevant(
    paths: &[PathBuf],
    output: &Path,
    inputs: &ProjectWatchInputs,
) -> bool {
    let output = absolute_path(output);
    paths.iter().any(|path| {
        let path = absolute_path(path);
        let temporary_output = path.parent() == output.parent()
            && path
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| {
                    let stem = output
                        .file_stem()
                        .and_then(OsStr::to_str)
                        .unwrap_or("renium-build");
                    name.starts_with(&format!(".{stem}."))
                        && name.contains(".tmp.")
                        && path.extension() == output.extension()
                });
        let transaction_backup = path.parent() == output.parent()
            && path
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| {
                    let output_name = output
                        .file_name()
                        .and_then(OsStr::to_str)
                        .unwrap_or("renium-build");
                    name.starts_with(&format!(".{output_name}.")) && name.ends_with(".previous")
                });
        if path == output
            || temporary_output
            || transaction_backup
            || inputs
                .ignored
                .iter()
                .any(|ignored| path == *ignored || path.starts_with(ignored))
        {
            return false;
        }
        let excluded = path.components().any(|component| {
            component
                .as_os_str()
                .to_str()
                .is_some_and(|name| matches!(name, ".git" | ".renium" | "node_modules"))
        });
        !excluded
            && (inputs.files.contains(&path)
                || inputs
                    .directories
                    .iter()
                    .any(|directory| path == *directory || path.starts_with(directory)))
    })
}

fn build_args_for_watch_event(
    args: &BuildArgs,
    loaded: &LoadedProject,
    paths: &[PathBuf],
    typescript_config_files: &BTreeSet<PathBuf>,
) -> BuildArgs {
    let paths = paths
        .iter()
        .map(|path| absolute_path(path))
        .collect::<Vec<_>>();
    let project_changed = paths.contains(&absolute_path(&loaded.path));
    let wally_changed = project_changed
        || ["wally.toml", "wally.lock"]
            .into_iter()
            .map(|path| absolute_path(&loaded.root.join(path)))
            .any(|path| paths.contains(&path));
    let typescript_changed = project_changed
        || paths
            .iter()
            .any(|path| typescript_config_files.contains(path))
        || paths.iter().any(|path| {
            path.extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| {
                    matches!(extension.to_ascii_lowercase().as_str(), "ts" | "tsx")
                })
        });
    let mut selected = args.clone();
    if selected.wally == ToolPolicy::Auto && !wally_changed {
        selected.wally = ToolPolicy::Never;
    }
    if selected.typescript == ToolPolicy::Auto && !typescript_changed {
        selected.typescript = ToolPolicy::Never;
    }
    selected
}

struct RobloxTsWatchRuntime {
    desired: bool,
    process: Option<RobloxTsWatch>,
    restart_failures: u32,
    retry_at: Instant,
    projection_pending: bool,
}

impl RobloxTsWatchRuntime {
    fn start(loaded: &LoadedProject, args: &BuildArgs) -> Result<Self> {
        Ok(Self {
            desired: roblox_ts_watch_desired(loaded, args)?,
            process: start_ready_roblox_ts_watch(loaded, args)?,
            restart_failures: 0,
            retry_at: Instant::now(),
            projection_pending: false,
        })
    }

    fn drain(&mut self) -> RobloxTsDrain {
        self.process
            .as_mut()
            .map(RobloxTsWatch::drain_incremental)
            .unwrap_or_default()
    }

    fn schedule_restart(&mut self) {
        drop(self.process.take());
        self.restart_failures = self.restart_failures.saturating_add(1);
        self.retry_at = Instant::now() + roblox_ts_retry_delay(self.restart_failures);
        self.projection_pending = true;
    }

    fn complete_cycle(&mut self, loaded: &LoadedProject, args: &BuildArgs, output: &Path) {
        self.restart_failures = 0;
        self.projection_pending = !build_after_roblox_ts(loaded, args, output);
    }

    fn poll(&mut self, loaded: &LoadedProject, args: &BuildArgs, output: &Path) -> Result<()> {
        let drain = self.drain();
        if drain.overflowed {
            self.schedule_restart();
        } else {
            if drain.successful_cycle {
                self.complete_cycle(loaded, args, output);
            }
            if self
                .process
                .as_mut()
                .map(RobloxTsWatch::is_running)
                .transpose()?
                == Some(false)
            {
                self.schedule_restart();
            }
        }
        if !self.desired || self.process.is_some() || Instant::now() < self.retry_at {
            return Ok(());
        }
        match start_ready_roblox_ts_watch(loaded, args) {
            Ok(Some(process)) => {
                self.process = Some(process);
                if self.projection_pending {
                    self.projection_pending = !build_after_roblox_ts(loaded, args, output);
                }
            }
            Ok(None) => {
                self.desired = false;
                self.restart_failures = 0;
            }
            Err(error) => {
                self.restart_failures = self.restart_failures.saturating_add(1);
                self.retry_at = Instant::now() + roblox_ts_retry_delay(self.restart_failures);
                crate::log_global(2, format_args!("roblox-ts watch restart failed: {error:#}"));
            }
        }
        Ok(())
    }
}

pub(super) fn watch_build(
    loaded: &mut LoadedProject,
    args: &BuildArgs,
    output: &Path,
) -> Result<()> {
    let mut inputs = project_watch_inputs(loaded)?;
    let mut typescript = RobloxTsWatchRuntime::start(loaded, args)?;
    let initial_typescript_graph = roblox_ts_config_graph(&loaded.root)?;
    let mut typescript_output_roots = initial_typescript_graph
        .output_roots
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    let mut typescript_config_files = initial_typescript_graph.files;
    let mut typescript_config_fingerprint = RobloxTsConfigGraph {
        files: typescript_config_files.clone(),
        output_roots: typescript_output_roots.iter().cloned().collect(),
    }
    .fingerprint();
    let mut initial_args = args.clone();
    if typescript.process.is_some() {
        initial_args.typescript = ToolPolicy::Never;
    }
    build_once(loaded, &initial_args, output, true, None)?;
    let (sender, receiver) = mpsc::sync_channel(4_096);
    let watch_overflowed = Arc::new(AtomicBool::new(false));
    let callback_overflowed = Arc::clone(&watch_overflowed);
    let mut watcher = notify::recommended_watcher(move |event| match sender.try_send(event) {
        Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => {}
        Err(mpsc::TrySendError::Full(_)) => {
            callback_overflowed.store(true, Ordering::Release);
        }
    })?;
    let mut watched = BTreeMap::new();
    configure_project_watcher(&mut watcher, &mut watched, &inputs)?;
    loop {
        let mut rescan_required = false;
        let mut typescript_cycle_completed = false;
        let mut typescript_output_overflowed = false;
        let event = loop {
            match receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(Ok(event)) => break Some(event),
                Ok(Err(error)) => {
                    crate::log_global(2, format_args!("Build watcher failed: {error}"));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    typescript.poll(loaded, args, output)?;
                    if watch_overflowed.swap(false, Ordering::AcqRel) {
                        rescan_required = true;
                        break None;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("Project watcher stopped");
                }
            }
        };
        let drain = typescript.drain();
        typescript_cycle_completed |= drain.successful_cycle;
        typescript_output_overflowed |= drain.overflowed;
        let mut paths = event.map(|event| event.paths).unwrap_or_default();
        let batch_deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < batch_deadline
            && let Ok(event) = receiver.recv_timeout(
                Duration::from_millis(100)
                    .min(batch_deadline.saturating_duration_since(Instant::now())),
            )
        {
            match event {
                Ok(event) => paths.extend(event.paths),
                Err(error) => crate::log_global(2, format_args!("Build watcher failed: {error}")),
            }
        }
        rescan_required |= watch_overflowed.swap(false, Ordering::AcqRel);
        let drain = typescript.drain();
        typescript_cycle_completed |= drain.successful_cycle;
        typescript_output_overflowed |= drain.overflowed;
        if typescript_output_overflowed {
            typescript.schedule_restart();
            continue;
        }
        paths.sort();
        paths.dedup();
        configure_project_watcher(&mut watcher, &mut watched, &inputs)?;
        if !rescan_required && !build_watch_event_is_relevant(&paths, output, &inputs) {
            continue;
        }
        let typescript_config_path_changed = paths
            .iter()
            .any(|path| typescript_config_files.contains(path));
        let current_typescript_graph = roblox_ts_config_graph(&loaded.root);
        let current_typescript_config_fingerprint = current_typescript_graph
            .as_ref()
            .ok()
            .map(RobloxTsConfigGraph::fingerprint);
        let typescript_configuration_changed = typescript_config_path_changed
            || rescan_required
                && current_typescript_config_fingerprint
                    .as_ref()
                    .is_none_or(|fingerprint| *fingerprint != typescript_config_fingerprint);
        let project_graph_changed = rescan_required
            || paths.iter().any(|path| {
                let name = path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                name.ends_with(".project.json") || name.ends_with(".project.jsonc")
            });
        if project_graph_changed {
            match project_config::load_project(Some(&loaded.path), None) {
                Ok(current) => {
                    *loaded = current;
                    inputs = project_watch_inputs(loaded)?;
                    configure_project_watcher(&mut watcher, &mut watched, &inputs)?;
                }
                Err(error) => {
                    crate::log_global(2, format_args!("Build configuration failed: {error:#}"));
                    continue;
                }
            }
        }
        if typescript_configuration_changed {
            drop(typescript.process.take());
            let current_typescript_graph = match current_typescript_graph {
                Ok(graph) => graph,
                Err(error) => {
                    crate::log_global(
                        2,
                        format_args!("roblox-ts configuration is not ready: {error:#}"),
                    );
                    continue;
                }
            };
            let desired = match roblox_ts_watch_desired(loaded, args) {
                Ok(desired) => desired,
                Err(error) => {
                    crate::log_global(
                        2,
                        format_args!("roblox-ts configuration is not ready: {error:#}"),
                    );
                    continue;
                }
            };
            typescript_config_fingerprint = current_typescript_graph.fingerprint();
            typescript_config_files = current_typescript_graph.files;
            typescript_output_roots = current_typescript_graph.output_roots.into_iter().collect();
            typescript.projection_pending = desired;
            if desired {
                match start_ready_roblox_ts_watch(loaded, args) {
                    Ok(Some(process)) => {
                        typescript.process = Some(process);
                        typescript.desired = true;
                        typescript.restart_failures = 0;
                        typescript.retry_at = Instant::now();
                    }
                    Ok(None) => {
                        typescript.desired = false;
                        typescript.projection_pending = false;
                    }
                    Err(error) => {
                        typescript.desired = true;
                        typescript.restart_failures =
                            typescript.restart_failures.saturating_add(1).max(1);
                        typescript.retry_at =
                            Instant::now() + roblox_ts_retry_delay(typescript.restart_failures);
                        crate::log_global(
                            2,
                            format_args!("roblox-ts watch restart failed: {error:#}"),
                        );
                        continue;
                    }
                }
            } else {
                typescript.desired = false;
            }
            inputs = project_watch_inputs(loaded)?;
            configure_project_watcher(&mut watcher, &mut watched, &inputs)?;
            if typescript.process.is_some() {
                typescript.projection_pending = !build_after_roblox_ts(loaded, args, output);
            } else {
                match build_once(loaded, args, output, true, None) {
                    Ok(()) => typescript.projection_pending = false,
                    Err(error) => crate::log_global(
                        2,
                        format_args!(
                            "Build after roblox-ts configuration change failed: {error:#}"
                        ),
                    ),
                }
            }
            continue;
        }
        if typescript_cycle_completed {
            typescript.complete_cycle(loaded, args, output);
            continue;
        }
        if typescript.process.is_some()
            && roblox_ts_event_is_related(&paths, &typescript_output_roots)
        {
            continue;
        }
        if typescript
            .process
            .as_ref()
            .is_some_and(RobloxTsWatch::projection_blocked)
        {
            typescript.projection_pending = true;
            continue;
        }
        let force_full_build = rescan_required || typescript.projection_pending;
        let mut selected =
            build_args_for_watch_event(args, loaded, &paths, &typescript_config_files);
        if typescript.process.is_some() {
            selected.typescript = ToolPolicy::Never;
        }
        let changed_paths = (!force_full_build).then_some(paths.as_slice());
        if let Err(error) = build_once(loaded, &selected, output, true, changed_paths) {
            crate::log_global(2, format_args!("Build failed: {error:#}"));
        } else {
            typescript.projection_pending = false;
        }
    }
}
