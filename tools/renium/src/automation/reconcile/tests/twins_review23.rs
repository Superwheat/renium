use super::*;

fn namespace(d: &mut SettingsBytecode) {
    let ids = d
        .instances
        .iter()
        .map(|i| (i.settings_id.clone(), format!("s-{}", i.settings_id)))
        .collect();
    for i in &mut d.instances {
        i.settings_id = format!("s-{}", i.settings_id);
        remap_record_reference_ids(&mut i.properties, &ids);
        remap_record_reference_ids(&mut i.attributes, &ids);
    }
}

fn uid(d: &mut SettingsBytecode, i: usize, n: usize) {
    d.instances[i].properties.insert(
        "UniqueId".into(),
        json!({"_type":"UniqueId","value":format!("{n:032x}")}),
    );
}

fn mutual(a: &[usize], b: &[usize], n: usize, nested: bool, broken: bool) -> SettingsBytecode {
    let mut d = twin_document();
    let mut nodes = [HashMap::new(), HashMap::new()];
    for (g, order) in [a, b].into_iter().enumerate() {
        for &label in order {
            let root = twin_add(
                &mut d,
                0,
                if g == 0 { "A" } else { "B" },
                if nested { "Model" } else { "ObjectValue" },
                json!({}),
            );
            let node = if nested {
                twin_add(&mut d, root, "Pointer", "ObjectValue", json!({}))
            } else {
                root
            };
            nodes[g].insert(label, node);
        }
    }
    for g in 0..2 {
        for (&label, &node) in &nodes[g] {
            let target = if broken && g == 1 && label == n - 1 {
                0
            } else {
                label % n
            };
            let r = twin_ref(&d, nodes[1 - g][&target]);
            d.instances[node].properties.insert("Value".into(), r);
        }
    }
    d
}

fn invariants(e: &SettingsBytecode, s: &SettingsBytecode) -> pairing::twins::Resolved {
    let r = pairing::twins::resolve(e, s);
    assert!(!r.recovered);
    let mut targets = HashSet::new();
    for (source, &target) in r.partners.iter().enumerate() {
        if let Some(target) = target {
            assert!(targets.insert(target));
            assert_eq!(
                e.instances[target].class_name,
                s.instances[source].class_name
            );
        }
        if let Some(lock) = r.locked[source] {
            assert_eq!(target, Some(lock));
        }
        if r.rejected[source] {
            assert!(target.is_none());
            assert!(!r.proven[source]);
            assert!(r.locked[source].is_none());
        }
    }
    r
}

#[test]
fn review23_small_satisfiable_budget() {
    for n in [9, 12, 16, 20, 30, 40] {
        let order = (0..n).collect::<Vec<_>>();
        let mut addition = order.clone();
        addition.insert(1, n);
        let reverse = (0..n).rev().collect::<Vec<_>>();
        let e = mutual(&order, &order, n, false, false);
        let mut s = mutual(&addition, &reverse, n, false, false);
        namespace(&mut s);
        let start = Instant::now();
        let r = invariants(&e, &s);
        let resolve_ms = start.elapsed().as_secs_f64() * 1000.;
        let start = Instant::now();
        let c = twin_align(&mut e.clone(), &mut s.clone());
        eprintln!(
            "SMALL n={n} resolve_ms={resolve_ms:.3} align_ms={:.3} budgets={} conflicts={c:?}",
            start.elapsed().as_secs_f64() * 1000.,
            r.budget_conflicts.len()
        );
    }
}

fn broad_tree(n: usize, width: usize) -> SettingsBytecode {
    let mut e = twin_document();
    let anchor = twin_add(&mut e, 0, "Anchor", "Part", json!({}));
    let reference = twin_ref(&e, anchor);
    for label in 0..n {
        let root = twin_add(&mut e, 0, "Twin", "Model", json!({}));
        for j in 0..width {
            twin_add(
                &mut e,
                root,
                &format!("Leaf{j}"),
                if j + 1 == width {
                    "StringValue"
                } else {
                    "ObjectValue"
                },
                if j + 1 == width {
                    json!({"Value":label.to_string()})
                } else {
                    json!({"Value":reference})
                },
            );
        }
    }
    e
}
#[test]

