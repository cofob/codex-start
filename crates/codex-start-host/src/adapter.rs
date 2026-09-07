//! A fixed-project, stdio-compatible entry point for Desktop and IDE clients.

use std::{ffi::OsString, fs, path::Path};

use codex_start_core::{EffectiveConfig, TtyMode, WorktreeMode};

use crate::{
    cli::{AdapterArgs, RunArgs, RunOptions},
    error::{HostError, Result},
};

pub(crate) const ATTACHMENT_RUNTIME_INFO_ENV: &str = "CODEX_START_ADAPTER_ATTACHMENT_RUNTIME_INFO";

/// Select a foreground run. The client owns the server process and worktree.
pub fn run_args(args: AdapterArgs) -> RunArgs {
    let options = args.options;
    RunArgs {
        environment: args.environment,
        options: RunOptions {
            runtime: options.runtime,
            runtime_program: options.runtime_program,
            home: options.home,
            network: options.network,
            offline: options.offline,
            no_network: options.no_network,
            no_worktree: true,
            publish: options.publish,
            rebuild: options.rebuild,
            pull: options.pull,
            no_tty: true,
            dry_run: options.dry_run,
            ephemeral: true,
            allow_hosts: options.allow_hosts,
            runtime_args: options.runtime_args,
            ..RunOptions::default()
        },
        codex_args: if args.codex_args.is_empty() {
            vec!["app-server".into()]
        } else {
            args.codex_args
        },
    }
}

/// Keep these invariants even when global or project settings select other modes.
pub fn configure(config: &mut EffectiveConfig, cwd: &Path) -> Result<()> {
    if cfg!(windows) {
        return Err(HostError::Usage(
            "adapter requires macOS or Linux host paths; use it inside WSL on Windows".to_owned(),
        ));
    }
    if !cwd.is_dir() {
        return Err(HostError::Usage("--project must be a directory".to_owned()));
    }
    // Internal workers select a detached lifecycle outside the normal config layers.
    for key in [
        "CODEX_START_SESSION_WORKER",
        "CODEX_START_SESSION_INTERACTIVE",
    ] {
        if std::env::var_os(key).is_some() {
            return Err(HostError::Usage(
                "adapter cannot run inside a session worker".to_owned(),
            ));
        }
    }
    config.worktree = WorktreeMode::Never;
    config.sessions.enabled = false;
    config.tty = TtyMode::Never;
    config.workdir = Some(cwd.to_path_buf());
    // Different clients can use the same project at the same time.
    config.name = None;
    Ok(())
}

pub(crate) fn launcher_arguments(arguments: &[OsString]) -> Result<Vec<OsString>> {
    let mut result = Vec::new();
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        let text = argument
            .to_str()
            .ok_or_else(|| HostError::Usage("adapter options must be UTF-8".to_owned()))?;
        if text == "--" {
            break;
        }
        let path_flag = ["--project", "--config", "--runtime-program"]
            .into_iter()
            .find(|flag| text == *flag || text.starts_with(&format!("{flag}=")));
        if let Some(flag) = path_flag {
            let value = text
                .strip_prefix(&format!("{flag}="))
                .map(std::ffi::OsStr::new)
                .or_else(|| arguments.next().map(OsString::as_os_str))
                .ok_or_else(|| HostError::Usage(format!("{flag} requires a path")))?;
            let path = fs::canonicalize(value)
                .map_err(|source| HostError::io(Path::new(value), source))?;
            if flag == "--project" && !path.is_dir() {
                return Err(HostError::Usage("--project must be a directory".to_owned()));
            }
            result.extend([flag.into(), path.into_os_string()]);
        } else {
            result.push(argument.clone());
            // Values are opaque, including engine arguments that look like launcher flags.
            if [
                "--environment",
                "--runtime",
                "--home",
                "--network",
                "--publish",
                "-p",
                "--allow-host",
                "--runtime-arg",
                "--output",
            ]
            .contains(&text)
                && let Some(value) = arguments.next()
            {
                result.push(value.clone());
            }
        }
    }
    Ok(result)
}
