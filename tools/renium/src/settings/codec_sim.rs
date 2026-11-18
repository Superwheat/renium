//! Seeded law simulation of the store codec.
//!
//! Each seed generates a store document, a tree of instances whose
//! properties and attributes reach every value path of the encoder, and
//! states for each value the form decoding must give back. With
//! d1 = decode(encode(d0)), five laws must hold:
//!
//! 1. d1 holds d0's instances: the same ids, names, classes and parents, in
//!    the same order. The structure decode gives them without values, and the
//!    string-set decode gives d1 for sets of strings one of which the store
//!    holds whole, and nothing for a set it does not.
//! 2. Every value of d1 is the stated canonical form of d0's value, exactly:
//!    the same keys, number kinds and float bits. A document with a property
//!    named for a record field (Name, ClassName, Parent) or an object named
//!    Attributes is refused instead, by both encoders: a store cannot give
//!    such a property back.
//! 3. encode(d1) == encode(d0) byte for byte, and decode(encode(d1)) == d1.
//! 4. The dense-references encoder finds a reference's target by its
//!    instanceIndex alone, checking a settingsId beside it against that
//!    target; every other reference it writes as the outside reference its
//!    selectors describe, and one with an instanceIndex beside pathSegments
//!    it refuses. So decode(dense(d0)) is d0's canonical form with those
//!    references kept; where no reference differs, dense(d0) == encode(d0)
//!    byte for byte; and dense(d1) == encode(d1), since a decoded store
//!    names every local target by index.
//! 5. Keeping untouched property columns in their compact form while
//!    editing selected rows produces exactly the eager encoder's bytes.
//!
//! The canonical forms, as the codec writes and reads them:
//!
//! - A property whose value is null is dropped; inside arrays and objects a
//!   null stays.
//! - Integers stay integers and floats keep their bits.
//! - A numeric array comes back as floats, a non-finite item as
//!   `{"_type":"Float","value":"nan"|"inf"|"-inf"}`, whichever of the two
//!   non-finite forms it had.
//! - Vector2, Vector3, UDim, UDim2, Color3, Rect and CFrame keep only their
//!   own fields, each the nearest f32 as a float, a finite number past f32's
//!   range clamped to the largest finite f32 of its sign.
//! - An EnumItem property keeps only its name, "" when that is no string. A
//!   nested one keeps enumType and name when both are strings and enumType
//!   is not empty, and otherwise is read like a property.
//! - BrickColor becomes `{"BrickColor": number}`; ColorSequence becomes
//!   `{"ColorSequence": {"keypoints": [{"time", "color": [r, g, b]}]}}` with
//!   black for a keypoint without a color; NumberSequence becomes
//!   `{"NumberSequence": {"keypoints": [{"time", "value", "envelope"}]}}`;
//!   both drop keypoints that are not objects and fields they do not name.
//! - Font loses `_type` and any field that is no string; weight and style
//!   keep the part after their last dot.
//! - A reference with a target in the store becomes
//!   `{"_type":"Ref","instanceIndex": target + 1}`. Any other keeps
//!   instanceId (or its instanceIndex in hex), debugId, pathSegments, path,
//!   settingsId (or referent, or ref) and pathOrdinals, and gains `_type`.
//! - TextWrapped is true wherever TextScaled is true.
//! - An attribute given typed or in its decoded form (`{"BrickColor": 5}`,
//!   `{"Vector3": {...}}`) comes back decoded: a typed number as its number,
//!   an EnumItem with an enumType as `_type`, enumType and name, and Font,
//!   BrickColor and the sequences in the forms above.
//! - The version becomes `SETTINGS_BINARY_VERSION`.
//!
//! `crate::tests::support::run_seeds` names the variables that run more seeds
//! or replay one.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Number, Value, json};

use super::bytecode::{
    SETTINGS_BINARY_PARALLEL_CHUNK_SIZE, SETTINGS_BINARY_PARALLEL_MIN_INSTANCES,
    SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance, decode_settings_bytecode,
    encode_settings_bytecode, encode_settings_bytecode_with_dense_references,
};
use crate::tests::support::{SimRng, run_seeds};

/// One seed of each `Kind` but the ordinary in every run of this many.
const SHAPE_CYCLE: u64 = 24;
/// The fewest instances a document of copies holds: its values compress far
/// more than the decoder's allowance per file byte, as a place of copied
/// parts does, so it decodes on the allowance every file has.
const COPIES: usize = 600;
const DEFAULT_SEEDS: u64 = SHAPE_CYCLE;
/// The deepest hierarchy a store may hold, the root counted.
const MAX_DEPTH: usize = 512;
/// A large document holds one script of this many lines, past a megabyte,
/// as generated modules are.
const LONG_SOURCE_LINES: usize = 80_000;
const SCRIPT_CLASSES: [&str; 3] = ["Script", "LocalScript", "ModuleScript"];
const CLASSES: [&str; 12] = [
    "Folder",
    "Part",
    "Model",
    "Script",
    "LocalScript",
    "ModuleScript",
    "TextLabel",
    "Configuration",
    "ObjectValue",
    "StringValue",
    "Ünï",
    "",
];
const ROOT_NAMES: [&str; 4] = ["Workspace", "ReplicatedStorage", "ServerStorage", "Ünïcødé"];
const ROOT_IDS: [&str; 6] = [
    "1",
    "editor:root",
    "debug:0_0",
    "debug:0_00",
    "debug:0_9223372036854775807",
    "debug:0_9223372036854775808",
];
const NAMES: [&str; 9] = [
    "Part",
    "Part",
    "Folder",
    "",
    "Ünïcødé",
    "日本",
    "a.b",
    "Model",
    "Item",
];
const OUTSIDE_SERVICE: &str = "OtherService";
const TEXTS: [&str; 10] = [
    "",
    "Enemy",
    "Ünïcødé ✓",
    "日本語",
    "line\nbreak",
    "tab\there",
    "emoji 🚀",
    "a.b.c",
    "debug:0_5",
    "nul \u{0} byte",
];
const KEYS: [&str; 9] = [
    "a",
    "b",
    "",
    "Ünï",
    "日本",
    "with space",
    "_x",
    "x.y",
    "Value",
];
const ENUM_TYPES: [&str; 4] = ["Material", "Enum.KeyCode", "SortOrder", "Ünï"];
const ENUM_NAMES: [&str; 5] = ["Plastic", "LayoutOrder", "ButtonL2", "", "Ünï"];
const ATTRIBUTE_NAMES: [&str; 7] = ["Health", "", "Ünï", "Mode", "Speed", "Tint", "Shape"];
const FIXED: [(&str, &[&str]); 6] = [
    ("Vector2", &["x", "y"]),
    ("Vector3", &["x", "y", "z"]),
    ("UDim", &["scale", "offset"]),
    ("UDim2", &["xScale", "xOffset", "yScale", "yOffset"]),
    ("Color3", &["r", "g", "b"]),
    ("Rect", &["minX", "minY", "maxX", "maxY"]),
];
const VECTOR3: usize = 1;
const COLOR3: usize = 4;
/// Rotations many CFrames share, so a CFrame group finds its rotation table
/// smaller than the inline form.
const ROTATIONS: [[f64; 9]; 4] = [
    [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
    [0.0, 0.0, 1.0, 0.0, 1.0, 0.0, -1.0, 0.0, 0.0],
    [1.0, 0.0, 0.0, 0.0, -1.0, 0.0, 0.0, 0.0, -1.0],
    [0.36, 0.48, -0.8, -0.8, 0.6, 0.0, 0.48, 0.64, 0.6],
];
const NONFINITE: [&str; 3] = ["nan", "inf", "-inf"];
/// What shrinking a failing document may spend, in instances run through the
/// laws: a large one still reports within seconds.
const SHRINK_INSTANCES: usize = 200_000;
/// A string no generated document holds.
const ABSENT: &str = "\u{1}no store holds this\u{2}";
/// Properties a store keeps only in the instance record.
const RECORD_FIELDS: [&str; 3] = ["Name", "ClassName", "Parent"];

/// A generated value and the form each encoder's decode must give back.
#[derive(Clone)]
struct Stated {
    input: Value,
    plain: Value,
    /// None where the dense encoder must refuse the document.
    dense: Option<Value>,
}

impl Stated {
    fn new(input: Value, canonical: Value) -> Self {
        Self {
            input,
            dense: Some(canonical.clone()),
            plain: canonical,
        }
    }

    fn kept(value: Value) -> Self {
        Self::new(value.clone(), value)
    }

    fn array(items: Vec<Stated>) -> Self {
        Self {
            input: Value::Array(items.iter().map(|item| item.input.clone()).collect()),
            plain: Value::Array(items.iter().map(|item| item.plain.clone()).collect()),
            dense: items
                .into_iter()
                .map(|item| item.dense)
                .collect::<Option<Vec<_>>>()
                .map(Value::Array),
        }
    }

    fn object(fields: Vec<(String, Stated)>) -> Self {
        let map = |part: fn(&Stated) -> Value| {
            Value::Object(
                fields
                    .iter()
                    .map(|(key, value)| (key.clone(), part(value)))
                    .collect(),
            )
        };
        Self {
            input: map(|value| value.input.clone()),
            plain: map(|value| value.plain.clone()),
            dense: fields
                .iter()
                .map(|(key, value)| Some((key.clone(), value.dense.clone()?)))
                .collect::<Option<Map<_, _>>>()
                .map(Value::Object),
        }
    }

    /// The decoded attribute form as input: decoding gives it, so encoding
    /// takes it.
    fn raw_key(type_name: &str, stated: Stated) -> Self {
        Self::new(json!({ type_name: stated.input }), stated.plain)
    }
}

#[derive(Clone)]
struct Entry {
    attribute: bool,
    name: String,
    value: Stated,
}

#[derive(Clone)]
struct Node {
    id: String,
    name: String,
    class: String,
    parent: Option<usize>,
}

#[derive(Clone)]
struct Doc {
    version: u8,
    nodes: Vec<Node>,
    entries: Vec<Vec<Entry>>,
}

/// Where references can point: each instance's id, and its path as names
/// from the root with each name's ordinal among same-named siblings.
struct Shape {
    root: String,
    ids: Vec<String>,
    paths: Vec<Vec<String>>,
    ordinals: Vec<Vec<usize>>,
    ambiguous: Vec<bool>,
}

impl Shape {
    fn of(nodes: &[Node]) -> Self {
        let mut seen = HashMap::<(Option<usize>, &str), usize>::new();
        let ordinal = nodes
            .iter()
            .map(|node| {
                let count = seen.entry((node.parent, node.name.as_str())).or_insert(0);
                *count += 1;
                *count
            })
            .collect::<Vec<_>>();
        let mut paths = Vec::<Vec<String>>::with_capacity(nodes.len());
        let mut ordinals = Vec::with_capacity(nodes.len());
        for index in 0..nodes.len() {
            let mut chain = vec![index];
            while let Some(parent) = nodes[*chain.last().unwrap()].parent {
                chain.push(parent);
            }
            chain.reverse();
            paths.push(chain.iter().map(|&at| nodes[at].name.clone()).collect());
            ordinals.push(chain.iter().map(|&at| ordinal[at]).collect());
        }
        let mut sharing = HashMap::<&[String], usize>::new();
        for path in &paths {
            *sharing.entry(path.as_slice()).or_default() += 1;
        }
        let ambiguous = paths
            .iter()
            .map(|path| sharing[path.as_slice()] > 1)
            .collect();
        Self {
            root: nodes
                .iter()
                .find(|node| node.parent.is_none())
                .map(|node| node.name.clone())
                .unwrap_or_default(),
            ids: nodes.iter().map(|node| node.id.clone()).collect(),
            paths,
            ordinals,
            ambiguous,
        }
    }
}

fn float_json(number: f64) -> Value {
    Number::from_f64(number).map_or_else(
        || {
            let text = if number.is_nan() {
                "nan"
            } else if number > 0.0 {
                "inf"
            } else {
                "-inf"
            };
            json!({"_type": "Float", "value": text})
        },
        Value::Number,
    )
}

/// A fixed numeric component is stored as the nearest f32, and a finite
/// number past f32's range as the largest finite f32 of its sign.
fn stored_f32(number: f64) -> f32 {
    let value = number as f32;
    if number.is_finite() && value.is_infinite() {
        f32::MAX.copysign(value)
    } else {
        value
    }
}

fn last_part(text: &str) -> &str {
    text.rsplit('.').next().unwrap_or(text)
}

fn outside(fields: &[(&str, Value)]) -> Value {
    let mut reference = Map::new();
    reference.insert("_type".into(), json!("Ref"));
    for (key, value) in fields {
        reference.insert((*key).into(), value.clone());
    }
    Value::Object(reference)
}

struct Gen<'a> {
    rng: &'a mut SimRng,
    shape: &'a Shape,
    pool: Vec<Stated>,
    counter: usize,
    /// Whether the next script's source is the long one.
    long_source: bool,
}

