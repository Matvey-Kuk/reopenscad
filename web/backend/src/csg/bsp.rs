//! Binary space partitioning tree used to evaluate exact boolean operations.
//!
//! The tree stores the polygons of one solid; each node owns a splitting plane
//! and the polygons coplanar with it. Classifying a polygon of the *other*
//! solid against the tree tells us exactly whether it lies inside or outside,
//! and polygons that span a plane are split along the exact intersection line.
//! Because every split is an exact plane/plane intersection, sharp edges are
//! preserved to the last representable bit — there is no grid anywhere.
//!
//! # Robustness and coplanar faces
//!
//! Vertex/plane classification uses the absolute [`EPSILON`] from
//! [`super::mesh`]. A vertex within `EPSILON` of the plane counts as
//! *coplanar*, so slivers thinner than the tolerance are never created.
//!
//! Coplanar faces are the classic failure mode. They are handled explicitly:
//! a polygon that lies in the node plane is filed as *coplanar-front* when its
//! own normal agrees with the node plane and *coplanar-back* when it opposes.
//! Clipping then routes coplanar-front polygons down the front subtree and
//! coplanar-back polygons down the back subtree. That single rule makes
//! touching faces behave correctly for all three operations:
//!
//! * `union` keeps one copy of a shared face (the second copy is clipped away
//!   by the double `clip_to`/`invert` pass),
//! * `difference` removes a face that is exactly cancelled by an opposing
//!   face of the tool,
//! * `intersection` keeps a shared face exactly once.
//!
//! The tree is built and traversed with explicit work stacks rather than
//! recursion so that deep trees from large models cannot overflow the stack.

use super::mesh::{Plane, Polygon, Vec3, EPSILON};
use super::prof;

const COPLANAR: u8 = 0;
const FRONT: u8 = 1;
const BACK: u8 = 2;
const SPANNING: u8 = 3;

/// Number of candidate planes sampled when choosing a node split.
const SPLIT_CANDIDATES: usize = 12;

#[derive(Clone, Debug, Default)]
struct BspNode {
    plane: Option<Plane>,
    front: Option<usize>,
    back: Option<usize>,
    polygons: Vec<Polygon>,
}

/// A BSP tree over one solid's polygons.
#[derive(Clone, Debug)]
pub struct Bsp {
    nodes: Vec<BspNode>,
}

impl Default for Bsp {
    fn default() -> Self {
        Self::new()
    }
}

impl Bsp {
    pub fn new() -> Self {
        Self {
            nodes: vec![BspNode::default()],
        }
    }

    pub fn from_polygons(polygons: Vec<Polygon>) -> Self {
        let mut tree = Self::new();
        prof::count("bsp:input_polygons", polygons.len() as u64);
        prof::scope("bsp:build", || tree.build(polygons));
        tree
    }

    fn allocate(&mut self) -> usize {
        self.nodes.push(BspNode::default());
        self.nodes.len() - 1
    }

    /// Adds polygons to the tree, creating nodes as needed.
    pub fn build(&mut self, polygons: Vec<Polygon>) {
        if polygons.is_empty() {
            return;
        }
        let mut work = vec![(0usize, polygons)];
        // The coplanar buckets are drained into the node before the next
        // iteration, so one pair of buffers serves the whole build; only the
        // front/back lists have to outlive the step, because they go on the
        // work stack.
        let mut coplanar_front = Vec::new();
        let mut coplanar_back = Vec::new();
        while let Some((index, polygons)) = work.pop() {
            if polygons.is_empty() {
                continue;
            }
            if self.nodes[index].plane.is_none() {
                self.nodes[index].plane = Some(choose_plane(&polygons));
            }
            let plane = self.nodes[index].plane.expect("plane was just assigned");
            let mut front = Vec::new();
            let mut back = Vec::new();
            for polygon in polygons {
                split_polygon_owned(
                    plane,
                    polygon,
                    &mut coplanar_front,
                    &mut coplanar_back,
                    &mut front,
                    &mut back,
                );
            }
            self.nodes[index].polygons.extend(coplanar_front.drain(..));
            self.nodes[index].polygons.extend(coplanar_back.drain(..));
            if !front.is_empty() {
                let child = match self.nodes[index].front {
                    Some(child) => child,
                    None => {
                        let child = self.allocate();
                        self.nodes[index].front = Some(child);
                        child
                    }
                };
                work.push((child, front));
            }
            if !back.is_empty() {
                let child = match self.nodes[index].back {
                    Some(child) => child,
                    None => {
                        let child = self.allocate();
                        self.nodes[index].back = Some(child);
                        child
                    }
                };
                work.push((child, back));
            }
        }
    }

