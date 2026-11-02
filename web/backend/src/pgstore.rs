//! Postgres-backed workspace storage.
//!
//! The filesystem store in `main.rs` is the system of record for local
//! development: one JSON document per workspace, an in-process LRU, and an
//! in-process `Mutex` guarding the optimistic-concurrency check. That last
//! part is exactly what stops working the moment the service runs on more
//! than one machine, which is what Cloud Run does by default.
//!
//! So this backend does not merely move the bytes to Postgres, it moves the
//! *concurrency control* there too. Every mutation is a single statement whose
//! `WHERE` clause carries the expected revision, so the database performs the
//! compare-and-swap under its own row lock:
//!
//! ```sql
//! UPDATE workspaces SET ..., revision = revision + 1
//!  WHERE id = $1 AND revision = $2
//! ```
//!
//! `rows_affected == 0` is the conflict signal. Two instances racing on the
//! same base revision therefore resolve to exactly one winner and one 409, and
//! no lost update is possible regardless of how many processes are running.
//!
//! Nothing is cached in RAM. A workspace read always goes to the database,
//! because a stale cache on instance A is indistinguishable from a lost write
//! by instance B from the client's point of view. The row is small (bounded by
//! `MAX_BODY`) and lookups are by primary key.

use std::str::FromStr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use postgres::{Client, Config, Row};
use serde_json::Value;

use crate::assetstore::{
    check_limits, chunk_fits, pending_refusal, session_refusal, settle_put, storage_full, AssetError, AssetIndexView, AssetMeta,
    PendingUpload, Processed, UploadReceipt, UploadSession, ASSET_TTL_MS, MAX_EXPIRED_NAMES, MAX_STAGED_BYTES, MAX_UPLOAD_RECEIPTS,
    UNKNOWN_UPLOAD_SESSION,
    UPLOAD_FINISH_CLAIM_MS, UPLOAD_RECEIPT_TTL_MS, UPLOAD_SESSION_IDLE_MS,
};
use crate::feedback::FeedbackState;
use crate::rendercache;
use crate::{
    coalesces_with_current, default_workspace_name, edit_events, resolve_edit, revision_written_at, superseded_source,
    update_mark, CreationStats, CREATION_BUCKET_MILLIS,
    CREATION_WEEK_BUCKETS, CREATION_WINDOW_BUCKETS, sanitize_name, sanitize_plate, sanitize_tolerance,
    unix_millis, valid_workspace_id, validate_source, EditResult, RevisionInfo, RevisionMark, UpdateError, Workspace,
    WorkspaceUpdate, DEFAULT_TOLERANCE, MAX_BODY, MAX_EVENTS, MAX_REVISION_HISTORY,
    MAX_REVISION_HISTORY_BYTES,
    MAX_WORKSPACES,
};

/// Idle connections kept alive between requests. The server is thread-per-
/// connection with `MAX_CONNECTIONS = 256`, but only a small fraction of those
/// threads touch the database at any instant, and Cloud SQL charges for
/// connection slots. Opening one per request instead would add a TLS/startup
/// round trip to every workspace read.
const MAX_IDLE_CONNECTIONS: usize = 8;
/// Connections this process may hold open **in total**, idle or checked out.
///
/// This is the important bound, and it is about the database rather than about
/// us: a `db-f1-micro` Cloud SQL instance allows on the order of 25 client
/// connections. With 256 request threads, 32 long pollers each issuing 26
/// sequential probes, and several Cloud Run instances sharing one database,
/// an unbounded pool reaches `FATAL: sorry, too many clients already` — a
/// server-side error, so it is not retried, and it surfaces as a 500 for
/// *every* caller rather than as a queue for a few.
///
/// Capping checkouts turns that into back-pressure: threads wait briefly for a
/// connection instead of the database refusing everyone at once.
const MAX_TOTAL_CONNECTIONS: usize = 8;
/// How long a request thread waits for a connection before giving up. Bounded
/// so a database outage sheds load instead of parking all 256 request threads;
/// well inside the browser's tolerance for a workspace read.
const CHECKOUT_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the background sweeper enforces the TTL and the count cap. The
/// filesystem store prunes inline on every create; doing that against SQL
/// would put two `DELETE`s on the hot path of workspace creation.
const SWEEP_INTERVAL: Duration = Duration::from_secs(600);
/// Advisory-lock key that serialises schema migration across instances. Any
/// stable arbitrary constant works; this one is "reopenscad" in hex-ish form.
const MIGRATION_LOCK_KEY: i64 = 0x_7265_6f70_656e_0001;
/// Advisory-lock namespace (first key of the two-`int4` form) for per-design
/// render-cache stores; the second key is `hashtext(design id)`.
const RENDER_CACHE_LOCK_SPACE: i32 = 0x7263_0001;
/// Namespace of the single lock that serialises the render cache's total trim.
const RENDER_CACHE_TOTAL_LOCK: i32 = 0x7263_0002;
/// Advisory lock serialising asset writes' deployment-wide byte check, taken
/// after the design's row lock.
const ASSET_TOTAL_LOCK: i32 = 0x6173_0001;
/// Unused one-time upload URLs kept at once, across the deployment.
const MAX_PENDING_UPLOAD_TOKENS: i64 = 1024;

const SELECT_COLUMNS: &str =
    "id, name, code, revision, created_at, updated_at, plate, tolerance, preview, share_token, revision_name, revision_source, events";

/// The retained-workspace ceiling, from `REOPENSCAD_MAX_WORKSPACES`.
///
/// Defaults to the filesystem store's `MAX_WORKSPACES`, but unlike that one it
/// governs a shared, multi-tenant table (see [`PgWorkspaceStore::prune`]), so
/// it has to be raisable at deploy time. Read once and cached; a value below
/// the default is ignored, because lowering it can only evict other people's
/// work faster.
fn max_workspaces() -> usize {
    static CACHED: AtomicUsize = AtomicUsize::new(0);
    let cached = CACHED.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let configured = std::env::var("REOPENSCAD_MAX_WORKSPACES")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|limit| *limit >= MAX_WORKSPACES)
        .unwrap_or(MAX_WORKSPACES);
    CACHED.store(configured, Ordering::Relaxed);
    configured
}

// ---------------------------------------------------------------------------
// Connection pool
// ---------------------------------------------------------------------------

/// A hand-rolled pool: a bounded stack of idle `postgres::Client`s, plus a
/// counting semaphore bounding how many exist at all.
///
/// The `postgres` crate is the synchronous driver, which matches this server's
/// blocking thread-per-connection model — `sqlx`/`tokio-postgres` would drag a
/// whole async runtime into a codebase that has none.
struct Pool {
    config: Config,
    idle: Mutex<Vec<Client>>,
    /// Connections currently checked out or sitting in `idle`. Guarded rather
    /// than atomic because waiters block on `released`.
    leased: Mutex<usize>,
    released: Condvar,
}

/// Holds one slot of the `MAX_TOTAL_CONNECTIONS` budget for as long as a
/// connection is checked out, and hands it back — waking one waiter — however
/// the caller leaves, including by panicking.
struct Lease<'pool> {
    pool: &'pool Pool,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if let Ok(mut leased) = self.pool.leased.lock() {
            *leased = leased.saturating_sub(1);
        }
        self.pool.released.notify_one();
    }
}

impl Pool {
    fn new(config: Config) -> Self {
        Self {
            config,
            idle: Mutex::new(Vec::new()),
            leased: Mutex::new(0),
            released: Condvar::new(),
        }
    }

    /// Waits for a slot in the connection budget, or gives up.
    ///
    /// Giving up is a 503-shaped answer rather than an attempt to open a
    /// connection anyway: exceeding the database's own `max_connections` would
    /// fail every caller, not just this one.
    fn lease(&self) -> Result<Lease<'_>, String> {
        let unavailable = || "Design store is unavailable.".to_string();
        let mut leased = self.leased.lock().map_err(|_| unavailable())?;
        let deadline = Instant::now() + CHECKOUT_TIMEOUT;
        while *leased >= MAX_TOTAL_CONNECTIONS {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("The design store is busy. Retry shortly.".into());
            }
            let (guard, timeout) = self
                .released
                .wait_timeout(leased, remaining)
                .map_err(|_| unavailable())?;
            leased = guard;
            if timeout.timed_out() && *leased >= MAX_TOTAL_CONNECTIONS {
                return Err("The design store is busy. Retry shortly.".into());
            }
        }
        *leased += 1;
        Ok(Lease { pool: self })
    }

    /// Takes a connection against an already-held lease.
    fn take(&self) -> Result<Client, String> {
        loop {
            let candidate = self
                .idle
                .lock()
                .map_err(|_| "Design store is unavailable.".to_string())?
                .pop();
            match candidate {
                // A pooled connection can have been closed by the server (idle
                // timeout, Cloud SQL maintenance) while it sat here.
                Some(client) if !client.is_closed() => return Ok(client),
                Some(_) => continue,
                None => return connect(&self.config),
            }
        }
    }

    fn give_back(&self, client: Client) {
        if client.is_closed() {
            return;
        }
        if let Ok(mut idle) = self.idle.lock() {
            if idle.len() < MAX_IDLE_CONNECTIONS {
                idle.push(client);
            }
        }
    }

    /// Runs `action` against a pooled connection, retrying it once on a fresh
    /// connection if the pooled one turned out to be dead. Pooled connections
    /// go stale routinely — Cloud SQL closes idle ones and cycles instances
    /// during maintenance — so without this a maintenance window would surface
    /// as a burst of 500s.
    ///
    /// The retry is at-least-once, not exactly-once: if the statement reached
    /// the server and only the *response* was lost, the retry runs it again.
    /// Every statement here is written so that this is harmless:
    ///
    /// * `update` is a compare-and-swap on `revision`. A re-run after a
    ///   successful first attempt matches zero rows, so the caller is told
    ///   "conflict" for a write that in fact landed. The client refetches and
    ///   finds its own change — a spurious 409, never a lost or doubled edit.
    /// * `update_settings` is an unconditional assignment of the same values,
    ///   so re-running it is a no-op.
    /// * `create` uses `ON CONFLICT DO NOTHING` and re-runs with a *new* id, so
    ///   a lost response can strand one unreferenced row. Nobody holds its URL,
    ///   and the TTL sweeper reclaims it.
    ///
    /// Reads are trivially idempotent.
    fn with<T>(
        &self,
        mut action: impl FnMut(&mut Client) -> Result<T, postgres::Error>,
    ) -> Result<T, String> {
        // Held for the whole call, including the retry, so the budget counts
        // connections rather than statements. Released by `Drop` on every exit
        // path, panics included.
        let _lease = self.lease()?;
        let mut client = self.take()?;
        match action(&mut client) {
            Ok(value) => {
                self.give_back(client);
                Ok(value)
            }
            Err(error) => {
                let broken = client.is_closed() || is_connection_error(&error);
                if !broken {
                    self.give_back(client);
                    return Err(describe(&error));
                }
                let mut fresh = connect(&self.config)?;
                match action(&mut fresh) {
                    Ok(value) => {
                        self.give_back(fresh);
                        Ok(value)
                    }
                    Err(error) => Err(describe(&error)),
                }
            }
        }
    }
}

/// A connection-level failure (as opposed to a statement the server rejected)
/// is the only case worth retrying; a syntax or constraint error would fail
/// identically on a new connection.
fn is_connection_error(error: &postgres::Error) -> bool {
    error.as_db_error().is_none()
}

/// Turns a driver error into something safe to put in a response body.
///
/// These strings reach anonymous callers as HTTP 500 bodies, and Postgres is
/// generous in its error text: a failed login renders as `FATAL: password
/// authentication failed for user "reopenscad"`, and a constraint violation
/// names tables and columns. None of that is the caller's business, so the
/// detail goes to the log — where an operator can see it — and the caller gets
/// a sentence.
fn describe(error: &postgres::Error) -> String {
    eprintln!("workspace storage error: {error}");
    "The design store is temporarily unavailable.".to_string()
}

#[cfg(feature = "postgres-tls")]
fn uses_unix_socket(config: &Config) -> bool {
    config
        .get_hosts()
        .iter()
        .any(|host| matches!(host, postgres::config::Host::Unix(_)))
}

/// Connection failures are logged in full and reported in the abstract, for
/// the same reason as [`describe`]: `Pool::take` runs on the request path, so
/// whatever this returns can end up in a response body, and a rejected login
/// names the database user. An operator reading the log sees the real cause.
fn connect_failed(reason: impl std::fmt::Display) -> String {
    eprintln!("workspace database connection failed: {reason}");
    "The design store is temporarily unavailable.".to_string()
}

#[cfg(not(feature = "postgres-tls"))]
fn connect(config: &Config) -> Result<Client, String> {
    config.connect(postgres::NoTls).map_err(connect_failed)
}

