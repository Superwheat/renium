use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::op;
use crate::app::timing::current_millis;
use crate::cli::AutomationArgs;
use crate::daemon::daemon_control_endpoints;
use crate::daemon::transport::{
    BoundedLineRead, DAEMON_CONTROL_CONNECT_TIMEOUT, DAEMON_CONTROL_IDLE_TIMEOUT,
    DAEMON_CONTROL_RESPONSE_TIMEOUT, MAX_DAEMON_LINE_BYTES, read_bounded_line,
};
use crate::system::files::absolutize_for_daemon;

fn transport_failure(id: u64, error: anyhow::Error) -> super::Response {
    super::Response::failure(
        id,
        Instant::now(),
        super::Failure::new("bridge_off", format!("{error:#}"), true, "bind"),
    )
}

pub(crate) fn automation_command(args: AutomationArgs) {
    let operation = match super::opcode_by_name(args.operation.trim()) {
        Ok(operation) => operation,
        Err(error) => cli_failure("bad_op", format!("{error:#}"), "cap"),
    };
    let (cx, parameters) = match cli_parameters(operation, &args.args) {
        Ok(parsed) => parsed,
        Err(error) => cli_failure("bad_req", format!("{error:#}"), operation.name),
    };
    let request = super::Request {
        v: super::PROTOCOL_VERSION,
        id: current_millis().min(u128::from(u64::MAX)) as u64,
        op: operation.id,
        cx,
        p: parameters,
    };
    let reviewed = operation.review
        && request.cx.is_some()
        && (matches!(operation.id, op::STUDIO_OPEN | op::STUDIO_CLOSE)
            || request.p.get("destructive").and_then(Value::as_bool) == Some(true));
    let mut response = if reviewed {
        send_reviewed_request(&request)
    } else {
        send_request(&request)
    }
    .unwrap_or_else(|error| transport_failure(request.id, error));
    if !reviewed
        && operation.review
        && response
            .e
            .as_ref()
            .is_some_and(|error| error.c == "rejected" && error.n == "review-prepare")
    {
        response = send_reviewed_request(&request)
            .unwrap_or_else(|error| transport_failure(request.id, error));
    }
    print_response(&response);
    if response.ok == 0 {
        std::process::exit(1);
    }
}

fn send_reviewed_request(request: &super::Request) -> Result<super::Response> {
    let prepared = send_request(&super::Request {
        v: super::PROTOCOL_VERSION,
        id: request.id,
        op: op::REVIEW_PREPARE,
        cx: request.cx,
        p: json!({ "op": request.op, "p": request.p }),
    })?;
    if prepared.ok == 0 {
        return Ok(prepared);
    }
    let review_id = prepared
        .r
        .as_ref()
        .and_then(|value| value.get("reviewId"))
        .and_then(Value::as_str)
        .context("review-prepare did not return reviewId")?;
    send_request(&super::Request {
        v: super::PROTOCOL_VERSION,
        id: request.id,
        op: op::REVIEW_APPLY,
        cx: request.cx,
        p: json!({ "reviewId": review_id }),
    })
}

fn cli_failure(code: &str, message: String, next: &str) -> ! {
    let response = super::Response::failure(
        0,
        Instant::now(),
        super::Failure::new(code, message, false, next),
    );
    print_response(&response);
    std::process::exit(1);
}

fn print_response(response: &super::Response) {
    std::println!(
        "{}",
        serde_json::to_string(response)
            .unwrap_or_else(|_| "{\"v\":1,\"id\":0,\"ok\":0}".to_string())
    );
}

fn read_json(source: &str) -> Result<Value> {
    let text = if source == "-" {
        let mut text = String::new();
        io::stdin().read_to_string(&mut text)?;
        text
    } else {
        fs::read_to_string(source).with_context(|| format!("Failed to read {source}"))?
    };
    serde_json::from_str(&text).with_context(|| format!("Invalid JSON in {source}"))
}

fn normalize_bind(mut payload: Value) -> Result<Value> {
    let object = payload
        .as_object_mut()
        .context("bind payload must be a JSON object")?;
    let root = object.get("root").and_then(Value::as_str).unwrap_or(".");
    object.insert(
        "root".to_string(),
        Value::String(absolutize_for_daemon(Path::new(root)).display().to_string()),
    );
    Ok(payload)
}

