//! Permission state for protected properties only. Ordinary edits do not enter
//! this gate, and a permission grant never changes Roblox's global security.
use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const WRITE_WARNING: &str = "Protected property writes can change engine-managed state. Running unknown scripts or plugins in this session may be dangerous. Enable read-write only at the user's explicit request.";
const APPROVAL_TTL: Duration = Duration::from_secs(300);
#[cfg(any(windows, target_os = "macos", test))]
const MAX_PENDING: usize = 128;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Mode {
    #[default]
    Ask,
    ReadOnly,
    ReadWrite,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Scope {
    pub(crate) project: String,
    pub(crate) runtime: String,
    pub(crate) pid: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum Operation {
    Read,
    Write { value: Value },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Intent {
    pub(crate) path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) ordinals: Vec<usize>,
    pub(crate) class_name: String,
    // Resolved by the backend, not accepted as an identity claim from Luau.
    pub(crate) instance_id: String,
    pub(crate) property: String,
    pub(crate) operation: Operation,
}

#[cfg(any(windows, target_os = "macos", test))]
#[derive(Debug, Serialize)]
#[serde(
    tag = "status",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum Decision {
    Allowed,
    ApprovalRequired { request_id: String, intent: Intent },
}

// Studio prints numbers canonically (and rounds float properties to f32).
// Do not apply numeric coercion to strings or enums: "01" is not string "1".
#[cfg(any(windows, target_os = "macos", test))]
pub(crate) fn property_text_matches(
    class: &str,
    property: &str,
    expected: &str,
    actual: &str,
) -> bool {
    use rbx_dom_weak::types::VariantType;
    use rbx_reflection::DataType;
    if expected == actual {
        return true;
    }
    let Ok(database) = rbx_reflection_database::get() else {
        return false;
    };
    let Some(descriptor) =
        crate::rbx::encode::rbx_model_property_descriptor(database, class, property)
    else {
        return false;
    };
    match descriptor.data_type {
        DataType::Value(VariantType::Float32) => {
            matches!((expected.parse::<f32>(), actual.parse::<f32>()), (Ok(a), Ok(b)) if a.is_finite() && a == b)
        }
        DataType::Value(VariantType::Float64) => {
            matches!((expected.parse::<f64>(), actual.parse::<f64>()), (Ok(a), Ok(b)) if a.is_finite() && a == b)
        }
        DataType::Value(VariantType::Int32 | VariantType::Int64) => {
            matches!((expected.parse::<i64>(), actual.parse::<i64>()), (Ok(a), Ok(b)) if a == b)
        }
        DataType::Value(VariantType::Vector3) => {
            let vector = |text: &str| -> Option<[f32; 3]> {
                let mut parts = text.split(',');
                let mut values = [0.0; 3];
                for value in &mut values {
                    *value = parts.next()?.trim().parse::<f32>().ok()?;
                    if !value.is_finite() {
                        return None;
                    }
                }
                parts.next().is_none().then_some(values)
            };
            matches!((vector(expected), vector(actual)), (Some(a), Some(b)) if a == b)
        }
        _ => false,
    }
}

struct Pending {
    scope: Scope,
    intent: Intent,
    created: Instant,
}

#[derive(Default)]
pub(crate) struct Policy {
    modes: HashMap<Scope, Mode>,
    pending: HashMap<String, Pending>,
}

impl Policy {
    pub(crate) fn mode(&self, scope: &Scope) -> Mode {
        self.modes.get(scope).copied().unwrap_or_default()
    }

    pub(crate) fn set_mode(&mut self, scope: &Scope, mode: Mode, accept_risk: bool) -> Result<()> {
        if mode == Mode::ReadWrite && !accept_risk {
            bail!("{WRITE_WARNING} Explicit risk acknowledgement is required");
        }
        if mode != Mode::ReadWrite && accept_risk {
            bail!("Risk acknowledgement applies only to read-write mode");
        }
        self.pending.retain(|_, pending| &pending.scope != scope);
        if mode == Mode::Ask {
            self.modes.remove(scope);
        } else {
            self.modes.insert(scope.clone(), mode);
        }
        Ok(())
    }

    pub(crate) fn retain_runtimes(&mut self, active: &[String]) {
        self.modes
            .retain(|scope, _| active.contains(&scope.runtime));
        self.pending.retain(|_, pending| {
            pending.created.elapsed() < APPROVAL_TTL && active.contains(&pending.scope.runtime)
        });
    }

    #[cfg(any(windows, target_os = "macos", test))]
    pub(crate) fn check(&mut self, scope: &Scope, intent: &Intent) -> Result<Decision> {
        let trusted = trusted_property(intent)?;
        match (self.mode(scope), &intent.operation) {
            (Mode::ReadWrite, _) | (Mode::ReadOnly, Operation::Read) => Ok(Decision::Allowed),
            (Mode::ReadOnly, Operation::Write { .. }) => {
                bail!(
                    "Read-only mode rejects protected writes; request approval in ask mode or explicitly enable read-write"
                )
            }
            (Mode::Ask, _) => {
                if trusted {
                    return Ok(Decision::Allowed);
                }
                self.pending
                    .retain(|_, p| p.created.elapsed() < APPROVAL_TTL);
                if let Some((id, _)) = self
                    .pending
                    .iter()
                    .find(|(_, p)| &p.scope == scope && &p.intent == intent)
                {
                    return Ok(Decision::ApprovalRequired {
                        request_id: id.clone(),
                        intent: intent.clone(),
                    });
                }
                if self.pending.len() >= MAX_PENDING {
                    bail!(
                        "Too many pending property approvals; approve or reject existing requests first"
                    );
                }
                let id = super::authorization::random_id()?;
                self.pending.insert(
                    id.clone(),
                    Pending {
                        scope: scope.clone(),
                        intent: intent.clone(),
                        created: Instant::now(),
                    },
                );
                Ok(Decision::ApprovalRequired {
                    request_id: id,
                    intent: intent.clone(),
                })
            }
        }
    }

    // Consume before invoking the backend. A lost reply does not create a
    // reusable permission. The backend must recheck instance identity first.
    pub(crate) fn approve(&mut self, scope: &Scope, id: &str) -> Result<Intent> {
        let Some(pending) = self.pending.get(id) else {
            bail!("Property approval is unknown, expired, revoked, or already used");
        };
        if &pending.scope != scope {
            bail!("Property approval belongs to a different Studio runtime or project");
        }
        let pending = self.pending.remove(id).expect("approval checked above");
        if pending.created.elapsed() >= APPROVAL_TTL {
            bail!("Property approval expired; request the operation again");
        }
        Ok(pending.intent)
    }

    pub(crate) fn reject(&mut self, scope: &Scope, id: &str) -> Result<()> {
        if self.pending.get(id).is_some_and(|p| &p.scope != scope) {
            bail!("Property approval belongs to a different Studio runtime or project");
        }
        self.pending.remove(id);
        Ok(())
    }
}

// The class/identity come from native resolution, never from a caller's claim.
// Keep this explicit: adding a member is a security policy decision, not a
// wildcard or a trusted flag that requests can supply.
#[cfg(any(windows, target_os = "macos", test))]
fn trusted_property(intent: &Intent) -> Result<bool> {
    if intent.property != "CollisionFidelity"
        || !crate::rbx::decode::rbx_reflection_class_is_a(
            rbx_reflection_database::get()?,
            &intent.class_name,
            "TriangleMeshPart",
        )
    {
        return Ok(false);
    }
    if let Operation::Write { value } = &intent.operation
        && !matches!(
            value.as_str(),
            Some("Default" | "Hull" | "Box" | "PreciseConvexDecomposition")
        )
    {
        bail!("CollisionFidelity must be Default, Hull, Box, or PreciseConvexDecomposition");
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn property_readback_uses_declared_precision_without_coercing_text() {
        assert!(property_text_matches(
            "MeshPart",
            "UnscaledCofm",
            "1.6071045649823645e-7, 0, 0.055660780519247055",
            "1.60710456e-07, 0, 0.0556607805"
        ));
        assert!(!property_text_matches(
            "MeshPart",
            "UnscaledCofm",
            "1, 2, 3",
            "1, 2, 4"
        ));
        assert!(!property_text_matches(
            "MeshPart",
            "UnscaledCofm",
            "1, 2, 3",
            "1, 2, 3, 4"
        ));
        assert!(property_text_matches("NumberValue", "Value", "3.0", "3"));
        assert!(property_text_matches(
            "Part",
            "Transparency",
            "0.1",
            "0.100000001"
        ));
        assert!(!property_text_matches(
            "NumberValue",
            "Value",
            "0.1",
            "0.100000001"
        ));
        assert!(!property_text_matches(
            "Part",
            "Transparency",
            "0.1",
            "0.1001"
        ));
        assert!(!property_text_matches("StringValue", "Value", "01", "1"));
        assert!(!property_text_matches("UnknownClass", "Value", "3.0", "3"));
        assert!(!property_text_matches(
            "NumberValue",
            "Value",
            "inf",
            "Infinity"
        ));
    }

    fn scope() -> Scope {
        Scope {
            project: "test-project".into(),
            runtime: "edit-one".into(),
            pid: 10,
        }
    }
    fn intent(value: Option<Value>) -> Intent {
        Intent {
            path: "Workspace".into(),
            ordinals: Vec::new(),
            class_name: "Workspace".into(),
            instance_id: "instance-one".into(),
            property: "StreamingEnabled".into(),
            operation: value.map_or(Operation::Read, |value| Operation::Write { value }),
        }
    }
    fn ticket(policy: &mut Policy, intent: &Intent) -> String {
        let Decision::ApprovalRequired { request_id, .. } = policy.check(&scope(), intent).unwrap()
        else {
            panic!("ask must not grant automatic permission")
        };
        request_id
    }

    #[test]
    fn mode_matrix_requires_opt_in_and_read_only_never_writes() {
        let mut policy = Policy::default();
        assert_eq!(policy.mode(&scope()), Mode::Ask);
        for mode in [Mode::Ask, Mode::ReadOnly, Mode::ReadWrite] {
            assert_eq!(
                policy.set_mode(&scope(), mode, false).is_ok(),
                mode != Mode::ReadWrite
            );
            if mode == Mode::ReadWrite {
                policy.set_mode(&scope(), mode, true).unwrap();
            }
            let read = policy.check(&scope(), &intent(None));
            let write = policy.check(&scope(), &intent(Some(json!(true))));
            match mode {
                Mode::Ask => {
                    assert!(matches!(read.unwrap(), Decision::ApprovalRequired { .. }));
                    assert!(matches!(write.unwrap(), Decision::ApprovalRequired { .. }));
                }
                Mode::ReadOnly => {
                    assert!(matches!(read.unwrap(), Decision::Allowed));
                    assert!(write.is_err());
                }
                Mode::ReadWrite => {
                    assert!(matches!(read.unwrap(), Decision::Allowed));
                    assert!(matches!(write.unwrap(), Decision::Allowed));
                }
            }
        }
    }

    #[test]
    fn trusted_collision_policy_checks_class_value_and_read_only_mode() {
        let mut policy = Policy::default();
        let mut request = Intent {
            class_name: "MeshPart".into(),
            property: "CollisionFidelity".into(),
            ..intent(None)
        };
        assert!(matches!(
            policy.check(&scope(), &request).unwrap(),
            Decision::Allowed
        ));
        for value in ["Default", "Hull", "Box", "PreciseConvexDecomposition"] {
            request.operation = Operation::Write {
                value: json!(value),
            };
            assert!(matches!(
                policy.check(&scope(), &request).unwrap(),
                Decision::Allowed
            ));
        }
        for invalid in [
            json!(4),
            json!(false),
            json!("Anything"),
            json!({"trusted":true}),
        ] {
            request.operation = Operation::Write { value: invalid };
            assert!(policy.check(&scope(), &request).is_err());
        }
        request.operation = Operation::Write {
            value: json!("Hull"),
        };
        policy.set_mode(&scope(), Mode::ReadOnly, false).unwrap();
        assert!(policy.check(&scope(), &request).is_err());
        policy.set_mode(&scope(), Mode::Ask, false).unwrap();
        request.class_name = "Folder".into();
        assert!(matches!(
            policy.check(&scope(), &request).unwrap(),
            Decision::ApprovalRequired { .. }
        ));
        request.class_name = "MeshPart".into();
        request.property = "Source".into();
        assert!(matches!(
            policy.check(&scope(), &request).unwrap(),
            Decision::ApprovalRequired { .. }
        ));
    }

    #[test]
    fn approvals_are_exact_one_shot_and_cannot_cross_scopes() {
        let mut policy = Policy::default();
        let original = intent(Some(json!(false)));
        let id = ticket(&mut policy, &original);
        assert_eq!(
            id,
            ticket(&mut policy, &original),
            "repeat prompts reuse the same pending decision"
        );
        assert_ne!(id, ticket(&mut policy, &intent(Some(json!(true)))));
        for changed in [
            Scope {
                runtime: "replacement".into(),
                ..scope()
            },
            Scope { pid: 11, ..scope() },
            Scope {
                project: "another".into(),
                ..scope()
            },
        ] {
            assert!(policy.approve(&changed, &id).is_err());
        }
        assert_eq!(policy.approve(&scope(), &id).unwrap(), original);
        assert!(policy.approve(&scope(), &id).is_err());
    }

    #[test]
    fn mode_changes_runtime_departure_and_expiry_revoke_pending_approvals() {
        let mut policy = Policy::default();
        let id = ticket(&mut policy, &intent(Some(Value::Null)));
        policy.set_mode(&scope(), Mode::ReadOnly, false).unwrap();
        assert!(policy.approve(&scope(), &id).is_err());
        policy.retain_runtimes(&[]);
        assert_eq!(policy.mode(&scope()), Mode::Ask);
        let id = ticket(&mut policy, &intent(None));
        policy.pending.get_mut(&id).unwrap().created -= APPROVAL_TTL;
        assert!(policy.approve(&scope(), &id).is_err());
        let id = ticket(&mut policy, &intent(None));
        policy.reject(&scope(), &id).unwrap();
        assert!(policy.approve(&scope(), &id).is_err());
    }
}
