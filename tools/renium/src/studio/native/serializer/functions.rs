use anyhow::{Context, Result, ensure};
use serde_json::Value;

pub(super) struct FunctionInput {
    pub mode: u32,
    pub bytes: Vec<u8>,
}
#[cfg(windows)]
#[derive(Debug)]
pub(super) struct YieldContract {
    pub layout: u32,
    pub sites: Vec<[u64; 2]>,
}

impl FunctionInput {
    // Public RTTI and callable storage alone cannot authorize hidden callbacks.
    // Windows runs the structural detector for each image; macOS has no
    // qualified continuation adapter and refuses here.
    #[cfg(windows)]
    pub(super) fn qualify_yield(
        &mut self,
        qualify: impl FnOnce() -> Result<YieldContract>,
    ) -> Result<()> {
        self.bytes[52..56].fill(0);
        let contract = qualify()?;
        ensure!(
            !contract.sites.is_empty() && contract.sites.len() < 64,
            "yield-callback-site: invalid qualified caller count"
        );
        let encoded = contract.layout | ((contract.sites.len() as u32) << 26);
        for site in contract.sites {
            for address in site {
                self.bytes.extend_from_slice(&address.to_le_bytes());
            }
        }
        self.bytes[52..56].copy_from_slice(&encoded.to_le_bytes());
        Ok(())
    }

    /// Checks the enum arguments against the items this engine declares. A
    /// post names none: before it is prepared its first word is the zero the
    /// helper sends as the request type.
    pub(super) fn validate_enums(&self, items: &Value) -> Result<()> {
        let names: &[&str] = match self.mode {
            1 => &["ThrottlingPriority", "HttpRequestType"],
            2 => &["ThrottlingPriority", "HttpContentType", "HttpRequestType"],
            5 => &["HttpRequestType"],
            _ => &[],
        };
        for (index, name) in names.iter().enumerate() {
            let value = u32::from_le_bytes(self.bytes[32 + index * 4..36 + index * 4].try_into()?);
            let declared = items[*name]
                .as_array()
                .with_context(|| format!("http-enum-items detector: engine omitted {name}"))?;
            ensure!(
                declared
                    .iter()
                    .any(|item| item.as_u64() == Some(value as u64)),
                "http-enum-items detector: {name}={value} is not declared by this engine"
            );
        }
        Ok(())
    }

    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    #[cfg(any(target_os = "macos", test))]
    pub(super) fn validate_call_contract(
        &self,
        name: &str,
        module: &str,
        entry_rva: u64,
        field: usize,
    ) -> Result<()> {
        ensure!(
            !matches!(self.mode, 1 | 2 | 5),
            "yield-call-contract: HttpRbxApiService.{name}; candidate={module}+0x{entry_rva:X}, descriptor-field=0x{field:X}; hidden success/error continuation ABI, ownership and execution context unverified; no call executed"
        );
        Ok(())
    }
}

fn roblox_https(url: &url::Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url
            .host_str()
            .is_some_and(|host| host == "roblox.com" || host.ends_with(".roblox.com"))
        && url.port().is_none_or(|port| port == 443)
}

/// The address an authenticated request may go to, as the engine will read it.
fn roblox_url(raw: &str) -> Result<String> {
    ensure!(
        !raw.chars()
            .any(|c| c == '\\' || c == '@' || c.is_control() || c.is_whitespace()),
        "http-url detector: raw URL contains a backslash, @, control character or whitespace"
    );
    let url = url::Url::parse(raw).context("Expected an HTTPS Roblox URL")?;
    ensure!(
        roblox_https(&url),
        "Authenticated Studio requests require an HTTPS Roblox URL"
    );
    Ok(url.to_string())
}

// GetAsync and PostAsync send the signed-in session to a host the engine
// derives from its own base address, so that address has to be Roblox's.
pub(super) fn validate_api_base(name: &str, items: &Value) -> Result<()> {
    if !matches!(name, "GetAsync" | "PostAsync") {
        return Ok(());
    }
    let base = items["BaseUrl"].as_str().unwrap_or_default();
    ensure!(
        url::Url::parse(base).is_ok_and(|url| roblox_https(&url)),
        "http-relative-base detector: the engine's base address {base:?} is not an HTTPS Roblox address; use {name}FullUrl with an explicit HTTPS Roblox URL"
    );
    Ok(())
}

