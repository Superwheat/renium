use super::*;
use crate::tests::support::{settings_document, settings_instance};
use serde_json::json;

struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 as usize % bound
    }
}

fn generated(seed: u64, count: usize) -> SettingsBytecode {
    let mut rng = Rng(seed);
    let mut root = settings_instance("root", "Workspace", "Workspace", None);
    root.properties.insert(
        "CurrentCamera".into(),
        json!({"_type": "Ref", "instanceIndex": 2}),
    );
    let camera = settings_instance("camera", "Camera", "Camera", Some(0));
    let mut instances = vec![root, camera];
    for index in 2..count {
        let id = match seed % 3 {
            0 => format!("debug:0_{index}"),
            1 => format!("{index:x}"),
            _ => format!("editor:{index}"),
        };
        let name = match rng.below(4) {
            0 => "duplicate".to_string(),
            1 => "".to_string(),
            _ => format!("child-{index}"),
        };
        let mut instance = settings_instance(&id, &name, "Part", Some(0));
        instance
            .properties
            .insert("Transparency".into(), json!(rng.below(8) as f64 / 8.0));
        instance.properties.insert(
            "UniqueId".into(),
            json!({"_type":"UniqueId", "value":format!("{:032x}", index + 1)}),
        );
        instance.properties.insert(
            "Nested".into(),
            json!({"array":[index, {"_type":"Ref", "instanceIndex":1 + rng.below(count)}]}),
        );
        instances.push(instance);
    }
    settings_document(instances)
}

/// A pull that holds both stores decoded answers what one that holds their
/// bytes answers.
fn documents_answer_like_bytes(reference: &[u8], observed: &[u8]) {
    let from_bytes = align_settings_bytes(reference, observed, true);
    let from_documents = decode_settings_bytecode(reference).and_then(|reference| {
        align_pulled_settings_documents_to_reference(
            reference,
            decode_settings_bytecode(observed)?,
            std::time::Instant::now(),
        )
    });
    match (from_bytes, from_documents) {
        (Ok(SettingsAlignment::Equivalent), Ok(SettingsAlignment::Equivalent)) => {}
        (Ok(SettingsAlignment::Changed(bytes)), Ok(SettingsAlignment::Changed(documents))) => {
            assert!(bytes == documents, "the two entries write different stores")
        }
        (Err(bytes), Err(documents)) => assert_eq!(bytes.to_string(), documents.to_string()),
        _ => panic!("the document entry answers differently"),
    }
}

fn same_result(reference: &[u8], observed: &[u8], keep_camera: bool) {
    if keep_camera {
        documents_answer_like_bytes(reference, observed);
    }
    let expected = align_settings_bytes_oracle(reference, observed, keep_camera);
    let actual = align_settings_bytes(reference, observed, keep_camera);
    match (expected, actual) {
        (Ok(SettingsAlignment::Equivalent), Ok(SettingsAlignment::Equivalent)) => {}
        (Ok(SettingsAlignment::Changed(expected)), Ok(SettingsAlignment::Changed(actual))) => {
            assert_eq!(expected, actual)
        }
        (Err(expected), Err(actual)) => assert_eq!(expected.to_string(), actual.to_string()),
        _ => panic!("alignment outcome differs"),
    }
}

