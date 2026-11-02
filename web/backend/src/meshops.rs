//! Mesh primitives behind `split_object` and the scan tools: convex hull,
//! affine transforms, summaries, and registration-key placement on a planar
//! cut.
//!
//! Everything works on [`TriMesh`] (indexed, outward wound, millimetres) and
//! honours the thread's request deadline and cancellation
//! ([`crate::csg::bsp::interrupt_message`]) plus an optional extra deadline,
//! so a hostile mesh cannot hold a compute permit for long.

use crate::vec3::{add, cross, dot, norm, scale, sub};
use crate::assets::TriMesh;
use crate::meshanalysis::{MeshIndex, Section};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Instant;

pub type Mat4 = [[f64; 4]; 4];
type V = [f64; 3];

/// Errs once the request deadline passed, the call was cancelled, or
/// `deadline` passed.
pub fn check(deadline: Option<Instant>) -> Result<(), String> {
    if let Some(message) = crate::csg::bsp::interrupt_message() {
        return Err(message.to_string());
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err("the operation ran out of its time budget; use a smaller mesh, a lower budget or a coarser resolution".into());
    }
    Ok(())
}

/// Bounds of the referenced points.
pub fn bounds(mesh: &TriMesh) -> Option<(V, V)> {
    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    for triangle in &mesh.triangles {
        for &index in triangle {
            let p = mesh.points[index as usize];
            for axis in 0..3 {
                lo[axis] = lo[axis].min(p[axis]);
                hi[axis] = hi[axis].max(p[axis]);
            }
        }
    }
    (lo[0] <= hi[0]).then_some((lo, hi))
}

fn round(value: f64, decimals: i32) -> f64 {
    let factor = 10f64.powi(decimals);
    (value * factor).round() / factor
}

fn round3(v: V) -> V {
    v.map(|value| round(value, 4))
}

/// The compact summary of a mesh: triangle and
/// vertex counts, bounds, size, volume, area, shells and validity (closed,
/// consistently wound, positive volume).
pub fn summary(mesh: &TriMesh) -> Value {
    let triangles: Vec<[[f64; 3]; 3]> =
        mesh.triangles.iter().map(|t| t.map(|i| mesh.points[i as usize])).collect();
    let measure = crate::analysis::measure_triangles(&triangles);
    let valid = measure.watertight() && measure.winding_consistent() && measure.volume > 0.0;
    json!({
        "triangles": mesh.triangles.len(),
        "vertices": measure.vertices,
        "boundsMin": round3(measure.bounds.min),
        "boundsMax": round3(measure.bounds.max),
        "size": round3(sub(measure.bounds.max, measure.bounds.min)),
        "volume": round(measure.volume, 3),
        "surfaceArea": round(measure.surface_area, 3),
        "shells": measure.components,
        "watertight": measure.watertight(),
        "valid": valid,
    })
}

/// Concatenates meshes (no welding).
pub fn concat(meshes: impl IntoIterator<Item = TriMesh>) -> TriMesh {
    let mut out = TriMesh::default();
    for mesh in meshes {
        let base = out.points.len() as u32;
        out.points.extend(mesh.points);
        out.triangles.extend(mesh.triangles.into_iter().map(|t| t.map(|i| i + base)));
    }
    out
}

// ---------------------------------------------------------------------------
// Convex hull
// ---------------------------------------------------------------------------

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    let text = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|text| text.to_string()))
        .unwrap_or_else(|| "internal error".into());
    text.strip_prefix(crate::csg::bsp::BUDGET_PANIC_PREFIX).map(str::to_string).unwrap_or(text)
}

fn hull_polygons(points: &[V]) -> Result<Vec<crate::csg::mesh::Polygon>, String> {
    let vecs: Vec<crate::csg::mesh::Vec3> =
        points.iter().map(|p| crate::csg::mesh::Vec3::new(p[0], p[1], p[2])).collect();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::csg::hull::convex_hull(&vecs))).map_err(panic_text)
}

