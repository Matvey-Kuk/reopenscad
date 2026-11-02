//! The `compare_sections` MCP tool: cut a reference surface (typically a
//! scan) and a candidate (the model) with the same
//! planes and measure the two contours against each other in 2D, both ways,
//! with open scan contours kept open (their gaps are absences of evidence,
//! not geometry), and draw every plane at one shared millimetre scale so
//! the overlays are directly comparable.

use crate::vec3::{cross as cross3, dot as dot3, sub as sub3};
use crate::meshanalysis::{self, MeshIndex, Work};
use crate::plot2d::{self, Canvas};
use crate::scancommon::{self, design_tolerance, Design, Stats, Surface};
use crate::AppState;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

type P2 = [f64; 2];
type P3 = [f64; 3];

/// Wall-clock budget of one call.
const BUDGET: Duration = Duration::from_secs(60);
const MAX_PLANES: usize = 24;
/// Most resampled points per side and plane.
const MAX_SAMPLES: usize = 20_000;
const REFERENCE_INK: [u8; 4] = [25, 60, 170, 255];
const CANDIDATE_INK: [u8; 4] = [235, 130, 20, 255];
const DEVIATION_INK: [u8; 4] = [215, 25, 25, 255];
const UNSUPPORTED_INK: [u8; 4] = [140, 140, 140, 255];

