//! What AI agents do to a design over MCP, for the people watching it.
//!
//! Every MCP tool call that names a design becomes one entry in that design's
//! activity log: a running line while it works, then a one-line summary, the
//! source lines it read, the geometry it found (boxes, points, rays, section
//! loops, axes) as overlay items in design coordinates, the pictures it was
//! shown, coloured surfaces (scan features, deviation heatmaps) and the links it
//! produced. The browser reads the log through the
//! design's `/events` long poll (`activity=<version>`) and fetches pictures
//! and meshes from `/api/designs/<id>/activity/<seq>/<file>`.
//!
//! The log lives in memory only, bounded per design and in total: it is a
//! view of what happened recently in this process, not a record. Tool replies
//! are never changed by it.

use crate::vec3::cross;
use crate::assets::TriMesh;
use crate::raster::Rgba;
use crate::AppState;
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

/// Designs with a log at once; the least recently active one goes first.
const MAX_DESIGNS: usize = 256;
/// Entries kept per design.
const MAX_ENTRIES: usize = 60;
/// Entries a fresh browser (`activity=0`) is sent.
const INITIAL_ENTRIES: usize = 30;
/// Bytes kept per design, and in total: pictures, meshes and the detail
/// (overlays) alike.
const MAX_DESIGN_BYTES: usize = 96 << 20;
const MAX_TOTAL_BYTES: usize = 256 << 20;
/// A tool name and a summary as stored; an unknown tool's name is the caller's.
const MAX_TOOL_CHARS: usize = 64;
const MAX_SUMMARY_CHARS: usize = 300;
/// Pictures kept per entry, and the largest one.
const MAX_IMAGES: usize = 8;
const MAX_IMAGE_BYTES: usize = 4 << 20;
/// Coloured surfaces kept per entry, and the most triangles one may have.
const MAX_MESHES: usize = 6;
const MAX_MESH_TRIANGLES: usize = 500_000;
/// Overlay items per entry, and polyline points across them.
const MAX_ITEMS: usize = 400;
const MAX_POLYLINE_POINTS: usize = 8_000;
/// Intersection pairs passed on for the inspection panel.
const MAX_PAIRS: usize = 40;

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

struct Blob {
    name: String,
    content_type: &'static str,
    bytes: Arc<Vec<u8>>,
}

struct Entry {
    seq: u64,
    version: u64,
    tool: String,
    running: bool,
    error: Option<String>,
    started_ms: u64,
    started: Instant,
    elapsed_ms: Option<u64>,
    summary: String,
    detail: Map<String, Value>,
    /// `detail` serialized, measured when it is set.
    detail_bytes: usize,
    blobs: Vec<Blob>,
}

impl Entry {
    fn bytes(&self) -> usize {
        self.blobs.iter().map(|blob| blob.bytes.len()).sum::<usize>() + self.detail_bytes + self.tool.len() + self.summary.len()
    }

    fn measure(&mut self) {
        self.detail_bytes = serde_json::to_vec(&self.detail).map_or(0, |bytes| bytes.len());
    }

    /// Whether dropping its heavy parts would free anything.
    fn heavy(&self) -> bool {
        !self.blobs.is_empty() || HEAVY.iter().any(|key| self.detail.contains_key(*key))
    }

    fn public(&self, design: &str) -> Value {
        let mut value = Value::Object(self.detail.clone());
        value["seq"] = json!(self.seq);
        value["version"] = json!(self.version);
        value["tool"] = json!(self.tool);
        value["state"] = json!(if self.running {
            "running"
        } else if self.error.is_some() {
            "error"
        } else {
            "done"
        });
        value["startedAt"] = json!(self.started_ms);
        value["summary"] = json!(self.summary);
        if let Some(elapsed) = self.elapsed_ms {
            value["elapsedMs"] = json!(elapsed);
        }
        if let Some(error) = &self.error {
            value["error"] = json!(error);
        }
        // Files are named in the detail by their blob name; the browser gets URLs.
        let url = |name: &str| format!("/api/designs/{design}/activity/{}/{name}", self.seq);
        for key in ["images", "meshes"] {
            if let Some(items) = value.get_mut(key).and_then(Value::as_array_mut) {
                for item in items {
                    if let Some(name) = item.get("file").and_then(Value::as_str).map(str::to_string) {
                        item["url"] = json!(url(&name));
                    }
                }
            }
        }
        value
    }
}

#[derive(Default)]
struct Log {
    version: u64,
    entries: VecDeque<Entry>,
    touched: Option<Instant>,
}

impl Log {
    fn bytes(&self) -> usize {
        self.entries.iter().map(Entry::bytes).sum()
    }
}

#[derive(Default)]
struct Store {
    logs: HashMap<String, Log>,
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(Store::default()))
}

/// Versions and sequence numbers come from one process-wide counter, so they
/// only ever grow, also across a design's log being evicted and recreated.
fn next_number() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn lock() -> std::sync::MutexGuard<'static, Store> {
    store().lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Store {
    fn log_mut(&mut self, design: &str) -> &mut Log {
        if !self.logs.contains_key(design) && self.logs.len() >= MAX_DESIGNS {
            if let Some(oldest) = self
                .logs
                .iter()
                .min_by_key(|(_, log)| log.touched)
                .map(|(id, _)| id.clone())
            {
                self.logs.remove(&oldest);
            }
        }
        let log = self.logs.entry(design.to_string()).or_default();
        log.touched = Some(Instant::now());
        log
    }

    /// Drops the oldest entries' heavy parts until both byte budgets hold.
    fn trim(&mut self, design: &str) {
        if let Some(log) = self.logs.get_mut(design) {
            let mut bytes = log.bytes();
            for entry in log.entries.iter_mut() {
                if bytes <= MAX_DESIGN_BYTES {
                    break;
                }
                bytes -= entry.bytes();
                drop_heavy(entry);
                bytes += entry.bytes();
            }
        }
        let mut total: usize = self.logs.values().map(Log::bytes).sum();
        while total > MAX_TOTAL_BYTES {
            let oldest = self
                .logs
                .iter_mut()
                .flat_map(|(_, log)| log.entries.iter_mut())
                .filter(|entry| entry.heavy())
                .min_by_key(|entry| entry.seq);
            let Some(entry) = oldest else { break };
            total -= entry.bytes();
            drop_heavy(entry);
            total += entry.bytes();
        }
    }
}

/// Detail that can be large: everything but the summary and the read lines.
const HEAVY: [&str; 5] = ["images", "meshes", "overlay", "intersections", "links"];

/// Forgets an entry's files and large detail; its summary stays.
fn drop_heavy(entry: &mut Entry) {
    entry.blobs.clear();
    for key in HEAVY {
        entry.detail.remove(key);
    }
    entry.measure();
}

fn clip(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_string();
    }
    text.chars().take(chars.saturating_sub(1)).chain(['…']).collect()
}

/// The design's activity version: moves whenever an entry starts or changes.
pub(crate) fn version(design: &str) -> u64 {
    lock().logs.get(design).map_or(0, |log| log.version)
}

/// Entries changed after `since` (the last 30 when `since` is 0), oldest first.
pub(crate) fn since(design: &str, since: u64) -> Value {
    let store = lock();
    let Some(log) = store.logs.get(design) else {
        return json!({"version": 0, "entries": []});
    };
    let changed: Vec<&Entry> = log.entries.iter().filter(|entry| entry.version > since).collect();
    let skip = if since == 0 { changed.len().saturating_sub(INITIAL_ENTRIES) } else { 0 };
    let entries: Vec<Value> = changed.into_iter().skip(skip).map(|entry| entry.public(design)).collect();
    json!({"version": log.version, "entries": entries})
}

/// One stored picture or mesh of an entry.
pub(crate) fn file(design: &str, seq: u64, name: &str) -> Option<(&'static str, Arc<Vec<u8>>)> {
    let store = lock();
    let entry = store.logs.get(design)?.entries.iter().find(|entry| entry.seq == seq)?;
    let blob = entry.blobs.iter().find(|blob| blob.name == name)?;
    Some((blob.content_type, Arc::clone(&blob.bytes)))
}

/// Drops a deleted design's log.
pub(crate) fn forget(design: &str) {
    lock().logs.remove(design);
}

// ---------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------

/// An entry being recorded. Finishing it fills in the outcome; dropping it
/// unfinished (a panic, an early return) marks it stopped, so nothing stays
/// "running" forever.
pub(crate) struct Ticket {
    design: String,
    seq: u64,
    finished: bool,
}

