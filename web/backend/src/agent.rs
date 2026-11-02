//! Analysis tools for agents: object listing, measurement, revision diff,
//! PNG rendering, intersection/clearance checks and top-level parameters.
//!
//! Every tool reads a design's current revision, the one its viewport shows:
//! what an agent inspects is always what the person sees. To try something the
//! agent saves it as a named revision (`evaluate` can do that in the same
//! call), and `restore_revision` copies an old revision on top. Each tool is
//! exposed twice from the same function: as an HTTP route
//! (`/api/designs/{id}/{tool}`, alias `/api/workspaces/{id}/{tool}`) and as an
//! MCP tool, so the two can never drift apart.
//!
//! The browser alone also sends `code` to the intersection routes (including
//! the stateless `/api/intersections`), because a person's check covers the
//! editor's text as they typed it; those are the only routes that read a body's
//! source, see [`BROWSER_CODE_ACTIONS`].
//!
//! Cost control is the server's usual: the routes are metered as evaluator
//! routes by `route_cost`, each request holds one render-queue permit for its
//! whole run, every compile runs under the render deadline and the exact
//! booleans are capped by triangle count.

use crate::vec3::{cross as cross3, dot as dot3, norm as norm3};
use super::*;
use crate::analysis::{self, MeshIndex, P};
use crate::scadsource;
use crate::support;

/// Wall-clock budget for one analysis request, counted from the moment it
/// starts (its compile included), so the evaluator permit it holds is
/// released within roughly this long. Exact booleans abort once it passes
/// (reported as undetermined or skipped, never as clear).
pub(crate) const BATCH_BUDGET: Duration = Duration::from_secs(90);
/// Most IDs any one ID list (objectIds, hiddenObjectIds, visibleObjectIds,
/// a group, allowContact pairs) may carry; checked before anything else.
const MAX_ID_LIST: usize = 4096;
/// Most triangles (summed over its members) a group may union exactly.
const MAX_GROUP_TRIANGLES: usize = analysis::MAX_BOOLEAN_TRIANGLES;

/// Starts a request's analysis budget: the deadline, and the kernel scope
/// that makes exact booleans on this thread abort once it passes.
fn request_deadline(started: Instant) -> (Instant, crate::csg::bsp::DeadlineScope) {
    let deadline = started + BATCH_BUDGET;
    (deadline, crate::csg::bsp::deadline_scope(deadline))
}
/// Most explicit distance pairs one `measure` call computes.
const MAX_MEASURE_PAIRS: usize = 32;
/// Most visible objects an intersection check compares pairwise.
const MAX_CHECKED_OBJECTS: usize = 200;
/// Default image size for agents: big enough to read, small enough to keep a
/// tool result light.
const DEFAULT_IMAGE_WIDTH: u32 = 800;
const DEFAULT_IMAGE_HEIGHT: u32 = 600;

/// A failure with the HTTP status it should carry. MCP uses the message.
#[derive(Debug)]
pub(crate) struct ApiError {
    pub status: u16,
    pub message: String,
}

impl ApiError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    fn bad(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }
}

// ---------------------------------------------------------------------------
// What gets evaluated.
// ---------------------------------------------------------------------------

/// The source an analysis evaluates, and where it came from.
pub(crate) struct EvalSource {
    /// What compiles: `base`, plus any instrumentation (see [`probe_source`]).
    pub source: String,
    /// The source as written.
    pub base: String,
    pub workspace: Option<Workspace>,
    /// The revision evaluated, or `None` for a browser's own text.
    pub revision: Option<u64>,
    pub overridden: scadsource::Overridden,
    /// Rewritten by [`probe_source`] (asserts reported, values echoed): its
    /// compile must not stand for the revision's in the object cache.
    pub instrumented: bool,
}

impl EvalSource {
    pub fn header(&self) -> Value {
        json!({
            "designId": self.workspace.as_ref().map(|workspace| workspace.id.clone()),
            "workspaceId": self.workspace.as_ref().map(|workspace| workspace.id.clone()),
            "revision": self.revision,
        })
    }

    fn tolerance(&self) -> f64 {
        self.workspace
            .as_ref()
            .map_or(DEFAULT_TOLERANCE, |workspace| workspace.tolerance)
    }

    /// A rewritten-source byte range as original-source lines and offsets.
    pub(crate) fn source_location(&self, range: Option<engine::SourceRange>) -> Value {
        let Some(range) = range else {
            return Value::Null;
        };
        let start = self
            .overridden
            .original_offset(range.start)
            .min(self.base.len());
        let end = self
            .overridden
            .original_offset(range.end)
            .clamp(start, self.base.len());
        json!({
            "start": start,
            "end": end,
            "startLine": scadsource::line_of(&self.base, start),
            "endLine": scadsource::line_of(&self.base, end.saturating_sub(1).max(start)),
        })
    }

    /// A rewritten-source range as `{start, end}` in original-source bytes.
    fn original_range(&self, range: Option<engine::SourceRange>) -> Value {
        match self.source_location(range) {
            Value::Null => Value::Null,
            location => json!({"start": location["start"], "end": location["end"]}),
        }
    }

}

fn input_code(input: &Value, key: &str) -> Result<Option<String>, ApiError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(code)) => {
            if code.trim().is_empty() {
                return Err(ApiError::bad(format!("{key} is empty.")));
            }
            if code.len() > MAX_BODY {
                return Err(ApiError::new(
                    413,
                    format!("{key} exceeds the 2 MiB limit."),
                ));
            }
            Ok(Some(code.clone()))
        }
        Some(_) => Err(ApiError::bad(format!(
            "{key} must be a string of SCAD source."
        ))),
    }
}

fn load_workspace(state: &AppState, id: &str) -> Result<Workspace, ApiError> {
    if !valid_workspace_id(id) {
        return Err(ApiError::bad("designId is invalid."));
    }
    state
        .workspaces
        .get(id)
        .map_err(|error| ApiError::new(500, error))?
        .ok_or_else(|| ApiError::new(404, "Design not found."))
}

/// The source of `revision` of `workspace`, from the current document or the
/// retained history.
pub(crate) fn revision_source(
    state: &AppState,
    workspace: &Workspace,
    revision: u64,
) -> Result<String, ApiError> {
    if revision == workspace.revision {
        return Ok(workspace.code.clone());
    }
    if revision > workspace.revision || revision == 0 {
        return Err(ApiError::new(
            404,
            format!(
                "Revision {revision} does not exist; the current revision is {}.",
                workspace.revision
            ),
        ));
    }
    match state
        .workspaces
        .revision_code(&workspace.id, revision)
        .map_err(|error| ApiError::new(500, error))?
    {
        Some(code) => Ok(code),
        None => {
            let retained: Vec<u64> = state
                .workspaces
                .revision_list(&workspace.id)
                .unwrap_or_default()
                .into_iter()
                .map(|info| info.revision)
                .collect();
            Err(ApiError::new(
                404,
                format!(
                    "Revision {revision} is not retained. Retained revisions: {:?} plus the current {} (the last {MAX_REVISION_HISTORY} are kept, fewer on a large source; a browser autosave is replaced by the next one).",
                    retained, workspace.revision
                ),
            ))
        }
    }
}

/// Resolves what an analysis evaluates: the design's current revision, or the
/// `code` a browser route (and the server's own validation of what it
/// generated) passes in. The MCP tools and agent routes refuse `code` first.
pub(crate) fn eval_source(
    state: &AppState,
    workspace_id: Option<&str>,
    input: &Value,
) -> Result<EvalSource, ApiError> {
    let workspace_id = workspace_id.or_else(|| crate::input_design_id(input));
    let code = input_code(input, "code")?;
    let workspace = workspace_id
        .map(|id| load_workspace(state, id))
        .transpose()?;
    let (base, revision) = match (code, &workspace) {
        (Some(code), _) => (code, None),
        (None, Some(workspace)) => (workspace.code.clone(), Some(workspace.revision)),
        (None, None) => return Err(ApiError::bad("designId is required.")),
    };
    let overridden = scadsource::apply_overrides(&base, &[], true).map_err(ApiError::bad)?;
    Ok(EvalSource {
        source: overridden.source.clone(),
        base,
        workspace,
        revision,
        overridden,
        instrumented: false,
    })
}

/// Compiles an [`EvalSource`] at preview quality (the same mesh the viewer
/// draws and every export writes, under the exact kernel), remembering the
/// object list when it is a workspace's current revision.
pub(crate) fn compile_eval(
    source: &EvalSource,
) -> Result<Arc<engine::MultipartCompileOutput>, ApiError> {
    let current_revision = match (&source.workspace, source.revision) {
        (Some(workspace), Some(revision))
            if !source.instrumented && revision == workspace.revision =>
        {
            Some((workspace.id.as_str(), revision))
        }
        _ => None,
    };
    let compiled = compile_parts_guarded(&source.source, engine::Quality::Preview).map_err(|error| {
        if let Some((id, revision)) = current_revision {
            remember_compile_failure(id, revision, &error);
        }
        ApiError::new(422, error)
    })?;
    if multipart_binary_size(&compiled, true).is_none_or(|size| size > MAX_OUTPUT) {
        return Err(ApiError::new(
            422,
            "Compiled multipart output exceeds the 64 MiB limit.",
        ));
    }
    if let (Some(workspace), Some(revision)) = (&source.workspace, source.revision) {
        if !source.instrumented && revision == workspace.revision {
            remember_objects(&workspace.id, revision, &compiled.parts);
        }
    }
    Ok(compiled)
}

// ---------------------------------------------------------------------------
// Object summaries and the cache that serves them on workspace reads.
// ---------------------------------------------------------------------------

/// What the last compile of a workspace revision says about its objects.
#[derive(Clone, Debug)]
pub(crate) enum CachedObjects {
    /// The object summaries.
    Listed(Vec<Value>),
    /// More objects than a workspace read lists; `/objects` lists them.
    TooMany(usize),
    /// The revision does not compile; the error.
    Failed(String),
}

/// Object summaries of recently compiled workspace revisions, so `GET
/// /api/designs/{id}` can list objects without compiling. Keyed by
/// `(id, revision, asset digest)`: the revision identifies the source and
/// the digest (`assets::current_digest`, read inside the compile's asset
/// scope) the meshes it imports, so a compile that started before an asset
/// write, or ran on another instance, can never answer for the new assets.
/// Revisions that have too many objects or do not compile are remembered
/// too, so a read never recompiles them just to find that out again.
static OBJECT_SUMMARIES: Mutex<VecDeque<(String, u64, String, CachedObjects)>> =
    Mutex::new(VecDeque::new());
const OBJECT_SUMMARY_ENTRIES: usize = 256;

/// Models with more objects than this are not listed on a workspace read
/// (`/objects` lists them), so the cache stays a few MB at most.
const OBJECT_SUMMARY_MAX_PARTS: usize = 512;

fn remember(id: &str, revision: u64, entry: CachedObjects) {
    let assets = crate::assets::current_digest();
    if let Ok(mut cache) = OBJECT_SUMMARIES.lock() {
        cache.retain(|(key, _, _, _)| key != id);
        cache.push_back((id.to_string(), revision, assets, entry));
        while cache.len() > OBJECT_SUMMARY_ENTRIES {
            cache.pop_front();
        }
    }
}

/// Drops design `id`'s cached object list: its assets changed, so the same
/// revision now compiles to different geometry.
pub(crate) fn forget_objects(id: &str) {
    if let Ok(mut cache) = OBJECT_SUMMARIES.lock() {
        cache.retain(|(key, _, _, _)| key != id);
    }
}

pub(crate) fn remember_objects(id: &str, revision: u64, parts: &[engine::CompiledPart]) {
    let entry = if parts.len() > OBJECT_SUMMARY_MAX_PARTS {
        CachedObjects::TooMany(parts.len())
    } else {
        CachedObjects::Listed(parts.iter().map(part_summary_public).collect())
    };
    remember(id, revision, entry);
}

/// What [`remember_objects`] would record for `parts`, as JSON: the summary
/// list, or the part count when there are too many to list. Stored with a
/// cached render so a hit can fill the object cache without compiling.
pub(crate) fn object_summaries_value(parts: &[engine::CompiledPart]) -> Value {
    if parts.len() > OBJECT_SUMMARY_MAX_PARTS {
        json!(parts.len())
    } else {
        Value::Array(parts.iter().map(part_summary_public).collect())
    }
}

/// Records an [`object_summaries_value`] for `(id, revision)`.
pub(crate) fn remember_object_summaries(id: &str, revision: u64, value: &Value) {
    match value {
        Value::Array(summaries) => remember(id, revision, CachedObjects::Listed(summaries.clone())),
        Value::Number(count) => {
            if let Some(count) = count.as_u64() {
                remember(id, revision, CachedObjects::TooMany(count as usize));
            }
        }
        _ => {}
    }
}

/// Records that `revision` of `id` does not compile.
///
/// Failures that depend on load rather than on the source (a time limit, the
/// memory budget, a queue) are not remembered: the next read may succeed.
pub(crate) fn remember_compile_failure(id: &str, revision: u64, error: &str) {
    let lowered = error.to_ascii_lowercase();
    // Asset read failures (`assets::try_lookup`) depend on storage, not on the
    // source, so they are not remembered either.
    if ["time limit", "time budget", "memory", "cancel", "busy", "queue", "while this compile ran", "could not be read", "missing from storage", "no longer matches", "could not be decoded"]
        .iter()
        .any(|transient| lowered.contains(transient))
    {
        return;
    }
    remember(id, revision, CachedObjects::Failed(error.chars().take(2000).collect()));
}

/// What is known about the objects of `(id, revision)` without compiling.
pub(crate) fn object_cache(id: &str, revision: u64) -> Option<CachedObjects> {
    let assets = crate::assets::current_digest();
    let cache = OBJECT_SUMMARIES.lock().ok()?;
    cache
        .iter()
        .find(|(key, cached, digest, _)| key == id && *cached == revision && *digest == assets)
        .map(|(_, _, _, entry)| entry.clone())
}

/// `(objectsStatus, objects, objectsHint)` for a workspace read.
pub(crate) fn objects_status(id: &str, revision: u64) -> (&'static str, Vec<Value>, Option<String>) {
    match object_cache(id, revision) {
        Some(CachedObjects::Listed(objects)) => ("current", objects, None),
        Some(CachedObjects::TooMany(count)) => (
            "too_many",
            Vec::new(),
            Some(format!(
                "This revision has {count} objects, more than the {OBJECT_SUMMARY_MAX_PARTS} a design read lists. Call POST /api/designs/{{id}}/objects or the MCP list_design_objects tool."
            )),
        ),
        Some(CachedObjects::Failed(error)) => (
            "failed",
            Vec::new(),
            Some(format!("This revision does not compile: {error}")),
        ),
        None => ("not_compiled", Vec::new(), None),
    }
}

/// Exact axis-aligned bounds of a mesh.
pub(crate) fn mesh_bounds(mesh: &engine::Mesh) -> Option<analysis::Aabb> {
    let mut bounds = analysis::Aabb::empty();
    for triangle in &mesh.triangles {
        for vertex in triangle.vertices {
            bounds.grow([vertex.x, vertex.y, vertex.z]);
        }
    }
    (!bounds.is_empty()).then_some(bounds)
}

fn aabb_public(bounds: &analysis::Aabb) -> Value {
    json!({
        "min": bounds.min,
        "max": bounds.max,
        "size": [bounds.max[0] - bounds.min[0], bounds.max[1] - bounds.min[1], bounds.max[2] - bounds.min[2]],
    })
}

fn object_summary(part: &engine::CompiledPart, source: &EvalSource) -> Value {
    let mut value = part_summary_public(part);
    value["sourceRange"] = source.original_range(part.source_range);
    value["source"] = source.source_location(part.source_range);
    value
}

/// `objects` for one source.
pub(crate) fn objects_value(
    state: &AppState,
    id: Option<&str>,
    input: &Value,
) -> Result<Value, ApiError> {
    let started = Instant::now();
    let source = eval_source(state, id, input)?;
    let compiled = compile_eval(&source)?;
    let mut value = source.header();
    value["ok"] = json!(true);
    value["objects"] = json!(compiled
        .parts
        .iter()
        .map(|part| object_summary(part, &source))
        .collect::<Vec<_>>());
    value["backgroundObjects"] = json!(compiled
        .background_parts
        .iter()
        .map(|part| object_summary(part, &source))
        .collect::<Vec<_>>());
    attach_diagnostics(&mut value, &compiled.messages, started);
    Ok(value)
}

/// Resolves `objectIds` (keep) / `hiddenObjectIds` (drop) into parts.
fn select_parts<'a>(
    parts: &'a [engine::CompiledPart],
    input: &Value,
    keep_key: &str,
) -> Result<Vec<&'a engine::CompiledPart>, ApiError> {
    let strings = |key: &str| -> Result<Option<Vec<String>>, ApiError> {
        match input.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Array(values)) => values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| ApiError::bad(format!("{key} must contain only strings.")))
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Some),
            Some(_) => Err(ApiError::bad(format!(
                "{key} must be an array of object IDs."
            ))),
        }
    };
    let keep = strings(keep_key)?;
    let hidden = strings("hiddenObjectIds")?;
    for (key, list) in [(keep_key, &keep), ("hiddenObjectIds", &hidden)] {
        if list.as_ref().is_some_and(|list| list.len() > MAX_ID_LIST) {
            return Err(ApiError::bad(format!("{key} may list at most {MAX_ID_LIST} IDs.")));
        }
    }
    if keep.is_some() && hidden.is_some() {
        return Err(ApiError::bad(format!(
            "Provide {keep_key} or hiddenObjectIds, not both."
        )));
    }
    let available: HashSet<&str> = parts.iter().map(|part| part.id.as_str()).collect();
    for id in keep.iter().chain(hidden.iter()).flatten() {
        if !available.contains(id.as_str()) {
            return Err(ApiError::bad(format!(
                "Unknown object ID {id:?}. Objects are: {}.",
                parts.iter().map(|part| part.id.as_str()).collect::<Vec<_>>().join(", ")
            )));
        }
    }
    let as_set = |list: &Option<Vec<String>>| -> Option<HashSet<String>> {
        list.as_ref().map(|list| list.iter().cloned().collect())
    };
    let (keep, hidden) = (as_set(&keep), as_set(&hidden));
    Ok(parts
        .iter()
        .filter(|part| {
            keep.as_ref().is_none_or(|keep| keep.contains(&part.id))
                && hidden
                    .as_ref()
                    .is_none_or(|hidden| !hidden.contains(&part.id))
        })
        .collect())
}

// ---------------------------------------------------------------------------
// measure
// ---------------------------------------------------------------------------

fn measure_public(measure: &analysis::MeshMeasure) -> Value {
    let mut value = json!({
        "bounds": aabb_public(&measure.bounds),
        "volume": measure.volume,
        "surfaceArea": measure.surface_area,
        "triangles": measure.triangles,
        "vertices": measure.vertices,
        "components": measure.components,
        "watertight": measure.watertight(),
        "manifold": measure.manifold(),
        "windingConsistent": measure.winding_consistent(),
        "outwardNormals": measure.volume >= 0.0,
        "boundaryEdges": measure.boundary_edges,
        "nonManifoldEdges": measure.non_manifold_edges,
        "inconsistentEdges": measure.inconsistent_edges,
        "degenerateTriangles": measure.degenerate_triangles,
    });
    if !measure.complete {
        // Stopped by the deadline or a cancellation: the topology is unknown,
        // not good or bad.
        for key in ["watertight", "manifold", "windingConsistent", "boundaryEdges", "nonManifoldEdges", "inconsistentEdges", "components"] {
            value[key] = Value::Null;
        }
        value["complete"] = json!(false);
        value["incompleteReason"] = json!("the time budget ran out (or the call was cancelled) before the topology census finished");
        return value;
    }
    if !measure.defects.is_empty() {
        value["defects"] = json!(measure.defects.iter().map(defect_public).collect::<Vec<_>>());
        if measure.defect_clusters > measure.defects.len() {
            value["defectClusters"] = json!(measure.defect_clusters);
        }
    }
    value
}

/// One group of open or non-manifold edges, located: `kind`, `edges`,
/// `centroid`, `bounds` `[min, max]` and up to three sample edges, all in
/// model millimetres rounded to 0.001.
fn defect_public(cluster: &analysis::DefectCluster) -> Value {
    json!({
        "kind": cluster.kind,
        "edges": cluster.edges,
        "centroid": point3(cluster.centroid),
        "bounds": [point3(cluster.bounds.min), point3(cluster.bounds.max)],
        "samples": cluster
            .samples
            .iter()
            .map(|[a, b]| json!([point3(*a), point3(*b)]))
            .collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// Diagnostics envelope shared by every evaluating response.
// ---------------------------------------------------------------------------

/// Console lines that are warnings or errors rather than echo/info output.
pub(crate) fn warning_lines(messages: &[String]) -> Vec<&String> {
    messages
        .iter()
        .filter(|line| {
            let line = line.trim_start();
            line.starts_with("WARNING") || line.starts_with("ERROR") || line.starts_with("DEPRECATED")
        })
        .collect()
}

/// Adds `console` (every line the evaluator printed, ECHO included),
/// `warnings` (the WARNING/ERROR/DEPRECATED subset) and `elapsedMs` to a
/// response, so no evaluating tool needs a second call to see diagnostics.
fn attach_diagnostics(value: &mut Value, messages: &[String], started: Instant) {
    value["console"] = json!(messages);
    value["warnings"] = json!(warning_lines(messages));
    value["elapsedMs"] = json!(started.elapsed().as_millis());
}

// ---------------------------------------------------------------------------
// Measurement targets: objects, their separate solids, and groups.
// ---------------------------------------------------------------------------

/// One thing measured or checked: a top-level object, one separate solid of
/// an object (`splitComponents`), or the union of several objects (`groups`).
struct Target<'a> {
    id: String,
    name: String,
    /// `"object"`, `"component"` or `"group"`.
    kind: &'static str,
    mesh: std::borrow::Cow<'a, engine::Mesh>,
    source_range: Option<engine::SourceRange>,
    color: Option<[f64; 4]>,
    /// Every colour of an object painted in several, `None` for the default
    /// material; empty when the object is one colour (`color` says which).
    colors: Vec<Option<[f64; 4]>>,
    /// Group members, or the parent object of a component.
    members: Vec<String>,
}

impl<'a> Target<'a> {
    fn object(part: &'a engine::CompiledPart) -> Self {
        Self {
            id: part.id.clone(),
            name: part.name.clone(),
            kind: "object",
            mesh: std::borrow::Cow::Borrowed(&part.mesh),
            source_range: part.source_range,
            color: part.color,
            colors: part
                .face_colors
                .as_ref()
                .map(|colors| colors.palette.clone())
                .unwrap_or_default(),
            members: Vec::new(),
        }
    }

    /// Identity fields shared by every response that lists targets.
    fn identity(&self, source: &EvalSource) -> Value {
        let mut value = json!({
            "id": self.id,
            "name": self.name,
            "kind": self.kind,
            "color": self.color,
            "source": source.source_location(self.source_range),
        });
        if !self.colors.is_empty() {
            value["colors"] = json!(self.colors);
        }
        match self.kind {
            "group" => value["members"] = json!(self.members),
            "component" => value["parent"] = json!(self.members.first()),
            _ => {}
        }
        value
    }
}

/// Triangles back into an engine mesh (normals recomputed).
fn mesh_from_triangles(triangles: &[[P; 3]]) -> engine::Mesh {
    let vec3 = |p: P| engine::Vec3 {
        x: p[0],
        y: p[1],
        z: p[2],
    };
    engine::Mesh {
        triangles: triangles
            .iter()
            .map(|t| {
                let u = [t[1][0] - t[0][0], t[1][1] - t[0][1], t[1][2] - t[0][2]];
                let v = [t[2][0] - t[0][0], t[2][1] - t[0][1], t[2][2] - t[0][2]];
                let n = [
                    u[1] * v[2] - u[2] * v[1],
                    u[2] * v[0] - u[0] * v[2],
                    u[0] * v[1] - u[1] * v[0],
                ];
                let length = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                let n = if length > 0.0 {
                    [n[0] / length, n[1] / length, n[2] / length]
                } else {
                    [0.0, 0.0, 0.0]
                };
                engine::Triangle {
                    vertices: [vec3(t[0]), vec3(t[1]), vec3(t[2])],
                    normal: vec3(n),
                }
            })
            .collect(),
    }
}

/// Most groups one request may define.
const MAX_GROUPS: usize = 32;
/// Most separate solids `splitComponents` reports per object.
const MAX_COMPONENTS_PER_OBJECT: usize = 64;

/// `groups`: `[["object-1", "object-2"], ...]` (IDs `group-1`, `group-2`, …)
/// or `{"lid": ["object-1", "object-2"], ...}` (the keys are the IDs).
fn parse_groups(
    input: &Value,
    parts: &[engine::CompiledPart],
) -> Result<Vec<(String, Vec<String>)>, ApiError> {
    // Sizes first, so an oversized request is refused before any per-ID work.
    let count = match input.get("groups") {
        Some(Value::Array(values)) => values.len(),
        Some(Value::Object(map)) => map.len(),
        _ => 0,
    };
    if count > MAX_GROUPS {
        return Err(ApiError::bad(format!("At most {MAX_GROUPS} groups per call.")));
    }
    let known: HashSet<&str> = parts.iter().map(|part| part.id.as_str()).collect();
    let ids_of = |value: &Value, label: &str| -> Result<Vec<String>, ApiError> {
        let values = value
            .as_array()
            .ok_or_else(|| ApiError::bad(format!("{label} must be an array of object IDs.")))?;
        if values.len() > MAX_ID_LIST {
            return Err(ApiError::bad(format!("{label} may list at most {MAX_ID_LIST} IDs.")));
        }
        let ids: Vec<String> = values
            .iter()
            .map(|id| {
                id.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| ApiError::bad(format!("{label} must contain only strings.")))
            })
            .collect::<Result<_, _>>()?;
        if ids.is_empty() {
            return Err(ApiError::bad(format!("{label} must name at least one object.")));
        }
        let mut seen = HashSet::new();
        for id in &ids {
            if !seen.insert(id.as_str()) {
                return Err(ApiError::bad(format!("{label} names {id:?} twice.")));
            }
            if !known.contains(id.as_str()) {
                return Err(ApiError::bad(format!(
                    "Unknown object ID {id:?} in {label}. Objects are: {}.",
                    parts.iter().map(|part| part.id.as_str()).collect::<Vec<_>>().join(", ")
                )));
            }
        }
        Ok(ids)
    };
    let groups: Vec<(String, Vec<String>)> = match input.get("groups") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let id = format!("group-{}", index + 1);
                let members = ids_of(value, &format!("groups[{index}]"))?;
                Ok((id, members))
            })
            .collect::<Result<_, ApiError>>()?,
        Some(Value::Object(map)) => map
            .iter()
            .map(|(name, value)| {
                if name.is_empty() || name.len() > 64 {
                    return Err(ApiError::bad("Group names must be 1 to 64 characters."));
                }
                if known.contains(name.as_str()) {
                    return Err(ApiError::bad(format!(
                        "Group name {name:?} collides with an object ID."
                    )));
                }
                Ok((name.clone(), ids_of(value, &format!("groups.{name}"))?))
            })
            .collect::<Result<_, ApiError>>()?,
        Some(_) => {
            return Err(ApiError::bad(
                "groups must be [[\"object-1\", \"object-2\"], ...] or {\"name\": [ids...]}.",
            ))
        }
    };
    Ok(groups)
}