/// With the `postgres-tls` feature a TCP `DATABASE_URL` is encrypted; a unix
/// socket is not, because there is nothing on the wire to protect — on Cloud
/// Run the socket at `/cloudsql/PROJECT:REGION:INSTANCE` is served by the
/// in-sandbox Cloud SQL connector, which does its own authenticated TLS to the
/// instance.
#[cfg(feature = "postgres-tls")]
fn connect(config: &Config) -> Result<Client, String> {
    if uses_unix_socket(config) {
        return config.connect(postgres::NoTls).map_err(connect_failed);
    }
    let connector = native_tls::TlsConnector::new().map_err(connect_failed)?;
    config
        .connect(postgres_native_tls::MakeTlsConnector::new(connector))
        .map_err(connect_failed)
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct PgWorkspaceStore {
    pool: Arc<Pool>,
    nonce: Arc<AtomicU64>,
}

impl PgWorkspaceStore {
    /// Connects, migrates, and starts the background sweeper.
    ///
    /// Migration runs under a Postgres advisory lock so that N instances
    /// booting simultaneously (the normal Cloud Run cold-start pattern) cannot
    /// race each other through `CREATE TABLE`.
    pub fn open(url: &str) -> Result<Self, String> {
        let config = Config::from_str(url).map_err(|error| {
            format!("DATABASE_URL is not a valid Postgres connection string: {error}")
        })?;
        let pool = Arc::new(Pool::new(config));
        pool.with(migrate)?;
        let store = Self {
            pool: Arc::clone(&pool),
            nonce: Arc::new(AtomicU64::new(unix_millis())),
        };
        // One opportunistic sweep at boot, then on a timer. Deliberately not a
        // `SELECT *` of the table: both statements are set-based deletes.
        let sweeper = store.clone();
        let _ = thread::Builder::new()
            .name("reopenscad-sweeper".into())
            .spawn(move || loop {
                if let Err(error) = sweeper.prune() {
                    eprintln!("workspace sweep failed: {error}");
                }
                thread::sleep(SWEEP_INTERVAL);
            });
        Ok(store)
    }

    /// Enforces the same two limits the filesystem store enforces, as SQL.
    ///
    /// One thing changes meaning in the move to a shared database, and it is
    /// worth being explicit about: on disk the count cap was per-machine and
    /// the only workspaces it could evict were the developer's own. Here it is
    /// global across every user of the deployment, and creation is
    /// unauthenticated — so a burst of `MAX_WORKSPACES` new workspaces evicts
    /// the least recently touched ones belonging to *other people*. Per-IP rate
    /// limiting slows that down; it does not prevent it from a botnet.
    ///
    /// `REOPENSCAD_MAX_WORKSPACES` exists so an operator can raise the ceiling
    /// far above plausible abuse without rebuilding. Sizing it is a real
    /// decision, not a formality: see DEPLOY.md.
    /// There is deliberately no age limit here either; see the note on the
    /// filesystem store's `prune`. Rows go only when the ceiling is reached.
    fn prune(&self) -> Result<(), String> {
        let overflow = max_workspaces() as i64;
        self.pool.with(|client| {
            client.execute(
                "DELETE FROM workspaces WHERE id IN (
                     SELECT id FROM workspaces ORDER BY updated_at DESC, id DESC OFFSET $1
                 )",
                &[&overflow],
            )?;
            // Minute buckets only matter inside the 7-day window; keep a
            // day of slack so a slow sweep never trims a live one. The legacy
            // hourly table is no longer read; drain it.
            client.execute(
                "DELETE FROM workspace_creation_minutes WHERE minute < $1",
                &[&(current_minute() - (CREATION_WEEK_BUCKETS + CREATION_WINDOW_BUCKETS) as i64)],
            )?;
            client.execute("DELETE FROM workspace_creation_hours", &[])?;
            // Expired one-time upload URLs.
            client.execute(
                "DELETE FROM asset_upload_tokens WHERE expires_at < $1",
                &[&(unix_millis() as i64)],
            )?;
            // Idle chunked uploads, with their staged parts (cascade). A
            // chunk being appended holds the row lock, and the row no longer
            // matches once it commits; a finish claim refreshes updated_at.
            client.execute(
                "DELETE FROM asset_upload_sessions WHERE updated_at <= $1",
                &[&(unix_millis().saturating_sub(UPLOAD_SESSION_IDLE_MS) as i64)],
            )?;
            // Kept replies of finished uploads past their lifetime.
            client.execute(
                "DELETE FROM asset_upload_receipts WHERE created_at <= $1",
                &[&(unix_millis().saturating_sub(UPLOAD_RECEIPT_TTL_MS) as i64)],
            )?;
            // Stored renders past the total byte ceiling, least recently
            // used first. Per-design trimming happens on every store.
            client.execute(
                "DELETE FROM render_cache WHERE (workspace_id, cache_key) IN (
                     SELECT workspace_id, cache_key FROM (
                         SELECT workspace_id, cache_key,
                                SUM(bytes) OVER (ORDER BY last_used DESC, workspace_id, cache_key) AS running
                         FROM render_cache
                     ) ranked WHERE running > $1::BIGINT
                 )",
                &[&(rendercache::MAX_TOTAL_BYTES as i64)],
            )?;
            // Self-heal. Instances still running a binary from before the
            // counters (a rolling deploy) create rows without counting them,
            // and the migration seeded the counters only once. Neither number
            // may be lower than what the rows themselves prove.
            client.execute(
                "INSERT INTO reopenscad_counters (name, value)
                     SELECT 'workspaces_created', COUNT(*) FROM workspaces
                 ON CONFLICT (name)
                 DO UPDATE SET value = GREATEST(reopenscad_counters.value, EXCLUDED.value)",
                &[],
            )?;
            client.execute(
                "INSERT INTO workspace_creation_minutes (minute, created)
                     SELECT created_at / $1, COUNT(*) FROM workspaces
                     WHERE created_at >= $2
                     GROUP BY created_at / $1
                 ON CONFLICT (minute)
                 DO UPDATE SET created = GREATEST(workspace_creation_minutes.created, EXCLUDED.created)",
                &[
                    &(CREATION_BUCKET_MILLIS as i64),
                    &((current_minute() - CREATION_WEEK_BUCKETS as i64) * CREATION_BUCKET_MILLIS as i64),
                ],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    pub fn create_with_settings(
        &self,
        code: String,
        name: Option<String>,
        plate: Option<String>,
        tolerance: Option<f64>,
    ) -> Result<Workspace, String> {
        self.create_marked(code, name, plate, tolerance, RevisionMark::browser_created())
    }

    /// [`Self::create_with_settings`], naming revision 1.
    pub fn create_marked(
        &self,
        code: String,
        name: Option<String>,
        plate: Option<String>,
        tolerance: Option<f64>,
        mark: RevisionMark,
    ) -> Result<Workspace, String> {
        if code.len() > MAX_BODY {
            return Err("Design code exceeds the 2 MiB limit.".into());
        }
        let now = unix_millis() as i64;
        let name = sanitize_name(name);
        let plate = plate.as_deref().map(sanitize_plate).unwrap_or_default();
        let tolerance = tolerance.map(sanitize_tolerance).unwrap_or(DEFAULT_TOLERANCE);
        // Ids carry 112 bits of entropy, so a collision is theoretical; the
        // loop exists because `ON CONFLICT DO NOTHING` makes handling it free.
        for _ in 0..8 {
            let id = self.generate_id();
            let minute = now / CREATION_BUCKET_MILLIS as i64;
            // The counters move in the insert's transaction, so a created
            // workspace is always counted and a failed insert never is.
            let inserted = self.pool.with(|client| {
                let mut transaction = client.transaction()?;
                let inserted = transaction.execute(
                    "INSERT INTO workspaces
                         (id, name, code, revision, created_at, updated_at, plate, tolerance, revision_name, revision_source, events)
                     VALUES ($1, $2, $3, 1, $4, $4, $5, $6, $7, $8, '[]'::jsonb)
                     ON CONFLICT (id) DO NOTHING",
                    &[&id, &name, &code, &now, &plate, &tolerance, &mark.name, &mark.source],
                )?;
                if inserted == 1 {
                    // An upsert, so a missing counter row heals itself
                    // instead of silently not counting.
                    transaction.execute(
                        "INSERT INTO reopenscad_counters (name, value)
                         VALUES ('workspaces_created', 1)
                         ON CONFLICT (name)
                         DO UPDATE SET value = reopenscad_counters.value + 1",
                        &[],
                    )?;
                    transaction.execute(
                        "INSERT INTO workspace_creation_minutes (minute, created) VALUES ($1, 1)
                         ON CONFLICT (minute)
                         DO UPDATE SET created = workspace_creation_minutes.created + 1",
                        &[&minute],
                    )?;
                }
                transaction.commit()?;
                Ok(inserted)
            })?;
            if inserted == 1 {
                return Ok(Workspace {
                    id,
                    name,
                    code,
                    revision: 1,
                    created_at: now as u64,
                    updated_at: now as u64,
                    plate,
                    tolerance,
                    preview: String::new(),
                    share_token: String::new(),
                    revision_name: mark.name,
                    revision_source: mark.source,
                    events: Vec::new(),
                });
            }
        }
        Err("Could not allocate a design id.".into())
    }

    /// Workspaces ever created (eviction does not lower it) and created in
    /// the rolling last 24 hours and 7 days, to the minute. One round trip,
    /// three index reads.
    pub fn creation_stats(&self) -> Result<CreationStats, String> {
        let oldest = current_minute() - (CREATION_WINDOW_BUCKETS as i64 - 1);
        let oldest_week = current_minute() - (CREATION_WEEK_BUCKETS as i64 - 1);
        let row = self.pool.with(|client| {
            client.query_one(
                "SELECT
                     COALESCE((SELECT value FROM reopenscad_counters
                               WHERE name = 'workspaces_created'), 0),
                     COALESCE((SELECT SUM(created) FROM workspace_creation_minutes
                               WHERE minute >= $1), 0)::BIGINT,
                     COALESCE((SELECT SUM(created) FROM workspace_creation_minutes
                               WHERE minute >= $2), 0)::BIGINT",
                &[&oldest, &oldest_week],
            )
        })?;
        Ok(CreationStats {
            total: row.get::<_, i64>(0).max(0) as u64,
            last_24h: row.get::<_, i64>(1).max(0) as u64,
            last_7d: row.get::<_, i64>(2).max(0) as u64,
        })
    }

    /// A stored render of design `id`, marking it used. Errors are misses.
    pub fn cached_render(&self, id: &str, key: &str) -> Option<Vec<u8>> {
        if !valid_workspace_id(id) || !rendercache::valid_key(key) {
            return None;
        }
        let (id, key, now) = (id.to_string(), key.to_string(), unix_millis() as i64);
        self.pool
            .with(|client| {
                client.query_opt(
                    "UPDATE render_cache SET last_used = $3
                     WHERE workspace_id = $1 AND cache_key = $2
                     RETURNING body",
                    &[&id, &key, &now],
                )
            })
            .ok()
            .flatten()
            .map(|row| row.get::<_, Vec<u8>>(0))
    }

    /// Stores a render of design `id` and keeps only the design's most
    /// recently used [`rendercache::ENTRIES_PER_DESIGN`]. Best effort: a
    /// design deleted meanwhile fails the foreign key, which is fine.
    pub fn store_render(&self, id: &str, key: &str, body: &[u8]) {
        if !valid_workspace_id(id)
            || !rendercache::valid_key(key)
            || body.len() > rendercache::MAX_ENTRY_BYTES
        {
            return;
        }
        let (id, key, now) = (id.to_string(), key.to_string(), unix_millis() as i64);
        let bytes = body.len() as i64;
        let keep = rendercache::ENTRIES_PER_DESIGN as i64;
        let total_cap = rendercache::MAX_TOTAL_BYTES as i64;
        let stored = self.pool.with(|client| {
            let mut transaction = client.transaction()?;
            // Serialises stores of one design, across instances, so two
            // concurrent inserts cannot each trim before seeing the other
            // and leave more than the per-design limit.
            transaction.execute(
                "SELECT pg_advisory_xact_lock($1, hashtext($2))",
                &[&RENDER_CACHE_LOCK_SPACE, &id],
            )?;
            transaction.execute(
                "INSERT INTO render_cache (workspace_id, cache_key, body, bytes, created_at, last_used)
                 VALUES ($1, $2, $3, $4, $5, $5)
                 ON CONFLICT (workspace_id, cache_key)
                 DO UPDATE SET body = EXCLUDED.body, bytes = EXCLUDED.bytes, last_used = EXCLUDED.last_used",
                &[&id, &key, &body, &bytes, &now],
            )?;
            transaction.execute(
                "DELETE FROM render_cache WHERE workspace_id = $1 AND cache_key IN (
                     SELECT cache_key FROM render_cache WHERE workspace_id = $1
                     ORDER BY last_used DESC, cache_key OFFSET $2
                 )",
                &[&id, &keep],
            )?;
            // The total ceiling, enforced on the write that crosses it rather
            // than only by the sweeper. Taken last and only ever after a
            // design lock, so lock order is fixed and cannot deadlock; it
            // serialises the global trims so two never both delete for the
            // same overshoot.
            transaction.execute("SELECT pg_advisory_xact_lock($1, 0)", &[&RENDER_CACHE_TOTAL_LOCK])?;
            transaction.execute(
                "DELETE FROM render_cache WHERE (workspace_id, cache_key) IN (
                     SELECT workspace_id, cache_key FROM (
                         SELECT workspace_id, cache_key,
                                SUM(bytes) OVER (ORDER BY last_used DESC, workspace_id, cache_key) AS running
                         FROM render_cache
                     ) ranked WHERE running > $1::BIGINT
                 )",
                &[&total_cap],
            )?;
            transaction.commit()
        });
        if let Err(error) = stored {
            eprintln!("render cache store failed: {error}");
        }
    }

    pub fn get(&self, id: &str) -> Result<Option<Workspace>, String> {
        if !valid_workspace_id(id) {
            return Ok(None);
        }
        let owned = id.to_string();
        let row = self.pool.with(|client| {
            client.query_opt(
                &format!("SELECT {SELECT_COLUMNS} FROM workspaces WHERE id = $1"),
                &[&owned],
            )
        })?;
        row.map(|row| row_to_workspace(&row)).transpose()
    }

    /// Revision probe for the `/events` long poll.
    ///
    /// The poll wakes 26 times over 5.2 s. Re-reading the full row each time
    /// would stream the whole document (up to 2 MiB) out of the database on
    /// every tick for every subscriber; this reads one `bigint` instead and the
    /// caller only fetches the document once something actually changed.
    pub fn revision_of(&self, id: &str) -> Result<Option<u64>, String> {
        if !valid_workspace_id(id) {
            return Ok(None);
        }
        let owned = id.to_string();
        let row = self.pool.with(|client| {
            client.query_opt("SELECT revision FROM workspaces WHERE id = $1", &[&owned])
        })?;
        Ok(row.map(|row| row.get::<_, i64>(0).max(0) as u64))
    }

    pub fn update(&self, id: &str, update: WorkspaceUpdate) -> Result<EditResult, UpdateError> {
        let expected = update
            .expected_revision
            .or(update.base_revision)
            .ok_or_else(|| {
                UpdateError::Invalid(
                    "baseRevision or expectedRevision is required for every update.".into(),
                )
            })?;
        let mark = update_mark(&update)?;
        let current = self
            .get(id)
            .map_err(UpdateError::Internal)?
            .ok_or(UpdateError::NotFound)?;
        if expected != current.revision {
            return Err(UpdateError::Conflict(current));
        }
        let (code, hunks) = resolve_edit(&current.code, &update)?;
        if code.len() > MAX_BODY {
            return Err(UpdateError::Invalid(
                "Design code exceeds the 2 MiB limit.".into(),
            ));
        }
        validate_source(&code).map_err(UpdateError::Validation)?;

        let now = unix_millis();
        let events = edit_events(id, current.revision + 1, &update.source, &hunks, now);
        // Fields the caller did not mention are passed as NULL and resolved by
        // `COALESCE` against the *current* column value, not against the copy
        // this thread read a moment ago. That matters because settings writes
        // are revisionless: a build plate picked in another tab between the
        // read above and the statement below would otherwise be overwritten by
        // a plain code save, which is exactly the collision `update_settings`
        // was made revisionless to avoid.
        let name = update.name.clone().map(|name| sanitize_name(Some(name)));
        let plate = update.plate.as_deref().map(sanitize_plate);
        let tolerance = update.tolerance.map(sanitize_tolerance);
        let appended = serde_json::to_value(&events)
            .map_err(|error| UpdateError::Internal(error.to_string()))?;

        // The compare-and-swap. `revision = revision + 1` is computed by the
        // server under the row lock, and the `AND revision = $9` predicate is
        // what makes a concurrent writer on the same base revision lose. There
        // is no read-modify-write window for a second instance to slip into.
        let owned = id.to_string();
        let now_i64 = now as i64;
        // Room for this save's events: the oldest ones go first.
        let drop_count = events.len().min(MAX_EVENTS) as i32;
        let cap = (MAX_EVENTS - drop_count as usize) as i32;
        let expected_i64 = expected as i64;
        let previous_code = current.code.clone();
        let previous_written = revision_written_at(&current) as i64;
        let previous_name = current.revision_name.clone();
        let previous_source = superseded_source(&current);
        // One burst of browser typing keeps one entry, unless a comment names it.
        let retain_previous = !coalesces_with_current(&current, &mark, now)
            || crate::feedback::pins_revision(self.feedback_state(id), current.revision);
        let keep_rows = MAX_REVISION_HISTORY as i64;
        let byte_budget = MAX_REVISION_HISTORY_BYTES as i64;
        let row = self
            .pool
            .with(|client| {
                // The CAS and the history of the revision it supersedes commit
                // together: a save never lands without its predecessor being
                // diffable, and a losing writer records nothing.
                let mut transaction = client.transaction()?;
                let row = transaction.query_opt(
                    &format!(
                        "UPDATE workspaces SET
                             code = $1,
                             name = COALESCE($2, name),
                             plate = COALESCE($3, plate),
                             tolerance = COALESCE($4, tolerance),
                             updated_at = $5,
                             revision = revision + 1,
                             revision_name = $10,
                             revision_source = $11,
                             events = (CASE WHEN jsonb_array_length(events) > $6
                                            THEN (SELECT COALESCE(jsonb_agg(e ORDER BY i), '[]'::jsonb)
                                                  FROM jsonb_array_elements(events) WITH ORDINALITY AS t(e, i)
                                                  WHERE i > jsonb_array_length(events) - $6)
                                            ELSE events END) || $7
                         WHERE id = $8 AND revision = $9
                         RETURNING {SELECT_COLUMNS}"
                    ),
                    &[
                        &code,
                        &name,
                        &plate,
                        &tolerance,
                        &now_i64,
                        &cap,
                        &appended,
                        &owned,
                        &expected_i64,
                        &mark.name,
                        &mark.source,
                    ],
                )?;
                if row.is_none() {
                    transaction.rollback()?;
                    return Ok(None);
                }
                // `previous_code` is exactly revision `expected`: the CAS just
                // proved nothing moved it.
                if retain_previous {
                    record_history(
                        &mut transaction,
                        &owned,
                        expected_i64,
                        &previous_code,
                        previous_written,
                        &previous_name,
                        &previous_source,
                        keep_rows,
                        byte_budget,
                    )?;
                }
                transaction.commit()?;
                Ok(row)
            })
            .map_err(UpdateError::Internal)?;
        match row {
            Some(row) => Ok(EditResult {
                workspace: row_to_workspace(&row).map_err(UpdateError::Internal)?,
                events,
            }),
            // Zero rows means either the workspace vanished or somebody else
            // moved the revision between our read and our write.
            None => match self.get(id).map_err(UpdateError::Internal)? {
                Some(latest) => Err(UpdateError::Conflict(latest)),
                None => Err(UpdateError::NotFound),
            },
        }
    }

    /// Viewer settings are revisionless, so this is an unconditional write and
    /// cannot collide with a concurrent code save. `COALESCE` keeps "absent
    /// means leave as is" a property of the statement rather than of a
    /// read-modify-write in the process.
    pub fn update_settings(
        &self,
        id: &str,
        plate: Option<&str>,
        tolerance: Option<f64>,
    ) -> Result<Workspace, UpdateError> {
        if !valid_workspace_id(id) {
            return Err(UpdateError::NotFound);
        }
        let plate = plate.map(sanitize_plate);
        let tolerance = tolerance.map(sanitize_tolerance);
        let owned = id.to_string();
        let now = unix_millis() as i64;
        let row = self
            .pool
            .with(|client| {
                client.query_opt(
                    &format!(
                        "UPDATE workspaces SET
                             plate = COALESCE($1, plate),
                             tolerance = COALESCE($2, tolerance),
                             updated_at = $3
                         WHERE id = $4
                         RETURNING {SELECT_COLUMNS}"
                    ),
                    &[&plate, &tolerance, &now, &owned],
                )
            })
            .map_err(UpdateError::Internal)?;
        row.map(|row| row_to_workspace(&row))
            .transpose()
            .map_err(UpdateError::Internal)?
            .ok_or(UpdateError::NotFound)
    }

    /// Replaces the stored thumbnail. No revision, and no touch of
    /// `updated_at` — see the note on the filesystem store's `set_preview`.
    pub fn set_preview(&self, id: &str, preview: &str) -> Result<(), UpdateError> {
        if !valid_workspace_id(id) {
            return Err(UpdateError::NotFound);
        }
        let (owned, preview) = (id.to_string(), preview.to_string());
        let updated = self
            .pool
            .with(|client| {
                client.execute(
                    "UPDATE workspaces SET preview = $1 WHERE id = $2",
                    &[&preview, &owned],
                )
            })
            .map_err(UpdateError::Internal)?;
        if updated == 0 {
            return Err(UpdateError::NotFound);
        }
        Ok(())
    }

    /// The token for `id`, minting one the first time it is asked for.
    ///
    /// The `WHERE share_token = ''` makes minting a compare-and-swap under the
    /// row lock: two tabs sharing at the same moment cannot produce two links,
    /// because the loser updates zero rows and re-reads the winner's.
    pub fn share_token(&self, id: &str) -> Result<String, UpdateError> {
        if !valid_workspace_id(id) {
            return Err(UpdateError::NotFound);
        }
        let owned = id.to_string();
        let token = crate::generate_share_token(&self.nonce);
        let existing = self
            .pool
            .with(|client| {
                client.query_opt(
                    "UPDATE workspaces SET share_token = $1
                     WHERE id = $2 AND share_token = ''
                     RETURNING share_token",
                    &[&token, &owned],
                )
            })
            .map_err(UpdateError::Internal)?;
        if let Some(row) = existing {
            return row.try_get::<_, String>("share_token").map_err(|error| {
                UpdateError::Internal(format!("share token column is unreadable: {error}"))
            });
        }
        // Either the workspace is gone or it was already shared.
        let row = self
            .pool
            .with(|client| {
                client.query_opt(
                    "SELECT share_token FROM workspaces WHERE id = $1",
                    &[&owned],
                )
            })
            .map_err(UpdateError::Internal)?
            .ok_or(UpdateError::NotFound)?;
        row.try_get::<_, String>("share_token").map_err(|error| {
            UpdateError::Internal(format!("share token column is unreadable: {error}"))
        })
    }

    /// The workspace a read-only token points at.
    pub fn resolve_share(&self, token: &str) -> Result<Option<Workspace>, String> {
        if !crate::valid_share_token(token) {
            return Ok(None);
        }
        let owned = token.to_string();
        let row = self.pool.with(|client| {
            client.query_opt(
                &format!("SELECT {SELECT_COLUMNS} FROM workspaces WHERE share_token = $1"),
                &[&owned],
            )
        })?;
        row.map(|row| row_to_workspace(&row)).transpose()
    }

    pub fn revision_code(&self, id: &str, revision: u64) -> Result<Option<String>, String> {
        if !valid_workspace_id(id) {
            return Ok(None);
        }
        let owned = id.to_string();
        let revision = revision as i64;
        let row = self.pool.with(|client| {
            client.query_opt(
                "SELECT code FROM workspaces WHERE id = $1 AND revision = $2
                 UNION ALL
                 SELECT code FROM workspace_revisions WHERE workspace_id = $1 AND revision = $2
                 LIMIT 1",
                &[&owned, &revision],
            )
        })?;
        row.map(|row| row.try_get::<_, String>(0).map_err(|error| error.to_string()))
            .transpose()
    }

    /// Design `id`'s comment document; empty when it has none.
    pub fn feedback_state(&self, id: &str) -> Result<FeedbackState, String> {
        let owned = id.to_string();
        let row = self.pool.with(|client| {
            client.query_opt("SELECT state FROM workspace_feedback WHERE workspace_id = $1", &[&owned])
        })?;
        Ok(row
            .and_then(|row| serde_json::from_value(row.get::<_, Value>(0)).ok())
            .unwrap_or_default())
    }

    pub fn feedback_summary(&self, id: &str) -> Result<(u64, usize), String> {
        let owned = id.to_string();
        let row = self.pool.with(|client| {
            client.query_opt(
                "SELECT version, open_count FROM workspace_feedback WHERE workspace_id = $1",
                &[&owned],
            )
        })?;
        Ok(row.map_or((0, 0), |row| {
            (row.get::<_, i64>(0).max(0) as u64, row.get::<_, i32>(1).max(0) as usize)
        }))
    }

    /// Read-modify-write of the comment document under its row lock, so two
    /// instances writing comments to one design cannot lose either. The row
    /// is created on first use; a design that no longer exists fails the
    /// foreign key and reports "Design not found".
    pub fn feedback_mutate<T>(
        &self,
        id: &str,
        mut change: impl FnMut(&mut FeedbackState) -> Result<T, String>,
    ) -> Result<T, String> {
        let owned = id.to_string();
        let outcome = self.pool.with(|client| {
            let mut transaction = client.transaction()?;
            let inserted = transaction.execute(
                "INSERT INTO workspace_feedback (workspace_id)
                     SELECT id FROM workspaces WHERE id = $1
                 ON CONFLICT (workspace_id) DO NOTHING",
                &[&owned],
            );
            inserted?;
            let Some(row) = transaction.query_opt(
                "SELECT state FROM workspace_feedback WHERE workspace_id = $1 FOR UPDATE",
                &[&owned],
            )?
            else {
                return Ok(Err("Design not found.".to_string()));
            };
            let current: FeedbackState =
                serde_json::from_value(row.get::<_, Value>(0)).unwrap_or_default();
            let mut next = current.clone();
            let result = change(&mut next);
            if result.is_ok() && next != current {
                let document = serde_json::to_value(&next).unwrap_or(Value::Null);
                transaction.execute(
                    "UPDATE workspace_feedback SET state = $2, version = $3, open_count = $4
                     WHERE workspace_id = $1",
                    &[
                        &owned,
                        &document,
                        &(next.version as i64),
                        &(next.open_count() as i32),
                    ],
                )?;
            }
            transaction.commit()?;
            Ok(result)
        })?;
        outcome
    }

    /// Persists a browser sighting for other instances. Best effort; the
    /// caller throttles it.
    pub fn note_viewer(&self, id: &str, now: u64) {
        let owned = id.to_string();
        let result = self.pool.with(|client| {
            client.execute(
                "INSERT INTO workspace_feedback (workspace_id, viewer_seen_at)
                     SELECT id, $2 FROM workspaces WHERE id = $1
                 ON CONFLICT (workspace_id) DO UPDATE SET viewer_seen_at = EXCLUDED.viewer_seen_at",
                &[&owned, &(now as i64)],
            )
        });
        if let Err(error) = result {
            eprintln!("viewer presence write failed: {error}");
        }
    }

    pub fn viewer_seen_at(&self, id: &str) -> u64 {
        let owned = id.to_string();
        self.pool
            .with(|client| {
                client.query_opt(
                    "SELECT viewer_seen_at FROM workspace_feedback WHERE workspace_id = $1",
                    &[&owned],
                )
            })
            .ok()
            .flatten()
            .map_or(0, |row| row.get::<_, i64>(0).max(0) as u64)
    }

    /// Design `id`'s assets (metadata only), by name.
    #[cfg(test)]
    pub fn asset_list(&self, id: &str) -> Result<Vec<AssetMeta>, String> {
        Ok(self.asset_snapshot(id)?.0)
    }

    /// Design `id`'s assets (metadata only) and its assets version, read by
    /// one statement so the two always describe the same state.
    pub fn asset_snapshot(&self, id: &str) -> Result<(Vec<AssetMeta>, u64), String> {
        if !valid_workspace_id(id) {
            return Ok((Vec::new(), 0));
        }
        let owned = id.to_string();
        let rows = self.pool.with(|client| {
            client.query(
                "SELECT a.name, a.sha256, a.bytes, a.triangles, a.created_at, a.report, a.expires_at, w.assets_version
                 FROM workspaces w LEFT JOIN workspace_assets a ON a.workspace_id = w.id
                 WHERE w.id = $1 ORDER BY a.name",
                &[&owned],
            )
        })?;
        let version = rows.first().map_or(0, |row| row.get::<_, i64>(7).max(0) as u64);
        let now = unix_millis();
        let metas = rows
            .iter()
            .filter(|row| row.get::<_, Option<String>>(0).is_some())
            .map(row_to_asset_meta)
            .filter(|meta: &AssetMeta| !meta.expired(now))
            .collect();
        Ok((metas, version))
    }

    /// The live and expired assets of design `id`: the cheap index a compile
    /// reads before it fetches any mesh. An expired row the sweep has not
    /// reached yet is already expired here.
    pub fn asset_index(&self, id: &str, now: u64) -> Result<AssetIndexView, String> {
        if !valid_workspace_id(id) {
            return Ok((Vec::new(), Vec::new()));
        }
        let owned = id.to_string();
        let rows = self.pool.with(|client| {
            client.query(
                "SELECT name, sha256, expires_at, created_at, report->>'origin' IS NOT NULL FROM workspace_assets WHERE workspace_id = $1
                 UNION ALL
                 SELECT name, '', expired_at, 0, FALSE FROM workspace_assets_expired WHERE workspace_id = $1
                 ORDER BY 1",
                &[&owned],
            )
        })?;
        let (mut live, mut expired) = (Vec::new(), Vec::new());
        for row in &rows {
            let (name, sha): (String, String) = (row.get(0), row.get(1));
            let (expires_at, created_at) = (row.get::<_, i64>(2).max(0) as u64, row.get::<_, i64>(3).max(0) as u64);
            // Legacy server-made rows expired when stored: see AssetMeta::expiry.
            let expiry = if row.get::<_, bool>(4) {
                created_at
            } else if expires_at == 0 {
                created_at.saturating_add(ASSET_TTL_MS)
            } else {
                expires_at
            };
            if sha.is_empty() {
                expired.push((name, expiry));
            } else if now >= expiry {
                expired.retain(|(other, _)| other != &name);
                expired.push((name, expiry));
            } else {
                live.push((name, sha));
            }
        }
        // A name uploaded again is live, whatever became of it before.
        expired.retain(|(name, _)| !live.iter().any(|(other, _)| other == name));
        Ok((live, expired))
    }

    /// Deletes every expired asset and records its name. Each design is
    /// settled in its own transaction under its row lock, the same lock order
    /// as a put; a design another writer holds is left to the next pass, so
    /// any number of instances may sweep at once. Returns what went, by design.
    pub fn sweep_expired_assets(&self, now: u64) -> Result<Vec<(String, Vec<String>)>, String> {
        let now_ms = now as i64;
        let designs = self.pool.with(|client| {
            client.query(
                "SELECT DISTINCT workspace_id FROM workspace_assets
                 WHERE (expires_at <> 0 AND expires_at <= $1) OR (expires_at = 0 AND created_at + $2 <= $1) OR report->>'origin' IS NOT NULL
                 LIMIT 1000",
                &[&now_ms, &(ASSET_TTL_MS as i64)],
            )
        })?;
        let mut swept = Vec::new();
        for row in &designs {
            let id: String = row.get(0);
            let names = self.pool.with(|client| {
                let mut transaction = client.transaction()?;
                if transaction.query_opt("SELECT 1 FROM workspaces WHERE id = $1 FOR UPDATE SKIP LOCKED", &[&id])?.is_none() {
                    return Ok(Vec::new());
                }
                let names = purge_expired(&mut transaction, &id, now_ms)?;
                if !names.is_empty() {
                    // Not updated_at: expiring is no use of the design.
                    transaction.execute("UPDATE workspaces SET assets_version = assets_version + 1 WHERE id = $1", &[&id])?;
                }
                transaction.commit()?;
                Ok(names)
            })?;
            if !names.is_empty() {
                swept.push((id, names));
            }
        }
        Ok(swept)
    }

    /// The stored (cleaned binary STL) bytes of asset `name`, only while its
    /// content hash is still `sha256`: a replaced asset answers `None`, never
    /// the new bytes under the old digest.
    pub fn asset_blob(&self, id: &str, name: &str, sha256: &str) -> Result<Option<Vec<u8>>, String> {
        if !valid_workspace_id(id) {
            return Ok(None);
        }
        let (owned, name, sha256) = (id.to_string(), name.to_string(), sha256.to_string());
        let row = self.pool.with(|client| {
            client.query_opt(
                "SELECT mesh FROM workspace_assets WHERE workspace_id = $1 AND name = $2 AND sha256 = $3",
                &[&owned, &name, &sha256],
            )
        })?;
        Ok(row.map(|row| row.get::<_, Vec<u8>>(0)))
    }

    pub fn assets_version(&self, id: &str) -> Result<u64, String> {
        Ok(self.poll_state(id)?.map_or(0, |(_, version)| version))
    }

    /// `(revision, assets_version)` in one round trip, for `/events`.
    pub fn poll_state(&self, id: &str) -> Result<Option<(u64, u64)>, String> {
        if !valid_workspace_id(id) {
            return Ok(None);
        }
        let owned = id.to_string();
        let row = self.pool.with(|client| {
            client.query_opt(
                "SELECT revision, assets_version FROM workspaces WHERE id = $1",
                &[&owned],
            )
        })?;
        Ok(row.map(|row| {
            (row.get::<_, i64>(0).max(0) as u64, row.get::<_, i64>(1).max(0) as u64)
        }))
    }

    /// Stores (or replaces) one asset under the per-design and deployment
    /// limits, bumping the design's assets version and touching it for
    /// eviction, all in one transaction. When the deployment is over its
    /// byte ceiling, the assets of the least recently used other designs are
    /// evicted to make room (skipping any design another transaction holds).
    /// Returns the new version and the designs whose assets were evicted.
    pub fn asset_put(&self, id: &str, processed: &Processed, max_total: u64) -> Result<(u64, Vec<String>), AssetError> {
        if !valid_workspace_id(id) {
            return Err(AssetError::new(404, "Design not found."));
        }
        let owned = id.to_string();
        let now = unix_millis() as i64;
        let outcome = self
            .pool
            .with(|client| {
                let mut transaction = client.transaction()?;
                // Lock order for every asset write: the design's row first,
                // then (puts only) the total lock. It serialises the design's
                // asset writes across instances and fails cleanly if it was
                // evicted.
                if transaction
                    .query_opt("SELECT 1 FROM workspaces WHERE id = $1 FOR UPDATE", &[&owned])?
                    .is_none()
                {
                    return Ok(Err(AssetError::new(404, "Design not found.")));
                }
                // Rolled back with a refused write, like everything here.
                purge_expired(&mut transaction, &owned, now)?;
                let existing: Vec<AssetMeta> = transaction
                    .query(
                        "SELECT name, sha256, bytes, triangles, created_at, report, expires_at
                         FROM workspace_assets WHERE workspace_id = $1",
                        &[&owned],
                    )?
                    .iter()
                    .map(row_to_asset_meta)
                    .collect();
                transaction.execute("SELECT pg_advisory_xact_lock($1, 0)", &[&ASSET_TOTAL_LOCK])?;
                let total: i64 = transaction
                    .query_one("SELECT COALESCE(SUM(bytes), 0)::BIGINT FROM workspace_assets", &[])?
                    .get(0);
                let meta = match settle_put(&existing, processed, now as u64) {
                    Ok(meta) => meta,
                    Err(error) => return Ok(Err(error)),
                };
                let mut needed = match check_limits(&existing, &meta, total.max(0) as u64, max_total) {
                    Ok(needed) => needed,
                    Err(error) => return Ok(Err(error)),
                };
                let mut evicted = Vec::new();
                if needed > 0 {
                    let candidates = transaction.query(
                        "SELECT a.workspace_id, SUM(a.bytes)::BIGINT
                         FROM workspace_assets a JOIN workspaces w ON w.id = a.workspace_id
                         WHERE a.workspace_id <> $1
                         GROUP BY a.workspace_id, w.updated_at
                         ORDER BY w.updated_at ASC, a.workspace_id ASC",
                        &[&owned],
                    )?;
                    for row in &candidates {
                        if needed == 0 {
                            break;
                        }
                        let victim: String = row.get(0);
                        // Never wait on another design's lock while holding
                        // ours and the total lock: skip it instead.
                        if transaction
                            .query_opt(
                                "SELECT 1 FROM workspaces WHERE id = $1 FOR UPDATE SKIP LOCKED",
                                &[&victim],
                            )?
                            .is_none()
                        {
                            continue;
                        }
                        let freed: i64 = transaction
                            .query_one(
                                "WITH gone AS (DELETE FROM workspace_assets WHERE workspace_id = $1 RETURNING bytes)
                                 SELECT COALESCE(SUM(bytes), 0)::BIGINT FROM gone",
                                &[&victim],
                            )?
                            .get(0);
                        transaction.execute(
                            "UPDATE workspaces SET assets_version = assets_version + 1 WHERE id = $1",
                            &[&victim],
                        )?;
                        needed = needed.saturating_sub(freed.max(0) as u64);
                        evicted.push(victim);
                    }
                    if needed > 0 {
                        return Ok(Err(storage_full()));
                    }
                }
                transaction.execute(
                    "INSERT INTO workspace_assets
                         (workspace_id, name, sha256, mesh, report, bytes, triangles, created_at, expires_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                     ON CONFLICT (workspace_id, name) DO UPDATE SET
                         sha256 = EXCLUDED.sha256, mesh = EXCLUDED.mesh, report = EXCLUDED.report,
                         bytes = EXCLUDED.bytes, triangles = EXCLUDED.triangles,
                         created_at = EXCLUDED.created_at, expires_at = EXCLUDED.expires_at",
                    &[
                        &owned,
                        &meta.name,
                        &meta.sha256,
                        &processed.stl,
                        &meta.report,
                        &(meta.bytes as i64),
                        &(meta.triangles as i64),
                        &(meta.created_at as i64),
                        &(meta.expires_at as i64),
                    ],
                )?;
                transaction.execute(
                    "DELETE FROM workspace_assets_expired WHERE workspace_id = $1 AND name = $2",
                    &[&owned, &meta.name],
                )?;
                let version: i64 = transaction
                    .query_one(
                        "UPDATE workspaces SET assets_version = assets_version + 1, updated_at = $2
                         WHERE id = $1 RETURNING assets_version",
                        &[&owned, &now],
                    )?
                    .get(0);
                transaction.commit()?;
                Ok(Ok((version.max(0) as u64, evicted)))
            })
            .map_err(AssetError::internal)?;
        outcome
    }

    /// Deletes live asset `name`; `None` when the design has no such asset.
    pub fn asset_delete(&self, id: &str, name: &str) -> Result<Option<u64>, String> {
        if !valid_workspace_id(id) {
            return Ok(None);
        }
        let (owned, name) = (id.to_string(), name.to_string());
        let now = unix_millis() as i64;
        self.pool.with(|client| {
            let mut transaction = client.transaction()?;
            // Same lock order as `asset_put`: the design's row before any
            // asset row, so a concurrent put and delete cannot deadlock.
            if transaction
                .query_opt("SELECT 1 FROM workspaces WHERE id = $1 FOR UPDATE", &[&owned])?
                .is_none()
            {
                return Ok(None);
            }
            let target = transaction.query_opt(
                "SELECT report->>'origin' IS NOT NULL, expires_at, created_at FROM workspace_assets WHERE workspace_id = $1 AND name = $2",
                &[&owned, &name],
            )?;
            let Some(target) = target else {
                return Ok(None);
            };
            let (legacy, expires_at, created_at): (bool, i64, i64) = (target.get(0), target.get(1), target.get(2));
            let expiry = if expires_at == 0 { created_at.saturating_add(ASSET_TTL_MS as i64) } else { expires_at };
            if legacy || now >= expiry {
                return Ok(None);
            }
            transaction.execute("DELETE FROM workspace_assets WHERE workspace_id = $1 AND name = $2", &[&owned, &name])?;
            let version: i64 = transaction
                .query_one(
                    "UPDATE workspaces SET assets_version = assets_version + 1, updated_at = $2
                     WHERE id = $1 RETURNING assets_version",
                    &[&owned, &now],
                )?
                .get(0);
            transaction.commit()?;
            Ok(Some(version.max(0) as u64))
        })
    }

    /// Records a one-time upload URL so whichever instance receives the PUT
    /// can take it. Capped across the deployment, per design and per client.
    pub fn create_upload_token(&self, token: &str, pending: &PendingUpload) -> Result<(), String> {
        let now = unix_millis() as i64;
        let token = token.to_string();
        let refused = self.pool.with(|client| {
            let mut transaction = client.transaction()?;
            // Serialise the count-then-insert across instances.
            transaction.execute("SELECT pg_advisory_xact_lock($1, 1)", &[&ASSET_TOTAL_LOCK])?;
            transaction.execute("DELETE FROM asset_upload_tokens WHERE expires_at < $1", &[&now])?;
            let counts = transaction.query_one(
                "SELECT COUNT(*),
                        COUNT(*) FILTER (WHERE workspace_id = $1),
                        COUNT(*) FILTER (WHERE client <> '' AND client = $2)
                 FROM asset_upload_tokens",
                &[&pending.design_id, &pending.client],
            )?;
            let (all, design, client_count): (i64, i64, i64) = (counts.get(0), counts.get(1), counts.get(2));
            if let Some(refusal) = pending_refusal(all as usize, design as usize, client_count as usize, MAX_PENDING_UPLOAD_TOKENS as usize) {
                return Ok(Some(refusal));
            }
            transaction.execute(
                "INSERT INTO asset_upload_tokens (token, workspace_id, name, budget, expires_at, client)
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &token,
                    &pending.design_id,
                    &pending.name,
                    &(pending.budget as i64),
                    &(pending.expires_at as i64),
                    &pending.client,
                ],
            )?;
            transaction.commit()?;
            Ok(None)
        })?;
        match refused {
            Some(refusal) => Err(refusal),
            None => Ok(()),
        }
    }

    /// A pending one-time upload URL, without consuming it.
    pub fn peek_upload_token(&self, token: &str) -> Result<Option<PendingUpload>, String> {
        let token = token.to_string();
        let now = unix_millis() as i64;
        let row = self.pool.with(|client| {
            client.query_opt(
                "SELECT workspace_id, name, budget, expires_at, client FROM asset_upload_tokens
                 WHERE token = $1 AND expires_at > $2",
                &[&token, &now],
            )
        })?;
        Ok(row.as_ref().map(row_to_pending))
    }

    /// Takes (consumes) a one-time upload URL, on any instance; `None` when
    /// unknown, used or expired.
    pub fn take_upload_token(&self, token: &str) -> Result<Option<PendingUpload>, String> {
        let token = token.to_string();
        let row = self.pool.with(|client| {
            client.query_opt(
                "DELETE FROM asset_upload_tokens WHERE token = $1
                 RETURNING workspace_id, name, budget, expires_at, client",
                &[&token],
            )
        })?;
        let now = unix_millis();
        Ok(row.as_ref().map(row_to_pending).filter(|pending| pending.expires_at > now))
    }

    /// Puts back a token taken for an upload that then failed on the
    /// server's side (queue full, storage error), so it can be retried.
    pub fn restore_upload_token(&self, token: &str, pending: &PendingUpload) -> Result<(), String> {
        let token = token.to_string();
        self.pool.with(|client| {
            client.execute(
                "INSERT INTO asset_upload_tokens (token, workspace_id, name, budget, expires_at, client)
                 SELECT $1, $2, $3, $4, $5, $6 WHERE EXISTS (SELECT 1 FROM workspaces WHERE id = $2)
                 ON CONFLICT (token) DO NOTHING",
                &[
                    &token,
                    &pending.design_id,
                    &pending.name,
                    &(pending.budget as i64),
                    &(pending.expires_at as i64),
                    &pending.client,
                ],
            )
        })?;
        Ok(())
    }

    /// Keeps a one-time upload URL valid until at least `expires_at`.
    pub fn extend_upload_token(&self, token: &str, expires_at: u64) -> Result<(), String> {
        let token = token.to_string();
        self.pool.with(|client| {
            client.execute(
                "UPDATE asset_upload_tokens SET expires_at = GREATEST(expires_at, $2) WHERE token = $1",
                &[&token, &(expires_at as i64)],
            )
        })?;
        Ok(())
    }

    /// A live chunked upload.
    pub fn upload_session(&self, upload_id: &str, now: u64) -> Result<Option<UploadSession>, String> {
        let upload_id = upload_id.to_string();
        let cutoff = now.saturating_sub(UPLOAD_SESSION_IDLE_MS) as i64;
        let row = self.pool.with(|client| {
            client.query_opt(
                "SELECT upload_id, target, workspace_id, name, budget, total, received, client, updated_at
                 FROM asset_upload_sessions WHERE upload_id = $1 AND updated_at > $2",
                &[&upload_id, &cutoff],
            )
        })?;
        Ok(row.as_ref().map(row_to_session))
    }

    /// Opens a chunked upload, any instance may take its chunks. Capped per
    /// client, per design and across the deployment; the same client
    /// restarting the same target replaces its old session.
    pub fn open_upload_session(&self, session: &UploadSession, now: u64) -> Result<(), AssetError> {
        let cutoff = now.saturating_sub(UPLOAD_SESSION_IDLE_MS) as i64;
        let outcome = self
            .pool
            .with(|client| {
                let mut transaction = client.transaction()?;
                transaction.execute("SELECT pg_advisory_xact_lock($1, 2)", &[&ASSET_TOTAL_LOCK])?;
                transaction.execute("DELETE FROM asset_upload_sessions WHERE updated_at <= $1", &[&cutoff])?;
                // Not one whose file another request is processing.
                transaction.execute(
                    "DELETE FROM asset_upload_sessions WHERE target = $1 AND name = $2 AND client = $3
                         AND finishing_at <= $4",
                    &[
                        &session.target,
                        &session.name,
                        &session.client,
                        &(now.saturating_sub(UPLOAD_FINISH_CLAIM_MS) as i64),
                    ],
                )?;
                let live: Vec<UploadSession> = transaction
                    .query(
                        "SELECT upload_id, target, workspace_id, name, budget, total, received, client, updated_at
                         FROM asset_upload_sessions",
                        &[],
                    )?
                    .iter()
                    .map(row_to_session)
                    .collect();
                if let Some(refusal) = session_refusal(&live, session, MAX_STAGED_BYTES) {
                    return Ok(Err(refusal));
                }
                let inserted = transaction.execute(
                    "INSERT INTO asset_upload_sessions
                         (upload_id, target, workspace_id, name, budget, total, received, client, created_at, updated_at)
                     SELECT $1, $2, $3, $4, $5, $6, 0, $7, $8, $8
                     WHERE EXISTS (SELECT 1 FROM workspaces WHERE id = $3)",
                    &[
                        &session.upload_id,
                        &session.target,
                        &session.design_id,
                        &session.name,
                        &(session.budget as i64),
                        &(session.total as i64),
                        &session.client,
                        &(now as i64),
                    ],
                )?;
                if inserted == 0 {
                    return Ok(Err(AssetError::new(404, "Design not found.")));
                }
                transaction.commit()?;
                Ok(Ok(()))
            })
            .map_err(AssetError::internal)?;
        outcome
    }

    /// Stages one chunk at `start` under the session's row lock, so chunks
    /// sent to different instances stay in order. Returns the bytes received.
    pub fn append_upload_chunk(&self, upload_id: &str, start: u64, bytes: &[u8], now: u64) -> Result<u64, AssetError> {
        let upload_id = upload_id.to_string();
        let cutoff = now.saturating_sub(UPLOAD_SESSION_IDLE_MS) as i64;
        let outcome = self
            .pool
            .with(|client| {
                let mut transaction = client.transaction()?;
                let Some(row) = transaction.query_opt(
                    "SELECT received FROM asset_upload_sessions WHERE upload_id = $1 AND updated_at > $2 FOR UPDATE",
                    &[&upload_id, &cutoff],
                )?
                else {
                    return Ok(Err(AssetError::new(404, UNKNOWN_UPLOAD_SESSION)));
                };
                let received = row.get::<_, i64>(0).max(0) as u64;
                let write = match chunk_fits(received, start, bytes.len() as u64) {
                    Ok(write) => write,
                    Err(error) => return Ok(Err(error)),
                };
                let received = if write {
                    transaction.execute(
                        "DELETE FROM asset_upload_chunks WHERE upload_id = $1 AND byte_offset >= $2",
                        &[&upload_id, &(start as i64)],
                    )?;
                    transaction.execute(
                        "INSERT INTO asset_upload_chunks (upload_id, byte_offset, bytes, created_at) VALUES ($1, $2, $3, $4)",
                        &[&upload_id, &(start as i64), &bytes, &(now as i64)],
                    )?;
                    start + bytes.len() as u64
                } else {
                    received
                };
                transaction.execute(
                    "UPDATE asset_upload_sessions SET received = $2, updated_at = $3 WHERE upload_id = $1",
                    &[&upload_id, &(received as i64), &(now as i64)],
                )?;
                transaction.commit()?;
                Ok(Ok(received))
            })
            .map_err(AssetError::internal)?;
        outcome
    }

    /// The staged file, assembled chunk by chunk (one row in memory at a time
    /// besides the result).
    pub fn upload_session_bytes(&self, upload_id: &str) -> Result<Vec<u8>, String> {
        let upload_id = upload_id.to_string();
        let (total, offsets) = self.pool.with(|client| {
            let total = client
                .query_opt("SELECT total FROM asset_upload_sessions WHERE upload_id = $1", &[&upload_id])?
                .map(|row| row.get::<_, i64>(0).max(0) as u64);
            let offsets: Vec<i64> = client
                .query("SELECT byte_offset FROM asset_upload_chunks WHERE upload_id = $1 ORDER BY byte_offset", &[&upload_id])?
                .iter()
                .map(|row| row.get(0))
                .collect();
            Ok((total, offsets))
        })?;
        let total = total.ok_or(UNKNOWN_UPLOAD_SESSION)?;
        let mut out = Vec::with_capacity(total.min(crate::meshimport::MAX_UPLOAD_BYTES as u64) as usize);
        for offset in offsets {
            if offset as u64 != out.len() as u64 {
                return Err("The staged upload has a gap; start it again.".into());
            }
            let part: Vec<u8> = self.pool.with(|client| {
                client
                    .query_one(
                        "SELECT bytes FROM asset_upload_chunks WHERE upload_id = $1 AND byte_offset = $2",
                        &[&upload_id, &offset],
                    )
                    .map(|row| row.get(0))
            })?;
            out.extend_from_slice(&part);
        }
        if out.len() as u64 != total {
            return Err(format!("The staged upload holds {} of {total} bytes; send the missing chunks.", out.len()));
        }
        Ok(out)
    }

    /// Claims a complete upload for processing, on any instance: `Ok(false)`
    /// while another request's claim (younger than `UPLOAD_FINISH_CLAIM_MS`)
    /// holds. Taking it refreshes the session, so the sweeper leaves it be.
    pub fn claim_upload_finish(&self, upload_id: &str, now: u64) -> Result<bool, String> {
        let upload_id = upload_id.to_string();
        let stale = now.saturating_sub(UPLOAD_FINISH_CLAIM_MS) as i64;
        let now = now.max(1) as i64;
        self.pool.with(|client| {
            let claimed = client.execute(
                "UPDATE asset_upload_sessions SET finishing_at = $2, updated_at = $2
                 WHERE upload_id = $1 AND finishing_at <= $3",
                &[&upload_id, &now, &stale],
            )?;
            if claimed == 1 {
                return Ok(Ok(true));
            }
            let exists = client.query_opt("SELECT 1 FROM asset_upload_sessions WHERE upload_id = $1", &[&upload_id])?;
            Ok(if exists.is_some() { Ok(false) } else { Err(UNKNOWN_UPLOAD_SESSION.to_string()) })
        })?
    }

    /// Gives a finish claim back after a failure worth retrying.
    pub fn release_upload_finish(&self, upload_id: &str) -> Result<(), String> {
        let upload_id = upload_id.to_string();
        self.pool.with(|client| {
            client.execute("UPDATE asset_upload_sessions SET finishing_at = 0 WHERE upload_id = $1", &[&upload_id])
        })?;
        Ok(())
    }

    /// Ends a finished upload in one transaction: its reply is kept for a
    /// re-sent last chunk and its session and parts go. Expired receipts are
    /// dropped, and the oldest past `MAX_UPLOAD_RECEIPTS`.
    pub fn complete_upload_session(&self, receipt: &UploadReceipt) -> Result<(), String> {
        let receipt = receipt.clone();
        self.pool.with(|client| {
            let mut transaction = client.transaction()?;
            transaction.execute(
                "DELETE FROM asset_upload_receipts WHERE created_at <= $1",
                &[&(receipt.created_at.saturating_sub(UPLOAD_RECEIPT_TTL_MS) as i64)],
            )?;
            transaction.execute(
                "INSERT INTO asset_upload_receipts (upload_id, target, total, final_start, response, created_at)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (upload_id) DO UPDATE SET response = EXCLUDED.response, created_at = EXCLUDED.created_at",
                &[
                    &receipt.upload_id,
                    &receipt.target,
                    &(receipt.total as i64),
                    &(receipt.final_start as i64),
                    &receipt.response,
                    &(receipt.created_at as i64),
                ],
            )?;
            transaction.execute(
                "DELETE FROM asset_upload_receipts WHERE upload_id IN (
                     SELECT upload_id FROM asset_upload_receipts ORDER BY created_at DESC, upload_id OFFSET $1
                 )",
                &[&(MAX_UPLOAD_RECEIPTS as i64)],
            )?;
            transaction.execute("DELETE FROM asset_upload_sessions WHERE upload_id = $1", &[&receipt.upload_id])?;
            transaction.commit()
        })?;
        Ok(())
    }

    /// A finished upload's kept reply.
    pub fn upload_receipt(&self, upload_id: &str, now: u64) -> Result<Option<UploadReceipt>, String> {
        let upload_id = upload_id.to_string();
        let cutoff = now.saturating_sub(UPLOAD_RECEIPT_TTL_MS) as i64;
        let row = self.pool.with(|client| {
            client.query_opt(
                "SELECT upload_id, target, total, final_start, response, created_at
                 FROM asset_upload_receipts WHERE upload_id = $1 AND created_at > $2",
                &[&upload_id, &cutoff],
            )
        })?;
        Ok(row.map(|row| UploadReceipt {
            upload_id: row.get(0),
            target: row.get(1),
            total: row.get::<_, i64>(2).max(0) as u64,
            final_start: row.get::<_, i64>(3).max(0) as u64,
            response: row.get(4),
            created_at: row.get::<_, i64>(5).max(0) as u64,
        }))
    }

    /// Drops a session and its parts.
    pub fn drop_upload_session(&self, upload_id: &str) -> Result<(), String> {
        let upload_id = upload_id.to_string();
        self.pool.with(|client| client.execute("DELETE FROM asset_upload_sessions WHERE upload_id = $1", &[&upload_id]))?;
        Ok(())
    }

    pub fn revision_list(&self, id: &str) -> Result<Vec<RevisionInfo>, String> {
        if !valid_workspace_id(id) {
            return Ok(Vec::new());
        }
        let owned = id.to_string();
        let rows = self.pool.with(|client| {
            client.query(
                "SELECT revision, created_at, octet_length(code)::BIGINT, name, source
                 FROM workspace_revisions WHERE workspace_id = $1 ORDER BY revision",
                &[&owned],
            )
        })?;
        rows.iter()
            .map(|row| {
                let revision: i64 = row.try_get(0).map_err(|error| error.to_string())?;
                let created: i64 = row.try_get(1).map_err(|error| error.to_string())?;
                let bytes: i64 = row.try_get(2).map_err(|error| error.to_string())?;
                Ok(RevisionInfo {
                    revision: revision.max(0) as u64,
                    timestamp: created.max(0) as u64,
                    bytes: bytes.max(0) as usize,
                    name: row.try_get(3).map_err(|error| error.to_string())?,
                    source: row.try_get(4).map_err(|error| error.to_string())?,
                })
            })
            .collect()
    }

    fn generate_id(&self) -> String {
        crate::generate_workspace_id(&self.nonce)
    }
}

