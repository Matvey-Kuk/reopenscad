//! Bottom-up printability (support) analysis.
//!
//! The mesh is sliced at mid-layer heights `bed + (i + 0.5) * layer_height`
//! and each cross-section is rasterized onto a 2D grid. A pixel of layer `i`
//! is supported when layer `i - 1` has material within the overhang reach
//! `layer_height * tan(max_overhang_deg)`; every pixel of the first layer is
//! supported by the bed. Unsupported pixels are grouped into 8-connected
//! components and classified by the supported parts of the same layer they
//! touch ("anchors"):
//!
//! * no anchor: an **island** that starts in midair,
//! * one anchor: a **cantilever**, `span` = geodesic reach from the anchor,
//! * two or more anchors: a **bridge**, `span` = twice the largest geodesic
//!   distance from any unsupported pixel to its nearest anchor (the length of
//!   a straight bridge between two anchors; an upper bound for odd shapes).
//!
//! Anchors are separate contact arcs, and an arc resting on two separate
//! pieces of the layer below counts once per piece: a slab joining two feet
//! that stand 0.5 mm apart bridges between them, even though the contacts on
//! either side of the narrow gap touch and form one arc.
//!
//! Sub-line-width steps: a perimeter can hang about half its width past the
//! line it sits on. So a short steep stretch is printable when (1) each layer
//! steps out at most `reach + line_width / 2` past the one below, so the outer
//! perimeter still rests half on it, and (2) measured from each of the
//! layers below (enough of them that a sustained slope more than
//! `ENVELOPE_TOLERANCE_DEG` past the limit fails), the stretch drifts at most `1.5 * line_width`
//! past the overhang-limit envelope `k * reach`, so the next perimeter in still
//! rests half within it and holds the outer one. Both distances get half a
//! pixel for raster centres. Thread flanks (60 degrees, ending after a few
//! layers) pass; a sustained steeper slope fails (2) a few layers in, and a
//! ledge wider than half a line fails (1) at once. A component of unsupported
//! pixels whose every pixel passes is not reported.
//!
//! The rasterizer uses the nonzero winding rule, so overlapping closed shells
//! (for example a union given as concatenated box meshes) fill as their union.
//! Consecutive layers' unsupported regions that lie within reach of each other
//! are reported as one issue. Islands are tracked upward through a union-find
//! of material components across layers; `joins_at` is the first layer where
//! the island's material connects to material rooted at the bed. Joining later
//! never excuses the unsupported start: the island is reported either way.

use crate::analysis::P;
use std::collections::BinaryHeap;

#[derive(Clone, Debug, PartialEq)]
pub struct PrintProfile {
    /// Slice thickness, mm.
    pub layer_height: f64,
    /// Extrusion width, mm; also sets the grid resolution (half of it).
    pub line_width: f64,
    /// Measured from vertical: a wall leaning up to this angle needs no support.
    pub max_overhang_deg: f64,
    /// Longest unsupported span between two anchors that prints without support, mm.
    pub max_bridge: f64,
    /// Bed height; `None` = lowest Z of the triangles given.
    pub bed_z: Option<f64>,
}

impl Default for PrintProfile {
    fn default() -> Self {
        PrintProfile { layer_height: 0.2, line_width: 0.42, max_overhang_deg: 45.0, max_bridge: 10.0, bed_z: None }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SupportIssue {
    /// "island" | "cantilever" | "bridge"
    pub kind: &'static str,
    /// True only for a bridge whose span <= max_bridge.
    pub ok: bool,
    /// 0-based layer index where it starts.
    pub first_layer: usize,
    /// Mid-layer height of the first layer.
    pub z: f64,
    /// Mid-layer height of the last layer of this grouped issue.
    pub z_top: f64,
    /// Bounds of the unsupported region over all its layers (z uses mid-layer heights).
    pub min: [f64; 3],
    pub max: [f64; 3],
    /// Centroid of the first layer's unsupported area.
    pub location: [f64; 3],
    /// Unsupported area in the first layer, mm^2.
    pub area: f64,
    /// Bridge: distance between anchors; cantilever: reach from its anchor (max over its layers).
    pub span: Option<f64>,
    /// Anchor points (bridge: 2+, cantilever: 1, island: 0); at most `MAX_ANCHORS`.
    pub anchors: Vec<[f64; 3]>,
    /// Island only: z where it first connects to geometry supported from the bed.
    pub joins_at: Option<f64>,
    /// One plain sentence.
    pub reason: String,
    /// Number of layers this issue spans.
    pub layer_count: usize,
    /// Indices of the parts whose material occupies the unsupported pixels, by pixel count, largest first.
    pub parts: Vec<usize>,
}

/// Issue counts over all issues found, before truncation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SupportTotals {
    pub islands: usize,
    pub cantilevers: usize,
    /// All bridges, including the ones too long.
    pub bridges: usize,
    pub bridges_too_long: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SupportReport {
    /// The profile used, with `bed_z` resolved to `Some`.
    pub profile: PrintProfile,
    pub layers: usize,
    /// Sorted by z, then area descending; at most `MAX_SUPPORT_ISSUES`.
    pub issues: Vec<SupportIssue>,
    /// Issues dropped beyond `MAX_SUPPORT_ISSUES`.
    pub truncated: usize,
    /// False if the request deadline cut the analysis short.
    pub complete: bool,
    /// Grid pixel size used, mm.
    pub resolution: f64,
    /// Counts over all issues, including truncated ones.
    pub totals: SupportTotals,
    /// Honest caveats: resolution changes, budgets hit, limits applied.
    pub notes: Vec<String>,
}

pub const MAX_SUPPORT_ISSUES: usize = 50;
/// Anchor points listed per issue.
pub const MAX_ANCHORS: usize = 8;
/// Grid size cap per layer.
const MAX_PIXELS: usize = 4_000_000;
const NONE: u32 = u32::MAX;
/// Layers analyzed at most.
const MAX_LAYERS: usize = 50_000;
/// Largest support search radius, in pixels; the grid is coarsened beyond it.
const MAX_RADIUS_PX: f64 = 64.0;
/// Union-find nodes (material components over all layers).
const MAX_UF_NODES: usize = 8_000_000;
/// Neighbourhood probes over the whole analysis.
const MAX_PROBES: u64 = 3_000_000_000;
/// Issues kept in full detail while analyzing; later ones are only counted.
const MAX_RETAINED_ISSUES: usize = 20_000;
/// The sub-line-width step test (see the module docs) looks down enough layers
/// that a sustained slope this much steeper than the limit still fails it
/// (17 layers at the default 0.2 mm / 45 degrees, 0.42 mm lines).
const ENVELOPE_TOLERANCE_DEG: f64 = 5.0;
/// Cap on the layers it looks down (history is kept as one bit per pixel).
const MAX_ENVELOPE_LAYERS: usize = 48;

// Pixel states.
const EMPTY: u8 = 0;
const SUPPORTED: u8 = 1;
const UNSUPPORTED: u8 = 2;
/// Unsupported and attributed to a reported issue.
const ISSUE: u8 = 3;
/// Unsupported but too small or thin to matter; still material for the layer above.
const NOISE: u8 = 4;

fn sanitize(profile: &PrintProfile) -> PrintProfile {
    let d = PrintProfile::default();
    let pick = |v: f64, def: f64| if v.is_finite() && v > 0.0 { v } else { def };
    PrintProfile {
        layer_height: pick(profile.layer_height, d.layer_height),
        line_width: pick(profile.line_width, d.line_width),
        max_overhang_deg: if profile.max_overhang_deg.is_finite() {
            profile.max_overhang_deg.clamp(0.0, 89.0)
        } else {
            d.max_overhang_deg
        },
        max_bridge: if profile.max_bridge.is_finite() { profile.max_bridge.max(0.0) } else { d.max_bridge },
        bed_z: profile.bed_z.filter(|z| z.is_finite()),
    }
}

struct Grid {
    ox: f64,
    oy: f64,
    px: f64,
    nx: usize,
    ny: usize,
}

impl Grid {
    fn center(&self, idx: usize) -> (f64, f64) {
        let (i, j) = (idx % self.nx, idx / self.nx);
        (self.ox + (i as f64 + 0.5) * self.px, self.oy + (j as f64 + 0.5) * self.px)
    }
}

/// Inclusive pixel bounding box.
#[derive(Clone, Copy)]
struct BBox {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
}

impl BBox {
    const EMPTY: BBox = BBox { x0: usize::MAX, y0: usize::MAX, x1: 0, y1: 0 };
    fn is_empty(&self) -> bool {
        self.x0 > self.x1
    }
    fn add(&mut self, i: usize, j: usize) {
        self.x0 = self.x0.min(i);
        self.x1 = self.x1.max(i);
        self.y0 = self.y0.min(j);
        self.y1 = self.y1.max(j);
    }
}

struct UnionFind {
    parent: Vec<u32>,
    rooted: Vec<bool>,
}

impl UnionFind {
    fn push(&mut self, rooted: bool) -> u32 {
        self.parent.push(self.parent.len() as u32);
        self.rooted.push(rooted);
        (self.parent.len() - 1) as u32
    }
    fn find(&mut self, mut a: u32) -> u32 {
        while self.parent[a as usize] != a {
            let gp = self.parent[self.parent[a as usize] as usize];
            self.parent[a as usize] = gp;
            a = gp;
        }
        a
    }
    fn union(&mut self, a: u32, b: u32) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[rb as usize] = ra;
            let r = self.rooted[ra as usize] || self.rooted[rb as usize];
            self.rooted[ra as usize] = r;
        }
    }
    fn is_rooted(&mut self, a: u32) -> bool {
        let r = self.find(a);
        self.rooted[r as usize]
    }
}

