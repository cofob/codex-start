//! `SQLite` state. A transaction owns each registration and request transition.

use crate::{
    DeviceId, Error, Event, Registration, RegistrationResult, RequestId, SessionId, crypto, now,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

pub struct Store {
    db: Connection,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: DeviceId,
    pub name: String,
    pub public_key: String,
    pub created_at: u64,
    pub revoked: bool,
}

impl Store {
    ///
    /// # Errors
    /// Returns an error if validation, authentication, or the database operation fails.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let mut db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS devices (id TEXT PRIMARY KEY, name TEXT NOT NULL, public_key TEXT NOT NULL,
              token_hash TEXT NOT NULL, created_at INTEGER NOT NULL, revoked INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS pairing (id TEXT PRIMARY KEY, code TEXT UNIQUE NOT NULL, name TEXT NOT NULL,
              public_key TEXT NOT NULL, expires_at INTEGER NOT NULL, approved INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS events (sequence INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
              message TEXT NOT NULL, created_at INTEGER NOT NULL, dedup TEXT UNIQUE);
            CREATE TABLE IF NOT EXISTS event_keys (key TEXT PRIMARY KEY);
            CREATE TABLE IF NOT EXISTS journal_stats (bytes INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS event_age ON events(created_at);
            CREATE TABLE IF NOT EXISTS alerts (sequence INTEGER PRIMARY KEY, event TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS requests (device_id TEXT NOT NULL, request_id TEXT NOT NULL,
              payload_hash TEXT NOT NULL, outcome TEXT, PRIMARY KEY(device_id,request_id));")?;
        // SQLite length(TEXT) counts characters. Count UTF-8 bytes for the disk budget.
        // Migrate once, including old counters, without scanning history on each open.
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let migrated: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='journal_byte_counter_v1')",
            [],
            |row| row.get(0),
        )?;
        if !migrated {
            tx.execute_batch("DROP TRIGGER IF EXISTS journal_insert;
                DROP TRIGGER IF EXISTS journal_delete;
                DELETE FROM journal_stats;
                INSERT INTO journal_stats SELECT COALESCE(SUM(length(CAST(message AS BLOB))),0) FROM events;
                CREATE TRIGGER journal_insert AFTER INSERT ON events BEGIN UPDATE journal_stats SET bytes=bytes+length(CAST(NEW.message AS BLOB)); END;
                CREATE TRIGGER journal_delete AFTER DELETE ON events BEGIN UPDATE journal_stats SET bytes=bytes-length(CAST(OLD.message AS BLOB)); END;
                INSERT INTO metadata VALUES ('journal_byte_counter_v1','1');")?;
        }
        tx.commit()?;
        Ok(Self { db })
    }

    ///
    /// # Errors
    /// Returns an error if validation, authentication, or the database operation fails.
    /// Persist a received event and its reconnect cursor in the same transaction.
    ///
    /// # Errors
    /// Returns an error if the event cannot be encoded or committed.
    pub fn receive_event(&mut self, event: &Event) -> Result<bool, Error> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let cursor: u64 = tx.query_row(
            "SELECT COALESCE((SELECT CAST(value AS INTEGER) FROM metadata WHERE key='remote_cursor'),0)",
            [], |row| row.get(0),
        )?;
        if event.sequence <= cursor {
            return Ok(false);
        }
        let method = event.message["method"].as_str().unwrap_or_default();
        if matches!(
            method,
            "turn/completed" | "job/exited" | "serverRequest/resolved"
        ) || event.message.get("id").is_some()
        {
            tx.execute(
                "INSERT OR IGNORE INTO alerts VALUES (?1,?2)",
                params![event.sequence, serde_json::to_string(event)?],
            )?;
        }
        if method == "serverRequest/resolved" {
            let id = &event.message["params"]["requestId"];
            tx.execute("DELETE FROM alerts WHERE json_extract(event,'$.sessionId')=?1 AND json_extract(event,'$.message.id')=json_extract(?2,'$')", params![event.session_id.0, serde_json::to_string(id)?])?;
        }
        tx.execute("DELETE FROM alerts WHERE sequence NOT IN (SELECT sequence FROM alerts ORDER BY sequence DESC LIMIT 2048)", [])?;
        tx.execute("INSERT INTO metadata VALUES ('remote_cursor',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [event.sequence.to_string()])?;
        tx.commit()?;
        Ok(true)
    }

    /// Read alerts that Android has not yet delivered.
    ///
    /// # Errors
    /// Returns an error if the stored alerts cannot be read.
    pub fn alerts(&self) -> Result<Vec<Event>, Error> {
        let mut query = self
            .db
            .prepare("SELECT event FROM alerts ORDER BY sequence LIMIT 64")?;
        let rows = query.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    /// Confirm platform delivery after the notification has been posted.
    ///
    /// # Errors
    /// Returns an error if the acknowledgement cannot be committed.
    pub fn acknowledge_alert(&self, sequence: u64) -> Result<(), Error> {
        self.db
            .execute("DELETE FROM alerts WHERE sequence=?1", [sequence])?;
        Ok(())
    }

    /// Read or create a stable secret.
    ///
    /// # Errors
    /// Returns an error if private storage is unavailable.
    pub fn secret(&self, key: &str) -> Result<String, Error> {
        if matches!(key, "connection_password" | "group_password") {
            let current = self.value(key)?;
            if let Some(value) = current
                .as_ref()
                .filter(|v| crypto::decode_password(v).is_ok())
            {
                return Ok(value.clone());
            }
            // The compact format replaces old passwords, not identity keys or tokens.
            let tx = self.db.unchecked_transaction()?;
            let password = crypto::random_password();
            tx.execute("INSERT INTO metadata VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, password])?;
            if current.is_some() {
                tx.execute("DELETE FROM pairing", [])?;
            }
            tx.commit()?;
            return Ok(password);
        }
        self.db.execute(
            "INSERT OR IGNORE INTO metadata VALUES (?1,?2)",
            params![key, crypto::random_secret()],
        )?;
        Ok(self
            .db
            .query_row("SELECT value FROM metadata WHERE key=?1", [key], |r| {
                r.get(0)
            })?)
    }

    ///
    /// # Errors
    /// Returns an error if validation, authentication, or the database operation fails.
    pub fn value(&self, key: &str) -> Result<Option<String>, Error> {
        Ok(self
            .db
            .query_row("SELECT value FROM metadata WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    ///
    /// # Errors
    /// Returns an error if validation, authentication, or the database operation fails.
    pub fn set_value(&self, key: &str, value: &str) -> Result<(), Error> {
        self.db.execute("INSERT INTO metadata VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key,value])?;
        Ok(())
    }

    ///
    /// # Errors
    /// Returns an error if validation, authentication, or the database operation fails.
    pub fn rotate_password(&mut self) -> Result<(), Error> {
        let tx = self.db.transaction()?;
        tx.execute("INSERT INTO metadata VALUES ('connection_password',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [crypto::random_password()])?;
        tx.execute("DELETE FROM pairing", [])?;
        tx.commit()?;
        Ok(())
    }

    ///
    /// # Errors
    /// Returns an error if validation, authentication, or the database operation fails.
    pub fn register(
        &mut self,
        request: Registration,
        fingerprint: &str,
        nonce: &str,
    ) -> Result<RegistrationResult, Error> {
        crypto::decode32(&request.public_key)?;
        if request.name.trim().is_empty() || request.name.len() > 128 {
            return Err(Error::Invalid(
                "device name must contain 1–128 bytes".into(),
            ));
        }
        if let Some(password) = request.connection_password {
            let expected = self.secret("connection_password")?;
            if !constant_equal(password.as_bytes(), expected.as_bytes()) {
                return Err(Error::Unauthorized);
            }
            return self.issue_device(&request.name, &request.public_key);
        }
        let id = DeviceId::generate();
        let code = crypto::approval_code(&request.public_key, fingerprint, nonce);
        let expires = now() + 600;
        self.db
            .execute("DELETE FROM pairing WHERE expires_at<=?1", [now()])?;
        let count: u64 = self
            .db
            .query_row("SELECT COUNT(*) FROM pairing", [], |r| r.get(0))?;
        if count >= 64 {
            return Err(Error::Invalid("too many pending registrations".into()));
        }
        self.db.execute(
            "INSERT INTO pairing (id,code,name,public_key,expires_at) VALUES (?1,?2,?3,?4,?5)",
            params![id.0, code, request.name, request.public_key, expires],
        )?;
        Ok(RegistrationResult {
            device_id: id,
            code: Some(code),
            token: None,
            expires_at: Some(expires),
            event_cursor: self.bounds()?.1,
        })
    }

    fn issue_device(&self, name: &str, key: &str) -> Result<RegistrationResult, Error> {
        let id = DeviceId::generate();
        let token = crypto::random_secret();
        self.db.execute("INSERT INTO devices (id,name,public_key,token_hash,created_at) VALUES (?1,?2,?3,?4,?5)", params![id.0,name,key,crypto::digest(token.as_bytes()),now()])?;
        Ok(RegistrationResult {
            device_id: id,
            code: None,
            token: Some(token),
            expires_at: None,
            event_cursor: self.bounds()?.1,
        })
    }

    ///
    /// # Errors
    /// Returns an error if validation, authentication, or the database operation fails.
    pub fn approve(&self, code: &str) -> Result<(), Error> {
        if self.db.execute(
            "UPDATE pairing SET approved=1 WHERE code=?1 AND expires_at>?2",
            params![code.to_uppercase(), now()],
        )? != 1
        {
            return Err(Error::Invalid("pairing code is unknown or expired".into()));
        }
        Ok(())
    }

    ///
    /// # Errors
    /// Returns an error if validation, authentication, or the database operation fails.
    pub fn claim(
        &mut self,
        id: &DeviceId,
        nonce: &str,
        fingerprint: &str,
        signature: &str,
    ) -> Result<Option<RegistrationResult>, Error> {
        let row: Option<(String, String, bool)> = self
            .db
            .query_row(
                "SELECT name,public_key,approved FROM pairing WHERE id=?1 AND expires_at>?2",
                params![id.0, now()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (name, key, approved) = row.ok_or(Error::Unauthorized)?;
        crypto::verify(&key, signature, nonce, fingerprint)?;
        if !approved {
            return Ok(None);
        }
        let tx = self.db.transaction()?;
        let token = crypto::random_secret();
        tx.execute("INSERT INTO devices (id,name,public_key,token_hash,created_at) VALUES (?1,?2,?3,?4,?5)",params![id.0,name,key,crypto::digest(token.as_bytes()),now()])?;
        tx.execute("DELETE FROM pairing WHERE id=?1", [&id.0])?;
        tx.commit()?;
        Ok(Some(RegistrationResult {
            device_id: id.clone(),
            code: None,
            token: Some(token),
            expires_at: None,
            event_cursor: self.bounds()?.1,
        }))
    }

    ///
    /// # Errors
    /// Returns an error if validation, authentication, or the database operation fails.
    pub fn authenticate(
        &self,
        auth: &crate::Authentication,
        nonce: &str,
        fingerprint: &str,
    ) -> Result<(), Error> {
        let row: Option<(String, String)> = self
            .db
            .query_row(
                "SELECT public_key,token_hash FROM devices WHERE id=?1 AND revoked=0",
                [&auth.device_id.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (key, hash) = row.ok_or(Error::Unauthorized)?;
        if !constant_equal(
            hash.as_bytes(),
            crypto::digest(auth.token.as_bytes()).as_bytes(),
        ) {
            return Err(Error::Unauthorized);
        }
        crypto::verify(&key, &auth.signature, nonce, fingerprint)
    }

    ///
    /// # Errors
    /// Returns an error if validation, authentication, or the database operation fails.
    pub fn devices(&self) -> Result<Vec<Device>, Error> {
        let mut statement = self.db.prepare(
            "SELECT id,name,public_key,created_at,revoked FROM devices ORDER BY created_at",
        )?;
        Ok(statement
            .query_map([], |r| {
                Ok(Device {
                    id: DeviceId(r.get(0)?),
                    name: r.get(1)?,
                    public_key: r.get(2)?,
                    created_at: r.get(3)?,
                    revoked: r.get(4)?,
                })
            })?
            .collect::<Result<_, _>>()?)
    }

    ///
    /// # Errors
    /// Returns an error if validation, authentication, or the database operation fails.
    pub fn revoke(&self, id: &DeviceId) -> Result<(), Error> {
        if self
            .db
            .execute("UPDATE devices SET revoked=1 WHERE id=?1", [&id.0])?
            == 0
        {
            return Err(Error::Invalid("unknown device".into()));
        }
        Ok(())
    }

    ///
    /// # Errors
    /// Returns an error if validation, authentication, or the database operation fails.
    pub fn active(&self, id: &DeviceId) -> Result<bool, Error> {
        Ok(self
            .db
            .query_row("SELECT revoked=0 FROM devices WHERE id=?1", [&id.0], |r| {
                r.get(0)
            })
            .optional()?
            .unwrap_or(false))
    }

    ///
    /// # Errors
    /// Returns an error if validation, authentication, or the database operation fails.
    pub fn append(&self, session: &SessionId, message: &Value) -> Result<Option<Event>, Error> {
        let encoded = serde_json::to_string(message)?;
        if encoded.len() > crate::MAX_MESSAGE_BYTES {
            return Err(Error::Invalid("event is too large".into()));
        }
        let dedup = if message["method"] == "turn/completed" {
            message
                .pointer("/params/turn/id")
                .and_then(Value::as_str)
                .map(|id| format!("{}:{id}:completed", session.0))
        } else if message["method"] == "job/exited" {
            Some(format!("{}:job:exited", session.0))
        } else {
            None
        };
        let created = now();
        let tx = self.db.unchecked_transaction()?;
        if let Some(key) = &dedup
            && tx.execute("INSERT OR IGNORE INTO event_keys VALUES (?1)", [key])? == 0
        {
            return Ok(None);
        }
        tx.execute(
            "INSERT INTO events (session_id,message,created_at,dedup) VALUES (?1,?2,?3,?4)",
            params![session.0, encoded, created, dedup],
        )?;
        let sequence = u64::try_from(tx.last_insert_rowid())
            .map_err(|_| Error::Invalid("event sequence overflow".into()))?;
        tx.execute(
            "DELETE FROM events WHERE created_at<?1 OR sequence<=?2",
            params![
                created.saturating_sub(7 * 86400),
                sequence.saturating_sub(100_000)
            ],
        )?;
        // Each row update changes a byte counter. Retention does not scan the full journal.
        prune_journal(&tx, 104_857_600)?;
        tx.commit()?;
        Ok(Some(Event {
            sequence,
            session_id: session.clone(),
            message: message.clone(),
            created_at: created,
        }))
    }

    ///
    /// # Errors
    /// Returns an error if validation, authentication, or the database operation fails.
    pub fn events(&self, after: u64, limit: u32) -> Result<Vec<Event>, Error> {
        let mut q = self.db.prepare("SELECT sequence,session_id,message,created_at FROM events WHERE sequence>?1 ORDER BY sequence LIMIT ?2")?;
        let rows = q.query_map(params![after, limit.min(512)], |r| {
            Ok((
                r.get::<_, u64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, u64>(3)?,
            ))
        })?;
        let mut events = Vec::new();
        let mut bytes = 0;
        for row in rows {
            let (sequence, id, message, created_at) = row?;
            if !events.is_empty() && bytes + message.len() > 1024 * 1024 {
                break;
            }
            bytes += message.len();
            events.push(Event {
                sequence,
                session_id: SessionId(id),
                message: serde_json::from_str(&message)?,
                created_at,
            });
        }
        Ok(events)
    }

    ///
    /// # Errors
    /// Returns an error if validation, authentication, or the database operation fails.
    pub fn bounds(&self) -> Result<(u64, u64), Error> {
        Ok(self.db.query_row(
            "SELECT COALESCE(MIN(sequence),0),COALESCE(MAX(sequence),0) FROM events",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// Returns the previous result, or reserves a new operation before dispatch.
    ///
    /// # Errors
    /// Returns an error if validation, authentication, or the database operation fails.
    pub fn begin_request(
        &self,
        device: &DeviceId,
        id: &RequestId,
        payload: &Value,
    ) -> Result<Option<Value>, Error> {
        let hash = crypto::digest(serde_json::to_string(payload)?.as_bytes());
        let row: Option<(String, Option<String>)> = self
            .db
            .query_row(
                "SELECT payload_hash,outcome FROM requests WHERE device_id=?1 AND request_id=?2",
                params![device.0, id.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((prior, outcome)) = row {
            if prior != hash {
                return Err(Error::Invalid(
                    "request ID reused with different content".into(),
                ));
            }
            return match outcome {
                Some(value) => Ok(Some(serde_json::from_str(&value)?)),
                None => Err(Error::Invalid(
                    "delivery unknown; inspect task state before retrying".into(),
                )),
            };
        }
        self.db.execute(
            "INSERT INTO requests VALUES (?1,?2,?3,NULL)",
            params![device.0, id.0, hash],
        )?;
        Ok(None)
    }

    ///
    /// # Errors
    /// Returns an error if validation, authentication, or the database operation fails.
    pub fn finish_request(
        &self,
        device: &DeviceId,
        id: &RequestId,
        result: &Value,
    ) -> Result<(), Error> {
        self.db.execute(
            "UPDATE requests SET outcome=?3 WHERE device_id=?1 AND request_id=?2",
            params![device.0, id.0, serde_json::to_string(result)?],
        )?;
        Ok(())
    }
}

fn prune_journal(db: &Connection, maximum_bytes: u64) -> Result<(), Error> {
    while db.query_row("SELECT bytes FROM journal_stats", [], |row| {
        row.get::<_, u64>(0)
    })? > maximum_bytes
    {
        db.execute(
            "DELETE FROM events WHERE sequence=(SELECT MIN(sequence) FROM events)",
            [],
        )?;
    }
    Ok(())
}

fn constant_equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b)
        .fold(0_u8, |result, (a, b)| result | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    fn store() -> Store {
        Store::open(Path::new(":memory:")).unwrap()
    }

    #[test]
    fn rotation_preserves_devices_but_invalidates_invitations_and_pending_codes() {
        let mut db = store();
        let password = db.secret("connection_password").unwrap();
        let key = crypto::public_key(&crypto::random_secret()).unwrap();
        let request = Registration {
            name: "phone".into(),
            public_key: key,
            connection_password: Some(password.clone()),
        };
        let device = db
            .register(request.clone(), "fingerprint", "nonce")
            .unwrap();
        let mut pending = request.clone();
        pending.connection_password = None;
        let code = db
            .register(pending, "fingerprint", "nonce")
            .unwrap()
            .code
            .unwrap();
        db.rotate_password().unwrap();
        assert!(db.register(request, "fingerprint", "nonce").is_err());
        assert!(db.approve(&code).is_err());
        assert!(db.active(&device.device_id).unwrap());
        db.revoke(&device.device_id).unwrap();
        assert!(!db.active(&device.device_id).unwrap());
    }

    #[test]
    fn proof_binds_token_to_device_key_and_server() {
        let mut db = store();
        let key = crypto::random_secret();
        let password = db.secret("connection_password").unwrap();
        let result = db
            .register(
                Registration {
                    name: "phone".into(),
                    public_key: crypto::public_key(&key).unwrap(),
                    connection_password: Some(password),
                },
                "fp",
                "nonce",
            )
            .unwrap();
        let auth = crate::Authentication {
            device_id: result.device_id,
            token: result.token.unwrap(),
            signature: crypto::sign(&key, "challenge", "fp").unwrap(),
        };
        db.authenticate(&auth, "challenge", "fp").unwrap();
        assert!(db.authenticate(&auth, "different", "fp").is_err());
        assert!(db.authenticate(&auth, "challenge", "other host").is_err());
    }

    #[test]
    fn journal_and_request_outcomes_survive_restart_without_repeating_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let db = Store::open(&path).unwrap();
        let session = SessionId::generate();
        let device = DeviceId::generate();
        let id = RequestId::generate();
        let event =
            serde_json::json!({"method":"turn/completed","params":{"turn":{"id":"turn-1"}}});
        db.append(&session, &event).unwrap();
        assert!(db.append(&session, &event).unwrap().is_none());
        assert!(db.begin_request(&device, &id, &event).unwrap().is_none());
        drop(db);
        let db = Store::open(&path).unwrap();
        assert_eq!(db.events(0, 10).unwrap().len(), 1);
        assert!(db.begin_request(&device, &id, &event).is_err());
        db.finish_request(&device, &id, &serde_json::json!({"ok":true}))
            .unwrap();
        assert_eq!(
            db.begin_request(&device, &id, &event).unwrap(),
            Some(serde_json::json!({"ok":true}))
        );
    }

    #[test]
    fn unicode_journal_budget_migrates_old_counters_and_keeps_the_latest_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let db = Store::open(&path).unwrap();
        let session = SessionId::generate();
        let event = serde_json::json!({"method":"item/agentMessage/delta","params":{"delta":"🦀".repeat(64)}});
        let bytes = serde_json::to_vec(&event).unwrap().len() as u64;
        for _ in 0..3 {
            db.append(&session, &event).unwrap();
        }
        // Reproduce a database written by the earlier character-counting schema.
        db.db
            .execute_batch(
                "DELETE FROM metadata WHERE key='journal_byte_counter_v1';
            UPDATE journal_stats SET bytes=(SELECT SUM(length(message)) FROM events);",
            )
            .unwrap();
        drop(db);
        let db = Store::open(&path).unwrap();
        assert_eq!(
            db.db
                .query_row("SELECT bytes FROM journal_stats", [], |row| row
                    .get::<_, u64>(0))
                .unwrap(),
            bytes * 3
        );
        prune_journal(&db.db, bytes).unwrap();
        assert_eq!(db.bounds().unwrap(), (3, 3));
        assert_eq!(db.events(0, 10).unwrap()[0].message, event);
        drop(db);
        let db = Store::open(&path).unwrap();
        db.append(&session, &event).unwrap();
        assert_eq!(
            db.db
                .query_row("SELECT bytes FROM journal_stats", [], |row| row
                    .get::<_, u64>(0))
                .unwrap(),
            bytes * 2
        );
    }

    #[test]
    fn resolved_approval_keeps_a_durable_notification_dismissal() {
        let mut db = store();
        let session = SessionId::generate();
        let mut event = Event {
            sequence: 1,
            session_id: session,
            created_at: now(),
            message: serde_json::json!({"id":42,"method":"item/commandExecution/requestApproval","params":{}}),
        };
        db.receive_event(&event).unwrap();
        assert_eq!(db.alerts().unwrap().len(), 1);
        event.sequence = 2;
        event.message =
            serde_json::json!({"method":"serverRequest/resolved","params":{"requestId":42}});
        db.receive_event(&event).unwrap();
        let alerts = db.alerts().unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].sequence, 2);
        db.acknowledge_alert(2).unwrap();
        assert!(db.alerts().unwrap().is_empty());
    }

    #[test]
    fn replay_does_not_restore_acknowledged_alerts_or_move_the_cursor_backwards() {
        let mut db = store();
        let mut event = Event {
            sequence: 2,
            session_id: SessionId::generate(),
            created_at: now(),
            message: serde_json::json!({"method":"turn/completed","params":{"turn":{"id":"one"}}}),
        };
        assert!(db.receive_event(&event).unwrap());
        db.acknowledge_alert(2).unwrap();
        assert!(!db.receive_event(&event).unwrap());
        event.sequence = 1;
        assert!(!db.receive_event(&event).unwrap());
        assert!(db.alerts().unwrap().is_empty());
        assert_eq!(db.value("remote_cursor").unwrap().as_deref(), Some("2"));
    }
}
