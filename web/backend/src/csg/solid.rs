//! `Solid`: a closed polygon soup, and the operations OpenSCAD performs on it.
//!
//! # Why faces are merged, and why only once
//!
//! A BSP boolean splits polygons by *infinite* planes, so a single flat wall of
//! the result can come back as dozens of coplanar fragments. Reference exports
//! from a Nef-polyhedron kernel contain the minimal facet set instead. To get
//! comparable facet counts (and a comparable Euler characteristic) the kernel
//! runs a face-merging pass — [`Solid::sealed`]:
//!
//! 1. weld vertices onto a lattice so coincident corners share one index,
//! 2. group polygons by their oriented plane,
//! 3. cancel every directed edge that appears in both directions inside a
//!    group — those are interior edges between two fragments,
//! 4. chain the surviving directed edges into boundary loops, keeping the
//!    interior on the left at every branch,
//! 5. drop vertices that are collinear in every face at once, and add back the
//!    vertices needed where a face's corner lands mid-edge on a face in
//!    another plane,
//! 6. classify loops into outlines and holes by signed area and containment,
//!    and re-triangulate only when a face actually has holes.
//!
//! Step 3 is the whole trick: it is exact integer bookkeeping on welded vertex
//! indices, so it neither invents nor loses geometry.
//!
//! The pass runs **once, on the way out**, and deliberately not after every
//! boolean. Merging is not free of consequence for what comes next: it welds
//! at a tolerance, it drops collinear vertices, and step 6 re-triangulates.
//! Each of those can leave the surface a fraction of a micron from closed —
//! harmless in an export, fatal as *input*, because a BSP decides "inside" by
//! ray-classification against the other solid's faces and a solid with a
//! pinhole in it has no reliable inside. Feeding merged solids back in was
//! measured doing exactly that: a union of two knuckles that touch face to
//! face returned less volume than either operand alone, and the tablet-box
//! fixture lost 0.84% of its material and came out with 87 boundary edges.
//! With the same booleans run on unmerged soup and the pass moved to the
//! export path, every hard-geometry fixture matches OpenSCAD's volume to seven
//! significant figures. See `tests/fixtures/hard-geometry/README.md`.
//!
//! The polygon count between booleans is higher as a result. That costs less
//! than the pass it replaces: merging was about a quarter of kernel runtime,
//! and the OpenAPPA corpus compiles no slower than it did.


use super::bsp;
use super::mesh::{
    dedup_ring, Bounds, FastMap, IndexedMesh, Matrix4, Plane, Polygon, Vec3, VertexWelder, EPSILON,
    PLANE_ANGLE_EPSILON, PLANE_OFFSET_EPSILON, WELD_EPSILON,
};
use super::poly2d::{triangulate_with_holes, Point2};
use super::prof;

/// Ceiling on the number of BSP cells [`Solid::convex_decomposition`] will
/// enumerate. A deeply concave solid can have thousands of them, and every
/// consumer of the decomposition is at least quadratic in the count.
pub const MAX_CONVEX_CELLS: usize = 64;

/// A closed, orientable boundary representation.
#[derive(Clone, Debug, Default)]
pub struct Solid {
    pub polygons: Vec<Polygon>,
}

impl Solid {
    pub fn from_faces(faces: Vec<Vec<Vec3>>) -> Self {
        let mut polygons = Vec::with_capacity(faces.len());
        for mut vertices in faces {
            dedup_ring(&mut vertices, EPSILON);
            if vertices.len() < 3 {
                continue;
            }
            if let Some(polygon) = Polygon::new(vertices) {
                polygons.push(polygon);
            }
        }
        Self { polygons }
    }

    pub fn is_empty(&self) -> bool {
        self.polygons.is_empty()
    }

    pub fn bounds(&self) -> Option<Bounds> {
        Bounds::from_points(
            self.polygons
                .iter()
                .flat_map(|polygon| polygon.vertices.iter().copied()),
        )
    }

    pub fn transformed(&self, matrix: Matrix4) -> Self {
        let mirrors = matrix.linear_determinant() < 0.0;
        let mut polygons = Vec::with_capacity(self.polygons.len());
        for polygon in &self.polygons {
            let mut vertices: Vec<Vec3> = polygon
                .vertices
                .iter()
                .map(|point| matrix.apply(*point))
                .collect();
            if mirrors {
                vertices.reverse();
            }
            dedup_ring(&mut vertices, EPSILON);
            if vertices.len() < 3 {
                continue;
            }
            if let Some(polygon) = Polygon::new(vertices) {
                polygons.push(polygon);
            }
        }
        Self { polygons }
    }

    pub fn flipped(&self) -> Self {
        Self {
            polygons: self.polygons.iter().map(Polygon::flipped).collect(),
        }
    }

    /// Union that short-circuits when the operands cannot touch: disjoint
    /// bounding boxes mean the result is simply both polygon sets.
    pub fn union(&self, other: &Self) -> Self {
        if self.is_empty() {
            return other.clone();
        }
        if other.is_empty() {
            return self.clone();
        }
        self.clone().into_union(other.clone())
    }

    pub fn difference(&self, other: &Self) -> Self {
        if self.is_empty() || other.is_empty() {
            return self.clone();
        }
        self.clone().into_difference(other.clone())
    }

    pub fn intersection(&self, other: &Self) -> Self {
        if self.is_empty() || other.is_empty() {
            return Self::default();
        }
        self.clone().into_intersection(other.clone())
    }

