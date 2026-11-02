//! `fit_surface`: constrained tensor-product B-spline patches fitted to a
//! scan surface or part of one, checked against all of it.
//!
//! The surface is parameterised over a frame it is a height field of: its
//! best-fit plane (u, v along the plane, w along its normal) or a fitted
//! cylinder (u along the circumference at the cylinder's radius, v along
//! the axis, w radial). The (u, v) footprint is split into a grid of
//! patches, each a clamped uniform bicubic B-spline of w with n x n control
//! values. Neighbouring patches share their boundary curve (C0) and, by
//! default, their cross-boundary derivative (C1 of the height field, so the
//! surface is tangent continuous, G1); a cylinder that goes all the way
//! round also closes up across its seam. Shared values are one unknown, so
//! the constraints hold exactly, not as penalties. A second-difference
//! penalty (smoothness) keeps the controls with no data under them, and
//! the curvature, in check. The grid grows until every accepted sample is
//! within tolerance or the control budget runs out.
//!
//! Every reply measures the fit over the whole surface, not over the samples
//! it accepted: fit residuals, then an independent comparison of the
//! tessellated fit with the scan surface by exact nearest distance both
//! ways. The result is pure SCAD (a shell module) that is compiled before
//! the reply. Nothing is stored: the surface enters the design as a revision
//! when the caller pastes that SCAD in, and compares from there as an object.

use crate::assets::TriMesh;
use crate::meshanalysis::MeshIndex;
use crate::meshops;
use crate::scancommon::{self as sc, add, cross, dot, eigen3, norm, r3, round_to, scale, sub, unit, Design, SceneObject, Stats, P};
use crate::AppState;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const BUDGET: Duration = Duration::from_secs(90);
/// Samples drawn from the surface for the fit.
const DEFAULT_SAMPLES: usize = 12_000;
const MAX_SAMPLES: usize = 40_000;
/// Unknowns of the dense least-squares system.
const MAX_UNKNOWNS: usize = 1600;
/// Cells per patch and direction of the coverage mask and tessellation.
const MASK: usize = 24;

pub(crate) fn tool_definitions() -> Vec<Value> {
    let surface = json!({
        "description": "The surface to fit: {asset: scan, frameOf: <object importing the scan>} (or matrix), or object IDs.",
        "anyOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}, {"type": "object"}]
    });
    vec![crate::mcp_tool(
        "fit_surface",
        "Fit constrained bicubic B-spline patches to part of a scan (within {min, max} keeps to the faces \
         touching that box) and check the fit against all of that part. projection: \
         plane (a height field over the best-fit plane) or cylinder (a full turn closes across its seam). \
         patches [pu, pv] (default [1, 1]) share boundary curves (C0) and, by default, slope (G1). \
         maxControlPoints is per patch and direction: the grid grows from 4 until every accepted sample \
         is within tolerance (mm; default the design's). Samples farther than 3 x tolerance and 4 x rms \
         are rejected up to maxRejectFraction of the area and listed. Reply: comparison (independent, \
         both ways), unsupported (patch area with no data), and scad (<prefix>_shell(thickness) builds a \
         printable slab; compiled and checked). Nothing is stored: paste scad into the design as a \
         revision to use the surface, and compare_surfaces its object against the scan to recheck it.",
        json!({
            "type": "object",
            "required": ["designId", "surface"],
            "properties": {
                "within": sc::within_schema(sc::SURFACE_FRAME),
                "designId": {"type": "string"},
                "surface": surface,
                "projection": {"type": "string", "enum": ["plane", "cylinder"], "default": "plane"},
                "patches": {"type": "array", "items": {"type": "integer", "minimum": 1, "maximum": 6}, "minItems": 2, "maxItems": 2},
                "continuity": {"type": "string", "enum": ["C0", "G1"], "default": "G1"},
                "maxControlPoints": {"type": "integer", "minimum": 4, "maximum": 32, "default": 12},
                "tolerance": {"type": "number", "exclusiveMinimum": 0},
                "smoothness": {"type": "number", "minimum": 0, "maximum": 10, "default": 0.001},
                "maxRejectFraction": {"type": "number", "minimum": 0, "maximum": 0.2, "default": 0.02},
                "samples": {"type": "integer", "minimum": 200, "maximum": MAX_SAMPLES, "default": DEFAULT_SAMPLES},
                "thickness": {"type": "number", "exclusiveMinimum": 0, "default": 1, "description": "Default slab thickness, mm, on the side away from the surface's normal."},
                "prefix": {"type": "string", "pattern": "^[A-Za-z_][A-Za-z0-9_]{0,30}$", "default": "fit"},
                "detail": {"type": "boolean", "default": false},
                "image": {"anyOf": [{"type": "boolean"}, {"type": "object"}]}
            }
        }),
    )]
}

