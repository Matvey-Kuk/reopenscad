//! Exact measurements on compiled triangle meshes.
//!
//! Everything here works on the meshes the engine hands back — the geometry a
//! user actually exports — rather than on the evaluator's shape tree. That is
//! the point: the shape tree's signed-distance functions are bounds, not
//! distances, once a model goes through `difference`, `hull`, non-uniform
//! `scale` and friends, so anything measured through them can be off by a
//! large fraction of a millimetre (see [`mesh_distance`]). A mesh is the
//! ground truth, and on a mesh the questions "how far apart", "how much
//! overlap", "how big" all have exact answers.
//!
//! * [`MeshIndex`] – a triangle BVH supporting nearest-point, mesh-mesh
//!   distance and inside/outside queries.
//! * [`measure_mesh`] – bounds, volume, area, topology (watertight, manifold,
//!   winding, connected components).
//! * [`boolean_overlap_measured`] / [`boolean_difference`] – exact booleans through the
//!   CSG kernel, guarded by a triangle budget and a panic catch.

use crate::vec3::{add, cross, dot, scale as mul, sub};
use crate::csg;
use crate::engine::Mesh;

pub type P = [f64; 3];

fn length2(a: P) -> f64 {
    dot(a, a)
}

/// Axis-aligned box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aabb {
    pub min: P,
    pub max: P,
}

impl Aabb {
    pub fn empty() -> Self {
        Self {
            min: [f64::INFINITY; 3],
            max: [f64::NEG_INFINITY; 3],
        }
    }

    pub fn is_empty(&self) -> bool {
        (0..3).any(|axis| self.min[axis] > self.max[axis])
    }

    pub fn grow(&mut self, point: P) {
        for axis in 0..3 {
            self.min[axis] = self.min[axis].min(point[axis]);
            self.max[axis] = self.max[axis].max(point[axis]);
        }
    }

    pub fn merge(&mut self, other: &Aabb) {
        if other.is_empty() {
            return;
        }
        self.grow(other.min);
        self.grow(other.max);
    }

    /// Smallest distance between two boxes (0 when they touch or overlap).
    fn distance2(&self, other: &Aabb) -> f64 {
        let mut total = 0.0;
        for axis in 0..3 {
            let gap = (self.min[axis] - other.max[axis])
                .max(other.min[axis] - self.max[axis])
                .max(0.0);
            total += gap * gap;
        }
        total
    }

    fn point_distance2(&self, point: P) -> f64 {
        let mut total = 0.0;
        for axis in 0..3 {
            let gap = (self.min[axis] - point[axis])
                .max(point[axis] - self.max[axis])
                .max(0.0);
            total += gap * gap;
        }
        total
    }

    fn longest_axis(&self) -> usize {
        let size = sub(self.max, self.min);
        if size[0] >= size[1] && size[0] >= size[2] {
            0
        } else if size[1] >= size[2] {
            1
        } else {
            2
        }
    }
}

pub fn mesh_triangles(mesh: &Mesh) -> Vec<[P; 3]> {
    mesh.triangles
        .iter()
        .map(|triangle| {
            triangle
                .vertices
                .map(|vertex| [vertex.x, vertex.y, vertex.z])
        })
        .filter(|corners| {
            corners
                .iter()
                .all(|corner| corner.iter().all(|v| v.is_finite()))
        })
        .collect()
}

pub fn triangles_bounds(triangles: &[[P; 3]]) -> Aabb {
    let mut bounds = Aabb::empty();
    for triangle in triangles {
        for corner in triangle {
            bounds.grow(*corner);
        }
    }
    bounds
}

// ---------------------------------------------------------------------------
// Primitive distance tests (after Ericson, "Real-Time Collision Detection").
// ---------------------------------------------------------------------------

/// Closest point to `p` on triangle `abc`.
pub fn closest_point_on_triangle(p: P, a: P, b: P, c: P) -> P {
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
        let denominator = d1 - d3;
        let v = if denominator != 0.0 {
            d1 / denominator
        } else {
            0.0
        };
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
        let denominator = d2 - d6;
        let w = if denominator != 0.0 {
            d2 / denominator
        } else {
            0.0
        };
        return add(a, mul(ac, w));
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        let denominator = (d4 - d3) + (d5 - d6);
        let w = if denominator != 0.0 {
            (d4 - d3) / denominator
        } else {
            0.0
        };
        return add(b, mul(sub(c, b), w));
    }
    let sum = va + vb + vc;
    if sum == 0.0 || !sum.is_finite() {
        // Degenerate (zero-area) triangle: the nearest of its three edges.
        let candidates = [
            closest_point_on_segment(p, a, b),
            closest_point_on_segment(p, b, c),
            closest_point_on_segment(p, c, a),
        ];
        return candidates
            .into_iter()
            .min_by(|x, y| length2(sub(p, *x)).total_cmp(&length2(sub(p, *y))))
            .unwrap_or(a);
    }
    let denominator = 1.0 / sum;
    let v = vb * denominator;
    let w = vc * denominator;
    add(a, add(mul(ab, v), mul(ac, w)))
}

fn closest_point_on_segment(p: P, a: P, b: P) -> P {
    let ab = sub(b, a);
    let denominator = length2(ab);
    if denominator <= 0.0 {
        return a;
    }
    let t = (dot(sub(p, a), ab) / denominator).clamp(0.0, 1.0);
    add(a, mul(ab, t))
}