fn direct_place_add(args: &[String]) -> Result<Value> {
    if args.len() < 2 {
        bail!(
            "Expected: rbx a place-add CX PLACE_ID NAME [--game-id ID] [--alias NAME] [--root PATH]"
        );
    }
    let place_id = args[0]
        .parse::<i64>()
        .context("PLACE_ID must be an integer for place-add")?;
    let mut payload = json!({ "placeId": place_id, "name": args[1] });
    let object = payload.as_object_mut().context("place-add payload")?;
    let mut index = 2;
    while index < args.len() {
        let (key, integer) = match args[index].as_str() {
            "--game" | "--game-id" => ("gameId", true),
            "--alias" => ("alias", false),
            "--root" => ("root", false),
            _ => bail!("Unknown place-add option {}", args[index]),
        };
        let value = args
            .get(index + 1)
            .with_context(|| format!("Expected a value after {} for place-add", args[index]))?;
        object.insert(
            key.to_string(),
            if integer {
                json!(
                    value
                        .parse::<i64>()
                        .context("GAME_ID must be an integer for place-add")?
                )
            } else {
                Value::String(value.clone())
            },
        );
        index += 2;
    }
    Ok(payload)
}

fn cli_parameters(operation: &super::Opcode, args: &[String]) -> Result<(Option<u64>, Value)> {
    if operation.id == op::CAP {
        if !args.is_empty() {
            bail!("Expected: rbx a cap");
        }
        return Ok((None, json!({})));
    }
    if operation.id == op::BIND {
        if args
            .first()
            .is_some_and(|value| value == "-J" || value == "--json-file")
        {
            if args.len() != 2 {
                bail!("Expected: rbx a bind -J FILE");
            }
            return Ok((None, normalize_bind(read_json(&args[1])?)?));
        }
        let bootstrap = args.iter().any(|value| value == "--bootstrap");
        let positional = args
            .iter()
            .filter(|value| value.as_str() != "--bootstrap")
            .collect::<Vec<_>>();
        if positional.len() > 2 {
            bail!("Expected: rbx a bind [project] [place] [--bootstrap]");
        }
        return Ok((
            None,
            normalize_bind(json!({
                "root": positional.first().map_or(".", |value| value.as_str()),
                "place": positional.get(1),
                "bootstrap": bootstrap,
            }))?,
        ));
    }
    if operation.id == op::STUDIOS {
        let payload = match args {
            [] => json!({}),
            [flag, source] if flag == "-J" || flag == "--json-file" => read_json(source)?,
            _ => bail!("Expected: rbx a studios [-J FILE]"),
        };
        if !payload.is_object() {
            bail!("studios payload must be a JSON object");
        }
        return Ok((None, payload));
    }
    let cx = args
        .first()
        .with_context(|| format!("Expected: rbx a {} CX", operation.name))?
        .parse::<u64>()
        .with_context(|| format!("Context ID must be an integer for {}", operation.name))?;
    let remaining = &args[1..];
    if remaining.is_empty() {
        return Ok((
            Some(cx),
            if matches!(operation.id, op::PULL | op::PUSH) {
                json!({ "destructive": true })
            } else {
                json!({})
            },
        ));
    }
    if operation.id == op::PLACE_ADD
        && remaining
            .first()
            .is_some_and(|value| value != "-J" && value != "--json-file")
    {
        return Ok((Some(cx), direct_place_add(remaining)?));
    }
    if operation.id == op::PLACE_RENAME
        && remaining
            .first()
            .is_some_and(|value| value != "-J" && value != "--json-file")
    {
        let [place_id, alias] = remaining else {
            bail!("Expected: rbx a place-rename CX PLACE_ID ALIAS");
        };
        return Ok((
            Some(cx),
            json!({
                "placeId": place_id
                    .parse::<i64>()
                    .context("PLACE_ID must be an integer for place-rename")?,
                "alias": alias,
            }),
        ));
    }
    if operation.id == op::PLACE_REORDER
        && remaining
            .first()
            .is_some_and(|value| value != "-J" && value != "--json-file")
    {
        let order = remaining
            .iter()
            .map(|value| {
                value
                    .parse::<i64>()
                    .context("PLACE_ID must be an integer for place-reorder")
            })
            .collect::<Result<Vec<_>>>()?;
        return Ok((Some(cx), json!({ "order": order })));
    }
    let (service, source) = match remaining {
        [flag, source] if flag == "-J" || flag == "--json-file" => (None, source),
        [service, flag, source]
            if operation.id == op::BATCH && (flag == "-J" || flag == "--json-file") =>
        {
            (Some(service), source)
        }
        _ if operation.id == op::BATCH => bail!("Expected: rbx a bb CX [SERVICE] -J FILE"),
        _ => bail!("Expected: rbx a {} CX -J FILE", operation.name),
    };
    let mut payload = match read_json(source)? {
        Value::Object(object) => Value::Object(object),
        Value::Array(values) if operation.id == op::BATCH => json!({ "ops": values }),
        _ => bail!("{} payload must be a JSON object", operation.name),
    };
    if let Some(service) = service {
        payload
            .as_object_mut()
            .context("Batch payload must be an object")?
            .insert("service".to_string(), Value::String(service.clone()));
    }
    if matches!(operation.id, op::PULL | op::PUSH) {
        payload
            .as_object_mut()
            .context("Sync payload must be an object")?
            .insert("destructive".to_string(), Value::Bool(true));
    }
    Ok((Some(cx), payload))
}

