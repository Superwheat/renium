//! What a pull of the instances Studio named values of takes from their
//! capture, on stores encoded as a pull writes them.

use super::*;
use crate::roblox::schema::{
    PropertySchemaEntry, PropertySchemaMap, TYPE_ID_BOOL, TYPE_ID_CFRAME, TYPE_ID_NUMBER,
    TYPE_ID_REF, TYPE_ID_VECTOR3,
};
use crate::tests::support::{settings_document, settings_instance};

fn row(
    id: &str,
    name: &str,
    class: &str,
    parent: Option<usize>,
    values: Value,
) -> SettingsBytecodeInstance {
    let mut row = settings_instance(id, name, class, parent);
    row.properties = values.as_object().cloned().unwrap_or_default();
    row
}

fn unique_id(value: &str) -> Value {
    json!({"_type": "UniqueId", "value": value})
}

/// A Workspace with its viewport camera, two parts under a folder, values
/// under the first, a decal under the second, and an animation.
fn place() -> SettingsBytecode {
    settings_document(vec![
        row(
            "ws",
            "Workspace",
            "Workspace",
            None,
            json!({"CurrentCamera": {"_type": "Ref", "settingsId": "cam"}}),
        ),
        row(
            "cam",
            "Camera",
            "Camera",
            Some(0),
            json!({"FieldOfView": 70.0}),
        ),
        row("model", "Model", "Folder", Some(0), json!({})),
        row(
            "p1",
            "P1",
            "Part",
            Some(2),
            json!({"Transparency": 0.0, "Anchored": true, "Size": [4.0, 1.0, 2.0], "UniqueId": unique_id("u1")}),
        ),
        row("tag", "Tag", "StringValue", Some(3), json!({"Value": "a"})),
        row(
            "link",
            "Link",
            "ObjectValue",
            Some(3),
            json!({"Value": {"_type": "Ref", "settingsId": "p2"}}),
        ),
        row(
            "p2",
            "P2",
            "Part",
            Some(2),
            json!({"Transparency": 0.0, "UniqueId": unique_id("u2")}),
        ),
        row(
            "anim",
            "Anim",
            "Animation",
            Some(2),
            json!({"AnimationContent": "rbxassetid://1"}),
        ),
        row(
            "decal",
            "Decal",
            "Decal",
            Some(6),
            json!({"TextureContent": "rbxassetid://2", "ColorMapContent": "rbxassetid://2", "Transparency": 0.0}),
        ),
    ])
}

fn edited(edit: impl FnOnce(&mut SettingsBytecode)) -> SettingsBytecode {
    let mut place = place();
    edit(&mut place);
    place
}

fn instance<'d>(document: &'d mut SettingsBytecode, id: &str) -> &'d mut SettingsBytecodeInstance {
    (document.instances.iter_mut())
        .find(|instance| instance.settings_id == id)
        .unwrap()
}

fn set(document: &mut SettingsBytecode, id: &str, name: &str, value: Value) {
    instance(document, id).properties.insert(name.into(), value);
}

fn encoded(document: &SettingsBytecode) -> Vec<u8> {
    encode_settings_bytecode(document).unwrap()
}

/// Studio's touch of the values `properties` of the instance `id` at `path`.
fn touched(id: &str, path: &[&str], properties: &[&str]) -> TouchedInstance {
    TouchedInstance {
        path_segments: path.iter().map(|name| name.to_string()).collect(),
        path_ordinals: vec![1; path.len()],
        kind: "values".into(),
        fields: Some(TouchedFields {
            debug_id: id.into(),
            properties: properties.iter().map(|name| name.to_string()).collect(),
            attributes: false,
            tags: false,
        }),
    }
}

const P1: [&str; 3] = ["Workspace", "Model", "P1"];
const P2: [&str; 3] = ["Workspace", "Model", "P2"];
const TAG: [&str; 4] = ["Workspace", "Model", "P1", "Tag"];
const LINK: [&str; 4] = ["Workspace", "Model", "P1", "Link"];

/// The plugin's schema of `classes`, each with its properties and types.
fn plugin_schema(classes: &[(&str, &[(&str, u8)])]) -> PropertySchemaMap {
    (classes.iter())
        .map(|(class, properties)| {
            let entries = (properties.iter())
                .map(|(name, type_id)| PropertySchemaEntry {
                    name: name.to_string(),
                    type_id: *type_id,
                    enum_type: None,
                })
                .collect();
            (class.to_string(), entries)
        })
        .collect()
}

