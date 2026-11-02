#[allow(dead_code)]
#[path = "csg/mod.rs"]
mod csg;
/// What agents did to a design over MCP, shown live in the browser.
mod activity;
mod agent;
mod analysis;
mod assets;
/// Per-design STL assets: storage, uploads, HTTP and MCP surfaces.
mod assetstore;
mod engine;
mod insertion;
mod export;
mod feedback;
mod gltf;
/// In-flight MCP job coalescing, short result cache, listing and cancel.
mod jobs;
/// `Preview::vertices` is read only by this module's own tests.
#[allow(dead_code)]
mod mcpapp;
mod meshanalysis;
mod meshimport;
mod meshops;
/// Postgres workspace storage. Optional: the crate builds and every test runs
/// without it, and only a deployment that sets `DATABASE_URL` needs it.
#[cfg(feature = "postgres")]
mod pgstore;
mod printlayout;
mod raster;
mod rendercache;
mod plot2d;
mod scadsource;
mod reverse_guide;
mod scancommon;
mod split;
mod scanfeatures;
mod scanreport;
mod sectioncompare;
mod surfacecompare;
mod curvefit;
mod step;
/// fit_surface: constrained B-spline patches fitted to scan regions.
mod surfacefit;
mod support;
mod textfit;
mod threemf;
mod vec3;

use csg::cleanup::CleanupReport;
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
use std::sync::{Arc, Condvar, Mutex, OnceLock};
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
/// A minute: waiting longer for a slot means several jobs are ahead of you
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
/// Ceiling on a stored render thumbnail, as the `data:` URL's own length.
///
/// The browser writes a 320-wide JPEG, which lands around 8 KB of base64 for a
/// typical model and a little over 20 KB for a busy one. 96 KB leaves a wide
/// margin over that while keeping the workspace document small enough that
/// loading one is still a single cheap read — and it bounds what an attacker
/// can park in the store per workspace, which is the reason it is a hard limit
/// rather than a guideline.
const MAX_PREVIEW_BYTES: usize = 96 * 1024;
/// Workspaces held in RAM with their full source. Everything else lives on
/// disk and is faulted in on demand.
const MAX_CACHED_WORKSPACES: usize = 64;

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
const MCP_INSTRUCTIONS: &str = "Create and edit single-file SCAD designs, then inspect, render or export them.

Record the brief in the file. Every design starts with a header comment block holding what the user asked for and decided, so the file alone gives any later agent the full context: `// Requirements:` (purpose, dimensions and fit targets, printer, material, tolerance, constraints) and `// Decisions:` (choices made, alternatives rejected and why). Before every edit, read the header (get_design_code startLine 1 with an endLine covering it), and whenever the user states or changes a requirement or decision, update the header in the same write as the code. Never drop or reword a requirement the user gave unless the user changes it.

Access: design IDs and URLs are capabilities with no login; anyone holding one can read and edit, so never post them publicly or in shared logs. For view-only access use create_share_link. Designs do not expire, but a deployment holds a bounded number and evicts the least recently touched; each keeps its last 200 revisions (fewer on a large source: 4 MiB of history per design).

Work in the design the person gave you; create a new one only when they ask for a new design or none exists, never to try something out.

Revisions: what you work on is always what the person sees. Every write is a saved, named revision that the open browser renders and highlights at once; source that fails to compile is rejected. To try a pose or a value, save it as a revision and look at it there; history is never rewritten, and restore_revision copies an old revision on top. Writes require revisionName, at most 40 characters, naming what the revision is; updates take expectedRevision. Analysis, render and export tools read only the current revision (diff compares two stored ones).

evaluate is the one-call loop: optionally save, then compile once and get objects, every pairwise intersection against the tolerance, the diagnostics, and optionally echo values, assert margins and a PNG. A model can compile and still not be a closed solid (meshes.valid); printable is true only once checks: [\"supports\"] finds nothing.

On a large source, locate code with list_modules or get_design_code grep, read only those lines, and edit with update_design_code edits or line ranges; change values with set_parameters. For lettering use set_text. For printing, record each part's orientation with set_print_orientation instead of describing it in words; get_print_layout packs the parts on the plate and export_print_parts returns oriented part and plate files.

Feedback on the model: the person can pin comments to points on the 3D view. list_feedback reads them and resolve_feedback closes them with a reply they see. When you cannot tell which feature or location they mean, call request_pointing; ask open questions in chat. Tool replies carry openFeedback when comments are waiting.

Scans and other meshes: attach an STL with upload_asset (base64, files up to 1.4 MB) or create_asset_upload (a one-time PUT URL for files up to 128 MiB), then use it as import(\"name.stl\"). Uploaded STLs are temporary: one per design, deleted 3 hours after the upload with no renewal, and a design still importing one stops rendering then; delete_asset removes one sooner. To turn an STL into a real SCAD part, call get_reverse_engineering_guide.

If a call times out on your side, retry it unchanged: an identical call attaches to the job still running (or gets its reply, kept 3 minutes), and a replayed write returns its original reply. jobs lists and cancels a design's jobs.

ui://reopenscad/design/{designId} is an embedded 3D preview app with STL and 3MF downloads. Deprecated aliases from when designs were called workspaces: the *_workspace* tool names, the workspaceId argument and ui://reopenscad/workspace/{id}.";

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
    /// Per-workspace edit locks (sharded by id). An edit holds its shard
    /// for its revision-history write and its document write, so two saves of
    /// one workspace serialise their read-modify-write of the history sidecar
    /// while saves of other workspaces proceed in parallel.
    edit_locks: Arc<Vec<Mutex<()>>>,
    /// Stored render responses, under `<root>/render-cache/`.
    renders: rendercache::FsRenderCache,
    /// Comments pinned to the model, in `<root>/<id>.feedback` sidecars.
    feedback: Arc<feedback::FsFeedback>,
    /// Mesh assets, in `<root>/<id>.assets/` directories.
    assets: Arc<assetstore::FsAssets>,
}

/// Shards of [`WorkspaceStore::edit_locks`].
const EDIT_LOCK_SHARDS: usize = 64;

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
    /// Creation statistics, mirrored to `WORKSPACES_CREATED_FILE`. Only
    /// `create` moves them: eviction shrinks `index`, never these.
    created: CreationCounter,
}

/// Subdirectory of the store root holding stored renders. A directory, so
/// the boot scan (files only) never mistakes it for a workspace.
const RENDER_CACHE_DIR: &str = "render-cache";

/// The creation counter's file in the store root. The `.count` extension
/// keeps it out of the `.json` scan that builds the index.
const WORKSPACES_CREATED_FILE: &str = "workspaces-created.count";

/// Milliseconds in one creation bucket: one minute, so "the last 24 hours"
/// is a rolling window accurate to a minute rather than to an hour.
const CREATION_BUCKET_MILLIS: u64 = 60 * 1000;
/// Buckets summed for "created in the last 24 hours": the current, partial
/// minute plus the 1439 before it.
const CREATION_WINDOW_BUCKETS: u64 = 24 * 60;
/// Buckets summed for "created in the last 7 days", and how long the ring
/// keeps minute buckets at all.
const CREATION_WEEK_BUCKETS: u64 = 7 * CREATION_WINDOW_BUCKETS;
/// The bucket width counters written before per-minute buckets used.
const LEGACY_HOUR_MILLIS: u64 = 60 * 60 * 1000;

/// Lifetime creation total plus a ring of per-minute creation counts, which
/// is all the landing page's three numbers need. Bounded: buckets older than
/// a week are dropped on every write (at most 10080 entries).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct CreationCounter {
    total: u64,
    /// `(minute since the epoch, workspaces created in it)`, oldest first.
    #[serde(default)]
    minutes: Vec<(u64, u64)>,
    /// Hourly buckets from counter files written before `minutes` existed.
    /// Read once and folded into `minutes` (at the hour's last minute, so an
    /// upgrade never ages a creation out early); never written again.
    #[serde(default, skip_serializing)]
    hours: Vec<(u64, u64)>,
}

impl CreationCounter {
    /// Folds legacy hourly buckets into minute buckets.
    fn upgrade(&mut self) {
        if self.hours.is_empty() {
            return;
        }
        let per_hour = LEGACY_HOUR_MILLIS / CREATION_BUCKET_MILLIS;
        for (hour, count) in std::mem::take(&mut self.hours) {
            self.minutes.push((hour * per_hour + per_hour - 1, count));
        }
        self.minutes.sort_unstable();
        self.minutes.dedup_by(|later, earlier| {
            if later.0 == earlier.0 {
                earlier.1 = earlier.1.saturating_add(later.1);
                true
            } else {
                false
            }
        });
    }

    fn record(&mut self, now_millis: u64) {
        self.upgrade();
        let minute = now_millis / CREATION_BUCKET_MILLIS;
        self.total = self.total.saturating_add(1);
        match self.minutes.last_mut() {
            Some((last, count)) if *last == minute => *count = count.saturating_add(1),
            _ => self.minutes.push((minute, 1)),
        }
        let oldest = minute.saturating_sub(CREATION_WEEK_BUCKETS - 1);
        self.minutes.retain(|(bucket, _)| *bucket >= oldest);
    }

    /// Workspaces created in the 24 hours before `now_millis`, to the minute.
    fn last_24h(&self, now_millis: u64) -> u64 {
        self.created_within(now_millis, CREATION_WINDOW_BUCKETS)
    }

    /// Workspaces created in the 7 days before `now_millis`, to the minute.
    fn last_7d(&self, now_millis: u64) -> u64 {
        self.created_within(now_millis, CREATION_WEEK_BUCKETS)
    }

    /// Workspaces created in the `buckets` minutes ending at `now_millis`.
    fn created_within(&self, now_millis: u64, buckets: u64) -> u64 {
        let minute = now_millis / CREATION_BUCKET_MILLIS;
        let oldest = minute.saturating_sub(buckets - 1);
        let per_hour = LEGACY_HOUR_MILLIS / CREATION_BUCKET_MILLIS;
        self.minutes
            .iter()
            .copied()
            .chain(
                self.hours
                    .iter()
                    .map(|(hour, count)| (hour * per_hour + per_hour - 1, *count)),
            )
            .filter(|(bucket, _)| (oldest..=minute).contains(bucket))
            .map(|(_, count)| count)
            .sum()
    }
}

/// Public, aggregate creation numbers served by `GET /api/stats`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct CreationStats {
    total: u64,
    last_24h: u64,
    last_7d: u64,
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
    /// Identifier of a 3D printer build plate, set through the HTTP API; empty
    /// means "no plate", the default, and print layouts then use a 256 mm bed.
    #[serde(default)]
    plate: String,
    /// Fabrication clearance used by the intersection checker, in millimetres.
    #[serde(default = "default_tolerance")]
    tolerance: f64,
    /// A small `data:` URL of the last render, drawn by the browser that made
    /// it, or empty for a workspace nobody has rendered yet.
    ///
    /// Stored rather than regenerated because the only thing that can produce
    /// it is a WebGL context with the model already on screen — the server
    /// meshes geometry but has no renderer, and spinning one up to redraw
    /// thumbnails on demand would be a large amount of machinery for a picture
    /// the client is holding anyway. It rides in the workspace document, so it
    /// inherits that document's atomic write and its eviction, and it is never
    /// included in a `/events` poll: see `workspace_payload`.
    #[serde(default)]
    preview: String,
    /// Capability for the read-only view, or empty until the workspace has
    /// been shared once.
    ///
    /// Deliberately independent of `id` rather than derived from it: the whole
    /// point of the read-only link is that holding it does not get you the
    /// editable one, and anything derivable — a hash, a prefix, a
    /// transformation — is only as strong as the reader's willingness to try.
    /// The mapping lives on the server and goes one way for everybody else.
    #[serde(default)]
    share_token: String,
    /// The current revision's name; empty in documents written before revisions
    /// had names (see [`revision_label`]).
    #[serde(default)]
    revision_name: String,
    /// Who wrote the current revision: `ai` or `browser`; empty when unrecorded.
    #[serde(default)]
    revision_source: String,
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
    /// Settings ride along on the same PATCH. Absent means "leave as is", so a
    /// plain code save never clears a stored plate.
    plate: Option<String>,
    tolerance: Option<f64>,
    #[serde(default = "default_edit_source")]
    source: String,
    /// Names the revision this update creates. MCP tools require it; a browser
    /// save or a REST call without one gets [`default_revision_name`].
    revision_name: Option<String>,
}

#[derive(Debug)]
struct EditResult {
    workspace: Workspace,
    /// One event per changed region, all carrying the new revision. A
    /// byte-range edit is one region; a full-code replacement is diffed
    /// against the source it replaces, so a two-line change highlights two
    /// lines instead of the whole file.
    events: Vec<WorkspaceEvent>,
}

impl EditResult {
    /// The span covering every changed region, in new-source bytes, as
    /// `(start, end_in_old_source_terms, inserted_length)` of one equivalent
    /// edit. Used where a single summary range is reported.
    fn span(&self) -> (usize, usize, usize) {
        edit_span(&self.events)
    }
}

/// Most separate highlighted regions one save records; beyond this the
/// closest regions are merged.
const MAX_EDIT_HUNKS: usize = 16;

/// One changed region of a save: `start` is a byte offset into the source
/// after every earlier region was applied (equal to the offset in the new
/// source), `removed` bytes of the old source were replaced by `inserted`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EditHunk {
    start: usize,
    removed: usize,
    inserted: usize,
}

/// Resolves an update into the new source and its changed regions.
/// A byte-range edit is taken as given; full `code` is diffed line by line
/// against `current` so only the lines that actually changed are recorded.
fn resolve_edit(current: &str, update: &WorkspaceUpdate) -> Result<(String, Vec<EditHunk>), UpdateError> {
    if let Some(code) = &update.code {
        let hunks = scadsource::change_hunks(current, code, MAX_EDIT_HUNKS)
            .into_iter()
            .map(|hunk| EditHunk {
                start: hunk.start,
                removed: hunk.removed,
                inserted: hunk.inserted,
            })
            .collect();
        return Ok((code.clone(), hunks));
    }
    let start = update
        .start
        .ok_or_else(|| UpdateError::Invalid("Provide code or a start/end/text edit.".into()))?;
    let end = update
        .end
        .ok_or_else(|| UpdateError::Invalid("Provide code or a start/end/text edit.".into()))?;
    let text = update.text.clone().unwrap_or_default();
    if start > end
        || end > current.len()
        || !current.is_char_boundary(start)
        || !current.is_char_boundary(end)
    {
        return Err(UpdateError::Invalid(
            "Edit range must use valid UTF-8 byte offsets within the document.".into(),
        ));
    }
    let mut code = current.to_string();
    code.replace_range(start..end, &text);
    Ok((
        code,
        vec![EditHunk {
            start,
            removed: end - start,
            inserted: text.len(),
        }],
    ))
}

/// The events one save appends: one per changed region (an unchanged
/// re-save still records one empty event, so every revision has one).
fn edit_events(
    id: &str,
    revision: u64,
    source: &str,
    hunks: &[EditHunk],
    timestamp: u64,
) -> Vec<WorkspaceEvent> {
    let source = normalize_source(source);
    let empty = [EditHunk {
        start: 0,
        removed: 0,
        inserted: 0,
    }];
    let hunks = if hunks.is_empty() { &empty[..] } else { hunks };
    hunks
        .iter()
        .map(|hunk| WorkspaceEvent {
            workspace_id: id.to_string(),
            revision,
            source: source.clone(),
            start: hunk.start,
            end: hunk.start + hunk.removed,
            inserted_length: hunk.inserted,
            timestamp,
        })
        .collect()
}

/// `(start, end, inserted_length)` of the single edit spanning every event of
/// one save, `start`/`end` in old-source bytes.
fn edit_span(events: &[WorkspaceEvent]) -> (usize, usize, usize) {
    let (Some(first), Some(last)) = (events.first(), events.last()) else {
        return (0, 0, 0);
    };
    // Net growth of every region before the last one, to convert the last
    // region's (sequential) start back into old-source terms.
    let shift: isize = events[..events.len() - 1]
        .iter()
        .map(|event| event.inserted_length as isize - (event.end - event.start) as isize)
        .sum();
    let old_end = (last.end as isize - shift).max(first.start as isize) as usize;
    let new_end = last.start + last.inserted_length;
    (first.start, old_end, new_end - first.start)
}

/// When a workspace's current revision was written: its own edit event's
/// time, not `updated_at`, which settings writes also move.
fn revision_written_at(workspace: &Workspace) -> u64 {
    workspace
        .events
        .iter()
        .rev()
        .find(|event| event.revision == workspace.revision)
        .map(|event| event.timestamp)
        .unwrap_or(if workspace.revision <= 1 {
            workspace.created_at
        } else {
            workspace.updated_at
        })
}

/// How many superseded revisions a workspace keeps. The current revision lives
/// in the workspace document itself; these are the ones before it, newest last.
const MAX_REVISION_HISTORY: usize = 200;
/// Byte ceiling on the retained history of one workspace, in both stores.
/// Oldest snapshots go first once it is reached: a 2 MiB source keeps at least
/// one predecessor, a 20 KB one all 200, a 70 KB one about 58. This is what
/// bounds the store (ceiling x design count), so the raised count did not
/// raise the worst case.
const MAX_REVISION_HISTORY_BYTES: usize = 4 * 1024 * 1024;
/// Longest revision name, in characters.
const MAX_REVISION_NAME_CHARS: usize = 40;
/// The name of a revision saved by the browser's autosave.
const BROWSER_REVISION_NAME: &str = "Edit in browser";
/// The name of a design a browser made, or copied from a read-only link.
const BROWSER_CREATED_REVISION_NAME: &str = "New design";
/// The name of a REST write by an agent that gave none. MCP tools require one.
const AGENT_REVISION_NAME: &str = "Agent edit";
/// A browser autosave replaces the autosave before it, instead of stacking on
/// it, when that one is younger than this: one burst of typing is one revision.
const BROWSER_COALESCE_MILLIS: u64 = 60 * 1000;
/// However long the burst, an autosave from an earlier period of this length is
/// kept: wall-clock periods, so no burst start time has to be stored.
const BROWSER_CHECKPOINT_MILLIS: u64 = 5 * 60 * 1000;

/// One superseded revision's source.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RevisionSnapshot {
    revision: u64,
    code: String,
    /// When this revision was written (its `updatedAt` while current).
    timestamp: u64,
    /// Empty for snapshots stored before revisions had names.
    #[serde(default)]
    name: String,
    /// `ai` or `browser`; empty when unrecorded.
    #[serde(default)]
    source: String,
}

/// A retained revision without its source: what a listing shows.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RevisionInfo {
    revision: u64,
    timestamp: u64,
    bytes: usize,
    name: String,
    source: String,
}

/// The name and author a write records for the revision it creates.
#[derive(Clone, Debug)]
struct RevisionMark {
    name: String,
    source: String,
}

impl RevisionMark {
    fn browser_created() -> Self {
        Self { name: BROWSER_CREATED_REVISION_NAME.into(), source: "browser".into() }
    }

    fn agent(name: String) -> Self {
        Self { name, source: "ai".into() }
    }
}

/// A revision name as stored: trimmed, non-empty, at most
/// [`MAX_REVISION_NAME_CHARS`] characters. Rejected rather than clipped, so an
/// agent learns its name did not fit. Control characters and line separators
/// become spaces: the name is one line in a list. Invisible and direction
/// characters go, or a name could be blank on screen or read reversed.
fn validate_revision_name(raw: &str) -> Result<String, String> {
    let invisible = |c: char| matches!(c, '\u{ad}' | '\u{61c}' | '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}');
    let cleaned: String = raw
        .chars()
        .filter(|c| !invisible(*c))
        .map(|c| if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') { ' ' } else { c })
        .collect();
    let name = cleaned.trim();
    if name.is_empty() {
        return Err(format!(
            "revisionName must not be empty: name what this revision is, in at most {MAX_REVISION_NAME_CHARS} characters."
        ));
    }
    let length = name.chars().count();
    if length > MAX_REVISION_NAME_CHARS {
        return Err(format!(
            "revisionName is {length} characters; the limit is {MAX_REVISION_NAME_CHARS}. Nothing was saved."
        ));
    }
    Ok(name.to_string())
}

/// The name a save without an explicit one gets.
fn default_revision_name(source: &str) -> &'static str {
    if normalize_source(source) == "ai" {
        AGENT_REVISION_NAME
    } else {
        BROWSER_REVISION_NAME
    }
}

/// The revision name and author `update` records, or why its name is invalid.
fn update_mark(update: &WorkspaceUpdate) -> Result<RevisionMark, UpdateError> {
    let name = match update.revision_name.as_deref() {
        Some(name) => validate_revision_name(name).map_err(UpdateError::Invalid)?,
        None => default_revision_name(&update.source).to_string(),
    };
    Ok(RevisionMark { name, source: normalize_source(&update.source) })
}

/// A superseded revision's snapshot row, its author falling back on the edit
/// log for a revision saved before revisions recorded one.
fn superseded_source(workspace: &Workspace) -> String {
    if !workspace.revision_source.is_empty() {
        return workspace.revision_source.clone();
    }
    event_source(workspace, workspace.revision).unwrap_or_default()
}

fn event_source(workspace: &Workspace, revision: u64) -> Option<String> {
    workspace.events.iter().rev().find(|event| event.revision == revision).map(|event| event.source.clone())
}

/// Whether `update`, recorded as `mark`, is the same burst of browser typing as
/// the current revision: then the revision number still moves (open tabs and
/// conflict checks rely on it) but the autosave it supersedes is not retained,
/// so the list shows one entry per burst. A named revision, an agent's or a
/// restore is never replaced this way, and a pause starts a new entry. So does
/// each new [`BROWSER_CHECKPOINT_MILLIS`] period, or an hour of typing without a
/// pause would leave nothing to go back to. The stores also keep a revision that
/// a model comment names ([`feedback::pins_revision`]).
fn coalesces_with_current(current: &Workspace, mark: &RevisionMark, now: u64) -> bool {
    mark.source == "browser"
        && mark.name == BROWSER_REVISION_NAME
        && current.revision_source == "browser"
        && current.revision_name == BROWSER_REVISION_NAME
        && now.saturating_sub(revision_written_at(current)) < BROWSER_COALESCE_MILLIS
        && now / BROWSER_CHECKPOINT_MILLIS == revision_written_at(current) / BROWSER_CHECKPOINT_MILLIS
}

/// The label of a revision stored without a name.
fn revision_label(name: &str, revision: u64) -> String {
    if name.trim().is_empty() {
        format!("Revision {revision}")
    } else {
        name.to_string()
    }
}

/// `agent`, `browser` or `unknown`: the stored author as agents and the panel
/// read it.
fn revision_author(source: &str) -> &'static str {
    match source {
        "ai" => "agent",
        "browser" => "browser",
        _ => "unknown",
    }
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

/// Plate ids are catalogue keys, so the server only has to keep them boring:
/// lowercase slug characters, bounded length. Anything else collapses to "no
/// plate" rather than being rejected, because a bad setting must never fail a
/// code save.
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

