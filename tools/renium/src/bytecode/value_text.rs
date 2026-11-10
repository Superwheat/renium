//! Stored values as the text forms of `rbx tr`, `rbx f` and `rbx in` print
//! them.

use serde_json::{Map, Number, Value};

use crate::bytecode::edit::orientation_from_rotation_rows;
use crate::bytecode::explorer::compact_path_string;

/// Where a value prints: in a row of fields, where a space ends it, or alone
/// up to the end of its line.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Place {
    Row,
    Line,
}

/// The shown fields of one instance in print order: `Name=value` for each
/// property, `@Name=value` for each attribute and, in one last entry, its
/// tags as `#tag` words.
pub(super) fn field_texts(
    properties: Option<&Value>,
    attributes: Option<&Value>,
    place: Place,
) -> Vec<String> {
    let mut texts = Vec::new();
    let mut tags = None;
    for (name, value) in entries(properties) {
        if name == "Tags"
            && let Some(words) = tag_words(value)
        {
            tags = Some(words);
            continue;
        }
        texts.push(format!("{}={}", field_name(name), value_text(value, place)));
    }
    for (name, value) in entries(attributes) {
        texts.push(format!(
            "@{}={}",
            field_name(name),
            value_text(value, place)
        ));
    }
    texts.extend(tags.filter(|words| !words.is_empty()));
    texts
}

fn entries(record: Option<&Value>) -> impl Iterator<Item = (&String, &Value)> {
    record.and_then(Value::as_object).into_iter().flatten()
}

/// One stored value in the shortest text that still reads as that value:
/// a trimmed number, a string quoted only when it would not read as one,
/// `x,y,z`, a UDim as `scale,offset` and a UDim2 as `{xs,xo},{ys,yo}`, a
/// color as `#rrggbb`, a CFrame as `x,y,z;rx,ry,rz` (position, then
/// Orientation in degrees), an enum by its item name, a reference as the
/// dotted path of its target or `nil`, a binary payload as its type and
/// size, and anything else as compact JSON.
pub(super) fn value_text(value: &Value, place: Place) -> String {
    match value {
        Value::Null => "nil".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number_text(number),
        Value::String(text) => string_text(text, place),
        Value::Object(object) => typed_text(object, place).unwrap_or_else(|| json_text(value)),
        Value::Array(_) => json_text(value),
    }
}

fn typed_text(object: &Map<String, Value>, place: Place) -> Option<String> {
    let part = |key: &str| object.get(key).and_then(component_text);
    let kind = object.get("_type")?.as_str()?;
    match kind {
        "Vector3" | "Vector3int16" => Some(format!("{},{},{}", part("x")?, part("y")?, part("z")?)),
        "Vector2" | "Vector2int16" => Some(format!("{},{}", part("x")?, part("y")?)),
        "UDim" => Some(format!("{},{}", part("scale")?, part("offset")?)),
        "UDim2" => Some(format!(
            "{{{},{}}},{{{},{}}}",
            part("xScale")?,
            part("xOffset")?,
            part("yScale")?,
            part("yOffset")?
        )),
        "Color3" => color_text(object),
        "CFrame" => cframe_text(object.get("components")?.as_array()?),
        "EnumItem" => Some(string_text(object.get("name")?.as_str()?, place)),
        "Ref" => reference_text(object, place),
        _ => wrapped_text(kind, object, place),
    }
}

/// A typed object that only wraps one scalar prints as that scalar, and one
/// that only carries a binary payload as `<Type N bytes>`.
fn wrapped_text(kind: &str, object: &Map<String, Value>, place: Place) -> Option<String> {
    if object.len() != 2 {
        return None;
    }
    if let Some(payload) = object.get("base64").and_then(Value::as_str) {
        let padding = payload
            .bytes()
            .rev()
            .take_while(|byte| *byte == b'=')
            .count();
        let bytes = (payload.len() / 4 * 3).saturating_sub(padding);
        return Some(format!("<{kind} {bytes} bytes>"));
    }
    match object.get("value")? {
        Value::String(text)
            if kind == "Float" && matches!(text.as_str(), "inf" | "-inf" | "nan") =>
        {
            Some(text.clone())
        }
        inner @ (Value::String(_) | Value::Number(_) | Value::Bool(_)) => {
            Some(value_text(inner, place))
        }
        _ => None,
    }
}

