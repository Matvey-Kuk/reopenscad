#[allow(dead_code)]
#[path = "csg/mod.rs"]
mod csg;
mod engine;
/// `Preview::vertices` is read only by this module's own tests.
#[allow(dead_code)]
mod mcpapp;
/// Postgres workspace storage. Optional: the crate builds and every test runs
/// without it, and only a deployment that sets `DATABASE_URL` needs it.
#[cfg(feature = "postgres")]
mod pgstore;
mod threemf;

use mcpapp::base64_encode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
#[cfg(unix)]
use std::{os::unix::fs::OpenOptionsExt, os::unix::fs::PermissionsExt};

const MAX_BODY: usize = 2 * 1024 * 1024;
const MAX_OUTPUT: usize = 64 * 1024 * 1024;
/// Compute jobs that may run at the same time. One permit is one *request*,
/// not one core: if the engine fans a single compile out across worker threads
/// the permit still covers all of them, so this number times the engine's
/// per-request thread count is the real CPU ceiling. Tune it with
/// `REOPENSCAD_MAX_CONCURRENT_JOBS` rather than by editing this line, so the
/// deadline constants the browser cross-checks stay stable.
const MAX_CONCURRENT_JOBS: usize = 2;
const MAX_EVENTS: usize = 64;

// ---------------------------------------------------------------------------
// Denial-of-service guard rails.
//
// The service renders untrusted geometry for anonymous callers, so every
// unbounded resource (threads, sockets, header buffers, workspace files,
// resident workspace source, response bytes) needs an explicit ceiling.
// ---------------------------------------------------------------------------

/// Sockets served concurrently. Each one owns a thread; past this the accept
/// loop sheds load with 503 instead of spawning until the OS refuses.
const MAX_CONNECTIONS: usize = 256;
/// Long-poll (`/events`) slots. These threads sleep for seconds at a time, so
/// they get their own budget rather than competing for `MAX_CONNECTIONS`.
const MAX_CONCURRENT_POLLS: usize = 32;
/// Compute jobs that may *wait* for a slot at once, across every client.
///
/// The queue is what makes a third concurrent user wait instead of being
/// rejected, but an unbounded queue is itself the attack: it costs memory, it
/// costs a parked connection thread per entry, and it converts a load spike
/// into a latency spike nobody can escape. Ten waiters plus
/// `MAX_CONCURRENT_JOBS` runners is twelve of the 256 connection threads, so
/// the queue can never crowd out the cheap routes. It is also a wait a caller
/// can reason about: ten jobs ahead of a ~60 s render is already past
/// `MAX_QUEUE_WAIT`, so a deeper queue would only manufacture waits that end
/// in a timeout anyway.
const MAX_QUEUED_JOBS: usize = 10;
/// Waiting jobs a single client may hold.
///
/// Round-robin already stops one client from being *served* unfairly, but it
/// does nothing about admission: without this cap one caller could fill all
/// ten waiting places and everyone else would meet a full queue, which is
/// exactly the starvation the fair scheduler exists to prevent. Three of ten
/// guarantees at least four distinct clients can always be queued at once,
/// while still letting a single editor keep a preview, a final render and an
/// export in flight.
const MAX_QUEUED_PER_CLIENT: usize = 3;
/// Longest a request may wait for a compute slot before it is turned away.
///
/// Deliberately equal to `PREVIEW_DEADLINE`: waiting longer for a slot than a
/// whole preview is allowed to take means at least two jobs are ahead of you
/// and retrying later is genuinely better than holding the socket. This time
/// is *not* charged against the render deadline — see `RenderDeadline`.
const MAX_QUEUE_WAIT: Duration = Duration::from_secs(60);
/// Coarse cost of one compute job, used only to turn queue depth into a
/// `Retry-After` a caller can act on. Deliberately an order-of-magnitude
/// figure — a real workspace on the exact kernel takes tens of seconds — and
/// always presented to callers as an estimate.
const QUEUE_DRAIN_ESTIMATE_SECS: u64 = 15;
/// How often a queued job re-checks its own cancellation flag. A grant is
/// delivered by condvar notification, so this only bounds cancellation
/// latency, not scheduling latency.
const QUEUE_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Total bytes accepted for the header block (request line excluded).
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// Maximum individual header lines retained for one request.
const MAX_HEADERS: usize = 100;
/// Longest accepted request line (method + path + query + version).
const MAX_REQUEST_LINE: usize = 8 * 1024;
/// Wall-clock budget for the request line plus every header.
const HEADER_DEADLINE: Duration = Duration::from_secs(10);
/// Additional wall-clock budget for streaming the body after the headers.
const BODY_DEADLINE: Duration = Duration::from_secs(20);
/// Per-syscall socket timeouts. The deadlines above bound the whole request.
const SOCKET_READ_TIMEOUT: Duration = Duration::from_secs(5);
const SOCKET_WRITE_TIMEOUT: Duration = Duration::from_secs(20);
/// Chunk used while streaming a request body so a claimed Content-Length
/// cannot pre-allocate memory the peer never sends.
const BODY_CHUNK: usize = 16 * 1024;

/// Workspace files kept on disk. Oldest are evicted once the cap is reached.
const MAX_WORKSPACES: usize = 512;
/// Workspaces held in RAM with their full source. Everything else lives on
/// disk and is faulted in on demand.
const MAX_CACHED_WORKSPACES: usize = 64;
/// Idle lifetime of a workspace file before the sweeper deletes it (14 days).
const WORKSPACE_TTL_MS: u64 = 14 * 24 * 60 * 60 * 1000;

/// Per-IP request bucket: burst then a sustained refill rate.
const IP_REQUEST_BURST: f64 = 300.0;
const IP_REQUEST_REFILL_PER_SEC: f64 = 60.0;
/// Per-IP bucket for evaluator/render/export/MCP routes.
const IP_HEAVY_BURST: f64 = 60.0;
const IP_HEAVY_REFILL_PER_SEC: f64 = 6.0;
/// Token cost charged to the heavy bucket per route class.
const HEAVY_COST_EVALUATOR: f64 = 5.0;
const HEAVY_COST_POLL: f64 = 1.0;
/// Per-IP response-byte budget. Answers amplification (a tiny POST /api/render
/// can return megabytes) by billing what was actually served.
const IP_BYTE_BURST: f64 = 128.0 * 1024.0 * 1024.0;
const IP_BYTE_REFILL_PER_SEC: f64 = 16.0 * 1024.0 * 1024.0;
/// Bounds the rate limiter itself so it cannot become the memory-exhaustion
/// vector it is meant to prevent.
const MAX_RATE_BUCKETS: usize = 4096;
/// Buckets untouched for this long are recycled first.
const RATE_BUCKET_IDLE: Duration = Duration::from_secs(120);
const DEFAULT_SOURCE: &str = "// Welcome to ReOpenSCAD\ncube(10, center=true);\n";
/// Legacy-era MCP revisions: the ones that open with an `initialize` handshake.
/// Oldest first; the last entry is what an unknown revision negotiates down to.
const SUPPORTED_MCP_PROTOCOLS: &[&str] = &["2025-03-26", "2025-06-18", "2025-11-25"];
/// Modern-era MCP revisions: stateless, no handshake, protocol version carried
/// per request in `_meta`. Serving both eras on one endpoint is explicitly
/// sanctioned ("A dual-era server MAY serve both eras concurrently on the same
/// endpoint or process").
const SUPPORTED_MODERN_MCP_PROTOCOLS: &[&str] = &["2026-07-28"];
/// Reserved `_meta` keys carrying the modern per-request protocol fields.
const MCP_META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const MCP_META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const MCP_META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
/// Non-standard `_meta` key (hence our own prefix) advertising the whole
/// version roster, so the UI's connection panel reads it instead of hardcoding.
const MCP_META_SUPPORTED_VERSIONS: &str = "io.reopenscad/supportedProtocolVersions";
/// MCP-specification error codes (the `-32020..=-32099` reserved sub-range).
const MCP_HEADER_MISMATCH: i64 = -32020;
const MCP_UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;
/// Freshness hint for the results that never change while the process runs:
/// the tool list, the resource list and the SCAD dictionary are all compiled in.
const MCP_STATIC_CACHE_TTL_MS: u64 = 3_600_000;
/// Shared by legacy `initialize` and modern `server/discover` so the two eras
/// describe the server identically.
const MCP_INSTRUCTIONS: &str = "Create and edit persistent single-file SCAD workspaces, then render or export them. Workspace URLs are access capabilities. Read ui://reopenscad/workspace/{workspaceId} for an embedded 3D preview app with STL and 3MF downloads.";

#[derive(Clone)]
struct AppState {
    web_root: PathBuf,
    /// Admission control for everything that runs the evaluator: a bounded,
    /// per-client fair queue rather than a bare semaphore.
    render_queue: RenderQueue,
    /// Separate budget for `/events`, whose threads block for seconds.
    poll_limiter: JobLimiter,
    rate_limiter: RateLimiter,
    cancellations: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    workspaces: Store,
    public_origin: Option<String>,
}

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

#[derive(Clone)]
struct WorkspaceStore {
    root: PathBuf,
    inner: Arc<Mutex<StoreInner>>,
    nonce: Arc<AtomicU64>,
}

/// Disk is the system of record. RAM holds an id index (a few dozen bytes per
/// workspace) plus a bounded LRU of fully loaded workspaces, so total resident
/// source is capped at `MAX_CACHED_WORKSPACES * MAX_BODY` rather than growing
/// with the number of workspaces ever created.
struct StoreInner {
    /// Every workspace id present on disk, with its last-modified stamp.
    index: HashMap<String, WorkspaceMeta>,
    /// Fully loaded workspaces, at most `MAX_CACHED_WORKSPACES`.
    cache: HashMap<String, Workspace>,
    /// Cache keys, least-recently-used first.
    order: Vec<String>,
}

#[derive(Clone, Copy)]
struct WorkspaceMeta {
    updated_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Workspace {
    id: String,
    #[serde(default = "default_workspace_name")]
    name: String,
    code: String,
    revision: u64,
    created_at: u64,
    updated_at: u64,
    /// Viewer settings. Both carry `serde` defaults so workspace files written
    /// before these fields existed still deserialize instead of being treated
    /// as corrupt (which `load` would answer with a 404).
    ///
    /// Identifier of the selected 3D printer build plate; empty means "no
    /// plate", the default. The value is opaque to the server: the browser owns
    /// the bed catalogue, the workspace only remembers which entry was chosen.
    #[serde(default)]
    plate: String,
    /// Fabrication clearance used by the intersection checker, in millimetres.
    #[serde(default = "default_tolerance")]
    tolerance: f64,
    #[serde(default)]
    events: Vec<WorkspaceEvent>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceEvent {
    #[serde(default)]
    workspace_id: String,
    revision: u64,
    source: String,
    start: usize,
    end: usize,
    inserted_length: usize,
    timestamp: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceUpdate {
    code: Option<String>,
    name: Option<String>,
    start: Option<usize>,
    end: Option<usize>,
    text: Option<String>,
    expected_revision: Option<u64>,
    base_revision: Option<u64>,
    /// Viewer settings ride along on the same PATCH. Absent means "leave as is",
    /// so a plain code save never clears a plate the user picked.
    plate: Option<String>,
    tolerance: Option<f64>,
    #[serde(default = "default_edit_source")]
    source: String,
}

#[derive(Debug)]
struct EditResult {
    workspace: Workspace,
    event: WorkspaceEvent,
}

fn default_workspace_name() -> String {
    "Untitled model".into()
}

fn default_edit_source() -> String {
    "browser".into()
}

/// Matches the browser's `DEFAULT_TOLERANCE`: 0.2 mm of fabrication clearance.
const DEFAULT_TOLERANCE: f64 = 0.2;
/// Any clearance beyond this is a typo, not a tolerance.
const MAX_TOLERANCE: f64 = 1000.0;
/// Bounds the plate identifier so it can never become an unbounded string in
/// the persisted document.
const MAX_PLATE_ID: usize = 64;

fn default_tolerance() -> f64 {
    DEFAULT_TOLERANCE
}

/// Plate ids are catalogue keys chosen by the client, so the server only has to
/// keep them boring: lowercase slug characters, bounded length. Anything else
/// collapses to "no plate" rather than being rejected, because a bad viewer
/// preference must never fail a code save.
fn sanitize_plate(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() > MAX_PLATE_ID
        || !trimmed
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return String::new();
    }
    trimmed.to_ascii_lowercase()
}

/// Non-finite or negative clearances fall back to the default; the checker
/// treats 0 as "surfaces may touch", so 0 itself is a legitimate value.
fn sanitize_tolerance(value: f64) -> f64 {
    if !value.is_finite() || value < 0.0 || value > MAX_TOLERANCE {
        return DEFAULT_TOLERANCE;
    }
    value
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

impl WorkspaceStore {
    /// Scans the directory for workspace ids without reading file contents, so
    /// boot memory is O(workspace count) instead of O(total source bytes).
    fn open(root: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&root)?;
        #[cfg(unix)]
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        let mut index = HashMap::new();
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            if !valid_workspace_id(id) {
                continue;
            }
            let updated_at = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|delta| delta.as_millis().min(u128::from(u64::MAX)) as u64)
                .unwrap_or_else(unix_millis);
            index.insert(id.to_string(), WorkspaceMeta { updated_at });
        }
        let store = Self {
            root,
            inner: Arc::new(Mutex::new(StoreInner {
                index,
                cache: HashMap::new(),
                order: Vec::new(),
            })),
            nonce: Arc::new(AtomicU64::new(unix_millis())),
        };
        if let Ok(mut inner) = store.inner.lock() {
            store.prune(&mut inner, 0);
        }
        Ok(store)
    }

    /// Enforces the TTL and the workspace count cap. `headroom` reserves slots
    /// for workspaces about to be created.
    fn prune(&self, inner: &mut StoreInner, headroom: usize) {
        let now = unix_millis();
        let expired: Vec<String> = inner
            .index
            .iter()
            .filter(|(_, meta)| now.saturating_sub(meta.updated_at) > WORKSPACE_TTL_MS)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.forget(inner, &id);
        }
        let capacity = MAX_WORKSPACES.saturating_sub(headroom);
        if inner.index.len() <= capacity {
            return;
        }
        let mut ordered: Vec<(u64, String)> = inner
            .index
            .iter()
            .map(|(id, meta)| (meta.updated_at, id.clone()))
            .collect();
        ordered.sort_unstable();
        let excess = inner.index.len() - capacity;
        for (_, id) in ordered.into_iter().take(excess) {
            self.forget(inner, &id);
        }
    }

    /// Drops one workspace from disk and from every in-memory structure.
    fn forget(&self, inner: &mut StoreInner, id: &str) {
        let _ = fs::remove_file(self.root.join(format!("{id}.json")));
        inner.index.remove(id);
        inner.cache.remove(id);
        inner.order.retain(|key| key != id);
    }

    /// Records `workspace` in the LRU cache, evicting the coldest entries.
    fn cache_put(inner: &mut StoreInner, workspace: Workspace) {
        inner.order.retain(|key| key != &workspace.id);
        inner.order.push(workspace.id.clone());
        inner.cache.insert(workspace.id.clone(), workspace);
        while inner.order.len() > MAX_CACHED_WORKSPACES {
            let coldest = inner.order.remove(0);
            inner.cache.remove(&coldest);
        }
    }

    /// Returns a workspace from the cache, or faults it in from disk.
    fn load(&self, inner: &mut StoreInner, id: &str) -> Result<Option<Workspace>, String> {
        if !valid_workspace_id(id) || !inner.index.contains_key(id) {
            return Ok(None);
        }
        if let Some(workspace) = inner.cache.get(id).cloned() {
            inner.order.retain(|key| key != id);
            inner.order.push(id.to_string());
            return Ok(Some(workspace));
        }
        let bytes = match fs::read(self.root.join(format!("{id}.json"))) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                inner.index.remove(id);
                return Ok(None);
            }
            Err(error) => return Err(format!("Workspace could not be read: {error}")),
        };
        let workspace = match serde_json::from_slice::<Workspace>(&bytes) {
            Ok(workspace) if workspace.id == id => workspace,
            _ => {
                self.forget(inner, id);
                return Ok(None);
            }
        };
        Self::cache_put(inner, workspace.clone());
        Ok(Some(workspace))
    }

    /// Convenience wrapper the store tests drive directly; the request path
    /// reaches this through [`Store::create`].
    #[cfg(test)]
    fn create(&self, code: String, name: Option<String>) -> Result<Workspace, String> {
        self.create_with_settings(code, name, None, None)
    }

    /// `plate`/`tolerance` let a caller seed the viewer settings at creation
    /// time, which is how the browser carries the user's last plate choice into
    /// a brand new workspace.
    fn create_with_settings(
        &self,
        code: String,
        name: Option<String>,
        plate: Option<String>,
        tolerance: Option<f64>,
    ) -> Result<Workspace, String> {
        if code.len() > MAX_BODY {
            return Err("Workspace code exceeds the 2 MiB limit.".into());
        }
        let now = unix_millis();
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "Workspace store is unavailable.".to_string())?;
        self.prune(&mut inner, 1);
        let id = loop {
            let candidate = self.generate_id();
            if !inner.index.contains_key(&candidate) {
                break candidate;
            }
        };
        let workspace = Workspace {
            id: id.clone(),
            name: sanitize_name(name),
            code,
            revision: 1,
            created_at: now,
            updated_at: now,
            plate: plate.as_deref().map(sanitize_plate).unwrap_or_default(),
            tolerance: tolerance.map(sanitize_tolerance).unwrap_or(DEFAULT_TOLERANCE),
            events: Vec::new(),
        };
        self.persist(&workspace)?;
        inner.index.insert(id, WorkspaceMeta { updated_at: now });
        Self::cache_put(&mut inner, workspace.clone());
        Ok(workspace)
    }

    fn get(&self, id: &str) -> Result<Option<Workspace>, String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "Workspace store is unavailable.".to_string())?;
        self.load(&mut inner, id)
    }

    /// Workspace ids currently on disk. Used by the eviction tests.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().map(|inner| inner.index.len()).unwrap_or(0)
    }

    fn update(&self, id: &str, update: WorkspaceUpdate) -> Result<EditResult, UpdateError> {
        let expected = update
            .expected_revision
            .or(update.base_revision)
            .ok_or_else(|| {
                UpdateError::Invalid(
                    "baseRevision or expectedRevision is required for every update.".into(),
                )
            })?;
        let current = self
            .inner
            .lock()
            .map_err(|_| UpdateError::Internal("Workspace store is unavailable.".into()))
            .and_then(|mut inner| {
                self.load(&mut inner, id)
                    .map_err(UpdateError::Internal)
            })?
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
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| UpdateError::Internal("Workspace store is unavailable.".into()))?;
        let latest = self
            .load(&mut inner, id)
            .map_err(UpdateError::Internal)?
            .ok_or(UpdateError::NotFound)?;
        if latest.revision != expected {
            return Err(UpdateError::Conflict(latest));
        }
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
        let mut workspace = latest;
        workspace.code = code;
        if let Some(name) = update.name {
            workspace.name = sanitize_name(Some(name));
        }
        if let Some(plate) = update.plate.as_deref() {
            workspace.plate = sanitize_plate(plate);
        }
        if let Some(tolerance) = update.tolerance {
            workspace.tolerance = sanitize_tolerance(tolerance);
        }
        workspace.revision += 1;
        workspace.updated_at = now;
        workspace.events.push(event.clone());
        if workspace.events.len() > MAX_EVENTS {
            workspace
                .events
                .drain(..workspace.events.len() - MAX_EVENTS);
        }
        self.persist(&workspace).map_err(UpdateError::Internal)?;
        inner.index.insert(id.into(), WorkspaceMeta { updated_at: now });
        Self::cache_put(&mut inner, workspace.clone());
        Ok(EditResult { workspace, event })
    }

    /// Writes viewer settings without touching the source.
    ///
    /// Deliberately not part of `update`: a plate or tolerance change is not an
    /// edit to the document, so it must not bump the revision, emit an edit
    /// event, or require a `baseRevision`. Making it revisionless also means it
    /// can never collide with a concurrent code save from another tab.
    fn update_settings(
        &self,
        id: &str,
        plate: Option<&str>,
        tolerance: Option<f64>,
    ) -> Result<Workspace, UpdateError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| UpdateError::Internal("Workspace store is unavailable.".into()))?;
        let mut workspace = self
            .load(&mut inner, id)
            .map_err(UpdateError::Internal)?
            .ok_or(UpdateError::NotFound)?;
        if let Some(plate) = plate {
            workspace.plate = sanitize_plate(plate);
        }
        if let Some(tolerance) = tolerance {
            workspace.tolerance = sanitize_tolerance(tolerance);
        }
        let now = unix_millis();
        workspace.updated_at = now;
        self.persist(&workspace).map_err(UpdateError::Internal)?;
        inner.index.insert(id.into(), WorkspaceMeta { updated_at: now });
        Self::cache_put(&mut inner, workspace.clone());
        Ok(workspace)
    }

    fn persist(&self, workspace: &Workspace) -> Result<(), String> {
        let final_path = self.root.join(format!("{}.json", workspace.id));
        let temporary_path = self.root.join(format!(
            ".{}-{}.tmp",
            workspace.id,
            self.nonce.fetch_add(1, Ordering::Relaxed)
        ));
        let bytes = serde_json::to_vec_pretty(workspace).map_err(|error| error.to_string())?;
        let result = (|| -> io::Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temporary_path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary_path, &final_path)?;
            fs::File::open(&self.root)?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        result.map_err(|error| error.to_string())
    }

    fn generate_id(&self) -> String {
        generate_workspace_id(&self.nonce)
    }
}

/// Mints a workspace id. Free-standing rather than a method because both
/// storage backends need identical ids: a workspace URL is a capability, so
/// the entropy behind it must not depend on where the row happens to live.
fn generate_workspace_id(nonce: &AtomicU64) -> String {
    const ADJECTIVES: &[&str] = &[
        "bouncy", "cosmic", "dapper", "fuzzy", "jolly", "mighty", "nimble", "plucky", "quirky",
        "sunny", "wobbly", "zippy",
    ];
    const NOUNS: &[&str] = &[
        "badger", "capybara", "gecko", "llama", "otter", "penguin", "quokka", "raccoon", "walrus",
        "wombat", "yak", "zebra",
    ];
    let mut random = [0u8; 16];
    if fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut random))
        .is_err()
    {
        let seed = nonce.fetch_add(1, Ordering::Relaxed) ^ unix_millis();
        for (index, byte) in random.iter_mut().enumerate() {
            *byte = seed.rotate_left(index as u32).to_le_bytes()[index % 8];
        }
    }
    let adjective = ADJECTIVES[random[0] as usize % ADJECTIVES.len()];
    let noun = NOUNS[random[1] as usize % NOUNS.len()];
    let token: String = random[2..]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{adjective}-{noun}-{token}")
}

// ---------------------------------------------------------------------------
// Storage backend selection
// ---------------------------------------------------------------------------

/// The storage backend the request path talks to.
///
/// Local development keeps the filesystem store verbatim — it is the default,
/// it needs no service to be running, and every existing test drives it
/// directly. Setting `DATABASE_URL` swaps in the Postgres backend, which is
/// what a multi-instance deployment (Cloud Run) requires: an in-process
/// `Mutex` cannot arbitrate an optimistic-concurrency check between two
/// containers, but a `WHERE revision = $n` predicate can.
///
/// The variants share one method surface, so choosing a backend never reaches
/// the HTTP or MCP handlers.
#[derive(Clone)]
enum Store {
    Filesystem(WorkspaceStore),
    #[cfg(feature = "postgres")]
    Postgres(pgstore::PgWorkspaceStore),
}

impl Store {
    fn create(&self, code: String, name: Option<String>) -> Result<Workspace, String> {
        self.create_with_settings(code, name, None, None)
    }

    fn create_with_settings(
        &self,
        code: String,
        name: Option<String>,
        plate: Option<String>,
        tolerance: Option<f64>,
    ) -> Result<Workspace, String> {
        match self {
            Self::Filesystem(store) => store.create_with_settings(code, name, plate, tolerance),
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.create_with_settings(code, name, plate, tolerance),
        }
    }

    fn get(&self, id: &str) -> Result<Option<Workspace>, String> {
        match self {
            Self::Filesystem(store) => store.get(id),
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.get(id),
        }
    }

    /// Cheap "has anything changed?" probe for the `/events` long poll.
    ///
    /// The filesystem store answers from its resident index, so a full `get`
    /// costs it nothing extra. Postgres answers with a single `bigint` instead
    /// of streaming the document back 26 times per poll.
    fn revision_of(&self, id: &str) -> Result<Option<u64>, String> {
        match self {
            Self::Filesystem(store) => Ok(store.get(id)?.map(|workspace| workspace.revision)),
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.revision_of(id),
        }
    }

    fn update(&self, id: &str, update: WorkspaceUpdate) -> Result<EditResult, UpdateError> {
        match self {
            Self::Filesystem(store) => store.update(id, update),
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.update(id, update),
        }
    }

    fn update_settings(
        &self,
        id: &str,
        plate: Option<&str>,
        tolerance: Option<f64>,
    ) -> Result<Workspace, UpdateError> {
        match self {
            Self::Filesystem(store) => store.update_settings(id, plate, tolerance),
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.update_settings(id, plate, tolerance),
        }
    }
}

/// Picks the backend from the environment.
///
/// `DATABASE_URL` present means Postgres; absent means the filesystem store
/// rooted at `REOPENSCAD_DATA_DIR`. The error when `DATABASE_URL` is set but
/// the binary was built without the feature is deliberate and loud: silently
/// falling back to per-instance disk on Cloud Run would look like it worked
/// and then lose a workspace the moment the service scaled past one instance.
fn open_workspace_store(fallback_root: PathBuf) -> io::Result<Store> {
    match env::var("DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => open_postgres_store(url.trim()),
        _ => {
            println!("Workspace storage: filesystem ({})", fallback_root.display());
            WorkspaceStore::open(fallback_root).map(Store::Filesystem)
        }
    }
}

#[cfg(feature = "postgres")]
fn open_postgres_store(url: &str) -> io::Result<Store> {
    println!("Workspace storage: Postgres");
    pgstore::PgWorkspaceStore::open(url)
        .map(Store::Postgres)
        .map_err(io::Error::other)
}

#[cfg(not(feature = "postgres"))]
fn open_postgres_store(_url: &str) -> io::Result<Store> {
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "DATABASE_URL is set but this binary was built without the `postgres` feature. \
         Rebuild with `cargo build --release --features postgres`.",
    ))
}

#[derive(Debug)]
enum UpdateError {
    NotFound,
    Conflict(Workspace),
    Invalid(String),
    Validation(String),
    Internal(String),
}

fn sanitize_name(name: Option<String>) -> String {
    let name = name.unwrap_or_else(default_workspace_name);
    let trimmed = name.trim();
    if trimmed.is_empty() {
        default_workspace_name()
    } else {
        trimmed.chars().take(120).collect()
    }
}

fn normalize_source(source: &str) -> String {
    if source.eq_ignore_ascii_case("ai") || source.eq_ignore_ascii_case("mcp") {
        "ai".into()
    } else {
        "browser".into()
    }
}

fn valid_workspace_id(value: &str) -> bool {
    (16..=96).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn validate_source(source: &str) -> Result<(), String> {
    engine::validate(source)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

struct CompiledMesh {
    bytes: Vec<u8>,
    console: String,
    elapsed_ms: u128,
    triangles: usize,
}

struct JobLimiter {
    active: Arc<AtomicUsize>,
    maximum: usize,
}

struct JobPermit {
    active: Arc<AtomicUsize>,
}

struct CancellationRegistration {
    id: String,
    cancellations: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

impl Clone for JobLimiter {
    fn clone(&self) -> Self {
        Self {
            active: Arc::clone(&self.active),
            maximum: self.maximum,
        }
    }
}

impl JobLimiter {
    fn new(maximum: usize) -> Self {
        Self {
            active: Arc::new(AtomicUsize::new(0)),
            maximum,
        }
    }

    fn try_acquire(&self) -> Option<JobPermit> {
        let mut current = self.active.load(Ordering::Acquire);
        loop {
            if current >= self.maximum {
                return None;
            }
            match self.active.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(JobPermit {
                        active: Arc::clone(&self.active),
                    })
                }
                Err(updated) => current = updated,
            }
        }
    }
}

impl Drop for JobPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// The render queue.
//
// This is a multi-user service, so contention for the evaluator is the normal
// case, not the exceptional one. A bare semaphore answers the third concurrent
// caller with 429 — the caller did nothing wrong and has no way to know when
// to come back. A queue turns that into a wait, and three properties make the
// wait safe rather than merely deferred:
//
//   * it is bounded — in depth, per client, and in time — so it cannot become
//     the memory/latency hole it was introduced to close;
//   * it is fair — clients are served round-robin, so one caller with a
//     backlog delays everyone by one job, not by its whole backlog;
//   * waiting is free — the render deadline starts when the permit is taken
//     (see `RenderDeadline`), and a queued job can be cancelled out of the
//     queue without ever having run.
//
// Fairness key: the client IP, the same identity the per-IP rate limiter uses
// (`canonical_ip`, honouring `REOPENSCAD_TRUSTED_PROXY_HOPS`). Workspace id is
// the tempting alternative — it is what a "user" means in this app — but it is
// minted by the caller for free, so keying on it would let one client claim
// unlimited round-robin shares just by creating workspaces. IP is the only
// identity here that costs something to vary, and reusing it keeps one notion
// of "who is this" across rate limiting and scheduling. Two people behind one
// NAT share a share; that is the standard trade and it is the safe direction.
// ---------------------------------------------------------------------------

/// Fair-scheduling identity. Anonymous covers the (practically impossible)
/// case of a socket with no readable peer address; they share one share
/// between them rather than each getting a free one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum ClientKey {
    Ip(IpAddr),
    Anonymous,
}