impl Gen<'_> {
    fn chance(&mut self, one_in: usize) -> bool {
        self.rng.below(one_in) == 0
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.rng.below(items.len())]
    }

    fn fresh(&mut self) -> usize {
        self.counter += 1;
        self.counter
    }

    fn text(&mut self) -> String {
        match self.rng.below(14) {
            0 => "x".repeat(200 + self.rng.below(200)),
            1 | 2 => format!("s{}", self.rng.below(1000)),
            _ => self.pick(&TEXTS).to_string(),
        }
    }

    fn integer(&mut self) -> i64 {
        match self.rng.below(3) {
            0 => self.pick(&[
                0,
                1,
                -1,
                63,
                64,
                -64,
                -65,
                127,
                128,
                -129,
                300,
                -300,
                1 << 40,
                i64::MAX,
                i64::MIN,
            ]),
            1 => self.rng.below(1000) as i64 - 500,
            _ => (self.rng.next() as i64) >> self.rng.below(64),
        }
    }

    fn unsigned(&mut self) -> u64 {
        match self.rng.below(3) {
            0 => (1 << 63) + self.rng.below(1000) as u64,
            1 => u64::MAX - self.rng.below(1000) as u64,
            _ => self.rng.next() | (1 << 63),
        }
    }

    /// A float an f32 holds exactly.
    fn single(&mut self) -> f64 {
        if self.chance(2) {
            return f64::from(self.pick(&[
                0.5,
                -0.0,
                1.0,
                -2.5,
                16_777_216.0,
                f32::MAX,
                f32::MIN,
                f32::MIN_POSITIVE,
                f32::from_bits(1),
                -3.0e38,
            ]));
        }
        let value = f32::from_bits(self.rng.next() as u32);
        f64::from(if value.is_finite() { value } else { 1.5 })
    }

    /// A finite float no f32 holds.
    fn double(&mut self) -> f64 {
        if self.chance(2) {
            return self.pick(&[
                0.1,
                -0.3,
                1e300,
                -1e-300,
                f64::MAX,
                f64::MIN_POSITIVE,
                5e-324,
                3.5e38,
                -3.5e38,
                16_777_217.0,
            ]);
        }
        let value = f64::from_bits(self.rng.next());
        if value.is_finite() && f64::from(value as f32) != value {
            value
        } else {
            0.1
        }
    }

    /// A non-finite number in one of its two JSON forms, and its value.
    fn nonfinite(&mut self) -> (Value, f64) {
        let text = self.pick(&NONFINITE);
        let number = match text {
            "nan" => f64::NAN,
            "inf" => f64::INFINITY,
            _ => f64::NEG_INFINITY,
        };
        let value = if self.chance(2) {
            json!({"_type": "Float", "value": text})
        } else {
            json!({"t": "numeric", "v": text})
        };
        (value, number)
    }

    /// A number where the codec reads any number: its JSON and its value.
    fn component(&mut self) -> (Value, f64) {
        match self.rng.below(8) {
            0 => {
                let value = self.integer();
                (json!(value), value as f64)
            }
            1 => {
                let value = self.unsigned();
                (json!(value), value as f64)
            }
            2 | 3 => {
                let value = self.single();
                (json!(value), value)
            }
            4 => {
                let value = self.double();
                (json!(value), value)
            }
            5 => {
                let value = self.pick(&[1e39, -1e39, 1e-50, -1e-50]);
                (json!(value), value)
            }
            6 => self.nonfinite(),
            _ => {
                let value = self.rng.below(200) as i64 - 100;
                (json!(value), value as f64)
            }
        }
    }

    fn scalar(&mut self) -> Stated {
        Stated::kept(match self.rng.below(8) {
            0 => json!(self.chance(2)),
            1 => json!(self.integer()),
            2 => json!(self.unsigned()),
            3 => json!(self.single()),
            4 => json!(self.double()),
            5 => self.nonfinite().0,
            _ => json!(self.text()),
        })
    }

    /// Any value a property holds, in its top-level canonical form.
    fn property(&mut self) -> Stated {
        match self.rng.below(20) {
            0 => Stated::kept(Value::Null),
            1..=6 => self.scalar(),
            7 => self.numeric_array(),
            8 => self.mixed_array(0),
            9 => Stated::kept(json!([])),
            10 => self.object(0),
            11 => self.typed_object(),
            12 => {
                let which = self.rng.below(FIXED.len());
                self.fixed(which)
            }
            13 => {
                let shared = self.chance(2);
                self.cframe(shared)
            }
            14 => self.enum_item(false),
            15 => self.brick_color(),
            16 | 17 => self.reference(),
            18 => self.typed_value(),
            _ => self.shared(),
        }
    }

    /// Any value inside an array or object, in its nested canonical form.
    fn nested(&mut self, depth: usize) -> Stated {
        let kinds = if depth < 2 { 18 } else { 11 };
        match self.rng.below(kinds) {
            0 => Stated::kept(Value::Null),
            1..=4 => self.scalar(),
            5 => self.enum_item(true),
            6 => self.reference(),
            7 => {
                let which = self.rng.below(FIXED.len());
                self.fixed(which)
            }
            8 => self.cframe(false),
            9 => self.brick_color(),
            10 => self.typed_value(),
            11 => self.numeric_array(),
            12 | 13 => self.mixed_array(depth + 1),
            14 => self.object(depth + 1),
            15 => self.typed_object(),
            16 => Stated::kept(json!([])),
            _ => Stated::kept(json!({})),
        }
    }

    /// A typed Font, ColorSequence or NumberSequence, which a property
    /// stores in its decoded form.
    fn typed_value(&mut self) -> Stated {
        match self.rng.below(3) {
            0 => {
                let (mut input, canonical) = self.font_fields();
                input.insert("_type".into(), json!("Font"));
                Stated::new(Value::Object(input), Value::Object(canonical))
            }
            1 => self.color_sequence(false),
            _ => self.number_sequence(false),
        }
    }

    fn numeric_array(&mut self) -> Stated {
        let exact = self.chance(2);
        let len = 1 + self.rng.below(6);
        let (input, canonical) = (0..len)
            .map(|_| {
                let (value, number) = if exact {
                    let value = self.single();
                    (json!(value), value)
                } else {
                    self.component()
                };
                (value, float_json(number))
            })
            .unzip();
        Stated::new(Value::Array(input), Value::Array(canonical))
    }

    /// An array with at least one item that is no number.
    fn mixed_array(&mut self, depth: usize) -> Stated {
        let mut items = (0..self.rng.below(4))
            .map(|_| self.nested(depth))
            .collect::<Vec<_>>();
        let odd = match self.rng.below(3) {
            0 => Value::Null,
            1 => json!(self.chance(2)),
            _ => json!(self.text()),
        };
        let at = self.rng.below(items.len() + 1);
        items.insert(at, Stated::kept(odd));
        Stated::array(items)
    }

    fn object(&mut self, depth: usize) -> Stated {
        let mut fields = Vec::new();
        for _ in 0..self.rng.below(5) {
            let key = self.pick(&KEYS).to_string();
            if fields.iter().all(|(held, _)| *held != key) {
                let value = self.nested(depth);
                fields.push((key, value));
            }
        }
        Stated::object(fields)
    }

    fn tags(&mut self) -> Stated {
        let tags = (0..self.rng.below(4))
            .map(|_| json!(self.pick(&TEXTS[..4])))
            .collect();
        Stated::kept(Value::Array(tags))
    }

    /// An object whose type the codec writes as a plain object.
    fn typed_object(&mut self) -> Stated {
        Stated::kept(match self.rng.below(5) {
            0 => {
                json!({"_type": "BinaryString", "base64": self.pick(&["", "AAEC", "UGxhc3RpYw=="])})
            }
            1 => json!({"_type": "NumberRange", "min": self.integer(), "max": self.single()}),
            2 => json!({"_type": "PhysicalProperties", "density": 0.7, "friction": self.double()}),
            3 => json!({"_type": "Faces", "top": true, "bottom": self.chance(2)}),
            _ => json!({"_type": 7, "x": self.integer()}),
        })
    }

    fn fixed(&mut self, which: usize) -> Stated {
        let (type_name, fields) = FIXED[which];
        let mut input = Map::new();
        let mut canonical = Map::new();
        input.insert("_type".into(), json!(type_name));
        canonical.insert("_type".into(), json!(type_name));
        for field in fields {
            let (value, number) = self.component();
            input.insert((*field).into(), value);
            canonical.insert((*field).into(), float_json(f64::from(stored_f32(number))));
        }
        if self.chance(6) {
            input.insert("extra".into(), json!(1));
        }
        Stated::new(Value::Object(input), Value::Object(canonical))
    }

    /// A CFrame; a shared one takes its rotation from `ROTATIONS`.
    fn cframe(&mut self, shared: bool) -> Stated {
        let mut parts = (0..3).map(|_| self.component()).collect::<Vec<_>>();
        if shared {
            let rotation = self.pick(&ROTATIONS);
            for number in rotation {
                let value = if number.fract() == 0.0 && self.chance(2) {
                    json!(number as i64)
                } else {
                    json!(number)
                };
                parts.push((value, number));
            }
        } else {
            parts.extend((0..9).map(|_| self.component()));
        }
        let (input, canonical): (Vec<_>, Vec<_>) = parts
            .into_iter()
            .map(|(value, number)| (value, float_json(f64::from(stored_f32(number)))))
            .unzip();
        let mut value = json!({"_type": "CFrame", "components": input});
        if self.chance(8) {
            value["extra"] = json!(true);
        }
        Stated::new(value, json!({"_type": "CFrame", "components": canonical}))
    }

    fn enum_item(&mut self, nested: bool) -> Stated {
        let enum_type = match self.rng.below(5) {
            0 => None,
            1 => Some(json!("")),
            2 => Some(json!(3)),
            _ => Some(json!(self.pick(&ENUM_TYPES))),
        };
        let name = match self.rng.below(6) {
            0 => None,
            1 => Some(json!(5)),
            _ => Some(json!(self.pick(&ENUM_NAMES))),
        };
        let mut input = Map::new();
        input.insert("_type".into(), json!("EnumItem"));
        if let Some(enum_type) = &enum_type {
            input.insert("enumType".into(), enum_type.clone());
        }
        if let Some(name) = &name {
            input.insert("name".into(), name.clone());
        }
        if self.chance(2) {
            input.insert("value".into(), json!(self.rng.below(300)));
        }
        let name = name.as_ref().and_then(Value::as_str);
        let enum_type = enum_type
            .as_ref()
            .and_then(Value::as_str)
            .filter(|enum_type| !enum_type.is_empty());
        let canonical = match (nested, enum_type, name) {
            (true, Some(enum_type), Some(name)) => {
                json!({"_type": "EnumItem", "enumType": enum_type, "name": name})
            }
            _ => json!({"_type": "EnumItem", "name": name.unwrap_or("")}),
        };
        Stated::new(Value::Object(input), canonical)
    }

    fn brick_color(&mut self) -> Stated {
        let number = match self.rng.below(4) {
            0 => None,
            1 => Some(json!(self.single())),
            _ => Some(json!(self.rng.below(1033))),
        };
        let mut input = json!({"_type": "BrickColor"});
        if let Some(number) = &number {
            input["number"] = number.clone();
        }
        if self.chance(4) {
            input["name"] = json!("Medium stone grey");
        }
        Stated::new(input, json!({"BrickColor": number}))
    }

    fn font_fields(&mut self) -> (Map<String, Value>, Map<String, Value>) {
        let mut input = Map::new();
        input.insert(
            "family".into(),
            json!(self.pick(&[
                "rbxasset://fonts/families/SourceSansPro.json",
                "rbxasset://fonts/families/Ünï.json"
            ])),
        );
        input.insert(
            "weight".into(),
            json!(self.pick(&[
                "Regular",
                "Bold",
                "Enum.FontWeight.Bold",
                "Enum.FontWeight.Light"
            ])),
        );
        input.insert(
            "style".into(),
            json!(self.pick(&["Normal", "Italic", "Enum.FontStyle.Italic"])),
        );
        if self.chance(2) {
            input.insert(
                "cachedFaceId".into(),
                json!("rbxasset://fonts/SourceSansPro-Regular.ttf"),
            );
        }
        let canonical = input
            .iter()
            .map(|(key, value)| {
                let text = value.as_str().unwrap_or_default();
                let kept = if matches!(key.as_str(), "weight" | "style") {
                    last_part(text)
                } else {
                    text
                };
                (key.clone(), json!(kept))
            })
            .collect();
        (input, canonical)
    }

    /// A ColorSequence; `raw_key` gives it as an attribute decodes, with a
    /// color array per keypoint.
    fn color_sequence(&mut self, raw_key: bool) -> Stated {
        let mut input = Vec::new();
        let mut canonical = Vec::new();
        for _ in 0..self.rng.below(4) {
            let time = (!self.chance(5)).then(|| json!(self.single()));
            let rgb = [self.component(), self.component(), self.component()];
            let color = rgb.iter().map(|(_, number)| float_json(*number)).collect();
            let given = rgb.iter().map(|(value, _)| value.clone());
            let mut keypoint = json!({});
            if let Some(time) = &time {
                keypoint["time"] = time.clone();
            }
            if raw_key {
                keypoint["color"] = Value::Array(given.collect());
            } else if self.chance(6) {
                keypoint["value"] = json!("red");
            } else {
                let [r, g, b] = rgb.map(|(value, _)| value);
                keypoint["value"] = json!({"r": r, "g": g, "b": b});
            }
            let color = if keypoint.get("value").is_some_and(Value::is_string) {
                json!([0.0, 0.0, 0.0])
            } else {
                Value::Array(color)
            };
            canonical.push(json!({"time": time, "color": color}));
            input.push(keypoint);
            if !raw_key && self.chance(5) {
                input.push(json!(5));
            }
        }
        let canonical = json!({"ColorSequence": {"keypoints": canonical}});
        if raw_key {
            return Stated::new(json!({"ColorSequence": {"keypoints": input}}), canonical);
        }
        Stated::new(
            json!({"_type": "ColorSequence", "keypoints": input}),
            canonical,
        )
    }

    /// A NumberSequence; `raw_key` gives it as an attribute decodes.
    fn number_sequence(&mut self, raw_key: bool) -> Stated {
        let mut input = Vec::new();
        let mut canonical = Vec::new();
        for _ in 0..self.rng.below(4) {
            let mut keypoint = json!({});
            let mut stated = json!({});
            for key in ["time", "value", "envelope"] {
                let value = (!self.chance(5)).then(|| self.scalar().input);
                if let Some(value) = &value {
                    keypoint[key] = value.clone();
                }
                stated[key] = value.unwrap_or(Value::Null);
            }
            if self.chance(6) {
                keypoint["extra"] = json!(1);
            }
            input.push(keypoint);
            canonical.push(stated);
            if !raw_key && self.chance(5) {
                input.push(json!("not a keypoint"));
            }
        }
        let canonical = json!({"NumberSequence": {"keypoints": canonical}});
        if raw_key {
            return Stated::new(json!({"NumberSequence": {"keypoints": input}}), canonical);
        }
        Stated::new(
            json!({"_type": "NumberSequence", "keypoints": input}),
            canonical,
        )
    }

    /// One of a few values every document repeats across its instances.
    fn shared(&mut self) -> Stated {
        if self.pool.len() < 4 {
            let value = match self.pool.len() {
                0 => Stated::kept(json!(self.text())),
                1 => self.cframe(true),
                2 => self.fixed(VECTOR3),
                _ => self.numeric_array(),
            };
            self.pool.push(value);
        }
        self.pool[self.rng.below(self.pool.len())].clone()
    }

    /// The path selectors of an instance of the store; ordinals where its
    /// path alone names more than one instance.
    fn local_path(&mut self, target: usize) -> Vec<(&'static str, Value)> {
        let mut fields = vec![("pathSegments", json!(self.shape.paths[target]))];
        if self.shape.ambiguous[target] || self.chance(3) {
            fields.push(("pathOrdinals", json!(self.shape.ordinals[target])));
        }
        fields
    }

    /// Selectors that name no instance of the store.
    fn missing_path(&mut self) -> Vec<(&'static str, Value)> {
        let target = self.rng.below(self.shape.ids.len());
        if self.chance(2) {
            let mut ordinals = self.shape.ordinals[target].clone();
            *ordinals.last_mut().unwrap() = 100_000;
            return vec![
                ("pathSegments", json!(self.shape.paths[target])),
                ("pathOrdinals", json!(ordinals)),
            ];
        }
        let segments = [self.shape.root.clone(), format!("Missing{}", self.fresh())];
        vec![("pathSegments", json!(segments))]
    }

    /// Selectors whose path leads into another service: none of them can
    /// name an instance of this store, whatever ids they carry.
    fn leading_out(&mut self) -> (Vec<(&'static str, Value)>, Value) {
        let mut segments = vec![OUTSIDE_SERVICE.to_string()];
        segments.extend((0..self.rng.below(3)).map(|_| self.pick(&NAMES).to_string()));
        let mut given = vec![("pathSegments", json!(segments))];
        let mut kept = Vec::new();
        let index = (!self.chance(3)).then(|| 1 + self.rng.below(self.shape.ids.len() + 5));
        let instance_id = self.chance(2).then(|| format!("{:x}", self.fresh() + 4096));
        if let Some(index) = index {
            given.push(("instanceIndex", json!(index)));
        }
        if let Some(instance_id) = &instance_id {
            given.push(("instanceId", json!(instance_id)));
        }
        if let Some(instance_id) = instance_id.or(index.map(|index| format!("{index:x}"))) {
            kept.push(("instanceId", json!(instance_id)));
        }
        if self.chance(3) {
            given.push(("debugId", json!("0_77")));
            kept.push(("debugId", json!("0_77")));
        }
        kept.push(("pathSegments", json!(segments)));
        if self.chance(3) {
            given.push(("path", json!("Other.Thing")));
            kept.push(("path", json!("Other.Thing")));
        }
        if self.chance(2) {
            let target = self.rng.below(self.shape.ids.len());
            let id = json!(self.shape.ids[target]);
            let key = self.pick(&["settingsId", "referent", "ref"]);
            given.push((key, id.clone()));
            kept.push(("settingsId", id));
        }
        if self.chance(3) {
            let ordinals = vec![1; segments.len()];
            given.push(("pathOrdinals", json!(ordinals)));
            kept.push(("pathOrdinals", json!(ordinals)));
        }
        (given, outside(&kept))
    }

    fn reference(&mut self) -> Stated {
        let target = self.rng.below(self.shape.ids.len());
        let index = json!(target + 1);
        let id = json!(self.shape.ids[target]);
        let resolved = json!({"_type": "Ref", "instanceIndex": target + 1});
        let (selectors, plain, dense) = match self.rng.below(11) {
            0 => (
                vec![("instanceIndex", index)],
                resolved.clone(),
                Some(resolved),
            ),
            1 => (
                vec![("settingsId", id.clone())],
                resolved,
                Some(outside(&[("settingsId", id)])),
            ),
            2 => {
                let alias = self.pick(&["referent", "ref"]);
                (
                    vec![(alias, id.clone())],
                    resolved,
                    Some(outside(&[("settingsId", id)])),
                )
            }
            3 => {
                let path = self.local_path(target);
                let kept = outside(&path);
                (path, resolved, Some(kept))
            }
            4 => (
                vec![("instanceIndex", index), ("settingsId", id)],
                resolved.clone(),
                Some(resolved),
            ),
            5 => {
                let mut selectors = self.local_path(target);
                selectors.push(("settingsId", id));
                let kept = outside(&selectors);
                (selectors, resolved, Some(kept))
            }
            6 => {
                let mut selectors = self.local_path(target);
                selectors.push(("instanceIndex", index));
                (selectors, resolved, None)
            }
            7 => {
                let missing = json!(format!("gone:{}", self.fresh()));
                let key = self.pick(&["settingsId", "referent", "ref"]);
                let kept = outside(&[("settingsId", missing.clone())]);
                (vec![(key, missing)], kept.clone(), Some(kept))
            }
            8 => {
                let mut selectors = Vec::new();
                for (key, value) in [
                    ("instanceId", json!("4a")),
                    ("debugId", json!("0_123")),
                    ("path", json!("Workspace.Model.Part")),
                ] {
                    if self.chance(2) || (selectors.is_empty() && key == "path") {
                        selectors.push((key, value));
                    }
                }
                let kept = outside(&selectors);
                (selectors, kept.clone(), Some(kept))
            }
            9 => {
                let selectors = self.missing_path();
                let kept = outside(&selectors);
                (selectors, kept.clone(), Some(kept))
            }
            _ => {
                let (selectors, kept) = self.leading_out();
                (selectors, kept.clone(), Some(kept))
            }
        };
        let wrapped = self.chance(5);
        let mut input = Map::new();
        if !wrapped || self.chance(2) {
            input.insert("_type".into(), json!("Ref"));
        }
        for (key, value) in selectors {
            input.insert(key.into(), value);
        }
        let input = if wrapped {
            json!({"Ref": input})
        } else {
            Value::Object(input)
        };
        Stated {
            input,
            plain,
            dense,
        }
    }

    fn attribute(&mut self) -> Stated {
        match self.rng.below(17) {
            0 => Stated::kept(json!(self.chance(2))),
            1 => Stated::kept(json!(self.integer())),
            2 => Stated::kept(json!(self.unsigned())),
            3 => Stated::kept(json!(self.single())),
            4 => Stated::kept(json!(self.double())),
            5 => {
                let number = self.scalar_number();
                let type_name = self.pick(&["Float64", "Float32", "Int32", "Int64"]);
                Stated::new(json!({"_type": type_name, "value": number}), number)
            }
            6 => Stated::kept(json!({"_type": "Float", "value": self.pick(&NONFINITE)})),
            7 => Stated::kept(json!(self.text())),
            8 => {
                let which = self.rng.below(FIXED.len());
                let stated = self.fixed(which);
                self.maybe_raw_key(FIXED[which].0, stated)
            }
            9 => {
                let shared = self.chance(2);
                let stated = self.cframe(shared);
                self.maybe_raw_key("CFrame", stated)
            }
            10 => self.enum_attribute(),
            11 => {
                let (mut input, canonical) = self.font_fields();
                if self.chance(2) {
                    input.insert("_type".into(), json!("Font"));
                }
                Stated::new(Value::Object(input), Value::Object(canonical))
            }
            12 => {
                let raw_key = self.chance(2);
                self.color_sequence(raw_key)
            }
            13 => {
                let raw_key = self.chance(2);
                self.number_sequence(raw_key)
            }
            14 => {
                let number = json!(self.rng.below(1033));
                let input = if self.chance(2) {
                    json!({"_type": "BrickColor", "number": number})
                } else {
                    json!({"BrickColor": number})
                };
                Stated::new(input, json!({"BrickColor": number}))
            }
            15 => {
                let (type_name, value) = if self.chance(2) {
                    let value = json!({"_type": "NumberRange", "min": self.integer(), "max": self.single()});
                    ("NumberRange", value)
                } else {
                    let value =
                        json!({"_type": "BinaryString", "base64": self.pick(&["", "AAEC"])});
                    ("BinaryString", value)
                };
                self.maybe_raw_key(type_name, Stated::kept(value))
            }
            _ => Stated::kept(json!({"_type": "Enum", "name": "Material"})),
        }
    }

    fn scalar_number(&mut self) -> Value {
        match self.rng.below(3) {
            0 => json!(self.integer()),
            1 => json!(self.single()),
            _ => json!(self.double()),
        }
    }

    fn maybe_raw_key(&mut self, type_name: &str, stated: Stated) -> Stated {
        if self.chance(2) {
            Stated::raw_key(type_name, stated)
        } else {
            stated
        }
    }

    fn enum_attribute(&mut self) -> Stated {
        let name = self.pick(&ENUM_NAMES);
        if self.chance(3) {
            let mut input = json!({"_type": "EnumItem", "name": name});
            if self.chance(2) {
                input["value"] = json!(7);
            }
            return Stated::new(input, json!({"_type": "EnumItem", "name": name}));
        }
        let enum_type = if self.chance(5) {
            ""
        } else {
            self.pick(&ENUM_TYPES)
        };
        let input = if self.chance(2) {
            json!({"_type": "EnumItem", "enumType": enum_type, "name": name, "value": 3})
        } else {
            json!({"EnumItem": {"enumType": enum_type, "name": name}})
        };
        Stated::new(
            input,
            json!({"_type": "EnumItem", "enumType": enum_type, "name": name}),
        )
    }

    /// A property the encoder refuses: one named for a record field, or an
    /// object named Attributes.
    fn refused_property(&mut self) -> (String, Stated) {
        if self.chance(2) {
            return ("Attributes".into(), self.object(0));
        }
        let name = self.pick(&RECORD_FIELDS).to_string();
        (name, Stated::kept(json!(self.text())))
    }

    /// The properties next to the refused ones that a store keeps: a record
    /// field that is null, dropped as any null property is, and an
    /// Attributes property that is no object.
    fn near_refused_property(&mut self) -> (&'static str, Stated) {
        if self.chance(2) {
            return (self.pick(&RECORD_FIELDS), Stated::kept(Value::Null));
        }
        ("Attributes", Stated::kept(json!(self.text())))
    }

    fn named_property(&mut self) -> (String, Stated) {
        let (name, value) = match self.rng.below(16) {
            13 => self.near_refused_property(),
            0 | 1 => ("Value", self.property()),
            2 => ("Size", self.fixed(VECTOR3)),
            3 => ("CFrame", self.cframe(true)),
            4 => ("PivotOffset", self.cframe(false)),
            5 => ("Tags", self.tags()),
            6 => ("Color", self.fixed(COLOR3)),
            7 => ("Target", self.reference()),
            8 => ("Text", Stated::kept(json!(self.text()))),
            9 => ("", self.property()),
            10 => ("Ünïcødé", self.property()),
            11 => ("Data", self.object(0)),
            12 => ("Material", self.enum_item(false)),
            _ => ("Shared", self.shared()),
        };
        (name.to_string(), value)
    }

    /// A value of each fixed numeric type and two CFrames: a large document
    /// gives every instance fresh ones, so each group decodes in parallel
    /// chunks, and a document of copies gives every instance the same ones.
    fn columns(&mut self) -> Vec<(String, Stated)> {
        let mut columns = (0..FIXED.len())
            .map(|which| (format!("Column{which}"), self.fixed(which)))
            .collect::<Vec<_>>();
        columns.push(("CFrame".into(), self.cframe(true)));
        columns.push(("PivotOffset".into(), self.cframe(false)));
        columns
    }

    fn text_properties(&mut self) -> Vec<(String, Stated)> {
        let mut entries = Vec::new();
        for (name, choices) in [
            ("TextScaled", [json!(true), json!(false), Value::Null]),
            ("TextWrapped", [json!(true), json!(false), json!(1)]),
        ] {
            if !self.chance(4) {
                let value = choices[self.rng.below(choices.len())].clone();
                entries.push((name.to_string(), Stated::kept(value)));
            }
        }
        entries
    }

    fn source(&mut self) -> String {
        if std::mem::take(&mut self.long_source) {
            return "local value = 1\n".repeat(LONG_SOURCE_LINES + self.rng.below(1000));
        }
        self.pick(&[
            "",
            "print(\"hi\")\n",
            "-- Ünïcødé ✓\nreturn {}\n",
            "local t = {}\nfor i = 1, 10 do\n\tt[i] = i\nend\nreturn t\n",
        ])
        .to_string()
    }

    fn entries(&mut self, class: &str, large: bool) -> Vec<Entry> {
        let mut properties = if large { self.columns() } else { Vec::new() };
        if SCRIPT_CLASSES.contains(&class) && !self.chance(4) {
            let source = self.source();
            properties.push(("Source".into(), Stated::kept(json!(source))));
        }
        if class == "TextLabel" {
            properties.extend(self.text_properties());
        }
        for _ in 0..self.rng.below(if large { 2 } else { 6 }) {
            let (name, value) = self.named_property();
            if properties.iter().all(|(held, _)| *held != name) {
                properties.push((name, value));
            }
        }
        let mut entries = properties
            .into_iter()
            .map(|(name, value)| Entry {
                attribute: false,
                name,
                value,
            })
            .collect::<Vec<_>>();
        for _ in 0..self.rng.below(if large { 2 } else { 4 }) {
            let name = self.pick(&ATTRIBUTE_NAMES).to_string();
            if entries
                .iter()
                .all(|entry| !entry.attribute || entry.name != name)
            {
                let value = self.attribute();
                entries.push(Entry {
                    attribute: true,
                    name,
                    value,
                });
            }
        }
        entries
    }
}

