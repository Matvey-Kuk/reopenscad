//! Comments pinned to the 3D model, for the agent.
//!
//! A person looking at a design in the browser clicks a point on a surface
//! and types a remark; the comment keeps the 3D point, the face normal, the
//! object and triangle it landed on, the revision it was made against, and
//! the source line of that object. An agent reads them with `list_feedback`,
//! answers and closes them with `resolve_feedback`, and can ask for them with
//! `request_pointing`, which switches every open browser tab of the design
//! into comment mode with a one-line hint naming what the agent needs located;
//! the person's answer is a pin on that spot.
//!
//! Storage is one small document per design ([`FeedbackState`]): a sidecar
//! file next to the design on the filesystem store, one row in
//! `workspace_feedback` on Postgres. Every change bumps `version`, which is
//! what the browser's `/events` long poll and a waiting tool watch.
//!
//! Comments are reachable only through the editable design id (a
//! capability): neither `/api/shared/` nor the read-only view ever reads
//! them.

use super::*;

/// Comments one design keeps. When full, the oldest resolved comment makes
/// room; a design full of open comments refuses new ones.
pub(crate) const MAX_COMMENTS: usize = 100;
/// Characters in a comment, an agent reply, and a pointing hint.
pub(crate) const MAX_TEXT_CHARS: usize = 1000;
pub(crate) const MAX_REPLY_CHARS: usize = 1000;
pub(crate) const MAX_WHAT_CHARS: usize = 200;
/// Longest a tool call may block waiting for feedback.
pub(crate) const MAX_WAIT_SECONDS: u64 = 120;
/// An unanswered request leaves comment mode after this long.
pub(crate) const REQUEST_TTL_MS: u64 = 15 * 60 * 1000;
/// A browser counts as connected when it long-polled `/events` this recently.
/// A visible tab re-polls within a second of each 5 s poll ending.
pub(crate) const VIEWER_FRESH_MS: u64 = 20_000;
/// Concurrent waiting tool calls, per design and in total. Each one parks a
/// connection thread for up to [`MAX_WAIT_SECONDS`].
pub(crate) const MAX_WAITERS_PER_DESIGN: usize = 2;
pub(crate) const MAX_WAITERS_TOTAL: usize = 16;
/// Most comment ids one `resolve_feedback` call names.
const MAX_RESOLVE_IDS: usize = MAX_COMMENTS;
/// Longest object id or name a comment records.
const MAX_LABEL_CHARS: usize = 128;
/// How often a waiter re-reads the version, and how often it reports progress.
const WAIT_TICK: Duration = Duration::from_millis(400);
const RECHECK_EVERY: Duration = Duration::from_secs(2);
const PROGRESS_EVERY: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FeedbackState {
    #[serde(default)]
    pub version: u64,
    /// Sequence number of the last comment created; never reused.
    #[serde(default)]
    pub last_seq: u64,
    #[serde(default)]
    pub comments: Vec<Comment>,
    #[serde(default)]
    pub request: Option<FeedbackRequest>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Comment {
    pub id: String,
    pub seq: u64,
    pub revision: u64,
    pub text: String,
    pub point: [f64; 3],
    #[serde(default)]
    pub normal: Option<[f64; 3]>,
    #[serde(default)]
    pub object_id: Option<String>,
    #[serde(default)]
    pub object_name: Option<String>,
    /// Triangle index within the object's mesh at `revision`.
    #[serde(default)]
    pub face: Option<u64>,
    /// 1-based line where the clicked object's source starts (object-level
    /// provenance, not per face).
    #[serde(default)]
    pub source_line: Option<u64>,
    /// The `request_pointing` call this pin answers, if any, and what it
    /// asked to have located.
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_what: Option<String>,
    pub status: String,
    #[serde(default)]
    pub reply: Option<String>,
    pub created_at: u64,
    #[serde(default)]
    pub resolved_at: Option<u64>,
    /// The state version of this comment's last change (creation, status,
    /// reply): what `list_feedback`'s `since` cursor compares, so a reopened
    /// comment reaches an agent polling past it.
    #[serde(default)]
    pub changed: u64,
    /// Idempotency key from the browser: a retried save of the same draft
    /// returns the stored comment instead of adding a second one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FeedbackRequest {
    pub id: String,
    /// What the agent asked to have pointed out; empty for "a spot".
    #[serde(default)]
    pub what: String,
    pub created_at: u64,
    pub expires_at: u64,
    /// active, answered (a pin was saved for it), dismissed (the person left
    /// comment mode), superseded or expired.
    pub status: String,
    #[serde(default)]
    pub ended_at: Option<u64>,
}

impl FeedbackState {
    pub fn open_count(&self) -> usize {
        self.comments.iter().filter(|comment| comment.status == "open").count()
    }

    /// The request still asking for feedback at `now`, if any.
    pub fn active_request(&self, now: u64) -> Option<&FeedbackRequest> {
        self.request
            .as_ref()
            .filter(|request| request.status == "active" && request.expires_at > now)
    }

    /// The request with its status as of `now` (an active one past its
    /// expiry reads as expired).
    fn request_as_of(&self, now: u64) -> Option<FeedbackRequest> {
        self.request.clone().map(|mut request| {
            if request.status == "active" && request.expires_at <= now {
                request.status = "expired".into();
                request.ended_at = Some(request.expires_at);
            }
            request
        })
    }
}

// ---------------------------------------------------------------------------
// Pure state changes
// ---------------------------------------------------------------------------

/// Removes control characters other than newline and tab, trims, and checks
/// the length.
fn clean_text(value: Option<&Value>, field: &str, limit: usize, required: bool) -> Result<Option<String>, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return if required { Err(format!("{field} is required.")) } else { Ok(None) };
    };
    let raw = value.as_str().ok_or_else(|| format!("{field} must be a string."))?;
    let cleaned: String = raw
        .chars()
        .filter(|character| !character.is_control() || *character == '\n' || *character == '\t')
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        return if required { Err(format!("{field} must not be empty.")) } else { Ok(None) };
    }
    if cleaned.chars().count() > limit {
        return Err(format!("{field} is longer than {limit} characters."));
    }
    Ok(Some(cleaned))
}

fn vector(value: Option<&Value>, field: &str) -> Result<Option<[f64; 3]>, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let items = value
        .as_array()
        .filter(|items| items.len() == 3)
        .ok_or_else(|| format!("{field} must be [x, y, z]."))?;
    let mut out = [0.0; 3];
    for (slot, item) in out.iter_mut().zip(items) {
        let number = item.as_f64().filter(|number| number.is_finite() && number.abs() <= 1e7);
        *slot = number.ok_or_else(|| format!("{field} must hold three finite numbers."))?;
    }
    Ok(Some(out))
}

fn label(value: Option<&Value>, field: &str) -> Result<Option<String>, String> {
    clean_text(value, field, MAX_LABEL_CHARS, false)
}