/// Closest points between segments `p1q1` and `p2q2`.
pub fn closest_points_segments(p1: P, q1: P, p2: P, q2: P) -> (P, P) {
    let d1 = sub(q1, p1);
    let d2 = sub(q2, p2);
    let r = sub(p1, p2);
    let a = length2(d1);
    let e = length2(d2);
    let f = dot(d2, r);
    const TINY: f64 = 1e-30;
    let (s, t);
    if a <= TINY && e <= TINY {
        return (p1, p2);
    }
    if a <= TINY {
        s = 0.0;
        t = (f / e).clamp(0.0, 1.0);
    } else {
        let c = dot(d1, r);
        if e <= TINY {
            t = 0.0;
            s = (-c / a).clamp(0.0, 1.0);
        } else {
            let b = dot(d1, d2);
            let denominator = a * e - b * b;
            let mut s_value = if denominator > TINY * a * e.max(1.0) {
                ((b * f - c * e) / denominator).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let mut t_value = (b * s_value + f) / e;
            if t_value < 0.0 {
                t_value = 0.0;
                s_value = (-c / a).clamp(0.0, 1.0);
            } else if t_value > 1.0 {
                t_value = 1.0;
                s_value = ((b - c) / a).clamp(0.0, 1.0);
            }
            s = s_value;
            t = t_value;
        }
    }
    (add(p1, mul(d1, s)), add(p2, mul(d2, t)))
}

/// Where segment `pq` crosses triangle `abc`, if it does. Coplanar contact is
/// not reported here; the vertex/edge distances in [`triangle_distance`]
/// already reach zero for it.
fn segment_triangle_intersection(p: P, q: P, a: P, b: P, c: P) -> Option<P> {
    let direction = sub(q, p);
    let e1 = sub(b, a);
    let e2 = sub(c, a);
    let h = cross(direction, e2);
    let determinant = dot(e1, h);
    let scale = length2(direction).sqrt() * length2(e1).sqrt() * length2(e2).sqrt();
    if determinant.abs() <= 1e-14 * scale.max(1e-300) {
        return None;
    }
    let inverse = 1.0 / determinant;
    let s = sub(p, a);
    let u = inverse * dot(s, h);
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let qv = cross(s, e1);
    let v = inverse * dot(direction, qv);
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = inverse * dot(e2, qv);
    if !(0.0..=1.0).contains(&t) {
        return None;
    }
    Some(add(p, mul(direction, t)))
}

/// Exact distance between two triangles, with the closest points.
///
/// Two disjoint triangles realise their distance at a vertex-to-face or an
/// edge-to-edge pair, so the minimum over those fifteen candidates is exact.
/// Intersecting triangles are caught first: then one triangle has an edge
/// piercing the other.
pub fn triangle_distance(t1: &[P; 3], t2: &[P; 3]) -> (f64, P, P) {
    for index in 0..3 {
        let (p, q) = (t1[index], t1[(index + 1) % 3]);
        if let Some(hit) = segment_triangle_intersection(p, q, t2[0], t2[1], t2[2]) {
            return (0.0, hit, hit);
        }
        let (p, q) = (t2[index], t2[(index + 1) % 3]);
        if let Some(hit) = segment_triangle_intersection(p, q, t1[0], t1[1], t1[2]) {
            return (0.0, hit, hit);
        }
    }
    let mut best = (f64::INFINITY, t1[0], t2[0]);
    let mut consider = |pa: P, pb: P| {
        let d = length2(sub(pa, pb));
        if d < best.0 {
            best = (d, pa, pb);
        }
    };
    for vertex in t1 {
        consider(
            *vertex,
            closest_point_on_triangle(*vertex, t2[0], t2[1], t2[2]),
        );
    }
    for vertex in t2 {
        consider(
            closest_point_on_triangle(*vertex, t1[0], t1[1], t1[2]),
            *vertex,
        );
    }
    for i in 0..3 {
        for j in 0..3 {
            let (pa, pb) = closest_points_segments(t1[i], t1[(i + 1) % 3], t2[j], t2[(j + 1) % 3]);
            consider(pa, pb);
        }
    }
    (best.0.sqrt(), best.1, best.2)
}

// ---------------------------------------------------------------------------
// Bounding volume hierarchy.
// ---------------------------------------------------------------------------

const LEAF_SIZE: usize = 4;

#[derive(Clone, Debug)]
struct Node {
    bounds: Aabb,
    /// Leaf: `count > 0`, triangles `start..start + count` of `order`.
    /// Inner: `count == 0`, children at `start` and `start + 1`.
    start: usize,
    count: usize,
}

/// A triangle soup plus a BVH over it.
pub struct MeshIndex {
    pub triangles: Vec<[P; 3]>,
    order: Vec<usize>,
    nodes: Vec<Node>,
    /// One vertex of every connected shell, so containment is tested per
    /// shell rather than for the first vertex only: an object of several
    /// pieces can have one piece buried in its neighbour while another piece
    /// is outside. All shells are kept; [`containment`] bounds the work.
    representatives: Vec<P>,
    /// Per triangle: unit normal and plane offset (`[nx, ny, nz, d]`), for
    /// the plane-distance lower bound that prunes nearest-point queries.
    planes: Vec<[f64; 4]>,
}

/// Point-in-solid probes one containment check may spend (both directions
/// together) before it gives up and reports the containment as unknown.
pub const MAX_CONTAINMENT_PROBES: usize = 20_000;

/// One vertex per vertex-connected shell of a triangle soup.
fn shell_representatives(triangles: &[[P; 3]]) -> Vec<P> {
    let mut ids: std::collections::HashMap<[u64; 3], usize> = std::collections::HashMap::new();
    let mut points = Vec::new();
    let mut parent: Vec<usize> = Vec::new();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    for triangle in triangles {
        let corners = triangle.map(|corner| {
            *ids.entry(corner.map(f64::to_bits)).or_insert_with(|| {
                points.push(corner);
                parent.push(parent.len());
                parent.len() - 1
            })
        });
        for corner in &corners[1..] {
            let (ra, rb) = (find(&mut parent, corners[0]), find(&mut parent, *corner));
            if ra != rb {
                parent[ra] = rb;
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut representatives = Vec::new();
    for index in 0..points.len() {
        if seen.insert(find(&mut parent, index)) {
            representatives.push(points[index]);
        }
    }
    representatives
}

impl MeshIndex {
    pub fn new(triangles: Vec<[P; 3]>) -> Self {
        let mut order: Vec<usize> = (0..triangles.len()).collect();
        let centroids: Vec<P> = triangles
            .iter()
            .map(|t| mul(add(add(t[0], t[1]), t[2]), 1.0 / 3.0))
            .collect();
        let mut nodes = Vec::with_capacity(2 * triangles.len() / LEAF_SIZE + 1);
        nodes.push(Node {
            bounds: triangles_bounds(&triangles),
            start: 0,
            count: triangles.len(),
        });
        let mut stack = vec![0usize];
        while let Some(node_index) = stack.pop() {
            let (start, count) = (nodes[node_index].start, nodes[node_index].count);
            if count <= LEAF_SIZE {
                continue;
            }
            let mut centroid_bounds = Aabb::empty();
            for &triangle in &order[start..start + count] {
                centroid_bounds.grow(centroids[triangle]);
            }
            let axis = centroid_bounds.longest_axis();
            let middle = count / 2;
            order[start..start + count].select_nth_unstable_by(middle, |x, y| {
                centroids[*x][axis].total_cmp(&centroids[*y][axis])
            });
            let left = nodes.len();
            for (child_start, child_count) in [(start, middle), (start + middle, count - middle)] {
                let mut bounds = Aabb::empty();
                for &triangle in &order[child_start..child_start + child_count] {
                    for corner in &triangles[triangle] {
                        bounds.grow(*corner);
                    }
                }
                nodes.push(Node {
                    bounds,
                    start: child_start,
                    count: child_count,
                });
            }
            nodes[node_index].start = left;
            nodes[node_index].count = 0;
            stack.push(left);
            stack.push(left + 1);
        }
        let representatives = shell_representatives(&triangles);
        // Unit normal and offset per triangle; a degenerate triangle gets a
        // zero normal, so its plane distance (0) never prunes it.
        let planes = triangles
            .iter()
            .map(|t| {
                let n = cross(sub(t[1], t[0]), sub(t[2], t[0]));
                let length = length2(n).sqrt();
                if length > 1e-300 && length.is_finite() {
                    let n = mul(n, 1.0 / length);
                    [n[0], n[1], n[2], dot(n, t[0])]
                } else {
                    [0.0; 4]
                }
            })
            .collect();
        Self {
            triangles,
            order,
            nodes,
            representatives,
            planes,
        }
    }

    pub fn from_mesh(mesh: &Mesh) -> Self {
        Self::new(mesh_triangles(mesh))
    }

    pub fn is_empty(&self) -> bool {
        self.triangles.is_empty()
    }

    /// One vertex of every connected shell.
    pub fn shell_points(&self) -> &[P] {
        &self.representatives
    }

    pub fn bounds(&self) -> Aabb {
        self.nodes
            .first()
            .map_or_else(Aabb::empty, |node| node.bounds)
    }

    fn leaf_triangles(&self, node: &Node) -> impl Iterator<Item = &[P; 3]> {
        self.order[node.start..node.start + node.count]
            .iter()
            .map(|index| &self.triangles[*index])
    }

    /// Nearest surface point to `point`, and its distance.
    pub fn nearest(&self, point: P) -> Option<(f64, P)> {
        self.nearest_triangle(point).map(|(distance, closest, _)| (distance, closest))
    }

    /// [`Self::nearest`], plus the index (into `triangles`) of the triangle
    /// the nearest point lies on.
    pub fn nearest_triangle(&self, point: P) -> Option<(f64, P, usize)> {
        self.nearest_triangle_within(point, f64::INFINITY)
    }

    /// The nearest surface point strictly closer than `bound`, or `None` when
    /// there is none. A finite bound prunes every BVH node and triangle at or
    /// beyond it, which is what makes the inscribed-ball loops of the
    /// thickness check cheap: they only ask whether a ball is still empty.
    pub fn nearest_within(&self, point: P, bound: f64) -> Option<(f64, P)> {
        self.nearest_triangle_within(point, bound)
            .map(|(distance, closest, _)| (distance, closest))
    }

    /// Whether any surface point lies strictly closer than `radius` to
    /// `point`; stops at the first one found.
    pub fn any_within(&self, point: P, radius: f64) -> bool {
        if self.is_empty() || !(radius > 0.0) {
            return false;
        }
        let bound = radius * radius;
        let mut stack = vec![0usize];
        while let Some(index) = stack.pop() {
            let node = &self.nodes[index];
            if node.bounds.point_distance2(point) >= bound {
                continue;
            }
            if node.count > 0 {
                for &triangle in &self.order[node.start..node.start + node.count] {
                    let t = &self.triangles[triangle];
                    if self.plane_distance2(triangle, point) >= bound {
                        continue;
                    }
                    if length2(sub(point, closest_point_on_triangle(point, t[0], t[1], t[2]))) < bound {
                        return true;
                    }
                }
            } else {
                stack.push(node.start);
                stack.push(node.start + 1);
            }
        }
        false
    }

    /// Squared distance from `point` to the plane of triangle `triangle`: a
    /// lower bound on its distance to the triangle, three multiply-adds.
    #[inline]
    fn plane_distance2(&self, triangle: usize, point: P) -> f64 {
        let [nx, ny, nz, offset] = self.planes[triangle];
        let distance = nx * point[0] + ny * point[1] + nz * point[2] - offset;
        distance * distance
    }

    fn nearest_triangle_within(&self, point: P, bound: f64) -> Option<(f64, P, usize)> {
        // Nothing is strictly closer than a bound of zero or less (or NaN).
        if self.is_empty() || !(bound > 0.0) {
            return None;
        }
        let mut best = (if bound.is_finite() { bound * bound } else { f64::INFINITY }, point, usize::MAX);
        let mut stack = vec![0usize];
        while let Some(index) = stack.pop() {
            let node = &self.nodes[index];
            if node.bounds.point_distance2(point) >= best.0 {
                continue;
            }
            if node.count > 0 {
                for &triangle in &self.order[node.start..node.start + node.count] {
                    if self.plane_distance2(triangle, point) >= best.0 {
                        continue;
                    }
                    let t = &self.triangles[triangle];
                    let closest = closest_point_on_triangle(point, t[0], t[1], t[2]);
                    let d = length2(sub(point, closest));
                    if d < best.0 {
                        best = (d, closest, triangle);
                    }
                }
            } else {
                let (left, right) = (node.start, node.start + 1);
                let dl = self.nodes[left].bounds.point_distance2(point);
                let dr = self.nodes[right].bounds.point_distance2(point);
                // Visit the nearer child first so the bound tightens early.
                if dl < dr {
                    stack.push(right);
                    stack.push(left);
                } else {
                    stack.push(left);
                    stack.push(right);
                }
            }
        }
        (best.2 != usize::MAX).then(|| (best.0.sqrt(), best.1, best.2))
    }

    /// Number of surface crossings of the ray `origin + t * direction`, t > 0.
    fn ray_crossings(&self, origin: P, direction: P) -> usize {
        let inverse = direction.map(|d| 1.0 / d);
        let mut crossings = 0;
        let mut stack = vec![0usize];
        while let Some(index) = stack.pop() {
            let node = &self.nodes[index];
            if !ray_hits_box(origin, inverse, &node.bounds) {
                continue;
            }
            if node.count > 0 {
                for t in self.leaf_triangles(node) {
                    if ray_triangle(origin, direction, t) {
                        crossings += 1;
                    }
                }
            } else {
                stack.push(node.start);
                stack.push(node.start + 1);
            }
        }
        crossings
    }

    /// Whether `point` is inside the closed surface, by majority vote of
    /// three ray-parity tests along unrelated irrational directions (one ray
    /// can graze an edge; three rarely all do).
    pub fn contains(&self, point: P) -> bool {
        if self.is_empty() || self.bounds().point_distance2(point) > 0.0 {
            return false;
        }
        const DIRECTIONS: [P; 3] = [
            [0.577_215_664_9, 0.312_734_528_1, 0.754_877_666_2],
            [-0.414_213_562_3, 0.832_050_294_3, -0.369_822_485_9],
            [0.267_949_192_4, -0.706_820_401_7, -0.654_534_243_1],
        ];
        let inside = DIRECTIONS
            .iter()
            .filter(|direction| self.ray_crossings(point, **direction) % 2 == 1)
            .count();
        inside >= 2
    }
}

impl MeshIndex {
    /// Nearest surface hit of the ray `origin + t * direction` with
    /// `t_min < t < t_max`, skipping triangle `skip`: `(t, triangle index)`.
    pub fn ray_first_hit(
        &self,
        origin: P,
        direction: P,
        t_min: f64,
        t_max: f64,
        skip: Option<usize>,
    ) -> Option<(f64, usize)> {
        if self.is_empty() {
            return None;
        }
        let inverse = direction.map(|d| 1.0 / d);
        let mut best: Option<(f64, usize)> = None;
        let mut stack = vec![0usize];
        while let Some(index) = stack.pop() {
            let node = &self.nodes[index];
            let limit = best.map_or(t_max, |(t, _)| t);
            match ray_box_entry(origin, inverse, &node.bounds) {
                Some(entry) if entry < limit => {}
                _ => continue,
            }
            if node.count > 0 {
                for &triangle in &self.order[node.start..node.start + node.count] {
                    if Some(triangle) == skip {
                        continue;
                    }
                    if let Some(t) = ray_triangle_t(origin, direction, &self.triangles[triangle]) {
                        if t > t_min && t < best.map_or(t_max, |(bt, _)| bt) {
                            best = Some((t, triangle));
                        }
                    }
                }
            } else {
                stack.push(node.start);
                stack.push(node.start + 1);
            }
        }
        best
    }
}

/// Parameter at which the ray enters the box (0 when it starts inside).
fn ray_box_entry(origin: P, inverse: P, bounds: &Aabb) -> Option<f64> {
    let mut near = 0.0f64;
    let mut far = f64::INFINITY;
    for axis in 0..3 {
        let t1 = (bounds.min[axis] - origin[axis]) * inverse[axis];
        let t2 = (bounds.max[axis] - origin[axis]) * inverse[axis];
        let (low, high) = if t1 <= t2 { (t1, t2) } else { (t2, t1) };
        if low.is_nan() || high.is_nan() {
            if origin[axis] < bounds.min[axis] || origin[axis] > bounds.max[axis] {
                return None;
            }
            continue;
        }
        near = near.max(low);
        far = far.min(high);
        if near > far {
            return None;
        }
    }
    Some(near)
}

/// Moller-Trumbore with closed edges: the hit parameter, if any.
fn ray_triangle_t(origin: P, direction: P, t: &[P; 3]) -> Option<f64> {
    let e1 = sub(t[1], t[0]);
    let e2 = sub(t[2], t[0]);
    let h = cross(direction, e2);
    let determinant = dot(e1, h);
    if determinant.abs() < 1e-18 {
        return None;
    }
    let inverse = 1.0 / determinant;
    let s = sub(origin, t[0]);
    let u = inverse * dot(s, h);
    const SLACK: f64 = 1e-9;
    if !(-SLACK..=1.0 + SLACK).contains(&u) {
        return None;
    }
    let q = cross(s, e1);
    let v = inverse * dot(direction, q);
    if v < -SLACK || u + v > 1.0 + SLACK {
        return None;
    }
    Some(inverse * dot(e2, q))
}

fn ray_hits_box(origin: P, inverse: P, bounds: &Aabb) -> bool {
    let mut near = 0.0f64;
    let mut far = f64::INFINITY;
    for axis in 0..3 {
        let t1 = (bounds.min[axis] - origin[axis]) * inverse[axis];
        let t2 = (bounds.max[axis] - origin[axis]) * inverse[axis];
        let (low, high) = if t1 <= t2 { (t1, t2) } else { (t2, t1) };
        if low.is_nan() || high.is_nan() {
            // Origin on a slab boundary with a zero direction component.
            if origin[axis] < bounds.min[axis] || origin[axis] > bounds.max[axis] {
                return false;
            }
            continue;
        }
        near = near.max(low);
        far = far.min(high);
        if near > far {
            return false;
        }
    }
    true
}

fn ray_triangle(origin: P, direction: P, t: &[P; 3]) -> bool {
    let e1 = sub(t[1], t[0]);
    let e2 = sub(t[2], t[0]);
    let h = cross(direction, e2);
    let determinant = dot(e1, h);
    if determinant.abs() < 1e-18 {
        return false;
    }
    let inverse = 1.0 / determinant;
    let s = sub(origin, t[0]);
    let u = inverse * dot(s, h);
    if !(0.0..1.0).contains(&u) {
        return false;
    }
    let q = cross(s, e1);
    let v = inverse * dot(direction, q);
    if v < 0.0 || u + v >= 1.0 {
        return false;
    }
    inverse * dot(e2, q) > 0.0
}

/// Unit normal of a triangle by its winding (outward for the engine's
/// meshes); zero for a degenerate triangle.
pub fn triangle_normal(t: &[P; 3]) -> P {
    let normal = cross(sub(t[1], t[0]), sub(t[2], t[0]));
    let length = length2(normal).sqrt();
    if length > 0.0 {
        mul(normal, 1.0 / length)
    } else {
        [0.0; 3]
    }
}

// ---------------------------------------------------------------------------
// Clearance regions: where two surfaces come closer than a tolerance.
// ---------------------------------------------------------------------------

/// Default surface samples per side of a clearance-region search. Sampling
/// is nearest-point queries against a BVH, so unlike the exact booleans it
/// is not bounded by [`MAX_BOOLEAN_TRIANGLES`]: it runs on any mesh the
/// check accepts (a multi-million-triangle scan included) at a cost of
/// roughly `samples * log(triangles)`.
pub const CLEARANCE_SAMPLES: usize = 20_000;
/// Most regions a clearance-region search reports.
pub const MAX_CLEARANCE_REGIONS: usize = 8;

/// One connected patch where the clearance between two surfaces is below
/// the tolerance (negative inside the other solid).
#[derive(Clone, Debug, PartialEq)]
pub struct ClearanceRegion {
    /// Area-weighted centre of the patch's samples.
    pub center: P,
    /// Smallest signed clearance in the patch and where it occurs.
    pub min_clearance: f64,
    pub min_point: P,
    /// Approximate patch area (mean of both surfaces' sampled areas).
    pub area: f64,
    pub samples: usize,
    pub bounds: Aabb,
    /// Some sample lies inside the other solid.
    pub overlapping: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClearanceRegions {
    /// Largest regions by area first (ties by tighter clearance); at most
    /// [`MAX_CLEARANCE_REGIONS`].
    pub regions: Vec<ClearanceRegion>,
    /// Regions found before truncation.
    pub total_regions: usize,
    /// Sample spacing (mm) on the surfaces near the other part.
    pub spacing: f64,
    pub samples: usize,
    /// The deadline passed before every sample was taken.
    pub truncated: bool,
}

/// Sample points of the triangles of `index` lying within `reach` of
/// `other`'s bounds, each with the area it stands for, at most `budget` of
/// them: the centroids of a regular barycentric subdivision of each
/// triangle, so the spacing is roughly uniform across the surface.
fn surface_samples(index: &MeshIndex, other: &Aabb, reach: f64, budget: usize) -> (Vec<(P, f64)>, f64) {
    let near: Vec<&[P; 3]> = index
        .triangles
        .iter()
        .filter(|t| triangles_bounds(std::slice::from_ref(*t)).distance2(other) <= reach * reach)
        .collect();
    let area_of = |t: &[P; 3]| 0.5 * length2(cross(sub(t[1], t[0]), sub(t[2], t[0]))).sqrt();
    let total: f64 = near.iter().map(|t| area_of(t)).sum();
    if near.is_empty() || total <= 0.0 || budget == 0 {
        return (Vec::new(), 0.0);
    }
    if near.len() >= budget {
        // More triangles than samples: one centroid per triangle, strided.
        let stride = near.len().div_ceil(budget);
        let samples: Vec<(P, f64)> = near
            .iter()
            .step_by(stride)
            .map(|t| (mul(add(add(t[0], t[1]), t[2]), 1.0 / 3.0), area_of(t) * stride as f64))
            .collect();
        let spacing = (total / samples.len() as f64).sqrt();
        return (samples, spacing);
    }
    let mut spacing = (total / (budget - near.len()) as f64).sqrt();
    let levels = |spacing: f64| -> Vec<usize> {
        near.iter()
            .map(|t| ((area_of(t).sqrt() / spacing).ceil() as usize).clamp(1, 256))
            .collect()
    };
    let mut k = levels(spacing);
    while k.iter().map(|k| k * k).sum::<usize>() > budget {
        spacing *= 1.25;
        k = levels(spacing);
    }
    let mut samples = Vec::new();
    for (t, &k) in near.iter().zip(&k) {
        let weight = area_of(t) / (k * k) as f64;
        let at = |u: f64, v: f64| add(t[0], add(mul(sub(t[1], t[0]), u), mul(sub(t[2], t[0]), v)));
        let kf = k as f64;
        for i in 0..k {
            for j in 0..k - i {
                samples.push((at((i as f64 + 1.0 / 3.0) / kf, (j as f64 + 1.0 / 3.0) / kf), weight));
                if i + j + 1 < k {
                    samples.push((at((i as f64 + 2.0 / 3.0) / kf, (j as f64 + 2.0 / 3.0) / kf), weight));
                }
            }
        }
    }
    (samples, spacing)
}

/// Finds where the surfaces of `a` and `b` come closer than `tolerance`
/// (or interpenetrate): samples both surfaces near the other part (up to
/// `samples_per_side` points each), measures each sample's signed clearance
/// to the other solid (nearest-point distance, negative inside it), and
/// clusters the samples below the tolerance into connected regions.
/// Sampling stops at `deadline` (reported as `truncated`).
pub fn clearance_regions(
    a: &MeshIndex,
    b: &MeshIndex,
    tolerance: f64,
    samples_per_side: usize,
    deadline: Option<std::time::Instant>,
) -> ClearanceRegions {
    let reach = tolerance.max(0.0) + 1e-9;
    let mut hits: Vec<(P, f64, f64)> = Vec::new();
    let mut spacing: f64 = 0.0;
    let mut taken = 0usize;
    let mut truncated = false;
    'sides: for (from, to) in [(a, b), (b, a)] {
        let (samples, side_spacing) = surface_samples(from, &to.bounds(), reach, samples_per_side);
        spacing = spacing.max(side_spacing);
        for (index, (point, weight)) in samples.into_iter().enumerate() {
            // Often: a client gone should stop this within milliseconds, and
            // a nearest-point query costs far more than the poll.
            if index % 64 == 0 && (csg::bsp::deadline_passed() || deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline)) {
                truncated = true;
                break 'sides;
            }
            taken += 1;
            let Some((distance, _)) = to.nearest(point) else {
                continue;
            };
            if distance >= reach && to.bounds().point_distance2(point) > 0.0 {
                continue;
            }
            if distance >= reach {
                // Far from the surface: only an interior sample matters,
                // and an interior sample is deep, not "near".
                if to.contains(point) {
                    hits.push((point, -distance, weight));
                }
                continue;
            }
            let signed = if to.contains(point) { -distance } else { distance };
            if signed < tolerance {
                hits.push((point, signed, weight));
            }
        }
    }
    // Single-linkage clustering on a voxel grid: samples closer than a few
    // spacings (and the tolerance) belong to the same patch.
    //
    // Cells are `link / sqrt(3)` wide, so two samples sharing a cell are
    // always within `link`: each cell joins as a whole for free. Cells up
    // to two apart can still hold linked samples; such a pair is tested
    // exactly when small, and otherwise only between the few samples of
    // each cell facing the other, so a large tolerance (which piles tens
    // of thousands of samples into a handful of cells) costs linear time
    // instead of hundreds of millions of comparisons.
    let link = (spacing * 2.5).max(tolerance).max(1e-6);
    let cell_size = link / 3f64.sqrt();
    let cell = |p: P| p.map(|c| (c / cell_size).floor() as i64);
    let mut grid: std::collections::HashMap<[i64; 3], Vec<usize>> = std::collections::HashMap::new();
    for (index, hit) in hits.iter().enumerate() {
        grid.entry(cell(hit.0)).or_default().push(index);
    }
    let mut parent: Vec<usize> = (0..hits.len()).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    fn union(parent: &mut [usize], a: usize, b: usize) {
        let (ra, rb) = (find(parent, a), find(parent, b));
        if ra != rb {
            parent[ra] = rb;
        }
    }
    const EXACT_PAIRS: usize = 64;
    const FACING: usize = 8;
    let mut keys: Vec<[i64; 3]> = grid.keys().copied().collect();
    keys.sort_unstable();
    for members in grid.values() {
        for &member in &members[1..] {
            union(&mut parent, members[0], member);
        }
    }
    let (mut near, mut far): (Vec<(f64, usize)>, Vec<(f64, usize)>) = (Vec::new(), Vec::new());
    'cells: for (position, key) in keys.iter().enumerate() {
        if position % 64 == 0 && (csg::bsp::deadline_passed() || deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline)) {
            truncated = true;
            break 'cells;
        }
        let members = &grid[key];
        for dx in -2i64..=2 {
            for dy in -2i64..=2 {
                for dz in -2i64..=2 {
                    let offset = [dx, dy, dz];
                    let neighbour = [key[0] + dx, key[1] + dy, key[2] + dz];
                    // Each unordered pair of cells once.
                    if neighbour <= *key {
                        continue;
                    }
                    // Cells whose closest faces are already farther apart
                    // than `link` cannot hold a linked pair.
                    let gap2: f64 = offset
                        .iter()
                        .map(|&d| ((d.abs() - 1).max(0) as f64 * cell_size).powi(2))
                        .sum();
                    if gap2 > link * link {
                        continue;
                    }
                    let Some(others) = grid.get(&neighbour) else {
                        continue;
                    };
                    if find(&mut parent, members[0]) == find(&mut parent, others[0]) {
                        continue;
                    }
                    let within = |i: usize, j: usize| length2(sub(hits[i].0, hits[j].0)) <= link * link;
                    let linked = if members.len() * others.len() <= EXACT_PAIRS {
                        members.iter().any(|&i| others.iter().any(|&j| within(i, j)))
                    } else {
                        // The closest pair lies among the samples of each
                        // cell nearest the other along the cell offset.
                        let direction = offset.map(|d| d as f64);
                        let facing = |list: &mut Vec<(f64, usize)>, from: &[usize], sign: f64| {
                            list.clear();
                            list.extend(from.iter().map(|&i| (-sign * dot(hits[i].0, direction), i)));
                            if list.len() > FACING {
                                list.select_nth_unstable_by(FACING - 1, |a, b| a.0.total_cmp(&b.0));
                                list.truncate(FACING);
                            }
                        };
                        facing(&mut near, members, 1.0);
                        facing(&mut far, others, -1.0);
                        near.iter().any(|&(_, i)| far.iter().any(|&(_, j)| within(i, j)))
                    };
                    if linked {
                        union(&mut parent, members[0], others[0]);
                    }
                }
            }
        }
    }
    let mut clusters: std::collections::HashMap<usize, Vec<usize>> = std::collections::HashMap::new();
    for index in 0..hits.len() {
        let root = find(&mut parent, index);
        clusters.entry(root).or_default().push(index);
    }
    let mut regions: Vec<ClearanceRegion> = clusters
        .into_values()
        .map(|members| {
            let mut bounds = Aabb::empty();
            let (mut weighted, mut weight) = ([0.0; 3], 0.0);
            let mut min = (f64::INFINITY, [0.0; 3]);
            for &member in &members {
                let (point, clearance, w) = hits[member];
                bounds.grow(point);
                weighted = add(weighted, mul(point, w));
                weight += w;
                if clearance < min.0 {
                    min = (clearance, point);
                }
            }
            ClearanceRegion {
                center: if weight > 0.0 { mul(weighted, 1.0 / weight) } else { min.1 },
                min_clearance: min.0,
                min_point: min.1,
                // Both surfaces were sampled; a patch is seen from each.
                area: weight * 0.5,
                samples: members.len(),
                bounds,
                overlapping: min.0 < 0.0,
            }
        })
        .collect();
    regions.sort_by(|x, y| {
        y.area
            .total_cmp(&x.area)
            .then(x.min_clearance.total_cmp(&y.min_clearance))
            .then(x.center.map(f64::to_bits).cmp(&y.center.map(f64::to_bits)))
    });
    let total_regions = regions.len();
    regions.truncate(MAX_CLEARANCE_REGIONS);
    ClearanceRegions {
        regions,
        total_regions,
        spacing,
        samples: taken,
        truncated,
    }
}

/// Result of [`mesh_distance`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeshDistance {
    /// Smallest distance between the two surfaces; 0 when they touch or cross.
    pub distance: f64,
    /// Realising point on the first surface.
    pub point_a: P,
    /// Realising point on the second surface.
    pub point_b: P,
    /// False when the work budget ran out before the search converged; the
    /// distance is then an upper bound (a real pair at that distance exists).
    pub exact: bool,
    /// Proven lower bound on the true distance: equal to `distance` when
    /// `exact`, otherwise the smallest box distance among the node pairs the
    /// search had not opened yet (possibly 0). Only this, never `distance`,
    /// may be used to claim clearance from an incomplete search.
    pub lower_bound: f64,
}

/// Triangle-pair tests one distance query may spend before it stops refining.
pub const MAX_PAIR_TESTS: usize = 40_000_000;

/// Exact minimum distance between two triangle surfaces.
///
/// Branch-and-bound over both BVHs: a node pair is opened only while its box
/// distance is below the best triangle-triangle distance found so far, so the
/// answer is the true surface-to-surface minimum, not a sampled one. This is
/// what replaced the engine's sampled check, which evaluated one part's
/// surface samples against the *other part's signed-distance function*. Those
/// SDFs are exact only for primitives; through `difference`, `hull`,
/// non-uniform `scale`, extrusions and the like they are lower bounds, so the
/// reported clearance could be a fraction of the real gap (0.146 mm reported
/// for meshes 0.35 mm apart), and the samples themselves could miss an
/// edge-to-edge approach entirely.
#[allow(dead_code)]
pub fn mesh_distance(a: &MeshIndex, b: &MeshIndex) -> Option<MeshDistance> {
    mesh_distance_budgeted(a, b, MAX_PAIR_TESTS)
}

fn mesh_distance_budgeted(a: &MeshIndex, b: &MeshIndex, max_tests: usize) -> Option<MeshDistance> {
    mesh_distance_shifted(a, b, [0.0; 3], max_tests)
}

/// [`mesh_distance`] with `a` translated by `shift` (no index rebuild), and
/// a caller-chosen budget of triangle-pair tests. Points are reported in the
/// shifted frame (`point_a` already includes `shift`).
pub fn mesh_distance_shifted(a: &MeshIndex, b: &MeshIndex, shift: P, max_tests: usize) -> Option<MeshDistance> {
    mesh_distance_swept(a, b, shift, [0.0; 3], max_tests).map(|swept| swept.distance)
}

/// Result of [`mesh_distance_swept`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SweptDistance {
    pub distance: MeshDistance,
    /// Fraction of the sweep (0..1) at which the closest pair occurs.
    pub at: f64,
}

