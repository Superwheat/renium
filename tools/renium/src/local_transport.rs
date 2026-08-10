use std::io::{self, BufRead, Read};
use std::net::{IpAddr, SocketAddr};
#[cfg(target_os = "macos")]
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub(super) const DEFAULT_DAEMON_CONTROL_PORT: u16 = 8780;
pub(super) const MAX_DAEMON_LINE_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_DAEMON_CONTROL_CONNECTIONS: usize = 16;
pub(super) const DAEMON_CONTROL_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
pub(super) const DAEMON_CONTROL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const DAEMON_CONTROL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub(super) const DAEMON_CONTROL_QUEUE_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const DAEMON_DISCOVERY_MAX_AGE_MS: u128 = 7 * 24 * 60 * 60 * 1000;
pub(super) const DAEMON_DISCOVERY_MAX_FUTURE_SKEW_MS: u128 = 5 * 60 * 1000;

#[derive(Debug, PartialEq)]
pub(super) enum BoundedLineRead {
    Eof,
    Line,
    TooLong,
}

pub(super) fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    output: &mut String,
    max_bytes: usize,
) -> io::Result<BoundedLineRead> {
    output.clear();
    let mut bytes = Vec::with_capacity(8192.min(max_bytes.saturating_add(1)));
    let read = {
        let mut limited = reader.take(max_bytes.saturating_add(1) as u64);
        limited.read_until(b'\n', &mut bytes)?
    };
    if read == 0 {
        return Ok(BoundedLineRead::Eof);
    }

    let has_newline = bytes.last() == Some(&b'\n');
    let content_len = bytes.len().saturating_sub(usize::from(has_newline));
    if content_len > max_bytes {
        loop {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                break;
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |index| index + 1);
            reader.consume(consumed);
            if newline.is_some() {
                break;
            }
        }
        return Ok(BoundedLineRead::TooLong);
    }

    *output = String::from_utf8(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(BoundedLineRead::Line)
}

pub(super) fn normalize_loopback_host(host: &str) -> Result<String> {
    let raw = host.trim();
    let normalized = if raw.is_empty() || raw.eq_ignore_ascii_case("localhost") {
        "127.0.0.1".to_string()
    } else {
        raw.strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .unwrap_or(raw)
            .to_string()
    };
    let address: IpAddr = normalized
        .parse()
        .with_context(|| format!("Bridge host must be a loopback IP address, got {raw:?}"))?;
    if !address.is_loopback() {
        bail!("Renium only permits loopback bridge hosts; refusing {raw:?}");
    }
    Ok(address.to_string())
}

pub(super) fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

pub(super) fn is_loopback_endpoint(address: &SocketAddr) -> bool {
    address.ip().is_loopback()
}

#[cfg(windows)]
fn windows_local_tcp_connections(table_class: i32) -> Result<Vec<(u16, u32)>> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCPTABLE_OWNER_PID,
    };
    use windows_sys::Win32::Networking::WinSock::AF_INET;

    unsafe {
        let mut size = 0;
        GetExtendedTcpTable(
            std::ptr::null_mut(),
            &raw mut size,
            0,
            AF_INET as u32,
            table_class,
            0,
        );
        if size == 0 {
            bail!("GetExtendedTcpTable returned no table size");
        }
        let mut buffer = vec![0u8; size as usize];
        let status = GetExtendedTcpTable(
            buffer.as_mut_ptr().cast(),
            &raw mut size,
            0,
            AF_INET as u32,
            table_class,
            0,
        );
        if status != 0 {
            bail!("GetExtendedTcpTable failed with status {status}");
        }
        let table = &*buffer.as_ptr().cast::<MIB_TCPTABLE_OWNER_PID>();
        let rows = std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize);
        Ok(rows
            .iter()
            .map(|row| {
                (
                    u16::from_be((row.dwLocalPort & 0xFFFF) as u16),
                    row.dwOwningPid,
                )
            })
            .collect())
    }
}

#[cfg(windows)]
pub(super) fn pid_for_local_tcp_port(port: u16) -> Result<u32> {
    use windows_sys::Win32::NetworkManagement::IpHelper::TCP_TABLE_OWNER_PID_ALL;

    let mut pids = windows_local_tcp_connections(TCP_TABLE_OWNER_PID_ALL)?
        .into_iter()
        .filter_map(|(candidate, pid)| (candidate == port).then_some(pid))
        .collect::<Vec<_>>();
    pids.sort_unstable();
    pids.dedup();
    match pids.as_slice() {
        [pid] => Ok(*pid),
        [] => bail!("No TCP connection with local port {port} found"),
        _ => bail!("Local TCP port {port} belongs to multiple processes"),
    }
}

#[cfg(target_os = "macos")]
pub(super) fn pid_for_local_tcp_port(port: u16) -> Result<u32> {
    let output = Command::new("lsof")
        .args([
            "-nP",
            "-a",
            &format!("-iTCP:{port}"),
            "-sTCP:ESTABLISHED",
            "-Fpc",
        ])
        .output()
        .context("Failed to run lsof while mapping the Studio bridge")?;
    if !output.status.success() && output.status.code() != Some(1) {
        bail!("lsof failed while mapping the Studio bridge");
    }
    let mut current_pid = None;
    let mut matches = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(value) = line.strip_prefix('p') {
            current_pid = value.parse::<u32>().ok();
        } else if let Some(command) = line.strip_prefix('c')
            && (command.contains("RobloxStudio") || command.contains("ReniumStudio"))
            && let Some(pid) = current_pid
        {
            matches.push(pid);
        }
    }
    matches.sort_unstable();
    matches.dedup();
    match matches.as_slice() {
        [pid] => Ok(*pid),
        _ => bail!(
            "Bridge port {port} mapped to {} Studio processes",
            matches.len()
        ),
    }
}

#[cfg(windows)]
pub(super) fn local_tcp_ports_owned_by_pid(pid: u32) -> Vec<u16> {
    use windows_sys::Win32::NetworkManagement::IpHelper::TCP_TABLE_OWNER_PID_CONNECTIONS;

    let Ok(connections) = windows_local_tcp_connections(TCP_TABLE_OWNER_PID_CONNECTIONS) else {
        return Vec::new();
    };
    connections
        .into_iter()
        .filter_map(|(port, owner)| (owner == pid).then_some(port))
        .collect()
}

#[cfg(target_os = "macos")]
pub(super) fn local_tcp_ports_owned_by_pid(pid: u32) -> Vec<u16> {
    let Ok(output) = Command::new("lsof")
        .args(["-nP", "-a", "-p"])
        .arg(pid.to_string())
        .args(["-iTCP", "-F", "n"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let mut ports = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('n'))
        .filter_map(|endpoint| endpoint.split("->").next())
        .filter_map(|endpoint| endpoint.rsplit(':').next())
        .filter_map(|port| port.parse::<u16>().ok())
        .collect::<Vec<_>>();
    ports.sort_unstable();
    ports.dedup();
    ports
}
