//! In-flight job registry for evaluator-running MCP tool calls.
//!
//! An agent whose client times out a slow call usually retries it unchanged.
//! Before this, every retry was a fresh job: the abandoned one was cancelled
//! when its socket closed (`ClientWatch`), so a model slower than the client's
//! timeout never finished, and each retry cost a full queue place. Here:
//!
//! * **Coalescing.** A call is keyed by everything its reply depends on (tool,
//!   arguments, the design's current revision, the asset digest, the host the
//!   reply's URLs are built from). A call whose key matches a job that is
//!   queued or running attaches to that job and gets its reply; it never takes
//!   a queue place of its own.
//!   Settings that change without a revision bump (tolerance, plate, name)
//!   are part of a read's key; a reply with any partial or retryable marker
//!   is never cached; a cached reply is marked `replayed: true`.
//! * **Short result cache.** A finished job's reply is kept for
//!   [`RESULT_TTL`] (bounded by count and bytes), so a retry that arrives after
//!   the job finished gets the reply at once. Writes carry `expectedRevision`
//!   in their key, so replaying a write returns the original result instead of
//!   a revision conflict (idempotent retry).
//! * **Detached from the socket.** A job runs under its own cancellation flag
//!   rather than its first caller's disconnect flag. It is cancelled when every
//!   attached caller has gone and none came back within [`orphan_grace`]
//!   (a retry usually follows a timeout within seconds), at once when the
//!   last caller to leave sent `notifications/cancelled` (scoped by the
//!   unguessable MCP session id, never the client address), by the `jobs` tool, or by its
//!   own deadline as before.
//! * **Listing and cancel.** Jobs carry an id and the design they belong to;
//!   the `jobs` tool lists a design's jobs and cancels by id. The design id is
//!   the capability: a job is visible only to callers holding its design id.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// How long a finished job's reply is served to identical calls.
pub const RESULT_TTL: Duration = Duration::from_secs(180);
/// Ceiling on cached replies, by count and by serialised size.
pub const MAX_CACHED_RESULTS: usize = 64;
pub const MAX_CACHED_BYTES: usize = 64 * 1024 * 1024;
/// Finished jobs (cached or not) kept for listing.
const MAX_RECENT: usize = 128;
/// Default time a job with no caller left keeps running for a retry to attach.
pub const DEFAULT_ORPHAN_GRACE: Duration = Duration::from_secs(30);
const REAPER_INTERVAL: Duration = Duration::from_millis(100);
const WAIT_POLL: Duration = Duration::from_millis(100);
/// Longest a follower waits for a job it attached to. Every job is bounded by
/// its own queue wait and deadlines well inside this; it is a backstop so no
/// caller can hang on a job that never publishes.
pub const MAX_FOLLOW_WAIT: Duration = Duration::from_secs(15 * 60);
/// Most `notifications/cancelled` targets tracked at once.
const MAX_RPC_TARGETS: usize = 4096;

/// `REOPENSCAD_JOB_ORPHAN_GRACE_MS`, else [`DEFAULT_ORPHAN_GRACE`].
pub fn orphan_grace() -> Duration {
    std::env::var("REOPENSCAD_JOB_ORPHAN_GRACE_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_ORPHAN_GRACE)
}

/// The process-wide registry.
pub fn registry() -> &'static JobRegistry {
    static REGISTRY: OnceLock<JobRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| JobRegistry::new(orphan_grace(), RESULT_TTL))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Queued,
    Running,
    Done,
    Cancelled,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Queued => "queued",
            Phase::Running => "running",
            Phase::Done => "done",
            Phase::Cancelled => "cancelled",
        }
    }
}

/// What a call is, for the registry.
pub struct JobSpec {
    /// Hash of everything the reply depends on.
    pub key: [u8; 32],
    pub tool: String,
    pub design: Option<String>,
}

/// Who is waiting: the caller's disconnect flag (from its `ClientWatch`) and
/// the scope + JSON-RPC id `notifications/cancelled` names it by. The scope
/// must be unguessable by other clients (an MCP session id, not an address):
/// `None` means the call cannot be withdrawn by notification.
#[derive(Default)]
pub struct Caller {
    pub gone: Option<Arc<AtomicBool>>,
    pub rpc: Option<String>,
}

/// Why a waiter stopped waiting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Departure {
    /// `notifications/cancelled` named its request.
    Withdrawn,
    /// Its client disconnected (it may retry).
    Disconnected,
}

struct Waiter {
    token: u64,
    gone: Option<Arc<AtomicBool>>,
    /// Set (under the job's lock) by `notifications/cancelled`.
    withdrawn: bool,
    /// Its departure was recorded in `JobState::last_departure`.
    departed: bool,
}

impl Waiter {
    fn alive(&self) -> bool {
        !self.withdrawn && !self.gone.as_ref().is_some_and(|gone| gone.load(Ordering::Acquire))
    }
}