fn settings_id(rng: &mut SimRng, index: usize, used: &mut HashSet<String>) -> String {
    let id = match rng.below(11) {
        0 => format!("debug:0_{index}"),
        1 => format!("debug:0_{}", (u64::MAX >> 1) - index as u64),
        2 => format!("debug:0_0{index}"),
        3 => format!("debug:0_{}", (1_u64 << 63) + index as u64),
        4 => format!("{index:x}"),
        5 => format!("Ünï-{index}"),
        6 => "debug:0_".to_string(),
        7 => format!("debug:0_{index}:"),
        8 => format!("debug:1_{index}"),
        _ => format!("editor:{index}"),
    };
    if used.insert(id.clone()) {
        return id;
    }
    let id = format!("editor:{index}");
    used.insert(id.clone());
    id
}

/// Instances in creation order, each child after its parent, then in another
/// order half the time: a store may list a parent after its children.
fn nodes(rng: &mut SimRng, count: usize, deepest: bool) -> Vec<Node> {
    let mut used = HashSet::new();
    let mut nodes = Vec::with_capacity(count);
    for index in 0..count {
        let (id, name, parent) = if index == 0 {
            let id = ROOT_IDS[rng.below(ROOT_IDS.len())].to_string();
            used.insert(id.clone());
            (
                id,
                ROOT_NAMES[rng.below(ROOT_NAMES.len())].to_string(),
                None,
            )
        } else {
            let name = if rng.below(3) == 0 {
                format!("N{index}")
            } else {
                NAMES[rng.below(NAMES.len())].to_string()
            };
            let parent = if deepest && index < MAX_DEPTH {
                index - 1
            } else {
                rng.below(index)
            };
            (settings_id(rng, index, &mut used), name, Some(parent))
        };
        nodes.push(Node {
            id,
            name,
            class: CLASSES[rng.below(CLASSES.len())].to_string(),
            parent,
        });
    }
    if rng.below(2) == 0 {
        return nodes;
    }
    let mut order = (0..count).collect::<Vec<_>>();
    rng.shuffle(&mut order);
    let mut position = vec![0; count];
    for (at, &index) in order.iter().enumerate() {
        position[index] = at;
    }
    order
        .iter()
        .map(|&index| {
            let node = &nodes[index];
            Node {
                parent: node.parent.map(|parent| position[parent]),
                ..node.clone()
            }
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Ordinary,
    /// Past the codec's parallel thresholds.
    Large,
    /// A branch at the deepest hierarchy a store may hold.
    Deepest,
    /// Every instance holds the same values.
    Copies,
    /// An ordinary document with one property the encoders refuse.
    Refused,
    Empty,
}

fn generate(seed: u64) -> Doc {
    let mut rng = SimRng(seed);
    let kind = match seed % SHAPE_CYCLE {
        3 => Kind::Large,
        7 => Kind::Copies,
        11 => Kind::Deepest,
        15 => Kind::Refused,
        19 => Kind::Empty,
        _ => Kind::Ordinary,
    };
    let count = match kind {
        Kind::Ordinary | Kind::Refused => 1 + rng.below(40),
        Kind::Large => {
            SETTINGS_BINARY_PARALLEL_CHUNK_SIZE.max(SETTINGS_BINARY_PARALLEL_MIN_INSTANCES)
                + 1
                + rng.below(64)
        }
        Kind::Deepest => MAX_DEPTH + rng.below(16),
        Kind::Copies => COPIES + rng.below(100),
        Kind::Empty => 0,
    };
    let nodes = nodes(&mut rng, count, kind == Kind::Deepest);
    let shape = Shape::of(&nodes);
    let mut generator = Gen {
        rng: &mut rng,
        shape: &shape,
        pool: Vec::new(),
        counter: 0,
        long_source: kind == Kind::Large,
    };
    let mut entries: Vec<Vec<Entry>> = if kind == Kind::Copies {
        let copy = generator.columns();
        let entries = copy
            .into_iter()
            .map(|(name, value)| Entry {
                attribute: false,
                name,
                value,
            })
            .collect::<Vec<_>>();
        vec![entries; nodes.len()]
    } else {
        nodes
            .iter()
            .map(|node| generator.entries(&node.class, kind == Kind::Large))
            .collect()
    };
    if kind == Kind::Refused {
        let (name, value) = generator.refused_property();
        let held = &mut entries[generator.rng.below(nodes.len())];
        held.retain(|entry| entry.attribute || entry.name != name);
        held.push(Entry {
            attribute: false,
            name,
            value,
        });
    }
    let version = [SETTINGS_BINARY_VERSION, SETTINGS_BINARY_VERSION - 1, 0][rng.below(3)];
    Doc {
        version,
        nodes,
        entries,
    }
}

impl Doc {
    fn instance(node: &Node) -> SettingsBytecodeInstance {
        SettingsBytecodeInstance::new(
            node.id.clone(),
            node.name.clone(),
            node.class.clone(),
            node.parent,
        )
    }

    fn input(&self) -> SettingsBytecode {
        let instances = self
            .nodes
            .iter()
            .zip(&self.entries)
            .map(|(node, entries)| {
                let mut instance = Self::instance(node);
                for entry in entries {
                    let values = if entry.attribute {
                        &mut instance.attributes
                    } else {
                        &mut instance.properties
                    };
                    values.insert(entry.name.clone(), entry.value.input.clone());
                }
                instance
            })
            .collect();
        SettingsBytecode {
            version: self.version,
            instances,
        }
    }

    /// Whether both encoders must refuse the document: it holds a property
    /// named for a record field, or an object named Attributes.
    fn refused(&self) -> bool {
        self.entries.iter().flatten().any(|entry| {
            !entry.attribute
                && !entry.value.input.is_null()
                && (RECORD_FIELDS.contains(&entry.name.as_str())
                    || (entry.name == "Attributes" && entry.value.input.is_object()))
        })
    }

    /// What decoding an encoder's bytes must give; None where the dense
    /// encoder must refuse the document.
    fn stated(&self, dense: bool) -> Option<SettingsBytecode> {
        let instances = self
            .nodes
            .iter()
            .zip(&self.entries)
            .map(|(node, entries)| {
                let mut instance = Self::instance(node);
                for entry in entries {
                    let value = if dense {
                        entry.value.dense.clone()?
                    } else {
                        entry.value.plain.clone()
                    };
                    if entry.attribute {
                        instance.attributes.insert(entry.name.clone(), value);
                    } else if !entry.value.input.is_null() {
                        instance.properties.insert(entry.name.clone(), value);
                    }
                }
                let given = |name: &str| {
                    entries
                        .iter()
                        .find(|entry| !entry.attribute && entry.name == name)
                        .map(|entry| &entry.value.input)
                };
                if given("TextScaled") == Some(&json!(true))
                    && given("TextWrapped") != Some(&json!(true))
                {
                    instance
                        .properties
                        .insert("TextWrapped".into(), json!(true));
                }
                Some(instance)
            })
            .collect::<Option<Vec<_>>>()?;
        Some(SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        })
    }

    fn entry(&self, index: usize, attribute: bool, name: &str) -> Option<&Entry> {
        self.entries
            .get(index)?
            .iter()
            .find(|entry| entry.attribute == attribute && entry.name == name)
    }
}

/// Exact equality: integers and floats differ, and floats compare by bits.
fn same(left: &Value, right: &Value) -> bool {
    // A decoded document holds a payload of some size by its handle.
    let held = (
        crate::settings::held::resolve(left),
        crate::settings::held::resolve(right),
    );
    let (left, right): (&Value, &Value) = (&held.0, &held.1);
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => {
            if left.is_f64() || right.is_f64() {
                left.is_f64()
                    && right.is_f64()
                    && left.as_f64().map(f64::to_bits) == right.as_f64().map(f64::to_bits)
            } else {
                left.as_i64() == right.as_i64() && left.as_u64() == right.as_u64()
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| same(left, right))
        }
        (Value::Object(left), Value::Object(right)) => same_map(left, right),
        _ => left == right,
    }
}

