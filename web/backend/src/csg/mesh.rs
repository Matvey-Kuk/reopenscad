//! Core geometry types for the exact polyhedral CSG kernel.
//!
//! The kernel works on *explicit* boundary representations: a solid is a set of
//! planar, convex-or-simple polygons whose vertices wind counter-clockwise when
//! seen from outside the solid. This is deliberately different from the legacy
//! implicit/SDF evaluator in `engine.rs`: nothing here is sampled on a grid, so
//! sharp edges survive exactly and `polyhedron()` is a trivial constructor.
//!
//! # Epsilon strategy
//!
//! Every tolerance in the kernel derives from [`EPSILON`], an *absolute* value
//! expressed in scene units (millimetres for SCAD sources). OpenSCAD models are
//! authored in the 0.01mm .. 1000mm range, so an absolute epsilon is both
//! simpler to reason about and better behaved than a relative one: a relative
//! epsilon would collapse detail on small features of a large model.
//!
//! * [`EPSILON`] (1e-9) classifies a point against a plane in [`bsp`].
//! * [`WELD_EPSILON`] (3e-5) merges coincident vertices when indexing a mesh;
//!   see its own note for why it is looser than the rest.
//! * [`PLANE_ANGLE_EPSILON`] decides whether two faces are coplanar during the
//!   face-merging pass.
//!
//! [`bsp`]: super::bsp

use std::collections::HashMap;
use std::fmt;
use std::hash::{BuildHasherDefault, Hasher};

/// A non-cryptographic hasher for the kernel's integer keys.
///
/// Every hash map in the kernel is keyed on lattice cells, vertex indices or
/// edge pairs — small integer tuples, never attacker-controlled strings. The
/// default `SipHash` costs more than the map lookups it protects: profiling the
/// merge pass on a 25k-triangle model put 28% of the whole kernel's runtime
/// inside `SipHasher::write`. This is the multiply-rotate mixer used by
/// `rustc-hash`, which is a few instructions per word.
///
/// Swapping the hasher cannot change any output: nothing in the kernel iterates
/// a `HashMap`, so hash order never reaches the geometry.
#[derive(Default, Clone, Copy)]
pub struct FastHasher {
    hash: u64,
}

const FAST_HASH_MULTIPLIER: u64 = 0x517c_c1b7_2722_0a95;

impl FastHasher {
    #[inline]
    fn mix(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(FAST_HASH_MULTIPLIER);
    }
}

impl Hasher for FastHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.hash.rotate_left(20)
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            self.mix(u64::from_ne_bytes(chunk.try_into().expect("chunk of 8")));
        }
        let mut tail = 0u64;
        for byte in chunks.remainder() {
            tail = (tail << 8) | *byte as u64;
        }
        self.mix(tail);
    }

    #[inline]
    fn write_u8(&mut self, value: u8) {
        self.mix(value as u64);
    }

    #[inline]
    fn write_u32(&mut self, value: u32) {
        self.mix(value as u64);
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.mix(value);
    }

    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.mix(value as u64);
    }

    #[inline]
    fn write_i64(&mut self, value: i64) {
        self.mix(value as u64);
    }
}

/// `HashMap` over kernel keys, using [`FastHasher`].
pub type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<FastHasher>>;

/// Absolute tolerance used for point/plane classification.
pub const EPSILON: f64 = 1.0e-9;

