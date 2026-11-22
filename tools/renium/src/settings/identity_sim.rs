use super::*;
use crate::tests::support::{SimRng, run_seeds};
use serde_json::json;

fn exhaustive_proposals(
    context: &IdentityScoreContext<'_>,
    observed: &[usize],
    candidates: &[usize],
) -> Vec<(usize, usize)> {
    observed
        .iter()
        .filter_map(|&index| {
            let scores = candidates
                .iter()
                .filter_map(|&candidate| {
                    identity_score(context, candidate, index).map(|score| (candidate, score))
                })
                .collect::<Vec<_>>();
            let best = scores.iter().map(|(_, score)| score).max()?;
            let mut winners = scores.iter().filter(|(_, score)| score == best);
            let candidate = winners.next()?.0;
            winners.next().is_none().then_some((index, candidate))
        })
        .collect()
}

#[test]
fn parallel_proposals_match_exhaustive_scores_on_seeded_groups() {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap();
    run_seeds(64, |seed, _| {
        let mut rng = SimRng(seed);
        let count = if seed % 2 == 0 { 96 } else { 15 };
        let mut reference = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: (0..count)
                .map(|index| {
                    let mut node = SettingsBytecodeInstance::new(
                        format!("id:{index}"),
                        "Duplicate".into(),
                        "Part".into(),
                        None,
                    );
                    node.properties
                        .insert("Value".into(), json!(rng.below(count / 2)));
                    node.attributes
                        .insert("Nested".into(), json!({"a/b": [rng.below(3), "text"]}));
                    node
                })
                .collect(),
        };
        if seed % 3 == 0 {
            for index in 0..count {
                if rng.below(4) == 0 {
                    reference.instances[index].properties.insert(
                        "Target".into(),
                        json!({"_type":"Ref", "settingsId":format!("id:{}", rng.below(count))}),
                    );
                }
            }
        }
        let mut order = (0..count).collect::<Vec<_>>();
        rng.shuffle(&mut order);
        let mut observed = reference.clone();
        for (index, &original) in order.iter().enumerate() {
            observed.instances[index] = reference.instances[original].clone();
            if rng.below(8) == 0 {
                observed.instances[index]
                    .properties
                    .insert("Value".into(), json!("edited"));
            }
        }
        let reference_graph = build_reference_graph(&reference);
        let observed_graph = build_reference_graph(&observed);
        let mut assigned_reference = vec![None; count];
        let mut assigned_observed = vec![None; count];
        for (index, &original) in order.iter().enumerate() {
            if rng.below(3) == 0 {
                assigned_reference[index] = Some(original);
                assigned_observed[original] = Some(index);
            }
        }
        let keys = (0..count).map(|_| rng.below(3) + 1).collect::<Vec<_>>();
        let subtree_keys = (
            keys.clone(),
            order.iter().map(|&index| keys[index]).collect(),
        );
        let context = IdentityScoreContext {
            reference: RowSource::Full(&reference.instances),
            observed: RowSource::Full(&observed.instances),
            reference_graph: &reference_graph,
            observed_graph: &observed_graph,
            assigned_reference: &assigned_reference,
            assigned_observed: &assigned_observed,
            subtree_keys: (seed % 4 == 0).then_some(&subtree_keys),
        };
        let mut candidates = (0..count).collect::<Vec<_>>();
        rng.shuffle(&mut candidates);
        let expected = exhaustive_proposals(&context, &order, &candidates);
        let actual = pool.install(|| identity_group_proposals(&context, &order, &candidates));
        if actual != expected {
            return Err(
                "proposal order, winner or ambiguity differs from exhaustive scoring".into(),
            );
        }
        Ok(())
    });
}

