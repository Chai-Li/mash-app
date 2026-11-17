//! Manual local/LAN connections. Arguments are passed directly to ADB, never a shell.

use crate::{adb, platform};
use std::net::Ipv6Addr;
use std::path::Path;
use std::time::{Duration, Instant};

pub(super) fn endpoint(host: &str, port: &str) -> Result<String, String> {
    let host = host.trim();
    let port = port.trim();
    if host.is_empty() {
        return Err("请输入设备 IP 地址或主机名".into());
    }
    let port = if !port.is_empty() && port.bytes().all(|c| c.is_ascii_digit()) {
        port.parse::<u16>().ok().filter(|port| *port != 0)
    } else {
        None
    }
    .ok_or("请输入 1-65535 之间的端口号")?;

    // Brackets disambiguate the port from IPv6 colons. Preserve zone IDs for
    // link-local addresses (for example fe80::1234%12 on Windows).
    let unbracketed = host.strip_prefix('[').and_then(|v| v.strip_suffix(']'));
    let ipv6 = unbracketed.unwrap_or(host);
    if host.contains(':') || unbracketed.is_some() {
        let (address, zone) = ipv6
            .split_once('%')
            .map_or((ipv6, None), |(a, z)| (a, Some(z)));
        if address.parse::<Ipv6Addr>().is_err()
            || zone.is_some_and(|zone| {
                zone.is_empty()
                    || !zone
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
            })
        {
            return Err("IP 地址格式无效；端口号请填在单独的端口字段中".into());
        }
        return Ok(format!("[{ipv6}]:{port}"));
    }
    if host.starts_with('-')
        || host.chars().any(|c| {
            c.is_whitespace()
                || c.is_control()
                || matches!(c, '/' | '\\' | '[' | ']' | '?' | '#' | '@')
        })
    {
        return Err("请输入 IP 地址或主机名，不要包含协议、路径或端口".into());
    }
    Ok(format!("{host}:{port}"))
}

struct AdbReply {
    success: bool,
    stdout: String,
    stderr: String,
}

fn run(adb_path: &Path, args: &[&str], timeout: Duration) -> Result<AdbReply, String> {
    let output = platform::output_with_timeout(adb::adb_command(adb_path).args(args), timeout)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::TimedOut {
                "ADB 连接超时，请检查设备地址、端口及网络后重试".to_string()
            } else {
                format!("无法运行 ADB: {error}")
            }
        })?;
    Ok(AdbReply {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().into(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().into(),
    })
}

pub(super) fn connect(adb_path: &Path, serial: &str) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    connect_with(serial, |args| {
        let remaining = deadline.saturating_duration_since(Instant::now());
        run(adb_path, args, remaining)
    })
}