/// Disk offsets (dx, dy) with dx^2 + dy^2 <= r^2 (r in pixels), nearest first.
fn disk(r: f64) -> Vec<(i64, i64)> {
    let ri = r.max(0.0).floor() as i64;
    let mut v = Vec::new();
    for dy in -ri..=ri {
        for dx in -ri..=ri {
            if ((dx * dx + dy * dy) as f64) <= r * r + 1e-9 {
                v.push((dx, dy));
            }
        }
    }
    v.sort_by_key(|&(dx, dy)| dx * dx + dy * dy);
    v
}

const NEIGH8: [(i64, i64); 8] = [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)];

/// Rasterize the cross-section at height `z` into `out` (nonzero winding).
fn slice_layer(
    tris: &[[P; 3]],
    active: &[usize],
    z: f64,
    g: &Grid,
    rows: &mut [Vec<(f64, i32)>],
    out: &mut [u8],
    mut segs: Option<&mut Vec<[f64; 4]>>,
) -> BBox {
    let mut touched: Vec<usize> = Vec::new();
    for &t in active {
        let v = &tris[t];
        let d = [v[0][2] - z, v[1][2] - z, v[2][2] - z];
        let above = [d[0] >= 0.0, d[1] >= 0.0, d[2] >= 0.0];
        if above[0] == above[1] && above[1] == above[2] {
            continue;
        }
        let mut pts = [[0.0f64; 2]; 2];
        let mut n = 0;
        for (a, b) in [(0, 1), (1, 2), (2, 0)] {
            if above[a] != above[b] && n < 2 {
                // Interpolate from a canonical endpoint so the two triangles sharing
                // this edge produce bit-identical points; otherwise a scanline row
                // through the shared point can count one crossing twice or not at all.
                let (a, b) = if v[a] <= v[b] { (a, b) } else { (b, a) };
                let s = d[a] / (d[a] - d[b]);
                pts[n] = [v[a][0] + (v[b][0] - v[a][0]) * s, v[a][1] + (v[b][1] - v[a][1]) * s];
                n += 1;
            }
        }
        if n < 2 {
            continue;
        }
        let e1 = [v[1][0] - v[0][0], v[1][1] - v[0][1], v[1][2] - v[0][2]];
        let e2 = [v[2][0] - v[0][0], v[2][1] - v[0][1], v[2][2] - v[0][2]];
        let nx = e1[1] * e2[2] - e1[2] * e2[1];
        let ny = e1[2] * e2[0] - e1[0] * e2[2];
        let (mut p, mut q) = (pts[0], pts[1]);
        // Outward normal must lie to the right of the segment direction (CCW loops).
        if (q[0] - p[0]) * ny - (q[1] - p[1]) * nx > 0.0 {
            std::mem::swap(&mut p, &mut q);
        }
        if let Some(segs) = segs.as_deref_mut() {
            segs.push([p[0], p[1], q[0], q[1]]);
        }
        if p[1] == q[1] {
            continue;
        }
        let sign = if q[1] > p[1] { 1 } else { -1 };
        let (lo, hi) = if p[1] < q[1] { (p, q) } else { (q, p) };
        // Rows whose center yc satisfies lo.y <= yc < hi.y.
        let j0 = ((lo[1] - g.oy) / g.px - 0.5).ceil().max(0.0) as usize;
        let j1 = (((hi[1] - g.oy) / g.px - 0.5).ceil().max(0.0) as usize).min(g.ny);
        for (j, row) in rows.iter_mut().enumerate().take(j1).skip(j0) {
            let yc = g.oy + (j as f64 + 0.5) * g.px;
            let x = lo[0] + (hi[0] - lo[0]) * (yc - lo[1]) / (hi[1] - lo[1]);
            if row.is_empty() {
                touched.push(j);
            }
            row.push((x, sign));
        }
    }
    let mut bb = BBox::EMPTY;
    for j in touched {
        let row = &mut rows[j];
        row.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut w = 0;
        for k in 0..row.len() {
            w -= row[k].1;
            if w != 0 && k + 1 < row.len() {
                let i0 = ((row[k].0 - g.ox) / g.px - 0.5).ceil().max(0.0) as usize;
                let i1 = (((row[k + 1].0 - g.ox) / g.px - 0.5).ceil().max(0.0) as usize).min(g.nx);
                if i0 < i1 {
                    out[j * g.nx + i0..j * g.nx + i1].fill(SUPPORTED);
                    bb.add(i0, j);
                    bb.add(i1 - 1, j);
                }
            }
        }
        row.clear();
    }
    bb
}

fn nbrs(idx: usize, nx: usize, ny: usize, off: &[(i64, i64)]) -> impl Iterator<Item = usize> + '_ {
    let (i, j) = ((idx % nx) as i64, (idx / nx) as i64);
    off.iter().filter_map(move |&(dx, dy)| {
        let (a, b) = (i + dx, j + dy);
        (a >= 0 && b >= 0 && (a as usize) < nx && (b as usize) < ny).then(|| b as usize * nx + a as usize)
    })
}

/// Uniform grid of slice segments; each segment is listed in every cell its
/// bounding box (grown by the query radius) touches, so a query reads one cell.
#[derive(Default)]
struct SegGrid {
    ox: f64,
    oy: f64,
    cs: f64,
    cnx: usize,
    cny: usize,
    start: Vec<u32>,
    items: Vec<u32>,
}

impl SegGrid {
    fn cell(&self, x: f64, y: f64) -> (usize, usize) {
        let cx = ((x - self.ox) / self.cs).floor().clamp(0.0, (self.cnx - 1) as f64) as usize;
        let cy = ((y - self.oy) / self.cs).floor().clamp(0.0, (self.cny - 1) as f64) as usize;
        (cx, cy)
    }

    /// Rebuilds for `segs`; returns the work done.
    fn build(&mut self, segs: &[[f64; 4]], g: &Grid, cs: f64, grow: f64) -> u64 {
        self.ox = g.ox;
        self.oy = g.oy;
        self.cs = cs;
        self.cnx = ((g.nx as f64 * g.px / cs).ceil() as usize).max(1);
        self.cny = ((g.ny as f64 * g.px / cs).ceil() as usize).max(1);
        let ncell = self.cnx * self.cny;
        self.start.clear();
        self.start.resize(ncell + 1, 0);
        let range = |s: &[f64; 4], this: &SegGrid| {
            let (a0, b0) = this.cell(s[0].min(s[2]) - grow, s[1].min(s[3]) - grow);
            let (a1, b1) = this.cell(s[0].max(s[2]) + grow, s[1].max(s[3]) + grow);
            (a0, b0, a1, b1)
        };
        let mut work = 0u64;
        for s in segs {
            let (a0, b0, a1, b1) = range(s, self);
            for b in b0..=b1 {
                for a in a0..=a1 {
                    self.start[b * self.cnx + a + 1] += 1;
                }
            }
            work += ((a1 - a0 + 1) * (b1 - b0 + 1)) as u64;
        }
        for c in 0..ncell {
            self.start[c + 1] += self.start[c];
        }
        self.items.clear();
        self.items.resize(self.start[ncell] as usize, 0);
        let mut fill = self.start.clone();
        for (k, s) in segs.iter().enumerate() {
            let (a0, b0, a1, b1) = range(s, self);
            for b in b0..=b1 {
                for a in a0..=a1 {
                    let c = b * self.cnx + a;
                    self.items[fill[c] as usize] = k as u32;
                    fill[c] += 1;
                }
            }
        }
        work + ncell as u64
    }