#[test]
fn parallel_subtree_keys_match_interned_keys_on_seeded_trees() {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap();
    run_seeds(64, |seed, _| {
        let mut rng = SimRng(seed);
        let count = if seed % 4 == 0 { 160 } else { 40 };
        let build_doc = |rng: &mut SimRng| -> SettingsBytecode {
            let mut instances = Vec::with_capacity(count);
            for index in 0..count {
                let parent = if index == 0 {
                    None
                } else {
                    Some(rng.below(index))
                };
                let class = match rng.below(4) {
                    0 => "Model",
                    1 => "Folder",
                    2 => "MeshPart",
                    _ => "Part",
                };
                let name = match rng.below(4) {
                    0 => "Twin",
                    1 => "Mesh",
                    2 => "Container",
                    _ => "Part",
                };
                let mut node = SettingsBytecodeInstance::new(
                    format!("id:{index}"),
                    name.into(),
                    class.into(),
                    parent,
                );
                node.properties.insert(
                    "CFrame".into(),
                    json!({
                        "_type": "CFrame",
                        "components": [
                            rng.below(4) as f64,
                            0.0,
                            0.0,
                            1.0, 0.0, 0.0,
                            0.0, 1.0, 0.0,
                            0.0, 0.0, 1.0
                        ]
                    }),
                );
                if rng.below(3) == 0 {
                    let mut payload = "A".repeat(400);
                    if rng.below(2) == 0 {
                        payload.replace_range(180..184, "BBBB");
                    }
                    node.properties.insert(
                        "AeroMeshData".into(),
                        json!({"_type": "BinaryString", "base64": payload}),
                    );
                }
                if rng.below(5) == 0 {
                    node.properties.insert(
                        "Target".into(),
                        json!({"_type": "Ref", "settingsId": format!("id:{}", rng.below(count))}),
                    );
                }
                if rng.below(5) == 0 {
                    node.properties
                        .insert("ScriptGuid".into(), json!(format!("guid-{index}")));
                }
                if rng.below(4) == 0 {
                    node.attributes.insert("Tag".into(), json!(rng.below(3)));
                }
                instances.push(node);
            }
            SettingsBytecode {
                version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
                instances,
            }
        };
        let reference = build_doc(&mut rng);
        let mut observed = reference.clone();
        for index in 0..count {
            observed.instances[index].settings_id = format!("debug:{index}");
            if rng.below(6) == 0 {
                observed.instances[index]
                    .properties
                    .insert("Transparency".into(), json!(rng.below(3) as f64 / 4.0));
            }
        }
        let ref_required: Vec<bool> = (0..count).map(|_| rng.below(3) != 0).collect();
        let obs_required: Vec<bool> = (0..count).map(|_| rng.below(3) != 0).collect();

        let mut interned = AHashMap::new();
        let expected_ref = identity_subtree_keys(&reference, &mut interned, ref_required.clone());
        let expected_obs = identity_subtree_keys(&observed, &mut interned, obs_required.clone());

        let ref_graph = build_reference_graph(&reference);
        let obs_graph = build_reference_graph(&observed);
        let obs_tree = DocTree::build(&observed);
        let exact = pool.install(|| {
            compute_subtree_keys(
                &reference,
                &ref_graph,
                ref_required,
                &observed,
                &obs_graph,
                &obs_tree,
                obs_required,
            )
        });
        let (actual_ref, actual_obs) = (&exact.keys.0, &exact.keys.1);

        let mut exp_to_act = AHashMap::new();
        let mut act_to_exp = AHashMap::new();
        for (&exp, &act) in expected_ref
            .iter()
            .chain(&expected_obs)
            .zip(actual_ref.iter().chain(actual_obs))
        {
            if (exp == 0) != (act == 0) {
                return Err(format!("zero mismatch: expected={exp} actual={act}"));
            }
            if exp == 0 {
                continue;
            }
            if let Some(&prev) = exp_to_act.get(&exp)
                && prev != act
            {
                return Err(format!(
                    "split equivalence class: exp={exp} act={act} vs {prev}"
                ));
            }
            if let Some(&prev) = act_to_exp.get(&act)
                && prev != exp
            {
                return Err(format!(
                    "merged equivalence class: act={act} exp={exp} vs {prev}"
                ));
            }
            exp_to_act.insert(exp, act);
            act_to_exp.insert(act, exp);
        }
        for (index, &refl) in exact.ref_reflexive.iter().enumerate() {
            if refl && !holds_same_content(&reference.instances[index], &reference.instances[index])
            {
                return Err(format!("false reflexive flag at {index}"));
            }
        }
        Ok(())
    });
}

