//! The separate test server receives its file input from the Edit controller.
//! Bind that input before its loader initializes services or runs game scripts.
use super::*;
use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction, Mnemonic, OpKind, Register};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValueOrigin {
    Capture(i64),
    Payload(i64, i64),
}

impl ValueOrigin {
    fn offset(self, offset: i64) -> Option<Self> {
        Some(match self {
            Self::Capture(at) => Self::Capture(at.checked_add(offset)?),
            Self::Payload(capture, at) => Self::Payload(capture, at.checked_add(offset)?),
        })
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct TestServerInput {
    pub slot: usize,
    pub callback: usize,
    pub capture: usize,
    pub path: usize,
    #[serde(skip)]
    code: Vec<(usize, Vec<u8>)>,
}

fn address(i: &Instruction, registers: &HashMap<Register, ValueOrigin>) -> Option<ValueOrigin> {
    if i.is_ip_rel_memory_operand() || i.memory_index() != Register::None {
        return None;
    }
    registers
        .get(&i.memory_base().full_register())?
        .offset(i.memory_displacement64() as i64)
}

fn fields(
    entry: usize,
    registers: HashMap<Register, ValueOrigin>,
    relevant: &HashSet<usize>,
    readers: &HashSet<usize>,
    string_copy: usize,
    code: &BTreeMap<usize, Vec<u8>>,
    depth: usize,
) -> Vec<(i64, i64)> {
    if depth > 4 {
        return Vec::new();
    }
    let Some(bytes) = code.get(&entry) else {
        return Vec::new();
    };
    let instructions: Vec<_> = Decoder::with_ip(64, bytes, entry as u64, DecoderOptions::NONE)
        .into_iter()
        .collect();
    let reads_file = instructions.iter().any(|i| {
        i.flow_control() == FlowControl::Call
            && readers.contains(&(i.near_branch_target() as usize))
    });
    let mut registers = registers;
    let mut found = Vec::new();
    for i in instructions {
        if i.flow_control() == FlowControl::IndirectCall
            && i.is_ip_rel_memory_operand()
            && i.ip_rel_memory_address() as usize == string_copy
            && reads_file
            && let Some(ValueOrigin::Payload(capture, path)) = registers.get(&Register::RDX)
        {
            found.push((*capture, *path));
        }
        if i.op0_kind() == OpKind::NearBranch64
            && matches!(
                i.flow_control(),
                FlowControl::Call | FlowControl::UnconditionalBranch
            )
        {
            let target = i.near_branch_target() as usize;
            if relevant.contains(&target) {
                found.extend(fields(
                    target,
                    registers.clone(),
                    relevant,
                    readers,
                    string_copy,
                    code,
                    depth + 1,
                ));
            }
        }
        if matches!(
            i.flow_control(),
            FlowControl::Call | FlowControl::IndirectCall
        ) {
            for register in [
                Register::RAX,
                Register::RCX,
                Register::RDX,
                Register::R8,
                Register::R9,
                Register::R10,
                Register::R11,
            ] {
                registers.remove(&register);
            }
            continue;
        }
        if i.op0_kind() != OpKind::Register {
            continue;
        }
        let destination = i.op0_register().full_register();
        let value = match i.mnemonic() {
            Mnemonic::Mov if i.op1_kind() == OpKind::Register => {
                registers.get(&i.op1_register().full_register()).copied()
            }
            Mnemonic::Mov if i.op1_kind() == OpKind::Memory && i.memory_size().size() == 8 => {
                match address(&i, &registers) {
                    Some(ValueOrigin::Capture(capture)) => Some(ValueOrigin::Payload(capture, 0)),
                    _ => None,
                }
            }
            Mnemonic::Lea => address(&i, &registers),
            Mnemonic::Add | Mnemonic::Sub
                if matches!(
                    i.op1_kind(),
                    OpKind::Immediate8to64 | OpKind::Immediate32to64
                ) =>
            {
                registers.get(&destination).and_then(|origin| {
                    origin.offset(
                        (i.immediate(1) as i64)
                            * if i.mnemonic() == Mnemonic::Sub { -1 } else { 1 },
                    )
                })
            }
            Mnemonic::Cmp | Mnemonic::Test | Mnemonic::Push => continue,
            _ => None,
        };
        registers.remove(&destination);
        if let Some(value) = value {
            registers.insert(destination, value);
        }
    }
    found
}

pub(crate) fn test_server_input(image_path: &std::path::Path) -> Result<TestServerInput> {
    let mapped = observation::ImageView::open(image_path)?;
    let pe = PeImage::parse(mapped.bytes())?;
    let anchors = image::anchors(
        &pe,
        &[
            "Studio.App.DataModelLoading.CannotOpenFileForRead".into(),
            "XPEAVDataModel@RBX@@@std@@".into(),
        ],
    )?;
    let tables = image::rtti(&pe, &anchors)?;
    let index = image::index(&pe, &anchors.iter().map(|a| a.rva).collect())?;
    let readers: HashSet<_> = anchors
        .iter()
        .filter(|a| a.text == "Studio.App.DataModelLoading.CannotOpenFileForRead")
        .flat_map(|a| {
            index
                .references
                .get(&a.rva)
                .into_iter()
                .flatten()
                .map(|e| e.target)
        })
        .collect();
    // Studio has more than one file-reader overload with this diagnostic.
    // The payload callback and its QString data flow below select the reader;
    // the diagnostic alone is not a unique function identity.
    anyhow::ensure!(!readers.is_empty(), "Separate test file reader is missing");
    let imports = image::imports(&pe)?;
    let string_copy = imports
        .as_array()
        .context("Missing Qt import table")?
        .iter()
        .find(|row| row["module"] == "Qt5Core.dll" && row["symbol"] == "??0QString@@QEAA@AEBV0@@Z")
        .and_then(|row| row["iatRva"].as_u64())
        .context("Missing Qt file-input copy")? as usize;
    let mut relevant = readers.clone();
    let mut frontier = readers.clone();
    for _ in 0..4 {
        frontier = frontier
            .iter()
            .flat_map(|entry| {
                index
                    .callers
                    .get(entry)
                    .into_iter()
                    .flatten()
                    .map(|e| e.target)
            })
            .filter(|entry| !relevant.contains(entry))
            .collect();
        relevant.extend(&frontier);
    }
    let mut code = BTreeMap::new();
    for entry in &relevant {
        let (begin, end) = image::code_range(&pe, &index, *entry)?;
        if begin == *entry && !index.invalid.contains(entry) {
            code.insert(
                *entry,
                slice(pe.bytes, pe.rva_to_offset(begin)?, end - begin)?.to_vec(),
            );
        }
    }
    let mut matches = Vec::new();
    for table in tables {
        let kind = table["type"].as_str().unwrap_or_default();
        if !kind.starts_with(".?AV?$_Func_impl_no_alloc@")
            || !kind.ends_with("@@XPEAVDataModel@RBX@@@std@@")
            || table["offsetToTop"] != 0
        {
            continue;
        }
        for method in table["slots"]
            .as_array()
            .context("Missing file-loader callback methods")?
        {
            let callback = method["targetRva"]
                .as_u64()
                .context("Missing callback function")? as usize;
            // std::function's invoker may be a leaf tail-call thunk without an
            // unwind entry, so it is absent from the caller index. Decode the
            // RTTI-owned slot directly and follow its target into the graph.
            if let Ok((begin, end)) = image::code_range(&pe, &index, callback)
                && begin == callback
                && !index.invalid.contains(&callback)
            {
                code.entry(callback)
                    .or_insert(slice(pe.bytes, pe.rva_to_offset(begin)?, end - begin)?.to_vec());
            }
            let found = fields(
                callback,
                HashMap::from([(Register::RCX, ValueOrigin::Capture(0))]),
                &relevant,
                &readers,
                string_copy,
                &code,
                0,
            );
            if let [(capture, path)] = found.as_slice()
                && (0..=65536).contains(capture)
                && capture % 8 == 0
                && (0..=65536).contains(path)
                && path % 8 == 0
            {
                matches.push(TestServerInput {
                    slot: table["vtableRva"]
                        .as_u64()
                        .context("Missing callback table")? as usize
                        + method["byteOffset"]
                            .as_u64()
                            .context("Missing callback slot")? as usize,
                    callback,
                    capture: *capture as usize,
                    path: *path as usize,
                    code: code
                        .iter()
                        .map(|(rva, bytes)| (*rva, bytes.clone()))
                        .collect(),
                });
            }
        }
    }
    anyhow::ensure!(
        matches.len() == 1,
        "Separate server input resolved {} callbacks",
        matches.len()
    );
    Ok(matches.remove(0))
}

impl TestServerInput {
    fn arm(&self, pid: u32, root_pid: u32, file: &std::path::Path) -> Result<()> {
        let memory = ProcessMemory::open_read_only(pid)?;
        let loaded = modules(pid)?;
        let studio = loaded.first().context("Destination server process ended")?;
        for (rva, bytes) in &self.code {
            anyhow::ensure!(
                memory.read_vec(studio.base + rva, bytes.len())? == *bytes,
                "Separate server loader code changed after discovery"
            );
        }
        anyhow::ensure!(
            memory.read_u64(studio.base + self.slot)? as usize == studio.base + self.callback,
            "Separate server loader callback was replaced"
        );
        crate::project::workflows::windows_launch::prepare_test_server(
            pid,
            root_pid,
            [
                self.slot as u64,
                self.callback as u64,
                self.capture as u64,
                self.path as u64,
            ],
            file,
        )
    }
}

pub(super) fn arguments(command: &str) -> Result<Vec<std::ffi::OsString>> {
    use std::os::windows::ffi::OsStringExt;
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn CommandLineToArgvW(command: *const u16, count: *mut i32) -> *mut *mut u16;
    }
    let command: Vec<_> = command.encode_utf16().chain([0]).collect();
    let mut count = 0;
    let pointer = unsafe { CommandLineToArgvW(command.as_ptr(), &mut count) };
    anyhow::ensure!(!pointer.is_null(), "Cannot parse the source test task");
    let result = (|| {
        anyhow::ensure!(
            (1..=256).contains(&count),
            "Invalid source test argument count"
        );
        let mut result = Vec::new();
        for index in 0..count as usize {
            let value = unsafe { *pointer.add(index) };
            let length = (0..32768)
                .find(|offset| unsafe { *value.add(*offset) == 0 })
                .context("Unterminated test task argument")?;
            result.push(std::ffi::OsString::from_wide(unsafe {
                std::slice::from_raw_parts(value, length)
            }));
        }
        Ok(result)
    })();
    unsafe { windows_sys::Win32::Foundation::LocalFree(pointer.cast()) };
    result
}

/// Launch the same native test task with destination input. Parent session,
/// bootstrap protocol and root Stop identity come from the live source task.
pub(crate) fn start_destination_server(
    source_pid: u32,
    edit_pid: u32,
    place: i64,
    universe: i64,
    file: &std::path::Path,
) -> Result<u32> {
    let loaded = modules(source_pid)?;
    let image = loaded.first().context("Source test server ended")?;
    let command = crate::studio::performance::process_command_line(source_pid)
        .context("Cannot read the owned source test task")?;
    let source_identity = crate::app::update::process_start_identity(source_pid)
        .context("Source test server ended")?;
    let mut args = arguments(&command)?;
    let value = |name: &str| -> Option<&std::ffi::OsString> {
        args.windows(2)
            .find(|pair| {
                pair[0]
                    .to_str()
                    .is_some_and(|v| v.eq_ignore_ascii_case(name))
            })
            .map(|pair| &pair[1])
    };
    anyhow::ensure!(
        args.first()
            .is_some_and(|p| std::path::Path::new(p) == image.path)
            && value("-task").is_some_and(|v| v == "StartServer")
            && value("-editpid").is_some_and(|v| v.to_str() == Some(&edit_pid.to_string()))
            && value("-parentPid").is_some_and(|v| v.to_str() == Some(&edit_pid.to_string()))
            && value("-universeId").is_some_and(|v| v.to_str() == Some(&universe.to_string())),
        "The source server no longer belongs to this Edit controller"
    );
    for (name, replacement) in [
        ("-placeId", place.to_string()),
        ("-port", "0".into()),
        ("-numTestServerPlayersUponStartup", "0".into()),
    ] {
        let index = args
            .iter()
            .position(|arg| arg.to_str().is_some_and(|v| v.eq_ignore_ascii_case(name)))
            .context("Source test task omitted a required server argument")?;
        anyhow::ensure!(
            index + 1 < args.len(),
            "Incomplete source test task argument"
        );
        args[index + 1] = replacement.into();
    }
    // Serialize creation across callers and daemon restarts. The native task
    // arguments retain Studio's root session identity; an already running
    // destination is reusable even before its bridge has connected.
    let directory = std::env::temp_dir()
        .join("renium-native")
        .join("test-servers");
    std::fs::create_dir_all(&directory)?;
    let lock = std::fs::File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join(format!("{source_pid}-{source_identity}-{place}.lock")))?;
    lock.lock()?;
    let mut existing = Vec::new();
    for process in crate::studio::performance::process_entries()?
        .into_iter()
        .filter(|process| process.studio)
    {
        if crate::studio::performance::process_command_line(process.pid)
            .and_then(|command| arguments(&command).ok())
            .as_ref()
            == Some(&args)
        {
            existing.push(process.pid);
        }
    }
    anyhow::ensure!(
        existing.len() <= 1,
        "This root playtest already has multiple destination servers; stop the duplicate before traveling"
    );
    anyhow::ensure!(
        crate::app::update::process_start_identity(source_pid).as_ref() == Some(&source_identity),
        "Source test server ended during destination preparation"
    );
    if let Some(pid) = existing.first() {
        return Ok(*pid);
    }
    let binding = test_server_input(&image.path)?;
    crate::project::workflows::windows_launch::spawn_prepared(
        &image.path,
        &args[1..]
            .iter()
            .map(|value| value.as_os_str())
            .collect::<Vec<_>>(),
        Some(edit_pid),
        |pid| binding.arm(pid, source_pid, file),
    )
    .map(|(pid, ())| pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "inspects an explicitly supplied installed Studio image"]
    fn installed_server_input_binding() -> Result<()> {
        let image = std::env::var("RENIUM_TELEPORT_TEST_IMAGE")?;
        println!(
            "{}",
            serde_json::to_string(&test_server_input(std::path::Path::new(&image))?)?
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires explicitly owned source server, Edit controller and saved destination"]
    fn owned_destination_server_load() -> Result<()> {
        let source = std::env::var("RENIUM_TELEPORT_TEST_SERVER_PID")?.parse()?;
        let edit = std::env::var("RENIUM_TELEPORT_TEST_PID")?.parse()?;
        let destination = std::env::var("RENIUM_TELEPORT_TEST_DESTINATION")?.parse()?;
        let universe = std::env::var("RENIUM_TELEPORT_TEST_UNIVERSE")?.parse()?;
        let file = std::env::var("RENIUM_TELEPORT_TEST_FILE")?;
        let pid = start_destination_server(
            source,
            edit,
            destination,
            universe,
            std::path::Path::new(&file),
        )?;
        println!(
            "{}",
            serde_json::json!({"pid":pid,"destination":destination})
        );
        Ok(())
    }
}