// The client identity of the request this thread is serving.
//
// A thread-local rather than a parameter: the identity is established once in
// `serve_request`, and threading it through a dozen handler signatures (and
// the MCP tool dispatch beneath them) would touch far more code than it
// informs. `RESPONSE_BYTES` already works this way. Thread-per-connection
// makes it exactly request-scoped.
thread_local! {
    static CLIENT_KEY: std::cell::Cell<ClientKey> = const { std::cell::Cell::new(ClientKey::Anonymous) };
}

fn set_client_key(address: Option<IpAddr>) {
    let key = address.map_or(ClientKey::Anonymous, ClientKey::Ip);
    CLIENT_KEY.with(|current| current.set(key));
}

fn current_client_key() -> ClientKey {
    CLIENT_KEY.with(std::cell::Cell::get)
}

// ---------------------------------------------------------------------------
// Search-engine visibility.
//
// A workspace URL *is* the credential: there is no login, so anything that
// republishes the URL — a search result, a cached copy, an archive snapshot —
// hands out read/write access. Indexing is therefore deny-by-default and one
// route opts back in.
//
// Same thread-local shape as `CLIENT_KEY`, and for the same reason: every
// response in this server funnels through `respond_text`, which has no route
// context of its own. Defaulting to "not indexable" means a route added later
// is hidden until someone deliberately says otherwise, and a response written
// before `serve_request` has parsed the path (a malformed-request rejection)
// is hidden too.
// ---------------------------------------------------------------------------
const ROBOTS_TAG: &str = "noindex, nofollow, noarchive";

thread_local! {
    static INDEXABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn set_indexable(indexable: bool) {
    INDEXABLE.with(|current| current.set(indexable));
}

fn is_indexable() -> bool {
    INDEXABLE.with(std::cell::Cell::get)
}

/// Crawl policy. `Disallow` stops a compliant crawler from ever fetching a
/// workspace, its source, or its exports; `X-Robots-Tag` and the shell's
/// `<meta name="robots">` catch the ones that fetch anyway.
///
/// The landing page is left crawlable on purpose — it is the front door, it
/// names no workspace, and blanket-blocking the origin would make the project
/// unfindable to buy nothing.
const ROBOTS_TXT: &str = "\
# A workspace URL is its own credential: there is no login, so anyone holding\n\
# the link can read and change the work behind it. Workspace pages, the API\n\
# that serves their source, and the export routes must never be crawled,\n\
# cached or indexed. The landing page below is deliberately left open.\n\
User-agent: *\n\
Disallow: /workspaces/\n\
Disallow: /api/\n\
Disallow: /mcp\n\
";

/// The landing page is the front door and should be findable. Everything else
/// — the workspace shell, the JSON API, the STL/3MF/OBJ exports, `/mcp` — must
/// not be.
///
/// `?workspace=<id>` is the legacy way of naming a workspace, so `/` carrying
/// one is a workspace page wearing the landing page's path and is excluded
/// here as well.
fn route_is_indexable(request: &Request) -> bool {
    request.method == "GET"
        && matches!(request.path.as_str(), "/" | "/index.html")
        && !query_parameters(&request.query).contains_key("workspace")
}

/// Why a request could not be given a compute slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum QueueError {
    /// The queue is at `MAX_QUEUED_JOBS` across all clients.
    Full,
    /// This client already has `MAX_QUEUED_PER_CLIENT` jobs waiting.
    ClientLimit,
    /// Waited `MAX_QUEUE_WAIT` without reaching the front.
    TimedOut,
    /// The caller cancelled while still queued; nothing ever ran.
    Cancelled,
    /// The queue's own lock is poisoned — a bug, reported as a server fault.
    Unavailable,
}

impl QueueError {
    fn status(self) -> u16 {
        match self {
            QueueError::Cancelled => 409,
            QueueError::Unavailable => 500,
            _ => 429,
        }
    }

    /// Stable machine-readable discriminator, so a client never has to parse
    /// the prose to decide what happened.
    fn reason(self) -> &'static str {
        match self {
            QueueError::Full => "queue_full",
            QueueError::ClientLimit => "client_queue_limit",
            QueueError::TimedOut => "queue_wait_timeout",
            QueueError::Cancelled => "cancelled",
            QueueError::Unavailable => "queue_unavailable",
        }
    }

    /// Whether trying the identical request again can succeed. Every shedding
    /// outcome here is transient by construction; only an explicit cancel is
    /// the caller's own decision.
    fn retryable(self) -> bool {
        !matches!(self, QueueError::Cancelled)
    }
}

/// A refusal, together with the queue state that produced it.
///
/// The state matters because the audience includes AI clients. "Try again
/// later" is not actionable; "10 jobs are queued ahead of you, wait ~30 s and
/// call this tool again with the same arguments" is. Just as important is what
/// this says about the *input*: a shed request never reached the evaluator, so
/// nothing here is evidence that the caller's SCAD is wrong. Without saying so
/// explicitly an agent will "fix" perfectly good geometry in response to a
/// capacity error.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct QueueRejection {
    error: QueueError,
    running: usize,
    waiting: usize,
    capacity: usize,
    max_depth: usize,
    max_per_client: usize,
}

impl QueueRejection {
    fn status(self) -> u16 {
        self.error.status()
    }

    /// Seconds to wait before retrying.
    ///
    /// Derived from real state, not guessed: every job ahead needs a slot, and
    /// slots turn over `capacity` at a time. `QUEUE_DRAIN_ESTIMATE_SECS` is the
    /// order-of-magnitude cost of one exact-kernel job on this engine — the
    /// figure is deliberately coarse, and the caller is told it is an estimate.
    fn retry_after_seconds(self) -> u64 {
        match self.error {
            QueueError::Cancelled | QueueError::Unavailable => 0,
            QueueError::ClientLimit => QUEUE_DRAIN_ESTIMATE_SECS,
            // Rounds up: `waiting` jobs drain `capacity` at a time.
            _ => {
                let batches = self.waiting.div_ceil(self.capacity.max(1)).max(1) as u64;
                (batches * QUEUE_DRAIN_ESTIMATE_SECS).clamp(5, 120)
            }
        }
    }

    fn message(self) -> String {
        let retry = self.retry_after_seconds();
        match self.error {
            QueueError::Full => format!(
                "The render queue is full: {} job(s) compiling and {} waiting, against a limit of {} queued. \
                 This is a transient server capacity limit — your model was never compiled, so nothing \
                 about the SCAD code is wrong and it should not be changed. Wait about {retry} s and \
                 retry the same request unchanged.",
                self.running, self.waiting, self.max_depth
            ),
            QueueError::ClientLimit => format!(
                "This client already has {} renders queued, which is the per-client limit ({} of the \
                 {} queue places, so one caller can never lock the others out). Nothing is wrong with \
                 the SCAD code — it was never compiled. Wait for one of your renders to finish (or \
                 cancel one), then retry the same request; about {retry} s should be enough.",
                self.waiting.min(self.max_per_client),
                self.max_per_client,
                self.max_depth
            ),
            QueueError::TimedOut => format!(
                "Waited {} s for a free render slot and never reached the front of the queue ({} \
                 job(s) compiling, {} still waiting). The server is saturated; this says nothing \
                 about the SCAD code, which was never compiled. Wait about {retry} s and retry the \
                 same request unchanged.",
                MAX_QUEUE_WAIT.as_secs(),
                self.running,
                self.waiting
            ),
            QueueError::Cancelled => "Render cancelled.".to_string(),
            QueueError::Unavailable => {
                "The render queue is unavailable. This is a server fault, not a problem with the \
                 request."
                    .to_string()
            }
        }
    }

    /// The machine-readable half. Shipped alongside the prose everywhere: as
    /// the JSON body on the HTTP routes and as `structuredContent` on an MCP
    /// tool error, so a well-built client never parses English.
    fn detail(self) -> Value {
        json!({
            "ok": false,
            "error": self.message(),
            "reason": self.error.reason(),
            "retryable": self.error.retryable(),
            "retryAfterSeconds": self.retry_after_seconds(),
            // Named so it cannot be misread: the request never reached the
            // evaluator, so this is not a diagnostic about the caller's SCAD.
            "codeProblem": false,
            "queue": {
                "running": self.running,
                "waiting": self.waiting,
                "concurrency": self.capacity,
                "maxQueued": self.max_depth,
                "maxQueuedPerClient": self.max_per_client
            }
        })
    }
}

/// One job waiting for a slot.
struct QueueWaiter {
    ticket: u64,
    /// Present for render paths, which carry a cancellable `requestId`. It is
    /// what `GET /api/queue` looks the job up by.
    request_id: Option<String>,
    enqueued: Instant,
}

#[derive(Default)]
struct QueueState {
    /// Jobs holding a permit right now.
    active: usize,
    /// Round-robin cursor: every client with at least one waiting job appears
    /// exactly once, oldest-turn first.
    order: VecDeque<ClientKey>,
    /// Per-client FIFO. A client is in `order` if and only if it has a
    /// non-empty entry here.
    waiting: HashMap<ClientKey, VecDeque<QueueWaiter>>,
    /// Total waiters, kept alongside `waiting` so the depth check is O(1).
    depth: usize,
    /// `requestId`s currently executing, so the status route can distinguish
    /// "still queued" from "compiling".
    running: HashMap<String, Instant>,
    next_ticket: u64,
}

impl QueueState {
    /// The ticket that the next free permit belongs to.
    fn head(&self) -> Option<(ClientKey, u64)> {
        let client = *self.order.front()?;
        let ticket = self.waiting.get(&client)?.front()?.ticket;
        Some((client, ticket))
    }

    /// Removes the head ticket and rotates its client to the back of the
    /// round-robin — the one place fairness is actually enforced.
    fn take_head(&mut self) {
        let Some(client) = self.order.pop_front() else {
            return;
        };
        let mut drained = true;
        if let Some(queue) = self.waiting.get_mut(&client) {
            if queue.pop_front().is_some() {
                self.depth = self.depth.saturating_sub(1);
            }
            drained = queue.is_empty();
        }
        if drained {
            self.waiting.remove(&client);
        } else {
            self.order.push_back(client);
        }
    }

    /// Withdraws a waiter that gave up (cancelled, timed out, or disconnected)
    /// before it was ever scheduled.
    fn withdraw(&mut self, client: ClientKey, ticket: u64) {
        let Some(queue) = self.waiting.get_mut(&client) else {
            return;
        };
        if let Some(index) = queue.iter().position(|waiter| waiter.ticket == ticket) {
            queue.remove(index);
            self.depth = self.depth.saturating_sub(1);
        }
        if queue.is_empty() {
            self.waiting.remove(&client);
            if let Some(index) = self.order.iter().position(|key| *key == client) {
                self.order.remove(index);
            }
        }
    }

    /// How many jobs are scheduled ahead of `request_id`, and how long it has
    /// been waiting.
    ///
    /// Exact for this scheduler rather than a FIFO approximation: a ticket that
    /// is its client's `index`-th gets served in round `index`, so a client
    /// ahead of us in the rotation contributes at most `index + 1` jobs before
    /// ours and a client behind us at most `index`.
    fn position_of(&self, request_id: &str) -> Option<(usize, Duration)> {
        let (client, index, enqueued) = self.waiting.iter().find_map(|(client, queue)| {
            let index = queue
                .iter()
                .position(|waiter| waiter.request_id.as_deref() == Some(request_id))?;
            Some((*client, index, queue[index].enqueued))
        })?;
        let our_turn = self.order.iter().position(|key| *key == client)?;
        let mut ahead = index;
        for (turn, key) in self.order.iter().enumerate() {
            if *key == client {
                continue;
            }
            let pending = self.waiting.get(key).map_or(0, VecDeque::len);
            ahead += if turn < our_turn {
                pending.min(index + 1)
            } else {
                pending.min(index)
            };
        }
        Some((ahead, enqueued.elapsed()))
    }
}

struct QueueInner {
    state: Mutex<QueueState>,
    /// Signalled when a permit is released or a waiter withdraws.
    scheduled: Condvar,
    capacity: usize,
    max_depth: usize,
    max_per_client: usize,
    max_wait: Duration,
}

/// Bounded, per-client-fair admission control for the evaluator.
struct RenderQueue {
    inner: Arc<QueueInner>,
}

impl Clone for RenderQueue {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// Held for the duration of a compute job. Dropping it frees the slot and
/// wakes the next client in the rotation.
struct QueuePermit {
    inner: Arc<QueueInner>,
    request_id: Option<String>,
}

/// Only so `Result<QueuePermit, QueueRejection>::unwrap_err` is usable in
/// tests; the queue internals are deliberately not printed.
impl std::fmt::Debug for QueuePermit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QueuePermit")
            .field("requestId", &self.request_id)
            .finish()
    }
}

impl Drop for QueuePermit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.active = state.active.saturating_sub(1);
            if let Some(id) = &self.request_id {
                state.running.remove(id);
            }
        }
        self.inner.scheduled.notify_all();
    }
}

impl RenderQueue {
    fn new(
        capacity: usize,
        max_depth: usize,
        max_per_client: usize,
        max_wait: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(QueueInner {
                state: Mutex::new(QueueState::default()),
                scheduled: Condvar::new(),
                capacity: capacity.max(1),
                max_depth,
                max_per_client: max_per_client.max(1),
                max_wait,
            }),
        }
    }

    /// Reads `MAX_CONCURRENT_JOBS` / `MAX_QUEUED_JOBS` with environment
    /// overrides, so a deployment (or a test) can retune concurrency without a
    /// rebuild — and without touching the deadline constants the browser
    /// cross-checks against.
    fn from_environment() -> Self {
        fn tunable(name: &str, default: usize, ceiling: usize) -> usize {
            env::var(name)
                .ok()
                .and_then(|value| value.trim().parse::<usize>().ok())
                .filter(|value| *value >= 1 && *value <= ceiling)
                .unwrap_or(default)
        }
        let capacity = tunable("REOPENSCAD_MAX_CONCURRENT_JOBS", MAX_CONCURRENT_JOBS, 64);
        let depth = tunable("REOPENSCAD_MAX_QUEUE_DEPTH", MAX_QUEUED_JOBS, 128);
        let per_client = tunable(
            "REOPENSCAD_MAX_QUEUED_PER_CLIENT",
            MAX_QUEUED_PER_CLIENT,
            depth,
        );
        Self::new(capacity, depth, per_client, MAX_QUEUE_WAIT)
    }

    /// Waits for a compute slot.
    ///
    /// Returns as soon as one is free and this job is at the front of the
    /// rotation. `cancellation` is the same flag `POST /api/cancel` sets, so a
    /// job can be pulled out of the queue before it ever starts; it is polled
    /// rather than signalled because the cancel route must not need to know
    /// which queue, if any, its target is sitting in.
    fn acquire(
        &self,
        client: ClientKey,
        request_id: Option<&str>,
        cancellation: Option<&AtomicBool>,
    ) -> Result<QueuePermit, QueueRejection> {
        let cancelled = || cancellation.is_some_and(|flag| flag.load(Ordering::Acquire));
        let ticket;
        {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| self.rejection_blind(QueueError::Unavailable))?;
            // Uncontended: a free slot and nobody already waiting for one.
            // Skipping the queue here is not queue-jumping, it is the absence
            // of a queue to jump.
            if state.depth == 0 && state.active < self.inner.capacity {
                state.active += 1;
                if let Some(id) = request_id {
                    state.running.insert(id.to_string(), Instant::now());
                }
                return Ok(self.permit(request_id));
            }
            if state.depth >= self.inner.max_depth {
                return Err(self.rejection(&state, QueueError::Full));
            }
            if state
                .waiting
                .get(&client)
                .is_some_and(|queue| queue.len() >= self.inner.max_per_client)
            {
                return Err(self.rejection(&state, QueueError::ClientLimit));
            }
            ticket = state.next_ticket;
            state.next_ticket = state.next_ticket.wrapping_add(1);
            let queue = state.waiting.entry(client).or_default();
            queue.push_back(QueueWaiter {
                ticket,
                request_id: request_id.map(str::to_string),
                enqueued: Instant::now(),
            });
            let first = queue.len() == 1;
            state.depth += 1;
            if first {
                state.order.push_back(client);
            }
        }

        let give_up = Instant::now() + self.inner.max_wait;
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| self.rejection_blind(QueueError::Unavailable))?;
        loop {
            if cancelled() {
                state.withdraw(client, ticket);
                let rejection = self.rejection(&state, QueueError::Cancelled);
                drop(state);
                self.inner.scheduled.notify_all();
                return Err(rejection);
            }
            if state.active < self.inner.capacity && state.head() == Some((client, ticket)) {
                state.take_head();
                state.active += 1;
                if let Some(id) = request_id {
                    state.running.insert(id.to_string(), Instant::now());
                }
                return Ok(self.permit(request_id));
            }
            if Instant::now() >= give_up {
                state.withdraw(client, ticket);
                let rejection = self.rejection(&state, QueueError::TimedOut);
                drop(state);
                self.inner.scheduled.notify_all();
                return Err(rejection);
            }
            let (guard, _) = self
                .inner
                .scheduled
                .wait_timeout(state, QUEUE_POLL_INTERVAL)
                .map_err(|_| self.rejection_blind(QueueError::Unavailable))?;
            state = guard;
        }
    }

    fn permit(&self, request_id: Option<&str>) -> QueuePermit {
        QueuePermit {
            inner: Arc::clone(&self.inner),
            request_id: request_id.map(str::to_string),
        }
    }

    fn rejection(&self, state: &QueueState, error: QueueError) -> QueueRejection {
        QueueRejection {
            error,
            running: state.active,
            waiting: state.depth,
            capacity: self.inner.capacity,
            max_depth: self.inner.max_depth,
            max_per_client: self.inner.max_per_client,
        }
    }

    /// The rejection to report when the queue's own lock could not be taken,
    /// so no live counts are available.
    fn rejection_blind(&self, error: QueueError) -> QueueRejection {
        QueueRejection {
            error,
            running: 0,
            waiting: 0,
            capacity: self.inner.capacity,
            max_depth: self.inner.max_depth,
            max_per_client: self.inner.max_per_client,
        }
    }

    /// What `GET /api/queue?requestId=…` reports.
    fn status(&self, request_id: &str) -> Value {
        let Ok(state) = self.inner.state.lock() else {
            return self.rejection_blind(QueueError::Unavailable).detail();
        };
        let running = state.running.get(request_id).map(Instant::elapsed);
        let queued = state.position_of(request_id);
        let mut payload = json!({
            "ok": true,
            "requestId": request_id,
            "state": match (&queued, &running) {
                (Some(_), _) => "queued",
                (None, Some(_)) => "running",
                _ => "unknown",
            },
            // Load, for a caller deciding whether to wait at all.
            "running": state.active,
            "waiting": state.depth,
            "capacity": self.inner.capacity,
            "maxWaitMs": self.inner.max_wait.as_millis(),
        });
        if let Some((ahead, waited)) = queued {
            // 1-based: "you are 3rd in line" reads better than "2 ahead".
            payload["position"] = json!(ahead + 1);
            payload["waitedMs"] = json!(waited.as_millis());
        }
        if let Some(elapsed) = running {
            payload["runningMs"] = json!(elapsed.as_millis());
        }
        payload
    }
}

/// How much of the per-IP budget one request costs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RouteCost {
    /// Static files, health, workspace reads.
    Cheap,
    /// `/events` long polls: cheap per call but held open for seconds.
    Poll,
    /// Anything that runs the evaluator or serves a mesh.
    Evaluator,
}

impl RouteCost {
    fn heavy_tokens(self) -> f64 {
        match self {
            RouteCost::Cheap => 0.0,
            RouteCost::Poll => HEAVY_COST_POLL,
            RouteCost::Evaluator => HEAVY_COST_EVALUATOR,
        }
    }
}

/// Per-IP token buckets: request count, expensive-route count, and bytes
/// served. The byte bucket is what actually bounds amplification, since a
/// 78-byte render request can return megabytes.
#[derive(Clone)]
struct RateLimiter {
    buckets: Arc<Mutex<HashMap<IpAddr, IpBucket>>>,
}

struct IpBucket {
    requests: f64,
    heavy: f64,
    bytes: f64,
    updated: Instant,
}

impl IpBucket {
    fn new(now: Instant) -> Self {
        Self {
            requests: IP_REQUEST_BURST,
            heavy: IP_HEAVY_BURST,
            bytes: IP_BYTE_BURST,
            updated: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        if elapsed <= 0.0 {
            return;
        }
        self.updated = now;
        self.requests = (self.requests + elapsed * IP_REQUEST_REFILL_PER_SEC).min(IP_REQUEST_BURST);
        self.heavy = (self.heavy + elapsed * IP_HEAVY_REFILL_PER_SEC).min(IP_HEAVY_BURST);
        self.bytes = (self.bytes + elapsed * IP_BYTE_REFILL_PER_SEC).min(IP_BYTE_BURST);
    }
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            buckets: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Charges one request against `address`. Returns false when the caller
    /// should be answered with 429 instead of being dispatched.
    fn admit(&self, address: IpAddr, cost: RouteCost) -> bool {
        self.admit_at(address, cost, Instant::now())
    }

    fn admit_at(&self, address: IpAddr, cost: RouteCost, now: Instant) -> bool {
        let Ok(mut buckets) = self.buckets.lock() else {
            // A poisoned limiter must not become an open door.
            return false;
        };
        Self::make_room(&mut buckets, &address, now);
        let bucket = buckets
            .entry(address)
            .or_insert_with(|| IpBucket::new(now));
        bucket.refill(now);
        let heavy = cost.heavy_tokens();
        if bucket.requests < 1.0 || bucket.heavy < heavy || bucket.bytes <= 0.0 {
            return false;
        }
        bucket.requests -= 1.0;
        bucket.heavy -= heavy;
        true
    }

    /// Bills bytes already written to the socket. The bucket is allowed to go
    /// negative so an oversized response is paid for on the next request.
    fn charge_bytes(&self, address: IpAddr, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let Ok(mut buckets) = self.buckets.lock() else {
            return;
        };
        if let Some(bucket) = buckets.get_mut(&address) {
            bucket.refill(Instant::now());
            bucket.bytes -= bytes as f64;
        }
    }

    /// Keeps the bucket map bounded so the limiter cannot be turned into a
    /// memory-exhaustion vector by spraying source addresses.
    fn make_room(buckets: &mut HashMap<IpAddr, IpBucket>, wanted: &IpAddr, now: Instant) {
        if buckets.len() < MAX_RATE_BUCKETS || buckets.contains_key(wanted) {
            return;
        }
        buckets.retain(|_, bucket| now.saturating_duration_since(bucket.updated) < RATE_BUCKET_IDLE);
        while buckets.len() >= MAX_RATE_BUCKETS {
            let Some(coldest) = buckets
                .iter()
                .min_by_key(|(_, bucket)| bucket.updated)
                .map(|(address, _)| *address)
            else {
                break;
            };
            buckets.remove(&coldest);
        }
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.buckets.lock().map(|buckets| buckets.len()).unwrap_or(0)
    }
}

thread_local! {
    /// Bytes this connection's thread has written to its socket. `respond_text`
    /// is the single write path, so this is an exact tally.
    static RESPONSE_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn note_response_bytes(count: u64) {
    RESPONSE_BYTES.with(|total| total.set(total.get().saturating_add(count)));
}

fn take_response_bytes() -> u64 {
    RESPONSE_BYTES.with(|total| total.replace(0))
}

/// Classifies a route by how much work answering it costs the server.
fn route_cost(method: &str, path: &str) -> RouteCost {
    if path == "/mcp" {
        return RouteCost::Evaluator;
    }
    if path == "/api/render" || path == "/api/export" {
        return RouteCost::Evaluator;
    }
    // Queue status is an O(1) read under one lock, but it is polled for the
    // whole life of a render, so it is metered like the other poll.
    if path == "/api/queue" {
        return RouteCost::Poll;
    }
    if path == "/api/workspaces" && method == "POST" {
        return RouteCost::Evaluator;
    }
    if let Some((_, action)) = workspace_api_route(path) {
        return match (method, action) {
            (_, "events") => RouteCost::Poll,
            (_, "render") | (_, "export") | (_, "intersections") => RouteCost::Evaluator,
            ("PATCH", "") | ("PUT", "") | ("POST", "") => RouteCost::Evaluator,
            _ => RouteCost::Cheap,
        };
    }
    RouteCost::Cheap
}

impl Drop for CancellationRegistration {
    fn drop(&mut self) {
        if let Ok(mut cancellations) = self.cancellations.lock() {
            cancellations.remove(&self.id);
        }
    }
}

fn register_cancellation(
    state: &AppState,
    request_id: &str,
) -> Result<(Arc<AtomicBool>, CancellationRegistration), String> {
    let cancellation = Arc::new(AtomicBool::new(false));
    let mut cancellations = state
        .cancellations
        .lock()
        .map_err(|_| "Cancellation registry is unavailable.".to_string())?;
    if cancellations.contains_key(request_id) {
        return Err("requestId is already active.".into());
    }
    cancellations.insert(request_id.to_string(), Arc::clone(&cancellation));
    drop(cancellations);
    Ok((
        cancellation,
        CancellationRegistration {
            id: request_id.to_string(),
            cancellations: Arc::clone(&state.cancellations),
        },
    ))
}

// ---------------------------------------------------------------------------
// Render deadlines.
//
// Cancellation is cooperative and caller-driven: the engine stops only when
// somebody sets its flag. Nobody sets it when a caller walks away, and an
// adversarial or merely enormous model can otherwise hold a render slot for as
// long as it likes. Every compile therefore runs under a watchdog that trips
// the same flag on a wall-clock deadline, turning a silent hang into an error
// the caller can act on. The check itself lives where it already was — inside
// the engine's existing cancellation polls — so it costs nothing per sample.
// ---------------------------------------------------------------------------

/// Both limits are set against the heaviest model in the project's own test
/// corpus, `tests/fixtures/openappa/openappa-multipart.scad`: 772 lines that
/// expand to ~111k shape nodes. A deadline that rejected it would be a bug, so
/// each leaves several times that much headroom for a slower machine or a
/// legitimately larger model, while still bounding the runaway case to
/// something a caller can wait out and retry.
///
/// Rendering now goes through the exact polyhedral kernel, which computes real
/// booleans instead of sampling a distance field. That case takes 14-24 s
/// depending on machine load, against 6 s for the sampler it replaced, so the
/// preview limit is doubled to keep the same several-times headroom. Preview
/// and render produce the *same* mesh under that kernel — quality no longer
/// selects a resolution — so the two deadlines now bound the same work.
const PREVIEW_DEADLINE: Duration = Duration::from_secs(60);
/// Full-quality renders and exports are deliberate, one-off requests rather
/// than something the editor repeats as the model is typed, so they can afford
/// to wait considerably longer than a preview.
const RENDER_DEADLINE: Duration = Duration::from_secs(120);
/// How often the watchdog wakes to compare against its deadline.
const DEADLINE_POLL_INTERVAL: Duration = Duration::from_millis(25);

fn deadline_for(quality: engine::Quality) -> Duration {
    match quality {
        engine::Quality::Preview => PREVIEW_DEADLINE,
        engine::Quality::Render => RENDER_DEADLINE,
    }
}

/// Trips a render's cancellation flag once its deadline passes.
///
/// Dropping the guard retires the watchdog, so a render that finishes in time
/// leaves nothing behind.
struct RenderDeadline {
    expired: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    limit: Duration,
}

impl RenderDeadline {
    fn start(cancellation: &Arc<AtomicBool>, limit: Duration) -> Self {
        let expired = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let cancellation = Arc::clone(cancellation);
        let watched_expiry = Arc::clone(&expired);
        let watched_finish = Arc::clone(&finished);
        thread::spawn(move || {
            let expiry = Instant::now() + limit;
            while !watched_finish.load(Ordering::Acquire) {
                if Instant::now() >= expiry {
                    watched_expiry.store(true, Ordering::Release);
                    cancellation.store(true, Ordering::Release);
                    return;
                }
                thread::sleep(DEADLINE_POLL_INTERVAL);
            }
        });
        Self {
            expired,
            finished,
            limit,
        }
    }

    fn expired(&self) -> bool {
        self.expired.load(Ordering::Acquire)
    }

    /// Replace the engine's generic cancellation message with a timeout that
    /// says what happened and what to do about it. Any other failure, and a
    /// cancellation the caller actually asked for, passes through untouched.
    fn explain(&self, error: String) -> String {
        if self.expired() {
            format!(
                "Render exceeded its {} second time limit and was stopped. Simplify the model, \
                 reduce $fn, or render fewer objects at once.",
                self.limit.as_secs()
            )
        } else {
            error
        }
    }
}

impl Drop for RenderDeadline {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::Release);
    }
}