    /// [`Self::union`] for operands that are about to be dropped.
    ///
    /// The BSP consumes its input polygons, so a boolean between two owned
    /// solids never has to copy them. `union_all` and `intersection_all` fold
    /// thousands of temporaries, and the deep clones of their polygon lists
    /// were pure allocator traffic.
    pub fn into_union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        if let (Some(left), Some(right)) = (self.bounds(), other.bounds()) {
            if !left.overlaps(right, EPSILON) {
                let mut polygons = self.polygons;
                polygons.extend(other.polygons);
                return Self { polygons };
            }
        }
        Self {
            polygons: prof::scope("bool:union", || bsp::union(self.polygons, other.polygons)),
        }
    }

    /// [`Self::difference`] for operands that are about to be dropped.
    pub fn into_difference(self, other: Self) -> Self {
        if self.is_empty() || other.is_empty() {
            return self;
        }
        if let (Some(left), Some(right)) = (self.bounds(), other.bounds()) {
            if !left.overlaps(right, EPSILON) {
                return self;
            }
        }
        Self {
            polygons: prof::scope("bool:difference", || {
                bsp::difference(self.polygons, other.polygons)
            }),
        }
    }

    /// [`Self::intersection`] for operands that are about to be dropped.
    pub fn into_intersection(self, other: Self) -> Self {
        if self.is_empty() || other.is_empty() {
            return Self::default();
        }
        if let (Some(left), Some(right)) = (self.bounds(), other.bounds()) {
            if !left.overlaps(right, EPSILON) {
                return Self::default();
            }
        }
        Self {
            polygons: prof::scope("bool:intersection", || {
                bsp::intersection(self.polygons, other.polygons)
            }),
        }
    }

    /// Fan triangulation of every face; correct for convex and star-shaped
    /// faces, which is what the kernel produces after [`Self::sealed`].
    pub fn to_indexed_mesh(&self) -> IndexedMesh {
        let mut welder = VertexWelder::default();
        let mut triangles = Vec::new();
        for polygon in &self.polygons {
            if polygon.vertices.len() < 3 {
                continue;
            }
            let indices: Vec<u32> = polygon
                .vertices
                .iter()
                .map(|point| welder.insert(*point))
                .collect();
            // Faces reaching here are convex (the merging pass triangulates
            // anything that is not), so a fan is a valid triangulation — as
            // long as it starts at a vertex where the boundary actually turns.
            //
            // Which diagonal a fan picks for a quad is visible in the exported
            // facet set, and reference exports use a different one for roughly
            // half of them. Splitting along the shorter diagonal, and fanning
            // from the face's lowest in-plane vertex, were both measured
            // against the goldens and neither beat this: the reference
            // tessellator's choice is not a simple function of the face
            // geometry. Nothing downstream depends on the choice — area,
            // volume, and manifoldness are identical either way.
            let origin = corner_index(&polygon.vertices);
            let count = indices.len();
            let spans: Vec<[usize; 3]> = (1..count - 1)
                .map(|index| {
                    [
                        origin,
                        (origin + index) % count,
                        (origin + index + 1) % count,
                    ]
                })
                .collect();
            // A fan is only a triangulation if none of its spans is flat,
            // and starting at a turning vertex is not enough to guarantee
            // that: a run of collinear vertices *beginning* at the origin
            // leaves the first spans flat however the origin was chosen, and
            // a face can have such a run at each end. Those vertices are the
            // seams the T-junction repair just added, so a flat span is the
            // one carrying the two short edges along a seam. `drop_zero_area_
            // triangles` deletes it a few lines below — by the same test, on
            // purpose — and that hands the exporter one long edge where the
            // neighbouring face has two short ones, which is a hole.
            //
            // Ear clipping is conforming where the fan is not: it can only
            // clip at a turning corner, so a flat vertex survives to the end.
            // It is the fallback rather than the rule because it is O(n^2) and
            // the overwhelming majority of faces have no flat vertex at all.
            let flat = spans.iter().any(|span| {
                let [a, b, c] = span.map(|index| polygon.vertices[index]);
                b.sub(a).cross(c.sub(a)).length() <= 0.0
            });
            if flat {
                triangles.extend(triangulate_ring(&polygon.vertices, polygon.plane, &indices));
                continue;
            }
            for span in spans {
                let triangle = span.map(|index| indices[index]);
                if triangle[0] == triangle[1]
                    || triangle[1] == triangle[2]
                    || triangle[0] == triangle[2]
                {
                    continue;
                }
                triangles.push(triangle);
            }
        }
        let mut mesh = IndexedMesh {
            vertices: welder.vertices,
            triangles,
        };
        drop_zero_area_triangles(&mut mesh);
        mesh
    }

    /// Merges coplanar fragments back into whole faces, *without* the
    /// cross-plane T-junction repair. See the module docs.
    ///
    /// Only correct where the operand is known convex and the repair therefore
    /// has nothing to do — `hull()`, which is assembled by clipping a box and
    /// needs its clip fragments folded back together. Everything else wants
    /// [`Self::sealed`]; a merge that skips the repair can leave a seam open,
    /// and that is not a difference an exporter or a later boolean forgives.
    pub fn simplified(&self) -> Self {
        self.merge_faces(false)
    }

    /// [`Self::simplified`] plus the cross-plane T-junction repair.
    ///
    /// Closes seams that survive the merge because a vertex is a genuine
    /// corner of one face and sits mid-edge on a neighbouring one. This is the
    /// export path: [`crate::engine::exact::solid_to_mesh`] calls it, and the
    /// booleans no longer merge at all, so it is the only place a solid is
    /// merged.
    pub fn sealed(&self) -> Self {
        self.merge_faces(true)
    }

    fn merge_faces(&self, seal: bool) -> Self {
        prof::scope("merge:total", || self.merge_faces_inner(seal))
    }

    fn merge_faces_inner(&self, seal: bool) -> Self {
        if self.polygons.is_empty() {
            return Self::default();
        }
        let mut welder = VertexWelder::default();
        let mut planes: PlaneIndex = PlaneIndex::default();
        // Indexed by plane group id. `PlaneIndex` hands out ids densely from
        // zero, so this is a `BTreeMap<usize, _>` without the per-entry node:
        // iterating it visits the groups in the same ascending order.
        let mut groups: Vec<Vec<Vec<u32>>> = Vec::new();
        for polygon in &self.polygons {
            let mut ring: Vec<u32> = prof::scope("merge:weld", || {
                polygon
                    .vertices
                    .iter()
                    .map(|point| welder.insert(*point))
                    .collect()
            });
            dedup_indices(&mut ring);
            if ring.len() < 3 {
                continue;
            }
            let group = prof::scope("merge:plane_index", || planes.insert(polygon.plane));
            if group >= groups.len() {
                groups.resize_with(group + 1, Vec::new);
            }
            groups[group].push(ring);
        }

        let vertices = welder.vertices;
        let mut group_loops: Vec<(Plane, Vec<Vec<u32>>)> = Vec::with_capacity(groups.len());
        prof::scope("merge:extract_loops", || {
            for (group, rings) in groups.into_iter().enumerate() {
                if rings.is_empty() {
                    continue;
                }
                let plane = planes.planes[group];
                let loops = extract_loops_owned(plane, rings, &vertices);
                if !loops.is_empty() {
                    group_loops.push((plane, loops));
                }
            }
        });
        // Collinear vertices must be dropped from *every* face at once or not
        // at all; dropping them face by face is what leaves T-junctions.
        prof::scope("merge:collinear", || {
            remove_globally_collinear_vertices(&vertices, &mut group_loops)
        });
        if seal {
            // Faces in *different* planes can still meet along a partly
            // subdivided edge; those T-junctions leave boundary edges behind.
            insert_t_junction_vertices(&vertices, &mut group_loops);
        }

        let mut polygons = Vec::new();
        prof::scope("merge:emit_faces", || {
            for (plane, loops) in group_loops {
                emit_faces(plane, &loops, &vertices, &mut polygons);
            }
        });
        Self { polygons }
    }

    /// An exact decomposition into convex point sets, obtained from the solid
    /// leaves of a BSP tree. Used by `minkowski()`, which is only exact on
    /// convex operands.
    pub fn convex_decomposition(&self) -> Vec<Vec<Vec3>> {
        if self.polygons.is_empty() {
            return Vec::new();
        }
        if self.is_convex() {
            let mut points: Vec<Vec3> = self
                .polygons
                .iter()
                .flat_map(|polygon| polygon.vertices.iter().copied())
                .collect();
            deduplicate(&mut points);
            return vec![points];
        }
        let Some(bounds) = self.bounds() else {
            return Vec::new();
        };
        let tree = bsp::Bsp::from_polygons(self.polygons.clone());
        let cells = tree.solid_cells();
        if cells.len() > MAX_CONVEX_CELLS {
            return Vec::new();
        }
        let mut output = Vec::new();
        for constraints in cells {
            let mut cell = bounding_box_polygons(bounds, 1.0);
            for plane in &constraints {
                cell = clip_convex(&cell, *plane);
                if cell.is_empty() {
                    break;
                }
            }
            if cell.is_empty() {
                continue;
            }
            let mut points: Vec<Vec3> = cell
                .iter()
                .flat_map(|polygon| polygon.vertices.iter().copied())
                .collect();
            deduplicate(&mut points);
            if points.len() >= 4 {
                output.push(points);
            }
        }
        output
    }

    /// True when every vertex lies on or behind every face plane.
    pub fn is_convex(&self) -> bool {
        let scale = self
            .bounds()
            .map(|bounds| {
                let size = bounds.size();
                size.x.max(size.y).max(size.z).max(1.0)
            })
            .unwrap_or(1.0);
        let tolerance = scale * 1.0e-9;
        for polygon in &self.polygons {
            for other in &self.polygons {
                for vertex in &other.vertices {
                    if polygon.plane.distance(*vertex) > tolerance {
                        return false;
                    }
                }
            }
        }
        true
    }
}

/// Unions many solids with a balanced binary reduction.
///
/// Folding left to right costs `O(n^2)` because every step re-processes the
/// whole accumulated result; pairing neighbours and halving keeps the total
/// work near `O(n log n)`. Models built from grids of small parts — pixel-art
/// silhouettes, lattices — are otherwise unusable.
pub fn union_all(solids: Vec<Solid>) -> Solid {
    let mut level: Vec<Solid> = solids.into_iter().filter(|solid| !solid.is_empty()).collect();
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        let mut iterator = level.into_iter();
        while let Some(left) = iterator.next() {
            match iterator.next() {
                Some(right) => next.push(left.into_union(right)),
                None => next.push(left),
            }
        }
        level = next;
    }
    level.pop().unwrap_or_default()
}