impl Ticket {
    pub(crate) fn seq(&self) -> u64 {
        self.seq
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if !self.finished {
            update(&self.design, self.seq, |entry| {
                entry.running = false;
                entry.error = Some("The call stopped without a reply.".into());
                entry.summary = format!("{} stopped", tool_title(&entry.tool));
                entry.measure();
            });
        }
    }
}

fn update(design: &str, seq: u64, change: impl FnOnce(&mut Entry)) {
    let mut store = lock();
    let version = next_number();
    let Some(log) = store.logs.get_mut(design) else { return };
    let Some(entry) = log.entries.iter_mut().find(|entry| entry.seq == seq) else { return };
    change(entry);
    entry.version = version;
    log.version = version;
}

fn push(design: &str, mut entry: Entry) {
    let mut store = lock();
    let log = store.log_mut(design);
    entry.version = next_number();
    log.version = entry.version;
    log.entries.push_back(entry);
    while log.entries.len() > MAX_ENTRIES {
        log.entries.pop_front();
    }
}

/// Starts an entry for a call of `tool` on `design`. `None` when the design
/// does not exist (a mistyped id must not create a log).
pub(crate) fn begin(state: &AppState, design: &str, tool: &str, arguments: &Value) -> Option<Ticket> {
    if !matches!(state.workspaces.revision_of(design), Ok(Some(_))) {
        return None;
    }
    let seq = next_number();
    let mut detail = Map::new();
    // A read is shown as it starts: the lines are known from the arguments.
    if let Some(read) = requested_read(tool, arguments) {
        detail.insert("read".into(), read);
    }
    let mut entry = Entry {
        seq,
        version: 0,
        tool: clip(tool, MAX_TOOL_CHARS),
        running: true,
        error: None,
        started_ms: crate::unix_millis(),
        started: Instant::now(),
        elapsed_ms: None,
        summary: clip(&running_text(tool, arguments), MAX_SUMMARY_CHARS),
        detail,
        detail_bytes: 0,
        blobs: Vec::new(),
    };
    entry.measure();
    push(design, entry);
    Some(Ticket { design: design.to_string(), seq, finished: false })
}

/// Records a call's outcome: its summary and everything worth showing.
pub(crate) fn finish(mut ticket: Ticket, arguments: &Value, result: &Value, offered: Offered) {
    ticket.finished = true;
    let (design, seq) = (ticket.design.clone(), ticket.seq);
    let tool = {
        let store = lock();
        let Some(entry) = store.logs.get(&design).and_then(|log| log.entries.iter().find(|entry| entry.seq == seq)) else {
            return;
        };
        entry.tool.clone()
    };
    let outcome = describe_result(&tool, arguments, result, offered);
    update(&design, seq, |entry| {
        entry.running = false;
        entry.elapsed_ms = Some(entry.started.elapsed().as_millis() as u64);
        entry.summary = clip(&outcome.summary, MAX_SUMMARY_CHARS);
        entry.error = outcome.error;
        // The read shown while running stays unless the reply is more exact.
        for (key, value) in outcome.detail {
            entry.detail.insert(key, value);
        }
        entry.blobs = outcome.blobs;
        entry.measure();
    });
    lock().trim(&design);
}

/// Marks a started call as the person's own (the Compare parts panel), not an
/// agent's, with the revision it compares so the browser can tell it has moved.
pub(crate) fn mark_from_browser(ticket: &Ticket, revision: Option<u64>) {
    update(&ticket.design, ticket.seq, |entry| {
        entry.detail.insert("fromBrowser".into(), json!(true));
        if let Some(revision) = revision {
            entry.detail.insert("revision".into(), json!(revision));
        }
        entry.measure();
    });
}

/// One entry as the browser reads it, for a route that answers with its own call.
pub(crate) fn entry_public(design: &str, seq: u64) -> Option<Value> {
    let store = lock();
    store.logs.get(design)?.entries.iter().find(|entry| entry.seq == seq).map(|entry| entry.public(design))
}

/// Records a call that only names its design in the reply (`create_design`).
pub(crate) fn record_finished(state: &AppState, design: &str, tool: &str, arguments: &Value, result: &Value) {
    if let Some(ticket) = begin(state, design, tool, arguments) {
        finish(ticket, arguments, result, Offered::default());
    }
}

// ---------------------------------------------------------------------------
// Offers: what tools hand the browser beside their reply
// ---------------------------------------------------------------------------

/// A coloured surface a tool offers for the browser: a binary STL whose
/// triangles are grouped by colour, with `[first, count, slot]` runs over a
/// palette (the same form the render route uses for painted objects).
pub(crate) struct Surface {
    label: String,
    stl: Vec<u8>,
    palette: Vec<[f64; 4]>,
    runs: Vec<[u32; 3]>,
    legend: Value,
}

/// Everything tools offered during one call: coloured surfaces, pictures the
/// agent did not ask for (cheap ones, drawn for the console), and overlay
/// items in design coordinates.
#[derive(Default)]
pub(crate) struct Offered {
    surfaces: Vec<Surface>,
    images: Vec<(String, Vec<u8>)>,
    items: Vec<Value>,
}

thread_local! {
    static SINK: RefCell<Option<Offered>> = const { RefCell::new(None) };
}

/// Runs `work` collecting what tools offer on this thread.
pub(crate) fn capture<T>(work: impl FnOnce() -> T) -> (T, Offered) {
    let outer = SINK.with(|sink| sink.borrow_mut().replace(Offered::default()));
    /// Restores the outer sink also when `work` panics.
    struct Restore(Option<Option<Offered>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(outer) = self.0.take() {
                SINK.with(|sink| *sink.borrow_mut() = outer);
            }
        }
    }
    let mut restore = Restore(Some(outer));
    let value = work();
    let offered = SINK.with(|sink| sink.borrow_mut().take()).unwrap_or_default();
    if let Some(outer) = restore.0.take() {
        SINK.with(|sink| *sink.borrow_mut() = outer);
    }
    (value, offered)
}

/// Whether anything offered now would be kept: tools skip work they would
/// only do for the browser when nobody collects it.
pub(crate) fn collecting() -> bool {
    SINK.with(|sink| sink.borrow().is_some())
}

fn offer(add: impl FnOnce(&mut Offered)) {
    SINK.with(|sink| {
        if let Some(offered) = sink.borrow_mut().as_mut() {
            add(offered);
        }
    });
}

/// Offers a picture for the console that the agent was not sent.
pub(crate) fn offer_image(caption: &str, png: Vec<u8>) {
    if png.len() <= MAX_IMAGE_BYTES {
        offer(|offered| {
            if offered.images.len() < MAX_IMAGES {
                offered.images.push((clip(caption, MAX_SUMMARY_CHARS), png));
            }
        });
    }
}

/// Offers overlay items (the `Overlay` forms: box, segment, point, polyline,
/// plane) in design coordinates.
pub(crate) fn offer_items(items: Vec<Value>) {
    offer(|offered| {
        let room = MAX_ITEMS.saturating_sub(offered.items.len());
        offered.items.extend(items.into_iter().take(room));
    });
}

