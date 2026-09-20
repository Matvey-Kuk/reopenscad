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
//!
//! # Termination
//!
//! [`Bsp::build`] subdivides until it runs out of polygons, and it only gets
//! there because each step files at least one polygon at the node it is
//! working on: the splitting plane belongs to a polygon of that very set, so
//! that polygon is coplanar with it by construction. The premise is not free.
//! A split fragment inherits its parent's plane, and a near-degenerate one can
//! end up further than [`EPSILON`] from it, at which point its plane no longer
//! selects it and the step can consume nothing at all. `build` detects that
//! case and stops subdividing rather than descending into a child that would
//! repeat it forever; see the comment on the guard.
//!
//! That guard is specific to a divergence that was diagnosed. Behind it sits a
//! node budget that is not: a split cascade which grows faster than it files
//! is not an infinite loop, but an exponential allocation on a shared server
//! ends the same way, with the instance killed and every concurrent request
//! dropped. [`node_limit`] bounds what any such failure can cost, and a build
//! that reaches it aborts rather than returning the partial tree — half a
//! partition is wrong geometry, quietly.

use super::mesh::{Plane, Polygon, Vec3, EPSILON};
use super::prof;

const COPLANAR: u8 = 0;
const FRONT: u8 = 1;
const BACK: u8 = 2;
const SPANNING: u8 = 3;

/// Number of candidate planes sampled when choosing a node split.
const SPLIT_CANDIDATES: usize = 12;

/// Marks the panic [`Bsp::build`] raises when it runs out of node budget, so
/// the engine can turn that one panic into a real error message and leave
/// every other panic reported as the internal fault it is.
pub const BUDGET_PANIC_PREFIX: &str = "reopenscad-csg-budget: ";

/// Nodes a single [`Bsp::build`] call may add per polygon handed to it.
///
/// Measured against real work rather than guessed: the OpenAPPA corpus and the
/// workspace models build 1.00–1.07 nodes per input polygon, because a healthy
/// subdivision files polygons at roughly the rate it creates places to put
/// them. Sixty-four is a factor of sixty over the worst of those, so a model
/// has to be diverging, not merely awkward, to reach it.
const NODES_PER_POLYGON: usize = 64;

/// Floor under the per-call allowance, so a boolean between two tiny operands
/// still gets room for an ordinary tree. The divergence this guards against
/// allocates millions of nodes from two polygons, so a floor this size costs
/// nothing in detection and removes any chance of refusing small real work.
const MIN_NODE_ALLOWANCE: usize = 65_536;

/// Bytes of node storage one tree may occupy before the build is abandoned.
///
/// The per-polygon allowance above scales with the operand, which is what
/// catches a runaway on a small input; this is the ceiling that stops a large
/// input from authorising a large runaway. 256 MiB is sized against the
/// deployment in `web/DEPLOY.md`: a 2 GiB instance admitting
/// `MAX_CONCURRENT_JOBS = 2` renders, each of which also holds meshes and a
/// response buffer. Running out is not graceful — Cloud Run kills the instance
/// and every concurrent request with it — so the kernel stops well short.
const NODE_BUDGET_BYTES: usize = 256 * 1024 * 1024;

/// Environment override for [`NODE_BUDGET_BYTES`], in mebibytes.
///
/// The ceiling is a judgement about a machine, not a property of the geometry,
/// so an operator who meets a legitimate model that needs more room can raise
/// it without a rebuild — and one running a smaller instance can lower it.
const NODE_BUDGET_ENV: &str = "REOPENSCAD_MAX_BSP_MIB";

/// Nodes the tree may reach before a build is abandoned.
///
/// The per-polygon allowance is what catches a runaway on a small operand; the
/// ceiling is what stops a large operand from authorising a large runaway. A
/// build that is handed an already-standing tree gets its allowance on top of
/// what is there, because `union` builds into the tree it just made.
fn node_limit(polygons: usize, existing: usize, ceiling: usize) -> usize {
    polygons
        .saturating_mul(NODES_PER_POLYGON)
        .max(MIN_NODE_ALLOWANCE)
        .saturating_add(existing)
        .min(ceiling)
}