#[test]
fn pulled_alignment_matches_eager_encoder_on_seeded_edits() {
    let (mut accepted, mut rejected) = (0, 0);
    for seed in 1..=48 {
        let count = if seed <= 3 { 2_052 } else { 12 + seed as usize };
        let reference = generated(seed, count);
        for edit in 0..10 {
            let mut observed = reference.clone();
            match edit {
                0 => {}
                1 => {
                    observed.instances[1]
                        .properties
                        .insert("FieldOfView".into(), json!(55));
                }
                2 => {
                    observed.instances[0]
                        .properties
                        .insert("Gravity".into(), json!(197.25));
                }
                3 => {
                    observed.instances[2]
                        .properties
                        .insert("Transparency".into(), json!(0.375));
                }
                4 => {
                    let first = observed.instances[2].properties.remove("UniqueId").unwrap();
                    let second = observed.instances[3]
                        .properties
                        .insert("UniqueId".into(), first)
                        .unwrap();
                    observed.instances[2]
                        .properties
                        .insert("UniqueId".into(), second);
                }
                5 => {
                    observed.instances[2].name.push('x');
                }
                6 => {
                    observed.instances[3].parent_index = Some(2);
                }
                7 => {
                    for (index, instance) in observed.instances.iter_mut().enumerate() {
                        instance.settings_id = format!("debug:0_{}", index + count);
                    }
                    observed.instances[1]
                        .attributes
                        .insert("view".into(), json!(seed));
                }
                8 => {
                    observed.instances[2].properties.insert(
                        "External".into(),
                        json!({"_type":"Ref", "settingsId":"root", "pathSegments":["Lighting"]}),
                    );
                }
                _ => {
                    observed.instances[3].properties.insert(
                        "Enum".into(),
                        json!({"_type":"EnumItem", "name":"Plastic", "enumType":"Material"}),
                    );
                }
            }
            if identities_already_aligned(&reference, &observed) {
                accepted += 1;
                let mut matched = observed.clone();
                assert!(align_settings_ids_by_matching(&reference, &mut matched));
                stabilize_settings_reference_ids(&mut matched);
                let mut expected = observed.clone();
                stabilize_settings_reference_ids(&mut expected);
                assert_eq!(matched, expected);
            } else {
                rejected += 1;
            }
            let reference = encode_settings_bytecode(&reference).unwrap();
            let observed = encode_settings_bytecode(&observed).unwrap();
            same_result(&reference, &observed, true);
            same_result(&reference, &observed, false);
        }
    }
    assert!(accepted > 100 && rejected > 100);
}

/// A store whose camera alone moved is unchanged to a pull, whatever else it
/// holds; one with another edit goes the way it went before.
#[test]
fn a_pull_whose_camera_alone_moved_is_unchanged_under_any_property_spelling() {
    for (class, fields) in [
        ("Part", json!({"Color":[1,0,0], "Color3uint8":[0,1,0]})),
        (
            "Model",
            json!({"WorldPivotData":{"_type":"CFrame", "components":[0,0,0,1,0,0,0,1,0,0,0,1]}}),
        ),
        ("Script", json!({"Disabled":true})),
        (
            "Shirt",
            json!({"ShirtTemplateContent":{"_type":"Content", "value":"rbxassetid://1"}}),
        ),
        (
            "Shirt",
            json!({"ShirtTemplate":"rbxassetid://2", "ShirtTemplateContent":"rbxassetid://1"}),
        ),
    ] {
        let mut reference = generated(3, 5);
        reference.instances[2].class_name = class.into();
        reference.instances[2].properties = fields.as_object().unwrap().clone();
        for edited in [false, true] {
            let mut observed = reference.clone();
            observed.instances[1]
                .properties
                .insert("FieldOfView".into(), json!(65));
            if edited {
                observed.instances[0]
                    .properties
                    .insert("Gravity".into(), json!(198));
                same_result(
                    &encode_settings_bytecode(&reference).unwrap(),
                    &encode_settings_bytecode(&observed).unwrap(),
                    true,
                );
            } else {
                assert!(matches!(
                    pulled(&reference, &observed),
                    SettingsAlignment::Equivalent
                ));
            }
        }
    }
}

#[test]
fn runtime_identity_reuse_retains_legacy_reencoding() {
    let mut reference = settings_document(vec![settings_instance(
        "debug:0_1",
        "Lighting",
        "Lighting",
        None,
    )]);
    reference.instances[0]
        .properties
        .insert("Brightness".into(), json!(2));
    let before = encode_settings_bytecode(&reference).unwrap();
    reference.instances[0]
        .properties
        .insert("Brightness".into(), json!(3));
    let mut after = encode_settings_bytecode(&reference).unwrap();
    after[b"RBSSET\0".len()] = 10;
    same_result(&before, &after, true);
}

