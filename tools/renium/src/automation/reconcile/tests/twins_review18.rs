use super::*;

fn namespace(document: &mut SettingsBytecode) {
    let ids = document
        .instances
        .iter()
        .map(|i| (i.settings_id.clone(), format!("studio-{}", i.settings_id)))
        .collect::<HashMap<_, _>>();
    for i in &mut document.instances {
        i.settings_id = ids[&i.settings_id].clone();
        remap_record_reference_ids(&mut i.properties, &ids);
        remap_record_reference_ids(&mut i.attributes, &ids);
    }
}

fn reference_subset(targets: &[usize], depth: usize) -> SettingsBytecode {
    let mut d = twin_document();
    for name in ["A", "B", "C", "D"] {
        twin_add(&mut d, 0, name, "Part", json!({}));
    }
    for &target in targets {
        let reference = twin_ref(&d, target);
        let mut parent = twin_add(&mut d, 0, "Twin", "ObjectValue", json!({"Value":reference}));
        for _ in 0..depth {
            parent = twin_add(&mut d, parent, "Child", "Folder", json!({}));
        }
    }
    d
}

fn check_resolution(e: &SettingsBytecode, s: &SettingsBytecode) -> pairing::twins::Resolved {
    let r = pairing::twins::resolve(e, s);
    assert_eq!(
        r.partners.iter().flatten().count(),
        r.partners.iter().flatten().collect::<HashSet<_>>().len()
    );
    for (source, target) in r.partners.iter().enumerate() {
        if let Some(target) = target {
            assert_eq!(
                s.instances[source].class_name,
                e.instances[*target].class_name
            );
        }
        if r.locked[source].is_some() {
            assert_eq!(*target, r.locked[source]);
        }
        if target.is_some() && r.locked[source].is_none() {
            assert_eq!(
                s.instances[source].parent_index.and_then(|p| r.partners[p]),
                e.instances[target.unwrap()].parent_index,
                "orphan at {source}"
            );
        }
    }
    // Independently check reusable record evidence with the full final comparator.
    let mut remapped = s.clone();
    for (source, target) in r.partners.iter().enumerate() {
        if let Some(target) = target {
            remapped.instances[source]
                .settings_id
                .clone_from(&e.instances[*target].settings_id);
        }
    }
    r.align_references(&mut remapped);
    for (source, target) in r.partners.iter().enumerate() {
        if let Some(target) = target
            && r.unchanged[source]
            && settings_parent_id(e, *target) == settings_parent_id(&remapped, source)
        {
            assert!(
                settings_instances_equal(e, *target, &remapped, source),
                "cached record {source}->{target}"
            );
        }
    }
    r
}

#[test]
fn three_versus_four_reference_permutations() {
    let mut count = 0;
    for mut code in 0..2187 {
        let [a, b, c, d, e, f, g] = std::array::from_fn(|_| {
            let n = code % 3 + 1;
            code /= 3;
            n
        });
        let small = [a, b, c];
        let large = [d, e, f, g];
        for reverse in [false, true] {
            let (left, right) = if reverse {
                (&large[..], &small[..])
            } else {
                (&small[..], &large[..])
            };
            let mut editor = reference_subset(left, 0);
            let mut studio = reference_subset(right, 0);
            namespace(&mut studio);
            let r = check_resolution(&editor, &studio);
            let can_pair = (1..=3).all(|v| {
                small.iter().filter(|&&x| x == v).count()
                    <= large.iter().filter(|&&x| x == v).count()
            });
            assert_eq!(
                r.proven[5..].iter().filter(|&&p| p).count(),
                if can_pair { 3 } else { 0 },
                "{left:?} vs {right:?}"
            );
            for source in 5..r.partners.len() {
                if r.proven[source] {
                    assert_eq!(right[source - 5], left[r.partners[source].unwrap() - 5]);
                }
            }
            if can_pair {
                assert!(
                    twin_align(&mut editor, &mut studio).is_empty(),
                    "{left:?} vs {right:?}"
                );
            }
            count += 1;
        }
    }
    std::eprintln!("three-versus-four: {count} cases");
}

fn permutations(values: &[usize]) -> Vec<Vec<usize>> {
    if values.is_empty() {
        return vec![vec![]];
    }
    let mut out = Vec::new();
    for (i, &head) in values.iter().enumerate() {
        let tail = values
            .iter()
            .enumerate()
            .filter_map(|(j, &v)| (i != j).then_some(v))
            .collect::<Vec<_>>();
        for mut rest in permutations(&tail) {
            rest.insert(0, head);
            out.push(rest);
        }
    }
    out
}

