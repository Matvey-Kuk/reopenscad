//! Export-time sliver removal.
//!
//! The kernel's booleans cut faces exactly along the planes of the other
//! operand, so wherever two construction edges are collinear or coincident the
//! final triangulation can end up with a *cap* (a vertex lying on, or within a
//! hair of, the opposite edge: zero area, two long edges) or a *needle* (one
//! edge far shorter than the others). Both are harmless to the solid — the
//! mesh is still closed — but reviewers flag them and some slicers warn.
//!
//! [`remove_slivers`] fixes them without moving the surface by more than the
//! sliver's own altitude:
//!
//! * a **needle** is removed by collapsing its short edge into one of its
//!   endpoints. The surviving vertex keeps its exact coordinates, so the only
//!   geometry that moves is the collapsed vertex, by less than the edge length.
//!   The collapse is refused when it would break the 2-manifold (the classic
//!   link condition) or turn a well-shaped neighbour inside out.
//! * a **cap** is removed by flipping its longest edge. The cap vertex lies
//!   within `altitude` of that edge, which also bounds the edge of the
//!   neighbouring triangle, so the two replacement triangles cover the same
//!   surface to within that altitude. The flip is refused unless both new
//!   triangles keep the neighbour's orientation and the pair's worst altitude
//!   strictly improves, which is also what guarantees termination.
//!
//! Topology is maintained throughout: every operation preserves the closed
//! 2-manifold, so a watertight input stays watertight and the Euler
//! characteristic is unchanged.

use super::mesh::{FastMap, IndexedMesh, Vec3, VertexWelder};

/// Triangles whose smallest altitude is below this are slivers, in mm.
///
/// One micrometre: two orders of magnitude below any printer's resolution, and
/// the threshold the review tooling that reported the problem used.
pub const SLIVER_ALTITUDE: f64 = 1.0e-3;

/// Repair passes before giving up. Each pass fixes every independent sliver,
/// so real meshes settle in two or three; the cap only guards pathological
/// inputs against running forever.
const MAX_PASSES: usize = 24;

/// What a cleanup did, for tests and diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub collapses: usize,
    pub flips: usize,
    /// Slivers still present when no further repair applied.
    pub remaining: usize,
}

/// Smallest altitude of a triangle: twice its area over its longest edge.
pub fn min_altitude(a: Vec3, b: Vec3, c: Vec3) -> f64 {
    let doubled_area = b.sub(a).cross(c.sub(a)).length();
    let longest = b
        .sub(a)
        .length()
        .max(c.sub(b).length())
        .max(a.sub(c).length());
    if longest <= 0.0 {
        return 0.0;
    }
    doubled_area / longest
}

/// Weld a triangle soup with the kernel's tolerance, dropping facets that
/// collapse to a line or a point.
pub fn weld_triangles(triangles: impl IntoIterator<Item = [Vec3; 3]>) -> IndexedMesh {
    weld_triangles_tracked(triangles).0
}

/// [`weld_triangles`], plus the input index of every triangle it kept, in
/// output order: what per-triangle data (colours) needs to follow the weld.
pub fn weld_triangles_tracked(
    triangles: impl IntoIterator<Item = [Vec3; 3]>,
) -> (IndexedMesh, Vec<usize>) {
    let mut welder = VertexWelder::default();
    let mut indexed = Vec::new();
    let mut kept = Vec::new();
    for (index, corners) in triangles.into_iter().enumerate() {
        if !corners.iter().all(|point| point.is_finite()) {
            continue;
        }
        let indices = corners.map(|point| welder.insert(point));
        if indices[0] == indices[1] || indices[1] == indices[2] || indices[0] == indices[2] {
            continue;
        }
        indexed.push(indices);
        kept.push(index);
    }
    let mesh = IndexedMesh {
        vertices: welder.vertices,
        triangles: indexed,
    };
    (mesh, kept)
}

struct Repair<'a> {
    vertices: &'a [Vec3],
    triangles: Vec<[u32; 3]>,
    alive: Vec<bool>,
    dirty: Vec<bool>,
    /// Directed edge -> triangle, `None` when the edge is used twice in the
    /// same direction (a non-manifold input), which disables repairs there.
    half_edges: FastMap<(u32, u32), Option<u32>>,
    vertex_triangles: Vec<Vec<u32>>,
    /// Per-triangle group (a colour slot), or empty. A flip never pairs two
    /// triangles of different groups: re-splitting their quad would move area
    /// from one colour to the other across the seam.
    groups: Vec<u16>,
}

impl<'a> Repair<'a> {
    fn new(vertices: &'a [Vec3], triangles: Vec<[u32; 3]>) -> Self {
        let count = triangles.len();
        let mut repair = Self {
            vertices,
            triangles,
            alive: vec![true; count],
            dirty: vec![false; count],
            half_edges: FastMap::default(),
            vertex_triangles: Vec::new(),
            groups: Vec::new(),
        };
        repair.index();
        repair
    }