/// Intersects many solids with the same balanced reduction.
pub fn intersection_all(solids: Vec<Solid>) -> Solid {
    if solids.iter().any(Solid::is_empty) {
        return Solid::default();
    }
    let mut level = solids;
    if level.is_empty() {
        return Solid::default();
    }
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        let mut iterator = level.into_iter();
        while let Some(left) = iterator.next() {
            match iterator.next() {
                Some(right) => next.push(left.into_intersection(right)),
                None => next.push(left),
            }
        }
        level = next;
    }
    level.pop().unwrap_or_default()
}

/// Triangulates one face's ring, keeping every vertex it was given.
///
/// Used where a fan would be flat. Projects onto the face plane, ear-clips,
/// and maps each corner back to the welded index it came from — by projected
/// coordinate, which is exact because the clipper only ever re-emits points it
/// was handed.
fn triangulate_ring(points: &[Vec3], plane: Plane, indices: &[u32]) -> Vec<[u32; 3]> {
    let (basis_u, basis_v) = plane_basis(plane);
    let projected: Vec<Point2> = points
        .iter()
        .map(|point| [point.dot(basis_u), point.dot(basis_v)])
        .collect();
    let mut lookup: FastMap<[u64; 2], u32> = FastMap::default();
    for (point, index) in projected.iter().zip(indices.iter()) {
        lookup.insert([point[0].to_bits(), point[1].to_bits()], *index);
    }
    let mut output = Vec::new();
    for triangle in triangulate_with_holes(&projected, &[]) {
        let mut resolved = [0u32; 3];
        let mut complete = true;
        for (slot, corner) in resolved.iter_mut().zip(triangle.iter()) {
            match lookup.get(&[corner[0].to_bits(), corner[1].to_bits()]) {
                Some(index) => *slot = *index,
                None => complete = false,
            }
        }
        if !complete
            || resolved[0] == resolved[1]
            || resolved[1] == resolved[2]
            || resolved[0] == resolved[2]
        {
            continue;
        }
        output.push(resolved);
    }
    output
}

/// The first vertex at which the ring genuinely turns, falling back to 0.
fn corner_index(points: &[Vec3]) -> usize {
    let count = points.len();
    for index in 0..count {
        let previous = points[(index + count - 1) % count];
        let current = points[index];
        let next = points[(index + 1) % count];
        let first = current.sub(previous);
        let second = next.sub(current);
        let base = first.length().max(second.length());
        if base > 0.0 && first.cross(second).length() > base * 1.0e-12 {
            return index;
        }
    }
    0
}

fn deduplicate(points: &mut Vec<Vec3>) {
    let mut welder = VertexWelder::default();
    for point in points.iter() {
        welder.insert(*point);
    }
    *points = welder.vertices;
}

fn dedup_indices(ring: &mut Vec<u32>) {
    // `Vec::dedup` drops exactly the runs the explicit copy loop dropped, in
    // place, saving one allocation per polygon of every merged solid.
    ring.dedup();
    while ring.len() >= 2 && ring[0] == ring[ring.len() - 1] {
        ring.pop();
    }
}

fn drop_zero_area_triangles(mesh: &mut IndexedMesh) {
    let vertices = &mesh.vertices;
    mesh.triangles.retain(|triangle| {
        let a = vertices[triangle[0] as usize];
        let b = vertices[triangle[1] as usize];
        let c = vertices[triangle[2] as usize];
        b.sub(a).cross(c.sub(a)).length() > 0.0
    });
}

/// Hash-bucketed index of oriented planes, so that fragments of one wall land
/// in the same group even when their computed normals differ in the last bits.
#[derive(Default)]
struct PlaneIndex {
    planes: Vec<Plane>,
    buckets: FastMap<[i64; 4], Vec<usize>>,
    /// Memo from the exact bits of a query plane to the group it resolved to.
    ///
    /// This is a pure short-cut, not a second matching rule. Once a query
    /// resolves to group `g`, it must keep resolving to `g`: a plane inserted
    /// later can only create a new group if it matched nothing already
    /// present, and a plane that fails to match `planes[g]` cannot match a
    /// query bit-identical to it either. So the set of groups a repeated query
    /// matches never changes, and neither does the first of them in probe
    /// order. Fragments of one wall carry a bit-identical plane through the
    /// BSP, so the memo answers nearly every lookup with one hash probe
    /// instead of eighty-one.
    memo: FastMap<[u64; 4], usize>,
}

impl PlaneIndex {
    fn exact_key(plane: Plane) -> [u64; 4] {
        [
            plane.normal.x.to_bits(),
            plane.normal.y.to_bits(),
            plane.normal.z.to_bits(),
            plane.offset.to_bits(),
        ]
    }

    fn key(plane: Plane) -> [i64; 4] {
        // Coarse cells (1e-4 on the unit normal, 1e-4 on the offset) keep the
        // bucket lists short; exact membership is decided by `Plane::same_as`.
        [
            (plane.normal.x * 1.0e4).round() as i64,
            (plane.normal.y * 1.0e4).round() as i64,
            (plane.normal.z * 1.0e4).round() as i64,
            (plane.offset * 1.0e4).round() as i64,
        ]
    }

    fn insert(&mut self, plane: Plane) -> usize {
        let exact = Self::exact_key(plane);
        if let Some(group) = self.memo.get(&exact) {
            return *group;
        }
        let group = self.search(plane);
        self.memo.insert(exact, group);
        group
    }

    fn search(&mut self, plane: Plane) -> usize {
        let key = Self::key(plane);
        for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    for dw in -1..=1 {
                        let probe = [key[0] + dx, key[1] + dy, key[2] + dz, key[3] + dw];
                        if let Some(candidates) = self.buckets.get(&probe) {
                            for candidate in candidates {
                                if self.planes[*candidate].same_as(plane) {
                                    return *candidate;
                                }
                            }
                        }
                    }
                }
            }
        }
        let index = self.planes.len();
        self.planes.push(plane);
        self.buckets.entry(key).or_default().push(index);
        index
    }
}

/// Orthonormal basis `(u, v)` of a plane with `u x v == normal`, so that a
/// counter-clockwise loop around `normal` is counter-clockwise in `(u, v)`.
pub fn plane_basis(plane: Plane) -> (Vec3, Vec3) {
    let normal = plane.normal;
    let helper = if normal.x.abs() < 0.9 {
        Vec3::new(1.0, 0.0, 0.0)
    } else {
        Vec3::new(0.0, 1.0, 0.0)
    };
    let u = normal.cross(helper).normalized();
    let v = normal.cross(u);
    (u, v)
}

/// The boundary loops of a set of coplanar polygons, in world space.
///
/// This is the reusable half of [`Solid::simplified`]: it cancels interior
/// edges and chains what is left into closed rings, without deciding how the
/// rings should be triangulated. `planar` uses it to read 2D contours back off
/// a lifted prism.
pub fn boundary_loops(polygons: &[Polygon], plane: Plane) -> Vec<Vec<Vec3>> {
    let mut welder = VertexWelder::default();
    let mut rings: Vec<Vec<u32>> = Vec::with_capacity(polygons.len());
    for polygon in polygons {
        let mut ring: Vec<u32> = polygon
            .vertices
            .iter()
            .map(|point| welder.insert(*point))
            .collect();
        dedup_indices(&mut ring);
        if ring.len() >= 3 {
            rings.push(ring);
        }
    }
    let vertices = welder.vertices;
    extract_loops(plane, &rings, &vertices)
        .into_iter()
        .map(|chain| {
            chain
                .into_iter()
                .map(|index| vertices[index as usize])
                .collect()
        })
        .collect()
}