/// Exact distance between the surface of `a` swept by translation from
/// `shift` to `shift + delta` (every triangle's Minkowski sum with the
/// segment) and the surface of `b`: the minimum over the whole motion of the
/// surface distance, in one branch-and-bound. `point_a` is on `a` at the pose
/// `shift + at * delta`.
pub fn mesh_distance_swept(a: &MeshIndex, b: &MeshIndex, shift: P, delta: P, max_tests: usize) -> Option<SweptDistance> {
    let moved = |bounds: &Aabb| {
        let mut out = Aabb {
            min: add(bounds.min, shift),
            max: add(bounds.max, shift),
        };
        out.grow(add(bounds.min, add(shift, delta)));
        out.grow(add(bounds.max, add(shift, delta)));
        out
    };
    let place = |t: &[P; 3]| [add(t[0], shift), add(t[1], shift), add(t[2], shift)];
    if a.is_empty() || b.is_empty() {
        return None;
    }
    let mut tests = 0usize;
    let mut leaves = 0usize;
    let mut exact = true;
    let mut lower2 = f64::INFINITY;
    // Seed with any real pair so the bound is finite from the start.
    let mut best = {
        let (d, pa, pb, at) = swept_triangle_distance(&place(&a.triangles[0]), delta, &b.triangles[0]);
        (d * d, pa, pb, at)
    };
    let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
    while let Some((ia, ib)) = stack.pop() {
        if best.0 == 0.0 {
            break;
        }
        let (na, nb) = (&a.nodes[ia], &b.nodes[ib]);
        if moved(&na.bounds).distance2(&nb.bounds) >= best.0 {
            continue;
        }
        match (na.count > 0, nb.count > 0) {
            (true, true) => {
                if tests >= max_tests
                    || ({
                        leaves += 1;
                        leaves % 256 == 0
                    } && csg::bsp::deadline_passed())
                {
                    // Out of budget: the best pair so far is a real pair, so
                    // the answer is an upper bound. The true minimum is no
                    // smaller than the nearest unopened node pair, which
                    // gives the proven lower bound. Stop expanding entirely.
                    exact = false;
                    lower2 = moved(&na.bounds).distance2(&nb.bounds);
                    for &(ra, rb) in &stack {
                        lower2 = lower2.min(moved(&a.nodes[ra].bounds).distance2(&b.nodes[rb].bounds));
                    }
                    break;
                }
                for ta in a.leaf_triangles(na) {
                    let ta = place(ta);
                    for tb in b.leaf_triangles(nb) {
                        tests += 1;
                        let (d, pa, pb, at) = swept_triangle_distance(&ta, delta, tb);
                        if d * d < best.0 {
                            best = (d * d, pa, pb, at);
                        }
                    }
                }
            }
            (leaf_a, leaf_b) => {
                // Split the larger (non-leaf) node; push the farther pair first
                // so the nearer one is explored next.
                let split_a = !leaf_a && (leaf_b || volume(&na.bounds) >= volume(&nb.bounds));
                let children: [(usize, usize); 2] = if split_a {
                    [(na.start, ib), (na.start + 1, ib)]
                } else {
                    [(ia, nb.start), (ia, nb.start + 1)]
                };
                let d0 = moved(&a.nodes[children[0].0].bounds).distance2(&b.nodes[children[0].1].bounds);
                let d1 = moved(&a.nodes[children[1].0].bounds).distance2(&b.nodes[children[1].1].bounds);
                if d0 <= d1 {
                    stack.push(children[1]);
                    stack.push(children[0]);
                } else {
                    stack.push(children[0]);
                    stack.push(children[1]);
                }
            }
        }
    }
    let distance = best.0.sqrt();
    Some(SweptDistance {
        distance: MeshDistance {
            distance,
            point_a: best.1,
            point_b: best.2,
            exact,
            lower_bound: if exact {
                distance
            } else {
                lower2.min(best.0).sqrt()
            },
        },
        at: best.3,
    })
}

/// Distance between triangle `ta` swept along `delta` (the convex solid
/// `ta (+) [0, delta]`) and triangle `tb`: `(distance, point on ta at the
/// realising pose (world, at `t`), point on tb, fraction `t` of delta)`.
///
/// The solid's boundary lies on `ta`, `ta + delta` and the three edge
/// parallelograms, so outside it the distance is the minimum over those
/// eight triangles; `tb` is wholly inside it exactly when a vertex `v` of
/// `tb` has `v - t * delta` on `ta` for some `t` in [0, 1].
fn swept_triangle_distance(ta: &[P; 3], delta: P, tb: &[P; 3]) -> (f64, P, P, f64) {
    let (d, pa, pb) = triangle_distance(ta, tb);
    let d2 = length2(delta);
    if d2 == 0.0 || d == 0.0 {
        return (d, pa, pb, 0.0);
    }
    let mut best = (d, pa, pb, 0.0);
    let end = [add(ta[0], delta), add(ta[1], delta), add(ta[2], delta)];
    let (d, pa, pb) = triangle_distance(&end, tb);
    if d < best.0 {
        best = (d, pa, pb, 1.0);
    }
    for index in 0..3 {
        let (e0, e1) = (ta[index], ta[(index + 1) % 3]);
        for face in [[e0, e1, add(e1, delta)], [e0, add(e1, delta), add(e0, delta)]] {
            let (d, pa, pb) = triangle_distance(&face, tb);
            if d < best.0 {
                // pa = e0 + alpha (e1 - e0) + t delta: solve for t.
                let (edge, r) = (sub(e1, e0), sub(pa, e0));
                let (ee, ed, rr_e, rr_d) = (dot(edge, edge), dot(edge, delta), dot(r, edge), dot(r, delta));
                let determinant = ee * d2 - ed * ed;
                let t = if determinant.abs() > 1e-18 * ee * d2 {
                    (ee * rr_d - ed * rr_e) / determinant
                } else {
                    rr_d / d2
                }
                .clamp(0.0, 1.0);
                best = (d, pa, pb, t);
            }
        }
    }
    if best.0 > 0.0 {
        for v in tb {
            if let Some(hit) = segment_triangle_intersection(*v, sub(*v, delta), ta[0], ta[1], ta[2]) {
                let t = (length2(sub(*v, hit)) / d2).sqrt().clamp(0.0, 1.0);
                return (0.0, *v, *v, t);
            }
        }
    }
    best
}

/// Where an edge of one triangle passes strictly through the interior of the
/// other: a transversal surface crossing, which for closed surfaces means the
/// solids overlap with positive volume. Touching (coplanar faces, a vertex or
/// edge resting on a face) is not a crossing.
fn transversal_crossing(t1: &[P; 3], t2: &[P; 3]) -> Option<P> {
    fn strict(p: P, q: P, t: &[P; 3]) -> Option<P> {
        let direction = sub(q, p);
        let e1 = sub(t[1], t[0]);
        let e2 = sub(t[2], t[0]);
        let h = cross(direction, e2);
        let determinant = dot(e1, h);
        let scale = length2(direction).sqrt() * length2(e1).sqrt() * length2(e2).sqrt();
        if determinant.abs() <= 1e-9 * scale.max(1e-300) {
            return None;
        }
        let inverse = 1.0 / determinant;
        let s = sub(p, t[0]);
        let u = inverse * dot(s, h);
        let qv = cross(s, e1);
        let v = inverse * dot(direction, qv);
        let along = inverse * dot(e2, qv);
        const MARGIN: f64 = 1e-9;
        (u > MARGIN && v > MARGIN && u + v < 1.0 - MARGIN && along > MARGIN && along < 1.0 - MARGIN)
            .then(|| add(p, mul(direction, along)))
    }
    for index in 0..3 {
        if let Some(hit) = strict(t1[index], t1[(index + 1) % 3], t2) {
            return Some(hit);
        }
        if let Some(hit) = strict(t2[index], t2[(index + 1) % 3], t1) {
            return Some(hit);
        }
    }
    None
}

/// Outcome of [`find_crossing`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Crossing {
    Found(P),
    None,
    /// The budget ran out before every candidate pair was tested.
    Unknown,
}

/// Searches for a transversal crossing between the two surfaces.
pub fn find_crossing(a: &MeshIndex, b: &MeshIndex) -> Crossing {
    if a.is_empty() || b.is_empty() {
        return Crossing::None;
    }
    let mut tests = 0usize;
    let mut leaves = 0usize;
    let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
    while let Some((ia, ib)) = stack.pop() {
        let (na, nb) = (&a.nodes[ia], &b.nodes[ib]);
        if na.bounds.distance2(&nb.bounds) > 0.0 {
            continue;
        }
        match (na.count > 0, nb.count > 0) {
            (true, true) => {
                if tests >= MAX_PAIR_TESTS
                    || ({
                        leaves += 1;
                        leaves % 256 == 0
                    } && csg::bsp::deadline_passed())
                {
                    return Crossing::Unknown;
                }
                for ta in a.leaf_triangles(na) {
                    for tb in b.leaf_triangles(nb) {
                        tests += 1;
                        if let Some(hit) = transversal_crossing(ta, tb) {
                            return Crossing::Found(hit);
                        }
                    }
                }
            }
            (leaf_a, leaf_b) => {
                let split_a = !leaf_a && (leaf_b || volume(&na.bounds) >= volume(&nb.bounds));
                if split_a {
                    stack.push((na.start, ib));
                    stack.push((na.start + 1, ib));
                } else {
                    stack.push((ia, nb.start));
                    stack.push((ia, nb.start + 1));
                }
            }
        }
    }
    Crossing::None
}

fn volume(bounds: &Aabb) -> f64 {
    let size = sub(bounds.max, bounds.min);
    size[0].max(0.0) * size[1].max(0.0) * size[2].max(0.0) + size[0] + size[1] + size[2]
}

// ---------------------------------------------------------------------------
// Whole-mesh measurements.
// ---------------------------------------------------------------------------

/// Measurements of one closed mesh.
#[derive(Clone, Debug, PartialEq)]
pub struct MeshMeasure {
    pub bounds: Aabb,
    pub volume: f64,
    pub surface_area: f64,
    pub triangles: usize,
    pub vertices: usize,
    /// Edge-connected shells.
    pub components: usize,
    pub boundary_edges: usize,
    pub non_manifold_edges: usize,
    /// Edges used twice in the same direction: neighbouring faces disagree
    /// about which side is outside.
    pub inconsistent_edges: usize,
    pub degenerate_triangles: usize,
    /// Where the boundary and non-manifold edges are: connected groups,
    /// largest first, at most [`MAX_DEFECT_CLUSTERS`]. Empty when the mesh
    /// has neither.
    pub defects: Vec<DefectCluster>,
    /// Every defect group found, including those beyond the cap.
    pub defect_clusters: usize,
    /// False when this thread's deadline or cancellation stopped the
    /// measurement part-way: bounds, volume and area are exact, but the
    /// topology counts and defects are partial and must not be trusted
    /// (`watertight` and friends answer false).
    pub complete: bool,
}

/// How many loop steps pass between deadline/cancellation polls.
const MEASURE_POLL_EVERY: usize = 4096;

/// Counts loop steps and polls this thread's deadline and cancellation
/// flags (`csg::bsp::interrupt_message`) every [`MEASURE_POLL_EVERY`] steps;
/// once interrupted it stays interrupted.
struct InterruptPoll {
    steps: usize,
    interrupted: bool,
}

impl InterruptPoll {
    fn new() -> Self {
        Self { steps: 0, interrupted: csg::bsp::interrupt_message().is_some() }
    }

    /// True once the work should stop.
    fn stop(&mut self) -> bool {
        if !self.interrupted {
            self.steps += 1;
            if self.steps % MEASURE_POLL_EVERY == 0 {
                self.interrupted = csg::bsp::interrupt_message().is_some();
            }
        }
        self.interrupted
    }
}

/// Most defect groups a measurement lists.
pub const MAX_DEFECT_CLUSTERS: usize = 10;
/// Sample edges kept per defect group.
pub const DEFECT_SAMPLE_EDGES: usize = 3;

/// One connected group of open (boundary) or non-manifold edges: edges of
/// the same kind that share a vertex belong to the same group, so a hole in
/// the surface is one group however many edges ring it.
#[derive(Clone, Debug, PartialEq)]
pub struct DefectCluster {
    /// `"boundary"` or `"nonManifold"`.
    pub kind: &'static str,
    pub edges: usize,
    /// Mean of the edge midpoints.
    pub centroid: P,
    pub bounds: Aabb,
    /// A few of the edges, as endpoint pairs.
    pub samples: Vec<[P; 2]>,
}

/// Groups the defect edges (welded vertex pairs) of one kind by shared
/// vertices.
fn defect_clusters(
    kind: &'static str,
    edges: &[(u32, u32)],
    vertices: &[csg::Vec3],
    poll: &mut InterruptPoll,
) -> Vec<DefectCluster> {
    let mut parent: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    fn find(parent: &mut std::collections::HashMap<u32, u32>, x: u32) -> u32 {
        let mut root = x;
        while let Some(&next) = parent.get(&root) {
            if next == root {
                break;
            }
            root = next;
        }
        let mut current = x;
        while current != root {
            let next = parent[&current];
            parent.insert(current, root);
            current = next;
        }
        root
    }
    for &(a, b) in edges {
        if poll.stop() {
            return Vec::new();
        }
        parent.entry(a).or_insert(a);
        parent.entry(b).or_insert(b);
        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
        if ra != rb {
            parent.insert(ra, rb);
        }
    }
    let mut groups: std::collections::HashMap<u32, Vec<(u32, u32)>> =
        std::collections::HashMap::new();
    for &(a, b) in edges {
        if poll.stop() {
            return Vec::new();
        }
        let root = find(&mut parent, a);
        groups.entry(root).or_default().push((a, b));
    }
    let point = |index: u32| -> P {
        let v = vertices[index as usize];
        [v.x, v.y, v.z]
    };
    groups
        .into_values()
        .map(|mut group| {
            group.sort_unstable();
            let mut bounds = Aabb::empty();
            let mut sum = [0.0; 3];
            for &(a, b) in &group {
                let (pa, pb) = (point(a), point(b));
                bounds.grow(pa);
                bounds.grow(pb);
                for axis in 0..3 {
                    sum[axis] += (pa[axis] + pb[axis]) * 0.5;
                }
            }
            let count = group.len() as f64;
            DefectCluster {
                kind,
                edges: group.len(),
                centroid: sum.map(|value| value / count),
                bounds,
                samples: group
                    .iter()
                    .take(DEFECT_SAMPLE_EDGES)
                    .map(|&(a, b)| [point(a), point(b)])
                    .collect(),
            }
        })
        .collect()
}

impl MeshMeasure {
    pub fn watertight(&self) -> bool {
        self.complete && self.boundary_edges == 0 && self.non_manifold_edges == 0
    }
    pub fn manifold(&self) -> bool {
        self.complete && self.non_manifold_edges == 0
    }
    pub fn winding_consistent(&self) -> bool {
        self.complete && self.inconsistent_edges == 0
    }
}

fn to_csg(point: P) -> csg::Vec3 {
    csg::Vec3::new(point[0], point[1], point[2])
}