#[cfg(test)]
thread_local! {
    /// Lets a test make the next document writes on this thread fail.
    static PERSIST_FAILS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
        // A store that predates the counter (or lost the file) starts from the
        // workspaces it holds rather than from zero; read before the boot
        // prune, which would otherwise make that floor smaller.
        let recorded = fs::read(root.join(WORKSPACES_CREATED_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CreationCounter>(&bytes).ok());
        let mut created = recorded.clone().unwrap_or_default();
        created.upgrade();
        created.total = created.total.max(index.len() as u64);
        let root_renders = root.join(RENDER_CACHE_DIR);
        let feedback_store = Arc::new(feedback::FsFeedback::new(root.clone()));
        let asset_store = Arc::new(assetstore::FsAssets::new(root.clone()));
        let store = Self {
            root,
            inner: Arc::new(Mutex::new(StoreInner {
                index,
                cache: HashMap::new(),
                order: Vec::new(),
                created: created.clone(),
            })),
            nonce: Arc::new(AtomicU64::new(unix_millis())),
            edit_locks: Arc::new((0..EDIT_LOCK_SHARDS).map(|_| Mutex::new(())).collect()),
            renders: rendercache::FsRenderCache::new(root_renders),
            feedback: feedback_store,
            assets: asset_store,
        };
        store.sweep_orphans();
        if recorded.as_ref() != Some(&created) {
            if let Err(error) = store.write_created(&created) {
                eprintln!("workspace counter could not be initialised: {error}");
            }
        }
        if let Ok(mut inner) = store.inner.lock() {
            store.prune(&mut inner, 0);
        }
        Ok(store)
    }

    /// Workspaces ever created (including ones since evicted) and created in
    /// the last 24 hours.
    fn creation_stats(&self) -> Result<CreationStats, String> {
        self.inner
            .lock()
            .map(|inner| {
                let now = unix_millis();
                CreationStats {
                    total: inner.created.total,
                    last_24h: inner.created.last_24h(now),
                    last_7d: inner.created.last_7d(now),
                }
            })
            .map_err(|_| "Design store is unavailable.".to_string())
    }

    /// Rewrites the counter file atomically (temp file, fsync, rename), so a
    /// crash leaves either the old value or the new one, never a torn number.
    /// Callers hold `inner`, which serialises writers.
    fn write_created(&self, created: &CreationCounter) -> io::Result<()> {
        let bytes = serde_json::to_vec(created).map_err(io::Error::other)?;
        let final_path = self.root.join(WORKSPACES_CREATED_FILE);
        let temporary_path = self.root.join(format!(
            ".{WORKSPACES_CREATED_FILE}-{}.tmp",
            self.nonce.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| -> io::Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temporary_path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary_path, &final_path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        result
    }

    /// Enforces the workspace count cap. `headroom` reserves slots for
    /// workspaces about to be created.
    ///
    /// There is deliberately no age limit. A workspace used to be deleted after
    /// fourteen idle days, which made the URL a promise the service did not
    /// keep: the link is the only handle anyone has on their work, it looks
    /// permanent, and it silently stopped resolving while they were not
    /// looking. Nothing expires now — a workspace is evicted only when the
    /// store is full, least-recently-touched first, which is pressure rather
    /// than a clock.
    fn prune(&self, inner: &mut StoreInner, headroom: usize) {
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
        // The share sidecar goes with the workspace it points at, or eviction
        // would leave a slowly growing field of files naming nothing.
        //
        // Read directly rather than through `load`, which calls *this* on a
        // document it cannot parse — going the other way round closes a
        // mutual recursion that ends in a stack overflow. Best-effort by
        // nature: a document too corrupt to parse has no recoverable token,
        // and the sidecar it may have owned is already inert.
        let token = inner
            .cache
            .get(id)
            .map(|workspace| workspace.share_token.clone())
            .or_else(|| {
                fs::read(self.root.join(format!("{id}.json")))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Workspace>(&bytes).ok())
                    .map(|workspace| workspace.share_token)
            });
        if let Some(token) = token.filter(|token| !token.is_empty()) {
            let _ = fs::remove_file(self.share_path(&token));
        }
        let _ = fs::remove_file(self.root.join(format!("{id}.json")));
        let _ = fs::remove_file(self.history_path(id));
        self.feedback.remove(id);
        self.renders.remove_design(id);
        self.assets.remove_design(id);
        activity::forget(id);
        inner.index.remove(id);
        inner.cache.remove(id);
        inner.order.retain(|key| key != id);
    }

    /// Where a share token's pointer file lives.
    ///
    /// A file per token rather than an in-memory map, because `open` reads no
    /// workspace contents at boot — that is what keeps startup O(count) rather
    /// than O(bytes) — and a token index would have had to read all of them.
    /// The `.share` extension keeps these out of the `.json` scan that builds
    /// the workspace index.
    fn share_path(&self, token: &str) -> PathBuf {
        self.root.join(format!("{token}.share"))
    }

    /// Superseded revisions live in a sidecar rather than in the workspace
    /// document, so the LRU cache of loaded workspaces, every `/events` read
    /// and every save stay the size of one source; the sidecar is read only
    /// when a diff asks for an old revision. The `.history` extension keeps it
    /// out of the `.json` scan that builds the index.
    fn history_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.history"))
    }

    fn read_history(&self, id: &str) -> Vec<RevisionSnapshot> {
        fs::read(self.history_path(id))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// The edit lock shard of `id`.
    fn edit_lock(&self, id: &str) -> std::sync::MutexGuard<'_, ()> {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        id.hash(&mut hasher);
        let shard = &self.edit_locks[hasher.finish() as usize % self.edit_locks.len()];
        shard.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Removes revision-history sidecars and share pointers whose workspace
    /// is gone (an eviction that raced an edit, or a crash between writes),
    /// and temporary files a crash left behind. Run once at boot.
    fn sweep_orphans(&self) {
        let Ok(inner) = self.inner.lock() else {
            return;
        };
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let orphan = if name.starts_with('.') && name.ends_with(".tmp") {
                true
            } else if let Some(id) = name.strip_suffix(".history") {
                !inner.index.contains_key(id)
            } else if let Some(id) = name.strip_suffix(".feedback") {
                !inner.index.contains_key(id)
            } else if name.ends_with(".share") {
                fs::read_to_string(&path)
                    .map(|id| !inner.index.contains_key(id.trim()))
                    .unwrap_or(false)
            } else {
                false
            };
            if orphan {
                let _ = fs::remove_file(&path);
            }
        }
        self.renders.sweep(|id| inner.index.contains_key(id));
        self.assets.sweep(|id| inner.index.contains_key(id));
    }

    /// Every design id, least recently used first: the order asset storage
    /// evicts in when it is full.
    fn ids_least_recent_first(&self) -> Vec<String> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        let mut ids: Vec<(u64, String)> = inner
            .index
            .iter()
            .map(|(id, meta)| (meta.updated_at, id.clone()))
            .collect();
        ids.sort_unstable();
        ids.into_iter().map(|(_, id)| id).collect()
    }

    /// Marks design `id` as just used (an asset write), for eviction order.
    fn touch(&self, id: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            if inner.index.contains_key(id) {
                inner.index.insert(id.to_string(), WorkspaceMeta { updated_at: unix_millis() });
            }
        }
    }

    /// Appends `snapshot`. The caller holds `id`'s edit lock, and writes the
    /// history before the document that supersedes the snapshot, so a crash or a
    /// failed save can leave a snapshot of the still-current revision (harmless:
    /// it equals the current source and listings skip it) but never a saved
    /// revision without its predecessor. Nothing is evicted here: see
    /// [`Self::trim_saved_history`], which runs once the document has landed.
    fn append_history(&self, id: &str, snapshot: RevisionSnapshot) -> Result<(), String> {
        let mut history = self.read_history(id);
        history.retain(|entry| entry.revision != snapshot.revision);
        history.push(snapshot);
        history.sort_by_key(|entry| entry.revision);
        self.write_history(id, &history)
    }

    /// Applies the count and byte ceilings after a save landed. Until then a
    /// failed save has cost nothing; a failure here only leaves the history one
    /// snapshot over its ceiling until the next save trims it, so it is logged
    /// rather than failing a save that has already happened.
    fn trim_saved_history(&self, id: &str) {
        let mut history = self.read_history(id);
        let before = history.len();
        trim_history(&mut history);
        if history.len() != before {
            if let Err(error) = self.write_history(id, &history) {
                eprintln!("revision history of {id} could not be trimmed: {error}");
            }
        }
    }

    /// Replaces the history sidecar atomically (temp file, fsync, rename).
    fn write_history(&self, id: &str, history: &[RevisionSnapshot]) -> Result<(), String> {
        let bytes = serde_json::to_vec(history).map_err(|error| error.to_string())?;
        let temporary = self.root.join(format!(
            ".{id}-{}.history.tmp",
            self.nonce.fetch_add(1, Ordering::Relaxed)
        ));
        let written = (|| -> io::Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, self.history_path(id))
        })();
        if let Err(error) = written {
            let _ = fs::remove_file(&temporary);
            return Err(format!("Revision history could not be written: {error}"));
        }
        Ok(())
    }

    fn revision_code(&self, id: &str, revision: u64) -> Result<Option<String>, String> {
        let Some(workspace) = self.get(id)? else {
            return Ok(None);
        };
        if workspace.revision == revision {
            return Ok(Some(workspace.code));
        }
        Ok(self
            .read_history(id)
            .into_iter()
            .find(|entry| entry.revision == revision)
            .map(|entry| entry.code))
    }

    fn revision_list(&self, id: &str) -> Result<Vec<RevisionInfo>, String> {
        Ok(self
            .read_history(id)
            .iter()
            .map(|entry| RevisionInfo {
                revision: entry.revision,
                timestamp: entry.timestamp,
                bytes: entry.code.len(),
                name: entry.name.clone(),
                source: entry.source.clone(),
            })
            .collect())
    }

    /// The token for `id`, minting one the first time it is asked for.
    ///
    /// Idempotent: sharing a workspace twice hands back the same link, so a
    /// link already sent to somebody keeps working.
    fn share_token(&self, id: &str) -> Result<String, UpdateError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| UpdateError::Internal("Design store is unavailable.".into()))?;
        let mut workspace = self
            .load(&mut inner, id)
            .map_err(UpdateError::Internal)?
            .ok_or(UpdateError::NotFound)?;
        if !workspace.share_token.is_empty() {
            return Ok(workspace.share_token);
        }
        let token = generate_share_token(&self.nonce);
        // The pointer lands before the workspace does. A sidecar with no
        // workspace naming it is inert; a workspace naming a sidecar that does
        // not exist is a link that 404s.
        fs::write(self.share_path(&token), &workspace.id)
            .map_err(|error| UpdateError::Internal(error.to_string()))?;
        workspace.share_token = token.clone();
        self.persist(&workspace).map_err(UpdateError::Internal)?;
        Self::cache_put(&mut inner, workspace);
        Ok(token)
    }

    /// The workspace a read-only token points at.
    fn resolve_share(&self, token: &str) -> Result<Option<Workspace>, String> {
        if !valid_share_token(token) {
            return Ok(None);
        }
        let Ok(id) = fs::read_to_string(self.share_path(token)) else {
            return Ok(None);
        };
        let id = id.trim();
        if !valid_workspace_id(id) {
            return Ok(None);
        }
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "Design store is unavailable.".to_string())?;
        let workspace = self.load(&mut inner, id)?;
        // A sidecar that still names a workspace whose own token has moved on
        // is stale, and must not resolve: otherwise revoking would not.
        Ok(workspace.filter(|workspace| workspace.share_token == token))
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
            Err(error) => return Err(format!("Design could not be read: {error}")),
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

    /// `plate`/`tolerance` let a caller seed the settings at creation time; a
    /// read-only link's clone copies them from its source this way.
    fn create_with_settings(
        &self,
        code: String,
        name: Option<String>,
        plate: Option<String>,
        tolerance: Option<f64>,
    ) -> Result<Workspace, String> {
        self.create_marked(code, name, plate, tolerance, RevisionMark::browser_created())
    }

    /// [`Self::create_with_settings`], naming revision 1.
    fn create_marked(
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
        let now = unix_millis();
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "Design store is unavailable.".to_string())?;
        self.prune(&mut inner, 1);
        let id = loop {
            let candidate = self.generate_id();
            if !inner.index.contains_key(&candidate) {
                break candidate;
            }
        };
        let workspace = Workspace {
            preview: String::new(),
            share_token: String::new(),
            id: id.clone(),
            name: sanitize_name(name),
            code,
            revision: 1,
            created_at: now,
            updated_at: now,
            plate: plate.as_deref().map(sanitize_plate).unwrap_or_default(),
            tolerance: tolerance.map(sanitize_tolerance).unwrap_or(DEFAULT_TOLERANCE),
            revision_name: mark.name,
            revision_source: mark.source,
            events: Vec::new(),
        };
        self.persist(&workspace)?;
        inner.index.insert(id, WorkspaceMeta { updated_at: now });
        Self::cache_put(&mut inner, workspace.clone());
        // The workspace already exists, so a counter that fails to persist is
        // logged rather than failing the create; the in-memory value still
        // advances and the next successful write carries it.
        inner.created.record(now);
        if let Err(error) = self.write_created(&inner.created) {
            eprintln!("workspace counter could not be written: {error}");
        }
        Ok(workspace)
    }

    fn get(&self, id: &str) -> Result<Option<Workspace>, String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "Design store is unavailable.".to_string())?;
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
        let mark = update_mark(&update)?;
        let current = self
            .inner
            .lock()
            .map_err(|_| UpdateError::Internal("Design store is unavailable.".into()))
            .and_then(|mut inner| {
                self.load(&mut inner, id)
                    .map_err(UpdateError::Internal)
            })?
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
        // Only edits take this lock, so the history write below (up to a few
        // MiB plus an fsync) never stalls reads, settings writes or other
        // workspaces' saves.
        let _edit = self.edit_lock(id);
        let latest = self
            .inner
            .lock()
            .map_err(|_| UpdateError::Internal("Design store is unavailable.".into()))
            .and_then(|mut inner| self.load(&mut inner, id).map_err(UpdateError::Internal))?
            .ok_or(UpdateError::NotFound)?;
        if latest.revision != expected {
            return Err(UpdateError::Conflict(latest));
        }
        // History first: the edit lock keeps the revision where it is, and a
        // failure here fails the save rather than losing diff history.
        let replaced = coalesces_with_current(&latest, &mark, unix_millis())
            && !feedback::pins_revision(self.feedback.state(id), latest.revision);
        if !replaced {
            self.append_history(
                id,
                RevisionSnapshot {
                    revision: latest.revision,
                    code: latest.code.clone(),
                    timestamp: revision_written_at(&latest),
                    name: latest.revision_name.clone(),
                    source: superseded_source(&latest),
                },
            )
            .map_err(UpdateError::Internal)?;
        }
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| UpdateError::Internal("Design store is unavailable.".into()))?;
        // Re-read under the store lock: a settings write or a thumbnail may
        // have landed meanwhile, and they must survive this save. An
        // eviction may also have; then the sidecar just written is removed.
        let Some(latest) = self.load(&mut inner, id).map_err(UpdateError::Internal)? else {
            let _ = fs::remove_file(self.history_path(id));
            return Err(UpdateError::NotFound);
        };
        if latest.revision != expected {
            return Err(UpdateError::Conflict(latest));
        }
        let now = unix_millis();
        let events = edit_events(id, expected + 1, &update.source, &hunks, now);
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
        workspace.revision_name = mark.name;
        workspace.revision_source = mark.source;
        workspace.events.extend(events.iter().cloned());
        if workspace.events.len() > MAX_EVENTS {
            workspace
                .events
                .drain(..workspace.events.len() - MAX_EVENTS);
        }
        self.persist(&workspace).map_err(UpdateError::Internal)?;
        inner.index.insert(id.into(), WorkspaceMeta { updated_at: now });
        Self::cache_put(&mut inner, workspace.clone());
        drop(inner);
        // Only now: until the save has landed, nothing may be evicted for it.
        self.trim_saved_history(id);
        Ok(EditResult { workspace, events })
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
            .map_err(|_| UpdateError::Internal("Design store is unavailable.".into()))?;
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

    /// Replaces the stored thumbnail. Deliberately not a revision-bearing
    /// edit: a preview is derived from the source rather than part of it, so
    /// two tabs racing to upload one should both simply win, and neither
    /// should make the other's next save conflict.
    fn set_preview(&self, id: &str, preview: &str) -> Result<(), UpdateError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| UpdateError::Internal("Design store is unavailable.".into()))?;
        let mut workspace = self
            .load(&mut inner, id)
            .map_err(UpdateError::Internal)?
            .ok_or(UpdateError::NotFound)?;
        workspace.preview = preview.to_string();
        // `updated_at` is left alone on purpose. It is what eviction orders by,
        // and a thumbnail upload is the machine talking, not the user: letting
        // it count as a touch would let a background refresh keep a workspace
        // alive ahead of one someone is actually editing.
        self.persist(&workspace).map_err(UpdateError::Internal)?;
        Self::cache_put(&mut inner, workspace);
        Ok(())
    }

    fn persist(&self, workspace: &Workspace) -> Result<(), String> {
        #[cfg(test)]
        if PERSIST_FAILS.with(std::cell::Cell::get) {
            return Err("persist failed (test)".into());
        }
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
    #[cfg(test)]
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

    /// [`Self::create_with_settings`], naming revision 1.
    fn create_marked(&self, code: String, name: Option<String>, mark: RevisionMark) -> Result<Workspace, String> {
        match self {
            Self::Filesystem(store) => store.create_marked(code, name, None, None, mark),
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.create_marked(code, name, None, None, mark),
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

    /// `(revision, assets version)` in one probe, for `/events` polls that
    /// also watch assets.
    fn poll_state(&self, id: &str) -> Result<Option<(u64, u64)>, String> {
        match self {
            Self::Filesystem(store) => Ok(store
                .get(id)?
                .map(|workspace| (workspace.revision, store.assets.version(id)))),
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.poll_state(id),
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

    fn set_preview(&self, id: &str, preview: &str) -> Result<(), UpdateError> {
        match self {
            Self::Filesystem(store) => store.set_preview(id, preview),
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.set_preview(id, preview),
        }
    }

    fn share_token(&self, id: &str) -> Result<String, UpdateError> {
        match self {
            Self::Filesystem(store) => store.share_token(id),
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.share_token(id),
        }
    }

    fn resolve_share(&self, token: &str) -> Result<Option<Workspace>, String> {
        match self {
            Self::Filesystem(store) => store.resolve_share(token),
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.resolve_share(token),
        }
    }

    /// The source of `revision`: the current one, or one of the last
    /// `MAX_REVISION_HISTORY` superseded ones. `None` when it was never
    /// written or has aged out.
    fn revision_code(&self, id: &str, revision: u64) -> Result<Option<String>, String> {
        match self {
            Self::Filesystem(store) => store.revision_code(id, revision),
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.revision_code(id, revision),
        }
    }

    /// Retained superseded revisions, oldest first. The current revision is
    /// not listed.
    fn revision_list(&self, id: &str) -> Result<Vec<RevisionInfo>, String> {
        match self {
            Self::Filesystem(store) => store.revision_list(id),
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.revision_list(id),
        }
    }

    /// Lifetime and last-24-hour creation counts; eviction lowers neither.
    fn creation_stats(&self) -> Result<CreationStats, String> {
        match self {
            Self::Filesystem(store) => store.creation_stats(),
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.creation_stats(),
        }
    }

    /// A stored render response of design `id` (see [`rendercache`]), or
    /// `None` on a miss. Failures are misses: the caller just renders.
    fn cached_render(&self, id: &str, key: &str) -> Option<Vec<u8>> {
        match self {
            Self::Filesystem(store) => store.renders.get(id, key),
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.cached_render(id, key),
        }
    }

    /// Stores a render response of design `id`. Best effort.
    fn store_render(&self, id: &str, key: &str, body: &[u8]) {
        match self {
            Self::Filesystem(store) => store.renders.put(id, key, body),
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.store_render(id, key, body),
        }
    }

    /// The model comments of design `id` (see [`feedback`]); empty when it
    /// has none. Callers check that the design exists.
    fn feedback_state(&self, id: &str) -> Result<feedback::FeedbackState, String> {
        match self {
            Self::Filesystem(store) => store.feedback.state(id),
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.feedback_state(id),
        }
    }

    /// `(version, open comments)` of design `id`'s feedback: the cheap probe
    /// waiters and the `/events` poll watch.
    fn feedback_summary(&self, id: &str) -> Result<(u64, usize), String> {
        match self {
            Self::Filesystem(store) => store.feedback.summary(id),
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.feedback_summary(id),
        }
    }

    /// Applies `change` to design `id`'s feedback atomically; nothing is
    /// written when it fails.
    fn feedback_mutate<T>(
        &self,
        id: &str,
        change: impl FnMut(&mut feedback::FeedbackState) -> Result<T, String>,
    ) -> Result<T, String> {
        match self {
            Self::Filesystem(store) => store.feedback.mutate(id, change),
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => store.feedback_mutate(id, change),
        }
    }

    /// Records that a browser is long-polling design `id`.
    fn note_viewer(&self, id: &str) {
        let now = unix_millis();
        let persist = feedback::note_viewer_locally(id, now, 10_000);
        #[cfg(feature = "postgres")]
        if let Self::Postgres(store) = self {
            if persist {
                store.note_viewer(id, now);
            }
        }
        let _ = persist;
    }

    /// When a browser last long-polled design `id` (ms), on any instance.
    fn viewer_seen_at(&self, id: &str) -> u64 {
        let local = feedback::viewer_seen_locally(id);
        match self {
            Self::Filesystem(_) => local,
            #[cfg(feature = "postgres")]
            Self::Postgres(store) => local.max(store.viewer_seen_at(id)),
        }
    }
}

/// Drops the oldest snapshots past the count and byte ceilings, always keeping
/// the newest one.
fn trim_history(history: &mut Vec<RevisionSnapshot>) {
    if history.len() > MAX_REVISION_HISTORY {
        history.drain(..history.len() - MAX_REVISION_HISTORY);
    }
    let mut total: usize = history.iter().map(|entry| entry.code.len()).sum();
    while history.len() > 1 && total > MAX_REVISION_HISTORY_BYTES {
        total -= history[0].code.len();
        history.remove(0);
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

/// 128 bits of hex. Not the funny-id shape on purpose: a read-only link should
/// not be mistakable for a workspace link at a glance, in a chat window or in
/// a bug report.
fn generate_share_token(nonce: &AtomicU64) -> String {
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
    random.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn valid_share_token(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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
    /// Zero-area triangles in the mesh (see [`degenerate_triangle_count`]).
    degenerate_triangles: usize,
    messages: Vec<String>,
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

// `HEAD` on `/media/`: the same headers as `GET`, no body. Same
// thread-local shape as `INDEXABLE`, read by `respond_text`.
thread_local! {
    static HEAD_ONLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Crawl policy. `Disallow` stops a compliant crawler from ever fetching a
/// workspace, its source, or its exports; `X-Robots-Tag` and the shell's
/// `<meta name="robots">` catch the ones that fetch anyway.
///
/// The landing page is left crawlable on purpose — it is the front door, it
/// names no workspace, and blanket-blocking the origin would make the project
/// unfindable to buy nothing.
const ROBOTS_TXT: &str = "\
# A design URL is its own credential: there is no login, so anyone holding\n\
# the link can read and change the work behind it. Design pages, the API\n\
# that serves their source, and the export routes must never be crawled,\n\
# cached or indexed. The landing page below is deliberately left open.\n\
User-agent: *\n\
Disallow: /designs/\n\
Disallow: /workspaces/\n\
Disallow: /shared/\n\
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
        && !query_parameters(&request.query).contains_key("design")
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
        // The thread's kernel interrupts count too: a queued job whose client
        // has hung up (`ClientWatch`) leaves the queue instead of waiting for
        // a slot it would only give straight back.
        let cancelled = || {
            cancellation.is_some_and(|flag| flag.load(Ordering::Acquire))
                || csg::bsp::interrupt_message().is_some()
        };
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

    /// A slot only if one is free right now with nobody waiting: never
    /// queues, never waits. For optional work (an object list on a plain
    /// workspace read) that should happen when the server is idle and be
    /// skipped, not delayed, when it is busy.
    fn try_acquire_idle(&self) -> Option<QueuePermit> {
        let mut state = self.inner.state.lock().ok()?;
        if state.depth == 0 && state.active < self.inner.capacity {
            state.active += 1;
            return Some(self.permit(None));
        }
        None
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

/// The per-IP limits. Production defaults are the `IP_*` constants; each can
/// be retuned with an environment variable (see [`RATE_LIMIT_SETTINGS`]), for
/// example to give a local verification loop more headroom.
#[derive(Clone, Copy, Debug, PartialEq)]
struct RateLimits {
    request_burst: f64,
    request_refill_per_sec: f64,
    heavy_burst: f64,
    heavy_refill_per_sec: f64,
    byte_burst: f64,
    byte_refill_per_sec: f64,
}

const MIB: f64 = 1024.0 * 1024.0;

/// `(variable, accepted range, unit multiplier, field)` for every tunable
/// limit. Byte limits are configured in MiB.
#[allow(clippy::type_complexity)]
const RATE_LIMIT_SETTINGS: &[(&str, (f64, f64), f64, fn(&mut RateLimits) -> &mut f64)] = &[
    ("REOPENSCAD_IP_REQUEST_BURST", (1.0, 1_000_000.0), 1.0, |l| &mut l.request_burst),
    ("REOPENSCAD_IP_REQUEST_REFILL_PER_SEC", (0.01, 1_000_000.0), 1.0, |l| &mut l.request_refill_per_sec),
    // The heavy burst must afford at least one evaluator request.
    ("REOPENSCAD_IP_HEAVY_BURST", (HEAVY_COST_EVALUATOR, 1_000_000.0), 1.0, |l| &mut l.heavy_burst),
    ("REOPENSCAD_IP_HEAVY_REFILL_PER_SEC", (0.01, 1_000_000.0), 1.0, |l| &mut l.heavy_refill_per_sec),
    ("REOPENSCAD_IP_BYTE_BURST_MIB", (1.0, 65_536.0), MIB, |l| &mut l.byte_burst),
    ("REOPENSCAD_IP_BYTE_REFILL_MIB_PER_SEC", (0.01, 65_536.0), MIB, |l| &mut l.byte_refill_per_sec),
];

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            request_burst: IP_REQUEST_BURST,
            request_refill_per_sec: IP_REQUEST_REFILL_PER_SEC,
            heavy_burst: IP_HEAVY_BURST,
            heavy_refill_per_sec: IP_HEAVY_REFILL_PER_SEC,
            byte_burst: IP_BYTE_BURST,
            byte_refill_per_sec: IP_BYTE_REFILL_PER_SEC,
        }
    }
}

impl RateLimits {
    /// The defaults with every set variable applied. An unparsable or
    /// out-of-range value is an error, never silently replaced: a typo in a
    /// limit must not quietly leave a public server on different limits than
    /// the operator believes.
    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let mut limits = Self::default();
        for (name, (low, high), unit, field) in RATE_LIMIT_SETTINGS {
            let Some(raw) = lookup(name) else { continue };
            let value = raw
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite() && (*low..=*high).contains(value))
                .ok_or_else(|| {
                    format!("{name}={raw:?} is invalid: expected a number from {low} to {high}.")
                })?;
            *field(&mut limits) = value * unit;
        }
        Ok(limits)
    }

    fn from_environment() -> Result<Self, String> {
        Self::from_lookup(|name| env::var(name).ok().filter(|value| !value.trim().is_empty()))
    }

    fn describe(&self) -> String {
        format!(
            "requests {}/{} per s, compute {}/{} per s ({} per evaluator call), bytes {:.0}/{:.1} MiB per s",
            self.request_burst,
            self.request_refill_per_sec,
            self.heavy_burst,
            self.heavy_refill_per_sec,
            HEAVY_COST_EVALUATOR,
            self.byte_burst / MIB,
            self.byte_refill_per_sec / MIB
        )
    }
}

/// Why the limiter refused a request, and when the same request would pass
/// if nothing else were charged in the meantime.
#[derive(Clone, Copy, Debug, PartialEq)]
struct RateLimited {
    /// `"requests"`, `"compute"` or `"bytes"`.
    bucket: &'static str,
    /// Tokens available and needed in that bucket (bytes for `"bytes"`).
    available: f64,
    needed: f64,
    capacity: f64,
    refill_per_sec: f64,
    /// Exact seconds until the bucket can pay, never under 0.
    retry_after: f64,
}

impl RateLimited {
    /// Whole seconds for `Retry-After`: rounded up, at least 1.
    fn retry_after_seconds(&self) -> u64 {
        (self.retry_after.ceil() as u64).max(1)
    }

    fn detail(&self) -> Value {
        json!({
            "reason": "rate_limited",
            "bucket": self.bucket,
            "retryable": true,
            "retryAfterSeconds": self.retry_after_seconds(),
            "retryAfterMs": (self.retry_after * 1000.0).ceil() as u64,
            "codeProblem": false,
            "limit": {
                "available": self.available,
                "needed": self.needed,
                "capacity": self.capacity,
                "refillPerSecond": self.refill_per_sec,
                "unit": if self.bucket == "bytes" { "bytes" } else { "tokens" },
                "evaluatorCost": HEAVY_COST_EVALUATOR,
            },
        })
    }

    fn explanation(&self) -> String {
        let what = match self.bucket {
            "requests" => "the per-client request budget",
            "compute" => "the per-client compute budget for evaluating routes (render, export, analysis, MCP)",
            _ => "the per-client response-bytes budget",
        };
        format!(
            "Rate limit exceeded: {what} is used up. This is a transient server-side \
             throttle: the request was never evaluated, so nothing about its arguments or \
             SCAD code is wrong. Wait {} s and retry the same request unchanged \
             (other requests from this client in the meantime spend the same budget).",
            self.retry_after_seconds()
        )
    }
}

/// Per-IP token buckets: request count, expensive-route count, and bytes
/// served. The byte bucket is what actually bounds amplification, since a
/// 78-byte render request can return megabytes.
#[derive(Clone)]
struct RateLimiter {
    buckets: Arc<Mutex<HashMap<IpAddr, IpBucket>>>,
    limits: RateLimits,
}

struct IpBucket {
    requests: f64,
    heavy: f64,
    bytes: f64,
    updated: Instant,
}

impl IpBucket {
    fn new(now: Instant, limits: &RateLimits) -> Self {
        Self {
            requests: limits.request_burst,
            heavy: limits.heavy_burst,
            bytes: limits.byte_burst,
            updated: now,
        }
    }

    fn refill(&mut self, now: Instant, limits: &RateLimits) {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        if elapsed <= 0.0 {
            return;
        }
        self.updated = now;
        self.requests = (self.requests + elapsed * limits.request_refill_per_sec).min(limits.request_burst);
        self.heavy = (self.heavy + elapsed * limits.heavy_refill_per_sec).min(limits.heavy_burst);
        self.bytes = (self.bytes + elapsed * limits.byte_refill_per_sec).min(limits.byte_burst);
    }

    /// The first bucket that cannot pay for a request costing `heavy`.
    fn shortfall(&self, heavy: f64, limits: &RateLimits) -> Option<RateLimited> {
        let wait = |available: f64, needed: f64, refill: f64| ((needed - available) / refill).max(0.0);
        let mut worst: Option<RateLimited> = None;
        let mut consider = |limited: RateLimited| {
            if worst.is_none_or(|current| limited.retry_after > current.retry_after) {
                worst = Some(limited);
            }
        };
        if self.requests < 1.0 {
            consider(RateLimited {
                bucket: "requests",
                available: self.requests,
                needed: 1.0,
                capacity: limits.request_burst,
                refill_per_sec: limits.request_refill_per_sec,
                retry_after: wait(self.requests, 1.0, limits.request_refill_per_sec),
            });
        }
        if self.heavy < heavy {
            consider(RateLimited {
                bucket: "compute",
                available: self.heavy,
                needed: heavy,
                capacity: limits.heavy_burst,
                refill_per_sec: limits.heavy_refill_per_sec,
                retry_after: wait(self.heavy, heavy, limits.heavy_refill_per_sec),
            });
        }
        if self.bytes <= 0.0 {
            // Any positive balance admits; one byte stands in for "positive".
            consider(RateLimited {
                bucket: "bytes",
                available: self.bytes,
                needed: 1.0,
                capacity: limits.byte_burst,
                refill_per_sec: limits.byte_refill_per_sec,
                retry_after: wait(self.bytes, 1.0, limits.byte_refill_per_sec),
            });
        }
        worst
    }
}

impl RateLimiter {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_limits(RateLimits::default())
    }

    fn with_limits(limits: RateLimits) -> Self {
        Self {
            buckets: Arc::new(Mutex::new(HashMap::new())),
            limits,
        }
    }

    /// Charges one request against `address`, or says which bucket refused
    /// it and for how long; the caller then answers 429 instead of
    /// dispatching.
    fn check(&self, address: IpAddr, cost: RouteCost) -> Result<(), RateLimited> {
        self.check_at(address, cost, Instant::now())
    }

    #[cfg(test)]
    fn admit_at(&self, address: IpAddr, cost: RouteCost, now: Instant) -> bool {
        self.check_at(address, cost, now).is_ok()
    }

    fn check_at(&self, address: IpAddr, cost: RouteCost, now: Instant) -> Result<(), RateLimited> {
        let limits = self.limits;
        let Ok(mut buckets) = self.buckets.lock() else {
            // A poisoned limiter must not become an open door.
            return Err(RateLimited {
                bucket: "requests",
                available: 0.0,
                needed: 1.0,
                capacity: limits.request_burst,
                refill_per_sec: limits.request_refill_per_sec,
                retry_after: 1.0,
            });
        };
        Self::make_room(&mut buckets, &address, now);
        let bucket = buckets
            .entry(address)
            .or_insert_with(|| IpBucket::new(now, &limits));
        bucket.refill(now, &limits);
        let heavy = cost.heavy_tokens();
        if let Some(limited) = bucket.shortfall(heavy, &limits) {
            return Err(limited);
        }
        bucket.requests -= 1.0;
        bucket.heavy -= heavy;
        Ok(())
    }

    /// Charges one more evaluator-sized unit of work to the current client,
    /// for requests that compile more than once (diff).
    /// `Err` when the client's compute bucket cannot pay, with the wait until
    /// it could: the caller should stop doing extra work. Anonymous
    /// (test/loopback-less) callers pass.
    fn charge_extra_compile(&self) -> Result<(), RateLimited> {
        let ClientKey::Ip(address) = current_client_key() else {
            return Ok(());
        };
        let limits = self.limits;
        let Ok(mut buckets) = self.buckets.lock() else {
            return Err(RateLimited {
                bucket: "compute",
                available: 0.0,
                needed: HEAVY_COST_EVALUATOR,
                capacity: limits.heavy_burst,
                refill_per_sec: limits.heavy_refill_per_sec,
                retry_after: 1.0,
            });
        };
        let Some(bucket) = buckets.get_mut(&address) else {
            return Ok(());
        };
        bucket.refill(Instant::now(), &limits);
        if bucket.heavy < HEAVY_COST_EVALUATOR {
            return Err(RateLimited {
                bucket: "compute",
                available: bucket.heavy,
                needed: HEAVY_COST_EVALUATOR,
                capacity: limits.heavy_burst,
                refill_per_sec: limits.heavy_refill_per_sec,
                retry_after: ((HEAVY_COST_EVALUATOR - bucket.heavy) / limits.heavy_refill_per_sec)
                    .max(0.0),
            });
        }
        bucket.heavy -= HEAVY_COST_EVALUATOR;
        Ok(())
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
            bucket.refill(Instant::now(), &self.limits);
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
    // A one-time asset upload URL: a large body plus mesh processing.
    if path.starts_with("/api/uploads/") {
        return RouteCost::Evaluator;
    }
    if let Some(action) = path.strip_prefix("/api/") {
        if agent::BROWSER_CODE_ACTIONS.contains(&action) {
            return RouteCost::Evaluator;
        }
    }
    // Queue status is an O(1) read under one lock, but it is polled for the
    // whole life of a render, so it is metered like the other poll.
    if path == "/api/queue" {
        return RouteCost::Poll;
    }
    if is_design_collection_route(path) && method == "POST" {
        return RouteCost::Evaluator;
    }
    // One counter read, answered from a short in-process memo.
    if path == "/api/stats" {
        return RouteCost::Cheap;
    }
    // Reading a shared workspace is a plain document read, but it is reachable
    // by anyone holding a link, so it is metered like the other cheap routes
    // rather than being exempt.
    if let Some(rest) = path.strip_prefix("/api/shared/") {
        // Rendering compiles (or at least may), so it is metered as one, and
        // an asset download serves a mesh.
        let rest = rest.trim_end_matches('/');
        return if rest.ends_with("/render") || rest.ends_with("/clone") || rest.contains("/assets/") {
            RouteCost::Evaluator
        } else {
            RouteCost::Cheap
        };
    }
    if let Some((_, action)) = workspace_api_route(path) {
        return match (method, action) {
            (_, "events") => RouteCost::Poll,
            // The agent activity log and its pictures are in-memory reads.
            ("GET", action) if action == "activity" || action.starts_with("activity/") => RouteCost::Cheap,
            // Listing assets is a metadata read; downloading serves a mesh,
            // and uploads and deletes write (an upload also processes one).
            ("GET", "assets") => RouteCost::Cheap,
            (_, action) if action == "assets" || action.starts_with("assets/") => RouteCost::Evaluator,
            // Comment reads are plain reads; a comment write is a small
            // store write, metered like a poll rather than a compile.
            ("GET", action) if action == "feedback" || action.starts_with("feedback/") => RouteCost::Cheap,
            (_, action) if action == "feedback" || action.starts_with("feedback/") => RouteCost::Poll,
            (_, "render") | (_, "export") => RouteCost::Evaluator,
            (method, action)
                if agent::AGENT_ACTIONS.contains(&action) || action == "revisions" =>
            {
                if agent::agent_action_runs_evaluator(method, action, true) {
                    RouteCost::Evaluator
                } else {
                    RouteCost::Cheap
                }
            }
            // A thumbnail upload is a write to the store, and a write is what
            // the stricter bucket exists to meter. `GET` of one is a plain
            // read and falls through to `Cheap` with everything else.
            ("PUT", "preview") => RouteCost::Evaluator,
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
/// selects a resolution — so the two deadlines now bound the same work, and a
/// preview gets the same 120 s a render does (detailed scans compile slowly).
const PREVIEW_DEADLINE: Duration = Duration::from_secs(120);
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

/// What a request costs: its route's cost, except that
/// `GET /api/designs/{id}?objects=compute` may compile and so is `Evaluator`.
fn effective_route_cost(request: &Request) -> RouteCost {
    let cost = route_cost(&request.method, &request.path);
    if cost == RouteCost::Cheap && wants_object_compute(request) {
        RouteCost::Evaluator
    } else {
        cost
    }
}

/// Whether a request of `cost` runs under a [`ClientWatch`]: compute, except
/// uploads (their client is still sending).
fn watches_client(request: &Request, cost: RouteCost) -> bool {
    cost == RouteCost::Evaluator && !assetstore::is_upload_route(&request.method, &request.path)
}

/// How often [`ClientWatch`] checks whether its client is still there.
const CLIENT_WATCH_INTERVAL: Duration = Duration::from_millis(100);
/// The interim response [`ClientWatch`] sends to tell a half-closed client
/// from a gone one. Every HTTP/1.1 client must skip a 1xx it did not expect
/// (RFC 9110 §15.2); browsers and curl do.
const CLIENT_LIVENESS_PROBE: &[u8] = b"HTTP/1.1 102 Processing\r\n\r\n";

// Whether the request on this thread was HTTP/1.1 or later, so it may be sent
// an interim 1xx (an HTTP/1.0 client must not be). Set by `read_request`.
thread_local! {
    static INTERIM_RESPONSES_OK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// The live [`ClientWatch`]'s gate on this (handler) thread: `true` once the
// response has started, after which the watch never writes. The watch writes
// its probe holding the lock, so the two never interleave.
thread_local! {
    static RESPONSE_GATE: std::cell::RefCell<Option<Arc<Mutex<bool>>>> = const { std::cell::RefCell::new(None) };
}

// The live [`ClientWatch`]'s "client disconnected" flag on this (handler)
// thread, so a shared job can tell which of its callers are still there.
thread_local! {
    static CLIENT_GONE: std::cell::RefCell<Option<Arc<AtomicBool>>> = const { std::cell::RefCell::new(None) };
}

fn current_client_gone() -> Option<Arc<AtomicBool>> {
    CLIENT_GONE.with(|slot| slot.borrow().clone())
}

// The JSON-RPC id of the `tools/call` this thread is answering, so the call's
// job can be withdrawn by a `notifications/cancelled` naming it.
thread_local! {
    static MCP_REQUEST_ID: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// The scope `notifications/cancelled` matches a request id in: the
/// request's `Mcp-Session-Id` plus the id as JSON.
///
/// Never the client address: clients behind one NAT or proxy share it, and
/// JSON-RPC ids are small predictable integers, so any of them could cancel
/// another's job. The session id is 128 random bits issued at `initialize`
/// and is used purely as an opaque cancellation scope: it is not stored, not
/// required, and an unknown one is not refused (instances share no session
/// store). A request without one cannot be withdrawn by notification; its job
/// still stops when its client disconnects (after the orphan grace).
fn mcp_rpc_scope(request: &Request, id: &Value) -> Option<String> {
    let session = request.header("mcp-session-id").filter(|session| valid_mcp_session_id(session))?;
    Some(format!("session|{session}|{id}"))
}

/// Visible ASCII (0x21–0x7E) per the Streamable HTTP transport; at least
/// 16 bytes so a short guessable value is never a scope, at most 128.
fn valid_mcp_session_id(session: &str) -> bool {
    (16..=128).contains(&session.len()) && session.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

/// A fresh `Mcp-Session-Id`, or `None` when no randomness is available (the
/// client then simply cannot cancel by notification).
fn mint_mcp_session_id() -> Option<String> {
    assetstore::mint_token().ok()
}

/// Marks the response on this thread as started; a [`ClientWatch`] writes
/// nothing to the socket from here on. Called by every response writer.
fn begin_response() {
    RESPONSE_GATE.with(|gate| {
        if let Some(gate) = gate.borrow().as_ref() {
            *gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        }
    });
}

/// Cancels the compute of a request whose client hung up.
///
/// Nothing else notices a disconnect until the response is written, so a
/// caller that timed out and retried used to leave its abandoned job holding a
/// render slot to the end, and a few retries filled both. While the guard
/// lives, a thread peeks the socket (without consuming or blocking), as nginx
/// does to log a 499; a reset sets a flag installed as a kernel cancellation
/// flag on this thread (`csg::bsp::cancellation_scope`), which compiles,
/// booleans, analysis loops and the render queue all poll.
///
/// End of stream alone proves nothing: a client that only shut down its
/// sending side (`shutdown(SHUT_WR)`) still waits for the answer, at any
/// point. So on end of stream the watch sends one interim
/// `102 Processing` (HTTP/1.1 clients only, and only before the response has
/// started). A half-closed client reads and skips it; a closed socket answers
/// it with a reset, which the next peek sees, or the send fails outright.
/// Either of those, and only those, cancels. Without the probe (HTTP/1.0) end
/// of stream is left alone and the job runs on.
struct ClientWatch {
    finished: Arc<AtomicBool>,
    /// Joined on drop, so the watch's duplicate descriptor is closed before
    /// the handler's own is and a `Connection: close` FIN is not held back.
    watcher: Option<thread::JoinHandle<()>>,
    _scope: csg::bsp::CancelScope,
}

impl ClientWatch {
    /// Watches `stream` from the calling (handler) thread. `may_probe`: the
    /// client accepts an interim 1xx (HTTP/1.1).
    fn start(stream: &TcpStream, may_probe: bool) -> Option<Self> {
        let probe = stream.try_clone().ok()?;
        let gone = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(Mutex::new(false));
        let (watched_gone, watched_finish, watched_gate) = (Arc::clone(&gone), Arc::clone(&finished), Arc::clone(&gate));
        let watcher = thread::Builder::new()
            .name("reopenscad-client-watch".into())
            .spawn(move || {
                let mut probed = false;
                while !watched_finish.load(Ordering::Acquire) {
                    match peer_state(&probe) {
                        PeerState::Present => {}
                        PeerState::Reset => {
                            watched_gone.store(true, Ordering::Release);
                            return;
                        }
                        PeerState::EndOfStream if probed => {}
                        PeerState::EndOfStream => {
                            if !may_probe {
                                return;
                            }
                            let responding = watched_gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                            if *responding {
                                return;
                            }
                            probed = true;
                            if send_probe(&probe, CLIENT_LIVENESS_PROBE).is_err() {
                                drop(responding);
                                watched_gone.store(true, Ordering::Release);
                                return;
                            }
                        }
                    }
                    thread::park_timeout(CLIENT_WATCH_INTERVAL);
                }
            })
            .ok()?;
        RESPONSE_GATE.with(|slot| *slot.borrow_mut() = Some(gate));
        CLIENT_GONE.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&gone)));
        Some(Self {
            finished,
            watcher: Some(watcher),
            _scope: csg::bsp::cancellation_scope(gone),
        })
    }
}

impl Drop for ClientWatch {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::Release);
        RESPONSE_GATE.with(|slot| *slot.borrow_mut() = None);
        CLIENT_GONE.with(|slot| *slot.borrow_mut() = None);
        if let Some(watcher) = self.watcher.take() {
            watcher.thread().unpark();
            let _ = watcher.join();
        }
    }
}

/// A non-blocking send of `bytes` in one piece (a few bytes into an idle
/// socket's send buffer); an error or a short write is `Err`.
#[cfg(unix)]
fn send_probe(stream: &TcpStream, bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    extern "C" {
        fn send(socket: i32, buffer: *const u8, length: usize, flags: i32) -> isize;
    }
    #[cfg(target_os = "linux")]
    const FLAGS: i32 = 0x40 | 0x4000; // MSG_DONTWAIT | MSG_NOSIGNAL
    #[cfg(not(target_os = "linux"))]
    const FLAGS: i32 = 0x80; // MSG_DONTWAIT; SIGPIPE is ignored by the runtime
    // SAFETY: a buffer we own, on a descriptor `stream` keeps open.
    let sent = unsafe { send(stream.as_raw_fd(), bytes.as_ptr(), bytes.len(), FLAGS) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    if sent as usize != bytes.len() {
        return Err(io::Error::new(io::ErrorKind::WriteZero, "short probe write"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn send_probe(_stream: &TcpStream, _bytes: &[u8]) -> io::Result<()> {
    Ok(())
}

enum PeerState {
    /// Nothing to read yet, or bytes waiting (a pipelined next request).
    Present,
    /// The peer shut down its sending side, or closed: not yet told apart.
    EndOfStream,
    /// The connection was reset or failed.
    Reset,
}

/// What a non-blocking, non-consuming peek of the socket says about the peer.
#[cfg(unix)]
fn peer_state(stream: &TcpStream) -> PeerState {
    use std::os::unix::io::AsRawFd;
    extern "C" {
        fn recv(socket: i32, buffer: *mut u8, length: usize, flags: i32) -> isize;
    }
    const MSG_PEEK: i32 = 0x2;
    #[cfg(target_os = "linux")]
    const MSG_DONTWAIT: i32 = 0x40;
    #[cfg(not(target_os = "linux"))]
    const MSG_DONTWAIT: i32 = 0x80;
    let mut byte = 0u8;
    // SAFETY: a one-byte buffer we own, on a descriptor `stream` keeps open.
    let read = unsafe { recv(stream.as_raw_fd(), &mut byte, 1, MSG_PEEK | MSG_DONTWAIT) };
    match read {
        0 => PeerState::EndOfStream,
        read if read > 0 => PeerState::Present,
        _ => match io::Error::last_os_error().kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => PeerState::Present,
            _ => PeerState::Reset,
        },
    }
}

#[cfg(not(unix))]
fn peer_state(_stream: &TcpStream) -> PeerState {
    PeerState::Present
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

// ---------------------------------------------------------------------------
// Memory watchdog.
//
// A 503 on a heavy model (a long hull() chain, say) does not come from this
// server — nothing here answers a render with 503. It is the platform's answer
// after the instance was killed for exceeding its memory limit: measured
// locally, a 150-segment hull chain at $fn=48 peaks at 1.6 GiB resident and a
// 300-segment one at $fn=64 at 5.6 GiB, against a 2 GiB Cloud Run instance
// that admits two renders at once. The per-tree BSP node budget cannot see
// this, because every individual tree stays under it.
//
// The watchdog samples resident memory and, past the budget, raises
// `csg::bsp::MEMORY_PRESSURE`, which every BSP build polls; the build aborts
// with an explanation, the render fails with a 422 naming the limit, and the
// instance survives. The budget is `REOPENSCAD_MAX_RSS_MIB`, else 85% of the
// container's memory limit; with neither known (or on a platform whose
// resident size cannot be read) the watchdog stays off.
// ---------------------------------------------------------------------------

const MEMORY_WATCH_INTERVAL: Duration = Duration::from_millis(50);
/// Share of the container limit the process may use before builds abort,
/// leaving room for response buffers and the allocator's slack.
const MEMORY_BUDGET_SHARE: f64 = 0.85;

/// Resident set size of this process, in bytes.
#[cfg(target_os = "linux")]
fn resident_bytes() -> Option<u64> {
    let statm = fs::read_to_string("/proc/self/statm").ok()?;
    extern "C" {
        fn sysconf(name: i32) -> i64;
    }
    const SC_PAGESIZE: i32 = 30;
    // SAFETY: plain libc query; arm64 kernels may use 16 or 64 KiB pages.
    let page = unsafe { sysconf(SC_PAGESIZE) };
    let page = if page > 0 { page as u64 } else { 4096 };
    parse_statm_resident(&statm).map(|pages| pages * page)
}

#[cfg(target_os = "macos")]
fn resident_bytes() -> Option<u64> {
    // `mach_task_basic_info`, declared with `#pragma pack(4)`.
    #[repr(C, packed(4))]
    #[derive(Default)]
    struct MachTaskBasicInfo {
        virtual_size: u64,
        resident_size: u64,
        resident_size_max: u64,
        user_time: [i32; 2],
        system_time: [i32; 2],
        policy: i32,
        suspend_count: i32,
    }
    extern "C" {
        static mach_task_self_: u32;
        fn task_info(task: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
    }
    const MACH_TASK_BASIC_INFO: i32 = 20;
    let mut info = MachTaskBasicInfo::default();
    let mut count = (std::mem::size_of::<MachTaskBasicInfo>() / 4) as u32;
    // SAFETY: `task_info` writes at most `count` 32-bit words into `info`,
    // which is exactly that large; `mach_task_self_` is the task's own port.
    let status = unsafe {
        task_info(
            mach_task_self_,
            MACH_TASK_BASIC_INFO,
            &mut info as *mut MachTaskBasicInfo as *mut i32,
            &mut count,
        )
    };
    (status == 0).then_some(info.resident_size)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn resident_bytes() -> Option<u64> {
    None
}

/// Resident pages: the second field of `/proc/self/statm`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_statm_resident(statm: &str) -> Option<u64> {
    statm.split_whitespace().nth(1)?.parse().ok()
}

/// A cgroup memory limit file's value in bytes; `max` (v2) and the huge
/// sentinel v1 uses for "unlimited" mean no limit.
fn parse_cgroup_limit(text: &str) -> Option<u64> {
    let value: u64 = text.trim().parse().ok()?;
    (value > 0 && value < (1u64 << 60)).then_some(value)
}

/// The memory budget in MiB: explicit, or a share of the container limit.
fn memory_budget_mib() -> Option<usize> {
    if let Some(explicit) = env::var("REOPENSCAD_MAX_RSS_MIB")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
    {
        return (explicit > 0).then_some(explicit);
    }
    let limit = [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ]
    .iter()
    .find_map(|path| fs::read_to_string(path).ok().as_deref().and_then(parse_cgroup_limit))?;
    Some(((limit as f64 * MEMORY_BUDGET_SHARE) / (1024.0 * 1024.0)) as usize)
}

/// Returns freed heap to the OS. glibc keeps freed small blocks in its arenas,
/// so after a heavy render the resident size can stay near its peak with
/// nothing live in it; without a trim the watchdog would read that as
/// pressure forever.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn release_free_memory() {
    extern "C" {
        fn malloc_trim(pad: usize) -> i32;
    }
    // SAFETY: plain glibc call with no pointers involved.
    unsafe {
        malloc_trim(0);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn release_free_memory() {}

impl RenderQueue {
    /// Jobs holding a permit right now.
    fn running(&self) -> usize {
        self.inner.state.lock().map_or(0, |state| state.active)
    }
}

fn start_memory_watchdog(queue: RenderQueue) {
    let Some(budget) = memory_budget_mib() else {
        println!("Memory watchdog: off (no REOPENSCAD_MAX_RSS_MIB and no container limit found)");
        return;
    };
    if resident_bytes().is_none() {
        println!("Memory watchdog: off (resident memory cannot be read on this platform)");
        return;
    }
    println!("Memory watchdog: renders abort above {budget} MiB resident");
    csg::bsp::MEMORY_LIMIT_MIB.store(budget, Ordering::Relaxed);
    let _ = thread::Builder::new()
        .name("memory-watchdog".into())
        .spawn(move || {
            let mut trimmed_idle = false;
            loop {
                thread::sleep(MEMORY_WATCH_INTERVAL);
                let Some(bytes) = resident_bytes() else {
                    continue;
                };
                let resident = (bytes / (1024 * 1024)) as usize;
                let pressure = &csg::bsp::MEMORY_PRESSURE;
                let busy = queue.running() > 0;
                if !busy {
                    // Nothing is computing, so whatever is resident is the
                    // allocator's cache, not live geometry: never hold
                    // pressure against the next request because of it.
                    if pressure.swap(false, Ordering::Relaxed) {
                        eprintln!("memory watchdog: idle at {resident} MiB; builds resume");
                    }
                    if !trimmed_idle && resident > budget / 2 {
                        release_free_memory();
                    }
                    trimmed_idle = true;
                    continue;
                }
                trimmed_idle = false;
                if resident > budget {
                    csg::bsp::MEMORY_RESIDENT_MIB.store(resident, Ordering::Relaxed);
                    if !pressure.swap(true, Ordering::Relaxed) {
                        eprintln!(
                            "memory watchdog: {resident} MiB > {budget} MiB budget; aborting geometry builds"
                        );
                    }
                    // Aborted builds free their trees, but glibc keeps the
                    // pages; trim every tick while over budget so steady
                    // traffic cannot pin pressure on with cached arenas.
                    release_free_memory();
                } else if pressure.load(Ordering::Relaxed) && resident >= budget * 9 / 10 {
                    release_free_memory();
                } else if resident < budget * 9 / 10 && pressure.swap(false, Ordering::Relaxed) {
                    eprintln!("memory watchdog: back to {resident} MiB; builds resume");
                }
            }
        });
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
    let rate_limits = RateLimits::from_environment()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let state = Arc::new(AppState {
        web_root,
        render_queue: RenderQueue::from_environment(),
        poll_limiter: JobLimiter::new(MAX_CONCURRENT_POLLS),
        rate_limiter: RateLimiter::with_limits(rate_limits),
        cancellations: Arc::new(Mutex::new(HashMap::new())),
        workspaces,
        public_origin,
    });
    start_memory_watchdog(state.render_queue.clone());
    assetstore::start_expiry_sweeper(Arc::clone(&state));
    let listener = TcpListener::bind(format!("{host}:{port}"))?;

    println!("ReOpenSCAD running at http://{host}:{port}");
    println!("Geometry engine: native Rust parser + implicit CSG mesher");
    println!("Per-client rate limits: {}", rate_limits.describe());
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
    const BODY: &[u8] = br#"{"ok":false,"error":"The server is at capacity: it already holds its limit of 256 concurrent connections (MAX_CONNECTIONS), or it is shutting down. This is not about the request or its SCAD code; retry in 2 s.","reason":"connection_limit","retryable":true,"retryAfterSeconds":2,"codeProblem":false}"#;
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
        .unwrap_or(5180);
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
                println!("Usage: reopenscad-server [--host 127.0.0.1] [--port 5180]");
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
    drop(take_upload_slot());
    // An upload is admitted on its head, before any body byte is read: the
    // origin rule, the per-IP rate and byte buckets, the target (a pending
    // upload URL, or an existing design), and only then one of the bounded
    // upload slots. A connection that fails any of these never holds one.
    let mut admitted = false;
    let socket_peer = *peer;
    let request = read_request(stream, &mut |head, content_length| {
        admit_upload_head(state, head, content_length, socket_peer)?;
        admitted = true;
        Ok(())
    });
    // Held until this request is answered: a large upload's body stays in
    // memory through processing.
    let _upload_slot = take_upload_slot();
    let request = match request {
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
        // Name the origin the deployment actually accepts. Saying "localhost"
        // when REOPENSCAD_PUBLIC_ORIGIN is set sends an operator hunting for a
        // networking fault instead of reading one env var.
        let body = match state.public_origin.as_deref() {
            // serde builds the string so an origin containing a quote cannot
            // break out of the JSON body.
            Some(origin) => json!({
                "ok": false,
                "error": format!(
                    "This server only accepts requests for {origin}. Check the Host header, \
                     or REOPENSCAD_PUBLIC_ORIGIN if this origin is wrong."
                ),
            })
            .to_string(),
            None => r#"{"ok":false,"error":"Only same-origin localhost requests are allowed. Set REOPENSCAD_PUBLIC_ORIGIN to serve a public origin."}"#.to_string(),
        };
        return respond_json(stream, 403, &body);
    }
    if matches!(request.method.as_str(), "POST" | "PATCH" | "PUT")
        && !request.body.is_empty()
        && !assetstore::is_upload_route(&request.method, &request.path)
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
    // One cost for admission and the client watch alike.
    let cost = effective_route_cost(&request);
    if let Some(address) = (*peer).filter(|_| !admitted) {
        if let Err(limited) = state.rate_limiter.check(address, cost) {
            return respond_rate_limited(stream, &request.path, &limited);
        }
    }
    // A compute request whose client hangs up is cancelled: compiles, kernel
    // booleans and analysis loops on this thread poll the flag the watch sets.
    let _client_watch = watches_client(&request, cost)
        .then(|| ClientWatch::start(stream, INTERIM_RESPONSES_OK.with(std::cell::Cell::get)))
        .flatten();
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
    if is_design_collection_route(&request.path) && request.method == "POST" {
        return handle_create_workspace(stream, state, &request);
    }
    if let Some((id, action)) = workspace_api_route(&request.path) {
        if action == "assets" || action.starts_with("assets/") {
            return assetstore::handle_route(stream, state, &request, id, &action["assets".len()..]);
        }
        // Every compile of a design's code sees the design's assets.
        let assets_id = workspace_action_compiles(action).then_some(id);
        return assetstore::with_design_assets(state, assets_id, || {
            handle_workspace_route(stream, state, &request, id, action)
        });
    }
    if let Some(token) = request.path.strip_prefix("/api/uploads/") {
        return assetstore::handle_upload_token(stream, state, &request, token.trim_end_matches('/'));
    }
    if let Some(rest) = request.path.strip_prefix("/api/shared/") {
        let rest = rest.trim_end_matches('/');
        if let Some((token, assets)) = rest.split_once("/assets") {
            if assets.is_empty() || assets.starts_with('/') {
                return assetstore::handle_shared_route(stream, state, &request, token, assets);
            }
        }
        if let Some(token) = rest.strip_suffix("/clone") {
            return match request.method.as_str() {
                "POST" => handle_shared_clone(stream, state, &request, token),
                _ => respond_error(stream, 405, "Use POST to copy a read-only design."),
            };
        }
        if let Some(token) = rest.strip_suffix("/render") {
            return match request.method.as_str() {
                "POST" => handle_shared_render(stream, state, &request, token),
                _ => respond_error(stream, 405, "Use POST to render a read-only design."),
            };
        }
        if let Some(token) = rest.strip_suffix("/revisions") {
            return match request.method.as_str() {
                "GET" => handle_shared_revisions(stream, state, token),
                _ => respond_error(stream, 405, "A read-only design cannot be written to."),
            };
        }
        return match request.method.as_str() {
            "GET" => handle_shared_workspace(stream, state, rest),
            _ => respond_error(stream, 405, "A read-only design cannot be written to."),
        };
    }
    if let Some(action) = request.path.strip_prefix("/api/") {
        if agent::BROWSER_CODE_ACTIONS.contains(&action) {
            return match request.method.as_str() {
                "POST" => {
                    // Unsaved code plus a designId compiles with that design's assets.
                    let design = body_asset_design(state, &request.body);
                    assetstore::with_design_assets(state, design.as_deref(), || {
                        agent::handle_agent_route(stream, state, &request, None, action)
                    })
                }
                _ => respond_error(stream, 405, "Use POST with a JSON body carrying code."),
            };
        }
    }
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/api/health") => handle_health(stream),
        ("GET", "/api/stats") => handle_stats(stream, state),
        ("POST", "/api/render") | ("POST", "/api/export") => {
            // The editor's unsaved code, compiled with the assets of the
            // design it names (`designId`/`workspaceId`), if any.
            let design = body_asset_design(state, &request.body);
            assetstore::with_design_assets(state, design.as_deref(), || {
                if request.path == "/api/render" {
                    handle_render(stream, state, &request.body)
                } else {
                    handle_export(stream, state, &request.body)
                }
            })
        }
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
        // Pre-rename design links. Permanent redirect (308 keeps the method,
        // and only GET reaches here) to the canonical `/designs/<id>`, query
        // and all; the response carries the same X-Robots-Tag as any other
        // non-landing route.
        ("GET", path) if legacy_workspace_shell_id(path).is_some() => {
            let id = legacy_workspace_shell_id(path).unwrap_or_default();
            let location = if request.query.is_empty() {
                format!("/designs/{id}")
            } else {
                format!("/designs/{id}?{}", request.query)
            };
            respond_text(
                stream,
                308,
                "text/plain; charset=utf-8",
                b"Moved to /designs/",
                &[("Location", location.as_str())],
            )
        }
        ("GET", path) => handle_static(stream, state, path, request.header("range")),
        ("HEAD", path) if path.starts_with("/media/") => {
            HEAD_ONLY.with(|head| head.set(true));
            let result = handle_static(stream, state, path, request.header("range"));
            HEAD_ONLY.with(|head| head.set(false));
            result
        }
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

thread_local! {
    /// The large-body slot of the request this thread is serving, if it is
    /// an asset upload. Set by `read_request`, taken by `serve_request`.
    static UPLOAD_SLOT: std::cell::RefCell<Option<assetstore::UploadSlot>> = const { std::cell::RefCell::new(None) };
}

fn take_upload_slot() -> Option<assetstore::UploadSlot> {
    UPLOAD_SLOT.with(|current| current.borrow_mut().take())
}

/// Parses one request with hard caps on size and wall-clock time.
///
/// Every accumulator here is bounded *before* it grows: the request line, the
/// header block, the header count, and the body. The socket read timeout is
/// per-syscall, so it alone cannot stop a peer that dribbles one byte just
/// inside the timeout forever; the `Instant` deadlines below do.
/// Called with an upload request's head (no body yet) and its declared
/// length, before the body is read; an `Err` rejects it unread.
type UploadAdmission<'a> = dyn FnMut(&Request, usize) -> Result<(), RequestError> + 'a;

fn read_request(stream: &TcpStream, admit: &mut UploadAdmission<'_>) -> Result<Request, RequestError> {
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
    // HTTP/1.1 and later may be sent an interim 1xx (see `ClientWatch`).
    let interim_ok = parts.next().is_some_and(|version| version.starts_with("HTTP/") && version != "HTTP/1.0" && version != "HTTP/0.9");
    INTERIM_RESPONSES_OK.with(|cell| cell.set(interim_ok));
    let (path, query) = raw_path
        .split_once('?')
        .map_or((raw_path, ""), |(path, query)| (path, query));
    let path = path.to_string();
    let query = query.to_string();
    // Asset uploads carry a raw STL of up to 128 MiB; every other route keeps
    // the 2 MiB ceiling. Decided from the request line, before any body byte.
    let upload = assetstore::is_upload_route(&method, &path);
    let body_limit = if upload { meshimport::MAX_UPLOAD_BYTES } else { MAX_BODY };
    let too_large = || {
        if upload {
            return RequestError::new(413, &assetstore::upload_too_large_message(None));
        }
        let mut message = format!("Request exceeds the {} MiB limit", body_limit / (1024 * 1024));
        if path == "/mcp" {
            // Almost always an upload_asset call carrying a big base64 file.
            message.push_str(
                ". To attach an STL larger than upload_asset's inline limit, call create_asset_upload \
                 {\"designId\", \"name\"} and PUT the file with the curl command (or, over 24 MiB, the chunked \
                 script) it returns.",
            );
        }
        RequestError::new(413, &message)
    };

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
                if content_length > body_limit {
                    return Err(too_large());
                }
            }
            headers.push((name, value));
        }
    }
    if content_length > body_limit {
        return Err(too_large());
    }
    let mut body_window = BODY_DEADLINE;
    if upload {
        let head = Request {
            method: method.clone(),
            path: path.clone(),
            query: query.clone(),
            headers: headers.clone(),
            body: Vec::new(),
        };
        // Takes the upload slot (for a large body) as its last step.
        admit(&head, content_length)?;
        if content_length > MAX_BODY {
            body_window = assetstore::UPLOAD_BODY_DEADLINE;
        }
    }
    let paced = upload && content_length > MAX_BODY;
    if upload
        && headers
            .iter()
            .any(|(name, value)| name == "expect" && value.eq_ignore_ascii_case("100-continue"))
    {
        // curl -T asks before sending a large file; answer so it does not
        // stall a second waiting.
        let _ = (&mut &*stream).write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }

    // Stream the body instead of pre-allocating `content_length`, so a claimed
    // length the peer never sends costs nothing.
    let body_started = Instant::now();
    let body_deadline = body_started + body_window;
    let mut body = Vec::with_capacity(content_length.min(BODY_CHUNK));
    let mut chunk = [0u8; BODY_CHUNK];
    while body.len() < content_length {
        if Instant::now() >= body_deadline {
            return Err(RequestError::new(408, "Timed out while reading the body"));
        }
        if paced && !assetstore::upload_pace_ok(body.len(), body_started.elapsed()) {
            return Err(RequestError::new(
                408,
                "The upload arrived too slowly (under 64 KiB/s); retry on a faster connection",
            ));
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

/// Whether a design action may compile the design's code (and so needs its
/// assets loaded). Plain reads, polls and comment routes do not.
fn workspace_action_compiles(action: &str) -> bool {
    !(matches!(action, "events" | "preview" | "code" | "share" | "revisions" | "activity")
        || action.starts_with("activity/")
        || action == "feedback"
        || action.starts_with("feedback/")
        || action == "assets"
        || action.starts_with("assets/"))
}

/// The design whose assets a stateless compile route (`/api/render`,
/// `/api/export`, the analysis routes) installs: `designId`/`workspaceId` of
/// the JSON body, or the design behind a read-only `shareToken`. Only the
/// assets are taken from it; the body's code is what compiles, and nothing
/// in the reply names the design.
fn body_asset_design(state: &AppState, body: &[u8]) -> Option<String> {
    let input = serde_json::from_slice::<Value>(body).ok()?;
    if let Some(id) = input_design_id(&input) {
        return Some(id.to_string());
    }
    let token = input.get("shareToken").and_then(Value::as_str)?;
    state
        .workspaces
        .resolve_share(token)
        .ok()
        .flatten()
        .map(|workspace| workspace.id)
}

/// Pre-body admission of an asset upload (see `serve_request`): everything
/// that can refuse it is checked before it may take an upload slot.
fn admit_upload_head(
    state: &AppState,
    head: &Request,
    content_length: usize,
    socket_peer: Option<IpAddr>,
) -> Result<(), RequestError> {
    if !request_is_allowed(head, state) {
        return Err(RequestError::new(403, "This server does not accept requests for this host or origin."));
    }
    let peer = forwarded_client_ip(head).or(socket_peer);
    if let Some(address) = peer {
        if let Err(limited) = state.rate_limiter.check(address, route_cost(&head.method, &head.path)) {
            return Err(RequestError::new(
                429,
                &format!(
                    "Too many requests from your address ({} budget); retry in {} seconds.",
                    limited.bucket,
                    limited.retry_after.ceil().max(1.0) as u64
                ),
            ));
        }
        // The body is billed before it is read: a client out of byte budget
        // is refused here on its next upload.
        state.rate_limiter.charge_bytes(address, content_length as u64);
    }
    assetstore::admit_upload_target(state, head)
        .map_err(|error| RequestError::new(error.status, &error.message))?;
    // A chunk of a chunked upload: its range is checked before its body.
    let range = assetstore::upload_range(&head.headers, content_length)
        .map_err(|error| RequestError::new(error.status, &error.message))?;
    if let Some(address) = peer {
        // The hourly raw-upload allowance (stored bytes have their own
        // quota, charged after cleaning).
        assetstore::raw_upload_admit(address, content_length as u64, unix_millis(), assetstore::client_raw_upload_bytes())
            .map_err(|error| RequestError::new(error.status, &error.message))?;
    }
    // The last chunk of a chunked upload processes the whole file, so it is
    // sized by the file, not by the chunk.
    let processed = range
        .filter(|range| range.is_final())
        .map_or(content_length, |range| range.total.min(usize::MAX as u64) as usize)
        .max(content_length);
    if processed > MAX_BODY {
        // A bounded number of large bodies are held at once (a couple per
        // client); the slot lives until the request is answered (see
        // `take_upload_slot`).
        let slot = assetstore::UploadSlot::try_acquire(peer, processed > assetstore::LARGE_UPLOAD_BYTES)
            .map_err(|message| RequestError::new(503, message))?;
        UPLOAD_SLOT.with(|current| *current.borrow_mut() = Some(slot));
    }
    Ok(())
}

/// `/api/designs` is the canonical collection; `/api/workspaces` is the
/// pre-rename alias. Both resolve here, so admission (`route_cost`) and
/// dispatch can never tell them apart.
fn is_design_collection_route(path: &str) -> bool {
    path == "/api/designs" || path == "/api/workspaces"
}

/// `/api/designs/{id}/{action}`, or its pre-rename alias
/// `/api/workspaces/{id}/{action}`, as `(id, action)`.
fn workspace_api_route(path: &str) -> Option<(&str, &str)> {
    let remainder = path
        .strip_prefix("/api/designs/")
        .or_else(|| path.strip_prefix("/api/workspaces/"))?;
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
    // A caller may seed the settings of the new design.
    let plate = input
        .get("plate")
        .and_then(Value::as_str)
        .map(str::to_string);
    let tolerance = input.get("tolerance").and_then(Value::as_f64);
    match state
        .workspaces
        .create_with_settings(code, name, plate, tolerance)
    {
        Ok(workspace) => respond_workspace(stream, 201, state, request, &workspace, &[]),
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
            Ok(Some(workspace)) => {
                if wants_object_compute(request)
                    && agent::object_cache(id, workspace.revision).is_none()
                {
                    let _permit = match state
                        .render_queue
                        .acquire(current_client_key(), None, None)
                    {
                        Ok(permit) => permit,
                        Err(rejection) => return respond_queue_rejection(stream, rejection),
                    };
                    match compile_parts_guarded(&workspace.code, engine::Quality::Preview) {
                        Ok(compiled) => {
                            agent::remember_objects(id, workspace.revision, &compiled.parts)
                        }
                        Err(error) => {
                            agent::remember_compile_failure(id, workspace.revision, &error)
                        }
                    }
                } else if wants_opportunistic_objects(request) {
                    compile_objects_if_idle(state, &workspace);
                }
                respond_workspace(stream, 200, state, request, &workspace, &[])
            }
            Ok(None) if allow_missing_workspace(request) => respond_value(
                stream,
                200,
                &json!({"ok": true, "missing": true, "designId": id, "workspaceId": id}),
            ),
            Ok(None) => respond_error(stream, 404, "Design not found."),
            Err(error) => respond_error(stream, 500, &error),
        },
        ("PATCH", "") => handle_update_workspace(stream, state, request, id),
        ("GET", "code") => handle_query_workspace_code(stream, state, request, id),
        ("GET", "preview") => handle_get_workspace_preview(stream, state, id),
        ("POST", "share") => handle_create_share_link(stream, state, request, id),
        ("PUT", "preview") => handle_put_workspace_preview(stream, state, request, id),
        ("GET", "events") => handle_workspace_events(stream, state, request, id),
        (_, "activity") => activity::handle_route(stream, request, id, ""),
        (_, action) if action.starts_with("activity/") => {
            activity::handle_route(stream, request, id, &action["activity".len()..])
        }
        (_, "feedback") => feedback::handle_route(stream, state, request, id, ""),
        (_, action) if action.starts_with("feedback/") => {
            feedback::handle_route(stream, state, request, id, &action["feedback/".len()..])
        }
        ("POST", "render") => handle_workspace_render(stream, state, request, id),
        ("GET", "export") | ("POST", "export") => {
            handle_workspace_export(stream, state, request, id)
        }
        ("GET", "objects")
        | ("POST", "objects")
        | ("POST", "measure")
        | ("POST", "compare")
        | ("POST", "image")
        | ("GET", "image")
        | ("POST", "diff")
        | ("GET", "diff")
        | ("POST", "intersections")
        | ("POST", "evaluate")
        | ("GET", "print-layout")
        | ("POST", "print-layout")
        | ("GET", "parameters")
        | ("POST", "parameters")
        | ("GET", "revisions")
        | ("POST", "revisions") => agent::handle_agent_route(stream, state, request, Some(id), action),
        _ => respond_error(stream, 404, "Design endpoint not found."),
    }
}

/// `?objects=compute` on a workspace read: compile when the object list is
/// not already cached, rather than answering with an empty list.
fn wants_object_compute(request: &Request) -> bool {
    request.method == "GET"
        && workspace_api_route(&request.path).is_some_and(|(_, action)| action.is_empty())
        && query_parameters(&request.query)
            .get("objects")
            .is_some_and(|value| value == "compute")
}

/// A plain workspace read from a non-browser client (an agent, curl) fills a
/// missing object list when that costs nobody anything: see
/// [`compile_objects_if_idle`]. Browsers are skipped — the editor renders the
/// workspace itself right after loading it and takes the objects from that
/// render — as is `?objects=none`.
fn wants_opportunistic_objects(request: &Request) -> bool {
    request.header("sec-fetch-mode").is_none()
        && query_parameters(&request.query)
            .get("objects")
            .is_none_or(|value| value != "none")
}

/// Compile `workspace` for its object list if the list is not cached, an
/// evaluator slot is free *right now* (no queue, no wait) and the client's
/// per-IP evaluator budget can pay for it. Otherwise nothing happens and the
/// read answers `objectsStatus: "not_compiled"` with a hint, so a plain read
/// can never be used to queue evaluator work or to exceed the evaluator rate
/// limit: the read itself is charged as cheap, and the compile is charged
/// separately, only when it runs.
fn compile_objects_if_idle(state: &AppState, workspace: &Workspace) {
    if agent::object_cache(&workspace.id, workspace.revision).is_some() {
        return;
    }
    let Some(_permit) = state.render_queue.try_acquire_idle() else {
        return;
    };
    if state.rate_limiter.charge_extra_compile().is_err() {
        return;
    }
    match compile_parts_guarded(&workspace.code, engine::Quality::Preview) {
        Ok(compiled) => agent::remember_objects(&workspace.id, workspace.revision, &compiled.parts),
        Err(error) => agent::remember_compile_failure(&workspace.id, workspace.revision, &error),
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
    let mut input = match parse_json_body(&request.body) {
        Ok(input) => input,
        Err(error) => return respond_error(stream, 400, &format!("Invalid update: {error}")),
    };
    // Patch forms (`startLine`/`endLine`/`text`, `edits`, `values`) are
    // resolved to a byte-range edit first. That needs no evaluator; the
    // validation below does, under its permit.
    if let Err(error) = agent::resolve_patch(state, id, &mut input) {
        return agent::respond_update_error(stream, state, request, error);
    }
    let update = match serde_json::from_value::<WorkspaceUpdate>(input) {
        Ok(update) => update,
        Err(error) => return respond_error(stream, 400, &format!("Invalid update: {error}")),
    };
    // A PATCH that carries only viewer settings is not an edit: it takes the
    // revisionless path so changing a setting cannot conflict with, or
    // invalidate, an in-flight code save.
    //
    // It also runs BEFORE the evaluator permit is taken. This path never calls
    // `validate_source`, so charging it a permit would let a busy render queue
    // answer a settings change with a 429.
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
            Ok(workspace) => respond_workspace(stream, 200, state, request, &workspace, &[]),
            Err(UpdateError::NotFound) => respond_error(stream, 404, "Design not found."),
            Err(UpdateError::Internal(error)) => respond_error(stream, 500, &error),
            Err(_) => respond_error(stream, 400, "Design settings could not be updated."),
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
            &result.events,
        ),
        Err(UpdateError::NotFound) => respond_error(stream, 404, "Design not found."),
        Err(UpdateError::Conflict(workspace)) => respond_value(
            stream,
            409,
            &json!({
                "ok": false,
                "error": "Design revision conflict.",
                "workspace": workspace_public(&workspace, state, request, &[], None)
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
        Ok(None) => return respond_error(stream, 404, "Design not found."),
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

/// Mints (or returns) the read-only link for a workspace.
///
/// Idempotent, so sharing twice hands back the same link and one already sent
/// to somebody keeps working.
fn handle_create_share_link(
    stream: &mut TcpStream,
    state: &AppState,
    request: &Request,
    id: &str,
) -> io::Result<()> {
    match state.workspaces.share_token(id) {
        Ok(token) => respond_value(
            stream,
            200,
            &json!({
                "ok": true,
                "designId": id,
                "workspaceId": id,
                "shareToken": token,
                "url": agent::shared_url(state, request, &token)
            }),
        ),
        Err(UpdateError::NotFound) => respond_error(stream, 404, "Design not found."),
        Err(UpdateError::Internal(message)) => respond_error(stream, 500, &message),
        Err(_) => respond_error(stream, 500, "Could not create a read-only link."),
    }
}

/// The read-only view of a shared workspace.
///
/// Answers the source and the viewer settings and **not** the workspace id.
/// That omission is the entire security property of the feature: a read-only
/// link that carried the id would be an editable link with an extra step.
fn handle_shared_workspace(
    stream: &mut TcpStream,
    state: &AppState,
    token: &str,
) -> io::Result<()> {
    match state.workspaces.resolve_share(token) {
        Ok(Some(workspace)) => respond_value(
            stream,
            200,
            &json!({
                "ok": true,
                "readOnly": true,
                "code": workspace.code,
                "revision": workspace.revision,
                "plate": workspace.plate,
                "tolerance": workspace.tolerance,
            }),
        ),
        Ok(None) => respond_error(stream, 404, "This read-only link is not valid."),
        Err(error) => respond_error(stream, 500, &error),
    }
}

/// The stored render thumbnail, or 404 when nothing has rendered this
/// workspace yet.
///
/// A separate route rather than a field on the workspace payload: the recents
/// list asks for a dozen of these at once and wants them cached, while every
/// other consumer of a workspace — the editor, the long poll, the MCP tools —
/// wants nothing to do with a base64 image.
fn handle_get_workspace_preview(
    stream: &mut TcpStream,
    state: &AppState,
    id: &str,
) -> io::Result<()> {
    let workspace = match state.workspaces.get(id) {
        Ok(Some(workspace)) => workspace,
        Ok(None) => return respond_error(stream, 404, "Design not found."),
        Err(error) => return respond_error(stream, 500, &error),
    };
    if workspace.preview.is_empty() {
        return respond_error(stream, 404, "No preview has been stored for this design.");
    }
    respond_value(
        stream,
        200,
        &json!({"ok": true, "designId": id, "workspaceId": id, "preview": workspace.preview}),
    )
}

/// Stores a render thumbnail drawn by the browser.
///
/// Accepts only a `data:image/...;base64,` URL, and only one small enough to
/// belong in a workspace document. Both checks are the point: this is the one
/// endpoint that takes opaque bytes and hands them back to another browser to
/// render, so anything that is not plainly an image — an `svg` with script in
/// it, an arbitrary `data:text/html` — has no business being stored under a
/// key the app will later put in an `img` tag.
fn handle_put_workspace_preview(
    stream: &mut TcpStream,
    state: &AppState,
    request: &Request,
    id: &str,
) -> io::Result<()> {
    let input = match parse_json_body(&request.body) {
        Ok(input) => input,
        Err(error) => return respond_error(stream, 400, &error),
    };
    let preview = input
        .get("preview")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if preview.is_empty() {
        return respond_error(stream, 400, "A preview data URL is required.");
    }
    if preview.len() > MAX_PREVIEW_BYTES {
        return respond_error(
            stream,
            413,
            &format!(
                "Preview exceeds the {} KiB limit.",
                MAX_PREVIEW_BYTES / 1024
            ),
        );
    }
    if !is_safe_preview_data_url(preview) {
        return respond_error(
            stream,
            400,
            "A preview must be a base64 PNG, JPEG or WebP data URL.",
        );
    }
    match state.workspaces.set_preview(id, preview) {
        Ok(()) => respond_value(stream, 200, &json!({"ok": true, "designId": id, "workspaceId": id})),
        Err(UpdateError::NotFound) => respond_error(stream, 404, "Design not found."),
        Err(UpdateError::Internal(message)) => respond_error(stream, 500, &message),
        // `set_preview` performs no validation and takes no revision, so the
        // remaining variants cannot arise; answering 500 is the honest reply
        // if that ever stops being true.
        Err(_) => respond_error(stream, 500, "Could not store the preview."),
    }
}

/// True for a `data:` URL this app is willing to store and serve back.
///
/// Raster types only, base64 only, and a strict base64 alphabet — no SVG
/// (which can carry script), no `charset`/parameter soup, nothing that could
/// be coaxed into being interpreted as anything but an image.
fn is_safe_preview_data_url(value: &str) -> bool {
    const ALLOWED: &[&str] = &[
        "data:image/png;base64,",
        "data:image/jpeg;base64,",
        "data:image/webp;base64,",
    ];
    let Some(prefix) = ALLOWED.iter().find(|prefix| value.starts_with(**prefix)) else {
        return false;
    };
    let payload = &value[prefix.len()..];
    !payload.is_empty()
        && payload
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
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
        "designId": workspace.id,
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
                br#"{"ok":false,"error":"Too many open design subscriptions: the server holds at most 32 concurrent /events long polls (MAX_CONCURRENT_POLLS). Close idle tabs or retry in 1 s.","reason":"poll_limit","retryable":true,"retryAfterSeconds":1,"codeProblem":false}"#,
                &[("Retry-After", "1")],
            )
        }
    };
    let parameters = query_parameters(&request.query);
    let since = query_u64(&parameters, "since").unwrap_or(0);
    // `feedback=<version>` comes from a browser that shows model comments:
    // it counts as a connected viewer for `request_pointing`, and a
    // feedback change ends its poll as a code change does.
    let feedback_since = query_u64(&parameters, "feedback");
    if feedback_since.is_some() {
        state.workspaces.note_viewer(id);
    }
    // The wait loop asks only for the revision. With workspaces in Postgres a
    // full read per tick would stream the whole document (up to 2 MiB) out of
    // the database 26 times per poll per subscriber; the document is fetched
    // once, after the loop decides there is something to say.
    // `assets=<version>` comes from a browser that knows the design's asset
    // version: an asset upload or delete ends its poll as a code change does.
    let assets_since = query_u64(&parameters, "assets");
    // `activity=<version>` comes from a browser that shows what agents do:
    // an agent's tool call starting or finishing ends its poll too.
    let activity_since = query_u64(&parameters, "activity");
    let mut settled = false;
    for attempt in 0..=25 {
        let probe = match assets_since {
            Some(_) => state
                .workspaces
                .poll_state(id)
                .map(|state| state.map(|(revision, assets)| (revision, Some(assets)))),
            None => state
                .workspaces
                .revision_of(id)
                .map(|revision| revision.map(|revision| (revision, None))),
        };
        match probe {
            Ok(Some((revision, assets_version))) => {
                let feedback_moved = feedback_since.is_some_and(|seen| {
                    state
                        .workspaces
                        .feedback_summary(id)
                        .is_ok_and(|(version, _)| version != seen)
                });
                let assets_moved = assets_since.is_some_and(|seen| assets_version != Some(seen));
                let activity_moved = activity_since.is_some_and(|seen| activity::version(id) != seen);
                if revision > since || feedback_moved || assets_moved || activity_moved || attempt == 25 {
                    settled = true;
                    break;
                }
            }
            Ok(None) => return respond_error(stream, 404, "Design not found."),
            Err(error) => return respond_error(stream, 500, &error),
        }
        thread::sleep(Duration::from_millis(200));
    }
    debug_assert!(settled);
    let workspace = match state.workspaces.get(id) {
        // Deleted between the probe and the read: rare, but the poll must
        // answer 404 rather than panic on a missing document.
        Ok(Some(workspace)) => workspace,
        Ok(None) => return respond_error(stream, 404, "Design not found."),
        Err(error) => return respond_error(stream, 500, &error),
    };
    let mut payload = workspace_events_payload(id, &workspace, since);
    // Revisionless like the viewer settings: an asset write changes what the
    // same revision compiles to, so the browser re-renders when this moves.
    let assets_version = state.workspaces.assets_version(id).unwrap_or(0);
    payload["assetsVersion"] = json!(assets_version);
    payload["assetsChanged"] = json!(assets_since.is_some_and(|seen| seen != assets_version));
    if feedback_since.is_some() {
        if let Ok(feedback) = state.workspaces.feedback_state(id) {
            payload["feedback"] = feedback::events_summary(&feedback, unix_millis());
        }
    }
    if let Some(seen) = activity_since {
        payload["activity"] = activity::since(id, seen);
    }
    respond_value(stream, 200, &payload)
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
        "designId": id,
        "workspaceId": id,
        "changed": changed,
        "revision": workspace.revision,
        "revisionName": revision_label(&workspace.revision_name, workspace.revision),
        "objects": [],
        "updatedRanges": &events,
        // Viewer settings ride every poll, not just changed ones: they are revisionless, so
        // `changed` says nothing about them. Without these a settings change would never
        // reach a tab already looking at the workspace.
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
    let workspace = match state.workspaces.get(id) {
        Ok(Some(workspace)) => workspace,
        Ok(None) => return respond_error(stream, 404, "Design not found."),
        Err(error) => return respond_error(stream, 500, &error),
    };
    if input.get("overrides").is_some_and(|value| !value.is_null()) {
        return respond_error(stream, 400, &removed_argument_error("render", "overrides"));
    }
    render_stored_workspace(stream, state, &input, workspace, RenderAccess::Editable)
}

/// `POST /api/shared/{token}/render`: the same per-part, coloured, cached
/// render the editor gets, for a read-only link. The stored source is
/// compiled, never one from the body, and the reply omits the design id.
fn handle_shared_render(
    stream: &mut TcpStream,
    state: &AppState,
    request: &Request,
    token: &str,
) -> io::Result<()> {
    let input = match parse_json_body(&request.body) {
        Ok(value) => value,
        Err(error) => return respond_error(stream, 400, &error),
    };
    let workspace = match state.workspaces.resolve_share(token) {
        Ok(Some(workspace)) => workspace,
        Ok(None) => return respond_error(stream, 404, "This read-only link is not valid."),
        Err(error) => return respond_error(stream, 500, &error),
    };
    if input.get("overrides").is_some_and(|value| !value.is_null()) {
        return respond_error(stream, 400, &removed_argument_error("render", "overrides"));
    }
    let id = workspace.id.clone();
    assetstore::with_design_assets(state, Some(&id), || {
        render_stored_workspace(stream, state, &input, workspace, RenderAccess::ReadOnly)
    })
}

/// `GET /api/shared/{token}/revisions`: the history a read-only link may look
/// at (it cannot restore), without the design id.
fn handle_shared_revisions(stream: &mut TcpStream, state: &AppState, token: &str) -> io::Result<()> {
    let workspace = match state.workspaces.resolve_share(token) {
        Ok(Some(workspace)) => workspace,
        Ok(None) => return respond_error(stream, 404, "This read-only link is not valid."),
        Err(error) => return respond_error(stream, 500, &error),
    };
    match agent::revisions_public(state, &workspace) {
        Ok(value) => respond_value(stream, 200, &value),
        Err(error) => respond_error(stream, error.status, &error.message),
    }
}

/// `POST /api/shared/{token}/clone`: a new, editable design from a
/// read-only link, holding the body's `code` (else the stored source), the
/// plate and tolerance, and server-side copies of the design's mesh assets
/// (billed to the caller's storage quota). Answers like `POST /api/designs`.
fn handle_shared_clone(
    stream: &mut TcpStream,
    state: &AppState,
    request: &Request,
    token: &str,
) -> io::Result<()> {
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
    let source = match state.workspaces.resolve_share(token) {
        Ok(Some(workspace)) => workspace,
        Ok(None) => return respond_error(stream, 404, "This read-only link is not valid."),
        Err(error) => return respond_error(stream, 500, &error),
    };
    let code = input
        .get("code")
        .and_then(Value::as_str)
        .map_or_else(|| source.code.clone(), str::to_string);
    // Validated against the assets the copy is about to hold.
    if let Err(error) = assetstore::with_design_assets(state, Some(&source.id), || validate_source(&code)) {
        return respond_validation_error(stream, error);
    }
    if let Err(error) = assetstore::copy_quota_check(state, &source.id) {
        return respond_error(stream, error.status, &error.message);
    }
    let workspace = match state.workspaces.create_with_settings(
        code,
        None,
        Some(source.plate.clone()),
        Some(source.tolerance),
    ) {
        Ok(workspace) => workspace,
        Err(error) => return respond_error(stream, 500, &error),
    };
    if let Err(error) = assetstore::copy_assets(state, &source.id, &workspace.id) {
        return respond_error(
            stream,
            error.status,
            &format!("The copy could not take the design's meshes: {}", error.message),
        );
    }
    respond_workspace(stream, 201, state, request, &workspace, &[])
}

/// Who asked for a stored design's render: the id holder, or a read-only
/// link. Only the first may see the design id in the reply.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RenderAccess {
    Editable,
    ReadOnly,
}

fn render_stored_workspace(
    stream: &mut TcpStream,
    state: &AppState,
    input: &Value,
    workspace: Workspace,
    access: RenderAccess,
) -> io::Result<()> {
    let id = workspace.id.as_str();
    let revision = match input.get("revision").and_then(Value::as_u64) {
        Some(revision) => revision,
        None => return respond_error(stream, 400, "revision is required for design render."),
    };
    let request_id = input
        .get("requestId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !valid_request_id(request_id) {
        return respond_error(stream, 400, "A valid requestId is required.");
    }
    let object_ids = match requested_object_ids(input, &HashMap::new()) {
        Ok(object_ids) => object_ids,
        Err(error) => return respond_error(stream, 400, &error),
    };
    if workspace.revision != revision {
        return respond_value(
            stream,
            409,
            &json!({"ok": false, "error": "Design revision conflict.", "currentRevision": workspace.revision}),
        );
    }
    let quality = if input.get("mode").and_then(Value::as_str) == Some("render") {
        engine::Quality::Render
    } else {
        engine::Quality::Preview
    };
    let mode_echo = input.get("mode").and_then(Value::as_str).unwrap_or("preview");
    // An unchanged revision is served from storage before it can queue: that
    // is what makes reopening a design instant rather than another compile.
    let cache_key = rendercache::render_key(
        if quality == engine::Quality::Render { "render" } else { "preview" },
        &workspace.code,
        &workspace.code,
        &json!([]),
        object_ids.as_deref(),
    );
    let serve_cached = |stream: &mut TcpStream| -> Option<io::Result<()>> {
        let stored = state.workspaces.cached_render(id, &cache_key)?;
        let body = cached_render_body(stored, &workspace, mode_echo, quality, access)?;
        Some(respond_text(
            stream,
            200,
            "application/json; charset=utf-8",
            &body,
            &[("X-Render-Cache", "hit")],
        ))
    };
    // Held through transmission. When every hit slot is busy, the lookup
    // waits for an evaluator slot below instead, so a burst of hits is
    // admitted like a burst of renders rather than buffered without bound.
    let hit_permit = rendercache::HitPermit::try_acquire();
    if hit_permit.is_some() {
        if let Some(written) = serve_cached(stream) {
            return written;
        }
    }
    let hit_slots_were_busy = hit_permit.is_none();
    drop(hit_permit);
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
    if hit_slots_were_busy {
        if let Some(written) = serve_cached(stream) {
            return written;
        }
    }
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
    let selection_requested = object_ids.is_some();
    let sends_combined = sends_combined_mesh(&compiled, selection_requested);
    if multipart_binary_size(&compiled, sends_combined).is_none_or(|size| size > MAX_OUTPUT) {
        return respond_error(stream, 422, RENDER_RESPONSE_TOO_LARGE);
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
    if !multipart_response_fits(&compiled, &response_mesh, sends_combined) {
        return respond_error(stream, 422, RENDER_RESPONSE_TOO_LARGE);
    }
    // Preview only: that is the quality every other caller caches, and a
    // render-mode compile may carry different triangles.
    if quality == engine::Quality::Preview {
        agent::remember_objects(id, workspace.revision, &compiled.parts);
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
    let combined_mesh = sends_combined.then(|| base64_encode(&response_mesh.binary_stl()));
    // `%` objects travel under their own key rather than in `objects`: the
    // viewer may draw them transparently, but they are not addressable by the
    // visibility/selection protocol and never appear in `mesh` or an export,
    // matching OpenSCAD's background modifier.
    let background_objects: Vec<_> = compiled.background_parts.iter().map(part_public).collect();
    // Everything here is a function of the cache key's inputs; the
    // per-request fields (ids, revision, timings, the echoed mode) are added
    // on the way out, both here and on a hit.
    let mut response = json!({
        "ok": true,
        "mesh": combined_mesh,
        "triangles": response_mesh.triangles.len(),
        "degenerateTriangles": degenerate_triangle_count(&response_mesh),
        "console": compiled.messages.join("\n"),
        "warnings": agent::warning_lines(&compiled.messages),
        "overrides": [],
        "objects": objects,
        "backgroundObjects": background_objects
    });
    let stored = rendercache::encode_entry(&agent::object_summaries_value(&compiled.parts), &response);
    if access == RenderAccess::Editable {
        response["designId"] = json!(id);
        response["workspaceId"] = json!(id);
    }
    response["revision"] = json!(workspace.revision);
    // Compute time, deliberately exclusive of the wait — the same interval
    // the deadline measures. The wait is reported separately so a caller can
    // tell a slow model from a busy server.
    response["elapsedMs"] = json!(started.elapsed().as_millis());
    response["queuedMs"] = json!(queued_ms);
    response["mode"] = json!(mode_echo);
    response["cached"] = json!(false);
    let body = match serde_json::to_vec(&response) {
        Ok(body) => body,
        Err(error) => return respond_error(stream, 500, &format!("JSON encoding failed: {error}")),
    };
    drop(response);
    // The mesh checks above do not see the rest of the JSON (per-triangle
    // color runs, console, object metadata); this is the limit on the whole.
    if body.len() > MAX_OUTPUT {
        return respond_error(stream, 422, RENDER_RESPONSE_TOO_LARGE);
    }
    let written = respond_text(
        stream,
        200,
        "application/json; charset=utf-8",
        &body,
        &[("X-Render-Cache", "miss")],
    );
    // After the answer, and without the evaluator slot: storing a few MB is
    // not compute, and the caller should not wait for it.
    drop(_permit);
    // Unseeded `rands()` differs on every run: storing it would freeze one
    // draw as the design's geometry.
    if let Some(stored) = stored.filter(|_| !compiled.nondeterministic) {
        state.workspaces.store_render(id, &cache_key, &stored);
    }
    written
}

/// The response for a stored render, with this request's fields spliced on,
/// or `None` when the entry is malformed (treated as a miss). Only the
/// object summaries are parsed; the meshes are passed through as bytes.
fn cached_render_body(
    stored: Vec<u8>,
    workspace: &Workspace,
    mode_echo: &str,
    quality: engine::Quality,
    access: RenderAccess,
) -> Option<Vec<u8>> {
    let (summaries, open) = rendercache::decode_entry(stored)?;
    // A hit fills the object list a design read reports, as a compile would.
    if quality == engine::Quality::Preview {
        agent::remember_object_summaries(&workspace.id, workspace.revision, &summaries);
    }
    let mut fields = json!({
        "revision": workspace.revision,
        "elapsedMs": 0,
        "queuedMs": 0,
        "mode": mode_echo,
        "cached": true,
    });
    if access == RenderAccess::Editable {
        fields["designId"] = json!(workspace.id);
        fields["workspaceId"] = json!(workspace.id);
    }
    Some(rendercache::close_response(open, fields.as_object()?))
}

const RENDER_RESPONSE_TOO_LARGE: &str = "This model is too large to display: its render response \
     exceeds the 64 MiB limit (about 1,000,000 triangles across all objects). Reduce its triangle \
     count, for example by importing the mesh with a lower triangle budget.";

/// Whether a render response carries the combined mesh on top of the per-object
/// ones: only when it is not recoverable from `objects`.
fn sends_combined_mesh(output: &engine::MultipartCompileOutput, selection_requested: bool) -> bool {
    selection_requested || output.parts.is_empty()
}

fn multipart_binary_size(
    output: &engine::MultipartCompileOutput,
    include_combined: bool,
) -> Option<usize> {
    include_combined
        .then_some(&output.mesh)
        .into_iter()
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
    sends_combined: bool,
) -> bool {
    let response_size = if sends_combined { mesh_binary_size(response_mesh) } else { Some(0) };
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
        .is_some_and(|encoded| encoded <= MAX_OUTPUT)
}

/// A compiled object without its mesh payload.
///
/// The MCP object listing and the workspace payload both need to name the
/// addressable parts, but neither should ship a base64 STL per part: an AI
/// client wants IDs, sizes and extents, and the browser gets meshes from the
/// render route instead.
fn part_summary_public(part: &engine::CompiledPart) -> Value {
    // Bounds of the mesh itself, not of the shape tree: the tree's bounds are
    // conservative (a `difference` reports its first operand's box, a hull its
    // inputs'), which is how one body's z-max used to be reported as its
    // neighbour's.
    let bounds = agent::mesh_bounds(&part.mesh).map_or_else(
        || json!({"min": vec3_public(part.bounds.min), "max": vec3_public(part.bounds.max)}),
        |bounds| json!({"min": bounds.min, "max": bounds.max}),
    );
    json!({
        "id": part.id,
        "name": part.name,
        "kind": "solid",
        "triangles": part.mesh.triangles.len(),
        "bounds": bounds,
        "sourceRange": part.source_range.map(|range| json!({"start": range.start, "end": range.end})),
        "visible": part.visible,
        "selectable": part.selectable,
        "color": part.color,
        "colors": part.face_colors.as_ref().map(|colors| &colors.palette),
        "background": part.background
    })
}

/// An object painted in several colours, for the viewer: the palette and
/// `[first triangle, count, palette slot]` runs over the object's STL. Null
/// for a one-colour object, which `color` describes.
fn face_colors_public(part: &engine::CompiledPart) -> Value {
    part.face_colors.as_ref().map_or(Value::Null, |colors| {
        json!({
            "palette": colors.palette,
            "runs": colors.runs().iter().map(|(first, count, slot)| json!([first, count, slot])).collect::<Vec<_>>(),
        })
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
        // Per-triangle colours when the object is painted in several.
        "faceColors": face_colors_public(part),
        "background": part.background
    })
}

fn vec3_public(point: engine::Vec3) -> Value {
    json!([point.x, point.y, point.z])
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
        return respond_error(stream, 400, "revision is required for design export.");
    };
    let workspace = match state.workspaces.get(id) {
        Ok(Some(workspace)) if workspace.revision == revision => workspace,
        Ok(Some(workspace)) => {
            return respond_value(
                stream,
                409,
                &json!({"ok": false, "error": "Design revision conflict.", "currentRevision": workspace.revision}),
            )
        }
        Ok(None) => return respond_error(stream, 404, "Design not found."),
        Err(error) => return respond_error(stream, 500, &error),
    };
    let object_ids = match requested_object_ids(&input, &parameters) {
        Ok(object_ids) => object_ids,
        Err(error) => return respond_error(stream, 400, &error),
    };
    if input.get("overrides").is_some_and(|value| !value.is_null()) || parameters.contains_key("overrides") {
        return respond_error(stream, 400, &removed_argument_error("export", "overrides"));
    }
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
    let options = match ExportOptions::from_request(format.as_str(), &input, &parameters) {
        Ok(options) => options,
        Err(error) => return respond_error(stream, 400, &error),
    };
    // `print=1`: each object in its `// @print` orientation on z = 0;
    // `layout=1`: one plate of the automatic bed layout.
    let print = match printlayout::export_print_options(&input, &parameters) {
        Ok(print) => print,
        Err(error) => return respond_error(stream, 400, &error),
    };
    let exported = match print {
        None => export_source(&workspace.code, object_ids.as_deref(), options, &workspace.name),
        Some(_) if options.union => Err("print/layout exports keep objects separate; drop union.".to_string()),
        Some(print) => export_bodies(&workspace.code, object_ids.as_deref(), false, &workspace.name)
            .and_then(|mut bodies| {
                printlayout::apply_print_poses(&workspace.code, &mut bodies, &print, &workspace.plate)?;
                encode_bodies(bodies, options.format, &workspace.name)
            }),
    };
    let (data, report) = match exported {
        Ok(data) => data,
        Err(error) => return respond_error(stream, 422, &error),
    };
    respond_export(stream, format.as_str(), &data, &report)
}

/// The formats every export surface offers: the workspace export route, the
/// stateless `/api/export` route and the MCP `render_workspace` tool. Keeping
/// one list means a new format can never be wired into just one of them.
/// The format id doubles as the file extension (`model.{format}`), so these
/// are the extensions users see on the download.
const EXPORT_FORMATS: [&str; 6] = ["stl", "off", "obj", "3mf", "step", "glb"];

fn is_supported_export_format(format: &str) -> bool {
    EXPORT_FORMATS.contains(&format)
}

fn supported_export_formats_sentence() -> String {
    format!("Supported export formats are {}.", EXPORT_FORMATS.join(", "))
}

/// The one message every oversized export answers with, so the HTTP routes and
/// the MCP tool cannot describe the same limit two different ways.
const EXPORT_TOO_LARGE: &str = "Export exceeds the 64 MiB limit.";

/// Serialize a mesh in one of [`EXPORT_FORMATS`].
///
/// `title` reaches the formats that carry metadata — 3MF's title, STEP's
/// product name, glTF's node name — and the bare mesh formats ignore it.
/// Callers must validate the format first.
///
/// The size check lives here rather than at each call site because the two
/// newest formats need it *during* serialisation, not after. STEP runs about
/// ten times the size of the same model's STL, so "build it, then measure it"
/// would let a single request materialise hundreds of megabytes on its way to
/// a rejection — a cheap way to push this server over its memory limit. Both
/// writers take the budget and give up the moment they cross it.
#[cfg(test)]
fn encode_mesh(mesh: &engine::Mesh, format: &str, title: &str) -> Result<Vec<u8>, String> {
    let options = ExportOptions::from_request(format, &json!({}), &HashMap::new())?;
    let body = export::ExportBody {
        id: "model".into(),
        name: title.into(),
        color: None,
        face_colors: None,
        mesh: mesh.clone(),
    };
    encode_bodies(vec![body], options.format, title).map(|(data, _)| data)
}

/// How an export request wants its bodies written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExportOptions {
    format: export::Format,
    /// Boolean-union every exported object into one body, the historical
    /// behaviour. Off by default: objects export as separate bodies.
    union: bool,
}

impl ExportOptions {
    /// Read `format`, `encoding` (`binary`|`ascii`, STL only) and `union` from
    /// a JSON body, falling back to the query string.
    fn from_request(
        format: &str,
        input: &Value,
        parameters: &HashMap<String, String>,
    ) -> Result<Self, String> {
        if !is_supported_export_format(format) {
            return Err(supported_export_formats_sentence());
        }
        let encoding = match input.get("encoding") {
            Some(Value::String(value)) => Some(value.clone()),
            Some(Value::Null) | None => parameters.get("encoding").cloned(),
            Some(_) => return Err("encoding must be \"binary\" or \"ascii\".".into()),
        };
        let ascii = match encoding.as_deref() {
            None | Some("binary") => false,
            Some("ascii") if format == "stl" => true,
            Some("ascii") => return Err("encoding \"ascii\" applies to stl only.".into()),
            Some(_) => return Err("encoding must be \"binary\" or \"ascii\".".into()),
        };
        let union = match input.get("union") {
            Some(Value::Bool(value)) => *value,
            Some(Value::Null) | None => match parameters.get("union").map(String::as_str) {
                None | Some("false") | Some("0") => false,
                Some("true") | Some("1") => true,
                Some(_) => return Err("union must be true or false.".into()),
            },
            Some(_) => return Err("union must be a boolean.".into()),
        };
        let format = match format {
            "stl" if ascii => export::Format::StlAscii,
            "stl" => export::Format::Stl,
            "off" => export::Format::Off,
            "obj" => export::Format::Obj,
            "3mf" => export::Format::ThreeMf,
            "step" => export::Format::Step,
            "glb" => export::Format::Glb,
            _ => unreachable!("validated export format {format:?}"),
        };
        Ok(Self { format, union })
    }
}

/// Compile `source` and serialise the selected objects: the whole export
/// pipeline behind both HTTP export routes.
fn export_source(
    source: &str,
    object_ids: Option<&[String]>,
    options: ExportOptions,
    title: &str,
) -> Result<(Vec<u8>, CleanupReport), String> {
    let bodies = export_bodies(source, object_ids, options.union, title)?;
    encode_bodies(bodies, options.format, title)
}

/// Clean and serialise bodies in one of [`EXPORT_FORMATS`].
///
/// `title` reaches the formats that carry metadata — 3MF's title, STEP's
/// product name, glTF's root node name — and the bare mesh formats ignore it.
///
/// The size check lives here rather than at each call site because STEP and
/// GLB need it *during* serialisation, not after. STEP runs about ten times
/// the size of the same model's STL, so "build it, then measure it" would let
/// a single request materialise hundreds of megabytes on its way to a
/// rejection — a cheap way to push this server over its memory limit. Both
/// writers take the budget and give up the moment they cross it.
fn encode_bodies(
    mut bodies: Vec<export::ExportBody>,
    format: export::Format,
    title: &str,
) -> Result<(Vec<u8>, CleanupReport), String> {
    // Cleanup and the writers are plain code over a bounded mesh, but a panic
    // in either must still answer as a failed export rather than a connection
    // that closes without a status.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let report = export::clean_bodies(&mut bodies);
        if bodies.is_empty() {
            return Err(EXPORT_NOTHING_SOLID.to_string());
        }
        let data = export::encode(&bodies, format, title, MAX_OUTPUT)
            .ok_or_else(|| EXPORT_TOO_LARGE.to_string())?;
        Ok((data, report))
    }))
    .unwrap_or_else(|_| Err("Internal error: the export writer failed on this model.".to_string()))
}

/// Every selected body was a shell enclosing (next to) no volume, which
/// cleanup drops rather than writing as a solid.
const EXPORT_NOTHING_SOLID: &str = "Nothing to export: every selected object encloses less than \
     0.000000001 mm³ (a 10 µm cube) and was dropped as degenerate. Make the geometry larger or \
     check for zero-thickness parts.";

/// Response headers describing the export-time sliver cleanup, so a client
/// can tell a clean file from one that still has needle or cap triangles
/// (altitude under [`csg::cleanup::SLIVER_ALTITUDE`]) the repair could not
/// remove without moving the surface.
fn cleanup_headers(report: &CleanupReport) -> [(&'static str, String); 2] {
    [
        ("X-ReOpenSCAD-Slivers", report.remaining.to_string()),
        (
            "X-ReOpenSCAD-Sliver-Repairs",
            (report.collapses + report.flips).to_string(),
        ),
    ]
}

/// The same report as JSON, for MCP results.
fn cleanup_value(report: &CleanupReport) -> Value {
    json!({
        "remainingSlivers": report.remaining,
        "collapsedEdges": report.collapses,
        "flippedEdges": report.flips,
        "sliverAltitudeMm": csg::cleanup::SLIVER_ALTITUDE,
        "note": if report.remaining == 0 {
            "No triangle in the file has an altitude under 1 µm."
        } else {
            "Some needle/cap triangles (altitude under 1 µm) could not be removed without moving the surface; the bodies are still closed."
        }
    })
}

/// A finished export file, with its cleanup report in headers.
fn respond_export(
    stream: &mut TcpStream,
    format: &str,
    data: &[u8],
    report: &CleanupReport,
) -> io::Result<()> {
    let disposition = format!("attachment; filename=\"model.{format}\"");
    let cleanup = cleanup_headers(report);
    let mut headers: Vec<(&str, &str)> = vec![("Content-Disposition", &disposition)];
    headers.extend(cleanup.iter().map(|(name, value)| (*name, value.as_str())));
    respond_text(stream, 200, export_content_type(format), data, &headers)
}

fn export_content_type(format: &str) -> &'static str {
    match format {
        "stl" => "model/stl",
        "obj" => "model/obj",
        "3mf" => "model/3mf",
        // Both registered with IANA; `model/step` is the Part 21 text form and
        // `model/gltf-binary` is the `.glb` container.
        "step" => "model/step",
        "glb" => "model/gltf-binary",
        _ => "application/octet-stream",
    }
}

/// The bodies an export writes.
///
/// No object IDs means every top-level object; an ID list selects some of
/// them, in model order. Each selected object is its own body — never unioned
/// with the others — unless `union` asks for the historical single-body
/// export, which goes through [`export_mesh`] unchanged.
fn export_bodies(
    source: &str,
    object_ids: Option<&[String]>,
    union: bool,
    title: &str,
) -> Result<Vec<export::ExportBody>, String> {
    if union {
        let mesh = export_mesh(source, object_ids)?;
        return Ok(vec![export::ExportBody {
            id: "model".into(),
            name: title.into(),
            color: None,
            face_colors: None,
            mesh,
        }]);
    }
    let output = compile_parts_guarded(source, engine::Quality::Render)?;
    let bodies = selected_bodies(&output, object_ids)?;
    // Only what will be written counts: the binary STL size of the selected
    // bodies is the smallest any format makes of them.
    let size = bodies.iter().try_fold(84usize, |total, body| {
        total.checked_add(body.mesh.triangles.len().checked_mul(50)?)
    });
    if size.is_none_or(|size| size > MAX_OUTPUT) {
        return Err(EXPORT_TOO_LARGE.into());
    }
    Ok(bodies)
}

/// The compiled parts named by `object_ids` (all parts when `None`), in model
/// order, as export bodies. Validates the selection the same way
/// [`engine::mesh_selected_parts`] does.
fn selected_bodies(
    output: &engine::MultipartCompileOutput,
    object_ids: Option<&[String]>,
) -> Result<Vec<export::ExportBody>, String> {
    let selected: Vec<&engine::CompiledPart> = match object_ids {
        None => output.parts.iter().collect(),
        Some([]) => return Err("Selected-part export requires at least one object ID.".into()),
        Some(object_ids) => {
            let mut requested = HashSet::with_capacity(object_ids.len());
            for object_id in object_ids {
                if !requested.insert(object_id.as_str()) {
                    return Err(format!("Duplicate selected object ID: {object_id}."));
                }
            }
            let unknown: Vec<&str> = object_ids
                .iter()
                .map(String::as_str)
                .filter(|id| !output.parts.iter().any(|part| part.id == *id))
                .collect();
            if !unknown.is_empty() {
                return Err(format!(
                    "Unknown selected object IDs: {}.",
                    unknown.join(", ")
                ));
            }
            output
                .parts
                .iter()
                .filter(|part| requested.contains(part.id.as_str()))
                .collect()
        }
    };
    if selected.is_empty() {
        return Err("The model contains no 3D geometry.".into());
    }
    Ok(selected
        .into_iter()
        .map(|part| export::ExportBody {
            id: part.id.clone(),
            name: part.name.clone(),
            color: part.color,
            face_colors: part.face_colors.clone(),
            mesh: part.mesh.clone(),
        })
        .collect())
}

/// The historical export: every selected object boolean-unioned into one
/// mesh. No object IDs means the whole model, and an ID filter is only
/// meaningful against parts compiled at export quality. Reached only with
/// `union: true`.
///
/// The selected-object union is a second, unbudgeted boolean after the
/// compile, so it runs under the same render deadline as the compile (one
/// watchdog and cancellation flag across both, plus the kernel's per-thread
/// deadline), and a kernel abort comes back as an error, never a panic that
/// would drop the connection.
fn export_mesh(source: &str, object_ids: Option<&[String]>) -> Result<engine::Mesh, String> {
    let Some(object_ids) = object_ids else {
        return compile_guarded(source, engine::Quality::Render).map(|output| output.mesh.clone());
    };
    let cancellation = Arc::new(AtomicBool::new(false));
    let deadline = RenderDeadline::start(&cancellation, RENDER_DEADLINE);
    let _kernel_deadline = csg::bsp::deadline_scope(Instant::now() + RENDER_DEADLINE);
    let output =
        engine::compile_parts_shared(source, engine::Quality::Render, Some(&cancellation))
            .map_err(|error| deadline.explain(error.to_string()))?;
    selected_parts_mesh(
        &output,
        Some(object_ids),
        engine::Quality::Render,
        Some(&cancellation),
    )
    .map_err(|error| deadline.explain(error))
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
    events: &[WorkspaceEvent],
    objects: Option<Vec<Value>>,
) -> Value {
    // A workspace read must stay cheap, so objects come from the summary
    // cache that every compile of this exact revision fills (a browser
    // render, an MCP update, an analysis call). `?objects=compute` fills it on
    // a miss; otherwise `objectsStatus: "not_compiled"` says why it is empty.
    let (status, objects, hint) = match objects {
        Some(objects) => ("current", objects, None),
        None => {
            let (status, objects, hint) = agent::objects_status(&workspace.id, workspace.revision);
            let hint = match status {
                "current" => None,
                "not_compiled" => Some(OBJECTS_NOT_COMPILED_HINT.to_string()),
                _ => hint,
            };
            (status, objects, hint)
        }
    };
    json!({
        "id": workspace.id,
        "name": workspace.name,
        "code": workspace.code,
        "revision": workspace.revision,
        "revisionName": revision_label(&workspace.revision_name, workspace.revision),
        "createdAt": workspace.created_at,
        "updatedAt": workspace.updated_at,
        "plate": workspace.plate,
        "tolerance": workspace.tolerance,
        "objectsStatus": status,
        "objectsHint": hint,
        "objects": objects,
        "updatedRanges": events,
        "url": workspace_url(state, request, &workspace.id)
    })
}

/// Why a design read has no objects, and how to get them.
const OBJECTS_NOT_COMPILED_HINT: &str = "This revision has not been compiled yet and the server \
     was too busy (or your evaluator rate budget too low) to compile it for a plain read. Retry \
     with ?objects=compute (waits for an evaluator slot), call POST /api/designs/{id}/objects \
     or the MCP list_design_objects tool, or render the design.";

fn respond_workspace(
    stream: &mut TcpStream,
    status: u16,
    state: &AppState,
    request: &Request,
    workspace: &Workspace,
    events: &[WorkspaceEvent],
) -> io::Result<()> {
    respond_value(
        stream,
        status,
        &json!({
            "ok": true,
            "designId": workspace.id,
            "url": workspace_url(state, request, &workspace.id),
            "workspace": workspace_public(workspace, state, request, events, None)
        }),
    )
}

fn workspace_url(state: &AppState, request: &Request, id: &str) -> String {
    format!("{}/designs/{id}", request_base_url(state, request))
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
    let host = request.header("host").unwrap_or("127.0.0.1:5180");
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

/// Answers a request the per-IP rate limiter shed.
///
/// Same principle as the queue rejections: a transient capacity condition must
/// never read like "your input was bad". On `/mcp` that means a JSON-RPC error
/// whose `data` carries the retry contract in fields, since the body has not
/// been parsed yet and the request id is not known — parsing a 2 MiB body just
/// to shed it would defeat the point of shedding. The body names the bucket
/// that refused and the exact wait computed from its refill rate, and
/// `Retry-After` carries the same wait rounded up to whole seconds.
fn respond_rate_limited(stream: &mut TcpStream, path: &str, limited: &RateLimited) -> io::Result<()> {
    let prose = limited.explanation();
    let detail = limited.detail();
    let body = if path == "/mcp" {
        json!({
            "jsonrpc": "2.0",
            "id": Value::Null,
            "error": {
                "code": -32000,
                "message": prose,
                "data": detail,
            }
        })
    } else {
        let mut body = detail;
        body["ok"] = json!(false);
        body["error"] = json!(prose);
        body
    };
    let encoded = serde_json::to_vec(&body).unwrap_or_default();
    let retry = limited.retry_after_seconds().to_string();
    respond_text(
        stream,
        429,
        "application/json; charset=utf-8",
        &encoded,
        &[("Retry-After", retry.as_str())],
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
            let result = json!({
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
            });
            if id.is_null() {
                return respond_text(stream, 202, "application/json", b"", &[]);
            }
            let body = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
            // Issued per the Streamable HTTP transport; it only scopes
            // notifications/cancelled (see `mcp_rpc_scope`).
            let session = mint_mcp_session_id();
            let headers: Vec<(&str, &str)> =
                session.as_deref().map(|session| vec![("Mcp-Session-Id", session)]).unwrap_or_default();
            return respond_text(stream, 200, "application/json; charset=utf-8", body.as_bytes(), &headers);
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
            return respond_mcp_tool_call(stream, state, request, id, name, arguments, &params, false);
        }
        "notifications/initialized" | "notifications/cancelled" => {
            if method == "notifications/cancelled" {
                withdraw_cancelled_mcp_request(request, &params);
            }
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

/// Honours `notifications/cancelled`: the call it names stops waiting, and
/// its job is cancelled if no other caller is attached (see [`jobs`]).
fn withdraw_cancelled_mcp_request(request: &Request, params: &Value) {
    if let Some(request_id) = params.get("requestId").filter(|id| id.is_string() || id.is_number()) {
        if let Some(scope) = mcp_rpc_scope(request, request_id) {
            jobs::registry().withdraw_rpc(&scope);
        }
    }
}

/// Answers a `tools/call`.
///
/// A feedback tool asked to wait answers as a server-sent event stream when
/// the client accepts one: progress notifications (or, without a progress
/// token, SSE comments) every few seconds keep clients and proxies from
/// timing the call out, and a failed write ends the wait early. Everything
/// else is a single JSON reply.
#[allow(clippy::too_many_arguments)]
fn respond_mcp_tool_call(
    stream: &mut TcpStream,
    state: &AppState,
    request: &Request,
    id: Value,
    name: &str,
    arguments: Value,
    params: &Value,
    modern: bool,
) -> io::Result<()> {
    let finish = |result: Value| if modern { modern_mcp_result(result) } else { result };
    struct ClearRequestId;
    impl Drop for ClearRequestId {
        fn drop(&mut self) {
            MCP_REQUEST_ID.with(|slot| *slot.borrow_mut() = None);
        }
    }
    MCP_REQUEST_ID.with(|slot| *slot.borrow_mut() = if id.is_null() { None } else { mcp_rpc_scope(request, &id) });
    let _clear_request_id = ClearRequestId;
    let accepts_stream = request
        .header("accept")
        .is_some_and(|accept| accept.to_ascii_lowercase().contains("text/event-stream"));
    if !(accepts_stream && feedback::tool_may_wait(canonical_mcp_tool_name(name), &arguments)) {
        let result = finish(mcp_call_tool(state, request, name, arguments));
        return respond_value(stream, 200, &json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }
    begin_response();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nX-Accel-Buffering: no\r\nConnection: close\r\n"
    )?;
    if !is_indexable() {
        write!(stream, "X-Robots-Tag: {ROBOTS_TAG}\r\n")?;
    }
    write!(stream, "\r\n")?;
    stream.flush()?;
    let token = params
        .get("_meta")
        .and_then(|meta| meta.get("progressToken"))
        // Echoed in every notification, so a huge token would turn the wait
        // into a slow upload to the client; anything long falls back to
        // anonymous keep-alive comments.
        .filter(|token| token.as_str().is_some_and(|token| token.len() <= 128) || token.is_i64() || token.is_u64())
        .cloned();
    let result = {
        let mut sink = |elapsed: u64, total: u64, message: &str| -> io::Result<()> {
            let chunk = match &token {
                Some(token) => sse_event(&json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {"progressToken": token, "progress": elapsed, "total": total, "message": message}
                })),
                None => ": waiting for feedback\n\n".to_string(),
            };
            note_response_bytes(chunk.len() as u64);
            stream.write_all(chunk.as_bytes())?;
            stream.flush()
        };
        finish(mcp_call_tool_with_progress(state, request, name, arguments, Some(&mut sink)))
    };
    let chunk = sse_event(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
    note_response_bytes(chunk.len() as u64);
    stream.write_all(chunk.as_bytes())?;
    stream.flush()
}

fn sse_event(message: &Value) -> String {
    format!("event: message\ndata: {message}\n\n")
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
    // is simply accepted. A cancellation is still honoured.
    if id.is_null() {
        if method == "notifications/cancelled" {
            withdraw_cancelled_mcp_request(request, params);
        }
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
            return respond_mcp_tool_call(stream, state, request, id, name, arguments, params, true);
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

/// URI of the embedded viewer app, one instance per design.
const DESIGN_APP_URI_PREFIX: &str = "ui://reopenscad/design/";
/// Pre-rename URI of the same app, still served.
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
    vec![
        json!({
            "uriTemplate": format!("{DESIGN_APP_URI_PREFIX}{{designId}}"),
            "name": "design_app",
            "title": "Design preview app",
            "description": "A self-contained HTML app: an interactive 3D preview of the design \
                            plus STL and 3MF download links. Read it with the design ID returned \
                            by create_design or get_design_link.",
            "mimeType": "text/html"
        }),
        json!({
            "uriTemplate": format!("{WORKSPACE_APP_URI_PREFIX}{{workspaceId}}"),
            "name": "workspace_app",
            "title": "Design preview app (deprecated URI)",
            "description": "Deprecated alias of ui://reopenscad/design/{designId}: the same app \
                            under the pre-rename URI.",
            "mimeType": "text/html"
        }),
    ]
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
    let Some(id) = uri
        .strip_prefix(DESIGN_APP_URI_PREFIX)
        .or_else(|| uri.strip_prefix(WORKSPACE_APP_URI_PREFIX))
    else {
        return Err(format!("Unknown resource uri {uri:?}."));
    };
    if !valid_workspace_id(id) {
        return Err("designId is invalid.".into());
    }
    let workspace = state
        .workspaces
        .get(id)?
        .ok_or_else(|| "Design not found.".to_string())?;
    // The embedded viewer app meshes the workspace, so it queues like any
    // other compute. `resources/read` has no tool-result shape to carry the
    // structured rejection, so the prose has to carry it alone.
    let _permit = state
        .render_queue
        .acquire(current_client_key(), None, None)
        .map_err(QueueRejection::message)?;
    let compiled = assetstore::with_design_assets(state, Some(id), || {
        compile_parts_guarded(&workspace.code, engine::Quality::Preview)
    })?;
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
        return Err("The design app exceeds the response limit.".into());
    }
    Ok(json!({
        "uri": uri,
        "name": workspace.name,
        "mimeType": "text/html",
        "text": html
    }))
}

/// The arguments every analysis tool shares: which design. They read its
/// current revision, the one its viewport shows; nothing else can be analysed.
fn mcp_target_properties() -> serde_json::Map<String, Value> {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "designId".into(),
        json!({"type": "string"}),
    );
    properties.insert(
        "workspaceId".into(),
        json!({"type": "string", "deprecated": true}),
    );
    properties
}

/// The `revisionName` argument of every tool that creates a revision.
pub(crate) fn revision_name_schema() -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": MAX_REVISION_NAME_CHARS,
        "description": "What this revision is, e.g. \"Hinge pin 0.2 mm larger\"."
    })
}

/// The `returnCode` argument of every tool that creates a revision.
pub(crate) fn return_code_schema() -> Value {
    json!({"type": "boolean", "default": false, "description": "Also return the full stored source."})
}

/// The `objects` argument of every tool that creates a revision.
pub(crate) fn objects_mode_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["changed", "full"],
        "default": "changed",
        "description": "changed: only objects added or changed since the previous revision (all, if it is not cached); full: all, with every field."
    })
}

/// The `expectedRevision` argument of every update.
pub(crate) fn expected_revision_schema() -> Value {
    json!({"type": "integer", "minimum": 1, "description": "The revision you last read or wrote; a mismatch fails."})
}

/// The `bedZ` argument of the printability checks.
fn bed_z_schema() -> Value {
    json!({"type": "number", "description": "Bed plane for overhang and supports (default: the model's lowest Z)."})
}

/// The items of an `edits` argument.
fn edit_item_schema() -> Value {
    json!({
        "type": "object",
        "required": ["search", "replace"],
        "properties": {
            "search": {"type": "string"},
            "replace": {"type": "string"},
            "occurrence": {"type": "integer", "minimum": 1},
            "replaceAll": {"type": "boolean"}
        }
    })
}

/// The `printProfile` argument of the supports check.
fn print_profile_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "description": "For checks: [\"supports\"]; defaults are a generic FDM profile, not a guarantee.",
        "properties": {
            "layerHeight": {"type": "number", "minimum": 0.01, "maximum": 2, "default": 0.2},
            "lineWidth": {"type": "number", "minimum": 0.05, "maximum": 3, "default": 0.42},
            "maxOverhang": {"type": "number", "minimum": 0, "maximum": 89, "default": 45, "description": "Degrees from vertical."},
            "maxBridge": {"type": "number", "minimum": 0, "maximum": 1000, "default": 10, "description": "Longest unsupported span, mm."}
        }
    })
}

fn mcp_schema(extra: Value, required: &[&str]) -> Value {
    let mut properties = mcp_target_properties();
    if let Value::Object(extra) = extra {
        properties.extend(extra);
    }
    let required: Vec<&str> = std::iter::once("designId").chain(required.iter().copied()).collect();
    json!({"type": "object", "required": required, "properties": properties})
}

fn canonical_mcp_tool_definitions() -> Vec<Value> {
    let mut tools = core_mcp_tool_definitions();
    tools.extend(feedback::tool_definitions());
    tools.extend(assetstore::tool_definitions());
    tools.extend(split::tool_definitions());
    tools.extend(printlayout::tool_definitions());
    tools.extend(scanfeatures::tool_definitions());
    tools.extend(surfacecompare::tool_definitions());
    tools.extend(sectioncompare::tool_definitions());
    tools.extend(curvefit::tool_definitions());
    tools.extend(surfacefit::tool_definitions());
    tools.extend(scanreport::tool_definitions());
    tools.push(jobs_tool_definition());
    tools
}

fn core_mcp_tool_definitions() -> Vec<Value> {
    let ids = json!({"type": "array", "items": {"type": "string"}});
    vec![
        mcp_tool(
            "create_design",
            "Create a single-file SCAD design (revision 1); returns designId, browser URL and compiled \
             objects. Source that fails to compile is rejected. Start the code with the // Requirements: \
             and // Decisions: header recording the user's brief. Give the user the URL right away, so \
             they can watch live, and again at the end of your reply. The URL is an editable capability: \
             share it with the user, never publicly. Work in the design the person gave you; create a new \
             one only when they ask for a new design or none exists, never to try something out.",
            json!({
                "type": "object",
                "required": ["revisionName"],
                "properties": {
                    "revisionName": revision_name_schema(),
                    "code": {"type": "string", "description": "Initial source, starting with the header. Default: a 10 mm cube."},
                    "returnCode": return_code_schema(),
                    "objects": objects_mode_schema(),
                    "name": {"type": "string", "description": "Display name (also used in exports)."}
                }
            }),
        ),
        mcp_tool(
            "get_supported_scad",
            "Return the SCAD language and geometry dictionary this server implements, including arguments \
             accepted without effect or ignored with a warning. Check it before relying on a less common \
             feature.",
            json!({"type": "object", "properties": {}}),
        ),
        mcp_tool(
            "get_reverse_engineering_guide",
            "Return the guide to turning an uploaded STL into a real SCAD part. Read it before \
             reverse-engineering a scan.",
            json!({"type": "object", "properties": {}}),
        ),
        mcp_tool(
            "get_design_code",
            "Read source with few tokens: the current revision, or an older one with revision (text only; \
             restore_revision copies it on top). Forms: startLine/endLine (1-based inclusive; default the \
             whole file); grep (case-insensitive substring, optionally within startLine/endLine) with \
             around context lines; or start/end (UTF-8 byte range). Lines over maxLineLength (0 = never; \
             not applied to a single line or a byte range) become placeholders listed in elidedLines and \
             exact is false; never use a placeholder as an edit search string. Also returns revision \
             (pass it as expectedRevision) and the objects when cached or an evaluator is idle; otherwise \
             objectsStatus is \"not_compiled\" and list_design_objects lists them.",
            json!({
                "type": "object",
                "required": ["designId"],
                "properties": {
                    "designId": {"type": "string"},
                    "revision": {"type": "integer", "minimum": 1, "description": "A retained revision instead of the current one (see list_revisions)."},
                    "start": {"type": "integer", "minimum": 0},
                    "end": {"type": "integer", "minimum": 0},
                    "startLine": {"type": "integer", "minimum": 1},
                    "endLine": {"type": "integer", "minimum": 1},
                    "grep": {"type": "string"},
                    "around": {"type": "integer", "minimum": 0, "maximum": 50, "default": 2},
                    "maxLineLength": {"type": "integer", "minimum": 0, "default": 2000}
                }
            }),
        ),
        mcp_tool(
            "update_design_code",
            "Validate and atomically change source as one new revision. Use exactly one form: edits, \
             {search, replace} applied in order, each search matching exactly once unless occurrence \
             (1-based) or replaceAll is given (on a failing edit nothing is written); \
             startLine/endLine/text (1-based inclusive, endLine defaults to startLine; endLine = \
             startLine - 1 inserts before startLine; text \"\" deletes); start/end/text (UTF-8 byte \
             range); or code (replace everything). Prefer edits or line ranges on large sources. Source \
             that fails to compile is rejected (valid: false) and the previous revision stays. warnings \
             flags a compiled object that is not a valid closed solid. When the user states or changes a \
             requirement or decision, also update the // Requirements: and // Decisions: header in the \
             same call; never drop recorded requirements.",
            json!({
                "type": "object",
                "required": ["designId", "expectedRevision", "revisionName"],
                "properties": {
                    "designId": {"type": "string"},
                    "expectedRevision": expected_revision_schema(),
                    "revisionName": revision_name_schema(),
                    "code": {"type": "string"},
                    "start": {"type": "integer", "minimum": 0},
                    "end": {"type": "integer", "minimum": 0},
                    "startLine": {"type": "integer", "minimum": 1},
                    "endLine": {"type": "integer", "minimum": 0},
                    "text": {"type": "string"},
                    "edits": {"type": "array", "items": edit_item_schema()},
                    "returnCode": return_code_schema(),
                    "objects": objects_mode_schema(),
                    "name": {"type": "string"}
                }
            }),
        ),
        mcp_tool(
            "get_parameters",
            "List top-level parameters (file-scope assignments): name, line, value, trailing // comment \
             (Customizer hints). Values over 200 chars are summarised as \"<list, N items, M chars>\"; \
             verbose: true returns full records (expression, byte offsets, unabridged values).",
            {
                let mut properties = mcp_target_properties();
                properties.insert("names".into(), json!({"type": "array", "items": {"type": "string"}}));
                properties.insert("filter".into(), json!({"type": "string", "description": "Name substring, case-insensitive."}));
                properties.insert("verbose".into(), json!({"type": "boolean", "default": false}));
                json!({"type": "object", "required": ["designId"], "properties": properties})
            },
        ),
        mcp_tool(
            "set_text",
            "Printable lettering without writing SCAD: size (mm) or fit {width, height} (mm, auto-size to \
             the box), font (e.g. \"Great Vibes\", \"Liberation Sans:style=Bold\"). Thickens strokes with \
             offset(r) up to minStroke (mm; default is two 0.42 mm lines) and warns with suggestedSize \
             when gaps under minGap (mm) would close. height is the extrusion (mm); color is the material \
             that becomes a separate 3MF part; direction/language e.g. \"rtl\", \"he\". Written as a new \
             revision in the block between // reopenscad:text id=<id> markers, replacing that block if it \
             exists (so later calls change it in place) or appended.",
            json!({
                "type": "object",
                "required": ["designId", "expectedRevision", "revisionName", "text"],
                "properties": {
                    "designId": {"type": "string"},
                    "expectedRevision": expected_revision_schema(),
                    "revisionName": revision_name_schema(),
                    "id": {"type": "string", "description": "Block id (default text-1); letters, digits, - and _."},
                    "text": {"type": "string"},
                    "font": {"type": "string"},
                    "size": {"type": "number", "minimum": 0.5},
                    "fit": {"type": "object", "properties": {"width": {"type": "number"}, "height": {"type": "number"}}},
                    "halign": {"type": "string", "enum": ["left", "center", "right"]},
                    "valign": {"type": "string", "enum": ["top", "center", "baseline", "bottom"]},
                    "direction": {"type": "string"},
                    "language": {"type": "string"},
                    "height": {"type": "number", "minimum": 0.01, "default": 1},
                    "minStroke": {"type": "number", "minimum": 0, "default": 0.84},
                    "minGap": {"type": "number", "minimum": 0, "default": 0.4},
                    "translate": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
                    "rotate": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
                    "color": {"type": "string"},
                    "name": {"type": "string", "description": "Label on the block's marker line."},
                    "spacing": {"type": "number", "minimum": 0.5, "maximum": 3, "description": "text() letter spacing (1 = the font's); kept as given."},
                    "autoSpacing": {"type": "boolean", "description": "Widen spacing (up to 1.2x) when letters would fuse through a thin bridge. Default: on unless spacing is given."},
                    "objects": objects_mode_schema(),
                    "returnCode": return_code_schema()
                }
            }),
        ),
        mcp_tool(
            "list_modules",
            "Outline of the source without reading it: every module and function with parameters, line \
             range and comment, plus banner comments (// ---- Hinge ----). grep keeps definitions whose \
             name or comment contains it (case-insensitive). Nothing is compiled.",
            {
                let mut properties = mcp_target_properties();
                properties.insert("grep".into(), json!({"type": "string"}));
                json!({"type": "object", "required": ["designId"], "properties": properties})
            },
        ),
        mcp_tool(
            "set_parameters",
            "Set existing top-level parameters, e.g. {\"open_angle\": 35, \"part\": \"inner\"}, as one \
             validated, named revision. Values must be literals. Every top-level assignment of a name is \
             rewritten; unknown names are rejected.",
            json!({
                "type": "object",
                "required": ["designId", "expectedRevision", "revisionName", "values"],
                "properties": {
                    "designId": {"type": "string"},
                    "expectedRevision": expected_revision_schema(),
                    "revisionName": revision_name_schema(),
                    "returnCode": return_code_schema(),
                    "objects": objects_mode_schema(),
                    "values": {"type": "object", "additionalProperties": true}
                }
            }),
        ),
        mcp_tool(
            "render_design",
            "Export the current revision as one file; returns its data and a download URL. All objects \
             (or objectIds) go in as SEPARATE bodies in world coordinates unless union=true. 3mf: one \
             named, coloured part per object, so slicers can assign a filament per part. glb is in \
             metres; step is an AP214 B-rep. Each keeps per-object names and colours where the format \
             can. Sliver triangles (under 1 µm) are removed; cleanup.remainingSlivers counts the rest.",
            json!({
                "type": "object",
                "required": ["designId"],
                "properties": {
                    "designId": {"type": "string"},
                    "format": {"type": "string", "enum": EXPORT_FORMATS, "default": "stl"},
                    "includeData": {"type": "boolean", "default": true},
                    "objectIds": {
                        "type": "array",
                        "items": {"type": "string"}
                    },
                    "union": {
                        "type": "boolean",
                        "default": false
                    },
                    "encoding": {
                        "type": "string",
                        "enum": ["binary", "ascii"],
                        "default": "binary",
                        "description": "STL only."
                    }
                }
            }),
        ),
        mcp_tool(
            "render_image",
            "Render a PNG of the current revision plus metadata (camera, mm per pixel, objects). Camera: \
             view (auto-framed) or eye/target/up; viewHeightMm is the orthographic scale. objectIds shows \
             only those objects; colors overrides object colours ({\"object-1\": \"#ff0000\"}), otherwise \
             color() is used. section cuts the model: {\"axis\": \"y\", \"offset\": 0} or {\"point\": \
             [x,y,z], \"normal\": [x,y,z]} (the half the normal points into is removed; the cut face is \
             hatched). Default 800x600. views (up to 6) renders ONE tiled, labelled contact sheet instead \
             (default 400x320 per tile; not with eye).",
            mcp_schema(
                json!({
                    "objectIds": ids,
                    "hiddenObjectIds": ids,
                    "colors": {"type": "object", "additionalProperties": true},
                    "view": {"type": "string", "enum": ["default", "iso", "front", "back", "left", "right", "top", "bottom"]},
                    "views": {"type": "array", "maxItems": 6, "items": {"type": "string", "enum": ["default", "iso", "front", "back", "left", "right", "top", "bottom"]}},
                    "eye": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
                    "target": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
                    "up": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
                    "projection": {"type": "string", "enum": ["perspective", "orthographic"]},
                    "fovDeg": {"type": "number"},
                    "zoom": {"type": "number"},
                    "viewHeightMm": {"type": "number"},
                    "section": {"type": "object"},
                    "width": {"type": "integer", "minimum": 64, "maximum": 2048, "description": "Of the model area; the label band and legend are added outside it, so the PNG can be larger (render.width, render.height, render.model_area). views: the whole sheet."},
                    "height": {"type": "integer", "minimum": 64, "maximum": 2048, "description": "See width."},
                    "background": {"type": "string"},
                    "edges": {"type": "boolean"},
                    "axes": {"type": "boolean"},
                    "grid": {"type": "boolean"},
                    "label": {"type": "boolean"},
                    "showBackground": {"type": "boolean", "description": "Also draw %-modifier objects (grey)."}
                }),
                &[],
            ),
        ),
        mcp_tool(
            "list_design_objects",
            "List the addressable top-level objects: the IDs accepted by objectIds/visibleObjectIds \
             everywhere, plus name, triangle count, exact bounds, colour and source lines.",
            mcp_schema(json!({}), &[]),
        ),
        mcp_tool(
            "measure",
            "Exact measurements of each object's mesh (volume in mm^3), with watertight/manifold status; \
             defects locates open or non-manifold edges. pairs (max 32; objects, groups or components) \
             adds the exact minimum distance with closest points, or the overlap volume and region. \
             groups measures a union as one (IDs group-1... or the names; overlaps counted once); \
             splitComponents lists each separate solid as object-N#1, ... largest first. checks (opt-in, \
             per object, +Z up): thickness: minimum wall from dense surface samples, as the largest \
             inside ball's diameter, with the area below thicknessThreshold (mm; sharp wedges such as \
             thread crests are counted apart in sharpTipSamples). overhang: downward surface tilted over \
             overhangAngle degrees from vertical (faces on the bed plane count as bed, so a floating \
             object has its underside counted). sharpEdges: convex edges with interior dihedral angle at \
             or below sharpEdgeAngle (a 45-degree chamfer gives 135). supports: slices bottom-up by \
             printProfile (assumed: true when defaulted) into islands (start in midair, even when they \
             join higher up: joinsAt), cantilevers (one anchor, beyond maxOverhang) and bridges (two or \
             more anchors; span vs maxBridge).",
            mcp_schema(
                json!({
                    "objectIds": ids,
                    "hiddenObjectIds": ids,
                    "pairs": {"type": "array", "items": {"type": "array", "items": {"type": "string"}, "minItems": 2, "maxItems": 2}},
                    "groups": {"description": "[[\"object-1\", \"object-2\"], ...] (IDs group-1, ...) or {\"name\": [ids]}.", "anyOf": [{"type": "array", "items": ids}, {"type": "object", "additionalProperties": ids}]},
                    "splitComponents": {"type": "boolean", "default": false},
                    "checks": {"type": "array", "items": {"type": "string", "enum": ["thickness", "overhang", "sharpEdges", "supports"]}},
                    "printProfile": print_profile_schema(),
                    "thicknessThreshold": {"type": "number", "minimum": 0, "default": 0.8},
                    "thicknessAt": {"type": "object", "required": ["point"], "properties": {"point": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3}, "radius": {"type": "number", "exclusiveMinimum": 0, "maximum": 100, "default": 2}}, "description": "Dense wall thickness of the surface within radius (mm) of point (a suspect lip, a socket mouth)."},
                    "overhangAngle": {"type": "number", "minimum": 0, "maximum": 90, "default": 45},
                    "bedZ": bed_z_schema(),
                    "sharpEdgeAngle": {"type": "number", "minimum": 0, "maximum": 180, "default": 90}
                }),
                &[],
            ),
        ),
        mcp_tool(
            "evaluate",
            "The default check, and the way to save and check in one call. With code or edits plus \
             revisionName and expectedRevision it first saves a new revision (source that fails to \
             compile is rejected with valid: false and nothing is saved). Then it compiles the current \
             revision once and returns objects (as measure, with pairs, groups, splitComponents, checks), \
             intersections (as check_intersections, over objectIds), optional image and diagnostics. echo \
             returns computed values; asserts each assert's margin and operands (all asserts run). \
             verdict.printability is obstacles, incomplete, not-checked (no checks: [\"supports\"]) or \
             clear-under-profile; printable is true only for clear-under-profile. complete is false when \
             any pair was skipped or undetermined: never read that as clear. Compact unless verbose: \
             healthy objects omit mesh stats; clean pairs are {a, b, relation, clearance}.",
            mcp_schema(
                json!({
                    "code": {"type": "string", "description": "Saved as a new revision first. Needs revisionName and expectedRevision. Not with edits."},
                    "edits": {"type": "array", "description": "As in update_design_code, saved as a new revision first. Needs revisionName and expectedRevision. Not with code.", "items": edit_item_schema()},
                    "revisionName": revision_name_schema(),
                    "expectedRevision": expected_revision_schema(),
                    "objectIds": ids,
                    "hiddenObjectIds": ids,
                    "pairs": {"type": "array", "items": {"type": "array", "items": {"type": "string"}, "minItems": 2, "maxItems": 2}},
                    "groups": {"description": "As in measure.", "anyOf": [{"type": "array", "items": ids}, {"type": "object", "additionalProperties": ids}]},
                    "splitComponents": {"type": "boolean", "default": false},
                    "checks": {"type": "array", "items": {"type": "string", "enum": ["thickness", "overhang", "sharpEdges", "supports"]}},
                    "printProfile": print_profile_schema(),
                    "thicknessThreshold": {"type": "number", "minimum": 0, "default": 0.8},
                    "thicknessAt": {"type": "object", "required": ["point"], "properties": {"point": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3}, "radius": {"type": "number", "exclusiveMinimum": 0, "maximum": 100, "default": 2}}, "description": "As in measure."},
                    "echo": {"type": "array", "items": {"type": "string"}, "maxItems": 50, "description": "Top-level variables or simple expressions of them, e.g. \"socket_r - pin_r\"."},
                    "asserts": {"description": "true: margin (how far from failing, in its units) and lhs/rhs of failing asserts and the 5 tightest passing ones; \"all\": every assert.", "anyOf": [{"type": "boolean"}, {"type": "string", "enum": ["all"]}]},
                    "bedZ": bed_z_schema(),
                    "tolerance": {"type": "number", "minimum": 0},
                    "allowContact": {"anyOf": [{"type": "boolean"}, {"type": "array", "items": {"type": "array", "items": {"type": "string"}, "minItems": 2, "maxItems": 2}}]},
                    "intersections": {"description": "true (default), false, or options {tolerance, allowContact, visibleObjectIds, hiddenObjectIds, groups, splitComponents}.", "anyOf": [{"type": "boolean"}, {"type": "object"}]},
                    "verbose": {"type": "boolean", "default": false, "description": "Full records: every pair field and per-object mesh statistics."},
                    "image": {"description": "false (default), true, or render_image options.", "anyOf": [{"type": "boolean"}, {"type": "object"}]}
                }),
                &[],
            ),
        ),
        mcp_tool(
            "diff",
            "Compare two stored revisions geometrically (default: toRevision current, fromRevision the \
             newest retained one before it). Per object (matched by ID): status, volume change and exact \
             added and removed volume (a conservative box on very large meshes); also which source lines \
             changed. checks: [\"supports\"] re-runs the support check on both and adds \
             printabilityChanges {resolved, introduced, remaining}.",
            mcp_schema(
                json!({
                    "fromRevision": {"type": "integer", "minimum": 1},
                    "toRevision": {"type": "integer", "minimum": 1},
                    "checks": {"type": "array", "items": {"type": "string", "enum": ["supports"]}},
                    "printProfile": print_profile_schema(),
                    "bedZ": {"type": "number", "description": "Bed plane for the supports re-check (default: each version's lowest Z)."}
                }),
                &[],
            ),
        ),
        mcp_tool(
            "list_revisions",
            "Revision history, newest first. Up to the last 200 are kept (fewer on a large source: 4 MiB \
             of history per design). limit and before page through it. Read old source with \
             get_design_code revision, compare with diff, go back with restore_revision.",
            json!({
                "type": "object",
                "required": ["designId"],
                "properties": {
                    "designId": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_REVISION_HISTORY + 1, "default": 50},
                    "before": {"type": "integer", "minimum": 1, "description": "Only revisions older than this."}
                }
            }),
        ),
        mcp_tool(
            "restore_revision",
            "Copy an old revision's source into a NEW revision on top, validated like any write (a source \
             that no longer compiles, e.g. after an asset was deleted, is rejected with valid: false). \
             History is never rewritten.",
            json!({
                "type": "object",
                "required": ["designId", "revision", "revisionName", "expectedRevision"],
                "properties": {
                    "designId": {"type": "string"},
                    "revision": {"type": "integer", "minimum": 1},
                    "revisionName": revision_name_schema(),
                    "expectedRevision": expected_revision_schema(),
                    "returnCode": return_code_schema(),
                    "objects": objects_mode_schema()
                }
            }),
        ),
        mcp_tool(
            "get_design_link",
            "Return the browser capability URL of a design, plus the ui:// URI of the preview app that \
             resources/read serves.",
            json!({
                "type": "object",
                "required": ["designId"],
                "properties": {"designId": {"type": "string"}}
            }),
        ),
        mcp_tool(
            "create_share_link",
            "Mint (or return the existing) read-only link: viewers can see, download and read the source \
             but not edit, and it does not reveal the editable URL.",
            json!({
                "type": "object",
                "required": ["designId"],
                "properties": {"designId": {"type": "string"}}
            }),
        ),
        mcp_tool(
            "check_intersections",
            "Check every pair of visible objects for intersection and clearance on the exact meshes. \
             tolerance (mm; default the design's) is the required minimum gap. relation: overlap \
             (positive shared volume), contact (surfaces touch, zero volume), clearanceViolation (0 < gap \
             < tolerance), clear (gap >= tolerance) or undetermined. Contact passes at tolerance 0; above \
             it, it violates unless allowContact marks it intended seating. toleranceViolated = overlap, \
             clearanceViolation, undetermined or disallowed contact. Overlap depth is the thickness of \
             the exact overlap solid at its thickest point (a press fit reads its radial interference), \
             else the smaller of vertexPenetration and overlapBoundsMinExtent; depthMethod says which. \
             determined=false: not decided within the work budget (true clearance in \
             [clearanceLowerBound, clearance]); treat it as a problem. Compact unless verbose: clean \
             pairs are {a, b, relation, clearance}. Problem pairs carry clearanceRegions: up to 8 patches \
             where signed clearance (negative = inside the other part) is below tolerance, from surface \
             sampling, so they work beyond the 60k-triangle limit of the exact overlap volume.",
            mcp_schema(
                json!({
                    "tolerance": {"type": "number", "minimum": 0},
                    "allowContact": {"description": "true: contact is intended for every pair; or a list of [idA, idB] pairs where it is.", "anyOf": [{"type": "boolean"}, {"type": "array", "items": {"type": "array", "items": {"type": "string"}, "minItems": 2, "maxItems": 2}}]},
                    "groups": {"description": "Check a union as one: [[ids...], ...] (IDs group-1, ...) or {\"name\": [ids]}. An object may be in one group.", "anyOf": [{"type": "array", "items": ids}, {"type": "object", "additionalProperties": ids}]},
                    "splitComponents": {"type": "boolean", "default": false, "description": "Each separate solid of an object is its own target (object-N#1, ...)."},
                    "visibleObjectIds": ids,
                    "hiddenObjectIds": ids,
                    "verbose": {"type": "boolean", "default": false, "description": "Full records: every pair field and per-object mesh statistics."}
                }),
                &[],
            ),
        ),
        mcp_tool(
            "check_insertion",
            "Can object `moving` follow a translation path without touching the fixed objects, and with \
             what clearance? path: waypoint offsets [[x,y,z], ...] from moving's modelled position, a \
             polyline (start at [0,0,0] to begin where modelled; L-shaped: \
             [[0,0,0],[0,-30,0],[0,-30,-20]]), or direction + distance. Continuous, not sampled: no \
             contact can be missed. passes: no contact on the whole path. clearanceOk: clearance >= \
             tolerance throughout (default the design's). collision.penetration.depth is a lower bound. \
             complete: false with undetermined when the budget ran out; passes/clearanceOk are then \
             omitted unless already decided. Touching at the start counts as a collision: start with a \
             small gap to slide along a surface.",
            mcp_schema(
                json!({
                    "moving": {"type": "string"},
                    "fixed": {"description": "Default: all other visible objects.", "anyOf": [{"type": "string"}, ids.clone()]},
                    "path": {"type": "array", "minItems": 1, "maxItems": 64, "items": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3}},
                    "direction": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3, "description": "With distance, instead of path."},
                    "distance": {"type": "number", "minimum": 0, "description": "mm."},
                    "tolerance": {"type": "number", "minimum": 0},
                    "precision": {"type": "number", "minimum": 0.001, "default": 0.01, "description": "mm of travel, for firstViolation."},
                    "visibleObjectIds": ids,
                    "hiddenObjectIds": ids
                }),
                &["moving"],
            ),
        ),
        mcp_tool(
            "describe_geometry",
            "What each object is and how it is posed; for an imported mesh, save import(\"scan.stl\"); as \
             a revision first. Per object: size, units (suggestedScale to mm), principalAxes, layFlat \
             candidates, symmetry, and fitted planes, cylinders (hole = bore rather than boss) and \
             spheres. alignMatrix and layFlat matrix are row-major 4x4: `multmatrix(m) \
             import(\"scan.stl\");` aligns the oriented box to X/Y/Z at the origin, or lays that face on \
             z = 0. within {min, max} describes only the faces touching that box.",
            mcp_schema(
                json!({
                    "objectIds": ids,
                    "hiddenObjectIds": ids,
                    "within": scancommon::within_schema("design coordinates, the same as the objects' bounds"),
                    "features": {"type": "boolean", "default": true, "description": "Fit planes, cylinders, spheres (slower)."}
                }),
                &[],
            ),
        ),
        mcp_tool(
            "probe",
            "Exact point queries (nearest across the selected objects), up to 256 rays and 256 points per \
             call. rays [{origin, direction}] return the first hit; points [[x,y,z]] the closest surface \
             point with signed distance (negative inside). allHits: true returns every crossing of each \
             ray, nearest first, and spans between hits of kind material (a wall thickness), gap (air: a \
             recess or clearance) or ambiguous (open surfaces).",
            mcp_schema(
                json!({
                    "objectIds": ids,
                    "hiddenObjectIds": ids,
                    "rays": {"type": "array", "maxItems": 256, "items": {"type": "object", "required": ["origin", "direction"], "properties": {
                        "origin": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
                        "direction": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3}
                    }}},
                    "points": {"type": "array", "maxItems": 256, "items": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3}},
                    "allHits": {"type": "boolean", "default": false},
                    "maxDistance": {"type": "number", "exclusiveMinimum": 0, "description": "allHits only, mm."}
                }),
                &[],
            ),
        ),
        mcp_tool(
            "section_profile",
            "Cut the selected objects with a plane and return 2D loops: plane {axis, offset (mm)} or \
             {point, normal}. Loop points are in the plane basis (model point = origin + x*u + y*v; \
             plane.placeMatrix is that multmatrix), outer loops counter-clockwise, holes with negative \
             area. tolerance (mm) simplifies loops, doubling until they fit maxPoints. scad: true adds \
             polygon() code. within {min, max} keeps the contours to that box, cutting them at its walls: \
             a cut loop becomes open chains with no area (area counts whole loops only).",
            mcp_schema(
                json!({
                    "objectIds": ids,
                    "hiddenObjectIds": ids,
                    "plane": {"type": "object", "properties": {
                        "axis": {"type": "string", "enum": ["x", "y", "z"]},
                        "offset": {"type": "number"},
                        "point": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
                        "normal": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3}
                    }},
                    "tolerance": {"type": "number", "minimum": 0, "default": 0.05},
                    "maxPoints": {"type": "integer", "minimum": 3, "maximum": 20000, "default": 2000},
                    "within": scancommon::within_schema("design coordinates, the same as the plane's point and offset (not the loops' plane coordinates)"),
                    "scad": {"type": "boolean", "default": false}
                }),
                &["plane"],
            ),
        ),
        mcp_tool(
            "fit_axis",
            "Fit the true centerline of elongated objects (handles, tubes, tapered shafts, scanned limbs) \
             from the area centroids of perpendicular slices. Prefer it over describe_geometry \
             principalAxes, which tapers and bulges pull off-centre. alignMatrix moves the axis onto +Z \
             at the origin. initial seeds the direction (default: longest principal axis); trim skips \
             that fraction at each end.",
            mcp_schema(
                json!({
                    "objectIds": ids,
                    "hiddenObjectIds": ids,
                    "initial": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
                    "sections": {"type": "integer", "minimum": 3, "maximum": 200, "default": 24},
                    "trim": {"type": "number", "minimum": 0, "maximum": 0.45, "default": 0.05},
                    "range": {"type": "array", "items": {"type": "number"}, "minItems": 2, "maxItems": 2, "description": "[t0, t1] mm along the axis from the body centre."}
                }),
                &[],
            ),
        ),
        mcp_tool(
            "align_to",
            "Rotation math, no design needed: takes pairs[0].from exactly onto pairs[0].to and, if given, \
             pairs[1].from as close as possible to pairs[1].to (residualDeg is what is left). Example, \
             axis d and face normal n: [{from: d, to: [0,0,1]}, {from: n, to: [1,0,0]}]. Returns matrix \
             (row-major 4x4 for multmatrix), rotate ([x, y, z] degrees for OpenSCAD rotate()) and scad \
             snippets. Pass matrix to place to centre and ground the result.",
            json!({
                "type": "object",
                "required": ["pairs"],
                "properties": {
                    "pairs": {"type": "array", "minItems": 1, "maxItems": 2, "items": {"type": "object", "required": ["from", "to"], "properties": {
                        "from": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
                        "to": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3}
                    }}},
                    "pivot": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3, "description": "Rotate about this point. Default origin."},
                    "then": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3}
                }
            }),
        ),
        mcp_tool(
            "place",
            "Compute the placement that centres and grounds objects (default: all visible; they move \
             together): applies matrix first, then the translate centring the exact bounds on X/Y \
             (\"xy\"), X/Y/Z (\"xyz\") or not at all (\"none\") at on, and drops the lowest point to z = \
             0 (ground wins over z centring). Write the result into the code yourself.",
            mcp_schema(
                json!({
                    "objectIds": ids,
                    "hiddenObjectIds": ids,
                    "matrix": {"type": "array", "minItems": 3, "maxItems": 4, "items": {"type": "array", "items": {"type": "number"}, "minItems": 4, "maxItems": 4}, "description": "Row-major affine, applied before centring."},
                    "center": {"type": "string", "enum": ["xy", "xyz", "none"], "default": "xy"},
                    "ground": {"type": "boolean", "default": true},
                    "on": {"type": "array", "items": {"type": "number"}, "minItems": 2, "maxItems": 3, "description": "Target centre (default [0, 0])."}
                }),
                &[],
            ),
        ),
    ]
}

