// OpenSSH wrapper that applies forwarded configuration without a shell.
use std::{
    env,
    ffi::{OsStr, OsString},
    os::unix::process::CommandExt as _,
    process::{Command, ExitCode},
};

const OPENSSH: &str = "/usr/bin/ssh";

fn command_for(
    args: impl IntoIterator<Item = OsString>,
    config: Option<&OsStr>,
    known_hosts: Option<&OsStr>,
) -> Command {
    let mut command = Command::new(OPENSSH);
    if let Some(config) = config {
        command.arg("-F").arg(config);
    }
    if let Some(known_hosts) = known_hosts {
        let mut option = OsString::from("UserKnownHostsFile=");
        option.push(known_hosts);
        command.arg("-o").arg(option);
    }
    command.args(args);
    command
}

fn main() -> ExitCode {
    let config = env::var_os("CODEX_START_SSH_CONFIG");
    let known_hosts = env::var_os("CODEX_START_KNOWN_HOSTS");
    let error = command_for(
        env::args_os().skip(1),
        config.as_deref(),
        known_hosts.as_deref(),
    )
    .exec();
    eprintln!("codex-start-ssh: could not execute {OPENSSH}: {error}");
    ExitCode::from(if error.kind() == std::io::ErrorKind::NotFound {
        127
    } else {
        126
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};

    use super::command_for;

    #[test]
    fn prepends_forwarded_paths_as_distinct_openssh_arguments() {
        let command = command_for(
            [
                OsString::from("-p"),
                OsString::from("22"),
                OsString::from("git@example"),
            ],
            Some(OsStr::new("/home/codex/custom dir/config")),
            Some(OsStr::new("/home/codex/custom dir/known_hosts")),
        );
        assert_eq!(command.get_program(), "/usr/bin/ssh");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                OsStr::new("-F"),
                OsStr::new("/home/codex/custom dir/config"),
                OsStr::new("-o"),
                OsStr::new("UserKnownHostsFile=/home/codex/custom dir/known_hosts"),
                OsStr::new("-p"),
                OsStr::new("22"),
                OsStr::new("git@example"),
            ]
        );
    }
}