fn small_integer(value: Option<&Value>, field: &str) -> Result<Option<u64>, String> {
    match value.filter(|value| !value.is_null()) {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|number| *number <= 1_000_000_000)
            .map(Some)
            .ok_or_else(|| format!("{field} must be a non-negative integer.")),
    }
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// Whether a comment names `revision`, so that revision must stay readable and
/// restorable. A state that cannot be read counts as yes: keeping one autosave
/// too many costs less than a pin whose revision is gone.
pub(crate) fn pins_revision(state: Result<FeedbackState, String>, revision: u64) -> bool {
    state.map_or(true, |state| state.comments.iter().any(|comment| comment.revision == revision))
}

/// Adds a comment from the browser's JSON body. `current_revision` caps the
/// revision the client claims to have commented on.
pub(crate) fn add_comment(
    state: &mut FeedbackState,
    input: &Value,
    current_revision: u64,
    now: u64,
) -> Result<Comment, String> {
    // A bare pin is a full answer to request_pointing; the text is optional.
    let text = clean_text(input.get("text"), "text", MAX_TEXT_CHARS, false)?.unwrap_or_default();
    let point = vector(input.get("point"), "point")?.ok_or_else(|| "point is required.".to_string())?;
    let normal = vector(input.get("normal"), "normal")?.and_then(|normal| {
        let length = (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]).sqrt();
        (length > 1e-9).then(|| normal.map(|axis| round3(axis / length)))
    });
    let object_id = label(input.get("objectId"), "objectId")?;
    let object_name = label(input.get("objectName"), "objectName")?;
    let face = small_integer(input.get("face"), "face")?;
    let source_line = small_integer(input.get("sourceLine"), "sourceLine")?.filter(|line| *line > 0);
    let revision = small_integer(input.get("revision"), "revision")?
        .filter(|revision| *revision > 0 && *revision <= current_revision)
        .unwrap_or(current_revision);
    let client_key = input
        .get("clientKey")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty() && key.len() <= 64 && key.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))
        .map(str::to_string);
    if let Some(existing) = client_key
        .as_ref()
        .and_then(|key| state.comments.iter().find(|comment| comment.client_key.as_ref() == Some(key)))
    {
        return Ok(existing.clone());
    }
    // Tied to the request only while that request is still the live one; the
    // first pin answers it and ends comment mode for it.
    let answered = input
        .get("requestId")
        .and_then(Value::as_str)
        .and_then(|id| state.active_request(now).filter(|request| request.id == id))
        .map(|request| (request.id.clone(), request.what.clone()));
    let (request_id, request_what) = match answered {
        Some((id, what)) => (Some(id), (!what.is_empty()).then_some(what)),
        None => (None, None),
    };
    if state.comments.len() >= MAX_COMMENTS {
        let Some(oldest_resolved) = state.comments.iter().position(|comment| comment.status != "open") else {
            return Err(format!(
                "This design already holds {MAX_COMMENTS} open comments. Resolve some before adding more."
            ));
        };
        state.comments.remove(oldest_resolved);
    }
    state.last_seq += 1;
    let comment = Comment {
        id: format!("c{}", state.last_seq),
        seq: state.last_seq,
        revision,
        text,
        point: point.map(round3),
        normal,
        object_id,
        object_name,
        face,
        source_line,
        request_id,
        request_what,
        status: "open".into(),
        reply: None,
        created_at: now,
        resolved_at: None,
        changed: state.version + 1,
        client_key,
    };
    state.comments.push(comment.clone());
    if let Some(request) = state
        .request
        .as_mut()
        .filter(|request| comment.request_id.as_ref() == Some(&request.id))
    {
        request.status = "answered".into();
        request.ended_at = Some(now);
    }
    state.version += 1;
    Ok(comment)
}

/// Sets each named comment's status. Returns the ids changed and the ids not
/// found.
pub(crate) fn set_status(
    state: &mut FeedbackState,
    ids: &[String],
    resolved: bool,
    reply: Option<&str>,
    now: u64,
) -> (Vec<String>, Vec<String>) {
    let mut changed = Vec::new();
    let mut missing = Vec::new();
    let next_version = state.version + 1;
    for id in ids {
        match state.comments.iter_mut().find(|comment| &comment.id == id) {
            Some(comment) => {
                comment.changed = next_version;
                comment.status = if resolved { "resolved" } else { "open" }.into();
                comment.resolved_at = resolved.then_some(now);
                if let Some(reply) = reply {
                    comment.reply = Some(reply.to_string());
                }
                changed.push(id.clone());
            }
            None => missing.push(id.clone()),
        }
    }
    if !changed.is_empty() {
        state.version += 1;
    }
    (changed, missing)
}

pub(crate) fn delete_comment(state: &mut FeedbackState, id: &str) -> bool {
    let before = state.comments.len();
    state.comments.retain(|comment| comment.id != id);
    let removed = state.comments.len() != before;
    if removed {
        state.version += 1;
    }
    removed
}

/// Starts a request, superseding any active one.
pub(crate) fn start_request(state: &mut FeedbackState, what: String, now: u64, nonce: u64) -> FeedbackRequest {
    if let Some(previous) = state.request.as_mut().filter(|request| request.status == "active") {
        previous.status = "superseded".into();
        previous.ended_at = Some(now);
    }
    let request = FeedbackRequest {
        id: format!("r{}{}", base36(now), base36(nonce)),
        what,
        created_at: now,
        expires_at: now + REQUEST_TTL_MS,
        status: "active".into(),
        ended_at: None,
    };
    state.request = Some(request.clone());
    state.version += 1;
    request
}

/// The person left comment mode while request `id` was showing. False when
/// `id` is not the active request (already dismissed, superseded or expired).
pub(crate) fn dismiss_request(state: &mut FeedbackState, id: &str, now: u64) -> bool {
    let Some(request) = state
        .request
        .as_mut()
        .filter(|request| request.id == id && request.status == "active" && request.expires_at > now)
    else {
        return false;
    };
    request.status = "dismissed".into();
    request.ended_at = Some(now);
    state.version += 1;
    true
}

fn base36(mut value: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(value % 36) as usize]);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Public shapes
// ---------------------------------------------------------------------------

/// The direction a normal mostly faces, as `+Z`, `-X`, ...
fn facing(normal: [f64; 3]) -> String {
    let (axis, value) = normal
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        .map(|(axis, value)| (axis, *value))
        .unwrap_or((2, 1.0));
    format!("{}{}", if value < 0.0 { "-" } else { "+" }, ["X", "Y", "Z"][axis])
}

