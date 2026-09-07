//! Explicit daemon startup and private local administration.

use super::{DaemonCommand, DaemonOptions, DeviceCommand, PasswordCommand, error, state};
use crate::{
    cli::{Cli, Command, OutputFormat},
    error::Result,
    paths::{atomic_write, create_private_dir},
};
use serde_json::{Value, json};
use std::os::unix::process::CommandExt;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub async fn dispatch(cli: &Cli) -> Result<u8> {
    let config = cli
        .config
        .as_deref()
        .map(std::fs::canonicalize)
        .transpose()
        .map_err(error)?;
    let root = state::root(config.as_deref())?;
    let response = match cli.command.as_ref() {
        Some(Command::Daemon(args)) => match &args.command {
            DaemonCommand::Run(options) => {
                return super::server::run(root, config, options.clone()).await;
            }
            DaemonCommand::Start(options) => {
                start(&root, config.as_deref(), options).await?;
                json!({"status":"running"})
            }
            DaemonCommand::Restart => {
                restart(&root, config.as_deref()).await?;
                json!({"status":"running"})
            }
            DaemonCommand::Stop => control(&root, json!({"method":"stop"})).await?,
            DaemonCommand::Status => control(&root, json!({"method":"status"}))
                .await
                .unwrap_or_else(|_| json!({"status":"stopped"})),
            DaemonCommand::Install(options) => {
                install(&root, config.as_deref(), options).await?;
                json!({"status":"installed"})
            }
            DaemonCommand::Uninstall => {
                uninstall(&root).await?;
                json!({"status":"uninstalled"})
            }
        },
        Some(Command::Connect(args)) => {
            if control(&root, json!({"method":"status"})).await.is_err() {
                let options = std::fs::read(root.join("options.json"))
                    .ok()
                    .and_then(|v| serde_json::from_slice(&v).ok())
                    .unwrap_or_default();
                start(&root, config.as_deref(), &options).await?;
            }
            let response = control(&root, json!({"method":"invitation","host":args.host})).await?;
            if cli.output == OutputFormat::Human {
                let uri = response["uri"]
                    .as_str()
                    .ok_or_else(|| error("missing invitation"))?;
                let code = invitation_qr(uri)?;
                println!(
                    "{}\n{}",
                    code.render::<qrcode::render::unicode::Dense1x2>()
                        .light_color(qrcode::render::unicode::Dense1x2::Light)
                        .dark_color(qrcode::render::unicode::Dense1x2::Dark)
                        .build(),
                    super::output::invitation(uri)?
                );
                return Ok(0);
            }
            response
        }
        Some(Command::Approve { code }) => {
            control(&root, json!({"method":"approve","code":code})).await?
        }
        Some(Command::Device(args)) => match &args.command {
            DeviceCommand::List => control(&root, json!({"method":"devices"})).await?,
            DeviceCommand::Revoke { id } => {
                control(&root, json!({"method":"revoke","id":id})).await?
            }
        },
        Some(Command::ConnectionPassword(args)) => match args.command {
            PasswordCommand::Rotate => control(&root, json!({"method":"rotate"})).await?,
        },
        _ => return Err(error("invalid remote command")),
    };
    let rendered = if cli.output == OutputFormat::Json {
        serde_json::to_string(&response).map_err(error)?
    } else {
        super::output::response(
            cli.command
                .as_ref()
                .ok_or_else(|| error("missing command"))?,
            &response,
        )?
    };
    println!("{rendered}");
    Ok(0)
}

fn invitation_qr(uri: &str) -> Result<qrcode::QrCode> {
    let payload = uri
        .strip_prefix(codex_start_remote::INVITATION_PREFIX)
        .ok_or_else(|| error("unsupported invitation"))?;
    for version in 1..=40 {
        let mut bits = qrcode::bits::Bits::new(qrcode::Version::Normal(version));
        if bits
            .push_byte_data(codex_start_remote::INVITATION_PREFIX.as_bytes())
            .is_ok()
            && bits.push_alphanumeric_data(payload.as_bytes()).is_ok()
            && bits.push_terminator(qrcode::EcLevel::H).is_ok()
        {
            return qrcode::QrCode::with_bits(bits, qrcode::EcLevel::H).map_err(error);
        }
    }
    Err(error("invitation exceeds QR capacity"))
}

