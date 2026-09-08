//! Read-only discovery of local Codex history, independent of running containers.
//! Never import, repair, resume, or acquire another client's writer during discovery.

use super::{error, state::State};
#[cfg(not(test))]
use crate::paths::AppPaths;
use crate::{error::Result, home};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone)]
struct Entry {
    path: PathBuf,
    home: PathBuf,
    modified: Option<SystemTime>,
    size: u64,
    value: Value,
}

#[derive(Default)]
pub struct History {
    entries: BTreeMap<PathBuf, Entry>,
    refreshed: Option<Instant>,
    warnings: Vec<String>,
}

fn store_id(path: &Path) -> String {
    format!(
        "history-{}",
        &blake3::hash(path.as_os_str().as_encoded_bytes()).to_hex()[..24]
    )
}

#[cfg_attr(test, allow(clippy::unnecessary_wraps))]
fn homes(state: &State) -> Result<Vec<(String, PathBuf)>> {
    #[cfg(test)]
    {
        Ok(["Host Codex", "Managed Codex"]
            .into_iter()
            .filter_map(|name| {
                std::fs::canonicalize(state.root.join("history-fixture").join(name))
                    .ok()
                    .map(|path| (name.to_owned(), path))
            })
            .collect())
    }
    #[cfg(not(test))]
    {
        let paths = AppPaths::discover()?;
        let mut configs = home::discover_home_configs(&paths)?;
        let config_path = state.config.clone().unwrap_or_else(|| paths.config_file());
        match std::fs::read_to_string(&config_path) {
            Ok(text) => configs.extend(
                codex_start_core::ConfigDocument::parse_file(&config_path, &text)
                    .map_err(error)?
                    .homes,
            ),
            Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => {}
            Err(failure) => return Err(error(failure)),
        }
        let mut found = BTreeMap::new();
        // The default host store remains visible even if a named home called "host" was overridden.
        if let Some(user) = std::env::var_os("HOME") {
            found.insert(PathBuf::from(user).join(".codex"), "Host Codex".to_owned());
        }
        if let Some(path) = std::env::var_os("CODEX_HOME") {
            found.insert(PathBuf::from(path), "Host CODEX_HOME".to_owned());
        }
        for (name, config) in configs {
            let resolved = home::ResolvedHome::preview(
                &name,
                &crate::configuration::host_home_spec(&config),
                &paths,
            )?;
            found.entry(resolved.codex_home).or_insert(name);
        }
        let mut unique = BTreeMap::new();
        for (path, name) in found {
            if path.is_dir() {
                unique
                    .entry(std::fs::canonicalize(path).map_err(error)?)
                    .or_insert(name);
            }
        }
        Ok(unique
            .into_iter()
            .map(|(path, name)| (name, path))
            .collect())
    }
}

fn database(root: &Path, prefix: &str) -> Result<Option<Connection>> {
    home::latest_versioned_database(root, prefix)?
        .map(|path| {
            let db = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(error)?;
            db.busy_timeout(Duration::from_millis(250)).map_err(error)?;
            Ok(db)
        })
        .transpose()
}

fn metadata(root: &Path) -> Result<BTreeMap<String, Value>> {
    let Some(db) = database(root, "state_")? else {
        return Ok(BTreeMap::new());
    };
    let mut statement = db
        .prepare("SELECT id, substr(title,1,320), substr(preview,1,320), updated_at, is_pinned, cwd, source, archived, created_at FROM threads")
        .map_err(error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                json!({"name":row.get::<_, String>(1)?,"preview":row.get::<_, String>(2)?,
            "updatedAt":row.get::<_, i64>(3)?,"isPinned":row.get::<_, bool>(4)?,"cwd":row.get::<_, String>(5)?,
            "source":serde_json::from_str::<Value>(&row.get::<_, String>(6)?).unwrap_or(json!(row.get::<_, String>(6)?)),
            "archived":row.get::<_, bool>(7)?,"createdAt":row.get::<_, i64>(8)?}),
            ))
        })
        .map_err(error)?;
    rows.map(|row| row.map_err(error)).collect()
}