/// The union of a group's members as one target, meshed through the same
/// implicit union as a selected-object export (overlaps counted once).
fn group_target<'a>(
    compiled: &'a engine::MultipartCompileOutput,
    id: &str,
    members: &[String],
) -> Result<Target<'a>, ApiError> {
    let triangles: usize = compiled
        .parts
        .iter()
        .filter(|part| members.contains(&part.id))
        .map(|part| part.mesh.triangles.len())
        .sum();
    if members.len() > 1 && triangles > MAX_GROUP_TRIANGLES {
        return Err(ApiError::new(
            422,
            format!(
                "Group {id:?} has {triangles} triangles, more than the {MAX_GROUP_TRIANGLES} an exact union may take; use fewer members or lower $fn."
            ),
        ));
    }
    if crate::csg::bsp::deadline_passed() {
        return Err(ApiError::new(
            422,
            format!("Group {id:?} was not meshed: the request used its {} s time budget.", BATCH_BUDGET.as_secs()),
        ));
    }
    let mesh = engine::mesh_selected_parts(&compiled.parts, members, engine::Quality::Preview, None)
        .map_err(|error| ApiError::new(422, format!("Group {id:?} could not be meshed: {error}")))?;
    let first = compiled.parts.iter().find(|part| part.id == members[0]);
    Ok(Target {
        id: id.to_string(),
        name: format!("group {id} ({})", members.join(" + ")),
        kind: "group",
        mesh: std::borrow::Cow::Owned(mesh),
        source_range: first.and_then(|part| part.source_range),
        color: None,
        colors: Vec::new(),
        members: members.to_vec(),
    })
}

/// An object as its separate solids (`object-1#1`, `object-1#2`, … largest
/// first), or as itself when it is one solid.
fn component_targets(part: &engine::CompiledPart) -> Result<Vec<Target<'_>>, ApiError> {
    let solids = analysis::split_solids(&analysis::mesh_triangles(&part.mesh));
    if solids.len() <= 1 {
        return Ok(vec![Target::object(part)]);
    }
    if solids.len() > MAX_COMPONENTS_PER_OBJECT {
        return Err(ApiError::new(
            422,
            format!(
                "{} has {} separate solids, more than the {MAX_COMPONENTS_PER_OBJECT} splitComponents reports per object.",
                part.id,
                solids.len()
            ),
        ));
    }
    Ok(solids
        .iter()
        .enumerate()
        .map(|(index, solid)| Target {
            id: format!("{}#{}", part.id, index + 1),
            name: format!("{} (solid {})", part.name, index + 1),
            kind: "component",
            mesh: std::borrow::Cow::Owned(mesh_from_triangles(solid)),
            source_range: part.source_range,
            color: part.color,
            colors: Vec::new(),
            members: vec![part.id.clone()],
        })
        .collect())
}

fn split_components_input(input: &Value) -> Result<bool, ApiError> {
    match input.get("splitComponents") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(ApiError::bad("splitComponents must be a boolean.")),
    }
}

fn pair_ids(input: &Value) -> Result<Vec<(String, String)>, ApiError> {
    let mut pairs = Vec::new();
    let mut push = |value: &Value| -> Result<(), ApiError> {
        let ids = value
            .as_array()
            .filter(|ids| ids.len() == 2)
            .and_then(|ids| Some((ids[0].as_str()?.to_string(), ids[1].as_str()?.to_string())))
            .ok_or_else(|| ApiError::bad("Each pair must be [\"object-a\", \"object-b\"]."))?;
        pairs.push(ids);
        Ok(())
    };
    if let Some(value) = input.get("distance").filter(|value| !value.is_null()) {
        push(value)?;
    }
    match input.get("pairs") {
        None | Some(Value::Null) => {}
        Some(Value::Array(values)) => {
            if values.len() > MAX_ID_LIST {
                return Err(ApiError::bad(format!(
                    "At most {MAX_MEASURE_PAIRS} distance pairs per call."
                )));
            }
            for value in values {
                push(value)?;
            }
        }
        Some(_) => return Err(ApiError::bad("pairs must be an array of [idA, idB] pairs.")),
    }
    // A pair asked for twice (in either order) is computed once.
    let mut seen = std::collections::HashSet::new();
    pairs.retain(|(a, b)| {
        seen.insert(if a <= b {
            (a.clone(), b.clone())
        } else {
            (b.clone(), a.clone())
        })
    });
    if pairs.len() > MAX_MEASURE_PAIRS {
        return Err(ApiError::bad(format!(
            "At most {MAX_MEASURE_PAIRS} distance pairs per call."
        )));
    }
    Ok(pairs)
}

/// Opt-in printability checks for `measure`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PrintChecks {
    thickness: Option<f64>,
    overhang: Option<f64>,
    sharp_edges: Option<f64>,
    /// `thicknessAt`: a point and radius whose surface is measured densely.
    thickness_at: Option<([f64; 3], f64)>,
    /// The print bed plane for the overhang check: explicit `bedZ`, else
    /// filled in with the lowest Z of the whole model.
    bed_z: Option<f64>,
    /// The layer-by-layer support check and its print profile.
    supports: Option<SupportProfile>,
}

/// The `printProfile` of a support check, validated; `assumed` when every
/// value is the generic default.
#[derive(Clone, Copy, Debug, PartialEq)]
struct SupportProfile {
    layer_height: f64,
    line_width: f64,
    max_overhang: f64,
    max_bridge: f64,
    assumed: bool,
}

impl SupportProfile {
    fn profile(&self, bed_z: Option<f64>) -> support::PrintProfile {
        support::PrintProfile {
            layer_height: self.layer_height,
            line_width: self.line_width,
            max_overhang_deg: self.max_overhang,
            max_bridge: self.max_bridge,
            bed_z,
        }
    }
}

const SUPPORT_PROFILE_NOTE: &str = "A generic FDM profile (the defaults unless printProfile was passed), not a guarantee for a particular printer, material or slicer.";

/// `printProfile` {layerHeight, lineWidth, maxOverhang, maxBridge}.
fn support_profile(input: &Value) -> Result<SupportProfile, ApiError> {
    let defaults = support::PrintProfile::default();
    let given = match input.get("printProfile") {
        None | Some(Value::Null) => None,
        Some(Value::Object(given)) => Some(given),
        Some(_) => {
            return Err(ApiError::bad(
                "printProfile must be an object {layerHeight, lineWidth, maxOverhang, maxBridge}.",
            ))
        }
    };
    if let Some(key) = given.and_then(|given| {
        given.keys().find(|key| {
            !matches!(key.as_str(), "layerHeight" | "lineWidth" | "maxOverhang" | "maxBridge")
        })
    }) {
        return Err(ApiError::bad(format!(
            "Unknown printProfile field {key}; use layerHeight, lineWidth, maxOverhang, maxBridge."
        )));
    }
    let mut assumed = true;
    let mut field = |key: &str, default: f64, low: f64, high: f64| -> Result<f64, ApiError> {
        match given.and_then(|given| given.get(key)) {
            None | Some(Value::Null) => Ok(default),
            Some(value) => {
                assumed = false;
                value
                    .as_f64()
                    .filter(|number| *number >= low && *number <= high)
                    .ok_or_else(|| {
                        ApiError::bad(format!(
                            "printProfile.{key} must be a number from {low} to {high}."
                        ))
                    })
            }
        }
    };
    let layer_height = field("layerHeight", defaults.layer_height, 0.01, 2.0)?;
    let line_width = field("lineWidth", defaults.line_width, 0.05, 3.0)?;
    let max_overhang = field("maxOverhang", defaults.max_overhang_deg, 0.0, 89.0)?;
    let max_bridge = field("maxBridge", defaults.max_bridge, 0.0, 1000.0)?;
    Ok(SupportProfile { layer_height, line_width, max_overhang, max_bridge, assumed })
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn point3(point: [f64; 3]) -> Value {
    json!([round3(point[0]), round3(point[1]), round3(point[2])])
}

fn support_issue_public(issue: &support::SupportIssue) -> Value {
    json!({
        "kind": issue.kind,
        "ok": issue.ok,
        "firstLayer": issue.first_layer,
        "z": round3(issue.z),
        "zTop": round3(issue.z_top),
        "location": point3(issue.location),
        "bounds": {"min": point3(issue.min), "max": point3(issue.max)},
        "area": round3(issue.area),
        "span": issue.span.map(round3),
        "anchors": issue.anchors.iter().map(|anchor| point3(*anchor)).collect::<Vec<_>>(),
        "joinsAt": issue.joins_at.map(round3),
        "reason": issue.reason,
    })
}

/// Support analysis of a whole model. Objects rest on each other on the bed
/// (a slab on two separate posts is bridged, not floating), so every object
/// is sliced together; each issue names the objects occupying its pixels.
struct ModelSupports {
    profile: SupportProfile,
    report: support::SupportReport,
    /// For each issue: the IDs of the objects it lies on, largest first.
    owners: Vec<Vec<String>>,
}

impl ModelSupports {
    fn new(parts: &[engine::CompiledPart], profile: SupportProfile, bed_z: Option<f64>) -> Self {
        let meshes: Vec<Vec<[P; 3]>> = parts
            .iter()
            .map(|part| analysis::mesh_triangles(&part.mesh))
            .collect();
        let report = support::support_check_parts(&meshes, &profile.profile(bed_z));
        let size = |index: usize| {
            mesh_bounds(&parts[index].mesh).map_or(0.0, |bounds| {
                (0..3).map(|axis| bounds.max[axis] - bounds.min[axis]).product::<f64>()
            })
        };
        let mut indexes: Vec<Option<MeshIndex>> = Vec::new();
        let owners = report
            .issues
            .iter()
            .map(|issue| {
                let mut found: Vec<usize> = issue.parts.iter().copied().filter(|index| *index < parts.len()).collect();
                found.sort_unstable();
                found.dedup();
                if found.is_empty() && !parts.is_empty() {
                    // No owner named: the object whose surface is nearest.
                    if indexes.is_empty() {
                        indexes = parts.iter().map(|part| Some(MeshIndex::from_mesh(&part.mesh))).collect();
                    }
                    found.extend(
                        indexes
                            .iter()
                            .enumerate()
                            .filter_map(|(index, mesh)| {
                                mesh.as_ref()?.nearest(issue.location).map(|(distance, _)| (distance, index))
                            })
                            .min_by(|a, b| a.0.total_cmp(&b.0))
                            .map(|(_, index)| index),
                    );
                }
                found.sort_by(|a, b| size(*b).total_cmp(&size(*a)));
                found.iter().map(|index| parts[*index].id.clone()).collect()
            })
            .collect();
        Self { profile, report, owners }
    }

    /// Indices of the issues lying on any of `objects`.
    fn issues_on(&self, objects: &[String]) -> Vec<usize> {
        (0..self.report.issues.len())
            .filter(|issue| self.owners[*issue].iter().any(|owner| objects.contains(owner)))
            .collect()
    }

    /// Whether the scan finished and listed every issue it counted.
    fn exhaustive(&self) -> bool {
        self.report.complete && self.report.truncated == 0
    }
}

/// Which issues of a split object belong to each of its components: the
/// component whose solid contains the issue's location, else the nearest.
fn component_supports(model: &ModelSupports, targets: &[Target]) -> HashMap<String, Vec<usize>> {
    let mut assigned: HashMap<String, Vec<usize>> = HashMap::new();
    let mut parents: Vec<&String> = targets
        .iter()
        .filter(|target| target.kind == "component")
        .filter_map(|target| target.members.first())
        .collect();
    parents.sort();
    parents.dedup();
    for parent in parents {
        let siblings: Vec<(&String, MeshIndex)> = targets
            .iter()
            .filter(|target| target.kind == "component" && target.members.first() == Some(parent))
            .map(|target| (&target.id, MeshIndex::from_mesh(&target.mesh)))
            .collect();
        for issue in model.issues_on(std::slice::from_ref(parent)) {
            let location = model.report.issues[issue].location;
            let owner = siblings
                .iter()
                .find(|(_, index)| index.contains(location))
                .map(|(id, _)| *id)
                .or_else(|| {
                    siblings
                        .iter()
                        .filter_map(|(id, index)| index.nearest(location).map(|(distance, _)| (distance, *id)))
                        .min_by(|a, b| a.0.total_cmp(&b.0))
                        .map(|(_, id)| id)
                });
            if let Some(owner) = owner {
                assigned.entry(owner.clone()).or_default().push(issue);
            }
        }
    }
    assigned
}

/// Support notes that mean a budget or limit cut the analysis short.
fn support_note_is_incomplete(note: &str) -> bool {
    let note = note.to_ascii_lowercase();
    ["budget", "deadline", "time", "skipped", "stopped", "not checked", "only the lowest", "only counted", "too many"]
        .iter()
        .any(|word| note.contains(word))
}

fn support_counts_public(islands: usize, cantilevers: usize, bridges: usize, too_long: usize) -> Value {
    json!({"islands": islands, "cantilevers": cantilevers, "bridges": bridges, "bridgesTooLong": too_long})
}

/// `checks.supports` of one target: the whole-model scan's profile, totals
/// and notes, and the issues listed on this target (`issues` indices into
/// the report). `own` are the object IDs this target stands for; an issue
/// that also lies on other objects names them in `alsoTouches`.
fn supports_public(model: &ModelSupports, issues: &[usize], own: &[String]) -> Value {
    let report = &model.report;
    let profile = &report.profile;
    let totals = &report.totals;
    let listed = |kind: &str, ok: Option<bool>| {
        issues
            .iter()
            .map(|index| &report.issues[*index])
            .filter(|issue| issue.kind == kind && ok.map_or(true, |ok| issue.ok == ok))
            .count()
    };
    let public: Vec<Value> = issues
        .iter()
        .map(|index| {
            let mut value = support_issue_public(&report.issues[*index]);
            let others: Vec<&String> = model.owners[*index].iter().filter(|owner| !own.contains(owner)).collect();
            value["objects"] = json!(model.owners[*index]);
            if !others.is_empty() {
                value["alsoTouches"] = json!(others);
            }
            value
        })
        .collect();
    json!({
        "profile": {
            "layerHeight": profile.layer_height,
            "lineWidth": profile.line_width,
            "maxOverhang": profile.max_overhang_deg,
            "maxBridge": profile.max_bridge,
            "bedZ": profile.bed_z.map(round3),
            "assumed": model.profile.assumed,
            "note": SUPPORT_PROFILE_NOTE,
        },
        "scope": "whole model: all objects are sliced together, so objects resting on each other count as support; counts are the whole model's (before truncation), issues are those lying on this target",
        "orientation": "+Z up as modelled; layers are sliced bottom-up from bedZ",
        "layers": report.layers,
        "resolution": report.resolution,
        "complete": report.complete,
        "counts": support_counts_public(totals.islands, totals.cantilevers, totals.bridges, totals.bridges_too_long),
        "listedHere": support_counts_public(
            listed("island", None),
            listed("cantilever", None),
            listed("bridge", None),
            listed("bridge", Some(false)),
        ),
        "issues": public,
        "truncated": report.truncated,
        "notes": report.notes,
    })
}

impl PrintChecks {
    fn any(&self) -> bool {
        self.thickness.is_some()
            || self.overhang.is_some()
            || self.sharp_edges.is_some()
            || self.thickness_at.is_some()
            || self.supports.is_some()
    }
}

pub(crate) const DEFAULT_THICKNESS_THRESHOLD: f64 = 0.8;
/// How `thickness` / `thicknessAt` measure, as reported to clients.
const THICKNESS_METHOD: &str = "At sample points on every face (centroid; near corners and edges next to creases; a grid on faces larger than the spacing; along valley edges), then densely around each thin region (refinedSamples) to pin its minimum down: the diameter of the largest ball inside the solid touching the surface there (shrinking-ball medial-axis estimate, which sees oblique walls and short ligaments a perpendicular ray misses); where that ball is pinched by the neighbouring face across a blunt convex edge (a box edge), it slides along the edge's bisector until a wall's far side stops it, else the straight inward ray along -normal is used. Points on sharp wedges whose apex is on the surface (thread crests, knife edges: wedges of 80 degrees or sharper) are left out of min and counted in sharpTipSamples; the sharpEdges check reports those. Each location says its method (ball or ray). regions groups the thin samples into connected patches (min, at, across, area mm^2, bounds), thinnest first.";
pub(crate) const DEFAULT_OVERHANG_ANGLE: f64 = 45.0;
pub(crate) const DEFAULT_SHARP_EDGE_ANGLE: f64 = 90.0;

fn print_checks(input: &Value) -> Result<PrintChecks, ApiError> {
    let number = |key: &str, default: f64, range: std::ops::RangeInclusive<f64>| -> Result<f64, ApiError> {
        match input.get(key) {
            None | Some(Value::Null) => Ok(default),
            Some(value) => value
                .as_f64()
                .filter(|number| range.contains(number))
                .ok_or_else(|| {
                    ApiError::bad(format!(
                        "{key} must be a number from {} to {}.",
                        range.start(),
                        range.end()
                    ))
                }),
        }
    };
    let mut checks = PrintChecks::default();
    checks.bed_z = match input.get("bedZ") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_f64()
                .filter(|z| z.is_finite())
                .ok_or_else(|| ApiError::bad("bedZ must be a finite number (mm)."))?,
        ),
    };
    if let Some(probe) = input.get("thicknessAt").filter(|probe| !probe.is_null()) {
        let point = probe
            .get("point")
            .and_then(Value::as_array)
            .filter(|point| point.len() == 3)
            .and_then(|point| {
                let coordinates: Vec<f64> = point.iter().filter_map(Value::as_f64).collect();
                (coordinates.len() == 3 && coordinates.iter().all(|c| c.is_finite()))
                    .then(|| [coordinates[0], coordinates[1], coordinates[2]])
            })
            .ok_or_else(|| ApiError::bad("thicknessAt.point must be [x, y, z] in mm."))?;
        let radius = match probe.get("radius") {
            None | Some(Value::Null) => 2.0,
            Some(value) => value
                .as_f64()
                .filter(|radius| *radius > 0.0 && *radius <= 100.0)
                .ok_or_else(|| ApiError::bad("thicknessAt.radius must be a number from 0 to 100 (mm)."))?,
        };
        checks.thickness_at = Some((point, radius));
    }
    let names = match input.get("checks") {
        None | Some(Value::Null) => return Ok(checks),
        Some(Value::Array(names)) => names,
        Some(_) => {
            return Err(ApiError::bad(
                "checks must be an array of \"thickness\", \"overhang\", \"sharpEdges\", \"supports\".",
            ))
        }
    };
    for name in names {
        match name.as_str() {
            Some("thickness") => {
                checks.thickness = Some(number(
                    "thicknessThreshold",
                    DEFAULT_THICKNESS_THRESHOLD,
                    0.0..=1000.0,
                )?)
            }
            Some("overhang") => {
                checks.overhang = Some(number("overhangAngle", DEFAULT_OVERHANG_ANGLE, 0.0..=90.0)?)
            }
            Some("sharpEdges") => {
                checks.sharp_edges = Some(number(
                    "sharpEdgeAngle",
                    DEFAULT_SHARP_EDGE_ANGLE,
                    0.0..=180.0,
                )?)
            }
            Some("supports") => checks.supports = Some(support_profile(input)?),
            _ => {
                return Err(ApiError::bad(format!(
                    "Unknown check {name}; use \"thickness\", \"overhang\", \"sharpEdges\" or \"supports\"."
                )))
            }
        }
    }
    Ok(checks)
}

fn print_checks_public(mesh: &engine::Mesh, checks: PrintChecks) -> Value {
    let triangles = analysis::mesh_triangles(mesh);
    let mut value = json!({});
    let spot = |spot: &analysis::ThinSpot| {
        json!({
            "thickness": spot.thickness,
            "from": point_public(spot.from),
            "to": point_public(spot.to),
            "method": spot.method,
        })
    };
    let thickness_public = |check: &analysis::ThicknessCheck| {
        json!({
            "method": THICKNESS_METHOD,
            "threshold": check.threshold,
            "min": check.min.as_ref().map(|found| found.thickness),
            "minLocation": check.min.as_ref().map(spot),
            "samples": check.samples,
            "sampleSpacing": check.spacing,
            "triangles": check.triangles,
            "complete": check.complete(),
            "skippedTriangles": check.skipped,
            "belowThreshold": check.below_threshold,
            "belowThresholdArea": check.below_threshold_area,
            "sharpTipSamples": check.sharp_tips,
            "thinnest": check.thinnest.iter().map(spot).collect::<Vec<_>>(),
            // Thin areas as connected regions, thinnest first: where (the
            // thinnest point and the region's box), how thin, how much area.
            "regionCount": check.region_count,
            "regions": check.regions.iter().map(|region| json!({
                "min": round_to(region.min.thickness, 4),
                "at": rounded3(region.min.from),
                "across": rounded3(region.min.to),
                "method": region.min.method,
                "area": round_to(region.area, 3),
                "samples": region.samples,
                "bounds": {"min": rounded3(region.bounds.min), "max": rounded3(region.bounds.max)},
            })).collect::<Vec<_>>(),
            "refinedSamples": check.refined_samples,
            "caveat": format!(
                "Sampled estimate: surface points about {:.3} mm apart. A thin spot smaller than that, or one formed by a convex edge (a rim or lip) where the inscribed ball is pinched by the neighbouring face, can be missed; probe a suspect region densely with thicknessAt {{point, radius}}.",
                check.spacing
            ),
        })
    };
    let needs_index = checks.thickness.is_some() || checks.thickness_at.is_some();
    let index = needs_index.then(|| MeshIndex::new(triangles.clone()));
    if let (Some(threshold), Some(index)) = (checks.thickness, &index) {
        let check = analysis::thickness_check(index, threshold, analysis::MAX_THICKNESS_SAMPLES);
        value["thickness"] = thickness_public(&check);
    }
    if let (Some((point, radius)), Some(index)) = (checks.thickness_at, &index) {
        let threshold = checks.thickness.unwrap_or(DEFAULT_THICKNESS_THRESHOLD);
        let check = analysis::thickness_probe(index, point, radius, threshold);
        value["thicknessAt"] = if check.triangles == 0 {
            json!({"point": point_public(point), "radius": radius, "note": "No surface of this object within radius of point."})
        } else {
            let mut probe = thickness_public(&check);
            probe["point"] = point_public(point);
            probe["radius"] = json!(radius);
            probe
        };
    }
    if let Some(angle) = checks.overhang {
        let index = MeshIndex::new(triangles.clone());
        let check = analysis::overhang_check(&index, angle, checks.bed_z);
        value["overhang"] = json!({
            "orientation": "+Z up; downward faces on the bed plane (bedZ: the request's, else the lowest Z of the whole model) rest on the bed and are excluded, so an object floating above the bed counts its underside as overhang",
            "bedZ": checks.bed_z,
            "angle": check.angle,
            "area": check.area,
            "triangles": check.triangles,
            "bounds": check.bounds.as_ref().map(aabb_public),
            "regions": check.regions,
            "largestRegions": check.largest.iter().map(|region| json!({
                "area": region.area,
                "triangles": region.triangles,
                "bounds": aabb_public(&region.bounds),
                "minZ": region.bounds.min[2],
            })).collect::<Vec<_>>(),
            "bedArea": check.bed_area,
        });
    }
    if let Some(angle) = checks.sharp_edges {
        let check = analysis::sharp_edge_check(&triangles, angle);
        value["sharpEdges"] = json!({
            "threshold": check.threshold,
            "note": "convex mesh edges whose interior dihedral angle is at or below threshold (box edge 90, 45-degree chamfer 135)",
            "edges": check.edges,
            "length": check.length,
            "creases": check.creases,
            "longestCreases": check.longest.iter().map(|crease| json!({
                "length": crease.length,
                "edges": crease.edges,
                "minAngle": crease.min_angle,
                "bounds": aabb_public(&crease.bounds),
                "location": point_public(crease.location),
            })).collect::<Vec<_>>(),
        });
    }
    value
}

/// The validated, compile-independent arguments of a measurement.
struct MeasureRequest {
    pairs: Vec<(String, String)>,
    checks: PrintChecks,
    split: bool,
}

fn measure_request(input: &Value) -> Result<MeasureRequest, ApiError> {
    Ok(MeasureRequest {
        pairs: pair_ids(input)?,
        checks: print_checks(input)?,
        split: split_components_input(input)?,
    })
}

/// The lowest Z of every object of a model: the print bed.
fn model_floor(compiled: &engine::MultipartCompileOutput) -> Option<f64> {
    compiled
        .parts
        .iter()
        .filter_map(|part| mesh_bounds(&part.mesh))
        .map(|bounds| bounds.min[2])
        .reduce(f64::min)
}

