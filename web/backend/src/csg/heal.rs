//! Export-time repair of a boolean result that did not come out closed.
//!
//! The BSP booleans are exact in the sense that every output edge lies on two
//! input planes, but the *vertices* are computed in floating point, and the
//! same geometric corner can be reached by different split sequences. Where
//! several planes meet nearly at one point — a scanned mesh offset by a
//! fraction of a millimetre from a copy of itself, a cut plane through a
//! cluster of scan vertices — those answers can land tens of nanometres apart,
//! just beyond [`super::mesh::WELD_EPSILON`]. Welding then joins some of the
//! copies and not others, and the surface comes out with a pinhole: a handful
//! of boundary edges or a tiny non-manifold fan around one point, while the
//! rest of the model is perfect. Which parameter values trip it is effectively
//! random, which is exactly how it looks from outside: a sweep of a tolerance
//! value flips validity on its third decimal.
//!
//! [`heal`] runs only on such a mesh and only touches the defect. It
//!
//! 1. merges the vertices of defective edges that lie within a tolerance of
//!    one another (escalating from a few welding tolerances up to a micron,
//!    capped relative to the model's size),
//! 2. drops the triangles that collapse, and cancels zero-volume fins (the
//!    same triangle present in both orientations),
//! 3. splits a boundary edge at boundary vertices lying on it (T-junctions
//!    the merge's tolerance exposes),
//! 4. closes small boundary loops that remain, by ear clipping, when they are
//!    tiny: short in perimeter, or a sliver whose mean width is below the
//!    tolerance.
//!
//! Each step moves geometry by at most the tolerance in force, and only at
//! defect vertices. The result is kept only when it has strictly fewer
//! defective edges than the input, so a mesh the repair cannot improve is
//! returned untouched, and the [`HealReport`] says where the defects were.

use super::mesh::{FastMap, IndexedMesh, Vec3};

/// Tolerances tried in turn, in model units (mm). The first is a few
/// welding tolerances, the last a micron: far below any printer's resolution.
const STEPS: [f64; 4] = [1.0e-4, 3.0e-4, 1.0e-3, CEILING];

/// Repair passes, each followed by the consumers' re-weld.
const PASSES: usize = 3;

/// The most any vertex is ever moved by the repair, in model units.
const CEILING: f64 = 3.0e-3;

/// No step's tolerance exceeds this fraction of the model's largest extent, so
/// a model authored in metres is not repaired with a millimetre-scale reach.
const RELATIVE_CAP: f64 = 3.0e-5;

/// Meshes with more defective edges than this are reported, not repaired:
/// the repair's cost grows with the defect count, and a surface that broken
/// is not a near-coincidence the repair is designed for.
const MAX_DEFECTS: usize = 100_000;

/// Holes with more edges than this are not filled: they are not pinholes.
const MAX_FILL_EDGES: usize = 64;

/// What [`heal`] did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HealReport {
    /// Defective (boundary, non-manifold or inconsistently wound) edges before.
    pub defects_before: usize,
    /// ... and after the repair.
    pub defects_after: usize,
    /// Largest tolerance the repair needed, in model units.
    pub tolerance: f64,
    /// Where the defects were: one centre per connected group, largest first.
    pub sites: Vec<Vec3>,
    pub merged_vertices: usize,
    pub split_edges: usize,
    pub filled_triangles: usize,
    pub removed_triangles: usize,
    /// Vertices duplicated to pull apart solids that touched along an edge.
    pub separated_vertices: usize,
    /// How far those copies were moved, in model units.
    pub nudge: f64,
    /// Triangles of open, zero-thickness pieces that were dropped.
    pub film_triangles: usize,
}

impl HealReport {
    pub fn healed(&self) -> bool {
        self.defects_after == 0
    }

    /// One console line for the diagnostics.
    pub fn message(&self) -> String {
        let sites: Vec<String> = self
            .sites
            .iter()
            .take(3)
            .map(|p| format!("[{:.3}, {:.3}, {:.3}]", p.x, p.y, p.z))
            .collect();
        let more = if self.sites.len() > 3 {
            format!(" and {} more place(s)", self.sites.len() - 3)
        } else {
            String::new()
        };
        let place = format!("{}{more}", sites.join(", "));
        if self.healed() {
            let mut actions = Vec::new();
            if self.merged_vertices > 0 {
                actions.push(format!("merged {} vertices", self.merged_vertices));
            }
            if self.split_edges > 0 {
                actions.push(format!("split {} edges", self.split_edges));
            }
            if self.filled_triangles > 0 {
                actions.push(format!("filled {} triangles", self.filled_triangles));
            }
            if self.removed_triangles > 0 {
                actions.push(format!(
                    "removed {} doubled or collapsed triangles",
                    self.removed_triangles
                ));
            }
            if self.film_triangles > 0 {
                actions.push(format!(
                    "dropped {} triangles of open zero-thickness film",
                    self.film_triangles
                ));
            }
            if self.separated_vertices > 0 {
                actions.push(format!(
                    "pulled solids that touch along an edge apart by {:.0e} mm",
                    self.nudge
                ));
            }
            format!(
                "WARNING: faces of the boolean operands coincide or touch (within {:.0e} mm) near {place}; \
                 the result had {} open/non-manifold edge(s) there and was auto-healed ({}). Offsetting \
                 coincident or touching faces by a visible amount (e.g. 0.01 mm) avoids this.",
                self.tolerance.max(self.nudge),
                self.defects_before,
                actions.join(", ")
            )
        } else {
            format!(
                "WARNING: faces of the boolean operands coincide or touch near {place}; the result had {} \
                 open/non-manifold edge(s) and {} remain after automatic repair. Offset coincident or \
                 touching faces by a visible amount (e.g. 0.01 mm).",
                self.defects_before, self.defects_after
            )
        }
    }
}

/// Undirected edge census: `(forward, backward)` use counts keyed by the
/// ordered pair `(min, max)`.
fn census(triangles: &[[u32; 3]]) -> FastMap<(u32, u32), (u32, u32)> {
    let mut edges: FastMap<(u32, u32), (u32, u32)> = FastMap::default();
    edges.reserve(triangles.len() * 3 / 2);
    for triangle in triangles {
        for index in 0..3 {
            let (from, to) = (triangle[index], triangle[(index + 1) % 3]);
            if from < to {
                edges.entry((from, to)).or_default().0 += 1;
            } else {
                edges.entry((to, from)).or_default().1 += 1;
            }
        }
    }
    edges
}

fn defective(counts: (u32, u32)) -> bool {
    counts != (1, 1)
}

/// Number of defective edges, the measure the repair minimises.
pub fn defect_count(triangles: &[[u32; 3]]) -> usize {
    census(triangles).values().filter(|counts| defective(**counts)).count()
}

struct UnionFind(Vec<u32>);

impl UnionFind {
    fn find(&mut self, mut x: u32) -> u32 {
        while self.0[x as usize] != x {
            let parent = self.0[x as usize];
            self.0[x as usize] = self.0[parent as usize];
            x = parent;
        }
        x
    }
    /// Joins two sets; the smaller index stays the root, so the surviving
    /// vertex of a cluster is deterministic.
    fn union(&mut self, a: u32, b: u32) -> bool {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return false;
        }
        let (keep, drop) = if ra < rb { (ra, rb) } else { (rb, ra) };
        self.0[drop as usize] = keep;
        true
    }
}

