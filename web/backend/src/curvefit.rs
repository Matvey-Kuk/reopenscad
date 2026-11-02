//! The `fit_curve` MCP tool: a uniform B-spline fitted to a section contour
//! (of a scan or a model) or to given 2D points, under a
//! control-point budget and a tolerance, with outlier rejection, optional
//! constant-normal-thickness band, and an independent comparison of the
//! result against the data (and the cut surface) that also says where the
//! curve has no data behind it. The fitted curve comes back as editable
//! OpenSCAD source (control points plus pure-SCAD evaluation), compiled
//! once to prove it evaluates to the same curve.

use crate::meshanalysis::{self, MeshIndex, Work};
use crate::plot2d::Canvas;
use crate::scancommon::{self, Design, Stats};
use crate::AppState;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

type V2 = [f64; 2];

/// Wall-clock budget of one call.
const BUDGET: Duration = Duration::from_secs(60);
/// Unsupported spans numbered in the picture, longest first.
const MAX_SPAN_LABELS: usize = 6;
/// Most samples the fit itself uses (chains are decimated to this).
const MAX_FIT_SAMPLES: usize = 4000;
/// Most samples kept for the independent comparison.
const MAX_COMPARE_SAMPLES: usize = 20000;
/// Dense curve points used for comparisons and drawing.
const DENSE_CURVE_POINTS: usize = 2000;
/// Polyline points per span used to re-parameterise samples.
const REPARAM_PER_SPAN: usize = 48;
const MAX_CONTROL_POINTS: usize = 128;
/// Most section chains one fit joins.
const MAX_CHAINS: usize = 256;
/// Chains listed in the reply.
const LISTED_CHAINS: usize = 50;
/// Largest coordinate accepted, mm.
const MAX_COORDINATE: f64 = 1e7;
const PREFIX_RULE: &str = "prefix must match ^[A-Za-z_][A-Za-z0-9_]{0,31}$.";

