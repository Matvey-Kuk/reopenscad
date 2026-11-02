//! Reading uploaded meshes (STL) and cleaning raw scans. An open surface stays
//! open: cleaning never invents geometry.
//!
//! Uploads are untrusted: the parser never trusts a header count before
//! checking it against the file size and the triangle cap, and never panics
//! on malformed bytes. Cleaning targets what 3D scans typically get wrong:
//! duplicated and near-duplicated vertices, slivers, floating debris,
//! non-manifold seams, inconsistent winding, and far more triangles than
//! booleans can afford.

use crate::vec3::{add, cross, dot, norm as length, scale, sub};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::time::Instant;

use serde::Serialize;

use crate::assets::TriMesh;

/// Largest upload accepted, in bytes: a binary STL of about 2.68M
/// triangles, the size phone scans come in. Uploads are decimated to the
/// triangle budget, so stored size does not grow with it. Parsing and
/// cleaning 2.5M triangles peaks around 700 MB resident and ~7 s (release);
/// the server lets one such upload clean at a time.
pub const MAX_UPLOAD_BYTES: usize = 128 * 1024 * 1024;
/// Most triangles a parsed file may contain before cleaning (above what
/// [`MAX_UPLOAD_BYTES`] of binary STL can hold).
pub const MAX_INPUT_TRIANGLES: usize = 3_000_000;
/// Default triangle budget after cleaning: enough to keep a phone or
/// structured-light scan's detail (rims, fillets, thin walls). Exact
/// booleans and overlap volumes cap at 60k triangles
/// ([`crate::analysis::MAX_BOOLEAN_TRIANGLES`]): a scan that takes part in a
/// boolean has to be uploaded with a budget under that; a background scan
/// (`%`) never takes part.
pub const DEFAULT_TRIANGLE_BUDGET: usize = 500_000;
/// Hard ceiling for a caller-chosen budget.
pub const MAX_TRIANGLE_BUDGET: usize = 1_000_000;

/// An ASCII facet is normally a triangle; some writers emit polygons, which
/// are fanned. Anything longer is treated as corrupt rather than buffered.
const MAX_FACET_VERTICES: usize = 1024;
/// Near-duplicate weld distance as a fraction of the bounding-box diagonal.
const WELD_TOLERANCE_FRACTION: f64 = 1e-6;
/// A component is debris when it has both under this fraction of the largest
/// component's area and under this fraction of its triangle count. Requiring
/// both keeps a low-poly but large part next to a dense one.
const DEBRIS_FRACTION: f64 = 0.01;
/// Ray-parity containment tests the cavity orientation may run; shells left
/// undetermined keep their input winding.
const MAX_NESTING_TESTS: usize = 20_000;
/// Bounding-box pairs the cavity orientation may compare before giving up.
const MAX_NESTING_PAIRS: usize = 20_000_000;
/// A collapse whose endpoint touches more faces than this is refused: its
/// one-ring checks would cost O(valence^2) per attempt (hostile fans).
const MAX_COLLAPSE_VALENCE: usize = 64;

type Vec3 = [f64; 3];

#[derive(Clone, Copy, Debug)]
pub struct CleanOptions {
    pub triangle_budget: usize,
}

impl Default for CleanOptions {
    fn default() -> Self {
        Self { triangle_budget: DEFAULT_TRIANGLE_BUDGET }
    }
}

/// Work accounting for one [`clean`]: fixed step budgets for the
/// super-linear stages (always on) plus an optional wall-clock deadline.
struct Work {
    deadline: Option<Instant>,
    /// Whether the thread's request interrupts are polled.
    polls: bool,
    expired: bool,
    /// Why the request stopped, when it was cancelled or timed out.
    interrupted: Option<&'static str>,
}

impl Work {
    fn new(deadline: Option<Instant>) -> Self {
        Self { deadline, polls: true, expired: false, interrupted: None }
    }

