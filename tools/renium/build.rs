use std::collections::HashSet;
use std::env;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

fn run(command: &mut Command, label: &str) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to start {label}: {error}"));
    assert!(status.success(), "{label} failed");
}

fn build_windows(out_dir: &Path) {
    let source = PathBuf::from("native").join("renium_studio_helper.cpp");
    let output = out_dir.join("renium-studio-helper.dll");
    let mut build = cc::Build::new();
    build.cpp(true).static_crt(true);
    let compiler = build.get_compiler();
    let mut command = compiler.to_command();
    command.args([
        "/nologo",
        "/LD",
        "/O2",
        "/EHsc",
        "/std:c++20",
        "/MT",
        "/DUNICODE",
        "/D_UNICODE",
    ]);
    command.arg(&source);
    command.arg(format!("/Fo{}\\", out_dir.display()));
    command.arg(format!("/Fe{}", output.display()));
    command.args(["/link", "/INCREMENTAL:NO", "/Brepro"]);
    command.arg(format!(
        "/IMPLIB:{}",
        out_dir.join("renium-studio-helper.lib").display()
    ));
    run(&mut command, "Windows Studio helper build");
    println!("cargo:rerun-if-changed={}", source.display());
}

fn build_macos(out_dir: &Path) {
    let helper_source = PathBuf::from("native").join("renium_studio_helper_macos.cpp");
    let launcher_source = PathBuf::from("native").join("renium_studio_launcher_macos.c");
    let shield_source = PathBuf::from("native").join("renium_input_shield_macos.m");
    let helper = out_dir.join("renium-studio-helper.dylib");
    let launcher = out_dir.join("renium-studio-launcher");
    let shield = out_dir.join("renium-input-shield");
    let mut helper_command = Command::new("clang++");
    helper_command.args([
        "-dynamiclib",
        "-O2",
        "-std=c++20",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-fvisibility=hidden",
        "-pthread",
        "-Wl,-dead_strip",
        "-o",
    ]);
    helper_command.arg(&helper).arg(&helper_source);
    run(&mut helper_command, "macOS Studio helper build");
    let mut launcher_command = Command::new("clang");
    launcher_command.args([
        "-O2",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-Wl,-dead_strip",
        "-o",
    ]);
    launcher_command.arg(&launcher).arg(&launcher_source);
    run(&mut launcher_command, "macOS Studio launcher build");
    let mut shield_command = Command::new("clang");
    shield_command.args([
        "-O2",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-fobjc-arc",
        "-framework",
        "AppKit",
        "-framework",
        "ApplicationServices",
        "-o",
    ]);
    shield_command.arg(&shield).arg(&shield_source);
    run(&mut shield_command, "macOS input shield build");
    println!("cargo:rerun-if-changed={}", helper_source.display());
    println!("cargo:rerun-if-changed={}", launcher_source.display());
    println!("cargo:rerun-if-changed={}", shield_source.display());
}

fn build_linux(out_dir: &Path) {
    let source = PathBuf::from("native").join("renium_input_shield_linux.c");
    let output = out_dir.join("renium-input-shield");
    let mut command = cc::Build::new().get_compiler().to_command();
    command.args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror", "-o"]);
    command.arg(&output).arg(&source).arg("-ldl");
    run(&mut command, "Linux input shield build");
    println!("cargo:rerun-if-changed={}", source.display());
}

fn emit_build_metadata() {
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let build_timestamp = env::var("SOURCE_DATE_EPOCH").unwrap_or_else(|_| {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or_else(
            |_| "0".to_string(),
            |duration| duration.as_secs().to_string(),
        )
    });
    println!("cargo:rustc-env=BUILD_GIT_HASH={git_hash}");
    println!("cargo:rustc-env=BUILD_TIMESTAMP_UNIX={build_timestamp}");
}