fn cyclic(a_order: &[usize], b_order: &[usize], marked: bool, nested: bool) -> SettingsBytecode {
    let mut d = twin_document();
    let mut aa = HashMap::new();
    let mut bb = HashMap::new();
    for (name, order, map) in [("A", a_order, &mut aa), ("B", b_order, &mut bb)] {
        for &label in order {
            let root = twin_add(
                &mut d,
                0,
                name,
                if nested { "Model" } else { "ObjectValue" },
                json!({}),
            );
            let node = if nested {
                twin_add(&mut d, root, "Pointer", "ObjectValue", json!({}))
            } else {
                root
            };
            if marked && name == "B" {
                d.instances[node]
                    .attributes
                    .insert("Marker".into(), json!(label));
            }
            map.insert(label, node);
        }
    }
    for (&label, &node) in &aa {
        let v = twin_ref(&d, bb[&(label % 2)]);
        d.instances[node].properties.insert("Value".into(), v);
    }
    for (&label, &node) in &bb {
        let v = twin_ref(&d, aa[&(label % 2)]);
        d.instances[node].properties.insert("Value".into(), v);
    }
    d
}

#[test]
fn mutual_reference_groups_with_one_unequal_size() {
    let mut cases = 0;
    let mut failures = Vec::new();
    let mut failure_count = 0;
    for ea in permutations(&[0, 1]) {
        for sa in permutations(&[0, 1, 2]) {
            for eb in permutations(&[0, 1]) {
                for sb in permutations(&[0, 1]) {
                    for marked in [false, true] {
                        for nested in [false, true] {
                            for reverse in [false, true] {
                                let a = cyclic(&ea, &eb, marked, nested);
                                let b = cyclic(&sa, &sb, marked, nested);
                                let (mut editor, mut studio) =
                                    if reverse { (b, a) } else { (a, b) };
                                namespace(&mut studio);
                                check_resolution(&editor, &studio);
                                let conflicts = twin_align(&mut editor, &mut studio);
                                assert_twin_ids_unique(&studio);
                                if !conflicts.is_empty() {
                                    failure_count += 1;
                                }
                                if !conflicts.is_empty() && failures.len() < 8 {
                                    failures.push(format!("ea={ea:?} sa={sa:?} eb={eb:?} sb={sb:?} marked={marked} nested={nested} reverse={reverse}: {conflicts:?}"));
                                }
                                cases += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    std::eprintln!(
        "mutual cycles: {cases} cases; failure_count={failure_count}; first failures={failures:?}"
    );
    assert!(failures.is_empty());
}

#[test]
fn abandoned_subtrees_have_no_stale_descendants_or_keys() {
    for depth in [1, 2, 4] {
        for order in [[3, 1, 2], [2, 3, 1], [2, 2, 1]] {
            let mut e = reference_subset(&[1, 2], depth);
            let mut s = reference_subset(&order, depth);
            namespace(&mut s);
            let r = check_resolution(&e, &s);
            for (source, i) in s.instances.iter().enumerate() {
                if i.parent_index.is_some_and(|p| r.partners[p].is_none()) {
                    assert!(r.partners[source].is_none(), "stale descendant {source}");
                }
            }
            let mut interner = pairing::PathInterner::new();
            let ek = pairing::structural_path_ids(&e, &mut interner);
            let sk = pairing::structural_path_ids(&s, &mut interner);
            let keys = pairing::resolved_path_ids(
                &s,
                &sk,
                &ek,
                &r.partners,
                &mut interner,
                |part, ordinal| part.ordinal = ordinal,
            );
            for (source, i) in s.instances.iter().enumerate().skip(5) {
                if r.partners[source].is_none() {
                    assert!(!ek.contains(&keys[source]));
                    assert_eq!(
                        interner.node(keys[source]).parent,
                        i.parent_index.map(|p| keys[p])
                    );
                    std::eprintln!(
                        "depth={depth} source={source} key={}",
                        pairing::render_structural_key(&interner, keys[source])
                    );
                }
            }
            assert!(twin_align(&mut e, &mut s).is_empty());
            assert_twin_ids_unique(&s);
        }
    }
}

fn snapshot(d: &SettingsBytecode) -> ProjectSnapshot {
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
fn baseline_deletions_both_sides_and_reference_targets() {
    for files in [false, true] {
        for target in [false, true] {
            let original = reference_subset(&[3, 1, 2], 0);
            let base = snapshot(&original);
            let mut changed = original.clone();
            let removed = changed
                .instances
                .remove(if target { 3 } else { 5 })
                .settings_id;
            if target {
                for i in &mut changed.instances {
                    if i.properties
                        .get("Value")
                        .is_some_and(|v| v.get("settingsId") == Some(&json!(removed)))
                    {
                        i.properties.insert("Value".into(), json!({"_type":"Ref"}));
                    }
                }
            }
            let (e, s) = if files {
                (&changed, &original)
            } else {
                (&original, &changed)
            };
            let (merged, conflicts, changes) = merge::merge_snapshots_with_changes(
                Some(&base),
                &snapshot(e),
                &snapshot(s),
                ConflictPreference::None,
                None,
            )
            .unwrap();
            assert!(
                conflicts.is_empty(),
                "files={files} target={target}: {conflicts:?}"
            );
            let result = settings_document(merged.entries.values().next()).unwrap();
            assert_eq!(result.instances.len(), original.instances.len() - 1);
            assert!(result.instances.iter().all(|i| i.settings_id != removed));
            assert!(settings_documents_equivalent(&result, &changed));
            assert_eq!(!changes.studio.is_empty(), files);
            assert_eq!(!changes.editor.is_empty(), !files);
        }
    }
}

#[test]
#[ignore = "private stores, read-only performance probe"]
fn repeated_real_comparison() {
    let read =
        |name| SettingsBytecode::read_file(Path::new(&std::env::var_os(name).unwrap())).unwrap();
    let e = read("RENIUM_REPRO_EDITOR");
    let s = read("RENIUM_REPRO_STUDIO");
    for run in 0..9 {
        let mut e = e.clone();
        let mut s = s.clone();
        let now = std::time::Instant::now();
        let conflicts = twin_align(&mut e, &mut s);
        std::eprintln!(
            "BENCH run={run} align_ms={:.3} conflicts={}",
            now.elapsed().as_secs_f64() * 1000.0,
            conflicts.len()
        );
        assert!(conflicts.is_empty(), "{conflicts:?}");
    }
}

#[test]
fn minimal_mutual_subset() {
    let mut e = cyclic(&[0, 1], &[0, 1], false, false);
    let mut s = cyclic(&[0, 2, 1], &[0, 1], false, false);
    namespace(&mut s);
    let r = pairing::twins::resolve(&e, &s);
    std::eprintln!("raw partners={:?} proven={:?}", r.partners, r.proven);
    align_settings_ids_to_reference(&e, &mut s);
    std::eprintln!(
        "aligned ids={:?}",
        s.instances
            .iter()
            .map(|i| &i.settings_id)
            .collect::<Vec<_>>()
    );
    let r = pairing::twins::resolve(&e, &s);
    std::eprintln!(
        "after prealign partners={:?} proven={:?}",
        r.partners,
        r.proven
    );
    let conflicts = twin_align(&mut e, &mut s);
    std::eprintln!("conflicts={conflicts:?}");
    assert!(conflicts.is_empty());
}

#[test]
fn mutual_subset_swapped_b() {
    let mut e = cyclic(&[0, 1], &[0, 1], false, false);
    let mut s = cyclic(&[0, 2, 1], &[1, 0], false, false);
    namespace(&mut s);
    let correct = [Some(0), Some(1), None, Some(2), Some(4), Some(3)];
    for (source, target) in correct.iter().enumerate().skip(1) {
        if let Some(target) = target {
            let ev = &e.instances[*target].properties["Value"]["settingsId"];
            let sv = &s.instances[source].properties["Value"]["settingsId"];
            let et = e
                .instances
                .iter()
                .position(|i| json!(i.settings_id) == *ev)
                .unwrap();
            let st = s
                .instances
                .iter()
                .position(|i| json!(i.settings_id) == *sv)
                .unwrap();
            assert_eq!(correct[st], Some(et), "an exact joint assignment exists");
        }
    }
    let r = pairing::twins::resolve(&e, &s);
    std::eprintln!(
        "swapped B raw partners={:?} proven={:?}",
        r.partners,
        r.proven
    );
    align_settings_ids_to_reference(&e, &mut s);
    let r = pairing::twins::resolve(&e, &s);
    std::eprintln!(
        "swapped B aligned partners={:?} proven={:?}",
        r.partners,
        r.proven
    );
    let conflicts = twin_align(&mut e, &mut s);
    std::eprintln!("swapped B conflicts={conflicts:?}");
    assert!(conflicts.is_empty());
}

fn indirect_dependency(studio: bool) -> SettingsBytecode {
    let mut d = twin_document();
    let x = twin_add(&mut d, 0, "X", "Part", json!({}));
    let y = twin_add(&mut d, 0, "Y", "Part", json!({}));
    let mut targets = Vec::new();
    for i in 0..3 {
        let t = twin_add(&mut d, 0, "Target", "Model", json!({}));
        targets.push(t);
        if i == 2 {
            d.instances[t]
                .attributes
                .insert("Edit".into(), json!(studio));
        }
        twin_add(&mut d, t, "Pointer", "ObjectValue", json!({}));
    }
    let mut uu = Vec::new();
    for v in if studio { [y, x] } else { [x, y] } {
        let r = twin_ref(&d, v);
        uu.push(twin_add(&mut d, 0, "U", "ObjectValue", json!({"Value":r})));
    }
    for &t in &targets {
        let r = twin_ref(&d, uu[0]);
        d.instances[t + 1].properties.insert("Value".into(), r);
    }
    for _ in 0..2 {
        let r = twin_ref(&d, targets[if studio { 1 } else { 0 }]);
        twin_add(&mut d, 0, "R", "ObjectValue", json!({"Value":r}));
    }
    if studio {
        namespace(&mut d);
    }
    d
}

#[test]
fn indirect_reference_dependency_reaches_recovery() {
    pairing::twins::take_recoveries();
    let mut e = indirect_dependency(false);
    let mut s = indirect_dependency(true);
    let r = check_resolution(&e, &s);
    let count = pairing::twins::take_recoveries();
    std::eprintln!(
        "indirect RECOVERY hits={count} partners={:?} proven={:?}",
        r.partners,
        r.proven
    );
    let conflicts = twin_align(&mut e, &mut s);
    assert_twin_ids_unique(&s);
    assert_eq!(pairing::twins::take_recoveries(), 0, "{conflicts:?}");
    assert_eq!(count, 0);
    assert_eq!(&r.partners[9..11], &[Some(10), Some(9)]);
    assert!(!pairing::twins::resolve(&e, &s).recovered);
    assert!(
        conflicts
            .iter()
            .all(|conflict| !conflict.contains("Workspace.U")),
        "{conflicts:?}"
    );
    assert!(
        conflicts
            .iter()
            .any(|conflict| conflict.contains("Workspace.Target"))
    );
}

#[test]
fn recovery_preserves_locks_keys_and_class_diagnostics() {
    for mixed in [false, true] {
        let mut e = indirect_dependency(false);
        let mut s = indirect_dependency(true);
        for d in [&mut e, &mut s] {
            let moved = twin_add(
                d,
                0,
                "Locked",
                "Folder",
                json!({"UniqueId":{"_type":"UniqueId","value":"000000000000000000000000000000ff"}}),
            );
            twin_add(d, moved, "Child", "Folder", json!({}));
        }
        let moved = s.instances.len() - 2;
        s.instances[moved].name = "Renamed".into();
        let parent = twin_add(&mut e, 0, "Classes", "Model", json!({}));
        let sp = twin_add(&mut s, 0, "Classes", "Model", json!({}));
        twin_add(&mut e, parent, "Child", "Part", json!({}));
        twin_add(&mut s, sp, "Child", "Folder", json!({}));
        if mixed {
            twin_add(&mut e, parent, "Child", "Folder", json!({}));
            twin_add(&mut s, sp, "Child", "Folder", json!({}));
        }
        twin_add(&mut e, 0, "EditorOnly", "Folder", json!({}));
        twin_add(&mut s, 0, "StudioOnly", "Folder", json!({}));
        namespace(&mut s);
        let r = check_resolution(&e, &s);
        assert!(!r.recovered);
        assert_eq!(r.locked[moved], r.partners[moved]);
        let conflicts = twin_align(&mut e, &mut s);
        assert_twin_ids_unique(&s);
        std::eprintln!("recovery safety mixed={mixed} conflicts={conflicts:?}");
        assert!(
            conflicts
                .iter()
                .any(|v| v.contains("same instance identity"))
        );
        assert!(
            conflicts.iter().any(|v| v.contains("different classes")
                && v.contains("Classes<Model>[1].Child<Part>[1]")),
            "class mismatch must survive recovery: mixed={mixed}"
        );
    }
}

#[test]
fn matchable_three_four_subsets_through_complete_alignment() {
    let mut count = 0;
    let mut failed = 0;
    let mut first = None;
    for mut code in 0..2187 {
        let [a, b, c, d, e, f, g] = std::array::from_fn(|_| {
            let n = code % 3 + 1;
            code /= 3;
            n
        });
        let small = [a, b, c];
        let large = [d, e, f, g];
        if !(1..=3).all(|v| {
            small.iter().filter(|&&x| x == v).count() <= large.iter().filter(|&&x| x == v).count()
        }) {
            continue;
        }
        for reverse in [false, true] {
            let (left, right) = if reverse {
                (&large[..], &small[..])
            } else {
                (&small[..], &large[..])
            };
            let mut e = reference_subset(left, 0);
            let mut s = reference_subset(right, 0);
            namespace(&mut s);
            let conflicts = twin_align(&mut e, &mut s);
            assert_twin_ids_unique(&s);
            if !conflicts.is_empty() {
                failed += 1;
                if first.is_none() {
                    first = Some(format!("{left:?} vs {right:?}: {conflicts:?}"));
                }
            }
            count += 1;
        }
    }
    std::eprintln!("full 3/4 alignment: cases={count} failed={failed} first={first:?}");
    assert_eq!(failed, 0);
}

#[test]
fn original_486_permutations_through_complete_alignment() {
    let mut count = 0;
    let mut failed = 0;
    let mut first = None;
    for mut code in 0..243 {
        let [a, b, c, d, e] = std::array::from_fn(|_| {
            let n = code % 3 + 1;
            code /= 3;
            n
        });
        let small = [a, b];
        let large = [c, d, e];
        if !(1..=3).all(|v| {
            small.iter().filter(|&&x| x == v).count() <= large.iter().filter(|&&x| x == v).count()
        }) {
            continue;
        }
        for reverse in [false, true] {
            let (left, right) = if reverse {
                (&large[..], &small[..])
            } else {
                (&small[..], &large[..])
            };
            let mut editor = reference_subset(left, 0);
            let mut studio = reference_subset(right, 0);
            namespace(&mut studio);
            let conflicts = twin_align(&mut editor, &mut studio);
            if !conflicts.is_empty() {
                failed += 1;
                if first.is_none() {
                    first = Some(format!("{left:?} vs {right:?}: {conflicts:?}"));
                }
            }
            count += 1;
        }
    }
    std::eprintln!("full 2/3 alignment: cases={count} failed={failed} first={first:?}");
    assert_eq!(failed, 0);
}

fn next(seed: &mut u64, bound: usize) -> usize {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    ((*seed >> 32) as usize) % bound
}

fn random_document(seed: &mut u64, studio: bool) -> SettingsBytecode {
    let mut d = twin_document();
    for group in ["A", "B", "C"] {
        for _ in 0..(next(seed, 3) + 1) {
            let root = twin_add(&mut d, 0, group, "Model", json!({}));
            for _ in 0..(next(seed, 2) + 1) {
                twin_add(&mut d, root, "Part", "Part", json!({}));
            }
            for _ in 0..(next(seed, 2) + 1) {
                twin_add(&mut d, root, "Pointer", "ObjectValue", json!({}));
            }
        }
    }
    for i in 1..d.instances.len() {
        let class = d.instances[i].class_name.clone();
        if class == "ObjectValue" {
            let target = next(seed, d.instances.len() - 1) + 1;
            let r = twin_ref(&d, target);
            d.instances[i].properties.insert("Value".into(), r);
        }
        if next(seed, 4) == 0 {
            d.instances[i]
                .attributes
                .insert("Marker".into(), json!(next(seed, 2)));
        }
        if next(seed, 7) == 0 {
            d.instances[i].properties.insert(
                "UniqueId".into(),
                json!({"_type":"UniqueId","value":format!("{:032x}",next(seed,10)+1)}),
            );
        }
    }
    for _ in 0..2 {
        twin_add(&mut d, 0, "Control", "Folder", json!({}));
    }
    if studio {
        namespace(&mut d);
    }
    d
}

#[test]
fn adversarial_valid_documents_exercise_recovery() {
    pairing::twins::take_recoveries();
    let mut count = 0;
    let mut hits = 0;
    for seed in 0..6000u64 {
        let mut state = seed;
        let mut e = random_document(&mut state, false);
        let mut s = random_document(&mut state, true);
        // Both inputs contain additions; ordinary unique settings IDs and valid parents/references.
        check_resolution(&e, &s);
        let mut found = pairing::twins::take_recoveries();
        let conflicts = twin_align(&mut e, &mut s);
        assert_twin_ids_unique(&s);
        found += pairing::twins::take_recoveries();
        if found > 0 {
            hits += 1;
            std::eprintln!(
                "RECOVERY seed={seed} hits={found} conflicts={}",
                conflicts.len()
            );
        }
        count += 1;
    }
    std::eprintln!("FUZZ cases={count} recovery_cases={hits}");
    assert_eq!(hits, 0);
}

#[test]
fn mixed_class_control() {
    for reverse in [false, true] {
        let mut editor = twin_document();
        let mut studio = twin_document();
        for class in ["Part", "Folder"] {
            twin_add(&mut editor, 0, "Child", class, json!({}));
        }
        for _ in 0..2 {
            twin_add(&mut studio, 0, "Child", "Folder", json!({}));
        }
        if reverse {
            std::mem::swap(&mut editor, &mut studio);
        }
        namespace(&mut studio);
        let conflicts = twin_align(&mut editor, &mut studio);
        assert_eq!(
            conflicts
                .iter()
                .filter(|v| v.contains("different classes"))
                .count(),
            1,
            "{conflicts:?}"
        );
        assert_twin_ids_unique(&studio);
    }
}

#[test]
fn mixed_classes_preserve_matched_locks_and_one_sided_additions() {
    for reverse in [false, true] {
        for added in ["Part", "Folder"] {
            let mut editor = twin_document();
            let mut studio = twin_document();
            for document in [&mut editor, &mut studio] {
                twin_add(
                    document,
                    0,
                    "Child",
                    "Part",
                    json!({"UniqueId":{"_type":"UniqueId","value":"000000000000000000000000000000f1"}}),
                );
                twin_add(document, 0, "Child", "Folder", json!({}));
            }
            twin_add(&mut studio, 0, "Child", added, json!({}));
            if reverse {
                std::mem::swap(&mut editor, &mut studio);
            }
            namespace(&mut studio);
            let resolved = check_resolution(&editor, &studio);
            assert_eq!(resolved.partners[1], Some(1));
            assert_eq!(resolved.locked[1], Some(1));
            assert!(twin_align(&mut editor, &mut studio).is_empty());
            assert_eq!(editor.instances.len() + studio.instances.len(), 7);
            assert_twin_ids_unique(&studio);
        }
    }
}

#[test]
fn proven_subtrees_still_compare_changed_locked_records() {
    let mut editor = reference_subset(&[1, 2], 1);
    let mut studio = reference_subset(&[2, 1], 1);
    for document in [&mut editor, &mut studio] {
        document.instances[6].properties.insert(
            "UniqueId".into(),
            json!({"_type":"UniqueId","value":"000000000000000000000000000000f2"}),
        );
    }
    studio.instances[6]
        .attributes
        .insert("Changed".into(), json!(true));
    namespace(&mut studio);
    let conflicts = twin_align(&mut editor, &mut studio);
    assert!(!conflicts.is_empty());
    assert!(
        conflicts.iter().any(|value| value.contains("Changed")),
        "{conflicts:?}"
    );
}

#[test]
fn joint_reference_search_rejects_a_graph_without_a_complete_counterpart() {
    for reverse in [false, true] {
        let editor = cyclic(&[0, 1], &[0, 1], false, false);
        let mut studio = cyclic(&[0, 2, 1], &[1, 0], false, false);
        // B0 now points into the other cycle; neither A0 nor the added A2 can
        // supply the missing reciprocal edge for an exact two-cycle counterpart.
        let other = twin_ref(&studio, 3);
        studio.instances[5].properties.insert("Value".into(), other);
        let (mut editor, mut studio) = if reverse {
            (studio, editor)
        } else {
            (editor, studio)
        };
        namespace(&mut studio);
        check_resolution(&editor, &studio);
        assert!(!twin_align(&mut editor, &mut studio).is_empty());
    }
}
