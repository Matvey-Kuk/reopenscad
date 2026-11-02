//! Shared plumbing of the scan-to-CAD tools (`extract_features`,
//! `compare_surfaces`, `compare_sections`, `measure_between`, `fit_curve`):
//! resolving a surface argument (design objects, or an asset posed by a
//! matrix or by the object that imports it) into one indexed mesh with its
//! open boundary, area-weighted surface sampling, weighted statistics,
//! colour maps, and heatmap rendering with legends and markers.
//!
//! Nothing here uses solid booleans, so every surface may be open: a
//! one-sided scan or a model.

pub(crate) use crate::vec3::{add, cross, dot, norm, scale, sub};
use crate::agent::{self, EvalSource};
use crate::assets::TriMesh;
use crate::engine;
use crate::meshanalysis::{self, MeshIndex};
use crate::meshops::{self, Mat4};
use crate::raster::{self, Rgba};
use crate::AppState;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) type P = [f64; 3];

/// Most triangles one surface argument may carry into a comparison.
pub(crate) const MAX_SURFACE_TRIANGLES: usize = 1_000_000;

/// The tolerance of the design a call names; the default for an unknown one.
pub(crate) fn design_tolerance(state: &AppState, input: &Value) -> f64 {
    crate::input_design_id(input)
        .filter(|id| crate::valid_workspace_id(id))
        .and_then(|id| state.workspaces.get(id).ok().flatten())
        .map_or(crate::DEFAULT_TOLERANCE, |workspace| workspace.tolerance)
}

// ---------------------------------------------------------------------------
// Small vector helpers
// ---------------------------------------------------------------------------

pub(crate) fn unit(a: P) -> P {
    let n = norm(a);
    if n > 0.0 && n.is_finite() {
        scale(a, 1.0 / n)
    } else {
        [0.0; 3]
    }
}

pub(crate) fn round_to(value: f64, decimals: i32) -> f64 {
    let factor = 10f64.powi(decimals);
    // + 0.0 turns -0.0 (an empty f64 sum, a rounded tiny negative) into 0.
    (value * factor).round() / factor + 0.0
}

pub(crate) fn r3(p: P) -> [f64; 3] {
    p.map(|v| round_to(v, 3))
}

pub(crate) fn corners(mesh: &TriMesh, t: usize) -> [P; 3] {
    mesh.triangles[t].map(|i| mesh.points[i as usize])
}

/// (unit normal, area) of triangle `t`.
pub(crate) fn face_normal_area(mesh: &TriMesh, t: usize) -> (P, f64) {
    let [a, b, c] = corners(mesh, t);
    let n = cross(sub(b, a), sub(c, a));
    let length = norm(n);
    (if length > 0.0 { scale(n, 1.0 / length) } else { [0.0; 3] }, 0.5 * length)
}

// ---------------------------------------------------------------------------
// Argument helpers (String errors, as the derive tools use)
// ---------------------------------------------------------------------------

pub(crate) fn number(input: &Value, key: &str, default: f64, min: f64, max: f64) -> Result<f64, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_f64()
            .filter(|v| v.is_finite() && (min..=max).contains(v))
            .ok_or_else(|| format!("{key} must be a number from {min} to {max}.")),
    }
}

pub(crate) fn integer(input: &Value, key: &str, default: usize, min: usize, max: usize) -> Result<usize, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .map(|v| v as usize)
            .filter(|v| (min..=max).contains(v))
            .ok_or_else(|| format!("{key} must be an integer from {min} to {max}.")),
    }
}

pub(crate) fn flag(input: &Value, key: &str, default: bool) -> Result<bool, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(format!("{key} must be true or false.")),
    }
}

pub(crate) fn vec3(value: &Value, what: &str) -> Result<P, String> {
    let items = value.as_array().filter(|items| items.len() == 3).ok_or_else(|| format!("{what} must be [x, y, z]."))?;
    let mut out = [0.0; 3];
    for (slot, item) in out.iter_mut().zip(items) {
        *slot = item.as_f64().filter(|v| v.is_finite()).ok_or_else(|| format!("{what} must hold finite numbers."))?;
    }
    Ok(out)
}

pub(crate) fn matrix(value: &Value) -> Result<Mat4, String> {
    let rows = value
        .as_array()
        .filter(|rows| rows.len() == 3 || rows.len() == 4)
        .ok_or("matrix must be 3 or 4 rows of 4 numbers (row-major, as multmatrix takes it).")?;
    let mut m = [[0.0, 0.0, 0.0, 0.0], [0.0, 0.0, 0.0, 0.0], [0.0, 0.0, 0.0, 0.0], [0.0, 0.0, 0.0, 1.0]];
    for (r, row) in rows.iter().enumerate() {
        let cells = row.as_array().filter(|cells| cells.len() == 4).ok_or("each matrix row must hold 4 numbers.")?;
        for (c, cell) in cells.iter().enumerate() {
            m[r][c] = cell.as_f64().filter(|v| v.is_finite()).ok_or("matrix entries must be finite numbers.")?;
        }
    }
    if rows.len() == 4 && m[3] != [0.0, 0.0, 0.0, 1.0] {
        return Err("matrix row 4 must be [0, 0, 0, 1] (affine transforms only).".into());
    }
    Ok(m)
}

pub(crate) fn rounded_matrix(m: &Mat4) -> Value {
    json!(m.map(|row| row.map(|v| round_to(v, 6))))
}

/// Inverse of an affine matrix; `None` when singular.
pub(crate) fn invert_affine(m: &Mat4) -> Option<Mat4> {
    let a = [[m[0][0], m[0][1], m[0][2]], [m[1][0], m[1][1], m[1][2]], [m[2][0], m[2][1], m[2][2]]];
    let det = a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1]) - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0])
        + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0]);
    if !(det.abs() > 1e-15) || !det.is_finite() {
        return None;
    }
    let inv_det = 1.0 / det;
    let mut r = [[0.0; 3]; 3];
    r[0][0] = (a[1][1] * a[2][2] - a[1][2] * a[2][1]) * inv_det;
    r[0][1] = (a[0][2] * a[2][1] - a[0][1] * a[2][2]) * inv_det;
    r[0][2] = (a[0][1] * a[1][2] - a[0][2] * a[1][1]) * inv_det;
    r[1][0] = (a[1][2] * a[2][0] - a[1][0] * a[2][2]) * inv_det;
    r[1][1] = (a[0][0] * a[2][2] - a[0][2] * a[2][0]) * inv_det;
    r[1][2] = (a[0][2] * a[1][0] - a[0][0] * a[1][2]) * inv_det;
    r[2][0] = (a[1][0] * a[2][1] - a[1][1] * a[2][0]) * inv_det;
    r[2][1] = (a[0][1] * a[2][0] - a[0][0] * a[2][1]) * inv_det;
    r[2][2] = (a[0][0] * a[1][1] - a[0][1] * a[1][0]) * inv_det;
    let t = [m[0][3], m[1][3], m[2][3]];
    let mut out = [[0.0; 4]; 4];
    for i in 0..3 {
        for j in 0..3 {
            out[i][j] = r[i][j];
        }
        out[i][3] = -(r[i][0] * t[0] + r[i][1] * t[1] + r[i][2] * t[2]);
    }
    out[3][3] = 1.0;
    Some(out)
}

// ---------------------------------------------------------------------------
// Surfaces
// ---------------------------------------------------------------------------

/// One surface argument, resolved: an indexed mesh in design coordinates.
pub(crate) struct Surface {
    pub label: String,
    /// "objects" (compiled design objects) or "asset".
    pub kind: &'static str,
    pub mesh: TriMesh,
    pub open_edges: usize,
    /// Scan data as uploaded: what it shows was measured.
    /// False for model objects.
    pub observed: bool,
    /// The uploaded scan this surface comes from, for assets.
    pub scan: Option<String>,
    /// Nearest-point index over the open edges (slim triangles along each).
    rim_index: Option<MeshIndex>,
    /// How the surface was obtained, for the reply.
    pub source: Value,
}

impl Surface {
    pub(crate) fn new(label: String, kind: &'static str, mesh: TriMesh, source: Value) -> Self {
        let (boundary, open_edges) = boundary_faces(&mesh);
        let rim = rim_edges(&mesh, &boundary);
        let rim_index = (!rim.is_empty()).then(|| {
            let mut slim = TriMesh::default();
            for [a, b] in rim.values().flatten() {
                let along = sub(*b, *a);
                let side = unit(cross(along, if along[0].abs() < 0.9 * norm(along) { [1.0, 0.0, 0.0] } else { [0.0, 1.0, 0.0] }));
                let base = slim.points.len() as u32;
                slim.points.extend([*a, *b, add(scale(add(*a, *b), 0.5), scale(side, 1e-7 * norm(along).max(1e-9)))]);
                slim.triangles.push([base, base + 1, base + 2]);
            }
            MeshIndex::new(&slim)
        });
        Self { label, kind, mesh, open_edges, observed: false, scan: None, rim_index, source }
    }

    /// Distance from `point` to the surface's open edge (infinite when closed).
    pub(crate) fn rim_distance(&self, point: P) -> f64 {
        self.rim_index.as_ref().map_or(f64::INFINITY, |index| index.distance(point))
    }

    pub(crate) fn index(&self) -> MeshIndex {
        MeshIndex::new(&self.mesh)
    }

    /// This surface kept to the faces touching `within`.
    pub(crate) fn cropped(self, within: &Within) -> Self {
        let Self { label, kind, mesh, observed, scan, source, .. } = self;
        let mut out = Self::new(label, kind, within.crop(&mesh), source);
        out.observed = observed;
        out.scan = scan;
        out
    }

    pub(crate) fn area(&self) -> f64 {
        (0..self.mesh.triangles.len()).map(|t| face_normal_area(&self.mesh, t).1).sum()
    }

    pub(crate) fn bounds(&self) -> Option<(P, P)> {
        meshops::bounds(&self.mesh)
    }

    pub(crate) fn brief(&self) -> Value {
        let mut value = json!({
            "label": self.label,
            "kind": self.kind,
            "triangles": self.mesh.triangles.len(),
            "area": round_to(self.area(), 2),
            "openEdges": self.open_edges,
            "closed": self.open_edges == 0,
            "source": self.source,
        });
        if let Some((min, max)) = self.bounds() {
            value["bounds"] = json!({"min": r3(min), "max": r3(max)});
        }
        value
    }
}

/// Per triangle whether it touches an open (single-use) edge, and the number
/// of open edges.
pub(crate) fn boundary_faces(mesh: &TriMesh) -> (Vec<bool>, usize) {
    let mut uses: HashMap<(u32, u32), u32> = HashMap::with_capacity(mesh.triangles.len() * 3 / 2 + 1);
    for t in &mesh.triangles {
        for k in 0..3 {
            let (a, b) = (t[k], t[(k + 1) % 3]);
            *uses.entry((a.min(b), a.max(b))).or_insert(0) += 1;
        }
    }
    let open = uses.values().filter(|count| **count == 1).count();
    let flags = mesh
        .triangles
        .iter()
        .map(|t| (0..3).any(|k| uses.get(&(t[k].min(t[(k + 1) % 3]), t[k].max(t[(k + 1) % 3]))) == Some(&1)))
        .collect();
    (flags, open)
}

fn rim_edges(mesh: &TriMesh, boundary: &[bool]) -> HashMap<usize, Vec<[P; 2]>> {
    let mut uses: HashMap<(u32, u32), u32> = HashMap::new();
    for (t, tri) in mesh.triangles.iter().enumerate() {
        if !boundary[t] {
            continue;
        }
        for k in 0..3 {
            let (a, b) = (tri[k], tri[(k + 1) % 3]);
            uses.insert((a.min(b), a.max(b)), 0);
        }
    }
    for tri in &mesh.triangles {
        for k in 0..3 {
            let (a, b) = (tri[k], tri[(k + 1) % 3]);
            if let Some(count) = uses.get_mut(&(a.min(b), a.max(b))) {
                *count += 1;
            }
        }
    }
    let mut out: HashMap<usize, Vec<[P; 2]>> = HashMap::new();
    for (t, tri) in mesh.triangles.iter().enumerate() {
        if !boundary[t] {
            continue;
        }
        for k in 0..3 {
            let (a, b) = (tri[k], tri[(k + 1) % 3]);
            if uses.get(&(a.min(b), a.max(b))) == Some(&1) {
                out.entry(t).or_default().push([mesh.points[a as usize], mesh.points[b as usize]]);
            }
        }
    }
    out
}