/// The plugin reads every reference, and a model's pivot.
fn schemas() -> PropertySchemaMap {
    plugin_schema(&[
        ("ObjectValue", &[("Value", TYPE_ID_REF)]),
        ("Model", &[("WorldPivot", TYPE_ID_CFRAME)]),
        (
            "Part",
            &[("Transparency", TYPE_ID_NUMBER), ("Anchored", TYPE_ID_BOOL)],
        ),
    ])
}

/// What a capture of Studio as `studio` holds takes at the instance `id`
/// where the plugin's schema is `schemas`: it and the instances under it,
/// breadth first, without the references a native capture leaves out.
fn captured_with(
    studio: &SettingsBytecode,
    id: &str,
    schemas: &PropertySchemaMap,
) -> Vec<CapturedInstance> {
    let rows = &studio.instances;
    let top = (0..rows.len())
        .find(|row| rows[*row].settings_id == id)
        .unwrap();
    let mut order = vec![(top, None)];
    let mut at = 0;
    while let Some(&(row, _)) = order.get(at) {
        let children = (0..rows.len()).filter(|child| rows[*child].parent_index == Some(row));
        order.extend(children.map(|child| (child, Some(at))));
        at += 1;
    }
    let database = rbx_reflection_database::get().unwrap();
    let classes = order.iter().map(|(row, _)| rows[*row].class_name.as_str());
    let conversions = class_conversions(database, schemas, classes);
    (order.into_iter())
        .map(|(row, parent)| {
            let row = &rows[row];
            let mut properties = row.properties.clone();
            properties.retain(|_, value| {
                !crate::settings::equivalence::reconciliation_value_contains_reference(value)
            });
            CapturedInstance {
                name: row.name.clone(),
                class_name: row.class_name.clone(),
                parent,
                values: CapturedValues {
                    properties,
                    attributes: row.attributes.clone(),
                    overlaid: conversions[&row.class_name].overlaid.clone(),
                },
            }
        })
        .collect()
}

fn captured_of(studio: &SettingsBytecode, id: &str) -> Vec<CapturedInstance> {
    captured_with(studio, id, &schemas())
}

fn pull_with(
    held: &SettingsBytecode,
    studio: &SettingsBytecode,
    entries: &[TouchedInstance],
    schemas: &PropertySchemaMap,
) -> std::result::Result<Option<Vec<u8>>, String> {
    let mut plan = StoreInstancePull::plan("Workspace", &encoded(held), entries)?;
    let captured = (plan.roots.iter())
        .map(|root| captured_with(studio, &root.debug_id, schemas))
        .collect::<Vec<_>>();
    plan.apply(&captured)
}

fn pull_over(
    held: &SettingsBytecode,
    studio: &SettingsBytecode,
    entries: &[TouchedInstance],
) -> std::result::Result<Option<Vec<u8>>, String> {
    pull_with(held, studio, entries, &schemas())
}

fn pull(
    studio: &SettingsBytecode,
    entries: &[TouchedInstance],
) -> std::result::Result<Option<Vec<u8>>, String> {
    pull_over(&place(), studio, entries)
}

#[test]
fn named_values_are_written_as_a_whole_pull_writes_them() {
    let studio = edited(|place| {
        set(place, "p1", "Transparency", json!(0.5));
        set(place, "p2", "Transparency", json!(0.25));
    });
    let entries = [
        touched("p1", &P1, &["Transparency"]),
        touched("p2", &P2, &["Transparency"]),
    ];
    assert_eq!(pull(&studio, &entries), Ok(Some(encoded(&studio))));
    assert_eq!(pull(&place(), &entries), Ok(None));
    // A touched instance under another is captured with it.
    let studio = edited(|place| {
        set(place, "p1", "Transparency", json!(0.5));
        set(place, "tag", "Value", json!("b"));
    });
    let entries = [
        touched("tag", &TAG, &["Value"]),
        touched("p1", &P1, &["Transparency"]),
    ];
    let plan = StoreInstancePull::plan("Workspace", &encoded(&place()), &entries).unwrap();
    assert_eq!(plan.roots.len(), 1);
    assert_eq!(plan.roots[0].debug_id, "p1");
    assert_eq!(pull(&studio, &entries), Ok(Some(encoded(&studio))));
}