    /// Turns the solid inside out: every polygon and plane is reversed and the
    /// front/back subtrees are swapped.
    pub fn invert(&mut self) {
        for node in &mut self.nodes {
            for polygon in &mut node.polygons {
                polygon.flip();
            }
            if let Some(plane) = node.plane {
                node.plane = Some(plane.flipped());
            }
            std::mem::swap(&mut node.front, &mut node.back);
        }
    }

    /// Removes the parts of `polygons` that lie inside this solid.
    pub fn clip_polygons(&self, polygons: Vec<Polygon>) -> Vec<Polygon> {
        let mut work = Vec::new();
        self.clip_polygons_with(polygons, &mut work)
    }

    /// [`Self::clip_polygons`] borrowing its traversal stack from the caller.
    ///
    /// `clip_to` runs this once per node of the tree being clipped, so the
    /// stack is allocated and thrown away thousands of times per boolean.
    fn clip_polygons_with(
        &self,
        polygons: Vec<Polygon>,
        work: &mut Vec<(usize, Vec<Polygon>)>,
    ) -> Vec<Polygon> {
        prof::count("bsp:clip_calls", 1);
        if polygons.is_empty() {
            return polygons;
        }
        let mut output = Vec::with_capacity(polygons.len());
        work.push((0usize, polygons));
        while let Some((index, polygons)) = work.pop() {
            if polygons.is_empty() {
                continue;
            }
            let node = &self.nodes[index];
            let Some(plane) = node.plane else {
                output.extend(polygons);
                continue;
            };
            let mut front = Vec::new();
            let mut back = Vec::new();
            let mut coplanar_front = Vec::new();
            let mut coplanar_back = Vec::new();
            for polygon in polygons {
                split_polygon_owned(
                    plane,
                    polygon,
                    &mut coplanar_front,
                    &mut coplanar_back,
                    &mut front,
                    &mut back,
                );
            }
            front.extend(coplanar_front);
            back.extend(coplanar_back);
            match node.front {
                Some(child) => work.push((child, front)),
                None => output.extend(front),
            }
            if let Some(child) = node.back {
                work.push((child, back));
            }
            // Without a back subtree the back half-space is solid, so those
            // fragments are inside this solid and get discarded.
        }
        output
    }

    /// Removes the parts of this solid that lie inside `other`.
    pub fn clip_to(&mut self, other: &Bsp) {
        prof::count("bsp:clip_to_nodes", self.nodes.len() as u64);
        let mut work = Vec::new();
        for index in 0..self.nodes.len() {
            let polygons = std::mem::take(&mut self.nodes[index].polygons);
            if polygons.is_empty() {
                continue;
            }
            self.nodes[index].polygons = other.clip_polygons_with(polygons, &mut work);
        }
    }

    pub fn all_polygons(&self) -> Vec<Polygon> {
        let mut output = Vec::with_capacity(self.polygon_count());
        for node in &self.nodes {
            output.extend(node.polygons.iter().cloned());
        }
        output
    }

    /// [`Self::all_polygons`] for a tree that is about to be dropped: the
    /// polygons are moved out instead of cloned, which is most of the boolean
    /// operations' allocation traffic.
    pub fn into_all_polygons(self) -> Vec<Polygon> {
        let mut output = Vec::with_capacity(self.polygon_count());
        for node in self.nodes {
            output.extend(node.polygons);
        }
        output
    }

    pub fn polygon_count(&self) -> usize {
        self.nodes.iter().map(|node| node.polygons.len()).sum()
    }

    /// Every solid leaf of the tree, as the list of half-spaces that define it.
    ///
    /// A path that descends into the *back* of a node and finds no back child
    /// has reached a region entirely inside the solid; the returned planes are
    /// oriented so that the cell is `{p : plane.distance(p) <= 0}` for each.
    /// Intersecting them yields a convex polytope, so the collection is an
    /// exact convex decomposition of the solid.
    pub fn solid_cells(&self) -> Vec<Vec<Plane>> {
        let mut cells = Vec::new();
        if self.nodes[0].plane.is_none() {
            return cells;
        }
        let mut work = vec![(0usize, Vec::<Plane>::new())];
        while let Some((index, constraints)) = work.pop() {
            let node = &self.nodes[index];
            let Some(plane) = node.plane else {
                continue;
            };
            // Front half-space: outside unless a subtree says otherwise.
            if let Some(child) = node.front {
                let mut branch = constraints.clone();
                branch.push(plane.flipped());
                work.push((child, branch));
            }
            let mut branch = constraints;
            branch.push(plane);
            match node.back {
                Some(child) => work.push((child, branch)),
                None => cells.push(branch),
            }
        }
        cells
    }
}