pub(crate) fn send_request(request: &super::Request) -> Result<super::Response> {
    if let Some(response) = try_send_request(request)? {
        return Ok(response);
    }
    if request.op == op::BIND {
        crate::daemon::start_shared_daemon("8781,8782", 1.0);
    }
    try_send_request(request)?.context("Renium daemon is not running")
}

pub(crate) fn try_send_request(request: &super::Request) -> Result<Option<super::Response>> {
    let Some(stream) = daemon_control_endpoints().into_iter().find_map(|address| {
        TcpStream::connect_timeout(&address, DAEMON_CONTROL_CONNECT_TIMEOUT).ok()
    }) else {
        return Ok(None);
    };
    send_on_stream(stream, request).map(Some)
}

fn send_on_stream(mut stream: TcpStream, request: &super::Request) -> Result<super::Response> {
    send_on_stream_with_timeout(&mut stream, request, DAEMON_CONTROL_RESPONSE_TIMEOUT)
}

fn send_on_stream_with_timeout(
    stream: &mut TcpStream,
    request: &super::Request,
    timeout: Duration,
) -> Result<super::Response> {
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(DAEMON_CONTROL_IDLE_TIMEOUT));
    writeln!(stream, "{}", serde_json::to_string(request)?)?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    match read_bounded_line(&mut reader, &mut line, MAX_DAEMON_LINE_BYTES)? {
        BoundedLineRead::Line => {}
        BoundedLineRead::Eof => bail!("Renium daemon closed the connection before responding"),
        BoundedLineRead::TooLong => bail!("Renium daemon response exceeded the protocol limit"),
    }
    serde_json::from_str(line.trim()).context("Invalid Renium daemon response")
}

pub(crate) fn shared_daemon_available() -> bool {
    let request = super::Request {
        v: super::PROTOCOL_VERSION,
        id: current_millis().min(u128::from(u64::MAX)) as u64,
        op: op::CAP,
        cx: None,
        p: json!({}),
    };
    daemon_control_endpoints().into_iter().any(|address| {
        let Ok(mut stream) = TcpStream::connect_timeout(&address, DAEMON_CONTROL_CONNECT_TIMEOUT)
        else {
            return false;
        };
        send_on_stream_with_timeout(&mut stream, &request, Duration::from_millis(500))
            .is_ok_and(|response| response.ok == 1)
    })
}

pub(crate) fn run_stdio_proxy() -> Result<()> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();
    loop {
        match read_bounded_line(&mut reader, &mut line, MAX_DAEMON_LINE_BYTES)? {
            BoundedLineRead::Eof => break,
            BoundedLineRead::Line => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let response = match serde_json::from_str::<super::Request>(trimmed) {
                    Ok(request) => try_send_request(&request)?.unwrap_or_else(|| {
                        transport_failure(
                            request.id,
                            anyhow::anyhow!("Renium daemon is not running"),
                        )
                    }),
                    Err(error) => super::Response::failure(
                        0,
                        Instant::now(),
                        super::Failure::new(
                            "bad_req",
                            format!("Invalid request JSON: {error}"),
                            false,
                            "cap",
                        ),
                    ),
                };
                std::println!("{}", serde_json::to_string(&response)?);
                io::stdout().flush()?;
            }
            BoundedLineRead::TooLong => {
                let response = super::runtime::oversized_automation_request_response();
                std::println!("{}", serde_json::to_string(&response)?);
                io::stdout().flush()?;
            }
        }
    }
    Ok(())
}
