//! Host filesystem RPCs from the Desktop/IDE connection, never from workload tools.

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Default)]
pub struct Filesystem {
    watches: BTreeMap<String, Watch>,
}

struct Watch {
    path: PathBuf,
    snapshot: BTreeMap<PathBuf, (u64, Option<SystemTime>)>,
}

impl Filesystem {
    pub fn handles(&self, method: &str, params: &Value) -> bool {
        if method == "fs/unwatch" {
            return params["watchId"]
                .as_str()
                .is_some_and(|id| self.watches.contains_key(id));
        }
        method.starts_with("fs/")
            && !["path", "sourcePath", "destinationPath"].iter().any(|key| {
                params[key]
                    .as_str()
                    .is_some_and(|path| Path::new(path).starts_with("/home/codex"))
            })
    }

    pub fn request(&mut self, method: &str, params: &Value) -> io::Result<Value> {
        if method == "fs/unwatch" {
            self.watches.remove(string(params, "watchId")?);
            return Ok(json!({}));
        }
        if method == "fs/copy" {
            let source = path(params, "sourcePath")?;
            let destination = path(params, "destinationPath")?;
            copy(
                &source,
                &destination,
                params["recursive"].as_bool().unwrap_or(false),
            )?;
            return Ok(json!({}));
        }
        let path = path(params, "path")?;
        match method {
            "fs/readFile" => Ok(json!({"dataBase64": STANDARD.encode(fs::read(path)?)})),
            "fs/writeFile" => {
                let data = STANDARD
                    .decode(string(params, "dataBase64")?)
                    .map_err(io::Error::other)?;
                fs::write(path, data)?;
                Ok(json!({}))
            }
            "fs/createDirectory" => {
                if params["recursive"].as_bool().unwrap_or(true) {
                    fs::create_dir_all(path)?;
                } else {
                    fs::create_dir(path)?;
                }
                Ok(json!({}))
            }
            "fs/readDirectory" => {
                let mut entries = Vec::new();
                for entry in fs::read_dir(path)? {
                    let entry = entry?;
                    let meta = fs::metadata(entry.path()).ok();
                    entries.push(json!({"fileName":entry.file_name().to_string_lossy(),
                        "isDirectory":meta.as_ref().is_some_and(fs::Metadata::is_dir),
                        "isFile":meta.as_ref().is_some_and(fs::Metadata::is_file)}));
                }
                entries.sort_by(|a, b| a["fileName"].as_str().cmp(&b["fileName"].as_str()));
                Ok(json!({"entries":entries}))
            }
            "fs/getMetadata" => {
                let link = fs::symlink_metadata(&path)?;
                let meta = fs::metadata(&path).unwrap_or(link.clone());
                Ok(json!({"isDirectory":meta.is_dir(), "isFile":meta.is_file(),
                    "isSymlink":link.is_symlink(), "createdAtMs":milliseconds(meta.created()),
                    "modifiedAtMs":milliseconds(meta.modified())}))
            }
            "fs/remove" => {
                let result = remove(&path, params["recursive"].as_bool().unwrap_or(true));
                if let Err(error) = result
                    && (error.kind() != io::ErrorKind::NotFound
                        || !params["force"].as_bool().unwrap_or(true))
                {
                    return Err(error);
                }
                Ok(json!({}))
            }
            "fs/watch" => {
                let id = string(params, "watchId")?;
                let path = fs::canonicalize(path)?;
                let snapshot = snapshot(&path);
                self.watches.insert(
                    id.to_owned(),
                    Watch {
                        path: path.clone(),
                        snapshot,
                    },
                );
                Ok(json!({"path":path}))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported host filesystem method",
            )),
        }
    }

    pub fn changes(&mut self) -> Vec<Value> {
        let mut events = Vec::new();
        for (id, watch) in &mut self.watches {
            let current = snapshot(&watch.path);
            let changed = current
                .keys()
                .chain(watch.snapshot.keys())
                .filter(|path| current.get(*path) != watch.snapshot.get(*path))
                .collect::<std::collections::BTreeSet<_>>();
            if !changed.is_empty() {
                events.push(
                    json!({"method":"fs/changed", "params":{"watchId":id,"changedPaths":changed}}),
                );
            }
            watch.snapshot = current;
        }
        events
    }
}