#[test]
fn compact_qr_uses_high_correction_and_alphanumeric_capacity() {
    let uri = format!(
        "{}{}",
        codex_start_remote::INVITATION_PREFIX,
        "A".repeat(104)
    );
    let code = invitation_qr(&uri).unwrap();
    assert_eq!(code.error_correction_level(), qrcode::EcLevel::H);
    // The URL prefix uses byte mode; the Base32 payload uses alphanumeric mode.
    assert_eq!(code.width(), 53);
}

pub async fn control(root: &Path, request: Value) -> Result<Value> {
    let stream = tokio::net::UnixStream::connect(root.join("control.sock"))
        .await
        .map_err(error)?;
    let (mut read, mut write) = stream.into_split();
    write
        .write_all(format!("{}\n", serde_json::to_string(&request).map_err(error)?).as_bytes())
        .await
        .map_err(error)?;
    let mut line = String::new();
    let mut reader = BufReader::new(&mut read);
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .map_err(error)?
        .map_err(error)?;
    let response: Value = serde_json::from_str(&line).map_err(error)?;
    if let Some(message) = response.get("error").and_then(Value::as_str) {
        return Err(error(message));
    }
    Ok(response)
}

async fn restart(root: &Path, config: Option<&Path>) -> Result<()> {
    let path = root.join("options.json");
    let options = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(error)?,
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => DaemonOptions::default(),
        Err(cause) => return Err(error(cause)),
    };
    if control(root, json!({"method":"status"})).await.is_ok() {
        control(root, json!({"method":"stop"})).await?;
    }
    // Socket removal precedes full shutdown. Wait for the process lock so the
    // replacement cannot start while the previous daemon still owns its port.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match crate::locking::RunLock::acquire(root, "daemon") {
            Ok(lock) => {
                drop(lock);
                break;
            }
            Err(cause) if tokio::time::Instant::now() >= deadline => return Err(cause),
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    start(root, config, &options).await
}

async fn start(root: &Path, config: Option<&Path>, options: &DaemonOptions) -> Result<()> {
    if control(root, json!({"method":"status"})).await.is_ok() {
        return Ok(());
    }
    atomic_write(
        &root.join("options.json"),
        &serde_json::to_string(options).map_err(error)?,
    )?;
    let executable = std::env::current_exe().map_err(error)?;
    let mut command = std::process::Command::new(executable);
    command.args(arguments(config, options));
    command.process_group(0);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("daemon.log"))
        .map_err(error)?;
    crate::paths::set_private_file(&root.join("daemon.log"))?;
    command
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(error)?)
        .stderr(log)
        .spawn()
        .map_err(error)?;
    wait_ready(root).await
}

async fn wait_ready(root: &Path) -> Result<()> {
    for _ in 0..100 {
        if control(root, json!({"method":"status"})).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(error(format!(
        "daemon did not start; inspect {}",
        root.join("daemon.log").display()
    )))
}

fn arguments(config: Option<&Path>, options: &DaemonOptions) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(config) = config {
        args.extend(["--config".into(), config.to_string_lossy().into_owned()]);
    }
    args.extend([
        "daemon".into(),
        "run".into(),
        "--bind".into(),
        options.bind.to_string(),
    ]);
    if options.no_yggdrasil {
        args.push("--no-yggdrasil".into());
    }
    if let Some(host) = &options.advertise_host {
        args.extend(["--advertise-host".into(), host.clone()]);
    }
    args
}

fn service_name(root: &Path) -> String {
    format!(
        "cs.fob.wtf.remote.{}",
        root.file_name().unwrap_or_default().to_string_lossy()
    )
}