pub(super) fn input(class: &str, name: &str, arguments: &[Value]) -> Result<FunctionInput> {
    ensure!(
        class == "HttpRbxApiService",
        "Native function access does not yet support class {class}"
    );
    let (mode, required, maximum) = match name {
        "GetAsync" | "GetAsyncFullUrl" => (1u32, 1, 3),
        "PostAsync" | "PostAsyncFullUrl" => (2, 2, 5),
        "GetDocumentationUrl" => (3, 1, 1),
        _ => anyhow::bail!("Native function access does not yet support {class}.{name}"),
    };
    ensure!(
        (required..=maximum).contains(&arguments.len()),
        "{name} expects {required}–{maximum} arguments"
    );
    let first = arguments[0]
        .as_str()
        .context("First argument must be a string")?;
    let second = if mode == 2 {
        arguments[1].as_str().context("Body must be a string")?
    } else {
        ""
    };
    ensure!(
        first.len() + second.len() <= 60 * 1024,
        "Function arguments exceed 60 KiB"
    );
    let normalized;
    let first = if name.ends_with("FullUrl") {
        normalized = roblox_url(first)?;
        normalized.as_str()
    } else {
        first
    };
    if matches!(name, "GetAsync" | "PostAsync") {
        ensure!(
            first.starts_with('/')
                && !first.starts_with("//")
                && !first
                    .bytes()
                    .any(|byte| byte == b'\\' || byte <= b' ' || byte == 127 || byte == b'#'),
            "http-relative-path detector: expected /path on the engine's API host; authority, backslashes, fragments and whitespace are forbidden"
        );
    }
    ensure!(
        first.len() + second.len() <= 60 * 1024,
        "Normalized function arguments exceed 60 KiB"
    );
    let mut bytes = vec![0; 56];
    bytes[20..24].copy_from_slice(&mode.to_le_bytes());
    for (index, argument) in arguments.iter().skip(required).enumerate() {
        let number = argument
            .as_u64()
            .filter(|value| *value <= i32::MAX as u64)
            .context("Enum arguments must be nonnegative integers")? as u32;
        bytes[32 + index * 4..36 + index * 4].copy_from_slice(&number.to_le_bytes());
    }
    bytes[44..48].copy_from_slice(&(first.len() as u32).to_le_bytes());
    bytes[48..52].copy_from_slice(&(second.len() as u32).to_le_bytes());
    bytes.extend_from_slice(first.as_bytes());
    bytes.extend_from_slice(second.as_bytes());
    Ok(FunctionInput { mode, bytes })
}

// Internal-only allowlist: DataModel.GetFastFlag for this one package notice.
// The public `input` entry point above continues to reject DataModel calls.
#[cfg(windows)]
pub(super) fn package_flag_input() -> FunctionInput {
    let flag = b"RemovePackageModificationPopupDialog";
    let mode = 4u32;
    let mut bytes = vec![0; 56];
    bytes[20..24].copy_from_slice(&mode.to_le_bytes());
    bytes[44..48].copy_from_slice(&(flag.len() as u32).to_le_bytes());
    bytes.extend_from_slice(flag);
    FunctionInput { mode, bytes }
}

// Internal-only as well: DataModel.HttpPostAsync, the one engine request
// that takes its content type as text, for the uploads Renium makes under the
// Studio login. The body travels beside the request.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn post_input(url: &str, content_type: &str) -> Result<FunctionInput> {
    let url = roblox_url(url)?;
    ensure!(
        !content_type.is_empty()
            && content_type.len() <= 256
            && content_type.bytes().all(|byte| (b' '..127).contains(&byte)),
        "http-content-type detector: expected printable ASCII"
    );
    ensure!(url.len() <= 8 * 1024, "The address exceeds 8 KiB");
    let mode = 5u32;
    let mut bytes = vec![0; 56];
    bytes[20..24].copy_from_slice(&mode.to_le_bytes());
    bytes[44..48].copy_from_slice(&(url.len() as u32).to_le_bytes());
    bytes[48..52].copy_from_slice(&(content_type.len() as u32).to_le_bytes());
    bytes.extend_from_slice(url.as_bytes());
    bytes.extend_from_slice(content_type.as_bytes());
    Ok(FunctionInput { mode, bytes })
}

#[cfg(windows)]
pub(super) fn gamepad_connection_input(connect: bool, device_id: i32) -> FunctionInput {
    let mode = if connect { 6u32 } else { 7u32 };
    let mut bytes = vec![0; 56];
    bytes[20..24].copy_from_slice(&mode.to_le_bytes());
    bytes[32..36].copy_from_slice(&(device_id as u32).to_le_bytes());
    FunctionInput { mode, bytes }
}