/// Picks a splitting plane that keeps the tree shallow.
///
/// Sampling a bounded number of candidates and scoring them by
/// `8 * splits + |front - back|` avoids the quadratic blow-up of always taking
/// the first polygon's plane, which matters because boolean inputs here are
/// frequently long axis-aligned runs of coplanar faces.
fn choose_plane(polygons: &[Polygon]) -> Plane {
    if polygons.len() <= 2 {
        return polygons[0].plane;
    }
    let stride = (polygons.len() / SPLIT_CANDIDATES).max(1);
    let mut best = polygons[0].plane;
    let mut best_score = f64::INFINITY;
    let mut index = 0;
    while index < polygons.len() {
        let plane = polygons[index].plane;
        let mut front = 0usize;
        let mut back = 0usize;
        let mut splits = 0usize;
        // Score against a bounded sample so this stays linear overall.
        let sample_stride = (polygons.len() / 64).max(1);
        let mut probe = 0;
        while probe < polygons.len() {
            match classify_polygon(plane, &polygons[probe]) {
                FRONT => front += 1,
                BACK => back += 1,
                SPANNING => splits += 1,
                _ => {}
            }
            probe += sample_stride;
        }
        let score = 8.0 * splits as f64 + (front as f64 - back as f64).abs();
        if score < best_score {
            best_score = score;
            best = plane;
        }
        index += stride;
    }
    best
}

fn classify_polygon(plane: Plane, polygon: &Polygon) -> u8 {
    let mut kind = 0u8;
    for vertex in &polygon.vertices {
        let distance = plane.distance(*vertex);
        kind |= if distance < -EPSILON {
            BACK
        } else if distance > EPSILON {
            FRONT
        } else {
            COPLANAR
        };
        // Once a polygon has vertices on both sides it is spanning, and no
        // further vertex can change that. `choose_plane` scores every candidate
        // against a sample of the node's polygons, so this loop runs an order
        // of magnitude more often than the split itself.
        if kind == SPANNING {
            break;
        }
    }
    kind
}

/// Splits `polygon` by `plane`, appending the results to the four buckets.
///
/// Coplanar polygons go to `coplanar_front` or `coplanar_back` depending on
/// whether their own normal agrees with the plane; that is what makes touching
/// faces resolve deterministically.
pub fn split_polygon(
    plane: Plane,
    polygon: &Polygon,
    coplanar_front: &mut Vec<Polygon>,
    coplanar_back: &mut Vec<Polygon>,
    front: &mut Vec<Polygon>,
    back: &mut Vec<Polygon>,
) {
    split_polygon_owned(
        plane,
        polygon.clone(),
        coplanar_front,
        coplanar_back,
        front,
        back,
    );
}

/// [`split_polygon`] taking ownership.
///
/// Three of the four outcomes file the polygon unchanged, so taking it by value
/// lets those cases move it into the bucket instead of cloning its vertex
/// vector. The tree build and every clip pass hit this path millions of times
/// on a large model, and the clones were the kernel's single largest source of
/// allocator traffic.
pub fn split_polygon_owned(
    plane: Plane,
    polygon: Polygon,
    coplanar_front: &mut Vec<Polygon>,
    coplanar_back: &mut Vec<Polygon>,
    front: &mut Vec<Polygon>,
    back: &mut Vec<Polygon>,
) {
    let count = polygon.vertices.len();
    // Classification is per-vertex scratch that dies with the call; almost
    // every polygon here is a triangle or a quad, so it lives on the stack.
    let mut inline = [0u8; 32];
    let mut spilled: Vec<u8> = Vec::new();
    let kinds: &mut [u8] = if count <= inline.len() {
        &mut inline[..count]
    } else {
        spilled.resize(count, 0);
        &mut spilled[..]
    };
    let mut polygon_kind = 0u8;
    for (slot, vertex) in polygon.vertices.iter().enumerate() {
        let distance = plane.distance(*vertex);
        let kind = if distance < -EPSILON {
            BACK
        } else if distance > EPSILON {
            FRONT
        } else {
            COPLANAR
        };
        polygon_kind |= kind;
        kinds[slot] = kind;
    }

    match polygon_kind {
        COPLANAR => {
            if plane.normal.dot(polygon.plane.normal) > 0.0 {
                coplanar_front.push(polygon);
            } else {
                coplanar_back.push(polygon);
            }
        }
        FRONT => front.push(polygon),
        BACK => back.push(polygon),
        _ => {
            let mut front_vertices: Vec<Vec3> = Vec::with_capacity(count + 1);
            let mut back_vertices: Vec<Vec3> = Vec::with_capacity(count + 1);
            for index in 0..count {
                let next = (index + 1) % count;
                let current_kind = kinds[index];
                let next_kind = kinds[next];
                let current = polygon.vertices[index];
                let next_vertex = polygon.vertices[next];
                if current_kind != BACK {
                    front_vertices.push(current);
                }
                if current_kind != FRONT {
                    back_vertices.push(current);
                }
                if (current_kind | next_kind) == SPANNING {
                    let numerator = plane.offset - plane.normal.dot(current);
                    let denominator = plane.normal.dot(next_vertex.sub(current));
                    if denominator != 0.0 {
                        let t = numerator / denominator;
                        let crossing = current.lerp(next_vertex, t.clamp(0.0, 1.0));
                        front_vertices.push(crossing);
                        back_vertices.push(crossing);
                    }
                }
            }
            push_fragment(front_vertices, polygon.plane, front);
            push_fragment(back_vertices, polygon.plane, back);
        }
    }
}

