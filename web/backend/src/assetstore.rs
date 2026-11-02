//! Server side of per-design mesh assets: scans and other STL meshes a design
//! can `import("name.stl")`.
//!
//! What lives here, and why it is not in `assets.rs`: that module is the
//! evaluator's view (an [`AssetSet`] installed for one compile). This one is
//! the service around it — name validation, the upload pipeline
//! (parse → clean → describe), the filesystem backend, the decoded-set cache,
//! one-time upload URLs, and the HTTP and MCP surfaces. The Postgres backend
//! is in `pgstore.rs`; both answer through the `Store` methods defined below.
//!
//! Assets belong to the design, not to a revision: a revision snapshots only
//! the source, so `diff` or an older revision evaluates against the assets the
//! design holds now.
//!
//! Uploads are temporary. A design holds one uploaded STL at a time, kept for
//! [`ASSET_TTL_MS`] from the moment its upload completes; nothing renews it.
//! Expiry is decided when metadata is read, so it is exact
//! however late the sweep ([`start_expiry_sweeper`]) frees the space; a small
//! record of what expired stays so `import()` can say why it failed.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub use crate::assets::ASSET_TTL_MS;
use crate::assets::{iso_utc, time_left, AssetSet, AssetSource, TriMesh};
use crate::{meshanalysis, meshimport, rendercache};
use crate::{
    current_client_key, respond_error, respond_queue_rejection, respond_text, respond_value,
    unix_millis, valid_workspace_id, AppState, ClientKey, Request, Store,
};

/// Assets one design may hold.
pub const MAX_ASSETS_PER_DESIGN: usize = 32;
/// Default ceiling on stored asset bytes across every design of a deployment;
/// `REOPENSCAD_MAX_ASSET_STORAGE_MIB` overrides it.
const DEFAULT_MAX_TOTAL_ASSET_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Largest file `upload_asset` accepts inline as base64, so the call still
/// fits the 2 MiB MCP request body. Larger files go through
/// `create_asset_upload`.
pub const MAX_INLINE_UPLOAD_BYTES: usize = 1_400_000;
/// How long a one-time upload URL stays valid.
pub const UPLOAD_TOKEN_TTL_MS: u64 = 10 * 60 * 1000;
/// Unused upload URLs held in memory (filesystem store) at once.
const MAX_PENDING_UPLOADS: usize = 256;
/// Unused upload URLs one design may have pending at once.
pub const MAX_PENDING_UPLOADS_PER_DESIGN: usize = 4;
/// Unused upload URLs one client (IP) may have pending at once.
pub const MAX_PENDING_UPLOADS_PER_CLIENT: usize = 8;
/// Large request bodies (asset uploads) being received or processed at once.
/// Each may hold up to `meshimport::MAX_UPLOAD_BYTES` in memory.
pub const MAX_UPLOADS_IN_FLIGHT: usize = 4;
/// Of those, how many one client (IP) may hold.
pub const MAX_UPLOADS_PER_CLIENT: usize = 2;
/// Bodies above this are "large" (a raw phone scan rather than a part):
/// they get their own, smaller slot counts, and clean one at a time.
pub const LARGE_UPLOAD_BYTES: usize = 25 * 1024 * 1024;
/// Large bodies in flight at once, and per client (IP).
pub const MAX_LARGE_UPLOADS_IN_FLIGHT: usize = 2;
pub const MAX_LARGE_UPLOADS_PER_CLIENT: usize = 1;
/// Body deadline for an asset upload: 128 MiB at ~450 KB/s.
pub const UPLOAD_BODY_DEADLINE: Duration = Duration::from_secs(300);
/// Slowest average rate an upload body may arrive at once
/// [`UPLOAD_RATE_GRACE`] has passed; slower senders are cut off so a few
/// trickling connections cannot hold every upload slot for two minutes.
pub const UPLOAD_MIN_BYTES_PER_SEC: u64 = 64 * 1024;
pub const UPLOAD_RATE_GRACE: Duration = Duration::from_secs(10);
/// Default stored asset bytes one client (IP) may add per hour (uploads and
/// copies); `REOPENSCAD_ASSET_QUOTA_MIB_PER_HOUR` overrides it.
const DEFAULT_CLIENT_QUOTA_BYTES: u64 = 512 * 1024 * 1024;
/// Default raw upload bytes (request bodies, before cleaning) one client may
/// send per hour; `REOPENSCAD_UPLOAD_RAW_MIB_PER_HOUR` overrides it. Stored
/// bytes are bounded separately by the triangle budget and the quota above.
const DEFAULT_CLIENT_RAW_UPLOAD_BYTES: u64 = 2048 * 1024 * 1024;
const QUOTA_WINDOW_MS: u64 = 60 * 60 * 1000;
/// Clients tracked by the quota at once.
const MAX_QUOTA_CLIENTS: usize = 16_384;
/// Smallest triangle budget a caller may ask for.
const MIN_TRIANGLE_BUDGET: usize = 100;
/// Decoded meshes kept in memory, and their total triangle ceiling.
const DECODED_MESHES: usize = 64;
const DECODED_MAX_TRIANGLES: usize = 3_000_000;

/// How often the background sweep looks for expired assets.
const EXPIRY_SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Expired asset names remembered per design, newest last.
pub const MAX_EXPIRED_NAMES: usize = 128;
/// The tool holding the reverse-engineering playbook.
pub const GUIDE_TOOL: &str = "get_reverse_engineering_guide";

/// The deployment-wide stored-bytes ceiling.
pub fn max_total_asset_bytes() -> u64 {
    std::env::var("REOPENSCAD_MAX_ASSET_STORAGE_MIB")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|mib| *mib >= 1)
        .map(|mib| mib.saturating_mul(1024 * 1024))
        .unwrap_or(DEFAULT_MAX_TOTAL_ASSET_BYTES)
}

/// `[A-Za-z0-9._-]{1,64}`, ending in `.stl` (any case), no leading dot, no
/// `..`. Safe as a path component and as a SCAD string literal as-is.
pub fn valid_asset_name(name: &str) -> bool {
    name.len() > 4
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && !name.starts_with('.')
        && !name.contains("..")
        && name.to_ascii_lowercase().ends_with(".stl")
}

const ASSET_NAME_RULE: &str = "Asset names use letters, digits, '.', '_' and '-' (at most 64 \
     characters), end in .stl, and do not start with '.' or contain '..'.";

/// Clamps a caller's triangle budget into the accepted range.
pub fn clamp_budget(budget: Option<u64>) -> usize {
    budget
        .map(|budget| (budget.min(usize::MAX as u64) as usize).clamp(MIN_TRIANGLE_BUDGET, meshimport::MAX_TRIANGLE_BUDGET))
        .unwrap_or(meshimport::DEFAULT_TRIANGLE_BUDGET)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// One stored asset's metadata. `report` is `{"clean": CleanReport,
/// "description": Description}` as written at upload time.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AssetMeta {
    pub name: String,
    pub sha256: String,
    pub bytes: u64,
    pub triangles: u64,
    pub created_at: u64,
    /// Unix ms. 0 on an asset stored before uploads expired: see [`Self::expiry`].
    #[serde(default)]
    pub expires_at: u64,
    #[serde(default)]
    pub report: Value,
}

/// `expiresAt`, its ISO form and the time left, as reply fields.
pub fn expiry_fields(expires_at: u64, now: u64) -> Value {
    let left = expires_at.saturating_sub(now);
    json!({
        "expiresAt": expires_at,
        "expiresAtIso": iso_utc(expires_at),
        "expiresInSeconds": left / 1000,
        "expiresIn": time_left(left),
    })
}

/// Writes [`expiry_fields`] into a reply object.
fn set_expiry_fields(value: &mut Value, expires_at: u64, now: u64) {
    if let (Some(object), Value::Object(fields)) = (value.as_object_mut(), expiry_fields(expires_at, now)) {
        object.extend(fields);
    }
}

/// The STL reply's line on what happens next: expiry, nudge, guide.
fn upload_expiry_line(expires_at: u64, now: u64) -> String {
    format!(
        "Kept 3 hours with no renewal: this STL is deleted at {} (in {}). Reverse-engineer it \
         into a real SCAD part and remove the STL, so the design outlives the scan: {GUIDE_TOOL} shows how.",
        iso_utc(expires_at),
        time_left(expires_at.saturating_sub(now)),
    )
}

impl AssetMeta {
    /// When this asset expires. One stored before uploads expired counts from
    /// when it was stored. A [legacy server-made entry](Self::legacy_server_made)
    /// was expired from the moment it was stored, so every read, the upload
    /// rule, `import()` and the sweep already treat it as gone.
    pub fn expiry(&self) -> u64 {
        if self.legacy_server_made() {
            self.created_at
        } else if self.expires_at == 0 {
            self.created_at.saturating_add(ASSET_TTL_MS)
        } else {
            self.expires_at
        }
    }

    pub fn expired(&self, now: u64) -> bool {
        now >= self.expiry()
    }

    /// Legacy: a mesh an older version made on the server (a fit, a crop)
    /// rather than an upload, marked `report.origin`. Nothing writes these any
    /// more; old entries only wait for the next sweep. See [`Self::expiry`].
    fn legacy_server_made(&self) -> bool {
        self.report.get("origin").is_some_and(|origin| !origin.is_null())
    }

    pub fn public(&self) -> Value {
        let mut value = json!({
            "name": self.name,
            "sha256": self.sha256,
            "bytes": self.bytes,
            "triangles": self.triangles,
            "createdAt": self.created_at,
            "report": self.report.get("clean").cloned().unwrap_or(Value::Null),
            "description": self.report.get("description").cloned().unwrap_or(Value::Null),
        });
        set_expiry_fields(&mut value, self.expiry(), unix_millis());
        value
    }
}

/// What a stored asset is, which decides how it may enter a design.
#[derive(Clone, Debug, PartialEq)]
pub enum AssetKind {
    /// An upload: the design's one STL.
    Upload,
    /// Copied with its expiry into a fresh design.
    Copy,
}

/// An upload that went through the pipeline: the cleaned mesh in its storage
/// encoding plus its metadata.
pub struct Processed {
    pub meta: AssetMeta,
    pub stl: Vec<u8>,
    pub kind: AssetKind,
}

/// The refusal of a second STL, naming the one held and how to remove it. It
/// goes to holders of a one-time URL too, so it never names the design.
pub fn second_upload_refusal(held: &AssetMeta, now: u64) -> AssetError {
    AssetError::new(
        409,
        format!(
            "This design already holds the STL {name} (expires in {left}), and a design keeps one STL at a time. Remove it first \
             with delete_asset (name \"{name}\"), DELETE .../assets/{name} or the remove button in the editor, then upload \
             again. Removing it starts a new 3-hour clock for the next upload.",
            name = held.name,
            left = time_left(held.expiry().saturating_sub(now)),
        ),
    )
}

/// The live upload among `metas`, if any.
fn live_upload(metas: &[AssetMeta], now: u64) -> Option<&AssetMeta> {
    metas.iter().find(|meta| !meta.expired(now))
}

/// Applies the one-STL rule to `processed` against the design's current
/// assets, and fixes its expiry. Stores call it inside the write's lock, so
/// racing uploads cannot both pass. Returns the metadata to store.
pub fn settle_put(existing: &[AssetMeta], processed: &Processed, now: u64) -> Result<AssetMeta, AssetError> {
    let mut meta = processed.meta.clone();
    match &processed.kind {
        AssetKind::Upload => {
            if let Some(held) = live_upload(existing, now) {
                return Err(second_upload_refusal(held, now));
            }
            if meta.expires_at == 0 {
                meta.expires_at = now + ASSET_TTL_MS;
            }
        }
        AssetKind::Copy => meta.expires_at = meta.expiry(),
    }
    Ok(meta)
}

/// Wall-clock cap on repairing one upload, so a hostile mesh cannot hold a
/// render permit for long: a base plus an allowance per million input
/// triangles (2.5M triangles clean in ~7 s on a laptop core; this leaves
/// room for slower server cores).
const UPLOAD_CLEAN_DEADLINE: Duration = Duration::from_secs(20);
const UPLOAD_CLEAN_PER_MILLION: Duration = Duration::from_secs(10);

fn clean_deadline(triangles: usize) -> Duration {
    UPLOAD_CLEAN_DEADLINE + UPLOAD_CLEAN_PER_MILLION.mul_f64(triangles as f64 / 1e6)
}

/// Large uploads clean one at a time: each peaks at several hundred MB.
static LARGE_CLEAN: Mutex<()> = Mutex::new(());

/// The 413 text for an upload body over the limit.
pub fn upload_too_large_message(bytes: Option<usize>) -> String {
    let size = bytes.map(|bytes| format!(" ({:.1} MiB)", bytes as f64 / (1024.0 * 1024.0))).unwrap_or_default();
    format!(
        "The upload{size} exceeds the {} MiB limit (about {:.1}M triangles of binary STL). The server decimates every upload \
         to the triangle budget (default {}, max {}), so a smaller export loses nothing: re-export as binary STL, or \
         decimate below {} MiB first.",
        meshimport::MAX_UPLOAD_BYTES / (1024 * 1024),
        (meshimport::MAX_UPLOAD_BYTES - 84) as f64 / 50.0 / 1e6,
        meshimport::DEFAULT_TRIANGLE_BUDGET,
        meshimport::MAX_TRIANGLE_BUDGET,
        meshimport::MAX_UPLOAD_BYTES / (1024 * 1024),
    )
}

/// Parses, cleans and describes an uploaded STL. CPU-heavy: callers hold a
/// render-queue permit.
pub fn process_upload(name: &str, bytes: &[u8], budget: usize, now: u64) -> Result<Processed, String> {
    if bytes.len() > meshimport::MAX_UPLOAD_BYTES {
        return Err(format!("{name}: {}", upload_too_large_message(Some(bytes.len()))));
    }
    if bytes.is_empty() {
        return Err(format!("{name} is empty; send the STL file as the request body."));
    }
    let _large = (bytes.len() > LARGE_UPLOAD_BYTES).then(|| LARGE_CLEAN.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
    let raw = meshimport::parse_stl(bytes).map_err(|error| format!("{name} could not be read as STL: {error}"))?;
    let deadline = std::time::Instant::now() + clean_deadline(raw.triangles.len());
    let (cleaned, report) = meshimport::clean_until(raw, meshimport::CleanOptions { triangle_budget: budget }, Some(deadline))
        .map_err(|error| format!("{name} could not be repaired in time: {error}"))?;
    if cleaned.triangles.is_empty() {
        return Err(format!("{name} has no usable triangles after cleaning."));
    }
    let stl = meshimport::encode_binary_stl(&cleaned);
    let description = meshanalysis::describe(&cleaned, meshanalysis::DescribeOptions::default());
    let report = json!({
        "clean": serde_json::to_value(&report).unwrap_or(Value::Null),
        "description": serde_json::to_value(&description).unwrap_or(Value::Null),
    });
    Ok(Processed {
        meta: AssetMeta {
            name: name.to_string(),
            sha256: hex(&rendercache::sha256(&stl)),
            bytes: stl.len() as u64,
            triangles: cleaned.triangles.len() as u64,
            created_at: now,
            expires_at: 0,
            report,
        },
        stl,
        kind: AssetKind::Upload,
    })
}

/// A failed asset operation, with the HTTP status it maps to.
#[derive(Debug, PartialEq)]
pub struct AssetError {
    pub status: u16,
    pub message: String,
}

impl AssetError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(500, message)
    }
}

pub fn storage_full() -> AssetError {
    AssetError::new(
        503,
        "The server's asset storage is full. Try again later or use a smaller triangle budget.",
    )
}

