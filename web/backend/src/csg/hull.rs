//! Exact convex hull for the polyhedral CSG kernel (`hull()`).
//!
//! The 3D hull is built with the classic incremental / quickhull construction:
//! a non-degenerate tetrahedron is seeded from extreme points, then every
//! remaining point is inserted by deleting the faces it can *see*, walking the
//! horizon of that visible region and stitching a fan of new triangles back to
//! the point.
//!
//! Two properties matter for reproducing OpenSCAD's `hull()` output exactly:
//!
//! * **Determinism.** The input cloud is sorted with `f64::total_cmp` on
//!   `(x, y, z)` and deduplicated before anything else happens, so the face
//!   list depends only on the *set* of points, never on the order the caller
//!   happened to produce them in. Every max-search below breaks ties towards
//!   the lowest index, so the seed simplex is a pure function of that sorted
//!   list too.
//!
//! * **Coplanar merging.** A raw quickhull returns a triangulation; OpenSCAD
//!   returns polygons. After the hull is built, faces are grouped by
//!   [`Plane::same_as`] and each group's boundary loop is recovered by
//!   cancelling directed edges that occur in both directions. The hull of the
//!   eight corners of a cube therefore yields exactly six quads, not twelve
//!   triangles.
//!
//! # Epsilon strategy
//!
//! `mesh::EPSILON` is deliberately absolute, but a hull is a purely relative
//! construction: whether a point is "outside" a face only has meaning relative
//! to the size of the cloud. So every tolerance here is derived from the
//! largest axis extent of the input, clamped to at least 1.0 so that tiny
//! models keep an absolute floor comparable to the rest of the kernel:
//!
//! * `scale * 1e-10` — "point lies strictly above a face plane". This is the
//!   only predicate that decides topology, and it is used **strictly**: a
//!   point that merely touches a face plane is never treated as visible. That
//!   single choice is what keeps sliver faces out. If a face `F` is strictly
//!   visible from `p`, then `p` is further than the tolerance from `F`'s
//!   plane, hence further than the tolerance from every line inside it — so no
//!   horizon edge can ever be collinear with `p`, and no zero-area triangle
//!   can be created. Points sitting exactly on a hull face (or on a hull edge,
//!   or inside the hull) are simply never inserted.
//! * `scale * 1e-12` — area / degeneracy tests on the emitted polygons.
//! * `scale * 1e-12` — collinear-vertex collapse on merged faces. Kept this
//!   tight on purpose: a looser value could delete a genuine corner from one
//!   face while its neighbour keeps it, which would tear the surface.

use std::collections::HashMap;

use super::mesh::{Plane, Polygon, Vec3};

/// Relative tolerance for the "strictly outside a face plane" predicate.
const ABOVE_RELATIVE: f64 = 1.0e-10;
/// Relative tolerance for area and degeneracy tests.
const AREA_RELATIVE: f64 = 1.0e-12;
/// Relative tolerance for collapsing collinear vertices of a merged face.
const COLLINEAR_RELATIVE: f64 = 1.0e-12;

#[derive(Clone, Copy, Debug)]
struct Face {
    a: usize,
    b: usize,
    c: usize,
    plane: Plane,
    alive: bool,
}

impl Face {
    fn vertices(&self) -> [usize; 3] {
        [self.a, self.b, self.c]
    }

    /// The three directed edges, wound the same way as the face.
    fn edges(&self) -> [(usize, usize); 3] {
        [(self.a, self.b), (self.b, self.c), (self.c, self.a)]
    }
}

/// Convex hull of a 3D point cloud.
///
/// Returns outward-facing faces with coplanar triangles merged into single
/// convex polygons (so a hull of 8 cube corners yields exactly 6 quads).
/// Returns an empty Vec when the input is degenerate (fewer than 4
/// affinely independent points).
pub fn convex_hull(points: &[Vec3]) -> Vec<Polygon> {
    super::prof::scope("hull:convex_hull", || convex_hull_inner(points))
}