#[test]
fn a_difference_studio_did_not_name_falls_back() {
    let p1 = [touched("p1", &P1, &["Transparency"])];
    let studio = edited(|place| {
        set(place, "p1", "Transparency", json!(0.5));
        set(place, "p1", "Anchored", json!(false));
    });
    assert_eq!(
        pull(&studio, &p1),
        Err("Anchored differs where Studio named no change".into())
    );
    let studio = edited(|place| {
        set(place, "p1", "Transparency", json!(0.5));
        set(place, "tag", "Value", json!("b"));
    });
    assert_eq!(
        pull(&studio, &p1),
        Err("Value differs where Studio named no change".into())
    );
    // A UniqueId is never Studio's to change.
    let studio = edited(|place| set(place, "p1", "UniqueId", unique_id("u9")));
    assert_eq!(
        pull(&studio, &[touched("p1", &P1, &["UniqueId"])]),
        Err("UniqueId differs where Studio named no change".into())
    );
    // An instance Studio added under a touched one.
    let studio = edited(|place| {
        set(place, "p1", "Transparency", json!(0.5));
        (place.instances).push(row("new", "New", "Folder", Some(3), json!({})));
    });
    assert_eq!(
        pull(&studio, &p1),
        Err("a captured instance holds other children than stored".into())
    );
    // An instance Studio removed from under a touched one.
    let studio = edited(|place| {
        set(place, "p1", "Transparency", json!(0.5));
        place.instances.remove(4);
        for instance in &mut place.instances {
            if let Some(parent) = instance.parent_index.filter(|parent| *parent > 4) {
                instance.parent_index = Some(parent - 1);
            }
        }
    });
    assert_eq!(
        pull(&studio, &p1),
        Err("a captured instance holds other children than stored".into())
    );
}

#[test]
fn a_reference_stays_as_held_unless_studio_named_it() {
    let studio = edited(|place| set(place, "p1", "Transparency", json!(0.5)));
    assert_eq!(
        pull(&studio, &[touched("p1", &P1, &["Transparency"])]),
        Ok(Some(encoded(&studio)))
    );
    let studio = edited(|place| {
        set(
            place,
            "link",
            "Value",
            json!({"_type": "Ref", "settingsId": "p1"}),
        );
    });
    assert_eq!(
        pull(&studio, &[touched("link", &LINK, &["Value"])]),
        Err("Value changed, which the capture does not hold".into())
    );
    // One the plugin does not read, which the capture leaves out.
    let unread = plugin_schema(&[("Part", &[("Transparency", TYPE_ID_NUMBER)])]);
    assert_eq!(
        pull_with(
            &place(),
            &studio,
            &[touched("link", &LINK, &["Value"])],
            &unread
        ),
        Err("Value changed, which the capture does not hold".into())
    );
    // A reference set where none was: the capture holds it on neither side.
    let empty = |value: Option<&str>| {
        edited(|place| {
            let mut empty = row("empty", "Empty", "ObjectValue", Some(3), json!({}));
            if let Some(target) = value {
                (empty.properties).insert(
                    "Value".into(),
                    json!({"_type": "Ref", "settingsId": target}),
                );
            }
            place.instances.push(empty);
        })
    };
    let path = ["Workspace", "Model", "P1", "Empty"];
    assert_eq!(
        pull_over(
            &empty(None),
            &empty(Some("p2")),
            &[touched("empty", &path, &["Value"])]
        ),
        Err("Value changed, which the capture does not hold".into())
    );
}