/// Applies the per-design and deployment-wide limits to replacing (or adding)
/// `incoming` among `existing`, given the deployment's current total. `Ok`
/// carries how many bytes other designs' assets must give up (least recently
/// used first) for it to fit under `max_total`; zero when it fits as is.
pub fn check_limits(existing: &[AssetMeta], incoming: &AssetMeta, total_bytes: u64, max_total: u64) -> Result<u64, AssetError> {
    let replaced = existing.iter().find(|meta| meta.name == incoming.name);
    let others: Vec<&AssetMeta> = existing.iter().filter(|meta| meta.name != incoming.name).collect();
    if replaced.is_none() && others.len() >= MAX_ASSETS_PER_DESIGN {
        return Err(AssetError::new(
            409,
            format!(
                "This design already holds {MAX_ASSETS_PER_DESIGN} assets, the limit. Delete one (delete_asset or DELETE /api/designs/{{id}}/assets/{{name}})."
            ),
        ));
    }
    let design_bytes: u64 = others.iter().map(|meta| meta.bytes).sum::<u64>() + incoming.bytes;
    let growth = incoming.bytes.saturating_sub(replaced.map_or(0, |meta| meta.bytes));
    // What this design keeps after the write can never be evicted to make
    // room for itself.
    if design_bytes > max_total {
        return Err(storage_full());
    }
    Ok(total_bytes.saturating_add(growth).saturating_sub(max_total))
}

/// `name=sha;name=sha` in name order: the same digest [`AssetSet::digest`]
/// computes, derived from metadata alone so a cache hit needs no blob reads.
#[cfg(test)]
pub fn digest_of(metas: &[AssetMeta]) -> String {
    let mut sorted: Vec<&AssetMeta> = metas.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    sorted
        .iter()
        .map(|meta| format!("{}={}", meta.name, meta.sha256))
        .collect::<Vec<_>>()
        .join(";")
}

/// Decodes a stored asset (binary STL this server wrote).
pub fn decode_stored(bytes: &[u8]) -> Result<TriMesh, String> {
    meshimport::parse_stl(bytes)
}

type MeshCell = Arc<OnceLock<Result<Arc<TriMesh>, String>>>;

/// Decoded meshes by content hash, most recently used first. A cell is
/// inserted before its mesh is read, so concurrent compiles that miss on the
/// same mesh wait for one read and one decode instead of each doing both.
static DECODED: Mutex<VecDeque<(String, MeshCell)>> = Mutex::new(VecDeque::new());

fn decoded_cache() -> std::sync::MutexGuard<'static, VecDeque<(String, MeshCell)>> {
    DECODED.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The mesh whose stored bytes hash to `sha256`, decoded once and shared.
/// `fetch` reads the stored bytes on a miss; a failure is reported to every
/// waiter and not remembered.
pub(crate) fn decoded_mesh(sha256: &str, fetch: impl FnOnce() -> Result<Vec<u8>, String>) -> Result<Arc<TriMesh>, String> {
    let cell = {
        let mut cache = decoded_cache();
        let cell = match cache.iter().position(|(key, _)| key == sha256) {
            Some(index) => cache.remove(index).map(|(_, cell)| cell).unwrap_or_default(),
            None => MeshCell::default(),
        };
        cache.push_front((sha256.to_string(), Arc::clone(&cell)));
        cell
    };
    let result = cell
        .get_or_init(|| {
            let bytes = fetch()?;
            if hex(&rendercache::sha256(&bytes)) != sha256 {
                return Err("A stored asset no longer matches its hash; upload it again.".to_string());
            }
            decode_stored(&bytes)
                .map(Arc::new)
                .map_err(|error| format!("A stored asset could not be decoded: {error}"))
        })
        .clone();
    let mut cache = decoded_cache();
    if result.is_err() {
        cache.retain(|(_, other)| !Arc::ptr_eq(other, &cell));
    }
    let (mut position, mut triangles) = (0usize, 0usize);
    cache.retain(|(_, cell)| {
        position += 1;
        triangles += cell
            .get()
            .and_then(|result| result.as_ref().ok())
            .map_or(0, |mesh| mesh.triangles.len());
        position <= DECODED_MESHES && (position == 1 || triangles <= DECODED_MAX_TRIANGLES)
    });
    result
}

/// A `'static` handle on the asset storage, for lazy sets that outlive the
/// borrow of the request's state.
#[derive(Clone)]
pub(crate) enum AssetReader {
    Filesystem(Arc<FsAssets>),
    #[cfg(feature = "postgres")]
    Postgres(crate::pgstore::PgWorkspaceStore),
}

/// `(name, sha256)` of a design's live assets and `(name, when)` of the
/// expired ones, from one read.
pub(crate) type AssetIndexView = (Vec<(String, String)>, Vec<(String, u64)>);

impl AssetReader {
    fn index(&self, id: &str) -> Result<AssetIndexView, String> {
        match self {
            AssetReader::Filesystem(assets) => Ok(assets.index_view(id, unix_millis())),
            #[cfg(feature = "postgres")]
            AssetReader::Postgres(store) => store.asset_index(id, unix_millis()),
        }
    }

    fn blob(&self, id: &str, name: &str, sha256: &str) -> Result<Option<Vec<u8>>, String> {
        match self {
            // Content-addressed: the file name is the hash.
            AssetReader::Filesystem(assets) => {
                let _ = name;
                assets.blob(id, sha256)
            }
            #[cfg(feature = "postgres")]
            AssetReader::Postgres(store) => store.asset_blob(id, name, sha256),
        }
    }
}

/// Design `id`'s assets as a lazy [`AssetSource`]: the index is read on the
/// first lookup or digest, each mesh when `import()` first names it.
struct DesignAssets {
    reader: AssetReader,
    id: String,
    /// What the index read found expired.
    expired: Mutex<Vec<(String, u64)>>,
}

/// Index reads per design, so tests can prove a route never read assets.
#[cfg(test)]
static INDEX_READS: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);

#[cfg(test)]
fn index_reads(id: &str) -> usize {
    INDEX_READS.lock().unwrap().as_ref().and_then(|reads| reads.get(id).copied()).unwrap_or(0)
}

/// Mesh blob reads per design.
#[cfg(test)]
static MESH_READS: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);

#[cfg(test)]
fn mesh_reads(id: &str) -> usize {
    MESH_READS.lock().unwrap().as_ref().and_then(|reads| reads.get(id).copied()).unwrap_or(0)
}

impl AssetSource for DesignAssets {
    fn index(&self) -> Result<Vec<(String, String)>, String> {
        #[cfg(test)]
        {
            *INDEX_READS.lock().unwrap().get_or_insert_with(HashMap::new).entry(self.id.clone()).or_insert(0) += 1;
        }
        let (live, expired) = self.reader.index(&self.id)?;
        *self.expired.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = expired;
        Ok(live)
    }

    fn expired(&self) -> Vec<(String, u64)> {
        self.expired.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    fn mesh(&self, name: &str, sha256: &str) -> Result<Arc<TriMesh>, String> {
        decoded_mesh(sha256, || {
            #[cfg(test)]
            {
                *MESH_READS.lock().unwrap().get_or_insert_with(HashMap::new).entry(self.id.clone()).or_insert(0) += 1;
            }
            if let Some(bytes) = self.reader.blob(&self.id, name, sha256)? {
                return Ok(bytes);
            }
            // Gone under this hash: say why rather than compiling without it.
            let (now, _) = self.reader.index(&self.id)?;
            Err(match now.iter().find(|(other, _)| other == name) {
                Some((_, current)) if current != sha256 => {
                    format!("Asset {name} was replaced while this compile ran; run it again.")
                }
                Some(_) => format!("Asset {name} is missing from storage."),
                None => format!("Asset {name} was deleted while this compile ran; run it again."),
            })
        })
    }
}

/// The lazy asset set of design `id`. Building it reads nothing.
pub fn design_set(state: &AppState, id: &str) -> Option<Arc<AssetSet>> {
    valid_workspace_id(id).then(|| {
        Arc::new(AssetSet::lazy(DesignAssets { reader: state.workspaces.asset_reader(), id: id.to_string(), expired: Mutex::default() }))
    })
}

/// Runs `f` with design `id`'s assets installed for `import()` on this
/// thread. Every compile of a design's code (stored or unsaved) goes through
/// here; compiles run on the request's own thread. Nothing is read until the
/// compile (holding its render permit) asks for an asset or the digest.
pub fn with_design_assets<R>(state: &AppState, id: Option<&str>, f: impl FnOnce() -> R) -> R {
    let set = id.and_then(|id| design_set(state, id));
    crate::assets::with_assets(set, f)
}

// ---------------------------------------------------------------------------
// Filesystem backend: `<root>/<id>.assets/index.json` plus one
// `<sha256>.stl` blob per distinct mesh.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FsIndex {
    version: u64,
    assets: Vec<AssetMeta>,
    /// What expired here, newest last, so `import()` can say why it fails.
    #[serde(default)]
    expired: Vec<ExpiredAsset>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExpiredAsset {
    pub name: String,
    pub expired_at: u64,
}

/// Moves the expired assets of `index` to its record of expired names.
/// Returns them; nothing is written, and their blobs are the caller's to drop
/// once the index is on disk.
fn drain_expired(index: &mut FsIndex, now: u64) -> Vec<AssetMeta> {
    let (gone, live): (Vec<AssetMeta>, Vec<AssetMeta>) = std::mem::take(&mut index.assets).into_iter().partition(|meta| meta.expired(now));
    index.assets = live;
    for meta in &gone {
        index.expired.retain(|other| other.name != meta.name);
        index.expired.push(ExpiredAsset { name: meta.name.clone(), expired_at: meta.expiry() });
    }
    let surplus = index.expired.len().saturating_sub(MAX_EXPIRED_NAMES);
    index.expired.drain(..surplus);
    gone
}

pub struct FsAssets {
    root: PathBuf,
    /// Serialises writers; also guards the version map and the total.
    state: Mutex<FsState>,
    nonce: AtomicU64,
    /// Chunked uploads in progress; their parts are files in `.uploads/`.
    uploads: Mutex<HashMap<String, FsUpload>>,
    /// Final replies of recently finished chunked uploads, oldest first, at
    /// most [`MAX_UPLOAD_RECEIPTS`].
    receipts: Mutex<VecDeque<UploadReceipt>>,
}

#[derive(Default)]
struct FsState {
    versions: HashMap<String, u64>,
    /// Stored bytes across every design, computed on first need.
    total: Option<u64>,
}

/// Suffix of a design's asset directory in the store root.
pub const FS_ASSET_DIR_SUFFIX: &str = ".assets";

impl FsAssets {
    pub fn new(root: PathBuf) -> Self {
        // Sessions live in memory, so parts from a previous process are
        // unreachable: drop them. remove_dir_all does not follow symlinks: a
        // symlink there is removed itself, never what it points to.
        let _ = fs::remove_dir_all(root.join(UPLOAD_PARTS_DIR));
        Self {
            root,
            state: Mutex::new(FsState::default()),
            nonce: AtomicU64::new(unix_millis()),
            uploads: Mutex::new(HashMap::new()),
            receipts: Mutex::new(VecDeque::new()),
        }
    }

    fn dir(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}{FS_ASSET_DIR_SUFFIX}"))
    }