/// `objects`, `groups`, `splitComponents`, `total`, `distances` and `units`
/// of one compiled source: the body of `measure`, shared with `evaluate`.
fn measure_compiled(
    source: &EvalSource,
    compiled: &engine::MultipartCompileOutput,
    input: &Value,
    request: &MeasureRequest,
) -> Result<Value, ApiError> {
    let MeasureRequest { pairs, split, .. } = request;
    let split = *split;
    let mut checks = request.checks;
    if checks.bed_z.is_none() {
        checks.bed_z = model_floor(compiled);
    }
    let selected = select_parts(&compiled.parts, input, "objectIds")?;
    let groups = parse_groups(input, &compiled.parts)?;
    let mut targets: Vec<Target> = Vec::new();
    for part in &selected {
        if split {
            targets.extend(component_targets(part)?);
        } else {
            targets.push(Target::object(part));
        }
    }
    let group_targets: Vec<Target> = groups
        .iter()
        .map(|(id, members)| group_target(compiled, id, members))
        .collect::<Result<_, _>>()?;
    // Supports are judged on the whole model (objects support each other):
    // an object lists the issues lying on it, a group those of its members,
    // a component those of its object that lie in or nearest it.
    let model_supports = checks
        .supports
        .map(|profile| ModelSupports::new(&compiled.parts, profile, checks.bed_z));
    let component_issues = model_supports
        .as_ref()
        .map(|model| component_supports(model, &targets))
        .unwrap_or_default();
    let mut total_bounds = analysis::Aabb::empty();
    let mut total_volume = 0.0;
    let mut describe = |target: &Target, count: bool| {
        let measure = analysis::measure_mesh(&target.mesh);
        if count {
            total_bounds.merge(&measure.bounds);
            total_volume += measure.volume;
        }
        let mut value = measure_public(&measure);
        if let (Value::Object(value), Value::Object(identity)) =
            (&mut value, target.identity(source))
        {
            value.extend(identity);
        }
        if checks.any() {
            let mut own = checks;
            own.supports = None;
            value["checks"] = print_checks_public(&target.mesh, own);
            if let Some(model) = &model_supports {
                let (objects, issues) = match target.kind {
                    "group" => (target.members.clone(), model.issues_on(&target.members)),
                    "component" => (
                        target.members.clone(),
                        component_issues.get(&target.id).cloned().unwrap_or_default(),
                    ),
                    _ => {
                        let own = vec![target.id.clone()];
                        let issues = model.issues_on(&own);
                        (own, issues)
                    }
                };
                value["checks"]["supports"] = supports_public(model, &issues, &objects);
            }
        }
        value
    };
    let objects: Vec<Value> = targets.iter().map(|target| describe(target, true)).collect();
    let groups_public: Vec<Value> = group_targets
        .iter()
        .map(|target| describe(target, false))
        .collect();
    // Pairs may name objects (also when split), components and groups.
    let mut addressable: Vec<&Target> = targets.iter().chain(group_targets.iter()).collect();
    let extra: Vec<Target> = if split {
        // Whole objects stay addressable by their own ID when split.
        selected
            .iter()
            .filter(|part| !targets.iter().any(|target| target.id == part.id))
            .map(|part| Target::object(part))
            .collect()
    } else {
        Vec::new()
    };
    addressable.extend(extra.iter());
    // Unselected objects remain addressable by pairs, as before.
    let listed: HashSet<String> = addressable.iter().map(|target| target.id.clone()).collect();
    let unselected: Vec<Target> = compiled
        .parts
        .iter()
        .filter(|part| !listed.contains(&part.id))
        .map(Target::object)
        .collect();
    addressable.extend(unselected.iter());
    let mut indexes: HashMap<String, MeshIndex> = HashMap::new();
    let mut distances = Vec::new();
    for (a, b) in pairs {
        let find = |id: &str| {
            addressable
                .iter()
                .find(|target| target.id == id)
                .copied()
                .ok_or_else(|| {
                    ApiError::bad(format!(
                        "Unknown ID {id:?} in pairs. Addressable: {}.",
                        addressable
                            .iter()
                            .map(|target| target.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })
        };
        let (target_a, target_b) = (find(a)?, find(b)?);
        for target in [target_a, target_b] {
            indexes
                .entry(target.id.clone())
                .or_insert_with(|| MeshIndex::from_mesh(&target.mesh));
        }
        let epsilon = epsilon_for(&[target_a, target_b]);
        let report = analysis::pair_report(&indexes[a.as_str()], &indexes[b.as_str()], epsilon);
        distances.push(match report {
            Some(report) => pair_public(
                target_a,
                target_b,
                (&indexes[a.as_str()], &indexes[b.as_str()]),
                &report,
                PairPolicy {
                    tolerance: 0.0,
                    allow_contact: true,
                },
                source,
            ),
            None => unchecked_pair(target_a, target_b, "undetermined", "an object has no triangles"),
        });
    }
    let mut value = json!({});
    value["objects"] = json!(objects);
    if !group_targets.is_empty() {
        value["groups"] = json!(groups_public);
    }
    value["splitComponents"] = json!(split);
    value["total"] = json!({
        "objects": targets.len(),
        "volume": total_volume,
        "bounds": (!total_bounds.is_empty()).then(|| aabb_public(&total_bounds)),
        "note": "Sum of the listed objects' volumes (overlaps counted twice) and their combined bounds; measure a group for the volume of their union.",
    });
    value["distances"] = json!(distances);
    value["units"] = json!("mm, mm^2, mm^3");
    Ok(value)
}

/// Copies every field of `from` (an object) into `into`.
fn merge_fields(into: &mut Value, from: Value) {
    if let (Value::Object(into), Value::Object(from)) = (into, from) {
        into.extend(from);
    }
}

pub(crate) fn measure_value(
    state: &AppState,
    id: Option<&str>,
    input: &Value,
) -> Result<Value, ApiError> {
    let started = Instant::now();
    let (_, _deadline) = request_deadline(started);
    let source = eval_source(state, id, input)?;
    let request = measure_request(input)?;
    let compiled = compile_eval(&source)?;
    let mut value = source.header();
    value["ok"] = json!(true);
    merge_fields(&mut value, measure_compiled(&source, &compiled, input, &request)?);
    attach_diagnostics(&mut value, &compiled.messages, started);
    flag_invalid_meshes(&mut value);
    Ok(value)
}

// ---------------------------------------------------------------------------
// intersections
// ---------------------------------------------------------------------------

/// Length below which a gap or a penetration is numerical noise: relative to
/// the parts' size, never below a nanometre-scale floor.
fn epsilon_for(targets: &[&Target]) -> f64 {
    let span = targets
        .iter()
        .filter_map(|target| mesh_bounds(&target.mesh))
        .map(|bounds| {
            (0..3)
                .map(|axis| bounds.max[axis] - bounds.min[axis])
                .fold(0.0f64, f64::max)
        })
        .fold(0.0f64, f64::max);
    (span * 1e-9).max(1e-6)
}

fn point_public(point: P) -> Value {
    json!(point)
}

/// What a pair is judged against.
#[derive(Clone, Copy, Debug)]
struct PairPolicy {
    /// Required minimum gap, mm.
    tolerance: f64,
    /// Whether face contact (zero gap, zero overlap volume) is intended.
    allow_contact: bool,
}

/// Which pairs may touch: `allowContact` is `true` (every pair), `false`
/// (default), or a list of `[idA, idB]` pairs.
#[derive(Clone, Debug, Default)]
enum ContactPolicy {
    #[default]
    Never,
    Always,
    Pairs(HashSet<(String, String)>),
}

impl ContactPolicy {
    fn from_input(input: &Value) -> Result<Self, ApiError> {
        match input.get("allowContact") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => Ok(Self::Never),
            Some(Value::Bool(true)) => Ok(Self::Always),
            Some(Value::Array(values)) => {
                if values.len() > MAX_ID_LIST {
                    return Err(ApiError::bad(format!(
                        "allowContact may list at most {MAX_ID_LIST} pairs."
                    )));
                }
                let mut pairs = HashSet::new();
                for value in values {
                    let (a, b) = value
                        .as_array()
                        .filter(|ids| ids.len() == 2)
                        .and_then(|ids| Some((ids[0].as_str()?, ids[1].as_str()?)))
                        .ok_or_else(|| {
                            ApiError::bad("allowContact must be true/false or a list of [idA, idB] pairs.")
                        })?;
                    pairs.insert(Self::key(a, b));
                }
                Ok(Self::Pairs(pairs))
            }
            Some(_) => Err(ApiError::bad(
                "allowContact must be true/false or a list of [idA, idB] pairs.",
            )),
        }
    }

    fn key(a: &str, b: &str) -> (String, String) {
        if a <= b {
            (a.to_string(), b.to_string())
        } else {
            (b.to_string(), a.to_string())
        }
    }

    /// A pair naming a component (`object-1#2`) also matches its parent's ID.
    fn allows(&self, a: &Target, b: &Target) -> bool {
        match self {
            Self::Never => false,
            Self::Always => true,
            Self::Pairs(pairs) => {
                let names = |target: &Target| -> Vec<String> {
                    let mut names = vec![target.id.clone()];
                    if target.kind == "component" {
                        names.extend(target.members.iter().cloned());
                    }
                    names
                };
                names(a).iter().any(|left| {
                    names(b)
                        .iter()
                        .any(|right| pairs.contains(&Self::key(left, right)))
                })
            }
        }
    }

    fn validate(&self, known: &[&str]) -> Result<(), ApiError> {
        if let Self::Pairs(pairs) = self {
            for (a, b) in pairs {
                for id in [a, b] {
                    if !known.contains(&id.as_str()) {
                        return Err(ApiError::bad(format!(
                            "Unknown ID {id:?} in allowContact. Known: {}.",
                            known.join(", ")
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

/// One pair's relationship.
///
/// `relation` is the verdict, one of:
/// - `overlap`: the solids share positive volume (`overlapVolume` > 0);
/// - `contact`: surfaces touch (gap within numerical epsilon) with no shared
///   volume;
/// - `clearanceViolation`: a positive gap smaller than `tolerance`;
/// - `clear`: the gap is at least `tolerance`;
/// - `undetermined`: could not be decided within the work budget.
///
/// `toleranceViolated` is true for overlap, clearanceViolation, undetermined,
/// and contact when `tolerance` > 0 unless contact is allowed for the pair.
fn pair_public(
    a: &Target,
    b: &Target,
    (index_a, index_b): (&MeshIndex, &MeshIndex),
    report: &analysis::PairReport,
    policy: PairPolicy,
    source: &EvalSource,
) -> Value {
    let tolerance = policy.tolerance;
    let epsilon = epsilon_for(&[a, b]);
    let intersects = report.intersects;
    // Surfaces meet but neither the exact boolean nor the fallback crossing
    // test could decide within budget: say so instead of guessing "clear".
    let undetermined = !report.determined;
    let distance = report.distance.distance;
    let clearance = if intersects { 0.0 } else { distance };
    // An incomplete distance search gives `distance` as an upper bound; only
    // its proven lower bound can establish that the gap clears the tolerance.
    let clearance_lower = if intersects {
        0.0
    } else {
        report.distance.lower_bound.min(distance)
    };
    let touching = !intersects && !undetermined && distance <= epsilon;
    // A gap meets the tolerance when it is at least the tolerance, give or
    // take numerical noise: 0.2 mm measured against 0.2 mm passes.
    let required = tolerance - epsilon;
    let gap_short = !intersects && !undetermined && !touching && clearance < required;
    let clearance_undetermined =
        !intersects && !undetermined && !touching && !gap_short && clearance_lower < required;
    let contact_allowed = touching && (policy.allow_contact || tolerance <= epsilon);
    let relation = if undetermined || clearance_undetermined {
        "undetermined"
    } else if intersects {
        "overlap"
    } else if touching {
        "contact"
    } else if gap_short {
        "clearanceViolation"
    } else {
        "clear"
    };
    let (overlap, overlap_note) = match &report.overlap {
        Some(Ok(result)) => (Some(*result), None),
        Some(Err(skipped)) => (None, Some(skipped.to_string())),
        None => (None, None),
    };
    let region = report.overlap_region(index_a, index_b);
    let overlap_bounds = region.map(|(bounds, _)| bounds);
    let overlap_exact = region.is_some_and(|(_, exact)| exact);
    let depth = report.depth_detail(overlap_bounds);
    let location = report.location(overlap_bounds);
    json!({
        "a": a.id,
        "b": b.id,
        "aName": a.name,
        "bName": b.name,
        "relation": relation,
        "intersects": if undetermined { Value::Null } else { json!(intersects) },
        // False when either the overlap or the clearance-versus-tolerance
        // verdict could not be decided within budget: never read it as clean.
        "determined": relation != "undetermined",
        "touching": touching,
        "contactAllowed": touching.then_some(contact_allowed),
        "contained": report.contained,
        "clearance": clearance,
        // Equal to clearance when distanceExact; otherwise the true clearance
        // lies in [clearanceLowerBound, clearance].
        "clearanceLowerBound": clearance_lower,
        // The gap is below the tolerance without the solids overlapping
        // (contact included when tolerance > 0, whether or not allowed).
        "withinTolerance": gap_short || (touching && tolerance > epsilon),
        "toleranceViolated": relation == "overlap"
            || relation == "clearanceViolation"
            || relation == "undetermined"
            || (touching && !contact_allowed),
        "depth": depth.map(|depth| depth.depth),
        "depthMethod": depth.map(|depth| depth.method),
        "overlapThickness": depth.and_then(|depth| depth.thickness),
        "vertexPenetration": depth.and_then(|depth| depth.vertex),
        "overlapBoundsMinExtent": depth.and_then(|depth| depth.bounds_min_extent),
        "location": point_public(location),
        "closestPoints": (!intersects).then(|| json!({
            "a": point_public(report.distance.point_a),
            "b": point_public(report.distance.point_b),
        })),
        "distanceExact": report.distance.exact,
        // 0 only when it is known to be 0; null when it was not computed.
        "overlapVolume": match (intersects, undetermined, overlap) {
            (_, _, Some(result)) if intersects => json!(result.volume),
            (false, false, _) => json!(0.0),
            _ => Value::Null,
        },
        "overlapBounds": overlap_bounds.map(|bounds| aabb_public(&bounds)),
        "overlapBoundsExact": overlap_exact,
        "overlapNote": overlap_note,
        "aKind": a.kind,
        "bKind": b.kind,
        "aSourceRange": source.original_range(a.source_range),
        "bSourceRange": source.original_range(b.source_range),
        "aSource": source.source_location(a.source_range),
        "bSource": source.source_location(b.source_range),
    })
}

/// A pair that was not decided (`relation` `"skipped"` or `"undetermined"`),
/// carrying every verdict flag a reader might test, each saying "not clear":
/// `determined: false`, `toleranceViolated: true`, and null for every
/// measurement that was not made.
fn unchecked_pair(a: &Target, b: &Target, relation: &str, error: &str) -> Value {
    json!({
        "a": a.id,
        "b": b.id,
        "aName": a.name,
        "bName": b.name,
        "aKind": a.kind,
        "bKind": b.kind,
        "relation": relation,
        "skipped": relation == "skipped",
        "determined": false,
        "intersects": Value::Null,
        "touching": Value::Null,
        "contactAllowed": Value::Null,
        "clearance": Value::Null,
        "clearanceLowerBound": Value::Null,
        "withinTolerance": Value::Null,
        "toleranceViolated": true,
        "overlapVolume": Value::Null,
        "depth": Value::Null,
        "location": Value::Null,
        "error": error,
    })
}

/// Every pair of the checked targets of one compiled source.
///
/// Pairs not started by `deadline` are reported as skipped instead of run,
/// so a model with many dense, interleaved objects cannot hold the permit
/// indefinitely.
fn check_pairs(
    targets: &[Target],
    tolerance: f64,
    contact: &ContactPolicy,
    source: &EvalSource,
    deadline: Instant,
) -> Result<Vec<Value>, ApiError> {
    if targets.len() > MAX_CHECKED_OBJECTS {
        return Err(ApiError::new(
            422,
            format!(
                "{} objects exceed the {MAX_CHECKED_OBJECTS}-object limit for a pairwise check; pass visibleObjectIds to check a subset.",
                targets.len()
            ),
        ));
    }
    // Every pair and index build polls for a gone client as well as the
    // deadline: this loop is where a cancelled check would otherwise keep
    // burning its permit for seconds.
    let _deadline = crate::csg::bsp::deadline_scope(deadline);
    let mut indexes: Vec<MeshIndex> = Vec::with_capacity(targets.len());
    for target in targets {
        if crate::csg::bsp::deadline_passed() {
            break;
        }
        indexes.push(MeshIndex::from_mesh(&target.mesh));
    }
    let mut pairs = Vec::new();
    for left in 0..targets.len() {
        for right in left + 1..targets.len() {
            pairs.push((left, right));
        }
    }
    // Sequential on the request thread on purpose: this request holds one of
    // `MAX_CONCURRENT_JOBS` permits, each sized to one core, and the exact
    // booleans honour the thread's deadline scope only on this thread.
    let mut results = Vec::with_capacity(pairs.len());
    for &(left, right) in &pairs {
        let (a, b) = (&targets[left], &targets[right]);
        if indexes.len() < targets.len() || crate::csg::bsp::deadline_passed() {
            results.push(unchecked_pair(
                a,
                b,
                "skipped",
                "Not checked: the request was cancelled or used its time budget. Check this pair on its own with visibleObjectIds.",
            ));
            continue;
        }
        results.push(
            match analysis::pair_report(&indexes[left], &indexes[right], epsilon_for(&[a, b])) {
                Some(report) => {
                    let mut pair = pair_public(
                        a,
                        b,
                        (&indexes[left], &indexes[right]),
                        &report,
                        PairPolicy {
                            tolerance,
                            allow_contact: contact.allows(a, b),
                        },
                        source,
                    );
                    attach_clearance_regions(&mut pair, (&indexes[left], &indexes[right]), tolerance, deadline);
                    pair
                }
                // Never dropped: a pair missing from the list would read as
                // nothing to report.
                None => unchecked_pair(
                    a,
                    b,
                    "undetermined",
                    "Not decided: an object has no triangles to compare.",
                ),
            },
        );
    }
    Ok(results)
}

/// Adds `clearanceRegions` (and `clearanceSampling`) to a pair that
/// overlaps or comes closer than the tolerance: up to
/// [`analysis::MAX_CLEARANCE_REGIONS`] connected patches where the signed
/// clearance (negative inside the other solid) is below the tolerance, the
/// largest first, so a fit against a scan shows where it is tight. Found by
/// sampling both surfaces (at most [`analysis::CLEARANCE_SAMPLES`] points
/// each) with nearest-point queries, so it is not bounded by the exact
/// booleans' triangle cap; the request deadline truncates it.
fn attach_clearance_regions(pair: &mut Value, (a, b): (&MeshIndex, &MeshIndex), tolerance: f64, deadline: Instant) {
    let relevant = matches!(
        pair["relation"].as_str(),
        Some("overlap" | "contact" | "clearanceViolation")
    ) || pair["withinTolerance"] == json!(true);
    if !relevant || crate::csg::bsp::deadline_passed() {
        return;
    }
    let found = analysis::clearance_regions(a, b, tolerance, analysis::CLEARANCE_SAMPLES, Some(deadline));
    let point = |p: P| p.map(|value| round_to(value, 3));
    pair["clearanceRegions"] = json!(found
        .regions
        .iter()
        .map(|region| json!({
            "center": point(region.center),
            "minClearance": round_to(region.min_clearance, 4),
            "at": point(region.min_point),
            "area": round_to(region.area, 3),
            "sampleCount": region.samples,
            "bounds": {"min": point(region.bounds.min), "max": point(region.bounds.max)},
            "overlapping": region.overlapping,
        }))
        .collect::<Vec<_>>());
    pair["clearanceSampling"] = json!({
        "spacing": round_to(found.spacing, 4),
        "samples": found.samples,
        "regionsFound": found.total_regions,
        "truncated": found.truncated,
    });
}

/// The targets an intersection check compares: the visible objects, with
/// `groups` replacing their members and `splitComponents` splitting the rest
/// into their separate solids.
fn intersection_targets<'a>(
    compiled: &'a engine::MultipartCompileOutput,
    input: &Value,
    contact: &ContactPolicy,
) -> Result<Vec<Target<'a>>, ApiError> {
    let selected = select_parts(&compiled.parts, input, "visibleObjectIds")?;
    let split = split_components_input(input)?;
    let groups = parse_groups(input, &compiled.parts)?;
    let mut grouped: HashMap<&str, &str> = HashMap::new();
    for (id, members) in &groups {
        for member in members {
            if let Some(other) = grouped.insert(member.as_str(), id.as_str()) {
                return Err(ApiError::bad(format!(
                    "{member} is in both group {other:?} and group {id:?}; an object can be in one group per check."
                )));
            }
        }
    }
    let mut targets = Vec::new();
    for part in selected {
        if grouped.contains_key(part.id.as_str()) {
            continue;
        }
        if split {
            targets.extend(component_targets(part)?);
        } else {
            targets.push(Target::object(part));
        }
    }
    for (id, members) in &groups {
        targets.push(group_target(compiled, id, members)?);
    }
    let mut known: Vec<&str> = compiled.parts.iter().map(|part| part.id.as_str()).collect();
    known.extend(targets.iter().map(|target| target.id.as_str()));
    contact.validate(&known)?;
    Ok(targets)
}

/// Whether a pair needs attention: a violation, or undecided or skipped.
fn is_problem_pair(pair: &Value) -> bool {
    pair["toleranceViolated"] == json!(true)
        || pair["skipped"] == json!(true)
        || pair["determined"] == json!(false)
}

fn pair_summary(pairs: &[Value]) -> Value {
    let count = |relation: &str| {
        pairs
            .iter()
            .filter(|pair| pair["relation"] == json!(relation))
            .count()
    };
    let violations = pairs
        .iter()
        .filter(|pair| pair["toleranceViolated"] == json!(true))
        .count();
    let min_clearance = pairs
        .iter()
        .filter(|pair| pair["intersects"] == json!(false))
        .filter_map(|pair| pair["clearance"].as_f64())
        .fold(None, |best: Option<f64>, value| {
            Some(best.map_or(value, |best| best.min(value)))
        });
    let overlap: f64 = pairs
        .iter()
        .filter_map(|pair| pair["overlapVolume"].as_f64())
        .sum();
    let skipped = pairs
        .iter()
        .filter(|pair| pair["skipped"] == json!(true))
        .count();
    let undetermined = pairs
        .iter()
        .filter(|pair| pair["determined"] == json!(false) && pair["skipped"] != json!(true))
        .count();
    let contacts = count("contact");
    let allowed_contacts = pairs
        .iter()
        .filter(|pair| pair["relation"] == json!("contact") && pair["contactAllowed"] == json!(true))
        .count();
    json!({
        "pairs": pairs.len(),
        // Every pair was evaluated and decided: nothing skipped or undetermined.
        "complete": skipped == 0 && undetermined == 0,
        "skipped": skipped,
        "undetermined": undetermined,
        "overlaps": count("overlap"),
        "contacts": contacts,
        "allowedContacts": allowed_contacts,
        "clearanceViolations": count("clearanceViolation"),
        "clear": count("clear"),
        "intersecting": count("overlap"),
        "toleranceViolations": violations,
        "minClearance": min_clearance,
        "totalOverlapVolume": overlap,
    })
}

fn tolerance_input(input: &Value, source: &EvalSource) -> Result<f64, ApiError> {
    match input.get("tolerance") {
        None | Some(Value::Null) => Ok(source.tolerance()),
        Some(value) => value
            .as_f64()
            .filter(|tolerance| tolerance.is_finite() && *tolerance >= 0.0)
            .ok_or_else(|| {
                ApiError::bad("tolerance must be a non-negative number of millimetres.")
            }),
    }
}

/// The verdict fields every intersection response repeats.
fn intersection_header(value: &mut Value, tolerance: f64, contact: &ContactPolicy) {
    value["supported"] = json!(true);
    value["tolerance"] = json!(tolerance);
    value["allowContact"] = match contact {
        ContactPolicy::Never => json!(false),
        ContactPolicy::Always => json!(true),
        ContactPolicy::Pairs(pairs) => json!(pairs.iter().map(|(a, b)| json!([a, b])).collect::<Vec<_>>()),
    };
    value["method"] = json!("exact: BVH triangle-triangle distance on the exported meshes; overlap volume and bounds from the CSG kernel");
    value["semantics"] = json!(INTERSECTION_SEMANTICS);
}

/// `complete`, `summary` and `intersections` (every pair) of one compiled
/// source: the body of a single intersection check, shared with `evaluate`.
fn intersections_compiled(
    source: &EvalSource,
    compiled: &engine::MultipartCompileOutput,
    input: &Value,
    tolerance: f64,
    contact: &ContactPolicy,
    deadline: Instant,
) -> Result<Value, ApiError> {
    let targets = intersection_targets(compiled, input, contact)?;
    let pairs = check_pairs(&targets, tolerance, contact, source, deadline)?;
    let summary = pair_summary(&pairs);
    Ok(json!({
        "complete": summary["complete"],
        "summary": summary,
        "intersections": pairs,
    }))
}

pub(crate) fn intersections_value(
    state: &AppState,
    id: Option<&str>,
    input: &Value,
) -> Result<Value, ApiError> {
    let started = Instant::now();
    let (deadline, _deadline_scope) = request_deadline(started);
    let source = eval_source(state, id, input)?;
    let tolerance = tolerance_input(input, &source)?;
    let contact = ContactPolicy::from_input(input)?;
    let mut value = source.header();
    value["ok"] = json!(true);
    intersection_header(&mut value, tolerance, &contact);
    let compiled = compile_eval(&source)?;
    merge_fields(
        &mut value,
        intersections_compiled(&source, &compiled, input, tolerance, &contact, deadline)?,
    );
    attach_diagnostics(&mut value, &compiled.messages, started);
    Ok(value)
}

/// How `check_intersections` classifies a pair; repeated in every response
/// and in the tool description.
pub(crate) const INTERSECTION_SEMANTICS: &str = "relation per pair: overlap = the solids share positive volume (overlapVolume > 0); contact = surfaces touch (gap within numerical epsilon) with zero shared volume; clearanceViolation = a positive gap smaller than tolerance; clear = gap >= tolerance; undetermined = not decided within the work budget (treat as a problem). tolerance is the required minimum gap: at tolerance 0 contact passes; at tolerance > 0 contact violates unless allowContact (true, or a list of [idA, idB] pairs) marks it intended. toleranceViolated = overlap, clearanceViolation, undetermined, or disallowed contact. depth (overlaps only) estimates how far the solids interpenetrate: overlapThickness, the thickness of the exact overlap solid at its thickest point (diameter of the largest ball found inside it; e.g. 0.01 mm for a 0.01 mm over-insertion at any angle, or the radial interference of a press fit); when the exact boolean was skipped, the smaller of vertexPenetration (deepest vertex of either solid inside the other) and overlapBoundsMinExtent (thinnest axis-aligned extent of the overlap bounds). depthMethod names which. It is not an exact minimum translation distance.";

// ---------------------------------------------------------------------------
// diff
// ---------------------------------------------------------------------------

/// Order-independent identity of a mesh.
fn mesh_fingerprint(mesh: &engine::Mesh) -> Vec<[u64; 9]> {
    let mut triangles: Vec<[u64; 9]> = mesh
        .triangles
        .iter()
        .map(|triangle| {
            let mut corners = triangle
                .vertices
                .map(|vertex| [vertex.x.to_bits(), vertex.y.to_bits(), vertex.z.to_bits()]);
            // Rotate (not sort) so winding is part of the identity.
            let first = (0..3).min_by_key(|index| corners[*index]).unwrap_or(0);
            corners.rotate_left(first);
            [
                corners[0][0],
                corners[0][1],
                corners[0][2],
                corners[1][0],
                corners[1][1],
                corners[1][2],
                corners[2][0],
                corners[2][1],
                corners[2][2],
            ]
        })
        .collect();
    triangles.sort_unstable();
    triangles
}

fn line_span(source: &str, start: usize, end: usize) -> Value {
    json!([
        scadsource::line_of(source, start),
        scadsource::line_of(source, end.max(start + 1).min(source.len().max(1)) - 1)
    ])
}

pub(crate) fn diff_value(
    state: &AppState,
    id: Option<&str>,
    input: &Value,
) -> Result<Value, ApiError> {
    let diff_started = Instant::now();
    let (deadline, _deadline_scope) = request_deadline(diff_started);
    let workspace_id = id
        .or_else(|| crate::input_design_id(input))
        .ok_or_else(|| ApiError::bad("designId is required."))?;
    let workspace = load_workspace(state, workspace_id)?;
    let revision_of = |key: &str| -> Result<Option<u64>, ApiError> {
        match input.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_u64()
                .map(Some)
                .ok_or_else(|| ApiError::bad(format!("{key} must be a revision number."))),
        }
    };
    let to_revision = revision_of("toRevision")?.unwrap_or(workspace.revision);
    // `from` defaults to the newest retained revision before `to`, which need
    // not be `to - 1`: a browser autosave spends a number and keeps no copy.
    let from_revision = match revision_of("fromRevision")? {
        Some(revision) => revision,
        None => state
            .workspaces
            .revision_list(&workspace.id)
            .map_err(|error| ApiError::new(500, error))?
            .iter()
            .map(|info| info.revision)
            .filter(|revision| *revision < to_revision)
            .max()
            .ok_or_else(|| ApiError::bad(format!("fromRevision is required: revision {to_revision} has no retained predecessor.")))?,
    };
    let to_source = revision_source(state, &workspace, to_revision)?;
    let from_source = revision_source(state, &workspace, from_revision)?;
    let recheck = diff_recheck(input)?;
    // Two compiles for one request: bill the second.
    if let Err(limited) = state.rate_limiter.charge_extra_compile() {
        return Err(ApiError::new(
            429,
            format!(
                "Rate limit exceeded for this client (compute bucket): a diff compiles twice. Wait {} s and retry unchanged; nothing is wrong with the request.",
                limited.retry_after_seconds()
            ),
        ));
    }
    let from_compiled =
        compile_parts_guarded(&from_source, engine::Quality::Preview).map_err(|error| {
            ApiError::new(
                422,
                format!("The \"from\" source failed to compile: {error}"),
            )
        })?;
    let to_compiled =
        compile_parts_guarded(&to_source, engine::Quality::Preview).map_err(|error| {
            ApiError::new(422, format!("The \"to\" source failed to compile: {error}"))
        })?;

    let mut ids: Vec<&str> = from_compiled
        .parts
        .iter()
        .map(|part| part.id.as_str())
        .collect();
    let mut seen: HashSet<&str> = ids.iter().copied().collect();
    for part in &to_compiled.parts {
        if seen.insert(part.id.as_str()) {
            ids.push(part.id.as_str());
        }
    }
    let from_parts: HashMap<&str, &engine::CompiledPart> = from_compiled
        .parts
        .iter()
        .map(|part| (part.id.as_str(), part))
        .collect();
    let to_parts: HashMap<&str, &engine::CompiledPart> = to_compiled
        .parts
        .iter()
        .map(|part| (part.id.as_str(), part))
        .collect();
    let mut changed_bounds = analysis::Aabb::empty();
    let mut exact = true;
    let (mut added_volume, mut removed_volume, mut net_delta) = (0.0, 0.0, 0.0);
    let mut objects = Vec::new();
    let mut counts = HashMap::<&str, usize>::new();
    let mut printability_text: Vec<String> = Vec::new();
    let beds = (
        recheck.and_then(|checks| checks.bed_z).or_else(|| model_floor(&from_compiled)),
        recheck.and_then(|checks| checks.bed_z).or_else(|| model_floor(&to_compiled)),
    );
    let side_supports = recheck.and_then(|checks| checks.supports).map(|profile| {
        (
            ModelSupports::new(&from_compiled.parts, profile, beds.0),
            ModelSupports::new(&to_compiled.parts, profile, beds.1),
        )
    });
    for id in ids {
        let before = from_parts.get(id).copied();
        let after = to_parts.get(id).copied();
        let measure = |part: Option<&engine::CompiledPart>| {
            part.map(|part| analysis::measure_mesh(&part.mesh))
        };
        let (m_before, m_after) = (measure(before), measure(after));
        let volume_before = m_before.as_ref().map_or(0.0, |m| m.volume);
        let volume_after = m_after.as_ref().map_or(0.0, |m| m.volume);
        let status = match (before, after) {
            (None, Some(_)) => "added",
            (Some(_), None) => "removed",
            (Some(a), Some(b)) if mesh_fingerprint(&a.mesh) == mesh_fingerprint(&b.mesh) => {
                "unchanged"
            }
            _ => "changed",
        };
        *counts.entry(status).or_default() += 1;
        let mut entry = json!({
            "id": id,
            "name": after.or(before).map(|part| part.name.clone()),
            "status": status,
            "volumeBefore": m_before.as_ref().map(|m| m.volume),
            "volumeAfter": m_after.as_ref().map(|m| m.volume),
            "volumeDelta": volume_after - volume_before,
            "boundsBefore": m_before.as_ref().map(|m| aabb_public(&m.bounds)),
            "boundsAfter": m_after.as_ref().map(|m| aabb_public(&m.bounds)),
        });
        net_delta += volume_after - volume_before;
        match status {
            "added" => {
                added_volume += volume_after;
                if let Some(m) = &m_after {
                    changed_bounds.merge(&m.bounds);
                }
                entry["changedBounds"] = m_after.as_ref().map(|m| aabb_public(&m.bounds)).into();
            }
            "removed" => {
                removed_volume += volume_before;
                if let Some(m) = &m_before {
                    changed_bounds.merge(&m.bounds);
                }
                entry["changedBounds"] = m_before.as_ref().map(|m| aabb_public(&m.bounds)).into();
            }
            "changed" => {
                let (a, b) = (before.unwrap(), after.unwrap());
                let (ta, tb) = (
                    analysis::mesh_triangles(&a.mesh),
                    analysis::mesh_triangles(&b.mesh),
                );
                let budget_left = Instant::now() < deadline;
                let removed = budget_left.then(|| analysis::boolean_difference(&ta, &tb));
                let added = budget_left.then(|| analysis::boolean_difference(&tb, &ta));
                match (removed, added) {
                    (Some(Ok(removed)), Some(Ok(added))) => {
                        let mut bounds = analysis::Aabb::empty();
                        for part in [removed.bounds, added.bounds].into_iter().flatten() {
                            bounds.merge(&part);
                        }
                        changed_bounds.merge(&bounds);
                        added_volume += added.volume;
                        removed_volume += removed.volume;
                        entry["removedVolume"] = json!(removed.volume);
                        entry["addedVolume"] = json!(added.volume);
                        entry["symmetricDifferenceVolume"] = json!(removed.volume + added.volume);
                        entry["changedBounds"] =
                            (!bounds.is_empty()).then(|| aabb_public(&bounds)).into();
                        entry["changedBoundsExact"] = json!(true);
                    }
                    (removed, added) => {
                        exact = false;
                        let reason = [removed, added]
                            .into_iter()
                            .find_map(|result| match result {
                                Some(Err(error)) => Some(error.to_string()),
                                None => Some(format!(
                                    "the {} s time budget was used up",
                                    BATCH_BUDGET.as_secs()
                                )),
                                _ => None,
                            })
                            .unwrap_or_default();
                        let mut bounds = analysis::Aabb::empty();
                        for m in [&m_before, &m_after].into_iter().flatten() {
                            bounds.merge(&m.bounds);
                        }
                        changed_bounds.merge(&bounds);
                        entry["changedBounds"] = json!(aabb_public(&bounds));
                        entry["changedBoundsExact"] = json!(false);
                        entry["note"] = json!(format!(
                            "Exact symmetric difference not computed ({reason}); changedBounds is the union of both versions' bounds."
                        ));
                    }
                }
            }
            _ => {}
        }
        entry["source"] = match after.or(before) {
            Some(part) => {
                let text = if after.is_some() { &to_source } else { &from_source };
                part.source_range.map_or(Value::Null, |range| {
                    let start = range.start.min(text.len());
                    let end = range.end.clamp(start, text.len());
                    json!({"start": start, "end": end, "lines": line_span(text, start, end)})
                })
            }
            None => Value::Null,
        };
        entry["summary"] = json!(diff_object_line(&entry));
        if let Some(profile) = recheck.and_then(|checks| checks.supports) {
            let changes = if Instant::now() < deadline {
                let (old, new) = side_supports.as_ref().expect("computed with the profile");
                printability_changes(id, old, new, profile, beds)
            } else {
                json!({"skipped": format!("the {} s time budget was used up", BATCH_BUDGET.as_secs())})
            };
            let introduced = changes["introduced"].as_array().map_or(0, Vec::len);
            if changes.get("skipped").is_some() {
                printability_text.push(format!("{id}: support re-check skipped (time budget)"));
            } else if let Some(resolved) = changes["resolved"].as_array().map(Vec::len) {
                if resolved + introduced > 0 {
                    printability_text.push(format!(
                        "{id}: {resolved} support problems resolved, {introduced} introduced, {} remaining",
                        changes["remaining"]
                    ));
                }
            } else {
                let unconfirmed = changes["unconfirmed"].as_array().map_or(0, Vec::len);
                printability_text.push(format!(
                    "{id}: {unconfirmed} support problems not found again but unconfirmed (the re-check did not finish or listed too many issues), {introduced} introduced"
                ));
            }
            entry["printabilityChanges"] = changes;
        }
        objects.push(entry);
    }
    let clearance_changes = diff_clearances(&from_compiled, &to_compiled, &objects, deadline);
    let mut text: Vec<String> = objects
        .iter()
        .filter(|entry| entry["status"] != json!("unchanged"))
        .filter_map(|entry| entry["summary"].as_str().map(str::to_string))
        .collect();
    for change in &clearance_changes {
        text.push(change["summary"].as_str().unwrap_or_default().to_string());
    }
    text.extend(printability_text);
    if text.is_empty() {
        text.push("No geometric change.".into());
    }
    let mut retained_revisions: Vec<u64> = state
        .workspaces
        .revision_list(&workspace.id)
        .unwrap_or_default()
        .into_iter()
        .map(|info| info.revision)
        .collect();
    retained_revisions.push(workspace.revision);
    let (edit_start, edit_end, edit_text) = scadsource::minimal_edit(&from_source, &to_source);
    let source_change = (from_source != to_source).then(|| {
        json!({
            "fromLines": line_span(&from_source, edit_start, edit_end),
            "toLines": line_span(&to_source, edit_start, edit_start + edit_text.len()),
            "removedBytes": edit_end - edit_start,
            "insertedBytes": edit_text.len(),
        })
    });
    Ok(json!({
        "ok": true,
        "designId": workspace.id,
        "workspaceId": workspace.id,
        "from": {"revision": from_revision},
        "to": {"revision": to_revision},
        "fromConsole": from_compiled.messages,
        "toConsole": to_compiled.messages,
        "warnings": {
            "from": warning_lines(&from_compiled.messages),
            "to": warning_lines(&to_compiled.messages),
        },
        "elapsedMs": diff_started.elapsed().as_millis(),
        "identical": counts.get("changed").is_none() && counts.get("added").is_none() && counts.get("removed").is_none(),
        "summary": {
            "changed": counts.get("changed").copied().unwrap_or(0),
            "added": counts.get("added").copied().unwrap_or(0),
            "removed": counts.get("removed").copied().unwrap_or(0),
            "unchanged": counts.get("unchanged").copied().unwrap_or(0),
            "volumeDelta": net_delta,
            "addedVolume": added_volume,
            "removedVolume": removed_volume,
            "changedBounds": (!changed_bounds.is_empty()).then(|| aabb_public(&changed_bounds)),
            "exact": exact,
            "text": text,
        },
        "clearanceChanges": clearance_changes,
        "sourceChange": source_change,
        "objects": objects,
        "retainedRevisions": retained_revisions,
    }))
}

/// `diff`'s optional re-check: `checks` (only "supports"), `printProfile`
/// and `bedZ`. `None` when no checks were asked for.
fn diff_recheck(input: &Value) -> Result<Option<PrintChecks>, ApiError> {
    if input.get("checks").is_none_or(Value::is_null) {
        return Ok(None);
    }
    let checks = print_checks(input)?;
    if checks.thickness.is_some() || checks.overhang.is_some() || checks.sharp_edges.is_some() {
        return Err(ApiError::bad("diff re-checks only \"supports\"; use checks: [\"supports\"]."));
    }
    Ok(checks.supports.is_some().then_some(checks))
}

/// Problem issues (not `ok`) lying on object `id` on one side of a diff.
fn support_problems<'a>(model: &'a ModelSupports, id: &str) -> Vec<&'a support::SupportIssue> {
    model
        .issues_on(&[id.to_string()])
        .into_iter()
        .map(|index| &model.report.issues[index])
        .filter(|issue| !issue.ok)
        .collect()
}

/// One-to-one matching of support issues before and after an edit: pairs
/// of the same kind at a similar height (within a layer and 1 mm) whose
/// locations lie within max(2 mm, 4 pixels), closest pairs first.
fn match_support_issues(
    old: &[&support::SupportIssue],
    new: &[&support::SupportIssue],
    layer_height: f64,
    resolution: f64,
) -> Vec<(usize, usize)> {
    let reach = 2.0_f64.max(4.0 * resolution);
    let mut candidates: Vec<(f64, usize, usize)> = Vec::new();
    for (i, a) in old.iter().enumerate() {
        for (j, b) in new.iter().enumerate() {
            if a.kind != b.kind || (a.z - b.z).abs() > layer_height + 1.0 {
                continue;
            }
            let distance = (0..3)
                .map(|axis| (a.location[axis] - b.location[axis]).powi(2))
                .sum::<f64>()
                .sqrt();
            if distance <= reach {
                candidates.push((distance, i, j));
            }
        }
    }
    candidates.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (mut used_old, mut used_new) = (vec![false; old.len()], vec![false; new.len()]);
    let mut pairs = Vec::new();
    for (_, i, j) in candidates {
        if !used_old[i] && !used_new[j] {
            used_old[i] = true;
            used_new[j] = true;
            pairs.push((i, j));
        }
    }
    pairs
}

/// `printabilityChanges` of one diff object: support problems resolved,
/// introduced and remaining between the two versions. A problem not found
/// again counts as resolved only when the "after" scan finished and listed
/// every issue; otherwise it is `unconfirmed` and `resolved` is "unknown".
fn printability_changes(
    id: &str,
    before: &ModelSupports,
    after: &ModelSupports,
    profile: SupportProfile,
    beds: (Option<f64>, Option<f64>),
) -> Value {
    let old = support_problems(before, id);
    let new = support_problems(after, id);
    let resolution = before.report.resolution.max(after.report.resolution);
    let pairs = match_support_issues(&old, &new, profile.layer_height, resolution);
    let gone: Vec<Value> = (0..old.len())
        .filter(|i| !pairs.iter().any(|(a, _)| a == i))
        .map(|i| support_issue_public(old[i]))
        .collect();
    let introduced: Vec<Value> = (0..new.len())
        .filter(|j| !pairs.iter().any(|(_, b)| b == j))
        .map(|j| support_issue_public(new[j]))
        .collect();
    let confirmed = after.exhaustive();
    let mut value = json!({
        "introduced": introduced,
        "remaining": pairs.len(),
        "before": old.len(),
        "after": new.len(),
        "complete": confirmed && before.exhaustive(),
        "profile": {
            "layerHeight": profile.layer_height,
            "lineWidth": profile.line_width,
            "maxOverhang": profile.max_overhang,
            "maxBridge": profile.max_bridge,
            "assumed": profile.assumed,
            "bedZBefore": beds.0.map(round3),
            "bedZAfter": beds.1.map(round3),
        },
    });
    if confirmed {
        value["resolved"] = json!(gone);
    } else {
        value["resolved"] = json!("unknown");
        value["unconfirmed"] = json!(gone);
    }
    if !before.exhaustive() {
        value["note"] = json!("The \"before\" scan did not finish or listed too many issues, so some introduced problems may have existed before.");
    }
    value
}

/// Most object pairs whose clearance a diff re-measures.
const MAX_DIFF_CLEARANCE_PAIRS: usize = 16;

/// One line saying what happened to an object in a diff: volume change in
/// percent and whether its bounds moved.
fn diff_object_line(entry: &Value) -> String {
    let id = entry["id"].as_str().unwrap_or_default();
    let label = match entry["name"].as_str() {
        Some(name) if !name.is_empty() && name != id => format!("{id} ({name})"),
        _ => id.to_string(),
    };
    let volume = |key: &str| entry[key].as_f64().unwrap_or(0.0);
    match entry["status"].as_str().unwrap_or_default() {
        "added" => format!("{label} added, volume {:.3} mm^3", volume("volumeAfter")),
        "removed" => format!("{label} removed, volume was {:.3} mm^3", volume("volumeBefore")),
        "unchanged" => format!("{label} unchanged"),
        _ => {
            let (before, after) = (volume("volumeBefore"), volume("volumeAfter"));
            let percent = if before.abs() > 1e-12 {
                format!(" ({:+.2}%)", 100.0 * (after - before) / before)
            } else {
                String::new()
            };
            let bounds = if entry["boundsBefore"] == entry["boundsAfter"] {
                "bounds unchanged".to_string()
            } else {
                let size = |key: &str| -> Option<[f64; 3]> {
                    let bounds = &entry[key];
                    let mut size = [0.0; 3];
                    for (axis, value) in size.iter_mut().enumerate() {
                        *value = bounds["max"][axis].as_f64()? - bounds["min"][axis].as_f64()?;
                    }
                    Some(size)
                };
                match (size("boundsBefore"), size("boundsAfter")) {
                    (Some(a), Some(b)) => format!(
                        "size {:.3} x {:.3} x {:.3} -> {:.3} x {:.3} x {:.3} mm",
                        a[0], a[1], a[2], b[0], b[1], b[2]
                    ),
                    _ => "bounds changed".to_string(),
                }
            };
            format!(
                "{label} volume {before:.3} -> {after:.3} mm^3{percent}, {bounds}"
            )
        }
    }
}

/// Clearance before and after for pairs that involve a changed or added
/// object and exist on both sides (at most [`MAX_DIFF_CLEARANCE_PAIRS`],
/// within the deadline): "object-1/object-3 clearance 0.229 -> 0.229 mm".
fn diff_clearances(
    from: &engine::MultipartCompileOutput,
    to: &engine::MultipartCompileOutput,
    objects: &[Value],
    deadline: Instant,
) -> Vec<Value> {
    let changed: Vec<&str> = objects
        .iter()
        .filter(|entry| entry["status"] == json!("changed"))
        .filter_map(|entry| entry["id"].as_str())
        .collect();
    if changed.is_empty() {
        return Vec::new();
    }
    let part = |output: &'_ engine::MultipartCompileOutput, id: &str| -> Option<MeshIndex> {
        output
            .parts
            .iter()
            .find(|part| part.id == id)
            .map(|part| MeshIndex::from_mesh(&part.mesh))
    };
    let both: Vec<&str> = objects
        .iter()
        .filter(|entry| matches!(entry["status"].as_str(), Some("changed" | "unchanged")))
        .filter_map(|entry| entry["id"].as_str())
        .collect();
    // Unique unordered pairs with at least one changed object, enumerated
    // only up to the cap; the rest are counted, not built.
    let changed_set: HashSet<&str> = changed.iter().copied().collect();
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    let mut eligible = 0usize;
    for (i, a) in both.iter().enumerate() {
        for b in &both[i + 1..] {
            if changed_set.contains(a) || changed_set.contains(b) {
                eligible += 1;
                if pairs.len() < MAX_DIFF_CLEARANCE_PAIRS {
                    pairs.push((a, b));
                }
            }
        }
        if pairs.len() >= MAX_DIFF_CLEARANCE_PAIRS && Instant::now() >= deadline {
            break;
        }
    }
    let skipped = eligible.saturating_sub(pairs.len());
    // (clearance, overlap, decided): undecided when the overlap test or the
    // distance search ran out of budget (the distance is then an upper bound).
    let measure = |output: &engine::MultipartCompileOutput, a: &str, b: &str| -> Option<(f64, bool, bool)> {
        let (ia, ib) = (part(output, a)?, part(output, b)?);
        let epsilon = (bounds_span(&ia).max(bounds_span(&ib)) * 1e-9).max(1e-6);
        let report = analysis::pair_report(&ia, &ib, epsilon)?;
        let decided = report.determined && (report.intersects || report.distance.exact);
        Some((report.distance.distance, report.intersects, decided))
    };
    let describe = |measured: Option<(f64, bool, bool)>| match measured {
        Some((_, true, _)) => "overlap".to_string(),
        Some((clearance, false, true)) => format!("{clearance:.3}"),
        Some((clearance, false, false)) => format!("undetermined (<= {clearance:.3})"),
        None => "?".to_string(),
    };
    let public = |measured: Option<(f64, bool, bool)>| {
        measured.map(|(clearance, overlap, decided)| {
            json!({"clearance": clearance, "overlap": overlap, "determined": decided})
        })
    };
    let mut changes = Vec::new();
    for (a, b) in pairs {
        if Instant::now() >= deadline {
            break;
        }
        let (before, after) = (measure(from, a, b), measure(to, a, b));
        let same = match (before, after) {
            (Some((x, i, true)), Some((y, j, true))) => i == j && (x - y).abs() < 1e-6,
            _ => false,
        };
        changes.push(json!({
            "a": a,
            "b": b,
            "before": public(before),
            "after": public(after),
            "summary": format!(
                "{a}/{b} clearance {} -> {} mm{}",
                describe(before),
                describe(after),
                if same { " (unchanged)" } else { "" }
            ),
        }));
    }
    if skipped > 0 {
        changes.push(json!({"summary": format!("{skipped} more pairs not re-measured; use check_intersections.")}));
    }
    changes
}

fn bounds_span(index: &MeshIndex) -> f64 {
    let bounds = index.bounds();
    (0..3)
        .map(|axis| bounds.max[axis] - bounds.min[axis])
        .fold(0.0f64, f64::max)
}

// ---------------------------------------------------------------------------
// render_image
// ---------------------------------------------------------------------------

/// Render options accepted at the top level of the request (snake_case as
/// [`raster::RenderOptions`] spells them, or camelCase), optionally nested
/// under `camera`.
const RENDER_OPTION_KEYS: &[(&str, &str)] = &[
    ("width", "width"),
    ("height", "height"),
    ("view", "view"),
    ("eye", "eye"),
    ("target", "target"),
    ("up", "up"),
    ("projection", "projection"),
    ("fov_deg", "fov_deg"),
    ("fovDeg", "fov_deg"),
    ("zoom", "zoom"),
    ("view_height_mm", "view_height_mm"),
    ("viewHeightMm", "view_height_mm"),
    ("background", "background"),
    ("section", "section"),
    ("edges", "edges"),
    ("axes", "axes"),
    ("grid", "grid"),
    ("label", "label"),
    ("supersample", "supersample"),
];

pub(crate) fn render_options(input: &Value) -> Result<raster::RenderOptions, ApiError> {
    let mut options = serde_json::Map::new();
    options.insert("width".into(), json!(DEFAULT_IMAGE_WIDTH));
    options.insert("height".into(), json!(DEFAULT_IMAGE_HEIGHT));
    for scope in [input.get("camera"), Some(input)].into_iter().flatten() {
        for (key, field) in RENDER_OPTION_KEYS {
            if let Some(value) = scope.get(*key).filter(|value| !value.is_null()) {
                options.insert((*field).to_string(), value.clone());
            }
        }
    }
    serde_json::from_value(Value::Object(options))
        .map_err(|error| ApiError::bad(format!("Invalid render options: {error}")))
}

fn rgba_hex(color: raster::Rgba) -> String {
    format!("#{:02x}{:02x}{:02x}", color[0], color[1], color[2])
}

/// The validated, compile-independent arguments of an image.
struct ImageRequest {
    options: raster::RenderOptions,
    colors: HashMap<String, raster::Color>,
    show_background: bool,
    /// Several named views tiled into one contact sheet.
    views: Option<Vec<raster::NamedView>>,
}

/// `views`: up to [`raster::MAX_SHEET_VIEWS`] named views for one tiled
/// contact sheet. Without an explicit size the sheet gets 400x320 per tile.
fn sheet_views(input: &Value, options: &mut raster::RenderOptions) -> Result<Option<Vec<raster::NamedView>>, ApiError> {
    let views = match input.get("views") {
        None | Some(Value::Null) => return Ok(None),
        Some(value) => serde_json::from_value::<Vec<raster::NamedView>>(value.clone()).map_err(|_| {
            ApiError::bad("views must be an array of named views: default, iso, front, back, left, right, top, bottom.")
        })?,
    };
    if views.is_empty() || views.len() > raster::MAX_SHEET_VIEWS {
        return Err(ApiError::bad(format!(
            "views must list 1 to {} named views.",
            raster::MAX_SHEET_VIEWS
        )));
    }
    if options.eye.is_some() {
        return Err(ApiError::bad("views and eye are exclusive: views uses auto-framed named cameras."));
    }
    let sized = |key: &str| {
        [input.get("camera"), Some(input)]
            .into_iter()
            .flatten()
            .any(|scope| scope.get(key).is_some_and(|value| !value.is_null()))
    };
    let (columns, rows) = raster::sheet_grid(views.len());
    if !sized("width") {
        options.width = 400 * columns;
    }
    if !sized("height") {
        options.height = 320 * rows;
    }
    Ok(Some(views))
}

fn image_request(input: &Value) -> Result<ImageRequest, ApiError> {
    let mut options = render_options(input)?;
    let views = sheet_views(input, &mut options)?;
    let colors: HashMap<String, raster::Color> = match input.get("colors") {
        None | Some(Value::Null) => HashMap::new(),
        Some(value) => serde_json::from_value(value.clone()).map_err(|error| {
            ApiError::bad(format!(
                "colors must map object IDs to \"#rrggbb\" or [r, g, b]: {error}"
            ))
        })?,
    };
    let show_background = input
        .get("showBackground")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(ImageRequest {
        options,
        colors,
        show_background,
        views,
    })
}

/// Renders the selected objects of one compiled source: the PNG and its
/// metadata (`mimeType`, `bytes`, `render`, `objects`). Shared by `image`
/// and `evaluate`.
fn image_compiled(
    compiled: &engine::MultipartCompileOutput,
    input: &Value,
    request: &ImageRequest,
) -> Result<(Vec<u8>, Value), ApiError> {
    let ImageRequest {
        options,
        colors,
        show_background,
        views,
    } = request;
    let visible: HashSet<&str> = select_parts(&compiled.parts, input, "objectIds")?
        .into_iter()
        .map(|part| part.id.as_str())
        .collect();
    let known: HashSet<&str> = compiled
        .parts
        .iter()
        .chain(compiled.background_parts.iter())
        .map(|part| part.id.as_str())
        .collect();
    for key in colors.keys() {
        if !known.contains(key.as_str()) {
            return Err(ApiError::bad(format!(
                "colors names unknown object ID {key:?}."
            )));
        }
    }
    let scene: Vec<&engine::CompiledPart> = compiled
        .parts
        .iter()
        .chain(compiled.background_parts.iter().filter(|_| *show_background))
        .collect();
    // Colour with alpha: a `color([r, g, b, a])` below 1 renders translucent
    // (blended over what is behind it), as does the grey of `%` background
    // objects; an override colour is opaque.
    let colour_of = |part: &engine::CompiledPart| -> Option<raster::Rgba> {
        colors
            .get(&part.id)
            .map(|color| color.0)
            .or_else(|| {
                part.color
                    .and_then(|color| raster::color_from_numbers(&color))
            })
            .or_else(|| part.background.then_some([0xb4, 0xb4, 0xb4, 0x80]))
    };
    let is_visible = |part: &engine::CompiledPart| part.background || visible.contains(part.id.as_str());
    let labels: Vec<String> = scene
        .iter()
        .map(|part| format!("{} [{}]", part.name, part.id))
        .collect();
    let objects: Vec<raster::RenderObject> = scene
        .iter()
        .zip(&labels)
        .map(|(part, label)| raster::RenderObject {
            name: label.as_str(),
            mesh: &part.mesh,
            color: colour_of(part),
            // An override colour repaints the whole object.
            face_colors: part
                .face_colors
                .as_ref()
                .filter(|_| !colors.contains_key(&part.id)),
            visible: is_visible(part),
        })
        .collect();
    let (png, render) = match views {
        Some(views) => {
            let sheet = raster::render_sheet(&objects, options, views)
                .map_err(|error| ApiError::new(422, format!("Image could not be rendered: {error}.")))?;
            (sheet.png.clone(), sheet_public(&sheet))
        }
        None => {
            let output = raster::render(&objects, options)
                .map_err(|error| ApiError::new(422, format!("Image could not be rendered: {error}.")))?;
            (output.png, serde_json::to_value(&output.meta).unwrap_or(Value::Null))
        }
    };
    let meta = json!({
        "mimeType": "image/png",
        "bytes": png.len(),
        "render": render,
        "objects": scene
            .iter()
            .map(|part| {
                let colour = colour_of(part);
                json!({
                    "id": part.id,
                    "name": part.name,
                    "visible": is_visible(part),
                    "background": part.background,
                    "color": colour.map(rgba_hex).unwrap_or_else(|| rgba_hex(raster::OPENSCAD_YELLOW)),
                    "alpha": colour.map_or(1.0, |colour| f64::from(colour[3]) / 255.0),
                })
            })
            .collect::<Vec<_>>(),
    });
    Ok((png, meta))
}

/// Contact-sheet metadata: the sheet layout and, per tile, its pixel rect
/// and the camera it used (so any tile can be re-rendered alone).
fn sheet_public(sheet: &raster::SheetOutput) -> Value {
    let round = |values: [f64; 3]| values.map(|value| round_to(value, 3));
    json!({
        "width": sheet.width,
        "height": sheet.height,
        "columns": sheet.columns,
        "rows": sheet.rows,
        "visibleBounds": sheet.tiles.first().and_then(|tile| tile.meta.visible_bounds.as_ref()).map(|bounds| json!({
            "min": round(bounds.min),
            "max": round(bounds.max),
        })),
        "tiles": sheet.tiles.iter().map(|tile| json!({
            "view": tile.view,
            "x": tile.x,
            "y": tile.y,
            "width": tile.meta.width,
            "height": tile.meta.height,
            "modelArea": tile.meta.model_area,
            "projection": tile.meta.camera.projection,
            "eye": round(tile.meta.camera.eye),
            "target": round(tile.meta.camera.target),
            "up": round(tile.meta.camera.up),
            "viewHeightMm": round_to(tile.meta.camera.view_height_mm, 3),
            "mmPerPixel": round_to(tile.meta.mm_per_pixel, 4),
            "gridSpacingMm": tile.meta.grid_spacing_mm,
        })).collect::<Vec<_>>(),
    })
}

/// `value` rounded to `decimals` places (non-finite values unchanged).
fn round_to(value: f64, decimals: i32) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let scale = 10f64.powi(decimals);
    let rounded = (value * scale).round() / scale;
    if rounded == 0.0 { 0.0 } else { rounded }
}

/// Renders the selected objects to a PNG; returns the bytes and metadata.
pub(crate) fn image_value(
    state: &AppState,
    id: Option<&str>,
    input: &Value,
) -> Result<(Vec<u8>, Value), ApiError> {
    let started = Instant::now();
    let (_, _deadline_scope) = request_deadline(started);
    let source = eval_source(state, id, input)?;
    let request = image_request(input)?;
    let compiled = compile_eval(&source)?;
    let (png, image) = image_compiled(&compiled, input, &request)?;
    let mut meta = source.header();
    meta["ok"] = json!(true);
    merge_fields(&mut meta, image);
    attach_diagnostics(&mut meta, &compiled.messages, started);
    Ok((png, meta))
}

// ---------------------------------------------------------------------------
// evaluate: one compile, every answer
// ---------------------------------------------------------------------------

/// `input` with `overlay`'s fields (an object) on top; `drop` keys removed
/// from the base first.
fn overlaid(input: &Value, overlay: Option<&Value>, drop: &[&str]) -> Value {
    let mut merged = input.clone();
    if let Some(object) = merged.as_object_mut() {
        for key in drop {
            object.remove(*key);
        }
    }
    if let Some(overlay) = overlay.filter(|value| value.is_object()) {
        merge_fields(&mut merged, overlay.clone());
    }
    merged
}

/// A section switch of `evaluate`: `true`/`false`, or an object of options
/// (which turns it on). `None` when absent.
fn section_switch(input: &Value, key: &str) -> Result<Option<bool>, ApiError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(on)) => Ok(Some(*on)),
        Some(Value::Object(_)) => Ok(Some(true)),
        Some(_) => Err(ApiError::bad(format!(
            "{key} must be true, false or an object of options."
        ))),
    }
}

/// Flags every object of a `measure` / `evaluate` reply whose mesh is not a
/// closed, consistently wound manifold solid: a WARNING line each (the model
/// compiled, but a slicer may misprint or "repair" it), and `meshes`
/// `{valid, invalid: [...]}`. Returns whether all meshes are valid.
fn flag_invalid_meshes(value: &mut Value) -> bool {
    let mut invalid = Vec::new();
    let mut warnings = Vec::new();
    for object in value["objects"].as_array().into_iter().flatten() {
        let good = |key: &str| object[key] != json!(false);
        if good("watertight") && good("manifold") && good("windingConsistent") && good("outwardNormals") {
            continue;
        }
        let count = |key: &str| object[key].as_u64().unwrap_or(0);
        let id = object["id"].as_str().unwrap_or_default();
        let mut problems = Vec::new();
        if count("boundaryEdges") > 0 {
            problems.push(format!("{} open (boundary) edges", count("boundaryEdges")));
        }
        if count("nonManifoldEdges") > 0 {
            problems.push(format!("{} non-manifold edges", count("nonManifoldEdges")));
        }
        if count("inconsistentEdges") > 0 {
            problems.push(format!("{} edges with inconsistent winding", count("inconsistentEdges")));
        }
        if object["outwardNormals"] == json!(false) {
            problems.push("inside-out (negative volume)".to_string());
        }
        // Point at the largest defect group (the object's `defects` lists
        // them all), so the fix can start there instead of bisecting.
        let near = object["defects"]
            .get(0)
            .map(|cluster| {
                format!(
                    " ({} {} edges around {})",
                    cluster["edges"],
                    if cluster["kind"] == json!("boundary") { "open" } else { "non-manifold" },
                    cluster["centroid"]
                )
            })
            .unwrap_or_default();
        if let Some(first) = problems.first_mut() {
            first.push_str(&near);
        }
        warnings.push(json!(format!(
            "WARNING: {id} ({}) compiled but is not a valid closed solid: {}. A slicer may refuse, misprint or silently repair it; look for self-overlapping 2D outlines (offset of text, overlapping polygon() paths) or zero-thickness joins.",
            object["name"].as_str().unwrap_or(id),
            if problems.is_empty() { "mesh checks failed".to_string() } else { problems.join(", ") }
        )));
        invalid.push(json!({
            "id": id,
            "boundaryEdges": count("boundaryEdges"),
            "nonManifoldEdges": count("nonManifoldEdges"),
            "inconsistentEdges": count("inconsistentEdges"),
        }));
    }
    let valid = invalid.is_empty();
    if let Some(list) = value["warnings"].as_array_mut() {
        list.extend(warnings);
    } else if !warnings.is_empty() {
        value["warnings"] = json!(warnings);
    }
    value["meshes"] = json!({"valid": valid, "invalid": invalid});
    valid
}

/// `evaluate`'s `verdict`: compiling and being printable are different
/// claims, so they are reported apart, with what was and was not checked.
fn evaluate_verdict(value: &Value, meshes_valid: bool) -> Value {
    let mut problems = Vec::new();
    let mut checked = vec!["mesh validity (watertight, manifold, winding)"];
    let mut not_checked = Vec::new();
    if !meshes_valid {
        problems.push("one or more objects are not valid closed solids (see meshes.invalid)".to_string());
    }
    let mut incomplete: Vec<String> = Vec::new();
    let objects = value["objects"].as_array().cloned().unwrap_or_default();
    let mut thickness_checks: Vec<(&str, &Value)> = Vec::new();
    for object in &objects {
        for key in ["thickness", "thicknessAt"] {
            if let Some(check) = object["checks"].get(key).filter(|check| check.is_object()) {
                thickness_checks.push((key, check));
            }
        }
    }
    if thickness_checks.is_empty() {
        not_checked.push("wall thickness (pass checks: [\"thickness\"])");
    } else {
        checked.push("wall thickness");
        for (key, check) in &thickness_checks {
            // A probe that reached no surface of this object says nothing.
            if check.get("note").is_some() && check.get("samples").is_none() {
                continue;
            }
            let thin = check["belowThreshold"].as_u64().unwrap_or(0);
            if thin > 0 {
                problems.push(format!("{key}: {thin} wall-thickness samples below the threshold"));
            }
            if check["complete"] == json!(false) || check["samples"].as_u64() == Some(0) || check["min"].is_null() {
                incomplete.push(format!("{key}: the scan did not finish or measured nothing"));
            }
        }
    }
    let support_checks: Vec<(&Value, &Value)> = objects
        .iter()
        .filter_map(|object| {
            object["checks"].get("supports").filter(|check| check.is_object()).map(|check| (object, check))
        })
        .collect();
    let supports_checked = !support_checks.is_empty();
    let mut support_notes: Vec<String> = Vec::new();
    if supports_checked {
        checked.push("supports layer by layer (islands, cantilevers, bridges) under the print profile");
        // Every object carries the same whole-model scan: counts (totals,
        // counted before the issue list was truncated), notes, complete.
        let model = support_checks[0].1;
        let on = |kind: &str, ok: Option<bool>| -> String {
            let ids: Vec<&str> = support_checks
                .iter()
                .filter(|(_, check)| {
                    check["issues"].as_array().is_some_and(|issues| {
                        issues.iter().any(|issue| {
                            issue["kind"] == json!(kind) && ok.is_none_or(|ok| issue["ok"] == json!(ok))
                        })
                    })
                })
                .filter_map(|(object, _)| object["id"].as_str())
                .collect();
            if ids.is_empty() {
                String::new()
            } else {
                format!(" (on {})", ids.join(", "))
            }
        };
        let counts = &model["counts"];
        let islands = counts["islands"].as_u64().unwrap_or(0);
        let cantilevers = counts["cantilevers"].as_u64().unwrap_or(0);
        let long = counts["bridgesTooLong"].as_u64().unwrap_or(0);
        if islands > 0 {
            problems.push(format!("{islands} islands start in midair and need support{}", on("island", None)));
        }
        if cantilevers > 0 {
            problems.push(format!("{cantilevers} cantilevers overhang beyond the profile{}", on("cantilever", None)));
        }
        if long > 0 {
            problems.push(format!("{long} bridges longer than maxBridge{}", on("bridge", Some(false))));
        }
        let truncated = model["truncated"].as_u64().unwrap_or(0);
        if truncated > 0 {
            support_notes.push(format!(
                "{truncated} support issues beyond the first {} are counted above but not listed; fix the listed ones and re-check to see the rest",
                support::MAX_SUPPORT_ISSUES
            ));
        }
        if model["complete"] == json!(false) {
            incomplete.push("supports: the layer-by-layer analysis did not finish".to_string());
        }
        for note in model["notes"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if support_note_is_incomplete(note) {
                incomplete.push(format!("supports: {note}"));
            } else {
                support_notes.push(note.to_string());
            }
        }
    } else {
        not_checked.push("supports layer by layer: islands, cantilevers, bridges (pass checks: [\"supports\"])");
    }
    match value["intersections"]["summary"]["toleranceViolations"].as_u64() {
        Some(violations) => {
            checked.push("intersections and clearance");
            if violations > 0 {
                problems.push(format!("{violations} object pairs violate the tolerance"));
            }
            if value["intersections"]["complete"] == json!(false) {
                problems.push("some intersection pairs were not decided".to_string());
            }
        }
        None => not_checked.push("intersections (intersections: false)"),
    }
    if let Some(failed) = value["asserts"]["failed"].as_u64() {
        checked.push("asserts");
        if failed > 0 {
            problems.push(format!("{failed} asserts fail"));
        }
    }
    let printability = if !problems.is_empty() {
        "obstacles"
    } else if !incomplete.is_empty() {
        "incomplete"
    } else if !supports_checked {
        "not-checked"
    } else {
        "clear-under-profile"
    };
    json!({
        "compiled": true,
        "meshesValid": meshes_valid,
        // True only when supports were analysed and every check that ran
        // finished and found nothing; a watertight mesh alone is not enough.
        "printable": printability == "clear-under-profile",
        "printability": printability,
        "supportNotes": support_notes,
        "problems": problems,
        "incomplete": incomplete,
        "checked": checked,
        "notChecked": not_checked,
        "note": "printability: obstacles = a check found problems; incomplete = a check did not finish; not-checked = the layer-by-layer supports check was not requested (checks: [\"supports\"]), so printable stays false; clear-under-profile = supports were analysed and nothing was found under the stated generic print profile (not a guarantee for every printer or material). What is under notChecked (bed adhesion, slicer output) is not judged.",
    })
}

/// What `evaluate` was asked to probe: `asserts` (true, or "all" to list
/// every assert rather than the failing and tightest ones) and `echo`.
struct ProbeRequest {
    asserts: Option<bool>,
    echo: Vec<String>,
}

impl ProbeRequest {
    fn from_input(input: &Value) -> Result<Self, ApiError> {
        let asserts = match input.get("asserts") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => None,
            Some(Value::Bool(true)) => Some(false),
            Some(Value::String(all)) if all == "all" => Some(true),
            Some(_) => return Err(ApiError::bad("asserts must be true, false or \"all\".")),
        };
        let echo = match input.get("echo") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(names)) => names
                .iter()
                .map(|name| {
                    name.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| ApiError::bad("echo must be a list of variable names."))
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return Err(ApiError::bad("echo must be a list of variable names.")),
        };
        Ok(Self { asserts, echo })
    }

    fn any(&self) -> bool {
        self.asserts.is_some() || !self.echo.is_empty()
    }
}