fn cell(point: Vec3, size: f64) -> [i64; 3] {
    [
        (point.x / size).floor() as i64,
        (point.y / size).floor() as i64,
        (point.z / size).floor() as i64,
    ]
}

/// Groups the defect vertices into connected sites (by shared defective
/// edges) and returns their centres, largest group first.
fn defect_sites(vertices: &[Vec3], triangles: &[[u32; 3]]) -> Vec<Vec3> {
    let edges = census(triangles);
    let mut uf = UnionFind((0..vertices.len() as u32).collect());
    let mut involved: Vec<u32> = Vec::new();
    for (&(a, b), &counts) in &edges {
        if defective(counts) {
            uf.union(a, b);
            involved.push(a);
            involved.push(b);
        }
    }
    involved.sort_unstable();
    involved.dedup();
    let mut groups: FastMap<u32, (Vec3, usize)> = FastMap::default();
    for vertex in involved {
        let root = uf.find(vertex);
        let entry = groups.entry(root).or_insert((Vec3::new(0.0, 0.0, 0.0), 0));
        entry.0 = entry.0.add(vertices[vertex as usize]);
        entry.1 += 1;
    }
    let mut sites: Vec<(usize, Vec3)> = groups
        .into_values()
        .map(|(sum, count)| (count, sum.mul(1.0 / count as f64)))
        .collect();
    sites.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(a.1.x.total_cmp(&b.1.x))
            .then(a.1.y.total_cmp(&b.1.y))
            .then(a.1.z.total_cmp(&b.1.z))
    });
    sites.into_iter().map(|(_, centre)| centre).collect()
}

/// Working state: the mesh plus, per triangle, the tag of the input triangle
/// it descends from (a colour slot, or anything else the caller tracks).
struct Work {
    /// How far the farthest input vertex now represented by each vertex has
    /// been moved to it, so repeated merges stay within the tolerance.
    reach: Vec<f64>,
    vertices: Vec<Vec3>,
    triangles: Vec<[u32; 3]>,
    tags: Vec<u32>,
}

impl Work {
    /// Re-welds the referenced vertices exactly as every consumer of the
    /// exported triangles will (a [`super::mesh::VertexWelder`] fed in
    /// triangle order), so the defects the repair sees are the ones they
    /// will see. Returns the defect count afterwards.
    fn reweld(&mut self) -> usize {
        let mut welder = super::mesh::VertexWelder::default();
        let mut reach: Vec<f64> = Vec::new();
        let mut triangles = Vec::with_capacity(self.triangles.len());
        let mut tags = Vec::with_capacity(self.tags.len());
        for (t, tag) in self.triangles.iter().zip(&self.tags) {
            let mapped = t.map(|x| {
                let point = self.vertices[x as usize];
                let index = welder.insert(point);
                if index as usize >= reach.len() {
                    reach.resize(index as usize + 1, 0.0);
                }
                let moved = welder.vertices[index as usize].sub(point).length() + self.reach[x as usize];
                if moved > reach[index as usize] {
                    reach[index as usize] = moved;
                }
                index
            });
            if mapped[0] == mapped[1] || mapped[1] == mapped[2] || mapped[0] == mapped[2] {
                continue;
            }
            triangles.push(mapped);
            tags.push(*tag);
        }
        reach.resize(welder.vertices.len(), 0.0);
        self.vertices = welder.vertices;
        self.reach = reach;
        self.triangles = triangles;
        self.tags = tags;
        self.drop_degenerate();
        defect_count(&self.triangles)
    }