/// Inserts the superseded revision `revision` and trims the history to the
/// count and byte ceilings, inside the caller's transaction.
#[allow(clippy::too_many_arguments)]
fn record_history(
    transaction: &mut postgres::Transaction<'_>,
    id: &String,
    revision: i64,
    code: &String,
    timestamp: i64,
    name: &String,
    source: &String,
    keep_rows: i64,
    byte_budget: i64,
) -> Result<(), postgres::Error> {
    transaction.execute(
        "INSERT INTO workspace_revisions (workspace_id, revision, code, created_at, name, source)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (workspace_id, revision) DO NOTHING",
        &[id, &revision, code, &timestamp, name, source],
    )?;
    // Same ceilings as the filesystem store: newest first, drop whatever is
    // past the row count or pushes the running total past the byte ceiling,
    // always keeping the newest snapshot. Counted in rows, not revision
    // numbers: a coalesced browser autosave spends a number but keeps no row.
    transaction.execute(
        "DELETE FROM workspace_revisions AS r
         USING (
             SELECT revision,
                    SUM(octet_length(code)) OVER (ORDER BY revision DESC) AS running,
                    ROW_NUMBER() OVER (ORDER BY revision DESC) AS position
             FROM workspace_revisions WHERE workspace_id = $1
         ) AS t
         WHERE r.workspace_id = $1 AND r.revision = t.revision
           AND t.position > 1 AND (t.running > $2 OR t.position > $3)",
        &[id, &byte_budget, &keep_rows],
    )?;
    Ok(())
}

