//! SQLite storage. Everything durable lives here: hosts, tokens, exec history,
//! audit trail. Deliberately dumb and synchronous — the hub handles tens of
//! hosts, not tens of thousands, and a mutex around a 1ms query beats a pool
//! abstraction nobody will read.

use anyhow::{Context, Result};
use hypha_proto::{now, EnrollTokenInfo, Host};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub struct Db {
    conn: std::sync::Mutex<Connection>,
}

fn parse_tags(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

fn join_tags(t: &[String]) -> String {
    t.iter()
        .map(|s| s.trim().replace(',', ""))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Self {
            conn: std::sync::Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS hosts (
                id         TEXT PRIMARY KEY,
                hostname   TEXT NOT NULL,
                pubkey     TEXT NOT NULL,
                os         TEXT NOT NULL DEFAULT '',
                arch       TEXT NOT NULL DEFAULT '',
                version    TEXT NOT NULL DEFAULT '',
                tags       TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL,
                last_seen  INTEGER NOT NULL DEFAULT 0,
                revoked    INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS enroll_tokens (
                token      TEXT PRIMARY KEY,
                tags       TEXT NOT NULL DEFAULT '',
                label      TEXT NOT NULL DEFAULT '',
                reusable   INTEGER NOT NULL DEFAULT 0,
                revoked    INTEGER NOT NULL DEFAULT 0,
                uses       INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL,
                used_at    INTEGER,
                used_by    TEXT
            );
            CREATE TABLE IF NOT EXISTS execs (
                id         TEXT PRIMARY KEY,
                command    TEXT NOT NULL,
                hosts      TEXT NOT NULL DEFAULT '',
                actor      TEXT NOT NULL DEFAULT 'admin',
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS audit (
                id     INTEGER PRIMARY KEY AUTOINCREMENT,
                ts     INTEGER NOT NULL,
                actor  TEXT NOT NULL DEFAULT '',
                action TEXT NOT NULL,
                host   TEXT NOT NULL DEFAULT '',
                detail TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS audit_ts ON audit (ts DESC);
            "#,
        )?;
        // Older databases predate reusable tokens. SQLite's ALTER TABLE is
        // cheap here; ignore "duplicate column" if we've already migrated.
        for (col, decl) in [
            ("label", "TEXT NOT NULL DEFAULT ''"),
            ("reusable", "INTEGER NOT NULL DEFAULT 0"),
            ("revoked", "INTEGER NOT NULL DEFAULT 0"),
            ("uses", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            conn.execute(&format!("ALTER TABLE enroll_tokens ADD COLUMN {col} {decl}"), []).ok();
        }
        Ok(())
    }

    // --- hosts ------------------------------------------------------------

    pub fn insert_host(&self, h: &Host, pubkey: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO hosts (id, hostname, pubkey, os, arch, version, tags, created_at, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            params![
                h.id,
                h.hostname,
                pubkey,
                h.os,
                h.arch,
                h.version,
                join_tags(&h.tags),
                h.created_at
            ],
        )?;
        Ok(())
    }

    pub fn pubkey_of(&self, id: &str) -> Option<String> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT pubkey FROM hosts WHERE id = ?1 AND revoked = 0",
                params![id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn touch_host(&self, id: &str, hostname: &str, version: &str) {
        self.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE hosts SET last_seen = ?2, hostname = ?3, version = ?4 WHERE id = ?1",
                params![id, now(), hostname, version],
            )
            .ok();
    }

    pub fn set_tags(&self, id: &str, tags: &[String]) {
        self.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE hosts SET tags = ?2 WHERE id = ?1",
                params![id, join_tags(tags)],
            )
            .ok();
    }

    pub fn delete_host(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM hosts WHERE id = ?1", params![id])?;
        conn.execute(
            "INSERT INTO audit (ts, actor, action, host, detail) VALUES (?1, 'admin', 'host.delete', ?2, '')",
            params![now(), id],
        )?;
        Ok(())
    }

    pub fn list_hosts(&self) -> Result<Vec<Host>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, hostname, os, arch, version, tags, created_at, last_seen
             FROM hosts WHERE revoked = 0 ORDER BY hostname ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Host {
                id: r.get(0)?,
                hostname: r.get(1)?,
                os: r.get(2)?,
                arch: r.get(3)?,
                version: r.get(4)?,
                tags: parse_tags(&r.get::<_, String>(5)?),
                created_at: r.get(6)?,
                last_seen: r.get(7)?,
                online: false,
                load: 0.0,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    // --- enroll tokens ----------------------------------------------------

    pub fn create_enroll_token(
        &self,
        token: &str,
        tags: &[String],
        label: &str,
        reusable: bool,
        ttl: i64,
    ) {
        let ts = now();
        self.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO enroll_tokens
                 (token, tags, label, reusable, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![token, join_tags(tags), label, reusable as i32, ts, ts + ttl],
            )
            .ok();
    }

    /// Validate a token. Single-use tokens are burned atomically; reusable ones
    /// are counted instead. Returns the tags to apply to the new host.
    pub fn consume_enroll_token(&self, token: &str, used_by: &str) -> Option<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let row: Option<(String, i64, Option<i64>, i64, i64)> = conn
            .query_row(
                "SELECT tags, expires_at, used_at, reusable, revoked
                 FROM enroll_tokens WHERE token = ?1",
                params![token],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .ok()
            .flatten();
        let (tags, expires_at, used_at, reusable, revoked) = row?;
        if revoked != 0 || expires_at < now() {
            return None;
        }
        if reusable == 0 {
            if used_at.is_some() {
                return None;
            }
            conn.execute(
                "UPDATE enroll_tokens SET used_at = ?2, used_by = ?3, uses = 1 WHERE token = ?1",
                params![token, now(), used_by],
            )
            .ok()?;
        } else {
            conn.execute(
                "UPDATE enroll_tokens SET uses = uses + 1, used_at = ?2, used_by = ?3 WHERE token = ?1",
                params![token, now(), used_by],
            )
            .ok()?;
        }
        Some(parse_tags(&tags))
    }

    pub fn list_enroll_tokens(&self) -> Vec<EnrollTokenInfo> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT token, tags, label, reusable, revoked, created_at,
                    expires_at, uses, used_at
             FROM enroll_tokens ORDER BY created_at DESC",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let rows = stmt.query_map([], |r| {
            Ok(EnrollTokenInfo {
                token: r.get(0)?,
                tags: parse_tags(&r.get::<_, String>(1)?),
                label: r.get(2)?,
                reusable: r.get::<_, i64>(3)? != 0,
                revoked: r.get::<_, i64>(4)? != 0,
                created_at: r.get(5)?,
                expires_at: r.get(6)?,
                uses: r.get(7)?,
                last_used_at: r.get(8)?,
                install_cmd: String::new(),
            })
        });
        match rows {
            Ok(r) => r.filter_map(|x| x.ok()).collect(),
            Err(_) => vec![],
        }
    }

    pub fn revoke_enroll_token(&self, token: &str) -> bool {
        self.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE enroll_tokens SET revoked = 1 WHERE token = ?1",
                params![token],
            )
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    // --- audit ------------------------------------------------------------

    pub fn audit(&self, action: &str, host: &str, detail: &str) {
        self.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO audit (ts, actor, action, host, detail) VALUES (?1, 'admin', ?2, ?3, ?4)",
                params![now(), action, host, detail],
            )
            .ok();
    }

    pub fn record_exec(&self, id: &str, command: &str, hosts: &str) {
        self.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO execs (id, command, hosts, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![id, command, hosts, now()],
            )
            .ok();
    }

    pub fn recent_execs(&self, limit: i64) -> Vec<serde_json::Value> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT id, command, hosts, created_at FROM execs ORDER BY created_at DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let rows = stmt.query_map(params![limit], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, String>(0)?,
                "command": r.get::<_, String>(1)?,
                "hosts": r.get::<_, String>(2)?,
                "created_at": r.get::<_, i64>(3)?,
            }))
        });
        match rows {
            Ok(r) => r.filter_map(|x| x.ok()).collect(),
            Err(_) => vec![],
        }
    }
}