fn connect_with(
    serial: &str,
    mut run_adb: impl FnMut(&[&str]) -> Result<AdbReply, String>,
) -> Result<String, String> {
    let reply = run_adb(&["connect", serial])?;
    let details = [reply.stdout.as_str(), reply.stderr.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let failed = details.lines().any(|line| {
        let line = line.trim().to_ascii_lowercase();
        [
            "failed",
            "unable",
            "cannot",
            "could not",
            "error:",
            "connection refused",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
    });
    if !reply.success || failed {
        return Err(format!("ADB 连接 {serial} 失败: {details}"));
    }
    if reply.stdout.lines().any(|line| {
        let line = line.trim().to_ascii_lowercase();
        line.starts_with("connected to ")
    }) {
        return Ok(serial.into());
    }

    // ADB versions may use different success messages. Confirm known device
    // states when possible, without treating an unavailable auxiliary query as
    // proof that the connect operation failed. The device picker refreshes next.
    match run_adb(&["-s", serial, "get-state"]) {
        Ok(state) if unavailable_device_state(&state).is_some() => {
            return Err(format!(
                "ADB 设备 {serial} 未就绪（{}），请检查设备上的调试授权后重试",
                unavailable_device_state(&state).unwrap()
            ));
        }
        Ok(state) if state.success && state.stdout.trim() == "device" => {}
        Ok(state) => eprintln!(
            "[adb] connection state unavailable for {serial}: {} {}",
            state.stdout, state.stderr
        ),
        Err(error) => eprintln!("[adb] connection state unavailable for {serial}: {error}"),
    }
    Ok(serial.into())
}

fn unavailable_device_state(reply: &AdbReply) -> Option<&'static str> {
    for message in [&reply.stdout, &reply.stderr] {
        let message = message.trim().to_ascii_lowercase();
        for state in ["offline", "unauthorized"] {
            if message == state
                || message
                    .lines()
                    .any(|line| line.starts_with(&format!("error: device {state}")))
            {
                return Some(state);
            }
        }
    }
    None
}

pub(super) fn refresh_connections(adb_path: &Path) {
    refresh_with(|args| run(adb_path, args, Duration::from_secs(5)));
}

pub(super) fn scan_devices(
    adb_path: &Path,
    selected_serial: Option<&str>,
) -> Result<Vec<adb::AdbDevice>, String> {
    scan_with(selected_serial, |args, timeout| {
        run(adb_path, args, timeout)
    })
}

fn scan_with(
    selected_serial: Option<&str>,
    mut run_adb: impl FnMut(&[&str], Duration) -> Result<AdbReply, String>,
) -> Result<Vec<adb::AdbDevice>, String> {
    let read_devices = |reply: AdbReply| {
        if reply.success {
            Ok(adb::Adb::ready_devices_from_output(&reply.stdout))
        } else {
            Err(format!(
                "无法读取 ADB 设备列表: {} {}",
                reply.stdout, reply.stderr
            ))
        }
    };
    let devices = read_devices(run_adb(&["devices", "-l"], Duration::from_secs(5))?)?;
    let target = selected_serial.unwrap_or(adb::BLUESTACKS_SERIAL);
    if !adb::Adb::is_tcp_serial(target) || devices.iter().any(|device| device.serial == target) {
        return Ok(devices);
    }

    // A stale saved address must not prevent switching to another online device.
    // Reconnect and its follow-up query share five seconds; preserve the initial
    // list if either auxiliary operation fails or exhausts that budget.
    let deadline = Instant::now() + Duration::from_secs(5);
    if let Err(error) = run_adb(&["connect", target], Duration::from_secs(5)) {
        eprintln!("[adb] preferred device reconnect failed; continuing device scan: {error}");
    }
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return Ok(devices);
    };
    match run_adb(&["devices", "-l"], remaining).and_then(read_devices) {
        Ok(current) => Ok(current),
        Err(error) => {
            eprintln!("[adb] device refresh unavailable; using initial device list: {error}");
            Ok(devices)
        }
    }
}