/// Lazily compiled design for the surface arguments of one call.
pub(crate) struct Design<'a> {
    state: &'a AppState,
    input: &'a Value,
    compiled: Option<(EvalSource, Arc<engine::MultipartCompileOutput>)>,
}

impl<'a> Design<'a> {
    pub(crate) fn new(state: &'a AppState, input: &'a Value) -> Self {
        Self { state, input, compiled: None }
    }

    pub(crate) fn compiled(&mut self) -> Result<&(EvalSource, Arc<engine::MultipartCompileOutput>), String> {
        if self.compiled.is_none() {
            let source = agent::eval_source(self.state, None, self.input).map_err(|error| error.message)?;
            let compiled = agent::compile_eval(&source).map_err(|error| error.message)?;
            self.compiled = Some((source, compiled));
        }
        Ok(self.compiled.as_ref().expect("just compiled"))
    }

    /// Compiler console of the compile, if one ran.
    pub(crate) fn messages(&self) -> Vec<String> {
        self.compiled.as_ref().map(|(_, compiled)| compiled.messages.clone()).unwrap_or_default()
    }

    pub(crate) fn header(&self) -> Value {
        match &self.compiled {
            Some((source, _)) => source.header(),
            None => json!({"designId": crate::input_design_id(self.input)}),
        }
    }

    fn part(&mut self, id: &str) -> Result<engine::CompiledPart, String> {
        self.with_part(id, |part| part.clone())
    }