pub(crate) fn tool_definitions() -> Vec<Value> {
    let vec3 = json!({"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3});
    let surface = scancommon::surface_schema();
    let plane = json!({"type": "object", "properties": {
        "axis": {"type": "string", "enum": ["x", "y", "z"]},
        "offset": {"type": "number"},
        "point": vec3.clone(),
        "normal": vec3.clone()
    }});
    vec![crate::mcp_tool(
        "compare_sections",
        "Cut a reference (scan or objects) and a candidate (the model) with the same planes and measure \
         the contours against each other in 2D, mm, both ways, with no solid booleans; open contours stay \
         open. planes: up to 24, or sweep {axis | point+normal, from, to, count}. within {min, max} keeps \
         both contours to one box; a plane with nothing inside is not scored. maxDistance is capped at 50 \
         mm and 10% of the plane's extent (maxDistanceUsed). Reference points with no candidate within \
         maxDistance are uncovered; candidate points past the end of an open reference chain are \
         unsupported (neither is in the stats). Other far candidate points score at true distance, so \
         extra geometry fails. Deviation counts along the other contour's normal between same-facing \
         surfaces; a candidate face behind the scan and turned away is hidden (not scored); opposite \
         facing is facingMismatchLength (scored, failing above 2%). Signed against closed loops of closed \
         meshes (+ = the model is too small there), else unsigned. passes: rms <= tolerance, p95 <= 2 x \
         tolerance both ways, coverage >= 0.9. Plane basis as in section_profile. image: one contact \
         sheet at one shared scale: reference dark blue, candidate orange, unsupported dashed grey, red \
         lines beyond tolerance, numbered worst points.",
        json!({
            "type": "object",
            "required": ["designId", "reference", "candidate"],
            "properties": {
                "designId": {"type": "string"},
                "within": scancommon::within_schema(scancommon::SURFACE_FRAME),
                "reference": surface.clone(),
                "candidate": surface,
                "planes": {"type": "array", "minItems": 1, "maxItems": MAX_PLANES, "items": plane},
                "sweep": {"type": "object", "required": ["from", "to", "count"], "properties": {
                    "axis": {"type": "string", "enum": ["x", "y", "z"]},
                    "point": vec3.clone(),
                    "normal": vec3,
                    "from": {"type": "number"},
                    "to": {"type": "number"},
                    "count": {"type": "integer", "minimum": 2, "maximum": MAX_PLANES}
                }},
                "tolerance": {"type": "number", "minimum": 0, "description": "mm; default the design's."},
                "maxDistance": {"type": "number", "exclusiveMinimum": 0, "default": 2},
                "sectionTolerance": {"type": "number", "minimum": 0, "default": 0.02, "description": "Contour simplification, mm; clamped to tolerance/4."},
                "markers": {"type": "integer", "minimum": 0, "maximum": 20, "default": 5},
                "image": {"description": "true (default), false, or {width, height (per tile), columns}.", "anyOf": [{"type": "boolean"}, {"type": "object"}]},
                "detail": scancommon::detail_schema()
            }
        }),
    )]
}

pub(crate) fn mcp_dispatch(state: &AppState, name: &str, arguments: &Value) -> Value {
    if name != "compare_sections" {
        return crate::mcp_tool_error(&format!("Unknown tool {name:?}."));
    }
    match compare_sections(state, arguments) {
        Ok((value, images)) if images.is_empty() => json!({
            "content": [{"type": "text", "text": serde_json::to_string(&value).unwrap_or_default()}],
            "structuredContent": value,
            "isError": false
        }),
        Ok((value, images)) => scancommon::image_result(scancommon::compact(value, arguments), images),
        Err(error) => crate::mcp_tool_error(&error),
    }
}

// ---------------------------------------------------------------------------
// Geometry helpers
// ---------------------------------------------------------------------------

fn unit3(a: P3) -> P3 {
    let n = dot3(a, a).sqrt();
    if n > 0.0 && n.is_finite() {
        [a[0] / n, a[1] / n, a[2] / n]
    } else {
        [0.0; 3]
    }
}
fn cross2(a: P2, b: P2) -> f64 {
    a[0] * b[1] - a[1] * b[0]
}
fn sub2(a: P2, b: P2) -> P2 {
    [a[0] - b[0], a[1] - b[1]]
}
fn len2(a: P2) -> f64 {
    (a[0] * a[0] + a[1] * a[1]).sqrt()
}
fn r2(p: P2) -> [f64; 2] {
    p.map(|v| scancommon::round_to(v, 3))
}

/// The basis `section_within` uses for a plane normal, so every contour of
/// one plane is expressed in the same coordinates whichever side is empty.
fn plane_basis(n: P3) -> (P3, P3) {
    let k = (0..3).min_by(|&i, &j| n[i].abs().total_cmp(&n[j].abs())).unwrap_or(0);
    let mut e = [0.0; 3];
    e[k] = 1.0;
    let d = dot3(e, n);
    let u = unit3([e[0] - n[0] * d, e[1] - n[1] * d, e[2] - n[2] * d]);
    (u, cross3(n, u))
}

#[derive(Clone, Debug)]
struct Plane {
    point: P3,
    normal: P3,
    u: P3,
    v: P3,
    label: String,
}

impl Plane {
    fn new(point: P3, normal: P3, label: String) -> Self {
        let normal = unit3(normal);
        let (u, v) = plane_basis(normal);
        Self { point, normal, u, v, label }
    }

    fn to3(&self, p: P2) -> P3 {
        [
            self.point[0] + self.u[0] * p[0] + self.v[0] * p[1],
            self.point[1] + self.u[1] * p[0] + self.v[1] * p[1],
            self.point[2] + self.u[2] * p[0] + self.v[2] * p[1],
        ]
    }

    fn to2(&self, p: P3) -> P2 {
        let d = sub3(p, self.point);
        [dot3(d, self.u), dot3(d, self.v)]
    }

    fn public(&self) -> Value {
        let (u, v, n, o) = (self.u, self.v, self.normal, self.point);
        let m = [[u[0], v[0], n[0], o[0]], [u[1], v[1], n[1], o[1]], [u[2], v[2], n[2], o[2]], [0.0, 0.0, 0.0, 1.0]];
        json!({
            "label": self.label,
            "point": scancommon::r3(self.point),
            "normal": scancommon::r3(self.normal),
            "origin": scancommon::r3(self.point),
            "u": scancommon::r3(self.u),
            "v": scancommon::r3(self.v),
            "placeMatrix": scancommon::rounded_matrix(&m),
        })
    }
}

fn axis_name(v: P3) -> String {
    for (k, name) in ["x", "y", "z"].iter().enumerate() {
        if (v[k].abs() - 1.0).abs() < 1e-9 {
            return format!("{}{name}", if v[k] > 0.0 { "+" } else { "-" });
        }
    }
    "u".into()
}

fn axis_index(value: &Value, what: &str) -> Result<usize, String> {
    match value.as_str() {
        Some("x" | "X") => Ok(0),
        Some("y" | "Y") => Ok(1),
        Some("z" | "Z") => Ok(2),
        _ => Err(format!("{what} must be \"x\", \"y\" or \"z\".")),
    }
}

fn number_label(value: f64) -> String {
    scancommon::short(value)
}

fn parse_plane(value: &Value, what: &str) -> Result<Plane, String> {
    if !value.is_object() {
        return Err(format!("{what} must be {{axis, offset}} or {{point, normal}}."));
    }
    if let Some(axis) = value.get("axis").filter(|a| !a.is_null()) {
        let k = axis_index(axis, &format!("{what}.axis"))?;
        let offset = scancommon::number(value, "offset", 0.0, -1.0e6, 1.0e6).map_err(|e| format!("{what}.{e}"))?;
        let mut point = [0.0; 3];
        let mut normal = [0.0; 3];
        point[k] = offset;
        normal[k] = 1.0;
        return Ok(Plane::new(point, normal, format!("{}={}", ["x", "y", "z"][k], number_label(offset))));
    }
    let point = scancommon::vec3(value.get("point").unwrap_or(&Value::Null), &format!("{what}.point"))?;
    let normal = scancommon::vec3(value.get("normal").unwrap_or(&Value::Null), &format!("{what}.normal"))?;
    if dot3(normal, normal) < 1e-18 {
        return Err(format!("{what}.normal must be non-zero."));
    }
    let n = unit3(normal);
    Ok(Plane::new(
        point,
        n,
        format!("p=[{},{},{}] n=[{},{},{}]", number_label(point[0]), number_label(point[1]), number_label(point[2]), number_label(n[0]), number_label(n[1]), number_label(n[2])),
    ))
}

fn parse_planes(input: &Value) -> Result<Vec<Plane>, String> {
    match (input.get("planes").filter(|v| !v.is_null()), input.get("sweep").filter(|v| !v.is_null())) {
        (Some(_), Some(_)) => Err("Give planes or sweep, not both.".into()),
        (None, None) => Err("planes (a list of {axis, offset} or {point, normal}) or sweep {axis, from, to, count} is required.".into()),
        (Some(planes), None) => {
            let items = planes
                .as_array()
                .filter(|items| (1..=MAX_PLANES).contains(&items.len()))
                .ok_or(format!("planes must be an array of 1 to {MAX_PLANES} planes."))?;
            items.iter().enumerate().map(|(i, p)| parse_plane(p, &format!("planes[{i}]"))).collect()
        }
        (None, Some(sweep)) => {
            let from = sweep.get("from").and_then(Value::as_f64).filter(|v| v.is_finite()).ok_or("sweep.from must be a number (mm).")?;
            let to = sweep.get("to").and_then(Value::as_f64).filter(|v| v.is_finite()).ok_or("sweep.to must be a number (mm).")?;
            let count = sweep
                .get("count")
                .and_then(Value::as_u64)
                .filter(|c| (2..=MAX_PLANES as u64).contains(c))
                .ok_or(format!("sweep.count must be an integer from 2 to {MAX_PLANES}."))? as usize;
            let (base, normal, axis) = match sweep.get("axis").filter(|a| !a.is_null()) {
                Some(axis) => {
                    let k = axis_index(axis, "sweep.axis")?;
                    let mut normal = [0.0; 3];
                    normal[k] = 1.0;
                    ([0.0; 3], normal, Some(k))
                }
                None => {
                    let point = scancommon::vec3(sweep.get("point").unwrap_or(&Value::Null), "sweep.point")?;
                    let normal = scancommon::vec3(sweep.get("normal").unwrap_or(&Value::Null), "sweep.normal")?;
                    if dot3(normal, normal) < 1e-18 {
                        return Err("sweep.normal must be non-zero.".into());
                    }
                    (point, unit3(normal), None)
                }
            };
            Ok((0..count)
                .map(|i| {
                    let t = from + (to - from) * i as f64 / (count - 1) as f64;
                    let point = [base[0] + normal[0] * t, base[1] + normal[1] * t, base[2] + normal[2] * t];
                    let label = match axis {
                        Some(k) => format!("{}={}", ["x", "y", "z"][k], number_label(t)),
                        None => format!("d={}", number_label(t)),
                    };
                    Plane::new(point, normal, label)
                })
                .collect())
        }
    }
}

// ---------------------------------------------------------------------------
// Contours, samples and the nearest-segment index
// ---------------------------------------------------------------------------

/// Most contour chains kept per side and plane (the longest).
const MAX_CHAINS: usize = 2000;
/// Largest maxDistance honoured, mm (also capped at 10% of the plane's extent).
const MAX_MATCH_DISTANCE: f64 = 50.0;
/// Most pixels of the whole contact sheet.
const MAX_SHEET_PIXELS: f64 = 16.0e6;

#[derive(Clone, Debug)]
struct Chain {
    points: Vec<P2>,
    closed: bool,
    /// The loop's orientation tells material from air (a closed loop cut
    /// from a closed mesh). Loops of an open mesh are closed by nesting
    /// parity only, which is wrong for e.g. a bore scanned from inside.
    signed: bool,
    /// Cumulative arc length at each point (and at the closing point for a
    /// closed chain).
    arc: Vec<f64>,
    length: f64,
}

impl Chain {
    fn new(mut points: Vec<P2>, closed: bool) -> Self {
        points.dedup_by(|a, b| len2(sub2(*a, *b)) < 1e-12);
        if closed && points.len() > 2 && len2(sub2(points[0], points[points.len() - 1])) < 1e-12 {
            points.pop();
        }
        let mut arc = vec![0.0];
        let count = if closed { points.len() } else { points.len().saturating_sub(1) };
        for i in 0..count {
            let next = points[(i + 1) % points.len()];
            let last = *arc.last().unwrap();
            arc.push(last + len2(sub2(next, points[i])));
        }
        let length = *arc.last().unwrap();
        Self { points, closed, signed: closed, arc, length }
    }

    fn segment_count(&self) -> usize {
        if self.points.len() < 2 {
            0
        } else if self.closed {
            self.points.len()
        } else {
            self.points.len() - 1
        }
    }

    fn segment(&self, i: usize) -> (P2, P2) {
        (self.points[i], self.points[(i + 1) % self.points.len()])
    }
}

/// The contours of one surface at one plane, in the plane's basis; returns
/// the chains and how many short ones were dropped over [`MAX_CHAINS`].
fn contours(surface: &Surface, plane: &Plane, tolerance: f64, within: Option<&scancommon::Within>, work: &mut Work) -> Result<(Vec<Chain>, usize), String> {
    let mut section = meshanalysis::section_within(&surface.mesh, plane.point, plane.normal, tolerance, work)
        .map_err(|error| format!("compare_sections stopped while cutting {}: {error}", surface.label))?;
    if let Some(within) = within {
        within.clip_section(&mut section);
    }
    let signed = surface.open_edges == 0;
    let mut chains: Vec<Chain> = section
        .loops
        .iter()
        .map(|path| {
            let points = path
                .points
                .iter()
                .map(|p| {
                    let p3 = [
                        section.origin[0] + section.u[0] * p[0] + section.v[0] * p[1],
                        section.origin[1] + section.u[1] * p[0] + section.v[1] * p[1],
                        section.origin[2] + section.u[2] * p[0] + section.v[2] * p[1],
                    ];
                    plane.to2(p3)
                })
                .collect();
            let mut chain = Chain::new(points, path.closed);
            chain.signed = chain.closed && signed;
            chain
        })
        .filter(|chain| chain.points.len() >= 2 && chain.length > 0.0)
        .collect();
    let mut dropped = 0;
    if chains.len() > MAX_CHAINS {
        chains.sort_by(|a, b| b.length.total_cmp(&a.length));
        dropped = chains.len() - MAX_CHAINS;
        chains.truncate(MAX_CHAINS);
    }
    Ok((chains, dropped))
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    point: P2,
    chain: usize,
    weight: f64,
}

/// Samples every `spacing` along the chains, plus the resampled polyline
/// of each chain (an open chain keeps its true end points), which is what
/// the other side is matched against and what is drawn: its size is capped
/// by the sample budget, unlike the raw section.
fn resample(chains: &[Chain], spacing: f64) -> (Vec<Sample>, Vec<Chain>) {
    let mut out = Vec::new();
    let mut lines = Vec::with_capacity(chains.len());
    for (c, chain) in chains.iter().enumerate() {
        let n = ((chain.length / spacing).ceil() as usize).max(1);
        let step = chain.length / n as f64;
        let mut seg = 0;
        let first = out.len();
        for k in 0..n {
            let s = (k as f64 + 0.5) * step;
            while seg + 1 < chain.segment_count() && chain.arc[seg + 1] < s {
                seg += 1;
            }
            let (a, b) = chain.segment(seg);
            let seg_len = chain.arc[seg + 1] - chain.arc[seg];
            let t = if seg_len > 0.0 { ((s - chain.arc[seg]) / seg_len).clamp(0.0, 1.0) } else { 0.0 };
            out.push(Sample { point: [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t], chain: c, weight: step });
        }
        let points: Vec<P2> = out[first..].iter().map(|s| s.point).collect();
        let mut line = if chain.closed {
            if points.len() >= 3 {
                Chain::new(points, true)
            } else {
                chain.clone()
            }
        } else {
            let mut all = vec![chain.points[0]];
            all.extend(points);
            all.push(chain.points[chain.points.len() - 1]);
            Chain::new(all, false)
        };
        line.signed = chain.signed;
        lines.push(line);
    }
    (out, lines)
}

struct Hit {
    distance: f64,
    nearest: P2,
    chain: usize,
    /// Arc position of the nearest point along its chain.
    arc: f64,
    /// Positive outside the closed loop's material (right of a CCW outer
    /// loop / CW hole); `None` for an open chain or an unsigned loop.
    outside: Option<bool>,
}

struct SegmentIndex<'a> {
    chains: &'a [Chain],
    cell: f64,
    cells: HashMap<(i64, i64), Vec<(u32, u32)>>,
    lo: (i64, i64),
    hi: (i64, i64),
}

impl<'a> SegmentIndex<'a> {
    fn new(chains: &'a [Chain], cell: f64, deadline: Instant) -> Result<Self, String> {
        let mut cells: HashMap<(i64, i64), Vec<(u32, u32)>> = HashMap::new();
        let (mut lo, mut hi) = ((i64::MAX, i64::MAX), (i64::MIN, i64::MIN));
        for (c, chain) in chains.iter().enumerate() {
            crate::meshops::check(Some(deadline))?;
            for i in 0..chain.segment_count() {
                let (a, b) = chain.segment(i);
                let (x0, x1) = ((a[0].min(b[0]) / cell).floor() as i64, (a[0].max(b[0]) / cell).floor() as i64);
                let (y0, y1) = ((a[1].min(b[1]) / cell).floor() as i64, (a[1].max(b[1]) / cell).floor() as i64);
                lo = (lo.0.min(x0), lo.1.min(y0));
                hi = (hi.0.max(x1), hi.1.max(y1));
                // A diagonal segment: only the cells near the line.
                for x in x0..=x1 {
                    for y in y0..=y1 {
                        if (x1 - x0) * (y1 - y0) > 4 {
                            let center = [(x as f64 + 0.5) * cell, (y as f64 + 0.5) * cell];
                            if segment_distance(center, a, b).0 > cell * 0.75 {
                                continue;
                            }
                        }
                        cells.entry((x, y)).or_default().push((c as u32, i as u32));
                    }
                }
            }
        }
        Ok(Self { chains, cell, cells, lo, hi })
    }

    /// Nearest segment point within `radius` (infinite: anywhere), by rings
    /// of cells around `p`, stopping once no farther ring can be closer.
    fn nearest(&self, p: P2, radius: f64) -> Option<Hit> {
        if self.cells.is_empty() {
            return None;
        }
        let (cx, cy) = ((p[0] / self.cell).floor() as i64, (p[1] / self.cell).floor() as i64);
        let reach = (cx - self.lo.0).abs().max((cx - self.hi.0).abs()).max((cy - self.lo.1).abs()).max((cy - self.hi.1).abs());
        let rings = if radius.is_finite() { ((radius / self.cell).ceil() as i64 + 1).min(reach) } else { reach };
        let mut best: Option<(f64, P2, usize, usize, f64)> = None;
        let visit = |x: i64, y: i64, best: &mut Option<(f64, P2, usize, usize, f64)>| {
            let Some(list) = self.cells.get(&(x, y)) else { return };
            for &(c, i) in list {
                let (a, b) = self.chains[c as usize].segment(i as usize);
                let (d, q, t) = segment_distance(p, a, b);
                if d <= radius && best.as_ref().is_none_or(|bst| d < bst.0) {
                    *best = Some((d, q, c as usize, i as usize, t));
                }
            }
        };
        for k in 0..=rings.max(0) {
            if best.as_ref().is_some_and(|b| b.0 <= (k as f64 - 1.0).max(0.0) * self.cell) {
                break;
            }
            if k == 0 {
                visit(cx, cy, &mut best);
                continue;
            }
            for dx in -k..=k {
                visit(cx + dx, cy - k, &mut best);
                visit(cx + dx, cy + k, &mut best);
            }
            for dy in -k + 1..=k - 1 {
                visit(cx - k, cy + dy, &mut best);
                visit(cx + k, cy + dy, &mut best);
            }
        }
        let (distance, nearest, c, i, t) = best?;
        let chain = &self.chains[c];
        let arc = chain.arc[i] + t * (chain.arc[i + 1] - chain.arc[i]);
        let outside = chain.signed.then(|| {
            let (a, b) = chain.segment(i);
            let d1 = sub2(b, a);
            let w = sub2(p, nearest);
            let at_vertex = |t: f64| (t <= 1e-9, t >= 1.0 - 1e-9);
            match at_vertex(t) {
                (false, false) => cross2(d1, w) < 0.0,
                (start, _) => {
                    // Nearest at a vertex: decide with both adjacent segments.
                    let n = chain.points.len();
                    let (prev, next) = if start { ((i + n - 1) % n, i) } else { (i, (i + 1) % n) };
                    let (pa, pb) = chain.segment(prev);
                    let (na, nb) = chain.segment(next);
                    let (d0, d1) = (sub2(pb, pa), sub2(nb, na));
                    let (left0, left1) = (cross2(d0, w) > 0.0, cross2(d1, w) > 0.0);
                    let inside = if cross2(d0, d1) >= 0.0 { left0 && left1 } else { left0 || left1 };
                    !inside
                }
            }
        });
        Some(Hit { distance, nearest, chain: c, arc, outside })
    }
}

/// (distance, closest point, t) from `p` to segment ab.
fn segment_distance(p: P2, a: P2, b: P2) -> (f64, P2, f64) {
    let d = sub2(b, a);
    let l2 = d[0] * d[0] + d[1] * d[1];
    let t = if l2 > 0.0 { (((p[0] - a[0]) * d[0] + (p[1] - a[1]) * d[1]) / l2).clamp(0.0, 1.0) } else { 0.0 };
    let q = [a[0] + d[0] * t, a[1] + d[1] * t];
    (len2(sub2(p, q)), q, t)
}

fn bounds(chains: &[Chain]) -> Option<(P2, P2)> {
    let mut lo = [f64::INFINITY; 2];
    let mut hi = [f64::NEG_INFINITY; 2];
    for p in chains.iter().flat_map(|c| c.points.iter()) {
        for k in 0..2 {
            lo[k] = lo[k].min(p[k]);
            hi[k] = hi[k].max(p[k]);
        }
    }
    lo[0].is_finite().then_some((lo, hi))
}

fn union_bounds(a: Option<(P2, P2)>, b: Option<(P2, P2)>) -> Option<(P2, P2)> {
    match (a, b) {
        (Some(a), Some(b)) => Some(([a.0[0].min(b.0[0]), a.0[1].min(b.0[1])], [a.1[0].max(b.1[0]), a.1[1].max(b.1[1])])),
        (a, None) => a,
        (None, b) => b,
    }
}

fn side_public(chains: &[Chain], dropped: usize) -> Value {
    let closed = chains.iter().filter(|c| c.closed).count();
    let mut value = json!({
        "chains": chains.len(),
        "closed": closed,
        "open": chains.len() - closed,
        "signed": closed > 0 && chains.iter().filter(|c| c.closed).all(|c| c.signed),
        "length": scancommon::round_to(chains.iter().map(|c| c.length).sum(), 3),
    });
    if dropped > 0 {
        value["chainsDropped"] = json!(dropped);
    }
    if let Some((lo, hi)) = bounds(chains) {
        value["bounds2d"] = json!({"min": r2(lo), "max": r2(hi)});
        value["extent"] = json!([scancommon::round_to(hi[0] - lo[0], 3), scancommon::round_to(hi[1] - lo[1], 3)]);
    }
    value
}

// ---------------------------------------------------------------------------
// One direction of the comparison
// ---------------------------------------------------------------------------

/// What happened to one sample.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Fate {
    /// Matched within maxDistance: (signed deviation, nearest point, whether
    /// the sign is meaningful).
    Matched(f64, P2, bool),
    /// Farther than maxDistance from a closed reference loop or the middle
    /// of a chain: scored at its true distance (the model has geometry the
    /// observed contour contradicts).
    Far(f64, P2, bool),
    /// No counterpart within maxDistance (reference side: uncovered).
    Missing,
    /// Its nearest counterpart lies past the end of an open chain: nothing
    /// observed there (candidate side: unsupported).
    PastEnd,
    /// Along the other contour's normal but facing the opposite way: the
    /// wrong side of a wall. Scored, and failing above 2% of the length.
    Mismatch(f64, P2, bool),
    /// Candidate only: behind the scan contour (against its normal) and not
    /// facing its way: a back or inner face the scan cannot see. Excluded
    /// (unsupported).
    Hidden,
}