/// The current creation bucket, in minutes since the epoch.
fn current_minute() -> i64 {
    (unix_millis() / CREATION_BUCKET_MILLIS) as i64
}

/// Maps a row, without panicking on a schema that is not what we expect.
///
/// `Row::get` panics on a type mismatch, which in a thread-per-connection
/// server means the client's socket is closed mid-response and it sees a reset
/// rather than an error. A hand-applied or half-migrated table — `revision`
/// created as `integer`, say — would do exactly that on every read. `try_get`
/// turns it into a 500 with a log line instead.
fn row_to_pending(row: &Row) -> PendingUpload {
    PendingUpload {
        design_id: row.get(0),
        name: row.get(1),
        budget: row.get::<_, i64>(2).max(0) as usize,
        expires_at: row.get::<_, i64>(3).max(0) as u64,
        client: row.get(4),
    }
}

fn row_to_session(row: &Row) -> UploadSession {
    UploadSession {
        upload_id: row.get(0),
        target: row.get(1),
        design_id: row.get(2),
        name: row.get(3),
        budget: row.get::<_, i64>(4).max(0) as usize,
        total: row.get::<_, i64>(5).max(0) as u64,
        received: row.get::<_, i64>(6).max(0) as u64,
        client: row.get(7),
        updated_at: row.get::<_, i64>(8).max(0) as u64,
    }
}

