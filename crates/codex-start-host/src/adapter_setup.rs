//! Install and remove client integration for the installed adapter.

use crate::{
    adapter_settings,
    error::{HostError, Result},
};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    process::Command,
};

const LABEL: &str = "cs.fob.wtf.adapter";
const VARIABLE: &str = "CODEX_CLI_PATH";

#[derive(Clone, Debug, Args)]
pub struct SetupArgs {
    #[command(subcommand)]
    action: SetupCommand,
}

#[derive(Clone, Debug, Subcommand)]
enum SetupCommand {
    /// Connect the installed adapter to selected clients.
    #[command(override_usage = "codex-start-adapter install [OPTIONS]")]
    Install(Options),
    /// Restore the client settings saved during installation.
    #[command(override_usage = "codex-start-adapter uninstall [OPTIONS]")]
    Uninstall(Options),
}

#[derive(Clone, Debug, Args)]
#[allow(clippy::struct_excessive_bools)] // Independent command-line switches.
struct Options {
    /// Configure VS Code user settings.
    #[arg(long)]
    vscode: bool,
    /// Configure `ChatGPT` app (Codex Desktop) on macOS.
    #[arg(
        long,
        alias = "chatgpt",
        help = "Configure ChatGPT app (Codex Desktop) on macOS"
    )]
    desktop: bool,
    /// Select all clients supported on this platform.
    #[arg(long)]
    all: bool,
    /// Use a specific VS Code user settings file; also selects VS Code.
    #[arg(long)]
    vscode_settings: Option<PathBuf>,
    /// Refuse to prompt when no client was selected.
    #[arg(long)]
    non_interactive: bool,
}

struct Paths {
    adapter: PathBuf,
    state: PathBuf,
    vscode: PathBuf,
    agent: PathBuf,
    launchctl: PathBuf,
}

#[derive(Serialize, Deserialize)]
enum Previous {
    Missing,
    Present(Value),
}

#[derive(Serialize, Deserialize)]
struct EditorReceipt {
    path: PathBuf,
    adapter: String,
    previous: Previous,
}

#[derive(Serialize, Deserialize)]
struct DesktopReceipt {
    adapter: String,
    previous: Option<String>,
    agent: String,
}

pub fn run(args: SetupArgs) -> Result<u8> {
    let (install, mut options) = match args.action {
        SetupCommand::Install(options) => (true, options),
        SetupCommand::Uninstall(options) => (false, options),
    };
    let paths = Paths::discover(&options)?;
    options.vscode |= options.vscode_settings.is_some() || options.all;
    options.desktop |= options.all && cfg!(target_os = "macos");
    if !options.vscode && !options.desktop {
        if options.non_interactive
            || !std::io::stdin().is_terminal()
            || !std::io::stderr().is_terminal()
        {
            return Err(HostError::Usage(
                "select --vscode, --desktop, or --all for non-interactive setup".into(),
            ));
        }
        let items = if cfg!(target_os = "macos") {
            vec!["VS Code (user settings)", "ChatGPT app (Codex Desktop)"]
        } else {
            vec!["VS Code (user settings)"]
        };
        let selection = dialoguer::MultiSelect::new()
            .with_prompt(if install {
                "Install codex-start adapter — Space to select, Enter to apply"
            } else {
                "Uninstall codex-start adapter — Space to select, Enter to apply"
            })
            .items(&items)
            .defaults(&vec![true; items.len()])
            .interact_opt()
            .map_err(|error| HostError::Runtime(error.to_string()))?;
        let Some(selection) = selection else {
            return Ok(0);
        };
        options.vscode = selection.contains(&0);
        options.desktop = selection.contains(&1);
    }
    if options.desktop && !cfg!(target_os = "macos") {
        return Err(HostError::Usage(
            "Desktop integration is supported on macOS only".into(),
        ));
    }
    if install && !paths.adapter.is_file() {
        return Err(HostError::NotFound(format!(
            "installed adapter {}",
            paths.adapter.display()
        )));
    }
    crate::paths::create_private_dir(&paths.state)?;
    let lock_path = paths.state.join("setup.lock");
    regular_or_missing(&lock_path)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| HostError::io(&lock_path, error))?;
    fs2::FileExt::lock_exclusive(&lock).map_err(|error| HostError::io(&lock_path, error))?;
    // Validate selected files before writing to either client.
    if options.vscode {
        let contents = read_optional(&paths.vscode)?.unwrap_or_else(|| "{}\n".into());
        adapter_settings::update(&contents, Some(&json!(paths.adapter)))?;
    }
    if options.desktop {
        regular_or_missing(&paths.agent)?;
    }
    if options.vscode {
        editor(&paths, install)?;
        println!(
            "VS Code: {}. Reload the VS Code window.",
            if install {
                "adapter installed"
            } else {
                "integration removed"
            }
        );
    }
    if options.desktop {
        desktop(&paths, install)?;
        println!(
            "ChatGPT app (Codex Desktop): {}. Quit and reopen the app.",
            if install {
                "adapter installed"
            } else {
                "integration removed"
            }
        );
    }
    Ok(0)
}