#[test]
fn a_property_studio_names_another_way_is_named() {
    let studio = edited(|place| {
        set(place, "p1", "Size", json!([8.0, 1.0, 2.0]));
        set(place, "anim", "AnimationContent", json!("rbxassetid://3"));
        set(place, "decal", "TextureContent", json!("rbxassetid://4"));
        set(place, "decal", "ColorMapContent", json!("rbxassetid://4"));
    });
    let decal = ["Workspace", "Model", "P2", "Decal"];
    let entries = [
        touched("p1", &P1, &["size"]),
        touched("anim", &["Workspace", "Model", "Anim"], &["AnimationId"]),
        touched("decal", &decal, &["TextureContent"]),
    ];
    assert_eq!(pull(&studio, &entries), Ok(Some(encoded(&studio))));
    assert_eq!(
        pull(&studio, &[touched("decal", &decal, &["Transparency"])]),
        Err("ColorMapContent differs where Studio named no change".into())
    );
    let shot = |frame: f64| {
        edited(|place| {
            let frame = json!([frame, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]);
            (place.instances).push(row(
                "shot",
                "Shot",
                "Camera",
                Some(2),
                json!({"CFrame": frame}),
            ));
        })
    };
    let entries = [touched(
        "shot",
        &["Workspace", "Model", "Shot"],
        &["CoordinateFrame"],
    )];
    assert_eq!(
        pull_over(&shot(0.0), &shot(5.0), &entries),
        Ok(Some(encoded(&shot(5.0))))
    );
    // The same value under the name it goes by today is no change to a pull.
    let held = edited(|place| {
        let size = instance(place, "p1").properties.remove("Size").unwrap();
        set(place, "p1", "size", size);
    });
    assert_eq!(
        pull_over(&held, &place(), &[touched("p1", &P1, &["Size"])]),
        Err("a pull counts the change as none".into())
    );
}

/// Studio announces a model's pivot as WorldPivotData, which a capture
/// stores as WorldPivot: from the bytes where the plugin's schema of models
/// leaves it out, from the plugin's reading where the schema names it.
#[test]
fn a_models_pivot_is_named_as_studio_announces_it() {
    let pivot = |x: f64| json!([x, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]);
    let rig = |x: f64| {
        edited(|place| {
            let model = row(
                "rig",
                "Rig",
                "Model",
                Some(0),
                json!({"WorldPivot": pivot(x)}),
            );
            place.instances.push(model);
        })
    };
    let moved = [touched("rig", &["Workspace", "Rig"], &["WorldPivotData"])];
    assert_eq!(
        pull_with(&rig(0.0), &rig(5.0), &moved, &PropertySchemaMap::new()),
        Ok(Some(encoded(&rig(5.0))))
    );
    assert_eq!(
        pull_over(&rig(0.0), &rig(5.0), &moved),
        Err("WorldPivot changed, which the capture does not hold".into())
    );
    // Unnamed, what the plugin reads is as held whatever the bytes say.
    let mut attributed = touched("rig", &["Workspace", "Rig"], &[]);
    attributed.fields.as_mut().unwrap().attributes = true;
    assert_eq!(pull_over(&rig(0.0), &rig(5.0), &[attributed]), Ok(None));
}

/// A capture converts a class with its native filter where the plugin's
/// schema names a property of it, and as the bytes have it where not.
#[cfg(windows)]
#[test]
fn a_capture_converts_a_class_as_the_plugins_schema_of_it_says() {
    use rbx_dom_weak::types::{CFrame, Matrix3, Ref, Variant, Vector3};
    let pivot = CFrame::new(Vector3::new(5.0, 0.0, 0.0), Matrix3::identity());
    let model = rbx_binary::FlatInstance {
        referent: Ref::some(1),
        parent_index: None,
        name: "Rig".into(),
        class: "Model".into(),
        properties: vec![(
            rbx_dom_weak::Ustr::from("WorldPivotData"),
            Variant::OptionalCFrame(Some(pivot)),
        )],
    };
    let subtree = vec![(model, None)];
    let database = rbx_reflection_database::get().unwrap();
    let names = |schemas: &PropertySchemaMap| {
        let conversions = class_conversions(database, schemas, ["Model"]);
        let captured = captured_instances("Workspace", &subtree, &conversions).unwrap();
        let properties = &captured[0].values.properties;
        (
            properties.keys().cloned().collect::<Vec<_>>(),
            captured[0].values.overlaid.clone(),
        )
    };
    let (read, overlaid) = names(&schemas());
    assert!(read.contains(&"WorldPivot".to_string()), "{read:?}");
    assert!(overlaid.contains("WorldPivot"));
    let (plain, overlaid) = names(&PropertySchemaMap::new());
    assert!(plain.contains(&"WorldPivotData".to_string()), "{plain:?}");
    assert!(overlaid.is_empty());
}