pub(crate) fn mcp_dispatch(state: &AppState, name: &str, arguments: &Value) -> Value {
    let result = match name {
        "fit_surface" => fit_value(state, arguments),
        _ => Err(format!("Unknown tool {name:?}.")),
    };
    match result {
        Ok((value, images)) => sc::image_result(sc::compact(value, arguments), images),
        Err(error) => crate::mcp_tool_error(&error),
    }
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// Where the height field lives: (u, v, w) <-> design coordinates.
#[derive(Clone)]
enum Frame {
    Plane { origin: P, e1: P, e2: P, n: P },
    /// u = arc length at `radius` from e1 toward e2, v along `axis`, w radial.
    Cylinder { origin: P, axis: P, e1: P, e2: P, radius: f64, periodic: bool },
}

impl Frame {
    fn local(&self, p: P) -> [f64; 3] {
        match self {
            Frame::Plane { origin, e1, e2, n } => {
                let d = sub(p, *origin);
                [dot(d, *e1), dot(d, *e2), dot(d, *n)]
            }
            Frame::Cylinder { origin, axis, e1, e2, radius, .. } => {
                let d = sub(p, *origin);
                let v = dot(d, *axis);
                let (x, y) = (dot(d, *e1), dot(d, *e2));
                [y.atan2(x) * radius, v, (x * x + y * y).sqrt() - radius]
            }
        }
    }

    fn world(&self, u: f64, v: f64, w: f64) -> P {
        match self {
            Frame::Plane { origin, e1, e2, n } => add(add(add(*origin, scale(*e1, u)), scale(*e2, v)), scale(*n, w)),
            Frame::Cylinder { origin, axis, e1, e2, radius, .. } => {
                let a = u / radius;
                add(add(*origin, scale(*axis, v)), scale(add(scale(*e1, a.cos()), scale(*e2, a.sin())), radius + w))
            }
        }
    }

    /// The direction of +w at a design point.
    fn up(&self, p: P) -> P {
        match self {
            Frame::Plane { n, .. } => *n,
            Frame::Cylinder { origin, axis, .. } => {
                let d = sub(p, *origin);
                unit(sub(d, scale(*axis, dot(d, *axis))))
            }
        }
    }

    /// Length along u per unit of u at height w (1 for a plane).
    fn u_stretch(&self, w: f64) -> f64 {
        match self {
            Frame::Plane { .. } => 1.0,
            Frame::Cylinder { radius, .. } => (radius + w) / radius,
        }
    }

    fn public(&self) -> Value {
        match self {
            Frame::Plane { origin, e1, e2, n } => json!({"kind": "plane", "origin": r3(*origin), "u": r3(*e1), "v": r3(*e2), "w": r3(*n)}),
            Frame::Cylinder { origin, axis, e1, e2, radius, periodic } => json!({
                "kind": "cylinder", "origin": r3(*origin), "axis": r3(*axis), "e1": r3(*e1), "e2": r3(*e2),
                "radius": round_to(*radius, 4), "periodic": periodic,
            }),
        }
    }
}

fn plane_frame(samples: &[sc::Sample]) -> Frame {
    let total: f64 = samples.iter().map(|s| s.weight).sum::<f64>().max(1e-12);
    let center = scale(samples.iter().fold([0.0; 3], |acc, s| add(acc, scale(s.point, s.weight))), 1.0 / total);
    let mut cov = [[0.0; 3]; 3];
    for s in samples {
        let d = sub(s.point, center);
        for i in 0..3 {
            for j in 0..3 {
                cov[i][j] += s.weight * d[i] * d[j];
            }
        }
    }
    let (_, axes) = eigen3(cov);
    let mean_normal = samples.iter().fold([0.0; 3], |acc, s| add(acc, scale(s.normal, s.weight)));
    let mut n = axes[2];
    if dot(n, mean_normal) < 0.0 {
        n = scale(n, -1.0);
    }
    let e1 = axes[0];
    let e2 = unit(cross(n, e1));
    Frame::Plane { origin: center, e1, e2, n }
}

fn cylinder_frame(samples: &[sc::Sample]) -> Result<Frame, String> {
    // The axis is the direction the normals do not vary along.
    let mut cov = [[0.0; 3]; 3];
    for s in samples {
        for i in 0..3 {
            for j in 0..3 {
                cov[i][j] += s.weight * s.normal[i] * s.normal[j];
            }
        }
    }
    let (_, axes) = eigen3(cov);
    let axis = axes[2];
    let total: f64 = samples.iter().map(|s| s.weight).sum::<f64>().max(1e-12);
    let center = scale(samples.iter().fold([0.0; 3], |acc, s| add(acc, scale(s.point, s.weight))), 1.0 / total);
    let a1 = axes[0];
    let a2 = unit(cross(axis, a1));
    // Algebraic circle fit in the plane across the axis: x^2 + y^2 + Dx + Ey + F = 0.
    let mut m = [[0.0; 3]; 3];
    let mut b = [0.0; 3];
    for s in samples {
        let d = sub(s.point, center);
        let (x, y) = (dot(d, a1), dot(d, a2));
        let row = [x, y, 1.0];
        let target = -(x * x + y * y);
        for i in 0..3 {
            for j in 0..3 {
                m[i][j] += s.weight * row[i] * row[j];
            }
            b[i] += s.weight * row[i] * target;
        }
    }
    let solution = solve(&m.iter().flatten().copied().collect::<Vec<_>>(), &b, 3).ok_or("The surface does not determine a cylinder (its normals do not turn around an axis).")?;
    let (cx, cy) = (-solution[0] / 2.0, -solution[1] / 2.0);
    let radius = (cx * cx + cy * cy - solution[2]).sqrt();
    if !radius.is_finite() || radius <= 0.0 {
        return Err("No cylinder fits this surface: use projection plane.".into());
    }
    let origin = add(center, add(scale(a1, cx), scale(a2, cy)));
    // Start the angle opposite the gap in the data, so u is continuous.
    let mut angles: Vec<f64> = samples.iter().map(|s| {
        let d = sub(s.point, origin);
        dot(d, a2).atan2(dot(d, a1))
    }).collect();
    angles.sort_by(f64::total_cmp);
    let mut gap = (angles[0] + std::f64::consts::TAU - angles[angles.len() - 1], angles[angles.len() - 1]);
    for pair in angles.windows(2) {
        if pair[1] - pair[0] > gap.0 {
            gap = (pair[1] - pair[0], pair[0]);
        }
    }
    let periodic = gap.0 < 0.05 * std::f64::consts::TAU;
    // e1 points to the middle of the gap's far side: angles then run over (-pi, pi).
    let start = gap.1 + gap.0 / 2.0 + std::f64::consts::PI;
    let e1 = add(scale(a1, start.cos()), scale(a2, start.sin()));
    let e2 = unit(cross(axis, e1));
    Ok(Frame::Cylinder { origin, axis, e1, e2, radius, periodic })
}

// ---------------------------------------------------------------------------
// B-spline patches
// ---------------------------------------------------------------------------

/// Clamped uniform cubic knots for n controls on [0, 1].
fn knots(n: usize) -> Vec<f64> {
    let m = n - 3;
    let mut k = vec![0.0; 4];
    k.extend((1..m).map(|i| i as f64 / m as f64));
    k.extend([1.0; 4]);
    k
}

/// The first control index and the 4 cubic basis weights at t in [0, 1].
fn basis(n: usize, knots: &[f64], t: f64) -> (usize, [f64; 4]) {
    let m = n - 3;
    let t = t.clamp(0.0, 1.0);
    let s = 3 + ((t * m as f64).floor() as usize).min(m - 1);
    // The basis functions of span s (Piegl & Tiller, A2.2).
    let mut b = [0.0; 4];
    let (mut left, mut right) = ([0.0; 4], [0.0; 4]);
    b[0] = 1.0;
    for j in 1..=3 {
        left[j] = t - knots[s + 1 - j];
        right[j] = knots[s + j] - t;
        let mut saved = 0.0;
        for r in 0..j {
            let temp = b[r] / (right[r + 1] + left[j - r]);
            b[r] = saved + right[r + 1] * temp;
            saved = left[j - r] * temp;
        }
        b[j] = saved;
    }
    (s - 3, b)
}

/// Derivative weights (d/dt) of the cubic basis at t.
fn basis_derivative(n: usize, knots: &[f64], t: f64) -> (usize, [f64; 4]) {
    let h = 1e-6;
    let (s0, a) = basis(n, knots, (t - h).max(0.0));
    let (s1, b) = basis(n, knots, (t + h).min(1.0));
    let dt = (t + h).min(1.0) - (t - h).max(0.0);
    // Express both in the window starting at the lower first index.
    let start = s0.min(s1);
    let mut out = [0.0; 4];
    for j in 0..4 {
        if let Some(slot) = (s1 + j).checked_sub(start).filter(|&k| k < 4) {
            out[slot] += b[j] / dt;
        }
        if let Some(slot) = (s0 + j).checked_sub(start).filter(|&k| k < 4) {
            out[slot] -= a[j] / dt;
        }
    }
    (start, out)
}

/// A control value as a combination of unknowns.
type Combo = Vec<(usize, f64)>;

fn combine(parts: &[(&Combo, f64)]) -> Combo {
    let mut map: HashMap<usize, f64> = HashMap::new();
    for (combo, factor) in parts {
        for &(var, coef) in *combo {
            *map.entry(var).or_insert(0.0) += coef * factor;
        }
    }
    let mut out: Combo = map.into_iter().filter(|(_, c)| c.abs() > 1e-15).collect();
    out.sort_by_key(|(var, _)| *var);
    out
}

struct Layout {
    pu: usize,
    pv: usize,
    n: usize,
    /// Patch rectangles in (u, v).
    rects: Vec<[f64; 4]>,
    /// combos[patch][b * n + a]
    combos: Vec<Vec<Combo>>,
    unknowns: usize,
    knots: Vec<f64>,
}

impl Layout {
    fn new(domain: [f64; 4], pu: usize, pv: usize, n: usize, g1: bool, periodic: bool) -> Self {
        let mut rects = Vec::new();
        for j in 0..pv {
            for i in 0..pu {
                let u0 = domain[0] + (domain[1] - domain[0]) * i as f64 / pu as f64;
                let u1 = domain[0] + (domain[1] - domain[0]) * (i + 1) as f64 / pu as f64;
                let v0 = domain[2] + (domain[3] - domain[2]) * j as f64 / pv as f64;
                let v1 = domain[2] + (domain[3] - domain[2]) * (j + 1) as f64 / pv as f64;
                rects.push([u0, u1, v0, v1]);
            }
        }
        let mut combos: Vec<Vec<Combo>> = Vec::with_capacity(pu * pv);
        let mut unknowns = 0;
        for j in 0..pv {
            for i in 0..pu {
                let mut own: Vec<Combo> = vec![Vec::new(); n * n];
                for b in 0..n {
                    for a in 0..n {
                        let left = (i > 0).then(|| j * pu + i - 1);
                        let first = (periodic && i == pu - 1).then_some(j * pu);
                        let below = (j > 0).then(|| (j - 1) * pu + i);
                        let get = |patch: usize, a: usize, b: usize, combos: &Vec<Vec<Combo>>, own: &Vec<Combo>| -> Combo {
                            if patch == j * pu + i { own[b * n + a].clone() } else { combos[patch][b * n + a].clone() }
                        };
                        // Only the controls a constraint ties take this path:
                        // the boundary row (C0), and with G1 the row next to
                        // it. Everything else is free, or tied the other way.
                        let tied_left = a == 0 || (g1 && a == 1);
                        let tied_seam = a == n - 1 || (g1 && a == n - 2 && n > 3);
                        let tied_below = b == 0 || (g1 && b == 1);
                        let combo = if let (Some(l), true) = (left, tied_left) {
                            if a == 0 {
                                get(l, n - 1, b, &combos, &own)
                            } else {
                                combine(&[(&get(l, n - 1, b, &combos, &own), 2.0), (&get(l, n - 2, b, &combos, &own), -1.0)])
                            }
                        } else if let (Some(f), true) = (first, tied_seam && (pu > 1 || a >= 2)) {
                            if a == n - 1 {
                                get(f, 0, b, &combos, &own)
                            } else {
                                combine(&[(&get(f, 0, b, &combos, &own), 2.0), (&get(f, 1, b, &combos, &own), -1.0)])
                            }
                        } else if let (Some(d), true) = (below, tied_below) {
                            if b == 0 {
                                get(d, a, n - 1, &combos, &own)
                            } else {
                                combine(&[(&get(d, a, n - 1, &combos, &own), 2.0), (&get(d, a, n - 2, &combos, &own), -1.0)])
                            }
                        } else {
                            unknowns += 1;
                            vec![(unknowns - 1, 1.0)]
                        };
                        own[b * n + a] = combo;
                    }
                }
                combos.push(own);
            }
        }
        Self { pu, pv, n, rects, combos, unknowns, knots: knots(n) }
    }

    /// The patch a (u, v) falls in and its local (s, t).
    fn locate(&self, u: f64, v: f64) -> (usize, f64, f64) {
        let first = self.rects[0];
        let last = self.rects[self.rects.len() - 1];
        let fu = ((u - first[0]) / (last[1] - first[0])).clamp(0.0, 1.0);
        let fv = ((v - first[2]) / (last[3] - first[2])).clamp(0.0, 1.0);
        let i = ((fu * self.pu as f64).floor() as usize).min(self.pu - 1);
        let j = ((fv * self.pv as f64).floor() as usize).min(self.pv - 1);
        let k = j * self.pu + i;
        let r = self.rects[k];
        (k, ((u - r[0]) / (r[1] - r[0])).clamp(0.0, 1.0), ((v - r[2]) / (r[3] - r[2])).clamp(0.0, 1.0))
    }

    /// The sparse row (unknown, coefficient) of w at (u, v).
    fn row(&self, u: f64, v: f64) -> Combo {
        let (k, s, t) = self.locate(u, v);
        let (a0, bu) = basis(self.n, &self.knots, s);
        let (b0, bv) = basis(self.n, &self.knots, t);
        let mut parts: Vec<(&Combo, f64)> = Vec::with_capacity(16);
        for (jb, wv) in bv.iter().enumerate() {
            for (ja, wu) in bu.iter().enumerate() {
                let weight = wu * wv;
                if weight != 0.0 {
                    parts.push((&self.combos[k][(b0 + jb) * self.n + a0 + ja], weight));
                }
            }
        }
        combine(&parts)
    }

    fn controls(&self, x: &[f64]) -> Vec<Vec<f64>> {
        self.combos.iter().map(|own| own.iter().map(|combo| combo.iter().map(|&(var, coef)| x[var] * coef).sum()).collect()).collect()
    }
}

/// Evaluated patches: control values per patch.
struct Fit {
    layout: Layout,
    controls: Vec<Vec<f64>>,
}

impl Fit {
    fn height(&self, u: f64, v: f64) -> f64 {
        let (k, s, t) = self.layout.locate(u, v);
        self.height_in(k, s, t)
    }

    fn height_in(&self, k: usize, s: f64, t: f64) -> f64 {
        let n = self.layout.n;
        let (a0, bu) = basis(n, &self.layout.knots, s);
        let (b0, bv) = basis(n, &self.layout.knots, t);
        let mut w = 0.0;
        for (jb, wv) in bv.iter().enumerate() {
            for (ja, wu) in bu.iter().enumerate() {
                w += wu * wv * self.controls[k][(b0 + jb) * n + a0 + ja];
            }
        }
        w
    }

    /// (dw/du, dw/dv) at local (s, t) of patch k.
    fn slope_in(&self, k: usize, s: f64, t: f64) -> (f64, f64) {
        let n = self.layout.n;
        let r = self.layout.rects[k];
        let (a0, bu) = basis(n, &self.layout.knots, s);
        let (b0, bv) = basis(n, &self.layout.knots, t);
        let (da, du) = basis_derivative(n, &self.layout.knots, s);
        let (db, dv) = basis_derivative(n, &self.layout.knots, t);
        let at = |a: usize, b: usize| self.controls[k][b.min(n - 1) * n + a.min(n - 1)];
        let mut wu = 0.0;
        let mut wv = 0.0;
        for jb in 0..4 {
            for ja in 0..4 {
                wu += du[ja] * bv[jb] * at(da + ja, b0 + jb);
                wv += bu[ja] * dv[jb] * at(a0 + ja, db + jb);
            }
        }
        (wu / (r[1] - r[0]), wv / (r[3] - r[2]))
    }

    fn slope(&self, u: f64, v: f64) -> (f64, f64) {
        let (k, s, t) = self.layout.locate(u, v);
        self.slope_in(k, s, t)
    }
}

/// Solves the symmetric positive definite system `a x = b` (row-major n x n)
/// by Cholesky; `None` when it is not positive definite.
fn solve(a: &[f64], b: &[f64], n: usize) -> Option<Vec<f64>> {
    let mut l = vec![0.0; n * n];
    for i in 0..n {
        for j in 0..=i {
            let mut sum = a[i * n + j];
            for k in 0..j {
                sum -= l[i * n + k] * l[j * n + k];
            }
            if i == j {
                if !(sum > 0.0) {
                    return None;
                }
                l[i * n + i] = sum.sqrt();
            } else {
                l[i * n + j] = sum / l[j * n + j];
            }
        }
    }
    let mut y = vec![0.0; n];
    for i in 0..n {
        let mut sum = b[i];
        for k in 0..i {
            sum -= l[i * n + k] * y[k];
        }
        y[i] = sum / l[i * n + i];
    }
    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        let mut sum = y[i];
        for k in i + 1..n {
            sum -= l[k * n + i] * x[k];
        }
        x[i] = sum / l[i * n + i];
    }
    Some(x)
}

