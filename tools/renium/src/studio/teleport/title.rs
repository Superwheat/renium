//! Window state only: no DataModel names or published identity are modified.
use super::*;

#[cfg(windows)]
struct Caption {
    pid: u32,
    identity: String,
    hwnd: isize,
    ordinary: String,
    written: Option<String>,
    cleanup: crate::studio::native::serializer::NativePlayCaption,
}

#[cfg(windows)]
impl Caption {
    fn update(&mut self, destination: Option<&str>, transitioning: bool) -> Result<()> {
        use crate::studio::input::{teleport_caption_update, teleport_caption_window};
        ensure!(
            crate::app::update::process_start_identity(self.pid).as_deref() == Some(&self.identity),
            "Teleport caption process ended"
        );
        let (hwnd, current) = teleport_caption_window(self.pid)?;
        ensure!(hwnd == self.hwnd, "Teleport caption window changed");
        // Accept Studio's normal caption rewrites, never strip a substring from
        // an unrelated window or restore over a new document's ordinary title.
        if self.written.as_deref() != Some(&current) {
            ensure!(
                current.ends_with(" - Roblox Studio"),
                "Studio caption ownership changed"
            );
            self.ordinary.clone_from(&current);
        }
        let next = destination.map_or_else(
            || {
                if transitioning {
                    format!("{} - Teleporting...", self.ordinary)
                } else {
                    self.ordinary.clone()
                }
            },
            |name| {
                let state = if transitioning {
                    "Teleporting"
                } else {
                    "Teleported"
                };
                format!("{} - {state} to {name}", self.ordinary)
            },
        );
        if next != current {
            self.cleanup.update(
                &self.ordinary,
                (transitioning || destination.is_some()).then_some(next.as_str()),
            )?;
            teleport_caption_update(self.pid, self.hwnd, &current, &next)?;
        }
        self.written = (transitioning || destination.is_some()).then_some(next);
        Ok(())
    }
}

#[cfg(windows)]
#[derive(Default)]
struct State {
    captions: Vec<Caption>,
    destination: Option<String>,
    retired: bool,
    session: Option<Session>,
    transitioning: bool,
    transition_name: Option<String>,
}

#[cfg(windows)]
impl State {
    fn apply(&mut self) -> Result<()> {
        for caption in &mut self.captions {
            let name = if self.transitioning {
                &self.transition_name
            } else {
                &self.destination
            };
            caption.update(name.as_deref(), self.transitioning)?;
        }
        Ok(())
    }

    fn restore(&mut self) {
        self.destination = None;
        self.transitioning = false;
        self.transition_name = None;
        for caption in &mut self.captions {
            if caption.written.is_some()
                && let Err(error) = caption.update(None, false)
            {
                crate::app::output::log_global(
                    2,
                    format_args!("[renium] teleport caption restore: {error:#}"),
                );
            }
        }
    }
}

pub(super) struct Lease {
    #[cfg(windows)]
    state: Arc<Mutex<State>>,
    #[cfg(windows)]
    worker: Option<thread::JoinHandle<()>>,
}

#[cfg(windows)]
pub(super) struct ClientCaption(Caption);
#[cfg(windows)]
impl ClientCaption {
    pub(super) fn start(pid: u32) -> Result<Self> {
        let identity =
            crate::app::update::process_start_identity(pid).context("Teleport client ended")?;
        let (hwnd, ordinary) = crate::studio::input::teleport_caption_window(pid)?;
        let cleanup =
            crate::studio::native::serializer::NativePlayCaption::arm(pid, &ordinary, hwnd)?;
        Ok(Self(Caption {
            pid,
            identity,
            hwnd,
            ordinary,
            written: None,
            cleanup,
        }))
    }
    pub(super) fn update(&mut self, name: Option<&str>, transitioning: bool) -> Result<()> {
        self.0.update(name, transitioning)
    }
}

