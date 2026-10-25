//! Mach-O and reflection contracts for the native macOS playtest path.
//! Addresses are derived from the selected executable and rechecked in-process.
use super::super::super::arm64_playtest;
use super::*;
use serde_json::{Value, json};
use std::collections::BTreeSet;

const TELEPORT: &str = "N3RBX10Reflection18BoundYieldFuncDescINS_15TeleportServiceEFNSt3__110shared_ptrINS_8InstanceEEExNS4_IKNS3_6vectorIS6_NS3_9allocatorIS6_EEEEEES6_ELb0ELi3EEE";
const INVOKE: &str = "N3RBX10Reflection18BoundYieldFuncDescINS_16BindableFunctionEFNSt3__110shared_ptrIKNS0_5TupleEEES7_ELb0ELi1EEE";
const EXECUTE: &str = "N3RBX10Reflection18BoundYieldFuncDescINS_6Studio17StudioTestServiceEFNS0_7VariantES4_ELb0ELi1EEE";
const ARGS: &str =
    "N3RBX10Reflection13BoundFuncDescINS_6Studio17StudioTestServiceEFNS0_7VariantEvELb0ELi0EEE";
const SOLO_PRODUCER: &str = "DataModelLoader18loadPlaySoloServer";

#[path = "macos_teleport_dispatch.rs"]
mod dispatch;
pub(crate) use dispatch::NativeMacTeleportDispatch;
#[path = "macos_play_join.rs"]
mod join;
pub(crate) use join::NativeMacPlayJoin;
#[path = "macos_teleport_gui.rs"]
mod gui;
pub(crate) use gui::NativeMacTeleportGui;
#[path = "macos_play_caption.rs"]
mod caption;
pub(crate) use caption::NativeMacPlayCaption;
#[path = "macos_play_routing.rs"]
mod routing;
pub(crate) use routing::{
    NativeMacTestClient, NativeMacTestJoin, TestClientRoute, prepare_test_client,
    process_arguments, start_destination_server, test_client_configuration,
};

fn host_identity() -> Result<(u64, u64)> {
    // Use the same kernel certificate as the helper, including microseconds.
    // The human-readable process identity used by the CLI is not this ABI.
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let actual = unsafe {
        libc::proc_pidinfo(
            std::process::id() as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size as i32,
        )
    };
    anyhow::ensure!(
        actual == size as i32,
        "Cannot identify the native teleport host"
    );
    let info = unsafe { info.assume_init() };
    Ok((info.pbi_start_tvsec, info.pbi_start_tvusec))
}

struct Catalog {
    executable: PathBuf,
    uuid: [u8; 16],
    bytes: OnceLock<Vec<u8>>,
}

type TypeKey = ([u8; 16], String);
type SingletonKey = ([u8; 16], String, Option<String>);
static TABLES: OnceLock<Mutex<HashMap<TypeKey, u64>>> = OnceLock::new();
static SINGLETONS: OnceLock<Mutex<HashMap<SingletonKey, u64>>> = OnceLock::new();

struct LoaderBinding {
    slot: u64,
    entry: u64,
    fields: arm64_playtest::Fields,
    place: u64,
    universe: u64,
}

impl Catalog {
    fn open(memory: &Memory) -> Result<Self> {
        Ok(Self {
            executable: memory.executable.clone(),
            uuid: memory.trace.image_uuid,
            bytes: OnceLock::new(),
        })
    }