fn align_settings_bytes_oracle(
    reference_bytes: &[u8],
    observed_bytes: &[u8],
    keep_viewport_camera: bool,
) -> Result<SettingsAlignment> {
    if reference_bytes == observed_bytes {
        return Ok(SettingsAlignment::Equivalent);
    }
    let cache_key = settings_alignment_cache_key(reference_bytes, observed_bytes);
    if cache_key.is_some_and(settings_alignment_is_cached) {
        return Ok(SettingsAlignment::Equivalent);
    }
    let (reference, observed) = if reference_bytes.len().saturating_add(observed_bytes.len())
        >= SETTINGS_PARALLEL_DECODE_MIN_BYTES
    {
        rayon::join(
            || decode_settings_bytecode(reference_bytes),
            || decode_settings_bytecode(observed_bytes),
        )
    } else {
        (
            decode_settings_bytecode(reference_bytes),
            decode_settings_bytecode(observed_bytes),
        )
    };
    let mut reference = reference?;
    let mut observed = observed?;
    // Taken before alignment rewrites either document.
    let viewport_camera = keep_viewport_camera
        .then(|| reference_viewport_camera(&reference))
        .flatten();
    let keep_camera = |observed: &mut SettingsBytecode| {
        viewport_camera
            .as_ref()
            .is_some_and(|camera| keep_reference_viewport_camera(camera, observed))
    };
    let topology_matches = settings_topology_matches(&reference, &observed);
    let positionally_equivalent =
        topology_matches && positional_values_equivalent(&reference, &observed, None, false);
    if positionally_equivalent {
        if document_adds_enum_types(&reference, &observed) {
            if !align_settings_ids_oracle(&reference, &mut observed) {
                bail!("duplicate instance identity is ambiguous after comparing references");
            }
            keep_camera(&mut observed);
            let upgraded = encode_settings_bytecode(&observed)?;
            drop_settings_documents(reference, observed);
            return Ok(SettingsAlignment::Changed(upgraded));
        }
        if let Some(key) = cache_key {
            cache_settings_alignment(key);
        }
        drop_settings_documents(reference, observed);
        return Ok(SettingsAlignment::Equivalent);
    }
    // A store whose instances stand where the reference's stand, under the
    // same ids, already carries the reference's identities: nothing is left
    // to align, and its own bytes are what the encoder writes for it again.
    // This is a pull after values changed in Studio, the common one.
    if topology_matches && identities_already_aligned_oracle(&reference, &observed) {
        rayon::join(
            || stabilize_settings_reference_ids(&mut reference),
            || stabilize_settings_reference_ids(&mut observed),
        );
        let camera_kept = keep_camera(&mut observed);
        let aligned = if camera_kept {
            encode_settings_bytecode(&observed)?
        } else {
            observed_bytes.to_vec()
        };
        return finish_settings_alignment_oracle(
            reference,
            observed,
            aligned,
            cache_key,
            camera_kept,
        );
    }
    match align_settings_ids_for_contiguous_structural_change(&mut reference, &mut observed) {
        ContiguousStructuralAlignment::Aligned => {
            keep_camera(&mut observed);
            let aligned = encode_settings_bytecode_with_dense_references(&observed)?;
            drop_settings_documents(reference, observed);
            return Ok(SettingsAlignment::Changed(aligned));
        }
        ContiguousStructuralAlignment::PreparedMismatch => {}
        ContiguousStructuralAlignment::NotApplicable => {
            rayon::join(
                || stabilize_settings_reference_ids(&mut reference),
                || stabilize_settings_reference_ids(&mut observed),
            );
        }
    }
    if !align_settings_ids_oracle(&reference, &mut observed) {
        bail!("duplicate instance identity is ambiguous after comparing references");
    }
    let camera_kept = keep_camera(&mut observed);
    let aligned = encode_settings_bytecode(&observed)?;
    finish_settings_alignment_oracle(reference, observed, aligned, cache_key, camera_kept)
}

