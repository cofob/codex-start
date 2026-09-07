//! Human-readable output for local gateway commands.
use super::{DaemonCommand, DeviceCommand, error};
use crate::{cli::Command, error::Result};
use codex_start_remote::{Connection, Invitation};
use serde_json::Value;

fn text(value: &Value) -> String {
    value
        .as_str()
        .unwrap_or("unknown")
        .chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

pub(super) fn invitation(uri: &str) -> Result<String> {
    let invite = Invitation::decode(uri).map_err(error)?;
    let mut lines = vec![
        format!("Invitation link: {uri}"),
        String::new(),
        "Manual connection:".into(),
    ];
    match invite.connection {
        Connection::Direct { host, port } => {
            lines.extend([
                "  Transport: Direct".into(),
                format!("  Host: {}", text(&Value::String(host))),
                format!("  Port: {port}"),
                format!(
                    "  TLS fingerprint: {}",
                    invite.fingerprint.unwrap_or_default()
                ),
            ]);
        }
        Connection::Yggdrasil {
            public_key,
            group_password,
            port,
        } => {
            lines.extend([
                "  Transport: Yggdrasil".into(),
                format!("  Host public key: {public_key}"),
                format!("  Group password: {group_password}"),
                format!("  Port: {port}"),
            ]);
        }
    }
    lines.push(format!(
        "  Connection password: {}",
        invite.connection_password
    ));
    lines.push(String::new());
    lines.push("You can paste the invitation link into the app's Invitation field.".into());
    Ok(lines.join("\n"))
}

pub(super) fn response(command: &Command, value: &Value) -> Result<String> {
    Ok(match command {
        Command::Daemon(args) => match args.command {
            DaemonCommand::Start(_) => "Daemon is running.".into(),
            DaemonCommand::Restart => "Daemon restarted and is ready.".into(),
            DaemonCommand::Stop => "Daemon shutdown requested.".into(),
            DaemonCommand::Install(_) => "Daemon service installed.".into(),
            DaemonCommand::Uninstall => "Daemon service uninstalled.".into(),
            DaemonCommand::Status if value["status"] == "running" => format!("Daemon is running.\nServer: {}\nDaemon ID: {}\nYggdrasil: {}", text(&value["discovery"]["name"]), text(&value["discovery"]["daemonId"]), text(&value["yggdrasil"])),
            DaemonCommand::Status => "Daemon is stopped.".into(),
            DaemonCommand::Run(_) => return Err(error("unexpected daemon run response")),
        },
        Command::Approve { .. } => "Device registration approved.".into(),
        Command::Device(args) => match &args.command {
            DeviceCommand::Revoke { id } => format!("Device {} revoked.", text(&Value::String(id.clone()))),
            DeviceCommand::List => {
                let devices = value["data"].as_array().ok_or_else(|| error("invalid device list"))?;
                if devices.is_empty() { "No registered devices.".into() } else {
                    let mut lines = vec!["Registered devices:".into()];
                    for device in devices {
                        lines.push(format!("  {}  {}  ({})", text(&device["id"]), text(&device["name"]), if device["revoked"] == true { "revoked" } else { "active" }));
                    }
                    lines.join("\n")
                }
            }
        },
        Command::ConnectionPassword(_) => "Connection password rotated. Old invitations no longer work.\nRegistered devices remain connected. Run codex-start connect for a new invitation.".into(),
        _ => return Err(error("unsupported remote response")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::Parser;
    use serde_json::json;

    #[test]
    fn human_status_and_devices_do_not_expose_json_or_terminal_controls() {
        let cli = Cli::try_parse_from(["codex-start", "daemon", "status"]).unwrap();
        assert_eq!(
            response(cli.command.as_ref().unwrap(), &json!({"status":"stopped"})).unwrap(),
            "Daemon is stopped."
        );
        let cli = Cli::try_parse_from(["codex-start", "device", "list"]).unwrap();
        let rendered = response(
            cli.command.as_ref().unwrap(),
            &json!({"data":[{"id":"phone-id", "name":"Phone\u{1b}[2J", "revoked":false}]}),
        )
        .unwrap();
        assert!(rendered.contains("phone-id"));
        assert!(rendered.contains("active"));
        assert!(!rendered.contains('\u{1b}'));
    }

    #[test]
    fn manual_parameters_match_both_invitation_transports() {
        use codex_start_remote::{PROTOCOL_VERSION, crypto};
        for connection in [
            Connection::Direct {
                host: "192.0.2.10".into(),
                port: 47321,
            },
            Connection::Yggdrasil {
                public_key: "ab".repeat(32),
                group_password: crypto::random_password(),
                port: 47321,
            },
        ] {
            let fingerprint =
                matches!(connection, Connection::Direct { .. }).then(|| "cd".repeat(32));
            let invite = Invitation {
                version: PROTOCOL_VERSION,
                connection,
                fingerprint,
                connection_password: crypto::random_password(),
            };
            let rendered = invitation(&invite.encode().unwrap()).unwrap();
            assert!(rendered.contains(&invite.connection_password));
            assert!(rendered.contains("Port: 47321"));
            match &invite.connection {
                Connection::Direct { host, .. } => {
                    assert!(rendered.contains(host));
                    assert!(rendered.contains(invite.fingerprint.as_ref().unwrap()));
                }
                Connection::Yggdrasil {
                    public_key,
                    group_password,
                    ..
                } => {
                    assert!(rendered.contains(public_key));
                    assert!(rendered.contains(group_password));
                    assert!(!rendered.contains("TLS fingerprint"));
                }
            }
        }
    }
}