fn refresh_with(mut run_adb: impl FnMut(&[&str]) -> Result<AdbReply, String>) {
    // Restarting the shared ADB server forgets manually added TCP transports.
    // Reconnect only offline transports so online LAN devices remain selectable.
    if let Err(error) = run_adb(&["reconnect", "offline"]) {
        eprintln!("[adb] reconnect offline failed; continuing device scan: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(success: bool, stdout: &str, stderr: &str) -> AdbReply {
        AdbReply {
            success,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn manual_endpoints_cover_lan_local_hostname_and_ipv6() {
        for (host, port, expected) in [
            ("192.168.1.50", "5555", "192.168.1.50:5555"),
            (" 127.0.0.1 ", " 5565 ", "127.0.0.1:5565"),
            ("android.local", "37123", "android.local:37123"),
            ("localhost", "65535", "localhost:65535"),
            ("2001:db8::1", "5555", "[2001:db8::1]:5555"),
            ("[2001:db8::1]", "5555", "[2001:db8::1]:5555"),
            ("fe80::1%12", "5555", "[fe80::1%12]:5555"),
        ] {
            assert_eq!(endpoint(host, port).unwrap(), expected);
        }
    }

    #[test]
    fn invalid_host_and_port_are_explained_before_running_adb() {
        for host in [
            "",
            " ",
            "http://192.168.1.50",
            "192.168.1.50:5555",
            "[bad]",
            "[::1",
            "bad host",
            "-H",
            "::1%",
        ] {
            assert!(endpoint(host, "5555").is_err(), "{host}");
        }
        for port in ["", "0", "65536", "-1", "+5555", "5555abc", "55.5", "5 555"] {
            assert!(endpoint("192.168.1.50", port).is_err(), "{port}");
        }
    }

    #[test]
    fn successful_and_existing_connections_return_the_requested_endpoint() {
        for stdout in [
            "connected to 192.168.1.50:5555",
            "already connected to 192.168.1.50:5555",
        ] {
            let result = connect_with("192.168.1.50:5555", |args| {
                Ok(match args[0] {
                    "connect" => {
                        assert_eq!(args, ["connect", "192.168.1.50:5555"]);
                        reply(true, stdout, "")
                    }
                    "-s" => {
                        assert_eq!(args, ["-s", "192.168.1.50:5555", "get-state"]);
                        reply(true, "device", "")
                    }
                    _ => panic!("unexpected ADB call"),
                })
            });
            assert_eq!(result.unwrap(), "192.168.1.50:5555");
        }
        assert!(connect_with("unable-phone.local:5555", |_| Ok(reply(
            true,
            "connected to unable-phone.local:5555",
            ""
        )))
        .is_ok());
    }

    #[test]
    fn connection_failure_is_not_hidden_by_successful_exit_status() {
        for (success, stdout, stderr) in [
            (
                true,
                "failed to connect to '192.168.1.50:5555': Connection refused",
                "",
            ),
            (true, "cannot connect to 192.168.1.50:5555", ""),
            (true, "", "Unable to connect to 192.168.1.50:5555"),
            (false, "network unreachable", ""),
            (false, "", "connection refused"),
        ] {
            let error = connect_with("192.168.1.50:5555", |_| Ok(reply(success, stdout, stderr)))
                .unwrap_err();
            assert!(error.contains("192.168.1.50:5555"));
            assert!(error.contains(if stdout.is_empty() { stderr } else { stdout }));
        }
        assert_eq!(
            connect_with("192.168.1.50:5555", |_| Err("ADB 连接超时".into())).unwrap_err(),
            "ADB 连接超时"
        );
    }

    #[test]
    fn already_connected_devices_report_offline_or_pending_authorization() {
        for state in ["offline", "unauthorized"] {
            for state_in_stderr in [false, true] {
                let error = connect_with("192.168.1.50:5555", |args| {
                    Ok(if args[0] == "connect" {
                        reply(true, "already connected to 192.168.1.50:5555", "")
                    } else if state_in_stderr {
                        reply(false, "", &format!("error: device {state}.\nCheck the confirmation dialog on your device."))
                    } else {
                        reply(true, state, "")
                    })
                }).unwrap_err();
                assert!(error.contains("未就绪"));
                assert!(error.contains(state));
            }
        }
        let result = connect_with("192.168.1.50:5555", |args| {
            if args[0] == "connect" {
                Ok(reply(true, "already connected to 192.168.1.50:5555", ""))
            } else {
                Err("query unavailable".into())
            }
        });
        assert_eq!(result.unwrap(), "192.168.1.50:5555");
    }

    #[test]
    fn unfamiliar_success_output_uses_device_state_without_blocking_on_auxiliary_failures() {
        for state in ["device", "offline", "unauthorized"] {
            let mut calls = 0;
            let result = connect_with("192.168.1.50:5555", |args| {
                calls += 1;
                if calls == 1 {
                    Ok(reply(true, "", ""))
                } else {
                    assert_eq!(args, ["-s", "192.168.1.50:5555", "get-state"]);
                    Ok(reply(true, state, ""))
                }
            });
            assert_eq!(result.is_ok(), state == "device");
            assert_eq!(calls, 2);
        }
        let mut calls = 0;
        assert!(connect_with("192.168.1.50:5555", |_| {
            calls += 1;
            if calls == 1 {
                Ok(reply(true, "", ""))
            } else {
                Err("query unavailable".into())
            }
        })
        .is_ok());
    }

    #[test]
    fn refreshing_preserves_online_transports_and_tolerates_reconnect_errors() {
        for command_fails in [false, true] {
            let mut online = vec!["192.168.1.50:5555", "127.0.0.1:5565"];
            refresh_with(|args| {
                // Model ADB's transport lifecycle: restarting the server or
                // disconnecting transports forgets manually connected devices.
                if args == ["kill-server"] || args == ["disconnect"] || args == ["reconnect"] {
                    online.clear();
                }
                if args.first() == Some(&"disconnect") && args.len() == 2 {
                    online.retain(|serial| *serial != args[1]);
                }
                if command_fails {
                    Err("unavailable".into())
                } else {
                    Ok(reply(true, "", ""))
                }
            });
            assert_eq!(online, ["192.168.1.50:5555", "127.0.0.1:5565"]);
        }
    }

    #[test]
    fn scan_keeps_other_online_devices_selectable_when_saved_lan_reconnect_times_out() {
        let mut calls = Vec::new();
        let devices = scan_with(Some("192.168.1.50:5555"), |args, timeout| {
            assert!(timeout <= Duration::from_secs(5));
            calls.push(args.join(" "));
            match args[0] {
                "connect" => Err("ADB 连接超时".into()),
                "devices" => Ok(reply(
                    true,
                    "List of devices attached\n192.168.1.60:5555\tdevice\n127.0.0.1:5555\tdevice\n",
                    "",
                )),
                _ => panic!("unexpected ADB call"),
            }
        })
        .unwrap();
        assert_eq!(
            devices
                .iter()
                .map(|device| device.serial.as_str())
                .collect::<Vec<_>>(),
            ["192.168.1.60:5555", "127.0.0.1:5555"]
        );
        assert_eq!(
            calls,
            ["devices -l", "connect 192.168.1.50:5555", "devices -l"]
        );
        assert_eq!(
            adb::Adb::select_ready_serial(&devices, Some("192.168.1.50:5555")).unwrap(),
            None
        );
    }

    #[test]
    fn scan_avoids_reconnecting_online_devices_and_non_network_serials() {
        for selected in [Some("192.168.1.50:5555"), Some("usb-missing")] {
            let mut calls = 0;
            let devices = scan_with(selected, |args, _| {
                calls += 1;
                assert_eq!(args, ["devices", "-l"]);
                Ok(reply(
                    true,
                    "List of devices attached\n192.168.1.50:5555\tdevice\n",
                    "",
                ))
            })
            .unwrap();
            assert_eq!(calls, 1);
            assert_eq!(devices[0].serial, "192.168.1.50:5555");
        }
    }

    #[test]
    fn scan_preserves_initial_devices_when_followup_query_fails() {
        let mut calls = 0;
        let devices = scan_with(Some("192.168.1.50:5555"), |_, _| {
            calls += 1;
            match calls {
                1 => Ok(reply(
                    true,
                    "List of devices attached\n192.168.1.60:5555\tdevice\n",
                    "",
                )),
                2 => Ok(reply(true, "failed to connect", "")),
                3 => Err("query unavailable".into()),
                _ => panic!("unexpected ADB call"),
            }
        })
        .unwrap();
        assert_eq!(devices[0].serial, "192.168.1.60:5555");
    }

    #[test]
    fn scan_without_saved_device_can_still_connect_local_default() {
        let mut calls = 0;
        let devices = scan_with(None, |args, _| {
            calls += 1;
            Ok(match calls {
                1 => reply(true, "List of devices attached\n", ""),
                2 => {
                    assert_eq!(args, ["connect", adb::BLUESTACKS_SERIAL]);
                    reply(true, "connected to 127.0.0.1:5555", "")
                }
                3 => reply(
                    true,
                    "List of devices attached\n127.0.0.1:5555\tdevice\n",
                    "",
                ),
                _ => panic!("unexpected ADB call"),
            })
        })
        .unwrap();
        assert_eq!(devices[0].serial, adb::BLUESTACKS_SERIAL);
    }
}