/// A field the native bytes alone carry (a part's deprecated Velocity) is
/// compared like any other: only what the plugin's schema names and its
/// reading supplies stays as held.
#[test]
fn only_what_the_plugin_reads_stays_as_held() {
    let velocity = |y: f64, transparency: f64| {
        edited(|place| {
            set(place, "p1", "Velocity", json!([0.0, y, 0.0]));
            set(place, "p1", "Transparency", json!(transparency));
        })
    };
    let entries = [touched(
        "p1",
        &P1,
        &["Transparency", "AssemblyLinearVelocity"],
    )];
    assert_eq!(
        pull_over(&velocity(0.0, 0.0), &velocity(5.0, 0.5), &entries),
        Err("Velocity differs where Studio named no change".into())
    );
    let read = plugin_schema(&[(
        "Part",
        &[
            ("Transparency", TYPE_ID_NUMBER),
            ("Velocity", TYPE_ID_VECTOR3),
        ],
    )]);
    assert_eq!(
        pull_with(&velocity(0.0, 0.0), &velocity(5.0, 0.5), &entries, &read),
        Ok(Some(encoded(&velocity(0.0, 0.5))))
    );
    let named = [touched("p1", &P1, &["Velocity"])];
    assert_eq!(
        pull_with(&velocity(0.0, 0.0), &velocity(5.0, 0.0), &named, &read),
        Err("Velocity changed, which the capture does not hold".into())
    );
}

/// What Studio names after the proof and before the check is of the
/// capture too: a reference it names there is not taken as held.
#[test]
fn a_name_studio_gives_after_the_proof_reaches_the_capture() {
    let studio = edited(|place| {
        set(place, "p1", "Transparency", json!(0.5));
        set(
            place,
            "link",
            "Value",
            json!({"_type": "Ref", "settingsId": "p1"}),
        );
    });
    let entries = [touched("p1", &P1, &["Transparency"])];
    let plan = || StoreInstancePull::plan("Workspace", &encoded(&place()), &entries).unwrap();
    let mut since = plan();
    assert_eq!(
        since.name_since(&[touched("link", &LINK, &["Value"])]),
        Ok(())
    );
    assert_eq!(
        since.apply(&[captured_of(&studio, "p1")]),
        Err("Value changed, which the capture does not hold".into())
    );
    let mut since = plan();
    let tag = edited(|place| {
        set(place, "p1", "Transparency", json!(0.5));
        set(place, "tag", "Value", json!("b"));
    });
    assert_eq!(
        since.name_since(&[touched("tag", &TAG, &["Value"])]),
        Ok(())
    );
    assert_eq!(
        since.apply(&[captured_of(&tag, "p1")]),
        Ok(Some(encoded(&tag)))
    );
    let mut since = plan();
    let both = edited(|place| {
        set(place, "p1", "Transparency", json!(0.5));
        set(place, "p1", "Anchored", json!(false));
    });
    assert_eq!(
        since.name_since(&[touched("p1", &P1, &["Anchored"])]),
        Ok(())
    );
    assert_eq!(
        since.apply(&[captured_of(&both, "p1")]),
        Ok(Some(encoded(&both)))
    );
    let camera = touched("cam", &["Workspace", "Camera"], &["FieldOfView"]);
    assert_eq!(plan().name_since(&[camera]), Ok(()));
    assert_eq!(
        plan().name_since(&[touched("p2", &P2, &["Transparency"])]),
        Err("a change since the proof is of no captured instance".into())
    );
}

