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

fn permutations(xs: &[usize]) -> Vec<Vec<usize>> {
    if xs.is_empty() {
        return vec![vec![]];
    }
    let mut out = vec![];
    for (i, &x) in xs.iter().enumerate() {
        let rest = xs
            .iter()
            .enumerate()
            .filter_map(|(j, &v)| (i != j).then_some(v))
            .collect::<Vec<_>>();
        for mut p in permutations(&rest) {
            p.insert(0, x);
            out.push(p);
        }
    }
    out
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
fn twins20_both_sides_additions_all_orders_and_locked_moves() {
    let mut cases = 0;
    for ea in permutations(&[0, 1, 2]) {
        for eb in permutations(&[0, 1]) {
            for sa in permutations(&[0, 1]) {
                for sb in permutations(&[0, 1, 2]) {
                    for nested in [false, true] {
                        for edit in 0..3 {
                            let mut e = mutual(&ea, &eb, 2, nested, false);
                            let mut s = mutual(&sa, &sb, 2, nested, false);
                            let mut locked = 0;
                            for d in [&mut e, &mut s] {
                                let p = twin_add(d, 0, "Container", "Folder", json!({}));
                                locked = twin_add(d, p, "Locked", "Part", json!({}));
                                uid(d, locked, 99);
                            }
                            if edit == 1 {
                                s.instances[locked].name = "Renamed".into();
                            }
                            if edit == 2 {
                                s.instances[locked].parent_index = Some(0);
                            }
                            namespace(&mut s);
                            invariants(&e, &s);
                            let c = twin_align(&mut e, &mut s);
                            assert_twin_ids_unique(&s);
                            if edit == 0 {
                                assert!(
                                    c.is_empty(),
                                    "ea={ea:?} eb={eb:?} sa={sa:?} sb={sb:?} nested={nested}: {c:?}"
                                );
                            } else {
                                assert!(
                                    c.iter().any(|v| v.contains("same instance identity")),
                                    "{c:?}"
                                );
                            }
                            cases += 1;
                        }
                    }
                }
            }
        }
    }
    eprintln!("BOTH_ADDITIONS cases={cases}");
}

fn stress(n: usize, satisfiable: bool) {
    let mut a = (0..n).collect::<Vec<_>>();
    let e = mutual(&a, &a, n, false, false);
    let b = (0..n).rev().collect::<Vec<_>>();
    if satisfiable {
        a.insert(1, n);
    }
    let mut s = mutual(&a, &b, n, false, !satisfiable);
    namespace(&mut s);
    let start = Instant::now();
    let r = invariants(&e, &s);
    let resolve_time = start.elapsed();
    let start = Instant::now();
    let conflicts = twin_align(&mut e.clone(), &mut s.clone());
    let align_time = start.elapsed();
    eprintln!(
        "STRESS n={n} sat={satisfiable} resolve_ms={:.3} align_ms={:.3} budget={} conflicts={}",
        resolve_time.as_secs_f64() * 1000.,
        align_time.as_secs_f64() * 1000.,
        r.budget_conflicts.len(),
        conflicts.len()
    );
    // Far below the old 17-40 seconds at six pairs; allow slow/contended CI hosts.
    assert!(resolve_time < Duration::from_secs(3), "{resolve_time:?}");
    assert!(align_time < Duration::from_secs(3), "{align_time:?}");
    if !r.budget_conflicts.is_empty() {
        assert!(n >= 511, "small graphs must be decided, not budget-limited");
        assert_eq!(r.budget_conflicts.len(), 1);
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert!(conflicts[0].contains(
            "within the comparison budget; reorder or rename them, or resolve with --prefer"
        ));
        assert!(!r.proven.iter().any(|&v| v));
        assert!(!r.rejected.iter().any(|&v| v));
    } else if satisfiable {
        assert!(r.proven.iter().filter(|&&v| v).count() >= 2 * n);
        reference_oracle(&e, &s, &r);
        assert!(conflicts.is_empty(), "{conflicts:?}");
    } else {
        assert!(!r.proven.iter().any(|&v| v));
        assert!(!conflicts.is_empty());
    }
}

// Independent oracle: interchangeability must prove the actual referenced
// record's outgoing edge when the mapped target identity differs.
fn reference_oracle(e: &SettingsBytecode, s: &SettingsBytecode, r: &pairing::twins::Resolved) {
    let target_of = |d: &SettingsBytecode, i: usize| {
        d.instances
            .iter()
            .position(|v| {
                v.settings_id
                    == d.instances[i].properties["Value"]["settingsId"]
                        .as_str()
                        .unwrap()
            })
            .unwrap()
    };
    for (source, &target) in r.partners.iter().enumerate().skip(1) {
        if let Some(target) = target {
            let sr = target_of(s, source);
            let er = target_of(e, target);
            let mapped = r.partners[sr].expect("referenced target must be assigned");
            if mapped != er {
                assert_eq!(s.instances[sr].name, e.instances[er].name);
                assert_eq!(r.partners[target_of(s, sr)], Some(target_of(e, er)));
            }
        }
    }
}

#[test]
fn twins20_search_stress_3_unsatisfiable() {
    stress(3, false);
}
#[test]
fn twins20_search_stress_4_unsatisfiable() {
    stress(4, false);
}
#[test]
fn twins20_search_stress_5_unsatisfiable() {
    stress(5, false);
}
#[test]
fn twins20_search_stress_6_unsatisfiable() {
    stress(6, false);
}
#[test]
fn twins20_search_stress_9_satisfiable() {
    stress(9, true);
}
#[test]
fn twins20_search_stress_511_satisfiable_boundary() {
    stress(511, true);
}
#[test]
fn twins20_search_stress_512_unsatisfiable_boundary() {
    stress(512, false);
}

#[test]
fn twins20_class_diagnostics_distinguish_residuals() {
    for (left, right) in [
        (vec!["Part", "Folder"], vec!["Model", "Model"]),
        (
            vec!["Part", "Part", "Folder", "Folder"],
            vec!["Part", "Folder", "Model", "Model"],
        ),
    ] {
        for reverse in [false, true] {
            let mut e = twin_document();
            let mut s = twin_document();
            for (document, classes) in [(&mut e, &left), (&mut s, &right)] {
                for class in classes {
                    twin_add(document, 0, "Child", class, json!({}));
                }
            }
            if reverse {
                std::mem::swap(&mut e, &mut s);
            }
            namespace(&mut s);
            let conflicts = twin_align(&mut e, &mut s);
            assert_eq!(conflicts.len(), 2, "{conflicts:?}");
            assert_ne!(conflicts[0], conflicts[1]);
            assert!(conflicts.iter().all(|v| v.contains("different classes")));
        }
    }
}

fn next(seed: &mut u64, n: usize) -> usize {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    (*seed as usize) % n
}

fn shuffled(d: &SettingsBytecode, seed: &mut u64) -> SettingsBytecode {
    let mut children = vec![vec![]; d.instances.len()];
    for (i, v) in d.instances.iter().enumerate().skip(1) {
        children[v.parent_index.unwrap()].push(i);
    }
    for xs in &mut children {
        for i in (1..xs.len()).rev() {
            let j = next(seed, i + 1);
            xs.swap(i, j);
        }
    }
    let mut pending = vec![0];
    let mut order = vec![];
    while let Some(i) = pending.pop() {
        order.push(i);
        pending.extend(&children[i]);
    }
    let mut pos = vec![0; order.len()];
    for (j, &i) in order.iter().enumerate() {
        pos[i] = j;
    }
    let instances = order
        .iter()
        .map(|&i| {
            let mut v = d.instances[i].clone();
            v.parent_index = v.parent_index.map(|p| pos[p]);
            v
        })
        .collect();
    SettingsBytecode {
        version: d.version,
        instances,
    }
}

// Three duplicate units, descendant cycles, and references through a non-unit root.
fn generated(seed: u64) -> (SettingsBytecode, SettingsBytecode) {
    let mut state = seed + 123;
    let mut e = twin_document();
    let width = 2 + next(&mut state, 2);
    let mut anchors = vec![];
    for j in 0..width {
        anchors.push(twin_add(
            &mut e,
            0,
            &format!("Anchor{j}"),
            "Part",
            json!({}),
        ));
    }
    let mut nodes = vec![vec![]; 3];
    for (g, row) in nodes.iter_mut().enumerate() {
        for _ in 0..width {
            let r = twin_add(&mut e, 0, &format!("Unit{g}"), "Model", json!({}));
            let mid = if seed.is_multiple_of(2) {
                twin_add(&mut e, r, "Inner", "Folder", json!({}))
            } else {
                r
            };
            row.push(twin_add(&mut e, mid, "Pointer", "ObjectValue", json!({})));
        }
    }
    for g in 0..3 {
        for j in 0..width {
            let t = if g == 2 && !seed.is_multiple_of(3) {
                anchors[j]
            } else {
                nodes[(g + 1) % 3][(j + 1) % width]
            };
            let v = twin_ref(&e, t);
            e.instances[nodes[g][j]]
                .properties
                .insert("Value".into(), v);
        }
    }
    // Equal first two roots, third is changed after cloning so this group has no unit.
    let mut envelopes = vec![];
    for j in 0..3 {
        let t = twin_add(&mut e, 0, "Envelope", "Model", json!({}));
        envelopes.push(t);
        let v = twin_ref(&e, nodes[0][0]);
        twin_add(&mut e, t, "Pointer", "ObjectValue", json!({"Value":v}));
        if j == 2 {
            e.instances[t]
                .attributes
                .insert("Edited".into(), json!(false));
        }
    }
    for _ in 0..2 {
        let r = twin_ref(&e, envelopes[0]);
        twin_add(&mut e, 0, "Reader", "ObjectValue", json!({"Value":r}));
    }
    let mut s = e.clone();
    s.instances[envelopes[2]]
        .attributes
        .insert("Edited".into(), json!(true));
    for i in s.instances.len() - 2..s.instances.len() {
        let v = twin_ref(&s, envelopes[1]);
        s.instances[i].properties.insert("Value".into(), v);
    }
    // Preserve the non-unit root order; only the units and their descendants shuffle.
    e = shuffled(&e, &mut state);
    s = shuffled(&s, &mut state);
    namespace(&mut s);
    (e, s)
}

#[test]
fn twins20_generated_indirect_chains_and_descendant_cycles() {
    pairing::twins::take_recoveries();
    let mut hits = vec![];
    for seed in 0..6000 {
        let (mut e, mut s) = generated(seed);
        let r = pairing::twins::resolve(&e, &s);
        let c = twin_align(&mut e, &mut s);
        assert_twin_ids_unique(&s);
        let n = pairing::twins::take_recoveries();
        if n > 0 || r.recovered {
            hits.push((seed, n, c));
            if hits.len() > 10 {
                break;
            }
        }
    }
    eprintln!("GENERATED cases=6000 hits={hits:?}");
    assert!(hits.is_empty());
}

#[test]
fn twins20_three_class_residual_counts_and_locks() {
    let classes = ["Part", "Folder", "Model"];
    let mut duplicate_examples = vec![];
    let mut cases = 0;
    for locks in [false, true] {
        for a in 0..27 {
            for b in 0..27 {
                let counts = |mut code: usize| {
                    let mut ns = [0; 3];
                    for v in &mut ns {
                        *v = code % 3;
                        code /= 3;
                    }
                    ns
                };
                let left = counts(a);
                let right = counts(b);
                let mut e = twin_document();
                let mut s = twin_document();
                for (d, ns) in [(&mut e, left), (&mut s, right)] {
                    for (k, &class) in classes.iter().enumerate() {
                        if locks {
                            let i = twin_add(d, 0, "Child", class, json!({}));
                            uid(d, i, k + 1);
                        }
                        for _ in 0..ns[k] {
                            twin_add(d, 0, "Child", class, json!({}));
                        }
                    }
                }
                namespace(&mut s);
                invariants(&e, &s);
                let c = twin_align(&mut e, &mut s);
                assert_twin_ids_unique(&s);
                let expected = left
                    .iter()
                    .zip(right)
                    .map(|(&l, r)| l.saturating_sub(r))
                    .sum::<usize>()
                    .min(
                        right
                            .iter()
                            .zip(left)
                            .map(|(&r, l)| r.saturating_sub(l))
                            .sum(),
                    );
                assert_eq!(c.len(), expected, "{left:?}/{right:?} locks={locks}: {c:?}");
                assert!(c.iter().all(|v| v.contains("different classes")), "{c:?}");
                if c.iter().collect::<HashSet<_>>().len() != c.len() && duplicate_examples.len() < 4
                {
                    duplicate_examples.push((left, right, locks, c));
                }
                cases += 1;
            }
        }
    }
    eprintln!("CLASS cases={cases} duplicate_examples={duplicate_examples:?}");
    assert!(
        duplicate_examples.is_empty(),
        "class changes must have distinct diagnostics"
    );
}

#[test]
fn twins20_changed_locked_records_never_reuse_identity_as_content() {
    for mode in 0..6 {
        let mut e = twin_document();
        let x = twin_add(&mut e, 0, "X", "Part", json!({}));
        let y = twin_add(&mut e, 0, "Y", "Part", json!({}));
        let r = twin_ref(&e, x);
        let i = twin_add(&mut e, 0, "Twin", "ObjectValue", json!({"Value":r}));
        uid(&mut e, i, 7);
        let r = twin_ref(&e, x);
        twin_add(&mut e, 0, "Twin", "ObjectValue", json!({"Value":r}));
        let mut s = e.clone();
        match mode {
            0 => {
                s.instances[i]
                    .attributes
                    .insert("Changed".into(), json!(true));
            }
            1 => {
                let v = twin_ref(&s, y);
                s.instances[i].properties.insert("Value".into(), v);
            }
            2 => {
                s.instances[i]
                    .properties
                    .insert("Archivable".into(), json!(false));
            }
            3 => {
                s.instances[i].name = "Renamed".into();
            }
            4 => {
                s.instances[i].parent_index = Some(x);
            }
            _ => {
                s.instances[i]
                    .attributes
                    .insert("Nested".into(), json!({"x":[1,2,3]}));
            }
        }
        namespace(&mut s);
        let r = invariants(&e, &s);
        if mode != 4 {
            assert!(!r.unchanged[i], "mode={mode}");
        }
        let c = twin_align(&mut e, &mut s);
        assert!(!c.is_empty(), "mode={mode}");
        if (3..=4).contains(&mode) {
            assert!(
                c.iter().any(|v| v.contains("same instance identity")),
                "{c:?}"
            );
        }
    }
}

#[test]
fn twins20_budget_preference_and_unrelated_components() {
    let order = (0..512).collect::<Vec<_>>();
    let reverse = (0..512).rev().collect::<Vec<_>>();
    let mut editor = mutual(&order, &order, 512, false, false);
    let mut studio = mutual(&order, &reverse, 512, false, true);
    for (d, values) in [(&mut editor, [1, 2]), (&mut studio, [2, 1])] {
        for value in values {
            twin_add(d, 0, "Independent", "Part", json!({"Value": value}));
        }
    }
    namespace(&mut studio);
    let resolved = invariants(&editor, &studio);
    let last = editor.instances.len() - 1;
    assert_eq!(resolved.partners[last], Some(last - 1));
    assert_eq!(resolved.partners[last - 1], Some(last));
    assert!(resolved.proven[last] && resolved.proven[last - 1]);
    assert!(!resolved.budget_sources[last] && !resolved.budget_sources[last - 1]);
    let conflicts = twin_align(&mut editor.clone(), &mut studio.clone());
    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    assert!(conflicts[0].contains("could not pair 1024 same-named siblings under Workspace"));
    for preference in [ConflictPreference::Editor, ConflictPreference::Studio] {
        let mut e = editor.clone();
        let mut s = studio.clone();
        let mut conflicts = Vec::new();
        pairing::align_first_pairing(
            Path::new("instances/Workspace.renium"),
            &mut e,
            &mut s,
            preference,
            &mut conflicts,
        );
        assert!(conflicts.is_empty(), "{conflicts:?}");
        for (source, target) in resolved.partners.iter().enumerate() {
            if let Some(target) = target {
                assert!(settings_instances_equal(&e, *target, &s, source));
            }
        }
    }
    for d in [&mut editor, &mut studio] {
        let locked = twin_add(d, 0, "Locked", "Part", json!({}));
        uid(d, locked, 123);
    }
    studio.instances.last_mut().unwrap().name = "Renamed".into();
    let conflicts = twin_align(&mut editor, &mut studio);
    assert_eq!(conflicts.len(), 3, "{conflicts:?}");
    assert!(
        conflicts
            .iter()
            .any(|v| v.contains("same instance identity"))
    );
}
