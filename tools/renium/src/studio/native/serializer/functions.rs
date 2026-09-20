use anyhow::{Context, Result, ensure};
use serde_json::Value;

pub(super) struct FunctionInput {
    pub mode: u32,
    pub bytes: Vec<u8>,
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
    if name.ends_with("FullUrl") {
        let url = url::Url::parse(first).context("Expected an HTTPS Roblox URL")?;
        ensure!(
            url.scheme() == "https"
                && url.username().is_empty()
                && url.password().is_none()
                && url
                    .host_str()
                    .is_some_and(|host| host == "roblox.com" || host.ends_with(".roblox.com"))
                && url.port().is_none_or(|port| port == 443),
            "Authenticated Studio requests require an HTTPS Roblox URL"
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(windows)]
    #[test]
    fn native_string_returns_preserve_receiver_across_a_batch() {
        assert!(
            std::process::Command::new(concat!(env!("OUT_DIR"), "/renium-functions-test.exe"))
                .status()
                .expect("run native function ABI regression")
                .success()
        );
    }

    #[test]
    fn function_arguments_preserve_types_and_restrict_authenticated_destinations() {
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