/// Drops points strictly inside the hull of the extreme points in 124
/// directions (Akl–Toussaint): exact, and on a scan it leaves a few percent
/// of the points for the incremental hull.
fn hull_candidates(points: &[V]) -> Result<Vec<V>, String> {
    let mut unique: Vec<V> = points.iter().copied().filter(|p| p.iter().all(|c| c.is_finite())).collect();
    unique.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])).then(a[2].total_cmp(&b[2])));
    unique.dedup();
    if unique.len() < 64 {
        return Ok(unique);
    }
    let mut extremes: Vec<V> = Vec::new();
    for dx in -2i32..=2 {
        for dy in -2i32..=2 {
            for dz in -2i32..=2 {
                if (dx, dy, dz) == (0, 0, 0) {
                    continue;
                }
                let d = [dx as f64, dy as f64, dz as f64];
                let best = unique.iter().copied().max_by(|a, b| dot(*a, d).total_cmp(&dot(*b, d)));
                if let Some(best) = best {
                    if !extremes.contains(&best) {
                        extremes.push(best);
                    }
                }
            }
        }
    }
    let polygons = hull_polygons(&extremes)?;
    if polygons.is_empty() {
        return Ok(unique);
    }
    let (lo, hi) = unique.iter().fold(([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]), |(lo, hi), p| {
        ([lo[0].min(p[0]), lo[1].min(p[1]), lo[2].min(p[2])], [hi[0].max(p[0]), hi[1].max(p[1]), hi[2].max(p[2])])
    });
    let eps = norm(sub(hi, lo)) * 1e-9;
    // Planes satisfy n·p = offset; each is oriented so the extremes'
    // centroid (strictly inside) lies on its negative side.
    let centre = scale(extremes.iter().fold([0.0; 3], |acc, p| add(acc, *p)), 1.0 / extremes.len() as f64);
    let planes: Vec<(V, f64)> = polygons
        .iter()
        .map(|polygon| {
            let n = polygon.plane.normal;
            let (n, offset) = ([n.x, n.y, n.z], polygon.plane.offset);
            if dot(n, centre) - offset > 0.0 { (scale(n, -1.0), -offset) } else { (n, offset) }
        })
        .collect();
    Ok(unique
        .into_iter()
        .filter(|p| planes.iter().any(|(n, offset)| dot(*n, *p) - offset > -eps))
        .collect())
}

/// The convex hull of the mesh's vertices as a closed, outward-wound mesh.
pub fn convex_hull(mesh: &TriMesh) -> Result<(TriMesh, usize), String> {
    let used: Vec<V> = {
        let mut seen = vec![false; mesh.points.len()];
        for triangle in &mesh.triangles {
            for &index in triangle {
                seen[index as usize] = true;
            }
        }
        mesh.points.iter().zip(seen).filter(|(_, used)| *used).map(|(p, _)| *p).collect()
    };
    let candidates = hull_candidates(&used)?;
    let polygons = hull_polygons(&candidates)?;
    if polygons.is_empty() {
        return Err("the points are flat or collinear, so their hull encloses no volume".into());
    }
    let mut out = TriMesh::default();
    let mut index: HashMap<[u64; 3], u32> = HashMap::new();
    for polygon in &polygons {
        let ids: Vec<u32> = polygon
            .vertices
            .iter()
            .map(|v| {
                let p = [v.x, v.y, v.z];
                *index.entry(p.map(f64::to_bits)).or_insert_with(|| {
                    out.points.push(p);
                    (out.points.len() - 1) as u32
                })
            })
            .collect();
        for k in 1..ids.len().saturating_sub(1) {
            out.triangles.push([ids[0], ids[k], ids[k + 1]]);
        }
    }
    if signed_volume(&out) < 0.0 {
        out.triangles.iter_mut().for_each(|t| t.swap(1, 2));
    }
    let hull_vertices = out.points.len();
    Ok((out, hull_vertices))
}

pub fn signed_volume(mesh: &TriMesh) -> f64 {
    mesh.triangles
        .iter()
        .map(|t| {
            let [a, b, c] = t.map(|i| mesh.points[i as usize]);
            dot(a, cross(b, c)) / 6.0
        })
        .sum()
}

// ---------------------------------------------------------------------------
// Transforms
// ---------------------------------------------------------------------------

pub fn apply(m: &Mat4, p: V) -> V {
    [0, 1, 2].map(|r| m[r][0] * p[0] + m[r][1] * p[1] + m[r][2] * p[2] + m[r][3])
}