/// Offers `mesh` (design coordinates) painted per triangle. `legend` is shown
/// beside it: `{title, range, dead, signed, keys: [{color, text}]}`.
pub(crate) fn offer_painted(label: &str, mesh: &TriMesh, colors: &[Rgba], legend: Value) {
    let room = SINK.with(|sink| sink.borrow().as_ref().is_some_and(|offered| offered.surfaces.len() < MAX_MESHES));
    if !room || mesh.triangles.is_empty() || mesh.triangles.len() > MAX_MESH_TRIANGLES || colors.len() != mesh.triangles.len() {
        return;
    }
    let mut slots: HashMap<Rgba, u32> = HashMap::new();
    let mut palette: Vec<[f64; 4]> = Vec::new();
    // 32 levels per channel keeps a smooth ramp to a few dozen draw calls.
    let mut slot_of = |color: Rgba| {
        let q = [color[0] & 0xf8, color[1] & 0xf8, color[2] & 0xf8, color[3]];
        *slots.entry(q).or_insert_with(|| {
            palette.push([q[0] as f64 / 255.0, q[1] as f64 / 255.0, q[2] as f64 / 255.0, q[3] as f64 / 255.0]);
            (palette.len() - 1) as u32
        })
    };
    let slot: Vec<u32> = colors.iter().map(|&color| slot_of(color)).collect();
    let mut order: Vec<usize> = (0..mesh.triangles.len()).collect();
    order.sort_by_key(|&t| slot[t]);
    let mut runs: Vec<[u32; 3]> = Vec::new();
    for (index, &t) in order.iter().enumerate() {
        match runs.last_mut() {
            Some(run) if run[2] == slot[t] => run[1] += 1,
            _ => runs.push([index as u32, 1, slot[t]]),
        }
    }
    let mut stl = vec![0u8; 80];
    stl.extend_from_slice(&(order.len() as u32).to_le_bytes());
    for &t in &order {
        let [a, b, c] = mesh.triangles[t].map(|i| mesh.points[i as usize]);
        let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let n = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
        let length = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-30);
        for value in n.iter().map(|x| x / length).chain(a).chain(b).chain(c) {
            stl.extend_from_slice(&(value as f32).to_le_bytes());
        }
        stl.extend_from_slice(&[0, 0]);
    }
    offer(|offered| {
        if offered.surfaces.len() < MAX_MESHES {
            offered.surfaces.push(Surface { label: label.to_string(), stl, palette, runs, legend });
        }
    });
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// `GET /api/designs/<id>/activity[?since=v]` and
/// `GET /api/designs/<id>/activity/<seq>/<file>`.
pub(crate) fn handle_route(stream: &mut TcpStream, request: &crate::Request, id: &str, rest: &str) -> io::Result<()> {
    if request.method != "GET" {
        return crate::respond_error(stream, 405, "Activity is read-only.");
    }
    let rest = rest.trim_matches('/');
    if rest.is_empty() {
        let parameters = crate::query_parameters(&request.query);
        let since_version = crate::query_u64(&parameters, "since").unwrap_or(0);
        let mut value = since(id, since_version);
        value["ok"] = json!(true);
        return crate::respond_value(stream, 200, &value);
    }
    let Some((seq, name)) = rest.split_once('/') else {
        return crate::respond_error(stream, 404, "Activity file not found.");
    };
    let Ok(seq) = seq.parse::<u64>() else {
        return crate::respond_error(stream, 404, "Activity file not found.");
    };
    match file(id, seq, name) {
        Some((content_type, bytes)) => crate::respond_text(
            stream,
            200,
            content_type,
            &bytes,
            &[("Cache-Control", "private, max-age=600, immutable"), ("X-Content-Type-Options", "nosniff")],
        ),
        None => crate::respond_error(stream, 404, "Activity file not found (it may have been dropped to save memory)."),
    }
}

// ---------------------------------------------------------------------------
// Describing calls
// ---------------------------------------------------------------------------

/// A tool's name as a heading ("check_intersections" -> "Check intersections").
fn tool_title(tool: &str) -> String {
    let words = tool.replace('_', " ");
    let mut chars = words.chars();
    chars.next().map(|first| first.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
}

fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| !text.is_empty())
}