#[test]
fn match_settings_ids_handles_grids_float_drift_large_payloads_and_refs() {
    let mut instances = vec![SettingsBytecodeInstance::new(
        "ws".into(),
        "Workspace".into(),
        "Workspace".into(),
        None,
    )];
    let payload_a = "Q".repeat(4096);
    let payload_b = "R".repeat(4096);
    for m in 0..4 {
        let model_idx = instances.len();
        instances.push(SettingsBytecodeInstance::new(
            format!("model:{m}"),
            "Building".into(),
            "Model".into(),
            Some(0),
        ));
        for gx in 0..10 {
            for gz in 0..10 {
                let mut part = SettingsBytecodeInstance::new(
                    format!("mesh:{m}:{gx}:{gz}"),
                    "MeshPart".into(),
                    "MeshPart".into(),
                    Some(model_idx),
                );
                part.properties.insert(
                    "AeroMeshData".into(),
                    json!({
                        "_type": "BinaryString",
                        "base64": if (gx + gz) % 2 == 0 { &payload_a } else { &payload_b }
                    }),
                );
                part.properties.insert(
                    "CFrame".into(),
                    json!({
                        "_type": "CFrame",
                        "components": [
                            (m * 100 + gx * 4) as f64,
                            0.0,
                            (gz * 4) as f64,
                            1.0, 0.0, 0.0,
                            0.0, 1.0, 0.0,
                            0.0, 0.0, 1.0
                        ]
                    }),
                );
                instances.push(part);
            }
        }
        let mut weld = SettingsBytecodeInstance::new(
            format!("weld:{m}"),
            "Weld".into(),
            "Weld".into(),
            Some(model_idx),
        );
        weld.properties.insert(
            "Part0".into(),
            json!({"_type": "Ref", "settingsId": format!("mesh:{m}:0:0")}),
        );
        weld.properties.insert(
            "Part1".into(),
            json!({"_type": "Ref", "settingsId": format!("mesh:{m}:9:9")}),
        );
        instances.push(weld);
    }
    let reference = SettingsBytecode {
        version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
        instances,
    };

    let mut observed = reference.clone();
    let old_to_new: HashMap<String, String> = observed
        .instances
        .iter()
        .enumerate()
        .map(|(i, inst)| (inst.settings_id.clone(), format!("debug:0_{i}")))
        .collect();
    for (i, inst) in observed.instances.iter_mut().enumerate() {
        inst.settings_id = format!("debug:0_{i}");
        crate::snapshot::refs::remap_record_reference_ids(&mut inst.properties, &old_to_new, None);
        if i % 2 == 1
            && let Some(components) = inst
                .properties
                .get_mut("CFrame")
                .and_then(|v| v.get_mut("components"))
                .and_then(Value::as_array_mut)
        {
            let x = components[0].as_f64().unwrap();
            components[0] = json!(x + 1e-5);
            components[3] = json!(1.0 - 2e-6);
        }
    }
    observed.instances.push(SettingsBytecodeInstance::new(
        "debug:0_new".into(),
        "Added".into(),
        "Part".into(),
        Some(0),
    ));

    let (ids, matched, _) = match_settings_ids(&reference, &observed, false, false).unwrap();
    for i in 0..reference.instances.len() {
        assert_eq!(matched[i], Some(i), "mismatch at instance {i}");
        assert_eq!(ids[i], reference.instances[i].settings_id);
    }
    assert_eq!(matched[reference.instances.len()], None);
    assert_eq!(ids[reference.instances.len()], "debug:0_new");
}