struct JobState {
    phase: Phase,
    started: Option<Instant>,
    result: Option<Arc<Value>>,
    waiters: Vec<Waiter>,
    orphaned_since: Option<Instant>,
    cancel_reason: Option<String>,
    /// How the most recent waiter to leave left, recorded under this lock in
    /// the order departures were observed. Decides, once nobody is left,
    /// between cancelling at once (explicit cancel) and the orphan grace.
    last_departure: Option<Departure>,
}

impl JobState {
    /// Records every waiter whose client disconnected since the last sweep.
    fn sweep_disconnects(&mut self) {
        let mut departed = false;
        for waiter in self.waiters.iter_mut() {
            if !waiter.departed && !waiter.alive() {
                waiter.departed = true;
                departed = true;
            }
        }
        if departed {
            self.last_departure = Some(Departure::Disconnected);
        }
    }

    /// Marks waiter `token` withdrawn by `notifications/cancelled`.
    fn withdraw(&mut self, token: u64) -> bool {
        // A disconnect that happened before this withdrawal but was not yet
        // seen is recorded first, so the withdrawal counts as the later one.
        self.sweep_disconnects();
        let Some(waiter) = self.waiters.iter_mut().find(|waiter| waiter.token == token) else {
            return false;
        };
        if waiter.departed {
            return false;
        }
        waiter.withdrawn = true;
        waiter.departed = true;
        self.last_departure = Some(Departure::Withdrawn);
        true
    }
}

pub struct Job {
    id: String,
    key: [u8; 32],
    tool: String,
    design: Option<String>,
    created: Instant,
    /// The job's own kernel cancellation flag.
    cancel: Arc<AtomicBool>,
    state: Mutex<JobState>,
    finished: Condvar,
}

impl Job {
    fn request_cancel(&self, state: &mut JobState, reason: &str) {
        if state.cancel_reason.is_none() {
            state.cancel_reason = Some(reason.to_string());
        }
        self.cancel.store(true, Ordering::Release);
    }

    fn summary(&self, state: &JobState) -> Value {
        let mut value = json!({
            "id": self.id,
            "tool": self.tool,
            "state": if state.phase == Phase::Running && self.cancel.load(Ordering::Acquire) { "cancelling" } else { state.phase.name() },
            "ageMs": self.created.elapsed().as_millis() as u64,
            "waiters": state.waiters.iter().filter(|waiter| waiter.alive()).count(),
        });
        if let Some(started) = state.started {
            value["runningMs"] = json!(started.elapsed().as_millis() as u64);
        }
        if let Some(reason) = &state.cancel_reason {
            value["cancelReason"] = json!(reason);
        }
        value
    }
}

/// A handle the job body uses to report that it left the queue.
pub struct JobHandle<'a> {
    job: &'a Job,
}

impl JobHandle<'_> {
    pub fn cancel_flag(&self) -> &AtomicBool {
        &self.job.cancel
    }

    pub fn set_running(&self) {
        if let Ok(mut state) = self.job.state.lock() {
            if state.phase == Phase::Queued {
                state.phase = Phase::Running;
                state.started = Some(Instant::now());
            }
        }
    }
}

struct Finished {
    id: String,
    key: [u8; 32],
    tool: String,
    design: Option<String>,
    created: Instant,
    finished: Instant,
    ran: Option<Duration>,
    phase: Phase,
    cancel_reason: Option<String>,
    /// Present while the reply may be served to identical calls.
    result: Option<(Arc<Value>, usize)>,
}

#[derive(Default)]
struct Registry {
    live: HashMap<[u8; 32], Arc<Job>>,
    recent: VecDeque<Finished>,
    cached_bytes: usize,
    /// `notifications/cancelled` targets: caller scope + request id, to the
    /// job and waiter token it names.
    rpc: HashMap<String, Vec<(Weak<Job>, u64)>>,
}

impl Registry {
    fn cached_count(&self) -> usize {
        self.recent.iter().filter(|entry| entry.result.is_some()).count()
    }

    fn prune(&mut self, ttl: Duration) {
        let now = Instant::now();
        for entry in self.recent.iter_mut() {
            if entry.result.is_some() && now.duration_since(entry.finished) >= ttl {
                if let Some((_, bytes)) = entry.result.take() {
                    self.cached_bytes = self.cached_bytes.saturating_sub(bytes);
                }
            }
        }
        // Oldest results out first until both ceilings hold.
        while self.cached_count() > MAX_CACHED_RESULTS || self.cached_bytes > MAX_CACHED_BYTES {
            let Some(entry) = self.recent.iter_mut().find(|entry| entry.result.is_some()) else {
                break;
            };
            if let Some((_, bytes)) = entry.result.take() {
                self.cached_bytes = self.cached_bytes.saturating_sub(bytes);
            }
        }
        while self.recent.len() > MAX_RECENT
            || self.recent.front().is_some_and(|entry| entry.result.is_none() && now.duration_since(entry.finished) >= ttl)
        {
            if let Some(entry) = self.recent.pop_front() {
                if let Some((_, bytes)) = entry.result {
                    self.cached_bytes = self.cached_bytes.saturating_sub(bytes);
                }
            }
        }
    }
}

