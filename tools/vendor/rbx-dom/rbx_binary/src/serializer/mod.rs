mod error;
mod state;

use std::{collections::HashMap, io::Write};

use rbx_dom_weak::{
    types::{Ref, Variant},
    Ustr, WeakDom,
};
use rbx_reflection::ReflectionDatabase;

use self::state::SerializerState;

pub use self::error::Error;

/// How a native reader should reuse an existing instance for a serialized row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstanceBindingMode {
    /// Reuse its identity and apply the serialized properties and hierarchy.
    Replace,
    /// Apply serialized properties to the existing identity without reparenting
    /// it (for example, a service or an engine-owned container).
    Properties,
    /// Preserve its properties and parent, but allow references and children
    /// in the payload to target its existing identity (for example, a viewport).
    ReferenceOnly,
}

/// Exact factory position in the emitted INST chunks, including split classes.
#[derive(Clone, Debug)]
pub struct SerializedInstanceBinding {
    /// The source DOM identity.
    pub referent: Ref,
    /// The signed identity used inside the binary payload.
    pub binary_referent: i32,
    /// The native class factory to bind.
    pub class_name: String,
    /// Zero-based factory call position across all chunks for this class.
    pub ordinal: u32,
    /// Total factory calls for this class across the payload.
    pub class_count: u32,
}

/// A configurable serializer for Roblox binary models and places.
///
/// ## Example
/// ```no_run
/// use std::fs::File;
/// use std::io::BufWriter;
///
/// use rbx_binary::Serializer;
/// use rbx_dom_weak::{InstanceBuilder, WeakDom};
///
/// let dom = WeakDom::new(InstanceBuilder::new("Folder"));
///
/// let output = BufWriter::new(File::create("PlainFolder.rbxm")?);
/// let serializer = Serializer::new();
/// serializer.serialize(output, &dom, &[dom.root_ref()])?;
///
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// ## Configuration
///
/// A custom [`ReflectionDatabase`][ReflectionDatabase] can be specified via
/// [`reflection_database`][reflection_database].
///
/// By default, the Serializer uses LZ4 compression, mimicking Roblox. This can
/// be changed via [`compression_type`][compression_type].
///
/// [ReflectionDatabase]: rbx_reflection::ReflectionDatabase
/// [reflection_database]: Serializer#method.reflection_database
/// [compression_type]: Serializer#method.compression_type
//
// future settings:
// * recursive: bool = true
#[non_exhaustive]
pub struct Serializer<'db> {
    database: &'db ReflectionDatabase<'db>,
    compression: CompressionType,
}

impl<'db> Serializer<'db> {
    /// Create a new `Serializer` with the default settings.
    pub fn new() -> Self {
        Serializer {
            database: rbx_reflection_database::get().unwrap(),
            compression: CompressionType::default(),
        }
    }

    /// Sets what reflection database for the serializer to use.
    #[inline]
    pub fn reflection_database(self, database: &'db ReflectionDatabase<'db>) -> Self {
        Self { database, ..self }
    }

    /// Sets what type of compression the serializer will use for compression.
    #[inline]
    pub fn compression_type(self, compression: CompressionType) -> Self {
        Self {
            compression,
            ..self
        }
    }

    /// Serialize a Roblox binary model or place into the given stream using
    /// this serializer.
    pub fn serialize<W: Write>(&self, writer: W, dom: &WeakDom, refs: &[Ref]) -> Result<(), Error> {
        self.serialize_inner(writer, dom, refs, None, None)
            .map(|_| ())
    }

    /// Serialize an insertion payload and return exact native factory bindings.
    ///
    /// Reference-only rows omit PROP and PRNT entries; property bindings omit
    /// only PRNT. Neither omits children or incoming references. This is not a
    /// standalone place export: the consumer
    /// must bind every requested row to an existing object before reading PROP.
    pub fn serialize_with_bindings<W: Write>(
        &self,
        writer: W,
        dom: &WeakDom,
        refs: &[Ref],
        bindings: &HashMap<Ref, InstanceBindingMode>,
    ) -> Result<Vec<SerializedInstanceBinding>, Error> {
        self.serialize_inner(writer, dom, refs, Some(bindings), None)
    }

    /// Serialize exactly these instances, in depth-first postorder, without
    /// walking their descendants. Include parent/reference anchors explicitly;
    /// repeated anchors must be reference-only bindings in subsequent streams.
    /// The shared property schema preserves default columns across batches.
    pub fn serialize_selection_with_bindings<W: Write>(
        &self,
        writer: W,
        dom: &WeakDom,
        postorder: &[Ref],
        bindings: &HashMap<Ref, InstanceBindingMode>,
        schema: &HashMap<Ustr, HashMap<Ustr, &Variant>>,
    ) -> Result<Vec<SerializedInstanceBinding>, Error> {
        self.serialize_inner(writer, dom, postorder, Some(bindings), Some(schema))
    }

    fn serialize_inner<'dom, W: Write>(
        &self,
        writer: W,
        dom: &'dom WeakDom,
        refs: &[Ref],
        bindings: Option<&'dom HashMap<Ref, InstanceBindingMode>>,
        schema: Option<&HashMap<Ustr, HashMap<Ustr, &Variant>>>,
    ) -> Result<Vec<SerializedInstanceBinding>, Error> {
        profiling::scope!("rbx_binary::seserialize");

        let mut serializer = SerializerState::new(self, dom, writer, bindings);

        if let Some(schema) = schema {
            serializer.add_selection(refs, schema)?;
        } else {
            serializer.add_instances(refs)?;
        }
        serializer.generate_referents();
        let bindings = serializer.instance_bindings()?;
        serializer.write_header()?;
        serializer.serialize_metadata()?;
        serializer.serialize_shared_strings()?;
        serializer.serialize_instances()?;
        serializer.serialize_properties()?;
        serializer.serialize_parents()?;
        serializer.serialize_end()?;

        Ok(bindings)
    }
}

impl Default for Serializer<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Indicates the types of compression that files can be written with.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
pub enum CompressionType {
    /// LZ4 compression. This is what Roblox uses by default.
    #[default]
    Lz4,
    /// No compression.
    None,
    /// ZSTD compression.
    Zstd,
}