/// [`engine::compile`] under the deadline for its quality.
fn compile_guarded(
    source: &str,
    quality: engine::Quality,
) -> Result<Arc<engine::CompileOutput>, String> {
    let cancellation = Arc::new(AtomicBool::new(false));
    let deadline = RenderDeadline::start(&cancellation, deadline_for(quality));
    engine::compile_shared(source, quality, Some(&cancellation))
        .map_err(|error| deadline.explain(error.to_string()))
}

/// [`engine::compile_parts`] under the deadline for its quality.
fn compile_parts_guarded(
    source: &str,
    quality: engine::Quality,
) -> Result<Arc<engine::MultipartCompileOutput>, String> {
    let cancellation = Arc::new(AtomicBool::new(false));
    let deadline = RenderDeadline::start(&cancellation, deadline_for(quality));
    engine::compile_parts_shared(source, quality, Some(&cancellation))
        .map_err(|error| deadline.explain(error.to_string()))
}

fn main() -> io::Result<()> {
    let (host, port) = parse_args();
    // `CARGO_MANIFEST_DIR` is a *build-time* path. It happens to be right when
    // the binary runs from the source tree, and is always wrong inside a
    // container, where the sources were compiled in a builder stage that no
    // longer exists. The env overrides are what make the image work.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let web_root = env::var_os("REOPENSCAD_WEB_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            manifest_dir
                .parent()
                .expect("backend must live inside web/")
                .to_path_buf()
        });
    let workspace_root = env::var_os("REOPENSCAD_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| web_root.join(".reopenscad-workspaces"));
    let workspaces = open_workspace_store(workspace_root)?;
    let public_origin = env::var("REOPENSCAD_PUBLIC_ORIGIN")
        .ok()
        .map(|origin| validate_public_origin(&origin))
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if !matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1") && public_origin.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "REOPENSCAD_PUBLIC_ORIGIN is required when binding a non-loopback host.",
        ));
    }
    let state = Arc::new(AppState {
        web_root,
        render_queue: RenderQueue::from_environment(),
        poll_limiter: JobLimiter::new(MAX_CONCURRENT_POLLS),
        rate_limiter: RateLimiter::new(),
        cancellations: Arc::new(Mutex::new(HashMap::new())),
        workspaces,
        public_origin,
    });
    let listener = TcpListener::bind(format!("{host}:{port}"))?;

    println!("ReOpenSCAD running at http://{host}:{port}");
    println!("Geometry engine: native Rust parser + implicit CSG mesher");
    // Bounds live sockets/threads. `JobLimiter` is a CAS counter, so the accept
    // loop never spawns past the ceiling and never panics trying.
    let connections = JobLimiter::new(MAX_CONNECTIONS);
    install_shutdown_handler(Arc::clone(&connections.active));
    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                // A container that has been told to go away must stop taking
                // new work even if the platform keeps routing to it for a
                // moment; 503 tells the caller to retry, which the load
                // balancer will send to a healthy instance.
                if SHUTTING_DOWN.load(Ordering::Relaxed) {
                    shed_connection(stream);
                    continue;
                }
                let Some(permit) = connections.try_acquire() else {
                    shed_connection(stream);
                    continue;
                };
                let state = Arc::clone(&state);
                // `thread::Builder::spawn` returns an error where
                // `thread::spawn` panics. A panic here would unwind `main` and
                // take the whole server down, turning thread exhaustion into a
                // permanent outage instead of transient back-pressure.
                let spawned = thread::Builder::new()
                    .name("reopenscad-conn".into())
                    .spawn(move || {
                        // Releases the connection slot even if the handler panics.
                        let _permit = permit;
                        if let Err(error) = handle_connection(stream, &state) {
                            eprintln!("request failed: {error}");
                        }
                    });
                if let Err(error) = spawned {
                    // The closure (and with it the socket and the permit) is
                    // dropped, so the peer is disconnected and we keep serving.
                    eprintln!("worker thread unavailable, dropping connection: {error}");
                }
            }
            Err(error) => {
                eprintln!("connection failed: {error}");
                // A persistent accept failure (EMFILE/ENFILE) would otherwise
                // spin this loop at 100% CPU. Back off briefly.
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Graceful shutdown
// ---------------------------------------------------------------------------

/// Set from the SIGTERM handler; read by the accept loop and the drain thread.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// How long to let in-flight requests finish after SIGTERM.
///
/// Cloud Run allows 10 s between SIGTERM and SIGKILL by default, so the
/// default here stays inside that window: exiting on our own terms flushes
/// stdout and closes database connections, being SIGKILLed does neither.
const DEFAULT_SHUTDOWN_GRACE_MS: u64 = 9_000;

#[cfg(unix)]
const SIGTERM: i32 = 15;

/// `sighandler_t`, spelled as the function pointer it actually is rather than
/// as a `usize`, so the compiler checks the handler's signature for us.
#[cfg(unix)]
type SignalHandler = extern "C" fn(i32);

/// What `signal(3)` returns when it could not install the handler.
#[cfg(unix)]
const SIG_ERR: usize = usize::MAX;

#[cfg(unix)]
extern "C" {
    fn signal(signum: i32, handler: SignalHandler) -> usize;
}

/// Signal handlers may only touch async-signal-safe state, so this does one
/// relaxed atomic store and returns. All the real work happens on the drain
/// thread, which is an ordinary thread with no such restriction.
#[cfg(unix)]
extern "C" fn on_terminate(_signal: i32) {
    SHUTTING_DOWN.store(true, Ordering::Relaxed);
}

/// Installs the SIGTERM handler and the thread that acts on it.
///
/// The accept loop blocks in `accept()`, which a signal will not reliably
/// interrupt, so the exit is driven from here instead: wait for the flag, wait
/// for the connection count to reach zero (or for the grace window to close),
/// then exit cleanly. SIGINT is deliberately left alone so Ctrl-C during local
/// development still kills the server instantly.
fn install_shutdown_handler(active: Arc<AtomicUsize>) {
    #[cfg(unix)]
    {
        // SAFETY: `on_terminate` matches `SignalHandler`, and its body is one
        // relaxed atomic store, which is async-signal-safe.
        let installed = unsafe { signal(SIGTERM, on_terminate) };
        if installed == SIG_ERR {
            // Not fatal — the process still runs, it just dies abruptly when
            // the platform replaces it — but it must not be silent, because
            // the symptom would be dropped requests with no explanation.
            eprintln!("could not install the SIGTERM handler; shutdown will not drain");
            return;
        }
        let grace = env::var("REOPENSCAD_SHUTDOWN_GRACE_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_SHUTDOWN_GRACE_MS);
        let spawned = thread::Builder::new()
            .name("reopenscad-drain".into())
            .spawn(move || {
                while !SHUTTING_DOWN.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(100));
                }
                eprintln!("SIGTERM received: draining in-flight requests");
                let deadline = Instant::now() + Duration::from_millis(grace);
                loop {
                    let live = active.load(Ordering::Acquire);
                    if live == 0 {
                        break;
                    }
                    if Instant::now() >= deadline {
                        // A render can legitimately run for `RENDER_DEADLINE`
                        // (120 s), far beyond any platform's SIGTERM window, so
                        // this outcome is expected rather than exceptional.
                        eprintln!("shutdown grace elapsed with {live} request(s) in flight");
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                eprintln!("shutdown complete");
                std::process::exit(0);
            });
        if let Err(error) = spawned {
            eprintln!("shutdown watcher unavailable: {error}");
        }
    }
    #[cfg(not(unix))]
    {
        let _ = active;
    }
}

/// Rejects a connection accepted past `MAX_CONNECTIONS` without spawning a
/// thread for it, on the accept thread, with hard timeouts.
fn shed_connection(mut stream: TcpStream) {
    const BODY: &[u8] = br#"{"ok":false,"error":"The server is at capacity. Try again shortly."}"#;
    let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
    let _ = write!(
        stream,
        // The only response in the server that does not go through
        // `respond_text`, so it repeats that function's standing headers
        // rather than being the one route that quietly drops them.
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nRetry-After: 2\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Robots-Tag: noindex, nofollow, noarchive\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        BODY.len()
    );
    let _ = stream.write_all(BODY);
}

/// Resolves the bind address.
///
/// Cloud Run (and every other PaaS) hands the port over in `$PORT` and expects
/// the process to listen on all interfaces; nothing on the command line is
/// available to a container image. CLI flags still win, so nothing about the
/// local `cargo run` workflow changes.
fn parse_args() -> (String, u16) {
    let mut host = env::var("HOST")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| String::from("127.0.0.1"));
    let mut port = env::var("PORT")
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .filter(|value| *value != 0)
        .unwrap_or(5173);
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--host" => host = arguments.next().unwrap_or(host),
            "--port" => {
                port = arguments
                    .next()
                    .and_then(|value| value.trim().parse::<u16>().ok())
                    // Port 0 binds an ephemeral port, so the server would
                    // announce `:0` and be unreachable at the printed address.
                    .filter(|value| *value != 0)
                    .unwrap_or(port)
            }
            "-h" | "--help" => {
                println!("Usage: reopenscad-server [--host 127.0.0.1] [--port 5173]");
                std::process::exit(0);
            }
            _ => {}
        }
    }
    (host, port)
}

fn handle_connection(mut stream: TcpStream, state: &AppState) -> io::Result<()> {
    // Per-syscall timeouts keep a stalled peer from pinning this thread; the
    // deadlines inside `read_request` bound the request as a whole. Without a
    // write timeout a peer that stops reading holds the thread indefinitely.
    stream.set_read_timeout(Some(SOCKET_READ_TIMEOUT))?;
    stream.set_write_timeout(Some(SOCKET_WRITE_TIMEOUT))?;
    let mut peer = stream
        .peer_addr()
        .ok()
        .map(|address| canonical_ip(address.ip()));
    take_response_bytes();
    let result = serve_request(&mut stream, state, &mut peer);
    // Bill what was actually written so a tiny request that produced megabytes
    // drains the caller's budget. `respond_text` is the only write path.
    if let Some(address) = peer {
        state
            .rate_limiter
            .charge_bytes(address, take_response_bytes());
    }
    result
}

/// Reads and discards at most 64 KiB over at most 200 ms. Bounded in both
/// bytes and time so it can never be turned into a way to keep a thread busy.
fn drain_briefly(stream: &mut TcpStream) {
    const DRAIN_LIMIT: usize = 64 * 1024;
    let restore = stream.read_timeout().ok().flatten();
    if stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .is_err()
    {
        return;
    }
    let deadline = Instant::now() + Duration::from_millis(200);
    let mut scratch = [0u8; 8 * 1024];
    let mut drained = 0usize;
    while drained < DRAIN_LIMIT && Instant::now() < deadline {
        match stream.read(&mut scratch) {
            Ok(0) => break,
            Ok(read) => drained += read,
            Err(_) => break,
        }
    }
    let _ = stream.set_read_timeout(restore);
}

/// Number of reverse proxies in front of this process, counted from the
/// socket inwards, read once at startup from `REOPENSCAD_TRUSTED_PROXY_HOPS`.
///
/// Zero — the default — means "no proxy": `X-Forwarded-For` is ignored
/// entirely and rate limiting uses the socket address, exactly as before.
static TRUSTED_PROXY_HOPS: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Resolves the client IP for rate limiting when a proxy sits in front.
///
/// `X-Forwarded-For` is attacker-controlled *at the left-hand end*: a client
/// can send its own header and a compliant proxy appends to it rather than
/// replacing it. Trusting the leftmost entry is therefore a way to hand every
/// attacker an unlimited supply of rate-limit buckets, which is why the
/// unconfigured default here is to not look at the header at all.
///
/// What is not attacker-controlled is the *right-hand* end: each proxy the
/// request genuinely passed through appended one entry, so with a known number
/// of hops `N` the (`N`-th from the right) entry was written by a proxy we
/// trust and cannot be forged. That is what this reads, and it is why the hop
/// count has to be configured per deployment rather than guessed.
///
/// Cloud Run: the Google front end appends the caller's address, so a service
/// reached directly on its `*.run.app` URL is one hop. Put an external HTTPS
/// load balancer in front and it becomes two, because the balancer appends as
/// well. Getting the number wrong fails safe in one direction (too many hops
/// yields a proxy address, i.e. one shared bucket) and unsafe in the other, so
/// the deployment docs state the value for each topology.
fn forwarded_client_ip(request: &Request) -> Option<IpAddr> {
    client_ip_from_chain(&request.headers, trusted_proxy_hops())
}

fn client_ip_from_chain(headers: &[(String, String)], hops: usize) -> Option<IpAddr> {
    if hops == 0 {
        return None;
    }
    // Several `X-Forwarded-For` headers are semantically one comma-joined
    // list, so they have to be concatenated in order before counting.
    let chain: Vec<&str> = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("x-forwarded-for"))
        .flat_map(|(_, value)| value.split(','))
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect();
    let index = chain.len().checked_sub(hops)?;
    parse_forwarded_address(chain[index]).map(canonical_ip)
}

fn trusted_proxy_hops() -> usize {
    let cached = TRUSTED_PROXY_HOPS.load(Ordering::Relaxed);
    if cached != usize::MAX {
        return cached;
    }
    let configured = env::var("REOPENSCAD_TRUSTED_PROXY_HOPS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        // More than a handful of hops is a typo, not a topology.
        .filter(|hops| *hops <= 8)
        .unwrap_or(0);
    TRUSTED_PROXY_HOPS.store(configured, Ordering::Relaxed);
    configured
}

/// Accepts the shapes proxies actually emit: a bare address, a bracketed IPv6
/// literal, and either form with a port suffix.
fn parse_forwarded_address(entry: &str) -> Option<IpAddr> {
    if let Ok(address) = entry.parse::<IpAddr>() {
        return Some(address);
    }
    if let Some(rest) = entry.strip_prefix('[') {
        let (inner, _) = rest.split_once(']')?;
        return inner.parse().ok();
    }
    // `1.2.3.4:5678` — only meaningful for IPv4, since a bare IPv6 literal
    // contains colons of its own and was already handled above.
    entry.rsplit_once(':')?.0.parse().ok()
}

/// Collapses IPv4-mapped IPv6 addresses so one client cannot hold two buckets.
fn canonical_ip(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(value) => value.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(address),
        value => value,
    }
}

/// `peer` starts as the socket address and is rewritten in place once the
/// headers are parsed, so `handle_connection` bills the response bytes to the
/// same identity that was admitted. Behind a proxy the socket address is the
/// proxy's, which would put every client in the world into one bucket.
fn serve_request(
    stream: &mut TcpStream,
    state: &AppState,
    peer: &mut Option<IpAddr>,
) -> io::Result<()> {
    // Connection threads are reused, so clear the previous request's verdict
    // before this one can be answered — including when parsing fails below.
    set_indexable(false);
    let request = match read_request(stream) {
        Ok(request) => request,
        Err(error) => {
            // Closing on a socket that still has unread data sends an RST and
            // the peer never sees the status. Drain a strictly bounded amount
            // first so the rejection is actually delivered.
            drain_briefly(stream);
            respond_error(stream, error.status, &error.message)?;
            return Ok(());
        }
    };
    set_indexable(route_is_indexable(&request));
    if !request_is_allowed(&request, state) {
        return respond_json(
            stream,
            403,
            r#"{"ok":false,"error":"Only same-origin localhost requests are allowed."}"#,
        );
    }
    if matches!(request.method.as_str(), "POST" | "PATCH" | "PUT")
        && !request.body.is_empty()
        && !request.header("content-type").is_some_and(|value| {
            value.eq_ignore_ascii_case("application/json")
                || value.to_ascii_lowercase().starts_with("application/json;")
        })
    {
        return respond_json(
            stream,
            415,
            r#"{"ok":false,"error":"Content-Type must be application/json."}"#,
        );
    }
    if let Some(forwarded) = forwarded_client_ip(&request) {
        *peer = Some(forwarded);
    }
    // The render queue schedules by the same identity the rate limiter meters,
    // so this is set once, here, after the proxy chain has had its say.
    set_client_key(*peer);
    // Per-IP admission runs before dispatch so an abusive source never reaches
    // the evaluator, the workspace store, or the MCP surface.
    if let Some(address) = *peer {
        let cost = route_cost(&request.method, &request.path);
        if !state.rate_limiter.admit(address, cost) {
            return respond_rate_limited(stream, &request.path);
        }
    }
    if request.path == "/mcp" {
        return match request.method.as_str() {
            "POST" => handle_mcp(stream, state, &request),
            // The modern revision drops the GET stream and the DELETE session
            // teardown; both must answer 405 rather than 404, which a client
            // would read as "no MCP endpoint here".
            "GET" | "DELETE" => respond_json(
                stream,
                405,
                r#"{"ok":false,"error":"MCP uses JSON-RPC 2.0 over HTTP POST."}"#,
            ),
            _ => respond_text(stream, 404, "text/plain; charset=utf-8", b"Not found", &[]),
        };
    }
    if request.path == "/api/workspaces" && request.method == "POST" {
        return handle_create_workspace(stream, state, &request);
    }
    if let Some((id, action)) = workspace_api_route(&request.path) {
        return handle_workspace_route(stream, state, &request, id, action);
    }
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/api/health") => handle_health(stream),
        ("POST", "/api/render") => handle_render(stream, state, &request.body),
        ("POST", "/api/export") => handle_export(stream, state, &request.body),
        ("POST", "/api/cancel") => handle_cancel(stream, state, &request.body),
        ("GET", "/api/queue") => handle_queue_status(stream, state, &request.query),
        // Served by the binary rather than from the web root: the deployed
        // image copies only the shell files, and a robots policy that ships
        // separately from the server that needs it is a robots policy that
        // eventually goes missing.
        ("GET", "/robots.txt") => respond_text(
            stream,
            200,
            "text/plain; charset=utf-8",
            ROBOTS_TXT.as_bytes(),
            &[],
        ),
        ("GET", path) => handle_static(stream, state, path),
        _ => respond_text(stream, 404, "text/plain; charset=utf-8", b"Not found", &[]),
    }
}

/// A rejected request, carrying the status the peer should be told about.
#[derive(Debug)]
struct RequestError {
    status: u16,
    message: String,
}

impl RequestError {
    fn new(status: u16, message: &str) -> Self {
        Self {
            status,
            message: message.to_string(),
        }
    }
}

impl From<io::Error> for RequestError {
    fn from(error: io::Error) -> Self {
        Self {
            status: 400,
            message: error.to_string(),
        }
    }
}

/// Parses one request with hard caps on size and wall-clock time.
///
/// Every accumulator here is bounded *before* it grows: the request line, the
/// header block, the header count, and the body. The socket read timeout is
/// per-syscall, so it alone cannot stop a peer that dribbles one byte just
/// inside the timeout forever; the `Instant` deadlines below do.
fn read_request(stream: &TcpStream) -> Result<Request, RequestError> {
    let started = Instant::now();
    let header_deadline = started + HEADER_DEADLINE;
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    let read = reader
        .by_ref()
        .take(MAX_REQUEST_LINE as u64 + 1)
        .read_line(&mut request_line)?;
    if read == 0 {
        return Err(RequestError::new(400, "Empty request"));
    }
    if read > MAX_REQUEST_LINE || !request_line.ends_with('\n') {
        return Err(RequestError::new(414, "Request line exceeds the 8 KiB limit"));
    }
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| RequestError::new(400, "Missing method"))?
        .to_string();
    let raw_path = parts
        .next()
        .ok_or_else(|| RequestError::new(400, "Missing path"))?;
    let (path, query) = raw_path
        .split_once('?')
        .map_or((raw_path, ""), |(path, query)| (path, query));
    let path = path.to_string();
    let query = query.to_string();

    let mut content_length = 0usize;
    let mut headers = Vec::new();
    let mut header_bytes = 0usize;
    loop {
        if Instant::now() >= header_deadline {
            return Err(RequestError::new(408, "Timed out while reading headers"));
        }
        if headers.len() >= MAX_HEADERS {
            return Err(RequestError::new(431, "Too many request headers"));
        }
        let remaining = MAX_HEADER_BYTES.saturating_sub(header_bytes);
        let mut line = String::new();
        let read = reader
            .by_ref()
            .take(remaining as u64 + 1)
            .read_line(&mut line)?;
        if read == 0 {
            break;
        }
        header_bytes += read;
        if header_bytes > MAX_HEADER_BYTES || !line.ends_with('\n') {
            return Err(RequestError::new(
                431,
                "Request headers exceed the 16 KiB limit",
            ));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                content_length = value
                    .parse()
                    .map_err(|_| RequestError::new(400, "Invalid Content-Length"))?;
                if content_length > MAX_BODY {
                    return Err(RequestError::new(413, "Request exceeds the 2 MiB limit"));
                }
            }
            headers.push((name, value));
        }
    }
    if content_length > MAX_BODY {
        return Err(RequestError::new(413, "Request exceeds the 2 MiB limit"));
    }

    // Stream the body instead of pre-allocating `content_length`, so a claimed
    // length the peer never sends costs nothing.
    let body_deadline = Instant::now() + BODY_DEADLINE;
    let mut body = Vec::with_capacity(content_length.min(BODY_CHUNK));
    let mut chunk = [0u8; BODY_CHUNK];
    while body.len() < content_length {
        if Instant::now() >= body_deadline {
            return Err(RequestError::new(408, "Timed out while reading the body"));
        }
        let wanted = (content_length - body.len()).min(chunk.len());
        let read = reader.read(&mut chunk[..wanted])?;
        if read == 0 {
            return Err(RequestError::new(400, "Request body ended early"));
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Ok(Request {
        method,
        path,
        query,
        headers,
        body,
    })
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }
}

fn request_is_allowed(request: &Request, state: &AppState) -> bool {
    let host = request.header("host").unwrap_or_default();
    if host.is_empty()
        || host.len() > 255
        || host
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'\\'))
    {
        return false;
    }
    let allowed_authority = state
        .public_origin
        .as_deref()
        .and_then(origin_authority)
        .map(str::to_string);
    let host_allowed = match allowed_authority.as_deref() {
        Some(authority) => host == authority,
        None => local_authority(host),
    };
    if !host_allowed {
        return false;
    }
    let Some(origin) = request.header("origin") else {
        return true;
    };
    let expected = state.public_origin.as_deref().unwrap_or_else(|| {
        if origin.starts_with("https://") {
            "https://"
        } else {
            "http://"
        }
    });
    if state.public_origin.is_some() {
        origin.trim_end_matches('/') == expected
    } else {
        origin_authority(origin).is_some_and(|authority| authority == host)
    }
}

fn local_authority(authority: &str) -> bool {
    let hostname = if authority.starts_with('[') {
        authority
            .split_once(']')
            .map(|(host, _)| host.trim_start_matches('['))
            .unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    matches!(hostname, "127.0.0.1" | "localhost" | "::1")
}

fn origin_authority(origin: &str) -> Option<&str> {
    let authority = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))?;
    if authority.is_empty() || authority.contains(['/', '?', '#']) {
        None
    } else {
        Some(authority)
    }
}

fn validate_public_origin(origin: &str) -> Result<String, String> {
    let normalized = origin.trim_end_matches('/');
    origin_authority(normalized)
        .ok_or_else(|| {
            "REOPENSCAD_PUBLIC_ORIGIN must be an http(s) origin without a path.".to_string()
        })
        .map(|_| normalized.to_string())
}

fn workspace_api_route(path: &str) -> Option<(&str, &str)> {
    let remainder = path.strip_prefix("/api/workspaces/")?;
    let (id, action) = remainder.split_once('/').unwrap_or((remainder, ""));
    if valid_workspace_id(id) {
        Some((id, action.trim_end_matches('/')))
    } else {
        None
    }
}

fn parse_json_body(body: &[u8]) -> Result<Value, String> {
    if body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(body).map_err(|error| format!("Invalid JSON: {error}"))
}

fn handle_create_workspace(
    stream: &mut TcpStream,
    state: &AppState,
    request: &Request,
) -> io::Result<()> {
    // `validate_source` runs the full evaluator; without a permit this route
    // was an unmetered way to spend CPU that the MCP path already guards.
    let _permit = match state
        .render_queue
        .acquire(current_client_key(), None, None)
    {
        Ok(permit) => permit,
        Err(rejection) => return respond_queue_rejection(stream, rejection),
    };
    let input = match parse_json_body(&request.body) {
        Ok(value) => value,
        Err(error) => return respond_error(stream, 400, &error),
    };
    let code = input
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_SOURCE)
        .to_string();
    if let Err(error) = validate_source(&code) {
        return respond_validation_error(stream, error);
    }
    let name = input
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string);
    // The browser sends the plate and tolerance it is currently showing, so a
    // new workspace opens with the same bed the user was last working against.
    let plate = input
        .get("plate")
        .and_then(Value::as_str)
        .map(str::to_string);
    let tolerance = input.get("tolerance").and_then(Value::as_f64);
    match state
        .workspaces
        .create_with_settings(code, name, plate, tolerance)
    {
        Ok(workspace) => respond_workspace(stream, 201, state, request, &workspace, None),
        Err(error) => respond_error(stream, 500, &error),
    }
}

fn handle_workspace_route(
    stream: &mut TcpStream,
    state: &AppState,
    request: &Request,
    id: &str,
    action: &str,
) -> io::Result<()> {
    match (request.method.as_str(), action) {
        ("GET", "") => match state.workspaces.get(id) {
            Ok(Some(workspace)) => respond_workspace(stream, 200, state, request, &workspace, None),
            Ok(None) if allow_missing_workspace(request) => respond_value(
                stream,
                200,
                &json!({"ok": true, "missing": true, "workspaceId": id}),
            ),
            Ok(None) => respond_error(stream, 404, "Workspace not found."),
            Err(error) => respond_error(stream, 500, &error),
        },
        ("PATCH", "") => handle_update_workspace(stream, state, request, id),
        ("GET", "code") => handle_query_workspace_code(stream, state, request, id),
        ("GET", "events") => handle_workspace_events(stream, state, request, id),
        ("POST", "render") => handle_workspace_render(stream, state, request, id),
        ("GET", "export") | ("POST", "export") => {
            handle_workspace_export(stream, state, request, id)
        }
        ("POST", "intersections") => handle_workspace_intersections(stream, state, request, id),
        _ => respond_error(stream, 404, "Workspace endpoint not found."),
    }
}

