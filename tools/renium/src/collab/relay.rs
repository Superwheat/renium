use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::system::files::atomic_write_file;

const WORKER_SOURCE: &str = include_str!("../../../renium-relay/src/index.ts");
const WORKER_MANIFEST: &str = include_str!("../../../renium-relay/package.json");
const WORKER_CONFIG: &str = include_str!("../../../renium-relay/wrangler.toml");
const WORKER_TSCONFIG: &str = include_str!("../../../renium-relay/tsconfig.json");

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Preferences {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    relay_url: Option<String>,
}

fn preferences_path() -> Result<PathBuf> {
    Ok(crate::app::update::user_data_dir()?.join("collab.json"))
}

fn read_preferences() -> Preferences {
    preferences_path()
        .ok()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_preferences(preferences: &Preferences) -> Result<()> {
    let path = preferences_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write_file(&path, &serde_json::to_vec_pretty(preferences)?)
}

pub(crate) fn default_relay() -> Option<String> {
    read_preferences()
        .relay_url
        .filter(|value| !value.trim().is_empty())
}

pub(crate) fn set_default_relay(url: Option<&str>) -> Result<Value> {
    let mut preferences = read_preferences();
    preferences.relay_url = url
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty());
    if let Some(url) = &preferences.relay_url
        && !(url.starts_with("https://")
            || url.starts_with("http://")
            || url.starts_with("wss://")
            || url.starts_with("ws://"))
    {
        bail!("Relay URLs start with https:// (or http:// for a local relay); got {url}");
    }
    write_preferences(&preferences)?;
    Ok(json!({ "relay": preferences.relay_url }))
}

fn relay_workspace() -> Result<PathBuf> {
    Ok(crate::app::update::user_data_dir()?.join("relay"))
}

fn write_sources(root: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(root.join("src"))?;
    for (name, content) in [
        ("src/index.ts", WORKER_SOURCE),
        ("package.json", WORKER_MANIFEST),
        ("wrangler.toml", WORKER_CONFIG),
        ("tsconfig.json", WORKER_TSCONFIG),
    ] {
        let path = root.join(name);
        if std::fs::read(&path).is_ok_and(|existing| existing == content.as_bytes()) {
            continue;
        }
        atomic_write_file(&path, content.as_bytes())?;
    }
    Ok(())
}

fn tool(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.cmd")
    } else {
        name.to_string()
    }
}

fn run_streaming(program: &str, args: &[&str], cwd: &std::path::Path) -> Result<Vec<String>> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| {
            format!("Could not run {program}; install Node.js from https://nodejs.org and retry")
        })?;
    let stdout = child.stdout.take().context("child has no stdout")?;
    let mut lines = Vec::new();
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        eprintln!("{line}");
        lines.push(line);
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("{program} {} failed ({status})", args.join(" "));
    }
    Ok(lines)
}

pub(crate) fn find_workers_url(lines: &[String]) -> Option<String> {
    lines.iter().find_map(|line| {
        let start = line.find("https://")?;
        let candidate = &line[start..];
        let end = candidate
            .find(|character: char| character.is_whitespace())
            .unwrap_or(candidate.len());
        let url = candidate[..end].trim_end_matches('/');
        url.ends_with(".workers.dev").then(|| url.to_string())
    })
}

pub(crate) fn deploy() -> Result<Value> {
    let root = relay_workspace()?;
    write_sources(&root)?;
    eprintln!("[renium] preparing the relay in {}", root.display());
    if !root.join("node_modules").is_dir() {
        run_streaming(
            &tool("npm"),
            &["install", "--no-audit", "--no-fund", "--loglevel=error"],
            &root,
        )?;
    }
    eprintln!(
        "[renium] deploying with wrangler; a browser window opens if Cloudflare needs you to sign in"
    );
    let lines = run_streaming(&tool("npx"), &["--yes", "wrangler", "deploy"], &root)?;
    let url = find_workers_url(&lines).context(
        "wrangler finished but printed no workers.dev address; run `npx wrangler deploy` in the relay folder to see why",
    )?;
    set_default_relay(Some(&url))?;
    Ok(json!({ "relay": url, "deployed": true, "workspace": root.display().to_string() }))
}

#[cfg(test)]
mod tests {
    use super::find_workers_url;

    #[test]
    fn extracts_the_deployed_worker_address() {
        let lines = vec![
            "Uploaded renium-relay (2.10 sec)".to_string(),
            "Deployed renium-relay triggers (0.50 sec)".to_string(),
            "  https://renium-relay.example.workers.dev".to_string(),
            "Current Version ID: abc".to_string(),
        ];
        assert_eq!(
            find_workers_url(&lines),
            Some("https://renium-relay.example.workers.dev".to_string())
        );
        assert_eq!(find_workers_url(&["nothing here".to_string()]), None);
    }
}