fn emit_instruction_revision() {
    let mut paths = vec![PathBuf::from("renium-agents.md")];
    let mut guides = std::fs::read_dir("renium-guides")
        .expect("failed to read agent guides")
        .map(|entry| entry.expect("failed to read an agent guide").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
        .collect::<Vec<_>>();
    guides.sort();
    paths.extend(guides);

    let mut hash = Sha256::new();
    for path in paths {
        hash.update(path.to_string_lossy().replace('\\', "/").as_bytes());
        hash.update([0]);
        hash.update(std::fs::read(&path).expect("failed to read agent instructions"));
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!(
        "cargo:rustc-env=RENIUM_INSTRUCTIONS_REVISION={:x}",
        hash.finalize()
    );
}

fn generate_operations(out_dir: &Path) {
    let path = Path::new("protocol").join("opcodes.json");
    let text = std::fs::read_to_string(&path).expect("failed to read opcode registry");
    let registry: serde_json::Value =
        serde_json::from_str(&text).expect("failed to parse opcode registry");
    let version = registry["version"]
        .as_u64()
        .expect("opcode registry version must be an unsigned integer");
    let version = u8::try_from(version).expect("opcode registry version must fit in u8");
    let operations = registry["operations"]
        .as_array()
        .expect("opcode registry operations must be an array");
    let mut constants = format!("pub const PROTOCOL_VERSION: u8 = {version};\n");
    let mut entries = String::new();
    let mut ids = HashSet::new();
    let mut names = HashSet::new();
    for operation in operations {
        let name = operation["name"]
            .as_str()
            .expect("opcode name must be a string");
        let id = operation["id"]
            .as_u64()
            .expect("opcode id must be an unsigned integer");
        let id = u16::try_from(id).expect("opcode id must fit in u16");
        assert!(ids.insert(id), "duplicate opcode {id}");
        assert!(names.insert(name), "duplicate operation name {name}");
        let constant = name.replace('-', "_").to_ascii_uppercase();
        writeln!(constants, "pub const {constant}: u16 = {id};")
            .expect("failed to generate opcode");
        let review = operation
            .get("review")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let runtime = operation
            .get("runtime")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let queued = operation
            .get("queued")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        writeln!(
            entries,
            "    super::Opcode {{ id: {id}, name: {name:?}, review: {review}, runtime: {runtime}, queued: {queued} }},"
        )
        .expect("failed to generate opcode metadata");
    }
    let mut source = constants;
    source.push_str("pub static REGISTRY: &[super::Opcode] = &[\n");
    source.push_str(&entries);
    source.push_str("];\n");
    std::fs::write(out_dir.join("operations.rs"), source)
        .expect("failed to write generated operations");
    println!("cargo:rerun-if-changed={}", path.display());
}

fn embed_windows_manifest(out_dir: &Path) {
    let manifest_path = out_dir.join("renium.exe.manifest");
    let manifest = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings xmlns:ws2="http://schemas.microsoft.com/SMI/2016/WindowsSettings">
      <ws2:longPathAware>true</ws2:longPathAware>
    </windowsSettings>
  </application>
</assembly>
"#;
    std::fs::write(&manifest_path, manifest).expect("failed to write Windows manifest");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest_path.display()
    );
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    emit_build_metadata();
    emit_instruction_revision();
    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS is missing");
    let host = env::var("HOST").expect("HOST is missing");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is missing"));
    generate_operations(&out_dir);
    match target_os.as_str() {
        "windows" => {
            build_windows(&out_dir);
            embed_windows_manifest(&out_dir);
        }
        "macos" if host.contains("apple-darwin") => build_macos(&out_dir),
        "macos" if env::var_os("RENIUM_SKIP_MAC_HELPER_BUILD").is_some() => {
            std::fs::write(out_dir.join("renium-studio-helper.dylib"), [])
                .expect("failed to create macOS helper placeholder");
            std::fs::write(out_dir.join("renium-studio-launcher"), [])
                .expect("failed to create macOS launcher placeholder");
            std::fs::write(out_dir.join("renium-input-shield"), [])
                .expect("failed to create macOS input shield placeholder");
        }
        "macos" => panic!(
            "macOS helper artifacts must be built on macOS; set RENIUM_SKIP_MAC_HELPER_BUILD=1 only for cross-target checking"
        ),
        "linux" if host.contains("linux") => build_linux(&out_dir),
        "linux" if env::var_os("RENIUM_SKIP_LINUX_HELPER_BUILD").is_some() => {
            std::fs::write(out_dir.join("renium-input-shield"), [])
                .expect("failed to create Linux input shield placeholder");
        }
        "linux" => panic!(
            "Linux helper artifacts must be built on Linux; set RENIUM_SKIP_LINUX_HELPER_BUILD=1 only for cross-target checking"
        ),
        _ => {}
    }
}