pub fn mat_mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut out = [[0.0; 4]; 4];
    for (r, row) in out.iter_mut().enumerate() {
        for (c, cell) in row.iter_mut().enumerate() {
            *cell = (0..4).map(|k| a[r][k] * b[k][c]).sum();
        }
    }
    out
}

pub fn translation(t: V) -> Mat4 {
    [[1.0, 0.0, 0.0, t[0]], [0.0, 1.0, 0.0, t[1]], [0.0, 0.0, 1.0, t[2]], [0.0, 0.0, 0.0, 1.0]]
}

fn determinant3(m: &Mat4) -> f64 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
}

/// Applies an affine matrix; a mirroring matrix (negative determinant)
/// reverses the winding so the result still faces outward.
pub fn transform(mesh: &TriMesh, m: &Mat4) -> Result<TriMesh, String> {
    let det = determinant3(m);
    if !det.is_finite() || det.abs() < 1e-12 || m.iter().flatten().any(|v| !v.is_finite()) {
        return Err("matrix is singular or not finite (it would flatten the mesh)".into());
    }
    let mut out = TriMesh { points: mesh.points.iter().map(|p| apply(m, *p)).collect(), triangles: mesh.triangles.clone() };
    if det < 0.0 {
        out.triangles.iter_mut().for_each(|t| t.swap(1, 2));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Registration keys on a planar cut
// ---------------------------------------------------------------------------

/// A right-handed frame on the cut plane: plane coordinates (x, y, z along
/// the normal) to model space.
#[derive(Clone, Copy, Debug)]
pub struct PlaneFrame {
    pub origin: V,
    pub u: V,
    pub v: V,
    pub n: V,
}

impl PlaneFrame {
    pub fn to_model(&self, p: V) -> V {
        add(self.origin, add(add(scale(self.u, p[0]), scale(self.v, p[1])), scale(self.n, p[2])))
    }
    pub fn to_plane(&self, p: V) -> V {
        let d = sub(p, self.origin);
        [dot(d, self.u), dot(d, self.v), dot(d, self.n)]
    }
    /// Plane coordinates to model space, as a row-major matrix.
    pub fn matrix(&self) -> Mat4 {
        let (u, v, n, o) = (self.u, self.v, self.n, self.origin);
        [[u[0], v[0], n[0], o[0]], [u[1], v[1], n[1], o[1]], [u[2], v[2], n[2], o[2]], [0.0, 0.0, 0.0, 1.0]]
    }
    /// Model space to plane coordinates.
    pub fn inverse(&self) -> Mat4 {
        let (u, v, n, o) = (self.u, self.v, self.n, self.origin);
        [[u[0], u[1], u[2], -dot(u, o)], [v[0], v[1], v[2], -dot(v, o)], [n[0], n[1], n[2], -dot(n, o)], [0.0, 0.0, 0.0, 1.0]]
    }
}

/// The frame of `section` with its normal along `normal` (the section's own
/// basis, with v flipped if needed so u × v = n), and the section's closed
/// loops in that frame.
pub fn section_frame(section: &Section, normal: V) -> (PlaneFrame, Vec<Vec<[f64; 2]>>) {
    let flip = dot(cross(section.u, section.v), normal) < 0.0;
    let v = if flip { scale(section.v, -1.0) } else { section.v };
    let frame = PlaneFrame { origin: section.origin, u: section.u, v, n: cross(section.u, v) };
    let loops = section
        .loops
        .iter()
        .filter(|l| l.closed && l.points.len() >= 3)
        .map(|l| l.points.iter().map(|p| if flip { [p[0], -p[1]] } else { *p }).collect())
        .collect();
    (frame, loops)
}

#[derive(Clone, Copy, Debug)]
pub struct KeyRequest {
    /// Keys wanted; 0 turns keys off.
    pub count: usize,
    pub diameter: Option<f64>,
    pub length: Option<f64>,
    /// Radial and axial play between key and socket, per side.
    pub clearance: f64,
    /// Material kept around every socket.
    pub min_wall: f64,
    /// Total glue gap between the two cut faces.
    pub gap: f64,
    /// Pins fused to the negative half instead of loose dowels.
    pub pins: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyPlan {
    pub diameter: f64,
    pub length: f64,
    pub socket_diameter: f64,
    /// Socket depth below each cut face (the positive half only, for pins).
    pub socket_depth: f64,
    /// Key centres in plane coordinates.
    pub positions: Vec<[f64; 2]>,
    /// Distance from each key centre to the nearest cut edge.
    pub edge_distance: Vec<f64>,
    /// Largest inscribed circle radius of the cut.
    pub max_inscribed_radius: f64,
    pub notes: Vec<String>,
}

fn point_segment_distance(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let length2 = dx * dx + dy * dy;
    let t = if length2 > 0.0 { (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / length2).clamp(0.0, 1.0) } else { 0.0 };
    let (qx, qy) = (a[0] + t * dx - p[0], a[1] + t * dy - p[1]);
    (qx * qx + qy * qy).sqrt()
}

fn inside_loops(p: [f64; 2], loops: &[Vec<[f64; 2]>]) -> bool {
    let mut inside = false;
    for path in loops {
        let mut j = path.len() - 1;
        for i in 0..path.len() {
            let (a, b) = (path[i], path[j]);
            if (a[1] > p[1]) != (b[1] > p[1]) && p[0] < (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) + a[0] {
                inside = !inside;
            }
            j = i;
        }
    }
    inside
}

/// Up to `count` points of `candidates` ((point, edge distance)), at least
/// `spacing` apart: one key at the most interior point; more keys start at
/// the candidate farthest from the candidates' centroid and continue by
/// farthest-point sampling, so they spread across the whole cut.
fn spread_keys(candidates: &[([f64; 2], f64)], count: usize, spacing: f64) -> Vec<([f64; 2], f64)> {
    let distance = |p: [f64; 2], q: [f64; 2]| ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2)).sqrt();
    let first = if count == 1 {
        candidates.iter().copied().max_by(|a, b| a.1.total_cmp(&b.1))
    } else {
        let n = candidates.len().max(1) as f64;
        let centroid = candidates.iter().fold([0.0, 0.0], |acc, (p, _)| [acc[0] + p[0] / n, acc[1] + p[1] / n]);
        candidates.iter().copied().max_by(|a, b| (distance(a.0, centroid) + 0.1 * a.1).total_cmp(&(distance(b.0, centroid) + 0.1 * b.1)))
    };
    let Some(first) = first else { return Vec::new() };
    let mut picked = vec![first];
    while picked.len() < count {
        let next = candidates
            .iter()
            .copied()
            .filter_map(|(p, d)| {
                let separation = picked.iter().map(|(q, _)| distance(p, *q)).fold(f64::INFINITY, f64::min);
                (separation >= spacing).then_some(((p, d), separation + 0.1 * d))
            })
            .max_by(|a, b| a.1.total_cmp(&b.1));
        match next {
            Some((point, _)) => picked.push(point),
            None => break,
        }
    }
    picked
}

/// Picks key positions on the cut: points of the section at least the
/// socket radius plus `min_wall` from every cut edge, whose sockets also
/// stay `min_wall` inside the material along the normal, preferring spots
/// well inside the cut and spread apart by farthest-point sampling. Diameter and
/// length default from the largest inscribed circle and the material depth.
pub fn plan_keys(
    loops: &[Vec<[f64; 2]>],
    frame: &PlaneFrame,
    index: &MeshIndex,
    request: KeyRequest,
    deadline: Option<Instant>,
) -> Result<KeyPlan, String> {
    let mut plan = KeyPlan {
        diameter: 0.0,
        length: 0.0,
        socket_diameter: 0.0,
        socket_depth: 0.0,
        positions: Vec::new(),
        edge_distance: Vec::new(),
        max_inscribed_radius: 0.0,
        notes: Vec::new(),
    };
    if loops.is_empty() {
        return Ok(plan);
    }
    let (mut lo, mut hi) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
    for p in loops.iter().flatten() {
        lo = [lo[0].min(p[0]), lo[1].min(p[1])];
        hi = [hi[0].max(p[0]), hi[1].max(p[1])];
    }
    let extent = (hi[0] - lo[0]).max(hi[1] - lo[1]);
    let step = (extent / 120.0).max(0.05);
    let segments: Vec<([f64; 2], [f64; 2])> = loops
        .iter()
        .flat_map(|path| (0..path.len()).map(move |i| (path[i], path[(i + 1) % path.len()])))
        .collect();
    // (point, distance to the nearest cut edge) for grid points in material.
    let mut field: Vec<([f64; 2], f64)> = Vec::new();
    let mut y = lo[1] + step / 2.0;
    while y < hi[1] {
        check(deadline)?;
        let mut x = lo[0] + step / 2.0;
        while x < hi[0] {
            let p = [x, y];
            if inside_loops(p, loops) {
                let d = segments.iter().map(|(a, b)| point_segment_distance(p, *a, *b)).fold(f64::INFINITY, f64::min);
                field.push((p, d));
            }
            x += step;
        }
        y += step;
    }
    let r_max = field.iter().map(|(_, d)| *d).fold(0.0, f64::max);
    plan.max_inscribed_radius = round(r_max, 3);
    if request.count == 0 {
        return Ok(plan);
    }
    let (c, wall) = (request.clearance, request.min_wall);
    let diameter = match request.diameter {
        Some(d) => d,
        None => {
            let room = r_max - wall - c;
            let d = (room.min(6.0) * 2.0).floor() / 2.0;
            if d >= 2.0 {
                d
            } else {
                ((2.0 * room * 0.9).min(2.0) * 10.0).floor() / 10.0
            }
        }
    };
    if !(diameter >= 0.5) {
        plan.notes.push(format!(
            "No room for keys: the cut's largest inscribed circle has radius {r_max:.2} mm, too small for a 0.5 mm key with \
             {c} mm clearance and {wall} mm walls. Glue the halves without keys or move the plane."
        ));
        return Ok(plan);
    }
    let socket_r = diameter / 2.0 + c;
    let spacing = 2.0 * socket_r + wall;
    // Engagement: how far a key reaches into each half past its cut face.
    // A dowel of length L spans the glue gap, so L - gap is shared by the
    // two sockets; a pin rises gap + L / 2 from half B, so L / 2 reaches A.
    let engage = (diameter / 2.0).max(1.0);
    let engagement = |length: f64| if request.pins { length / 2.0 } else { (length - request.gap) / 2.0 };
    let lengths: Vec<f64> = match request.length {
        Some(l) if engagement(l) + 1e-9 < engage => {
            return Err(format!(
                "keys.length {l} mm leaves {:.2} mm of key in each half past the {} mm glue gap; it needs at least {engage:.2} mm \
                 (half the diameter, 1 mm minimum): use a length of at least {:.2} mm.",
                engagement(l).max(0.0),
                request.gap,
                if request.pins { 2.0 * engage } else { 2.0 * engage + request.gap }
            ));
        }
        Some(l) => vec![l],
        None => [3.0, 2.5, 2.0, 1.5]
            .iter()
            .map(|k| {
                let reach = (k * diameter).min(24.0).max(2.0 * engage);
                (((if request.pins { reach } else { reach + request.gap }) * 2.0).ceil()) / 2.0
            })
            .collect(),
    };
    let mut chosen: Vec<([f64; 2], f64)> = Vec::new();
    let mut chosen_length = lengths[0];
    // Grid points far enough from the cut edges, subsampled to bound the
    // number of depth checks.
    let mut pool: Vec<([f64; 2], f64)> = field.iter().copied().filter(|(_, d)| *d >= socket_r + wall).collect();
    if pool.len() > 2500 {
        let stride = pool.len().div_ceil(2500);
        pool = pool.into_iter().step_by(stride).collect();
    }
    for &length in &lengths {
        check(deadline)?;
        let depth = length / 2.0 + c;
        let sides: &[f64] = if request.pins { &[1.0] } else { &[1.0, -1.0] };
        // The whole socket, sampled at most 1 mm (and one socket radius)
        // apart from the cut face to its floor: every sample inside the
        // material with no surface within socket radius + minWall, which
        // keeps the full wall around the sides and under the floor.
        let step = socket_r.min(1.0).max(0.1);
        let samples = ((depth / step).ceil() as usize).clamp(1, 64);
        let deep_enough = |p: [f64; 2]| {
            sides.iter().all(|&side| {
                (0..=samples).all(|k| {
                    let t = request.gap / 2.0 + depth * k as f64 / samples as f64;
                    let q = frame.to_model([p[0], p[1], side * t]);
                    index.contains(q) && index.distance(q) >= socket_r + wall
                })
            })
        };
        let valid: Vec<([f64; 2], f64)> = pool.iter().copied().filter(|(p, _)| deep_enough(*p)).collect();
        // Comfortable spots first (well inside the cut), then any that fit.
        let minimum = socket_r + wall;
        let comfortable = minimum + (2.0 * wall).max(0.25 * (r_max - minimum));
        let mut picked: Vec<([f64; 2], f64)> = Vec::new();
        for floor in [comfortable, minimum] {
            let candidates: Vec<([f64; 2], f64)> = valid.iter().copied().filter(|(_, d)| *d >= floor).collect();
            let attempt = spread_keys(&candidates, request.count, spacing);
            if attempt.len() > picked.len() {
                picked = attempt;
            }
            if picked.len() >= request.count {
                break;
            }
        }
        if picked.len() > chosen.len() {
            chosen = picked;
            chosen_length = length;
        }
        if chosen.len() >= request.count {
            break;
        }
    }
    if chosen.is_empty() {
        plan.notes.push(format!(
            "No key fits: no point of the cut keeps a {:.2} mm socket {wall} mm from the edges and {wall} mm inside the \
             material along the normal on both sides. The part may be hollow or thin here; pass keys.diameter/length \
             smaller, or move the plane.",
            2.0 * socket_r
        ));
        return Ok(plan);
    }
    if chosen.len() < request.count {
        plan.notes.push(format!("Only {} of {} keys fit with {spacing:.2} mm spacing.", chosen.len(), request.count));
    }
    if chosen.len() == 1 {
        plan.notes.push("A single round key does not stop the halves rotating about it; glue sets the angle.".into());
    }
    plan.diameter = diameter;
    plan.length = chosen_length;
    plan.socket_diameter = round(2.0 * socket_r, 4);
    plan.socket_depth = round(chosen_length / 2.0 + c, 4);
    plan.positions = chosen.iter().map(|(p, _)| [round(p[0], 3), round(p[1], 3)]).collect();
    plan.edge_distance = chosen.iter().map(|(_, d)| round(*d, 3)).collect();
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn box_mesh(lo: V, hi: V) -> TriMesh {
        let corner = |i: usize| [if i & 1 != 0 { hi[0] } else { lo[0] }, if i & 2 != 0 { hi[1] } else { lo[1] }, if i & 4 != 0 { hi[2] } else { lo[2] }];
        let quads = [[0, 2, 3, 1], [4, 5, 7, 6], [0, 1, 5, 4], [2, 6, 7, 3], [0, 4, 6, 2], [1, 3, 7, 5]];
        let mut mesh = TriMesh { points: (0..8).map(corner).collect(), triangles: Vec::new() };
        for [a, b, c, d] in quads {
            mesh.triangles.push([a, b, c]);
            mesh.triangles.push([a, c, d]);
        }
        mesh
    }

    fn sphere(radius: f64, segments: usize) -> TriMesh {
        let mut mesh = TriMesh::default();
        let rings = segments / 2;
        for r in 0..=rings {
            let phi = std::f64::consts::PI * r as f64 / rings as f64;
            for s in 0..segments {
                let theta = 2.0 * std::f64::consts::PI * s as f64 / segments as f64;
                mesh.points.push([radius * phi.sin() * theta.cos(), radius * phi.sin() * theta.sin(), radius * phi.cos()]);
            }
        }
        let id = |r: usize, s: usize| (r * segments + s % segments) as u32;
        for r in 0..rings {
            for s in 0..segments {
                mesh.triangles.push([id(r, s), id(r + 1, s), id(r + 1, s + 1)]);
                mesh.triangles.push([id(r, s), id(r + 1, s + 1), id(r, s + 1)]);
            }
        }
        crate::meshimport::clean(mesh, crate::meshimport::CleanOptions { triangle_budget: 100_000 }).0
    }

    #[test]
    fn box_is_valid_and_hull_of_two_boxes_spans_both() {
        let a = box_mesh([0.0; 3], [10.0; 3]);
        assert_eq!(summary(&a)["valid"], json!(true));
        assert!((signed_volume(&a) - 1000.0).abs() < 1e-9);
        let both = concat([a, box_mesh([20.0, 0.0, 0.0], [30.0, 10.0, 10.0])]);
        let (hull, vertices) = convex_hull(&both).unwrap();
        assert_eq!(vertices, 8);
        assert!((signed_volume(&hull) - 3000.0).abs() < 1e-6, "{}", signed_volume(&hull));
        assert_eq!(summary(&hull)["valid"], json!(true));
    }

    #[test]
    fn hull_prefilter_keeps_the_hull_of_a_sphere() {
        let s = sphere(10.0, 48);
        let (hull, _) = convex_hull(&s).unwrap();
        assert!((signed_volume(&hull) - signed_volume(&s)).abs() < 1e-6 * signed_volume(&s).abs());
    }

    #[test]
    fn keys_sit_inside_a_block_cut() {
        let block = box_mesh([0.0, 0.0, 0.0], [40.0, 20.0, 30.0]);
        let section = crate::meshanalysis::section(&block, [0.0, 0.0, 15.0], [0.0, 0.0, 1.0], 0.0);
        let (frame, loops) = section_frame(&section, [0.0, 0.0, 1.0]);
        assert!((dot(frame.n, [0.0, 0.0, 1.0]) - 1.0).abs() < 1e-9);
        let request = KeyRequest { count: 2, diameter: None, length: None, clearance: 0.2, min_wall: 1.2, gap: 0.1, pins: false };
        let plan = plan_keys(&loops, &frame, &MeshIndex::new(&block), request, None).unwrap();
        assert_eq!(plan.positions.len(), 2, "{plan:?}");
        assert!(plan.diameter >= 2.0 && plan.diameter <= 6.0);
        for p in &plan.positions {
            let m = frame.to_model([p[0], p[1], 0.0]);
            assert!(m[0] > 4.0 && m[0] < 36.0 && m[1] > 4.0 && m[1] < 16.0, "{m:?}");
        }
        let a = frame.to_model([plan.positions[0][0], plan.positions[0][1], 0.0]);
        let b = frame.to_model([plan.positions[1][0], plan.positions[1][1], 0.0]);
        assert!(norm(sub(a, b)) > 15.0, "keys spread apart: {a:?} {b:?}");
    }

    /// Every socket keeps the full minWall on a tapering part, along its
    /// whole depth, and a key too short to bridge the gap is refused.
    #[test]
    fn keys_keep_the_full_wall_and_bridge_the_gap() {
        let (b, t) = (20.0, 5.0);
        let points = vec![[-b, -b, 0.0], [b, -b, 0.0], [-b, b, 0.0], [b, b, 0.0], [-t, -t, 30.0], [t, -t, 30.0], [-t, t, 30.0], [t, t, 30.0]];
        let quads = [[0, 2, 3, 1], [4, 5, 7, 6], [0, 1, 5, 4], [2, 6, 7, 3], [0, 4, 6, 2], [1, 3, 7, 5]];
        let mut frustum = TriMesh { points, triangles: Vec::new() };
        for [a, b, c, d] in quads {
            frustum.triangles.push([a, b, c]);
            frustum.triangles.push([a, c, d]);
        }
        let index = MeshIndex::new(&frustum);
        let section = crate::meshanalysis::section(&frustum, [0.0, 0.0, 12.0], [0.0, 0.0, 1.0], 0.0);
        let (frame, loops) = section_frame(&section, [0.0, 0.0, 1.0]);
        let request = KeyRequest { count: 4, diameter: Some(4.0), length: Some(14.0), clearance: 0.2, min_wall: 1.5, gap: 0.2, pins: false };
        let plan = plan_keys(&loops, &frame, &index, request, None).unwrap();
        assert!(!plan.positions.is_empty(), "{plan:?}");
        let r = plan.socket_diameter / 2.0;
        for p in &plan.positions {
            for side in [1.0, -1.0] {
                for k in 0..=100 {
                    let z = side * (0.1 + plan.socket_depth * k as f64 / 100.0);
                    let q = frame.to_model([p[0], p[1], z]);
                    assert!(index.contains(q) && index.distance(q) >= r + 1.5 - 1e-6, "{p:?} z={z}: {}", index.distance(q));
                }
            }
        }
        let short = KeyRequest { gap: 5.0, length: Some(1.0), ..request };
        assert!(plan_keys(&loops, &frame, &index, short, None).unwrap_err().contains("glue gap"));
        let auto = plan_keys(&loops, &frame, &index, KeyRequest { gap: 5.0, length: None, ..request }, None).unwrap();
        assert!((auto.length - 5.0) / 2.0 >= (auto.diameter / 2.0).max(1.0), "{auto:?}");
    }
}