    fn read_index(&self, id: &str) -> FsIndex {
        fs::read(self.dir(id).join("index.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FsState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_atomic(&self, path: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
        let parent = path.parent().unwrap_or(&self.root);
        let temporary = parent.join(format!(".{}.tmp", self.nonce.fetch_add(1, Ordering::Relaxed)));
        let result = (|| -> io::Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn total(&self, state: &mut FsState) -> u64 {
        if let Some(total) = state.total {
            return total;
        }
        let mut total = 0;
        if let Ok(entries) = fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(id) = name.to_str().and_then(|name| name.strip_suffix(FS_ASSET_DIR_SUFFIX)) else {
                    continue;
                };
                total += self.read_index(id).assets.iter().map(|meta| meta.bytes).sum::<u64>();
            }
        }
        state.total = Some(total);
        total
    }

    #[cfg(test)]
    pub fn list(&self, id: &str) -> Vec<AssetMeta> {
        if !valid_workspace_id(id) {
            return Vec::new();
        }
        self.snapshot(id).0
    }

    /// See [`AssetIndexView`].
    pub fn index_view(&self, id: &str, now: u64) -> AssetIndexView {
        if !valid_workspace_id(id) {
            return (Vec::new(), Vec::new());
        }
        let mut index = self.read_index(id);
        drain_expired(&mut index, now);
        (
            index.assets.into_iter().map(|meta| (meta.name, meta.sha256)).collect(),
            index.expired.into_iter().map(|gone| (gone.name, gone.expired_at)).collect(),
        )
    }

    /// The live assets and the version, from one read of the index.
    pub fn snapshot(&self, id: &str) -> (Vec<AssetMeta>, u64) {
        if !valid_workspace_id(id) {
            return (Vec::new(), 0);
        }
        let mut index = self.read_index(id);
        drain_expired(&mut index, unix_millis());
        let mut assets = index.assets;
        assets.sort_by(|a, b| a.name.cmp(&b.name));
        (assets, index.version)
    }

    pub fn version(&self, id: &str) -> u64 {
        if !valid_workspace_id(id) {
            return 0;
        }
        let mut state = self.lock();
        if let Some(version) = state.versions.get(id) {
            return *version;
        }
        let version = self.read_index(id).version;
        if state.versions.len() > 4096 {
            state.versions.clear();
        }
        state.versions.insert(id.to_string(), version);
        version
    }

    pub fn blob(&self, id: &str, sha256: &str) -> Result<Option<Vec<u8>>, String> {
        if !valid_workspace_id(id) || sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(None);
        }
        match fs::read(self.dir(id).join(format!("{sha256}.stl"))) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    /// Empties design `id`'s assets (keeping the design), bumping its
    /// version. Returns the bytes freed. The caller holds the state lock.
    fn clear_locked(&self, state: &mut FsState, id: &str) -> u64 {
        let mut index = self.read_index(id);
        if index.assets.is_empty() {
            return 0;
        }
        let freed: u64 = index.assets.iter().map(|meta| meta.bytes).sum();
        let dir = self.dir(id);
        let blobs: Vec<String> = index.assets.drain(..).map(|meta| meta.sha256).collect();
        index.version += 1;
        let Ok(bytes) = serde_json::to_vec_pretty(&index) else {
            return 0;
        };
        if self.write_atomic(&dir.join("index.json"), &bytes).is_err() {
            return 0;
        }
        for sha in blobs {
            let _ = fs::remove_file(dir.join(format!("{sha}.stl")));
        }
        if let Some(total) = state.total.as_mut() {
            *total = total.saturating_sub(freed);
        }
        state.versions.insert(id.to_string(), index.version);
        freed
    }

    /// Deletes the expired assets of every design, keeping a record of their
    /// names. Returns each design touched with what went from it.
    pub fn sweep_expired(&self, now: u64) -> Vec<(String, Vec<String>)> {
        let mut swept = Vec::new();
        let Ok(entries) = fs::read_dir(&self.root) else {
            return swept;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|name| name.strip_suffix(FS_ASSET_DIR_SUFFIX)) else {
                continue;
            };
            if !valid_workspace_id(id) || !self.read_index(id).assets.iter().any(|meta| meta.expired(now)) {
                continue;
            }
            let mut state = self.lock();
            let mut index = self.read_index(id);
            let gone = drain_expired(&mut index, now);
            if gone.is_empty() {
                continue;
            }
            index.version += 1;
            let dir = self.dir(id);
            let written = serde_json::to_vec_pretty(&index).ok().is_some_and(|bytes| self.write_atomic(&dir.join("index.json"), &bytes).is_ok());
            if !written {
                continue;
            }
            self.drop_blobs(&dir, &index, &gone);
            if let Some(total) = state.total.as_mut() {
                *total = total.saturating_sub(gone.iter().map(|meta| meta.bytes).sum());
            }
            state.versions.insert(id.to_string(), index.version);
            swept.push((id.to_string(), gone.into_iter().map(|meta| meta.name).collect()));
        }
        swept
    }

    /// Removes the blobs of `gone` that nothing in `index` still uses.
    fn drop_blobs(&self, dir: &std::path::Path, index: &FsIndex, gone: &[AssetMeta]) {
        for meta in gone {
            if !index.assets.iter().any(|other| other.sha256 == meta.sha256) {
                let _ = fs::remove_file(dir.join(format!("{}.stl", meta.sha256)));
            }
        }
    }

    /// Stores `processed` under its name, under the one-STL rule of
    /// [`settle_put`]. Expired assets go in the same write.
    /// The caller has checked that the design exists. Over the deployment
    /// ceiling, the assets of other designs are evicted in `lru` order (least
    /// recently used first). Returns the new assets version and the designs
    /// whose assets were evicted.
    pub fn put(&self, id: &str, processed: &Processed, max_total: u64, lru: &[String]) -> Result<(u64, Vec<String>), AssetError> {
        if !valid_workspace_id(id) {
            return Err(AssetError::new(404, "Design not found."));
        }
        let mut state = self.lock();
        let now = unix_millis();
        let mut index = self.read_index(id);
        let mut total = self.total(&mut state);
        // In memory until the index is written below: a refused write leaves
        // the expired assets to the sweep.
        let expired = drain_expired(&mut index, now);
        total = total.saturating_sub(expired.iter().map(|meta| meta.bytes).sum());
        let meta = settle_put(&index.assets, processed, now)?;
        let mut needed = check_limits(&index.assets, &meta, total, max_total)?;
        let mut evicted = Vec::new();
        if needed > 0 {
            for victim in lru.iter().filter(|victim| victim.as_str() != id) {
                if needed == 0 {
                    break;
                }
                let freed = self.clear_locked(&mut state, victim);
                if freed > 0 {
                    needed = needed.saturating_sub(freed);
                    evicted.push(victim.clone());
                }
            }
            let expired_bytes: u64 = expired.iter().map(|meta| meta.bytes).sum();
            total = self.total(&mut state).saturating_sub(expired_bytes);
            if needed > 0 {
                return Err(storage_full());
            }
        }
        let dir = self.dir(id);
        fs::create_dir_all(&dir).map_err(|error| AssetError::internal(format!("Asset could not be stored: {error}")))?;
        let blob = dir.join(format!("{}.stl", meta.sha256));
        if !blob.exists() {
            self.write_atomic(&blob, &processed.stl)
                .map_err(|error| AssetError::internal(format!("Asset could not be stored: {error}")))?;
        }
        let previous = index.assets.iter().position(|other| other.name == meta.name);
        let old = previous.map(|position| index.assets.remove(position));
        index.expired.retain(|gone| gone.name != meta.name);
        index.assets.push(meta.clone());
        index.version += 1;
        let bytes = serde_json::to_vec_pretty(&index).map_err(|error| AssetError::internal(error.to_string()))?;
        self.write_atomic(&dir.join("index.json"), &bytes)
            .map_err(|error| AssetError::internal(format!("Asset index could not be written: {error}")))?;
        self.drop_blobs(&dir, &index, &expired);
        if let Some(old) = old {
            if !index.assets.iter().any(|other| other.sha256 == old.sha256) {
                let _ = fs::remove_file(dir.join(format!("{}.stl", old.sha256)));
            }
            state.total = Some(total.saturating_sub(old.bytes) + meta.bytes);
        } else {
            state.total = Some(total + meta.bytes);
        }
        state.versions.insert(id.to_string(), index.version);
        Ok((index.version, evicted))
    }

    /// Removes live asset `name`; `None` when there was no such asset.
    pub fn delete(&self, id: &str, name: &str) -> Result<Option<u64>, String> {
        if !valid_workspace_id(id) {
            return Ok(None);
        }
        let mut state = self.lock();
        let mut index = self.read_index(id);
        let now = unix_millis();
        if !index.assets.iter().any(|meta| meta.name == name && !meta.expired(now)) {
            return Ok(None);
        }
        let total = self.total(&mut state);
        let (gone, kept): (Vec<AssetMeta>, Vec<AssetMeta>) = std::mem::take(&mut index.assets).into_iter().partition(|meta| meta.name == name);
        index.assets = kept;
        index.version += 1;
        let dir = self.dir(id);
        let bytes = serde_json::to_vec_pretty(&index).map_err(|error| error.to_string())?;
        self.write_atomic(&dir.join("index.json"), &bytes)
            .map_err(|error| format!("Asset index could not be written: {error}"))?;
        self.drop_blobs(&dir, &index, &gone);
        state.total = Some(total.saturating_sub(gone.iter().map(|meta| meta.bytes).sum()));
        state.versions.insert(id.to_string(), index.version);
        Ok(Some(index.version))
    }

    /// Deletes every asset of design `id` (eviction).
    pub fn remove_design(&self, id: &str) {
        if !valid_workspace_id(id) {
            return;
        }
        let mut state = self.lock();
        let dir = self.dir(id);
        if dir.exists() {
            let bytes: u64 = self.read_index(id).assets.iter().map(|meta| meta.bytes).sum();
            let _ = fs::remove_dir_all(&dir);
            if let Some(total) = state.total.as_mut() {
                *total = total.saturating_sub(bytes);
            }
        }
        state.versions.remove(id);
    }

    /// Removes asset directories whose design is gone (boot).
    pub fn sweep(&self, exists: impl Fn(&str) -> bool) {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|name| name.strip_suffix(FS_ASSET_DIR_SUFFIX)) else {
                continue;
            };
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) && !exists(id) {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
        self.lock().total = None;
    }
}

// ---------------------------------------------------------------------------
// One-time upload URLs.
// ---------------------------------------------------------------------------

/// What a one-time upload URL is for.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingUpload {
    pub design_id: String,
    pub name: String,
    pub budget: usize,
    pub expires_at: u64,
    /// The client (IP) that asked for it; empty when unknown.
    pub client: String,
}

static PENDING: Mutex<Option<HashMap<String, PendingUpload>>> = Mutex::new(None);

/// 32 lowercase hex digits (128 bits) from the OS.
pub fn mint_token() -> Result<String, String> {
    let mut random = [0u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut random))
        .map_err(|error| format!("No randomness available for an upload URL: {error}"))?;
    Ok(hex(&random))
}

pub fn valid_token(token: &str) -> bool {
    token.len() == 32 && token.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Why one more pending upload URL is refused, given how many are pending
/// across the deployment (`max_all` allowed), for its design, and for its
/// client.
pub fn pending_refusal(all: usize, design: usize, client: usize, max_all: usize) -> Option<String> {
    if design >= MAX_PENDING_UPLOADS_PER_DESIGN {
        Some(format!(
            "This design already has {MAX_PENDING_UPLOADS_PER_DESIGN} unused upload URLs; use one or wait for them to expire (10 minutes)."
        ))
    } else if client >= MAX_PENDING_UPLOADS_PER_CLIENT {
        Some(format!(
            "You already have {MAX_PENDING_UPLOADS_PER_CLIENT} unused upload URLs; use one or wait for them to expire (10 minutes)."
        ))
    } else if all >= max_all {
        Some("Too many upload URLs are pending; use one or wait for them to expire.".into())
    } else {
        None
    }
}

fn pending_map(guard: &mut Option<HashMap<String, PendingUpload>>) -> &mut HashMap<String, PendingUpload> {
    guard.get_or_insert_with(HashMap::new)
}

/// Records a pending upload (filesystem store; Postgres keeps them in a
/// table so any instance can take them).
pub fn remember_upload(token: &str, pending: PendingUpload, now: u64) -> Result<(), String> {
    let mut guard = PENDING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let map = pending_map(&mut guard);
    map.retain(|_, entry| entry.expires_at > now);
    let design = map.values().filter(|entry| entry.design_id == pending.design_id).count();
    let client = if pending.client.is_empty() {
        0
    } else {
        map.values().filter(|entry| entry.client == pending.client).count()
    };
    if let Some(refusal) = pending_refusal(map.len(), design, client, MAX_PENDING_UPLOADS) {
        return Err(refusal);
    }
    map.insert(token.to_string(), pending);
    Ok(())
}

/// A pending upload, without consuming it; `None` when unknown or expired.
pub fn peek_upload(token: &str, now: u64) -> Option<PendingUpload> {
    let guard = PENDING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.as_ref()?.get(token).filter(|entry| entry.expires_at > now).cloned()
}

/// Takes (and so consumes) a pending upload; `None` when unknown or expired.
pub fn take_upload(token: &str, now: u64) -> Option<PendingUpload> {
    let mut guard = PENDING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let entry = guard.as_mut()?.remove(token)?;
    (entry.expires_at > now).then_some(entry)
}

/// Moves a pending upload's expiry to at least `expires_at`.
pub fn extend_upload(token: &str, expires_at: u64) {
    let mut guard = PENDING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(entry) = pending_map(&mut guard).get_mut(token) {
        entry.expires_at = entry.expires_at.max(expires_at);
    }
}

/// Puts back an upload taken for a request that then failed.
pub fn restore_upload(token: &str, pending: PendingUpload) {
    let mut guard = PENDING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    pending_map(&mut guard).entry(token.to_string()).or_insert(pending);
}

// ---------------------------------------------------------------------------
// Large-body admission.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Slots {
    total: usize,
    large: usize,
    /// Per client: (all slots, large slots).
    per_client: HashMap<IpAddr, (usize, usize)>,
}

static SLOTS: Mutex<Option<Slots>> = Mutex::new(None);

/// One of [`MAX_UPLOADS_IN_FLIGHT`] slots (at most
/// [`MAX_UPLOADS_PER_CLIENT`] per client) for receiving and processing a
/// body over 2 MiB; a `large` one (over [`LARGE_UPLOAD_BYTES`]) also takes
/// one of [`MAX_LARGE_UPLOADS_IN_FLIGHT`] (one per client). Released on drop.
pub struct UploadSlot {
    client: Option<IpAddr>,
    large: bool,
}

impl UploadSlot {
    /// A slot for `client`, or why there is none.
    pub fn try_acquire(client: Option<IpAddr>, large: bool) -> Result<Self, &'static str> {
        let mut guard = SLOTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let slots = guard.get_or_insert_with(Slots::default);
        if let Some(client) = client {
            let (all, big) = slots.per_client.get(&client).copied().unwrap_or((0, 0));
            if all >= MAX_UPLOADS_PER_CLIENT {
                return Err("You already have uploads in progress; wait for one to finish.");
            }
            if large && big >= MAX_LARGE_UPLOADS_PER_CLIENT {
                return Err("You already have a large upload (over 25 MiB) in progress; wait for it to finish.");
            }
        }
        if slots.total >= MAX_UPLOADS_IN_FLIGHT {
            return Err("Too many uploads are in progress; retry in a few seconds");
        }
        if large && slots.large >= MAX_LARGE_UPLOADS_IN_FLIGHT {
            return Err("Too many large uploads are in progress; retry in a minute");
        }
        slots.total += 1;
        slots.large += usize::from(large);
        if let Some(client) = client {
            let entry = slots.per_client.entry(client).or_insert((0, 0));
            entry.0 += 1;
            entry.1 += usize::from(large);
        }
        Ok(Self { client, large })
    }
}

impl Drop for UploadSlot {
    fn drop(&mut self) {
        let mut guard = SLOTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let slots = guard.get_or_insert_with(Slots::default);
        slots.total = slots.total.saturating_sub(1);
        if self.large {
            slots.large = slots.large.saturating_sub(1);
        }
        if let Some(client) = self.client {
            if let Some(count) = slots.per_client.get_mut(&client) {
                count.0 = count.0.saturating_sub(1);
                if self.large {
                    count.1 = count.1.saturating_sub(1);
                }
                if count.0 == 0 {
                    slots.per_client.remove(&client);
                }
            }
        }
    }
}

/// Whether an upload body arriving at this pace may continue: after
/// [`UPLOAD_RATE_GRACE`], the average must stay at or above
/// [`UPLOAD_MIN_BYTES_PER_SEC`].
pub fn upload_pace_ok(received: usize, elapsed: Duration) -> bool {
    elapsed <= UPLOAD_RATE_GRACE || received as f64 >= elapsed.as_secs_f64() * UPLOAD_MIN_BYTES_PER_SEC as f64
}

/// Checks, before a single body byte is read, that an upload route names
/// something that can take it: a pending one-time URL, or an existing design
/// and a valid asset name. Cheap reads only.
pub(crate) fn admit_upload_target(state: &AppState, head: &Request) -> Result<(), AssetError> {
    let path = head.path.as_str();
    if let Some(token) = path.strip_prefix("/api/uploads/") {
        let token = token.trim_end_matches('/');
        return match state.workspaces.peek_upload_token(token) {
            // A chunk of an upload in progress is judged when it finishes.
            Ok(Some(pending)) if request_upload_id(head).is_none() => admit_new_upload(state, &pending.design_id),
            Ok(Some(_)) => Ok(()),
            // Used up by its upload; a re-send of that upload's last chunk
            // still gets the stored reply.
            Ok(None) if replays_token_upload(state, head, token) => Ok(()),
            Ok(None) => Err(AssetError::new(404, UNKNOWN_UPLOAD_URL)),
            Err(error) => Err(AssetError::internal(error)),
        };
    }
    let Some((id, action)) = crate::workspace_api_route(path) else {
        return Err(AssetError::new(404, "Not found."));
    };
    let name = action.strip_prefix("assets/").unwrap_or_default();
    if !valid_asset_name(name) {
        return Err(AssetError::new(400, format!("Invalid asset name {name:?}. {ASSET_NAME_RULE}")));
    }
    design_exists(state, id)?;
    if request_upload_id(head).is_none() {
        admit_new_upload(state, id)?;
    }
    Ok(())
}

/// Routes whose body is a raw asset upload: `PUT /api/{designs|workspaces}/{id}/assets/{name}`
/// and `PUT /api/uploads/{token}`. They get the larger body limit, pre-body
/// admission, and skip the JSON content-type rule.
pub fn is_upload_route(method: &str, path: &str) -> bool {
    if method != "PUT" {
        return false;
    }
    if let Some(token) = path.strip_prefix("/api/uploads/") {
        return valid_token(token.trim_end_matches('/'));
    }
    crate::workspace_api_route(path).is_some_and(|(_, action)| {
        action.strip_prefix("assets/").is_some_and(|name| !name.is_empty() && !name.contains('/'))
    })
}

const UNKNOWN_UPLOAD_URL: &str = "This upload URL is unknown, used or expired. Ask for a new one with create_asset_upload.";

// ---------------------------------------------------------------------------
// Chunked uploads.
//
// Cloud Run refuses request bodies over 32 MiB, so a file larger than one
// chunk is sent as several PUTs to the same upload route, each with a
// standard `Content-Range: bytes start-end/total` header. The first chunk
// (start 0, no `Upload-Id`) opens a session and its 202 reply names the
// `uploadId`; every later chunk sends it back as the `Upload-Id` header (or
// `?uploadId=`). Chunks arrive in order; a wrong offset is answered 409 with
// `next`, so a client resumes from there. The final chunk runs the normal
// parse → clean → describe → store pipeline and answers like a single PUT.
//
// Parts are staged where every instance can reach them: Postgres rows with
// that store, a file under the data directory with the filesystem one —
// never in memory between requests.
// ---------------------------------------------------------------------------

/// The chunk size clients are told to use: under Cloud Run's 32 MiB body cap
/// with room for headers.
pub const UPLOAD_CHUNK_BYTES: usize = 24 * 1024 * 1024;
/// Largest chunk accepted.
pub const MAX_UPLOAD_CHUNK_BYTES: usize = 30 * 1024 * 1024;
/// Smallest chunk other than the last, so a file is at most ~128 parts.
pub const MIN_UPLOAD_CHUNK_BYTES: usize = 1024 * 1024;
/// A session with no chunk for this long is dropped with its parts.
pub const UPLOAD_SESSION_IDLE_MS: u64 = 30 * 60 * 1000;
/// Sessions one client (IP) may have open at once.
pub const MAX_UPLOAD_SESSIONS_PER_CLIENT: usize = 2;
/// Sessions one design may have open at once.
pub const MAX_UPLOAD_SESSIONS_PER_DESIGN: usize = 2;
/// Declared bytes one client's open sessions may add up to.
pub const MAX_STAGED_BYTES_PER_CLIENT: u64 = 256 * 1024 * 1024;
/// Declared bytes all open sessions may add up to (per instance with the
/// filesystem store, across the deployment with Postgres).
pub const MAX_STAGED_BYTES: u64 = 1024 * 1024 * 1024;
/// Directory, under the filesystem store's root, holding staged parts.
const UPLOAD_PARTS_DIR: &str = ".uploads";
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
const O_NOFOLLOW: i32 = 0x0100;
#[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "arm")))]
const O_NOFOLLOW: i32 = 0o100000;
#[cfg(all(target_os = "linux", not(any(target_arch = "aarch64", target_arch = "arm"))))]
const O_NOFOLLOW: i32 = 0o400000;

