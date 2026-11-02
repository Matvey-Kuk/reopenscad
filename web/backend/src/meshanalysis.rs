//! Geometry understanding for agents: what a mesh is, how it is posed, where
//! its features are, and exact queries against its surface.
//!
//! Everything here has to cope with scanned meshes (noisy, open, unwelded,
//! up to a few hundred thousand triangles) as well as clean CAD output, so:
//! - nothing may panic, whatever the input; degenerate input yields empty or
//!   best-effort results plus a human-readable note explaining why;
//! - every surface query goes through one bounding-volume hierarchy
//!   ([`MeshIndex`]) so describe stays well under two seconds at 200k
//!   triangles and callers can batch thousands of queries cheaply;
//! - heuristics (units, features, symmetry) report how well they fit rather
//!   than pretending to certainty, because an agent acts on these numbers.

use crate::vec3::{add, cross, dot, norm, scale as mul, sub};
use std::collections::HashMap;

use serde::Serialize;

use crate::assets::TriMesh;

#[derive(Clone, Copy, Debug)]
pub struct DescribeOptions {
    /// Fit planes, cylinders and spheres (the expensive part).
    pub features: bool,
    pub max_features: usize,
    pub max_lay_flat: usize,
}

impl Default for DescribeOptions {
    fn default() -> Self {
        Self { features: true, max_features: 12, max_lay_flat: 4 }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Description {
    pub triangles: usize,
    pub bounds_min: [f64; 3],
    pub bounds_max: [f64; 3],
    pub size: [f64; 3],
    /// Enclosed volume in mm^3 (absolute value; see `signed_volume`).
    pub volume: f64,
    /// Volume as wound: negative when the triangles face inward.
    pub signed_volume: f64,
    /// Centre of mass of the enclosed solid (surface centroid when the mesh
    /// encloses no volume).
    pub center_of_mass: [f64; 3],
    pub surface_area: f64,
    pub watertight: bool,
    pub units: UnitsGuess,
    pub principal_axes: PrincipalAxes,
    pub lay_flat: Vec<LayFlat>,
    pub symmetry: Option<Symmetry>,
    pub features: Vec<Feature>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnitsGuess {
    /// "mm", "cm", "m" or "in": the unit the file was most likely written in.
    pub likely: String,
    /// Uniform scale that converts the file to millimetres (1, 10, 1000, 25.4).
    pub suggested_scale: f64,
    pub reason: String,
}

/// Oriented bounding box from a PCA of the *surface* (area-weighted), for
/// posing and sizing a part. It is not a centerline: lumps, flares, a
/// lopsided end or uneven scan coverage tilt the PCA, and the axes snap to
/// the world X/Y/Z whenever that box is within 1% of the PCA box, so a body
/// tilted by a degree or two can come back looking aligned. For the true
/// axis of a tapered, flared or scanned elongated body use [`fit_axis`]
/// (centroids of perpendicular cuts).
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrincipalAxes {
    /// Area-weighted centroid of the surface.
    pub center: [f64; 3],
    /// Unit axes, longest extent first, right-handed.
    pub axes: [[f64; 3]; 3],
    /// Full extents of the oriented bounding box along `axes`.
    pub extents: [f64; 3],
    /// OpenSCAD `multmatrix` (row-major 4x4) that moves the oriented box's
    /// centre to the origin and its axes onto X, Y, Z.
    pub align_matrix: [[f64; 4]; 4],
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LayFlat {
    /// Outward normal of the face that would rest on the bed.
    pub normal: [f64; 3],
    /// Flat contact area in mm^2.
    pub contact_area: f64,
    /// `multmatrix` putting that face on z = 0 (normal to -Z), centred on X/Y.
    pub matrix: [[f64; 4]; 4],
    /// Height of the part once laid this way.
    pub height: f64,
    /// Whether the centre of mass sits over the contact patch.
    pub stable: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Symmetry {
    pub point: [f64; 3],
    pub normal: [f64; 3],
    /// Mean mirrored-surface deviation in mm (lower is more symmetric).
    pub deviation: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Feature {
    #[serde(rename_all = "camelCase")]
    Plane { point: [f64; 3], normal: [f64; 3], area: f64, rms: f64 },
    /// `hole` is true when the surface faces the axis (a bore, not a boss).
    #[serde(rename_all = "camelCase")]
    Cylinder {
        axis_point: [f64; 3],
        axis: [f64; 3],
        radius: f64,
        length: f64,
        hole: bool,
        coverage_deg: f64,
        rms: f64,
    },
    #[serde(rename_all = "camelCase")]
    Sphere { center: [f64; 3], radius: f64, hole: bool, rms: f64 },
    /// Conical surface (a tapered shaft, handle, countersink or tapered
    /// bore). `axis` points from the wide end (`radius_start`) to the narrow
    /// end (`radius_end`), towards `apex`; `axis_point` is the middle of the
    /// fitted stretch on the axis (the apex is far away for small angles, so
    /// prefer `axis_point` + `axis` for positioning). `half_angle_deg` > 0.
    #[serde(rename_all = "camelCase")]
    Cone {
        apex: [f64; 3],
        axis: [f64; 3],
        axis_point: [f64; 3],
        half_angle_deg: f64,
        radius_start: f64,
        radius_end: f64,
        length: f64,
        hole: bool,
        coverage_deg: f64,
        rms: f64,
    },
}

// ---------------------------------------------------------------------------
// Small vector kit. Plain arrays keep the hot loops free of abstraction cost.

type V3 = [f64; 3];

/// Unit vector, or zero for a zero/non-finite input (callers test for zero
/// instead of handling NaN downstream).
#[inline]
fn unit(a: V3) -> V3 {
    let l = norm(a);
    if l > 1e-300 && l.is_finite() {
        mul(a, 1.0 / l)
    } else {
        [0.0; 3]
    }
}
#[inline]
fn is_zero(a: V3) -> bool {
    a == [0.0; 3]
}
#[inline]
fn finite3(a: V3) -> bool {
    a.iter().all(|c| c.is_finite())
}
/// Component of `x` perpendicular to unit `axis`.
#[inline]
fn perp(x: V3, axis: V3) -> V3 {
    sub(x, mul(axis, dot(x, axis)))
}
/// Flips `a` so its largest-magnitude component is positive: deterministic
/// output for axes whose sign is otherwise arbitrary.
fn canonical(a: V3) -> V3 {
    let k = (0..3).max_by(|&i, &j| a[i].abs().total_cmp(&a[j].abs())).unwrap_or(0);
    if a[k] < 0.0 {
        mul(a, -1.0)
    } else {
        a
    }
}
/// Orthonormal (u, v) with u x v = n; u comes from the world axis least
/// aligned with `n`, so the basis is deterministic and well conditioned.
fn plane_basis(n: V3) -> (V3, V3) {
    let k = (0..3).min_by(|&i, &j| n[i].abs().total_cmp(&n[j].abs())).unwrap_or(0);
    let mut e = [0.0; 3];
    e[k] = 1.0;
    let u = unit(perp(e, n));
    let v = cross(n, u);
    (u, v)
}
fn round6(x: f64) -> f64 {
    if x.is_finite() {
        (x * 1e6).round() / 1e6
    } else {
        0.0
    }
}
fn round_v(a: V3) -> V3 {
    a.map(round6)
}

// ---------------------------------------------------------------------------
// Preparation: weld, drop junk, cache per-triangle geometry.

const NONE: u32 = u32::MAX;

/// The mesh as analysis sees it: vertices welded by exact position (STL
/// soups arrive unwelded, which would make every edge look like a boundary),
/// triangles with out-of-range indices, non-finite points or repeated
/// vertices dropped, and per-triangle normal/area/centroid cached.
struct Prep {
    points: Vec<V3>,
    tris: Vec<[u32; 3]>,
    normals: Vec<V3>,
    areas: Vec<f64>,
    centroids: Vec<V3>,
    total_area: f64,
    dropped: usize,
}

impl Prep {
    #[inline]
    fn corners(&self, t: usize) -> [V3; 3] {
        let [a, b, c] = self.tris[t];
        [self.points[a as usize], self.points[b as usize], self.points[c as usize]]
    }
}

fn prepare(mesh: &TriMesh) -> Prep {
    const BAD: u32 = u32::MAX - 1;
    let mut weld: HashMap<[u64; 3], u32> = HashMap::with_capacity(mesh.points.len());
    let mut remap = vec![NONE; mesh.points.len()];
    let mut points = Vec::new();
    let mut tris = Vec::with_capacity(mesh.triangles.len());
    let mut dropped = 0;
    'tri: for tri in &mesh.triangles {
        let mut ids = [0u32; 3];
        for k in 0..3 {
            let i = tri[k] as usize;
            if i >= mesh.points.len() {
                dropped += 1;
                continue 'tri;
            }
            if remap[i] == NONE {
                let p = mesh.points[i];
                remap[i] = if finite3(p) {
                    // `+ 0.0` folds -0.0 into 0.0 so both weld together.
                    let key = p.map(|c| (c + 0.0).to_bits());
                    *weld.entry(key).or_insert_with(|| {
                        points.push(p);
                        (points.len() - 1) as u32
                    })
                } else {
                    BAD
                };
            }
            if remap[i] == BAD {
                dropped += 1;
                continue 'tri;
            }
            ids[k] = remap[i];
        }
        if ids[0] == ids[1] || ids[1] == ids[2] || ids[0] == ids[2] {
            dropped += 1;
            continue;
        }
        tris.push(ids);
    }
    let mut normals = Vec::with_capacity(tris.len());
    let mut areas = Vec::with_capacity(tris.len());
    let mut centroids = Vec::with_capacity(tris.len());
    let mut total_area = 0.0;
    for ids in &tris {
        let [a, b, c] = ids.map(|i| points[i as usize]);
        let n = cross(sub(b, a), sub(c, a));
        let area = 0.5 * norm(n);
        let area = if area.is_finite() { area } else { 0.0 };
        normals.push(unit(n));
        areas.push(area);
        centroids.push(mul(add(add(a, b), c), 1.0 / 3.0));
        total_area += area;
    }
    Prep { points, tris, normals, areas, centroids, total_area, dropped }
}

/// Edge adjacency and the defects that decide whether the mesh is a closed
/// solid.
struct Topo {
    /// Triangle across edge k (v_k -> v_k+1), or NONE at boundaries and
    /// non-manifold edges.
    neighbors: Vec<[u32; 3]>,
    boundary_edges: usize,
    nonmanifold_edges: usize,
    misoriented_edges: usize,
    boundary_loops: usize,
}

impl Topo {
    fn watertight(&self) -> bool {
        self.boundary_edges == 0 && self.nonmanifold_edges == 0 && self.misoriented_edges == 0
    }
}

fn topology(prep: &Prep) -> Topo {
    // Sorting (key, tri, slot, forward) beats a HashMap at this size and
    // groups every edge's users together.
    let mut edges: Vec<(u64, u64)> = Vec::with_capacity(prep.tris.len() * 3);
    for (t, ids) in prep.tris.iter().enumerate() {
        for k in 0..3 {
            let (a, b) = (ids[k], ids[(k + 1) % 3]);
            let (lo, hi, forward) = if a < b { (a, b, 1) } else { (b, a, 0) };
            let key = ((lo as u64) << 32) | hi as u64;
            edges.push((key, ((t as u64) << 3) | ((k as u64) << 1) | forward));
        }
    }
    edges.sort_unstable();
    let mut neighbors = vec![[NONE; 3]; prep.tris.len()];
    let (mut boundary, mut nonmanifold, mut misoriented) = (0, 0, 0);
    let mut boundary_vertices: Vec<(u32, u32)> = Vec::new();
    let mut i = 0;
    while i < edges.len() {
        let mut j = i + 1;
        while j < edges.len() && edges[j].0 == edges[i].0 {
            j += 1;
        }
        match j - i {
            1 => {
                boundary += 1;
                let key = edges[i].0;
                boundary_vertices.push(((key >> 32) as u32, key as u32));
            }
            2 => {
                let (x, y) = (edges[i].1, edges[i + 1].1);
                if (x & 1) == (y & 1) {
                    misoriented += 1;
                }
                let (tx, kx) = ((x >> 3) as usize, ((x >> 1) & 3) as usize);
                let (ty, ky) = ((y >> 3) as usize, ((y >> 1) & 3) as usize);
                neighbors[tx][kx] = ty as u32;
                neighbors[ty][ky] = tx as u32;
            }
            _ => nonmanifold += 1,
        }
        i = j;
    }
    // Boundary loops = connected components of the boundary-edge graph
    // (union-find over the vertices involved).
    let mut parent: HashMap<u32, u32> = HashMap::new();
    fn find(parent: &mut HashMap<u32, u32>, x: u32) -> u32 {
        let mut r = x;
        while let Some(&p) = parent.get(&r) {
            if p == r {
                break;
            }
            r = p;
        }
        let mut c = x;
        while c != r {
            let next = parent[&c];
            parent.insert(c, r);
            c = next;
        }
        r
    }
    for &(a, b) in &boundary_vertices {
        parent.entry(a).or_insert(a);
        parent.entry(b).or_insert(b);
        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
        if ra != rb {
            parent.insert(ra, rb);
        }
    }
    let keys: Vec<u32> = parent.keys().copied().collect();
    let mut roots = std::collections::HashSet::new();
    for k in keys {
        roots.insert(find(&mut parent, k));
    }
    Topo {
        neighbors,
        boundary_edges: boundary,
        nonmanifold_edges: nonmanifold,
        misoriented_edges: misoriented,
        boundary_loops: roots.len(),
    }
}

// ---------------------------------------------------------------------------
// Bounding-volume hierarchy over triangles.

const LEAF_SIZE: usize = 4;

#[derive(Clone, Copy)]
struct Node {
    min: V3,
    max: V3,
    /// Leaf: first triangle; interior: unused.
    start: u32,
    /// Triangles in a leaf; 0 for interior nodes (left child = index + 1).
    count: u32,
    right: u32,
}

/// Acceleration structure for ray casts, closest-point and inside tests.
/// Build once (O(n log n)) and reuse it for a batch of queries; the free
/// functions [`raycast`] and [`closest_points`] build one per call.
pub struct MeshIndex {
    tris: Vec<[V3; 3]>,
    // Read by `raycast`/`closest_point`, which the server binary does not
    // call yet (library and test API; see the allow on those methods).
    #[allow(dead_code)]
    normals: Vec<V3>,
    /// Index of each stored triangle in the faces it was built from.
    ids: Vec<u32>,
    nodes: Vec<Node>,
}

/// Fixed, non-axis-aligned directions for parity voting: axis-aligned rays
/// would graze the edges and faces of axis-aligned CAD parts.
const PARITY_DIRS: [V3; 3] = [
    [0.538_516_480_713_450_4, 0.615_447_406_529_657_6, 0.575_655_446_537_087_4],
    [-0.702_727_871_598_384_6, 0.172_993_327_101_174_5, 0.690_041_311_542_498_6],
    [0.215_665_822_745_812_8, -0.893_472_696_847_301_5, 0.393_934_316_573_449_6],
];

impl MeshIndex {
    pub fn new(mesh: &TriMesh) -> Self {
        Self::from_faces(&mesh.points, mesh.triangles.iter())
    }

    /// Index over a subset of an indexed mesh's faces (one shell of it, say),
    /// skipping out-of-range indices and non-finite corners.
    pub fn from_faces<'a>(points: &[[f64; 3]], faces: impl Iterator<Item = &'a [u32; 3]>) -> Self {
        let (ids, tris) = faces
            .enumerate()
            .filter_map(|(i, t)| {
                let corners = [points.get(t[0] as usize)?, points.get(t[1] as usize)?, points.get(t[2] as usize)?];
                let corners = corners.map(|p| *p);
                corners.iter().all(|p| finite3(*p)).then_some((i as u32, corners))
            })
            .unzip();
        Self::from_corners(tris, ids)
    }

    fn from_prep(prep: &Prep) -> Self {
        let count = prep.tris.len();
        Self::from_corners((0..count).map(|t| prep.corners(t)).collect(), (0..count as u32).collect())
    }

    fn from_corners(tris: Vec<[V3; 3]>, ids: Vec<u32>) -> Self {
        let centroids: Vec<V3> =
            tris.iter().map(|[a, b, c]| mul(add(add(*a, *b), *c), 1.0 / 3.0)).collect();
        let mut order: Vec<u32> = (0..tris.len() as u32).collect();
        let mut nodes = Vec::with_capacity(tris.len() / 2 + 1);
        if !tris.is_empty() {
            build_node(&tris, &centroids, &mut order, 0, tris.len(), &mut nodes);
        }
        // Store triangles in leaf order for cache-friendly traversal.
        let tris: Vec<[V3; 3]> = order.iter().map(|&i| tris[i as usize]).collect();
        let ids = order.iter().map(|&i| ids[i as usize]).collect();
        let normals = tris.iter().map(|[a, b, c]| unit(cross(sub(*b, *a), sub(*c, *a)))).collect();
        Self { tris, normals, ids, nodes }
    }

    /// Nearest surface point to `point` as (distance, point, original face
    /// index, face normal); `None` for an empty mesh.
    pub fn nearest_face(&self, point: [f64; 3]) -> Option<(f64, [f64; 3], usize, [f64; 3])> {
        let (d2, q, t) = self.nearest(point)?;
        Some((d2.sqrt(), q, self.ids[t] as usize, self.normals[t]))
    }

    /// Every crossing of the ray within `max_distance`, nearest first, as
    /// (distance, original face index, face normal). Hits closer than 1e-9
    /// of each other on the same ray (a shared edge) are reported once.
    pub fn raycast_all(&self, origin: [f64; 3], direction: [f64; 3], max_distance: f64) -> Vec<(f64, usize, [f64; 3])> {
        let dir = unit(direction);
        if is_zero(dir) || !finite3(origin) || self.nodes.is_empty() {
            return Vec::new();
        }
        let inv = dir.map(|c| 1.0 / c);
        let mut hits = Vec::new();
        let mut stack: Vec<usize> = Vec::with_capacity(64);
        stack.push(0);
        while let Some(i) = stack.pop() {
            let node = &self.nodes[i];
            if ray_box(node.min, node.max, origin, inv, max_distance).is_none() {
                continue;
            }
            if node.count > 0 {
                for t in node.start as usize..(node.start + node.count) as usize {
                    if let Some(d) = ray_triangle(origin, dir, &self.tris[t]).filter(|d| *d <= max_distance) {
                        hits.push((d, self.ids[t] as usize, self.normals[t]));
                    }
                }
            } else {
                stack.push(i + 1);
                stack.push(node.right as usize);
            }
        }
        hits.sort_by(|a, b| a.0.total_cmp(&b.0));
        hits.dedup_by(|b, a| (b.0 - a.0).abs() < 1e-9 && dot(a.2, b.2) > 0.0);
        hits
    }

    /// Nearest hit along the ray; `None` for a miss, an empty mesh or a zero
    /// direction.
    pub fn raycast(&self, origin: [f64; 3], direction: [f64; 3]) -> Option<RayHit> {
        let dir = unit(direction);
        if is_zero(dir) || !finite3(origin) || self.nodes.is_empty() {
            return None;
        }
        let inv = dir.map(|c| 1.0 / c);
        let mut best = f64::INFINITY;
        let mut best_tri = usize::MAX;
        let mut stack: Vec<usize> = Vec::with_capacity(64);
        stack.push(0);
        while let Some(i) = stack.pop() {
            let node = &self.nodes[i];
            if ray_box(node.min, node.max, origin, inv, best).is_none() {
                continue;
            }
            if node.count > 0 {
                for t in node.start as usize..(node.start + node.count) as usize {
                    if let Some(d) = ray_triangle(origin, dir, &self.tris[t]) {
                        if d < best {
                            best = d;
                            best_tri = t;
                        }
                    }
                }
            } else {
                let (l, r) = (i + 1, node.right as usize);
                let tl = ray_box(self.nodes[l].min, self.nodes[l].max, origin, inv, best);
                let tr = ray_box(self.nodes[r].min, self.nodes[r].max, origin, inv, best);
                match (tl, tr) {
                    (Some(a), Some(b)) => {
                        // Push the farther child first so the nearer pops next.
                        if a <= b {
                            stack.push(r);
                            stack.push(l);
                        } else {
                            stack.push(l);
                            stack.push(r);
                        }
                    }
                    (Some(_), None) => stack.push(l),
                    (None, Some(_)) => stack.push(r),
                    (None, None) => {}
                }
            }
        }
        if best_tri == usize::MAX {
            return None;
        }
        let normal = self.normals[best_tri];
        Some(RayHit {
            point: add(origin, mul(dir, best)),
            normal,
            distance: best,
            entering: dot(dir, normal) < 0.0,
        })
    }

    /// Unsigned distance to the surface (infinite for an empty mesh).
    pub fn distance(&self, point: [f64; 3]) -> f64 {
        self.nearest(point).map_or(f64::INFINITY, |(d2, _, _)| d2.sqrt())
    }

    /// Whether `point` is inside the solid, by majority vote of ray-parity
    /// tests along three skewed directions. One ray can be fooled by passing
    /// exactly through an edge (counted twice) or, on an open mesh, by
    /// leaving through a hole; three independent rays rarely all are.
    pub fn contains(&self, point: [f64; 3]) -> bool {
        if self.nodes.is_empty() || !finite3(point) {
            return false;
        }
        // Two agreeing rays settle the majority; the third breaks a tie.
        let odd = |d: V3| self.crossings(point, d) % 2 == 1;
        let (first, second) = (odd(PARITY_DIRS[0]), odd(PARITY_DIRS[1]));
        if first == second {
            first
        } else {
            odd(PARITY_DIRS[2])
        }
    }

    fn crossings(&self, origin: V3, dir: V3) -> usize {
        let inv = dir.map(|c| 1.0 / c);
        let mut count = 0;
        let mut stack: Vec<usize> = Vec::with_capacity(64);
        stack.push(0);
        while let Some(i) = stack.pop() {
            let node = &self.nodes[i];
            if ray_box(node.min, node.max, origin, inv, f64::INFINITY).is_none() {
                continue;
            }
            if node.count > 0 {
                for t in node.start as usize..(node.start + node.count) as usize {
                    if matches!(ray_triangle(origin, dir, &self.tris[t]), Some(d) if d > 0.0) {
                        count += 1;
                    }
                }
            } else {
                stack.push(i + 1);
                stack.push(node.right as usize);
            }
        }
        count
    }

    /// (squared distance, closest point, triangle) by best-first descent.
    fn nearest(&self, p: V3) -> Option<(f64, V3, usize)> {
        if self.nodes.is_empty() || !finite3(p) {
            return None;
        }
        let mut best = f64::INFINITY;
        let mut best_q = [0.0; 3];
        let mut best_t = 0;
        let mut stack: Vec<(usize, f64)> = Vec::with_capacity(64);
        stack.push((0, box_dist2(&self.nodes[0], p)));
        while let Some((i, lb)) = stack.pop() {
            if lb >= best {
                continue;
            }
            let node = &self.nodes[i];
            if node.count > 0 {
                for t in node.start as usize..(node.start + node.count) as usize {
                    let [a, b, c] = self.tris[t];
                    let q = closest_on_triangle(p, a, b, c);
                    let d = sub(q, p);
                    let d2 = dot(d, d);
                    if d2 < best {
                        best = d2;
                        best_q = q;
                        best_t = t;
                    }
                }
            } else {
                let (l, r) = (i + 1, node.right as usize);
                let (dl, dr) = (box_dist2(&self.nodes[l], p), box_dist2(&self.nodes[r], p));
                if dl <= dr {
                    stack.push((r, dr));
                    stack.push((l, dl));
                } else {
                    stack.push((l, dl));
                    stack.push((r, dr));
                }
            }
        }
        best.is_finite().then_some((best, best_q, best_t))
    }
}

fn build_node(
    tris: &[[V3; 3]],
    centroids: &[V3],
    order: &mut [u32],
    start: usize,
    end: usize,
    nodes: &mut Vec<Node>,
) -> usize {
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    let mut cmin = [f64::INFINITY; 3];
    let mut cmax = [f64::NEG_INFINITY; 3];
    for &t in &order[start..end] {
        for p in &tris[t as usize] {
            for k in 0..3 {
                min[k] = min[k].min(p[k]);
                max[k] = max[k].max(p[k]);
            }
        }
        let c = centroids[t as usize];
        for k in 0..3 {
            cmin[k] = cmin[k].min(c[k]);
            cmax[k] = cmax[k].max(c[k]);
        }
    }
    let index = nodes.len();
    nodes.push(Node { min, max, start: start as u32, count: 0, right: 0 });
    let extent = sub(cmax, cmin);
    let axis = (0..3).max_by(|&i, &j| extent[i].total_cmp(&extent[j])).unwrap_or(0);
    if end - start <= LEAF_SIZE || !(extent[axis] > 0.0) {
        nodes[index].count = (end - start) as u32;
        return index;
    }
    let mid = (start + end) / 2;
    order[start..end].select_nth_unstable_by(mid - start, |&a, &b| {
        centroids[a as usize][axis].total_cmp(&centroids[b as usize][axis])
    });
    build_node(tris, centroids, order, start, mid, nodes);
    let right = build_node(tris, centroids, order, mid, end, nodes);
    nodes[index].right = right as u32;
    index
}

/// Slab test; returns the entry distance when the box is hit before `tmax`.
/// A ray parallel to a slab is tested by containment instead: the division
/// would give 0 * inf = NaN when the origin lies on the slab plane, and the
/// NaN would wrongly cull boxes the ray grazes (axis-aligned rays on
/// axis-aligned geometry do this constantly).
#[inline]
fn ray_box(min: V3, max: V3, o: V3, inv: V3, tmax: f64) -> Option<f64> {
    let mut t0: f64 = 0.0;
    let mut t1: f64 = tmax;
    for k in 0..3 {
        if inv[k].is_infinite() {
            if o[k] < min[k] || o[k] > max[k] {
                return None;
            }
            continue;
        }
        let a = (min[k] - o[k]) * inv[k];
        let b = (max[k] - o[k]) * inv[k];
        let (lo, hi) = if a < b { (a, b) } else { (b, a) };
        t0 = t0.max(lo);
        t1 = t1.min(hi);
        if t0 > t1 {
            return None;
        }
    }
    Some(t0)
}

/// Möller–Trumbore, two-sided (open meshes and inside-out shells still hit).
#[inline]
fn ray_triangle(o: V3, d: V3, tri: &[V3; 3]) -> Option<f64> {
    let [a, b, c] = *tri;
    let e1 = sub(b, a);
    let e2 = sub(c, a);
    let p = cross(d, e2);
    let det = dot(e1, p);
    let scale = norm(e1) * norm(e2);
    if !(det.abs() > 1e-14 * scale) {
        return None;
    }
    let inv = 1.0 / det;
    let s = sub(o, a);
    // A sliver of slack on the barycentric bounds so a ray through a shared
    // edge or vertex cannot slip between both triangles to rounding.
    const SLACK: f64 = 1e-10;
    let u = dot(s, p) * inv;
    if !(-SLACK..=1.0 + SLACK).contains(&u) {
        return None;
    }
    let q = cross(s, e1);
    let v = dot(d, q) * inv;
    if v < -SLACK || u + v > 1.0 + SLACK {
        return None;
    }
    let t = dot(e2, q) * inv;
    (t >= 0.0 && t.is_finite()).then_some(t)
}

#[inline]
fn box_dist2(node: &Node, p: V3) -> f64 {
    let mut d2 = 0.0;
    for k in 0..3 {
        let d = (node.min[k] - p[k]).max(0.0).max(p[k] - node.max[k]);
        d2 += d * d;
    }
    d2
}

/// Closest point on triangle abc (Ericson, Real-Time Collision Detection
/// 5.1.5): Voronoi-region tests, no square roots.
fn closest_on_triangle(p: V3, a: V3, b: V3, c: V3) -> V3 {
    let ab = sub(b, a);
    let ac = sub(c, a);
    let ap = sub(p, a);
    let d1 = dot(ab, ap);
    let d2 = dot(ac, ap);
    if d1 <= 0.0 && d2 <= 0.0 {
        return a;
    }
    let bp = sub(p, b);
    let d3 = dot(ab, bp);
    let d4 = dot(ac, bp);
    if d3 >= 0.0 && d4 <= d3 {
        return b;
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let v = d1 / (d1 - d3);
        return add(a, mul(ab, v));
    }
    let cp = sub(p, c);
    let d5 = dot(ab, cp);
    let d6 = dot(ac, cp);
    if d6 >= 0.0 && d5 <= d6 {
        return c;
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let w = d2 / (d2 - d6);
        return add(a, mul(ac, w));
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        let w = (d4 - d3) / ((d4 - d3) + (d5 - d6));
        return add(b, mul(sub(c, b), w));
    }
    let sum = va + vb + vc;
    if !(sum.abs() > 0.0) || !sum.is_finite() {
        return a;
    }
    let v = vb / sum;
    let w = vc / sum;
    let q = add(a, add(mul(ab, v), mul(ac, w)));
    if finite3(q) {
        q
    } else {
        a
    }
}

// ---------------------------------------------------------------------------
// Linear algebra helpers.

/// Eigen-decomposition of a symmetric 3x3 by cyclic Jacobi rotations:
/// unconditionally stable and accurate for the tiny matrices used here.
/// Returns eigenvalues and matching unit eigenvectors.
fn jacobi_eigen(m: [[f64; 3]; 3]) -> ([f64; 3], [V3; 3]) {
    let mut a = m;
    let mut v = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    let scale: f64 = a.iter().flatten().map(|x| x * x).sum::<f64>();
    if !scale.is_finite() {
        return ([0.0; 3], [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
    }
    for _ in 0..64 {
        let off = a[0][1] * a[0][1] + a[0][2] * a[0][2] + a[1][2] * a[1][2];
        if off <= 1e-30 * scale || off == 0.0 {
            break;
        }
        for (p, q) in [(0, 1), (0, 2), (1, 2)] {
            if a[p][q] == 0.0 {
                continue;
            }
            let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
            let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
            let t = if t.is_finite() { t } else { 0.0 };
            let c = 1.0 / (t * t + 1.0).sqrt();
            let s = t * c;
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
                let (vkp, vkq) = (row[p], row[q]);
                row[p] = c * vkp - s * vkq;
                row[q] = s * vkp + c * vkq;
            }
        }
    }
    let vals = [a[0][0], a[1][1], a[2][2]];
    let vecs = [0, 1, 2].map(|i| unit([v[0][i], v[1][i], v[2][i]]));
    (vals, vecs)
}

/// Eigenpairs sorted by ascending eigenvalue.
fn eigen_sorted(m: [[f64; 3]; 3]) -> ([f64; 3], [V3; 3]) {
    let (vals, vecs) = jacobi_eigen(m);
    let mut idx = [0, 1, 2];
    idx.sort_by(|&i, &j| vals[i].total_cmp(&vals[j]));
    (idx.map(|i| vals[i]), idx.map(|i| vecs[i]))
}

/// Gaussian elimination with partial pivoting; `None` when singular.
fn solve_linear(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Option<Vec<f64>> {
    let n = b.len();
    let scale = a.iter().flatten().fold(0.0f64, |m, x| m.max(x.abs()));
    if !(scale > 0.0) || !scale.is_finite() {
        return None;
    }
    for col in 0..n {
        let pivot = (col..n).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
        if a[pivot][col].abs() <= 1e-13 * scale {
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
            b[row] -= f * b[col];
        }
    }
    let mut x = vec![0.0; n];
    for row in (0..n).rev() {
        let mut s = b[row];
        for k in row + 1..n {
            s -= a[row][k] * x[k];
        }
        x[row] = s / a[row][row];
    }
    x.iter().all(|v| v.is_finite()).then_some(x)
}

/// Weighted circle fit: algebraic (Kåsa) start, then a few Gauss-Newton
/// steps on the geometric residual, which removes Kåsa's bias on short arcs.
/// Points are (x, y, weight). Returns (cx, cy, r).
fn fit_circle(pts: &[(f64, f64, f64)]) -> Option<(f64, f64, f64)> {
    let wsum: f64 = pts.iter().map(|p| p.2).sum();
    if pts.len() < 3 || !(wsum > 0.0) {
        return None;
    }
    let mx = pts.iter().map(|p| p.0 * p.2).sum::<f64>() / wsum;
    let my = pts.iter().map(|p| p.1 * p.2).sum::<f64>() / wsum;
    let mut m = vec![vec![0.0; 3]; 3];
    let mut rhs = vec![0.0; 3];
    for &(x, y, w) in pts {
        let (x, y) = (x - mx, y - my);
        let row = [x, y, 1.0];
        let z = -(x * x + y * y);
        for i in 0..3 {
            for j in 0..3 {
                m[i][j] += w * row[i] * row[j];
            }
            rhs[i] += w * row[i] * z;
        }
    }
    let s = solve_linear(m, rhs)?;
    let (mut cx, mut cy) = (-s[0] / 2.0, -s[1] / 2.0);
    let r2 = cx * cx + cy * cy - s[2];
    if !(r2 > 0.0) {
        return None;
    }
    let mut r = r2.sqrt();
    for _ in 0..8 {
        let mut jtj = vec![vec![0.0; 3]; 3];
        let mut jtr = vec![0.0; 3];
        for &(x, y, w) in pts {
            let (dx, dy) = (x - mx - cx, y - my - cy);
            let d = (dx * dx + dy * dy).sqrt();
            if d < 1e-300 {
                continue;
            }
            let res = d - r;
            let j = [-dx / d, -dy / d, -1.0];
            for a in 0..3 {
                for b in 0..3 {
                    jtj[a][b] += w * j[a] * j[b];
                }
                jtr[a] -= w * j[a] * res;
            }
        }
        let Some(step) = solve_linear(jtj, jtr) else { break };
        cx += step[0];
        cy += step[1];
        r += step[2];
        if step.iter().map(|v| v.abs()).fold(0.0, f64::max) < 1e-12 * r.abs().max(1e-12) {
            break;
        }
    }
    (r > 0.0 && r.is_finite()).then_some((cx + mx, cy + my, r))
}

/// Weighted algebraic sphere fit. Points are (p, weight); returns (c, r).
fn fit_sphere(pts: &[(V3, f64)]) -> Option<(V3, f64)> {
    let wsum: f64 = pts.iter().map(|p| p.1).sum();
    if pts.len() < 4 || !(wsum > 0.0) {
        return None;
    }
    let mean = mul(pts.iter().fold([0.0; 3], |acc, (p, w)| add(acc, mul(*p, *w))), 1.0 / wsum);
    let mut m = vec![vec![0.0; 4]; 4];
    let mut rhs = vec![0.0; 4];
    for &(p, w) in pts {
        let q = sub(p, mean);
        let row = [q[0], q[1], q[2], 1.0];
        let z = -dot(q, q);
        for i in 0..4 {
            for j in 0..4 {
                m[i][j] += w * row[i] * row[j];
            }
            rhs[i] += w * row[i] * z;
        }
    }
    let s = solve_linear(m, rhs)?;
    let c = [-s[0] / 2.0, -s[1] / 2.0, -s[2] / 2.0];
    let r2 = dot(c, c) - s[3];
    (r2 > 0.0 && r2.is_finite()).then(|| (add(c, mean), r2.sqrt()))
}

/// Andrew's monotone chain; counter-clockwise, no repeated closing point.
fn convex_hull_2d(mut pts: Vec<[f64; 2]>) -> Vec<[f64; 2]> {
    pts.retain(|p| p[0].is_finite() && p[1].is_finite());
    pts.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    pts.dedup();
    if pts.len() < 3 {
        return pts;
    }
    let cross2 = |o: [f64; 2], a: [f64; 2], b: [f64; 2]| {
        (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
    };
    let mut hull: Vec<[f64; 2]> = Vec::with_capacity(pts.len() * 2);
    for &p in pts.iter().chain(pts.iter().rev().skip(1)) {
        while hull.len() >= 2 && cross2(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0 {
            hull.pop();
        }
        hull.push(p);
    }
    hull.pop();
    hull
}

fn polygon_area(points: &[[f64; 2]]) -> f64 {
    let n = points.len();
    if n < 3 {
        return 0.0;
    }
    let mut s = 0.0;
    for i in 0..n {
        let (a, b) = (points[i], points[(i + 1) % n]);
        s += a[0] * b[1] - a[1] * b[0];
    }
    0.5 * s
}

fn point_in_polygon(p: [f64; 2], poly: &[[f64; 2]]) -> bool {
    let mut inside = false;
    let n = poly.len();
    let mut j = n.wrapping_sub(1);
    for i in 0..n {
        let (a, b) = (poly[i], poly[j]);
        if (a[1] > p[1]) != (b[1] > p[1]) {
            let x = a[0] + (p[1] - a[1]) / (b[1] - a[1]) * (b[0] - a[0]);
            if p[0] < x {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

// ---------------------------------------------------------------------------
// describe

pub fn describe(mesh: &TriMesh, options: DescribeOptions) -> Description {
    let mut out = Description { triangles: mesh.triangles.len(), ..Default::default() };
    let prep = prepare(mesh);
    if prep.dropped > 0 {
        out.notes.push(format!(
            "Skipped {} invalid or collapsed triangles (bad indices, non-finite or repeated vertices)",
            prep.dropped
        ));
    }
    if prep.tris.is_empty() || !(prep.total_area > 0.0) {
        out.units = guess_units([0.0; 3]);
        out.notes.push("Empty mesh: no triangles with area to analyse".into());
        return out;
    }

    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for p in &prep.points {
        for k in 0..3 {
            min[k] = min[k].min(p[k]);
            max[k] = max[k].max(p[k]);
        }
    }
    let size = sub(max, min);
    let diag = norm(size).max(1e-12);
    out.bounds_min = min;
    out.bounds_max = max;
    out.size = size;
    out.surface_area = prep.total_area;

    // Each stage is bounded on its own; between them, a request that gave
    // up (deadline, disconnect) gets what is done so far, said so.
    macro_rules! stop_if_interrupted {
        () => {
            if let Some(reason) = crate::csg::bsp::interrupt_message() {
                out.notes.insert(0, format!("stopped early, analysis incomplete: {reason}"));
                out.bounds_min = round_v(out.bounds_min);
                out.bounds_max = round_v(out.bounds_max);
                out.size = round_v(out.size);
                out.center_of_mass = round_v(out.center_of_mass);
                return out;
            }
        };
    }
    stop_if_interrupted!();
    let topo = topology(&prep);
    out.watertight = topo.watertight();
    if topo.boundary_edges > 0 {
        out.notes.push(format!(
            "Open mesh: {} boundary loop{} ({} boundary edges); volume, inside tests and lay-flat stability are approximate",
            topo.boundary_loops,
            if topo.boundary_loops == 1 { "" } else { "s" },
            topo.boundary_edges
        ));
    }
    if topo.nonmanifold_edges > 0 {
        out.notes.push(format!(
            "{} non-manifold edges (shared by more than two triangles)",
            topo.nonmanifold_edges
        ));
    }
    if topo.misoriented_edges > 0 {
        out.notes.push(format!(
            "{} edges join triangles with inconsistent winding (some normals are flipped)",
            topo.misoriented_edges
        ));
    }

    // Volume and centre of mass by signed tetrahedra against the bounds
    // centre (closer origin = less cancellation error).
    let origin = mul(add(min, max), 0.5);
    let mut vol6 = 0.0;
    let mut com_acc = [0.0; 3];
    for t in 0..prep.tris.len() {
        let [a, b, c] = prep.corners(t).map(|p| sub(p, origin));
        let v = dot(a, cross(b, c));
        vol6 += v;
        com_acc = add(com_acc, mul(add(add(a, b), c), v));
    }
    let signed_volume = vol6 / 6.0;
    out.signed_volume = signed_volume;
    out.volume = signed_volume.abs();
    if signed_volume < 0.0 && out.watertight {
        out.notes.push("Negative volume: triangles wind inward (normals point into the part); flip them before use".into());
    }
    let surface_centroid = mul(
        (0..prep.tris.len()).fold([0.0; 3], |acc, t| add(acc, mul(prep.centroids[t], prep.areas[t]))),
        1.0 / prep.total_area,
    );
    let enclosed = signed_volume.abs() > 1e-9 * diag * diag * diag;
    let com = if enclosed { add(origin, mul(com_acc, 1.0 / (4.0 * vol6))) } else { surface_centroid };
    let com = if finite3(com) { com } else { surface_centroid };
    out.center_of_mass = com;

    out.units = guess_units(size);
    if out.units.suggested_scale != 1.0 {
        out.notes.push(format!(
            "Looks like {}: multiply by {} for millimetres (size-based guess)",
            match out.units.likely.as_str() {
                "m" => "metres",
                "cm" => "centimetres",
                "in" => "inches",
                other => other,
            },
            out.units.suggested_scale
        ));
    }

    stop_if_interrupted!();
    out.principal_axes = principal_axes(&prep, surface_centroid);
    let ext = out.principal_axes.extents;
    if ext[0] > 2.0 * ext[1] {
        out.notes.push(
            "Elongated body: principalAxes is a surface-PCA box; for the true centerline of a tapered/flared body use fit_axis"
                .into(),
        );
    }
    stop_if_interrupted!();
    out.lay_flat = lay_flat(&prep, &out.principal_axes, com, diag, options.max_lay_flat);
    if out.lay_flat.is_empty() && options.max_lay_flat > 0 {
        out.notes.push("No flat face to rest on: needs supports or a flat cut to print".into());
    } else if out.lay_flat.first().is_some_and(|l| !l.stable) {
        out.notes.push("No stable resting face found: centre of mass overhangs every flat face".into());
    }

    stop_if_interrupted!();
    let index = MeshIndex::from_prep(&prep);
    out.symmetry = symmetry(&prep, &index, surface_centroid, &out.principal_axes, diag);

    stop_if_interrupted!();
    if options.features && options.max_features > 0 {
        let (features, found) = detect_features(&prep, &topo, diag, options.max_features);
        if found > features.len() {
            out.notes.push(format!(
                "{found} surface features found, listing the {} largest",
                features.len()
            ));
        }
        out.features = features;
    }

    out.bounds_min = round_v(out.bounds_min);
    out.bounds_max = round_v(out.bounds_max);
    out.size = round_v(out.size);
    out.center_of_mass = round_v(out.center_of_mass);
    out
}

/// Units from size alone. A file carries no units, so this is only a guess
/// from what is plausible for a 3D-printed part, and says so.
fn guess_units(size: V3) -> UnitsGuess {
    let l = size.iter().fold(0.0f64, |m, v| m.max(*v));
    let guess = |likely: &str, scale: f64, reason: String| UnitsGuess {
        likely: likely.into(),
        suggested_scale: scale,
        reason,
    };
    if !(l > 0.0) || !l.is_finite() {
        return guess("mm", 1.0, "No extent to judge from; assuming millimetres".into());
    }
    if l < 0.5 {
        guess(
            "m",
            1000.0,
            format!(
                "Largest extent is {l:.4} units: under half a millimetre if read as mm, which is implausible for a printed part, but {:.0} mm if read as metres (heuristic; confirm with the user)",
                l * 1000.0
            ),
        )
    } else if l < 1.0 {
        guess(
            "cm",
            10.0,
            format!(
                "Largest extent is {l:.3} units: under 1 mm if read as mm; as centimetres it is {:.1} mm (as inches it would be {:.1} mm). Heuristic; confirm with the user",
                l * 10.0,
                l * 25.4
            ),
        )
    } else if l <= 2000.0 {
        let mut reason = format!(
            "Largest extent is {l:.2} units, a plausible size in millimetres for a printed part"
        );
        if l < 10.0 {
            reason.push_str(&format!(
                "; it is small, so if it was modelled in inches scale by 25.4 ({:.0} mm) or in centimetres by 10",
                l * 25.4
            ));
        }
        guess("mm", 1.0, reason)
    } else {
        guess(
            "mm",
            1.0,
            format!(
                "Largest extent is {l:.0} units: larger than any desktop printer if read as mm; if this is a small part the file may be in micrometres (scale 0.001)"
            ),
        )
    }
}

/// Oriented-box extents along `axes`: (min, max) projections.
fn obb_projection(prep: &Prep, axes: &[V3; 3]) -> ([f64; 3], [f64; 3]) {
    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    for p in &prep.points {
        for k in 0..3 {
            let d = dot(*p, axes[k]);
            lo[k] = lo[k].min(d);
            hi[k] = hi[k].max(d);
        }
    }
    (lo, hi)
}

fn principal_axes(prep: &Prep, mu: V3) -> PrincipalAxes {
    // Covariance of the surface as a uniform area density: each triangle
    // contributes its exact second moment (sum of corner outer products plus
    // 9 c c^T, over 12), so tessellation density does not bias the axes the
    // way a vertex covariance would.
    let mut m = [[0.0; 3]; 3];
    for t in 0..prep.tris.len() {
        let w = prep.areas[t] / 12.0;
        if w <= 0.0 {
            continue;
        }
        let corners = prep.corners(t).map(|p| sub(p, mu));
        let c = sub(prep.centroids[t], mu);
        for r in 0..3 {
            for s in 0..3 {
                let mut acc = 9.0 * c[r] * c[s];
                for p in &corners {
                    acc += p[r] * p[s];
                }
                m[r][s] += w * acc;
            }
        }
    }
    let m = m.map(|row| row.map(|x| x / prep.total_area));
    let (_, pca) = jacobi_eigen(m);
    let world = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    let box_area = |axes: &[V3; 3]| {
        let (lo, hi) = obb_projection(prep, axes);
        let e = sub(hi, lo);
        e[0] * e[1] + e[1] * e[2] + e[0] * e[2]
    };
    // PCA axes are ill-defined for shapes with repeated eigenvalues (cubes,
    // cylinders); a part already aligned to the world frame keeps clean
    // world axes unless PCA gives a clearly tighter box. Surface area of the
    // box (not volume) so flat parts compare sensibly.
    let pca_ok = pca.iter().all(|a| !is_zero(*a));
    let mut axes = if pca_ok && box_area(&pca) < box_area(&world) * 0.99 { pca } else { world };
    let (lo, hi) = obb_projection(prep, &axes);
    let ext = sub(hi, lo);
    let mut idx = [0, 1, 2];
    idx.sort_by(|&i, &j| ext[j].total_cmp(&ext[i]));
    axes = idx.map(|i| axes[i]);
    let a0 = canonical(axes[0]);
    let a1 = canonical(axes[1]);
    let a2 = unit(cross(a0, a1));
    let axes = [a0, a1, a2];
    let (lo, hi) = obb_projection(prep, &axes);
    let center = (0..3).fold([0.0; 3], |acc, k| add(acc, mul(axes[k], 0.5 * (lo[k] + hi[k]))));
    let mut align = [[0.0; 4]; 4];
    for k in 0..3 {
        align[k] = [axes[k][0], axes[k][1], axes[k][2], -dot(axes[k], center)];
    }
    align[3] = [0.0, 0.0, 0.0, 1.0];
    PrincipalAxes { center: mu, axes, extents: sub(hi, lo), align_matrix: align }
}

/// Resting-face candidates. Flat faces are found by clustering triangle
/// normals (area-weighted, on a cube-map grid) and refining each cluster's
/// mean normal; the oriented-box axes are added so blocky parts always get
/// their six sides considered. A candidate counts only if it is a
/// supporting plane: the face lies on the extreme of the part along its
/// normal, so the part could actually sit on it. Contact area is the flat
/// area touching the bed; stability uses the convex hull of all contact
/// points (a four-legged table is stable though its legs touch little).
fn lay_flat(prep: &Prep, axes: &PrincipalAxes, com: V3, diag: f64, max: usize) -> Vec<LayFlat> {
    if max == 0 {
        return Vec::new();
    }
    const GRID: usize = 12;
    let bucket = |n: V3| -> usize {
        let k = (0..3).max_by(|&i, &j| n[i].abs().total_cmp(&n[j].abs())).unwrap_or(0);
        let s = usize::from(n[k] < 0.0);
        let (i, j) = ((k + 1) % 3, (k + 2) % 3);
        let cell = |x: f64| (((x / n[k].abs() + 1.0) * 0.5 * GRID as f64) as usize).min(GRID - 1);
        ((k * 2 + s) * GRID + cell(n[i])) * GRID + cell(n[j])
    };
    let mut buckets = vec![(0.0f64, [0.0f64; 3]); 6 * GRID * GRID];
    for t in 0..prep.tris.len() {
        let (n, a) = (prep.normals[t], prep.areas[t]);
        if a > 0.0 && !is_zero(n) {
            let b = &mut buckets[bucket(n)];
            b.0 += a;
            b.1 = add(b.1, mul(n, a));
        }
    }
    let mut order: Vec<usize> = (0..buckets.len()).filter(|&b| buckets[b].0 > 0.0).collect();
    order.sort_by(|&a, &b| buckets[b].0.total_cmp(&buckets[a].0));
    let cos5 = 5f64.to_radians().cos();
    let mut candidates: Vec<V3> = Vec::new();
    for &b in order.iter().take(24) {
        let seed = unit(buckets[b].1);
        // Refine across bucket borders: a face whose normal sits on a cell
        // edge would otherwise be split between two buckets.
        let mut acc = [0.0; 3];
        for t in 0..prep.tris.len() {
            if dot(prep.normals[t], seed) > cos5 {
                acc = add(acc, mul(prep.normals[t], prep.areas[t]));
            }
        }
        let n = unit(acc);
        if !is_zero(n) {
            candidates.push(n);
        }
    }
    for a in axes.axes {
        candidates.push(a);
        candidates.push(mul(a, -1.0));
    }
    let cos3 = 3f64.to_radians().cos();
    let mut unique: Vec<V3> = Vec::new();
    for n in candidates {
        if !is_zero(n) && unique.iter().all(|u| dot(*u, n) < cos3) {
            unique.push(n);
        }
    }

    // Contact = within eps of the bed and facing it within 5 degrees: tight
    // enough that a curved surface merely tangent to the bed (a torus, a
    // cylinder on its side) reports little area, loose enough for scan noise.
    let eps = (diag * 1e-3).max(1e-9);
    let cos5n = 5f64.to_radians().cos();
    let mut results = Vec::new();
    for n in unique {
        let h = prep.points.iter().fold(f64::NEG_INFINITY, |m, p| m.max(dot(*p, n)));
        let mut contact_area = 0.0;
        for t in 0..prep.tris.len() {
            if dot(prep.normals[t], n) > cos5n && prep.corners(t).iter().all(|p| dot(*p, n) >= h - eps) {
                contact_area += prep.areas[t];
            }
        }
        if !(contact_area > prep.total_area * 1e-6) {
            continue;
        }
        let r2 = mul(n, -1.0);
        let mut r0 = [0.0; 3];
        for a in axes.axes {
            r0 = unit(perp(a, n));
            if norm(perp(a, n)) > 0.1 {
                break;
            }
        }
        if is_zero(r0) {
            r0 = plane_basis(n).0;
        }
        let r1 = cross(r2, r0);
        let mut lo = [f64::INFINITY; 3];
        let mut hi = [f64::NEG_INFINITY; 3];
        let mut contact = Vec::new();
        for p in &prep.points {
            let q = [dot(*p, r0), dot(*p, r1), dot(*p, r2)];
            for k in 0..3 {
                lo[k] = lo[k].min(q[k]);
                hi[k] = hi[k].max(q[k]);
            }
            if dot(*p, n) >= h - eps {
                contact.push([q[0], q[1]]);
            }
        }
        let hull = convex_hull_2d(contact);
        let c2 = [dot(com, r0), dot(com, r1)];
        let stable = hull.len() >= 3 && {
            let tol = 1e-9 * diag * diag;
            (0..hull.len()).all(|i| {
                let (a, b) = (hull[i], hull[(i + 1) % hull.len()]);
                (b[0] - a[0]) * (c2[1] - a[1]) - (b[1] - a[1]) * (c2[0] - a[0]) >= -tol
            })
        };
        let t = [-(lo[0] + hi[0]) * 0.5, -(lo[1] + hi[1]) * 0.5, -lo[2]];
        let rows = [r0, r1, r2];
        let mut matrix = [[0.0; 4]; 4];
        for k in 0..3 {
            matrix[k] = [rows[k][0], rows[k][1], rows[k][2], t[k]];
        }
        matrix[3] = [0.0, 0.0, 0.0, 1.0];
        results.push(LayFlat {
            normal: round_v(n),
            contact_area,
            matrix,
            height: hi[2] - lo[2],
            stable,
        });
    }
    results.sort_by(|a, b| b.stable.cmp(&a.stable).then(b.contact_area.total_cmp(&a.contact_area)));
    results.truncate(max);
    results
}

/// Deterministic area-weighted surface samples (stratified over cumulative
/// area, low-discrepancy barycentrics) so repeated describes agree exactly.
fn surface_samples(prep: &Prep, count: usize) -> Vec<V3> {
    let mut cum = Vec::with_capacity(prep.tris.len());
    let mut acc = 0.0;
    for &a in &prep.areas {
        acc += a;
        cum.push(acc);
    }
    if !(acc > 0.0) {
        return Vec::new();
    }
    (0..count)
        .map(|k| {
            let target = (k as f64 + 0.5) / count as f64 * acc;
            let t = cum.partition_point(|&c| c < target).min(prep.tris.len() - 1);
            let mut r1 = (k as f64 * 0.754_877_666_246_692_7).fract();
            let mut r2 = (k as f64 * 0.569_840_290_998_053_3).fract();
            if r1 + r2 > 1.0 {
                r1 = 1.0 - r1;
                r2 = 1.0 - r2;
            }
            let [a, b, c] = prep.corners(t);
            add(a, add(mul(sub(b, a), r1), mul(sub(c, a), r2)))
        })
        .collect()
}

/// Mirror-symmetry plane: score = mean distance from mirrored surface
/// samples back to the surface. Candidates are planes through the surface
/// centroid normal to each principal and world axis; the best is refined by
/// a shrinking local search over tilt and offset.
fn symmetry(
    prep: &Prep,
    index: &MeshIndex,
    center: V3,
    axes: &PrincipalAxes,
    diag: f64,
) -> Option<Symmetry> {
    let samples = surface_samples(prep, 768);
    if samples.is_empty() {
        return None;
    }
    // `limit` lets a candidate bail out once it cannot beat the best.
    let score = |p: V3, n: V3, limit: f64| -> f64 {
        let budget = limit * samples.len() as f64;
        let mut sum = 0.0;
        for s in &samples {
            let m = sub(*s, mul(n, 2.0 * dot(sub(*s, p), n)));
            sum += index.distance(m);
            if sum > budget {
                return f64::INFINITY;
            }
        }
        sum / samples.len() as f64
    };
    let mut normals: Vec<V3> = Vec::new();
    for n in axes.axes.iter().copied().chain([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]) {
        if !is_zero(n) && normals.iter().all(|m| dot(*m, n).abs() < 0.999) {
            normals.push(n);
        }
    }
    let mut best = (f64::INFINITY, center, [0.0; 3]);
    for n in normals {
        let s = score(center, n, best.0);
        if s < best.0 {
            best = (s, center, n);
        }
    }
    if !best.0.is_finite() || best.0 > 0.05 * diag {
        return None;
    }
    let (mut dev, mut p, mut n) = best;
    let mut angle = 2f64.to_radians();
    let mut offset = 0.005 * diag;
    for _ in 0..24 {
        if dev <= 1e-9 * diag || angle < 0.02f64.to_radians() {
            break;
        }
        let (t1, t2) = plane_basis(n);
        let mut improved = false;
        let tries = [
            (unit(add(n, mul(t1, angle.tan()))), p),
            (unit(sub(n, mul(t1, angle.tan()))), p),
            (unit(add(n, mul(t2, angle.tan()))), p),
            (unit(sub(n, mul(t2, angle.tan()))), p),
            (n, add(p, mul(n, offset))),
            (n, sub(p, mul(n, offset))),
        ];
        for (cn, cp) in tries {
            let s = score(cp, cn, dev);
            if s < dev {
                dev = s;
                n = cn;
                p = cp;
                improved = true;
            }
        }
        if !improved {
            angle *= 0.5;
            offset *= 0.5;
        }
    }
    (dev <= 0.005 * diag).then(|| Symmetry {
        point: round_v(p),
        normal: round_v(canonical(n)),
        deviation: round6(dev),
    })
}

// ---------------------------------------------------------------------------
// Feature detection: hypothesise from a seed triangle and its neighbours,
// grow a region over edge adjacency while triangles fit, refit, regrow,
// accept only good fits. Region growing (rather than global RANSAC) is what
// keeps this fast and robust on scans: noise only has to be locally small.
// Order matters: spheres first (a sphere's equator band would otherwise pass
// as a short cylinder), then cylinders (a fine cylinder's adjacent facets
// would otherwise pass as small planes), then planes on what is left.

struct Grower {
    stamp: Vec<u32>,
    generation: u32,
    budget: usize,
}

impl Grower {
    fn grow(
        &mut self,
        topo: &Topo,
        available: &[bool],
        seeds: &[usize],
        accept: impl Fn(usize) -> bool,
    ) -> Vec<usize> {
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.stamp.fill(0);
            self.generation = 1;
        }
        let g = self.generation;
        let mut out = Vec::new();
        let mut stack = Vec::new();
        for &s in seeds {
            if self.stamp[s] != g {
                self.stamp[s] = g;
                if available[s] && accept(s) {
                    stack.push(s);
                }
            }
        }
        while let Some(t) = stack.pop() {
            out.push(t);
            self.budget = self.budget.saturating_sub(1);
            for &nb in &topo.neighbors[t] {
                if nb == NONE {
                    continue;
                }
                let nb = nb as usize;
                if self.stamp[nb] == g || !available[nb] {
                    continue;
                }
                self.stamp[nb] = g;
                if accept(nb) {
                    stack.push(nb);
                }
            }
        }
        out
    }
}

struct Detector<'a> {
    prep: &'a Prep,
    topo: &'a Topo,
    diag: f64,
    min_area: f64,
    available: Vec<bool>,
    grower: Grower,
    order: Vec<usize>,
}

struct CylFit {
    axis: V3,
    center: V3,
    axis_point: V3,
    radius: f64,
    rms: f64,
    area: f64,
    hole: bool,
    coverage: f64,
    length: f64,
    /// Smallest eigenvalue of the normal covariance over total area: ~0 when
    /// every normal is perpendicular to the axis.
    flatness: f64,
    /// Fraction of residual variance explained by position (0 = pure noise).
    systematic: f64,
}

/// Cone r(z) = r0 + k z about the line through `center` along `axis`, z
/// measured from `center` (the region's mean axial position).
#[derive(Clone)]
struct ConeFit {
    axis: V3,
    center: V3,
    r0: f64,
    k: f64,
    /// Perpendicular (geometric) rms distance to the cone.
    rms: f64,
    area: f64,
    hole: bool,
    coverage: f64,
    zmin: f64,
    zmax: f64,
    /// Smallest eigenvalue of the centred normal covariance over area: ~0
    /// when every normal makes the same angle with the axis.
    flatness: f64,
    /// As `CylFit::systematic`.
    systematic: f64,
}

impl ConeFit {
    fn radius_at(&self, z: f64) -> f64 {
        self.r0 + self.k * z
    }
    fn mean_radius(&self) -> f64 {
        self.radius_at(0.5 * (self.zmin + self.zmax))
    }
    fn half_angle_deg(&self) -> f64 {
        self.k.atan().to_degrees()
    }
    fn feature(&self) -> Feature {
        // Wide end first: flip the axis when the radius grows along it.
        let (axis, z_start, z_end) =
            if self.k > 0.0 { (mul(self.axis, -1.0), self.zmax, self.zmin) } else { (self.axis, self.zmin, self.zmax) };
        let apex = if self.k.abs() > 1e-12 { add(self.center, mul(self.axis, -self.r0 / self.k)) } else { self.center };
        Feature::Cone {
            apex: round_v(apex),
            axis: round_v(axis),
            axis_point: round_v(add(self.center, mul(self.axis, 0.5 * (self.zmin + self.zmax)))),
            half_angle_deg: round6(self.half_angle_deg().abs()),
            radius_start: round6(self.radius_at(z_start)),
            radius_end: round6(self.radius_at(z_end)),
            length: round6(self.zmax - self.zmin),
            hole: self.hole,
            coverage_deg: self.coverage,
            rms: round6(self.rms),
        }
    }
}

/// Weighted least squares of residuals `e` against a smooth function of
/// (axial position, angle): the fraction of residual variance explained.
/// Noise leaves ~0; a wrong surface model leaves structure (high).
fn systematic_fraction(samples: &[(f64, f64, f64, f64)]) -> f64 {
    let mut m = vec![vec![0.0; 9]; 9];
    let mut rhs = vec![0.0; 9];
    let mut se = 0.0;
    let feats = |z: f64, th: f64| {
        let (st, ct) = th.sin_cos();
        [1.0, ct, st, z * ct, z * st, z * z * ct, z * z * st, z, z * z]
    };
    for &(z, th, e, w) in samples {
        let f = feats(z, th);
        for i in 0..9 {
            for j in 0..9 {
                m[i][j] += w * f[i] * f[j];
            }
            rhs[i] += w * f[i] * e;
        }
        se += w * e * e;
    }
    if !(se > 0.0) {
        return 0.0;
    }
    let Some(c) = solve_linear(m, rhs) else { return 0.0 };
    let left: f64 = samples
        .iter()
        .map(|&(z, th, e, w)| {
            let pred: f64 = feats(z, th).iter().zip(&c).map(|(a, b)| a * b).sum();
            w * (e - pred).powi(2)
        })
        .sum();
    (1.0 - left / se).clamp(0.0, 1.0)
}

struct SphereFit {
    center: V3,
    radius: f64,
    rms: f64,
    area: f64,
    hole: bool,
    spread: f64,
    /// Smallest eigenvalue of the *centred* normal covariance over area:
    /// ~0 when every normal makes one angle with some axis, as on a cone or
    /// cylinder band (whose two rims always lie on some sphere).
    zonal: f64,
}

const MAX_ATTEMPTS: usize = 2500;
/// Cone hypotheses are costlier (6-parameter fit, several growth rounds) and
/// only tried where a wide arc was found, so they get their own cap.
const MAX_CONE_ATTEMPTS: usize = 400;

impl<'a> Detector<'a> {
    fn region_area(&self, region: &[usize]) -> f64 {
        region.iter().map(|&t| self.prep.areas[t]).sum()
    }

    /// Growth tolerance for curved fits. Accepted fits must reach half of
    /// it in rms: growth stops at the tolerance, so a wrong model (a bent
    /// tube read as a cylinder) still fits each triangle within it but
    /// leaves a systematic rms near it, while true surfaces leave only noise.
    fn cyl_tol(&self, r: f64) -> f64 {
        (0.001 * self.diag).max(0.01 * r)
    }

    /// Edge neighbours and their neighbours whose normals differ from the
    /// seed's by a curvature-sized angle: 2–45° excludes coplanar
    /// neighbours (no curvature information) and sharp edges (two different
    /// surfaces). Largest angle first: best conditioned.
    fn partners(&self, seed: usize) -> Vec<usize> {
        let n = self.prep.normals[seed];
        let (lo, hi) = (45f64.to_radians().cos(), 2f64.to_radians().cos());
        let mut out: Vec<(f64, usize)> = Vec::new();
        let consider = |t: usize, out: &mut Vec<(f64, usize)>| {
            if t == seed || !self.available[t] || self.prep.areas[t] <= 0.0 {
                return;
            }
            let c = dot(self.prep.normals[t], n);
            if c >= lo && c <= hi && out.iter().all(|&(_, u)| u != t) {
                out.push((c, t));
            }
        };
        for &a in &self.topo.neighbors[seed] {
            if a == NONE {
                continue;
            }
            consider(a as usize, &mut out);
            for &b in &self.topo.neighbors[a as usize] {
                if b != NONE {
                    consider(b as usize, &mut out);
                }
            }
        }
        out.sort_by(|a, b| a.0.total_cmp(&b.0));
        out.into_iter().map(|(_, t)| t).collect()
    }

    fn cyl_accept(&self, t: usize, axis: V3, c: V3, r: f64, sign: f64, tol: f64) -> bool {
        let n = self.prep.normals[t];
        if is_zero(n) || dot(n, axis).abs() > 20f64.to_radians().sin() {
            return false;
        }
        for p in self.prep.corners(t) {
            if (norm(perp(sub(p, c), axis)) - r).abs() > tol {
                return false;
            }
        }
        let radial = unit(perp(sub(self.prep.centroids[t], c), axis));
        dot(unit(perp(n, axis)), radial) * sign >= 25f64.to_radians().cos()
    }

    /// Whether a fitted region is evidence of a real cylinder.
    ///
    /// A smooth doubly-curved surface (a lumpy scan) is locally cylinder-like
    /// along its parabolic lines, where one principal curvature passes
    /// through zero: a short arc there fits a cylinder within scan noise.
    /// Real cylinders show it over a wide arc or fit cleanly; so a short arc
    /// (under 90 degrees) must leave a clearly sub-tolerance rms and little
    /// residual structure, while a wide one gets the looser bounds. On real
    /// scanned cylinders the residual is noise (systematic ~0.02); fits on
    /// curved freeform patches leave 0.2-0.5 of it explained by position.
    fn cyl_ok(&self, f: &CylFit) -> bool {
        let tol = self.cyl_tol(f.radius);
        let short_arc = f.coverage < 90.0;
        f.area >= self.min_area
            && f.radius <= self.diag
            && f.rms <= if short_arc { 0.3 } else { 0.5 } * tol
            && f.flatness < 0.03
            && f.systematic < if short_arc { 0.25 } else { 0.3 }
            && f.coverage >= 45.0
    }

    fn fit_cylinder(&self, region: &[usize], hint: V3) -> Option<CylFit> {
        let prep = self.prep;
        let area = self.region_area(region);
        if !(area > 0.0) {
            return None;
        }
        let mut nn = [[0.0; 3]; 3];
        for &t in region {
            let (n, a) = (prep.normals[t], prep.areas[t]);
            for r in 0..3 {
                for s in 0..3 {
                    nn[r][s] += a * n[r] * n[s];
                }
            }
        }
        let (vals, vecs) = eigen_sorted(nn);
        // With little arc the normals barely span a plane and the smallest
        // eigenvector is unreliable; keep the hypothesis axis then.
        let axis = canonical(unit(if vals[1] > 0.02 * area { vecs[0] } else { hint }));
        if is_zero(axis) {
            return None;
        }
        let (e1, e2) = plane_basis(axis);
        let mut pts = Vec::with_capacity(region.len() * 3);
        let (mut zmin, mut zmax) = (f64::INFINITY, f64::NEG_INFINITY);
        for &t in region {
            for p in prep.corners(t) {
                pts.push((dot(p, e1), dot(p, e2), prep.areas[t]));
                let z = dot(p, axis);
                zmin = zmin.min(z);
                zmax = zmax.max(z);
            }
        }
        let (cx, cy, radius) = fit_circle(&pts)?;
        let center = add(mul(e1, cx), mul(e2, cy));
        let mut se = 0.0;
        let mut wsum = 0.0;
        let mut bins = [false; 36];
        for &(x, y, w) in &pts {
            let d = (x - cx).hypot(y - cy);
            se += w * (d - radius) * (d - radius);
            wsum += w;
            let ang = (y - cy).atan2(x - cx).to_degrees().rem_euclid(360.0);
            bins[((ang / 10.0) as usize).min(35)] = true;
        }
        let mut facing = 0.0;
        for &t in region {
            let radial = unit(perp(sub(prep.centroids[t], center), axis));
            facing += prep.areas[t] * dot(prep.normals[t], radial);
        }
        // Systematic misfit: how much of the residual a smooth function of
        // (axial position, angle) explains. Noise is unexplained (R^2 ~ 0);
        // a bent tube, cone or torus patch read as a cylinder leaves residual
        // that varies with position along the axis (R^2 high), even when its
        // rms is as small as scan noise.
        let rms = (se / wsum).sqrt();
        let half = 0.5 * (zmax - zmin);
        let systematic = if rms > 1e-9 * self.diag && half > 0.0 {
            let zc = 0.5 * (zmin + zmax);
            let mut m = vec![vec![0.0; 9]; 9];
            let mut rhs = vec![0.0; 9];
            let mut samples = Vec::with_capacity(pts.len());
            let mut k = 0;
            for &t in region {
                for p in prep.corners(t) {
                    let (x, y, w) = pts[k];
                    k += 1;
                    let z = (dot(p, axis) - zc) / half;
                    let th = (y - cy).atan2(x - cx);
                    let (st, ct) = th.sin_cos();
                    let f = [1.0, ct, st, z * ct, z * st, z * z * ct, z * z * st, z, z * z];
                    let e = (x - cx).hypot(y - cy) - radius;
                    for i in 0..9 {
                        for j in 0..9 {
                            m[i][j] += w * f[i] * f[j];
                        }
                        rhs[i] += w * f[i] * e;
                    }
                    samples.push((f, e, w));
                }
            }
            match solve_linear(m, rhs) {
                Some(c) => {
                    let left: f64 = samples
                        .iter()
                        .map(|(f, e, w)| {
                            let pred: f64 = f.iter().zip(&c).map(|(a, b)| a * b).sum();
                            w * (e - pred).powi(2)
                        })
                        .sum();
                    (1.0 - left / se.max(1e-300)).clamp(0.0, 1.0)
                }
                None => 0.0,
            }
        } else {
            0.0
        };
        Some(CylFit {
            systematic,
            axis,
            center,
            axis_point: add(center, mul(axis, 0.5 * (zmin + zmax))),
            radius,
            rms,
            area,
            hole: facing < 0.0,
            coverage: bins.iter().filter(|b| **b).count() as f64 * 10.0,
            length: zmax - zmin,
            flatness: vals[0].max(0.0) / area,
        })
    }

    fn cone_accept(&self, t: usize, f: &ConeFit, sign: f64, tol: f64) -> bool {
        let n = self.prep.normals[t];
        if is_zero(n) {
            return false;
        }
        let cos_a = 1.0 / (1.0 + f.k * f.k).sqrt();
        for p in self.prep.corners(t) {
            let q = sub(p, f.center);
            let z = dot(q, f.axis);
            let r = f.radius_at(z);
            if !(r > 0.0) || ((norm(perp(q, f.axis)) - r) * cos_a).abs() > tol {
                return false;
            }
        }
        let e = unit(perp(sub(self.prep.centroids[t], f.center), f.axis));
        if is_zero(e) {
            return false;
        }
        dot(n, unit(sub(e, mul(f.axis, f.k)))) * sign >= 25f64.to_radians().cos()
    }

    fn cone_ok(&self, f: &ConeFit) -> bool {
        let tol = self.cyl_tol(f.mean_radius());
        f.area >= self.min_area
            && f.mean_radius() <= self.diag
            && f.radius_at(f.zmin) > 0.0
            && f.radius_at(f.zmax) > 0.0
            && (0.5..=60.0).contains(&f.half_angle_deg().abs())
            && f.rms <= 0.5 * tol
            && f.flatness < 0.03
            && f.systematic < 0.3
            && f.coverage >= 150.0
    }

    /// Levenberg-Marquardt fit of a cone to the corners of `region`
    /// (area-weighted), from the given axis, axis point, radius there and
    /// slope. Parameters: axis-point shift and axis tilt (two each, across
    /// the axis), radius, slope. Uses at most `max_points` corners (evenly
    /// strided) so trial fits stay cheap; rms, extent and coverage always use
    /// every corner, the costlier `systematic`/`flatness` only with `stats`.
    #[allow(clippy::too_many_arguments)]
    fn fit_cone(
        &self,
        region: &[usize],
        axis: V3,
        center: V3,
        r0: f64,
        k: f64,
        max_points: usize,
        stats: bool,
    ) -> Option<ConeFit> {
        let prep = self.prep;
        let area = self.region_area(region);
        if !(area > 0.0) || region.len() < 3 {
            return None;
        }
        let stride = (region.len() * 3).div_ceil(max_points.max(1)).max(1);
        let mut pts: Vec<(V3, f64)> = Vec::with_capacity(region.len() * 3 / stride + 1);
        let mut i = 0usize;
        for &t in region {
            for p in prep.corners(t) {
                if i % stride == 0 {
                    pts.push((p, prep.areas[t]));
                }
                i += 1;
            }
        }
        let wsum: f64 = pts.iter().map(|p| p.1).sum();
        if pts.len() < 6 || !(wsum > 0.0) {
            return None;
        }
        let recentre = |a: V3, c: V3, r0: f64, k: f64| {
            let zm = pts.iter().map(|(p, w)| w * dot(sub(*p, c), a)).sum::<f64>() / wsum;
            (add(c, mul(a, zm)), r0 + k * zm)
        };
        let cost = |a: V3, c: V3, r0: f64, k: f64| {
            pts.iter()
                .map(|(p, w)| {
                    let q = sub(*p, c);
                    let e = norm(perp(q, a)) - r0 - k * dot(q, a);
                    w * e * e
                })
                .sum::<f64>()
        };
        let mut a = unit(axis);
        if is_zero(a) || !finite3(center) || !r0.is_finite() || !k.is_finite() {
            return None;
        }
        let (mut c, mut r0) = recentre(a, center, r0, k);
        let mut k = k;
        let mut current = cost(a, c, r0, k);
        let mut lambda = 1e-3;
        for _ in 0..25 {
            let (e1, e2) = plane_basis(a);
            let mut jtj = [[0.0; 6]; 6];
            let mut jtr = [0.0; 6];
            for (p, w) in &pts {
                let q = sub(*p, c);
                let z = dot(q, a);
                let wv = sub(q, mul(a, z));
                let rho = norm(wv);
                if rho < 1e-12 {
                    continue;
                }
                let e = mul(wv, 1.0 / rho);
                let res = rho - r0 - k * z;
                let (ex, ey) = (dot(e, e1), dot(e, e2));
                let g = z + k * rho;
                let j = [-ex, -ey, -g * ex, -g * ey, -1.0, -z];
                for r in 0..6 {
                    for s2 in 0..6 {
                        jtj[r][s2] += w * j[r] * j[s2];
                    }
                    jtr[r] += w * j[r] * res;
                }
            }
            let mut stepped = false;
            while lambda < 1e10 {
                let mut m: Vec<Vec<f64>> = jtj.iter().map(|row| row.to_vec()).collect();
                for d in 0..6 {
                    m[d][d] += lambda * jtj[d][d].max(1e-12 * wsum);
                }
                let Some(step) = solve_linear(m, jtr.iter().map(|x| -x).collect()) else {
                    lambda *= 10.0;
                    continue;
                };
                let na = unit(add(a, add(mul(e1, step[2]), mul(e2, step[3]))));
                let nc = add(c, add(mul(e1, step[0]), mul(e2, step[1])));
                let (nc, nr0) = recentre(na, nc, r0 + step[4], k + step[5]);
                let nk = k + step[5];
                let next = cost(na, nc, nr0, nk);
                if next.is_finite() && !is_zero(na) && next <= current {
                    let small = step.iter().map(|x| x.abs()).fold(0.0, f64::max) < 1e-10 * (1.0 + r0.abs());
                    let gain = current - next;
                    (a, c, r0, k, current) = (na, nc, nr0, nk, next);
                    lambda = (lambda / 3.0).max(1e-12);
                    stepped = !small && gain > 1e-7 * current.max(1e-300);
                    break;
                }
                lambda *= 4.0;
            }
            if !stepped {
                break;
            }
        }
        // Statistics over every corner of the region (not the subsample).
        let cos_a = 1.0 / (1.0 + k * k).sqrt();
        let (e1, e2) = plane_basis(a);
        let (mut zmin, mut zmax) = (f64::INFINITY, f64::NEG_INFINITY);
        let mut bins = [false; 36];
        let mut samples = Vec::with_capacity(region.len() * 3);
        let mut se = 0.0;
        let mut ws = 0.0;
        for &t in region {
            let w = prep.areas[t];
            for p in prep.corners(t) {
                let q = sub(p, c);
                let z = dot(q, a);
                let wv = perp(q, a);
                let e = (norm(wv) - r0 - k * z) * cos_a;
                se += w * e * e;
                ws += w;
                zmin = zmin.min(z);
                zmax = zmax.max(z);
                let th = dot(wv, e2).atan2(dot(wv, e1));
                bins[((th.to_degrees().rem_euclid(360.0) / 10.0) as usize).min(35)] = true;
                if stats {
                    samples.push((z, th, e, w));
                }
            }
        }
        let half = 0.5 * (zmax - zmin);
        let zc = 0.5 * (zmin + zmax);
        if half > 0.0 {
            samples.iter_mut().for_each(|s| s.0 = (s.0 - zc) / half);
        }
        let rms = (se / ws.max(1e-300)).sqrt();
        let systematic = if stats && rms > 1e-9 * self.diag { systematic_fraction(&samples) } else { 0.0 };
        let mut facing = 0.0;
        let mut nmean = [0.0; 3];
        for &t in region {
            let e = unit(perp(sub(prep.centroids[t], c), a));
            facing += prep.areas[t] * dot(prep.normals[t], unit(sub(e, mul(a, k))));
            nmean = add(nmean, mul(prep.normals[t], prep.areas[t]));
        }
        let nmean = mul(nmean, 1.0 / area);
        let mut nn = [[0.0; 3]; 3];
        for &t in region {
            let d = sub(prep.normals[t], nmean);
            for r in 0..3 {
                for s2 in 0..3 {
                    nn[r][s2] += prep.areas[t] * d[r] * d[s2];
                }
            }
        }
        let (vals, _) = if stats { eigen_sorted(nn) } else { ([0.0; 3], [[0.0; 3]; 3]) };
        let fit = ConeFit {
            axis: a,
            center: c,
            r0,
            k,
            rms,
            area,
            hole: facing < 0.0,
            coverage: bins.iter().filter(|b| **b).count() as f64 * 10.0,
            zmin,
            zmax,
            flatness: vals[0].max(0.0) / area,
            systematic,
        };
        (finite3(fit.axis) && finite3(fit.center) && fit.r0.is_finite() && fit.k.is_finite()).then_some(fit)
    }

    /// Cone hypothesis from a loosely grown cylinder-like region: a trial
    /// fit on a subsample, then alternate growing and refitting so a long
    /// tapered body is followed end to end (a cylinder model only covers a
    /// band of it). `None` when the half-angle is under 0.5° (a cylinder
    /// then; a near-cylindrical cone would also fit short rings of curved
    /// tubes) or the cone does not hold up.
    fn grow_cone(
        &self,
        grower: &mut Grower,
        loose: &[usize],
        init: (V3, V3, f64),
    ) -> Option<(ConeFit, Vec<usize>)> {
        let (axis, center, radius) = init;
        let mut model = self.fit_cone(loose, axis, center, radius, 0.0, 900, true)?;
        let ha = model.half_angle_deg().abs();
        // Cheap early outs: a trial that already misfits (rms) or leaves
        // structured residual (a torus or bent-tube patch) is not a cone.
        if ha > 60.0 || ha < 0.25 || model.rms > 0.6 * self.cyl_tol(model.mean_radius()) || model.systematic > 0.4 {
            return None;
        }
        let mut region = loose.to_vec();
        for _ in 0..6 {
            let tol = self.cyl_tol(model.mean_radius());
            let sign = if model.hole { -1.0 } else { 1.0 };
            let m = model.clone();
            let grown = grower.grow(self.topo, &self.available, &region, |t| self.cone_accept(t, &m, sign, tol));
            if grown.len() < 6 {
                return None;
            }
            let next = self.fit_cone(&grown, m.axis, m.center, m.r0, m.k, 1500, false)?;
            let done = grown.len() <= region.len();
            region = grown;
            model = next;
            // Growth only adds triangles within tolerance, so a model that
            // already misses the final rms bound will not recover.
            if model.rms > 0.5 * self.cyl_tol(model.mean_radius()) {
                return None;
            }
            if done {
                break;
            }
        }
        let model = self.fit_cone(&region, model.axis, model.center, model.r0, model.k, 6000, true)?;
        self.cone_ok(&model).then_some((model, region))
    }

    fn detect_cylinders(&mut self) -> Vec<(f64, Feature)> {
        let mut found: Vec<(CylFit, Vec<usize>)> = Vec::new();
        let mut cones: Vec<(ConeFit, Vec<usize>)> = Vec::new();
        let mut cone_attempts = 0;
        let mut tried = vec![false; self.prep.tris.len()];
        let mut attempts = 0;
        let mut grower = std::mem::replace(&mut self.grower, Grower { stamp: Vec::new(), generation: 0, budget: 0 });
        grower.budget = 8 * self.prep.tris.len() + 1000;
        for i in 0..self.order.len() {
            let seed = self.order[i];
            if !self.available[seed] || tried[seed] || self.prep.areas[seed] <= 0.0 {
                continue;
            }
            if attempts >= MAX_ATTEMPTS || grower.budget == 0 {
                break;
            }
            tried[seed] = true;
            let Some(&partner) = self.partners(seed).first() else { continue };
            attempts += 1;
            let (n1, n2) = (self.prep.normals[seed], self.prep.normals[partner]);
            let axis = unit(cross(n1, n2));
            if is_zero(axis) {
                continue;
            }
            // Lines c_i + s n_i (in the plane across the axis) meet on the
            // axis: c1 - r n1 = c2 - r n2 gives the signed radius (positive
            // for a boss, negative for a bore whose normals face the axis).
            let (c1, c2) = (self.prep.centroids[seed], self.prep.centroids[partner]);
            let dn = sub(perp(n1, axis), perp(n2, axis));
            let denom = dot(dn, dn);
            if !(denom > 1e-12) {
                continue;
            }
            let rs = dot(perp(sub(c1, c2), axis), dn) / denom;
            let r = rs.abs();
            if !(r > 1e-9 * self.diag && r <= self.diag) {
                continue;
            }
            let sign = rs.signum();
            let center = sub(c1, mul(perp(n1, axis), rs));
            let loose_tol = 3.0 * self.cyl_tol(r);
            let loose = grower.grow(self.topo, &self.available, &[seed], |t| {
                self.cyl_accept(t, axis, center, r, sign, loose_tol)
            });
            if self.region_area(&loose) < 0.5 * self.min_area {
                for &t in &loose {
                    tried[t] = true;
                }
                continue;
            }
            let Some(fit) = self.fit_cylinder(&loose, axis) else {
                loose.iter().for_each(|&t| tried[t] = true);
                continue;
            };
            let tol = self.cyl_tol(fit.radius);
            let fsign = if fit.hole { -1.0 } else { 1.0 };
            let tight = grower.grow(self.topo, &self.available, &loose, |t| {
                self.cyl_accept(t, fit.axis, fit.center, fit.radius, fsign, tol)
            });
            let loose_fit = (fit.axis, fit.center, fit.radius, fit.coverage);
            let fit = self.fit_cylinder(&tight, fit.axis);
            let ok = fit.as_ref().is_some_and(|f| tight.len() >= 6 && self.cyl_ok(f));
            // A tapered surface grows only a band as a cylinder; try a cone
            // over the same arc and keep it when it covers clearly more.
            // Start from whichever region wraps further around (noise makes
            // the two-facet hypothesis poor, so often only the refit wraps).
            let start = match &fit {
                Some(f) if tight.len() >= 6 && f.coverage >= loose_fit.3 => Some((f.axis, f.center, f.radius, f.coverage, &tight)),
                _ => Some((loose_fit.0, loose_fit.1, loose_fit.2, loose_fit.3, &loose)),
            }
            .filter(|s| s.3 >= 150.0);
            if let (Some((ax, ce, ra, _, from)), true) = (start, cone_attempts < MAX_CONE_ATTEMPTS) {
                cone_attempts += 1;
                if let Some((cone, region)) = self.grow_cone(&mut grower, from, (ax, ce, ra)) {
                    let cyl_area = if ok { fit.as_ref().map_or(0.0, |f| f.area) } else { 0.0 };
                    if cone.area > 1.05 * cyl_area {
                        for &t in &region {
                            self.available[t] = false;
                        }
                        cones.push((cone, region));
                        continue;
                    }
                }
            }
            match fit {
                Some(f) if ok => {
                    for &t in &tight {
                        self.available[t] = false;
                    }
                    // One surface can grow as several regions (noise breaks
                    // adjacency, a slot interrupts the wall): merge with an
                    // earlier fit of the same cylinder by refitting the union.
                    let tol = self.cyl_tol(f.radius);
                    let same = found.iter().position(|(g, _): &(CylFit, Vec<usize>)| {
                        g.hole == f.hole
                            && dot(g.axis, f.axis).abs() >= 3f64.to_radians().cos()
                            && norm(perp(sub(f.axis_point, g.axis_point), g.axis)) <= tol
                            && (g.radius - f.radius).abs() <= tol
                    });
                    match same {
                        Some(i) => {
                            let mut region = std::mem::take(&mut found[i].1);
                            region.extend_from_slice(&tight);
                            match self.fit_cylinder(&region, found[i].0.axis) {
                                Some(m) => found[i] = (m, region),
                                None => found.push((f, tight)),
                            }
                        }
                        None => found.push((f, tight)),
                    }
                }
                _ => {
                    for &t in loose.iter().chain(&tight) {
                        tried[t] = true;
                    }
                }
            }
        }
        // Short arcs fitted early are ambiguous under noise (a 120-degree
        // patch fits r = 9.3 about as well as r = 10), so a fragment can be
        // accepted with the wrong radius. Fold every fragment whose
        // triangles fit a larger cylinder into it, then let each cylinder
        // absorb neighbours that fit its final model.
        found.sort_by(|a, b| b.0.area.total_cmp(&a.0.area));
        let mut merged: Vec<(CylFit, Vec<usize>)> = Vec::new();
        for (f, region) in found {
            let host = merged.iter().position(|(g, _)| {
                g.hole == f.hole && {
                    let tol = 1.5 * self.cyl_tol(g.radius);
                    let sign = if g.hole { -1.0 } else { 1.0 };
                    let fits = region
                        .iter()
                        .filter(|&&t| self.cyl_accept(t, g.axis, g.center, g.radius, sign, tol))
                        .count();
                    fits * 10 >= region.len() * 8
                }
            });
            match host {
                Some(i) => {
                    let mut union = merged[i].1.clone();
                    union.extend_from_slice(&region);
                    if let Some(m) = self.fit_cylinder(&union, merged[i].0.axis) {
                        merged[i] = (m, union);
                    }
                }
                None => merged.push((f, region)),
            }
        }
        // Cones: fold fragments of one cone together (noise or a slot can
        // split it), then let each absorb cylinder bands that lie on it (a
        // band of a shallow taper can pass as a cylinder before the cone is
        // found).
        cones.sort_by(|a, b| b.0.area.total_cmp(&a.0.area));
        let mut cone_merged: Vec<(ConeFit, Vec<usize>)> = Vec::new();
        let on_cone = |g: &ConeFit, region: &[usize]| {
            let tol = 1.5 * self.cyl_tol(g.mean_radius());
            let sign = if g.hole { -1.0 } else { 1.0 };
            let fits = region.iter().filter(|&&t| self.cone_accept(t, g, sign, tol)).count();
            fits * 10 >= region.len() * 8
        };
        for (f, region) in cones {
            match cone_merged.iter().position(|(g, _)| g.hole == f.hole && on_cone(g, &region)) {
                Some(i) => {
                    let mut union = std::mem::take(&mut cone_merged[i].1);
                    union.extend_from_slice(&region);
                    let g = &cone_merged[i].0;
                    if let Some(m) = self.fit_cone(&union, g.axis, g.center, g.r0, g.k, 6000, true) {
                        cone_merged[i].0 = m;
                    }
                    cone_merged[i].1 = union;
                }
                None => cone_merged.push((f, region)),
            }
        }
        let mut kept = Vec::with_capacity(merged.len());
        for (f, region) in merged {
            match cone_merged.iter().position(|(g, _)| g.hole == f.hole && on_cone(g, &region)) {
                Some(i) => {
                    cone_merged[i].1.extend_from_slice(&region);
                    let g = &cone_merged[i].0;
                    if let Some(m) = self.fit_cone(&cone_merged[i].1, g.axis, g.center, g.r0, g.k, 6000, true) {
                        cone_merged[i].0 = m;
                    }
                }
                None => kept.push((f, region)),
            }
        }
        let mut merged = kept;
        for (f, region) in cone_merged.iter_mut() {
            region.iter().for_each(|&t| self.available[t] = true);
            let tol = self.cyl_tol(f.mean_radius());
            let sign = if f.hole { -1.0 } else { 1.0 };
            let model = f.clone();
            let grown = grower.grow(self.topo, &self.available, region, |t| self.cone_accept(t, &model, sign, tol));
            if grown.len() >= region.len() {
                if let Some(m) = self.fit_cone(&grown, model.axis, model.center, model.r0, model.k, 6000, true) {
                    if m.hole == f.hole && self.cone_ok(&m) {
                        *f = m;
                        *region = grown;
                    }
                }
            }
            region.iter().for_each(|&t| self.available[t] = false);
        }
        for (f, region) in merged.iter_mut() {
            region.iter().for_each(|&t| self.available[t] = true);
            let tol = self.cyl_tol(f.radius);
            let sign = if f.hole { -1.0 } else { 1.0 };
            let (axis, center, radius) = (f.axis, f.center, f.radius);
            let grown = grower.grow(self.topo, &self.available, region, |t| {
                self.cyl_accept(t, axis, center, radius, sign, tol)
            });
            if grown.len() >= region.len() {
                if let Some(m) = self.fit_cylinder(&grown, axis) {
                    if m.hole == f.hole && self.cyl_ok(&m) {
                        *f = m;
                        *region = grown;
                    }
                }
            }
            region.iter().for_each(|&t| self.available[t] = false);
        }
        self.grower = grower;
        let cone_features: Vec<(f64, Feature)> = cone_merged.iter().map(|(f, _)| (f.area, f.feature())).collect();
        merged
            .into_iter()
            .map(|(f, _)| {
                (
                    f.area,
                    Feature::Cylinder {
                        axis_point: round_v(f.axis_point),
                        axis: round_v(f.axis),
                        radius: round6(f.radius),
                        length: round6(f.length),
                        hole: f.hole,
                        coverage_deg: f.coverage,
                        rms: round6(f.rms),
                    },
                )
            })
            .chain(cone_features)
            .collect()
    }

    fn sphere_accept(&self, t: usize, c: V3, r: f64, sign: f64, tol: f64) -> bool {
        let n = self.prep.normals[t];
        if is_zero(n) {
            return false;
        }
        for p in self.prep.corners(t) {
            if (norm(sub(p, c)) - r).abs() > tol {
                return false;
            }
        }
        dot(n, unit(sub(self.prep.centroids[t], c))) * sign >= 25f64.to_radians().cos()
    }

    fn fit_sphere(&self, region: &[usize]) -> Option<SphereFit> {
        let prep = self.prep;
        let area = self.region_area(region);
        if !(area > 0.0) {
            return None;
        }
        let mut pts = Vec::with_capacity(region.len() * 3);
        let mut nn = [[0.0; 3]; 3];
        for &t in region {
            let (n, a) = (prep.normals[t], prep.areas[t]);
            for p in prep.corners(t) {
                pts.push((p, a));
            }
            for r in 0..3 {
                for s in 0..3 {
                    nn[r][s] += a * n[r] * n[s];
                }
            }
        }
        let (center, radius) = fit_sphere(&pts)?;
        let mut se = 0.0;
        let mut wsum = 0.0;
        for &(p, w) in &pts {
            let d = norm(sub(p, center)) - radius;
            se += w * d * d;
            wsum += w;
        }
        let facing: f64 = region
            .iter()
            .map(|&t| prep.areas[t] * dot(prep.normals[t], unit(sub(prep.centroids[t], center))))
            .sum();
        let (vals, _) = eigen_sorted(nn);
        let nmean = mul(region.iter().fold([0.0; 3], |acc, &t| add(acc, mul(prep.normals[t], prep.areas[t]))), 1.0 / area);
        let mut centred = nn;
        for r in 0..3 {
            for s in 0..3 {
                centred[r][s] -= area * nmean[r] * nmean[s];
            }
        }
        let (cvals, _) = eigen_sorted(centred);
        Some(SphereFit {
            center,
            radius,
            rms: (se / wsum).sqrt(),
            area,
            hole: facing < 0.0,
            spread: vals[0].max(0.0) / area,
            zonal: cvals[0].max(0.0) / area,
        })
    }

    fn detect_spheres(&mut self) -> Vec<(f64, Feature)> {
        let mut found = Vec::new();
        let mut tried = vec![false; self.prep.tris.len()];
        let mut attempts = 0;
        let mut grower = std::mem::replace(&mut self.grower, Grower { stamp: Vec::new(), generation: 0, budget: 0 });
        grower.budget = 8 * self.prep.tris.len() + 1000;
        for i in 0..self.order.len() {
            let seed = self.order[i];
            if !self.available[seed] || tried[seed] || self.prep.areas[seed] <= 0.0 {
                continue;
            }
            if attempts >= MAX_ATTEMPTS || grower.budget == 0 {
                break;
            }
            tried[seed] = true;
            let partners = self.partners(seed);
            let Some(&p1) = partners.first() else { continue };
            let n1 = self.prep.normals[p1];
            let cos2 = 2f64.to_radians().cos();
            let Some(&p2) = partners[1..].iter().find(|&&t| dot(self.prep.normals[t], n1) < cos2)
            else {
                continue;
            };
            attempts += 1;
            // Point closest (least squares) to the three normal lines.
            let mut m = vec![vec![0.0; 3]; 3];
            let mut rhs = vec![0.0; 3];
            for t in [seed, p1, p2] {
                let (n, c) = (self.prep.normals[t], self.prep.centroids[t]);
                for r in 0..3 {
                    for s in 0..3 {
                        let pr = f64::from(u8::from(r == s)) - n[r] * n[s];
                        m[r][s] += pr;
                        rhs[r] += pr * c[s];
                    }
                }
            }
            let Some(x) = solve_linear(m, rhs) else { continue };
            let x = [x[0], x[1], x[2]];
            let r = [seed, p1, p2]
                .iter()
                .map(|&t| norm(sub(self.prep.centroids[t], x)))
                .sum::<f64>()
                / 3.0;
            if !(r > 1e-9 * self.diag && r <= self.diag) {
                continue;
            }
            let sign = if dot(self.prep.normals[seed], sub(self.prep.centroids[seed], x)) >= 0.0 {
                1.0
            } else {
                -1.0
            };
            let loose_tol = 3.0 * self.cyl_tol(r);
            let loose = grower.grow(self.topo, &self.available, &[seed], |t| {
                self.sphere_accept(t, x, r, sign, loose_tol)
            });
            if self.region_area(&loose) < 0.5 * self.min_area {
                loose.iter().for_each(|&t| tried[t] = true);
                continue;
            }
            let Some(fit) = self.fit_sphere(&loose) else {
                loose.iter().for_each(|&t| tried[t] = true);
                continue;
            };
            let tol = self.cyl_tol(fit.radius);
            let fsign = if fit.hole { -1.0 } else { 1.0 };
            let tight = grower.grow(self.topo, &self.available, &loose, |t| {
                self.sphere_accept(t, fit.center, fit.radius, fsign, tol)
            });
            let fit = self.fit_sphere(&tight);
            let ok = fit.as_ref().is_some_and(|f| {
                tight.len() >= 8
                    && f.area >= self.min_area
                    && f.radius <= self.diag
                    && f.rms <= 0.5 * self.cyl_tol(f.radius)
                    && f.spread >= 0.05
                    && f.zonal >= 0.004
            });
            match fit {
                Some(f) if ok => {
                    for &t in &tight {
                        self.available[t] = false;
                    }
                    found.push((
                        f.area,
                        Feature::Sphere {
                            center: round_v(f.center),
                            radius: round6(f.radius),
                            hole: f.hole,
                            rms: round6(f.rms),
                        },
                    ));
                }
                _ => loose.iter().chain(&tight).for_each(|&t| tried[t] = true),
            }
        }
        self.grower = grower;
        found
    }

    fn detect_planes(&mut self) -> Vec<(f64, V3, V3, f64)> {
        let prep = self.prep;
        let tol = 0.003 * self.diag;
        let cos10 = 10f64.to_radians().cos();
        let mut found = Vec::new();
        let mut tried = vec![false; prep.tris.len()];
        let mut grower = std::mem::replace(&mut self.grower, Grower { stamp: Vec::new(), generation: 0, budget: 0 });
        grower.budget = 8 * prep.tris.len() + 1000;
        let plane_accept = |t: usize, n: V3, p: V3| {
            dot(prep.normals[t], n) >= cos10
                && prep.corners(t).iter().all(|q| dot(sub(*q, p), n).abs() <= tol)
        };
        // A curved patch grown against a plane stops at the tolerance and so
        // can show a noise-sized rms; a quadratic height field explaining
        // most of its residual gives it away (noise stays unexplained).
        let diag = self.diag;
        let curved = |region: &[usize], n: V3, p: V3, rms: f64| -> bool {
            if rms <= 1e-9 * diag {
                return false;
            }
            let (u, v) = plane_basis(n);
            let mut m = vec![vec![0.0; 6]; 6];
            let mut rhs = vec![0.0; 6];
            let mut samples = Vec::with_capacity(region.len() * 3);
            for &t in region {
                for q in prep.corners(t) {
                    let d = sub(q, p);
                    let (x, y, e) = (dot(d, u) / diag, dot(d, v) / diag, dot(d, n));
                    let f = [1.0, x, y, x * x, x * y, y * y];
                    let w = prep.areas[t];
                    for i in 0..6 {
                        for j in 0..6 {
                            m[i][j] += w * f[i] * f[j];
                        }
                        rhs[i] += w * f[i] * e;
                    }
                    samples.push((f, e, w));
                }
            }
            let Some(c) = solve_linear(m, rhs) else { return false };
            let (mut total, mut left) = (0.0, 0.0);
            for (f, e, w) in samples {
                let pred: f64 = f.iter().zip(&c).map(|(a, b)| a * b).sum();
                total += w * e * e;
                left += w * (e - pred).powi(2);
            }
            total > 0.0 && 1.0 - left / total > 0.5
        };
        for i in 0..self.order.len() {
            let seed = self.order[i];
            if !self.available[seed] || tried[seed] || prep.areas[seed] <= 0.0 {
                continue;
            }
            if grower.budget == 0 {
                break;
            }
            tried[seed] = true;
            let (n0, p0) = (prep.normals[seed], prep.centroids[seed]);
            let first = grower.grow(self.topo, &self.available, &[seed], |t| plane_accept(t, n0, p0));
            let refit = |region: &[usize]| {
                let mut an = [0.0; 3];
                let mut ap = [0.0; 3];
                let mut a = 0.0;
                for &t in region {
                    an = add(an, mul(prep.normals[t], prep.areas[t]));
                    ap = add(ap, mul(prep.centroids[t], prep.areas[t]));
                    a += prep.areas[t];
                }
                (unit(an), mul(ap, 1.0 / a.max(1e-300)), a)
            };
            let (n1, p1, _) = refit(&first);
            let region = if is_zero(n1) {
                first
            } else {
                grower.grow(self.topo, &self.available, &first, |t| plane_accept(t, n1, p1))
            };
            let (n, p, area) = refit(&region);
            let mut se = 0.0;
            for &t in &region {
                let d: f64 = prep.corners(t).iter().map(|q| dot(sub(*q, p), n).powi(2)).sum();
                se += prep.areas[t] * d / 3.0;
            }
            let rms = (se / area.max(1e-300)).sqrt();
            if area >= self.min_area && !is_zero(n) && rms <= 0.5 * tol && !curved(&region, n, p, rms) {
                for &t in &region {
                    self.available[t] = false;
                }
                found.push((area, p, n, rms));
            } else {
                region.iter().for_each(|&t| tried[t] = true);
            }
        }
        self.grower = grower;
        // Merge coplanar patches (a face split by a boss, two feet on the
        // same plane) into one feature.
        found.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut merged: Vec<(f64, V3, V3, f64)> = Vec::new();
        let cos3 = 3f64.to_radians().cos();
        for (a, p, n, rms) in found {
            if let Some(m) = merged
                .iter_mut()
                .find(|m| dot(m.2, n) >= cos3 && dot(sub(p, m.1), m.2).abs() <= tol)
            {
                let total = m.0 + a;
                m.1 = add(mul(m.1, m.0 / total), mul(p, a / total));
                m.3 = ((m.3 * m.3 * m.0 + rms * rms * a) / total).sqrt();
                m.0 = total;
            } else {
                merged.push((a, p, n, rms));
            }
        }
        merged
    }
}

/// Returns the largest `max` features and how many were found in total.
fn detect_features(prep: &Prep, topo: &Topo, diag: f64, max: usize) -> (Vec<Feature>, usize) {
    let n = prep.tris.len();
    // Visit seeds in a scattered but deterministic order (stride coprime
    // with n), so the attempt budget samples the whole part, not one end.
    let mut step = ((n as f64) * 0.618_033_988_749_895) as usize | 1;
    let gcd = |mut a: usize, mut b: usize| {
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a
    };
    while n > 1 && gcd(step, n) != 1 {
        step += 1;
    }
    let order = (0..n).map(|k| (k * step) % n.max(1)).collect();
    let mut det = Detector {
        prep,
        topo,
        diag,
        min_area: (prep.total_area * 0.002).max(1e-12),
        available: vec![true; n],
        grower: Grower { stamp: vec![0; n], generation: 0, budget: 0 },
        order,
    };
    let mut all = det.detect_spheres();
    all.extend(det.detect_cylinders());
    for (area, p, normal, rms) in det.detect_planes() {
        all.push((
            area,
            Feature::Plane { point: round_v(p), normal: round_v(normal), area: round6(area), rms: round6(rms) },
        ));
    }
    all.sort_by(|a, b| b.0.total_cmp(&a.0));
    let total = all.len();
    all.truncate(max);
    (all.into_iter().map(|(_, f)| f).collect(), total)
}

// ---------------------------------------------------------------------------
// section

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Section {
    /// Plane origin and in-plane basis: point3d = origin + u*x + v*y.
    pub origin: [f64; 3],
    pub u: [f64; 3],
    pub v: [f64; 3],
    pub loops: Vec<SectionLoop>,
    /// Net material area of the cut (outer loops minus holes), mm^2.
    pub area: f64,
    /// Area centroid of the net cut region in 3D; `None` when no closed loop.
    pub centroid: Option<[f64; 3]>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SectionLoop {
    /// Plane coordinates, counter-clockwise for outer boundaries.
    pub points: Vec<[f64; 2]>,
    pub closed: bool,
    /// Signed area (negative for holes).
    pub area: f64,
    pub hole: bool,
    /// Area centroid of the loop's polygon, plane coordinates and 3D
    /// (closed loops only; for an open chain, the mean of its points).
    pub centroid: [f64; 2],
    pub centroid3d: [f64; 3],
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AxisSection {
    /// Position along the axis from `AxisFit::point`, mm.
    pub t: f64,
    /// Net-area centroid of the cut perpendicular to the axis.
    pub centroid: [f64; 3],
    /// Distance of that centroid from the fitted line, mm.
    pub offset: f64,
    pub area: f64,
    /// Radius of the circle with the same area (sqrt(area/pi)).
    pub equivalent_radius: f64,
    /// Farthest outer-loop point from the centroid.
    pub max_radius: f64,
    /// Left out of the line fit: its centroid lay far off the line the
    /// other cuts agree on (a flare, lug or debris at that position).
    pub excluded: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AxisFit {
    /// A point on the fitted centerline (the centroid of all section
    /// centroids) and its unit direction (pointing from the first section to
    /// the last, i.e. along +initial when one was given).
    pub point: [f64; 3],
    pub direction: [f64; 3],
    /// Extent of the whole body along the fitted axis: t of its two ends
    /// (the cuts sample the trimmed/ranged part of it).
    pub start_t: f64,
    pub end_t: f64,
    pub sections: Vec<AxisSection>,
    /// RMS and max distance of section centroids from the line (straightness).
    pub rms_offset: f64,
    pub max_offset: f64,
    /// Angle between the axis and +X, +Y, +Z in degrees (0..90, sign-agnostic).
    pub angle_to_axes_deg: [f64; 3],
    /// Linear fit of equivalent radius along t (mm per mm; negative when the
    /// body narrows along `direction`). Excluded cuts are left out.
    pub taper_slope: f64,
    /// Cone half-angle implied by the taper, degrees, signed like the slope
    /// (0 = straight).
    pub taper_half_angle_deg: f64,
    /// multmatrix moving `point` to the origin and `direction` onto +Z.
    pub align_matrix: [[f64; 4]; 4],
    pub iterations: usize,
    pub notes: Vec<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct AxisOptions {
    /// Starting direction; defaults to the longest principal axis.
    pub initial: Option<[f64; 3]>,
    /// Number of perpendicular sections (clamped 3..=200).
    pub sections: usize,
    /// Fraction of the length trimmed at each end before sampling (ends are
    /// often caps or flares that bias the centroid), 0..0.45.
    pub trim: f64,
    /// Only sample t in this range (mm along the axis from the body centre), if set.
    pub range: Option<(f64, f64)>,
}

impl Default for AxisOptions {
    fn default() -> Self {
        Self { initial: None, sections: 24, trim: 0.05, range: None }
    }
}

/// Centerline of an elongated body (handle, tube, tapered or flared shaft):
/// slice perpendicular to a trial axis, take each cut's area centroid, fit a
/// 3D line through the centroids, re-slice perpendicular to that line, and
/// repeat until the direction settles (< 0.01 degrees of change).
///
/// Unlike [`PrincipalAxes`] (a PCA of the surface, pulled by any lump,
/// flare or uneven scan coverage), the centroid of a cut does not depend on
/// how the surface is distributed along the body, so this recovers the
/// true axis of tapered, flared or partly scanned parts. A cut that crosses
/// several separate bodies uses the largest one (with its holes); a cut of an
/// open scan with no closed outline closes its longest chain straight across
/// when the gap is small. Errs only when fewer than two cuts are usable.
///
/// The work is bounded ([`ANALYSIS_WORK`]) and observes the thread's kernel
/// deadline and cancellation: when either stops it after a full iteration,
/// the last complete fit is returned with a note saying so; before that, it
/// errs with the reason.
pub fn fit_axis(mesh: &TriMesh, options: AxisOptions) -> Result<AxisFit, String> {
    fit_axis_within(mesh, options, &mut Work::new(ANALYSIS_WORK))
}

pub(crate) fn fit_axis_within(mesh: &TriMesh, options: AxisOptions, work: &mut Work) -> Result<AxisFit, String> {
    const MAX_ITERATIONS: usize = 12;
    work.poll()?;
    let prep = prepare(mesh);
    if prep.tris.is_empty() || !(prep.total_area > 0.0) {
        return Err("Empty mesh: no triangles with area to slice".into());
    }
    let surface_centroid = mul(
        (0..prep.tris.len()).fold([0.0; 3], |acc, t| add(acc, mul(prep.centroids[t], prep.areas[t]))),
        1.0 / prep.total_area,
    );
    let mut dir = match options.initial {
        Some(d) => {
            let d = unit(d);
            if is_zero(d) {
                return Err("initial direction must be a non-zero finite vector".into());
            }
            d
        }
        None => principal_axes(&prep, surface_centroid).axes[0],
    };
    if is_zero(dir) {
        return Err("Could not find a starting direction (degenerate mesh)".into());
    }
    let mut point = surface_centroid;
    let count = options.sections.clamp(3, 200);
    let trim = if options.trim.is_finite() { options.trim.clamp(0.0, 0.45) } else { 0.05 };
    let range = options.range.filter(|(a, b)| a.is_finite() && b.is_finite() && b > a);
    let scale = prep.points.iter().fold(0.0f64, |m, p| m.max(p.iter().fold(0.0f64, |a, c| a.max(c.abs()))));
    let eps = scale.max(1e-300) * 1e-12;

    let mut proj = vec![0.0; prep.points.len()];
    let mut buckets: Vec<Vec<u32>> = vec![Vec::new(); count];
    let mut iterations = 0;
    let mut converged = false;
    let mut last: Option<(Vec<CutSample>, usize)> = None;
    let mut stopped: Option<String> = None;
    'iterate: loop {
        iterations += 1;
        let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
        for (i, p) in prep.points.iter().enumerate() {
            let t = dot(sub(*p, point), dir);
            proj[i] = t;
            lo = lo.min(t);
            hi = hi.max(t);
        }
        let len = hi - lo;
        if !(len > 0.0) || !len.is_finite() {
            return Err("The mesh has no extent along the axis".into());
        }
        let mid = 0.5 * (lo + hi);
        let (mut a, mut b) = (lo + trim * len, hi - trim * len);
        if let Some((ra, rb)) = range {
            a = a.max(mid + ra);
            b = b.min(mid + rb);
        }
        if !(b > a) {
            return Err(format!(
                "range does not overlap the body: it spans t = {:.3}..{:.3} mm from its centre along the axis",
                lo - mid,
                hi - mid
            ));
        }
        let step = (b - a) / count as f64;
        for bucket in buckets.iter_mut() {
            bucket.clear();
        }
        // Stops the fit: the last complete iteration stands, if there is one.
        macro_rules! spend {
            ($result:expr) => {
                if let Err(reason) = $result {
                    if last.is_none() {
                        return Err(reason);
                    }
                    iterations -= 1;
                    stopped = Some(reason);
                    break 'iterate;
                }
            };
        }
        spend!(work.spend(prep.points.len() as u64 + prep.tris.len() as u64));
        for (ti, ids) in prep.tris.iter().enumerate() {
            let ts = ids.map(|i| proj[i as usize]);
            let tmin = ts[0].min(ts[1]).min(ts[2]) - eps;
            let tmax = ts[0].max(ts[1]).max(ts[2]) + eps;
            let k0 = ((tmin - a) / step - 0.5).ceil().max(0.0);
            let k1 = ((tmax - a) / step - 0.5).floor().min(count as f64 - 1.0);
            if !(k0 <= k1) {
                continue;
            }
            for k in k0 as usize..=k1 as usize {
                buckets[k].push(ti as u32);
            }
        }
        let (u, v) = plane_basis(dir);
        let mut cuts = Vec::new();
        let mut empty = 0;
        for (k, bucket) in buckets.iter().enumerate() {
            spend!(work.spend(bucket.len() as u64));
            let t = a + (k as f64 + 0.5) * step;
            let origin = add(point, mul(dir, t));
            let snap = |d: f64| if d.abs() <= eps { 0.0 } else { d };
            let mut loops = slice_core(
                &prep,
                bucket.iter().map(|&ti| ti as usize),
                |i| snap(proj[i] - t),
                eps,
                origin,
                u,
                v,
                true,
            );
            spend!(finish_loops(&mut loops, 0.0, work));
            fill_centroids(&mut loops, origin, u, v);
            spend!(work.spend(loops.iter().map(|l| l.points.len() as u64).sum::<u64>()));
            let sample = cut_sample(&loops, origin, u, v, work);
            spend!(sample.as_ref().map(|_| ()).map_err(Clone::clone));
            match sample.ok().flatten() {
                Some(c) => cuts.push(c),
                None => empty += 1,
            }
        }
        if cuts.len() < 2 {
            match last {
                Some(_) => break,
                None => {
                    return Err(format!(
                        "Only {} of {count} cuts across the axis found material; is the body elongated along the starting direction?",
                        cuts.len()
                    ))
                }
            }
        }
        let (np, nd) = robust_line(&mut cuts, dir);
        let change = dot(nd, dir).clamp(-1.0, 1.0).acos().to_degrees();
        point = np;
        dir = nd;
        last = Some((cuts, empty));
        if change < 0.01 {
            converged = true;
            break;
        }
        if iterations >= MAX_ITERATIONS {
            break;
        }
    }
    let Some((cuts, empty)) = last else { return Err("No usable cuts".into()) };

    let mut out = AxisFit { point: round_v(point), direction: round_v(dir), iterations, ..Default::default() };
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for p in &prep.points {
        let t = dot(sub(*p, point), dir);
        lo = lo.min(t);
        hi = hi.max(t);
    }
    out.start_t = round6(lo);
    let inliers: Vec<&CutSample> = cuts.iter().filter(|c| !c.excluded).collect();
    out.end_t = round6(hi);
    let mut se = 0.0;
    let mut max_at = 0.0;
    for c in &cuts {
        let d = sub(c.centroid, point);
        let t = dot(d, dir);
        let offset = norm(perp(d, dir));
        if !c.excluded {
            se += offset * offset;
        }
        if !c.excluded && offset > out.max_offset {
            out.max_offset = offset;
            max_at = t;
        }
        out.sections.push(AxisSection {
            t: round6(t),
            centroid: round_v(c.centroid),
            offset: round6(offset),
            area: round6(c.area),
            equivalent_radius: round6((c.area / std::f64::consts::PI).sqrt()),
            max_radius: round6(c.max_radius),
            excluded: c.excluded,
        });
    }
    out.rms_offset = round6((se / inliers.len() as f64).sqrt());
    out.max_offset = round6(out.max_offset);
    out.angle_to_axes_deg = dir.map(|c| round6(c.abs().min(1.0).acos().to_degrees()));

    // Taper: least-squares line through (t, equivalent radius).
    let n = inliers.len() as f64;
    let ts: Vec<f64> = inliers.iter().map(|c| dot(sub(c.centroid, point), dir)).collect();
    let rs: Vec<f64> = inliers.iter().map(|c| (c.area / std::f64::consts::PI).sqrt()).collect();
    let (tm, rm) = (ts.iter().sum::<f64>() / n, rs.iter().sum::<f64>() / n);
    let stt: f64 = ts.iter().map(|t| (t - tm) * (t - tm)).sum();
    let str_: f64 = ts.iter().zip(&rs).map(|(t, r)| (t - tm) * (r - rm)).sum();
    let slope = if stt > 0.0 { str_ / stt } else { 0.0 };
    out.taper_slope = round6(slope);
    out.taper_half_angle_deg = round6(slope.atan().to_degrees());

    // Align: direction -> +Z, the world axis least aligned with it -> +X.
    let (x_axis, y_axis) = plane_basis(dir);
    for (k, row) in [x_axis, y_axis, dir].iter().enumerate() {
        out.align_matrix[k] = [row[0], row[1], row[2], -dot(*row, point)];
    }
    out.align_matrix[3] = [0.0, 0.0, 0.0, 1.0];

    // Notes, most important first.
    let names = ["X", "Y", "Z"];
    let k = (0..3).min_by(|&i, &j| out.angle_to_axes_deg[i].total_cmp(&out.angle_to_axes_deg[j])).unwrap_or(2);
    let tilt = dir[k].abs().min(1.0).acos().to_degrees();
    if tilt >= 0.01 {
        out.notes.push(format!(
            "axis tilted {tilt:.2}° from {}; multmatrix(alignMatrix) puts it on Z",
            names[k]
        ));
    } else {
        out.notes.push(format!("axis is parallel to {} (within 0.01°)", names[k]));
    }
    if cuts.len() < 3 {
        out.notes.push(format!("fewer than 3 usable sections ({}): the axis is a two-point line", cuts.len()));
    }
    let mean_r = rm.max(1e-12);
    if out.max_offset > 0.03 * mean_r {
        out.notes.push(format!(
            "curved: max offset {:.2} mm at t = {:.1} mm (rms {:.2} mm) — not a straight body; fit a range with `range` or treat it as a bent part",
            out.max_offset, max_at, out.rms_offset
        ));
    }
    if slope.atan().to_degrees().abs() >= 0.1 {
        out.notes.push(format!(
            "tapered: equivalent radius {:.2} → {:.2} mm over the sampled length (half-angle {:.2}°)",
            rm + slope * (ts[0] - tm),
            rm + slope * (ts[ts.len() - 1] - tm),
            slope.atan().to_degrees()
        ));
    }
    let excluded: Vec<String> = cuts
        .iter()
        .filter(|c| c.excluded)
        .map(|c| format!("{:.1}", dot(sub(c.centroid, point), dir)))
        .collect();
    if !excluded.is_empty() {
        out.notes.push(format!(
            "{} cut{} off the line (a flare, lug or debris) left out of the fit, at t = {} mm",
            excluded.len(),
            if excluded.len() == 1 { " lying" } else { "s lying" },
            excluded.join(", ")
        ));
    }
    let multi = cuts.iter().filter(|c| c.bodies > 1).count();
    if multi > 0 {
        out.notes.push(format!(
            "{multi} cut{} crossed several separate bodies; the largest was used",
            if multi == 1 { "" } else { "s" }
        ));
    }
    let open = cuts.iter().filter(|c| c.closed_open).count();
    if open > 0 {
        out.notes.push(format!(
            "{open} cut{} crossed a hole in the surface; the open outline was closed straight across (centroid approximate)",
            if open == 1 { "" } else { "s" }
        ));
    }
    if empty > 0 {
        out.notes.push(format!("{empty} cut{} found no usable outline and were skipped", if empty == 1 { "" } else { "s" }));
    }
    if let Some(reason) = stopped {
        out.notes.insert(
            0,
            format!("stopped early, after {iterations} complete iteration{}: {reason}", if iterations == 1 { "" } else { "s" }),
        );
    } else if !converged {
        out.notes.push(format!("did not settle within {iterations} iterations (last change ≥ 0.01°)"));
    }
    Ok(out)
}

/// One cut across the axis as `fit_axis` uses it.
struct CutSample {
    centroid: V3,
    area: f64,
    max_radius: f64,
    bodies: usize,
    closed_open: bool,
    /// Left out of the line fit as an outlier.
    excluded: bool,
}

/// The largest closed outer loop with everything nested in it (holes and
/// islands), or, when the cut has no closed loop, the longest open chain
/// closed straight across if its gap is small.
fn cut_sample(loops: &[SectionLoop], origin: V3, u: V3, v: V3, work: &mut Work) -> Result<Option<CutSample>, String> {
    let to3 = |c: [f64; 2]| add(origin, add(mul(u, c[0]), mul(v, c[1])));
    let radius_of = |pts: &[[f64; 2]], c: [f64; 2]| pts.iter().fold(0.0f64, |m, p| m.max((p[0] - c[0]).hypot(p[1] - c[1])));
    if let Some(oi) = loops.iter().position(|l| l.closed && !l.hole && l.area > 0.0) {
        let outer = &loops[oi];
        let bounds = outer.points.iter().fold(
            [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY],
            |b, p| [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])],
        );
        let mut area = 0.0;
        let mut acc = [0.0; 2];
        let mut bodies = 1;
        let mut other_area = 0.0;
        for (j, l) in loops.iter().enumerate() {
            if !l.closed {
                continue;
            }
            let p = l.points[0];
            let member = j == oi
                || (p[0] >= bounds[0] && p[0] <= bounds[2] && p[1] >= bounds[1] && p[1] <= bounds[3] && {
                    work.spend(outer.points.len() as u64)?;
                    point_in_polygon(p, &outer.points)
                });
            if member {
                area += l.area;
                acc[0] += l.area * l.centroid[0];
                acc[1] += l.area * l.centroid[1];
            } else if !l.hole {
                bodies += 1;
                other_area += l.area;
            }
        }
        if !(area > 0.0) {
            return Ok(None);
        }
        let c = [acc[0] / area, acc[1] / area];
        // Specks (scan debris) do not count as separate bodies.
        let bodies = if other_area > 0.02 * area { bodies } else { 1 };
        return Ok(Some(CutSample {
            centroid: to3(c),
            area,
            max_radius: radius_of(&outer.points, c),
            bodies,
            closed_open: false,
            excluded: false,
        }));
    }
    Ok(open_chain_sample(loops, to3, radius_of))
}

fn open_chain_sample(
    loops: &[SectionLoop],
    to3: impl Fn([f64; 2]) -> V3,
    radius_of: impl Fn(&[[f64; 2]], [f64; 2]) -> f64,
) -> Option<CutSample> {
    let chain = loops
        .iter()
        .filter(|l| !l.closed && l.points.len() >= 3)
        .map(|l| (l, l.points.windows(2).map(|w| (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1])).sum::<f64>()))
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    let (l, length) = chain;
    let (f, e) = (l.points[0], l.points[l.points.len() - 1]);
    if (f[0] - e[0]).hypot(f[1] - e[1]) > 0.25 * length {
        return None;
    }
    let (c, a) = polygon_centroid(&l.points);
    let area = a.abs();
    (area > 0.0).then(|| CutSample {
        centroid: to3(c),
        area,
        max_radius: radius_of(&l.points, c),
        bodies: 1,
        closed_open: true,
        excluded: false,
    })
}

/// `fit_line`, then once more without the cuts lying far off the first line
/// (a flare or lug at one end pulls the centroid of its cut sideways), when
/// they are few: at most a fifth of the cuts, at least three remaining.
/// Many far cuts mean the body is curved, which is reported, not hidden.
fn robust_line(cuts: &mut [CutSample], prev: V3) -> (V3, V3) {
    cuts.iter_mut().for_each(|c| c.excluded = false);
    let (p, d) = fit_line(cuts.iter(), prev);
    let offsets: Vec<f64> = cuts.iter().map(|c| norm(perp(sub(c.centroid, p), d))).collect();
    let mut sorted = offsets.clone();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[sorted.len() / 2];
    let mean_r = cuts.iter().map(|c| (c.area / std::f64::consts::PI).sqrt()).sum::<f64>() / cuts.len() as f64;
    let limit = (4.0 * median).max(0.005 * mean_r);
    let far = offsets.iter().filter(|&&o| o > limit).count();
    if far == 0 || far * 5 > cuts.len() || cuts.len() - far < 3 {
        return (p, d);
    }
    for (c, o) in cuts.iter_mut().zip(&offsets) {
        c.excluded = *o > limit;
    }
    fit_line(cuts.iter().filter(|c| !c.excluded), d)
}

/// Line through cut centroids: unweighted PCA (each cut one vote; weighting
/// by area lets a wide flare dominate), oriented along `prev`.
fn fit_line<'a>(cuts: impl Iterator<Item = &'a CutSample> + Clone, prev: V3) -> (V3, V3) {
    let n = cuts.clone().count().max(1) as f64;
    let mean = mul(cuts.clone().fold([0.0; 3], |acc, c| add(acc, c.centroid)), 1.0 / n);
    let mut m = [[0.0; 3]; 3];
    for c in cuts {
        let d = sub(c.centroid, mean);
        for r in 0..3 {
            for s in 0..3 {
                m[r][s] += d[r] * d[s];
            }
        }
    }
    let (_, vecs) = eigen_sorted(m);
    let mut d = vecs[2];
    if is_zero(d) {
        d = prev;
    }
    if dot(d, prev) < 0.0 {
        d = mul(d, -1.0);
    }
    (mean, d)
}

/// Area centroid and signed area of a polygon (shoelace, relative to the
/// first vertex for precision); the mean of the points when it has no area.
fn polygon_centroid(points: &[[f64; 2]]) -> ([f64; 2], f64) {
    let n = points.len();
    if n == 0 {
        return ([0.0; 2], 0.0);
    }
    let mean = || {
        let s = points.iter().fold([0.0; 2], |a, p| [a[0] + p[0], a[1] + p[1]]);
        [s[0] / n as f64, s[1] / n as f64]
    };
    if n < 3 {
        return (mean(), 0.0);
    }
    let o = points[0];
    let (mut a2, mut cx, mut cy) = (0.0, 0.0, 0.0);
    for i in 1..n - 1 {
        let (p, q) = ([points[i][0] - o[0], points[i][1] - o[1]], [points[i + 1][0] - o[0], points[i + 1][1] - o[1]]);
        let c = p[0] * q[1] - p[1] * q[0];
        a2 += c;
        cx += (p[0] + q[0]) * c;
        cy += (p[1] + q[1]) * c;
    }
    let scale = points.iter().fold(0.0f64, |m, p| m.max((p[0] - o[0]).abs()).max((p[1] - o[1]).abs()));
    if !(a2.abs() > 1e-14 * scale * scale) {
        return (mean(), 0.0);
    }
    ([o[0] + cx / (3.0 * a2), o[1] + cy / (3.0 * a2)], 0.5 * a2)
}

fn fill_centroids(loops: &mut [SectionLoop], origin: V3, u: V3, v: V3) {
    for l in loops.iter_mut() {
        let c = if l.closed {
            polygon_centroid(&l.points).0
        } else {
            let n = l.points.len().max(1) as f64;
            let s = l.points.iter().fold([0.0; 2], |a, p| [a[0] + p[0], a[1] + p[1]]);
            [s[0] / n, s[1] / n]
        };
        l.centroid = c;
        l.centroid3d = add(origin, add(mul(u, c[0]), mul(v, c[1])));
    }
}

/// Net area (outer loops positive, holes negative, as normalised by
/// `finish_loops`) and its area centroid in 3D.
fn net_area_centroid(loops: &[SectionLoop]) -> (f64, Option<V3>) {
    let mut area = 0.0;
    let mut acc = [0.0; 3];
    let mut any = false;
    for l in loops.iter().filter(|l| l.closed) {
        any = true;
        area += l.area;
        acc = add(acc, mul(l.centroid3d, l.area));
    }
    let centroid = (any && area.abs() > 0.0).then(|| mul(acc, 1.0 / area)).filter(|c| finite3(*c));
    (area, centroid)
}

/// Cuts `mesh` with the plane through `point` with `normal`. `tolerance`
/// (mm) simplifies loops (Douglas-Peucker); 0 keeps every vertex.
///
/// Segments are keyed by the welded mesh edge they cross, so chaining is
/// topological and exact (no coordinate hashing tolerance). Vertices lying
/// exactly on the plane are treated as being on one side (symbolic
/// perturbation), which keeps every crossing consistent between the two
/// triangles sharing an edge; if that yields nothing (the plane coincides
/// with a face at the very bottom), the other side is tried.
// Library API; the server's own analysis calls `section_within`.
#[allow(dead_code)]
pub fn section(mesh: &TriMesh, point: [f64; 3], normal: [f64; 3], tolerance: f64) -> Section {
    section_within(mesh, point, normal, tolerance, &mut Work::unbounded()).expect("unbounded work never runs out")
}

/// [`section`] within a work budget that also observes the thread's
/// deadline and cancellation; errs with the reason it stopped.
pub(crate) fn section_within(
    mesh: &TriMesh,
    point: [f64; 3],
    normal: [f64; 3],
    tolerance: f64,
    work: &mut Work,
) -> Result<Section, String> {
    let n = unit(normal);
    if is_zero(n) || !finite3(point) {
        return Ok(Section { origin: if finite3(point) { point } else { [0.0; 3] }, ..Default::default() });
    }
    let (u, v) = plane_basis(n);
    let prep = prepare(mesh);
    let mut loops = slice(&prep, point, n, u, v, true);
    if loops.is_empty() {
        loops = slice(&prep, point, n, u, v, false);
    }
    let tolerance = if tolerance.is_finite() { tolerance.max(0.0) } else { 0.0 };
    work.spend(prep.tris.len() as u64)?;
    finish_loops(&mut loops, tolerance, work)?;
    fill_centroids(&mut loops, point, u, v);
    let (area, centroid) = net_area_centroid(&loops);
    Ok(Section { origin: point, u, v, loops, area, centroid: centroid.map(round_v) })
}

fn slice(prep: &Prep, origin: V3, n: V3, u: V3, v: V3, zero_above: bool) -> Vec<SectionLoop> {
    let scale = prep.points.iter().fold(norm(origin), |m, p| m.max(p.iter().fold(0.0f64, |a, c| a.max(c.abs()))));
    let eps = scale * 1e-12;
    let dist: Vec<f64> = prep
        .points
        .iter()
        .map(|p| {
            let d = dot(sub(*p, origin), n);
            if d.abs() <= eps {
                0.0
            } else {
                d
            }
        })
        .collect();
    slice_core(prep, 0..prep.tris.len(), |i| dist[i], eps, origin, u, v, zero_above)
}

/// Chains the crossings of the triangles `tris` with the plane whose signed
/// vertex distances are `dist` (already snapped to 0 within `eps`).
#[allow(clippy::too_many_arguments)]
fn slice_core(
    prep: &Prep,
    tris: impl Iterator<Item = usize>,
    dist: impl Fn(usize) -> f64,
    eps: f64,
    origin: V3,
    u: V3,
    v: V3,
    zero_above: bool,
) -> Vec<SectionLoop> {
    let above = |d: f64| if zero_above { d >= 0.0 } else { d > 0.0 };
    let key = |a: u32, b: u32| if a < b { ((a as u64) << 32) | b as u64 } else { ((b as u64) << 32) | a as u64 };
    // A triangle with outward normal crossing the plane contributes one
    // segment, oriented so the material (below the plane) is on its left
    // when viewed from +n: it runs from the edge going above->below to the
    // edge going below->above. That makes outer boundaries counter-clockwise.
    let mut segs: Vec<(u64, u64)> = Vec::new();
    for t in tris {
        let ids = prep.tris[t];
        let s = ids.map(|i| above(dist(i as usize)));
        if s[0] == s[1] && s[1] == s[2] {
            continue;
        }
        let (mut start, mut end) = (None, None);
        for k in 0..3 {
            let (a, b) = (ids[k], ids[(k + 1) % 3]);
            let (sa, sb) = (s[k], s[(k + 1) % 3]);
            if sa && !sb {
                start = Some(key(a, b));
            } else if !sa && sb {
                end = Some(key(a, b));
            }
        }
        if let (Some(a), Some(b)) = (start, end) {
            segs.push((a, b));
        }
    }
    if segs.is_empty() {
        return Vec::new();
    }
    let point_of = |k: u64| -> [f64; 2] {
        let (a, b) = ((k >> 32) as usize, (k & 0xffff_ffff) as usize);
        let (da, db) = (dist(a), dist(b));
        let t = if da == db { 0.0 } else { da / (da - db) };
        let p = add(prep.points[a], mul(sub(prep.points[b], prep.points[a]), t));
        let q = sub(p, origin);
        [dot(q, u), dot(q, v)]
    };
    let mut by_start: HashMap<u64, Vec<usize>> = HashMap::with_capacity(segs.len());
    let mut end_count: HashMap<u64, usize> = HashMap::with_capacity(segs.len());
    for (i, &(a, b)) in segs.iter().enumerate() {
        by_start.entry(a).or_default().push(i);
        *end_count.entry(b).or_default() += 1;
    }
    let mut used = vec![false; segs.len()];
    let mut out = Vec::new();
    let walk = |first: usize, used: &mut [bool], out: &mut Vec<SectionLoop>| {
        let mut keys = vec![segs[first].0];
        let mut cur = first;
        loop {
            used[cur] = true;
            let end = segs[cur].1;
            keys.push(end);
            if end == segs[first].0 {
                break;
            }
            let next = by_start.get(&end).and_then(|list| list.iter().copied().find(|&i| !used[i]));
            match next {
                Some(i) => cur = i,
                None => break,
            }
        }
        let closed = keys.len() > 2 && keys.first() == keys.last();
        if closed {
            keys.pop();
        }
        let mut pts: Vec<[f64; 2]> = Vec::with_capacity(keys.len());
        for k in keys {
            let p = point_of(k);
            // Plane-touching vertices give runs of identical points.
            if pts.last().is_none_or(|q| (q[0] - p[0]).abs() + (q[1] - p[1]).abs() > eps) {
                pts.push(p);
            }
        }
        if closed && pts.len() > 1 {
            let (f, l) = (pts[0], pts[pts.len() - 1]);
            if (f[0] - l[0]).abs() + (f[1] - l[1]).abs() <= eps {
                pts.pop();
            }
        }
        if (closed && pts.len() >= 3) || (!closed && pts.len() >= 2) {
            out.push(SectionLoop { points: pts, closed, area: 0.0, hole: false, centroid: [0.0; 2], centroid3d: [0.0; 3] });
        }
    };
    // Open chains first (they start where nothing ends), then closed loops.
    for i in 0..segs.len() {
        if !used[i] && end_count.get(&segs[i].0).copied().unwrap_or(0) == 0 {
            walk(i, &mut used, &mut out);
        }
    }
    for i in 0..segs.len() {
        if !used[i] {
            walk(i, &mut used, &mut out);
        }
    }
    out
}

/// Bounded work for one analysis call, and the request's interrupts.
///
/// Slicing and nesting are near-linear in the common case, but a cut through
/// thousands of separate bodies, or through thousands of nested rings, is
/// not, and `fit_axis` repeats that for up to 200 cuts and 12 iterations. A
/// budget turns "minutes of CPU on hostile input" into a prompt answer, and
/// the polls let a request that gave up (deadline, disconnect) stop too.
pub(crate) struct Work {
    left: u64,
    since_poll: u64,
    polls: bool,
}

/// What one `fit_axis` or `section` call may spend, in loop-vertex visits:
/// a few seconds of CPU at most.
pub(crate) const ANALYSIS_WORK: u64 = 3_000_000_000;
const WORK_EXHAUSTED: &str = "the cuts are too complex to analyse within this request's work budget \
     (thousands of separate or nested outlines per cut); select fewer objects or use fewer sections";

impl Work {
    pub(crate) fn new(limit: u64) -> Self {
        Self { left: limit, since_poll: 0, polls: true }
    }

    /// No limit and no interrupt polls: the original, pure behaviour.
    #[allow(dead_code)]
    fn unbounded() -> Self {
        Self { left: u64::MAX, since_poll: 0, polls: false }
    }

    fn spend(&mut self, units: u64) -> Result<(), String> {
        if !self.polls {
            return Ok(());
        }
        let units = units.max(1);
        if self.left <= units {
            self.left = 0;
            return Err(WORK_EXHAUSTED.into());
        }
        self.left -= units;
        self.since_poll += units;
        if self.since_poll >= 1 << 18 {
            self.since_poll = 0;
            self.poll()?;
        }
        Ok(())
    }

    fn poll(&self) -> Result<(), String> {
        match crate::csg::bsp::interrupt_message() {
            Some(message) if self.polls => Err(message.to_string()),
            _ => Ok(()),
        }
    }
}

/// Simplify, then classify nesting: a loop inside an odd number of others is
/// a hole. Orientation is normalised (outer CCW, holes CW) so the result is
/// usable even when the mesh winding is inconsistent.
fn finish_loops(loops: &mut Vec<SectionLoop>, tolerance: f64, work: &mut Work) -> Result<(), String> {
    if tolerance > 0.0 {
        for l in loops.iter_mut() {
            work.spend(l.points.len() as u64)?;
            l.points = if l.closed {
                simplify_closed(&l.points, tolerance)
            } else {
                douglas_peucker(&l.points, tolerance)
            };
        }
    }
    let boxes: Vec<[f64; 4]> = loops
        .iter()
        .map(|l| {
            l.points.iter().fold(
                [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY],
                |b, p| [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])],
            )
        })
        .collect();
    // Which closed loops' boxes may contain a point: a uniform grid over the
    // boxes, so a cut through 10,000 separate bodies tests each loop against
    // its few neighbours rather than all 10,000. The depth is a count, so
    // the order candidates come in does not matter, and the exact test is
    // unchanged: the result is identical to the all-pairs scan.
    let closed: Vec<usize> = (0..loops.len()).filter(|&i| loops[i].closed && !loops[i].points.is_empty()).collect();
    let index = BoxGrid::new(&closed, &boxes);
    work.spend(index.size() as u64)?;
    let mut candidates = Vec::new();
    let mut depth = vec![0usize; loops.len()];
    for &i in &closed {
        let p = loops[i].points[0];
        candidates.clear();
        index.query(p, &mut candidates);
        work.spend(candidates.len() as u64)?;
        for &j in &candidates {
            if j != i && p[0] >= boxes[j][0] && p[0] <= boxes[j][2] && p[1] >= boxes[j][1] && p[1] <= boxes[j][3] {
                work.spend(loops[j].points.len() as u64)?;
                if point_in_polygon(p, &loops[j].points) {
                    depth[i] += 1;
                }
            }
        }
    }
    for (l, d) in loops.iter_mut().zip(depth) {
        if !l.closed {
            l.area = 0.0;
            l.hole = false;
            continue;
        }
        l.hole = d % 2 == 1;
        let a = polygon_area(&l.points);
        if (l.hole && a > 0.0) || (!l.hole && a < 0.0) {
            l.points.reverse();
        }
        l.area = polygon_area(&l.points);
    }
    loops.sort_by(|a, b| b.closed.cmp(&a.closed).then(b.area.abs().total_cmp(&a.area.abs())));
    Ok(())
}

/// A uniform grid of 2D boxes for point queries. A box spanning more than
/// [`BoxGrid::WIDE`] cells goes on a list every query returns, which keeps
/// building linear; such boxes are rare unless the loops nest, and nested
/// loops have to be tested against each other anyway.
struct BoxGrid {
    origin: [f64; 2],
    cell: [f64; 2],
    side: usize,
    cells: Vec<Vec<usize>>,
    wide: Vec<usize>,
}

impl BoxGrid {
    const WIDE: usize = 64;

    fn new(members: &[usize], boxes: &[[f64; 4]]) -> Self {
        let mut low = [f64::INFINITY; 2];
        let mut high = [f64::NEG_INFINITY; 2];
        for &i in members {
            let b = boxes[i];
            low = [low[0].min(b[0]), low[1].min(b[1])];
            high = [high[0].max(b[2]), high[1].max(b[3])];
        }
        let side = ((members.len() as f64).sqrt().ceil() as usize).clamp(1, 1024);
        let span = [(high[0] - low[0]).max(0.0), (high[1] - low[1]).max(0.0)];
        let usable = members.len() >= 32 && span[0].is_finite() && span[1].is_finite() && span[0] > 0.0 && span[1] > 0.0;
        let mut grid = Self {
            origin: low,
            cell: [span[0] / side as f64, span[1] / side as f64],
            side: if usable { side } else { 0 },
            cells: if usable { vec![Vec::new(); side * side] } else { Vec::new() },
            wide: Vec::new(),
        };
        if !usable {
            grid.wide = members.to_vec();
            return grid;
        }
        for &i in members {
            let b = boxes[i];
            let (x0, y0) = (grid.coordinate(b[0], 0), grid.coordinate(b[1], 1));
            let (x1, y1) = (grid.coordinate(b[2], 0), grid.coordinate(b[3], 1));
            if (x1 - x0 + 1) * (y1 - y0 + 1) > Self::WIDE {
                grid.wide.push(i);
                continue;
            }
            for y in y0..=y1 {
                for x in x0..=x1 {
                    grid.cells[y * side + x].push(i);
                }
            }
        }
        grid
    }

    fn coordinate(&self, value: f64, axis: usize) -> usize {
        let at = ((value - self.origin[axis]) / self.cell[axis]).floor();
        if at.is_nan() || at < 0.0 {
            0
        } else {
            (at as usize).min(self.side - 1)
        }
    }

    fn size(&self) -> usize {
        self.cells.iter().map(Vec::len).sum::<usize>() + self.wide.len()
    }

    /// Every box that may contain `p` (a superset; boxes are re-tested).
    fn query(&self, p: [f64; 2], out: &mut Vec<usize>) {
        out.extend_from_slice(&self.wide);
        if self.side > 0 {
            let (x, y) = (self.coordinate(p[0], 0), self.coordinate(p[1], 1));
            out.extend_from_slice(&self.cells[y * self.side + x]);
        }
    }
}

fn seg_dist(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 { (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
    ((p[0] - a[0] - t * dx).powi(2) + (p[1] - a[1] - t * dy).powi(2)).sqrt()
}

/// Iterative Douglas-Peucker (no recursion depth limit on long scan loops).
fn douglas_peucker(points: &[[f64; 2]], tol: f64) -> Vec<[f64; 2]> {
    let n = points.len();
    if n < 3 {
        return points.to_vec();
    }
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;
    let mut stack = vec![(0, n - 1)];
    while let Some((s, e)) = stack.pop() {
        let mut best = (0.0, 0);
        for i in s + 1..e {
            let d = seg_dist(points[i], points[s], points[e]);
            if d > best.0 {
                best = (d, i);
            }
        }
        if best.0 > tol {
            keep[best.1] = true;
            stack.push((s, best.1));
            stack.push((best.1, e));
        }
    }
    points.iter().zip(keep).filter(|(_, k)| *k).map(|(p, _)| *p).collect()
}

/// Closed-loop DP: split at the vertex farthest from the first, simplify
/// both halves, rejoin. Keeps the original if simplification would collapse
/// the loop below a triangle.
fn simplify_closed(points: &[[f64; 2]], tol: f64) -> Vec<[f64; 2]> {
    let n = points.len();
    if n < 4 {
        return points.to_vec();
    }
    let p0 = points[0];
    let far = (1..n)
        .max_by(|&i, &j| {
            let di = (points[i][0] - p0[0]).hypot(points[i][1] - p0[1]);
            let dj = (points[j][0] - p0[0]).hypot(points[j][1] - p0[1]);
            di.total_cmp(&dj)
        })
        .unwrap_or(n / 2);
    let first = douglas_peucker(&points[..=far], tol);
    let mut second_in: Vec<[f64; 2]> = points[far..].to_vec();
    second_in.push(p0);
    let second = douglas_peucker(&second_in, tol);
    let mut out = first;
    out.extend_from_slice(&second[1..second.len() - 1]);
    // The split vertex was kept unconditionally; drop it if it is redundant.
    if out.len() > 3 && seg_dist(out[0], out[out.len() - 1], out[1]) <= tol {
        out.remove(0);
    }
    if out.len() >= 3 {
        out
    } else {
        points.to_vec()
    }
}

// ---------------------------------------------------------------------------
// Ray hits

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RayHit {
    pub point: [f64; 3],
    pub normal: [f64; 3],
    pub distance: f64,
    /// True when the ray hits the outside of the surface (enters material).
    pub entering: bool,
}

/// Converts the engine's triangle soup into an indexed mesh (welding exact
/// duplicates) so analysis can run on compiled design objects too.
pub fn from_engine_mesh(mesh: &crate::engine::Mesh) -> TriMesh {
    let mut index = std::collections::HashMap::<[u64; 3], u32>::new();
    let mut out = TriMesh::default();
    for triangle in &mesh.triangles {
        let mut ids = [0u32; 3];
        for (slot, vertex) in triangle.vertices.iter().enumerate() {
            let point = [vertex.x, vertex.y, vertex.z];
            let key = point.map(f64::to_bits);
            ids[slot] = *index.entry(key).or_insert_with(|| {
                out.points.push(point);
                (out.points.len() - 1) as u32
            });
        }
        out.triangles.push(ids);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    /// Appends a triangle, flipping it so its normal agrees with `hint`
    /// (generators then need not get winding right by hand).
    fn tri(m: &mut TriMesh, a: u32, b: u32, c: u32, hint: V3) {
        let [pa, pb, pc] = [a, b, c].map(|i| m.points[i as usize]);
        if dot(cross(sub(pb, pa), sub(pc, pa)), hint) >= 0.0 {
            m.triangles.push([a, b, c]);
        } else {
            m.triangles.push([a, c, b]);
        }
    }
    fn quad(m: &mut TriMesh, a: u32, b: u32, c: u32, d: u32, hint: V3) {
        tri(m, a, b, c, hint);
        tri(m, a, c, d, hint);
    }
    fn pt(m: &mut TriMesh, p: V3) -> u32 {
        m.points.push(p);
        (m.points.len() - 1) as u32
    }

    fn box_mesh(lo: V3, hi: V3) -> TriMesh {
        let mut m = TriMesh::default();
        for i in 0..8 {
            m.points.push([
                if i & 1 == 0 { lo[0] } else { hi[0] },
                if i & 2 == 0 { lo[1] } else { hi[1] },
                if i & 4 == 0 { lo[2] } else { hi[2] },
            ]);
        }
        quad(&mut m, 0, 1, 3, 2, [0.0, 0.0, -1.0]);
        quad(&mut m, 4, 5, 7, 6, [0.0, 0.0, 1.0]);
        quad(&mut m, 0, 1, 5, 4, [0.0, -1.0, 0.0]);
        quad(&mut m, 2, 3, 7, 6, [0.0, 1.0, 0.0]);
        quad(&mut m, 0, 2, 6, 4, [-1.0, 0.0, 0.0]);
        quad(&mut m, 1, 3, 7, 5, [1.0, 0.0, 0.0]);
        m
    }

    fn uv_sphere(r: f64, seg: usize, rings: usize) -> TriMesh {
        let mut m = TriMesh::default();
        let top = pt(&mut m, [0.0, 0.0, r]);
        let bottom = pt(&mut m, [0.0, 0.0, -r]);
        let mut grid = Vec::new();
        for i in 1..rings {
            let phi = PI * i as f64 / rings as f64;
            let row: Vec<u32> = (0..seg)
                .map(|j| {
                    let th = 2.0 * PI * j as f64 / seg as f64;
                    pt(&mut m, [r * phi.sin() * th.cos(), r * phi.sin() * th.sin(), r * phi.cos()])
                })
                .collect();
            grid.push(row);
        }
        for j in 0..seg {
            let k = (j + 1) % seg;
            let h = m.points[grid[0][j] as usize];
            tri(&mut m, top, grid[0][j], grid[0][k], h);
            let last = grid.len() - 1;
            let h = m.points[grid[last][j] as usize];
            tri(&mut m, bottom, grid[last][j], grid[last][k], h);
            for i in 0..grid.len() - 1 {
                let h = m.points[grid[i][j] as usize];
                quad(&mut m, grid[i][j], grid[i][k], grid[i + 1][k], grid[i + 1][j], h);
            }
        }
        m
    }

    fn noise(i: u64) -> f64 {
        let mut z = i.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }

    /// Closed cylinder along Z, base at z = 0; every vertex jittered by up to
    /// `amp` per axis to imitate scan noise.
    fn cylinder(r: f64, h: f64, seg: usize, rings: usize, amp: f64) -> TriMesh {
        let mut m = TriMesh::default();
        let mut grid = Vec::new();
        for i in 0..=rings {
            let z = h * i as f64 / rings as f64;
            let row: Vec<u32> = (0..seg)
                .map(|j| {
                    let th = 2.0 * PI * j as f64 / seg as f64;
                    pt(&mut m, [r * th.cos(), r * th.sin(), z])
                })
                .collect();
            grid.push(row);
        }
        let cb = pt(&mut m, [0.0, 0.0, 0.0]);
        let ct = pt(&mut m, [0.0, 0.0, h]);
        for (i, p) in m.points.iter_mut().enumerate() {
            for (k, c) in p.iter_mut().enumerate() {
                *c += amp * noise((i * 3 + k) as u64);
            }
        }
        for j in 0..seg {
            let k = (j + 1) % seg;
            tri(&mut m, cb, grid[0][j], grid[0][k], [0.0, 0.0, -1.0]);
            tri(&mut m, ct, grid[rings][j], grid[rings][k], [0.0, 0.0, 1.0]);
            for i in 0..rings {
                let th = 2.0 * PI * (j as f64 + 0.5) / seg as f64;
                quad(&mut m, grid[i][j], grid[i][k], grid[i + 1][k], grid[i + 1][j], [th.cos(), th.sin(), 0.0]);
            }
        }
        m
    }

    /// Box [-hx,hx]x[-hy,hy]x[0,h] with a through-hole of radius r along Z.
    fn box_with_hole(hx: f64, hy: f64, h: f64, r: f64, seg: usize) -> TriMesh {
        let mut angles: Vec<f64> = (0..seg).map(|i| 2.0 * PI * i as f64 / seg as f64).collect();
        for (sx, sy) in [(1.0, 1.0), (-1.0, 1.0), (-1.0, -1.0), (1.0, -1.0)] {
            angles.push((sy * hy).atan2(sx * hx).rem_euclid(2.0 * PI));
        }
        angles.sort_by(f64::total_cmp);
        angles.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
        let mut m = TriMesh::default();
        let mut ring = Vec::new();
        for &th in &angles {
            let (c, s) = (th.cos(), th.sin());
            let k = (hx / c.abs()).min(hy / s.abs());
            let ids = [0.0, h].map(|z| (pt(&mut m, [r * c, r * s, z]), pt(&mut m, [k * c, k * s, z])));
            ring.push(ids);
        }
        let n = ring.len();
        for i in 0..n {
            let j = (i + 1) % n;
            let [(cb0, sb0), (ct0, st0)] = ring[i];
            let [(cb1, sb1), (ct1, st1)] = ring[j];
            quad(&mut m, ct0, st0, st1, ct1, [0.0, 0.0, 1.0]);
            quad(&mut m, cb0, sb0, sb1, cb1, [0.0, 0.0, -1.0]);
            let mid = mul(add(m.points[sb0 as usize], m.points[sb1 as usize]), 0.5);
            quad(&mut m, sb0, sb1, st1, st0, [mid[0], mid[1], 0.0]);
            let mid = mul(add(m.points[cb0 as usize], m.points[cb1 as usize]), 0.5);
            quad(&mut m, cb0, cb1, ct1, ct0, [-mid[0], -mid[1], 0.0]);
        }
        m
    }

    fn rotation(ax: f64, ay: f64, az: f64) -> [[f64; 3]; 3] {
        let (sa, ca) = ax.sin_cos();
        let (sb, cb) = ay.sin_cos();
        let (sc, cc) = az.sin_cos();
        let rx = [[1.0, 0.0, 0.0], [0.0, ca, -sa], [0.0, sa, ca]];
        let ry = [[cb, 0.0, sb], [0.0, 1.0, 0.0], [-sb, 0.0, cb]];
        let rz = [[cc, -sc, 0.0], [sc, cc, 0.0], [0.0, 0.0, 1.0]];
        let mm = |a: [[f64; 3]; 3], b: [[f64; 3]; 3]| {
            let mut o = [[0.0; 3]; 3];
            for i in 0..3 {
                for j in 0..3 {
                    o[i][j] = (0..3).map(|k| a[i][k] * b[k][j]).sum();
                }
            }
            o
        };
        mm(rz, mm(ry, rx))
    }

    fn transform(m: &TriMesh, r: [[f64; 3]; 3], t: V3) -> TriMesh {
        TriMesh {
            points: m.points.iter().map(|p| add([dot(r[0], *p), dot(r[1], *p), dot(r[2], *p)], t)).collect(),
            triangles: m.triangles.clone(),
        }
    }

    fn apply(m: &[[f64; 4]; 4], p: V3) -> V3 {
        [0, 1, 2].map(|k| m[k][0] * p[0] + m[k][1] * p[1] + m[k][2] * p[2] + m[k][3])
    }

    fn planes(d: &Description) -> usize {
        d.features.iter().filter(|f| matches!(f, Feature::Plane { .. })).count()
    }

    #[test]
    fn eigen_solver_recovers_known_axes() {
        let r = rotation(0.3, -0.7, 1.1);
        let vals = [5.0, 2.0, 0.5];
        let mut m = [[0.0; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                m[i][j] = (0..3).map(|k| r[i][k] * vals[k] * r[j][k]).sum();
            }
        }
        let (v, vecs) = eigen_sorted(m);
        assert!((v[0] - 0.5).abs() < 1e-12 && (v[2] - 5.0).abs() < 1e-12);
        let col0 = [r[0][2], r[1][2], r[2][2]];
        assert!((dot(vecs[0], col0).abs() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn box_basics() {
        let d = describe(&box_mesh([0.0; 3], [40.0, 30.0, 20.0]), DescribeOptions::default());
        assert!(d.watertight);
        assert!((d.volume - 24000.0).abs() < 1e-6);
        assert!((d.surface_area - 2.0 * (1200.0 + 800.0 + 600.0)).abs() < 1e-6);
        assert_eq!(d.size, [40.0, 30.0, 20.0]);
        assert_eq!(d.units.likely, "mm");
        assert_eq!(planes(&d), 6);
        assert!((d.principal_axes.extents[0] - 40.0).abs() < 1e-9);
        assert!((d.center_of_mass[2] - 10.0).abs() < 1e-9);
        let s = d.symmetry.expect("box is symmetric");
        assert!(s.deviation < 1e-3, "{s:?}");
        assert!(d.notes.is_empty(), "{:?}", d.notes);
    }

    #[test]
    fn meters_are_flagged() {
        let d = describe(&box_mesh([0.0; 3], [0.04, 0.03, 0.02]), DescribeOptions::default());
        assert_eq!(d.units.likely, "m");
        assert_eq!(d.units.suggested_scale, 1000.0);
        assert!(d.notes.iter().any(|n| n.contains("multiply by 1000")), "{:?}", d.notes);
    }

    #[test]
    fn box_with_hole_finds_six_planes_and_bore() {
        let m = box_with_hole(20.0, 15.0, 20.0, 5.0, 48);
        let d = describe(&m, DescribeOptions::default());
        assert!(d.watertight, "{:?}", d.notes);
        assert_eq!(planes(&d), 6, "{:#?}", d.features);
        let cyls: Vec<_> = d
            .features
            .iter()
            .filter_map(|f| match f {
                Feature::Cylinder { radius, hole, length, coverage_deg, axis, .. } => {
                    Some((*radius, *hole, *length, *coverage_deg, *axis))
                }
                _ => None,
            })
            .collect();
        assert_eq!(cyls.len(), 1, "{:#?}", d.features);
        let (r, hole, len, cov, axis) = cyls[0];
        assert!(hole);
        assert!((r - 5.0).abs() < 0.1, "radius {r}");
        assert!((len - 20.0).abs() < 1e-6);
        assert_eq!(cov, 360.0);
        assert!(axis[2].abs() > 0.9999);
        assert!(d.features.len() == 7);
    }

    #[test]
    fn noisy_cylinder_boss() {
        let m = cylinder(10.0, 30.0, 64, 30, 0.05);
        let d = describe(&m, DescribeOptions::default());
        let cyl = d
            .features
            .iter()
            .find_map(|f| match f {
                Feature::Cylinder { radius, hole, axis, rms, .. } => Some((*radius, *hole, *axis, *rms)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{:#?}", d.features));
        assert!(!cyl.1, "boss, not a hole");
        assert!((cyl.0 - 10.0).abs() < 0.2, "radius {}", cyl.0);
        assert!(cyl.2[2].abs() > 0.999);
        assert!(cyl.3 < 0.1);
        assert_eq!(planes(&d), 2, "{:#?}", d.features);
    }

    #[test]
    fn sphere_fit() {
        let m = uv_sphere(10.0, 48, 24);
        let d = describe(&m, DescribeOptions::default());
        assert!(d.watertight);
        assert!((d.volume - 4.0 / 3.0 * PI * 1000.0).abs() / 4188.79 < 0.02);
        let sphere = d.features.iter().find_map(|f| match f {
            Feature::Sphere { radius, hole, .. } => Some((*radius, *hole)),
            _ => None,
        });
        let (r, hole) = sphere.unwrap_or_else(|| panic!("{:#?}", d.features));
        assert!((r - 10.0).abs() < 0.2 && !hole);
        assert!(d.lay_flat.is_empty() || !d.lay_flat[0].stable || d.lay_flat[0].contact_area < 5.0);
    }

    #[test]
    fn rotated_box_align_and_lay_flat() {
        let r = rotation(0.5, 0.35, 0.2);
        let m = transform(&box_mesh([-20.0, -10.0, -5.0], [20.0, 10.0, 5.0]), r, [7.0, -3.0, 12.0]);
        let d = describe(&m, DescribeOptions::default());
        let pa = &d.principal_axes;
        for (e, want) in pa.extents.iter().zip([40.0, 20.0, 10.0]) {
            assert!((e - want).abs() < 1e-6, "{:?}", pa.extents);
        }
        assert!((dot(cross(pa.axes[0], pa.axes[1]), pa.axes[2]) - 1.0).abs() < 1e-9);
        let mut lo = [f64::INFINITY; 3];
        let mut hi = [f64::NEG_INFINITY; 3];
        for p in &m.points {
            let q = apply(&pa.align_matrix, *p);
            for k in 0..3 {
                lo[k] = lo[k].min(q[k]);
                hi[k] = hi[k].max(q[k]);
            }
        }
        for k in 0..3 {
            assert!((hi[k] - pa.extents[k] / 2.0).abs() < 1e-6 && (lo[k] + pa.extents[k] / 2.0).abs() < 1e-6);
        }
        let best = &d.lay_flat[0];
        assert!((best.contact_area - 800.0).abs() < 1e-6, "{:?}", d.lay_flat);
        assert!((best.height - 10.0).abs() < 1e-6);
        assert!(best.stable);
        let mut lo = [f64::INFINITY; 3];
        let mut hi = [f64::NEG_INFINITY; 3];
        for p in &m.points {
            let q = apply(&best.matrix, *p);
            for k in 0..3 {
                lo[k] = lo[k].min(q[k]);
                hi[k] = hi[k].max(q[k]);
            }
        }
        assert!(lo[2].abs() < 1e-6 && (hi[2] - 10.0).abs() < 1e-6);
        assert!((lo[0] + hi[0]).abs() < 1e-6 && (lo[1] + hi[1]).abs() < 1e-6);
        assert_eq!(d.lay_flat.len(), 4);
    }

    #[test]
    fn section_box_with_hole() {
        let m = box_with_hole(20.0, 15.0, 20.0, 5.0, 48);
        let s = section(&m, [0.0, 0.0, 10.0], [0.0, 0.0, 1.0], 0.0);
        assert_eq!(s.loops.len(), 2, "{:?}", s.loops);
        assert!(s.loops.iter().all(|l| l.closed));
        let (outer, hole) = (&s.loops[0], &s.loops[1]);
        assert!(!outer.hole && (outer.area - 1200.0).abs() < 1e-6);
        assert!(hole.hole && hole.area < 0.0 && (hole.area.abs() - PI * 25.0).abs() < 1.0);
        assert!((dot(cross(s.u, s.v), [0.0, 0.0, 1.0]) - 1.0).abs() < 1e-12);
        // Simplification drops the collinear side subdivisions of the outer loop.
        let simple = section(&m, [0.0, 0.0, 10.0], [0.0, 0.0, 1.0], 0.01);
        assert_eq!(simple.loops[0].points.len(), 4);
        // A plane through a ring of vertices (the top face) still yields the outline.
        let top = section(&m, [0.0, 0.0, 20.0], [0.0, 0.0, 1.0], 0.0);
        assert_eq!(top.loops.len(), 2, "{:?}", top.loops);
        let bottom = section(&m, [0.0, 0.0, 0.0], [0.0, 0.0, 1.0], 0.0);
        assert_eq!(bottom.loops.len(), 2, "{:?}", bottom.loops);
        // Vertical cut through the hole: two rectangles.
        let side = section(&m, [0.0, 0.0, 0.0], [0.0, 1.0, 0.0], 0.0);
        assert_eq!(side.loops.len(), 2);
        assert!(side.loops.iter().all(|l| !l.hole && (l.area - 15.0 * 20.0).abs() < 1e-6));
    }

    #[test]
    fn raycast_box() {
        let m = box_mesh([0.0; 3], [10.0, 10.0, 10.0]);
        let index = MeshIndex::new(&m);
        let hits = [
            ([5.0, 5.0, -10.0], [0.0, 0.0, 2.0]),
            ([5.0, 5.0, 5.0], [1.0, 0.0, 0.0]),
            ([50.0, 5.0, 5.0], [0.0, 1.0, 0.0]),
            ([5.0, 5.0, 5.0], [0.0, 0.0, 0.0]),
        ]
        .map(|(origin, direction)| index.raycast(origin, direction));
        let h = hits[0].unwrap();
        assert!((h.distance - 10.0).abs() < 1e-9 && h.entering);
        assert!((h.normal[2] + 1.0).abs() < 1e-9);
        let h = hits[1].unwrap();
        assert!((h.distance - 5.0).abs() < 1e-9 && !h.entering);
        assert!(hits[2].is_none() && hits[3].is_none());
    }

    #[test]
    fn open_and_degenerate_meshes_do_not_panic() {
        let mut m = box_mesh([0.0; 3], [10.0, 10.0, 10.0]);
        m.triangles.truncate(10); // drop the +X face
        let d = describe(&m, DescribeOptions::default());
        assert!(!d.watertight);
        assert!(d.notes.iter().any(|n| n.starts_with("Open mesh: 1 boundary loop")), "{:?}", d.notes);
        let s = section(&m, [5.0, 5.0, 5.0], [0.0, 0.0, 1.0], 0.0);
        assert_eq!(s.loops.len(), 1);
        assert!(!s.loops[0].closed && s.loops[0].points.len() >= 4, "{:?}", s.loops);
        assert_eq!(s.loops[0].points.first(), Some(&[5.0, 5.0]));

        let empty = TriMesh::default();
        let d = describe(&empty, DescribeOptions::default());
        assert!(d.notes.iter().any(|n| n.contains("Empty")));
        assert!(section(&empty, [0.0; 3], [0.0, 0.0, 1.0], 0.1).loops.is_empty());
        assert!(MeshIndex::new(&empty).raycast([0.0; 3], [1.0, 0.0, 0.0]).is_none());

        let junk = TriMesh {
            points: vec![[0.0; 3], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0], [f64::NAN, 0.0, 0.0]],
            triangles: vec![[0, 1, 2], [0, 0, 1], [0, 1, 3], [0, 1, 9]],
        };
        let d = describe(&junk, DescribeOptions::default());
        assert!(!d.notes.is_empty());
        let _ = section(&junk, [0.5, 0.0, 0.0], [1.0, 0.0, 0.0], 0.0);
        let _ = section(&m, [0.0; 3], [0.0; 3], 0.0);
        let _ = MeshIndex::new(&junk).raycast([0.0, 0.0, -1.0], [0.0, 0.0, 1.0]);

        // Unwelded soup: same box, every triangle with its own vertices.
        let b = box_mesh([0.0; 3], [10.0, 10.0, 10.0]);
        let mut soup = TriMesh::default();
        for t in &b.triangles {
            let base = soup.points.len() as u32;
            soup.points.extend(t.iter().map(|&i| b.points[i as usize]));
            soup.triangles.push([base, base + 1, base + 2]);
        }
        assert!(describe(&soup, DescribeOptions::default()).watertight);
    }

    /// ~200k triangle noisy torus: no primitive matches it, so the feature
    /// search runs to its budget (the worst case), unlike a clean CAD part.
    #[test]
    fn timing_200k() {
        let (seg_u, seg_v) = (500usize, 200usize);
        let (big, small) = (40.0, 12.0);
        let mut m = TriMesh::default();
        for i in 0..seg_u {
            let a = 2.0 * PI * i as f64 / seg_u as f64;
            for j in 0..seg_v {
                let b = 2.0 * PI * j as f64 / seg_v as f64;
                let rr = big + small * b.cos();
                let n = 0.02 * noise((i * seg_v + j) as u64);
                m.points.push([rr * a.cos() + n, rr * a.sin(), small * b.sin()]);
            }
        }
        let id = |i: usize, j: usize| ((i % seg_u) * seg_v + (j % seg_v)) as u32;
        for i in 0..seg_u {
            for j in 0..seg_v {
                let a = 2.0 * PI * (i as f64 + 0.5) / seg_u as f64;
                let b = 2.0 * PI * (j as f64 + 0.5) / seg_v as f64;
                let hint = [b.cos() * a.cos(), b.cos() * a.sin(), b.sin()];
                quad(&mut m, id(i, j), id(i + 1, j), id(i + 1, j + 1), id(i, j + 1), hint);
            }
        }
        assert_eq!(m.triangles.len(), 200_000);
        let t = std::time::Instant::now();
        let d = describe(&m, DescribeOptions::default());
        let t_describe = t.elapsed();
        let t = std::time::Instant::now();
        let _ = describe(&m, DescribeOptions { features: false, ..Default::default() });
        let t_no_features = t.elapsed();
        assert!(d.watertight);
        let t = std::time::Instant::now();
        let index = MeshIndex::new(&m);
        let t_index = t.elapsed();
        let t = std::time::Instant::now();
        let mut hits = 0;
        for k in 0..1000 {
            let a = 2.0 * PI * k as f64 / 1000.0;
            // k = 0 is axis-parallel in Y: exercises the slab-test NaN case.
            hits += usize::from(index.raycast([0.0, 0.0, 0.0], [a.cos(), a.sin(), 0.05]).is_some());
        }
        let t_rays = t.elapsed();
        assert_eq!(hits, 1000);
        let t = std::time::Instant::now();
        let s = section(&m, [0.0; 3], [0.0, 0.0, 1.0], 0.01);
        let t_section = t.elapsed();
        assert_eq!(s.loops.len(), 2);
        println!(
            "200k: describe {t_describe:?} (without features {t_no_features:?}), index {t_index:?}, 1000 rays {t_rays:?}, section {t_section:?}",
        );
        // A torus is neither plane, cylinder nor sphere: nothing may be claimed.
        assert!(d.features.is_empty(), "{:?}", d.features);
        assert!(d.symmetry.is_some());
        // Generous: the point is "no blow-up", and a loaded machine is slow.
        assert!(t_describe.as_secs_f64() < 10.0 || cfg!(debug_assertions), "describe took {t_describe:?}");
    }

    /// A phone-scan-like lumpy egg (doubly curved everywhere, +-0.2 mm
    /// noise, open base) has no cylinder anywhere, but along its parabolic
    /// lines short arcs fit one within noise. None may be claimed, at any
    /// cleaning budget.
    #[test]
    fn lumpy_egg_scan_has_no_cylinders() {
        let (n, m) = (250usize, 240usize);
        let mut seed = 3u64;
        let mut jitter = || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 11) as f64 / (1u64 << 53) as f64) * 0.4 - 0.2
        };
        let mut raw = TriMesh::default();
        for i in 0..=n {
            for j in 0..m {
                let t = PI * 0.92 * i as f64 / n as f64;
                let p = 2.0 * PI * j as f64 / m as f64;
                let r = 40.0 * (1.0 + 0.15 * (3.0 * p).cos() * t.sin()) + jitter();
                raw.points.push([r * t.sin() * p.cos() * 1.3, r * t.sin() * p.sin(), 50.0 * t.cos() + 50.0]);
            }
        }
        let id = |i: usize, j: usize| (i * m + j % m) as u32;
        for i in 0..n {
            for j in 0..m {
                let (a, b, c, d) = (id(i, j), id(i, j + 1), id(i + 1, j + 1), id(i + 1, j));
                raw.triangles.push([a, d, c]);
                raw.triangles.push([a, c, b]);
            }
        }
        // Closed over its open base with a fan: the shape is what is judged.
        let base = raw.points.len() as u32;
        raw.points.push([0.0, 0.0, 50.0 * (PI * 0.92).cos() + 50.0]);
        for j in 0..m {
            raw.triangles.push([id(n, j), id(n, j + 1), base]);
        }
        for budget in [20_000, 60_000, 100_000] {
            let options = crate::meshimport::CleanOptions { triangle_budget: budget };
            let (mesh, _) = crate::meshimport::clean(raw.clone(), options);
            let d = describe(&mesh, DescribeOptions::default());
            let curved: Vec<_> = d
                .features
                .iter()
                .filter(|f| matches!(f, Feature::Cylinder { .. } | Feature::Cone { .. }))
                .collect();
            assert!(curved.is_empty(), "budget {budget}: {curved:?}");
        }
    }

    /// Tube swept along `frame(s)` for s in 0..=1 (centre, unit tangent,
    /// radius), capped at both ends, every vertex jittered by up to `amp`.
    fn sweep(seg: usize, rings: usize, amp: f64, frame: impl Fn(f64) -> (V3, V3, f64)) -> TriMesh {
        let mut m = TriMesh::default();
        let mut grid = Vec::new();
        let mut ends = Vec::new();
        for i in 0..=rings {
            let (c, t, r) = frame(i as f64 / rings as f64);
            let (e1, e2) = plane_basis(unit(t));
            let row: Vec<u32> = (0..seg)
                .map(|j| {
                    let th = 2.0 * PI * j as f64 / seg as f64;
                    pt(&mut m, add(c, add(mul(e1, r * th.cos()), mul(e2, r * th.sin()))))
                })
                .collect();
            grid.push(row);
            if i == 0 || i == rings {
                ends.push((c, unit(t)));
            }
        }
        let cb = pt(&mut m, ends[0].0);
        let ct = pt(&mut m, ends[1].0);
        for (i, p) in m.points.iter_mut().enumerate() {
            for (k, c) in p.iter_mut().enumerate() {
                *c += amp * noise((i * 3 + k) as u64 + 7);
            }
        }
        for j in 0..seg {
            let k = (j + 1) % seg;
            tri(&mut m, cb, grid[0][j], grid[0][k], mul(ends[0].1, -1.0));
            tri(&mut m, ct, grid[rings][j], grid[rings][k], ends[1].1);
            for i in 0..rings {
                let mid = mul(add(m.points[grid[i][j] as usize], m.points[grid[i + 1][k] as usize]), 0.5);
                let (c, t, _) = frame((i as f64 + 0.5) / rings as f64);
                let hint = perp(sub(mid, c), unit(t));
                quad(&mut m, grid[i][j], grid[i][k], grid[i + 1][k], grid[i + 1][j], hint);
            }
        }
        m
    }

    /// Rotation by `angle` about unit `k` (Rodrigues).
    fn axis_rotation(k: V3, angle: f64) -> [[f64; 3]; 3] {
        let k = unit(k);
        let (s, c) = angle.sin_cos();
        let mut r = [[0.0; 3]; 3];
        let kx = [[0.0, -k[2], k[1]], [k[2], 0.0, -k[0]], [-k[1], k[0], 0.0]];
        for i in 0..3 {
            for j in 0..3 {
                r[i][j] = f64::from(u8::from(i == j)) * c + s * kx[i][j] + (1.0 - c) * k[i] * k[j];
            }
        }
        r
    }

    /// Handle-like body along +Z (0..100): frustum r = 12 - 0.04 z (half-angle
    /// 2.29°), with a lopsided flare at the base (z < 4) bulging towards +X.
    fn flared_handle(seg: usize, rings: usize, amp: f64) -> TriMesh {
        sweep(seg, rings, amp, |s| {
            let z = 100.0 * s;
            let f = if z < 4.0 { (1.0 - z / 4.0).powi(2) } else { 0.0 };
            ([3.0 * f, 0.0, z], [0.0, 0.0, 1.0], 12.0 - 0.04 * z + 4.0 * f)
        })
    }

    fn angle_deg(a: V3, b: V3) -> f64 {
        dot(unit(a), unit(b)).abs().min(1.0).acos().to_degrees()
    }

    /// fit_line with area weights, for comparison only.
    fn fit_line_weighted(cuts: &[CutSample], prev: V3) -> (V3, V3) {
        let w: f64 = cuts.iter().map(|c| c.area).sum();
        let mean = mul(cuts.iter().fold([0.0; 3], |acc, c| add(acc, mul(c.centroid, c.area))), 1.0 / w);
        let mut m = [[0.0; 3]; 3];
        for c in cuts {
            let d = sub(c.centroid, mean);
            for r in 0..3 {
                for s in 0..3 {
                    m[r][s] += c.area * d[r] * d[s];
                }
            }
        }
        let d = eigen_sorted(m).1[2];
        (mean, if dot(d, prev) < 0.0 { mul(d, -1.0) } else { d })
    }

    #[test]
    fn section_area_and_centroid() {
        let m = box_with_hole(20.0, 15.0, 20.0, 5.0, 48);
        let s = section(&m, [0.0, 0.0, 10.0], [0.0, 0.0, 1.0], 0.0);
        let net: f64 = s.loops.iter().map(|l| l.area).sum();
        assert!((s.area - net).abs() < 1e-9 && (s.area - (1200.0 - PI * 25.0)).abs() < 1.0, "{}", s.area);
        let c = s.centroid.expect("closed loops");
        assert!(norm(sub(c, [0.0, 0.0, 10.0])) < 1e-9, "{c:?}");
        let b = box_mesh([10.0, 0.0, 0.0], [30.0, 10.0, 10.0]);
        let s = section(&b, [0.0, 0.0, 5.0], [0.0, 0.0, 1.0], 0.0);
        assert!((s.area - 200.0).abs() < 1e-9);
        assert!(norm(sub(s.centroid.unwrap(), [20.0, 5.0, 5.0])) < 1e-9);
        assert!(norm(sub(s.loops[0].centroid3d, [20.0, 5.0, 5.0])) < 1e-9);
        // Open cut: no centroid, loop centroid is the mean of the chain.
        let mut open = box_mesh([0.0; 3], [10.0, 10.0, 10.0]);
        open.triangles.truncate(10);
        let s = section(&open, [5.0, 5.0, 5.0], [0.0, 0.0, 1.0], 0.0);
        assert!(s.centroid.is_none() && s.area == 0.0);
    }

    #[test]
    fn fit_axis_tilted_flared_frustum() {
        let tilt_axis = [0.7f64.cos(), 0.7f64.sin(), 0.0];
        let r = axis_rotation(tilt_axis, 1.8f64.to_radians());
        let m = transform(&flared_handle(96, 200, 0.05), r, [12.0, -40.0, 5.0]);
        let truth = [r[0][2], r[1][2], r[2][2]];
        let fit = fit_axis(&m, AxisOptions::default()).expect("fit");
        let err = angle_deg(fit.direction, truth);
        let pca = describe(&m, DescribeOptions { features: false, ..Default::default() }).principal_axes.axes[0];
        let pca_err = angle_deg(pca, truth);
        println!(
            "flared frustum: fit_axis error {err:.4}°, principal axis error {pca_err:.4}°, half-angle {:.4}° (want 2.2906), iterations {}, notes {:?}",
            fit.taper_half_angle_deg, fit.iterations, fit.notes
        );
        assert!(err < 0.05, "direction error {err}°");
        assert!(pca_err > 0.1 && pca_err > 4.0 * err, "surface PCA should be biased by the flare: {pca_err}°");
        assert!((fit.taper_half_angle_deg.abs() - 0.04f64.atan().to_degrees()).abs() < 0.2, "{}", fit.taper_half_angle_deg);
        assert!(fit.notes[0].starts_with("axis tilted 1.8") && fit.notes[0].contains("from Z"), "{:?}", fit.notes);
        assert!(fit.max_offset < 0.1, "{}", fit.max_offset);
        assert!(!fit.notes.iter().any(|n| n.starts_with("curved")));
        assert!((fit.angle_to_axes_deg[2] - 1.8).abs() < 0.05);
        assert_eq!(fit.sections.len(), 24);
        // align_matrix: the axis lands on +Z through the origin.
        let p = apply(&fit.align_matrix, fit.point);
        assert!(norm(p) < 1e-5);
        let q = apply(&fit.align_matrix, add(fit.point, mul(fit.direction, 10.0)));
        assert!(norm(sub(q, [0.0, 0.0, 10.0])) < 1e-5, "{q:?}");
        // Untrimmed, the flare enters the cuts: one vote per cut is more
        // robust than area weighting, which lets the widest (flared) cut dominate.
        let raw = fit_axis(&m, AxisOptions { trim: 0.0, ..Default::default() }).expect("fit");
        let weighted = {
            let cuts: Vec<CutSample> = raw
                .sections
                .iter()
                .map(|s| CutSample { centroid: s.centroid, area: s.area, max_radius: 0.0, bodies: 1, closed_open: false, excluded: false })
                .collect();
            fit_line_weighted(&cuts, raw.direction).1
        };
        let (raw_err, weighted_err) = (angle_deg(raw.direction, truth), angle_deg(weighted, truth));
        println!("untrimmed: unweighted error {raw_err:.4}°, area-weighted {weighted_err:.4}°");
        assert!(raw_err < 0.05 && raw_err < weighted_err, "{raw_err} vs {weighted_err}");
        assert!(raw.sections[0].excluded && raw.sections.iter().filter(|s| s.excluded).count() == 1, "{:?}", raw.sections);
        assert!(raw.notes.iter().any(|n| n.contains("left out of the fit")), "{:?}", raw.notes);
    }

    #[test]
    fn fit_axis_straight_cylinder() {
        let r = rotation(0.4, -0.25, 0.9);
        let m = transform(&cylinder(10.0, 80.0, 64, 40, 0.05), r, [3.0, 4.0, 5.0]);
        let truth = [r[0][2], r[1][2], r[2][2]];
        let fit = fit_axis(&m, AxisOptions::default()).expect("fit");
        let err = angle_deg(fit.direction, truth);
        assert!(err < 0.05, "{err}");
        assert!(fit.taper_half_angle_deg.abs() < 0.05, "{}", fit.taper_half_angle_deg);
        assert!(fit.max_offset < 0.05, "{}", fit.max_offset);
        assert!(fit.sections.iter().all(|s| (s.equivalent_radius - 10.0).abs() < 0.05 && (s.max_radius - 10.0).abs() < 0.2));
        assert!((fit.end_t - fit.start_t - 80.0).abs() < 0.3);
        // Starting from a poor guess still converges.
        let off = [truth[0] + 0.2, truth[1] - 0.1, truth[2]];
        let fit2 = fit_axis(&m, AxisOptions { initial: Some(off), ..Default::default() }).expect("fit");
        assert!(angle_deg(fit2.direction, truth) < 0.05);
        assert!(dot(fit2.direction, off) > 0.0);
        // A range outside the body is an error, not a panic.
        assert!(fit_axis(&m, AxisOptions { range: Some((500.0, 600.0)), ..Default::default() }).is_err());
        assert!(fit_axis(&TriMesh::default(), AxisOptions::default()).is_err());
    }

    #[test]
    fn fit_axis_bent_tube_reports_curvature() {
        let m = sweep(48, 120, 0.0, |s| {
            let th = s * 60f64.to_radians();
            ([100.0 * (1.0 - th.cos()), 0.0, 100.0 * th.sin()], [th.sin(), 0.0, th.cos()], 8.0)
        });
        let fit = fit_axis(&m, AxisOptions::default()).expect("fit");
        println!("bent tube: max offset {:.3} mm, notes {:?}", fit.max_offset, fit.notes);
        assert!(fit.max_offset > 2.0, "{}", fit.max_offset);
        assert!(fit.notes.iter().any(|n| n.starts_with("curved")), "{:?}", fit.notes);
    }

    #[test]
    fn fit_axis_timing_200k() {
        let m = transform(&flared_handle(500, 200, 0.05), rotation(0.03, 0.01, 0.0), [0.0; 3]);
        assert!(m.triangles.len() >= 200_000);
        let t = std::time::Instant::now();
        let fit = fit_axis(&m, AxisOptions::default()).expect("fit");
        let took = t.elapsed();
        println!("fit_axis 200k: {took:?}, {} iterations", fit.iterations);
        assert!(fit.iterations <= 6, "{}", fit.iterations);
        assert!(took.as_secs_f64() < 3.0 || cfg!(debug_assertions), "{took:?}");
    }

    fn cones(d: &Description) -> Vec<(V3, f64, f64, f64, bool, f64)> {
        d.features
            .iter()
            .filter_map(|f| match f {
                Feature::Cone { axis, half_angle_deg, radius_start, radius_end, hole, length, .. } => {
                    Some((*axis, *half_angle_deg, *radius_start, *radius_end, *hole, *length))
                }
                _ => None,
            })
            .collect()
    }

    /// The scan case: a noisy, tilted, tapered handle with a lopsided flare
    /// must read as one cone (not several cylinders with drifting axes).
    #[test]
    fn tapered_handle_is_one_cone() {
        let tilt = axis_rotation([0.3, -0.9, 0.0], 1.8f64.to_radians());
        let m = transform(&flared_handle(96, 200, 0.05), tilt, [5.0, 5.0, 5.0]);
        let truth = [tilt[0][2], tilt[1][2], tilt[2][2]];
        let t = std::time::Instant::now();
        let d = describe(&m, DescribeOptions::default());
        println!("tapered handle describe {:?}: {:#?}", t.elapsed(), d.features);
        let c = cones(&d);
        assert_eq!(c.len(), 1, "{:#?}", d.features);
        assert!(!d.features.iter().any(|f| matches!(f, Feature::Cylinder { .. })), "{:#?}", d.features);
        let (axis, ha, r0, r1, hole, len) = c[0];
        println!("cone axis error {:.4}°, half-angle {ha:.4}° (want 2.2906)", angle_deg(axis, truth));
        assert!(!hole);
        assert!(angle_deg(axis, truth) < 0.1, "axis off by {}°", angle_deg(axis, truth));
        assert!(dot(axis, truth) > 0.0, "axis runs wide -> narrow");
        assert!((ha - 0.04f64.atan().to_degrees()).abs() < 0.1, "half-angle {ha}");
        assert!(r0 > r1 && r0 < 12.2 && r1 > 7.8 && len > 85.0, "{r0} {r1} {len}");
        assert!(d.notes.iter().any(|n| n.starts_with("Elongated body")), "{:?}", d.notes);
    }

    /// Clean CAD cones: a frustum boss and a countersunk (tapered) bore.
    #[test]
    fn cad_frustum_and_cylinder_stay_apart() {
        let m = sweep(64, 10, 0.0, |s| ([0.0, 0.0, 40.0 * s], [0.0, 0.0, 1.0], 20.0 - 10.0 * s));
        let d = describe(&m, DescribeOptions::default());
        let c = cones(&d);
        assert_eq!(c.len(), 1, "{:#?}", d.features);
        let want = (10.0f64 / 40.0).atan().to_degrees();
        assert!((c[0].1 - want).abs() < 0.3, "{} vs {want}", c[0].1);
        assert!(c[0].0[2] > 0.9999, "{:?}", c[0].0);
        assert_eq!(planes(&d), 2, "{:#?}", d.features);
        // A plain cylinder stays a cylinder.
        let d = describe(&cylinder(10.0, 30.0, 64, 30, 0.05), DescribeOptions::default());
        assert!(cones(&d).is_empty(), "{:#?}", d.features);
    }

    /// A tapered bore (surface facing the axis) and a cone split by a gap
    /// in the scan: one cone each, the split halves merged.
    #[test]
    fn tapered_bore_and_split_cone() {
        let mut m = sweep(64, 20, 0.0, |s| ([0.0, 0.0, 40.0 * s], [0.0, 0.0, 1.0], 10.0 - 4.0 * s));
        m.triangles.iter_mut().for_each(|t| t.swap(1, 2));
        let d = describe(&m, DescribeOptions::default());
        let c = cones(&d);
        assert_eq!(c.len(), 1, "{:#?}", d.features);
        assert!(c[0].4, "bore faces the axis");
        assert!((c[0].1 - (4.0f64 / 40.0).atan().to_degrees()).abs() < 0.2);

        let mut h = flared_handle(96, 200, 0.05);
        let pts = h.points.clone();
        h.triangles.retain(|t| {
            let z = t.iter().map(|&i| pts[i as usize][2]).sum::<f64>() / 3.0;
            !(48.0..52.0).contains(&z)
        });
        let d = describe(&h, DescribeOptions::default());
        let c = cones(&d);
        assert_eq!(c.len(), 1, "{:#?}", d.features);
        assert!(c[0].5 > 85.0, "merged across the gap: length {}", c[0].5);
    }

    fn merged(parts: impl Iterator<Item = TriMesh>) -> TriMesh {
        let mut all = TriMesh::default();
        for part in parts {
            let base = all.points.len() as u32;
            all.points.extend(part.points);
            all.triangles.extend(part.triangles.iter().map(|t| t.map(|i| i + base)));
        }
        all
    }

    /// 10,000 disjoint prisms (120k triangles), every cut crossing all of
    /// them: the nesting test used to compare every outline with every
    /// other, 200 cuts x 12 iterations over, with nothing to stop it.
    #[test]
    fn fit_axis_on_ten_thousand_prisms_is_bounded() {
        let mesh = merged((0..10_000).map(|i| {
            let (x, y) = ((i % 100) as f64 * 3.0, (i / 100) as f64 * 3.0);
            box_mesh([x, y, 0.0], [x + 1.0, y + 1.0, 50.0])
        }));
        assert_eq!(mesh.triangles.len(), 120_000);
        let options = AxisOptions { initial: Some([0.0, 0.0, 1.0]), sections: 200, ..Default::default() };
        let started = std::time::Instant::now();
        let result = fit_axis(&mesh, options);
        let elapsed = started.elapsed();
        assert!(elapsed < std::time::Duration::from_secs(20), "took {elapsed:?}");
        let fit = result.expect("separate bodies are cheap to classify now");
        assert!(fit.direction[2] > 0.999, "{:?}", fit.direction);
    }

    /// Thousands of nested rings are inherently quadratic to classify, so
    /// the work budget has to end it: with the last full fit and a note, or
    /// an error when not even one iteration fits.
    #[test]
    fn fit_axis_on_deeply_nested_rings_runs_out_of_budget_promptly() {
        let rings = 3000;
        let mesh = merged((0..rings).flat_map(|i| {
            // A square tube: four walls, each a thin box.
            let (r, w) = (10.0 + i as f64 * 0.2, 0.05);
            [
                box_mesh([-r, -r, 0.0], [r, -r + w, 50.0]),
                box_mesh([-r, r - w, 0.0], [r, r, 50.0]),
                box_mesh([-r, -r + w, 0.0], [-r + w, r - w, 50.0]),
                box_mesh([r - w, -r + w, 0.0], [r, r - w, 50.0]),
            ]
        }));
        let options = AxisOptions { initial: Some([0.0, 0.0, 1.0]), sections: 200, ..Default::default() };
        let started = std::time::Instant::now();
        let mut work = Work::new(ANALYSIS_WORK / 10);
        let result = fit_axis_within(&mesh, options, &mut work);
        let elapsed = started.elapsed();
        assert!(elapsed < std::time::Duration::from_secs(10), "took {elapsed:?}");
        match result {
            Ok(fit) => assert!(fit.notes[0].starts_with("stopped early"), "{:?}", fit.notes),
            Err(error) => assert!(error.contains("work budget"), "{error}"),
        }
    }

    #[test]
    fn fit_axis_stops_promptly_when_cancelled() {
        let rings = 3000;
        let mesh = merged((0..rings).map(|i| {
            let r = 10.0 + i as f64 * 0.2;
            box_mesh([-r, -r, 0.0], [r, r, 50.0])
        }));
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _scope = crate::csg::bsp::cancellation_scope(std::sync::Arc::clone(&flag));
        let tripper = {
            let flag = std::sync::Arc::clone(&flag);
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(200));
                flag.store(true, std::sync::atomic::Ordering::Release);
                std::time::Instant::now()
            })
        };
        let options = AxisOptions { initial: Some([0.0, 0.0, 1.0]), sections: 200, ..Default::default() };
        let result = fit_axis(&mesh, options);
        let finished = std::time::Instant::now();
        let tripped = tripper.join().unwrap();
        let overrun = finished.saturating_duration_since(tripped);
        assert!(overrun < std::time::Duration::from_millis(500), "ran {overrun:?} past the cancel");
        match result {
            Ok(fit) => assert!(fit.notes[0].contains("Render cancelled."), "{:?}", fit.notes),
            Err(error) => assert_eq!(error, "Render cancelled."),
        }
    }
}