fn same_map(left: &Map<String, Value>, right: &Map<String, Value>) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .all(|(key, value)| right.get(key).is_some_and(|other| same(value, other)))
}

pub(crate) fn same_document(left: &SettingsBytecode, right: &SettingsBytecode) -> bool {
    left.version == right.version
        && left.instances.len() == right.instances.len()
        && left
            .instances
            .iter()
            .zip(&right.instances)
            .all(|(left, right)| {
                left.settings_id == right.settings_id
                    && left.name == right.name
                    && left.class_name == right.class_name
                    && left.parent_index == right.parent_index
                    && same_map(&left.properties, &right.properties)
                    && same_map(&left.attributes, &right.attributes)
            })
}

pub(crate) fn captured_input(seed: u64) -> SettingsBytecode {
    generate(seed).input()
}

fn shown(value: &Value) -> String {
    let text = value.to_string();
    if text.chars().count() > 240 {
        format!("{}...", text.chars().take(240).collect::<String>())
    } else {
        text
    }
}

/// `decoded` holds the instances of `given`, with the decoded version.
fn records_kept(
    given: &SettingsBytecode,
    decoded: &SettingsBytecode,
    law: &str,
) -> Result<(), String> {
    if decoded.version != SETTINGS_BINARY_VERSION {
        return Err(format!("{law}: decoded version {}", decoded.version));
    }
    if decoded.instances.len() != given.instances.len() {
        return Err(format!(
            "{law}: {} instances decode as {}",
            given.instances.len(),
            decoded.instances.len()
        ));
    }
    let record = |instance: &SettingsBytecodeInstance| {
        (
            instance.settings_id.clone(),
            instance.name.clone(),
            instance.class_name.clone(),
            instance.parent_index,
        )
    };
    for (index, (given, decoded)) in given.instances.iter().zip(&decoded.instances).enumerate() {
        if record(given) != record(decoded) {
            return Err(format!(
                "{law}: instance {index} {:?} decodes as {:?}",
                record(given),
                record(decoded)
            ));
        }
    }
    Ok(())
}

