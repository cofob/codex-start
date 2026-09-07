//! Bounded, device-scoped file transfers with checksums and atomic publication.

use super::{error, state::State};
use crate::error::Result;
use base64::{Engine, engine::general_purpose::STANDARD};
use codex_start_remote::DeviceId;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

const CHUNK: usize = 256 * 1024;
const MAX_FILE: u64 = 1024 * 1024 * 1024;

#[derive(Default)]
pub struct Transfers {
    entries: BTreeMap<String, Transfer>,
}
struct Transfer {
    device: DeviceId,
    file: PathBuf,
    session: String,
    destination: String,
    size: u64,
    offset: u64,
    digest: String,
    upload: bool,
    touched: Instant,
}
impl Drop for Transfer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
    }
}

fn field<'a>(params: &'a Value, name: &str) -> Result<&'a str> {
    params[name]
        .as_str()
        .filter(|s| !s.is_empty() && !s.contains('\0'))
        .ok_or_else(|| error(format!("missing or invalid {name}")))
}

pub async fn dispatch(
    state: &Arc<State>,
    device: &DeviceId,
    method: &str,
    params: &Value,
) -> Result<Value> {
    let mut transfers = state.files.lock().await;
    transfers
        .entries
        .retain(|_, value| value.touched.elapsed() < Duration::from_secs(1800));
    if matches!(method, "file/upload/start" | "file/download/start") {
        return start(state, device, method, params, &mut transfers).await;
    }
    let id = field(params, "transferId")?;
    let transfer = transfers
        .entries
        .get_mut(id)
        .filter(|t| &t.device == device)
        .ok_or_else(|| error("unknown or expired file transfer"))?;
    transfer.touched = Instant::now();
    match method {
        "file/status" => {
            Ok(json!({"offset":transfer.offset,"size":transfer.size,"sha256":transfer.digest}))
        }
        "file/close" => {
            transfers.entries.remove(id);
            Ok(json!({}))
        }
        "file/upload/chunk" if transfer.upload => {
            if params["offset"].as_u64() != Some(transfer.offset) {
                return Err(error(
                    "upload offset does not match; query file/status before continuing",
                ));
            }
            let encoded = field(params, "dataBase64")?;
            if encoded.len() > CHUNK * 4 / 3 + 4 {
                return Err(error("file chunk is too large"));
            }
            let bytes = STANDARD.decode(encoded).map_err(error)?;
            let end = transfer.offset + u64::try_from(bytes.len()).map_err(error)?;
            if bytes.len() > CHUNK || end > transfer.size {
                return Err(error("file chunk exceeds declared size"));
            }
            let mut file = tokio::fs::OpenOptions::new()
                .append(true)
                .open(&transfer.file)
                .await
                .map_err(error)?;
            file.write_all(&bytes).await.map_err(error)?;
            file.sync_data().await.map_err(error)?;
            transfer.offset = end;
            Ok(json!({"offset":end}))
        }
        "file/download/chunk" if !transfer.upload => {
            let offset = params["offset"]
                .as_u64()
                .filter(|n| *n <= transfer.size)
                .ok_or_else(|| error("invalid download offset"))?;
            let mut file = tokio::fs::File::open(&transfer.file).await.map_err(error)?;
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(error)?;
            let size =
                usize::try_from((transfer.size - offset).min(u64::try_from(CHUNK).map_err(error)?))
                    .map_err(error)?;
            let mut bytes = vec![0; size];
            file.read_exact(&mut bytes).await.map_err(error)?;
            Ok(json!({"offset":offset,"dataBase64":STANDARD.encode(bytes)}))
        }
        "file/upload/finish" if transfer.upload => {
            finish(state, id, transfer).await?;
            transfers.entries.remove(id);
            Ok(json!({"complete":true}))
        }
        _ => Err(error("invalid transfer operation")),
    }
}