    /// Runs `f` on object `id`'s compiled part without copying it.
    fn with_part<R>(&mut self, id: &str, f: impl FnOnce(&engine::CompiledPart) -> R) -> Result<R, String> {
        let (_, compiled) = self.compiled()?;
        compiled
            .parts
            .iter()
            .chain(compiled.background_parts.iter())
            .find(|part| part.id == id || part.name == id)
            .map(f)
            .ok_or_else(|| {
                format!(
                    "Unknown object {id:?}. Objects are: {}.",
                    compiled
                        .parts
                        .iter()
                        .chain(compiled.background_parts.iter())
                        .map(|part| part.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    }

    /// Source line range of an object, when known.
    pub(crate) fn object_lines(&mut self, id: &str) -> Value {
        let Ok(part) = self.part(id) else { return Value::Null };
        let Some((source, _)) = &self.compiled else { return Value::Null };
        let location = source.source_location(part.source_range);
        if location.is_null() {
            Value::Null
        } else {
            json!([location["startLine"], location["endLine"]])
        }
    }

    /// Resolves one surface argument; `what` names it in errors.
    pub(crate) fn surface(&mut self, spec: &Value, what: &str) -> Result<Surface, String> {
        let ids: Option<Vec<String>> = match spec {
            Value::String(id) => Some(vec![id.clone()]),
            Value::Array(items) => Some(
                items
                    .iter()
                    .map(|item| item.as_str().map(str::to_string).ok_or(format!("{what} lists object IDs as strings.")))
                    .collect::<Result<_, _>>()?,
            ),
            Value::Object(map) if map.contains_key("objectIds") => Some(
                map["objectIds"]
                    .as_array()
                    .ok_or(format!("{what}.objectIds must be an array of object IDs."))?
                    .iter()
                    .map(|item| item.as_str().map(str::to_string).ok_or(format!("{what}.objectIds holds strings.")))
                    .collect::<Result<_, _>>()?,
            ),
            Value::Object(_) => None,
            _ => {
                return Err(format!(
                    "{what} must be an object ID, a list of IDs, {{objectIds}}, or {{asset | assets, matrix | frameOf}}."
                ))
            }
        };
        if let Some(mut ids) = ids {
            if ids.is_empty() || ids.len() > 64 {
                return Err(format!("{what} must name 1 to 64 objects."));
            }
            // Each object once, and the size checked before any mesh is copied.
            let mut seen = std::collections::HashSet::new();
            ids.retain(|id| seen.insert(id.clone()));
            let mut triangles = 0usize;
            for id in &ids {
                triangles += self.with_part(id, |part| part.mesh.triangles.len())?;
            }
            if triangles > MAX_SURFACE_TRIANGLES {
                return Err(format!("{what} has {triangles} triangles, over the {MAX_SURFACE_TRIANGLES} limit; name fewer objects."));
            }
            let mut meshes = Vec::new();
            for id in &ids {
                meshes.push(self.with_part(id, |part| meshanalysis::from_engine_mesh(&part.mesh))?);
            }
            let mesh = meshops::concat(meshes);
            check_size(&mesh, what)?;
            let lines: Vec<Value> = ids.iter().map(|id| self.object_lines(id)).collect();
            let label = spec.get("label").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| ids.join("+"));
            return Ok(Surface::new(label, "objects", mesh, json!({"objectIds": ids, "lines": lines})));
        }
        if let Some(names) = spec.get("assets").and_then(Value::as_array) {
            // Several assets in one pose, as one surface.
            if names.is_empty() || names.len() > 16 {
                return Err(format!("{what}.assets must list 1 to 16 asset names."));
            }
            let mut triangles = 0;
            for name in names {
                let name = name.as_str().ok_or(format!("{what}.assets holds asset names."))?;
                triangles += crate::assets::try_lookup(name)?.map_or(0, |asset| asset.mesh.triangles.len());
            }
            if triangles > MAX_SURFACE_TRIANGLES {
                return Err(format!("{what} has {triangles} triangles, over the {MAX_SURFACE_TRIANGLES} limit; select fewer assets."));
            }
            let mut parts = Vec::new();
            for name in names {
                let name = name.as_str().ok_or(format!("{what}.assets holds asset names."))?;
                let mut one = spec.clone();
                one.as_object_mut().map(|o| o.remove("assets"));
                one["asset"] = json!(name);
                parts.push(self.surface(&one, what)?);
            }
            let label = spec.get("label").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| parts.iter().map(|p| p.label.as_str()).collect::<Vec<_>>().join("+"));
            let sources: Vec<Value> = parts.iter().map(|p| p.source.clone()).collect();
            let observed = parts.iter().all(|p| p.observed);
            let scan = parts.first().and_then(|p| p.scan.clone()).filter(|first| parts.iter().all(|p| p.scan.as_ref() == Some(first)));
            let mesh = meshops::concat(parts.into_iter().map(|p| p.mesh));
            check_size(&mesh, what)?;
            let mut surface = Surface::new(label, "asset", mesh, json!({"assets": sources}));
            surface.observed = observed;
            surface.scan = scan;
            return Ok(surface);
        }
        let name = spec
            .get("asset")
            .and_then(Value::as_str)
            .ok_or(format!("{what} needs objectIds, asset (a design asset name such as \"scan.stl\") or assets."))?;
        let asset = crate::assets::try_lookup(name)?
            .ok_or_else(|| format!("{what}: {}", crate::assets::no_asset_message(name)))?;
        let mut mesh = (*asset.mesh).clone();
        let mut source = json!({"asset": name});
        let pose = match (spec.get("matrix").filter(|v| !v.is_null()), spec.get("frameOf").and_then(Value::as_str)) {
            (Some(_), Some(_)) => return Err(format!("{what}: give matrix or frameOf, not both.")),
            (Some(m), None) => Some(matrix(m)?),
            (None, Some(object)) => {
                let (m, residual, via) = self.frame_of(object, name, &mesh)?;
                source["frameOf"] = json!(object);
                source["frameResidual"] = json!(residual);
                if via != name {
                    source["frameVia"] = json!(via);
                }
                Some(m)
            }
            (None, None) => None,
        };
        if let Some(m) = pose {
            mesh = meshops::transform(&mesh, &m)?;
            source["matrix"] = rounded_matrix(&m);
        }
        check_size(&mesh, what)?;
        let label = spec.get("label").and_then(Value::as_str).unwrap_or(name).to_string();
        let (observed, scan, provenance) = self.provenance(name);
        source["provenance"] = json!(provenance);
        let mut surface = Surface::new(label, "asset", mesh, source);
        surface.observed = observed;
        surface.scan = scan;
        Ok(surface)
    }

    /// Whether asset `name` is measured scan data, the uploaded scan it
    /// comes from, and a one-word provenance.
    fn provenance(&self, name: &str) -> (bool, Option<String>, &'static str) {
        let Some(id) = crate::input_design_id(self.input) else { return (false, None, "unknown") };
        let Ok(metas) = self.state.workspaces.asset_list(id) else { return (false, None, "unknown") };
        let find = |n: &str| metas.iter().find(|meta| meta.name == n);
        match find(name) {
            Some(_) => (true, Some(name.to_string()), "upload"),
            None => (false, None, "unknown"),
        }
    }

    /// The matrix taking asset `name` to where design object `object` shows it: object must be that asset
    /// imported under transforms only. Returns (matrix, residual mm, asset
    /// the object imports).
    pub(crate) fn frame_of(&mut self, object: &str, name: &str, mesh: &TriMesh) -> Result<(Mat4, f64, String), String> {
        let part = self.part(object)?;
        let placed = meshanalysis::from_engine_mesh(&part.mesh);
        if let Some((m, residual)) = fit_frame(mesh, &placed) {
            return Ok((m, residual, name.to_string()));
        }
        // Re-meshed on import (the engine merges or re-triangulates): match
        // the shape itself before compiling the design again with a marker,
        // which a heavy model can take longer than the call's budget for.
        if let Some((m, residual)) = fit_frame_by_shape(mesh, &placed) {
            return Ok((m, residual, name.to_string()));
        }
        let asset = Arc::new(mesh.clone());
        if let Some((m, residual)) = self.frame_by_marker(object, name, &asset)? {
            return Ok((m, residual, name.to_string()));
        }
        Err(format!(
            "Object {object:?} is not {name:?} placed by transforms only, so its pose cannot be \
             recovered: pass matrix (the multmatrix the code applies) instead."
        ))
    }

    /// Recovers the pose of `import("name")` inside `object` by compiling
    /// the source with that import replaced by a small irregular polytope
    /// (in the asset's coordinates) and locating its vertices in the
    /// object's mesh: robust to the re-triangulation the mesher applies.
    fn frame_by_marker(&mut self, object: &str, name: &str, asset: &TriMesh) -> Result<Option<(Mat4, f64)>, String> {
        let (source, compiled) = self.compiled()?;
        let Some(position) = compiled.parts.iter().chain(compiled.background_parts.iter()).position(|part| part.id == object || part.name == object) else {
            return Ok(None);
        };
        let Some((lo, hi)) = meshops::bounds(asset) else { return Ok(None) };
        let code = source.source.clone();
        let Some((start, end, centered)) = find_import(&code, name) else { return Ok(None) };
        let size = sub(hi, lo).map(|v| v.max(1.0));
        // Six points in general position spanning the asset's box.
        let unit_points = [[0.05, 0.1, 0.15], [0.93, 0.2, 0.1], [0.15, 0.87, 0.3], [0.2, 0.3, 0.95], [0.8, 0.75, 0.6], [0.55, 0.12, 0.8]];
        let marker: Vec<P> = unit_points.iter().map(|u| [lo[0] + u[0] * size[0], lo[1] + u[1] * size[1], lo[2] + u[2] * size[2]]).collect();
        let hull_input = TriMesh { points: marker.clone(), triangles: vec![[0, 1, 2], [0, 1, 3], [0, 2, 3], [1, 2, 3], [0, 4, 5], [1, 4, 5]] };
        let Ok((hull, _)) = meshops::convex_hull(&hull_input) else { return Ok(None) };
        if hull.points.len() != marker.len() {
            return Ok(None);
        }
        let fmt = |p: &P| format!("[{},{},{}]", p[0], p[1], p[2]);
        let mut replacement = format!(
            "polyhedron(points=[{}], faces=[{}])",
            hull.points.iter().map(fmt).collect::<Vec<_>>().join(","),
            hull.triangles.iter().map(|t| format!("[{},{},{}]", t[0], t[1], t[2])).collect::<Vec<_>>().join(",")
        );
        if centered {
            let mid = scale(add(lo, hi), 0.5);
            replacement = format!("translate({}) {replacement}", fmt(&scale(mid, -1.0)));
        }
        let modified = format!("{}{}{}", &code[..start], replacement, &code[end..]);
        let mut input = json!({"code": modified});
        if let Some(id) = crate::input_design_id(self.input) {
            input["designId"] = json!(id);
        }
        let Ok(probe_source) = agent::eval_source(self.state, None, &input) else { return Ok(None) };
        let Ok(probe) = agent::compile_eval(&probe_source) else { return Ok(None) };
        let Some(part) = probe.parts.iter().chain(probe.background_parts.iter()).nth(position) else { return Ok(None) };
        let placed = meshanalysis::from_engine_mesh(&part.mesh);
        // The object should now be just the marker (6 vertices); more means
        // other geometry shares the object, and the search below is
        // O(n^4), so give up rather than guess.
        if placed.points.len() > 12 {
            return Ok(None);
        }
        let marker = hull.points;
        let span = norm(sub(hi, lo)).max(1.0);
        let n = placed.points.len();
        // Try images of the first four marker points; the right assignment
        // maps every other marker point onto a placed vertex.
        for a in 0..n {
            for b in 0..n {
                for c in 0..n {
                    for d in 0..n {
                        if a == b || a == c || a == d || b == c || b == d || c == d {
                            continue;
                        }
                        let images = [placed.points[a], placed.points[b], placed.points[c], placed.points[d]];
                        let Some(m) = affine_from_four(&[marker[0], marker[1], marker[2], marker[3]], &images) else { continue };
                        let mut worst: f64 = 0.0;
                        let fits = marker.iter().all(|p| {
                            let q = meshops::apply(&m, *p);
                            let best = placed.points.iter().map(|v| norm(sub(*v, q))).fold(f64::INFINITY, f64::min);
                            worst = worst.max(best);
                            best <= 1e-5 * span.max(norm(sub(images[1], images[0])))
                        });
                        if fits {
                            return Ok(Some((m, round_to(worst, 6))));
                        }
                    }
                }
            }
        }
        Ok(None)
    }
}

/// Byte span of the first `import("name" ...)` call in `code` and whether
/// it passes center = true.
fn find_import(code: &str, name: &str) -> Option<(usize, usize, bool)> {
    let bytes = code.as_bytes();
    let mut from = 0;
    while let Some(offset) = code[from..].find("import") {
        let start = from + offset;
        from = start + 6;
        if start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
            continue;
        }
        let mut i = from;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if bytes.get(i) != Some(&b'(') {
            continue;
        }
        let mut depth = 0;
        let mut in_string = false;
        let mut end = None;
        let mut j = i;
        while j < bytes.len() {
            let c = bytes[j];
            if in_string {
                if c == b'\\' {
                    j += 1;
                } else if c == b'"' {
                    in_string = false;
                }
            } else if c == b'"' {
                in_string = true;
            } else if c == b'(' {
                depth += 1;
            } else if c == b')' {
                depth -= 1;
                if depth == 0 {
                    end = Some(j + 1);
                    break;
                }
            }
            j += 1;
        }
        let end = end?;
        let args = &code[i..end];
        let quoted = format!("\"{name}\"");
        let dotted = format!("\"./{name}\"");
        if args.contains(&quoted) || args.contains(&dotted) {
            let compact: String = args.chars().filter(|c| !c.is_whitespace()).collect();
            return Some((start, end, compact.contains("center=true")));
        }
    }
    None
}

/// The affine map taking four points onto four images; `None` when the
/// sources are coplanar.
fn affine_from_four(from: &[P; 4], to: &[P; 4]) -> Option<Mat4> {
    let basis = [sub(from[1], from[0]), sub(from[2], from[0]), sub(from[3], from[0])];
    let images = [sub(to[1], to[0]), sub(to[2], to[0]), sub(to[3], to[0])];
    // Columns of A = basis; M3 * A = B  =>  M3 = B * A^-1.
    let a: Mat4 = [
        [basis[0][0], basis[1][0], basis[2][0], 0.0],
        [basis[0][1], basis[1][1], basis[2][1], 0.0],
        [basis[0][2], basis[1][2], basis[2][2], 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let inv = invert_affine(&a)?;
    let mut m = [[0.0; 4]; 4];
    for r in 0..3 {
        for c in 0..3 {
            m[r][c] = (0..3).map(|k| images[k][r] * inv[k][c]).sum();
        }
    }
    let t = sub(to[0], [dot([m[0][0], m[0][1], m[0][2]], from[0]), dot([m[1][0], m[1][1], m[1][2]], from[0]), dot([m[2][0], m[2][1], m[2][2]], from[0])]);
    for r in 0..3 {
        m[r][3] = t[r];
    }
    m[3][3] = 1.0;
    Some(m)
}

fn check_size(mesh: &TriMesh, what: &str) -> Result<(), String> {
    if mesh.triangles.is_empty() {
        return Err(format!("{what} has no triangles."));
    }
    if mesh.triangles.len() > MAX_SURFACE_TRIANGLES {
        return Err(format!(
            "{what} has {} triangles, over the {MAX_SURFACE_TRIANGLES} limit; use a lower $fn for a model, or a smaller budget when uploading a scan.",
            mesh.triangles.len()
        ));
    }
    Ok(())
}

/// The faces of `mesh` where `keep` is true, with only the vertices they use.
pub(crate) fn keep_faces(mesh: &TriMesh, keep: &[bool]) -> TriMesh {
    let mut remap: HashMap<u32, u32> = HashMap::new();
    let mut out = TriMesh::default();
    for (t, tri) in mesh.triangles.iter().enumerate() {
        if !keep[t] {
            continue;
        }
        out.triangles.push(tri.map(|v| {
            *remap.entry(v).or_insert_with(|| {
                out.points.push(mesh.points[v as usize]);
                (out.points.len() - 1) as u32
            })
        }));
    }
    out
}

/// Eigenvectors of a symmetric 3x3 matrix, by Jacobi rotation, sorted by
/// eigenvalue (largest first).
pub(crate) fn eigen3(mut a: [[f64; 3]; 3]) -> ([f64; 3], [P; 3]) {
    let mut v = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    for _ in 0..64 {
        let (mut p, mut q, mut largest) = (0, 1, 0.0);
        for i in 0..3 {
            for j in i + 1..3 {
                if a[i][j].abs() > largest {
                    largest = a[i][j].abs();
                    p = i;
                    q = j;
                }
            }
        }
        if largest < 1e-15 {
            break;
        }
        let theta = 0.5 * (2.0 * a[p][q]).atan2(a[q][q] - a[p][p]);
        let (c, s) = (theta.cos(), theta.sin());
        for k in 0..3 {
            let (akp, akq) = (a[k][p], a[k][q]);
            a[k][p] = c * akp - s * akq;
            a[k][q] = s * akp + c * akq;
        }
        for k in 0..3 {
            let (apk, aqk) = (a[p][k], a[q][k]);
            a[p][k] = c * apk - s * aqk;
            a[q][k] = s * apk + c * aqk;
        }
        for row in v.iter_mut() {
            let (vp, vq) = (row[p], row[q]);
            row[p] = c * vp - s * vq;
            row[q] = s * vp + c * vq;
        }
    }
    let mut order = [0, 1, 2];
    order.sort_by(|&i, &j| a[j][j].total_cmp(&a[i][i]));
    let values = order.map(|i| a[i][i]);
    let vectors = order.map(|i| unit([v[0][i], v[1][i], v[2][i]]));
    (values, vectors)
}

/// The similarity (rotation or mirror, uniform scale, translation) taking
/// `asset` onto `placed` when `placed` is the same surface however it was
/// re-meshed: principal axes matched under each sign choice, accepted when
/// every checked asset vertex lies on `placed` (within 1e-5 of its size).
/// Returns the matrix and the largest residual; `None` for a symmetric shape
/// (axes ambiguous, or more than one sign choice fits), or a different surface.
pub(crate) fn fit_frame_by_shape(asset: &TriMesh, placed: &TriMesh) -> Option<(Mat4, f64)> {
    fn moments(mesh: &TriMesh) -> Option<(P, [f64; 3], [P; 3])> {
        let used: Vec<P> = {
            let mut seen = vec![false; mesh.points.len()];
            for tri in &mesh.triangles {
                for &v in tri {
                    seen[v as usize] = true;
                }
            }
            mesh.points.iter().zip(&seen).filter(|(_, s)| **s).map(|(p, _)| *p).collect()
        };
        if used.len() < 4 {
            return None;
        }
        let center = scale(used.iter().fold([0.0; 3], |acc, p| add(acc, *p)), 1.0 / used.len() as f64);
        let mut cov = [[0.0; 3]; 3];
        for p in &used {
            let d = sub(*p, center);
            for i in 0..3 {
                for j in 0..3 {
                    cov[i][j] += d[i] * d[j] / used.len() as f64;
                }
            }
        }
        let (values, axes) = eigen3(cov);
        Some((center, values, axes))
    }
    let (ca, va, aa) = moments(asset)?;
    let (cp, vp, ap) = moments(placed)?;
    let trace_a: f64 = va.iter().sum();
    if !(trace_a > 0.0) {
        return None;
    }
    let s = (vp.iter().sum::<f64>() / trace_a).sqrt();
    // Near-equal principal moments leave the axes undetermined.
    if (0..2).any(|i| (va[i] - va[i + 1]).abs() <= 1e-6 * trace_a) {
        return None;
    }
    let size = meshops::bounds(placed).map_or(1.0, |(lo, hi)| norm(sub(hi, lo)).max(1e-9));
    let limit = 1e-5 * size;
    let index = MeshIndex::new(placed);
    let step = (asset.points.len() / 300).max(1);
    let mut accepted: Vec<(Mat4, f64)> = Vec::new();
    for signs in 0..8u32 {
        let flip = |k: usize| if signs & (1 << k) != 0 { -1.0 } else { 1.0 };
        // R maps asset axis k onto placed axis k (times its sign).
        let mut r = [[0.0; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                r[i][j] = (0..3).map(|k| flip(k) * ap[k][i] * aa[k][j]).sum();
            }
        }
        let mut m: Mat4 = [[0.0; 4]; 4];
        for i in 0..3 {
            for j in 0..3 {
                m[i][j] = s * r[i][j];
            }
            m[i][3] = cp[i] - (0..3).map(|j| s * r[i][j] * ca[j]).sum::<f64>();
        }
        m[3][3] = 1.0;
        let mut worst: f64 = 0.0;
        for p in asset.points.iter().step_by(step) {
            worst = worst.max(index.distance(meshops::apply(&m, *p)));
            if worst > limit {
                break;
            }
        }
        if worst <= limit {
            accepted.push((m, worst));
        }
    }
    // A shape that lands on itself under another sign choice (a rectangle
    // turned half way round) does not tell which pose is the real one:
    // leave it to the marker recovery rather than guess.
    match accepted.as_slice() {
        [only] => Some(*only),
        _ => None,
    }
}

/// Least-squares affine map from `asset` to `placed` when `placed` is the
/// asset's triangles in the same order under one affine transform (an
/// import under translate/rotate/scale/multmatrix). Returns the matrix and
/// the largest vertex residual, `None` when they do not correspond.
pub(crate) fn fit_frame(asset: &TriMesh, placed: &TriMesh) -> Option<(Mat4, f64)> {
    if asset.triangles.len() != placed.triangles.len() || asset.triangles.is_empty() {
        return None;
    }
    // A mirror flips winding: corner k of a placed triangle may be corner
    // 2 - k of the asset's. Try both correspondences.
    let mut best: Option<(Mat4, f64)> = None;
    for flipped in [false, true] {
        let pair = |t: usize, k: usize| {
            let a = asset.points[asset.triangles[t][if flipped { (3 - k) % 3 } else { k }] as usize];
            let b = placed.points[placed.triangles[t][k] as usize];
            (a, b)
        };
        let count = asset.triangles.len();
        let step = (count / 4000).max(1);
        // Normal equations for each output row: [x y z 1] . row = target.
        let mut ata = [[0.0f64; 4]; 4];
        let mut atb = [[0.0f64; 4]; 3];
        let mut t = 0;
        while t < count {
            for k in 0..3 {
                let (a, b) = pair(t, k);
                let x = [a[0], a[1], a[2], 1.0];
                for i in 0..4 {
                    for j in 0..4 {
                        ata[i][j] += x[i] * x[j];
                    }
                    for r in 0..3 {
                        atb[r][i] += x[i] * b[r];
                    }
                }
            }
            t += step;
        }
        let Some(rows) = (0..3).map(|r| solve4(ata, atb[r])).collect::<Option<Vec<_>>>() else { continue };
        let m: Mat4 = [rows[0], rows[1], rows[2], [0.0, 0.0, 0.0, 1.0]];
        let mut residual: f64 = 0.0;
        let check_step = (count / 20000).max(1);
        let mut t = 0;
        while t < count {
            for k in 0..3 {
                let (a, b) = pair(t, k);
                residual = residual.max(norm(sub(meshops::apply(&m, a), b)));
            }
            t += check_step;
        }
        let size = meshops::bounds(placed).map_or(1.0, |(lo, hi)| norm(sub(hi, lo)).max(1.0));
        if residual <= 1e-4 * size && best.as_ref().is_none_or(|(_, r)| residual < *r) {
            best = Some((m, round_to(residual, 6)));
        }
    }
    best
}

fn solve4(mut a: [[f64; 4]; 4], mut b: [f64; 4]) -> Option<[f64; 4]> {
    for col in 0..4 {
        let pivot = (col..4).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
        if a[pivot][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, pivot);
        b.swap(col, pivot);
        for row in 0..4 {
            if row != col {
                let f = a[row][col] / a[col][col];
                for k in col..4 {
                    a[row][k] -= f * a[col][k];
                }
                b[row] -= f * b[col];
            }
        }
    }
    Some([b[0] / a[0][0], b[1] / a[1][1], b[2] / a[2][2], b[3] / a[3][3]])
}

// ---------------------------------------------------------------------------
// Sampling
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub(crate) struct Sample {
    pub point: P,
    pub normal: P,
    pub tri: usize,
    /// Surface area this sample stands for, mm^2.
    pub weight: f64,
}

/// The `within` argument: an axis-aligned box a tool keeps to, so one call
/// answers about one feature. Same coordinates as the tool's other arguments.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Within {
    pub min: P,
    pub max: P,
}

/// How much of one surface lies inside a box.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct WithinStats {
    /// Faces at least partly inside.
    pub triangles: usize,
    pub area: f64,
    pub total_area: f64,
}

impl WithinStats {
    pub(crate) fn public(&self) -> Value {
        json!({
            "triangles": self.triangles,
            "area": round_to(self.area, 3),
            "areaFraction": round_to(self.area / self.total_area.max(1e-12), 4),
        })
    }
}

/// How much of a cut's contours lies inside a box.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct WithinLength {
    pub inside: f64,
    pub total: f64,
    /// Closed loops the box cut open.
    pub loops_cut: usize,
}