fn structure_kept(bytes: &[u8], d1: &SettingsBytecode) -> Result<(), String> {
    let structure = SettingsBytecode::decode_structure(bytes)
        .map_err(|error| format!("law 1: the structure decode failed: {error:#}"))?;
    let bare = SettingsBytecode {
        version: d1.version,
        instances: d1
            .instances
            .iter()
            .map(|instance| {
                SettingsBytecodeInstance::new(
                    instance.settings_id.clone(),
                    instance.name.clone(),
                    instance.class_name.clone(),
                    instance.parent_index,
                )
            })
            .collect(),
    };
    if structure != bare {
        let at = structure
            .instances
            .iter()
            .zip(&bare.instances)
            .position(|(left, right)| left != right);
        return Err(format!(
            "law 1: the structure decode differs at instance {at:?} of {}",
            bare.instances.len()
        ));
    }
    Ok(())
}

/// The string-set decode gives d1 when the store holds every string of one
/// of the sets, and nothing when it holds none of them whole.
fn string_sets_kept(bytes: &[u8], d1: &SettingsBytecode) -> Result<(), String> {
    let absent = vec![ABSENT.to_string()];
    let held = d1.instances.first().map_or_else(Vec::new, |instance| {
        vec![instance.name.clone(), instance.class_name.clone()]
    });
    let decode = |sets: &[Vec<String>]| {
        SettingsBytecode::decode_if_contains_any_string_set(bytes, sets)
            .map_err(|error| format!("law 1: the string-set decode failed: {error:#}"))
    };
    if decode(std::slice::from_ref(&absent))?.is_some() {
        return Err(
            "law 1: the string-set decode gave a store for a string it does not hold".into(),
        );
    }
    match decode(&[absent, held])? {
        Some(found) if same_document(&found, d1) => Ok(()),
        Some(_) => Err("law 1: the string-set decode gave another document".into()),
        None => Err("law 1: the string-set decode missed a set the store holds".into()),
    }
}

