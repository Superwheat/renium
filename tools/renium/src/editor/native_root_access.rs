//! Engine access for native-root writes. The routing, identity checks and
//! transaction fences remain in apply_write and run unchanged in its tests.
#[cfg(any(windows, target_os = "macos"))]
use crate::studio::native::serializer;
use anyhow::Result;
use std::time::Duration;

pub(super) enum Property {
    #[cfg(any(windows, target_os = "macos"))]
    Engine(serializer::NativeProperty),
    #[cfg(test)]
    Test {
        class: String,
        identity: String,
        state: TestState,
    },
}

impl Property {
    pub(super) fn class_name(&self) -> &str {
        match self {
            #[cfg(any(windows, target_os = "macos"))]
            Self::Engine(property) => &property.class_name,
            #[cfg(test)]
            Self::Test { class, .. } => class,
        }
    }
    pub(super) fn instance_id(&self) -> &str {
        match self {
            #[cfg(any(windows, target_os = "macos"))]
            Self::Engine(property) => &property.instance_id,
            #[cfg(test)]
            Self::Test { identity, .. } => identity,
        }
    }
    pub(super) fn ensure_writable(&self) -> Result<()> {
        match self {
            #[cfg(any(windows, target_os = "macos"))]
            Self::Engine(property) => property.ensure_writable(),
            #[cfg(test)]
            Self::Test { .. } => Ok(()),
        }
    }
    pub(super) fn restart_deadline(&mut self, duration: Duration) {
        match self {
            #[cfg(any(windows, target_os = "macos"))]
            Self::Engine(property) => property.restart_deadline(duration),
            #[cfg(test)]
            Self::Test { .. } => {
                let _ = duration;
            }
        }
    }
    pub(super) fn read(&mut self) -> Result<String> {
        match self {
            #[cfg(any(windows, target_os = "macos"))]
            Self::Engine(property) => property.read(),
            #[cfg(test)]
            Self::Test { state, .. } => Ok(state.borrow().value.clone()),
        }
    }
    pub(super) fn write(&mut self, value: &str) -> Result<()> {
        match self {
            #[cfg(any(windows, target_os = "macos"))]
            Self::Engine(property) => property.write(value).map(|_| ()),
            #[cfg(test)]
            Self::Test { state, .. } => {
                let mut state = state.borrow_mut();
                state.writes += 1;
                state.value = value.into();
                Ok(())
            }
        }
    }
}

pub(super) enum Terrain {
    #[cfg(any(windows, target_os = "macos"))]
    Engine(serializer::NativeTerrain),
    #[cfg(test)]
    Test { identity: String, state: TestState },
}

impl Terrain {
    pub(super) fn instance_id(&self) -> &str {
        match self {
            #[cfg(target_os = "macos")]
            Self::Engine(terrain) => terrain.instance_id(),
            #[cfg(windows)]
            Self::Engine(_) => "", // Windows retains the prepared object itself.
            #[cfg(test)]
            Self::Test { identity, .. } => identity,
        }
    }
    pub(super) fn expect(&mut self, baseline: &str) -> Result<()> {
        match self {
            #[cfg(any(windows, target_os = "macos"))]
            Self::Engine(terrain) => terrain.expect(baseline),
            #[cfg(test)]
            Self::Test { .. } => {
                let _ = baseline;
                Ok(())
            }
        }
    }
    pub(super) fn write(
        &mut self,
        token: &str,
        smooth: Option<&[u8]>,
        physics: Option<&[u8]>,
    ) -> Result<bool> {
        match self {
            #[cfg(any(windows, target_os = "macos"))]
            Self::Engine(terrain) => terrain.write(token, smooth, physics),
            #[cfg(test)]
            Self::Test { state, .. } => {
                let _ = (token, smooth, physics);
                state.borrow_mut().writes += 1;
                Ok(true)
            }
        }
    }
    pub(super) fn fingerprint(&self) -> String {
        match self {
            #[cfg(any(windows, target_os = "macos"))]
            Self::Engine(terrain) => terrain.fingerprint(),
            #[cfg(test)]
            Self::Test { .. } => "test-baseline".into(),
        }
    }
}

pub(super) fn prepare_property(
    pid: u32,
    title: &str,
    segments: &[String],
    ordinals: &[usize],
    name: &str,
    timeout: Duration,
) -> Result<Property> {
    #[cfg(test)]
    if let Some(state) = mock_target(segments, ordinals) {
        let class = state.borrow().class.clone();
        let identity = state.borrow().identity.clone();
        return Ok(Property::Test {
            class,
            identity,
            state,
        });
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        serializer::prepare_property(pid, title, segments, ordinals, name, timeout)
            .map(Property::Engine)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (pid, title, segments, ordinals, name, timeout);
        anyhow::bail!("Native property access requires Windows or macOS")
    }
}

pub(super) fn prepare_terrain(
    pid: u32,
    title: &str,
    segments: &[String],
    ordinals: &[usize],
    timeout: Duration,
) -> Result<Terrain> {
    #[cfg(test)]
    if let Some(state) = mock_target(segments, ordinals) {
        let identity = state.borrow().identity.clone();
        return Ok(Terrain::Test { identity, state });
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        serializer::prepare_terrain(pid, title, segments, ordinals, timeout).map(Terrain::Engine)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (pid, title, segments, ordinals, timeout);
        anyhow::bail!("Native Terrain access requires Windows or macOS")
    }
}

pub(super) fn register_history(pid: u32, title: &str, token: &str, terrain: bool) -> Result<()> {
    #[cfg(test)]
    if TEST_ACCESS.with_borrow(Option::is_some) {
        return Ok(());
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        serializer::register_history_if_available(pid, title, token, terrain).map(|_| ())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (pid, title, token, terrain);
        anyhow::bail!("Native history access requires Windows or macOS")
    }
}

#[cfg(test)]
pub(super) type TestState = std::rc::Rc<std::cell::RefCell<TestAccess>>;
#[cfg(test)]
#[derive(Default)]
pub(super) struct TestAccess {
    pub class: String,
    pub identity: String,
    pub value: String,
    pub paths: Vec<(Vec<String>, Vec<usize>)>,
    pub writes: usize,
}
#[cfg(test)]
thread_local! { pub(super) static TEST_ACCESS: std::cell::RefCell<Option<TestState>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
fn mock_target(segments: &[String], ordinals: &[usize]) -> Option<TestState> {
    TEST_ACCESS
        .with_borrow(|state| state.clone())
        .inspect(|state| {
            state
                .borrow_mut()
                .paths
                .push((segments.into(), ordinals.into()))
        })
}