struct Inner {
    state: Mutex<Registry>,
    grace: Duration,
    ttl: Duration,
    /// See [`MAX_FOLLOW_WAIT`].
    follow_limit: Duration,
    next: AtomicU64,
    reaper: OnceLock<()>,
}

pub struct JobRegistry {
    inner: Arc<Inner>,
}

/// How a call was answered, for logs and tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Served {
    /// This call ran the job.
    Ran,
    /// Attached to an identical job already queued or running.
    Attached,
    /// An identical job had finished within the TTL.
    Cached,
}

impl JobRegistry {
    pub fn new(grace: Duration, ttl: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(Registry::default()),
                grace,
                ttl,
                follow_limit: MAX_FOLLOW_WAIT,
                next: AtomicU64::new(1),
                reaper: OnceLock::new(),
            }),
        }
    }

    fn start_reaper(&self) {
        self.inner.reaper.get_or_init(|| {
            let weak: Weak<Inner> = Arc::downgrade(&self.inner);
            let _ = thread::Builder::new().name("reopenscad-job-reaper".into()).spawn(move || loop {
                thread::sleep(REAPER_INTERVAL);
                let Some(inner) = weak.upgrade() else { return };
                reap(&inner);
            });
        });
    }

    /// Runs `work` as the job for `spec`, or answers from an identical job.
    ///
    /// `cacheable` decides whether a finished reply may be served to later
    /// identical calls (a transient queue refusal must not be). `cancelled`
    /// builds the reply for a caller that withdrew, or for a job cancelled
    /// before it produced one.
    pub fn run(
        &self,
        spec: JobSpec,
        caller: Caller,
        work: impl FnOnce(&JobHandle<'_>) -> Value,
        cacheable: impl Fn(&Value) -> bool,
        cancelled: impl Fn(&str) -> Value,
    ) -> (Value, Served) {
        self.start_reaper();
        let token = self.inner.next.fetch_add(1, Ordering::Relaxed);
        let waiter = Waiter { token, gone: caller.gone.clone(), withdrawn: false, departed: false };
        let (job, leader) = {
            let Ok(mut registry) = self.inner.state.lock() else {
                return (work(&JobHandle { job: &detached_job(&spec) }), Served::Ran);
            };
            registry.prune(self.inner.ttl);
            if let Some(hit) = registry
                .recent
                .iter()
                .rev()
                .find(|entry| entry.key == spec.key)
                .and_then(|entry| entry.result.as_ref())
            {
                return ((*hit.0).clone(), Served::Cached);
            }
            // A job already cancelling cannot answer a new caller; it gets a
            // fresh job (the old one leaves `live` only if still the entry).
            let (job, leader) = match registry.live.get(&spec.key).filter(|job| !job.cancel.load(Ordering::Acquire)) {
                Some(job) => {
                    let job = Arc::clone(job);
                    if let Ok(mut state) = job.state.lock() {
                        state.waiters.push(waiter);
                        state.orphaned_since = None;
                    }
                    (job, false)
                }
                None => {
                    let number = self.inner.next.fetch_add(1, Ordering::Relaxed);
                    let job = Arc::new(Job {
                        id: format!("job-{number}-{:02x}{:02x}", spec.key[0], spec.key[1]),
                        key: spec.key,
                        tool: spec.tool.clone(),
                        design: spec.design.clone(),
                        created: Instant::now(),
                        cancel: Arc::new(AtomicBool::new(false)),
                        state: Mutex::new(JobState {
                            phase: Phase::Queued,
                            started: None,
                            result: None,
                            waiters: vec![waiter],
                            orphaned_since: None,
                            cancel_reason: None,
                            last_departure: None,
                        }),
                        finished: Condvar::new(),
                    });
                    registry.live.insert(spec.key, Arc::clone(&job));
                    (job, true)
                }
            };
            if let Some(rpc) = &caller.rpc {
                // Bounded: a flood of distinct ids stops registering new
                // targets (those calls just cannot be withdrawn by notification).
                if registry.rpc.len() < MAX_RPC_TARGETS || registry.rpc.contains_key(rpc) {
                    registry.rpc.entry(rpc.clone()).or_default().push((Arc::downgrade(&job), token));
                }
            }
            (job, leader)
        };
        /// Drops this call's `notifications/cancelled` target, also when the
        /// job body panics.
        struct Unregister<'a> {
            inner: &'a Inner,
            rpc: Option<String>,
            token: u64,
        }
        impl Drop for Unregister<'_> {
            fn drop(&mut self) {
                let Some(rpc) = &self.rpc else { return };
                let mut registry = self.inner.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(targets) = registry.rpc.get_mut(rpc) {
                    targets.retain(|(_, target)| *target != self.token);
                    if targets.is_empty() {
                        registry.rpc.remove(rpc);
                    }
                }
            }
        }
        let _unregister = Unregister { inner: &self.inner, rpc: caller.rpc, token };
        if leader {
            (self.lead(&job, work, &cacheable, &cancelled), Served::Ran)
        } else {
            (self.follow(&job, token, &cancelled), Served::Attached)
        }
    }

    fn lead(
        &self,
        job: &Arc<Job>,
        work: impl FnOnce(&JobHandle<'_>) -> Value,
        cacheable: &dyn Fn(&Value) -> bool,
        cancelled: &dyn Fn(&str) -> Value,
    ) -> Value {
        /// Publishes a reply even if `work` panics, so followers never hang.
        struct Publish<'a> {
            registry: &'a JobRegistry,
            job: &'a Arc<Job>,
            result: Option<Value>,
            cacheable: bool,
        }
        impl Drop for Publish<'_> {
            fn drop(&mut self) {
                let result = Arc::new(self.result.take().unwrap_or_else(|| {
                    json!({"content": [{"type": "text", "text": "The job failed unexpectedly."}],
                           "structuredContent": {"ok": false, "error": "The job failed unexpectedly."},
                           "isError": true})
                }));
                self.registry.finish(self.job, result, self.cacheable);
            }
        }
        let mut publish = Publish { registry: self, job, result: None, cacheable: false };
        let value = {
            // The job's flag replaces the caller's disconnect flag: the job
            // outlives its first caller's socket (see the module docs).
            let _scope = crate::csg::bsp::isolated_cancellation_scope(Arc::clone(&job.cancel));
            work(&JobHandle { job })
        };
        let was_cancelled = job.cancel.load(Ordering::Acquire);
        let value = if was_cancelled && value["isError"] == json!(true) {
            let reason = job.state.lock().ok().and_then(|state| state.cancel_reason.clone()).unwrap_or_default();
            cancelled(&format!("Job {} was cancelled ({reason}).", job.id))
        } else {
            value
        };
        publish.cacheable = !was_cancelled && cacheable(&value);
        publish.result = Some(value.clone());
        drop(publish);
        value
    }

    fn follow(&self, job: &Arc<Job>, token: u64, cancelled: &dyn Fn(&str) -> Value) -> Value {
        let Ok(mut state) = job.state.lock() else {
            return cancelled("The job registry is unavailable.");
        };
        let give_up = Instant::now() + self.inner.follow_limit;
        loop {
            if let Some(result) = &state.result {
                return (**result).clone();
            }
            let alive = state.waiters.iter().find(|waiter| waiter.token == token).is_some_and(Waiter::alive);
            if !alive {
                state.sweep_disconnects();
                state.waiters.retain(|waiter| waiter.token != token);
                return cancelled("This call was withdrawn; the job keeps running for any other caller.");
            }
            if Instant::now() >= give_up {
                // Leaving as a disconnect: the job keeps its grace for others.
                if let Some(waiter) = state.waiters.iter_mut().find(|waiter| waiter.token == token) {
                    waiter.departed = true;
                }
                state.waiters.retain(|waiter| waiter.token != token);
                state.last_departure = Some(Departure::Disconnected);
                return cancelled("Gave up waiting for the job; call again to attach to it or get its reply.");
            }
            state = match job.finished.wait_timeout(state, WAIT_POLL) {
                Ok((guard, _)) => guard,
                Err(_) => return cancelled("The job registry is unavailable."),
            };
        }
    }

    fn finish(&self, job: &Arc<Job>, result: Arc<Value>, cacheable: bool) {
        let (phase, ran, reason) = {
            let mut state = job.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            state.phase = if job.cancel.load(Ordering::Acquire) { Phase::Cancelled } else { Phase::Done };
            state.result = Some(Arc::clone(&result));
            (state.phase, state.started.map(|started| started.elapsed()), state.cancel_reason.clone())
        };
        job.finished.notify_all();
        let bytes = if cacheable { serde_json::to_vec(&*result).map_or(usize::MAX, |bytes| bytes.len()) } else { 0 };
        let mut registry = self.inner.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if registry.live.get(&job.key).is_some_and(|live| Arc::ptr_eq(live, job)) {
            registry.live.remove(&job.key);
        }
        let keep = cacheable && bytes <= MAX_CACHED_BYTES;
        if keep {
            registry.cached_bytes += bytes;
        }
        registry.recent.push_back(Finished {
            id: job.id.clone(),
            key: job.key,
            tool: job.tool.clone(),
            design: job.design.clone(),
            created: job.created,
            finished: Instant::now(),
            ran,
            phase,
            cancel_reason: reason,
            result: keep.then_some((result, bytes)),
        });
        registry.prune(self.inner.ttl);
    }

    /// Withdraws the caller `rpc` names from whatever job it waits on. A job
    /// left with no caller is cancelled at once. Returns how many were found.
    pub fn withdraw_rpc(&self, rpc: &str) -> usize {
        let targets = match self.inner.state.lock() {
            Ok(registry) => registry.rpc.get(rpc).cloned().unwrap_or_default(),
            Err(_) => return 0,
        };
        let mut found = 0;
        for (job, token) in &targets {
            let Some(job) = job.upgrade() else { continue };
            let Ok(mut state) = job.state.lock() else { continue };
            if state.withdraw(*token) {
                found += 1;
            }
            drop(state);
            job.finished.notify_all();
        }
        if found > 0 {
            reap(&self.inner);
        }
        found
    }

    /// The jobs of `design`: live first, then recently finished.
    pub fn list(&self, design: &str) -> Vec<Value> {
        let Ok(mut registry) = self.inner.state.lock() else {
            return Vec::new();
        };
        registry.prune(self.inner.ttl);
        let mut live: Vec<&Arc<Job>> = registry.live.values().filter(|job| job.design.as_deref() == Some(design)).collect();
        live.sort_by_key(|job| job.created);
        let mut jobs: Vec<Value> = live
            .into_iter()
            .filter_map(|job| job.state.lock().ok().map(|state| job.summary(&state)))
            .collect();
        jobs.extend(registry.recent.iter().rev().filter(|entry| entry.design.as_deref() == Some(design)).map(|entry| {
            let mut value = json!({
                "id": entry.id,
                "tool": entry.tool,
                "state": entry.phase.name(),
                "ageMs": entry.created.elapsed().as_millis() as u64,
                "finishedMsAgo": entry.finished.elapsed().as_millis() as u64,
                "resultCached": entry.result.is_some(),
            });
            if let Some(ran) = entry.ran {
                value["runningMs"] = json!(ran.as_millis() as u64);
            }
            if let Some(reason) = &entry.cancel_reason {
                value["cancelReason"] = json!(reason);
            }
            value
        }));
        jobs
    }

    /// Cancels live job `id` of `design`. `Err` when there is no such job.
    pub fn cancel(&self, design: &str, id: &str) -> Result<Value, String> {
        let registry = self.inner.state.lock().map_err(|_| "The job registry is unavailable.".to_string())?;
        if let Some(job) = registry.live.values().find(|job| job.id == id && job.design.as_deref() == Some(design)) {
            let mut state = job.state.lock().map_err(|_| "The job registry is unavailable.".to_string())?;
            job.request_cancel(&mut state, "cancelled with the jobs tool");
            return Ok(job.summary(&state));
        }
        if let Some(entry) = registry.recent.iter().find(|entry| entry.id == id && entry.design.as_deref() == Some(design)) {
            return Err(format!("Job {id} already finished ({}); nothing to cancel.", entry.phase.name()));
        }
        Err(format!("No job {id} on this design."))
    }

    #[cfg(test)]
    fn with_follow_limit(mut self, limit: Duration) -> Self {
        Arc::get_mut(&mut self.inner).expect("not shared yet").follow_limit = limit;
        self
    }

    #[cfg(test)]
    pub fn live_count(&self) -> usize {
        self.inner.state.lock().map_or(0, |registry| registry.live.len())
    }
}