/// The first value of `decoded` that is not what `doc` states for it.
fn values_stated(
    doc: &Doc,
    dense: bool,
    stated: &SettingsBytecode,
    decoded: &SettingsBytecode,
    law: &str,
) -> Result<(), String> {
    records_kept(stated, decoded, law)?;
    for (index, (stated, decoded)) in stated.instances.iter().zip(&decoded.instances).enumerate() {
        for (attribute, stated, decoded) in [
            (false, &stated.properties, &decoded.properties),
            (true, &stated.attributes, &decoded.attributes),
        ] {
            let keys = stated.keys().chain(decoded.keys());
            for key in keys {
                let (expected, got) = (stated.get(key), decoded.get(key));
                if expected
                    .zip(got)
                    .is_some_and(|(expected, got)| same(expected, got))
                {
                    continue;
                }
                let kind = if attribute { "attribute" } else { "property" };
                let node = &doc.nodes[index];
                let given = doc
                    .entry(index, attribute, key)
                    .map_or_else(|| "nothing".to_string(), |entry| shown(&entry.value.input));
                let encoder = if dense { "dense " } else { "" };
                return Err(format!(
                    "{law}: instance {index} ({:?}, class {:?}) {kind} {key:?}: generated {given}, stated {}, decoded from the {encoder}encoding {}",
                    node.id,
                    node.class,
                    expected.map_or_else(|| "nothing".to_string(), shown),
                    got.map_or_else(|| "nothing".to_string(), shown),
                ));
            }
        }
    }
    Ok(())
}