    /// Whether (x, y) lies within `r` of any segment; returns the work done too.
    fn within(&self, segs: &[[f64; 4]], x: f64, y: f64, r: f64) -> (bool, u64) {
        let (cx, cy) = self.cell(x, y);
        let c = cy * self.cnx + cx;
        let list = &self.items[self.start[c] as usize..self.start[c + 1] as usize];
        let r2 = r * r;
        for &k in list {
            let s = &segs[k as usize];
            let (dx, dy) = (s[2] - s[0], s[3] - s[1]);
            let len2 = dx * dx + dy * dy;
            let t = if len2 > 0.0 { (((x - s[0]) * dx + (y - s[1]) * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
            let (ex, ey) = (s[0] + t * dx - x, s[1] + t * dy - y);
            if ex * ex + ey * ey <= r2 {
                return (true, list.len() as u64);
            }
        }
        (false, list.len() as u64 + 1)
    }
}

#[derive(Default)]
struct Edt {
    /// Nearest material column in the same row (absolute), -1 if none.
    col: Vec<i64>,
    /// Squared distance (pixels^2) to the nearest material pixel, and its index.
    d2: Vec<f64>,
    near: Vec<usize>,
    sites: Vec<usize>,
    v: Vec<usize>,
    zb: Vec<f64>,
}

/// Exact Euclidean distance transform (Felzenszwalb-Huttenlocher) of the
/// non-empty pixels of `mask` within rectangle `r`, with nearest-pixel indices.
fn edt_nearest(mask: &[u8], nx: usize, r: BBox, e: &mut Edt) {
    let (w, h) = (r.x1 - r.x0 + 1, r.y1 - r.y0 + 1);
    e.col.clear();
    e.col.resize(w * h, -1);
    for jj in 0..h {
        let row = (r.y0 + jj) * nx;
        let mut last = -1i64;
        for ii in 0..w {
            if mask[row + r.x0 + ii] != EMPTY {
                last = (r.x0 + ii) as i64;
            }
            e.col[jj * w + ii] = last;
        }
        let mut next = -1i64;
        for ii in (0..w).rev() {
            let i = (r.x0 + ii) as i64;
            if mask[row + r.x0 + ii] != EMPTY {
                next = i;
            }
            let c = &mut e.col[jj * w + ii];
            if next >= 0 && (*c < 0 || next - i < i - *c) {
                *c = next;
            }
        }
    }
    e.d2.clear();
    e.d2.resize(w * h, f64::INFINITY);
    e.near.clear();
    e.near.resize(w * h, 0);
    let f = |col: &[i64], jj: usize, ii: usize| {
        let c = col[jj * w + ii];
        let d = (r.x0 + ii) as f64 - c as f64;
        d * d
    };
    for ii in 0..w {
        e.sites.clear();
        e.sites.extend((0..h).filter(|&jj| e.col[jj * w + ii] >= 0));
        if e.sites.is_empty() {
            continue;
        }
        e.v.clear();
        e.zb.clear();
        e.v.push(e.sites[0]);
        e.zb.push(f64::NEG_INFINITY);
        e.zb.push(f64::INFINITY);
        for &q in &e.sites[1..] {
            let fq = f(&e.col, q, ii) + (q * q) as f64;
            loop {
                let k = e.v.len() - 1;
                let p = e.v[k];
                let s = (fq - (f(&e.col, p, ii) + (p * p) as f64)) / (2.0 * q as f64 - 2.0 * p as f64);
                if s <= e.zb[k] && k > 0 {
                    e.v.pop();
                    e.zb.pop();
                    continue;
                }
                e.v.push(q);
                *e.zb.last_mut().unwrap() = s;
                e.zb.push(f64::INFINITY);
                break;
            }
        }
        let mut k = 0;
        for jj in 0..h {
            while e.zb[k + 1] < jj as f64 {
                k += 1;
            }
            let p = e.v[k];
            let dy = jj as f64 - p as f64;
            e.d2[jj * w + ii] = dy * dy + f(&e.col, p, ii);
            e.near[jj * w + ii] = (r.y0 + p) * nx + e.col[p * w + ii] as usize;
        }
    }
}

#[derive(Default)]
struct Runs {
    /// (row, x0, x1 exclusive) per run.
    span: Vec<(usize, usize, usize)>,
    parent: Vec<u32>,
    label: Vec<u32>,
}

impl Runs {
    fn find(&mut self, mut a: u32) -> u32 {
        while self.parent[a as usize] != a {
            let gp = self.parent[self.parent[a as usize] as usize];
            self.parent[a as usize] = gp;
            a = gp;
        }
        a
    }
}

/// 8-connected component labeling of the pixels in `bb` matching `pred`, by row
/// runs. Writes `base + component` into `out` for every matching pixel and
/// returns the component count.
fn label_runs(bb: BBox, nx: usize, pred: impl Fn(usize) -> bool, base: u32, out: &mut [u32], r: &mut Runs) -> u32 {
    r.span.clear();
    r.parent.clear();
    let mut prev_row = 0..0;
    for j in bb.y0..=bb.y1 {
        let row_start = r.span.len();
        let mut i = bb.x0;
        while i <= bb.x1 {
            if !pred(j * nx + i) {
                i += 1;
                continue;
            }
            let a = i;
            while i <= bb.x1 && pred(j * nx + i) {
                i += 1;
            }
            let id = r.span.len() as u32;
            r.span.push((j, a, i));
            r.parent.push(id);
            // Previous-row runs touching [a - 1, i] (8-connectivity).
            if j > bb.y0 {
                for p in prev_row.clone() {
                    let (_, pa, pb) = r.span[p];
                    if pa <= i && pb + 1 > a {
                        let (x, y) = (r.find(p as u32), r.find(id));
                        if x != y {
                            r.parent[x.max(y) as usize] = x.min(y);
                        }
                    }
                }
            }
        }
        prev_row = row_start..r.span.len();
    }
    r.label.clear();
    r.label.resize(r.span.len(), NONE);
    let mut count = 0;
    for k in 0..r.span.len() {
        let root = r.find(k as u32) as usize;
        if r.label[root] == NONE {
            r.label[root] = count;
            count += 1;
        }
        let (j, a, b) = r.span[k];
        out[j * nx + a..j * nx + b].fill(base + r.label[root]);
    }
    count
}

fn step_len(dx: i64, dy: i64) -> f64 {
    if dx != 0 && dy != 0 {
        std::f64::consts::SQRT_2
    } else {
        1.0
    }
}

#[derive(PartialEq)]
struct Dist(f64, usize);
impl Eq for Dist {}
impl PartialOrd for Dist {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Dist {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Min-heap on distance.
        other.0.total_cmp(&self.0).then(other.1.cmp(&self.1))
    }
}

/// Analyze one mesh; every issue reports `parts == [0]`.
#[allow(dead_code)] // the server binary goes through `support_check_parts`
pub fn support_check(triangles: &[[P; 3]], profile: &PrintProfile) -> SupportReport {
    analyze(triangles, None, profile)
}

/// Analyze several parts printed together (sliced as their union). Each part is
/// also rasterized on its own where issues occur, so every issue names the parts
/// whose material it covers.
pub fn support_check_parts(parts: &[Vec<[P; 3]>], profile: &PrintProfile) -> SupportReport {
    let tris: Vec<[P; 3]> = parts.iter().flatten().copied().collect();
    let part_of: Vec<u32> = parts.iter().enumerate().flat_map(|(i, p)| std::iter::repeat_n(i as u32, p.len())).collect();
    analyze(&tris, Some(&part_of), profile)
}

fn severity(kind: &str, ok: bool) -> u8 {
    match kind {
        "island" => 3,
        "bridge" if ok => 0,
        _ => 2,
    }
}

fn analyze(triangles: &[[P; 3]], part_of: Option<&[u32]>, profile: &PrintProfile) -> SupportReport {
    let mut prof = sanitize(profile);
    let finite: Vec<usize> = (0..triangles.len())
        .filter(|&t| triangles[t].iter().all(|p| p.iter().all(|c| c.is_finite())))
        .collect();
    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    for &t in &finite {
        for p in &triangles[t] {
            for a in 0..3 {
                lo[a] = lo[a].min(p[a]);
                hi[a] = hi[a].max(p[a]);
            }
        }
    }
    let bed = prof.bed_z.unwrap_or(if lo[2].is_finite() { lo[2] } else { 0.0 });
    prof.bed_z = Some(bed);
    let lh = prof.layer_height;
    let mut report = SupportReport {
        profile: prof.clone(),
        layers: 0,
        issues: Vec::new(),
        truncated: 0,
        complete: true,
        resolution: prof.line_width / 2.0,
        totals: SupportTotals::default(),
        notes: Vec::new(),
    };
    if finite.len() < triangles.len() {
        report.notes.push(format!("{} triangles with non-finite coordinates were ignored", triangles.len() - finite.len()));
    }
    if finite.is_empty() || hi[2] <= bed {
        return report;
    }
    let fail = |mut report: SupportReport, note: String| {
        report.complete = false;
        report.notes.push(note);
        report
    };
    let ext = [hi[0] - lo[0], hi[1] - lo[1], hi[2] - bed];
    if !ext.iter().all(|e| e.is_finite()) {
        return fail(report, "not analyzed: the model's extent is too large to represent".into());
    }

    // Grid. A pixel is supported when its centre lies inside the previous layer's
    // cross-section (its pixel is filled there) or within the overhang reach of
    // that cross-section's exact outline (slice segments), so there is no raster
    // slack. `radius` (pixels) only bounds the search for a previous-layer pixel
    // to link material components through.
    let reach = lh * prof.max_overhang_deg.to_radians().tan();
    let mut px = prof.line_width / 2.0;
    if reach / px + 1.5 > MAX_RADIUS_PX {
        px = reach / (MAX_RADIUS_PX - 1.5);
        report.notes.push(format!(
            "pixel size raised to {px:.3} mm so the {reach:.2} mm per-layer overhang reach fits the search radius"
        ));
    }
    let (mut radius, mut pad, mut nxf, mut nyf);
    let mut coarsened = false;
    let mut tries = 0;
    loop {
        radius = reach / px + 1.5;
        pad = radius.floor() + 4.0;
        nxf = (ext[0] / px).ceil() + 2.0 * pad;
        nyf = (ext[1] / px).ceil() + 2.0 * pad;
        if nxf.is_finite() && nyf.is_finite() && nxf * nyf <= MAX_PIXELS as f64 {
            break;
        }
        tries += 1;
        let grow = (nxf * nyf / MAX_PIXELS as f64).sqrt();
        if tries > 200 || !grow.is_finite() || !px.is_finite() {
            return fail(report, "not analyzed: the model's extent is too large for the analysis grid".into());
        }
        px *= grow.max(1.05);
        coarsened = true;
    }
    if coarsened {
        report.notes.push(format!(
            "grid coarsened to {px:.3} mm per pixel to stay within {MAX_PIXELS} pixels per layer; features thinner than that are not resolved"
        ));
        if px > 2.0 * prof.line_width {
            report.complete = false;
            report.notes.push(format!(
                "the {px:.3} mm grid is too coarse for {:.2} mm lines, so this is not a reliable check; analyze smaller parts separately",
                prof.line_width
            ));
        }
    }
    report.resolution = px;
    let (nx, ny) = (nxf as usize, nyf as usize);
    let g = Grid { ox: lo[0] - pad * px, oy: lo[1] - pad * px, px, nx, ny };
    let n = nx * ny;
    let support_disk = disk(radius);
    let group_disk = disk(radius + 2.0);
    let min_pixels = ((prof.line_width * prof.line_width) / (px * px)).max(2.0);
    let thin_extent = 3.0 * prof.line_width;
    // Exact-outline support test: reach plus a small numeric epsilon.
    let sup_r = reach + (0.1 * px).clamp(1e-6, 0.02);
    let corner_depth = (std::f64::consts::SQRT_2 - 1.0) * reach + 0.5 * px;
    let corner_extent = 2.0 * (reach + px);
    // Sub-line-width step test: disks of `k * reach` plus the slack (half a line
    // for k = 1, one and a half beyond) plus half a pixel, for k = 1..=env_layers;
    // skipped when they would be huge.
    let env_tan = (prof.max_overhang_deg + ENVELOPE_TOLERANCE_DEG).min(89.0).to_radians().tan()
        - prof.max_overhang_deg.to_radians().tan();
    let env_layers = ((1.5 * prof.line_width) / (lh * env_tan)).ceil().clamp(2.0, MAX_ENVELOPE_LAYERS as f64) as usize;
    let env_r = |k: usize| {
        let slack = if k == 1 { 0.5 } else { 1.5 } * prof.line_width;
        (k as f64 * reach + slack) / px + 0.5
    };
    let env_disks: Vec<Vec<(i64, i64)>> = if env_r(env_layers) <= MAX_RADIUS_PX {
        (1..=env_layers).map(|k| disk(env_r(k))).collect()
    } else {
        Vec::new()
    };

    let layers_f = (ext[2] / lh).ceil().max(0.0);
    let layers = if layers_f > MAX_LAYERS as f64 { MAX_LAYERS } else { layers_f as usize };
    report.layers = layers;
    if layers_f > MAX_LAYERS as f64 {
        report.complete = false;
        report.notes.push(format!(
            "only the lowest {MAX_LAYERS} layers (up to z={:.1}) were analyzed",
            bed + MAX_LAYERS as f64 * lh
        ));
    }

    // Triangles ordered by their lowest z, swept upward.
    let mut order = finite;
    let tz = |t: usize, f: fn(f64, f64) -> f64| f(f(triangles[t][0][2], triangles[t][1][2]), triangles[t][2][2]);
    order.sort_by(|&a, &b| tz(a, f64::min).total_cmp(&tz(b, f64::min)));
    let mut next_tri = 0;
    let mut active: Vec<usize> = Vec::new();

    let mut rows: Vec<Vec<(f64, i32)>> = vec![Vec::new(); ny];
    // Rotating state buffers: [cur, prev, prev2] by index (layer % 3).
    let mut state: [Vec<u8>; 3] = [vec![EMPTY; n], vec![EMPTY; n], vec![EMPTY; n]];
    let mut bbox = [BBox::EMPTY; 3];
    // Occupancy bits of layers i-3 .. i-env_layers (ring by layer index).
    let older_n = if env_disks.is_empty() { 0 } else { env_layers - 2 };
    let mut older: Vec<Vec<u64>> = vec![vec![0; n.div_ceil(64)]; older_n];
    let mut older_bb = vec![BBox::EMPTY; older.len()];
    let mut comp: [Vec<u32>; 2] = [vec![NONE; n], vec![NONE; n]];
    // Per supported pixel of the current layer: the previous layer's component it rests on.
    let mut src: Vec<u32> = vec![NONE; n];
    let mut issue_map: [Vec<u32>; 2] = [vec![NONE; n], vec![NONE; n]];
    let mut mark: Vec<u32> = vec![0; n];
    let mut stamp: u32 = 0;
    let mut dist: Vec<f32> = vec![0.0; n];
    let mut scratch: Vec<u8> = if part_of.is_some() { vec![EMPTY; n] } else { Vec::new() };
    let mut uf = UnionFind { parent: Vec::new(), rooted: Vec::new() };
    let mut issues: Vec<SupportIssue> = Vec::new();
    // Per issue: (part, pixel count).
    let mut part_counts: Vec<Vec<(usize, usize)>> = Vec::new();
    let mut overflow = SupportTotals::default();
    // Island issue index -> a union-find element of its material.
    let mut open_islands: Vec<(usize, u32)> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut runs = Runs::default();
    let mut edt = Edt::default();
    let mut segs: [Vec<[f64; 4]>; 2] = [Vec::new(), Vec::new()];
    let mut seg_grid = SegGrid::default();
    let mut pixels: Vec<usize> = Vec::new();
    let mut contacts: Vec<usize> = Vec::new();
    let mut probes: u64 = 0;
    let mut stop: Option<&'static str> = None;

    let neighbors = |idx: usize, off: &'static [(i64, i64)]| nbrs(idx, nx, ny, off);

    'layers: for layer in 0..layers {
        if crate::csg::bsp::deadline_passed() {
            stop = Some("the time budget ran out");
            break;
        }
        let z = bed + (layer as f64 + 0.5) * lh;
        let c3 = layer % 3;
        let c2 = layer % 2;

        while next_tri < order.len() && tz(order[next_tri], f64::min) <= z {
            active.push(order[next_tri]);
            next_tri += 1;
        }
        active.retain(|&t| tz(t, f64::max) >= z);

        // Keep layer i-3 (about to be overwritten) for the step test.
        if layer >= 3 && !older.is_empty() {
            let slot = (layer - 3) % older.len();
            let ob = older_bb[slot];
            // Whole 64-pixel words: the slot has no bits outside its bbox, and the
            // state is EMPTY outside its own, so neighbouring pixels stay correct.
            let bits = &mut older[slot];
            if !ob.is_empty() {
                for j in ob.y0..=ob.y1 {
                    bits[(j * nx + ob.x0) >> 6..=(j * nx + ob.x1) >> 6].fill(0);
                }
            }
            let sb = bbox[c3];
            if !sb.is_empty() {
                let st = &state[c3];
                for j in sb.y0..=sb.y1 {
                    for w in (j * nx + sb.x0) >> 6..=(j * nx + sb.x1) >> 6 {
                        let chunk = &st[w * 64..(w * 64 + 64).min(n)];
                        let mut word = 0u64;
                        for (b, &v) in chunk.iter().enumerate() {
                            word |= u64::from(v != EMPTY) << b;
                        }
                        bits[w] = word;
                    }
                }
            }
            older_bb[slot] = sb;
        }

        // Clear and rasterize the current layer.
        let old = bbox[c3];
        if !old.is_empty() {
            for j in old.y0..=old.y1 {
                state[c3][j * nx + old.x0..=j * nx + old.x1].fill(EMPTY);
            }
        }
        segs[c2].clear();
        let bb = slice_layer(triangles, &active, z, &g, &mut rows, &mut state[c3], Some(&mut segs[c2]));
        bbox[c3] = bb;
        if bb.is_empty() {
            continue;
        }
        let [s0, s1, s2] = &mut state;
        let (cur, prev, prev2) = match c3 {
            0 => (s0, &*s2, &*s1),
            1 => (s1, &*s0, &*s2),
            _ => (s2, &*s1, &*s0),
        };
        let [comp0, comp1] = &mut comp;
        let (ccomp, pcomp) = if c2 == 0 { (comp0, &*comp1) } else { (comp1, &*comp0) };

        // Material components of this layer.
        let first_comp = uf.parent.len() as u32;
        let count = label_runs(bb, nx, |s| cur[s] != EMPTY, first_comp, ccomp, &mut runs);
        if uf.parent.len() + count as usize > MAX_UF_NODES {
            stop = Some("the model has too many separate cross-section pieces");
            break;
        }
        for _ in 0..count {
            uf.push(layer == 0);
        }
        if layer == 0 {
            continue;
        }

        // Support from the previous layer. Large radii use an exact Euclidean
        // distance transform of the previous layer instead of disk probing.
        let use_edt = support_disk.len() > 48;
        let ri = radius.ceil() as usize;
        let er = BBox {
            x0: bb.x0.saturating_sub(ri),
            y0: bb.y0.saturating_sub(ri),
            x1: (bb.x1 + ri).min(nx - 1),
            y1: (bb.y1 + ri).min(ny - 1),
        };
        if use_edt {
            edt_nearest(prev, nx, er, &mut edt);
            probes += 8 * ((er.x1 - er.x0 + 1) * (er.y1 - er.y0 + 1)) as u64;
        }
        let prev_segs = &segs[1 - c2];
        probes += seg_grid.build(prev_segs, &g, (8.0 * px).max(2.0 * sup_r), sup_r);
        let mut last_pair = (NONE, NONE);
        for j in bb.y0..=bb.y1 {
            if (j - bb.y0).is_multiple_of(32) {
                if crate::csg::bsp::deadline_passed() {
                    stop = Some("the time budget ran out");
                    break 'layers;
                }
                if probes > MAX_PROBES {
                    stop = Some("the work budget ran out");
                    break 'layers;
                }
            }
            for i in bb.x0..=bb.x1 {
                let s = j * nx + i;
                if cur[s] == EMPTY {
                    continue;
                }
                if prev[s] != EMPTY {
                    cur[s] = SUPPORTED;
                    src[s] = pcomp[s];
                    if last_pair != (ccomp[s], pcomp[s]) {
                        last_pair = (ccomp[s], pcomp[s]);
                        uf.union(last_pair.0, last_pair.1);
                    }
                    continue;
                }
                let (x, y) = g.center(s);
                let (hit, work) = seg_grid.within(prev_segs, x, y, sup_r);
                probes += work;
                if !hit {
                    cur[s] = UNSUPPORTED;
                    continue;
                }
                cur[s] = SUPPORTED;
                // Link to the previous layer's material through its nearest pixel.
                let near = if use_edt {
                    let at = (j - er.y0) * (er.x1 - er.x0 + 1) + (i - er.x0);
                    (edt.d2[at] <= radius * radius + 1e-9).then_some(edt.near[at])
                } else {
                    probes += support_disk.len() as u64;
                    nbrs(s, nx, ny, &support_disk).find(|&q| prev[q] != EMPTY)
                };
                src[s] = near.map_or(NONE, |q| pcomp[q]);
                if let Some(q) = near {
                    if last_pair != (ccomp[s], pcomp[q]) {
                        last_pair = (ccomp[s], pcomp[q]);
                        uf.union(last_pair.0, last_pair.1);
                    }
                }
            }
        }

        // Unsupported components.
        let [im0, im1] = &mut issue_map;
        let (cmap, pmap) = if c2 == 0 { (im0, &*im1) } else { (im1, &*im0) };
        let mut layer_has_issue = false;
        for j in bb.y0..=bb.y1 {
            for i in bb.x0..=bb.x1 {
                let s0 = j * nx + i;
                if cur[s0] != UNSUPPORTED {
                    continue;
                }
                if crate::csg::bsp::deadline_passed() {
                    stop = Some("the time budget ran out");
                    break 'layers;
                }
                if probes > MAX_PROBES {
                    stop = Some("the work budget ran out");
                    break 'layers;
                }
                // Collect the component; ISSUE marks it as visited.
                pixels.clear();
                cur[s0] = ISSUE;
                stack.push(s0);
                while let Some(p) = stack.pop() {
                    pixels.push(p);
                    for q in neighbors(p, &NEIGH8) {
                        if cur[q] == UNSUPPORTED {
                            cur[q] = ISSUE;
                            stack.push(q);
                        }
                    }
                }
                probes += 8 * pixels.len() as u64;

                // Contacts: supported pixels touching the component. Enclosed: no empty neighbour.
                stamp = stamp.wrapping_add(2);
                if stamp < 2 {
                    mark.fill(0);
                    stamp = 2;
                }
                contacts.clear();
                let mut enclosed = true;
                for &p in &pixels {
                    for q in neighbors(p, &NEIGH8) {
                        match cur[q] {
                            SUPPORTED if mark[q] != stamp => {
                                mark[q] = stamp;
                                contacts.push(q);
                            }
                            EMPTY => enclosed = false,
                            _ => {}
                        }
                    }
                }
                // Anchors: 8-connected runs of contact pixels (separate contact arcs),
                // split where the run passes from one piece of the layer below to another.
                let mut anchor_acc: Vec<(f64, f64, usize)> = Vec::new();
                for &c in &contacts {
                    if mark[c] != stamp {
                        continue;
                    }
                    mark[c] = stamp + 1;
                    stack.push(c);
                    let mut acc = (0.0, 0.0, 0);
                    while let Some(p) = stack.pop() {
                        let (x, y) = g.center(p);
                        acc.0 += x;
                        acc.1 += y;
                        acc.2 += 1;
                        for q in neighbors(p, &NEIGH8) {
                            if mark[q] == stamp && src[q] == src[c] {
                                mark[q] = stamp + 1;
                                stack.push(q);
                            }
                        }
                    }
                    anchor_acc.push(acc);
                }

                // Bounds & centroid.
                let (mut sx, mut sy) = (0.0, 0.0);
                let mut bmin = [f64::INFINITY, f64::INFINITY, z];
                let mut bmax = [f64::NEG_INFINITY, f64::NEG_INFINITY, z];
                for &p in &pixels {
                    let (x, y) = g.center(p);
                    sx += x;
                    sy += y;
                    bmin[0] = bmin[0].min(x - px / 2.0);
                    bmin[1] = bmin[1].min(y - px / 2.0);
                    bmax[0] = bmax[0].max(x + px / 2.0);
                    bmax[1] = bmax[1].max(y + px / 2.0);
                }
                let cnt = pixels.len() as f64;

                // Reach from the contacts (geodesic, in pixels).
                let far = if anchor_acc.is_empty() {
                    None
                } else {
                    let mut heap = BinaryHeap::new();
                    for &p in &pixels {
                        dist[p] = f32::INFINITY;
                    }
                    for &p in &pixels {
                        let (i, j) = ((p % nx) as i64, (p / nx) as i64);
                        let mut best = f64::INFINITY;
                        for &(dx, dy) in &NEIGH8 {
                            let (a, b) = (i + dx, j + dy);
                            if a >= 0
                                && b >= 0
                                && (a as usize) < nx
                                && (b as usize) < ny
                                && cur[b as usize * nx + a as usize] == SUPPORTED
                            {
                                best = best.min(step_len(dx, dy));
                            }
                        }
                        if best.is_finite() {
                            dist[p] = best as f32;
                            heap.push(Dist(best, p));
                        }
                    }
                    let mut far: f64 = 0.0;
                    while let Some(Dist(d, p)) = heap.pop() {
                        // Compare as stored (f32) so relaxations strictly decrease and terminate.
                        if d as f32 > dist[p] {
                            continue;
                        }
                        far = far.max(d);
                        let (i, j) = ((p % nx) as i64, (p / nx) as i64);
                        for &(dx, dy) in &NEIGH8 {
                            let (a, b) = (i + dx, j + dy);
                            if a < 0 || b < 0 || a as usize >= nx || b as usize >= ny {
                                continue;
                            }
                            let q = b as usize * nx + a as usize;
                            let nd = d + step_len(dx, dy);
                            if cur[q] == ISSUE && (nd as f32) < dist[q] {
                                dist[q] = nd as f32;
                                heap.push(Dist(nd, q));
                            }
                        }
                    }
                    Some(far)
                };

                // Noise filtering.
                let noise = if anchor_acc.is_empty() {
                    // A thin feature can drop out of one slice: material at the same
                    // pixels two layers down means it continues, not that it floats.
                    layer >= 2 && pixels.iter().any(|&p| prev2[p] != EMPTY)
                } else {
                    let core = pixels.iter().any(|&p| {
                        let (i, j) = ((p % nx) as i64, (p / nx) as i64);
                        let at = |dx: i64, dy: i64| {
                            let (a, b) = (i + dx, j + dy);
                            a >= 0 && b >= 0 && (a as usize) < nx && (b as usize) < ny && cur[b as usize * nx + a as usize] == ISSUE
                        };
                        (at(-1, 0) || at(1, 0)) && (at(0, -1) || at(0, 1))
                    });
                    let extent = (bmax[0] - bmin[0]).max(bmax[1] - bmin[1]);
                    // The round reach test leaves a small wedge at each convex corner of
                    // a wall leaning exactly at the limit (its faces are fine; only the
                    // corner edge leans more, as with a slicer's mitered offset). Such a
                    // wedge is at most (sqrt2-1)*reach deep and about 2*reach wide.
                    let corner = far.unwrap_or(f64::INFINITY) * px <= corner_depth && extent <= corner_extent;
                    let small = cnt < min_pixels || (!core && extent <= thin_extent) || corner;
                    // Sub-line-width step: every pixel within the envelope of each layer below
                    // (layer i-1-k for disk k; layers below the bed count as support).
                    small
                        || (!env_disks.is_empty()
                            && pixels.iter().all(|&p| {
                                env_disks.iter().enumerate().take(layer).all(|(k, d)| {
                                    probes += d.len() as u64;
                                    match k {
                                        0 => nbrs(p, nx, ny, d).any(|q| prev[q] != EMPTY),
                                        1 => nbrs(p, nx, ny, d).any(|q| prev2[q] != EMPTY),
                                        _ => {
                                            let bits = &older[(layer - 1 - k) % older.len()];
                                            nbrs(p, nx, ny, d).any(|q| bits[q >> 6] >> (q & 63) & 1 != 0)
                                        }
                                    }
                                })
                            }))
                };
                if noise {
                    for &p in &pixels {
                        cur[p] = NOISE;
                    }
                    continue;
                }

                // Classification: no contact = island; contact arcs on two or more
                // sides, or a hole fully enclosed by support = bridge; else cantilever.
                let kind = if anchor_acc.is_empty() {
                    "island"
                } else if anchor_acc.len() >= 2 || enclosed {
                    "bridge"
                } else {
                    "cantilever"
                };
                // Distances run from contact pixel centres, which lie inside the
                // reach band; add the reach back (less half a pixel for centre
                // offsets) to measure from the previous layer's actual edge.
                let span = far.map(|d| {
                    let reach_out = (d * px + reach - 0.5 * px).max(d * px);
                    if kind == "bridge" {
                        2.0 * reach_out
                    } else {
                        reach_out
                    }
                });
                let ok = kind == "bridge" && span.is_some_and(|s| s <= prof.max_bridge);
                let anchors: Vec<[f64; 3]> =
                    anchor_acc.iter().take(MAX_ANCHORS).map(|&(x, y, c)| [x / c as f64, y / c as f64, z]).collect();

                // Continue an issue from the previous layer when within reach of its region.
                let cont = pixels.iter().find_map(|&p| {
                    nbrs(p, nx, ny, &group_disk).find(|&q| prev[q] == ISSUE && pmap[q] != NONE).map(|q| pmap[q])
                });
                probes += (group_disk.len() * pixels.len()) as u64;
                let id = match cont {
                    Some(id) => {
                        let is = &mut issues[id as usize];
                        for a in 0..3 {
                            is.min[a] = is.min[a].min(bmin[a]);
                            is.max[a] = is.max[a].max(bmax[a]);
                        }
                        is.z_top = is.z_top.max(z);
                        is.layer_count += 1;
                        // Escalate to the worst classification seen; islands stay islands.
                        let (old, new) = (severity(is.kind, is.ok), severity(kind, ok));
                        if new > old {
                            is.kind = kind;
                            is.ok = ok;
                            is.span = span;
                            is.anchors = anchors;
                        } else if is.kind == kind && kind != "island" {
                            if let Some(s) = span {
                                is.span = Some(is.span.map_or(s, |o| o.max(s)));
                            }
                            is.ok = kind == "bridge" && is.span.is_some_and(|s| s <= prof.max_bridge);
                        }
                        id
                    }
                    None if issues.len() >= MAX_RETAINED_ISSUES => {
                        match kind {
                            "island" => overflow.islands += 1,
                            "cantilever" => overflow.cantilevers += 1,
                            _ => {
                                overflow.bridges += 1;
                                overflow.bridges_too_long += usize::from(!ok);
                            }
                        }
                        NONE
                    }
                    None => {
                        issues.push(SupportIssue {
                            kind,
                            ok,
                            first_layer: layer,
                            z,
                            z_top: z,
                            min: bmin,
                            max: bmax,
                            location: [sx / cnt, sy / cnt, z],
                            area: cnt * px * px,
                            span,
                            anchors,
                            joins_at: None,
                            reason: String::new(),
                            layer_count: 1,
                            parts: Vec::new(),
                        });
                        part_counts.push(Vec::new());
                        let id = issues.len() - 1;
                        if kind == "island" {
                            open_islands.push((id, ccomp[pixels[0]]));
                        }
                        id as u32
                    }
                };
                for &p in &pixels {
                    cmap[p] = id;
                }
                if id != NONE {
                    layer_has_issue = true;
                    if part_of.is_none() {
                        add_count(&mut part_counts[id as usize], 0, pixels.len());
                    }
                }
            }
        }

        // Which parts occupy this layer's issue pixels: rasterize each part alone.
        if let (Some(part_of), true) = (part_of, layer_has_issue) {
            let mut buckets: Vec<(u32, Vec<usize>)> = Vec::new();
            let mut sorted = active.clone();
            sorted.sort_by_key(|&t| part_of[t]);
            for t in sorted {
                match buckets.last_mut() {
                    Some((p, v)) if *p == part_of[t] => v.push(t),
                    _ => buckets.push((part_of[t], vec![t])),
                }
            }
            for (part, tris) in buckets {
                let pb = slice_layer(triangles, &tris, z, &g, &mut rows, &mut scratch, None);
                if pb.is_empty() {
                    continue;
                }
                let mut local: Vec<(u32, usize)> = Vec::new();
                for j in pb.y0..=pb.y1 {
                    for i in pb.x0..=pb.x1 {
                        let s = j * nx + i;
                        if scratch[s] != EMPTY && cur[s] == ISSUE && cmap[s] != NONE {
                            match local.iter_mut().find(|e| e.0 == cmap[s]) {
                                Some(e) => e.1 += 1,
                                None => local.push((cmap[s], 1)),
                            }
                        }
                    }
                    scratch[j * nx + pb.x0..=j * nx + pb.x1].fill(EMPTY);
                }
                for (id, c) in local {
                    add_count(&mut part_counts[id as usize], part as usize, c);
                }
            }
        }

        // Islands that now connect to bed-rooted material.
        open_islands.retain(|&(id, e)| {
            if uf.is_rooted(e) {
                issues[id].joins_at = Some(z);
                false
            } else {
                true
            }
        });
    }
    if let Some(why) = stop {
        report.complete = false;
        report.notes.push(format!("analysis stopped early: {why}; layers above the last finished one were not checked"));
    }
    if issues.len() >= MAX_RETAINED_ISSUES {
        report.complete = false;
        report.notes.push(format!(
            "more than {MAX_RETAINED_ISSUES} separate issues; later ones are only counted, without grouping across layers"
        ));
    }

    let overflow_count = overflow.islands + overflow.cantilevers + overflow.bridges;
    let mut totals = overflow;
    for (is, mut counts) in issues.iter_mut().zip(part_counts) {
        counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        is.parts = counts.into_iter().map(|(p, _)| p).collect();
        is.reason = describe(is, &prof);
        match is.kind {
            "island" => totals.islands += 1,
            "cantilever" => totals.cantilevers += 1,
            _ => {
                totals.bridges += 1;
                totals.bridges_too_long += usize::from(!is.ok);
            }
        }
    }
    report.totals = totals;
    issues.sort_by(|a, b| a.z.total_cmp(&b.z).then(b.area.total_cmp(&a.area)));
    if issues.len() > MAX_SUPPORT_ISSUES {
        report.truncated = issues.len() - MAX_SUPPORT_ISSUES;
        issues.truncate(MAX_SUPPORT_ISSUES);
    }
    report.truncated += overflow_count;
    report.issues = issues;
    report
}

fn add_count(counts: &mut Vec<(usize, usize)>, part: usize, n: usize) {
    match counts.iter_mut().find(|e| e.0 == part) {
        Some(e) => e.1 += n,
        None => counts.push((part, n)),
    }
}

fn describe(is: &SupportIssue, prof: &PrintProfile) -> String {
    let at = format!("({:.1}, {:.1}, {:.1})", is.location[0], is.location[1], is.z);
    match is.kind {
        "island" => match is.joins_at {
            Some(j) => format!(
                "starts in midair at z={:.1} near {at} with nothing below; joins the rest at z={j:.1}, which does not make the start printable",
                is.z
            ),
            None => format!(
                "starts in midair at z={:.1} near {at} with nothing below and never connects to anything supported from the bed",
                is.z
            ),
        },
        "cantilever" => format!(
            "overhangs {:.1} mm out from a single anchor at z={:.1} near {at}; needs support",
            is.span.unwrap_or(0.0),
            is.z
        ),
        _ => {
            let span = is.span.unwrap_or(0.0);
            if is.ok {
                format!(
                    "bridges {span:.1} mm between {} anchors at z={:.1} near {at}; within the {:.1} mm bridge limit",
                    is.anchors.len(),
                    is.z,
                    prof.max_bridge
                )
            } else {
                format!(
                    "bridges {span:.1} mm between {} anchors at z={:.1} near {at}; longer than the {:.1} mm bridge limit, needs support",
                    is.anchors.len(),
                    is.z,
                    prof.max_bridge
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hexahedron from 8 corners indexed by bits (x, y, z), outward-facing.
    fn hexa(corner: impl Fn(usize) -> P) -> Vec<[P; 3]> {
        let quads = [[0, 2, 3, 1], [4, 5, 7, 6], [0, 1, 5, 4], [2, 6, 7, 3], [0, 4, 6, 2], [1, 3, 7, 5]];
        let mut t = Vec::new();
        for q in quads {
            t.push([corner(q[0]), corner(q[1]), corner(q[2])]);
            t.push([corner(q[0]), corner(q[2]), corner(q[3])]);
        }
        t
    }

    fn cube(min: P, max: P) -> Vec<[P; 3]> {
        hexa(|i| {
            [
                if i & 1 == 0 { min[0] } else { max[0] },
                if i & 2 == 0 { min[1] } else { max[1] },
                if i & 4 == 0 { min[2] } else { max[2] },
            ]
        })
    }

    /// Square frustum centered on the z axis: half-width `b` at z0, `t` at z1.
    fn frustum(b: f64, t: f64, z0: f64, z1: f64) -> Vec<[P; 3]> {
        hexa(|i| {
            let h = if i & 4 == 0 { b } else { t };
            [if i & 1 == 0 { -h } else { h }, if i & 2 == 0 { -h } else { h }, if i & 4 == 0 { z0 } else { z1 }]
        })
    }

    /// Closed n-gon frustum around the z axis (radius r0 at z0, r1 at z1).
    fn cone(n: usize, r0: f64, r1: f64, z0: f64, z1: f64) -> Vec<[P; 3]> {
        let pt = |k: usize, r: f64, z: f64| {
            let a = std::f64::consts::TAU * k as f64 / n as f64;
            [r * a.cos(), r * a.sin(), z]
        };
        let mut t = Vec::new();
        for k in 0..n {
            let (a0, a1) = (pt(k, r0, z0), pt(k + 1, r0, z0));
            let (b0, b1) = (pt(k, r1, z1), pt(k + 1, r1, z1));
            t.push([a0, a1, b1]);
            t.push([a0, b1, b0]);
            t.push([[0.0, 0.0, z0], a1, a0]);
            t.push([[0.0, 0.0, z1], b0, b1]);
        }
        t
    }

    fn check(parts: &[Vec<[P; 3]>], profile: &PrintProfile) -> SupportReport {
        let tris: Vec<[P; 3]> = parts.iter().flatten().copied().collect();
        let r = support_check(&tris, profile);
        assert!(r.complete, "{:?}", r.notes);
        r
    }

    #[test]
    fn supported_shapes_have_no_issues() {
        let p = PrintProfile::default();
        assert!(check(&[cube([0.0; 3], [10.0; 3])], &p).issues.is_empty());
        // Narrowing upward, and widening at exactly 45 degrees.
        assert!(check(&[frustum(10.0, 2.0, 0.0, 8.0)], &p).issues.is_empty());
        let r = check(&[frustum(5.0, 9.0, 0.0, 4.0)], &p);
        assert!(r.issues.is_empty(), "{:?}", r.issues);
        // Round shapes: cylinder, cone narrowing upward, cone widening at 45 degrees.
        assert!(check(&[cone(64, 8.0, 8.0, 0.0, 10.0)], &p).issues.is_empty());
        assert!(check(&[cone(64, 8.0, 1.0, 0.0, 10.0)], &p).issues.is_empty());
        let r = check(&[cone(64, 4.0, 10.0, 0.0, 6.0)], &p);
        assert!(r.issues.is_empty(), "{:?}", r.issues);
        // Overlapping shells (nonzero fill) on the bed.
        assert!(check(&[cube([0.0; 3], [10.0; 3]), cube([5.0, 5.0, 0.0], [15.0, 15.0, 6.0])], &p).issues.is_empty());
        assert_eq!(check(&[cube([0.0; 3], [10.0; 3])], &p).profile.bed_z, Some(0.0));
    }

    #[test]
    fn exact_45_degrees_with_thick_layers_is_clean() {
        let p = PrintProfile { layer_height: 0.3, ..Default::default() };
        let r = check(&[frustum(5.0, 11.0, 0.0, 6.0)], &p);
        assert!(r.issues.is_empty(), "{:?}", r.issues);
        let r = check(&[cone(64, 4.0, 10.0, 0.0, 6.0)], &p);
        assert!(r.issues.is_empty(), "{:?}", r.issues);
        // A 60 degree slope at the default profile.
        let r = check(&[frustum(5.0, 8.5, 0.0, 2.0)], &PrintProfile::default());
        assert!(!r.issues.is_empty());
    }

    #[test]
    fn steep_overhang_is_flagged() {
        // 4 mm out over 2 mm height: ~63 degrees from vertical.
        let r = check(&[frustum(5.0, 9.0, 0.0, 2.0)], &PrintProfile::default());
        assert!(!r.issues.is_empty());
    }

    /// A round core with thread-like 60 degree teeth: each underside flank
    /// steps 0.35 mm per layer (under a line width) and ends after ~0.9 mm.
    fn sawtooth(depth: f64, teeth: usize) -> Vec<Vec<[P; 3]>> {
        let rise = depth / 60f64.to_radians().tan();
        let pitch = 2.0 * rise + 0.3;
        let mut parts = vec![cone(64, 5.0, 5.0, 0.0, 2.0 + teeth as f64 * pitch)];
        for t in 0..teeth {
            let z = 2.0 + t as f64 * pitch;
            parts.push(cone(64, 5.0, 5.0 + depth, z, z + rise));
            parts.push(cone(64, 5.0 + depth, 5.0, z + rise, z + 2.0 * rise));
        }
        parts
    }

    #[test]
    fn sub_line_width_thread_steps_are_clear() {
        let p = PrintProfile::default();
        // M12 and M24 coarse thread depths (0.95 and 1.62 mm).
        for depth in [0.95, 1.62] {
            let r = check(&sawtooth(depth, 4), &p);
            assert!(r.issues.is_empty(), "depth {depth}: {:?}", r.issues);
        }
        // A 2 mm flat ledge on a post is still a cantilever.
        let r = check(&[cube([0.0; 3], [4.0, 4.0, 6.0]), cube([0.0, 0.0, 5.0], [6.0, 4.0, 6.0])], &p);
        assert_eq!(r.issues.len(), 1, "{:?}", r.issues);
        assert_eq!(r.issues[0].kind, "cantilever");
        assert!(!r.issues[0].ok && r.issues[0].span.unwrap() > 1.8);
        // A ledge only half a millimetre wide is still more than half a line.
        let r = check(&[cube([0.0; 3], [4.0, 4.0, 6.0]), cube([0.0, 0.0, 5.0], [4.6, 4.0, 6.0])], &p);
        assert!(r.issues.iter().any(|i| i.kind == "cantilever" && !i.ok), "{:?}", r.issues);
        // The same 60 degree flank sustained over 3.5 mm is not excused.
        let r = check(&[cone(64, 4.0, 7.5, 0.0, 2.0)], &p);
        assert!(r.issues.iter().any(|i| !i.ok), "{:?}", r.issues);
        // Nor a sustained 58 degree slope.
        let r = check(&[cone(64, 4.0, 4.0 + 6.0 * 58f64.to_radians().tan(), 0.0, 6.0)], &p);
        assert!(r.issues.iter().any(|i| !i.ok), "{:?}", r.issues);
    }

    #[test]
    fn notch_between_two_feet_is_a_bridge() {
        // Two L-shaped feet 0.4 mm apart (the gap is within reach, so the contacts
        // on both sides join), opening into a 6 x 2 mm notch at the far edge.
        let feet = [
            cube([-6.0, 0.0, 0.0], [-0.2, 8.0, 5.0]),
            cube([-6.0, 8.0, 0.0], [-3.0, 10.0, 5.0]),
            cube([0.2, 0.0, 0.0], [6.0, 8.0, 5.0]),
            cube([3.0, 8.0, 0.0], [6.0, 10.0, 5.0]),
        ];
        let mut parts = feet.to_vec();
        parts.push(cube([-6.0, 0.0, 5.0], [6.0, 10.0, 6.0]));
        let r = check(&parts, &PrintProfile::default());
        assert!(!r.issues.is_empty() && r.issues.iter().all(|i| i.kind == "bridge" && i.ok), "{:?}", r.issues);
        // A single L-shaped post under a slab's overhanging corner is still a cantilever.
        let parts = [cube([-6.0, 0.0, 0.0], [6.0, 8.0, 5.0]), cube([-6.0, 0.0, 5.0], [6.0, 10.0, 6.0])];
        let r = check(&parts, &PrintProfile::default());
        assert!(r.issues.iter().any(|i| i.kind == "cantilever" && !i.ok), "{:?}", r.issues);
    }

    #[test]
    #[ignore]
    fn perf_large_model() {
        let t = std::time::Instant::now();
        let r = check(&[cone(128, 100.0, 60.0, 0.0, 100.0), cube([-100.0, -100.0, 50.0], [100.0, 100.0, 52.0])], &PrintProfile::default());
        eprintln!("{} layers, {} issues, res {}, {:?}", r.layers, r.issues.len(), r.resolution, t.elapsed());
    }

    #[test]
    fn one_layer_gap_island_next_to_post_is_kept() {
        // The slab starts one layer above the post's top and 0.3 mm beside it.
        let parts = [cube([0.0, 0.0, 0.0], [2.0, 2.0, 10.0]), cube([2.3, 0.0, 10.2], [8.0, 2.0, 11.0])];
        let r = check(&parts, &PrintProfile::default());
        assert!(r.issues.iter().any(|i| i.kind == "island" && (i.z - 10.3).abs() < 1e-6), "{:?}", r.issues);
    }

    #[test]
    fn long_one_pixel_cantilever_is_kept() {
        // A 0.3 mm wide arm (one pixel row on this grid) reaching 6 mm out.
        let parts = [cube([0.0, 0.0, 0.0], [2.0, 2.0, 10.0]), cube([0.0, 1.0, 10.0], [8.0, 1.3, 11.0])];
        let r = check(&parts, &PrintProfile::default());
        assert_eq!(r.issues.len(), 1, "{:?}", r.issues);
        assert_eq!(r.issues[0].kind, "cantilever");
        assert!(r.issues[0].span.unwrap() > 5.0);
    }

    #[test]
    fn overhang_reach_uses_physical_distances() {
        // An 11 degree flare passes at 45 degrees but not at 0.
        let flare = [frustum(5.0, 6.0, 0.0, 5.0)];
        assert!(check(&flare, &PrintProfile::default()).issues.is_empty());
        let strict = check(&flare, &PrintProfile { max_overhang_deg: 0.0, ..Default::default() });
        assert!(!strict.issues.is_empty());
        // Thick layers on a fine grid: the reach (2 mm) exceeds the radius cap, so the
        // grid is coarsened instead of clamping the reach; 45 degree walls stay clean.
        let p = PrintProfile { layer_height: 2.0, line_width: 0.05, ..Default::default() };
        let r = check(&[frustum(5.0, 9.0, 0.0, 4.0)], &p);
        assert!(r.issues.is_empty(), "{:?}", r.issues);
        assert!(r.notes.iter().any(|n| n.contains("pixel size raised")), "{:?}", r.notes);
        let r = check(&[frustum(5.0, 13.0, 0.0, 4.0)], &p);
        assert!(!r.issues.is_empty());
    }

    #[test]
    fn huge_bounds_do_not_panic() {
        let r = support_check(&cube([0.0; 3], [1e20, 10.0, 10.0]), &PrintProfile::default());
        assert!(!r.complete && !r.notes.is_empty());
        let r = support_check(&cube([-1e308, 0.0, 0.0], [1e308, 10.0, 10.0]), &PrintProfile::default());
        assert!(!r.complete && !r.notes.is_empty());
        let r = support_check(&cube([0.0; 3], [10.0, 10.0, 1e20]), &PrintProfile::default());
        assert!(!r.complete && !r.notes.is_empty());
    }

    #[test]
    fn roof_over_enclosed_hole_is_a_bridge() {
        let parts = [
            cube([0.0, 0.0, 0.0], [3.0, 10.0, 8.0]),
            cube([7.0, 0.0, 0.0], [10.0, 10.0, 8.0]),
            cube([3.0, 0.0, 0.0], [7.0, 3.0, 8.0]),
            cube([3.0, 7.0, 0.0], [7.0, 10.0, 8.0]),
            cube([0.0, 0.0, 8.0], [10.0, 10.0, 10.0]),
        ];
        let r = check(&parts, &PrintProfile::default());
        assert_eq!(r.issues.len(), 1, "{:?}", r.issues);
        let is = &r.issues[0];
        assert_eq!(is.kind, "bridge");
        assert!(is.ok);
        assert!((is.span.unwrap() - 4.0).abs() < 0.6, "span {:?}", is.span);
        let r = check(&parts, &PrintProfile { max_bridge: 3.0, ..Default::default() });
        assert!(!r.issues[0].ok);
        assert_eq!(r.totals.bridges_too_long, 1);
    }

    #[test]
    fn grouped_issue_escalates_to_worst_layer() {
        // A short bridge whose next layer grows a 4 mm cantilever off it.
        let parts = [
            cube([0.0, 0.0, 0.0], [2.0, 2.0, 10.0]),
            cube([5.0, 0.0, 0.0], [7.0, 2.0, 10.0]),
            cube([0.0, 0.0, 10.0], [7.0, 2.0, 10.2]),
            cube([2.0, 0.0, 10.2], [5.0, 6.0, 11.0]),
        ];
        let r = check(&parts, &PrintProfile::default());
        assert!(r.issues.iter().all(|i| !i.ok), "{:?}", r.issues);
        assert!(r.issues.iter().any(|i| i.kind == "cantilever" && i.span.unwrap() > 3.0), "{:?}", r.issues);
    }

    #[test]
    fn parts_and_totals() {
        let post = cube([-1.0, -1.0, 0.0], [1.0, 1.0, 10.0]);
        let slab = cube([-6.0, -1.0, 10.0], [6.0, 1.0, 12.0]);
        let r = support_check_parts(&[post.clone(), slab.clone()], &PrintProfile::default());
        assert_eq!(r.issues.len(), 2);
        assert!(r.issues.iter().all(|i| i.parts == vec![1]), "{:?}", r.issues);
        assert_eq!(r.totals, SupportTotals { islands: 0, cantilevers: 2, bridges: 0, bridges_too_long: 0 });
        let all: Vec<[P; 3]> = post.into_iter().chain(slab).collect();
        let single = support_check(&all, &PrintProfile::default());
        assert!(single.issues.iter().all(|i| i.parts == vec![0]));
    }

    #[test]
    fn edt_matches_brute_force() {
        let (nx, ny) = (23usize, 17usize);
        let mut seed = 12345u64;
        let mask: Vec<u8> = (0..nx * ny)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                u8::from((seed >> 33).is_multiple_of(13))
            })
            .collect();
        let r = BBox { x0: 2, y0: 1, x1: 20, y1: 15 };
        let mut e = Edt::default();
        edt_nearest(&mask, nx, r, &mut e);
        let w = r.x1 - r.x0 + 1;
        for j in r.y0..=r.y1 {
            for i in r.x0..=r.x1 {
                let mut best = f64::INFINITY;
                for b in r.y0..=r.y1 {
                    for a in r.x0..=r.x1 {
                        if mask[b * nx + a] != 0 {
                            let d = (a as f64 - i as f64).powi(2) + (b as f64 - j as f64).powi(2);
                            best = best.min(d);
                        }
                    }
                }
                let at = (j - r.y0) * w + (i - r.x0);
                assert_eq!(e.d2[at], best, "at ({i}, {j})");
                if best.is_finite() {
                    let q = e.near[at];
                    assert_eq!(mask[q], 1);
                    let d = ((q % nx) as f64 - i as f64).powi(2) + ((q / nx) as f64 - j as f64).powi(2);
                    assert_eq!(d, best);
                }
            }
        }
    }

    #[test]
    fn t_shape_cantilevers() {
        let r = check(
            &[cube([-1.0, -1.0, 0.0], [1.0, 1.0, 10.0]), cube([-6.0, -1.0, 10.0], [6.0, 1.0, 12.0])],
            &PrintProfile::default(),
        );
        assert_eq!(r.issues.len(), 2, "{:?}", r.issues);
        for is in &r.issues {
            assert_eq!(is.kind, "cantilever");
            assert!(!is.ok);
            assert!((is.z - 10.1).abs() < 1e-6);
            assert_eq!(is.anchors.len(), 1);
            let span = is.span.unwrap();
            assert!((span - 5.0).abs() < 0.3, "span {span}");
        }
    }

    #[test]
    fn bridge_between_posts() {
        let parts = [
            cube([0.0, 0.0, 0.0], [2.0, 2.0, 10.0]),
            cube([10.0, 0.0, 0.0], [12.0, 2.0, 10.0]),
            cube([0.0, 0.0, 10.0], [12.0, 2.0, 11.0]),
        ];
        let r = check(&parts, &PrintProfile::default());
        assert_eq!(r.issues.len(), 1, "{:?}", r.issues);
        let is = &r.issues[0];
        assert_eq!(is.kind, "bridge");
        assert!(is.ok);
        assert_eq!(is.anchors.len(), 2);
        let span = is.span.unwrap();
        assert!((span - 8.0).abs() < 0.3, "span {span}");
        let r = check(&parts, &PrintProfile { max_bridge: 5.0, ..Default::default() });
        assert!(!r.issues[0].ok);
        assert!(r.issues[0].reason.contains("bridge limit"));
    }

    #[test]
    fn islands_and_joins() {
        let parts = [
            // Floating column, later joined to the pillar by a top bar.
            cube([0.0, 0.0, 10.0], [4.0, 4.0, 20.0]),
            cube([10.0, 0.0, 0.0], [14.0, 4.0, 22.0]),
            cube([0.0, 0.0, 20.0], [14.0, 4.0, 22.0]),
            // Floating box that never connects.
            cube([30.0, 0.0, 5.0], [34.0, 4.0, 7.0]),
        ];
        let r = check(&parts, &PrintProfile::default());
        let islands: Vec<_> = r.issues.iter().filter(|i| i.kind == "island").collect();
        assert_eq!(islands.len(), 2, "{:?}", r.issues);
        let free = islands.iter().find(|i| i.location[0] > 25.0).unwrap();
        assert!((free.z - 5.1).abs() < 1e-6);
        assert_eq!(free.joins_at, None);
        assert!(free.reason.contains("never connects"));
        let joined = islands.iter().find(|i| i.location[0] < 5.0).unwrap();
        assert!((joined.z - 10.1).abs() < 1e-6);
        let j = joined.joins_at.unwrap();
        assert!((j - 20.1).abs() < 0.25, "joins_at {j}");
        assert!(joined.anchors.is_empty() && joined.span.is_none());
        // The top bar spans the gap between the column and the pillar.
        assert!(r.issues.iter().any(|i| i.kind == "bridge" && (i.z - 20.1).abs() < 1e-6));
    }

    #[test]
    fn lattice_has_only_short_bridges() {
        let mut parts = Vec::new();
        for x in [0.0, 5.0, 10.0] {
            parts.push(cube([x, 0.0, 0.0], [x + 2.0, 2.0, 12.0]));
        }
        for z in [4.0, 8.0] {
            parts.push(cube([0.0, 0.0, z], [12.0, 2.0, z + 1.0]));
        }
        let r = check(&parts, &PrintProfile::default());
        assert_eq!(r.issues.len(), 4, "{:?}", r.issues);
        for is in &r.issues {
            assert_eq!(is.kind, "bridge");
            assert!(is.ok);
            assert!((is.span.unwrap() - 3.0).abs() < 0.3, "{:?}", is.span);
        }
    }

    #[test]
    fn floating_model_and_truncation() {
        let p = PrintProfile { bed_z: Some(-5.0), ..Default::default() };
        let r = check(&[cube([0.0; 3], [2.0; 3])], &p);
        assert_eq!(r.issues.len(), 1);
        assert_eq!(r.issues[0].kind, "island");
        let many: Vec<_> = (0..60).map(|i| cube([i as f64 * 3.0, 0.0, 1.0], [i as f64 * 3.0 + 1.0, 1.0, 2.0])).collect();
        let r = check(&many, &PrintProfile { bed_z: Some(0.0), ..Default::default() });
        assert_eq!(r.issues.len(), MAX_SUPPORT_ISSUES);
        assert_eq!(r.truncated, 10);
        assert_eq!(r.totals.islands, 60);
        assert!(support_check(&[], &PrintProfile::default()).issues.is_empty());
    }
}