/// Rewrites `source` per [`scadsource::instrument`], keeping offsets mapped
/// back to the original source; returns the assert sites.
fn probe_source(
    source: &mut EvalSource,
    request: &ProbeRequest,
) -> Result<Vec<scadsource::AssertSite>, ApiError> {
    let mut instrumented = scadsource::instrument(&source.source, request.asserts.is_some(), &request.echo)
        .map_err(ApiError::bad)?;
    // Assert lines as in the original source (an override can change the
    // line count above an assert).
    for site in &mut instrumented.asserts {
        let original = source.overridden.original_offset(site.start).min(source.base.len());
        site.line = scadsource::line_of(&source.base, original);
    }
    // The new edits are in post-override coordinates and never inside an
    // override's replacement (that is a literal), so they map back exactly.
    let mut edits = source.overridden.edits.clone();
    for (start, end, length) in &instrumented.edits {
        edits.push((
            source.overridden.original_offset(*start),
            source.overridden.original_offset(*end),
            *length,
        ));
    }
    edits.sort_by_key(|edit| edit.0);
    source.overridden.edits = edits;
    source.source = instrumented.source;
    source.instrumented = true;
    Ok(instrumented.asserts)
}

/// A value from an ECHO line as JSON: a literal when it reads as one, else
/// the display text.
fn echo_value(text: &str) -> Value {
    if text == "undef" {
        return Value::Null;
    }
    scadsource::literal_value(text).unwrap_or_else(|| json!(text))
}