pub fn measure_triangles(triangles: &[[P; 3]]) -> MeshMeasure {
    let mut poll = InterruptPoll::new();
    let bounds = triangles_bounds(triangles);
    let mut volume = 0.0;
    let mut area = 0.0;
    let mut degenerate = 0;
    for t in triangles {
        volume += dot(t[0], cross(t[1], t[2])) / 6.0;
        let doubled = length2(cross(sub(t[1], t[0]), sub(t[2], t[0]))).sqrt();
        area += doubled * 0.5;
    }
    let welded = csg::cleanup::weld_triangles(triangles.iter().map(|t| t.map(to_csg)));
    // Collapsed by welding, or kept but with (near) zero area — each counted once.
    degenerate += triangles.len() - welded.triangles.len();
    for index in 0..welded.triangles.len() {
        let [a, b, c] = welded.triangle_points(index);
        if b.sub(a).cross(c.sub(a)).length() <= 1e-12 {
            degenerate += 1;
        }
    }
    let statistics = welded.statistics();
    // Directed-edge census: an undirected edge shared by two faces should
    // appear once in each direction.
    let mut directed: std::collections::HashMap<(u32, u32), usize> =
        std::collections::HashMap::new();
    for triangle in &welded.triangles {
        if poll.stop() {
            break;
        }
        for index in 0..3 {
            *directed
                .entry((triangle[index], triangle[(index + 1) % 3]))
                .or_insert(0) += 1;
        }
    }
    let mut inconsistent = 0;
    for &count in directed.values() {
        if count > 1 {
            inconsistent += 1;
        }
    }
    // Components: union-find over edge-sharing faces.
    let mut parent: Vec<usize> = (0..welded.triangles.len()).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    let mut edge_owner: std::collections::HashMap<(u32, u32), usize> =
        std::collections::HashMap::new();
    for (face, triangle) in welded.triangles.iter().enumerate() {
        if poll.stop() {
            break;
        }
        for index in 0..3 {
            let (from, to) = (triangle[index], triangle[(index + 1) % 3]);
            let key = (from.min(to), from.max(to));
            if let Some(&other) = edge_owner.get(&key) {
                let (ra, rb) = (find(&mut parent, face), find(&mut parent, other));
                if ra != rb {
                    parent[ra] = rb;
                }
            } else {
                edge_owner.insert(key, face);
            }
        }
    }
    let mut roots = std::collections::HashSet::new();
    for face in 0..welded.triangles.len() {
        if poll.stop() {
            break;
        }
        roots.insert(find(&mut parent, face));
    }
    // Locate the defects (only when there are any: a healthy mesh pays
    // nothing more).
    let mut defects = Vec::new();
    if !poll.interrupted && statistics.boundary_edges + statistics.non_manifold_edges > 0 {
        let mut undirected: std::collections::HashMap<(u32, u32), usize> =
            std::collections::HashMap::new();
        for (&(from, to), &count) in &directed {
            if poll.stop() {
                break;
            }
            *undirected.entry((from.min(to), from.max(to))).or_insert(0) += count;
        }
        let (mut open, mut fins): (Vec<(u32, u32)>, Vec<(u32, u32)>) = (Vec::new(), Vec::new());
        for (&edge, &count) in &undirected {
            match count {
                1 => open.push(edge),
                2 => {}
                _ => fins.push(edge),
            }
        }
        defects = defect_clusters("boundary", &open, &welded.vertices, &mut poll);
        defects.extend(defect_clusters("nonManifold", &fins, &welded.vertices, &mut poll));
        // Largest first; ties in a stable spatial order.
        defects.sort_by(|a, b| {
            b.edges.cmp(&a.edges).then_with(|| {
                a.centroid
                    .partial_cmp(&b.centroid)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });
    }
    let defect_clusters = defects.len();
    defects.truncate(MAX_DEFECT_CLUSTERS);
    MeshMeasure {
        bounds,
        volume,
        surface_area: area,
        triangles: triangles.len(),
        vertices: statistics.vertices,
        components: roots.len(),
        boundary_edges: statistics.boundary_edges,
        non_manifold_edges: statistics.non_manifold_edges,
        inconsistent_edges: inconsistent,
        degenerate_triangles: degenerate,
        defects,
        defect_clusters,
        complete: !poll.interrupted,
    }
}

pub fn measure_mesh(mesh: &Mesh) -> MeshMeasure {
    measure_triangles(&mesh_triangles(mesh))
}

/// Splits a mesh into its separate solids: edge-connected shells after
/// welding, with every inward-facing (void) shell assigned to the smallest
/// outward shell that encloses it, so a hollow part stays one solid. Shells
/// are returned largest volume first; a single solid comes back as one entry.
/// Zero-area triangles are dropped (they carry no geometry).
pub fn split_solids(triangles: &[[P; 3]]) -> Vec<Vec<[P; 3]>> {
    let welded = csg::cleanup::weld_triangles(triangles.iter().map(|t| t.map(to_csg)));
    let count = welded.triangles.len();
    let mut groups = Groups::new(count);
    let mut edge_owner: std::collections::HashMap<(u32, u32), usize> =
        std::collections::HashMap::new();
    for (face, triangle) in welded.triangles.iter().enumerate() {
        for index in 0..3 {
            let (from, to) = (triangle[index], triangle[(index + 1) % 3]);
            let key = (from.min(to), from.max(to));
            match edge_owner.get(&key) {
                Some(&other) => groups.union(face, other),
                None => {
                    edge_owner.insert(key, face);
                }
            }
        }
    }
    let mut shells: std::collections::HashMap<usize, Vec<[P; 3]>> =
        std::collections::HashMap::new();
    for face in 0..count {
        let [a, b, c] = welded.triangle_points(face);
        let root = groups.find(face);
        shells
            .entry(root)
            .or_default()
            .push([[a.x, a.y, a.z], [b.x, b.y, b.z], [c.x, c.y, c.z]]);
    }
    let signed = |shell: &[[P; 3]]| -> f64 {
        shell
            .iter()
            .map(|t| dot(t[0], cross(t[1], t[2])) / 6.0)
            .sum()
    };
    let mut solids: Vec<(f64, Vec<[P; 3]>)> = Vec::new();
    let mut voids: Vec<Vec<[P; 3]>> = Vec::new();
    for (_, shell) in shells {
        let volume = signed(&shell);
        if volume < 0.0 {
            voids.push(shell);
        } else {
            solids.push((volume, shell));
        }
    }
    if solids.len() <= 1 {
        let mut all: Vec<[P; 3]> = solids.into_iter().flat_map(|(_, shell)| shell).collect();
        all.extend(voids.into_iter().flatten());
        return if all.is_empty() { Vec::new() } else { vec![all] };
    }
    solids.sort_by(|a, b| b.0.total_cmp(&a.0));
    let indexes: Vec<MeshIndex> = solids
        .iter()
        .map(|(_, shell)| MeshIndex::new(shell.clone()))
        .collect();
    for void in voids {
        let probe = void[0][0];
        // Smallest enclosing solid; the largest one when none is found
        // (an open or self-touching void), so no geometry is lost.
        let owner = (0..solids.len())
            .rev()
            .find(|&index| indexes[index].contains(probe))
            .unwrap_or(0);
        solids[owner].1.extend(void);
    }
    solids.into_iter().map(|(_, shell)| shell).collect()
}

// ---------------------------------------------------------------------------
// Printability checks: wall thickness, overhangs, sharp edges.
// ---------------------------------------------------------------------------

/// Unit normal and area of a triangle; `None` when it is degenerate.
fn unit_normal(t: &[P; 3]) -> Option<(P, f64)> {
    let n = cross(sub(t[1], t[0]), sub(t[2], t[0]));
    let doubled = length2(n).sqrt();
    (doubled > 1e-12 && doubled.is_finite()).then(|| (mul(n, 1.0 / doubled), doubled * 0.5))
}

fn centroid(t: &[P; 3]) -> P {
    mul(add(add(t[0], t[1]), t[2]), 1.0 / 3.0)
}

fn bounds_span(bounds: &Aabb) -> f64 {
    if bounds.is_empty() {
        return 0.0;
    }
    length2(sub(bounds.max, bounds.min)).sqrt()
}

/// Tiny union-find used by the region groupings below.
struct Groups(Vec<usize>);

impl Groups {
    fn new(count: usize) -> Self {
        Self((0..count).collect())
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.0[x] != x {
            self.0[x] = self.0[self.0[x]];
            x = self.0[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.0[ra] = rb;
        }
    }
}

/// Triangles one thickness scan samples, per object (beyond it, a stride).
pub const MAX_THICKNESS_SAMPLES: usize = 20_000;
/// Surface points one thickness scan (or probe) measures, per object.
pub const MAX_THICKNESS_POINTS: usize = 60_000;
/// Locations reported per check.
pub const MAX_CHECK_LOCATIONS: usize = 10;

/// An inscribed ball whose two contacts face each other this squarely
/// (outward normals 100 degrees or more apart) spans a wall outright: either
/// two walls facing each other, or a wedge of 80 degrees or sharper, whose
/// tip (or truncated lip) really is a thin feature. A blunter convex edge (a
/// box edge, bevelled or not) pinches the ball without anything being thin.
const OPPOSED_CONTACTS_COS: f64 = -0.173_648_177_666_930_3; // cos 100 degrees

/// One measured wall: from the surface point `from`, the wall's opposite
/// surface is at `to`, `thickness` away across the material.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThinSpot {
    pub thickness: f64,
    pub from: P,
    pub to: P,
    /// `"ball"` (diameter of the largest ball inside the solid touching the
    /// surface at `from`) or `"ray"` (straight across along the inward normal).
    pub method: &'static str,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ThicknessCheck {
    pub threshold: f64,
    /// Surface points whose wall was measured.
    pub samples: usize,
    pub triangles: usize,
    /// Triangles not sampled at all (sampling stride or the deadline); 0 when
    /// every triangle was.
    pub skipped: usize,
    /// Mean spacing of the sample points (mm), so a reader knows what size
    /// of feature could fall between them.
    pub spacing: f64,
    pub min: Option<ThinSpot>,
    /// Samples thinner than `threshold`, and the surface area they stand for.
    pub below_threshold: usize,
    pub below_threshold_area: f64,
    /// Samples on a sharp wedge (80 degrees or sharper, apex on the surface:
    /// thread crests, knife edges), left out of `min` and the counts.
    pub sharp_tips: usize,
    /// The thinnest spots, spread out (not ten samples at the same place).
    pub thinnest: Vec<ThinSpot>,
    /// The smallest thickness three samples agree on: a minimum that one
    /// stray sample (a bevelled corner, a tessellation sliver) cannot set.
    pub low_quantile: Option<ThinSpot>,
    /// Every measured sample, thinnest first (bounded by the point budget),
    /// for callers that filter them (e.g. ignore numerical seams).
    pub lowest: Vec<ThinSpot>,
    /// Connected thin patches (thinnest first, at most
    /// [`MAX_CHECK_LOCATIONS`]); `region_count` counts all of them.
    pub regions: Vec<ThinRegion>,
    pub region_count: usize,
    /// Extra points the dense pass around thin spots measured.
    pub refined_samples: usize,
    /// The deadline or a cancellation cut the scan short somewhere other
    /// than whole triangles (`skipped`): valley-edge or refinement samples,
    /// or the region grouping.
    pub interrupted: bool,
}

impl ThicknessCheck {
    /// Every planned sample was measured and grouped.
    pub fn complete(&self) -> bool {
        self.skipped == 0 && !self.interrupted
    }
}

impl ThicknessCheck {
    /// The smallest thickness at least three samples agree on (within 10%),
    /// ignoring samples thinner than `floor` (numerical seams).
    pub fn robust_min_above(&self, floor: f64) -> Option<ThinSpot> {
        let kept: Vec<&ThinSpot> = self.lowest.iter().filter(|spot| spot.thickness >= floor).collect();
        kept.windows(3)
            .find(|run| run[2].thickness <= run[0].thickness * 1.1 + 1e-6)
            .map(|run| *run[0])
            .or_else(|| kept.first().map(|spot| **spot))
    }
}

/// Wall thickness at surface point `p` (inward unit normal `inward`, on
/// triangle `triangle`): the diameter of the largest ball inside the solid
/// that touches the surface at `p` (the "shrinking ball" medial-axis
/// estimate), which sees oblique walls and short ligaments a straight ray
/// misses: between a chamfer and a cone, between two groove tips. The ball
/// starts at half the straight-through ray and shrinks onto the nearest
/// surface point until it is empty.
///
/// A ball near a convex edge is pinched by the face across the edge (on a
/// box, a point 0.1 mm from an edge gets a 0.2 mm ball) without any wall
/// being thin. Contacts facing each other at 100 degrees or more always
/// count (see [`OPPOSED_CONTACTS_COS`]); otherwise the ball counts only when
/// the two tangent planes (at `p` and at the opposite contact) meet well
/// inside the material, as between two groove roots, and not along a convex
/// edge on or outside the surface. A pinched ball is slid away from that
/// edge, growing, until it touches a third surface ([`grow_along_bisector`]),
/// and the straight ray caps the result.
fn wall_at(walls: &Walls, p: P, inward: P, triangle: usize) -> Option<ThinSpot> {
    let (index, t_min) = (walls.index, walls.t_min);
    let (ray, hit) = index.ray_first_hit(p, inward, t_min, f64::INFINITY, Some(triangle))?;
    // The ray must leave the solid there (an outward face seen from inside);
    // entering another face first means an inverted or self-overlapping
    // region, not a wall.
    if unit_normal(&index.triangles[hit]).is_none_or(|(n, _)| dot(n, inward) <= 0.0) {
        return None;
    }
    let ray_spot = ThinSpot {
        thickness: ray,
        from: p,
        to: add(p, mul(inward, ray)),
        method: "ray",
    };
    let floor = t_min * 10.0;
    let mut radius = ray * 0.5;
    let mut contact = ray_spot.to;
    for _ in 0..48 {
        let centre = add(p, mul(inward, radius));
        // Only a surface point inside the ball matters: the bounded query
        // prunes everything else (p itself sits exactly on the ball).
        let Some((_, nearest)) = index.nearest_within(centre, radius * (1.0 - 1e-9) - floor) else {
            break;
        };
        // The ball through p (tangent there) and through `nearest`:
        // |p + r n - q| = r  =>  r = |p - q|^2 / (2 n . (q - p)).
        let towards = sub(nearest, p);
        let along = 2.0 * dot(inward, towards);
        if along <= 0.0 {
            // The surface curls back behind p's tangent plane right at p
            // (a concave crease): no ball of positive size; keep the ray.
            return Some(ray_spot);
        }
        let next = length2(towards) / along;
        if !(next < radius) {
            break;
        }
        radius = next;
        contact = nearest;
        if radius <= floor {
            break;
        }
    }
    // A ball this small is the sample sitting a hair from an edge (skinny
    // cap triangles of extruded text put interior points within a micron of
    // a wall), where the arithmetic is at the floor's scale; it says nothing
    // about thickness, so measure straight across instead.
    if radius < (floor * 100.0).max(5e-5) && ray > 4.0 * radius {
        return Some(ray_spot);
    }
    if radius >= ray * 0.5 * (1.0 - 1e-9) {
        // Never shrank: the ray's far wall bounds the ball, a plain wall.
        return Some(ray_spot);
    }
    let centre = add(p, mul(inward, radius));
    let outward_p = mul(inward, -1.0);
    let to_contact = sub(contact, centre);
    let contact_length = length2(to_contact).sqrt();
    if contact_length <= 0.0 {
        return Some(ray_spot);
    }
    let outward_q = mul(to_contact, 1.0 / contact_length);
    let cosine = dot(outward_p, outward_q);
    if cosine > OPPOSED_CONTACTS_COS {
        // Where the tangent planes at p and at the contact meet, nearest the
        // centre: c + r / (1 + cos) (n_p + n_q).
        let meet = add(centre, mul(add(outward_p, outward_q), radius / (1.0 + cosine)));
        let pinched = index.any_within(meet, 0.25 * radius) || !index.contains(meet);
        if pinched {
            // Slide the ball away from the edge along the bisector of the
            // two faces, growing it, until it touches a third surface: the
            // edge's true local size (a box edge grows to the box, a thin
            // plate's edge to the plate, a sharp lip to the lip).
            let grown = grow_along_bisector(index, meet, centre, radius, floor, [(p, outward_p), (contact, outward_q)]);
            return match grown {
                Some(ball) if ball.diameter < ray => Some({
                    let spot = ThinSpot {
                        thickness: ball.diameter,
                        from: p,
                        to: ball.contact,
                        method: "ball",
                    };
                    walls.classify(
                        spot,
                        ball.centre,
                        ball.diameter / 2.0,
                        [(ball.opposed_point, ball.opposed_normal), (ball.contact, ball.normal)],
                    )
                }),
                _ => Some(ray_spot),
            };
        }
    }
    let spot = ThinSpot {
        thickness: (2.0 * radius).min(ray),
        from: p,
        to: contact,
        method: "ball",
    };
    Some(walls.classify(spot, centre, radius, [(p, outward_p), (contact, outward_q)]))
}

/// What one thickness scan measures against: the solid, and its sharp
/// convex edges (see [`sharp_convex_edges`]).
struct Walls<'a> {
    index: &'a MeshIndex,
    sharp: Option<MeshIndex>,
    t_min: f64,
}

impl<'a> Walls<'a> {
    fn new(index: &'a MeshIndex) -> Self {
        Self {
            index,
            sharp: sharp_convex_edges(index),
            t_min: (bounds_span(&index.bounds()) * 1e-9).max(1e-9),
        }
    }

    /// `spot` as measured, or as a sharp-edge tip: a ball wedged into a sharp
    /// convex edge (thread crest, knife edge), whose balls shrink to nothing
    /// at the edge whatever the part's walls are. It is one when the nearest
    /// sharp edge lies on both of the ball's tangent planes (through `a` with
    /// outward normal `normal_a`, and `b` / `normal_b`): that edge is the
    /// wedge's apex. Two parallel walls never qualify (no point is on both
    /// planes), however near a sharp edge elsewhere.
    fn classify(&self, spot: ThinSpot, centre: P, radius: f64, contacts: [(P, P); 2]) -> ThinSpot {
        let Some(sharp) = &self.sharp else {
            return spot;
        };
        let Some((distance, edge)) = sharp.nearest(centre) else {
            return spot;
        };
        let on_plane = |(point, normal): (P, P)| dot(normal, sub(edge, point)).abs() <= 0.5 * radius;
        // A wedge of opening a puts its apex r / sin(a / 2) from the centre;
        // 60 r covers wedges down to about 2 degrees.
        if distance <= 60.0 * radius && contacts.into_iter().all(on_plane) {
            ThinSpot { method: "tip", ..spot }
        } else {
            spot
        }
    }
}

/// The convex edges of the surface sharper than a wall's far side (outward
/// normals 100 degrees or more apart: wedges of 80 degrees or sharper, such
/// as thread crests and knife edges), as needle triangles for a nearest-
/// point index; `None` when there are none.
fn sharp_convex_edges(index: &MeshIndex) -> Option<MeshIndex> {
    let key = |p: P| p.map(f64::to_bits);
    let mut faces: std::collections::HashMap<([u64; 3], [u64; 3]), Vec<usize>> =
        std::collections::HashMap::new();
    for (triangle, t) in index.triangles.iter().enumerate() {
        for i in 0..3 {
            let (a, b) = (key(t[i]), key(t[(i + 1) % 3]));
            faces.entry(if a < b { (a, b) } else { (b, a) }).or_default().push(triangle);
        }
    }
    let mut needles = Vec::new();
    for ((a, b), owners) in faces {
        let [first, second] = owners[..] else {
            continue;
        };
        let (Some((n1, _)), Some((n2, _))) = (
            unit_normal(&index.triangles[first]),
            unit_normal(&index.triangles[second]),
        ) else {
            continue;
        };
        let convex = dot(n1, sub(centroid(&index.triangles[second]), centroid(&index.triangles[first]))) < 0.0;
        if convex && dot(n1, n2) <= OPPOSED_CONTACTS_COS {
            let (a, b) = (a.map(f64::from_bits), b.map(f64::from_bits));
            // A needle: the edge with a third corner a hair off it, so the
            // nearest point on it is the nearest point on the edge.
            let hair = mul(add(n1, n2), 1e-9);
            needles.push([a, b, add(b, hair)]);
        }
    }
    (!needles.is_empty()).then(|| MeshIndex::new(needles))
}

/// Slides a ball tangent to two planes (`planes`: a point and outward normal each, meeting
/// along a line through `meet`) from `centre` / `radius` along their
/// bisector, away from the line, the radius growing in proportion to the
/// distance from it, until it touches a third surface. Returns the ball's
/// diameter and that third contact when the contact faces one of the two
/// planes as a wall's far side does (see [`OPPOSED_CONTACTS_COS`]); `None`
/// when the ball was stopped by another face of the same convex corner (a
/// box corner, the next facet of a tessellated cylinder), which says nothing
/// about thickness.
struct GrownBall {
    diameter: f64,
    centre: P,
    /// The third contact and its outward normal.
    contact: P,
    normal: P,
    /// The first two planes' one it faces (a point on it and its normal).
    opposed_point: P,
    opposed_normal: P,
}

fn grow_along_bisector(
    index: &MeshIndex,
    meet: P,
    centre: P,
    radius: f64,
    floor: f64,
    planes: [(P, P); 2],
) -> Option<GrownBall> {
    let away = sub(centre, meet);
    let start = length2(away).sqrt();
    if start <= 0.0 || radius <= 0.0 {
        return None;
    }
    let direction = mul(away, 1.0 / start);
    let ball_at = |t: f64| (add(meet, mul(direction, t)), radius * t / start);
    let fits = |t: f64| {
        let (centre, ball) = ball_at(t);
        !index.any_within(centre, ball * (1.0 - 1e-7) - floor)
    };
    let cap = start + 2.0 * bounds_span(&index.bounds());
    let (mut low, mut high) = (start, start * 2.0);
    while fits(high) {
        if high >= cap {
            return None;
        }
        low = high;
        high *= 2.0;
    }
    // To a part in ten thousand of the ball's size: far below any sample
    // spacing, at about half the queries of a fixed 24 halvings.
    for _ in 0..24 {
        if high - low <= low * 1e-4 {
            break;
        }
        let middle = 0.5 * (low + high);
        if fits(middle) {
            low = middle;
        } else {
            high = middle;
        }
    }
    let (centre, ball) = ball_at(high);
    let (_, contact) = index.nearest(centre)?;
    let towards = sub(contact, centre);
    let length = length2(towards).sqrt();
    if length <= 0.0 {
        return None;
    }
    let outward = mul(towards, 1.0 / length);
    let (opposed_point, opposed_normal) = *planes
        .iter()
        .min_by(|x, y| dot(x.1, outward).total_cmp(&dot(y.1, outward)))?;
    (dot(opposed_normal, outward) <= OPPOSED_CONTACTS_COS).then(|| GrownBall {
        diameter: 2.0 * ball.min(radius * low / start).max(radius),
        centre,
        contact,
        normal: outward,
        opposed_point,
        opposed_normal,
    })
}

/// Sample points on a triangle: the centroid, then near each corner and
/// across each edge, then (for a triangle much larger than `spacing`) a
/// barycentric grid of about that spacing, at most `cap` points. A facet no
/// larger than `spacing` gets its corner and edge points only next to a
/// creased edge (`creases[i]`: edge i -> i+1), where the wall can change
/// abruptly; on a smooth tessellated curve its centroid already samples the
/// surface at the spacing, and seven points per tiny facet were most of the
/// scan's cost there.
fn triangle_samples(t: &[P; 3], area: f64, spacing: f64, cap: usize, creases: [bool; 3]) -> Vec<P> {
    // Centroid, corners 0..2, then edges 0-1, 0-2 (edge 2), 1-2 (edge 1).
    const WEIGHTS: [[f64; 3]; 7] = [
        [1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0],
        [0.8, 0.1, 0.1],
        [0.1, 0.8, 0.1],
        [0.1, 0.1, 0.8],
        [0.45, 0.45, 0.1],
        [0.45, 0.1, 0.45],
        [0.1, 0.45, 0.45],
    ];
    let large = (2.0 * area).sqrt() > spacing;
    let wanted = [
        true,
        large || creases[0] || creases[2],
        large || creases[0] || creases[1],
        large || creases[1] || creases[2],
        large || creases[0],
        large || creases[2],
        large || creases[1],
    ];
    let at = |w: [f64; 3]| add(add(mul(t[0], w[0]), mul(t[1], w[1])), mul(t[2], w[2]));
    let mut points: Vec<P> = WEIGHTS
        .iter()
        .zip(wanted)
        .filter(|(_, wanted)| *wanted)
        .map(|(w, _)| at(*w))
        .collect();
    // A grid with n divisions per side has about n^2 / 2 cells of area
    // area / n^2 * 2, so spacing^2 per cell needs n ~ sqrt(2 area) / spacing.
    let divisions = ((2.0 * area).sqrt() / spacing.max(1e-9)).ceil() as usize;
    if divisions > 3 {
        let divisions = divisions.min(((2 * cap) as f64).sqrt() as usize).max(4);
        let step = 1.0 / divisions as f64;
        for i in 0..divisions {
            for j in 0..divisions - i {
                let (u, v) = ((i as f64 + 1.0 / 3.0) * step, (j as f64 + 1.0 / 3.0) * step);
                if u + v < 1.0 {
                    points.push(at([1.0 - u - v, u, v]));
                }
            }
        }
    }
    points.truncate(cap.max(1));
    points
}

/// A concave (valley) edge of the surface: its endpoints, the inward
/// bisector of its two faces' normals, and one of the faces.
#[derive(Clone, Copy, Debug)]
struct ConcaveEdge {
    a: P,
    b: P,
    inward: P,
    triangle: usize,
}

/// Edges where two faces meet in a valley (the material's inside angle is
/// over 180 degrees). The thinnest spot of a ligament often sits right on
/// one (a groove's root, the inner corner of a socket), where a ball tangent
/// to either face cannot reach it, so they are sampled along their length
/// with the bisector normal. Only edges of `triangles` are considered.
fn concave_edges(index: &MeshIndex, triangles: &[usize]) -> Vec<ConcaveEdge> {
    let key = |p: P| p.map(f64::to_bits);
    let mut faces: std::collections::HashMap<([u64; 3], [u64; 3]), Vec<usize>> =
        std::collections::HashMap::new();
    for &triangle in triangles {
        let t = &index.triangles[triangle];
        for i in 0..3 {
            let (a, b) = (key(t[i]), key(t[(i + 1) % 3]));
            faces.entry(if a < b { (a, b) } else { (b, a) }).or_default().push(triangle);
        }
    }
    let mut edges = Vec::new();
    for ((a, b), owners) in faces {
        let [first, second] = owners[..] else {
            continue;
        };
        let (Some((n1, _)), Some((n2, _))) = (
            unit_normal(&index.triangles[first]),
            unit_normal(&index.triangles[second]),
        ) else {
            continue;
        };
        // Valley: the second face's centroid is on the outer side of the
        // first face's plane (by more than flat-ish noise).
        let rise = dot(n1, sub(centroid(&index.triangles[second]), centroid(&index.triangles[first])));
        let bisector = add(n1, n2);
        let length = length2(bisector).sqrt();
        if rise <= 1e-9 || dot(n1, n2) > 0.999 || length < 1e-9 {
            continue;
        }
        edges.push(ConcaveEdge {
            a: a.map(f64::from_bits),
            b: b.map(f64::from_bits),
            inward: mul(bisector, -1.0 / length),
            triangle: first,
        });
    }
    // HashMap order is arbitrary; keep results reproducible.
    edges.sort_by(|x, y| x.a.partial_cmp(&y.a).unwrap_or(std::cmp::Ordering::Equal));
    edges
}

/// Points of triangle `t` within `radius` of `centre`: its nearest point to
/// the centre, then a grid of `spacing` in the triangle's plane around it,
/// kept inside the triangle; at most `cap`.
fn disc_samples(t: &[P; 3], centre: P, radius: f64, spacing: f64, cap: usize) -> Vec<P> {
    let nearest = closest_point_on_triangle(centre, t[0], t[1], t[2]);
    let mut points = vec![nearest];
    let Some((normal, _)) = unit_normal(t) else {
        return points;
    };
    let edge = sub(t[1], t[0]);
    let u = mul(edge, 1.0 / length2(edge).sqrt());
    let v = cross(normal, u);
    // Barycentric inside test with a little slack for points on edges.
    let (e0, e1) = (sub(t[1], t[0]), sub(t[2], t[0]));
    let (d00, d01, d11) = (dot(e0, e0), dot(e0, e1), dot(e1, e1));
    let denominator = d00 * d11 - d01 * d01;
    let inside = |p: P| {
        let w = sub(p, t[0]);
        let (d20, d21) = (dot(w, e0), dot(w, e1));
        let b1 = (d11 * d20 - d01 * d21) / denominator;
        let b2 = (d00 * d21 - d01 * d20) / denominator;
        b1 >= -1e-9 && b2 >= -1e-9 && b1 + b2 <= 1.0 + 1e-9
    };
    let steps = (radius / spacing).ceil() as i64;
    let radius2 = radius * radius;
    'grid: for i in -steps..=steps {
        for j in -steps..=steps {
            if points.len() >= cap.max(1) {
                break 'grid;
            }
            if i == 0 && j == 0 {
                continue;
            }
            let point = add(nearest, add(mul(u, i as f64 * spacing), mul(v, j as f64 * spacing)));
            if length2(sub(point, centre)) <= radius2 && inside(point) {
                points.push(point);
            }
        }
    }
    // A point on (or a hair from) the triangle's boundary sees the
    // neighbouring face at distance ~0 and its ball degenerates; pull every
    // point a tenth of the spacing towards the centroid (never past it).
    let middle = centroid(t);
    points
        .into_iter()
        .map(|point| {
            let towards = sub(middle, point);
            let distance = length2(towards).sqrt();
            if distance <= 0.0 {
                return point;
            }
            add(point, mul(towards, (0.1 * spacing / distance).min(1.0)))
        })
        .collect()
}

