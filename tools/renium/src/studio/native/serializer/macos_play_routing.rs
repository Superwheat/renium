//! Local-test routing binds the installed engine's configuration and loader.
use super::super::super::super::arm64_routing;
use super::*;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

impl Catalog {
    fn function_bytes<'a>(image: &'a MachImage<'a>, entry: u64) -> Result<&'a [u8]> {
        let position = image
            .function_starts
            .binary_search(&entry)
            .map_err(|_| anyhow::anyhow!("Routing entry is not a function"))?;
        let end = *image
            .function_starts
            .get(position + 1)
            .context("Routing function has no end")?;
        anyhow::ensure!(end - entry <= 65536, "Routing function exceeds its bound");
        Self::address_bytes(image, entry, (end - entry) as usize)
    }

    fn client_binding(&self) -> Result<ClientBinding> {
        let image = self.image()?;
        let table = self.table("N3RBX6Studio11PlaceOpenerE")?;
        let table_address = image.image_base + table;
        let typeinfo = read_u64(Self::address_bytes(&image, table_address - 8, 8)?, 0).unwrap();
        let mut bases = Vec::new();
        for reference in Self::pointers(&image, typeinfo) {
            if let Ok(bytes) = Self::address_bytes(&image, reference - 8, 8) {
                let top = read_u64(bytes, 0).unwrap() as i64;
                if (-65536..0).contains(&top) {
                    bases.push((-top as u64, reference + 8 - image.image_base));
                }
            }
        }
        anyhow::ensure!(
            !bases.is_empty(),
            "PlaceOpener has no component ownership bases"
        );
        let anchors = [
            ("Invalid Place Config, expected a Lua table", false),
            ("PlaceOpener::scheduleReplacePlace", true),
            ("PlaceOpener/startClient", true),
            ("PlaceOpener/openPlace", true),
        ];
        let references = self.functions_referencing_all(&anchors.map(|(name, _)| name))?;
        let mut functions = Vec::new();
        for ((anchor, member), candidates) in anchors.into_iter().zip(references) {
            let candidates = candidates
                .into_iter()
                .filter(|entry| {
                    !member
                        || Self::pointers(&image, image.image_base + entry)
                            .iter()
                            .any(|slot| {
                                *slot >= table_address
                                    && Self::address_bytes(
                                        &image,
                                        table_address,
                                        (*slot + 8 - table_address) as usize,
                                    )
                                    .ok()
                                    .is_some_and(|bytes| {
                                        bytes.chunks_exact(8).all(|v| {
                                            image
                                                .function_starts
                                                .binary_search(&read_u64(v, 0).unwrap())
                                                .is_ok()
                                        })
                                    })
                            })
                })
                .collect::<Vec<_>>();
            anyhow::ensure!(
                candidates.len() == 1,
                "{anchor} resolves {} functions",
                candidates.len()
            );
            functions.push(candidates[0]);
        }
        let [parse, schedule, start, open] = functions.try_into().unwrap();
        let mut continuations = BTreeSet::new();
        let mut calls = Vec::new();
        for pair in image.function_starts.windows(2) {
            let Ok(code) = Self::function_bytes(&image, pair[0]) else {
                continue;
            };
            for (i, bytes) in code.chunks_exact(4).enumerate() {
                let word = read_u32(bytes, 0).unwrap();
                let pc = pair[0] + i as u64 * 4;
                if word & 0xfc000000 == 0x94000000
                    && arm64_routing::branch(pc, word) == Some(image.image_base + open)
                {
                    calls.push(pc - image.image_base);
                    continuations.insert(pair[0]);
                }
            }
        }
        anyhow::ensure!(
            continuations.len() == 1 && !calls.is_empty() && calls.len() <= 8,
            "Client continuation is ambiguous"
        );
        let continuation = *continuations.first().unwrap();
        let destroy = arm64_routing::configuration_destructor(
            continuation,
            Self::function_bytes(&image, continuation)?,
        )
        .context("Client configuration destructor is ambiguous")?
            - image.image_base;
        let imports = self
            .imports("__ZN7QWidget5closeEv")?
            .into_iter()
            .filter(|(kind, _)| *kind == 6 || *kind == 7)
            .map(|(_, at)| at)
            .collect::<Vec<_>>();
        anyhow::ensure!(
            !imports.is_empty() && imports.len() <= 8,
            "Client window close imports are unavailable"
        );
        let mut code = Vec::new();
        for at in [
            parse,
            schedule,
            start,
            open,
            destroy,
            continuation - image.image_base,
        ] {
            code.push((
                at,
                Self::function_bytes(&image, image.image_base + at)?.to_vec(),
            ));
        }
        Ok(ClientBinding {
            table,
            bases,
            parse,
            schedule,
            start,
            open,
            destroy,
            close: imports,
            calls,
            code,
        })
    }
    fn strings(&self, wanted: &str) -> Result<BTreeSet<u64>> {
        let image = self.image()?;
        let needle = format!("{wanted}\0");
        Ok(image
            .sections
            .iter()
            .flat_map(|section| {
                let bytes = &image.bytes[section.offset..section.offset + section.size as usize];
                memchr::memmem::find_iter(bytes, needle.as_bytes())
                    .filter(|at| *at == 0 || bytes[*at - 1] == 0)
                    .map(|at| section.address + at as u64)
                    .collect::<Vec<_>>()
            })
            .collect())
    }