/// Studio's live failures: a part welded to another, the anchored part of
/// that weld, and a Motor6D's offset each moved a part outside the touched
/// one with no event. Named, the pull writes the touched row alone and a
/// whole capture differs; the plugin names nothing of such an instance, and
/// the pull falls back.
#[test]
fn a_change_studio_makes_beside_the_roots_is_left_to_a_whole_pull() {
    let frame = |x: f64| json!([x, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]);
    let rig = |edit: &dyn Fn(&mut SettingsBytecode)| {
        edited(|place| {
            (place.instances).extend([
                row(
                    "hub",
                    "Hub",
                    "Part",
                    Some(0),
                    json!({"Anchored": true, "CFrame": frame(0.0)}),
                ),
                row(
                    "wheel",
                    "Wheel",
                    "Part",
                    Some(0),
                    json!({"CFrame": frame(1.0)}),
                ),
                row("weld", "Weld", "WeldConstraint", Some(0), json!({})),
                row(
                    "motor",
                    "Motor",
                    "Motor6D",
                    Some(0),
                    json!({"C0": frame(0.0)}),
                ),
            ]);
            edit(place);
        })
    };
    let held = rig(&|_| {});
    for (id, name, property, carried) in [
        ("wheel", "Wheel", "CFrame", "hub"),
        ("hub", "Hub", "CFrame", "wheel"),
        ("motor", "Motor", "C0", "wheel"),
    ] {
        let studio = rig(&|place| {
            set(place, id, property, frame(3.0));
            set(place, carried, "CFrame", frame(4.0));
        });
        let named = touched(id, &["Workspace", name], &[property]);
        let written = pull_over(&held, &studio, &[named]).unwrap().unwrap();
        assert_ne!(written, encoded(&studio), "{id}: the joint moved {carried}");
        let mut sent = touched(id, &["Workspace", name], &[]);
        sent.fields = None;
        assert_eq!(
            pull_over(&held, &studio, &[sent]),
            Err("an instance changed that Studio named nothing of".into())
        );
    }
}

/// A store a pull aligned holds some rows as an older store held them, in a
/// form a pull counts as the captured one. Outside the roots they stay, and
/// a whole pull leaves the written store as it is; under a root such a row
/// differs from its capture where Studio named nothing.
#[test]
fn a_store_a_pull_aligned_is_pulled_as_any_other() {
    let aligned = |edit: &dyn Fn(&mut SettingsBytecode)| {
        edited(|place| {
            set(
                place,
                "tag",
                "Value",
                json!({"_type": "Content", "value": "a"}),
            );
            edit(place);
        })
    };
    let studio = edited(|place| set(place, "p2", "Transparency", json!(0.5)));
    let written = pull_over(
        &aligned(&|_| {}),
        &studio,
        &[touched("p2", &P2, &["Transparency"])],
    );
    let written = written.unwrap().unwrap();
    assert_eq!(
        written,
        encoded(&aligned(&|place| set(
            place,
            "p2",
            "Transparency",
            json!(0.5)
        )))
    );
    assert!(matches!(
        crate::settings::equivalence::align_pulled_settings_bytes_to_reference(
            &written,
            &encoded(&studio)
        ),
        Ok(crate::settings::equivalence::SettingsAlignment::Equivalent)
    ));
    let studio = edited(|place| set(place, "p1", "Transparency", json!(0.5)));
    assert_eq!(
        pull_over(
            &aligned(&|_| {}),
            &studio,
            &[touched("p1", &P1, &["Transparency"])]
        ),
        Err("Value differs where Studio named no change".into())
    );
}

#[test]
fn attributes_and_tags_change_where_studio_names_them() {
    let flagged = |attributes: bool, tags: bool| {
        let mut entry = touched("p1", &P1, &[]);
        let fields = entry.fields.as_mut().unwrap();
        (fields.attributes, fields.tags) = (attributes, tags);
        [entry]
    };
    let studio = edited(|place| {
        (instance(place, "p1").attributes).insert("Speed".into(), json!(2));
    });
    assert_eq!(
        pull(&studio, &flagged(false, true)),
        Err("attributes differ where Studio named no change".into())
    );
    assert_eq!(
        pull(&studio, &flagged(true, false)),
        Ok(Some(encoded(&studio)))
    );
    let studio = edited(|place| set(place, "p1", "Tags", json!(["Lava"])));
    assert_eq!(
        pull(&studio, &flagged(true, false)),
        Err("Tags differs where Studio named no change".into())
    );
    assert_eq!(
        pull(&studio, &flagged(false, true)),
        Ok(Some(encoded(&studio)))
    );
    let studio = edited(|place| {
        (instance(place, "p1").attributes)
            .insert("Target".into(), json!({"_type": "Ref", "settingsId": "p2"}));
    });
    assert_eq!(
        pull(&studio, &flagged(true, false)),
        Err("a changed attribute holds a reference".into())
    );
}

