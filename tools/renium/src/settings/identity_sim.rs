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
            reference: &reference,
            observed: &observed,
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