    fn functions_referencing(&self, wanted: &str) -> Result<Vec<u64>> {
        Ok(self.functions_referencing_all(&[wanted])?.remove(0))
    }

    fn functions_referencing_all(&self, wanted: &[&str]) -> Result<Vec<Vec<u64>>> {
        let image = self.image()?;
        let mut by_address = BTreeMap::<u64, Vec<usize>>::new();
        for (index, wanted) in wanted.iter().enumerate() {
            let addresses = self.strings(wanted)?;
            anyhow::ensure!(!addresses.is_empty(), "Studio omitted {wanted}");
            for address in addresses {
                by_address.entry(address).or_default().push(index);
            }
        }
        let addresses = by_address.keys().copied().collect();
        let mut result = vec![Vec::new(); wanted.len()];
        for pair in image.function_starts.windows(2) {
            let length = (pair[1] - pair[0]) as usize;
            if length <= 65536
                && let Ok(bytes) = Self::address_bytes(&image, pair[0], length)
            {
                for address in arm64_routing::references(pair[0], bytes, &addresses) {
                    for &index in &by_address[&address] {
                        let entry = pair[0] - image.image_base;
                        if result[index].last() != Some(&entry) {
                            result[index].push(entry);
                        }
                    }
                }
            }
        }
        Ok(result)
    }

    /// Imported symbol identity, not the current import-table position.
    fn imports(&self, wanted: &str) -> Result<Vec<(u64, u64)>> {
        let image = self.image()?;
        let bytes = image.bytes;
        let mut cursor = 32;
        let (mut symbols, mut strings, mut indirect) = (None, None, None);
        let mut sections = Vec::new();
        for _ in 0..read_u32(bytes, 16).context("Missing Mach commands")? {
            let command = read_u32(bytes, cursor).context("Missing Mach command")?;
            let size = read_u32(bytes, cursor + 4).context("Missing command size")? as usize;
            anyhow::ensure!(
                size >= 8 && cursor + size <= bytes.len(),
                "Invalid Mach command"
            );
            if command == 2 {
                symbols = read_u32(bytes, cursor + 8).map(|v| v as usize);
                strings = read_u32(bytes, cursor + 16).map(|v| v as usize);
            } else if command == 11 {
                indirect = read_u32(bytes, cursor + 56).map(|v| v as usize);
            } else if command == 0x19 {
                for i in 0..read_u32(bytes, cursor + 64).unwrap_or(0) as usize {
                    let at = cursor + 72 + i * 80;
                    anyhow::ensure!(at + 80 <= cursor + size, "Invalid Mach import section");
                    let kind = read_u32(bytes, at + 64).unwrap() & 255;
                    if [6, 7, 8].contains(&kind) {
                        sections.push((
                            read_u64(bytes, at + 32).unwrap(),
                            read_u64(bytes, at + 40).unwrap(),
                            kind,
                            read_u32(bytes, at + 68).unwrap(),
                            if kind == 8 {
                                read_u32(bytes, at + 72).unwrap()
                            } else {
                                8
                            },
                        ));
                    }
                }
            }
            cursor += size;
        }
        let symbols = symbols.context("Missing Mach symbols")?;
        let strings = strings.context("Missing Mach strings")?;
        let indirect = indirect.context("Missing Mach indirect symbols")?;
        let mut found = Vec::new();
        for (address, size, kind, first, stride) in sections {
            anyhow::ensure!(
                stride > 0 && size % stride as u64 == 0,
                "Invalid Mach stub stride"
            );
            for i in 0..size / stride as u64 {
                let symbol = read_u32(bytes, indirect + (first as usize + i as usize) * 4)
                    .context("Invalid indirect symbol")?;
                if symbol & 0xc0000000 != 0 {
                    continue;
                }
                let string = read_u32(bytes, symbols + symbol as usize * 16)
                    .context("Invalid import symbol")? as usize;
                let text = bytes
                    .get(strings + string..)
                    .context("Invalid import string")?;
                if text.starts_with(wanted.as_bytes()) && text.get(wanted.len()) == Some(&0) {
                    found.push((kind as u64, address + i * stride as u64 - image.image_base));
                }
            }
        }
        Ok(found)
    }