#[test]
fn namesakes_are_told_apart_by_their_unique_ids() {
    let twin = |place: &mut SettingsBytecode| {
        (place.instances).push(row(
            "twin",
            "P1",
            "Part",
            Some(2),
            json!({"Transparency": 0.0, "UniqueId": unique_id("u3")}),
        ));
    };
    let held = edited(twin);
    let studio = edited(|place| {
        twin(place);
        set(place, "twin", "Transparency", json!(0.5));
    });
    let entries = [touched("twin", &P1, &["Transparency"])];
    assert_eq!(
        pull_over(&held, &studio, &entries),
        Ok(Some(encoded(&studio)))
    );
    let without = |document: &SettingsBytecode| {
        let mut document = document.clone();
        for instance in &mut document.instances {
            instance.properties.remove("UniqueId");
        }
        document
    };
    assert_eq!(
        pull_over(&without(&held), &without(&studio), &entries),
        Err("a captured root is none of the stored instances at its path".into())
    );
    // Two touched namesakes Studio holds as one instance.
    let mut second = touched("twin", &P1, &["Transparency"]);
    second.path_ordinals[2] = 2;
    let first = touched("twin", &P1, &["Transparency"]);
    let mut plan = StoreInstancePull::plan("Workspace", &encoded(&held), &[first, second]).unwrap();
    let captured = [captured_of(&studio, "twin"), captured_of(&studio, "twin")];
    assert_eq!(
        plan.apply(&captured),
        Err("two captured instances are one stored instance".into())
    );
    // Captured under a touched folder, both namesakes stand at the path, and
    // the names do not say which one changed.
    let entries = [
        touched("model", &["Workspace", "Model"], &[]),
        touched("twin", &P1, &["Transparency"]),
    ];
    assert_eq!(
        pull_over(&held, &studio, &entries),
        Err("a touched instance is not told from its namesakes".into())
    );
}

#[test]
fn a_touch_a_pull_of_instances_cannot_take_falls_back() {
    let roots = |held: &SettingsBytecode, entries: &[TouchedInstance]| {
        StoreInstancePull::plan("Workspace", &encoded(held), entries).map(|plan| plan.roots.len())
    };
    let place = place();
    // A pull keeps the stored viewport camera, and so does this one.
    let camera = touched("cam", &["Workspace", "Camera"], &["FieldOfView"]);
    assert_eq!(roots(&place, &[camera]), Ok(0));
    assert_eq!(pull(&place, &[]), Ok(None));
    for (entry, reason) in [
        (touched("p1", &P1, &["Name"]), "Name changed"),
        (touched("p1", &P1, &["source"]), "source changed"),
        (touched("p1", &P1, &["Archivable"]), "Archivable changed"),
        (
            touched("lock", &["Workspace", "__Renium_SessionLock"], &[]),
            "the session lock changed",
        ),
        (
            touched("ws", &["Workspace"], &["Gravity"]),
            "the service itself changed",
        ),
        (
            touched("x", &["Workspace", "Gone"], &["Value"]),
            "a touched instance is none the store holds",
        ),
    ] {
        assert_eq!(roots(&place, &[entry]), Err(reason.into()));
    }
    let mut unnamed = touched("p1", &P1, &[]);
    unnamed.fields = None;
    assert_eq!(
        roots(&place, &[unnamed]),
        Err("an instance changed that Studio named nothing of".into())
    );
    assert_eq!(
        roots(&place, &[touched("p1", &P1, &[]), touched("p1", &P1, &[])]),
        Err("two touched instances stand at one path".into())
    );
    assert_eq!(
        StoreInstancePull::plan("MaterialService", &encoded(&place), &[]).err(),
        Some("MaterialService keeps a stored value no capture holds".into())
    );
    let model = [touched("model", &["Workspace", "Model"], &[])];
    let camera_inside = edited(|place| {
        set(
            place,
            "ws",
            "CurrentCamera",
            json!({"_type": "Ref", "settingsId": "view"}),
        );
        (place.instances).push(row("view", "View", "Camera", Some(2), json!({})));
    });
    assert_eq!(
        roots(&camera_inside, &model),
        Err("a Camera stands under a touched instance".into())
    );
    let terrain = edited(|place| {
        (place.instances).push(row("terrain", "Terrain", "Terrain", Some(0), json!({})));
    });
    assert_eq!(
        roots(
            &terrain,
            &[touched(
                "terrain",
                &["Workspace", "Terrain"],
                &["WaterColor"]
            )]
        ),
        Err("a Terrain stands under a touched instance".into())
    );
    // The folder and the six instances under it, and as many more as reach
    // the limit; one more is past it.
    let crowded = |extra: usize| {
        edited(|place| {
            for index in 0..extra {
                let name = format!("V{index}");
                (place.instances).push(row(&name, &name, "Folder", Some(2), json!({})));
            }
        })
    };
    assert_eq!(roots(&crowded(INSTANCE_PULL_ROWS - 7), &model), Ok(1));
    assert_eq!(
        roots(&crowded(INSTANCE_PULL_ROWS - 6), &model),
        Err(format!(
            "more than {INSTANCE_PULL_ROWS} stored instances stand under the touched ones"
        ))
    );
}

