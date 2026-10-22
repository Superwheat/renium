use super::*;
fn pick23(state: &mut u64, n: usize) -> usize {
    *state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
    (*state >> 32) as usize % n
}
#[test]
fn review23_isomorphic_incoming_and_descendant_graphs() {
    let mut failures = Vec::new();
    let mut budgets = 0;
    let selected = std::env::var("REVIEW23_SEED")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    for seed in 0..600u64 {
        if selected.is_some_and(|v| v != seed) {
            continue;
        }
        let mut random = seed + 1;
        let nested = seed % 2 == 0;
        let width = 2 + (seed % 3 == 0) as usize;
        let mut e = twin_document();
        let mut nodes = Vec::new();
        let mut roots = Vec::new();
        for g in 0..3 {
            for _ in 0..width {
                let r = twin_add(
                    &mut e,
                    0,
                    &format!("G{g}"),
                    if nested { "Folder" } else { "ObjectValue" },
                    json!({}),
                );
                roots.push(r);
                nodes.push(if nested {
                    twin_add(&mut e, r, "Pointer", "ObjectValue", json!({}))
                } else {
                    r
                });
            }
        }
        for &node in &nodes {
            let t = nodes[pick23(&mut random, nodes.len())];
            let r = twin_ref(&e, t);
            e.instances[node].properties.insert("Value".into(), r);
        }
        // Unique referrers outside any twin component point into roots or descendants.
        for name in ["OutsideA", "OutsideB"] {
            let t = nodes[pick23(&mut random, nodes.len())];
            let r = twin_ref(&e, t);
            twin_add(&mut e, 0, name, "ObjectValue", json!({"Value":r}));
        }
        if seed % 5 == 0 {
            e.instances[roots[0]].properties.insert(
                "UniqueId".into(),
                json!({"_type":"UniqueId","value":"00000000000000000000000000000123"}),
            );
        }
        let mut s = e.clone();
        if seed % 3 != 1 {
            let index = pick23(&mut random, roots.len());
            let old = roots[index];
            let mut root = s.instances[old].clone();
            root.settings_id = "extra-root".into();
            root.properties.remove("UniqueId");
            let r = s.instances.len();
            s.instances.push(root);
            if nested {
                let mut child = s.instances[nodes[index]].clone();
                child.settings_id = "extra-child".into();
                child.parent_index = Some(r);
                s.instances.push(child);
            }
        }
        let mut s = twin_observation(&s);
        let c = twin_align(&mut e, &mut s);
        assert_twin_ids_unique(&s);
        assert_eq!(pairing::twins::take_recoveries(), 0, "seed={seed}");
        if c.iter().any(|v| v.contains("comparison budget")) {
            budgets += 1;
        } else if !c.is_empty() {
            eprintln!("ISOMORPHIC_FAILURE seed={seed} conflicts={c:?}");
            failures.push((seed, c));
        }
    }
    eprintln!("ISOMORPHIC cases=600 budgets={budgets} failures={failures:?}");
    assert_eq!(budgets, 0);
    assert!(failures.is_empty(), "{failures:?}");
}

#[test]
fn review23_outside_reference_to_identical_descendant() {
    let mut e = twin_document();
    let mut first = 0;
    for _ in 0..2 {
        let root = twin_add(&mut e, 0, "Twin", "Folder", json!({}));
        let child = twin_add(&mut e, root, "Pointer", "ObjectValue", json!({}));
        let r = twin_ref(&e, child);
        e.instances[child].properties.insert("Value".into(), r);
        if first == 0 {
            first = child;
        }
    }
    let r = twin_ref(&e, first);
    twin_add(&mut e, 0, "Reader", "ObjectValue", json!({"Value":r}));
    let mut s = twin_observation(&e);
    let resolved = pairing::twins::resolve(&e, &s);
    eprintln!(
        "OUTSIDE before_align partners={:?} proven={:?} recovered={}",
        resolved.partners, resolved.proven, resolved.recovered
    );
    let target_of = |d: &SettingsBytecode, name: &str| {
        let record = d.instances.iter().find(|i| i.name == name).unwrap();
        let id = record.properties["Value"]["settingsId"].as_str().unwrap();
        d.instances
            .iter()
            .position(|i| i.settings_id == id)
            .unwrap()
    };
    assert_eq!(
        resolved.partners[target_of(&s, "Reader")],
        Some(target_of(&e, "Reader"))
    );
    let c = twin_align(&mut e, &mut s);
    eprintln!("OUTSIDE conflicts={c:?}");
    assert!(c.is_empty(), "{c:?}");
}
