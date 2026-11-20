mod error;
mod header;
mod state;

use std::{
    collections::{HashMap, HashSet},
    io::Read,
    str,
    sync::Arc,
};

use rbx_dom_weak::{
    types::{Ref, Variant},
    Ustr, WeakDom,
};
use rbx_reflection::ReflectionDatabase;

use self::state::DeserializerState;

#[cfg(any(test, feature = "unstable_text_format"))]
pub(crate) use self::header::FileHeader;

pub use self::error::Error;

#[allow(missing_docs)]
pub struct FlatInstance {
    pub referent: Ref,
    pub parent_index: Option<usize>,
    pub name: String,
    pub class: Ustr,
    pub properties: Vec<(Ustr, Variant)>,
}

#[allow(missing_docs)]
pub struct FlatDom {
    pub metadata: HashMap<String, String>,
    pub root_indices: Vec<usize>,
    pub instances: Vec<FlatInstance>,
}

/// A sparse document and the exact property names elided from each instance.
/// Each elided value equals its reflection default, including floating bits.
#[allow(missing_docs)]
pub struct SparseDom {
    pub dom: WeakDom,
    pub elided_defaults: HashMap<Ref, Vec<Ustr>>,
}

/// A configurable deserializer for Roblox binary models and places.
///
/// ## Example
/// ```no_run
/// use std::fs::File;
/// use std::io::BufReader;
///
/// use rbx_binary::Deserializer;
///
/// let input = BufReader::new(File::open("File.rbxm")?);
///
/// let deserializer = Deserializer::new();
/// let dom = deserializer.deserialize(input)?;
///
/// // rbx_binary always returns a DOM with a DataModel at the top level.
/// // To get to the instances from our file, we need to go one level deeper.
///
/// println!("Root instances in file:");
/// for &referent in dom.root().children() {
///     let instance = dom.get_by_ref(referent).unwrap();
///     println!("- {}", instance.name);
/// }
///
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// ## Configuration
///
/// A custom [`ReflectionDatabase`][ReflectionDatabase] can be specified via
/// [`reflection_database`][reflection_database].
///
/// [ReflectionDatabase]: rbx_reflection::ReflectionDatabase
/// [reflection_database]: Deserializer#method.reflection_database
#[derive(Clone)]
pub struct Deserializer<'db> {
    database: &'db ReflectionDatabase<'db>,
    elide_defaults: bool,
    record_elided_defaults: bool,
    strict_float_types: bool,
    retain_defaults_for_classes: HashSet<String>,
    retain_defaults_for_properties: HashSet<String>,
    flat_property_filter: Option<Arc<HashMap<String, HashSet<String>>>>,
    keep_below: Option<(HashSet<String>, HashSet<String>)>,
}

impl<'db> Deserializer<'db> {
    /// Create a new `Deserializer` with the default settings.
    pub fn new() -> Self {
        Self {
            database: rbx_reflection_database::get().unwrap(),
            elide_defaults: false,
            record_elided_defaults: false,
            strict_float_types: false,
            retain_defaults_for_classes: HashSet::new(),
            retain_defaults_for_properties: HashSet::new(),
            flat_property_filter: None,
            keep_below: None,
        }
    }

    /// Sets what reflection database for the deserializer to use.
    #[inline]
    pub fn reflection_database(mut self, database: &'db ReflectionDatabase<'db>) -> Self {
        self.database = database;
        self
    }

    #[inline]
    #[allow(missing_docs)]
    pub fn elide_defaults(mut self, enabled: bool) -> Self {
        self.elide_defaults = enabled;
        self
    }

    /// Reject floating-point wire widths that disagree with known property
    /// types. The default reader recovers legacy Float64 values for Float32
    /// properties, which is useful for repair but unsafe as upload validation.
    /// This checks the serialized data before that recovery can hide a mismatch;
    /// it does not establish that Roblox's engine can load the entire file.
    pub fn strict_float_types(mut self, enabled: bool) -> Self {
        self.strict_float_types = enabled;
        self
    }

    /// Keep explicitly serialized values for these classes, even when they
    /// equal reflection defaults. This preserves evidence of field presence
    /// without expanding defaults for the rest of a large snapshot.
    pub fn retain_defaults_for_classes(mut self, classes: HashSet<String>) -> Self {
        self.retain_defaults_for_classes = classes;
        self
    }

    /// Keep explicitly serialized values for these canonical property names,
    /// even when they equal reflection defaults.
    pub fn retain_defaults_for_properties(mut self, properties: HashSet<String>) -> Self {
        self.retain_defaults_for_properties = properties;
        self
    }

    #[inline]
    #[allow(missing_docs)]
    pub fn flat_property_filter(mut self, filter: Arc<HashMap<String, HashSet<String>>>) -> Self {
        self.flat_property_filter = Some(filter);
        self
    }