    fn defect_vertices(&self) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        for (&(a, b), &counts) in &census(&self.triangles) {
            if defective(counts) {
                out.push(a);
                out.push(b);
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Step 1 and 2: merge defect vertices within `tolerance`, drop what
    /// collapses, cancel fins. Returns the number of vertices merged away.
    fn merge(&mut self, tolerance: f64) -> usize {
        let candidates = self.defect_vertices();
        if candidates.is_empty() {
            return 0;
        }
        let mut grid: FastMap<[i64; 3], Vec<u32>> = FastMap::default();
        for &vertex in &candidates {
            grid.entry(cell(self.vertices[vertex as usize], tolerance))
                .or_default()
                .push(vertex);
        }
        // Faces around each candidate, for the orientation check.
        let mut around: FastMap<u32, Vec<u32>> = FastMap::default();
        for &vertex in &candidates {
            around.insert(vertex, Vec::new());
        }
        for (index, t) in self.triangles.iter().enumerate() {
            for corner in t {
                if let Some(list) = around.get_mut(corner) {
                    list.push(index as u32);
                }
            }
        }
        // Greedy clustering around seeds, in index order: a vertex joins a
        // seed only if it — and everything already merged into it — lies
        // within `tolerance` of the seed, which never moves. Chains of
        // near neighbours therefore cannot drag a vertex further than that.
        let mut target: FastMap<u32, u32> = FastMap::default();
        let mut merged = 0;
        let mut polls = 0usize;
        let mut nearby: Vec<u32> = Vec::new();
        for &seed in &candidates {
            super::bsp::poll_interrupt(&mut polls, 256);
            if target.contains_key(&seed) {
                continue;
            }
            let point = self.vertices[seed as usize];
            let key = cell(point, tolerance);
            nearby.clear();
            for dx in -1..=1 {
                for dy in -1..=1 {
                    for dz in -1..=1 {
                        if let Some(list) = grid.get(&[key[0] + dx, key[1] + dy, key[2] + dz]) {
                            nearby.extend(list.iter().copied().filter(|other| *other > seed));
                        }
                    }
                }
            }
            nearby.sort_unstable();
            for &other in &nearby {
                if target.contains_key(&other) {
                    continue;
                }
                let distance = self.vertices[other as usize].sub(point).length();
                if distance + self.reach[other as usize] > tolerance {
                    continue;
                }
                // Within welding distance every consumer merges the pair
                // anyway; only a real move has to prove it flips nothing.
                if distance > super::mesh::WELD_EPSILON
                    && self.collapse_flips(other, seed, &around[&other], &target)
                {
                    continue;
                }
                target.insert(other, seed);
                let reach = distance + self.reach[other as usize];
                if reach > self.reach[seed as usize] {
                    self.reach[seed as usize] = reach;
                }
                merged += 1;
            }
        }
        if merged == 0 {
            return 0;
        }
        for triangle in &mut self.triangles {
            for corner in triangle.iter_mut() {
                if let Some(seed) = target.get(corner) {
                    *corner = *seed;
                }
            }
        }
        self.drop_degenerate();
        merged
    }

    /// Whether moving `vertex` onto `seed` (with the merges already decided
    /// in `target`) would turn any face around it inside out.
    fn collapse_flips(&self, vertex: u32, seed: u32, faces: &[u32], target: &FastMap<u32, u32>) -> bool {
        let resolve = |x: u32| target.get(&x).copied().unwrap_or(x);
        for face in faces {
            let t = self.triangles[*face as usize].map(resolve);
            let moved = t.map(|x| if x == vertex { seed } else { x });
            if moved[0] == moved[1] || moved[1] == moved[2] || moved[0] == moved[2] {
                continue;
            }
            let normal = |t: [u32; 3]| {
                let [a, b, c] = t.map(|x| self.vertices[x as usize]);
                b.sub(a).cross(c.sub(a))
            };
            if normal(t).dot(normal(moved)) <= 0.0 {
                return true;
            }
        }
        false
    }

    /// Removes triangles with a repeated corner, triangles present twice in
    /// the same orientation (keeping one), and pairs present in both
    /// orientations (a zero-volume fin, both removed).
    fn drop_degenerate(&mut self) -> usize {
        let before = self.triangles.len();
        let mut keep = vec![true; self.triangles.len()];
        // Copies of each triangle, keyed by its corner set, with orientation.
        let mut copies: FastMap<[u32; 3], Vec<(usize, bool)>> = FastMap::default();
        for (index, t) in self.triangles.iter().enumerate() {
            if t[0] == t[1] || t[1] == t[2] || t[0] == t[2] {
                keep[index] = false;
                continue;
            }
            // Canonical rotation (smallest index first) and orientation flag.
            let rotation = if t[0] < t[1] && t[0] < t[2] {
                *t
            } else if t[1] < t[2] {
                [t[1], t[2], t[0]]
            } else {
                [t[2], t[0], t[1]]
            };
            let (key, forward) = if rotation[1] < rotation[2] {
                (rotation, true)
            } else {
                ([rotation[0], rotation[2], rotation[1]], false)
            };
            copies.entry(key).or_default().push((index, forward));
        }
        let mut repeated: Vec<([u32; 3], Vec<(usize, bool)>)> =
            copies.into_iter().filter(|(_, list)| list.len() > 1).collect();
        if !repeated.is_empty() {
            repeated.sort_by_key(|(key, _)| *key);
            let mut edges = census(&self.triangles);
            for (key, list) in repeated {
                // A copy in each orientation is a zero-volume fin: cancel
                // such pairs outright.
                let (mut forward, mut backward): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
                for (index, orientation) in list {
                    if orientation { forward.push(index) } else { backward.push(index) }
                }
                let mut drop: Vec<(usize, bool)> = Vec::new();
                while !forward.is_empty() && !backward.is_empty() {
                    drop.push((forward.pop().unwrap(), true));
                    drop.push((backward.pop().unwrap(), false));
                }
                // Same-orientation copies are a doubled sheet. Keep one or
                // none, whichever leaves the triangle's edges balanced.
                let (left, orientation) = if forward.is_empty() { (backward, false) } else { (forward, true) };
                if left.len() > 1 {
                    let ring = if orientation { key } else { [key[0], key[2], key[1]] };
                    let score = |kept: u32| -> usize {
                        (0..3)
                            .filter(|corner| {
                                let (from, to) = (ring[*corner], ring[(corner + 1) % 3]);
                                let (lo, hi) = (from.min(to), from.max(to));
                                let mut counts = edges.get(&(lo, hi)).copied().unwrap_or_default();
                                let excess = left.len() as u32 - kept;
                                if from < to { counts.0 -= excess } else { counts.1 -= excess }
                                counts != (1, 1) && counts != (0, 0)
                            })
                            .count()
                    };
                    let kept = if score(0) < score(1) { 0 } else { 1 };
                    for index in &left[kept..] {
                        drop.push((*index, orientation));
                    }
                }
                for (index, _) in drop {
                    keep[index] = false;
                    let t = self.triangles[index];
                    for corner in 0..3 {
                        let (from, to) = (t[corner], t[(corner + 1) % 3]);
                        let entry = edges.entry((from.min(to), from.max(to))).or_default();
                        if from < to { entry.0 -= 1 } else { entry.1 -= 1 }
                    }
                }
            }
        }
        let mut cursor = 0;
        self.triangles.retain(|_| {
            cursor += 1;
            keep[cursor - 1]
        });
        let mut cursor = 0;
        self.tags.retain(|_| {
            cursor += 1;
            keep[cursor - 1]
        });
        before - self.triangles.len()
    }

    /// Step 3: splits boundary edges at boundary vertices within `tolerance`
    /// of their interior. Returns the number of edges split.
    fn split_t_junctions(&mut self, tolerance: f64) -> usize {
        let edges = census(&self.triangles);
        let mut open: Vec<(u32, u32)> = Vec::new();
        for (&(a, b), &counts) in &edges {
            match counts {
                (1, 0) => open.push((a, b)),
                (0, 1) => open.push((b, a)),
                _ => {}
            }
        }
        if open.is_empty() {
            return 0;
        }
        open.sort_unstable();
        let mut boundary: Vec<u32> = open.iter().flat_map(|(a, b)| [*a, *b]).collect();
        boundary.sort_unstable();
        boundary.dedup();
        // A uniform grid sized to the typical open edge: walking an edge in
        // steps of one cell and looking at the 27 cells around each step
        // covers its whole `tolerance` tube, and the cells hold a bounded
        // number of vertices however the boundary is laid out.
        let mut lengths: Vec<f64> = open
            .iter()
            .map(|(a, b)| self.vertices[*a as usize].sub(self.vertices[*b as usize]).length())
            .collect();
        lengths.sort_by(f64::total_cmp);
        let size = lengths[lengths.len() / 2].max(tolerance * 4.0);
        let mut grid: FastMap<[i64; 3], Vec<u32>> = FastMap::default();
        for &vertex in &boundary {
            grid.entry(cell(self.vertices[vertex as usize], size)).or_default().push(vertex);
        }
        let mut inserts: FastMap<(u32, u32), Vec<(f64, u32)>> = FastMap::default();
        let mut polls = 0usize;
        let mut seen: Vec<u32> = Vec::new();
        for &(a, b) in &open {
            super::bsp::poll_interrupt(&mut polls, 64);
            let (pa, pb) = (self.vertices[a as usize], self.vertices[b as usize]);
            let direction = pb.sub(pa);
            let length_squared = direction.dot(direction);
            if length_squared <= 0.0 {
                continue;
            }
            // Edges far longer than the cells are not pinhole seams; skip
            // rather than walk thousands of cells.
            let steps = (length_squared.sqrt() / size).ceil() as usize;
            if steps > 4096 {
                continue;
            }
            seen.clear();
            for step in 0..=steps {
                let key = cell(pa.add(direction.mul(step as f64 / steps.max(1) as f64)), size);
                for dx in -1..=1 {
                    for dy in -1..=1 {
                        for dz in -1..=1 {
                            if let Some(list) = grid.get(&[key[0] + dx, key[1] + dy, key[2] + dz]) {
                                seen.extend_from_slice(list);
                            }
                        }
                    }
                }
            }
            seen.sort_unstable();
            seen.dedup();
            let mut found: Vec<(f64, u32)> = Vec::new();
            for &c in &seen {
                if c == a || c == b {
                    continue;
                }
                let pc = self.vertices[c as usize];
                let t = pc.sub(pa).dot(direction) / length_squared;
                if t <= 0.0 || t >= 1.0 {
                    continue;
                }
                if pa.add(direction.mul(t)).sub(pc).length() <= tolerance {
                    found.push((t, c));
                }
            }
            if !found.is_empty() {
                found.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
                inserts.insert((a, b), found);
            }
        }
        if inserts.is_empty() {
            return 0;
        }
        let mut split = 0;
        let mut triangles = Vec::with_capacity(self.triangles.len() + inserts.len() * 2);
        let mut tags = Vec::with_capacity(triangles.capacity());
        for (t, tag) in self.triangles.iter().zip(&self.tags) {
            let mut ring: Vec<u32> = Vec::with_capacity(6);
            let mut edges_split = 0;
            for index in 0..3 {
                let (from, to) = (t[index], t[(index + 1) % 3]);
                ring.push(from);
                if let Some(points) = inserts.get(&(from, to)) {
                    edges_split += 1;
                    ring.extend(points.iter().map(|(_, c)| *c));
                }
            }
            if edges_split == 0 {
                triangles.push(*t);
                tags.push(*tag);
                continue;
            }
            // Conforming re-triangulation of the whole ring, every piece
            // checked to keep the face's orientation and to have area; when
            // that fails the face is left as it was.
            let [p, q, r] = t.map(|x| self.vertices[x as usize]);
            let normal = q.sub(p).cross(r.sub(p));
            let pieces = ear_clip(&mut ring.clone(), &self.vertices, normal);
            let valid = pieces.len() == ring.len() - 2
                && pieces.iter().all(|piece| {
                    let [a, b, c] = piece.map(|x| self.vertices[x as usize]);
                    b.sub(a).cross(c.sub(a)).dot(normal) > 0.0
                });
            if valid {
                split += edges_split;
                for piece in pieces {
                    triangles.push(piece);
                    tags.push(*tag);
                }
            } else {
                triangles.push(*t);
                tags.push(*tag);
            }
        }
        self.triangles = triangles;
        self.tags = tags;
        self.drop_degenerate();
        split
    }

    /// Step 4: fills boundary loops that are pinholes or slivers.
    fn fill_small_holes(&mut self, tolerance: f64) -> usize {
        let edges = census(&self.triangles);
        let mut next: FastMap<u32, Vec<u32>> = FastMap::default();
        for (&(a, b), &counts) in &edges {
            match counts {
                (1, 0) => next.entry(a).or_default().push(b),
                (0, 1) => next.entry(b).or_default().push(a),
                _ => {}
            }
        }
        if next.is_empty() {
            return 0;
        }
        // Tag for filler triangles: any triangle touching the loop.
        let mut owner: FastMap<(u32, u32), u32> = FastMap::default();
        for (t, tag) in self.triangles.iter().zip(&self.tags) {
            for index in 0..3 {
                owner.insert((t[index], t[(index + 1) % 3]), *tag);
            }
        }
        let mut starts: Vec<u32> = next.keys().copied().collect();
        starts.sort_unstable();
        let mut used: FastMap<u32, bool> = FastMap::default();
        let mut filled = 0;
        for start in starts {
            if used.contains_key(&start) {
                continue;
            }
            // Follow a simple loop; give up on branching vertices.
            let mut loop_vertices = vec![start];
            let mut current = start;
            let mut closed = false;
            while loop_vertices.len() <= MAX_FILL_EDGES {
                let Some(outgoing) = next.get(&current) else { break };
                if outgoing.len() != 1 {
                    break;
                }
                let following = outgoing[0];
                if following == start {
                    closed = true;
                    break;
                }
                if loop_vertices.contains(&following) {
                    break;
                }
                loop_vertices.push(following);
                current = following;
            }
            for vertex in &loop_vertices {
                used.insert(*vertex, true);
            }
            if !closed || loop_vertices.len() < 3 {
                continue;
            }
            let points: Vec<Vec3> = loop_vertices.iter().map(|v| self.vertices[*v as usize]).collect();
            let count = points.len();
            let perimeter: f64 = (0..count).map(|i| points[(i + 1) % count].sub(points[i]).length()).sum();
            let mut area_vector = Vec3::new(0.0, 0.0, 0.0);
            for i in 0..count {
                area_vector = area_vector.add(points[i].cross(points[(i + 1) % count]));
            }
            let area = area_vector.length() * 0.5;
            let small = perimeter <= 20.0 * tolerance || area <= perimeter * tolerance * 0.5;
            if !small {
                continue;
            }
            // The hole is bounded by edges a->b of existing faces, so the
            // filler runs the other way.
            let tag = owner
                .get(&(loop_vertices[0], loop_vertices[1]))
                .copied()
                .unwrap_or(0);
            let mut ring: Vec<u32> = loop_vertices.iter().rev().copied().collect();
            let normal = area_vector.mul(-1.0);
            for triangle in ear_clip(&mut ring, &self.vertices, normal) {
                self.triangles.push(triangle);
                self.tags.push(tag);
                filled += 1;
            }
        }
        filled
    }
}

impl Work {
    /// Step 6: drops connected pieces that are films: two sheets of surface,
    /// facing each other, never further apart than `tolerance` — what is left
    /// of two nearly coincident surfaces subtracted from each other, which no
    /// printer can make and no boolean can close. The evidence is geometric:
    /// every face of the piece is a sliver narrower than `tolerance`, or every
    /// sample of it (corners and centroid) lies within `tolerance` of an
    /// opposite-facing face of the same piece. Only pieces with a defective
    /// edge or a vertex in `touched` are considered, and never all of them.
    /// Returns the number of triangles removed.
    fn drop_films(&mut self, tolerance: f64, touched: &[u32]) -> usize {
        let mut touched: Vec<u32> = touched.to_vec();
        touched.sort_unstable();
        let edges = census(&self.triangles);
        let count = self.triangles.len();
        let mut uf = UnionFind((0..count as u32).collect());
        let mut first: FastMap<(u32, u32), u32> = FastMap::default();
        for (index, t) in self.triangles.iter().enumerate() {
            for corner in 0..3 {
                let (from, to) = (t[corner], t[(corner + 1) % 3]);
                let key = (from.min(to), from.max(to));
                match first.get(&key) {
                    Some(other) => {
                        uf.union(index as u32, *other);
                    }
                    None => {
                        first.insert(key, index as u32);
                    }
                }
            }
        }
        let mut pieces: FastMap<u32, (Vec<u32>, bool)> = FastMap::default();
        for (index, t) in self.triangles.iter().enumerate() {
            let root = uf.find(index as u32);
            let entry = pieces.entry(root).or_default();
            entry.0.push(index as u32);
            for corner in 0..3 {
                let (from, to) = (t[corner], t[(corner + 1) % 3]);
                if defective(edges[&(from.min(to), from.max(to))]) || touched.binary_search(&t[corner]).is_ok() {
                    entry.1 = true;
                }
            }
        }
        let mut roots: Vec<u32> = pieces.keys().copied().collect();
        roots.sort_unstable();
        let mut drop = vec![false; count];
        let mut dropped_pieces = 0;
        let mut polls = 0usize;
        for root in &roots {
            let (faces, suspect) = &pieces[root];
            // Brute force is quadratic in the piece; pinhole films are small.
            if !*suspect || faces.len() > 2048 {
                continue;
            }
            let points = |face: u32| self.triangles[face as usize].map(|x| self.vertices[x as usize]);
            let film = faces.iter().all(|&face| {
                super::bsp::poll_interrupt(&mut polls, 64);
                let [a, b, c] = points(face);
                if super::cleanup::min_altitude(a, b, c) <= tolerance {
                    return true;
                }
                let normal = b.sub(a).cross(c.sub(a));
                let samples = [a, b, c, a.add(b).add(c).mul(1.0 / 3.0)];
                samples.iter().all(|sample| {
                    faces.iter().any(|&other| {
                        let [p, q, r] = points(other);
                        let facing = q.sub(p).cross(r.sub(p));
                        if facing.dot(normal) >= 0.0 {
                            return false;
                        }
                        let near = crate::analysis::closest_point_on_triangle(
                            [sample.x, sample.y, sample.z],
                            [p.x, p.y, p.z],
                            [q.x, q.y, q.z],
                            [r.x, r.y, r.z],
                        );
                        Vec3::new(near[0], near[1], near[2]).sub(*sample).length() <= tolerance
                    })
                })
            });
            if film {
                dropped_pieces += 1;
                for face in faces {
                    drop[*face as usize] = true;
                }
            }
        }
        if dropped_pieces == 0 || dropped_pieces == roots.len() {
            return 0;
        }
        let removed = drop.iter().filter(|d| **d).count();
        let mut cursor = 0;
        self.triangles.retain(|_| {
            cursor += 1;
            !drop[cursor - 1]
        });
        let mut cursor = 0;
        self.tags.retain(|_| {
            cursor += 1;
            !drop[cursor - 1]
        });
        removed
    }
}

impl Work {
    /// Step 5: separates solids that touch along an edge.
    ///
    /// Where two pieces of material meet along a line (two cubes edge to
    /// edge, a thread root exactly on a cone's rim) the exact answer has an
    /// edge shared by four faces. No amount of welding fixes that: it is the
    /// geometry. Each such edge is resolved by pairing its faces around the
    /// edge so every pair bounds one wedge of material, giving every wedge
    /// its own copy of the shared vertices, and moving each copy `epsilon`
    /// into its own wedge. The pieces end up `epsilon`-ish apart instead of
    /// touching, which is what a slicer would make of it anyway. Returns the
    /// number of vertices split.
    fn separate_pinches(&mut self) -> Vec<Vec<u32>> {
        let mut by_edge: FastMap<(u32, u32), Vec<(u32, u8, bool)>> = FastMap::default();
        for (index, t) in self.triangles.iter().enumerate() {
            for corner in 0..3 {
                let (from, to) = (t[corner], t[(corner + 1) % 3]);
                by_edge
                    .entry((from.min(to), from.max(to)))
                    .or_default()
                    .push((index as u32, corner as u8, from < to));
            }
        }
        let count = self.triangles.len();
        // Neighbour across each triangle edge, `u32::MAX` when unpaired.
        let mut link: Vec<[u32; 3]> = vec![[u32::MAX; 3]; count];
        let mut pinched = false;
        for (&(a, b), faces) in &by_edge {
            let forward = faces.iter().filter(|face| face.2).count();
            if forward * 2 != faces.len() {
                continue;
            }
            if faces.len() == 2 {
                let (f, g) = (faces[0], faces[1]);
                link[f.0 as usize][f.1 as usize] = g.0;
                link[g.0 as usize][g.1 as usize] = f.0;
                continue;
            }
            // Order the faces around the edge axis.
            let (pa, pb) = (self.vertices[a as usize], self.vertices[b as usize]);
            let axis = pb.sub(pa);
            let length_squared = axis.dot(axis);
            if length_squared <= 0.0 {
                continue;
            }
            let unit = axis.mul(1.0 / length_squared.sqrt());
            let helper = if unit.x.abs() < 0.9 { Vec3::new(1.0, 0.0, 0.0) } else { Vec3::new(0.0, 1.0, 0.0) };
            let u = unit.cross(helper).normalized();
            let v = unit.cross(u);
            let mut around: Vec<(f64, (u32, u8, bool))> = faces
                .iter()
                .map(|face| {
                    let t = self.triangles[face.0 as usize];
                    let apex = t[(face.1 as usize + 2) % 3];
                    let offset = self.vertices[apex as usize].sub(pa);
                    let w = offset.sub(axis.mul(offset.dot(axis) / length_squared));
                    (w.dot(v).atan2(w.dot(u)), *face)
                })
                .collect();
            around.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1 .0.cmp(&y.1 .0)));
            // Coincident faces (a film thinner than welding) sit at one
            // angle; order each such run backward-first so a forward face is
            // paired with the coincident backward one, making the film its
            // own zero-volume piece that `drop_films` can remove.
            let mut run = 0;
            while run < around.len() {
                let mut end = run + 1;
                while end < around.len() && around[end].0 - around[run].0 <= 1.0e-9 {
                    end += 1;
                }
                around[run..end].sort_by_key(|entry| (entry.1 .2, entry.1 .0));
                run = end;
            }
            // A face running a->b has its material on the decreasing-angle
            // side, so its partner is the previous face around the edge,
            // which must run b->a for the pairing to be consistent.
            let n = around.len();
            let consistent = (0..n).all(|i| around[i].1 .2 != around[(i + 1) % n].1 .2);
            if !consistent {
                continue;
            }
            for i in 0..n {
                let face = around[i].1;
                if face.2 {
                    let partner = around[(i + n - 1) % n].1;
                    link[face.0 as usize][face.1 as usize] = partner.0;
                    link[partner.0 as usize][partner.1 as usize] = face.0;
                }
            }
            pinched = true;
        }
        if !pinched {
            return Vec::new();
        }
        // Umbrellas: corners of one vertex connected through linked edges.
        let mut uf = UnionFind((0..(count * 3) as u32).collect());
        for (index, t) in self.triangles.iter().enumerate() {
            for corner in 0..3 {
                let vertex = t[corner];
                // The two edges of this triangle at `vertex`.
                for edge in [corner, (corner + 2) % 3] {
                    let neighbour = link[index][edge];
                    if neighbour == u32::MAX {
                        continue;
                    }
                    let other = self.triangles[neighbour as usize];
                    if let Some(position) = other.iter().position(|x| *x == vertex) {
                        uf.union((index * 3 + corner) as u32, neighbour * 3 + position as u32);
                    }
                }
            }
        }
        // Group corners per vertex by umbrella.
        let mut umbrellas: FastMap<u32, Vec<u32>> = FastMap::default();
        let mut roots: FastMap<u32, Vec<u32>> = FastMap::default();
        for (index, t) in self.triangles.iter().enumerate() {
            for corner in 0..3 {
                let id = (index * 3 + corner) as u32;
                let root = uf.find(id);
                let list = roots.entry(root).or_default();
                if list.is_empty() {
                    umbrellas.entry(t[corner]).or_default().push(root);
                }
                list.push(id);
            }
        }
        let mut split_vertices: Vec<u32> = umbrellas
            .iter()
            .filter(|(_, list)| list.len() > 1)
            .map(|(vertex, _)| *vertex)
            .collect();
        split_vertices.sort_unstable();
        let mut copies = Vec::with_capacity(split_vertices.len());
        for vertex in split_vertices {
            let mut group = Vec::new();
            for (rank, root) in umbrellas[&vertex].iter().enumerate() {
                let target = if rank == 0 {
                    vertex
                } else {
                    self.vertices.push(self.vertices[vertex as usize]);
                    self.reach.push(self.reach[vertex as usize]);
                    (self.vertices.len() - 1) as u32
                };
                for corner in &roots[root] {
                    self.triangles[(*corner / 3) as usize][(*corner % 3) as usize] = target;
                }
                group.push(target);
            }
            copies.push(group);
        }
        copies
    }