    /// Whether the deadline has passed or the request was cancelled or ran
    /// out of its own time (the thread's `csg::bsp` interrupt); sticky.
    fn expired(&mut self) -> bool {
        if !self.expired {
            if let Some(message) = crate::csg::bsp::interrupt_message().filter(|_| self.polls) {
                self.expired = true;
                self.interrupted = Some(message);
            } else if self.deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                self.expired = true;
            }
        }
        self.expired
    }

    fn check(&mut self) -> Result<(), String> {
        if self.expired() {
            Err(match self.interrupted {
                Some(message) => message.to_string(),
                None => "cleaning the mesh took longer than its time budget; upload a smaller or simpler mesh".into(),
            })
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CleanReport {
    pub input_triangles: usize,
    pub output_triangles: usize,
    pub input_vertices: usize,
    pub output_vertices: usize,
    /// Near-duplicate vertices merged into a neighbour within the weld tolerance.
    pub vertices_welded: usize,
    pub degenerate_removed: usize,
    pub duplicate_removed: usize,
    /// Connected components found before debris removal.
    pub components: usize,
    pub small_components_removed: usize,
    /// Triangles dropped to resolve edges shared by more than two faces.
    pub non_manifold_faces_removed: usize,
    pub boundary_edges_remaining: usize,
    /// Edges still shared by more than two faces in the output.
    pub non_manifold_edges: usize,
    pub orientation_fixed: bool,
    pub watertight: bool,
    pub decimated: bool,
    pub bounds_min: [f64; 3],
    pub bounds_max: [f64; 3],
    pub volume: f64,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// STL parsing
// ---------------------------------------------------------------------------

/// Parses binary or ASCII STL. Rejects inputs over [`MAX_UPLOAD_BYTES`] or
/// [`MAX_INPUT_TRIANGLES`], truncated binaries, non-finite coordinates, and
/// returns a welded indexed mesh.
///
/// Format detection follows the size rule rather than the "solid" prefix,
/// because many binary exporters write "solid" into the 80-byte header: a
/// file whose length is exactly `84 + 50 * count` is binary; otherwise a
/// "solid" prefix means ASCII; anything else is a corrupt binary.
pub fn parse_stl(bytes: &[u8]) -> Result<TriMesh, String> {
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Err(format!(
            "STL file is {:.1} MB; the limit is {} MB",
            bytes.len() as f64 / (1024.0 * 1024.0),
            MAX_UPLOAD_BYTES / (1024 * 1024)
        ));
    }
    if bytes.is_empty() {
        return Err("STL file is empty".into());
    }
    let looks_ascii = starts_with_solid(bytes);
    if bytes.len() >= 84 {
        let declared = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as u64;
        let expected = 84 + 50 * declared;
        if expected == bytes.len() as u64 {
            return parse_binary(bytes, declared as usize);
        }
        if !looks_ascii {
            let body = bytes.len() - 84;
            // Some exporters leave the count at zero; the size still tells.
            if declared == 0 && body > 0 && body % 50 == 0 {
                return parse_binary(bytes, body / 50);
            }
            if declared as usize > MAX_INPUT_TRIANGLES {
                return Err(format!(
                    "binary STL declares {declared} triangles; the limit is {MAX_INPUT_TRIANGLES}"
                ));
            }
            return Err(format!(
                "binary STL declares {declared} triangles ({expected} bytes) but the file has {} bytes; it is truncated or corrupt",
                bytes.len()
            ));
        }
    } else if !looks_ascii {
        return Err("file is too short to be an STL".into());
    }
    parse_ascii(bytes)
}

fn starts_with_solid(bytes: &[u8]) -> bool {
    let start = bytes.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(bytes.len());
    bytes.len() - start >= 5 && bytes[start..start + 5].eq_ignore_ascii_case(b"solid")
}

fn parse_binary(bytes: &[u8], count: usize) -> Result<TriMesh, String> {
    if count > MAX_INPUT_TRIANGLES {
        return Err(format!("binary STL has {count} triangles; the limit is {MAX_INPUT_TRIANGLES}"));
    }
    if count == 0 {
        return Err("STL contains no triangles".into());
    }
    // `count` is bounded by both the cap and the verified file size, so
    // these reservations cannot be inflated by a lying header.
    let mut welder = VertexWelder::with_capacity(count / 2 + 3);
    let mut triangles = Vec::with_capacity(count);
    for record in bytes[84..84 + 50 * count].chunks_exact(50) {
        let mut corner = [0u32; 3];
        for (slot, index) in corner.iter_mut().enumerate() {
            let base = 12 + slot * 12;
            let read = |offset: usize| {
                f32::from_le_bytes([
                    record[base + offset],
                    record[base + offset + 1],
                    record[base + offset + 2],
                    record[base + offset + 3],
                ]) as f64
            };
            *index = welder.insert([read(0), read(4), read(8)])?;
        }
        triangles.push(corner);
    }
    Ok(TriMesh { points: welder.points, triangles })
}

fn parse_ascii(bytes: &[u8]) -> Result<TriMesh, String> {
    let mut tokens = bytes.split(|b| b.is_ascii_whitespace()).filter(|token| !token.is_empty());
    let mut welder = VertexWelder::with_capacity(1024);
    let mut triangles: Vec<[u32; 3]> = Vec::new();
    let mut polygon: Vec<u32> = Vec::new();
    while let Some(token) = tokens.next() {
        if token.eq_ignore_ascii_case(b"vertex") {
            let mut coordinates = [0.0; 3];
            for value in coordinates.iter_mut() {
                let text = tokens.next().ok_or("ASCII STL ends inside a vertex")?;
                *value = parse_number(text)?;
            }
            if polygon.len() >= MAX_FACET_VERTICES {
                return Err(format!("ASCII STL facet has more than {MAX_FACET_VERTICES} vertices"));
            }
            polygon.push(welder.insert(coordinates)?);
        } else if token.eq_ignore_ascii_case(b"endfacet") {
            if polygon.len() < 3 {
                return Err("ASCII STL facet has fewer than 3 vertices".into());
            }
            for i in 1..polygon.len() - 1 {
                if triangles.len() >= MAX_INPUT_TRIANGLES {
                    return Err(format!("STL has more than {MAX_INPUT_TRIANGLES} triangles"));
                }
                triangles.push([polygon[0], polygon[i], polygon[i + 1]]);
            }
            polygon.clear();
        } else if token.eq_ignore_ascii_case(b"facet") && !polygon.is_empty() {
            return Err("ASCII STL facet is missing its endfacet".into());
        }
    }
    if !polygon.is_empty() {
        return Err("ASCII STL is truncated inside a facet".into());
    }
    if triangles.is_empty() {
        return Err("STL contains no triangles".into());
    }
    Ok(TriMesh { points: welder.points, triangles })
}

fn parse_number(token: &[u8]) -> Result<f64, String> {
    let text = std::str::from_utf8(token).map_err(|_| "ASCII STL has a non-text coordinate".to_string())?;
    text.parse::<f64>().map_err(|_| {
        let shown: String = text.chars().take(24).collect();
        format!("ASCII STL has an invalid coordinate \"{shown}\"")
    })
}

/// Merges vertices whose single-precision coordinates are bit-identical:
/// STL stores every triangle's corners separately, so this recovers the
/// shared topology the format throws away.
struct VertexWelder {
    lookup: HashMap<[u32; 3], u32>,
    points: Vec<Vec3>,
}

impl VertexWelder {
    fn with_capacity(capacity: usize) -> Self {
        Self { lookup: HashMap::with_capacity(capacity), points: Vec::with_capacity(capacity) }
    }

    fn insert(&mut self, point: Vec3) -> Result<u32, String> {
        let single = point.map(|value| value as f32);
        if single.iter().any(|value| !value.is_finite()) {
            return Err("STL has a non-finite or out-of-range coordinate".into());
        }
        // -0.0 and 0.0 are the same position but different bits.
        let key = single.map(|value| if value == 0.0 { 0 } else { value.to_bits() });
        let next = self.points.len() as u32;
        let index = *self.lookup.entry(key).or_insert(next);
        if index == next {
            self.points.push(single.map(f64::from));
        }
        Ok(index)
    }
}

// ---------------------------------------------------------------------------
// Cleaning
// ---------------------------------------------------------------------------

/// Turns a raw scan into a closed, consistently wound solid within the
/// triangle budget. The super-linear stages run on fixed work budgets and
/// fall back to cheaper strategies (recorded as warnings) when those run
/// out, so hostile input cannot stall it; see [`clean_until`] for a
/// wall-clock deadline on top.
// The server repairs uploads with `clean_until`; this is the library/test entry.
#[allow(dead_code)]
pub fn clean(mesh: TriMesh, options: CleanOptions) -> (TriMesh, CleanReport) {
    // The pure entry: no deadline and no interrupt polls, so it cannot fail.
    let mut work = Work { polls: false, ..Work::new(None) };
    match clean_with(mesh, options, &mut work) {
        Ok(result) => result,
        // Only a deadline produces an error, and there is none.
        Err(error) => unreachable!("clean without a deadline failed: {error}"),
    }
}

/// [`clean`] with a wall-clock deadline. Past it, decimation stops early
/// (recorded as a warning); the call fails only when the deadline passes before the mandatory
/// stages finish, or when stopping decimation leaves the mesh over its
/// triangle budget.
pub fn clean_until(
    mesh: TriMesh,
    options: CleanOptions,
    deadline: Option<Instant>,
) -> Result<(TriMesh, CleanReport), String> {
    clean_with(mesh, options, &mut Work::new(deadline))
}

fn clean_with(mesh: TriMesh, options: CleanOptions, work: &mut Work) -> Result<(TriMesh, CleanReport), String> {
    let budget = options.triangle_budget.clamp(4, MAX_TRIANGLE_BUDGET);
    let mut report = CleanReport {
        input_triangles: mesh.triangles.len(),
        input_vertices: mesh.points.len(),
        ..CleanReport::default()
    };
    let TriMesh { mut points, triangles } = mesh;

    // A mesh handed in from elsewhere may carry bad indices or coordinates;
    // dropping those faces up front lets every later stage index freely.
    let point_valid: Vec<bool> = points.iter().map(|p| p.iter().all(|c| c.is_finite())).collect();
    let mut triangles: Vec<[u32; 3]> = triangles
        .into_iter()
        .filter(|t| t.iter().all(|&i| point_valid.get(i as usize).copied().unwrap_or(false)))
        .collect();
    let invalid = report.input_triangles - triangles.len();
    if invalid > 0 {
        report.warnings.push(format!("Dropped {invalid} triangles with invalid vertices"));
    }

    let diagonal = bounds_of(&points, &triangles).map(|(lo, hi)| length(sub(hi, lo))).unwrap_or(0.0);
    if triangles.is_empty() || !(diagonal > 0.0) || !diagonal.is_finite() {
        report.degenerate_removed = triangles.len();
        return Ok(finish(Vec::new(), Vec::new(), report));
    }

    work.check()?;
    report.vertices_welded = weld_near_duplicates(&points, &mut triangles, diagonal * WELD_TOLERANCE_FRACTION);
    let (degenerate, duplicate) = remove_degenerate_and_duplicates(&points, &mut triangles, diagonal);
    report.degenerate_removed = degenerate;
    report.duplicate_removed = duplicate;

    work.check()?;
    let debris = remove_debris(&points, &mut triangles);
    report.components = debris.components;
    report.small_components_removed = debris.removed_components;
    if debris.removed_components > 0 {
        report.warnings.push(format!(
            "Removed {} floating fragment{} ({} triangles)",
            debris.removed_components,
            plural(debris.removed_components),
            debris.removed_triangles
        ));
    }

    report.non_manifold_faces_removed = fix_non_manifold_edges(&points, &mut triangles);
    if report.non_manifold_faces_removed > 0 {
        report.warnings.push(format!(
            "Dropped {} faces on edges shared by more than two triangles",
            report.non_manifold_faces_removed
        ));
    }

    let flipped = orient_consistently(&mut triangles);

    work.check()?;
    let nesting = orient_outward(&points, &mut triangles);
    let outward_flipped = nesting.flipped;
    if nesting.undetermined > 0 {
        report.warnings.push(format!(
            "Could not tell whether {} shell{} nested inside others; kept {} winding as uploaded",
            nesting.undetermined,
            plural(nesting.undetermined),
            if nesting.undetermined == 1 { "its" } else { "their" }
        ));
    }
    let total_flipped = flipped + outward_flipped;
    report.orientation_fixed = total_flipped > 0;
    if total_flipped > 0 {
        report.warnings.push(format!("Reoriented {total_flipped} faces"));
    }

    if triangles.len() > budget {
        work.check()?;
        let before = triangles.len();
        let completed = decimate(&mut points, &mut triangles, budget, work);
        if let Some(message) = work.interrupted {
            return Err(message.to_string());
        }
        report.decimated = triangles.len() < before;
        report.warnings.push(format!("Decimated from {before} to {} triangles", triangles.len()));
        if triangles.len() > budget {
            if !completed {
                return Err(format!(
                    "simplifying the mesh to {budget} triangles took longer than its time budget \
                     (stopped at {}); upload a smaller mesh or lower the triangle budget",
                    triangles.len()
                ));
            }
            report.warnings.push(format!(
                "Could not reach the {budget}-triangle budget without breaking the surface"
            ));
        }
    }

    Ok(finish(points, triangles, report))
}

/// Compacts the vertex list and fills in the topology, bounds and volume
/// fields from the final mesh.
fn finish(points: Vec<Vec3>, triangles: Vec<[u32; 3]>, mut report: CleanReport) -> (TriMesh, CleanReport) {
    let mut remap = vec![u32::MAX; points.len()];
    let mut compact_points = Vec::new();
    let mut compact_triangles = Vec::with_capacity(triangles.len());
    for triangle in &triangles {
        compact_triangles.push(triangle.map(|index| {
            let slot = &mut remap[index as usize];
            if *slot == u32::MAX {
                *slot = compact_points.len() as u32;
                compact_points.push(points[index as usize]);
            }
            *slot
        }));
    }
    let mesh = TriMesh { points: compact_points, triangles: compact_triangles };

    let topology = Topology::of(&mesh.triangles);
    report.output_triangles = mesh.triangles.len();
    report.output_vertices = mesh.points.len();
    report.boundary_edges_remaining = topology.boundary;
    report.non_manifold_edges = topology.non_manifold;
    report.watertight = !mesh.triangles.is_empty() && topology.is_closed();
    if let Some((lo, hi)) = bounds_of(&mesh.points, &mesh.triangles) {
        report.bounds_min = lo;
        report.bounds_max = hi;
        report.volume = signed_volume(&mesh.points, mesh.triangles.iter(), scale(add(lo, hi), 0.5));
    }
    if mesh.triangles.is_empty() {
        report.warnings.push("The mesh has no usable triangles".into());
    } else if !report.watertight && topology.non_manifold == 0 && topology.inconsistent == 0 {
        report.warnings.push(format!(
            "Kept as an open surface ({} boundary edges): booleans and volume need a closed solid",
            topology.boundary
        ));
    } else if !report.watertight {
        let mut problems = Vec::new();
        if topology.boundary > 0 {
            problems.push(format!("{} open boundary edges", topology.boundary));
        }
        if topology.non_manifold > 0 {
            problems.push(format!("{} non-manifold edges", topology.non_manifold));
        }
        if topology.inconsistent > 0 {
            problems.push(format!("{} inconsistently wound edges", topology.inconsistent));
        }
        report.warnings.push(format!("The mesh is not watertight: {}", problems.join(", ")));
    }
    (mesh, report)
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Merges vertices closer than `tolerance` using a hash grid; returns how
/// many were merged. Only referenced vertices take part, so stray unused
/// points cannot pull real ones around.
fn weld_near_duplicates(points: &[Vec3], triangles: &mut [[u32; 3]], tolerance: f64) -> usize {
    if !(tolerance > 0.0) {
        return 0;
    }
    let mut referenced = vec![false; points.len()];
    for triangle in triangles.iter() {
        for &index in triangle {
            referenced[index as usize] = true;
        }
    }
    // Cells four tolerances wide mean most points only need their own cell;
    // neighbours are visited only when the point sits near a cell wall.
    let cell = tolerance * 4.0;
    let tolerance_squared = tolerance * tolerance;
    let mut heads: HashMap<[i64; 3], u32> = HashMap::new();
    let mut next_in_cell = vec![u32::MAX; points.len()];
    let mut remap: Vec<u32> = (0..points.len() as u32).collect();
    let mut welded = 0;
    for (index, point) in points.iter().enumerate() {
        if !referenced[index] {
            continue;
        }
        let key = point.map(|value| (value / cell).floor() as i64);
        let mut range = [(0i64, 0i64); 3];
        for axis in 0..3 {
            let low_wall = key[axis] as f64 * cell;
            if point[axis] - low_wall < tolerance {
                range[axis].0 = -1;
            }
            if low_wall + cell - point[axis] < tolerance {
                range[axis].1 = 1;
            }
        }
        let mut found = None;
        'search: for dx in range[0].0..=range[0].1 {
            for dy in range[1].0..=range[1].1 {
                for dz in range[2].0..=range[2].1 {
                    let neighbour = [key[0].saturating_add(dx), key[1].saturating_add(dy), key[2].saturating_add(dz)];
                    let mut cursor = heads.get(&neighbour).copied().unwrap_or(u32::MAX);
                    while cursor != u32::MAX {
                        if distance_squared(points[cursor as usize], *point) <= tolerance_squared {
                            found = Some(cursor);
                            break 'search;
                        }
                        cursor = next_in_cell[cursor as usize];
                    }
                }
            }
        }
        match found {
            Some(representative) => {
                remap[index] = representative;
                welded += 1;
            }
            None => {
                next_in_cell[index] = heads.insert(key, index as u32).unwrap_or(u32::MAX);
            }
        }
    }
    if welded > 0 {
        for triangle in triangles.iter_mut() {
            *triangle = triangle.map(|index| remap[index as usize]);
        }
    }
    welded
}

/// Removes triangles with repeated corners or (near) zero area, then exact
/// duplicates regardless of winding. Returns (degenerate, duplicate) counts.
fn remove_degenerate_and_duplicates(points: &[Vec3], triangles: &mut Vec<[u32; 3]>, diagonal: f64) -> (usize, usize) {
    let area_floor = 1e-12 * diagonal * diagonal;
    let before = triangles.len();
    triangles.retain(|&[a, b, c]| {
        a != b
            && b != c
            && a != c
            && length(face_normal(points[a as usize], points[b as usize], points[c as usize])) > area_floor
    });
    let degenerate = before - triangles.len();

    let mut keyed: Vec<([u32; 3], u32)> = triangles
        .iter()
        .enumerate()
        .map(|(index, triangle)| {
            let mut key = *triangle;
            key.sort_unstable();
            (key, index as u32)
        })
        .collect();
    keyed.sort_unstable();
    let mut keep = vec![true; triangles.len()];
    let mut duplicate = 0;
    for pair in keyed.windows(2) {
        if pair[0].0 == pair[1].0 {
            keep[pair[1].1 as usize] = false;
            duplicate += 1;
        }
    }
    if duplicate > 0 {
        let mut index = 0;
        triangles.retain(|_| {
            index += 1;
            keep[index - 1]
        });
    }
    (degenerate, duplicate)
}

struct DebrisStats {
    components: usize,
    removed_components: usize,
    removed_triangles: usize,
}

/// Drops floating fragments: scans pick up dust, turntable bits and stray
/// surface patches that are tiny next to the scanned object.
fn remove_debris(points: &[Vec3], triangles: &mut Vec<[u32; 3]>) -> DebrisStats {
    let labels = component_labels(points.len(), triangles);
    let count = labels.iter().copied().max().map_or(0, |max| max as usize + 1);
    let mut triangle_counts = vec![0usize; count];
    let mut areas = vec![0.0f64; count];
    for (triangle, &label) in triangles.iter().zip(&labels) {
        triangle_counts[label as usize] += 1;
        areas[label as usize] += triangle_area(points, triangle);
    }
    let largest_area = areas.iter().copied().fold(0.0, f64::max);
    let largest_count = triangle_counts.iter().copied().max().unwrap_or(0);
    let largest_label = (0..count).max_by(|&a, &b| areas[a].total_cmp(&areas[b]));
    let removed: Vec<bool> = (0..count)
        .map(|label| {
            Some(label) != largest_label
                && (triangle_counts[label] < 4
                    || (areas[label] < DEBRIS_FRACTION * largest_area
                        && (triangle_counts[label] as f64) < DEBRIS_FRACTION * largest_count as f64))
        })
        .collect();
    let removed_components = removed.iter().filter(|&&r| r).count();
    let mut removed_triangles = 0;
    if removed_components > 0 {
        let mut index = 0;
        triangles.retain(|_| {
            index += 1;
            let drop = removed[labels[index - 1] as usize];
            removed_triangles += drop as usize;
            !drop
        });
    }
    DebrisStats { components: count, removed_components, removed_triangles }
}

/// Labels each triangle with a dense component id (shared-vertex connectivity).
fn component_labels(point_count: usize, triangles: &[[u32; 3]]) -> Vec<u32> {
    let mut parent: Vec<u32> = (0..point_count as u32).collect();
    fn find(parent: &mut [u32], mut x: u32) -> u32 {
        while parent[x as usize] != x {
            let grand = parent[parent[x as usize] as usize];
            parent[x as usize] = grand;
            x = grand;
        }
        x
    }
    for &[a, b, c] in triangles {
        for other in [b, c] {
            let (ra, rb) = (find(&mut parent, a), find(&mut parent, other));
            if ra != rb {
                parent[ra.max(rb) as usize] = ra.min(rb);
            }
        }
    }
    let mut dense = vec![u32::MAX; point_count];
    let mut next = 0;
    triangles
        .iter()
        .map(|triangle| {
            let root = find(&mut parent, triangle[0]) as usize;
            if dense[root] == u32::MAX {
                dense[root] = next;
                next += 1;
            }
            dense[root]
        })
        .collect()
}

/// Undirected edge incidence: every triangle corner `(key, triangle * 3 +
/// slot)` sorted by key, so the faces sharing an edge form a run. Sorting a
/// flat vector is far cheaper than a hash map at scan sizes.
fn edge_incidence(triangles: &[[u32; 3]]) -> Vec<(u64, u32)> {
    let mut incidence = Vec::with_capacity(triangles.len() * 3);
    for (index, triangle) in triangles.iter().enumerate() {
        for slot in 0..3 {
            let (a, b) = (triangle[slot], triangle[(slot + 1) % 3]);
            incidence.push((edge_key(a, b), index as u32 * 3 + slot as u32));
        }
    }
    incidence.sort_unstable();
    incidence
}

/// How many edges have a single face: the open boundary of a surface. Uploads
/// keep those as they are; nothing closes them.
pub fn open_edge_count(triangles: &[[u32; 3]]) -> usize {
    edge_runs(&edge_incidence(triangles)).filter(|run| run.len() == 1).count()
}

fn edge_key(a: u32, b: u32) -> u64 {
    ((a.min(b) as u64) << 32) | a.max(b) as u64
}

/// Iterates the runs of `incidence` that share one edge.
fn edge_runs(incidence: &[(u64, u32)]) -> impl Iterator<Item = &[(u64, u32)]> {
    incidence.chunk_by(|a, b| a.0 == b.0)
}

/// Whether the corner's directed edge runs from the lower to the higher index.
fn corner_is_forward(triangles: &[[u32; 3]], corner: u32) -> bool {
    let triangle = triangles[(corner / 3) as usize];
    let slot = (corner % 3) as usize;
    triangle[slot] < triangle[(slot + 1) % 3]
}

/// Edge-level health of a mesh.
struct Topology {
    boundary: usize,
    non_manifold: usize,
    inconsistent: usize,
}

impl Topology {
    fn of(triangles: &[[u32; 3]]) -> Self {
        let incidence = edge_incidence(triangles);
        let mut topology = Topology { boundary: 0, non_manifold: 0, inconsistent: 0 };
        for run in edge_runs(&incidence) {
            match run.len() {
                1 => topology.boundary += 1,
                2 => {
                    if corner_is_forward(triangles, run[0].1) == corner_is_forward(triangles, run[1].1) {
                        topology.inconsistent += 1;
                    }
                }
                _ => topology.non_manifold += 1,
            }
        }
        topology
    }

    fn is_closed(&self) -> bool {
        self.boundary == 0 && self.non_manifold == 0 && self.inconsistent == 0
    }
}

/// Where an edge has more than two faces, keeps the largest face plus the
/// largest one wound against it (the pair most likely to be the real
/// surface) and drops the rest.
/// Returns the number of faces dropped.
fn fix_non_manifold_edges(points: &[Vec3], triangles: &mut Vec<[u32; 3]>) -> usize {
    let incidence = edge_incidence(triangles);
    let mut alive = vec![true; triangles.len()];
    let mut dropped = 0;
    let mut faces: Vec<(f64, u32)> = Vec::new();
    for run in edge_runs(&incidence) {
        if run.len() <= 2 {
            continue;
        }
        faces.clear();
        faces.extend(
            run.iter()
                .filter(|(_, corner)| alive[(corner / 3) as usize])
                .map(|&(_, corner)| (triangle_area(points, &triangles[(corner / 3) as usize]), corner)),
        );
        if faces.len() <= 2 {
            continue;
        }
        faces.sort_by(|a, b| b.0.total_cmp(&a.0));
        let first = faces[0].1;
        let first_forward = corner_is_forward(triangles, first);
        let partner = faces[1..]
            .iter()
            .find(|(_, corner)| corner_is_forward(triangles, *corner) != first_forward)
            .unwrap_or(&faces[1])
            .1;
        for &(_, corner) in faces.iter() {
            if corner != first && corner != partner {
                alive[(corner / 3) as usize] = false;
                dropped += 1;
            }
        }
    }
    if dropped > 0 {
        let mut index = 0;
        triangles.retain(|_| {
            index += 1;
            alive[index - 1]
        });
    }
    dropped
}

/// Makes winding consistent across every manifold edge by flood fill. Each
/// component keeps its majority winding so only the stray faces count as
/// fixed; the global sign is settled by [`orient_outward`]. Returns the
/// number of faces flipped.
fn orient_consistently(triangles: &mut [[u32; 3]]) -> usize {
    let incidence = edge_incidence(triangles);
    let mut neighbours = vec![[u32::MAX; 3]; triangles.len()];
    let mut same_direction = vec![[false; 3]; triangles.len()];
    for run in edge_runs(&incidence) {
        if let [(_, first), (_, second)] = *run {
            let same = corner_is_forward(triangles, first) == corner_is_forward(triangles, second);
            for (from, to) in [(first, second), (second, first)] {
                neighbours[(from / 3) as usize][(from % 3) as usize] = to / 3;
                same_direction[(from / 3) as usize][(from % 3) as usize] = same;
            }
        }
    }
    let mut visited = vec![false; triangles.len()];
    let mut flip = vec![false; triangles.len()];
    let mut component: Vec<u32> = Vec::new();
    let mut flipped = 0;
    for seed in 0..triangles.len() {
        if visited[seed] {
            continue;
        }
        visited[seed] = true;
        component.clear();
        component.push(seed as u32);
        let mut cursor = 0;
        while cursor < component.len() {
            let face = component[cursor] as usize;
            cursor += 1;
            for slot in 0..3 {
                let neighbour = neighbours[face][slot];
                if neighbour != u32::MAX && !visited[neighbour as usize] {
                    visited[neighbour as usize] = true;
                    flip[neighbour as usize] = flip[face] ^ same_direction[face][slot];
                    component.push(neighbour);
                }
            }
        }
        let flips = component.iter().filter(|&&face| flip[face as usize]).count();
        let invert = flips * 2 > component.len();
        for &face in &component {
            if flip[face as usize] != invert {
                triangles[face as usize].swap(1, 2);
                flipped += 1;
            }
        }
    }
    flipped
}

struct NestingStats {
    flipped: usize,
    /// Shells whose nesting could not be settled within the work budget;
    /// they keep their input winding.
    undetermined: usize,
}

/// Gives every closed shell positive volume, except shells nested an odd
/// number of times inside larger ones (cavities), which face inward. Open
/// shells have no inside, so they keep their (consistent) input winding.
///
/// Nesting is found per shell with a bounding-box prefilter (a container's
/// box must enclose the shell's box and it must have more volume) and then
/// a BVH ray-parity test against each remaining candidate, so it scales to
/// many shells. When the pair or test budget runs out, the shells not yet
/// settled keep their input winding rather than being forced outward: a
/// reversed cavity is worse than an unoriented one.
fn orient_outward(points: &[Vec3], triangles: &mut [[u32; 3]]) -> NestingStats {
    let mut stats = NestingStats { flipped: 0, undetermined: 0 };
    if triangles.is_empty() {
        return stats;
    }
    let labels = component_labels(points.len(), triangles);
    let count = labels.iter().copied().max().map_or(0, |max| max as usize + 1);
    let mut open = vec![false; count];
    for run in edge_runs(&edge_incidence(triangles)) {
        if let [(_, corner)] = *run {
            open[labels[(corner / 3) as usize] as usize] = true;
        }
    }
    let mut members: Vec<Vec<u32>> = vec![Vec::new(); count];
    for (index, &label) in labels.iter().enumerate() {
        members[label as usize].push(index as u32);
    }
    let center = bounds_of(points, triangles).map(|(lo, hi)| scale(add(lo, hi), 0.5)).unwrap_or([0.0; 3]);
    let volumes: Vec<f64> = members
        .iter()
        .map(|faces| signed_volume(points, faces.iter().map(|&f| &triangles[f as usize]), center))
        .collect();
    let boxes: Vec<(Vec3, Vec3)> = members
        .iter()
        .map(|faces| {
            let mut lo = [f64::INFINITY; 3];
            let mut hi = [f64::NEG_INFINITY; 3];
            for &face in faces {
                for &index in &triangles[face as usize] {
                    let p = points[index as usize];
                    for axis in 0..3 {
                        lo[axis] = lo[axis].min(p[axis]);
                        hi[axis] = hi[axis].max(p[axis]);
                    }
                }
            }
            (lo, hi)
        })
        .collect();
    let encloses = |outer: usize, inner: usize| {
        let ((olo, ohi), (ilo, ihi)) = (boxes[outer], boxes[inner]);
        (0..3).all(|axis| olo[axis] <= ilo[axis] && ihi[axis] <= ohi[axis])
    };
    let mut order: Vec<usize> = (0..count).collect();
    order.sort_by(|&a, &b| volumes[b].abs().total_cmp(&volumes[a].abs()));
    let mut indexes: Vec<Option<crate::meshanalysis::MeshIndex>> = (0..count).map(|_| None).collect();
    let (mut pairs, mut tests) = (0usize, 0usize);
    let mut depths: Vec<Option<usize>> = vec![Some(0); count];
    for (rank, &component) in order.iter().enumerate() {
        if volumes[component] == 0.0 {
            continue;
        }
        // Probe from a face centroid: a vertex may coincide with a vertex
        // of a touching shell.
        let [a, b, c] = triangles[members[component][0] as usize].map(|index| points[index as usize]);
        let probe = scale(add(add(a, b), c), 1.0 / 3.0);
        let mut depth = 0;
        for &outer in &order[..rank] {
            pairs += 1;
            if pairs > MAX_NESTING_PAIRS {
                depths[component] = None;
                break;
            }
            if open[outer] || volumes[outer].abs() <= volumes[component].abs() || !encloses(outer, component) {
                continue;
            }
            tests += 1;
            if tests > MAX_NESTING_TESTS {
                depths[component] = None;
                break;
            }
            let index = indexes[outer].get_or_insert_with(|| {
                crate::meshanalysis::MeshIndex::from_faces(points, members[outer].iter().map(|&f| &triangles[f as usize]))
            });
            if index.contains(probe) {
                depth += 1;
            }
        }
        if let Some(slot) = depths[component].as_mut() {
            *slot = depth;
        }
    }
    for component in 0..count {
        if volumes[component] == 0.0 || open[component] {
            continue;
        }
        let Some(depth) = depths[component] else {
            stats.undetermined += 1;
            continue;
        };
        let want_positive = depth % 2 == 0;
        if (volumes[component] > 0.0) != want_positive {
            for &face in &members[component] {
                triangles[face as usize].swap(1, 2);
            }
            stats.flipped += members[component].len();
        }
    }
    stats
}

// ---------------------------------------------------------------------------
// Decimation (Garland-Heckbert quadric error metric)
// ---------------------------------------------------------------------------

/// Symmetric 4x4 plane quadric: [aa, ab, ac, ad, bb, bc, bd, cc, cd, dd].
type Quadric = [f64; 10];

fn plane_quadric(normal: Vec3, d: f64, weight: f64) -> Quadric {
    let [a, b, c] = normal;
    [a * a, a * b, a * c, a * d, b * b, b * c, b * d, c * c, c * d, d * d].map(|value| value * weight)
}

fn quadric_error(q: &Quadric, p: Vec3) -> f64 {
    let [x, y, z] = p;
    q[0] * x * x
        + 2.0 * q[1] * x * y
        + 2.0 * q[2] * x * z
        + 2.0 * q[3] * x
        + q[4] * y * y
        + 2.0 * q[5] * y * z
        + 2.0 * q[6] * y
        + q[7] * z * z
        + 2.0 * q[8] * z
        + q[9]
}

/// The point minimising the summed quadric, or the best of the endpoints
/// and midpoint when the system is ill-conditioned (flat or ridge-like
/// neighbourhoods) or the solution lands far from the edge.
fn optimal_position(q: &Quadric, a: Vec3, b: Vec3) -> (Vec3, f64) {
    let m = [[q[0], q[1], q[2]], [q[1], q[4], q[5]], [q[2], q[5], q[7]]];
    let rhs = [-q[3], -q[6], -q[8]];
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    let trace = q[0] + q[4] + q[7];
    let midpoint = scale(add(a, b), 0.5);
    if det.abs() > 1e-9 * trace * trace * trace && det.is_finite() {
        let solve = |column: usize| {
            let mut replaced = m;
            for row in 0..3 {
                replaced[row][column] = rhs[row];
            }
            (replaced[0][0] * (replaced[1][1] * replaced[2][2] - replaced[1][2] * replaced[2][1])
                - replaced[0][1] * (replaced[1][0] * replaced[2][2] - replaced[1][2] * replaced[2][0])
                + replaced[0][2] * (replaced[1][0] * replaced[2][1] - replaced[1][1] * replaced[2][0]))
                / det
        };
        let candidate = [solve(0), solve(1), solve(2)];
        let edge_length_squared = distance_squared(a, b);
        if candidate.iter().all(|c| c.is_finite()) && distance_squared(candidate, midpoint) <= edge_length_squared {
            return (candidate, quadric_error(q, candidate).max(0.0));
        }
    }
    [a, b, midpoint]
        .into_iter()
        .map(|p| (p, quadric_error(q, p).max(0.0)))
        .min_by(|x, y| x.1.total_cmp(&y.1))
        .unwrap_or((midpoint, 0.0))
}

/// One heap entry; stale once either endpoint's version moves on.
#[derive(Clone, Copy)]
struct Candidate {
    cost: f64,
    keep: u32,
    remove: u32,
    keep_version: u32,
    remove_version: u32,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Candidate {}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Candidate {
    /// Reversed so `BinaryHeap` pops the cheapest collapse first.
    fn cmp(&self, other: &Self) -> Ordering {
        other.cost.total_cmp(&self.cost)
    }
}

struct Decimator<'a> {
    points: &'a mut Vec<Vec3>,
    triangles: &'a mut Vec<[u32; 3]>,
    quadrics: Vec<Quadric>,
    adjacency: Vec<Vec<u32>>,
    triangle_alive: Vec<bool>,
    vertex_alive: Vec<bool>,
    /// Vertices on open, non-manifold or inconsistently wound edges never
    /// move, so decimation cannot make a bad region worse.
    locked: Vec<bool>,
    version: Vec<u32>,
    /// Vertices confirmed to touch more than [`MAX_COLLAPSE_VALENCE`] live
    /// faces; they never take part in a collapse (see `too_dense`).
    dense: Vec<bool>,
    heap: BinaryHeap<Candidate>,
    alive_triangles: usize,
    scratch_keep: Vec<u32>,
    scratch_remove: Vec<u32>,
    scratch_neighbours: Vec<u32>,
}

/// Collapses edges in quadric-error order until `target` triangles remain.
/// Each collapse passes the link condition (keeps the surface a 2-manifold)
/// and must not flip or degenerate any surrounding face. Returns false when
/// the deadline in `work` cut it short.
fn decimate(points: &mut Vec<Vec3>, triangles: &mut Vec<[u32; 3]>, target: usize, work: &mut Work) -> bool {
    // Work around the bounding-box centre so plane offsets stay small and
    // the quadrics keep their precision far from the origin.
    let center = bounds_of(points, triangles).map(|(lo, hi)| scale(add(lo, hi), 0.5)).unwrap_or([0.0; 3]);
    for point in points.iter_mut() {
        *point = sub(*point, center);
    }

    let vertex_count = points.len();
    let mut quadrics = vec![[0.0; 10]; vertex_count];
    let mut degree = vec![0u32; vertex_count];
    for triangle in triangles.iter() {
        let [a, b, c] = triangle.map(|index| points[index as usize]);
        let normal = face_normal(a, b, c);
        let doubled_area = length(normal);
        if doubled_area > 0.0 {
            let unit = scale(normal, 1.0 / doubled_area);
            let quadric = plane_quadric(unit, -dot(unit, a), doubled_area * 0.5);
            for &index in triangle {
                add_quadric(&mut quadrics[index as usize], &quadric);
            }
        }
        for &index in triangle {
            degree[index as usize] += 1;
        }
    }
    let mut adjacency: Vec<Vec<u32>> = degree.iter().map(|&d| Vec::with_capacity(d as usize)).collect();
    for (index, triangle) in triangles.iter().enumerate() {
        for &vertex in triangle {
            adjacency[vertex as usize].push(index as u32);
        }
    }
    let mut locked = vec![false; vertex_count];
    let incidence = edge_incidence(triangles);
    for run in edge_runs(&incidence) {
        let healthy = run.len() == 2
            && corner_is_forward(triangles, run[0].1) != corner_is_forward(triangles, run[1].1);
        if !healthy {
            let key = run[0].0;
            locked[(key >> 32) as usize] = true;
            locked[(key & 0xffff_ffff) as usize] = true;
        }
    }
    drop(incidence);

    let alive_triangles = triangles.len();
    let mut decimator = Decimator {
        triangle_alive: vec![true; triangles.len()],
        vertex_alive: vec![true; vertex_count],
        version: vec![0; vertex_count],
        dense: vec![false; vertex_count],
        heap: BinaryHeap::new(),
        points,
        triangles,
        quadrics,
        adjacency,
        locked,
        alive_triangles,
        scratch_keep: Vec::new(),
        scratch_remove: Vec::new(),
        scratch_neighbours: Vec::new(),
    };

    // Rejected candidates are only re-queued when an endpoint changes, so a
    // few rebuild rounds retry edges whose neighbourhood has since moved.
    let mut completed = true;
    let mut pops: u64 = 0;
    'rounds: for _round in 0..4 {
        if decimator.alive_triangles <= target {
            break;
        }
        if work.expired() {
            completed = false;
            break;
        }
        decimator.rebuild_heap();
        let mut collapsed_any = false;
        while decimator.alive_triangles > target {
            pops += 1;
            if pops % 4096 == 0 && work.expired() {
                completed = false;
                break 'rounds;
            }
            let Some(candidate) = decimator.heap.pop() else { break };
            let (keep, remove) = (candidate.keep as usize, candidate.remove as usize);
            if !decimator.vertex_alive[keep]
                || !decimator.vertex_alive[remove]
                || decimator.version[keep] != candidate.keep_version
                || decimator.version[remove] != candidate.remove_version
            {
                continue;
            }
            if decimator.try_collapse(candidate.keep, candidate.remove) {
                collapsed_any = true;
            }
        }
        if !collapsed_any {
            break;
        }
    }

    let Decimator { points, triangles, triangle_alive, .. } = decimator;
    let mut index = 0;
    triangles.retain(|_| {
        index += 1;
        triangle_alive[index - 1]
    });
    for point in points.iter_mut() {
        *point = add(*point, center);
    }
    completed
}

fn add_quadric(target: &mut Quadric, other: &Quadric) {
    for (t, o) in target.iter_mut().zip(other) {
        *t += o;
    }
}

impl Decimator<'_> {
    fn rebuild_heap(&mut self) {
        self.heap.clear();
        for index in 0..self.triangles.len() {
            if !self.triangle_alive[index] {
                continue;
            }
            let triangle = self.triangles[index];
            for slot in 0..3 {
                let (a, b) = (triangle[slot], triangle[(slot + 1) % 3]);
                // Each interior edge appears once in each direction.
                if a < b {
                    self.push_candidate(a, b);
                }
            }
        }
    }

    /// Whether `vertex` touches more than [`MAX_COLLAPSE_VALENCE`] live
    /// faces. Dead faces are pruned only when the raw list is over the cap,
    /// and a vertex found dense stays dense, so a hostile 100k-face fan
    /// costs one prune, not one per candidate edge.
    fn too_dense(&mut self, vertex: u32) -> bool {
        let v = vertex as usize;
        if self.dense[v] {
            return true;
        }
        if self.adjacency[v].len() <= MAX_COLLAPSE_VALENCE {
            return false;
        }
        let alive = &self.triangle_alive;
        self.adjacency[v].retain(|&face| alive[face as usize]);
        if self.adjacency[v].len() > MAX_COLLAPSE_VALENCE {
            self.dense[v] = true;
        }
        self.dense[v]
    }

    fn push_candidate(&mut self, keep: u32, remove: u32) {
        if self.locked[keep as usize] || self.locked[remove as usize] {
            return;
        }
        if self.too_dense(keep) || self.too_dense(remove) {
            return;
        }
        let mut quadric = self.quadrics[keep as usize];
        add_quadric(&mut quadric, &self.quadrics[remove as usize]);
        let (_, cost) = optimal_position(&quadric, self.points[keep as usize], self.points[remove as usize]);
        self.heap.push(Candidate {
            cost,
            keep,
            remove,
            keep_version: self.version[keep as usize],
            remove_version: self.version[remove as usize],
        });
    }

    /// Alive triangles around `vertex`, pruning dead ones from its list.
    fn live_faces(&mut self, vertex: u32, into: &mut Vec<u32>) {
        let alive = &self.triangle_alive;
        self.adjacency[vertex as usize].retain(|&face| alive[face as usize]);
        into.clear();
        into.extend_from_slice(&self.adjacency[vertex as usize]);
    }

    fn try_collapse(&mut self, keep: u32, remove: u32) -> bool {
        // Both endpoints are bounded, so every one-ring scan below is
        // O(MAX_COLLAPSE_VALENCE) and linear membership tests stay cheap.
        if self.too_dense(keep) || self.too_dense(remove) {
            return false;
        }
        let mut keep_faces = std::mem::take(&mut self.scratch_keep);
        let mut remove_faces = std::mem::take(&mut self.scratch_remove);
        let mut neighbours = std::mem::take(&mut self.scratch_neighbours);
        self.live_faces(keep, &mut keep_faces);
        self.live_faces(remove, &mut remove_faces);
        let accepted = self.collapse_is_valid(keep, remove, &keep_faces, &remove_faces, &mut neighbours);
        if let Some(position) = accepted {
            self.apply_collapse(keep, remove, position, &remove_faces, &mut neighbours);
        }
        self.scratch_keep = keep_faces;
        self.scratch_remove = remove_faces;
        self.scratch_neighbours = neighbours;
        accepted.is_some()
    }

    fn collapse_is_valid(
        &self,
        keep: u32,
        remove: u32,
        keep_faces: &[u32],
        remove_faces: &[u32],
        neighbours: &mut Vec<u32>,
    ) -> Option<Vec3> {
        let shares_both = |face: u32| self.triangles[face as usize].contains(&keep);
        if remove_faces.iter().filter(|&&face| shares_both(face)).count() != 2 {
            return None;
        }
        // Link condition: the one-rings may only meet at the two vertices
        // opposite the collapsing edge.
        neighbours.clear();
        for &face in keep_faces {
            for &vertex in &self.triangles[face as usize] {
                if vertex != keep && vertex != remove && !neighbours.contains(&vertex) {
                    neighbours.push(vertex);
                }
            }
        }
        let keep_ring = neighbours.len();
        let mut common = 0;
        let mut remove_only = 0;
        let mut seen_remove: [u32; 64] = [u32::MAX; 64];
        let mut seen_count = 0;
        for &face in remove_faces {
            for &vertex in &self.triangles[face as usize] {
                if vertex == keep || vertex == remove || seen_remove[..seen_count.min(64)].contains(&vertex) {
                    continue;
                }
                if seen_count >= 64 {
                    return None; // pathological valence; leave it alone
                }
                seen_remove[seen_count] = vertex;
                seen_count += 1;
                if neighbours[..keep_ring].contains(&vertex) {
                    common += 1;
                } else {
                    remove_only += 1;
                }
            }
        }
        // Exactly two shared neighbours, and not the last tetrahedron.
        if common != 2 || keep_ring + remove_only < 3 {
            return None;
        }

        let mut quadric = self.quadrics[keep as usize];
        add_quadric(&mut quadric, &self.quadrics[remove as usize]);
        let (position, _) = optimal_position(&quadric, self.points[keep as usize], self.points[remove as usize]);

        for (&face, moving) in keep_faces.iter().map(|f| (f, keep)).chain(remove_faces.iter().map(|f| (f, remove))) {
            let triangle = self.triangles[face as usize];
            if triangle.contains(&keep) && triangle.contains(&remove) {
                continue;
            }
            let before = triangle.map(|index| self.points[index as usize]);
            let after = triangle.map(|index| if index == moving { position } else { self.points[index as usize] });
            let normal_before = face_normal(before[0], before[1], before[2]);
            let normal_after = face_normal(after[0], after[1], after[2]);
            if dot(normal_before, normal_after) <= 0.01 * length(normal_before) * length(normal_after) {
                return None;
            }
        }
        Some(position)
    }

    fn apply_collapse(&mut self, keep: u32, remove: u32, position: Vec3, remove_faces: &[u32], neighbours: &mut Vec<u32>) {
        for &face in remove_faces {
            let triangle = &mut self.triangles[face as usize];
            if triangle.contains(&keep) {
                self.triangle_alive[face as usize] = false;
                self.alive_triangles -= 1;
            } else {
                for index in triangle.iter_mut() {
                    if *index == remove {
                        *index = keep;
                    }
                }
                self.adjacency[keep as usize].push(face);
            }
        }
        let removed_quadric = self.quadrics[remove as usize];
        add_quadric(&mut self.quadrics[keep as usize], &removed_quadric);
        self.points[keep as usize] = position;
        self.version[keep as usize] += 1;
        self.vertex_alive[remove as usize] = false;
        self.adjacency[remove as usize] = Vec::new();

        let alive = &self.triangle_alive;
        self.adjacency[keep as usize].retain(|&face| alive[face as usize]);
        neighbours.clear();
        for &face in &self.adjacency[keep as usize] {
            for &vertex in &self.triangles[face as usize] {
                if vertex != keep && !neighbours.contains(&vertex) {
                    neighbours.push(vertex);
                }
            }
        }
        for index in 0..neighbours.len() {
            let other = neighbours[index];
            self.push_candidate(keep, other);
        }
    }
}