fn count(value: &Value) -> usize {
    value.as_array().map_or(0, Vec::len)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn mm(value: f64) -> String {
    let rounded = (value * 100.0).round() / 100.0;
    format!("{} mm", rounded + 0.0)
}

fn quoted(text: &str) -> String {
    let short: String = text.chars().take(40).collect();
    format!("“{short}{}”", if text.chars().count() > 40 { "…" } else { "" })
}

/// What a call is about to do, shown while it runs.
fn running_text(tool: &str, arguments: &Value) -> String {
    match tool {
        "get_design_code" => match (text(&arguments["grep"]), arguments["startLine"].as_u64()) {
            (Some(pattern), _) => format!("Searching the source for {}", quoted(pattern)),
            (None, Some(start)) => match arguments["endLine"].as_u64() {
                Some(end) => format!("Reading lines {start}–{end}"),
                None => format!("Reading from line {start}"),
            },
            _ => "Reading the source".into(),
        },
        "list_modules" => "Reading the outline of the source".into(),
        "get_parameters" => "Reading the parameters".into(),
        "list_design_objects" => "Listing the objects".into(),
        "evaluate" => match text(&arguments["revisionName"]) {
            Some(name) => format!("Saving {} and checking it", quoted(name)),
            None => "Checking the model".into(),
        },
        "measure" => "Measuring".into(),
        "check_intersections" => "Checking intersections".into(),
        "check_insertion" => "Checking whether a part can be inserted".into(),
        "describe_geometry" => "Describing the geometry".into(),
        "probe" => "Probing points and rays".into(),
        "section_profile" => "Cutting a section".into(),
        "fit_axis" => "Fitting an axis".into(),
        "place" => "Working out a placement".into(),
        "diff" => "Comparing revisions".into(),
        "render_image" => "Looking at the model".into(),
        "render_design" => "Exporting".into(),
        "export_print_parts" => "Exporting print parts".into(),
        "get_print_layout" => "Reading the print layout".into(),
        "split_object" => "Planning a split".into(),
        "create_share_link" => "Creating a view-only link".into(),
        "get_design_link" => "Getting the design link".into(),
        "update_design_code" => "Editing the source".into(),
        "restore_revision" => "Restoring a revision".into(),
        "list_revisions" => "Listing the revisions".into(),
        "set_parameters" => "Setting parameters".into(),
        "set_text" => "Placing text".into(),
        "set_print_orientation" => "Setting a print orientation".into(),
        "upload_asset" | "create_asset_upload" => "Uploading a mesh".into(),
        "list_assets" => "Listing the meshes".into(),
        "delete_asset" => "Deleting a mesh".into(),
        "compare_surfaces" => "Comparing surfaces".into(),
        "compare_sections" => "Comparing sections with the scan".into(),
        "measure_between" => "Measuring between surfaces".into(),
        "fit_curve" => "Fitting a curve".into(),
        "scan_report" => "Checking the model against the scan".into(),
        "list_feedback" if arguments["waitSeconds"].as_f64().is_some_and(|wait| wait > 0.0) => {
            "Waiting for your comments".into()
        }
        "list_feedback" => "Reading your comments".into(),
        "request_pointing" => "Asking you to point at the model".into(),
        "resolve_feedback" => "Answering your comments".into(),
        "jobs" => "Checking its running jobs".into(),
        "get_supported_scad" => "Reading the SCAD dictionary".into(),
        "get_reverse_engineering_guide" => "Reading the reverse-engineering guide".into(),
        other => tool_title(other),
    }
}

/// The source lines a read asks for, known before it runs.
fn requested_read(tool: &str, arguments: &Value) -> Option<Value> {
    if tool != "get_design_code" || arguments.get("grep").is_some() {
        return None;
    }
    let start = arguments["startLine"].as_u64()?;
    let end = arguments["endLine"].as_u64().unwrap_or(start);
    Some(json!({"ranges": [[start, end.max(start)]], "kind": "read"}))
}

struct Outcome {
    summary: String,
    error: Option<String>,
    detail: Map<String, Value>,
    blobs: Vec<Blob>,
}

fn describe_result(tool: &str, arguments: &Value, result: &Value, offered: Offered) -> Outcome {
    let structured = &result["structuredContent"];
    let mut detail = Map::new();
    let mut blobs = Vec::new();
    // Source that does not compile is a normal reply to the agent, but nothing
    // was saved: to the person watching it is a failure.
    if result["isError"] == json!(true) || structured["ok"] == json!(false) {
        let error = text(&structured["error"])
            .or_else(|| text(&result["content"][0]["text"]))
            .unwrap_or("The call failed.")
            .chars()
            .take(300)
            .collect::<String>();
        return Outcome {
            summary: format!("{} failed", running_text(tool, arguments)),
            error: Some(error),
            detail,
            blobs,
        };
    }
    let mut overlay = Overlay::default();
    let summary = describe(tool, arguments, structured, &mut detail, &mut overlay);
    for item in offered.items {
        let points = item["points"].as_array().map_or(0, Vec::len);
        if overlay.polyline_points + points <= MAX_POLYLINE_POINTS {
            overlay.polyline_points += points;
            overlay.push(item);
        }
    }
    if !overlay.items.is_empty() {
        detail.insert("overlay".into(), Value::Array(overlay.items));
    }
    // Which revision the findings describe, so the browser draws them only on
    // that model.
    if let Some(revision) = structured["revision"].as_u64() {
        detail.insert("revision".into(), json!(revision));
    }
    // Pictures the agent was shown, with the caption that came with each.
    let content = result["content"].as_array().cloned().unwrap_or_default();
    let mut images = Vec::new();
    for (index, item) in content.iter().enumerate() {
        if images.len() >= MAX_IMAGES {
            break;
        }
        if item["type"] != "image" || item["mimeType"] != "image/png" {
            continue;
        }
        let Some(bytes) = item["data"].as_str().and_then(crate::base64_decode) else { continue };
        if bytes.len() > MAX_IMAGE_BYTES {
            continue;
        }
        let caption = index
            .checked_sub(1)
            .and_then(|before| content.get(before))
            .filter(|before| before["type"] == "text")
            .and_then(|before| before["text"].as_str())
            .filter(|caption| caption.len() < 400 && !caption.trim_start().starts_with('{'))
            .map(|caption| clip(caption, MAX_SUMMARY_CHARS))
            .unwrap_or_else(|| tool_title(tool));
        let name = format!("{}.png", images.len());
        images.push(json!({"file": name, "title": caption}));
        blobs.push(Blob { name, content_type: "image/png", bytes: Arc::new(bytes) });
    }
    // Then the ones drawn only for the console (the agent asked for none).
    for (caption, png) in offered.images {
        if images.len() >= MAX_IMAGES {
            break;
        }
        let name = format!("{}.png", images.len());
        images.push(json!({"file": name, "title": caption, "forConsole": true}));
        blobs.push(Blob { name, content_type: "image/png", bytes: Arc::new(png) });
    }
    if !images.is_empty() {
        detail.insert("images".into(), Value::Array(images));
    }
    let mut meshes = Vec::new();
    for (index, surface) in offered.surfaces.into_iter().enumerate() {
        let name = format!("{index}.stl");
        meshes.push(json!({
            "file": name,
            "label": surface.label,
            "palette": surface.palette,
            "runs": surface.runs,
            "legend": surface.legend,
        }));
        blobs.push(Blob { name, content_type: "model/stl", bytes: Arc::new(surface.stl) });
    }
    if !meshes.is_empty() {
        detail.insert("meshes".into(), Value::Array(meshes));
    }
    Outcome { summary, error: None, detail, blobs }
}

// ---------------------------------------------------------------------------
// Overlay items
// ---------------------------------------------------------------------------

/// Overlay items in design coordinates. `tone` names a colour role
/// (`bad`, `warn`, `good`, `info`, `agent`), never a literal colour.
#[derive(Default)]
struct Overlay {
    items: Vec<Value>,
    polyline_points: usize,
}

fn point(value: &Value) -> Option<[f64; 3]> {
    let items = value.as_array()?;
    if items.len() != 3 {
        return None;
    }
    let mut out = [0.0; 3];
    for (slot, item) in out.iter_mut().zip(items) {
        *slot = item.as_f64().filter(|x| x.is_finite())?;
    }
    Some(out)
}

fn round(p: [f64; 3]) -> Value {
    json!(p.map(|x| (x * 1000.0).round() / 1000.0 + 0.0))
}

fn along(origin: [f64; 3], direction: [f64; 3], t: f64) -> [f64; 3] {
    [origin[0] + direction[0] * t, origin[1] + direction[1] * t, origin[2] + direction[2] * t]
}

impl Overlay {
    fn push(&mut self, item: Value) {
        if self.items.len() < MAX_ITEMS {
            self.items.push(item);
        }
    }

    fn boxed(&mut self, bounds: &Value, tone: &str, label: Option<String>, dashed: bool) {
        if let (Some(min), Some(max)) = (point(&bounds["min"]), point(&bounds["max"])) {
            self.push(json!({"kind": "box", "min": round(min), "max": round(max), "tone": tone, "label": label, "dashed": dashed}));
        }
    }

    fn segment(&mut self, a: [f64; 3], b: [f64; 3], tone: &str, label: Option<String>) {
        self.push(json!({"kind": "segment", "a": round(a), "b": round(b), "tone": tone, "label": label}));
    }

    fn dot(&mut self, at: [f64; 3], tone: &str, label: Option<String>) {
        self.push(json!({"kind": "point", "at": round(at), "tone": tone, "label": label}));
    }

    fn polyline(&mut self, points: Vec<[f64; 3]>, closed: bool, tone: &str, label: Option<String>) {
        if points.len() < 2 || self.polyline_points + points.len() > MAX_POLYLINE_POINTS {
            return;
        }
        self.polyline_points += points.len();
        let points: Vec<Value> = points.into_iter().map(round).collect();
        self.push(json!({"kind": "polyline", "points": points, "closed": closed, "tone": tone, "label": label}));
    }

    /// A square patch of a plane, `half` mm each way from `origin`.
    fn plane(&mut self, origin: [f64; 3], u: [f64; 3], v: [f64; 3], half_u: f64, half_v: f64, tone: &str, label: Option<String>) {
        let corner = |su: f64, sv: f64| {
            [0, 1, 2].map(|axis| origin[axis] + u[axis] * su * half_u + v[axis] * sv * half_v)
        };
        let corners = [corner(-1.0, -1.0), corner(1.0, -1.0), corner(1.0, 1.0), corner(-1.0, 1.0)];
        self.push(json!({"kind": "plane", "corners": corners.map(round), "tone": tone, "label": label}));
    }

    /// Plane-local 2D points (`[u, v]`) on `plane` (`{origin, u, v}`) as 3D.
    fn loop2d(&mut self, plane: &Value, points: &Value, closed: bool, tone: &str, label: Option<String>) {
        let (Some(origin), Some(u), Some(v)) = (point(&plane["origin"]), point(&plane["u"]), point(&plane["v"])) else { return };
        let points: Vec<[f64; 3]> = points
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| Some((item[0].as_f64()?, item[1].as_f64()?)))
                    .map(|(x, y)| [0, 1, 2].map(|axis| origin[axis] + u[axis] * x + v[axis] * y))
                    .collect()
            })
            .unwrap_or_default();
        self.polyline(points, closed, tone, label);
    }

    /// The patch of a plane covering 2D bounds (`{min: [u, v], max: [u, v]}`), padded.
    fn plane_over(&mut self, plane: &Value, bounds: Option<([f64; 2], [f64; 2])>, tone: &str, label: Option<String>) {
        let (Some(origin), Some(u), Some(v)) = (point(&plane["origin"]), point(&plane["u"]), point(&plane["v"])) else { return };
        let ((lo, hi), pad) = match bounds {
            Some(bounds) => (bounds, 0.12),
            None => (([-20.0, -20.0], [20.0, 20.0]), 0.0),
        };
        let center = [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0];
        let half_u = ((hi[0] - lo[0]) / 2.0) * (1.0 + pad) + 1.0;
        let half_v = ((hi[1] - lo[1]) / 2.0) * (1.0 + pad) + 1.0;
        let at = [0, 1, 2].map(|axis| origin[axis] + u[axis] * center[0] + v[axis] * center[1]);
        self.plane(at, u, v, half_u, half_v, tone, label);
    }

    /// Intersection or clearance findings of a check_intersections-style reply.
    fn pairs(&mut self, pairs: &Value) {
        for pair in pairs.as_array().into_iter().flatten() {
            let relation = pair["relation"].as_str().unwrap_or("");
            let tone = match relation {
                "overlap" => "bad",
                "contact" | "clearanceViolation" => "warn",
                _ => continue,
            };
            if relation == "contact" && pair["toleranceViolated"] == json!(false) {
                continue;
            }
            let names = format!(
                "{} × {}",
                text(&pair["aName"]).or(text(&pair["a"])).unwrap_or("?"),
                text(&pair["bName"]).or(text(&pair["b"])).unwrap_or("?")
            );
            if relation == "overlap" {
                self.boxed(&pair["overlapBounds"], tone, None, true);
            }
            if let (Some(a), Some(b)) = (point(&pair["closestPoints"]["a"]), point(&pair["closestPoints"]["b"])) {
                self.segment(a, b, tone, None);
            }
            if let Some(at) = point(&pair["location"]) {
                self.dot(at, tone, Some(names));
            }
        }
    }
}

/// 2D bounds of a section plane's loops.
fn loops_bounds(objects: &Value) -> Option<([f64; 2], [f64; 2])> {
    let mut lo = [f64::INFINITY; 2];
    let mut hi = [f64::NEG_INFINITY; 2];
    for object in objects.as_array().into_iter().flatten() {
        for found in object["loops"].as_array().into_iter().flatten() {
            for p in found["points"].as_array().into_iter().flatten() {
                if let (Some(x), Some(y)) = (p[0].as_f64(), p[1].as_f64()) {
                    lo = [lo[0].min(x), lo[1].min(y)];
                    hi = [hi[0].max(x), hi[1].max(y)];
                }
            }
        }
    }
    lo[0].is_finite().then_some((lo, hi))
}