impl WithinLength {
    pub(crate) fn add(&mut self, other: WithinLength) {
        self.inside += other.inside;
        self.total += other.total;
        self.loops_cut += other.loops_cut;
    }

    pub(crate) fn public(&self) -> Value {
        json!({
            "contourLength": round_to(self.inside, 3),
            "contourFraction": round_to(self.inside / self.total.max(1e-12), 4),
            "loopsCut": self.loops_cut,
        })
    }
}

/// The frame of a `within` given with surfaces.
pub(crate) const SURFACE_FRAME: &str = "design coordinates, surfaces as posed by frameOf or matrix (with neither: the asset's own)";

/// The `detail` argument of the scan tools.
pub(crate) fn detail_schema() -> Value {
    json!({"type": "boolean", "default": false, "description": "Also return console, histograms, cameras, knots, pose matrices, every attempt."})
}

/// A surface argument of the scan tools.
pub(crate) fn surface_schema() -> Value {
    json!({
        "description": "An object ID, ID list, {objectIds}, or {asset or assets, frameOf: object importing the scan | matrix: row-major 4x4}; optional label.",
        "anyOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}, {"type": "object"}]
    })
}

/// The schema of `within`; `coordinates` says whose.
pub(crate) fn within_schema(coordinates: &str) -> Value {
    let corner = json!({"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3});
    json!({
        "type": "object",
        "required": ["min", "max"],
        "properties": {"min": corner.clone(), "max": corner},
        "description": format!("Axis-aligned box, min below max on every axis, in {coordinates}. The reply states how much was inside; nothing inside returns nothingInside, never a pass."),
    })
}

impl Within {
    /// `within` of a tool's input, validated; `None` when absent.
    pub(crate) fn parse(input: &Value) -> Result<Option<Self>, String> {
        let Some(value) = input.get("within").filter(|value| !value.is_null()) else { return Ok(None) };
        const SHAPE: &str = "within must be a box {min: [x, y, z], max: [x, y, z]}";
        if !value.is_object() {
            return Err(format!("{SHAPE}."));
        }
        let corner = |key: &str| -> Result<P, String> {
            let items = value.get(key).and_then(Value::as_array).filter(|items| items.len() == 3).ok_or_else(|| format!("{SHAPE}: {key} needs three numbers."))?;
            let mut out = [0.0; 3];
            for (slot, item) in out.iter_mut().zip(items) {
                *slot = item.as_f64().filter(|v| v.is_finite()).ok_or_else(|| format!("{SHAPE}: {key} needs three finite numbers."))?;
            }
            Ok(out)
        };
        let (min, max) = (corner("min")?, corner("max")?);
        for (k, axis) in ["x", "y", "z"].iter().enumerate() {
            if min[k] >= max[k] {
                return Err(format!("within is an empty box: min {axis} ({}) must be below max {axis} ({}).", min[k], max[k]));
            }
        }
        Ok(Some(Self { min, max }))
    }

    pub(crate) fn contains(&self, p: P) -> bool {
        (0..3).all(|k| p[k] >= self.min[k] && p[k] <= self.max[k])
    }

    /// The part of a triangle inside the box, as a convex polygon (empty when
    /// it misses it).
    pub(crate) fn clip_triangle(&self, corners: [P; 3]) -> Vec<P> {
        let lo = corners.iter().fold(corners[0], |acc, p| [acc[0].min(p[0]), acc[1].min(p[1]), acc[2].min(p[2])]);
        let hi = corners.iter().fold(corners[0], |acc, p| [acc[0].max(p[0]), acc[1].max(p[1]), acc[2].max(p[2])]);
        if (0..3).any(|k| hi[k] < self.min[k] || lo[k] > self.max[k]) {
            return Vec::new();
        }
        let mut polygon = corners.to_vec();
        for k in 0..3 {
            for (bound, keep_above) in [(self.min[k], true), (self.max[k], false)] {
                let inside = |p: &P| if keep_above { p[k] >= bound } else { p[k] <= bound };
                let mut next = Vec::with_capacity(polygon.len() + 1);
                for i in 0..polygon.len() {
                    let (a, b) = (polygon[i], polygon[(i + 1) % polygon.len()]);
                    if inside(&a) {
                        next.push(a);
                    }
                    if inside(&a) != inside(&b) {
                        let t = (bound - a[k]) / (b[k] - a[k]);
                        next.push(add(a, scale(sub(b, a), t)));
                    }
                }
                polygon = next;
                if polygon.is_empty() {
                    return polygon;
                }
            }
        }
        polygon
    }

    /// The area of a triangle inside the box.
    pub(crate) fn clipped_area(&self, corners: [P; 3]) -> f64 {
        let polygon = self.clip_triangle(corners);
        (1..polygon.len().saturating_sub(1)).map(|k| 0.5 * norm(cross(sub(polygon[k], polygon[0]), sub(polygon[k + 1], polygon[0])))).sum()
    }

    /// Per face of `mesh`, whether any of it lies inside, and the area inside.
    fn faces(&self, mesh: &TriMesh) -> (Vec<bool>, f64) {
        let mut inside = 0.0;
        let keep = (0..mesh.triangles.len())
            .map(|t| {
                let area = self.clipped_area(corners(mesh, t));
                inside += area;
                area > 0.0
            })
            .collect();
        (keep, inside)
    }

    /// How much of `mesh` lies inside.
    pub(crate) fn stats(&self, mesh: &TriMesh) -> WithinStats {
        let (keep, area) = self.faces(mesh);
        let total_area = (0..mesh.triangles.len()).map(|t| face_normal_area(mesh, t).1).sum();
        WithinStats { triangles: keep.iter().filter(|&&k| k).count(), area, total_area }
    }

    /// The whole faces of `mesh` that are at least partly inside: for tools
    /// that work on a mesh (a fit, a description), on dense scans this is the
    /// box to within one face.
    pub(crate) fn crop(&self, mesh: &TriMesh) -> TriMesh {
        keep_faces(mesh, &self.faces(mesh).0)
    }

    /// Greys the faces no part of which is inside.
    pub(crate) fn grey_outside(&self, mesh: &TriMesh, colors: &mut [Rgba]) {
        for (color, keep) in colors.iter_mut().zip(self.faces(mesh).0) {
            if !keep {
                *color = UNSUPPORTED_GREY;
            }
        }
    }

    /// The box as a reply field, with what was inside merged in.
    pub(crate) fn public(&self, extra: Value) -> Value {
        let mut value = json!({"min": r3(self.min), "max": r3(self.max)});
        if let (Some(base), Value::Object(extra)) = (value.as_object_mut(), extra) {
            base.extend(extra);
        }
        value
    }

    /// The part of the segment `a`-`b` inside, as parameters along it.
    fn clip_segment(&self, a: P, b: P) -> Option<(f64, f64)> {
        let (mut t0, mut t1) = (0.0f64, 1.0f64);
        for k in 0..3 {
            let d = b[k] - a[k];
            if d.abs() < 1e-15 {
                if a[k] < self.min[k] || a[k] > self.max[k] {
                    return None;
                }
                continue;
            }
            let (near, far) = ((self.min[k] - a[k]) / d, (self.max[k] - a[k]) / d);
            let (near, far) = if near <= far { (near, far) } else { (far, near) };
            t0 = t0.max(near);
            t1 = t1.min(far);
            if t0 > t1 {
                return None;
            }
        }
        Some((t0, t1))
    }

    /// Keeps the part of a cut's contours inside the box. A loop entirely
    /// inside stays closed with its area; one crossing a wall becomes open
    /// chains with no area, so the section's area counts whole loops only.
    pub(crate) fn clip_section(&self, section: &mut crate::meshanalysis::Section) -> WithinLength {
        let (origin, u, v) = (section.origin, section.u, section.v);
        let to3 = |p: [f64; 2]| add(origin, add(scale(u, p[0]), scale(v, p[1])));
        let to2 = |p: P| [dot(sub(p, origin), u), dot(sub(p, origin), v)];
        let mut length = WithinLength::default();
        let mut kept = Vec::new();
        let mut all_whole = true;
        for path in std::mem::take(&mut section.loops) {
            let points: Vec<P> = path.points.iter().map(|p| to3(*p)).collect();
            let segments = path.points.len().saturating_sub(1) + usize::from(path.closed && path.points.len() > 1);
            let segment = |i: usize| (points[i], points[(i + 1) % points.len()]);
            let path_length: f64 = (0..segments).map(|i| { let (a, b) = segment(i); norm(sub(b, a)) }).sum();
            length.total += path_length;
            if points.iter().all(|p| self.contains(*p)) {
                length.inside += path_length;
                kept.push(path);
                continue;
            }
            all_whole = false;
            let mut chains: Vec<Vec<P>> = Vec::new();
            let mut open = false;
            for i in 0..segments {
                let (a, b) = segment(i);
                let Some((t0, t1)) = self.clip_segment(a, b) else {
                    open = false;
                    continue;
                };
                let (start, end) = (add(a, scale(sub(b, a), t0)), add(a, scale(sub(b, a), t1)));
                length.inside += norm(sub(end, start));
                if !open {
                    chains.push(vec![start]);
                }
                if let Some(chain) = chains.last_mut() {
                    chain.push(end);
                }
                open = t1 >= 1.0;
            }
            if path.closed {
                // Started inside: the first chain continues the last one.
                if chains.len() > 1 && open && chains[0][0] == points[0] {
                    let first = chains.remove(0);
                    if let Some(last) = chains.last_mut() {
                        last.extend(first.into_iter().skip(1));
                    }
                }
                if !chains.is_empty() {
                    length.loops_cut += 1;
                }
            }
            for chain in chains.into_iter().filter(|chain| chain.len() >= 2) {
                let count = chain.len() as f64;
                let mean = scale(chain.iter().fold([0.0; 3], |acc, p| add(acc, *p)), 1.0 / count);
                kept.push(crate::meshanalysis::SectionLoop {
                    points: chain.iter().map(|p| to2(*p)).collect(),
                    closed: false,
                    area: 0.0,
                    hole: false,
                    centroid: to2(mean),
                    centroid3d: mean,
                });
            }
        }
        section.loops = kept;
        if !all_whole {
            section.area = section.loops.iter().map(|path| path.area).sum();
            section.centroid = None;
        }
        length
    }
}

/// About `target` points spread over the surface in proportion to area,
/// deterministic for a given mesh; each weighs total area / samples, so the
/// weights sum to the area.
pub(crate) fn sample_surface(mesh: &TriMesh, target: usize) -> Vec<Sample> {
    sample_surface_in(mesh, target, None)
}

/// [`sample_surface`] of what lies inside `within`, cutting faces at its walls:
/// the weights sum to the area inside, and every point is inside. A sample's
/// `tri` stays the face of `mesh` it lies on.
pub(crate) fn sample_surface_in(mesh: &TriMesh, target: usize, within: Option<&Within>) -> Vec<Sample> {
    let faces: Vec<(usize, P, f64, [P; 3])> = match within {
        None => (0..mesh.triangles.len())
            .map(|t| {
                let (normal, area) = face_normal_area(mesh, t);
                (t, normal, area, corners(mesh, t))
            })
            .collect(),
        Some(within) => {
            let mut pieces = Vec::new();
            for t in 0..mesh.triangles.len() {
                let (normal, _) = face_normal_area(mesh, t);
                let polygon = within.clip_triangle(corners(mesh, t));
                for k in 1..polygon.len().saturating_sub(1) {
                    let piece = [polygon[0], polygon[k], polygon[k + 1]];
                    let area = 0.5 * norm(cross(sub(piece[1], piece[0]), sub(piece[2], piece[0])));
                    pieces.push((t, normal, area, piece));
                }
            }
            pieces
        }
    };
    let total: f64 = faces.iter().map(|f| f.2).sum();
    if !(total > 0.0) || target == 0 {
        return Vec::new();
    }
    let per_area = target as f64 / total;
    let mut samples = Vec::with_capacity(target + faces.len().min(target));
    let mut carry = 0.0;
    for &(t, normal, area, [a, b, c]) in &faces {
        if !(area > 0.0) {
            continue;
        }
        carry += area * per_area;
        let count = carry.floor() as usize;
        carry -= count as f64;
        if count == 0 {
            continue;
        }
        for k in 0..count {
            // Low-discrepancy points in the triangle (R2 sequence, folded).
            let (mut u, mut v) = if count == 1 {
                (1.0 / 3.0, 1.0 / 3.0)
            } else {
                let i = k as f64 + 0.5;
                ((i * 0.754_877_666_246_692_7).fract(), (i * 0.569_840_290_998_053_3).fract())
            };
            if u + v > 1.0 {
                u = 1.0 - u;
                v = 1.0 - v;
            }
            let point = add(a, add(scale(sub(b, a), u), scale(sub(c, a), v)));
            samples.push(Sample { point, normal, tri: t, weight: 0.0 });
        }
    }
    // Triangles are picked in proportion to their area, so every sample
    // stands for the same share of it (weighting by the picked triangle's
    // own area would count big triangles twice and drop the area of the
    // unpicked small ones).
    let weight = total / samples.len().max(1) as f64;
    for sample in &mut samples {
        sample.weight = weight;
    }
    samples
}

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

/// Area-weighted statistics of absolute deviations.
#[derive(Clone, Debug, Default)]
pub(crate) struct Stats {
    pub count: usize,
    pub area: f64,
    pub mean: f64,
    pub signed_mean: f64,
    pub rms: f64,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

impl Stats {
    /// From (signed value, weight) pairs; quantiles of |value|.
    pub(crate) fn of(values: &[(f64, f64)]) -> Self {
        let mut items: Vec<(f64, f64)> = values.iter().filter(|(v, w)| v.is_finite() && *w > 0.0).map(|(v, w)| (*v, *w)).collect();
        let area: f64 = items.iter().map(|(_, w)| w).sum();
        if items.is_empty() || !(area > 0.0) {
            return Self::default();
        }
        let mean = items.iter().map(|(v, w)| v.abs() * w).sum::<f64>() / area;
        let signed_mean = items.iter().map(|(v, w)| v * w).sum::<f64>() / area;
        let rms = (items.iter().map(|(v, w)| v * v * w).sum::<f64>() / area).sqrt();
        items.sort_by(|a, b| a.0.abs().total_cmp(&b.0.abs()));
        let quantile = |q: f64| {
            let mut acc = 0.0;
            for (v, w) in &items {
                acc += w;
                if acc >= q * area {
                    return v.abs();
                }
            }
            items.last().map_or(0.0, |(v, _)| v.abs())
        };
        Self {
            count: items.len(),
            area,
            mean,
            signed_mean,
            rms,
            p50: quantile(0.5),
            p95: quantile(0.95),
            max: items.last().map_or(0.0, |(v, _)| v.abs()),
        }
    }

    pub(crate) fn public(&self) -> Value {
        json!({
            "samples": self.count,
            "area": round_to(self.area, 2),
            "mean": round_to(self.mean, 4),
            "signedMean": round_to(self.signed_mean, 4),
            "rms": round_to(self.rms, 4),
            "p50": round_to(self.p50, 4),
            "p95": round_to(self.p95, 4),
            "max": round_to(self.max, 4),
        })
    }
}

/// Histogram of |values| (area fractions) over fixed bin edges.
pub(crate) fn histogram(values: &[(f64, f64)], edges: &[f64]) -> Value {
    let total: f64 = values.iter().map(|(_, w)| w).sum();
    let mut bins = vec![0.0; edges.len() + 1];
    for (v, w) in values {
        let slot = edges.iter().position(|edge| v.abs() < *edge).unwrap_or(edges.len());
        bins[slot] += w;
    }
    let mut out = Vec::new();
    let mut lower = 0.0;
    for (i, area) in bins.iter().enumerate() {
        let upper = edges.get(i).copied();
        out.push(json!({
            "from": round_to(lower, 4),
            "to": upper.map(|u| round_to(u, 4)),
            "fraction": if total > 0.0 { round_to(area / total, 4) } else { 0.0 },
        }));
        lower = upper.unwrap_or(lower);
    }
    json!(out)
}

// ---------------------------------------------------------------------------
// Spatial clustering of flagged samples
// ---------------------------------------------------------------------------

/// Connected groups of points (6-neighbour voxels of size `cell`); returns
/// index lists, largest total weight first.
pub(crate) fn cluster_points(points: &[(P, f64)], cell: f64) -> Vec<Vec<usize>> {
    if points.is_empty() || !(cell > 0.0) {
        return Vec::new();
    }
    let key = |p: P| [(p[0] / cell).floor() as i64, (p[1] / cell).floor() as i64, (p[2] / cell).floor() as i64];
    let mut cells: HashMap<[i64; 3], Vec<usize>> = HashMap::new();
    for (i, (p, _)) in points.iter().enumerate() {
        cells.entry(key(*p)).or_default().push(i);
    }
    let mut seen: HashMap<[i64; 3], bool> = HashMap::new();
    let mut groups = Vec::new();
    for start in cells.keys().copied().collect::<Vec<_>>() {
        if seen.contains_key(&start) {
            continue;
        }
        let mut group = Vec::new();
        let mut stack = vec![start];
        seen.insert(start, true);
        while let Some(c) = stack.pop() {
            group.extend(cells[&c].iter().copied());
            for dx in -1..=1 {
                for dy in -1..=1 {
                    for dz in -1..=1 {
                        let n = [c[0] + dx, c[1] + dy, c[2] + dz];
                        if cells.contains_key(&n) && !seen.contains_key(&n) {
                            seen.insert(n, true);
                            stack.push(n);
                        }
                    }
                }
            }
        }
        groups.push(group);
    }
    let weight = |g: &Vec<usize>| g.iter().map(|&i| points[i].1).sum::<f64>();
    groups.sort_by(|a, b| weight(b).total_cmp(&weight(a)));
    groups
}

/// How far apart, in degrees, the mean normals of neighbouring patches of one
/// region may turn, and how far any patch may turn from the region's first.
const REGION_CREASE_DEG: f64 = 35.0;
const REGION_SPREAD_DEG: f64 = 55.0;
/// Samples of one grid cell facing further apart than this are separate patches.
const CELL_NORMAL_DEG: f64 = 45.0;

/// Groups samples `(point, normal, weight)` into regions that each name one
/// wall, face or rim: neighbouring cells of size `cell`, split where the
/// surface creases (and, so a rounded edge or a cylinder does not run on
/// forever, where it has turned too far from where the region began). Largest
/// region first.
pub(crate) fn surface_regions(samples: &[(P, P, f64)], cell: f64) -> Vec<Vec<usize>> {
    if samples.is_empty() || !(cell > 0.0) {
        return Vec::new();
    }
    struct Patch {
        cell: [i64; 3],
        normal: P,
        area: f64,
        members: Vec<usize>,
    }
    let key = |p: P| [(p[0] / cell).floor() as i64, (p[1] / cell).floor() as i64, (p[2] / cell).floor() as i64];
    let same_cell = CELL_NORMAL_DEG.to_radians().cos();
    let mut patches: Vec<Patch> = Vec::new();
    let mut by_cell: HashMap<[i64; 3], Vec<usize>> = HashMap::new();
    for (i, &(point, normal, weight)) in samples.iter().enumerate() {
        let at = key(point);
        let slots = by_cell.entry(at).or_default();
        let found = slots.iter().copied().find(|&slot| dot(unit(patches[slot].normal), normal) >= same_cell);
        let slot = found.unwrap_or_else(|| {
            patches.push(Patch { cell: at, normal: [0.0; 3], area: 0.0, members: Vec::new() });
            slots.push(patches.len() - 1);
            patches.len() - 1
        });
        let patch = &mut patches[slot];
        // Weighted by area; a weightless sample still counts as a direction.
        patch.normal = add(patch.normal, scale(normal, weight.max(1e-12)));
        patch.area += weight;
        patch.members.push(i);
    }
    // Largest patches first, so a region starts in the middle of a wall.
    let mut order: Vec<usize> = (0..patches.len()).collect();
    order.sort_by(|&a, &b| patches[b].area.total_cmp(&patches[a].area).then(patches[a].cell.cmp(&patches[b].cell)));
    let (crease, spread) = (REGION_CREASE_DEG.to_radians().cos(), REGION_SPREAD_DEG.to_radians().cos());
    let mut taken = vec![false; patches.len()];
    let mut regions = Vec::new();
    for &start in &order {
        if taken[start] {
            continue;
        }
        taken[start] = true;
        let seed = unit(patches[start].normal);
        let mut stack = vec![start];
        let mut region = Vec::new();
        while let Some(at) = stack.pop() {
            region.extend(patches[at].members.iter().copied());
            let here = unit(patches[at].normal);
            let c = patches[at].cell;
            for dx in -1..=1 {
                for dy in -1..=1 {
                    for dz in -1..=1 {
                        for &next in by_cell.get(&[c[0] + dx, c[1] + dy, c[2] + dz]).map_or(&[][..], |slots| slots.as_slice()) {
                            let normal = unit(patches[next].normal);
                            if !taken[next] && dot(here, normal) >= crease && dot(seed, normal) >= spread {
                                taken[next] = true;
                                stack.push(next);
                            }
                        }
                    }
                }
            }
        }
        regions.push(region);
    }
    let weight = |g: &Vec<usize>| g.iter().map(|&i| samples[i].2).sum::<f64>();
    regions.sort_by(|a, b| weight(b).total_cmp(&weight(a)));
    regions
}

// ---------------------------------------------------------------------------
// Colours
// ---------------------------------------------------------------------------

pub(crate) const UNSUPPORTED_GREY: Rgba = [170, 170, 170, 255];
pub(crate) const WRONG_SIDE_PURPLE: Rgba = [150, 60, 200, 255];

fn lerp_rgb(a: [f64; 3], b: [f64; 3], t: f64) -> Rgba {
    let t = t.clamp(0.0, 1.0);
    [
        (a[0] + (b[0] - a[0]) * t).round() as u8,
        (a[1] + (b[1] - a[1]) * t).round() as u8,
        (a[2] + (b[2] - a[2]) * t).round() as u8,
        255,
    ]
}

/// Diverging blue (negative) - near-white (0) - red (positive), saturated
/// at |value| = range; values within `dead` of 0 are pale green ("fits").
pub(crate) fn diverging(value: f64, range: f64, dead: f64) -> Rgba {
    if !value.is_finite() {
        return UNSUPPORTED_GREY;
    }
    if value.abs() <= dead {
        return [120, 200, 120, 255];
    }
    let t = ((value.abs() - dead) / (range - dead).max(1e-9)).clamp(0.0, 1.0);
    let pale = [235.0, 235.0, 225.0];
    if value > 0.0 {
        lerp_rgb(pale, [200.0, 30.0, 30.0], t.sqrt())
    } else {
        lerp_rgb(pale, [30.0, 70.0, 200.0], t.sqrt())
    }
}

/// Distinct colours for painted features.
pub(crate) const REGION_COLORS: [Rgba; 8] = [
    [230, 120, 20, 255],
    [30, 130, 220, 255],
    [40, 170, 80, 255],
    [200, 50, 140, 255],
    [140, 90, 40, 255],
    [20, 170, 170, 255],
    [170, 150, 20, 255],
    [110, 70, 200, 255],
];

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub(crate) fn engine_mesh(mesh: &TriMesh) -> engine::Mesh {
    let v = |p: P| engine::Vec3 { x: p[0], y: p[1], z: p[2] };
    engine::Mesh {
        triangles: (0..mesh.triangles.len())
            .map(|t| {
                let [a, b, c] = corners(mesh, t);
                engine::Triangle { vertices: [v(a), v(b), v(c)], normal: v(face_normal_area(mesh, t).0) }
            })
            .collect(),
    }
}

/// Per-triangle colours as the renderer's palette form.
pub(crate) fn face_colors(colors: &[Rgba]) -> engine::FaceColors {
    let mut palette: Vec<Option<[f64; 4]>> = Vec::new();
    let mut slots: HashMap<Rgba, u16> = HashMap::new();
    let faces = colors
        .iter()
        .map(|color| {
            // Quantise to keep the palette within u16 slots.
            let q = [color[0] & 0xfc, color[1] & 0xfc, color[2] & 0xfc, color[3]];
            *slots.entry(q).or_insert_with(|| {
                palette.push(Some([q[0] as f64 / 255.0, q[1] as f64 / 255.0, q[2] as f64 / 255.0, q[3] as f64 / 255.0]));
                (palette.len() - 1) as u16
            })
        })
        .collect();
    engine::FaceColors { palette, faces }
}

pub(crate) struct SceneObject {
    pub name: String,
    pub mesh: engine::Mesh,
    pub color: Option<Rgba>,
    pub faces: Option<engine::FaceColors>,
}

impl SceneObject {
    pub(crate) fn plain(name: &str, mesh: &TriMesh, color: Rgba) -> Self {
        Self { name: name.to_string(), mesh: engine_mesh(mesh), color: Some(color), faces: None }
    }

    pub(crate) fn painted(name: &str, mesh: &TriMesh, colors: &[Rgba]) -> Self {
        Self { name: name.to_string(), mesh: engine_mesh(mesh), color: None, faces: Some(face_colors(colors)) }
    }
}

/// A colour scale drawn on the image.
pub(crate) struct Legend {
    pub title: String,
    /// Bar from -range to +range (signed) or 0..range.
    pub range: f64,
    pub signed: bool,
    pub dead: f64,
    /// Extra swatches: (colour, text).
    pub keys: Vec<(Rgba, String)>,
}

pub(crate) struct Marker {
    pub point: P,
    pub label: String,
    pub color: Rgba,
}

fn put_pixel(pixels: &mut [u8], width: u32, height: u32, x: i64, y: i64, color: Rgba) {
    if x >= 0 && y >= 0 && (x as u32) < width && (y as u32) < height {
        let offset = ((y as usize) * width as usize + x as usize) * 4;
        pixels[offset..offset + 4].copy_from_slice(&color);
    }
}

pub(crate) fn fill_rect(pixels: &mut [u8], width: u32, height: u32, x0: i64, y0: i64, w: i64, h: i64, color: Rgba) {
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            put_pixel(pixels, width, height, x, y, color);
        }
    }
}

/// A ring around (cx, cy), drawn only inside `area`.
fn ring(pixels: &mut [u8], width: u32, height: u32, area: raster::ModelArea, cx: f64, cy: f64, r: f64, thickness: f64, color: Rgba) {
    let reach = (r + thickness).ceil() as i64;
    let (x_end, y_end) = (area.x as i64 + area.width as i64, area.y as i64 + area.height as i64);
    let (cx, cy) = (cx.round(), cy.round());
    if !(cx.abs() < 1e9 && cy.abs() < 1e9) {
        return;
    }
    for dy in -reach..=reach {
        for dx in -reach..=reach {
            let d = ((dx * dx + dy * dy) as f64).sqrt();
            let (x, y) = (cx as i64 + dx, cy as i64 + dy);
            if (d - r).abs() <= thickness * 0.5 && x >= area.x as i64 && x < x_end && y >= area.y as i64 && y < y_end {
                put_pixel(pixels, width, height, x, y, color);
            }
        }
    }
}

/// A marker's text beside its ring, flipped to the left when it would leave
/// the area; None for a marker outside it, which has no place to name.
fn marker_label_at(area: raster::ModelArea, x: f64, y: f64, text_w: i64, s: i64) -> Option<(i64, i64)> {
    let (left, top) = (area.x as i64, area.y as i64);
    let (right, bottom) = (left + area.width as i64, top + area.height as i64);
    let (cx, cy) = (x.round(), y.round());
    if !(cx >= left as f64 && cx < right as f64 && cy >= top as f64 && cy < bottom as f64) {
        return None;
    }
    let (cx, cy) = (cx as i64, cy as i64);
    let mut tx = cx + 9 * s;
    if tx + text_w + 1 > right {
        tx = cx - 9 * s - text_w;
    }
    let tx = tx.clamp(left + 1, (right - text_w - 1).max(left + 1));
    let ty = (cy - 4 * s).clamp(top + 1, (bottom - 9 * s).max(top + 1));
    Some((tx, ty))
}

const LEGEND_INK: Rgba = [30, 30, 30, 255];

/// What a legend's column holds and how much room that takes.
struct LegendLayout {
    s: i64,
    /// Column width, separator and padding included.
    width: i64,
    /// Image height the column needs with the bar at its shortest.
    min_height: i64,
    title: Vec<String>,
    /// Scale ticks: value and text, top to bottom. Empty without a bar.
    ticks: Vec<(f64, String)>,
    ticks_w: i64,
    keys: Vec<(Rgba, String)>,
}

impl LegendLayout {
    fn of(legend: &Legend, s: usize) -> Self {
        let scale = s as i64;
        let has_bar = legend.range > 0.0;
        let ticks: Vec<(f64, String)> = if !has_bar {
            Vec::new()
        } else {
            let values = if legend.signed {
                vec![legend.range, legend.range / 2.0, 0.0, -legend.range / 2.0, -legend.range]
            } else {
                vec![legend.range, legend.range / 2.0, 0.0]
            };
            values.into_iter().map(|value| (value, if value > 0.0 && legend.signed { format!("+{}", short(value)) } else { short(value) })).collect()
        };
        let mut keys = Vec::new();
        if has_bar && legend.dead > 0.0 {
            keys.push(([120, 200, 120, 255], format!("<{}", short(legend.dead))));
        }
        keys.extend(legend.keys.iter().cloned());
        let ticks_w = ticks.iter().map(|(_, text)| raster::text_width(text, s) as i64).max().unwrap_or(0);
        let keys_w = keys.iter().map(|(_, text)| raster::text_width(text, s) as i64 + 10 * scale).max().unwrap_or(0);
        let bar_w = if has_bar { 15 * scale + ticks_w } else { 0 };
        let content = bar_w.max(keys_w).max(raster::text_width("deviation", s) as i64);
        let title = wrap_words(&legend.title, (content / (6 * scale)).max(1) as usize);
        let title_w = title.iter().map(|line| raster::text_width(line, s) as i64).max().unwrap_or(0);
        let width = content.max(title_w) + 13 * scale;
        let bar_min = if has_bar { 70 * scale } else { 0 };
        let min_height = (LEGEND_TOP * scale) + 11 * scale * (title.len() + keys.len()) as i64 + 8 * scale + bar_min;
        Self { s: scale, width, min_height, title, ticks, ticks_w, keys }
    }
}

/// Padding above the legend's title, in font-scale units.
const LEGEND_TOP: i64 = 6;

/// `text` broken at spaces into lines of at most `limit` characters (a longer
/// word stands alone on its line).
fn wrap_words(text: &str, limit: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(line) if line.chars().count() + 1 + word.chars().count() <= limit => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.to_string()),
        }
    }
    lines
}