fn convex_hull_inner(points: &[Vec3]) -> Vec<Polygon> {
    let points = normalize_points(points);
    if points.len() < 4 {
        return Vec::new();
    }

    let scale = point_scale(&points);
    let above_tolerance = scale * ABOVE_RELATIVE;
    let area_tolerance = scale * AREA_RELATIVE;
    let collinear_tolerance = scale * COLLINEAR_RELATIVE;

    let seed = match initial_simplex(&points, above_tolerance) {
        Some(seed) => seed,
        // Empty, collinear or coplanar: a hull with zero volume is not a solid.
        None => return Vec::new(),
    };

    let mut faces = match seed_faces(&points, seed) {
        Some(faces) => faces,
        None => return Vec::new(),
    };

    for index in 0..points.len() {
        if seed.contains(&index) {
            continue;
        }
        insert_point(&points, &mut faces, index, above_tolerance);
    }

    merge_coplanar_faces(&points, &faces, collinear_tolerance, area_tolerance)
}

/// Convex hull of a 2D point set, returned as a CCW contour.
/// Empty when fewer than 3 non-collinear points.
pub fn convex_hull_2d(points: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let mut sorted: Vec<[f64; 2]> = points
        .iter()
        .copied()
        .filter(|p| p[0].is_finite() && p[1].is_finite())
        .collect();
    sorted.sort_by(|left, right| left[0].total_cmp(&right[0]).then(left[1].total_cmp(&right[1])));
    sorted.dedup_by(|left, right| left[0] == right[0] && left[1] == right[1]);
    if sorted.len() < 3 {
        return Vec::new();
    }

    let mut extent: f64 = 0.0;
    for axis in 0..2 {
        let min = sorted
            .iter()
            .map(|p| p[axis])
            .fold(f64::INFINITY, f64::min);
        let max = sorted
            .iter()
            .map(|p| p[axis])
            .fold(f64::NEG_INFINITY, f64::max);
        extent = extent.max(max - min);
    }
    let scale = extent.max(1.0);
    // Twice the triangle area, so the tolerance carries squared units.
    let area_tolerance = scale * scale * AREA_RELATIVE;

    // Andrew's monotone chain: lower hull, then upper hull, both counter-
    // clockwise. `<= tolerance` pops collinear points as well as reflex ones,
    // so the contour never carries a redundant vertex.
    let cross = |o: [f64; 2], a: [f64; 2], b: [f64; 2]| {
        (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
    };

    let mut hull: Vec<[f64; 2]> = Vec::with_capacity(sorted.len() * 2);
    for point in sorted.iter().copied() {
        while hull.len() >= 2
            && cross(hull[hull.len() - 2], hull[hull.len() - 1], point) <= area_tolerance
        {
            hull.pop();
        }
        hull.push(point);
    }
    let lower = hull.len() + 1;
    for point in sorted.iter().rev().copied() {
        while hull.len() >= lower
            && cross(hull[hull.len() - 2], hull[hull.len() - 1], point) <= area_tolerance
        {
            hull.pop();
        }
        hull.push(point);
    }
    hull.pop();

    if hull.len() < 3 {
        return Vec::new();
    }
    hull
}

/// Deterministic canonical form of the input: finite points only, sorted by
/// `total_cmp` on `(x, y, z)`, exact duplicates removed.
fn normalize_points(points: &[Vec3]) -> Vec<Vec3> {
    let mut sorted: Vec<Vec3> = points.iter().copied().filter(|p| p.is_finite()).collect();
    sorted.sort_by(|left, right| {
        left.x
            .total_cmp(&right.x)
            .then(left.y.total_cmp(&right.y))
            .then(left.z.total_cmp(&right.z))
    });
    sorted.dedup_by(|left, right| left.x == right.x && left.y == right.y && left.z == right.z);
    sorted
}

/// Largest axis extent of the cloud, clamped to 1.0.
fn point_scale(points: &[Vec3]) -> f64 {
    let mut extent: f64 = 0.0;
    for axis in 0..3 {
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        for point in points {
            let value = point.component(axis);
            min = min.min(value);
            max = max.max(value);
        }
        extent = extent.max(max - min);
    }
    extent.max(1.0)
}

/// Four affinely independent seed indices, or `None` when the cloud is
/// degenerate. Every search breaks ties towards the lowest index so the seed
/// is a pure function of the sorted cloud.
fn initial_simplex(points: &[Vec3], tolerance: f64) -> Option<[usize; 4]> {
    // Extreme points along each axis are cheap and are guaranteed hull
    // vertices, which keeps the seed tetrahedron fat.
    let mut candidates: Vec<usize> = Vec::with_capacity(6);
    for axis in 0..3 {
        let mut lowest = 0usize;
        let mut highest = 0usize;
        for index in 1..points.len() {
            if points[index].component(axis) < points[lowest].component(axis) {
                lowest = index;
            }
            if points[index].component(axis) > points[highest].component(axis) {
                highest = index;
            }
        }
        candidates.push(lowest);
        candidates.push(highest);
    }
    candidates.sort_unstable();
    candidates.dedup();

    let mut first = candidates[0];
    let mut second = candidates[0];
    let mut best = 0.0;
    for left in 0..candidates.len() {
        for right in (left + 1)..candidates.len() {
            let distance = points[candidates[left]]
                .sub(points[candidates[right]])
                .length();
            if distance > best {
                best = distance;
                first = candidates[left];
                second = candidates[right];
            }
        }
    }
    if best <= tolerance {
        return None;
    }

    let direction = points[second].sub(points[first]).normalized();
    let mut third = usize::MAX;
    let mut best_off_line = tolerance;
    for index in 0..points.len() {
        let distance = points[index].sub(points[first]).cross(direction).length();
        if distance > best_off_line {
            best_off_line = distance;
            third = index;
        }
    }
    if third == usize::MAX {
        return None;
    }

    let plane = Plane::from_polygon(&[points[first], points[second], points[third]])?;
    let mut fourth = usize::MAX;
    let mut best_off_plane = tolerance;
    for index in 0..points.len() {
        let distance = plane.distance(points[index]).abs();
        if distance > best_off_plane {
            best_off_plane = distance;
            fourth = index;
        }
    }
    if fourth == usize::MAX {
        return None;
    }

    // Orient the base triangle so the apex sits on its negative side; the
    // canonical face list below then comes out with outward normals.
    if plane.distance(points[fourth]) > 0.0 {
        Some([first, third, second, fourth])
    } else {
        Some([first, second, third, fourth])
    }
}

fn make_face(points: &[Vec3], a: usize, b: usize, c: usize) -> Option<Face> {
    let plane = Plane::from_polygon(&[points[a], points[b], points[c]])?;
    Some(Face {
        a,
        b,
        c,
        plane,
        alive: true,
    })
}

fn seed_faces(points: &[Vec3], seed: [usize; 4]) -> Option<Vec<Face>> {
    let [a, b, c, d] = seed;
    Some(vec![
        make_face(points, a, b, c)?,
        make_face(points, a, d, b)?,
        make_face(points, b, d, c)?,
        make_face(points, c, d, a)?,
    ])
}

/// Delete every face the point can strictly see, then stitch new faces from
/// the horizon of that region back to the point.
fn insert_point(points: &[Vec3], faces: &mut Vec<Face>, index: usize, tolerance: f64) {
    let point = points[index];

    let mut visible: Vec<usize> = Vec::new();
    for (face_index, face) in faces.iter().enumerate() {
        if face.alive && face.plane.distance(point) > tolerance {
            visible.push(face_index);
        }
    }
    if visible.is_empty() {
        // Inside the hull, on a face, or on an edge: nothing to do. Handling
        // "on a face" here is what keeps sliver faces out of the result.
        return;
    }

    // On a closed manifold, a directed edge of a visible face is on the
    // horizon exactly when its twin does not belong to another visible face.
    let mut visible_edges: HashMap<(usize, usize), ()> = HashMap::new();
    for face_index in &visible {
        for edge in faces[*face_index].edges() {
            visible_edges.insert(edge, ());
        }
    }

    let mut horizon: Vec<(usize, usize)> = Vec::new();
    for face_index in &visible {
        for (from, to) in faces[*face_index].edges() {
            if !visible_edges.contains_key(&(to, from)) {
                horizon.push((from, to));
            }
        }
    }

    for face_index in &visible {
        faces[*face_index].alive = false;
    }

    for (from, to) in horizon {
        // Cannot be degenerate: `point` is further than `tolerance` from the
        // plane of the visible face that owned this edge, so it is off the
        // edge's supporting line by at least that much.
        if let Some(face) = make_face(points, from, to, index) {
            faces.push(face);
        }
    }
}

/// Groups coplanar faces, recovers each group's boundary loop by cancelling
/// opposite directed edges, and emits one polygon per loop.
fn merge_coplanar_faces(
    points: &[Vec3],
    faces: &[Face],
    collinear_tolerance: f64,
    area_tolerance: f64,
) -> Vec<Polygon> {
    let mut groups: Vec<(Plane, Vec<[usize; 3]>)> = Vec::new();
    for face in faces.iter().filter(|face| face.alive) {
        match groups
            .iter_mut()
            .find(|(plane, _)| plane.same_as(face.plane))
        {
            Some((_, members)) => members.push(face.vertices()),
            None => groups.push((face.plane, vec![face.vertices()])),
        }
    }

    let mut polygons: Vec<Polygon> = Vec::new();
    for (plane, members) in groups {
        let mut directed: Vec<(usize, usize)> = Vec::with_capacity(members.len() * 3);
        for [a, b, c] in &members {
            directed.push((*a, *b));
            directed.push((*b, *c));
            directed.push((*c, *a));
        }
        let present: HashMap<(usize, usize), ()> =
            directed.iter().map(|edge| (*edge, ())).collect();
        // An edge interior to the group appears in both directions and cancels.
        let boundary: Vec<(usize, usize)> = directed
            .into_iter()
            .filter(|(from, to)| !present.contains_key(&(*to, *from)))
            .collect();
        if boundary.len() < 3 {
            continue;
        }

        let mut outgoing: HashMap<usize, Vec<usize>> = HashMap::new();
        for (slot, (from, _)) in boundary.iter().enumerate() {
            outgoing.entry(*from).or_default().push(slot);
        }

        let mut used = vec![false; boundary.len()];
        for start in 0..boundary.len() {
            if used[start] {
                continue;
            }
            let mut loop_indices: Vec<usize> = Vec::new();
            let mut current = start;
            loop {
                used[current] = true;
                loop_indices.push(boundary[current].0);
                let next_vertex = boundary[current].1;
                if next_vertex == boundary[start].0 {
                    break;
                }
                let next = outgoing
                    .get(&next_vertex)
                    .and_then(|slots| slots.iter().copied().find(|slot| !used[*slot]));
                match next {
                    Some(slot) => current = slot,
                    // Not a closed loop: drop it rather than emit garbage.
                    None => {
                        loop_indices.clear();
                        break;
                    }
                }
            }
            if loop_indices.len() < 3 {
                continue;
            }

            let vertices: Vec<Vec3> = loop_indices.iter().map(|index| points[*index]).collect();
            let mut polygon = Polygon::with_plane(vertices, plane);
            if !polygon.simplify(collinear_tolerance) {
                continue;
            }
            if polygon.vertices.len() < 3 || polygon.area() <= area_tolerance {
                continue;
            }
            polygons.push(polygon);
        }
    }

    polygons
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cube_corners(size: f64) -> Vec<Vec3> {
        let mut points = Vec::new();
        for x in 0..2 {
            for y in 0..2 {
                for z in 0..2 {
                    points.push(Vec3::new(
                        x as f64 * size,
                        y as f64 * size,
                        z as f64 * size,
                    ));
                }
            }
        }
        points
    }

    /// Enclosed volume: fan-triangulate each polygon and sum the signed
    /// tetrahedra against the origin.
    fn hull_volume(polygons: &[Polygon]) -> f64 {
        let mut total = 0.0;
        for polygon in polygons {
            for index in 1..polygon.vertices.len() - 1 {
                let a = polygon.vertices[0];
                let b = polygon.vertices[index];
                let c = polygon.vertices[index + 1];
                total += a.dot(b.cross(c));
            }
        }
        total / 6.0
    }

    fn total_area(polygons: &[Polygon]) -> f64 {
        polygons.iter().map(|polygon| polygon.area()).sum()
    }

    /// Rotation-invariant identity of a face, for set comparisons.
    fn canonical_face(polygon: &Polygon) -> Vec<(u64, u64, u64)> {
        let bits: Vec<(u64, u64, u64)> = polygon
            .vertices
            .iter()
            .map(|v| (v.x.to_bits(), v.y.to_bits(), v.z.to_bits()))
            .collect();
        let start = (0..bits.len())
            .min_by_key(|index| bits[*index])
            .unwrap_or(0);
        (0..bits.len())
            .map(|offset| bits[(start + offset) % bits.len()])
            .collect()
    }

    fn face_set(polygons: &[Polygon]) -> Vec<Vec<(u64, u64, u64)>> {
        let mut faces: Vec<Vec<(u64, u64, u64)>> =
            polygons.iter().map(canonical_face).collect();
        faces.sort();
        faces
    }

    #[test]
    fn cube_corners_merge_into_six_quads() {
        let hull = convex_hull(&cube_corners(20.0));
        assert_eq!(hull.len(), 6, "expected six merged quads");
        for polygon in &hull {
            assert_eq!(polygon.vertices.len(), 4, "each cube face is a quad");
            assert!(polygon.area() > 0.0);
        }
        assert!((total_area(&hull) - 2400.0).abs() < 1e-9, "area {}", total_area(&hull));
        assert!((hull_volume(&hull) - 8000.0).abs() < 1e-9, "volume {}", hull_volume(&hull));
    }

    #[test]
    fn tetrahedron_returns_four_triangles() {
        let points = vec![
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(10.0, 0.0, 0.0),
            Vec3::new(0.0, 10.0, 0.0),
            Vec3::new(0.0, 0.0, 10.0),
        ];
        let hull = convex_hull(&points);
        assert_eq!(hull.len(), 4);
        for polygon in &hull {
            assert_eq!(polygon.vertices.len(), 3);
        }
        // Volume of the corner tetrahedron is 10^3 / 6.
        assert!((hull_volume(&hull) - 1000.0 / 6.0).abs() < 1e-9);
    }

    #[test]
    fn interior_point_is_ignored() {
        let mut points = cube_corners(20.0);
        points.push(Vec3::new(10.0, 10.0, 10.0));
        let hull = convex_hull(&points);
        assert_eq!(hull.len(), 6);
        for polygon in &hull {
            assert_eq!(polygon.vertices.len(), 4);
        }
        assert!((hull_volume(&hull) - 8000.0).abs() < 1e-9);
    }

    #[test]
    fn point_on_a_face_does_not_create_slivers() {
        let mut points = cube_corners(20.0);
        // Dead centre of the z = 0 face, and mid-edge of the x = 20 face.
        points.push(Vec3::new(10.0, 10.0, 0.0));
        points.push(Vec3::new(20.0, 10.0, 0.0));
        let hull = convex_hull(&points);
        assert_eq!(hull.len(), 6);
        for polygon in &hull {
            assert_eq!(polygon.vertices.len(), 4);
        }
        assert!((hull_volume(&hull) - 8000.0).abs() < 1e-9);
    }

    #[test]
    fn coplanar_input_is_empty() {
        let points = vec![
            Vec3::new(0.0, 0.0, 5.0),
            Vec3::new(10.0, 0.0, 5.0),
            Vec3::new(10.0, 10.0, 5.0),
            Vec3::new(0.0, 10.0, 5.0),
        ];
        assert!(convex_hull(&points).is_empty());
    }

    #[test]
    fn collinear_input_is_empty() {
        let points = vec![
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(2.0, 0.0, 0.0),
            Vec3::new(3.0, 0.0, 0.0),
        ];
        assert!(convex_hull(&points).is_empty());
    }

    #[test]
    fn two_points_are_empty() {
        let points = vec![Vec3::new(0.0, 0.0, 0.0), Vec3::new(1.0, 2.0, 3.0)];
        assert!(convex_hull(&points).is_empty());
    }

    #[test]
    fn empty_input_is_empty() {
        assert!(convex_hull(&[]).is_empty());
    }

    #[test]
    fn square_with_centre_gives_four_ccw_points() {
        let points = [
            [0.0, 0.0],
            [10.0, 0.0],
            [10.0, 10.0],
            [0.0, 10.0],
            [5.0, 5.0],
        ];
        let hull = convex_hull_2d(&points);
        assert_eq!(hull.len(), 4, "hull {:?}", hull);
        let mut signed = 0.0;
        for index in 0..hull.len() {
            let a = hull[index];
            let b = hull[(index + 1) % hull.len()];
            signed += a[0] * b[1] - b[0] * a[1];
        }
        assert!(signed > 0.0, "contour must wind counter-clockwise");
        assert!((signed / 2.0 - 100.0).abs() < 1e-9);
        for corner in points.iter().take(4) {
            assert!(hull.iter().any(|p| p == corner), "missing corner {:?}", corner);
        }
    }

    #[test]
    fn two_dimensional_degenerate_cases_are_empty() {
        assert!(convex_hull_2d(&[]).is_empty());
        assert!(convex_hull_2d(&[[0.0, 0.0], [1.0, 1.0]]).is_empty());
        assert!(convex_hull_2d(&[[0.0, 0.0], [1.0, 1.0], [2.0, 2.0], [3.0, 3.0]]).is_empty());
    }

    #[test]
    fn shuffled_input_gives_the_same_faces() {
        let base = sphere_points(120);
        let reference = convex_hull(&base);
        assert!(reference.len() > 4);

        let mut shuffled = base.clone();
        shuffled.reverse();
        assert_eq!(face_set(&reference), face_set(&convex_hull(&shuffled)));

        // A deterministic "shuffle" that is neither the original nor its
        // reverse: swap every pair and rotate.
        let mut rotated = base.clone();
        for index in (0..rotated.len() - 1).step_by(2) {
            rotated.swap(index, index + 1);
        }
        rotated.rotate_left(37);
        assert_eq!(face_set(&reference), face_set(&convex_hull(&rotated)));

        // Duplicated points must not change the answer either.
        let mut duplicated = base.clone();
        duplicated.extend(base.iter().copied());
        assert_eq!(face_set(&reference), face_set(&convex_hull(&duplicated)));
    }

    /// Deterministic pseudo-random points on a sphere of radius 10.
    fn sphere_points(count: usize) -> Vec<Vec3> {
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 11) as f64) / ((1u64 << 53) as f64)
        };
        let mut points = Vec::with_capacity(count);
        for _ in 0..count {
            let z = next() * 2.0 - 1.0;
            let angle = next() * std::f64::consts::TAU;
            let radius = (1.0f64 - z * z).max(0.0).sqrt();
            points.push(Vec3::new(
                10.0 * radius * angle.cos(),
                10.0 * radius * angle.sin(),
                10.0 * z,
            ));
        }
        points
    }

    #[test]
    fn sphere_cloud_produces_a_closed_convex_solid() {
        let points = sphere_points(400);
        let hull = convex_hull(&points);
        assert!(hull.len() > 20);

        for polygon in &hull {
            assert!(polygon.vertices.len() >= 3);
            assert!(polygon.area() > 0.0, "degenerate face {:?}", polygon.vertices);
        }

        // Closed surface: every directed edge is matched by its twin.
        let mut edges: HashMap<((u64, u64, u64), (u64, u64, u64)), i32> = HashMap::new();
        let key = |v: &Vec3| (v.x.to_bits(), v.y.to_bits(), v.z.to_bits());
        for polygon in &hull {
            for index in 0..polygon.vertices.len() {
                let from = key(&polygon.vertices[index]);
                let to = key(&polygon.vertices[(index + 1) % polygon.vertices.len()]);
                *edges.entry((from, to)).or_insert(0) += 1;
                *edges.entry((to, from)).or_insert(0) -= 1;
            }
        }
        assert!(
            edges.values().all(|balance| *balance == 0),
            "hull surface is not closed"
        );

        // Convex: no input point is outside any face.
        for point in &points {
            for polygon in &hull {
                assert!(
                    polygon.plane.distance(*point) <= 1e-9 * 10.0,
                    "point escapes the hull"
                );
            }
        }

        // Outward normals give a positive volume, bounded by the sphere.
        let volume = hull_volume(&hull);
        assert!(volume > 0.0);
        assert!(volume < 4.0 / 3.0 * std::f64::consts::PI * 1000.0 + 1e-6);
        assert!(volume > 0.9 * 4.0 / 3.0 * std::f64::consts::PI * 1000.0);
    }

    #[test]
    fn tolerances_survive_extreme_scales() {
        for size in [0.01, 0.5, 1.0, 1000.0, 100_000.0] {
            let hull = convex_hull(&cube_corners(size));
            assert_eq!(hull.len(), 6, "size {}", size);
            for polygon in &hull {
                assert_eq!(polygon.vertices.len(), 4, "size {}", size);
            }
            let expected = size * size * size;
            assert!(
                (hull_volume(&hull) - expected).abs() <= expected * 1e-9,
                "size {} gave volume {}",
                size,
                hull_volume(&hull)
            );
        }
    }

    #[test]
    fn octahedron_faces_are_triangles() {
        let points = vec![
            Vec3::new(5.0, 0.0, 0.0),
            Vec3::new(-5.0, 0.0, 0.0),
            Vec3::new(0.0, 5.0, 0.0),
            Vec3::new(0.0, -5.0, 0.0),
            Vec3::new(0.0, 0.0, 5.0),
            Vec3::new(0.0, 0.0, -5.0),
        ];
        let hull = convex_hull(&points);
        assert_eq!(hull.len(), 8);
        for polygon in &hull {
            assert_eq!(polygon.vertices.len(), 3);
        }
        // Volume of a regular octahedron with "radius" 5 is 4/3 * 5^3.
        assert!((hull_volume(&hull) - 4.0 / 3.0 * 125.0).abs() < 1e-9);
    }
}