#[cfg(windows)]
pub(super) fn gamepad_button_input(device_id: i32, key_code: i32, pressed: bool) -> FunctionInput {
    let mode = 8u32;
    let mut bytes = vec![0; 56];
    bytes[20..24].copy_from_slice(&mode.to_le_bytes());
    bytes[32..36].copy_from_slice(&(device_id as u32).to_le_bytes());
    bytes[36..40].copy_from_slice(&(key_code as u32).to_le_bytes());
    bytes[40..44].copy_from_slice(&u32::from(pressed).to_le_bytes());
    FunctionInput { mode, bytes }
}

#[cfg(windows)]
pub(super) fn gamepad_axis_input(
    device_id: i32,
    key_code: i32,
    x: f32,
    y: f32,
    z: f32,
) -> FunctionInput {
    let mode = 9u32;
    let mut bytes = vec![0; 56];
    bytes[20..24].copy_from_slice(&mode.to_le_bytes());
    bytes[32..36].copy_from_slice(&(device_id as u32).to_le_bytes());
    bytes[36..40].copy_from_slice(&(key_code as u32).to_le_bytes());
    bytes[44..48].copy_from_slice(&12u32.to_le_bytes());
    bytes.extend_from_slice(&x.to_le_bytes());
    bytes.extend_from_slice(&y.to_le_bytes());
    bytes.extend_from_slice(&z.to_le_bytes());
    FunctionInput { mode, bytes }
}

#[derive(Debug)]
pub(crate) struct FunctionFailure {
    pub code: u32,
    pub sent: Option<bool>,
    pub message: String,
}
impl std::fmt::Display for FunctionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sent = match self.sent {
            Some(true) => "true",
            Some(false) => "false",
            None => "unknown",
        };
        write!(
            f,
            "Function call failed (0x{:X}; requestSent={sent}): {}",
            self.code, self.message
        )
    }
}
impl std::error::Error for FunctionFailure {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn full_url_sends_only_the_validated_normalized_address() {
        for name in ["GetAsyncFullUrl", "PostAsyncFullUrl"] {
            let arguments = |raw: &str| {
                if name.starts_with("Post") {
                    vec![json!(raw), json!("")]
                } else {
                    vec![json!(raw)]
                }
            };
            for raw in [
                "https://www.roblox.com\\@other.example/",
                "https://www.roblox.com\\x",
                "https://www.roblox.com/@x",
                "https://www.roblox.com/\t",
                "https://www.roblox.com/\n",
                "https://www.roblox.com/\0",
                "https://www.roblox.com/a b",
                "https://www.roblox.com/\u{a0}",
            ] {
                assert!(
                    input("HttpRbxApiService", name, &arguments(raw)).is_err(),
                    "{name}: {raw:?}"
                );
            }
            let request = input(
                "HttpRbxApiService",
                name,
                &arguments("HTTPS://WWW.ROBLOX.COM:443/a/../b"),
            )
            .unwrap();
            assert_eq!(&request.bytes[56..], b"https://www.roblox.com/b");
            assert_eq!(
                u32::from_le_bytes(request.bytes[44..48].try_into().unwrap()) as usize,
                request.bytes.len() - 56
            );
        }
    }

    #[test]
    fn a_post_takes_a_roblox_address_and_a_plain_content_type() {
        let multipart = "multipart/form-data; boundary=renium-1f";
        for (url, content_type) in [
            ("https://apis.roblox.com.example.test/x", multipart),
            ("http://apis.roblox.com/x", multipart),
            ("https://apis.roblox.com/x", ""),
            ("https://apis.roblox.com/x", "text/plain\r\nCookie: a=b"),
            ("https://apis.roblox.com/x", "text/pl\u{e4}in"),
        ] {
            assert!(
                post_input(url, content_type).is_err(),
                "{url} {content_type:?}"
            );
        }
        assert!(post_input("https://apis.roblox.com/x", &"a".repeat(256)).is_ok());
        assert!(post_input("https://apis.roblox.com/x", &"a".repeat(257)).is_err());
        let long = format!("https://apis.roblox.com/{}", "a".repeat(8 * 1024));
        assert!(post_input(&long, multipart).is_err());
        let request = post_input("HTTPS://APIS.ROBLOX.COM:443/assets/../x", multipart).unwrap();
        assert_eq!(request.mode, 5);
        assert_eq!(
            &request.bytes[56..],
            format!("https://apis.roblox.com/x{multipart}").as_bytes()
        );
        assert_eq!(request.bytes[32..44], [0; 12]);
        let declared = |items: Value| request.validate_enums(&json!({ "HttpRequestType": items }));
        declared(json!([0, 2])).unwrap();
        assert!(declared(json!([2])).is_err());
        assert!(
            request
                .validate_call_contract("HttpPostAsync", "Studio", 1, 16)
                .is_err()
        );
    }