/// Draws the legend in the column right of the model area, whose full height
/// it fills: title, then the bar (half the image at most) with right-aligned
/// ticks, then the swatches, on one left edge.
fn draw_legend(pixels: &mut [u8], width: u32, height: u32, area: raster::ModelArea, legend: &Legend, layout: &LegendLayout) {
    let s = layout.s;
    let (w, h) = (width as usize, height as usize);
    let text_scale = s as usize;
    let column = (area.x + area.width) as i64;
    fill_rect(pixels, width, height, column, 0, width as i64 - column, height as i64, [255, 255, 255, 255]);
    fill_rect(pixels, width, height, column, 0, 1, height as i64, [190, 190, 190, 255]);
    let x0 = column + 6 * s;
    let text = |pixels: &mut [u8], x: i64, y: i64, content: &str| {
        raster::draw_text_colored(pixels, w, h, x.max(0) as usize, y.max(0) as usize, content, text_scale, LEGEND_INK);
    };
    let mut y = LEGEND_TOP * s;
    for line in &layout.title {
        text(pixels, x0, y, line);
        y += 11 * s;
    }
    if !layout.ticks.is_empty() {
        // The first tick's text reaches above the bar.
        y += 4 * s;
        let fixed = y + 8 * s + 11 * s * layout.keys.len() as i64 + LEGEND_TOP * s;
        let bar_h = (height as i64 - fixed).min(height as i64 / 2).max(60 * s);
        let bar_w = 10 * s;
        for row in 0..bar_h {
            let f = 1.0 - row as f64 / (bar_h - 1).max(1) as f64;
            let value = if legend.signed { (2.0 * f - 1.0) * legend.range } else { f * legend.range };
            fill_rect(pixels, width, height, x0, y + row, bar_w, 1, diverging(value, legend.range, legend.dead));
        }
        for (value, label) in &layout.ticks {
            let f = if legend.signed { (value / legend.range + 1.0) / 2.0 } else { value / legend.range };
            let tick_y = y + ((1.0 - f) * (bar_h - 1) as f64).round() as i64;
            fill_rect(pixels, width, height, x0 + bar_w, tick_y, 2 * s, s, LEGEND_INK);
            let label_x = x0 + bar_w + 5 * s + layout.ticks_w - raster::text_width(label, text_scale) as i64;
            text(pixels, label_x, tick_y - 3 * s, label);
        }
        y += bar_h + 8 * s;
    }
    for (color, label) in &layout.keys {
        fill_rect(pixels, width, height, x0, y, 7 * s, 7 * s, *color);
        text(pixels, x0 + 10 * s, y, label);
        y += 11 * s;
    }
}