#[test]
fn a_change_since_the_proof_is_inside_only_under_what_the_pull_captures() {
    let plan = StoreInstancePull::plan(
        "Workspace",
        &encoded(&place()),
        &[touched("p1", &P1, &["Transparency"])],
    )
    .unwrap();
    let pulls = BTreeMap::from([("Workspace".to_string(), &plan)]);
    let premise = |services: &[&str], entries: Option<Vec<TouchedInstance>>| {
        let now = ProofScope::Services {
            services: services.iter().map(|service| service.to_string()).collect(),
            touched: BTreeMap::from([("Workspace".to_string(), entries)]),
            values_only: BTreeSet::new(),
        };
        instance_premise(&now, &pulls)
    };
    let inside = |entry: TouchedInstance| premise(&["Workspace"], Some(vec![entry]));
    assert_eq!(
        inside(touched("p1", &P1, &["Anchored"])),
        ScopedPremise::Inside
    );
    assert_eq!(
        inside(touched("tag", &TAG, &["Value"])),
        ScopedPremise::Inside
    );
    let camera = touched("cam", &["Workspace", "Camera"], &["CFrame"]);
    assert_eq!(inside(camera), ScopedPremise::Inside);
    assert_eq!(
        inside(touched("p2", &P2, &["Transparency"])),
        ScopedPremise::False
    );
    assert_eq!(
        inside(touched("tag", &TAG, &["Name"])),
        ScopedPremise::False
    );
    let mut elsewhere = touched("tag", &TAG, &["Value"]);
    elsewhere.path_ordinals[2] = 2;
    assert_eq!(inside(elsewhere), ScopedPremise::False);
    let mut children = touched("p1", &P1, &[]);
    children.kind = "children".into();
    assert_eq!(inside(children), ScopedPremise::False);
    let mut unnamed = touched("tag", &TAG, &["Value"]);
    unnamed.fields = None;
    assert_eq!(inside(unnamed), ScopedPremise::False);
    assert_eq!(premise(&["Workspace"], None), ScopedPremise::False);
    let tag = Some(vec![touched("tag", &TAG, &["Value"])]);
    assert_eq!(
        premise(&["Lighting", "Workspace"], tag),
        ScopedPremise::False
    );
    assert_eq!(
        instance_premise(&ProofScope::Unknown("ended".into()), &pulls),
        ScopedPremise::False
    );
}

#[test]
fn studio_names_what_changed_of_an_instance_whose_values_changed() {
    let scope = ProofScope::parse(&json!({
        "services": ["Workspace"],
        "touched": {"Workspace": [
            {"pathSegments": ["Workspace", "P"], "pathOrdinals": [1, 2], "kind": "values",
             "debugId": "A-1", "properties": ["Size", "Color"], "attributes": true, "tags": false},
            {"pathSegments": ["Workspace", "Q"], "pathOrdinals": [1, 1], "kind": "values",
             "debugId": "B-1", "properties": {}, "attributes": false, "tags": true},
            {"pathSegments": ["Workspace", "R"], "pathOrdinals": [1, 1], "kind": "values"},
        ]},
    }));
    let ProofScope::Services { touched, .. } = scope else {
        panic!("no scope answer");
    };
    let fields = (touched["Workspace"].as_ref().unwrap().iter())
        .map(|entry| entry.fields.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        fields,
        [
            Some(TouchedFields {
                debug_id: "A-1".into(),
                properties: vec!["Size".into(), "Color".into()],
                attributes: true,
                tags: false,
            }),
            Some(TouchedFields {
                debug_id: "B-1".into(),
                properties: Vec::new(),
                attributes: false,
                tags: true,
            }),
            None,
        ]
    );
}