pub(crate) fn tool_definitions() -> Vec<Value> {
    let vec3 = json!({"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3});
    let plane = json!({"type": "object", "properties": {
        "axis": {"type": "string", "enum": ["x", "y", "z"]},
        "offset": {"type": "number"},
        "point": vec3.clone(),
        "normal": vec3
    }});
    vec![crate::mcp_tool(
        "fit_curve",
        "Fit an editable uniform B-spline to a contour within a control-point budget and tolerance (mm), \
         and check it independently. Data: section {surface (an open scan is fine), plane, chain, \
         sectionTolerance}, the cut in section_profile's plane basis (default the longest chain; several \
         are joined end to nearest end and the jumps stay gaps; \"all\" keeps the 256 longest), or points \
         [[x, y], ...] (optional plane places them in 3D). within {min, max} keeps a section's contour to \
         one box, cutting chains at its walls. closed default: the chain's own flag, false for points. \
         Control points grow until every accepted sample is within tolerance (withinTolerance false when \
         the budget ran out). rejectOutliers: isolated spikes farther than that are rejected and the fit \
         repeats. fixEnds pins open curve ends to the first/last sample. thickness: a \
         constant-normal-offset band, offsetConflicts where it would fold. Reply: fit (controlPoints in \
         plane mm), unsupportedSpans (curve with no data: gaps it bridges or invents), comparison \
         (independent of the fit; signed + = right of the curve direction, outside a counter-clockwise \
         loop), and validated scad (edit <prefix>_ctrl to reshape).",
        json!({
            "type": "object",
            "properties": {
                "within": scancommon::within_schema(scancommon::SURFACE_FRAME),
                "designId": {"type": "string"},
                "section": {"type": "object", "required": ["surface", "plane"], "properties": {
                    "surface": {"description": "As in compare_surfaces."},
                    "plane": plane.clone(),
                    "chain": {"description": "Chain index, list of indices, or \"all\"; default the longest.", "anyOf": [{"type": "integer", "minimum": 0}, {"type": "array", "items": {"type": "integer", "minimum": 0}}, {"type": "string", "enum": ["all"]}]},
                    "sectionTolerance": {"type": "number", "minimum": 0, "maximum": 5, "default": 0.02}
                }},
                "points": {"type": "array", "minItems": 3, "maxItems": MAX_COMPARE_SAMPLES, "items": {"type": "array", "items": {"type": "number"}, "minItems": 2, "maxItems": 2}},
                "plane": plane,
                "degree": {"type": "integer", "enum": [2, 3], "default": 3},
                "closed": {"type": "boolean"},
                "maxControlPoints": {"type": "integer", "minimum": 3, "maximum": MAX_CONTROL_POINTS, "default": 16},
                "tolerance": {"type": "number", "exclusiveMinimum": 0, "maximum": 50, "default": 0.1},
                "rejectOutliers": {"type": "number", "minimum": 0, "description": "mm; default 4 x tolerance; 0 disables."},
                "fixEnds": {"type": "boolean", "default": true},
                "thickness": {"type": "object", "required": ["value"], "properties": {
                    "value": {"type": "number", "exclusiveMinimum": 0, "maximum": 100},
                    "side": {"type": "string", "enum": ["left", "right", "both"], "default": "left"}
                }},
                "prefix": {"type": "string", "pattern": "^[A-Za-z_][A-Za-z0-9_]{0,31}$", "default": "fit"},
                "image": {"description": "true (default), false, or {width, height}.", "anyOf": [{"type": "boolean"}, {"type": "object"}]},
                "detail": scancommon::detail_schema()
            }
        }),
    )]
}

fn ok_result(structured: Value) -> Value {
    json!({
        "content": [{"type": "text", "text": serde_json::to_string(&structured).unwrap_or_default()}],
        "structuredContent": structured,
        "isError": false
    })
}

pub(crate) fn mcp_dispatch(state: &AppState, name: &str, arguments: &Value) -> Value {
    if name != "fit_curve" {
        return crate::mcp_tool_error(&format!("Unknown tool {name:?}."));
    }
    match fit_curve_value(state, arguments) {
        Ok((value, Some((caption, png)))) => scancommon::image_result(scancommon::compact(value, arguments), vec![(caption, png)]),
        Ok((value, None)) => ok_result(scancommon::compact(value, arguments)),
        Err(error) => crate::mcp_tool_error(&error),
    }
}

// ---------------------------------------------------------------------------
// 2D helpers
// ---------------------------------------------------------------------------

fn sub2(a: V2, b: V2) -> V2 {
    [a[0] - b[0], a[1] - b[1]]
}
fn len2(a: V2) -> f64 {
    (a[0] * a[0] + a[1] * a[1]).sqrt()
}
fn dist2(a: V2, b: V2) -> f64 {
    len2(sub2(a, b))
}
fn r4(p: V2) -> V2 {
    p.map(|v| scancommon::round_to(v, 4))
}
fn r2pt(p: V2) -> V2 {
    p.map(|v| scancommon::round_to(v, 3))
}

/// Closest point of segment ab to p: (distance, fraction along ab, signed
/// side: + when p is right of a->b).
fn segment_nearest(p: V2, a: V2, b: V2) -> (f64, f64, f64) {
    let d = sub2(b, a);
    let l2 = d[0] * d[0] + d[1] * d[1];
    let f = if l2 > 0.0 { (((p[0] - a[0]) * d[0] + (p[1] - a[1]) * d[1]) / l2).clamp(0.0, 1.0) } else { 0.0 };
    let q = [a[0] + d[0] * f, a[1] + d[1] * f];
    let side = d[0] * (p[1] - a[1]) - d[1] * (p[0] - a[0]);
    (dist2(p, q), f, if side > 0.0 { -1.0 } else { 1.0 })
}

/// Signed distance of p to the polyline (closed joins last to first);
/// returns (signed distance, foot point).
fn polyline_distance(p: V2, line: &[V2], closed: bool) -> (f64, V2) {
    let segments = if closed { line.len() } else { line.len().saturating_sub(1) };
    let mut best = (f64::INFINITY, [0.0; 2], 1.0);
    for i in 0..segments {
        let (a, b) = (line[i], line[(i + 1) % line.len()]);
        let (d, f, side) = segment_nearest(p, a, b);
        if d < best.0 {
            best = (d, [a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f], side);
        }
    }
    if segments == 0 {
        if let Some(only) = line.first() {
            return (dist2(p, *only), *only);
        }
    }
    (best.0 * best.2, best.1)
}

/// Signed area (counter-clockwise positive).
fn shoelace(points: &[V2]) -> f64 {
    let n = points.len();
    (0..n).map(|i| {
        let (a, b) = (points[i], points[(i + 1) % n]);
        a[0] * b[1] - b[0] * a[1]
    }).sum::<f64>() * 0.5
}

// ---------------------------------------------------------------------------
// Uniform B-spline, evaluated exactly as the emitted SCAD does
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Spline {
    deg: usize,
    closed: bool,
    ctrl: Vec<V2>,
}

impl Spline {
    /// Control points with the periodic wrap appended (closed).
    fn cp(&self) -> Vec<V2> {
        let mut cp = self.ctrl.clone();
        if self.closed {
            for i in 0..self.deg {
                cp.push(self.ctrl[i]);
            }
        }
        cp
    }

    fn knots(&self) -> Vec<f64> {
        let (n, p) = (self.ctrl.len(), self.deg);
        if self.closed {
            (0..=(n + p + p)).map(|i| i as f64).collect()
        } else {
            let mut k = vec![0.0; p + 1];
            k.extend((1..n - p).map(|i| i as f64));
            k.extend(std::iter::repeat_n((n - p) as f64, p + 1));
            k
        }
    }

    fn domain(&self) -> (f64, f64) {
        let (n, p) = (self.ctrl.len() as f64, self.deg as f64);
        if self.closed { (p, n + p) } else { (0.0, n - p) }
    }

    fn spans(&self) -> usize {
        if self.closed { self.ctrl.len() } else { self.ctrl.len() - self.deg }
    }
}

/// Largest i in [p, ncp - 1] with knots[i] <= t (SCAD: <prefix>_span).
fn span_of(knots: &[f64], t: f64, p: usize, ncp: usize) -> usize {
    let mut s = p;
    for i in p..ncp {
        if knots[i] <= t {
            s = i;
        }
    }
    s
}

/// De Boor, mirroring the SCAD recursion step for step.
fn eval_at(cp: &[V2], knots: &[f64], p: usize, t: f64) -> V2 {
    let s = span_of(knots, t, p, cp.len());
    let mut d: Vec<V2> = (0..=p).map(|j| cp[j + s - p]).collect();
    for r in 1..=p {
        d = (0..=p)
            .map(|j| {
                if j < r {
                    d[j]
                } else {
                    let i = j + s - p;
                    let a = (t - knots[i]) / (knots[i + p + 1 - r] - knots[i]);
                    [(1.0 - a) * d[j - 1][0] + a * d[j][0], (1.0 - a) * d[j - 1][1] + a * d[j][1]]
                }
            })
            .collect();
    }
    d[p]
}

/// The non-zero basis functions N_{s-p..s}(t) of span s.
fn basis(knots: &[f64], s: usize, t: f64, p: usize) -> Vec<f64> {
    let mut n = vec![0.0; p + 1];
    let mut left = vec![0.0; p + 1];
    let mut right = vec![0.0; p + 1];
    n[0] = 1.0;
    for j in 1..=p {
        left[j] = t - knots[s + 1 - j];
        right[j] = knots[s + j] - t;
        let mut saved = 0.0;
        for r in 0..j {
            let denominator = right[r + 1] + left[j - r];
            let temp = if denominator != 0.0 { n[r] / denominator } else { 0.0 };
            n[r] = saved + right[r + 1] * temp;
            saved = left[j - r] * temp;
        }
        n[j] = saved;
    }
    n
}

impl Spline {
    /// `n` points as `<prefix>_points(n)` computes them.
    fn points(&self, n: usize) -> Vec<V2> {
        let (cp, knots) = (self.cp(), self.knots());
        let (t0, t1) = self.domain();
        let den = if self.closed { n } else { n - 1 } as f64;
        (0..n).map(|q| eval_at(&cp, &knots, self.deg, t0 + (t1 - t0) * q as f64 / den)).collect()
    }

    /// `<prefix>_offset(n, d)`: points moved d along the left normal
    /// (numerical tangent from the neighbours).
    fn offset(&self, n: usize, d: f64) -> Vec<V2> {
        offset_points(&self.points(n), self.closed, d)
    }
}

fn offset_points(c: &[V2], closed: bool, d: f64) -> Vec<V2> {
    let m = c.len();
    (0..m)
        .map(|q| {
            let a = if closed { c[(q + m - 1) % m] } else { c[q.saturating_sub(1)] };
            let b = if closed { c[(q + 1) % m] } else { c[(q + 1).min(m - 1)] };
            let diff = [b[0] - a[0], b[1] - a[1]];
            let length = (diff[0] * diff[0] + diff[1] * diff[1]).sqrt();
            let t = [diff[0] / length, diff[1] / length];
            [c[q][0] + d * -t[1], c[q][1] + d * t[0]]
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Least squares
// ---------------------------------------------------------------------------

/// Gaussian elimination with partial pivoting, two right-hand sides.
fn solve(mut a: Vec<Vec<f64>>, mut b: Vec<V2>) -> Option<Vec<V2>> {
    let n = a.len();
    for col in 0..n {
        let pivot = (col..n).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
        if !(a[pivot][col].abs() > 1e-14) {
            return None;
        }
        a.swap(col, pivot);
        b.swap(col, pivot);
        for row in col + 1..n {
            let f = a[row][col] / a[col][col];
            if f == 0.0 {
                continue;
            }
            for k in col..n {
                a[row][k] -= f * a[col][k];
            }
            b[row] = [b[row][0] - f * b[col][0], b[row][1] - f * b[col][1]];
        }
    }
    let mut x = vec![[0.0; 2]; n];
    for row in (0..n).rev() {
        let mut acc = b[row];
        for k in row + 1..n {
            acc = [acc[0] - a[row][k] * x[k][0], acc[1] - a[row][k] * x[k][1]];
        }
        x[row] = [acc[0] / a[row][row], acc[1] / a[row][row]];
    }
    x.iter().all(|p| p[0].is_finite() && p[1].is_finite()).then_some(x)
}

/// Control points minimising squared distance of `samples` at parameters
/// `s` (normalised 0..1) plus a light second-difference penalty.
fn least_squares(
    samples: &[V2],
    s: &[f64],
    accepted: &[bool],
    n: usize,
    deg: usize,
    closed: bool,
    ends: Option<(V2, V2)>,
) -> Option<Spline> {
    let template = Spline { deg, closed, ctrl: vec![[0.0; 2]; n] };
    let knots = template.knots();
    let ncp = template.cp().len();
    let (t0, t1) = template.domain();
    let mut g = vec![vec![0.0; n]; n];
    let mut rhs = vec![[0.0; 2]; n];
    let mut used = 0usize;
    for (k, sample) in samples.iter().enumerate() {
        if !accepted[k] {
            continue;
        }
        used += 1;
        let t = (t0 + (t1 - t0) * s[k]).clamp(t0, t1);
        let span = span_of(&knots, t, deg, ncp);
        let values = basis(&knots, span, t, deg);
        let index = |j: usize| if closed { (span - deg + j) % n } else { span - deg + j };
        for (a, va) in values.iter().enumerate() {
            let ia = index(a);
            rhs[ia] = [rhs[ia][0] + va * sample[0], rhs[ia][1] + va * sample[1]];
            for (b, vb) in values.iter().enumerate() {
                g[ia][index(b)] += va * vb;
            }
        }
    }
    if used == 0 {
        return None;
    }
    let lambda = 1e-4 * used as f64 / n as f64;
    let stencils: Vec<[usize; 3]> = if closed {
        (0..n).map(|i| [(i + n - 1) % n, i, (i + 1) % n]).collect()
    } else {
        (1..n.saturating_sub(1)).map(|i| [i - 1, i, i + 1]).collect()
    };
    for stencil in stencils {
        let w = [1.0, -2.0, 1.0];
        for a in 0..3 {
            for b in 0..3 {
                g[stencil[a]][stencil[b]] += lambda * w[a] * w[b];
            }
        }
    }
    let fixed: Vec<Option<V2>> = (0..n)
        .map(|i| match ends {
            Some((first, _)) if i == 0 => Some(first),
            Some((_, last)) if i == n - 1 => Some(last),
            _ => None,
        })
        .collect();
    let free: Vec<usize> = (0..n).filter(|i| fixed[*i].is_none()).collect();
    let mut ctrl: Vec<V2> = fixed.iter().map(|f| f.unwrap_or([0.0; 2])).collect();
    if !free.is_empty() {
        let a: Vec<Vec<f64>> = free.iter().map(|&i| free.iter().map(|&j| g[i][j]).collect()).collect();
        let b: Vec<V2> = free
            .iter()
            .map(|&i| {
                let mut v = rhs[i];
                for (j, value) in fixed.iter().enumerate() {
                    if let Some(value) = value {
                        v = [v[0] - g[i][j] * value[0], v[1] - g[i][j] * value[1]];
                    }
                }
                v
            })
            .collect();
        let x = solve(a, b)?;
        for (slot, &i) in free.iter().enumerate() {
            ctrl[i] = x[slot];
        }
    }
    Some(Spline { deg, closed, ctrl })
}

/// Projects every sample onto the curve near its current parameter:
/// returns new normalised parameters and the distances.
fn reparameterise(spline: &Spline, samples: &[V2], s: &[f64]) -> (Vec<f64>, Vec<f64>) {
    let segments = spline.spans() * REPARAM_PER_SPAN;
    let (cp, knots) = (spline.cp(), spline.knots());
    let (t0, t1) = spline.domain();
    let count = if spline.closed { segments } else { segments + 1 };
    let line: Vec<V2> = (0..count)
        .map(|i| eval_at(&cp, &knots, spline.deg, t0 + (t1 - t0) * i as f64 / segments as f64))
        .collect();
    let window = (2 * REPARAM_PER_SPAN) as isize;
    let mut out_s = Vec::with_capacity(samples.len());
    let mut out_d = Vec::with_capacity(samples.len());
    for (k, p) in samples.iter().enumerate() {
        let centre = (s[k] * segments as f64).floor() as isize;
        let mut best = (f64::INFINITY, s[k]);
        for offset in -window..=window {
            let i = centre + offset;
            let i = if spline.closed {
                i.rem_euclid(segments as isize) as usize
            } else if i < 0 || i >= segments as isize {
                continue;
            } else {
                i as usize
            };
            let (a, b) = (line[i], line[(i + 1) % count]);
            let (d, f, _) = segment_nearest(*p, a, b);
            if d < best.0 {
                let u = (i as f64 + f) / segments as f64;
                best = (d, if spline.closed { u.rem_euclid(1.0) } else { u.clamp(0.0, 1.0) });
            }
        }
        out_s.push(best.1);
        out_d.push(best.0);
    }
    (out_s, out_d)
}

struct Attempt {
    spline: Spline,
    s: Vec<f64>,
    errors: Vec<f64>,
    accepted: Vec<bool>,
    max_error: f64,
    rejection_capped: bool,
}

/// One fit at `n` control points: solve / re-parameterise rounds, with
/// rejection of isolated outlier runs.
#[allow(clippy::too_many_arguments)]
fn fit_with(
    samples: &[V2],
    start_s: &[f64],
    n: usize,
    deg: usize,
    closed: bool,
    ends: Option<(V2, V2)>,
    reject: f64,
    deadline: Instant,
) -> Result<Option<Attempt>, String> {
    let m = samples.len();
    let mut accepted = vec![true; m];
    let mut gaps: Vec<f64> = samples.windows(2).map(|w| dist2(w[0], w[1])).collect();
    gaps.sort_by(f64::total_cmp);
    let spacing = gaps.get(gaps.len() / 2).copied().unwrap_or(0.0);
    let mut s = start_s.to_vec();
    let mut rejection_capped = false;
    let mut last: Option<(Spline, Vec<f64>)> = None;
    for round in 0..4 {
        let mut spline = None;
        for _ in 0..3 {
            crate::meshops::check(Some(deadline))?;
            let Some(fitted) = least_squares(samples, &s, &accepted, n, deg, closed, ends) else { return Ok(None) };
            let (new_s, errors) = reparameterise(&fitted, samples, &s);
            s = new_s;
            spline = Some((fitted, errors));
        }
        let (fitted, errors) = spline.expect("three rounds ran");
        last = Some((fitted, errors.clone()));
        if round == 3 || !(reject > 0.0) {
            break;
        }
        // Isolated runs of far samples: outliers, not an under-fit.
        let longest_run = 3.max(m / 100);
        let far: Vec<bool> = (0..m).map(|k| accepted[k] && errors[k] > reject).collect();
        let mut new_rejects = Vec::new();
        let mut k = 0;
        while k < m {
            if !far[k] {
                k += 1;
                continue;
            }
            let start = k;
            while k < m && far[k] {
                k += 1;
            }
            let wraps = closed && (start == 0 || k == m) && far[0] && far[m - 1];
            // A spike: the samples either side lie inside the limit and far
            // closer than the run's peak. An under-fit rises smoothly into
            // the limit and is no outlier.
            let peak = (start..k).map(|i| errors[i]).fold(0.0, f64::max);
            let calm = |i: Option<usize>| i.is_none_or(|i| errors[i] < reject && errors[i] < 0.35 * peak);
            let before = if start > 0 { Some(start - 1) } else if closed { Some(m - 1) } else { None };
            let after = if k < m { Some(k) } else if closed { Some(0) } else { None };
            // Short in length too: a steep real feature (a rib) spans more
            // than a few sample spacings between its calm neighbours.
            let extent = match (before, after) {
                (Some(b), Some(a)) => dist2(samples[b], samples[a]),
                _ => dist2(samples[start], samples[k - 1]) + 2.0 * spacing,
            };
            let short = extent <= 4.0 * spacing.max(1e-9);
            if k - start <= longest_run && short && !wraps && calm(before) && calm(after) {
                new_rejects.extend(start..k);
            }
        }
        if new_rejects.is_empty() {
            break;
        }
        let already = accepted.iter().filter(|a| !**a).count();
        if (already + new_rejects.len()) as f64 > 0.2 * m as f64 {
            rejection_capped = true;
            break;
        }
        for k in new_rejects {
            accepted[k] = false;
        }
    }
    let (spline, errors) = last.expect("one round ran");
    let max_error = errors.iter().zip(&accepted).filter(|(_, a)| **a).map(|(e, _)| *e).fold(0.0, f64::max);
    Ok(Some(Attempt { spline, s, errors, accepted, max_error, rejection_capped }))
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

fn plane_input(plane: &Value) -> Result<([f64; 3], [f64; 3]), String> {
    if !plane.is_object() {
        return Err("plane must be {axis, offset} or {point, normal}.".into());
    }
    if let Some(axis) = plane.get("axis").filter(|axis| !axis.is_null()) {
        let index = match axis.as_str() {
            Some("x" | "X") => 0,
            Some("y" | "Y") => 1,
            Some("z" | "Z") => 2,
            _ => return Err("plane.axis must be \"x\", \"y\" or \"z\".".into()),
        };
        let offset = scancommon::number(plane, "offset", 0.0, -1e7, 1e7).map_err(|e| format!("plane.{e}"))?;
        let mut point = [0.0; 3];
        let mut normal = [0.0; 3];
        point[index] = offset;
        normal[index] = 1.0;
        return Ok((point, normal));
    }
    let point = scancommon::vec3(plane.get("point").unwrap_or(&Value::Null), "plane.point")?;
    let normal = scancommon::unit(scancommon::vec3(plane.get("normal").unwrap_or(&Value::Null), "plane.normal")?);
    if scancommon::norm(normal) < 0.5 {
        return Err("plane.normal must be non-zero.".into());
    }
    Ok((point, normal))
}

/// Plane basis (origin, u, v, normal), the same as section_profile's.
type Basis = ([f64; 3], [f64; 3], [f64; 3], [f64; 3]);

fn basis_of(point: [f64; 3], normal: [f64; 3]) -> Result<Basis, String> {
    let empty = crate::assets::TriMesh::default();
    let section = meshanalysis::section_within(&empty, point, normal, 0.0, &mut Work::new(1000))?;
    Ok((section.origin, section.u, section.v, scancommon::unit(normal)))
}

fn lift(basis: &Basis, p: V2) -> [f64; 3] {
    let (o, u, v, _) = basis;
    [o[0] + u[0] * p[0] + v[0] * p[1], o[1] + u[1] * p[0] + v[1] * p[1], o[2] + u[2] * p[0] + v[2] * p[1]]
}

fn place_matrix(basis: &Basis) -> Value {
    let (o, u, v, n) = basis;
    let mut m = json!([
        [u[0], v[0], n[0], o[0]],
        [u[1], v[1], n[1], o[1]],
        [u[2], v[2], n[2], o[2]],
        [0.0, 0.0, 0.0, 1.0],
    ]);
    if let Some(rows) = m.as_array_mut() {
        for row in rows {
            if let Some(cells) = row.as_array_mut() {
                for cell in cells {
                    *cell = json!(scancommon::round_to(cell.as_f64().unwrap_or(0.0), 6));
                }
            }
        }
    }
    m
}

/// The data: comparison samples in order (`breaks[i]`: a jump, not data,
/// lies between sample i-1 and i), the decimated fit samples, and whether
/// the data itself closes.
struct Data {
    samples: Vec<V2>,
    breaks: Vec<bool>,
    fit: Vec<V2>,
    /// Index in `samples` of each fit sample.
    fit_index: Vec<usize>,
    closed: bool,
    /// Section data: consecutive samples on one chain are joined by real
    /// surface, so distances to the data are measured to its segments.
    continuous: bool,
}

/// Evenly spaced points along a polyline (closed adds the closing segment),
/// keeping its vertices' shape, about `spacing` apart.
fn densify(points: &[V2], closed: bool, spacing: f64) -> Vec<V2> {
    let mut out = Vec::new();
    let n = points.len();
    let segments = if closed { n } else { n.saturating_sub(1) };
    for i in 0..segments {
        let (a, b) = (points[i], points[(i + 1) % n]);
        let steps = ((dist2(a, b) / spacing).ceil() as usize).max(1);
        for k in 0..steps {
            let f = k as f64 / steps as f64;
            out.push([a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f]);
        }
    }
    if !closed {
        if let Some(last) = points.last() {
            out.push(*last);
        }
    }
    if out.is_empty() {
        out.extend_from_slice(points);
    }
    out
}

/// Indices of at most `limit` evenly spread points, keeping the last.
fn decimate(count: usize, limit: usize) -> Vec<usize> {
    if count <= limit {
        return (0..count).collect();
    }
    let step = count as f64 / limit as f64;
    let mut out: Vec<usize> = (0..limit).map(|i| ((i as f64 * step) as usize).min(count - 1)).collect();
    if let Some(slot) = out.last_mut() {
        *slot = count - 1;
    }
    out
}

/// Orders chosen chains end to nearest end (reversing as needed), growing
/// the joined path at whichever of its two free ends is nearer.
fn join_chains(mut chains: Vec<(Vec<V2>, bool)>, deadline: Instant) -> Result<Vec<Vec<V2>>, String> {
    chains.sort_by(|a, b| polyline_length(&b.0).total_cmp(&polyline_length(&a.0)));
    let mut ordered = std::collections::VecDeque::new();
    ordered.push_back(chains.remove(0).0);
    while !chains.is_empty() {
        crate::meshops::check(Some(deadline))?;
        let head = ordered.front().and_then(|c: &Vec<V2>| c.first().copied()).expect("non-empty chain");
        let tail = ordered.back().and_then(|c: &Vec<V2>| c.last().copied()).expect("non-empty chain");
        // (distance, chain, reverse it, attach at the head)
        let mut best = (f64::INFINITY, 0, false, false);
        for (i, (chain, _)) in chains.iter().enumerate() {
            let (first, last) = (chain[0], *chain.last().expect("non-empty"));
            for (d, reverse, at_head) in [
                (dist2(tail, first), false, false),
                (dist2(tail, last), true, false),
                (dist2(head, last), false, true),
                (dist2(head, first), true, true),
            ] {
                if d < best.0 {
                    best = (d, i, reverse, at_head);
                }
            }
        }
        let (_, index, reverse, at_head) = best;
        let (mut chain, _) = chains.remove(index);
        if reverse {
            chain.reverse();
        }
        if at_head {
            ordered.push_front(chain);
        } else {
            ordered.push_back(chain);
        }
    }
    Ok(ordered.into_iter().collect())
}

fn polyline_length(points: &[V2]) -> f64 {
    points.windows(2).map(|w| dist2(w[0], w[1])).sum()
}

// ---------------------------------------------------------------------------
// The tool
// ---------------------------------------------------------------------------

struct Thickness {
    value: f64,
    side: &'static str,
    offsets: [f64; 2],
}

pub(crate) fn fit_curve_value(state: &AppState, input: &Value) -> Result<(Value, Option<(String, Vec<u8>)>), String> {
    let started = Instant::now();
    let deadline = started + BUDGET;
    let _scope = crate::csg::bsp::deadline_scope(deadline);

    // Arguments first, so a malformed call fails before any evaluation.
    let deg = scancommon::integer(input, "degree", 3, 2, 3)?;
    let tolerance = scancommon::number(input, "tolerance", 0.1, 1e-6, 50.0)?;
    let budget = scancommon::integer(input, "maxControlPoints", 16, 3, MAX_CONTROL_POINTS)?;
    let reject = scancommon::number(input, "rejectOutliers", 4.0 * tolerance, 0.0, 1e6)?;
    let fix_ends = scancommon::flag(input, "fixEnds", true)?;
    let prefix = input.get("prefix").and_then(Value::as_str).unwrap_or("fit").to_string();
    let valid_prefix = prefix.len() <= 32
        && prefix.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid_prefix || input.get("prefix").is_some_and(|p| !p.is_string() && !p.is_null()) {
        return Err(PREFIX_RULE.into());
    }
    let thickness = match input.get("thickness") {
        None | Some(Value::Null) => None,
        Some(spec) => {
            let value = spec
                .get("value")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite() && *v > 0.0 && *v <= 100.0)
                .ok_or("thickness.value must be a number of mm above 0 (at most 100).")?;
            // Rounded as the SCAD writes it, so both evaluate the same band.
            let value = scancommon::round_to(value, 4).max(1e-4);
            let side = match spec.get("side").and_then(Value::as_str).unwrap_or("left") {
                "left" => "left",
                "right" => "right",
                "both" => "both",
                _ => return Err("thickness.side must be \"left\", \"right\" or \"both\".".into()),
            };
            let offsets = match side {
                "left" => [0.0, value],
                "right" => [0.0, -value],
                _ => [scancommon::round_to(-value / 2.0, 4), scancommon::round_to(value / 2.0, 4)],
            };
            Some(Thickness { value, side, offsets })
        }
    };
    let image = scancommon::image_wanted(input, true)?;
    let closed_input = match input.get("closed") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(value)) => Some(*value),
        Some(_) => return Err("closed must be true or false.".into()),
    };

    let mut design = Design::new(state, input);
    let mut source = json!({});
    let mut inside_box = None;
    let mut surface_index: Option<MeshIndex> = None;
    let (data, basis) = match (input.get("section").filter(|v| !v.is_null()), input.get("points").filter(|v| !v.is_null())) {
        (Some(_), Some(_)) => return Err("Give section or points, not both.".into()),
        (None, Some(_)) if input.get("within").is_some_and(|v| !v.is_null()) => return Err("within keeps a section to one box; given points, send only the points you want fitted.".into()),
        (None, None) => return Err("Give the data: section {surface, plane, chain} (a cut of a scan or model) or points [[x, y], ...].".into()),
        (Some(section), None) => {
            let plane = section.get("plane").ok_or("section.plane is required: {axis, offset} or {point, normal}.")?;
            let (point, normal) = plane_input(plane)?;
            let section_tolerance = scancommon::number(section, "sectionTolerance", 0.02, 0.0, 5.0)?;
            let spec = section.get("surface").ok_or("section.surface is required: an object ID, ID list, {objectIds} or {asset, matrix | frameOf}.")?;
            let surface = design.surface(spec, "section.surface")?;
            let mut cut = meshanalysis::section_within(&surface.mesh, point, normal, section_tolerance, &mut Work::new(meshanalysis::ANALYSIS_WORK))
                .map_err(|error| format!("The section stopped: {error}"))?;
            if let Some(within) = scancommon::Within::parse(input)? {
                let inside = within.clip_section(&mut cut);
                if cut.loops.is_empty() {
                    return Err(format!("Nothing inside the box: the cut of {} has no contour within it, so nothing was fitted.", surface.label));
                }
                inside_box = Some(within.public(inside.public()));
            } else if cut.loops.is_empty() {
                return Err(format!(
                    "The plane misses {} (no contour at that plane); check the plane against its bounds {}.",
                    surface.label,
                    surface.brief()["bounds"]
                ));
            }
            let chains: Vec<Value> = cut
                .loops
                .iter()
                .take(LISTED_CHAINS)
                .enumerate()
                .map(|(i, path)| json!({"index": i, "closed": path.closed, "points": path.points.len(), "length": scancommon::round_to(polyline_length(&path.points) + if path.closed { path.points.last().zip(path.points.first()).map_or(0.0, |(a, b)| dist2(*a, *b)) } else { 0.0 }, 3)}))
                .collect();
            let longest = (0..cut.loops.len())
                .max_by(|&a, &b| polyline_length(&cut.loops[a].points).total_cmp(&polyline_length(&cut.loops[b].points)))
                .unwrap_or(0);
            let mut dropped = 0;
            let mut chosen: Vec<usize> = match section.get("chain") {
                None | Some(Value::Null) => vec![longest],
                Some(Value::String(all)) if all == "all" => {
                    // The longest MAX_CHAINS; the rest (crumbs) are dropped.
                    let mut all: Vec<usize> = (0..cut.loops.len()).collect();
                    all.sort_by(|&a, &b| polyline_length(&cut.loops[b].points).total_cmp(&polyline_length(&cut.loops[a].points)));
                    dropped = all.len().saturating_sub(MAX_CHAINS);
                    all.truncate(MAX_CHAINS);
                    all.sort_unstable();
                    all
                }
                Some(Value::Number(index)) => vec![index.as_u64().map(|i| i as usize).ok_or("section.chain must be a chain index.")?],
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|item| item.as_u64().map(|i| i as usize).ok_or("section.chain lists chain indices."))
                    .collect::<Result<_, _>>()?,
                Some(_) => return Err("section.chain must be an index, a list of indices or \"all\".".into()),
            };
            chosen.sort_unstable();
            chosen.dedup();
            if chosen.len() > MAX_CHAINS {
                return Err(format!("section.chain lists {} chains; at most {MAX_CHAINS} can be joined.", chosen.len()));
            }
            if chosen.is_empty() {
                return Err("section.chain lists no chains.".into());
            }
            for index in &chosen {
                if *index >= cut.loops.len() {
                    return Err(format!("section.chain {index} does not exist; the cut has {} chains (first {LISTED_CHAINS}: {}).", cut.loops.len(), json!(chains)));
                }
            }
            let picked: Vec<(Vec<V2>, bool)> = chosen.iter().map(|&i| (cut.loops[i].points.clone(), cut.loops[i].closed)).collect();
            let single_closed = picked.len() == 1 && picked[0].1;
            let total: f64 = picked.iter().map(|(p, c)| polyline_length(p) + if *c { dist2(p[0], *p.last().expect("points")) } else { 0.0 }).sum();
            let spacing = (total / (MAX_COMPARE_SAMPLES as f64 * 0.9)).max(tolerance / 5.0).max(1e-4);
            let mut samples = Vec::new();
            let mut breaks = Vec::new();
            let ordered = if picked.len() == 1 { vec![picked[0].0.clone()] } else { join_chains(picked, deadline)? };
            for chain in &ordered {
                let dense = densify(chain, single_closed, spacing);
                for (k, p) in dense.iter().enumerate() {
                    breaks.push(k == 0 && !samples.is_empty());
                    samples.push(*p);
                }
            }
            if samples.len() > MAX_COMPARE_SAMPLES {
                // Rare: very many short chains. Keep breaks aligned.
                let keep: Vec<usize> = (0..MAX_COMPARE_SAMPLES).map(|i| i * samples.len() / MAX_COMPARE_SAMPLES).collect();
                let mut b2 = Vec::with_capacity(keep.len());
                let mut prev = 0;
                for (slot, &i) in keep.iter().enumerate() {
                    b2.push(slot > 0 && (prev + 1..=i).any(|j| breaks[j]));
                    prev = i;
                }
                samples = keep.iter().map(|&i| samples[i]).collect();
                breaks = b2;
            }
            let fit_index = decimate(samples.len(), MAX_FIT_SAMPLES);
            let fit = fit_index.iter().map(|&i| samples[i]).collect();
            let basis = (cut.origin, cut.u, cut.v, scancommon::unit(normal));
            source = json!({
                "kind": "section",
                "surface": surface.brief(),
                "chainCount": cut.loops.len(),
                "chains": chains,
                "chainsListed": chains.len(),
                "chainsUsed": chosen,
                "chainsDropped": dropped,
                "sectionTolerance": section_tolerance,
            });
            if let Some(inside) = inside_box.take() {
                source["within"] = inside;
            }
            // Only the triangles the plane crosses: the distance is to the
            // cut surface there, not to a face just beside the plane.
            let eps = 1e-9 * surface.bounds().map_or(1.0, |(lo, hi)| scancommon::norm(scancommon::sub(hi, lo)).max(1.0));
            let side = |p: [f64; 3]| scancommon::dot(scancommon::sub(p, point), normal);
            let crossing: Vec<[u32; 3]> = surface
                .mesh
                .triangles
                .iter()
                .filter(|t| {
                    let d = t.map(|i| side(surface.mesh.points[i as usize]));
                    d.iter().fold(f64::INFINITY, |a, b| a.min(*b)) <= eps && d.iter().fold(f64::NEG_INFINITY, |a, b| a.max(*b)) >= -eps
                })
                .copied()
                .collect();
            surface_index = Some(MeshIndex::from_faces(&surface.mesh.points, crossing.iter()));
            (Data { samples, breaks, fit, fit_index, closed: single_closed, continuous: true }, Some(basis))
        }
        (None, Some(points)) => {
            let items = points.as_array().filter(|items| (3..=MAX_COMPARE_SAMPLES).contains(&items.len())).ok_or(format!("points must be 3 to {MAX_COMPARE_SAMPLES} [x, y] pairs."))?;
            let mut samples = Vec::with_capacity(items.len());
            for item in items {
                let pair = item.as_array().filter(|pair| pair.len() == 2).ok_or("points holds [x, y] pairs.")?;
                let coordinate = |v: &Value| v.as_f64().filter(|v| v.is_finite() && v.abs() <= MAX_COORDINATE);
                let x = coordinate(&pair[0]).ok_or("points hold finite numbers within ±10,000,000 mm.")?;
                let y = coordinate(&pair[1]).ok_or("points hold finite numbers within ±10,000,000 mm.")?;
                samples.push([x, y]);
            }
            let basis = match input.get("plane").filter(|v| !v.is_null()) {
                Some(plane) => {
                    let (point, normal) = plane_input(plane)?;
                    Some(basis_of(point, normal)?)
                }
                None => None,
            };
            source = json!({"kind": "points", "samples": samples.len()});
            let breaks = vec![false; samples.len()];
            let fit_index = decimate(samples.len(), MAX_FIT_SAMPLES);
            let fit = fit_index.iter().map(|&i| samples[i]).collect();
            (Data { samples, breaks, fit, fit_index, closed: false, continuous: false }, basis)
        }
    };
    let closed = closed_input.unwrap_or(data.closed);
    let min_ctrl = if closed { deg + 2 } else { deg + 1 };
    if budget < min_ctrl {
        return Err(format!("maxControlPoints must be at least {min_ctrl} for a {} curve of degree {deg}.", if closed { "closed" } else { "open" }));
    }
    let budget = budget.min(data.fit.len().max(min_ctrl));
    if data.fit.len() < min_ctrl {
        return Err(format!("{} samples are too few for a degree-{deg} fit (need at least {min_ctrl}).", data.fit.len()));
    }

    // Chord-length parameters, normalised 0..1.
    let fit = &data.fit;
    let mut cumulative = vec![0.0];
    for w in fit.windows(2) {
        cumulative.push(cumulative.last().copied().unwrap_or(0.0) + dist2(w[0], w[1]));
    }
    let open_length = *cumulative.last().unwrap_or(&0.0);
    let total = if closed { open_length + dist2(fit[fit.len() - 1], fit[0]) } else { open_length };
    if !(total > 0.0) {
        return Err("The samples all coincide; nothing to fit.".into());
    }
    let start_s: Vec<f64> = cumulative.iter().map(|c| c / total).collect();
    let ends = (!closed && fix_ends).then(|| (fit[0], fit[fit.len() - 1]));

    // Grow the control points until within tolerance, then search back
    // for the fewest that still are.
    let mut attempts: Vec<Value> = Vec::new();
    let mut best: Option<(usize, Attempt)> = None;
    let mut last_fail = min_ctrl - 1;
    let mut n = min_ctrl;
    let mut warm = start_s.clone();
    let try_fit = |n: usize, warm: &[f64], attempts: &mut Vec<Value>| -> Result<Option<Attempt>, String> {
        let attempt = fit_with(fit, warm, n, deg, closed, ends, reject, deadline)?;
        if let Some(a) = &attempt {
            attempts.push(json!({"controlPoints": n, "maxError": scancommon::round_to(a.max_error, 4), "rejected": a.accepted.iter().filter(|x| !**x).count()}));
        }
        Ok(attempt)
    };
    loop {
        let Some(attempt) = try_fit(n, &warm, &mut attempts)? else {
            last_fail = n;
            if n >= budget {
                break;
            }
            n = (n + 1).min(budget);
            continue;
        };
        let ok = attempt.max_error <= tolerance;
        warm = attempt.s.clone();
        if ok {
            best = Some((n, attempt));
            break;
        }
        last_fail = n;
        best = match best {
            Some((bn, b)) if b.max_error <= attempt.max_error => Some((bn, b)),
            _ => Some((n, attempt)),
        };
        if n >= budget {
            break;
        }
        n = (n + (n / 6).max(1)).min(budget);
    }
    // Overshot by a growth step: bisect back down.
    if let Some((found, _)) = best.as_ref().filter(|(_, b)| b.max_error <= tolerance).map(|(n, _)| (*n, ())) {
        let (mut lo, mut hi) = (last_fail.max(min_ctrl - 1), found);
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            match try_fit(mid, &start_s, &mut attempts)? {
                Some(attempt) if attempt.max_error <= tolerance => {
                    hi = mid;
                    best = Some((mid, attempt));
                }
                _ => lo = mid,
            }
        }
    }
    let (_, attempt) = best.ok_or("The fit could not be solved (degenerate samples); check the data.")?;
    let rejection_capped = attempt.rejection_capped;
    // Round the control points once: everything below uses these.
    let spline = Spline { deg, closed, ctrl: attempt.spline.ctrl.iter().map(|p| r4(*p)).collect() };

    // Residuals of the accepted fit samples (fit's own measure).
    let residual = Stats::of(&attempt.errors.iter().zip(&attempt.accepted).filter(|(_, a)| **a).map(|(e, _)| (*e, 1.0)).collect::<Vec<_>>());

    // Dense curve for the independent comparison.
    let dense_n = DENSE_CURVE_POINTS.max(spline.spans() * 16);
    let dense = spline.points(if closed { dense_n } else { dense_n + 1 });
    let dense_len = dense.len();
    crate::meshops::check(Some(deadline))?;
    // (1) every sample to the curve.
    let sample_d: Vec<(f64, V2)> = data.samples.iter().map(|p| polyline_distance(*p, &dense, closed)).collect();
    let samples_to_curve = Stats::of(&sample_d.iter().map(|(d, _)| (*d, 1.0)).collect::<Vec<_>>());
    // Samples standing for a rejected fit sample (up to halfway to its
    // neighbours); every other sample must be within tolerance of the
    // rounded curve.
    let mut rejected_mask = vec![false; data.samples.len()];
    for (j, accepted) in attempt.accepted.iter().enumerate() {
        if *accepted {
            continue;
        }
        let at = data.fit_index[j];
        let lo = if j > 0 { (data.fit_index[j - 1] + at) / 2 + 1 } else { 0 };
        let hi = data.fit_index.get(j + 1).map_or(data.samples.len() - 1, |next| (at + next) / 2);
        for slot in &mut rejected_mask[lo.min(at)..=hi.max(at)] {
            *slot = true;
        }
    }
    let accepted_max = sample_d.iter().zip(&rejected_mask).filter(|(_, r)| !**r).map(|((d, _), _)| d.abs()).fold(0.0, f64::max);
    let within = accepted_max <= tolerance;
    let within_all = samples_to_curve.max <= tolerance;
    // Median spacing of the data (jumps excluded).
    let mut spacings: Vec<f64> = (1..data.samples.len()).filter(|i| !data.breaks[*i]).map(|i| dist2(data.samples[i - 1], data.samples[i])).collect();
    spacings.sort_by(f64::total_cmp);
    let median_spacing = spacings.get(spacings.len() / 2).copied().unwrap_or(0.0);
    let support_limit = (3.0 * median_spacing).max(2.0 * tolerance);
    // (2) the curve to the data.
    // Consecutive samples are joined by data unless a jump lies between
    // them; for discrete points, also unless they are farther apart than
    // the support limit (a gap in the points).
    let joined = |i: usize| -> bool {
        i + 1 < data.samples.len()
            && !data.breaks[i + 1]
            && (data.continuous || dist2(data.samples[i], data.samples[i + 1]) <= support_limit)
    };
    let data_distance = |p: V2| -> f64 {
        {
            let mut best = f64::INFINITY;
            for i in 0..data.samples.len() {
                let d = if joined(i) {
                    segment_nearest(p, data.samples[i], data.samples[i + 1]).0
                } else {
                    dist2(p, data.samples[i])
                };
                best = best.min(d);
            }
            if data.closed && data.samples.len() > 2 {
                best = best.min(segment_nearest(p, data.samples[data.samples.len() - 1], data.samples[0]).0);
            }
            best
        }
    };
    let curve_d: Vec<f64> = dense.iter().map(|p| data_distance(*p)).collect();
    crate::meshops::check(Some(deadline))?;
    let arc = |i: usize| -> f64 {
        let prev = if i > 0 { dist2(dense[i - 1], dense[i]) } else if closed { dist2(dense[dense_len - 1], dense[0]) } else { 0.0 };
        let next = if i + 1 < dense_len { dist2(dense[i], dense[i + 1]) } else if closed { dist2(dense[i], dense[0]) } else { 0.0 };
        0.5 * (prev + next)
    };
    let unsupported: Vec<bool> = curve_d.iter().map(|d| *d > support_limit).collect();
    let curve_to_samples = Stats::of(&(0..dense_len).filter(|i| !unsupported[*i]).map(|i| (curve_d[i], arc(i))).collect::<Vec<_>>());
    // Unsupported spans: maximal runs (wrapping on a closed curve).
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < dense_len {
        if !unsupported[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < dense_len && unsupported[i] {
            i += 1;
        }
        spans.push((start, i - 1));
    }
    if closed && spans.len() > 1 && spans[0].0 == 0 && spans.last().is_some_and(|s| s.1 == dense_len - 1) {
        let first = spans.remove(0);
        if let Some(last) = spans.last_mut() {
            last.1 = first.1 + dense_len;
        }
    }
    let unsupported_spans: Vec<Value> = spans
        .iter()
        .map(|&(a, b)| {
            let idx = |k: usize| k % dense_len;
            let length: f64 = (a..b).map(|k| dist2(dense[idx(k)], dense[idx(k + 1)])).sum();
            let max_gap = (a..=b).map(|k| curve_d[idx(k)]).fold(0.0, f64::max);
            let mut span = json!({
                "from2d": r2pt(dense[idx(a)]),
                "to2d": r2pt(dense[idx(b)]),
                "length": scancommon::round_to(length, 3),
                "maxGap": scancommon::round_to(max_gap, 3),
            });
            if let Some(basis) = &basis {
                span["from3d"] = json!(scancommon::r3(lift(basis, dense[idx(a)])));
                span["to3d"] = json!(scancommon::r3(lift(basis, dense[idx(b)])));
            }
            span
        })
        .collect();
    // (3) the lifted curve to the cut surface.
    let curve_to_surface = match (&surface_index, &basis) {
        (Some(index), Some(basis)) => {
            let values: Vec<(f64, f64)> = (0..dense_len)
                .filter(|i| !unsupported[*i])
                .filter_map(|i| index.nearest_face(lift(basis, dense[i])).map(|(d, ..)| (d, arc(i))))
                .collect();
            Some(Stats::of(&values))
        }
        _ => None,
    };
    crate::meshops::check(Some(deadline))?;

    // Rejected fit samples, measured against the final curve.
    let rejected: Vec<(V2, f64)> = fit
        .iter()
        .zip(&attempt.accepted)
        .filter(|(_, a)| !**a)
        .map(|(p, _)| (*p, polyline_distance(*p, &dense, closed).0))
        .collect();

    // Thickness band and where it would fold.
    let profile_n = (spline.spans() * 16).clamp(64, 1000);
    let mut thickness_value = Value::Null;
    let mut band_loops: Vec<Vec<V2>> = Vec::new();
    let mut folds = false;
    if let Some(t) = &thickness {
        let mut conflicts = Vec::new();
        // Signed curvature at the dense points (left turn positive).
        let curvature: Vec<f64> = (0..dense_len)
            .map(|i| {
                if !closed && (i == 0 || i + 1 == dense_len) {
                    return 0.0;
                }
                let (a, b, c) = (dense[(i + dense_len - 1) % dense_len], dense[i], dense[(i + 1) % dense_len]);
                let (u, w) = (sub2(b, a), sub2(c, b));
                let turn = (u[0] * w[1] - u[1] * w[0]).atan2(u[0] * w[0] + u[1] * w[1]);
                let ds = 0.5 * (len2(u) + len2(w));
                if ds > 0.0 { turn / ds } else { 0.0 }
            })
            .collect();
        for offset in t.offsets.iter().filter(|o| **o != 0.0) {
            let bad: Vec<bool> = curvature.iter().map(|k| k * offset >= 1.0).collect();
            let mut i = 0;
            while i < dense_len {
                if !bad[i] {
                    i += 1;
                    continue;
                }
                let start = i;
                while i < dense_len && bad[i] {
                    i += 1;
                }
                let worst = (start..i).max_by(|&a, &b| curvature[a].abs().total_cmp(&curvature[b].abs())).unwrap_or(start);
                let mut conflict = json!({
                    "offset": offset,
                    "at2d": r2pt(dense[worst]),
                    "minRadius": scancommon::round_to(1.0 / curvature[worst].abs(), 3),
                    "length": scancommon::round_to((start..i - 1).map(|k| dist2(dense[k], dense[k + 1])).sum::<f64>(), 3),
                });
                if let Some(basis) = &basis {
                    conflict["at3d"] = json!(scancommon::r3(lift(basis, dense[worst])));
                }
                conflicts.push(conflict);
                if conflicts.len() >= 20 {
                    break;
                }
            }
        }
        band_loops = t.offsets.iter().map(|o| spline.offset(profile_n, *o)).collect();
        thickness_value = json!({
            "value": t.value,
            "side": t.side,
            "offsets": t.offsets,
            "note": "Offsets are along the left normal of the curve direction (+ = left); both centres the band on the curve.",
            "offsetConflicts": conflicts,
            "folds": !conflicts.is_empty(),
        });
        folds = !conflicts.is_empty();
    }

    // SCAD, then compile it to prove it evaluates to this curve.
    let scad = scad_source(&prefix, &spline, profile_n, thickness.as_ref());
    let has_profile = closed || thickness.is_some();
    let expected_area = if !has_profile {
        None
    } else if let Some(t) = &thickness {
        if closed {
            Some((shoelace(&band_loops[0]).abs() - shoelace(&band_loops[1]).abs()).abs())
        } else {
            let mut band = spline.offset(profile_n, t.offsets[0]);
            band.extend(spline.offset(profile_n, t.offsets[1]).into_iter().rev());
            Some(shoelace(&band).abs())
        }
    } else {
        Some(shoelace(&spline.points(profile_n)).abs())
    };
    let validated = validate_scad(state, &prefix, &scad, &spline, has_profile, expected_area, thickness.as_ref(), folds)?;

    // Reply.
    let worst = sample_d
        .iter()
        .enumerate()
        .max_by(|a, b| a.1 .0.abs().total_cmp(&b.1 .0.abs()))
        .map(|(i, (d, foot))| (data.samples[i], *d, *foot));
    let mut value = design.header();
    value["ok"] = json!(true);
    value["source"] = source;
    if let Some(basis) = &basis {
        let (o, u, v, n) = basis;
        value["plane"] = json!({
            "origin": scancommon::r3(*o),
            "u": scancommon::r3(*u),
            "v": scancommon::r3(*v),
            "normal": scancommon::r3(*n),
            "placeMatrix": place_matrix(basis),
        });
    }
    value["data"] = json!({
        "samples": data.samples.len(),
        "fitSamples": fit.len(),
        "closed": data.closed,
        "medianSpacing": scancommon::round_to(median_spacing, 4),
        "jumps": data.breaks.iter().filter(|b| **b).count(),
    });
    let mut fit_value = json!({
        "degree": deg,
        "closed": closed,
        "controlPoints": spline.ctrl,
        "controlPointsUsed": spline.ctrl.len(),
        "budget": budget,
        "knots": spline.knots(),
        "domain": [spline.domain().0, spline.domain().1],
        "tolerance": tolerance,
        "withinTolerance": within,
        "maxErrorAccepted": scancommon::round_to(accepted_max, 4),
        "withinToleranceScope": "every data sample except those rejected, against the rounded control points",
        "residual": residual.public(),
        "attempts": attempts,
        "fixedEnds": ends.is_some(),
    });
    if !within {
        fit_value["note"] = json!(format!(
            "The budget of {budget} control points could not bring every accepted sample within {tolerance} mm \
             (max {:.4}); raise maxControlPoints or tolerance, or check for a misfit chain.",
            accepted_max
        ));
    }
    if !rejected.is_empty() {
        fit_value["withinToleranceAllSamples"] = json!(within_all);
    }
    value["fit"] = fit_value;
    value["rejected"] = json!({
        "count": rejected.len(),
        "limit": scancommon::round_to(reject, 4),
        "capped": rejection_capped,
        "note": if rejection_capped { "More than 20% of samples lay beyond rejectOutliers: they were kept, as that is a misfit, not noise." } else { "" },
        "points": rejected.iter().take(50).map(|(p, d)| {
            let mut point = json!({"at2d": r2pt(*p), "distance": scancommon::round_to(*d, 4)});
            if let Some(basis) = &basis {
                point["at3d"] = json!(scancommon::r3(lift(basis, *p)));
            }
            point
        }).collect::<Vec<_>>(),
    });
    value["unsupportedSpans"] = json!(unsupported_spans);
    value["supportLimit"] = json!(scancommon::round_to(support_limit, 4));
    let mut comparison = json!({
        "samplesToCurve": samples_to_curve.public(),
        "curveToSamples": curve_to_samples.public(),
        "unsupportedFraction": scancommon::round_to(unsupported.iter().filter(|u| **u).count() as f64 / dense_len as f64, 4),
        "signConvention": "+ = right of the curve direction (outside a counter-clockwise loop).",
    });
    if let Some(stats) = &curve_to_surface {
        comparison["curveToSurface"] = stats.public();
    }
    if let Some((p, d, foot)) = worst {
        let mut w = json!({"at2d": r2pt(p), "onCurve2d": r2pt(foot), "distance": scancommon::round_to(d, 4)});
        if let Some(basis) = &basis {
            w["at3d"] = json!(scancommon::r3(lift(basis, p)));
        }
        comparison["worstSample"] = w;
    }
    value["comparison"] = comparison;
    if !thickness_value.is_null() {
        value["thickness"] = thickness_value;
    }
    value["scad"] = json!(scad);
    value["validated"] = validated;
    let messages = design.messages();
    let warnings: Vec<&String> = crate::agent::warning_lines(&messages);
    if !warnings.is_empty() {
        value["warnings"] = json!(warnings);
    }
    value["elapsedMs"] = json!(started.elapsed().as_millis() as u64);

    // Someone is watching the design: the curve goes on the model, in the
    // plane it was fitted in (unsupported stretches in red; the worst sample is
    // marked from the reply), and a picture to the console if the agent asked
    // for none.
    let watched = crate::activity::collecting();
    if watched {
        if let Some(basis) = &basis {
            let step = (dense.len() / 1500).max(1);
            let lifted = |points: &[V2]| points.iter().step_by(step).map(|p| scancommon::r3(lift(basis, *p))).collect::<Vec<_>>();
            let mut items = vec![json!({"kind": "polyline", "points": lifted(&dense), "closed": spline.closed, "tone": "agent"})];
            let mut i = 0;
            while i < dense.len() && items.len() < 40 {
                if !unsupported[i] {
                    i += 1;
                    continue;
                }
                let start = i;
                while i < dense.len() && unsupported[i] {
                    i += 1;
                }
                let run = &dense[start.saturating_sub(1)..(i + 1).min(dense.len())];
                items.push(json!({"kind": "polyline", "points": lifted(run), "closed": false, "tone": "bad"}));
            }
            crate::activity::offer_items(items);
        }
    }
    // Drawn for the reply when asked for, else for the console when watched.
    let requested = image.is_some();
    let drawing = image.or_else(|| watched.then(|| json!({})));
    let png = match &drawing {
        Some(options) => {
            // Which model direction u and v run along, for the scale label.
            let name = |d: [f64; 3]| {
                let k = (0..3).max_by(|&a, &b| d[a].abs().total_cmp(&d[b].abs())).unwrap_or(0);
                format!("{}{}", if d[k] < 0.0 { "-" } else { "+" }, ["x", "y", "z"][k])
            };
            let axis_names = basis.as_ref().map_or(("x".to_string(), "y".to_string()), |(_, u, v, _)| (name(*u), name(*v)));
            Some(draw(options, &data, fit, &attempt.accepted, &spline, &dense, &unsupported, &band_loops, worst, axis_names))
        }
        None => None,
    };
    if let (Some(png), true) = (&png, requested) {
        value["image"] = json!({"mimeType": "image/png", "bytes": png.len()});
    }
    let caption = format!(
        "fit_curve: {} control points, degree {deg}, {}; samples-to-curve rms {:.4} max {:.4} mm (tolerance {}); {} unsupported \
         span(s). Orange = curve and thickness band, blue = control polygon, grey dots = data, red = unsupported spans (#k = \
         unsupportedSpans[k-1], the {} longest numbered), red crosses = rejected samples, magenta ring = worst sample.",
        spline.ctrl.len(),
        if closed { "closed" } else { "open" },
        samples_to_curve.rms,
        samples_to_curve.max,
        scancommon::short(tolerance),
        spans.len(),
        MAX_SPAN_LABELS,
    );
    match png {
        Some(png) if !requested => {
            crate::activity::offer_image(&caption, png);
            Ok((value, None))
        }
        png => Ok((value, png.map(|png| (caption, png)))),
    }
}

// ---------------------------------------------------------------------------
// SCAD
// ---------------------------------------------------------------------------

fn scad_number(value: f64) -> String {
    let text = format!("{}", scancommon::round_to(value, 4));
    if text == "-0" { "0".into() } else { text }
}

fn scad_source(p: &str, spline: &Spline, profile_n: usize, thickness: Option<&Thickness>) -> String {
    let ctrl = spline
        .ctrl
        .iter()
        .map(|c| format!("[{}, {}]", scad_number(c[0]), scad_number(c[1])))
        .collect::<Vec<_>>()
        .join(", ");
    let mut s = String::new();
    s.push_str(&format!(
        "// fit_curve: uniform B-spline, degree {}, {}, in plane coordinates (mm). Edit {p}_ctrl to reshape;\n\
         // place it in the model with multmatrix(placeMatrix) as for section_profile.\n",
        spline.deg,
        if spline.closed { "closed (periodic)" } else { "open, clamped" }
    ));
    s.push_str(&format!("{p}_deg = {};\n{p}_closed = {};\n{p}_ctrl = [{ctrl}];\n{p}_n = {profile_n};\n", spline.deg, spline.closed));
    if let Some(t) = thickness {
        s.push_str(&format!(
            "// Band: offsets along the left normal of the curve direction ({} side, {} mm).\n{p}_offsets = [{}, {}];\n",
            t.side,
            scad_number(t.value),
            scad_number(t.offsets[0]),
            scad_number(t.offsets[1])
        ));
    }
    s.push_str(&format!(
        "{p}_cp = {p}_closed ? concat({p}_ctrl, [for (i = [0 : 1 : {p}_deg - 1]) {p}_ctrl[i]]) : {p}_ctrl;\n\
         {p}_knots = {p}_closed ? [for (i = [0 : 1 : len({p}_cp) + {p}_deg]) i]\n\
         \x20   : concat([for (i = [0 : 1 : {p}_deg]) 0], [for (i = [1 : 1 : len({p}_ctrl) - {p}_deg - 1]) i], [for (i = [0 : 1 : {p}_deg]) len({p}_ctrl) - {p}_deg]);\n\
         {p}_t0 = {p}_closed ? {p}_deg : 0;\n\
         {p}_t1 = {p}_closed ? len({p}_ctrl) + {p}_deg : len({p}_ctrl) - {p}_deg;\n\
         function {p}_span(t) = let (c = [for (i = [{p}_deg : 1 : len({p}_cp) - 1]) if ({p}_knots[i] <= t) i]) c[len(c) - 1];\n\
         function {p}_db(t, s, d, r) = r > {p}_deg ? d[{p}_deg]\n\
         \x20   : {p}_db(t, s, [for (j = [0 : 1 : {p}_deg]) j < r ? d[j]\n\
         \x20       : let (i = j + s - {p}_deg, a = (t - {p}_knots[i]) / ({p}_knots[i + {p}_deg + 1 - r] - {p}_knots[i])) (1 - a) * d[j - 1] + a * d[j]], r + 1);\n\
         function {p}_at(t) = let (s = {p}_span(t)) {p}_db(t, s, [for (j = [0 : 1 : {p}_deg]) {p}_cp[j + s - {p}_deg]], 1);\n\
         // n points along the curve (a closed curve does not repeat its start).\n\
         function {p}_points(n) = [for (q = [0 : 1 : n - 1]) {p}_at({p}_t0 + ({p}_t1 - {p}_t0) * q / ({p}_closed ? n : n - 1))];\n\
         // The points moved d mm along the left normal.\n\
         function {p}_offset(n, d) = let (c = {p}_points(n), m = len(c))\n\
         \x20   [for (q = [0 : 1 : m - 1]) let (\n\
         \x20       a = {p}_closed ? c[(q + m - 1) % m] : c[max(q - 1, 0)],\n\
         \x20       b = {p}_closed ? c[(q + 1) % m] : c[min(q + 1, m - 1)],\n\
         \x20       t = (b - a) / norm(b - a)) c[q] + d * [-t[1], t[0]]];\n"
    ));
    if thickness.is_some() {
        s.push_str(&format!(
            "// Closed: the two offset loops [a, b]; open: one polygon around the band.\n\
             function {p}_band(n) = let (a = {p}_offset(n, {p}_offsets[0]), b = {p}_offset(n, {p}_offsets[1]))\n\
             \x20   {p}_closed ? [a, b] : concat(a, [for (q = [len(b) - 1 : -1 : 0]) b[q]]);\n"
        ));
        if spline.closed {
            s.push_str(&format!(
                "module {p}_profile() {{\n    b = {p}_band({p}_n);\n    difference() {{\n        union() {{ polygon(b[0]); polygon(b[1]); }}\n        intersection() {{ polygon(b[0]); polygon(b[1]); }}\n    }}\n}}\n"
            ));
        } else {
            s.push_str(&format!("module {p}_profile() polygon({p}_band({p}_n));\n"));
        }
    } else if spline.closed {
        s.push_str(&format!("module {p}_profile() polygon({p}_points({p}_n));\n"));
    }
    s
}

#[allow(clippy::too_many_arguments)]
fn validate_scad(
    state: &AppState,
    p: &str,
    scad: &str,
    spline: &Spline,
    has_profile: bool,
    expected_area: Option<f64>,
    thickness: Option<&Thickness>,
    folds: bool,
) -> Result<Value, String> {
    // The SCAD evaluates its own curve; compare it to ours at a few points.
    let n = 9;
    let expected: Vec<V2> = match thickness {
        Some(t) => spline.offset(n, t.offsets[1]),
        None => spline.points(n),
    };
    let expected_text = expected.iter().map(|q| format!("[{:?}, {:?}]", q[0], q[1])).collect::<Vec<_>>().join(", ");
    let probe = match thickness {
        Some(_) => format!("{p}_offset({n}, {p}_offsets[1])"),
        None => format!("{p}_points({n})"),
    };
    let mut code = format!(
        "{scad}\n{p}_expected = [{expected_text}];\necho({p}_dev = max([for (q = [0 : 1 : {n} - 1]) norm({probe}[q] - {p}_expected[q])]));\n"
    );
    if has_profile {
        code.push_str(&format!("linear_extrude(height = 1) {p}_profile();\n"));
    }
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
    let mut value = json!({"curveDeviation": deviation});
    let mut ok = deviation.is_some_and(|d| d <= 1e-6);
    if has_profile {
        let area = if compiled.parts.is_empty() { 0.0 } else { crate::meshops::signed_volume(&meshanalysis::from_engine_mesh(&compiled.mesh)).abs() };
        value["area"] = json!(scancommon::round_to(area, 4));
        if let Some(expected) = expected_area {
            value["expectedArea"] = json!(scancommon::round_to(expected, 4));
            let agrees = (area - expected).abs() <= 1e-6 * expected.max(1.0) + 1e-6;
            value["areaAgrees"] = json!(agrees);
            ok &= area > 0.0 && (agrees || folds);
        }
    }
    value["ok"] = json!(ok);
    if !ok {
        value["note"] = json!("The SCAD did not reproduce the fitted curve or profile exactly (a folded band does, if offsetConflicts lists folds).");
    }
    Ok(value)
}

// ---------------------------------------------------------------------------
// Image
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn draw(
    options: &Value,
    data: &Data,
    fit: &[V2],
    accepted: &[bool],
    spline: &Spline,
    dense: &[V2],
    unsupported: &[bool],
    band: &[Vec<V2>],
    worst: Option<(V2, f64, V2)>,
    axis_names: (String, String),
) -> Vec<u8> {
    let width = options.get("width").and_then(Value::as_u64).unwrap_or(900).clamp(200, 2400) as u32;
    let height = options.get("height").and_then(Value::as_u64).unwrap_or(700).clamp(150, 2400) as u32;
    let mut lo = [f64::INFINITY; 2];
    let mut hi = [f64::NEG_INFINITY; 2];
    for p in data.samples.iter().chain(dense.iter()).chain(band.iter().flatten()) {
        for k in 0..2 {
            lo[k] = lo[k].min(p[k]);
            hi[k] = hi[k].max(p[k]);
        }
    }
    let extent = [(hi[0] - lo[0]).max(1e-3), (hi[1] - lo[1]).max(1e-3)];
    let px_per_mm = ((width as f64 - 60.0) / (extent[0] * 1.12)).min((height as f64 - 70.0) / (extent[1] * 1.12));
    let center = [(lo[0] + hi[0]) * 0.5, (lo[1] + hi[1]) * 0.5];
    let mut canvas = Canvas::new(width, height, center, px_per_mm);
    canvas.grid((axis_names.0.as_str(), axis_names.1.as_str()));
    let blue = [120, 170, 230, 255];
    let mut polygon = spline.ctrl.clone();
    if spline.closed {
        polygon.push(spline.ctrl[0]);
    }
    canvas.dashed(&polygon, false, 1.2, 6.0, blue);
    for c in &spline.ctrl {
        canvas.ring(*c, 3.5, 1.4, [60, 120, 210, 255]);
    }
    let orange = [235, 120, 20, 255];
    canvas.polyline(dense, spline.closed, 2.4, orange);
    for edge in band {
        canvas.polyline(edge, spline.closed, 1.1, orange);
    }
    // Data on top of the curve, so a curve off the data shows.
    let stride = (data.samples.len() / 4000).max(1);
    for p in data.samples.iter().step_by(stride) {
        canvas.dot(*p, 1.3, [70, 70, 70, 200]);
    }
    // Unsupported runs, all highlighted; only the longest few get a number
    // (#k is unsupportedSpans[k-1]). Pictures carry no other text: the values
    // are in the reply, so a poor fit never buries the data under labels.
    let n = dense.len();
    let mut i = 0;
    let mut runs: Vec<Vec<V2>> = Vec::new();
    while i < n {
        if !unsupported[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && unsupported[i] {
            i += 1;
        }
        let mut run: Vec<V2> = dense[start.saturating_sub(1)..(i + 1).min(n)].to_vec();
        if spline.closed && i == n {
            run.push(dense[0]);
        }
        canvas.polyline(&run, false, 7.0, [215, 30, 30, 110]);
        canvas.dashed(&run, false, 3.0, 7.0, [215, 30, 30, 255]);
        runs.push(run);
    }
    // A closed curve's run through the seam is one span, merged the way the
    // reply merges it (the first run joins the last), so #k matches.
    if spline.closed && runs.len() > 1 && unsupported.first() == Some(&true) && unsupported.last() == Some(&true) {
        let first = runs.remove(0);
        if let Some(last) = runs.last_mut() {
            last.extend(first);
        }
    }
    let run_length =|run: &[V2]| run.windows(2).map(|w| ((w[1][0] - w[0][0]).powi(2) + (w[1][1] - w[0][1]).powi(2)).sqrt()).sum::<f64>();
    let mut longest: Vec<usize> = (0..runs.len()).collect();
    longest.sort_by(|&a, &b| run_length(&runs[b]).total_cmp(&run_length(&runs[a])));
    let scale = canvas.text_scale as f64;
    let mut placed: Vec<[f64; 4]> = Vec::new();
    for &k in longest.iter().take(MAX_SPAN_LABELS) {
        let run = &runs[k];
        let text = format!("#{}", k + 1);
        let at = canvas.to_px(run[run.len() / 2]);
        let (x, y) = (at[0] + 6.0, at[1] - 4.0 * scale);
        let rect = [x - 2.0, y - 2.0, x + crate::raster::text_width(&text, canvas.text_scale) as f64 + 2.0, y + 8.0 * scale + 2.0];
        if placed.iter().any(|r| rect[0] < r[2] && r[0] < rect[2] && rect[1] < r[3] && r[1] < rect[3]) {
            continue;
        }
        canvas.text_px(x, y, &text, [200, 20, 20, 255]);
        placed.push(rect);
    }

    for (p, a) in fit.iter().zip(accepted) {
        if !a {
            canvas.cross_mark(*p, 4.0, [215, 30, 30, 255]);
        }
    }
    if let Some((p, _, foot)) = worst {
        canvas.ring(p, 9.0, 2.0, [200, 20, 120, 255]);
        let (a, b) = (canvas.to_px(p), canvas.to_px(foot));
        canvas.line_px(a, b, 1.5, [200, 20, 120, 255]);
    }
    canvas.png()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scancommon::testkit;

    fn run(state: &AppState, input: Value) -> Value {
        fit_curve_value(state, &input).map(|(value, _)| value).unwrap_or_else(|error| panic!("{error}"))
    }

    fn circle(r: f64, n: usize) -> Vec<Value> {
        (0..n).map(|i| {
            let a = i as f64 / n as f64 * std::f64::consts::TAU;
            json!([r * a.cos(), r * a.sin()])
        }).collect()
    }

    #[test]
    fn spline_evaluation_matches_basis_functions() {
        for closed in [false, true] {
            let spline = Spline { deg: 3, closed, ctrl: vec![[0.0, 0.0], [1.0, 2.0], [3.0, 2.5], [4.0, 0.0], [5.0, -1.0], [6.0, 1.0]] };
            let (cp, knots) = (spline.cp(), spline.knots());
            let (t0, t1) = spline.domain();
            for k in 0..=20 {
                let t = t0 + (t1 - t0) * k as f64 / 20.0;
                let s = span_of(&knots, t, 3, cp.len());
                let b = basis(&knots, s, t, 3);
                assert!((b.iter().sum::<f64>() - 1.0).abs() < 1e-12);
                let mut p = [0.0; 2];
                for (j, w) in b.iter().enumerate() {
                    p[0] += w * cp[s - 3 + j][0];
                    p[1] += w * cp[s - 3 + j][1];
                }
                let q = eval_at(&cp, &knots, 3, t);
                assert!(dist2(p, q) < 1e-12, "{closed} {t} {p:?} {q:?}");
            }
        }
    }

    #[test]
    fn a_circle_fits_closed_and_the_scad_reproduces_it() {
        let (state, _root) = testkit::state();
        let value = run(&state, json!({"points": circle(10.0, 200), "closed": true, "tolerance": 0.01, "image": false}));
        let n = value["fit"]["controlPointsUsed"].as_u64().unwrap();
        assert!(n <= 12, "{}", value["fit"]);
        assert_eq!(value["fit"]["withinTolerance"], json!(true), "{}", value["fit"]);
        assert!(value["comparison"]["samplesToCurve"]["max"].as_f64().unwrap() < 0.012, "{}", value["comparison"]);
        assert_eq!(value["validated"]["ok"], json!(true), "{}", value["validated"]);
        let area = value["validated"]["area"].as_f64().unwrap();
        assert!((area - std::f64::consts::PI * 100.0).abs() < std::f64::consts::PI, "{area}");
        assert!(value["validated"]["curveDeviation"].as_f64().unwrap() <= 1e-6);
        assert!(value["unsupportedSpans"].as_array().unwrap().is_empty(), "{}", value["unsupportedSpans"]);
    }

    #[test]
    fn isolated_outliers_are_rejected_and_reported() {
        let (state, _root) = testkit::state();
        let mut points: Vec<Value> = (0..150)
            .map(|i| {
                let a = i as f64 / 149.0 * 2.0;
                let noise = ((i * 7919) % 13) as f64 / 13.0 * 0.02 - 0.01;
                json!([(20.0 + noise) * a.cos(), (20.0 + noise) * a.sin()])
            })
            .collect();
        for (k, i) in [30usize, 75, 120].iter().enumerate() {
            let a = *i as f64 / 149.0 * 2.0;
            points[*i] = json!([(23.0 + k as f64) * a.cos(), (23.0 + k as f64) * a.sin()]);
        }
        let value = run(&state, json!({"points": points, "tolerance": 0.05, "image": false}));
        assert_eq!(value["rejected"]["count"], json!(3), "{}", value["rejected"]);
        assert_eq!(value["fit"]["withinTolerance"], json!(true), "{}", value["fit"]);
        // The independent comparison still sees the outliers.
        assert!(value["comparison"]["samplesToCurve"]["max"].as_f64().unwrap() > 2.5, "{}", value["comparison"]);
        assert!(value["rejected"]["points"][0]["distance"].as_f64().unwrap() > 2.5);
    }

    #[test]
    fn a_cylinder_section_fits_and_matches_the_surface() {
        let (state, _root) = testkit::state();
        let id = state.workspaces.create("cylinder(r = 10, h = 10, $fn = 96);\n".into(), None).unwrap().id;
        let result = testkit::call(&state, "fit_curve", json!({
            "designId": id,
            "section": {"surface": "object-1", "plane": {"axis": "z", "offset": 5}},
            "tolerance": 0.05,
            "maxControlPoints": 40
        }));
        assert_eq!(result["isError"], json!(false), "{result}");
        let value = &result["structuredContent"];
        assert_eq!(value["fit"]["closed"], json!(true), "{}", value["fit"]);
        assert_eq!(value["fit"]["withinTolerance"], json!(true), "{}", value["fit"]);
        assert!(value["comparison"]["curveToSurface"]["max"].as_f64().unwrap() < 0.05, "{}", value["comparison"]);
        assert_eq!(value["validated"]["ok"], json!(true), "{}", value["validated"]);
        assert!(result["content"].as_array().unwrap().iter().any(|c| c["type"] == "image"));
        assert!(value["plane"]["placeMatrix"].is_array());
    }

    #[test]
    fn a_gap_in_the_data_is_reported_unsupported() {
        let (state, _root) = testkit::state();
        let points: Vec<Value> = (0..=100)
            .filter(|i| !(40..60).contains(i))
            .map(|i| {
                let x = i as f64 * 0.5;
                json!([x, 3.0 * (x / 8.0).sin()])
            })
            .collect();
        let value = run(&state, json!({"points": points, "tolerance": 0.05, "image": false}));
        let spans = value["unsupportedSpans"].as_array().unwrap();
        assert_eq!(spans.len(), 1, "{}", value["unsupportedSpans"]);
        let from = spans[0]["from2d"][0].as_f64().unwrap();
        let to = spans[0]["to2d"][0].as_f64().unwrap();
        assert!(from > 19.5 && to < 30.5 && to - from > 7.0, "{}", spans[0]);
        assert!(spans[0]["maxGap"].as_f64().unwrap() > 4.0);
    }

    #[test]
    fn a_thickness_band_validates_and_flags_folds() {
        let (state, _root) = testkit::state();
        let points: Vec<Value> = (0..=80).map(|i| {
            let a = i as f64 / 80.0 * std::f64::consts::PI;
            json!([10.0 * a.cos(), 10.0 * a.sin()])
        }).collect();
        // Band on the outside of the arc (right of the counter-clockwise run): no fold.
        let value = run(&state, json!({"points": points, "tolerance": 0.02, "thickness": {"value": 2, "side": "right"}, "image": false}));
        assert_eq!(value["validated"]["ok"], json!(true), "{}", value["validated"]);
        assert_eq!(value["thickness"]["folds"], json!(false), "{}", value["thickness"]);
        let area = value["validated"]["area"].as_f64().unwrap();
        let expected = std::f64::consts::PI / 2.0 * (144.0 - 100.0);
        assert!((area - expected).abs() < 0.02 * expected, "{area} vs {expected}");
        assert!(value["scad"].as_str().unwrap().contains("function fit_band(n)"));
        // 12 mm inward on a 10 mm radius folds the band.
        let folded = run(&state, json!({"points": points, "tolerance": 0.02, "thickness": {"value": 12, "side": "left"}, "image": false}));
        assert_eq!(folded["thickness"]["folds"], json!(true), "{}", folded["thickness"]);
        let conflict = &folded["thickness"]["offsetConflicts"][0];
        assert!((conflict["minRadius"].as_f64().unwrap() - 10.0).abs() < 0.5, "{conflict}");
        // Closed curve with a band.
        let ring = run(&state, json!({"points": circle(10.0, 120), "closed": true, "tolerance": 0.02, "thickness": {"value": 1.234567, "side": "both"}, "image": false, "prefix": "wall"}));
        assert_eq!(ring["validated"]["ok"], json!(true), "{}", ring["validated"]);
        let ring_area = ring["validated"]["area"].as_f64().unwrap();
        let expected = std::f64::consts::PI * (10.6173f64.powi(2) - 9.3827f64.powi(2));
        assert!((ring_area - expected).abs() < 0.02 * expected, "{ring_area} vs {expected}");
        assert!(ring["scad"].as_str().unwrap().contains("module wall_profile()"));
    }

    #[test]
    fn a_steep_rib_is_kept_not_rejected() {
        let (state, _root) = testkit::state();
        // A 1 mm wide, 2 mm tall rib on a 100 mm line.
        let points: Vec<Value> = (0..=1000)
            .map(|i| {
                let x = i as f64 * 0.1;
                json!([x, if (50.0..51.0).contains(&x) { 2.0 } else { 0.0 }])
            })
            .collect();
        let value = run(&state, json!({"points": points, "tolerance": 0.05, "maxControlPoints": 12, "image": false}));
        assert_eq!(value["rejected"]["count"], json!(0), "{}", value["rejected"]);
        assert_eq!(value["fit"]["withinTolerance"], json!(false), "{}", value["fit"]);
    }

    #[test]
    fn chains_join_at_either_end_and_duplicates_are_merged() {
        let line = |x0: f64, x1: f64| -> Vec<V2> { (0..=10).map(|i| [x0 + (x1 - x0) * i as f64 / 10.0, 0.0]).collect() };
        // B (0..1), A (1.1..5, longest), C (5.1..6): A first, then both ends.
        let joined = join_chains(vec![(line(0.0, 1.0), false), (line(1.1, 5.0), false), (line(5.1, 6.0), false)], Instant::now() + BUDGET).unwrap();
        let starts: Vec<f64> = joined.iter().map(|c| c[0][0]).collect();
        assert_eq!(starts, vec![0.0, 1.1, 5.1], "{starts:?}");
        let (state, _root) = testkit::state();
        let id = state.workspaces.create("cube([10, 10, 10]);\n".into(), None).unwrap().id;
        let mut many = vec![json!(0); 5000];
        many.push(json!(0));
        let value = run(&state, json!({"designId": id, "section": {"surface": "object-1", "plane": {"axis": "z", "offset": 9.95}, "chain": many}, "tolerance": 0.3, "maxControlPoints": 6, "image": false}));
        assert_eq!(value["source"]["chainsUsed"], json!([0]));
        // Measured to the side walls the plane crosses (as far as the curve
        // really is from them), not to the top face 0.05 away.
        let to_surface = value["comparison"]["curveToSurface"]["max"].as_f64().unwrap();
        let to_samples = value["comparison"]["curveToSamples"]["max"].as_f64().unwrap();
        assert!(to_surface > 0.1 && (to_surface - to_samples).abs() < 0.01, "{}", value["comparison"]);
    }

    #[test]
    fn bad_arguments_are_refused_before_any_work() {
        let (state, _root) = testkit::state();
        assert!(fit_curve_value(&state, &json!({"points": [[0, 0], [1e17, 1], [2, 0]]})).unwrap_err().contains("10,000,000"));
        assert!(fit_curve_value(&state, &json!({"points": [[0, 0], [1, 1], [2, 0]], "prefix": "1bad"})).unwrap_err().contains("prefix"));
        assert!(fit_curve_value(&state, &json!({})).unwrap_err().contains("section"));
        assert!(fit_curve_value(&state, &json!({"points": [[0, 0], [1, 1], [2, 0]], "closed": true, "maxControlPoints": 4})).unwrap_err().contains("at least 5"));
        let image = fit_curve_value(&state, &json!({"points": circle(5.0, 60), "closed": true}));
        let (_, png) = image.unwrap();
        assert_eq!(&png.unwrap().1[1..4], b"PNG");
    }
}
