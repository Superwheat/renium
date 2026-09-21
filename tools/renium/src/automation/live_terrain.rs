use super::*;

#[derive(Default)]
pub(super) struct Observer {
    relay: Vec<String>,
    attached: bool,
    paused: bool,
    catch_up: bool,
    retry_at: Option<Instant>,
    retry_delay: Duration,
    pub(super) error: Option<String>,
}

impl Observer {
    pub(super) fn start(context: &BoundContext, bridge: &BridgeServer) -> Self {
        let mut observer = Self::default();
        let state = (|| {
            let runtime = context
                .runtime_id
                .as_deref()
                .context("Live Sync has no Studio runtime")?;
            let state = bridge.call_for_runtime_with_timeout(
                "getStudioChangeState",
                json!({"nativeTerrainRelay": true}),
                BridgeTarget::Edit,
                runtime,
                Some(Duration::from_secs(3)),
            )?;
            ensure_plugin_api_ok(&state)?;
            Ok(state)
        })();
        match state {
            Ok(state) => observer.refresh(context, bridge, &state),
            Err(error) => observer.failed(error, Instant::now()),
        }
        observer.catch_up = true;
        observer
    }

    pub(super) fn wait_seconds(&self) -> f64 {
        if self.paused {
            return 1.0;
        }
        self.retry_at.map_or(25.0, |deadline| {
            deadline
                .saturating_duration_since(Instant::now())
                .as_secs_f64()
                .clamp(0.1, 25.0)
        })
    }

    pub(super) fn refresh(&mut self, context: &BoundContext, bridge: &BridgeServer, state: &Value) {
        if state["tracking"].as_bool() == Some(false)
            || state["twoWaySyncEnabled"].as_bool() == Some(false)
        {
            *self = Self {
                catch_up: true,
                ..Self::default()
            };
            return;
        }
        if state["nativeTerrainPaused"].as_bool() == Some(true) {
            self.pause();
            return;
        }
        let catch_up = self.catch_up;
        let path = serde_json::from_value(state["nativeTerrainRelay"].clone())
            .context("Studio has no Terrain relay; update the plugin if tracking is running");
        self.update(path, Instant::now(), |path| {
            let runtime = context
                .runtime_id
                .as_deref()
                .context("Live Sync has no Studio runtime")?;
            anyhow::ensure!(
                state["runtimeId"].as_str() == Some(runtime),
                "Terrain relay runtime changed"
            );
            let info = bridge.cached_bridge_info_for_runtime(BridgeTarget::Edit, runtime)?;
            let pid = bridge.studio_pid_for_runtime(BridgeTarget::Edit, runtime)?;
            let title = crate::studio::native::serializer::target_name(pid, &info.place_name)?;
            crate::studio::native::serializer::observe_terrain(pid, &title, path, catch_up)
        });
        self.catch_up = true;
    }

    fn failed(&mut self, error: anyhow::Error, now: Instant) {
        let message = format!(
            "unavailable, Studio Terrain edits will not sync live; retrying automatically: {error:#}"
        );
        if self.error.as_ref() != Some(&message) {
            log_global(5, format_args!("[renium] Terrain observation {message}"));
        }
        self.error = Some(message);
        self.attached = false;
        self.retry_delay = next_retry_delay(self.retry_delay);
        self.retry_at = Some(now + self.retry_delay);
    }

    fn pause(&mut self) {
        self.paused = true;
        self.catch_up = true;
        self.attached = false;
        self.retry_at = None;
        self.retry_delay = Duration::ZERO;
        self.error = Some("waiting for Edit mode".into());
    }

    fn update(
        &mut self,
        path: Result<Vec<String>>,
        now: Instant,
        attach: impl FnOnce(&[String]) -> Result<()>,
    ) {
        self.paused = false;
        let path = match path {
            Ok(path) => path,
            Err(error) => {
                self.relay.clear();
                if self.retry_at.is_none_or(|deadline| now >= deadline) {
                    self.failed(error, now);
                }
                self.attached = false;
                return;
            }
        };
        if path != self.relay {
            self.relay = path;
            self.attached = false;
            self.retry_at = None;
            self.retry_delay = Duration::ZERO;
        }
        if self.attached || self.retry_at.is_some_and(|deadline| now < deadline) {
            return;
        }
        match attach(&self.relay) {
            Ok(()) => {
                self.attached = true;
                self.error = None;
                self.retry_at = None;
                self.retry_delay = Duration::ZERO;
            }
            Err(error) => self.failed(error, now),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terrain_observation_recovers_and_follows_replaced_relays() {
        let mut observer = Observer::default();
        let now = Instant::now();
        let path = vec!["CoreGui".into(), "ReniumTerrainChanges_edit".into()];
        observer.update(Ok(path.clone()), now, |_| {
            anyhow::bail!("model transitioning")
        });
        assert!(observer.error.is_some());
        observer.update(Ok(path.clone()), now, |_| panic!("retry must back off"));
        observer.update(Ok(path.clone()), now + Duration::from_secs(1), |_| Ok(()));
        assert!(observer.attached && observer.error.is_none());
        observer.update(Ok(path.clone()), now, |_| {
            panic!("healthy relays need no rediscovery")
        });
        observer.update(
            Err(anyhow::anyhow!("tracking stopped")),
            now,
            |_| unreachable!(),
        );
        assert!(!observer.attached);
        let replaced = vec!["CoreGui".into(), "ReniumTerrainChanges_replaced".into()];
        observer.update(Ok(replaced.clone()), now, |actual| {
            assert_eq!(actual, replaced);
            Ok(())
        });
        assert!(observer.attached && observer.error.is_none());
        observer.pause();
        assert!(!observer.attached && observer.wait_seconds() == 1.0);
        observer.update(Ok(replaced.clone()), now, |_| Ok(()));
        assert!(observer.attached && observer.error.is_none());
        let mut restarted = Observer::default();
        restarted.update(Ok(replaced), now, |_| Ok(()));
        assert!(restarted.attached);
    }
}
