use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const RENIUM: &str = env!("CARGO_BIN_EXE_renium");
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A scratch folder that is also the home of every process started in it,
/// so the agent configs of the real home are never touched, the user's
/// Renium settings are not read, and no Studio audio helper starts.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("renium-mcp-{tag}-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(RENIUM);
        command
            .args(args)
            .current_dir(&self.0)
            .env("HOME", &self.0)
            .env("USERPROFILE", &self.0)
            .env("LOCALAPPDATA", &self.0)
            .env("APPDATA", &self.0)
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env("RENIUM_NO_AUDIO_HELPERS", "1")
            .env_remove("RENIUM_AGENT_CLI");
        command
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Server {
    child: Child,
    input: ChildStdin,
    lines: mpsc::Receiver<String>,
    id: u64,
}

impl Server {
    fn start(scratch: &Scratch) -> Self {
        let mut child = scratch
            .command(&["mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in output.lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            lines,
            id: 0,
        }
    }

    fn send(&mut self, message: Value) {
        writeln!(self.input, "{message}").unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let id = self.id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let response = self.receive();
        assert_eq!(response["jsonrpc"], "2.0", "{response}");
        assert_eq!(response["id"], id, "{response}");
        response
    }

    fn receive(&self) -> Value {
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(60))
            .expect("the server answers every request");
        serde_json::from_str(&line).expect("one JSON message per line")
    }

    fn call(&mut self, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": "rbx", "arguments": arguments}))["result"].clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_client_initializes_lists_the_tool_and_runs_rbx() {
    let scratch = Scratch::new("handshake");
    let mut server = Server::start(&scratch);
    let client = json!({"name": "test", "version": "1"});
    let initialized = server.request(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": client}),
    );
    assert_eq!(
        initialized["result"],
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "renium", "version": VERSION},
        })
    );
    server.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    assert_eq!(server.request("ping", json!({}))["result"], json!({}));
    let listed = server.request("tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "rbx");
    assert_eq!(
        server.call(json!({"args": ["--version"]})),
        json!({"content": [{"type": "text", "text": format!("renium {VERSION}")}], "isError": false})
    );
    assert_eq!(
        server.request("resources/list", json!({}))["error"]["code"],
        -32601
    );
    let older = server.request(
        "initialize",
        json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": client}),
    );
    assert_eq!(older["result"]["protocolVersion"], "2024-11-05");
}

#[test]
fn the_tool_definition_stays_under_a_thousand_characters() {
    let scratch = Scratch::new("size");
    let mut server = Server::start(&scratch);
    let listed = server.request("tools/list", json!({}));
    let tool = listed["result"]["tools"][0].to_string();
    let characters = tool.chars().count();
    assert!(characters < 1000, "{characters} characters: {tool}");
}

#[test]
fn steps_stop_at_the_first_failure() {
    let scratch = Scratch::new("steps");
    let mut server = Server::start(&scratch);
    let result = server.call(json!({
        "steps": [["ck", "-"], ["no-such-command"], ["init"]],
        "stdin": "return 1\n",
    }));
    assert_eq!(result["isError"], true, "{result}");
    let content = result["content"].as_array().unwrap();
    assert_eq!(content.len(), 2, "{result}");
    assert!(
        content[1]["text"]
            .as_str()
            .unwrap()
            .contains("Unknown command 'no-such-command'"),
        "{result}"
    );
    assert!(!scratch.0.join("RENIUM.md").exists());
    let broken = server.call(json!({"args": ["ck", "-"], "stdin": "local = 1\n"}));
    assert_eq!(broken["isError"], true, "{broken}");
}

#[test]
fn a_batch_is_answered_with_one_array() {
    let scratch = Scratch::new("batch");
    let mut server = Server::start(&scratch);
    let call = json!({"name": "rbx", "arguments": {"args": ["--version"]}});
    server.send(json!([
        {"jsonrpc": "2.0", "id": 1, "method": "ping"},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": call},
        {"jsonrpc": "2.0", "id": 3, "method": "resources/list"},
    ]));
    let answer = server.receive();
    let mut responses = answer
        .as_array()
        .unwrap_or_else(|| panic!("a batch is answered with an array: {answer}"))
        .clone();
    responses.sort_by_key(|response| response["id"].as_u64());
    assert_eq!(responses.len(), 3, "{answer}");
    assert_eq!(
        responses[0],
        json!({"jsonrpc": "2.0", "id": 1, "result": {}})
    );
    assert_eq!(
        responses[1]["result"]["content"][0]["text"],
        format!("renium {VERSION}")
    );
    assert_eq!(responses[2]["error"]["code"], -32601);

    // Notifications alone get no answer, so the next line answers the empty batch.
    server.send(json!([{"jsonrpc": "2.0", "method": "notifications/initialized"}]));
    server.send(json!([]));
    let invalid = server.receive();
    assert_eq!(invalid["error"]["code"], -32600, "{invalid}");
    assert_eq!(invalid["id"], Value::Null, "{invalid}");
}