fn index_titles(root: &Path) -> BTreeMap<String, String> {
    let mut titles = BTreeMap::new();
    if let Ok(file) = File::open(root.join("session_index.jsonl")) {
        for line in BufReader::new(file.take(16 * 1024 * 1024))
            .lines()
            .map_while(std::result::Result::ok)
        {
            if let Ok(value) = serde_json::from_str::<Value>(&line)
                && let (Some(id), Some(name)) =
                    (value["id"].as_str(), value["thread_name"].as_str())
            {
                titles.insert(id.to_owned(), name.chars().take(320).collect());
            }
        }
    }
    titles
}

impl History {
    fn refresh(&mut self, roots: &[(String, PathBuf)]) -> Result<()> {
        let mut seen = BTreeSet::new();
        self.warnings.clear();
        for (label, root) in roots {
            let names = if let Ok(names) = metadata(root) {
                names
            } else {
                self.warnings.push(format!("{label}: cannot read the Codex index. Showing saved files; names or dates can be older."));
                BTreeMap::new()
            };
            let titles = index_titles(root);
            let mut skipped = 0;
            for directory in ["sessions", "archived_sessions"] {
                let base = root.join(directory);
                if !base.exists() {
                    continue;
                }
                if std::fs::symlink_metadata(&base)
                    .map_err(error)?
                    .file_type()
                    .is_symlink()
                {
                    self.warnings.push(format!(
                        "{label}: symbolic-link history directory was not read."
                    ));
                    continue;
                }
                for file in walkdir::WalkDir::new(&base)
                    .follow_links(false)
                    .min_depth(1)
                {
                    let Ok(file) = file else {
                        skipped += 1;
                        continue;
                    };
                    if !file.file_type().is_file()
                        || file.path().extension().is_none_or(|ext| ext != "jsonl")
                    {
                        continue;
                    }
                    let path = file.path().to_path_buf();
                    let Ok(stat) = file.metadata() else {
                        skipped += 1;
                        continue;
                    };
                    seen.insert(path.clone());
                    let cached = self.entries.get(&path).filter(|entry| {
                        entry.modified == stat.modified().ok() && entry.size == stat.len()
                    });
                    let mut entry = if let Some(entry) = cached {
                        entry.clone()
                    } else {
                        let Ok(Some(value)) = home::history_summary(&path, root) else {
                            self.entries.remove(&path);
                            skipped += 1;
                            continue;
                        };
                        Entry {
                            path: path.clone(),
                            home: root.clone(),
                            modified: stat.modified().ok(),
                            size: stat.len(),
                            value,
                        }
                    };
                    let id = entry.value["id"].as_str().unwrap_or_default();
                    if let Some(name) = titles.get(id) {
                        entry.value["name"] = json!(name);
                    }
                    if let Some(metadata) =
                        names.get(entry.value["id"].as_str().unwrap_or_default())
                    {
                        for field in ["name", "preview", "updatedAt", "isPinned"] {
                            entry.value[field] = metadata[field].clone();
                        }
                    }
                    entry.value["historyStore"] = json!(store_id(root));
                    entry.value["historyHome"] = json!(label);
                    self.entries.insert(path, entry);
                }
            }
            if skipped > 0 {
                self.warnings
                    .push(format!("{label}: {skipped} saved files could not be read."));
            }
            self.add_metadata(root, label, names, &mut seen);
        }
        self.entries.retain(|path, _| seen.contains(path));
        self.refreshed = Some(Instant::now());
        Ok(())
    }

