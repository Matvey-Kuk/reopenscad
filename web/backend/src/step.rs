//! Dependency-free STEP (ISO 10303-21, AP214) writer: a real boundary
//! representation, not a triangle soup wearing a CAD extension.
//!
//! # Why this is not "STL with a different header"
//!
//! Every other exporter here hands the consumer facets. A slicer wants facets;
//! a CAD package does not. Import a triangulated STEP into Fusion, SolidWorks
//! or FreeCAD and you get one planar face per triangle: you cannot select the
//! top of a cube, you cannot fillet its edge, and you cannot offset its wall,
//! because as far as the modeller is concerned there is no top and no edge —
//! there are ninety-six unrelated surfaces. The whole point of shipping STEP
//! rather than a mesh format is that a flat wall arrives as *one* face with
//! *one* boundary, so this module's real work is recovering those faces.
//!
//! # Recovering faces from the export mesh
//!
//! The kernel already merges coplanar fragments back into whole faces on the
//! way out — [`crate::csg::Solid::sealed`] — but it then triangulates any face
//! that has holes or a non-convex outline, and the export path hands every
//! consumer an [`engine::Mesh`](crate::engine::Mesh). Rather than thread a
//! second, solid-shaped export path through the compile cache, the part
//! selection and the sampled-mesher fallback, this module re-derives the faces
//! from the triangles:
//!
//! 1. weld vertices with the kernel's own [`VertexWelder`], so two triangles
//!    that meet along an edge agree on its endpoints *by index*;
//! 2. build the half-edge twin map, which is exact integer bookkeeping;
//! 3. grow planar regions across twin edges, seeding from the largest triangle
//!    first and accepting a neighbour only when all three of its corners lie
//!    within tolerance *of the region's plane*;
//! 4. walk each region's boundary half-edges into closed loops;
//! 5. split the loops into one outer boundary and its holes by signed area.
//!
//! Step 3 is deliberately a point-to-plane test rather than a normal
//! comparison. A triangulated face is mostly slivers, and a sliver's own
//! normal is dominated by the noise in its two near-parallel edges — the same
//! trap `csg::solid::emit_triangles` documents. Its *vertices*, however, are
//! the kernel's exact face corners, so asking how far they are from the plane
//! is well conditioned where asking which way the sliver faces is not. Seeding
//! from the largest triangle makes the region's first plane the best
//! conditioned one available, and the area-weighted refit that follows lets
//! slivers contribute nothing to it.
//!
//! Step 4 never needs an angular tie-break at a pinch vertex, unlike the
//! kernel's own loop chaining: the triangulation is still there, so the next
//! boundary half-edge is found by rotating around the vertex through the
//! region's own triangles, which is exact.
//!
//! # Shared edges
//!
//! Two faces that meet along an edge must reference the *same* `EDGE_CURVE`
//! and the same two `VERTEX_POINT`s. A file where each face owns private
//! copies imports as a pile of disconnected surfaces — no solid, no volume,
//! nothing to machine — which is the single most common way a hand-written
//! STEP writer fails. Here it holds by construction: an edge is keyed on the
//! ordered pair of *welded vertex indices*, so the two faces that produced it
//! cannot disagree about it, and the two directions of traversal become the
//! `.T.`/`.F.` flag on `ORIENTED_EDGE` rather than a second curve.
//!
//! # When the mesh is not merged
//!
//! Models the exact kernel declines fall back to the sampled mesher, whose
//! output has no coplanar structure to recover. Those export as one face per
//! triangle — still a topologically valid closed shell with shared edges, just
//! without the face merging. That is a property of the input, not a bug here,
//! and it is better than refusing the export.

use crate::csg::mesh::{Bounds, FastMap, Plane, Vec3, VertexWelder};
use crate::csg::poly2d::{point_in_contour, signed_area, Point2};
use crate::csg::solid::plane_basis;
use crate::engine::Mesh;
use std::fmt::{Arguments, Write as _};

/// Relative tolerance for "this triangle lies in the region's plane".
///
/// Triangles that came from one merged kernel face share its exact corners, so
/// their distance to a plane fitted through them is at the level of f64
/// rounding — around 1e-16 of the model's own size. A relative 1e-9 therefore
/// leaves seven orders of magnitude of headroom while staying far below the
/// deviation of any tessellated curve worth keeping curved: a cylinder facet
/// on a 100 mm part is 1e-5 mm off its neighbour's plane even at `$fn = 3600`,
/// a hundred times this tolerance, so cylinders never collapse into one face.
const COPLANAR_RELATIVE_TOLERANCE: f64 = 1.0e-9;

/// `GLOBAL_UNCERTAINTY_ASSIGNED_CONTEXT`'s distance accuracy, in millimetres.
///
/// This is what a consumer uses to decide whether two faces are sewn together,
/// so it has to be looser than the kernel's vertex welding (3e-5 mm) would
/// leave a gap, and tighter than any feature anyone models. 1e-7 mm is the
/// value OpenCASCADE-based writers emit and is comfortably inside both bounds.
const DISTANCE_ACCURACY: f64 = 1.0e-7;

// ---------------------------------------------------------------------------
// Face recovery
// ---------------------------------------------------------------------------

/// One planar face: an outer boundary and the holes inside it, as welded
/// vertex indices.
#[derive(Debug)]
struct Face {
    plane: Plane,
    /// Counter-clockwise seen from the side `plane.normal` points to.
    outer: Vec<u32>,
    /// Clockwise in the same view, which is what `FACE_BOUND` expects of a
    /// hole once the face's own sense is `.T.`.
    inner: Vec<Vec<u32>>,
}

/// The recovered boundary representation.
#[derive(Debug, Default)]
struct Brep {
    vertices: Vec<Vec3>,
    faces: Vec<Face>,
    /// Canonical undirected edges as `(low, high)` welded indices, in first-use
    /// order. The index into this vector is the edge's identity, and it is what
    /// makes the two faces sharing an edge reference one `EDGE_CURVE`.
    edges: Vec<(u32, u32)>,
    edge_ids: FastMap<(u32, u32), u32>,
}

impl Brep {
    fn edge(&mut self, from: u32, to: u32) -> (u32, bool) {
        let forward = from < to;
        let key = if forward { (from, to) } else { (to, from) };
        let next = self.edges.len() as u32;
        let id = *self.edge_ids.entry(key).or_insert_with(|| {
            // `edges` and `edge_ids` are filled in lockstep, so the id handed
            // out here is always this edge's index.
            next
        });
        if id == next {
            self.edges.push(key);
        }
        (id, forward)
    }

    fn recover(mesh: &Mesh) -> Self {
        let (vertices, triangles) = weld(mesh);
        if triangles.is_empty() {
            return Self::default();
        }
        let twin = twin_map(&triangles);
        let tolerance = coplanar_tolerance(&vertices);
        let Regions { regions, region_of } = grow_regions(&vertices, &triangles, &twin, tolerance);

        let mut brep = Self {
            vertices,
            ..Self::default()
        };
        for region in &regions {
            let loops = boundary_loops(&triangles, &twin, &region_of, region);
            for face in split_into_faces(&brep.vertices, region.plane, loops) {
                brep.faces.push(face);
            }
        }
        // Register every edge now, in face order, so the emitted file lists
        // `EDGE_CURVE`s in a stable order regardless of how the regions grew.
        // The pairs are collected first because `Brep::edge` needs `&mut self`
        // while the loops it reads live inside the same struct.
        let pairs: Vec<(u32, u32)> = brep
            .faces
            .iter()
            .flat_map(|face| std::iter::once(&face.outer).chain(face.inner.iter()))
            .flat_map(|ring| {
                (0..ring.len()).map(move |index| (ring[index], ring[(index + 1) % ring.len()]))
            })
            .collect();
        for (from, to) in pairs {
            brep.edge(from, to);
        }
        brep
    }
}

