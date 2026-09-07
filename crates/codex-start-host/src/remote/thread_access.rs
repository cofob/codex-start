//! Read access does not acquire a thread writer lock.
use super::{error, sessions, state::State};
use crate::error::Result;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::io::AsyncReadExt;

pub async fn snapshot(state: &Arc<State>, session: &str, params: &Value) -> Result<Value> {
    let id = params["threadId"]
        .as_str()
        .ok_or_else(|| error("missing thread ID"))?;
    let backend = sessions::for_thread(state, session, id).await?;
    let mut result = async {
        let turns = backend
            .request(
                "thread/turns/list",
                json!({"threadId":id,"limit":1,"itemsView":"notLoaded","sortDirection":"desc"}),
            )
            .await?;
        let items = backend
            .request(
                "thread/items/list",
                json!({"threadId":id,"limit":100,"sortDirection":"desc"}),
            )
            .await?;
        Ok::<_, crate::error::HostError>(
            json!({"turns":turns,"items":items,"sessionId":backend.info.id,"recovered":false}),
        )
    }
    .await;
    if let Err(failure) = &result {
        let message = failure.to_string();
        let empty = message.contains("before first user message");
        if empty
            || message.contains("not supported yet")
            || message.contains("no persisted")
            || message.contains("missing source rollout")
        {
            result = legacy_snapshot(&backend, id, empty).await;
        }
    }
    match result {
        Ok(value) => Ok(value),
        Err(failure) if storage_error(&failure.to_string()) => {
            // The transcript remains useful when SQLite metadata cannot be decoded.
            // Do not change a live database or invent replacement metadata.
            uuid::Uuid::parse_str(id).map_err(error)?;
            let script = "find \"${CODEX_HOME:-$HOME/.codex}/sessions\" \"${CODEX_HOME:-$HOME/.codex}/archived_sessions\" -type f -name \"*${1}.jsonl\" -exec tail -c 16777216 {} \\;";
            let mut child = backend
                .file_command(script, &[id])?
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .map_err(error)?;
            let mut output = child
                .stdout
                .take()
                .ok_or_else(|| error("missing transcript stream"))?
                .take(16 * 1024 * 1024 + 1);
            let mut bytes = Vec::new();
            tokio::time::timeout(Duration::from_secs(15), output.read_to_end(&mut bytes))
                .await
                .map_err(error)?
                .map_err(error)?;
            if bytes.len() > 16 * 1024 * 1024 {
                return Err(error("transcript recovery exceeds 16 MiB"));
            }
            let items = transcript_items(&bytes);
            if items.is_empty() {
                return Err(failure);
            }
            Ok(
                json!({"sessionId":backend.info.id,"recovered":true,"warning":"Host history database is damaged. Showing saved transcript text in read-only mode. Host database repair is required before writing.","turns":{"data":[]},"items":{"data":items,"nextCursor":null}}),
            )
        }
        Err(failure) => Err(failure),
    }
}

async fn legacy_snapshot(backend: &sessions::Backend, id: &str, empty: bool) -> Result<Value> {
    // Only the server's explicit pre-first-message error permits a metadata-only
    // empty result. Missing lineage must still pass a full history read.
    let history = backend
        .request("thread/read", json!({"threadId":id,"includeTurns":!empty}))
        .await?;
    let turns = history["thread"]["turns"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let items: Vec<_> = turns
        .iter()
        .flat_map(|turn| turn["items"].as_array().into_iter().flatten())
        .map(|item| json!({"item":item}))
        .rev()
        .take(1000)
        .collect();
    Ok(
        json!({"turns":{"data":turns.into_iter().rev().take(1).collect::<Vec<_>>()},"items":{"data":items,"nextCursor":null},"sessionId":backend.info.id,"recovered":false}),
    )
}

fn storage_error(message: &str) -> bool {
    message.contains("history_mode") && message.contains("invalid utf-8")
        || message.contains("database disk image is malformed")
}

fn transcript_items(bytes: &[u8]) -> Vec<Value> {
    let mut messages = Vec::new();
    let mut legacy = Vec::new();
    for (index, line) in bytes.split(|b| *b == b'\n').enumerate() {
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let payload = &record["payload"];
        let (target, kind, mut text) = match record["type"].as_str() {
            Some("response_item") if payload["type"] == "message" => {
                let kind = match payload["role"].as_str() {
                    Some("user") => "userMessage",
                    Some("assistant") => "agentMessage",
                    _ => continue,
                };
                let text = payload["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|part| {
                        matches!(
                            part["type"].as_str(),
                            Some("input_text" | "output_text" | "text")
                        )
                    })
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                (&mut messages, kind, text)
            }
            Some("event_msg") => {
                let kind = match payload["type"].as_str() {
                    Some("user_message") => "userMessage",
                    Some("agent_message") => "agentMessage",
                    _ => continue,
                };
                (
                    &mut legacy,
                    kind,
                    payload["message"].as_str().unwrap_or_default().to_owned(),
                )
            }
            _ => continue,
        };
        if text.is_empty() {
            continue;
        }
        if text.len() > 512 * 1024 {
            let mut end = 512 * 1024;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push_str("\n[Recovered text truncated]");
        }
        target.push(json!({"item":{"id":format!("recovered-{index}"),"type":kind,"text":text}}));
    }
    // Legacy event records duplicate response items when both are present.
    let items = if messages.is_empty() {
        legacy
    } else {
        messages
    };
    let mut remaining = 4 * 1024 * 1024;
    items
        .into_iter()
        .rev()
        .take(1000)
        .take_while(|item| {
            let size = item.to_string().len();
            if size > remaining {
                return false;
            }
            remaining -= size;
            true
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_reads_text_and_ignores_partial_lines() {
        let bytes = b"broken\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hello\"}}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"reply\"}}\n";
        let items = transcript_items(bytes);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["item"]["text"], "reply");
        assert!(storage_error("error decoding history_mode: invalid utf-8"));
        assert!(!storage_error("thread already has an active writer"));
    }
    #[test]
    fn recovery_uses_response_messages_without_instruction_or_event_duplicates() {
        let records = [
            json!({"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"hidden instructions"}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"question"}]}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"question"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}}),
        ];
        let text = records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let items = transcript_items(text.as_bytes());
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["item"]["text"], "answer");
        assert_eq!(items[1]["item"]["text"], "question");
    }
}