/// Tool names from before workspaces were renamed to designs, and the
/// canonical tool each one now calls. Kept so agents configured against the
/// old names keep working.
const MCP_TOOL_ALIASES: &[(&str, &str)] = &[
    ("create_workspace", "create_design"),
    ("get_workspace_code", "get_design_code"),
    ("update_workspace_code", "update_design_code"),
    ("render_workspace", "render_design"),
    ("list_workspace_objects", "list_design_objects"),
    ("get_workspace_link", "get_design_link"),
];

/// The canonical tool a (possibly deprecated) tool name resolves to.
fn canonical_mcp_tool_name(name: &str) -> &str {
    MCP_TOOL_ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map_or(name, |(_, canonical)| canonical)
}

/// Every tool: the canonical ones first, then each deprecated alias, listed
/// (rather than hidden) so a client that validates calls against tools/list
/// still accepts the old name. An alias takes `workspaceId` where its
/// canonical tool takes `designId`; the server accepts either on every tool.
fn mcp_tool_definitions() -> Vec<Value> {
    let canonical = canonical_mcp_tool_definitions();
    let mut tools = canonical.clone();
    for (alias, target) in MCP_TOOL_ALIASES {
        let Some(tool) = canonical.iter().find(|tool| tool["name"] == *target) else {
            continue;
        };
        let mut schema = tool["inputSchema"].clone();
        strip_descriptions(&mut schema);
        if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
            for entry in required.iter_mut() {
                if entry == "designId" {
                    *entry = json!("workspaceId");
                }
            }
        }
        if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
            if properties.contains_key("designId") {
                properties.entry("workspaceId").or_insert_with(|| json!({"type": "string"}));
            }
        }
        tools.push(mcp_tool(alias, &format!("Deprecated alias of {target}; takes workspaceId for designId."), schema));
    }
    tools
}