/// Splits every edge of a coplanar group at the group's own vertices.
///
/// A BSP boolean only splits a polygon where a plane actually crosses it, so a
/// long face can sit next to two short ones and share only part of an edge.
/// Those T-junctions stop the directed-edge cancellation from firing (the long
/// edge never matches the short ones), which used to leave a wall of unmerged
/// strips behind. Subdividing first makes the cancellation exact.
fn subdivide_group_edges(rings: &mut [Vec<u32>], vertices: &[Vec3]) {
    prof::scope("merge:subdivide", || subdivide_group_edges_inner(rings, vertices))
}

fn subdivide_group_edges_inner(rings: &mut [Vec<u32>], vertices: &[Vec3]) {
    let mut group_vertices: Vec<u32> = rings.iter().flatten().copied().collect();
    group_vertices.sort_unstable();
    group_vertices.dedup();
    if group_vertices.len() < 3 {
        return;
    }
    let Some(bounds) = Bounds::from_points(group_vertices.iter().map(|i| vertices[*i as usize]))
    else {
        return;
    };
    let size = bounds.size();
    let scale = size.x.max(size.y).max(size.z).max(1.0);
    let tolerance = scale * 1.0e-10;

    // Index the group's vertices along its widest axis.
    //
    // A candidate can only subdivide an edge if it ends up within `tolerance`
    // of a point *on* that segment, so it must lie inside the segment's
    // bounding box grown by `tolerance` — on every axis, and in particular on
    // this one. Binary-searching that slab therefore skips only candidates the
    // full test below would have rejected anyway; the set of accepted
    // candidates is unchanged. Scanning every group vertex for every edge was
    // quadratic, and on a large flat face it dominated the whole merge pass.
    let axis = if size.x >= size.y && size.x >= size.z {
        0
    } else if size.y >= size.z {
        1
    } else {
        2
    };
    let mut sorted: Vec<(f64, u32)> = group_vertices
        .iter()
        .map(|index| (vertices[*index as usize].component(axis), *index))
        .collect();
    sorted.sort_by(|left, right| left.0.total_cmp(&right.0));

    let mut inserts: Vec<(f64, u32)> = Vec::new();
    for ring in rings.iter_mut() {
        let mut expanded: Vec<u32> = Vec::with_capacity(ring.len());
        let count = ring.len();
        for index in 0..count {
            let from = ring[index];
            let to = ring[(index + 1) % count];
            expanded.push(from);
            let start = vertices[from as usize];
            let end = vertices[to as usize];
            let direction = end.sub(start);
            let length_squared = direction.dot(direction);
            if length_squared <= 0.0 {
                continue;
            }
            let low = start.min(end).sub(Vec3::new(tolerance, tolerance, tolerance));
            let high = start.max(end).add(Vec3::new(tolerance, tolerance, tolerance));
            let slab = sorted.partition_point(|entry| entry.0 < low.component(axis));
            inserts.clear();
            for (coordinate, candidate) in &sorted[slab..] {
                if *coordinate > high.component(axis) {
                    break;
                }
                if *candidate == from || *candidate == to {
                    continue;
                }
                let point = vertices[*candidate as usize];
                if point.x < low.x
                    || point.x > high.x
                    || point.y < low.y
                    || point.y > high.y
                    || point.z < low.z
                    || point.z > high.z
                {
                    continue;
                }
                let t = point.sub(start).dot(direction) / length_squared;
                if t <= 0.0 || t >= 1.0 {
                    continue;
                }
                let projected = start.add(direction.mul(t));
                if projected.sub(point).length() > tolerance {
                    continue;
                }
                inserts.push((t, *candidate));
            }
            // Ties on `t` were previously broken by the scan order, which was
            // ascending vertex index; the slab visits candidates in a different
            // order, so the tie-break is made explicit to keep the output
            // identical.
            inserts.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.cmp(&right.1)));
            expanded.extend(inserts.iter().map(|(_, index)| *index));
        }
        *ring = expanded;
    }
}

/// Steps 3 and 4 of the merging pass: cancel interior edges, chain the rest.
fn extract_loops(plane: Plane, rings: &[Vec<u32>], vertices: &[Vec3]) -> Vec<Vec<u32>> {
    extract_loops_owned(plane, rings.to_vec(), vertices)
}

/// [`extract_loops`] taking ownership of the rings, which it rewrites in place.
fn extract_loops_owned(plane: Plane, mut rings: Vec<Vec<u32>>, vertices: &[Vec3]) -> Vec<Vec<u32>> {
    subdivide_group_edges(&mut rings, vertices);
    let rings = &rings[..];
    // Every directed ring edge is recorded as its undirected key plus a sign;
    // sorting groups the duplicates together, so the net traversal count of an
    // edge is a run sum. A net of zero means the edge is interior to the group.
    //
    // This used to be a `BTreeMap`, whose per-edge node allocations showed up
    // as one of the merge pass's larger costs. Sorting reproduces exactly the
    // same ascending key order, so `edges` comes out in the same sequence.
    let mut ledger: Vec<(u32, u32, i32)> = Vec::new();
    for ring in rings {
        for index in 0..ring.len() {
            let from = ring[index];
            let to = ring[(index + 1) % ring.len()];
            if from == to {
                continue;
            }
            if from < to {
                ledger.push((from, to, 1));
            } else {
                ledger.push((to, from, -1));
            }
        }
    }
    ledger.sort_unstable_by_key(|entry| (entry.0, entry.1));

    let mut edges: Vec<(u32, u32)> = Vec::new();
    let mut cursor = 0;
    while cursor < ledger.len() {
        let (low, high, _) = ledger[cursor];
        let mut count = 0i32;
        while cursor < ledger.len() && ledger[cursor].0 == low && ledger[cursor].1 == high {
            count += ledger[cursor].2;
            cursor += 1;
        }
        if count > 0 {
            for _ in 0..count {
                edges.push((low, high));
            }
        } else if count < 0 {
            for _ in 0..-count {
                edges.push((high, low));
            }
        }
    }
    if edges.is_empty() {
        return Vec::new();
    }

    let (basis_u, basis_v) = plane_basis(plane);
    let project = |index: u32| -> Point2 {
        let point = vertices[index as usize];
        [point.dot(basis_u), point.dot(basis_v)]
    };

    // Step 4: chain edges into loops.
    //
    // The adjacency is a compressed row: `slots[starts[v]..starts[v+1]]` are
    // the edges leaving the vertex whose rank is `v`, in ascending edge id,
    // which is the order the per-vertex `Vec`s used to hold them. Counting-
    // sorting into one buffer replaces a hash map plus one `Vec` per vertex.
    let outgoing = Adjacency::build(&edges);
    let mut used = vec![false; edges.len()];
    let mut loops: Vec<Vec<u32>> = Vec::new();
    for start in 0..edges.len() {
        if used[start] {
            continue;
        }
        let mut chain: Vec<u32> = Vec::new();
        let mut current = start;
        let origin = edges[start].0;
        let mut guard = 0usize;
        loop {
            guard += 1;
            if guard > edges.len() + 4 {
                chain.clear();
                break;
            }
            used[current] = true;
            chain.push(edges[current].0);
            let vertex = edges[current].1;
            if vertex == origin {
                break;
            }
            let incoming = project(edges[current].0);
            let at = project(vertex);
            let base = [incoming[0] - at[0], incoming[1] - at[1]];
            let Some(next) = pick_next_edge(&outgoing, &edges, &used, vertex, base, &project) else {
                chain.clear();
                break;
            };
            current = next;
        }
        if chain.len() >= 3 {
            loops.push(chain);
        }
    }
    loops
}