fn allow_missing_workspace(request: &Request) -> bool {
    query_parameters(&request.query)
        .get("allowMissing")
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

fn handle_update_workspace(
    stream: &mut TcpStream,
    state: &AppState,
    request: &Request,
    id: &str,
) -> io::Result<()> {
    let update = match serde_json::from_slice::<WorkspaceUpdate>(&request.body) {
        Ok(update) => update,
        Err(error) => return respond_error(stream, 400, &format!("Invalid update: {error}")),
    };
    // A PATCH that carries only viewer settings is not an edit: it takes the
    // revisionless path so picking a build plate cannot conflict with, or
    // invalidate, an in-flight code save.
    //
    // It also runs BEFORE the evaluator permit is taken. This path never calls
    // `validate_source`, so charging it a permit would let a busy render queue
    // answer "pick a build plate" with a 429 — a failure the browser cannot
    // usefully retry and the user would never understand.
    if update.code.is_none()
        && update.start.is_none()
        && update.end.is_none()
        && update.text.is_none()
        && (update.plate.is_some() || update.tolerance.is_some())
    {
        return match state
            .workspaces
            .update_settings(id, update.plate.as_deref(), update.tolerance)
        {
            Ok(workspace) => respond_workspace(stream, 200, state, request, &workspace, None),
            Err(UpdateError::NotFound) => respond_error(stream, 404, "Workspace not found."),
            Err(UpdateError::Internal(error)) => respond_error(stream, 500, &error),
            Err(_) => respond_error(stream, 400, "Workspace settings could not be updated."),
        };
    }
    // `WorkspaceStore::update` runs the full evaluator via `validate_source`,
    // so it needs the same permit the MCP update path takes.
    let _permit = match state
        .render_queue
        .acquire(current_client_key(), None, None)
    {
        Ok(permit) => permit,
        Err(rejection) => return respond_queue_rejection(stream, rejection),
    };
    match state.workspaces.update(id, update) {
        Ok(result) => respond_workspace(
            stream,
            200,
            state,
            request,
            &result.workspace,
            Some(&result.event),
        ),
        Err(UpdateError::NotFound) => respond_error(stream, 404, "Workspace not found."),
        Err(UpdateError::Conflict(workspace)) => respond_value(
            stream,
            409,
            &json!({
                "ok": false,
                "error": "Workspace revision conflict.",
                "workspace": workspace_public(&workspace, state, request, None, None)
            }),
        ),
        Err(UpdateError::Invalid(error)) => respond_error(stream, 400, &error),
        Err(UpdateError::Validation(error)) => respond_validation_error(stream, error),
        Err(UpdateError::Internal(error)) => respond_error(stream, 500, &error),
    }
}

fn handle_query_workspace_code(
    stream: &mut TcpStream,
    state: &AppState,
    request: &Request,
    id: &str,
) -> io::Result<()> {
    let workspace = match state.workspaces.get(id) {
        Ok(Some(workspace)) => workspace,
        Ok(None) => return respond_error(stream, 404, "Workspace not found."),
        Err(error) => return respond_error(stream, 500, &error),
    };
    let parameters = query_parameters(&request.query);
    let start = query_usize(&parameters, "start").unwrap_or(0);
    let end = query_usize(&parameters, "end").unwrap_or(workspace.code.len());
    match workspace_code_slice(&workspace, start, end) {
        Ok(payload) => respond_value(stream, 200, &payload),
        Err(error) => respond_error(stream, 400, &error),
    }
}

/// The partial-read payload shared by `GET /api/workspaces/{id}/code` and the
/// `get_workspace_code` MCP tool, so the two can never disagree about which
/// byte ranges are legal or what a slice reply looks like.
fn workspace_code_slice(workspace: &Workspace, start: usize, end: usize) -> Result<Value, String> {
    if start > end
        || end > workspace.code.len()
        || !workspace.code.is_char_boundary(start)
        || !workspace.code.is_char_boundary(end)
    {
        return Err("Range must use valid UTF-8 byte offsets within the document.".into());
    }
    Ok(json!({
        "ok": true,
        "workspaceId": workspace.id,
        "revision": workspace.revision,
        "start": start,
        "end": end,
        "totalLength": workspace.code.len(),
        "code": &workspace.code[start..end]
    }))
}

fn handle_workspace_events(
    stream: &mut TcpStream,
    state: &AppState,
    request: &Request,
    id: &str,
) -> io::Result<()> {
    // Long polls park a connection thread for seconds at a time. Without a gate
    // a few dozen concurrent pollers starve every other route.
    let _permit = match state.poll_limiter.try_acquire() {
        Some(permit) => permit,
        None => {
            return respond_text(
                stream,
                503,
                "application/json; charset=utf-8",
                br#"{"ok":false,"error":"Too many workspace subscribers. Retry shortly."}"#,
                &[("Retry-After", "1")],
            )
        }
    };
    let parameters = query_parameters(&request.query);
    let since = query_u64(&parameters, "since").unwrap_or(0);
    // The wait loop asks only for the revision. With workspaces in Postgres a
    // full read per tick would stream the whole document (up to 2 MiB) out of
    // the database 26 times per poll per subscriber; the document is fetched
    // once, after the loop decides there is something to say.
    let mut settled = false;
    for attempt in 0..=25 {
        match state.workspaces.revision_of(id) {
            Ok(Some(revision)) => {
                if revision > since || attempt == 25 {
                    settled = true;
                    break;
                }
            }
            Ok(None) => return respond_error(stream, 404, "Workspace not found."),
            Err(error) => return respond_error(stream, 500, &error),
        }
        thread::sleep(Duration::from_millis(200));
    }
    debug_assert!(settled);
    let workspace = match state.workspaces.get(id) {
        // Deleted between the probe and the read: rare, but the poll must
        // answer 404 rather than panic on a missing document.
        Ok(Some(workspace)) => workspace,
        Ok(None) => return respond_error(stream, 404, "Workspace not found."),
        Err(error) => return respond_error(stream, 500, &error),
    };
    respond_value(stream, 200, &workspace_events_payload(id, &workspace, since))
}

/// Builds the `/events` payload.
///
/// The source is included only when something actually changed. The browser
/// applies the payload solely under `changed === true` (see
/// `beginWorkspacePolling` in web/app.js), so echoing the full workspace on
/// every idle poll was pure amplification.
fn workspace_events_payload(id: &str, workspace: &Workspace, since: u64) -> Value {
    let events: Vec<_> = workspace
        .events
        .iter()
        .filter(|event| event.revision > since)
        .cloned()
        .collect();
    let changed = workspace.revision > since;
    let mut payload = json!({
        "ok": true,
        "workspaceId": id,
        "changed": changed,
        "revision": workspace.revision,
        "objects": [],
        "updatedRanges": &events,
        // Viewer settings ride every poll, not just changed ones: they are revisionless, so
        // `changed` says nothing about them. Without these a plate picked in one tab (or by
        // an MCP agent) would never reach another tab already looking at the workspace.
        "plate": workspace.plate,
        "tolerance": workspace.tolerance
    });
    if changed {
        payload["code"] = json!(workspace.code);
    }
    payload
}

fn handle_workspace_render(
    stream: &mut TcpStream,
    state: &AppState,
    request: &Request,
    id: &str,
) -> io::Result<()> {
    let input = match parse_json_body(&request.body) {
        Ok(value) => value,
        Err(error) => return respond_error(stream, 400, &error),
    };
    let revision = match input.get("revision").and_then(Value::as_u64) {
        Some(revision) => revision,
        None => return respond_error(stream, 400, "revision is required for workspace render."),
    };
    let request_id = input
        .get("requestId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !valid_request_id(request_id) {
        return respond_error(stream, 400, "A valid requestId is required.");
    }
    let object_ids = match requested_object_ids(&input, &HashMap::new()) {
        Ok(object_ids) => object_ids,
        Err(error) => return respond_error(stream, 400, &error),
    };
    let workspace = match state.workspaces.get(id) {
        Ok(Some(workspace)) if workspace.revision == revision => workspace,
        Ok(Some(workspace)) => {
            return respond_value(
                stream,
                409,
                &json!({"ok": false, "error": "Workspace revision conflict.", "currentRevision": workspace.revision}),
            )
        }
        Ok(None) => return respond_error(stream, 404, "Workspace not found."),
        Err(error) => return respond_error(stream, 500, &error),
    };
    // Registration comes BEFORE admission, and that ordering is the feature:
    // it is what lets `POST /api/cancel` reach a job that is still queued. The
    // queue polls this same flag, so a cancelled job is withdrawn from the
    // queue and never compiles at all.
    let (cancellation, _registration) = match register_cancellation(state, request_id) {
        Ok(registration) => registration,
        Err(error) => return respond_error(stream, 409, &error),
    };
    let queued_at = Instant::now();
    let _permit = match state.render_queue.acquire(
        current_client_key(),
        Some(request_id),
        Some(&cancellation),
    ) {
        Ok(permit) => permit,
        Err(rejection) => return respond_queue_rejection(stream, rejection),
    };
    let queued_ms = queued_at.elapsed().as_millis();
    let quality = if input.get("mode").and_then(Value::as_str) == Some("render") {
        engine::Quality::Render
    } else {
        engine::Quality::Preview
    };
    // `started` and the watchdog both begin HERE, after the permit is in hand.
    // Queue time is not compute time: a job that waited 40 s for a slot still
    // gets its whole `deadline_for(quality)` budget to actually run in, which
    // is the difference between a queue and a slow rejection.
    let started = Instant::now();
    let deadline = RenderDeadline::start(&cancellation, deadline_for(quality));
    let compiled = match engine::compile_parts_shared(&workspace.code, quality, Some(&cancellation))
    {
        Ok(compiled) => compiled,
        Err(error) if deadline.expired() => {
            return respond_error(stream, 422, &deadline.explain(error.to_string()))
        }
        Err(error) if error.to_string().to_ascii_lowercase().contains("cancelled") => {
            return respond_error(stream, 409, "Render cancelled.")
        }
        Err(error) => return respond_error(stream, 422, &error.to_string()),
    };
    if multipart_binary_size(&compiled).is_none_or(|size| size > MAX_OUTPUT) {
        return respond_error(stream, 422, "Compiled multipart output exceeds the limit.");
    }
    let response_mesh = match selected_parts_mesh(
        &compiled,
        object_ids.as_deref(),
        quality,
        Some(&cancellation),
    ) {
        Ok(mesh) => mesh,
        Err(error) if deadline.expired() => {
            return respond_error(stream, 422, &deadline.explain(error))
        }
        Err(_error) if cancellation.load(Ordering::Acquire) => {
            return respond_error(stream, 409, "Render cancelled.")
        }
        Err(error) => return respond_error(stream, 422, &error),
    };
    if !multipart_response_fits(&compiled, &response_mesh) {
        return respond_error(
            stream,
            422,
            "Aggregate multipart mesh output exceeds the 64 MiB limit.",
        );
    }
    let objects: Vec<_> = compiled.parts.iter().map(part_public).collect();
    // `mesh` and `objects[].mesh` describe the same geometry whenever the
    // request took every object: the combined mesh is the union of the parts,
    // and `%` background geometry travels under its own key. Sending both
    // doubled the response — 1.7 MB of base64 each on a mid-sized model — and
    // made the server base64-encode geometry the client then discarded, since
    // the viewer draws the per-object meshes. So the combined copy is sent
    // only when it is not recoverable from `objects`: when a selection was
    // requested, or when some object arrived without a mesh of its own.
    let selection_requested = object_ids.is_some();
    let objects_carry_geometry = !compiled.parts.is_empty();
    let combined_mesh = (selection_requested || !objects_carry_geometry)
        .then(|| base64_encode(&response_mesh.binary_stl()));
    // `%` objects travel under their own key rather than in `objects`: the
    // viewer may draw them transparently, but they are not addressable by the
    // visibility/selection protocol and never appear in `mesh` or an export,
    // matching OpenSCAD's background modifier.
    let background_objects: Vec<_> = compiled.background_parts.iter().map(part_public).collect();
    respond_value(
        stream,
        200,
        &json!({
            "ok": true,
            "workspaceId": id,
            "revision": workspace.revision,
            "mesh": combined_mesh,
            "triangles": response_mesh.triangles.len(),
            "console": compiled.messages.join("\n"),
            // Compute time, deliberately exclusive of the wait — the same
            // interval the deadline measures. The wait is reported separately
            // so a caller can tell a slow model from a busy server.
            "elapsedMs": started.elapsed().as_millis(),
            "queuedMs": queued_ms,
            "mode": input.get("mode").and_then(Value::as_str).unwrap_or("preview"),
            "objects": objects,
            "backgroundObjects": background_objects
        }),
    )
}

fn multipart_binary_size(output: &engine::MultipartCompileOutput) -> Option<usize> {
    std::iter::once(&output.mesh)
        .chain(output.parts.iter().map(|part| &part.mesh))
        .chain(output.background_parts.iter().map(|part| &part.mesh))
        .try_fold(0usize, |total, mesh| {
            total.checked_add(84usize.checked_add(mesh.triangles.len().checked_mul(50)?)?)
        })
}

fn mesh_binary_size(mesh: &engine::Mesh) -> Option<usize> {
    84usize.checked_add(mesh.triangles.len().checked_mul(50)?)
}

fn base64_size(bytes: usize) -> Option<usize> {
    bytes.checked_add(2)?.checked_div(3)?.checked_mul(4)
}

fn multipart_response_fits(
    output: &engine::MultipartCompileOutput,
    response_mesh: &engine::Mesh,
) -> bool {
    let response_size = mesh_binary_size(response_mesh);
    let part_size = output
        .parts
        .iter()
        .chain(output.background_parts.iter())
        .try_fold(0usize, |total, part| {
            total.checked_add(mesh_binary_size(&part.mesh)?)
        });
    response_size
        .and_then(|response| part_size.and_then(|parts| response.checked_add(parts)))
        .and_then(base64_size)
        .is_some_and(|encoded| encoded <= MAX_OUTPUT.saturating_sub(MAX_BODY))
}

/// A compiled object without its mesh payload.
///
/// The MCP object listing and the workspace payload both need to name the
/// addressable parts, but neither should ship a base64 STL per part: an AI
/// client wants IDs, sizes and extents, and the browser gets meshes from the
/// render route instead.
fn part_summary_public(part: &engine::CompiledPart) -> Value {
    json!({
        "id": part.id,
        "name": part.name,
        "kind": "solid",
        "triangles": part.mesh.triangles.len(),
        "bounds": {
            "min": vec3_public(part.bounds.min),
            "max": vec3_public(part.bounds.max)
        },
        "sourceRange": part.source_range.map(|range| json!({"start": range.start, "end": range.end})),
        "visible": part.visible,
        "selectable": part.selectable,
        "color": part.color,
        "background": part.background
    })
}

fn part_public(part: &engine::CompiledPart) -> Value {
    json!({
        "id": part.id,
        "name": part.name,
        "triangles": part.mesh.triangles.len(),
        "mesh": base64_encode(&part.mesh.binary_stl()),
        "bounds": {
            "min": [part.bounds.min.x, part.bounds.min.y, part.bounds.min.z],
            "max": [part.bounds.max.x, part.bounds.max.y, part.bounds.max.z]
        },
        "sourceRange": part.source_range.map(|range| json!({"start": range.start, "end": range.end})),
        "visible": part.visible,
        "selectable": part.selectable,
        // `[r, g, b, a]` in 0..1 from `color()`, or null for the default
        // material. `background` marks a `%` object: draw it, never export it.
        "color": part.color,
        "background": part.background
    })
}

fn handle_workspace_intersections(
    stream: &mut TcpStream,
    state: &AppState,
    request: &Request,
    id: &str,
) -> io::Result<()> {
    let workspace = match state.workspaces.get(id) {
        Ok(Some(workspace)) => workspace,
        Ok(None) => return respond_error(stream, 404, "Workspace not found."),
        Err(error) => return respond_error(stream, 500, &error),
    };
    let input = match parse_json_body(&request.body) {
        Ok(input) => input,
        Err(error) => return respond_error(stream, 400, &error),
    };
    let tolerance = input
        .get("tolerance")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let _permit = match state
        .render_queue
        .acquire(current_client_key(), None, None)
    {
        Ok(permit) => permit,
        Err(rejection) => return respond_queue_rejection(stream, rejection),
    };
    let compiled = match compile_parts_guarded(&workspace.code, engine::Quality::Preview) {
        Ok(compiled) => compiled,
        Err(error) => return respond_error(stream, 422, &error),
    };
    if multipart_binary_size(&compiled).is_none_or(|size| size > MAX_OUTPUT) {
        return respond_error(stream, 422, "Compiled multipart output exceeds the limit.");
    }
    let hidden = match hidden_part_ids(&compiled.parts, &input) {
        Ok(hidden) => hidden,
        Err(error) => return respond_error(stream, 400, &error),
    };
    match clearance_report(id, &workspace, &compiled.parts, tolerance, &hidden) {
        Ok(payload) => respond_value(stream, 200, &payload),
        Err(error) => respond_error(stream, 400, &error),
    }
}

/// The clearance/intersection report shared by `POST
/// /api/workspaces/{id}/intersections` and the `check_intersections` MCP tool.
fn clearance_report(
    id: &str,
    workspace: &Workspace,
    parts: &[engine::CompiledPart],
    tolerance: f64,
    hidden: &[String],
) -> Result<Value, String> {
    let intersections = engine::check_part_clearances(parts, tolerance, hidden)
        .map_err(|error| error.to_string())?;
    Ok(json!({
        "ok": true,
        "supported": true,
        "workspaceId": id,
        "revision": workspace.revision,
        "tolerance": tolerance,
        "intersections": intersections
            .iter()
            .map(|intersection| intersection_public_located(intersection, parts))
            .collect::<Vec<_>>()
    }))
}

fn hidden_part_ids(parts: &[engine::CompiledPart], input: &Value) -> Result<Vec<String>, String> {
    if input.get("visibleObjectIds").is_some() && input.get("hiddenObjectIds").is_some() {
        return Err("Provide visibleObjectIds or hiddenObjectIds, not both.".into());
    }
    let available: HashSet<_> = parts.iter().map(|part| part.id.as_str()).collect();
    if let Some(value) = input.get("visibleObjectIds") {
        let values = value
            .as_array()
            .ok_or_else(|| "visibleObjectIds must be an array of strings.".to_string())?;
        let visible = values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| "visibleObjectIds must contain only strings.".to_string())
            })
            .collect::<Result<HashSet<_>, _>>()?;
        let unknown: Vec<_> = visible.difference(&available).copied().collect();
        if !unknown.is_empty() {
            return Err(format!("Unknown visibleObjectIds: {}.", unknown.join(", ")));
        }
        return Ok(parts
            .iter()
            .filter(|part| !visible.contains(&part.id.as_str()))
            .map(|part| part.id.clone())
            .collect());
    }
    let Some(value) = input.get("hiddenObjectIds") else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| "hiddenObjectIds must be an array of strings.".to_string())?;
    let hidden = values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "hiddenObjectIds must contain only strings.".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let unknown: Vec<_> = hidden
        .iter()
        .filter(|id| !available.contains(id.as_str()))
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "Unknown hiddenObjectIds: {}.",
            unknown
                .iter()
                .map(|id| id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(hidden)
}

fn vec3_public(point: engine::Vec3) -> Value {
    json!([point.x, point.y, point.z])
}

fn bounds_public(bounds: engine::PartBounds) -> Value {
    json!({"min": vec3_public(bounds.min), "max": vec3_public(bounds.max)})
}

/// `intersection_public` plus the SCAD spans of the two parts, so a caller can
/// jump from a clash straight to the statements that produced it.
fn intersection_public_located(
    intersection: &engine::PartIntersection,
    parts: &[engine::CompiledPart],
) -> Value {
    let span = |id: &str| {
        parts
            .iter()
            .find(|part| part.id == id)
            .and_then(|part| part.source_range)
            .map(|range| json!({"start": range.start, "end": range.end}))
    };
    let mut value = intersection_public(intersection);
    if let Some(object) = value.as_object_mut() {
        object.insert("aSourceRange".into(), json!(span(&intersection.a)));
        object.insert("bSourceRange".into(), json!(span(&intersection.b)));
    }
    value
}

fn intersection_public(intersection: &engine::PartIntersection) -> Value {
    json!({
        "a": intersection.a,
        "b": intersection.b,
        "intersects": intersection.intersects,
        "withinTolerance": intersection.within_tolerance,
        "clearance": intersection.clearance,
        "depth": intersection.depth,
        // Where the extremum sits. `location` is the deepest penetration point
        // for a clash and the closest-approach point for a clearance, so an AI
        // client can point at the geometry instead of only naming the pair.
        "location": intersection.location.map(vec3_public),
        "overlapBounds": intersection.overlap.map(bounds_public)
    })
}

fn handle_workspace_export(
    stream: &mut TcpStream,
    state: &AppState,
    request: &Request,
    id: &str,
) -> io::Result<()> {
    let input = match parse_json_body(&request.body) {
        Ok(input) => input,
        Err(error) => return respond_error(stream, 400, &error),
    };
    let parameters = query_parameters(&request.query);
    let revision = input
        .get("revision")
        .and_then(Value::as_u64)
        .or_else(|| query_u64(&parameters, "revision"));
    let Some(revision) = revision else {
        return respond_error(stream, 400, "revision is required for workspace export.");
    };
    let workspace = match state.workspaces.get(id) {
        Ok(Some(workspace)) if workspace.revision == revision => workspace,
        Ok(Some(workspace)) => {
            return respond_value(
                stream,
                409,
                &json!({"ok": false, "error": "Workspace revision conflict.", "currentRevision": workspace.revision}),
            )
        }
        Ok(None) => return respond_error(stream, 404, "Workspace not found."),
        Err(error) => return respond_error(stream, 500, &error),
    };
    let object_ids = match requested_object_ids(&input, &parameters) {
        Ok(object_ids) => object_ids,
        Err(error) => return respond_error(stream, 400, &error),
    };
    let _permit = match state
        .render_queue
        .acquire(current_client_key(), None, None)
    {
        Ok(permit) => permit,
        Err(rejection) => return respond_queue_rejection(stream, rejection),
    };
    let format = input
        .get("format")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| parameters.get("format").cloned())
        .unwrap_or_else(|| "stl".into());
    if !is_supported_export_format(format.as_str()) {
        return respond_error(stream, 400, &supported_export_formats_sentence());
    }
    let selected_mesh = match export_mesh(&workspace.code, object_ids.as_deref()) {
        Ok(mesh) => mesh,
        Err(error) => return respond_error(stream, 422, &error),
    };
    let data = encode_mesh(&selected_mesh, format.as_str(), &workspace.name);
    if data.len() > MAX_OUTPUT {
        return respond_error(stream, 422, "Export exceeds the 64 MiB limit.");
    }
    let content_type = export_content_type(format.as_str());
    let disposition = format!("attachment; filename=\"model.{format}\"");
    respond_text(
        stream,
        200,
        content_type,
        &data,
        &[("Content-Disposition", &disposition)],
    )
}

/// The formats every export surface offers: the workspace export route, the
/// stateless `/api/export` route and the MCP `render_workspace` tool. Keeping
/// one list means a new format can never be wired into just one of them.
const EXPORT_FORMATS: [&str; 4] = ["stl", "off", "obj", "3mf"];

fn is_supported_export_format(format: &str) -> bool {
    EXPORT_FORMATS.contains(&format)
}

fn supported_export_formats_sentence() -> String {
    format!("Supported export formats are {}.", EXPORT_FORMATS.join(", "))
}

/// Serialize a mesh in one of [`EXPORT_FORMATS`].
///
/// `title` only reaches formats that carry metadata (3MF); the mesh formats
/// ignore it. Callers must validate the format first.
fn encode_mesh(mesh: &engine::Mesh, format: &str, title: &str) -> Vec<u8> {
    match format {
        "stl" => mesh.binary_stl(),
        "off" => mesh.off(),
        "obj" => mesh.obj(),
        "3mf" => threemf::write(mesh, title),
        _ => unreachable!("unvalidated export format {format:?}"),
    }
}

fn export_content_type(format: &str) -> &'static str {
    match format {
        "stl" => "model/stl",
        "obj" => "model/obj",
        "3mf" => "model/3mf",
        _ => "application/octet-stream",
    }
}

// Both export endpoints share this: no object IDs means the whole model, and an ID filter is
// only meaningful against parts compiled at export quality.
fn export_mesh(source: &str, object_ids: Option<&[String]>) -> Result<engine::Mesh, String> {
    let Some(object_ids) = object_ids else {
        return compile_guarded(source, engine::Quality::Render).map(|output| output.mesh.clone());
    };
    let output = compile_parts_guarded(source, engine::Quality::Render)?;
    selected_parts_mesh(&output, Some(object_ids), engine::Quality::Render, None)
}

fn selected_parts_mesh(
    output: &engine::MultipartCompileOutput,
    object_ids: Option<&[String]>,
    quality: engine::Quality,
    cancellation: Option<&AtomicBool>,
) -> Result<engine::Mesh, String> {
    let Some(object_ids) = object_ids else {
        return Ok(output.mesh.clone());
    };
    engine::mesh_selected_parts(&output.parts, object_ids, quality, cancellation)
        .map_err(|error| error.to_string())
}

fn requested_object_ids(
    input: &Value,
    parameters: &HashMap<String, String>,
) -> Result<Option<Vec<String>>, String> {
    if let Some(value) = input.get("objectIds") {
        let values = value
            .as_array()
            .ok_or_else(|| "objectIds must be an array of strings.".to_string())?;
        return values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "objectIds must contain only strings.".to_string())
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some);
    }
    Ok(parameters.get("objectIds").map(|value| {
        value
            .split(',')
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect()
    }))
}

/// Serialize a workspace for the wire.
///
/// `objects` is supplied only by callers that already paid for a compile. The
/// browser leaves it `None` and picks objects up from the render response,
/// which carries the per-part meshes it needs; the MCP write paths pass the
/// summaries so an agent learns the addressable part IDs in the same reply
/// that created or changed the code.
fn workspace_public(
    workspace: &Workspace,
    state: &AppState,
    request: &Request,
    event: Option<&WorkspaceEvent>,
    objects: Option<Vec<Value>>,
) -> Value {
    json!({
        "id": workspace.id,
        "name": workspace.name,
        "code": workspace.code,
        "revision": workspace.revision,
        "createdAt": workspace.created_at,
        "updatedAt": workspace.updated_at,
        "plate": workspace.plate,
        "tolerance": workspace.tolerance,
        "objects": objects.unwrap_or_default(),
        "updatedRanges": event.into_iter().collect::<Vec<_>>(),
        "url": workspace_url(state, request, &workspace.id)
    })
}

fn respond_workspace(
    stream: &mut TcpStream,
    status: u16,
    state: &AppState,
    request: &Request,
    workspace: &Workspace,
    event: Option<&WorkspaceEvent>,
) -> io::Result<()> {
    respond_value(
        stream,
        status,
        &json!({
            "ok": true,
            "url": workspace_url(state, request, &workspace.id),
            "workspace": workspace_public(workspace, state, request, event, None)
        }),
    )
}

fn workspace_url(state: &AppState, request: &Request, id: &str) -> String {
    format!("{}/workspaces/{id}", request_base_url(state, request))
}

fn request_base_url(state: &AppState, request: &Request) -> String {
    if let Some(origin) = &state.public_origin {
        return origin.clone();
    }
    if let Some(origin) = request.header("origin") {
        return origin.trim_end_matches('/').to_string();
    }
    let scheme = request
        .header("x-forwarded-proto")
        .filter(|scheme| matches!(*scheme, "http" | "https"))
        .unwrap_or("http");
    let host = request.header("host").unwrap_or("127.0.0.1:5173");
    format!("{scheme}://{host}")
}

fn query_parameters(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            if name.is_empty() {
                None
            } else {
                Some((
                    percent_decode(name),
                    percent_decode(&value.replace('+', " ")),
                ))
            }
        })
        .collect()
}

fn query_usize(parameters: &HashMap<String, String>, name: &str) -> Option<usize> {
    parameters.get(name)?.parse().ok()
}

fn query_u64(parameters: &HashMap<String, String>, name: &str) -> Option<u64> {
    parameters.get(name)?.parse().ok()
}

fn respond_validation_error(stream: &mut TcpStream, error: String) -> io::Result<()> {
    respond_value(
        stream,
        422,
        &json!({
            "ok": false,
            "valid": false,
            "error": error,
            "diagnostics": [{"severity": "error", "message": error}]
        }),
    )
}

fn respond_error(stream: &mut TcpStream, status: u16, error: &str) -> io::Result<()> {
    respond_value(stream, status, &json!({"ok": false, "error": error}))
}

/// Seconds a rate-limited caller should wait. The heavy bucket refills at
/// `IP_HEAVY_REFILL_PER_SEC`, so two seconds buys back more than one
/// evaluator-class request.
const RATE_LIMIT_RETRY_SECONDS: u64 = 2;