fn bounds2d(value: &Value) -> Option<([f64; 2], [f64; 2])> {
    let get = |key: &str| Some([value[key][0].as_f64()?, value[key][1].as_f64()?]);
    Some((get("min")?, get("max")?))
}

fn size_label(bounds: &Value) -> Option<String> {
    let size = point(&bounds["size"]).or_else(|| {
        let (min, max) = (point(&bounds["min"])?, point(&bounds["max"])?);
        Some([max[0] - min[0], max[1] - min[1], max[2] - min[2]])
    })?;
    let show = |x: f64| format!("{}", (x * 100.0).round() / 100.0 + 0.0);
    Some(format!("{} × {} × {} mm", show(size[0]), show(size[1]), show(size[2])))
}

/// Read ranges as `[[start, end], ...]` from items with `lines: [start, end]`
/// (or `source.lines`), or a single `line`.
fn line_ranges<'a>(items: impl Iterator<Item = &'a Value>) -> Vec<Value> {
    items
        .filter_map(|item| {
            let lines = item.get("lines").or_else(|| item.get("source").and_then(|source| source.get("lines")));
            match lines.map(|lines| (lines[0].as_u64(), lines[1].as_u64())) {
                Some((Some(start), Some(end))) => Some(json!([start, end.max(start)])),
                Some((Some(start), None)) => Some(json!([start, start])),
                _ => item.get("line").and_then(Value::as_u64).map(|line| json!([line, line])),
            }
        })
        .take(200)
        .collect()
}