/// Welds the triangle soup onto the kernel's own vertex tolerance.
///
/// Facets with a non-finite corner and facets that collapse to a line or a
/// point once welded are dropped: neither carries a boundary, and a
/// zero-length `EDGE_CURVE` is a degenerate curve that CAD kernels reject
/// outright rather than ignore.
fn weld(mesh: &Mesh) -> (Vec<Vec3>, Vec<[u32; 3]>) {
    let mut welder = VertexWelder::default();
    let mut triangles = Vec::with_capacity(mesh.triangles.len());
    for triangle in &mesh.triangles {
        let corners = triangle.vertices.map(|point| Vec3::new(point.x, point.y, point.z));
        if !corners.iter().all(|point| point.is_finite()) {
            continue;
        }
        let indices = corners.map(|point| welder.insert(point));
        if indices[0] == indices[1] || indices[1] == indices[2] || indices[0] == indices[2] {
            continue;
        }
        triangles.push(indices);
    }
    (welder.vertices, triangles)
}

/// Half-edge `h = 3 * triangle + corner` runs from corner `corner` to the next.
fn half_edge_ends(triangles: &[[u32; 3]], half_edge: usize) -> (u32, u32) {
    let triangle = triangles[half_edge / 3];
    let corner = half_edge % 3;
    (triangle[corner], triangle[(corner + 1) % 3])
}

fn next_half_edge(half_edge: usize) -> usize {
    half_edge - half_edge % 3 + (half_edge % 3 + 1) % 3
}

const NO_TWIN: u32 = u32::MAX;

/// Pairs each half-edge with the one running the other way.
///
/// A coherently oriented manifold has exactly one of each directed edge, so the
/// pairing is total. That is not assumed, and the way it is not assumed matters:
/// a directed edge carried by two or more facets — a non-manifold edge, or a
/// duplicated facet — marks its key *collided*, and a collided key pairs with
/// nothing on either side. Keeping the first claimant instead would pair one
/// half-edge with a twin that does not point back at it, and the asymmetry
/// survives a single cleanup pass: with `h1` and `h2` both running `(a,b)` and
/// `h3` running `(b,a)`, clearing the `h2`/`h3` pair on `h2`'s turn leaves
/// `twin[h1] = h3` against `twin[h3] = NO_TWIN`, and `h1` is never revisited.
/// The boundary walk would then see the edge as interior from one side and as
/// boundary from the other, which drops a ring and loses a whole face.
///
/// Unpaired half-edges simply land on a region boundary, which turns a defect
/// in the mesh into an open loop rather than into a wrong loop.
fn twin_map(triangles: &[[u32; 3]]) -> Vec<u32> {
    let count = triangles.len() * 3;
    // `NO_TWIN` cannot be a real half-edge index, so it doubles as the
    // collided-key marker.
    let mut directed: FastMap<(u32, u32), u32> = FastMap::default();
    directed.reserve(count);
    for half_edge in 0..count {
        directed
            .entry(half_edge_ends(triangles, half_edge))
            .and_modify(|slot| *slot = NO_TWIN)
            .or_insert(half_edge as u32);
    }
    let mut twin = vec![NO_TWIN; count];
    for (half_edge, slot) in twin.iter_mut().enumerate() {
        let ends = half_edge_ends(triangles, half_edge);
        if directed.get(&ends) != Some(&(half_edge as u32)) {
            // This half-edge's own key is shared, so it has no unambiguous
            // twin even if the opposite key is clean.
            continue;
        }
        if let Some(other) = directed.get(&(ends.1, ends.0)) {
            if *other != NO_TWIN {
                *slot = *other;
            }
        }
    }
    twin
}

/// Absolute coplanarity tolerance, scaled to the model so that a 1000 mm part
/// is not held to a 1 mm part's arithmetic.
fn coplanar_tolerance(vertices: &[Vec3]) -> f64 {
    let scale = Bounds::from_points(vertices.iter().copied())
        .map(|bounds| {
            let size = bounds.size();
            size.x.max(size.y).max(size.z).max(1.0)
        })
        .unwrap_or(1.0);
    scale * COPLANAR_RELATIVE_TOLERANCE
}

/// A maximal set of edge-connected, coplanar triangles.
struct Region {
    plane: Plane,
    triangles: Vec<usize>,
    id: u32,
}

/// The regions, plus `region_of[triangle]`.
///
/// The lookup is what lets the boundary walk ask "is my twin still in my
/// region" in O(1); the membership lists alone would make that a search.
struct Regions {
    regions: Vec<Region>,
    region_of: Vec<u32>,
}

/// Running area-weighted plane fit.
///
/// Newell sums are additive, so the plane can be refined as the region grows
/// without revisiting what is already in it. Weighting by area is the point:
/// it is what stops a sliver from tilting the plane away from the face the
/// sliver belongs to.
#[derive(Default)]
struct PlaneFit {
    normal_sum: Vec3,
    centroid_sum: Vec3,
    area_sum: f64,
}

impl PlaneFit {
    fn add(&mut self, a: Vec3, b: Vec3, c: Vec3) {
        let cross = b.sub(a).cross(c.sub(a));
        let area = cross.length() * 0.5;
        self.normal_sum = self.normal_sum.add(cross);
        self.centroid_sum = self
            .centroid_sum
            .add(a.add(b).add(c).mul(area / 3.0));
        self.area_sum += area;
    }

    fn plane(&self) -> Option<Plane> {
        if self.area_sum <= 0.0 || self.normal_sum.length() <= 0.0 {
            return None;
        }
        let normal = self.normal_sum.normalized();
        let centroid = self.centroid_sum.mul(1.0 / self.area_sum);
        Some(Plane::new(normal, normal.dot(centroid)))
    }
}

fn grow_regions(
    vertices: &[Vec3],
    triangles: &[[u32; 3]],
    twin: &[u32],
    tolerance: f64,
) -> Regions {
    let corners = |triangle: usize| -> [Vec3; 3] {
        triangles[triangle].map(|index| vertices[index as usize])
    };
    // Seeding from the largest triangle gives every region the best
    // conditioned starting plane it has available; a sliver seed would fit a
    // plane out of two near-parallel edges and then reject the face it belongs
    // to.
    let mut seeds: Vec<usize> = (0..triangles.len()).collect();
    let area = |triangle: usize| {
        let [a, b, c] = corners(triangle);
        b.sub(a).cross(c.sub(a)).length()
    };
    seeds.sort_by(|left, right| area(*right).total_cmp(&area(*left)));

    let mut region_of = vec![NO_TWIN; triangles.len()];
    let mut regions: Vec<Region> = Vec::new();
    let mut queue: Vec<usize> = Vec::new();
    for seed in seeds {
        if region_of[seed] != NO_TWIN {
            continue;
        }
        let id = regions.len() as u32;
        let mut fit = PlaneFit::default();
        let [a, b, c] = corners(seed);
        fit.add(a, b, c);
        let Some(mut plane) = fit.plane() else {
            // A triangle with no area at all: it has no plane to seed from and
            // no boundary to contribute, so it is left out of every region.
            region_of[seed] = u32::MAX - 1;
            continue;
        };
        region_of[seed] = id;
        let mut members = vec![seed];
        queue.clear();
        queue.push(seed);
        while let Some(triangle) = queue.pop() {
            for corner in 0..3 {
                let other = twin[triangle * 3 + corner];
                if other == NO_TWIN {
                    continue;
                }
                let candidate = other as usize / 3;
                if region_of[candidate] != NO_TWIN {
                    continue;
                }
                let points = corners(candidate);
                if points
                    .iter()
                    .any(|point| plane.distance(*point).abs() > tolerance)
                {
                    continue;
                }
                region_of[candidate] = id;
                members.push(candidate);
                fit.add(points[0], points[1], points[2]);
                if let Some(refined) = fit.plane() {
                    plane = refined;
                }
                queue.push(candidate);
            }
        }
        regions.push(Region {
            plane,
            triangles: members,
            id,
        });
    }
    // Members in triangle order, so the boundary walk starts from the same
    // half-edge however the growth happened to enqueue them and the emitted
    // loops come out in a stable order.
    for region in &mut regions {
        region.triangles.sort_unstable();
    }
    Regions { regions, region_of }
}