/// Opens a staged part without following a symlink in its place.
fn open_part(path: &std::path::Path, write: bool) -> io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(!write).write(write);
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "linux"))]
    options.custom_flags(O_NOFOLLOW);
    options.open(path)
}
/// How long a finished upload's final reply is kept, so a re-sent last chunk
/// (its reply was lost) gets that reply again instead of a 404.
pub const UPLOAD_RECEIPT_TTL_MS: u64 = 10 * 60 * 1000;
/// Receipts kept at once (per instance with the filesystem store, across the
/// deployment with Postgres); the oldest go first.
pub const MAX_UPLOAD_RECEIPTS: usize = 1024;
/// A claim to finish an upload older than this is taken for a crashed
/// finisher, and the last chunk may finish it again.
pub const UPLOAD_FINISH_CLAIM_MS: u64 = 3 * 60 * 1000;
pub(crate) const UPLOAD_FINISHING: &str = "This upload's last chunk is being processed by another request; send it again \
     in a few seconds to get the result.";

pub(crate) const UNKNOWN_UPLOAD_SESSION: &str = "This chunked upload is unknown or expired (sessions expire after 30 minutes \
     without a chunk). Start again from byte 0 without an Upload-Id header.";
const CONTENT_RANGE_RULE: &str = "Content-Range must read \"bytes <first>-<last>/<total>\" (inclusive byte \
     positions, e.g. bytes 0-25165823/60000000).";

/// A request's `Content-Range`; `end` is exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentRange {
    pub start: u64,
    pub end: u64,
    pub total: u64,
}

impl ContentRange {
    pub fn len(&self) -> u64 {
        self.end - self.start
    }
    pub fn is_final(&self) -> bool {
        self.end == self.total
    }
    /// The whole file in one request: no session needed.
    pub fn is_whole(&self) -> bool {
        self.start == 0 && self.is_final()
    }
}

/// Parses `bytes first-last/total` (inclusive positions, known total).
pub fn parse_content_range(value: &str) -> Option<ContentRange> {
    let rest = value.trim().strip_prefix("bytes")?;
    let (span, total) = rest.trim_start().split_once('/')?;
    let (first, last) = span.split_once('-')?;
    let number = |text: &str| -> Option<u64> {
        let text = text.trim();
        (!text.is_empty() && text.len() <= 15 && text.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| text.parse().ok())
            .flatten()
    };
    let (start, last, total) = (number(first)?, number(last)?, number(total)?);
    (start <= last && last < total).then_some(ContentRange { start, end: last + 1, total })
}

/// The `Content-Range` of an upload request, checked against its body length
/// and the limits before the body is read. `Ok(None)` without the header.
pub fn upload_range(headers: &[(String, String)], content_length: usize) -> Result<Option<ContentRange>, AssetError> {
    let Some(value) = headers.iter().find(|(name, _)| name == "content-range").map(|(_, value)| value.as_str()) else {
        return Ok(None);
    };
    let range = parse_content_range(value).ok_or_else(|| AssetError::new(400, CONTENT_RANGE_RULE))?;
    if range.total > meshimport::MAX_UPLOAD_BYTES as u64 {
        return Err(AssetError::new(413, upload_too_large_message(Some(range.total.min(usize::MAX as u64) as usize))));
    }
    if range.len() != content_length as u64 {
        return Err(AssetError::new(
            400,
            format!("Content-Range covers {} bytes but the body has {content_length}.", range.len()),
        ));
    }
    if range.len() > MAX_UPLOAD_CHUNK_BYTES as u64 {
        return Err(AssetError::new(
            413,
            format!(
                "A chunk may carry at most {} MiB; send {} MiB chunks.",
                MAX_UPLOAD_CHUNK_BYTES / (1024 * 1024),
                UPLOAD_CHUNK_BYTES / (1024 * 1024)
            ),
        ));
    }
    if !range.is_final() && range.len() < MIN_UPLOAD_CHUNK_BYTES as u64 {
        return Err(AssetError::new(
            400,
            format!("Every chunk but the last must carry at least {} MiB.", MIN_UPLOAD_CHUNK_BYTES / (1024 * 1024)),
        ));
    }
    Ok(Some(range))
}

/// The session a chunk names: the `Upload-Id` (or `X-Upload-Id`) header, or
/// the `uploadId` query parameter. Empty means none.
fn request_upload_id(request: &Request) -> Option<String> {
    request
        .header("upload-id")
        .or_else(|| request.header("x-upload-id"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| crate::query_parameters(&request.query).get("uploadId").filter(|value| !value.is_empty()).cloned())
}

/// One chunked upload in progress.
#[derive(Clone, Debug, PartialEq)]
pub struct UploadSession {
    /// 32 hex digits, minted by the server.
    pub upload_id: String,
    /// What it uploads to: `design:<id>/<name>` or `token:<token>`.
    pub target: String,
    pub design_id: String,
    pub name: String,
    pub budget: usize,
    /// Declared file size (the `/total` of every chunk).
    pub total: u64,
    /// Bytes staged so far, contiguous from 0.
    pub received: u64,
    /// The client (IP) that opened it; empty when unknown.
    pub client: String,
    pub updated_at: u64,
}

impl UploadSession {
    pub fn expired(&self, now: u64) -> bool {
        now.saturating_sub(self.updated_at) >= UPLOAD_SESSION_IDLE_MS
    }

    /// Whether opening `new` drops this session: the same client starting the
    /// same target over (a reload, a lost upload id).
    pub fn replaced_by(&self, new: &UploadSession) -> bool {
        self.target == new.target && self.name == new.name && self.client == new.client
    }

    fn progress(&self) -> Value {
        json!({"uploadId": self.upload_id, "received": self.received, "next": self.received, "total": self.total})
    }
}

/// The reply a finished chunked upload gave, kept for
/// [`UPLOAD_RECEIPT_TTL_MS`] so the same last chunk sent again (its reply was
/// lost) is answered with it.
#[derive(Clone, Debug, PartialEq)]
pub struct UploadReceipt {
    pub upload_id: String,
    pub target: String,
    pub total: u64,
    /// Where the last chunk started.
    pub final_start: u64,
    pub response: Value,
    pub created_at: u64,
}

impl UploadReceipt {
    pub fn expired(&self, now: u64) -> bool {
        now.saturating_sub(self.created_at) >= UPLOAD_RECEIPT_TTL_MS
    }

    /// Whether a chunk for `target` covering `range` repeats the last chunk.
    pub fn replays(&self, target: &str, range: &ContentRange) -> bool {
        self.target == target && self.total == range.total && range.is_final() && self.final_start == range.start
    }
}

/// Whether a finish claim taken at `claimed_at` (0: none) still holds.
pub fn finish_claimed(claimed_at: u64, now: u64) -> bool {
    claimed_at != 0 && now.saturating_sub(claimed_at) < UPLOAD_FINISH_CLAIM_MS
}

/// Why `new` may not open beside the `live` sessions (expired and replaced
/// ones already removed), or `None`.
pub fn session_refusal(live: &[UploadSession], new: &UploadSession, max_staged: u64) -> Option<AssetError> {
    if new.target.starts_with("token:") && live.iter().any(|session| session.target == new.target) {
        return Some(AssetError::new(
            409,
            "This upload URL is bound to another chunked upload in progress; resume that one with its Upload-Id, or wait \
             30 minutes for it to expire.",
        ));
    }
    if live.iter().filter(|session| session.design_id == new.design_id).count() >= MAX_UPLOAD_SESSIONS_PER_DESIGN {
        return Some(AssetError::new(
            429,
            format!("This design already has {MAX_UPLOAD_SESSIONS_PER_DESIGN} chunked uploads in progress; finish one first."),
        ));
    }
    if !new.client.is_empty() {
        let mine: Vec<&UploadSession> = live.iter().filter(|session| session.client == new.client).collect();
        if mine.len() >= MAX_UPLOAD_SESSIONS_PER_CLIENT {
            return Some(AssetError::new(
                429,
                format!("You already have {MAX_UPLOAD_SESSIONS_PER_CLIENT} chunked uploads in progress; finish one first."),
            ));
        }
        if mine.iter().map(|session| session.total).sum::<u64>().saturating_add(new.total) > MAX_STAGED_BYTES_PER_CLIENT {
            return Some(AssetError::new(
                429,
                format!(
                    "Your uploads in progress would exceed {} MiB; finish one first.",
                    MAX_STAGED_BYTES_PER_CLIENT / (1024 * 1024)
                ),
            ));
        }
    }
    if live.iter().map(|session| session.total).sum::<u64>().saturating_add(new.total) > max_staged {
        return Some(AssetError::new(503, "Too many large uploads are in progress; retry in a few minutes."));
    }
    None
}

/// Whether a chunk at `start` of `len` bytes is the next one for a session
/// holding `received` bytes: `Ok(true)` to write it, `Ok(false)` for a
/// re-send of the chunk that ended at `received` (already stored), 409
/// otherwise.
pub fn chunk_fits(received: u64, start: u64, len: u64) -> Result<bool, AssetError> {
    if start == received {
        Ok(true)
    } else if start < received && start + len == received {
        Ok(false)
    } else {
        Err(AssetError::new(409, format!("Chunks must arrive in order: send the chunk starting at byte {received} next.")))
    }
}

struct FsUpload {
    session: UploadSession,
    /// A chunk is being written; the entry is not swept or replaced.
    busy: bool,
    /// When a request claimed the finished file to process it (0: none);
    /// while it holds, the entry is not swept or replaced and no other
    /// request processes it.
    finishing_since: u64,
}

impl FsAssets {
    /// A session's staged file. Only server-minted ids (32 hex digits) name
    /// one; anything else maps to a name no session can have.
    fn part_path(&self, upload_id: &str) -> PathBuf {
        let name = if valid_token(upload_id) { upload_id } else { "invalid" };
        self.root.join(UPLOAD_PARTS_DIR).join(format!("{name}.part"))
    }

    /// The staging directory, created if missing; refused when it is not a
    /// real directory (a symlink planted there would point writes elsewhere).
    fn parts_dir(&self) -> io::Result<PathBuf> {
        let dir = self.root.join(UPLOAD_PARTS_DIR);
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.file_type().is_dir() => return Ok(dir),
            Ok(_) => return Err(io::Error::other(format!("{} is not a directory", dir.display()))),
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            Err(_) => {}
        }
        fs::create_dir_all(&self.root)?;
        match fs::create_dir(&dir) {
            Ok(()) => Ok(dir),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => match fs::symlink_metadata(&dir) {
                Ok(meta) if meta.file_type().is_dir() => Ok(dir),
                _ => Err(io::Error::other(format!("{} is not a directory", dir.display()))),
            },
            Err(error) => Err(error),
        }
    }

    fn uploads(&self) -> std::sync::MutexGuard<'_, HashMap<String, FsUpload>> {
        self.uploads.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn drop_upload_locked(&self, uploads: &mut HashMap<String, FsUpload>, upload_id: &str) {
        uploads.remove(upload_id);
        let _ = fs::remove_file(self.part_path(upload_id));
    }

    /// Drops expired sessions and their parts.
    fn sweep_uploads_locked(&self, uploads: &mut HashMap<String, FsUpload>, now: u64) {
        let expired: Vec<String> = uploads
            .iter()
            .filter(|(_, upload)| !upload.busy && !finish_claimed(upload.finishing_since, now) && upload.session.expired(now))
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.drop_upload_locked(uploads, &id);
        }
    }

    pub fn upload_session(&self, upload_id: &str, now: u64) -> Option<UploadSession> {
        self.uploads().get(upload_id).filter(|upload| !upload.session.expired(now)).map(|upload| upload.session.clone())
    }

    pub fn open_upload(&self, session: &UploadSession, now: u64) -> Result<(), AssetError> {
        let mut uploads = self.uploads();
        self.sweep_uploads_locked(&mut uploads, now);
        let replaced: Vec<String> = uploads
            .iter()
            .filter(|(_, upload)| {
                !upload.busy && !finish_claimed(upload.finishing_since, now) && upload.session.replaced_by(session)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in replaced {
            self.drop_upload_locked(&mut uploads, &id);
        }
        let live: Vec<UploadSession> = uploads.values().map(|upload| upload.session.clone()).collect();
        if let Some(refusal) = session_refusal(&live, session, MAX_STAGED_BYTES) {
            return Err(refusal);
        }
        if !valid_token(&session.upload_id) {
            return Err(AssetError::internal("The upload id is not a server-minted one."));
        }
        let path = self.part_path(&session.upload_id);
        let created = (|| -> io::Result<()> {
            self.parts_dir()?;
            // create_new (O_EXCL): never opens an existing file or symlink.
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            options.open(&path).map(drop)
        })();
        created.map_err(|error| AssetError::internal(format!("The upload could not be staged: {error}")))?;
        uploads.insert(session.upload_id.clone(), FsUpload { session: session.clone(), busy: false, finishing_since: 0 });
        Ok(())
    }

    /// Stages a chunk at `start`; returns the bytes received so far.
    pub fn append_upload(&self, upload_id: &str, start: u64, bytes: &[u8], now: u64) -> Result<u64, AssetError> {
        {
            let mut uploads = self.uploads();
            let upload = uploads
                .get_mut(upload_id)
                .filter(|upload| !upload.session.expired(now))
                .ok_or_else(|| AssetError::new(404, UNKNOWN_UPLOAD_SESSION))?;
            if !chunk_fits(upload.session.received, start, bytes.len() as u64)? {
                upload.session.updated_at = now;
                return Ok(upload.session.received);
            }
            if upload.busy {
                return Err(AssetError::new(409, "Another chunk of this upload is still arriving; wait for its reply."));
            }
            upload.busy = true;
        }
        // Written outside the lock: other uploads do not wait on this disk write.
        let written = (|| -> io::Result<()> {
            use std::io::{Seek, SeekFrom};
            let mut file = open_part(&self.part_path(upload_id), true)?;
            file.set_len(start)?;
            file.seek(SeekFrom::Start(start))?;
            file.write_all(bytes)?;
            file.flush()
        })();
        let mut uploads = self.uploads();
        let upload = uploads.get_mut(upload_id).ok_or_else(|| AssetError::new(404, UNKNOWN_UPLOAD_SESSION))?;
        upload.busy = false;
        written.map_err(|error| AssetError::internal(format!("The chunk could not be staged: {error}")))?;
        upload.session.received = start + bytes.len() as u64;
        upload.session.updated_at = now;
        Ok(upload.session.received)
    }

    /// The staged file, once complete.
    pub fn upload_bytes(&self, upload_id: &str) -> Result<Vec<u8>, String> {
        let total = self.uploads().get(upload_id).map(|upload| upload.session.total).ok_or(UNKNOWN_UPLOAD_SESSION)?;
        let mut bytes = Vec::with_capacity(total.min(meshimport::MAX_UPLOAD_BYTES as u64) as usize);
        open_part(&self.part_path(upload_id), false)
            .and_then(|mut file| file.read_to_end(&mut bytes))
            .map_err(|error| format!("The staged upload could not be read: {error}"))?;
        if bytes.len() as u64 != total {
            return Err(format!("The staged upload holds {} of {total} bytes; send the missing chunks.", bytes.len()));
        }
        Ok(bytes)
    }

    pub fn drop_upload(&self, upload_id: &str) {
        let mut uploads = self.uploads();
        self.drop_upload_locked(&mut uploads, upload_id);
    }

    /// Claims a complete upload for processing: `Ok(false)` while another
    /// request's claim holds.
    pub fn claim_finish(&self, upload_id: &str, now: u64) -> Result<bool, String> {
        let mut uploads = self.uploads();
        let upload = uploads.get_mut(upload_id).ok_or(UNKNOWN_UPLOAD_SESSION)?;
        if finish_claimed(upload.finishing_since, now) {
            return Ok(false);
        }
        upload.finishing_since = now.max(1);
        upload.session.updated_at = now;
        Ok(true)
    }

    /// Gives a claim back after a failure worth retrying.
    pub fn release_finish(&self, upload_id: &str) {
        if let Some(upload) = self.uploads().get_mut(upload_id) {
            upload.finishing_since = 0;
        }
    }

    /// Ends a finished upload: drops its session and parts and keeps its
    /// reply for a re-sent last chunk.
    pub fn complete_upload(&self, receipt: UploadReceipt) {
        let mut uploads = self.uploads();
        self.drop_upload_locked(&mut uploads, &receipt.upload_id);
        let mut receipts = self.receipts.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = receipt.created_at;
        receipts.retain(|kept| !kept.expired(now) && kept.upload_id != receipt.upload_id);
        while receipts.len() >= MAX_UPLOAD_RECEIPTS {
            receipts.pop_front();
        }
        receipts.push_back(receipt);
    }

    pub fn upload_receipt(&self, upload_id: &str, now: u64) -> Option<UploadReceipt> {
        let receipts = self.receipts.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        receipts.iter().find(|receipt| receipt.upload_id == upload_id && !receipt.expired(now)).cloned()
    }
}

/// What an upload request writes to.
enum UploadTarget<'a> {
    Design { id: &'a str, name: &'a str, budget: usize },
    Token(&'a str),
}

impl UploadTarget<'_> {
    fn key(&self) -> String {
        match self {
            UploadTarget::Design { id, name, .. } => format!("design:{id}/{name}"),
            UploadTarget::Token(token) => format!("token:{token}"),
        }
    }
}