impl Lease {
    pub(super) fn start(bridge: &Arc<BridgeServer>, session: &Session) -> Result<Self> {
        #[cfg(windows)]
        {
            let mut runtimes = session
                .clients(&bridge.list_bridge_clients())?
                .into_values()
                .map(|runtime| (BridgeTarget::Client, runtime))
                .collect::<Vec<_>>();
            runtimes.extend([
                (BridgeTarget::Edit, session.owner.clone()),
                (BridgeTarget::Server, session.server.clone()),
            ]);
            let mut pids = HashSet::new();
            let mut state = State {
                session: Some(session.clone()),
                ..State::default()
            };
            for (target, runtime) in runtimes {
                let pid = bridge.studio_pid_for_runtime(target, &runtime)?;
                if pids.insert(pid) {
                    let identity = crate::app::update::process_start_identity(pid)
                        .context("Teleport window process ended")?;
                    let (hwnd, ordinary) = crate::studio::input::teleport_caption_window(pid)?;
                    ensure!(
                        ordinary.ends_with(" - Roblox Studio"),
                        "Teleport window already has a caption owner"
                    );
                    let cleanup = crate::studio::native::serializer::NativePlayCaption::arm(
                        pid, &ordinary, hwnd,
                    )?;
                    state.captions.push(Caption {
                        pid,
                        identity,
                        hwnd,
                        ordinary,
                        written: None,
                        cleanup,
                    });
                }
            }
            let state = Arc::new(Mutex::new(state));
            let shared = Arc::clone(&state);
            let bridge = Arc::clone(bridge);
            let worker = thread::spawn(move || {
                loop {
                    {
                        let mut state = shared.lock_recover();
                        if state.retired
                            || !bridge.alive.load(Ordering::Relaxed)
                            || !state.session.as_ref().is_some_and(|session| {
                                if !session.registered() {
                                    return false;
                                }
                                let inventory = bridge.list_bridge_clients();
                                if state.transitioning {
                                    inventory.iter().any(|row| {
                                        row["role"] == "edit" && row["runtimeId"] == session.owner
                                    })
                                } else {
                                    session.clients(&inventory).is_ok()
                                }
                            })
                        {
                            state.retired = true;
                            state.restore();
                            break;
                        }
                        if (state.transitioning || state.destination.is_some())
                            && let Err(error) = state.apply()
                        {
                            crate::app::output::log_global(
                                2,
                                format_args!("[renium] teleport caption ended: {error:#}"),
                            );
                            state.retired = true;
                            state.restore();
                            break;
                        }
                    }
                    thread::park_timeout(Duration::from_millis(250));
                }
            });
            Ok(Self {
                state,
                worker: Some(worker),
            })
        }
        #[cfg(not(windows))]
        {
            let _ = (bridge, session);
            Ok(Self {})
        }
    }

    pub(super) fn arrived(&self, name: &str, at_source: bool) -> Result<()> {
        #[cfg(windows)]
        {
            let mut state = self.state.lock_recover();
            ensure!(!state.retired, "Teleport caption session ended");
            state.destination = (!at_source).then(|| name.to_owned());
            state.transitioning = false;
            state.apply()
        }
        #[cfg(not(windows))]
        {
            let _ = (name, at_source);
            Ok(())
        }
    }

    pub(super) fn transition(&self, name: Option<&str>) -> Result<()> {
        #[cfg(windows)]
        {
            let mut state = self.state.lock_recover();
            ensure!(!state.retired, "Teleport caption session ended");
            state.transitioning = true;
            state.transition_name = name.map(str::to_owned);
            state.apply()?;
        }
        #[cfg(not(windows))]
        let _ = name;
        Ok(())
    }

    pub(super) fn cancel_transition(&self) {
        #[cfg(windows)]
        {
            let mut state = self.state.lock_recover();
            state.transitioning = false;
            state.transition_name = None;
            if let Err(error) = state.apply() {
                crate::app::output::log_global(
                    2,
                    format_args!("[renium] teleport caption restore: {error:#}"),
                );
            }
        }
    }

    pub(super) fn rebind(&self, session: &Session, name: &str) -> Result<()> {
        #[cfg(windows)]
        {
            let mut state = self.state.lock_recover();
            ensure!(!state.retired, "Teleport caption session ended");
            state.session = Some(session.clone());
            state.transitioning = false;
        }
        #[cfg(not(windows))]
        let _ = session;
        self.arrived(name, session.current_place == session.source)
    }

    pub(super) fn restore(&self) {
        #[cfg(windows)]
        self.state.lock_recover().restore();
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            self.state.lock_recover().retired = true;
            if let Some(worker) = self.worker.take() {
                worker.thread().unpark();
                let _ = worker.join();
            }
            self.state.lock_recover().restore();
        }
    }
}