/// Drops every vertex that is collinear inside *all* of the loops that use it.
///
/// A vertex left over from a boolean split is only real geometry when at least
/// one incident face turns there. Removing such vertices face by face would
/// leave T-junctions (an edge subdivided on one side of the solid but not the
/// other), which is exactly what makes a merged mesh non-manifold. Doing it
/// globally keeps every shared edge conforming while still reaching the minimal
/// facet set that reference exports contain.
fn remove_globally_collinear_vertices(vertices: &[Vec3], groups: &mut [(Plane, Vec<Vec<u32>>)]) {
    let scale = Bounds::from_points(vertices.iter().copied())
        .map(|bounds| {
            let size = bounds.size();
            size.x.max(size.y).max(size.z).max(1.0)
        })
        .unwrap_or(1.0);
    let tolerance = scale * 1.0e-10;

    // Vertex ids are dense indices into `vertices`, so the per-vertex state
    // lives in flat arrays rather than hash maps: same values, no hashing and
    // no allocation per entry. `seen` marks which ids this round touched, so
    // the arrays can be reused without being cleared.
    let mut straight_everywhere: Vec<bool> = vec![false; vertices.len()];
    let mut seen: Vec<bool> = vec![false; vertices.len()];
    let mut touched: Vec<u32> = Vec::new();
    let mut removable: Vec<bool> = vec![false; vertices.len()];
    for _ in 0..8 {
        for vertex in touched.drain(..) {
            seen[vertex as usize] = false;
            removable[vertex as usize] = false;
        }
        for (_, loops) in groups.iter() {
            for ring in loops {
                let count = ring.len();
                for index in 0..count {
                    let vertex = ring[index];
                    let previous = vertices[ring[(index + count - 1) % count] as usize];
                    let current = vertices[vertex as usize];
                    let next = vertices[ring[(index + 1) % count] as usize];
                    let first = current.sub(previous);
                    let second = next.sub(current);
                    let base = first.length().max(second.length());
                    let straight = base > 0.0
                        && first.cross(second).length() <= tolerance * base
                        && first.dot(second) > 0.0;
                    let slot = vertex as usize;
                    if !seen[slot] {
                        seen[slot] = true;
                        touched.push(vertex);
                        straight_everywhere[slot] = true;
                    }
                    straight_everywhere[slot] &= straight;
                }
            }
        }
        let mut any_removable = false;
        for vertex in &touched {
            if straight_everywhere[*vertex as usize] {
                removable[*vertex as usize] = true;
                any_removable = true;
            }
        }
        if !any_removable {
            return;
        }
        let mut changed = false;
        for (_, loops) in groups.iter_mut() {
            for ring in loops.iter_mut() {
                let before = ring.len();
                ring.retain(|vertex| !removable[*vertex as usize]);
                if ring.len() != before {
                    changed = true;
                }
            }
            loops.retain(|ring| ring.len() >= 3);
        }
        if !changed {
            return;
        }
    }
}

/// Inserts, into every loop edge, any vertex of the merged surface that lies
/// strictly inside it.
///
/// [`remove_globally_collinear_vertices`] handles the case where a leftover
/// split point is collinear everywhere. The opposite case survives it: a point
/// that is a genuine corner of one face and sits in the middle of a
/// neighbouring face's edge. Left alone that is a T-junction, and a T-junction
/// is a hole in the surface as far as edge-adjacency is concerned. Candidates
/// are found through a uniform grid so the pass stays close to linear.
fn insert_t_junction_vertices(vertices: &[Vec3], groups: &mut [(Plane, Vec<Vec<u32>>)]) {
    let mut used: Vec<u32> = groups
        .iter()
        .flat_map(|(_, loops)| loops.iter().flatten().copied())
        .collect();
    used.sort_unstable();
    used.dedup();
    if used.len() < 4 {
        return;
    }
    let Some(bounds) = Bounds::from_points(used.iter().map(|index| vertices[*index as usize]))
    else {
        return;
    };
    let size = bounds.size();
    let scale = size.x.max(size.y).max(size.z).max(1.0);
    // At least the welder's tolerance. The welder has already declared that
    // two points this close are one point, so a corner this close to an edge
    // is *on* that edge — refusing to seam it there would contradict the
    // vertex identity the rest of the pass is built on. It matters: the
    // corners that need seaming are the ones a shallow crossing produced, and
    // a shallow crossing is exactly the ill-conditioned intersection whose
    // answer lands a micron out. At `scale * 1e-10` a 200 mm model seamed only
    // within 20 nm and left 30 edges open along the blades of the louvred
    // basket fixture.
    let tolerance = (scale * 1.0e-10).max(WELD_EPSILON);
    let cell = (scale / 64.0).max(tolerance * 16.0);

    let key = |point: Vec3| -> [i64; 3] {
        [
            (point.x / cell).floor() as i64,
            (point.y / cell).floor() as i64,
            (point.z / cell).floor() as i64,
        ]
    };
    let mut grid: FastMap<[i64; 3], Vec<u32>> = FastMap::default();
    for index in &used {
        grid.entry(key(vertices[*index as usize]))
            .or_default()
            .push(*index);
    }

    for (_, loops) in groups.iter_mut() {
        for ring in loops.iter_mut() {
            let count = ring.len();
            let mut expanded: Vec<u32> = Vec::with_capacity(count);
            for index in 0..count {
                let from = ring[index];
                let to = ring[(index + 1) % count];
                expanded.push(from);
                let start = vertices[from as usize];
                let end = vertices[to as usize];
                let direction = end.sub(start);
                let length_squared = direction.dot(direction);
                if length_squared <= 0.0 {
                    continue;
                }
                // Walk the cells the segment passes through, with a one-cell
                // margin so points near a boundary are not missed.
                let steps = (direction.length() / cell).ceil() as usize + 1;
                let mut cells: Vec<[i64; 3]> = Vec::with_capacity(steps * 27);
                for step in 0..=steps {
                    let point = start.add(direction.mul(step as f64 / steps as f64));
                    let base = key(point);
                    for dx in -1..=1 {
                        for dy in -1..=1 {
                            for dz in -1..=1 {
                                cells.push([base[0] + dx, base[1] + dy, base[2] + dz]);
                            }
                        }
                    }
                }
                cells.sort_unstable();
                cells.dedup();
                let mut inserts: Vec<(f64, u32)> = Vec::new();
                for probe in cells {
                    let Some(candidates) = grid.get(&probe) else {
                        continue;
                    };
                    for candidate in candidates {
                        if *candidate == from || *candidate == to {
                            continue;
                        }
                        let point = vertices[*candidate as usize];
                        let t = point.sub(start).dot(direction) / length_squared;
                        if t <= 0.0 || t >= 1.0 {
                            continue;
                        }
                        if start.add(direction.mul(t)).sub(point).length() > tolerance {
                            continue;
                        }
                        inserts.push((t, *candidate));
                    }
                }
                inserts.sort_by(|left, right| left.0.total_cmp(&right.0));
                inserts.dedup_by_key(|entry| entry.1);
                expanded.extend(inserts.into_iter().map(|(_, index)| index));
            }
            *ring = expanded;
        }
    }
}