impl Paths {
    fn discover(options: &Options) -> Result<Self> {
        let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .map(PathBuf::from)
            .ok_or_else(|| HostError::Config("user home is not available".into()))?;
        let app_paths = codex_start_core::AppPaths::discover()
            .map_err(|error| HostError::Config(error.to_string()))?;
        let user_settings = if cfg!(target_os = "macos") {
            home.join("Library/Application Support/Code/User/settings.json")
        } else if cfg!(windows) {
            std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .ok_or_else(|| HostError::Config("APPDATA is not set".into()))?
                .join("Code/User/settings.json")
        } else {
            std::env::var_os("XDG_CONFIG_HOME")
                .map_or_else(|| home.join(".config"), PathBuf::from)
                .join("Code/User/settings.json")
        };
        let vscode = options.vscode_settings.clone().unwrap_or(user_settings);
        let vscode = std::path::absolute(&vscode).map_err(|error| HostError::io(&vscode, error))?;
        Ok(Self {
            adapter: std::env::current_exe()
                .map_err(|error| HostError::io("executable", error))?
                .with_file_name(if cfg!(windows) {
                    "codex-start-adapter.exe"
                } else {
                    "codex-start-adapter"
                }),
            state: app_paths.data.join("adapter-install"),
            vscode,
            agent: home.join(format!("Library/LaunchAgents/{LABEL}.plist")),
            launchctl: PathBuf::from("/bin/launchctl"),
        })
    }
}

fn editor(paths: &Paths, install: bool) -> Result<()> {
    let receipt_path = paths.state.join(format!(
        "vscode-{}.json",
        blake3::hash(paths.vscode.to_string_lossy().as_bytes()).to_hex()
    ));
    let saved = read_optional(&receipt_path)?;
    let mut receipt = saved
        .as_deref()
        .map(serde_json::from_str::<EditorReceipt>)
        .transpose()
        .map_err(serialization)?;
    let before = read_optional(&paths.vscode)?.unwrap_or_else(|| "{}\n".into());
    let current = adapter_settings::value(&before)?;
    let adapter = paths
        .adapter
        .to_str()
        .ok_or_else(|| HostError::Config("adapter path must be UTF-8".into()))?
        .to_owned();
    let next = if install {
        if let Some(saved) = &receipt {
            if current != Some(json!(saved.adapter)) && current != Some(json!(adapter)) {
                return Err(HostError::Config("VS Code setting changed after installation; uninstall this integration before installing again".into()));
            }
        } else {
            receipt = Some(EditorReceipt {
                path: paths.vscode.clone(),
                adapter: adapter.clone(),
                previous: if current == Some(json!(adapter)) {
                    Previous::Missing
                } else {
                    current.clone().map_or(Previous::Missing, Previous::Present)
                },
            });
        }
        let receipt = receipt.as_mut().expect("installation receipt");
        receipt.adapter.clone_from(&adapter);
        write_atomic(
            &receipt_path,
            &serde_json::to_string_pretty(receipt).map_err(serialization)?,
        )?;
        Some(json!(adapter))
    } else {
        let saved = match receipt {
            Some(saved) => saved,
            None if current == Some(json!(adapter)) => EditorReceipt {
                path: paths.vscode.clone(),
                adapter: adapter.clone(),
                previous: Previous::Missing,
            },
            None => return Ok(()),
        };
        if current != Some(json!(saved.adapter)) {
            // Do not undo a newer manual setting change.
            fs::remove_file(&receipt_path).map_err(|error| HostError::io(&receipt_path, error))?;
            return Ok(());
        }
        match saved.previous {
            Previous::Missing => None,
            Previous::Present(value) => Some(value),
        }
    };
    let after = adapter_settings::update(&before, next.as_ref())?;
    if read_optional(&paths.vscode)?.unwrap_or_else(|| "{}\n".into()) != before {
        return Err(HostError::Config(
            "VS Code settings changed during setup; retry the command".into(),
        ));
    }
    write_atomic(&paths.vscode, &after)?;
    if !install && receipt_path.exists() {
        fs::remove_file(&receipt_path).map_err(|error| HostError::io(&receipt_path, error))?;
    }
    Ok(())
}