/// Walks a region's boundary half-edges into closed loops.
///
/// The next boundary half-edge after `h` is found by rotating around `h`'s head
/// through the region's own triangles until a half-edge leaves the region. That
/// is exact and needs no geometric tie-break, which matters at a pinch vertex —
/// a corner where the face touches itself — because there the loop's
/// continuation is not determined by the vertex alone.
fn boundary_loops(
    triangles: &[[u32; 3]],
    twin: &[u32],
    region_of: &[u32],
    region: &Region,
) -> Vec<Vec<u32>> {
    let is_boundary = |half_edge: usize| -> bool {
        let other = twin[half_edge];
        other == NO_TWIN || region_of[other as usize / 3] != region.id
    };

    let mut visited: FastMap<usize, ()> = FastMap::default();
    let mut loops = Vec::new();
    for triangle in &region.triangles {
        for corner in 0..3 {
            let start = triangle * 3 + corner;
            if !is_boundary(start) || visited.contains_key(&start) {
                continue;
            }
            let mut ring: Vec<u32> = Vec::new();
            let mut current = start;
            let limit = region.triangles.len() * 3;
            // Only a ring that walks back to where it started is emitted. Every
            // other exit — a step count past the region's own half-edge count,
            // a rotation that never finds the next boundary edge, a
            // continuation another ring already took — means the twin map is
            // inconsistent, and half a loop in an `EDGE_LOOP` is worse than no
            // loop: it is a face whose boundary does not close.
            let mut closed = false;
            for _ in 0..=limit {
                visited.insert(current, ());
                ring.push(half_edge_ends(triangles, current).0);
                let mut next = next_half_edge(current);
                let mut guard = 0usize;
                while !is_boundary(next) {
                    next = next_half_edge(twin[next] as usize);
                    guard += 1;
                    if guard > limit {
                        break;
                    }
                }
                if guard > limit {
                    break;
                }
                if next == start {
                    closed = true;
                    break;
                }
                if visited.contains_key(&next) {
                    break;
                }
                current = next;
            }
            if closed && ring.len() >= 3 {
                loops.push(ring);
            }
        }
    }
    loops
}

/// Splits a region's loops into faces: one outer boundary each, plus the holes
/// that fall inside it.
///
/// A connected planar region has exactly one outer loop, so the multi-outline
/// branch only fires on a region whose boundary pinched into two rings. Each
/// outline still becomes its own `ADVANCED_FACE`, which is what a CAD kernel
/// wants anyway.
fn split_into_faces(vertices: &[Vec3], plane: Plane, loops: Vec<Vec<u32>>) -> Vec<Face> {
    let (basis_u, basis_v) = plane_basis(plane);
    let project = |index: u32| -> Point2 {
        let point = vertices[index as usize];
        [point.dot(basis_u), point.dot(basis_v)]
    };

    let mut outlines: Vec<(Vec<u32>, Vec<Point2>, f64)> = Vec::new();
    let mut holes: Vec<(Vec<u32>, Vec<Point2>)> = Vec::new();
    for ring in loops {
        let projected: Vec<Point2> = ring.iter().map(|index| project(*index)).collect();
        let area = signed_area(&projected);
        if area > 0.0 {
            outlines.push((ring, projected, area));
        } else if area < 0.0 {
            holes.push((ring, projected));
        }
    }
    if outlines.is_empty() {
        // Nothing to hang the holes on. Only reachable on a region whose every
        // loop came out with zero or negative area, which means the region has
        // no outward-facing area at all — degenerate input, not a face.
        return Vec::new();
    }
    let mut faces: Vec<Face> = outlines
        .iter()
        .map(|(ring, _, _)| Face {
            plane,
            outer: ring.clone(),
            inner: Vec::new(),
        })
        .collect();
    for (ring, projected) in holes {
        // Smallest containing outline wins, so a hole inside an island inside a
        // hole lands on the island.
        let probe = projected[0];
        let mut best: Option<usize> = None;
        for (index, (_, outline, area)) in outlines.iter().enumerate() {
            if point_in_contour(probe, outline) {
                match best {
                    Some(current) if outlines[current].2 <= *area => {}
                    _ => best = Some(index),
                }
            }
        }
        // The region is one connected patch, so a hole in it is inside one of
        // its own outlines by construction; the point-in-contour test can only
        // miss when the probe lands exactly on an outline's edge. Falling back
        // to the largest outline keeps the loop in the file, because dropping
        // it would leave its edges used by one face instead of two — an open
        // shell, which is the one failure this writer must never produce.
        let index = best.unwrap_or_else(|| {
            outlines
                .iter()
                .enumerate()
                .max_by(|left, right| left.1 .2.total_cmp(&right.1 .2))
                .map(|(index, _)| index)
                .expect("outlines is non-empty")
        });
        faces[index].inner.push(ring);
    }
    faces
}

// ---------------------------------------------------------------------------
// ISO 10303-21 serialisation
// ---------------------------------------------------------------------------

/// Appends `#id = body;` lines and hands back the ids.
struct Part21 {
    text: String,
    next: u32,
    /// Ceiling on `text`, in bytes.
    budget: usize,
    /// Set the moment `text` crosses `budget`; entities stop being written
    /// from then on and [`Part21::finish`] refuses to hand the file back.
    over_budget: bool,
}

impl Part21 {
    /// Opens the file with its header already written.
    ///
    /// The header depends on nothing but the title, so writing it up front
    /// lets the whole file live in one buffer. Assembling it separately and
    /// copying the data section in behind it doubled peak memory for no
    /// reason, and STEP is by some way the largest thing this server
    /// serialises.
    fn new(name: &str, budget: usize) -> Self {
        let mut text = String::new();
        text.push_str("ISO-10303-21;\n");
        text.push_str("HEADER;\n");
        text.push_str("FILE_DESCRIPTION(('ReOpenSCAD exact polyhedral B-rep'),'2;1');\n");
        // A fixed timestamp keeps exports byte-for-byte reproducible, on the
        // same reasoning as the 3MF writer's fixed MS-DOS stamp.
        let _ = writeln!(
            text,
            "FILE_NAME('{name}','1970-01-01T00:00:00',(''),(''),\
             'ReOpenSCAD','ReOpenSCAD','');"
        );
        text.push_str("FILE_SCHEMA(('AUTOMOTIVE_DESIGN { 1 0 10303 214 3 1 1 }'));\n");
        text.push_str("ENDSEC;\n");
        text.push_str("DATA;\n");
        Self {
            text,
            next: 1,
            budget,
            over_budget: false,
        }
    }

    /// Writing through `format_args!` keeps every entity out of a temporary
    /// `String`, which matters: a merged face costs a handful of entities and a
    /// large model has hundreds of thousands of them.
    ///
    /// Past the budget the id is still handed out and the entity is silently
    /// dropped, so every caller's bookkeeping stays consistent and the walk
    /// finishes normally — only the buffer stops growing. Stopping here rather
    /// than at the caller is what bounds memory: STEP runs about ten times the
    /// size of the same model's STL, so the "build it, then measure it"
    /// pattern the other exporters use would let one request materialise
    /// hundreds of megabytes on its way to a rejection.
    fn entity(&mut self, body: Arguments<'_>) -> u32 {
        let id = self.next;
        self.next += 1;
        if self.over_budget {
            return id;
        }
        let _ = writeln!(self.text, "#{id}={body};");
        if self.text.len() > self.budget {
            self.over_budget = true;
        }
        id
    }

    /// Closes the file, or gives up if it outgrew its budget.
    fn finish(mut self) -> Option<Vec<u8>> {
        if self.over_budget {
            return None;
        }
        self.text.push_str("ENDSEC;\n");
        self.text.push_str("END-ISO-10303-21;\n");
        Some(self.text.into_bytes())
    }
}

macro_rules! entity {
    ($writer:expr, $($argument:tt)*) => {
        $writer.entity(format_args!($($argument)*))
    };
}

/// A STEP real literal always carries a decimal point; `10` is an integer in
/// Part 21 and an integer where a real is expected is a parse error in strict
/// readers. Rust's `Display` for `f64` never uses exponent notation, so the
/// output needs no exponent handling — only the point, and a guard against
/// `NaN`/`inf`, which Part 21 cannot express at all.
fn real(value: f64) -> String {
    if !value.is_finite() {
        return "0.".into();
    }
    let mut text = format!("{value}");
    if text == "-0" {
        text = "0".into();
    }
    if !text.contains('.') {
        text.push('.');
    }
    text
}

fn point_literal(point: Vec3) -> String {
    format!("({},{},{})", real(point.x), real(point.y), real(point.z))
}