/// Splits the instrumentation's ECHO lines out of the console; returns the
/// model's own lines and the probe records (kind, index, values).
fn split_probe_lines(messages: &[String]) -> (Vec<String>, Vec<(String, usize, Vec<String>)>) {
    let prefix = format!("ECHO: \"{}\", ", scadsource::PROBE_MARKER);
    let mut console = Vec::new();
    let mut probes = Vec::new();
    for line in messages {
        let Some(rest) = line.strip_prefix(&prefix) else {
            console.push(line.clone());
            continue;
        };
        let mut values = scadsource::split_echo_values(rest).into_iter();
        let kind = values.next().unwrap_or_default().trim_matches('"').to_string();
        let Some(index) = values.next().and_then(|index| index.parse().ok()) else {
            continue;
        };
        probes.push((kind, index, values.collect()));
    }
    (console, probes)
}

/// How far a comparison is from failing, in its own units: positive while it
/// holds (b - a for a < b), negative by how much it fails.
fn comparison_margin(op: &str, lhs: f64, rhs: f64) -> f64 {
    match op {
        "<" | "<=" => rhs - lhs,
        ">" | ">=" => lhs - rhs,
        "==" => -(lhs - rhs).abs(),
        _ => (lhs - rhs).abs(),
    }
}

/// `asserts` and `echo` sections of an evaluate reply from the probe lines.
fn probe_report(
    request: &ProbeRequest,
    sites: &[scadsource::AssertSite],
    probes: &[(String, usize, Vec<String>)],
) -> (Option<Value>, Option<Value>) {
    let echo = (!request.echo.is_empty()).then(|| {
        let mut values: Vec<Value> = request
            .echo
            .iter()
            .map(|name| json!({"name": name, "value": Value::Null, "evaluated": false}))
            .collect();
        for (kind, index, fields) in probes {
            if kind == "echo" {
                if let (Some(entry), Some(text)) = (values.get_mut(*index), fields.first()) {
                    entry["value"] = echo_value(text);
                    entry["evaluated"] = json!(true);
                }
            }
        }
        json!(values)
    });
    let Some(list_all) = request.asserts else {
        return (None, echo);
    };
    struct Seen {
        evaluations: usize,
        failures: usize,
        margin: Option<f64>,
        worst: Vec<Value>,
    }
    let mut seen: Vec<Seen> = sites
        .iter()
        .map(|_| Seen { evaluations: 0, failures: 0, margin: None, worst: Vec::new() })
        .collect();
    for (kind, index, fields) in probes {
        let (Some(site), Some(entry)) = (sites.get(*index), seen.get_mut(*index)) else {
            continue;
        };
        if kind != "assert" {
            continue;
        }
        entry.evaluations += 1;
        let passed = fields.first().map(String::as_str) == Some("true");
        if !passed {
            entry.failures += 1;
        }
        let mut margin: Option<f64> = None;
        let mut details = Vec::new();
        for (position, comparison) in site.comparisons.iter().enumerate() {
            let lhs = fields.get(1 + 2 * position).map(String::as_str).unwrap_or("undef");
            let rhs = fields.get(2 + 2 * position).map(String::as_str).unwrap_or("undef");
            let this = match (lhs.parse::<f64>(), rhs.parse::<f64>()) {
                (Ok(a), Ok(b)) => Some(comparison_margin(&comparison.op, a, b)),
                _ => None,
            };
            if let Some(this) = this {
                margin = Some(margin.map_or(this, |m: f64| m.min(this)));
            }
            details.push(json!({
                "lhs": comparison.lhs,
                "op": comparison.op,
                "rhs": comparison.rhs,
                "lhsValue": echo_value(lhs),
                "rhsValue": echo_value(rhs),
                "margin": this,
            }));
        }
        // Keep the evaluation closest to failing (or failing worst).
        let worse = match (margin, entry.margin) {
            (Some(new), Some(old)) => new < old,
            (Some(_), None) => true,
            (None, _) => entry.worst.is_empty() || !passed,
        };
        if worse {
            if margin.is_some() {
                entry.margin = margin;
            }
            entry.worst = details;
        }
    }
    let mut rows: Vec<Value> = sites
        .iter()
        .zip(&seen)
        .map(|(site, seen)| {
            json!({
                "line": site.line,
                "condition": site.condition,
                "evaluations": seen.evaluations,
                "failed": seen.failures,
                "ok": seen.failures == 0,
                "margin": seen.margin,
                "comparisons": seen.worst,
            })
        })
        .collect();
    let failed = rows.iter().filter(|row| row["ok"] == json!(false)).count();
    let not_evaluated = rows.iter().filter(|row| row["evaluations"] == json!(0)).count();
    let total = rows.len();
    if !list_all {
        // Failing ones, then the five passing ones closest to failing.
        let mut passing: Vec<Value> = rows
            .iter()
            .filter(|row| row["ok"] == json!(true) && row["margin"].is_number())
            .cloned()
            .collect();
        passing.sort_by(|a, b| a["margin"].as_f64().unwrap_or(f64::INFINITY).total_cmp(&b["margin"].as_f64().unwrap_or(f64::INFINITY)));
        passing.truncate(5);
        rows.retain(|row| row["ok"] == json!(false));
        rows.extend(passing);
    }
    let mut section = json!({
        "total": total,
        "failed": failed,
        "notEvaluated": not_evaluated,
        "listed": if list_all { "all" } else { "failing, then the 5 passing asserts with the smallest margin" },
        "asserts": rows,
        "marginNote": "margin is how far each comparison is from failing in its own units (b - a for a < b, a - b for a > b, -|a - b| for ==), the smallest over the assert's comparisons and evaluations; negative means it fails. comparisons shows the evaluation closest to failing.",
    });
    if failed > 0 {
        section["note"] = json!("As written the design stops at the first failing assert; this evaluation ran past the asserts to report all of them, so the geometry is what the code would build without them.");
    }
    (Some(section), echo)
}

/// Compiles the current revision once and answers, from that one compile and
/// within one permit and one time budget: the objects with bounds, volume,
/// watertight/manifold status (as `measure`, including `pairs`, `groups`,
/// `splitComponents` and `checks`), every pairwise intersection/clearance
/// verdict against the tolerance (as `check_intersections`; on by default,
/// `intersections: false` skips it, an object sets its own tolerance,
/// allowContact, visibleObjectIds, groups or splitComponents), optionally a
/// PNG (`image: true` or an object of `render_image` options), and the
/// diagnostics.
pub(crate) fn evaluate_value(
    state: &AppState,
    id: Option<&str>,
    input: &Value,
) -> Result<(Option<Vec<u8>>, Value), ApiError> {
    evaluate_revision(state, id, input, None)
}

/// [`evaluate_value`], refusing with a conflict when the current revision is not
/// `pinned`. Checked on the one document read that also supplies the source, so
/// a reply saying it evaluated what the call saved can only be about that.
fn evaluate_revision(
    state: &AppState,
    id: Option<&str>,
    input: &Value,
    pinned: Option<u64>,
) -> Result<(Option<Vec<u8>>, Value), ApiError> {
    let started = Instant::now();
    let (deadline, _deadline_scope) = request_deadline(started);
    let mut source = eval_source(state, id, input)?;
    if let (Some(pinned), Some(current)) = (pinned, source.revision) {
        if pinned != current {
            return Err(ApiError::new(
                409,
                format!("Revision conflict: revision {current} landed before revision {pinned} could be evaluated. Call evaluate again to check the current revision."),
            ));
        }
    }
    // Every argument is validated before the compile it would waste.
    let measure = measure_request(input)?;
    let probe = ProbeRequest::from_input(input)?;
    let sites = if probe.any() {
        probe_source(&mut source, &probe)?
    } else {
        Vec::new()
    };
    let check_intersections = section_switch(input, "intersections")?.unwrap_or(true);
    // Intersections check the measured selection unless told otherwise.
    let options = input.get("intersections").filter(|value| value.is_object());
    let own_selection = options.is_some_and(|options| {
        options.get("visibleObjectIds").is_some() || options.get("hiddenObjectIds").is_some()
    });
    let mut intersection_input = if own_selection {
        overlaid(input, None, &["objectIds", "hiddenObjectIds", "pairs"])
    } else {
        let mut base = overlaid(input, None, &["objectIds", "pairs"]);
        if let Some(ids) = input.get("objectIds").filter(|value| !value.is_null()) {
            base["visibleObjectIds"] = ids.clone();
        }
        base
    };
    intersection_input = overlaid(&intersection_input, options, &[]);
    let tolerance = tolerance_input(&intersection_input, &source)?;
    let contact = ContactPolicy::from_input(&intersection_input)?;
    let render_image = section_switch(input, "image")?.unwrap_or(false);
    // The image shows the measured selection unless its options say
    // otherwise; render options are read from the image object only.
    let mut image_input = json!({});
    for key in ["objectIds", "hiddenObjectIds"] {
        if let Some(ids) = input.get(key).filter(|value| !value.is_null()) {
            image_input[key] = ids.clone();
        }
    }
    if let Some(options) = input.get("image").filter(|value| value.is_object()) {
        if options.get("objectIds").is_some() || options.get("hiddenObjectIds").is_some() {
            image_input = json!({});
        }
        merge_fields(&mut image_input, options.clone());
    }
    let image = render_image.then(|| image_request(&image_input)).transpose()?;

    let compiled = compile_eval(&source)?;
    let mut value = source.header();
    value["ok"] = json!(true);
    merge_fields(&mut value, measure_compiled(&source, &compiled, input, &measure)?);
    value["intersections"] = if check_intersections {
        let mut section = json!({});
        intersection_header(&mut section, tolerance, &contact);
        merge_fields(
            &mut section,
            intersections_compiled(
                &source,
                &compiled,
                &intersection_input,
                tolerance,
                &contact,
                deadline,
            )?,
        );
        section
    } else {
        Value::Null
    };
    // Complete when every requested answer is decided.
    value["complete"] = json!(!check_intersections || value["intersections"]["complete"] == json!(true));
    let png = match &image {
        Some(request) => {
            let (png, mut meta) = image_compiled(&compiled, &image_input, request)?;
            meta["base64"] = json!(base64_encode(&png));
            value["image"] = meta;
            Some(png)
        }
        None => None,
    };
    if probe.any() {
        let (console, probes) = split_probe_lines(&compiled.messages);
        let (asserts, echo) = probe_report(&probe, &sites, &probes);
        if let Some(asserts) = asserts {
            value["asserts"] = asserts;
        }
        if let Some(echo) = echo {
            value["echo"] = echo;
        }
        attach_diagnostics(&mut value, &console, started);
    } else {
        attach_diagnostics(&mut value, &compiled.messages, started);
    }
    let meshes_valid = flag_invalid_meshes(&mut value);
    value["verdict"] = evaluate_verdict(&value, meshes_valid);
    Ok((png, value))
}

/// Mesh-health fields of an object record that are noise while the mesh is
/// healthy: dropped (with the counts) when the object is a clean closed solid.
const HEALTHY_MESH_FIELDS: &[&str] = &[
    "watertight",
    "manifold",
    "windingConsistent",
    "outwardNormals",
    "boundaryEdges",
    "nonManifoldEdges",
    "inconsistentEdges",
    "degenerateTriangles",
    "defects",
    "defectClusters",
];

/// The agent-facing shape of `evaluate` / `check_intersections` replies
/// (`verbose: true` keeps the full records the REST API returns).
///
/// Objects keep id, name, bounds, volume, source lines and any checks; mesh
/// statistics only when the mesh is not a healthy closed solid (`meshOk`
/// says which). Pairs keep `a, b, relation, clearance` when clean, and the
/// fields that locate the problem otherwise (see [`slim_pair`]).
pub(crate) fn slim_analysis(value: &mut Value, input: &Value) {
    if input.get("verbose").and_then(Value::as_bool) == Some(true) {
        return;
    }
    if let Some(objects) = value.get_mut("objects").and_then(Value::as_array_mut) {
        objects.iter_mut().for_each(slim_object);
    }
    slim_intersection_section(value);
    if let Some(section) = value.get_mut("intersections").filter(|section| section.is_object()) {
        slim_intersection_section(section);
    }
    // evaluate: the fixed explanations live in the tool description.
    for (section, keys) in [("verdict", &["note", "incomplete", "supportNotes"][..]), ("total", &["note"][..])] {
        if let Some(fields) = value.get_mut(section).and_then(Value::as_object_mut) {
            fields.retain(|key, field| !(keys.contains(&key.as_str()) && (key == "note" || is_empty_json(field))));
        }
    }
    if value.get("distances").is_some_and(is_empty_json) {
        if let Some(fields) = value.as_object_mut() {
            fields.remove("distances");
        }
    }
}

/// Null, `[]`, `{}` or `""`: a field an agent learns nothing from.
fn is_empty_json(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Array(items) => items.is_empty(),
        Value::Object(fields) => fields.is_empty(),
        Value::String(text) => text.is_empty(),
        _ => false,
    }
}

/// The per-reply boilerplate of an intersection result (the semantics and
/// method text live in the tool description), its pairs slimmed and its
/// summary without zero counts.
fn slim_intersection_section(holder: &mut Value) {
    if let Some(fields) = holder.as_object_mut() {
        for key in ["semantics", "method", "supported"] {
            fields.remove(key);
        }
    }
    for key in ["pairs", "intersections"] {
        if let Some(pairs) = holder.get_mut(key).and_then(Value::as_array_mut) {
            for pair in pairs.iter_mut().filter(|pair| pair.get("relation").is_some()) {
                *pair = slim_pair(pair);
            }
        }
    }
    if let Some(summary) = holder.get_mut("summary") {
        slim_pair_summary(summary);
    }
}

/// A pair summary without zero counts, nulls or the `intersecting` alias of
/// `overlaps`; `pairs`, `complete` and `toleranceViolations` always stay.
fn slim_pair_summary(summary: &mut Value) {
    if let Some(fields) = summary.as_object_mut() {
        if !fields.contains_key("toleranceViolations") {
            return;
        }
        fields.remove("intersecting");
        fields.retain(|key, field| {
            matches!(key.as_str(), "pairs" | "complete" | "toleranceViolations")
                || !(field.is_null() || *field == json!(0) || *field == json!(0.0))
        });
    }
}

fn slim_object(object: &mut Value) {
    let Some(fields) = object.as_object_mut() else {
        return;
    };
    let healthy = ["watertight", "manifold", "windingConsistent", "outwardNormals"]
        .iter()
        .all(|key| fields.get(*key) == Some(&json!(true)))
        && fields.get("degenerateTriangles").is_none_or(|count| count == &json!(0));
    for key in ["triangles", "vertices", "surfaceArea", "sourceRange", "color"] {
        fields.remove(key);
    }
    if healthy && fields.contains_key("watertight") {
        for key in HEALTHY_MESH_FIELDS {
            fields.remove(*key);
        }
        fields.insert("meshOk".into(), json!(true));
    } else if fields.contains_key("watertight") && fields.get("complete") != Some(&json!(false)) {
        // An unfinished census (complete: false) is unknown, not bad.
        fields.insert("meshOk".into(), json!(false));
    }
    if fields.get("components") == Some(&json!(1)) {
        fields.remove("components");
    }
    if let Some(source) = fields.get_mut("source").filter(|source| source.is_object()) {
        *source = json!({"startLine": source["startLine"], "endLine": source["endLine"]});
    }
}

