use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const RETENTION_DAYS: i64 = 30;
const CAP_BYTES: u64 = 512 * 1024 * 1024;
const LEASE_MINUTES: i64 = 10;
const CURSOR_MINUTES: i64 = 30;

pub struct StoredRef {
    pub research_id: String,
    pub context_id: String,
    pub value: Value,
}

pub struct Store {
    conn: Connection,
    db_path: PathBuf,
    cap_bytes: u64,
}

type CursorRow = (String, Option<String>, Option<String>, String, i64, String);

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        secure_directory(root)?;
        let db_path = root.join("research.sqlite3");
        if let Ok(meta) = fs::symlink_metadata(&db_path) {
            validate_private(&meta, false)?;
        }
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&db_path)
            .with_context(|| format!("STORE_IO: create {}", db_path.display()))?;
        fs::set_permissions(&db_path, fs::Permissions::from_mode(0o600))?;
        let conn = Connection::open(&db_path).context("STORE_IO: open database")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "journal_size_limit", 0_i64)?;
        conn.pragma_update(None, "wal_autocheckpoint", 1_i64)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)
            .context("STORE_IO: initialize database")?;
        for suffix in ["-wal", "-shm"] {
            let path = PathBuf::from(format!("{}{}", db_path.display(), suffix));
            if path.exists() {
                fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            }
        }
        Ok(Self {
            conn,
            db_path,
            cap_bytes: CAP_BYTES,
        })
    }

    pub fn create_research(&mut self, title: &str, context: &Value) -> Result<String> {
        let context_id = string_field(context, "contextId")?;
        if title.is_empty() || title.chars().count() > 500 {
            return Err(anyhow!("INVALID_ARGUMENT: invalid research title"));
        }
        let context_json = canonical(context);
        let id = id("research");
        let now = now();
        let protected_until = lease_deadline();
        self.capacity_transaction(title.len() + context_json.len() + 1024, None, |tx| {
            tx.execute("INSERT INTO researches(id,title,context_id,context_json,created_at,updated_at,protected_until) VALUES(?,?,?,?,?,?,?)",
                params![id, title, context_id, context_json, now, now, protected_until])?;
            Ok(())
        })?;
        Ok(id)
    }

    pub fn ensure_research(&mut self, id: &str, context_id: &str) -> Result<()> {
        let found: Option<String> = self
            .conn
            .query_row("SELECT context_id FROM researches WHERE id=?", [id], |r| {
                r.get(0)
            })
            .optional()?;
        match found {
            None => Err(anyhow!("RESEARCH_EXPIRED: research is unavailable")),
            Some(v) if v != context_id => Err(anyhow!(
                "CONTEXT_CHANGED: research belongs to another context"
            )),
            Some(_) => Ok(()),
        }
    }

    pub fn research_context(&self, id: &str) -> Result<Value> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT context_json FROM researches WHERE id=?",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        raw.map(|v| serde_json::from_str(&v).context("STORE_CORRUPT: invalid context"))
            .unwrap_or_else(|| Err(anyhow!("RESEARCH_EXPIRED: research is unavailable")))
    }

    pub fn meta(&self, key: &str) -> Result<Option<Value>> {
        let raw: Option<String> = self
            .conn
            .query_row("SELECT value_json FROM meta WHERE key=?", [key], |r| {
                r.get(0)
            })
            .optional()?;
        raw.map(|v| serde_json::from_str(&v).context("STORE_CORRUPT: invalid metadata"))
            .transpose()
    }

    pub fn set_meta(&mut self, key: &str, value: &Value) -> Result<()> {
        let value = canonical(value);
        self.capacity_transaction(key.len() + value.len() + 256, None, |tx| {
            tx.execute("INSERT INTO meta(key,value_json) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json", params![key, value])?;
            Ok(())
        })
    }

    pub fn put_ref(
        &mut self,
        research_id: &str,
        kind: &str,
        value: &Value,
        ttl_seconds: Option<u64>,
    ) -> Result<String> {
        let value = canonical(value);
        let context_id: String = self
            .conn
            .query_row(
                "SELECT context_id FROM researches WHERE id=?",
                [research_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("RESEARCH_EXPIRED: research is unavailable"))?;
        let ref_id = id("ref");
        let expires = ttl_seconds.map(|s| {
            (Utc::now() + Duration::seconds(s.min(i64::MAX as u64) as i64))
                .to_rfc3339_opts(SecondsFormat::Millis, true)
        });
        self.capacity_transaction(value.len() + kind.len() + 512, Some(research_id), |tx| {
            tx.execute("INSERT INTO refs(id,research_id,context_id,kind,value_json,expires_at) VALUES(?,?,?,?,?,?)",
                params![ref_id,research_id,context_id,kind,value,expires])?;
            Ok(())
        })?;
        Ok(ref_id)
    }

    pub fn get_ref(&self, id: &str, kind: &str) -> Result<StoredRef> {
        let row: Option<(String,String,String,Option<String>)> = self.conn.query_row(
            "SELECT research_id,context_id,value_json,expires_at FROM refs WHERE id=? AND kind=?", params![id,kind],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        let (research_id, context_id, raw, expires) =
            row.ok_or_else(|| anyhow!("INVALID_REFERENCE: reference is unavailable"))?;
        if expires.as_deref().is_some_and(|v| v <= now().as_str()) {
            return Err(anyhow!("INVALID_REFERENCE: reference expired"));
        }
        if !self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM researches WHERE id=?)",
            [&research_id],
            |r| r.get::<_, bool>(0),
        )? {
            return Err(anyhow!("RESEARCH_EXPIRED: research is unavailable"));
        }
        Ok(StoredRef {
            research_id,
            context_id,
            value: serde_json::from_str(&raw).context("STORE_CORRUPT: invalid reference")?,
        })
    }

    pub fn record(
        &mut self,
        research_id: &str,
        kind: &str,
        summary: &str,
        product_refs: &[String],
        evidence: &[Value],
    ) -> Result<()> {
        let context_id = research_context_id(&self.conn, research_id)?;
        let observed = evidence
            .iter()
            .filter_map(|v| v.get("observedAt").and_then(Value::as_str))
            .filter_map(normalize_time)
            .min()
            .unwrap_or_else(now);
        let mut prepared_evidence = Vec::new();
        for input in evidence {
            let obj = input
                .as_object()
                .ok_or_else(|| anyhow!("INVALID_ARGUMENT: evidence must be objects"))?;
            let eid = obj
                .get("evidenceRef")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| id("evidence"));
            let evidence_context = obj
                .get("contextId")
                .and_then(Value::as_str)
                .unwrap_or(&context_id);
            if evidence_context != context_id {
                return Err(anyhow!(
                    "CONTEXT_CHANGED: evidence belongs to another context"
                ));
            }
            let source_kind = obj
                .get("sourceKind")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("INVALID_ARGUMENT: evidence sourceKind is required"))?;
            if !matches!(source_kind, "ozon_page" | "local_journal") {
                return Err(anyhow!("INVALID_ARGUMENT: invalid evidence sourceKind"));
            }
            let source_url = obj.get("sourceUrl").cloned().unwrap_or(Value::Null);
            if source_kind == "local_journal" && !source_url.is_null() {
                return Err(anyhow!(
                    "INVALID_ARGUMENT: local journal evidence cannot have sourceUrl"
                ));
            }
            let value = json!({"evidenceRef":eid.clone(),"sourceKind":source_kind,"sourceUrl":source_url,
                "observedAt":obj.get("observedAt").and_then(Value::as_str).and_then(normalize_time).unwrap_or_else(||observed.clone()),
                "contextId":context_id.clone(),"sku":obj.get("sku").cloned().unwrap_or(Value::Null),
                "facts":obj.get("facts").cloned().unwrap_or_else(||json!([]))});
            prepared_evidence.push((eid, canonical(&value)));
        }
        let estimated = summary.len()
            + product_refs.iter().map(String::len).sum::<usize>()
            + prepared_evidence
                .iter()
                .map(|(id, value)| id.len() + value.len())
                .sum::<usize>()
            + 2048;
        self.capacity_transaction(estimated, Some(research_id), |tx| {
            let mut evidence_ids = Vec::with_capacity(prepared_evidence.len());
            for (eid, value) in &prepared_evidence {
                tx.execute("INSERT INTO evidence(id,research_id,seq,value_json) VALUES(?,?,(SELECT COALESCE(MAX(seq),0)+1 FROM evidence WHERE research_id=?),?)",
                    params![eid,research_id,research_id,value])?;
                evidence_ids.push(eid.clone());
            }
            let chunks: Vec<&[String]> = if product_refs.is_empty() {
                vec![&[]]
            } else {
                product_refs.chunks(36).collect()
            };
            for chunk in chunks {
                tx.execute("INSERT INTO events(id,research_id,seq,kind,observed_at,summary,product_refs,evidence_refs) VALUES(?,?,(SELECT COALESCE(MAX(seq),0)+1 FROM events WHERE research_id=?),?,?,?,?,?)",
                    params![id("event"),research_id,research_id,kind,observed,summary,canonical(&json!(chunk)),canonical(&json!(evidence_ids))])?;
            }
            tx.execute(
                "UPDATE researches SET updated_at=? WHERE id=?",
                params![now(), research_id],
            )?;
            Ok(())
        })
    }

    pub fn append_note(&mut self, args: &Value) -> Result<Value> {
        let research_id = string_field(args, "researchId")?;
        let operation_id = string_field(args, "operationId")?;
        let kind = string_field(args, "kind")?;
        let text = string_field(args, "text")?;
        if !matches!(kind, "requirements" | "assessment" | "conclusion") || text.is_empty() {
            return Err(anyhow!("INVALID_ARGUMENT: invalid note"));
        }
        let payload = canonical(args);
        if let Some((note_id, created_at, previous)) = self
            .conn
            .query_row(
                "SELECT id,created_at,payload_json FROM notes WHERE research_id=? AND operation_id=?",
                params![research_id, operation_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
        {
            if previous != payload {
                return Err(anyhow!(
                    "CONFLICT: operationId was reused with a different payload"
                ));
            }
            return Ok(json!({"noteId":note_id,"createdAt":created_at,"author":"agent"}));
        }
        let evidence_refs = strings(args.get("evidenceRefs"))?;
        let product_refs = strings(args.get("productRefs"))?;
        research_context_id(&self.conn, research_id)?;
        validate_ids(&self.conn, "evidence", research_id, &evidence_refs)?;
        validate_product_refs(&self.conn, research_id, &product_refs)?;
        let note_id = id("note");
        let created = now();
        self.capacity_transaction(payload.len() + 2048, Some(research_id), |tx| {
            tx.execute("INSERT INTO notes(id,research_id,seq,operation_id,kind,text,created_at,evidence_refs,product_refs,payload_json) VALUES(?,?,(SELECT COALESCE(MAX(seq),0)+1 FROM notes WHERE research_id=?),?,?,?,?,?,?,?)",
                params![note_id,research_id,research_id,operation_id,kind,text,created,canonical(&json!(evidence_refs)),canonical(&json!(product_refs)),payload])?;
            tx.execute("INSERT INTO events(id,research_id,seq,kind,observed_at,summary,product_refs,evidence_refs) VALUES(?,?,(SELECT COALESCE(MAX(seq),0)+1 FROM events WHERE research_id=?),'note_appended',?,?,?,?)",
                params![id("event"),research_id,research_id,created,format!("Agent {kind} note appended"),canonical(&json!(product_refs)),canonical(&json!(evidence_refs))])?;
            tx.execute(
                "UPDATE researches SET updated_at=? WHERE id=?",
                params![created, research_id],
            )?;
            Ok(())
        })?;
        Ok(json!({"noteId":note_id,"createdAt":created,"author":"agent"}))
    }

    pub fn list(&mut self, args: &Value) -> Result<Value> {
        let query = args.get("query").and_then(Value::as_str).map(str::to_owned);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(10)
            .clamp(1, 50) as usize;
        let (cutoff, offset) = self
            .cursor(args.get("cursor"), "list", None, query.as_deref())?
            .unwrap_or((max_updated(&self.conn)?, 0));
        let needle = query
            .as_ref()
            .map(|q| format!("%{}%", q.replace('%', "\\%").replace('_', "\\_")));
        let mut stmt=self.conn.prepare("SELECT id,title,created_at,updated_at,context_id FROM researches r WHERE updated_at<=? AND (? IS NULL OR title LIKE ? ESCAPE '\\' OR EXISTS(SELECT 1 FROM notes n WHERE n.research_id=r.id AND n.text LIKE ? ESCAPE '\\')) ORDER BY updated_at DESC,id DESC LIMIT ? OFFSET ?")?;
        let rows=stmt.query_map(params![cutoff,needle,needle,needle,(limit+1) as i64,offset as i64],|r|Ok(json!({"researchId":r.get::<_,String>(0)?,"title":r.get::<_,String>(1)?,"createdAt":r.get::<_,String>(2)?,"updatedAt":r.get::<_,String>(3)?,"contextId":r.get::<_,String>(4)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let more = rows.len() > limit;
        let researches = rows.into_iter().take(limit).collect::<Vec<_>>();
        drop(stmt);
        let next = if more {
            Some(self.new_cursor("list", None, query.as_deref(), &cutoff, offset + limit)?)
        } else {
            None
        };
        Ok(json!({"researches":researches,"nextCursor":next}))
    }

    pub fn read(&mut self, args: &Value) -> Result<Value> {
        let rid = string_field(args, "researchId")?.to_owned();
        research_context_id(&self.conn, &rid)?;
        let section = args
            .get("section")
            .and_then(Value::as_str)
            .unwrap_or("summary");
        if section == "summary" {
            return Ok(
                json!({"section":"summary","payload":self.summary(&rid)?,"nextCursor":null}),
            );
        }
        let cap = match section {
            "events" => 25,
            "evidence" => 20,
            "notes" => 10,
            _ => return Err(anyhow!("INVALID_ARGUMENT: unknown research section")),
        };
        let filter = if section == "evidence" {
            args.get("evidenceRefs").map(canonical)
        } else {
            None
        };
        if section == "evidence" {
            validate_selection(&self.conn, &rid, args.get("evidenceRefs"))?;
        }
        let (cutoff, offset) =
            match self.cursor(args.get("cursor"), section, Some(&rid), filter.as_deref())? {
                Some((cutoff, offset)) => (
                    cutoff
                        .parse::<i64>()
                        .map_err(|_| anyhow!("STORE_CORRUPT: invalid cursor cutoff"))?,
                    offset,
                ),
                None => (max_seq(&self.conn, section, &rid)?, 0),
            };
        let mut values = read_section(
            &self.conn,
            section,
            &rid,
            cutoff,
            offset,
            cap + 1,
            args.get("evidenceRefs"),
        )?;
        let more = values.len() > cap;
        values.truncate(cap);
        let next = if more {
            Some(self.new_cursor(
                section,
                Some(&rid),
                filter.as_deref(),
                &cutoff.to_string(),
                offset + cap,
            )?)
        } else {
            None
        };
        Ok(json!({"section":section,"payload":values,"nextCursor":next}))
    }

    pub fn lease(&mut self, id: &str) -> Result<()> {
        research_context_id(&self.conn, id)?;
        self.capacity_transaction(256, Some(id), |_| Ok(()))
    }

    pub fn maintain(&mut self) -> Result<()> {
        let maintenance_started = now();
        let cutoff = (Utc::now() - Duration::days(RETENTION_DAYS))
            .to_rfc3339_opts(SecondsFormat::Millis, true);
        let current = now();
        self.capacity_transaction(0, None, |tx| {
            tx.execute(
                "DELETE FROM refs WHERE expires_at IS NOT NULL AND expires_at<=?",
                [&maintenance_started],
            )?;
            tx.execute("DELETE FROM cursors WHERE expires_at<=?", [&maintenance_started])?;
            tx.execute("DELETE FROM researches WHERE updated_at<? AND (protected_until IS NULL OR protected_until<=?)",params![cutoff,current])?;
            Ok(())
        })
    }

    fn capacity_transaction<T>(
        &mut self,
        additional: usize,
        protect: Option<&str>,
        write: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let additional = u64::try_from(additional)
            .map_err(|_| anyhow!("STORAGE_FULL: write size exceeds the store budget"))?;
        let target = self
            .cap_bytes
            .checked_sub(additional)
            .ok_or_else(|| anyhow!("STORAGE_FULL: write size exceeds the store budget"))?;
        self.checkpoint_wal()?;
        let page_size = page_size(&self.conn)?;
        let max_pages = self.cap_bytes / page_size;
        if max_pages == 0 {
            return Err(anyhow!("STORAGE_FULL: write size exceeds the store budget"));
        }
        let max_pages = i64::try_from(max_pages).unwrap_or(i64::MAX);
        self.conn
            .pragma_update(None, "max_page_count", max_pages)
            .context("STORE_IO: set journal page bound")?;
        let tx = self.conn.transaction()?;
        if let Some(id) = protect {
            tx.execute(
                "UPDATE researches SET protected_until=? WHERE id=?",
                params![lease_deadline(), id],
            )?;
        }
        let result = (|| {
            let mut evicted = Self::enforce_capacity_tx(&tx, target)?;
            let value = write(&tx).map_err(map_sqlite_full)?;
            evicted |= Self::enforce_capacity_tx(&tx, self.cap_bytes)?;
            Ok((value, evicted))
        })();
        let (value, evicted) = match result {
            Ok(value) => {
                tx.commit().map_err(|error| map_sqlite_full(error.into()))?;
                value
            }
            Err(error) => {
                drop(tx);
                let _ = self.reclaim_space();
                return Err(error);
            }
        };
        // The commit is the success boundary. WAL autocheckpointing and a zero
        // journal-size limit normally make this cleanup immediate; failures are
        // retried before the next admission and must not turn a committed write
        // into a false failed result.
        let _ = self.checkpoint_wal();
        if evicted
            && self
                .storage_bytes()
                .is_ok_and(|bytes| bytes > self.cap_bytes)
        {
            let _ = self.reclaim_space();
        }
        Ok(value)
    }

    fn enforce_capacity_tx(tx: &Transaction<'_>, target: u64) -> Result<bool> {
        Self::enforce_capacity_tx_with_limit(tx, target, 256)
    }

    fn enforce_capacity_tx_with_limit(
        tx: &Transaction<'_>,
        target: u64,
        batch_limit: usize,
    ) -> Result<bool> {
        let mut evicted = false;
        while live_storage_bytes(tx)? > target {
            let deficit = (live_storage_bytes(tx)? - target).max(page_size(tx)?);
            let victims = victim_batch(tx, deficit, batch_limit)?;
            if victims.is_empty() {
                return Err(anyhow!(
                    "STORAGE_FULL: insufficient idle research can be reclaimed safely"
                ));
            }
            for id in victims {
                tx.execute("DELETE FROM researches WHERE id=?", [id])?;
            }
            evicted = true;
            // A small or scattered batch can leave page_count-freelist_count
            // unchanged. Deleted rows cannot be selected again, so continuing
            // is finite and may let later batches release a complete page.
        }
        Ok(evicted)
    }

    fn checkpoint_wal(&self) -> Result<()> {
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .context("STORE_IO: checkpoint journal WAL")
    }

    fn reclaim_space(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "PRAGMA wal_checkpoint(TRUNCATE); VACUUM; PRAGMA wal_checkpoint(TRUNCATE);",
            )
            .context("STORAGE_FULL: cannot physically reclaim the journal database")?;
        Ok(())
    }

    fn storage_bytes(&self) -> Result<u64> {
        let pages: i64 = self
            .conn
            .pragma_query_value(None, "page_count", |r| r.get(0))?;
        let size: i64 = self
            .conn
            .pragma_query_value(None, "page_size", |r| r.get(0))?;
        let pages = u64::try_from(pages).context("STORE_CORRUPT: negative page count")?;
        let size = u64::try_from(size).context("STORE_CORRUPT: negative page size")?;
        let wal = fs::metadata(format!("{}-wal", self.db_path.display()))
            .map(|m| m.len())
            .unwrap_or(0);
        pages
            .checked_mul(size)
            .and_then(|v| v.checked_add(wal))
            .ok_or_else(|| anyhow!("STORE_CORRUPT: storage size overflow"))
    }
    #[cfg(test)]
    fn set_cap_bytes(&mut self, value: u64) {
        self.cap_bytes = value;
    }
    fn summary(&self, rid: &str) -> Result<Value> {
        let (title, created, updated, context): (String, String, String, String) =
            self.conn.query_row(
                "SELECT title,created_at,updated_at,context_id FROM researches WHERE id=?",
                [rid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
        let refs = all_product_refs(&self.conn, rid)?;
        let product_count = refs.len();
        let shown = refs.into_iter().take(100).collect::<Vec<_>>();
        let counts = |table: &str| -> Result<u64> {
            let count: i64 = self.conn.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE research_id=?"),
                [rid],
                |r| r.get(0),
            )?;
            u64::try_from(count).context("STORE_CORRUPT: negative row count")
        };
        Ok(
            json!({"researchId":rid,"title":title,"createdAt":created,"updatedAt":updated,"contextId":context,"query":null,"productCount":product_count,"productRefs":shown,"productRefsTruncated":product_count>100,"eventCount":counts("events")?,"evidenceCount":counts("evidence")?,"noteCount":counts("notes")?}),
        )
    }
    fn cursor(
        &self,
        value: Option<&Value>,
        section: &str,
        rid: Option<&str>,
        filter: Option<&str>,
    ) -> Result<Option<(String, usize)>> {
        let Some(id) = value.and_then(Value::as_str) else {
            return Ok(None);
        };
        let row: Option<CursorRow> = self
            .conn
            .query_row("SELECT section,research_id,filter_json,cutoff,offset,expires_at FROM cursors WHERE id=?", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })
            .optional()?;
        let Some((s, r, f, c, o, e)) = row else {
            return Err(anyhow!("INVALID_REFERENCE: cursor is unavailable"));
        };
        if e <= now() || s != section || r.as_deref() != rid || f.as_deref() != filter {
            return Err(anyhow!("INVALID_REFERENCE: cursor binding mismatch"));
        }
        Ok(Some((
            c,
            usize::try_from(o).context("STORE_CORRUPT: negative cursor offset")?,
        )))
    }
    fn new_cursor(
        &mut self,
        section: &str,
        rid: Option<&str>,
        filter: Option<&str>,
        cutoff: &str,
        offset: usize,
    ) -> Result<String> {
        let id = id("cursor");
        let offset = i64::try_from(offset)
            .map_err(|_| anyhow!("INVALID_ARGUMENT: cursor offset overflow"))?;
        let expires_at = (Utc::now() + Duration::minutes(CURSOR_MINUTES))
            .to_rfc3339_opts(SecondsFormat::Millis, true);
        self.capacity_transaction(2048, rid, |tx| {
            tx.execute("INSERT INTO cursors(id,section,research_id,filter_json,cutoff,offset,expires_at) VALUES(?,?,?,?,?,?,?)",params![id,section,rid,filter,cutoff,offset,expires_at])?;
            Ok(())
        })?;
        Ok(id)
    }
}

fn lease_deadline() -> String {
    (Utc::now() + Duration::minutes(LEASE_MINUTES)).to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn page_size(conn: &Connection) -> Result<u64> {
    let size: i64 = conn.pragma_query_value(None, "page_size", |row| row.get(0))?;
    u64::try_from(size).context("STORE_CORRUPT: negative page size")
}

fn live_storage_bytes(conn: &Connection) -> Result<u64> {
    let pages: i64 = conn.pragma_query_value(None, "page_count", |row| row.get(0))?;
    let free: i64 = conn.pragma_query_value(None, "freelist_count", |row| row.get(0))?;
    let live = pages
        .checked_sub(free)
        .ok_or_else(|| anyhow!("STORE_CORRUPT: freelist exceeds page count"))?;
    u64::try_from(live)
        .context("STORE_CORRUPT: negative live page count")?
        .checked_mul(page_size(conn)?)
        .ok_or_else(|| anyhow!("STORE_CORRUPT: storage size overflow"))
}

fn victim_batch(conn: &Connection, deficit: u64, limit: usize) -> Result<Vec<String>> {
    let mut statement = conn.prepare(
        "SELECT r.id,
            length(r.title)+length(r.context_id)+length(r.context_json)+length(r.created_at)+length(r.updated_at)
            +COALESCE((SELECT SUM(length(value_json)+length(kind)) FROM refs WHERE research_id=r.id),0)
            +COALESCE((SELECT SUM(length(value_json)) FROM evidence WHERE research_id=r.id),0)
            +COALESCE((SELECT SUM(length(summary)+length(product_refs)+length(evidence_refs)) FROM events WHERE research_id=r.id),0)
            +COALESCE((SELECT SUM(length(text)+length(evidence_refs)+length(product_refs)+length(payload_json)) FROM notes WHERE research_id=r.id),0)
         FROM researches r
         WHERE r.protected_until IS NULL OR r.protected_until<=?
         ORDER BY r.updated_at,r.id LIMIT ?",
    )?;
    let candidates = statement
        .query_map(params![now(), i64::try_from(limit)?], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut selected = Vec::new();
    let mut estimated = 0_u64;
    for (id, bytes) in candidates {
        selected.push(id);
        estimated = estimated.saturating_add(u64::try_from(bytes).unwrap_or(0).max(1));
        if estimated >= deficit {
            break;
        }
    }
    Ok(selected)
}

fn map_sqlite_full(error: anyhow::Error) -> anyhow::Error {
    let full = error
        .downcast_ref::<rusqlite::Error>()
        .is_some_and(|error| {
            matches!(
                error,
                rusqlite::Error::SqliteFailure(failure, _)
                    if failure.code == rusqlite::ErrorCode::DiskFull
            )
        });
    if full {
        anyhow!("STORAGE_FULL: write exceeds the journal page bound")
    } else {
        error
    }
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}
fn normalize_time(value: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(value).ok().map(|v| {
        v.with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Millis, true)
    })
}
fn id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4())
}
fn canonical(value: &Value) -> String {
    fn sort(v: &Value) -> Value {
        match v {
            Value::Object(m) => Value::Object(
                m.iter()
                    .map(|(k, v)| (k.clone(), sort(v)))
                    .collect::<BTreeMap<_, _>>()
                    .into_iter()
                    .collect::<Map<_, _>>(),
            ),
            Value::Array(a) => Value::Array(a.iter().map(sort).collect()),
            _ => v.clone(),
        }
    }
    serde_json::to_string(&sort(value)).expect("JSON serialization")
}
fn string_field<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    v.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("INVALID_ARGUMENT: missing {k}"))
}
fn strings(v: Option<&Value>) -> Result<Vec<String>> {
    v.map(|v| {
        v.as_array()
            .ok_or_else(|| anyhow!("INVALID_ARGUMENT: expected array"))?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow!("INVALID_ARGUMENT: expected string"))
            })
            .collect()
    })
    .unwrap_or_else(|| Ok(vec![]))
}
fn research_context_id(conn: &Connection, id: &str) -> Result<String> {
    conn.query_row("SELECT context_id FROM researches WHERE id=?", [id], |r| {
        r.get(0)
    })
    .optional()?
    .ok_or_else(|| anyhow!("RESEARCH_EXPIRED: research is unavailable"))
}
fn max_updated(c: &Connection) -> Result<String> {
    Ok(c.query_row(
        "SELECT COALESCE(MAX(updated_at),'') FROM researches",
        [],
        |r| r.get(0),
    )?)
}
fn max_seq(c: &Connection, s: &str, r: &str) -> Result<i64> {
    let table = match s {
        "events" => "events",
        "evidence" => "evidence",
        "notes" => "notes",
        _ => return Err(anyhow!("INVALID_ARGUMENT: section")),
    };
    Ok(c.query_row(
        &format!("SELECT COALESCE(MAX(seq),0) FROM {table} WHERE research_id=?"),
        [r],
        |x| x.get(0),
    )?)
}
fn validate_ids(tx: &Connection, table: &str, rid: &str, ids: &[String]) -> Result<()> {
    for id in ids {
        let ok: bool = tx.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE id=? AND research_id=?)"),
            params![id, rid],
            |r| r.get(0),
        )?;
        if !ok {
            return Err(anyhow!("INVALID_REFERENCE: foreign or missing reference"));
        }
    }
    Ok(())
}
fn validate_product_refs(tx: &Connection, rid: &str, ids: &[String]) -> Result<()> {
    let known = all_product_refs(tx, rid)?;
    for id in ids {
        if !known.contains(id) {
            return Err(anyhow!(
                "INVALID_REFERENCE: foreign or missing product reference"
            ));
        }
    }
    Ok(())
}
fn validate_selection(c: &Connection, rid: &str, value: Option<&Value>) -> Result<()> {
    for id in strings(value)? {
        let ok: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM evidence WHERE id=? AND research_id=?)",
            params![id, rid],
            |r| r.get(0),
        )?;
        if !ok {
            return Err(anyhow!(
                "INVALID_REFERENCE: foreign or missing evidence reference"
            ));
        }
    }
    Ok(())
}
fn all_product_refs(c: &Connection, rid: &str) -> Result<BTreeSet<String>> {
    let mut s = BTreeSet::new();
    let mut q = c.prepare("SELECT product_refs FROM events WHERE research_id=?")?;
    for raw in q.query_map([rid], |r| r.get::<_, String>(0))? {
        for id in
            serde_json::from_str::<Vec<String>>(&raw?).context("STORE_CORRUPT: product refs")?
        {
            s.insert(id);
        }
    }
    Ok(s)
}
fn read_section(
    c: &Connection,
    s: &str,
    rid: &str,
    cutoff: i64,
    offset: usize,
    limit: usize,
    selection: Option<&Value>,
) -> Result<Vec<Value>> {
    let (table, expr) = match s {
        "events" => (
            "events",
            "json_object('eventId',id,'kind',kind,'observedAt',observed_at,'summary',summary,'productRefs',json(product_refs),'evidenceRefs',json(evidence_refs))",
        ),
        "evidence" => ("evidence", "json(value_json)"),
        "notes" => (
            "notes",
            "json_object('noteId',id,'operationId',operation_id,'kind',kind,'text',text,'createdAt',created_at,'author','agent','evidenceRefs',json(evidence_refs),'productRefs',json(product_refs))",
        ),
        _ => return Err(anyhow!("INVALID_ARGUMENT: section")),
    };
    let selected = strings(selection)?;
    let sql = if selected.is_empty() {
        format!(
            "SELECT {expr} FROM {table} WHERE research_id=? AND seq<=? ORDER BY seq LIMIT ? OFFSET ?"
        )
    } else {
        format!(
            "SELECT {expr} FROM {table} WHERE research_id=? AND seq<=? AND id IN (SELECT value FROM json_each(?)) ORDER BY seq LIMIT ? OFFSET ?"
        )
    };
    let mut st = c.prepare(&sql)?;
    let mapper = |r: &rusqlite::Row<'_>| r.get::<_, String>(0);
    let raws = if selected.is_empty() {
        st.query_map(params![rid, cutoff, limit as i64, offset as i64], mapper)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        st.query_map(
            params![
                rid,
                cutoff,
                canonical(&json!(selected)),
                limit as i64,
                offset as i64
            ],
            mapper,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    raws.into_iter()
        .map(|v| serde_json::from_str(&v).context("STORE_CORRUPT: journal JSON"))
        .collect()
}
fn secure_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    let m = fs::symlink_metadata(path)?;
    validate_private(&m, true)
}
fn validate_private(m: &fs::Metadata, dir: bool) -> Result<()> {
    if m.file_type().is_symlink() || dir && !m.is_dir() || !dir && !m.is_file() {
        return Err(anyhow!("STORE_SECURITY: unsafe store path"));
    }
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    if m.uid() != unsafe { geteuid() } || m.mode() & 0o077 != 0 {
        return Err(anyhow!(
            "STORE_SECURITY: store path has insecure ownership or permissions"
        ));
    }
    Ok(())
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS researches(id TEXT PRIMARY KEY,title TEXT NOT NULL,context_id TEXT NOT NULL,context_json TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL,protected_until TEXT);
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value_json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS refs(id TEXT PRIMARY KEY,research_id TEXT NOT NULL REFERENCES researches(id) ON DELETE CASCADE,context_id TEXT NOT NULL,kind TEXT NOT NULL,value_json TEXT NOT NULL,expires_at TEXT);
CREATE TABLE IF NOT EXISTS evidence(id TEXT PRIMARY KEY,research_id TEXT NOT NULL REFERENCES researches(id) ON DELETE CASCADE,seq INTEGER NOT NULL,value_json TEXT NOT NULL,UNIQUE(research_id,seq));
CREATE TABLE IF NOT EXISTS events(id TEXT PRIMARY KEY,research_id TEXT NOT NULL REFERENCES researches(id) ON DELETE CASCADE,seq INTEGER NOT NULL,kind TEXT NOT NULL,observed_at TEXT NOT NULL,summary TEXT,product_refs TEXT NOT NULL,evidence_refs TEXT NOT NULL,UNIQUE(research_id,seq));
CREATE TABLE IF NOT EXISTS notes(id TEXT PRIMARY KEY,research_id TEXT NOT NULL REFERENCES researches(id) ON DELETE CASCADE,seq INTEGER NOT NULL,operation_id TEXT NOT NULL,kind TEXT NOT NULL,text TEXT NOT NULL,created_at TEXT NOT NULL,evidence_refs TEXT NOT NULL,product_refs TEXT NOT NULL,payload_json TEXT NOT NULL,UNIQUE(research_id,operation_id),UNIQUE(research_id,seq));
CREATE TABLE IF NOT EXISTS cursors(id TEXT PRIMARY KEY,section TEXT NOT NULL,research_id TEXT,filter_json TEXT,cutoff TEXT NOT NULL,offset INTEGER NOT NULL,expires_at TEXT NOT NULL);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn private_tempdir() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn context(id: &str) -> Value {
        json!({"contextId":id,"regionLabel":null,"regionVerification":"unverified","accountState":"unknown","accessState":"available"})
    }
    #[test]
    fn persistence_and_foreign_refs() {
        let d = private_tempdir();
        let rid;
        let rf;
        {
            let mut s = Store::open(d.path()).unwrap();
            rid = s.create_research("t", &context("c1")).unwrap();
            rf = s.put_ref(&rid, "product", &json!({"x":1}), None).unwrap();
        }
        let mut s = Store::open(d.path()).unwrap();
        assert_eq!(s.get_ref(&rf, "product").unwrap().value, json!({"x":1}));
        assert!(
            s.ensure_research(&rid, "other")
                .unwrap_err()
                .to_string()
                .starts_with("CONTEXT_CHANGED")
        );
    }
    #[test]
    fn note_idempotency_and_conflict() {
        let d = private_tempdir();
        let mut s = Store::open(d.path()).unwrap();
        let r = s.create_research("t", &context("c")).unwrap();
        s.record(&r, "search", "q", &["p".into()], &[]).unwrap();
        let a = json!({"researchId":r,"operationId":"op","kind":"assessment","text":"ok","productRefs":["p"]});
        let first = s.append_note(&a).unwrap();
        assert_eq!(first, s.append_note(&a).unwrap());
        let event_count: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE research_id=?",
                [&r],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(event_count, 2, "retry must not append another note event");
        let mut b = a;
        b["text"] = json!("changed");
        assert!(
            s.append_note(&b)
                .unwrap_err()
                .to_string()
                .starts_with("CONFLICT")
        );
    }
    #[test]
    fn stable_paging() {
        let d = private_tempdir();
        let mut s = Store::open(d.path()).unwrap();
        for n in 0..3 {
            s.create_research(&format!("r{n}"), &context("c")).unwrap();
        }
        let p = s.list(&json!({"limit":2})).unwrap();
        assert_eq!(p["researches"].as_array().unwrap().len(), 2);
        let p2 = s
            .list(&json!({"limit":2,"cursor":p["nextCursor"]}))
            .unwrap();
        assert_eq!(p2["researches"].as_array().unwrap().len(), 1);
    }
    #[test]
    fn retention_respects_lease_and_reports_full() {
        let d = private_tempdir();
        let mut s = Store::open(d.path()).unwrap();
        let old = s.create_research("old", &context("c")).unwrap();
        s.conn
            .execute(
                "UPDATE researches SET updated_at='2000-01-01T00:00:00.000Z' WHERE id=?",
                [&old],
            )
            .unwrap();
        s.lease(&old).unwrap();
        s.maintain().unwrap();
        assert!(s.research_context(&old).is_ok());
        s.set_cap_bytes(1);
        assert!(
            s.maintain()
                .unwrap_err()
                .to_string()
                .starts_with("STORAGE_FULL")
        );
    }

    #[test]
    fn maintenance_reclaims_space_and_preserves_newer_research() {
        let d = private_tempdir();
        let mut s = Store::open(d.path()).unwrap();
        let old = s.create_research("old", &context("c")).unwrap();
        s.put_ref(&old, "large", &json!({"value":"x".repeat(200_000)}), None)
            .unwrap();
        s.conn
            .execute(
                "UPDATE researches SET updated_at=?,protected_until=NULL WHERE id=?",
                params![
                    (Utc::now() - Duration::minutes(1))
                        .to_rfc3339_opts(SecondsFormat::Millis, true),
                    old
                ],
            )
            .unwrap();
        s.reclaim_space().unwrap();
        let one_research_size = s.storage_bytes().unwrap();

        let newer = s.create_research("newer", &context("c")).unwrap();
        s.put_ref(&newer, "large", &json!({"value":"y".repeat(200_000)}), None)
            .unwrap();
        assert!(s.storage_bytes().unwrap() > one_research_size);
        s.set_cap_bytes(one_research_size + 16 * 1024);
        s.maintain().unwrap();

        assert!(s.research_context(&old).is_err());
        assert!(s.research_context(&newer).is_ok());
        assert!(s.storage_bytes().unwrap() <= s.cap_bytes);
    }

    #[test]
    fn maintenance_removes_expired_temporary_authority() {
        let d = private_tempdir();
        let mut s = Store::open(d.path()).unwrap();
        let research = s.create_research("r", &context("c")).unwrap();
        s.put_ref(&research, "temporary", &json!({}), Some(0))
            .unwrap();
        s.new_cursor("events", Some(&research), None, "0", 0)
            .unwrap();
        s.conn
            .execute(
                "UPDATE cursors SET expires_at='2000-01-01T00:00:00.000Z'",
                [],
            )
            .unwrap();

        s.maintain().unwrap();
        let refs: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM refs", [], |row| row.get(0))
            .unwrap();
        let cursors: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM cursors", [], |row| row.get(0))
            .unwrap();
        assert_eq!((refs, cursors), (0, 0));
    }

    #[test]
    fn maintenance_batches_many_small_researches_without_pruning_all() {
        let d = private_tempdir();
        let mut s = Store::open(d.path()).unwrap();
        for index in 0..80 {
            let research = s
                .create_research(&format!("small-{index:02}"), &context("c"))
                .unwrap();
            s.put_ref(
                &research,
                "payload",
                &json!({"value":format!("{index:02}-{}", "x".repeat(4096))}),
                None,
            )
            .unwrap();
        }
        s.conn
            .execute("UPDATE researches SET protected_until=NULL", [])
            .unwrap();
        s.reclaim_space().unwrap();
        let before = s.storage_bytes().unwrap();
        s.set_cap_bytes(before.saturating_sub(96 * 1024));

        s.maintain().unwrap();

        let remaining: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM researches", [], |row| row.get(0))
            .unwrap();
        assert!((1..80).contains(&remaining));
        assert!(s.storage_bytes().unwrap() <= s.cap_bytes);
    }

    #[test]
    fn long_lived_writes_enforce_cap_and_keep_active_research() {
        let d = private_tempdir();
        let mut s = Store::open(d.path()).unwrap();
        let old = s.create_research("old", &context("c")).unwrap();
        s.put_ref(&old, "payload", &json!({"value":"o".repeat(200_000)}), None)
            .unwrap();
        s.conn
            .execute(
                "UPDATE researches SET protected_until=NULL,updated_at=? WHERE id=?",
                params![
                    (Utc::now() - Duration::minutes(1))
                        .to_rfc3339_opts(SecondsFormat::Millis, true),
                    old
                ],
            )
            .unwrap();
        let active = s.create_research("active", &context("c")).unwrap();
        s.reclaim_space().unwrap();
        s.set_cap_bytes(s.storage_bytes().unwrap());

        s.put_ref(
            &active,
            "payload",
            &json!({"value":"a".repeat(100_000)}),
            None,
        )
        .unwrap();
        assert!(s.research_context(&old).is_err());
        assert!(s.research_context(&active).is_ok());
        assert!(s.storage_bytes().unwrap() <= s.cap_bytes);

        let error = s
            .put_ref(
                &active,
                "oversized",
                &json!({"value":"z".repeat(s.cap_bytes as usize)}),
                None,
            )
            .unwrap_err();
        assert!(error.to_string().starts_with("STORAGE_FULL"));
        assert!(s.research_context(&active).is_ok());
        assert!(s.storage_bytes().unwrap() <= s.cap_bytes);
    }

    #[test]
    fn invalid_writes_do_not_evict_idle_research() {
        let d = private_tempdir();
        let mut s = Store::open(d.path()).unwrap();
        let victim = s.create_research("victim", &context("c")).unwrap();
        s.put_ref(
            &victim,
            "payload",
            &json!({"value":"v".repeat(100_000)}),
            None,
        )
        .unwrap();
        let target = s.create_research("target", &context("c")).unwrap();
        s.conn
            .execute(
                "UPDATE researches SET protected_until=NULL WHERE id=?",
                [&victim],
            )
            .unwrap();
        s.reclaim_space().unwrap();
        s.set_cap_bytes(s.storage_bytes().unwrap());

        assert!(
            s.put_ref("research_missing", "product", &json!({"sku":"1"}), None)
                .unwrap_err()
                .to_string()
                .starts_with("RESEARCH_EXPIRED")
        );
        assert!(
            s.record(
                &target,
                "search",
                "invalid",
                &[],
                &[json!({"sourceKind":"invalid"})],
            )
            .unwrap_err()
            .to_string()
            .starts_with("INVALID_ARGUMENT")
        );
        assert!(
            s.append_note(&json!({
                "researchId":target,
                "operationId":"invalid-ref",
                "kind":"assessment",
                "text":"invalid",
                "productRefs":["missing"]
            }))
            .unwrap_err()
            .to_string()
            .starts_with("INVALID_REFERENCE")
        );

        assert!(s.research_context(&victim).is_ok());
        assert_eq!(
            s.conn
                .query_row("SELECT COUNT(*) FROM researches", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn underestimated_growth_rolls_back_write_and_eviction() {
        let d = private_tempdir();
        let mut s = Store::open(d.path()).unwrap();
        let research = s.create_research("protected", &context("c")).unwrap();
        s.reclaim_space().unwrap();
        let before_lease: String = s
            .conn
            .query_row(
                "SELECT protected_until FROM researches WHERE id=?",
                [&research],
                |row| row.get(0),
            )
            .unwrap();
        s.set_cap_bytes(s.storage_bytes().unwrap() + page_size(&s.conn).unwrap());

        let error = s
            .capacity_transaction(0, Some(&research), |tx| {
                tx.execute(
                    "INSERT INTO meta(key,value_json) VALUES('underestimated',?)",
                    ["x".repeat(100_000)],
                )?;
                Ok(())
            })
            .unwrap_err();

        assert!(error.to_string().starts_with("STORAGE_FULL"));
        assert_eq!(s.meta("underestimated").unwrap(), None);
        assert_eq!(
            s.conn
                .query_row(
                    "SELECT protected_until FROM researches WHERE id=?",
                    [&research],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            before_lease,
            "failed admission must roll back the lease with the write"
        );
        assert!(s.storage_bytes().unwrap() <= s.cap_bytes);
    }

    #[test]
    fn capacity_reclamation_crosses_a_small_batch_page_plateau() {
        let d = private_tempdir();
        let mut s = Store::open(d.path()).unwrap();
        let active = s.create_research("active", &context("c")).unwrap();
        let context_json = canonical(&context("c"));
        let tx = s.conn.transaction().unwrap();
        for index in 0..400 {
            tx.execute(
                "INSERT INTO researches(id,title,context_id,context_json,created_at,updated_at) VALUES(?,?,?,?,?,?)",
                params![
                    format!("idle_{index:04}"),
                    "idle",
                    "c",
                    context_json,
                    "2000-01-01T00:00:00.000Z",
                    "2000-01-01T00:00:00.000Z"
                ],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        s.reclaim_space().unwrap();

        let page = page_size(&s.conn).unwrap();
        let tx = s.conn.transaction().unwrap();
        tx.execute(
            "INSERT INTO refs(id,research_id,context_id,kind,value_json,expires_at) VALUES('plateau_ref',?,'c','product','{}',NULL)",
            [&active],
        )
        .unwrap();
        let before = live_storage_bytes(&tx).unwrap();
        let first = victim_batch(&tx, page, 1).unwrap();
        assert_eq!(first.len(), 1);
        tx.execute("DELETE FROM researches WHERE id=?", [&first[0]])
            .unwrap();
        assert_eq!(
            live_storage_bytes(&tx).unwrap(),
            before,
            "one tiny victim should exercise the no-page-progress plateau"
        );

        Store::enforce_capacity_tx_with_limit(&tx, before - page, 1).unwrap();
        tx.commit().unwrap();
        assert!(s.get_ref("plateau_ref", "product").is_ok());
        let remaining: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM researches WHERE id LIKE 'idle_%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!((1..400).contains(&remaining));
    }
}