/// Why finishing an upload failed.
enum FinishError {
    Asset(AssetError),
    Queue(crate::QueueRejection),
}

impl FinishError {
    /// Worth sending the last chunk again (the server was busy), so its
    /// session is kept.
    fn retryable(&self) -> bool {
        match self {
            FinishError::Queue(_) => true,
            FinishError::Asset(error) => error.status == 429 || error.status >= 500,
        }
    }
}

/// Processes and stores a complete file: what a single PUT does after its
/// body arrived. Holds a render-queue permit while processing. A one-time URL
/// is consumed only by a stored file.
fn finish_upload(state: &AppState, target: &UploadTarget<'_>, bytes: &[u8]) -> Result<Value, FinishError> {
    match *target {
        UploadTarget::Design { id, name, budget } => {
            if !valid_asset_name(name) {
                return Err(FinishError::Asset(AssetError::new(400, format!("Invalid asset name {name:?}. {ASSET_NAME_RULE}"))));
            }
            let _permit = state.render_queue.acquire(current_client_key(), None, None).map_err(FinishError::Queue)?;
            store_upload(state, id, name, bytes, budget, true).map_err(FinishError::Asset)
        }
        UploadTarget::Token(token) => {
            match state.workspaces.peek_upload_token(token) {
                Ok(Some(_)) => {}
                Ok(None) => return Err(FinishError::Asset(AssetError::new(404, UNKNOWN_UPLOAD_URL))),
                Err(error) => return Err(FinishError::Asset(AssetError::internal(error))),
            }
            let _permit = state.render_queue.acquire(current_client_key(), None, None).map_err(FinishError::Queue)?;
            let pending = match state.workspaces.take_upload_token(token) {
                Ok(Some(pending)) => pending,
                Ok(None) => return Err(FinishError::Asset(AssetError::new(404, UNKNOWN_UPLOAD_URL))),
                Err(error) => return Err(FinishError::Asset(AssetError::internal(error))),
            };
            store_upload(state, &pending.design_id, &pending.name, bytes, pending.budget, false).map_err(|error| {
                state.workspaces.restore_upload_token(token, pending);
                FinishError::Asset(error)
            })
        }
    }
}

fn respond_finish(stream: &mut TcpStream, result: Result<Value, FinishError>) -> io::Result<()> {
    match result {
        Ok(value) => respond_value(stream, 200, &value),
        Err(FinishError::Asset(error)) => respond_asset_error(stream, error),
        Err(FinishError::Queue(rejection)) => respond_queue_rejection(stream, rejection),
    }
}

/// Receives an upload PUT: a whole file, or one chunk of a chunked upload.
fn http_receive(stream: &mut TcpStream, state: &AppState, request: &Request, target: UploadTarget<'_>) -> io::Result<()> {
    let range = match upload_range(&request.headers, request.body.len()) {
        Ok(range) => range,
        Err(error) => return respond_asset_error(stream, error),
    };
    match range {
        Some(range) if !range.is_whole() || request_upload_id(request).is_some() => {
            http_chunk(stream, state, request, range, target)
        }
        _ => respond_finish(stream, finish_upload(state, &target, &request.body)),
    }
}

fn http_chunk(stream: &mut TcpStream, state: &AppState, request: &Request, range: ContentRange, target: UploadTarget<'_>) -> io::Result<()> {
    let now = unix_millis();
    let key = target.key();
    let session = match request_upload_id(request) {
        None if range.start != 0 => {
            return respond_error(
                stream,
                400,
                "Send the Upload-Id header the first chunk's reply returned (uploadId) with every later chunk.",
            );
        }
        None => {
            let (design_id, name, budget) = match target {
                UploadTarget::Design { id, name, budget } => (id.to_string(), name.to_string(), budget),
                UploadTarget::Token(token) => match state.workspaces.peek_upload_token(token) {
                    Ok(Some(pending)) => (pending.design_id, pending.name, pending.budget),
                    Ok(None) => return respond_error(stream, 404, UNKNOWN_UPLOAD_URL),
                    Err(error) => return respond_error(stream, 500, &error),
                },
            };
            if !valid_asset_name(&name) {
                return respond_error(stream, 400, &format!("Invalid asset name {name:?}. {ASSET_NAME_RULE}"));
            }
            if let Err(error) = design_exists(state, &design_id).and_then(|()| admit_new_upload(state, &design_id)) {
                return respond_asset_error(stream, error);
            }
            let upload_id = match mint_token() {
                Ok(upload_id) => upload_id,
                Err(error) => return respond_error(stream, 500, &error),
            };
            let session = UploadSession {
                upload_id,
                target: key.clone(),
                design_id,
                name,
                budget,
                total: range.total,
                received: 0,
                client: current_ip().map(|address| address.to_string()).unwrap_or_default(),
                updated_at: now,
            };
            if let Err(error) = state.workspaces.upload_session_open(&session, now) {
                return respond_asset_error(stream, error);
            }
            session
        }
        Some(upload_id) => match state.workspaces.upload_session(&upload_id, now) {
            Ok(Some(session)) if session.target == key => session,
            Ok(Some(_)) => return respond_error(stream, 404, UNKNOWN_UPLOAD_SESSION),
            // Finished already: the same last chunk again (its reply was
            // lost) gets the same reply.
            Ok(None) => return respond_receipt_or_unknown(stream, state, &upload_id, &key, &range, now),
            Err(error) => return respond_error(stream, 500, &error),
        },
    };
    if session.total != range.total {
        let mut value = session.progress();
        value["ok"] = json!(false);
        value["error"] = json!(format!(
            "This upload was started for {} bytes; every chunk's Content-Range must end in /{}.",
            session.total, session.total
        ));
        return respond_value(stream, 409, &value);
    }
    let received = match state.workspaces.upload_session_append(&session.upload_id, range.start, &request.body, now) {
        Ok(received) => received,
        Err(error) if error.status == 409 => {
            let current = state.workspaces.upload_session(&session.upload_id, now).ok().flatten().unwrap_or(session);
            let mut value = current.progress();
            value["ok"] = json!(false);
            value["error"] = json!(error.message);
            return respond_value(stream, 409, &value);
        }
        Err(error) if error.status == 404 => {
            return respond_receipt_or_unknown(stream, state, &session.upload_id, &key, &range, now);
        }
        Err(error) => return respond_asset_error(stream, error),
    };
    if let UploadTarget::Token(token) = target {
        // The URL stays valid for as long as its upload is alive.
        state.workspaces.extend_upload_token(token, now + UPLOAD_SESSION_IDLE_MS);
    }
    if received < session.total {
        let mut value = UploadSession { received, ..session }.progress();
        value["ok"] = json!(true);
        return respond_value(stream, 202, &value);
    }
    // One request processes the file; a concurrent re-send of the last chunk
    // (on this or another instance) is told to come back for the result.
    match state.workspaces.upload_session_claim_finish(&session.upload_id, now) {
        Ok(true) => {}
        Ok(false) => return respond_finishing(stream, &session),
        Err(error) if error == UNKNOWN_UPLOAD_SESSION => {
            // Finished between the append and the claim.
            return respond_receipt_or_unknown(stream, state, &session.upload_id, &key, &range, now);
        }
        Err(error) => return respond_error(stream, 500, &error),
    }
    let bytes = match state.workspaces.upload_session_bytes(&session.upload_id) {
        Ok(bytes) => bytes,
        Err(error) => {
            state.workspaces.upload_session_release_finish(&session.upload_id);
            return respond_error(stream, 500, &error);
        }
    };
    let finish_target = match target {
        UploadTarget::Design { .. } => {
            UploadTarget::Design { id: &session.design_id, name: &session.name, budget: session.budget }
        }
        token => token,
    };
    let result = finish_upload(state, &finish_target, &bytes);
    drop(bytes);
    match &result {
        Ok(value) => state.workspaces.upload_session_complete(UploadReceipt {
            upload_id: session.upload_id.clone(),
            target: key,
            total: session.total,
            final_start: range.start,
            response: value.clone(),
            created_at: unix_millis(),
        }),
        Err(error) if error.retryable() => state.workspaces.upload_session_release_finish(&session.upload_id),
        Err(_) => state.workspaces.upload_session_drop(&session.upload_id),
    }
    respond_finish(stream, result)
}

/// A chunk for a session that is gone: the stored reply when it repeats the
/// last chunk of a finished upload, 404 otherwise.
fn respond_receipt_or_unknown(
    stream: &mut TcpStream,
    state: &AppState,
    upload_id: &str,
    key: &str,
    range: &ContentRange,
    now: u64,
) -> io::Result<()> {
    match state.workspaces.upload_receipt(upload_id, now) {
        Ok(Some(receipt)) if receipt.replays(key, range) => {
            // Replayed as given, but for how much time is left now.
            let mut response = receipt.response;
            if let Some(expires_at) = response.get("expiresAt").and_then(Value::as_u64) {
                let now = unix_millis();
                set_expiry_fields(&mut response, expires_at, now);
                response["expiry"] = json!(upload_expiry_line(expires_at, now));
            }
            respond_value(stream, 200, &response)
        }
        Ok(_) => respond_error(stream, 404, UNKNOWN_UPLOAD_SESSION),
        Err(error) => respond_error(stream, 500, &error),
    }
}

/// 503 for a last chunk whose file another request is processing; the
/// client sends it again shortly and gets the stored reply.
fn respond_finishing(stream: &mut TcpStream, session: &UploadSession) -> io::Result<()> {
    let mut value = session.progress();
    value["ok"] = json!(false);
    value["error"] = json!(UPLOAD_FINISHING);
    value["reason"] = json!("upload_finishing");
    value["retryable"] = json!(true);
    value["retryAfterSeconds"] = json!(2);
    crate::respond_text(
        stream,
        503,
        "application/json; charset=utf-8",
        value.to_string().as_bytes(),
        &[("Retry-After", "2")],
    )
}

// ---------------------------------------------------------------------------
// Per-client storage quota.
// ---------------------------------------------------------------------------

type QuotaMap = Mutex<Option<HashMap<IpAddr, (u64, u64)>>>;

/// Stored bytes added per client and hour (what the quota counts).
static QUOTA: QuotaMap = Mutex::new(None);
/// Raw upload body bytes per client and hour.
static RAW_QUOTA: QuotaMap = Mutex::new(None);

/// Raw upload bytes one client may send per hour.
pub fn client_raw_upload_bytes() -> u64 {
    std::env::var("REOPENSCAD_UPLOAD_RAW_MIB_PER_HOUR")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|mib| *mib >= 1)
        .map(|mib| mib.saturating_mul(1024 * 1024))
        .unwrap_or(DEFAULT_CLIENT_RAW_UPLOAD_BYTES)
}

/// Admits and bills `bytes` of raw upload body for `client` (before the body
/// is read), under a one-hour window of `limit` bytes.
pub fn raw_upload_admit(client: IpAddr, bytes: u64, now: u64, limit: u64) -> Result<(), AssetError> {
    window_admit(&RAW_QUOTA, client, bytes, now, limit, true).map_err(|minutes| {
        AssetError::new(
            429,
            format!(
                "This would upload more than {} MiB of raw files from your address this hour. Try again in about {minutes} minutes.",
                limit / (1024 * 1024)
            ),
        )
    })
}

/// Stored asset bytes one client may add per hour.
pub fn client_quota_bytes() -> u64 {
    std::env::var("REOPENSCAD_ASSET_QUOTA_MIB_PER_HOUR")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|mib| *mib >= 1)
        .map(|mib| mib.saturating_mul(1024 * 1024))
        .unwrap_or(DEFAULT_CLIENT_QUOTA_BYTES)
}

/// Admits (and with `charge`, bills) `bytes` of new storage for `client`
/// under a fixed one-hour window of `limit` bytes.
pub fn quota_admit(client: IpAddr, bytes: u64, now: u64, limit: u64, charge: bool) -> Result<(), AssetError> {
    window_admit(&QUOTA, client, bytes, now, limit, charge).map_err(|minutes| {
        AssetError::new(
            429,
            format!(
                "This would store more than {} MiB of assets from your address this hour. Try again in about {minutes} minutes, or use a smaller triangle budget.",
                limit / (1024 * 1024)
            ),
        )
    })
}

/// One fixed-window counter: `Err(minutes until the window resets)` when
/// `bytes` more would exceed `limit`.
fn window_admit(quota: &QuotaMap, client: IpAddr, bytes: u64, now: u64, limit: u64, charge: bool) -> Result<(), u64> {
    let mut guard = quota.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    let used = match map.get(&client) {
        Some((start, used)) if now.saturating_sub(*start) < QUOTA_WINDOW_MS => *used,
        _ => 0,
    };
    if used.saturating_add(bytes) > limit {
        let start = map.get(&client).map_or(now, |(start, _)| *start);
        return Err((start + QUOTA_WINDOW_MS).saturating_sub(now).div_ceil(60_000).max(1));
    }
    if charge {
        if !map.contains_key(&client) && map.len() >= MAX_QUOTA_CLIENTS {
            map.retain(|_, (start, _)| now.saturating_sub(*start) < QUOTA_WINDOW_MS);
            if map.len() >= MAX_QUOTA_CLIENTS {
                if let Some(oldest) = map.iter().min_by_key(|(_, (start, _))| *start).map(|(address, _)| *address) {
                    map.remove(&oldest);
                }
            }
        }
        let entry = map.entry(client).or_insert((now, 0));
        if now.saturating_sub(entry.0) >= QUOTA_WINDOW_MS {
            *entry = (now, 0);
        }
        entry.1 = entry.1.saturating_add(bytes);
    }
    Ok(())
}

fn current_ip() -> Option<IpAddr> {
    match current_client_key() {
        ClientKey::Ip(address) => Some(address),
        _ => None,
    }
}

fn quota_check(bytes: u64) -> Result<(), AssetError> {
    match current_ip() {
        Some(client) => quota_admit(client, bytes, unix_millis(), client_quota_bytes(), false),
        None => Ok(()),
    }
}