fn emit_faces(plane: Plane, loops: &[Vec<u32>], vertices: &[Vec3], output: &mut Vec<Polygon>) {
    let (basis_u, basis_v) = plane_basis(plane);
    let project = |index: u32| -> Point2 {
        let point = vertices[index as usize];
        [point.dot(basis_u), point.dot(basis_v)]
    };
    // The way back to 3D from a triangulated face is a lookup, not an inverse
    // projection. Neither triangulator invents a point — every vertex they
    // emit is one they were given — so the original 3D vertex is always
    // available, and using it is the only way to keep the face's corners
    // exactly where the rest of the solid still believes they are.
    //
    // Rebuilding a corner as `u*basis_u + v*basis_v + offset*normal` instead
    // reconstructs it through a basis this function invented, and lands it a
    // few parts in 1e8 from where it started. That is above the welder's
    // tolerance, so the neighbouring face — which was not triangulated, and
    // kept the original — stops sharing the edge: the pass that exists to
    // tidy the surface up tears it instead.
    let mut exact: FastMap<[u64; 2], Vec3> = FastMap::default();

    // Step 5: classify loops and emit faces.
    let mut outlines: Vec<(Vec<Point2>, f64)> = Vec::new();
    let mut holes: Vec<Vec<Point2>> = Vec::new();
    let mut outline_rings: Vec<Vec<u32>> = Vec::new();
    for chain in loops.iter().cloned() {
        let projected: Vec<Point2> = chain.iter().map(|index| project(*index)).collect();
        for (point, index) in projected.iter().zip(chain.iter()) {
            exact.insert(
                [point[0].to_bits(), point[1].to_bits()],
                vertices[*index as usize],
            );
        }
        let area = super::poly2d::signed_area(&projected);
        if area > 0.0 {
            outlines.push((projected, area));
            outline_rings.push(chain);
        } else if area < 0.0 {
            holes.push(projected);
        }
    }
    if outlines.is_empty() {
        return;
    }

    let mut assigned: Vec<Vec<Vec<Point2>>> = vec![Vec::new(); outlines.len()];
    for hole in holes {
        let probe = hole[0];
        let mut best: Option<usize> = None;
        for (index, (outline, area)) in outlines.iter().enumerate() {
            if super::poly2d::point_in_contour(probe, outline) {
                match best {
                    Some(current) if outlines[current].1 <= *area => {}
                    _ => best = Some(index),
                }
            }
        }
        if let Some(index) = best {
            assigned[index].push(hole);
        }
    }

    let unproject = |point: Point2| -> Vec3 {
        match exact.get(&[point[0].to_bits(), point[1].to_bits()]) {
            Some(vertex) => *vertex,
            // Unreachable while the triangulators stay Steiner-point-free; kept
            // so that a future one which is not silently degrades to the old
            // approximation rather than dropping the vertex.
            None => basis_u
                .mul(point[0])
                .add(basis_v.mul(point[1]))
                .add(plane.normal.mul(plane.offset)),
        }
    };

    for (index, (outline, _)) in outlines.iter().enumerate() {
        if assigned[index].is_empty() {
            // Rebuild from the original welded vertices so exact coordinates
            // survive the round trip through the 2D projection.
            let mut vertices3: Vec<Vec3> = outline_rings[index]
                .iter()
                .map(|vertex| vertices[*vertex as usize])
                .collect();
            dedup_ring(&mut vertices3, EPSILON);
            if vertices3.len() < 3 {
                continue;
            }
            // Deliberately *not* `Polygon::simplify` here: it would strip the
            // collinear vertices that `insert_t_junction_vertices` just added
            // to keep shared edges conforming.
            let polygon = Polygon::with_plane(vertices3, plane);
            if polygon.area() > 0.0 && is_convex_ring(outline) {
                output.push(polygon);
                continue;
            }
            // Non-convex outline: a fan would be wrong, so triangulate it
            // properly — *with* its collinear vertices. Those vertices are not
            // decoration: each one is a corner of the face on the other side of
            // that edge, and dropping it here is precisely what opens a seam
            // there. The triangulator keeps them now, so there is nothing left
            // to trade away.
            emit_triangles(
                &triangulate_with_holes(outline, &[]),
                plane,
                &unproject,
                output,
            );
        } else {
            emit_triangles(
                &triangulate_with_holes(outline, &assigned[index]),
                plane,
                &unproject,
                output,
            );
        }
    }
}

/// Emits a triangulation as polygons carrying the *group's* plane.
///
/// Deriving each triangle's own plane from its three points — which is what
/// `Polygon::new` does — is wrong here. A triangulated merged face is mostly
/// slivers, and a sliver's Newell normal is dominated by the noise in its two
/// near-parallel edges: it can come out tilted off the face, or reversed, and
/// a reversed face plane is a hole as far as the next boolean is concerned.
/// The face's plane is already known exactly and every triangle lies in it by
/// construction, so it is passed down instead of re-derived. The winding
/// agrees: the triangulators return counter-clockwise triangles in the
/// `(u, v)` basis, and `plane_basis` builds that basis right-handed about the
/// normal.
fn emit_triangles(
    triangles: &[[Point2; 3]],
    plane: Plane,
    unproject: &impl Fn(Point2) -> Vec3,
    output: &mut Vec<Polygon>,
) {
    for triangle in triangles {
        let points: Vec<Vec3> = triangle.iter().map(|point| unproject(*point)).collect();
        let polygon = Polygon::with_plane(points, plane);
        if polygon.area() > 0.0 {
            output.push(polygon);
        }
    }
}

fn is_convex_ring(points: &[Point2]) -> bool {
    let count = points.len();
    if count < 3 {
        return false;
    }
    let mut sign = 0i32;
    for index in 0..count {
        let a = points[index];
        let b = points[(index + 1) % count];
        let c = points[(index + 2) % count];
        let cross = (b[0] - a[0]) * (c[1] - b[1]) - (b[1] - a[1]) * (c[0] - b[0]);
        if cross > 0.0 {
            if sign < 0 {
                return false;
            }
            sign = 1;
        } else if cross < 0.0 {
            if sign > 0 {
                return false;
            }
            sign = -1;
        }
    }
    true
}

/// Outgoing-edge adjacency of a directed edge list, as a compressed row.
///
/// Vertex ids here are indices into a solid-wide vertex array while a plane
/// group only touches a handful of them, so the rows are keyed by the vertex's
/// rank in the group's sorted vertex list rather than by the id itself.
struct Adjacency {
    vertices: Vec<u32>,
    starts: Vec<u32>,
    slots: Vec<usize>,
}

impl Adjacency {
    fn build(edges: &[(u32, u32)]) -> Self {
        let mut vertices: Vec<u32> = edges.iter().map(|(from, _)| *from).collect();
        vertices.sort_unstable();
        vertices.dedup();
        let mut counts = vec![0u32; vertices.len() + 1];
        for (from, _) in edges {
            let rank = vertices.partition_point(|vertex| vertex < from);
            counts[rank + 1] += 1;
        }
        for index in 1..counts.len() {
            counts[index] += counts[index - 1];
        }
        let starts = counts.clone();
        let mut cursors = counts;
        let mut slots = vec![0usize; edges.len()];
        // Edges are visited in ascending id, so each row ends up sorted by id.
        for (id, (from, _)) in edges.iter().enumerate() {
            let rank = vertices.partition_point(|vertex| vertex < from);
            slots[cursors[rank] as usize] = id;
            cursors[rank] += 1;
        }
        Self {
            vertices,
            starts,
            slots,
        }
    }

    fn outgoing(&self, vertex: u32) -> &[usize] {
        let rank = self.vertices.partition_point(|entry| *entry < vertex);
        if rank >= self.vertices.len() || self.vertices[rank] != vertex {
            return &[];
        }
        &self.slots[self.starts[rank] as usize..self.starts[rank + 1] as usize]
    }
}

/// Chooses the next edge that keeps the interior on the left: the smallest
/// clockwise turn away from the reversed incoming direction.
fn pick_next_edge(
    outgoing: &Adjacency,
    edges: &[(u32, u32)],
    used: &[bool],
    vertex: u32,
    base: [f64; 2],
    project: &impl Fn(u32) -> Point2,
) -> Option<usize> {
    let candidates = outgoing.outgoing(vertex);
    if candidates.is_empty() {
        return None;
    }
    let at = project(vertex);
    let mut best: Option<(f64, usize)> = None;
    for candidate in candidates {
        if used[*candidate] {
            continue;
        }
        let target = project(edges[*candidate].1);
        let direction = [target[0] - at[0], target[1] - at[1]];
        let cross = base[0] * direction[1] - base[1] * direction[0];
        let dot = base[0] * direction[0] + base[1] * direction[1];
        let mut clockwise = -cross.atan2(dot);
        if clockwise <= 0.0 {
            clockwise += 2.0 * std::f64::consts::PI;
        }
        match best {
            Some((score, _)) if score <= clockwise => {}
            _ => best = Some((clockwise, *candidate)),
        }
    }
    best.map(|(_, index)| index)
}