    fn identity_writers(&self) -> Result<Vec<(u64, u64)>> {
        type Writers = HashMap<[u8; 16], Vec<(u64, u64)>>;
        static CACHE: OnceLock<Mutex<Writers>> = OnceLock::new();
        let cache = CACHE.get_or_init(Default::default);
        if let Some(writers) = cache.lock_recover().get(&self.uuid).cloned() {
            return Ok(writers);
        }
        let image = self.image()?;
        let candidates = self.functions_referencing("GenerateTeleportJoin")?;
        let keys = [
            self.strings("UserId")?,
            self.strings("UserName")?,
            self.strings("DisplayName")?,
        ];
        let found = candidates
            .into_iter()
            .filter_map(|entry| {
                arm64_routing::writers(
                    image.image_base + entry,
                    Catalog::function_bytes(&image, image.image_base + entry).ok()?,
                    &keys,
                )
            })
            .collect::<Vec<_>>();
        anyhow::ensure!(
            found.len() == 1,
            "Local test identity has {} writers",
            found.len()
        );

        let writers = found[0]
            .iter()
            .map(|(call, writer)| (call - image.image_base, writer - image.image_base))
            .collect::<Vec<_>>();
        cache.lock_recover().insert(self.uuid, writers.clone());
        Ok(writers)
    }

    pub(super) fn server_loader(&self) -> Result<[u64; 4]> {
        let image = self.image()?;
        let kind = self.callback_type("getLoadServerDataModelPayload")?;
        let table = self.table(&kind)?;
        let imports = self.imports("__ZN7QString13toUtf8_helperERKS_")?;
        let utf8 = imports
            .iter()
            .filter(|(kind, _)| *kind == 8)
            .map(|(_, at)| *at)
            .collect::<Vec<_>>();
        anyhow::ensure!(utf8.len() == 1, "QString UTF-8 conversion is not unique");
        let mut found = Vec::new();
        for index in 0..128 {
            let slot = table + index * 8;
            let entry =
                read_u64(Self::address_bytes(&image, image.image_base + slot, 8)?, 0).unwrap();
            let Ok(position) = image.function_starts.binary_search(&entry) else {
                break;
            };
            let end = *image
                .function_starts
                .get(position + 1)
                .context("Missing callback end")?;
            let bytes = Self::address_bytes(&image, entry, (end - entry) as usize)?;
            if let Some((capture, path)) =
                arm64_routing::server_input(entry, bytes, image.image_base + utf8[0])
            {
                found.push([slot, entry - image.image_base, capture, path]);
            }
        }
        anyhow::ensure!(
            found.len() == 1,
            "Destination server input resolves {} callbacks",
            found.len()
        );
        Ok(found[0])
    }
}

#[derive(Clone, serde::Serialize)]
struct ClientBinding {
    table: u64,
    bases: Vec<(u64, u64)>,
    parse: u64,
    schedule: u64,
    start: u64,
    open: u64,
    destroy: u64,
    close: Vec<u64>,
    calls: Vec<u64>,
    #[serde(skip)]
    code: Vec<(u64, Vec<u8>)>,
}