/// Removes the text from a schema: an alias's accepted values stay, and its
/// canonical tool documents them. A property that is itself called
/// `description` holds a schema, not a string, so it stays.
fn strip_descriptions(schema: &mut Value) {
    match schema {
        Value::Object(fields) => {
            if fields.get("description").is_some_and(Value::is_string) {
                fields.remove("description");
            }
            fields.values_mut().for_each(strip_descriptions);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_descriptions),
        _ => {}
    }
}

/// Maps the pre-rename `workspaceId` argument and the canonical `designId`
/// onto the one key the tool bodies read. Both may be given only when they
/// agree.
fn normalize_design_arguments(mut arguments: Value) -> Result<Value, String> {
    let Some(object) = arguments.as_object_mut() else {
        return Ok(arguments);
    };
    // An explicit `designId: null` is the same as leaving it out.
    if let Some(design) = object.remove("designId").filter(|design| !design.is_null()) {
        match object.get("workspaceId").filter(|workspace| !workspace.is_null()) {
            Some(workspace) if workspace != &design => {
                return Err("designId and workspaceId name different designs; give only designId.".into())
            }
            _ => {
                object.insert("workspaceId".into(), design);
            }
        }
    }
    Ok(arguments)
}

/// `designId` (canonical) or `workspaceId` (pre-rename alias) of a request body.
pub(crate) fn input_design_id(input: &Value) -> Option<&str> {
    input
        .get("designId")
        .or_else(|| input.get("workspaceId"))
        .and_then(Value::as_str)
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
        canonical_mcp_tool_name(name),
        "create_design"
            | "update_design_code"
            | "restore_revision"
            | "set_parameters"
            | "render_design"
            | "render_image"
            | "list_design_objects"
            | "check_intersections"
            | "check_insertion"
            | "evaluate"
            | "measure"
            | "diff"
            | "set_text"
            | "describe_geometry"
            | "probe"
            | "section_profile"
            | "fit_axis"
            | "place"
            | "split_object"
            | "set_print_orientation"
            | "get_print_layout"
            | "export_print_parts"
            | "extract_features"
            | "compare_surfaces"
            | "measure_between"
            | "compare_sections"
            | "fit_curve"
            | "fit_surface"
            | "scan_report"
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
    mcp_call_tool_with_progress(state, request, name, arguments, None)
}