/// One surface point whose wall is to be measured.
#[derive(Clone, Copy, Debug)]
struct WallSample {
    point: P,
    inward: P,
    /// The triangle the point is on (skipped by its own ray).
    triangle: usize,
    /// Position in the scan's triangle list, for the skipped count.
    slot: usize,
    /// Surface area the sample stands for (0 for edge and refinement points).
    area: f64,
}

/// A sample's measured wall, with the area it stands for.
#[derive(Clone, Copy, Debug)]
struct Measured {
    spot: ThinSpot,
    area: f64,
}

/// Most worker threads one thickness scan uses. Bounded well below the
/// machine so concurrent requests share the cores rather than each claiming
/// all of them (the render queue already bounds how many run at once).
const MAX_THICKNESS_WORKERS: usize = 4;

/// Below this many samples a scan stays on the request's thread: spawning
/// costs more than it saves.
const PARALLEL_SAMPLES: usize = 512;

/// Measures `samples` (in parallel when there are many), stopping at the
/// request deadline or cancellation. Returns, per sample in order, its wall
/// (`None` when unmeasurable or not reached), and whether every sample was
/// reached.
fn measure_samples(walls: &Walls, samples: &[WallSample]) -> (Vec<Option<ThinSpot>>, Vec<bool>) {
    let run = |chunk: &[WallSample]| -> (Vec<Option<ThinSpot>>, Vec<bool>) {
        let mut spots = Vec::with_capacity(chunk.len());
        let mut reached = Vec::with_capacity(chunk.len());
        for (position, sample) in chunk.iter().enumerate() {
            if position % 32 == 0 && csg::bsp::deadline_passed() {
                break;
            }
            spots.push(wall_at(walls, sample.point, sample.inward, sample.triangle));
            reached.push(true);
        }
        spots.resize(chunk.len(), None);
        reached.resize(chunk.len(), false);
        (spots, reached)
    };
    let workers = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(MAX_THICKNESS_WORKERS)
        .min(samples.len().div_ceil(PARALLEL_SAMPLES).max(1));
    if workers <= 1 {
        return run(samples);
    }
    // Interleaved blocks rather than one contiguous run per worker, so one
    // dense corner of the model does not leave the other workers idle.
    const BLOCK: usize = 64;
    let blocks: Vec<&[WallSample]> = samples.chunks(BLOCK).collect();
    let results: Vec<Vec<(usize, (Vec<Option<ThinSpot>>, Vec<bool>))>> =
        csg::bsp::Interrupts::with_current(|interrupts| {
            std::thread::scope(|scope| {
                let handles: Vec<_> = (0..workers)
                    .map(|worker| {
                        let blocks = &blocks;
                        let run = &run;
                        scope.spawn(move || {
                            interrupts.install(|| {
                                (worker..blocks.len())
                                    .step_by(workers)
                                    .map(|block| (block, run(blocks[block])))
                                    .collect::<Vec<_>>()
                            })
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| handle.join().unwrap_or_default())
                    .collect()
            })
        });
    let mut ordered: Vec<Option<(Vec<Option<ThinSpot>>, Vec<bool>)>> = vec![None; blocks.len()];
    for (block, result) in results.into_iter().flatten() {
        ordered[block] = Some(result);
    }
    let mut spots = Vec::with_capacity(samples.len());
    let mut reached = Vec::with_capacity(samples.len());
    for (block, result) in ordered.into_iter().enumerate() {
        // A worker that panicked leaves its blocks unmeasured, not the scan.
        let (block_spots, block_reached) =
            result.unwrap_or_else(|| (vec![None; blocks[block].len()], vec![false; blocks[block].len()]));
        spots.extend(block_spots);
        reached.extend(block_reached);
    }
    (spots, reached)
}

/// Dihedral angle past which an edge counts as a crease for sampling: the
/// corner and edge samples of a triangle are spent only next to creases,
/// where the wall can change abruptly. A smooth tessellated curve ($fn of
/// 24 or more) gets one sample per small facet plus a grid on large ones.
const CREASE_COS: f64 = 0.939_692_620_785_908_4; // cos 20 degrees

/// Per triangle of `triangles`: which of its edges (i -> i+1) are creases,
/// boundary or non-manifold edges.
fn triangle_creases(index: &MeshIndex, triangles: &[usize]) -> Vec<[bool; 3]> {
    let key = |p: P| p.map(f64::to_bits);
    let mut faces: std::collections::HashMap<([u64; 3], [u64; 3]), Vec<usize>> =
        std::collections::HashMap::with_capacity(triangles.len() * 3 / 2 + 1);
    for (triangle, t) in index.triangles.iter().enumerate() {
        for i in 0..3 {
            let (a, b) = (key(t[i]), key(t[(i + 1) % 3]));
            faces.entry(if a < b { (a, b) } else { (b, a) }).or_default().push(triangle);
        }
    }
    triangles
        .iter()
        .map(|&triangle| {
            let t = &index.triangles[triangle];
            let normal = unit_normal(t).map(|(n, _)| n);
            let mut creased = [true; 3];
            for (i, crease) in creased.iter_mut().enumerate() {
                let (a, b) = (key(t[i]), key(t[(i + 1) % 3]));
                let owners = &faces[&if a < b { (a, b) } else { (b, a) }];
                if let (&[first, second], Some(n)) = (&owners[..], normal) {
                    let other = if first == triangle { second } else { first };
                    if let Some((m, _)) = unit_normal(&index.triangles[other]) {
                        *crease = dot(n, m) < CREASE_COS;
                    }
                }
            }
            creased
        })
        .collect()
}

/// Triangles (slots `0..planned`) with any sample the deadline cut off —
/// a triangle counts as measured only when all its samples were — and
/// whether any valley-edge sample (no slot) was cut off.
fn unreached_slots(samples: &[WallSample], reached: &[bool], planned: usize) -> (usize, bool) {
    let mut cut = vec![false; planned];
    let mut edges_cut = false;
    for (sample, reached) in samples.iter().zip(reached) {
        if *reached {
            continue;
        }
        match cut.get_mut(sample.slot) {
            Some(slot) => *slot = true,
            None => edges_cut = true,
        }
    }
    (cut.into_iter().filter(|cut| *cut).count(), edges_cut)
}

/// Measures the wall at every sample point of `triangles` (indices into the
/// index), within `budget` points and the request deadline; `keep` filters
/// sample points (a probe's sphere). `sample` gets each triangle, its area,
/// a point cap and its creased edges.
#[allow(clippy::too_many_arguments)]
fn scan_walls(
    index: &MeshIndex,
    triangles: &[usize],
    threshold: f64,
    spacing: f64,
    budget: usize,
    sample: &dyn Fn(&[P; 3], f64, usize, [bool; 3]) -> Vec<P>,
    keep: &dyn Fn(P) -> bool,
    refine: bool,
) -> ThicknessCheck {
    let walls = Walls::new(index);
    let per_triangle = (budget / triangles.len().max(1)).clamp(1, 4096);
    let mut check = ThicknessCheck {
        threshold,
        samples: 0,
        triangles: index.triangles.len(),
        skipped: 0,
        spacing,
        min: None,
        below_threshold: 0,
        below_threshold_area: 0.0,
        sharp_tips: 0,
        thinnest: Vec::new(),
        low_quantile: None,
        lowest: Vec::new(),
        regions: Vec::new(),
        region_count: 0,
        refined_samples: 0,
        interrupted: false,
    };
    // Phase 1: the sample points (cheap, sequential, deterministic).
    let creases = triangle_creases(index, triangles);
    let mut samples: Vec<WallSample> = Vec::new();
    let mut planned = 0usize;
    for (slot, &triangle) in triangles.iter().enumerate() {
        if samples.len() >= budget {
            break;
        }
        planned = slot + 1;
        let t = &index.triangles[triangle];
        let Some((normal, area)) = unit_normal(t) else {
            continue;
        };
        let inward = mul(normal, -1.0);
        let points: Vec<P> = sample(t, area, per_triangle, creases[slot])
            .into_iter()
            .filter(|point| keep(*point))
            .collect();
        let share = area / points.len().max(1) as f64;
        samples.extend(points.into_iter().map(|point| WallSample { point, inward, triangle, slot, area: share }));
    }
    // Valley edges of the planned triangles, along their length. Their
    // samples stand for no area of their own.
    for edge in concave_edges(index, &triangles[..planned]) {
        if samples.len() >= budget * 2 {
            break;
        }
        let length = length2(sub(edge.b, edge.a)).sqrt();
        let steps = ((length / spacing).ceil() as usize).clamp(1, 4096);
        for step in 0..steps {
            let along = (step as f64 + 0.5) / steps as f64;
            let point = add(edge.a, mul(sub(edge.b, edge.a), along));
            if keep(point) {
                samples.push(WallSample { point, inward: edge.inward, triangle: edge.triangle, slot: usize::MAX, area: 0.0 });
            }
        }
    }
    // Phase 2: measure them.
    let (spots, reached) = measure_samples(&walls, &samples);
    let mut measured: Vec<Measured> = Vec::with_capacity(samples.len());
    for (sample, spot) in samples.iter().zip(&spots) {
        if let Some(spot) = spot {
            record(&mut check, &mut measured, *spot, sample.area);
        }
    }
    let (unmeasured, edges_cut) = unreached_slots(&samples, &reached, planned);
    check.skipped = unmeasured + (triangles.len() - planned) + (index.triangles.len() - triangles.len());
    check.interrupted |= edges_cut;
    // Phase 3: around each thin region (or the thinnest spot when nothing is
    // under the threshold), measure densely to pin down the true minimum,
    // which a sample spacing set by the whole part's area easily straddles.
    if refine && csg::bsp::deadline_passed() {
        check.interrupted = true;
    } else if refine {
        let seeds: Vec<ThinSpot> = {
            let (regions, _) = thin_regions(&measured, threshold, spacing);
            let mut seeds: Vec<ThinSpot> = regions.iter().take(MAX_CHECK_LOCATIONS).map(|region| region.min).collect();
            if seeds.is_empty() {
                seeds.extend(check.min);
            }
            seeds
        };
        let radius = (spacing * 1.5).max(threshold * 0.25).max(1e-6);
        let fine = (radius / 8.0).max(1e-6);
        let mut extra: Vec<WallSample> = Vec::new();
        for seed in seeds {
            let radius2 = radius * radius;
            for triangle in triangles_near(index, seed.from, radius) {
                let t = &index.triangles[triangle];
                let Some((normal, _)) = unit_normal(t) else {
                    continue;
                };
                // A point a hair from a convex edge measures the edge's
                // numerics, not the wall (its ball is pinched to the floor
                // scale): keep refinement points a quarter step inside.
                let margin2 = (0.25 * fine) * (0.25 * fine);
                let inside = |point: P| {
                    (0..3).all(|i| length2(sub(point, closest_point_on_segment(point, t[i], t[(i + 1) % 3]))) >= margin2)
                };
                for point in disc_samples(t, seed.from, radius, fine, 64) {
                    if length2(sub(point, seed.from)) <= radius2 && keep(point) && inside(point) {
                        extra.push(WallSample { point, inward: mul(normal, -1.0), triangle, slot: usize::MAX, area: 0.0 });
                    }
                }
                if extra.len() >= REFINE_POINTS {
                    break;
                }
            }
            if extra.len() >= REFINE_POINTS {
                break;
            }
        }
        let (spots, reached) = measure_samples(&walls, &extra);
        check.interrupted |= reached.iter().any(|reached| !reached);
        let (samples, below, tips) = (check.samples, check.below_threshold, check.sharp_tips);
        for spot in spots.into_iter().flatten() {
            record(&mut check, &mut measured, spot, 0.0);
        }
        // Refinement points are counted in refinedSamples, not in samples
        // or belowThreshold (which stand for the surface coverage); they do
        // set min and join the regions.
        check.refined_samples = check.samples - samples;
        check.samples = samples;
        check.below_threshold = below;
        check.sharp_tips = tips;
    }
    let (regions, regions_complete) = thin_regions(&measured, threshold, spacing);
    check.interrupted |= !regions_complete;
    check.region_count = regions.len();
    check.regions = regions.into_iter().take(MAX_CHECK_LOCATIONS).collect();
    let mut spots: Vec<ThinSpot> = measured.into_iter().map(|entry| entry.spot).collect();
    spots.sort_by(|a, b| a.thickness.total_cmp(&b.thickness));
    // The smallest thickness at least three samples agree on (within 10%):
    // one stray sample cannot set it, and unlike a percentile it does not
    // depend on how much thick surface the part also has.
    check.low_quantile = spots
        .windows(3)
        .find(|run| run[2].thickness <= run[0].thickness * 1.1 + 1e-6)
        .map(|run| run[0])
        .or_else(|| spots.first().copied());
    let spread = threshold.clamp(0.5, 5.0);
    for spot in &spots {
        if check.thinnest.len() >= MAX_CHECK_LOCATIONS {
            break;
        }
        if check
            .thinnest
            .iter()
            .all(|kept| length2(sub(kept.from, spot.from)) > spread * spread)
        {
            check.thinnest.push(*spot);
        }
    }
    check.lowest = spots;
    check
}

/// Most extra points the refinement pass of one scan measures.
const REFINE_POINTS: usize = 4000;

/// Triangles with a point within `radius` of `centre`.
fn triangles_near(index: &MeshIndex, centre: P, radius: f64) -> Vec<usize> {
    let radius2 = radius * radius;
    let mut found = Vec::new();
    let mut stack = vec![0usize];
    while let Some(node_index) = stack.pop() {
        let Some(node) = index.nodes.get(node_index) else {
            break;
        };
        if node.bounds.point_distance2(centre) > radius2 {
            continue;
        }
        if node.count > 0 {
            for &triangle in &index.order[node.start..node.start + node.count] {
                let t = &index.triangles[triangle];
                if length2(sub(closest_point_on_triangle(centre, t[0], t[1], t[2]), centre)) <= radius2 {
                    found.push(triangle);
                }
            }
        } else {
            stack.push(node.start);
            stack.push(node.start + 1);
        }
    }
    found.sort_unstable();
    found
}

fn record(check: &mut ThicknessCheck, measured: &mut Vec<Measured>, spot: ThinSpot, area: f64) {
    if spot.method == "tip" {
        check.sharp_tips += 1;
        return;
    }
    check.samples += 1;
    if check.min.is_none_or(|best| spot.thickness < best.thickness) {
        check.min = Some(spot);
    }
    if spot.thickness < check.threshold {
        check.below_threshold += 1;
        check.below_threshold_area += area;
    }
    measured.push(Measured { spot, area });
}

/// A connected patch of surface thinner than the threshold.
#[derive(Clone, Debug, PartialEq)]
pub struct ThinRegion {
    /// Its thinnest measured wall.
    pub min: ThinSpot,
    /// Surface area the region's samples stand for (mm^2).
    pub area: f64,
    pub samples: usize,
    /// Bounds of the region's sample points.
    pub bounds: Aabb,
}

/// Most sample pairs compared between two neighbouring grid cells before
/// they are left apart: bounds the grouping on dense thin patches.
const REGION_PAIR_CHECKS: usize = 256;

/// Groups the thin samples into regions: samples within a couple of sample
/// spacings of each other (or a quarter of the threshold, on a coarse scan)
/// are one region. Thinnest region first; `false` when the deadline or a
/// cancellation stopped the grouping (regions may then be split).
///
/// Linear in the samples: the grid's cells are small enough (link / sqrt 3)
/// that a cell's samples are all within the link and join at once; two
/// cells join when any pair across them is, comparing at most
/// [`REGION_PAIR_CHECKS`] pairs and none once they share a region.
fn thin_regions(measured: &[Measured], threshold: f64, spacing: f64) -> (Vec<ThinRegion>, bool) {
    let thin: Vec<&Measured> = measured.iter().filter(|entry| entry.spot.thickness < threshold).collect();
    if thin.is_empty() {
        return (Vec::new(), true);
    }
    let link = (spacing * 2.5).max(threshold * 0.25).max(1e-6);
    let size = link / 3f64.sqrt();
    let key = |p: P| p.map(|c| (c / size).floor() as i64);
    let mut cells: std::collections::HashMap<[i64; 3], Vec<usize>> = std::collections::HashMap::new();
    for (i, entry) in thin.iter().enumerate() {
        cells.entry(key(entry.spot.from)).or_default().push(i);
    }
    let mut groups = Groups::new(thin.len());
    for members in cells.values() {
        for pair in members.windows(2) {
            groups.union(pair[0], pair[1]);
        }
    }
    let link2 = link * link;
    let mut ordered: Vec<(&[i64; 3], &Vec<usize>)> = cells.iter().collect();
    ordered.sort_by_key(|(cell, _)| **cell);
    let mut complete = true;
    for (number, (cell, members)) in ordered.iter().enumerate() {
        if number % 64 == 63 && csg::bsp::deadline_passed() {
            complete = false;
            break;
        }
        // Cells up to two away can hold points within the link.
        for dx in -2i64..=2 {
            for dy in -2i64..=2 {
                for dz in -2i64..=2 {
                    let other = [cell[0] + dx, cell[1] + dy, cell[2] + dz];
                    if other <= **cell {
                        continue;
                    }
                    let Some(others) = cells.get(&other) else {
                        continue;
                    };
                    if groups.find(members[0]) == groups.find(others[0]) {
                        continue;
                    }
                    let mut checks = 0usize;
                    'pairs: for &i in members.iter() {
                        for &j in others {
                            checks += 1;
                            if checks > REGION_PAIR_CHECKS {
                                break 'pairs;
                            }
                            if length2(sub(thin[i].spot.from, thin[j].spot.from)) <= link2 {
                                groups.union(i, j);
                                break 'pairs;
                            }
                        }
                    }
                }
            }
        }
    }
    let mut regions: std::collections::HashMap<usize, ThinRegion> = std::collections::HashMap::new();
    for (i, entry) in thin.iter().enumerate() {
        let root = groups.find(i);
        let region = regions.entry(root).or_insert_with(|| ThinRegion {
            min: entry.spot,
            area: 0.0,
            samples: 0,
            bounds: Aabb::empty(),
        });
        region.area += entry.area;
        region.samples += 1;
        region.bounds.grow(entry.spot.from);
        if entry.spot.thickness < region.min.thickness {
            region.min = entry.spot;
        }
    }
    let mut regions: Vec<ThinRegion> = regions.into_values().collect();
    regions.sort_by(|a, b| {
        a.min
            .thickness
            .total_cmp(&b.min.thickness)
            .then(b.area.total_cmp(&a.area))
            .then(a.min.from.partial_cmp(&b.min.from).unwrap_or(std::cmp::Ordering::Equal))
    });
    (regions, complete)
}

/// Minimum wall / feature thickness of a solid, from surface samples: each
/// triangle's centroid, points near its creased corners and edges, a grid on
/// large faces, and points along valley edges, each measured by [`wall_at`]
/// (inscribed ball, falling back to the straight inward ray where the ball
/// is pinched at a convex edge); then a dense pass around each thin region
/// (or the thinnest spot) pins its minimum down. It is still a sampled
/// estimate: `spacing` says how far apart the samples are, and
/// [`thickness_probe`] measures one region densely. `max_samples` caps the
/// triangles sampled (a stride beyond it).
pub fn thickness_check(index: &MeshIndex, threshold: f64, max_samples: usize) -> ThicknessCheck {
    let count = index.triangles.len();
    let stride = count.div_ceil(max_samples.max(1)).max(1);
    let triangles: Vec<usize> = (0..count).step_by(stride).collect();
    let area: f64 = triangles
        .iter()
        .filter_map(|&triangle| unit_normal(&index.triangles[triangle]).map(|(_, area)| area))
        .sum();
    // Spread the point budget evenly over the surface, but never coarser
    // than the threshold itself where the budget allows.
    let spacing = (area / MAX_THICKNESS_POINTS as f64).sqrt().max(threshold * 0.5).max(1e-6);
    let mut check = scan_walls(
        index,
        &triangles,
        threshold,
        spacing,
        MAX_THICKNESS_POINTS,
        &|t, area, cap, creases| triangle_samples(t, area, spacing, cap, creases),
        &|_| true,
        true,
    );
    check.spacing = spacing.min((area / check.samples.max(1) as f64).sqrt());
    check
}

/// [`thickness_check`] over only the sample points `keep` accepts (e.g. the
/// middle band of an extrusion's side walls, to read a 2D width).
pub fn thickness_check_where(
    index: &MeshIndex,
    threshold: f64,
    keep: &dyn Fn(P) -> bool,
) -> ThicknessCheck {
    let count = index.triangles.len();
    let stride = count.div_ceil(MAX_THICKNESS_SAMPLES).max(1);
    let triangles: Vec<usize> = (0..count).step_by(stride).collect();
    let area: f64 = triangles
        .iter()
        .filter_map(|&triangle| unit_normal(&index.triangles[triangle]).map(|(_, area)| area))
        .sum();
    let spacing = (area / MAX_THICKNESS_POINTS as f64).sqrt().max(threshold * 0.5).max(1e-6);
    scan_walls(
        index,
        &triangles,
        threshold,
        spacing,
        MAX_THICKNESS_POINTS,
        &|t, area, cap, _| triangle_samples(t, area, spacing, cap, [true; 3]),
        keep,
        false,
    )
}

/// Dense wall-thickness measurement of the surface within `radius` of
/// `centre`: every triangle reaching into the sphere is sampled at about
/// `radius / 25` spacing (within the point budget).
pub fn thickness_probe(index: &MeshIndex, centre: P, radius: f64, threshold: f64) -> ThicknessCheck {
    let triangles = triangles_near(index, centre, radius);
    let radius2 = radius * radius;
    let spacing = (radius / 25.0).max(1e-6);
    // Each triangle is sampled on a grid around its point nearest the
    // centre, not over its whole area, so a large face reaching into a
    // small probe is sampled densely where the probe is.
    let mut check = scan_walls(
        index,
        &triangles,
        threshold,
        spacing,
        MAX_THICKNESS_POINTS,
        &|t, _, cap, _| disc_samples(t, centre, radius, spacing, cap),
        &|point| length2(sub(point, centre)) <= radius2,
        false,
    );
    // Only the triangles in reach count as the probe's job.
    check.skipped = check.skipped.saturating_sub(index.triangles.len() - triangles.len());
    check.triangles = triangles.len();
    check
}

#[derive(Clone, Debug, PartialEq)]
pub struct OverhangRegion {
    pub area: f64,
    pub triangles: usize,
    pub bounds: Aabb,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OverhangCheck {
    /// Steepest printable angle, in degrees from vertical.
    pub angle: f64,
    pub area: f64,
    pub triangles: usize,
    pub bounds: Option<Aabb>,
    /// Connected overhang patches, largest first (at most
    /// [`MAX_CHECK_LOCATIONS`]); `regions` counts all of them.
    pub regions: usize,
    pub largest: Vec<OverhangRegion>,
    /// Downward faces resting on the object's lowest plane (the bed), which
    /// need no support and are not counted.
    pub bed_area: f64,
}

/// Downward-facing surface in print orientation (+Z up) that is tilted more
/// than `angle_degrees` from vertical, i.e. needs support. Faces lying on
/// the bed plane `bed_z` are on the bed and excluded; `None` takes the
/// object's own lowest Z. Callers checking an object within a larger model
/// pass the model's lowest Z, so an object floating above the bed has its
/// underside counted as overhang rather than as bed contact.
pub fn overhang_check(index: &MeshIndex, angle_degrees: f64, bed_z: Option<f64>) -> OverhangCheck {
    let bounds = index.bounds();
    let bed = bed_z.unwrap_or(bounds.min[2]);
    let bed_epsilon = (bounds_span(&bounds) * 1e-9).max(1e-6);
    let limit = -angle_degrees.to_radians().sin();
    let mut overhanging = Vec::new();
    let mut bed_area = 0.0;
    for (triangle, t) in index.triangles.iter().enumerate() {
        let Some((normal, area)) = unit_normal(t) else {
            continue;
        };
        if normal[2] >= limit - 1e-9 {
            continue;
        }
        if t.iter().all(|corner| corner[2] <= bed + bed_epsilon) {
            bed_area += area;
            continue;
        }
        overhanging.push((triangle, area));
    }
    // Patches: overhang triangles sharing a vertex.
    let mut groups = Groups::new(overhanging.len());
    let mut owner: std::collections::HashMap<[u64; 3], usize> = std::collections::HashMap::new();
    for (slot, &(triangle, _)) in overhanging.iter().enumerate() {
        for corner in &index.triangles[triangle] {
            match owner.entry(corner.map(f64::to_bits)) {
                std::collections::hash_map::Entry::Occupied(entry) => groups.union(slot, *entry.get()),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(slot);
                }
            }
        }
    }
    let mut regions: std::collections::HashMap<usize, OverhangRegion> =
        std::collections::HashMap::new();
    let mut total = Aabb::empty();
    let mut area = 0.0;
    for (slot, &(triangle, triangle_area)) in overhanging.iter().enumerate() {
        let root = groups.find(slot);
        let region = regions.entry(root).or_insert_with(|| OverhangRegion {
            area: 0.0,
            triangles: 0,
            bounds: Aabb::empty(),
        });
        region.area += triangle_area;
        region.triangles += 1;
        for corner in &index.triangles[triangle] {
            region.bounds.grow(*corner);
            total.grow(*corner);
        }
        area += triangle_area;
    }
    let count = regions.len();
    let mut largest: Vec<OverhangRegion> = regions.into_values().collect();
    largest.sort_by(|a, b| {
        b.area
            .total_cmp(&a.area)
            .then(a.bounds.min.map(f64::to_bits).cmp(&b.bounds.min.map(f64::to_bits)))
    });
    largest.truncate(MAX_CHECK_LOCATIONS);
    OverhangCheck {
        angle: angle_degrees,
        area,
        triangles: overhanging.len(),
        bounds: (!total.is_empty()).then_some(total),
        regions: count,
        largest,
        bed_area,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SharpCrease {
    /// Total length of the crease's edges.
    pub length: f64,
    pub edges: usize,
    /// Smallest interior (dihedral) angle along it, degrees.
    pub min_angle: f64,
    pub bounds: Aabb,
    /// Midpoint of its sharpest edge.
    pub location: P,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SharpEdgeCheck {
    pub threshold: f64,
    /// Mesh edges at or below the threshold, and their total length.
    pub edges: usize,
    pub length: f64,
    /// Chains of such edges sharing vertices (a cylinder's rim is one), all
    /// of them counted, the longest listed.
    pub creases: usize,
    pub longest: Vec<SharpCrease>,
}

/// Convex edges whose interior dihedral angle (90° for a box edge, 135°
/// after a 45° chamfer, 180° for a flat face) is at or below
/// `threshold_degrees`. Concave edges and open/non-manifold edges are not
/// counted.
pub fn sharp_edge_check(triangles: &[[P; 3]], threshold_degrees: f64) -> SharpEdgeCheck {
    let welded = csg::cleanup::weld_triangles(triangles.iter().map(|t| t.map(to_csg)));
    let point = |index: u32| -> P {
        let v = welded.vertices[index as usize];
        [v.x, v.y, v.z]
    };
    let faces: Vec<[P; 3]> = welded.triangles.iter().map(|t| t.map(point)).collect();
    let normals: Vec<Option<P>> = faces.iter().map(|t| unit_normal(t).map(|(n, _)| n)).collect();
    let mut edge_faces: std::collections::HashMap<(u32, u32), Vec<usize>> =
        std::collections::HashMap::new();
    for (face, triangle) in welded.triangles.iter().enumerate() {
        for index in 0..3 {
            let (from, to) = (triangle[index], triangle[(index + 1) % 3]);
            edge_faces
                .entry((from.min(to), from.max(to)))
                .or_default()
                .push(face);
        }
    }
    let slack = 0.01;
    let mut sharp: Vec<((u32, u32), f64)> = Vec::new();
    for (&(a, b), owners) in &edge_faces {
        let [f1, f2] = owners[..] else {
            continue;
        };
        let (Some(n1), Some(n2)) = (normals[f1], normals[f2]) else {
            continue;
        };
        let between = dot(n1, n2).clamp(-1.0, 1.0).acos().to_degrees();
        if between < 1e-6 {
            continue;
        }
        // Convex when face 2's far vertex lies behind face 1's plane.
        let far = welded.triangles[f2]
            .iter()
            .copied()
            .find(|v| *v != a && *v != b)
            .map(point);
        let Some(far) = far else { continue };
        let edge_length = length2(sub(point(a), point(b))).sqrt();
        if dot(n1, sub(far, point(a))) >= -1e-12 * edge_length.max(1.0) {
            continue;
        }
        let interior = 180.0 - between;
        if interior <= threshold_degrees + slack {
            sharp.push(((a, b), interior));
        }
    }
    sharp.sort_by(|x, y| x.0.cmp(&y.0));
    let mut groups = Groups::new(sharp.len());
    let mut owner: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    for (slot, &((a, b), _)) in sharp.iter().enumerate() {
        for vertex in [a, b] {
            match owner.entry(vertex) {
                std::collections::hash_map::Entry::Occupied(entry) => groups.union(slot, *entry.get()),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(slot);
                }
            }
        }
    }
    let mut creases: std::collections::HashMap<usize, SharpCrease> =
        std::collections::HashMap::new();
    let mut total_length = 0.0;
    for (slot, &((a, b), angle)) in sharp.iter().enumerate() {
        let root = groups.find(slot);
        let (pa, pb) = (point(a), point(b));
        let length = length2(sub(pa, pb)).sqrt();
        total_length += length;
        let crease = creases.entry(root).or_insert_with(|| SharpCrease {
            length: 0.0,
            edges: 0,
            min_angle: f64::INFINITY,
            bounds: Aabb::empty(),
            location: pa,
        });
        crease.length += length;
        crease.edges += 1;
        crease.bounds.grow(pa);
        crease.bounds.grow(pb);
        if angle < crease.min_angle {
            crease.min_angle = angle;
            crease.location = mul(add(pa, pb), 0.5);
        }
    }
    let count = creases.len();
    let mut longest: Vec<SharpCrease> = creases.into_values().collect();
    longest.sort_by(|x, y| {
        y.length
            .total_cmp(&x.length)
            .then(x.location.map(f64::to_bits).cmp(&y.location.map(f64::to_bits)))
    });
    longest.truncate(MAX_CHECK_LOCATIONS);
    SharpEdgeCheck {
        threshold: threshold_degrees,
        edges: sharp.len(),
        length: total_length,
        creases: count,
        longest,
    }
}

// ---------------------------------------------------------------------------
// Exact booleans through the CSG kernel.
// ---------------------------------------------------------------------------

/// Combined triangle count above which a boolean is not attempted. The BSP
/// kernel takes no cancellation flag, so this is what bounds its cost on a
/// public endpoint.
pub const MAX_BOOLEAN_TRIANGLES: usize = 60_000;

/// Volume and extent of a boolean result.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BooleanResult {
    pub volume: f64,
    /// `None` when the result is empty.
    pub bounds: Option<Aabb>,
}

/// Why a boolean was not computed.
#[derive(Clone, Debug, PartialEq)]
pub enum BooleanSkipped {
    TooLarge(usize),
    Failed(String),
}

impl std::fmt::Display for BooleanSkipped {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BooleanSkipped::TooLarge(count) => write!(
                formatter,
                "{count} triangles exceed the {MAX_BOOLEAN_TRIANGLES}-triangle budget for an exact boolean; \
                 select fewer objects or lower $fn"
            ),
            BooleanSkipped::Failed(reason) => write!(formatter, "the CSG kernel failed: {reason}"),
        }
    }
}

fn solid_of(triangles: &[[P; 3]]) -> csg::Solid {
    csg::Solid::from_faces(
        triangles
            .iter()
            .map(|t| t.iter().map(|p| to_csg(*p)).collect())
            .collect(),
    )
}

fn solid_result(solid: &csg::Solid) -> BooleanResult {
    let mut bounds = Aabb::empty();
    let mut volume = 0.0;
    for polygon in &solid.polygons {
        let vertices: Vec<P> = polygon.vertices.iter().map(|v| [v.x, v.y, v.z]).collect();
        for vertex in &vertices {
            bounds.grow(*vertex);
        }
        for index in 1..vertices.len().saturating_sub(1) {
            volume += dot(vertices[0], cross(vertices[index], vertices[index + 1])) / 6.0;
        }
    }
    BooleanResult {
        volume: volume.max(0.0),
        bounds: (!bounds.is_empty()).then_some(bounds),
    }
}

fn guarded_boolean(
    a: &[[P; 3]],
    b: &[[P; 3]],
    operation: fn(&csg::Solid, &csg::Solid) -> csg::Solid,
) -> Result<BooleanResult, BooleanSkipped> {
    guarded_solid(a, b, operation).map(|solid| solid_result(&solid))
}

fn guarded_solid(
    a: &[[P; 3]],
    b: &[[P; 3]],
    operation: fn(&csg::Solid, &csg::Solid) -> csg::Solid,
) -> Result<csg::Solid, BooleanSkipped> {
    let total = a.len() + b.len();
    if total > MAX_BOOLEAN_TRIANGLES {
        return Err(BooleanSkipped::TooLarge(total));
    }
    let (left, right) = (solid_of(a), solid_of(b));
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation(&left, &right))) {
        Ok(result) => Ok(result),
        Err(payload) => {
            let text = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "internal error".into());
            Err(BooleanSkipped::Failed(
                text.trim_start_matches(csg::bsp::BUDGET_PANIC_PREFIX)
                    .to_string(),
            ))
        }
    }
}