fn string<'a>(params: &'a Value, key: &str) -> io::Result<&'a str> {
    params[key].as_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{key} must be a string"),
        )
    })
}

fn path(params: &Value, key: &str) -> io::Result<PathBuf> {
    let path = PathBuf::from(string(params, key)?);
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "filesystem paths must be absolute",
        ));
    }
    Ok(path)
}

fn milliseconds(time: io::Result<SystemTime>) -> u64 {
    time.ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |time| {
            u64::try_from(time.as_millis()).unwrap_or(u64::MAX)
        })
}

fn remove(path: &Path, recursive: bool) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        if recursive {
            fs::remove_dir_all(path)
        } else {
            fs::remove_dir(path)
        }
    } else {
        fs::remove_file(path)
    }
}

fn copy(source: &Path, destination: &Path, recursive: bool) -> io::Result<()> {
    let meta = fs::symlink_metadata(source)?;
    if meta.is_symlink() {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(fs::read_link(source)?, destination);
        #[cfg(not(unix))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "symlink copy is unsupported on this host",
        ));
    }
    if !meta.is_dir() {
        fs::copy(source, destination)?;
        return Ok(());
    }
    if !recursive {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory copy requires recursive=true",
        ));
    }
    let canonical_source = fs::canonicalize(source)?;
    let parent = destination
        .ancestors()
        .find(|p| p.exists())
        .ok_or_else(|| io::Error::other("destination has no existing parent"))?;
    if fs::canonicalize(parent)?.starts_with(&canonical_source) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot copy a directory into itself",
        ));
    }
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        copy(&entry.path(), &destination.join(entry.file_name()), true)?;
    }
    Ok(())
}

fn snapshot(path: &Path) -> BTreeMap<PathBuf, (u64, Option<SystemTime>)> {
    walkdir::WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let meta = fs::symlink_metadata(entry.path()).ok()?;
            Some((entry.into_path(), (meta.len(), meta.modified().ok())))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_files_and_external_watch_changes_use_host_paths() {
        let root = tempfile::tempdir().unwrap();
        let mut service = Filesystem::default();
        let file = root.path().join("data");
        let data = STANDARD.encode([0, 255, 1, 128]);
        service
            .request("fs/writeFile", &json!({"path":file,"dataBase64":data}))
            .unwrap();
        assert_eq!(
            service
                .request("fs/readFile", &json!({"path":file}))
                .unwrap()["dataBase64"],
            data
        );
        service
            .request("fs/watch", &json!({"path":root.path(),"watchId":"one"}))
            .unwrap();
        assert!(service.changes().is_empty());
        fs::write(&file, b"external change").unwrap();
        let changes = service.changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0]["params"]["watchId"], "one");
        assert!(service.changes().is_empty());
        service
            .request("fs/unwatch", &json!({"watchId":"one"}))
            .unwrap();
        fs::remove_file(file).unwrap();
        assert!(service.changes().is_empty());
        assert!(
            service
                .request(
                    "fs/writeFile",
                    &json!({"path":"relative","dataBase64":data})
                )
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_and_remove_preserve_symlink_targets() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), b"keep").unwrap();
        let source = root.path().join("source");
        fs::create_dir(&source).unwrap();
        std::os::unix::fs::symlink(&outside, source.join("link")).unwrap();
        let destination = root.path().join("copy");
        copy(&source, &destination, true).unwrap();
        assert!(
            fs::symlink_metadata(destination.join("link"))
                .unwrap()
                .is_symlink()
        );
        assert!(copy(&source, &source.join("nested"), true).is_err());
        remove(&destination, true).unwrap();
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"keep");
    }
}
