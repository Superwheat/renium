//! Operation-scoped compatibility evidence. A version is diagnostic metadata,
//! never permission to call into an engine. Only read-only probes grant access.
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};

use crate::system::LockRecover;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum NativeOperation {
    Snapshot,
    Capture,
    ServiceReader,
    DescriptorBinding,
    History,
    VoxelHistory,
    Terrain,
    TerrainObservation,
    AttributeObservation,
    UndoSelection,
    PackageNotices,
    PackageActions,
    StudioActions,
    AudioGate,
    Input,
    ViewportCapture,
}

impl NativeOperation {
    pub(crate) const ALL: [Self; 16] = [
        Self::Snapshot,
        Self::Capture,
        Self::ServiceReader,
        Self::DescriptorBinding,
        Self::History,
        Self::VoxelHistory,
        Self::Terrain,
        Self::TerrainObservation,
        Self::AttributeObservation,
        Self::UndoSelection,
        Self::PackageNotices,
        Self::PackageActions,
        Self::StudioActions,
        Self::AudioGate,
        Self::Input,
        Self::ViewportCapture,
    ];

    fn fallback(self) -> &'static str {
        match self {
            Self::Snapshot | Self::Capture => {
                "plugin serialization; preserve the saved base for unreadable fields"
            }
            Self::ServiceReader => "plugin binary import",
            Self::DescriptorBinding => {
                "plugin properties where accessible; protected properties report unavailable"
            }
            Self::History | Self::VoxelHistory => "explicit transaction rollback",
            Self::Terrain => "plugin voxel API where lossless; otherwise retain pending changes",
            Self::TerrainObservation => "plugin Terrain observation",
            Self::AttributeObservation => "plugin attribute observation",
            Self::UndoSelection => "Studio default undo selection",
            Self::PackageNotices => "targeted dialog watcher",
            Self::PackageActions => "plugin package actions where supported",
            Self::StudioActions => "bridge commands where supported",
            Self::AudioGate => "audible output",
            Self::Input => "bridge input",
            Self::ViewportCapture => "bridge capture",
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProbeVerdict {
    pub(crate) operation: NativeOperation,
    pub(crate) verdict: &'static str,
    pub(crate) reason: String,
    pub(crate) readings: Value,
    pub(crate) fallback: &'static str,
}

impl ProbeVerdict {
    pub(super) fn from_result(operation: NativeOperation, result: &Result<Value>) -> Self {
        let (verdict, reason, readings) = match result {
            Ok(readings) => (
                "native",
                "Read-only structural probe passed".into(),
                readings.clone(),
            ),
            Err(error) => ("degraded", format!("{error:#}"), Value::Null),
        };
        Self {
            operation,
            verdict,
            reason,
            readings,
            fallback: operation.fallback(),
        }
    }

    fn unprobed(operation: NativeOperation) -> Self {
        Self {
            operation,
            verdict: "unprobed",
            reason: "Not requested in this process session".into(),
            readings: Value::Null,
            fallback: operation.fallback(),
        }
    }
}

#[derive(Default)]
struct SessionEvidence {
    identity: String,
    verdicts: HashMap<NativeOperation, ProbeVerdict>,
}

static EVIDENCE: OnceLock<Mutex<HashMap<u32, SessionEvidence>>> = OnceLock::new();

pub(crate) fn record(pid: u32, operation: NativeOperation, result: &Result<Value>) {
    let identity = crate::app::update::process_start_identity(pid).unwrap_or_default();
    let mut sessions = EVIDENCE.get_or_init(Default::default).lock_recover();
    let session = sessions.entry(pid).or_default();
    if session.identity != identity {
        *session = SessionEvidence {
            identity,
            ..Default::default()
        };
    }
    session
        .verdicts
        .insert(operation, ProbeVerdict::from_result(operation, result));
}

pub(crate) fn cached(pid: u32) -> Value {
    let identity = crate::app::update::process_start_identity(pid);
    let sessions = EVIDENCE.get_or_init(Default::default).lock_recover();
    let current = sessions
        .get(&pid)
        .filter(|session| session.identity == identity.as_deref().unwrap_or_default());
    let operations = NativeOperation::ALL
        .into_iter()
        .map(|operation| {
            current
                .and_then(|session| session.verdicts.get(&operation))
                .cloned()
                .unwrap_or_else(|| ProbeVerdict::unprobed(operation))
        })
        .collect::<Vec<_>>();
    json!({ "pid": pid, "processIdentity": identity, "processAlive": identity.is_some(), "readOnly": true, "operations": operations })
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn run(pid: u32, title: &str, operation: NativeOperation) -> Result<Value> {
    #[cfg(windows)]
    let result = super::windows::probe_operation(pid, title, operation);
    #[cfg(target_os = "macos")]
    let result = super::macos::probe_operation(pid, title, operation);
    record(pid, operation, &result);
    result
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn command(args: crate::cli::NativeProbeArgs) -> Result<()> {
    let pid = if let Some(executable) = &args.launch {
        let file = args
            .file
            .as_ref()
            .expect("clap requires --file with --launch");
        anyhow::ensure!(
            file.is_file(),
            "Scratch place does not exist: {}",
            file.display()
        );
        crate::project::workflows::spawn_studio(executable, &[file.as_os_str()])?
    } else {
        args.pid.expect("clap requires --pid or --launch")
    };
    let requested_title = args.title.as_deref().unwrap_or_default();
    if args.launch.is_some() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            if super::target_name(pid, requested_title)
                .is_ok_and(|title| run(pid, &title, NativeOperation::DescriptorBinding).is_ok())
            {
                break;
            }
            if !crate::daemon::is_process_alive(pid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }
    let title =
        super::target_name(pid, requested_title).unwrap_or_else(|_| requested_title.to_string());
    for operation in NativeOperation::ALL {
        let _ = run(pid, &title, operation);
    }
    let mut report = cached(pid);
    report["build"] = super::studio_build_number(pid)
        .ok()
        .map_or(Value::Null, |build| json!(build));
    report["target"] = json!(title);
    report["ok"] = json!(
        report["operations"]
            .as_array()
            .is_some_and(|rows| rows.iter().all(|row| row["verdict"] == "native"))
    );
    crate::app::output::print_json_output(&report, false)?;
    anyhow::ensure!(
        !args.require_native || report["ok"] == true,
        "Native compatibility probes failed; see the per-operation report"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failing_probe_does_not_grant_or_revoke_other_operations() {
        let pid = std::process::id();
        record(
            pid,
            NativeOperation::Capture,
            &Err(anyhow::anyhow!("serializer ABI changed")),
        );
        record(
            pid,
            NativeOperation::PackageNotices,
            &Ok(json!({"flag": 123})),
        );
        let report = cached(pid);
        let rows = report["operations"].as_array().unwrap();
        assert!(rows.iter().any(|row| row["operation"] == "capture"
            && row["verdict"] == "degraded"
            && row["reason"] == "serializer ABI changed"));
        assert!(
            rows.iter()
                .any(|row| row["operation"] == "packageNotices" && row["verdict"] == "native")
        );
        assert!(
            rows.iter()
                .any(|row| row["operation"] == "terrain" && row["verdict"] == "unprobed")
        );
    }
}
