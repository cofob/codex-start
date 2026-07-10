//! Safe external command construction and execution.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    io,
    path::PathBuf,
    process::{Command, ExitStatus, Stdio},
};

use crate::error::{HostError, Result};

/// How a command should connect to the parent terminal.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum IoMode {
    /// Capture stdout and stderr.
    #[default]
    Capture,
    /// Inherit stdin, stdout and stderr.
    Inherit,
    /// Send child stdout and stderr to the launcher's stderr.
    Diagnostic,
    /// Discard all streams.
    Null,
}

/// A shell-free external command description.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    /// Executable path or name.
    pub program: OsString,
    /// Exact argv entries, excluding argv zero.
    pub args: Vec<OsString>,
    /// Environment additions or replacements.
    pub env: BTreeMap<OsString, OsString>,
    /// Environment keys to remove.
    pub env_remove: BTreeSet<OsString>,
    /// Optional working directory.
    pub cwd: Option<PathBuf>,
    /// Stream behavior.
    pub io: IoMode,
}

impl CommandSpec {
    /// Construct a command without invoking a shell.
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            env_remove: BTreeSet::new(),
            cwd: None,
            io: IoMode::Capture,
        }
    }

    /// Add one argument.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Add a sequence of arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set stream handling.
    pub const fn io(mut self, mode: IoMode) -> Self {
        self.io = mode;
        self
    }

    fn to_command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        command.envs(&self.env);
        for key in &self.env_remove {
            command.env_remove(key);
        }
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        match self.io {
            IoMode::Capture => {
                command.stdin(Stdio::null());
                command.stdout(Stdio::piped());
                command.stderr(Stdio::piped());
            }
            IoMode::Inherit => {
                command.stdin(Stdio::inherit());
                command.stdout(Stdio::inherit());
                command.stderr(Stdio::inherit());
            }
            IoMode::Diagnostic => {
                command.stdin(Stdio::null());
                command.stdout(Stdio::piped());
                command.stderr(Stdio::inherit());
            }
            IoMode::Null => {
                command.stdin(Stdio::null());
                command.stdout(Stdio::null());
                command.stderr(Stdio::null());
            }
        }
        command
    }
}

/// Captured command result.
#[derive(Clone, Debug)]
pub struct CommandOutput {
    /// Process exit status.
    pub status: ExitStatus,
    /// Captured stdout bytes.
    pub stdout: Vec<u8>,
    /// Captured stderr bytes.
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    /// Decode stdout lossily and trim surrounding whitespace.
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_owned()
    }

    /// Return an error when the process was unsuccessful.
    pub fn require_success(self, program: &OsStr) -> Result<Self> {
        if self.status.success() {
            Ok(self)
        } else {
            Err(HostError::CommandFailed {
                program: program.to_owned(),
                status: self.status,
                stderr: sanitize_stderr(&self.stderr),
            })
        }
    }
}

/// Execute a command and capture its output.
pub fn run_capture(spec: &CommandSpec) -> Result<CommandOutput> {
    let output = spec
        .to_command()
        .output()
        .map_err(|source| HostError::CommandIo {
            program: spec.program.clone(),
            source,
        })?;
    Ok(CommandOutput {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

/// Execute a command and require success.
pub fn run_checked(spec: &CommandSpec) -> Result<CommandOutput> {
    run_capture(spec)?.require_success(&spec.program)
}

/// Execute an interactive command and return its exact exit code.
pub fn run_interactive(spec: &CommandSpec) -> Result<u8> {
    let mut interactive = spec.clone();
    interactive.io = IoMode::Inherit;
    let status = interactive
        .to_command()
        .status()
        .map_err(|source| HostError::CommandIo {
            program: interactive.program.clone(),
            source,
        })?;
    Ok(exit_code(status))
}

/// Execute a non-interactive diagnostic command without contaminating stdout.
pub fn run_diagnostic(spec: &CommandSpec) -> Result<u8> {
    let mut diagnostic = spec.clone();
    diagnostic.io = IoMode::Diagnostic;
    let mut child = diagnostic
        .to_command()
        .spawn()
        .map_err(|source| HostError::CommandIo {
            program: diagnostic.program.clone(),
            source,
        })?;
    let mut stdout = child.stdout.take().ok_or_else(|| {
        HostError::Runtime("diagnostic command stdout pipe was not created".to_owned())
    })?;
    let copy_result = io::copy(&mut stdout, &mut io::stderr().lock());
    let wait_result = child.wait();
    copy_result.map_err(|source| HostError::CommandIo {
        program: diagnostic.program.clone(),
        source,
    })?;
    let status = wait_result.map_err(|source| HostError::CommandIo {
        program: diagnostic.program,
        source,
    })?;
    Ok(exit_code(status))
}

/// Convert platform exit status to a portable byte exit code.
pub fn exit_code(status: ExitStatus) -> u8 {
    status
        .code()
        .and_then(|value| u8::try_from(value).ok())
        .unwrap_or(1)
}

fn sanitize_stderr(stderr: &[u8]) -> String {
    const LIMIT: usize = 8_192;
    let mut text = String::from_utf8_lossy(stderr).trim().to_owned();
    if text.len() > LIMIT {
        text.truncate(LIMIT);
        text.push('…');
    }
    text
}
