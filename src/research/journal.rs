use crate::error::{Code, fail};
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, Instant};
use uuid::Uuid;

const JOURNAL_VERSION: i64 = 1;
const RETENTION_DAYS: i64 = 30;
const CAP_BYTES: u64 = 512 * 1024 * 1024;
const LEASE_MINUTES: i64 = 10;
const CURSOR_MINUTES: i64 = 30;
const MAINTENANCE_INTERVAL: StdDuration = StdDuration::from_secs(60);

pub struct StoredRef {
    pub research_id: String,
    pub context_id: String,
    pub value: Value,
}

pub struct Journal {
    conn: Connection,
    db_path: PathBuf,
    cap_bytes: u64,
    last_maintenance: Option<Instant>,
}

/// A synchronous operation boundary. Nothing written here is durable until the
/// caller has built and validated its response and Journal commits this transaction.
pub struct JournalTxn<'a> {
    tx: Transaction<'a>,
    next_stage: u64,
    stage_failure: Option<Code>,
}

impl Journal {
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_journal(root).map_err(map_sqlite_full)
    }
    fn open_journal(root: &Path) -> Result<Self> {
        secure_directory(root)?;
        // The old research.sqlite3 and all user artifacts remain untouched.
        let db_path = root.join("journal.sqlite3");
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
        let mut conn = Connection::open(&db_path).context("STORE_IO: open database")?;
        conn.busy_timeout(StdDuration::from_secs(5))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != 0 && version != JOURNAL_VERSION {
            return Err(fail(
                Code::InvalidConfiguration,
                format!("unsupported journal schema version {version}"),
            ));
        }
        if version == 0 {
            let tables: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'", [], |r| r.get(0))?;
            if tables != 0 {
                return Err(fail(
                    Code::InvalidConfiguration,
                    "unversioned journal contains existing tables",
                ));
            }
        }
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "journal_size_limit", 0_i64)?;
        conn.pragma_update(None, "wal_autocheckpoint", 1_i64)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        if version == 0 {
            let tx = conn.transaction()?;
            tx.execute_batch(SCHEMA)
                .context("STORE_IO: initialize journal")?;
            tx.pragma_update(None, "user_version", JOURNAL_VERSION)?;
            tx.commit()?;
        }
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
            last_maintenance: None,
        })
    }

    pub fn commit_operation<T>(
        &mut self,
        research_id: Option<&str>,
        validate_commit: impl Fn() -> Result<()>,
        build: impl FnOnce(&mut JournalTxn<'_>) -> Result<T>,
    ) -> Result<T> {
        self.commit_operation_inner(research_id, false, validate_commit, build)
            .map_err(map_sqlite_full)
    }

    fn commit_operation_inner<T>(
        &mut self,
        research_id: Option<&str>,
        force_maintenance: bool,
        validate_commit: impl Fn() -> Result<()>,
        build: impl FnOnce(&mut JournalTxn<'_>) -> Result<T>,
    ) -> Result<T> {
        validate_commit()?;
        self.checkpoint_wal().map_err(map_sqlite_full)?;
        let page = page_size(&self.conn)?;
        if self.cap_bytes < page {
            return Err(fail(
                Code::StorageFull,
                "write size exceeds the store budget",
            ));
        }
        // Allow bounded transaction staging before idle records are evicted.
        // Durable admission below still uses the final live-page cap.
        let physical: i64 = self
            .conn
            .pragma_query_value(None, "page_count", |r| r.get(0))?;
        let physical = u64::try_from(physical).context("STORE_CORRUPT: negative page count")?;
        let staging_pages = physical.saturating_add(self.cap_bytes / page);
        self.conn.pragma_update(
            None,
            "max_page_count",
            i64::try_from(staging_pages).unwrap_or(i64::MAX),
        )?;
        let due = force_maintenance
            || self
                .last_maintenance
                .is_none_or(|t| t.elapsed() >= MAINTENANCE_INTERVAL);
        let outcome = {
            let tx = self.conn.transaction()?;
            let mut journal = JournalTxn {
                tx,
                next_stage: 0,
                stage_failure: None,
            };
            let result = (|| {
                if let Some(rid) = research_id {
                    journal.lease(rid)?;
                }
                let result = build(&mut journal).map_err(map_sqlite_full)?;
                journal.ensure_healthy()?;
                if due {
                    journal.maintain()?;
                }
                let evicted = Self::enforce_capacity_tx(&journal.tx, self.cap_bytes)?;
                validate_commit()?;
                Ok((result, evicted))
            })();
            match result {
                Ok(value) => journal
                    .tx
                    .commit()
                    .map(|()| value)
                    .map_err(|error| map_sqlite_full(error.into())),
                Err(error) => Err(error),
            }
        };
        let (value, evicted) = match outcome {
            Ok(value) => value,
            Err(error) => {
                let _ = self.reclaim_space();
                return Err(error);
            }
        };
        if due {
            self.last_maintenance = Some(Instant::now());
        }
        // Commit is the linearization boundary: cleanup may be retried at the
        // next operation, but can never report a failure for this committed one.
        let _ = self.checkpoint_wal();
        if evicted
            || self
                .storage_bytes()
                .is_ok_and(|bytes| bytes > self.cap_bytes)
        {
            let _ = self.reclaim_space();
        }
        let _ = self.conn.pragma_update(
            None,
            "max_page_count",
            i64::try_from(self.cap_bytes / page).unwrap_or(i64::MAX),
        );
        Ok(value)
    }

    #[cfg(test)]
    pub fn create_research(&mut self, title: &str, context: &Value) -> Result<String> {
        self.commit_operation(None, || Ok(()), |tx| tx.create_research(title, context))
    }
    pub fn ensure_research(&self, id: &str, context_id: &str) -> Result<()> {
        ensure_research(&self.conn, id, context_id)
    }
    #[cfg(test)]
    pub fn research_context(&self, id: &str) -> Result<Value> {
        research_context(&self.conn, id)
    }
    pub fn meta(&self, key: &str) -> Result<Option<Value>> {
        metadata(&self.conn, key)
    }
    pub fn get_ref(&self, id: &str, kind: &str) -> Result<StoredRef> {
        get_ref(&self.conn, id, kind)
    }
    /// Admit protection before source awaits; renew again inside the commit.
    pub fn lease(&mut self, id: &str) -> Result<()> {
        let changed = self
            .conn
            .execute(
                "UPDATE researches SET protected_until=? WHERE id=?",
                params![lease_deadline(), id],
            )
            .map_err(|error| map_sqlite_full(error.into()))?;
        if changed != 1 {
            return Err(fail(Code::ResearchExpired, "research is unavailable"));
        }
        Ok(())
    }
    pub fn maintain(&mut self) -> Result<()> {
        self.commit_operation_inner(None, true, || Ok(()), |_| Ok(()))
            .map_err(map_sqlite_full)
    }

    #[cfg(test)]
    pub fn put_ref(
        &mut self,
        rid: &str,
        kind: &str,
        value: &Value,
        ttl: Option<u64>,
    ) -> Result<String> {
        self.commit_operation(Some(rid), || Ok(()), |tx| tx.put_ref(rid, kind, value, ttl))
    }
    #[cfg(test)]
    pub fn record(
        &mut self,
        rid: &str,
        kind: &str,
        summary: &str,
        refs: &[String],
        evidence: &[Value],
    ) -> Result<()> {
        self.commit_operation(
            Some(rid),
            || Ok(()),
            |tx| tx.record(rid, kind, summary, refs, evidence),
        )
    }
    #[cfg(test)]
    pub fn append_note(&mut self, args: &Value) -> Result<Value> {
        let rid = string_field(args, "researchId")?;
        self.commit_operation(Some(rid), || Ok(()), |tx| tx.append_note(args))
    }
    #[cfg(test)]
    pub fn list(&mut self, args: &Value) -> Result<Value> {
        self.commit_operation(None, || Ok(()), |tx| tx.list(args))
    }
    #[cfg(test)]
    pub(crate) fn set_cap_bytes(&mut self, value: u64) {
        self.cap_bytes = value;
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
                return Err(fail(
                    Code::StorageFull,
                    "insufficient idle research can be reclaimed safely",
                ));
            }
            for id in victims {
                tx.execute("DELETE FROM researches WHERE id=?", [id])?;
            }
            evicted = true;
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
            .context("STORAGE_FULL: cannot physically reclaim the journal database")
    }
    fn storage_bytes(&self) -> Result<u64> {
        let pages: i64 = self
            .conn
            .pragma_query_value(None, "page_count", |r| r.get(0))?;
        let pages = u64::try_from(pages).context("STORE_CORRUPT: negative page count")?;
        let wal = fs::metadata(format!("{}-wal", self.db_path.display()))
            .map(|m| m.len())
            .unwrap_or(0);
        pages
            .checked_mul(page_size(&self.conn)?)
            .and_then(|v| v.checked_add(wal))
            .ok_or_else(|| fail(Code::SourceChanged, "storage size overflow"))
    }
}