/// Answers a request the per-IP rate limiter shed.
///
/// Same principle as the queue rejections: a transient capacity condition must
/// never read like "your input was bad". On `/mcp` that means a JSON-RPC error
/// whose `data` carries the retry contract in fields, since the body has not
/// been parsed yet and the request id is not known — parsing a 2 MiB body just
/// to shed it would defeat the point of shedding.
fn respond_rate_limited(stream: &mut TcpStream, path: &str) -> io::Result<()> {
    const PROSE: &str = "Rate limit exceeded for this client. This is a transient server-side \
                         throttle: the request was never evaluated, so nothing about its \
                         arguments or SCAD code is wrong. Wait 2 s and retry the same request \
                         unchanged.";
    let body = if path == "/mcp" {
        json!({
            "jsonrpc": "2.0",
            "id": Value::Null,
            "error": {
                "code": -32000,
                "message": PROSE,
                "data": {
                    "reason": "rate_limited",
                    "retryable": true,
                    "retryAfterSeconds": RATE_LIMIT_RETRY_SECONDS,
                    "codeProblem": false
                }
            }
        })
    } else {
        json!({
            "ok": false,
            "error": PROSE,
            "reason": "rate_limited",
            "retryable": true,
            "retryAfterSeconds": RATE_LIMIT_RETRY_SECONDS,
            "codeProblem": false
        })
    };
    let encoded = serde_json::to_vec(&body).unwrap_or_default();
    respond_text(
        stream,
        429,
        "application/json; charset=utf-8",
        &encoded,
        &[("Retry-After", "2")],
    )
}

/// Answers a request the render queue would not admit.
///
/// Carries the machine-readable detail in the body *and* `Retry-After` in the
/// headers, because the two audiences read different things: a browser's fetch
/// wrapper looks at the header, an agent looks at the JSON, and a person looks
/// at the prose. All three say the same thing — this is capacity, it is
/// transient, and the request itself was fine.
fn respond_queue_rejection(stream: &mut TcpStream, rejection: QueueRejection) -> io::Result<()> {
    let body = match serde_json::to_vec(&rejection.detail()) {
        Ok(bytes) => bytes,
        Err(error) => return respond_error(stream, 500, &format!("JSON encoding failed: {error}")),
    };
    let retry = rejection.retry_after_seconds().to_string();
    let headers: Vec<(&str, &str)> = if rejection.retry_after_seconds() > 0 {
        vec![("Retry-After", retry.as_str())]
    } else {
        Vec::new()
    };
    respond_text(
        stream,
        rejection.status(),
        "application/json; charset=utf-8",
        &body,
        &headers,
    )
}

fn respond_value(stream: &mut TcpStream, status: u16, value: &Value) -> io::Result<()> {
    match serde_json::to_vec(value) {
        Ok(bytes) => respond_text(
            stream,
            status,
            "application/json; charset=utf-8",
            &bytes,
            &[],
        ),
        Err(error) => respond_error(stream, 500, &format!("JSON encoding failed: {error}")),
    }
}

fn handle_mcp(stream: &mut TcpStream, state: &AppState, request: &Request) -> io::Result<()> {
    let message = match parse_json_body(&request.body) {
        Ok(Value::Object(message)) => message,
        Ok(_) => return mcp_protocol_error(stream, Value::Null, -32600, "Invalid Request"),
        Err(error) => return mcp_protocol_error(stream, Value::Null, -32700, &error),
    };
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return mcp_protocol_error(stream, id, -32600, "Invalid Request");
    }
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return mcp_protocol_error(stream, id, -32600, "Invalid Request");
    };
    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
    if !params.is_object() {
        return mcp_protocol_error(stream, id, -32602, "MCP params must be an object.");
    }
    if mcp_era(method, &params) == McpEra::Modern {
        return handle_modern_mcp(stream, state, request, id, method, &params);
    }
    if let Some(version) = unsupported_legacy_header(method, request.header("mcp-protocol-version"))
    {
        return mcp_error_response(
            stream,
            400,
            id,
            // Legacy vocabulary deliberately: -32022 is a *modern* error code,
            // and the compatibility matrix has clients read one as proof the
            // server is modern. This shape mirrors the lifecycle spec's own
            // example error instead.
            -32602,
            "Unsupported protocol version",
            Some(json!({"supported": SUPPORTED_MCP_PROTOCOLS, "requested": version})),
        );
    }
    let result = match method {
        "initialize" => {
            // An *unknown* version negotiates; a *missing or non-string* one is
            // malformed. The client MUST send a protocol version it supports,
            // so there is nothing to negotiate against and nothing to guess —
            // silently answering as if it had said "2025-03-26" would hand back
            // a version it never asked for. -32602 is the JSON-RPC code for
            // exactly this, and it is the one case where refusing is correct.
            let Some(requested) = params.get("protocolVersion").and_then(Value::as_str) else {
                return mcp_protocol_error(
                    stream,
                    id,
                    -32602,
                    "initialize requires a string params.protocolVersion.",
                );
            };
            let protocol_version = negotiated_mcp_protocol(requested);
            json!({
                "protocolVersion": protocol_version,
                "capabilities": {
                    "tools": {"listChanged": false},
                    "resources": {"listChanged": false, "subscribe": false}
                },
                "serverInfo": {"name": "reopenscad", "version": env!("CARGO_PKG_VERSION")},
                "instructions": MCP_INSTRUCTIONS,
                // Legacy `initialize` reports only the single version it
                // settled on, which leaves a UI with no way to say what else
                // is on offer without hardcoding a list that will drift. Every
                // result may carry `_meta` under a namespaced key, so the full
                // roster rides along there for the connection panel to read.
                "_meta": {MCP_META_SUPPORTED_VERSIONS: mcp_supported_versions()}
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": mcp_tool_definitions()}),
        "resources/list" => json!({"resources": mcp_resource_definitions()}),
        "resources/templates/list" => json!({"resourceTemplates": mcp_resource_templates()}),
        "resources/read" => {
            let Some(uri) = params.get("uri").and_then(Value::as_str) else {
                return mcp_protocol_error(stream, id, -32602, "Resource uri is required.");
            };
            match mcp_read_resource(state, request, uri) {
                Ok(contents) => json!({"contents": [contents]}),
                // -32002 is MCP's "resource not found"; anything the caller can
                // fix by picking another URI reports through it.
                Err(error) => return mcp_protocol_error(stream, id, -32002, &error),
            }
        }
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return mcp_protocol_error(stream, id, -32602, "Tool name is required.");
            };
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if !arguments.is_object() {
                return mcp_protocol_error(stream, id, -32602, "Tool arguments must be an object.");
            }
            return respond_value(
                stream,
                200,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": mcp_call_tool(state, request, name, arguments)
                }),
            );
        }
        "notifications/initialized" | "notifications/cancelled" => {
            if id.is_null() {
                return respond_text(stream, 202, "application/json", b"", &[]);
            }
            json!({})
        }
        _ if id.is_null() => return respond_text(stream, 202, "application/json", b"", &[]),
        _ => return mcp_protocol_error(stream, id, -32601, "Method not found"),
    };
    if id.is_null() {
        respond_text(stream, 202, "application/json", b"", &[])
    } else {
        respond_value(
            stream,
            200,
            &json!({"jsonrpc": "2.0", "id": id, "result": result}),
        )
    }
}

/// Legacy-era (`initialize`-based) protocol negotiation.
///
/// The lifecycle spec asks a server to *negotiate*, not to reject: if it cannot
/// speak the revision the client named, it answers with one it can and lets the
/// client decide whether to continue. Rejecting instead bounced every client
/// tracking a newer spec revision — Claude Code included, which offers a
/// post-2025-06-18 version and, having no fall-forward mechanism, simply failed
/// to connect. Modern (2026-07-28) clients reach this path too: they probe, see
/// no modern error, and fall back to `initialize`.
fn negotiated_mcp_protocol(requested: &str) -> &'static str {
    SUPPORTED_MCP_PROTOCOLS
        .iter()
        .copied()
        .find(|version| *version == requested)
        .unwrap_or(SUPPORTED_MCP_PROTOCOLS[SUPPORTED_MCP_PROTOCOLS.len() - 1])
}

/// Every protocol revision this server speaks, split by era, newest last within
/// each. `latest` is what a client should prefer and what an unknown legacy
/// version negotiates down to.
fn mcp_supported_versions() -> Value {
    json!({
        "legacy": SUPPORTED_MCP_PROTOCOLS,
        "modern": SUPPORTED_MODERN_MCP_PROTOCOLS,
        "latest": SUPPORTED_MODERN_MCP_PROTOCOLS[SUPPORTED_MODERN_MCP_PROTOCOLS.len() - 1]
    })
}

/// The legacy `MCP-Protocol-Version` header rule, as Transports states it.
///
/// Two cases that look alike but are not: an *absent* header is legal — "for
/// backwards compatibility, if the server does *not* receive an
/// `MCP-Protocol-Version` header ... the server SHOULD assume protocol version
/// `2025-03-26`", which clients predating 2025-06-18 rely on because they never
/// send the header at all. Only a header that is *present* and names a revision
/// we cannot speak is refused, and that refusal "MUST" be `400 Bad Request`.
///
/// `initialize` is exempt: it is the request that decides the version, so it
/// cannot be required to have already carried it. Returns the offending value.
fn unsupported_legacy_header<'a>(method: &str, header: Option<&'a str>) -> Option<&'a str> {
    if method == "initialize" {
        return None;
    }
    match header {
        None => None,
        Some(version) if SUPPORTED_MCP_PROTOCOLS.contains(&version) => None,
        Some(version) => Some(version),
    }
}

/// Which protocol era a `/mcp` request belongs to.
///
/// The spec keys this off how the client opens: "A request carrying modern
/// per-request `_meta` is served statelessly according to this revision. An
/// `initialize` request selects legacy semantics." `server/discover` exists only
/// in the modern era, so it counts as a modern opener even when a client sends
/// it bare as a compatibility probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpEra {
    Legacy,
    Modern,
}

fn mcp_era(method: &str, params: &Value) -> McpEra {
    if matches!(method, "initialize" | "notifications/initialized") {
        return McpEra::Legacy;
    }
    if method == "server/discover" || modern_requested_protocol(params).is_some() {
        return McpEra::Modern;
    }
    McpEra::Legacy
}

/// The protocol version a modern client declares in `params._meta`.
fn modern_requested_protocol(params: &Value) -> Option<&str> {
    params
        .get("_meta")?
        .get(MCP_META_PROTOCOL_VERSION)?
        .as_str()
}

/// Body value that `Mcp-Name` mirrors, for the methods that require the header.
/// `None` means the method does not carry an `Mcp-Name`.
fn mcp_name_body_value<'a>(method: &str, params: &'a Value) -> Option<&'a str> {
    match method {
        "tools/call" | "prompts/get" => params.get("name")?.as_str(),
        "resources/read" => params.get("uri")?.as_str(),
        _ => None,
    }
}

/// Decodes the `=?base64?…?=` sentinel Streamable HTTP uses for header values
/// that cannot travel as plain ASCII. Plain values pass through untouched.
/// `None` means the sentinel was present but its payload was not valid base64.
fn decode_mcp_header_value(raw: &str) -> Option<String> {
    let Some(encoded) = raw
        .strip_prefix("=?base64?")
        .and_then(|rest| rest.strip_suffix("?="))
    else {
        return Some(raw.to_string());
    };
    String::from_utf8(base64_decode(encoded)?).ok()
}

/// Streamable HTTP mirrors selected body fields into headers so intermediaries
/// can route without parsing the body; a server that parses the body MUST
/// reject any disagreement, otherwise the two sides of the network can act on
/// different values. Returns the message for a `HeaderMismatch` (-32020).
fn validate_modern_mcp_headers(
    method: &str,
    params: &Value,
    header_protocol: Option<&str>,
    header_method: Option<&str>,
    header_name: Option<&str>,
) -> Result<(), String> {
    let Some(header_protocol) = header_protocol else {
        return Err("Missing required MCP-Protocol-Version header.".into());
    };
    if let Some(body_protocol) = modern_requested_protocol(params) {
        if body_protocol != header_protocol {
            return Err(format!(
                "Header mismatch: MCP-Protocol-Version header value {header_protocol:?} does not \
                 match body value {body_protocol:?}"
            ));
        }
    }
    let Some(header_method) = header_method else {
        return Err("Missing required Mcp-Method header.".into());
    };
    if header_method != method {
        return Err(format!(
            "Header mismatch: Mcp-Method header value {header_method:?} does not match body value \
             {method:?}"
        ));
    }
    let Some(expected_name) = mcp_name_body_value(method, params) else {
        return Ok(());
    };
    let Some(header_name) = header_name else {
        return Err("Missing required Mcp-Name header.".into());
    };
    let Some(decoded) = decode_mcp_header_value(header_name) else {
        return Err("Mcp-Name header carries a malformed =?base64?…?= value.".into());
    };
    if decoded != expected_name {
        return Err(format!(
            "Header mismatch: Mcp-Name header value {decoded:?} does not match body value \
             {expected_name:?}"
        ));
    }
    Ok(())
}

/// `data` payload of an `UnsupportedProtocolVersionError`, which is what lets a
/// modern client retry on a revision we can actually serve. Only modern
/// revisions are listed: naming a legacy one here would invite a retry that
/// still carries modern `_meta`, which this path cannot honour.
fn unsupported_protocol_version_data(requested: &str) -> Value {
    json!({"supported": SUPPORTED_MODERN_MCP_PROTOCOLS, "requested": requested})
}

/// Every modern result carries `resultType`, and servers SHOULD identify
/// themselves in each result's `_meta` now that there is no handshake to do it.
fn modern_mcp_result(mut result: Value) -> Value {
    if let Some(object) = result.as_object_mut() {
        object.insert("resultType".into(), json!("complete"));
        object.insert(
            "_meta".into(),
            json!({
                MCP_META_SERVER_INFO: {
                    "name": "reopenscad",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        );
    }
    result
}

/// `CacheableResult`: the list/read/discover results MUST carry freshness hints.
/// `cache_scope` is `"public"` only when the payload is identical for every
/// caller — a workspace read is keyed by a URL that is itself the capability.
fn cacheable_mcp_result(result: Value, ttl_ms: u64, cache_scope: &str) -> Value {
    let mut result = modern_mcp_result(result);
    if let Some(object) = result.as_object_mut() {
        object.insert("ttlMs".into(), json!(ttl_ms));
        object.insert("cacheScope".into(), json!(cache_scope));
    }
    result
}

fn mcp_discover_result() -> Value {
    cacheable_mcp_result(
        json!({
            "supportedVersions": SUPPORTED_MODERN_MCP_PROTOCOLS,
            "capabilities": {
                "tools": {"listChanged": false},
                "resources": {"listChanged": false, "subscribe": false}
            },
            "instructions": MCP_INSTRUCTIONS
        }),
        MCP_STATIC_CACHE_TTL_MS,
        "public",
    )
}

/// Modern era (`2026-07-28`): no handshake, no session, no `ping`. Each request
/// stands alone, so everything the legacy path learned once at `initialize` is
/// revalidated here per request.
fn handle_modern_mcp(
    stream: &mut TcpStream,
    state: &AppState,
    request: &Request,
    id: Value,
    method: &str,
    params: &Value,
) -> io::Result<()> {
    // The revision defines no client-to-server notifications over Streamable
    // HTTP and leaves their header requirements unspecified, so a notification
    // is simply accepted.
    if id.is_null() {
        return respond_text(stream, 202, "application/json", b"", &[]);
    }
    if let Err(message) = validate_modern_mcp_headers(
        method,
        params,
        request.header("mcp-protocol-version"),
        request.header("mcp-method"),
        request.header("mcp-name"),
    ) {
        return mcp_error_response(stream, 400, id, MCP_HEADER_MISMATCH, &message, None);
    }
    let requested = modern_requested_protocol(params)
        .or_else(|| request.header("mcp-protocol-version"))
        .unwrap_or_default();
    if !SUPPORTED_MODERN_MCP_PROTOCOLS.contains(&requested) {
        let data = unsupported_protocol_version_data(requested);
        return mcp_error_response(
            stream,
            400,
            id,
            MCP_UNSUPPORTED_PROTOCOL_VERSION,
            "Unsupported protocol version",
            Some(data),
        );
    }
    // `protocolVersion` and `clientCapabilities` are required on every modern
    // request; a request missing one is malformed (-32602, HTTP 400).
    for key in [MCP_META_PROTOCOL_VERSION, MCP_META_CLIENT_CAPABILITIES] {
        if params.get("_meta").and_then(|meta| meta.get(key)).is_none() {
            return mcp_error_response(
                stream,
                400,
                id,
                -32602,
                &format!("params._meta[{key:?}] is required on every request."),
                None,
            );
        }
    }
    let result = match method {
        "server/discover" => mcp_discover_result(),
        "tools/list" => cacheable_mcp_result(
            json!({"tools": mcp_tool_definitions()}),
            MCP_STATIC_CACHE_TTL_MS,
            "public",
        ),
        "resources/list" => cacheable_mcp_result(
            json!({"resources": mcp_resource_definitions()}),
            MCP_STATIC_CACHE_TTL_MS,
            "public",
        ),
        "resources/templates/list" => cacheable_mcp_result(
            json!({"resourceTemplates": mcp_resource_templates()}),
            MCP_STATIC_CACHE_TTL_MS,
            "public",
        ),
        "resources/read" => {
            let Some(uri) = params.get("uri").and_then(Value::as_str) else {
                return mcp_error_response(stream, 400, id, -32602, "Resource uri is required.", None);
            };
            match mcp_read_resource(state, request, uri) {
                Ok(contents) => {
                    // The dictionary is compiled in and identical for everyone;
                    // a workspace app is per-workspace and changes on every
                    // edit, so it is private and immediately stale.
                    let (ttl_ms, scope) = if uri == SCAD_DICTIONARY_URI {
                        (MCP_STATIC_CACHE_TTL_MS, "public")
                    } else {
                        (0, "private")
                    };
                    cacheable_mcp_result(json!({"contents": [contents]}), ttl_ms, scope)
                }
                // Resource-not-found moved from -32002 to -32602 in this
                // revision. It is an addressing mistake, not a malformed
                // request, so it answers on HTTP 200 like any other RPC error.
                Err(error) => return mcp_error_response(stream, 200, id, -32602, &error, None),
            }
        }
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return mcp_error_response(stream, 400, id, -32602, "Tool name is required.", None);
            };
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if !arguments.is_object() {
                return mcp_error_response(
                    stream,
                    400,
                    id,
                    -32602,
                    "Tool arguments must be an object.",
                    None,
                );
            }
            modern_mcp_result(mcp_call_tool(state, request, name, arguments))
        }
        // Unknown method answers 404 so a client probing an unknown endpoint can
        // tell "no MCP here" from "this MCP does not do that".
        _ => return mcp_error_response(stream, 404, id, -32601, "Method not found", None),
    };
    respond_value(
        stream,
        200,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
}

fn mcp_protocol_error(
    stream: &mut TcpStream,
    id: Value,
    code: i64,
    message: &str,
) -> io::Result<()> {
    mcp_error_response(stream, 200, id, code, message, None)
}

fn mcp_error_response(
    stream: &mut TcpStream,
    status: u16,
    id: Value,
    code: i64,
    message: &str,
    data: Option<Value>,
) -> io::Result<()> {
    let mut error = json!({"code": code, "message": message});
    if let (Some(data), Some(object)) = (data, error.as_object_mut()) {
        object.insert("data".into(), data);
    }
    respond_value(
        stream,
        status,
        &json!({"jsonrpc": "2.0", "id": id, "error": error}),
    )
}

/// URI of the embedded viewer app, one instance per workspace.
const WORKSPACE_APP_URI_PREFIX: &str = "ui://reopenscad/workspace/";
const SCAD_DICTIONARY_URI: &str = "scad://reopenscad/dictionary";

/// Concrete resources.
///
/// Workspaces are deliberately absent: a workspace URL *is* its access
/// capability, so enumerating existing workspaces here would hand every
/// connected client the keys to all of them. They are reachable through the
/// template below, which requires the caller to already know the ID.
fn mcp_resource_definitions() -> Vec<Value> {
    vec![json!({
        "uri": SCAD_DICTIONARY_URI,
        "name": "supported_scad_dictionary",
        "title": "Supported SCAD dictionary",
        "description": "The language and geometry surface this engine implements, as JSON.",
        "mimeType": "application/json"
    })]
}

fn mcp_resource_templates() -> Vec<Value> {
    vec![json!({
        "uriTemplate": format!("{WORKSPACE_APP_URI_PREFIX}{{workspaceId}}"),
        "name": "workspace_app",
        "title": "Workspace preview app",
        "description": "A self-contained HTML app: an interactive 3D preview of the workspace \
                        plus STL and 3MF download links. Read it with the workspace ID returned \
                        by create_workspace or get_workspace_link.",
        "mimeType": "text/html"
    })]
}

fn mcp_read_resource(state: &AppState, request: &Request, uri: &str) -> Result<Value, String> {
    if uri == SCAD_DICTIONARY_URI {
        return Ok(json!({
            "uri": uri,
            "mimeType": "application/json",
            "text": serde_json::to_string_pretty(&supported_scad_dictionary())
                .map_err(|error| error.to_string())?
        }));
    }
    let Some(id) = uri.strip_prefix(WORKSPACE_APP_URI_PREFIX) else {
        return Err(format!("Unknown resource uri {uri:?}."));
    };
    if !valid_workspace_id(id) {
        return Err("workspaceId is invalid.".into());
    }
    let workspace = state
        .workspaces
        .get(id)?
        .ok_or_else(|| "Workspace not found.".to_string())?;
    // The embedded viewer app meshes the workspace, so it queues like any
    // other compute. `resources/read` has no tool-result shape to carry the
    // structured rejection, so the prose has to carry it alone.
    let _permit = state
        .render_queue
        .acquire(current_client_key(), None, None)
        .map_err(QueueRejection::message)?;
    let compiled = compile_parts_guarded(&workspace.code, engine::Quality::Preview)?;
    let preview = mcpapp::encode_preview(&compiled.mesh);
    let html = mcpapp::viewer_html(
        id,
        &workspace.name,
        workspace.revision,
        &compiled.parts,
        &preview,
        &workspace_url(state, request, id),
        &workspace_export_url(state, request, id, "stl", workspace.revision, None),
        &workspace_export_url(state, request, id, "3mf", workspace.revision, None),
    );
    if html.len() > MAX_OUTPUT {
        return Err("The workspace app exceeds the response limit.".into());
    }
    Ok(json!({
        "uri": uri,
        "name": workspace.name,
        "mimeType": "text/html",
        "text": html
    }))
}

fn mcp_tool_definitions() -> Vec<Value> {
    vec![
        mcp_tool(
            "create_workspace",
            "Create a persistent single-file SCAD workspace and return its browser URL.",
            json!({
                "type": "object",
                "properties": {
                    "code": {"type": "string", "description": "Initial SCAD source."},
                    "name": {"type": "string"}
                }
            }),
        ),
        mcp_tool(
            "get_supported_scad",
            "Return the language and geometry dictionary implemented by this server.",
            json!({"type": "object", "properties": {}}),
        ),
        mcp_tool(
            "get_workspace_code",
            "Read all or a UTF-8 byte range of workspace source to minimize token usage.",
            json!({
                "type": "object",
                "required": ["workspaceId"],
                "properties": {
                    "workspaceId": {"type": "string"},
                    "start": {"type": "integer", "minimum": 0},
                    "end": {"type": "integer", "minimum": 0}
                }
            }),
        ),
        mcp_tool(
            "update_workspace_code",
            "Validate and atomically replace source or apply one partial UTF-8 byte-range edit.",
            json!({
                "type": "object",
                "required": ["workspaceId", "expectedRevision"],
                "properties": {
                    "workspaceId": {"type": "string"},
                    "code": {"type": "string"},
                    "start": {"type": "integer", "minimum": 0},
                    "end": {"type": "integer", "minimum": 0},
                    "text": {"type": "string"},
                    "expectedRevision": {"type": "integer", "minimum": 1},
                    "name": {"type": "string"}
                }
            }),
        ),
        mcp_tool(
            "render_workspace",
            "Render a workspace as STL, OBJ, OFF, or 3MF and return data plus an export URL. \
             Object IDs for the optional objectIds filter come from list_workspace_objects.",
            json!({
                "type": "object",
                "required": ["workspaceId", "revision"],
                "properties": {
                    "workspaceId": {"type": "string"},
                    "revision": {"type": "integer", "minimum": 1},
                    "format": {"type": "string", "enum": EXPORT_FORMATS},
                    "includeData": {"type": "boolean", "default": true},
                    "objectIds": {"type": "array", "items": {"type": "string"}}
                }
            }),
        ),
        mcp_tool(
            "list_workspace_objects",
            "List the addressable top-level objects of a workspace: the object IDs accepted by \
             render_workspace's objectIds and check_intersections' visibleObjectIds, plus each \
             object's name, kind, triangle count and axis-aligned bounding box.",
            json!({
                "type": "object",
                "required": ["workspaceId"],
                "properties": {
                    "workspaceId": {"type": "string"},
                    "revision": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional guard: fail if the workspace has moved past this revision."
                    }
                }
            }),
        ),
        mcp_tool(
            "get_workspace_link",
            "Return the browser capability URL for an existing workspace, plus the ui:// URI of \
             the embedded preview app that resources/read serves.",
            json!({
                "type": "object",
                "required": ["workspaceId"],
                "properties": {"workspaceId": {"type": "string"}}
            }),
        ),
        mcp_tool(
            "check_intersections",
            "Check part intersections and clearances against a tolerance. Each result reports \
             the pair, the penetration depth or clearance, the world-space point where that \
             extremum occurs, the overlapping bounding box and each part's SCAD source range.",
            json!({
                "type": "object",
                "required": ["workspaceId", "tolerance"],
                "properties": {
                    "workspaceId": {"type": "string"},
                    "tolerance": {"type": "number", "minimum": 0},
                    "visibleObjectIds": {"type": "array", "items": {"type": "string"}}
                }
            }),
        ),
    ]
}

fn mcp_tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({"name": name, "description": description, "inputSchema": input_schema})
}

/// Tools whose body runs the evaluator, and therefore needs a compute permit.
///
/// Admission is centralised here rather than repeated inside each tool for one
/// reason: it is the only place with both the queue's live state and the tool
/// result shape, and a shed MCP call has to explain itself in that shape. The
/// cost is that a malformed call to a compute tool now waits for its slot
/// before being told it is malformed — bounded by `MAX_QUEUE_WAIT`, and only
/// under contention, which is a fair trade for a rejection an agent can act on.
fn mcp_tool_runs_evaluator(name: &str) -> bool {
    matches!(
        name,
        "create_workspace"
            | "update_workspace_code"
            | "render_workspace"
            | "list_workspace_objects"
            | "check_intersections"
    )
}

/// The tool result for a request the queue would not admit.
///
/// Deliberately an `isError: true` *tool* result rather than a JSON-RPC error:
/// this is a runtime condition inside a well-formed call, and the MCP spec
/// reserves protocol errors for calls it could not even dispatch. The prose is
/// written for a model — it names the condition, states outright that the SCAD
/// was never compiled so the code is not at fault, and gives an explicit retry
/// instruction with a wait derived from live queue depth. `structuredContent`
/// repeats all of it in fields so a client never has to read the sentence.
fn mcp_queue_rejection(rejection: QueueRejection) -> Value {
    let mut structured = rejection.detail();
    if let Some(object) = structured.as_object_mut() {
        object.insert(
            "nextStep".into(),
            json!(format!(
                "Wait {} seconds, then call this tool again with identical arguments. Do not modify the SCAD source in response to this error.",
                rejection.retry_after_seconds()
            )),
        );
    }
    json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(&structured).unwrap_or_default()}],
        "structuredContent": structured,
        "isError": true
    })
}

fn mcp_call_tool(state: &AppState, request: &Request, name: &str, arguments: Value) -> Value {
    let _permit = if mcp_tool_runs_evaluator(name) {
        match state
            .render_queue
            .acquire(current_client_key(), None, None)
        {
            Ok(permit) => Some(permit),
            Err(rejection) => return mcp_queue_rejection(rejection),
        }
    } else {
        None
    };
    let result = match name {
        "create_workspace" => mcp_create_workspace(state, request, &arguments),
        "get_supported_scad" => Ok(supported_scad_dictionary()),
        "get_workspace_code" => mcp_get_workspace_code(state, &arguments),
        "update_workspace_code" => mcp_update_workspace(state, request, arguments),
        "render_workspace" => mcp_render_workspace(state, request, &arguments),
        "list_workspace_objects" => mcp_list_workspace_objects(state, &arguments),
        "get_workspace_link" => mcp_workspace_link(state, request, &arguments),
        "check_intersections" => mcp_intersections(state, &arguments),
        _ => Err(format!("Unknown tool {name:?}.")),
    };
    match result {
        Ok(structured) => json!({
            "content": [{"type": "text", "text": serde_json::to_string_pretty(&structured).unwrap_or_default()}],
            "structuredContent": structured,
            "isError": false
        }),
        Err(error) => json!({
            "content": [{"type": "text", "text": error}],
            "structuredContent": {"ok": false, "error": error},
            "isError": true
        }),
    }
}