/// One call's summary line, filling `detail` and `overlay` on the way.
fn describe(tool: &str, arguments: &Value, s: &Value, detail: &mut Map<String, Value>, overlay: &mut Overlay) -> String {
    let objects = &s["objects"];
    match tool {
        "get_design_code" => {
            if let Some(pattern) = text(&arguments["grep"]) {
                let hits: Vec<Value> = s["matchLines"].as_array().cloned().unwrap_or_default().into_iter().take(200).collect();
                let blocks: Vec<Value> = s["matches"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|block| Some(json!([block["startLine"].as_u64()?, block["endLine"].as_u64()?])))
                    .take(100)
                    .collect();
                let found = hits.len();
                detail.insert("read".into(), json!({"ranges": blocks, "hits": hits, "kind": "grep"}));
                return format!("Searched the source for {} · {}", quoted(pattern), plural(found, "match", "matches"));
            }
            match (s["startLine"].as_u64(), s["endLine"].as_u64()) {
                (Some(start), Some(end)) => {
                    detail.insert("read".into(), json!({"ranges": [[start, end]], "kind": "read"}));
                    let total = s["totalLines"].as_u64().unwrap_or(end);
                    if start <= 1 && end >= total {
                        "Read the whole source".into()
                    } else {
                        format!("Read lines {start}–{end} of {total}")
                    }
                }
                _ => "Read the source".into(),
            }
        }
        "list_modules" => {
            let ranges = line_ranges(s["definitions"].as_array().into_iter().flatten());
            let n = count(&s["definitions"]);
            detail.insert("read".into(), json!({"ranges": ranges, "kind": "outline"}));
            format!("Read the outline · {}", plural(n, "module or function", "modules and functions"))
        }
        "get_parameters" => {
            let ranges = line_ranges(s["parameters"].as_array().into_iter().flatten());
            detail.insert("read".into(), json!({"ranges": ranges, "kind": "parameters"}));
            format!("Read {}", plural(count(&s["parameters"]), "parameter", "parameters"))
        }
        "list_design_objects" => format!("Listed {}", plural(count(objects), "object", "objects")),
        "evaluate" => {
            let pairs = &s["intersections"];
            overlay.pairs(&pairs["intersections"]);
            if pairs.is_object() {
                detail.insert("intersections".into(), slim_pairs(pairs, s["revision"].as_u64()));
            }
            let mut parts = vec![plural(count(objects), "object", "objects")];
            let overlaps = pairs["summary"]["overlaps"].as_u64().unwrap_or(0) as usize;
            let violations = pairs["summary"]["toleranceViolations"].as_u64().unwrap_or(0) as usize;
            if overlaps > 0 {
                parts.push(plural(overlaps, "overlap", "overlaps"));
            } else if violations > 0 {
                parts.push(plural(violations, "clearance problem", "clearance problems"));
            } else if pairs.is_object() {
                parts.push("no intersections".into());
            }
            if s["meshes"]["valid"] == json!(false) {
                parts.push("invalid mesh".into());
            }
            let errors = count(&s["diagnostics"]);
            if errors > 0 {
                parts.push(plural(errors, "diagnostic", "diagnostics"));
            }
            let head = match text(&s["saved"]["name"]) {
                Some(name) => format!("Saved {} and checked it", quoted(name)),
                None => "Checked the model".into(),
            };
            format!("{head} · {}", parts.join(" · "))
        }
        "measure" => {
            for object in objects.as_array().into_iter().flatten() {
                overlay.boxed(&object["bounds"], "info", size_label(&object["bounds"]), false);
            }
            for pair in s["distances"].as_array().into_iter().flatten() {
                if let (Some(a), Some(b)) = (point(&pair["closestPoints"]["a"]), point(&pair["closestPoints"]["b"])) {
                    let label = pair["clearance"].as_f64().map(mm);
                    overlay.segment(a, b, "agent", label);
                }
            }
            let first = objects.as_array().and_then(|items| items.first());
            match (count(objects), first) {
                (1, Some(object)) => format!(
                    "Measured {}: {}",
                    text(&object["name"]).unwrap_or("the object"),
                    size_label(&object["bounds"]).unwrap_or_default()
                ),
                (n, _) => format!("Measured {}", plural(n, "object", "objects")),
            }
        }
        "check_intersections" => {
            overlay.pairs(&s["intersections"]);
            detail.insert("intersections".into(), slim_pairs(s, s["revision"].as_u64()));
            let overlaps = s["summary"]["overlaps"].as_u64().unwrap_or(0) as usize;
            let violations = s["summary"]["toleranceViolations"].as_u64().unwrap_or(0) as usize;
            let tolerance = s["tolerance"].as_f64().map(mm).unwrap_or_default();
            if overlaps + violations == 0 {
                format!("No intersections at {tolerance} tolerance")
            } else {
                let mut parts = Vec::new();
                if overlaps > 0 {
                    parts.push(plural(overlaps, "overlap", "overlaps"));
                }
                if violations > overlaps {
                    parts.push(plural(violations - overlaps, "clearance problem", "clearance problems"));
                }
                format!("Found {} at {tolerance} tolerance", parts.join(" and "))
            }
        }
        "check_insertion" => {
            let path: Vec<[f64; 3]> = arguments["path"].as_array().into_iter().flatten().filter_map(point).collect();
            let collision = &s["collision"];
            if let (Some(a), Some(b)) = (point(&collision["pointA"]), point(&collision["pointB"])) {
                overlay.dot(a, "bad", Some("first contact".into()));
                if a != b {
                    overlay.segment(a, b, "bad", None);
                }
            }
            if path.len() >= 2 {
                overlay.polyline(path, false, "agent", Some("insertion path (offsets)".into()));
            }
            let moving = text(&s["moving"]).unwrap_or("the part");
            if s["passes"] == json!(true) {
                format!("{moving} can be inserted")
            } else {
                let travel = collision["travel"].as_f64().map(|t| format!(" after {}", mm(t))).unwrap_or_default();
                format!("{moving} collides{travel}")
            }
        }
        "describe_geometry" => {
            for object in objects.as_array().into_iter().flatten() {
                let bounds = json!({"min": object["boundsMin"], "max": object["boundsMax"]});
                overlay.boxed(&bounds, "info", text(&object["name"]).map(str::to_string), false);
                let axes = &object["principalAxes"];
                if let (Some(center), Some(min), Some(max)) = (point(&axes["center"]), point(&object["boundsMin"]), point(&object["boundsMax"])) {
                    let reach = ((max[0] - min[0]).powi(2) + (max[1] - min[1]).powi(2) + (max[2] - min[2]).powi(2)).sqrt() * 0.35;
                    for (index, axis) in axes["axes"].as_array().into_iter().flatten().take(3).enumerate() {
                        if let Some(direction) = point(axis) {
                            let tone = ["bad", "good", "info"][index];
                            overlay.segment(along(center, direction, -reach), along(center, direction, reach), tone, None);
                        }
                    }
                }
            }
            format!("Described {}", plural(count(objects), "object", "objects"))
        }
        "probe" => {
            let rays = arguments["rays"].as_array().cloned().unwrap_or_default();
            for (ray, answer) in rays.iter().zip(s["rays"].as_array().into_iter().flatten()) {
                let (Some(origin), Some(direction)) = (point(&ray["origin"]), point(&ray["direction"])) else { continue };
                let length = (direction[0].powi(2) + direction[1].powi(2) + direction[2].powi(2)).sqrt().max(1e-12);
                let direction = direction.map(|x| x / length);
                match point(&answer["point"]) {
                    Some(hit) => {
                        overlay.segment(origin, hit, "agent", None);
                        overlay.dot(hit, "agent", answer["distance"].as_f64().map(mm));
                    }
                    None => {
                        let reach = ray["maxDistance"].as_f64().or(arguments["maxDistance"].as_f64()).unwrap_or(50.0).min(500.0);
                        overlay.segment(origin, along(origin, direction, reach), "warn", Some("miss".into()));
                    }
                }
                for hit in answer["hits"].as_array().into_iter().flatten().take(16) {
                    if let Some(at) = point(&hit["point"]) {
                        overlay.dot(at, "info", None);
                    }
                }
            }
            for answer in s["points"].as_array().into_iter().flatten() {
                if let (Some(from), Some(closest)) = (point(&answer["point"]), point(&answer["closest"])) {
                    overlay.segment(from, closest, "agent", answer["distance"].as_f64().map(mm));
                    overlay.dot(closest, "agent", None);
                }
            }
            let parts: Vec<String> = [
                (count(&s["rays"]), "ray", "rays"),
                (count(&s["points"]), "point", "points"),
            ]
            .into_iter()
            .filter(|(n, _, _)| *n > 0)
            .map(|(n, one, many)| plural(n, one, many))
            .collect();
            format!("Probed {}", if parts.is_empty() { "the model".into() } else { parts.join(" and ") })
        }
        "section_profile" => {
            let plane = &s["plane"];
            overlay.plane_over(plane, loops_bounds(objects), "agent", None);
            let mut loops = 0;
            for object in objects.as_array().into_iter().flatten() {
                for found in object["loops"].as_array().into_iter().flatten() {
                    loops += 1;
                    let closed = found["closed"] != json!(false);
                    overlay.loop2d(plane, &found["points"], closed, if found["hole"] == json!(true) { "warn" } else { "agent" }, None);
                }
            }
            format!("Cut a section · {}", plural(loops, "loop", "loops"))
        }
        "fit_axis" => {
            for object in objects.as_array().into_iter().flatten() {
                if let (Some(at), Some(direction)) = (point(&object["point"]), point(&object["direction"])) {
                    let start = object["startT"].as_f64().unwrap_or(-10.0);
                    let end = object["endT"].as_f64().unwrap_or(10.0);
                    overlay.segment(along(at, direction, start), along(at, direction, end), "agent", text(&object["name"]).map(|n| format!("axis of {n}")));
                    overlay.dot(at, "agent", None);
                }
            }
            let first = objects.as_array().and_then(|items| items.first());
            match first.and_then(|object| object["taperHalfAngleDeg"].as_f64()) {
                Some(taper) if count(objects) == 1 => format!("Fitted an axis · taper {:.1}°", taper),
                _ => format!("Fitted {}", plural(count(objects), "axis", "axes")),
            }
        }
        "place" => {
            overlay.boxed(&s["boundsBefore"], "info", Some("before".into()), true);
            overlay.boxed(&s["boundsAfter"], "agent", Some("after".into()), false);
            match point(&s["translate"]) {
                Some(t) => format!("Worked out a placement · move [{}, {}, {}]", t[0] + 0.0, t[1] + 0.0, t[2] + 0.0),
                None => "Worked out a placement".into(),
            }
        }
        "diff" => {
            let mut changed = 0;
            for object in objects.as_array().into_iter().flatten() {
                if object["status"] != json!("unchanged") {
                    changed += 1;
                    overlay.boxed(&object["changedBounds"], "warn", text(&object["name"]).map(str::to_string), true);
                }
            }
            if s["identical"] == json!(true) {
                "Compared revisions · identical".into()
            } else {
                format!("Compared revisions · {} changed", plural(changed, "object", "objects"))
            }
        }
        "render_image" => "Looked at the model".into(),
        "render_design" => {
            if let Some(url) = text(&s["exportUrl"]) {
                detail.insert("links".into(), json!([{"label": format!("Download {}", text(&s["format"]).unwrap_or("file").to_uppercase()), "url": url}]));
            }
            format!("Exported {}", text(&s["format"]).unwrap_or("the model").to_uppercase())
        }
        "export_print_parts" => {
            let links: Vec<Value> = s["files"]
                .as_array()
                .into_iter()
                .flatten()
                .take(20)
                .filter_map(|file| Some(json!({"label": text(&file["name"]).unwrap_or("part"), "url": text(&file["url"])?})))
                .collect();
            let n = links.len();
            detail.insert("links".into(), Value::Array(links));
            format!("Exported {} for printing", plural(n, "part", "parts"))
        }
        "get_print_layout" => match s["fits"].as_bool().or(s["layout"]["fits"].as_bool()) {
            Some(true) => "Read the print layout · fits the plate".into(),
            Some(false) => "Read the print layout · does not fit".into(),
            None => "Read the print layout".into(),
        },
        "split_object" => {
            let plane = &s["plane"];
            if let (Some(at), Some(normal)) = (point(&plane["point"]), point(&plane["normal"])) {
                let helper = if normal[2].abs() < 0.9 { [0.0, 0.0, 1.0] } else { [1.0, 0.0, 0.0] };
                let u = unit(cross(normal, helper));
                let v = unit(cross(normal, u));
                overlay.plane(at, u, v, 25.0, 25.0, "agent", Some("cut".into()));
            }
            for key in s["keys"]["positions"].as_array().into_iter().flatten() {
                if let Some(at) = point(key) {
                    overlay.dot(at, "info", Some("key".into()));
                }
            }
            format!("Planned a split of {} (not applied)", text(&s["object"]["name"]).unwrap_or("an object"))
        }
        "create_share_link" => {
            if let Some(url) = text(&s["url"]) {
                detail.insert("links".into(), json!([{"label": "View-only link", "url": url}]));
            }
            "Created a view-only link".into()
        }
        "get_design_link" => "Got the design link".into(),
        "set_print_orientation" if s["unchanged"] == json!(true) => "No print orientation to clear".into(),
        "update_design_code" | "restore_revision" | "set_parameters" | "set_text" | "set_print_orientation" => {
            let revision = s["revision"].as_u64().map(|r| format!(" · revision {r}")).unwrap_or_default();
            let named = text(&s["revisionName"]).map(|name| format!(" · {}", quoted(name))).unwrap_or_default();
            let what = match tool {
                "set_parameters" => {
                    let names: Vec<&str> = s["applied"].as_array().into_iter().flatten().filter_map(|a| a["name"].as_str()).take(4).collect();
                    if names.is_empty() { "Set parameters".to_string() } else { format!("Set {}", names.join(", ")) }
                }
                "set_text" => "Placed text".into(),
                "set_print_orientation" => "Set a print orientation".into(),
                "restore_revision" => match s["restoredFrom"].as_u64() {
                    Some(from) => format!("Restored revision {from}"),
                    None => "Restored a revision".into(),
                },
                _ => "Edited the source".into(),
            };
            format!("{what}{revision}{named}")
        }
        "list_revisions" => format!("Listed {}", plural(count(&s["revisions"]), "revision", "revisions")),
        "create_design" => "Created this design".into(),
        "upload_asset" | "create_asset_upload" => match text(&s["name"]).or(text(&arguments["name"])) {
            Some(name) if tool == "upload_asset" => format!("Uploaded {name}"),
            Some(name) => format!("Prepared an upload of {name}"),
            None => "Uploaded a mesh".into(),
        },
        "list_assets" => format!("Listed {}", plural(count(&s["assets"]), "mesh", "meshes")),
        "delete_asset" => format!("Deleted {}", text(&arguments["name"]).unwrap_or("a mesh")),
        "compare_surfaces" => {
            for patch in s["worst"].as_array().into_iter().flatten().take(12) {
                if let Some(at) = point(&patch["at"]) {
                    let label = patch["signed"].as_f64().or(patch["maxDeviation"].as_f64()).map(mm);
                    overlay.dot(at, "bad", label);
                }
            }
            let forward = &s["referenceToCandidate"];
            let verdict = if s["verdict"]["matches"] == json!(true) { "matches" } else { "does not match" };
            let rms = forward["rms"].as_f64().or(s["shapeAccuracy"]["rms"].as_f64()).map(|r| format!(" · rms {}", mm(r))).unwrap_or_default();
            let coverage = forward["coverage"].as_f64().map(|c| format!(" · coverage {:.0}%", c * 100.0)).unwrap_or_default();
            format!("Compared: {verdict}{rms}{coverage}")
        }
        "compare_sections" => {
            for plane in s["planes"].as_array().into_iter().flatten() {
                let geometry = &plane["plane"];
                let bounds = bounds2d(&plane["reference"]["bounds2d"]).or_else(|| bounds2d(&plane["candidate"]["bounds2d"]));
                let tone = if plane["passes"] == json!(true) { "good" } else { "bad" };
                overlay.plane_over(geometry, bounds, tone, text(&geometry["label"]).map(str::to_string));
                for worst in plane["worst"].as_array().into_iter().flatten().take(3) {
                    if let Some(at) = point(&worst["at3d"]).or_else(|| point(&worst["at"])) {
                        overlay.dot(at, "bad", worst["signed"].as_f64().map(mm));
                    }
                }
            }
            let summary = &s["summary"];
            match (summary["passing"].as_u64(), summary["compared"].as_u64()) {
                (Some(passing), Some(compared)) => format!("Compared sections with the scan · {passing} of {compared} planes pass"),
                _ => "Compared sections with the scan".into(),
            }
        }
        "measure_between" => {
            for example in s["examples"].as_array().into_iter().flatten().take(16) {
                if let (Some(a), Some(b)) = (point(&example["start"]), point(&example["end"])) {
                    let tone = if example["kind"] == json!("gap") { "info" } else { "agent" };
                    overlay.segment(a, b, tone, example["distance"].as_f64().map(mm));
                }
            }
            let evidence = text(&s["evidence"]).unwrap_or("measured");
            match s["overall"]["p50"].as_f64() {
                Some(p50) if evidence != "insufficient" => format!("Measured between surfaces · median {} ({evidence})", mm(p50)),
                _ => format!("Measured between surfaces · {evidence} evidence"),
            }
        }
        "fit_curve" => {
            let fit = &s["fit"];
            let within = if fit["withinTolerance"] == json!(true) { "within tolerance" } else { "outside tolerance" };
            if let Some(at) = point(&s["comparison"]["worstSample"]["at3d"]) {
                overlay.dot(at, "bad", s["comparison"]["worstSample"]["distance"].as_f64().map(mm));
            }
            // The error and the tolerance, so a run of refits reads as progress.
            let error = match (fit["maxErrorAccepted"].as_f64(), fit["tolerance"].as_f64()) {
                (Some(max), Some(tolerance)) => format!(" · max {} (tol {})", mm(max), mm(tolerance)),
                _ => String::new(),
            };
            let spans = count(&s["unsupportedSpans"]);
            let gaps = if spans > 0 { format!(" · {}", plural(spans, "unsupported span", "unsupported spans")) } else { String::new() };
            match fit["controlPointsUsed"].as_u64() {
                Some(n) => format!("Fitted a curve · {n} control points{error}{gaps} · {within}"),
                None => "Fitted a curve".into(),
            }
        }
        "scan_report" => {
            let verdict = &s["verdict"];
            let matches = if verdict["matches"] == json!(true) { "matches" } else { "does not match" };
            let verified = if verdict["verified"] == json!(true) { ", verified" } else { ", not verified" };
            format!("Checked against the scan: {matches}{verified}")
        }
        "list_feedback" => format!("Read {}", plural(count(&s["comments"]), "comment", "comments")),
        "request_pointing" => "Asked you to point at the model".into(),
        "resolve_feedback" => format!("Answered {}", plural(count(&s["resolved"]).max(count(&arguments["ids"])), "comment", "comments")),
        "jobs" => format!("Checked its jobs · {} running or recent", count(&s["jobs"])),
        "get_supported_scad" => "Read the SCAD dictionary".into(),
        "get_reverse_engineering_guide" => "Read the reverse-engineering guide".into(),
        other => tool_title(other),
    }
}

