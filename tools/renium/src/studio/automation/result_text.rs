//! How a value returned by live Luau reads in text mode.
//!
//! A reader pays for every token of a result, and the shapes the bridge sends
//! are built for programs: full-precision floats, `{"_type":"Vector3",...}`
//! objects, one object per list item with its keys repeated. Text mode prints
//! the same information in the shortest form that still reads unambiguously;
//! `--output-mode json` keeps the bridge's values as they are. Text the code
//! built itself is never changed.

use std::collections::BTreeSet;

use serde_json::{Map, Number, Value};

/// Decimals a number keeps in text. Coordinates, sizes and times are read,
/// compared and typed back at this precision.
const DECIMALS: i32 = 3;

/// The most decimals a small difference is printed with.
const MOST_DECIMALS: i32 = 15;

/// A float as text mode prints it: three decimals. A value below 1 keeps
/// three significant digits instead (`0.000123`), and so does the distance of
/// a value from the whole number it would otherwise print as (`5.0001`, not
/// `5`), so a value that is not a whole number never reads as one. A
/// difference below what a double holds at that size is noise and is dropped.
fn rounded(value: f64) -> Number {
    if !value.is_finite() {
        return Number::from(0);
    }
    let whole = |value: f64| Number::from(value as i64);
    if value.abs() >= 1.0e15 {
        return Number::from_f64(value).unwrap_or_else(|| Number::from(0));
    }
    let nearest = value.round();
    let off = (value - nearest).abs();
    if off <= nearest.abs() * 1.0e-12 {
        return whole(nearest);
    }
    let vanishing = off < 0.5 * 10_f64.powi(-DECIMALS);
    let decimals = if vanishing || value.abs() < 1.0 {
        let size = if vanishing { off } else { value.abs() };
        // The first significant digit sits this many places after the point.
        let leading = (-size.log10()).ceil() as i32;
        (leading + DECIMALS - 1).clamp(DECIMALS, MOST_DECIMALS)
    } else {
        DECIMALS
    };
    let scale = 10_f64.powi(decimals);
    let short = (value * scale).round() / scale;
    if short == short.trunc() {
        return whole(short);
    }
    Number::from_f64(short).unwrap_or_else(|| Number::from(0))
}

fn number_text(value: f64) -> String {
    rounded(value).to_string()
}

fn joined(numbers: &[f64], separator: &str) -> String {
    numbers
        .iter()
        .map(|number| number_text(*number))
        .collect::<Vec<_>>()
        .join(separator)
}