pub(crate) fn short(value: f64) -> String {
    let text = if value.abs() >= 10.0 { format!("{value:.1}") } else if value.abs() >= 1.0 { format!("{value:.2}") } else { format!("{value:.3}") };
    let text = if text.contains('.') { text.trim_end_matches('0').trim_end_matches('.').to_string() } else { text };
    if text == "-0" { "0".into() } else { text }
}

/// Renders the scene with render_image options taken from `image` (an
/// object, or true for defaults), drawing the legend and markers. Pictures
/// carry no commentary: `title` goes into the returned metadata, for the
/// caller's caption, and marker labels should be short indices.
pub(crate) fn render_scene(
    objects: &[SceneObject],
    image: &Value,
    legend: Option<&Legend>,
    markers: &[Marker],
    title: Option<&str>,
) -> Result<(Vec<u8>, Value), String> {
    let options_input = if image.is_object() { image.clone() } else { json!({}) };
    let options = agent::render_options(&options_input).map_err(|error| error.message)?;
    let render_objects: Vec<raster::RenderObject> = objects
        .iter()
        .map(|object| raster::RenderObject {
            name: object.name.as_str(),
            mesh: &object.mesh,
            color: object.color,
            face_colors: object.faces.as_ref(),
            visible: true,
        })
        .collect();
    let s = raster::text_scale(options.width.clamp(raster::MIN_SIZE, raster::MAX_SIZE));
    let layout = legend.map(|legend| LegendLayout::of(legend, s));
    let reserve = layout.as_ref().map_or(raster::Reserve::default(), |layout| raster::Reserve { right: layout.width as u32, min_height: layout.min_height as u32 });
    let output = raster::render_with_overlay(&render_objects, &options, reserve, |pixels, width, height, meta| {
        let area = meta.model_area;
        for (i, marker) in markers.iter().enumerate() {
            if let Some([x, y]) = raster::project_point(meta, marker.point) {
                ring(pixels, width, height, area, x, y, (7 * s) as f64, (2 * s) as f64, [255, 255, 255, 255]);
                ring(pixels, width, height, area, x, y, (6 * s) as f64, (2 * s) as f64, marker.color);
                let text = if marker.label.is_empty() { format!("{}", i + 1) } else { marker.label.clone() };
                let text_w = raster::text_width(&text, s) as i64;
                if let Some((tx, ty)) = marker_label_at(area, x, y, text_w, s as i64) {
                    fill_rect(pixels, width, height, tx - 1, ty - 1, text_w + 2, (9 * s) as i64, [255, 255, 255, 220]);
                    raster::draw_text_colored(pixels, width as usize, height as usize, tx as usize, ty as usize, &text, s, marker.color);
                }
            }
        }
        if let (Some(legend), Some(layout)) = (legend, &layout) {
            draw_legend(pixels, width, height, area, legend, layout);
        }
    })
    .map_err(|error| format!("Image could not be rendered: {error}."))?;
    let mut meta = serde_json::to_value(&output.meta).unwrap_or(Value::Null);
    if let (Some(title), Some(fields)) = (title, meta.as_object_mut()) {
        fields.insert("title".into(), json!(title));
    }
    Ok((output.png, meta))
}