fn argument_workspace_id(arguments: &Value) -> Result<&str, String> {
    let id = arguments
        .get("workspaceId")
        .and_then(Value::as_str)
        .ok_or_else(|| "workspaceId is required.".to_string())?;
    if !valid_workspace_id(id) {
        return Err("workspaceId is invalid.".into());
    }
    Ok(id)
}

fn mcp_create_workspace(
    state: &AppState,
    request: &Request,
    arguments: &Value,
) -> Result<Value, String> {
    // The compute permit for this tool is taken by `mcp_call_tool`, which is
    // the only place that can turn a refusal into a tool result an agent can
    // act on.
    let code = arguments
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_SOURCE)
        .to_string();
    validate_source(&code).map_err(|error| format!("SCAD validation failed: {error}"))?;
    let name = arguments
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string);
    let workspace = state.workspaces.create(code, name)?;
    let objects = workspace_object_summaries(&workspace.code);
    Ok(json!({
        "ok": true,
        "workspace": workspace_public(&workspace, state, request, None, Some(objects)),
        "url": workspace_url(state, request, &workspace.id)
    }))
}

fn mcp_get_workspace_code(state: &AppState, arguments: &Value) -> Result<Value, String> {
    let id = argument_workspace_id(arguments)?;
    let workspace = state
        .workspaces
        .get(id)?
        .ok_or_else(|| "Workspace not found.".to_string())?;
    let start = arguments.get("start").and_then(Value::as_u64).unwrap_or(0) as usize;
    let end = arguments
        .get("end")
        .and_then(Value::as_u64)
        .unwrap_or(workspace.code.len() as u64) as usize;
    workspace_code_slice(&workspace, start, end)
}

fn mcp_update_workspace(
    state: &AppState,
    request: &Request,
    mut arguments: Value,
) -> Result<Value, String> {
    // Permit held by `mcp_call_tool`.
    let id = argument_workspace_id(&arguments)?.to_string();
    let object = arguments
        .as_object_mut()
        .ok_or_else(|| "Tool arguments must be an object.".to_string())?;
    object.remove("workspaceId");
    object.insert("source".into(), Value::String("ai".into()));
    let update: WorkspaceUpdate =
        serde_json::from_value(arguments).map_err(|error| format!("Invalid update: {error}"))?;
    match state.workspaces.update(&id, update) {
        Ok(result) => {
            let objects = workspace_object_summaries(&result.workspace.code);
            Ok(json!({
                "ok": true,
                "valid": true,
                "workspace": workspace_public(
                    &result.workspace,
                    state,
                    request,
                    Some(&result.event),
                    Some(objects)
                ),
                "diagnostics": []
            }))
        }
        Err(UpdateError::Validation(error)) => Ok(json!({
            "ok": false,
            "valid": false,
            "error": error,
            "diagnostics": [{"severity": "error", "message": error}]
        })),
        Err(UpdateError::Conflict(workspace)) => Err(format!(
            "Revision conflict: current revision is {}.",
            workspace.revision
        )),
        Err(UpdateError::NotFound) => Err("Workspace not found.".into()),
        Err(UpdateError::Invalid(error) | UpdateError::Internal(error)) => Err(error),
    }
}

fn mcp_render_workspace(
    state: &AppState,
    request: &Request,
    arguments: &Value,
) -> Result<Value, String> {
    let id = argument_workspace_id(arguments)?;
    let revision = arguments
        .get("revision")
        .and_then(Value::as_u64)
        .ok_or_else(|| "revision is required.".to_string())?;
    let workspace = state
        .workspaces
        .get(id)?
        .ok_or_else(|| "Workspace not found.".to_string())?;
    if workspace.revision != revision {
        return Err(format!(
            "Revision conflict: current revision is {}.",
            workspace.revision
        ));
    }
    let format = arguments
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("stl");
    if !is_supported_export_format(format) {
        return Err(supported_export_formats_sentence());
    }
    let object_ids = requested_object_ids(arguments, &HashMap::new())?;
    // Permit held by `mcp_call_tool`.
    let output = compile_parts_guarded(&workspace.code, engine::Quality::Render)?;
    if multipart_binary_size(&output).is_none_or(|size| size > MAX_OUTPUT) {
        return Err("Aggregate multipart mesh output exceeds the 64 MiB limit.".into());
    }
    let selected_mesh = selected_parts_mesh(
        &output,
        object_ids.as_deref(),
        engine::Quality::Render,
        None,
    )?;
    let data = encode_mesh(&selected_mesh, format, &workspace.name);
    if data.len() > MAX_OUTPUT {
        return Err("Export exceeds the 64 MiB limit.".into());
    }
    let include_data = arguments
        .get("includeData")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if include_data
        && base64_size(data.len()).is_none_or(|size| size > MAX_OUTPUT.saturating_sub(MAX_BODY))
    {
        return Err("Encoded MCP render output exceeds the response limit.".into());
    }
    Ok(json!({
        "ok": true,
        "workspaceId": id,
        "revision": workspace.revision,
        "format": format,
        "bytes": data.len(),
        "dataBase64": include_data.then(|| base64_encode(&data)),
        "exportUrl": workspace_export_url(state, request, id, format, revision, object_ids.as_deref()),
        "workspaceUrl": workspace_url(state, request, id)
    }))
}

fn workspace_export_url(
    state: &AppState,
    request: &Request,
    id: &str,
    format: &str,
    revision: u64,
    object_ids: Option<&[String]>,
) -> String {
    let selection_query = object_ids
        .map(|ids| format!("&objectIds={}", ids.join(",")))
        .unwrap_or_default();
    format!(
        "{}/api/workspaces/{id}/export?format={format}&revision={revision}{selection_query}",
        request_base_url(state, request)
    )
}

/// Best-effort object summaries for a workspace payload.
///
/// Used on write paths where a failure to mesh must not fail the write: an
/// empty list means "no addressable solids", which is exactly what a 2D-only
/// or empty document has.
fn workspace_object_summaries(source: &str) -> Vec<Value> {
    compile_parts_guarded(source, engine::Quality::Preview)
        .map(|output| output.parts.iter().map(part_summary_public).collect())
        .unwrap_or_default()
}

fn mcp_list_workspace_objects(state: &AppState, arguments: &Value) -> Result<Value, String> {
    let id = argument_workspace_id(arguments)?;
    let workspace = state
        .workspaces
        .get(id)?
        .ok_or_else(|| "Workspace not found.".to_string())?;
    if let Some(revision) = arguments.get("revision").and_then(Value::as_u64) {
        if workspace.revision != revision {
            return Err(format!(
                "Revision conflict: current revision is {}.",
                workspace.revision
            ));
        }
    }
    // Permit held by `mcp_call_tool`.
    let compiled = compile_parts_guarded(&workspace.code, engine::Quality::Preview)?;
    Ok(json!({
        "ok": true,
        "workspaceId": id,
        "revision": workspace.revision,
        "objects": compiled
            .parts
            .iter()
            .map(part_summary_public)
            .collect::<Vec<_>>()
    }))
}

fn mcp_workspace_link(
    state: &AppState,
    request: &Request,
    arguments: &Value,
) -> Result<Value, String> {
    let id = argument_workspace_id(arguments)?;
    state
        .workspaces
        .get(id)?
        .ok_or_else(|| "Workspace not found.".to_string())?;
    Ok(json!({
        "ok": true,
        "workspaceId": id,
        "url": workspace_url(state, request, id),
        "appUri": format!("{WORKSPACE_APP_URI_PREFIX}{id}")
    }))
}

fn mcp_intersections(state: &AppState, arguments: &Value) -> Result<Value, String> {
    let id = argument_workspace_id(arguments)?;
    let workspace = state
        .workspaces
        .get(id)?
        .ok_or_else(|| "Workspace not found.".to_string())?;
    let tolerance = arguments
        .get("tolerance")
        .and_then(Value::as_f64)
        .ok_or_else(|| "tolerance is required and must be a number.".to_string())?;
    // Permit held by `mcp_call_tool`.
    let compiled = compile_parts_guarded(&workspace.code, engine::Quality::Preview)?;
    if multipart_binary_size(&compiled).is_none_or(|size| size > MAX_OUTPUT) {
        return Err("Aggregate multipart mesh output exceeds the 64 MiB limit.".into());
    }
    let hidden = hidden_part_ids(&compiled.parts, arguments)?;
    clearance_report(id, &workspace, &compiled.parts, tolerance, &hidden)
}

fn supported_scad_dictionary() -> Value {
    json!({
        "engine": "ReOpenSCAD native Rust evaluator",
        "geometry": {
            "primitives2d": ["square", "polygon", "circle"],
            "primitives3d": ["cube", "sphere", "cylinder"],
            "parameterAliases": {
                "circle": {"diameter": "d"},
                "sphere": {"diameter": "d"},
                "cylinder": {"diameter": "d", "bottomDiameter": "d1", "topDiameter": "d2"}
            },
            "transforms": ["translate", "rotate", "scale", "mirror", "color"],
            "csg": ["union", "difference", "intersection", "hull", "minkowski"],
            "operations2d": ["offset"],
            "extrusions": ["linear_extrude"]
        },
        "language": [
            "variables", "vectors", "ranges", "arithmetic", "comparison", "boolean operators",
            "ternary expressions", "if/else", "for", "intersection_for", "let", "assert",
            "echo", "modules", "functions", "children"
        ],
        "functions": [
            "abs", "sign", "floor", "ceil", "round", "sqrt", "exp", "ln", "log", "sin",
            "cos", "tan", "asin", "acos", "atan", "pow", "min", "max", "norm", "cross",
            "rands", "len", "concat", "str", "chr", "ord", "lookup", "search", "is_undef",
            "is_bool", "is_num", "is_string", "is_list", "is_function", "version", "version_num"
        ],
        "exportFormats": EXPORT_FORMATS,
        "notes": [
            "The dictionary describes implemented behavior, not full OpenSCAD parity.",
            "minkowski uses an approximation in this engine build."
        ]
    })
}

fn handle_health(stream: &mut TcpStream) -> io::Result<()> {
    respond_json(
        stream,
        200,
        r#"{"ok":true,"engine":"Native Rust SCAD evaluator","version":"ReOpenSCAD 0.1-native"}"#,
    )
}

fn handle_render(stream: &mut TcpStream, state: &AppState, body: &[u8]) -> io::Result<()> {
    // Admission moved below the parse deliberately: the queue can only honour
    // a cancellation if it knows the `requestId`, and the `requestId` is in the
    // body. Parsing first costs a bounded JSON decode; not parsing first costs
    // the ability to cancel a queued render at all.
    let input = match parse_json_body(body) {
        Ok(input) => input,
        Err(error) => return respond_error(stream, 400, &error),
    };
    let source = match input.get("code").and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => value.to_string(),
        _ => return respond_error(stream, 400, "The editor is empty."),
    };
    let mode = input
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("preview")
        .to_string();
    let request_id = input
        .get("requestId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !valid_request_id(request_id) {
        return respond_error(stream, 400, "Invalid render request ID.");
    }
    let (cancellation, _registration) = match register_cancellation(state, request_id) {
        Ok(registration) => registration,
        Err(error) => return respond_error(stream, 409, &error),
    };
    let queued_at = Instant::now();
    let _permit = match state.render_queue.acquire(
        current_client_key(),
        Some(request_id),
        Some(&cancellation),
    ) {
        Ok(permit) => permit,
        Err(rejection) => return respond_queue_rejection(stream, rejection),
    };
    let queued_ms = queued_at.elapsed().as_millis();
    let quality = if mode == "render" {
        engine::Quality::Render
    } else {
        engine::Quality::Preview
    };
    // Started here, not at arrival: the watchdog measures compute, never wait.
    let deadline = RenderDeadline::start(&cancellation, deadline_for(quality));
    let compiled = match compile_native(&source, quality, Some(&cancellation)) {
        Ok(compiled) => compiled,
        Err(error) if deadline.expired() => {
            return respond_error(stream, 422, &deadline.explain(error))
        }
        Err(error) if error.to_ascii_lowercase().contains("cancelled") => {
            return respond_error(stream, 409, "Render cancelled.")
        }
        Err(error) => return respond_error(stream, 422, &error),
    };
    respond_value(
        stream,
        200,
        &json!({
            "ok": true,
            "mesh": base64_encode(&compiled.bytes),
            "triangles": compiled.triangles,
            "console": compiled.console,
            "elapsedMs": compiled.elapsed_ms,
            "queuedMs": queued_ms,
            "mode": mode
        }),
    )
}

fn compile_native(
    source: &str,
    quality: engine::Quality,
    cancellation: Option<&AtomicBool>,
) -> Result<CompiledMesh, String> {
    let started = Instant::now();
    let output = engine::compile_shared(source, quality, cancellation)
        .map_err(|error| error.to_string())?;
    let triangles = output.mesh.triangles.len();
    let bytes = output.mesh.binary_stl();
    if bytes.len() > MAX_OUTPUT {
        return Err("Compiled mesh exceeds the 64 MiB limit.".into());
    }
    Ok(CompiledMesh {
        bytes,
        console: output.messages.join("\n"),
        elapsed_ms: started.elapsed().as_millis(),
        triangles,
    })
}

fn handle_cancel(stream: &mut TcpStream, state: &AppState, body: &[u8]) -> io::Result<()> {
    let input = match parse_json_body(body) {
        Ok(input) => input,
        Err(error) => return respond_error(stream, 400, &error),
    };
    let request_id = input
        .get("requestId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !valid_request_id(request_id) {
        return respond_error(stream, 400, "Invalid render request ID.");
    }
    let cancellation = state
        .cancellations
        .lock()
        .map_err(|_| io::Error::other("Cancellation registry is unavailable"))?
        .get(request_id)
        .cloned();
    let cancelled = cancellation.is_some();
    if let Some(cancellation) = cancellation {
        cancellation.store(true, Ordering::Release);
    }
    respond_value(stream, 200, &json!({"ok": true, "cancelled": cancelled}))
}

/// `GET /api/queue?requestId=…` — where a render is in the queue right now.
///
/// A render answers once, at the end, so its own response cannot say "you are
/// third in line". Without a second channel a queued request is
/// indistinguishable from a slow one, which is exactly how a wait comes to
/// look like a hang. This is the smallest channel that fixes it: the browser
/// polls it alongside the render fetch it already has open, and no existing
/// response shape changes. (The alternatives — folding queue state into the
/// `/events` long poll, or converting render to an async job API — are
/// evaluated in the accompanying notes; `/events` is workspace-scoped and so
/// cannot describe a stateless `/api/render`, and the job API is a much larger
/// change than the problem justifies today.)
fn handle_queue_status(stream: &mut TcpStream, state: &AppState, query: &str) -> io::Result<()> {
    let parameters = query_parameters(query);
    let Some(request_id) = parameters.get("requestId") else {
        return respond_error(stream, 400, "requestId is required.");
    };
    if !valid_request_id(request_id) {
        return respond_error(stream, 400, "Invalid render request ID.");
    }
    respond_value(stream, 200, &state.render_queue.status(request_id))
}

fn valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn handle_export(stream: &mut TcpStream, state: &AppState, body: &[u8]) -> io::Result<()> {
    let _permit = match state
        .render_queue
        .acquire(current_client_key(), None, None)
    {
        Ok(permit) => permit,
        Err(rejection) => return respond_queue_rejection(stream, rejection),
    };
    let input = match parse_json_body(body) {
        Ok(input) => input,
        Err(error) => return respond_error(stream, 400, &error),
    };
    let source = match input.get("code").and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => value.to_string(),
        _ => return respond_error(stream, 400, "The editor is empty."),
    };
    let format = input
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("stl")
        .to_string();
    if !is_supported_export_format(format.as_str()) {
        return respond_error(stream, 400, &supported_export_formats_sentence());
    }
    let object_ids = match requested_object_ids(&input, &HashMap::new()) {
        Ok(object_ids) => object_ids,
        Err(error) => return respond_error(stream, 400, &error),
    };
    let mesh = match export_mesh(&source, object_ids.as_deref()) {
        Ok(mesh) => mesh,
        Err(error) => return respond_error(stream, 422, &error),
    };
    let data = encode_mesh(&mesh, format.as_str(), "model");
    if data.len() > MAX_OUTPUT {
        return respond_error(stream, 422, "Export exceeds the 64 MiB limit.");
    }
    let content_type = export_content_type(format.as_str());
    let disposition = format!("attachment; filename=\"model.{format}\"");
    respond_text(
        stream,
        200,
        content_type,
        &data,
        &[("Content-Disposition", &disposition)],
    )
}

/// Files that live in the web root for the developer's benefit and must never
/// be served, because the web root is also the document root.
///
/// The container image sidesteps this by copying only the three shell files
/// and `legacy-assets/` (see `Dockerfile`), but a developer running the binary
/// straight out of the source tree serves whatever is next to `index.html` —
/// and `docker-compose.yml` contains a database password.
fn is_private_web_root_file(relative: &str) -> bool {
    const PRIVATE: &[&str] = &[
        "dockerfile",
        "docker-compose.yml",
        "docker-compose.yaml",
        ".dockerignore",
        "deploy.md",
        "package.json",
        "package-lock.json",
    ];
    let name = relative.rsplit('/').next().unwrap_or(relative);
    let lowered = name.to_ascii_lowercase();
    PRIVATE.contains(&lowered.as_str())
        // Dotfiles in general: `.env` and friends are exactly the kind of
        // thing that appears next to a Dockerfile later on.
        || lowered.starts_with('.')
}

fn handle_static(stream: &mut TcpStream, state: &AppState, route: &str) -> io::Result<()> {
    if route.starts_with("/backend/") || route.starts_with("/.reopenscad-workspaces/") {
        return respond_text(stream, 404, "text/plain", b"Not found", &[]);
    }
    if workspace_shell_id(route).is_some() {
        let index = state.web_root.join("index.html");
        return respond_text(stream, 200, mime_type(&index), &fs::read(index)?, &[]);
    }
    let relative = if route == "/" {
        String::from("index.html")
    } else {
        percent_decode(route.trim_start_matches('/'))
    };
    // Checked on the decoded path, so `%2e%64ockerfile` and `./Dockerfile` are
    // covered too.
    if is_private_web_root_file(&relative) {
        return respond_text(stream, 404, "text/plain", b"Not found", &[]);
    }
    let root = match state.web_root.canonicalize() {
        Ok(root) => root,
        Err(_) => return respond_text(stream, 404, "text/plain", b"Not found", &[]),
    };
    let target = match root.join(&relative).canonicalize() {
        Ok(target) if target.starts_with(&root) && target.is_file() => target,
        _ => return respond_text(stream, 404, "text/plain", b"Not found", &[]),
    };
    let mime = mime_type(&target);
    let body = fs::read(target)?;
    respond_text(stream, 200, mime, &shell_body(mime, body), &[])
}

/// `index.html` is one file serving two audiences: the landing page, which
/// should be findable, and the workspace shell, which must not be.
///
/// The tag therefore lives in the file unconditionally and the single
/// indexable route takes it back out. That ordering is the safe one: if this
/// ever stops matching, the failure is a landing page nobody can find, not a
/// workspace anybody can.
const ROBOTS_META: &str = "<meta name=\"robots\" content=\"noindex, nofollow, noarchive\" />";

fn shell_body(mime: &str, body: Vec<u8>) -> Vec<u8> {
    if !is_indexable() || !mime.starts_with("text/html") {
        return body;
    }
    match String::from_utf8(body) {
        Ok(text) => text.replacen(ROBOTS_META, "", 1).into_bytes(),
        Err(error) => error.into_bytes(),
    }
}

fn workspace_shell_id(route: &str) -> Option<&str> {
    route
        .strip_prefix("/workspaces/")
        .map(|id| id.trim_end_matches('/'))
        .filter(|id| valid_workspace_id(id))
}

fn mime_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("wav") => "audio/wav",
        _ => "application/octet-stream",
    }
}

fn respond_json(stream: &mut TcpStream, status: u16, json: &str) -> io::Result<()> {
    respond_text(
        stream,
        status,
        "application/json; charset=utf-8",
        json.as_bytes(),
        &[],
    )
}

fn respond_text(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    extra_headers: &[(&str, &str)],
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Content Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        405 => "Method Not Allowed",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        500 => "Internal Server Error",
        _ => "Response",
    };
    // Single write path for the whole server; tally bytes here so the per-IP
    // byte budget sees exactly what was served.
    note_response_bytes(body.len() as u64);
    // `Referrer-Policy: no-referrer` is the header that matters most here. The
    // path of a workspace page is its password, and the default policy would
    // put that path in the `Referer` of every request the page makes off-site.
    // The links in the shell already carry `rel="noreferrer"`, but that is a
    // per-element opt-in that the next link added would have to remember; this
    // is the whole-origin rule that does not depend on remembering. Nothing in
    // this server reads `Referer` (same-origin enforcement uses `Origin` and
    // `Host`), so suppressing it costs nothing.
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n",
        body.len()
    )?;
    // Covers what a `<meta>` tag cannot: JSON API replies and STL/3MF/OBJ
    // exports are not HTML, and their URLs contain the workspace id too.
    if !is_indexable() {
        write!(stream, "X-Robots-Tag: {ROBOTS_TAG}\r\n")?;
    }
    for (name, value) in extra_headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write!(stream, "\r\n")?;
    stream.write_all(body)
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'%' && cursor + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[cursor + 1..cursor + 3]) {
                if let Ok(decoded) = u8::from_str_radix(hex, 16) {
                    output.push(decoded);
                    cursor += 3;
                    continue;
                }
            }
        }
        output.push(bytes[cursor]);
        cursor += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