/// One line saying where a comment is, for a model to read without parsing
/// the fields.
fn location_summary(comment: &Comment) -> String {
    let mut out = String::new();
    match (&comment.object_name, &comment.object_id) {
        (Some(name), _) => out.push_str(&format!("on {name}")),
        (None, Some(id)) => out.push_str(&format!("on {id}")),
        _ => out.push_str("on the model"),
    }
    if let Some(line) = comment.source_line {
        out.push_str(&format!(" (source line {line})"));
    }
    let [x, y, z] = comment.point;
    out.push_str(&format!(" at [{x}, {y}, {z}]"));
    if let Some(normal) = comment.normal {
        out.push_str(&format!(", surface facing {}", facing(normal)));
    }
    out.push_str(&format!(", revision {}", comment.revision));
    out
}

pub(crate) fn comment_public(comment: &Comment) -> Value {
    let mut value = json!({
        "id": comment.id,
        "status": comment.status,
        "text": comment.text,
        "where": location_summary(comment),
        "point": comment.point,
        "normal": comment.normal,
        "objectId": comment.object_id,
        "objectName": comment.object_name,
        "face": comment.face,
        "sourceLine": comment.source_line,
        "revision": comment.revision,
        "requestId": comment.request_id,
        "answers": comment.request_what,
        "reply": comment.reply,
        "createdAt": comment.created_at,
        "resolvedAt": comment.resolved_at
    });
    // Absent beats null for a model reading the reply: fewer tokens, same meaning.
    if let Some(object) = value.as_object_mut() {
        object.retain(|_, field| !field.is_null());
    }
    value
}

fn request_public(request: &FeedbackRequest) -> Value {
    json!({
        "id": request.id,
        "what": request.what,
        "status": request.status,
        "createdAt": request.created_at,
        "expiresAt": request.expires_at,
        "endedAt": request.ended_at
    })
}

/// The `/events` summary: enough for the browser to know whether to refetch
/// the list and whether to be in comment mode.
pub(crate) fn events_summary(state: &FeedbackState, now: u64) -> Value {
    json!({
        "version": state.version,
        "openCount": state.open_count(),
        "request": state.active_request(now).map(request_public)
    })
}

/// The browser's full view.
fn browser_state(state: &FeedbackState, now: u64) -> Value {
    json!({
        "ok": true,
        "version": state.version,
        "openCount": state.open_count(),
        // Numbers are never reused, so the browser can label its pin cursor
        // with the number the next comment will get.
        "lastSeq": state.last_seq,
        "comments": state.comments.iter().map(comment_public).collect::<Vec<_>>(),
        "request": state.active_request(now).map(request_public)
    })
}

// ---------------------------------------------------------------------------
// Filesystem backend
// ---------------------------------------------------------------------------

/// Feedback for the filesystem store: a `<id>.feedback` sidecar per design,
/// read once and kept in a bounded in-process cache. One mutex serialises
/// every write, which is plenty for comment traffic.
pub(crate) struct FsFeedback {
    root: PathBuf,
    cache: Mutex<HashMap<String, FeedbackState>>,
    nonce: AtomicU64,
}

/// Designs whose feedback the filesystem cache keeps loaded.
const FS_CACHE_LIMIT: usize = 256;

impl FsFeedback {
    pub fn new(root: PathBuf) -> Self {
        Self { root, cache: Mutex::new(HashMap::new()), nonce: AtomicU64::new(unix_millis()) }
    }

    pub fn path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.feedback"))
    }

    fn read_file(&self, id: &str) -> FeedbackState {
        fs::read(self.path(id))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn with_cached<T>(&self, id: &str, action: impl FnOnce(&mut FeedbackState) -> T) -> Result<T, String> {
        let mut cache = self.cache.lock().map_err(|_| "Feedback store is unavailable.".to_string())?;
        if !cache.contains_key(id) {
            if cache.len() >= FS_CACHE_LIMIT {
                cache.clear();
            }
            let loaded = self.read_file(id);
            cache.insert(id.to_string(), loaded);
        }
        let entry = cache.get_mut(id).ok_or_else(|| "Feedback store is unavailable.".to_string())?;
        Ok(action(entry))
    }

    pub fn state(&self, id: &str) -> Result<FeedbackState, String> {
        self.with_cached(id, |state| state.clone())
    }

    pub fn summary(&self, id: &str) -> Result<(u64, usize), String> {
        self.with_cached(id, |state| (state.version, state.open_count()))
    }

    /// Applies `change` and writes the sidecar atomically when it succeeds
    /// and moved the version. A failed write leaves the cache untouched.
    pub fn mutate<T>(
        &self,
        id: &str,
        change: impl FnOnce(&mut FeedbackState) -> Result<T, String>,
    ) -> Result<T, String> {
        self.with_cached(id, |state| {
            let mut next = state.clone();
            let outcome = change(&mut next)?;
            if next != *state {
                self.write(id, &next)?;
                *state = next;
            }
            Ok(outcome)
        })?
    }

    fn write(&self, id: &str, state: &FeedbackState) -> Result<(), String> {
        let bytes = serde_json::to_vec(state).map_err(|error| error.to_string())?;
        let temporary = self.root.join(format!(
            ".{id}-{}.feedback.tmp",
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
            fs::rename(&temporary, self.path(id))
        })();
        if let Err(error) = written {
            let _ = fs::remove_file(&temporary);
            return Err(format!("Feedback could not be saved: {error}"));
        }
        Ok(())
    }

    /// Forgets a design's feedback (eviction).
    pub fn remove(&self, id: &str) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.remove(id);
        }
        let _ = fs::remove_file(self.path(id));
    }
}

// ---------------------------------------------------------------------------
// Viewer presence
// ---------------------------------------------------------------------------

/// When a browser last long-polled each design on this instance.
static VIEWERS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);
/// When this instance last persisted each design's sighting.
static PERSISTED: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);
const MAX_VIEWER_ENTRIES: usize = 4096;

/// Records that a browser is watching `id`. Returns true when this instance
/// had not seen one for a while (the caller may then persist the sighting
/// for other instances).
pub(crate) fn note_viewer_locally(id: &str, now: u64, persist_every_ms: u64) -> bool {
    let Ok(mut guard) = VIEWERS.lock() else {
        return false;
    };
    let viewers = guard.get_or_insert_with(HashMap::new);
    if viewers.len() >= MAX_VIEWER_ENTRIES {
        viewers.retain(|_, seen| now.saturating_sub(*seen) < VIEWER_FRESH_MS);
        if viewers.len() >= MAX_VIEWER_ENTRIES {
            return false;
        }
    }
    viewers.insert(id.to_string(), now);
    drop(guard);
    // Compared against the last *persisted* sighting, not the last poll:
    // polls arrive every ~5 s, so the latter would never reach the interval.
    let Ok(mut guard) = PERSISTED.lock() else {
        return false;
    };
    let persisted = guard.get_or_insert_with(HashMap::new);
    if persisted.len() >= MAX_VIEWER_ENTRIES {
        persisted.retain(|_, at| now.saturating_sub(*at) < VIEWER_FRESH_MS);
    }
    let due = persisted.get(id).is_none_or(|at| now.saturating_sub(*at) >= persist_every_ms);
    if due {
        persisted.insert(id.to_string(), now);
    }
    due
}