/// Hard ceiling on nodes in one tree, from [`NODE_BUDGET_BYTES`] and the
/// measured size of a node rather than a round number chosen to look safe.
fn node_ceiling() -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CEILING: AtomicUsize = AtomicUsize::new(0);
    let cached = CEILING.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let bytes = std::env::var(NODE_BUDGET_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|mib| *mib > 0)
        .map_or(NODE_BUDGET_BYTES, |mib| mib.saturating_mul(1024 * 1024));
    let ceiling = (bytes / std::mem::size_of::<BspNode>()).max(1);
    CEILING.store(ceiling, Ordering::Relaxed);
    ceiling
}

/// Abandons a build that is not converging.
///
/// A panic rather than an error return because every boolean operation between
/// here and the engine — `Solid::union`, `union_all`, the parallel fold — is
/// infallible by signature, and threading a `Result` through all of them to
/// report a condition that must never happen would be a worse trade than
/// unwinding. `engine::compile_parts` catches this and turns it back into an
/// `EngineError`, so the caller still sees an ordinary failed render.
///
/// Failing here is deliberately *not* a silent degradation: stopping the
/// subdivision early and keeping the partial tree would answer with geometry
/// that is quietly wrong, which is worse than answering with nothing.
#[cold]
#[inline(never)]
fn budget_exhausted(limit: usize) -> ! {
    panic!(
        "{BUDGET_PANIC_PREFIX}This model is too complex for the exact kernel: a boolean \
         operation's BSP build passed its budget of {limit} nodes without converging. \
         Reduce $fn, or split the model into fewer overlapping parts."
    );
}

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
        // The guard inside rules out the one non-progressing step this build
        // is known to take, but "known" is the operative word: a split cascade
        // that merely grows faster than it files is not an infinite loop, and
        // on a shared instance an exponential allocation is as fatal as an
        // endless one. The budget bounds what any failure of this kind can
        // cost, whether or not it is a failure anybody has seen.
        let limit = node_limit(polygons.len(), self.nodes.len(), node_ceiling());
        self.build_within(polygons, limit);
    }

    /// [`Self::build`] against an explicit node ceiling.
    ///
    /// Separate so the budget can be exercised by a test at a size a test can
    /// afford: reaching the real floor of [`MIN_NODE_ALLOWANCE`] nodes needs an
    /// input far too large to build in a unit test, and an environment
    /// variable would leak across the suite's threads.
    fn build_within(&mut self, polygons: Vec<Polygon>, limit: usize) {
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
            if self.nodes.len() > limit {
                budget_exhausted(limit);
            }
            // Whether this set is what the node's plane was chosen from. A node
            // reached a second time -- `union` calls `build` again on a tree
            // that is already standing -- keeps the plane it got the first
            // time, and then a set that misses it entirely is unremarkable.
            let own_plane = self.nodes[index].plane.is_none();
            if own_plane {
                self.nodes[index].plane = Some(choose_plane(&polygons));
            }
            let plane = self.nodes[index].plane.expect("plane was just assigned");
            let mut front = Vec::new();
            let mut back = Vec::new();
            let mut split_any = false;
            for polygon in polygons {
                let kind = split_polygon_owned(
                    plane,
                    polygon,
                    &mut coplanar_front,
                    &mut coplanar_back,
                    &mut front,
                    &mut back,
                );
                split_any |= kind == SPANNING;
            }
            let filed = coplanar_front.len() + coplanar_back.len();
            self.nodes[index].polygons.extend(coplanar_front.drain(..));
            self.nodes[index].polygons.extend(coplanar_back.drain(..));
            // The subdivision terminates only because every step consumes at
            // least one polygon: the plane comes from a polygon of this very
            // set, so that polygon classifies as coplanar and stays here.
            //
            // A near-degenerate fragment breaks the premise. `push_fragment`
            // gives a split fragment the *parent's* plane, and for a sliver a
            // few nanometres across the lerped vertices need not sit within
            // EPSILON of it. `choose_plane` then hands back a plane that its
            // own donor classifies as FRONT or BACK, nothing is filed here,
            // the whole set moves to a freshly allocated child, that child
            // picks the same plane from the same polygons — and the loop
            // allocates a node per iteration until the process dies. That is
            // not hypothetical: two 1.2e-9 mm^2 needles out of a hinge union
            // did exactly this, and on a server it is an OOM, not an error.
            //
            // The divergent step is exactly: the plane came from this set,
            // nothing was filed here, nothing was split, every polygon went
            // whole to one side, and the child on that side has no plane yet.
            // Then the child's polygon list is this one, element for element
            // and in the same order, so `choose_plane` returns the same plane
            // and the step repeats forever. Anything else makes progress -- a
            // split shrinks the fragments, a populated pair of sides shrinks
            // both subsets, and either child or node holding a plane picked
            // from some other set classifies against a different plane.
            //
            // The fix is to restore the premise, not to stop subdividing:
            // file *one* polygon here, and let the rest descend as they would
            // have. That is enough for termination, because the step now
            // consumes one either way, and it is all that may be done.
            //
            // Filing the whole set instead — and so never allocating the
            // child — looks equivalent and is not. A node's payload really
            // does take no part in classification, so *where* a polygon is
            // stored is free; `clip_polygons` reads only planes and children,
            // and `clip_to` clips a node's payload against the other tree
            // wherever it sits. But the absent child is not free. A front
            // half-space with no front child means "outside" and a back
            // half-space with no back child means "inside", so collapsing the
            // subtree silently reclassifies that whole region instead of
            // letting the polygons that fall in it subdivide it. On the
            // coffee-bin model that turned 545 stalls into an open surface:
            // 1669 boundary edges, 400 non-manifold edges, and facets welded
            // between the wall and the floor.
            if own_plane && filed == 0 && !split_any {
                let to_front = match (front.is_empty(), back.is_empty()) {
                    (false, true) => Some(true),
                    (true, false) => Some(false),
                    // Both sides populated: each child gets a strictly smaller
                    // subset. Both empty: every polygon was dropped.
                    _ => None,
                };
                if let Some(to_front) = to_front {
                    let child = if to_front {
                        self.nodes[index].front
                    } else {
                        self.nodes[index].back
                    };
                    let would_repeat =
                        child.is_none_or(|child| self.nodes[child].plane.is_none());
                    if would_repeat {
                        let stalled = if to_front { &mut front } else { &mut back };
                        // Order is load-bearing: the child's set has to differ
                        // from this one, and taking the front keeps what is
                        // left in the order `choose_plane` will see it.
                        self.nodes[index].polygons.push(stalled.remove(0));
                    }
                }
            }
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
///
/// Returns how the polygon classified: `COPLANAR`, `FRONT`, `BACK` or
/// `SPANNING`. `build` needs it to tell a step that split something from one
/// that merely moved the whole set down one level.
pub fn split_polygon(
    plane: Plane,
    polygon: &Polygon,
    coplanar_front: &mut Vec<Polygon>,
    coplanar_back: &mut Vec<Polygon>,
    front: &mut Vec<Polygon>,
    back: &mut Vec<Polygon>,
) -> u8 {
    split_polygon_owned(
        plane,
        polygon.clone(),
        coplanar_front,
        coplanar_back,
        front,
        back,
    )
}

/// [`split_polygon`] taking ownership.
///
/// Three of the four outcomes file the polygon unchanged, so taking it by value
/// lets those cases move it into the bucket instead of cloning its vertex
/// vector. The tree build and every clip pass hit this path millions of times
/// on a large model, and the clones were the kernel's single largest source of
/// allocator traffic.
///
/// Returns the polygon's classification, as [`split_polygon`] does.
pub fn split_polygon_owned(
    plane: Plane,
    polygon: Polygon,
    coplanar_front: &mut Vec<Polygon>,
    coplanar_back: &mut Vec<Polygon>,
    front: &mut Vec<Polygon>,
    back: &mut Vec<Polygon>,
) -> u8 {
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
    polygon_kind
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

    /// The two polygons a hinge union produced that used to hang `build`
    /// forever. Both are needles of about 1.2e-9 mm^2, and the first one's
    /// stored plane -- inherited from the polygon it was split out of -- misses
    /// one of its own vertices by 1.0e-7, a hundred times [`EPSILON`]. So
    /// `choose_plane` returns that plane, its own donor classifies as BACK
    /// rather than COPLANAR, nothing stays at the node, and the pair descends
    /// into a fresh child that repeats the step. Before the progress guard in
    /// `build` this allocated a node per iteration until the process was
    /// killed; on the server that was an OOM, not a render error.
    ///
    /// Run on a worker so a regression fails the test in ten seconds instead of
    /// hanging the suite until the machine runs out of memory.
    #[test]
    fn a_sliver_whose_plane_misses_its_own_vertices_cannot_spin_the_build() {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let needle = Polygon::with_plane(
                vec![
                    Vec3::new(34.586_764_699_586_055, 52.140_000_000_000_01, 1.199_999_999_999_995_7),
                    Vec3::new(34.586_764_699_494_175, 52.539_999_860_859_96, 1.600_000_155_347_984),
                    Vec3::new(34.586_764_703_783_61, 52.539_999_848_703_92, 1.600_000_143_191_938),
                ],
                Plane::new(
                    Vec3::new(0.0, 0.707_107_171_626_174_1, -0.707_106_390_746_705_6),
                    36.020_040_363_809_98,
                ),
            );
            let wedge = Polygon::with_plane(
                vec![
                    Vec3::new(34.586_764_704_855_966, 52.539_999_850_267_86, 1.600_000_144_755_880_7),
                    Vec3::new(34.586_764_702_803_13, 52.140_000_045_638_48, 1.200_000_045_638_503_6),
                    Vec3::new(34.586_764_699_586_055, 52.140_000_000_000_01, 1.199_999_999_999_995_7),
                    Vec3::new(34.586_764_703_783_61, 52.539_999_860_859_96, 1.600_000_155_347_984),
                ],
                Plane::new(
                    Vec3::new(0.000_000_772_746_007, 0.707_106_520_892_892_8, -0.707_107_041_479_684_2),
                    36.020_032_276_364_14,
                ),
            );
            let tree = Bsp::from_polygons(vec![needle, wedge]);
            let _ = sender.send((tree.nodes.len(), tree.polygon_count()));
        });

        let (nodes, polygons) = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("building a BSP over near-degenerate slivers must terminate");
        assert!(
            nodes <= 8,
            "the build subdivided {nodes} nodes for two polygons: the progress guard is gone"
        );
        // Stalling files the polygons at the node; it must not discard them.
        assert_eq!(polygons, 2);
    }

    /// A build that will not converge has to stop, and it has to stop loudly:
    /// keeping the half-built tree would answer with geometry that is quietly
    /// wrong, which is worse than answering with nothing. The message carries
    /// the prefix `engine::compile_parts` keys on to turn this back into a
    /// render error rather than a dropped connection.
    #[test]
    fn a_build_that_passes_its_node_budget_aborts_instead_of_allocating() {
        // Forty overlapping cubes: a perfectly ordinary subdivision, run
        // against a ceiling far below the nodes it legitimately needs.
        let mut polygons = Vec::new();
        for step in 0..40 {
            let offset = step as f64 * 3.7;
            let cube = primitives::cube(Vec3::new(10.0, 10.0, 10.0), false).transformed(
                super::super::mesh::Matrix4::translation(Vec3::new(offset, offset, 0.0)),
            );
            polygons.extend(cube.polygons);
        }
        let panic = std::panic::catch_unwind(|| {
            let mut tree = Bsp::new();
            tree.build_within(polygons, 4);
            tree.nodes.len()
        })
        .expect_err("a build past its budget must not return a partial tree");
        let message = panic
            .downcast_ref::<String>()
            .expect("the budget abort panics with a formatted message");
        assert!(
            message.starts_with(BUDGET_PANIC_PREFIX),
            "the engine keys on this prefix to report the abort: {message}"
        );
    }

    /// The budget has to be loose enough that real work never meets it. These
    /// are the numbers behind that claim, so a later edit to the constants has
    /// to restate them rather than quietly narrowing the margin.
    #[test]
    fn the_node_budget_leaves_real_models_a_wide_margin() {
        let ceiling = usize::MAX;
        // Measured peak on the OpenAPPA corpus and the workspace models is
        // 1.07 nodes per input polygon; the allowance is sixty times that.
        assert_eq!(node_limit(10_000, 0, ceiling), 640_000);
        // Small operands get the floor, not a proportionally tiny allowance.
        assert_eq!(node_limit(2, 0, ceiling), MIN_NODE_ALLOWANCE);
        // `union` builds into a standing tree, so the allowance is on top of it.
        assert_eq!(node_limit(2, 500, ceiling), MIN_NODE_ALLOWANCE + 500);
        // The ceiling wins over any allowance a large operand would earn.
        assert_eq!(node_limit(10_000_000, 0, 4_096), 4_096);
        // Neither term may wrap on absurd input.
        assert_eq!(node_limit(usize::MAX, usize::MAX, ceiling), usize::MAX);
        // The real ceiling is a memory budget, so it has to be sane in nodes.
        assert!((100_000..100_000_000).contains(&node_ceiling()));
    }
}