fn desktop(paths: &Paths, install: bool) -> Result<()> {
    let receipt_path = paths.state.join("desktop.json");
    let receipt = read_optional(&receipt_path)?
        .as_deref()
        .map(serde_json::from_str::<DesktopReceipt>)
        .transpose()
        .map_err(serialization)?;
    let existing_agent = read_optional(&paths.agent)?;
    if existing_agent.is_some()
        && existing_agent.as_deref() != receipt.as_ref().map(|receipt| receipt.agent.as_str())
    {
        return Err(HostError::Config(format!(
            "{} is not the saved adapter LaunchAgent; preserve it and resolve the conflict first",
            paths.agent.display()
        )));
    }
    let current = launchctl(paths, &["getenv", VARIABLE], true)?;
    let current = (!current.is_empty()).then_some(current);
    if install {
        let adapter = paths
            .adapter
            .to_str()
            .ok_or_else(|| HostError::Config("adapter path must be UTF-8".into()))?
            .to_owned();
        if let Some(saved) = &receipt
            && current.as_deref().is_some_and(|current| {
                current != saved.adapter
                    && current != adapter
                    && Some(current) != saved.previous.as_deref()
            })
        {
            return Err(HostError::Config("Desktop CLI override changed after installation; uninstall this integration before installing again".into()));
        }
        let agent = agent_plist(&adapter);
        let previous = receipt.map_or_else(
            || current.filter(|current| current != &adapter),
            |receipt| receipt.previous,
        );
        let receipt = DesktopReceipt {
            adapter: adapter.clone(),
            previous,
            agent: agent.clone(),
        };
        write_atomic(
            &receipt_path,
            &serde_json::to_string_pretty(&receipt).map_err(serialization)?,
        )?;
        write_atomic(&paths.agent, &agent)?;
        let domain = gui_domain()?;
        let path = paths.agent.to_string_lossy();
        launchctl(paths, &["bootout", &domain, &path], true)?;
        launchctl(paths, &["bootstrap", &domain, &path], false)?;
        // Apply synchronously; the RunAtLoad agent also restores it on the next login.
        launchctl(paths, &["setenv", VARIABLE, &adapter], false)?;
    } else if let Some(receipt) = receipt {
        let domain = gui_domain()?;
        launchctl(
            paths,
            &["bootout", &domain, &paths.agent.to_string_lossy()],
            true,
        )?;
        if current.as_deref() == Some(&receipt.adapter) {
            if let Some(previous) = receipt.previous {
                launchctl(paths, &["setenv", VARIABLE, &previous], false)?;
            } else {
                launchctl(paths, &["unsetenv", VARIABLE], false)?;
            }
        }
        if existing_agent.is_some() {
            fs::remove_file(&paths.agent).map_err(|error| HostError::io(&paths.agent, error))?;
        }
        fs::remove_file(&receipt_path).map_err(|error| HostError::io(&receipt_path, error))?;
    } else if current.as_deref() == paths.adapter.to_str() {
        launchctl(paths, &["unsetenv", VARIABLE], false)?;
    }
    Ok(())
}

fn gui_domain() -> Result<String> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|error| HostError::io("id", error))?;
    if !output.status.success() {
        return Err(HostError::Runtime("cannot determine login user id".into()));
    }
    let uid = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .map_err(|error| HostError::Runtime(error.to_string()))?;
    Ok(format!("gui/{uid}"))
}