pub(crate) fn viewer_seen_locally(id: &str) -> u64 {
    VIEWERS
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().and_then(|viewers| viewers.get(id).copied()))
        .unwrap_or(0)
}

fn browser_connected(state: &AppState, id: &str, now: u64) -> bool {
    now.saturating_sub(state.workspaces.viewer_seen_at(id)) < VIEWER_FRESH_MS
}

// ---------------------------------------------------------------------------
// Waiting
// ---------------------------------------------------------------------------

static REQUEST_NONCE: AtomicU64 = AtomicU64::new(0);

/// A request id suffix unique across instances: a per-process random seed
/// (std's hasher keys are randomly seeded) mixed with a counter.
fn request_nonce() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    static SEED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let seed = *SEED.get_or_init(|| {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(unix_millis());
        hasher.write_u32(std::process::id());
        hasher.finish()
    });
    seed.wrapping_add(REQUEST_NONCE.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9e37_79b9_7f4a_7c15)) >> 8
}
static WAITERS_TOTAL: AtomicUsize = AtomicUsize::new(0);
static WAITERS_BY_DESIGN: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);

/// One admitted waiter; releases its slots on drop.
pub(crate) struct WaitSlot {
    id: String,
}

impl WaitSlot {
    pub fn acquire(id: &str) -> Result<Self, String> {
        let mut guard = WAITERS_BY_DESIGN
            .lock()
            .map_err(|_| "Feedback waiting is unavailable.".to_string())?;
        let by_design = guard.get_or_insert_with(HashMap::new);
        let current = by_design.get(id).copied().unwrap_or(0);
        if current >= MAX_WAITERS_PER_DESIGN {
            return Err(format!(
                "{MAX_WAITERS_PER_DESIGN} calls are already waiting for feedback on this design. Call again with waitSeconds 0, or after one returns."
            ));
        }
        if WAITERS_TOTAL.fetch_add(1, Ordering::SeqCst) >= MAX_WAITERS_TOTAL {
            WAITERS_TOTAL.fetch_sub(1, Ordering::SeqCst);
            return Err("The server is holding as many waiting feedback calls as it allows. Retry in a few seconds, or call with waitSeconds 0.".into());
        }
        by_design.insert(id.to_string(), current + 1);
        Ok(Self { id: id.to_string() })
    }
}

impl Drop for WaitSlot {
    fn drop(&mut self) {
        WAITERS_TOTAL.fetch_sub(1, Ordering::SeqCst);
        if let Ok(mut guard) = WAITERS_BY_DESIGN.lock() {
            if let Some(by_design) = guard.as_mut() {
                if let Some(count) = by_design.get_mut(&self.id) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        by_design.remove(&self.id);
                    }
                }
            }
        }
    }
}

/// Reports progress while a tool waits: `(elapsed seconds, total seconds,
/// message)`. An error (the client went away) ends the wait.
pub(crate) type ProgressSink<'a> = &'a mut dyn FnMut(u64, u64, &str) -> io::Result<()>;

#[derive(Debug, PartialEq)]
enum WaitEnd {
    Ready,
    TimedOut,
    Disconnected,
}

/// Waits until `ready` holds for the design's feedback, re-reading it only
/// when its version moves.
fn wait_for(
    state: &AppState,
    id: &str,
    seconds: u64,
    mut progress: Option<ProgressSink<'_>>,
    mut ready: impl FnMut(&FeedbackState, u64) -> bool,
) -> Result<WaitEnd, String> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    let mut seen_version = None;
    let mut last_check = started;
    let mut last_progress = started;
    loop {
        if SHUTTING_DOWN.load(Ordering::SeqCst) {
            return Ok(WaitEnd::TimedOut);
        }
        let (version, _) = state.workspaces.feedback_summary(id)?;
        // Re-read on every version change, and now and then anyway: a
        // request expires with time, not with a write.
        if seen_version != Some(version) || last_check.elapsed() >= RECHECK_EVERY {
            seen_version = Some(version);
            last_check = Instant::now();
            if ready(&state.workspaces.feedback_state(id)?, unix_millis()) {
                return Ok(WaitEnd::Ready);
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(WaitEnd::TimedOut);
        }
        if let Some(sink) = progress.as_mut() {
            if last_progress.elapsed() >= PROGRESS_EVERY {
                last_progress = Instant::now();
                if sink(started.elapsed().as_secs(), seconds, "Waiting for feedback in the browser").is_err() {
                    return Ok(WaitEnd::Disconnected);
                }
            }
        }
        thread::sleep(WAIT_TICK.min(deadline.saturating_duration_since(now)).max(Duration::from_millis(1)));
    }
}

fn wait_seconds(arguments: &Value) -> Result<u64, String> {
    let value = arguments.get("waitSeconds").or_else(|| arguments.get("wait_seconds"));
    match value.filter(|value| !value.is_null()) {
        None => Ok(0),
        Some(value) => {
            let seconds = value
                .as_f64()
                .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                .ok_or_else(|| "waitSeconds must be a non-negative number.".to_string())?;
            Ok((seconds.ceil() as u64).min(MAX_WAIT_SECONDS))
        }
    }
}

/// Whether a call to `name` with these arguments may block, so the transport
/// can stream progress while it does.
pub(crate) fn tool_may_wait(name: &str, arguments: &Value) -> bool {
    matches!(name, "list_feedback" | "request_pointing") && wait_seconds(arguments).unwrap_or(0) > 0
}

pub(crate) fn is_feedback_tool(name: &str) -> bool {
    matches!(name, "list_feedback" | "request_pointing" | "resolve_feedback")
}

// ---------------------------------------------------------------------------
// MCP tools
// ---------------------------------------------------------------------------

fn existing_design(state: &AppState, arguments: &Value) -> Result<(String, u64), String> {
    let id = argument_workspace_id(arguments)?.to_string();
    let revision = state
        .workspaces
        .revision_of(&id)?
        .ok_or_else(|| "Design not found.".to_string())?;
    Ok((id, revision))
}

fn status_filter(arguments: &Value) -> Result<&str, String> {
    match arguments.get("status").and_then(Value::as_str).unwrap_or("open") {
        status @ ("open" | "resolved" | "all") => Ok(status),
        other => Err(format!("status must be open, resolved or all, not {other:?}.")),
    }
}

fn matches_status(comment: &Comment, filter: &str) -> bool {
    filter == "all" || comment.status == filter
}