    /// Moves every copy of a split vertex that is still in use `epsilon`
    /// into its own wedge of material, so the pieces that touched no longer
    /// share a position. Returns the number of vertices moved.
    fn nudge_copies(&mut self, copies: &[Vec<u32>], epsilon: f64) -> usize {
        let mut normals: FastMap<u32, Vec3> = FastMap::default();
        let mut wanted: Vec<u32> = copies.iter().flatten().copied().collect();
        wanted.sort_unstable();
        for t in &self.triangles {
            let [p, q, r] = t.map(|x| self.vertices[x as usize]);
            let normal = q.sub(p).cross(r.sub(p));
            for corner in t {
                if wanted.binary_search(corner).is_ok() {
                    let entry = normals.entry(*corner).or_insert(Vec3::new(0.0, 0.0, 0.0));
                    *entry = entry.add(normal);
                }
            }
        }
        let mut moved = 0;
        for group in copies {
            let live: Vec<u32> = group.iter().copied().filter(|v| normals.contains_key(v)).collect();
            if live.len() < 2 {
                continue;
            }
            for vertex in live {
                let normal = normals[&vertex];
                let length = normal.length();
                if length > 0.0 && self.reach[vertex as usize] + epsilon <= CEILING {
                    let origin = self.vertices[vertex as usize];
                    self.vertices[vertex as usize] = origin.sub(normal.mul(epsilon / length));
                    moved += 1;
                }
            }
        }
        moved
    }
}

