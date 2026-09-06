//! Installed Desktop and IDE executable. All arguments belong to Codex.

use std::{
    io,
    process::{Command, ExitCode},
};

fn main() -> ExitCode {
    match launch() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("codex-start-adapter: cannot start sibling codex-start: {error}");
            ExitCode::FAILURE
        }
    }
}

fn launch() -> io::Result<u8> {
    let executable = std::env::current_exe()?.with_file_name(if cfg!(windows) {
        "codex-start.exe"
    } else {
        "codex-start"
    });
    let mut command = Command::new(executable);
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if arguments
        .first()
        .is_some_and(|arg| arg == "install" || arg == "uninstall")
    {
        command.arg("__adapter-setup");
    } else {
        command.args(["adapter", "--"]);
    }
    command.args(arguments);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Replace this process so signals, streams, and exit status reach the launcher.
        Err(command.exec())
    }
    #[cfg(not(unix))]
    {
        let status = command.status()?;
        Ok(status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .unwrap_or(1))
    }
}