/// A pair record without the fields an agent does not need to act on it.
fn slim_pair(pair: &Value) -> Value {
    let lines = |source: &Value| {
        (!source.is_null()).then(|| json!([source["startLine"], source["endLine"]]))
    };
    let mut slim = serde_json::Map::new();
    for key in ["a", "b", "relation", "clearance"] {
        if pair.get(key).is_some_and(|value| !value.is_null()) {
            slim.insert(key.into(), pair[key].clone());
        }
    }
    if pair["relation"] == json!("contact") {
        slim.insert("contactAllowed".into(), pair["contactAllowed"].clone());
    }
    // An inexact distance (a budget-limited search proved the pair clear
    // with only an upper bound) says so on every pair, clean ones included:
    // `clearance` is then an upper bound and the true value lies in
    // [clearanceLowerBound, clearance].
    if pair["distanceExact"] == json!(false) {
        slim.insert("distanceExact".into(), json!(false));
        if pair["clearanceLowerBound"].is_number() && pair["clearanceLowerBound"] != pair["clearance"] {
            slim.insert("clearanceLowerBound".into(), pair["clearanceLowerBound"].clone());
        }
    }
    let problem = is_problem_pair(pair);
    if !problem {
        return Value::Object(slim);
    }
    slim.insert("toleranceViolated".into(), json!(true));
    if pair["determined"] == json!(false) {
        slim.insert("determined".into(), json!(false));
    }
    // Only when it bounds an inexact clearance from below.
    if pair["clearanceLowerBound"].is_number() && pair["clearanceLowerBound"] != pair["clearance"] {
        slim.insert("clearanceLowerBound".into(), pair["clearanceLowerBound"].clone());
    }
    // How the regions were sampled matters only when the sampling was cut short.
    if pair["clearanceSampling"]["truncated"] == json!(true) {
        slim.insert("clearanceSampling".into(), pair["clearanceSampling"].clone());
    }
    for key in [
        "aName",
        "bName",
        "skipped",
        "error",
        "depth",
        "depthMethod",
        "overlapVolume",
        "overlapBounds",
        "location",
        "overlapNote",
        "clearanceRegions",
    ] {
        if pair.get(key).is_some_and(|value| !value.is_null()) {
            slim.insert(key.into(), pair[key].clone());
        }
    }
    if pair["relation"] == json!("clearanceViolation") || pair["relation"] == json!("contact") {
        slim.insert("closestPoints".into(), pair["closestPoints"].clone());
    }
    if let Some(lines) = lines(&pair["aSource"]) {
        slim.insert("aLines".into(), lines);
    }
    if let Some(lines) = lines(&pair["bSource"]) {
        slim.insert("bLines".into(), lines);
    }
    Value::Object(slim)
}

/// Whether an `evaluate` call saves a revision first.
pub(crate) fn evaluate_saves(arguments: &Value) -> bool {
    ["code", "edits"].iter().any(|key| arguments.get(*key).is_some_and(|value| !value.is_null()))
}

/// The save an `evaluate` with `code` or `edits` makes before it evaluates:
/// `Ok(None)` when there is nothing to save, otherwise what was written, or
/// the reply that says why nothing was.
fn evaluate_save(state: &AppState, arguments: &Value) -> Result<Option<Result<Value, Value>>, String> {
    if !evaluate_saves(arguments) {
        return Ok(None);
    }
    let id = crate::argument_workspace_id(arguments)?;
    let mut input = json!({
        "expectedRevision": arguments.get("expectedRevision").cloned().unwrap_or(Value::Null),
        "revisionName": arguments.get("revisionName").cloned().unwrap_or(Value::Null),
        "source": "ai",
    });
    for key in ["code", "edits"] {
        if let Some(value) = arguments.get(key).filter(|value| !value.is_null()) {
            input[key] = value.clone();
        }
    }
    match crate::mcp_store_update(state, id, input) {
        Ok(result) => Ok(Some(Ok(json!({
            "revision": result.workspace.revision,
            "name": result.workspace.revision_name,
            "ranges": result.events.iter().map(|event| json!({
                "start": event.start,
                "removedLength": event.end - event.start,
                "insertedLength": event.inserted_length,
            })).collect::<Vec<_>>(),
        })))),
        Err(error) => crate::mcp_update_failure(error).map(|reply| Some(Err(reply))),
    }
}

/// `evaluate` over MCP: the result, plus the image as an image content block
/// (and not repeated as base64 in the text) when one was asked for.
pub(crate) fn mcp_evaluate(state: &AppState, arguments: &Value) -> Value {
    let saved = match evaluate_save(state, arguments) {
        Ok(None) => None,
        Ok(Some(Ok(saved))) => Some(saved),
        // Source that does not compile: nothing saved, nothing to evaluate.
        Ok(Some(Err(rejected))) => {
            return json!({
                "content": [{"type": "text", "text": serde_json::to_string(&rejected).unwrap_or_default()}],
                "structuredContent": rejected,
                "isError": false
            })
        }
        Err(error) => return crate::mcp_tool_error(&error),
    };
    let pinned = saved.as_ref().and_then(|saved| saved["revision"].as_u64());
    match evaluate_revision(state, None, arguments_without_save(arguments).as_ref().unwrap_or(arguments), pinned) {
        Ok((png, mut value)) => {
            slim_analysis(&mut value, arguments);
            if let Some(saved) = saved {
                value["saved"] = saved;
            }
            let mut content = Vec::new();
            if let Some(png) = &png {
                if let Some(image) = value["image"].as_object_mut() {
                    image.remove("base64");
                }
                content.push(json!({"type": "image", "data": base64_encode(png), "mimeType": "image/png"}));
            }
            content.push(json!({"type": "text", "text": serde_json::to_string(&value).unwrap_or_default()}));
            json!({"content": content, "structuredContent": value, "isError": false})
        }
        // The save stands: what an agent reads next must say so.
        Err(error) => {
            let message = match &saved {
                Some(saved) => format!(
                    "Saved as revision {} ({}), but evaluating it failed: {}",
                    saved["revision"],
                    saved["name"].as_str().unwrap_or_default(),
                    error.message
                ),
                None => error.message,
            };
            let mut structured = json!({"ok": false, "error": message});
            if let Some(saved) = saved {
                structured["saved"] = saved;
            }
            json!({
                "content": [{"type": "text", "text": message}],
                "structuredContent": structured,
                "isError": true
            })
        }
    }
}

/// `arguments` for the evaluation after the save: the source just saved must
/// not be read as source to analyse.
fn arguments_without_save(arguments: &Value) -> Option<Value> {
    if !evaluate_saves(arguments) {
        return None;
    }
    let mut rest = arguments.clone();
    let fields = rest.as_object_mut()?;
    for key in ["code", "edits", "revisionName", "expectedRevision"] {
        fields.remove(key);
    }
    Some(rest)
}

// ---------------------------------------------------------------------------
// revisions
// ---------------------------------------------------------------------------

/// Most revisions one `list_revisions` call returns by default.
const DEFAULT_REVISION_LIST: usize = 50;

/// Every retained revision of `workspace` as `{revision, name, source,
/// timestamp, current?}`, newest first. Stored without a name it is labelled
/// by number, and its author comes from the edit log while that remembers it.
pub(crate) fn revision_rows(state: &AppState, workspace: &Workspace) -> Result<Vec<Value>, ApiError> {
    let history = state
        .workspaces
        .revision_list(&workspace.id)
        .map_err(|error| ApiError::new(500, error))?;
    let mut rows = Vec::with_capacity(history.len() + 1);
    let current_source = crate::superseded_source(workspace);
    rows.push(json!({
        "revision": workspace.revision,
        "name": crate::revision_label(&workspace.revision_name, workspace.revision),
        "source": crate::revision_author(&current_source),
        "timestamp": revision_written_at(workspace),
        "current": true,
    }));
    for info in history.iter().rev().filter(|info| info.revision < workspace.revision) {
        let source = if info.source.is_empty() {
            crate::event_source(workspace, info.revision).unwrap_or_default()
        } else {
            info.source.clone()
        };
        rows.push(json!({
            "revision": info.revision,
            "name": crate::revision_label(&info.name, info.revision),
            "source": crate::revision_author(&source),
            "timestamp": info.timestamp,
        }));
    }
    Ok(rows)
}

/// `list_revisions`: compact and paged, newest first.
pub(crate) fn mcp_list_revisions(state: &AppState, input: &Value) -> Result<Value, ApiError> {
    let id = crate::input_design_id(input).ok_or_else(|| ApiError::bad("designId is required."))?;
    let workspace = load_workspace(state, id)?;
    let rows = revision_rows(state, &workspace)?;
    let total = rows.len();
    let limit = input.get("limit").and_then(Value::as_u64).map_or(DEFAULT_REVISION_LIST, |limit| limit as usize).max(1);
    let before = input.get("before").and_then(Value::as_u64);
    let listed: Vec<Value> = rows
        .into_iter()
        .filter(|row| before.is_none_or(|before| row["revision"].as_u64().is_some_and(|revision| revision < before)))
        .take(limit)
        .collect();
    Ok(json!({
        "ok": true,
        "designId": id,
        "current": workspace.revision,
        "total": total,
        "revisions": listed,
    }))
}

// ---------------------------------------------------------------------------
// parameters
// ---------------------------------------------------------------------------

pub(crate) fn parameters_value(
    state: &AppState,
    id: Option<&str>,
    input: &Value,
) -> Result<Value, ApiError> {
    let source = eval_source(state, id, input)?;
    let mut value = source.header();
    value["ok"] = json!(true);
    let (parameters, total) =
        scadsource::parameters_capped(&source.base, scadsource::MAX_PARAMETERS);
    let truncated = parameters.len() < total;
    value["parameters"] = json!(parameters);
    value["total"] = json!(total);
    if truncated {
        value["truncated"] = json!(true);
        value["truncatedNote"] = json!(format!(
            "Only the first {} of {total} top-level assignments are listed; read the source for the rest.",
            scadsource::MAX_PARAMETERS
        ));
    }
    value["note"] = json!("Top-level assignments in source order. set_parameters rewrites their values as a new revision.");
    Ok(value)
}

/// `list_modules`: the modules and functions of a source with their line
/// ranges and leading comment, plus the file's banner comments, so an agent
/// can find "the hinge" and read only those lines. Text work: nothing is
/// compiled. `grep` keeps definitions whose name or comment contains it.
pub(crate) fn modules_value(state: &AppState, input: &Value) -> Result<Value, ApiError> {
    let source = eval_source(state, None, input)?;
    let (definitions, sections) = scadsource::definitions(&source.base);
    let grep = input
        .get("grep")
        .and_then(Value::as_str)
        .map(str::to_lowercase)
        .filter(|grep| !grep.is_empty());
    let total = definitions.len();
    let listed: Vec<Value> = definitions
        .iter()
        .filter(|definition| {
            grep.as_ref().is_none_or(|grep| {
                definition.name.to_lowercase().contains(grep.as_str())
                    || definition
                        .comment
                        .as_ref()
                        .is_some_and(|comment| comment.to_lowercase().contains(grep.as_str()))
            })
        })
        .map(|definition| {
            let mut row = json!({
                "kind": definition.kind,
                "name": definition.name,
                "params": definition.parameters,
                "lines": [definition.start_line, definition.end_line],
            });
            if definition.depth > 0 {
                row["nested"] = json!(true);
            }
            if let Some(comment) = &definition.comment {
                row["comment"] = json!(comment);
            }
            row
        })
        .collect();
    let mut value = source.header();
    value["ok"] = json!(true);
    value["totalLines"] = json!(source.base.split('\n').count());
    value["sections"] = json!(sections
        .iter()
        .map(|section| json!({"line": section.line, "title": section.title}))
        .collect::<Vec<_>>());
    value["definitions"] = json!(listed);
    value["total"] = json!(total);
    value["hint"] = json!("Read a definition with get_design_code startLine/endLine (its lines), or grep for a name.");
    Ok(value)
}

/// Longest value or expression the compact parameter listing prints verbatim.
const COMPACT_PARAMETER_CHARS: usize = 200;

/// A short stand-in for a value or expression too long to list.
fn elide_parameter(value: &Value, expression: &str) -> Value {
    match value {
        Value::Array(items) => json!(format!(
            "<list, {} items, {} chars>",
            items.len(),
            expression.len()
        )),
        Value::String(text) => json!(format!("<string, {} chars>", text.chars().count())),
        _ => json!(format!("<expression, {} chars>", expression.len())),
    }
}

/// `get_parameters` for MCP: the agent-facing listing.
///
/// By default each parameter is `{name, line, value | expression, comment}`:
/// `expression` only when it is not a literal (a literal's expression is its
/// value again), byte offsets and occurrence counts dropped, and anything over
/// [`COMPACT_PARAMETER_CHARS`] elided to its kind and size (font outlines and
/// point lists dominate a listing otherwise). `names` and `filter`
/// (case-insensitive substring of the name) select parameters; `verbose: true`
/// returns the full REST records.
pub(crate) fn compact_parameters(mut value: Value, input: &Value) -> Value {
    let verbose = input.get("verbose").and_then(Value::as_bool) == Some(true);
    let names: Option<Vec<&str>> = input
        .get("names")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect());
    let filter = input
        .get("filter")
        .and_then(Value::as_str)
        .map(str::to_lowercase)
        .filter(|filter| !filter.is_empty());
    let Some(parameters) = value.get_mut("parameters").and_then(Value::as_array_mut) else {
        return value;
    };
    parameters.retain(|parameter| {
        let name = parameter["name"].as_str().unwrap_or_default();
        names.as_ref().is_none_or(|names| names.contains(&name))
            && filter
                .as_ref()
                .is_none_or(|filter| name.to_lowercase().contains(filter.as_str()))
    });
    let mut elided = 0usize;
    if !verbose {
        for parameter in parameters.iter_mut() {
            let expression = parameter["expression"].as_str().unwrap_or_default().to_string();
            let literal = parameter["literal"].as_bool() == Some(true);
            let mut compact = serde_json::Map::new();
            compact.insert("name".into(), parameter["name"].take());
            compact.insert("line".into(), parameter["line"].take());
            if literal {
                let value = parameter["value"].take();
                if expression.len() > COMPACT_PARAMETER_CHARS {
                    elided += 1;
                    compact.insert("value".into(), elide_parameter(&value, &expression));
                } else {
                    compact.insert("value".into(), value);
                }
            } else if expression.len() > COMPACT_PARAMETER_CHARS {
                elided += 1;
                compact.insert("expression".into(), elide_parameter(&Value::Null, &expression));
            } else {
                compact.insert("expression".into(), json!(expression));
            }
            if parameter["comment"].is_string() {
                compact.insert("comment".into(), parameter["comment"].take());
            }
            if parameter["occurrences"].as_u64().is_some_and(|count| count > 1) {
                compact.insert("occurrences".into(), parameter["occurrences"].take());
            }
            *parameter = Value::Object(compact);
        }
    }
    let listed = parameters.len();
    value["listed"] = json!(listed);
    if elided > 0 {
        value["elided"] = json!(elided);
        value["elidedNote"] = json!(format!(
            "{elided} value(s) over {COMPACT_PARAMETER_CHARS} chars are summarised; read them with get_design_code (startLine/endLine) or pass verbose: true."
        ));
    }
    if let Some(object) = value.as_object_mut() {
        object.remove("note");
    }
    value
}

/// Parsed `values` of a set-parameters request.
fn parameter_values(input: &Value) -> Result<Vec<(String, String)>, ApiError> {
    let values = input
        .get("values")
        .or_else(|| input.get("parameters"))
        .ok_or_else(|| ApiError::bad("values is required: {\"name\": value, ...}."))?;
    let parsed = scadsource::parse_overrides(Some(values)).map_err(ApiError::bad)?;
    if parsed.is_empty() {
        return Err(ApiError::bad("values must name at least one parameter."));
    }
    Ok(parsed)
}

/// The byte-range edit a `set_parameters` call makes to `workspace`.
pub(crate) fn set_parameters_edit(
    workspace: &Workspace,
    input: &Value,
) -> Result<(usize, usize, String, Vec<Value>), ApiError> {
    let values = parameter_values(input)?;
    let rewritten =
        scadsource::apply_overrides(&workspace.code, &values, false).map_err(ApiError::bad)?;
    let (start, end, text) = scadsource::minimal_edit(&workspace.code, &rewritten.source);
    Ok((start, end, text, rewritten.applied))
}

/// Resolves the patch forms of a workspace update — `startLine`/`endLine` +
/// `text`, or `edits` — into a plain byte-range edit against the current
/// source, written back into `input` as `start`/`end`/`text`. Inputs that
/// use none of these forms pass through untouched.
pub(crate) fn resolve_patch(
    state: &AppState,
    id: &str,
    input: &mut Value,
) -> Result<(), UpdateError> {
    let has_lines = input.get("startLine").is_some_and(|value| !value.is_null());
    let has_edits = input.get("edits").is_some_and(|value| !value.is_null());
    let has_values = input.get("values").is_some_and(|value| !value.is_null());
    if !has_lines && !has_edits && !has_values {
        return Ok(());
    }
    let forms = [
        input.get("code").is_some_and(|value| !value.is_null()),
        input.get("start").is_some_and(|value| !value.is_null()),
        has_lines,
        has_edits,
        has_values,
    ];
    if forms.iter().filter(|form| **form).count() > 1 {
        return Err(UpdateError::Invalid(
            "Provide exactly one of: code, start/end/text, startLine/endLine/text, edits, or values.".into(),
        ));
    }
    let expected = input
        .get("expectedRevision")
        .or_else(|| input.get("baseRevision"))
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            UpdateError::Invalid(
                "baseRevision or expectedRevision is required for every update.".into(),
            )
        })?;
    let workspace = state
        .workspaces
        .get(id)
        .map_err(UpdateError::Internal)?
        .ok_or(UpdateError::NotFound)?;
    if workspace.revision != expected {
        return Err(UpdateError::Conflict(workspace));
    }
    let (start, end, text) = if has_values {
        let (start, end, text, _) = set_parameters_edit(&workspace, input)
            .map_err(|error| UpdateError::Invalid(error.message))?;
        (start, end, text)
    } else {
        let rewritten = if has_lines {
            let start_line = input
                .get("startLine")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    UpdateError::Invalid("startLine must be a 1-based line number.".into())
                })? as usize;
            let end_line = match input.get("endLine") {
                None | Some(Value::Null) => start_line,
                Some(value) => value
                    .as_u64()
                    .ok_or_else(|| UpdateError::Invalid("endLine must be a line number.".into()))?
                    as usize,
            };
            let text = input.get("text").and_then(Value::as_str).ok_or_else(|| {
                UpdateError::Invalid(
                    "text is required with startLine (\"\" deletes the lines).".into(),
                )
            })?;
            scadsource::replace_lines(&workspace.code, start_line, end_line, text)
                .map_err(UpdateError::Invalid)?
        } else {
            scadsource::search_replace(&workspace.code, &input["edits"])
                .map_err(UpdateError::Invalid)?
        };
        scadsource::minimal_edit(&workspace.code, &rewritten)
    };
    if let Some(object) = input.as_object_mut() {
        for key in ["startLine", "endLine", "edits", "values", "code"] {
            object.remove(key);
        }
        object.insert("start".into(), json!(start));
        object.insert("end".into(), json!(end));
        object.insert("text".into(), json!(text));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// Actions served by [`handle_agent_route`] under `/api/designs/{id}/` (alias
/// `/api/workspaces/{id}/`).
pub(crate) const AGENT_ACTIONS: &[&str] = &[
    "compare",
    "objects",
    "evaluate",
    "measure",
    "image",
    "diff",
    "intersections",
    "parameters",
    "print-layout",
];

/// The actions a browser asks about the editor's text rather than the stored
/// revision: a person's own check, with `code` in the body, on a design or
/// (a read-only view has no design id) as stateless `/api/{action}`. Every other
/// analysis reads the current revision, and the stateless forms are gone.
pub(crate) const BROWSER_CODE_ACTIONS: &[&str] = &["intersections"];

/// Whether an agent action compiles (and so needs a queue permit and the
/// evaluator rate bucket). Listing parameters and revisions is text work.
pub(crate) fn agent_action_runs_evaluator(
    method: &str,
    action: &str,
    workspace_route: bool,
) -> bool {
    match action {
        "parameters" => method == "POST" && workspace_route,
        // Restoring validates the source and writes a revision; listing reads.
        "revisions" => method == "POST",
        _ => AGENT_ACTIONS.contains(&action),
    }
}

fn respond_api_error(stream: &mut TcpStream, error: ApiError) -> io::Result<()> {
    respond_error(stream, error.status, &error.message)
}

pub(crate) fn handle_agent_route(
    stream: &mut TcpStream,
    state: &AppState,
    request: &Request,
    id: Option<&str>,
    action: &str,
) -> io::Result<()> {
    let mut input = match parse_json_body(&request.body) {
        Ok(Value::Object(object)) => Value::Object(object),
        Ok(_) => return respond_error(stream, 400, "The request body must be a JSON object."),
        Err(error) => return respond_error(stream, 400, &error),
    };
    // GET forms take their few scalar arguments from the query string.
    for (key, value) in query_parameters(&request.query) {
        if input.get(&key).is_some() {
            continue;
        }
        let parsed = match key.as_str() {
            "revision" | "fromRevision" | "toRevision" => {
                value.parse::<u64>().map(Value::from).ok()
            }
            "objectIds" | "hiddenObjectIds" | "visibleObjectIds" => Some(json!(value
                .split(',')
                .filter(|id| !id.is_empty())
                .collect::<Vec<_>>())),
            "view" | "projection" | "plate" => Some(json!(value)),
            "width" | "height" => value.parse::<u64>().map(Value::from).ok(),
            "zoom" | "tolerance" | "spacing" => value.parse::<f64>().map(Value::from).ok(),
            _ => None,
        };
        if let Some(parsed) = parsed {
            input[key.as_str()] = parsed;
        }
    }
    if let Err(message) = rest_removed_argument(action, &input) {
        return respond_error(stream, 400, &message);
    }
    if action == "revisions" {
        let Some(id) = id else {
            return respond_error(stream, 404, "Not found");
        };
        if request.method == "POST" {
            return handle_restore_revision(stream, state, request, id, input);
        }
        return match load_workspace(state, id).and_then(|workspace| revisions_public(state, &workspace)) {
            Ok(value) => respond_value(stream, 200, &value),
            Err(error) => respond_api_error(stream, error),
        };
    }
    if action == "parameters" && request.method == "POST" {
        return match id {
            None => respond_error(stream, 404, "Not found"),
            Some(id) => {
                let _permit = match state.render_queue.acquire(current_client_key(), None, None) {
                    Ok(permit) => permit,
                    Err(rejection) => return respond_queue_rejection(stream, rejection),
                };
                if input.get("values").is_none() {
                    return respond_error(
                        stream,
                        400,
                        "values is required: {\"name\": value, ...}.",
                    );
                }
                // The browser never calls this route, so an HTTP caller is an agent:
                // its edit is highlighted as AI unless it says otherwise.
                let source = match input.get("source").and_then(Value::as_str) {
                    Some(source) => json!(source),
                    None => json!("ai"),
                };
                input["source"] = source;
                if let Err(error) = resolve_patch(state, id, &mut input) {
                    return respond_update_error(stream, state, request, error);
                }
                let update = match serde_json::from_value::<WorkspaceUpdate>(input) {
                    Ok(update) => update,
                    Err(error) => {
                        return respond_error(stream, 400, &format!("Invalid update: {error}"))
                    }
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
                    Err(error) => respond_update_error(stream, state, request, error),
                }
            }
        };
    }
    if action == "parameters" {
        return match parameters_value(state, id, &input) {
            Ok(value) => respond_value(stream, 200, &value),
            Err(error) => respond_api_error(stream, error),
        };
    }
    let _permit = match state.render_queue.acquire(current_client_key(), None, None) {
        Ok(permit) => permit,
        Err(rejection) => return respond_queue_rejection(stream, rejection),
    };
    let result = match action {
        "objects" => objects_value(state, id, &input),
        "measure" => measure_value(state, id, &input),
        "diff" => diff_value(state, id, &input),
        "intersections" => intersections_value(state, id, &input),
        "print-layout" => crate::printlayout::layout_value(state, id, &input),
        "compare" => match id {
            Some(id) => crate::surfacecompare::browser_compare(state, id, &input)
                .map_err(|(status, message)| ApiError { status, message }),
            None => return respond_error(stream, 404, "Not found"),
        },
        "evaluate" => {
            let raw = input.get("response").and_then(Value::as_str) == Some("png");
            return match evaluate_value(state, id, &input) {
                Ok((Some(png), _)) if raw => respond_text(stream, 200, "image/png", &png, &[]),
                Ok((_, value)) => respond_value(stream, 200, &value),
                Err(error) => respond_api_error(stream, error),
            };
        }
        "image" => {
            let raw = input.get("response").and_then(Value::as_str) == Some("png")
                || query_parameters(&request.query)
                    .get("raw")
                    .is_some_and(|value| value == "1" || value == "true");
            return match image_value(state, id, &input) {
                Ok((png, _)) if raw => respond_text(stream, 200, "image/png", &png, &[]),
                Ok((png, mut meta)) => {
                    meta["image"] = json!({"mimeType": "image/png", "base64": base64_encode(&png)});
                    respond_value(stream, 200, &meta)
                }
                Err(error) => respond_api_error(stream, error),
            };
        }
        _ => return respond_error(stream, 404, "Design endpoint not found."),
    };
    match result {
        Ok(value) => respond_value(stream, 200, &value),
        Err(error) => respond_api_error(stream, error),
    }
}

/// The agent routes read the current revision too, so they refuse what the MCP
/// tools refuse. Only [`BROWSER_CODE_ACTIONS`] take `code`.
fn rest_removed_argument(action: &str, input: &Value) -> Result<(), String> {
    let given = |key: &str| input.get(key).is_some_and(|value| !value.is_null());
    // Saving first is the MCP tool's form; here a write is its own request.
    if action == "evaluate" && (given("code") || given("edits")) {
        return Err("POST .../evaluate reads the current revision. Save first with PATCH /api/designs/{id} (code or edits, revisionName, expectedRevision), then evaluate.".into());
    }
    for key in crate::REMOVED_ARGUMENTS {
        if !input.get(key).is_some_and(|value| !value.is_null()) {
            continue;
        }
        if key == "code" && BROWSER_CODE_ACTIONS.contains(&action) {
            continue;
        }
        if key == "revision" && action == "revisions" {
            continue;
        }
        return Err(crate::removed_argument_error(action, key));
    }
    Ok(())
}

/// The retained history as `GET .../revisions` and the Revisions panel read it.
pub(crate) fn revisions_public(state: &AppState, workspace: &Workspace) -> Result<Value, ApiError> {
    Ok(json!({
        "ok": true,
        "current": workspace.revision,
        "limit": MAX_REVISION_HISTORY,
        "revisions": revision_rows(state, workspace)?,
    }))
}

/// Why a restore did not land: the revision to copy, or the write of the copy.
pub(crate) enum RestoreFailure {
    Source(ApiError),
    Update(UpdateError),
}

/// Restoring, for `restore_revision` and the browser's button alike: the old
/// revision's source becomes a new named revision on top, validated, retained
/// and conflict-checked like any write; history is never rewritten. The caller
/// holds an evaluator permit and the design's assets. `source` is who asked.
pub(crate) fn restore_revision(state: &AppState, id: &str, input: &Value, source: &str) -> Result<(u64, EditResult), RestoreFailure> {
    let revision = input
        .get("revision")
        .and_then(Value::as_u64)
        .ok_or_else(|| RestoreFailure::Source(ApiError::bad("revision is required: the retained revision to copy.")))?;
    let name = input
        .get("revisionName")
        .and_then(Value::as_str)
        .ok_or_else(crate::missing_revision_name)
        .and_then(crate::validate_revision_name)
        .map_err(|error| RestoreFailure::Update(UpdateError::Invalid(error)))?;
    let workspace = load_workspace(state, id).map_err(RestoreFailure::Source)?;
    let code = revision_source(state, &workspace, revision).map_err(RestoreFailure::Source)?;
    let update = serde_json::from_value::<WorkspaceUpdate>(json!({
        "expectedRevision": input.get("expectedRevision").cloned().unwrap_or(Value::Null),
        "code": code,
        "revisionName": name,
        "source": source,
    }))
    .map_err(|error| RestoreFailure::Update(UpdateError::Invalid(format!("Invalid update: {error}"))))?;
    let result = state.workspaces.update(id, update).map_err(RestoreFailure::Update)?;
    Ok((revision, result))
}

/// `POST /api/designs/{id}/revisions` `{revision, revisionName, expectedRevision}`:
/// the browser's Restore. The design id in the path is the capability to write,
/// so a read-only link has no such route.
fn handle_restore_revision(
    stream: &mut TcpStream,
    state: &AppState,
    request: &Request,
    id: &str,
    input: Value,
) -> io::Result<()> {
    let _permit = match state.render_queue.acquire(current_client_key(), None, None) {
        Ok(permit) => permit,
        Err(rejection) => return respond_queue_rejection(stream, rejection),
    };
    match assetstore::with_design_assets(state, Some(id), || restore_revision(state, id, &input, "browser")) {
        Ok((_, result)) => respond_workspace(stream, 200, state, request, &result.workspace, &result.events),
        Err(RestoreFailure::Source(error)) => respond_api_error(stream, error),
        Err(RestoreFailure::Update(error)) => respond_update_error(stream, state, request, error),
    }
}

pub(crate) fn respond_update_error(
    stream: &mut TcpStream,
    state: &AppState,
    request: &Request,
    error: UpdateError,
) -> io::Result<()> {
    match error {
        UpdateError::NotFound => respond_error(stream, 404, "Design not found."),
        UpdateError::Conflict(workspace) => respond_value(
            stream,
            409,
            &json!({
                "ok": false,
                "error": "Design revision conflict.",
                "workspace": workspace_public(&workspace, state, request, &[], None)
            }),
        ),
        UpdateError::Invalid(error) => respond_error(stream, 400, &error),
        UpdateError::Validation(error) => respond_validation_error(stream, error),
        UpdateError::Internal(error) => respond_error(stream, 500, &error),
    }
}

// ---------------------------------------------------------------------------
// MCP
// ---------------------------------------------------------------------------

pub(crate) fn mcp_result(result: Result<Value, ApiError>) -> Result<Value, String> {
    result.map_err(|error| error.message)
}

/// `render_image` returns an image content block plus the metadata as text,
/// so a model sees the picture directly.
pub(crate) fn mcp_render_image(state: &AppState, arguments: &Value) -> Value {
    match image_value(state, None, arguments) {
        Ok((png, meta)) => json!({
            "content": [
                {"type": "image", "data": base64_encode(&png), "mimeType": "image/png"},
                {"type": "text", "text": serde_json::to_string(&meta).unwrap_or_default()}
            ],
            "structuredContent": meta,
            "isError": false
        }),
        Err(error) => json!({
            "content": [{"type": "text", "text": error.message}],
            "structuredContent": {"ok": false, "error": error.message},
            "isError": true
        }),
    }
}

pub(crate) fn mcp_create_share_link(
    state: &AppState,
    request: &Request,
    arguments: &Value,
) -> Result<Value, String> {
    let id = argument_workspace_id(arguments)?;
    let token = state
        .workspaces
        .share_token(id)
        .map_err(|error| match error {
            UpdateError::NotFound => "Design not found.".to_string(),
            UpdateError::Internal(message) => message,
            _ => "Could not create a read-only link.".to_string(),
        })?;
    Ok(json!({
        "ok": true,
        "designId": id,
        "workspaceId": id,
        "shareToken": token,
        "url": shared_url(state, request, &token),
        "readOnly": true,
        "note": "Anyone with this link can view the model and its source but cannot edit it, and it does not reveal the editable design link. Calling this again returns the same link."
    }))
}

pub(crate) fn shared_url(state: &AppState, request: &Request, token: &str) -> String {
    format!("{}/shared/{token}", request_base_url(state, request))
}

pub(crate) fn mcp_set_parameters(
    state: &AppState,
    request: &Request,
    arguments: &Value,
) -> Result<Value, String> {
    let id = argument_workspace_id(arguments)?.to_string();
    let mut input = arguments.clone();
    if input.get("values").is_none() {
        return Err("values is required: {\"name\": value, ...}.".into());
    }
    if let Some(object) = input.as_object_mut() {
        object.remove("workspaceId");
        object.insert("source".into(), json!("ai"));
    }
    let applied = {
        let workspace = state.workspaces.get(&id)?.ok_or("Design not found.")?;
        set_parameters_edit(&workspace, &input)
            .map(|(_, _, _, applied)| applied)
            .map_err(|error| error.message)?
    };
    let mut value = crate::mcp_apply_update(state, request, &id, input)?;
    value["applied"] = json!(applied);
    Ok(value)
}

// ---------------------------------------------------------------------------
// Geometry understanding: describe_geometry, probe, section_profile
// ---------------------------------------------------------------------------

/// Most objects one describe_geometry call analyses (feature fitting is the
/// expensive part); pass objectIds to pick.
const MAX_DESCRIBE_OBJECTS: usize = 16;
/// Most rays and most points one probe call answers (each).
pub(crate) const MAX_PROBES: usize = 256;
/// Most objects one probe or section_profile call indexes.
const MAX_QUERY_OBJECTS: usize = 64;
/// section_profile point budget: default and ceiling.
const DEFAULT_SECTION_POINTS: usize = 2000;
const MAX_SECTION_POINTS: usize = 20_000;
const DEFAULT_SECTION_TOLERANCE: f64 = 0.05;

/// Rounds every non-integer number in `value` to `decimals` places, so
/// geometry replies stay compact (0.1 µm is far below print resolution).
fn round_json(value: &mut Value, decimals: i32) {
    match value {
        Value::Number(number) if !number.is_i64() && !number.is_u64() => {
            if let Some(rounded) = number
                .as_f64()
                .map(|value| round_to(value, decimals))
                .and_then(serde_json::Number::from_f64)
            {
                *number = rounded;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| round_json(item, decimals)),
        Value::Object(fields) => fields.values_mut().for_each(|field| round_json(field, decimals)),
        _ => {}
    }
}

fn optional_bool(input: &Value, key: &str, default: bool) -> Result<bool, ApiError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(ApiError::bad(format!("{key} must be true or false."))),
    }
}