/// The six faces of an axis-aligned box, grown by `slack` on every side.
pub fn bounding_box_polygons(bounds: Bounds, slack: f64) -> Vec<Polygon> {
    let size = bounds.size();
    let scale = size.x.max(size.y).max(size.z).max(1.0);
    let pad = slack * scale;
    let min = bounds.min.sub(Vec3::new(pad, pad, pad));
    let max = bounds.max.add(Vec3::new(pad, pad, pad));
    super::primitives::cube(max.sub(min), false)
        .transformed(Matrix4::translation(min))
        .polygons
}

/// Clips a closed convex polytope to the back half-space of `plane`, sealing
/// the cut with a new face.
///
/// This is the cheap path for anything defined by half-spaces — `hull()`
/// arrives from the evaluator as a plane set — because it never builds a BSP
/// tree: it splits the existing faces once and chains the cut edges into a
/// single cap.
pub fn clip_convex(polygons: &[Polygon], plane: Plane) -> Vec<Polygon> {
    let mut kept: Vec<Polygon> = Vec::new();
    let mut segments: Vec<(Vec3, Vec3)> = Vec::new();
    // One set of buckets for the whole sweep: `hull()` clips the same box by
    // every support plane, so a fresh set per face was four allocations per
    // (plane, face) pair.
    let mut coplanar_front = Vec::new();
    let mut coplanar_back = Vec::new();
    let mut front = Vec::new();
    let mut back = Vec::new();
    for polygon in polygons {
        coplanar_front.clear();
        coplanar_back.clear();
        front.clear();
        back.clear();
        bsp::split_polygon(
            plane,
            polygon,
            &mut coplanar_front,
            &mut coplanar_back,
            &mut front,
            &mut back,
        );
        kept.append(&mut coplanar_back);
        // The cut boundary is made of the edges of the back fragments that lie
        // exactly in the plane. Read before the fragments are moved into
        // `kept`, so they no longer have to be cloned.
        for fragment in &back {
            let count = fragment.vertices.len();
            for index in 0..count {
                let a = fragment.vertices[index];
                let b = fragment.vertices[(index + 1) % count];
                if plane.distance(a).abs() <= EPSILON && plane.distance(b).abs() <= EPSILON {
                    segments.push((a, b));
                }
            }
        }
        kept.append(&mut back);
    }
    if kept.is_empty() {
        return Vec::new();
    }
    if let Some(cap) = chain_cap(&segments, plane) {
        kept.push(cap);
    }
    kept
}