/// Intersection findings for the browser's inspection panel, trimmed. The
/// problems come first, so a long list never loses one and reads as clear.
fn slim_pairs(s: &Value, revision: Option<u64>) -> Value {
    let all = s["intersections"].as_array().cloned().unwrap_or_default();
    let problem = |pair: &Value| match pair["relation"].as_str() {
        Some("overlap" | "clearanceViolation" | "skipped" | "undetermined") => true,
        Some("contact") => pair["toleranceViolated"] != json!(false),
        _ => pair["skipped"] == json!(true) || pair["determined"] == json!(false),
    };
    let (problems, rest): (Vec<&Value>, Vec<&Value>) = all.iter().partition(|pair| problem(pair));
    let dropped = all.len().saturating_sub(MAX_PAIRS);
    let pairs: Vec<Value> = problems
        .into_iter()
        .chain(rest)
        .take(MAX_PAIRS)
        .map(|pair| {
            let mut slim = Map::new();
            for key in [
                "a", "b", "aName", "bName", "relation", "clearance", "depth", "overlapVolume", "overlapBounds", "location",
                "closestPoints", "toleranceViolated", "contactAllowed", "determined", "skipped",
            ] {
                if let Some(value) = pair.get(key) {
                    slim.insert(key.into(), value.clone());
                }
            }
            Value::Object(slim)
        })
        .collect();
    json!({
        "intersections": pairs,
        "tolerance": s["tolerance"],
        "complete": s["complete"],
        "summary": s["summary"],
        "revision": revision,
        "truncated": dropped,
    })
}