fn vec3_input(value: &Value, what: &str) -> Result<P, ApiError> {
    let items = value
        .as_array()
        .filter(|items| items.len() == 3)
        .ok_or_else(|| ApiError::bad(format!("{what} must be [x, y, z].")))?;
    let mut point = [0.0; 3];
    for (slot, item) in point.iter_mut().zip(items) {
        *slot = item
            .as_f64()
            .filter(|value| value.is_finite())
            .ok_or_else(|| ApiError::bad(format!("{what} must hold three finite numbers.")))?;
    }
    Ok(point)
}

fn unit3(vector: P, what: &str) -> Result<P, ApiError> {
    let length = (vector[0] * vector[0] + vector[1] * vector[1] + vector[2] * vector[2]).sqrt();
    if !(length > 1e-12) {
        return Err(ApiError::bad(format!("{what} must be a non-zero vector.")));
    }
    Ok(vector.map(|component| component / length))
}

fn rounded3(point: P) -> [f64; 3] {
    point.map(|value| round_to(value, 4))
}

/// Compiles the source and picks the objects (`objectIds` / `hiddenObjectIds`),
/// refusing more than `limit` of them.
fn query_parts<'a>(
    compiled: &'a engine::MultipartCompileOutput,
    input: &Value,
    limit: usize,
    tool: &str,
) -> Result<Vec<&'a engine::CompiledPart>, ApiError> {
    let selected = select_parts(&compiled.parts, input, "objectIds")?;
    if selected.len() > limit {
        return Err(ApiError::new(
            422,
            format!(
                "{} objects exceed the {limit}-object limit of {tool}; pass objectIds to pick some.",
                selected.len()
            ),
        ));
    }
    Ok(selected)
}

fn part_identity(part: &engine::CompiledPart, source: &EvalSource) -> Value {
    let location = source.source_location(part.source_range);
    json!({
        "id": part.id,
        "name": part.name,
        "lines": (!location.is_null()).then(|| json!([location["startLine"], location["endLine"]])),
    })
}

/// `describe_geometry`: what each object is and how it sits — size and
/// probable units, principal axes with an aligning `multmatrix`, lay-flat
/// candidates, mirror symmetry and fitted planes/cylinders/spheres.
pub(crate) fn describe_geometry_value(
    state: &AppState,
    id: Option<&str>,
    input: &Value,
) -> Result<Value, ApiError> {
    let started = Instant::now();
    let (_, _deadline_scope) = request_deadline(started);
    let source = eval_source(state, id, input)?;
    let features = optional_bool(input, "features", true)?;
    let within = crate::scancommon::Within::parse(input).map_err(ApiError::bad)?;
    let compiled = compile_eval(&source)?;
    let selected = query_parts(&compiled, input, MAX_DESCRIBE_OBJECTS, "describe_geometry")?;
    let options = crate::meshanalysis::DescribeOptions {
        features,
        ..Default::default()
    };
    let objects: Vec<Value> = selected
        .iter()
        .map(|part| {
            let mut mesh = crate::meshanalysis::from_engine_mesh(&part.mesh);
            let mut value = part_identity(part, &source);
            if let Some(within) = &within {
                let stats = within.stats(&mesh);
                value["within"] = stats.public();
                if stats.triangles == 0 {
                    value["nothingInside"] = json!(true);
                    return value;
                }
                mesh = within.crop(&mesh);
            }
            let description = crate::meshanalysis::describe(&mesh, options);
            merge_fields(&mut value, serde_json::to_value(&description).unwrap_or(Value::Null));
            round_json(&mut value, 4);
            value
        })
        .collect();
    let mut value = source.header();
    value["ok"] = json!(true);
    if let Some(within) = &within {
        value["within"] = within.public(json!({}));
        if objects.iter().all(|object| object.get("nothingInside").is_some()) {
            value["nothingInside"] = json!(true);
            value["note"] = json!("Nothing inside the box: no selected object has surface within it.");
        }
    }
    value["objects"] = json!(objects);
    attach_diagnostics(&mut value, &compiled.messages, started);
    Ok(value)
}

/// `probe`: exact ray casts and closest-surface queries against the
/// selected objects (nearest across all of them).
pub(crate) fn probe_value(state: &AppState, id: Option<&str>, input: &Value) -> Result<Value, ApiError> {
    let started = Instant::now();
    let (_, _deadline_scope) = request_deadline(started);
    let source = eval_source(state, id, input)?;
    let list = |key: &str| -> Result<Vec<Value>, ApiError> {
        match input.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) if items.len() <= MAX_PROBES => Ok(items.clone()),
            Some(Value::Array(_)) => Err(ApiError::bad(format!("{key} may hold at most {MAX_PROBES} entries."))),
            Some(_) => Err(ApiError::bad(format!("{key} must be an array."))),
        }
    };
    let rays: Vec<(P, P)> = list("rays")?
        .iter()
        .map(|ray| {
            let origin = vec3_input(&ray["origin"], "rays[].origin")?;
            let direction = unit3(vec3_input(&ray["direction"], "rays[].direction")?, "rays[].direction")?;
            Ok((origin, direction))
        })
        .collect::<Result<_, ApiError>>()?;
    let points: Vec<P> = list("points")?
        .iter()
        .map(|point| vec3_input(point, "points[]"))
        .collect::<Result<_, ApiError>>()?;
    if rays.is_empty() && points.is_empty() {
        return Err(ApiError::bad("Provide rays ([{origin, direction}]) and/or points ([[x, y, z]])."));
    }
    let compiled = compile_eval(&source)?;
    let selected = query_parts(&compiled, input, MAX_QUERY_OBJECTS, "probe")?;
    let indexes: Vec<(&engine::CompiledPart, MeshIndex)> = selected
        .iter()
        .map(|part| (*part, MeshIndex::from_mesh(&part.mesh)))
        .collect();
    let all_hits = optional_bool(input, "allHits", false)?;
    let max_distance = match input.get("maxDistance") {
        None | Some(Value::Null) => f64::INFINITY,
        Some(value) => value
            .as_f64()
            .filter(|value| value.is_finite() && *value > 0.0)
            .ok_or_else(|| ApiError::bad("maxDistance must be a positive number of mm."))?,
    };
    let every_hit_indexes: Vec<(&str, crate::meshanalysis::MeshIndex)> = if all_hits {
        selected
            .iter()
            .map(|part| (part.id.as_str(), crate::meshanalysis::MeshIndex::new(&crate::meshanalysis::from_engine_mesh(&part.mesh))))
            .collect()
    } else {
        Vec::new()
    };
    let ray_results: Vec<Value> = rays
        .iter()
        .map(|&(origin, direction)| {
            if all_hits {
                return every_hit(&every_hit_indexes, origin, direction, max_distance);
            }
            let best = indexes
                .iter()
                .filter_map(|(part, index)| {
                    index
                        .ray_first_hit(origin, direction, 1e-9, f64::INFINITY, None)
                        .map(|(t, triangle)| (t, *part, analysis::triangle_normal(&index.triangles[triangle])))
                })
                .min_by(|x, y| x.0.total_cmp(&y.0));
            match best {
                Some((t, part, normal)) => {
                    let point = [0, 1, 2].map(|axis| origin[axis] + direction[axis] * t);
                    let facing = normal[0] * direction[0] + normal[1] * direction[1] + normal[2] * direction[2];
                    json!({
                        "hit": true,
                        "objectId": part.id,
                        "point": rounded3(point),
                        "normal": rounded3(normal),
                        "distance": round_to(t, 4),
                        "entering": facing < 0.0,
                    })
                }
                None => json!({"hit": false}),
            }
        })
        .collect();
    let point_results: Vec<Value> = points
        .iter()
        .map(|&point| {
            let inside: Vec<&str> = indexes
                .iter()
                .filter(|(_, index)| index.contains(point))
                .map(|(part, _)| part.id.as_str())
                .collect();
            let best = indexes
                .iter()
                .filter_map(|(part, index)| {
                    index
                        .nearest_triangle(point)
                        .map(|(distance, closest, triangle)| (distance, closest, *part, analysis::triangle_normal(&index.triangles[triangle])))
                })
                .min_by(|x, y| x.0.total_cmp(&y.0));
            match best {
                Some((distance, closest, part, normal)) => {
                    let is_inside = inside.contains(&part.id.as_str());
                    let mut value = json!({
                        "point": rounded3(point),
                        "objectId": part.id,
                        "closest": rounded3(closest),
                        "normal": rounded3(normal),
                        "distance": round_to(if is_inside { -distance } else { distance }, 4),
                        "inside": is_inside,
                    });
                    if !inside.is_empty() {
                        value["insideObjects"] = json!(inside);
                    }
                    value
                }
                None => json!({"point": rounded3(point), "objectId": Value::Null}),
            }
        })
        .collect();
    let mut value = source.header();
    value["ok"] = json!(true);
    value["objects"] = json!(indexes.iter().map(|(part, _)| part.id.as_str()).collect::<Vec<_>>());
    if !rays.is_empty() {
        value["rays"] = json!(ray_results);
    }
    if !points.is_empty() {
        value["points"] = json!(point_results);
    }
    attach_diagnostics(&mut value, &compiled.messages, started);
    Ok(value)
}

/// Every crossing of one ray with the selected objects, nearest first, and
/// the spans between consecutive crossings: inside an object's material
/// (entered then left), or a gap (left one surface, entered the next).
fn every_hit(indexes: &[(&str, crate::meshanalysis::MeshIndex)], origin: P, direction: P, max_distance: f64) -> Value {
    let mut hits: Vec<(f64, &str, P, bool)> = Vec::new();
    for (id, index) in indexes {
        for (t, _, normal) in index.raycast_all(origin, direction, max_distance) {
            if t > 1e-9 {
                hits.push((t, id, normal, dot3(normal, direction) < 0.0));
            }
        }
    }
    hits.sort_by(|a, b| a.0.total_cmp(&b.0));
    let point = |t: f64| [0, 1, 2].map(|axis| origin[axis] + direction[axis] * t);
    let spans: Vec<Value> = hits
        .windows(2)
        .map(|pair| {
            let (a, b) = (&pair[0], &pair[1]);
            let kind = match (a.3, b.3) {
                (true, false) if a.1 == b.1 => "material",
                (false, true) => "gap",
                _ => "ambiguous",
            };
            json!({
                "from": round_to(a.0, 4),
                "to": round_to(b.0, 4),
                "length": round_to(b.0 - a.0, 4),
                "kind": kind,
                "objects": if a.1 == b.1 { json!([a.1]) } else { json!([a.1, b.1]) },
            })
        })
        .collect();
    json!({
        "hit": !hits.is_empty(),
        "hits": hits.iter().map(|(t, id, normal, entering)| json!({
            "objectId": id,
            "point": rounded3(point(*t)),
            "normal": rounded3(*normal),
            "distance": round_to(*t, 4),
            "entering": entering,
        })).collect::<Vec<_>>(),
        "spans": spans,
    })
}

/// The plane of a section_profile request: a point on it and its unit normal.
fn section_plane_input(input: &Value) -> Result<(P, P), ApiError> {
    let plane = input
        .get("plane")
        .filter(|plane| plane.is_object())
        .ok_or_else(|| ApiError::bad("plane is required: {\"axis\": \"x\"|\"y\"|\"z\", \"offset\": mm} or {\"point\": [x,y,z], \"normal\": [x,y,z]}."))?;
    if let Some(axis) = plane.get("axis").filter(|axis| !axis.is_null()) {
        let index = match axis.as_str() {
            Some("x" | "X") => 0,
            Some("y" | "Y") => 1,
            Some("z" | "Z") => 2,
            _ => return Err(ApiError::bad("plane.axis must be \"x\", \"y\" or \"z\".")),
        };
        let offset = match plane.get("offset") {
            None | Some(Value::Null) => 0.0,
            Some(value) => value
                .as_f64()
                .filter(|value| value.is_finite())
                .ok_or_else(|| ApiError::bad("plane.offset must be a number (mm)."))?,
        };
        let mut point = [0.0; 3];
        let mut normal = [0.0; 3];
        point[index] = offset;
        normal[index] = 1.0;
        return Ok((point, normal));
    }
    let point = vec3_input(&plane["point"], "plane.point")?;
    let normal = unit3(vec3_input(&plane["normal"], "plane.normal")?, "plane.normal")?;
    Ok((point, normal))
}

/// `section_profile`: the cross-section of the selected objects with a
/// plane, as 2D loops in the plane's basis (simplified to fit maxPoints),
/// optionally as OpenSCAD `polygon()` code.
pub(crate) fn section_profile_value(
    state: &AppState,
    id: Option<&str>,
    input: &Value,
) -> Result<Value, ApiError> {
    let started = Instant::now();
    let (_, _deadline_scope) = request_deadline(started);
    let source = eval_source(state, id, input)?;
    let (plane_point, plane_normal) = section_plane_input(input)?;
    let requested = match input.get("tolerance") {
        None | Some(Value::Null) => DEFAULT_SECTION_TOLERANCE,
        Some(value) => value
            .as_f64()
            .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
            .ok_or_else(|| ApiError::bad("tolerance must be a number of mm between 0 and 100."))?,
    };
    let max_points = match input.get("maxPoints") {
        None | Some(Value::Null) => DEFAULT_SECTION_POINTS,
        Some(value) => value
            .as_u64()
            .filter(|value| (3..=MAX_SECTION_POINTS as u64).contains(value))
            .ok_or_else(|| ApiError::bad(format!("maxPoints must be an integer from 3 to {MAX_SECTION_POINTS}.")))?
            as usize,
    };
    let want_scad = optional_bool(input, "scad", false)?;
    let within = crate::scancommon::Within::parse(input).map_err(ApiError::bad)?;
    let compiled = compile_eval(&source)?;
    let selected = query_parts(&compiled, input, MAX_QUERY_OBJECTS, "section_profile")?;
    let meshes: Vec<crate::assets::TriMesh> = selected
        .iter()
        .map(|part| crate::meshanalysis::from_engine_mesh(&part.mesh))
        .collect();
    let mut tolerance = requested;
    let mut sections;
    let mut inside;
    let mut attempts = 0;
    // One budget for every attempt: a cut through thousands of outlines is
    // costly to classify, and the retry loop would multiply that.
    let mut work = crate::meshanalysis::Work::new(crate::meshanalysis::ANALYSIS_WORK);
    loop {
        sections = meshes
            .iter()
            .map(|mesh| crate::meshanalysis::section_within(mesh, plane_point, plane_normal, tolerance, &mut work))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| ApiError::new(422, format!("section_profile stopped: {error}")))?;
        inside = crate::scancommon::WithinLength::default();
        if let Some(within) = &within {
            for section in &mut sections {
                inside.add(within.clip_section(section));
            }
        }
        let total: usize = sections
            .iter()
            .flat_map(|section| section.loops.iter())
            .map(|path| path.points.len())
            .sum();
        attempts += 1;
        if total <= max_points || attempts >= 16 {
            break;
        }
        tolerance = if tolerance > 0.0 { tolerance * 2.0 } else { 0.01 };
    }
    let total_points: usize = sections
        .iter()
        .flat_map(|section| section.loops.iter())
        .map(|path| path.points.len())
        .sum();
    let basis = sections
        .iter()
        .find(|section| section.u.iter().any(|value| *value != 0.0))
        .map(|section| (section.origin, section.u, section.v));
    let point2 = |p: [f64; 2]| p.map(|value| round_to(value, 3));
    let objects: Vec<Value> = selected
        .iter()
        .zip(&sections)
        .map(|(part, section)| {
            let mut value = part_identity(part, &source);
            // Net material area (outer loops minus holes) and its centroid.
            let net = match section.centroid {
                Some(_) => section.area,
                None => section.loops.iter().map(|path| path.area).sum::<f64>(),
            };
            value["area"] = json!(round_to(net, 3));
            value["centroid"] = section.centroid.map_or(Value::Null, |centroid| json!(rounded3(centroid)));
            value["loops"] = json!(section
                .loops
                .iter()
                .map(|path| json!({
                    "area": round_to(path.area, 3),
                    "hole": path.hole,
                    "closed": path.closed,
                    "centroid": point2(path.centroid),
                    "centroid3d": rounded3(path.centroid3d),
                    "points": path.points.iter().map(|p| point2(*p)).collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>());
            if let Some((origin, u, v)) = basis {
                if section.loops.is_empty() || (section.origin, section.u, section.v) == (origin, u, v) {
                    return value;
                }
                value["origin"] = json!(rounded3(section.origin));
                value["u"] = json!(rounded3(section.u));
                value["v"] = json!(rounded3(section.v));
            }
            value
        })
        .collect();
    let mut value = source.header();
    value["ok"] = json!(true);
    let mut plane = json!({"point": rounded3(plane_point), "normal": rounded3(plane_normal)});
    if let Some((origin, u, v)) = basis {
        let n = cross3(u, v);
        plane["origin"] = json!(rounded3(origin));
        plane["u"] = json!(rounded3(u));
        plane["v"] = json!(rounded3(v));
        // Maps plane coordinates (x, y, z along the normal) to model space.
        plane["placeMatrix"] = json!([
            [u[0], v[0], n[0], origin[0]],
            [u[1], v[1], n[1], origin[1]],
            [u[2], v[2], n[2], origin[2]],
            [0.0, 0.0, 0.0, 1.0],
        ]);
        round_json(&mut plane["placeMatrix"], 6);
    }
    value["plane"] = plane;
    if let Some(within) = &within {
        value["within"] = within.public(inside.public());
        if sections.iter().all(|section| section.loops.is_empty()) {
            value["nothingInside"] = json!(true);
            value["note"] = json!("Nothing inside the box: the plane cuts no contour within it.");
        }
    }
    value["toleranceRequested"] = json!(requested);
    value["toleranceUsed"] = json!(round_to(tolerance, 6));
    value["simplified"] = json!(tolerance != requested);
    value["totalPoints"] = json!(total_points);
    value["maxPoints"] = json!(max_points);
    if total_points > max_points {
        value["note"] = json!("Still above maxPoints after coarsening the simplification 16 times; narrow objectIds or raise maxPoints.");
    }
    value["objects"] = json!(objects);
    if want_scad {
        let mut points = Vec::new();
        let mut paths = Vec::new();
        for path in sections.iter().flat_map(|section| section.loops.iter()).filter(|path| path.closed && path.points.len() >= 3) {
            let start = points.len();
            points.extend(path.points.iter().map(|p| point2(*p)));
            paths.push((start..points.len()).collect::<Vec<_>>());
        }
        let format_point = |p: &[f64; 2]| format!("[{},{}]", p[0], p[1]);
        let scad = format!(
            "// Section in plane coordinates; place it in the model with\n// multmatrix(placeMatrix) linear_extrude(height = h) section_profile();\nmodule section_profile() polygon(points = [{}], paths = [{}]);\n",
            points.iter().map(format_point).collect::<Vec<_>>().join(","),
            paths
                .iter()
                .map(|path| format!("[{}]", path.iter().map(usize::to_string).collect::<Vec<_>>().join(",")))
                .collect::<Vec<_>>()
                .join(","),
        );
        value["scad"] = json!(scad);
    }
    attach_diagnostics(&mut value, &compiled.messages, started);
    Ok(value)
}

// ---------------------------------------------------------------------------
// Positioning: fit_axis, align_to, place
// ---------------------------------------------------------------------------

/// A row-major 4x4 affine matrix, as OpenSCAD's multmatrix takes it.
pub(crate) type Mat4 = [[f64; 4]; 4];

const IDENTITY4: Mat4 = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];

fn mat_mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut out = [[0.0; 4]; 4];
    for (row, out_row) in out.iter_mut().enumerate() {
        for (column, cell) in out_row.iter_mut().enumerate() {
            *cell = (0..4).map(|k| a[row][k] * b[k][column]).sum();
        }
    }
    out
}

fn mat_apply(m: &Mat4, p: P) -> P {
    [0, 1, 2].map(|row| m[row][0] * p[0] + m[row][1] * p[1] + m[row][2] * p[2] + m[row][3])
}

fn rotate_vector(r: &[[f64; 3]; 3], v: P) -> P {
    [0, 1, 2].map(|row| dot3(r[row], v))
}

/// `[[...], ...]` with up to 6 decimals, for SCAD snippets.
fn scad_matrix(m: &Mat4) -> String {
    let rows: Vec<String> = m
        .iter()
        .map(|row| format!("[{}]", row.iter().map(|value| scad_number(*value)).collect::<Vec<_>>().join(", ")))
        .collect();
    format!("[{}]", rows.join(", "))
}

fn scad_number(value: f64) -> String {
    let rounded = round_to(value, 6);
    let rounded = if rounded == 0.0 { 0.0 } else { rounded };
    format!("{rounded}")
}

fn scad_vec3(v: P) -> String {
    format!("[{}]", v.iter().map(|value| scad_number(*value)).collect::<Vec<_>>().join(", "))
}

