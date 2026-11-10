use super::*;
use crate::settings::bytecode::SETTINGS_BINARY_VERSION;
use serde_json::json;

fn frame(degrees: f64) -> Value {
    let angle = degrees.to_radians();
    let c = angle.cos() as f32 as f64;
    let s = angle.sin() as f32 as f64;
    json!({"_type":"CFrame", "components":[0,0,0,c,-s,0,s,c,0,0,0,1]})
}

#[test]
fn identity_radius_contains_nine_micro_rotation_boundary() {
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: Vec::new(),
    };
    for n in 0..80 {
        let mut instance =
            SettingsBytecodeInstance::new(format!("id-{n}"), "Twin".into(), "Part".into(), None);
        instance.properties.insert("CFrame".into(), frame(0.0));
        instance.properties["CFrame"]["components"][3] = json!(1.0 - n as f64 * 0.0001);
        document.instances.push(instance);
    }
    let candidates = (0..80).collect::<Vec<_>>();
    let index = IdentityCandidateIndex::build(&document, &candidates).unwrap();
    for n in candidates {
        for drift in [-9e-6, -8.940696716308594e-6, 8.940696716308594e-6, 9e-6] {
            let mut observed = document.instances[n].clone();
            observed.properties["CFrame"]["components"][3] = json!(
                document.instances[n].properties["CFrame"]["components"][3]
                    .as_f64()
                    .unwrap()
                    + drift
            );
            assert!(
                index
                    .matching(&observed)
                    .unwrap()
                    .iter()
                    .any(|(_, candidate)| *candidate == n)
            );
        }
    }
}