async fn install(root: &Path, config: Option<&Path>, options: &DaemonOptions) -> Result<()> {
    let exe = std::env::current_exe().map_err(error)?;
    let name = service_name(root);
    let home = PathBuf::from(std::env::var_os("HOME").ok_or_else(|| error("HOME is unavailable"))?);
    let mut argv = vec![exe.to_string_lossy().into_owned()];
    argv.extend(arguments(config, options));
    let search_path =
        std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
    let log = root.join("daemon.log");
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(error)?;
    crate::paths::set_private_file(&log)?;
    #[cfg(target_os = "macos")]
    {
        let directory = home.join("Library/LaunchAgents");
        create_private_dir(&directory)?;
        let path = directory.join(format!("{name}.plist"));
        let uid = tokio::process::Command::new("id")
            .arg("-u")
            .output()
            .await
            .map_err(error)?;
        let domain = format!("gui/{}", String::from_utf8_lossy(&uid.stdout).trim());
        let service = format!("{domain}/{name}");
        if tokio::process::Command::new("launchctl")
            .args(["print", &service])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map_err(error)?
            .success()
        {
            checked("launchctl", &["bootout".into(), service]).await?;
        }
        let _ = control(root, json!({"method":"stop"})).await;
        atomic_write(&path, &launchd_plist(&name, &argv, &log, &search_path))?;
        bootstrap_launchd(&domain, &path).await?;
    }
    #[cfg(target_os = "linux")]
    {
        let directory = home.join(".config/systemd/user");
        create_private_dir(&directory)?;
        let line = argv
            .iter()
            .map(|a| {
                format!(
                    "\"{}\"",
                    a.replace('\\', "\\\\")
                        .replace('"', "\\\"")
                        .replace('%', "%%")
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        atomic_write(
            &directory.join(format!("{name}.service")),
            &format!(
                "[Unit]\nDescription=codex-start remote gateway\n[Service]\nExecStart={line}\nEnvironment=\"PATH={}\"\nRestart=on-failure\n[Install]\nWantedBy=default.target\n",
                search_path
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('%', "%%")
                    .replace('\n', "\\n")
            ),
        )?;
        checked("systemctl", &["--user".into(), "daemon-reload".into()]).await?;
        checked(
            "systemctl",
            &["--user".into(), "enable".into(), format!("{name}.service")],
        )
        .await?;
        checked(
            "systemctl",
            &["--user".into(), "restart".into(), format!("{name}.service")],
        )
        .await?;
    }
    wait_ready(root).await
}

#[cfg(target_os = "macos")]
async fn bootstrap_launchd(domain: &str, path: &Path) -> Result<()> {
    let args = [
        "bootstrap".into(),
        domain.into(),
        path.to_string_lossy().into_owned(),
    ];
    // bootout can return before launchd removes the previous registration.
    // bootstrap is keyed by the service label and cannot create a second instance.
    for attempt in 0..20 {
        match checked("launchctl", &args).await {
            Ok(()) => return Ok(()),
            Err(error) if attempt == 19 => return Err(error),
            Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    }
    unreachable!()
}

#[cfg(target_os = "macos")]
fn launchd_plist(name: &str, argv: &[String], log: &Path, search_path: &str) -> String {
    let arguments = argv.iter().fold(String::new(), |mut text, argument| {
        text.push_str("<string>");
        text.push_str(&xml(argument));
        text.push_str("</string>");
        text
    });
    let log = xml(&log.to_string_lossy());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Label</key><string>{}</string><key>ProgramArguments</key><array>{arguments}</array><key>EnvironmentVariables</key><dict><key>PATH</key><string>{}</string></dict><key>StandardOutPath</key><string>{log}</string><key>StandardErrorPath</key><string>{log}</string><key>RunAtLoad</key><true/><key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict></dict></plist>",
        xml(name),
        xml(search_path)
    )
}

async fn uninstall(root: &Path) -> Result<()> {
    let name = service_name(root);
    let home = PathBuf::from(std::env::var_os("HOME").ok_or_else(|| error("HOME is unavailable"))?);
    #[cfg(target_os = "macos")]
    {
        let path = home
            .join("Library/LaunchAgents")
            .join(format!("{name}.plist"));
        if path.exists() {
            checked(
                "launchctl",
                &["unload".into(), path.to_string_lossy().into_owned()],
            )
            .await?;
            std::fs::remove_file(path).map_err(error)?;
        }
    }
    #[cfg(target_os = "linux")]
    {
        checked(
            "systemctl",
            &[
                "--user".into(),
                "disable".into(),
                "--now".into(),
                format!("{name}.service"),
            ],
        )
        .await?;
        std::fs::remove_file(
            home.join(".config/systemd/user")
                .join(format!("{name}.service")),
        )
        .map_err(error)?;
        checked("systemctl", &["--user".into(), "daemon-reload".into()]).await?;
    }
    Ok(())
}

async fn checked(program: &str, args: &[String]) -> Result<()> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(error)?;
    if !output.status.success() {
        return Err(error(format!(
            "{program}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}
#[cfg(target_os = "macos")]
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