    /// Give every instance below the first top-level instance of each of
    /// these classes only these canonical properties and its name.
    /// `deserialize` then reads the values once the file has placed its
    /// instances, so a caller that drops those instances never holds what
    /// they hold.
    pub fn keep_below(mut self, classes: HashSet<String>, properties: HashSet<String>) -> Self {
        self.keep_below = Some((classes, properties));
        self
    }

    /// Deserialize a Roblox binary model or place from the given stream using
    /// this deserializer.
    pub fn deserialize<R: Read>(&self, reader: R) -> Result<WeakDom, Error> {
        Ok(self.read_state(reader)?.finish())
    }

    /// Elide only bit-exact reflection defaults and record their original field
    /// presence. Restore these fields, or seed their class columns, before writing.
    pub fn deserialize_sparse<R: Read>(&self, reader: R) -> Result<SparseDom, Error> {
        let mut options = self.clone();
        options.elide_defaults = true;
        options.record_elided_defaults = true;
        // WeakDom enforces uniqueness while inserting, including for nil IDs.
        options
            .retain_defaults_for_properties
            .insert("UniqueId".into());
        // A default canonical value must still block a later legacy alias.
        for class in options.database.classes.values() {
            for property in class.properties.values() {
                if let rbx_reflection::PropertyKind::Canonical {
                    serialization: rbx_reflection::PropertySerialization::Migrate(migration),
                } = &property.kind
                {
                    options
                        .retain_defaults_for_properties
                        .insert(property.name.to_string());
                    options.retain_defaults_for_properties.extend(
                        migration
                            .new_property_names()
                            .iter()
                            .map(|name| name.to_string()),
                    );
                }
            }
        }
        Ok(options.read_state(reader)?.finish_sparse())
    }

    fn read_state<R: Read>(&self, reader: R) -> Result<DeserializerState<'_, R>, Error> {
        profiling::scope!("rbx_binary::deserialize");
        let mut deserializer = DeserializerState::new(self, reader, false)?;
        // With `keep_below`, the values wait for the chunk that places the
        // instances.
        let mut placed = self.keep_below.is_none();
        let mut waiting = Vec::new();

        loop {
            let chunk = deserializer.next_chunk()?;

            match &chunk.name {
                b"META" => deserializer.decode_meta_chunk(&chunk.data)?,
                b"SSTR" => deserializer.decode_sstr_chunk(&chunk.data)?,
                b"INST" => deserializer.decode_inst_chunk(&chunk.data)?,
                b"PROP" if !placed => {
                    deserializer.check_prop_chunk_type(&chunk.data)?;
                    waiting.push(chunk);
                }
                b"PROP" => deserializer.decode_prop_chunk(&chunk.data)?,
                b"PRNT" | b"END\0" => {
                    if &chunk.name == b"PRNT" {
                        deserializer.decode_prnt_chunk(&chunk.data)?;
                        deserializer.mark_kept_below();
                        placed = true;
                    }
                    for held in waiting.drain(..) {
                        deserializer.decode_prop_chunk(&held.data)?;
                    }
                    if &chunk.name == b"END\0" {
                        deserializer.decode_end_chunk(&chunk.data)?;
                        break;
                    }
                }
                _ => match str::from_utf8(&chunk.name) {
                    Ok(name) => log::info!("Unknown binary chunk name {name}"),
                    Err(_) => log::info!("Unknown binary chunk name {:?}", chunk.name),
                },
            }
        }

        Ok(deserializer)
    }

    #[allow(missing_docs)]
    pub fn deserialize_flat<R: Read>(&self, reader: R) -> Result<FlatDom, Error> {
        profiling::scope!("rbx_binary::deserialize_flat");

        let mut deserializer = DeserializerState::new(self, reader, true)?;
        let mut prop_chunks = Vec::new();

        loop {
            let chunk = deserializer.next_chunk()?;

            if &chunk.name == b"PROP" {
                prop_chunks.push(chunk);
                continue;
            }

            if !prop_chunks.is_empty() {
                deserializer.decode_prop_chunks_parallel(core::mem::take(&mut prop_chunks))?;
            }

            match &chunk.name {
                b"META" => deserializer.decode_meta_chunk(&chunk.data)?,
                b"SSTR" => deserializer.decode_sstr_chunk(&chunk.data)?,
                b"INST" => deserializer.decode_inst_chunk(&chunk.data)?,
                b"PRNT" => deserializer.decode_prnt_chunk(&chunk.data)?,
                b"END\0" => {
                    deserializer.decode_end_chunk(&chunk.data)?;
                    break;
                }
                _ => match str::from_utf8(&chunk.name) {
                    Ok(name) => log::info!("Unknown binary chunk name {name}"),
                    Err(_) => log::info!("Unknown binary chunk name {:?}", chunk.name),
                },
            }
        }

        Ok(deserializer.finish_flat())
    }
}

impl Default for Deserializer<'_> {
    fn default() -> Self {
        Self::new()
    }
}