/// Part 21 string escaping.
///
/// Quotes double, backslashes double, and anything outside printable ASCII
/// goes through the `\X2\` UTF-16 escape the standard defines — a raw UTF-8
/// byte in a Part 21 string is not portable, and a workspace name is arbitrary
/// user text.
fn escape(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut pending: Option<String> = None;
    for character in text.chars() {
        let printable = matches!(character, ' '..='~');
        if printable {
            if let Some(escaped) = pending.take() {
                output.push_str("\\X2\\");
                output.push_str(&escaped);
                output.push_str("\\X0\\");
            }
            match character {
                '\'' => output.push_str("''"),
                '\\' => output.push_str("\\\\"),
                other => output.push(other),
            }
        } else if (character as u32) < 0x20 || character == '\u{7f}' {
            // Control characters carry no meaning in a STEP name and several
            // readers choke on them; drop them the way the 3MF writer does.
            continue;
        } else {
            let buffer = pending.get_or_insert_with(String::new);
            let mut units = [0u16; 2];
            for unit in character.encode_utf16(&mut units) {
                let _ = write!(buffer, "{unit:04X}");
            }
        }
    }
    if let Some(escaped) = pending {
        output.push_str("\\X2\\");
        output.push_str(&escaped);
        output.push_str("\\X0\\");
    }
    output
}

fn id_list(ids: &[u32]) -> String {
    let mut text = String::with_capacity(ids.len() * 7 + 2);
    text.push('(');
    for (position, id) in ids.iter().enumerate() {
        if position > 0 {
            text.push(',');
        }
        let _ = write!(text, "#{id}");
    }
    text.push(')');
    text
}

/// Serialise `mesh` as an AP214 `MANIFOLD_SOLID_BREP`, or `None` when the file
/// would exceed `budget` bytes.
///
/// `title` names the product and the file, and is the only place the caller's
/// text reaches the output.
pub fn write(mesh: &Mesh, title: &str, budget: usize) -> Option<Vec<u8>> {
    let mut brep = Brep::recover(mesh);
    let name = escape(title);

    let mut part = Part21::new(&name, budget);

    // --- units and representation context ---------------------------------
    //
    // The complex-entity syntax `(A()B()C())` is how Part 21 spells an instance
    // of a multiply-inherited ENTITY; the sub-entities must appear in
    // alphabetical order, which is why `NAMED_UNIT(*)` sits between the others
    // rather than first. `*` is the redeclared-inherited attribute.
    let length_unit = entity!(part, "(LENGTH_UNIT()NAMED_UNIT(*)SI_UNIT(.MILLI.,.METRE.))");
    let angle_unit = entity!(part, "(NAMED_UNIT(*)PLANE_ANGLE_UNIT()SI_UNIT($,.RADIAN.))");
    let solid_angle_unit = entity!(part, "(NAMED_UNIT(*)SI_UNIT($,.STERADIAN.)SOLID_ANGLE_UNIT())");
    let uncertainty = entity!(
        part,
        "UNCERTAINTY_MEASURE_WITH_UNIT(LENGTH_MEASURE({}),#{length_unit},\
         'distance_accuracy_value','confusion accuracy')",
        real(DISTANCE_ACCURACY)
    );
    let context = entity!(
        part,
        "(GEOMETRIC_REPRESENTATION_CONTEXT(3)\
         GLOBAL_UNCERTAINTY_ASSIGNED_CONTEXT((#{uncertainty}))\
         GLOBAL_UNIT_ASSIGNED_CONTEXT((#{length_unit},#{angle_unit},#{solid_angle_unit}))\
         REPRESENTATION_CONTEXT('Context','3D'))"
    );

    let origin = entity!(part, "CARTESIAN_POINT('',(0.,0.,0.))");
    let world_z = entity!(part, "DIRECTION('',(0.,0.,1.))");
    let world_x = entity!(part, "DIRECTION('',(1.,0.,0.))");
    let world = entity!(
        part,
        "AXIS2_PLACEMENT_3D('',#{origin},#{world_z},#{world_x})"
    );

    let shape = if brep.faces.is_empty() {
        // An empty `CLOSED_SHELL` is not a legal solid, so a model with no
        // geometry becomes a representation with no items rather than an
        // unreadable file.
        entity!(part, "SHAPE_REPRESENTATION('',(#{world}),#{context})")
    } else {
        let mut items = vec![world];
        items.extend(write_solids(&mut part, &mut brep));
        entity!(
            part,
            "ADVANCED_BREP_SHAPE_REPRESENTATION('',{},#{context})",
            id_list(&items)
        )
    };

    // --- product structure -------------------------------------------------
    let application = entity!(part, "APPLICATION_CONTEXT('automotive design')");
    entity!(
        part,
        "APPLICATION_PROTOCOL_DEFINITION('international standard','automotive_design',2000,\
         #{application})"
    );
    let product_context = entity!(part, "PRODUCT_CONTEXT('',#{application},'mechanical')");
    let product = entity!(
        part,
        "PRODUCT('{name}','{name}','',(#{product_context}))"
    );
    let formation = entity!(part, "PRODUCT_DEFINITION_FORMATION('','',#{product})");
    let definition_context = entity!(
        part,
        "PRODUCT_DEFINITION_CONTEXT('part definition',#{application},'design')"
    );
    let definition = entity!(
        part,
        "PRODUCT_DEFINITION('design','',#{formation},#{definition_context})"
    );
    let definition_shape = entity!(part, "PRODUCT_DEFINITION_SHAPE('','',#{definition})");
    entity!(
        part,
        "SHAPE_DEFINITION_REPRESENTATION(#{definition_shape},#{shape})"
    );
    // Without a category most importers treat the product as an assembly
    // placeholder and silently drop the shape.
    entity!(
        part,
        "PRODUCT_RELATED_PRODUCT_CATEGORY('part','',(#{product}))"
    );

    part.finish()
}