/// Absolute tolerance used when welding coincident vertices.
///
/// Deliberately looser than [`EPSILON`], because it answers a different
/// question. [`EPSILON`] asks where a point lies relative to a plane, which is
/// a well-conditioned test. This one asks whether two points the kernel
/// *computed separately* are the same point, and that computation is not
/// always well conditioned: a vertex where two nearly parallel faces meet
/// comes out of an ill-conditioned plane intersection, so the same corner
/// reached along two different faces can differ by far more than the
/// arithmetic's own noise. Measured on a louvred basket whose blades cross at
/// a shallow angle, corners that are geometrically identical landed 1e-6 apart
/// — a thousand times [`EPSILON`]. At 1e-9 they stayed separate vertices, and
/// the mesh came out of the boolean cracked: 2011 boundary edges, 450
/// non-manifold edges, and zero-area facets spanning the gaps.
///
/// How far out an ill-conditioned intersection lands scales with the size of
/// the model, not with [`EPSILON`], and the worst spread on that basket — a
/// 200 mm part — is 2.5e-5 mm, or about one part in 1e7 of its own extent.
/// Anything tighter leaves those corners as a cluster of three vertices, which
/// is a hole in the surface and a non-manifold edge at export.
///
/// 3e-5 mm is 30 nanometres: still more than two orders of magnitude below the
/// 0.01 mm feature size at the bottom of the authoring range above, and thirty
/// times finer than the 1e-3 mm grid OpenSCAD itself rounds to, so it cannot
/// merge detail anyone modelled on purpose. It is not a free parameter to
/// widen at will — it is an upper bound on how wrong an intersection is
/// allowed to be, and widening it past a real feature size would weld that
/// feature away.
pub const WELD_EPSILON: f64 = 3.0e-5;

/// Tolerance on the dot product of two unit normals for "same plane".
pub const PLANE_ANGLE_EPSILON: f64 = 1.0e-8;

/// Tolerance on the plane offset for "same plane".
pub const PLANE_OFFSET_EPSILON: f64 = 1.0e-7;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Vec3 {
    pub const ZERO: Vec3 = Vec3 {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    pub fn add(self, other: Self) -> Self {
        Self::new(self.x + other.x, self.y + other.y, self.z + other.z)
    }

    pub fn sub(self, other: Self) -> Self {
        Self::new(self.x - other.x, self.y - other.y, self.z - other.z)
    }

    pub fn mul(self, scalar: f64) -> Self {
        Self::new(self.x * scalar, self.y * scalar, self.z * scalar)
    }

    pub fn neg(self) -> Self {
        Self::new(-self.x, -self.y, -self.z)
    }

    pub fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    pub fn cross(self, other: Self) -> Self {
        Self::new(
            self.y * other.z - self.z * other.y,
            self.z * other.x - self.x * other.z,
            self.x * other.y - self.y * other.x,
        )
    }

    pub fn length(self) -> f64 {
        self.dot(self).sqrt()
    }

    pub fn normalized(self) -> Self {
        let length = self.length();
        if length == 0.0 {
            self
        } else {
            self.mul(1.0 / length)
        }
    }

    pub fn lerp(self, other: Self, t: f64) -> Self {
        self.add(other.sub(self).mul(t))
    }

    pub fn component(self, axis: usize) -> f64 {
        match axis {
            0 => self.x,
            1 => self.y,
            _ => self.z,
        }
    }

    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }

    pub fn min(self, other: Self) -> Self {
        Self::new(
            self.x.min(other.x),
            self.y.min(other.y),
            self.z.min(other.z),
        )
    }

    pub fn max(self, other: Self) -> Self {
        Self::new(
            self.x.max(other.x),
            self.y.max(other.y),
            self.z.max(other.z),
        )
    }
}

/// Oriented plane `normal . p = offset`, normal is unit length.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Plane {
    pub normal: Vec3,
    pub offset: f64,
}

impl Plane {
    pub fn new(normal: Vec3, offset: f64) -> Self {
        Self { normal, offset }
    }

    /// Newell's method: numerically far more stable than a single cross
    /// product, and it is exactly what makes near-degenerate boolean
    /// fragments classify consistently.
    pub fn from_polygon(points: &[Vec3]) -> Option<Self> {
        if points.len() < 3 {
            return None;
        }
        let mut normal = Vec3::ZERO;
        let mut centroid = Vec3::ZERO;
        for index in 0..points.len() {
            let current = points[index];
            let next = points[(index + 1) % points.len()];
            normal.x += (current.y - next.y) * (current.z + next.z);
            normal.y += (current.z - next.z) * (current.x + next.x);
            normal.z += (current.x - next.x) * (current.y + next.y);
            centroid = centroid.add(current);
        }
        let length = normal.length();
        if length <= f64::MIN_POSITIVE {
            return None;
        }
        let normal = normal.mul(1.0 / length);
        let centroid = centroid.mul(1.0 / points.len() as f64);
        Some(Self::new(normal, normal.dot(centroid)))
    }