    fn image(&self) -> Result<MachImage<'_>> {
        if self.bytes.get().is_none() {
            let bytes = fs::read(&self.executable)?;
            anyhow::ensure!(
                MachImage::parse(&bytes)?.image_uuid == self.uuid,
                "Studio executable changed during teleport discovery"
            );
            let _ = self.bytes.set(bytes);
        }
        MachImage::parse(self.bytes.get().unwrap())
    }

    fn pointers(image: &MachImage<'_>, target: u64) -> Vec<u64> {
        let mut result = Vec::new();
        for section in &image.sections {
            let bytes = &image.bytes[section.offset..section.offset + section.size as usize];
            for at in memchr::memmem::find_iter(bytes, &target.to_le_bytes()) {
                let address = section.address + at as u64;
                if address.is_multiple_of(8) {
                    result.push(address);
                }
            }
        }
        result
    }

    fn address_bytes<'a>(
        image: &'a MachImage<'_>,
        address: u64,
        length: usize,
    ) -> Result<&'a [u8]> {
        let section = image
            .sections
            .iter()
            .find(|section| {
                address >= section.address
                    && address
                        .checked_add(length as u64)
                        .is_some_and(|end| end <= section.address + section.size)
            })
            .context("Teleport metadata is outside a file-backed Studio section")?;
        let at = section.offset + (address - section.address) as usize;
        Ok(&image.bytes[at..at + length])
    }

    fn table(&self, kind: &str) -> Result<u64> {
        let key = (self.uuid, kind.to_owned());
        let cache = TABLES.get_or_init(Default::default);
        if let Some(table) = cache.lock_recover().get(&key).copied() {
            return Ok(table);
        }
        let image = self.image()?;
        let needle = format!("{kind}\0");
        let mut matches = BTreeSet::new();
        for section in &image.sections {
            let bytes = &image.bytes[section.offset..section.offset + section.size as usize];
            for at in memchr::memmem::find_iter(bytes, needle.as_bytes()) {
                if at != 0 && bytes[at - 1] != 0 {
                    continue;
                }
                let name = section.address + at as u64;
                for pointer in Self::pointers(&image, name) {
                    // Itanium type_info stores its name after its own vptr.
                    for reference in Self::pointers(&image, pointer - 8) {
                        if Self::address_bytes(&image, reference - 8, 8)
                            .ok()
                            .and_then(|bytes| read_u64(bytes, 0))
                            == Some(0)
                            && Self::address_bytes(&image, reference + 8, 8)
                                .ok()
                                .and_then(|bytes| read_u64(bytes, 0))
                                .is_some_and(|method| {
                                    image.function_starts.binary_search(&method).is_ok()
                                })
                        {
                            matches.insert(reference + 8 - image.image_base);
                        }
                    }
                }
            }
        }
        anyhow::ensure!(
            matches.len() == 1,
            "Native macOS type {kind} has {} complete vtables",
            matches.len()
        );
        let table = *matches.first().unwrap();
        cache.lock_recover().insert(key, table);
        Ok(table)
    }

    fn callback_type(&self, owner: &str) -> Result<String> {
        let image = self.image()?;
        let mut matches = BTreeSet::new();
        for section in &image.sections {
            let bytes = &image.bytes[section.offset..section.offset + section.size as usize];
            for at in memchr::memmem::find_iter(bytes, owner.as_bytes()) {
                let start = bytes[..at]
                    .iter()
                    .rposition(|byte| *byte == 0)
                    .map_or(0, |at| at + 1);
                let end = bytes[at..]
                    .iter()
                    .position(|byte| *byte == 0)
                    .map(|end| at + end);
                if let Some(end) = end.filter(|end| *end - start <= 4096)
                    && let Ok(kind) = std::str::from_utf8(&bytes[start..end])
                    && kind.starts_with("NSt3__110__function6__funcI")
                    && kind.ends_with("EFvPNS2_9DataModelEEEE")
                {
                    matches.insert(kind.to_owned());
                }
            }
        }
        anyhow::ensure!(
            matches.len() == 1,
            "Native macOS {owner} has {} model callbacks",
            matches.len()
        );
        Ok(matches.pop_first().unwrap())
    }

    fn singleton(&self, memory: &Memory, kind: &str, name: Option<&str>) -> Result<u64> {
        let key = (self.uuid, kind.to_owned(), name.map(str::to_owned));
        let cache = SINGLETONS.get_or_init(Default::default);
        let table = memory.base + self.table(kind)?;
        // Reflection singletons live in the executable's static data. Cache
        // their RVA, never a process address, and revalidate the type and name
        // in every receiver before using its binding.
        let cached = cache.lock_recover().get(&key).copied();
        if let Some(rva) = cached {
            let object = memory.base + rva;
            if memory.pointer(object).ok() == Some(table)
                && name.is_none_or(|name| {
                    memory
                        .pointer(object + 8)
                        .and_then(|at| memory.name(at))
                        .ok()
                        .as_deref()
                        == Some(name)
                })
            {
                return Ok(object);
            }
            cache.lock_recover().remove(&key);
        }
        let image = self.image()?;
        let mut matches = Vec::new();
        let mut remaining = 256 * 1024 * 1024_u64;
        for (address, size) in &image.writable_segments {
            remaining = remaining
                .checked_sub(*size)
                .context("Studio writable data exceeds the teleport discovery budget")?;
            for offset in (0..*size).step_by(1024 * 1024) {
                let begin = memory.base + address - image.image_base + offset;
                let bytes = memory.read(begin, (size - offset).min(1024 * 1024) as usize)?;
                for at in memchr::memmem::find_iter(&bytes, &table.to_le_bytes()) {
                    if !at.is_multiple_of(8) {
                        continue;
                    }
                    let object = begin + at as u64;
                    if name.is_none_or(|name| {
                        memory
                            .pointer(object + 8)
                            .and_then(|at| memory.name(at))
                            .ok()
                            .as_deref()
                            == Some(name)
                    }) {
                        matches.push(object);
                    }
                }
            }
        }
        anyhow::ensure!(
            matches.len() == 1,
            "Native macOS type {kind} ({name:?}) has {} singleton candidates",
            matches.len()
        );
        cache.lock_recover().insert(key, matches[0] - memory.base);
        Ok(matches[0])
    }

    fn member(&self, memory: &Memory, kind: &str, name: &str) -> Result<(u64, u64)> {
        let descriptor = self.singleton(memory, kind, Some(name))?;
        let (offset, function) =
            memory.function_binding(descriptor, memory.pointer(descriptor)?)?;
        Ok((descriptor + offset as u64, function))
    }

    fn identity(&self, memory: &Memory, context: &[u8], name: &str) -> Result<(usize, u64)> {
        let model = read_u64(context, 8).context("Missing source model")?;
        let class = read_u64(context, 40).context("Missing class layout")?;
        let descriptor = memory.member(model, class, name)?;
        anyhow::ensure!(
            memory.rtti(descriptor)? == "N3RBX10Reflection14PropDescriptorINS_9DataModelExEE",
            "{name} is not the reflected 64-bit DataModel identity"
        );
        let mut found = BTreeSet::new();
        for offset in memory.bindings(descriptor)? {
            let binding = memory.pointer(descriptor + offset as u64)?;
            anyhow::ensure!(
                memory.rtti(binding)?
                    == "N3RBX10Reflection14PropDescriptorINS_9DataModelExE7GetImplIMNS_13DataModelPropEKFxvEEE",
                "{name} has an incompatible identity getter"
            );
            for method in memory.vtable_methods(memory.pointer(binding)?)? {
                if let Some((field, adjustment)) =
                    super::super::super::arm64_functions::adjusted_dispatch_field(
                        method,
                        |address| memory.dispatch_code(address).ok(),
                        |field| memory.pointer(binding + field as u64).ok(),
                    )
                {
                    let function = memory.pointer(binding + field as u64)?;
                    if let Some(offset) =
                        arm64_playtest::integer_field(&memory.dispatch_code(function)?)
                    {
                        let offset = usize::try_from(adjustment + offset as i64)
                            .context("Negative DataModel identity offset")?;
                        anyhow::ensure!(
                            offset < 65536,
                            "DataModel identity exceeds its object bound"
                        );
                        found.insert(offset);
                    }
                }
            }
        }
        anyhow::ensure!(
            found.len() == 1,
            "{name} resolved {} identity fields",
            found.len()
        );
        let offset = *found.first().unwrap();
        let value = memory.pointer(model + offset as u64)?;
        anyhow::ensure!(
            value > 0 && value <= 9007199254740991,
            "{name} is not a published identity"
        );
        Ok((offset, value))
    }

    fn loader(&self, memory: &Memory, context: &[u8]) -> Result<LoaderBinding> {
        let place = self.identity(memory, context, "PlaceId")?;
        let universe = self.identity(memory, context, "GameId")?;
        let instance = read_u64(context, 8)
            .context("Missing source model")?
            .checked_sub(read_u64(context, 56).context("Missing model owner")?)
            .context("Invalid DataModel instance offset")? as usize;
        let kind = self.callback_type(SOLO_PRODUCER)?;
        let table = memory.base + self.table(&kind)?;
        let mut matches = Vec::new();
        for (index, entry) in memory.vtable_methods(table)?.into_iter().enumerate() {
            if let Some(fields) = arm64_playtest::producer_fields(
                entry,
                [instance + place.0, instance + universe.0],
                instance,
                |address| memory.dispatch_code(address).ok(),
            ) {
                matches.push(LoaderBinding {
                    slot: table + index as u64 * 8,
                    entry,
                    fields,
                    place: place.1,
                    universe: universe.1,
                });
            }
        }
        anyhow::ensure!(
            matches.len() == 1,
            "Solo serialization resolved {} callbacks",
            matches.len()
        );
        Ok(matches.pop().unwrap())
    }

    fn source_context(
        &self,
        memory: &Memory,
        contexts: &[u8],
        source: Option<(i64, i64)>,
    ) -> Result<Vec<u8>> {
        anyhow::ensure!(
            !contexts.is_empty() && contexts.len().is_multiple_of(64),
            "Studio has no complete model contexts for teleport preparation"
        );
        let mut matches = Vec::new();
        for context in contexts.chunks_exact(64) {
            let Ok((_, place)) = self.identity(memory, context, "PlaceId") else {
                continue;
            };
            let Ok((_, universe)) = self.identity(memory, context, "GameId") else {
                continue;
            };
            if source.is_none_or(|expected| expected == (place as i64, universe as i64)) {
                matches.push(context.to_vec());
            }
        }
        anyhow::ensure!(
            matches.len() == 1,
            "Native teleport source identity resolves {} models; retain the Edit owner before starting Play",
            matches.len()
        );
        Ok(matches.pop().unwrap())
    }
}