// ---------------------------------------------------------------------------
// Geometry helpers
// ---------------------------------------------------------------------------

fn normalize(a: Vec3) -> Option<Vec3> {
    let len = length(a);
    (len > 0.0 && len.is_finite()).then(|| scale(a, 1.0 / len))
}
fn distance_squared(a: Vec3, b: Vec3) -> f64 {
    let d = sub(a, b);
    dot(d, d)
}
/// Unnormalised normal; its length is twice the triangle's area.
fn face_normal(a: Vec3, b: Vec3, c: Vec3) -> Vec3 {
    cross(sub(b, a), sub(c, a))
}
fn triangle_area(points: &[Vec3], triangle: &[u32; 3]) -> f64 {
    let [a, b, c] = triangle.map(|index| points[index as usize]);
    0.5 * length(face_normal(a, b, c))
}

/// Bounds of the vertices actually used by `triangles`.
fn bounds_of(points: &[Vec3], triangles: &[[u32; 3]]) -> Option<(Vec3, Vec3)> {
    let mut bounds: Option<(Vec3, Vec3)> = None;
    for triangle in triangles {
        for &index in triangle {
            let p = points[index as usize];
            bounds = Some(match bounds {
                None => (p, p),
                Some((lo, hi)) => (
                    [lo[0].min(p[0]), lo[1].min(p[1]), lo[2].min(p[2])],
                    [hi[0].max(p[0]), hi[1].max(p[1]), hi[2].max(p[2])],
                ),
            });
        }
    }
    bounds
}

