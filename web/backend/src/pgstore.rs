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

use crate::{
    default_workspace_name, normalize_source, sanitize_name, sanitize_plate, sanitize_tolerance,
    unix_millis, valid_workspace_id, validate_source, EditResult, UpdateError, Workspace,
    WorkspaceEvent, WorkspaceUpdate, DEFAULT_TOLERANCE, MAX_BODY, MAX_EVENTS, MAX_WORKSPACES,
    WORKSPACE_TTL_MS,
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

const SELECT_COLUMNS: &str =
    "id, name, code, revision, created_at, updated_at, plate, tolerance, events";

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
        let unavailable = || "Workspace store is unavailable.".to_string();
        let mut leased = self.leased.lock().map_err(|_| unavailable())?;
        let deadline = Instant::now() + CHECKOUT_TIMEOUT;
        while *leased >= MAX_TOTAL_CONNECTIONS {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("The workspace store is busy. Retry shortly.".into());
            }
            let (guard, timeout) = self
                .released
                .wait_timeout(leased, remaining)
                .map_err(|_| unavailable())?;
            leased = guard;
            if timeout.timed_out() && *leased >= MAX_TOTAL_CONNECTIONS {
                return Err("The workspace store is busy. Retry shortly.".into());
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
                .map_err(|_| "Workspace store is unavailable.".to_string())?
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
    "The workspace store is temporarily unavailable.".to_string()
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
    "The workspace store is temporarily unavailable.".to_string()
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
    fn prune(&self) -> Result<(), String> {
        let cutoff = unix_millis().saturating_sub(WORKSPACE_TTL_MS) as i64;
        let overflow = max_workspaces() as i64;
        self.pool.with(|client| {
            client.execute("DELETE FROM workspaces WHERE updated_at < $1", &[&cutoff])?;
            client.execute(
                "DELETE FROM workspaces WHERE id IN (
                     SELECT id FROM workspaces ORDER BY updated_at DESC, id DESC OFFSET $1
                 )",
                &[&overflow],
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
        if code.len() > MAX_BODY {
            return Err("Workspace code exceeds the 2 MiB limit.".into());
        }
        let now = unix_millis() as i64;
        let name = sanitize_name(name);
        let plate = plate.as_deref().map(sanitize_plate).unwrap_or_default();
        let tolerance = tolerance.map(sanitize_tolerance).unwrap_or(DEFAULT_TOLERANCE);
        // Ids carry 112 bits of entropy, so a collision is theoretical; the
        // loop exists because `ON CONFLICT DO NOTHING` makes handling it free.
        for _ in 0..8 {
            let id = self.generate_id();
            let inserted = self.pool.with(|client| {
                client.execute(
                    "INSERT INTO workspaces
                         (id, name, code, revision, created_at, updated_at, plate, tolerance, events)
                     VALUES ($1, $2, $3, 1, $4, $4, $5, $6, '[]'::jsonb)
                     ON CONFLICT (id) DO NOTHING",
                    &[&id, &name, &code, &now, &plate, &tolerance],
                )
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
                    events: Vec::new(),
                });
            }
        }
        Err("Could not allocate a workspace id.".into())
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
        let current = self
            .get(id)
            .map_err(UpdateError::Internal)?
            .ok_or(UpdateError::NotFound)?;
        if expected != current.revision {
            return Err(UpdateError::Conflict(current));
        }
        let (code, start, end, inserted_length) = if let Some(code) = update.code {
            let old_len = current.code.len();
            let new_len = code.len();
            (code, 0, old_len, new_len)
        } else {
            let start = update.start.ok_or_else(|| {
                UpdateError::Invalid("Provide code or a start/end/text edit.".into())
            })?;
            let end = update.end.ok_or_else(|| {
                UpdateError::Invalid("Provide code or a start/end/text edit.".into())
            })?;
            let text = update.text.unwrap_or_default();
            if start > end
                || end > current.code.len()
                || !current.code.is_char_boundary(start)
                || !current.code.is_char_boundary(end)
            {
                return Err(UpdateError::Invalid(
                    "Edit range must use valid UTF-8 byte offsets within the document.".into(),
                ));
            }
            let mut code = current.code.clone();
            code.replace_range(start..end, &text);
            let inserted_length = text.len();
            (code, start, end, inserted_length)
        };
        if code.len() > MAX_BODY {
            return Err(UpdateError::Invalid(
                "Workspace code exceeds the 2 MiB limit.".into(),
            ));
        }
        validate_source(&code).map_err(UpdateError::Validation)?;

        let now = unix_millis();
        let event = WorkspaceEvent {
            workspace_id: id.to_string(),
            revision: current.revision + 1,
            source: normalize_source(&update.source),
            start,
            end,
            inserted_length,
            timestamp: now,
        };
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
        let appended = serde_json::to_value([&event])
            .map_err(|error| UpdateError::Internal(error.to_string()))?;

        // The compare-and-swap. `revision = revision + 1` is computed by the
        // server under the row lock, and the `AND revision = $9` predicate is
        // what makes a concurrent writer on the same base revision lose. There
        // is no read-modify-write window for a second instance to slip into.
        let owned = id.to_string();
        let now_i64 = now as i64;
        let cap = MAX_EVENTS as i32;
        let expected_i64 = expected as i64;
        let row = self
            .pool
            .with(|client| {
                client.query_opt(
                    &format!(
                        "UPDATE workspaces SET
                             code = $1,
                             name = COALESCE($2, name),
                             plate = COALESCE($3, plate),
                             tolerance = COALESCE($4, tolerance),
                             updated_at = $5,
                             revision = revision + 1,
                             events = (CASE WHEN jsonb_array_length(events) >= $6
                                            THEN events - 0 ELSE events END) || $7
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
                    ],
                )
            })
            .map_err(UpdateError::Internal)?;
        match row {
            Some(row) => Ok(EditResult {
                workspace: row_to_workspace(&row).map_err(UpdateError::Internal)?,
                event,
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

    fn generate_id(&self) -> String {
        crate::generate_workspace_id(&self.nonce)
    }
}

/// Maps a row, without panicking on a schema that is not what we expect.
///
/// `Row::get` panics on a type mismatch, which in a thread-per-connection
/// server means the client's socket is closed mid-response and it sees a reset
/// rather than an error. A hand-applied or half-migrated table — `revision`
/// created as `integer`, say — would do exactly that on every read. `try_get`
/// turns it into a 500 with a log line instead.
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
}
