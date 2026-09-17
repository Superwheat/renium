pub(crate) mod command;
pub(crate) mod document;
pub(crate) mod mirror;
pub(crate) mod relay;
pub(crate) mod room;
pub(crate) mod tunnel;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use yrs::sync::Awareness;

use crate::system::LockRecover;
use crate::system::files::atomic_write_file;
use mirror::{Mirror, MirrorStats};
use room::{Link, Room, UpstreamStatus};
use tunnel::Tunnel;

const STATE_FILE: &str = "collab.json";
const STATE_VERSION: u32 = 1;
const SYNC_WAIT: Duration = Duration::from_secs(20);
const AWARENESS_RENEWAL: Duration = Duration::from_secs(15);
const AWARENESS_STALE_MS: u64 = 45_000;
const TUNNEL_WAIT: Duration = Duration::from_secs(45);

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Role {
    Host,
    Guest,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PersistedSession {
    version: u32,
    role: Role,
    invite: String,
    name: String,
    tunnel: bool,
}

pub(crate) struct Session {
    root: PathBuf,
    role: Role,
    name: String,
    invite: String,
    token: String,
    local_port: u16,
    room: Arc<Room>,
    upstream: Option<Arc<UpstreamStatus>>,
    tunnel: Option<Arc<Tunnel>>,
    stats: Arc<MirrorStats>,
    stop: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
    started: Instant,
    error: Mutex<Option<String>>,
    ready: AtomicBool,
}

#[derive(Default)]
pub(crate) struct Manager {
    sessions: Mutex<HashMap<PathBuf, Arc<Session>>>,
}

pub(crate) struct StartOptions {
    pub(crate) name: Option<String>,
    pub(crate) relay: Option<String>,
    pub(crate) tunnel: bool,
}

fn canonical_root(root: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("Could not create {}", root.display()))?;
    Ok(crate::system::files::strip_extended_prefix(
        std::fs::canonicalize(root)?,
    ))
}

