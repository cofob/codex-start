use std::{
    env,
    ffi::OsStr,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

fn main() -> io::Result<()> {
    let manifest = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .ok_or_else(|| io::Error::other("CARGO_MANIFEST_DIR is unavailable"))?,
    );
    let root = manifest
        .join("../..")
        .canonicalize()
        .map_err(|error| io::Error::new(error.kind(), format!("workspace root: {error}")))?;
    let output = PathBuf::from(
        env::var_os("OUT_DIR").ok_or_else(|| io::Error::other("OUT_DIR is unavailable"))?,
    )
    .join("embedded_assets.rs");

    let mut files = Vec::new();
    for relative in [
        Path::new("Cargo.toml"),
        Path::new("Cargo.lock"),
        Path::new("rust-toolchain.toml"),
    ] {
        add_file(&root, relative, &mut files)?;
    }
    for relative in [
        Path::new("assets"),
        Path::new("images"),
        Path::new("crates/codex-start-core"),
        Path::new("crates/codex-start-host"),
        Path::new("crates/codex-start-proxy"),
        Path::new("xtask"),
    ] {
        collect_files(&root, relative, &mut files)?;
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files.dedup_by(|left, right| left.0 == right.0);

    let mut generated = fs::File::create(&output)?;
    writeln!(
        generated,
        "pub(crate) static EMBEDDED_FILES: &[(&str, &[u8])] = &["
    )?;
    for (relative, absolute) in files {
        println!("cargo:rerun-if-changed={}", absolute.display());
        writeln!(
            generated,
            "    ({relative:?}, include_bytes!({absolute:?})),",
            relative = relative.to_string_lossy(),
            absolute = absolute.to_string_lossy(),
        )?;
    }
    writeln!(generated, "];")?;
    Ok(())
}

fn collect_files(
    root: &Path,
    relative: &Path,
    files: &mut Vec<(PathBuf, PathBuf)>,
) -> io::Result<()> {
    let directory = root.join(relative);
    let mut entries = fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let file_name = entry.file_name();
        if ignored(&file_name) {
            continue;
        }
        let child = relative.join(file_name);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(root, &child, files)?;
        } else if file_type.is_file() {
            add_file(root, &child, files)?;
        }
    }
    Ok(())
}

fn add_file(root: &Path, relative: &Path, files: &mut Vec<(PathBuf, PathBuf)>) -> io::Result<()> {
    let absolute = root.join(relative).canonicalize()?;
    if absolute.is_file() {
        files.push((relative.to_path_buf(), absolute));
    }
    Ok(())
}

fn ignored(name: &OsStr) -> bool {
    matches!(name.to_str(), Some(".git" | "target" | ".DS_Store"))
}