fn unit(a: [f64; 3]) -> [f64; 3] {
    let length = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt().max(1e-12);
    a.map(|x| x / length)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scancommon::testkit;

    const CODE: &str = "w = 40;\nmodule base() cube([w, 30, 10]);\nbase();\ntranslate([10, 10, 9.9]) cylinder(h = 20, r = 6, $fn = 32);\n";

    fn design(state: &AppState) -> String {
        let reply = testkit::call(state, "create_design", json!({"code": CODE, "revisionName": "Test revision"}));
        assert_eq!(reply["isError"], json!(false), "{reply}");
        reply["structuredContent"]["designId"].as_str().unwrap().to_string()
    }

    fn entries(id: &str) -> Vec<Value> {
        since(id, 0)["entries"].as_array().cloned().unwrap()
    }

    fn last(id: &str, tool: &str) -> Value {
        entries(id).into_iter().rev().find(|entry| entry["tool"] == tool).unwrap_or_else(|| panic!("no {tool} entry"))
    }

    #[test]
    fn every_call_on_a_design_is_recorded_with_what_it_found() {
        let (state, _root) = testkit::state();
        let id = design(&state);
        assert_eq!(last(&id, "create_design")["summary"], "Created this design");

        testkit::call(&state, "get_design_code", json!({"designId": id, "startLine": 2, "endLine": 3}));
        let read = last(&id, "get_design_code");
        assert_eq!(read["state"], "done");
        assert_eq!(read["read"]["ranges"], json!([[2, 3]]));
        assert!(read["summary"].as_str().unwrap().starts_with("Read lines 2–3"), "{read}");

        testkit::call(&state, "check_intersections", json!({"designId": id}));
        let check = last(&id, "check_intersections");
        assert!(check["summary"].as_str().unwrap().contains("1 overlap"), "{check}");
        let overlay = check["overlay"].as_array().unwrap();
        assert!(overlay.iter().any(|item| item["kind"] == "box" && item["tone"] == "bad"), "{check}");
        assert!(overlay.iter().any(|item| item["kind"] == "point"), "{check}");
        // Findings on the saved model also go to the inspection panel, with their revision.
        assert_eq!(check["intersections"]["revision"], json!(1));
        assert_eq!(check["intersections"]["intersections"].as_array().unwrap().len(), 1);

        testkit::call(&state, "probe", json!({"designId": id, "rays": [{"origin": [5, 5, 50], "direction": [0, 0, -1]}]}));
        let probe = last(&id, "probe");
        let segment = probe["overlay"].as_array().unwrap().iter().find(|item| item["kind"] == "segment").cloned().unwrap();
        assert_eq!(segment["a"], json!([5.0, 5.0, 50.0]));
        assert_eq!(segment["b"], json!([5.0, 5.0, 10.0]));
    }

    #[test]
    fn evaluate_with_code_says_what_it_saved_and_fills_the_panel() {
        let (state, _root) = testkit::state();
        let id = design(&state);
        let saved = crate::mcp_call_tool(&state, &testkit::request(), "evaluate", json!({"designId": id, "code": "w = 60;\ncube(w);\n", "revisionName": "Wider", "expectedRevision": 1}));
        assert_eq!(saved["isError"], json!(false), "{saved}");
        let entry = last(&id, "evaluate");
        assert!(entry["summary"].as_str().unwrap().starts_with("Saved “Wider” and checked it"), "{entry}");
        assert!(entry.get("variant").is_none(), "{entry}");
        assert_eq!(entry["revision"], json!(2));
        assert_eq!(entry["intersections"]["revision"], json!(2));
    }

    #[test]
    fn unknown_designs_get_no_log_and_failures_say_why() {
        let (state, _root) = testkit::state();
        testkit::call(&state, "get_design_code", json!({"designId": "no-such-design-anywhere"}));
        assert_eq!(version("no-such-design-anywhere"), 0);

        let id = design(&state);
        testkit::call(&state, "section_profile", json!({"designId": id}));
        let failed = last(&id, "section_profile");
        assert_eq!(failed["state"], "error");
        assert!(failed["error"].as_str().unwrap().contains("plane"), "{failed}");
        assert!(failed["summary"].as_str().unwrap().ends_with("failed"), "{failed}");
    }

    #[test]
    fn polls_get_only_what_changed_and_a_fresh_page_the_recent_ones() {
        let (state, _root) = testkit::state();
        let id = design(&state);
        let seen = version(&id);
        assert!(seen > 0);
        assert!(since(&id, seen)["entries"].as_array().unwrap().is_empty());
        for _ in 0..40 {
            testkit::call(&state, "list_modules", json!({"designId": id}));
        }
        assert!(version(&id) > seen);
        assert_eq!(entries(&id).len(), INITIAL_ENTRIES);
        assert_eq!(since(&id, seen)["entries"].as_array().unwrap().len(), 40);
        let log_len = lock().logs.get(&id).map(|log| log.entries.len()).unwrap();
        assert!(log_len <= MAX_ENTRIES);
        forget(&id);
        assert_eq!(version(&id), 0);
    }

    #[test]
    fn parameter_reads_tint_their_lines() {
        let (state, _root) = testkit::state();
        let id = design(&state);
        testkit::call(&state, "get_parameters", json!({"designId": id}));
        assert_eq!(last(&id, "get_parameters")["read"]["ranges"], json!([[1, 1]]));
    }

    #[test]
    fn a_long_pair_list_keeps_its_problems_and_says_it_was_cut() {
        let clear = json!({"a": "a", "b": "b", "relation": "clear"});
        let mut pairs = vec![clear; MAX_PAIRS + 5];
        pairs.push(json!({"a": "x", "b": "y", "relation": "overlap", "overlapVolume": 2.0}));
        let slim = slim_pairs(&json!({"intersections": pairs, "complete": true}), Some(3));
        assert_eq!(slim["intersections"][0]["relation"], "overlap");
        assert_eq!(slim["intersections"].as_array().unwrap().len(), MAX_PAIRS);
        assert_eq!(slim["truncated"], json!(6));
    }

    #[test]
    fn findings_name_the_saved_revision_they_describe() {
        let (state, _root) = testkit::state();
        let id = design(&state);
        testkit::call(&state, "measure", json!({"designId": id}));
        assert_eq!(last(&id, "measure")["revision"], json!(1));
        testkit::call(&state, "set_parameters", json!({"designId": id, "expectedRevision": 1, "revisionName": "w 45", "values": {"w": 45}}));
        testkit::call(&state, "measure", json!({"designId": id}));
        assert_eq!(last(&id, "measure")["revision"], json!(2));
    }

    #[test]
    fn stored_names_and_summaries_are_bounded() {
        let (state, _root) = testkit::state();
        let id = design(&state);
        let long = "x".repeat(100_000);
        testkit::call(&state, &long, json!({"designId": id}));
        let entry = entries(&id).into_iter().last().unwrap();
        assert!(entry["tool"].as_str().unwrap().chars().count() <= MAX_TOOL_CHARS);
        assert!(entry["summary"].as_str().unwrap().chars().count() <= MAX_SUMMARY_CHARS);
    }

    #[test]
    fn a_ticket_dropped_unfinished_is_marked_stopped() {
        let (state, _root) = testkit::state();
        let id = design(&state);
        let ticket = begin(&state, &id, "fit_axis", &json!({"designId": id})).unwrap();
        assert_eq!(last(&id, "fit_axis")["state"], "running");
        assert_eq!(last(&id, "fit_axis")["summary"], "Fitting an axis");
        drop(ticket);
        let stopped = last(&id, "fit_axis");
        assert_eq!(stopped["state"], "error");
        assert!(stopped["error"].as_str().unwrap().contains("stopped"));
    }

    #[test]
    fn offered_surfaces_are_kept_only_inside_a_capture_grouped_by_colour() {
        let mesh = TriMesh {
            points: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]],
            triangles: vec![[0, 1, 2], [0, 2, 3]],
        };
        let red = [200, 30, 30, 255];
        let blue = [30, 70, 200, 255];
        assert!(!collecting());
        offer_painted("outside", &mesh, &[red, blue], json!({}));
        let ((), offered) = capture(|| {
            assert!(collecting());
            offer_painted("heat", &mesh, &[blue, red], json!({"title": "deviation"}));
            // Colours must match the triangles one to one.
            offer_painted("bad", &mesh, &[red], json!({}));
        });
        assert!(!collecting());
        assert_eq!(offered.surfaces.len(), 1);
        let surface = &offered.surfaces[0];
        assert_eq!(surface.label, "heat");
        assert_eq!(surface.stl.len(), 84 + 2 * 50);
        assert_eq!(surface.runs.len(), 2);
        assert_eq!(surface.runs.iter().map(|run| run[1]).sum::<u32>(), 2);
        assert_eq!(surface.palette.len(), 2);
    }

    #[test]
    fn pictures_are_served_and_dropped_oldest_first_over_the_budget() {
        let (state, _root) = testkit::state();
        let id = design(&state);
        testkit::call(&state, "render_image", json!({"designId": id}));
        let looked = last(&id, "render_image");
        let url = looked["images"][0]["url"].as_str().unwrap().to_string();
        let seq = looked["seq"].as_u64().unwrap();
        assert_eq!(url, format!("/api/designs/{id}/activity/{seq}/0.png"));
        let (kind, bytes) = file(&id, seq, "0.png").unwrap();
        assert_eq!(kind, "image/png");
        assert_eq!(&bytes[..4], b"\x89PNG");

        // Big files past the design's budget push the oldest ones out, detail and all.
        let big = Arc::new(vec![0u8; MAX_DESIGN_BYTES / 3]);
        for _ in 0..4 {
            let ticket = begin(&state, &id, "render_image", &json!({})).unwrap();
            let mut ticket = ticket;
            ticket.finished = true;
            let seq = ticket.seq;
            update(&id, seq, |entry| {
                entry.running = false;
                entry.blobs = vec![Blob { name: "0.png".into(), content_type: "image/png", bytes: Arc::clone(&big) }];
                entry.detail.insert("images".into(), json!([{"file": "0.png", "title": "big"}]));
            });
            lock().trim(&id);
        }
        assert!(file(&id, seq, "0.png").is_none());
        assert!(lock().logs.get(&id).unwrap().bytes() <= MAX_DESIGN_BYTES);
        let first = entries(&id).into_iter().find(|entry| entry["seq"] == json!(seq)).unwrap();
        assert!(first.get("images").is_none(), "{first}");
    }

    #[test]
    fn a_rejected_write_reads_as_failed_not_as_saved() {
        let (state, _root) = testkit::state();
        let id = design(&state);
        testkit::call(&state, "update_design_code", json!({"designId": id, "expectedRevision": 1, "code": "cube(;", "revisionName": "Broken"}));
        let entry = last(&id, "update_design_code");
        assert!(entry["summary"].as_str().unwrap().ends_with("failed"), "{entry}");
        assert!(entry["error"].is_string(), "{entry}");
    }
}