/// A value the bridge sends as a `_type` object, read into what it prints as.
enum Typed {
    Nil,
    /// A value that is a few numbers, under the name it prints with.
    Numbers(&'static str, Vec<f64>),
    Instance {
        path: String,
        class: Option<String>,
    },
    /// A value the bridge sends as text that reads on its own.
    Text(String),
}

/// A pitch this close to straight up or down leaves yaw and roll as one
/// angle (the sine of 89.9 degrees).
const GIMBAL_LOCK: f64 = 0.999_998_5;

/// A CFrame as its position and, when it is rotated, its orientation in
/// degrees: the angles the Orientation property shows (applied Y, X, Z).
fn cframe_numbers(components: &[f64]) -> Option<Vec<f64>> {
    let [x, y, z, m00, m01, m02, m10, m11, m12, _, _, m22] = *components else {
        return None;
    };
    let angles = if m12.abs() >= GIMBAL_LOCK {
        // Looking straight up or down: the whole turn about Y is yaw.
        let pitch = -90_f64.copysign(m12);
        let yaw = if m12 < 0.0 {
            m01.atan2(m00)
        } else {
            (-m01).atan2(m00)
        };
        [pitch, yaw.to_degrees(), 0.0]
    } else {
        [(-m12).asin(), m02.atan2(m22), m10.atan2(m11)].map(f64::to_degrees)
    };
    // The angles are worked out from single-precision components, so what
    // lies past three decimals is the noise of that arithmetic.
    let angles = angles.map(|angle| (angle * 1000.0).round() / 1000.0);
    if angles.iter().all(|angle| *angle == 0.0) {
        return Some(vec![x, y, z]);
    }
    Some(vec![x, y, z, angles[0], angles[1], angles[2]])
}

/// What a `_type` object prints as, or None for a shape that stays an object.
fn typed(object: &Map<String, Value>) -> Option<Typed> {
    let numbers = |keys: &[&str]| {
        keys.iter()
            .map(|key| object.get(*key)?.as_f64())
            .collect::<Option<Vec<_>>>()
    };
    let text = |key: &str| Some(object.get(key)?.as_str()?.to_string());
    Some(match object.get("_type")?.as_str()? {
        "Nil" if object.len() == 1 => Typed::Nil,
        "Vector3" => Typed::Numbers("Vector3", numbers(&["x", "y", "z"])?),
        "Vector2" => Typed::Numbers("Vector2", numbers(&["x", "y"])?),
        "UDim" => Typed::Numbers("UDim", numbers(&["scale", "offset"])?),
        "UDim2" => Typed::Numbers(
            "UDim2",
            numbers(&["xScale", "xOffset", "yScale", "yOffset"])?,
        ),
        "Rect" => Typed::Numbers("Rect", numbers(&["minX", "minY", "maxX", "maxY"])?),
        "NumberRange" => Typed::Numbers("NumberRange", numbers(&["min", "max"])?),
        "BrickColor" => Typed::Numbers("BrickColor", numbers(&["number"])?),
        // Named for its 0-255 scale: `Color3(1, 1, 1)` would read as white.
        "Color3" => Typed::Numbers(
            "RGB",
            numbers(&["r", "g", "b"])?
                .into_iter()
                .map(|channel| (channel * 255.0).round())
                .collect(),
        ),
        "CFrame" => {
            let components = object
                .get("components")?
                .as_array()?
                .iter()
                .map(Value::as_f64)
                .collect::<Option<Vec<_>>>()?;
            Typed::Numbers("CFrame", cframe_numbers(&components)?)
        }
        "Instance" => {
            let path = text("path")?;
            let path = match path.strip_prefix("game.") {
                Some("") => "game",
                Some(below) => below,
                None => &path,
            };
            Typed::Instance {
                path: path.to_string(),
                class: text("className"),
            }
        }
        // `Enum.Material.Wood`, and `inf`, `-inf` or `nan`.
        "EnumItem" | "Float" if object.len() == 2 => Typed::Text(text("value")?),
        // A table the serializer gave up on keeps its marker.
        "Truncated" | "Cycle" => return None,
        // Any other type arrives as its `tostring`, which seldom names it.
        kind if object.len() == 2 => {
            let text = text("value")?;
            Typed::Text(if text.starts_with(kind) {
                text
            } else {
                format!("{kind}({text})")
            })
        }
        _ => return None,
    })
}

/// A typed value where it stands alone or inside a JSON line.
fn typed_text(value: &Typed) -> String {
    match value {
        Typed::Nil => "nil".to_string(),
        Typed::Numbers("CFrame", numbers) if numbers.len() == 6 => format!(
            "CFrame({}, rot {})",
            joined(&numbers[..3], ", "),
            joined(&numbers[3..], ", ")
        ),
        Typed::Numbers(name, numbers) => format!("{name}({})", joined(numbers, ", ")),
        Typed::Instance {
            path,
            class: Some(class),
        } => format!("{path} ({class})"),
        Typed::Instance { path, class: None } => path.clone(),
        Typed::Text(text) => text.clone(),
    }
}

/// The value with every number rounded and every typed value as its text, for
/// results that print inside a JSON line.
pub(crate) fn compact_value(value: &Value) -> Value {
    match value {
        Value::Number(number) if number.is_f64() => {
            Value::Number(rounded(number.as_f64().unwrap_or_default()))
        }
        Value::Array(items) => Value::Array(items.iter().map(compact_value).collect()),
        Value::Object(object) => match typed(object) {
            Some(Typed::Nil) => Value::Null,
            Some(value) => Value::String(typed_text(&value)),
            None => Value::Object(
                object
                    .iter()
                    .map(|(key, item)| (key.clone(), compact_value(item)))
                    .collect(),
            ),
        },
        other => other.clone(),
    }
}

/// Text in a row as it is, unless it would read as several cells, as a value
/// of another kind or as the header of a list.
fn bare(text: &str) -> bool {
    !text.is_empty()
        && !text.contains(char::is_whitespace)
        && !text.starts_with(['"', '[', '{'])
        && !text.ends_with(':')
        && !matches!(text, "nil" | "true" | "false")
        && text.parse::<f64>().is_err()
}

fn text_cell(text: &str) -> String {
    if bare(text) {
        text.to_string()
    } else {
        Value::String(text.to_string()).to_string()
    }
}

/// The type every value of a column shares, when all of them are the same few
/// numbers: the header names it once and the cells hold the numbers alone.
fn column_type(records: &[&Map<String, Value>], key: &str) -> Option<&'static str> {
    let mut shared = None;
    for record in records {
        let name = match record.get(key) {
            None | Some(Value::Null) => continue,
            Some(Value::Object(object)) => match typed(object)? {
                Typed::Nil => continue,
                Typed::Numbers(name, _) => name,
                _ => return None,
            },
            Some(_) => return None,
        };
        if *shared.get_or_insert(name) != name {
            return None;
        }
    }
    shared
}

