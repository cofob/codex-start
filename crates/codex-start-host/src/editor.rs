//! Configurable editor argv templates.

use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use crate::{
    command::{CommandSpec, IoMode, run_interactive},
    error::{HostError, Result},
};

/// Open a path with configured argv or a deterministic platform fallback.
pub fn open(path: &Path, configured: &[String]) -> Result<u8> {
    let command_line = if configured.is_empty() {
        discover_editor()?
    } else {
        configured.to_vec()
    };
    let (program, templates) = command_line
        .split_first()
        .ok_or_else(|| HostError::Config("editor command is empty".to_owned()))?;
    let replacement = path.to_string_lossy();
    let mut replaced = false;
    let mut rendered = Vec::with_capacity(templates.len() + 1);
    for template in templates {
        if template == "{path}" {
            rendered.push(path.as_os_str().to_owned());
            replaced = true;
        } else if template.contains("{path}") {
            rendered.push(OsString::from(template.replace("{path}", &replacement)));
            replaced = true;
        } else {
            rendered.push(OsString::from(template));
        }
    }
    if !replaced {
        rendered.push(path.as_os_str().to_owned());
    }
    run_interactive(&CommandSpec::new(program).args(rendered).io(IoMode::Inherit))
}

fn discover_editor() -> Result<Vec<String>> {
    for variable in ["VISUAL", "EDITOR"] {
        if let Ok(value) = env::var(variable) {
            let parsed = parse_argv(&value)?;
            if !parsed.is_empty() {
                return Ok(parsed);
            }
        }
    }
    if executable("zed").is_some() {
        return Ok(vec!["zed".to_owned(), "{path}".to_owned()]);
    }
    if executable("code").is_some() {
        return Ok(vec!["code".to_owned(), "{path}".to_owned()]);
    }
    #[cfg(target_os = "macos")]
    if executable("open").is_some() {
        return Ok(vec![
            "open".to_owned(),
            "-a".to_owned(),
            "Zed".to_owned(),
            "{path}".to_owned(),
        ]);
    }
    #[cfg(target_os = "linux")]
    if executable("xdg-open").is_some() {
        return Ok(vec!["xdg-open".to_owned(), "{path}".to_owned()]);
    }
    Err(HostError::ExecutableMissing(
        "no editor found; configure settings.git.editor or set VISUAL/EDITOR".to_owned(),
    ))
}

fn executable(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.components().count() > 1 {
        return is_executable(path).then(|| path.to_path_buf());
    }
    env::var_os("PATH").and_then(|value| {
        env::split_paths(&value)
            .map(|directory| directory.join(name))
            .find(|candidate| is_executable(candidate))
    })
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    true
}

fn parse_argv(value: &str) -> Result<Vec<String>> {
    let mut output = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            } else {
                current.push(character);
            }
            continue;
        }
        if character.is_whitespace() && quote.is_none() {
            if !current.is_empty() {
                output.push(std::mem::take(&mut current));
            }
        } else {
            current.push(character);
        }
    }
    if escaped || quote.is_some() {
        return Err(HostError::Config(
            "VISUAL/EDITOR contains an unterminated escape or quote".to_owned(),
        ));
    }
    if !current.is_empty() {
        output.push(current);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::parse_argv;

    #[test]
    fn parses_editor_arguments_without_a_shell() {
        assert_eq!(
            parse_argv("code --reuse-window 'path with spaces'").expect("parse"),
            ["code", "--reuse-window", "path with spaces"]
        );
        assert!(parse_argv("code '").is_err());
    }
}