fn finish_settings_alignment_oracle(
    mut reference: SettingsBytecode,
    mut observed: SettingsBytecode,
    aligned: Vec<u8>,
    cache_key: Option<SettingsAlignmentCacheKey>,
    camera_kept: bool,
) -> Result<SettingsAlignment> {
    canonicalize_settings_property_names(&mut reference)?;
    canonicalize_settings_property_names(&mut observed)?;
    let result = if settings_documents_match(&reference, &observed, false)
        && !document_adds_enum_types(&reference, &observed)
    {
        // The cache answers for the bytes alone, also to a caller that keeps
        // no camera: a pair that only matches with the camera kept stays out.
        if let Some(key) = cache_key.filter(|_| !camera_kept) {
            cache_settings_alignment(key);
        }
        SettingsAlignment::Equivalent
    } else {
        SettingsAlignment::Changed(aligned)
    };
    drop_settings_documents(reference, observed);
    Ok(result)
}

fn identities_already_aligned_oracle(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
) -> bool {
    let mut seen = AHashSet::with_capacity(reference.instances.len());
    reference.instances.len() == observed.instances.len()
        && reference
            .instances
            .iter()
            .zip(&observed.instances)
            .all(|(reference, observed)| {
                reference.settings_id == observed.settings_id
                    && is_store_identity(&reference.settings_id)
                    && seen.insert(reference.settings_id.as_str())
            })
        && workspace_current_camera_index(reference) == workspace_current_camera_index(observed)
        && persistent_identities_follow_mapping(reference, observed, Some)
}

fn align_settings_ids_oracle(
    reference: &SettingsBytecode,
    observed: &mut SettingsBytecode,
) -> bool {
    if identities_already_aligned_oracle(reference, observed)
        && settings_topology_matches(reference, observed)
    {
        return true;
    }
    align_settings_ids_by_matching(reference, observed)
}

fn workspace_with_camera(root: &str, camera: &str) -> Vec<SettingsBytecodeInstance> {
    let mut workspace = settings_instance(root, "Workspace", "Workspace", None);
    workspace.properties.insert(
        "CurrentCamera".into(),
        json!({"_type": "Ref", "instanceIndex": 2}),
    );
    let mut viewport = settings_instance(camera, "Camera", "Camera", Some(0));
    viewport.properties.insert("FieldOfView".into(), json!(70));
    vec![workspace, viewport]
}

fn pulled(reference: &SettingsBytecode, observed: &SettingsBytecode) -> SettingsAlignment {
    let (reference, observed) = (
        encode_settings_bytecode(reference).unwrap(),
        encode_settings_bytecode(observed).unwrap(),
    );
    documents_answer_like_bytes(&reference, &observed);
    align_settings_bytes(&reference, &observed, true).unwrap()
}

fn with_moved_camera(reference: &SettingsBytecode) -> SettingsBytecode {
    let mut observed = reference.clone();
    observed.instances[1]
        .properties
        .insert("FieldOfView".into(), json!(55));
    observed
}

/// The pull discards the observed camera's values, so two spellings of one
/// of them that disagree are not its concern.
#[test]
fn a_conflict_in_the_camera_a_pull_discards_does_not_fail_the_pull() {
    let reference = settings_document(workspace_with_camera("ws", "cam"));
    let mut observed = reference.clone();
    let cframe = |x: f64| json!({"_type":"CFrame", "components":[x,0,0,1,0,0,0,1,0,0,0,1]});
    observed.instances[1]
        .properties
        .insert("CFrame".into(), cframe(1.0));
    observed.instances[1]
        .properties
        .insert("CoordinateFrame".into(), cframe(2.0));
    assert!(matches!(
        pulled(&reference, &observed),
        SettingsAlignment::Equivalent
    ));
}

/// Two instances that stand where they stood, under runtime ids that changed
/// places: the enum type one of them gained is found on the instance at its
/// position, not on the one that now carries its old id.
#[test]
fn an_enum_type_gained_is_seen_under_runtime_ids_that_changed_places() {
    let untyped = json!({"items":[{"_type":"EnumItem", "name":"Plastic"}]});
    let typed = json!({"items":[{"_type":"EnumItem", "name":"Plastic", "enumType":"Material"}]});
    let mut instances = workspace_with_camera("ws", "cam");
    for (id, name, mode) in [("debug:0_2", "A", &untyped), ("debug:0_3", "B", &typed)] {
        let mut part = settings_instance(id, name, "Part", Some(0));
        part.properties.insert("Mode".into(), mode.clone());
        instances.push(part);
    }
    let reference = settings_document(instances);
    for moved in [false, true] {
        let mut observed = if moved {
            with_moved_camera(&reference)
        } else {
            reference.clone()
        };
        observed.instances[2].settings_id = "debug:0_3".into();
        observed.instances[2]
            .properties
            .insert("Mode".into(), typed.clone());
        observed.instances[3].settings_id = "debug:0_2".into();
        observed.instances[3]
            .properties
            .insert("Mode".into(), untyped.clone());
        assert!(
            matches!(pulled(&reference, &observed), SettingsAlignment::Changed(_)),
            "camera moved: {moved}"
        );
    }
}