/// One cell of a row, or None for a value a cell cannot hold.
fn cell(value: Option<&Value>, column: Option<&str>) -> Option<String> {
    Some(match value {
        None | Some(Value::Null) => "nil".to_string(),
        Some(Value::String(text)) => text_cell(text),
        Some(Value::Object(object)) => match typed(object)? {
            Typed::Nil => "nil".to_string(),
            Typed::Numbers(_, numbers) if column.is_some() => joined(&numbers, ","),
            Typed::Numbers(name, numbers) => format!("{name}({})", joined(&numbers, ",")),
            Typed::Instance { path, .. } => text_cell(&path),
            Typed::Text(text) => text_cell(&text),
        },
        Some(Value::Array(items)) => {
            if !items
                .iter()
                .all(|item| matches!(item, Value::Number(_) | Value::Bool(_)))
            {
                return None;
            }
            compact_value(&Value::Array(items.clone())).to_string()
        }
        Some(scalar) => compact_value(scalar).to_string(),
    })
}

/// A list of records as a header line and one line per record: the keys are
/// printed once. A record may lack a key another has (a nil field is no field
/// in Luau), as long as the list still reads as one table.
fn rows(items: &[Value]) -> Option<String> {
    if items.len() < 2 {
        return None;
    }
    let records = items
        .iter()
        .map(|item| {
            item.as_object()
                .filter(|record| !record.is_empty() && !record.contains_key("_type"))
        })
        .collect::<Option<Vec<_>>>()?;
    let keys = records
        .iter()
        .flat_map(|record| record.keys())
        .collect::<BTreeSet<_>>();
    if keys
        .iter()
        .any(|key| key.is_empty() || key.contains(|c: char| c.is_whitespace() || c == ':'))
    {
        return None;
    }
    let filled = records.iter().map(|record| record.len()).sum::<usize>();
    if filled * 4 < records.len() * keys.len() * 3 {
        return None;
    }
    let columns = keys
        .iter()
        .map(|key| (key.as_str(), column_type(&records, key)))
        .collect::<Vec<_>>();
    let header = columns
        .iter()
        .map(|(key, column)| match column {
            Some(name) => format!("{key}:{name}"),
            None => key.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ");
    let mut lines = vec![header];
    for record in &records {
        let cells = columns
            .iter()
            .map(|(key, column)| cell(record.get(*key), *column))
            .collect::<Option<Vec<_>>>()?;
        lines.push(cells.join(" "));
    }
    Some(lines.join("\n"))
}

/// A list of one-line strings as those lines.
fn string_lines(items: &[Value]) -> Option<String> {
    if items.len() < 2 {
        return None;
    }
    items
        .iter()
        .map(|item| {
            item.as_str().filter(|text| {
                !text.trim().is_empty()
                    && !text.contains('\n')
                    && !text.ends_with(':')
                    && !reads_as_another_value(text)
            })
        })
        .collect::<Option<Vec<_>>>()
        .map(|lines| lines.join("\n"))
}

/// A list that prints as lines of its own instead of one JSON line.
fn block(value: &Value) -> Option<String> {
    let items = value.as_array()?;
    rows(items).or_else(|| string_lines(items))
}

/// The lines a run prints around its values. A returned string that starts
/// like one is quoted.
const LINE_MARKS: [&str; 8] = [
    "[print] ",
    "[warn] ",
    "[error] ",
    "[info] ",
    "note: ",
    "hint: ",
    "error: ",
    "... (cut: ",
];

/// Whether a returned string, printed as it is, would read as something
/// else: another value (`nil`, `true`, a number, a JSON value), the `ok` of a
/// run that returned nothing, or one of the lines a run prints around its
/// values.
fn reads_as_another_value(text: &str) -> bool {
    text.is_empty()
        || matches!(text, "nil" | "ok")
        || serde_json::from_str::<Value>(text).is_ok()
        || LINE_MARKS.iter().any(|mark| text.starts_with(mark))
}

/// One returned value as text mode prints it: a string as it is, `nil`, a
/// typed value in its short form, anything else as compact JSON. A string
/// that holds a JSON table prints as that table, without the layer of
/// escapes.
///
/// The only value of a run (`alone`) may spread over lines: a list of records
/// prints as a header and rows, a list of strings as lines, and a table that
/// holds such lists prints them under their keys, after its other fields.
/// Beside other values those forms would run into their neighbours.
pub(crate) fn value_text(value: &Value, alone: bool) -> String {
    match value {
        Value::String(text) => {
            if let Ok(table @ (Value::Array(_) | Value::Object(_))) =
                serde_json::from_str::<Value>(text)
            {
                return value_text(&table, alone);
            }
            if reads_as_another_value(text) {
                value.to_string()
            } else {
                text.clone()
            }
        }
        Value::Null => "nil".to_string(),
        Value::Array(_) if alone => {
            block(value).unwrap_or_else(|| compact_value(value).to_string())
        }
        Value::Object(object) => match typed(object) {
            Some(value) => typed_text(&value),
            None if alone => table_text(object),
            None => compact_value(value).to_string(),
        },
        other => compact_value(other).to_string(),
    }
}

fn table_text(object: &Map<String, Value>) -> String {
    let mut fields = Map::new();
    let mut blocks = Vec::new();
    for (key, item) in object {
        match block(item) {
            Some(lines) => blocks.push(format!("{key}:\n{lines}")),
            None => {
                fields.insert(key.clone(), compact_value(item));
            }
        }
    }
    if blocks.is_empty() {
        return Value::Object(fields).to_string();
    }
    if !fields.is_empty() {
        blocks.insert(0, Value::Object(fields).to_string());
    }
    blocks.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_keep_three_decimals_or_three_significant_digits() {
        for (value, text) in [
            (1.0 / 3.0, "0.333"),
            (1.2345679, "1.235"),
            (-716.0412597656, "-716.041"),
            (2.0, "2"),
            (0.64705884, "0.647"),
            (0.016666668, "0.0167"),
            (0.000123456, "0.000123"),
            (-0.0000098765, "-9.88e-6"),
            (0.0, "0"),
            (1234567.891, "1234567.891"),
            (0.1 + 0.2, "0.3"),
            (1.2000000476837158, "1.2"),
            // A value that is not a whole number does not print as one.
            (5.0001, "5.0001"),
            (4.99999, "4.99999"),
            (0.9996, "0.9996"),
            (-2.00000123456, "-2.00000123"),
            (0.99999994, "0.99999994"),
            // Less than a double holds at that size is noise.
            (12.000000000000002, "12"),
            (1.22464685e-16, "0"),
            (123456789.00000001, "123456789"),
            // Too large to round.
            (1.0e306, "1e+306"),
            (-2.5e15, "-2500000000000000.0"),
        ] {
            assert_eq!(number_text(value), text, "{value}");
        }
        assert_eq!(compact_value(&json!(7)), json!(7));
        assert_eq!(
            compact_value(&json!(9007199254740993_i64)),
            json!(9007199254740993_i64)
        );
        assert_eq!(
            compact_value(&json!("at 846.5567626953125")),
            json!("at 846.5567626953125")
        );
    }

    /// The components of a CFrame at the origin with this orientation in
    /// degrees, built the way Roblox applies it: Y, then X, then Z.
    fn oriented(rx: f64, ry: f64, rz: f64) -> Value {
        let (sx, cx) = rx.to_radians().sin_cos();
        let (sy, cy) = ry.to_radians().sin_cos();
        let (sz, cz) = rz.to_radians().sin_cos();
        json!({"_type": "CFrame", "components": [
            0, 0, 0,
            cy * cz + sy * sx * sz, sy * sx * cz - cy * sz, sy * cx,
            cx * sz, cx * cz, -sx,
            cy * sx * sz - sy * cz, sy * sz + cy * sx * cz, cy * cx,
        ]})
    }

    #[test]
    fn a_cframe_prints_the_orientation_roblox_shows() {
        for (angles, text) in [
            ((0.0, 0.0, 0.0), "CFrame(0, 0, 0)"),
            ((0.0, 90.0, 0.0), "CFrame(0, 0, 0, rot 0, 90, 0)"),
            ((30.0, 45.0, 60.0), "CFrame(0, 0, 0, rot 30, 45, 60)"),
            (
                (-20.0, 170.0, -100.0),
                "CFrame(0, 0, 0, rot -20, 170, -100)",
            ),
            // Straight down and straight up keep their yaw.
            ((-90.0, 30.0, 0.0), "CFrame(0, 0, 0, rot -90, 30, 0)"),
            ((90.0, 45.0, 0.0), "CFrame(0, 0, 0, rot 90, 45, 0)"),
            ((90.0, 0.0, 0.0), "CFrame(0, 0, 0, rot 90, 0, 0)"),
        ] {
            let (rx, ry, rz) = angles;
            assert_eq!(value_text(&oriented(rx, ry, rz), true), text, "{angles:?}");
        }
        // Components as Studio sends them, in single precision: a quarter
        // turn about Y, then one whose cosine is a little below zero.
        let quarter = json!({"_type": "CFrame", "components": [
            1, 2, 3, -4.371139e-8, 0, 1, 0, 1, 0, -1, 0, -4.371139e-8,
        ]});
        assert_eq!(value_text(&quarter, true), "CFrame(1, 2, 3, rot 0, 90, 0)");
        let turned = json!({"_type": "CFrame", "components": [
            0, 0, 0, 0.8660254, 0.5, 0, 0, -4.371139e-8, -1, -0.5, 0.8660254, 0,
        ]});
        assert_eq!(value_text(&turned, true), "CFrame(0, 0, 0, rot 90, 30, 0)");
        let noisy = json!({"_type": "CFrame", "components": [
            1, 2, 3, 1, 0, 0, 0, -4.371139e-8, -1, 0, 1, -4.371139e-8,
        ]});
        assert_eq!(value_text(&noisy, true), "CFrame(1, 2, 3, rot 90, 0, 0)");
    }

    #[test]
    fn typed_values_print_in_their_short_form() {
        let cases = [
            (
                json!({"_type": "Vector3", "x": 1.2345679, "y": 2, "z": 3.5}),
                "Vector3(1.235, 2, 3.5)",
            ),
            (
                json!({"_type": "CFrame", "components": [1.2345679, 2, 3.5, 1, 0, 0, 0, 1, 0, 0, 0, 1]}),
                "CFrame(1.235, 2, 3.5)",
            ),
            (
                json!({"_type": "Color3", "r": 0.6392157, "g": 0.63529414, "b": 0.64705884}),
                "RGB(163, 162, 165)",
            ),
            (
                json!({"_type": "UDim2", "xScale": 0.5, "xOffset": 0, "yScale": 1, "yOffset": -10}),
                "UDim2(0.5, 0, 1, -10)",
            ),
            (
                json!({"_type": "EnumItem", "value": "Enum.Material.Wood"}),
                "Enum.Material.Wood",
            ),
            (json!({"_type": "Float", "value": "-inf"}), "-inf"),
            // Types the plugin sends as their tostring are named.
            (
                json!({"_type": "Vector3int16", "value": "1, 2, 3"}),
                "Vector3int16(1, 2, 3)",
            ),
            (
                json!({"_type": "Ray", "value": "{0, 5, 0}, {0, -1, 0}"}),
                "Ray({0, 5, 0}, {0, -1, 0})",
            ),
            (
                json!({"_type": "DateTime", "value": "DateTime(1794373200)"}),
                "DateTime(1794373200)",
            ),
            (
                json!({"_type": "Instance", "className": "Part", "name": "Door", "path": "game.Workspace.Lobby.Door"}),
                "Workspace.Lobby.Door (Part)",
            ),
            (
                json!({"_type": "Instance", "className": "DataModel", "name": "Place", "path": "game."}),
                "game (DataModel)",
            ),
            (json!({"_type": "Nil"}), "nil"),
            (json!(null), "nil"),
            (json!(true), "true"),
        ];
        for (value, text) in cases {
            assert_eq!(value_text(&value, true), text, "{value}");
            assert_eq!(value_text(&value, false), text, "{value}");
        }
        // Inside a table a typed value is its text; other shapes stay objects.
        assert_eq!(
            value_text(
                &json!({
                    "at": {"_type": "Vector3", "x": 1, "y": 2, "z": 3},
                    "n": 1.0 / 3.0,
                    "gone": {"_type": "Nil"},
                    "loop": {"_type": "Cycle", "value": "table: 0x1"},
                    "keys": {"_type": "NumberSequence", "keypoints": [{"time": 0.123456, "value": 1}]},
                }),
                true
            ),
            r#"{"at":"Vector3(1, 2, 3)","gone":null,"keys":{"_type":"NumberSequence","keypoints":[{"time":0.123,"value":1}]},"loop":{"_type":"Cycle","value":"table: 0x1"},"n":0.333}"#
        );
    }

    #[test]
    fn a_list_of_records_prints_its_keys_once() {
        let records = json!([
            {"name": "Door", "x": 1.23456, "open": true, "at": {"_type": "Vector3", "x": 1, "y": 2.5, "z": 3},
                "in": {"_type": "Instance", "className": "Model", "path": "game.Workspace.Lobby"}, "tags": [1, 2]},
            {"name": "Big Gate", "x": 2, "open": false, "at": {"_type": "Nil"},
                "in": {"_type": "Instance", "className": "Model", "path": "game.Workspace.Main Hall"}, "tags": []},
            {"name": "12", "x": 0.5, "open": false, "tags": [true]},
        ]);
        assert_eq!(
            value_text(&records, true),
            [
                "at:Vector3 in name open tags x",
                "1,2.5,3 Workspace.Lobby Door true [1,2] 1.235",
                "nil \"Workspace.Main Hall\" \"Big Gate\" false [] 2",
                "nil nil \"12\" false [true] 0.5",
            ]
            .join("\n")
        );
        // Beside other values the list stays one line.
        assert_eq!(
            value_text(&json!([{"x": 1.23456}, {"x": 2}]), false),
            r#"[{"x":1.235},{"x":2}]"#
        );
        // A column of mixed types names the type in each cell, and a cell
        // that would read as a line of the run is quoted.
        assert_eq!(
            value_text(
                &json!([
                    {"k": "note:", "v": {"_type": "Vector3", "x": 1, "y": 2, "z": 3}},
                    {"k": "a", "v": {"_type": "Vector2", "x": 1, "y": 2}},
                    {"k": "b", "v": ""},
                ]),
                true
            ),
            "k v\n\"note:\" Vector3(1,2,3)\na Vector2(1,2)\nb \"\""
        );
        // Lists that are no table stay JSON.
        for (value, text) in [
            (json!([1.23456, 2, 3]), "[1.235,2,3]"),
            (json!([{"a": 1}]), r#"[{"a":1}]"#),
            (
                json!([{"a": 1}, {"b": 2}, {"c": 3}]),
                r#"[{"a":1},{"b":2},{"c":3}]"#,
            ),
            (
                json!([{"a": {"b": 1}}, {"a": {"b": 2}}]),
                r#"[{"a":{"b":1}},{"a":{"b":2}}]"#,
            ),
            (
                json!([{"a": ["x"]}, {"a": ["y"]}]),
                r#"[{"a":["x"]},{"a":["y"]}]"#,
            ),
            (json!([{"a b": 1}, {"a b": 2}]), r#"[{"a b":1},{"a b":2}]"#),
            (json!([{"a": 1}, 2]), r#"[{"a":1},2]"#),
            (json!([[1, 2], [3, 4]]), "[[1,2],[3,4]]"),
            (json!([]), "[]"),
            (json!({}), "{}"),
        ] {
            assert_eq!(value_text(&value, true), text);
        }
    }

    #[test]
    fn lists_inside_a_table_print_under_their_keys() {
        let table = json!({
            "count": 2,
            "map": "Mall",
            "parts": [{"name": "Door", "y": 1.23456}, {"name": "Gate", "y": 2}],
            "log": ["0.4 track", "0.8 drift"],
        });
        assert_eq!(
            value_text(&table, true),
            "{\"count\":2,\"map\":\"Mall\"}\nlog:\n0.4 track\n0.8 drift\nparts:\nname y\nDoor 1.235\nGate 2"
        );
        assert_eq!(
            value_text(&table, false),
            r#"{"count":2,"log":["0.4 track","0.8 drift"],"map":"Mall","parts":[{"name":"Door","y":1.235},{"name":"Gate","y":2}]}"#
        );
        assert_eq!(
            value_text(&json!({"log": ["a b", "c d"]}), true),
            "log:\na b\nc d"
        );
        assert_eq!(value_text(&json!(["a b", "c d"]), true), "a b\nc d");
        // Strings that would read as something else keep the list as JSON.
        for list in [
            json!(["a", "12"]),
            json!(["a", ""]),
            json!(["a", "  "]),
            json!(["a", "b\nc"]),
            json!(["a", "note: b"]),
            json!(["a", "parts:"]),
            json!(["a"]),
        ] {
            assert_eq!(value_text(&list, true), list.to_string());
        }
    }

    #[test]
    fn a_string_prints_as_it_is_unless_it_reads_as_another_value() {
        for (value, text) in [
            (json!("Door"), "Door"),
            (json!("two\nlines"), "two\nlines"),
            // Text the code built keeps every digit.
            (
                json!("local RAD = 0.0174532925199 at 846.5567626953125"),
                "local RAD = 0.0174532925199 at 846.5567626953125",
            ),
            (json!("12"), "\"12\""),
            (json!("nil"), "\"nil\""),
            (json!(""), "\"\""),
            (json!("hint: x"), "\"hint: x\""),
            // A JSON table in a string prints as the table.
            (
                json!(r#"{"cars":[{"slot":0,"lag":0.123456},{"slot":1,"lag":2}],"map":"Mall"}"#),
                "{\"map\":\"Mall\"}\ncars:\nlag slot\n0.123 0\n2 1",
            ),
            (json!("[1.23456,2]"), "[1.235,2]"),
        ] {
            assert_eq!(value_text(&value, true), text, "{value}");
        }
    }
}