/// Bytes of the caller's hourly storage quota taken up front (check and
/// charge in one locked step, so concurrent writes cannot both pass the
/// check and overshoot the limit together). Dropped without
/// [`QuotaReservation::commit`], it gives the bytes back.
#[must_use]
pub(crate) struct QuotaReservation {
    client: Option<IpAddr>,
    bytes: u64,
    window_start: u64,
    committed: bool,
}

impl QuotaReservation {
    pub(crate) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for QuotaReservation {
    fn drop(&mut self) {
        if self.committed || self.bytes == 0 {
            return;
        }
        if let Some(client) = self.client {
            window_refund(&QUOTA, client, self.bytes, self.window_start);
        }
    }
}

/// Gives back `bytes` charged in the window that started at `window_start`
/// (a later window owes nothing).
fn window_refund(quota: &QuotaMap, client: IpAddr, bytes: u64, window_start: u64) {
    let mut guard = quota.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(entry) = guard.as_mut().and_then(|map| map.get_mut(&client)) {
        if entry.0 == window_start {
            entry.1 = entry.1.saturating_sub(bytes);
        }
    }
}

fn quota_reserve(bytes: u64) -> Result<QuotaReservation, AssetError> {
    let Some(client) = current_ip() else {
        return Ok(QuotaReservation { client: None, bytes, window_start: 0, committed: false });
    };
    let now = unix_millis();
    quota_admit(client, bytes, now, client_quota_bytes(), true)?;
    let window_start = QUOTA
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .and_then(|map| map.get(&client).map(|entry| entry.0))
        .unwrap_or(now);
    Ok(QuotaReservation { client: Some(client), bytes, window_start, committed: false })
}

// ---------------------------------------------------------------------------
// Store dispatch.
// ---------------------------------------------------------------------------

impl Store {
    /// Design `id`'s assets, by name. Empty for a design without any.
    pub(crate) fn asset_list(&self, id: &str) -> Result<Vec<AssetMeta>, String> {
        Ok(self.asset_snapshot(id)?.0)
    }

    /// Design `id`'s assets and assets version, read consistently.
    pub(crate) fn asset_snapshot(&self, id: &str) -> Result<(Vec<AssetMeta>, u64), String> {
        match self {
            Store::Filesystem(store) => Ok(store.assets.snapshot(id)),
            #[cfg(feature = "postgres")]
            Store::Postgres(store) => store.asset_snapshot(id),
        }
    }

    /// A handle lazy asset sets read through.
    pub(crate) fn asset_reader(&self) -> AssetReader {
        match self {
            Store::Filesystem(store) => AssetReader::Filesystem(Arc::clone(&store.assets)),
            #[cfg(feature = "postgres")]
            Store::Postgres(store) => AssetReader::Postgres(store.clone()),
        }
    }

    /// The stored bytes of one asset, while its hash is still `meta.sha256`.
    pub(crate) fn asset_blob(&self, id: &str, meta: &AssetMeta) -> Result<Option<Vec<u8>>, String> {
        self.asset_reader().blob(id, &meta.name, &meta.sha256)
    }

    /// Bumped by every asset write of design `id`; watched by `/events`.
    pub(crate) fn assets_version(&self, id: &str) -> Result<u64, String> {
        match self {
            Store::Filesystem(store) => Ok(store.assets.version(id)),
            #[cfg(feature = "postgres")]
            Store::Postgres(store) => store.assets_version(id),
        }
    }

    /// Deletes expired assets across the deployment, keeping a record of their
    /// names. Safe from any number of instances at
    /// once. Returns each design touched with what went from it.
    pub(crate) fn sweep_expired_assets(&self, now: u64) -> Result<Vec<(String, Vec<String>)>, String> {
        match self {
            Store::Filesystem(store) => Ok(store.assets.sweep_expired(now)),
            #[cfg(feature = "postgres")]
            Store::Postgres(store) => store.sweep_expired_assets(now),
        }
    }

    pub(crate) fn asset_put(&self, id: &str, processed: &Processed) -> Result<u64, AssetError> {
        let max_total = max_total_asset_bytes();
        let (version, evicted) = match self {
            Store::Filesystem(store) => {
                let lru = store.ids_least_recent_first();
                let (version, evicted) = store.assets.put(id, processed, max_total, &lru)?;
                // An eviction that raced the write leaves no orphan behind.
                if store.get(id).ok().flatten().is_none() {
                    store.assets.remove_design(id);
                    return Err(AssetError::new(404, "Design not found."));
                }
                store.touch(id);
                (version, evicted)
            }
            #[cfg(feature = "postgres")]
            Store::Postgres(store) => store.asset_put(id, processed, max_total)?,
        };
        for victim in &evicted {
            eprintln!("asset storage full: evicted the assets of design {victim}");
            after_write(victim);
        }
        Ok(version)
    }

    pub(crate) fn asset_delete(&self, id: &str, name: &str) -> Result<Option<u64>, String> {
        match self {
            Store::Filesystem(store) => {
                let deleted = store.assets.delete(id, name)?;
                if deleted.is_some() {
                    store.touch(id);
                }
                Ok(deleted)
            }
            #[cfg(feature = "postgres")]
            Store::Postgres(store) => store.asset_delete(id, name),
        }
    }

    pub(crate) fn create_upload_token(&self, token: &str, pending: PendingUpload) -> Result<(), String> {
        match self {
            Store::Filesystem(_) => remember_upload(token, pending, unix_millis()),
            #[cfg(feature = "postgres")]
            Store::Postgres(store) => store.create_upload_token(token, &pending),
        }
    }

    pub(crate) fn peek_upload_token(&self, token: &str) -> Result<Option<PendingUpload>, String> {
        if !valid_token(token) {
            return Ok(None);
        }
        match self {
            Store::Filesystem(_) => Ok(peek_upload(token, unix_millis())),
            #[cfg(feature = "postgres")]
            Store::Postgres(store) => store.peek_upload_token(token),
        }
    }

    pub(crate) fn take_upload_token(&self, token: &str) -> Result<Option<PendingUpload>, String> {
        if !valid_token(token) {
            return Ok(None);
        }
        match self {
            Store::Filesystem(_) => Ok(take_upload(token, unix_millis())),
            #[cfg(feature = "postgres")]
            Store::Postgres(store) => store.take_upload_token(token),
        }
    }

    /// Keeps a one-time URL valid until at least `expires_at` (its chunked
    /// upload is still arriving).
    pub(crate) fn extend_upload_token(&self, token: &str, expires_at: u64) {
        match self {
            Store::Filesystem(_) => extend_upload(token, expires_at),
            #[cfg(feature = "postgres")]
            Store::Postgres(store) => {
                if let Err(error) = store.extend_upload_token(token, expires_at) {
                    eprintln!("an upload URL could not be extended: {error}");
                }
            }
        }
    }

    pub(crate) fn upload_session(&self, upload_id: &str, now: u64) -> Result<Option<UploadSession>, String> {
        if !valid_token(upload_id) {
            return Ok(None);
        }
        match self {
            Store::Filesystem(store) => Ok(store.assets.upload_session(upload_id, now)),
            #[cfg(feature = "postgres")]
            Store::Postgres(store) => store.upload_session(upload_id, now),
        }
    }

    pub(crate) fn upload_session_open(&self, session: &UploadSession, now: u64) -> Result<(), AssetError> {
        match self {
            Store::Filesystem(store) => store.assets.open_upload(session, now),
            #[cfg(feature = "postgres")]
            Store::Postgres(store) => store.open_upload_session(session, now),
        }
    }

    pub(crate) fn upload_session_append(&self, upload_id: &str, start: u64, bytes: &[u8], now: u64) -> Result<u64, AssetError> {
        match self {
            Store::Filesystem(store) => store.assets.append_upload(upload_id, start, bytes, now),
            #[cfg(feature = "postgres")]
            Store::Postgres(store) => store.append_upload_chunk(upload_id, start, bytes, now),
        }
    }

    pub(crate) fn upload_session_bytes(&self, upload_id: &str) -> Result<Vec<u8>, String> {
        match self {
            Store::Filesystem(store) => store.assets.upload_bytes(upload_id),
            #[cfg(feature = "postgres")]
            Store::Postgres(store) => store.upload_session_bytes(upload_id),
        }
    }

    pub(crate) fn upload_session_drop(&self, upload_id: &str) {
        match self {
            Store::Filesystem(store) => store.assets.drop_upload(upload_id),
            #[cfg(feature = "postgres")]
            Store::Postgres(store) => {
                if let Err(error) = store.drop_upload_session(upload_id) {
                    eprintln!("a finished upload session could not be dropped: {error}");
                }
            }
        }
    }

    pub(crate) fn upload_session_claim_finish(&self, upload_id: &str, now: u64) -> Result<bool, String> {
        match self {
            Store::Filesystem(store) => store.assets.claim_finish(upload_id, now),
            #[cfg(feature = "postgres")]
            Store::Postgres(store) => store.claim_upload_finish(upload_id, now),
        }
    }

    pub(crate) fn upload_session_release_finish(&self, upload_id: &str) {
        match self {
            Store::Filesystem(store) => store.assets.release_finish(upload_id),
            #[cfg(feature = "postgres")]
            Store::Postgres(store) => {
                if let Err(error) = store.release_upload_finish(upload_id) {
                    eprintln!("an upload's finish claim could not be released: {error}");
                }
            }
        }
    }

    pub(crate) fn upload_session_complete(&self, receipt: UploadReceipt) {
        match self {
            Store::Filesystem(store) => store.assets.complete_upload(receipt),
            #[cfg(feature = "postgres")]
            Store::Postgres(store) => {
                let upload_id = receipt.upload_id.clone();
                if let Err(error) = store.complete_upload_session(&receipt) {
                    eprintln!("a finished upload could not be recorded: {error}");
                    let _ = store.drop_upload_session(&upload_id);
                }
            }
        }
    }

    pub(crate) fn upload_receipt(&self, upload_id: &str, now: u64) -> Result<Option<UploadReceipt>, String> {
        if !valid_token(upload_id) {
            return Ok(None);
        }
        match self {
            Store::Filesystem(store) => Ok(store.assets.upload_receipt(upload_id, now)),
            #[cfg(feature = "postgres")]
            Store::Postgres(store) => store.upload_receipt(upload_id, now),
        }
    }