/// What a pull says about a store does not depend on whether its camera
/// moved: ids that changed places between two unchanged instances leave it
/// unchanged either way.
#[test]
fn ids_that_changed_places_answer_alike_with_the_camera_moved_or_not() {
    let mut instances = workspace_with_camera("a0", "a1");
    instances.push(settings_instance("a2", "Dup", "Part", Some(0)));
    instances.push(settings_instance("a3", "Dup", "Part", Some(0)));
    instances.push(settings_instance("a4", "Child", "Part", Some(5)));
    instances.push(settings_instance("a5", "F", "Folder", Some(0)));
    for (id, name, transparency) in [("a6", "X", 0.0), ("a7", "Y", 0.5)] {
        let mut part = settings_instance(id, name, "Part", Some(0));
        part.properties
            .insert("Transparency".into(), json!(transparency));
        instances.push(part);
    }
    let reference = settings_document(instances);
    for moved in [false, true] {
        let mut observed = if moved {
            with_moved_camera(&reference)
        } else {
            reference.clone()
        };
        observed.instances[6].settings_id = "a7".into();
        observed.instances[7].settings_id = "a6".into();
        assert!(
            matches!(pulled(&reference, &observed), SettingsAlignment::Equivalent),
            "camera moved: {moved}"
        );
    }
}

#[test]
fn a_kept_camera_that_points_at_an_instance_leaves_the_store_unchanged() {
    let mut instances = workspace_with_camera("ws", "cam");
    instances[1].properties.insert(
        "CameraSubject".into(),
        json!({"_type":"Ref", "instanceIndex": 3}),
    );
    instances.push(settings_instance("part", "Subject", "Part", Some(0)));
    let reference = settings_document(instances);
    assert!(matches!(
        pulled(&reference, &with_moved_camera(&reference)),
        SettingsAlignment::Equivalent
    ));
}

/// A caller that keeps no camera is never told a moved camera is no change,
/// also right after a pull of the same two stores was.
#[test]
fn a_moved_camera_is_a_change_to_a_caller_that_keeps_none() {
    let reference = generated(1, 2_052);
    let observed = with_moved_camera(&reference);
    let (reference, observed) = (
        encode_settings_bytecode(&reference).unwrap(),
        encode_settings_bytecode(&observed).unwrap(),
    );
    assert!(matches!(
        align_settings_bytes(&reference, &observed, true).unwrap(),
        SettingsAlignment::Equivalent
    ));
    assert!(matches!(
        align_settings_bytes(&reference, &observed, false).unwrap(),
        SettingsAlignment::Changed(_)
    ));
}

/// An edit next to a moved camera is written with the stored camera, under
/// ids a store gave as under Studio's own.
#[test]
fn an_edit_beside_a_moved_camera_is_written_with_the_stored_camera() {
    for seed in [2, 3] {
        let reference = generated(seed, 40);
        let mut observed = with_moved_camera(&reference);
        observed.instances[0]
            .properties
            .insert("Gravity".into(), json!(150));
        let SettingsAlignment::Changed(written) = pulled(&reference, &observed) else {
            panic!("the edit is a change");
        };
        let written = decode_settings_bytecode(&written).unwrap();
        assert_eq!(written.instances[0].properties["Gravity"], json!(150));
        assert_eq!(
            written.instances[1].properties.get("FieldOfView"),
            reference.instances[1].properties.get("FieldOfView")
        );
        same_result(
            &encode_settings_bytecode(&reference).unwrap(),
            &encode_settings_bytecode(&observed).unwrap(),
            true,
        );
    }
}
