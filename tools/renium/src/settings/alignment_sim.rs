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
    let stored = reference;
    let from_documents = decode_settings_bytecode(reference).and_then(|reference| {
        align_pulled_settings_documents_to_reference(
            reference,
            decode_settings_bytecode(observed)?,
            &|| Ok(observed.to_vec()),
            std::time::Instant::now(),
            stored,
        )
        .map(|pulled| pulled.alignment)
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
            if identities_already_aligned(&reference, &observed, Placed::default()) {
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
    let mut after = crate::settings::bytecode::encode_settings_bytecode_v11(&reference).unwrap();
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
    let positionally_equivalent = topology_matches
        && positional_values_equivalent(&reference, &observed, None, false, Placed::default());
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
        ContiguousStructuralAlignment::Aligned { .. } => {
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

/// The digest a pull takes of a captured state that gives this document.
fn pull_digest(document: &SettingsBytecode) -> [u8; 32] {
    let state = crate::settings::bytecode::captured_sim::captured_state(document, false);
    crate::settings::bytecode::pulled_digest_from_service_state(&state)
        .unwrap()
        .digest
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

/// A pull says whether the store it leaves holds the observed document as it
/// came: after an instance was added it does while every instance kept its
/// id, and not once the stored ids took the place of the observed ones.
#[test]
fn a_pull_says_whether_the_store_it_leaves_is_the_observed_document() {
    let reference = generated(2, 12);
    let stored = encode_settings_bytecode(&reference).unwrap();
    let pull = |observed: &SettingsBytecode| {
        align_pulled_settings_documents_to_reference(
            reference.clone(),
            observed.clone(),
            &|| encode_settings_bytecode_over(observed, false, Some(&stored)),
            std::time::Instant::now(),
            &stored,
        )
        .unwrap()
    };
    let mut added = reference.clone();
    added
        .instances
        .push(settings_instance("editor:new", "Added", "Folder", Some(0)));
    let kept = pull(&added);
    assert!(matches!(kept.alignment, SettingsAlignment::Changed(_)));
    assert!(kept.stored_as_observed);

    let mut renumbered = added.clone();
    renumbered.instances[5].settings_id = "debug:0_77".into();
    let rewritten = pull(&renumbered);
    let SettingsAlignment::Changed(bytes) = &rewritten.alignment else {
        panic!("an added instance leaves the store as it was");
    };
    assert_eq!(
        decode_settings_bytecode(bytes).unwrap().instances[5].settings_id,
        reference.instances[5].settings_id
    );
    assert!(!rewritten.stored_as_observed);

    let unchanged = pull(&reference);
    assert!(matches!(unchanged.alignment, SettingsAlignment::Equivalent));
    assert!(unchanged.stored_as_observed);
}

/// The digest a pull remembers an unchanged store by leaves out what the
/// viewport camera holds and nothing else.
#[test]
fn the_pull_digest_ignores_the_viewport_camera_and_nothing_else() {
    let mut instances = workspace_with_camera("ws", "cam");
    let mut second_camera = settings_instance("cam2", "Spare", "Camera", Some(0));
    second_camera
        .properties
        .insert("FieldOfView".into(), json!(70));
    instances.push(second_camera);
    let mut part = settings_instance("part", "Part", "Part", Some(0));
    part.properties.insert("Transparency".into(), json!(0.5));
    part.attributes.insert("Level".into(), json!(1));
    instances.push(part);
    let reference = settings_document(instances);
    let digest = pull_digest(&reference);

    let mut moved = with_moved_camera(&reference);
    moved.instances[1]
        .attributes
        .insert("Seen".into(), json!(true));
    assert!(pull_digest(&moved) == digest);

    let edits: [fn(&mut SettingsBytecode); 9] = [
        |document| document.instances[3].settings_id.push('x'),
        |document| document.instances[3].name.push('x'),
        |document| document.instances[3].class_name = "Folder".into(),
        |document| document.instances[3].parent_index = Some(2),
        |document| {
            document.instances[3]
                .properties
                .insert("Transparency".into(), json!(0.25));
        },
        |document| {
            document.instances[3]
                .attributes
                .insert("Level".into(), json!(2));
        },
        |document| {
            document.instances[2]
                .properties
                .insert("FieldOfView".into(), json!(55));
        },
        |document| document.instances[1].name.push('x'),
        |document| {
            document.instances.pop();
        },
    ];
    for (index, edit) in edits.iter().enumerate() {
        let mut edited = reference.clone();
        edit(&mut edited);
        assert!(
            pull_digest(&edited) != digest,
            "edit {index} left the digest as it was"
        );
    }
    // A number is told from the same digits as text, and 1 from 1.0.
    let mut text = reference.clone();
    text.instances[3]
        .properties
        .insert("Transparency".into(), json!("0.5"));
    let mut whole = reference.clone();
    whole.instances[3]
        .attributes
        .insert("Level".into(), json!(1.0));
    assert!(pull_digest(&text) != digest);
    assert!(pull_digest(&whole) != digest);
}

/// Two observed stores that differ only in what the viewport camera holds
/// are both unchanged against a stored file, or neither is.
#[test]
fn stores_that_differ_only_in_the_viewport_camera_are_unchanged_together() {
    for seed in 1..=24 {
        let reference = generated(seed, 14 + seed as usize);
        for edit in 0..4 {
            let mut observed = reference.clone();
            match edit {
                0 => {}
                1 => {
                    observed.instances[0]
                        .properties
                        .insert("Gravity".into(), json!(150));
                }
                2 => observed.instances[2].name.push('x'),
                _ => {
                    observed.instances[3]
                        .attributes
                        .insert("Level".into(), json!(seed));
                }
            }
            let moved = with_moved_camera(&observed);
            assert!(pull_digest(&observed) == pull_digest(&moved));
            let unchanged = |observed: &SettingsBytecode| {
                matches!(pulled(&reference, observed), SettingsAlignment::Equivalent)
            };
            assert!(
                unchanged(&observed) == unchanged(&moved),
                "seed {seed} edit {edit}"
            );
            assert!(
                unchanged(&observed) == (edit == 0),
                "seed {seed} edit {edit}"
            );
        }
    }
}

/// An instance of the trees the run sim builds its stores from.
#[derive(Clone)]
struct Node {
    label: usize,
    name: String,
    class: &'static str,
    material: Option<&'static str>,
    /// Sixty-fourths of its transparency, where it is a part.
    transparency: usize,
    viewport: bool,
    /// The label of the instance it names by its id.
    refers: Option<usize>,
    /// Its UniqueId, zero for the nil one.
    unique: Option<u64>,
    /// What one comparison weighs and another leaves out: a ScriptGuid, an
    /// engine's attribute and a transparency a hair off.
    guid: Option<usize>,
    engine: Option<usize>,
    noise: bool,
    children: Vec<Node>,
}

const MATERIALS: [&str; 3] = ["Wood", "Neon", "Glass"];

impl Node {
    fn new(
        labels: &mut usize,
        name: String,
        class: &'static str,
        material: Option<&'static str>,
    ) -> Self {
        *labels += 1;
        Node {
            label: *labels,
            name,
            class,
            material,
            transparency: 0,
            viewport: false,
            refers: None,
            unique: None,
            guid: None,
            engine: None,
            noise: false,
            children: Vec::new(),
        }
    }

    /// A namesake of this instance under new labels and UniqueIds, which
    /// holds another material where it holds one.
    fn twin(&self, rng: &mut Rng, labels: &mut usize) -> Node {
        let material = self.material.map(|_| MATERIALS[rng.below(3)]);
        let mut twin = Node::new(labels, self.name.clone(), self.class, material);
        twin.transparency = self.transparency;
        twin.refers = self.refers;
        twin.unique = self.unique.map(|_| rng.unique());
        for child in &self.children {
            let child = child.twin(rng, labels);
            twin.children.push(child);
        }
        twin
    }

    fn each(&mut self, visit: &mut impl FnMut(&mut Node)) {
        visit(self);
        for child in &mut self.children {
            child.each(visit);
        }
    }

    /// Gives each instance a UniqueId or none, and some what the matching or
    /// a place leaves out; each that names another names one of `targets`.
    fn decorate(&mut self, rng: &mut Rng, targets: &[usize]) {
        self.each(&mut |node| {
            node.unique = match rng.below(4) {
                0 => None,
                1 => Some(0),
                _ => Some(rng.unique()),
            };
            node.guid = (rng.below(4) == 0).then(|| rng.below(1 << 20));
            node.engine = (rng.below(5) == 0).then(|| rng.below(3));
            node.noise = rng.below(4) == 0;
            if matches!(node.class, "ObjectValue" | "Weld") && !targets.is_empty() {
                node.refers = Some(targets[rng.below(targets.len())]);
            }
        });
    }

    /// The labels of the instances a reference may name: twins and their
    /// children.
    fn targets(&mut self) -> Vec<usize> {
        let mut targets = Vec::new();
        self.each(&mut |node| {
            if matches!(node.name.as_str(), "A" | "C" | "D") {
                targets.push(node.label);
            }
        });
        targets
    }
}

impl Rng {
    fn unique(&mut self) -> u64 {
        self.below(usize::MAX) as u64 | 1
    }
}

/// An instance the run sim starts a tree with or adds: twins that hold one
/// of a few materials, twins alike told by their children, a unique part, a
/// folder of twins, a camera, twins that name another by its id, or a
/// folder of more twins than the matching weighs one by one.
fn run_sim_node(rng: &mut Rng, labels: &mut usize) -> Node {
    let material = Some(MATERIALS[rng.below(3)]);
    let unique = |kind: &str| format!("{kind}{}", *labels + 1);
    let mut node = match rng.below(10) {
        0 | 1 => Node::new(labels, "A".into(), "Part", material),
        2 => {
            let name = unique("U");
            Node::new(labels, name, "Part", material)
        }
        3 => {
            let name = unique("F");
            let mut folder = Node::new(labels, name, "Folder", None);
            for _ in 0..2 + rng.below(2) {
                let material = Some(MATERIALS[rng.below(3)]);
                folder
                    .children
                    .push(Node::new(labels, "C".into(), "Part", material));
            }
            folder
        }
        4 => {
            let mut twin = Node::new(labels, "A".into(), "Part", None);
            twin.children
                .push(Node::new(labels, "D".into(), "Part", material));
            twin
        }
        5 => Node::new(labels, "Camera".into(), "Camera", None),
        6 => Node::new(labels, "V".into(), "ObjectValue", None),
        7 => Node::new(labels, "W".into(), "Weld", None),
        8 => {
            let name = unique("G");
            let mut folder = Node::new(labels, name, "Folder", None);
            for transparency in 0..32 + rng.below(8) {
                let material = (rng.below(2) == 0).then(|| MATERIALS[rng.below(3)]);
                let mut part = Node::new(labels, "A".into(), "Part", material);
                part.transparency = transparency;
                folder.children.push(part);
            }
            folder
        }
        _ => Node::new(labels, "A".into(), "Part", None),
    };
    if node.class == "Part" && node.transparency == 0 {
        node.transparency = rng.below(3);
    }
    node
}

fn run_sim_tree(rng: &mut Rng, labels: &mut usize) -> Node {
    let mut root = Node::new(labels, "Workspace".into(), "Workspace", None);
    for _ in 0..3 + rng.below(4) {
        let node = run_sim_node(rng, labels);
        root.children.push(node);
    }
    let mut camera = Node::new(labels, "Camera".into(), "Camera", None);
    camera.viewport = true;
    let at = rng.below(root.children.len() + 1);
    root.children.insert(at, camera);
    let targets = root.targets();
    root.decorate(rng, &targets);
    let subject =
        (rng.below(2) == 0 && !targets.is_empty()).then(|| targets[rng.below(targets.len())]);
    root.each(&mut |node| {
        if node.viewport {
            node.refers = subject;
        }
    });
    root
}

/// Removes or adds one or two instances side by side under the root, a
/// folder or a twin, or does so ahead of a twin that takes the place of the
/// one after it: Studio removed a folder and the second of two twins, or
/// added a folder ahead of a twin and a namesake after it. Gives the kind of
/// the edit and whether the twin is a camera; none where the edit would
/// remove the viewport camera.
fn run_sim_edit(rng: &mut Rng, labels: &mut usize, root: &mut Node) -> Option<(usize, bool)> {
    let targets = root.targets();
    let holders = (0..root.children.len())
        .filter(|index| !root.children[*index].children.is_empty())
        .collect::<Vec<_>>();
    let container = if !holders.is_empty() && rng.below(3) == 0 {
        &mut root.children[holders[rng.below(holders.len())]]
    } else {
        root
    };
    let kids = &mut container.children;
    let kind = rng.below(4);
    let mut of_camera = false;
    match kind {
        0 => {
            let len = 1 + rng.below(2).min(kids.len() - 1);
            let start = rng.below(kids.len() - len + 1);
            if kids[start..start + len].iter().any(|node| node.viewport) {
                return None;
            }
            kids.drain(start..start + len);
        }
        1 => {
            let twins = (2..kids.len())
                .filter(|&at| {
                    kids[at].name == kids[at - 1].name
                        && kids[at].class == kids[at - 1].class
                        && !kids[at].viewport
                })
                .collect::<Vec<_>>();
            if twins.is_empty() {
                return None;
            }
            let at = twins[rng.below(twins.len())];
            let start = at - 2 - rng.below((at - 1).min(2));
            if kids[start..at - 1].iter().any(|node| node.viewport) {
                return None;
            }
            of_camera = kids[at].class == "Camera";
            kids.remove(at);
            kids.drain(start..at - 1);
        }
        2 => {
            let at = rng.below(kids.len() + 1);
            for offset in 0..1 + rng.below(2) {
                let mut node = run_sim_node(rng, labels);
                node.decorate(rng, &targets);
                kids.insert(at + offset, node);
            }
        }
        _ => {
            let at = rng.below(kids.len());
            of_camera = kids[at].class == "Camera";
            let twin = kids[at].twin(rng, labels);
            kids.insert(at + 1, twin);
            let name = format!("X{}", *labels + 1);
            let ahead = Node::new(labels, name, "Folder", None);
            kids.insert(at, ahead);
        }
    }
    Some((kind, of_camera))
}

/// Changes what one instance of Studio's copy holds beside a run: its
/// material, its transparency, whom it names, or what the matching or a
/// place leaves out; or gives the viewport camera another subject or none.
fn run_sim_change(rng: &mut Rng, root: &mut Node) {
    let targets = root.targets();
    if rng.below(6) == 5 {
        let subject = targets.get(rng.below(targets.len() + 1)).copied();
        return root.each(&mut |node| {
            if node.viewport {
                node.refers = subject;
            }
        });
    }
    let mut count = 0;
    root.each(&mut |_| count += 1);
    let mut pick = 1 + rng.below(count - 1);
    let change = rng.below(5);
    let (value, target) = (rng.below(3), targets.get(rng.below(targets.len().max(1))));
    root.each(&mut |node| {
        if pick == 0 {
            match change {
                0 => node.material = node.material.map(|_| MATERIALS[value]),
                1 => node.transparency = value,
                2 => node.refers = node.refers.and(target.copied()),
                3 => node.engine = Some(value),
                _ => {
                    node.noise = !node.noise;
                    node.guid = node.guid.map(|guid| guid + 1);
                }
            }
        }
        pick = pick.wrapping_sub(1);
    });
}

/// Crosses two twins both copies hold, namesakes under one parent without
/// UniqueIds: the viewport cameras name each its own of them, or the two
/// hold alike what the comparison by places weighs and crossed what it
/// leaves out, an engine's attribute or a transparency a hair off.
fn run_sim_cross(rng: &mut Rng, files: &mut Node, studio: &mut Node) {
    let mut held = HashSet::new();
    files.each(&mut |node| {
        held.insert(node.label);
    });
    let mut pairs = Vec::new();
    studio.each(&mut |node| {
        for (second, other) in node.children.iter().enumerate() {
            for one in &node.children[..second] {
                if one.name == other.name
                    && one.class == other.class
                    && !one.viewport
                    && !other.viewport
                    && held.contains(&one.label)
                    && held.contains(&other.label)
                {
                    pairs.push((one.label, other.label));
                }
            }
        }
    });
    let Some(&(one, other)) = pairs.get(rng.below(pairs.len().max(1))) else {
        return;
    };
    let kind = rng.below(3);
    for (tree, crossed) in [(files, false), (studio, true)] {
        tree.each(&mut |node| {
            if kind == 0 && node.viewport {
                node.refers = Some(if crossed { other } else { one });
            }
            if node.label == one || node.label == other {
                let marked = (node.label == one) != crossed;
                node.unique = None;
                if kind != 0 {
                    node.material = None;
                    node.transparency = 0;
                    node.engine = (kind == 1 && marked).then_some(1);
                    node.noise = kind == 2 && marked;
                }
            }
        });
    }
}

/// The store a tree gives under the ids `id` gives its labels and places.
fn run_sim_store(root: &Node, id: &dyn Fn(usize, usize) -> String) -> SettingsBytecode {
    fn walk<'a>(node: &'a Node, parent: Option<usize>, nodes: &mut Vec<(&'a Node, Option<usize>)>) {
        let index = nodes.len();
        nodes.push((node, parent));
        for child in &node.children {
            walk(child, Some(index), nodes);
        }
    }
    let mut nodes = Vec::new();
    walk(root, None, &mut nodes);
    let ids = (nodes.iter().enumerate())
        .map(|(index, (node, _))| (node.label, id(node.label, index)))
        .collect::<HashMap<_, _>>();
    let mut instances = Vec::new();
    for (node, parent) in &nodes {
        let mut instance =
            settings_instance(&ids[&node.label], node.name.as_str(), node.class, *parent);
        let properties = &mut instance.properties;
        if let Some(material) = node.material {
            properties.insert("Material".into(), json!(material));
        }
        if node.class == "Part" {
            let noise = if node.noise { 1e-9 } else { 0.0 };
            let transparency = node.transparency as f64 / 64.0 + noise;
            properties.insert("Transparency".into(), json!(transparency));
        }
        if let Some(unique) = node.unique {
            properties.insert(
                "UniqueId".into(),
                json!({"_type": "UniqueId", "value": format!("{unique:032x}")}),
            );
        }
        if let Some(guid) = node.guid {
            properties.insert("ScriptGuid".into(), json!(format!("{{{guid:08X}}}")));
        }
        if let Some(target) = node.refers {
            let named = ids
                .get(&target)
                .cloned()
                .unwrap_or(format!("gone:{target}"));
            let slot = match node.class {
                "Weld" => "Part1",
                "Camera" => "CameraSubject",
                _ => "Value",
            };
            properties.insert(slot.into(), json!({"_type": "Ref", "settingsId": named}));
        }
        if let Some(engine) = node.engine {
            instance.attributes.insert("RBX_Seen".into(), json!(engine));
        }
        instances.push(instance);
    }
    let camera = nodes.iter().position(|(node, _)| node.viewport).unwrap();
    let camera = instances[camera].settings_id.clone();
    instances[0].properties.insert(
        "CurrentCamera".into(),
        json!({"_type": "Ref", "settingsId": camera}),
    );
    settings_document(instances)
}

/// For each observed instance the reference instance whose id it took.
fn run_sim_pairs(reference: &SettingsBytecode, aligned: &SettingsBytecode) -> Vec<Option<usize>> {
    let ids = (reference.instances.iter().enumerate())
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    (aligned.instances.iter())
        .map(|instance| ids.get(instance.settings_id.as_str()).copied())
        .collect()
}

/// The id of an instance by its label and its place.
type RunSimId = Box<dyn Fn(usize, usize) -> String>;

/// The ids a seed gives the instances of each copy: the reference's are a
/// store's, or bare numbers of their places; Studio's are its own, some
/// handed back by a store, or numbers of their places as well.
fn run_sim_ids(seed: u64) -> (RunSimId, RunSimId) {
    let numbered = |_: usize, index: usize| format!("{index:x}");
    match seed % 5 {
        0 | 3 => (
            Box::new(|label, _| format!("editor:{label}")),
            Box::new(|label, _| format!("debug:0_{label}")),
        ),
        1 => (
            Box::new(|label, _| format!("editor:{label}")),
            Box::new(|label, _| {
                if label % 3 == 0 {
                    format!("editor:{label}")
                } else {
                    format!("debug:0_{label}")
                }
            }),
        ),
        2 => (
            Box::new(|label, _| format!("editor:{label}")),
            Box::new(numbered),
        ),
        _ => (Box::new(numbered), Box::new(numbered)),
    }
}

/// Two copies of a store whose instance counts differ by a run removed or
/// added beside, between or with twins, or inside one, are aligned beside
/// the run only as the matching pairs them, whichever copy is the
/// reference, and whatever else Studio changed: a material, a transparency,
/// whom an instance names, what the matching or a place leaves out, or the
/// viewport camera's subject; or two twins crossed (`run_sim_cross`).
#[test]
fn copies_a_run_apart_are_aligned_beside_it_as_the_matching_pairs_them() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    // Edits of each kind, of a camera, told runs, aligned beside them (of
    // a large group, naming one by its id), and refused.
    let counts: [AtomicUsize; 10] = Default::default();
    let count = |at: usize| counts[at].fetch_add(1, Ordering::Relaxed);
    let seeds = crate::tests::support::run_seeds(2_000, |seed, _| {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut labels = 0;
        let mut files = run_sim_tree(&mut rng, &mut labels);
        let mut studio = files.clone();
        let Some((kind, camera)) = run_sim_edit(&mut rng, &mut labels, &mut studio) else {
            return Ok(());
        };
        for _ in 0..rng.below(3) {
            run_sim_change(&mut rng, &mut studio);
        }
        if rng.below(4) == 0 {
            run_sim_cross(&mut rng, &mut files, &mut studio);
        }
        count(kind);
        if camera {
            count(4);
        }
        let (reference_id, observed_id) = run_sim_ids(seed);
        for (reference, observed) in [(&files, &studio), (&studio, &files)] {
            let reference = run_sim_store(reference, &*reference_id);
            let observed = run_sim_store(observed, &*observed_id);
            let run = crate::settings::bytecode::property_edit::Moved::between_copies(
                &observed.instances,
                &reference.instances,
            )
            .flatten()
            .and_then(|moved| moved.run())
            .filter(|run| run_is_told(run.clone(), &reference.instances, &observed.instances));
            let found = run.is_some();
            let mut by_run = observed.clone();
            let aligned = run.is_some_and(|(run, of_reference)| {
                aligned_beside_run(&reference, &mut by_run, run, !of_reference)
            });
            let mut matched = observed.clone();
            if !align_settings_ids_by_matching(&reference, &mut matched) {
                return Err("the matching aligns no ids".into());
            }
            if aligned {
                let (by_run, by_matching) = (
                    run_sim_pairs(&reference, &by_run),
                    run_sim_pairs(&reference, &matched),
                );
                if let Some(index) = (0..by_run.len()).find(|at| by_run[*at] != by_matching[*at]) {
                    let instance = &observed.instances[index];
                    return Err(format!(
                        "edit {kind}: {} {:?} ({index}) takes {:?} beside the run, {:?} in the matching",
                        instance.class_name, instance.name, by_run[index], by_matching[index]
                    ));
                }
                count(6);
                if observed.instances.len() > 40 {
                    count(8);
                }
                if (observed.instances.iter())
                    .any(|instance| matches!(instance.class_name.as_str(), "ObjectValue" | "Weld"))
                {
                    count(9);
                }
            } else if found {
                count(7);
            }
            if found {
                count(5);
            }
            let mut through = observed.clone();
            if !align_settings_ids_to_reference(&reference, &mut through) {
                return Err("not aligned".into());
            }
        }
        Ok(())
    });
    if std::env::var_os("RENIUM_SIM_SEED").is_some() {
        return;
    }
    let [
        k0,
        k1,
        k2,
        k3,
        of_camera,
        runs,
        beside,
        refused,
        large,
        naming,
    ] = counts.map(AtomicUsize::into_inner);
    // The least counts of 2,000 seeds, in proportion to the seeds run.
    let per = |count: usize| count * seeds as usize / 2_000;
    assert!(
        [k0, k1, k2, k3].iter().all(|count| *count >= per(100))
            && of_camera >= per(80)
            && runs >= per(1_000)
            && beside >= per(500)
            && refused >= per(300)
            && large >= per(100)
            && naming >= per(100),
        "edits {:?}, of a camera {of_camera}, told runs {runs}, aligned beside them {beside} \
         (of a large group {large}, naming one by its id {naming}), refused {refused}",
        [k0, k1, k2, k3]
    );
}

/// The shapes a review found the alignment beside a run pairing otherwise
/// than the matching: two of Studio's twins that hold what the files' one
/// holds, a twin that holds what none does, twins a reference tells apart
/// alone or otherwise than what they hold, and twins alike without
/// children; and twins the run leaves as they stood that a reference names
/// otherwise, and instances Studio renamed under the bare numbers the files
/// hold them by.
/// Each is left to the matching.
#[test]
fn twins_the_matching_pairs_otherwise_than_their_places_are_left_to_it() {
    let store = |rows: &[(&str, &str, Option<usize>, &str)]| {
        let mut instances = Vec::new();
        for (index, (name, class, parent, material)) in rows.iter().enumerate() {
            let mut instance = settings_instance(format!("i{index}"), *name, *class, *parent);
            if let Some(target) = material.strip_prefix("->") {
                instance.properties.insert(
                    "Value".into(),
                    json!({"_type": "Ref", "settingsId": target}),
                );
            } else if !material.is_empty() {
                instance
                    .properties
                    .insert("Material".into(), json!(material));
            }
            instances.push(instance);
        }
        settings_document(instances)
    };
    let renumbered = |mut document: SettingsBytecode| {
        for instance in &mut document.instances {
            instance.settings_id = format!("debug:{}", instance.settings_id);
        }
        for instance in &mut document.instances {
            if let Some(Value::Object(value)) = instance.properties.get_mut("Value") {
                let target = value["settingsId"].as_str().unwrap().to_string();
                value.insert("settingsId".into(), json!(format!("debug:{target}")));
            }
        }
        document
    };
    let shapes = [
        (
            store(&[
                ("Workspace", "Workspace", None, ""),
                ("A", "Part", Some(0), ""),
                ("D", "Part", Some(1), "Wood"),
            ]),
            store(&[
                ("Workspace", "Workspace", None, ""),
                ("X", "Folder", Some(0), ""),
                ("A", "Part", Some(0), ""),
                ("D", "Part", Some(2), "Wood"),
                ("A", "Part", Some(0), ""),
                ("D", "Part", Some(4), "Neon"),
            ]),
        ),
        (
            store(&[
                ("Workspace", "Workspace", None, ""),
                ("X", "Folder", Some(0), ""),
                ("A", "Part", Some(0), "Wood"),
                ("A", "Part", Some(0), "Neon"),
            ]),
            store(&[
                ("Workspace", "Workspace", None, ""),
                ("A", "Part", Some(0), "Glass"),
            ]),
        ),
        (
            store(&[
                ("Workspace", "Workspace", None, ""),
                ("X", "Folder", Some(0), ""),
                ("A", "Part", Some(0), "Wood"),
                ("A", "Part", Some(0), "Wood"),
                ("V", "ObjectValue", Some(0), "->i2"),
            ]),
            store(&[
                ("Workspace", "Workspace", None, ""),
                ("A", "Part", Some(0), "Wood"),
                ("V", "ObjectValue", Some(0), "->i1"),
            ]),
        ),
        (
            store(&[
                ("Workspace", "Workspace", None, ""),
                ("X", "Folder", Some(0), ""),
                ("A", "Part", Some(0), "Wood"),
                ("A", "Part", Some(0), "Neon"),
                ("V", "ObjectValue", Some(0), "->i2"),
            ]),
            store(&[
                ("Workspace", "Workspace", None, ""),
                ("A", "Part", Some(0), "Neon"),
                ("V", "ObjectValue", Some(0), "->i1"),
            ]),
        ),
        (
            store(&[
                ("Workspace", "Workspace", None, ""),
                ("A", "Part", Some(0), "Wood"),
            ]),
            store(&[
                ("Workspace", "Workspace", None, ""),
                ("X", "Folder", Some(0), ""),
                ("A", "Part", Some(0), "Wood"),
                ("A", "Part", Some(0), "Wood"),
            ]),
        ),
        (
            store(&[
                ("Workspace", "Workspace", None, ""),
                ("X", "Folder", Some(0), ""),
                ("A", "Part", Some(0), "Wood"),
                ("A", "Part", Some(0), "Wood"),
                ("V", "ObjectValue", Some(0), "->i2"),
            ]),
            store(&[
                ("Workspace", "Workspace", None, ""),
                ("A", "Part", Some(0), "Wood"),
                ("A", "Part", Some(0), "Wood"),
                ("V", "ObjectValue", Some(0), "->i2"),
            ]),
        ),
    ];
    let mut files = store(&[
        ("Workspace", "Workspace", None, ""),
        ("X", "Folder", Some(0), ""),
        ("A", "Part", Some(0), "Wood"),
        ("B", "Part", Some(0), "Neon"),
    ]);
    for (index, instance) in files.instances.iter_mut().enumerate() {
        instance.settings_id = format!("{index:x}");
    }
    let mut renamed = renumbered(store(&[
        ("Workspace", "Workspace", None, ""),
        ("A", "Part", Some(0), "Neon"),
        ("B", "Part", Some(0), "Wood"),
    ]));
    renamed.instances[1].settings_id = "3".into();
    renamed.instances[2].settings_id = "2".into();
    let shapes = (shapes.into_iter())
        .map(|(files, studio)| (files, renumbered(studio)))
        .chain([(files, renamed)]);
    for (shape, (files, studio)) in shapes.enumerate() {
        let run = crate::settings::bytecode::property_edit::Moved::between_copies(
            &studio.instances,
            &files.instances,
        )
        .flatten()
        .and_then(|moved| moved.run())
        .filter(|run| run_is_told(run.clone(), &files.instances, &studio.instances))
        .unwrap_or_else(|| panic!("shape {shape}: no run"));
        let placed = Placed {
            start: run.0.start,
            len: run.0.len(),
        };
        let mut matched = studio.clone();
        assert!(align_settings_ids_by_matching(&files, &mut matched));
        let place = (0..studio.instances.len())
            .map(|index| placed.counterpart(index, !run.1))
            .collect::<Vec<_>>();
        assert!(
            run_sim_pairs(&files, &matched) != place,
            "shape {shape}: the matching pairs the twins by their places"
        );
        let mut beside = studio.clone();
        assert!(
            !aligned_beside_run(&files, &mut beside, run.0, !run.1),
            "shape {shape}: aligned beside the run"
        );
        let mut aligned = studio.clone();
        assert!(align_settings_ids_to_reference(&files, &mut aligned));
        assert_eq!(
            run_sim_pairs(&files, &aligned),
            run_sim_pairs(&files, &matched),
            "shape {shape}"
        );
    }
}

/// A stored camera that holds an enum item without its type makes the answer
/// follow the camera, so a pull of such a store is not remembered.
#[test]
fn a_stored_camera_without_an_enum_type_makes_the_answer_follow_the_camera() {
    let untyped = json!({"items":[{"_type":"EnumItem", "name":"Plastic"}]});
    let typed = json!({"items":[{"_type":"EnumItem", "name":"Plastic", "enumType":"Material"}]});
    let mut instances = workspace_with_camera("ws", "cam");
    instances[1].properties.insert("Mode".into(), untyped);
    instances.push(settings_instance("part", "Part", "Part", Some(0)));
    let reference = settings_document(instances);
    let mut moved = with_moved_camera(&reference);
    moved.instances[1]
        .properties
        .insert("Mode".into(), typed.clone());
    let mut still = reference.clone();
    still.instances[1].properties.insert("Mode".into(), typed);
    assert!(pull_digest(&moved) == pull_digest(&still));
    assert!(matches!(
        pulled(&reference, &moved),
        SettingsAlignment::Equivalent
    ));
    assert!(matches!(
        pulled(&reference, &still),
        SettingsAlignment::Changed(_)
    ));
    assert!(viewport_camera_lacks_enum_types(&reference));
    assert!(!viewport_camera_lacks_enum_types(&still));
    assert!(!viewport_camera_lacks_enum_types(&settings_document(
        workspace_with_camera("ws", "cam")
    )));
    // An enum property of the camera itself never gains a type.
    let mut plain = workspace_with_camera("ws", "cam");
    plain[1].properties.insert(
        "CameraType".into(),
        json!({"_type":"EnumItem", "name":"Fixed"}),
    );
    assert!(!viewport_camera_lacks_enum_types(&settings_document(plain)));
}