/// One number of a composite value; a non-finite one is stored as a marker
/// object.
fn component_text(value: &Value) -> Option<String> {
    match value {
        Value::Number(number) => Some(number_text(number)),
        Value::Object(object) if object.get("_type").and_then(Value::as_str) == Some("Float") => {
            wrapped_text("Float", object, Place::Row)
        }
        _ => None,
    }
}

fn color_text(object: &Map<String, Value>) -> Option<String> {
    let channel = |key: &str| {
        let value = object.get(key)?.as_f64()?;
        (0.0..=1.0)
            .contains(&value)
            .then(|| (value * 255.0).round() as u8)
    };
    Some(format!(
        "#{:02x}{:02x}{:02x}",
        channel("r")?,
        channel("g")?,
        channel("b")?
    ))
}

fn cframe_text(components: &[Value]) -> Option<String> {
    let numbers = components
        .iter()
        .map(Value::as_f64)
        .collect::<Option<Vec<_>>>()?;
    let [x, y, z, rotation @ ..] = numbers.as_slice() else {
        return None;
    };
    let rotation: &[f64; 9] = rotation.try_into().ok()?;
    let [rx, ry, rz] = orientation_from_rotation_rows([
        [rotation[0], rotation[1], rotation[2]],
        [rotation[3], rotation[4], rotation[5]],
        [rotation[6], rotation[7], rotation[8]],
    ]);
    Some(format!(
        "{},{},{};{},{},{}",
        float_text(*x),
        float_text(*y),
        float_text(*z),
        degrees_text(rx),
        degrees_text(ry),
        degrees_text(rz)
    ))
}

fn reference_text(object: &Map<String, Value>, place: Place) -> Option<String> {
    let segments = object
        .get("pathSegments")?
        .as_array()?
        .iter()
        .map(|segment| segment.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()?;
    let ordinals = object
        .get("pathOrdinals")
        .and_then(Value::as_array)
        .and_then(|ordinals| {
            ordinals
                .iter()
                .map(|ordinal| ordinal.as_u64().map(|ordinal| ordinal as usize))
                .collect::<Option<Vec<_>>>()
        });
    compact_path_string(&segments, ordinals.as_deref()).map(|path| string_text(&path, place))
}

fn tag_words(value: &Value) -> Option<String> {
    let words = value
        .as_array()?
        .iter()
        .map(|tag| Some(format!("#{}", string_text(tag.as_str()?, Place::Row))))
        .collect::<Option<Vec<_>>>()?;
    Some(words.join(" "))
}

fn field_name(name: &str) -> String {
    if !name.is_empty() && name.chars().all(|ch| ch.is_alphanumeric() || ch == '_') {
        name.to_string()
    } else {
        Value::String(name.to_string()).to_string()
    }
}

fn string_text(text: &str, place: Place) -> String {
    if reads_as_itself(text, place) {
        text.to_string()
    } else {
        Value::String(text.to_string()).to_string()
    }
}

/// Whether a string printed without quotes reads back as the same string:
/// nothing in it ends the value early, and it is not the text of a value of
/// another kind (`nil`, a boolean, numbers, a quoted or bracketed form).
fn reads_as_itself(text: &str, place: Place) -> bool {
    let ends_value = |ch: char| ch.is_control() || (place == Place::Row && ch.is_whitespace());
    !text.is_empty()
        && text.trim() == text
        && !text.contains(ends_value)
        && !text.starts_with(['"', '#', '@', '{', '[', '<'])
        && !matches!(text, "nil" | "true" | "false")
        && !text
            .split([',', ';'])
            .all(|part| part.trim().parse::<f64>().is_ok())
}

fn number_text(number: &Number) -> String {
    match number.as_f64() {
        Some(float) if number.is_f64() => float_text(float),
        _ => number.to_string(),
    }
}

/// A float with at most three decimals, no trailing zeros and no leading
/// zero: `1`, `.582`, `-12.5`. One too small or too large for that keeps
/// three significant digits and an exponent (`3.12e-4`), so only zero prints
/// as `0`.
fn float_text(value: f64) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    if !(0.0005..1e15).contains(&value.abs()) {
        let text = format!("{value:.2e}");
        return match text.split_once('e') {
            Some((mantissa, exponent)) => format!("{}e{exponent}", trim_decimals(mantissa)),
            None => text,
        };
    }
    let text = format!("{value:.3}");
    let trimmed = trim_decimals(&text);
    match trimmed.strip_prefix("-0.") {
        Some(decimals) => format!("-.{decimals}"),
        None => trimmed
            .strip_prefix("0.")
            .map_or_else(|| trimmed.to_string(), |decimals| format!(".{decimals}")),
    }
}