fn reencoded(bytes: &[u8], d1: &SettingsBytecode) -> Result<(), String> {
    let again = encode_settings_bytecode(d1)
        .map_err(|error| format!("law 3: the decoded document does not encode: {error:#}"))?;
    if again != bytes {
        return Err(format!(
            "law 3: the decoded document encodes to other bytes ({} against {})",
            again.len(),
            bytes.len()
        ));
    }
    let d2 = decode_settings_bytecode(&again)
        .map_err(|error| format!("law 3: the second encoding does not decode: {error:#}"))?;
    if !same_document(&d2, d1) {
        return Err("law 3: the second round trip changed the document".into());
    }
    // The document a pull or a push decodes holds its payloads by handle:
    // it is the same document, and it encodes to the same bytes.
    let held = crate::settings::bytecode::decode_settings_bytecode_holding(bytes)
        .map_err(|error| format!("law 3: the encoding does not decode by handle: {error:#}"))?;
    if !same_document(&held, d1) || held != *d1 {
        return Err("law 3: the document that holds its payloads by handle is another".into());
    }
    if encode_settings_bytecode(&held).ok().as_deref() != Some(bytes) {
        return Err("law 3: a document that holds its payloads by handle encodes otherwise".into());
    }
    Ok(())
}

/// Law 4, with `plain` the document law 2 states and `d1` the decode of
/// `bytes`, the plain encoding of `d0`.
fn dense_kept(
    doc: &Doc,
    d0: &SettingsBytecode,
    d1: &SettingsBytecode,
    bytes: &[u8],
    plain: &SettingsBytecode,
) -> Result<(), String> {
    match (
        doc.stated(true),
        encode_settings_bytecode_with_dense_references(d0),
    ) {
        (None, Ok(_)) => {
            return Err(
                "law 4: the dense encoder took a reference with an instanceIndex beside pathSegments"
                    .into(),
            );
        }
        (None, Err(_)) => {}
        (Some(_), Err(error)) => {
            return Err(format!(
                "law 4: the dense encoder refused the document: {error:#}"
            ));
        }
        (Some(stated), Ok(dense_bytes)) if dense_bytes == bytes => {
            values_stated(doc, true, &stated, d1, "law 4")?;
        }
        (Some(stated), Ok(dense_bytes)) => {
            let decoded = decode_settings_bytecode(&dense_bytes)
                .map_err(|error| format!("law 4: the dense encoding does not decode: {error:#}"))?;
            values_stated(doc, true, &stated, &decoded, "law 4")?;
            if same_document(&stated, plain) {
                return Err(
                    "law 4: with no reference the two encoders resolve differently, they wrote other bytes"
                        .into(),
                );
            }
        }
    }
    let again = encode_settings_bytecode_with_dense_references(d1).map_err(|error| {
        format!("law 4: the dense encoder refused a decoded document: {error:#}")
    })?;
    if again != bytes {
        return Err(
            "law 4: a decoded document encodes to other bytes with dense references".into(),
        );
    }
    Ok(())
}

/// Law 2 for a document with a property no store can give back.
fn refused(d0: &SettingsBytecode) -> Result<(), String> {
    if encode_settings_bytecode(d0).is_ok() {
        return Err("law 2: the encoder took a property named for a record field or an object named Attributes".into());
    }
    if encode_settings_bytecode_with_dense_references(d0).is_ok() {
        return Err("law 4: the dense encoder took a property named for a record field or an object named Attributes".into());
    }
    Ok(())
}

fn laws(doc: &Doc) -> Result<(), String> {
    let d0 = doc.input();
    if doc.refused() {
        return refused(&d0);
    }
    let bytes = encode_settings_bytecode(&d0)
        .map_err(|error| format!("law 2: the encoder refused the document: {error:#}"))?;
    let d1 = decode_settings_bytecode(&bytes)
        .map_err(|error| format!("law 1: its encoding does not decode: {error:#}"))?;
    records_kept(&d0, &d1, "law 1")?;
    structure_kept(&bytes, &d1)?;
    string_sets_kept(&bytes, &d1)?;
    let plain = doc
        .stated(false)
        .expect("the plain encoder refuses nothing");
    values_stated(doc, false, &plain, &d1, "law 2")?;
    reencoded(&bytes, &d1)?;
    super::bytecode::property_edit::sim::check_codec_document(&bytes)
        .map_err(|error| format!("law 5: selective property edit differs: {error:#}"))?;
    dense_kept(doc, &d0, &d1, &bytes, &plain)
}

fn breaks(doc: &Doc, law: &str) -> bool {
    laws(doc).is_err_and(|failure| failure.starts_with(law))
}

/// The document with as few values as still break `law`: whole instances'
/// values first, in halving ranges, then single values, within
/// `SHRINK_INSTANCES`.
fn shrunk(doc: &Doc, law: &str) -> Doc {
    let mut trials = SHRINK_INSTANCES / doc.nodes.len().max(1);
    let mut doc = doc.clone();
    let count = doc.entries.len();
    let mut width = count.next_power_of_two();
    while width > 0 {
        for start in (0..count).step_by(width) {
            let end = (start + width).min(count);
            if trials == 0 || doc.entries[start..end].iter().all(Vec::is_empty) {
                continue;
            }
            trials -= 1;
            let mut trial = doc.clone();
            trial.entries[start..end].iter_mut().for_each(Vec::clear);
            if breaks(&trial, law) {
                doc = trial;
            }
        }
        width /= 2;
    }
    shrunk_values(doc, law, trials)
}