fn client_binding(memory: &Memory) -> Result<ClientBinding> {
    static CACHE: OnceLock<Mutex<HashMap<[u8; 16], ClientBinding>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let cached = cache.lock_recover().get(&memory.trace.image_uuid).cloned();
    let binding = if let Some(binding) = cached {
        binding
    } else {
        let binding = Catalog::open(memory)?.client_binding()?;
        cache
            .lock_recover()
            .insert(memory.trace.image_uuid, binding.clone());
        binding
    };
    for (at, code) in &binding.code {
        anyhow::ensure!(
            memory.code(memory.base + at, code.len())? == *code,
            "Client routing code changed"
        );
    }
    Ok(binding)
}

pub(crate) fn prepare_test_client(pid: u32) -> Result<()> {
    let mut memory = Memory::for_process(pid, Duration::from_secs(15))?;
    let contexts = memory.request(5, &[0], "")?;
    memory.base = read_u64(&contexts, 0).context("Client has no model")?;
    client_binding(&memory).map(|_| ())
}

pub(super) fn relay_binding(
    memory: &mut Memory,
    path: &[String],
    model_name: &str,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let contexts = memory.request(5, &[0], model_name)?;
    memory.base = read_u64(&contexts, 0).context("Routed model ended")?;
    let matches = contexts
        .chunks_exact(64)
        .filter_map(|context| {
            resolve_path(memory, context, path, &[])
                .ok()
                .map(|entries| (context, entries))
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        matches.len() == 1,
        "Route relay has {} model owners",
        matches.len()
    );
    let (context, entries) = &matches[0];
    let relay = entries.last().context("Route relay ended")?;
    let catalog = Catalog::open(memory)?;
    let (slot, attribute) = catalog.member(
        memory,
        "N3RBX10Reflection13BoundFuncDescINS_8InstanceEKFNS0_7VariantENS_10StringViewEELb0ELi1EEE",
        "GetAttribute",
    )?;
    let (_, invoke) = catalog.member(memory, INVOKE, "Invoke")?;
    let mut binding = vec![0; 112];
    let (seconds, micros) = host_identity()?;
    for (at, value) in [
        (0, read_u64(context, 8).unwrap()),
        (8, read_u64(context, 16).unwrap()),
        (16, relay.0),
        (24, relay.1),
        (32, parent_offset(memory, context)?),
        (40, read_u64(context, 24).unwrap()),
        (48, read_u64(context, 40).unwrap()),
        (56, read_u64(context, 48).unwrap()),
        (64, slot),
        (72, attribute),
        (80, invoke),
        (96, seconds),
        (104, micros),
    ] {
        put64(&mut binding, at, value);
    }
    put32(&mut binding, 88, std::process::id());
    Ok((binding, context.to_vec()))
}

pub(crate) struct NativeMacTestClient {
    memory: Memory,
    request: Vec<u8>,
}
impl NativeMacTestClient {
    pub(crate) fn arm(
        pid: u32,
        runtime: &str,
        path: &[String],
        model_name: &str,
        window: isize,
    ) -> Result<Self> {
        anyhow::ensure!(
            path == ["CoreGui", &format!("__ReniumTeleportRoute_{runtime}")],
            "Invalid routed client relay"
        );
        let mut memory = Memory::for_process(pid, Duration::from_secs(20))?;
        let (relay, _) = relay_binding(&mut memory, path, model_name)?;
        let binding = client_binding(&memory)?;
        // The caption lease already owns this exact window across every hop.
        // Querying AppKit again can block behind Studio's document work; the
        // native route validates the retained window when resolving its widget.
        let mut request = vec![0; 344];
        put32(&mut request, 0, 1);
        put32(&mut request, 4, 344);
        let mut token = [0; 8];
        getrandom::fill(&mut token)
            .map_err(|e| anyhow::anyhow!("Cannot identify client travel: {e}"))?;
        put64(&mut request, 16, u64::from_le_bytes(token).max(1));
        request[24..136].copy_from_slice(&relay);
        for (at, value) in [
            (136, binding.table),
            (152, binding.bases[0].1),
            (160, binding.parse),
            (168, binding.schedule),
            (176, binding.start),
            (184, binding.open),
            (192, binding.destroy),
        ] {
            put64(&mut request, at, memory.base + value);
        }
        put64(&mut request, 144, binding.bases[0].0);
        for (i, at) in binding.close.iter().enumerate() {
            put64(&mut request, 200 + i * 8, memory.base + at);
        }
        for (i, at) in binding.calls.iter().enumerate() {
            put64(&mut request, 264 + i * 8, memory.base + at);
        }
        put32(&mut request, 328, binding.calls.len() as u32);
        put32(&mut request, 332, u32::try_from(window)?);
        put32(&mut request, 336, binding.close.len() as u32);
        let mut lease = Self { memory, request };
        lease.invoke(1)?;
        Ok(lease)
    }
    fn invoke(&mut self, action: u32) -> Result<u32> {
        put32(&mut self.request, 8, action);
        self.memory.deadline = Instant::now() + Duration::from_secs(15);
        let response = self.memory.request(12, &self.request, "")?;
        anyhow::ensure!(
            response.len() == self.request.len(),
            "Invalid Mac client route receipt"
        );
        Ok(read_u32(&response, 12).unwrap())
    }
    pub(crate) fn complete(&mut self) -> Result<()> {
        anyhow::ensure!(
            self.invoke(3)? == 2,
            "Client route did not consume its configuration"
        );
        Ok(())
    }
}
impl Drop for NativeMacTestClient {
    fn drop(&mut self) {
        let _ = self.invoke(2);
    }
}

pub(crate) struct TestClientRoute {
    memory: Memory,
    request: Vec<u8>,
}
impl TestClientRoute {
    pub(crate) fn arm(server: u32, client: u32) -> Result<Self> {
        let memory = Memory::for_process(server, Duration::from_secs(5))?;
        let mut request = vec![0; 32];
        put32(&mut request, 0, 1);
        put32(&mut request, 4, 32);
        put32(&mut request, 24, client);
        let mut token = [0; 8];
        getrandom::fill(&mut token)
            .map_err(|e| anyhow::anyhow!("Cannot identify client launch: {e}"))?;
        put64(&mut request, 16, u64::from_le_bytes(token).max(1));
        let mut capture = Self { memory, request };
        capture.invoke(1)?;
        Ok(capture)
    }
    fn invoke(&mut self, action: u32) -> Result<Vec<u8>> {
        put32(&mut self.request, 8, action);
        self.memory.deadline = Instant::now() + Duration::from_secs(3);
        self.memory.request(14, &self.request, "")
    }
    pub(crate) fn command(&mut self) -> Result<Option<Vec<String>>> {
        let response = self.invoke(3)?;
        anyhow::ensure!(response.len() >= 32, "Truncated client launch receipt");
        if read_u32(&response, 12) != Some(2) {
            return Ok(None);
        }
        let mut arguments = Vec::new();
        let mut at = 32;
        while at < response.len() {
            let end = response[at..]
                .iter()
                .position(|b| *b == 0)
                .context("Unterminated launch argument")?
                + at;
            arguments.push(std::str::from_utf8(&response[at..end])?.to_owned());
            at = end + 1;
        }
        Ok(Some(arguments))
    }
}
impl Drop for TestClientRoute {
    fn drop(&mut self) {
        let _ = self.invoke(2);
    }
}

pub(crate) fn test_client_configuration(
    args: &[String],
    edit: u32,
    server: u32,
    universe: i64,
    place: i64,
) -> Result<Value> {
    let value = |key: &str| -> Result<&str> {
        args.windows(2)
            .find(|p| p[0].eq_ignore_ascii_case(key))
            .map(|p| p[1].as_str())
            .with_context(|| format!("Native client omitted {key}"))
    };
    anyhow::ensure!(
        value("-task")? == "StartClient"
            && value("-editpid")? == edit.to_string()
            && value("-parentpid")? == server.to_string()
            && value("-placeid")? == place.to_string()
            && value("-universeid")? == universe.to_string(),
        "Client no longer belongs to this test"
    );
    Ok(
        json!({"LaunchTask":"StartClient", "PlaceLaunchInfo":{"PlaceId":place,"UniverseId":universe},
        "PlayTestConfig":{"ServerPort":value("-port")?.parse::<u16>()?,
            "ServerName":value("-playtestsessionguid")?, "RbxTransportPort":value("-rbxtransportport")?.parse::<u16>()?,
            "RbxTransportToken":value("-rbxtransporttoken")?,
            "NumTestServerPlayersOnStartup":value("-numtestserverplayersuponstartup")?.parse::<u8>()?,
            "ExecuteTestService":true,"IsInStudioTestServiceMode":true,
            "PlayerEmulatorSerializedValues":value("-playeremulatorserializedvalues")?}}),
    )
}

/// Where a routed test player arrives and whether it joins as a client.
#[derive(Clone, Copy)]
pub(crate) struct TestJoinTarget {
    pub place: i64,
    pub universe: i64,
    pub client: bool,
}

pub(crate) struct NativeMacTestJoin {
    memory: Memory,
    request: Vec<u8>,
}
impl NativeMacTestJoin {
    pub(crate) fn arm(
        pid: u32,
        runtime: &str,
        path: &[String],
        target: TestJoinTarget,
        identity: &Value,
        model_name: &str,
    ) -> Result<Self> {
        let TestJoinTarget {
            place,
            universe,
            client,
        } = target;
        anyhow::ensure!(
            path == ["CoreGui", &format!("__ReniumRouteJoin_{runtime}")],
            "Invalid routed arrival relay"
        );
        let mut memory = Memory::for_process(pid, Duration::from_secs(20))?;
        let (relay, context) = relay_binding(&mut memory, path, model_name)?;
        let catalog = Catalog::open(&memory)?;
        let (place_field, _) = catalog.identity(&memory, &context, "PlaceId")?;
        let (universe_field, _) = catalog.identity(&memory, &context, "GameId")?;
        let (join_slot, join) = catalog.member(&memory,
            "N3RBX10Reflection19CustomBoundFuncDescINS_6PlayerEFNSt3__110shared_ptrIKNS0_10ValueTableEEEvELb0ELi0EEE", "GetJoinData")?;
        let (local_slot, local) = catalog.member(
            &memory,
            "N3RBX10Reflection13BoundFuncDescINS_15TeleportServiceEFNS0_7VariantEvELb0ELi0EEE",
            "GetLocalPlayerTeleportData",
        )?;
        let join_field = arm64_playtest::arrival_field(&memory.dispatch_code(join)?, false)
            .context("GetJoinData arrival field changed")?;
        let local_field = arm64_playtest::arrival_field(&memory.dispatch_code(local)?, true)
            .context("Local teleport data field changed")?;
        let table = catalog.singleton(&memory,
            "N3RBX10Reflection5TTypeINSt3__110shared_ptrIKNS0_10ValueTableEEELNS0_15TTypeStringModeE0EEE", None)?;
        let mut request = vec![0; 512];
        put32(&mut request, 0, 2);
        put32(&mut request, 4, 512);
        put32(&mut request, 280, u32::from(client));
        let mut token = [0; 8];
        getrandom::fill(&mut token)
            .map_err(|e| anyhow::anyhow!("Cannot identify routed arrival: {e}"))?;
        put64(&mut request, 16, u64::from_le_bytes(token).max(1));
        request[24..136].copy_from_slice(&relay);
        for (at, value) in [
            (136, u64::try_from(place)?),
            (144, u64::try_from(universe)?),
            (152, place_field as u64),
            (160, universe_field as u64),
            (168, join_slot),
            (176, join),
            (184, join_field as u64),
            (192, local_slot),
            (200, local),
            (208, local_field as u64),
            (216, table),
        ] {
            put64(&mut request, at, value);
        }
        if client {
            let writers = catalog.identity_writers()?;
            for (i, (call, writer)) in writers.iter().enumerate() {
                put64(&mut request, 224 + i * 8, memory.base + call);
                put64(&mut request, 248 + i * 8, memory.base + writer);
                memory.code(memory.base + call, 4)?;
                memory.dispatch_code(memory.base + writer)?;
                if i < 2 {
                    request[480 + i * 16..496 + i * 16]
                        .copy_from_slice(&memory.code(memory.base + writer, 16)?);
                }
            }
            let user = identity["userId"]
                .as_i64()
                .context("Missing routed test identity")?;
            anyhow::ensure!(user < 0, "Routed identity is not a local test player");
            put64(&mut request, 272, user as u64);
            for (key, at, capacity) in [("name", 288, 64), ("displayName", 352, 128)] {
                let value = identity[key]
                    .as_str()
                    .context("Missing routed player name")?;
                anyhow::ensure!(
                    !value.is_empty() && value.len() < capacity && !value.contains('\0'),
                    "Invalid routed player name"
                );
                request[at..at + value.len()].copy_from_slice(value.as_bytes());
            }
        }
        let mut lease = Self { memory, request };
        lease.invoke(1)?;
        Ok(lease)
    }
    fn invoke(&mut self, action: u32) -> Result<u32> {
        put32(&mut self.request, 8, action);
        self.memory.deadline = Instant::now() + Duration::from_secs(5);
        let response = self.memory.request(13, &self.request, "")?;
        anyhow::ensure!(
            response.len() == self.request.len(),
            "Truncated routed arrival receipt"
        );
        Ok(read_u32(&response, 12).unwrap())
    }
    pub(crate) fn complete(&mut self) -> Result<()> {
        anyhow::ensure!(
            self.invoke(3)? == 2,
            "Routed arrival did not consume its data"
        );
        Ok(())
    }
}
impl Drop for NativeMacTestJoin {
    fn drop(&mut self) {
        let _ = self.invoke(2);
    }
}

#[cfg(test)]
pub(super) fn discover_client(pid: u32) -> Result<Value> {
    // This diagnostic also runs without optimization; its full image scan is
    // intentionally outside the production handoff's deadline.
    let mut memory = Memory::for_process(pid, Duration::from_secs(120))?;
    let contexts = memory.request(5, &[0], "")?;
    memory.base = read_u64(&contexts, 0).context("Client has no model")?;
    Ok(serde_json::to_value(client_binding(&memory)?)?)
}

fn process_identity(pid: u32) -> Result<(u64, u64)> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let actual = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size as i32,
        )
    };
    anyhow::ensure!(actual == size as i32, "Owned test process ended");
    let info = unsafe { info.assume_init() };
    anyhow::ensure!(
        info.pbi_uid == unsafe { libc::getuid() },
        "Test belongs to another OS user"
    );
    Ok((info.pbi_start_tvsec, info.pbi_start_tvusec))
}