fn trim_decimals(text: &str) -> &str {
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.')
    } else {
        text
    }
}

/// An angle to three decimals, with float noise around zero dropped and a
/// half turn always positive, so equal rotations print alike.
fn degrees_text(degrees: f64) -> String {
    let rounded = (degrees * 1000.0).round() / 1000.0;
    if rounded == 0.0 {
        "0".to_string()
    } else if rounded == -180.0 {
        "180".to_string()
    } else {
        float_text(rounded)
    }
}

fn json_text(value: &Value) -> String {
    let mut shortened = value.clone();
    crate::app::output::shorten_single_precision_floats(&mut shortened);
    shortened.to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Place, field_texts, value_text};
    use crate::bytecode::edit::rotation_rows_from_orientation;

    fn row(value: serde_json::Value) -> String {
        value_text(&value, Place::Row)
    }

    #[test]
    fn numbers_drop_padding_and_keep_three_decimals() {
        assert_eq!(row(json!(1.0)), "1");
        assert_eq!(row(json!(0)), "0");
        assert_eq!(row(json!(0.0)), "0");
        assert_eq!(row(json!(-0.0)), "0");
        assert_eq!(row(json!(12)), "12");
        assert_eq!(row(json!(-3.50)), "-3.5");
        assert_eq!(row(json!(0.58151025)), ".582");
        assert_eq!(row(json!(-0.25)), "-.25");
        assert_eq!(row(json!(0.800000011920929)), ".8");
        assert_eq!(row(json!(16.128706)), "16.129");
        assert_eq!(row(json!(0.9996)), "1");
        assert_eq!(row(json!(9007199254740993_i64)), "9007199254740993");
        assert_eq!(row(json!(0.0004)), "4e-4");
        assert_eq!(row(json!(-0.000312345)), "-3.12e-4");
        assert_eq!(row(json!(3.4028235e38)), "3.4e38");
        assert_eq!(row(json!({"_type": "Float", "value": "-inf"})), "-inf");
        assert_eq!(row(json!({"_type": "Int64", "value": 42})), "42");
    }

    #[test]
    fn strings_are_quoted_only_when_they_would_not_read_as_one() {
        assert_eq!(row(json!("Plastic")), "Plastic");
        assert_eq!(row(json!("rbxassetid://123")), "rbxassetid://123");
        assert_eq!(row(json!("")), "\"\"");
        assert_eq!(row(json!("two words")), "\"two words\"");
        assert_eq!(value_text(&json!("two words"), Place::Line), "two words");
        assert_eq!(value_text(&json!(" padded"), Place::Line), "\" padded\"");
        assert_eq!(
            value_text(&json!("two\nlines"), Place::Line),
            "\"two\\nlines\""
        );
        for other in ["12", "-0.5", "1,2,3", "1, 2;3", "inf", "true", "nil"] {
            assert_eq!(row(json!(other)), format!("\"{other}\""), "{other}");
        }
        for other in ["#ff0000", "@Name", "{1,0},{1,0}", "[1]", "<b>Hi</b>"] {
            assert_eq!(row(json!(other)), format!("\"{other}\""), "{other}");
        }
        assert_eq!(row(json!("\"quoted\"")), "\"\\\"quoted\\\"\"");
        assert_eq!(row(json!("1,2,three")), "1,2,three");
    }

    #[test]
    fn vectors_udims_and_colors_print_their_numbers() {
        assert_eq!(
            row(json!({"_type": "Vector3", "x": 10.895435, "y": 16.0, "z": 0.9026873})),
            "10.895,16,.903"
        );
        assert_eq!(row(json!({"_type": "Vector2", "x": 0.5, "y": 1.0})), ".5,1");
        assert_eq!(
            row(json!({"_type": "Vector3int16", "x": 1, "y": -2, "z": 3})),
            "1,-2,3"
        );
        assert_eq!(
            row(json!({"_type": "UDim", "scale": 0.06, "offset": 8.0})),
            ".06,8"
        );
        assert_eq!(
            row(json!({
                "_type": "UDim2", "xScale": 1.0, "xOffset": 0.0, "yScale": 0.59394705, "yOffset": -24.0
            })),
            "{1,0},{.594,-24}"
        );
        assert_eq!(
            row(json!({"_type": "Color3", "r": 1.0, "g": 0.2745098, "b": 0.0})),
            "#ff4600"
        );
        assert_eq!(
            row(
                json!({"_type": "Vector3", "x": {"_type": "Float", "value": "inf"}, "y": 0.0, "z": 0.0})
            ),
            "inf,0,0"
        );
        assert_eq!(
            row(json!({"_type": "Color3", "r": 2.0, "g": 0.0, "b": 0.0})),
            "{\"_type\":\"Color3\",\"b\":0.0,\"g\":0.0,\"r\":2.0}"
        );
        assert_eq!(
            row(json!({"_type": "Vector3", "x": 1.0, "y": 2.0})),
            "{\"_type\":\"Vector3\",\"x\":1.0,\"y\":2.0}"
        );
    }

    #[test]
    fn a_cframe_prints_its_position_and_orientation() {
        let cframe = |position: [f64; 3], orientation: [f64; 3]| {
            let mut components = position.to_vec();
            components.extend(
                rotation_rows_from_orientation(orientation)
                    .iter()
                    .flatten()
                    .map(|cell| f64::from(*cell as f32)),
            );
            row(json!({"_type": "CFrame", "components": components}))
        };
        assert_eq!(cframe([1.0, 2.5, -3.0], [0.0, 0.0, 0.0]), "1,2.5,-3;0,0,0");
        assert_eq!(cframe([0.0, 0.0, 0.0], [0.0, 90.0, 0.0]), "0,0,0;0,90,0");
        assert_eq!(
            cframe([0.0, 0.0, 0.0], [10.0, -20.0, 30.0]),
            "0,0,0;10,-20,30"
        );
        assert_eq!(cframe([0.0, 0.0, 0.0], [0.0, 180.0, 0.0]), "0,0,0;0,180,0");
        assert_eq!(cframe([0.0, 0.0, 0.0], [0.0, -180.0, 0.0]), "0,0,0;0,180,0");
        assert_eq!(cframe([0.0, 0.0, 0.0], [90.0, 45.0, 0.0]), "0,0,0;90,45,0");
        assert_eq!(cframe([0.0, 0.0, 0.0], [-90.0, 0.0, 0.0]), "0,0,0;-90,0,0");
        let half_turn_with_noise = [
            0.0, 0.0, 0.0, -1.0, 0.0, -1e-7, 0.0, 1.0, 0.0, 1e-7, 0.0, -1.0,
        ];
        assert_eq!(
            row(json!({"_type": "CFrame", "components": half_turn_with_noise})),
            "0,0,0;0,180,0"
        );
        assert_eq!(
            row(json!({"_type": "CFrame", "components": [0.0, 0.0, 0.0, 1.0]})),
            "{\"_type\":\"CFrame\",\"components\":[0.0,0.0,0.0,1.0]}"
        );
    }

    #[test]
    fn enums_references_and_wrapped_values_print_what_they_name() {
        assert_eq!(
            row(
                json!({"_type": "EnumItem", "enumType": "Enum.Material", "name": "Neon", "value": 288})
            ),
            "Neon"
        );
        let reference = json!({
            "_type": "Ref",
            "pathOrdinals": [1, 1, 2],
            "pathSegments": ["Workspace", "Wood Crate", "Lid"],
            "settingsId": "debug:1_7",
        });
        assert_eq!(row(reference.clone()), "\"Workspace.Wood Crate.Lid[2]\"");
        assert_eq!(
            value_text(&reference, Place::Line),
            "Workspace.Wood Crate.Lid[2]"
        );
        assert_eq!(row(json!(null)), "nil");
        assert_eq!(
            row(
                json!({"_type": "Ref", "pathSegments": ["Workspace", "a.b"], "settingsId": "debug:1_8"})
            ),
            "{\"_type\":\"Ref\",\"pathSegments\":[\"Workspace\",\"a.b\"],\"settingsId\":\"debug:1_8\"}"
        );
        assert_eq!(
            row(json!({"_type": "UniqueId", "value": "6ca3c7a329a01ac40a4703db00003192"})),
            "6ca3c7a329a01ac40a4703db00003192"
        );
        assert_eq!(
            row(json!({"_type": "SharedString", "base64": "QUJDREU="})),
            "<SharedString 5 bytes>"
        );
        assert_eq!(
            row(json!({"_type": "BinaryString", "base64": ""})),
            "<BinaryString 0 bytes>"
        );
    }

    #[test]
    fn other_values_print_as_compact_json() {
        assert_eq!(
            row(json!({"_type": "NumberRange", "min": 0.05000000074505806, "max": 1.0})),
            "{\"_type\":\"NumberRange\",\"max\":1.0,\"min\":0.05}"
        );
        assert_eq!(
            row(
                json!({"family": "rbxasset://fonts/families/Arial.json", "style": "Normal", "weight": "Bold"})
            ),
            "{\"family\":\"rbxasset://fonts/families/Arial.json\",\"style\":\"Normal\",\"weight\":\"Bold\"}"
        );
        assert_eq!(row(json!([1, "a"])), "[1,\"a\"]");
    }

    #[test]
    fn fields_print_properties_then_attributes_then_tags() {
        let properties = json!({
            "Anchored": true,
            "Size": {"_type": "Vector3", "x": 4.0, "y": 1.0, "z": 2.0},
            "Tags": ["Enemy", "Big Boss"],
            "odd name": "x",
        });
        let attributes = json!({"Health": 100.0, "Owner": "No One"});
        assert_eq!(
            field_texts(Some(&properties), Some(&attributes), Place::Row),
            [
                "Anchored=true",
                "Size=4,1,2",
                "\"odd name\"=x",
                "@Health=100",
                "@Owner=\"No One\"",
                "#Enemy #\"Big Boss\"",
            ]
        );
        assert_eq!(
            field_texts(None, Some(&attributes), Place::Line),
            ["@Health=100", "@Owner=No One"]
        );
        assert_eq!(
            field_texts(Some(&json!({"Tags": []})), Some(&json!({})), Place::Row),
            Vec::<String>::new()
        );
    }
}