/// [`mcp_call_tool`], with a sink for progress while a feedback tool waits.
/// Every other tool's reply notes open feedback comments on its design.
fn mcp_call_tool_with_progress(
    state: &AppState,
    request: &Request,
    name: &str,
    arguments: Value,
    progress: Option<feedback::ProgressSink<'_>>,
) -> Value {
    // Every call that names a design is shown to whoever has it open: a
    // running line now, then what it found (see `activity`).
    let canonical = canonical_mcp_tool_name(name);
    let shown = normalize_design_arguments(arguments.clone()).ok();
    let ticket = shown
        .as_ref()
        .and_then(|arguments| input_design_id(arguments).map(|id| (id.to_string(), arguments)))
        .and_then(|(id, arguments)| activity::begin(state, &id, canonical, arguments));
    let (result, offered) = activity::capture(|| mcp_call_tool_recorded(state, request, name, arguments, progress));
    if let Some(shown) = &shown {
        match ticket {
            Some(ticket) => activity::finish(ticket, shown, &result, offered),
            None if canonical == "create_design" && result["isError"] == json!(false) => {
                if let Some(id) = result["structuredContent"]["designId"].as_str() {
                    activity::record_finished(state, id, canonical, shown, &result);
                }
            }
            None => {}
        }
    }
    result
}