/// A sample in the frame: (u, v, w, area weight).
#[derive(Clone, Copy)]
struct Local {
    u: f64,
    v: f64,
    w: f64,
    weight: f64,
}

/// Weighted least squares over the accepted samples plus the second-
/// difference penalty, with the constraints built into the unknowns.
fn least_squares(layout: Layout, data: &[Local], accepted: &[bool], smoothness: f64, deadline: Instant) -> Result<Fit, String> {
    let v = layout.unknowns;
    let mut ata = vec![0.0; v * v];
    let mut atb = vec![0.0; v];
    let add_row = |row: &Combo, target: f64, weight: f64, ata: &mut Vec<f64>, atb: &mut Vec<f64>| {
        for &(i, ci) in row {
            atb[i] += weight * ci * target;
            for &(j, cj) in row {
                ata[i * v + j] += weight * ci * cj;
            }
        }
    };
    let mut total = 0.0;
    for (index, sample) in data.iter().enumerate() {
        if index % 4096 == 0 {
            meshops::check(Some(deadline))?;
        }
        if !accepted[index] {
            continue;
        }
        total += sample.weight;
        add_row(&layout.row(sample.u, sample.v), sample.w, sample.weight, &mut ata, &mut atb);
    }
    // Second differences along u, along v, and mixed, per patch.
    let n = layout.n;
    let mut penalty: Vec<Combo> = Vec::new();
    for own in &layout.combos {
        for b in 0..n {
            for a in 0..n {
                if a + 2 < n {
                    penalty.push(combine(&[(&own[b * n + a], 1.0), (&own[b * n + a + 1], -2.0), (&own[b * n + a + 2], 1.0)]));
                }
                if b + 2 < n {
                    penalty.push(combine(&[(&own[b * n + a], 1.0), (&own[(b + 1) * n + a], -2.0), (&own[(b + 2) * n + a], 1.0)]));
                }
                if a + 1 < n && b + 1 < n {
                    penalty.push(combine(&[(&own[b * n + a], 1.0), (&own[b * n + a + 1], -1.0), (&own[(b + 1) * n + a], -1.0), (&own[(b + 1) * n + a + 1], 1.0)]));
                }
            }
        }
    }
    // Relative to the data's weight, so the setting means the same at any size.
    let lambda = smoothness.max(1e-9) * total.max(1e-12) / penalty.len().max(1) as f64;
    for row in &penalty {
        add_row(row, 0.0, lambda, &mut ata, &mut atb);
    }
    meshops::check(Some(deadline))?;
    let trace: f64 = (0..v).map(|i| ata[i * v + i]).sum::<f64>() / v.max(1) as f64;
    for i in 0..v {
        ata[i * v + i] += 1e-12 * trace.max(1e-12);
    }
    let x = solve(&ata, &atb, v).ok_or("The fit's normal equations are singular: raise smoothness or use fewer patches.")?;
    let controls = layout.controls(&x);
    Ok(Fit { layout, controls })
}