fn push_fragment(vertices: Vec<Vec3>, plane: Plane, output: &mut Vec<Polygon>) {
    if vertices.len() < 3 {
        return;
    }
    let mut fragment = Polygon::with_plane(vertices, plane);
    // A split can produce a zero-area sliver when the cut grazes a corner;
    // dropping it here keeps the boolean output free of degenerate facets.
    if !fragment.simplify(EPSILON) {
        return;
    }
    if fragment.area() <= EPSILON {
        return;
    }
    output.push(fragment);
}

/// `a` union `b`.
pub fn union(a: Vec<Polygon>, b: Vec<Polygon>) -> Vec<Polygon> {
    prof::count("bsp:op_union", 1);
    let mut left = Bsp::from_polygons(a);
    let mut right = Bsp::from_polygons(b);
    left.clip_to(&right);
    right.clip_to(&left);
    right.invert();
    right.clip_to(&left);
    right.invert();
    left.build(right.into_all_polygons());
    left.into_all_polygons()
}

/// `a` minus `b`.
pub fn difference(a: Vec<Polygon>, b: Vec<Polygon>) -> Vec<Polygon> {
    prof::count("bsp:op_difference", 1);
    let mut left = Bsp::from_polygons(a);
    let mut right = Bsp::from_polygons(b);
    left.invert();
    left.clip_to(&right);
    right.clip_to(&left);
    right.invert();
    right.clip_to(&left);
    right.invert();
    left.build(right.into_all_polygons());
    left.invert();
    left.into_all_polygons()
}