fn launchctl(paths: &Paths, arguments: &[&str], allow_failure: bool) -> Result<String> {
    let output = Command::new(&paths.launchctl)
        .args(arguments)
        .output()
        .map_err(|error| HostError::io(&paths.launchctl, error))?;
    if !allow_failure && !output.status.success() {
        return Err(HostError::Runtime(format!(
            "launchctl {} failed: {}",
            arguments[0],
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end_matches(['\r', '\n'])
        .to_owned())
}

fn agent_plist(adapter: &str) -> String {
    let adapter = adapter
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;");
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{LABEL}</string><key>ProgramArguments</key><array><string>/bin/launchctl</string><string>setenv</string><string>{VARIABLE}</string><string>{adapter}</string></array><key>RunAtLoad</key><true/></dict></plist>\n"
    )
}

fn serialization(error: impl std::fmt::Display) -> HostError {
    HostError::Serialization(error.to_string())
}

fn regular_or_missing(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(HostError::Config(format!(
            "{} must be a regular file",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(HostError::io(path, error)),
    }
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    regular_or_missing(path)?;
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(HostError::io(path, error)),
    }
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    regular_or_missing(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| HostError::Config("file has no parent".into()))?;
    fs::create_dir_all(parent).map_err(|error| HostError::io(parent, error))?;
    let mut file =
        tempfile::NamedTempFile::new_in(parent).map_err(|error| HostError::io(parent, error))?;
    if let Ok(metadata) = fs::metadata(path) {
        file.as_file()
            .set_permissions(metadata.permissions())
            .map_err(|error| HostError::io(path, error))?;
    }
    file.write_all(text.as_bytes())
        .map_err(|error| HostError::io(path, error))?;
    file.as_file()
        .sync_all()
        .map_err(|error| HostError::io(path, error))?;
    file.persist(path)
        .map_err(|error| HostError::io(path, error.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(root: &Path) -> Paths {
        Paths {
            adapter: root.join("bin & tools/codex-start-adapter"),
            state: root.join("state"),
            vscode: root.join("Code/User/settings.json"),
            agent: root.join("LaunchAgents/adapter.plist"),
            launchctl: root.join("launchctl"),
        }
    }

    #[test]
    fn editor_round_trip_preserves_prior_setting_and_later_edits() {
        for previous in [None, Some(Value::Null), Some(json!("/old/codex"))] {
            let root = tempfile::tempdir().unwrap();
            let paths = fixture(root.path());
            let original = adapter_settings::update(
                "{ // keep\n \"editor.fontSize\": 14,\n}",
                previous.as_ref(),
            )
            .unwrap();
            write_atomic(&paths.vscode, &original).unwrap();
            editor(&paths, true).unwrap();
            editor(&paths, true).unwrap();
            assert_eq!(
                adapter_settings::value(&read_optional(&paths.vscode).unwrap().unwrap()).unwrap(),
                Some(json!(paths.adapter))
            );
            let text = read_optional(&paths.vscode)
                .unwrap()
                .unwrap()
                .replace(": 14", ": 16");
            write_atomic(&paths.vscode, &text).unwrap();
            editor(&paths, false).unwrap();
            let restored = read_optional(&paths.vscode).unwrap().unwrap();
            assert_eq!(adapter_settings::value(&restored).unwrap(), previous);
            assert!(restored.contains("// keep"));
            assert!(restored.contains(": 16"));
            editor(&paths, false).unwrap();
        }
    }

    #[test]
    fn editor_uninstall_does_not_replace_a_new_manual_override() {
        let root = tempfile::tempdir().unwrap();
        let paths = fixture(root.path());
        editor(&paths, true).unwrap();
        let text = adapter_settings::update(
            &read_optional(&paths.vscode).unwrap().unwrap(),
            Some(&json!("/new/codex")),
        )
        .unwrap();
        write_atomic(&paths.vscode, &text).unwrap();
        editor(&paths, false).unwrap();
        assert_eq!(read_optional(&paths.vscode).unwrap().unwrap(), text);
    }

    #[test]
    fn adopts_manual_editor_installation_and_rejects_invalid_json() {
        let root = tempfile::tempdir().unwrap();
        let paths = fixture(root.path());
        write_atomic(
            &paths.vscode,
            &json!({"chatgpt.cliExecutable": paths.adapter}).to_string(),
        )
        .unwrap();
        editor(&paths, true).unwrap();
        editor(&paths, false).unwrap();
        assert!(
            adapter_settings::value(&read_optional(&paths.vscode).unwrap().unwrap())
                .unwrap()
                .is_none()
        );
        write_atomic(&paths.vscode, "{broken}").unwrap();
        assert!(editor(&paths, true).is_err());
        assert_eq!(read_optional(&paths.vscode).unwrap().unwrap(), "{broken}");
    }

    #[cfg(unix)]
    #[test]
    fn desktop_round_trip_restores_environment_and_removes_agent() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let paths = fixture(root.path());
        fs::write(
            &paths.launchctl,
            r"#!/usr/bin/env python3
import sys
from pathlib import Path
state=Path(__file__).with_name('environment')
if sys.argv[1] == 'getenv': print(state.read_text() if state.exists() else '', end='')
elif sys.argv[1] == 'setenv': state.write_text(sys.argv[3])
elif sys.argv[1] == 'unsetenv': state.unlink(missing_ok=True)
",
        )
        .unwrap();
        fs::set_permissions(&paths.launchctl, fs::Permissions::from_mode(0o700)).unwrap();
        let environment = root.path().join("environment");
        fs::write(&environment, "/original/codex").unwrap();
        desktop(&paths, true).unwrap();
        desktop(&paths, true).unwrap();
        assert_eq!(
            fs::read_to_string(&environment).unwrap(),
            paths.adapter.to_str().unwrap()
        );
        let plist = fs::read_to_string(&paths.agent).unwrap();
        assert!(plist.contains("bin &amp; tools"));
        #[cfg(target_os = "macos")]
        assert!(
            Command::new("/usr/bin/plutil")
                .arg("-lint")
                .arg(&paths.agent)
                .output()
                .unwrap()
                .status
                .success()
        );
        desktop(&paths, false).unwrap();
        assert_eq!(fs::read_to_string(&environment).unwrap(), "/original/codex");
        assert!(!paths.agent.exists());
        assert!(!paths.state.join("desktop.json").exists());
        desktop(&paths, false).unwrap();
    }
}
