use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::Path;

pub(super) fn create(name: &str, path: Option<&Path>) -> Result<Value> {
    let manifest = json!({
        "schemaVersion":1,"name":name,"version":"0.1.0","description":"A Renium workflow plugin",
        "executable":{"windows":[format!("target/release/{name}.exe")],"macos":[format!("target/release/{name}")],"linux":[format!("target/release/{name}")]},
        "permissions":[],"guide":"AGENTS.md",
        "commands":{"hello":{"description":"Say hello without opening Studio","arguments":[{"name":"name","type":"string","description":"Who to greet","default":"World"}]}}
    });
    super::manifest::validate(&serde_json::from_value(manifest.clone())?)?;
    let root = std::path::absolute(path.unwrap_or_else(|| Path::new(name)))?;
    if root.try_exists()? {
        bail!("Destination already exists: {}", root.display());
    }
    fs::create_dir(&root)?;
    let files = [
        ("renium-plugin.json", serde_json::to_string_pretty(&manifest)?),
        ("Cargo.toml", format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\nrust-version = \"1.89\"\n\n[workspace]\n\n[dependencies]\nrenium-plugin-sdk = {{ path = \"sdk\" }}\n")),
        ("src/main.rs", "use renium_plugin_sdk::{json, serve};\n\nfn main() -> std::process::ExitCode {\n    serve(|request| {\n        Ok(json!({\"message\": format!(\"Hello, {}!\", request.arguments[\"name\"].as_str().unwrap_or(\"World\"))}))\n    })\n}\n".into()),
        ("AGENTS.md", format!("# {name}\n\nUse `rbx {name} hello --name World` to greet someone. This command is offline and does not need Studio or Play. Read `rbx {name} --help` for commands.\n")),
        ("README.md", format!("# {name}\n\n1. Edit `renium-plugin.json` to define commands, arguments and permissions.\n2. Implement the handler in `src/main.rs`; use the included SDK to call Renium.\n3. Run `cargo build --release`.\n4. Run `rbx plugin install . --dev` while developing; omit `--dev` for a pinned install.\n5. Run `rbx {name} hello --name World`.\n\nNo Renium source edits or global SDK installation are needed. Keep stdout for the JSON protocol and diagnostics on stderr. Plugins are trusted native programs, not security sandboxes. The `studio` permission is disclosure, not permission to interrupt a user's session.\n")),
        (".gitignore", "/target/\n/sdk/target/\n".into()),
        ("sdk/Cargo.toml", include_str!("../../../renium-plugin-sdk/Cargo.toml").replace("../../LICENSE", "LICENSE")),
        ("sdk/LICENSE", include_str!("../../../../LICENSE").into()),
        ("sdk/src/lib.rs", include_str!("../../../renium-plugin-sdk/src/lib.rs").into()),
        ("sdk/src/process.rs", include_str!("../../../renium-plugin-sdk/src/process.rs").into()),
        ("sdk/src/lease.rs", include_str!("../../../renium-plugin-sdk/src/lease.rs").into()),
    ];
    for (path, content) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::File::options()
            .write(true)
            .create_new(true)
            .open(path)?
            .write_all(content.as_bytes())?;
    }
    Ok(
        json!({"created":root,"next":["cargo build --release","rbx plugin install . --dev"],"compiled":false}),
    )
}