/// Most triangles of an overlap solid whose thickness is sampled.
const OVERLAP_THICKNESS_SAMPLES: usize = 4096;

/// `a ∩ b`, plus how thick the overlap solid is at its thickest point when
/// it has volume: the local depth of the interference, measured across the
/// overlap itself (see [`inscribed_thickness`]).
pub fn boolean_overlap_measured(
    a: &[[P; 3]],
    b: &[[P; 3]],
) -> (Result<BooleanResult, BooleanSkipped>, Option<(f64, P)>) {
    let (ba, bb) = (triangles_bounds(a), triangles_bounds(b));
    if ba.is_empty() || bb.is_empty() || ba.distance2(&bb) > 0.0 {
        return (
            Ok(BooleanResult {
                volume: 0.0,
                bounds: None,
            }),
            None,
        );
    }
    let solid = match guarded_solid(a, b, csg::Solid::intersection) {
        Ok(solid) => solid,
        Err(skipped) => return (Err(skipped), None),
    };
    let result = solid_result(&solid);
    if result.volume <= 0.0 {
        return (Ok(result), None);
    }
    let mut triangles = Vec::new();
    for polygon in &solid.polygons {
        let vertices: Vec<P> = polygon.vertices.iter().map(|v| [v.x, v.y, v.z]).collect();
        for index in 1..vertices.len().saturating_sub(1) {
            triangles.push([vertices[0], vertices[index], vertices[index + 1]]);
        }
    }
    let index = MeshIndex::new(triangles);
    (Ok(result), inscribed_thickness(&index, OVERLAP_THICKNESS_SAMPLES))
}

/// The thickest point of a closed solid: the diameter of the largest ball
/// found inside it, with its centre. Rays are cast inward from ten points
/// on each sampled face; the ball centred halfway along each ray is as large
/// as the distance to the nearest surface allows. Exact for slabs and boxes
/// (any orientation); a slight underestimate for curved or lens-shaped
/// solids, by the sampling.
pub fn inscribed_thickness(index: &MeshIndex, max_triangles: usize) -> Option<(f64, P)> {
    const WEIGHTS: [[f64; 3]; 10] = [
        [1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0],
        // Edge midpoints: a triangulated rectangle's centre is one.
        [0.5, 0.5, 0.0],
        [0.5, 0.0, 0.5],
        [0.0, 0.5, 0.5],
        [0.8, 0.1, 0.1],
        [0.1, 0.8, 0.1],
        [0.1, 0.1, 0.8],
        [0.45, 0.45, 0.1],
        [0.45, 0.1, 0.45],
        [0.1, 0.45, 0.45],
    ];
    let count = index.triangles.len();
    let stride = count.div_ceil(max_triangles.max(1)).max(1);
    let t_min = (bounds_span(&index.bounds()) * 1e-9).max(1e-9);
    let mut best: Option<(f64, P)> = None;
    for (tested, triangle) in (0..count).step_by(stride).enumerate() {
        if tested % 256 == 255 && csg::bsp::deadline_passed() {
            break;
        }
        let t = &index.triangles[triangle];
        let Some((normal, _)) = unit_normal(t) else {
            continue;
        };
        let inward = mul(normal, -1.0);
        for weight in WEIGHTS {
            let from = add(
                add(mul(t[0], weight[0]), mul(t[1], weight[1])),
                mul(t[2], weight[2]),
            );
            let Some((distance, hit)) =
                index.ray_first_hit(from, inward, t_min, f64::INFINITY, Some(triangle))
            else {
                continue;
            };
            if unit_normal(&index.triangles[hit]).is_none_or(|(n, _)| dot(n, inward) <= 0.0) {
                continue;
            }
            let middle = add(from, mul(inward, distance * 0.5));
            let Some((radius, _)) = index.nearest(middle) else {
                continue;
            };
            let diameter = (2.0 * radius).min(distance);
            if best.is_none_or(|(current, _)| diameter > current) {
                best = Some((diameter, middle));
            }
        }
    }
    best
}