/// Moves design `id`'s expired assets to the record of expired names, inside
/// the caller's transaction, which holds the design's row. Returns their names.
fn purge_expired(transaction: &mut postgres::Transaction<'_>, id: &String, now: i64) -> Result<Vec<String>, postgres::Error> {
    let rows = transaction.query(
        "WITH gone AS (
             DELETE FROM workspace_assets
             WHERE workspace_id = $1
               AND ((expires_at <> 0 AND expires_at <= $2) OR (expires_at = 0 AND created_at + $3 <= $2) OR report->>'origin' IS NOT NULL)
             RETURNING name, CASE WHEN report->>'origin' IS NOT NULL THEN created_at WHEN expires_at = 0 THEN created_at + $3 ELSE expires_at END AS at
         )
         INSERT INTO workspace_assets_expired (workspace_id, name, expired_at)
         SELECT $1, name, at FROM gone
         ON CONFLICT (workspace_id, name) DO UPDATE SET expired_at = EXCLUDED.expired_at
         RETURNING name",
        &[id, &now, &(ASSET_TTL_MS as i64)],
    )?;
    if !rows.is_empty() {
        transaction.execute(
            "DELETE FROM workspace_assets_expired WHERE workspace_id = $1 AND name NOT IN (
                 SELECT name FROM workspace_assets_expired WHERE workspace_id = $1 ORDER BY expired_at DESC, name LIMIT $2
             )",
            &[id, &(MAX_EXPIRED_NAMES as i64)],
        )?;
    }
    Ok(rows.iter().map(|row| row.get(0)).collect())
}

fn row_to_asset_meta(row: &Row) -> AssetMeta {
    AssetMeta {
        name: row.get(0),
        sha256: row.get(1),
        bytes: row.get::<_, i64>(2).max(0) as u64,
        triangles: row.get::<_, i64>(3).max(0) as u64,
        created_at: row.get::<_, i64>(4).max(0) as u64,
        report: row.get::<_, Value>(5),
        expires_at: row.try_get::<_, i64>(6).unwrap_or(0).max(0) as u64,
    }
}