/// Emits the geometry and topology, returning one solid id per body.
fn write_solids(part: &mut Part21, brep: &mut Brep) -> Vec<u32> {
    // The bodies are worked out first, because deciding them can *change* a
    // face: an inside-out shell that belongs to no body has to be turned the
    // right way round before its `ADVANCED_FACE`s are written, not after.
    // Reading the edge ids back out of `edge_ids` costs a hash lookup per ring
    // edge and saves threading them out of the emission loop.
    let face_edges: Vec<Vec<u32>> = brep
        .faces
        .iter()
        .map(|face| {
            std::iter::once(&face.outer)
                .chain(face.inner.iter())
                .flat_map(|ring| {
                    (0..ring.len()).map(move |index| {
                        let (from, to) = (ring[index], ring[(index + 1) % ring.len()]);
                        let key = if from < to { (from, to) } else { (to, from) };
                        key
                    })
                })
                .map(|key| brep.edge_ids[&key])
                .collect()
        })
        .collect();
    let plan = plan_shells(brep, &face_edges);

    // Only vertices some face actually uses get emitted; welding can leave
    // orphans behind when a degenerate facet is dropped.
    let mut used = vec![false; brep.vertices.len()];
    for face in &brep.faces {
        for ring in std::iter::once(&face.outer).chain(face.inner.iter()) {
            for index in ring {
                used[*index as usize] = true;
            }
        }
    }

    // `CARTESIAN_POINT`s are shared between the `VERTEX_POINT` that names the
    // corner and the `LINE` that starts there. Part 21 has no ownership rule
    // that forbids it, every reader resolves entities by id, and on a model
    // with 100k vertices it is 100k fewer points to parse.
    let mut points = vec![0u32; brep.vertices.len()];
    let mut vertex_points = vec![0u32; brep.vertices.len()];
    for (index, vertex) in brep.vertices.iter().enumerate() {
        if !used[index] {
            continue;
        }
        let point = entity!(part, "CARTESIAN_POINT('',{})", point_literal(*vertex));
        points[index] = point;
        vertex_points[index] = entity!(part, "VERTEX_POINT('',#{point})");
    }

    let mut directions: FastMap<[u64; 3], u32> = FastMap::default();
    let mut direction = |part: &mut Part21, vector: Vec3| -> u32 {
        let key = [
            vector.x.to_bits(),
            vector.y.to_bits(),
            vector.z.to_bits(),
        ];
        if let Some(id) = directions.get(&key) {
            return *id;
        }
        let id = entity!(part, "DIRECTION('',{})", point_literal(vector));
        directions.insert(key, id);
        id
    };

    // A `VECTOR` is fully determined by its direction once the magnitude is
    // fixed, so axis-aligned models collapse thousands of them onto six.
    let mut vectors: FastMap<u32, u32> = FastMap::default();
    let mut edge_curves = Vec::with_capacity(brep.edges.len());
    for (from, to) in &brep.edges {
        let start = brep.vertices[*from as usize];
        let end = brep.vertices[*to as usize];
        let along = direction(part, end.sub(start).normalized());
        // `VECTOR`'s magnitude is the curve's parameterisation scale, not the
        // edge's length: the edge's extent comes from its two vertices, so a
        // unit vector is both correct and what every kernel writes.
        let vector = match vectors.get(&along) {
            Some(id) => *id,
            None => {
                let id = entity!(part, "VECTOR('',#{along},1.)");
                vectors.insert(along, id);
                id
            }
        };
        let line = entity!(part, "LINE('',#{},#{vector})", points[*from as usize]);
        edge_curves.push(entity!(
            part,
            "EDGE_CURVE('',#{},#{},#{line},.T.)",
            vertex_points[*from as usize],
            vertex_points[*to as usize]
        ));
    }

    // Edge ids are assigned in face order by `Brep::recover`, so re-deriving
    // them here walks the same map and cannot disagree.
    let edge_id = |from: u32, to: u32| -> (u32, bool) {
        let forward = from < to;
        let key = if forward { (from, to) } else { (to, from) };
        (brep.edge_ids[&key], forward)
    };

    let mut faces = Vec::with_capacity(brep.faces.len());
    let mut oriented = Vec::new();
    for face in &brep.faces {
        let (basis_u, _) = plane_basis(face.plane);
        let axis = direction(part, face.plane.normal);
        let reference = direction(part, basis_u);
        // Anchoring the placement on a corner of the face rather than on
        // `normal * offset` keeps the plane's origin on a coordinate the rest
        // of the file already states exactly.
        let anchor = points[face.outer[0] as usize];
        let placement = entity!(
            part,
            "AXIS2_PLACEMENT_3D('',#{anchor},#{axis},#{reference})"
        );
        let surface = entity!(part, "PLANE('',#{placement})");

        let mut bounds = Vec::with_capacity(1 + face.inner.len());
        for (position, ring) in std::iter::once(&face.outer)
            .chain(face.inner.iter())
            .enumerate()
        {
            oriented.clear();
            for index in 0..ring.len() {
                let (edge, forward) = edge_id(ring[index], ring[(index + 1) % ring.len()]);
                let sense = if forward { ".T." } else { ".F." };
                oriented.push(entity!(
                    part,
                    "ORIENTED_EDGE('',*,*,#{},{sense})",
                    edge_curves[edge as usize]
                ));
            }
            let edge_loop = entity!(part, "EDGE_LOOP('',{})", id_list(&oriented));
            // The outer ring winds counter-clockwise about the face normal and
            // the holes wind clockwise — already the sense `FACE_OUTER_BOUND`
            // and `FACE_BOUND` expect — so the orientation flag stays `.T.`
            // and the winding carries the meaning.
            bounds.push(if position == 0 {
                entity!(part, "FACE_OUTER_BOUND('',#{edge_loop},.T.)")
            } else {
                entity!(part, "FACE_BOUND('',#{edge_loop},.T.)")
            });
        }
        faces.push(entity!(
            part,
            "ADVANCED_FACE('',{},#{surface},.T.)",
            id_list(&bounds)
        ));
    }

    write_shells(part, &plan, &faces)
}

/// How the faces divide into bodies, and which bodies own which cavities.
struct ShellPlan {
    /// Component id per face, dense from zero.
    components: Vec<usize>,
    count: usize,
    /// Cavity component ids owned by each body component.
    voids: Vec<Vec<usize>>,
    /// Components that are cavities owned by some body, and so must not be
    /// emitted as solids in their own right.
    enclosed: Vec<bool>,
}

/// Groups the faces into bodies and emits one solid per body.
///
/// A `CLOSED_SHELL` is a *connected* face set — `connected_face_set` is
/// literally its supertype — so one shell holding every face of a model with
/// two separate bodies, or of a hollow box, is outside the schema even though
/// OpenCASCADE repairs it on import. Two things fall out of splitting it
/// properly: disjoint bodies arrive as the several solids they are, and a
/// cavity becomes a `BREP_WITH_VOIDS` rather than a second solid sitting
/// inside the first, which is the difference between a hollow box measuring
/// 7000 mm3 and a reader deciding it is 8000 mm3 of material with a 1000 mm3
/// lump in the middle.
fn plan_shells(brep: &mut Brep, face_edges: &[Vec<u32>]) -> ShellPlan {
    let components = connected_components(brep.edges.len(), face_edges);
    let count = components.iter().copied().max().map_or(0, |id| id + 1);

    // Signed volume by the divergence theorem: a planar face's contribution is
    // `offset * area / 3`, where `offset` is already the signed distance from
    // the origin to the face's plane and `area` is the outer loop's area less
    // its holes'. A shell bounding material comes out positive; a shell whose
    // faces point inward — the surface of a cavity — comes out negative, and
    // that sign is the whole classification.
    let mut volumes = vec![0.0f64; count];
    let mut bounds: Vec<Option<Bounds>> = vec![None; count];
    for (index, face) in brep.faces.iter().enumerate() {
        let component = components[index];
        let (basis_u, basis_v) = plane_basis(face.plane);
        let mut area = 0.0;
        for ring in std::iter::once(&face.outer).chain(face.inner.iter()) {
            let projected: Vec<Point2> = ring
                .iter()
                .map(|vertex| {
                    let point = brep.vertices[*vertex as usize];
                    [point.dot(basis_u), point.dot(basis_v)]
                })
                .collect();
            area += signed_area(&projected);
        }
        volumes[component] += face.plane.offset * area / 3.0;
        let points = face.outer.iter().map(|vertex| brep.vertices[*vertex as usize]);
        let face_bounds = Bounds::from_points(points);
        bounds[component] = match (bounds[component], face_bounds) {
            (Some(current), Some(extra)) => Some(Bounds {
                min: current.min.min(extra.min),
                max: current.max.max(extra.max),
            }),
            (current, extra) => current.or(extra),
        };
    }

    // Each cavity belongs to the tightest body that encloses it, decided on
    // axis-aligned bounding boxes. That is a heuristic, not a proof: a cavity
    // is strictly inside its body's box by construction, but so is anything
    // else inside that box, so a cavity sitting in the notch of one concave
    // body while a second body's box spans it can be attributed to the wrong
    // one. Picking the smallest enclosing box is what makes that unlikely
    // rather than impossible; the alternative is a point-in-solid ray cast per
    // cavity, which is a different order of work for a case CSG output does
    // not produce. The box is taken from each face's outer ring alone, which
    // is exact because a hole is in-plane and inside its own outer ring.
    let mut voids: Vec<Vec<usize>> = vec![Vec::new(); count];
    let mut enclosed = vec![false; count];
    let mut orphan: Vec<usize> = Vec::new();
    for (cavity, volume) in volumes.iter().enumerate() {
        if *volume >= 0.0 {
            continue;
        }
        let Some(inner) = bounds[cavity] else {
            continue;
        };
        let mut best: Option<usize> = None;
        for body in 0..count {
            if volumes[body] <= 0.0 {
                continue;
            }
            let Some(outer) = bounds[body] else { continue };
            if !contains(outer, inner) {
                continue;
            }
            match best {
                Some(current) if volumes[current] <= volumes[body] => {}
                _ => best = Some(body),
            }
        }
        match best {
            Some(body) => {
                voids[body].push(cavity);
                enclosed[cavity] = true;
            }
            // An inside-out shell with nothing around it is not a cavity, it is
            // a mesh defect. Dropping it would lose geometry the caller asked
            // to export; emitting it as it stands would be worse, because
            // `manifold_solid_brep.outer` must have its normals pointing out of
            // the material and this one's point in — importers read that as
            // negative volume. Turning it the right way round keeps the
            // geometry *and* the file valid. Edge identity is the unordered
            // vertex pair, so reversing the rings cannot disturb the shared
            // `EDGE_CURVE`s; only the `ORIENTED_EDGE` senses change, and those
            // are derived from the ring direction when the face is written.
            None => orphan.push(cavity),
        }
    }
    for face in brep
        .faces
        .iter_mut()
        .enumerate()
        .filter(|(index, _)| orphan.contains(&components[*index]))
        .map(|(_, face)| face)
    {
        face.outer.reverse();
        for hole in &mut face.inner {
            hole.reverse();
        }
        face.plane = face.plane.flipped();
    }

    ShellPlan {
        components,
        count,
        voids,
        enclosed,
    }
}