    // Newer clients can persist thread metadata and items before a rollout is present.
    fn add_metadata(
        &mut self,
        root: &Path,
        label: &str,
        names: BTreeMap<String, Value>,
        seen: &mut BTreeSet<PathBuf>,
    ) {
        let ids: BTreeSet<_> = seen
            .iter()
            .filter_map(|path| self.entries.get(path))
            .filter(|entry| entry.home == *root)
            .filter_map(|entry| entry.value["id"].as_str())
            .map(str::to_owned)
            .collect();
        for (id, mut value) in names {
            if ids.contains(&id) || uuid::Uuid::parse_str(&id).is_err() {
                continue;
            }
            value["id"] = json!(id);
            value["historyStore"] = json!(store_id(root));
            value["historyHome"] = json!(label);
            value["metadataOnly"] = json!(true);
            value["status"] = json!({"type":"notLoaded"});
            let path = root.join(format!(".history-metadata-{id}"));
            seen.insert(path.clone());
            self.entries.insert(
                path.clone(),
                Entry {
                    path,
                    home: root.to_path_buf(),
                    modified: None,
                    size: 0,
                    value,
                },
            );
        }
    }

    fn list(&self, params: &Value) -> Result<Value> {
        let limit = params["limit"].as_u64().unwrap_or(50).clamp(1, 100) as usize;
        let search = params["searchTerm"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        let cwd = params["cwd"].as_str().unwrap_or_default();
        let archived = params["archived"].as_bool().unwrap_or(false);
        let signature = json!([search, cwd, archived]).to_string();
        let after = params["cursor"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|cursor| {
                let value: Value = serde_json::from_str(cursor).map_err(error)?;
                if value["query"] != signature {
                    return Err(error("history filters changed; reload the first page"));
                }
                Ok((
                    value["time"]
                        .as_i64()
                        .ok_or_else(|| error("invalid history cursor"))?,
                    value["key"]
                        .as_str()
                        .ok_or_else(|| error("invalid history cursor"))?
                        .to_owned(),
                ))
            })
            .transpose()?;
        let mut entries: Vec<_> = self
            .entries
            .values()
            .filter(|entry| {
                let chat = &entry.value;
                chat["archived"] == archived
                    && (cwd.is_empty() || chat["cwd"] == cwd)
                    && ["name", "preview", "cwd", "historyHome"]
                        .iter()
                        .any(|field| {
                            chat[field]
                                .as_str()
                                .unwrap_or_default()
                                .to_lowercase()
                                .contains(&search)
                        })
            })
            .collect();
        let key = |entry: &Entry| {
            (
                entry.value["updatedAt"].as_i64().unwrap_or_default(),
                format!(
                    "{}/{}",
                    entry.value["historyStore"].as_str().unwrap_or_default(),
                    entry.value["id"].as_str().unwrap_or_default()
                ),
            )
        };
        entries.sort_by_key(|entry| std::cmp::Reverse(key(entry)));
        let mut unique = BTreeSet::new();
        entries.retain(|entry| unique.insert(key(entry).1));
        let total = entries.len();
        entries.retain(|entry| after.as_ref().is_none_or(|after| key(entry) < *after));
        let more = entries.len() > limit;
        entries.truncate(limit);
        let cursor = if more {
            entries.last().map(|entry| {
                let (time, key) = key(entry);
                json!({"query":signature,"time":time,"key":key}).to_string()
            })
        } else {
            None
        };
        let data: Vec<_> = entries.into_iter().map(|entry| json!({
            "session":{"id":entry.value["historyStore"],"name":entry.value["historyHome"],"cwd":entry.value["cwd"],
                "executionCwd":entry.value["cwd"],"kind":"history","profile":null,"capabilities":[],"status":"saved"},
            "chat":entry.value
        })).collect();
        let paths: BTreeSet<_> = self
            .entries
            .values()
            .filter_map(|entry| entry.value["cwd"].as_str())
            .collect();
        let projects: Vec<_> = paths.into_iter().map(|path| json!({"id":format!("history-project-{}", &blake3::hash(path.as_bytes()).to_hex()[..24]),
            "name":Path::new(path).file_name().map_or_else(|| path.to_owned(), |name| name.to_string_lossy().into_owned()),"path":path,"historyOnly":true})).collect();
        Ok(
            json!({"data":data,"nextCursor":cursor,"total":total,"warnings":self.warnings,"projects":projects}),
        )
    }

    fn find(&self, session: &str, thread: &str) -> Result<Entry> {
        self.entries
            .values()
            .filter(|entry| entry.value["historyStore"] == session && entry.value["id"] == thread)
            .max_by_key(|entry| entry.value["updatedAt"].as_i64().unwrap_or_default())
            .cloned()
            .ok_or_else(|| error("saved chat is no longer available; refresh history"))
    }
}

pub async fn list(state: &Arc<State>, params: Value) -> Result<Value> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        let mut history = state
            .history
            .lock()
            .map_err(|_| error("history lock poisoned"))?;
        if history
            .refreshed
            .is_none_or(|time| time.elapsed() > Duration::from_secs(10))
            || params["refresh"] == true
        {
            history.refresh(&homes(&state)?)?;
        }
        history.list(&params)
    })
    .await
    .map_err(error)?
}