/// Chains the cut segments into a single loop and orients it along `plane`.
fn chain_cap(segments: &[(Vec3, Vec3)], plane: Plane) -> Option<Polygon> {
    if segments.len() < 3 {
        return None;
    }
    let mut welder = VertexWelder::default();
    let mut edges: Vec<(u32, u32)> = Vec::new();
    for (a, b) in segments {
        let from = welder.insert(*a);
        let to = welder.insert(*b);
        if from != to {
            // The cap faces the opposite way from the kept fragments' edges.
            edges.push((to, from));
        }
    }
    let vertices = welder.vertices.clone();
    let mut next: FastMap<u32, u32> = FastMap::default();
    for (from, to) in &edges {
        next.insert(*from, *to);
    }
    let start = edges[0].0;
    let mut ring = vec![start];
    let mut current = start;
    for _ in 0..edges.len() {
        let Some(step) = next.get(&current) else {
            return None;
        };
        if *step == start {
            break;
        }
        ring.push(*step);
        current = *step;
    }
    if ring.len() < 3 {
        return None;
    }
    let mut points: Vec<Vec3> = ring.iter().map(|index| vertices[*index as usize]).collect();
    dedup_ring(&mut points, EPSILON);
    if points.len() < 3 {
        return None;
    }
    let mut polygon = Polygon::new(points)?;
    if polygon.plane.normal.dot(plane.normal) < 0.0 {
        polygon.flip();
    }
    // The cap of a back-clipped body faces along +normal.
    if polygon.plane.normal.dot(plane.normal) < 1.0 - PLANE_ANGLE_EPSILON {
        return None;
    }
    if (polygon.plane.offset - plane.offset).abs() > PLANE_OFFSET_EPSILON.max(EPSILON) {
        return None;
    }
    if !polygon.simplify(EPSILON) {
        return None;
    }
    Some(polygon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csg::{self, primitives};

    /// Two solids that meet face to face are a union the kernel has to get
    /// right — a printed-in-place hinge is nothing but that. It used to
    /// return *less* volume than one operand on its own, because the operands
    /// had been through the merge pass and came back with a pinhole in them,
    /// and a BSP cannot classify "inside" against a surface with a hole in it.
    /// The booleans no longer merge, which is what this pins.
    #[test]
    fn a_union_of_two_solids_that_touch_face_to_face_keeps_both() {
        let left = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false);
        let right = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false)
            .transformed(Matrix4::translation(Vec3::new(10.0, 0.0, 0.0)));
        let mesh = left.union(&right).sealed().to_indexed_mesh();
        assert!(mesh.is_manifold(), "{}", mesh.statistics());
        assert!(
            (mesh.volume() - 2000.0).abs() < 1.0e-9,
            "touching cubes should union to both, got {}",
            mesh.volume()
        );
    }

    /// A boolean must hand back raw fragments. Merging between booleans is
    /// what fed the next one a surface it could not classify against; the
    /// merge belongs on the export path and nowhere else.
    #[test]
    fn a_boolean_does_not_merge_its_own_result() {
        let left = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false);
        let right = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false)
            .transformed(Matrix4::translation(Vec3::new(0.0, 0.0, 10.0)));
        let raw = left.union(&right);
        let merged = raw.sealed();
        assert!(
            raw.polygons.len() > merged.polygons.len(),
            "the boolean returned {} faces and the merge {} — the merge has \
             moved back into the boolean",
            raw.polygons.len(),
            merged.polygons.len()
        );
    }

    #[test]
    fn simplifying_a_split_face_restores_one_quad() {
        // Two coplanar halves of a 2x1 rectangle share an interior edge.
        let left = Polygon::new(vec![
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(1.0, 1.0, 0.0),
            Vec3::new(0.0, 1.0, 0.0),
        ])
        .unwrap();
        let right = Polygon::new(vec![
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(2.0, 0.0, 0.0),
            Vec3::new(2.0, 1.0, 0.0),
            Vec3::new(1.0, 1.0, 0.0),
        ])
        .unwrap();
        let solid = Solid {
            polygons: vec![left, right],
        };
        let merged = solid.simplified();
        assert_eq!(merged.polygons.len(), 1);
        assert_eq!(merged.polygons[0].vertices.len(), 4);
        assert!((merged.polygons[0].area() - 2.0).abs() < 1e-12);
    }

    #[test]
    fn simplifying_keeps_a_hole_in_a_face() {
        // Four coplanar quads framing a 2x2 hole in a 10x10 face: exactly the
        // shape a boolean leaves behind. The merge must recover one outline
        // plus one hole loop, not one 100mm^2 face.
        let quad = |points: [[f64; 2]; 4]| {
            Polygon::new(
                points
                    .iter()
                    .map(|point| Vec3::new(point[0], point[1], 0.0))
                    .collect(),
            )
            .unwrap()
        };
        // The fragments are edge-conforming, exactly as infinite BSP split
        // planes leave them.
        let solid = Solid {
            polygons: vec![
                quad([[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0]]),
                quad([[4.0, 0.0], [6.0, 0.0], [6.0, 4.0], [4.0, 4.0]]),
                quad([[6.0, 0.0], [10.0, 0.0], [10.0, 4.0], [6.0, 4.0]]),
                quad([[0.0, 6.0], [4.0, 6.0], [4.0, 10.0], [0.0, 10.0]]),
                quad([[4.0, 6.0], [6.0, 6.0], [6.0, 10.0], [4.0, 10.0]]),
                quad([[6.0, 6.0], [10.0, 6.0], [10.0, 10.0], [6.0, 10.0]]),
                quad([[0.0, 4.0], [4.0, 4.0], [4.0, 6.0], [0.0, 6.0]]),
                quad([[6.0, 4.0], [10.0, 4.0], [10.0, 6.0], [6.0, 6.0]]),
            ],
        };
        let merged = solid.simplified();
        let area: f64 = merged.polygons.iter().map(Polygon::area).sum();
        assert!((area - 96.0).abs() < 1e-9, "{area}");
        let loops = boundary_loops(&solid.polygons, solid.polygons[0].plane);
        assert_eq!(loops.len(), 2, "one outline and one hole");
    }

    #[test]
    fn union_of_stacked_cubes_merges_the_shared_wall_away() {
        let a = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false);
        let b = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false)
            .transformed(Matrix4::translation(Vec3::new(0.0, 0.0, 10.0)));
        // `sealed` because that is the export path: booleans hand back raw
        // fragments now, and the shared wall is merged away on the way out.
        let solid = a.union(&b).sealed();
        let mesh = solid.to_indexed_mesh();
        assert!(mesh.is_manifold(), "{}", mesh.statistics());
        assert_eq!(mesh.triangles.len(), 12, "stacked cubes should be one box");
        assert_eq!(mesh.vertices.len(), 8);
        assert!((mesh.volume() - 2000.0).abs() < 1e-9);
    }

    #[test]
    fn difference_that_punches_a_through_hole_is_a_torus() {
        let block = primitives::cube(Vec3::new(20.0, 20.0, 5.0), false);
        let tool = primitives::cube(Vec3::new(4.0, 4.0, 20.0), false)
            .transformed(Matrix4::translation(Vec3::new(8.0, 8.0, -5.0)));
        let solid = block.difference(&tool).sealed();
        let mesh = solid.to_indexed_mesh();
        assert!(mesh.is_manifold(), "{}", mesh.statistics());
        assert!(
            (mesh.volume() - (400.0 - 16.0) * 5.0).abs() < 1e-9,
            "{}",
            mesh.volume()
        );
        assert_eq!(mesh.statistics().euler_characteristic, 0);
    }

    #[test]
    fn difference_producing_two_pieces_reports_two_components() {
        let bar = primitives::cube(Vec3::new(30.0, 5.0, 5.0), false);
        let cut = primitives::cube(Vec3::new(4.0, 20.0, 20.0), false)
            .transformed(Matrix4::translation(Vec3::new(13.0, -5.0, -5.0)));
        let solid = bar.difference(&cut);
        let mesh = solid.to_indexed_mesh();
        assert!(mesh.is_manifold(), "{}", mesh.statistics());
        assert!((mesh.volume() - (30.0 - 4.0) * 25.0).abs() < 1e-9);
        // Two disjoint boxes: 12 triangles each.
        assert_eq!(mesh.triangles.len(), 24);
    }

    #[test]
    fn intersection_of_two_boxes_is_a_box() {
        let a = primitives::cube(Vec3::new(10.0, 10.0, 10.0), true);
        let b = primitives::cube(Vec3::new(6.0, 20.0, 20.0), true);
        let solid = a.intersection(&b);
        let mesh = solid.to_indexed_mesh();
        assert!(mesh.is_manifold(), "{}", mesh.statistics());
        assert_eq!(mesh.triangles.len(), 12);
        assert!((mesh.volume() - 600.0).abs() < 1e-9);
    }

    #[test]
    fn booleans_preserve_sharp_edges_exactly() {
        let a = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false);
        let b = primitives::cube(Vec3::new(3.0, 3.0, 3.0), false)
            .transformed(Matrix4::translation(Vec3::new(2.0, 2.0, 7.0)));
        let mesh = a.difference(&b).to_indexed_mesh();
        // Every coordinate must be exactly one of the input plane offsets.
        for vertex in &mesh.vertices {
            for axis in 0..3 {
                let value = vertex.component(axis);
                assert!(
                    [0.0f64, 2.0, 5.0, 7.0, 10.0]
                        .iter()
                        .any(|k| (value - k).abs() < 1e-12),
                    "unexpected coordinate {value}"
                );
            }
        }
    }

    #[test]
    fn a_convex_solid_decomposes_into_one_piece() {
        let solid = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false);
        assert!(solid.is_convex());
        assert_eq!(solid.convex_decomposition().len(), 1);
        assert_eq!(solid.convex_decomposition()[0].len(), 8);
    }

    #[test]
    fn an_l_shape_decomposes_into_convex_cells_of_the_right_volume() {
        let a = primitives::cube(Vec3::new(10.0, 4.0, 4.0), false);
        let b = primitives::cube(Vec3::new(4.0, 10.0, 4.0), false);
        let solid = a.union(&b);
        assert!(!solid.is_convex());
        let cells = solid.convex_decomposition();
        assert!(cells.len() >= 2, "expected a real decomposition");
        for cell in &cells {
            assert!(cell.len() >= 4);
        }
    }

    #[test]
    fn a_chain_of_subtractions_stays_watertight_after_sealing() {
        let mut solid = primitives::cube(Vec3::new(30.0, 30.0, 10.0), false);
        let holes = [(2.0, 2.0, 4.0), (12.0, 5.0, 6.0), (22.0, 3.0, 5.0), (7.0, 18.0, 9.0)];
        for (x, y, side) in holes {
            let tool = primitives::cube(Vec3::new(side, side, 20.0), false)
                .transformed(Matrix4::translation(Vec3::new(x, y, -5.0)));
            solid = solid.difference(&tool);
        }
        let mesh = solid.sealed().to_indexed_mesh();
        assert!(mesh.is_manifold(), "{}", mesh.statistics());
        let removed: f64 = holes.iter().map(|(_, _, side)| side * side).sum();
        let expected = (900.0 - removed) * 10.0;
        assert!((mesh.volume() - expected).abs() < 1e-9, "{}", mesh.volume());
        // Four through-holes: chi = 2 - 2 * 4.
        assert_eq!(mesh.statistics().euler_characteristic, -6);
    }

    #[test]
    fn a_staircase_union_stays_watertight_after_sealing() {
        // Every step ends part way along the previous step's face, which is
        // exactly the T-junction the sealing pass exists to close.
        let mut solid = csg::Solid::default();
        for step in 0..6u32 {
            let height = 4.0 + step as f64 * 3.0;
            let piece = primitives::cube(Vec3::new(7.0, 20.0, height), false)
                .transformed(Matrix4::translation(Vec3::new(step as f64 * 5.0, 0.0, 0.0)));
            solid = if solid.is_empty() { piece } else { solid.union(&piece) };
        }
        let mesh = solid.sealed().to_indexed_mesh();
        assert!(mesh.is_manifold(), "{}", mesh.statistics());
        assert_eq!(mesh.statistics().euler_characteristic, 2);
        assert!(mesh.volume() > 0.0);
    }

    #[test]
    fn sealing_is_idempotent() {
        let solid = primitives::cube(Vec3::new(20.0, 20.0, 5.0), false)
            .difference(
                &primitives::cube(Vec3::new(4.0, 4.0, 20.0), false)
                    .transformed(Matrix4::translation(Vec3::new(8.0, 8.0, -5.0))),
            );
        let once = solid.sealed();
        let twice = once.sealed();
        assert_eq!(once.polygons.len(), twice.polygons.len());
        let first = once.to_indexed_mesh();
        let second = twice.to_indexed_mesh();
        assert_eq!(first.triangles.len(), second.triangles.len());
        assert!((first.volume() - second.volume()).abs() < 1e-12);
    }

    #[test]
    fn transforming_with_a_mirror_keeps_normals_outward() {
        let solid = primitives::cube(Vec3::new(4.0, 4.0, 4.0), true)
            .transformed(Matrix4::scaling(Vec3::new(-1.0, 1.0, 1.0)));
        let mesh = solid.to_indexed_mesh();
        assert!(mesh.volume() > 0.0, "{}", mesh.volume());
        assert!((mesh.volume() - 64.0).abs() < 1e-12);
    }
}
