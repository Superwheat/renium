use anyhow::{Context, Result, bail};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;

fn helper_path(helper: &[u8], platform: &str) -> Result<std::path::PathBuf> {
    let mut hasher = DefaultHasher::new();
    helper.hash(&mut hasher);
    let path = std::env::temp_dir().join(format!(
        "renium-input-shield-{platform}-{:016x}",
        hasher.finish()
    ));
    if !path.is_file() {
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&temporary, helper)
            .with_context(|| format!("Could not write {}", temporary.display()))?;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("Could not make {} executable", temporary.display()))?;
        if let Err(error) = std::fs::rename(&temporary, &path)
            && !path.is_file()
        {
            return Err(error).with_context(|| format!("Could not install {}", path.display()));
        }
        let _ = std::fs::remove_file(temporary);
    }
    Ok(path)
}

pub(super) struct InputShield {
    child: Child,
}

impl Drop for InputShield {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(super) fn start(
    helper: &[u8],
    platform: &str,
    arguments: impl IntoIterator<Item = String>,
) -> Result<InputShield> {
    let mut child = Command::new(helper_path(helper, platform)?)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Could not start the {platform} input shield"))?;
    let stdout = child
        .stdout
        .take()
        .with_context(|| format!("Could not read the {platform} input shield status"))?;
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = sender.send(result);
    });
    match receiver.recv_timeout(std::time::Duration::from_secs(1)) {
        Ok(Ok(line)) if line.trim() == "ready" => Ok(InputShield { child }),
        outcome => {
            let _ = child.kill();
            let output = child.wait_with_output().ok();
            let detail = output
                .as_ref()
                .map(|value| String::from_utf8_lossy(&value.stderr).trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| match outcome {
                    Ok(Ok(line)) => line.trim().to_string(),
                    Ok(Err(error)) => error.to_string(),
                    Err(_) => "startup timed out".to_string(),
                });
            bail!("Could not create the {platform} input shield: {detail}")
        }
    }
}