/// `a \ b`.
pub fn boolean_difference(a: &[[P; 3]], b: &[[P; 3]]) -> Result<BooleanResult, BooleanSkipped> {
    guarded_boolean(a, b, csg::Solid::difference)
}

// ---------------------------------------------------------------------------
// Pairwise relationship.
// ---------------------------------------------------------------------------

/// How two closed meshes relate.
#[derive(Clone, Debug, PartialEq)]
pub struct PairReport {
    pub distance: MeshDistance,
    /// One mesh sits wholly inside the other (surfaces never meet).
    pub contained: bool,
    /// Positive-volume overlap. When the exact boolean was skipped this is
    /// decided from containment, a penetrating vertex or a transversal
    /// surface crossing.
    pub intersects: bool,
    /// False when the exact boolean was skipped and the fallback tests ran
    /// out of budget before deciding: `intersects` is then a guess (false)
    /// and must be reported as unknown.
    pub determined: bool,
    pub overlap: Option<Result<BooleanResult, BooleanSkipped>>,
    /// Deepest vertex of either mesh inside the other, measured to the other
    /// surface.
    pub depth: Option<(f64, P)>,
    /// Thickest point of the exact overlap solid (see
    /// [`inscribed_thickness`]) and where it is, when the boolean ran and
    /// found volume.
    pub overlap_thickness: Option<(f64, P)>,
}

impl PairReport {
    /// Where the two overlap, when they do: the exact intersection solid's
    /// bounds when the boolean ran (`true`), else the overlap of the two
    /// meshes' boxes, which is conservative (`false`).
    pub fn overlap_region(&self, a: &MeshIndex, b: &MeshIndex) -> Option<(Aabb, bool)> {
        if !self.intersects {
            return None;
        }
        if let Some(Ok(BooleanResult {
            bounds: Some(bounds),
            ..
        })) = &self.overlap
        {
            return Some((*bounds, true));
        }
        let (ba, bb) = (a.bounds(), b.bounds());
        let min = [0, 1, 2].map(|axis| ba.min[axis].max(bb.min[axis]));
        let max = [0, 1, 2].map(|axis| ba.max[axis].min(bb.max[axis]));
        (0..3)
            .all(|axis| min[axis] <= max[axis])
            .then_some((Aabb { min, max }, false))
    }

    /// How deep the overlap is.
    ///
    /// Preferred: the thickness of the exact overlap solid at its thickest
    /// point (`overlap_thickness`), i.e. how far the surfaces interpenetrate,
    /// measured across the overlap itself: 0.01 mm for a
    /// 0.01 mm over-insertion of a tilted plate, the radial interference of
    /// a press fit. When the exact boolean was skipped (too large) it falls
    /// back to the smaller of the deepest vertex of either mesh inside the
    /// other (`vertex`) and the thinnest axis-aligned extent of the overlap
    /// bounds (`bounds_min_extent`). The bounding-box extent alone misleads
    /// whenever the overlap is not axis-aligned: that tilted plate's overlap
    /// spans 7 mm in every axis, and an annular press fit spans its height.
    /// An estimate of the separation needed, not an exact minimum
    /// translation distance.
    pub fn depth_detail(&self, region: Option<Aabb>) -> Option<DepthEstimate> {
        if !self.intersects {
            return None;
        }
        let bounds_min_extent = region.map(|bounds| {
            (0..3)
                .map(|axis| bounds.max[axis] - bounds.min[axis])
                .fold(f64::INFINITY, f64::min)
                .max(0.0)
        });
        let vertex = self.depth.map(|(depth, _)| depth);
        let thickness = self.overlap_thickness.map(|(thickness, _)| thickness);
        let (depth, method) = match (thickness, vertex, bounds_min_extent) {
            (Some(thickness), _, _) => (thickness, "overlapThickness"),
            (None, Some(vertex), Some(extent)) if extent < vertex => {
                (extent, "overlapBoundsMinExtent")
            }
            (None, Some(vertex), _) => (vertex, "deepestVertex"),
            (None, None, Some(extent)) => (extent, "overlapBoundsMinExtent"),
            (None, None, None) => (0.0, "unknown"),
        };
        Some(DepthEstimate {
            depth,
            method,
            thickness,
            vertex,
            bounds_min_extent,
        })
    }

    /// The point to show: deepest vertex or overlap centre for a clash, the
    /// midpoint of the closest pair for a gap.
    pub fn location(&self, region: Option<Aabb>) -> P {
        if self.intersects {
            if let Some((_, point)) = self.overlap_thickness {
                return point;
            }
            if let Some((_, point)) = self.depth {
                return point;
            }
            if let Some(bounds) = region {
                return [0, 1, 2].map(|axis| (bounds.min[axis] + bounds.max[axis]) / 2.0);
            }
            return self.distance.point_a;
        }
        [0, 1, 2].map(|axis| (self.distance.point_a[axis] + self.distance.point_b[axis]) / 2.0)
    }
}

/// See [`PairReport::depth_detail`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DepthEstimate {
    pub depth: f64,
    /// `"overlapThickness"`, `"deepestVertex"`, `"overlapBoundsMinExtent"`
    /// or `"unknown"`.
    pub method: &'static str,
    pub thickness: Option<f64>,
    pub vertex: Option<f64>,
    pub bounds_min_extent: Option<f64>,
}

/// Outcome of [`containment`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Containment {
    /// A shell of one mesh lies inside the other.
    Contained,
    /// Every shell was probed and none is inside the other.
    NotContained,
    /// The probe budget or the deadline ran out first.
    Unknown,
}

/// Whether any shell of `a` lies inside `b` or any shell of `b` inside `a`,
/// testing one vertex of every shell (not just the first few) within
/// `max_probes` point-in-solid tests.
pub fn containment(a: &MeshIndex, b: &MeshIndex, max_probes: usize) -> Containment {
    let mut probes = 0usize;
    for (inner, outer) in [(a, b), (b, a)] {
        let outer_bounds = outer.bounds();
        for vertex in &inner.representatives {
            // Outside the box is outside the solid: free, not a probe.
            if outer.is_empty() || outer_bounds.point_distance2(*vertex) > 0.0 {
                continue;
            }
            if probes >= max_probes || (probes % 256 == 255 && csg::bsp::deadline_passed()) {
                return Containment::Unknown;
            }
            probes += 1;
            if outer.contains(*vertex) {
                return Containment::Contained;
            }
        }
    }
    Containment::NotContained
}

/// Work limits for [`pair_report_budgeted`].
#[derive(Clone, Copy, Debug)]
pub struct PairBudget {
    pub pair_tests: usize,
    pub containment_probes: usize,
}

impl Default for PairBudget {
    fn default() -> Self {
        Self {
            pair_tests: MAX_PAIR_TESTS,
            containment_probes: MAX_CONTAINMENT_PROBES,
        }
    }
}

/// Relationship between two closed meshes. `epsilon` is the length below
/// which a gap or a penetration is numerical noise (touching faces).
pub fn pair_report(a: &MeshIndex, b: &MeshIndex, epsilon: f64) -> Option<PairReport> {
    pair_report_budgeted(a, b, epsilon, PairBudget::default())
}