fn review23_unbudgeted_candidates() {
    let n = std::env::var("REVIEW23_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(96);
    let width = std::env::var("REVIEW23_WIDTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(128);
    let e = broad_tree(n, width);
    let mut s = e.clone();
    for label in 0..n {
        s.instances[1 + (label + 1) * (width + 1)]
            .properties
            .insert("Value".into(), json!((n - 1 - label).to_string()));
    }
    namespace(&mut s);
    let start = Instant::now();
    let r = invariants(&e, &s);
    eprintln!(
        "BROAD roots={n} width={width} instances={} resolve_ms={:.3} budget={}",
        e.instances.len(),
        start.elapsed().as_secs_f64() * 1000.,
        r.budget_conflicts.len()
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    let start = Instant::now();
    let c = twin_align(&mut e.clone(), &mut s.clone());
    eprintln!(
        "BROAD_ALIGN ms={:.3} conflicts={}",
        start.elapsed().as_secs_f64() * 1000.,
        c.len()
    );
    assert!(
        c.is_empty() || (c.len() == 1 && c[0].contains("comparison budget")),
        "{c:?}"
    );
}

fn valid_references(d: &SettingsBytecode) {
    let ids = d
        .instances
        .iter()
        .map(|i| i.settings_id.as_str())
        .collect::<HashSet<_>>();
    for i in &d.instances {
        if let Some(id) = i
            .properties
            .get("Value")
            .and_then(|v| v.get("settingsId"))
            .and_then(Value::as_str)
        {
            assert!(ids.contains(id));
        }
    }
}

fn snapshot23(d: &SettingsBytecode) -> ProjectSnapshot {
    ProjectSnapshot {
        entries: [(
            PathBuf::from("instances/Workspace.renium"),
            SnapshotEntry::File(encode_settings_bytecode(d).unwrap()),
        )]
        .into_iter()
        .collect(),
    }
}

#[test]
fn review23_exhaustion_nested_merge_locks() {
    let n = 40;
    let order = (0..n).collect::<Vec<_>>();
    let mut extra = order.clone();
    extra.insert(1, n);
    let reverse = (0..n).rev().collect::<Vec<_>>();
    let mut e = mutual(&order, &order, n, true, false);
    let mut s = mutual(&extra, &reverse, n, true, false);
    // Identical persistent root identity; differently ordered descendant classes.
    uid(&mut e, 1, 123);
    uid(&mut s, 1, 123);
    for d in [&mut e, &mut s] {
        let roots = (1..d.instances.len())
            .filter(|&i| d.instances[i].class_name == "Model")
            .collect::<Vec<_>>();
        for root in roots {
            twin_add(
                d,
                root,
                "Shape",
                if root % 4 == 1 { "Part" } else { "Folder" },
                json!({}),
            );
        }
    }
    for (d, values) in [(&mut e, [1, 2]), (&mut s, [2, 1])] {
        for value in values {
            twin_add(
                d,
                0,
                "Independent",
                "StringValue",
                json!({"Value":value.to_string()}),
            );
        }
    }
    namespace(&mut s);
    let r = invariants(&e, &s);
    eprintln!(
        "NESTED exhausted={} proven={}",
        r.budget_conflicts.len(),
        r.proven.iter().filter(|&&v| v).count()
    );
    assert_eq!(r.budget_conflicts.len(), 1);
    assert_eq!(r.partners[1], Some(1));
    let last = s.instances.len() - 1;
    assert!(r.proven[last] && r.proven[last - 1]);
    assert!(!r.budget_sources[last] && !r.budget_sources[last - 1]);
    let (_, c) = merge_snapshots(
        None,
        &snapshot23(&e),
        &snapshot23(&s),
        ConflictPreference::None,
    )
    .unwrap();
    eprintln!("NESTED_CONFLICT {c:?}");
    assert_eq!(c.len(), 1);
    for pref in [ConflictPreference::Editor, ConflictPreference::Studio] {
        let (merged, c) = merge_snapshots(None, &snapshot23(&e), &snapshot23(&s), pref).unwrap();
        let d = settings_document(merged.entries.values().next()).unwrap();
        eprintln!(
            "NESTED_MERGE pref={pref:?} count={} conflicts={c:?}",
            d.instances.len()
        );
        assert!(c.is_empty(), "{c:?}");
        assert_twin_ids_unique(&d);
        valid_references(&d);
        assert_eq!(
            d.instances
                .iter()
                .filter(|i| i.properties.contains_key("UniqueId"))
                .count(),
            1
        );
        let mut independent = d
            .instances
            .iter()
            .filter(|i| i.name == "Independent")
            .map(|i| i.properties["Value"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        independent.sort();
        assert_eq!(independent, vec!["1", "2"]);
    }
}

#[test]
fn review23_nested_diagnostics() {
    let mut e = twin_document();
    let mut s = twin_document();
    for (d, classes) in [(&mut e, ["Part", "Folder"]), (&mut s, ["Model", "Model"])] {
        let mut parent = 0;
        for (name, class) in [
            ("Vehicles", "Folder"),
            ("DeliveryVan", "Model"),
            ("Chassis", "Model"),
            ("Suspension", "Folder"),
        ] {
            parent = twin_add(d, parent, name, class, json!({}));
        }
        for class in classes {
            twin_add(d, parent, "Mount", class, json!({}));
        }
    }
    namespace(&mut s);
    let c = twin_align(&mut e, &mut s);
    assert_eq!(c.len(), 2);
    assert_ne!(c[0], c[1]);
    for line in &c {
        eprintln!("DIAGNOSTIC chars={}\n  {line}", line.chars().count());
    }
    let status = crate::studio::automation::normalize_live_status(
        json!({"ok":true,"daemon":{"running":false,"error":"Reconciliation needs conflict resolution","conflicts":c}}),
    );
    crate::app::output::print_json_output(&status, true).unwrap();
}

fn ring_fixture(n: usize, nested: bool, ring: bool) -> (SettingsBytecode, SettingsBytecode) {
    let order = (0..n).collect::<Vec<_>>();
    let mut extra = order.clone();
    extra.insert(1, n);
    let reverse = (0..n).rev().collect::<Vec<_>>();
    let mut e = mutual(&order, &order, n, nested, false);
    let mut s = mutual(&extra, &reverse, n, nested, false);
    if ring {
        for (d, bs, acount) in [(&mut e, &order, n), (&mut s, &reverse, n + 1)] {
            for (bi, &label) in bs.iter().enumerate() {
                let alabel = (label + 1) % n;
                let apos = if acount > n && alabel >= 1 {
                    alabel + 1
                } else {
                    alabel
                };
                let step = if nested { 2 } else { 1 };
                let src = 1 + (acount + bi) * step + usize::from(nested);
                let dst = 1 + apos * step + usize::from(nested);
                let v = twin_ref(d, dst);
                d.instances[src].properties.insert("Value".into(), v);
            }
        }
    }

    (e, s)
}

#[test]
fn review23_small_rings() {
    for nested in [false, true] {
        for n in [10, 12, 20, 40] {
            for ring in [false, true] {
                let (mut e, mut s) = ring_fixture(n, nested, ring);
                namespace(&mut s);
                let start = Instant::now();
                let c = twin_align(&mut e, &mut s);
                eprintln!(
                    "RING nested={nested} n={n} ring={ring} ms={:.3} conflicts={c:?}",
                    start.elapsed().as_secs_f64() * 1000.
                );
                assert!(c.is_empty(), "{c:?}");
            }
        }
    }
}

#[test]
fn review23_ring_merge_keeps_addition() {
    let n = 20;
    let order = (0..n).collect::<Vec<_>>();
    let mut extra = order.clone();
    extra.insert(1, n);
    let reverse = (0..n).rev().collect::<Vec<_>>();
    let mut e = mutual(&order, &order, n, false, false);
    let mut s = mutual(&extra, &reverse, n, false, false);
    for (d, bs, acount) in [(&mut e, &order, n), (&mut s, &reverse, n + 1)] {
        for (bi, &label) in bs.iter().enumerate() {
            let alabel = (label + 1) % n;
            let apos = if acount > n && alabel >= 1 {
                alabel + 1
            } else {
                alabel
            };
            let v = twin_ref(d, 1 + apos);
            d.instances[1 + acount + bi]
                .properties
                .insert("Value".into(), v);
        }
    }
    namespace(&mut s);
    for preference in [
        ConflictPreference::None,
        ConflictPreference::Editor,
        ConflictPreference::Studio,
    ] {
        let (merged, c) =
            merge_snapshots(None, &snapshot23(&e), &snapshot23(&s), preference).unwrap();
        let d = settings_document(merged.entries.values().next()).unwrap();
        eprintln!(
            "RING_MERGE preference={preference:?} instances={} conflicts={c:?}",
            d.instances.len()
        );

        assert!(c.is_empty());
        assert_eq!(d.instances.len(), 42);
        assert_twin_ids_unique(&d);
        let ids = d
            .instances
            .iter()
            .map(|i| i.settings_id.as_str())
            .collect::<HashSet<_>>();
        for i in &d.instances {
            if let Some(id) = i
                .properties
                .get("Value")
                .and_then(|v| v.get("settingsId"))
                .and_then(Value::as_str)
            {
                assert!(ids.contains(id));
            }
        }
    }
}

#[test]
fn review23_budget_cost() {
    let e = broad_tree(96, 128);
    let mut s = e.clone();
    for label in 0..96 {
        s.instances[1 + (label + 1) * 129]
            .properties
            .insert("Value".into(), json!((95 - label).to_string()));
    }
    namespace(&mut s);
    let started = Instant::now();
    let r = invariants(&e, &s);
    eprintln!(
        "COST records={} unbudgeted={} budget={} ms={:.3}",
        r.record_comparisons,
        r.unbudgeted_comparisons,
        r.budget_conflicts.len(),
        started.elapsed().as_secs_f64() * 1000.
    );
    assert_eq!(r.unbudgeted_comparisons, 0);
    assert!(r.record_comparisons < 65_536);
    assert_eq!(r.budget_conflicts.len(), 1);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(!r.proven.iter().any(|&v| v));
    assert!(!r.rejected.iter().any(|&v| v));
}

#[test]
fn review23_tolerant_field_proposals_require_complete_proofs() {
    let mut e = twin_document();
    for index in 0..64 {
        let x = index as f64 + 0.12341;
        twin_add(
            &mut e,
            0,
            "Twin",
            "Part",
            json!({"CFrame":{"_type":"CFrame","components":[x,0,0,1,0,0,0,1,0,0,0,1]}}),
        );
    }
    let mut s = twin_observation(&e);
    for instance in &mut s.instances[1..] {
        let x = instance.properties["CFrame"]["components"][0]
            .as_f64()
            .unwrap();
        instance.properties["CFrame"]["components"][0] = json!(x + 0.00005);
    }
    let r = invariants(&e, &s);
    assert!(r.budget_conflicts.is_empty());
    assert_eq!(
        r.partners[1..],
        (1..=64).rev().map(Some).collect::<Vec<_>>()
    );
    assert!(twin_align(&mut e.clone(), &mut s.clone()).is_empty());
    let x = s.instances[1].properties["CFrame"]["components"][0]
        .as_f64()
        .unwrap();
    s.instances[1].properties["CFrame"]["components"][0] = json!(x + 0.01);
    assert!(
        !twin_align(&mut e, &mut s).is_empty(),
        "a coarse key is never equality evidence"
    );
}

#[test]
#[ignore = "measured budget boundary"]
fn review23_budget_boundary() {
    for nested in [false, true] {
        for n in [40, 48, 56, 64, 80, 96, 128] {
            let (mut e, mut s) = ring_fixture(n, nested, true);
            namespace(&mut s);
            let started = Instant::now();
            let c = twin_align(&mut e, &mut s);
            assert!(
                c.is_empty() || (c.len() == 1 && c[0].contains("comparison budget")),
                "{c:?}"
            );
            eprintln!(
                "BOUNDARY nested={nested} pairs={n} budget={} ms={:.3}",
                !c.is_empty(),
                started.elapsed().as_secs_f64() * 1000.
            );
        }
    }
}
