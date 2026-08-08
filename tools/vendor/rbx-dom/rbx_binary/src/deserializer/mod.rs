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
pub struct Deserializer<'db> {
    database: &'db ReflectionDatabase<'db>,
    elide_defaults: bool,
    flat_property_filter: Option<Arc<HashMap<String, HashSet<String>>>>,
}

impl<'db> Deserializer<'db> {
    /// Create a new `Deserializer` with the default settings.
    pub fn new() -> Self {
        Self {
            database: rbx_reflection_database::get().unwrap(),
            elide_defaults: false,
            flat_property_filter: None,
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

    #[inline]
    #[allow(missing_docs)]
    pub fn flat_property_filter(mut self, filter: Arc<HashMap<String, HashSet<String>>>) -> Self {
        self.flat_property_filter = Some(filter);
        self
    }

    /// Deserialize a Roblox binary model or place from the given stream using
    /// this deserializer.
    pub fn deserialize<R: Read>(&self, reader: R) -> Result<WeakDom, Error> {
        profiling::scope!("rbx_binary::deserialize");

        let mut deserializer = DeserializerState::new(self, reader, false)?;

        loop {
            let chunk = deserializer.next_chunk()?;

            match &chunk.name {
                b"META" => deserializer.decode_meta_chunk(&chunk.data)?,
                b"SSTR" => deserializer.decode_sstr_chunk(&chunk.data)?,
                b"INST" => deserializer.decode_inst_chunk(&chunk.data)?,
                b"PROP" => deserializer.decode_prop_chunk(&chunk.data)?,
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

        Ok(deserializer.finish())
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