pub(crate) fn process_arguments(pid: u32) -> Result<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as i32];
    let mut length = 0;
    let mut query = |bytes: *mut libc::c_void, length: &mut usize| unsafe {
        libc::sysctl(mib.as_mut_ptr(), 3, bytes, length, std::ptr::null_mut(), 0)
    };
    anyhow::ensure!(
        query(std::ptr::null_mut(), &mut length) == 0 && length <= 4 * 1024 * 1024,
        "Cannot read test arguments"
    );
    let mut bytes = vec![0; length];
    anyhow::ensure!(
        query(bytes.as_mut_ptr().cast(), &mut length) == 0,
        "Test arguments ended"
    );
    bytes.truncate(length);
    let count = read_u32(&bytes, 0).context("Missing test argument count")? as usize;
    anyhow::ensure!(count > 0 && count <= 512, "Invalid test argument count");
    let mut at = 4 + bytes[4..]
        .iter()
        .position(|v| *v == 0)
        .context("Missing executable argument")?;
    while bytes.get(at) == Some(&0) {
        at += 1;
    }
    let mut args = Vec::new();
    for _ in 0..count {
        let length = bytes
            .get(at..)
            .context("Truncated test arguments")?
            .iter()
            .position(|v| *v == 0)
            .context("Unterminated test argument")?;
        args.push(String::from_utf8(bytes[at..at + length].to_vec())?);
        at += length + 1;
    }
    Ok(args)
}