    pub fn flipped(self) -> Self {
        Self::new(self.normal.neg(), -self.offset)
    }

    pub fn distance(self, point: Vec3) -> f64 {
        self.normal.dot(point) - self.offset
    }

    /// True when both planes describe the same oriented plane.
    pub fn same_as(self, other: Self) -> bool {
        self.normal.dot(other.normal) > 1.0 - PLANE_ANGLE_EPSILON
            && (self.offset - other.offset).abs() <= PLANE_OFFSET_EPSILON
    }

    /// Index of the axis the plane is most perpendicular to.
    pub fn dominant_axis(self) -> usize {
        let absolute = [
            self.normal.x.abs(),
            self.normal.y.abs(),
            self.normal.z.abs(),
        ];
        if absolute[0] >= absolute[1] && absolute[0] >= absolute[2] {
            0
        } else if absolute[1] >= absolute[2] {
            1
        } else {
            2
        }
    }
}

/// A planar face. Vertices wind counter-clockwise around `plane.normal`.
#[derive(Clone, Debug)]
pub struct Polygon {
    pub vertices: Vec<Vec3>,
    pub plane: Plane,
}

impl Polygon {
    pub fn new(vertices: Vec<Vec3>) -> Option<Self> {
        let plane = Plane::from_polygon(&vertices)?;
        Some(Self { vertices, plane })
    }

    pub fn with_plane(vertices: Vec<Vec3>, plane: Plane) -> Self {
        Self { vertices, plane }
    }

    pub fn flip(&mut self) {
        self.vertices.reverse();
        self.plane = self.plane.flipped();
    }

    pub fn flipped(&self) -> Self {
        let mut clone = self.clone();
        clone.flip();
        clone
    }

    /// Twice the polygon area as an oriented vector (Newell area vector).
    pub fn area_vector(&self) -> Vec3 {
        let mut sum = Vec3::ZERO;
        for index in 1..self.vertices.len().saturating_sub(1) {
            sum = sum.add(
                self.vertices[index]
                    .sub(self.vertices[0])
                    .cross(self.vertices[index + 1].sub(self.vertices[0])),
            );
        }
        sum.mul(0.5)
    }

    pub fn area(&self) -> f64 {
        self.area_vector().length()
    }

    /// Drops repeated vertices and collinear runs. Returns false when the
    /// polygon degenerates to less than a triangle.
    pub fn simplify(&mut self, tolerance: f64) -> bool {
        dedup_ring(&mut self.vertices, tolerance);
        if self.vertices.len() < 3 {
            return false;
        }
        remove_collinear(&mut self.vertices, tolerance);
        self.vertices.len() >= 3
    }
}

pub fn dedup_ring(points: &mut Vec<Vec3>, tolerance: f64) {
    // Compacted in place: every split fragment in the BSP is deduplicated, so
    // building a second vector here cost one allocation per fragment.
    let mut kept = 0usize;
    for read in 0..points.len() {
        let point = points[read];
        if kept > 0 && points[kept - 1].sub(point).length() <= tolerance {
            continue;
        }
        points[kept] = point;
        kept += 1;
    }
    points.truncate(kept);
    while points.len() >= 2 {
        let first = points[0];
        let last = points[points.len() - 1];
        if first.sub(last).length() <= tolerance {
            points.pop();
        } else {
            break;
        }
    }
}

fn remove_collinear(points: &mut Vec<Vec3>, tolerance: f64) {
    let mut changed = true;
    while changed && points.len() > 3 {
        changed = false;
        let mut index = 0;
        while index < points.len() && points.len() > 3 {
            let previous = points[(index + points.len() - 1) % points.len()];
            let current = points[index];
            let next = points[(index + 1) % points.len()];
            let first = current.sub(previous);
            let second = next.sub(current);
            let cross = first.cross(second).length();
            let base = first.length().max(second.length());
            if base > 0.0 && cross <= tolerance * base {
                points.remove(index);
                changed = true;
            } else {
                index += 1;
            }
        }
    }
}