fn mcp_call_tool_recorded(
    state: &AppState,
    request: &Request,
    name: &str,
    arguments: Value,
    progress: Option<feedback::ProgressSink<'_>>,
) -> Value {
    let canonical = canonical_mcp_tool_name(name);
    if feedback::is_feedback_tool(canonical) {
        let arguments = match normalize_design_arguments(arguments) {
            Ok(arguments) => arguments,
            Err(error) => return mcp_tool_error(&error),
        };
        return match feedback::call_tool(state, request, canonical, &arguments, progress) {
            Ok(structured) => json!({
                "content": [{"type": "text", "text": serde_json::to_string(&structured).unwrap_or_default()}],
                "structuredContent": structured,
                "isError": false
            }),
            Err(error) => mcp_tool_error(&error),
        };
    }
    let hint_arguments = normalize_design_arguments(arguments.clone()).ok();
    // Every tool that compiles a design's code (stored or unsaved, given a
    // designId) sees the design's assets.
    let assets_design = hint_arguments
        .as_ref()
        .filter(|_| !assetstore::is_asset_tool(canonical))
        .and_then(|arguments| input_design_id(arguments).map(str::to_string));
    let mut result = assetstore::with_design_assets(state, assets_design.as_deref(), || {
        mcp_call_tool_inner(state, request, name, arguments)
    });
    if result["isError"] != json!(true) && mcp_tool_renders(canonical) {
        if let Some(note) = hint_arguments
            .as_ref()
            .and_then(|arguments| input_design_id(arguments))
            .and_then(|id| assetstore::live_stl_note(state, id))
        {
            if let Some(text) = note["text"].as_str() {
                let line = json!({"type": "text", "text": text});
                if let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) {
                    content.push(line);
                }
            }
            if let Some(structured) = result.get_mut("structuredContent").and_then(Value::as_object_mut) {
                structured.insert("liveStl".into(), note);
            }
        }
    }
    if result["isError"] != json!(true) {
        if let Some((open, hint)) = hint_arguments
            .as_ref()
            .and_then(|arguments| feedback::open_feedback_hint(state, arguments))
        {
            if let Some(structured) = result.get_mut("structuredContent").and_then(Value::as_object_mut) {
                structured.insert("openFeedback".into(), json!(open));
            }
            if let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) {
                content.push(json!({"type": "text", "text": hint}));
            }
        }
    }
    result
}