/// `a` intersected with `b`.
pub fn intersection(a: Vec<Polygon>, b: Vec<Polygon>) -> Vec<Polygon> {
    prof::count("bsp:op_intersection", 1);
    let mut left = Bsp::from_polygons(a);
    let mut right = Bsp::from_polygons(b);
    left.invert();
    right.clip_to(&left);
    right.invert();
    left.clip_to(&right);
    right.clip_to(&left);
    left.build(right.into_all_polygons());
    left.invert();
    left.into_all_polygons()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csg::primitives;

    fn volume_of(polygons: &[Polygon]) -> f64 {
        let mut total = 0.0;
        for polygon in polygons {
            for index in 1..polygon.vertices.len() - 1 {
                let a = polygon.vertices[0];
                let b = polygon.vertices[index];
                let c = polygon.vertices[index + 1];
                total += a.dot(b.cross(c)) / 6.0;
            }
        }
        total
    }

    fn area_of(polygons: &[Polygon]) -> f64 {
        polygons.iter().map(Polygon::area).sum()
    }

    #[test]
    fn splitting_a_square_in_half_yields_two_halves() {
        let square = Polygon::new(vec![
            Vec3::new(-1.0, -1.0, 0.0),
            Vec3::new(1.0, -1.0, 0.0),
            Vec3::new(1.0, 1.0, 0.0),
            Vec3::new(-1.0, 1.0, 0.0),
        ])
        .unwrap();
        let plane = Plane::new(Vec3::new(1.0, 0.0, 0.0), 0.0);
        let (mut cf, mut cb, mut front, mut back) = (vec![], vec![], vec![], vec![]);
        split_polygon(plane, &square, &mut cf, &mut cb, &mut front, &mut back);
        assert!(cf.is_empty() && cb.is_empty());
        assert_eq!(front.len(), 1);
        assert_eq!(back.len(), 1);
        assert!((front[0].area() - 2.0).abs() < 1e-12);
        assert!((back[0].area() - 2.0).abs() < 1e-12);
    }

    #[test]
    fn a_coplanar_square_is_filed_by_orientation() {
        let square = Polygon::new(vec![
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(1.0, 1.0, 0.0),
            Vec3::new(0.0, 1.0, 0.0),
        ])
        .unwrap();
        let plane = square.plane;
        let (mut cf, mut cb, mut front, mut back) = (vec![], vec![], vec![], vec![]);
        split_polygon(plane, &square, &mut cf, &mut cb, &mut front, &mut back);
        assert_eq!(cf.len(), 1);
        assert!(cb.is_empty() && front.is_empty() && back.is_empty());

        let (mut cf, mut cb, mut front, mut back) = (vec![], vec![], vec![], vec![]);
        split_polygon(
            plane.flipped(),
            &square,
            &mut cf,
            &mut cb,
            &mut front,
            &mut back,
        );
        assert_eq!(cb.len(), 1);
        assert!(cf.is_empty() && front.is_empty() && back.is_empty());
    }

    #[test]
    fn union_of_two_disjoint_cubes_has_both_volumes() {
        let a = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false);
        let b = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false)
            .transformed(super::super::mesh::Matrix4::translation(Vec3::new(
                20.0, 0.0, 0.0,
            )));
        let result = union(a.polygons, b.polygons);
        assert!((volume_of(&result) - 2000.0).abs() < 1e-9, "{}", volume_of(&result));
        assert!((area_of(&result) - 1200.0).abs() < 1e-9);
    }

    #[test]
    fn union_of_two_identical_cubes_is_one_cube() {
        let a = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false);
        let b = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false);
        let result = union(a.polygons, b.polygons);
        assert!((volume_of(&result) - 1000.0).abs() < 1e-9, "{}", volume_of(&result));
        assert!((area_of(&result) - 600.0).abs() < 1e-9, "{}", area_of(&result));
    }

    #[test]
    fn difference_of_overlapping_cubes_removes_the_overlap() {
        let a = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false);
        let b = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false)
            .transformed(super::super::mesh::Matrix4::translation(Vec3::new(
                5.0, 0.0, 0.0,
            )));
        let result = difference(a.polygons, b.polygons);
        assert!((volume_of(&result) - 500.0).abs() < 1e-9, "{}", volume_of(&result));
    }

    #[test]
    fn intersection_of_offset_cubes_is_the_overlap() {
        let a = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false);
        let b = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false)
            .transformed(super::super::mesh::Matrix4::translation(Vec3::new(
                5.0, 5.0, 0.0,
            )));
        let result = intersection(a.polygons, b.polygons);
        assert!((volume_of(&result) - 250.0).abs() < 1e-9, "{}", volume_of(&result));
    }

    #[test]
    fn face_touching_cubes_union_without_an_interior_wall() {
        let a = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false);
        let b = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false)
            .transformed(super::super::mesh::Matrix4::translation(Vec3::new(
                10.0, 0.0, 0.0,
            )));
        let result = union(a.polygons, b.polygons);
        assert!((volume_of(&result) - 2000.0).abs() < 1e-9, "{}", volume_of(&result));
        // 2000mm^3 box of 20x10x10 has 2*(200+100+200) = 1000mm^2 of surface.
        assert!((area_of(&result) - 1000.0).abs() < 1e-9, "{}", area_of(&result));
    }

    #[test]
    fn difference_with_a_shared_face_leaves_a_clean_cut() {
        let a = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false);
        let b = primitives::cube(Vec3::new(4.0, 4.0, 20.0), false)
            .transformed(super::super::mesh::Matrix4::translation(Vec3::new(
                3.0, 3.0, -5.0,
            )));
        let result = difference(a.polygons, b.polygons);
        assert!(
            (volume_of(&result) - (1000.0 - 160.0)).abs() < 1e-9,
            "{}",
            volume_of(&result)
        );
    }

    #[test]
    fn subtracting_a_disjoint_solid_changes_nothing() {
        let a = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false);
        let b = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false)
            .transformed(super::super::mesh::Matrix4::translation(Vec3::new(
                50.0, 0.0, 0.0,
            )));
        let result = difference(a.polygons, b.polygons);
        assert!((volume_of(&result) - 1000.0).abs() < 1e-9);
    }
}