/// Divergence-theorem volume, measured about `reference` (a point near the
/// mesh) to limit cancellation error.
fn signed_volume<'a>(points: &[Vec3], triangles: impl Iterator<Item = &'a [u32; 3]>, reference: Vec3) -> f64 {
    triangles
        .map(|triangle| {
            let [a, b, c] = triangle.map(|index| sub(points[index as usize], reference));
            dot(a, cross(b, c))
        })
        .sum::<f64>()
        / 6.0
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// The storage encoding of a cleaned mesh (binary STL).
pub fn encode_binary_stl(mesh: &TriMesh) -> Vec<u8> {
    let mut output = Vec::with_capacity(84 + 50 * mesh.triangles.len());
    let mut header = [0u8; 80];
    let label = b"ReOpenSCAD cleaned mesh";
    header[..label.len()].copy_from_slice(label);
    output.extend_from_slice(&header);
    output.extend_from_slice(&(mesh.triangles.len() as u32).to_le_bytes());
    for triangle in &mesh.triangles {
        let [a, b, c] = triangle.map(|index| mesh.points[index as usize]);
        let normal = normalize(face_normal(a, b, c)).unwrap_or([0.0; 3]);
        for value in normal.iter().chain(a.iter()).chain(b.iter()).chain(c.iter()) {
            output.extend_from_slice(&(*value as f32).to_le_bytes());
        }
        output.extend_from_slice(&[0, 0]);
    }
    output
}

#[cfg(test)]
mod tests {
    /// A cancelled request stops cleaning (and decimation) with the cancel
    /// message, while the pure `clean` ignores interrupts.
    #[test]
    fn clean_until_observes_request_cancellation() {
        let mut mesh = TriMesh::default();
        let n = 40usize;
        for i in 0..=n {
            for j in 0..=n {
                mesh.points.push([i as f64, j as f64, ((i * j) % 7) as f64 * 0.1]);
            }
        }
        for i in 0..n {
            for j in 0..n {
                let a = (i * (n + 1) + j) as u32;
                let b = a + 1;
                let c = a + (n + 1) as u32;
                mesh.triangles.push([a, b, c + 1]);
                mesh.triangles.push([a, c + 1, c]);
            }
        }
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let _scope = crate::csg::bsp::cancellation_scope(flag);
        let error = clean_until(mesh.clone(), CleanOptions { triangle_budget: 100 }, None).unwrap_err();
        assert!(error.contains("cancelled"), "{error}");
        assert!(!clean(mesh, CleanOptions { triangle_budget: 100 }).0.triangles.is_empty());
    }

    use super::*;
    use std::f64::consts::PI;
    use std::time::Instant;

    /// Small deterministic generator so tests need no dependencies.
    struct Lcg(u64);
    impl Lcg {
        fn next_u32(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
        fn unit(&mut self) -> f64 {
            self.next_u32() as f64 / (1u64 << 31) as f64
        }
    }

    fn uv_sphere(radius: f64, slices: u32, stacks: u32, noise: f64, seed: u64) -> TriMesh {
        let mut random = Lcg(seed);
        let mut jitter = |p: Vec3| scale(p, 1.0 + noise * (random.unit() - 0.5));
        let mut points = vec![jitter([0.0, 0.0, radius])];
        for i in 1..stacks {
            let theta = PI * i as f64 / stacks as f64;
            for j in 0..slices {
                let phi = 2.0 * PI * j as f64 / slices as f64;
                points.push(jitter([
                    radius * theta.sin() * phi.cos(),
                    radius * theta.sin() * phi.sin(),
                    radius * theta.cos(),
                ]));
            }
        }
        let bottom = points.len() as u32;
        points.push(jitter([0.0, 0.0, -radius]));
        let ring = |i: u32, j: u32| 1 + (i - 1) * slices + j % slices;
        let mut triangles = Vec::new();
        for j in 0..slices {
            triangles.push([0, ring(1, j), ring(1, j + 1)]);
        }
        for i in 1..stacks - 1 {
            for j in 0..slices {
                let (a, b, c, d) = (ring(i, j), ring(i, j + 1), ring(i + 1, j), ring(i + 1, j + 1));
                triangles.push([a, c, d]);
                triangles.push([a, d, b]);
            }
        }
        for j in 0..slices {
            triangles.push([bottom, ring(stacks - 1, j + 1), ring(stacks - 1, j)]);
        }
        TriMesh { points, triangles }
    }

    fn unit_cube() -> TriMesh {
        let points = (0..8).map(|i| [(i & 1) as f64, ((i >> 1) & 1) as f64, ((i >> 2) & 1) as f64]).collect();
        let triangles = vec![
            [0, 2, 1], [1, 2, 3], // z = 0
            [4, 5, 6], [5, 7, 6], // z = 1
            [0, 1, 4], [1, 5, 4], // y = 0
            [2, 6, 3], [3, 6, 7], // y = 1
            [0, 4, 2], [2, 4, 6], // x = 0
            [1, 3, 5], [3, 7, 5], // x = 1
        ];
        TriMesh { points, triangles }
    }

    fn volume(mesh: &TriMesh) -> f64 {
        signed_volume(&mesh.points, mesh.triangles.iter(), [0.0; 3])
    }

    fn ascii_stl(mesh: &TriMesh) -> String {
        let mut text = String::from("solid test\n");
        for triangle in &mesh.triangles {
            text.push_str("  facet normal 0 0 0\n    outer loop\n");
            for &index in triangle {
                let p = mesh.points[index as usize];
                text.push_str(&format!("      vertex {} {} {}\n", p[0], p[1], p[2]));
            }
            text.push_str("    endloop\n  endfacet\n");
        }
        text.push_str("endsolid test\n");
        text
    }

    #[test]
    fn cube_is_already_valid() {
        let cube = unit_cube();
        assert!((volume(&cube) - 1.0).abs() < 1e-12);
        let (_, report) = clean(cube, CleanOptions::default());
        assert!(report.watertight, "{report:?}");
        assert!(!report.orientation_fixed);
    }

    #[test]
    fn binary_roundtrip() {
        let sphere = uv_sphere(5.0, 24, 12, 0.0, 1);
        let bytes = encode_binary_stl(&sphere);
        let parsed = parse_stl(&bytes).unwrap();
        assert_eq!(parsed.triangles.len(), sphere.triangles.len());
        assert_eq!(parsed.points.len(), sphere.points.len());
        assert!((volume(&parsed) - volume(&sphere)).abs() < 1e-3);
    }

    #[test]
    fn ascii_roundtrip() {
        let sphere = uv_sphere(5.0, 24, 12, 0.0, 1);
        let parsed = parse_stl(ascii_stl(&sphere).as_bytes()).unwrap();
        assert_eq!(parsed.triangles.len(), sphere.triangles.len());
        assert_eq!(parsed.points.len(), sphere.points.len());
        assert!((volume(&parsed) - volume(&sphere)).abs() < 1e-3);
    }

    #[test]
    fn binary_with_solid_header_is_binary() {
        let mut bytes = encode_binary_stl(&unit_cube());
        bytes[..6].copy_from_slice(b"solid ");
        let parsed = parse_stl(&bytes).unwrap();
        assert_eq!(parsed.triangles.len(), 12);
        assert_eq!(parsed.points.len(), 8);
    }

    #[test]
    fn rejects_malicious_and_malformed_input() {
        // Header claims ~4e9 triangles with almost no body.
        let mut bytes = vec![0u8; 84 + 50];
        bytes[80..84].copy_from_slice(&4_000_000_000u32.to_le_bytes());
        assert!(parse_stl(&bytes).unwrap_err().contains("limit"));
        // Count that fits the cap but not the file.
        bytes[80..84].copy_from_slice(&1000u32.to_le_bytes());
        assert!(parse_stl(&bytes).unwrap_err().contains("truncated"));
        // Truncated binary of a real mesh.
        let full = encode_binary_stl(&unit_cube());
        assert!(parse_stl(&full[..full.len() - 7]).is_err());
        // Oversized upload.
        assert!(parse_stl(&vec![0u8; MAX_UPLOAD_BYTES + 1]).is_err());
        // Non-finite coordinates.
        let mut nan = full.clone();
        nan[84 + 12..84 + 16].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(parse_stl(&nan).is_err());
        assert!(parse_stl(b"solid x\nfacet normal 0 0 1\nouter loop\nvertex 0 0 inf\nvertex 1 0 0\nvertex 0 1 0\nendloop\nendfacet\nendsolid").is_err());
        assert!(parse_stl(b"solid x\nfacet normal 0 0 1\nouter loop\nvertex 0 0 1e300\nvertex 1 0 0\nvertex 0 1 0\nendloop\nendfacet\nendsolid").is_err());
        // Empty and degenerate files.
        assert!(parse_stl(b"").is_err());
        assert!(parse_stl(b"solid empty\nendsolid empty\n").is_err());
        let mut empty_binary = vec![0u8; 84];
        empty_binary[80..84].copy_from_slice(&0u32.to_le_bytes());
        assert!(parse_stl(&empty_binary).is_err());
        assert!(parse_stl(b"solid x\nfacet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 1 0").is_err());
        assert!(parse_stl(b"solid x\nfacet\nvertex 0 0 0\nvertex 1 0 0\nendfacet").is_err());
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut random = Lcg(42);
        let valid = encode_binary_stl(&unit_cube());
        let ascii = ascii_stl(&unit_cube()).into_bytes();
        for round in 0..3000 {
            let length = (random.next_u32() % 2000) as usize;
            let mut bytes: Vec<u8> = (0..length).map(|_| random.next_u32() as u8).collect();
            match round % 4 {
                1 => {
                    bytes = valid.clone();
                    for _ in 0..1 + random.next_u32() % 8 {
                        let at = random.next_u32() as usize % bytes.len();
                        bytes[at] = random.next_u32() as u8;
                    }
                }
                2 => {
                    bytes = ascii.clone();
                    for _ in 0..1 + random.next_u32() % 8 {
                        let at = random.next_u32() as usize % bytes.len();
                        bytes[at] = b" 0123456789.-eEvertxfacndl\n"[random.next_u32() as usize % 27];
                    }
                    let cut = random.next_u32() as usize % (bytes.len() + 1);
                    bytes.truncate(cut.max(bytes.len() / 2));
                }
                3 => {
                    let mut prefixed = b"solid ".to_vec();
                    prefixed.extend(bytes);
                    bytes = prefixed;
                }
                _ => {}
            }
            if let Ok(mesh) = parse_stl(&bytes) {
                let (cleaned, _) = clean(mesh, CleanOptions::default());
                assert!(cleaned.triangles.iter().flatten().all(|&i| (i as usize) < cleaned.points.len()));
            }
        }
    }

    #[test]
    fn random_triangle_soups_never_panic() {
        let mut random = Lcg(7);
        for _ in 0..2000 {
            let point_count = 1 + random.next_u32() % 12;
            let points: Vec<Vec3> = (0..point_count)
                .map(|_| [random.unit().floor(), (random.unit() * 3.0).floor(), random.unit() * 2.0])
                .collect();
            let triangles: Vec<[u32; 3]> = (0..random.next_u32() % 40)
                .map(|_| [0; 3].map(|_| random.next_u32() % (point_count + 1)))
                .collect();
            let (cleaned, report) = clean(TriMesh { points, triangles }, CleanOptions { triangle_budget: 8 });
            assert!(cleaned.triangles.iter().flatten().all(|&i| (i as usize) < cleaned.points.len()));
            assert_eq!(report.output_triangles, cleaned.triangles.len());
        }
    }

    #[test]
    fn flipped_triangles_are_reoriented() {
        let mut sphere = uv_sphere(10.0, 48, 24, 0.0, 5);
        let expected = volume(&sphere);
        let mut random = Lcg(11);
        let mut flipped = 0;
        for triangle in sphere.triangles.iter_mut() {
            if random.unit() < 0.15 {
                triangle.swap(1, 2);
                flipped += 1;
            }
        }
        assert!(flipped > 0);
        let (_, report) = clean(sphere, CleanOptions::default());
        assert!(report.watertight, "{report:?}");
        assert!(report.orientation_fixed);
        assert!((report.volume - expected).abs() < 1e-6 * expected, "{} vs {expected}", report.volume);

        // Wholly inside-out input comes back outward.
        let mut inverted = uv_sphere(10.0, 48, 24, 0.0, 5);
        for triangle in inverted.triangles.iter_mut() {
            triangle.swap(1, 2);
        }
        let (_, report) = clean(inverted, CleanOptions::default());
        assert!(report.volume > 0.0 && report.orientation_fixed);
    }

    #[test]
    fn cavity_stays_inward() {
        let outer = uv_sphere(10.0, 32, 16, 0.0, 1);
        let mut inner = uv_sphere(5.0, 32, 16, 0.0, 1);
        let offset = outer.points.len() as u32;
        // Inner shell deliberately wound outward too; it must end up inward.
        let mut mesh = outer.clone();
        mesh.points.append(&mut inner.points);
        mesh.triangles.extend(inner.triangles.iter().map(|t| t.map(|i| i + offset)));
        let expected = volume(&outer) - volume(&inner_sphere());
        let (_, report) = clean(mesh, CleanOptions::default());
        assert!(report.watertight);
        assert!((report.volume - expected).abs() < 1e-6 * expected, "{} vs {expected}", report.volume);
        fn inner_sphere() -> TriMesh {
            uv_sphere(5.0, 32, 16, 0.0, 1)
        }
    }

    #[test]
    fn debris_components_are_removed() {
        let mut mesh = uv_sphere(10.0, 30, 20, 0.0, 9);
        let sphere_triangles = mesh.triangles.len();
        let base = mesh.points.len() as u32;
        // A tiny tetrahedron of dust and a stray triangle, well away.
        mesh.points.extend([[30.0, 0.0, 0.0], [30.1, 0.0, 0.0], [30.0, 0.1, 0.0], [30.0, 0.0, 0.1]]);
        mesh.triangles.extend([[base, base + 2, base + 1], [base, base + 1, base + 3], [base, base + 3, base + 2], [base + 1, base + 2, base + 3]]);
        mesh.points.extend([[-30.0, 0.0, 0.0], [-29.0, 0.0, 0.0], [-30.0, 1.0, 0.0]]);
        mesh.triangles.push([base + 4, base + 5, base + 6]);
        let (cleaned, report) = clean(mesh, CleanOptions::default());
        assert_eq!(report.components, 3);
        assert_eq!(report.small_components_removed, 2);
        assert_eq!(cleaned.triangles.len(), sphere_triangles);
        assert!(report.watertight);
        assert!(report.bounds_max[0] < 11.0 && report.bounds_min[0] > -11.0);
    }

    #[test]
    fn duplicates_near_duplicates_and_non_manifold_fins() {
        let mut cube = unit_cube();
        // A near-duplicate of vertex 7 used by one face.
        cube.points.push([1.0 + 1e-9, 1.0, 1.0]);
        cube.triangles[3] = [5, 8, 6];
        // An exact duplicate face and a fin hanging off edge 0-1.
        cube.triangles.push([1, 2, 3]);
        cube.points.push([0.5, -0.4, -0.2]);
        cube.triangles.push([0, 1, 9]);
        let (_, report) = clean(cube, CleanOptions::default());
        assert_eq!(report.vertices_welded, 1);
        assert_eq!(report.duplicate_removed, 1);
        assert_eq!(report.non_manifold_faces_removed, 1);
        assert!(report.watertight, "{report:?}");
        assert!((report.volume - 1.0).abs() < 1e-9);
    }

    #[test]
    fn noisy_sphere_decimates_within_budget() {
        let sphere = uv_sphere(20.0, 400, 251, 0.002, 13);
        assert_eq!(sphere.triangles.len(), 200_000);
        let expected = volume(&sphere);
        let started = Instant::now();
        let (cleaned, report) = clean(sphere, CleanOptions { triangle_budget: 60_000 });
        let elapsed = started.elapsed();
        eprintln!("200k -> {} triangles in {elapsed:?}", cleaned.triangles.len());
        assert!(report.decimated);
        assert!(cleaned.triangles.len() <= 60_000 && cleaned.triangles.len() > 55_000);
        assert!(report.watertight, "{report:?}");
        assert_eq!(report.boundary_edges_remaining, 0);
        let error = (report.volume - expected).abs() / expected;
        assert!(error < 0.01, "volume error {error}");
    }

    /// Generous wall-clock bound for the adversarial tests: they must not
    /// blow up, but a loaded CI box can be several times slower than a
    /// laptop, so the bound is only enforced in release builds.
    fn assert_fast(elapsed: std::time::Duration, seconds: f64, what: &str) {
        eprintln!("{what}: {elapsed:?}");
        assert!(cfg!(debug_assertions) || elapsed.as_secs_f64() < seconds, "{what} took {elapsed:?}");
    }

    #[test]
    fn huge_open_fan_cleans_quickly() {
        // A 100k-triangle cone around one apex, its 100k-edge base left open.
        let ring = 100_000u32;
        let mut mesh = TriMesh { points: vec![[0.0, 0.0, 10.0]], triangles: Vec::new() };
        for i in 0..ring {
            let angle = std::f64::consts::TAU * i as f64 / ring as f64;
            mesh.points.push([20.0 * angle.cos(), 20.0 * angle.sin(), 0.0]);
        }
        for i in 0..ring {
            mesh.triangles.push([0, 1 + i, 1 + (i + 1) % ring]);
        }
        let started = Instant::now();
        let (cleaned, report) = clean(mesh, CleanOptions::default());
        assert_fast(started.elapsed(), 5.0, "100k-triangle fan");
        assert!(!report.watertight, "{report:?}");
        assert_eq!(report.boundary_edges_remaining, 100_000, "{report:?}");
        assert!(cleaned.triangles.len() <= DEFAULT_TRIANGLE_BUDGET, "{}", cleaned.triangles.len());
    }

    #[test]
    fn deadline_fails_cleanly() {
        let sphere = uv_sphere(20.0, 400, 251, 0.002, 13);
        let past = Instant::now();
        let error = clean_until(sphere.clone(), CleanOptions::default(), Some(past)).unwrap_err();
        assert!(error.contains("time budget"), "{error}");
        let far = Instant::now() + std::time::Duration::from_secs(600);
        let (cleaned, _) = clean_until(sphere, CleanOptions::default(), Some(far)).unwrap();
        assert!(cleaned.triangles.len() <= DEFAULT_TRIANGLE_BUDGET);
    }

    #[test]
    fn forty_hollow_boxes_keep_their_cavities() {
        fn box_shell(lo: Vec3, hi: Vec3, inward: bool, mesh: &mut TriMesh) {
            let base = mesh.points.len() as u32;
            for i in 0..8 {
                mesh.points.push([
                    if i & 1 == 0 { lo[0] } else { hi[0] },
                    if i & 2 == 0 { lo[1] } else { hi[1] },
                    if i & 4 == 0 { lo[2] } else { hi[2] },
                ]);
            }
            let faces = [
                [0, 2, 3, 1], [4, 5, 7, 6], [0, 1, 5, 4], [2, 6, 7, 3], [0, 4, 6, 2], [1, 3, 7, 5],
            ];
            for [a, b, c, d] in faces {
                for mut t in [[a, b, c], [a, c, d]] {
                    if inward {
                        t.swap(1, 2);
                    }
                    mesh.triangles.push(t.map(|i| base + i));
                }
            }
        }
        let mut mesh = TriMesh::default();
        for k in 0..40 {
            let x = k as f64 * 12.0;
            box_shell([x, 0.0, 0.0], [x + 10.0, 10.0, 10.0], false, &mut mesh);
            // Cavities wound correctly (inward) in half, outward in the rest.
            box_shell([x + 2.0, 2.0, 2.0], [x + 8.0, 8.0, 8.0], k % 2 == 0, &mut mesh);
        }
        let expected = 40.0 * (1000.0 - 216.0);
        let (_, report) = clean(mesh, CleanOptions::default());
        assert!(report.watertight, "{report:?}");
        assert!((report.volume - expected).abs() < 1e-6 * expected, "{} vs {expected}", report.volume);
    }

    #[test]
    #[ignore = "slow: run with --ignored for the 2M-triangle timing"]
    fn two_million_triangles_timing() {
        let sphere = uv_sphere(50.0, 2000, 501, 0.002, 17);
        let expected = volume(&sphere);
        let bytes = encode_binary_stl(&sphere);
        let started = Instant::now();
        let parsed = if bytes.len() <= MAX_UPLOAD_BYTES { parse_stl(&bytes).unwrap() } else { sphere };
        let parse_time = started.elapsed();
        let (cleaned, report) = clean(parsed, CleanOptions::default());
        eprintln!(
            "2M: parse {parse_time:?}, total {:?}, {} triangles, volume error {:.4}%",
            started.elapsed(),
            cleaned.triangles.len(),
            100.0 * (report.volume - expected).abs() / expected
        );
        assert!(report.watertight);
    }

    /// A 20 x 20 x 5 panel with two 4 x 12 windows, scanned from above: the
    /// top and every wall but no underside, so its open base is the outer
    /// rim with the two window rims inside it.
    fn open_window_panel() -> TriMesh {
        let size = 20u32;
        let window = |x: u32, y: u32| (4..16).contains(&y) && ((4..8).contains(&x) || (12..16).contains(&x));
        let layer = (size + 1) * (size + 1);
        let at = |x: u32, y: u32| y * (size + 1) + x;
        let mut mesh = TriMesh::default();
        for z in [5.0, 0.0] {
            for y in 0..=size {
                for x in 0..=size {
                    mesh.points.push([x as f64, y as f64, z]);
                }
            }
        }
        for y in 0..size {
            for x in (0..size).filter(|&x| !window(x, y)) {
                let (a, b, c, d) = (at(x, y), at(x + 1, y), at(x + 1, y + 1), at(x, y + 1));
                mesh.triangles.extend([[a, b, c], [a, c, d]]);
            }
        }
        let incidence = edge_incidence(&mesh.triangles);
        let rim: Vec<(u32, u32)> = edge_runs(&incidence)
            .filter(|run| run.len() == 1)
            .map(|run| {
                let (triangle, slot) = (mesh.triangles[(run[0].1 / 3) as usize], (run[0].1 % 3) as usize);
                (triangle[slot], triangle[(slot + 1) % 3])
            })
            .collect();
        for (a, b) in rim {
            mesh.triangles.extend([[b, a, a + layer], [b, a + layer, b + layer]]);
        }
        mesh
    }

    #[test]
    fn open_surfaces_stay_open_and_keep_their_winding() {
        let panel = open_window_panel();
        let (cleaned, report) = clean(panel, CleanOptions::default());
        assert!(!report.watertight, "{report:?}");
        assert_eq!((report.boundary_edges_remaining, report.non_manifold_edges), (80 + 2 * 32, 0));
        assert!(!report.orientation_fixed, "{report:?}");
        assert!(report.warnings.iter().any(|w| w.starts_with("Kept as an open surface")), "{:?}", report.warnings);
        // The top still faces up: an open shell is never flipped by volume.
        for triangle in &cleaned.triangles {
            let [a, b, c] = triangle.map(|i| cleaned.points[i as usize]);
            if a[2] == 5.0 && b[2] == 5.0 && c[2] == 5.0 {
                assert!(face_normal(a, b, c)[2] > 0.0);
            }
        }
        // Inside out, it stays as uploaded too.
        let mut inverted = open_window_panel();
        inverted.triangles.iter_mut().for_each(|t| t.swap(1, 2));
        let (_, report) = clean(inverted, CleanOptions::default());
        assert!(!report.orientation_fixed);
    }

}