/// One source-owned alternate input, revoked on drop, timeout, Stop, or host exit.
pub(crate) struct NativeMacPlayLoad {
    memory: Memory,
    request: Vec<u8>,
}

impl NativeMacPlayLoad {
    pub(crate) fn arm(
        pid: u32,
        title: &str,
        source: (i64, i64),
        destination: (i64, i64),
        bytes: &[u8],
        nonce: &str,
        root: Option<&NativeMacTeleportDispatch>,
    ) -> Result<Self> {
        anyhow::ensure!(
            source.0 > 0 && source.1 > 0 && destination.0 > 0 && destination.1 == source.1,
            "Native Play input requires a readable destination in the source universe"
        );
        anyhow::ensure!(
            !nonce.is_empty()
                && nonce.len() < 128
                && nonce
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "Invalid owned Play launch nonce"
        );
        anyhow::ensure!(
            (14..=128 * 1024 * 1024).contains(&bytes.len()) && bytes.starts_with(b"<roblox"),
            "Native Play input is not bounded saved place content"
        );
        let mut memory = Memory::for_process(pid, Duration::from_secs(30))?;
        let contexts = memory.request(5, &[0], title)?;
        memory.base = read_u64(&contexts, 0).context("Missing Studio image base")?;
        let catalog = Catalog::open(&memory)?;
        let context = if let Some(root) = root {
            root.root_context(&memory, &contexts)?
        } else {
            catalog.source_context(&memory, &contexts, Some(source))?
        };
        let binding = catalog.loader(&memory, &context)?;
        anyhow::ensure!(
            binding.place == source.0 as u64 && binding.universe == source.1 as u64,
            "The source Edit identity changed before destination preparation"
        );
        let descriptor = memory.member(
            read_u64(&context, 8).unwrap(),
            read_u64(&context, 40).unwrap(),
            "GetAttribute",
        )?;
        let kind = memory.rtti(descriptor)?;
        anyhow::ensure!(
            kind == "N3RBX10Reflection13BoundFuncDescINS_8InstanceEKFNS0_7VariantENS_10StringViewEELb0ELi1EEE",
            "The native launch attribute getter changed ABI: {kind}"
        );
        let (_, attribute) = memory.function_binding(descriptor, memory.pointer(descriptor)?)?;
        let string_table = memory.base + catalog.table("N3RBX10Reflection5TTypeINSt3__112basic_stringIcNS2_11char_traitsIcEENS2_9allocatorIcEEEELNS0_15TTypeStringModeE0EEE")?;
        let host = std::process::id();
        let (seconds, micros) = host_identity()?;
        let mut token = [0; 8];
        getrandom::fill(&mut token)
            .map_err(|e| anyhow::anyhow!("Cannot create Play loader lease: {e}"))?;
        let mut request = vec![0; 296];
        put32(&mut request, 0, 1);
        put32(&mut request, 4, 296);
        put32(&mut request, 144, host);
        put32(&mut request, 148, 30000);
        for (offset, value) in [
            (16, u64::from_le_bytes(token).max(1)),
            (24, binding.slot),
            (32, binding.entry),
            (40, read_u64(&context, 56).unwrap()),
            (48, read_u64(&context, 8).unwrap()),
            (56, read_u64(&context, 16).unwrap()),
            (64, binding.place),
            (72, binding.universe),
            (80, destination.0 as u64),
            (88, destination.1 as u64),
            (96, binding.fields.capture as u64),
            (104, binding.fields.string as u64),
            (112, binding.fields.place as u64),
            (120, binding.fields.universe as u64),
            (128, attribute),
            (136, string_table),
            (152, seconds),
            (160, micros),
        ] {
            request[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        request[168..168 + nonce.len()].copy_from_slice(nonce.as_bytes());
        let mut lease = Self { memory, request };
        let mut payload = lease.request.clone();
        put32(&mut payload, 8, 1);
        payload.extend_from_slice(bytes);
        lease.memory.deadline = Instant::now() + Duration::from_secs(10);
        lease.request = lease.memory.request(7, &payload, "")?;
        anyhow::ensure!(
            lease.request.len() == 296,
            "Invalid Mac Play loader receipt"
        );
        Ok(lease)
    }

    fn invoke(&mut self, action: u32) -> Result<u32> {
        put32(&mut self.request, 8, action);
        self.memory.deadline = Instant::now() + Duration::from_secs(3);
        self.request = self.memory.request(7, &self.request, "")?;
        anyhow::ensure!(self.request.len() == 296, "Invalid Mac Play loader receipt");
        read_u32(&self.request, 12).context("Missing Play loader phase")
    }

    pub(crate) fn consumed(&mut self) -> Result<bool> {
        let phase = self.invoke(3)?;
        anyhow::ensure!(
            !matches!(phase, 4 | 5),
            "Mac Play loading was cancelled before consumption"
        );
        Ok(phase == 3)
    }
}

impl Drop for NativeMacPlayLoad {
    fn drop(&mut self) {
        let _ = self.invoke(2);
    }
}

pub(crate) fn discover(pid: u32, title: &str) -> Result<Value> {
    let mut memory = Memory::for_process(pid, Duration::from_secs(30))?;
    let contexts = memory.request(5, &[0], title)?;
    anyhow::ensure!(
        !contexts.is_empty() && contexts.len().is_multiple_of(64),
        "Studio has no live model for teleport qualification"
    );
    memory.base = read_u64(&contexts, 0).context("Missing Studio image base")?;
    let catalog = Catalog::open(&memory)?;
    let outcome = |result: Result<Value>| match result {
        Ok(value) => json!({"status":"observed","value":value}),
        Err(error) => json!({"status":"unresolved","reason":format!("{error:#}")}),
    };
    let member =
        |kind, name| {
            outcome(catalog.member(&memory, kind, name).map(
                |(slot, entry)| json!({"slotRva":slot-memory.base,"entryRva":entry-memory.base}),
            ))
        };
    let args = member(ARGS, "GetTestArgs");
    let join = outcome((|| {
        let mut result = Vec::new();
        for (kind, name) in [
            (
                "N3RBX10Reflection19CustomBoundFuncDescINS_6PlayerEFNSt3__110shared_ptrIKNS0_10ValueTableEEEvELb0ELi0EEE",
                "GetJoinData",
            ),
            (
                "N3RBX10Reflection13BoundFuncDescINS_15TeleportServiceEFNS0_7VariantEvELb0ELi0EEE",
                "GetLocalPlayerTeleportData",
            ),
        ] {
            let (slot, entry) = catalog.member(&memory, kind, name)?;
            result.push(
                json!({"name":name,"slotRva":slot-memory.base,"entryRva":entry-memory.base,
                "code":base64::encode(memory.dispatch_code(entry)?)}),
            );
        }
        Ok(json!(result))
    })());
    let producer = outcome((|| {
        let kind = catalog.callback_type(SOLO_PRODUCER)?;
        let table = catalog.table(&kind)?;
        let methods = memory.vtable_methods(memory.base + table)?;
        let mut code = Vec::new();
        for entry in methods {
            let bytes = memory.dispatch_code(entry)?;
            code.push(json!({"entryRva":entry-memory.base,"code":base64::encode(bytes)}));
        }
        Ok(json!({"type":kind,"tableRva":table,"methods":code}))
    })());
    let loader = outcome((|| {
        let context = catalog.source_context(&memory, &contexts, None)?;
        let binding = catalog.loader(&memory, &context)?;
        Ok(
            json!({"slotRva":binding.slot-memory.base,"entryRva":binding.entry-memory.base,
            "fields":binding.fields,"placeId":binding.place,"gameId":binding.universe}),
        )
    })());
    Ok(
        json!({"ok":true,"schemaVersion":1,"platform":"macos-arm64","pid":pid,"imageUuid":memory.trace.image_uuid,
        "nativeSwitchReady":false,"readOnly":true,
        "teleport":member(TELEPORT,"TeleportAsync"),"invoke":member(INVOKE,"Invoke"),
        "execute":member(EXECUTE,"ExecutePlayModeAsync"),"args":args,"join":join,"soloProducer":producer,"loader":loader,
        "serverLoader":outcome(catalog.server_loader().map(|binding| json!(binding)))}),
    )
}

pub(crate) fn probe_load(pid: u32, title: &str) -> Result<Value> {
    let mut memory = Memory::for_process(pid, Duration::from_secs(15))?;
    let contexts = memory.request(5, &[0], title)?;
    memory.base = read_u64(&contexts, 0).context("Missing Studio image base")?;
    let catalog = Catalog::open(&memory)?;
    // Studio can retain an unpublished staging model beside the owned Edit and
    // Play models. This read-only probe qualifies the image's loader contract;
    // arm() separately checks the exact retained Edit owner before mutation.
    let binding = contexts
        .chunks_exact(64)
        .find_map(|context| catalog.loader(&memory, context).ok())
        .context("No published DataModel qualified the native Play loader")?;
    Ok(
        json!({"slotRva":binding.slot-memory.base,"entryRva":binding.entry-memory.base,
        "fields":binding.fields,"readOnly":true}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_native_teleport_contracts() {
        for name in [
            "teleport_dispatch",
            "playtest",
            "play_join",
            "teleport_gui",
            "test_client",
        ] {
            assert!(
                std::process::Command::new(
                    std::path::Path::new(env!("OUT_DIR")).join(format!("renium-{name}-test"))
                )
                .status()
                .expect("run native Mac teleport regression")
                .success()
            );
        }
    }

    #[test]
    #[ignore = "requires an explicitly owned Studio process"]
    fn installed_mac_playtest_bindings() -> Result<()> {
        let pid = std::env::var("RENIUM_TELEPORT_TEST_PID")?.parse()?;
        let title = std::env::var("RENIUM_TELEPORT_TEST_TITLE").unwrap_or_default();
        println!("{}", discover(pid, &title)?);
        Ok(())
    }

    #[test]
    #[ignore = "requires an explicitly owned stopped Studio"]
    fn mac_native_dispatch_owned_process() -> Result<()> {
        let pid = std::env::var("RENIUM_TELEPORT_TEST_PID")?.parse()?;
        let nonce = std::env::var("RENIUM_TELEPORT_TEST_NONCE")?;
        let mut lease = NativeMacTeleportDispatch::before_play(pid, "", &nonce)?;
        println!("TELEPORT_DISPATCH_ARMED pid={pid}");
        lease.renew()?;
        anyhow::ensure!(
            !lease.accepted(1, 1)?,
            "An idle launch already accepted a request"
        );
        drop(lease);
        println!("TELEPORT_DISPATCH_RESTORED pid={pid}");
        Ok(())
    }

    #[test]
    #[ignore = "requires an explicitly owned stopped Studio and saved destination"]
    fn mac_native_loader_owned_process() -> Result<()> {
        let pid = std::env::var("RENIUM_PLAY_LOADER_TEST_PID")?.parse()?;
        let source = std::env::var("RENIUM_PLAY_LOADER_TEST_SOURCE")?.parse()?;
        let destination = std::env::var("RENIUM_PLAY_LOADER_TEST_DESTINATION")?.parse()?;
        let universe = std::env::var("RENIUM_PLAY_LOADER_TEST_UNIVERSE")?.parse()?;
        let bytes = fs::read(std::env::var("RENIUM_PLAY_LOADER_TEST_FILE")?)?;
        let nonce = std::env::var("RENIUM_PLAY_LOADER_TEST_NONCE")?;
        let title = std::env::var("RENIUM_TELEPORT_TEST_TITLE").unwrap_or_default();
        let mut lease = NativeMacPlayLoad::arm(
            pid,
            &title,
            (source, universe),
            (destination, universe),
            &bytes,
            &nonce,
            None,
        )?;
        println!("PLAY_LOADER_ARMED pid={pid}");
        let deadline = Instant::now() + Duration::from_secs(25);
        while !lease.consumed()? {
            anyhow::ensure!(
                Instant::now() < deadline,
                "Owned Play did not start before its loader expired"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        println!("PLAY_LOADER_CONSUMED pid={pid}");
        Ok(())
    }
}