/// The tools that render or export a design, aliases canonicalised. Their
/// replies carry the note about a live STL import.
fn mcp_tool_renders(canonical: &str) -> bool {
    matches!(canonical, "evaluate" | "render_design" | "render_image" | "export_print_parts" | "get_print_layout")
}

fn mcp_tool_error(error: &str) -> Value {
    json!({
        "content": [{"type": "text", "text": error}],
        "structuredContent": {"ok": false, "error": error},
        "isError": true
    })
}

fn mcp_call_tool_inner(state: &AppState, request: &Request, name: &str, arguments: Value) -> Value {
    let name = canonical_mcp_tool_name(name);
    let arguments = match normalize_design_arguments(arguments) {
        Ok(arguments) => arguments,
        Err(error) => {
            return json!({
                "content": [{"type": "text", "text": error}],
                "structuredContent": {"ok": false, "error": error},
                "isError": true
            })
        }
    };
    if let Err(error) = mcp_check_arguments(name, &arguments) {
        return mcp_tool_error(&error);
    }
    if assetstore::is_asset_tool(name) {
        return assetstore::mcp_dispatch(state, request, name, &arguments);
    }
    if name == "jobs" {
        return mcp_jobs(&arguments);
    }
    if !mcp_tool_runs_evaluator(name) {
        return mcp_dispatch_tool(state, request, name, arguments);
    }
    let Some(spec) = mcp_job_spec(state, request, name, &arguments) else {
        let _permit = match state.render_queue.acquire(current_client_key(), None, None) {
            Ok(permit) => permit,
            Err(rejection) => return mcp_queue_rejection(rejection),
        };
        return mcp_dispatch_tool(state, request, name, arguments);
    };
    // Identical calls share one job; the job holds the permit (see `jobs`).
    let caller = jobs::Caller {
        gone: current_client_gone(),
        rpc: MCP_REQUEST_ID.with(|slot| slot.borrow().clone()),
    };
    let write = mcp_call_writes(name, &arguments);
    let design = spec.design.clone();
    let (mut result, served) = jobs::registry().run(
        spec,
        caller,
        |job| {
            let _permit = match state.render_queue.acquire(current_client_key(), None, Some(job.cancel_flag())) {
                Ok(permit) => permit,
                Err(rejection) => return mcp_queue_rejection(rejection),
            };
            job.set_running();
            mcp_dispatch_tool(state, request, name, arguments)
        },
        // A write that landed must replay (idempotent retry) even when its
        // object list was skipped; the list's status says so already.
        // So must an evaluate whose save landed and whose evaluation then failed.
        |result: &Value| {
            if write {
                result["isError"] == json!(false) || result["structuredContent"]["saved"].is_object()
            } else {
                mcp_result_cacheable(result)
            }
        },
        mcp_tool_error,
    );
    if served == jobs::Served::Cached {
        let current = design
            .filter(|_| write)
            .and_then(|id| state.workspaces.get(&id).ok().flatten())
            .map(|workspace| workspace.revision);
        mark_replayed(&mut result, current);
    }
    result
}

/// Arguments that used to run tools on geometry other than the current
/// revision. Any tool whose schema no longer lists one refuses it, rather than
/// silently answering about something else than the agent asked.
const REMOVED_ARGUMENTS: [&str; 7] = ["code", "overrides", "variants", "sweep", "revision", "fromCode", "toCode"];

/// Where each tool's accepted arguments and required names are read from, so
/// the checks and `tools/list` cannot disagree.
fn mcp_tool_schema(name: &str) -> Option<&'static Value> {
    static SCHEMAS: OnceLock<HashMap<String, Value>> = OnceLock::new();
    SCHEMAS
        .get_or_init(|| {
            canonical_mcp_tool_definitions()
                .into_iter()
                .filter_map(|tool| Some((tool["name"].as_str()?.to_string(), tool["inputSchema"].clone())))
                .collect()
        })
        .get(name)
}

/// Checks a call against what is gone and what is required, before any queueing
/// or compiling: a removed argument, a missing design, an invalid revision name.
fn mcp_check_arguments(name: &str, arguments: &Value) -> Result<(), String> {
    let Some(schema) = mcp_tool_schema(name) else {
        return Ok(());
    };
    let given = |key: &str| arguments.get(key).is_some_and(|value| !value.is_null());
    let accepted = |key: &str| schema["properties"].get(key).is_some();
    for key in REMOVED_ARGUMENTS {
        if given(key) && !accepted(key) {
            return Err(removed_argument_error(name, key));
        }
    }
    if name == "set_text" && given("dryRun") {
        return Err("set_text no longer takes dryRun: lettering is written as a named revision, which the person sees. To change it, call set_text again with the same id; restore_revision goes back.".into());
    }
    let required = |key: &str| schema["required"].as_array().is_some_and(|names| names.iter().any(|entry| entry == key));
    if required("designId") && input_design_id(arguments).is_none() {
        return Err(format!(
            "designId is required: {name} works on a saved design's current revision. Create one with create_design."
        ));
    }
    if name == "evaluate" {
        match (given("code"), given("edits")) {
            (true, true) => return Err("Give code or edits, not both.".into()),
            (false, false) => {
                if given("revisionName") || given("expectedRevision") {
                    return Err("revisionName and expectedRevision go with code or edits, which evaluate saves as a new revision first. Without them it evaluates the current revision and takes neither.".into());
                }
            }
            _ => {
                if !given("expectedRevision") {
                    return Err("expectedRevision is required with code or edits: the revision you last read or wrote.".into());
                }
                return arguments.get("revisionName").map_or_else(
                    || Err(missing_revision_name()),
                    |value| value.as_str().map_or_else(|| Err(missing_revision_name()), |raw| validate_revision_name(raw).map(|_| ())),
                );
            }
        }
    }
    if required("revisionName") {
        let Some(raw) = arguments.get("revisionName").and_then(Value::as_str) else {
            return Err(missing_revision_name());
        };
        validate_revision_name(raw)?;
    }
    Ok(())
}

fn missing_revision_name() -> String {
    format!(
        "revisionName is required: name the revision this creates, in at most {MAX_REVISION_NAME_CHARS} characters (e.g. \"Hinge pin 0.2 mm larger\"). Nothing was saved."
    )
}

fn removed_argument_error(tool: &str, argument: &str) -> String {
    let how = match argument {
        "revision" => "To look at an old revision, copy it on top with restore_revision (get_design_code revision reads its source as text, diff compares two revisions)",
        "fromCode" | "toCode" => "diff compares stored revisions: save each side as a named revision, then pass fromRevision and toRevision",
        _ => "Save what you want to check as a named revision first (update_design_code, set_parameters, or evaluate with code and revisionName), or go back to an old one with restore_revision",
    };
    format!(
        "{tool} no longer takes {argument}: it reads the design's current revision, the one the person sees in the viewport. {how}, then call {tool} with designId."
    )
}

/// Whether a call stores a new revision (its job key carries expectedRevision).
fn mcp_call_writes(name: &str, arguments: &Value) -> bool {
    match name {
        "update_design_code" | "restore_revision" | "set_parameters" | "set_text" | "set_print_orientation" => true,
        "evaluate" => agent::evaluate_saves(arguments),
        _ => false,
    }
}

/// Marks a reply served from the result cache: `replayed: true` always, and
/// for a write replayed after another write landed (`current` is the
/// design's revision now) `currentRevision` with the object list flagged
/// stale, since it describes the replayed revision, not the design now.
fn mark_replayed(result: &mut Value, current: Option<u64>) {
    let Some(structured) = result.get_mut("structuredContent").and_then(Value::as_object_mut) else {
        return;
    };
    structured.insert("replayed".into(), json!(true));
    let replayed = structured.get("revision").and_then(Value::as_u64);
    let (Some(current), Some(replayed)) = (current, replayed) else { return };
    if current == replayed {
        return;
    }
    structured.insert("currentRevision".into(), json!(current));
    if structured.contains_key("objectsStatus") {
        structured.insert("objectsStatus".into(), json!("superseded"));
    }
    let note = format!(
        "Replayed the reply of an identical earlier write (revision {replayed}); the design has since moved on to \
         revision {current}, so this reply (objects included) describes revision {replayed}, not the design now. \
         Read the design again before the next edit."
    );
    structured.insert("replayNote".into(), json!(note.clone()));
    if let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) {
        content.push(json!({"type": "text", "text": note}));
    }
}

/// The registry key of an evaluator-running call: everything its reply
/// depends on. `None` runs the call unshared: `create_design` (each call is
/// meant to make a new design) and a design that cannot be read now.
fn mcp_job_spec(state: &AppState, request: &Request, name: &str, arguments: &Value) -> Option<jobs::JobSpec> {
    if name == "create_design" {
        return None;
    }
    let design = input_design_id(arguments).map(str::to_string);
    // A write's key carries its expectedRevision, which only one write can
    // match, so replaying it returns the original reply (idempotent retry).
    // A read depends on the design's current revision as well.
    let write = mcp_call_writes(name, arguments);
    // The source itself, not just the revision number, so a key can never
    // match another store's design that happens to share the id. Settings
    // change without a revision bump, and replies read them (the default
    // tolerance above all), so they are part of the key too. A write's key
    // leaves them out: a write may itself change them, and its replay is
    // marked instead (see `mark_replayed`).
    let revision = match (&design, write) {
        (Some(id), false) => {
            let workspace = state.workspaces.get(id).ok()??;
            serde_json::to_string(&json!([
                workspace.revision,
                workspace.code,
                workspace.name,
                workspace.plate,
                workspace.tolerance.to_bits(),
                workspace.share_token,
            ]))
            .ok()?
        }
        _ => String::new(),
    };
    let arguments_bytes = serde_json::to_vec(arguments).ok()?;
    // Replies carry URLs built from these.
    let host = [request.header("host"), request.header("x-forwarded-host"), request.header("x-forwarded-proto")]
        .map(|value| value.unwrap_or_default())
        .join("|");
    let digest = crate::assets::current_digest();
    Some(jobs::JobSpec {
        key: jobs::key_of(&[
            rendercache::ENGINE_VERSION.as_bytes(),
            name.as_bytes(),
            &arguments_bytes,
            revision.as_bytes(),
            digest.as_bytes(),
            host.as_bytes(),
        ]),
        tool: name.to_string(),
        design,
    })
}

/// Whether a finished reply may be served to a later identical call: only a
/// successful *and complete* one. A refusal (queue full), a cancellation or a
/// time-limit stop may well go differently next time, and a compile error is
/// cheap to reproduce. Partial successes are found structurally: see
/// [`mcp_result_partial`].
fn mcp_result_cacheable(result: &Value) -> bool {
    result["isError"] == json!(false) && !mcp_result_partial(result)
}

/// Whether a reply carries any marker of a partial or retryable outcome,
/// anywhere in it: `complete: false`, a skipped/rate-limited/timed-out sweep
/// variant or pair, `incomplete` sections, `truncated: true` (deadline-cut
/// sampling), `retryable: true`, an `objectsStatus` other than `current`.
fn mcp_result_partial(result: &Value) -> bool {
    let non_empty = |value: &Value| match value {
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
        Value::Null => false,
    };
    let mut stack = vec![result];
    while let Some(value) = stack.pop() {
        match value {
            Value::Array(items) => stack.extend(items),
            Value::Object(fields) => {
                for (key, value) in fields {
                    let partial = match key.as_str() {
                        "complete" => value == &json!(false),
                        "skipped" | "incomplete" | "timedOut" | "partial" | "deadlineExceeded" => non_empty(value),
                        "truncated" | "retryable" => value == &json!(true),
                        "objectsStatus" => value.as_str() != Some("current"),
                        "printability" => value.as_str() == Some("incomplete"),
                        "relation" => value.as_str() == Some("skipped"),
                        "reason" => matches!(
                            value.as_str(),
                            Some("rate_limited" | "timeout" | "timed_out" | "deadline" | "cancelled" | "busy" | "queue_full")
                        ),
                        _ => false,
                    };
                    if partial {
                        return true;
                    }
                    stack.push(value);
                }
            }
            _ => {}
        }
    }
    false
}

/// The `jobs` tool: lists a design's jobs, or cancels one by id.
fn mcp_jobs(arguments: &Value) -> Value {
    let id = match argument_workspace_id(arguments) {
        Ok(id) => id,
        Err(error) => return mcp_tool_error(&error),
    };
    let registry = jobs::registry();
    let mut structured = json!({"ok": true, "designId": id});
    if let Some(job) = arguments.get("cancel").and_then(Value::as_str) {
        match registry.cancel(id, job) {
            Ok(summary) => structured["cancelled"] = summary,
            Err(error) => return mcp_tool_error(&error),
        }
    }
    structured["jobs"] = json!(registry.list(id));
    json!({
        "content": [{"type": "text", "text": serde_json::to_string(&structured).unwrap_or_default()}],
        "structuredContent": structured,
        "isError": false
    })
}

fn jobs_tool_definition() -> Value {
    mcp_tool(
        "jobs",
        "List a design's evaluator jobs (queued, running, finished in the last 3 minutes) and cancel one \
         with cancel. An identical call (same tool, arguments, revision) attaches to the running job, and \
         a finished reply is reused for 3 minutes (replayed: true), so after a client timeout retry the \
         same call unchanged. A job whose callers all disconnected stops after 30 s. \
         notifications/cancelled with the call's request id (sent with the Mcp-Session-Id initialize \
         returned) also withdraws it.",
        json!({
            "type": "object",
            "required": ["designId"],
            "properties": {
                "designId": {"type": "string"},
                "cancel": {"type": "string", "description": "Id of a queued or running job."}
            }
        }),
    )
}