    #[test]
    fn relative_paths_cannot_replace_the_engine_host() {
        for name in ["GetAsync", "PostAsync"] {
            for path in [
                "https://apis.roblox.com/x",
                "//evil.test/x",
                "/\\evil.test",
                " /x",
                "/x\n",
                "/x#fragment",
                "x",
            ] {
                let args = if name == "PostAsync" {
                    vec![json!(path), json!("")]
                } else {
                    vec![json!(path)]
                };
                assert!(input("HttpRbxApiService", name, &args).is_err(), "{path:?}");
            }
        }
        input(
            "HttpRbxApiService",
            "GetAsync",
            &[json!("/v1/items?q=a%20b")],
        )
        .unwrap();
    }

    #[test]
    fn relative_calls_need_a_roblox_base_address() {
        for name in ["GetAsync", "PostAsync"] {
            for base in [
                json!(null),
                json!(""),
                json!("http://www.roblox.com/"),
                json!("https://www.roblox.com.example.test/"),
                json!("https://example.test/"),
                json!("https://www.roblox.com:8443/"),
            ] {
                let error = validate_api_base(name, &json!({ "BaseUrl": base })).unwrap_err();
                assert!(error.to_string().contains("http-relative-base detector"));
            }
            validate_api_base(name, &json!({"BaseUrl":"https://www.roblox.com/"})).unwrap();
        }
        validate_api_base("GetAsyncFullUrl", &json!({})).unwrap();
        // The check every call path runs before a request is prepared.
        let items = json!({"ThrottlingPriority":[0],"HttpContentType":[0],"HttpRequestType":[0]});
        let error = super::super::validate_function_arguments(
            "HttpRbxApiService",
            "GetAsync",
            &[json!("/x")],
            &items,
        )
        .unwrap_err();
        assert!(error.to_string().contains("http-relative-base detector"));
    }