/// Distance along the surface normal from a sample to the fit (signed: +
/// above the fit along +w).
fn residual(fit: &Fit, frame: &Frame, sample: &Local) -> f64 {
    let (wu, wv) = fit.slope(sample.u, sample.v);
    let wu = wu / frame.u_stretch(sample.w);
    (sample.w - fit.height(sample.u, sample.v)) / (1.0 + wu * wu + wv * wv).sqrt()
}

// ---------------------------------------------------------------------------
// The tool
// ---------------------------------------------------------------------------

pub(crate) fn fit_value(state: &AppState, input: &Value) -> Result<(Value, Vec<(String, Vec<u8>)>), String> {
    let started = Instant::now();
    let deadline = started + BUDGET;
    let _scope = crate::csg::bsp::deadline_scope(deadline);
    let id = crate::argument_workspace_id(input)?;
    let tolerance = sc::number(input, "tolerance", state.workspaces.get(id).ok().flatten().map_or(0.2, |w| w.tolerance), 1e-6, 100.0)?;
    let smoothness = sc::number(input, "smoothness", 0.001, 0.0, 10.0)?;
    let budget = sc::integer(input, "maxControlPoints", 12, 4, 32)?;
    let max_reject = sc::number(input, "maxRejectFraction", 0.02, 0.0, 0.2)?;
    let sample_count = sc::integer(input, "samples", DEFAULT_SAMPLES, 200, MAX_SAMPLES)?;
    let thickness = sc::number(input, "thickness", 1.0, 1e-3, 1e3)?;
    let detail = sc::flag(input, "detail", false)?;
    let image = sc::image_wanted(input, true)?;
    let g1 = match input.get("continuity").and_then(Value::as_str) {
        None | Some("G1") => true,
        Some("C0") => false,
        Some(other) => return Err(format!("continuity must be \"C0\" or \"G1\", not {other:?}.")),
    };
    let (pu, pv) = match input.get("patches") {
        None | Some(Value::Null) => (1, 1),
        Some(Value::Array(items)) if items.len() == 2 => (
            items[0].as_u64().filter(|n| (1..=6).contains(n)).ok_or("patches holds two counts from 1 to 6.")? as usize,
            items[1].as_u64().filter(|n| (1..=6).contains(n)).ok_or("patches holds two counts from 1 to 6.")? as usize,
        ),
        Some(_) => return Err("patches must be [pu, pv].".into()),
    };
    let prefix = input.get("prefix").and_then(Value::as_str).unwrap_or("fit");
    if !prefix.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') || !prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || prefix.len() > 31 {
        return Err("prefix must be a SCAD identifier (letters, digits, _), at most 31 characters.".into());
    }
    crate::assetstore::design_exists(state, id).map_err(|error| error.message)?;
    let mut design = Design::new(state, input);
    let surface = design.surface(input.get("surface").ok_or("surface is required: the surface to fit, e.g. {asset: \"vent.stl\", frameOf: \"background-1\"}.")?, "surface")?;
    let keep_to = sc::Within::parse(input)?;
    let kept = keep_to.as_ref().map(|within| within.stats(&surface.mesh));
    if kept.is_some_and(|stats| stats.triangles == 0) {
        return Err(format!("Nothing inside the box: {} has no surface within it, so nothing was fitted.", surface.label));
    }
    let surface = match &keep_to {
        Some(within) => surface.cropped(within),
        None => surface,
    };
    let samples = sc::sample_surface(&surface.mesh, sample_count);
    if samples.len() < 32 {
        return Err("The surface is too small to fit.".into());
    }
    let projection = input.get("projection").and_then(Value::as_str).unwrap_or("plane");
    let frame = match projection {
        "plane" => plane_frame(&samples),
        "cylinder" => cylinder_frame(&samples)?,
        other => return Err(format!("projection must be \"plane\" or \"cylinder\", not {other:?}.")),
    };
    // A height field cannot fold over: surface facing sideways to the
    // frame cannot be fitted this way.
    let total_area: f64 = samples.iter().map(|s| s.weight).sum();
    let sideways: f64 = samples.iter().filter(|s| dot(s.normal, frame.up(s.point)).abs() < 0.2).map(|s| s.weight).sum();
    if sideways > 0.05 * total_area {
        return Err(format!(
            "{:.0}% of the surface faces sideways to its {projection} frame, so it is not a height field over it: fit a \
             smaller part of it (within; extract_features finds a scan's smooth surfaces and their boxes), use projection {}, or several fits.",
            100.0 * sideways / total_area,
            if projection == "plane" { "cylinder" } else { "plane" }
        ));
    }
    let data: Vec<Local> = samples.iter().map(|s| {
        let [u, v, w] = frame.local(s.point);
        Local { u, v, w, weight: s.weight }
    }).collect();
    let periodic = matches!(frame, Frame::Cylinder { periodic: true, .. });
    let mut domain = [f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY];
    for d in &data {
        domain = [domain[0].min(d.u), domain[1].max(d.u), domain[2].min(d.v), domain[3].max(d.v)];
    }
    if let Frame::Cylinder { radius, periodic: true, .. } = frame {
        domain[0] = -std::f64::consts::PI * radius;
        domain[1] = std::f64::consts::PI * radius;
    }
    if !(domain[1] - domain[0] > 1e-6 && domain[3] - domain[2] > 1e-6) {
        return Err("The surface has no extent in one direction of its frame.".into());
    }

    // Grow the control grid until the accepted samples are within tolerance.
    let max_n = budget.min(((MAX_UNKNOWNS as f64 / (pu * pv) as f64).sqrt() as usize).max(4));
    let mut attempts = Vec::new();
    let mut accepted = vec![true; data.len()];
    let mut n = 4;
    let (fit, residuals) = loop {
        let layout = Layout::new(domain, pu, pv, n, g1, periodic);
        let mut fit = least_squares(layout, &data, &accepted, smoothness, deadline)?;
        let mut residuals: Vec<f64> = data.iter().map(|d| residual(&fit, &frame, d)).collect();
        // Reject a small fraction of outliers once, then refit without them.
        let rms = (residuals.iter().zip(&data).map(|(r, d)| r * r * d.weight).sum::<f64>() / total_area).sqrt();
        let limit = (3.0 * tolerance).max(4.0 * rms);
        let outliers: Vec<usize> = (0..data.len()).filter(|&i| residuals[i].abs() > limit).collect();
        let outlier_area: f64 = outliers.iter().map(|&i| data[i].weight).sum();
        let mut rejected = vec![false; data.len()];
        if !outliers.is_empty() && outlier_area <= max_reject * total_area {
            for &i in &outliers {
                rejected[i] = true;
            }
            accepted = rejected.iter().map(|r| !r).collect();
            fit = least_squares(Layout::new(domain, pu, pv, n, g1, periodic), &data, &accepted, smoothness, deadline)?;
            residuals = data.iter().map(|d| residual(&fit, &frame, d)).collect();
        } else {
            accepted = vec![true; data.len()];
        }
        let max_accepted = (0..data.len()).filter(|&i| accepted[i]).map(|i| residuals[i].abs()).fold(0.0, f64::max);
        attempts.push(json!({"controlsPerPatch": n, "maxError": round_to(max_accepted, 4), "rejected": accepted.iter().filter(|a| !**a).count()}));
        if max_accepted <= tolerance || n >= max_n {
            break (fit, residuals);
        }
        n = (n + 2).min(max_n);
        meshops::check(Some(deadline))?;
    };
    let n = fit.layout.n;
    let rejected_area: f64 = (0..data.len()).filter(|&i| !accepted[i]).map(|i| data[i].weight).sum();

    // Residuals over every sample (rejected included): the whole surface.
    let all: Vec<(f64, f64)> = residuals.iter().zip(&data).map(|(r, d)| (*r, d.weight)).collect();
    let stats = Stats::of(&all);
    let within = all.iter().filter(|(r, _)| r.abs() <= tolerance).map(|(_, w)| w).sum::<f64>() / total_area;
    let mut residual_value = stats.public();
    residual_value["withinTolerance"] = json!(round_to(within, 4));
    residual_value["note"] = json!("Every sample of the surface, the rejected ones included, along the fit's normal.");

    // Coverage of each patch by data, and the tessellation of the covered part.
    let mut covered = vec![vec![false; MASK * MASK]; pu * pv];
    for d in &data {
        let (k, s, t) = fit.layout.locate(d.u, d.v);
        let (ci, cj) = (((s * MASK as f64) as usize).min(MASK - 1), ((t * MASK as f64) as usize).min(MASK - 1));
        covered[k][cj * MASK + ci] = true;
    }
    let mut fit_mesh = TriMesh::default();
    let mut unsupported = Vec::new();
    let mut unsupported_area = 0.0;
    let mut domain_area = 0.0;
    for (k, rect) in fit.layout.rects.iter().enumerate() {
        let corner = |ci: usize, cj: usize| {
            let (s, t) = (ci as f64 / MASK as f64, cj as f64 / MASK as f64);
            let (u, v) = (rect[0] + (rect[1] - rect[0]) * s, rect[2] + (rect[3] - rect[2]) * t);
            frame.world(u, v, fit.height_in(k, s, t))
        };
        let cell_area = (rect[1] - rect[0]) * (rect[3] - rect[2]) / (MASK * MASK) as f64;
        let mut index: HashMap<(usize, usize), u32> = HashMap::new();
        let mut vertex = |mesh: &mut TriMesh, ci: usize, cj: usize| {
            *index.entry((ci, cj)).or_insert_with(|| {
                mesh.points.push(corner(ci, cj));
                (mesh.points.len() - 1) as u32
            })
        };
        let mut holes: Vec<bool> = vec![false; MASK * MASK];
        for cj in 0..MASK {
            for ci in 0..MASK {
                domain_area += cell_area;
                if covered[k][cj * MASK + ci] {
                    let (a, b, c, d) = (vertex(&mut fit_mesh, ci, cj), vertex(&mut fit_mesh, ci + 1, cj), vertex(&mut fit_mesh, ci + 1, cj + 1), vertex(&mut fit_mesh, ci, cj + 1));
                    fit_mesh.triangles.push([a, b, c]);
                    fit_mesh.triangles.push([a, c, d]);
                } else {
                    holes[cj * MASK + ci] = true;
                    unsupported_area += cell_area;
                }
            }
        }
        // Clusters of uncovered cells: where the patch runs on smoothness alone.
        let mut seen = vec![false; MASK * MASK];
        for start in 0..MASK * MASK {
            if !holes[start] || seen[start] {
                continue;
            }
            let mut stack = vec![start];
            seen[start] = true;
            let mut cells = Vec::new();
            while let Some(c) = stack.pop() {
                cells.push(c);
                let (ci, cj) = (c % MASK, c / MASK);
                let mut push = |x: usize, y: usize| {
                    let c = y * MASK + x;
                    if holes[c] && !seen[c] {
                        seen[c] = true;
                        stack.push(c);
                    }
                };
                if ci > 0 { push(ci - 1, cj); }
                if ci + 1 < MASK { push(ci + 1, cj); }
                if cj > 0 { push(ci, cj - 1); }
                if cj + 1 < MASK { push(ci, cj + 1); }
            }
            let (mut s, mut t) = (0.0, 0.0);
            for &c in &cells {
                s += ((c % MASK) as f64 + 0.5) / MASK as f64;
                t += ((c / MASK) as f64 + 0.5) / MASK as f64;
            }
            let (s, t) = (s / cells.len() as f64, t / cells.len() as f64);
            let (u, v) = (rect[0] + (rect[1] - rect[0]) * s, rect[2] + (rect[3] - rect[2]) * t);
            unsupported.push(json!({"patch": k, "area": round_to(cells.len() as f64 * cell_area, 2), "center": r3(frame.world(u, v, fit.height_in(k, s, t)))}));
        }
    }
    unsupported.sort_by(|a, b| b["area"].as_f64().unwrap_or(0.0).total_cmp(&a["area"].as_f64().unwrap_or(0.0)));
    unsupported.truncate(8);
    meshops::check(Some(deadline))?;

    // Independent check: the fitted surface against the scan, exact
    // nearest distances, both ways.
    let scan_index = surface.index();
    let fit_index = MeshIndex::new(&fit_mesh);
    let to_fit: Vec<(f64, f64)> = samples.iter().filter_map(|s| fit_index.distance(s.point).is_finite().then(|| (fit_index.distance(s.point), s.weight))).collect();
    let fit_samples = sc::sample_surface(&fit_mesh, sample_count.min(8000));
    let to_scan: Vec<(f64, f64)> = fit_samples.iter().map(|s| (scan_index.distance(s.point), s.weight)).collect();
    let (scan_to_fit, fit_to_scan) = (Stats::of(&to_fit), Stats::of(&to_scan));
    let comparison = json!({
        "scanToFit": scan_to_fit.public(),
        "fitToScan": fit_to_scan.public(),
        "note": "The tessellated fit (where data lies under it) against the surface, exact nearest distances: independent of the fit's own residuals.",
    });

    // Continuity across every shared boundary (and the seam).
    let mut c0_gap: f64 = 0.0;
    let mut tangent: f64 = 0.0;
    let mut check = |k1: usize, k2: usize, along_u: bool, fit: &Fit| {
        for q in 0..=8 {
            let x = q as f64 / 8.0;
            let ((s1, t1), (s2, t2)) = if along_u { ((1.0, x), (0.0, x)) } else { ((x, 1.0), (x, 0.0)) };
            c0_gap = c0_gap.max((fit.height_in(k1, s1, t1) - fit.height_in(k2, s2, t2)).abs());
            let (a, b) = (fit.slope_in(k1, s1, t1), fit.slope_in(k2, s2, t2));
            let normal = |(wu, wv): (f64, f64)| unit([-wu, -wv, 1.0]);
            tangent = tangent.max(dot(normal(a), normal(b)).clamp(-1.0, 1.0).acos().to_degrees());
        }
    };
    for j in 0..pv {
        for i in 0..pu {
            let k = j * pu + i;
            if i + 1 < pu {
                check(k, k + 1, true, &fit);
            } else if periodic {
                check(k, j * pu, true, &fit);
            }
            if j + 1 < pv {
                check(k, k + pu, false, &fit);
            }
        }
    }
    let boundaries = pv * (pu - 1 + periodic as usize) + pu * (pv - 1);

    // Which side the material goes: away from the surface's normal.
    let inside = if samples.iter().map(|s| dot(s.normal, frame.up(s.point)) * s.weight).sum::<f64>() >= 0.0 { -1.0 } else { 1.0 };
    let scad = scad_source(prefix, &frame, &fit, inside, thickness, g1);
    let validation = validate(state, prefix, &scad, &fit)?;

    let reasons: Vec<String> = [
        (stats.p95 > tolerance).then(|| format!("residual p95 {} mm > tolerance", sc::short(stats.p95))),
        (scan_to_fit.p95.max(fit_to_scan.p95) > tolerance).then(|| {
            format!("independent comparison p95 {} mm > tolerance (scan to fit {}, fit to scan {})", sc::short(scan_to_fit.p95.max(fit_to_scan.p95)), sc::short(scan_to_fit.p95), sc::short(fit_to_scan.p95))
        }),
        (rejected_area > 0.0).then(|| format!("{:.1}% of the area rejected as outliers", 100.0 * rejected_area / total_area)),
        (unsupported_area > 0.1 * domain_area).then(|| format!("{:.0}% of the patch area has no data under it", 100.0 * unsupported_area / domain_area)),
        (validation["ok"] != json!(true)).then(|| "the generated SCAD did not reproduce the fit".to_string()),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut value = design.header();
    value["ok"] = json!(true);
    value["source"] = surface.brief();
    if let (Some(within), Some(stats)) = (&keep_to, kept) {
        value["within"] = within.public(stats.public());
    }
    value["tolerance"] = json!(tolerance);
    value["frame"] = frame.public();
    value["patches"] = json!([pu, pv]);
    value["patchRects"] = json!(fit.layout.rects.iter().map(|r| r.map(|x| round_to(x, 4))).collect::<Vec<_>>());
    value["controlsPerPatch"] = json!(n);
    value["unknowns"] = json!(fit.layout.unknowns);
    value["continuity"] = json!({
        "kind": if g1 { "G1" } else { "C0" },
        "boundaries": boundaries,
        "maxGap": round_to(c0_gap, 6),
        "maxTangentAngleDeg": round_to(tangent, 4),
    });
    value["withinTolerance"] = json!(reasons.is_empty());
    value["reasons"] = json!(reasons);
    value["residual"] = residual_value;
    value["comparison"] = comparison;
    value["unsupported"] = json!({"area": round_to(unsupported_area, 2), "fraction": round_to(unsupported_area / domain_area.max(1e-12), 4), "clusters": unsupported});
    value["rejected"] = json!({"samples": accepted.iter().filter(|a| !**a).count(), "area": round_to(rejected_area, 2), "fraction": round_to(rejected_area / total_area, 4)});
    value["attempts"] = json!(attempts);
    value["scad"] = json!(scad);
    value["scadCheck"] = validation;
    value["usage"] = json!(format!(
        "Paste scad into the design with update_design_code and call {prefix}_shell(thickness = {});  // or {prefix}_xyz(k, s, t, 0) for points on patch k. \
         Nothing is stored: to recheck, compare_surfaces {{reference: <the scan>, candidate: <the shell's object>, within}}.",
        sc::short(thickness)
    ));
    if detail {
        value["controls"] = json!(fit.controls.iter().map(|c| c.iter().map(|x| round_to(*x, 5)).collect::<Vec<_>>()).collect::<Vec<_>>());
    }
    let mut images = Vec::new();
    let colors_wanted = image.is_some() || crate::activity::collecting();
    if colors_wanted {
        // The surface coloured by its distance to the fit, per face.
        let colors: Vec<crate::raster::Rgba> = (0..surface.mesh.triangles.len())
            .map(|t| {
                let [a, b, c] = sc::corners(&surface.mesh, t);
                let p = scale(add(add(a, b), c), 1.0 / 3.0);
                let [u, v, w] = frame.local(p);
                let d = w - fit.height(u, v);
                sc::diverging(d, (3.0 * tolerance).max(stats.p95), tolerance)
            })
            .collect();
        if crate::activity::collecting() {
            crate::activity::offer_painted(&format!("fit_surface {}", surface.label), &surface.mesh, &colors, json!({"title": "distance to the fit, mm", "range": round_to((3.0 * tolerance).max(stats.p95), 4), "dead": tolerance, "signed": true, "keys": []}));
        }
        if let Some(image) = image {
            let legend = sc::Legend { title: "distance to the fit, mm".into(), range: (3.0 * tolerance).max(stats.p95), signed: true, dead: tolerance, keys: vec![] };
            let scene = vec![SceneObject::painted(&surface.label, &surface.mesh, &colors)];
            let (lo, hi) = surface.bounds().unwrap_or(([0.0; 3], [1.0; 3]));
            let view = sc::facing_view(&image, scale(add(lo, hi), 0.5), frame.up(scale(add(lo, hi), 0.5)), norm(sub(hi, lo)) * 2.2);
            let (png, _) = sc::render_scene(&scene, &view, Some(&legend), &[], None)?;
            images.push(("The surface coloured by its distance to the fitted surface (green within tolerance; red above, blue below the fit).".to_string(), png));
        }
    }
    value["elapsedMs"] = json!(started.elapsed().as_millis() as u64);
    Ok((value, images))
}