/// Emits one `CLOSED_SHELL` per body and one solid per body that is not itself
/// a cavity.
fn write_shells(part: &mut Part21, plan: &ShellPlan, faces: &[u32]) -> Vec<u32> {
    // Bucket the faces in one pass rather than rescanning them per component:
    // a model built from a grid of small parts has hundreds of bodies, and the
    // obvious nested loop is quadratic in that.
    let mut members: Vec<Vec<u32>> = vec![Vec::new(); plan.count];
    for (index, face) in faces.iter().enumerate() {
        members[plan.components[index]].push(*face);
    }
    let shells: Vec<u32> = members
        .iter()
        .map(|faces| entity!(part, "CLOSED_SHELL('',{})", id_list(faces)))
        .collect();

    let mut solids = Vec::new();
    for (component, shell) in shells.iter().enumerate() {
        if plan.enclosed[component] {
            continue;
        }
        if plan.voids[component].is_empty() {
            solids.push(entity!(part, "MANIFOLD_SOLID_BREP('',#{shell})"));
            continue;
        }
        // The cavity's faces already point into the void, which is the sense
        // ISO 10303-42 asks of a void shell, so the orientation flag keeps
        // them rather than reversing them.
        let oriented: Vec<u32> = plan.voids[component]
            .iter()
            .map(|cavity| {
                let void_shell = shells[*cavity];
                entity!(part, "ORIENTED_CLOSED_SHELL('',*,#{void_shell},.T.)")
            })
            .collect();
        solids.push(entity!(
            part,
            "BREP_WITH_VOIDS('',#{shell},{})",
            id_list(&oriented)
        ));
    }
    solids
}

/// Union-find over faces joined by a shared edge.
fn connected_components(edges: usize, face_edges: &[Vec<u32>]) -> Vec<usize> {
    let mut parent: Vec<usize> = (0..face_edges.len()).collect();
    fn find(parent: &mut [usize], mut node: usize) -> usize {
        while parent[node] != node {
            parent[node] = parent[parent[node]];
            node = parent[node];
        }
        node
    }
    let mut first_face: Vec<Option<usize>> = vec![None; edges];
    for (face, used) in face_edges.iter().enumerate() {
        for edge in used {
            match first_face[*edge as usize] {
                Some(other) => {
                    let (left, right) = (find(&mut parent, face), find(&mut parent, other));
                    if left != right {
                        parent[left] = right;
                    }
                }
                None => first_face[*edge as usize] = Some(face),
            }
        }
    }
    // Renumber to dense component ids in face order, so the emitted shells
    // follow the faces rather than the union-find's internal representatives.
    let mut label: FastMap<usize, usize> = FastMap::default();
    (0..face_edges.len())
        .map(|face| {
            let root = find(&mut parent, face);
            let next = label.len();
            *label.entry(root).or_insert(next)
        })
        .collect()
}

fn contains(outer: Bounds, inner: Bounds) -> bool {
    outer.min.x <= inner.min.x
        && outer.min.y <= inner.min.y
        && outer.min.z <= inner.min.z
        && outer.max.x >= inner.max.x
        && outer.max.y >= inner.max.y
        && outer.max.z >= inner.max.z
}

#[cfg(test)]
mod tests {
    /// `write` takes an output budget and refuses anything past it, which is
    /// the server's protection rather than a property these tests are about.
    /// They all hand it an unlimited one and unwrap — a test model that did
    /// not fit would be a bug in the test, not a case to handle.
    fn rendered(mesh: &Mesh, title: &str) -> Vec<u8> {
        write(mesh, title, usize::MAX).expect("the test model fits any budget")
    }

    use super::*;
    use crate::engine::{self, Quality};
    use std::collections::{HashMap, HashSet};

    fn compile(source: &str) -> engine::Mesh {
        engine::compile(source, Quality::Render, None)
            .expect("compiles")
            .mesh
    }

    fn step(source: &str) -> String {
        String::from_utf8(rendered(&compile(source), "unit test")).expect("STEP is text")
    }

    /// One `#id = TYPE(...)` record, with the ids it references.
    struct Record {
        kind: String,
        body: String,
        references: Vec<u32>,
    }