    // The request the daemon encodes is the request the helper reads: the
    // proved layout, the proved call sites and both arguments.
    #[cfg(windows)]
    #[test]
    fn helper_reads_the_yield_request_the_daemon_encodes() {
        let helper = |arguments: &[&str]| {
            let output = crate::system::tools::hidden_command(concat!(
                env!("OUT_DIR"),
                "/renium-functions-test.exe"
            ))
            .args(arguments)
            .output()
            .expect("run the native function test");
            assert!(output.status.success(), "{arguments:?}");
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        let hex = |request: &FunctionInput| {
            request
                .bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        let request = || {
            input(
                "HttpRbxApiService",
                "PostAsyncFullUrl",
                &[json!("https://apis.roblox.com/x"), json!("{}")],
            )
            .unwrap()
        };
        let layout: u32 = helper(&["--contract"]).parse().unwrap();
        assert_ne!(layout, 0);
        assert_eq!(helper(&["--decode", &hex(&request())]), "refused");
        let mut proved = request();
        proved
            .qualify_yield(|| {
                Ok(YieldContract {
                    layout,
                    sites: vec![[0x1111, 0x2222], [0x3333, 0x4444]],
                })
            })
            .unwrap();
        assert_eq!(
            helper(&["--decode", &hex(&proved)]),
            "mode=2 first=https://apis.roblox.com/x second={} sites=1111:2222,3333:4444,"
        );
        let mut other = request();
        other
            .qualify_yield(|| {
                Ok(YieldContract {
                    layout: layout ^ 4,
                    sites: vec![[0x1111, 0x2222]],
                })
            })
            .unwrap();
        assert_eq!(helper(&["--decode", &hex(&other)]), "refused");
    }

    #[test]
    fn enum_arguments_use_engine_items_including_omitted_defaults() {
        let request = input(
            "HttpRbxApiService",
            "PostAsync",
            &[json!("/x"), json!(""), json!(17), json!(23), json!(41)],
        )
        .unwrap();
        let mut items =
            json!({"ThrottlingPriority":[17],"HttpContentType":[23],"HttpRequestType":[41]});
        request.validate_enums(&items).unwrap();
        for name in ["ThrottlingPriority", "HttpContentType", "HttpRequestType"] {
            let saved = items[name].take();
            assert!(
                request
                    .validate_enums(&items)
                    .unwrap_err()
                    .to_string()
                    .contains(name)
            );
            items[name] = saved;
        }
        items["HttpRequestType"] = json!([42]);
        assert!(request.validate_enums(&items).is_err());
        let defaults = input("HttpRbxApiService", "GetAsync", &[json!("/x")]).unwrap();
        assert!(defaults.validate_enums(&items).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn native_yield_callbacks_return_errors_and_retire_after_timeout() {
        assert!(
            crate::system::tools::hidden_command(concat!(
                env!("OUT_DIR"),
                "/renium-functions-test.exe"
            ))
            .arg("--yield")
            .status()
            .expect("isolated yield ABI regression")
            .success()
        );
    }

    #[test]
    fn macos_yield_calls_refuse_without_a_continuation_adapter() {
        for name in [
            "GetAsync",
            "GetAsyncFullUrl",
            "PostAsync",
            "PostAsyncFullUrl",
        ] {
            let args = if name.starts_with("Post") {
                vec![
                    json!(if name.ends_with("FullUrl") {
                        "https://apis.roblox.com/x"
                    } else {
                        "/x"
                    }),
                    json!("{}"),
                ]
            } else {
                vec![json!(if name.ends_with("FullUrl") {
                    "https://apis.roblox.com/x"
                } else {
                    "/x"
                })]
            };
            let input = input("HttpRbxApiService", name, &args).unwrap();
            for (entry, field) in [(0x1234, 16), (0x87654321, 768)] {
                let error = input
                    .validate_call_contract(name, "Studio", entry, field)
                    .unwrap_err()
                    .to_string();
                assert!(error.contains("yield-call-contract"));
                assert!(error.contains(name));
                assert!(error.contains(&format!("Studio+0x{entry:X}")));
                assert!(error.contains(&format!("descriptor-field=0x{field:X}")));
                assert!(error.contains("no call executed"));
            }
        }
        input(
            "HttpRbxApiService",
            "GetDocumentationUrl",
            &[json!("reference/engine")],
        )
        .unwrap()
        .validate_call_contract("GetDocumentationUrl", "Studio", 1234, 16)
        .unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn native_string_returns_preserve_receiver_across_a_batch() {
        assert!(
            crate::system::tools::hidden_command(concat!(
                env!("OUT_DIR"),
                "/renium-functions-test.exe"
            ))
            .status()
            .expect("run native function ABI regression")
            .success()
        );
    }

    #[test]
    fn function_arguments_preserve_types_and_restrict_authenticated_destinations() {
        assert!(
            input(
                "DataModel",
                "GetFastFlag",
                &[json!("RemovePackageModificationPopupDialog")]
            )
            .is_err()
        );
        for url in [
            "https://roblox.com.evil.test/x",
            "http://apis.roblox.com/x",
            "https://user@apis.roblox.com/x",
            "https://apis.roblox.com:8443/x",
        ] {
            assert!(input("HttpRbxApiService", "GetAsyncFullUrl", &[json!(url)]).is_err());
        }
        assert!(
            input(
                "HttpRbxApiService",
                "GetAsyncFullUrl",
                &[json!("https://apis.roblox.com/x"), json!(-1)]
            )
            .is_err()
        );
        assert!(
            input(
                "HttpRbxApiService",
                "GetAsyncFullUrl",
                &[json!("https://apis.roblox.com/x"), json!("0")]
            )
            .is_err()
        );
        assert!(
            input(
                "HttpRbxApiService",
                "PostAsyncFullUrl",
                &[json!("https://apis.roblox.com/x"), json!({})]
            )
            .is_err()
        );
        let payload = input(
            "HttpRbxApiService",
            "PostAsyncFullUrl",
            &[
                json!("https://apis.roblox.com/x"),
                json!("{}"),
                json!(0),
                json!(0),
                json!(1),
            ],
        )
        .unwrap();
        assert_eq!(payload.mode, 2);
        assert_eq!(
            u32::from_le_bytes(payload.bytes[40..44].try_into().unwrap()),
            1
        );
    }
}