    pub(crate) fn restore_upload_token(&self, token: &str, pending: PendingUpload) {
        match self {
            Store::Filesystem(_) => restore_upload(token, pending),
            #[cfg(feature = "postgres")]
            Store::Postgres(store) => {
                if let Err(error) = store.restore_upload_token(token, &pending) {
                    eprintln!("an upload URL could not be restored: {error}");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Operations shared by HTTP and MCP.
// ---------------------------------------------------------------------------

pub(crate) fn design_exists(state: &AppState, id: &str) -> Result<(), AssetError> {
    match state.workspaces.get(id) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(AssetError::new(404, "Design not found.")),
        Err(error) => Err(AssetError::internal(error)),
    }
}

/// Refuses a new upload to a design that already holds a live one. Checked
/// when an upload is asked for and again before its file is processed; the
/// store's own check under its lock ([`settle_put`]) is what settles a race.
pub(crate) fn admit_new_upload(state: &AppState, id: &str) -> Result<(), AssetError> {
    let now = unix_millis();
    let metas = state.workspaces.asset_list(id).map_err(AssetError::internal)?;
    match live_upload(&metas, now) {
        Some(held) => Err(second_upload_refusal(held, now)),
        None => Ok(()),
    }
}

/// One background pass: deletes expired assets, and any full-size originals
/// an earlier version left on this instance's disk.
pub(crate) fn sweep_expired(state: &AppState, now: u64) -> Result<Vec<(String, Vec<String>)>, String> {
    remove_legacy_originals(&legacy_originals_root(state));
    let swept = state.workspaces.sweep_expired_assets(now)?;
    for (id, _) in &swept {
        after_write(id);
    }
    Ok(swept)
}

/// Where earlier versions kept the file as sent of each decimated upload.
fn legacy_originals_root(state: &AppState) -> PathBuf {
    match &state.workspaces {
        Store::Filesystem(store) => store.root.join("originals"),
        #[cfg(feature = "postgres")]
        Store::Postgres(_) => std::env::var_os("REOPENSCAD_ORIGINALS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("reopenscad-originals")),
    }
}

/// Deletes the files earlier versions wrote under `root` and nothing else
/// (`<design>/<name>.<sha>.orig|json|tmp<pid>`): the directory may be one an
/// operator named. Nothing writes here any more, so after the first pass it
/// costs one failed read_dir.
fn remove_legacy_originals(root: &std::path::Path) {
    let Ok(designs) = fs::read_dir(root) else { return };
    for design in designs.flatten().filter(|entry| entry.file_name().to_str().is_some_and(valid_workspace_id)) {
        let dir = design.path();
        for file in fs::read_dir(&dir).into_iter().flatten().flatten().map(|entry| entry.path()) {
            let extension = file.extension().and_then(|e| e.to_str()).unwrap_or_default();
            if extension == "orig" || extension == "json" || extension.starts_with("tmp") {
                let _ = fs::remove_file(file);
            }
        }
        let _ = fs::remove_dir(&dir);
    }
    let _ = fs::remove_dir(root);
}

/// Frees the space of expired assets every few minutes. Reads never depend on
/// it: an expired asset is gone from every read the moment it expires.
pub(crate) fn start_expiry_sweeper(state: Arc<AppState>) {
    let spawned = std::thread::Builder::new().name("asset-expiry-sweep".into()).spawn(move || loop {
        match sweep_expired(&state, unix_millis()) {
            Ok(swept) => {
                for (id, names) in swept {
                    println!("expired assets deleted from design {id}: {}", names.join(", "));
                }
            }
            Err(error) => eprintln!("the asset expiry sweep failed: {error}"),
        }
        std::thread::sleep(EXPIRY_SWEEP_INTERVAL);
    });
    if let Err(error) = spawned {
        eprintln!("the asset expiry sweep could not start: {error}");
    }
}

fn after_write(id: &str) {
    // Decoded meshes are keyed by content hash and object summaries by the
    // asset digest, so neither can serve stale geometry; this only frees the
    // superseded summaries early.
    crate::agent::forget_objects(id);
}

/// Bills an upload's request bytes to the caller's byte bucket.
fn charge_upload_bytes(state: &AppState, bytes: usize) {
    if let ClientKey::Ip(address) = current_client_key() {
        state.rate_limiter.charge_bytes(address, bytes as u64);
    }
}

/// Validates, processes and stores one upload. The caller holds a
/// render-queue permit (processing is CPU-heavy). `include_id` is false for a
/// one-time upload URL: whoever holds one may add this one file, and must not
/// learn the design id (a permanent edit capability) from the reply.
pub(crate) fn store_upload(
    state: &AppState,
    id: &str,
    name: &str,
    bytes: &[u8],
    budget: usize,
    include_id: bool,
) -> Result<Value, AssetError> {
    if !valid_asset_name(name) {
        return Err(AssetError::new(400, format!("Invalid asset name {name:?}. {ASSET_NAME_RULE}")));
    }
    design_exists(state, id)?;
    // A second STL, or a client already at its hourly quota, fails before
    // the costly processing.
    admit_new_upload(state, id)?;
    quota_check(1)?;
    let mut processed = process_upload(name, bytes, budget, unix_millis()).map_err(|error| AssetError::new(400, error))?;
    // The clock starts when the upload completes, not when cleaning began.
    let now = unix_millis();
    processed.meta.created_at = now;
    processed.meta.expires_at = now + ASSET_TTL_MS;
    let reservation = quota_reserve(processed.meta.bytes)?;
    let version = state.workspaces.asset_put(id, &processed)?;
    reservation.commit();
    after_write(id);
    let mut value = processed.meta.public();
    value["ok"] = json!(true);
    if include_id {
        value["designId"] = json!(id);
    }
    value["assetsVersion"] = json!(version);
    value["import"] = json!(format!("import(\"{name}\");"));
    value["expiry"] = json!(upload_expiry_line(processed.meta.expires_at, now));
    value["guide"] = json!(GUIDE_TOOL);
    Ok(value)
}

pub(crate) fn list_value(state: &AppState, id: &str, include_id: bool) -> Result<Value, AssetError> {
    // One snapshot: the list and the version always describe the same state.
    let (metas, version) = state.workspaces.asset_snapshot(id).map_err(AssetError::internal)?;
    let server_time = unix_millis();
    let used: u64 = metas.iter().map(|meta| meta.bytes).sum();
    let mut value = json!({
        "ok": true,
        // So a client counts down on this clock, not its own.
        "serverTime": server_time,
        "assets": metas.iter().map(AssetMeta::public).collect::<Vec<_>>(),
        "assetsVersion": version,
        "limits": {
            "maxAssets": 1,
            "usedBytes": used,
            "maxUploadBytes": meshimport::MAX_UPLOAD_BYTES,
            "defaultTriangleBudget": meshimport::DEFAULT_TRIANGLE_BUDGET,
            "maxTriangleBudget": meshimport::MAX_TRIANGLE_BUDGET,
        },
    });
    let (_, expired) = state.workspaces.asset_reader().index(id).map_err(AssetError::internal)?;
    if !expired.is_empty() {
        value["expired"] = json!(expired.iter().map(|(name, at)| json!({"name": name, "expiredAt": at})).collect::<Vec<_>>());
    }
    if include_id {
        value["designId"] = json!(id);
    }
    Ok(value)
}

pub(crate) fn delete_value(state: &AppState, id: &str, name: &str) -> Result<Value, AssetError> {
    if !valid_asset_name(name) {
        return Err(AssetError::new(400, format!("Invalid asset name {name:?}. {ASSET_NAME_RULE}")));
    }
    design_exists(state, id)?;
    match state.workspaces.asset_delete(id, name).map_err(AssetError::internal)? {
        Some(version) => {
            after_write(id);
            let mut value = json!({"ok": true, "designId": id, "deleted": name, "assetsVersion": version});
            let code = state.workspaces.get(id).ok().flatten().map(|workspace| workspace.code).unwrap_or_default();
            if names_file(&string_literals(&code), name) {
                value["note"] = json!(format!("The code still imports {name}; remove that import."));
            }
            Ok(value)
        }
        None => Err(no_such_asset(state, id, name)),
    }
}

/// The 404 for asset `name`, saying so when it expired. Never names the design.
fn no_such_asset(state: &AppState, id: &str, name: &str) -> AssetError {
    let expired = state.workspaces.asset_reader().index(id).ok().and_then(|(_, expired)| expired.into_iter().find(|(other, _)| other == name));
    match expired {
        Some((_, at)) => AssetError::new(404, format!("{name} expired at {} and is deleted; there is nothing left to remove. Remove its import from the code.", iso_utc(at))),
        None => AssetError::new(404, format!("This design has no asset named {name:?}.")),
    }
}

/// The contents of every string literal of SCAD `code`, comments skipped: an
/// import commented out does not count, and one through a variable does.
pub(crate) fn string_literals(code: &str) -> Vec<String> {
    let bytes = code.as_bytes();
    let (mut literals, mut at) = (Vec::new(), 0);
    while at < bytes.len() {
        match bytes[at] {
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at += 2;
                while at < bytes.len() && !(bytes[at] == b'*' && bytes.get(at + 1) == Some(&b'/')) {
                    at += 1;
                }
                at += 2;
            }
            b'"' => {
                at += 1;
                let start = at;
                while at < bytes.len() && bytes[at] != b'"' {
                    at += if bytes[at] == b'\\' { 2 } else { 1 };
                }
                literals.push(String::from_utf8_lossy(&bytes[start..at.min(bytes.len())]).into_owned());
                at += 1;
            }
            _ => at += 1,
        }
    }
    literals
}

fn names_file(literals: &[String], name: &str) -> bool {
    literals.iter().any(|literal| literal.strip_prefix("./").unwrap_or(literal) == name)
}

/// The note every render of a design importing a live STL carries: which
/// ones, the time left, and the nudge. `None` when it imports none.
pub(crate) fn live_stl_note(state: &AppState, id: &str) -> Option<Value> {
    let metas = state.workspaces.asset_list(id).ok()?;
    if metas.is_empty() {
        return None;
    }
    let code = state.workspaces.get(id).ok()??.code;
    let literals = string_literals(&code);
    let imported: Vec<&AssetMeta> = metas.iter().filter(|meta| names_file(&literals, &meta.name)).collect();
    let expires_at = imported.iter().map(|meta| meta.expiry()).min()?;
    let now = unix_millis();
    let files: Vec<&str> = imported.iter().map(|meta| meta.name.as_str()).collect();
    let mut note = expiry_fields(expires_at, now);
    note["files"] = json!(files);
    note["guide"] = json!(GUIDE_TOOL);
    note["text"] = json!(format!(
        "{} is a temporary STL: it expires in {} (at {}), and this design stops rendering if it still imports it then. Reverse-engineer \
         it into a real SCAD part and remove the import and the STL, so the design stands on its own. {GUIDE_TOOL} shows how.",
        files.join(", "),
        time_left(expires_at.saturating_sub(now)),
        iso_utc(expires_at),
    ));
    Some(note)
}

/// One asset's metadata and bytes, consistent with each other: the bytes are
/// read by the hash the metadata names, and a replacement between the two
/// reads is retried rather than served under the wrong hash.
fn find_asset(state: &AppState, id: &str, name: &str) -> Result<(AssetMeta, Vec<u8>), AssetError> {
    for _ in 0..3 {
        let metas = state.workspaces.asset_list(id).map_err(AssetError::internal)?;
        let meta = metas
            .into_iter()
            .find(|meta| meta.name == name)
            .ok_or_else(|| no_such_asset(state, id, name))?;
        if let Some(bytes) = state.workspaces.asset_blob(id, &meta).map_err(AssetError::internal)? {
            return Ok((meta, bytes));
        }
    }
    Err(AssetError::internal(format!("Asset {name:?} could not be read from storage; try again.")))
}

/// Whether the caller's storage quota can pay for copying design `from`'s
/// assets, checked before the copy is created.
pub(crate) fn copy_quota_check(state: &AppState, from: &str) -> Result<(), AssetError> {
    let (metas, _) = state.workspaces.asset_snapshot(from).map_err(AssetError::internal)?;
    quota_check(metas.iter().map(|meta| meta.bytes).sum())
}

/// Copies every asset of design `from` into design `to` (a fresh copy made
/// from a read-only link), billed to the caller's storage quota. The caller
/// holds a render-queue permit.
pub(crate) fn copy_assets(state: &AppState, from: &str, to: &str) -> Result<usize, AssetError> {
    let (metas, _) = state.workspaces.asset_snapshot(from).map_err(AssetError::internal)?;
    if metas.is_empty() {
        return Ok(0);
    }
    let bytes: u64 = metas.iter().map(|meta| meta.bytes).sum();
    let reservation = quota_reserve(bytes)?;
    let now = unix_millis();
    for meta in &metas {
        let (meta, stl) = find_asset(state, from, &meta.name)?;
        if hex(&rendercache::sha256(&stl)) != meta.sha256 {
            return Err(AssetError::internal(format!("Asset {:?} is corrupt in storage.", meta.name)));
        }
        // The copy keeps the original's clock: copying renews nothing.
        let expires_at = meta.expiry();
        let processed = Processed { meta: AssetMeta { created_at: now, expires_at, ..meta }, stl, kind: AssetKind::Copy };
        state.workspaces.asset_put(to, &processed)?;
    }
    reservation.commit();
    after_write(to);
    Ok(metas.len())
}

// ---------------------------------------------------------------------------
// HTTP.
// ---------------------------------------------------------------------------

fn respond_asset_error(stream: &mut TcpStream, error: AssetError) -> io::Result<()> {
    respond_error(stream, error.status, &error.message)
}

fn respond_stl(stream: &mut TcpStream, meta: &AssetMeta, bytes: &[u8]) -> io::Result<()> {
    let disposition = format!("attachment; filename=\"{}\"", meta.name);
    let etag = format!("\"{}\"", meta.sha256);
    respond_text(
        stream,
        200,
        "model/stl",
        bytes,
        &[("Content-Disposition", disposition.as_str()), ("ETag", etag.as_str()), ("Cache-Control", "private, no-cache")],
    )
}

fn query_budget(request: &Request) -> usize {
    clamp_budget(crate::query_u64(&crate::query_parameters(&request.query), "budget"))
}

/// `/api/designs/{id}/assets[/{name}]`; `rest` is what follows `assets`.
pub(crate) fn handle_route(stream: &mut TcpStream, state: &AppState, request: &Request, id: &str, rest: &str) -> io::Result<()> {
    let name = rest.trim_start_matches('/');
    match (request.method.as_str(), name.is_empty()) {
        ("GET", true) => match design_exists(state, id).and_then(|()| list_value(state, id, true)) {
            Ok(value) => respond_value(stream, 200, &value),
            Err(error) => respond_asset_error(stream, error),
        },
        ("GET", false) => match design_exists(state, id).and_then(|()| find_asset(state, id, name)) {
            Ok((meta, bytes)) => respond_stl(stream, &meta, &bytes),
            Err(error) => respond_asset_error(stream, error),
        },
        ("PUT", false) => {
            if !valid_asset_name(name) {
                return respond_error(stream, 400, &format!("Invalid asset name {name:?}. {ASSET_NAME_RULE}"));
            }
            http_receive(stream, state, request, UploadTarget::Design { id, name, budget: query_budget(request) })
        }
        ("DELETE", false) => match delete_value(state, id, name) {
            Ok(value) => respond_value(stream, 200, &value),
            Err(error) => respond_asset_error(stream, error),
        },
        _ => respond_error(
            stream,
            405,
            "Assets: GET .../assets lists them; GET, PUT (raw STL body) or DELETE .../assets/{name}.",
        ),
    }
}

/// `GET /api/shared/{token}/assets[/{name}]`: read-only, and never names
/// the design id.
pub(crate) fn handle_shared_route(stream: &mut TcpStream, state: &AppState, request: &Request, token: &str, rest: &str) -> io::Result<()> {
    if request.method != "GET" {
        return respond_error(stream, 405, "A read-only design cannot be written to.");
    }
    let workspace = match state.workspaces.resolve_share(token) {
        Ok(Some(workspace)) => workspace,
        Ok(None) => return respond_error(stream, 404, "This read-only link is not valid."),
        Err(error) => return respond_error(stream, 500, &error),
    };
    let name = rest.trim_start_matches('/');
    let result = if name.is_empty() {
        list_value(state, &workspace.id, false).map(|value| respond_value(stream, 200, &value))
    } else {
        find_asset(state, &workspace.id, name).map(|(meta, bytes)| respond_stl(stream, &meta, &bytes))
    };
    match result {
        Ok(written) => written,
        Err(error) => respond_asset_error(stream, error),
    }
}

/// `PUT /api/uploads/{token}`: a one-time upload URL from
/// `create_asset_upload`, whole or in chunks. The URL is consumed only once
/// the complete file holds a compute permit, and put back if the upload then
/// fails, so it is used up by exactly one stored file. The reply never names
/// the design.
/// Whether `head` re-sends the last chunk of a finished chunked upload to the
/// one-time URL `token` (its receipt is still kept).
fn replays_token_upload(state: &AppState, head: &Request, token: &str) -> bool {
    let (Some(upload_id), Some(range)) = (
        request_upload_id(head),
        head.header("content-range").and_then(parse_content_range),
    ) else {
        return false;
    };
    matches!(
        state.workspaces.upload_receipt(&upload_id, unix_millis()),
        Ok(Some(receipt)) if receipt.replays(&format!("token:{token}"), &range)
    )
}

pub(crate) fn handle_upload_token(stream: &mut TcpStream, state: &AppState, request: &Request, token: &str) -> io::Result<()> {
    if request.method != "PUT" {
        return respond_error(stream, 405, "Upload the STL file with PUT, e.g. curl -T scan.stl '<url>'.");
    }
    match state.workspaces.peek_upload_token(token) {
        Ok(Some(_)) => {}
        Ok(None) if replays_token_upload(state, request, token) => {}
        Ok(None) => return respond_error(stream, 404, UNKNOWN_UPLOAD_URL),
        Err(error) => return respond_error(stream, 500, &error),
    }
    http_receive(stream, state, request, UploadTarget::Token(token))
}

// ---------------------------------------------------------------------------
// MCP.
// ---------------------------------------------------------------------------

pub(crate) fn tool_definitions() -> Vec<Value> {
    let design = json!({"type": "string"});
    let name = json!({"type": "string", "pattern": "^[A-Za-z0-9_-][A-Za-z0-9._-]{0,59}\\.[sS][tT][lL]$", "description": "File name as written in import(\"...\"), e.g. \"scan.stl\"."});
    let terms = format!(
        "Kept for 3 hours from when the upload completes, with no renewal; then the STL is deleted and a \
         design still importing it stops rendering. One STL per design: a second upload, under any name, is refused until delete_asset \
         removes the first. Reverse-engineer it into a real SCAD part and remove it ({GUIDE_TOOL} shows how)."
    );
    let stored = format!(
        "The server stores it cleaned and decimated to the budget (default {} triangles, at most {}); the mesh analysis tools read \
         that stored mesh, and detail lost to decimation cannot be recovered.",
        meshimport::DEFAULT_TRIANGLE_BUDGET,
        meshimport::MAX_TRIANGLE_BUDGET
    );
    let budget = json!({"type": "integer", "minimum": MIN_TRIANGLE_BUDGET, "maximum": meshimport::MAX_TRIANGLE_BUDGET, "default": meshimport::DEFAULT_TRIANGLE_BUDGET, "description": "Triangles after cleaning; larger meshes are decimated to it. The mesh analysis tools read the stored mesh, and detail lost to decimation cannot be recovered. Exact booleans with the mesh cap at 60k triangles."});
    vec![
        crate::mcp_tool(
            "upload_asset",
            &format!(
                "Attach an STL (e.g. a 3D scan) to a design for import(\"name.stl\"). {terms} {stored} \
                 Open boundaries stay open and nothing is filled, so a one-sided scan renders and compares, \
                 but booleans and volume need a closed solid. base64 is at most {MAX_INLINE_UPLOAD_BYTES} \
                 bytes decoded (about 28k triangles of binary STL). For anything larger use \
                 create_asset_upload (up to {} MiB).",
                meshimport::MAX_UPLOAD_BYTES / (1024 * 1024)
            ),
            json!({
                "type": "object",
                "required": ["designId", "name", "base64"],
                "properties": {
                    "designId": design,
                    "name": name,
                    "base64": {"type": "string", "description": "The STL file (binary or ASCII), base64-encoded."},
                    "budget": budget,
                }
            }),
        ),
        crate::mcp_tool(
            "create_asset_upload",
            &format!(
                "Get a one-time PUT URL to upload an STL too big for upload_asset (over \
                 {MAX_INLINE_UPLOAD_BYTES} bytes; up to {} MiB, about {:.1}M triangles of binary STL). \
                 {terms} {stored} Returns uploadUrl, expiresAt (of the \
                 URL: unix ms, 10 minutes), maxBytes and a curl command for files up to 24 MiB. With \
                 includeScript: true, or bytes over 24 MiB, also chunkedScript, a POSIX sh loop sending \
                 24 MiB chunks; the final PUT answers like upload_asset. A rejected upload leaves the URL \
                 usable. The server never fetches URLs, so send the file from where it is.",
                meshimport::MAX_UPLOAD_BYTES / (1024 * 1024),
                (meshimport::MAX_UPLOAD_BYTES - 84) as f64 / 50.0 / 1e6
            ),
            json!({
                "type": "object",
                "required": ["designId", "name"],
                "properties": {
                    "designId": design,
                    "name": name,
                    "budget": budget,
                    "bytes": {"type": "integer", "minimum": 0, "description": "The file size, if known."},
                    "includeScript": {"type": "boolean", "default": false, "description": "Include chunkedScript and the long usage note."},
                    "uploadUrl": {"type": "string", "description": "A still-pending uploadUrl: returns chunkedScript for it instead of minting another."}
                }
            }),
        ),
        crate::mcp_tool(
            "list_assets",
            "List a design's live STL assets for import(\"name.stl\"), with expiry, size and bounds, \
             plus the per-design limits. detail: true adds the full sha256, cleanup \
             report and geometry description.",
            json!({"type": "object", "required": ["designId"], "properties": {
                "designId": design,
                "detail": {"type": "boolean", "default": false, "description": "Full records per asset."}
            }}),
        ),
        crate::mcp_tool(
            "delete_asset",
            "Delete an STL asset and free its name. Deleting the uploaded STL lets a new STL be \
             uploaded (with a new 3-hour clock). Code still \
             importing it then renders without it, with a missing-file warning, so remove the import too.",
            json!({"type": "object", "required": ["designId", "name"], "properties": {"designId": design, "name": name}}),
        ),
    ]
}

/// Whether `name` is one of the asset tools.
pub(crate) fn is_asset_tool(name: &str) -> bool {
    matches!(name, "upload_asset" | "create_asset_upload" | "list_assets" | "delete_asset")
}

fn argument_name(arguments: &Value) -> Result<&str, String> {
    let name = arguments
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "name is required (the file name used in import(\"...\"), e.g. \"scan.stl\").".to_string())?;
    if !valid_asset_name(name) {
        return Err(format!("Invalid asset name {name:?}. {ASSET_NAME_RULE}"));
    }
    Ok(name)
}

fn argument_budget(arguments: &Value) -> usize {
    clamp_budget(arguments.get("budget").and_then(Value::as_u64))
}

fn decode_inline(text: &str, design: &str, name: &str) -> Result<Vec<u8>, String> {
    let text = match text.find(";base64,") {
        Some(position) if text.starts_with("data:") => &text[position + 8..],
        _ => text,
    };
    let compact: String = text.chars().filter(|character| !character.is_ascii_whitespace()).collect();
    if compact.len() / 4 * 3 > MAX_INLINE_UPLOAD_BYTES + 3 {
        return Err(inline_too_large(None, design, name));
    }
    let bytes = crate::base64_decode(&compact).ok_or_else(|| "base64 is not valid base64.".to_string())?;
    if bytes.len() > MAX_INLINE_UPLOAD_BYTES {
        return Err(inline_too_large(Some(bytes.len()), design, name));
    }
    Ok(bytes)
}

/// The upload_asset error for a file over the inline cap, naming the exact
/// next step.
pub(crate) fn inline_too_large(bytes: Option<usize>, design: &str, name: &str) -> String {
    let size = bytes.map(|bytes| format!(" ({bytes} bytes)")).unwrap_or_default();
    format!(
        "The file{size} is over upload_asset's {MAX_INLINE_UPLOAD_BYTES}-byte inline limit. Next step: call \
         create_asset_upload {{\"designId\": \"{design}\", \"name\": \"{name}\"}}, then, from where the file is, run \
         the curl command it returns (curl --data-binary @{name} '<uploadUrl>') for files up to {chunk} MiB, or its \
         chunkedScript (sh) for larger ones: {chunk} MiB chunks with Content-Range, for example\n{}\nThat route takes \
         files up to {} MiB (raw scans included), cleans and decimates them to the triangle budget, and replies with the \
         cleaning report and geometry description.",
        chunked_upload_script(name, "<uploadUrl>"),
        meshimport::MAX_UPLOAD_BYTES / (1024 * 1024),
        chunk = UPLOAD_CHUNK_BYTES / (1024 * 1024),
    )
}

/// A POSIX sh loop (also fine in bash and zsh, with macOS or GNU tools) that
/// PUTs `file` to `url` in [`UPLOAD_CHUNK_BYTES`] chunks with Content-Range,
/// carrying the Upload-Id the first reply returns, and prints the last reply.
/// The token and expiry of a pending upload URL `url` minted for `asset` of
/// design `id`; an error when it is not one (expired, used, or another
/// design's or file's).
fn existing_upload(state: &AppState, id: &str, asset: &str, url: &str) -> Result<(String, u64), String> {
    let token = url.trim_end_matches('/').rsplit('/').next().unwrap_or_default();
    let refused = || {
        "uploadUrl is not a pending upload URL for this design and file (expired, already used, or another file's); \
         omit it to get a new one."
            .to_string()
    };
    if !valid_token(token) {
        return Err(refused());
    }
    match state.workspaces.peek_upload_token(token)? {
        Some(pending) if pending.design_id == id && pending.name == asset && pending.expires_at > unix_millis() => {
            Ok((token.to_string(), pending.expires_at))
        }
        _ => Err(refused()),
    }
}

pub(crate) fn chunked_upload_script(file: &str, url: &str) -> String {
    // POSIX sh. Follows the server's `next` (202 and 409), retries a chunk
    // after a network error, 429 or 5xx (a re-sent chunk is a no-op; a re-sent
    // last chunk replays the stored reply), and on a stop prints where it
    // stopped and how to resume (RO_OFFSET / RO_UPLOAD_ID), exiting 1.
    let mib = UPLOAD_CHUNK_BYTES / (1024 * 1024);
    let chunk = UPLOAD_CHUNK_BYTES;
    [
        format!("f='{file}'; u='{url}'"),
        "[ -r \"$f\" ] || { echo \"Cannot read $f\" >&2; exit 1; }".to_string(),
        "n=$(wc -c <\"$f\" | tr -d ' '); [ \"$n\" -gt 0 ] || { echo \"$f is empty\" >&2; exit 1; }".to_string(),
        "b=$(mktemp) || exit 1; o=${RO_OFFSET:-0}; id=${RO_UPLOAD_ID:-}; a=0; c=000; r=".to_string(),
        "while :; do".to_string(),
        format!("  l=$((n - o)); [ \"$l\" -gt {chunk} ] && l={chunk}"),
        "  : >\"$b\"".to_string(),
        format!(
            "  c=$(dd if=\"$f\" bs=1048576 skip=$((o / 1048576)) count={mib} 2>/dev/null | curl -sS -o \"$b\" -w '%{{http_code}}' \
             -X PUT -H \"Content-Range: bytes $o-$((o + l - 1))/$n\" -H \"Upload-Id: $id\" --data-binary @- \"$u\") || c=000"
        ),
        "  r=$(cat \"$b\"); x=$(printf '%s' \"$r\" | sed -n 's/.*\"uploadId\":\"\\([0-9a-f]*\\)\".*/\\1/p'); [ -n \"$x\" ] && id=$x".to_string(),
        "  nx=$(printf '%s' \"$r\" | sed -n 's/.*\"next\":\\([0-9]*\\).*/\\1/p')".to_string(),
        "  w=$(printf '%s' \"$r\" | sed -n 's/.*\"retryAfterSeconds\":\\([0-9]*\\).*/\\1/p')".to_string(),
        "  case $c in".to_string(),
        "    200|201) break ;;".to_string(),
        "    202) [ -n \"$nx\" ] || break; o=$nx; a=0 ;;".to_string(),
        "    409) [ -n \"$nx\" ] && [ \"$a\" -lt 3 ] || break; o=$nx; a=$((a + 1)) ;;".to_string(),
        "    000|429|5??) [ \"$a\" -lt 6 ] || break; a=$((a + 1)); sleep \"${w:-$((a * 2))}\" ;;".to_string(),
        "    *) break ;;".to_string(),
        "  esac".to_string(),
        "done; rm -f \"$b\"; printf '%s\\n' \"$r\"".to_string(),
        "case $c in 2??) ;; *) echo \"Upload stopped (HTTP $c) at byte $o of $n. Resume: RO_OFFSET=$o RO_UPLOAD_ID=$id sh <this script>\" >&2; exit 1 ;; esac"
            .to_string(),
    ]
    .join("\n")
}

/// A `list_assets` entry without the cleanup report and geometry
/// description (`detail: true` keeps them; describe_geometry on
/// `import("name.stl")` analyses one): name, short sha256, bytes, triangles,
/// createdAt and the bounds and size from the description.
fn compact_asset(asset: &mut Value) {
    let description = asset["description"].take();
    let short = asset["sha256"].as_str().map(|sha| sha.chars().take(12).collect::<String>());
    let mut compact = json!({
        "name": asset["name"],
        "sha256": short,
        "bytes": asset["bytes"],
        "triangles": asset["triangles"],
        "createdAt": asset["createdAt"],
        "expiresAt": asset["expiresAt"],
        "expiresIn": asset["expiresIn"],
    });
    if description.is_object() {
        compact["boundsMin"] = description["boundsMin"].clone();
        compact["boundsMax"] = description["boundsMax"].clone();
        compact["size"] = description["size"].clone();
    }
    *asset = compact;
}

/// Runs an asset tool and shapes the MCP tool result. `upload_asset`
/// processes a mesh, so it takes a compute permit like the evaluator tools.
pub(crate) fn mcp_dispatch(state: &AppState, request: &Request, name: &str, arguments: &Value) -> Value {
    let _permit = if name == "upload_asset" {
        match state.render_queue.acquire(current_client_key(), None, None) {
            Ok(permit) => Some(permit),
            Err(rejection) => return crate::mcp_queue_rejection(rejection),
        }
    } else {
        None
    };
    match call_tool(state, request, name, arguments) {
        Ok(structured) => json!({
            "content": [{"type": "text", "text": serde_json::to_string(&structured).unwrap_or_default()}],
            "structuredContent": structured,
            "isError": false
        }),
        Err(error) => crate::mcp_tool_error(&error),
    }
}

/// Runs an asset tool. Arguments are normalized (`workspaceId` holds the
/// design id). The caller holds a compute permit for `upload_asset`.
pub(crate) fn call_tool(state: &AppState, request: &Request, name: &str, arguments: &Value) -> Result<Value, String> {
    let id = crate::argument_workspace_id(arguments)?;
    let message = |error: AssetError| error.message;
    match name {
        "upload_asset" => {
            let asset = argument_name(arguments)?;
            let text = arguments
                .get("base64")
                .and_then(Value::as_str)
                .ok_or_else(|| "base64 is required: the STL file, base64-encoded.".to_string())?;
            let bytes = decode_inline(text, id, asset)?;
            charge_upload_bytes(state, bytes.len());
            store_upload(state, id, asset, &bytes, argument_budget(arguments), true).map_err(message)
        }
        "create_asset_upload" => {
            let asset = argument_name(arguments)?;
            design_exists(state, id).map_err(message)?;
            admit_new_upload(state, id).map_err(message)?;
            // `uploadUrl` from an earlier reply: answer for that URL (with
            // the script) instead of minting another, which would leave the
            // first pending and count against the per-design limit.
            let existing = match arguments.get("uploadUrl").and_then(Value::as_str) {
                Some(url) => Some(existing_upload(state, id, asset, url)?),
                None => None,
            };
            let reused = existing.is_some();
            let (token, expires_at) = match existing {
                Some(existing) => existing,
                None => {
                    let token = mint_token()?;
                    let expires_at = unix_millis() + UPLOAD_TOKEN_TTL_MS;
                    state.workspaces.create_upload_token(
                        &token,
                        PendingUpload {
                            design_id: id.to_string(),
                            name: asset.to_string(),
                            budget: argument_budget(arguments),
                            expires_at,
                            client: current_ip().map(|address| address.to_string()).unwrap_or_default(),
                        },
                    )?;
                    (token, expires_at)
                }
            };
            let url = format!("{}/api/uploads/{token}", crate::request_base_url(state, request));
            let chunk = UPLOAD_CHUNK_BYTES / (1024 * 1024);
            let curl = format!("curl --fail-with-body -X PUT --data-binary @{asset} '{url}'");
            // The STL's own clock starts when the upload completes.
            let now = unix_millis();
            let stl_expiry = json!({
                "keptSeconds": ASSET_TTL_MS / 1000,
                "expiresAtIfUploadedNow": now + ASSET_TTL_MS,
                "expiresAtIsoIfUploadedNow": iso_utc(now + ASSET_TTL_MS),
                "note": format!(
                    "The STL is kept 3 hours from when this upload completes, with no renewal; the upload's reply states its exact expiresAt. \
                     Reverse-engineer it into a real SCAD part and remove the STL: {GUIDE_TOOL} shows how."
                ),
            });
            // The chunked script and the long note only when they are needed:
            // asked for, or a stated file size over one chunk.
            let chunked = reused
                || arguments.get("includeScript").and_then(Value::as_bool) == Some(true)
                || arguments
                    .get("bytes")
                    .and_then(Value::as_u64)
                    .is_some_and(|bytes| bytes > UPLOAD_CHUNK_BYTES as u64);
            if !chunked {
                return Ok(json!({
                    "ok": true,
                    "designId": id,
                    "name": asset,
                    "uploadUrl": url,
                    "method": "PUT",
                    "expiresAt": expires_at,
                    "maxBytes": meshimport::MAX_UPLOAD_BYTES,
                    "curl": curl,
                    "stlExpiry": stl_expiry,
                    "guide": GUIDE_TOOL,
                    "note": format!(
                        "One-time URL (10 min). Run curl from the directory holding {asset}; the reply is upload_asset's JSON. \
                         Files over {chunk} MiB: call again with uploadUrl set to this uploadUrl for the chunked upload script \
                         for this same URL (no new URL is minted). \
                         Then use import(\"{asset}\");."
                    ),
                }));
            }
            Ok(json!({
                "ok": true,
                "designId": id,
                "name": asset,
                "uploadUrl": url,
                "method": "PUT",
                "expiresAt": expires_at,
                "expiresInSeconds": UPLOAD_TOKEN_TTL_MS / 1000,
                "maxBytes": meshimport::MAX_UPLOAD_BYTES,
                "maxMiB": meshimport::MAX_UPLOAD_BYTES / (1024 * 1024),
                "curl": curl,
                "curlAlternatives": [format!("curl --fail-with-body -T {asset} '{url}'")],
                "chunkBytes": UPLOAD_CHUNK_BYTES,
                "chunkedScript": chunked_upload_script(asset, &url),
                "stlExpiry": stl_expiry,
                "guide": GUIDE_TOOL,
                "note": format!(
                    "One-time URL: PUT the raw STL file (binary or ASCII, up to {} MiB) as the body, from the directory holding \
                     {asset} (or set f to its path). Files up to {chunk} MiB: run curl. Larger files: run chunkedScript with sh \
                     (it works in bash and zsh too); it sends {chunk} MiB chunks with Content-Range: bytes first-last/total and \
                     the Upload-Id the first reply returns, each answered 202 {{uploadId, received, next}} until the last, retries \
                     a chunk after a network error, 429 or 5xx, and if it still stops prints the byte it stopped at and the \
                     RO_OFFSET/RO_UPLOAD_ID values that resume it, exiting 1. The \
                     server cleans the file and decimates it to {} triangles; the final reply is the same JSON upload_asset returns \
                     (expiresAt and expiresIn of the STL, report = cleaning report, description = size, units, axes, features). Then use import(\"{asset}\"); in the \
                     design.",
                    meshimport::MAX_UPLOAD_BYTES / (1024 * 1024),
                    argument_budget(arguments),
                ),
            }))
        }
        "list_assets" => {
            let mut value = design_exists(state, id).and_then(|()| list_value(state, id, true)).map_err(message)?;
            if arguments.get("detail").and_then(Value::as_bool) != Some(true) {
                if let Some(assets) = value.get_mut("assets").and_then(Value::as_array_mut) {
                    assets.iter_mut().for_each(compact_asset);
                }
            }
            Ok(value)
        }
        "delete_asset" => delete_value(state, id, argument_name(arguments)?).map_err(message),
        _ => Err(format!("Unknown tool {name:?}.")),
    }
}

#[cfg(test)]
#[path = "assetstore_tests.rs"]
mod tests;