async fn start(
    state: &Arc<State>,
    device: &DeviceId,
    method: &str,
    params: &Value,
    transfers: &mut Transfers,
) -> Result<Value> {
    if transfers.entries.len() >= 8
        || transfers
            .entries
            .values()
            .filter(|t| &t.device == device)
            .count()
            >= 4
    {
        return Err(error("too many file transfers"));
    }
    let session = field(params, "sessionId")?.to_owned();
    let destination = field(params, "path")?.to_owned();
    if !destination.starts_with('/') {
        return Err(error("file path must be absolute"));
    }
    let backend = super::sessions::get(state, &session).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let directory = state.root.join("transfers");
    crate::paths::create_private_dir(&directory)?;
    let path = directory.join(&id);
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .await
        .map_err(error)?;
    crate::paths::set_private_file(&path)?;
    let upload = method == "file/upload/start";
    let mut transfer = Transfer {
        device: device.clone(),
        file: path,
        session,
        destination,
        size: 0,
        offset: 0,
        digest: String::new(),
        upload,
        touched: Instant::now(),
    };
    if upload {
        transfer.size = params["size"]
            .as_u64()
            .filter(|n| *n <= MAX_FILE)
            .ok_or_else(|| error("file exceeds the 1 GiB transfer limit"))?;
        field(params, "sha256")?.clone_into(&mut transfer.digest);
        codex_start_remote::crypto::decode32(&transfer.digest).map_err(error)?;
    } else {
        let maximum = params
            .get("maximumBytes")
            .map_or(Some(MAX_FILE), Value::as_u64)
            .filter(|size| *size <= MAX_FILE)
            .ok_or_else(|| error("invalid download byte limit"))?;
        let mut command = backend.file_command("exec cat -- \"$1\"", &[&transfer.destination])?;
        let mut child = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(error)?;
        let mut stream = child
            .stdout
            .take()
            .ok_or_else(|| error("missing file stream"))?;
        let mut hash = Sha256::new();
        tokio::time::timeout(Duration::from_secs(600), async {
            let mut buffer = vec![0; CHUNK];
            loop {
                let count = stream.read(&mut buffer).await.map_err(error)?;
                if count == 0 {
                    break;
                }
                transfer.size += u64::try_from(count).map_err(error)?;
                if transfer.size > maximum {
                    return Err(error(format!(
                        "file exceeds the {maximum} byte preview or download limit"
                    )));
                }
                hash.update(&buffer[..count]);
                file.write_all(&buffer[..count]).await.map_err(error)?;
            }
            if !child.wait().await.map_err(error)?.success() {
                return Err(error("cannot read file in the session"));
            }
            Ok(())
        })
        .await
        .map_err(error)??;
        transfer.digest = hex::encode(hash.finalize());
    }
    file.sync_all().await.map_err(error)?;
    let result =
        json!({"transferId":id,"size":transfer.size,"sha256":transfer.digest,"chunkBytes":CHUNK});
    transfers.entries.insert(id, transfer);
    Ok(result)
}

async fn finish(state: &Arc<State>, id: &str, transfer: &Transfer) -> Result<()> {
    if transfer.offset != transfer.size {
        return Err(error("upload is incomplete"));
    }
    let mut file = tokio::fs::File::open(&transfer.file).await.map_err(error)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; CHUNK];
    loop {
        let n = file.read(&mut buffer).await.map_err(error)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    if hex::encode(hash.finalize()) != transfer.digest {
        return Err(error("upload checksum does not match"));
    }
    file.rewind().await.map_err(error)?;
    let backend = super::sessions::get(state, &transfer.session).await?;
    let temporary = format!("{}.codex-start-{id}", transfer.destination);
    let script =
        "umask 077; set -C; trap 'rm -f -- \"$1\"' EXIT; cat > \"$1\" && mv -f -- \"$1\" \"$2\"";
    let mut command = backend.file_command(script, &[&temporary, &transfer.destination])?;
    let mut child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(error)?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| error("missing file input"))?;
    tokio::time::timeout(Duration::from_secs(600), async {
        tokio::io::copy(&mut file, &mut input)
            .await
            .map_err(error)?;
        input.shutdown().await.map_err(error)?;
        drop(input);
        if !child.wait().await.map_err(error)?.success() {
            return Err(error(
                "file publication failed; inspect the destination before retrying",
            ));
        }
        Ok(())
    })
    .await
    .map_err(error)??;
    Ok(())
}
