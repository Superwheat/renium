use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::system::LockRecover;
use crate::system::tools::download_to_file;

const RELEASES: &str = "https://github.com/cloudflare/cloudflared/releases/latest/download";

pub(crate) struct Tunnel {
    child: Mutex<Option<Child>>,
    pub(crate) url: String,
}

impl Tunnel {
    pub(crate) fn stop(&self) {
        if let Some(mut child) = self.child.lock_recover().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.stop();
    }
}

fn binary_name() -> &'static str {
    if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    }
}

fn download_asset() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "cloudflared-windows-amd64.exe",
        ("windows", "x86") => "cloudflared-windows-386.exe",
        ("macos", "aarch64") => "cloudflared-darwin-arm64.tgz",
        ("macos", "x86_64") => "cloudflared-darwin-amd64.tgz",
        ("linux", "x86_64") => "cloudflared-linux-amd64",
        ("linux", "aarch64") => "cloudflared-linux-arm64",
        (os, arch) => bail!("No cloudflared build is published for {os}/{arch}"),
    })
}

pub(crate) fn binary_path() -> Result<PathBuf> {
    Ok(crate::app::update::user_data_dir()?
        .join("tools")
        .join(binary_name()))
}

pub(crate) fn ensure_binary() -> Result<PathBuf> {
    if let Ok(found) = which("cloudflared") {
        return Ok(found);
    }
    let path = binary_path()?;
    if path.is_file() {
        return Ok(path);
    }
    let asset = download_asset()?;
    let directory = path.parent().context("cloudflared path has no parent")?;
    std::fs::create_dir_all(directory)?;
    let url = format!("{RELEASES}/{asset}");
    if asset.ends_with(".tgz") {
        let archive = directory.join(asset);
        download_to_file(&url, &archive)?;
        let status = Command::new("tar")
            .args(["-xzf"])
            .arg(&archive)
            .arg("-C")
            .arg(directory)
            .status()
            .context("Could not run tar to unpack cloudflared")?;
        let _ = std::fs::remove_file(&archive);
        if !status.success() || !path.is_file() {
            bail!("cloudflared archive did not contain the expected binary");
        }
    } else {
        download_to_file(&url, &path)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(path)
}

fn which(name: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
        if cfg!(windows) {
            let candidate = directory.join(format!("{name}.exe"));
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    bail!("{name} is not on PATH")
}

pub(crate) fn open(port: u16, timeout: Duration) -> Result<Arc<Tunnel>> {
    let binary = ensure_binary()?;
    let mut child = Command::new(&binary)
        .args([
            "tunnel",
            "--no-autoupdate",
            "--url",
            &format!("http://127.0.0.1:{port}"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Could not start {}", binary.display()))?;
    let stderr = child.stderr.take().context("cloudflared has no stderr")?;
    let found = Arc::new(Mutex::new(None::<String>));
    let reader_found = found.clone();
    thread::Builder::new()
        .name("renium-cloudflared".to_string())
        .spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(url) = find_tunnel_url(&line) {
                    *reader_found.lock_recover() = Some(url);
                }
            }
        })
        .context("Could not read cloudflared output")?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(url) = found.lock_recover().clone() {
            wait_for_dns(&url, deadline);
            return Ok(Arc::new(Tunnel {
                child: Mutex::new(Some(child)),
                url,
            }));
        }
        if let Ok(Some(status)) = child.try_wait() {
            bail!("cloudflared exited before publishing a tunnel ({status})");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            bail!(
                "cloudflared did not publish a tunnel address within {} seconds",
                timeout.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_dns(url: &str, deadline: Instant) {
    use std::net::ToSocketAddrs;
    let Some(host) = url.strip_prefix("https://") else {
        return;
    };
    while Instant::now() < deadline {
        if (host, 443u16)
            .to_socket_addrs()
            .is_ok_and(|mut addresses| addresses.next().is_some())
        {
            return;
        }
        thread::sleep(Duration::from_millis(500));
    }
}

pub(crate) fn find_tunnel_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let candidate = &line[start..];
    let end = candidate
        .find(|character: char| character.is_whitespace() || character == '|')
        .unwrap_or(candidate.len());
    let url = &candidate[..end];
    url.ends_with(".trycloudflare.com").then(|| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::find_tunnel_url;

    #[test]
    fn extracts_quick_tunnel_address() {
        let line = "2026-09-17T00:00:00Z INF |  https://quiet-river-1234.trycloudflare.com                                     |";
        assert_eq!(
            find_tunnel_url(line),
            Some("https://quiet-river-1234.trycloudflare.com".to_string())
        );
        assert_eq!(
            find_tunnel_url("INF Requesting new quick Tunnel on trycloudflare.com..."),
            None
        );
        assert_eq!(find_tunnel_url("https://api.trycloudflare.com/x"), None);
    }
}