fn num(value: f64) -> String {
    let text = format!("{}", round_to(value, 6));
    if text == "-0" { "0".into() } else { text }
}

fn vec3(p: P) -> String {
    format!("[{}, {}, {}]", num(p[0]), num(p[1]), num(p[2]))
}

/// Pure SCAD evaluating the patches and building a slab under them.
fn scad_source(p: &str, frame: &Frame, fit: &Fit, inside: f64, thickness: f64, g1: bool) -> String {
    let n = fit.layout.n;
    let rects = fit.layout.rects.iter().map(|r| format!("[{}, {}, {}, {}]", num(r[0]), num(r[1]), num(r[2]), num(r[3]))).collect::<Vec<_>>().join(", ");
    let controls = fit
        .controls
        .iter()
        .map(|c| {
            let rows = (0..n).map(|b| format!("[{}]", (0..n).map(|a| num(c[b * n + a])).collect::<Vec<_>>().join(", "))).collect::<Vec<_>>().join(",\n    ");
            format!("[\n    {rows}]")
        })
        .collect::<Vec<_>>()
        .join(",\n  ");
    let knots = fit.layout.knots.iter().map(|k| num(*k)).collect::<Vec<_>>().join(", ");
    let frame_lines = match frame {
        Frame::Plane { origin, e1, e2, n } => format!(
            "{p}_kind = \"plane\";\n{p}_origin = {};\n{p}_e1 = {};\n{p}_e2 = {};\n{p}_up = {};\n",
            vec3(*origin), vec3(*e1), vec3(*e2), vec3(*n)
        ),
        Frame::Cylinder { origin, axis, e1, e2, radius, .. } => format!(
            "{p}_kind = \"cylinder\";\n{p}_origin = {};\n{p}_axis = {};\n{p}_e1 = {};\n{p}_e2 = {};\n{p}_radius = {};\n",
            vec3(*origin), vec3(*axis), vec3(*e1), vec3(*e2), num(*radius)
        ),
    };
    let to_world = match frame {
        Frame::Plane { .. } => format!("{p}_origin + u * {p}_e1 + v * {p}_e2 + w * {p}_up"),
        Frame::Cylinder { .. } => format!(
            "let (a = u / {p}_radius * 180 / PI) {p}_origin + v * {p}_axis + ({p}_radius + w) * (cos(a) * {p}_e1 + sin(a) * {p}_e2)"
        ),
    };
    format!(
        "// fit_surface: {count} bicubic B-spline patch{plural} ({n} x {n} controls each, {cont}) over a {kind} frame, mm.\n\
         // Edit {p}_w (per patch: rows along v, columns along u) to reshape; {p}_shell(thickness) builds a slab\n\
         // under the surface; {p}_xyz(k, s, t, off) is the point at (s, t) in [0, 1]^2 of patch k, off mm along +w.\n\
         {frame_lines}{p}_n = {n};\n{p}_knots = [{knots}];\n{p}_inside = {inside};\n\
         {p}_patches = [{rects}];\n\
         {p}_w = [\n  {controls}];\n\
         function {p}_span(t) = 3 + min(floor(t * ({p}_n - 3)), {p}_n - 4);\n\
         function {p}_db(t, s, d, r) = r > 3 ? d[3]\n\
         \x20   : {p}_db(t, s, [for (j = [0 : 1 : 3]) j < r ? d[j]\n\
         \x20       : let (i = j + s - 3, a = (t - {p}_knots[i]) / ({p}_knots[i + 4 - r] - {p}_knots[i])) (1 - a) * d[j - 1] + a * d[j]], r + 1);\n\
         function {p}_curve(c, t) = let (s = {p}_span(t)) {p}_db(t, s, [for (j = [0 : 1 : 3]) c[j + s - 3]], 1);\n\
         function {p}_height(k, s, t) = {p}_curve([for (row = {p}_w[k]) {p}_curve(row, s)], t);\n\
         function {p}_at(m, i, j) = j * m + i;\n\
         function {p}_xyz(k, s, t, off) = let (b = {p}_patches[k], u = b[0] + (b[1] - b[0]) * s, v = b[2] + (b[3] - b[2]) * t,\n\
         \x20   w = {p}_height(k, s, t) + off) {to_world};\n\
         module {p}_patch_shell(k, thickness, steps) {{\n\
         \x20   hi = max(0, {p}_inside * thickness);\n\
         \x20   lo = min(0, {p}_inside * thickness);\n\
         \x20   m = steps + 1;\n\
         \x20   o = m * m;\n\
         \x20   points = concat([for (j = [0 : steps], i = [0 : steps]) {p}_xyz(k, i / steps, j / steps, hi)],\n\
         \x20                   [for (j = [0 : steps], i = [0 : steps]) {p}_xyz(k, i / steps, j / steps, lo)]);\n\
         \x20   faces = concat(\n\
         \x20       [for (j = [0 : steps - 1], i = [0 : steps - 1]) [{p}_at(m, i, j), {p}_at(m, i, j + 1), {p}_at(m, i + 1, j + 1), {p}_at(m, i + 1, j)]],\n\
         \x20       [for (j = [0 : steps - 1], i = [0 : steps - 1]) [o + {p}_at(m, i, j), o + {p}_at(m, i + 1, j), o + {p}_at(m, i + 1, j + 1), o + {p}_at(m, i, j + 1)]],\n\
         \x20       [for (i = [0 : steps - 1]) [{p}_at(m, i, 0), {p}_at(m, i + 1, 0), o + {p}_at(m, i + 1, 0), o + {p}_at(m, i, 0)]],\n\
         \x20       [for (i = [0 : steps - 1]) [{p}_at(m, i + 1, steps), {p}_at(m, i, steps), o + {p}_at(m, i, steps), o + {p}_at(m, i + 1, steps)]],\n\
         \x20       [for (j = [0 : steps - 1]) [{p}_at(m, 0, j + 1), {p}_at(m, 0, j), o + {p}_at(m, 0, j), o + {p}_at(m, 0, j + 1)]],\n\
         \x20       [for (j = [0 : steps - 1]) [{p}_at(m, steps, j), {p}_at(m, steps, j + 1), o + {p}_at(m, steps, j + 1), o + {p}_at(m, steps, j)]]);\n\
         \x20   polyhedron(points = points, faces = faces);\n\
         }}\n\
         module {p}_shell(thickness = {thick}, steps = 16) {{\n\
         \x20   for (k = [0 : len({p}_patches) - 1]) {p}_patch_shell(k, thickness, steps);\n\
         }}\n",
        count = fit.layout.rects.len(),
        plural = if fit.layout.rects.len() == 1 { "" } else { "es" },
        cont = if g1 { "tangent continuous (G1) across shared boundaries" } else { "C0 across shared boundaries" },
        kind = match frame { Frame::Plane { .. } => "plane", Frame::Cylinder { .. } => "cylinder" },
        thick = num(thickness),
    )
}