/// Strict standard-alphabet base64 decode, used for the `=?base64?…?=` sentinel
/// that MCP header values carry when they are not plain ASCII. Rejects anything
/// malformed rather than guessing, so a bad header cannot smuggle a value past
/// the header/body comparison.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return Some(Vec::new());
    }
    if bytes.len() % 4 != 0 {
        return None;
    }
    let padding = bytes.iter().rev().take_while(|byte| **byte == b'=').count();
    if padding > 2 {
        return None;
    }
    let mut output = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for &byte in &bytes[..bytes.len() - padding] {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
        }
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_store() -> (WorkspaceStore, PathBuf) {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let root = env::temp_dir().join(format!(
            "reopenscad-workspace-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let store = WorkspaceStore::open(root.clone()).unwrap();
        (store, root)
    }

    fn temporary_state(public_origin: Option<&str>) -> (AppState, PathBuf) {
        let (workspaces, root) = temporary_store();
        (
            AppState {
                web_root: PathBuf::new(),
                render_queue: RenderQueue::new(
                    MAX_CONCURRENT_JOBS,
                    MAX_QUEUED_JOBS,
                    MAX_QUEUED_PER_CLIENT,
                    MAX_QUEUE_WAIT,
                ),
                poll_limiter: JobLimiter::new(MAX_CONCURRENT_POLLS),
                rate_limiter: RateLimiter::new(),
                cancellations: Arc::new(Mutex::new(HashMap::new())),
                workspaces: Store::Filesystem(workspaces),
                public_origin: public_origin.map(str::to_string),
            },
            root,
        )
    }

    /// Every workspace file written before the build-plate feature existed lacks
    /// `plate` and `tolerance`. Without the serde defaults `load` treats the
    /// document as corrupt and answers 404, silently deleting real work, so this
    /// deserializes the exact pre-change shape.
    #[test]
    fn workspace_files_written_before_the_viewer_settings_still_load() {
        let legacy = r#"{
            "id": "bouncy-badger-d359d13470665db8e7bd93af4fff",
            "name": "Untitled.scad",
            "code": "cube(10);",
            "revision": 3,
            "createdAt": 1756000000000,
            "updatedAt": 1756000000001,
            "events": []
        }"#;
        let workspace: Workspace = serde_json::from_str(legacy).unwrap();
        assert_eq!(workspace.code, "cube(10);");
        assert_eq!(workspace.revision, 3);
        assert_eq!(workspace.plate, "");
        assert_eq!(workspace.tolerance, DEFAULT_TOLERANCE);

        // And the same file round-trips through the store, which is the path the
        // HTTP GET actually takes.
        let (store, root) = temporary_store();
        let id = "bouncy-badger-d359d13470665db8e7bd93af4fff";
        fs::write(root.join(format!("{id}.json")), legacy).unwrap();
        let reopened = WorkspaceStore::open(root.clone()).unwrap();
        let loaded = reopened.get(id).unwrap().expect("legacy workspace loads");
        assert_eq!(loaded.plate, "");
        assert_eq!(loaded.tolerance, DEFAULT_TOLERANCE);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    /// Picking a build plate is a viewer preference, not an edit: it must persist
    /// without bumping the revision (which would look like a remote edit to every
    /// other tab) and without needing a `baseRevision` to race against.
    #[test]
    fn viewer_settings_persist_without_touching_the_revision() {
        let (store, root) = temporary_store();
        let created = store
            .create_with_settings(
                "cube(1);".into(),
                None,
                Some("prusa-mk4s".into()),
                Some(0.35),
            )
            .unwrap();
        assert_eq!(created.plate, "prusa-mk4s");
        assert_eq!(created.tolerance, 0.35);

        let updated = store
            .update_settings(&created.id, Some("bambu-x1"), Some(0.0))
            .unwrap();
        assert_eq!(updated.plate, "bambu-x1");
        assert_eq!(updated.tolerance, 0.0, "a deliberate 0 mm clearance survives");
        assert_eq!(updated.revision, created.revision, "no phantom edit");

        // A later code save must not clear the plate the user picked.
        let edited = store
            .update(
                &created.id,
                WorkspaceUpdate {
                    code: Some("cube(2);".into()),
                    name: None,
                    start: None,
                    end: None,
                    text: None,
                    expected_revision: Some(updated.revision),
                    base_revision: None,
                    plate: None,
                    tolerance: None,
                    source: "browser".into(),
                },
            )
            .unwrap();
        assert_eq!(edited.workspace.plate, "bambu-x1");
        assert_eq!(edited.workspace.tolerance, 0.0);
        assert_eq!(edited.workspace.revision, updated.revision + 1);

        // Settings reach disk, not just the cache.
        let bytes = fs::read(root.join(format!("{}.json", created.id))).unwrap();
        let stored: Workspace = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(stored.plate, "bambu-x1");
        fs::remove_dir_all(root).unwrap();
    }

    /// Viewer settings are revisionless, so the poll's `changed` flag says
    /// nothing about them: they have to ride every events payload, or a plate
    /// picked in one tab (or by an MCP agent) would never reach a tab already
    /// looking at the workspace.
    #[test]
    fn events_payload_always_carries_the_viewer_settings() {
        let workspace = Workspace {
            id: "sunny-otter-0011223344556677".into(),
            name: "Untitled model".into(),
            code: "cube(10);".into(),
            revision: 7,
            created_at: 1,
            updated_at: 2,
            plate: "prusa-mk4s".into(),
            tolerance: 0.35,
            events: Vec::new(),
        };
        // Nothing changed: the settings still have to be there.
        let idle = workspace_events_payload(&workspace.id, &workspace, 7);
        assert_eq!(idle["changed"], json!(false));
        assert_eq!(idle["plate"], json!("prusa-mk4s"));
        assert_eq!(idle["tolerance"], json!(0.35));
        let changed = workspace_events_payload(&workspace.id, &workspace, 6);
        assert_eq!(changed["changed"], json!(true));
        assert_eq!(changed["plate"], json!("prusa-mk4s"));
    }

    /// Plate ids are client-chosen catalogue keys, so they are sanitized rather
    /// than trusted — but a nonsense preference must degrade to "no plate", never
    /// fail the request that carried it.
    #[test]
    fn plate_ids_and_tolerances_are_sanitized() {
        assert_eq!(sanitize_plate("prusa-mk4s"), "prusa-mk4s");
        assert_eq!(sanitize_plate("  Bambu_X1  "), "bambu_x1");
        assert_eq!(sanitize_plate("../../etc/passwd"), "");
        assert_eq!(sanitize_plate("<script>"), "");
        assert_eq!(sanitize_plate(&"a".repeat(MAX_PLATE_ID + 1)), "");
        assert_eq!(sanitize_plate(""), "");

        assert_eq!(sanitize_tolerance(0.0), 0.0);
        assert_eq!(sanitize_tolerance(0.45), 0.45);
        assert_eq!(sanitize_tolerance(-1.0), DEFAULT_TOLERANCE);
        assert_eq!(sanitize_tolerance(f64::NAN), DEFAULT_TOLERANCE);
        assert_eq!(sanitize_tolerance(f64::INFINITY), DEFAULT_TOLERANCE);
        assert_eq!(sanitize_tolerance(MAX_TOLERANCE + 1.0), DEFAULT_TOLERANCE);
    }

    #[test]
    fn parses_json_source() {
        let json = r#"{"note":"a nested \"code\": marker","code":"cube([1, 2, 3]);\n// café"}"#;
        let input = parse_json_body(json.as_bytes()).unwrap();
        assert_eq!(
            input.get("code").and_then(Value::as_str).unwrap(),
            "cube([1, 2, 3]);\n// café"
        );
    }

    #[test]
    fn encodes_base64() {
        assert_eq!(base64_encode(b"ReOpenSCAD"), "UmVPcGVuU0NBRA==");
    }

    #[test]
    fn rejects_traversal_after_decode() {
        assert_eq!(percent_decode("..%2fsecret"), "../secret");
    }

    #[test]
    fn native_compile_produces_binary_stl() {
        let compiled = compile_native(
            "difference() { cube(10, center=true); sphere(3); }",
            engine::Quality::Preview,
            None,
        )
        .unwrap();
        assert!(compiled.triangles > 100);
        assert_eq!(compiled.bytes.len(), 84 + compiled.triangles * 50);
        assert!(compiled.console.contains("Exact polyhedral kernel"));
    }

    #[test]
    fn validates_render_request_ids() {
        assert!(valid_request_id("render-42_a"));
        assert!(!valid_request_id(""));
        assert!(!valid_request_id("render/../../cancel"));
    }

    #[test]
    fn accepts_cli_and_same_origin_requests_for_deployment_hosts() {
        let (local_state, local_root) = temporary_state(None);
        let (public_state, public_root) = temporary_state(Some("https://cad.example"));
        let request = |host: &str, origin: Option<&str>| Request {
            method: "POST".into(),
            path: "/api/render".into(),
            query: String::new(),
            headers: [
                Some(("host".into(), host.into())),
                origin.map(|value| ("origin".into(), value.into())),
            ]
            .into_iter()
            .flatten()
            .collect(),
            body: Vec::new(),
        };
        assert!(request_is_allowed(
            &request("127.0.0.1:5173", Some("http://127.0.0.1:5173")),
            &local_state
        ));
        assert!(!request_is_allowed(
            &request("attacker.example", Some("http://attacker.example")),
            &local_state
        ));
        assert!(!request_is_allowed(
            &request("127.0.0.1:5173", Some("https://attacker.example")),
            &local_state
        ));
        assert!(request_is_allowed(
            &request("127.0.0.1:5173", None),
            &local_state
        ));
        assert!(!request_is_allowed(
            &request("bad host", None),
            &local_state
        ));
        assert!(request_is_allowed(
            &request("cad.example", Some("https://cad.example")),
            &public_state
        ));
        assert!(!request_is_allowed(
            &request("poison.example", Some("https://cad.example")),
            &public_state
        ));
        fs::remove_dir_all(local_root).unwrap();
        fs::remove_dir_all(public_root).unwrap();
    }

    #[test]
    fn render_limiter_releases_capacity() {
        let limiter = JobLimiter::new(2);
        let first = limiter.try_acquire().expect("first slot");
        let second = limiter.try_acquire().expect("second slot");
        assert!(limiter.try_acquire().is_none());
        drop(first);
        assert!(limiter.try_acquire().is_some());
        drop(second);
    }

    // --- The render queue -------------------------------------------------
    //
    // Five properties, one test each: contention waits instead of failing, the
    // deadline never charges for the wait, one client cannot starve another,
    // a cancelled job never starts, and the whole thing stays bounded.

    fn test_queue(capacity: usize, depth: usize, per_client: usize) -> RenderQueue {
        RenderQueue::new(capacity, depth, per_client, Duration::from_secs(5))
    }

    fn client(last_octet: u8) -> ClientKey {
        ClientKey::Ip(IpAddr::from([203, 0, 113, last_octet]))
    }

    /// Blocks until `count` jobs are parked, so a test can build an exact
    /// queue order without sleeping and hoping.
    fn await_depth(queue: &RenderQueue, count: usize) {
        for _ in 0..2_000 {
            if queue.inner.state.lock().unwrap().depth == count {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("queue never reached depth {count}");
    }

    #[test]
    fn contended_jobs_queue_instead_of_being_rejected() {
        // The old behaviour: the third caller got 429 and lost its work. The
        // queue's whole point is that all of them finish.
        let queue = Arc::new(test_queue(1, 8, 8));
        let done = Arc::new(AtomicUsize::new(0));
        let workers: Vec<_> = (0..5)
            .map(|index| {
                let queue = Arc::clone(&queue);
                let done = Arc::clone(&done);
                thread::spawn(move || {
                    let permit = queue
                        .acquire(client(index), None, None)
                        .expect("every contended job must be admitted, not shed");
                    thread::sleep(Duration::from_millis(10));
                    done.fetch_add(1, Ordering::SeqCst);
                    drop(permit);
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(done.load(Ordering::SeqCst), 5);
        let state = queue.inner.state.lock().unwrap();
        assert_eq!(state.active, 0, "permits must be returned");
        assert_eq!(state.depth, 0, "no waiter may be left behind");
    }

    #[test]
    fn the_render_deadline_starts_at_execution_not_at_arrival() {
        // The requirement in one assertion: a job that spent longer waiting
        // than its entire compute budget still gets that budget in full.
        let queue = test_queue(1, 4, 4);
        let budget = Duration::from_millis(200);
        let blocker = queue.acquire(client(1), None, None).unwrap();
        let queued_at = Instant::now();
        let waiter = thread::spawn(move || {
            let queue = queue;
            let permit = queue.acquire(client(2), None, None).unwrap();
            let admitted = Instant::now();
            let cancellation = Arc::new(AtomicBool::new(false));
            let deadline = RenderDeadline::start(&cancellation, budget);
            // Nothing spent in the queue may have been charged already.
            assert!(
                !deadline.expired(),
                "a job that waited must not start out of time"
            );
            thread::sleep(budget / 2);
            assert!(!deadline.expired(), "half the budget must still be left");
            thread::sleep(budget);
            assert!(deadline.expired(), "the budget must still be enforced");
            drop(permit);
            admitted
        });
        // Hold the only slot for twice the compute budget.
        thread::sleep(budget * 2);
        drop(blocker);
        let admitted = waiter.join().unwrap();
        let waited = admitted.duration_since(queued_at);
        assert!(
            waited >= budget,
            "the test is meaningless unless the job really waited (waited {waited:?})"
        );
    }

    #[test]
    fn one_client_flooding_the_queue_cannot_starve_another() {
        // Round-robin, stated as a schedule: a backlog delays the newcomer by
        // one job, not by the whole backlog. Plain FIFO would serve
        // A, A, A, B — the starvation this exists to prevent.
        let queue = Arc::new(test_queue(1, 8, 8));
        let served = Arc::new(Mutex::new(Vec::new()));
        let gate = queue.acquire(client(9), None, None).unwrap();
        let mut workers = Vec::new();
        // Enqueue one at a time so the arrival order is exact, not raced.
        for (index, key) in [(0, client(1)), (1, client(1)), (2, client(1)), (3, client(2))] {
            let worker_queue = Arc::clone(&queue);
            let served = Arc::clone(&served);
            let label = if key == client(2) { "B" } else { "A" };
            workers.push(thread::spawn(move || {
                let permit = worker_queue.acquire(key, None, None).unwrap();
                served.lock().unwrap().push(format!("{label}{index}"));
                drop(permit);
            }));
            await_depth(&queue, index + 1);
        }
        drop(gate);
        for worker in workers {
            worker.join().unwrap();
        }
        let order = served.lock().unwrap().clone();
        assert_eq!(
            order,
            vec!["A0", "B3", "A1", "A2"],
            "the newcomer must be served after one queued job, not after all three"
        );
    }

    #[test]
    fn cancelling_a_queued_job_removes_it_without_ever_running_it() {
        let queue = Arc::new(test_queue(1, 4, 4));
        let cancellation = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let gate = queue.acquire(client(1), None, None).unwrap();
        let waiter = {
            let queue = Arc::clone(&queue);
            let cancellation = Arc::clone(&cancellation);
            let started = Arc::clone(&started);
            thread::spawn(move || {
                let outcome = queue.acquire(client(2), Some("render-1"), Some(&cancellation));
                if outcome.is_ok() {
                    started.store(true, Ordering::SeqCst);
                }
                outcome.err()
            })
        };
        await_depth(&queue, 1);
        // Exactly what `POST /api/cancel` does to a registered request.
        cancellation.store(true, Ordering::Release);
        let rejection = waiter.join().unwrap().expect("cancel must not hand out a permit");
        assert_eq!(rejection.error, QueueError::Cancelled);
        assert_eq!(rejection.status(), 409);
        assert!(
            !started.load(Ordering::SeqCst),
            "a cancelled job must never begin"
        );
        assert_eq!(
            queue.inner.state.lock().unwrap().depth,
            0,
            "the cancelled job must leave the queue"
        );
        drop(gate);
    }

    #[test]
    fn the_queue_stays_bounded_and_says_so_in_a_way_a_client_can_act_on() {
        let queue = Arc::new(test_queue(1, 3, 2));
        let gate = queue.acquire(client(9), None, None).unwrap();
        let mut workers = Vec::new();
        let mut enqueue = |key: ClientKey, depth: usize| {
            let worker_queue = Arc::clone(&queue);
            workers.push(thread::spawn(move || {
                drop(worker_queue.acquire(key, None, None).unwrap());
            }));
            await_depth(&queue, depth);
        };
        // One client fills its own allowance of two.
        enqueue(client(1), 1);
        enqueue(client(1), 2);
        // Its third is refused even though the queue still has a place free:
        // that free place belongs to somebody else, which is the whole point.
        let capped = queue.acquire(client(1), None, None).unwrap_err();
        assert_eq!(capped.error, QueueError::ClientLimit);
        // And the place really was available to another client.
        enqueue(client(2), 3);
        // Now the global cap bites, for everyone.
        let full = queue.acquire(client(3), None, None).unwrap_err();
        assert_eq!(full.error, QueueError::Full);
        for rejection in [capped, full] {
            assert_eq!(rejection.status(), 429);
            assert!(rejection.retry_after_seconds() > 0, "429 without a schedule");
            let detail = rejection.detail();
            assert_eq!(detail["retryable"], json!(true));
            // The distinction an agent must not get wrong.
            assert_eq!(detail["codeProblem"], json!(false));
            assert!(detail["error"].as_str().unwrap().contains("retry"));
        }
        drop(gate);
        for worker in workers {
            worker.join().unwrap();
        }
    }

    #[test]
    fn queue_status_names_the_position_a_waiting_caller_is_in() {
        let queue = Arc::new(test_queue(1, 4, 4));
        let gate = queue.acquire(client(9), Some("running-1"), None).unwrap();
        let mut workers = Vec::new();
        for (index, id) in ["queued-1", "queued-2"].iter().enumerate() {
            let worker_queue = Arc::clone(&queue);
            let id = (*id).to_string();
            workers.push(thread::spawn(move || {
                drop(worker_queue.acquire(client(1), Some(&id), None).unwrap());
            }));
            await_depth(&queue, index + 1);
        }
        let running = queue.status("running-1");
        assert_eq!(running["state"], json!("running"));
        let first = queue.status("queued-1");
        assert_eq!(first["state"], json!("queued"));
        assert_eq!(first["position"], json!(1));
        let second = queue.status("queued-2");
        assert_eq!(second["position"], json!(2));
        assert_eq!(second["waiting"], json!(2));
        assert_eq!(queue.status("never-heard-of-it")["state"], json!("unknown"));
        drop(gate);
        for worker in workers {
            worker.join().unwrap();
        }
    }

    #[test]
    fn a_shed_mcp_tool_call_reads_as_transient_capacity_not_as_bad_input() {
        // What an AI client actually receives. It must be a tool error (so the
        // model sees it at all), it must say retry, and it must rule out the
        // interpretation that sends an agent rewriting working geometry.
        let queue = test_queue(1, 1, 1);
        let gate = queue.acquire(client(1), None, None).unwrap();
        let waiter = thread::spawn({
            let queue = queue.clone();
            move || drop(queue.acquire(client(2), None, None).unwrap())
        });
        // Fill the single waiting place, then shed the next call.
        for _ in 0..2_000 {
            if queue.inner.state.lock().unwrap().depth == 1 {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let rejection = queue.acquire(client(3), None, None).unwrap_err();
        let result = mcp_queue_rejection(rejection);
        assert_eq!(result["isError"], json!(true));
        let structured = &result["structuredContent"];
        assert_eq!(structured["reason"], json!("queue_full"));
        assert_eq!(structured["retryable"], json!(true));
        assert_eq!(structured["codeProblem"], json!(false));
        assert!(structured["retryAfterSeconds"].as_u64().unwrap() > 0);
        assert_eq!(structured["queue"]["maxQueued"], json!(1));
        let prose = structured["error"].as_str().unwrap();
        assert!(prose.contains("transient"), "{prose}");
        assert!(prose.contains("nothing about the SCAD code is wrong"), "{prose}");
        assert!(
            structured["nextStep"]
                .as_str()
                .unwrap()
                .contains("Do not modify the SCAD source"),
            "an agent must be told explicitly not to 'fix' the model"
        );
        // The text content mirrors the structured payload, for clients that
        // only read `content`.
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("retryable"));
        drop(gate);
        waiter.join().unwrap();
    }

    #[test]
    fn cancellable_jobs_reject_duplicate_ids_and_unregister_on_drop() {
        let (state, root) = temporary_state(None);
        let (signal, registration) = register_cancellation(&state, "workspace-render-1").unwrap();
        assert!(register_cancellation(&state, "workspace-render-1").is_err());
        let registered = state
            .cancellations
            .lock()
            .unwrap()
            .get("workspace-render-1")
            .cloned()
            .unwrap();
        registered.store(true, Ordering::Release);
        assert!(signal.load(Ordering::Acquire));
        drop(registration);
        assert!(!state
            .cancellations
            .lock()
            .unwrap()
            .contains_key("workspace-render-1"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspaces_have_funny_unpredictable_capability_ids_and_persist() {
        let (store, root) = temporary_store();
        let first = store
            .create("cube(1);".into(), Some("Fixture".into()))
            .unwrap();
        let second = store.create("sphere(2);".into(), None).unwrap();
        assert_ne!(first.id, second.id);
        assert!(valid_workspace_id(&first.id));
        assert!(first.id.split('-').count() >= 3);
        drop(store);

        let reopened = WorkspaceStore::open(root.clone()).unwrap();
        let loaded = reopened.get(&first.id).unwrap().unwrap();
        assert_eq!(loaded.code, "cube(1);");
        assert_eq!(loaded.name, "Fixture");
        #[cfg(unix)]
        {
            assert_eq!(
                fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(root.join(format!("{}.json", first.id)))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn range_updates_are_validated_atomic_and_revision_checked() {
        let (store, root) = temporary_store();
        let workspace = store.create("cube(1);".into(), None).unwrap();
        let updated = store
            .update(
                &workspace.id,
                WorkspaceUpdate {
                    code: None,
                    name: None,
                    start: Some(5),
                    end: Some(6),
                    text: Some("12".into()),
                    expected_revision: Some(1),
                    base_revision: None,
                    plate: None,
                    tolerance: None,
                    source: "ai".into(),
                },
            )
            .unwrap();
        assert_eq!(updated.workspace.code, "cube(12);");
        assert_eq!(updated.workspace.revision, 2);
        assert_eq!(updated.event.source, "ai");
        assert_eq!(updated.event.workspace_id, workspace.id);
        assert!(matches!(
            store.update(
                &workspace.id,
                WorkspaceUpdate {
                    code: Some("sphere(3);".into()),
                    name: None,
                    start: None,
                    end: None,
                    text: None,
                    expected_revision: Some(1),
                    base_revision: None,
                    plate: None,
                    tolerance: None,
                    source: "browser".into(),
                }
            ),
            Err(UpdateError::Conflict(_))
        ));
        assert_eq!(store.get(&workspace.id).unwrap().unwrap().code, "cube(12);");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn every_update_requires_an_optimistic_revision_token() {
        let (store, root) = temporary_store();
        let workspace = store.create("cube(1);".into(), None).unwrap();
        let result = store.update(
            &workspace.id,
            WorkspaceUpdate {
                code: Some("cube(2);".into()),
                name: None,
                start: None,
                end: None,
                text: None,
                expected_revision: None,
                base_revision: None,
                plate: None,
                tolerance: None,
                source: "browser".into(),
            },
        );
        assert!(
            matches!(result, Err(UpdateError::Invalid(message)) if message.contains("required"))
        );
        assert_eq!(store.get(&workspace.id).unwrap().unwrap().revision, 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_updates_do_not_mutate_persisted_source() {
        let (store, root) = temporary_store();
        let workspace = store.create("cube(1);".into(), None).unwrap();
        let result = store.update(
            &workspace.id,
            WorkspaceUpdate {
                code: Some("cube(;".into()),
                name: None,
                start: None,
                end: None,
                text: None,
                expected_revision: Some(1),
                base_revision: None,
                plate: None,
                tolerance: None,
                source: "ai".into(),
            },
        );
        assert!(matches!(result, Err(UpdateError::Validation(_))));
        assert_eq!(store.get(&workspace.id).unwrap().unwrap().revision, 1);
        drop(store);
        let loaded = WorkspaceStore::open(root.clone())
            .unwrap()
            .get(&workspace.id)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.code, "cube(1);");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn initialize_negotiates_down_instead_of_rejecting_newer_clients() {
        // Versions we implement are echoed back verbatim, per the lifecycle spec.
        for supported in SUPPORTED_MCP_PROTOCOLS {
            assert_eq!(negotiated_mcp_protocol(supported), *supported);
        }
        // Anything newer — or older, or nonsense — gets our newest revision rather
        // than an error, so the client keeps its chance to fall back. Rejecting is
        // what made real clients (Claude Code) fail to connect at all.
        let newest = SUPPORTED_MCP_PROTOCOLS[SUPPORTED_MCP_PROTOCOLS.len() - 1];
        for requested in ["2026-07-28", "2025-11-25", "2024-11-05", "not-a-version", ""] {
            assert_eq!(negotiated_mcp_protocol(requested), newest);
        }
        // The list stays newest-last, or the line above hands out the oldest revision.
        let mut sorted = SUPPORTED_MCP_PROTOCOLS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, SUPPORTED_MCP_PROTOCOLS.to_vec());
    }

    #[test]
    fn absent_protocol_version_header_is_legal_but_an_unsupported_one_is_not() {
        // `initialize` is the request that *decides* the version, so no header
        // rule can apply to it — not even a nonsense one.
        assert_eq!(unsupported_legacy_header("initialize", None), None);
        assert_eq!(
            unsupported_legacy_header("initialize", Some("not-a-version")),
            None
        );

        // Absent header: clients older than 2025-06-18 never send it, and the
        // spec says to assume 2025-03-26 rather than reject. Rejecting here is
        // the bug this test exists to keep fixed.
        assert_eq!(unsupported_legacy_header("tools/list", None), None);

        // Every revision we negotiate must be accepted back as a header, or a
        // client that negotiated down would be cut off on its very next call.
        for supported in SUPPORTED_MCP_PROTOCOLS {
            assert_eq!(unsupported_legacy_header("tools/list", Some(supported)), None);
        }

        // Present but unspeakable: refused, and the caller turns this into the
        // 400 the spec mandates. `2026-07-28` counts as unsupported *here*
        // because a bare legacy request cannot be served under modern rules.
        for bad in ["2024-11-05", "2026-07-28", "not-a-version", ""] {
            assert_eq!(
                unsupported_legacy_header("tools/list", Some(bad)),
                Some(bad),
                "header {bad:?} should be refused"
            );
        }
    }

    #[test]
    fn era_follows_how_the_client_opens_not_which_method_it_calls() {
        let modern = json!({"_meta": {MCP_META_PROTOCOL_VERSION: "2026-07-28"}});
        let bare = json!({});

        // `initialize` is the legacy opener and stays legacy even if a confused
        // client decorates it with modern metadata.
        assert_eq!(mcp_era("initialize", &bare), McpEra::Legacy);
        assert_eq!(mcp_era("initialize", &modern), McpEra::Legacy);
        assert_eq!(mcp_era("notifications/initialized", &bare), McpEra::Legacy);

        // A legacy post-handshake call carries no modern `_meta`, so the browser
        // UI's `tools/list` keeps its byte-compatible legacy responses.
        assert_eq!(mcp_era("tools/list", &bare), McpEra::Legacy);
        assert_eq!(
            mcp_era("tools/call", &json!({"_meta": {"progressToken": 7}})),
            McpEra::Legacy
        );

        // The modern `_meta` protocol version is the modern opener, and
        // `server/discover` exists only in the modern era, so a bare probe for
        // it must still be answered with a modern error rather than -32601.
        assert_eq!(mcp_era("tools/list", &modern), McpEra::Modern);
        assert_eq!(mcp_era("server/discover", &bare), McpEra::Modern);
        assert_eq!(mcp_era("server/discover", &modern), McpEra::Modern);
    }

    #[test]
    fn discover_advertises_versions_capabilities_and_identity() {
        let result = mcp_discover_result();

        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["supportedVersions"], json!(["2026-07-28"]));
        // Only modern revisions are listed: a client picking one of these keeps
        // sending modern `_meta`, which the legacy revisions cannot serve.
        for version in SUPPORTED_MCP_PROTOCOLS {
            assert!(!result["supportedVersions"]
                .as_array()
                .unwrap()
                .contains(&json!(version)));
        }
        assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
        assert_eq!(result["capabilities"]["resources"]["subscribe"], false);
        assert_eq!(result["_meta"][MCP_META_SERVER_INFO]["name"], "reopenscad");
        assert!(result["instructions"].as_str().unwrap().contains("SCAD"));
        // `server/discover` is a cacheable result like the lists.
        assert_eq!(result["ttlMs"], json!(MCP_STATIC_CACHE_TTL_MS));
        assert_eq!(result["cacheScope"], "public");
    }

    #[test]
    fn unsupported_version_reports_what_the_client_can_retry_with() {
        assert_eq!(MCP_UNSUPPORTED_PROTOCOL_VERSION, -32022);
        let data = unsupported_protocol_version_data("1900-01-01");
        assert_eq!(
            data,
            json!({"supported": ["2026-07-28"], "requested": "1900-01-01"})
        );
        // Both MCP codes live in the range the spec reserves for itself.
        for code in [MCP_HEADER_MISMATCH, MCP_UNSUPPORTED_PROTOCOL_VERSION] {
            assert!((-32099..=-32020).contains(&code));
        }
    }

    #[test]
    fn header_mirroring_is_validated_against_the_body() {
        let call = json!({
            "name": "render_workspace",
            "_meta": {MCP_META_PROTOCOL_VERSION: "2026-07-28"}
        });
        let ok = |params: &Value, protocol, method, name| {
            validate_modern_mcp_headers(
                "tools/call",
                params,
                Some(protocol),
                Some(method),
                Some(name),
            )
        };

        assert!(ok(&call, "2026-07-28", "tools/call", "render_workspace").is_ok());

        // Every standard header is required.
        assert!(validate_modern_mcp_headers(
            "tools/call",
            &call,
            None,
            Some("tools/call"),
            Some("render_workspace")
        )
        .is_err());
        assert!(validate_modern_mcp_headers(
            "tools/call",
            &call,
            Some("2026-07-28"),
            None,
            Some("render_workspace")
        )
        .is_err());
        assert!(validate_modern_mcp_headers(
            "tools/call",
            &call,
            Some("2026-07-28"),
            Some("tools/call"),
            None
        )
        .is_err());

        // A header that disagrees with the body is the case the rule exists for:
        // a gateway routing on the header must not reach a different decision
        // than the server executing the body.
        assert!(ok(&call, "2025-06-18", "tools/call", "render_workspace").is_err());
        assert!(ok(&call, "2026-07-28", "tools/list", "render_workspace").is_err());
        assert!(ok(&call, "2026-07-28", "tools/call", "create_workspace").is_err());

        // `Mcp-Name` mirrors `params.uri` for reads and is absent for the lists.
        let read = json!({
            "uri": SCAD_DICTIONARY_URI,
            "_meta": {MCP_META_PROTOCOL_VERSION: "2026-07-28"}
        });
        assert_eq!(
            mcp_name_body_value("resources/read", &read),
            Some(SCAD_DICTIONARY_URI)
        );
        assert_eq!(mcp_name_body_value("tools/list", &read), None);
        assert!(validate_modern_mcp_headers(
            "resources/read",
            &read,
            Some("2026-07-28"),
            Some("resources/read"),
            Some(SCAD_DICTIONARY_URI)
        )
        .is_ok());
        assert!(validate_modern_mcp_headers(
            "tools/list",
            &json!({"_meta": {MCP_META_PROTOCOL_VERSION: "2026-07-28"}}),
            Some("2026-07-28"),
            Some("tools/list"),
            None
        )
        .is_ok());

        // A name outside the header-safe ASCII set travels base64-wrapped, and
        // must be decoded before the comparison.
        let unicode = json!({
            "uri": "scad://reopenscad/wörld",
            "_meta": {MCP_META_PROTOCOL_VERSION: "2026-07-28"}
        });
        let encoded = format!("=?base64?{}?=", base64_encode("scad://reopenscad/wörld".as_bytes()));
        assert!(validate_modern_mcp_headers(
            "resources/read",
            &unicode,
            Some("2026-07-28"),
            Some("resources/read"),
            Some(&encoded)
        )
        .is_ok());
        assert!(validate_modern_mcp_headers(
            "resources/read",
            &unicode,
            Some("2026-07-28"),
            Some("resources/read"),
            Some("=?base64?not base64?=")
        )
        .is_err());
    }

    #[test]
    fn decodes_the_base64_header_sentinel() {
        assert_eq!(
            decode_mcp_header_value("render_workspace").as_deref(),
            Some("render_workspace")
        );
        assert_eq!(
            decode_mcp_header_value("=?base64?SGVsbG8sIOS4lueVjA==?=").as_deref(),
            Some("Hello, 世界")
        );
        assert_eq!(decode_mcp_header_value("=?base64?%%%%?="), None);
        for original in ["", "a", "ab", "abc", "ReOpenSCAD", "scad://x/ü"] {
            let round_tripped = base64_decode(&base64_encode(original.as_bytes())).unwrap();
            assert_eq!(round_tripped, original.as_bytes());
        }
    }

    #[test]
    fn modern_results_carry_result_type_and_cache_hints() {
        // Ordinary results only gain `resultType` plus the server identity that
        // used to arrive once, at `initialize`.
        let call = modern_mcp_result(json!({"content": [], "isError": false}));
        assert_eq!(call["resultType"], "complete");
        assert_eq!(
            call["_meta"][MCP_META_SERVER_INFO]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert!(call.get("ttlMs").is_none());
        assert!(call.get("cacheScope").is_none());

        // The cacheable ones additionally MUST carry both hints.
        let list = cacheable_mcp_result(
            json!({"tools": mcp_tool_definitions()}),
            MCP_STATIC_CACHE_TTL_MS,
            "public",
        );
        assert_eq!(list["resultType"], "complete");
        assert_eq!(list["ttlMs"], json!(MCP_STATIC_CACHE_TTL_MS));
        assert_eq!(list["cacheScope"], "public");
        assert!(list["ttlMs"].as_u64().is_some(), "ttlMs must be >= 0");

        // A workspace read is per-workspace and changes on every edit, so it is
        // never shared across callers and never considered fresh.
        let read = cacheable_mcp_result(json!({"contents": []}), 0, "private");
        assert_eq!(read["ttlMs"], json!(0));
        assert_eq!(read["cacheScope"], "private");
        for scope in [&list["cacheScope"], &read["cacheScope"]] {
            assert!(matches!(scope.as_str(), Some("public" | "private")));
        }

        // Legacy results are untouched: no `resultType`, no `_meta`, no hints.
        let legacy = json!({"tools": mcp_tool_definitions()});
        assert!(legacy.get("resultType").is_none());
        assert!(legacy.get("_meta").is_none());
    }

    #[test]
    fn exposes_complete_mcp_workspace_tool_contract() {
        let definitions = mcp_tool_definitions();
        let names: Vec<_> = definitions
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect();
        assert_eq!(
            names,
            vec![
                "create_workspace",
                "get_supported_scad",
                "get_workspace_code",
                "update_workspace_code",
                "render_workspace",
                "list_workspace_objects",
                "get_workspace_link",
                "check_intersections"
            ]
        );
        assert!(definitions
            .iter()
            .all(|tool| tool.get("inputSchema").is_some()));
        let update = definitions
            .iter()
            .find(|tool| tool["name"] == "update_workspace_code")
            .unwrap();
        assert!(update["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("expectedRevision")));
        let render = definitions
            .iter()
            .find(|tool| tool["name"] == "render_workspace")
            .unwrap();
        assert!(render["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("revision")));
    }

    #[test]
    fn parses_workspace_routes_and_query_ranges() {
        let id = "fuzzy-wombat-00112233445566778899aabbccdd";
        assert_eq!(
            workspace_api_route(&format!("/api/workspaces/{id}/events")),
            Some((id, "events"))
        );
        let parameters = query_parameters("since=42&format=stl");
        assert_eq!(query_u64(&parameters, "since"), Some(42));
        assert_eq!(parameters.get("format").map(String::as_str), Some("stl"));
        assert_eq!(
            workspace_shell_id("/workspaces/tidy-rocket-jy8p"),
            Some("tidy-rocket-jy8p")
        );
        assert!(workspace_shell_id("/workspaces/../../backend/src/main.rs").is_none());
        let request = Request {
            method: "GET".into(),
            path: format!("/api/workspaces/{id}"),
            query: "allowMissing=1".into(),
            headers: vec![],
            body: vec![],
        };
        assert!(allow_missing_workspace(&request));
    }

    #[test]
    fn export_mesh_filters_by_object_id_on_both_export_endpoints() {
        let source = "cube(2); translate([4,0,0]) cube(2);";
        // Triangle counts cannot separate the two: the mesher adapts its grid to the bounds it is
        // given, so a single small part can carry more facets than the whole model.
        let lowest_x = |mesh: &engine::Mesh| {
            mesh.triangles
                .iter()
                .flat_map(|triangle| triangle.vertices)
                .fold(f64::INFINITY, |lowest, vertex| lowest.min(vertex.x))
        };
        let whole = export_mesh(source, None).unwrap();
        let subset = export_mesh(source, Some(&["object-2".into()])).unwrap();
        assert!(!whole.triangles.is_empty());
        assert!(lowest_x(&whole) < 1.0);
        assert!(lowest_x(&subset) > 3.0);
        assert!(export_mesh(source, Some(&["model".into()]))
            .unwrap_err()
            .contains("Unknown"));
    }

    #[test]
    fn every_advertised_export_format_encodes_and_3mf_is_a_zip_package() {
        let mesh = export_mesh("cube(2);", None).unwrap();
        assert!(is_supported_export_format("3mf"));
        assert!(!is_supported_export_format("amf"));
        for format in EXPORT_FORMATS {
            let data = encode_mesh(&mesh, format, "unit test");
            assert!(!data.is_empty(), "{format} produced no bytes");
        }
        // The tool schema and the validator must not drift apart: a format the
        // schema offers has to be one `encode_mesh` can actually produce.
        let render_tool = mcp_tool_definitions()
            .into_iter()
            .find(|tool| tool["name"] == "render_workspace")
            .expect("render_workspace tool");
        let advertised = render_tool["inputSchema"]["properties"]["format"]["enum"]
            .as_array()
            .expect("format enum")
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(advertised, EXPORT_FORMATS.map(String::from).to_vec());

        let package = encode_mesh(&mesh, "3mf", "unit test");
        assert_eq!(&package[..4], b"PK\x03\x04", "3MF must be a ZIP container");
        assert_eq!(export_content_type("3mf"), "model/3mf");
    }

    #[test]
    fn mcp_exposes_object_discovery_and_the_embedded_app() {
        let names: Vec<_> = mcp_tool_definitions()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect();
        assert!(
            names.contains(&"list_workspace_objects".to_string()),
            "an MCP client has no other way to learn object IDs"
        );
        let template = mcp_resource_templates();
        assert_eq!(
            template[0]["uriTemplate"],
            json!("ui://reopenscad/workspace/{workspaceId}")
        );
        assert_eq!(template[0]["mimeType"], json!("text/html"));
        // Workspace URLs are access capabilities, so the concrete listing must
        // never enumerate them.
        for resource in mcp_resource_definitions() {
            assert!(!resource["uri"]
                .as_str()
                .unwrap()
                .starts_with(WORKSPACE_APP_URI_PREFIX));
        }
    }

    #[test]
    fn intersection_reports_carry_the_location_of_the_extremum() {
        let output = engine::compile_parts(
            "cube(10); translate([6,2,2]) cube(8);",
            engine::Quality::Preview,
            None,
        )
        .unwrap();
        let intersections = engine::check_part_clearances(&output.parts, 0.2, &[]).unwrap();
        let report = intersection_public_located(&intersections[0], &output.parts);
        assert_eq!(report["intersects"], json!(true));
        let location = report["location"].as_array().expect("a witness point");
        assert_eq!(location.len(), 3);
        // The witness must sit inside the shared bounding box of the two parts.
        let overlap = &report["overlapBounds"];
        for axis in 0..3 {
            let value = location[axis].as_f64().unwrap();
            let low = overlap["min"][axis].as_f64().unwrap();
            let high = overlap["max"][axis].as_f64().unwrap();
            assert!(
                value >= low - 1e-6 && value <= high + 1e-6,
                "axis {axis}: {value} outside [{low}, {high}]"
            );
        }
        assert!(report.get("aSourceRange").is_some());
    }

    #[test]
    fn selected_exports_validate_ids_and_preserve_union_semantics() {
        let output = engine::compile_parts(
            "cube(2); translate([4,0,0]) cube(2);",
            engine::Quality::Preview,
            None,
        )
        .unwrap();
        assert!(selected_parts_mesh(&output, Some(&[]), engine::Quality::Preview, None).is_err());
        assert!(selected_parts_mesh(
            &output,
            Some(&["missing".into()]),
            engine::Quality::Preview,
            None
        )
        .unwrap_err()
        .contains("Unknown"));
        assert!(selected_parts_mesh(
            &output,
            Some(&["object-1".into(), "object-1".into()]),
            engine::Quality::Preview,
            None
        )
        .is_err());
        let subset = selected_parts_mesh(
            &output,
            Some(&["object-1".into()]),
            engine::Quality::Preview,
            None,
        )
        .unwrap();
        assert!(!subset.triangles.is_empty());
        assert_ne!(subset.binary_stl(), output.mesh.binary_stl());
        let all = vec!["object-1".into(), "object-2".into()];
        assert_eq!(
            selected_parts_mesh(&output, Some(&all), engine::Quality::Preview, None)
                .unwrap()
                .binary_stl(),
            output.mesh.binary_stl()
        );
        let (state, root) = temporary_state(None);
        let request = Request {
            method: "POST".into(),
            path: "/mcp".into(),
            query: String::new(),
            headers: vec![("host".into(), "127.0.0.1:5173".into())],
            body: vec![],
        };
        assert!(workspace_export_url(
            &state,
            &request,
            "fuzzy-wombat-00112233445566778899aabbccdd",
            "stl",
            7,
            Some(&["object-1".into(), "object-2".into()])
        )
        .ends_with("format=stl&revision=7&objectIds=object-1,object-2"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn intersection_visibility_rejects_unknown_or_malformed_ids() {
        let output = engine::compile_parts(
            "cube(2); translate([4,0,0]) cube(2);",
            engine::Quality::Preview,
            None,
        )
        .unwrap();
        assert!(
            hidden_part_ids(&output.parts, &json!({"visibleObjectIds": ["missing"]}))
                .unwrap_err()
                .contains("Unknown")
        );
        assert!(hidden_part_ids(&output.parts, &json!({"visibleObjectIds": [42]})).is_err());
        assert_eq!(
            hidden_part_ids(&output.parts, &json!({"visibleObjectIds": ["object-1"]})).unwrap(),
            vec!["object-2"]
        );
    }

    #[test]
    fn public_origins_are_explicit_and_path_free() {
        assert_eq!(
            validate_public_origin("https://cad.example/").unwrap(),
            "https://cad.example"
        );
        assert!(validate_public_origin("https://cad.example/path").is_err());
        assert!(validate_public_origin("javascript:alert(1)").is_err());
    }

    // -----------------------------------------------------------------------
    // Denial-of-service guard rails.
    // -----------------------------------------------------------------------

    /// Feeds `payload` to `read_request` over a real loopback socket.
    fn parse_over_socket(payload: &[u8]) -> Result<Request, RequestError> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let payload = payload.to_vec();
        let client = thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(address).unwrap();
            // The server may bail mid-stream; a broken pipe is expected then.
            let _ = stream.write_all(&payload);
            let _ = stream.flush();
            let mut sink = Vec::new();
            let _ = stream.read_to_end(&mut sink);
        });
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let parsed = read_request(&stream);
        drop(stream);
        let _ = client.join();
        parsed
    }

    #[test]
    fn accepts_an_ordinary_request_after_hardening() {
        let request =
            parse_over_socket(b"POST /api/render HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello")
                .unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/render");
        assert_eq!(request.body, b"hello");
    }

    #[test]
    fn rejects_oversized_header_blocks() {
        let mut payload = b"GET /api/health HTTP/1.1\r\nHost: x\r\n".to_vec();
        // Comfortably past MAX_HEADER_BYTES but far below what the old
        // unbounded loop would happily buffer.
        for index in 0..64 {
            payload.extend_from_slice(format!("X-Pad-{index}: {}\r\n", "A".repeat(1024)).as_bytes());
        }
        payload.extend_from_slice(b"\r\n");
        let error = parse_over_socket(&payload).unwrap_err();
        assert_eq!(error.status, 431);
        assert!(error.message.contains("16 KiB"), "{}", error.message);
    }

    #[test]
    fn rejects_a_single_oversized_header_line() {
        let mut payload = b"GET /api/health HTTP/1.1\r\n".to_vec();
        payload.extend_from_slice(b"X-Pad: ");
        payload.extend_from_slice(&vec![b'A'; MAX_HEADER_BYTES + 4096]);
        payload.extend_from_slice(b"\r\n\r\n");
        assert_eq!(parse_over_socket(&payload).unwrap_err().status, 431);
    }

    #[test]
    fn rejects_too_many_headers() {
        let mut payload = b"GET /api/health HTTP/1.1\r\n".to_vec();
        for index in 0..(MAX_HEADERS + 10) {
            payload.extend_from_slice(format!("X-N-{index}: 1\r\n").as_bytes());
        }
        payload.extend_from_slice(b"\r\n");
        let error = parse_over_socket(&payload).unwrap_err();
        assert_eq!(error.status, 431);
        assert!(error.message.contains("Too many"), "{}", error.message);
    }

    #[test]
    fn rejects_an_oversized_request_line() {
        let mut payload = b"GET /".to_vec();
        payload.extend_from_slice(&vec![b'a'; MAX_REQUEST_LINE + 1024]);
        payload.extend_from_slice(b" HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(parse_over_socket(&payload).unwrap_err().status, 414);
    }

    #[test]
    fn rejects_an_oversized_declared_body() {
        let payload = format!(
            "POST /api/render HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        // Rejected from the header itself: no bytes of the body are buffered.
        assert_eq!(
            parse_over_socket(payload.as_bytes()).unwrap_err().status,
            413
        );
    }

    #[test]
    fn connection_limiter_sheds_instead_of_growing() {
        let limiter = JobLimiter::new(3);
        let held: Vec<_> = (0..3).map(|_| limiter.try_acquire().unwrap()).collect();
        assert!(
            limiter.try_acquire().is_none(),
            "accept loop must shed past MAX_CONNECTIONS"
        );
        drop(held);
        assert!(limiter.try_acquire().is_some(), "slots must be reusable");
    }

    #[test]
    fn rate_limiter_throttles_request_floods() {
        let limiter = RateLimiter::new();
        let address: IpAddr = "203.0.113.7".parse().unwrap();
        let now = Instant::now();
        for _ in 0..(IP_REQUEST_BURST as usize) {
            assert!(limiter.admit_at(address, RouteCost::Cheap, now));
        }
        assert!(!limiter.admit_at(address, RouteCost::Cheap, now));
        // A different source is unaffected.
        let other: IpAddr = "203.0.113.8".parse().unwrap();
        assert!(limiter.admit_at(other, RouteCost::Cheap, now));
        // Tokens come back with time.
        let later = now + Duration::from_secs(2);
        assert!(limiter.admit_at(address, RouteCost::Cheap, later));
    }

    #[test]
    fn rate_limiter_is_stricter_on_expensive_routes() {
        let limiter = RateLimiter::new();
        let address: IpAddr = "198.51.100.4".parse().unwrap();
        let now = Instant::now();
        let allowed = (IP_HEAVY_BURST / HEAVY_COST_EVALUATOR) as usize;
        for _ in 0..allowed {
            assert!(limiter.admit_at(address, RouteCost::Evaluator, now));
        }
        assert!(!limiter.admit_at(address, RouteCost::Evaluator, now));
        // Cheap routes still work: the heavy bucket is separate.
        assert!(limiter.admit_at(address, RouteCost::Cheap, now));
    }

    #[test]
    fn rate_limiter_bills_bytes_served() {
        let limiter = RateLimiter::new();
        let address: IpAddr = "198.51.100.9".parse().unwrap();
        let now = Instant::now();
        assert!(limiter.admit_at(address, RouteCost::Evaluator, now));
        // One amplified response drains the whole byte budget.
        limiter.charge_bytes(address, IP_BYTE_BURST as u64 + 1);
        assert!(
            !limiter.admit_at(address, RouteCost::Cheap, now),
            "byte budget must gate the next request"
        );
    }

    #[test]
    fn rate_limiter_memory_is_bounded() {
        let limiter = RateLimiter::new();
        let now = Instant::now();
        for index in 0..(MAX_RATE_BUCKETS as u32 + 500) {
            let address = IpAddr::V4(std::net::Ipv4Addr::from(index));
            limiter.admit_at(address, RouteCost::Cheap, now);
        }
        assert!(
            limiter.tracked() <= MAX_RATE_BUCKETS,
            "tracked {} buckets",
            limiter.tracked()
        );
    }

    #[test]
    fn classifies_route_costs() {
        assert_eq!(route_cost("GET", "/index.html"), RouteCost::Cheap);
        assert_eq!(route_cost("GET", "/api/health"), RouteCost::Cheap);
        assert_eq!(route_cost("POST", "/api/render"), RouteCost::Evaluator);
        assert_eq!(route_cost("POST", "/api/export"), RouteCost::Evaluator);
        assert_eq!(route_cost("POST", "/mcp"), RouteCost::Evaluator);
        assert_eq!(route_cost("POST", "/api/workspaces"), RouteCost::Evaluator);
        let id = "sunny-otter-0011223344556677";
        assert_eq!(
            route_cost("GET", &format!("/api/workspaces/{id}/events")),
            RouteCost::Poll
        );
        assert_eq!(
            route_cost("POST", &format!("/api/workspaces/{id}/render")),
            RouteCost::Evaluator
        );
        assert_eq!(
            route_cost("PATCH", &format!("/api/workspaces/{id}")),
            RouteCost::Evaluator
        );
        assert_eq!(
            route_cost("GET", &format!("/api/workspaces/{id}")),
            RouteCost::Cheap
        );
    }

    #[test]
    fn events_payload_omits_source_when_nothing_changed() {
        let workspace = Workspace {
            id: "sunny-otter-0011223344556677".into(),
            name: "Untitled model".into(),
            code: "cube(10);".repeat(4096),
            revision: 7,
            created_at: 1,
            updated_at: 2,
            plate: String::new(),
            tolerance: DEFAULT_TOLERANCE,
            events: Vec::new(),
        };
        let idle = workspace_events_payload(&workspace.id, &workspace, 7);
        assert_eq!(idle["changed"], json!(false));
        assert!(
            idle.get("code").is_none(),
            "idle polls must not echo the workspace source"
        );
        assert_eq!(idle["revision"], json!(7));
        let changed = workspace_events_payload(&workspace.id, &workspace, 6);
        assert_eq!(changed["changed"], json!(true));
        assert_eq!(changed["code"], json!(workspace.code));
    }

    #[test]
    fn workspace_store_evicts_expired_entries() {
        let (store, root) = temporary_store();
        let workspace = store.create("cube(1);".into(), None).unwrap();
        let path = root.join(format!("{}.json", workspace.id));
        assert!(path.exists());
        {
            let mut inner = store.inner.lock().unwrap();
            let stale = unix_millis() - WORKSPACE_TTL_MS - 1;
            inner
                .index
                .insert(workspace.id.clone(), WorkspaceMeta { updated_at: stale });
            store.prune(&mut inner, 0);
        }
        assert_eq!(store.len(), 0);
        assert!(!path.exists(), "expired workspace files must be deleted");
        assert!(store.get(&workspace.id).unwrap().is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_store_caps_the_total_count() {
        let (store, root) = temporary_store();
        // Synthesise more workspaces than the cap without paying for a full
        // validate + fsync per entry.
        {
            let mut inner = store.inner.lock().unwrap();
            for index in 0..(MAX_WORKSPACES + 32) {
                let id = format!("sunny-otter-{index:016x}0000");
                assert!(valid_workspace_id(&id), "{id}");
                fs::write(root.join(format!("{id}.json")), b"{}").unwrap();
                inner.index.insert(
                    id,
                    WorkspaceMeta {
                        updated_at: unix_millis() + index as u64,
                    },
                );
            }
            store.prune(&mut inner, 0);
        }
        assert_eq!(store.len(), MAX_WORKSPACES);
        let survivors = fs::read_dir(&root).unwrap().count();
        assert_eq!(survivors, MAX_WORKSPACES, "oldest files must be unlinked");
        // Creating one more still leaves headroom rather than overflowing.
        store.create("cube(1);".into(), None).unwrap();
        assert!(store.len() <= MAX_WORKSPACES);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_store_keeps_only_a_bounded_cache_resident() {
        let (store, root) = temporary_store();
        let mut ids = Vec::new();
        for _ in 0..(MAX_CACHED_WORKSPACES + 8) {
            ids.push(store.create("cube(1);".into(), None).unwrap().id);
        }
        {
            let inner = store.inner.lock().unwrap();
            assert_eq!(inner.index.len(), ids.len());
            assert!(
                inner.cache.len() <= MAX_CACHED_WORKSPACES,
                "resident cache grew to {}",
                inner.cache.len()
            );
            assert!(!inner.cache.contains_key(&ids[0]), "coldest must be evicted");
        }
        // Evicted from RAM but still readable from disk.
        let reloaded = store.get(&ids[0]).unwrap().expect("workspace on disk");
        assert_eq!(reloaded.code, "cube(1);");
        assert_eq!(reloaded.revision, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_store_survives_a_restart_without_loading_every_source() {
        let (store, root) = temporary_store();
        let created = store.create("cube(3);".into(), None).unwrap();
        let reopened = WorkspaceStore::open(root.clone()).unwrap();
        {
            let inner = reopened.inner.lock().unwrap();
            assert_eq!(inner.index.len(), 1);
            assert!(inner.cache.is_empty(), "boot must not load sources into RAM");
        }
        let loaded = reopened.get(&created.id).unwrap().expect("persisted");
        assert_eq!(loaded.code, "cube(3);");
        let _ = fs::remove_dir_all(root);
    }

    /// The web root doubles as the document root, so the deployment files that
    /// now live beside `index.html` would otherwise be downloadable — and
    /// `docker-compose.yml` carries a database password.
    #[test]
    fn deployment_files_are_not_served_from_the_web_root() {
        for private in [
            "Dockerfile",
            "dockerfile",
            "docker-compose.yml",
            "docker-compose.yaml",
            ".dockerignore",
            "DEPLOY.md",
            ".env",
            ".env.production",
            "package.json",
        ] {
            assert!(is_private_web_root_file(private), "{private} was servable");
        }
        // Whatever the shell actually needs must still resolve.
        for public in [
            "index.html",
            "app.js",
            "styles.css",
            "legacy-assets/icons/reopenscad.svg",
            "README.md",
        ] {
            assert!(!is_private_web_root_file(public), "{public} was blocked");
        }
    }

    fn forwarded(values: &[&str]) -> Vec<(String, String)> {
        values
            .iter()
            .map(|value| ("x-forwarded-for".to_string(), value.to_string()))
            .collect()
    }

    /// The default deployment has no proxy, so the header must not be able to
    /// influence which rate-limit bucket a caller lands in.
    #[test]
    fn forwarded_headers_are_ignored_without_a_configured_hop_count() {
        let headers = forwarded(&["203.0.113.9"]);
        assert_eq!(client_ip_from_chain(&headers, 0), None);
    }

    /// A client can prepend anything it likes to `X-Forwarded-For`; only the
    /// entries the trusted proxies appended, counted from the right, are real.
    /// Without this the header is an unlimited supply of rate-limit buckets.
    #[test]
    fn a_spoofed_forwarded_prefix_cannot_choose_the_rate_limit_bucket() {
        // What Cloud Run passes on when the caller sent its own header: the
        // forged value first, the address the front end observed appended last.
        let headers = forwarded(&["1.1.1.1, 9.9.9.9, 198.51.100.4"]);
        assert_eq!(
            client_ip_from_chain(&headers, 1),
            Some("198.51.100.4".parse().unwrap())
        );
    }

    /// Behind an external HTTPS load balancer the chain gains one more hop, so
    /// the client address moves one place left.
    #[test]
    fn two_hops_reads_past_the_load_balancer_entry() {
        let headers = forwarded(&["198.51.100.4, 130.211.0.1"]);
        assert_eq!(
            client_ip_from_chain(&headers, 2),
            Some("198.51.100.4".parse().unwrap())
        );
    }

    /// Repeated headers are one logical list, and a chain shorter than the
    /// configured hop count means the request did not arrive the way the
    /// deployment claims, so it falls back to the socket address.
    #[test]
    fn split_headers_join_and_a_short_chain_falls_back() {
        let headers = forwarded(&["198.51.100.4", "130.211.0.1"]);
        assert_eq!(
            client_ip_from_chain(&headers, 2),
            Some("198.51.100.4".parse().unwrap())
        );
        assert_eq!(client_ip_from_chain(&headers, 5), None);
    }

    /// Proxies emit ports and bracketed IPv6 literals; both have to collapse
    /// to the same bucket key as the bare address would.
    #[test]
    fn forwarded_entries_normalise_ports_and_ipv6_forms() {
        assert_eq!(
            client_ip_from_chain(&forwarded(&["198.51.100.4:41234"]), 1),
            Some("198.51.100.4".parse().unwrap())
        );
        assert_eq!(
            client_ip_from_chain(&forwarded(&["[2001:db8::1]:443"]), 1),
            Some("2001:db8::1".parse().unwrap())
        );
        assert_eq!(
            client_ip_from_chain(&forwarded(&["2001:db8::1"]), 1),
            Some("2001:db8::1".parse().unwrap())
        );
        // IPv4-mapped IPv6 collapses, so one client cannot hold two buckets.
        assert_eq!(
            client_ip_from_chain(&forwarded(&["::ffff:198.51.100.4"]), 1),
            Some("198.51.100.4".parse().unwrap())
        );
    }

    /// Garbage in the trusted position must not be silently promoted to some
    /// other address; it degrades to the socket address instead.
    #[test]
    fn an_unparseable_forwarded_entry_falls_back() {
        assert_eq!(client_ip_from_chain(&forwarded(&["unknown"]), 1), None);
        assert_eq!(client_ip_from_chain(&[], 1), None);
    }

    // -----------------------------------------------------------------------
    // Workspace URL confidentiality.
    //
    // The URL is the credential, so the headers that stop it being republished
    // are load-bearing security controls, not hygiene. They are also exactly
    // the kind of thing that disappears in a refactor without any test going
    // red, so these drive a real socket through `serve_request`.
    // -----------------------------------------------------------------------

    /// The repository's `web/` directory: the document root the deployed
    /// binary is pointed at, so the shell under test is the shipped one.
    fn web_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("backend/ has a parent")
            .to_path_buf()
    }

    /// Runs one raw request through the real request pipeline and returns the
    /// whole response, headers included.
    fn serve_over_socket(raw: &str) -> String {
        let (mut state, root) = temporary_state(None);
        state.web_root = web_root();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let raw = raw.to_string();
        let client = thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(address).unwrap();
            stream.write_all(raw.as_bytes()).unwrap();
            stream.flush().unwrap();
            let mut response = Vec::new();
            let _ = stream.read_to_end(&mut response);
            String::from_utf8_lossy(&response).into_owned()
        });
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        serve_request(&mut stream, &state, &mut None).unwrap();
        drop(stream);
        let response = client.join().unwrap();
        let _ = fs::remove_dir_all(root);
        response
    }

    fn get(path: &str) -> String {
        serve_over_socket(&format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:5173\r\n\r\n"))
    }

    /// Header block only. The shell's own text talks about these headers, so a
    /// whole-response substring search would find them in the body.
    fn get_headers(path: &str) -> String {
        let response = get(path);
        response
            .split_once("\r\n\r\n")
            .map(|(headers, _)| headers.to_string())
            .unwrap_or(response)
    }

    /// The header that stops a workspace path travelling to a third party in
    /// `Referer`. It has to be on *every* response, because the shell's own
    /// subresources and any future outbound link inherit it from the document.
    #[test]
    fn every_response_suppresses_the_referrer() {
        for path in ["/", "/app.js", "/api/health", "/robots.txt", "/nope"] {
            assert!(
                get_headers(path).contains("\r\nReferrer-Policy: no-referrer"),
                "{path} did not suppress the referrer"
            );
        }
    }

    /// Deny-by-default: a route has to be named indexable to lose the tag, so
    /// a route added later is hidden rather than exposed.
    #[test]
    fn only_the_landing_page_is_indexable() {
        let tag = format!("\r\nX-Robots-Tag: {ROBOTS_TAG}");
        for path in [
            "/workspaces/wobbly-wombat-ec5e6bd2b1eaa013ccba04c67f88",
            "/api/health",
            "/?workspace=wobbly-wombat-ec5e6bd2b1eaa013ccba04c67f88",
            "/app.js",
            "/nope",
        ] {
            assert!(get_headers(path).contains(&tag), "{path} was left indexable");
        }
        assert!(!get_headers("/").contains("X-Robots-Tag"));
        assert!(!get_headers("/index.html").contains("X-Robots-Tag"));
    }

    /// Both halves of the same predicate: the shell carries `noindex` on the
    /// workspace route and only the landing page has it removed.
    #[test]
    fn the_shell_hides_workspaces_and_shows_the_front_door() {
        assert!(
            get("/workspaces/wobbly-wombat-ec5e6bd2b1eaa013ccba04c67f88").contains(ROBOTS_META)
        );
        assert!(get("/?workspace=wobbly-wombat-ec5e6bd2b1eaa013ccba04c67f88").contains(ROBOTS_META));
        let landing = get("/");
        assert!(landing.contains("<title>ReOpenSCAD</title>"));
        assert!(!landing.contains("name=\"robots\""));
    }

    /// A shell served by anything other than this binary — a static host, a
    /// mirror, a copy of the file — must still hide workspaces.
    #[test]
    fn the_shell_file_on_disk_is_noindex() {
        let shell = fs::read_to_string(web_root().join("index.html")).unwrap();
        assert!(shell.contains(ROBOTS_META));
    }

    /// The workspace routes, the API that serves workspace source and the MCP
    /// endpoint are closed; the landing page stays open so the project is
    /// findable at all.
    #[test]
    fn robots_txt_closes_workspaces_and_leaves_the_landing_page_open() {
        let response = get("/robots.txt");
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Content-Type: text/plain; charset=utf-8"));
        for rule in [
            "User-agent: *",
            "Disallow: /workspaces/",
            "Disallow: /api/",
            "Disallow: /mcp",
        ] {
            assert!(response.contains(rule), "robots.txt is missing {rule:?}");
        }
        assert!(!response.contains("Disallow: /\r\n"));
        assert!(!response.contains("Disallow: /\n"));
    }

    /// A shared cache holding a workspace response would hand one visitor's
    /// work to the next.
    #[test]
    fn nothing_may_be_cached() {
        for path in [
            "/",
            "/workspaces/wobbly-wombat-ec5e6bd2b1eaa013ccba04c67f88",
            "/api/health",
        ] {
            assert!(
                get_headers(path).contains("\r\nCache-Control: no-store"),
                "{path} was cacheable"
            );
        }
    }

    /// Connection threads are pooled, so the landing page must not leave the
    /// next request on that thread indexable.
    #[test]
    fn indexability_does_not_leak_between_requests_on_a_thread() {
        // Stand in for "the previous request on this thread was the landing
        // page"; the reset inside `serve_request` is what has to undo it.
        set_indexable(true);
        let response = get("/workspaces/wobbly-wombat-ec5e6bd2b1eaa013ccba04c67f88");
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        assert!(headers.contains("X-Robots-Tag"));
        assert!(body.contains(ROBOTS_META));
        set_indexable(false);
    }
}