fn random_hex(bytes: usize) -> Result<String> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer)
        .map_err(|error| anyhow::anyhow!("OS random source failed: {error}"))?;
    Ok(buffer.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn default_name() -> String {
    std::env::var("RENIUM_COLLAB_NAME")
        .ok()
        .or_else(|| std::env::var("USERNAME").ok())
        .or_else(|| std::env::var("USER").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "Collaborator".to_string())
}

fn color_for(name: &str) -> String {
    let palette = [
        "#4f8ff7", "#56c27e", "#e2b340", "#e06060", "#b07cf0", "#3fbfbf", "#f08c4a", "#d95fa6",
    ];
    let hash = crate::system::files::fnv1a(name.as_bytes());
    palette[(hash % palette.len() as u64) as usize].to_string()
}

fn websocket_url(invite: &str) -> Result<String> {
    let trimmed = invite.trim();
    let url = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if trimmed.starts_with("wss://") || trimmed.starts_with("ws://") {
        trimmed.to_string()
    } else {
        bail!("Invites are wss:// or https:// links; got {trimmed}");
    };
    if !url.contains("token=") {
        bail!("The invite has no token");
    }
    Ok(url)
}

fn state_path(root: &Path) -> PathBuf {
    root.join(".renium").join(STATE_FILE)
}

fn persist(root: &Path, session: &PersistedSession) -> Result<()> {
    crate::project::version_control::ensure_renium_local_state_ignored(root)?;
    let bytes = serde_json::to_vec_pretty(session)?;
    atomic_write_file(&state_path(root), &bytes)
}

fn forget(root: &Path) {
    let _ = std::fs::remove_file(state_path(root));
}

fn persisted(root: &Path) -> Option<PersistedSession> {
    let bytes = std::fs::read(state_path(root)).ok()?;
    let session: PersistedSession = serde_json::from_slice(&bytes).ok()?;
    (session.version == STATE_VERSION).then_some(session)
}

impl Manager {
    pub(crate) fn start(&self, root: &Path, options: StartOptions) -> Result<Value> {
        let root = canonical_root(root)?;
        if self.sessions.lock_recover().contains_key(&root) {
            bail!(
                "A collaboration session is already running for {}",
                root.display()
            );
        }
        if !root.join("renium.project.jsonc").is_file() {
            bail!(
                "{} has no renium.project.jsonc; start from a project or join an invite",
                root.display()
            );
        }
        let name = options.name.unwrap_or_else(default_name);
        let relay = match options.relay {
            Some(relay) if relay.trim().is_empty() => Some(relay::default_relay().context(
                "No relay is configured yet; run `rbx collab relay deploy` once, or pass --relay URL",
            )?),
            other => other,
        };
        let session = match relay {
            Some(relay) => {
                let relay = relay.trim().trim_end_matches('/').to_string();
                let base = websocket_url(&format!("{relay}/?token=x"))?;
                let base = base.trim_end_matches("/?token=x").to_string();
                let invite = format!("{base}/rooms/{}?token={}", random_hex(12)?, random_hex(16)?);
                self.launch(root.clone(), Role::Host, name, Some(invite), false)?
            }
            None => self.launch(root.clone(), Role::Host, name, None, options.tunnel)?,
        };
        Ok(session.status())
    }

    pub(crate) fn join(&self, root: &Path, invite: &str, name: Option<String>) -> Result<Value> {
        let root = canonical_root(root)?;
        if self.sessions.lock_recover().contains_key(&root) {
            bail!(
                "A collaboration session is already running for {}",
                root.display()
            );
        }
        let invite = websocket_url(invite)?;
        let session = self.launch(
            root,
            Role::Guest,
            name.unwrap_or_else(default_name),
            Some(invite),
            false,
        )?;
        Ok(session.status())
    }

    fn launch(
        &self,
        root: PathBuf,
        role: Role,
        name: String,
        upstream: Option<String>,
        expose: bool,
    ) -> Result<Arc<Session>> {
        let awareness = Arc::new(Mutex::new(Awareness::new(document::new_doc())));
        {
            let guard = awareness.lock_recover();
            guard.set_local_state(json!({ "user": name, "color": color_for(&name) }))?;
        }
        let room = Room::new(awareness)?;
        let token = random_hex(16)?;
        let listener = room::listen(room.clone(), token.clone())?;
        let upstream_status = upstream
            .as_ref()
            .map(|_| Arc::new(UpstreamStatus::default()));
        if let (Some(url), Some(status)) = (&upstream, &upstream_status) {
            room::connect_upstream(room.clone(), url.clone(), status.clone())?;
        }
        let tunnel = if expose {
            let tunnel = tunnel::open(listener.port, TUNNEL_WAIT).inspect_err(|_| room.stop())?;
            Some(tunnel)
        } else {
            None
        };
        let invite = match (&upstream, &tunnel) {
            (Some(url), _) => url.clone(),
            (None, Some(tunnel)) => format!(
                "{}/?token={token}",
                tunnel.url.replacen("https://", "wss://", 1)
            ),
            (None, None) => format!("ws://127.0.0.1:{}/?token={token}", listener.port),
        };
        let session = Arc::new(Session {
            root: root.clone(),
            role,
            name: name.clone(),
            invite: invite.clone(),
            token,
            local_port: listener.port,
            room: room.clone(),
            upstream: upstream_status.clone(),
            tunnel: tunnel.clone(),
            stats: Arc::new(MirrorStats::default()),
            stop: Arc::new(AtomicBool::new(false)),
            worker: Mutex::new(None),
            started: Instant::now(),
            error: Mutex::new(None),
            ready: AtomicBool::new(false),
        });
        let heartbeat_room = room.clone();
        thread::Builder::new()
            .name("renium-collab-heartbeat".to_string())
            .spawn(move || {
                while !heartbeat_room.stopping() {
                    thread::sleep(AWARENESS_RENEWAL);
                    if heartbeat_room.stopping() {
                        break;
                    }
                    let awareness = heartbeat_room.awareness().lock_recover();
                    if let Some(state) = awareness.local_state::<Value>() {
                        let _ = awareness.set_local_state(state);
                    }
                }
            })
            .context("Could not start the collaboration heartbeat")?;
        let worker_session = session.clone();
        let handle = thread::Builder::new()
            .name(format!("renium-collab-{}", listener.port))
            .spawn(move || worker_session.run())
            .context("Could not start the collaboration worker")?;
        *session.worker.lock_recover() = Some(handle);
        persist(
            &root,
            &PersistedSession {
                version: STATE_VERSION,
                role,
                invite,
                name,
                tunnel: expose,
            },
        )?;
        self.sessions.lock_recover().insert(root, session.clone());
        Ok(session)
    }

    pub(crate) fn stop(&self, root: &Path) -> Result<Value> {
        let root = canonical_root(root)?;
        let session = self.sessions.lock_recover().remove(&root);
        forget(&root);
        match session {
            Some(session) => {
                session.shutdown();
                Ok(json!({ "stopped": true, "role": session.role }))
            }
            None => Ok(json!({ "stopped": false })),
        }
    }

    pub(crate) fn restore(&self, root: &Path) -> Result<Option<Value>> {
        let root = canonical_root(root)?;
        if self.sessions.lock_recover().contains_key(&root) {
            return Ok(None);
        }
        let Some(saved) = persisted(&root) else {
            return Ok(None);
        };
        let session = match saved.role {
            Role::Guest => self.launch(root, Role::Guest, saved.name, Some(saved.invite), false)?,
            Role::Host if saved.invite.starts_with("ws://127.0.0.1") => {
                self.launch(root, Role::Host, saved.name, None, saved.tunnel)?
            }
            Role::Host if saved.tunnel => self.launch(root, Role::Host, saved.name, None, true)?,
            Role::Host => self.launch(root, Role::Host, saved.name, Some(saved.invite), false)?,
        };
        Ok(Some(session.status()))
    }

    pub(crate) fn status(&self, root: &Path) -> Result<Value> {
        let root = canonical_root(root)?;
        let session = self.sessions.lock_recover().get(&root).cloned();
        let mut status = match session {
            Some(session) => session.status(),
            None => json!({ "running": false }),
        };
        if let Some(relay) = relay::default_relay() {
            status["defaultRelay"] = json!(relay);
        }
        Ok(status)
    }

    pub(crate) fn set_awareness(&self, root: &Path, fields: &Map<String, Value>) -> Result<Value> {
        let root = canonical_root(root)?;
        let session = self
            .sessions
            .lock_recover()
            .get(&root)
            .cloned()
            .context("No collaboration session is running for this project")?;
        session.set_awareness(fields)?;
        Ok(json!({ "ok": true }))
    }
}

impl Session {
    fn run(self: Arc<Self>) {
        if let Err(error) = self.run_inner() {
            *self.error.lock_recover() = Some(format!("{error:#}"));
        }
    }

    fn run_inner(&self) -> Result<()> {
        if let Some(status) = &self.upstream {
            let started = Instant::now();
            while !self.room.synced_upstream() {
                if self.stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                if started.elapsed() >= SYNC_WAIT {
                    let detail = status
                        .error
                        .lock_recover()
                        .clone()
                        .unwrap_or_else(|| "no response from the room".to_string());
                    *self.error.lock_recover() =
                        Some(format!("Still trying to reach the room: {detail}"));
                }
                thread::sleep(Duration::from_millis(50));
            }
            *self.error.lock_recover() = None;
        }
        let mut mirror = Mirror::open(
            &self.root,
            self.room.awareness().clone(),
            self.stats.clone(),
        )?;
        if room::document_is_empty(self.room.awareness()) {
            if !self.root.join("renium.project.jsonc").is_file() {
                bail!(
                    "The room is empty and {} has no project to share",
                    self.root.display()
                );
            }
            mirror.seed_from_disk()?;
        } else {
            mirror.materialize()?;
            mirror.refresh_inputs()?;
        }
        self.ready.store(true, Ordering::Release);
        mirror.run(&self.stop);
        Ok(())
    }

    fn shutdown(&self) {
        self.stop.store(true, Ordering::Release);
        self.room.stop();
        if let Some(tunnel) = &self.tunnel {
            tunnel.stop();
        }
        if let Some(handle) = self.worker.lock_recover().take() {
            let _ = handle.join();
        }
    }

    fn set_awareness(&self, fields: &Map<String, Value>) -> Result<()> {
        let awareness = self.room.awareness().lock_recover();
        let mut state = awareness
            .local_state::<Value>()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        for (key, value) in fields {
            if value.is_null() {
                state.remove(key);
            } else {
                state.insert(key.clone(), value.clone());
            }
        }
        state
            .entry("user".to_string())
            .or_insert_with(|| json!(self.name));
        state
            .entry("color".to_string())
            .or_insert_with(|| json!(color_for(&self.name)));
        awareness.set_local_state(Value::Object(state))?;
        Ok(())
    }

    fn participants(&self) -> Vec<Value> {
        let awareness = self.room.awareness().lock_recover();
        let own = awareness.client_id();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_millis() as u64)
            .unwrap_or(0);
        let mut participants = awareness
            .iter()
            .filter_map(|(client, state)| {
                let data = state.data.as_ref()?;
                if client != own && now.saturating_sub(state.last_updated) > AWARENESS_STALE_MS {
                    return None;
                }
                let mut value: Map<String, Value> = serde_json::from_str(data).ok()?;
                value.insert("clientId".to_string(), json!(client));
                value.insert("self".to_string(), json!(client == own));
                Some(Value::Object(value))
            })
            .collect::<Vec<_>>();
        participants.sort_by(|left, right| {
            right["self"]
                .as_bool()
                .cmp(&left["self"].as_bool())
                .then_with(|| left["user"].as_str().cmp(&right["user"].as_str()))
        });
        participants
    }

    fn status(&self) -> Value {
        let connected = match &self.upstream {
            Some(status) => status.connected.load(Ordering::Acquire),
            None => true,
        };
        let error = self
            .error
            .lock_recover()
            .clone()
            .or_else(|| self.stats.error.lock_recover().clone())
            .or_else(|| {
                self.upstream
                    .as_ref()
                    .and_then(|status| status.error.lock_recover().clone())
                    .filter(|_| !connected)
            });
        let mut value = json!({
            "running": true,
            "role": self.role,
            "name": self.name,
            "invite": self.invite,
            "localUrl": format!("ws://127.0.0.1:{}/?token={}", self.local_port, self.token),
            "connected": connected,
            "ready": self.ready.load(Ordering::Acquire),
            "tunnel": self.tunnel.is_some(),
            "peers": self.room.peer_count(Link::Local),
            "participants": self.participants(),
            "files": self.stats.files.load(Ordering::Acquire),
            "localChanges": self.stats.local_changes.load(Ordering::Acquire),
            "remoteChanges": self.stats.remote_changes.load(Ordering::Acquire),
            "uptimeSeconds": self.started.elapsed().as_secs(),
            "root": self.root.display().to_string(),
        });
        if let Some(error) = error {
            value["error"] = json!(error);
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invites_normalize_to_websocket_urls() {
        assert_eq!(
            websocket_url("https://a.trycloudflare.com/?token=t").unwrap(),
            "wss://a.trycloudflare.com/?token=t"
        );
        assert_eq!(
            websocket_url(" ws://127.0.0.1:1/?token=t ").unwrap(),
            "ws://127.0.0.1:1/?token=t"
        );
        assert!(websocket_url("https://a.b/").is_err());
        assert!(websocket_url("ftp://x?token=1").is_err());
    }

    #[test]
    fn colors_are_stable_per_name() {
        assert_eq!(color_for("alice"), color_for("alice"));
        assert!(color_for("alice").starts_with('#'));
    }
}