/// Runs one MCP tool body and wraps its outcome as a tool result.
fn mcp_dispatch_tool(state: &AppState, request: &Request, name: &str, arguments: Value) -> Value {
    let result = match name {
        "create_design" => mcp_create_workspace(state, request, &arguments),
        "get_supported_scad" => Ok(supported_scad_dictionary()),
        "get_reverse_engineering_guide" => Ok(reverse_guide::value()),
        "get_design_code" => mcp_get_workspace_code(state, &arguments),
        "update_design_code" => mcp_update_workspace(state, request, arguments),
        "render_design" => mcp_render_workspace(state, request, &arguments),
        "list_design_objects" => {
            agent::mcp_result(agent::objects_value(state, None, &arguments))
        }
        "get_design_link" => mcp_workspace_link(state, request, &arguments),
        "check_intersections" => {
            agent::mcp_result(agent::intersections_value(state, None, &arguments)).map(|mut value| {
                agent::slim_analysis(&mut value, &arguments);
                value
            })
        }
        "measure" => agent::mcp_result(agent::measure_value(state, None, &arguments)),
        "evaluate" => return agent::mcp_evaluate(state, &arguments),
        "list_revisions" => agent::mcp_result(agent::mcp_list_revisions(state, &arguments)),
        "restore_revision" => mcp_restore_revision(state, request, &arguments),
        "diff" => agent::mcp_result(agent::diff_value(state, None, &arguments)),
        "render_image" => return agent::mcp_render_image(state, &arguments),
        "describe_geometry" => agent::mcp_result(agent::describe_geometry_value(state, None, &arguments)),
        "probe" => agent::mcp_result(agent::probe_value(state, None, &arguments)),
        "check_insertion" => agent::mcp_result(agent::insertion_value(state, None, &arguments)),
        "section_profile" => agent::mcp_result(agent::section_profile_value(state, None, &arguments)),
        "fit_axis" => agent::mcp_result(agent::fit_axis_value(state, None, &arguments)),
        "align_to" => agent::mcp_result(agent::align_to_value(&arguments)),
        "place" => agent::mcp_result(agent::place_value(state, None, &arguments)),
        "split_object" => return split::mcp_dispatch(state, name, &arguments),
        "extract_features" => return scanfeatures::mcp_dispatch(state, &arguments),
        "compare_surfaces" | "measure_between" => return surfacecompare::mcp_dispatch(state, name, &arguments),
        "compare_sections" => return sectioncompare::mcp_dispatch(state, name, &arguments),
        "fit_curve" => return curvefit::mcp_dispatch(state, name, &arguments),
        "fit_surface" => return surfacefit::mcp_dispatch(state, name, &arguments),
        "scan_report" => return scanreport::mcp_dispatch(state, name, &arguments),
        "set_print_orientation" | "get_print_layout" | "export_print_parts" => {
            printlayout::mcp_dispatch(state, request, name, &arguments)
        }
        "list_modules" => agent::mcp_result(agent::modules_value(state, &arguments)),
        "set_text" => mcp_set_text(state, request, &arguments),
        "get_parameters" => agent::mcp_result(agent::parameters_value(state, None, &arguments))
            .map(|value| agent::compact_parameters(value, &arguments)),
        "set_parameters" => agent::mcp_set_parameters(state, request, &arguments),
        "create_share_link" => agent::mcp_create_share_link(state, request, &arguments),
        _ => Err(format!("Unknown tool {name:?}.")),
    };
    match result {
        Ok(structured) => json!({
            "content": [{"type": "text", "text": serde_json::to_string(&structured).unwrap_or_default()}],
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

/// The design an MCP call names, and its stored record.
fn argument_design<'a>(state: &AppState, arguments: &'a Value) -> Result<(&'a str, Workspace), String> {
    let id = argument_workspace_id(arguments)?;
    let workspace = state.workspaces.get(id)?.ok_or_else(|| "Design not found.".to_string())?;
    Ok((id, workspace))
}

fn argument_workspace_id(arguments: &Value) -> Result<&str, String> {
    let id = arguments
        .get("workspaceId")
        .and_then(Value::as_str)
        .ok_or_else(|| "designId is required.".to_string())?;
    if !valid_workspace_id(id) {
        return Err("designId is invalid.".into());
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
    let revision_name = arguments
        .get("revisionName")
        .and_then(Value::as_str)
        .ok_or_else(missing_revision_name)
        .and_then(validate_revision_name)?;
    let workspace = state.workspaces.create_marked(code, name, RevisionMark::agent(revision_name))?;
    let objects = workspace_object_summaries(&workspace);
    Ok(mcp_design_reply(state, request, &workspace, objects, None, arguments))
}

/// The agent-facing reply to an MCP write (create, update, set_parameters).
///
/// Deliberately slimmer than `workspace_public` (the REST/browser shape): the
/// agent just sent or patched the source, so echoing a 70k-char design back
/// costs tokens for nothing. `returnCode: true` opts back into the code.
fn mcp_design_reply(
    state: &AppState,
    request: &Request,
    workspace: &Workspace,
    objects: ObjectSummaries,
    previous: Option<Vec<Value>>,
    arguments: &Value,
) -> Value {
    let mut reply = json!({
        "ok": true,
        "valid": true,
        "designId": workspace.id,
        "name": workspace.name,
        "revision": workspace.revision,
        "revisionName": revision_label(&workspace.revision_name, workspace.revision),
        "url": workspace_url(state, request, &workspace.id),
        "totalLength": workspace.code.len(),
        "diagnostics": []
    });
    let objects = match objects {
        Ok(objects) => objects,
        // Not computed: no count, no list and no diff, since an empty list
        // would read as "every object removed".
        Err((status, hint)) => {
            reply["objectsStatus"] = json!(status);
            reply["objectsHint"] = json!(hint);
            if arguments.get("returnCode").and_then(Value::as_bool) == Some(true) {
                reply["code"] = json!(workspace.code);
            }
            return reply;
        }
    };
    let invalid: Vec<String> = objects
        .iter()
        .filter(|object| object["meshOk"] == json!(false))
        .filter_map(|object| object["id"].as_str().map(str::to_string))
        .collect();
    let object_count = objects.len();
    let (shown, objects) = mcp_reply_objects(objects, previous, arguments);
    reply["objectsStatus"] = json!("current");
    reply["objectCount"] = json!(object_count);
    reply["objectsShown"] = json!(shown);
    merge_json(&mut reply, objects);
    if arguments.get("returnCode").and_then(Value::as_bool) == Some(true) {
        reply["code"] = json!(workspace.code);
    }
    if !invalid.is_empty() {
        reply["warnings"] = json!([format!(
            "WARNING: {} compiled but {} not a valid closed solid; call evaluate for the edge counts before exporting.",
            invalid.join(", "),
            if invalid.len() == 1 { "is" } else { "are" }
        )]);
    }
    reply
}

fn merge_json(target: &mut Value, extra: Value) {
    if let (Some(target), Value::Object(extra)) = (target.as_object_mut(), extra) {
        target.extend(extra);
    }
}

/// The object part of a write reply: `("full", {objects})` when the caller
/// passed `objects: "full"` (every summary field); `("changed", {objects,
/// objectsUnchanged, objectsRemoved?})` when the previous revision's
/// summaries are cached, listing only added objects and objects whose
/// triangles or bounds changed; otherwise `("all", {objects})` with id,
/// name, triangles and meshOk per object.
/// Whether an object summary is unchanged from the previous revision's: the
/// geometry (triangles, bounds) and the identity behind its id (name,
/// colour, visibility, and the length of its source span, since a reorder of
/// two look-alike calls hands an id to other code). The span's offset is left
/// out: an edit above an object moves it without changing the object.
fn same_object(old: &Value, new: &Value) -> bool {
    let span = |object: &Value| {
        let range = &object["sourceRange"];
        range["end"].as_u64().zip(range["start"].as_u64()).map(|(end, start)| end.saturating_sub(start))
    };
    ["triangles", "bounds", "name", "kind", "color", "colors", "visible", "selectable", "background"]
        .iter()
        .all(|key| old.get(*key) == new.get(*key))
        && span(old) == span(new)
}

fn mcp_reply_objects(objects: Vec<Value>, previous: Option<Vec<Value>>, arguments: &Value) -> (&'static str, Value) {
    if arguments.get("objects").and_then(Value::as_str) == Some("full") {
        return ("full", json!({"objects": objects}));
    }
    let compact = |object: &Value, bounds: bool| {
        let mut entry = json!({
            "id": object["id"],
            "name": object["name"],
            "triangles": object["triangles"],
            "meshOk": object["meshOk"],
        });
        if bounds {
            entry["bounds"] = json!({"min": object["bounds"]["min"], "max": object["bounds"]["max"]});
        }
        if object["visible"] == json!(false) {
            entry["visible"] = json!(false);
        }
        entry
    };
    let Some(previous) = previous else {
        return ("all", json!({"objects": objects.iter().map(|object| compact(object, false)).collect::<Vec<_>>()}));
    };
    let before: HashMap<&str, &Value> = previous
        .iter()
        .filter_map(|object| Some((object["id"].as_str()?, object)))
        .collect();
    let mut unchanged = 0usize;
    let mut changed = Vec::new();
    for object in &objects {
        match object["id"].as_str().and_then(|id| before.get(id)) {
            Some(old) if same_object(old, object) => unchanged += 1,
            Some(_) => changed.push(compact(object, true)),
            None => {
                let mut entry = compact(object, true);
                entry["added"] = json!(true);
                changed.push(entry);
            }
        }
    }
    let current: HashSet<&str> = objects.iter().filter_map(|object| object["id"].as_str()).collect();
    let removed: Vec<&str> = before.keys().copied().filter(|id| !current.contains(id)).collect();
    let mut value = json!({"objects": changed, "objectsUnchanged": unchanged});
    if !removed.is_empty() {
        let mut removed = removed;
        removed.sort_unstable();
        value["objectsRemoved"] = json!(removed);
    }
    ("changed", value)
}

fn mcp_get_workspace_code(state: &AppState, arguments: &Value) -> Result<Value, String> {
    let (_, workspace) = argument_design(state, arguments)?;
    if let Some(revision) = arguments.get("revision").filter(|value| !value.is_null()) {
        let revision = revision.as_u64().ok_or("revision must be a positive integer.")?;
        return mcp_old_revision_code(state, workspace, revision, arguments);
    }
    let mut value = mcp_code_view(&workspace, arguments)?;
    // Objects ride along when they are cheap to have: cached, or compiled
    // now on an idle evaluator within the caller's rate budget. Otherwise
    // the status and hint say how to get them (list_workspace_objects).
    compile_objects_if_idle(state, &workspace);
    let (status, objects, hint) = agent::objects_status(&workspace.id, workspace.revision);
    value["objectsStatus"] = json!(status);
    value["objects"] = json!(objects);
    value["objectsHint"] = json!(hint.unwrap_or_else(|| {
        "Not compiled yet and the server was busy; call list_design_objects to compile and list them.".into()
    }));
    if status == "current" {
        value["objectsHint"] = Value::Null;
    }
    Ok(value)
}

/// `get_design_code` of a named revision: the same views over that source,
/// without the object list (which only exists for the current revision).
fn mcp_old_revision_code(state: &AppState, mut workspace: Workspace, revision: u64, arguments: &Value) -> Result<Value, String> {
    let current = workspace.revision;
    workspace.code = agent::revision_source(state, &workspace, revision).map_err(|error| error.message)?;
    workspace.revision = revision;
    let mut value = mcp_code_view(&workspace, arguments)?;
    value["currentRevision"] = json!(current);
    if revision != current {
        value["note"] = json!(format!(
            "Revision {revision} is not current: this is its source as text. To work on it, restore_revision copies it on top; expectedRevision is still {current}."
        ));
    }
    Ok(value)
}

/// Lines longer than this are elided from a `get_design_code` read unless the
/// caller asks for exact bytes or a single line (data lines such as font
/// outlines run to tens of thousands of chars).
const DEFAULT_MAX_LINE_LENGTH: usize = 2000;

/// `get_design_code`: a byte range (exact), a line range, or grep windows,
/// with over-long lines elided to a placeholder that says how to read them.
fn mcp_code_view(workspace: &Workspace, arguments: &Value) -> Result<Value, String> {
    let code = &workspace.code;
    let number = |key: &str| arguments.get(key).and_then(Value::as_u64).map(|n| n as usize);
    let byte_range = number("start").is_some() || number("end").is_some();
    let grep = arguments
        .get("grep")
        .and_then(Value::as_str)
        .filter(|pattern| !pattern.is_empty());
    if byte_range && (grep.is_some() || number("startLine").is_some() || number("endLine").is_some()) {
        return Err("Give either start/end (bytes) or startLine/endLine/grep, not both.".into());
    }
    if byte_range {
        let start = number("start").unwrap_or(0);
        let end = number("end").unwrap_or(code.len());
        // Byte reads stay exact unless the caller asks for elision.
        let mut value = workspace_code_slice(workspace, start, end)?;
        if let Some(limit) = number("maxLineLength").filter(|limit| *limit > 0) {
            let first_line = scadsource::line_of(code, start);
            let (text, elided) = elide_long_lines(&code[start..end], first_line, limit);
            value["code"] = json!(text);
            if !elided.is_empty() {
                value["elidedLines"] = json!(elided);
                value["exact"] = json!(false);
            }
        }
        return Ok(value);
    }
    let lines: Vec<&str> = code.split('\n').collect();
    let total_lines = lines.len();
    let first = number("startLine").unwrap_or(1).max(1);
    let last = number("endLine").unwrap_or(total_lines).min(total_lines);
    if first > last {
        return Err(format!("Line range is empty: the source has {total_lines} lines."));
    }
    let single_line = first == last;
    let limit = match number("maxLineLength") {
        Some(0) => usize::MAX,
        Some(limit) => limit,
        None if single_line => usize::MAX,
        None => DEFAULT_MAX_LINE_LENGTH,
    };
    let mut value = json!({
        "ok": true,
        "designId": workspace.id,
        "revision": workspace.revision,
        "totalLength": code.len(),
        "totalLines": total_lines,
    });
    let mut elided = Vec::new();
    if let Some(pattern) = grep {
        let around = number("around").unwrap_or(2).min(50);
        let needle = pattern.to_lowercase();
        let hits: Vec<usize> = (first..=last)
            .filter(|line| lines[line - 1].to_lowercase().contains(&needle))
            .collect();
        // Merge overlapping windows so each line is printed once.
        let mut windows: Vec<(usize, usize)> = Vec::new();
        for hit in &hits {
            let (from, to) = (hit.saturating_sub(around).max(first), (hit + around).min(last));
            match windows.last_mut() {
                Some(window) if from <= window.1 + 1 => window.1 = window.1.max(to),
                _ => windows.push((from, to)),
            }
        }
        const MAX_GREP_WINDOWS: usize = 40;
        let truncated = windows.len() > MAX_GREP_WINDOWS;
        windows.truncate(MAX_GREP_WINDOWS);
        let matches: Vec<Value> = windows
            .iter()
            .map(|&(from, to)| {
                let (text, mut window_elided) =
                    elide_long_lines(&lines[from - 1..to].join("\n"), from, limit);
                elided.append(&mut window_elided);
                json!({"startLine": from, "endLine": to, "code": text})
            })
            .collect();
        value["grep"] = json!(pattern);
        value["matchLines"] = json!(hits);
        value["matches"] = json!(matches);
        if truncated {
            value["truncated"] = json!(format!("Only the first {MAX_GREP_WINDOWS} windows are shown; narrow grep or startLine/endLine."));
        }
    } else {
        let (text, line_elided) = elide_long_lines(&lines[first - 1..last].join("\n"), first, limit);
        elided = line_elided;
        value["startLine"] = json!(first);
        value["endLine"] = json!(last);
        value["code"] = json!(text);
    }
    if !elided.is_empty() {
        value["exact"] = json!(false);
        value["elidedLines"] = json!(elided);
        value["elidedNote"] = json!(
            "Lines longer than maxLineLength are replaced by a placeholder (not the source); read one with startLine = endLine = its line, and do not use a placeholder as an edit search string."
        );
    }
    Ok(value)
}

/// `text` with every line over `limit` chars replaced by a placeholder; the
/// elided lines as `{line, length}` (`first_line` numbers the first line).
fn elide_long_lines(text: &str, first_line: usize, limit: usize) -> (String, Vec<Value>) {
    let mut elided = Vec::new();
    let out: Vec<String> = text
        .split('\n')
        .enumerate()
        .map(|(index, line)| {
            let length = line.chars().count();
            if length <= limit {
                return line.to_string();
            }
            let number = first_line + index;
            elided.push(json!({"line": number, "length": length}));
            let head: String = line.chars().take(80).collect();
            format!("{head}… <elided: line {number} is {length} chars>")
        })
        .collect();
    (out.join("\n"), elided)
}

/// `set_text`: lays out printable lettering ([`textfit::layout`]) and, with
/// a designId, writes it as the marked block `id` (replacing that block, or
/// appending it) in one validated revision.
fn mcp_set_text(state: &AppState, request: &Request, arguments: &Value) -> Result<Value, String> {
    let mut spec = textfit::TextSpec::from_input(arguments)?;
    let layout = textfit::layout(&mut spec)?;
    let id = argument_workspace_id(arguments)?.to_string();
    let expected = arguments
        .get("expectedRevision")
        .and_then(Value::as_u64)
        .ok_or("expectedRevision is required to write (the revision you read).")?;
    let workspace = state
        .workspaces
        .get(&id)?
        .ok_or_else(|| "Design not found.".to_string())?;
    let block = layout["scad"].as_str().unwrap_or_default().to_string();
    let (start, end, text) = match textfit::find_block(&workspace.code, &spec.id)? {
        Some((start, end)) => (start, end, block),
        None => {
            let separator = if workspace.code.is_empty() || workspace.code.ends_with('\n') { "\n" } else { "\n\n" };
            (workspace.code.len(), workspace.code.len(), format!("{separator}{block}"))
        }
    };
    let mut reply = mcp_apply_update(
        state,
        request,
        &id,
        json!({
            "expectedRevision": expected,
            "start": start,
            "end": end,
            "text": text,
            "source": "ai",
            "revisionName": arguments.get("revisionName").cloned().unwrap_or(Value::Null),
            "returnCode": arguments.get("returnCode").cloned().unwrap_or(Value::Null),
            "objects": arguments.get("objects").cloned().unwrap_or(Value::Null),
        }),
    )?;
    reply["text"] = layout;
    Ok(reply)
}

fn mcp_update_workspace(
    state: &AppState,
    request: &Request,
    arguments: Value,
) -> Result<Value, String> {
    // Permit held by `mcp_call_tool`.
    let id = argument_workspace_id(&arguments)?.to_string();
    let mut input = arguments;
    let object = input
        .as_object_mut()
        .ok_or_else(|| "Tool arguments must be an object.".to_string())?;
    object.remove("workspaceId");
    object.insert("source".into(), Value::String("ai".into()));
    mcp_apply_update(state, request, &id, input)
}

/// `restore_revision`: the old revision's source, as a new revision on top.
fn mcp_restore_revision(state: &AppState, request: &Request, arguments: &Value) -> Result<Value, String> {
    let id = argument_workspace_id(arguments)?.to_string();
    let options = json!({"returnCode": arguments.get("returnCode"), "objects": arguments.get("objects")});
    match agent::restore_revision(state, &id, arguments, "ai") {
        Ok((revision, result)) => {
            let mut reply = mcp_update_reply(state, request, &id, &result, &options);
            reply["restoredFrom"] = json!(revision);
            Ok(reply)
        }
        Err(agent::RestoreFailure::Source(error)) => Err(error.message),
        Err(agent::RestoreFailure::Update(error)) => mcp_update_failure(error),
    }
}

/// One validated workspace write from MCP: resolves the patch forms
/// (`startLine`/`endLine`/`text`, `edits`, `values`), stores, and answers
/// with the new revision and its objects. Shared by `update_workspace_code`
/// and `set_parameters`.
fn mcp_apply_update(
    state: &AppState,
    request: &Request,
    id: &str,
    mut input: Value,
) -> Result<Value, String> {
    let options = json!({"returnCode": input.get("returnCode").cloned(), "objects": input.get("objects").cloned()});
    if let Some(object) = input.as_object_mut() {
        object.remove("returnCode");
        object.remove("objects");
    }
    match mcp_store_update(state, id, input) {
        Ok(result) => Ok(mcp_update_reply(state, request, id, &result, &options)),
        Err(error) => mcp_update_failure(error),
    }
}

/// The reply to a write that landed: the new revision, its objects, what changed.
fn mcp_update_reply(state: &AppState, request: &Request, id: &str, result: &EditResult, options: &Value) -> Value {
    // The previous revision's summaries, read before this compile
    // replaces them in the cache, so the reply lists only what changed.
    let previous = match agent::object_cache(id, result.workspace.revision.saturating_sub(1)) {
        Some(agent::CachedObjects::Listed(objects)) => Some(objects),
        _ => None,
    };
    let objects = workspace_object_summaries(&result.workspace);
    let span = result.span();
    let edit_lines = |start: usize, inserted: usize| {
        json!([
            scadsource::line_of(&result.workspace.code, start),
            scadsource::line_of(
                &result.workspace.code,
                (start + inserted).saturating_sub(1).max(start)
            )
        ])
    };
    let mut reply = mcp_design_reply(state, request, &result.workspace, objects, previous, options);
    reply["edit"] = json!({
            "start": span.0,
            "end": span.1,
            "insertedLength": span.2,
            "lines": edit_lines(span.0, span.2),
            // Each changed region separately (a full-code update is
            // diffed against the previous source line by line).
            "ranges": result.events.iter().map(|event| json!({
                "start": event.start,
                "removedLength": event.end - event.start,
                "insertedLength": event.inserted_length,
                "lines": edit_lines(event.start, event.inserted_length),
                "source": event.source,
            })).collect::<Vec<_>>(),
    });
    reply
}

/// Resolves an MCP write's patch forms and stores it as one revision.
pub(crate) fn mcp_store_update(state: &AppState, id: &str, mut input: Value) -> Result<EditResult, UpdateError> {
    agent::resolve_patch(state, id, &mut input)?;
    let update = serde_json::from_value::<WorkspaceUpdate>(input)
        .map_err(|error| UpdateError::Invalid(format!("Invalid update: {error}")))?;
    state.workspaces.update(id, update)
}

/// A write that did not land, as an MCP tool answers it: source that does not
/// compile is a reply an agent reads, everything else a tool error.
pub(crate) fn mcp_update_failure(error: UpdateError) -> Result<Value, String> {
    match error {
        UpdateError::Validation(error) => Ok(json!({
            "ok": false,
            "valid": false,
            "error": error,
            "diagnostics": [{"severity": "error", "message": error}]
        })),
        UpdateError::Conflict(workspace) => Err(format!(
            "Revision conflict: current revision is {}.",
            workspace.revision
        )),
        UpdateError::NotFound => Err("Design not found.".into()),
        UpdateError::Invalid(error) | UpdateError::Internal(error) => Err(error),
    }
}

fn mcp_render_workspace(
    state: &AppState,
    request: &Request,
    arguments: &Value,
) -> Result<Value, String> {
    let (id, workspace) = argument_design(state, arguments)?;
    let revision = workspace.revision;
    let format = arguments
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("stl");
    let options = ExportOptions::from_request(format, arguments, &HashMap::new())?;
    let object_ids = requested_object_ids(arguments, &HashMap::new())?;
    // Permit held by `mcp_call_tool`.
    let bodies = export_bodies(
        &workspace.code,
        object_ids.as_deref(),
        options.union,
        &workspace.name,
    )?;
    let bodies_public: Vec<Value> = bodies
        .iter()
        .map(|body| {
            json!({
                "id": body.id,
                "name": body.name,
                "color": body.color,
            })
        })
        .collect();
    let (data, cleanup) = encode_bodies(bodies, options.format, &workspace.name)?;
    let mut export_url =
        workspace_export_url(state, request, id, format, revision, object_ids.as_deref());
    if options.union {
        export_url.push_str("&union=true");
    }
    if options.format == export::Format::StlAscii {
        export_url.push_str("&encoding=ascii");
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
        "designId": id,
        "workspaceId": id,
        "revision": workspace.revision,
        "format": format,
        "bytes": data.len(),
        "bodies": bodies_public,
        "cleanup": cleanup_value(&cleanup),
        "dataBase64": include_data.then(|| base64_encode(&data)),
        "exportUrl": export_url,
        "designUrl": workspace_url(state, request, id),
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
        "{}/api/designs/{id}/export?format={format}&revision={revision}{selection_query}",
        request_base_url(state, request)
    )
}

/// How long a write spends meshing for the object list in its reply.
///
/// A write is validated by parsing and evaluating the source, which needs no
/// meshing; the object list after it is a convenience that meshes every part
/// (the whole CSG). On a heavy model that took longer than the client's
/// timeout, so a big edit "timed out" although it was saved, and a retry then
/// met a revision conflict. Past this budget the write answers without the
/// list (`objectsStatus: "pending"`) and `evaluate` does the meshing.
const WRITE_OBJECTS_BUDGET: Duration = Duration::from_secs(10);

#[cfg(test)]
thread_local! {
    /// Lets a test shrink the budget without touching the process environment.
    static WRITE_OBJECTS_BUDGET_OVERRIDE: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

fn write_objects_budget() -> Duration {
    #[cfg(test)]
    if let Some(budget) = WRITE_OBJECTS_BUDGET_OVERRIDE.with(std::cell::Cell::get) {
        return budget;
    }
    env::var("REOPENSCAD_WRITE_OBJECTS_BUDGET_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(WRITE_OBJECTS_BUDGET)
}

/// Object summaries for a write reply, or the `objectsStatus` and hint when
/// they were not computed.
type ObjectSummaries = Result<Vec<Value>, (&'static str, String)>;

/// Best-effort object summaries for a workspace payload: compiling, meshing
/// and measuring every part all within [`WRITE_OBJECTS_BUDGET`].
///
/// Used on write paths where a failure to mesh must not fail the write. `Ok`
/// with an empty list means "no addressable solids", which is exactly what a
/// 2D-only or empty document has; `Err` means the list is unknown (timed out,
/// cancelled or failed), which must never read as "every object removed".
fn workspace_object_summaries(workspace: &Workspace) -> ObjectSummaries {
    let budget = write_objects_budget();
    let cancellation = Arc::new(AtomicBool::new(false));
    let deadline = RenderDeadline::start(&cancellation, budget);
    let pending = || {
        (
            "pending",
            format!(
                "Saved as revision {}. Meshing the objects took over {} s, so this reply skips the object list; \
                 call evaluate (or list_design_objects) for objects, bounds and manifold checks.",
                workspace.revision,
                budget.as_secs_f64()
            ),
        )
    };
    let compiled = engine::compile_parts_shared(&workspace.code, engine::Quality::Preview, Some(&cancellation));
    if deadline.expired() {
        return Err(pending());
    }
    let output = compiled.map_err(|error| ("failed", format!("Saved, but meshing the objects failed: {error}")))?;
    agent::remember_objects(&workspace.id, workspace.revision, &output.parts);
    // Measuring stays under the same budget (and the job's cancellation): a
    // huge mesh's topology census is not free.
    let objects = csg::bsp::with_cancellation(Some(&cancellation), || {
        let mut objects = Vec::with_capacity(output.parts.len());
        for part in &output.parts {
            let mut summary = part_summary_public(part);
            // Agents writing code learn at once when a part compiled but is
            // not a valid closed solid.
            let measure = analysis::measure_mesh(&part.mesh);
            if !measure.complete {
                return None;
            }
            summary["meshOk"] = json!(measure.watertight() && measure.manifold() && measure.winding_consistent() && measure.volume >= 0.0);
            objects.push(summary);
        }
        Some(objects)
    });
    match objects {
        Some(objects) if !deadline.expired() => Ok(objects),
        _ if deadline.expired() => Err(pending()),
        _ => Err((
            "pending",
            format!(
                "Saved as revision {}. Checking the objects was interrupted, so this reply skips the object list; \
                 call evaluate (or list_design_objects) for objects, bounds and manifold checks.",
                workspace.revision
            ),
        )),
    }
}

fn mcp_workspace_link(
    state: &AppState,
    request: &Request,
    arguments: &Value,
) -> Result<Value, String> {
    let (id, _) = argument_design(state, arguments)?;
    Ok(json!({
        "ok": true,
        "designId": id,
        "workspaceId": id,
        "url": workspace_url(state, request, id),
        "appUri": format!("{DESIGN_APP_URI_PREFIX}{id}")
    }))
}

fn supported_scad_dictionary() -> Value {
    json!({
        "engine": "ReOpenSCAD native Rust evaluator",
        "geometry": {
            "primitives2d": ["square", "polygon", "circle", "text"],
            "primitives3d": ["cube", "sphere", "cylinder", "polyhedron"],
            "parameterAliases": {
                "circle": {"diameter": "d"},
                "sphere": {"diameter": "d"},
                "cylinder": {"diameter": "d", "bottomDiameter": "d1", "topDiameter": "d2"}
            },
            "transforms": ["translate", "rotate", "scale", "mirror", "multmatrix", "resize", "color"],
            "csg": ["union", "difference", "intersection", "hull", "minkowski", "render", "group"],
            "operations2d": ["offset", "projection"],
            "extrusions": ["linear_extrude", "rotate_extrude"],
            "extensions": {
                "note": "ReOpenSCAD extensions: NOT part of OpenSCAD. OpenSCAD prints 'WARNING: Ignoring unknown module' for them and draws nothing in their place, so a file using them renders differently there; a user module of the same name shadows the extension (as for any built-in).",
                "sweep_path": "sweep_path(points = [[x,y,z], ...]) children(); the volume the 3D children sweep when translated along the polyline: the union over segments of minkowski(children, segment), exact (convex pieces of the children hulled with their copy at each segment end). Use it to model insertion envelopes and keep-out volumes, e.g. difference() { body(); sweep_path([[0,0,0],[0,0,-40]]) offset_peg(); }. Up to 65 points; a single point is a plain translate; 2D children are ignored with a warning. The child must split into at most 64 convex pieces and pieces x segments must stay within 1024, otherwise the compile fails with a sweep_path() error (sweep hull() of a complex child, or parts one at a time). For checking a path instead of modelling it, use check_insertion."
            },
            "acceptedHints": {
                "note": "Accepted, evaluated, and deliberately without effect: they steer OpenSCAD's preview or text shaper, never the final mesh, so no warning is printed.",
                "convexity": ["polygon", "polyhedron", "linear_extrude", "rotate_extrude", "render", "minkowski", "projection", "resize"],
                "language": ["text"],
                "script": ["text (\"latin\"; any other script warns that glyphs are not shaped)"]
            },
            "ignoredWithWarning": {
                "note": "Arguments that would change geometry in OpenSCAD but are not modelled here. Each prints a WARNING line naming what was ignored.",
                "linear_extrude": ["segments", "v (any direction other than +Z)"],
                "text": ["direction ttb/btt and em with the built-in Stroke face", "script other than Latin (no complex shaping)"]
            }
        },
        "language": [
            "variables", "vectors", "ranges", "arithmetic", "comparison", "boolean operators",
            "bitwise operators (& | ~ << >>)", "hexadecimal literals (0x1f)",
            "ternary expressions", "if/else", "for", "intersection_for", "let", "assert",
            "echo", "modules", "functions", "children", "recursive modules"
        ],
        "functions": [
            "abs", "sign", "floor", "ceil", "round", "sqrt", "exp", "ln", "log", "sin",
            "cos", "tan", "asin", "acos", "atan", "pow", "min", "max", "norm", "cross",
            "rands", "len", "concat", "str", "chr", "ord", "lookup", "search", "is_undef",
            "is_bool", "is_num", "is_string", "is_list", "is_function", "version", "version_num"
        ],
        "exportFormats": EXPORT_FORMATS,
        "fonts": engine::available_fonts().into_iter().map(|font| json!({
            "family": font.family, "style": font.style, "font": font.font,
            "license": font.license, "description": font.description
        })).collect::<Vec<_>>(),
        "text": engine::TEXT_NOTES,
        "objectNaming": engine::OBJECT_NAMING_NOTES,
        "analysisTools": {
            "targets": "Every analysis tool takes designId and reads the current revision, the one the person sees in the viewport. To try something, save it as a named revision (evaluate with code or edits does it in one call); restore_revision copies an old revision on top.",
            "tools": {
                "evaluate": "Compile once, answer everything: objects with bounds/volume/manifold status (measure), every pairwise intersection against tolerance (check_intersections), optionally a PNG (image: true or render_image options), and diagnostics, in one evaluation.",
                "list_design_objects": "Object IDs, names, exact mesh bounds, colours, source lines.",
                "measure": "Per object: bounds, volume, surface area, components, watertight/manifold/winding, defects (located groups of open/non-manifold edges); pairs: exact minimum distance or overlap; groups: the union of several objects measured as one; splitComponents: each separate solid of an object on its own (object-N#k); opt-in checks [thickness, overhang, sharpEdges] for minimum wall thickness, overhangs (default 45 deg, +Z up, the bed is bedZ or the lowest Z of the whole model, so a floating object's underside counts) and sharp convex edges (default <= 90 deg).",
                "check_intersections": "Pairwise exact clearance (BVH triangle distance), overlap volume and bounds (CSG kernel); relation overlap/contact/clearanceViolation/clear/undetermined against tolerance (the required minimum gap), allowContact for intended seating; groups and splitComponents as in measure. Skipped or undetermined pairs carry determined:false and toleranceViolated:true and count against complete.",
                "render_image": "PNG with named or explicit camera, per-object visibility and colours (color() alpha below 1 renders translucent), section plane; views: up to 6 named views as one labelled contact sheet.",
                "describe_geometry": "Per object: units guess, principal axes with an aligning multmatrix, lay-flat candidates with matrices, mirror symmetry, fitted planes/cylinders/spheres (holes flagged).",
                "check_insertion": "Whether one object can travel a translation path (polyline of offsets, or direction + distance) past the others: exact swept-volume clearance per chunk plus conservative advancement to the first contact, so no contact is missed; passes, clearanceOk vs tolerance, collision (travel, points, penetration), minClearance and firstViolation with locations.",
                "probe": "Exact ray hits and signed closest-surface distances, up to 256 rays and 256 points per call.",
                "set_print_orientation / get_print_layout / export_print_parts": "Print orientation per object stored in the source as `// @print <object> rotate=[x, y, z]` or `face=-z|+x|...|[nx, ny, nz]` comment lines (OpenSCAD ignores them); the layout tool reports size and bed contact once oriented plus an automatic packing on the plate: the `plate` argument, or the stored value if there is one, otherwise a 256 mm bed (platesNeeded, overflow); the export tool returns one URL per object (oriented, on z = 0) and one per plate (3MF by default).",
                "section_profile": "Plane cross-section as 2D loops in the plane basis (areas, centroids, holes, placeMatrix), per-object net area and centroid, auto-simplified to maxPoints, optional polygon() code.",
                "fit_axis": "Centerline of elongated bodies from perpendicular section centroids (unbiased by tapers, unlike principalAxes): point, direction, straightness, taper, sections, alignMatrix.",
                "align_to": "Pure math: rotation taking 1-2 direction pairs onto targets, as multmatrix and OpenSCAD rotate() angles.",
                "place": "Translate centring objects on X/Y (or X/Y/Z) and dropping them to z = 0 after an optional matrix, from exact transformed bounds.",
                "diff": "Two stored revisions: per-object added/removed/changed, volume delta, exact changed region.",
                "extract_features": "Opening rims, wall transitions, scan boundaries and smooth surfaces of a scan, each described with its bounds and a within box for other tools, shown in one labelled picture; stores nothing.",
                "compare_surfaces": "Scan-to-model agreement without booleans (open meshes welcome): both directions, area-weighted rms/p95/max, coverage, unsupported model area (no scan behind it), facing mismatches, worst patches with coordinates, signed heatmaps of both surfaces.",
                "compare_sections": "Scan and model cut by the same planes (list or sweep), open scan contours kept open, deviation both ways per plane, worst points, one shared-scale overlay sheet with mm grid, dimensions and deviation markers.",
                "measure_between": "Ray distances from one surface to another (thickness, recess depth, air gap) with example rays and their first crossings, distributions and patches, classed observed / inferred / insufficient evidence.",
                "scan_report": "One call: surface heatmaps per feature box, shared-scale section overlays and thickness evidence with one verdict (matches, verified).",
                "fit_curve": "Constrained B-spline fit of a section contour or 2D points: control-point budget, tolerance, periodic, outlier rejection, constant-thickness band; editable SCAD, unsupported spans and an independent comparison with the data and the surface.",
                "fit_surface": "Constrained bicubic B-spline patches over a plane or cylinder frame: control budget, tolerance, smoothness, shared boundaries with C0 or tangent (G1) continuity; residuals over the whole surface, an independent comparison, unsupported areas, and a compiled SCAD shell to paste into the design; nothing is stored.",
                "get_parameters / set_parameters": "Top-level assignments with values and lines; rewrite values as one named revision.",
                "list_revisions / restore_revision": "The revision history, newest first, with name, source and time; restoring copies an old revision's source into a new revision on top. get_design_code with revision reads old source as text."
            },
            "intersectionSemantics": agent::INTERSECTION_SEMANTICS,
            "diagnostics": "Every evaluating response (objects, measure, intersections, image, diff, render) carries console (all ECHO/WARNING lines), warnings (the WARNING/ERROR subset) and elapsedMs. Render responses also report degenerateTriangles (zero-area triangles; exports remove them, see cleanup).",
            "renderAndExport": "render_design and the design render/export routes write the current revision. POST /api/render and POST /api/export compile the browser editor's own text when it could not be saved.",
            "rateLimits": "Evaluating calls spend a per-client compute budget. A 429 names the bucket (requests, compute or bytes) and gives retryAfterSeconds computed from its refill rate (also in Retry-After).",
            "http": "Same tools over HTTP: POST /api/designs/{id}/{evaluate|objects|measure|intersections|image|diff|parameters|print-layout} on the current revision. GET /api/designs/{id}/export?print=1 writes each object in its print orientation on z = 0; layout=1&sheet=N one plate of the layout (printer=<plate id>, spacing=mm). GET /api/designs/{id}/revisions lists retained revisions; POST there with {revision, revisionName, expectedRevision} restores one. Every /api/designs/... route is also served at its pre-rename alias /api/workspaces/...",
            "limits": {
                "analysisRequestSeconds": agent::BATCH_BUDGET.as_secs(),
                "exactBooleanTriangles": analysis::MAX_BOOLEAN_TRIANGLES,
                "imageMaxEdgePx": raster::MAX_SIZE,
                "retainedRevisions": MAX_REVISION_HISTORY,
                "retainedRevisionBytes": MAX_REVISION_HISTORY_BYTES,
                "revisionNameChars": MAX_REVISION_NAME_CHARS
            }
        },
        "notes": [
            "The dictionary describes implemented behavior, not full OpenSCAD parity.",
            "minkowski is exact on the polyhedral kernel; only the sampled fallback approximates it, with a warning.",
            "Unknown or misspelt arguments to built-in modules, surplus positional arguments, children given to primitives, and dropped 2D top-level objects are reported as WARNING lines.",
            "A module outside this list is ignored with a warning rather than failing the compile,              so the rest of the model still renders.",
            "Module recursion is bounded by the evaluator's stack budget; very deep recursion is              reported as an error instead of being allowed to exhaust the process."
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

/// How long one counter read answers `/api/stats` for, per process. The
/// landing page is the busiest public route, and this keeps a burst of
/// visitors from turning into one database read each.
const STATS_MEMO_TTL: Duration = Duration::from_secs(15);

/// `GET /api/stats` — public aggregate numbers for the landing page. Carries
/// nothing about any single workspace, so unlike every other response it may
/// be cached briefly, by browsers and by shared caches alike.
fn handle_stats(stream: &mut TcpStream, state: &AppState) -> io::Result<()> {
    static MEMO: Mutex<Option<(Instant, CreationStats)>> = Mutex::new(None);
    let memoised = MEMO
        .lock()
        .ok()
        .and_then(|memo| *memo)
        .filter(|(at, _)| at.elapsed() < STATS_MEMO_TTL)
        .map(|(_, count)| count);
    // Test builds read through: every test has its own temporary store, and a
    // process-wide memo would hand one test another test's count.
    let memoised = if cfg!(test) { None } else { memoised };
    let stats = match memoised {
        Some(stats) => stats,
        None => match state.workspaces.creation_stats() {
            Ok(stats) => {
                if let Ok(mut memo) = MEMO.lock() {
                    *memo = Some((Instant::now(), stats));
                }
                stats
            }
            Err(error) => return respond_error(stream, 500, &error),
        },
    };
    let body = json!({
        "designsCreated": stats.total,
        "designsCreatedLast24h": stats.last_24h,
        "designsCreatedLast7d": stats.last_7d,
        "workspacesCreated": stats.total,
        "workspacesCreatedLast24h": stats.last_24h,
    })
    .to_string();
    respond_text(
        stream,
        200,
        "application/json; charset=utf-8",
        body.as_bytes(),
        &[("Cache-Control", "public, max-age=30")],
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
    if input.get("overrides").is_some_and(|value| !value.is_null()) {
        return respond_error(stream, 400, &removed_argument_error("render", "overrides"));
    }
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
            "degenerateTriangles": compiled.degenerate_triangles,
            "console": compiled.console,
            "warnings": agent::warning_lines(&compiled.messages),
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
        degenerate_triangles: degenerate_triangle_count(&output.mesh),
        messages: output.messages.clone(),
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
    if input.get("overrides").is_some_and(|value| !value.is_null()) {
        return respond_error(stream, 400, &removed_argument_error("export", "overrides"));
    }
    let format = input
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("stl")
        .to_string();
    let options = match ExportOptions::from_request(format.as_str(), &input, &HashMap::new()) {
        Ok(options) => options,
        Err(error) => return respond_error(stream, 400, &error),
    };
    let object_ids = match requested_object_ids(&input, &HashMap::new()) {
        Ok(object_ids) => object_ids,
        Err(error) => return respond_error(stream, 400, &error),
    };
    let title = input
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty() && name.len() <= 200)
        .unwrap_or("model");
    let (data, report) = match export_source(&source, object_ids.as_deref(), options, title) {
        Ok(data) => data,
        Err(error) => return respond_error(stream, 422, &error),
    };
    respond_export(stream, format.as_str(), &data, &report)
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

fn handle_static(
    stream: &mut TcpStream,
    state: &AppState,
    route: &str,
    range: Option<&str>,
) -> io::Result<()> {
    if route.starts_with("/backend/") || route.starts_with("/.reopenscad-workspaces/") {
        return respond_text(stream, 404, "text/plain", b"Not found", &[]);
    }
    if workspace_shell_id(route).is_some() || shared_shell_token(route).is_some() {
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
    if relative.starts_with("media/") {
        return respond_media(stream, mime, &body, range);
    }
    respond_text(stream, 200, mime, &shell_body(mime, body), &[])
}

/// One `start-end`, `start-` or `-suffix` range, clamped to the body; `None`
/// when it is malformed or starts past the end. Multi-range requests are not
/// supported and fall into `None` as well.
fn parse_byte_range(spec: &str, len: u64) -> Option<(u64, u64)> {
    let last = len.checked_sub(1)?;
    let (start, end) = spec.split_once('-')?;
    let (start, end) = (start.trim(), end.trim());
    if start.is_empty() {
        let suffix: u64 = end.parse().ok()?;
        return (suffix > 0).then(|| (len.saturating_sub(suffix), last));
    }
    let start: u64 = start.parse().ok()?;
    let end = if end.is_empty() {
        last
    } else {
        end.parse::<u64>().ok()?.min(last)
    };
    (start <= end).then_some((start, end))
}

/// `media/` holds the landing page's promo video and its poster: large,
/// public, immutable between deploys. Unlike the rest of the web root they
/// are cacheable, and they answer single byte ranges, without which Safari
/// refuses to play a `<video>` at all.
fn respond_media(
    stream: &mut TcpStream,
    mime: &str,
    body: &[u8],
    range: Option<&str>,
) -> io::Result<()> {
    const CACHE: (&str, &str) = ("Cache-Control", "public, max-age=86400");
    const RANGES: (&str, &str) = ("Accept-Ranges", "bytes");
    let Some(spec) = range.and_then(|value| value.trim().strip_prefix("bytes=")) else {
        return respond_text(stream, 200, mime, body, &[CACHE, RANGES]);
    };
    let len = body.len() as u64;
    match parse_byte_range(spec, len) {
        Some((start, end)) => {
            let content_range = format!("bytes {start}-{end}/{len}");
            respond_text(
                stream,
                206,
                mime,
                &body[start as usize..=end as usize],
                &[CACHE, RANGES, ("Content-Range", content_range.as_str())],
            )
        }
        None => {
            let content_range = format!("bytes */{len}");
            respond_text(
                stream,
                416,
                "text/plain",
                b"Range not satisfiable",
                &[("Content-Range", content_range.as_str())],
            )
        }
    }
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

/// `/designs/<id>`: the canonical browser route of a design.
fn workspace_shell_id(route: &str) -> Option<&str> {
    route
        .strip_prefix("/designs/")
        .map(|id| id.trim_end_matches('/'))
        .filter(|id| valid_workspace_id(id))
}

/// `/workspaces/<id>`: the pre-rename browser route, answered with a
/// redirect to [`workspace_shell_id`]'s form so old links keep working.
fn legacy_workspace_shell_id(route: &str) -> Option<&str> {
    route
        .strip_prefix("/workspaces/")
        .map(|id| id.trim_end_matches('/'))
        .filter(|id| valid_workspace_id(id))
}

/// `/shared/<token>` serves the same shell as a workspace route. Which mode
/// the app comes up in is decided in the browser, from the path.
fn shared_shell_token(route: &str) -> Option<&str> {
    route
        .strip_prefix("/shared/")
        .map(|token| token.trim_end_matches('/'))
        .filter(|token| valid_share_token(token))
}

fn mime_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
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
        206 => "Partial Content",
        416 => "Range Not Satisfiable",
        202 => "Accepted",
        301 => "Moved Permanently",
        308 => "Permanent Redirect",
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
    let head_only = HEAD_ONLY.with(std::cell::Cell::get);
    // Single write path for the whole server; tally bytes here so the per-IP
    // byte budget sees exactly what was served.
    if !head_only {
        note_response_bytes(body.len() as u64);
    }
    // `Referrer-Policy: no-referrer` is the header that matters most here. The
    // path of a workspace page is its password, and the default policy would
    // put that path in the `Referer` of every request the page makes off-site.
    // The links in the shell already carry `rel="noreferrer"`, but that is a
    // per-element opt-in that the next link added would have to remember; this
    // is the whole-origin rule that does not depend on remembering. Nothing in
    // this server reads `Referer` (same-origin enforcement uses `Origin` and
    // `Host`), so suppressing it costs nothing.
    begin_response();
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n",
        body.len()
    )?;
    // `no-store` unless the caller opts a response in explicitly: a shared
    // cache holding a workspace response would hand one visitor's work to the
    // next, so cacheability is never a default.
    if !extra_headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("Cache-Control"))
    {
        write!(stream, "Cache-Control: no-store\r\n")?;
    }
    // Covers what a `<meta>` tag cannot: JSON API replies and STL/3MF/OBJ
    // exports are not HTML, and their URLs contain the workspace id too.
    if !is_indexable() {
        write!(stream, "X-Robots-Tag: {ROBOTS_TAG}\r\n")?;
    }
    for (name, value) in extra_headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write!(stream, "\r\n")?;
    if head_only {
        return Ok(());
    }
    stream.write_all(body)
}

/// RFC 3986 percent-encoding of everything but unreserved characters.
#[cfg(test)]
fn percent_encode(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

/// Triangles with (near) zero area, by the same 1e-12 doubled-area threshold
/// `measure` uses. O(n) and allocation-free, so every render can report it.
fn degenerate_triangle_count(mesh: &engine::Mesh) -> usize {
    mesh.triangles
        .iter()
        .filter(|triangle| {
            let [a, b, c] = triangle.vertices;
            let (u, v) = (
                [b.x - a.x, b.y - a.y, b.z - a.z],
                [c.x - a.x, c.y - a.y, c.z - a.z],
            );
            let n = [
                u[1] * v[2] - u[2] * v[1],
                u[2] * v[0] - u[0] * v[2],
                u[0] * v[1] - u[1] * v[0],
            ];
            (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt() <= 1e-12
        })
        .count()
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
#[path = "main_tests.rs"]
mod tests;