impl Fate {
    fn measured(&self) -> Option<(f64, P2, bool)> {
        match *self {
            Fate::Matched(d, q, s) | Fate::Far(d, q, s) | Fate::Mismatch(d, q, s) => Some((d, q, s)),
            _ => None,
        }
    }
}

/// cos of the angle within which an offset counts as along a normal.
const ALONG_NORMAL: f64 = 0.866;
/// cos 60 deg: normals closer than this face the same way.
const SAME_FACING: f64 = 0.5;

fn dot2(a: P2, b: P2) -> f64 {
    a[0] * b[0] + a[1] * b[1]
}

fn unit2(a: P2) -> P2 {
    let n = len2(a);
    if n > 0.0 && n.is_finite() {
        [a[0] / n, a[1] / n]
    } else {
        [0.0, 0.0]
    }
}

/// The in-plane normal of `mesh` at plane point `p`: the nearest face's
/// normal projected into (u, v); `None` when that face lies almost in the
/// plane.
fn plane_normal(mesh: &MeshIndex, plane: &Plane, p: P2) -> Option<P2> {
    let (_, _, _, n) = mesh.nearest_face(plane.to3(p))?;
    let projected = [dot3(n, plane.u), dot3(n, plane.v)];
    (len2(projected) > 0.2).then(|| unit2(projected))
}

struct Direction {
    samples: Vec<Sample>,
    fates: Vec<Fate>,
}

struct Matching<'a> {
    fine: &'a SegmentIndex<'a>,
    /// Unbounded search for samples with nothing within maxDistance; `None`
    /// leaves them Missing.
    coarse: Option<&'a SegmentIndex<'a>>,
    max_distance: f64,
    end_slack: f64,
    /// The sign convention flips for candidate samples (inside the
    /// reference = model too small = positive).
    positive_inside: bool,
    plane: &'a Plane,
    /// The 3D meshes of this side and of the other, for normals.
    own: &'a MeshIndex,
    other: &'a MeshIndex,
    /// Samples within this of the other contour touch it whatever the angle.
    touch: f64,
    /// The candidate side: a sample hidden behind the scan is unsupported.
    hidden_allowed: bool,
}

impl Matching<'_> {
    fn signed(&self, hit: &Hit) -> (f64, bool) {
        match hit.outside {
            Some(outside) => {
                let positive = if self.positive_inside { !outside } else { outside };
                (if positive { hit.distance } else { -hit.distance }, true)
            }
            None => (hit.distance, false),
        }
    }

    fn past_end(&self, hit: &Hit, chains: &[Chain]) -> bool {
        let chain = &chains[hit.chain];
        !chain.closed && (hit.arc <= self.end_slack || hit.arc >= chain.length - self.end_slack) && hit.distance > self.end_slack
    }

    /// Classifies one sample as compare_surfaces does, in the plane: a
    /// deviation counts only along the other contour's normal between
    /// surfaces facing the same way; a candidate face behind the scan and
    /// turned away from it is hidden (the scan cannot see it); facing
    /// opposite ways is a wall's wrong side; beside an open chain's end is
    /// unobserved; anything else contradicts the other contour and is
    /// scored at its true distance.
    fn fate(&self, p: P2) -> Fate {
        let found = match self.fine.nearest(p, self.max_distance) {
            Some(hit) => Some((hit, false, self.fine.chains)),
            None => self.coarse.and_then(|coarse| coarse.nearest(p, f64::INFINITY).map(|hit| (hit, true, coarse.chains))),
        };
        let Some((hit, far, chains)) = found else { return Fate::Missing };
        let own_normal = plane_normal(self.own, self.plane, p);
        let other_normal = plane_normal(self.other, self.plane, hit.nearest);
        let offset = sub2(p, hit.nearest);
        let touching = hit.distance <= self.touch;
        let at_end = self.past_end(&hit, chains);
        let along = touching
            || match other_normal {
                Some(n) => dot2(unit2(offset), n).abs() >= ALONG_NORMAL,
                None => !at_end,
            };
        let (same, opposite) = match (own_normal, other_normal) {
            (Some(a), Some(b)) => (dot2(a, b) >= SAME_FACING, dot2(a, b) <= -SAME_FACING),
            _ => (true, false),
        };
        let behind = other_normal.is_some_and(|n| dot2(offset, n) < 0.0);
        let (d, signed) = self.signed(&hit);
        let scored = if far { Fate::Far(d, hit.nearest, signed) } else { Fate::Matched(d, hit.nearest, signed) };
        if along {
            if same {
                return scored;
            }
            if self.hidden_allowed && behind && !touching {
                return Fate::Hidden;
            }
            if opposite {
                return Fate::Mismatch(d, hit.nearest, signed);
            }
            if touching {
                // Meeting at a corner.
                return scored;
            }
        } else if at_end {
            return Fate::PastEnd;
        }
        scored
    }
}