    /// Parses the DATA section into records.
    ///
    /// Deliberately a real parse of the emitted text rather than a look at the
    /// writer's own bookkeeping: the invariants below are properties of the
    /// file a CAD package will read, and a bug that leaves the writer's tables
    /// consistent while emitting something else is exactly the bug worth
    /// catching.
    fn records(text: &str) -> HashMap<u32, Record> {
        let data = text
            .split_once("DATA;\n")
            .expect("DATA section")
            .1
            .rsplit_once("ENDSEC;")
            .expect("DATA terminator")
            .0;
        let mut records = HashMap::new();
        for line in data.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let line = line.strip_suffix(';').expect("record terminator");
            let (id, body) = line.split_once('=').expect("record assignment");
            let id: u32 = id.trim_start_matches('#').parse().expect("entity id");
            let body = body.trim().to_string();
            let kind = body
                .split(['(', ' '])
                .next()
                .unwrap_or_default()
                .to_string();
            let mut references = Vec::new();
            let bytes = body.as_bytes();
            let mut cursor = 0;
            while cursor < bytes.len() {
                if bytes[cursor] == b'#' {
                    let start = cursor + 1;
                    let mut end = start;
                    while end < bytes.len() && bytes[end].is_ascii_digit() {
                        end += 1;
                    }
                    references.push(body[start..end].parse().expect("reference id"));
                    cursor = end;
                } else {
                    cursor += 1;
                }
            }
            assert!(
                records
                    .insert(
                        id,
                        Record {
                            kind,
                            body,
                            references,
                        }
                    )
                    .is_none(),
                "#{id} defined twice"
            );
        }
        records
    }

    fn of_kind<'a>(records: &'a HashMap<u32, Record>, kind: &str) -> Vec<(&'a u32, &'a Record)> {
        let mut found: Vec<_> = records
            .iter()
            .filter(|(_, record)| record.kind == kind)
            .collect();
        found.sort_by_key(|(id, _)| **id);
        found
    }

    /// Every `#id` a record mentions has to be defined, or the importer hits a
    /// dangling pointer and gives up on the whole file.
    #[test]
    fn the_entity_graph_has_no_dangling_references() {
        for source in [
            "cube(10);",
            "difference() { cube(20, center=true); cylinder(h=40, r=4, center=true, $fn=16); }",
            "sphere(r=8, $fn=24);",
        ] {
            let text = step(source);
            let records = records(&text);
            assert!(!records.is_empty(), "{source} produced no entities");
            for (id, record) in &records {
                for reference in &record.references {
                    assert!(
                        records.contains_key(reference),
                        "#{id} ({}) references undefined #{reference} in {source}",
                        record.kind
                    );
                }
            }
        }
    }

    #[test]
    fn the_file_carries_the_part21_frame_and_millimetres() {
        let text = step("cube(10);");
        assert!(text.starts_with("ISO-10303-21;\n"));
        assert!(text.trim_end().ends_with("END-ISO-10303-21;"));
        assert!(text.contains("\nHEADER;\n"));
        assert!(text.contains("FILE_DESCRIPTION(("));
        assert!(text.contains("FILE_NAME('unit test'"));
        assert!(text.contains("FILE_SCHEMA(('AUTOMOTIVE_DESIGN { 1 0 10303 214 3 1 1 }'));"));
        assert!(text.contains("\nDATA;\n"));
        assert_eq!(text.matches("ENDSEC;").count(), 2);
        assert!(text.contains("SI_UNIT(.MILLI.,.METRE.)"));
        assert!(text.contains("GEOMETRIC_REPRESENTATION_CONTEXT(3)"));
        assert!(text.contains("GLOBAL_UNCERTAINTY_ASSIGNED_CONTEXT"));
        // The shape has to hang off a product definition or importers treat the
        // file as an empty assembly.
        assert!(text.contains("SHAPE_DEFINITION_REPRESENTATION"));
        assert!(text.contains("PRODUCT_RELATED_PRODUCT_CATEGORY"));
        assert!(text.contains("ADVANCED_BREP_SHAPE_REPRESENTATION"));
    }

    /// The whole reason to emit STEP instead of a mesh: a cube is six faces,
    /// not twelve triangles, and its corners are eight vertices.
    #[test]
    fn a_cube_is_six_advanced_faces_over_twelve_shared_edges() {
        let text = step("cube(10);");
        let records = records(&text);
        assert_eq!(of_kind(&records, "ADVANCED_FACE").len(), 6);
        assert_eq!(of_kind(&records, "EDGE_CURVE").len(), 12);
        assert_eq!(of_kind(&records, "VERTEX_POINT").len(), 8);
        assert_eq!(of_kind(&records, "FACE_OUTER_BOUND").len(), 6);
        assert_eq!(of_kind(&records, "FACE_BOUND").len(), 0);
        assert_eq!(of_kind(&records, "CLOSED_SHELL").len(), 1);
        assert_eq!(of_kind(&records, "MANIFOLD_SOLID_BREP").len(), 1);
        // Euler on a closed shell: V - E + F = 2.
        assert_eq!(8 - 12 + 6, 2);
        // Every face is a quad, so each loop holds four oriented edges.
        for (_, record) in of_kind(&records, "EDGE_LOOP") {
            assert_eq!(record.references.len(), 4, "{}", record.body);
        }
    }

    /// The property that decides whether the file imports as a solid or as a
    /// heap of loose surfaces.
    #[test]
    fn every_edge_curve_is_shared_by_exactly_two_faces() {
        for source in [
            "cube(10);",
            "difference() { cube(20, center=true); cylinder(h=40, r=4, center=true, $fn=16); }",
            "cylinder(h=10, r=5, $fn=12);",
            "union() { cube(10); translate([10,0,0]) cube(10); }",
        ] {
            let text = step(source);
            let records = records(&text);
            // ORIENTED_EDGE -> EDGE_CURVE, counted.
            let mut uses: HashMap<u32, usize> = HashMap::new();
            for (_, record) in of_kind(&records, "ORIENTED_EDGE") {
                let curve = *record.references.first().expect("oriented edge curve");
                assert_eq!(records[&curve].kind, "EDGE_CURVE");
                *uses.entry(curve).or_default() += 1;
            }
            let curves: HashSet<u32> = of_kind(&records, "EDGE_CURVE")
                .into_iter()
                .map(|(id, _)| *id)
                .collect();
            assert_eq!(uses.len(), curves.len(), "unused EDGE_CURVE in {source}");
            for (curve, count) in &uses {
                assert_eq!(
                    *count, 2,
                    "EDGE_CURVE #{curve} is used {count} times in {source}, not twice"
                );
            }
            // ... and in opposite directions, which is what makes the shell
            // closed rather than merely connected.
            let mut senses: HashMap<u32, Vec<bool>> = HashMap::new();
            for (_, record) in of_kind(&records, "ORIENTED_EDGE") {
                let curve = record.references[0];
                senses
                    .entry(curve)
                    .or_default()
                    .push(record.body.ends_with(".T.)"));
            }
            for (curve, seen) in senses {
                assert_eq!(
                    seen.iter().filter(|sense| **sense).count(),
                    1,
                    "EDGE_CURVE #{curve} is traversed the same way twice in {source}"
                );
            }
        }
    }

    /// Two faces meeting at an edge have to name the same corners, not two
    /// copies that happen to be at the same coordinates.
    #[test]
    fn adjacent_faces_reference_the_same_vertex_points() {
        let text = step("cube(10);");
        let records = records(&text);
        let mut corners_per_face: Vec<HashSet<u32>> = Vec::new();
        for (_, face) in of_kind(&records, "ADVANCED_FACE") {
            let mut corners = HashSet::new();
            for bound in &face.references {
                let Some(bound) = records.get(bound) else {
                    continue;
                };
                if !bound.kind.ends_with("BOUND") {
                    continue;
                }
                for edge_loop in &bound.references {
                    for oriented in &records[edge_loop].references {
                        let curve = records[oriented].references[0];
                        corners.insert(records[&curve].references[0]);
                        corners.insert(records[&curve].references[1]);
                    }
                }
            }
            corners_per_face.push(corners);
        }
        assert_eq!(corners_per_face.len(), 6);
        // Opposite faces of a cube share nothing; adjacent ones share exactly
        // the two corners of the edge between them. Across all pairs that is
        // twelve adjacencies, one per edge.
        let shared = corners_per_face
            .iter()
            .enumerate()
            .flat_map(|(index, left)| {
                corners_per_face[index + 1..]
                    .iter()
                    .map(move |right| left.intersection(right).count())
            })
            .filter(|count| *count == 2)
            .count();
        assert_eq!(shared, 12, "a cube has twelve shared edges");
    }

    /// The face-merging pass is what makes a wall one face, so the number of
    /// vertices in the file has to be the number of corners the welded solid
    /// has — not three per triangle.
    #[test]
    fn the_vertex_count_matches_the_welded_solid() {
        for (source, expected) in [("cube(10);", 8usize), ("cylinder(h=10, r=5, $fn=12);", 24)] {
            let mesh = compile(source);
            let (welded, _) = weld(&mesh);
            let text = String::from_utf8(rendered(&mesh, "unit test")).unwrap();
            let records = records(&text);
            assert_eq!(
                of_kind(&records, "VERTEX_POINT").len(),
                welded.len(),
                "{source}"
            );
            assert_eq!(welded.len(), expected, "{source}");
            assert_eq!(
                of_kind(&records, "CARTESIAN_POINT").len(),
                // One per vertex, plus the single origin the world placement
                // uses; the plane placements and lines reuse vertex points.
                welded.len() + 1,
                "{source}"
            );
        }
    }

    /// A through-hole is the case that separates a B-rep writer from a
    /// triangle dumper: the face around the hole needs an inner bound, and the
    /// bound has to wind against the outer one.
    #[test]
    fn a_through_hole_produces_a_face_with_an_inner_bound() {
        let text = step("difference() { cube([20,20,5]); translate([10,10,-1]) cylinder(h=7, r=4, $fn=16); }");
        let records = records(&text);
        let inner = of_kind(&records, "FACE_BOUND");
        assert_eq!(
            inner.len(),
            2,
            "the top and bottom faces each keep one hole loop"
        );
        for (_, bound) in &inner {
            let edge_loop = &records[&bound.references[0]];
            assert_eq!(edge_loop.kind, "EDGE_LOOP");
            assert_eq!(
                edge_loop.references.len(),
                16,
                "the hole keeps all sixteen of the cylinder's edges"
            );
        }
        // Both holed faces are still one face each, with one outer and one
        // inner bound.
        let holed: Vec<_> = of_kind(&records, "ADVANCED_FACE")
            .into_iter()
            .filter(|(_, face)| {
                face.references
                    .iter()
                    .any(|id| records[id].kind == "FACE_BOUND")
            })
            .collect();
        assert_eq!(holed.len(), 2);
        for (_, face) in holed {
            let bounds: Vec<&str> = face
                .references
                .iter()
                .filter(|id| records[id].kind.ends_with("BOUND"))
                .map(|id| records[id].kind.as_str())
                .collect();
            assert_eq!(bounds, vec!["FACE_OUTER_BOUND", "FACE_BOUND"]);
        }
    }

    /// Winding is the whole meaning of a bound here, since every orientation
    /// flag is `.T.`: the outer loop must run counter-clockwise about the
    /// face's plane normal and the hole clockwise.
    #[test]
    fn hole_loops_wind_against_their_outer_loop() {
        let mesh = compile(
            "difference() { cube([20,20,5]); translate([10,10,-1]) cylinder(h=7, r=4, $fn=16); }",
        );
        let brep = Brep::recover(&mesh);
        let holed: Vec<&Face> = brep
            .faces
            .iter()
            .filter(|face| !face.inner.is_empty())
            .collect();
        assert_eq!(holed.len(), 2);
        for face in holed {
            let (basis_u, basis_v) = plane_basis(face.plane);
            let project = |ring: &[u32]| -> Vec<Point2> {
                ring.iter()
                    .map(|index| {
                        let point = brep.vertices[*index as usize];
                        [point.dot(basis_u), point.dot(basis_v)]
                    })
                    .collect()
            };
            assert!(signed_area(&project(&face.outer)) > 0.0);
            for hole in &face.inner {
                assert!(signed_area(&project(hole)) < 0.0);
            }
        }
    }

    /// Curved surfaces must stay faceted: merging a cylinder's wall into one
    /// "plane" would be a silent geometry change, not a simplification.
    #[test]
    fn a_tessellated_cylinder_keeps_one_face_per_facet() {
        let text = step("cylinder(h=10, r=5, $fn=12);");
        let records = records(&text);
        // Twelve wall facets plus the two caps.
        assert_eq!(of_kind(&records, "ADVANCED_FACE").len(), 14);
        // Each cap is a single twelve-sided face, not a fan.
        let dodecagons = of_kind(&records, "EDGE_LOOP")
            .into_iter()
            .filter(|(_, record)| record.references.len() == 12)
            .count();
        assert_eq!(dodecagons, 2);
    }

    #[test]
    fn a_two_cube_union_merges_the_wall_they_share_away() {
        // The shared 10x10 wall is interior, so the union is one box: six
        // faces, eight corners, twelve edges.
        let text = step("union() { cube(10); translate([10,0,0]) cube(10); }");
        let records = records(&text);
        assert_eq!(of_kind(&records, "ADVANCED_FACE").len(), 6);
        assert_eq!(of_kind(&records, "VERTEX_POINT").len(), 8);
        assert_eq!(of_kind(&records, "EDGE_CURVE").len(), 12);
    }

    /// A concave outline has to survive as one face with its reflex corners,
    /// not be split back into the convex pieces the mesher used.
    #[test]
    fn an_l_shaped_face_stays_one_face_with_six_corners() {
        let text = step("union() { cube([20,10,5]); cube([10,20,5]); }");
        let records = records(&text);
        assert_eq!(
            of_kind(&records, "ADVANCED_FACE").len(),
            8,
            "an L-prism has two L faces and six walls"
        );
        let hexagons = of_kind(&records, "EDGE_LOOP")
            .into_iter()
            .filter(|(_, record)| record.references.len() == 6)
            .count();
        assert_eq!(hexagons, 2, "the two L faces keep all six corners");
    }

    /// A `CLOSED_SHELL` is a connected face set, so two bodies that never
    /// touch must not be crammed into one.
    #[test]
    fn disjoint_bodies_become_separate_solids() {
        let text = step("cube(10); translate([20,0,0]) cube(10);");
        let records = records(&text);
        assert_eq!(of_kind(&records, "CLOSED_SHELL").len(), 2);
        assert_eq!(of_kind(&records, "MANIFOLD_SOLID_BREP").len(), 2);
        assert_eq!(of_kind(&records, "BREP_WITH_VOIDS").len(), 0);
        // Both solids have to reach the representation, or one body is lost.
        let representation = of_kind(&records, "ADVANCED_BREP_SHAPE_REPRESENTATION");
        assert_eq!(representation.len(), 1);
        assert_eq!(representation[0].1.references.len(), 4, "placement + 2 solids + context");
    }

    /// A fully enclosed cavity is a void in one solid, not a second solid
    /// sitting inside the first — the difference between 7000 mm3 of material
    /// and 8000.
    #[test]
    fn an_enclosed_cavity_becomes_a_void_rather_than_a_second_solid() {
        let text = step("difference(){ cube(20,center=true); cube(10,center=true); }");
        let records = records(&text);
        assert_eq!(of_kind(&records, "CLOSED_SHELL").len(), 2);
        assert_eq!(of_kind(&records, "MANIFOLD_SOLID_BREP").len(), 0);
        let voids = of_kind(&records, "BREP_WITH_VOIDS");
        assert_eq!(voids.len(), 1);
        // Outer shell, then one oriented void shell.
        let solid = voids[0].1;
        assert_eq!(solid.references.len(), 2);
        assert_eq!(records[&solid.references[0]].kind, "CLOSED_SHELL");
        let oriented = &records[&solid.references[1]];
        assert_eq!(oriented.kind, "ORIENTED_CLOSED_SHELL");
        assert_eq!(records[&oriented.references[0]].kind, "CLOSED_SHELL");
        // The cavity's own shell is the smaller one: six faces each, but the
        // void's faces point inward, which is what `.T.` preserves.
        assert!(oriented.body.ends_with(".T.)"), "{}", oriented.body);
    }

    #[test]
    fn reals_are_always_part21_reals() {
        assert_eq!(real(0.0), "0.");
        assert_eq!(real(-0.0), "0.");
        assert_eq!(real(10.0), "10.");
        assert_eq!(real(-2.5), "-2.5");
        assert_eq!(real(f64::NAN), "0.");
        assert_eq!(real(f64::INFINITY), "0.");
        // Rust's `Display` never reaches for exponent notation, which is what
        // lets this skip the `1.E-7` spelling entirely.
        assert!(!real(1.0e-7).contains('E'));
        assert!(!real(1.0e-7).contains('e'));
        assert_eq!(real(1.0e-7), "0.0000001");
    }

    #[test]
    fn strings_are_part21_escaped() {
        assert_eq!(escape("plain"), "plain");
        assert_eq!(escape("it's"), "it''s");
        assert_eq!(escape("a\\b"), "a\\\\b");
        assert_eq!(escape("tab\there"), "tabhere");
        assert_eq!(escape("café"), "caf\\X2\\00E9\\X0\\");
        // Consecutive non-ASCII characters share one escape sequence.
        assert_eq!(escape("úé"), "\\X2\\00FA00E9\\X0\\");
        let text = String::from_utf8(rendered(&compile("cube(1);"), "it's a 'cube'")).unwrap();
        assert!(text.contains("FILE_NAME('it''s a ''cube'''"));
    }

    #[test]
    fn a_model_with_no_geometry_still_writes_a_readable_file() {
        let empty = engine::Mesh::default();
        let text = String::from_utf8(rendered(&empty, "nothing")).unwrap();
        assert!(text.starts_with("ISO-10303-21;\n"));
        assert!(text.contains("SHAPE_REPRESENTATION('',("));
        assert!(!text.contains("CLOSED_SHELL"), "an empty shell is illegal");
        let records = records(&text);
        for (id, record) in &records {
            for reference in &record.references {
                assert!(records.contains_key(reference), "#{id} -> #{reference}");
            }
        }
    }

    /// Non-finite coordinates cannot be spelled in Part 21 at all, so they must
    /// never reach the file.
    #[test]
    fn non_finite_facets_are_dropped_rather_than_written() {
        let broken = Vec3::new(f64::NAN, 0.0, 0.0);
        let mesh = engine::Mesh {
            triangles: vec![engine::Triangle {
                vertices: [
                    engine::Vec3 {
                        x: broken.x,
                        y: broken.y,
                        z: broken.z,
                    },
                    engine::Vec3 {
                        x: 0.0,
                        y: 1.0,
                        z: 0.0,
                    },
                    engine::Vec3 {
                        x: 0.0,
                        y: 0.0,
                        z: 1.0,
                    },
                ],
                normal: engine::Vec3 {
                    x: 1.0,
                    y: 0.0,
                    z: 0.0,
                },
            }],
        };
        let text = String::from_utf8(rendered(&mesh, "broken")).unwrap();
        assert!(!text.contains("NaN"));
        assert!(!text.contains("inf"));
        assert!(!text.contains("CLOSED_SHELL"));
    }

    /// Region growing must not depend on which triangle it happens to start
    /// from, and the export must be byte-for-byte reproducible so it can be
    /// cached and diffed.
    #[test]
    fn the_export_is_deterministic() {
        let mesh = compile("difference() { cube(20, center=true); sphere(r=12, $fn=20); }");
        let first = rendered(&mesh, "unit test");
        let second = rendered(&mesh, "unit test");
        assert_eq!(first, second);
    }
}