/// An MCP tool result carrying `structured` as JSON text plus PNG images
/// (each with a caption).
pub(crate) fn image_result(structured: Value, images: Vec<(String, Vec<u8>)>) -> Value {
    let mut content = Vec::new();
    for (caption, png) in &images {
        content.push(json!({"type": "text", "text": caption}));
        content.push(json!({"type": "image", "data": crate::base64_encode(png), "mimeType": "image/png"}));
    }
    content.insert(0, json!({"type": "text", "text": serde_json::to_string(&structured).unwrap_or_default()}));
    json!({"content": content, "structuredContent": structured, "isError": false})
}

/// [`image_result`] with `structured` sent once: `content` holds one summary
/// line and the images, `structuredContent` the full result.
pub(crate) fn summary_result(summary: String, structured: Value, images: Vec<(String, Vec<u8>)>) -> Value {
    let mut content = vec![json!({"type": "text", "text": summary})];
    for (caption, png) in &images {
        content.push(json!({"type": "text", "text": caption}));
        content.push(json!({"type": "image", "data": crate::base64_encode(png), "mimeType": "image/png"}));
    }
    json!({"content": content, "structuredContent": structured, "isError": false})
}

/// The scan tools' reply as agents get it: without the bulk that is rarely
/// read (compiler console, histograms, camera records, knot vectors, the
/// pose matrices repeated in every surface's source, long attempt logs),
/// unless the call passed `detail: true`. `warnings` stay.
pub(crate) fn compact(mut value: Value, input: &Value) -> Value {
    if input.get("detail").and_then(Value::as_bool) == Some(true) {
        return value;
    }
    fn strip(value: &mut Value, parent: &str) {
        match value {
            Value::Object(map) => {
                for key in ["console", "histogram", "camera", "knots"] {
                    map.remove(key);
                }
                if parent == "source" {
                    map.remove("matrix");
                }
                if let Some(Value::Array(attempts)) = map.get_mut("attempts") {
                    let skip = attempts.len().saturating_sub(3);
                    attempts.drain(..skip);
                }
                for (key, child) in map.iter_mut() {
                    strip(child, key);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| strip(item, parent)),
            _ => {}
        }
    }
    strip(&mut value, "");
    value
}

/// `image` argument: false/absent = none, true = defaults, object = options.
pub(crate) fn image_wanted(input: &Value, default: bool) -> Result<Option<Value>, String> {
    match input.get("image") {
        None | Some(Value::Null) => Ok(default.then(|| json!({}))),
        Some(Value::Bool(false)) => Ok(None),
        Some(Value::Bool(true)) => Ok(Some(json!({}))),
        Some(value @ Value::Object(_)) => Ok(Some(value.clone())),
        Some(_) => Err("image must be true, false or render_image options.".into()),
    }
}

/// A view direction looking at a patch with mean outward normal `normal`
/// (camera on the normal side), as render options.
pub(crate) fn facing_view(image: &Value, center: P, normal: P, distance: f64) -> Value {
    let mut image = if image.is_object() { image.clone() } else { json!({}) };
    if image.get("eye").is_none() && image.get("view").is_none() && norm(normal) > 0.5 {
        let n = unit(normal);
        let up = if n[2].abs() > 0.9 { [0.0, 1.0, 0.0] } else { [0.0, 0.0, 1.0] };
        image["eye"] = json!(r3(add(center, scale(n, distance))));
        image["target"] = json!(r3(center));
        image["up"] = json!(up);
    }
    image
}

/// Test fixtures shared by the scan tools' tests: a filesystem-backed
/// state and an MCP call returning the whole tool result.
#[cfg(test)]
pub(crate) mod testkit {
    use crate::{
        AppState, JobLimiter, RateLimiter, RenderQueue, Request, Store, WorkspaceStore, MAX_CONCURRENT_JOBS,
        MAX_CONCURRENT_POLLS, MAX_QUEUED_JOBS, MAX_QUEUED_PER_CLIENT, MAX_QUEUE_WAIT,
    };
    use serde_json::Value;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    pub(crate) fn state() -> (AppState, PathBuf) {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let root = std::env::temp_dir().join(format!("reopenscad-scan-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let state = AppState {
            web_root: PathBuf::new(),
            render_queue: RenderQueue::new(MAX_CONCURRENT_JOBS, MAX_QUEUED_JOBS, MAX_QUEUED_PER_CLIENT, MAX_QUEUE_WAIT),
            poll_limiter: JobLimiter::new(MAX_CONCURRENT_POLLS),
            rate_limiter: RateLimiter::new(),
            cancellations: Arc::new(Mutex::new(HashMap::new())),
            workspaces: Store::Filesystem(WorkspaceStore::open(root.clone()).unwrap()),
            public_origin: None,
        };
        (state, root)
    }

    /// Stores `mesh` the way a user's upload_asset does (cleaned, open
    /// surfaces kept open), so scan tools treat it as measured data.
    pub(crate) fn upload(state: &AppState, id: &str, name: &str, mesh: &crate::assets::TriMesh) {
        upload_with(state, id, name, mesh, serde_json::json!({}));
    }

    /// [`upload`] with extra upload_asset arguments (budget).
    /// The whole compiled model of design `id`, as a mesh to store in a test.
    pub(crate) fn compiled_mesh(state: &AppState, id: &str) -> crate::assets::TriMesh {
        let source = crate::agent::eval_source(state, Some(id), &serde_json::json!({})).unwrap();
        crate::meshanalysis::from_engine_mesh(&crate::agent::compile_eval(&source).unwrap().mesh)
    }

    pub(crate) fn upload_with(state: &AppState, id: &str, name: &str, mesh: &crate::assets::TriMesh, extra: Value) -> Value {
        let stl = crate::meshimport::encode_binary_stl(mesh);
        let mut arguments = serde_json::json!({"designId": id, "name": name, "base64": crate::base64_encode(&stl)});
        if let (Some(arguments), Some(extra)) = (arguments.as_object_mut(), extra.as_object()) {
            arguments.extend(extra.clone());
        }
        // A second scan in one design: tests of tools that take two surfaces
        // store it as one from before one STL per design could be.
        if !state.workspaces.asset_list(id).unwrap().is_empty() {
            let mut processed = crate::assetstore::process_upload(name, &stl, 500_000, crate::unix_millis()).unwrap();
            processed.kind = crate::assetstore::AssetKind::Copy;
            processed.meta.expires_at = crate::unix_millis() + crate::assetstore::ASSET_TTL_MS;
            state.workspaces.asset_put(id, &processed).unwrap();
            return serde_json::json!({"structuredContent": processed.meta.public(), "isError": false});
        }
        let reply = call(state, "upload_asset", arguments);
        assert_eq!(reply["isError"], serde_json::json!(false), "{reply}");
        reply
    }

    pub(crate) fn request() -> Request {
        Request {
            method: "POST".into(),
            path: "/mcp".into(),
            query: String::new(),
            headers: vec![("host".into(), "127.0.0.1:5173".into())],
            body: vec![],
        }
    }

    pub(crate) fn call(state: &AppState, name: &str, arguments: Value) -> Value {
        crate::mcp_call_tool(state, &request(), name, arguments)
    }

    /// Saves `code` (in a new design, unless `id` names one) with `overrides`
    /// written into it, -D style, as a named revision, as an agent has to before
    /// a tool can look at it. Returns the design id for the tool call.
    pub(crate) fn save_revision(state: &AppState, id: Option<&str>, code: Option<&str>, overrides: Option<&Value>) -> String {
        let id = match id {
            Some(id) => id.to_string(),
            None => state.workspaces.create(code.unwrap_or("").to_string(), None).unwrap().id,
        };
        let current = state.workspaces.get(&id).unwrap().unwrap();
        let base = code.map_or_else(|| current.code.clone(), str::to_string);
        let parsed = crate::scadsource::parse_overrides(overrides).unwrap();
        let source = crate::scadsource::apply_overrides(&base, &parsed, true).unwrap().source;
        if source != current.code {
            let update = serde_json::from_value(serde_json::json!({
                "expectedRevision": current.revision,
                "code": source,
                "source": "ai",
                "revisionName": "Test revision",
            }))
            .unwrap();
            crate::assetstore::with_design_assets(state, Some(&id), || state.workspaces.update(&id, update)).unwrap();
        }
        id
    }

    /// A new design whose revisions 1 and 2 hold `from` and `to`, for `diff`.
    pub(crate) fn design_with_revisions(state: &AppState, from: &str, to: &str) -> String {
        let id = save_revision(state, None, Some(from), None);
        let update = serde_json::from_value(serde_json::json!({"expectedRevision": 1, "code": to, "source": "ai", "revisionName": "To"})).unwrap();
        crate::assetstore::with_design_assets(state, Some(&id), || state.workspaces.update(&id, update)).unwrap();
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flat 400 x 40 plate lying on z = 0, facing up.
    fn wide_plate() -> TriMesh {
        TriMesh {
            points: vec![[-200.0, -20.0, 0.0], [200.0, -20.0, 0.0], [200.0, 20.0, 0.0], [-200.0, 20.0, 0.0]],
            triangles: vec![[0, 1, 2], [0, 2, 3]],
        }
    }

    fn deviation_legend() -> Legend {
        Legend {
            title: "deviation mm".into(),
            range: 0.917,
            signed: true,
            dead: 0.3,
            keys: vec![([170, 170, 170, 255], "not observed".into()), ([230, 140, 40, 255], "unexplained".into()), ([140, 60, 200, 255], "facing mismatch".into())],
        }
    }

    fn scene_pixels(legend: Option<&Legend>, markers: &[Marker], image: Value) -> (usize, usize, Vec<u8>, raster::ModelArea) {
        let scene = [SceneObject::plain("plate", &wide_plate(), [20, 220, 220, 255])];
        let (png, meta) = render_scene(&scene, &image, legend, markers, None).unwrap();
        let (w, h, rgba) = raster::tests::decode_png(&png);
        let area = meta["model_area"].clone();
        let field = |name: &str| area[name].as_u64().unwrap() as u32;
        (w, h, rgba, raster::ModelArea { x: field("x"), y: field("y"), width: field("width"), height: field("height") })
    }

    fn top_view(width: u32, height: u32, zoom: f64) -> Value {
        json!({"view": "top", "projection": "orthographic", "width": width, "height": height, "zoom": zoom, "edges": false, "axes": false, "grid": true, "background": "#ffffff"})
    }

    /// The plate's colour, which no legend uses.
    fn is_plate(p: &[u8]) -> bool {
        p[1] as i32 > p[0] as i32 + 100 && p[2] as i32 > p[0] as i32 + 100
    }

    #[test]
    fn the_legend_has_its_own_column_and_the_part_all_of_the_model_area() {
        for (width, height) in [(1024, 768), (800, 600), (640, 200), (300, 900), (64, 64)] {
            for legend in [deviation_legend(), Legend { title: "distance to the fit, mm".into(), range: 2.0, signed: false, dead: 0.0, keys: vec![] }, Legend { title: "features".into(), range: 0.0, signed: false, dead: 0.0, keys: vec![([10, 10, 200, 255], "main surface".into())] }] {
                let (w, h, rgba, area) = scene_pixels(Some(&legend), &[], top_view(width, height, 1.0));
                assert_eq!((area.width, area.height), (width, height), "the model keeps the requested size");
                let column = area.width as usize;
                assert!(w > column);
                let mut model = (usize::MAX, 0);
                let mut legend_ink = 0;
                for y in 0..h {
                    for x in 0..w {
                        let p = &rgba[(y * w + x) * 4..(y * w + x) * 4 + 4];
                        if x >= column {
                            legend_ink += usize::from(p[..3] != [255, 255, 255]);
                        }
                        if is_plate(p) {
                            assert!(x < column && y < area.height as usize, "model outside its area at {x},{y} ({width}x{height})");
                            model = (model.0.min(x), model.1.max(x));
                        }
                    }
                }
                assert!(legend_ink > 0, "a legend was drawn");
                assert!(model.0 > 0 && model.1 + 1 < column, "whole part: {model:?} of {column}");
                assert!(((model.0 + model.1) as i64 - (column as i64 - 1)).abs() <= 2, "centred: {model:?} of {column}");
                assert!(model.1 - model.0 > column * 4 / 5, "fills the area: {model:?} of {column}");
                // The whole legend fits: the column's last rows are blank.
                let blank = (column + 1..w).all(|x| (h - 2..h).all(|y| rgba[(y * w + x) * 4..(y * w + x) * 4 + 3] == [255, 255, 255]));
                assert!(blank, "legend clipped at {width}x{height}");
            }
        }
    }

    #[test]
    fn numbered_rings_at_the_frame_edge_stay_inside_the_model_area() {
        let legend = deviation_legend();
        // Zoomed so the plate's end sits right at the edge of the model area.
        let image = top_view(800, 600, 1.07);
        let bare = scene_pixels(Some(&legend), &[], image.clone());
        let marker = |x: f64, label: &str| Marker { point: [x, 0.0, 0.0], label: label.into(), color: [20, 20, 20, 255] };
        // At the edge, far off the frame, and past it on the left.
        let marked = scene_pixels(Some(&legend), &[marker(200.0, "12"), marker(205.0, "3"), marker(5000.0, "4"), marker(-5000.0, "5")], image);
        assert_eq!((bare.0, bare.1), (marked.0, marked.1));
        let (w, h, area) = (bare.0, bare.1, bare.3);
        let (mut changed, mut leftmost) = (0, usize::MAX);
        for y in 0..h {
            for x in 0..w {
                let offset = (y * w + x) * 4;
                if bare.2[offset..offset + 4] != marked.2[offset..offset + 4] {
                    assert!(x < area.width as usize && y < area.height as usize, "marker outside the model area at {x},{y}");
                    changed += 1;
                    leftmost = leftmost.min(x);
                }
            }
        }
        assert!(changed > 20, "the rings are drawn");
        assert!(leftmost < area.width as usize - 20, "the labels flipped to the left of their rings: {leftmost}");
    }

    #[test]
    fn marker_labels_are_placed_inside_the_area_or_not_at_all() {
        let area = raster::ModelArea { x: 0, y: 0, width: 200, height: 100 };
        assert_eq!(marker_label_at(area, 50.0, 50.0, 12, 1), Some((59, 46)));
        // Flipped at the right edge; clamped at the top and the bottom.
        assert_eq!(marker_label_at(area, 195.0, 50.0, 12, 1), Some((174, 46)));
        assert_eq!(marker_label_at(area, 50.0, 0.0, 12, 1).unwrap().1, 1);
        assert_eq!(marker_label_at(area, 50.0, 99.0, 12, 1).unwrap().1, 91);
        for (x, y) in [(200.0, 50.0), (-1.0, 50.0), (50.0, 100.0), (50.0, -1.0), (f64::NAN, 5.0)] {
            assert_eq!(marker_label_at(area, x, y, 12, 1), None);
        }
    }

    #[test]
    fn long_legend_titles_wrap_instead_of_widening_the_column() {
        assert_eq!(wrap_words("distance to the fit, mm", 12), vec!["distance to", "the fit, mm"]);
        assert_eq!(wrap_words("deviation mm", 40), vec!["deviation mm"]);
        let layout = LegendLayout::of(&Legend { title: "distance to the fit, mm".into(), range: 1.0, signed: true, dead: 0.2, keys: vec![] }, 1);
        assert!(layout.title.len() > 1);
        assert!(layout.width < raster::text_width("distance to the fit, mm", 1) as i64);
    }

    fn quad() -> TriMesh {
        TriMesh {
            points: vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [10.0, 10.0, 0.0], [0.0, 10.0, 0.0]],
            triangles: vec![[0, 1, 2], [0, 2, 3]],
        }
    }

    #[test]
    fn a_re_meshed_import_is_posed_by_its_shape() {
        // A bumpy, asymmetric sheet; "placed" is it rotated, scaled and moved,
        // with its triangles reversed in order and re-split along the other
        // diagonal: no triangle correspondence survives.
        let (n, f) = (20u32, |x: f64, y: f64| 0.8 * (x / 3.0).sin() + 0.3 * (y / 2.0).cos() + 0.01 * x * y);
        let mut asset = TriMesh::default();
        for j in 0..=n {
            for i in 0..=n {
                asset.points.push([i as f64 * 1.5, j as f64, f(i as f64 * 1.5, j as f64)]);
            }
        }
        let at = |i: u32, j: u32| j * (n + 1) + i;
        let mut other = asset.clone();
        for j in 0..n {
            for i in 0..n {
                asset.triangles.push([at(i, j), at(i + 1, j), at(i + 1, j + 1)]);
                asset.triangles.push([at(i, j), at(i + 1, j + 1), at(i, j + 1)]);
                other.triangles.push([at(i, j), at(i + 1, j), at(i, j + 1)]);
                other.triangles.push([at(i + 1, j), at(i + 1, j + 1), at(i, j + 1)]);
            }
        }
        other.triangles.reverse();
        let (c, s) = (0.6f64, 0.8f64);
        let m: Mat4 = [[2.0 * c, -2.0 * s, 0.0, 15.0], [2.0 * s, 2.0 * c, 0.0, -4.0], [0.0, 0.0, 2.0, 7.0], [0.0, 0.0, 0.0, 1.0]];
        let placed = meshops::transform(&other, &m).unwrap();
        assert!(fit_frame(&asset, &placed).is_none());
        let (found, residual) = fit_frame_by_shape(&asset, &placed).expect("posed by shape");
        assert!(residual < 1e-6, "{residual}");
        for i in 0..3 {
            for j in 0..4 {
                assert!((found[i][j] - m[i][j]).abs() < 1e-6, "{found:?}");
            }
        }
        // A plain rectangle fits itself turned half way round: ambiguous, so
        // no pose (the marker recovery decides instead).
        let rectangle = TriMesh { points: vec![[0.0, 0.0, 0.0], [30.0, 0.0, 0.0], [30.0, 10.0, 0.0], [0.0, 10.0, 0.0]], triangles: vec![[0, 1, 2], [0, 2, 3]] };
        let half_turn: Mat4 = [[-1.0, 0.0, 0.0, 30.0], [0.0, -1.0, 0.0, 10.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];
        let mut turned = meshops::transform(&rectangle, &half_turn).unwrap();
        turned.triangles = vec![[0, 1, 3], [1, 2, 3]];
        assert!(fit_frame_by_shape(&rectangle, &turned).is_none());
        // A different surface is not taken for it.
        let mut wrong = placed.clone();
        for p in &mut wrong.points {
            p[2] += 0.05 * p[0].sin();
        }
        assert!(fit_frame_by_shape(&asset, &wrong).is_none());
    }

    #[test]
    fn samples_cover_the_area_and_flag_open_borders() {
        let mesh = quad();
        let samples = sample_surface(&mesh, 1000);
        let area: f64 = samples.iter().map(|s| s.weight).sum();
        assert!((area - 100.0).abs() < 1e-6, "{area}");
        assert!(samples.len() >= 990 && samples.len() <= 1000);
        // One big triangle beside many tiny ones: weights follow area.
        let mut mixed = TriMesh { points: vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0]], triangles: vec![[0, 1, 2]] };
        for i in 0..2000 {
            let x = 20.0 + (i % 50) as f64;
            let y = (i / 50) as f64;
            let base = mixed.points.len() as u32;
            mixed.points.extend([[x, y, 0.0], [x + 0.5, y, 0.0], [x, y + 0.1, 0.0]]);
            mixed.triangles.push([base, base + 1, base + 2]);
        }
        let mixed_samples = sample_surface(&mixed, 200);
        let total: f64 = mixed_samples.iter().map(|s| s.weight).sum();
        let big: f64 = mixed_samples.iter().filter(|s| s.tri == 0).map(|s| s.weight).sum();
        assert!((total - 100.0).abs() < 1e-6, "{total}");
        assert!((big / total - 0.5).abs() < 0.05, "{}", big / total);
        assert!(samples.iter().all(|s| s.point[0] >= 0.0 && s.point[0] <= 10.0 && s.point[1] >= 0.0 && s.point[1] <= 10.0));
        let (flags, open) = boundary_faces(&mesh);
        assert_eq!(open, 4);
        assert_eq!(flags, vec![true, true]);
    }

    #[test]
    fn weighted_stats_and_frames() {
        let stats = Stats::of(&[(1.0, 1.0), (-3.0, 1.0)]);
        assert!((stats.mean - 2.0).abs() < 1e-12);
        assert!((stats.signed_mean + 1.0).abs() < 1e-12);
        assert!((stats.rms - 5f64.sqrt()).abs() < 1e-12);
        assert_eq!(stats.max, 3.0);
        let mesh = TriMesh {
            points: vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0], [0.0, 0.0, 10.0]],
            triangles: vec![[0, 2, 1], [0, 1, 3], [0, 3, 2], [1, 2, 3]],
        };
        let m: Mat4 = [[0.0, -1.0, 0.0, 5.0], [1.0, 0.0, 0.0, -2.0], [0.0, 0.0, 2.0, 1.0], [0.0, 0.0, 0.0, 1.0]];
        let placed = meshops::transform(&mesh, &m).unwrap();
        let (fit, residual) = fit_frame(&mesh, &placed).expect("frame");
        assert!(residual < 1e-9);
        for r in 0..3 {
            for c in 0..4 {
                assert!((fit[r][c] - m[r][c]).abs() < 1e-9);
            }
        }
        let quad = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 2.0, 0.0], [0.0, 0.0, 3.0]];
        let images = quad.map(|p| meshops::apply(&m, p));
        let four = affine_from_four(&quad, &images).unwrap();
        assert!((0..3).all(|r| (0..4).all(|c| (four[r][c] - m[r][c]).abs() < 1e-9)));
        let code = "x = 1;\ntranslate([1,0,0]) import(file = \"scan.stl\", center = true);\n";
        let (start, end, centered) = find_import(code, "scan.stl").unwrap();
        assert_eq!(&code[start..end], "import(file = \"scan.stl\", center = true)");
        assert!(centered);
        assert!(find_import(code, "other.stl").is_none());
        let inverse = invert_affine(&m).unwrap();
        let p = meshops::apply(&inverse, meshops::apply(&m, [1.0, 2.0, 3.0]));
        assert!(norm(sub(p, [1.0, 2.0, 3.0])) < 1e-9);
    }
}