pub(crate) fn start_destination_server(
    source: u32,
    edit: u32,
    place: i64,
    universe: i64,
    file: &Path,
) -> Result<u32> {
    let identity = process_identity(source)?;
    let executable = process_executable_path(source)?;
    let memory = Memory::for_process(source, Duration::from_secs(15))?;
    let catalog = Catalog::open(&memory)?;
    let binding = catalog.server_loader()?;
    let mut args = process_arguments(source)?;
    let value = |key: &str| {
        args.windows(2)
            .find(|p| p[0].eq_ignore_ascii_case(key))
            .map(|p| p[1].as_str())
    };
    anyhow::ensure!(
        value("-task") == Some("StartServer")
            && value("-editpid") == Some(&edit.to_string())
            && value("-parentPid") == Some(&edit.to_string())
            && value("-universeId") == Some(&universe.to_string()),
        "Source server does not belong to this Edit controller"
    );
    for (key, value) in [
        ("-placeId", place.to_string()),
        ("-port", "0".into()),
        ("-numTestServerPlayersUponStartup", "0".into()),
    ] {
        let at = args
            .iter()
            .position(|s| s.eq_ignore_ascii_case(key))
            .context("Missing source server launch argument")?;
        *args
            .get_mut(at + 1)
            .context("Incomplete server launch argument")? = value;
    }
    let directory = std::env::temp_dir().join("renium-native/test-servers");
    fs::create_dir_all(&directory)?;
    let lease_path = directory.join(format!(
        "{source}-{}-{}-{place}.input",
        identity.0, identity.1
    ));
    let mut lease = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .mode(0o600)
        .open(&lease_path)?;
    lease.lock()?;
    for pid in crate::studio::input::studio_process_ids() {
        if process_arguments(pid)
            .ok()
            .is_some_and(|actual| actual.get(1..) == args.get(1..))
        {
            return Ok(pid);
        }
    }
    anyhow::ensure!(
        process_identity(source)? == identity,
        "Source test ended during preparation"
    );
    lease.set_len(0)?;
    let file = file.to_str().context("Destination path is not UTF-8")?;
    anyhow::ensure!(
        file.len() < 4000 && file.starts_with('/') && !file.contains('\0'),
        "Invalid destination file"
    );
    let mut input = vec![0; 80];
    for (at, value) in [(0, 1), (4, 80), (8, source), (12, file.len() as u32)] {
        put32(&mut input, at, value);
    }
    for (at, value) in [
        (16, identity.0),
        (24, identity.1),
        (32, binding[0]),
        (40, binding[1]),
        (48, binding[2]),
        (56, binding[3]),
    ] {
        put64(&mut input, at, value);
    }
    input[64..80].copy_from_slice(&memory.trace.image_uuid);
    lease.write_all(&input)?;
    lease.write_all(file.as_bytes())?;
    lease.flush()?;
    let bundle = executable
        .ancestors()
        .find(|path| path.extension() == Some(OsStr::new("app")))
        .context("Studio app bundle ended")?;
    let diagnostics = lease_path.with_extension("stderr");
    let _cleanup = crate::system::files::OnDrop::new(|| {
        let _ = fs::remove_file(&diagnostics);
        let _ = fs::remove_file(&lease_path);
    });
    let _diagnostics = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&diagnostics)?;
    let before = crate::studio::input::studio_process_ids();
    let status = Command::new("/usr/bin/open")
        .args(["-g", "-n", "-a"])
        .arg(bundle)
        .arg("--stderr")
        .arg(&diagnostics)
        .args(["--env", "RENIUM_BACKGROUND_LAUNCH=1", "--env"])
        .arg(format!("RENIUM_TEST_SERVER_INPUT={}", lease_path.display()))
        .arg("--args")
        .args(&args[1..])
        .status()?;
    anyhow::ensure!(status.success(), "Destination Studio launch failed");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        anyhow::ensure!(
            process_identity(source)? == identity,
            "Source ended during destination launch"
        );
        if let Ok(log) = fs::read_to_string(&diagnostics)
            && let Some(error) = log
                .lines()
                .find_map(|line| line.strip_prefix("Renium destination server: "))
        {
            bail!(
                "Destination server startup failed: {}",
                error.chars().take(2048).collect::<String>()
            );
        }
        for pid in crate::studio::input::studio_process_ids() {
            if !before.contains(&pid)
                && !lease_path.exists()
                && process_arguments(pid)
                    .ok()
                    .is_some_and(|actual| actual.get(1..) == args.get(1..))
            {
                return Ok(pid);
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    bail!("Destination server did not start")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires an explicitly owned Studio process"]
    fn installed_mac_client_routing() -> Result<()> {
        let pid = std::env::var("RENIUM_TELEPORT_TEST_PID")?.parse()?;
        let started = Instant::now();
        println!("{}", discover_client(pid)?);
        println!("Client discovery: {:?}", started.elapsed());
        Ok(())
    }
}
