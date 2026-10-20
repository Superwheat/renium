use rbx_dom_weak::{types::Variant, InstanceBuilder, WeakDom};
use rbx_reflection::ReflectionDatabase;

use crate::{Deserializer, Serializer};

#[test]
fn legacy_camera_doubles_keep_their_values_when_reflection_requires_floats() {
    let mut tree = WeakDom::new(InstanceBuilder::new("DataModel"));
    for (name, size, stretch) in [("First", 300.0f64, 1.0f64), ("Second", 615.25, 1.5)] {
        tree.insert(
            tree.root_ref(),
            InstanceBuilder::new("Camera")
                .with_name(name)
                .with_property("OrthographicSize", size)
                .with_property("ViewStretch", stretch),
        );
    }
    // Reproduce files written before these properties were in the database.
    let old_database = ReflectionDatabase::new();
    let mut bytes = Vec::new();
    Serializer::new()
        .reflection_database(&old_database)
        .serialize(&mut bytes, &tree, tree.root().children())
        .unwrap();

    let decoded = Deserializer::new().deserialize(bytes.as_slice()).unwrap();
    let strict = Deserializer::new().strict_float_types(true);
    let error = strict
        .deserialize(bytes.as_slice())
        .unwrap_err()
        .to_string();
    assert!(error.contains("Camera.OrthographicSize"), "{error}");
    assert!(
        error.contains("should be Float32, but it was Float64"),
        "{error}"
    );
    let flat_error = match strict.deserialize_flat(bytes.as_slice()) {
        Ok(_) => panic!("flat decoder silently normalized legacy float widths"),
        Err(error) => error.to_string(),
    };
    assert_eq!(error, flat_error);
    for (name, size, stretch) in [("First", 300.0f32, 1.0f32), ("Second", 615.25, 1.5)] {
        let camera = decoded
            .descendants()
            .find(|item| item.name == name)
            .unwrap();
        assert_eq!(
            camera.properties.get(&"OrthographicSize".into()),
            Some(&Variant::Float32(size)),
        );
        assert_eq!(
            camera.properties.get(&"ViewStretch".into()),
            Some(&Variant::Float32(stretch)),
        );
    }
}