impl Direction {
    fn measure(samples: Vec<Sample>, matching: &Matching, deadline: Instant) -> Result<Self, String> {
        let mut fates = Vec::with_capacity(samples.len());
        for (i, sample) in samples.iter().enumerate() {
            if i % 256 == 0 {
                crate::meshops::check(Some(deadline))?;
            }
            fates.push(matching.fate(sample.point));
        }
        Ok(Self { samples, fates })
    }

    fn matched_values(&self) -> Vec<(f64, f64)> {
        self.samples.iter().zip(&self.fates).filter_map(|(s, f)| f.measured().map(|(d, _, _)| (d, s.weight))).collect()
    }

    fn total(&self) -> f64 {
        self.samples.iter().map(|s| s.weight).sum()
    }

    fn weight_where(&self, keep: impl Fn(&Fate) -> bool) -> f64 {
        self.samples.iter().zip(&self.fates).filter(|(_, f)| keep(f)).map(|(s, _)| s.weight).sum()
    }

    /// Runs of consecutive samples on one chain whose fate passes `keep`
    /// (a run through the start of a closed chain counts once), as
    /// (length, from, to, chain), longest first.
    fn runs(&self, closed: &[bool], keep: impl Fn(&Fate) -> bool) -> Vec<(f64, usize, usize, usize)> {
        // (length, first sample, last sample, chain)
        let mut runs: Vec<(f64, usize, usize, usize)> = Vec::new();
        let mut current: Option<(f64, usize, usize, usize)> = None;
        for (i, (s, f)) in self.samples.iter().zip(&self.fates).enumerate() {
            if keep(f) {
                current = match current {
                    Some((len, from, _, chain)) if chain == s.chain => Some((len + s.weight, from, i, chain)),
                    other => {
                        runs.extend(other);
                        Some((s.weight, i, i, s.chain))
                    }
                };
            } else if let Some(done) = current.take() {
                runs.push(done);
            }
        }
        runs.extend(current);
        // Merge the run ending at a closed chain's last sample into the one
        // starting at its first.
        let mut ranges: HashMap<usize, (usize, usize)> = HashMap::new();
        for (i, s) in self.samples.iter().enumerate() {
            let entry = ranges.entry(s.chain).or_insert((i, i));
            entry.1 = i;
        }
        for (chain, (first, last)) in ranges {
            if !closed.get(chain).copied().unwrap_or(false) {
                continue;
            }
            let head = runs.iter().position(|r| r.3 == chain && r.1 == first);
            let tail = runs.iter().position(|r| r.3 == chain && r.2 == last);
            if let (Some(h), Some(t)) = (head, tail) {
                if h != t {
                    let (head_run, tail_run) = (runs[h], runs[t]);
                    runs[t] = (tail_run.0 + head_run.0, tail_run.1, head_run.2, chain);
                    runs.remove(h);
                }
            }
        }
        runs.sort_by(|a, b| b.0.total_cmp(&a.0));
        runs
    }

    /// Points of a run, in order (wrapping through a closed chain's start).
    fn run_points(&self, run: &(f64, usize, usize, usize)) -> Vec<P2> {
        let (_, from, to, chain) = *run;
        if from <= to {
            return self.samples[from..=to].iter().map(|s| s.point).collect();
        }
        let first = self.samples.iter().position(|s| s.chain == chain).unwrap_or(0);
        let last = self.samples.iter().rposition(|s| s.chain == chain).unwrap_or(0);
        self.samples[from..=last].iter().chain(self.samples[first..=to].iter()).map(|s| s.point).collect()
    }

    fn spans(&self, plane: &Plane, closed: &[bool], keep: impl Fn(&Fate) -> bool) -> Vec<Value> {
        self.runs(closed, keep)
            .into_iter()
            .take(10)
            .map(|(length, from, to, chain)| {
                let (from, to) = (self.samples[from].point, self.samples[to].point);
                json!({
                    "chain": chain,
                    "length": scancommon::round_to(length, 3),
                    "from2d": r2(from),
                    "to2d": r2(to),
                    "from3d": scancommon::r3(plane.to3(from)),
                    "to3d": scancommon::r3(plane.to3(to)),
                })
            })
            .collect()
    }

    fn public(&self, tolerance: f64) -> Value {
        let values = self.matched_values();
        let mut value = Stats::of(&values).public();
        let signed = self.fates.iter().filter(|f| f.measured().is_some_and(|m| m.2)).count();
        let matched = values.len();
        value["signed"] = json!(matched > 0 && signed == matched);
        if matched > 0 && signed > 0 && signed < matched {
            value["signedFraction"] = json!(scancommon::round_to(signed as f64 / matched as f64, 3));
        }
        value["histogram"] = scancommon::histogram(&values, &[tolerance / 2.0, tolerance, 2.0 * tolerance, 4.0 * tolerance]);
        value
    }
}

// ---------------------------------------------------------------------------
// One plane
// ---------------------------------------------------------------------------

struct Worst {
    from: &'static str,
    point: P2,
    nearest: P2,
    distance: f64,
    signed: Option<f64>,
}

struct PlaneResult {
    plane: Plane,
    /// Resampled polylines (what is matched and drawn).
    reference: Vec<Chain>,
    candidate: Vec<Chain>,
    forward: Direction,
    backward: Direction,
    /// Every reference sample at its distance to the candidate contour
    /// (value, length weight): the shape accuracy, summed over planes too.
    shape: Vec<(f64, f64)>,
    worst: Vec<Worst>,
    value: Value,
    passes: Option<bool>,
}