    fn index(&mut self) {
        self.half_edges.clear();
        self.vertex_triangles = vec![Vec::new(); self.vertices.len()];
        for (index, triangle) in self.triangles.iter().enumerate() {
            if !self.alive[index] {
                continue;
            }
            for corner in 0..3 {
                let key = (triangle[corner], triangle[(corner + 1) % 3]);
                self.half_edges
                    .entry(key)
                    .and_modify(|slot| *slot = None)
                    .or_insert(Some(index as u32));
                self.vertex_triangles[triangle[corner] as usize].push(index as u32);
            }
        }
        self.dirty.iter_mut().for_each(|flag| *flag = false);
    }

    fn points(&self, triangle: [u32; 3]) -> [Vec3; 3] {
        triangle.map(|index| self.vertices[index as usize])
    }

    fn altitude(&self, triangle: [u32; 3]) -> f64 {
        let [a, b, c] = self.points(triangle);
        min_altitude(a, b, c)
    }

    fn normal(&self, triangle: [u32; 3]) -> Vec3 {
        let [a, b, c] = self.points(triangle);
        b.sub(a).cross(c.sub(a))
    }

    fn slivers(&self, tolerance: f64) -> Vec<(f64, u32)> {
        let mut found: Vec<(f64, u32)> = self
            .triangles
            .iter()
            .enumerate()
            .filter(|(index, _)| self.alive[*index])
            .map(|(index, triangle)| (self.altitude(*triangle), index as u32))
            .filter(|(altitude, _)| *altitude < tolerance)
            .collect();
        found.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.cmp(&right.1)));
        found
    }

    /// Collapse vertex `from` onto vertex `onto`. `from`'s position is dropped.
    fn collapse(&mut self, from: u32, onto: u32, tolerance: f64) -> bool {
        let around_from = &self.vertex_triangles[from as usize];
        let around_onto = &self.vertex_triangles[onto as usize];
        if around_from
            .iter()
            .chain(around_onto.iter())
            .any(|triangle| self.dirty[*triangle as usize] || !self.alive[*triangle as usize])
        {
            return false;
        }
        let shared: Vec<u32> = around_from
            .iter()
            .copied()
            .filter(|triangle| self.triangles[*triangle as usize].contains(&onto))
            .collect();
        if shared.len() != 2 {
            return false;
        }
        // Link condition: the only vertices adjacent to both ends are the two
        // apexes of the triangles on the edge. Anything else would pinch the
        // surface into a non-manifold vertex or a doubled face.
        let apexes: Vec<u32> = shared
            .iter()
            .filter_map(|triangle| {
                self.triangles[*triangle as usize]
                    .iter()
                    .copied()
                    .find(|vertex| *vertex != from && *vertex != onto)
            })
            .collect();
        if apexes.len() != 2 || apexes[0] == apexes[1] {
            return false;
        }
        let neighbours = |list: &[u32], skip: u32| {
            let mut set: Vec<u32> = list
                .iter()
                .flat_map(|triangle| self.triangles[*triangle as usize])
                .filter(|vertex| *vertex != skip)
                .collect();
            set.sort_unstable();
            set.dedup();
            set
        };
        let from_ring = neighbours(around_from, from);
        let onto_ring = neighbours(around_onto, onto);
        let common = from_ring
            .iter()
            .filter(|vertex| **vertex != onto && onto_ring.binary_search(vertex).is_ok())
            .count();
        if common != 2 {
            return false;
        }
        // No well-shaped neighbour may turn over.
        let mut updates = Vec::new();
        for triangle in around_from.iter().copied() {
            if shared.contains(&triangle) {
                continue;
            }
            let old = self.triangles[triangle as usize];
            let new = old.map(|vertex| if vertex == from { onto } else { vertex });
            let (old_normal, new_normal) = (self.normal(old), self.normal(new));
            if self.altitude(old) >= tolerance && old_normal.dot(new_normal) <= 0.0 {
                return false;
            }
            updates.push((triangle, new));
        }
        for triangle in shared {
            self.alive[triangle as usize] = false;
        }
        for (triangle, new) in updates {
            self.triangles[triangle as usize] = new;
        }
        let touched: Vec<u32> = self.vertex_triangles[from as usize]
            .iter()
            .chain(self.vertex_triangles[onto as usize].iter())
            .copied()
            .collect();
        for triangle in touched {
            self.dirty[triangle as usize] = true;
        }
        true
    }

    /// Flip the edge `a -> b` of triangle `t = (a, b, c)`.
    fn flip(&mut self, triangle: u32, corner: usize) -> bool {
        let t = self.triangles[triangle as usize];
        let (a, b, c) = (t[corner], t[(corner + 1) % 3], t[(corner + 2) % 3]);
        let Some(Some(neighbour)) = self.half_edges.get(&(b, a)).copied() else {
            return false;
        };
        if neighbour == triangle
            || self
                .groups
                .get(triangle as usize)
                .is_some_and(|group| Some(group) != self.groups.get(neighbour as usize))
            || self.dirty[neighbour as usize]
            || !self.alive[neighbour as usize]
            || self.dirty[triangle as usize]
        {
            return false;
        }
        let other = self.triangles[neighbour as usize];
        let Some(d) = other.iter().copied().find(|vertex| *vertex != a && *vertex != b) else {
            return false;
        };
        if d == c || self.half_edges.contains_key(&(c, d)) || self.half_edges.contains_key(&(d, c))
        {
            return false;
        }
        let first = [a, d, c];
        let second = [d, b, c];
        let reference = self.normal(other);
        if self.normal(first).dot(reference) <= 0.0 || self.normal(second).dot(reference) <= 0.0 {
            return false;
        }
        let before = self.altitude(t).min(self.altitude(other));
        let after = self.altitude(first).min(self.altitude(second));
        if after <= before {
            return false;
        }
        self.triangles[triangle as usize] = first;
        self.triangles[neighbour as usize] = second;
        // Record the new diagonal so a later flip in this pass cannot create
        // the same edge a second time.
        self.half_edges.insert((d, c), Some(triangle));
        self.half_edges.insert((c, d), Some(neighbour));
        self.dirty[triangle as usize] = true;
        self.dirty[neighbour as usize] = true;
        true
    }

    fn repair(&mut self, triangle: u32, tolerance: f64) -> Option<bool> {
        if self.dirty[triangle as usize] || !self.alive[triangle as usize] {
            return None;
        }
        let t = self.triangles[triangle as usize];
        let points = self.points(t);
        let lengths: [f64; 3] =
            std::array::from_fn(|corner| points[(corner + 1) % 3].sub(points[corner]).length());
        let shortest = (0..3)
            .min_by(|left, right| lengths[*left].total_cmp(&lengths[*right]))
            .unwrap_or(0);
        if lengths[shortest] < tolerance {
            let (u, v) = (t[shortest], t[(shortest + 1) % 3]);
            if self.collapse(u, v, tolerance) || self.collapse(v, u, tolerance) {
                return Some(true);
            }
        }
        // Longest edge first: for a cap that is the only flip that helps. A
        // needle whose short edge is real detail can still often be fixed by
        // flipping one of its long edges, which `flip` accepts only when the
        // pair's worst altitude improves.
        let mut order = [0usize, 1, 2];
        order.sort_by(|left, right| lengths[*right].total_cmp(&lengths[*left]));
        if order.into_iter().any(|corner| self.flip(triangle, corner)) {
            return Some(false);
        }
        None
    }
}