pub fn pair_report_budgeted(
    a: &MeshIndex,
    b: &MeshIndex,
    epsilon: f64,
    budget: PairBudget,
) -> Option<PairReport> {
    let distance = mesh_distance_budgeted(a, b, budget.pair_tests)?;
    // Separation is proven only by the lower bound: an incomplete distance
    // search's `distance` is an upper bound and says nothing about clearance.
    let separated = distance.lower_bound > epsilon;
    let containment = if distance.distance > epsilon {
        containment(a, b, budget.containment_probes)
    } else {
        Containment::NotContained
    };
    let contained = containment == Containment::Contained;
    if separated && containment == Containment::NotContained {
        return Some(PairReport {
            distance,
            contained: false,
            intersects: false,
            determined: true,
            overlap: None,
            depth: None,
            overlap_thickness: None,
        });
    }
    // Surfaces meet (or one swallows the other): find how deep.
    let mut depth: Option<(f64, P)> = None;
    let mut probe = |inner: &MeshIndex, outer: &MeshIndex| {
        let outer_bounds = outer.bounds();
        let mut seen = std::collections::HashSet::new();
        for triangle in &inner.triangles {
            for vertex in triangle {
                if outer_bounds.point_distance2(*vertex) > 0.0
                    || !seen.insert(vertex.map(f64::to_bits))
                {
                    continue;
                }
                if seen.len() > 20_000 || (seen.len() % 256 == 0 && csg::bsp::deadline_passed()) {
                    return;
                }
                if let Some((d, _)) = outer.nearest(*vertex) {
                    if d > epsilon
                        && depth.is_none_or(|(best, _)| d > best)
                        && outer.contains(*vertex)
                    {
                        depth = Some((d, *vertex));
                    }
                }
            }
        }
    };
    probe(a, b);
    probe(b, a);
    let (overlap, overlap_thickness) = boolean_overlap_measured(&a.triangles, &b.triangles);
    let volume_scale = epsilon.max(1e-9).powi(2) * 10.0;
    let (intersects, determined) = match &overlap {
        Ok(result) => (result.volume > volume_scale, true),
        Err(_) if contained || depth.is_some() => (true, true),
        Err(_) => match find_crossing(a, b) {
            Crossing::Found(_) => (true, true),
            // No crossing, but an unprobed shell could still be buried in
            // the other mesh.
            Crossing::None => (false, containment != Containment::Unknown),
            Crossing::Unknown => (false, false),
        },
    };
    Some(PairReport {
        distance,
        contained,
        intersects,
        determined,
        overlap: Some(overlap),
        depth,
        overlap_thickness,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 10 x 10 x `thickness` plate rotated 45 degrees about y (normal
    /// (1, 0, 1)/sqrt 2), centred on (5, 5, 5): an oblique thin wall.
    fn slanted_slab(thickness: f64) -> Vec<[P; 3]> {
        let (s, c) = std::f64::consts::FRAC_PI_4.sin_cos();
        box_triangles([-5.0, -5.0, -thickness / 2.0], [5.0, 5.0, thickness / 2.0])
            .into_iter()
            .map(|t| {
                t.map(|[x, y, z]| [x * c + z * s + 5.0, y + 5.0, -x * s + z * c + 5.0])
            })
            .collect()
    }

    /// A prism whose cross-section (x-z) is a triangle with a `degrees`
    /// tip at the origin, 20 mm long, 10 mm deep along y, closed by a flat
    /// back at x = 20.
    fn knife(degrees: f64) -> Vec<[P; 3]> {
        let half = (degrees / 2.0).to_radians().tan() * 20.0;
        let outline = [[0.0, 0.0], [20.0, -half], [20.0, half]];
        let at = |c: [f64; 2], y: f64| [c[0], y, c[1]];
        let mut triangles = Vec::new();
        for i in 0..3 {
            let (a, b) = (outline[i], outline[(i + 1) % 3]);
            triangles.push([at(a, 0.0), at(b, 10.0), at(b, 0.0)]);
            triangles.push([at(a, 0.0), at(a, 10.0), at(b, 10.0)]);
        }
        triangles.push([at(outline[0], 0.0), at(outline[1], 0.0), at(outline[2], 0.0)]);
        triangles.push([at(outline[0], 10.0), at(outline[2], 10.0), at(outline[1], 10.0)]);
        let measured = measure_triangles(&triangles);
        assert!(measured.watertight() && measured.winding_consistent() && measured.volume > 0.0, "{measured:?}");
        triangles
    }

    /// A block (x in -5..15, y and z in 0..10) with a 90-degree V-groove
    /// along y cut down from the top and another up from the bottom, both at
    /// x = 5, their tips `gap` apart around z = 5.
    fn double_groove(gap: f64) -> Vec<[P; 3]> {
        let (top, bottom) = (5.0 + gap / 2.0, 5.0 - gap / 2.0);
        // The x-z outline, counter-clockwise, extruded along y.
        let outline = [
            [-5.0, 0.0],
            [5.0 - bottom, 0.0],
            [5.0, bottom],
            [5.0 + bottom, 0.0],
            [15.0, 0.0],
            [15.0, 10.0],
            [5.0 + (10.0 - top), 10.0],
            [5.0, top],
            [5.0 - (10.0 - top), 10.0],
            [-5.0, 10.0],
        ];
        let at = |c: [f64; 2], y: f64| [c[0], y, c[1]];
        let mut triangles = Vec::new();
        for i in 0..outline.len() {
            let (a, b) = (outline[i], outline[(i + 1) % outline.len()]);
            // The outline runs counter-clockwise in (x, z), so a side's
            // outward normal is (dz, 0, -dx).
            triangles.push([at(a, 0.0), at(b, 10.0), at(b, 0.0)]);
            triangles.push([at(a, 0.0), at(a, 10.0), at(b, 10.0)]);
        }
        // Caps: fan from the centre point, which sees every outline vertex.
        let centre = [5.0, 5.0];
        for i in 0..outline.len() {
            let (a, b) = (outline[i], outline[(i + 1) % outline.len()]);
            triangles.push([at(centre, 0.0), at(a, 0.0), at(b, 0.0)]);
            triangles.push([at(centre, 10.0), at(b, 10.0), at(a, 10.0)]);
        }
        triangles
    }

    fn box_triangles(min: P, max: P) -> Vec<[P; 3]> {
        let corner = |i: usize| -> P {
            [
                if i & 1 == 0 { min[0] } else { max[0] },
                if i & 2 == 0 { min[1] } else { max[1] },
                if i & 4 == 0 { min[2] } else { max[2] },
            ]
        };
        // Outward-facing quads as corner indices.
        let quads = [
            [0, 2, 3, 1],
            [4, 5, 7, 6],
            [0, 1, 5, 4],
            [2, 6, 7, 3],
            [0, 4, 6, 2],
            [1, 3, 7, 5],
        ];
        let mut triangles = Vec::new();
        for quad in quads {
            triangles.push([corner(quad[0]), corner(quad[1]), corner(quad[2])]);
            triangles.push([corner(quad[0]), corner(quad[2]), corner(quad[3])]);
        }
        triangles
    }

    #[test]
    fn box_measurements_are_exact() {
        let measure = measure_triangles(&box_triangles([0.0; 3], [2.0, 3.0, 4.0]));
        assert!((measure.volume - 24.0).abs() < 1e-9);
        assert!((measure.surface_area - 52.0).abs() < 1e-9);
        assert_eq!(measure.components, 1);
        assert!(measure.watertight());
        assert!(measure.winding_consistent());
        assert_eq!(measure.bounds.max, [2.0, 3.0, 4.0]);
    }

    #[test]
    fn flipped_face_breaks_winding() {
        let mut triangles = box_triangles([0.0; 3], [1.0; 3]);
        triangles[0].swap(1, 2);
        let measure = measure_triangles(&triangles);
        assert!(!measure.winding_consistent());
        assert!(measure.watertight());
    }

    #[test]
    fn closed_box_has_no_defects() {
        let measure = measure_triangles(&box_triangles([0.0; 3], [1.0; 3]));
        assert!(measure.defects.is_empty());
        assert_eq!(measure.defect_clusters, 0);
    }

    #[test]
    fn a_measurement_past_the_deadline_is_marked_incomplete() {
        // A big open grid (many boundary edges to cluster) under a deadline
        // that has already passed: the census stops instead of finishing.
        let mut triangles = Vec::new();
        for i in 0..200 {
            for j in 0..200 {
                let (x, y) = (i as f64, j as f64);
                triangles.push([[x, y, 0.0], [x + 1.0, y, 0.0], [x, y + 1.0, 0.0]]);
            }
        }
        let healthy = measure_triangles(&triangles);
        assert!(healthy.complete);
        assert!(!healthy.defects.is_empty());
        let _deadline = csg::bsp::deadline_scope(std::time::Instant::now());
        let cut = measure_triangles(&triangles);
        assert!(!cut.complete);
        assert!(!cut.watertight() && !cut.manifold() && !cut.winding_consistent(), "unknown is never reported good");
        assert_eq!(cut.bounds, healthy.bounds, "the cheap sums are still exact");
    }

    #[test]
    fn defect_clustering_stops_when_interrupted() {
        let vertices: Vec<csg::Vec3> = (0..20_000).map(|i| csg::Vec3::new(i as f64, 0.0, 0.0)).collect();
        let edges: Vec<(u32, u32)> = (0..19_999u32).map(|i| (i, i + 1)).collect();
        let mut poll = InterruptPoll::new();
        assert_eq!(defect_clusters("boundary", &edges, &vertices, &mut poll).len(), 1);
        let _deadline = csg::bsp::deadline_scope(std::time::Instant::now());
        let mut poll = InterruptPoll::new();
        assert!(defect_clusters("boundary", &edges, &vertices, &mut poll).is_empty());
        assert!(poll.interrupted);
    }

    #[test]
    fn missing_face_is_one_located_boundary_cluster() {
        // Triangles 0 and 1 are the z = 0 face.
        let triangles = box_triangles([0.0; 3], [2.0, 4.0, 6.0])[2..].to_vec();
        let measure = measure_triangles(&triangles);
        assert_eq!(measure.boundary_edges, 4);
        assert_eq!(measure.defects.len(), 1);
        let cluster = &measure.defects[0];
        assert_eq!(cluster.kind, "boundary");
        assert_eq!(cluster.edges, 4);
        assert_eq!(cluster.centroid, [1.0, 2.0, 0.0]);
        assert_eq!(cluster.bounds.min, [0.0, 0.0, 0.0]);
        assert_eq!(cluster.bounds.max, [2.0, 4.0, 0.0]);
        assert_eq!(cluster.samples.len(), DEFECT_SAMPLE_EDGES);
        for [a, b] in &cluster.samples {
            assert_eq!((a[2], b[2]), (0.0, 0.0));
        }
    }

    #[test]
    fn two_holes_are_two_clusters() {
        // Drop the bottom (0, 1) and top (2, 3) faces: two separate rings.
        let mut triangles = box_triangles([0.0; 3], [1.0; 3]);
        triangles.drain(0..4);
        let measure = measure_triangles(&triangles);
        assert_eq!(measure.defects.len(), 2);
        let mut heights: Vec<f64> = measure.defects.iter().map(|c| c.centroid[2]).collect();
        heights.sort_by(f64::total_cmp);
        assert_eq!(heights, vec![0.0, 1.0]);
        assert!(measure.defects.iter().all(|c| c.kind == "boundary" && c.edges == 4));
        // Holes in two far-apart boxes: also two, each at its own box.
        let mut apart = box_triangles([0.0; 3], [1.0; 3])[2..].to_vec();
        apart.extend(box_triangles([10.0; 3], [11.0; 3])[2..].to_vec());
        let centroids: Vec<P> = measure_triangles(&apart).defects.iter().map(|c| c.centroid).collect();
        assert_eq!(centroids, vec![[0.5, 0.5, 0.0], [10.5, 10.5, 10.0]]);
    }

    #[test]
    fn fin_is_a_non_manifold_cluster_and_clusters_are_capped() {
        let mut triangles = box_triangles([0.0; 3], [1.0; 3]);
        // A third triangle on the edge (0,0,0)-(1,0,0).
        triangles.push([[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.5, -1.0, 0.0]]);
        let measure = measure_triangles(&triangles);
        assert!(measure.defects.iter().any(|c| c.kind == "nonManifold" && c.edges == 1));
        // Fifteen open boxes: fifteen clusters, ten listed.
        let mut many = Vec::new();
        for index in 0..15 {
            let x = index as f64 * 3.0;
            many.extend(box_triangles([x, 0.0, 0.0], [x + 1.0, 1.0, 1.0])[2..].to_vec());
        }
        let measure = measure_triangles(&many);
        assert_eq!(measure.defect_clusters, 15);
        assert_eq!(measure.defects.len(), MAX_DEFECT_CLUSTERS);
    }

    #[test]
    fn two_shells_are_two_components() {
        let mut triangles = box_triangles([0.0; 3], [1.0; 3]);
        triangles.extend(box_triangles([3.0; 3], [4.0; 3]));
        assert_eq!(measure_triangles(&triangles).components, 2);
    }

    #[test]
    fn distance_between_boxes_is_exact() {
        let a = MeshIndex::new(box_triangles([0.0; 3], [10.0; 3]));
        let b = MeshIndex::new(box_triangles([10.35, 0.0, 0.0], [20.0, 10.0, 10.0]));
        let d = mesh_distance(&a, &b).unwrap();
        assert!((d.distance - 0.35).abs() < 1e-12, "{}", d.distance);
        assert!(d.exact);
    }

    #[test]
    fn edge_to_edge_distance_is_found() {
        // Two boxes approaching corner-edge to corner-edge diagonally: the
        // closest pair is an edge pair with no vertex at it.
        let a = MeshIndex::new(box_triangles([0.0; 3], [1.0; 3]));
        let b = MeshIndex::new(box_triangles([2.0, 2.0, -5.0], [3.0, 3.0, 5.0]));
        let d = mesh_distance(&a, &b).unwrap();
        assert!((d.distance - 2f64.sqrt()).abs() < 1e-12);
    }

    #[test]
    fn containment_and_overlap() {
        let a = MeshIndex::new(box_triangles([0.0; 3], [10.0; 3]));
        let inner = MeshIndex::new(box_triangles([2.0; 3], [4.0; 3]));
        assert!(a.contains([5.0; 3]));
        assert!(!a.contains([11.0, 5.0, 5.0]));
        let report = pair_report(&a, &inner, 1e-6).unwrap();
        assert!(report.contained && report.intersects);
        let overlap = report.overlap.unwrap().unwrap();
        assert!((overlap.volume - 8.0).abs() < 1e-6);

        let shifted = MeshIndex::new(box_triangles([9.0, 0.0, 0.0], [12.0, 10.0, 10.0]));
        let report = pair_report(&a, &shifted, 1e-6).unwrap();
        assert!(report.intersects);
        let overlap = report.overlap.unwrap().unwrap();
        assert!((overlap.volume - 100.0).abs() < 1e-6, "{}", overlap.volume);
        let bounds = overlap.bounds.unwrap();
        assert!((bounds.min[0] - 9.0).abs() < 1e-9 && (bounds.max[0] - 10.0).abs() < 1e-9);
        // Every vertex of `shifted` lies on a face of `a`, so no vertex
        // penetrates; the overlap bounds carry the 1 mm thickness instead.
        assert!(report.depth.is_none());
    }

    #[test]
    fn crossing_without_penetrating_vertices_is_found() {
        // A long bar through a thin slab: no vertex of either is inside the
        // other, only edges cross.
        let bar = MeshIndex::new(box_triangles([-10.0, 4.0, 4.0], [20.0, 6.0, 6.0]));
        let slab = MeshIndex::new(box_triangles([4.0, 0.0, 0.0], [6.0, 10.0, 10.0]));
        assert!(matches!(find_crossing(&bar, &slab), Crossing::Found(_)));
        let apart = MeshIndex::new(box_triangles([7.0, 0.0, 0.0], [9.0, 10.0, 10.0]));
        assert_eq!(find_crossing(&slab, &apart), Crossing::None);
        let touching = MeshIndex::new(box_triangles([6.0, 0.0, 0.0], [9.0, 10.0, 10.0]));
        assert_eq!(find_crossing(&slab, &touching), Crossing::None);
    }

    #[test]
    fn a_buried_second_shell_counts_as_containment() {
        let mut pieces = box_triangles([20.0; 3], [21.0; 3]);
        pieces.extend(box_triangles([2.0; 3], [3.0; 3]));
        let outer = MeshIndex::new(box_triangles([0.0; 3], [10.0; 3]));
        let report = pair_report(&MeshIndex::new(pieces), &outer, 1e-6).unwrap();
        assert!(report.contained && report.intersects);
    }

    #[test]
    fn touching_faces_do_not_intersect() {
        let a = MeshIndex::new(box_triangles([0.0; 3], [1.0; 3]));
        let b = MeshIndex::new(box_triangles([1.0, 0.0, 0.0], [2.0, 1.0, 1.0]));
        let report = pair_report(&a, &b, 1e-6).unwrap();
        assert_eq!(report.distance.distance, 0.0);
        assert!(!report.intersects);
    }

    #[test]
    fn an_incomplete_distance_search_never_proves_separation() {
        // A bar through a slab: they overlap, but the seed pair (the two
        // bottom faces) is 4 mm apart. A search stopped at once used to
        // report that 4 mm as a determined, intersection-free gap.
        let bar = MeshIndex::new(box_triangles([-10.0, 4.0, 4.0], [20.0, 6.0, 6.0]));
        let slab = MeshIndex::new(box_triangles([4.0, 0.0, 0.0], [6.0, 10.0, 10.0]));
        let distance = mesh_distance_budgeted(&bar, &slab, 0).unwrap();
        assert!(!distance.exact);
        assert!(distance.distance > 1e-6);
        assert_eq!(distance.lower_bound, 0.0);
        let budget = PairBudget {
            pair_tests: 0,
            ..PairBudget::default()
        };
        let report = pair_report_budgeted(&bar, &slab, 1e-6, budget).unwrap();
        assert!(report.intersects && report.determined, "{report:?}");

        // A real gap: the lower bound is a proof of clearance on its own.
        let a = MeshIndex::new(box_triangles([0.0; 3], [1.0; 3]));
        // Bottom faces at different heights, so the seed pair is not the
        // closest one and the search has work left when it is stopped.
        let b = MeshIndex::new(box_triangles([5.0, 0.0, 0.5], [6.0, 1.0, 10.0]));
        let distance = mesh_distance_budgeted(&a, &b, 0).unwrap();
        assert!(!distance.exact, "{distance:?}");
        assert!(distance.lower_bound > 1e-6 && distance.lower_bound <= 4.0 + 1e-12);
        assert!(distance.distance > 4.0);
        let report = pair_report_budgeted(&a, &b, 1e-6, budget).unwrap();
        assert!(!report.intersects && report.determined);
        // Complete searches carry lower_bound == distance.
        let exact = mesh_distance(&a, &b).unwrap();
        assert!(exact.exact && exact.lower_bound == exact.distance);
    }

    #[test]
    fn containment_checks_every_shell_not_the_first_64() {
        // 100 small shells outside the outer box, then one buried inside it.
        let mut pieces = Vec::new();
        for index in 0..100 {
            let x = 20.0 + 2.0 * index as f64;
            pieces.extend(box_triangles([x, 0.0, 0.0], [x + 1.0, 1.0, 1.0]));
        }
        pieces.extend(box_triangles([2.0; 3], [3.0; 3]));
        let pieces = MeshIndex::new(pieces);
        let outer = MeshIndex::new(box_triangles([0.0; 3], [10.0; 3]));
        assert_eq!(pieces.representatives.len(), 101);
        assert_eq!(containment(&pieces, &outer, MAX_CONTAINMENT_PROBES), Containment::Contained);
        let report = pair_report(&pieces, &outer, 1e-6).unwrap();
        assert!(report.contained && report.intersects && report.determined);
        // Out of probes: unknown, not "not contained".
        assert_eq!(containment(&pieces, &outer, 0), Containment::Unknown);
    }

    /// A closed cylinder of `segments` facets around Z, outward-wound.
    fn cylinder_triangles(radius: f64, height: f64, segments: usize) -> Vec<[P; 3]> {
        let at = |i: usize, z: f64| {
            let angle = std::f64::consts::TAU * (i % segments) as f64 / segments as f64;
            [radius * angle.cos(), radius * angle.sin(), z]
        };
        let mut triangles = Vec::new();
        for i in 0..segments {
            let (a, b) = (at(i, 0.0), at(i + 1, 0.0));
            let (c, d) = (at(i, height), at(i + 1, height));
            triangles.push([a, b, d]);
            triangles.push([a, d, c]);
            triangles.push([[0.0, 0.0, 0.0], b, a]);
            triangles.push([[0.0, 0.0, height], c, d]);
        }
        triangles
    }

    #[test]
    fn thin_spots_come_back_as_located_regions() {
        // A thick block and two separate 0.4 mm plates: two thin regions,
        // each with its own location and area, the block not among them.
        let mut mesh = box_triangles([0.0; 3], [10.0; 3]);
        mesh.extend(box_triangles([20.0, 0.0, 0.0], [30.0, 10.0, 0.4]));
        mesh.extend(box_triangles([60.0, 0.0, 0.0], [65.0, 10.0, 0.4]));
        let check = thickness_check(&MeshIndex::new(mesh), 0.8, MAX_THICKNESS_SAMPLES);
        assert_eq!(check.region_count, 2, "{:?}", check.regions);
        let mut regions = check.regions.clone();
        regions.sort_by(|a, b| a.bounds.min[0].total_cmp(&b.bounds.min[0]));
        for (region, (low, high, faces, total)) in regions.iter().zip([(20.0, 30.0, 200.0, 216.0), (60.0, 65.0, 100.0, 112.0)]) {
            assert!((region.min.thickness - 0.4).abs() < 1e-3, "{region:?}");
            assert!(region.bounds.min[0] >= low - 1e-9 && region.bounds.max[0] <= high + 1e-9, "{region:?}");
            // Top and bottom at least; the 0.4 mm rims may count too.
            assert!(region.area >= faces * 0.95 && region.area <= total * 1.01, "{region:?}");
        }
        assert!(check.refined_samples > 0);
        // A plain block has none.
        let block = thickness_check(&MeshIndex::new(box_triangles([0.0; 3], [10.0; 3])), 0.8, MAX_THICKNESS_SAMPLES);
        assert_eq!((block.region_count, block.below_threshold), (0, 0));
    }

    /// Region grouping stays linear when one patch holds a dense crowd of
    /// thin samples (it compared every pair of neighbouring cells before).
    #[test]
    fn dense_thin_patches_group_quickly() {
        let spot = |x: f64, y: f64| ThinSpot { thickness: 0.1, from: [x, y, 0.0], to: [x, y, 0.1], method: "ray" };
        let mut measured = Vec::new();
        for i in 0..400 {
            for j in 0..500 {
                measured.push(Measured { spot: spot(i as f64 * 0.01, j as f64 * 0.01), area: 1e-4 });
            }
        }
        // A second patch well away.
        measured.push(Measured { spot: spot(50.0, 50.0), area: 1e-4 });
        let started = std::time::Instant::now();
        let (regions, complete) = thin_regions(&measured, 0.8, 0.01);
        assert!(started.elapsed() < std::time::Duration::from_secs(3), "{:?}", started.elapsed());
        assert!(complete);
        assert_eq!(regions.len(), 2, "{}", regions.len());
        assert_eq!(regions[0].samples + regions[1].samples, 200_001);
        // Past the deadline the grouping stops and says so.
        let _scope = csg::bsp::deadline_scope(std::time::Instant::now());
        let (_, complete) = thin_regions(&measured, 0.8, 0.01);
        assert!(!complete);
    }

    /// A triangle counts as measured only when every one of its samples was,
    /// and a cut-off scan never reports itself complete.
    #[test]
    fn interrupted_scans_are_never_complete() {
        let sample = |slot: usize| WallSample { point: [0.0; 3], inward: [0.0, 0.0, -1.0], triangle: 0, slot, area: 0.0 };
        let samples = [sample(0), sample(0), sample(1), sample(usize::MAX)];
        assert_eq!(unreached_slots(&samples, &[true, false, true, true], 2), (1, false));
        assert_eq!(unreached_slots(&samples, &[true, true, true, false], 2), (0, true));
        assert_eq!(unreached_slots(&samples, &[true; 4], 2), (0, false));
        let index = MeshIndex::new(box_triangles([0.0; 3], [20.0, 20.0, 0.5]));
        let _scope = csg::bsp::deadline_scope(std::time::Instant::now());
        let check = thickness_check(&index, 0.8, MAX_THICKNESS_SAMPLES);
        assert!(!check.complete(), "{check:?}");
    }

    /// The case that made the scan slow: a finely tessellated curved
    /// surface, where every facet edge pinched the ball and every facet got
    /// seven samples. Now the minimum is still the diameter, the scan is
    /// complete, and it takes a fraction of a second.
    #[test]
    fn curved_surfaces_are_fast_and_accurate() {
        let index = MeshIndex::new(cylinder_triangles(5.0, 20.0, 128));
        let started = std::time::Instant::now();
        let check = thickness_check(&index, 0.8, MAX_THICKNESS_SAMPLES);
        let elapsed = started.elapsed();
        assert_eq!(check.skipped, 0);
        let min = check.min.unwrap().thickness;
        // The inscribed diameter of a 128-gon of radius 5 is 9.994.
        assert!((min - 10.0).abs() < 0.05, "{min}");
        assert!(elapsed < std::time::Duration::from_secs(3), "{elapsed:?}");
    }

    #[test]
    fn thickness_overhang_and_sharp_edges() {
        let plate = MeshIndex::new(box_triangles([0.0; 3], [20.0, 20.0, 0.5]));
        let check = thickness_check(&plate, 0.8, MAX_THICKNESS_SAMPLES);
        assert!((check.min.unwrap().thickness - 0.5).abs() < 1e-3, "{:?}", check.min);
        assert_eq!(check.skipped, 0);
        assert!(check.samples > 12, "{}", check.samples);
        // Top and bottom (800 mm^2 of the 840) are thin; the 0.5 mm sides
        // are measured across the plate (20 mm), not as thin.
        assert!((check.below_threshold_area - 800.0).abs() < 40.0, "{}", check.below_threshold_area);
        // Sampling caps the triangles and says so.
        let sampled = thickness_check(&plate, 0.8, 4);
        assert_eq!(sampled.skipped, 8);

        // A block with no thin wall: box edges and corners pinch the
        // inscribed ball, which must not read as a thin spot.
        let block = MeshIndex::new(box_triangles([0.0; 3], [10.0; 3]));
        let check = thickness_check(&block, 0.8, MAX_THICKNESS_SAMPLES);
        assert!(check.min.unwrap().thickness > 9.0, "{:?}", check.min);
        assert_eq!(check.below_threshold, 0);

        // A thin oblique wall: a 45-degree slab 0.4 mm thick, whose faces
        // are big triangles. The probe finds it too.
        let slab = MeshIndex::new(slanted_slab(0.4));
        let check = thickness_check(&slab, 0.8, MAX_THICKNESS_SAMPLES);
        assert!((check.min.unwrap().thickness - 0.4).abs() < 1e-3, "{:?}", check.min);
        let probe = thickness_probe(&slab, [5.0, 5.0, 5.0], 2.0, 0.8);
        assert!(probe.triangles > 0 && probe.samples > 100, "{probe:?}");
        assert!((probe.min.unwrap().thickness - 0.4).abs() < 1e-3);
        // Two 90-degree V-grooves, one cut from the top and one from the
        // bottom, tip to tip 0.6 mm apart: no face normal crosses the
        // ligament (every inward ray runs parallel to a face of the other
        // groove); the samples along the groove roots find it.
        let groove_mesh = double_groove(0.6);
        let measured = measure_triangles(&groove_mesh);
        assert!(measured.watertight() && measured.winding_consistent() && measured.volume > 0.0, "{measured:?}");
        let grooves = MeshIndex::new(groove_mesh);
        let check = thickness_check(&grooves, 0.8, MAX_THICKNESS_SAMPLES);
        let min = check.min.unwrap();
        assert!((min.thickness - 0.6).abs() < 1e-3, "{min:?}");
        assert!(check.below_threshold > 0);
        let probe = thickness_probe(&grooves, [5.0, 5.0, 5.0], 1.0, 0.8);
        assert!((probe.min.unwrap().thickness - 0.6).abs() < 0.05, "{:?}", probe.min);
        // A small probe on a huge two-triangle face still samples densely.
        let sheet = MeshIndex::new(box_triangles([0.0; 3], [1000.0, 1000.0, 0.5]));
        let probe = thickness_probe(&sheet, [1.0, 1.0, 0.5], 0.1, 0.8);
        assert!(probe.samples > 100, "{probe:?}");
        assert!((probe.min.unwrap().thickness - 0.5).abs() < 1e-3);

        // A 5-degree knife edge: its tip is a sharp edge, not a 0.0x mm wall.
        let wedge = MeshIndex::new(knife(5.0));
        let check = thickness_check(&wedge, 0.8, MAX_THICKNESS_SAMPLES);
        assert!(check.sharp_tips > 0, "{check:?}");
        assert!(check.min.unwrap().thickness > 0.2, "{:?}", check.min);
        let away = thickness_probe(&slab, [100.0, 100.0, 100.0], 1.0, 0.8);
        assert_eq!((away.triangles, away.samples), (0, 0));

        // A T: a post on the bed carrying a slab whose underside overhangs.
        let mut tee = box_triangles([4.0, 4.0, 0.0], [6.0, 6.0, 10.0]);
        tee.extend(box_triangles([0.0, 0.0, 10.0], [10.0, 10.0, 12.0]));
        let tee = MeshIndex::new(tee);
        let check = overhang_check(&tee, 45.0, None);
        assert!((check.area - 100.0).abs() < 1e-9, "{check:?}");
        assert!((check.bed_area - 4.0).abs() < 1e-9);
        assert_eq!(check.regions, 1);
        assert_eq!(check.bounds.unwrap().min[2], 10.0);
        // 90 degrees from vertical: nothing needs support.
        assert_eq!(overhang_check(&tee, 90.0, None).triangles, 0);
        // The same T floating 5 mm above an explicit bed at z = -5: the
        // post's foot is no longer on the bed and needs support too.
        let floating = overhang_check(&tee, 45.0, Some(-5.0));
        assert!((floating.area - 104.0).abs() < 1e-9, "{floating:?}");
        assert_eq!(floating.bed_area, 0.0);

        let cube = box_triangles([0.0; 3], [2.0; 3]);
        let sharp = sharp_edge_check(&cube, 90.0);
        assert_eq!((sharp.edges, sharp.creases), (12, 1));
        assert!((sharp.length - 24.0).abs() < 1e-9);
        assert!((sharp.longest[0].min_angle - 90.0).abs() < 1e-9);
        assert_eq!(sharp_edge_check(&cube, 89.0).edges, 0);
    }

    #[test]
    fn clearance_clustering_with_a_huge_tolerance_stays_linear() {
        // Two 2 mm cubes 1 mm apart checked against a 50 mm tolerance:
        // every sample of both is a hit, and they all land in one or two
        // cells, which used to cost ~10^9 pairwise comparisons.
        let a = MeshIndex::new(box_triangles([0.0; 3], [2.0; 3]));
        let b = MeshIndex::new(box_triangles([3.0, 0.0, 0.0], [5.0, 2.0, 2.0]));
        let started = std::time::Instant::now();
        let found = clearance_regions(&a, &b, 50.0, CLEARANCE_SAMPLES, None);
        let elapsed = started.elapsed();
        eprintln!("huge-tolerance clearance: {elapsed:?}, {} samples", found.samples);
        assert!(found.samples > 10_000, "{}", found.samples);
        assert_eq!(found.total_regions, 1);
        assert!(!found.truncated);
        // Generous: a loaded machine is slow, the quadratic case took minutes.
        assert!(cfg!(debug_assertions) || elapsed.as_secs_f64() < 10.0, "took {elapsed:?}");

        // A deadline already passed is honoured and reported.
        let late = clearance_regions(&a, &b, 50.0, CLEARANCE_SAMPLES, Some(std::time::Instant::now()));
        assert!(late.truncated);
    }
}
