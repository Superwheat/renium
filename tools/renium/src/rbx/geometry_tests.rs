use super::*;

#[test]
fn parallel_default_presence_keeps_sequential_counts_and_ties() {
    for distinct in [2, 17, 64, 65, 137] {
        let values = (0..5000)
            .map(|index| RbxVariant::Int32(index % distinct))
            .collect::<Vec<_>>();
        let mut expected = ExportPresence::default();
        for value in &values {
            expected.record(value);
        }
        for size in [1, 7, 2048, usize::MAX] {
            let chunks = values
                .par_chunks(size)
                .map(|chunk| {
                    let mut presence = ExportPresence::default();
                    for value in chunk {
                        presence.record(value);
                    }
                    presence
                })
                .collect::<Vec<_>>();
            let mut actual = ExportPresence::default();
            for chunk in chunks {
                actual.merge(chunk);
            }
            assert_eq!(actual.present, expected.present);
            assert_eq!(actual.overflowed, expected.overflowed);
            assert_eq!(
                actual.fill(),
                expected.fill(),
                "{distinct} distinct, {size} rows"
            );
        }
    }
}

pub(crate) fn fill_engine_geometry_slow(dom: &mut RbxWeakDom, base: &RbxWeakDom) {
    let mut by_path: HashMap<(Vec<String>, Vec<usize>), (GeometrySources, GeometryValues)> =
        HashMap::new();
    let mut by_identity: HashMap<
        (rbx_dom_weak::Ustr, rbx_dom_weak::Ustr, String),
        GeometryCandidates,
    > = HashMap::new();
    for instance in base.descendants() {
        let values = engine_geometry_properties(instance.class.as_str())
            .iter()
            .filter(|name| has_data(instance, name))
            .map(|name| {
                let key = rbx_dom_weak::Ustr::from(name);
                (key, instance.properties.get(&key).cloned().unwrap())
            })
            .collect::<GeometryValues>();
        if values.is_empty() {
            continue;
        }
        let sources = GeometrySources::of(instance);
        if let Some(identity) = &sources.identity {
            for (key, value) in &values {
                by_identity
                    .entry((instance.class, *key, identity.clone()))
                    .or_default()
                    .entry(sources.fidelity)
                    .and_modify(|known| {
                        if known.as_ref() != Some(value) {
                            *known = None;
                        }
                    })
                    .or_insert(Some(value.clone()));
            }
        }
        let path = crate::rbx::model::rbx_dom_instance_path_parts(base, instance.referent());
        by_path.insert(path, (sources, values));
    }
    if by_path.is_empty() {
        return;
    }
    let targets = dom
        .descendants()
        .filter(|instance| {
            engine_geometry_properties(instance.class.as_str())
                .iter()
                .any(|name| !has_data(instance, name))
        })
        .map(|instance| instance.referent())
        .collect::<Vec<_>>();
    let mut filled: HashMap<rbx_dom_weak::Ustr, usize> = HashMap::new();
    let mut missing: HashMap<rbx_dom_weak::Ustr, usize> = HashMap::new();
    let mut logged = 0usize;
    for referent in targets {
        let path = crate::rbx::model::rbx_dom_instance_path_parts(dom, referent);
        let Some(instance) = dom.get_by_ref_mut(referent) else {
            continue;
        };
        let sources = GeometrySources::of(instance);
        let at_path = by_path.get(&path);
        let agreed = at_path.filter(|(base_sources, _)| base_sources.agrees_with(&sources));
        for name in engine_geometry_properties(instance.class.as_str()) {
            if has_data(instance, name)
                || *name == COLLISION_HULL_PROPERTY && collides_as_box(instance)
            {
                continue;
            }
            let key = rbx_dom_weak::Ustr::from(name);
            let value = agreed
                .and_then(|(_, values)| values.iter().find(|(known, _)| *known == key))
                .map(|(_, value)| value)
                .or_else(|| {
                    sources
                        .identity
                        .as_ref()
                        .and_then(|identity| {
                            by_identity.get(&(instance.class, key, identity.clone()))
                        })
                        .and_then(|values| matching_geometry(values, sources.fidelity))
                });
            match value {
                Some(value) => {
                    *filled.entry(key).or_default() += 1;
                    instance.properties.insert(key, value.clone());
                }
                None => {
                    *missing.entry(key).or_default() += 1;
                    if logged < 20 {
                        logged += 1;
                        let reason = match at_path {
                            Some((base_sources, _)) => format!("base part has {base_sources:?}"),
                            None => "no base part at this path".to_string(),
                        };
                        crate::app::output::log_global(
                            4,
                            format_args!(
                                "[renium] no {name} for {} with {sources:?}: {reason}",
                                path.0.join(".")
                            ),
                        );
                    }
                }
            }
        }
    }
    for (key, count) in &filled {
        crate::app::output::log_global(
            4,
            format_args!(
                "[renium] the base place supplied {count} {key} values; {} parts still lack one",
                missing.get(key).copied().unwrap_or_default()
            ),
        );
    }
}

fn generated(seed: u64, base: bool) -> RbxWeakDom {
    use crate::tests::support::SimRng;
    let mut rng = SimRng(seed);
    let mut dom =
        RbxWeakDom::new(RbxInstanceBuilder::new("DataModel").with_referent(RbxRef::some(1)));
    let mut parents = vec![
        dom.insert(
            dom.root_ref(),
            RbxInstanceBuilder::new("Workspace")
                .with_name("Renamed")
                .with_referent(RbxRef::some(2)),
        ),
    ];
    for index in 0..96 {
        let class = ["Folder", "MeshPart", "UnionOperation", "Part"][rng.below(4)];
        let name = ["a", "a", "A", "a.b", ""][rng.below(5)];
        let parent = parents[rng.below(parents.len())];
        let asset = rng.below(5);
        let fidelity = rng.below(4) as u32;
        let supplied = rng.below(3) == 0;
        let moved = rng.below(4) == 0;
        let mut node = RbxInstanceBuilder::new(class)
            .with_referent(RbxRef::some(index + 3))
            .with_name(if !base && moved { "Moved" } else { name });
        if class == "MeshPart" {
            node = node
                .with_property(
                    "MeshId",
                    RbxContentId::from(format!("rbxassetid://{asset}")),
                )
                .with_property("CollisionFidelity", RbxEnum::from_u32(fidelity));
        }
        if base || supplied {
            for property in engine_geometry_properties(class) {
                node = node.with_property(
                    *property,
                    RbxSharedString::new(vec![asset as u8, fidelity as u8]),
                );
            }
        }
        let referent = dom.insert(parent, node);
        if index % 5 == 0 {
            parents.push(referent);
        }
    }
    dom
}

#[test]
fn geometry_fill_matches_scan_oracle_on_generated_places() {
    crate::tests::support::run_seeds(64, |seed, _| {
        let base = generated(seed, true);
        let mut expected = generated(seed, false);
        let mut actual = generated(seed, false);
        fill_engine_geometry_slow(&mut expected, &base);
        fill_engine_geometry_from(&mut actual, &base);
        for node in expected.descendants() {
            if actual.get_by_ref(node.referent()).unwrap().properties != node.properties {
                return Err("geometry properties differ".into());
            }
        }
        let mut expected_bytes = Vec::new();
        let mut actual_bytes = Vec::new();
        rbx_binary::to_writer(&mut expected_bytes, &expected, expected.root().children()).unwrap();
        rbx_binary::to_writer(&mut actual_bytes, &actual, actual.root().children()).unwrap();
        if actual_bytes != expected_bytes {
            return Err("serialized geometry differs".into());
        }
        Ok(())
    });
}