/// Whether something accepts connections on this local port.
#[cfg(windows)]
fn listening(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    )
    .is_ok()
}

#[cfg(windows)]
fn wait_until(what: &str, mut holds: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !holds() {
        assert!(std::time::Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `docs --serve` never ends by itself and is reachable on its port for as
/// long as it runs.
#[cfg(windows)]
#[test]
fn a_running_command_ends_when_the_server_is_killed() {
    let scratch = Scratch::new("killed");
    // A fresh update check on record keeps the command from starting one.
    let checked = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    fs::create_dir_all(scratch.0.join("Renium")).unwrap();
    fs::write(
        scratch.0.join("Renium").join("update-check.json"),
        json!({"schemaVersion": 2, "checkedAtUnixMs": checked, "error": "not checked in tests"})
            .to_string(),
    )
    .unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut server = Server::start(&scratch);
    let arguments = json!({"args": ["docs", "--serve", "--port", port.to_string()]});
    server.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": "rbx", "arguments": arguments},
    }));
    wait_until("the command serves its port", || {
        if let Ok(answer) = server.lines.try_recv() {
            panic!("the command ended instead of serving port {port}: {answer}");
        }
        listening(port)
    });
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    wait_until("the command ended with the server", || !listening(port));
}

#[test]
fn install_registers_claude_code_and_codex_once_and_uninstall_restores_them() {
    let home = Scratch::new("install");
    fs::create_dir_all(home.0.join(".claude")).unwrap();
    fs::create_dir_all(home.0.join(".codex")).unwrap();
    let claude_path = home.0.join(".claude.json");
    let codex_path = home.0.join(".codex").join("config.toml");
    let claude = "{\n  \"numStartups\": 3,\n  \"mcpServers\": {\n    \"other\": {\n      \"command\": \"node\"\n    }\n  }\n}\n";
    let codex = "model = \"gpt-5\"\n# keep this comment\n\n[projects.\"C:\\\\work\"]\ntrust_level = \"trusted\"\n";
    fs::write(&claude_path, claude).unwrap();
    fs::write(&codex_path, codex).unwrap();
    let register = |flag: &str| -> Value {
        let output = home
            .command(&["--output-mode", "json", "mcp", flag])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };

    let installed = register("--install");
    assert_eq!(installed["ok"], true, "{installed}");
    assert_eq!(
        installed["changed"],
        json!([claude_path, codex_path]),
        "{installed}"
    );
    let command = installed["command"].as_str().unwrap().to_string();
    assert_eq!(
        fs::canonicalize(&command).unwrap(),
        fs::canonicalize(RENIUM).unwrap()
    );
    let claude_after: Value =
        serde_json::from_str(&fs::read_to_string(&claude_path).unwrap()).unwrap();
    assert_eq!(
        claude_after,
        json!({
            "numStartups": 3,
            "mcpServers": {
                "other": {"command": "node"},
                "renium": {"command": command, "args": ["mcp"]},
            },
        })
    );
    let codex_after = fs::read_to_string(&codex_path).unwrap();
    assert!(codex_after.starts_with(codex), "{codex_after}");
    let table: toml::Table = toml::from_str(&codex_after).unwrap();
    let entry = &table["mcp_servers"]["renium"];
    assert_eq!(entry["command"].as_str(), Some(command.as_str()));
    assert_eq!(entry["args"], toml::Value::Array(vec!["mcp".into()]));
    assert_eq!(table["model"].as_str(), Some("gpt-5"));
    assert_eq!(
        fs::read_to_string(home.0.join(".claude.json.renium.bak")).unwrap(),
        claude
    );
    assert_eq!(
        fs::read_to_string(home.0.join(".codex").join("config.toml.renium.bak")).unwrap(),
        codex
    );
    assert!(!home.0.join(".cursor").exists());

    let again = register("--install");
    assert_eq!(again["changed"], json!([]), "{again}");
    assert_eq!(
        again["present"],
        json!([claude_path, codex_path]),
        "{again}"
    );
    assert_eq!(fs::read_to_string(&codex_path).unwrap(), codex_after);

    let removed = register("--uninstall");
    assert_eq!(
        removed["changed"],
        json!([claude_path, codex_path]),
        "{removed}"
    );
    assert_eq!(fs::read_to_string(&claude_path).unwrap(), claude);
    assert_eq!(fs::read_to_string(&codex_path).unwrap(), codex);
}