pub(crate) fn tool_definitions() -> Vec<Value> {
    vec![
        mcp_tool(
            "list_feedback",
            "Read the comments a person pinned to the 3D model: text (may be empty), point and normal \
             (model-space mm), object, source line, revision, reply. since returns only comments created \
             or changed after that cursor. waitSeconds blocks until a matching comment arrives or the \
             person leaves comment mode; on timeout call again with the returned cursor. Resolve what you \
             handled with resolve_feedback.",
            json!({
                "type": "object",
                "required": ["designId"],
                "properties": {
                    "designId": {"type": "string"},
                    "status": {"type": "string", "enum": ["open", "resolved", "all"], "default": "open"},
                    "since": {"type": "integer", "minimum": 0},
                    "waitSeconds": {"type": "number", "minimum": 0, "maximum": MAX_WAIT_SECONDS, "default": 0}
                }
            }),
        ),
        mcp_tool(
            "request_pointing",
            "Have the person point at something on the 3D model, when you cannot tell which feature they \
             mean (\"that edge\"); ask open questions in chat instead. Switches open tabs into comment \
             mode with a hint from what. With waitSeconds the call returns when they save a pin \
             (requestStatus answered; the first pin closes comment mode), leave comment mode (dismissed), \
             or on timeout (still active; list_feedback with waitSeconds keeps waiting). A new call \
             replaces the previous one. If no tab polled in the last 20 s (browserConnected), it returns \
             at once with the URL to give the person; the request stays active for 15 minutes.",
            json!({
                "type": "object",
                "required": ["designId"],
                "properties": {
                    "designId": {"type": "string"},
                    "what": {"type": "string", "maxLength": MAX_WHAT_CHARS, "description": "The thing to locate, e.g. \"the edge that needs a chamfer\". Not a question."},
                    "waitSeconds": {"type": "number", "minimum": 0, "maximum": MAX_WAIT_SECONDS, "default": 0}
                }
            }),
        ),
        mcp_tool(
            "resolve_feedback",
            "Mark feedback comments resolved, with an optional short reply the person sees under each \
             (what you changed, or why not). One reply applies to every id given.",
            json!({
                "type": "object",
                "required": ["designId", "commentIds"],
                "properties": {
                    "designId": {"type": "string"},
                    "commentIds": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": MAX_RESOLVE_IDS},
                    "reply": {"type": "string", "maxLength": MAX_REPLY_CHARS}
                }
            }),
        ),
    ]
}

pub(crate) fn call_tool(
    state: &AppState,
    request: &Request,
    name: &str,
    arguments: &Value,
    progress: Option<ProgressSink<'_>>,
) -> Result<Value, String> {
    match name {
        "list_feedback" => mcp_list(state, request, arguments, progress),
        "request_pointing" => mcp_request(state, request, arguments, progress),
        "resolve_feedback" => mcp_resolve(state, arguments),
        _ => Err(format!("Unknown tool {name:?}.")),
    }
}

fn mcp_list(
    state: &AppState,
    request: &Request,
    arguments: &Value,
    progress: Option<ProgressSink<'_>>,
) -> Result<Value, String> {
    let (id, _) = existing_design(state, arguments)?;
    let filter = status_filter(arguments)?;
    let since = match arguments.get("since").filter(|value| !value.is_null()) {
        None => None,
        Some(value) => Some(value.as_u64().ok_or_else(|| "since must be a non-negative integer cursor.".to_string())?),
    };
    let seconds = wait_seconds(arguments)?;
    let matching = |feedback: &FeedbackState| -> Vec<Comment> {
        feedback
            .comments
            .iter()
            .filter(|comment| matches_status(comment, filter) && since.is_none_or(|since| comment.changed > since))
            .cloned()
            .collect()
    };
    let initial = state.workspaces.feedback_state(&id)?;
    let mut waited = None;
    if seconds > 0 && matching(&initial).is_empty() {
        let _slot = WaitSlot::acquire(&id)?;
        // A request that ends while we wait (the person left comment mode)
        // also returns, so an agent never sits out the full timeout.
        let initial_request = initial.active_request(unix_millis()).map(|request| request.id.clone());
        let end = wait_for(state, &id, seconds, progress, |feedback, now| {
            !matching(feedback).is_empty()
                || initial_request.as_ref().is_some_and(|request| {
                    feedback.active_request(now).is_none_or(|active| &active.id != request)
                })
        })?;
        waited = Some(end);
    }
    let now = unix_millis();
    let feedback = state.workspaces.feedback_state(&id)?;
    let comments = matching(&feedback);
    let cursor = feedback.version.max(since.unwrap_or(0));
    let mut reply = json!({
        "ok": true,
        "designId": id,
        "url": workspace_url(state, request, &id),
        "status": filter,
        "count": comments.len(),
        "openCount": feedback.open_count(),
        "cursor": cursor,
        "comments": comments.iter().map(comment_public).collect::<Vec<_>>(),
        "request": feedback.request_as_of(now).as_ref().map(request_public),
        "browserConnected": browser_connected(state, &id, now)
    });
    if comments.is_empty() {
        reply["message"] = json!(match waited {
            Some(WaitEnd::TimedOut) | Some(WaitEnd::Disconnected) => format!(
                "No feedback yet. Call list_feedback again with since {cursor} and waitSeconds to keep waiting."
            ),
            _ if feedback.active_request(now).is_none() && feedback.request_as_of(now).is_some_and(|request| request.status == "dismissed") =>
                "No feedback: the person dismissed comment mode.".to_string(),
            _ => "No matching feedback.".to_string(),
        });
    }
    Ok(reply)
}

fn mcp_request(
    state: &AppState,
    request: &Request,
    arguments: &Value,
    progress: Option<ProgressSink<'_>>,
) -> Result<Value, String> {
    let (id, _) = existing_design(state, arguments)?;
    let what = clean_text(arguments.get("what"), "what", MAX_WHAT_CHARS, false)?.unwrap_or_default();
    let seconds = wait_seconds(arguments)?;
    let url = workspace_url(state, request, &id);
    let now = unix_millis();
    let connected = browser_connected(state, &id, now);
    // Take the wait slot before starting, so a refused wait does not leave a
    // request behind that nobody collects.
    let slot = if seconds > 0 && connected { Some(WaitSlot::acquire(&id)?) } else { None };
    let nonce = request_nonce();
    let started = state
        .workspaces
        .feedback_mutate(&id, |feedback| Ok(start_request(feedback, what.clone(), now, nonce)))?;
    if !connected {
        return Ok(json!({
            "ok": true,
            "designId": id,
            "requestId": started.id,
            "requestStatus": "active",
            "browserConnected": false,
            "url": url,
            "count": 0,
            "comments": [],
            "message": format!(
                "No browser has this design open, so nobody is in comment mode yet. Give the person this URL: {url} - opening it enters comment mode (the request stays active for 15 minutes). Then call list_feedback with waitSeconds to collect the pins."
            )
        }));
    }
    let mut end = None;
    if let Some(_slot) = slot {
        let request_id = started.id.clone();
        // The answer is the first pin saved for this request; leaving comment
        // mode, or a replacement request, also ends the wait.
        end = Some(wait_for(state, &id, seconds, progress, |feedback, now| {
            feedback.comments.iter().any(|comment| comment.request_id.as_deref() == Some(request_id.as_str()))
                || feedback.active_request(now).is_none_or(|active| active.id != request_id)
        })?);
    }
    let now = unix_millis();
    let feedback = state.workspaces.feedback_state(&id)?;
    let current = feedback
        .request
        .as_ref()
        .filter(|request| request.id == started.id)
        .map(|_| feedback.request_as_of(now))
        .flatten();
    let status = current.as_ref().map_or("superseded".to_string(), |request| request.status.clone());
    let comments: Vec<Value> = feedback
        .comments
        .iter()
        .filter(|comment| comment.request_id.as_deref() == Some(started.id.as_str()))
        .map(comment_public)
        .collect();
    let message = match (end, status.as_str()) {
        (None, _) => "The browser is in comment mode. Call list_feedback with waitSeconds to collect the pins.".to_string(),
        (_, "answered") => "The person pinned the spot; the request is answered and comment mode has closed.".to_string(),
        (_, "dismissed") => "The person dismissed comment mode without pinning anything.".to_string(),
        (_, "active") => "No pin yet; the browser is still in comment mode. Call list_feedback with waitSeconds to keep waiting.".to_string(),
        (_, other) => format!("The request ended ({other}) without a pin."),
    };
    Ok(json!({
        "ok": true,
        "designId": id,
        "requestId": started.id,
        "requestStatus": status,
        "browserConnected": true,
        "url": url,
        "count": comments.len(),
        "comments": comments,
        "cursor": feedback.version,
        "message": message
    }))
}