fn shrunk_values(mut doc: Doc, law: &str, mut trials: usize) -> Doc {
    for index in 0..doc.entries.len() {
        let mut position = 0;
        while position < doc.entries[index].len() && trials > 0 {
            trials -= 1;
            let mut trial = doc.clone();
            trial.entries[index].remove(position);
            if breaks(&trial, law) {
                doc = trial;
            } else {
                position += 1;
            }
        }
    }
    doc
}

fn described(doc: &Doc) -> String {
    let mut lines = vec![format!(
        "{} instances, version {}",
        doc.nodes.len(),
        doc.version
    )];
    for (index, entries) in doc.entries.iter().enumerate() {
        let node = &doc.nodes[index];
        for entry in entries {
            let kind = if entry.attribute {
                "attribute"
            } else {
                "property"
            };
            lines.push(format!(
                "  instance {index} ({:?}, class {:?}, parent {:?}) {kind} {:?} = {}",
                node.id,
                node.class,
                node.parent,
                entry.name,
                shown(&entry.value.input)
            ));
        }
    }
    if lines.len() == 1 {
        lines.extend(doc.nodes.iter().take(8).enumerate().map(|(index, node)| {
            format!(
                "  instance {index}: {:?} {:?} {:?} parent {:?}",
                node.id, node.name, node.class, node.parent
            )
        }));
    }
    lines.join("\n")
}

fn run_seed(seed: u64, log: &mut Vec<String>) -> Result<(), String> {
    let doc = generate(seed);
    log.push(format!(
        "{} instances, {} values",
        doc.nodes.len(),
        doc.entries.iter().map(Vec::len).sum::<usize>()
    ));
    laws(&doc).map_err(|failure| {
        let law = failure.split(':').next().unwrap_or_default().to_string();
        let smallest = shrunk(&doc, &law);
        let failure = laws(&smallest).err().unwrap_or(failure);
        format!(
            "{failure}\nsmallest document that breaks it: {}",
            described(&smallest)
        )
    })
}

#[test]
fn store_documents_keep_the_codec_laws() {
    run_seeds(DEFAULT_SEEDS, run_seed);
}

fn parallel_columns(index: usize) -> Map<String, Value> {
    let mut values = Map::from_iter([
        ("Signed".into(), json!(i64::MIN + index as i64)),
        ("Unsigned".into(), json!(u64::MAX - index as u64)),
        ("Single".into(), json!(index as f64 / 8.0)),
        ("Double".into(), json!(index as f64 + 0.1)),
        (
            "Enum".into(),
            json!({"_type":"EnumItem", "name":if index.is_multiple_of(2) { "Plastic" } else { "FutureValue" }}),
        ),
        ("NumericSingles".into(), json!([index, -0.0, 0.125])),
        ("NumericDoubles".into(), json!([index, 0.1, 1e39])),
        (
            "Object".into(),
            json!({"_type":"FutureType", "value":index, "items":[false,null,"text"]}),
        ),
        (
            "Outside".into(),
            json!({"_type":"Ref", "instanceIndex":index+1, "pathSegments":["OtherService", "Name"], "pathOrdinals":[1,1]}),
        ),
    ]);
    for (type_name, fields) in FIXED {
        let mut object: Map<String, Value> = fields
            .iter()
            .map(|field| ((*field).into(), json!(index as f64 + 0.1)))
            .collect();
        object.insert("_type".into(), json!(type_name));
        values.insert(type_name.into(), Value::Object(object));
    }
    if !index.is_multiple_of(3) {
        for (name, value) in [
            ("SparseFalse", json!(false)),
            ("SparseTrue", json!(true)),
            ("SparseInt", json!(index as i64 - 5)),
            ("SparseSingle", json!(index as f64 / 8.0)),
            ("SparseDouble", json!(index as f64 + 0.1)),
            ("SparseEnum", json!({"_type":"EnumItem", "name":"Plastic"})),
        ] {
            values.insert(name.into(), value);
        }
    }
    values
}

fn parallel_document(seed: u64) -> SettingsBytecode {
    let mut rng = SimRng(seed);
    let count = [0, 1, 2047, 2048, 4095, 4096, 4097, 8193][seed as usize % 8];
    let mut ids = (0..count)
        .map(|index| format!("debug:{index:x}"))
        .collect::<Vec<_>>();
    rng.shuffle(&mut ids);
    let instances =
        ids.into_iter()
            .enumerate()
            .map(|(index, id)| {
                let mut instance = SettingsBytecodeInstance::new(
                    id,
                    format!("duplicate-{}", rng.below(4)),
                    if index == 0 {
                        "Camera"
                    } else {
                        CLASSES[index % CLASSES.len()]
                    }
                    .into(),
                    (index + 1 < count).then_some(count - 1),
                );
                instance
                    .properties
                    .insert("UniqueId".into(), json!("shared"));
                instance.properties.extend(parallel_columns(index));
                instance
                    .properties
                    .insert("Label".into(), json!(format!("text-{index}")));
                instance.properties.insert(
                    "Link".into(),
                    json!({"_type":"Ref", "instanceIndex":1+rng.below(count)}),
                );
                instance.properties.insert("Nested".into(), json!([
            {"Ref":{"instanceIndex":1+rng.below(count)}},
            if seed.is_multiple_of(2) { json!({"_type":"EnumItem", "name":"Plastic"}) }
            else { json!({"_type":"EnumItem", "enumType":"Material", "name":"Plastic"}) },
        ]));
                instance.properties.insert(
                    "Mixed".into(),
                    if index % 2 == 0 {
                        json!(true)
                    } else {
                        json!(index)
                    },
                );
                instance.properties.insert(
                    "Vector".into(),
                    json!({"_type":"Vector3", "x":index, "y":-0.0, "z":0.125}),
                );
                let mut components = [0_f64; 12];
                for (at, component) in components.iter_mut().enumerate() {
                    *component = if at < 3 {
                        index as f64
                    } else if seed.is_multiple_of(2) {
                        ROTATIONS[index % ROTATIONS.len()][at - 3]
                    } else {
                        f64::from(rng.below(1_000_000) as f32 / 65536.0)
                    };
                }
                instance.properties.insert(
                    "CFrame".into(),
                    json!({"_type":"CFrame", "components":components}),
                );
                instance.attributes.insert(
                    "Tagged".into(),
                    json!({"_type":"Vector3", "x":1, "y":2, "z":3}),
                );
                if index + 1 == count {
                    instance.name = "Workspace".into();
                    instance.class_name = "Workspace".into();
                    instance.properties.insert(
                        "CurrentCamera".into(),
                        json!({"_type":"Ref", "instanceIndex":1}),
                    );
                }
                instance
            })
            .collect();
    SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    }
}

#[test]
fn parallel_codec_keeps_serial_bytes_and_documents() {
    let pools = [1, 4, 64].map(|workers| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build()
            .unwrap()
    });
    run_seeds(16, |seed, _| {
        let document = parallel_document(seed);
        let bytes = pools[0]
            .install(|| encode_settings_bytecode(&document))
            .map_err(|error| error.to_string())?;
        let expected = pools[0]
            .install(|| decode_settings_bytecode(&bytes))
            .map_err(|error| error.to_string())?;
        let mut state =
            super::bytecode::parallel_codec_sim::captured_state(&expected, seed % 3 != 1);
        state.dense_index_topology = seed % 3 != 0;
        let service_bytes = pools[0]
            .install(|| super::bytecode::encode_service_settings_binary(&state))
            .map_err(|error| error.to_string())?;
        for pool in &pools[1..] {
            pool.install(|| -> Result<(), String> {
                let encoded =
                    encode_settings_bytecode(&document).map_err(|error| error.to_string())?;
                if bytes != encoded {
                    return Err("parallel encoder changed bytes".into());
                }
                if service_bytes
                    != super::bytecode::encode_service_settings_binary(&state)
                        .map_err(|error| error.to_string())?
                {
                    return Err("parallel service encoder changed bytes".into());
                }
                let decoded =
                    decode_settings_bytecode(&bytes).map_err(|error| error.to_string())?;
                if !same_document(&decoded, &expected) {
                    return Err("parallel decoder changed values".into());
                }
                let mut legacy = bytes.clone();
                legacy[7] = SETTINGS_BINARY_VERSION - 1;
                let mut expected_legacy = expected.clone();
                expected_legacy.version -= 1;
                let decoded_legacy =
                    decode_settings_bytecode(&legacy).map_err(|error| error.to_string())?;
                if !same_document(&decoded_legacy, &expected_legacy) {
                    return Err("parallel legacy decode changed values".into());
                }
                Ok(())
            })?;
        }
        Ok(())
    });
}