/// Ear clipping of a 3D ring around `normal`; falls back to a fan when no ear
/// is found (a degenerate sliver, where any triangulation is as good).
fn ear_clip(ring: &mut Vec<u32>, vertices: &[Vec3], normal: Vec3) -> Vec<[u32; 3]> {
    let mut out = Vec::new();
    let point = |v: u32| vertices[v as usize];
    while ring.len() > 3 {
        let count = ring.len();
        let mut clipped = false;
        for index in 0..count {
            let (a, b, c) = (ring[(index + count - 1) % count], ring[index], ring[(index + 1) % count]);
            let (pa, pb, pc) = (point(a), point(b), point(c));
            let turn = pb.sub(pa).cross(pc.sub(pb)).dot(normal);
            if turn <= 0.0 {
                continue;
            }
            let inside = ring.iter().any(|&other| {
                if other == a || other == b || other == c {
                    return false;
                }
                let p = point(other);
                let s1 = pb.sub(pa).cross(p.sub(pa)).dot(normal);
                let s2 = pc.sub(pb).cross(p.sub(pb)).dot(normal);
                let s3 = pa.sub(pc).cross(p.sub(pc)).dot(normal);
                s1 >= 0.0 && s2 >= 0.0 && s3 >= 0.0
            });
            if inside {
                continue;
            }
            out.push([a, b, c]);
            ring.remove(index);
            clipped = true;
            break;
        }
        if !clipped {
            let first = ring[0];
            for window in ring[1..].windows(2) {
                out.push([first, window[0], window[1]]);
            }
            ring.clear();
            return out;
        }
    }
    if ring.len() == 3 {
        out.push([ring[0], ring[1], ring[2]]);
    }
    out
}