fn rounded_matrix(m: &Mat4) -> Value {
    let mut value = json!(m);
    round_json(&mut value, 6);
    value
}

/// A 4x4 (or 3x4, the last row implied) affine matrix argument.
fn matrix_input(value: &Value, what: &str) -> Result<Mat4, ApiError> {
    let shape = || ApiError::bad(format!("{what} must be a 4x4 (or 3x4) row-major matrix of finite numbers, as multmatrix takes."));
    let rows = value.as_array().filter(|rows| rows.len() == 3 || rows.len() == 4).ok_or_else(shape)?;
    let mut m = IDENTITY4;
    for (row, item) in rows.iter().enumerate() {
        let cells = item.as_array().filter(|cells| cells.len() == 4).ok_or_else(shape)?;
        for (column, cell) in cells.iter().enumerate() {
            m[row][column] = cell.as_f64().filter(|value| value.is_finite()).ok_or_else(shape)?;
        }
    }
    let last = m[3];
    if (last[0].abs() + last[1].abs() + last[2].abs() + (last[3] - 1.0).abs()) > 1e-9 {
        return Err(ApiError::bad(format!("{what} must be affine: its last row is [0, 0, 0, 1].")));
    }
    Ok(m)
}

/// Some vector perpendicular to unit `a`.
fn any_perpendicular(a: P) -> P {
    let axis = if a[0].abs() <= a[1].abs() && a[0].abs() <= a[2].abs() {
        [1.0, 0.0, 0.0]
    } else if a[1].abs() <= a[2].abs() {
        [0.0, 1.0, 0.0]
    } else {
        [0.0, 0.0, 1.0]
    };
    let c = cross3(a, axis);
    let length = norm3(c);
    c.map(|value| value / length)
}

/// Rotation (Rodrigues) by `angle` radians about unit `axis`.
fn axis_angle_rotation(axis: P, angle: f64) -> [[f64; 3]; 3] {
    let (s, c) = angle.sin_cos();
    let t = 1.0 - c;
    let [x, y, z] = axis;
    [
        [t * x * x + c, t * x * y - s * z, t * x * z + s * y],
        [t * x * y + s * z, t * y * y + c, t * y * z - s * x],
        [t * x * z - s * y, t * y * z + s * x, t * z * z + c],
    ]
}

/// The smallest rotation taking unit `a` onto unit `b` (180 degrees about a
/// perpendicular axis when they are antiparallel).
fn rotation_between(a: P, b: P) -> [[f64; 3]; 3] {
    let axis = cross3(a, b);
    let sine = norm3(axis);
    let cosine = dot3(a, b).clamp(-1.0, 1.0);
    if sine < 1e-12 {
        if cosine > 0.0 {
            return [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        }
        return axis_angle_rotation(any_perpendicular(a), std::f64::consts::PI);
    }
    axis_angle_rotation(axis.map(|value| value / sine), sine.atan2(cosine))
}

/// Orthonormal frame with `primary` exact and `secondary` projected onto
/// its orthogonal complement, as columns; `None` when they are parallel.
fn frame(primary: P, secondary: P) -> Option<[[f64; 3]; 3]> {
    let e1 = primary;
    let projected = [0, 1, 2].map(|axis| secondary[axis] - dot3(secondary, e1) * e1[axis]);
    let length = norm3(projected);
    if length < 1e-9 {
        return None;
    }
    let e2 = projected.map(|value| value / length);
    let e3 = cross3(e1, e2);
    Some([[e1[0], e2[0], e3[0]], [e1[1], e2[1], e3[1]], [e1[2], e2[2], e3[2]]])
}

/// OpenSCAD `rotate([ax, ay, az])` angles (degrees; X, then Y, then Z about
/// the fixed axes, i.e. R = Rz * Ry * Rx) for rotation `r`.
///
/// Every angle comes from `atan2`, which is well-conditioned everywhere:
/// `asin(-r20)` loses half the digits near +-90 degrees, and switching to the
/// gimbal-lock branch whenever `|sin(ay)| > 1 - 1e-9` (anything within about
/// 0.0026 degrees of it) returned a *different* rotation for, e.g.,
/// `rotate([90, 89.999, 0])`. The branch is now taken only when `cos(ay)` is
/// at round-off level, where `ax` and `az` really are indistinguishable.
fn euler_xyz(r: &[[f64; 3]; 3]) -> P {
    let cy = r[0][0].hypot(r[1][0]);
    let ay = (-r[2][0]).atan2(cy);
    let (ax, az) = if cy > 16.0 * f64::EPSILON {
        (r[2][1].atan2(r[2][2]), r[1][0].atan2(r[0][0]))
    } else {
        // Gimbal lock: only ax - az (or ax + az) is determined; put it in az.
        (0.0, (-r[0][1]).atan2(r[1][1]))
    };
    [ax, ay, az].map(f64::to_degrees)
}

/// Largest entry-wise difference between two rotations.
fn rotation_error(a: &[[f64; 3]; 3], b: &[[f64; 3]; 3]) -> f64 {
    (0..3).flat_map(|row| (0..3).map(move |column| (a[row][column] - b[row][column]).abs())).fold(0.0, f64::max)
}

/// How closely `rotate()` angles must rebuild the matrix, entry-wise, once
/// rounded the way they are written out (6 decimals of a degree, about
/// 2e-8 rad, so a correct extraction is always well inside it).
const EULER_TOLERANCE: f64 = 1e-6;

/// The matrix OpenSCAD builds for `rotate([ax, ay, az])` (degrees).
fn rotation_from_euler(angles: P) -> [[f64; 3]; 3] {
    let [ax, ay, az] = angles.map(f64::to_radians);
    let rx = axis_angle_rotation([1.0, 0.0, 0.0], ax);
    let ry = axis_angle_rotation([0.0, 1.0, 0.0], ay);
    let rz = axis_angle_rotation([0.0, 0.0, 1.0], az);
    let mul = |a: &[[f64; 3]; 3], b: &[[f64; 3]; 3]| -> [[f64; 3]; 3] {
        [0, 1, 2].map(|row| [0, 1, 2].map(|column| (0..3).map(|k| a[row][k] * b[k][column]).sum()))
    };
    mul(&rz, &mul(&ry, &rx))
}

/// Total rotation angle (degrees) and unit axis of `r`.
fn angle_axis(r: &[[f64; 3]; 3]) -> (f64, P) {
    let cosine = ((r[0][0] + r[1][1] + r[2][2] - 1.0) / 2.0).clamp(-1.0, 1.0);
    let angle = cosine.acos();
    let skew = [r[2][1] - r[1][2], r[0][2] - r[2][0], r[1][0] - r[0][1]];
    let length = norm3(skew);
    if angle < 1e-9 {
        return (0.0, [0.0, 0.0, 1.0]);
    }
    if length > 1e-6 {
        return (angle.to_degrees(), skew.map(|value| value / length));
    }
    // About 180 degrees: the axis is the column of R + I with the largest norm.
    let columns = [0, 1, 2].map(|column| [0, 1, 2].map(|row| r[row][column] + if row == column { 1.0 } else { 0.0 }));
    let best = columns.iter().copied().max_by(|a, b| norm3(*a).total_cmp(&norm3(*b))).unwrap_or([0.0, 0.0, 1.0]);
    let length = norm3(best);
    (angle.to_degrees(), best.map(|value| value / length))
}

/// `align_to`: the rotation taking direction(s) `from` onto `to`, as a
/// multmatrix and as OpenSCAD rotate() angles. Pure math; no design.
pub(crate) fn align_to_value(input: &Value) -> Result<Value, ApiError> {
    let pairs = input
        .get("pairs")
        .and_then(Value::as_array)
        .filter(|pairs| (1..=2).contains(&pairs.len()))
        .ok_or_else(|| ApiError::bad("pairs is required: 1 or 2 entries {\"from\": [x,y,z], \"to\": [x,y,z]} (the first is matched exactly, the second as closely as possible)."))?;
    let mut directions = Vec::new();
    for (index, pair) in pairs.iter().enumerate() {
        let from = unit3(vec3_input(&pair["from"], &format!("pairs[{index}].from"))?, &format!("pairs[{index}].from"))?;
        let to = unit3(vec3_input(&pair["to"], &format!("pairs[{index}].to"))?, &format!("pairs[{index}].to"))?;
        directions.push((from, to));
    }
    let pivot = match input.get("pivot") {
        None | Some(Value::Null) => [0.0; 3],
        Some(value) => vec3_input(value, "pivot")?,
    };
    let then = match input.get("then") {
        None | Some(Value::Null) => [0.0; 3],
        Some(value) => vec3_input(value, "then")?,
    };
    let (from1, to1) = directions[0];
    let mut residual = None;
    let rotation = if let Some(&(from2, to2)) = directions.get(1) {
        let source = frame(from1, from2).ok_or_else(|| ApiError::bad("pairs[1].from is parallel to pairs[0].from, so it fixes no extra rotation; give a non-parallel secondary direction or one pair."))?;
        let target = frame(to1, to2).ok_or_else(|| ApiError::bad("pairs[1].to is parallel to pairs[0].to, so it fixes no extra rotation; give a non-parallel secondary direction or one pair."))?;
        // R = target * source^T maps each source frame column onto the target's.
        let r: [[f64; 3]; 3] = [0, 1, 2].map(|row| [0, 1, 2].map(|column| (0..3).map(|k| target[row][k] * source[column][k]).sum()));
        let mapped = rotate_vector(&r, from2);
        residual = Some(dot3(mapped, to2).clamp(-1.0, 1.0).acos().to_degrees());
        r
    } else {
        rotation_between(from1, to1)
    };
    let rotated_pivot = rotate_vector(&rotation, pivot);
    let translation = [0, 1, 2].map(|axis| pivot[axis] - rotated_pivot[axis] + then[axis]);
    let mut matrix = IDENTITY4;
    for row in 0..3 {
        matrix[row][..3].copy_from_slice(&rotation[row]);
        matrix[row][3] = translation[row];
    }
    // Verify the angles as they are serialised, not as computed: the reader
    // gets the rounded ones.
    let euler = euler_xyz(&rotation).map(|value| round_to(value, 6));
    let euler_error = rotation_error(&rotation_from_euler(euler), &rotation);
    let rotate_verified = euler_error <= EULER_TOLERANCE;
    let (angle, axis) = angle_axis(&rotation);
    let has_pivot = pivot.iter().any(|value| *value != 0.0);
    let rotate_form = if has_pivot {
        format!(
            "translate({}) rotate({}) translate({}) children();",
            scad_vec3([0, 1, 2].map(|axis| pivot[axis] + then[axis])),
            scad_vec3(euler),
            scad_vec3(pivot.map(|value| -value))
        )
    } else if then.iter().any(|value| *value != 0.0) {
        format!("translate({}) rotate({}) children();", scad_vec3(then), scad_vec3(euler))
    } else {
        format!("rotate({}) children();", scad_vec3(euler))
    };
    let mut value = json!({
        "ok": true,
        "matrix": rounded_matrix(&matrix),
        "rotate": euler,
        "rotateVerified": rotate_verified,
        "angleDeg": round_to(angle, 4),
        "axis": rounded3(axis),
        "scad": {
            "multmatrix": format!("multmatrix({}) children();", scad_matrix(&matrix)),
            "rotate": rotate_form,
        },
        "note": "matrix is row-major for multmatrix(); rotate is OpenSCAD's rotate([x, y, z]) (about X, then Y, then Z, fixed axes) and reproduces the same rotation. Replace children() with the object (e.g. import(\"scan.stl\")). Chain with place to centre and ground the result.",
    });
    if let Some(residual) = residual {
        value["residualDeg"] = json!(round_to(residual, 4));
    }
    if !rotate_verified {
        // Never hand out a rotate() that is a different rotation.
        value["rotate"] = Value::Null;
        value["scad"]["rotate"] = Value::Null;
        value["note"] = json!(format!(
            "rotate([x, y, z]) angles could not reproduce this rotation (off by {euler_error:.1e} per matrix entry); use the multmatrix form."
        ));
    }
    Ok(value)
}

const MAX_AXIS_SECTIONS: u64 = 200;

/// `fit_axis`: the true centerline of elongated bodies, from the centroids of
/// perpendicular sections (unbiased by tapers and flares, unlike the PCA box).
pub(crate) fn fit_axis_value(state: &AppState, id: Option<&str>, input: &Value) -> Result<Value, ApiError> {
    let started = Instant::now();
    let (_, _deadline_scope) = request_deadline(started);
    let source = eval_source(state, id, input)?;
    let mut options = crate::meshanalysis::AxisOptions::default();
    if let Some(value) = input.get("initial").filter(|value| !value.is_null()) {
        options.initial = Some(unit3(vec3_input(value, "initial")?, "initial")?);
    }
    if let Some(value) = input.get("sections").filter(|value| !value.is_null()) {
        options.sections = value
            .as_u64()
            .filter(|value| (3..=MAX_AXIS_SECTIONS).contains(value))
            .ok_or_else(|| ApiError::bad(format!("sections must be an integer from 3 to {MAX_AXIS_SECTIONS}.")))?
            as usize;
    }
    if let Some(value) = input.get("trim").filter(|value| !value.is_null()) {
        options.trim = value
            .as_f64()
            .filter(|value| (0.0..=0.45).contains(value))
            .ok_or_else(|| ApiError::bad("trim must be a number from 0 to 0.45 (fraction of the length skipped at each end)."))?;
    }
    if let Some(value) = input.get("range").filter(|value| !value.is_null()) {
        let pair = value
            .as_array()
            .filter(|items| items.len() == 2)
            .and_then(|items| Some((items[0].as_f64()?, items[1].as_f64()?)))
            .filter(|(low, high)| low.is_finite() && high.is_finite() && low < high)
            .ok_or_else(|| ApiError::bad("range must be [t0, t1] with t0 < t1 (mm along the axis from the body centre)."))?;
        options.range = Some(pair);
    }
    let compiled = compile_eval(&source)?;
    let selected = query_parts(&compiled, input, MAX_DESCRIBE_OBJECTS, "fit_axis")?;
    let mut failures = Vec::new();
    let objects: Vec<Value> = selected
        .iter()
        .map(|part| {
            let mesh = crate::meshanalysis::from_engine_mesh(&part.mesh);
            let mut value = part_identity(part, &source);
            match crate::meshanalysis::fit_axis(&mesh, options) {
                Ok(fit) => {
                    let matrix = fit.align_matrix;
                    let mut fitted = serde_json::to_value(&fit).unwrap_or(Value::Null);
                    round_json(&mut fitted, 4);
                    if let Some(sections) = fitted.get_mut("sections") {
                        round_json(sections, 3);
                    }
                    merge_fields(&mut value, fitted);
                    value["alignMatrix"] = rounded_matrix(&matrix);
                    value["scad"] = json!(format!("multmatrix({}) children();", scad_matrix(&matrix)));
                }
                Err(error) => {
                    failures.push(format!("{}: {error}", part.id));
                    value["error"] = json!(error);
                }
            }
            value
        })
        .collect();
    if !objects.is_empty() && failures.len() == objects.len() {
        return Err(ApiError::new(422, format!("fit_axis could not fit an axis: {}", failures.join("; "))));
    }
    let mut value = source.header();
    value["ok"] = json!(true);
    value["objects"] = json!(objects);
    value["hint"] = json!(
        "alignMatrix moves the fitted point to the origin and the axis onto +Z: wrap the object, e.g. \
         multmatrix(alignMatrix) import(\"scan.stl\");. To point the axis elsewhere, use align_to with \
         {from: direction, to: target}; then place (matrix: alignMatrix) to centre it on X/Y and drop it to z = 0. \
         rmsOffset/maxOffset show how straight the centerline is; taperSlope is the radius change per mm."
    );
    attach_diagnostics(&mut value, &compiled.messages, started);
    Ok(value)
}

/// `place`: the translate that centres the selected objects (after an
/// optional matrix) on X/Y (or X/Y/Z) and drops them to z = 0, from the
/// exact bounds of the transformed vertices.
pub(crate) fn place_value(state: &AppState, id: Option<&str>, input: &Value) -> Result<Value, ApiError> {
    let started = Instant::now();
    let (_, _deadline_scope) = request_deadline(started);
    let source = eval_source(state, id, input)?;
    let matrix = match input.get("matrix") {
        None | Some(Value::Null) => IDENTITY4,
        Some(value) => matrix_input(value, "matrix")?,
    };
    let center = match input.get("center") {
        None | Some(Value::Null) => "xy",
        Some(value) => match value.as_str() {
            Some(mode @ ("xy" | "xyz" | "none")) => mode,
            _ => return Err(ApiError::bad("center must be \"xy\", \"xyz\" or \"none\".")),
        },
    };
    let ground = optional_bool(input, "ground", true)?;
    let on = match input.get("on") {
        None | Some(Value::Null) => [0.0; 3],
        Some(value) => {
            let items = value
                .as_array()
                .filter(|items| items.len() == 2 || items.len() == 3)
                .ok_or_else(|| ApiError::bad("on must be [x, y] (or [x, y, z] with center \"xyz\")."))?;
            let mut point = [0.0; 3];
            for (slot, item) in point.iter_mut().zip(items) {
                *slot = item.as_f64().filter(|value| value.is_finite()).ok_or_else(|| ApiError::bad("on must hold finite numbers."))?;
            }
            point
        }
    };
    let compiled = compile_eval(&source)?;
    let selected = query_parts(&compiled, input, MAX_QUERY_OBJECTS, "place")?;
    let mut before = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
    let mut after = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
    let grow = |bounds: &mut (P, P), p: P| {
        for axis in 0..3 {
            bounds.0[axis] = bounds.0[axis].min(p[axis]);
            bounds.1[axis] = bounds.1[axis].max(p[axis]);
        }
    };
    for part in &selected {
        for triangle in &part.mesh.triangles {
            for vertex in &triangle.vertices {
                let point = [vertex.x, vertex.y, vertex.z];
                grow(&mut before, point);
                grow(&mut after, mat_apply(&matrix, point));
            }
        }
    }
    if !after.0[0].is_finite() {
        return Err(ApiError::new(422, "The selected objects have no geometry to place."));
    }
    let mid = |axis: usize| (after.0[axis] + after.1[axis]) / 2.0;
    let mut translate = [0.0; 3];
    if center != "none" {
        translate[0] = on[0] - mid(0);
        translate[1] = on[1] - mid(1);
    }
    if center == "xyz" {
        translate[2] = on[2] - mid(2);
    }
    if ground {
        translate[2] = -after.0[2];
    }
    let mut shift = IDENTITY4;
    for axis in 0..3 {
        shift[axis][3] = translate[axis];
    }
    let combined = mat_mul(&shift, &matrix);
    let final_bounds = (
        [0, 1, 2].map(|axis| after.0[axis] + translate[axis]),
        [0, 1, 2].map(|axis| after.1[axis] + translate[axis]),
    );
    let bounds_value = |bounds: (P, P)| json!({"min": rounded3(bounds.0), "max": rounded3(bounds.1), "size": rounded3([0, 1, 2].map(|axis| bounds.1[axis] - bounds.0[axis]))});
    let identity = matrix == IDENTITY4;
    let wrap = if identity {
        format!("translate({}) children();", scad_vec3(translate))
    } else {
        format!("translate({}) multmatrix({}) children();", scad_vec3(translate), scad_matrix(&matrix))
    };
    let mut value = source.header();
    value["ok"] = json!(true);
    value["objects"] = json!(selected.iter().map(|part| part.id.as_str()).collect::<Vec<_>>());
    value["translate"] = json!(rounded3(translate));
    value["matrix"] = rounded_matrix(&combined);
    value["boundsBefore"] = bounds_value(before);
    value["boundsAfter"] = bounds_value(final_bounds);
    value["scad"] = json!({
        "translate": wrap,
        "multmatrix": format!("multmatrix({}) children();", scad_matrix(&combined)),
    });
    value["note"] = json!("Bounds are exact (every vertex transformed). Replace children() with the object(s), e.g. import(\"scan.stl\"). Several objects move together as one group.");
    attach_diagnostics(&mut value, &compiled.messages, started);
    Ok(value)
}

// ---------------------------------------------------------------------------
// check_insertion
// ---------------------------------------------------------------------------

/// Most waypoints one insertion path may hold.
pub(crate) const MAX_INSERTION_WAYPOINTS: usize = 64;

fn strip_nulls(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.retain(|_, field| !field.is_null());
            fields.values_mut().for_each(strip_nulls);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_nulls),
        _ => {}
    }
}

/// `check_insertion`: whether object `moving` can travel along a
/// translation path without touching the fixed objects, and with what
/// clearance (see [`crate::insertion`]).
pub(crate) fn insertion_value(state: &AppState, id: Option<&str>, input: &Value) -> Result<Value, ApiError> {
    let started = Instant::now();
    let (deadline, _deadline_scope) = request_deadline(started);
    let source = eval_source(state, id, input)?;
    let tolerance = tolerance_input(input, &source)?;
    let precision = match input.get("precision") {
        None | Some(Value::Null) => 0.01,
        Some(value) => value
            .as_f64()
            .filter(|precision| precision.is_finite() && *precision >= 0.001)
            .ok_or_else(|| ApiError::bad("precision must be a number of millimetres >= 0.001."))?,
    };
    let moving_id = input
        .get("moving")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad("moving must be the ID of the object that moves."))?
        .to_string();
    let mut waypoints: Vec<P> = match (input.get("path").filter(|v| !v.is_null()), input.get("direction").filter(|v| !v.is_null())) {
        (Some(_), Some(_)) => return Err(ApiError::bad("Provide path or direction + distance, not both.")),
        (Some(Value::Array(items)), None) => {
            if items.is_empty() || items.len() > MAX_INSERTION_WAYPOINTS {
                return Err(ApiError::bad(format!(
                    "path must hold 1 to {MAX_INSERTION_WAYPOINTS} waypoints [x, y, z]."
                )));
            }
            items
                .iter()
                .map(|item| vec3_input(item, "path[]"))
                .collect::<Result<_, _>>()?
        }
        (Some(_), None) => return Err(ApiError::bad("path must be an array of [x, y, z] offsets.")),
        (None, Some(direction)) => {
            let direction = unit3(vec3_input(direction, "direction")?, "direction")?;
            let distance = input
                .get("distance")
                .and_then(Value::as_f64)
                .filter(|distance| distance.is_finite() && *distance >= 0.0)
                .ok_or_else(|| ApiError::bad("distance (mm, >= 0) is required with direction."))?;
            vec![[0.0; 3], direction.map(|component| component * distance)]
        }
        (None, None) => {
            return Err(ApiError::bad(
                "Provide path ([[x, y, z], ...] offsets of the moving object, starting e.g. at [0, 0, 0]) or direction + distance.",
            ))
        }
    };
    if waypoints.len() == 1 {
        waypoints.push(waypoints[0]);
    }
    let fixed_ids: Option<Vec<String>> = match input.get("fixed") {
        None | Some(Value::Null) => None,
        Some(Value::String(one)) => Some(vec![one.clone()]),
        Some(Value::Array(items)) if items.len() <= MAX_ID_LIST => Some(
            items
                .iter()
                .map(|item| item.as_str().map(str::to_string).ok_or_else(|| ApiError::bad("fixed must list object IDs.")))
                .collect::<Result<_, _>>()?,
        ),
        Some(_) => return Err(ApiError::bad("fixed must be an object ID or a list of them.")),
    };
    let compiled = compile_eval(&source)?;
    let visible = select_parts(&compiled.parts, input, "visibleObjectIds")?;
    let known = |wanted: &str| -> Result<(), ApiError> {
        if compiled.parts.iter().any(|part| part.id == wanted) {
            Ok(())
        } else {
            Err(ApiError::bad(format!(
                "Unknown object ID {wanted:?}. Objects are: {}.",
                compiled.parts.iter().map(|part| part.id.as_str()).collect::<Vec<_>>().join(", ")
            )))
        }
    };
    known(&moving_id)?;
    let moving_part = compiled.parts.iter().find(|part| part.id == moving_id).expect("known");
    let fixed_parts: Vec<&engine::CompiledPart> = match &fixed_ids {
        Some(ids) => {
            for wanted in ids {
                known(wanted)?;
                if *wanted == moving_id {
                    return Err(ApiError::bad("fixed must not include the moving object."));
                }
            }
            compiled.parts.iter().filter(|part| ids.contains(&part.id)).collect()
        }
        None => visible.into_iter().filter(|part| part.id != moving_id).collect(),
    };
    if fixed_parts.is_empty() {
        return Err(ApiError::bad("No fixed objects to check against."));
    }
    if fixed_parts.len() > MAX_CHECKED_OBJECTS {
        return Err(ApiError::new(
            422,
            format!("{} fixed objects exceed the {MAX_CHECKED_OBJECTS}-object limit; pass fixed.", fixed_parts.len()),
        ));
    }
    let moving = MeshIndex::from_mesh(&moving_part.mesh);
    let fixed: Vec<MeshIndex> = fixed_parts.iter().map(|part| MeshIndex::from_mesh(&part.mesh)).collect();
    let path = crate::insertion::Path::new(waypoints.clone());
    let options = crate::insertion::Options {
        tolerance,
        precision,
        deadline,
        max_evaluations: crate::insertion::MAX_EVALUATIONS,
        pair_tests: crate::insertion::QUERY_PAIR_TESTS,
    };
    let report = crate::insertion::check(&moving, &fixed, &path, &options);

    let r = |value: f64| round_to(value, 4);
    let sample_public = |sample: &crate::insertion::Sample| {
        json!({
            "travel": r(sample.travel),
            "offset": rounded3(sample.offset),
            "segment": (waypoints.len() > 2).then_some(sample.segment),
            "clearance": r(sample.clearance),
            "objectId": fixed_parts[sample.object].id,
            "pointA": rounded3(sample.point_a),
            "pointB": rounded3(sample.point_b),
        })
    };
    let mut value = source.header();
    value["ok"] = json!(true);
    value["moving"] = json!(moving_id);
    value["fixed"] = json!(fixed_parts.iter().map(|part| part.id.as_str()).collect::<Vec<_>>());
    value["tolerance"] = json!(tolerance);
    value["pathLength"] = json!(r(report.length));
    value["passes"] = json!(report.passes);
    value["clearanceOk"] = json!(report.clearance_ok);
    value["complete"] = json!(report.undetermined.is_none());
    value["undetermined"] = json!(report.undetermined);
    if let Some(collision) = &report.collision {
        let mut public = sample_public(collision);
        public["remaining"] = json!(r(report.length - collision.travel));
        if report.starts_blocked {
            public["atStart"] = json!(true);
            public["kind"] = json!(if report.starts_inside { "inside" } else { "contact" });
        }
        if let Some(penetration) = &report.penetration {
            public["penetration"] = json!({
                "depth": r(penetration.depth),
                "travel": r(penetration.travel),
                "offset": rounded3(penetration.offset),
                "point": rounded3(penetration.point),
            });
        }
        value["collision"] = public;
    }
    if let Some(minimum) = &report.min_clearance {
        if report.collision.is_none() {
            let mut public = sample_public(minimum);
            if report.min_lower_bound < minimum.clearance - 1e-4 {
                public["lowerBound"] = json!(r(report.min_lower_bound));
            }
            value["minClearance"] = public;
        }
    }
    if let Some(violation) = &report.first_violation {
        if report.collision.is_none_or(|collision| violation.travel < collision.travel - 1e-9) {
            value["firstViolation"] = sample_public(violation);
        }
    }
    if report.checked_to < report.length - 1e-9 && report.collision.is_none() {
        value["checkedTo"] = json!(r(report.checked_to));
    }
    value["queries"] = json!(report.evaluations);
    attach_diagnostics(&mut value, &compiled.messages, started);
    strip_nulls(&mut value);
    Ok(value)
}

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;