impl JournalTxn<'_> {
    /// Roll back one recoverable item while preserving other successful items in
    /// the enclosing operation. No quota admission or durable commit occurs here.
    pub fn stage<T>(&mut self, build: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.ensure_healthy()?;
        let index = self.next_stage;
        self.next_stage = self
            .next_stage
            .checked_add(1)
            .ok_or_else(|| fail(Code::SourceChanged, "journal stage counter exhausted"))?;
        let name = format!("journal_item_{index}");
        self.tx
            .execute_batch(&format!("SAVEPOINT {name}"))
            .map_err(|error| map_sqlite_full(error.into()))?;
        match build(self).map_err(map_sqlite_full) {
            Ok(value) => {
                if let Err(error) = self.tx.execute_batch(&format!("RELEASE SAVEPOINT {name}")) {
                    let error = map_sqlite_full(error.into());
                    self.stage_failure = Some(if crate::error::code(&error) == "STORAGE_FULL" {
                        Code::StorageFull
                    } else {
                        Code::SourceChanged
                    });
                    return Err(error);
                }
                Ok(value)
            }
            Err(error) => {
                let full = crate::error::code(&error) == "STORAGE_FULL";
                if self
                    .tx
                    .execute_batch(&format!(
                        "ROLLBACK TO SAVEPOINT {name}; RELEASE SAVEPOINT {name}"
                    ))
                    .is_err()
                    || full
                {
                    self.stage_failure = Some(if full {
                        Code::StorageFull
                    } else {
                        Code::SourceChanged
                    });
                }
                Err(error)
            }
        }
    }
    fn ensure_healthy(&self) -> Result<()> {
        if let Some(code) = self.stage_failure {
            return Err(fail(
                code,
                "journal item staging could not be completed safely",
            ));
        }
        Ok(())
    }
    pub fn create_research(&mut self, title: &str, context: &Value) -> Result<String> {
        let context_id = string_field(context, "contextId")?;
        if title.is_empty() || title.chars().count() > 500 {
            return Err(fail(Code::InvalidArgument, "invalid research title"));
        }
        let rid = id("research");
        let created = now();
        self.tx.execute("INSERT INTO researches(id,title,context_id,context_json,created_at,updated_at,protected_until) VALUES(?,?,?,?,?,?,?)",
            params![rid, title, context_id, canonical(context), created, created, lease_deadline()])?;
        Ok(rid)
    }
    pub fn ensure_research(&self, rid: &str, context_id: &str) -> Result<()> {
        ensure_research(&self.tx, rid, context_id)
    }
    pub fn research_context(&self, rid: &str) -> Result<Value> {
        research_context(&self.tx, rid)
    }
    pub fn meta(&self, key: &str) -> Result<Option<Value>> {
        metadata(&self.tx, key)
    }
    pub fn set_meta(&mut self, key: &str, value: &Value) -> Result<()> {
        self.tx.execute("INSERT INTO meta(key,value_json) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json", params![key, canonical(value)])?;
        Ok(())
    }
    pub fn put_ref(
        &mut self,
        rid: &str,
        kind: &str,
        value: &Value,
        ttl: Option<u64>,
    ) -> Result<String> {
        validate_ref_kind(kind)?;
        let context_id = research_context_id(&self.tx, rid)?;
        let ref_id = id("ref");
        let expires = ttl
            .map(|seconds| {
                let seconds = i64::try_from(seconds).map_err(|_| {
                    fail(Code::InvalidArgument, "reference lifetime is out of range")
                })?;
                let duration = Duration::try_seconds(seconds).ok_or_else(|| {
                    fail(Code::InvalidArgument, "reference lifetime is out of range")
                })?;
                let expiry = Utc::now().checked_add_signed(duration).ok_or_else(|| {
                    fail(Code::InvalidArgument, "reference lifetime is out of range")
                })?;
                Ok::<_, anyhow::Error>(expiry.to_rfc3339_opts(SecondsFormat::Millis, true))
            })
            .transpose()?;
        self.tx.execute("INSERT INTO refs(id,research_id,context_id,kind,value_json,expires_at) VALUES(?,?,?,?,?,?)",
            params![ref_id, rid, context_id, kind, canonical(value), expires])?;
        Ok(ref_id)
    }
    pub fn get_ref(&self, id: &str, kind: &str) -> Result<StoredRef> {
        get_ref(&self.tx, id, kind)
    }
    pub fn record(
        &mut self,
        rid: &str,
        kind: &str,
        summary: &str,
        product_refs: &[String],
        evidence: &[Value],
    ) -> Result<()> {
        let context_id = research_context_id(&self.tx, rid)?;
        validate_product_refs(&self.tx, rid, product_refs)?;
        let observed = observation_time(evidence);
        let prepared = prepare_evidence(evidence, &context_id, &observed)?;
        insert_record(
            &self.tx,
            rid,
            kind,
            summary,
            product_refs,
            &prepared,
            &observed,
        )
    }
    /// Attach a single immutable candidate observation to each issued product ref.
    /// Repeated SKUs remain separate observations; there is no latest baseline.
    pub fn record_search(
        &mut self,
        rid: &str,
        summary: &str,
        product_refs: &[String],
        evidence: &[Value],
        items: &mut Value,
    ) -> Result<()> {
        let context_id = research_context_id(&self.tx, rid)?;
        let rows = items
            .as_array()
            .ok_or_else(|| fail(Code::InvalidArgument, "search items must be an array"))?;
        if rows.len() != product_refs.len() || rows.len() > 36 {
            return Err(fail(
                Code::InvalidArgument,
                "search items and productRefs mismatch",
            ));
        }
        let observed = observation_time(evidence);
        let prepared = prepare_evidence(evidence, &context_id, &observed)?;
        for (row, reference) in rows.iter().zip(product_refs) {
            let sku = string_field(row, "sku")?;
            if row.get("productRef").and_then(Value::as_str) != Some(reference) {
                return Err(fail(Code::InvalidArgument, "search productRef mismatch"));
            }
            let stored = self.get_ref(reference, "product")?;
            if stored.research_id != rid {
                return Err(fail(Code::InvalidReference, "foreign product reference"));
            }
            if stored.context_id != context_id {
                return Err(fail(
                    Code::ContextChanged,
                    "product reference belongs to another context",
                ));
            }
            if stored.value.get("searchSnapshot").is_some() {
                return Err(fail(
                    Code::InvalidReference,
                    "product snapshot is immutable",
                ));
            }
            if stored.value.get("sku").and_then(Value::as_str) != Some(sku)
                || stored.value.get("url") != row.get("url")
            {
                return Err(fail(
                    Code::InvalidReference,
                    "product reference binding mismatch",
                ));
            }
            validate_image_refs(&self.tx, rid, &context_id, row)?;
            let mut snapshot = row.clone();
            snapshot["observedAt"] = json!(observed);
            snapshot["contextId"] = json!(context_id);
            let value = json!({"sku":sku,"url":row.get("url").cloned().unwrap_or(Value::Null),"searchSnapshot":snapshot});
            self.tx.execute(
                "UPDATE refs SET value_json=? WHERE id=? AND research_id=? AND kind='product'",
                params![canonical(&value), reference, rid],
            )?;
        }
        insert_record(
            &self.tx,
            rid,
            "search",
            summary,
            product_refs,
            &prepared,
            &observed,
        )
    }
    pub fn append_note(&mut self, args: &Value) -> Result<Value> {
        let rid = string_field(args, "researchId")?;
        let operation = string_field(args, "operationId")?;
        let kind = string_field(args, "kind")?;
        let text = string_field(args, "text")?;
        if operation.is_empty()
            || !matches!(kind, "requirements" | "assessment" | "conclusion")
            || text.is_empty()
            || text.chars().count() > 12_000
        {
            return Err(fail(Code::InvalidArgument, "invalid note"));
        }
        research_context_id(&self.tx, rid)?;
        let payload = canonical(args);
        let previous: Option<(String, String, String)> = self.tx.query_row("SELECT id,created_at,payload_json FROM notes WHERE research_id=? AND operation_id=?", params![rid,operation], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some((note_id, created, previous)) = previous {
            if previous != payload {
                return Err(fail(
                    Code::Conflict,
                    "operationId was reused with a different payload",
                ));
            }
            return Ok(json!({"noteId":note_id,"createdAt":created,"author":"agent"}));
        }
        let evidence = strings(args.get("evidenceRefs"))?;
        let products = strings(args.get("productRefs"))?;
        validate_ids(&self.tx, "evidence", rid, &evidence)?;
        validate_product_refs(&self.tx, rid, &products)?;
        let note_id = id("note");
        let created = now();
        self.tx.execute("INSERT INTO notes(id,research_id,seq,operation_id,kind,text,created_at,evidence_refs,product_refs,payload_json) VALUES(?,?,(SELECT COALESCE(MAX(seq),0)+1 FROM notes WHERE research_id=?),?,?,?,?,?,?,?)",
            params![note_id,rid,rid,operation,kind,text,created,canonical(&json!(evidence)),canonical(&json!(products)),payload])?;
        insert_event(
            &self.tx,
            rid,
            &json!({
                "eventId": id("event"), "kind": "note_appended", "observedAt": created,
                "summary": format!("Agent {kind} note appended"),
                "productRefs": products, "evidenceRefs": evidence
            }),
        )?;
        self.tx.execute(
            "UPDATE researches SET updated_at=? WHERE id=?",
            params![created, rid],
        )?;
        Ok(json!({"noteId":note_id,"createdAt":created,"author":"agent"}))
    }
    pub fn list(&mut self, args: &Value) -> Result<Value> {
        let limit = input_limit(args, 10, 50)?;
        let state = self.cursor(args.get("cursor"), "list", None)?;
        if state.is_some() && args.get("query").is_some() {
            return Err(fail(
                Code::InvalidArgument,
                "query cannot accompany a listing cursor",
            ));
        }
        let query = state
            .as_ref()
            .and_then(|s| s.get("query"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| args.get("query").and_then(Value::as_str).map(str::to_owned));
        let cutoff = match state.as_ref() {
            Some(s) => string_field(s, "cutoff")?.to_owned(),
            None => max_updated(&self.tx)?,
        };
        let after_time = state
            .as_ref()
            .and_then(|s| s.get("afterTime"))
            .and_then(Value::as_str);
        let after_id = state
            .as_ref()
            .and_then(|s| s.get("afterId"))
            .and_then(Value::as_str);
        let needle = query.as_ref().map(|q| {
            format!(
                "%{}%",
                q.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            )
        });
        let mut stmt = self.tx.prepare("SELECT id,title,created_at,updated_at,context_id FROM researches r WHERE updated_at<=? AND (? IS NULL OR updated_at<? OR (updated_at=? AND id<?)) AND (? IS NULL OR title LIKE ? ESCAPE '\\' OR EXISTS(SELECT 1 FROM notes n WHERE n.research_id=r.id AND n.text LIKE ? ESCAPE '\\')) ORDER BY updated_at DESC,id DESC LIMIT ?")?;
        let rows = stmt
            .query_map(
                params![
                    cutoff,
                    after_time,
                    after_time,
                    after_time,
                    after_id,
                    needle,
                    needle,
                    needle,
                    (limit + 1) as i64
                ],
                |r| {
                    Ok(json!({
                        "researchId": r.get::<_,
                        String>(0)?,
                        "title": r.get::<_,
                        String>(1)?,
                        "createdAt": r.get::<_,
                        String>(2)?,
                        "updatedAt": r.get::<_,
                        String>(3)?,
                        "contextId": r.get::<_,
                        String>(4)?
                    }))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        let more = rows.len() > limit;
        let rows = rows.into_iter().take(limit).collect::<Vec<_>>();
        let next = if more {
            let last = rows.last().expect("positive listing limit");
            Some(self.new_cursor("list", None, &json!({"query":query,"cutoff":cutoff,"afterTime":last["updatedAt"],"afterId":last["researchId"]}))?)
        } else {
            None
        };
        Ok(json!({"researches":rows,"nextCursor":next}))
    }
    pub fn read(&mut self, args: &Value) -> Result<Value> {
        let rid = string_field(args, "researchId")?;
        research_context_id(&self.tx, rid)?;
        let budget = self.read_budget(rid)?;
        let section = args
            .get("section")
            .and_then(Value::as_str)
            .unwrap_or("summary");
        if section == "summary" {
            return Ok(json!({"section":"summary","payload":self.summary(rid)?,"nextCursor":null}));
        }
        if section == "candidates" {
            let refs = selection(args.get("productRefs"), 20, "productRefs")?;
            let mut values = Vec::with_capacity(refs.len());
            for reference in refs {
                let stored = self.get_ref(&reference, "product")?;
                if stored.research_id != rid {
                    return Err(fail(Code::InvalidReference, "foreign product reference"));
                }
                values.push(stored.value.get("searchSnapshot").cloned().ok_or_else(|| {
                    fail(
                        Code::InvalidReference,
                        "product reference has no candidate snapshot",
                    )
                })?);
            }
            return bounded_section(section, values, Value::Null, budget);
        }
        let default = match section {
            "notes" => 10,
            "events" => 25,
            "evidence" => 20,
            _ => return Err(fail(Code::InvalidArgument, "unknown research section")),
        };
        let limit = input_limit(args, default, 25)?;
        let selector = match section {
            "notes" => args.get("noteIds"),
            "evidence" => args.get("evidenceRefs"),
            _ => None,
        };
        if let Some(selector) = selector {
            if args.get("cursor").is_some() || args.get("limit").is_some() {
                return Err(fail(
                    Code::InvalidArgument,
                    "exact selection cannot be paged",
                ));
            }
            let selected = selection(
                Some(selector),
                if section == "notes" { 10 } else { 20 },
                if section == "notes" {
                    "noteIds"
                } else {
                    "evidenceRefs"
                },
            )?;
            validate_ids(&self.tx, section, rid, &selected)?;
            let values = read_section(
                &self.tx,
                section,
                rid,
                max_seq(&self.tx, section, rid)?,
                0,
                selected.len(),
                Some(selector),
            )?;
            return bounded_section(section, values, Value::Null, budget);
        }
        let state = self.cursor(args.get("cursor"), section, Some(rid))?;
        let cutoff = state
            .as_ref()
            .and_then(|s| s.get("cutoff"))
            .and_then(Value::as_i64)
            .map(Ok)
            .unwrap_or_else(|| max_seq(&self.tx, section, rid))?;
        let offset = state
            .as_ref()
            .map(|s| {
                s.get("offset")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| fail(Code::SourceChanged, "invalid cursor offset"))
            })
            .transpose()?
            .unwrap_or(0) as usize;
        let mut values = read_section(&self.tx, section, rid, cutoff, offset, limit + 1, None)?;
        let loaded = values.len();
        values.truncate(limit);
        let placeholder = json!("cursor_00000000-0000-0000-0000-000000000000");
        while !values.is_empty()
            && crate::research::response::serialized_utf16_len(
                &json!({"section":section,"payload":values,"nextCursor":placeholder}),
            )? > budget
        {
            values.pop();
        }
        if values.is_empty() && loaded > 0 {
            return Err(fail(
                Code::ResultTooLarge,
                "one journal record exceeds the response budget; select a different record",
            ));
        }
        let more = loaded > values.len();
        let next = if more {
            json!(self.new_cursor(
                section,
                Some(rid),
                &json!({"cutoff": cutoff,"offset": offset+values.len()})
            )?)
        } else {
            Value::Null
        };
        bounded_section(section, values, next, budget)
    }
    fn read_budget(&self, rid: &str) -> Result<usize> {
        // Local research replies use this captured Context with no live evidence
        // or warnings. Measure their complete envelope rather than reserving an
        // arbitrary amount which can make an otherwise valid page inaccessible.
        let envelope = json!({
            "schemaVersion": "3", "researchId": rid, "data": null,
            "context": self.research_context(rid)?, "observedAt": now(),
            "evidence": [], "warnings": []
        });
        let overhead =
            crate::research::response::serialized_utf16_len(&envelope)?.saturating_sub(4);
        crate::research::response::MAX_RESULT_UNITS
            .checked_sub(overhead)
            .ok_or_else(|| {
                fail(
                    Code::ResultTooLarge,
                    "research context exceeds the response budget",
                )
            })
    }
    pub fn lease(&mut self, rid: &str) -> Result<()> {
        research_context_id(&self.tx, rid)?;
        self.tx.execute(
            "UPDATE researches SET protected_until=? WHERE id=?",
            params![lease_deadline(), rid],
        )?;
        Ok(())
    }
    fn maintain(&mut self) -> Result<()> {
        let current = now();
        let cutoff = (Utc::now() - Duration::days(RETENTION_DAYS))
            .to_rfc3339_opts(SecondsFormat::Millis, true);
        self.tx.execute(
            "DELETE FROM refs WHERE expires_at IS NOT NULL AND expires_at<=?",
            [&current],
        )?;
        self.tx
            .execute("DELETE FROM cursors WHERE expires_at<=?", [&current])?;
        self.tx.execute("DELETE FROM researches WHERE updated_at<? AND (protected_until IS NULL OR protected_until<=?)",params![cutoff,current])?;
        Ok(())
    }
    fn summary(&self, rid: &str) -> Result<Value> {
        let (title, created, updated, context): (String, String, String, String) =
            self.tx.query_row(
                "SELECT title,created_at,updated_at,context_id FROM researches WHERE id=?",
                [rid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
        let refs = all_product_refs(&self.tx, rid)?;
        let total_refs = refs.len();
        let product_count: i64 = self.tx.query_row(
            "SELECT COUNT(DISTINCT json_extract(value_json,'$.sku')) FROM refs WHERE research_id=? AND context_id=? AND kind='product' AND (expires_at IS NULL OR expires_at>?)",
            params![rid,context,now()],|r|r.get(0)
        )?;
        let product_count =
            u64::try_from(product_count).context("STORE_CORRUPT: negative product count")?;
        let counts = |table: &str| -> Result<u64> {
            let count: i64 = self.tx.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE research_id=?"),
                [rid],
                |r| r.get(0),
            )?;
            u64::try_from(count).context("STORE_CORRUPT: negative row count")
        };
        Ok(json!({
            "researchId": rid,
            "title": title,
            "createdAt": created,
            "updatedAt": updated,
            "contextId": context,
            "productCount": product_count,
            "productRefs": refs.into_iter().take(100).collect::<Vec<_>>(),
            "productRefsTruncated": total_refs>100,
            "eventCount": counts("events")?,
            "evidenceCount": counts("evidence")?,
            "noteCount": counts("notes")?
        }))
    }
    fn cursor(
        &self,
        value: Option<&Value>,
        section: &str,
        rid: Option<&str>,
    ) -> Result<Option<Value>> {
        let Some(reference) = value else {
            return Ok(None);
        };
        let reference = reference
            .as_str()
            .ok_or_else(|| fail(Code::InvalidArgument, "cursor must be a string"))?;
        let row: Option<(String, Option<String>, String, String)> = self
            .tx
            .query_row(
                "SELECT section,research_id,state_json,expires_at FROM cursors WHERE id=?",
                [reference],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((stored_section, research, state, expires)) = row else {
            return Err(fail(Code::InvalidReference, "cursor is unavailable"));
        };
        if stored_section != section || research.as_deref() != rid || expires <= now() {
            return Err(fail(Code::InvalidReference, "cursor binding mismatch"));
        }
        Ok(Some(
            serde_json::from_str(&state).context("STORE_CORRUPT: invalid cursor state")?,
        ))
    }
    fn new_cursor(&mut self, section: &str, rid: Option<&str>, state: &Value) -> Result<String> {
        let reference = id("cursor");
        let expires = (Utc::now() + Duration::minutes(CURSOR_MINUTES))
            .to_rfc3339_opts(SecondsFormat::Millis, true);
        self.tx.execute(
            "INSERT INTO cursors(id,section,research_id,state_json,expires_at) VALUES(?,?,?,?,?)",
            params![reference, section, rid, canonical(state), expires],
        )?;
        Ok(reference)
    }
}

fn observation_time(evidence: &[Value]) -> String {
    evidence
        .iter()
        .filter_map(|v| v.get("observedAt").and_then(Value::as_str))
        .filter_map(normalize_time)
        .min()
        .unwrap_or_else(now)
}
fn ensure_research(conn: &Connection, rid: &str, context_id: &str) -> Result<()> {
    if research_context_id(conn, rid)? != context_id {
        return Err(fail(
            Code::ContextChanged,
            "research belongs to another context",
        ));
    }
    Ok(())
}
fn research_context(conn: &Connection, rid: &str) -> Result<Value> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT context_json FROM researches WHERE id=?",
            [rid],
            |r| r.get(0),
        )
        .optional()?;
    serde_json::from_str(
        &raw.ok_or_else(|| fail(Code::ResearchExpired, "research is unavailable"))?,
    )
    .context("STORE_CORRUPT: invalid context")
}
fn metadata(conn: &Connection, key: &str) -> Result<Option<Value>> {
    let raw: Option<String> = conn
        .query_row("SELECT value_json FROM meta WHERE key=?", [key], |r| {
            r.get(0)
        })
        .optional()?;
    raw.map(|v| serde_json::from_str(&v).context("STORE_CORRUPT: invalid metadata"))
        .transpose()
}
fn validate_ref_kind(kind: &str) -> Result<()> {
    if !matches!(
        kind,
        "product"
            | "image"
            | "review"
            | "search_ref"
            | "search_cursor"
            | "search_refinements"
            | "product_cursor"
            | "review_search"
            | "review_cursor"
            | "review_seen"
    ) {
        return Err(fail(Code::InvalidArgument, "unknown reference kind"));
    }
    Ok(())
}
fn get_ref(conn: &Connection, reference: &str, kind: &str) -> Result<StoredRef> {
    validate_ref_kind(kind)?;
    let row: Option<(String, String, String, Option<String>)> = conn
        .query_row(
            "SELECT research_id,context_id,value_json,expires_at FROM refs WHERE id=? AND kind=?",
            params![reference, kind],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let (research_id, context_id, raw, expires) =
        row.ok_or_else(|| fail(Code::InvalidReference, "reference is unavailable"))?;
    if expires.as_deref().is_some_and(|e| e <= now().as_str()) {
        return Err(fail(Code::InvalidReference, "reference expired"));
    }
    if research_context_id(conn, &research_id)? != context_id {
        return Err(fail(
            Code::ContextChanged,
            "reference belongs to another context",
        ));
    }
    Ok(StoredRef {
        research_id,
        context_id,
        value: serde_json::from_str(&raw).context("STORE_CORRUPT: invalid reference")?,
    })
}
fn validate_product_refs(conn: &Connection, rid: &str, refs: &[String]) -> Result<()> {
    let context_id = research_context_id(conn, rid)?;
    for reference in refs {
        let stored = get_ref(conn, reference, "product")?;
        if stored.research_id != rid {
            return Err(fail(Code::InvalidReference, "foreign product reference"));
        }
        if stored.context_id != context_id {
            return Err(fail(
                Code::ContextChanged,
                "product reference belongs to another context",
            ));
        }
    }
    Ok(())
}
fn all_product_refs(conn: &Connection, rid: &str) -> Result<BTreeSet<String>> {
    let mut stmt=conn.prepare("SELECT id FROM refs WHERE research_id=? AND context_id=(SELECT context_id FROM researches WHERE id=?) AND kind='product' AND (expires_at IS NULL OR expires_at>?) ORDER BY id")?;
    Ok(stmt
        .query_map(params![rid, rid, now()], |r| r.get(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?)
}
fn input_limit(args: &Value, default: usize, max: usize) -> Result<usize> {
    match args.get("limit") {
        None => Ok(default),
        Some(v) => v
            .as_u64()
            .filter(|v| *v >= 1 && *v <= max as u64)
            .map(|v| v as usize)
            .ok_or_else(|| fail(Code::InvalidArgument, format!("limit must be 1..{max}"))),
    }
}
fn selection(value: Option<&Value>, max: usize, name: &str) -> Result<Vec<String>> {
    let selected = strings(value)?;
    if selected.is_empty()
        || selected.len() > max
        || selected.iter().collect::<BTreeSet<_>>().len() != selected.len()
    {
        return Err(fail(
            Code::InvalidArgument,
            format!("{name} requires 1..{max} unique references"),
        ));
    }
    Ok(selected)
}
fn bounded_section(section: &str, values: Vec<Value>, next: Value, budget: usize) -> Result<Value> {
    let value = json!({"section":section,"payload":values,"nextCursor":next});
    if crate::research::response::serialized_utf16_len(&value)? > budget {
        return Err(fail(
            Code::ResultTooLarge,
            "journal selection exceeds the response budget",
        ));
    }
    Ok(value)
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
        .ok_or_else(|| fail(Code::SourceChanged, "freelist exceeds page count"))?;
    u64::try_from(live)
        .context("STORE_CORRUPT: negative live page count")?
        .checked_mul(page_size(conn)?)
        .ok_or_else(|| fail(Code::SourceChanged, "storage size overflow"))
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
        fail(Code::StorageFull, "write exceeds the journal page bound")
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

fn prepare_evidence(
    evidence: &[Value],
    context_id: &str,
    observed: &str,
) -> Result<Vec<(String, String)>> {
    let mut prepared = Vec::with_capacity(evidence.len());
    for input in evidence {
        let obj = input
            .as_object()
            .ok_or_else(|| fail(Code::InvalidArgument, "evidence must be objects"))?;
        let eid = obj
            .get("evidenceRef")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| id("evidence"));
        let evidence_context = obj
            .get("contextId")
            .and_then(Value::as_str)
            .unwrap_or(context_id);
        if evidence_context != context_id {
            return Err(fail(
                Code::ContextChanged,
                "evidence belongs to another context",
            ));
        }
        let source_kind = obj
            .get("sourceKind")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(Code::InvalidArgument, "evidence sourceKind is required"))?;
        if !matches!(source_kind, "ozon_page" | "local_journal") {
            return Err(fail(Code::InvalidArgument, "invalid evidence sourceKind"));
        }
        let source_url = obj.get("sourceUrl").cloned().unwrap_or(Value::Null);
        if source_kind == "local_journal" && !source_url.is_null() {
            return Err(fail(
                Code::InvalidArgument,
                "local journal evidence cannot have sourceUrl",
            ));
        }
        let value = json!({
            "evidenceRef": eid.clone(),
            "sourceKind": source_kind,
            "sourceUrl": source_url,
            "observedAt": obj.get("observedAt").and_then(Value::as_str).and_then(normalize_time).unwrap_or_else(|| observed.to_owned()),
            "contextId": context_id,
            "sku": obj.get("sku").cloned().unwrap_or(Value::Null),
            "facts": obj.get("facts").cloned().unwrap_or_else(|| json!([]))
        });
        prepared.push((eid, canonical(&value)));
    }
    Ok(prepared)
}

fn insert_record(
    tx: &Connection,
    research_id: &str,
    kind: &str,
    summary: &str,
    product_refs: &[String],
    evidence: &[(String, String)],
    observed: &str,
) -> Result<()> {
    let mut evidence_ids = Vec::with_capacity(evidence.len());
    for (eid, value) in evidence {
        tx.execute(
            "INSERT INTO evidence(id,research_id,seq,value_json) VALUES(?,?,(SELECT COALESCE(MAX(seq),0)+1 FROM evidence WHERE research_id=?),?)",
            params![eid, research_id, research_id, value],
        )?;
        evidence_ids.push(eid.clone());
    }
    let count = product_refs
        .len()
        .div_ceil(36)
        .max(evidence_ids.len().div_ceil(100))
        .max(1);
    let mut product_chunks = product_refs.chunks(36);
    let mut evidence_chunks = evidence_ids.chunks(100);
    for _ in 0..count {
        insert_event(
            tx,
            research_id,
            &json!({
                "eventId": id("event"), "kind": kind, "observedAt": observed,
                "summary": summary, "productRefs": product_chunks.next().unwrap_or_default(),
                "evidenceRefs": evidence_chunks.next().unwrap_or_default()
            }),
        )?;
    }
    tx.execute(
        "UPDATE researches SET updated_at=? WHERE id=?",
        params![now(), research_id],
    )?;
    Ok(())
}

/// All event producers pass through the public read contract so successful
/// operations cannot leave schema-invalid or unaddressable event history.
fn insert_event(conn: &Connection, research_id: &str, event: &Value) -> Result<()> {
    let envelope = json!({
        "schemaVersion": "3", "researchId": research_id,
        "data": {"section": "events","payload": [event],"nextCursor": null},
        "context": research_context(conn,research_id)?,
        "observedAt": string_field(event,"observedAt")?, "evidence": [], "warnings": []
    });
    crate::contracts::validate_output("ozon_get_research", &envelope)?;
    crate::research::response::bounded_value(&envelope)?;
    conn.execute(
        "INSERT INTO events(id,research_id,seq,kind,observed_at,summary,product_refs,evidence_refs) VALUES(?,?,(SELECT COALESCE(MAX(seq),0)+1 FROM events WHERE research_id=?),?,?,?,?,?)",
        params![string_field(event,"eventId")?,research_id,research_id,string_field(event,"kind")?,string_field(event,"observedAt")?,string_field(event,"summary")?,canonical(&event["productRefs"]),canonical(&event["evidenceRefs"])]
    )?;
    Ok(())
}

fn validate_image_refs(
    conn: &Connection,
    research_id: &str,
    context_id: &str,
    item: &Value,
) -> Result<()> {
    for reference in strings(item.get("imageRefs"))? {
        let stored = get_ref(conn, &reference, "image")?;
        if stored.research_id != research_id {
            return Err(fail(Code::InvalidReference, "foreign image reference"));
        }
        if stored.context_id != context_id {
            return Err(fail(
                Code::ContextChanged,
                "image reference belongs to another context",
            ));
        }
        if stored.value.get("url").and_then(Value::as_str).is_none() {
            return Err(fail(Code::SourceChanged, "image reference has no URL"));
        }
    }
    Ok(())
}

fn string_field<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    v.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| fail(Code::InvalidArgument, format!("missing {k}")))
}
fn strings(v: Option<&Value>) -> Result<Vec<String>> {
    v.map(|v| {
        v.as_array()
            .ok_or_else(|| fail(Code::InvalidArgument, "expected array"))?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| fail(Code::InvalidArgument, "expected string"))
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
    .ok_or_else(|| fail(Code::ResearchExpired, "research is unavailable"))
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
        _ => return Err(fail(Code::InvalidArgument, "section")),
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
            return Err(fail(Code::InvalidReference, "foreign or missing reference"));
        }
    }
    Ok(())
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
        _ => return Err(fail(Code::InvalidArgument, "section")),
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
        return Err(fail(Code::InvalidConfiguration, "unsafe store path"));
    }
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    if m.uid() != unsafe { geteuid() } || m.mode() & 0o077 != 0 {
        return Err(fail(
            Code::InvalidConfiguration,
            "store path has insecure ownership or permissions",
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
CREATE TABLE IF NOT EXISTS cursors(id TEXT PRIMARY KEY,section TEXT NOT NULL,research_id TEXT REFERENCES researches(id) ON DELETE CASCADE,state_json TEXT NOT NULL,expires_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS refs_research_kind ON refs(research_id,kind);
CREATE INDEX IF NOT EXISTS researches_updated ON researches(updated_at DESC,id DESC);
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
    fn source_admission_and_failed_operation_do_not_retire_idle_history() {
        let d = private_tempdir();
        let mut journal = Journal::open(d.path()).unwrap();
        let active = journal.create_research("active", &context("c")).unwrap();
        let idle = journal.create_research("idle", &context("c")).unwrap();
        journal.conn.execute(
            "UPDATE researches SET updated_at='2000-01-01T00:00:00.000Z',protected_until=NULL WHERE id=?",
            [&idle],
        ).unwrap();
        journal.last_maintenance = None;
        journal.lease(&active).unwrap();
        assert!(journal.research_context(&idle).is_ok());
        let error = journal
            .commit_operation(
                Some(&active),
                || Err(fail(Code::Cancelled, "cancelled before commit")),
                |tx| tx.put_ref(&active, "product", &json!({"sku":"123"}), None),
            )
            .unwrap_err();
        assert_eq!(crate::error::code(&error), "CANCELLED");
        assert!(journal.research_context(&idle).is_ok());
        assert!(journal.last_maintenance.is_none());
    }

    #[test]
    fn persistence_and_foreign_refs() {
        let d = private_tempdir();
        let rid;
        let rf;
        {
            let mut s = Journal::open(d.path()).unwrap();
            rid = s.create_research("t", &context("c1")).unwrap();
            rf = s.put_ref(&rid, "product", &json!({"x":1}), None).unwrap();
        }
        let s = Journal::open(d.path()).unwrap();
        assert_eq!(s.get_ref(&rf, "product").unwrap().value, json!({"x":1}));
        assert!(
            s.ensure_research(&rid, "other")
                .unwrap_err()
                .downcast::<crate::error::RuntimeError>()
                .unwrap()
                .code
                .as_str()
                .starts_with("CONTEXT_CHANGED")
        );
    }

    fn search_item(product_ref: &str, sku: &str, price_type: &str, seller: Value) -> Value {
        json!({
            "productRef": product_ref,
            "sku": sku,
            "title": "Item",
            "url": format!("https://www.ozon.ru/product/{sku}/"),
            "availability": "unknown",
            "prices": [{"amountMinor": 100,"currency": "RUB","type": price_type,"condition": null,"evidenceRefs": ["generated"]}],
            "seller": seller,
            "matchesDisplayedPriceRange": null,
            "deliveryLabel": null,
            "rating": null,
            "reviewCount": null,
            "imageRefs": [],
            "evidenceRefs": ["generated"]
        })
    }

    #[test]
    fn note_idempotency_and_conflict() {
        let d = private_tempdir();
        let mut s = Journal::open(d.path()).unwrap();
        let r = s.create_research("t", &context("c")).unwrap();
        let product = s.put_ref(&r, "product", &json!({"sku":"1"}), None).unwrap();
        s.record(&r, "search", "q", std::slice::from_ref(&product), &[])
            .unwrap();
        let a = json!({"researchId":r,"operationId":"op","kind":"assessment","text":"ok","productRefs":[product]});
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
                .downcast::<crate::error::RuntimeError>()
                .unwrap()
                .code
                .as_str()
                .starts_with("CONFLICT")
        );
    }
    #[test]
    fn stable_paging() {
        let d = private_tempdir();
        let mut s = Journal::open(d.path()).unwrap();
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
        let mut s = Journal::open(d.path()).unwrap();
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
                .downcast::<crate::error::RuntimeError>()
                .unwrap()
                .code
                .as_str()
                .starts_with("STORAGE_FULL")
        );
    }

    #[test]
    fn maintenance_reclaims_space_and_preserves_newer_research() {
        let d = private_tempdir();
        let mut s = Journal::open(d.path()).unwrap();
        let old = s.create_research("old", &context("c")).unwrap();
        s.put_ref(&old, "product", &json!({"value":"x".repeat(200_000)}), None)
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
        s.put_ref(
            &newer,
            "product",
            &json!({"value":"y".repeat(200_000)}),
            None,
        )
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
        let mut s = Journal::open(d.path()).unwrap();
        let research = s.create_research("r", &context("c")).unwrap();
        s.put_ref(&research, "product", &json!({}), Some(0))
            .unwrap();
        s.commit_operation(
            Some(&research),
            || Ok(()),
            |tx| tx.new_cursor("events", Some(&research), &json!({"cutoff":0,"offset":0})),
        )
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
        let mut s = Journal::open(d.path()).unwrap();
        for index in 0..80 {
            let research = s
                .create_research(&format!("small-{index:02}"), &context("c"))
                .unwrap();
            s.put_ref(
                &research,
                "product",
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
        let mut s = Journal::open(d.path()).unwrap();
        let old = s.create_research("old", &context("c")).unwrap();
        s.put_ref(&old, "product", &json!({"value":"o".repeat(200_000)}), None)
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
            "product",
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
                "product",
                &json!({"value":"z".repeat(s.cap_bytes as usize)}),
                None,
            )
            .unwrap_err();
        assert!(
            error
                .downcast::<crate::error::RuntimeError>()
                .unwrap()
                .code
                .as_str()
                .starts_with("STORAGE_FULL")
        );
        assert!(s.research_context(&active).is_ok());
        assert!(s.storage_bytes().unwrap() <= s.cap_bytes);
    }

    #[test]
    fn invalid_writes_do_not_evict_idle_research() {
        let d = private_tempdir();
        let mut s = Journal::open(d.path()).unwrap();
        let victim = s.create_research("victim", &context("c")).unwrap();
        s.put_ref(
            &victim,
            "product",
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
                .downcast::<crate::error::RuntimeError>()
                .unwrap()
                .code
                .as_str()
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
            .downcast::<crate::error::RuntimeError>()
            .unwrap()
            .code
            .as_str()
            .starts_with("INVALID_ARGUMENT")
        );
        assert!(
            s.append_note(&json!({
                "researchId": target,
                "operationId": "invalid-ref",
                "kind": "assessment",
                "text": "invalid",
                "productRefs": ["missing"]
            }))
            .unwrap_err()
            .downcast::<crate::error::RuntimeError>()
            .unwrap()
            .code
            .as_str()
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
        let mut s = Journal::open(d.path()).unwrap();
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
            .commit_operation(
                Some(&research),
                || Ok(()),
                |journal| {
                    let tx = &journal.tx;
                    tx.execute(
                        "INSERT INTO meta(key,value_json) VALUES('underestimated',?)",
                        ["x".repeat(100_000)],
                    )?;
                    Ok(())
                },
            )
            .unwrap_err();

        assert!(
            error
                .downcast::<crate::error::RuntimeError>()
                .unwrap()
                .code
                .as_str()
                .starts_with("STORAGE_FULL")
        );
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
        let mut s = Journal::open(d.path()).unwrap();
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

        Journal::enforce_capacity_tx_with_limit(&tx, before - page, 1).unwrap();
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
    #[test]
    fn new_journal_is_versioned_and_preserves_the_old_store_and_artifacts() {
        let d = private_tempdir();
        let old_path = d.path().join("research.sqlite3");
        fs::write(&old_path, b"old private research bytes").unwrap();
        fs::write(d.path().join("user-observation.txt"), b"retain evidence").unwrap();
        let store = Journal::open(d.path()).unwrap();
        assert_eq!(store.db_path.file_name().unwrap(), "journal.sqlite3");
        assert_eq!(
            store
                .conn
                .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            JOURNAL_VERSION
        );
        assert_eq!(fs::read(old_path).unwrap(), b"old private research bytes");
        assert_eq!(
            fs::read(d.path().join("user-observation.txt")).unwrap(),
            b"retain evidence"
        );
        drop(store);
        let conn = Connection::open(d.path().join("journal.sqlite3")).unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
        drop(conn);
        let error = Journal::open(d.path()).err().unwrap();
        assert_eq!(crate::error::code(&error), "INVALID_ARGUMENT");
        let conn = Connection::open(d.path().join("journal.sqlite3")).unwrap();
        assert_eq!(
            conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn candidate_observations_are_immutable_and_product_count_is_unique_skus() {
        let d = private_tempdir();
        let mut store = Journal::open(d.path()).unwrap();
        let rid = store.create_research("candidates", &context("c")).unwrap();
        let mut refs = Vec::new();
        for price in ["regular", "original"] {
            let reference = store
                .commit_operation(
                    Some(&rid),
                    || Ok(()),
                    |tx| {
                        let reference = tx.put_ref(
                            &rid,
                            "product",
                            &json!({"sku":"123","url":"https://www.ozon.ru/product/123/"}),
                            None,
                        )?;
                        let mut rows = json!([search_item(&reference, "123", price, Value::Null)]);
                        tx.record_search(
                            &rid,
                            "search",
                            std::slice::from_ref(&reference),
                            &[],
                            &mut rows,
                        )?;
                        assert!(rows[0].get("novelty").is_none());
                        Ok(reference)
                    },
                )
                .unwrap();
            refs.push(reference);
        }
        let snapshots = store
            .commit_operation(
                Some(&rid),
                || Ok(()),
                |tx| tx.read(&json!({"researchId":rid,"section":"candidates","productRefs":refs})),
            )
            .unwrap();
        assert_eq!(snapshots["payload"][0]["prices"][0]["type"], "regular");
        assert_eq!(snapshots["payload"][1]["prices"][0]["type"], "original");
        assert_eq!(snapshots["payload"][0]["contextId"], "c");
        assert!(snapshots["payload"][0]["observedAt"].as_str().is_some());
        let summary = store
            .commit_operation(
                Some(&rid),
                || Ok(()),
                |tx| tx.read(&json!({"researchId":rid})),
            )
            .unwrap();
        assert_eq!(summary["payload"]["productCount"], 1);
        assert_eq!(summary["payload"]["eventCount"], 2);
        assert_eq!(
            summary["payload"]["productRefs"].as_array().unwrap().len(),
            2
        );
        assert!(
            store
                .commit_operation(
                    Some(&rid),
                    || Ok(()),
                    |tx| {
                        let mut rows =
                            json!([search_item(&refs[0], "123", "regular", Value::Null)]);
                        tx.record_search(&rid, "again", &refs[..1], &[], &mut rows)
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn variant_product_authority_is_valid_for_notes_without_an_event() {
        let d = private_tempdir();
        let mut store = Journal::open(d.path()).unwrap();
        let rid = store.create_research("variant", &context("c")).unwrap();
        let (variant,note)=store.commit_operation(Some(&rid),||Ok(()),|tx| {
            let variant=tx.put_ref(&rid,"product",&json!({"sku":"456","url":null}),None)?;
            let note=tx.append_note(&json!({"researchId":rid,"operationId":"variant-note","kind":"assessment","text":"variant was observed","productRefs":[variant]}))?;
            Ok((variant,note))
        }).unwrap();
        let value = store
            .commit_operation(
                Some(&rid),
                || Ok(()),
                |tx| {
                    tx.read(&json!({"researchId":rid,"section":"notes","noteIds":[note["noteId"]]}))
                },
            )
            .unwrap();
        assert_eq!(value["payload"][0]["productRefs"], json!([variant]));
        assert!(store.commit_operation(Some(&rid),||Ok(()),|tx|tx.append_note(&json!({"researchId":rid,"operationId":"missing","kind":"assessment","text":"bad","productRefs":["missing"]}))).is_err());
    }

    #[test]
    fn filtered_listing_cursor_keeps_query_and_does_not_skip_after_issued_row_update() {
        let d = private_tempdir();
        let mut store = Journal::open(d.path()).unwrap();
        let a = store.create_research("match A", &context("c")).unwrap();
        let b = store.create_research("match B", &context("c")).unwrap();
        let c = store.create_research("match C", &context("c")).unwrap();
        store.create_research("unrelated", &context("c")).unwrap();
        for (rid, time) in [
            (&a, "2026-09-01T00:00:01.000Z"),
            (&b, "2026-09-01T00:00:02.000Z"),
            (&c, "2026-09-01T00:00:03.000Z"),
        ] {
            store
                .conn
                .execute(
                    "UPDATE researches SET updated_at=? WHERE id=?",
                    params![time, rid],
                )
                .unwrap();
        }
        let first = store
            .commit_operation(
                None,
                || Ok(()),
                |tx| tx.list(&json!({"query":"match","limit":1})),
            )
            .unwrap();
        assert_eq!(first["researches"][0]["researchId"], c);
        store.commit_operation(Some(&c),||Ok(()),|tx|tx.append_note(&json!({"researchId":c,"operationId":"change-list-position","kind":"assessment","text":"updated"}))).unwrap();
        let second = store
            .commit_operation(
                None,
                || Ok(()),
                |tx| tx.list(&json!({"cursor":first["nextCursor"],"limit":1})),
            )
            .unwrap();
        assert_eq!(second["researches"][0]["researchId"], b);
        let third = store
            .commit_operation(
                None,
                || Ok(()),
                |tx| tx.list(&json!({"cursor":second["nextCursor"],"limit":1})),
            )
            .unwrap();
        assert_eq!(third["researches"][0]["researchId"], a);
        assert!(third["nextCursor"].is_null());
    }

    #[test]
    fn valid_large_notes_page_by_serialized_size_and_exact_selection_is_addressable() {
        for text in ["a".repeat(12_000), "\n".repeat(12_000), "😀".repeat(12_000)] {
            let d = private_tempdir();
            let mut store = Journal::open(d.path()).unwrap();
            let rid = store.create_research("large notes", &context("c")).unwrap();
            let note_ids=store.commit_operation(Some(&rid),||Ok(()),|tx| {
                (0..5).map(|n| tx.append_note(&json!({"researchId":rid,"operationId":format!("large-{n}"),"kind":"assessment","text":text})).map(|note|note["noteId"].clone())).collect::<Result<Vec<_>>>()
            }).unwrap();
            let mut cursor = None;
            let mut returned = Vec::new();
            loop {
                let mut args = json!({"researchId":rid,"section":"notes"});
                if let Some(cursor) = cursor {
                    args["cursor"] = cursor;
                }
                let page = store
                    .commit_operation(Some(&rid), || Ok(()), |tx| tx.read(&args))
                    .unwrap();
                assert!(
                    crate::research::response::serialized_utf16_len(&page).unwrap()
                        <= crate::research::response::METADATA_BUDGET_UNITS
                );
                let notes = page["payload"].as_array().unwrap();
                assert!(!notes.is_empty());
                for note in notes {
                    assert_eq!(note["text"], text);
                    returned.push(note["noteId"].clone());
                }
                cursor = if page["nextCursor"].is_null() {
                    None
                } else {
                    Some(page["nextCursor"].clone())
                };
                if cursor.is_none() {
                    break;
                }
            }
            assert_eq!(returned, note_ids);
            let one = store
                .commit_operation(
                    Some(&rid),
                    || Ok(()),
                    |tx| {
                        tx.read(
                            &json!({"researchId":rid,"section":"notes","noteIds":[note_ids[4]]}),
                        )
                    },
                )
                .unwrap();
            assert_eq!(one["payload"][0]["text"], text);
            assert!(one["nextCursor"].is_null());
            for invalid in [
                json!([]),
                json!([note_ids[0], note_ids[0]]),
                json!(["missing"]),
            ] {
                assert!(
                    store
                        .commit_operation(
                            Some(&rid),
                            || Ok(()),
                            |tx| tx.read(
                                &json!({"researchId":rid,"section":"notes","noteIds":invalid})
                            )
                        )
                        .is_err()
                );
            }
            for limit in [0, 26] {
                assert!(
                    store
                        .commit_operation(
                            Some(&rid),
                            || Ok(()),
                            |tx| tx
                                .read(&json!({"researchId":rid,"section":"notes","limit":limit}))
                        )
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn event_and_evidence_pages_are_whole_and_oversized_record_is_controlled() {
        let d = private_tempdir();
        let mut store = Journal::open(d.path()).unwrap();
        let events = store
            .create_research("large events", &context("c"))
            .unwrap();
        let evidence = store
            .create_research("large evidence", &context("c"))
            .unwrap();
        for n in 0..4 {
            // Each event is schema-valid, including all its long opaque evidence
            // references. Whole-record paging must also handle this shape.
            let records = (0..64)
                .map(|index| {
                    json!({
                        "evidenceRef": format!("event-evidence-{n}-{index}-{}", "r".repeat(300)),
                        "sourceKind": "local_journal", "facts": []
                    })
                })
                .collect::<Vec<_>>();
            store
                .commit_operation(
                    Some(&events),
                    || Ok(()),
                    |tx| tx.record(&events, "search", &"s".repeat(2_000), &[], &records),
                )
                .unwrap();
            store.commit_operation(Some(&evidence), || Ok(()), |tx| tx.record(&evidence, "search", "captured", &[], &[json!({
                "evidenceRef": format!("evidence-{n}"), "sourceKind": "local_journal",
                "facts": [{"fieldPath": "/first","value": "f".repeat(10_000)},{"fieldPath": "/second","value": "f".repeat(10_000)}]
            })])).unwrap();
        }
        for (rid, section) in [(&events, "events"), (&evidence, "evidence")] {
            let first = store
                .commit_operation(
                    Some(rid),
                    || Ok(()),
                    |tx| tx.read(&json!({"researchId":rid,"section":section})),
                )
                .unwrap();
            assert_eq!(first["payload"].as_array().unwrap().len(), 2);
            let second = store.commit_operation(Some(rid), || Ok(()), |tx| tx.read(&json!({"researchId":rid,"section":section,"cursor":first["nextCursor"]}))).unwrap();
            assert_eq!(second["payload"].as_array().unwrap().len(), 2);
            assert!(second["nextCursor"].is_null());
        }
        let large = store
            .create_research("oversized record", &context("c"))
            .unwrap();
        let facts = (0..6)
            .map(|n| json!({"fieldPath":format!("/part/{n}"),"value":"z".repeat(10_000)}))
            .collect::<Vec<_>>();
        store.commit_operation(Some(&large), || Ok(()), |tx| tx.record(&large, "search", "oversized", &[], &[json!({"evidenceRef":"oversized-evidence","sourceKind":"local_journal","facts":facts})])).unwrap();
        let error = store
            .commit_operation(
                Some(&large),
                || Ok(()),
                |tx| tx.read(&json!({"researchId":large,"section":"evidence"})),
            )
            .unwrap_err();
        assert_eq!(crate::error::code(&error), "RESULT_TOO_LARGE");
    }

    #[test]
    fn failed_response_validation_rolls_back_the_entire_observation_operation() {
        let d = private_tempdir();
        let mut store = Journal::open(d.path()).unwrap();
        let rid = store.create_research("atomic", &context("c")).unwrap();
        let before: String = store
            .conn
            .query_row(
                "SELECT protected_until FROM researches WHERE id=?",
                [&rid],
                |r| r.get(0),
            )
            .unwrap();
        for expected in ["RESULT_TOO_LARGE", "SOURCE_CHANGED"] {
            let error=store.commit_operation(Some(&rid),||Ok(()),|tx| {
            let reference=tx.put_ref(&rid,"product",&json!({"sku":"123","url":"https://www.ozon.ru/product/123/"}),None)?;
            let mut rows=json!([search_item(&reference,"123","regular",Value::Null)]);
            tx.record_search(&rid,"observed",&[reference],&[json!({"evidenceRef":"rolled-back","sourceKind":"local_journal","facts":[]})],&mut rows)?;
            tx.set_meta("operation-staging",&json!(true))?;
            let _=tx.new_cursor("notes",Some(&rid),&json!({"cutoff":0,"offset":0}))?;
            if expected == "SOURCE_CHANGED" {
                    crate::contracts::validate_output("ozon_get_research", &json!({"data":null}))?;
                    unreachable!("malformed envelope must fail validation");
                }
                crate::research::response::bounded_value(&json!({"text":"x".repeat(60_000)}))
        }).unwrap_err();
            assert_eq!(crate::error::code(&error), expected);
            for table in ["refs", "evidence", "events", "cursors"] {
                assert_eq!(
                    store
                        .conn
                        .query_row::<i64, _, _>(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r
                            .get(0))
                        .unwrap(),
                    0
                );
            }
            assert_eq!(store.meta("operation-staging").unwrap(), None);
            let after: String = store
                .conn
                .query_row(
                    "SELECT protected_until FROM researches WHERE id=?",
                    [&rid],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(before, after);
        }
    }

    #[test]
    fn cancelled_commit_rolls_back_quota_eviction_and_all_staged_rows() {
        use std::cell::Cell;
        let d = private_tempdir();
        let mut store = Journal::open(d.path()).unwrap();
        let idle = store.create_research("idle", &context("c")).unwrap();
        store
            .put_ref(
                &idle,
                "product",
                &json!({"sku":"old","padding":"x".repeat(100_000)}),
                None,
            )
            .unwrap();
        let active = store.create_research("active", &context("c")).unwrap();
        store
            .conn
            .execute(
                "UPDATE researches SET protected_until=NULL WHERE id=?",
                [&idle],
            )
            .unwrap();
        store.reclaim_space().unwrap();
        store.set_cap_bytes(store.storage_bytes().unwrap());
        let checks = Cell::new(0);
        let error = store
            .commit_operation(
                Some(&active),
                || {
                    checks.set(checks.get() + 1);
                    if checks.get() == 2 {
                        return Err(fail(Code::Cancelled, "cancelled before commit"));
                    }
                    Ok(())
                },
                |tx| {
                    tx.put_ref(
                        &active,
                        "product",
                        &json!({"sku":"new","padding":"y".repeat(80_000)}),
                        None,
                    )?;
                    Ok(())
                },
            )
            .unwrap_err();
        assert_eq!(crate::error::code(&error), "CANCELLED");
        assert_eq!(checks.get(), 2);
        assert!(store.research_context(&idle).is_ok());
        assert_eq!(
            store
                .conn
                .query_row::<i64, _, _>(
                    "SELECT COUNT(*) FROM refs WHERE research_id=?",
                    [&active],
                    |r| r.get(0)
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn periodic_maintenance_runs_between_successful_operations_and_rolls_back_with_failure() {
        let d = private_tempdir();
        let mut store = Journal::open(d.path()).unwrap();
        let idle = store.create_research("old", &context("c")).unwrap();
        store.conn.execute("UPDATE researches SET updated_at='2000-01-01T00:00:00.000Z',protected_until=NULL WHERE id=?",[&idle]).unwrap();
        store.last_maintenance = None;
        let error = store
            .commit_operation(
                None,
                || Ok(()),
                |_| Err::<(), _>(fail(Code::InvalidArgument, "failed build")),
            )
            .unwrap_err();
        assert_eq!(crate::error::code(&error), "INVALID_ARGUMENT");
        assert!(store.research_context(&idle).is_ok());
        store.commit_operation(None, || Ok(()), |_| Ok(())).unwrap();
        assert!(store.research_context(&idle).is_err());
    }

    #[test]
    fn per_item_savepoints_keep_successes_and_roll_back_failed_nested_items() {
        let d = private_tempdir();
        let mut store = Journal::open(d.path()).unwrap();
        let rid = store.create_research("batch", &context("c")).unwrap();
        let kept=store.commit_operation(Some(&rid),||Ok(()),|tx| {
            let first=tx.stage(|item| item.put_ref(&rid,"product",&json!({"sku":"1"}),None))?;
            let failed=tx.stage(|item| {
                let orphan=item.put_ref(&rid,"product",&json!({"sku":"2"}),None)?;
                item.record(&rid,"products","failed item",&[orphan],&[json!({"evidenceRef":"failed-item","sourceKind":"local_journal","facts":[]})])?;
                Err::<(),_>(fail(Code::SourceChanged,"item decoding failed"))
            }).unwrap_err();
            assert_eq!(crate::error::code(&failed),"SOURCE_CHANGED");
            let second=tx.stage(|item| {
                let reference=item.put_ref(&rid,"product",&json!({"sku":"3"}),None)?;
                assert!(item.stage(|nested| {
                    nested.put_ref(&rid,"product",&json!({"sku":"4"}),None)?;
                    Err::<(),_>(fail(Code::InvalidReference,"nested item failed"))
                }).is_err());
                Ok(reference)
            })?;
            Ok(vec![first,second])
        }).unwrap();
        assert_eq!(
            all_product_refs(&store.conn, &rid).unwrap(),
            kept.into_iter().collect()
        );
        for table in ["events", "evidence"] {
            assert_eq!(
                store
                    .conn
                    .query_row::<i64, _, _>(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r
                        .get(0))
                    .unwrap(),
                0
            );
        }
        let error = store
            .commit_operation(
                Some(&rid),
                || Ok(()),
                |tx| {
                    tx.set_meta("poisoned-stage", &json!(true))?;
                    // Storage failures are fatal even if the item loop catches them.
                    let _ = tx.stage(|item| {
                        item.put_ref(&rid, "product", &json!({"sku":"5"}), None)?;
                        Err::<(), _>(fail(Code::StorageFull, "storage admission failed"))
                    });
                    Ok(())
                },
            )
            .unwrap_err();
        assert_eq!(crate::error::code(&error), "STORAGE_FULL");
        assert_eq!(store.meta("poisoned-stage").unwrap(), None);
        assert_eq!(all_product_refs(&store.conn, &rid).unwrap().len(), 2);
    }

    #[test]
    fn large_batch_event_chunks_keep_all_authority_and_read_as_schema_valid_history() {
        for (evidence_count, product_count) in [(101_usize, 37), (400, 8)] {
            let d = private_tempdir();
            let mut store = Journal::open(d.path()).unwrap();
            let rid = store.create_research("large batch", &context("c")).unwrap();
            let observed = "2026-09-30T00:00:00.000Z";
            let evidence=(0..evidence_count).map(|index|json!({
                "evidenceRef": format!("batch-evidence-{index}"), "sourceKind": "local_journal",
                "observedAt": observed, "facts": [{"fieldPath": "/title","value": "Observed"}]
            })).collect::<Vec<_>>();
            let products = store
                .commit_operation(
                    Some(&rid),
                    || Ok(()),
                    |tx| {
                        let products = (0..product_count)
                            .map(|n| {
                                tx.put_ref(&rid, "product", &json!({"sku":format!("{}",n+1)}), None)
                            })
                            .collect::<Result<Vec<_>>>()?;
                        tx.record(
                            &rid,
                            "products",
                            "Captured product batch",
                            &products,
                            &evidence,
                        )?;
                        Ok(products)
                    },
                )
                .unwrap();
            let mut seen_products = Vec::new();
            let mut seen_evidence = Vec::new();
            let mut event_count = 0;
            let mut cursor = None;
            loop {
                let mut args = json!({"researchId":rid,"section":"events","limit":1});
                if let Some(cursor) = cursor {
                    args["cursor"] = cursor;
                }
                let data = store
                    .commit_operation(Some(&rid), || Ok(()), |tx| tx.read(&args))
                    .unwrap();
                let envelope = json!({"schemaVersion": "3","researchId": rid,"data": data,
                    "context": context("c"),"observedAt": observed,"evidence": [],"warnings": []});
                crate::contracts::validate_output("ozon_get_research", &envelope).unwrap();
                crate::research::response::bounded_value(&envelope).unwrap();
                for event in data["payload"].as_array().unwrap() {
                    assert_eq!(event["kind"], "products");
                    assert_eq!(event["observedAt"], observed);
                    assert_eq!(event["summary"], "Captured product batch");
                    seen_products.extend(strings(event.get("productRefs")).unwrap());
                    seen_evidence.extend(strings(event.get("evidenceRefs")).unwrap());
                    event_count += 1;
                }
                cursor = if data["nextCursor"].is_null() {
                    None
                } else {
                    Some(data["nextCursor"].clone())
                };
                if cursor.is_none() {
                    break;
                }
            }
            assert_eq!(event_count, evidence_count.div_ceil(100));
            assert_eq!(seen_products, products);
            assert_eq!(
                seen_evidence,
                evidence
                    .iter()
                    .map(|row| row["evidenceRef"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                all_product_refs(&store.conn, &rid).unwrap().len(),
                product_count
            );
            let mut returned_evidence = Vec::new();
            let mut cursor = None;
            loop {
                let mut args = json!({"researchId":rid,"section":"evidence","limit":25});
                if let Some(cursor) = cursor {
                    args["cursor"] = cursor;
                }
                let data = store
                    .commit_operation(Some(&rid), || Ok(()), |tx| tx.read(&args))
                    .unwrap();
                crate::contracts::validate_output(
                    "ozon_get_research",
                    &json!({"schemaVersion": "3","researchId": rid,"data": data,
                        "context": context("c"),"observedAt": observed,"evidence": [],"warnings": []}),
                )
                .unwrap();
                returned_evidence.extend(
                    data["payload"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|row| row["evidenceRef"].as_str().unwrap().to_owned()),
                );
                cursor = if data["nextCursor"].is_null() {
                    None
                } else {
                    Some(data["nextCursor"].clone())
                };
                if cursor.is_none() {
                    break;
                }
            }
            assert_eq!(returned_evidence, seen_evidence);
            let error=store.commit_operation(Some(&rid),||Ok(()),|tx| {
                let orphan=tx.put_ref(&rid,"product",&json!({"sku":"999"}),None)?;
                tx.record(&rid,"unsupported-event-kind","Invalid event",&[orphan],&[json!({"evidenceRef":"orphan-evidence","sourceKind":"local_journal","facts":[]})])
            }).unwrap_err();
            assert_eq!(crate::error::code(&error), "SOURCE_CHANGED");
            assert_eq!(
                all_product_refs(&store.conn, &rid).unwrap().len(),
                product_count
            );
            assert_eq!(
                store
                    .conn
                    .query_row::<i64, _, _>(
                        "SELECT COUNT(*) FROM evidence WHERE research_id=?",
                        [&rid],
                        |r| r.get(0)
                    )
                    .unwrap(),
                evidence_count as i64
            );
            assert_eq!(
                store
                    .conn
                    .query_row::<i64, _, _>(
                        "SELECT COUNT(*) FROM events WHERE research_id=?",
                        [&rid],
                        |r| r.get(0)
                    )
                    .unwrap(),
                event_count as i64
            );
        }
    }
}