/// Remove sliver triangles from a closed indexed mesh, in place.
///
/// Vertex positions are never changed; collapsed vertices simply stop being
/// referenced and are compacted away.
pub fn remove_slivers(mesh: &mut IndexedMesh, tolerance: f64) -> CleanupReport {
    remove_slivers_tracked(mesh, tolerance).0
}

/// [`remove_slivers`], plus the input index of every triangle that survives,
/// in output order. A repair rewrites triangles in place, so a survivor keeps
/// the slot — and whatever per-triangle data rides on it — it started in.
pub fn remove_slivers_tracked(mesh: &mut IndexedMesh, tolerance: f64) -> (CleanupReport, Vec<usize>) {
    remove_slivers_grouped(mesh, tolerance, &[])
}

/// [`remove_slivers_tracked`] that never flips an edge between triangles of
/// different `groups` (one per triangle, or empty for no constraint).
pub fn remove_slivers_grouped(
    mesh: &mut IndexedMesh,
    tolerance: f64,
    groups: &[u16],
) -> (CleanupReport, Vec<usize>) {
    let mut report = CleanupReport::default();
    let triangles = std::mem::take(&mut mesh.triangles);
    let mut repair = Repair::new(&mesh.vertices, triangles);
    if groups.len() == repair.triangles.len() {
        repair.groups = groups.to_vec();
    }
    for _ in 0..MAX_PASSES {
        let slivers = repair.slivers(tolerance);
        report.remaining = slivers.len();
        if slivers.is_empty() {
            break;
        }
        let mut progressed = false;
        for (_, triangle) in slivers {
            match repair.repair(triangle, tolerance) {
                Some(true) => report.collapses += 1,
                Some(false) => report.flips += 1,
                None => continue,
            }
            progressed = true;
        }
        repair.index();
        if !progressed {
            break;
        }
    }
    report.remaining = repair.slivers(tolerance).len();

    let Repair {
        triangles, alive, ..
    } = repair;
    let mut remap = vec![u32::MAX; mesh.vertices.len()];
    let mut vertices = Vec::new();
    let mut kept = Vec::with_capacity(triangles.len());
    let mut sources = Vec::with_capacity(triangles.len());
    for (index, (triangle, alive)) in triangles.into_iter().zip(alive).enumerate() {
        if !alive {
            continue;
        }
        sources.push(index);
        let mapped = triangle.map(|vertex| {
            let slot = &mut remap[vertex as usize];
            if *slot == u32::MAX {
                *slot = vertices.len() as u32;
                vertices.push(mesh.vertices[vertex as usize]);
            }
            *slot
        });
        kept.push(mapped);
    }
    mesh.vertices = vertices;
    mesh.triangles = kept;
    (report, sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quad_box_with_cap() -> IndexedMesh {
        // A unit cube whose top face is split by a vertex sitting on the
        // middle of one of its edges: the top's two triangles plus a
        // zero-area cap along that edge, mirrored on the side face.
        let v = |x: f64, y: f64, z: f64| Vec3::new(x, y, z);
        let corners = [
            v(0.0, 0.0, 0.0),
            v(1.0, 0.0, 0.0),
            v(1.0, 1.0, 0.0),
            v(0.0, 1.0, 0.0),
            v(0.0, 0.0, 1.0),
            v(1.0, 0.0, 1.0),
            v(1.0, 1.0, 1.0),
            v(0.0, 1.0, 1.0),
        ];
        let mut triangles = Vec::new();
        let quad = |triangles: &mut Vec<[Vec3; 3]>, a: usize, b: usize, c: usize, d: usize| {
            triangles.push([corners[a], corners[b], corners[c]]);
            triangles.push([corners[a], corners[c], corners[d]]);
        };
        quad(&mut triangles, 0, 3, 2, 1); // bottom
        quad(&mut triangles, 1, 2, 6, 5); // +x
        quad(&mut triangles, 2, 3, 7, 6); // +y
        quad(&mut triangles, 3, 0, 4, 7); // -x
        // Top split at m = midpoint of 4-5, with a fan from 7 and 6.
        let m = v(0.5, 0.0, 1.0);
        triangles.push([corners[4], m, corners[7]]);
        triangles.push([m, corners[6], corners[7]]);
        triangles.push([m, corners[5], corners[6]]);
        // Front (-y) face keeps its original two triangles plus the cap
        // (4, 5, m) that closes the T-vertex: zero area.
        triangles.push([corners[0], corners[1], corners[5]]);
        triangles.push([corners[0], corners[5], corners[4]]);
        triangles.push([corners[4], corners[5], m]);
        weld_triangles(triangles)
    }

    #[test]
    fn a_zero_area_cap_is_flipped_away_and_the_box_stays_closed() {
        let mut mesh = quad_box_with_cap();
        assert!(mesh.is_manifold(), "{}", mesh.statistics());
        let volume = mesh.volume();
        let report = remove_slivers(&mut mesh, SLIVER_ALTITUDE);
        assert_eq!(report.remaining, 0, "{report:?}");
        assert!(mesh.is_manifold(), "{}", mesh.statistics());
        assert!((mesh.volume() - volume).abs() < 1e-12);
        for index in 0..mesh.triangles.len() {
            let [a, b, c] = mesh.triangle_points(index);
            assert!(min_altitude(a, b, c) >= SLIVER_ALTITUDE);
        }
    }

    #[test]
    fn a_needle_edge_is_collapsed() {
        // A tetrahedron with one vertex split into two 1e-5 apart (an extra
        // needle-shaped pair of triangles).
        let v = |x: f64, y: f64, z: f64| Vec3::new(x, y, z);
        let a = v(0.0, 0.0, 0.0);
        let b = v(10.0, 0.0, 0.0);
        let c = v(0.0, 10.0, 0.0);
        let d = v(0.0, 0.0, 10.0);
        let e = v(10.0, 0.0, 0.0002); // near b, beyond weld tolerance
        let triangles = vec![
            [a, c, b],
            [a, e, d],
            [a, b, e],
            [b, c, e],
            [e, c, d],
            [a, d, c],
        ];
        let mut mesh = weld_triangles(triangles);
        assert!(mesh.is_manifold(), "{}", mesh.statistics());
        let report = remove_slivers(&mut mesh, SLIVER_ALTITUDE);
        assert!(report.collapses >= 1, "{report:?}");
        assert_eq!(report.remaining, 0);
        assert!(mesh.is_manifold(), "{}", mesh.statistics());
        assert_eq!(mesh.triangles.len(), 4);
    }
}