pub async fn rpc(state: &Arc<State>, session: &str, method: &str, params: &Value) -> Result<Value> {
    let history = state.history.clone();
    let session = session.to_owned();
    let method = method.to_owned();
    let params = params.clone();
    tokio::task::spawn_blocking(move || {
        let entry = history.lock().map_err(|_| error("history lock poisoned"))?
            .find(&session, params["threadId"].as_str().unwrap_or_default())?;
        match method.as_str() {
            "thread/read" if params["includeTurns"] != true => Ok(json!({"thread":entry.value})),
            "thread/snapshot" => {
                let items = items(&entry, &json!({"limit":100}))?;
                Ok(json!({"sessionId":entry.value["historyStore"],"recovered":true,
                    "warning":"Saved local history · read-only. Open this chat in its source app to continue work. No running session is changed.",
                    "turns":{"data":[]},"items":items}))
            }
            "thread/items/list" => items(&entry, &params),
            "thread/turns/list" => Ok(json!({"data":[],"nextCursor":null})),
            _ => Err(error("saved history is read-only; use its source app or a connected project session")),
        }
    }).await.map_err(error)?
}

fn items(entry: &Entry, params: &Value) -> Result<Value> {
    let limit = params["limit"].as_u64().unwrap_or(100).clamp(1, 100) as usize;
    let cursor = params["cursor"].as_str().unwrap_or_default();
    let ascending = params["sortDirection"] == "asc";
    if !cursor.starts_with("file:")
        && let Ok(Some(db)) = database(&entry.home, "thread_history_")
    {
        let read = || -> Result<Value> {
            let before = if cursor.is_empty() {
                if ascending { -1 } else { i64::MAX }
            } else {
                cursor
                    .strip_prefix("db:")
                    .ok_or_else(|| error("invalid item cursor"))?
                    .parse()
                    .map_err(error)?
            };
            let sql = if ascending {
                "SELECT rollout_ordinal, CASE WHEN length(item_json)<=2097152 THEN item_json END FROM thread_items WHERE thread_id=?1 AND rollout_ordinal>?2 ORDER BY rollout_ordinal ASC LIMIT ?3"
            } else {
                "SELECT rollout_ordinal, CASE WHEN length(item_json)<=2097152 THEN item_json END FROM thread_items WHERE thread_id=?1 AND rollout_ordinal<?2 ORDER BY rollout_ordinal DESC LIMIT ?3"
            };
            let mut statement = db.prepare(sql).map_err(error)?;
            let rows = statement
                .query_map(
                    rusqlite::params![entry.value["id"].as_str(), before, limit + 1],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                )
                .map_err(error)?;
            let mut page = Vec::new();
            let mut bytes = 0;
            let mut more = false;
            for row in rows {
                let row = row.map_err(error)?;
                if page.len() == limit || bytes + row.1.len() > 4 * 1024 * 1024 {
                    more = true;
                    break;
                }
                bytes += row.1.len();
                page.push(row);
            }
            let rows = page;
            let next = if more {
                rows.last().map(|(ordinal, _)| format!("db:{ordinal}"))
            } else {
                None
            };
            let data = rows
                .into_iter()
                .map(|(_, value)| {
                    serde_json::from_str::<Value>(&value)
                        .map(|item| json!({"item":item}))
                        .map_err(error)
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(json!({"data":data,"nextCursor":next}))
        };
        match read() {
            Ok(page)
                if !page["data"].as_array().is_none_or(Vec::is_empty)
                    || !cursor.is_empty()
                    || entry.value["metadataOnly"] == true =>
            {
                return Ok(page);
            }
            Err(failure) if !cursor.is_empty() => return Err(failure),
            _ => {}
        }
    }
    if cursor.starts_with("db:") {
        return Err(error(
            "saved item database is unavailable; reload this chat",
        ));
    }
    file_items(entry, limit, cursor, ascending)
}

fn file_items(entry: &Entry, limit: usize, cursor: &str, ascending: bool) -> Result<Value> {
    // Bounded reverse file pages also work when both SQLite indexes are unavailable.
    // Revalidate the discovered file; never accept a path from the mobile client.
    let canonical = std::fs::canonicalize(&entry.path).map_err(error)?;
    if canonical != entry.path || !canonical.starts_with(&entry.home) {
        return Err(error("saved history path changed"));
    }
    let mut file = File::open(&canonical).map_err(error)?;
    let length = file.metadata().map_err(error)?.len();
    if ascending {
        let start: u64 = if cursor.is_empty() {
            0
        } else {
            cursor
                .strip_prefix("file:")
                .ok_or_else(|| error("invalid item cursor"))?
                .parse()
                .map_err(error)?
        };
        if start > length {
            return Err(error("saved file changed; reload this chat"));
        }
        file.seek(SeekFrom::Start(start)).map_err(error)?;
        let mut bytes = Vec::new();
        file.take(16 * 1024 * 1024)
            .read_to_end(&mut bytes)
            .map_err(error)?;
        let mut offset = start;
        let mut data = Vec::new();
        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
            if line.last() != Some(&b'\n') && start + (bytes.len() as u64) < length {
                break;
            }
            let position = offset;
            offset += line.len() as u64;
            if let Ok(record) = serde_json::from_slice::<Value>(line)
                && let Some(item) =
                    saved_item(&record, entry.value["historyMode"] == "paginated", position)
            {
                data.push(json!({"item":item}));
                if data.len() == limit {
                    break;
                }
            }
        }
        if offset == start && start < length {
            return Err(error("saved record exceeds the 16 MiB read limit"));
        }
        return Ok(
            json!({"data":data,"nextCursor":(offset < length).then(|| format!("file:{offset}")),"savedFile":true}),
        );
    }
    let end = if cursor.is_empty() {
        length
    } else {
        cursor
            .strip_prefix("file:")
            .ok_or_else(|| error("invalid item cursor"))?
            .parse()
            .map_err(error)?
    };
    if end > length {
        return Err(error("saved file changed; reload this chat"));
    }
    let start = end.saturating_sub(16 * 1024 * 1024);
    file.seek(SeekFrom::Start(start)).map_err(error)?;
    let mut bytes = Vec::new();
    file.take(end - start)
        .read_to_end(&mut bytes)
        .map_err(error)?;
    let boundary = if start == 0 {
        0
    } else {
        bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |index| index + 1)
    };
    let mut offset = end;
    let mut data = Vec::new();
    for line in bytes[boundary..]
        .split_inclusive(|byte| *byte == b'\n')
        .rev()
    {
        offset -= line.len() as u64;
        if let Ok(record) = serde_json::from_slice::<Value>(line)
            && let Some(item) =
                saved_item(&record, entry.value["historyMode"] == "paginated", offset)
        {
            data.push(json!({"item":item}));
            if data.len() == limit {
                break;
            }
        }
    }
    if offset == end && end > 0 {
        return Err(error("saved record exceeds the 16 MiB read limit"));
    }
    Ok(
        json!({"data":data,"nextCursor":(offset > 0).then(|| format!("file:{offset}")),"savedFile":true}),
    )
}