fn row_to_workspace(row: &Row) -> Result<Workspace, String> {
    fn column<'row, T: postgres::types::FromSql<'row>>(
        row: &'row Row,
        name: &str,
    ) -> Result<T, String> {
        row.try_get(name).map_err(|error| {
            eprintln!("workspace row column `{name}` is not the expected type: {error}");
            "The stored workspace could not be read.".to_string()
        })
    }
    let name: String = column(row, "name")?;
    let events: Value = column(row, "events")?;
    Ok(Workspace {
        id: column(row, "id")?,
        name: if name.trim().is_empty() {
            default_workspace_name()
        } else {
            name
        },
        code: column(row, "code")?,
        revision: column::<i64>(row, "revision")?.max(0) as u64,
        created_at: column::<i64>(row, "created_at")?.max(0) as u64,
        updated_at: column::<i64>(row, "updated_at")?.max(0) as u64,
        plate: column(row, "plate")?,
        tolerance: column(row, "tolerance")?,
        preview: column(row, "preview")?,
        share_token: column(row, "share_token")?,
        revision_name: column(row, "revision_name")?,
        revision_source: column(row, "revision_source")?,
        // A row whose event log somehow failed to round-trip must not make the
        // workspace unreadable; the log is a UI convenience, the code is not.
        events: serde_json::from_value(events).unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------
// Migrations
// ---------------------------------------------------------------------------

/// Versioned, idempotent, and safe to run from every instance at once.
///
/// The advisory lock is taken **first**, before any DDL. `CREATE TABLE IF NOT
/// EXISTS` is not actually race-free in Postgres — two sessions running it
/// concurrently can lose the catalogue insert with `23505 duplicate key value
/// violates unique constraint "pg_type_typname_nsp_index"` — and that is a
/// database error, so it is not retried and the instance exits. Which is
/// precisely the scenario this is supposed to survive: a fleet cold-starting
/// together. An advisory lock needs no table of its own, so it can be taken
/// before the bookkeeping table exists.
fn migrate(client: &mut Client) -> Result<(), postgres::Error> {
    let mut transaction = client.transaction()?;
    transaction.execute("SELECT pg_advisory_xact_lock($1)", &[&MIGRATION_LOCK_KEY])?;
    transaction.batch_execute(
        "CREATE TABLE IF NOT EXISTS reopenscad_schema (
             version    INTEGER PRIMARY KEY,
             applied_at BIGINT NOT NULL
         )",
    )?;
    let applied: Vec<i32> = transaction
        .query("SELECT version FROM reopenscad_schema", &[])?
        .iter()
        .map(|row| row.get::<_, i32>(0))
        .collect();
    for (version, statements) in MIGRATIONS {
        if applied.contains(version) {
            continue;
        }
        for statement in *statements {
            transaction.batch_execute(statement)?;
        }
        transaction.execute(
            "INSERT INTO reopenscad_schema (version, applied_at) VALUES ($1, $2)
             ON CONFLICT (version) DO NOTHING",
            &[version, &(unix_millis() as i64)],
        )?;
    }
    transaction.commit()
}

/// The whole schema, as ordered migrations. Adding a column means appending a
/// new `(version, statements)` pair, never editing an existing one.
const MIGRATIONS: &[(i32, &[&str])] = &[(
    1,
    &[
        "CREATE TABLE IF NOT EXISTS workspaces (
             id         TEXT PRIMARY KEY,
             name       TEXT NOT NULL,
             code       TEXT NOT NULL,
             revision   BIGINT NOT NULL,
             created_at BIGINT NOT NULL,
             updated_at BIGINT NOT NULL,
             plate      TEXT NOT NULL DEFAULT '',
             tolerance  DOUBLE PRECISION NOT NULL DEFAULT 0.2,
             events     JSONB NOT NULL DEFAULT '[]'::jsonb
         )",
        // Both pruning statements order by `updated_at`; without this they are
        // sequential scans of the whole table every ten minutes.
        "CREATE INDEX IF NOT EXISTS workspaces_updated_at_idx
             ON workspaces (updated_at DESC, id DESC)",
    ],
), (
    2,
    // The render thumbnail the recents list shows. `DEFAULT ''` rather than
    // nullable so the row maps to the same `String` the filesystem store's
    // `serde` default produces, and so an existing deployment migrates without
    // rewriting every row.
    &["ALTER TABLE workspaces ADD COLUMN IF NOT EXISTS preview TEXT NOT NULL DEFAULT ''"],
), (
    3,
    &[
        // The read-only capability. Unique where it exists, so two workspaces
        // can never answer to the same link; `''` means "never shared", and a
        // partial index lets every unshared row keep that same empty value.
        "ALTER TABLE workspaces ADD COLUMN IF NOT EXISTS share_token TEXT NOT NULL DEFAULT ''",
        "CREATE UNIQUE INDEX IF NOT EXISTS workspaces_share_token_idx
             ON workspaces (share_token) WHERE share_token <> ''",
    ],
), (
    4,
    // Superseded sources for `diff`. A child table rather than a JSONB column
    // so a save writes one row instead of rewriting the whole history, and a
    // read of the current workspace never streams old sources back. Cascade
    // so eviction of a workspace takes its history with it.
    &[
        "CREATE TABLE IF NOT EXISTS workspace_revisions (
             workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
             revision     BIGINT NOT NULL,
             code         TEXT NOT NULL,
             created_at   BIGINT NOT NULL,
             PRIMARY KEY (workspace_id, revision)
         )",
    ],
), (
    5,
    // Creation statistics for the landing page. Neither table references
    // `workspaces`, so eviction lowers neither number. Both are seeded from
    // the rows already present, so a populated deployment does not restart
    // its count at zero.
    &[
        "CREATE TABLE IF NOT EXISTS reopenscad_counters (
             name  TEXT PRIMARY KEY,
             value BIGINT NOT NULL
         )",
        "INSERT INTO reopenscad_counters (name, value)
             SELECT 'workspaces_created', COUNT(*) FROM workspaces
             ON CONFLICT (name) DO NOTHING",
        "CREATE TABLE IF NOT EXISTS workspace_creation_hours (
             hour    BIGINT PRIMARY KEY,
             created BIGINT NOT NULL
         )",
        "INSERT INTO workspace_creation_hours (hour, created)
             SELECT created_at / 3600000, COUNT(*) FROM workspaces
             WHERE created_at >= (EXTRACT(EPOCH FROM now()) * 1000)::BIGINT - 2 * 86400000
             GROUP BY created_at / 3600000
             ON CONFLICT (hour) DO NOTHING",
    ],
), (
    6,
    // Per-minute creation buckets, so "the last 24 hours" is a rolling
    // window rather than the current hour plus the 23 before it. Seeded from
    // the rows present; the sweeper keeps both counters at least as high as
    // the rows prove, which also covers creates by pre-counter binaries
    // during a rolling deploy.
    &[
        "CREATE TABLE IF NOT EXISTS workspace_creation_minutes (
             minute  BIGINT PRIMARY KEY,
             created BIGINT NOT NULL
         )",
        "INSERT INTO workspace_creation_minutes (minute, created)
             SELECT created_at / 60000, COUNT(*) FROM workspaces
             WHERE created_at >= (EXTRACT(EPOCH FROM now()) * 1000)::BIGINT - 2 * 86400000
             GROUP BY created_at / 60000
             ON CONFLICT (minute) DO NOTHING",
    ],
), (
    7,
    // Stored render responses (see `rendercache.rs`), so reopening an
    // unchanged design does not recompile it. Cascade so evicting a design
    // takes its renders with it; `last_used` drives both LRU bounds.
    &[
        "CREATE TABLE IF NOT EXISTS render_cache (
             workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
             cache_key    TEXT NOT NULL,
             body         BYTEA NOT NULL,
             bytes        BIGINT NOT NULL,
             created_at   BIGINT NOT NULL,
             last_used    BIGINT NOT NULL,
             PRIMARY KEY (workspace_id, cache_key)
         )",
        "CREATE INDEX IF NOT EXISTS render_cache_last_used_idx
             ON render_cache (last_used DESC, workspace_id, cache_key)",
    ],
), (
    8,
    // Comments pinned to the model (see `feedback.rs`): one small document
    // per design, its version and open count as columns so the `/events`
    // poll and waiting tools probe without parsing it, and when a browser
    // last polled so any instance can tell whether one is watching. Cascade
    // so evicting a design takes its comments with it.
    &[
        "CREATE TABLE IF NOT EXISTS workspace_feedback (
             workspace_id   TEXT PRIMARY KEY REFERENCES workspaces (id) ON DELETE CASCADE,
             state          JSONB NOT NULL DEFAULT '{}'::jsonb,
             version        BIGINT NOT NULL DEFAULT 0,
             open_count     INTEGER NOT NULL DEFAULT 0,
             viewer_seen_at BIGINT NOT NULL DEFAULT 0
         )",
    ],
), (
    9,
    // Mesh assets a design can `import()` (see `assetstore.rs`): the cleaned
    // binary STL, its digest and the upload report. Cascade so evicting a
    // design takes its assets with it; revisions snapshot only code.
    // `assets_version` moves on every asset write so `/events` polls notice.
    // One-time upload URLs live in a table so any instance can take them.
    &[
        "ALTER TABLE workspaces ADD COLUMN IF NOT EXISTS assets_version BIGINT NOT NULL DEFAULT 0",
        "CREATE TABLE IF NOT EXISTS workspace_assets (
             workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
             name         TEXT NOT NULL,
             sha256       TEXT NOT NULL,
             mesh         BYTEA NOT NULL,
             report       JSONB NOT NULL DEFAULT '{}'::jsonb,
             bytes        BIGINT NOT NULL,
             triangles    BIGINT NOT NULL DEFAULT 0,
             created_at   BIGINT NOT NULL,
             PRIMARY KEY (workspace_id, name)
         )",
        "CREATE TABLE IF NOT EXISTS asset_upload_tokens (
             token        TEXT PRIMARY KEY,
             workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
             name         TEXT NOT NULL,
             budget       BIGINT NOT NULL,
             expires_at   BIGINT NOT NULL
         )",
    ],
), (
    10,
    // Who asked for each one-time upload URL, so pending URLs are capped per
    // client as well as per design.
    &["ALTER TABLE asset_upload_tokens ADD COLUMN IF NOT EXISTS client TEXT NOT NULL DEFAULT ''"],
), (
    11,
    // Chunked uploads (Cloud Run caps a request body at 32 MiB): a session per
    // upload and its staged parts, so whichever instance receives a chunk can
    // append it and the last one can assemble the file. Cascades: evicting a
    // design drops its sessions, dropping a session drops its parts.
    &[
        "CREATE TABLE IF NOT EXISTS asset_upload_sessions (
             upload_id    TEXT PRIMARY KEY,
             target       TEXT NOT NULL,
             workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
             name         TEXT NOT NULL,
             budget       BIGINT NOT NULL,
             total        BIGINT NOT NULL,
             received     BIGINT NOT NULL DEFAULT 0,
             client       TEXT NOT NULL DEFAULT '',
             created_at   BIGINT NOT NULL,
             updated_at   BIGINT NOT NULL
         )",
        "CREATE TABLE IF NOT EXISTS asset_upload_chunks (
             upload_id   TEXT NOT NULL REFERENCES asset_upload_sessions (upload_id) ON DELETE CASCADE,
             byte_offset BIGINT NOT NULL,
             bytes       BYTEA NOT NULL,
             created_at  BIGINT NOT NULL,
             PRIMARY KEY (upload_id, byte_offset)
         )",
    ],
), (
    12,
    // A finish claim, so one request (on any instance) processes a complete
    // upload, and the kept final replies, so a re-sent last chunk whose reply
    // was lost gets it again instead of a 404.
    &[
        "ALTER TABLE asset_upload_sessions ADD COLUMN IF NOT EXISTS finishing_at BIGINT NOT NULL DEFAULT 0",
        "CREATE TABLE IF NOT EXISTS asset_upload_receipts (
             upload_id   TEXT PRIMARY KEY,
             target      TEXT NOT NULL,
             total       BIGINT NOT NULL,
             final_start BIGINT NOT NULL,
             response    JSONB NOT NULL,
             created_at  BIGINT NOT NULL
         )",
        "CREATE INDEX IF NOT EXISTS asset_upload_receipts_created ON asset_upload_receipts (created_at)",
    ],
), (
    13,
    // Whether an upload was closed into a solid. Dropped again in migration 15.
    &[
        "ALTER TABLE asset_upload_tokens ADD COLUMN IF NOT EXISTS seal BOOLEAN NOT NULL DEFAULT FALSE",
        "ALTER TABLE asset_upload_sessions ADD COLUMN IF NOT EXISTS seal BOOLEAN NOT NULL DEFAULT FALSE",
    ],
), (
    14,
    // Named revisions. Rows from before keep '' and are labelled on read, so
    // the ALTERs rewrite nothing (a constant default is catalogue-only).
    &[
        "ALTER TABLE workspaces ADD COLUMN IF NOT EXISTS revision_name TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE workspaces ADD COLUMN IF NOT EXISTS revision_source TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE workspace_revisions ADD COLUMN IF NOT EXISTS name TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE workspace_revisions ADD COLUMN IF NOT EXISTS source TEXT NOT NULL DEFAULT ''",
    ],
), (
    15,
    // Uploaded STLs expire 3 hours after their upload, read-time exact, and an
    // earlier row counts from when it was stored; the sweep frees the space.
    // Expired names are kept (a few per design) so `import()` can say why it
    // fails. Sealing is gone: uploads keep open surfaces open.
    &[
        "ALTER TABLE workspace_assets ADD COLUMN IF NOT EXISTS expires_at BIGINT NOT NULL DEFAULT 0",
        "UPDATE workspace_assets SET expires_at = created_at + 10800000 WHERE expires_at = 0",
        "CREATE INDEX IF NOT EXISTS workspace_assets_expires ON workspace_assets (expires_at)",
        "CREATE TABLE IF NOT EXISTS workspace_assets_expired (
             workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
             name         TEXT NOT NULL,
             expired_at   BIGINT NOT NULL,
             PRIMARY KEY (workspace_id, name)
         )",
        "ALTER TABLE asset_upload_tokens DROP COLUMN IF EXISTS seal",
        "ALTER TABLE asset_upload_sessions DROP COLUMN IF EXISTS seal",
    ],
)];

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// These need a live Postgres, so they are opt-in: set
/// `REOPENSCAD_TEST_DATABASE_URL` and run
/// `cargo test --features postgres pgstore`. Without it every case returns
/// early, so the default `cargo test` is unaffected.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::assetstore::AssetKind;
    use serde_json::json;
    use std::sync::mpsc;

    /// The Cloud Run connection string is a unix socket path, not a host:port,
    /// and getting it wrong means the deployed service cannot reach its
    /// database at all. Both accepted spellings are pinned here because this
    /// is not something a local Postgres test would ever exercise.
    #[test]
    fn a_cloud_sql_socket_url_parses_as_a_unix_host() {
        for connection_string in [
            // libpq keyword/value form, which is what DEPLOY.md recommends.
            "host=/cloudsql/my-project:us-central1:reopenscad \
             user=reopenscad password=secret dbname=reopenscad",
            // URL form with the socket directory as a query parameter.
            "postgresql:///reopenscad\
             ?host=/cloudsql/my-project:us-central1:reopenscad\
             &user=reopenscad&password=secret",
        ] {
            let config = Config::from_str(&connection_string.replace(['\n', ' '], " "))
                .unwrap_or_else(|error| panic!("{connection_string:?}: {error}"));
            assert!(
                config
                    .get_hosts()
                    .iter()
                    .any(|host| matches!(host, postgres::config::Host::Unix(path)
                        if path.to_string_lossy().starts_with("/cloudsql/"))),
                "{connection_string:?} did not resolve to a unix socket: {:?}",
                config.get_hosts()
            );
            assert_eq!(config.get_dbname(), Some("reopenscad"));
        }
    }

    /// Connection failures surface to callers as HTTP 500 bodies, so the
    /// message must never carry the credentials out of the connection string.
    #[test]
    fn a_connection_failure_does_not_echo_the_credentials() {
        let Err(error) = PgWorkspaceStore::open(
            "postgres://someuser:hunter2@127.0.0.1:1/reopenscad?connect_timeout=1",
        ) else {
            panic!("port 1 cannot be a Postgres server");
        };
        assert!(!error.contains("hunter2"), "{error}");
        assert!(!error.contains("someuser"), "{error}");
    }

    fn store() -> Option<PgWorkspaceStore> {
        let url = std::env::var("REOPENSCAD_TEST_DATABASE_URL").ok()?;
        Some(PgWorkspaceStore::open(&url).expect("test database"))
    }

    /// Both landing-page numbers move with every create, inside its
    /// transaction, and eviction lowers neither. Deltas, because the test
    /// database is shared with the other cases.
    #[test]
    fn creation_stats_count_creates_and_survive_eviction() {
        let Some(store) = store() else { return };
        let before = store.creation_stats().unwrap();
        let first = store
            .create_with_settings("cube(1);".into(), None, None, None)
            .unwrap();
        store
            .create_with_settings("cube(2);".into(), None, None, None)
            .unwrap();
        let after = store.creation_stats().unwrap();
        assert!(after.total >= before.total + 2, "{before:?} -> {after:?}");
        assert!(after.last_24h >= before.last_24h + 2, "{before:?} -> {after:?}");
        assert!(after.last_7d >= before.last_7d + 2, "{before:?} -> {after:?}");
        let owned = first.id.clone();
        store
            .pool
            .with(|client| client.execute("DELETE FROM workspaces WHERE id = $1", &[&owned]))
            .unwrap();
        let evicted = store.creation_stats().unwrap();
        assert!(evicted.total >= after.total, "eviction lowered the total");
        assert!(evicted.last_24h >= after.last_24h);
        // Buckets past the window are not counted.
        // One minute past 24 hours is out; the minute before it is in.
        let stale = current_minute() - CREATION_WINDOW_BUCKETS as i64;
        let edge = stale + 1;
        store
            .pool
            .with(|client| {
                client.execute(
                    "INSERT INTO workspace_creation_minutes (minute, created) VALUES ($1, 1000000)
                     ON CONFLICT (minute) DO NOTHING",
                    &[&stale],
                )
            })
            .unwrap();
        assert!(store.creation_stats().unwrap().last_24h < 1_000_000);
        let before_edge = store.creation_stats().unwrap().last_24h;
        store
            .pool
            .with(|client| {
                client.execute(
                    "INSERT INTO workspace_creation_minutes (minute, created) VALUES ($1, 7)
                     ON CONFLICT (minute) DO UPDATE SET created = workspace_creation_minutes.created + 7",
                    &[&edge],
                )
            })
            .unwrap();
        assert!(store.creation_stats().unwrap().last_24h >= before_edge + 7);
        // A row created behind the counters' back (a pre-counter binary) is
        // healed into both numbers by the sweep.
        let healed_before = store.creation_stats().unwrap();
        let (id, now) = (crate::generate_workspace_id(&store.nonce), unix_millis() as i64);
        store
            .pool
            .with(|client| {
                client.execute("DELETE FROM reopenscad_counters", &[])?;
                client.execute(
                    "INSERT INTO workspaces (id, name, code, revision, created_at, updated_at)
                     VALUES ($1, 'x', 'cube(1);', 1, $2, $2)",
                    &[&id, &now],
                )
            })
            .unwrap();
        store.prune().unwrap();
        let healed = store.creation_stats().unwrap();
        assert!(healed.total >= 1, "{healed:?}");
        assert!(healed.last_24h >= healed_before.last_24h.min(1), "{healed:?}");
    }

    #[test]
    fn a_workspace_round_trips_through_the_database() {
        let Some(store) = store() else { return };
        let created = store
            .create_with_settings("cube(1);".into(), Some("Test".into()), None, None)
            .unwrap();
        let loaded = store.get(&created.id).unwrap().expect("persisted");
        assert_eq!(loaded.code, "cube(1);");
        assert_eq!(loaded.revision, 1);
        assert_eq!(store.revision_of(&created.id).unwrap(), Some(1));
    }

    /// The whole reason this backend exists.
    ///
    /// Two writers hold the same base revision, which is precisely the state
    /// two Cloud Run instances are in when a user has the workspace open in
    /// two tabs. Under the filesystem store's in-process `Mutex` both would
    /// pass the revision check in their own process and the second write would
    /// silently overwrite the first. Here the `WHERE revision = $n` predicate
    /// is evaluated by Postgres, so exactly one `UPDATE` matches a row.
    #[test]
    fn concurrent_updates_on_one_base_revision_produce_one_winner() {
        let Some(store) = store() else { return };
        let created = store
            .create_with_settings("cube(1);".into(), None, None, None)
            .unwrap();
        let base = created.revision;

        // A barrier would be better, but a channel rendezvous keeps this to
        // std: both threads block until the main thread releases them at once.
        let (release, start) = mpsc::channel::<()>();
        let start = Arc::new(Mutex::new(start));
        let mut handles = Vec::new();
        for code in ["cube(2);", "cube(3);"] {
            let store = store.clone();
            let id = created.id.clone();
            let start = Arc::clone(&start);
            handles.push(thread::spawn(move || {
                let _ = start.lock().unwrap().recv();
                store.update(
                    &id,
                    WorkspaceUpdate {
                        code: Some(code.to_string()),
                        name: None,
                        start: None,
                        end: None,
                        text: None,
                        expected_revision: Some(base),
                        base_revision: None,
                        plate: None,
                        tolerance: None,
                        source: "browser".into(),
                        revision_name: None,
                    },
                )
            }));
        }
        let _ = release.send(());
        let _ = release.send(());
        let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        let winners = outcomes.iter().filter(|result| result.is_ok()).count();
        let conflicts = outcomes
            .iter()
            .filter(|result| matches!(result, Err(UpdateError::Conflict(_))))
            .count();
        assert_eq!(winners, 1, "exactly one writer may win: {outcomes:?}");
        assert_eq!(conflicts, 1, "the loser must get a conflict: {outcomes:?}");

        // And the surviving document is the winner's, at exactly one bump —
        // no lost update, no double increment.
        let final_state = store.get(&created.id).unwrap().unwrap();
        assert_eq!(final_state.revision, base + 1);
        let winner = outcomes
            .iter()
            .find_map(|result| result.as_ref().ok())
            .unwrap();
        assert_eq!(final_state.code, winner.workspace.code);
    }

    /// Viewer settings take the revisionless path, so a plate change must not
    /// invalidate a code save that a different instance is holding.
    #[test]
    fn settings_writes_do_not_disturb_the_revision() {
        let Some(store) = store() else { return };
        let created = store
            .create_with_settings("cube(1);".into(), None, None, None)
            .unwrap();
        let updated = store
            .update_settings(&created.id, Some("bambulab-x1c"), Some(0.35))
            .unwrap();
        assert_eq!(updated.revision, created.revision);
        assert_eq!(updated.plate, "bambulab-x1c");
        assert!((updated.tolerance - 0.35).abs() < 1e-9);
        // Absent fields mean "leave as is", enforced by the statement itself.
        let again = store.update_settings(&created.id, None, None).unwrap();
        assert_eq!(again.plate, "bambulab-x1c");
    }

    /// A code save must not roll back a build plate somebody picked in another
    /// tab while the save was in flight. `update_settings` is revisionless
    /// precisely so it cannot collide with an edit, which only holds if the
    /// edit statement leaves columns it was not given alone — hence the
    /// `COALESCE`s rather than writing back the values this thread read.
    #[test]
    fn a_code_save_does_not_roll_back_a_concurrent_settings_change() {
        let Some(store) = store() else { return };
        let created = store
            .create_with_settings("cube(1);".into(), None, None, None)
            .unwrap();
        // Stand in for the interleaving: the caller read revision 1 with an
        // empty plate, another tab set a plate, then the caller's edit lands.
        store
            .update_settings(&created.id, Some("prusa-mk4"), Some(0.4))
            .unwrap();
        let saved = store
            .update(
                &created.id,
                WorkspaceUpdate {
                    code: Some("cube(2);".into()),
                    name: None,
                    start: None,
                    end: None,
                    text: None,
                    expected_revision: Some(created.revision),
                    base_revision: None,
                    plate: None,
                    tolerance: None,
                    source: "browser".into(),
                    revision_name: None,
                },
            )
            .expect("a settings write must not consume the revision")
            .workspace;
        assert_eq!(saved.code, "cube(2);");
        assert_eq!(saved.revision, created.revision + 1);
        assert_eq!(saved.plate, "prusa-mk4", "the plate was clobbered");
        assert!((saved.tolerance - 0.4).abs() < 1e-9, "{}", saved.tolerance);
        // The name was never supplied either, so it must be untouched.
        assert_eq!(saved.name, created.name);
    }

    /// The event log is bounded in SQL, not by a read-modify-write in the
    /// process, so it stays capped no matter which instance did the writing.
    #[test]
    fn the_event_log_stays_bounded() {
        let Some(store) = store() else { return };
        let mut workspace = store
            .create_with_settings("cube(1);".into(), None, None, None)
            .unwrap();
        for index in 0..(MAX_EVENTS + 5) {
            workspace = store
                .update(
                    &workspace.id,
                    WorkspaceUpdate {
                        code: Some(format!("cube({});", index + 1)),
                        name: None,
                        start: None,
                        end: None,
                        text: None,
                        expected_revision: Some(workspace.revision),
                        base_revision: None,
                        plate: None,
                        tolerance: None,
                        source: "ai".into(),
                        revision_name: None,
                    },
                )
                .unwrap()
                .workspace;
        }
        assert!(
            workspace.events.len() <= MAX_EVENTS,
            "event log grew to {}",
            workspace.events.len()
        );
        assert_eq!(
            workspace.events.last().unwrap().revision,
            workspace.revision
        );
    }

    fn code_update(code: &str, expected: u64) -> WorkspaceUpdate {
        WorkspaceUpdate {
            code: Some(code.into()),
            name: None,
            start: None,
            end: None,
            text: None,
            expected_revision: Some(expected),
            base_revision: None,
            plate: None,
            tolerance: None,
            source: "ai".into(),
            revision_name: None,
        }
    }

    /// A full-code save records one event per changed region (the log
    /// stays bounded with several events per save), and its predecessor's
    /// history row is timed by that revision's own edit, committed with it.
    #[test]
    fn multi_region_saves_and_history_commit_together() {
        let Some(store) = store() else { return };
        let old: String = (0..60).map(|line| format!("// {line}\n")).collect();
        let created = store.create_with_settings(old.clone(), None, None, None).unwrap();
        let mut revision = created.revision;
        let mut code = old;
        let mut last = None;
        for round in 0..(MAX_EVENTS / 2 + 3) {
            let next = code
                .replacen("// 5\n", &format!("// five {round}\n"), 1)
                .replacen("// 50\n", &format!("// fifty {round}\n"), 1);
            let next = if next == code {
                code.replace(&format!("// five {}\n", round - 1), &format!("// five {round}\n"))
                    .replace(&format!("// fifty {}\n", round - 1), &format!("// fifty {round}\n"))
            } else {
                next
            };
            let result = store.update(&created.id, code_update(&next, revision)).unwrap();
            assert_eq!(result.events.len(), 2, "{:?}", result.events);
            revision = result.workspace.revision;
            code = next;
            last = Some(result);
        }
        let last = last.unwrap();
        assert!(last.workspace.events.len() <= MAX_EVENTS);
        let tail: Vec<_> = last.workspace.events.iter().rev().take(2).collect();
        assert!(tail.iter().all(|event| event.revision == revision));
        // A settings write moves updated_at but not the revision's time.
        let written = last.events[0].timestamp;
        std::thread::sleep(Duration::from_millis(5));
        store.update_settings(&created.id, Some("p1s"), None).unwrap();
        store.update(&created.id, code_update("cube(1);", revision)).unwrap();
        let history = store.revision_list(&created.id).unwrap();
        let entry = history.iter().find(|entry| entry.revision == revision).expect("predecessor kept");
        assert_eq!(entry.timestamp, written);
        // A losing writer records no history.
        let before = store.revision_list(&created.id).unwrap().len();
        assert!(matches!(
            store.update(&created.id, code_update("cube(9);", revision)),
            Err(UpdateError::Conflict(_))
        ));
        assert_eq!(store.revision_list(&created.id).unwrap().len(), before);
    }

    /// Names travel with a revision into the history; rows from before the
    /// migration read as unnamed; a browser's burst keeps one row but moves the
    /// number, and the count ceiling counts rows.
    #[test]
    fn revision_names_persist_and_browser_bursts_coalesce() {
        let Some(store) = store() else { return };
        let created = store
            .create_marked("cube(1);".into(), None, None, None, RevisionMark::agent("First".into()))
            .unwrap();
        assert_eq!(store.get(&created.id).unwrap().unwrap().revision_name, "First");
        let named = |code: &str, expected: u64, name: Option<&str>, source: &str| WorkspaceUpdate {
            revision_name: name.map(str::to_string),
            source: source.into(),
            ..code_update(code, expected)
        };
        store.update(&created.id, named("cube(2);", 1, Some("Second"), "ai")).unwrap();
        let history = store.revision_list(&created.id).unwrap();
        assert_eq!((history[0].name.as_str(), history[0].source.as_str()), ("First", "ai"));
        // Browser saves: revision 3 stays until it is coalesced by 4 and 5.
        store.update(&created.id, named("cube(3);", 2, None, "browser")).unwrap();
        store.update(&created.id, named("cube(4);", 3, None, "browser")).unwrap();
        let latest = store.update(&created.id, named("cube(5);", 4, None, "browser")).unwrap().workspace;
        assert_eq!(latest.revision, 5);
        assert_eq!(latest.revision_name, crate::BROWSER_REVISION_NAME);
        let kept: Vec<u64> = store.revision_list(&created.id).unwrap().iter().map(|entry| entry.revision).collect();
        assert_eq!(kept, vec![1, 2]);
        // A revision a comment names survives the autosave after it.
        store
            .feedback_mutate(&created.id, |state| {
                crate::feedback::add_comment(state, &serde_json::json!({"text": "here", "point": [1, 1, 1], "revision": 5}), 5, unix_millis())
            })
            .unwrap();
        store.update(&created.id, named("cube(6);", 5, None, "browser")).unwrap();
        assert_eq!(store.revision_code(&created.id, 5).unwrap().as_deref(), Some("cube(5);"));
        store.update(&created.id, named("cube(7);", 6, None, "browser")).unwrap();
        assert_eq!(store.revision_code(&created.id, 6).unwrap(), None);
        // A row from before the migration: empty columns, still listed and readable.
        let owned = created.id.clone();
        store
            .pool
            .with(|client| {
                client.execute("UPDATE workspace_revisions SET name = '', source = '' WHERE workspace_id = $1", &[&owned])?;
                client.execute("UPDATE workspaces SET revision_name = '', revision_source = '' WHERE id = $1", &[&owned])
            })
            .unwrap();
        assert_eq!(store.revision_list(&created.id).unwrap()[0].name, "");
        let legacy = store.get(&created.id).unwrap().unwrap();
        assert_eq!(crate::revision_label(&legacy.revision_name, legacy.revision), "Revision 7");
    }

    /// Stored renders round-trip, stay bounded per design, and disappear
    /// with the design.
    #[test]
    fn render_cache_is_bounded_per_design_and_cascades() {
        let Some(store) = store() else { return };
        let workspace = store.create_with_settings("cube(1);".into(), None, None, None).unwrap();
        let key = |index: usize| {
            rendercache::render_key("preview", &format!("cube({index});"), "", &serde_json::json!([]), None)
        };
        assert!(store.cached_render(&workspace.id, &key(0)).is_none());
        for index in 0..=rendercache::ENTRIES_PER_DESIGN {
            store.store_render(&workspace.id, &key(index), format!("{{\"n\":{index}}}").as_bytes());
            thread::sleep(Duration::from_millis(2));
        }
        assert!(store.cached_render(&workspace.id, &key(0)).is_none(), "oldest must be trimmed");
        assert_eq!(
            store.cached_render(&workspace.id, &key(1)).as_deref(),
            Some(&b"{\"n\":1}"[..])
        );
        // Oversized bodies are not stored.
        store.store_render(&workspace.id, &key(50), &vec![b' '; rendercache::MAX_ENTRY_BYTES + 1]);
        assert!(store.cached_render(&workspace.id, &key(50)).is_none());
        let owned = workspace.id.clone();
        store
            .pool
            .with(|client| client.execute("DELETE FROM workspaces WHERE id = $1", &[&owned]))
            .unwrap();
        let remaining: i64 = store
            .pool
            .with(|client| {
                client.query_one("SELECT COUNT(*) FROM render_cache WHERE workspace_id = $1", &[&owned])
            })
            .unwrap()
            .get(0);
        assert_eq!(remaining, 0);
    }

    /// Assets round-trip, bump the design's asset version, respect the
    /// per-design count, and go with the design; upload tokens work once.
    #[test]
    fn assets_round_trip_are_limited_and_cascade() {
        let Some(store) = store() else { return };
        let workspace = store.create_with_settings("cube(1);".into(), None, None, None).unwrap();
        let id = workspace.id.clone();
        let processed = |name: &str, payload: &[u8]| Processed {
            meta: AssetMeta {
                name: name.into(),
                sha256: format!("{:064x}", payload.len()),
                bytes: payload.len() as u64,
                triangles: 1,
                created_at: 1,
                expires_at: unix_millis() + 3_600_000,
                report: serde_json::json!({"clean": {}, "description": {}}),
            },
            stl: payload.to_vec(),
            kind: crate::assetstore::AssetKind::Copy,
        };
        assert_eq!(store.poll_state(&id).unwrap(), Some((1, 0)));
        assert_eq!(store.asset_put(&id, &processed("a.stl", b"abc"), u64::MAX).unwrap().0, 1);
        assert_eq!(store.asset_put(&id, &processed("a.stl", b"abcd"), u64::MAX).unwrap().0, 2);
        let listed = store.asset_list(&id).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].bytes, 4);
        assert_eq!(store.asset_snapshot(&id).unwrap(), (listed.clone(), 2), "list and version from one read");
        assert_eq!(store.asset_blob(&id, "a.stl", &listed[0].sha256).unwrap().as_deref(), Some(&b"abcd"[..]));
        assert_eq!(store.asset_blob(&id, "a.stl", &format!("{:064x}", 3)).unwrap(), None, "never the new bytes under the old hash");
        for index in 1..crate::assetstore::MAX_ASSETS_PER_DESIGN {
            store.asset_put(&id, &processed(&format!("n{index}.stl"), b"x"), u64::MAX).unwrap();
        }
        let refused = store.asset_put(&id, &processed("over.stl", b"x"), u64::MAX).unwrap_err();
        assert_eq!(refused.status, 409);
        assert_eq!(store.asset_put(&id, &processed("over2.stl", b"x"), 0).unwrap_err().status, 409);
        assert_eq!(store.asset_delete(&id, "a.stl").unwrap(), Some(crate::assetstore::MAX_ASSETS_PER_DESIGN as u64 + 2), "two puts of a.stl, one per other asset, then the delete");
        assert_eq!(store.asset_delete(&id, "a.stl").unwrap(), None);
        assert_eq!(store.asset_put("missing-design-0000000000", &processed("a.stl", b"x"), u64::MAX).unwrap_err().status, 404);

        let pending = PendingUpload {
            design_id: id.clone(),
            name: "big.stl".into(),
            budget: 1000,
            expires_at: unix_millis() + 60_000,
            client: format!("client-{id}"),
        };
        let token = crate::assetstore::mint_token().unwrap();
        store.create_upload_token(&token, &pending).unwrap();
        assert_eq!(store.peek_upload_token(&token).unwrap(), Some(pending.clone()), "peeking does not consume");
        assert_eq!(store.take_upload_token(&token).unwrap(), Some(pending.clone()));
        assert_eq!(store.take_upload_token(&token).unwrap(), None);
        store.restore_upload_token(&token, &pending).unwrap();
        assert_eq!(store.take_upload_token(&token).unwrap(), Some(pending.clone()), "restored after a failed upload");
        // At most four pending per design.
        let minted: Vec<String> = (0..crate::assetstore::MAX_PENDING_UPLOADS_PER_DESIGN)
            .map(|_| crate::assetstore::mint_token().unwrap())
            .collect();
        for token in &minted {
            store.create_upload_token(token, &pending).unwrap();
        }
        let refused = store.create_upload_token(&crate::assetstore::mint_token().unwrap(), &pending).unwrap_err();
        assert!(refused.contains("This design"), "{refused}");
        for token in &minted {
            store.take_upload_token(token).unwrap();
        }
        let stale = crate::assetstore::mint_token().unwrap();
        store.create_upload_token(&stale, &PendingUpload { expires_at: unix_millis() + 1, ..pending }).unwrap();
        thread::sleep(Duration::from_millis(5));
        assert_eq!(store.take_upload_token(&stale).unwrap(), None);

        let owned = id.clone();
        store
            .pool
            .with(|client| client.execute("DELETE FROM workspaces WHERE id = $1", &[&owned]))
            .unwrap();
        assert!(store.asset_list(&id).unwrap().is_empty());
    }

    /// Uploads, expiry and the sweep against the real store: one STL per
    /// design under its lock, exact at read time, and several instances
    /// sweeping at once.
    #[test]
    fn assets_expire_at_read_time_and_the_sweep_runs_from_any_instance() {
        let Some(store) = store() else { return };
        let id = store.create_with_settings("cube(1);".into(), None, None, None).unwrap().id;
        let now = unix_millis();
        let processed = |name: &str, kind: AssetKind, payload: &[u8], expires_at: u64| {
            Processed {
                meta: AssetMeta {
                    name: name.into(),
                    sha256: format!("{:064x}", payload.len()),
                    bytes: payload.len() as u64,
                    triangles: 1,
                    created_at: now,
                    expires_at,
                    report: json!({}),
                },
                stl: payload.to_vec(),
                kind,
            }
        };
        let hour = 3_600_000;
        store.asset_put(&id, &processed("scan.stl", AssetKind::Upload, b"abcd", now + hour), u64::MAX).unwrap();
        // A second STL, under any name, is refused by the store itself.
        for name in ["scan.stl", "other.stl"] {
            let refused = store.asset_put(&id, &processed(name, AssetKind::Upload, b"xyz", now + hour), u64::MAX).unwrap_err();
            assert_eq!(refused.status, 409);
            assert!(refused.message.contains("scan.stl"), "{}", refused.message);
        }
        let (live, expired) = store.asset_index(&id, now).unwrap();
        assert_eq!((live.len(), expired.len()), (1, 0));

        // Past the expiry every read has lost it, before any sweep.
        let (live, expired) = store.asset_index(&id, now + hour).unwrap();
        assert!(live.is_empty());
        assert_eq!(expired, vec![("scan.stl".to_string(), now + hour)]);
        let version = store.assets_version(&id).unwrap();

        // Three instances at once: each row goes once, and the version moves once.
        let other = store.create_with_settings("cube(1);".into(), None, None, None).unwrap().id;
        store.asset_put(&other, &processed("keep.stl", AssetKind::Upload, b"abcde", now + 2 * hour), u64::MAX).unwrap();
        let swept: Vec<Vec<(String, Vec<String>)>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..3).map(|_| scope.spawn(|| store.sweep_expired_assets(now + hour).unwrap())).collect();
            handles.into_iter().map(|handle| handle.join().unwrap()).collect()
        });
        let mine: Vec<&String> = swept.iter().flatten().filter(|(design, _)| design == &id).flat_map(|(_, names)| names).collect();
        assert_eq!(mine.len(), 1, "{swept:?}");
        assert_eq!(store.assets_version(&id).unwrap(), version + 1);
        let rows: i64 = store.pool.with(|client| client.query_one("SELECT COUNT(*) FROM workspace_assets WHERE workspace_id = $1", &[&id])).unwrap().get(0);
        assert_eq!(rows, 0, "the space is freed");
        assert_eq!(store.asset_list(&other).unwrap().len(), 1, "another design's live upload stays");
        // The names are remembered, and a new upload of one clears it.
        let (_, expired) = store.asset_index(&id, now + hour).unwrap();
        assert_eq!(expired.len(), 1);
        store.asset_put(&id, &processed("scan.stl", AssetKind::Upload, b"new", now + hour * 3), u64::MAX).unwrap();
        let (live, expired) = store.asset_index(&id, now).unwrap();
        assert_eq!(live.len(), 1);
        assert!(expired.is_empty(), "{expired:?}");
        store.asset_delete(&id, "scan.stl").unwrap().unwrap();
        assert!(store.asset_list(&id).unwrap().is_empty());
        // A legacy server-made row is gone from every read at once, blocks no
        // upload and answers no delete; the next sweep frees it.
        let legacy = Processed {
            meta: AssetMeta { created_at: now - hour, report: json!({"origin": "derived", "fit": {"parent": "scan.stl"}}), ..processed("hull.stl", AssetKind::Copy, b"ab", now + hour).meta },
            ..processed("hull.stl", AssetKind::Copy, b"ab", now + hour)
        };
        store.asset_put(&id, &legacy, u64::MAX).unwrap();
        assert!(store.asset_list(&id).unwrap().is_empty());
        assert_eq!(store.asset_index(&id, now).unwrap().1, vec![("hull.stl".to_string(), now - hour)]);
        assert_eq!(store.asset_delete(&id, "hull.stl").unwrap(), None);
        let swept = store.sweep_expired_assets(now).unwrap();
        assert!(swept.iter().any(|(design, names)| design == &id && names == &vec!["hull.stl".to_string()]), "{swept:?}");
        let rows: i64 = store.pool.with(|client| client.query_one("SELECT COUNT(*) FROM workspace_assets WHERE workspace_id = $1", &[&id])).unwrap().get(0);
        assert_eq!(rows, 0);
        store.asset_put(&id, &legacy, u64::MAX).unwrap();
        store.asset_put(&id, &processed("scan.stl", AssetKind::Upload, b"new", now + hour * 3), u64::MAX).unwrap();
        assert_eq!(store.asset_list(&id).unwrap().len(), 1);
        store.asset_delete(&id, "scan.stl").unwrap().unwrap();
        // A row from before expiry existed counts from when it was stored.
        store.asset_put(&id, &processed("old.stl", AssetKind::Upload, b"ab", now + hour), u64::MAX).unwrap();
        let owned = id.clone();
        store.pool.with(|client| client.execute("UPDATE workspace_assets SET expires_at = 0, created_at = $2 WHERE workspace_id = $1", &[&owned, &((now - 2 * hour) as i64)])).unwrap();
        assert_eq!(store.asset_list(&id).unwrap()[0].expiry(), now + hour);
        assert_eq!(store.asset_index(&id, now + hour).unwrap().0.len(), 0);
    }

    /// A put and a delete of the same design both lock the design's row
    /// first, so running them against each other never deadlocks.
    #[test]
    fn concurrent_asset_puts_and_deletes_do_not_deadlock() {
        let Some(store) = store() else { return };
        let id = store.create_with_settings("cube(1);".into(), None, None, None).unwrap().id;
        let processed = |name: &str| Processed {
            meta: AssetMeta {
                name: name.into(),
                sha256: format!("{:064x}", 7),
                bytes: 3,
                triangles: 1,
                created_at: 1,
                expires_at: unix_millis() + 3_600_000,
                report: serde_json::json!({}),
            },
            stl: b"abc".to_vec(),
            kind: crate::assetstore::AssetKind::Copy,
        };
        thread::scope(|scope| {
            for worker in 0..4 {
                let (store, id) = (&store, &id);
                scope.spawn(move || {
                    for round in 0..25 {
                        let name = format!("n{}.stl", (worker + round) % 3);
                        if (worker + round) % 2 == 0 {
                            store.asset_put(id, &processed(&name), u64::MAX).unwrap();
                        } else {
                            store.asset_delete(id, &name).unwrap();
                        }
                    }
                });
            }
        });
        let (metas, version) = store.asset_snapshot(&id).unwrap();
        assert!(metas.len() <= 3 && version > 0);
    }

    /// Over the deployment ceiling a put evicts the least recently used
    /// other design's assets (bumping its version) instead of refusing.
    #[test]
    fn a_full_store_evicts_least_recently_used_assets() {
        let Some(store) = store() else { return };
        let processed = |name: &str, payload: &[u8]| Processed {
            meta: AssetMeta {
                name: name.into(),
                sha256: format!("{:064x}", payload.len()),
                bytes: payload.len() as u64,
                triangles: 1,
                created_at: 1,
                expires_at: unix_millis() + 3_600_000,
                report: serde_json::json!({}),
            },
            stl: payload.to_vec(),
            kind: crate::assetstore::AssetKind::Copy,
        };
        let old = store.create_with_settings("cube(1);".into(), None, None, None).unwrap().id;
        let new = store.create_with_settings("cube(2);".into(), None, None, None).unwrap().id;
        store.asset_put(&old, &processed("a.stl", &[1u8; 64]), u64::MAX).unwrap();
        let owned = old.clone();
        store
            .pool
            .with(|client| client.execute("UPDATE workspaces SET updated_at = 0 WHERE id = $1", &[&owned]))
            .unwrap();
        let total: i64 = store
            .pool
            .with(|client| client.query_one("SELECT COALESCE(SUM(bytes), 0)::BIGINT FROM workspace_assets", &[]))
            .unwrap()
            .get(0);
        let (_, evicted) = store.asset_put(&new, &processed("b.stl", &[2u8; 32]), total as u64).unwrap();
        assert_eq!(evicted, vec![old.clone()]);
        assert_eq!(store.asset_snapshot(&old).unwrap(), (Vec::new(), 2));
        assert_eq!(store.asset_list(&new).unwrap().len(), 1);
    }

    /// Superseded sources land in `workspace_revisions`, stay bounded, and
    /// disappear with the workspace.
    #[test]
    fn feedback_round_trips_serialises_writers_and_cascades() {
        let Some(store) = store() else { return };
        let workspace = store.create_with_settings("cube(1);".into(), None, None, None).unwrap();
        let id = workspace.id.clone();
        assert_eq!(store.feedback_summary(&id).unwrap(), (0, 0));
        let input = serde_json::json!({"text": "here", "point": [1, 2, 3]});
        // Concurrent writers from several connections all land.
        thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..3 {
                        store
                            .feedback_mutate(&id, |state| crate::feedback::add_comment(state, &input, 1, crate::unix_millis()))
                            .unwrap();
                    }
                });
            }
        });
        let state = store.feedback_state(&id).unwrap();
        assert_eq!(state.comments.len(), 12);
        assert_eq!(state.last_seq, 12);
        assert_eq!(store.feedback_summary(&id).unwrap(), (12, 12));
        // A failed change writes nothing.
        assert!(store.feedback_mutate(&id, |state| crate::feedback::add_comment(state, &serde_json::json!({}), 1, 1)).is_err());
        assert_eq!(store.feedback_summary(&id).unwrap().0, 12);
        store.note_viewer(&id, 4242);
        assert_eq!(store.viewer_seen_at(&id), 4242);
        assert_eq!(store.feedback_state(&id).unwrap().comments.len(), 12, "presence leaves comments alone");
        // Missing designs are refused rather than given orphan rows.
        assert!(store.feedback_mutate("no-such-design-zz", |_| Ok(())).is_err());
        store.note_viewer("no-such-design-zz", 1);
        store
            .pool
            .with(|client| client.execute("DELETE FROM workspaces WHERE id = $1", &[&id]))
            .unwrap();
        assert_eq!(store.feedback_summary(&id).unwrap(), (0, 0));
    }

    #[test]
    fn revision_history_is_kept_bounded_and_cascades() {
        let Some(store) = store() else { return };
        let mut workspace = store.create_with_settings("cube(1);".into(), None, None, None).unwrap();
        let total = MAX_REVISION_HISTORY as u64 + 3;
        for size in 2..=total + 1 {
            workspace = store
                .update(
                    &workspace.id,
                    WorkspaceUpdate {
                        code: Some(format!("cube({size});")),
                        name: None,
                        start: None,
                        end: None,
                        text: None,
                        expected_revision: Some(workspace.revision),
                        base_revision: None,
                        plate: None,
                        tolerance: None,
                        source: "ai".into(),
                        revision_name: None,
                    },
                )
                .unwrap()
                .workspace;
        }
        let retained = store.revision_list(&workspace.id).unwrap();
        assert_eq!(retained.len(), MAX_REVISION_HISTORY);
        assert_eq!(retained.last().unwrap().revision, workspace.revision - 1);
        assert_eq!(
            store.revision_code(&workspace.id, workspace.revision - 1).unwrap().as_deref(),
            Some(format!("cube({});", workspace.revision - 1).as_str())
        );
        assert_eq!(
            store.revision_code(&workspace.id, workspace.revision).unwrap(),
            Some(workspace.code.clone())
        );
        assert!(store.revision_code(&workspace.id, 1).unwrap().is_none());
        let owned = workspace.id.clone();
        store
            .pool
            .with(|client| client.execute("DELETE FROM workspaces WHERE id = $1", &[&owned]))
            .unwrap();
        assert!(store.revision_list(&workspace.id).unwrap().is_empty());
    }

    /// Chunked uploads stage in Postgres: chunks sent to different instances
    /// (two store handles) append in order under the session's row lock, the
    /// file assembles from either, caps hold per design and per URL, and idle
    /// sessions are swept with their parts.
    #[test]
    fn chunked_upload_sessions_span_instances() {
        let Some(first) = store() else { return };
        let Some(second) = store() else { return };
        let design = first.create_with_settings("cube(1);".into(), None, None, None).unwrap().id;
        let now = unix_millis();
        let client = format!("pg-client-{}", crate::assetstore::mint_token().unwrap());
        let session = |name: &str, target: String, client: &str, total: u64| UploadSession {
            upload_id: crate::assetstore::mint_token().unwrap(),
            target,
            design_id: design.clone(),
            name: name.into(),
            budget: 1000,
            total,
            received: 0,
            client: client.into(),
            updated_at: now,
        };
        let mib = 1024 * 1024u64;
        let data: Vec<u8> = (0..(2 * mib + 77)).map(|index| (index % 251) as u8).collect();
        let upload = session("a.stl", format!("design:{design}/a.stl"), &client, data.len() as u64);
        first.open_upload_session(&upload, now).unwrap();
        let id = upload.upload_id.clone();
        assert_eq!(first.append_upload_chunk(&id, 0, &data[..mib as usize], now).unwrap(), mib);
        let conflict = second.append_upload_chunk(&id, 2 * mib, &data[2 * mib as usize..], now).unwrap_err();
        assert_eq!(conflict.status, 409, "{}", conflict.message);
        assert_eq!(second.append_upload_chunk(&id, mib, &data[mib as usize..2 * mib as usize], now).unwrap(), 2 * mib);
        assert_eq!(first.append_upload_chunk(&id, mib, &data[mib as usize..2 * mib as usize], now).unwrap(), 2 * mib, "a re-sent chunk is a no-op");
        assert_eq!(second.append_upload_chunk(&id, 2 * mib, &data[2 * mib as usize..], now).unwrap(), data.len() as u64);
        assert_eq!(second.upload_session(&id, now).unwrap().unwrap().received, data.len() as u64);
        assert_eq!(first.upload_session_bytes(&id).unwrap(), data);

        // Per design: this one already has a session, a second fits, a third does not.
        first.open_upload_session(&session("b.stl", format!("design:{design}/b.stl"), "pg-other-1", 2 * mib), now).unwrap();
        let refused = first.open_upload_session(&session("c.stl", format!("design:{design}/c.stl"), "pg-other-2", 2 * mib), now).unwrap_err();
        assert_eq!(refused.status, 429, "{}", refused.message);
        // The same client restarting a target replaces its session.
        let restarted = session("a.stl", format!("design:{design}/a.stl"), &client, 3 * mib);
        first.open_upload_session(&restarted, now).unwrap();
        assert!(first.upload_session(&id, now).unwrap().is_none(), "replaced");
        let parts: i64 = first
            .pool
            .with(|connection| connection.query_one("SELECT COUNT(*) FROM asset_upload_chunks WHERE upload_id = $1", &[&id]))
            .unwrap()
            .get(0);
        assert_eq!(parts, 0, "its parts went with it");

        // Idle sessions stop answering and the sweeper drops them.
        let other = first.create_with_settings("cube(2);".into(), None, None, None).unwrap().id;
        let stale = UploadSession {
            design_id: other.clone(),
            ..session("s.stl", format!("token:{}", crate::assetstore::mint_token().unwrap()), "pg-stale", 2 * mib)
        };
        let long_ago = now - UPLOAD_SESSION_IDLE_MS - 1;
        first.open_upload_session(&stale, long_ago).unwrap();
        first.append_upload_chunk(&stale.upload_id, 0, &data[..mib as usize], long_ago).unwrap();
        assert!(first.upload_session(&stale.upload_id, now).unwrap().is_none());
        assert_eq!(first.append_upload_chunk(&stale.upload_id, mib, &data[mib as usize..2 * mib as usize], now).unwrap_err().status, 404);
        first.prune().unwrap();
        let left: i64 = first
            .pool
            .with(|connection| {
                connection.query_one("SELECT COUNT(*) FROM asset_upload_sessions WHERE upload_id = $1", &[&stale.upload_id])
            })
            .unwrap()
            .get(0);
        assert_eq!(left, 0, "swept");
        first.drop_upload_session(&restarted.upload_id).unwrap();
        // Evicting the design takes its sessions.
        first.pool.with(|connection| connection.execute("DELETE FROM workspaces WHERE id = $1", &[&design])).unwrap();
        let left: i64 = first
            .pool
            .with(|connection| connection.query_one("SELECT COUNT(*) FROM asset_upload_sessions WHERE workspace_id = $1", &[&design]))
            .unwrap()
            .get(0);
        assert_eq!(left, 0, "cascaded");
    }

    /// Races between instances (two store handles, many threads): one
    /// writer per chunk, one finisher per file, a receipt any instance can
    /// replay, and a sweep that never takes a session a chunk just refreshed.
    #[test]
    fn chunked_upload_races_across_instances() {
        let Some(first) = store() else { return };
        let Some(second) = store() else { return };
        let stores = Arc::new([first, second]);
        let design = stores[0].create_with_settings("cube(1);".into(), None, None, None).unwrap().id;
        let mib = 1024 * 1024u64;
        let data: Arc<Vec<u8>> = Arc::new((0..(3 * mib + 5)).map(|index| (index % 241) as u8).collect());
        let new_session = |name: &str, client: &str, total: u64, now: u64| UploadSession {
            upload_id: crate::assetstore::mint_token().unwrap(),
            target: format!("design:{design}/{name}"),
            design_id: design.clone(),
            name: name.into(),
            budget: 1000,
            total,
            received: 0,
            client: client.into(),
            updated_at: now,
        };
        let client = format!("pg-race-{}", crate::assetstore::mint_token().unwrap());
        let upload = new_session("race.stl", &client, data.len() as u64, unix_millis());
        stores[0].open_upload_session(&upload, unix_millis()).unwrap();
        let id = upload.upload_id.clone();

        // Every chunk sent by 6 threads at once over both instances.
        for (start, end) in [(0, mib), (mib, 2 * mib), (2 * mib, data.len() as u64)] {
            let outcomes: Vec<Result<u64, u16>> = (0..6)
                .map(|thread_index| {
                    let (stores, data, id) = (Arc::clone(&stores), Arc::clone(&data), id.clone());
                    thread::spawn(move || {
                        stores[thread_index % 2]
                            .append_upload_chunk(&id, start, &data[start as usize..end as usize], unix_millis())
                            .map_err(|error| error.status)
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect();
            assert!(outcomes.iter().all(|outcome| *outcome == Ok(end)), "{outcomes:?}");
        }
        assert_eq!(stores[1].upload_session_bytes(&id).unwrap(), *data, "one copy of each chunk, in order");

        // Both instances try to finish at once: exactly one claim.
        let claims: Vec<bool> = (0..8)
            .map(|thread_index| {
                let (stores, id) = (Arc::clone(&stores), id.clone());
                thread::spawn(move || stores[thread_index % 2].claim_upload_finish(&id, unix_millis()).unwrap())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(claims.iter().filter(|claimed| **claimed).count(), 1, "{claims:?}");
        // The same client restarting the target does not drop a session being finished.
        let restart = new_session("race.stl", &client, 2 * mib, unix_millis());
        let _ = stores[1].open_upload_session(&restart, unix_millis());
        assert!(stores[0].upload_session(&id, unix_millis()).unwrap().is_some(), "the claimed session survives");
        let _ = stores[0].drop_upload_session(&restart.upload_id);
        // A stale claim (crashed finisher) can be retaken; a released one too.
        assert!(stores[0].claim_upload_finish(&id, unix_millis() + UPLOAD_FINISH_CLAIM_MS + 1).unwrap());
        stores[1].release_upload_finish(&id).unwrap();
        assert!(stores[1].claim_upload_finish(&id, unix_millis()).unwrap());

        // Completing keeps the reply (readable on the other instance) and
        // drops the session and its parts in one go.
        let response = serde_json::json!({"ok": true, "name": "race.stl", "bytes": 684});
        let receipt = UploadReceipt {
            upload_id: id.clone(),
            target: upload.target.clone(),
            total: data.len() as u64,
            final_start: 2 * mib,
            response: response.clone(),
            created_at: unix_millis(),
        };
        stores[0].complete_upload_session(&receipt).unwrap();
        assert!(stores[1].upload_session(&id, unix_millis()).unwrap().is_none());
        let parts: i64 = stores[1]
            .pool
            .with(|connection| connection.query_one("SELECT COUNT(*) FROM asset_upload_chunks WHERE upload_id = $1", &[&id]))
            .unwrap()
            .get(0);
        assert_eq!(parts, 0);
        let kept = stores[1].upload_receipt(&id, unix_millis()).unwrap().expect("kept");
        assert_eq!(kept.response, response);
        let range = crate::assetstore::ContentRange { start: 2 * mib, end: data.len() as u64, total: data.len() as u64 };
        assert!(kept.replays(&upload.target, &range));
        assert!(stores[1].upload_receipt(&id, unix_millis() + UPLOAD_RECEIPT_TTL_MS).unwrap().is_none(), "expires");
        assert_eq!(stores[1].claim_upload_finish(&id, unix_millis()).unwrap_err(), UNKNOWN_UPLOAD_SESSION);

        // Sweep against a session on the edge of expiry: an append that
        // succeeded always leaves its session and parts in place.
        for _ in 0..20 {
            let edge = unix_millis() - UPLOAD_SESSION_IDLE_MS + 15;
            let racing = new_session("edge.stl", &format!("pg-edge-{}", crate::assetstore::mint_token().unwrap()), 2 * mib, edge);
            stores[0].open_upload_session(&racing, edge).unwrap();
            let appender = {
                let (stores, data, id) = (Arc::clone(&stores), Arc::clone(&data), racing.upload_id.clone());
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(14));
                    stores[1].append_upload_chunk(&id, 0, &data[..mib as usize], unix_millis())
                })
            };
            thread::sleep(Duration::from_millis(15));
            stores[0].prune().unwrap();
            let appended = appender.join().unwrap();
            let alive = stores[0].upload_session(&racing.upload_id, unix_millis()).unwrap();
            match appended {
                Ok(received) => {
                    assert_eq!(received, mib);
                    assert_eq!(alive.map(|session| session.received), Some(mib), "an accepted chunk's session is kept");
                }
                Err(error) => assert_eq!(error.status, 404, "{}", error.message),
            }
            let _ = stores[0].drop_upload_session(&racing.upload_id);
        }
        stores[0].pool.with(|connection| connection.execute("DELETE FROM workspaces WHERE id = $1", &[&design])).unwrap();
    }
}