/// A stand-in job for when the registry lock is poisoned: the call runs
/// unregistered rather than failing.
fn detached_job(spec: &JobSpec) -> Job {
    Job {
        id: "job-unregistered".into(),
        key: spec.key,
        tool: spec.tool.clone(),
        design: spec.design.clone(),
        created: Instant::now(),
        cancel: Arc::new(AtomicBool::new(false)),
        state: Mutex::new(JobState {
            phase: Phase::Queued,
            started: None,
            result: None,
            waiters: Vec::new(),
            orphaned_since: None,
            cancel_reason: None,
            last_departure: None,
        }),
        finished: Condvar::new(),
    }
}

/// Cancels live jobs nobody waits for any more: at once when the last caller
/// to leave withdrew explicitly, after the grace period when it disconnected
/// (whatever earlier callers did).
fn reap(inner: &Inner) {
    let jobs: Vec<Arc<Job>> = match inner.state.lock() {
        Ok(registry) => registry.live.values().cloned().collect(),
        Err(_) => return,
    };
    let now = Instant::now();
    for job in jobs {
        let Ok(mut state) = job.state.lock() else { continue };
        if state.result.is_some() || job.cancel.load(Ordering::Acquire) {
            continue;
        }
        state.sweep_disconnects();
        if state.waiters.iter().any(Waiter::alive) {
            state.orphaned_since = None;
            continue;
        }
        if state.last_departure == Some(Departure::Withdrawn) {
            job.request_cancel(&mut state, "every caller sent notifications/cancelled");
            continue;
        }
        let since = *state.orphaned_since.get_or_insert(now);
        if now.duration_since(since) >= inner.grace {
            let reason = format!("every caller disconnected and none retried within {} s", inner.grace.as_secs_f64());
            job.request_cancel(&mut state, &reason);
        }
    }
}