fn compare_plane(
    plane: Plane,
    reference: &Surface,
    candidate: &Surface,
    meshes: (&MeshIndex, &MeshIndex),
    options: &Options,
    work: &mut Work,
    deadline: Instant,
) -> Result<PlaneResult, String> {
    let (ref_chains, ref_dropped) = contours(reference, &plane, options.section_tolerance, options.within.as_ref(), work)?;
    let (cand_chains, cand_dropped) = contours(candidate, &plane, options.section_tolerance, options.within.as_ref(), work)?;
    let extent = union_bounds(bounds(&ref_chains), bounds(&cand_chains))
        .map_or(1.0, |(lo, hi)| (hi[0] - lo[0]).max(hi[1] - lo[1]).max(1e-3));
    let ref_length: f64 = ref_chains.iter().map(|c| c.length).sum();
    let cand_length: f64 = cand_chains.iter().map(|c| c.length).sum();
    let longest = ref_length.max(cand_length);
    let spacing = (0.1f64.min(extent / 400.0)).max(longest / MAX_SAMPLES as f64).max(1e-4);
    let max_distance = options.max_distance.min(MAX_MATCH_DISTANCE).min((0.1 * extent).max(0.5));
    let cell = options.max_distance.min(max_distance).min(extent / 64.0).max(max_distance / 8.0).max(spacing / 4.0);
    let coarse_cell = (extent / 64.0).max(spacing).max(cell);
    let end_slack = (spacing * 0.5).max(1e-6);
    let (ref_samples, ref_lines) = resample(&ref_chains, spacing);
    let (cand_samples, cand_lines) = resample(&cand_chains, spacing);
    let ref_fine = SegmentIndex::new(&ref_lines, cell, deadline)?;
    let ref_coarse = SegmentIndex::new(&ref_lines, coarse_cell, deadline)?;
    let cand_fine = SegmentIndex::new(&cand_lines, cell, deadline)?;
    let cand_coarse = SegmentIndex::new(&cand_lines, coarse_cell, deadline)?;
    let forward = Direction::measure(
        ref_samples,
        &Matching {
            fine: &cand_fine,
            coarse: None,
            max_distance,
            end_slack,
            positive_inside: false,
            plane: &plane,
            own: meshes.0,
            other: meshes.1,
            touch: options.tolerance / 2.0,
            hidden_allowed: false,
        },
        deadline,
    )?;
    // A candidate point with no reference near it is scored at its true
    // distance, unless the nearest reference geometry is the end of an
    // open chain: then the scan simply did not see there (unsupported).
    let backward = Direction::measure(
        cand_samples,
        &Matching {
            fine: &ref_fine,
            coarse: Some(&ref_coarse),
            max_distance,
            end_slack,
            positive_inside: true,
            plane: &plane,
            own: meshes.1,
            other: meshes.0,
            touch: options.tolerance / 2.0,
            hidden_allowed: true,
        },
        deadline,
    )?;

    let tolerance = options.tolerance;
    let ref_total = forward.total();
    let covered = forward.weight_where(|f| f.measured().is_some());
    let coverage = if ref_total > 0.0 { covered / ref_total } else { 0.0 };
    let cand_total = backward.total();
    let supported = backward.weight_where(|f| f.measured().is_some());
    let far = backward.weight_where(|f| matches!(f, Fate::Far(..)));
    let hidden = backward.weight_where(|f| matches!(f, Fate::Hidden));
    let forward_mismatch = forward.weight_where(|f| matches!(f, Fate::Mismatch(..)));
    let backward_mismatch = backward.weight_where(|f| matches!(f, Fate::Mismatch(..)));
    let mismatch_ok = forward_mismatch <= 0.02 * ref_total && backward_mismatch <= 0.02 * cand_total;
    let forward_stats = Stats::of(&forward.matched_values());
    let backward_stats = Stats::of(&backward.matched_values());

    // Worst points: both directions, beyond tolerance, spread apart.
    let mut candidates: Vec<Worst> = Vec::new();
    for (direction, from) in [(&forward, "reference"), (&backward, "candidate")] {
        for (s, f) in direction.samples.iter().zip(&direction.fates) {
            if let Some((d, q, signed)) = f.measured() {
                if d.abs() > tolerance {
                    candidates.push(Worst { from, point: s.point, nearest: q, distance: d.abs(), signed: signed.then_some(d) });
                }
            }
        }
    }
    candidates.sort_by(|a, b| b.distance.total_cmp(&a.distance));
    let separation = (5.0 * spacing).max(extent / 20.0);
    let mut worst: Vec<Worst> = Vec::new();
    for item in candidates {
        if worst.len() >= options.markers {
            break;
        }
        if worst.iter().all(|w| len2(sub2(w.point, item.point)) >= separation) {
            worst.push(item);
        }
    }

    let mut value = json!({
        "plane": plane.public(),
        "reference": side_public(&ref_chains, ref_dropped),
        "candidate": side_public(&cand_chains, cand_dropped),
    });
    let ref_closed: Vec<bool> = ref_lines.iter().map(|c| c.closed).collect();
    let cand_closed: Vec<bool> = cand_lines.iter().map(|c| c.closed).collect();
    let mut forward_value = forward.public(tolerance);
    forward_value["coverage"] = json!(scancommon::round_to(coverage, 4));
    forward_value["uncoveredLength"] = json!(scancommon::round_to(ref_total - covered, 3));
    forward_value["facingMismatchLength"] = json!(scancommon::round_to(forward_mismatch, 3));
    let uncovered = forward.spans(&plane, &ref_closed, |f| f.measured().is_none());
    if !uncovered.is_empty() {
        forward_value["uncoveredSpans"] = json!(uncovered);
    }
    let mut backward_value = backward.public(tolerance);
    backward_value["supportedFraction"] = json!(if cand_total > 0.0 { scancommon::round_to(supported / cand_total, 4) } else { 0.0 });
    backward_value["unsupportedLength"] = json!(scancommon::round_to(cand_total - supported, 3));
    backward_value["beyondMaxDistanceLength"] = json!(scancommon::round_to(far, 3));
    backward_value["hiddenLength"] = json!(scancommon::round_to(hidden, 3));
    backward_value["facingMismatchLength"] = json!(scancommon::round_to(backward_mismatch, 3));
    let unsupported = backward.spans(&plane, &cand_closed, |f| f.measured().is_none());
    if !unsupported.is_empty() {
        backward_value["unsupportedSpans"] = json!(unsupported);
    }
    // Shape accuracy over the whole reference contour: every point at its
    // real distance to the candidate contour, the uncovered ones included.
    let shape_values = shape_values(&forward, &cand_coarse, deadline)?;
    let shape = Stats::of(&shape_values);
    let mut shape_value = shape.public();
    shape_value["withinTolerance"] = json!(scancommon::round_to(
        shape_values.iter().filter(|(d, _)| d.abs() <= tolerance).map(|(_, w)| w).sum::<f64>() / ref_total.max(1e-12),
        4
    ));
    shape_value["missingLength"] = json!(scancommon::round_to(ref_total - shape.area, 3));
    value["shapeAccuracy"] = shape_value;
    value["referenceToCandidate"] = forward_value;
    value["candidateToReference"] = backward_value;
    value["worst"] = json!(worst
        .iter()
        .enumerate()
        .map(|(i, w)| json!({
            "n": i + 1,
            "from": w.from,
            "point2d": r2(w.point),
            "point3d": scancommon::r3(plane.to3(w.point)),
            "nearest2d": r2(w.nearest),
            "nearest3d": scancommon::r3(plane.to3(w.nearest)),
            "distance": scancommon::round_to(w.distance, 4),
            "signed": w.signed.map(|s| scancommon::round_to(s, 4)),
        }))
        .collect::<Vec<_>>());
    value["sampleSpacing"] = json!(scancommon::round_to(spacing, 5));
    value["maxDistanceUsed"] = json!(scancommon::round_to(max_distance, 4));
    value["hiddenLength"] = json!(scancommon::round_to(hidden, 3));

    let passes = match (ref_chains.is_empty(), cand_chains.is_empty()) {
        (true, true) => {
            value["note"] = json!("Neither surface reaches this plane.");
            None
        }
        (true, false) => {
            value["note"] = json!("The reference has no section at this plane: nothing observed to compare the candidate with.");
            None
        }
        (false, true) => {
            value["note"] = json!("The candidate has no section at this plane: the reference contour is not reproduced at all.");
            Some(false)
        }
        (false, false) => {
            if !mismatch_ok {
                value["note"] = json!("The contours face opposite ways over more than 2% of their length: the model has the wrong side of a wall here.");
            } else if supported <= 0.0 {
                value["note"] = json!("Every candidate point lies past the ends of the open reference contour: nothing observed to score it against.");
            } else if far > 0.0 {
                value["note"] = json!(format!(
                    "{} mm of candidate contour lies farther than maxDistance from the closed reference contour; it is scored at its true distance.",
                    scancommon::short(far)
                ));
            }
            Some(
                forward_stats.count > 0
                    && forward_stats.rms <= tolerance
                    && forward_stats.p95 <= 2.0 * tolerance
                    && coverage >= 0.9
                    && (backward_stats.count == 0 || (backward_stats.rms <= tolerance && backward_stats.p95 <= 2.0 * tolerance))
                    && mismatch_ok,
            )
        }
    };
    value["passes"] = passes.map_or(Value::Null, Value::Bool);
    Ok(PlaneResult { plane, reference: ref_lines, candidate: cand_lines, forward, backward, shape: shape_values, worst, value, passes })
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn draw_tile(
    result: &PlaneResult,
    index: usize,
    width: u32,
    height: u32,
    px_per_mm: f64,
    tolerance: f64,
    deadline: Instant,
) -> Result<Canvas, String> {
    let center = union_bounds(bounds(&result.reference), bounds(&result.candidate))
        .map_or([0.0, 0.0], |(lo, hi)| [(lo[0] + hi[0]) * 0.5, (lo[1] + hi[1]) * 0.5]);
    let mut canvas = Canvas::new(width, height, center, px_per_mm);
    let s = canvas.text_scale as f64;
    canvas.grid((&axis_name(result.plane.u), &axis_name(result.plane.v)));

    for chain in &result.candidate {
        crate::meshops::check(Some(deadline))?;
        canvas.polyline(&chain.points, chain.closed, 2.0 * s, CANDIDATE_INK);
    }
    // Unsupported candidate stretches, dashed grey over the orange.
    let cand_closed: Vec<bool> = result.candidate.iter().map(|c| c.closed).collect();
    for run in result.backward.runs(&cand_closed, |f| f.measured().is_none()) {
        crate::meshops::check(Some(deadline))?;
        let points = result.backward.run_points(&run);
        if points.len() >= 2 {
            canvas.dashed(&points, false, 2.2 * s, 6.0 * s, UNSUPPORTED_INK);
        }
    }

    for chain in &result.reference {
        crate::meshops::check(Some(deadline))?;
        canvas.polyline(&chain.points, chain.closed, 2.0 * s, REFERENCE_INK);
        if !chain.closed {
            for end in [chain.points[0], chain.points[chain.points.len() - 1]] {
                canvas.dot(end, 3.5 * s, REFERENCE_INK);
            }
        }
    }
    // Uncovered reference stretches (no candidate near them), red.
    let ref_closed: Vec<bool> = result.reference.iter().map(|c| c.closed).collect();
    for run in result.forward.runs(&ref_closed, |f| f.measured().is_none()) {
        crate::meshops::check(Some(deadline))?;
        let points = result.forward.run_points(&run);
        if points.len() >= 2 {
            canvas.polyline(&points, false, 3.0 * s, DEVIATION_INK);
        } else if let Some(p) = points.first() {
            canvas.dot(*p, 2.0 * s, DEVIATION_INK);
        }
    }
    for chain in result.candidate.iter().filter(|c| !c.closed) {
        for end in [chain.points[0], chain.points[chain.points.len() - 1]] {
            canvas.dot(end, 3.0 * s, CANDIDATE_INK);
        }
    }

    // Deviation lines from samples beyond tolerance, subsampled.
    let beyond: Vec<(P2, P2)> = [&result.forward, &result.backward]
        .iter()
        .flat_map(|d| d.samples.iter().zip(&d.fates))
        .filter_map(|(sample, fate)| fate.measured().filter(|m| m.0.abs() > tolerance).map(|m| (sample.point, m.1)))
        .collect();
    let stride = beyond.len().div_ceil(160).max(1);
    for (p, q) in beyond.iter().step_by(stride) {
        let (a, b) = (canvas.to_px(*p), canvas.to_px(*q));
        canvas.line_px(a, b, 1.3 * s, DEVIATION_INK);
    }
    crate::meshops::check(Some(deadline))?;

    // Reference extent dimensions.
    if let Some((lo, hi)) = bounds(&result.reference) {
        let ink = [70, 70, 110, 255];
        if hi[0] - lo[0] > 1e-6 {
            canvas.dimension([lo[0], lo[1]], [hi[0], lo[1]], [0.0, 16.0 * s], ink);
        }
        if hi[1] - lo[1] > 1e-6 {
            canvas.dimension([lo[0], lo[1]], [lo[0], hi[1]], [-16.0 * s, 0.0], ink);
        }
    }

    for (i, w) in result.worst.iter().enumerate() {
        canvas.ring(w.point, 7.0 * s, 1.8 * s, DEVIATION_INK);
        let c = canvas.to_px(w.point);
        canvas.text_px(c[0] + 10.0 * s, c[1] - 4.0 * s, &format!("{}", i + 1), DEVIATION_INK);
    }

    // Only which plane this tile is; its numbers and verdict are in planes[]
    // of the reply, never painted over the contours.
    let ink = match result.passes {
        Some(true) => [30, 140, 60, 255],
        Some(false) => DEVIATION_INK,
        None => UNSUPPORTED_INK,
    };
    canvas.text_px(6.0 * s / 2.0 + 4.0, 5.0, &format!("{}  {}", index + 1, result.plane.label), ink);
    Ok(canvas)
}

// ---------------------------------------------------------------------------
// The tool
// ---------------------------------------------------------------------------

struct Options {
    within: Option<scancommon::Within>,
    tolerance: f64,
    max_distance: f64,
    section_tolerance: f64,
    markers: usize,
}

/// Every reference sample with its distance to the candidate contour:
/// the measured distance where there is one, else the nearest point of the
/// candidate contour however far (an indexed search). With no candidate
/// contour the samples have no distance and are left out (missing length).
fn shape_values(forward: &Direction, candidate: &SegmentIndex, deadline: Instant) -> Result<Vec<(f64, f64)>, String> {
    let mut out = Vec::with_capacity(forward.samples.len());
    for (i, (s, f)) in forward.samples.iter().zip(&forward.fates).enumerate() {
        if i % 1024 == 0 {
            crate::meshops::check(Some(deadline))?;
        }
        match f.measured() {
            Some((d, _, _)) => out.push((d, s.weight)),
            None => {
                if let Some(hit) = candidate.nearest(s.point, f64::INFINITY) {
                    out.push((hit.distance, s.weight));
                }
            }
        }
    }
    Ok(out)
}

pub(crate) fn compare_sections(state: &AppState, input: &Value) -> Result<(Value, Vec<(String, Vec<u8>)>), String> {
    let started = Instant::now();
    let deadline = started + BUDGET;
    let _scope = crate::csg::bsp::deadline_scope(deadline);
    let planes = parse_planes(input)?;
    let tolerance = match input.get("tolerance") {
        None | Some(Value::Null) => design_tolerance(state, input),
        Some(_) => scancommon::number(input, "tolerance", 0.2, 0.0, 100.0)?,
    };
    // Simplifying the contours coarser than a quarter of the tolerance
    // would erase the very deviation being measured.
    let section_requested = scancommon::number(input, "sectionTolerance", 0.02, 0.0, 10.0)?;
    let section_tolerance = section_requested.min(tolerance / 4.0);
    let options = Options {
        within: scancommon::Within::parse(input)?,
        tolerance,
        max_distance: scancommon::number(input, "maxDistance", 2.0, 1e-4, 1000.0)?,
        section_tolerance,
        markers: scancommon::integer(input, "markers", 5, 0, 20)?,
    };
    let image = scancommon::image_wanted(input, true)?;
    let reference_spec = input.get("reference").ok_or("reference is required: the observed surface (a scan or objects).")?;
    let candidate_spec = input.get("candidate").ok_or("candidate is required: the model surface (object IDs).")?;
    let mut design = Design::new(state, input);
    let reference = design.surface(reference_spec, "reference")?;
    let candidate = design.surface(candidate_spec, "candidate")?;

    let reference_index = reference.index();
    let candidate_index = candidate.index();
    let mut work = Work::new(meshanalysis::ANALYSIS_WORK);
    let mut results: Vec<PlaneResult> = Vec::new();
    let mut skipped = Vec::new();
    for plane in planes {
        if Instant::now() >= deadline {
            skipped.push(json!({"plane": plane.public(), "reason": "time budget spent"}));
            continue;
        }
        match compare_plane(plane.clone(), &reference, &candidate, (&reference_index, &candidate_index), &options, &mut work, deadline) {
            Ok(result) => results.push(result),
            Err(error) if Instant::now() >= deadline || error.contains("deadline") || error.contains("budget") => {
                skipped.push(json!({"plane": plane.public(), "reason": error}));
            }
            Err(error) => return Err(error),
        }
    }

    // One scale for every tile: the largest plane extent fits with margin.
    // The whole sheet stays within MAX_SHEET_PIXELS.
    let (tile_w, tile_h, columns) = {
        let opts = image.clone().unwrap_or(json!({}));
        let many = results.len() > 6;
        let mut w = scancommon::integer(&opts, "width", if many { 480 } else { 640 }, 200, 2000)? as f64;
        let mut h = scancommon::integer(&opts, "height", if many { 360 } else { 480 }, 150, 2000)? as f64;
        let total = results.len().max(1) as f64 * w * h;
        if total > MAX_SHEET_PIXELS {
            let shrink = (MAX_SHEET_PIXELS / total).sqrt();
            w = (w * shrink).floor().max(160.0);
            h = (h * shrink).floor().max(120.0);
        }
        let auto = match results.len() {
            0 | 1 => 1,
            2..=4 => 2,
            _ => 3,
        };
        (w as u32, h as u32, scancommon::integer(&opts, "columns", auto, 1, 6)?)
    };
    let margin = 90.0 * if tile_w >= 700 { 2.0 } else { 1.0 };
    let px_per_mm = results
        .iter()
        .filter_map(|r| union_bounds(bounds(&r.reference), bounds(&r.candidate)))
        .map(|(lo, hi)| {
            let (w, h) = ((hi[0] - lo[0]).max(1e-3), (hi[1] - lo[1]).max(1e-3));
            ((tile_w as f64 - margin).max(50.0) / w).min((tile_h as f64 - margin).max(50.0) / h)
        })
        .fold(f64::INFINITY, f64::min);
    let px_per_mm = if px_per_mm.is_finite() && px_per_mm > 0.0 { px_per_mm } else { 10.0 };

    // Overall, length-weighted over every plane.
    let shape_all: Vec<(f64, f64)> = results.iter().flat_map(|r| r.shape.iter().copied()).collect();
    let mut forward_all: Vec<(f64, f64)> = Vec::new();
    let mut backward_all: Vec<(f64, f64)> = Vec::new();
    let (mut ref_total, mut covered, mut cand_total, mut supported) = (0.0, 0.0, 0.0, 0.0);
    for r in &results {
        forward_all.extend(r.forward.matched_values());
        backward_all.extend(r.backward.matched_values());
        ref_total += r.forward.total();
        covered += r.forward.weight_where(|f| f.measured().is_some());
        cand_total += r.backward.total();
        supported += r.backward.weight_where(|f| f.measured().is_some());
    }
    let worst_plane = results
        .iter()
        .enumerate()
        .filter(|(_, r)| r.passes == Some(false) || r.forward.matched_values().iter().any(|v| v.0 != 0.0))
        .max_by(|(_, a), (_, b)| {
            let key = |r: &PlaneResult| if r.passes == Some(false) && r.forward.matched_values().is_empty() { f64::INFINITY } else { Stats::of(&r.forward.matched_values()).p95.max(Stats::of(&r.backward.matched_values()).p95) };
            key(a).total_cmp(&key(b))
        })
        .map(|(i, _)| i);
    let passing = results.iter().filter(|r| r.passes == Some(true)).count();
    let judged = results.iter().filter(|r| r.passes.is_some()).count();
    let overall_forward = Stats::of(&forward_all);
    let overall_backward = Stats::of(&backward_all);
    let overall_coverage = if ref_total > 0.0 { covered / ref_total } else { 0.0 };
    let mut verdict = format!(
        "{passing} of {judged} compared planes pass at tolerance {} mm (rms <= tol and p95 <= 2 x tol both ways, coverage >= 90%). \
         Overall reference->candidate rms {} p95 {} max {} mm over {}% of the reference contour length; candidate->reference \
         rms {} max {} mm over {}% of the candidate contour (the rest lies past the ends of open reference contours).",
        scancommon::short(tolerance),
        scancommon::short(overall_forward.rms),
        scancommon::short(overall_forward.p95),
        scancommon::short(overall_forward.max),
        (overall_coverage * 100.0).round(),
        scancommon::short(overall_backward.rms),
        scancommon::short(overall_backward.max),
        (if cand_total > 0.0 { supported / cand_total * 100.0 } else { 0.0 }).round(),
    );
    if let Some(i) = worst_plane.filter(|i| results[*i].passes == Some(false)) {
        verdict.push_str(&format!(" Worst: plane {} ({}).", i + 1, results[i].plane.label));
    }
    if judged == 0 {
        verdict = if options.within.is_some() {
            "Nothing inside the box: no plane cuts both surfaces within it, so nothing was compared.".into()
        } else {
            "No plane cuts both surfaces: nothing was compared.".into()
        };
    }

    let mut value = design.header();
    value["ok"] = json!(true);
    value["tolerance"] = json!(tolerance);
    value["maxDistance"] = json!(options.max_distance);
    if results.iter().any(|r| r.value["maxDistanceUsed"].as_f64().is_some_and(|used| used < options.max_distance)) {
        value["maxDistanceClamped"] = json!(format!(
            "maxDistance is capped at {MAX_MATCH_DISTANCE} mm and at 10% of each plane's extent (at least 0.5 mm); see maxDistanceUsed per plane."
        ));
    }
    value["sectionTolerance"] = json!({
        "requested": section_requested,
        "used": scancommon::round_to(section_tolerance, 6),
        "clamped": section_tolerance < section_requested,
    });
    value["reference"] = reference.brief();
    value["candidate"] = candidate.brief();
    if let Some(within) = &options.within {
        let length = |side: &str| -> f64 { results.iter().filter_map(|r| r.value[side]["length"].as_f64()).sum() };
        value["within"] = within.public(json!({"referenceLength": scancommon::round_to(length("reference"), 3), "candidateLength": scancommon::round_to(length("candidate"), 3)}));
        if judged == 0 {
            value["nothingInside"] = json!(true);
        }
    }
    value["planes"] = json!(results.iter().map(|r| r.value.clone()).collect::<Vec<_>>());
    value["summary"] = json!({
        "planes": results.len(),
        "compared": judged,
        "passing": passing,
        "worstPlane": worst_plane.map(|i| i + 1),
        "shapeAccuracy": {
            "stats": Stats::of(&shape_all).public(),
            "withinTolerance": scancommon::round_to(shape_all.iter().filter(|(d, _)| d.abs() <= tolerance).map(|(_, w)| w).sum::<f64>() / ref_total.max(1e-12), 4),
            "note": "Every reference contour point at its real distance to the candidate contour (uncovered points included; a plane where the candidate has no contour counts as missing length): the accuracy of the whole section, not just where it already matches.",
        },
        "referenceToCandidate": {
            "stats": overall_forward.public(),
            "coverage": scancommon::round_to(overall_coverage, 4),
        },
        "candidateToReference": {
            "stats": overall_backward.public(),
            "supportedFraction": if cand_total > 0.0 { scancommon::round_to(supported / cand_total, 4) } else { 0.0 },
        },
    });
    value["verdict"] = json!(verdict);
    value["scale"] = json!({"pxPerMm": scancommon::round_to(px_per_mm, 4), "mmPerPx": scancommon::round_to(1.0 / px_per_mm, 5)});
    let messages = design.messages();
    let warnings: Vec<&String> = messages.iter().filter(|m| m.contains("WARNING") || m.contains("ERROR")).collect();
    value["console"] = json!(messages);
    value["warnings"] = json!(warnings);

    let mut images = Vec::new();
    if image.is_some() && !results.is_empty() {
        let mut tiles: Vec<Canvas> = Vec::with_capacity(results.len());
        for (i, r) in results.iter().enumerate() {
            match draw_tile(r, i, tile_w, tile_h, px_per_mm, tolerance, deadline) {
                Ok(tile) => tiles.push(tile),
                Err(error) => {
                    skipped.push(json!({"image": format!("tiles from plane {} on were not drawn: {error}", i + 1)}));
                    break;
                }
            }
        }
        if !tiles.is_empty() {
            let (png, width, height, origins) = plot2d::sheet(&tiles, columns);
            value["image"] = json!({
                "mimeType": "image/png",
                "bytes": png.len(),
                "width": width,
                "height": height,
                "tile": {"width": tile_w, "height": tile_h, "columns": columns.min(tiles.len())},
                "tiles": origins.iter().enumerate().map(|(i, o)| json!({"plane": i + 1, "x": o[0], "y": o[1], "center2d": r2(tiles[i].center)})).collect::<Vec<_>>(),
            });
            let caption = format!(
                "compare_sections: {} plane(s), every tile at one scale of {} px/mm ({} mm/px); blue = reference, orange = \
                 candidate, dashed grey = unsupported or hidden candidate, red = deviation > {} mm or uncovered reference, numbered \
                 rings = planes[n].worst[k-1]. Each tile names its plane (number and position, green = passes, red = fails); \
                 the numbers are in planes[].",
                tiles.len(),
                scancommon::short(px_per_mm),
                scancommon::short(1.0 / px_per_mm),
                scancommon::short(tolerance)
            );
            images.push((caption, png));
        }
    }
    if !skipped.is_empty() {
        value["complete"] = json!(false);
        value["skipped"] = json!(skipped);
    }
    value["elapsedMs"] = json!(started.elapsed().as_millis() as u64);
    Ok((value, images))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        JobLimiter, RateLimiter, RenderQueue, Request, Store, WorkspaceStore, MAX_CONCURRENT_JOBS, MAX_CONCURRENT_POLLS,
        MAX_QUEUED_JOBS, MAX_QUEUED_PER_CLIENT, MAX_QUEUE_WAIT,
    };
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    fn state() -> (AppState, PathBuf) {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let root = std::env::temp_dir().join(format!("reopenscad-sections-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
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

    fn request() -> Request {
        Request { method: "POST".into(), path: "/mcp".into(), query: String::new(), headers: vec![("host".into(), "127.0.0.1:5173".into())], body: vec![] }
    }

    fn call(state: &AppState, arguments: Value) -> Value {
        crate::mcp_call_tool(state, &request(), "compare_sections", arguments)
    }

    fn ids(state: &AppState, code: &str) -> (String, Vec<String>) {
        let id = state.workspaces.create(code.into(), None).unwrap().id;
        let objects = crate::agent::objects_value(state, None, &json!({"designId": id})).unwrap();
        let list = objects["objects"].as_array().unwrap().iter().map(|o| o["id"].as_str().unwrap().to_string()).collect();
        (id, list)
    }

    #[test]
    fn shape_accuracy_measures_a_contour_far_beyond_max_distance() {
        let (state, root) = crate::scancommon::testkit::state();
        let id = state.workspaces.create("cube(20);\ntranslate([30, 0, 0]) cube(20);\n".into(), None).unwrap().id;
        let started = Instant::now();
        let reply = crate::scancommon::testkit::call(&state, "compare_sections", json!({"designId": id, "reference": "object-1", "candidate": "object-2", "planes": [{"axis": "z", "offset": 10}], "image": false}));
        assert_eq!(reply["isError"], json!(false), "{reply}");
        let plane = &reply["structuredContent"]["planes"][0];
        // Nothing is within maxDistance, yet every point has its real distance
        // to the candidate square (10 mm at the facing side, 30 at the far one).
        assert_eq!(plane["referenceToCandidate"]["coverage"], json!(0.0), "{plane}");
        let shape = &plane["shapeAccuracy"];
        assert!((shape["max"].as_f64().unwrap() - 30.0).abs() < 1e-3 && shape["p50"].as_f64().unwrap() > 9.0, "{shape}");
        assert_eq!(reply["structuredContent"]["summary"]["shapeAccuracy"]["stats"]["max"], shape["max"]);
        assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn identical_cubes_match_and_offset_cylinders_fail() {
        let (state, root) = state();
        let (id, objects) = ids(&state, "cube(20);\ntranslate([0, 0, 0]) cube(20);\n");
        let result = call(&state, json!({"designId": id, "reference": objects[0], "candidate": objects[1], "planes": [{"axis": "z", "offset": 10}], "image": false}));
        let value = &result["structuredContent"];
        assert_eq!(value["ok"], json!(true), "{value}");
        let plane = &value["planes"][0];
        assert_eq!(plane["passes"], json!(true), "{plane}");
        assert!(plane["referenceToCandidate"]["max"].as_f64().unwrap() < 1e-6, "{plane}");
        assert_eq!(plane["referenceToCandidate"]["coverage"], json!(1.0));
        assert_eq!(plane["reference"]["closed"], json!(1));
        assert!((plane["reference"]["extent"][0].as_f64().unwrap() - 20.0).abs() < 1e-6);

        let (id, objects) = ids(&state, "cylinder(r = 10, h = 20, $fn = 128);\ntranslate([0, 0, 0]) cylinder(r = 10.5, h = 20, $fn = 128);\n");
        let result = call(&state, json!({"designId": id, "reference": objects[0], "candidate": objects[1], "sweep": {"axis": "z", "from": 5, "to": 15, "count": 3}, "tolerance": 0.2}));
        let value = &result["structuredContent"];
        assert_eq!(value["ok"], json!(true), "{value}");
        assert_eq!(value["summary"]["passing"], json!(0), "{value}");
        let forward = &value["planes"][1]["referenceToCandidate"];
        assert!((forward["rms"].as_f64().unwrap() - 0.5).abs() < 0.02, "{forward}");
        // Reference inside the candidate: the model is too large (negative).
        assert!(forward["signedMean"].as_f64().unwrap() < -0.45, "{forward}");
        assert!(value["planes"][1]["worst"].as_array().unwrap().len() >= 1);
        // One image, with tiles at one shared scale.
        let content = result["content"].as_array().unwrap();
        let images: Vec<&Value> = content.iter().filter(|c| c["type"] == "image").collect();
        assert_eq!(images.len(), 1);
        assert_eq!(value["image"]["tiles"].as_array().unwrap().len(), 3);
        assert!(value["scale"]["pxPerMm"].as_f64().unwrap() > 0.0);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn an_open_reference_stays_open_and_leaves_the_rest_unsupported() {
        let (state, root) = state();
        // Reference: the +x half of a tube wall only (an open shell, like a
        // one-sided scan); candidate: the whole cylinder.
        let code = "difference() { cylinder(r = 10, h = 20, $fn = 96); translate([-20, -20, -1]) cube([20, 40, 22]); }\n\
                    translate([0, 0, 0]) cylinder(r = 10, h = 20, $fn = 96);\n";
        let (id, objects) = ids(&state, code);
        // Cut the reference solid into an open surface: keep only its curved face
        // by exporting it as an asset subset is not available here, so compare
        // against an asset built from the open half-shell.
        let mesh = {
            let mut points = Vec::new();
            let mut triangles = Vec::new();
            let n = 48;
            for i in 0..=n {
                let a = -std::f64::consts::FRAC_PI_2 + std::f64::consts::PI * i as f64 / n as f64;
                points.push([10.0 * a.cos(), 10.0 * a.sin(), 0.0]);
                points.push([10.0 * a.cos(), 10.0 * a.sin(), 20.0]);
            }
            for i in 0..n as u32 {
                let (a, b, c, d) = (2 * i, 2 * i + 1, 2 * i + 2, 2 * i + 3);
                triangles.push([a, c, d]);
                triangles.push([a, d, b]);
            }
            crate::assets::TriMesh { points, triangles }
        };
        crate::scancommon::testkit::upload_with(&state, &id, "half.stl", &mesh, json!({}));
        let result = call(&state, json!({"designId": id, "reference": {"asset": "half.stl"}, "candidate": objects[1], "planes": [{"axis": "z", "offset": 10}], "image": true}));
        let value = &result["structuredContent"];
        assert_eq!(value["ok"], json!(true), "{value}");
        let plane = &value["planes"][0];
        assert_eq!(plane["reference"]["open"], json!(1), "{plane}");
        assert_eq!(plane["reference"]["closed"], json!(0), "{plane}");
        let forward = &plane["referenceToCandidate"];
        assert!(forward["max"].as_f64().unwrap() < 0.05, "{forward}");
        assert_eq!(forward["coverage"], json!(1.0), "{forward}");
        let backward = &plane["candidateToReference"];
        let supported = backward["supportedFraction"].as_f64().unwrap();
        assert!((supported - 0.5).abs() < 0.08, "{backward}");
        assert!(backward["max"].as_f64().unwrap() <= 0.1 + 1e-9, "the unsupported half is not scored: {backward}");
        assert!(backward["unsupportedSpans"].as_array().is_some_and(|s| !s.is_empty()), "{backward}");
        assert_eq!(plane["passes"], json!(true), "{plane}");
        assert!(result["content"].as_array().unwrap().iter().any(|c| c["type"] == "image"));
        let _ = objects;
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn signs_follow_the_closed_loop_material() {
        let chain = Chain::new(vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]], true);
        let chains = vec![chain];
        let deadline = Instant::now() + Duration::from_secs(10);
        let index = SegmentIndex::new(&chains, 2.0, deadline).unwrap();
        // Unbounded search walks out to the far side.
        assert!((index.nearest([5.0, 5.0], f64::INFINITY).unwrap().distance - 5.0).abs() < 1e-12);
        assert_eq!(index.nearest([5.0, -1.0], 2.0).unwrap().outside, Some(true));
        assert_eq!(index.nearest([5.0, 1.0], 2.0).unwrap().outside, Some(false));
        // At a convex corner, outside diagonally.
        assert_eq!(index.nearest([11.0, 11.0], 2.0).unwrap().outside, Some(true));
        assert!(index.nearest([5.0, 5.0], 2.0).is_none());
        let (samples, lines) = resample(&chains, 0.5);
        assert_eq!(samples.len(), 80);
        assert!(lines[0].closed && lines[0].points.len() == 80);
        // A run through the start of a closed chain is one span.
        let direction = Direction {
            fates: (0..80).map(|i| if !(3..77).contains(&i) { Fate::Missing } else { Fate::Matched(0.0, [0.0, 0.0], true) }).collect(),
            samples,
        };
        let runs = direction.runs(&[true], |f| f.measured().is_none());
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert!((runs[0].0 - 3.0).abs() < 1e-9);
        assert_eq!(direction.run_points(&runs[0]).len(), 6);
        let samples = direction.samples;
        assert!((samples.iter().map(|s| s.weight).sum::<f64>() - 40.0).abs() < 1e-9);
        let planes = parse_planes(&json!({"sweep": {"point": [0, 0, 0], "normal": [0, 0, 2], "from": 1, "to": 3, "count": 3}})).unwrap();
        assert_eq!(planes.len(), 3);
        assert!((planes[2].point[2] - 3.0).abs() < 1e-12);
        assert_eq!(planes[0].u, [1.0, 0.0, 0.0]);
        assert_eq!(planes[0].v, [0.0, 1.0, 0.0]);
    }
    #[test]
    fn extra_model_geometry_off_a_closed_reference_fails() {
        let (state, root) = state();
        // The model has a 3 mm tab the 20 x 20 reference square lacks.
        let code = "cube([20, 20, 10]);\nunion() { cube([20, 20, 10]); translate([20, 5, 0]) cube([3, 10, 10]); }\n";
        let (id, objects) = ids(&state, code);
        let result = call(&state, json!({"designId": id, "reference": objects[0], "candidate": objects[1], "planes": [{"axis": "z", "offset": 5}], "tolerance": 0.2, "image": false}));
        let value = &result["structuredContent"];
        assert_eq!(value["ok"], json!(true), "{value}");
        let plane = &value["planes"][0];
        assert_eq!(plane["passes"], json!(false), "{plane}");
        let backward = &plane["candidateToReference"];
        assert!((backward["max"].as_f64().unwrap() - 3.0).abs() < 0.05, "{backward}");
        assert!(backward["beyondMaxDistanceLength"].as_f64().unwrap() > 5.0, "{backward}");
        assert_eq!(backward["supportedFraction"], json!(1.0), "{backward}");
        // The tab sticks out of the reference: the model is too large there.
        assert!(plane["worst"][0]["signed"].as_f64().unwrap() < -2.9, "{plane}");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn loops_cut_from_an_open_mesh_are_unsigned() {
        let (state, root) = state();
        let (id, objects) = ids(&state, "difference() { translate([-20, -20, 0]) cube([40, 40, 20]); translate([0, 0, -1]) cylinder(r = 10.5, h = 22, $fn = 128); }\n");
        // A bore scanned from inside: a tube wall with no caps (open mesh,
        // closed section loops).
        let mut points = Vec::new();
        let mut triangles = Vec::new();
        let n = 96u32;
        for i in 0..n {
            let a = std::f64::consts::TAU * i as f64 / n as f64;
            points.push([10.0 * a.cos(), 10.0 * a.sin(), 0.0]);
            points.push([10.0 * a.cos(), 10.0 * a.sin(), 20.0]);
        }
        for i in 0..n {
            let (a, b, c, d) = (2 * i, 2 * i + 1, (2 * i + 2) % (2 * n), (2 * i + 3) % (2 * n));
            triangles.push([a, d, c]);
            triangles.push([a, b, d]);
        }
        crate::scancommon::testkit::upload_with(&state, &id, "bore.stl", &crate::assets::TriMesh { points, triangles }, json!({}));
        let result = call(&state, json!({"designId": id, "reference": {"asset": "bore.stl"}, "candidate": objects[0], "planes": [{"axis": "z", "offset": 10}], "image": false}));
        let plane = &result["structuredContent"]["planes"][0];
        assert_eq!(plane["reference"]["closed"], json!(1), "{plane}");
        assert_eq!(plane["reference"]["signed"], json!(false), "{plane}");
        assert_eq!(plane["candidateToReference"]["signed"], json!(false), "{plane}");
        assert_eq!(plane["referenceToCandidate"]["signed"], json!(true), "{plane}");
        assert!((plane["candidateToReference"]["rms"].as_f64().unwrap() - 0.5).abs() < 0.02, "{plane}");
        // The block's outer faces lie behind the bore scan: hidden, not scored.
        assert!(plane["hiddenLength"].as_f64().unwrap() > 150.0, "{plane}");
        // The bore is too large: material missing, positive.
        assert!(plane["referenceToCandidate"]["signedMean"].as_f64().unwrap() > 0.45, "{plane}");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn huge_tiles_are_shrunk_to_the_sheet_budget_and_section_tolerance_is_clamped() {
        let (state, root) = state();
        let (id, objects) = ids(&state, "cube(20);\ntranslate([0, 0, 0]) cube(20);\n");
        let result = call(&state, json!({"designId": id, "reference": objects[0], "candidate": objects[1], "sweep": {"axis": "z", "from": 1, "to": 19, "count": 24}, "sectionTolerance": 5, "tolerance": 0.2, "image": {"width": 2000, "height": 2000}}));
        let value = &result["structuredContent"];
        assert_eq!(value["ok"], json!(true), "{value}");
        let (w, h) = (value["image"]["width"].as_f64().unwrap(), value["image"]["height"].as_f64().unwrap());
        assert!(w * h <= 16.5e6, "{w} x {h}");
        assert_eq!(value["sectionTolerance"]["clamped"], json!(true));
        assert_eq!(value["sectionTolerance"]["used"], json!(0.05));
        assert_eq!(value["summary"]["passing"], json!(24), "{value}");
        std::fs::remove_dir_all(root).ok();
    }
    #[test]
    fn a_shell_behind_a_one_sided_scan_is_hidden_not_wrong() {
        let (state, root) = state();
        // A 2 mm plate whose top face coincides with a scanned top face.
        let (id, objects) = ids(&state, "translate([0, 0, -2]) cube([20, 20, 2]);\n");
        let quad = crate::assets::TriMesh {
            points: vec![[0.0, 0.0, 0.0], [20.0, 0.0, 0.0], [20.0, 20.0, 0.0], [0.0, 20.0, 0.0]],
            triangles: vec![[0, 1, 2], [0, 2, 3]],
        };
        crate::scancommon::testkit::upload(&state, &id, "top.stl", &quad);
        let result = call(&state, json!({"designId": id, "reference": {"asset": "top.stl"}, "candidate": objects[0], "planes": [{"axis": "x", "offset": 10}], "tolerance": 0.2, "image": false}));
        let value = &result["structuredContent"];
        assert_eq!(value["ok"], json!(true), "{value}");
        let plane = &value["planes"][0];
        assert_eq!(plane["reference"]["open"], json!(1), "{plane}");
        assert_eq!(plane["passes"], json!(true), "{plane}");
        assert!(plane["hiddenLength"].as_f64().unwrap() > 20.0, "the underside is hidden: {plane}");
        assert!(plane["candidateToReference"]["max"].as_f64().unwrap() <= 0.1 + 1e-9, "{plane}");
        assert_eq!(plane["candidateToReference"]["facingMismatchLength"], json!(0.0), "{plane}");
        assert_eq!(plane["referenceToCandidate"]["coverage"], json!(1.0), "{plane}");
        std::fs::remove_dir_all(root).ok();
    }
}