fn saved_item(record: &Value, paginated: bool, offset: u64) -> Option<Value> {
    let payload = &record["payload"];
    if paginated {
        return (record["type"] == "event_msg" && payload["type"] == "item_completed").then(|| {
            let mut item = payload["item"].clone();
            if let Some(kind) = item["type"].as_str() {
                let mut chars = kind.chars();
                item["type"] = json!(
                    chars
                        .next()
                        .map(|c| c.to_lowercase().to_string())
                        .unwrap_or_default()
                        + chars.as_str()
                );
            }
            item
        });
    }
    if record["type"] != "response_item" || payload["type"] != "message" {
        return None;
    }
    let kind = match payload["role"].as_str()? {
        "user" => "userMessage",
        "assistant" => "agentMessage",
        _ => return None,
    };
    let text = payload["content"]
        .as_array()?
        .iter()
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Some(json!({"id":format!("saved-{offset}"),"type":kind,"text":text}))
}

#[cfg(test)]
pub(super) fn prepare_fixture(root: &Path) {
    let first = root.join("history-fixture/Host Codex");
    let second = root.join("history-fixture/Managed Codex");
    for home in [&first, &second] {
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        std::fs::create_dir_all(home.join("archived_sessions")).unwrap();
    }
    for index in 0..65 {
        let id = uuid::Uuid::from_u128(index + 1).to_string();
        let title = match index {
            0 => "VS Code saved fixture".to_owned(),
            1 => "Desktop Work saved fixture".to_owned(),
            2 => "Agent child saved fixture".to_owned(),
            _ => format!("Saved task {index:03}"),
        };
        let source = match index % 5 {
            0 => json!("vscode"),
            1 => json!("appServer"),
            2 => json!({"subagent":{"thread_spawn":{"parent_thread_id":"parent"}}}),
            3 => json!("exec"),
            _ => json!("cli"),
        };
        let text = [
            json!({"type":"session_meta","payload":{"id":id,"cwd":"/fixture/project","source":source}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":title}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Saved answer with selectable text."}]}}),
        ].into_iter().map(|value| value.to_string()+"\n").collect::<String>();
        let folder = if index == 64 {
            "archived_sessions"
        } else {
            "sessions"
        };
        std::fs::write(
            first
                .join(folder)
                .join(format!("rollout-fixture-{id}.jsonl")),
            &text,
        )
        .unwrap();
        if index == 0 {
            std::fs::write(
                second
                    .join(folder)
                    .join(format!("rollout-fixture-{id}.jsonl")),
                &text,
            )
            .unwrap();
        }
    }
    std::fs::write(first.join("state_5.sqlite"), b"damaged test index").unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, History, Vec<(String, PathBuf)>) {
        let root = tempfile::tempdir().unwrap();
        prepare_fixture(root.path());
        let roots = ["Host Codex", "Managed Codex"]
            .into_iter()
            .map(|name| {
                (
                    name.to_owned(),
                    std::fs::canonicalize(root.path().join("history-fixture").join(name)).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        let mut history = History::default();
        history.refresh(&roots).unwrap();
        (root, history, roots)
    }

    #[test]
    fn all_sources_and_homes_page_without_a_session_and_preserve_archives() {
        let (_root, history, _) = fixture();
        let first = history.list(&json!({"limit":50})).unwrap();
        assert_eq!(first["data"].as_array().unwrap().len(), 50);
        assert_eq!(first["total"], 65);
        assert_eq!(first["projects"].as_array().unwrap().len(), 1);
        assert_eq!(first["warnings"].as_array().unwrap().len(), 1);
        let second = history
            .list(&json!({"limit":50,"cursor":first["nextCursor"]}))
            .unwrap();
        assert_eq!(second["data"].as_array().unwrap().len(), 15);
        assert!(second["nextCursor"].is_null());
        let ids: BTreeSet<_> = first["data"]
            .as_array()
            .unwrap()
            .iter()
            .chain(second["data"].as_array().unwrap())
            .map(|row| json!([row["session"]["id"], row["chat"]["id"]]).to_string())
            .collect();
        assert_eq!(ids.len(), 65);
        assert_eq!(history.list(&json!({"archived":true})).unwrap()["total"], 1);
        assert!(
            history
                .list(&json!({"archived":true,"cursor":first["nextCursor"]}))
                .is_err()
        );
    }

    #[test]
    fn search_includes_other_clients_and_keeps_duplicate_ids_in_separate_homes() {
        let (_root, history, _) = fixture();
        let page = history.list(&json!({"searchTerm":"VS CODE"})).unwrap();
        assert_eq!(page["total"], 2);
        assert_eq!(page["data"][0]["chat"]["id"], page["data"][1]["chat"]["id"]);
        assert_ne!(
            page["data"][0]["session"]["id"],
            page["data"][1]["session"]["id"]
        );
        assert_eq!(
            history.list(&json!({"searchTerm":"Desktop Work"})).unwrap()["total"],
            1
        );
        assert_eq!(
            history.list(&json!({"searchTerm":"Agent child"})).unwrap()["total"],
            1
        );
        assert_eq!(
            history.list(&json!({"cwd":"/another/project"})).unwrap()["total"],
            0
        );
    }

    #[test]
    fn database_only_threads_keep_titles_and_complete_tool_items_in_both_directions() {
        let root = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(root.path()).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let db = Connection::open(path.join("state_5.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads (id TEXT, title TEXT, preview TEXT, updated_at INTEGER, is_pinned INTEGER, cwd TEXT, source TEXT, archived INTEGER, created_at INTEGER)").unwrap();
        db.execute("INSERT INTO threads VALUES (?1,'Desktop database chat','',20,1,'/project','vscode',0,10)",[&id]).unwrap();
        let transcript = Connection::open(path.join("thread_history_1.sqlite")).unwrap();
        transcript.execute_batch("CREATE TABLE thread_items (thread_id TEXT, rollout_ordinal INTEGER, item_json TEXT)").unwrap();
        for (index, kind) in [
            "userMessage",
            "reasoning",
            "commandExecution",
            "agentMessage",
        ]
        .into_iter()
        .enumerate()
        {
            transcript.execute("INSERT INTO thread_items VALUES (?1,?2,?3)", rusqlite::params![id,index,json!({"id":index.to_string(),"type":kind,"text":"test","aggregatedOutput":"tool output"}).to_string()]).unwrap();
        }
        let mut history = History::default();
        history.refresh(&[("Host".to_owned(), path)]).unwrap();
        let page = history.list(&json!({})).unwrap();
        assert_eq!(page["total"], 1);
        assert_eq!(page["data"][0]["chat"]["name"], "Desktop database chat");
        assert_eq!(page["data"][0]["chat"]["isPinned"], true);
        let entry = history.entries.values().next().unwrap();
        let newest = items(entry, &json!({"limit":2})).unwrap();
        assert_eq!(newest["data"][0]["item"]["type"], "agentMessage");
        assert_eq!(newest["data"][1]["item"]["aggregatedOutput"], "tool output");
        let older = items(entry, &json!({"limit":2,"cursor":newest["nextCursor"]})).unwrap();
        assert_eq!(older["data"][0]["item"]["type"], "reasoning");
        assert!(older["nextCursor"].is_null());
        let oldest = items(entry, &json!({"sortDirection":"asc","limit":1})).unwrap();
        assert_eq!(oldest["data"][0]["item"]["type"], "userMessage");
        assert_eq!(
            db.query_row("SELECT count(*) FROM threads", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn damaged_index_keeps_saved_answers_readable_and_item_pages_do_not_duplicate() {
        let (_root, history, _) = fixture();
        let row = history.list(&json!({"searchTerm":"Desktop Work"})).unwrap()["data"][0].clone();
        let entry = history
            .find(
                row["session"]["id"].as_str().unwrap(),
                row["chat"]["id"].as_str().unwrap(),
            )
            .unwrap();
        let first = items(&entry, &json!({"limit":1})).unwrap();
        assert_eq!(
            first["data"][0]["item"]["text"],
            "Saved answer with selectable text."
        );
        let next = items(&entry, &json!({"limit":1,"cursor":first["nextCursor"]})).unwrap();
        assert_eq!(next["data"][0]["item"]["type"], "userMessage");
        assert_ne!(
            first["data"][0]["item"]["id"],
            next["data"][0]["item"]["id"]
        );
        let exported = items(&entry, &json!({"sortDirection":"asc","limit":1})).unwrap();
        assert_eq!(exported["data"][0]["item"]["type"], "userMessage");
        let exported = items(
            &entry,
            &json!({"sortDirection":"asc","limit":1,"cursor":exported["nextCursor"]}),
        )
        .unwrap();
        assert_eq!(exported["data"][0]["item"]["type"], "agentMessage");
        assert!(exported["nextCursor"].is_null());
        assert!(
            history
                .find("history-wrong-home", entry.value["id"].as_str().unwrap())
                .is_err()
        );
    }

    #[test]
    fn changed_or_linked_files_are_not_followed_and_removed_chats_leave_the_cache() {
        let (_root, mut history, roots) = fixture();
        let path = history.entries.keys().next().unwrap().clone();
        let entry = history.entries.get(&path).unwrap().clone();
        let other = roots[0].1.join("unrelated.txt");
        std::fs::write(&other, "do not expose").unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&other, &path).unwrap();
        assert!(items(&entry, &json!({})).is_err());
        history.refresh(&roots).unwrap();
        assert!(!history.entries.contains_key(&path));
    }

    #[test]
    #[ignore = "explicit read-only audit of the user's local history; prints counts only"]
    fn local_history_audit() {
        let paths = std::env::var("CODEX_START_HISTORY_AUDIT").unwrap();
        let roots = paths
            .split(':')
            .map(|path| ("Local store".to_owned(), PathBuf::from(path)))
            .collect::<Vec<_>>();
        let mut history = History::default();
        let started = Instant::now();
        history.refresh(&roots).unwrap();
        let first = history.list(&json!({"limit":100})).unwrap();
        println!(
            "Local history: {} active copies, {} projects, {} warnings; first scan {:?}",
            first["total"],
            first["projects"].as_array().unwrap().len(),
            first["warnings"].as_array().unwrap().len(),
            started.elapsed()
        );
        let mut count = first["data"].as_array().unwrap().len();
        let mut cursor = first["nextCursor"].clone();
        while !cursor.is_null() {
            let page = history.list(&json!({"limit":100,"cursor":cursor})).unwrap();
            count += page["data"].as_array().unwrap().len();
            cursor = page["nextCursor"].clone();
        }
        assert_eq!(count as u64, first["total"].as_u64().unwrap());
        let entry = history.entries.values().next().unwrap();
        let page = items(entry, &json!({"limit":10})).unwrap();
        println!(
            "Paged all {count} active copies; sample transcript: {} items",
            page["data"].as_array().unwrap().len()
        );
    }
}