/// SHA-256 over length-prefixed parts, so no two part lists collide.
pub fn key_of(parts: &[&[u8]]) -> [u8; 32] {
    let mut bytes = Vec::new();
    for part in parts {
        bytes.extend_from_slice(&(part.len() as u64).to_be_bytes());
        bytes.extend_from_slice(part);
    }
    crate::rendercache::sha256(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn spec(key: u8, design: &str) -> JobSpec {
        JobSpec { key: [key; 32], tool: "evaluate".into(), design: Some(design.into()) }
    }

    fn reply(text: &str) -> Value {
        json!({"content": [{"type": "text", "text": text}], "isError": false})
    }

    fn cancelled(message: &str) -> Value {
        json!({"content": [{"type": "text", "text": message}], "isError": true})
    }

    /// Waits for `condition`, failing after a few seconds.
    fn eventually(condition: impl Fn() -> bool) {
        let give_up = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < give_up, "condition never held");
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// A job body that runs until `release` is set (or it is cancelled).
    fn blocking_work(runs: &AtomicUsize, release: &AtomicBool, handle: &JobHandle<'_>) -> Value {
        runs.fetch_add(1, Ordering::SeqCst);
        handle.set_running();
        while !release.load(Ordering::Acquire) {
            if crate::csg::bsp::interrupt_message().is_some() {
                return cancelled("Render cancelled.");
            }
            thread::sleep(Duration::from_millis(5));
        }
        reply("done")
    }

    #[test]
    fn identical_calls_attach_to_one_job_and_a_retry_gets_the_cached_reply() {
        let registry = Arc::new(JobRegistry::new(Duration::from_secs(30), RESULT_TTL));
        let runs = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(AtomicBool::new(false));
        let callers: Vec<_> = (0..3)
            .map(|_| {
                let (registry, runs, release) = (Arc::clone(&registry), Arc::clone(&runs), Arc::clone(&release));
                thread::spawn(move || {
                    registry.run(spec(1, "d"), Caller::default(), |handle| blocking_work(&runs, &release, handle), |_| true, cancelled)
                })
            })
            .collect();
        eventually(|| registry.list("d").first().is_some_and(|job| job["waiters"] == json!(3)));
        assert_eq!(registry.list("d")[0]["state"], "running");
        release.store(true, Ordering::Release);
        let served: Vec<Served> = callers.into_iter().map(|caller| {
            let (value, served) = caller.join().unwrap();
            assert_eq!(value, reply("done"));
            served
        }).collect();
        assert_eq!(runs.load(Ordering::SeqCst), 1, "one job ran for three calls");
        assert_eq!(served.iter().filter(|served| **served == Served::Ran).count(), 1);
        let (value, served) = registry.run(spec(1, "d"), Caller::default(), |_| panic!("must not run"), |_| true, cancelled);
        assert_eq!((value, served), (reply("done"), Served::Cached));
        assert_eq!(registry.list("d")[0]["resultCached"], true);
        assert!(registry.list("other").is_empty(), "jobs are scoped by design");
    }

    #[test]
    fn a_job_outlives_its_callers_socket_for_the_grace_period_and_then_stops() {
        let registry = Arc::new(JobRegistry::new(Duration::from_millis(300), RESULT_TTL));
        let runs = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(AtomicBool::new(false));
        let gone = Arc::new(AtomicBool::new(false));
        let first = {
            let (registry, runs, release, gone) = (Arc::clone(&registry), Arc::clone(&runs), Arc::clone(&release), Arc::clone(&gone));
            thread::spawn(move || {
                // The caller's disconnect flag is also on the thread, as the
                // ClientWatch installs it; the job must not see it.
                let _client = crate::csg::bsp::cancellation_scope(Arc::clone(&gone));
                registry.run(spec(2, "d"), Caller { gone: Some(gone), rpc: None }, |handle| blocking_work(&runs, &release, handle), |_| true, cancelled)
            })
        };
        eventually(|| runs.load(Ordering::SeqCst) == 1);
        gone.store(true, Ordering::Release);
        thread::sleep(Duration::from_millis(150));
        assert_eq!(registry.live_count(), 1, "still running inside the grace period");
        // A retry attaches, and the job finishes for it.
        let retry = {
            let (registry, runs, release) = (Arc::clone(&registry), Arc::clone(&runs), Arc::clone(&release));
            thread::spawn(move || registry.run(spec(2, "d"), Caller::default(), |handle| blocking_work(&runs, &release, handle), |_| true, cancelled))
        };
        thread::sleep(Duration::from_millis(400));
        assert_eq!(registry.live_count(), 1, "an attached retry keeps it alive past the grace");
        release.store(true, Ordering::Release);
        assert_eq!(retry.join().unwrap(), (reply("done"), Served::Attached));
        first.join().unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        // Nobody comes back: cancelled after the grace, not cached.
        let gone = Arc::new(AtomicBool::new(true));
        let never = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let (value, _) = registry.run(spec(3, "d"), Caller { gone: Some(gone), rpc: None }, |handle| blocking_work(&runs, &never, handle), |_| true, cancelled);
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert!(value["content"][0]["text"].as_str().unwrap().contains("disconnected"), "{value}");
        assert_eq!(registry.list("d")[0]["state"], "cancelled");
        assert_eq!(registry.list("d")[0]["resultCached"], false);
        assert_eq!(registry.live_count(), 0);
    }

    #[test]
    fn cancel_by_id_needs_the_design_and_stops_the_job() {
        let registry = Arc::new(JobRegistry::new(Duration::from_secs(30), RESULT_TTL));
        let runs = Arc::new(AtomicUsize::new(0));
        let never = Arc::new(AtomicBool::new(false));
        let caller = {
            let (registry, runs, never) = (Arc::clone(&registry), Arc::clone(&runs), Arc::clone(&never));
            thread::spawn(move || registry.run(spec(4, "mine"), Caller::default(), |handle| blocking_work(&runs, &never, handle), |_| true, cancelled))
        };
        eventually(|| !registry.list("mine").is_empty());
        let id = registry.list("mine")[0]["id"].as_str().unwrap().to_string();
        assert!(registry.cancel("theirs", &id).is_err(), "another design cannot cancel it");
        assert_eq!(registry.cancel("mine", &id).unwrap()["id"], json!(id));
        let (value, _) = caller.join().unwrap();
        assert_eq!(value["isError"], true);
        assert!(value["content"][0]["text"].as_str().unwrap().contains("jobs tool"));
        assert!(registry.cancel("mine", &id).unwrap_err().contains("already finished"));
    }

    #[test]
    fn notifications_cancelled_withdraws_the_last_caller_and_cancels_at_once() {
        let registry = Arc::new(JobRegistry::new(Duration::from_secs(30), RESULT_TTL));
        let runs = Arc::new(AtomicUsize::new(0));
        let never = Arc::new(AtomicBool::new(false));
        let caller = {
            let (registry, runs, never) = (Arc::clone(&registry), Arc::clone(&runs), Arc::clone(&never));
            thread::spawn(move || {
                registry.run(spec(5, "d"), Caller { gone: None, rpc: Some("ip|7".into()) }, |handle| blocking_work(&runs, &never, handle), |_| true, cancelled)
            })
        };
        eventually(|| runs.load(Ordering::SeqCst) == 1);
        assert_eq!(registry.withdraw_rpc("ip|8"), 0);
        assert_eq!(registry.withdraw_rpc("ip|7"), 1);
        let (value, _) = caller.join().unwrap();
        assert!(value["content"][0]["text"].as_str().unwrap().contains("notifications/cancelled"), "{value}");
    }

    /// A withdrawal by an earlier caller must not make a later caller's
    /// disconnect cancel at once: what counts is how the *last* one left.
    #[test]
    fn an_earlier_withdrawal_does_not_skip_the_grace_for_a_later_disconnect() {
        let registry = Arc::new(JobRegistry::new(Duration::from_millis(400), RESULT_TTL));
        let runs = Arc::new(AtomicUsize::new(0));
        let never = Arc::new(AtomicBool::new(false));
        let leader = {
            let (registry, runs, never) = (Arc::clone(&registry), Arc::clone(&runs), Arc::clone(&never));
            thread::spawn(move || {
                registry.run(spec(10, "d"), Caller { gone: None, rpc: Some("s|1".into()) }, |handle| blocking_work(&runs, &never, handle), |_| true, cancelled)
            })
        };
        eventually(|| runs.load(Ordering::SeqCst) == 1);
        let gone = Arc::new(AtomicBool::new(false));
        let follower = {
            let (registry, gone) = (Arc::clone(&registry), Arc::clone(&gone));
            thread::spawn(move || registry.run(spec(10, "d"), Caller { gone: Some(gone), rpc: None }, |_| reply("never"), |_| true, cancelled))
        };
        eventually(|| registry.list("d").first().is_some_and(|job| job["waiters"] == json!(2)));
        assert_eq!(registry.withdraw_rpc("s|1"), 1);
        thread::sleep(Duration::from_millis(150));
        assert_eq!(registry.list("d")[0]["state"], "running", "the follower still waits");
        gone.store(true, Ordering::Release);
        let left = Instant::now();
        follower.join().unwrap();
        thread::sleep(Duration::from_millis(200));
        assert_eq!(registry.list("d")[0]["state"], "running", "a disconnect gets the grace");
        let (value, _) = leader.join().unwrap();
        assert!(left.elapsed() >= Duration::from_millis(400), "cancelled only after the grace");
        assert!(value["content"][0]["text"].as_str().unwrap().contains("disconnected"), "{value}");
    }

    /// The reverse order: everyone else disconnected, then the last caller
    /// withdraws explicitly, so the job stops at once.
    #[test]
    fn a_withdrawal_after_the_others_disconnected_cancels_at_once() {
        let registry = Arc::new(JobRegistry::new(Duration::from_secs(30), RESULT_TTL));
        let runs = Arc::new(AtomicUsize::new(0));
        let never = Arc::new(AtomicBool::new(false));
        let gone = Arc::new(AtomicBool::new(false));
        let leader = {
            let (registry, runs, never, gone) = (Arc::clone(&registry), Arc::clone(&runs), Arc::clone(&never), Arc::clone(&gone));
            thread::spawn(move || {
                registry.run(spec(11, "d"), Caller { gone: Some(gone), rpc: None }, |handle| blocking_work(&runs, &never, handle), |_| true, cancelled)
            })
        };
        eventually(|| runs.load(Ordering::SeqCst) == 1);
        let follower = {
            let registry = Arc::clone(&registry);
            thread::spawn(move || registry.run(spec(11, "d"), Caller { gone: None, rpc: Some("s|2".into()) }, |_| reply("never"), |_| true, cancelled))
        };
        eventually(|| registry.list("d").first().is_some_and(|job| job["waiters"] == json!(2)));
        gone.store(true, Ordering::Release);
        assert_eq!(registry.withdraw_rpc("s|2"), 1);
        let started = Instant::now();
        let (value, _) = leader.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(value["content"][0]["text"].as_str().unwrap().contains("notifications/cancelled"), "{value}");
        follower.join().unwrap();
    }

    #[test]
    fn a_follower_never_waits_past_its_limit_and_the_job_keeps_running() {
        let registry = Arc::new(JobRegistry::new(Duration::from_secs(30), RESULT_TTL).with_follow_limit(Duration::from_millis(200)));
        let runs = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(AtomicBool::new(false));
        let leader = {
            let (registry, runs, release) = (Arc::clone(&registry), Arc::clone(&runs), Arc::clone(&release));
            thread::spawn(move || registry.run(spec(12, "d"), Caller::default(), |handle| blocking_work(&runs, &release, handle), |_| true, cancelled))
        };
        eventually(|| runs.load(Ordering::SeqCst) == 1);
        let (value, served) = registry.run(spec(12, "d"), Caller::default(), |_| reply("never"), |_| true, cancelled);
        assert_eq!(served, Served::Attached);
        assert!(value["content"][0]["text"].as_str().unwrap().contains("Gave up"), "{value}");
        assert_eq!(registry.live_count(), 1, "the job is not cancelled by a follower giving up");
        release.store(true, Ordering::Release);
        assert_eq!(leader.join().unwrap().0, reply("done"));
    }

    #[test]
    fn a_cancelling_job_is_not_joined_and_targets_are_dropped_after_a_panic() {
        let registry = Arc::new(JobRegistry::new(Duration::from_secs(30), RESULT_TTL));
        let entered = Arc::new(AtomicBool::new(false));
        let stuck = {
            let (registry, entered) = (Arc::clone(&registry), Arc::clone(&entered));
            // Ignores its cancellation for a while, like a long kernel step.
            thread::spawn(move || {
                registry.run(spec(13, "d"), Caller { gone: None, rpc: Some("s|3".into()) }, |_| {
                    entered.store(true, Ordering::Release);
                    thread::sleep(Duration::from_millis(400));
                    panic!("boom")
                }, |_| true, cancelled)
            })
        };
        eventually(|| entered.load(Ordering::Acquire));
        let id = registry.list("d")[0]["id"].as_str().unwrap().to_string();
        registry.cancel("d", &id).unwrap();
        let (value, served) = registry.run(spec(13, "d"), Caller::default(), |_| reply("fresh"), |_| true, cancelled);
        assert_eq!((value, served), (reply("fresh"), Served::Ran), "a new call does not attach to a cancelling job");
        assert!(stuck.join().is_err());
        assert!(registry.inner.state.lock().unwrap().rpc.is_empty(), "the panicked call's target is gone");
    }

    #[test]
    fn uncacheable_replies_are_not_served_again() {
        let registry = JobRegistry::new(Duration::from_secs(30), RESULT_TTL);
        let (_, served) = registry.run(spec(6, "d"), Caller::default(), |_| reply("busy"), |_| false, cancelled);
        assert_eq!(served, Served::Ran);
        let (value, served) = registry.run(spec(6, "d"), Caller::default(), |_| reply("fresh"), |_| false, cancelled);
        assert_eq!((value, served), (reply("fresh"), Served::Ran));
    }

    #[test]
    fn the_cache_is_bounded_and_expires() {
        let registry = JobRegistry::new(Duration::from_secs(30), Duration::from_millis(50));
        for key in 0..(MAX_CACHED_RESULTS as u8 + 10) {
            registry.run(spec(key, "d"), Caller::default(), |_| reply("x"), |_| true, cancelled);
        }
        {
            let state = registry.inner.state.lock().unwrap();
            assert_eq!(state.cached_count(), MAX_CACHED_RESULTS);
        }
        thread::sleep(Duration::from_millis(60));
        let (_, served) = registry.run(spec(1, "d"), Caller::default(), |_| reply("x"), |_| true, cancelled);
        assert_eq!(served, Served::Ran, "an expired reply is recomputed");
    }

    #[test]
    fn a_panicking_job_still_releases_its_followers() {
        let registry = Arc::new(JobRegistry::new(Duration::from_secs(30), RESULT_TTL));
        let entered = Arc::new(AtomicBool::new(false));
        let leader = {
            let (registry, entered) = (Arc::clone(&registry), Arc::clone(&entered));
            thread::spawn(move || {
                registry.run(spec(9, "d"), Caller::default(), |_| {
                    entered.store(true, Ordering::Release);
                    thread::sleep(Duration::from_millis(200));
                    panic!("boom")
                }, |_| true, cancelled)
            })
        };
        eventually(|| entered.load(Ordering::Acquire));
        let (value, served) = registry.run(spec(9, "d"), Caller::default(), |_| reply("never"), |_| true, cancelled);
        assert!(leader.join().is_err());
        assert_eq!(served, Served::Attached);
        assert_eq!(value["isError"], true);
    }
}