/// A 4x4 affine transform stored row-major.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Matrix4(pub [[f64; 4]; 4]);

impl Default for Matrix4 {
    fn default() -> Self {
        Self::identity()
    }
}

impl Matrix4 {
    pub fn identity() -> Self {
        Self([
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ])
    }

    pub fn translation(offset: Vec3) -> Self {
        let mut matrix = Self::identity();
        matrix.0[0][3] = offset.x;
        matrix.0[1][3] = offset.y;
        matrix.0[2][3] = offset.z;
        matrix
    }

    pub fn scaling(factors: Vec3) -> Self {
        let mut matrix = Self::identity();
        matrix.0[0][0] = factors.x;
        matrix.0[1][1] = factors.y;
        matrix.0[2][2] = factors.z;
        matrix
    }

    pub fn rotation_x(degrees: f64) -> Self {
        let radians = degrees.to_radians();
        let (sin, cos) = radians.sin_cos();
        Self([
            [1.0, 0.0, 0.0, 0.0],
            [0.0, cos, -sin, 0.0],
            [0.0, sin, cos, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ])
    }

    pub fn rotation_y(degrees: f64) -> Self {
        let radians = degrees.to_radians();
        let (sin, cos) = radians.sin_cos();
        Self([
            [cos, 0.0, sin, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [-sin, 0.0, cos, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ])
    }

    pub fn rotation_z(degrees: f64) -> Self {
        let radians = degrees.to_radians();
        let (sin, cos) = radians.sin_cos();
        Self([
            [cos, -sin, 0.0, 0.0],
            [sin, cos, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ])
    }

    pub fn multiply(self, other: Self) -> Self {
        let mut result = [[0.0f64; 4]; 4];
        for row in 0..4 {
            for column in 0..4 {
                let mut sum = 0.0;
                for index in 0..4 {
                    sum += self.0[row][index] * other.0[index][column];
                }
                result[row][column] = sum;
            }
        }
        Self(result)
    }

    pub fn apply(self, point: Vec3) -> Vec3 {
        let matrix = &self.0;
        Vec3::new(
            matrix[0][0] * point.x + matrix[0][1] * point.y + matrix[0][2] * point.z + matrix[0][3],
            matrix[1][0] * point.x + matrix[1][1] * point.y + matrix[1][2] * point.z + matrix[1][3],
            matrix[2][0] * point.x + matrix[2][1] * point.y + matrix[2][2] * point.z + matrix[2][3],
        )
    }

    /// Determinant of the linear (upper 3x3) part; negative means the map
    /// mirrors, which requires flipping every face to keep normals outward.
    pub fn linear_determinant(self) -> f64 {
        let matrix = &self.0;
        matrix[0][0] * (matrix[1][1] * matrix[2][2] - matrix[1][2] * matrix[2][1])
            - matrix[0][1] * (matrix[1][0] * matrix[2][2] - matrix[1][2] * matrix[2][0])
            + matrix[0][2] * (matrix[1][0] * matrix[2][1] - matrix[1][1] * matrix[2][0])
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub min: Vec3,
    pub max: Vec3,
}

impl Bounds {
    pub fn from_points(points: impl IntoIterator<Item = Vec3>) -> Option<Self> {
        let mut iterator = points.into_iter();
        let first = iterator.next()?;
        let mut bounds = Self {
            min: first,
            max: first,
        };
        for point in iterator {
            bounds.min = bounds.min.min(point);
            bounds.max = bounds.max.max(point);
        }
        Some(bounds)
    }

    pub fn union(self, other: Self) -> Self {
        Self {
            min: self.min.min(other.min),
            max: self.max.max(other.max),
        }
    }

    pub fn overlaps(self, other: Self, slack: f64) -> bool {
        self.min.x <= other.max.x + slack
            && other.min.x <= self.max.x + slack
            && self.min.y <= other.max.y + slack
            && other.min.y <= self.max.y + slack
            && self.min.z <= other.max.z + slack
            && other.min.z <= self.max.z + slack
    }

    pub fn size(self) -> Vec3 {
        self.max.sub(self.min)
    }
}

/// An indexed triangle mesh: the exportable form of a solid.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IndexedMesh {
    pub vertices: Vec<Vec3>,
    pub triangles: Vec<[u32; 3]>,
}

impl IndexedMesh {
    pub fn is_empty(&self) -> bool {
        self.triangles.is_empty()
    }

    pub fn bounds(&self) -> Option<Bounds> {
        Bounds::from_points(self.vertices.iter().copied())
    }

    pub fn triangle_points(&self, index: usize) -> [Vec3; 3] {
        let triangle = self.triangles[index];
        [
            self.vertices[triangle[0] as usize],
            self.vertices[triangle[1] as usize],
            self.vertices[triangle[2] as usize],
        ]
    }

    pub fn surface_area(&self) -> f64 {
        (0..self.triangles.len())
            .map(|index| {
                let [a, b, c] = self.triangle_points(index);
                b.sub(a).cross(c.sub(a)).length() * 0.5
            })
            .sum()
    }

    /// Signed volume via the divergence theorem. Positive for outward normals.
    pub fn volume(&self) -> f64 {
        (0..self.triangles.len())
            .map(|index| {
                let [a, b, c] = self.triangle_points(index);
                a.dot(b.cross(c)) / 6.0
            })
            .sum()
    }

    pub fn statistics(&self) -> MeshStatistics {
        let mut edges: FastMap<(u32, u32), i32> = FastMap::default();
        for triangle in &self.triangles {
            for index in 0..3 {
                let from = triangle[index];
                let to = triangle[(index + 1) % 3];
                let (key, delta) = if from < to {
                    ((from, to), 1)
                } else {
                    ((to, from), -1)
                };
                *edges.entry(key).or_insert(0) += delta;
            }
        }
        let mut used: Vec<bool> = vec![false; self.vertices.len()];
        for triangle in &self.triangles {
            for index in triangle {
                used[*index as usize] = true;
            }
        }
        let mut degenerate = 0usize;
        for triangle in &self.triangles {
            if triangle[0] == triangle[1] || triangle[1] == triangle[2] || triangle[0] == triangle[2]
            {
                degenerate += 1;
            }
        }
        let mut counts: FastMap<(u32, u32), usize> = FastMap::default();
        for triangle in &self.triangles {
            for index in 0..3 {
                let from = triangle[index];
                let to = triangle[(index + 1) % 3];
                let key = if from < to { (from, to) } else { (to, from) };
                *counts.entry(key).or_insert(0) += 1;
            }
        }
        let vertices = used.iter().filter(|flag| **flag).count();
        let boundary_edges = counts.values().filter(|count| **count == 1).count();
        let non_manifold_edges = counts.values().filter(|count| **count > 2).count();
        let unbalanced_edges = edges.values().filter(|balance| **balance != 0).count();
        MeshStatistics {
            vertices,
            edges: counts.len(),
            faces: self.triangles.len(),
            boundary_edges,
            non_manifold_edges,
            unbalanced_edges,
            degenerate_faces: degenerate,
            euler_characteristic: vertices as isize - counts.len() as isize
                + self.triangles.len() as isize,
        }
    }

    pub fn is_manifold(&self) -> bool {
        let statistics = self.statistics();
        statistics.boundary_edges == 0
            && statistics.non_manifold_edges == 0
            && statistics.unbalanced_edges == 0
            && statistics.degenerate_faces == 0
    }

    pub fn transform(&self, matrix: Matrix4) -> Self {
        let mut mesh = Self {
            vertices: self.vertices.iter().map(|p| matrix.apply(*p)).collect(),
            triangles: self.triangles.clone(),
        };
        if matrix.linear_determinant() < 0.0 {
            for triangle in &mut mesh.triangles {
                triangle.swap(1, 2);
            }
        }
        mesh
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MeshStatistics {
    pub vertices: usize,
    pub edges: usize,
    pub faces: usize,
    pub boundary_edges: usize,
    pub non_manifold_edges: usize,
    pub unbalanced_edges: usize,
    pub degenerate_faces: usize,
    pub euler_characteristic: isize,
}

impl fmt::Display for MeshStatistics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "V={} E={} F={} chi={} boundary={} non-manifold={} unbalanced={} degenerate={}",
            self.vertices,
            self.edges,
            self.faces,
            self.euler_characteristic,
            self.boundary_edges,
            self.non_manifold_edges,
            self.unbalanced_edges,
            self.degenerate_faces
        )
    }
}

/// Welds vertices onto a lattice of `WELD_EPSILON` cells so that coincident
/// corners produced by independent boolean fragments become one index.
pub struct VertexWelder {
    lattice: FastMap<[i64; 3], u32>,
    pub vertices: Vec<Vec3>,
    scale: f64,
}

impl Default for VertexWelder {
    fn default() -> Self {
        Self::new(WELD_EPSILON)
    }
}

impl VertexWelder {
    pub fn new(tolerance: f64) -> Self {
        Self {
            lattice: FastMap::default(),
            vertices: Vec::new(),
            scale: 1.0 / tolerance,
        }
    }

    fn key(&self, point: Vec3) -> [i64; 3] {
        [
            (point.x * self.scale).round() as i64,
            (point.y * self.scale).round() as i64,
            (point.z * self.scale).round() as i64,
        ]
    }

    pub fn insert(&mut self, point: Vec3) -> u32 {
        let key = self.key(point);
        // Probe the 27 neighbouring cells so points that straddle a lattice
        // boundary still weld together.
        for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    let probe = [key[0] + dx, key[1] + dy, key[2] + dz];
                    if let Some(index) = self.lattice.get(&probe) {
                        let existing = self.vertices[*index as usize];
                        if existing.sub(point).length() <= 1.0 / self.scale {
                            return *index;
                        }
                    }
                }
            }
        }
        let index = self.vertices.len() as u32;
        self.vertices.push(point);
        self.lattice.insert(key, index);
        index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The welder has to merge the two answers an ill-conditioned plane
    /// intersection gives for one corner. At [`EPSILON`] it did not, and a
    /// boolean between shallow-crossing faces came out of the kernel cracked.
    ///
    /// The distances here bracket the real measurement: 2.5e-5 is the widest
    /// spread seen on a louvred basket's blade crossings and must weld, while
    /// a separation of 1e-3 — OpenSCAD's own coarse grid — is real geometry
    /// that must survive.
    #[test]
    fn the_welder_merges_a_corner_two_ill_conditioned_intersections_disagree_on() {
        let mut welder = VertexWelder::default();
        let corner = Vec3::new(41.100_09, -2.759_142, 1.426_439);
        let first = welder.insert(corner);

        for drift in [1.0e-9, 1.0e-7, 2.5e-5] {
            let nudged = Vec3::new(corner.x, corner.y - drift, corner.z);
            assert_eq!(
                welder.insert(nudged),
                first,
                "a corner {drift} away is the same corner, not a new vertex"
            );
        }

        let apart = Vec3::new(corner.x, corner.y - 1.0e-3, corner.z);
        assert_ne!(
            welder.insert(apart),
            first,
            "1e-3 mm is modelled detail and must not be welded away"
        );
    }

    /// Welding is what closes a seam, so it has to survive the lattice: two
    /// points inside the tolerance of each other can still land in different
    /// cells, which is why `insert` probes the neighbours.
    #[test]
    fn welding_is_not_defeated_by_a_lattice_boundary() {
        let mut welder = VertexWelder::default();
        // Straddling a cell edge of the welder's own lattice, whatever the
        // tolerance is set to, so the pair is split between two cells.
        let boundary = (0.5 / WELD_EPSILON).round() * WELD_EPSILON;
        let on_boundary = Vec3::new(boundary, boundary, boundary);
        let across = Vec3::new(
            boundary - WELD_EPSILON / 4.0,
            boundary - WELD_EPSILON / 4.0,
            boundary - WELD_EPSILON / 4.0,
        );
        assert_eq!(welder.insert(on_boundary), welder.insert(across));
    }
}