/// Compiles the SCAD, checks its heights against the fit and its shell.
fn validate(state: &AppState, p: &str, scad: &str, fit: &Fit) -> Result<Value, String> {
    let probes: Vec<(usize, f64, f64)> = (0..fit.layout.rects.len()).flat_map(|k| [(k, 0.0, 0.0), (k, 0.37, 0.61), (k, 1.0, 1.0), (k, 0.5, 0.0)]).take(24).collect();
    let expected = probes.iter().map(|&(k, s, t)| format!("{:?}", fit.height_in(k, s, t))).collect::<Vec<_>>().join(", ");
    let probe = probes.iter().map(|&(k, s, t)| format!("{p}_height({k}, {s}, {t})")).collect::<Vec<_>>().join(", ");
    let code = format!("{scad}\n{p}_expected = [{expected}];\n{p}_probe = [{probe}];\necho({p}_dev = max([for (q = [0 : 1 : len({p}_expected) - 1]) abs({p}_probe[q] - {p}_expected[q])]));\n{p}_shell(steps = 8);\n");
    let source = crate::agent::eval_source(state, None, &json!({"code": code})).map_err(|e| format!("Validation could not start: {}", e.message))?;
    let compiled = match crate::agent::compile_eval(&source) {
        Ok(compiled) => compiled,
        Err(error) => return Ok(json!({"ok": false, "error": format!("The generated SCAD did not compile: {}", error.message)})),
    };
    let tag = format!("{p}_dev = ");
    let deviation = compiled
        .messages
        .iter()
        .find_map(|m| m.find(&tag).map(|at| m[at + tag.len()..].trim().trim_matches('"').to_string()))
        .and_then(|text| text.parse::<f64>().ok());
    let volume = if compiled.parts.is_empty() { 0.0 } else { meshops::signed_volume(&crate::meshanalysis::from_engine_mesh(&compiled.mesh)) };
    let ok = deviation.is_some_and(|d| d <= 1e-6) && volume > 0.0;
    let mut value = json!({"ok": ok, "heightDeviation": deviation, "shellVolume": round_to(volume, 3)});
    if !ok {
        value["note"] = json!("The SCAD did not reproduce the fitted heights, or its shell is not a positive solid.");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scancommon::testkit::{call, state, upload_with};

    /// A 60 x 40 mm height field z = f(x, y), facing +Z, 1 mm cells; cells
    /// for which `skip` holds are left out (a hole in the scan).
    fn sheet(f: impl Fn(f64, f64) -> f64, skip: impl Fn(f64, f64) -> bool) -> TriMesh {
        let (nx, ny) = (60, 40);
        let mut mesh = TriMesh::default();
        for j in 0..=ny {
            for i in 0..=nx {
                let (x, y) = (i as f64, j as f64);
                mesh.points.push([x, y, f(x, y)]);
            }
        }
        let at = |i: u32, j: u32| j * (nx + 1) + i;
        for j in 0..ny {
            for i in 0..nx {
                if skip(i as f64 + 0.5, j as f64 + 0.5) {
                    continue;
                }
                mesh.triangles.push([at(i, j), at(i + 1, j), at(i + 1, j + 1)]);
                mesh.triangles.push([at(i, j), at(i + 1, j + 1), at(i, j + 1)]);
            }
        }
        mesh
    }

    /// An open cylinder of radius r, `sweep` degrees, along Z, outward normals.
    fn cylinder(r: f64, sweep: f64, height: f64) -> TriMesh {
        let (na, nz) = (((sweep / 4.0) as u32).max(8), 20);
        let full = sweep >= 360.0;
        let columns = if full { na } else { na + 1 };
        let mut mesh = TriMesh::default();
        for k in 0..=nz {
            for a in 0..columns {
                let angle = (sweep * a as f64 / na as f64).to_radians();
                mesh.points.push([r * angle.cos(), r * angle.sin(), height * k as f64 / nz as f64]);
            }
        }
        let at = |a: u32, k: u32| k * columns + a % columns;
        for k in 0..nz {
            for a in 0..na {
                mesh.triangles.push([at(a, k), at(a + 1, k), at(a + 1, k + 1)]);
                mesh.triangles.push([at(a, k), at(a + 1, k + 1), at(a, k + 1)]);
            }
        }
        mesh
    }

    #[test]
    fn the_basis_sums_to_one_and_interpolates_the_ends() {
        for n in [4, 5, 9] {
            let k = knots(n);
            for q in 0..=20 {
                let (_, b) = basis(n, &k, q as f64 / 20.0);
                assert!((b.iter().sum::<f64>() - 1.0).abs() < 1e-12, "{n} {q} {b:?}");
            }
            assert_eq!(basis(n, &k, 0.0), (0, [1.0, 0.0, 0.0, 0.0]));
            assert_eq!(basis(n, &k, 1.0).1[3], 1.0);
        }
    }

    #[test]
    fn patches_fit_a_wavy_sheet_with_shared_tangent_continuous_boundaries() {
        let (state, root) = state();
        let id = state.workspaces.create("%import(\"wave.stl\");\ntranslate([0, 0, -10]) cube(1);\n".into(), None).unwrap().id;
        let wave = |x: f64, y: f64| 1.5 * (x / 9.0).sin() * (y / 11.0).cos();
        upload_with(&state, &id, "wave.stl", &sheet(wave, |_, _| false), json!({}));
        let reply = call(&state, "fit_surface", json!({
            "designId": id, "surface": {"asset": "wave.stl"}, "patches": [2, 2], "tolerance": 0.05, "image": true, "detail": true
        }));
        assert_eq!(reply["isError"], json!(false), "{reply}");
        let s = &reply["structuredContent"];
        assert_eq!(s["withinTolerance"], json!(true), "{s}");
        assert!(s["residual"]["p95"].as_f64().unwrap() <= 0.05, "{s}");
        assert!(s["comparison"]["scanToFit"]["max"].as_f64().unwrap() < 0.1, "{s}");
        assert!(s["comparison"]["fitToScan"]["max"].as_f64().unwrap() < 0.1, "{s}");
        // Four boundaries between the four patches, closed exactly and tangent.
        assert_eq!(s["continuity"]["boundaries"], json!(4), "{s}");
        assert!(s["continuity"]["maxGap"].as_f64().unwrap() < 1e-9, "{s}");
        assert!(s["continuity"]["maxTangentAngleDeg"].as_f64().unwrap() < 0.05, "{s}");
        assert_eq!(s["scadCheck"]["ok"], json!(true), "{s}");
        assert!(s["unsupported"]["fraction"].as_f64().unwrap() < 0.05, "{s}");
        assert_eq!(s["controls"].as_array().unwrap().len(), 4);
        // The way an agent uses a fit: its SCAD pasted in as a revision,
        // compared with the scan as an object.
        let code = format!("%import(\"wave.stl\");\n{}\nfit_shell(thickness = 1);\n", s["scad"].as_str().unwrap());
        let revision = state.workspaces.get(&id).unwrap().unwrap().revision;
        let pasted = call(&state, "evaluate", json!({"designId": id, "code": code, "expectedRevision": revision, "revisionName": "Fitted surface", "image": false}));
        assert_eq!(pasted["isError"], json!(false), "{pasted}");
        let shell = pasted["structuredContent"]["objects"][0]["id"].clone();
        let compared = call(&state, "compare_surfaces", json!({"designId": id, "reference": {"asset": "wave.stl"}, "candidate": {"objectIds": [shell]}, "samples": 3000, "image": false}));
        assert_eq!(compared["isError"], json!(false), "{compared}");
        assert!(compared["structuredContent"]["shapeAccuracy"]["p95"].as_f64().unwrap() < 0.1, "{compared}");
        // C0 on a 2 x 2 grid: every boundary control is shared both ways, so
        // the patches meet everywhere, also along v next to a u boundary.
        let grid = call(&state, "fit_surface", json!({"designId": id, "surface": {"asset": "wave.stl"}, "patches": [2, 2], "continuity": "C0", "tolerance": 0.05, "image": false}));
        assert!(grid["structuredContent"]["continuity"]["maxGap"].as_f64().unwrap() < 1e-9, "{grid}");
        // C0 only: the boundaries still meet, their slopes are free.
        let c0 = call(&state, "fit_surface", json!({"designId": id, "surface": {"asset": "wave.stl"}, "patches": [3, 1], "continuity": "C0", "tolerance": 0.05, "image": false}));
        let c = &c0["structuredContent"];
        assert!(c["continuity"]["maxGap"].as_f64().unwrap() < 1e-9, "{c}");
        assert!(c["unknowns"].as_u64().unwrap() > s["unknowns"].as_u64().unwrap() / 4, "{c}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_fit_stores_nothing_and_its_reply_names_no_stored_mesh() {
        let (state, root) = state();
        let id = state.workspaces.create("%import(\"wave.stl\");\ncube(1);\n".into(), None).unwrap().id;
        upload_with(&state, &id, "wave.stl", &sheet(|x, y| 0.02 * x + 0.01 * y, |_, _| false), json!({}));
        let before = state.workspaces.asset_snapshot(&id).unwrap();
        // A name from before this changed is ignored, and stores nothing either.
        let reply = call(&state, "fit_surface", json!({"designId": id, "surface": {"asset": "wave.stl"}, "tolerance": 0.05, "image": false, "name": "kept.stl"}));
        assert_eq!(reply["isError"], json!(false), "{reply}");
        assert_eq!(state.workspaces.asset_snapshot(&id).unwrap(), before, "the asset list and its version do not move");
        let s = &reply["structuredContent"];
        assert_eq!(s["scadCheck"]["ok"], json!(true), "{s}");
        assert!(s["scad"].as_str().unwrap().contains("module fit_shell"), "{s}");
        for gone in ["surface", "sha256", "expiresAt", "expiresAtIso", "expiresInSeconds", "expiresIn", "expiry"] {
            assert!(s.get(gone).is_none(), "{gone}: {s}");
        }
        let text = reply.to_string();
        assert!(!text.contains(".surface.stl") && !text.contains("kept.stl"), "{text}");
        assert_eq!(s["source"]["source"]["asset"], json!("wave.stl"), "the scan it fitted is still named: {s}");
        // The tool's own text says the same.
        let tools = tool_definitions();
        let definition = serde_json::to_string(&tools[0]).unwrap();
        assert!(!definition.contains(".surface.stl") && !definition.contains("stored fit") && definition.contains("Nothing is stored"), "{definition}");
        assert!(tools[0]["inputSchema"]["properties"].get("name").is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cylinders_fit_round_and_holes_are_reported_unsupported() {
        let (state, root) = state();
        let id = state.workspaces.create("%import(\"half.stl\");\n%import(\"tube.stl\");\n%import(\"holed.stl\");\ncube(1);\n".into(), None).unwrap().id;
        upload_with(&state, &id, "half.stl", &cylinder(10.0, 180.0, 30.0), json!({}));
        upload_with(&state, &id, "tube.stl", &cylinder(10.0, 360.0, 30.0), json!({}));
        upload_with(&state, &id, "holed.stl", &sheet(|x, _| 0.02 * x, |x, y| (x - 30.0).hypot(y - 20.0) < 8.0), json!({}));
        // A half cylinder is no height field over its plane...
        let flat = call(&state, "fit_surface", json!({"designId": id, "surface": {"asset": "half.stl"}, "image": false}));
        assert_eq!(flat["isError"], json!(true), "{flat}");
        assert!(flat["content"][0]["text"].as_str().unwrap().contains("sideways"), "{flat}");
        // ...but is one over its cylinder.
        let half = call(&state, "fit_surface", json!({"designId": id, "surface": {"asset": "half.stl"}, "projection": "cylinder", "tolerance": 0.05, "image": false}));
        let h = &half["structuredContent"];
        assert_eq!(half["isError"], json!(false), "{half}");
        assert!((h["frame"]["radius"].as_f64().unwrap() - 10.0).abs() < 0.05, "{h}");
        assert_eq!(h["frame"]["periodic"], json!(false), "{h}");
        assert!(h["comparison"]["scanToFit"]["p95"].as_f64().unwrap() < 0.05, "{h}");
        assert_eq!(h["scadCheck"]["ok"], json!(true), "{h}");
        // A full tube closes up across its seam, tangent continuous.
        let tube = call(&state, "fit_surface", json!({"designId": id, "surface": {"asset": "tube.stl"}, "projection": "cylinder", "patches": [2, 1], "tolerance": 0.05, "image": false}));
        let t = &tube["structuredContent"];
        assert_eq!(t["frame"]["periodic"], json!(true), "{t}");
        assert_eq!(t["continuity"]["boundaries"], json!(2), "{t}");
        assert!(t["continuity"]["maxGap"].as_f64().unwrap() < 1e-9, "{t}");
        assert!(t["continuity"]["maxTangentAngleDeg"].as_f64().unwrap() < 0.1, "{t}");
        // A hole in the data is fitted by smoothness alone, and said so.
        let holed = call(&state, "fit_surface", json!({"designId": id, "surface": {"asset": "holed.stl"}, "image": false}));
        let u = &holed["structuredContent"]["unsupported"];
        assert!(u["fraction"].as_f64().unwrap() > 0.05, "{u}");
        let center = &u["clusters"][0]["center"];
        assert!((center[0].as_f64().unwrap() - 30.0).abs() < 3.0 && (center[1].as_f64().unwrap() - 20.0).abs() < 3.0, "{u}");
        let _ = std::fs::remove_dir_all(root);
    }
}