fn mcp_resolve(state: &AppState, arguments: &Value) -> Result<Value, String> {
    let (id, _) = existing_design(state, arguments)?;
    let ids: Vec<String> = arguments
        .get("commentIds")
        .and_then(Value::as_array)
        .ok_or_else(|| "commentIds must be a list of comment ids.".to_string())?
        .iter()
        .map(|value| value.as_str().map(str::to_string).ok_or_else(|| "commentIds must hold strings.".to_string()))
        .collect::<Result<_, _>>()?;
    if ids.is_empty() || ids.len() > MAX_RESOLVE_IDS {
        return Err(format!("commentIds must name 1 to {MAX_RESOLVE_IDS} comments."));
    }
    let reply = clean_text(arguments.get("reply"), "reply", MAX_REPLY_CHARS, false)?;
    let now = unix_millis();
    let (resolved, missing) = state
        .workspaces
        .feedback_mutate(&id, |feedback| Ok(set_status(feedback, &ids, true, reply.as_deref(), now)))?;
    let open = state.workspaces.feedback_summary(&id)?.1;
    let mut out = json!({"ok": true, "designId": id, "resolved": resolved, "openCount": open});
    if !missing.is_empty() {
        out["notFound"] = json!(missing);
    }
    Ok(out)
}

/// The note other design tools carry while comments are open, or `None`.
pub(crate) fn open_feedback_hint(state: &AppState, arguments: &Value) -> Option<(usize, String)> {
    let id = arguments.get("workspaceId").and_then(Value::as_str).filter(|id| valid_workspace_id(id))?;
    let (_, open) = state.workspaces.feedback_summary(id).ok()?;
    (open > 0).then(|| {
        (
            open,
            format!(
                "{open} open feedback comment{} on the 3D model - call list_feedback.",
                if open == 1 { "" } else { "s" }
            ),
        )
    })
}

// ---------------------------------------------------------------------------
// Browser routes: /api/designs/{id}/feedback[/...]
// ---------------------------------------------------------------------------