/// Repairs `mesh` in place when it has defective edges. `tags` (one per
/// triangle, may be empty) follow the triangles they describe. Returns `None`
/// when the mesh was already closed and manifold.
pub fn heal(mesh: &mut IndexedMesh, tags: &mut Vec<u32>) -> Option<HealReport> {
    super::prof::scope("heal:total", || heal_inner(mesh, tags))
}

fn heal_inner(mesh: &mut IndexedMesh, tags: &mut Vec<u32>) -> Option<HealReport> {
    let before = super::prof::scope("heal:census", || defect_count(&mesh.triangles));
    if before == 0 {
        return None;
    }
    if before > MAX_DEFECTS {
        // Not a pinhole but a broken surface: report it, do not try.
        return Some(HealReport {
            defects_before: before,
            defects_after: before,
            sites: defect_sites(&mesh.vertices, &mesh.triangles),
            ..HealReport::default()
        });
    }
    let sites = defect_sites(&mesh.vertices, &mesh.triangles);
    let extent = mesh
        .bounds()
        .map(|bounds| {
            let size = bounds.size();
            size.x.max(size.y).max(size.z)
        })
        .unwrap_or(0.0);
    // Relative to the model, never beyond the largest step in absolute
    // terms, and never below welding.
    let cap = (extent * RELATIVE_CAP).clamp(super::mesh::WELD_EPSILON, CEILING);
    // Coordinates are exported as f32: a separation has to outlast rounding
    // at this magnitude, and the weld every consumer applies after it.
    let magnitude = mesh
        .bounds()
        .map(|bounds| {
            [bounds.min.x, bounds.min.y, bounds.min.z, bounds.max.x, bounds.max.y, bounds.max.z]
                .iter()
                .fold(0.0f64, |m, v| m.max(v.abs()))
        })
        .unwrap_or(0.0);
    let f32_ulp = magnitude * f64::from(f32::EPSILON);
    let mut work = Work {
        reach: vec![0.0; mesh.vertices.len()],
        vertices: mesh.vertices.clone(),
        triangles: mesh.triangles.clone(),
        tags: if tags.len() == mesh.triangles.len() {
            tags.clone()
        } else {
            vec![0; mesh.triangles.len()]
        },
    };
    let mut report = HealReport {
        defects_before: before,
        defects_after: before,
        sites,
        ..HealReport::default()
    };
    let start = work.triangles.len();
    work.drop_degenerate();
    // A pass can leave distinct vertices within welding distance that the
    // consumers' weld then joins differently; re-weld and go again once.
    for pass in 0..=PASSES {
        report.defects_after = work.reweld();
        if report.defects_after == 0 || pass == PASSES {
            break;
        }
        for step in STEPS {
            let tolerance = step.min(cap);
            report.tolerance = report.tolerance.max(tolerance);
            report.merged_vertices += work.merge(tolerance);
            for _ in 0..4 {
                let split = work.split_t_junctions(tolerance);
                report.split_edges += split;
                if split == 0 {
                    break;
                }
            }
            if defect_count(&work.triangles) > 0 {
                report.filled_triangles += work.fill_small_holes(tolerance);
                work.drop_degenerate();
            }
            report.defects_after = defect_count(&work.triangles);
            if report.defects_after == 0 || tolerance >= cap {
                break;
            }
        }
        if report.defects_after > 0 {
            // What is left may be genuine edge contact between two solids, or
            // a film thinner than welding: split the shared edges, drop pieces
            // with no thickness, and pull apart what still touches.
            let copies = work.separate_pinches();
            let touched: Vec<u32> = copies.iter().flatten().copied().collect();
            report.film_triangles += work.drop_films(cap, &touched);
            let epsilon = 1.0e-4f64.max(4.0 * super::mesh::WELD_EPSILON + 4.0 * f32_ulp);
            // Past the ceiling the nudge would be visible: leave it reported.
            if epsilon <= CEILING {
                let moved = work.nudge_copies(&copies, epsilon);
                if moved > 0 {
                    report.separated_vertices += moved;
                    report.nudge = epsilon;
                }
            }
        }
    }
    // The result is judged the way every consumer will see it: as re-welded
    // above, and also rounded to f32 as an STL stores it. A repair too small
    // to survive that rounding would be undone there.
    if report.defects_after == 0 {
        let single = super::cleanup::weld_triangles(work.triangles.iter().map(|t| {
            t.map(|x| {
                let p = work.vertices[x as usize];
                Vec3::new(p.x as f32 as f64, p.y as f32 as f64, p.z as f32 as f64)
            })
        }));
        report.defects_after = defect_count(&single.triangles);
    }
    report.removed_triangles = (start + report.filled_triangles)
        .saturating_sub(work.triangles.len() + report.film_triangles);
    if report.defects_after >= before {
        report.defects_after = before;
        return Some(report);
    }
    // Compact away vertices nothing references any more.
    let mut remap = vec![u32::MAX; work.vertices.len()];
    let mut vertices = Vec::new();
    for triangle in &mut work.triangles {
        for corner in triangle.iter_mut() {
            let slot = &mut remap[*corner as usize];
            if *slot == u32::MAX {
                *slot = vertices.len() as u32;
                vertices.push(work.vertices[*corner as usize]);
            }
            *corner = *slot;
        }
    }
    mesh.vertices = vertices;
    mesh.triangles = work.triangles;
    *tags = work.tags;
    Some(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cube() -> IndexedMesh {
        let vertices = (0..8)
            .map(|i| Vec3::new((i & 1) as f64, ((i >> 1) & 1) as f64, ((i >> 2) & 1) as f64))
            .collect();
        let triangles = vec![
            [0, 2, 3], [0, 3, 1], [4, 5, 7], [4, 7, 6], [0, 1, 5], [0, 5, 4],
            [2, 6, 7], [2, 7, 3], [0, 4, 6], [0, 6, 2], [1, 3, 7], [1, 7, 5],
        ];
        IndexedMesh { vertices, triangles }
    }

    #[test]
    fn a_closed_mesh_is_left_alone() {
        let mut mesh = cube();
        assert_eq!(defect_count(&mesh.triangles), 0);
        assert!(heal(&mut mesh, &mut Vec::new()).is_none());
    }

    #[test]
    fn a_split_corner_is_merged_back() {
        // Corner 7 of one face lands 6e-5 away from the others' copy: beyond
        // welding, well inside the repair's reach.
        let mut mesh = cube();
        mesh.vertices.push(Vec3::new(1.0 + 6.0e-5, 1.0, 1.0));
        mesh.triangles[2] = [4, 5, 8];
        assert!(defect_count(&mesh.triangles) > 0);
        let report = heal(&mut mesh, &mut Vec::new()).expect("defective");
        assert!(report.healed(), "{report:?}");
        assert_eq!(defect_count(&mesh.triangles), 0);
        assert!((mesh.volume() - 1.0).abs() < 1e-3);
        assert!(report.message().contains("auto-healed"));
    }

    #[test]
    fn a_pinhole_is_filled() {
        // Remove one triangle and replace it with a sliver-sized gap: the
        // missing face is a hole of width 1e-5 between two triangles.
        let mut mesh = cube();
        let a = mesh.vertices.len() as u32;
        mesh.vertices.push(Vec3::new(0.5, 0.0, 0.5 + 1.0e-5));
        // Face y=0 is [0,1,5],[0,5,4]; split diagonal 0-5 with a vertex just
        // off it so one side has a T-junction and a sliver hole.
        mesh.triangles[4] = [0, 1, 5];
        mesh.triangles[5] = [0, a, 4];
        mesh.triangles.push([a, 5, 4]);
        let mut tags = vec![0; mesh.triangles.len()];
        let report = heal(&mut mesh, &mut tags).expect("defective");
        assert!(report.healed(), "{report:?}");
        assert_eq!(tags.len(), mesh.triangles.len());
        assert!((mesh.volume() - 1.0).abs() < 1e-3);
    }

    #[test]
    fn a_large_hole_is_not_papered_over() {
        let mut mesh = cube();
        mesh.triangles.truncate(10);
        let report = heal(&mut mesh, &mut Vec::new()).expect("defective");
        assert!(!report.healed());
        assert_eq!(mesh.triangles.len(), 10, "an unimprovable mesh is returned untouched");
    }

    fn boxed(min: Vec3, max: Vec3, base: u32) -> (Vec<Vec3>, Vec<[u32; 3]>) {
        let vertices = (0..8)
            .map(|i| {
                Vec3::new(
                    if i & 1 == 0 { min.x } else { max.x },
                    if i & 2 == 0 { min.y } else { max.y },
                    if i & 4 == 0 { min.z } else { max.z },
                )
            })
            .collect();
        let triangles = cube().triangles.iter().map(|t| t.map(|x| x + base)).collect();
        (vertices, triangles)
    }

    fn f32_defects(mesh: &IndexedMesh) -> usize {
        let welded = crate::csg::cleanup::weld_triangles((0..mesh.triangles.len()).map(|i| {
            mesh.triangle_points(i)
                .map(|p| Vec3::new(p.x as f32 as f64, p.y as f32 as f64, p.z as f32 as f64))
        }));
        defect_count(&welded.triangles)
    }

    /// An open surface is not a film just because its signed volume about
    /// the origin happens to be small.
    #[test]
    fn an_open_box_is_never_dropped_as_a_film_wherever_it_sits() {
        for z in [0.0, 2.0, 3.0] {
            let (mut vertices, mut triangles) = boxed(Vec3::new(0.0, 0.0, z), Vec3::new(1.0, 1.0, z + 1.0), 0);
            triangles.retain(|t| !(t.iter().all(|x| vertices[*x as usize].z == z + 1.0)));
            assert_eq!(triangles.len(), 10);
            let (v2, t2) = boxed(Vec3::new(5.0, 0.0, 0.0), Vec3::new(6.0, 1.0, 1.0), 8);
            vertices.extend(v2);
            triangles.extend(t2);
            let mut mesh = IndexedMesh { vertices, triangles };
            heal(&mut mesh, &mut Vec::new());
            assert_eq!(mesh.triangles.len(), 22, "z={z}: open box faces were deleted");
        }
    }

    /// A thin plate touching a big block along an edge is real material.
    #[test]
    fn a_thin_plate_touching_a_large_block_is_kept() {
        let (mut vertices, mut triangles) = boxed(Vec3::new(0.0, 0.0, 0.0), Vec3::new(1000.0, 10.0, 1.0), 0);
        let (v2, t2) = boxed(Vec3::new(1000.0, 0.0, 1.0), Vec3::new(1010.0, 10.0, 1.02), 8);
        vertices.extend(v2);
        triangles.extend(t2);
        // Share the touching edge's vertices, as a weld would.
        let mut mesh = IndexedMesh { vertices, triangles };
        let welded = crate::csg::cleanup::weld_triangles((0..mesh.triangles.len()).map(|i| mesh.triangle_points(i)));
        mesh = welded;
        assert!(defect_count(&mesh.triangles) > 0, "the shared edge is non-manifold");
        let before = mesh.vertices.clone();
        let report = heal(&mut mesh, &mut Vec::new()).expect("defective");
        assert_eq!(mesh.triangles.len(), 24, "{report:?}");
        for vertex in &mesh.vertices {
            let moved = before.iter().map(|b| b.sub(*vertex).length()).fold(f64::MAX, f64::min);
            assert!(moved <= CEILING, "a vertex moved {moved}");
        }
    }

    /// Merging never moves a vertex further than the tolerance in force,
    /// however the candidates chain together.
    #[test]
    fn a_chain_of_close_vertices_does_not_collapse_transitively() {
        let tolerance = 1.0e-4;
        let mut vertices: Vec<Vec3> = (0..100).map(|i| Vec3::new(i as f64 * 9.0e-5, 0.0, 0.0)).collect();
        vertices.push(Vec3::new(0.0, 1.0, 0.0));
        let apex = 100u32;
        let triangles: Vec<[u32; 3]> = (0..99u32).map(|i| [i, i + 1, apex]).collect();
        let original: Vec<[Vec3; 3]> = triangles.iter().map(|t| t.map(|x| vertices[x as usize])).collect();
        let mut work = Work {
            reach: vec![0.0; vertices.len()],
            vertices,
            tags: (0..triangles.len() as u32).collect(),
            triangles,
        };
        work.merge(tolerance);
        assert!(work.triangles.len() >= 40, "the chain collapsed: {} faces left", work.triangles.len());
        for (t, tag) in work.triangles.iter().zip(&work.tags) {
            for corner in 0..3 {
                let moved = work.vertices[t[corner] as usize].sub(original[*tag as usize][corner]).length();
                assert!(moved <= tolerance * (1.0 + 1e-9), "a corner moved {moved}");
            }
        }
    }

    /// A merge that would turn a thin face inside out is refused.
    #[test]
    fn a_merge_never_flips_a_face() {
        let vertices = vec![
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(0.5, -4.0e-5, 0.0),
            Vec3::new(0.5, 5.0e-5, 0.0),
            Vec3::new(0.5, -1.0, 0.0),
        ];
        // Open faces; vertices 2 and 3 are both on the boundary, and moving
        // 3 onto 2 would turn face 0 inside out.
        let triangles = vec![[0, 1, 3], [1, 0, 4], [2, 1, 4]];
        let normals: Vec<Vec3> = triangles
            .iter()
            .map(|t: &[u32; 3]| {
                let [a, b, c] = t.map(|x| vertices[x as usize]);
                b.sub(a).cross(c.sub(a))
            })
            .collect();
        let mut work = Work {
            reach: vec![0.0; vertices.len()],
            vertices,
            tags: (0..triangles.len() as u32).collect(),
            triangles,
        };
        work.merge(1.0e-4);
        for (t, tag) in work.triangles.iter().zip(&work.tags) {
            let [a, b, c] = t.map(|x| work.vertices[x as usize]);
            assert!(b.sub(a).cross(c.sub(a)).dot(normals[*tag as usize]) > 0.0, "face {tag} flipped");
        }
    }

    /// A triangle with T-junctions on two edges is re-triangulated
    /// conformingly: same orientation, no zero-area pieces, same area.
    #[test]
    fn t_junctions_on_two_edges_split_conformingly() {
        let vertices = vec![
            Vec3::new(0.0, 0.0, 0.0),  // 0 A
            Vec3::new(1.0, 0.0, 0.0),  // 1 B
            Vec3::new(0.0, 1.0, 0.0),  // 2 C
            Vec3::new(0.5, 0.0, 0.0),  // 3 P on AB
            Vec3::new(0.5, 0.5, 0.0),  // 4 Q on BC
            Vec3::new(0.5, -1.0, 0.0), // 5 D
            Vec3::new(1.0, 1.0, 0.0),  // 6 E
        ];
        let triangles = vec![[0, 1, 2], [3, 0, 5], [1, 3, 5], [4, 1, 6], [2, 4, 6]];
        let mut work = Work {
            reach: vec![0.0; vertices.len()],
            vertices,
            tags: (0..triangles.len() as u32).collect(),
            triangles,
        };
        assert!(work.split_t_junctions(1.0e-6) > 0);
        let mut area = 0.0;
        for (t, tag) in work.triangles.iter().zip(&work.tags) {
            if *tag != 0 {
                continue;
            }
            let [a, b, c] = t.map(|x| work.vertices[x as usize]);
            let normal = b.sub(a).cross(c.sub(a));
            assert!(normal.z > 0.0, "piece {t:?} is flat or reversed");
            area += normal.z * 0.5;
        }
        assert!((area - 0.5).abs() < 1e-12, "area {area}");
    }

    /// A separation nudge has to survive an f32 STL export: far from the
    /// origin it cannot, and then the defect is reported, not hidden.
    #[test]
    fn a_pinch_far_from_the_origin_is_validated_in_f32() {
        for offset in [0.0, 10_000.0] {
            let o = Vec3::new(offset, offset, offset);
            let (mut vertices, mut triangles) = boxed(o, o.add(Vec3::new(1.0, 1.0, 1.0)), 0);
            let (v2, t2) = boxed(o.add(Vec3::new(1.0, 1.0, 0.0)), o.add(Vec3::new(2.0, 2.0, 1.0)), 8);
            vertices.extend(v2);
            triangles.extend(t2);
            let raw = IndexedMesh { vertices, triangles };
            let mut mesh = crate::csg::cleanup::weld_triangles((0..raw.triangles.len()).map(|i| raw.triangle_points(i)));
            assert!(defect_count(&mesh.triangles) > 0);
            let report = heal(&mut mesh, &mut Vec::new()).expect("defective");
            if report.healed() {
                assert_eq!(f32_defects(&mesh), 0, "offset {offset}: healed in f64 only");
            } else {
                assert!(offset > 0.0, "a pinch near the origin should heal: {report:?}");
            }
        }
    }

    /// Thousands of boundary edges sharing one X must not make the T-junction
    /// search quadratic.
    #[test]
    fn many_boundary_edges_in_one_slab_stay_fast() {
        let mut vertices = Vec::new();
        let mut triangles = Vec::new();
        for i in 0..20_000u32 {
            let y = i as f64;
            vertices.push(Vec3::new(0.0, y, 0.0));
            vertices.push(Vec3::new(0.0, y + 0.5, 1.0));
            vertices.push(Vec3::new(0.0, y + 0.9, 0.0));
            triangles.push([3 * i, 3 * i + 1, 3 * i + 2]);
        }
        let mut work = Work {
            reach: vec![0.0; vertices.len()],
            vertices,
            tags: vec![0; triangles.len()],
            triangles,
        };
        let start = std::time::Instant::now();
        work.split_t_junctions(1.0e-4);
        work.merge(1.0e-4);
        assert!(start.elapsed().as_secs_f64() < 3.0, "{:?}", start.elapsed());
    }

    /// Per-triangle tags follow the triangles through every step.
    #[test]
    fn tags_follow_their_triangles() {
        let mut mesh = cube();
        let a = mesh.vertices.len() as u32;
        mesh.vertices.push(Vec3::new(0.5, 0.0, 0.5 + 1.0e-5));
        mesh.triangles[5] = [0, a, 4];
        mesh.triangles.push([a, 5, 4]);
        let mut tags: Vec<u32> = (0..mesh.triangles.len() as u32).map(|i| i / 2).collect();
        let normals: Vec<Vec3> = (0..mesh.triangles.len())
            .map(|i| {
                let [p, q, r] = mesh.triangle_points(i);
                q.sub(p).cross(r.sub(p)).normalized()
            })
            .collect();
        let by_tag: Vec<Vec3> = (0..7).map(|tag| normals[tags.iter().position(|t| *t == tag).unwrap()]).collect();
        heal(&mut mesh, &mut tags).expect("defective");
        assert_eq!(tags.len(), mesh.triangles.len());
        for (index, tag) in tags.iter().enumerate() {
            let [p, q, r] = mesh.triangle_points(index);
            let normal = q.sub(p).cross(r.sub(p)).normalized();
            assert!(normal.dot(by_tag[*tag as usize]) > 0.99, "triangle {index} carries the wrong tag {tag}");
        }
    }
}
