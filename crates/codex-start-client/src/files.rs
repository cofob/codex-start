//! File transfer uses bounded chunks and never repeats an uncertain upload mutation.

use crate::{Client, Error};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
const CHUNK: usize = 256 * 1024;
const MAX_FILE: u64 = 1024 * 1024 * 1024;

fn transfer_id(value: &Value) -> Result<&str, Error> {
    value["transferId"]
        .as_str()
        .ok_or_else(|| Error::Message("missing transfer ID".into()))
}

impl Client {
    /// Upload a file, check its digest, then publish it at the destination.
    ///
    /// # Errors
    /// Returns an error if a local read, remote write, checksum, or connection fails.
    pub async fn upload_file(
        &self,
        session: &str,
        source: &Path,
        destination: &str,
    ) -> Result<(), Error> {
        let mut file = tokio::fs::File::open(source).await?;
        let size = file.metadata().await?.len();
        if size > MAX_FILE {
            return Err(Error::Message(
                "file exceeds the 1 GiB transfer limit".into(),
            ));
        }
        let mut hash = Sha256::new();
        let mut buffer = vec![0; CHUNK];
        loop {
            let n = file.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        file.rewind().await?;
        let started = self.request("file/upload/start", json!({"sessionId":session,"path":destination,"size":size,"sha256":hex::encode(hash.finalize())})).await?;
        let id = transfer_id(&started)?;
        let result = async {
            let mut offset = 0_u64;
            loop {
                let n = file.read(&mut buffer).await?; if n == 0 { break; }
                let response = self.request("file/upload/chunk", json!({"transferId":id,"offset":offset,"dataBase64":STANDARD.encode(&buffer[..n])})).await?;
                offset += u64::try_from(n).map_err(|_| Error::Message("file offset overflow".into()))?;
                if response["offset"].as_u64() != Some(offset) { return Err(Error::Message("upload offset does not match".into())); }
            }
            self.request("file/upload/finish", json!({"transferId":id})).await?;
            Ok(())
        }.await;
        if result.is_err() {
            let _ = self.request("file/close", json!({"transferId":id})).await;
        }
        result
    }

    /// Download a stable snapshot into a new local file and verify its checksum.
    ///
    /// # Errors
    /// Returns an error if the destination exists, the file is too large, or delivery fails.
    pub async fn download_file(
        &self,
        session: &str,
        source: &str,
        destination: &Path,
    ) -> Result<(), Error> {
        self.download_file_limited(session, source, destination, MAX_FILE)
            .await
    }

    /// Download a snapshot with a caller-defined byte limit for previews.
    ///
    /// # Errors
    /// Returns an error if the snapshot exceeds the limit or transfer verification fails.
    pub async fn download_file_limited(
        &self,
        session: &str,
        source: &str,
        destination: &Path,
        maximum_bytes: u64,
    ) -> Result<(), Error> {
        let maximum_bytes = maximum_bytes.min(MAX_FILE);
        let started = self
            .request(
                "file/download/start",
                json!({"sessionId":session,"path":source,"maximumBytes":maximum_bytes}),
            )
            .await?;
        let id = transfer_id(&started)?;
        let result = async {
            let size = started["size"]
                .as_u64()
                .filter(|n| *n <= maximum_bytes)
                .ok_or_else(|| Error::Message("invalid download size".into()))?;
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)
                .await?;
            let mut offset = 0_u64;
            let mut hash = Sha256::new();
            while offset < size {
                let result = self
                    .request(
                        "file/download/chunk",
                        json!({"transferId":id,"offset":offset}),
                    )
                    .await?;
                let encoded = result["dataBase64"]
                    .as_str()
                    .filter(|s| s.len() <= CHUNK * 4 / 3 + 4)
                    .ok_or_else(|| Error::Message("invalid download chunk".into()))?;
                let bytes = STANDARD
                    .decode(encoded)
                    .map_err(|e| Error::Message(e.to_string()))?;
                if bytes.is_empty()
                    || bytes.len() > CHUNK
                    || result["offset"].as_u64() != Some(offset)
                {
                    return Err(Error::Message("download sequence does not match".into()));
                }
                offset += u64::try_from(bytes.len())
                    .map_err(|_| Error::Message("file offset overflow".into()))?;
                if offset > size {
                    return Err(Error::Message("download exceeds declared size".into()));
                }
                hash.update(&bytes);
                file.write_all(&bytes).await?;
            }
            if started["sha256"].as_str() != Some(hex::encode(hash.finalize()).as_str()) {
                return Err(Error::Message("download checksum does not match".into()));
            }
            file.sync_all().await?;
            Ok(())
        }
        .await;
        let _ = self.request("file/close", json!({"transferId":id})).await;
        result
    }
}