pub(crate) fn handle_route(
    stream: &mut TcpStream,
    state: &AppState,
    request: &Request,
    id: &str,
    rest: &str,
) -> io::Result<()> {
    let revision = match state.workspaces.revision_of(id) {
        Ok(Some(revision)) => revision,
        Ok(None) => return respond_error(stream, 404, "Design not found."),
        Err(error) => return respond_error(stream, 500, &error),
    };
    let input = match request.method.as_str() {
        "POST" | "PATCH" => match parse_json_body(&request.body) {
            Ok(value) if value.is_object() => value,
            Ok(_) => return respond_error(stream, 400, "Body must be a JSON object."),
            Err(error) => return respond_error(stream, 400, &error),
        },
        _ => json!({}),
    };
    let now = unix_millis();
    let outcome: Result<Value, (u16, String)> = match (request.method.as_str(), rest) {
        ("GET", "") => state
            .workspaces
            .feedback_state(id)
            .map(|feedback| browser_state(&feedback, now))
            .map_err(|error| (500, error)),
        ("POST", "") => state
            .workspaces
            .feedback_mutate(id, |feedback| add_comment(feedback, &input, revision, now))
            .map(|comment| json!({"ok": true, "comment": comment_public(&comment)}))
            .map_err(|error| (400, error)),
        ("POST", "dismiss") => {
            let request_id = input.get("requestId").and_then(Value::as_str).unwrap_or_default().to_string();
            state
                .workspaces
                .feedback_mutate(id, |feedback| Ok(dismiss_request(feedback, &request_id, now)))
                .map(|ended| json!({"ok": true, "ended": ended}))
                .map_err(|error| (500, error))
        }
        (method @ ("PATCH" | "DELETE"), comment) if !comment.is_empty() && !comment.contains('/') && comment.len() <= 32 => {
            let comment = comment.to_string();
            if method == "DELETE" {
                state
                    .workspaces
                    .feedback_mutate(id, |feedback| Ok(delete_comment(feedback, &comment)))
                    .map_err(|error| (500, error))
                    .and_then(|removed| if removed { Ok(json!({"ok": true})) } else { Err((404, "Comment not found.".into())) })
            } else {
                let resolved = match input.get("status").and_then(Value::as_str) {
                    Some("resolved") => true,
                    Some("open") => false,
                    _ => return respond_error(stream, 400, "status must be open or resolved."),
                };
                state
                    .workspaces
                    .feedback_mutate(id, |feedback| Ok(set_status(feedback, std::slice::from_ref(&comment), resolved, None, now)))
                    .map_err(|error| (500, error))
                    .and_then(|(changed, _)| if changed.is_empty() { Err((404, "Comment not found.".into())) } else { Ok(json!({"ok": true})) })
            }
        }
        _ => Err((404, "Feedback endpoint not found.".into())),
    };
    match outcome {
        Ok(mut value) => {
            // GET carries its own snapshot's version; a second read could pair
            // that list with a newer version and the browser would never refetch.
            if value.get("version").is_none() {
                if let Ok((version, _)) = state.workspaces.feedback_summary(id) {
                    value["version"] = json!(version);
                }
            }
            respond_value(stream, 200, &value)
        }
        Err((status, error)) => respond_error(stream, status, &error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment_input(text: &str) -> Value {
        json!({"text": text, "point": [1.23456, 2.0, 3.0], "normal": [0.0, 0.0, 2.0], "objectId": "lid", "objectName": "Lid", "face": 7, "sourceLine": 12, "revision": 3})
    }

    #[test]
    fn a_comment_is_validated_and_recorded() {
        let mut state = FeedbackState::default();
        let comment = add_comment(&mut state, &comment_input("  round this edge \u{7} "), 5, 100).unwrap();
        assert_eq!(comment.id, "c1");
        assert_eq!(comment.text, "round this edge");
        assert_eq!(comment.point, [1.235, 2.0, 3.0]);
        assert_eq!(comment.normal, Some([0.0, 0.0, 1.0]));
        assert_eq!(comment.revision, 3);
        assert_eq!(state.version, 1);
        let public = comment_public(&comment);
        assert_eq!(public["where"], "on Lid (source line 12) at [1.235, 2, 3], surface facing +Z, revision 3");
        // A revision from the future is clamped to the current one.
        let mut future = comment_input("x");
        future["revision"] = json!(99);
        assert_eq!(add_comment(&mut state, &future, 5, 100).unwrap().revision, 5);
        for bad in [
            json!({"text": "x"}),
            json!({"text": "x", "point": [0, 0]}),
            json!({"text": "x", "point": [0, 0, "a"]}),
            json!({"text": "x".repeat(MAX_TEXT_CHARS + 1), "point": [0, 0, 0]}),
            json!({"text": "x", "point": [0, 0, 0], "face": -1}),
        ] {
            assert!(add_comment(&mut state, &bad, 5, 100).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_full_design_drops_resolved_comments_first_and_then_refuses() {
        let mut state = FeedbackState::default();
        for _ in 0..MAX_COMMENTS {
            add_comment(&mut state, &comment_input("x"), 1, 1).unwrap();
        }
        assert!(add_comment(&mut state, &comment_input("x"), 1, 1).is_err());
        set_status(&mut state, &["c5".into()], true, Some("done"), 2);
        let added = add_comment(&mut state, &comment_input("y"), 1, 3).unwrap();
        assert_eq!(added.id, format!("c{}", MAX_COMMENTS + 1));
        assert_eq!(state.comments.len(), MAX_COMMENTS);
        assert!(state.comments.iter().all(|comment| comment.id != "c5"));
    }

    #[test]
    fn requests_supersede_end_and_expire() {
        let mut state = FeedbackState::default();
        let first = start_request(&mut state, "where?".into(), 1000, 1);
        let second = start_request(&mut state, "here?".into(), 2000, 2);
        assert_ne!(first.id, second.id);
        assert!(!dismiss_request(&mut state, &first.id, 2001));
        // Comments tie to the live request only.
        let mut input = comment_input("there");
        input["requestId"] = json!(second.id);
        assert_eq!(add_comment(&mut state, &input, 1, 2002).unwrap().request_id.as_deref(), Some(second.id.as_str()));
        input["requestId"] = json!(first.id);
        assert_eq!(add_comment(&mut state, &input, 1, 2003).unwrap().request_id, None);
        // The pin answered the request, so it is no longer there to dismiss.
        assert_eq!(state.request.as_ref().unwrap().status, "answered");
        assert_eq!(state.comments[0].request_what.as_deref(), Some("here?"));
        assert!(!dismiss_request(&mut state, &second.id, 2004));
        assert!(state.active_request(2005).is_none());
        let fourth = start_request(&mut state, "where now?".into(), 2500, 4);
        assert!(dismiss_request(&mut state, &fourth.id, 2501));
        assert_eq!(state.request.as_ref().unwrap().status, "dismissed");
        let third = start_request(&mut state, "again".into(), 3000, 3);
        assert!(state.active_request(3000 + REQUEST_TTL_MS - 1).is_some());
        assert!(state.active_request(3000 + REQUEST_TTL_MS).is_none());
        assert_eq!(state.request_as_of(3000 + REQUEST_TTL_MS).unwrap().status, "expired");
        assert!(!dismiss_request(&mut state, &third.id, 3000 + REQUEST_TTL_MS));
    }

    #[test]
    fn resolving_reports_missing_ids_and_only_bumps_on_change() {
        let mut state = FeedbackState::default();
        add_comment(&mut state, &comment_input("a"), 1, 1).unwrap();
        let version = state.version;
        let (changed, missing) = set_status(&mut state, &["c9".into()], true, None, 2);
        assert!(changed.is_empty());
        assert_eq!(missing, vec!["c9".to_string()]);
        assert_eq!(state.version, version);
        let (changed, _) = set_status(&mut state, &["c1".into()], true, Some("Rounded to 2 mm."), 3);
        assert_eq!(changed, vec!["c1".to_string()]);
        assert_eq!(state.comments[0].reply.as_deref(), Some("Rounded to 2 mm."));
        assert_eq!(state.open_count(), 0);
        assert!(delete_comment(&mut state, "c1"));
        assert!(!delete_comment(&mut state, "c1"));
    }

    #[test]
    fn a_retried_save_is_not_a_second_comment_and_reopening_moves_the_cursor() {
        let mut state = FeedbackState::default();
        let mut input = comment_input("once");
        input["clientKey"] = json!("draft-1a2b");
        let first = add_comment(&mut state, &input, 1, 1).unwrap();
        let again = add_comment(&mut state, &input, 1, 2).unwrap();
        assert_eq!(first.id, again.id);
        assert_eq!(state.comments.len(), 1);
        let cursor = state.version;
        set_status(&mut state, &["c1".into()], true, None, 3);
        set_status(&mut state, &["c1".into()], false, None, 4);
        assert!(state.comments[0].changed > cursor, "a reopened comment is newer than the cursor");
        assert!(comment_public(&state.comments[0]).get("reply").is_none(), "nulls are omitted");
    }

    #[test]
    fn wait_seconds_are_capped() {
        assert_eq!(wait_seconds(&json!({})).unwrap(), 0);
        assert_eq!(wait_seconds(&json!({"waitSeconds": 5.2})).unwrap(), 6);
        assert_eq!(wait_seconds(&json!({"wait_seconds": 999})).unwrap(), MAX_WAIT_SECONDS);
        assert!(wait_seconds(&json!({"waitSeconds": -1})).is_err());
        assert!(tool_may_wait("list_feedback", &json!({"waitSeconds": 1})));
        assert!(!tool_may_wait("resolve_feedback", &json!({"waitSeconds": 1})));
    }

    #[test]
    fn wait_slots_are_capped_per_design() {
        let id = "feedback-slot-test-design";
        let a = WaitSlot::acquire(id).unwrap();
        let b = WaitSlot::acquire(id).unwrap();
        assert!(WaitSlot::acquire(id).is_err());
        drop(a);
        let c = WaitSlot::acquire(id).unwrap();
        drop((b, c));
    }

    fn test_state() -> (AppState, PathBuf) {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let root = std::env::temp_dir().join(format!(
            "reopenscad-feedback-state-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let store = WorkspaceStore::open(root.clone()).unwrap();
        let state = AppState {
            web_root: PathBuf::new(),
            render_queue: RenderQueue::new(MAX_CONCURRENT_JOBS, MAX_QUEUED_JOBS, MAX_QUEUED_PER_CLIENT, MAX_QUEUE_WAIT),
            poll_limiter: JobLimiter::new(MAX_CONCURRENT_POLLS),
            rate_limiter: RateLimiter::new(),
            cancellations: Arc::new(Mutex::new(HashMap::new())),
            workspaces: Store::Filesystem(store),
            public_origin: Some("http://localhost:5173".into()),
        };
        (state, root)
    }

    fn mcp_request() -> Request {
        Request { method: "POST".into(), path: "/mcp".into(), query: String::new(), headers: vec![], body: vec![] }
    }

    #[test]
    fn request_pointing_returns_the_first_pin() {
        let (state, root) = test_state();
        let workspace = state.workspaces.create("cube(10);".into(), None).unwrap();
        let id = workspace.id.clone();
        let request = mcp_request();

        // No browser: the request is recorded and the URL handed back at once.
        let result = crate::mcp_call_tool(&state, &request, "request_pointing", json!({"designId": id, "waitSeconds": 30}));
        let structured = &result["structuredContent"];
        assert_eq!(structured["browserConnected"], false, "{result}");
        assert_eq!(structured["url"], format!("http://localhost:5173/designs/{id}"));
        assert!(state.workspaces.feedback_state(&id).unwrap().active_request(unix_millis()).is_some());

        // A browser polls; the call waits for the first pin of its request.
        state.workspaces.note_viewer(&id);
        let started_request = |what: &str| {
            let started = Instant::now();
            loop {
                let feedback = state.workspaces.feedback_state(&id).unwrap();
                if let Some(active) = feedback.active_request(unix_millis()).filter(|active| active.what == what) {
                    break active.id.clone();
                }
                assert!(started.elapsed() < Duration::from_secs(5));
                thread::sleep(Duration::from_millis(20));
            }
        };
        let reply = thread::scope(|scope| {
            let waiting = scope.spawn(|| {
                crate::mcp_call_tool(&state, &request, "request_pointing", json!({"designId": id, "what": "the edge to round", "waitSeconds": 20}))
            });
            let request_id = started_request("the edge to round");
            // An unrelated comment does not answer it; a bare pin for it does.
            let unrelated = json!({"text": "unrelated", "point": [1, 1, 10]});
            state.workspaces.feedback_mutate(&id, |feedback| add_comment(feedback, &unrelated, 1, unix_millis())).unwrap();
            let pin = json!({"point": [5, 5, 10], "normal": [0, 0, 1], "objectId": "object-1", "requestId": request_id});
            state.workspaces.feedback_mutate(&id, |feedback| add_comment(feedback, &pin, 1, unix_millis())).unwrap();
            waiting.join().unwrap()
        });
        let structured = &reply["structuredContent"];
        assert_eq!(structured["requestStatus"], "answered", "the pin answers it: {reply}");
        assert_eq!(structured["comments"][0]["answers"], "the edge to round");
        assert!(state.workspaces.feedback_state(&id).unwrap().active_request(unix_millis()).is_none());
        assert_eq!(structured["count"], 1);
        assert_eq!(structured["comments"][0]["text"], "");
        assert_eq!(structured["comments"][0]["objectId"], "object-1");

        // Leaving comment mode ends a wait with no pins.
        let dismissed = thread::scope(|scope| {
            let waiting = scope.spawn(|| crate::mcp_call_tool(&state, &request, "request_pointing", json!({"designId": id, "what": "the hole", "waitSeconds": 20})));
            let request_id = started_request("the hole");
            state.workspaces.feedback_mutate(&id, |feedback| Ok(dismiss_request(feedback, &request_id, unix_millis()))).unwrap();
            waiting.join().unwrap()
        });
        assert_eq!(dismissed["structuredContent"]["requestStatus"], "dismissed", "{dismissed}");
        assert_eq!(dismissed["structuredContent"]["count"], 0);
        assert!(dismissed["structuredContent"]["message"].as_str().unwrap().contains("dismissed"));

        // Other design tools now point at the open comments.
        let code = crate::mcp_call_tool(&state, &request, "get_design_code", json!({"designId": id}));
        assert_eq!(code["structuredContent"]["openFeedback"], 2, "{code}");
        assert!(code["content"].as_array().unwrap().iter().any(|item| item["text"].as_str().unwrap_or("").contains("call list_feedback")));

        // list -> resolve -> list.
        let listed = crate::mcp_call_tool(&state, &request, "list_feedback", json!({"designId": id}));
        assert_eq!(listed["structuredContent"]["count"], 2);
        let cursor = listed["structuredContent"]["cursor"].as_u64().unwrap();
        let resolved = crate::mcp_call_tool(&state, &request, "resolve_feedback", json!({"designId": id, "commentIds": ["c1", "c404"], "reply": "Rounded."}));
        assert_eq!(resolved["structuredContent"]["resolved"], json!(["c1"]));
        assert_eq!(resolved["structuredContent"]["notFound"], json!(["c404"]));
        let all = crate::mcp_call_tool(&state, &request, "list_feedback", json!({"designId": id, "status": "resolved"}));
        assert_eq!(all["structuredContent"]["comments"][0]["reply"], "Rounded.");

        // Waiting past the cursor times out with a call-again message.
        let started = Instant::now();
        let empty = crate::mcp_call_tool(&state, &request, "list_feedback", json!({"designId": id, "since": cursor, "waitSeconds": 1}));
        assert!(started.elapsed() >= Duration::from_millis(900));
        assert!(empty["structuredContent"]["message"].as_str().unwrap().contains("call list_feedback again") || empty["structuredContent"]["message"].as_str().unwrap().contains("Call list_feedback again"), "{empty}");
        assert_eq!(empty["isError"], false);

        // Unknown designs are errors.
        let missing = crate::mcp_call_tool(&state, &request, "list_feedback", json!({"designId": "no-such-design-zz"}));
        assert_eq!(missing["isError"], true);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn the_filesystem_sidecar_round_trips() {
        let root = std::env::temp_dir().join(format!("reopenscad-feedback-{}", unix_millis()));
        fs::create_dir_all(&root).unwrap();
        let store = FsFeedback::new(root.clone());
        store.mutate("design-a", |state| add_comment(state, &comment_input("hi"), 1, 1)).unwrap();
        // A failed change writes nothing.
        assert!(store.mutate("design-a", |state| add_comment(state, &json!({}), 1, 1)).is_err());
        let fresh = FsFeedback::new(root.clone());
        assert_eq!(fresh.state("design-a").unwrap().comments.len(), 1);
        assert_eq!(fresh.summary("design-a").unwrap(), (1, 1));
        fresh.remove("design-a");
        assert!(!fresh.path("design-a").exists());
        let _ = fs::remove_dir_all(root);
    }
}
