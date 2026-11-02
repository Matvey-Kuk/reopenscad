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

use super::mesh::{FastMap, Plane, Polygon, Vec3, EPSILON};
use super::prof;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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

/// Raised by the server's memory watchdog while the process is over its
/// resident-memory budget (see `start_memory_watchdog` in `main.rs`).
///
/// A boolean fold over hundreds of operands — a long `hull()` chain is the
/// usual one — allocates BSP trees and polygon lists far faster than the
/// node budget above can see, because each tree stays under it; it is the
/// *sum* that outgrows the container. Running out is not graceful: the
/// platform kills the instance and every request on it answers 503 with no
/// explanation. Builds poll this flag and abort with an explanation instead,
/// which `engine::compile_parts` turns into an ordinary failed render. It is
/// process-wide on purpose: under memory pressure every in-flight build is
/// part of the problem, and stopping all of them is strictly better than the
/// kill that would stop them anyway.
pub static MEMORY_PRESSURE: AtomicBool = AtomicBool::new(false);
/// The budget and the resident size that tripped it, in MiB, for the message.
pub static MEMORY_LIMIT_MIB: AtomicUsize = AtomicUsize::new(0);
pub static MEMORY_RESIDENT_MIB: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Wall-clock deadline for builds on this thread, set by analysis code
    /// (`crate::analysis`) around booleans that have no cancellation flag of
    /// their own. `None` (the default) means no deadline.
    static BUILD_DEADLINE: std::cell::Cell<Option<std::time::Instant>> =
        const { std::cell::Cell::new(None) };
    /// Cancellation flags in force on this thread, any one of which stops
    /// every build: the compile's own flag (borrowed, installed by
    /// [`with_cancellation`] for exactly as long as the borrow lives) and
    /// request-wide ones such as "the client disconnected" (owned, installed
    /// by [`cancellation_scope`]). Without them the flag a request's watchdog
    /// trips was only read between CSG nodes, so one long boolean, face merge
    /// or sealing pass ran to the end however long ago its request gave up.
    static BUILD_CANCEL: std::cell::RefCell<CancelFlags> =
        const { std::cell::RefCell::new(CancelFlags { borrowed: Vec::new(), owned: Vec::new() }) };
}

#[derive(Clone, Default)]
struct CancelFlags {
    borrowed: Vec<*const AtomicBool>,
    owned: Vec<std::sync::Arc<AtomicBool>>,
}

impl CancelFlags {
    fn any_set(&self) -> bool {
        // SAFETY: a borrowed pointer is only present while the frame that
        // installed it (`with_cancellation`, or `Interrupts::install` on a
        // scoped worker of that frame) is running.
        self.borrowed.iter().any(|flag| unsafe { &**flag }.load(Ordering::Acquire))
            || self.owned.iter().any(|flag| flag.load(Ordering::Acquire))
    }
}

/// Restores the previous deadline when dropped.
pub struct DeadlineScope(Option<std::time::Instant>);

impl Drop for DeadlineScope {
    fn drop(&mut self) {
        BUILD_DEADLINE.with(|cell| cell.set(self.0));
    }
}

/// Builds on this thread abort (with a [`BUDGET_PANIC_PREFIX`] panic) once
/// `deadline` passes, until the returned scope is dropped. A tighter deadline
/// already in force is kept.
pub fn deadline_scope(deadline: std::time::Instant) -> DeadlineScope {
    BUILD_DEADLINE.with(|cell| {
        let previous = cell.get();
        cell.set(Some(previous.map_or(deadline, |earlier| earlier.min(deadline))));
        DeadlineScope(previous)
    })
}

/// Runs `body` with `flag` added to this thread's cancellation flags: every
/// kernel poll ([`deadline_passed`], [`check_interrupt`], the BSP build's
/// step check) sees it set and aborts with a [`BUDGET_PANIC_PREFIX`] panic
/// whose message is `Render cancelled.`. `None` runs `body` unchanged.
///
/// Closure-scoped rather than a guard so the stored pointer can never outlive
/// the borrow: it is removed on return and on unwind alike.
pub fn with_cancellation<R>(flag: Option<&AtomicBool>, body: impl FnOnce() -> R) -> R {
    let Some(flag) = flag else {
        return body();
    };
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            BUILD_CANCEL.with(|cell| {
                cell.borrow_mut().borrowed.pop();
            });
        }
    }
    BUILD_CANCEL.with(|cell| cell.borrow_mut().borrowed.push(flag as *const AtomicBool));
    let _pop = Pop;
    body()
}

/// Removes the flag [`cancellation_scope`] installed when dropped.
pub struct CancelScope(std::sync::Arc<AtomicBool>);

impl Drop for CancelScope {
    fn drop(&mut self) {
        BUILD_CANCEL.with(|cell| {
            let mut flags = cell.borrow_mut();
            if let Some(position) = flags.owned.iter().rposition(|flag| std::sync::Arc::ptr_eq(flag, &self.0)) {
                flags.owned.remove(position);
            }
        });
    }
}

/// [`with_cancellation`] for a flag the caller shares rather than lends,
/// until the returned scope is dropped. The server installs one per request
/// that it sets when the client disconnects.
pub fn cancellation_scope(flag: std::sync::Arc<AtomicBool>) -> CancelScope {
    BUILD_CANCEL.with(|cell| cell.borrow_mut().owned.push(std::sync::Arc::clone(&flag)));
    CancelScope(flag)
}

/// Restores the cancellation flags [`isolated_cancellation_scope`] set aside.
pub struct IsolatedCancelScope(Option<CancelFlags>);

impl Drop for IsolatedCancelScope {
    fn drop(&mut self) {
        if let Some(previous) = self.0.take() {
            BUILD_CANCEL.with(|cell| *cell.borrow_mut() = previous);
        }
    }
}

/// Makes `flag` the only cancellation flag on this thread until the returned
/// scope is dropped, setting the others (such as the request's "client
/// disconnected" flag) aside. For work that must outlive the request that
/// started it, such as a shared job other callers wait on.
pub fn isolated_cancellation_scope(flag: std::sync::Arc<AtomicBool>) -> IsolatedCancelScope {
    let previous = BUILD_CANCEL.with(|cell| {
        std::mem::replace(&mut *cell.borrow_mut(), CancelFlags { borrowed: Vec::new(), owned: vec![flag] })
    });
    IsolatedCancelScope(Some(previous))
}

/// This thread's deadline and cancellation flags, to hand to worker threads
/// (see [`Interrupts::with_current`] and [`Interrupts::install`]).
///
/// The captured flags include pointers lent by [`with_cancellation`] frames,
/// valid only while those frames run. The `'a` brand enforces that in the
/// type system: an `Interrupts` exists only inside a
/// [`Interrupts::with_current`] closure, `'a` is chosen fresh for that call
/// and is invariant, so neither the value nor a clone of it can be returned,
/// stored outside, or moved to a non-scoped (`'static`) thread.
///
/// ```compile_fail
/// use reopenscad_server::csg::bsp::Interrupts;
/// let leaked = Interrupts::with_current(|interrupts| interrupts.clone());
/// ```
///
/// ```compile_fail
/// use reopenscad_server::csg::bsp::Interrupts;
/// Interrupts::with_current(|interrupts| {
///     let interrupts = interrupts.clone();
///     std::thread::spawn(move || interrupts.install(|| ()));
/// });
/// ```
///
/// ```
/// use reopenscad_server::csg::bsp::Interrupts;
/// Interrupts::with_current(|interrupts| {
///     std::thread::scope(|scope| { scope.spawn(|| interrupts.install(|| ())); });
/// });
/// ```
#[derive(Clone)]
pub struct Interrupts<'a> {
    deadline: Option<std::time::Instant>,
    cancel: CancelFlags,
    _brand: std::marker::PhantomData<fn(&'a ()) -> &'a ()>,
}

// SAFETY: the pointers are to `AtomicBool`s, which are `Sync`, and they are
// only dereferenced while the `with_cancellation` frames that installed them
// are still running: an `Interrupts<'a>` cannot leave the `with_current`
// closure that created it (see the type's docs), and that closure runs
// inside those frames. A thread that can see it is therefore a scoped thread
// joined before the closure returns.
unsafe impl Send for Interrupts<'_> {}
unsafe impl Sync for Interrupts<'_> {}

impl Interrupts<'_> {
    /// Runs `body` with what builds on this thread are currently interrupted
    /// by, to hand to scoped worker threads spawned inside `body`.
    pub fn with_current<R>(body: impl for<'a> FnOnce(&Interrupts<'a>) -> R) -> R {
        let interrupts = Interrupts {
            deadline: BUILD_DEADLINE.with(|cell| cell.get()),
            cancel: BUILD_CANCEL.with(|cell| cell.borrow().clone()),
            _brand: std::marker::PhantomData,
        };
        body(&interrupts)
    }

    /// Runs `body` on this (worker) thread under the captured deadline and
    /// flags.
    pub fn install<R>(&self, body: impl FnOnce() -> R) -> R {
        let _deadline = self.deadline.map(deadline_scope);
        struct Restore(Option<CancelFlags>);
        impl Drop for Restore {
            fn drop(&mut self) {
                if let Some(previous) = self.0.take() {
                    BUILD_CANCEL.with(|cell| *cell.borrow_mut() = previous);
                }
            }
        }
        let previous = BUILD_CANCEL.with(|cell| {
            let mut flags = cell.borrow_mut();
            let previous = flags.clone();
            flags.borrowed.extend(self.cancel.borrowed.iter().copied());
            flags.owned.extend(self.cancel.owned.iter().cloned());
            previous
        });
        let _restore = Restore(Some(previous));
        body()
    }
}

fn cancel_requested() -> bool {
    BUILD_CANCEL.with(|cell| cell.borrow().any_set())
}

/// Whether this thread's build deadline has passed or its compile was
/// cancelled. Long loops outside the kernel poll this too.
pub fn deadline_passed() -> bool {
    cancel_requested()
        || BUILD_DEADLINE.with(|cell| cell.get().is_some_and(|deadline| std::time::Instant::now() >= deadline))
}

/// Why builds on this thread should stop now, if they should: the same text
/// the abort panic carries, for callers that return an error instead.
pub fn interrupt_message() -> Option<&'static str> {
    if cancel_requested() {
        Some(CANCELLED_MESSAGE)
    } else if deadline_passed() {
        Some(DEADLINE_MESSAGE)
    } else {
        None
    }
}

const CANCELLED_MESSAGE: &str = "Render cancelled.";
const DEADLINE_MESSAGE: &str = "The request's time budget ran out before the model was done; \
     simplify it, check fewer objects at once or lower $fn.";

/// Aborts the current build (with a [`BUDGET_PANIC_PREFIX`] panic) if this
/// thread's deadline has passed or its compile was cancelled.
#[inline]
pub fn check_interrupt() {
    if deadline_passed() {
        deadline_exhausted();
    }
}

/// [`check_interrupt`] every `every` calls, for hot loops: `counter` is the
/// caller's own tally.
#[inline]
pub fn poll_interrupt(counter: &mut usize, every: usize) {
    *counter += 1;
    if *counter % every == 0 {
        check_interrupt();
    }
}

#[cold]
#[inline(never)]
fn deadline_exhausted() -> ! {
    // The same words as `engine::check_cancelled`, so callers that map a
    // cancelled render (or explain an expired deadline) see one message.
    let message = if cancel_requested() { CANCELLED_MESSAGE } else { DEADLINE_MESSAGE };
    panic!("{BUDGET_PANIC_PREFIX}{message}");
}


#[cold]
#[inline(never)]
fn memory_exhausted() -> ! {
    let limit = MEMORY_LIMIT_MIB.load(Ordering::Relaxed);
    let resident = MEMORY_RESIDENT_MIB.load(Ordering::Relaxed);
    panic!(
        "{BUDGET_PANIC_PREFIX}This model needs more memory than this server allows: the render \
         was stopped when the server reached {resident} MiB of its {limit} MiB memory budget \
         (REOPENSCAD_MAX_RSS_MIB, or 85% of the container limit). The usual cause is a boolean \
         over many large operands, such as a long hull() chain or a union of many curved \
         parts. Lower $fn/$fa/$fs on the curved parts, use fewer or simpler hull() segments, or \
         render fewer objects at once. If another heavy render was running at the same time, \
         retrying alone may succeed."
    );
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
        prof::scope("bsp:build_any", || self.build_counted(polygons))
    }

    fn build_counted(&mut self, polygons: Vec<Polygon>) {
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
        // Polled on entry too: a fold of many small operands (a minkowski
        // sum's pieces, a hull chain) runs thousands of builds that never
        // reach the 256-step check below.
        check_interrupt();
        // Building into a standing tree (`union`, and the last step of every
        // boolean) hands a set down its chains like a clip does; see
        // `ChainSkips`. Built on first use, they cover the nodes standing
        // then, whose planes and existing links this build never changes.
        let mut skips: Option<Option<ChainSkips>> = None;
        // The third field is the largest set the support peel may run on
        // again in this branch; see `peel_chain`.
        let mut work = vec![(0usize, polygons, usize::MAX, None::<Ball>, 0usize)];
        // The coplanar buckets are drained into the node before the next
        // iteration, so one pair of buffers serves the whole build; only the
        // front/back lists have to outlive the step, because they go on the
        // work stack.
        let mut coplanar_front = Vec::new();
        let mut coplanar_back = Vec::new();
        let mut steps = 0usize;
        while let Some((index, mut polygons, peel_floor, ball, streak)) = work.pop() {
            if polygons.is_empty() {
                continue;
            }
            if self.nodes.len() > limit {
                budget_exhausted(limit);
            }
            if MEMORY_PRESSURE.load(Ordering::Relaxed) {
                memory_exhausted();
            }
            steps += 1;
            if steps % 256 == 0 && deadline_passed() {
                deadline_exhausted();
            }
            // Whether this set is what the node's plane was chosen from. A node
            // reached a second time -- `union` calls `build` again on a tree
            // that is already standing -- keeps the plane it got the first
            // time, and then a set that misses it entirely is unremarkable.
            let mut index = index;
            let mut ball = ball;
            let mut streak = streak;
            if streak >= SKIP_STREAK && self.nodes[index].plane.is_some() {
                if let Some(skips) = skips.get_or_insert_with(|| ChainSkips::new(self)) {
                    let around = *ball.get_or_insert_with(|| ChainSkips::ball(&polygons));
                    let landed = skips.advance(self, index, around);
                    if landed == index {
                        // Not here; try again a few steps further down.
                        streak = 0;
                    }
                    index = landed;
                }
            }
            let own_plane = self.nodes[index].plane.is_none();
            let mut peel_floor = peel_floor;
            if own_plane {
                let (mut plane, one_sided) = choose_plane_scored(&polygons);
                // A nearly convex set builds a chain as long as itself, and
                // step by step that is quadratic. When the sample says the
                // set looks like that, `peel_chain` lays the chain down at
                // the cost of what lies near each plane. Once per halving of
                // the set along a branch, so a set that turns out not to
                // peel cannot make every level pay for the attempt.
                if one_sided
                    && polygons.len() >= PEEL_MIN_POLYGONS
                    && polygons.len() <= peel_floor
                {
                    let size = polygons.len();
                    let (chain, rest) = prof::scope("bsp:peel", || peel_chain(polygons));
                    polygons = rest;
                    peel_floor = size / 2;
                    let peeled = !chain.is_empty();
                    let last = chain.len().saturating_sub(1);
                    for (position, node) in chain.into_iter().enumerate() {
                        self.nodes[index].plane = Some(node.plane);
                        self.nodes[index].polygons.extend(node.filed);
                        if !node.front.is_empty() {
                            let child = self.allocate();
                            self.nodes[index].front = Some(child);
                            work.push((child, node.front, peel_floor, None, 0));
                        }
                        if position < last || !polygons.is_empty() {
                            let child = self.allocate();
                            self.nodes[index].back = Some(child);
                            index = child;
                        }
                    }
                    if polygons.is_empty() {
                        continue;
                    }
                    if peeled {
                        // `index` is now the fresh back child under the chain.
                        plane = choose_plane_scored(&polygons).0;
                    }
                }
                self.nodes[index].plane = Some(plane);
            }
            let plane = self.nodes[index].plane.expect("plane was just assigned");
            // A standing node the whole set passes on one side of (`union`
            // building into its tree, most often down a long chain): hand
            // the set on as it is, as the split below would.
            if !own_plane {
                if let Some(kind) = one_side(plane, &polygons) {
                    let existing = if kind == FRONT {
                        self.nodes[index].front
                    } else {
                        self.nodes[index].back
                    };
                    let child = match existing {
                        Some(child) => child,
                        None => {
                            let child = self.allocate();
                            if kind == FRONT {
                                self.nodes[index].front = Some(child);
                            } else {
                                self.nodes[index].back = Some(child);
                            }
                            child
                        }
                    };
                    // The set is unchanged, so its ball is too.
                    work.push((child, polygons, peel_floor, ball, streak + 1));
                    continue;
                }
            }
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
            // stored is free; `clip_polygons_with` reads only planes and children,
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
                work.push((child, front, peel_floor, None, 0));
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
                work.push((child, back, peel_floor, None, 0));
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

    /// Removes the parts of `polygons` that lie inside this solid, borrowing
    /// the traversal stack from the caller.
    ///
    /// `clip_to` runs this once per node of the tree being clipped, so the
    /// stack is allocated and thrown away thousands of times per boolean.
    fn clip_polygons_with(
        &self,
        polygons: Vec<Polygon>,
        work: &mut Vec<(usize, Vec<Polygon>, Option<Ball>, usize)>,
        skips: &mut Option<Option<ChainSkips>>,
    ) -> Vec<Polygon> {
        prof::count("bsp:clip_calls", 1);
        if polygons.is_empty() {
            return polygons;
        }
        let mut output = Vec::with_capacity(polygons.len());
        work.push((0usize, polygons, None, 0));
        let mut steps = 0usize;
        while let Some((index, polygons, ball, streak)) = work.pop() {
            if polygons.is_empty() {
                continue;
            }
            poll_interrupt(&mut steps, 1024);
            // A set that has passed a run of nodes unchanged is probably
            // walking a chain; see `ChainSkips`, built for this tree on first
            // use and kept for the caller's next set.
            let mut ball = ball;
            let mut streak = streak;
            let mut index = index;
            if streak >= SKIP_STREAK {
                if let Some(skips) = skips.get_or_insert_with(|| ChainSkips::new(self)) {
                    let around = *ball.get_or_insert_with(|| ChainSkips::ball(&polygons));
                    let landed = skips.advance(self, index, around);
                    if landed == index {
                        streak = 0;
                    }
                    index = landed;
                }
            }
            let node = &self.nodes[index];
            let Some(plane) = node.plane else {
                output.extend(polygons);
                continue;
            };
            // Wholly on one side, the set moves on as it is. On the chain a
            // nearly convex operand builds, that is nearly every step, and
            // splitting into fresh buckets cost four allocations a node.
            if let Some(kind) = one_side(plane, &polygons) {
                let child = if kind == FRONT { node.front } else { node.back };
                match child {
                    Some(child) => work.push((child, polygons, ball, streak + 1)),
                    None if kind == FRONT => output.extend(polygons),
                    None => {}
                }
                continue;
            }
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
                Some(child) => work.push((child, front, None, 0)),
                None => output.extend(front),
            }
            if let Some(child) = node.back {
                work.push((child, back, None, 0));
            }
            // Without a back subtree the back half-space is solid, so those
            // fragments are inside this solid and get discarded.
        }
        output
    }

    /// Removes the parts of this solid that lie inside `other`.
    pub fn clip_to(&mut self, other: &Bsp) {
        prof::scope("bsp:clip_to", || self.clip_to_inner(other))
    }

    fn clip_to_inner(&mut self, other: &Bsp) {
        prof::count("bsp:clip_to_nodes", self.nodes.len() as u64);
        let mut work = Vec::new();
        let mut skips = None;
        for index in 0..self.nodes.len() {
            if index % 1024 == 1023 {
                check_interrupt();
            }
            let polygons = std::mem::take(&mut self.nodes[index].polygons);
            if polygons.is_empty() {
                continue;
            }
            self.nodes[index].polygons = other.clip_polygons_with(polygons, &mut work, &mut skips);
        }
    }

    /// Every polygon of a tree that is about to be dropped: the
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

/// Trees smaller than this are walked node by node; the shortcut below would
/// cost more to set up than it saves.
const SKIP_MIN_NODES: usize = 512;
/// Nodes one skip of the lowest level spans; each level spans this many skips
/// of the one below.
const SKIP_RUN: usize = 16;
/// Levels of skips: runs of 16, 256 and 4096 nodes.
///
/// A set is only tested against them after this many consecutive nodes it
/// passed unchanged, so ordinary trees, whose sets split within a few levels,
/// never pay for building or consulting them.
const SKIP_STREAK: usize = 8;

const SKIP_LEVELS: usize = 3;

/// One jump down a run of nodes along one side's links: from a node to
/// `target`, the first node not jumped over, past `len` planes.
///
/// The run's planes are summarised against the tree's centre `c` so a whole
/// run can be tested at once: `high`/`low` bound `n.c - w` over its planes,
/// `normal` is their mean normal and `spread` bounds how far any of their
/// normals is from it. For a set inside a ball (centre `p`, radius `r`),
/// every vertex `v` then has, for every plane of the run,
/// `n.v - w <= high + normal.(p - c) + r + spread * (|p - c| + r)`, and the
/// matching lower bound with `low`.
#[derive(Clone, Copy)]
struct Skip {
    target: u32,
    len: u32,
    high: f64,
    low: f64,
    normal: Vec3,
    spread: f64,
    /// Longest plane normal on the run.
    norm: f64,
}

impl Skip {
    fn empty(at: usize) -> Self {
        Self {
            target: at as u32,
            len: 0,
            high: f64::NEG_INFINITY,
            low: f64::INFINITY,
            normal: Vec3::ZERO,
            spread: 0.0,
            norm: 0.0,
        }
    }

    /// The consecutive runs `parts` (each starting where the previous one
    /// ends) as one run from `start`.
    fn joined(start: usize, parts: &[Skip]) -> Self {
        let Some(last) = parts.last() else {
            return Self::empty(start);
        };
        let len: u32 = parts.iter().map(|part| part.len).sum();
        let normal = parts
            .iter()
            .fold(Vec3::ZERO, |sum, part| sum.add(part.normal.mul(part.len as f64)))
            .mul(1.0 / len.max(1) as f64);
        // Each part's normals lie within its spread of its own mean, which
        // lies this far from the combined mean.
        let spread = parts
            .iter()
            .map(|part| part.spread + part.normal.sub(normal).length())
            .fold(0.0, f64::max);
        Self {
            target: last.target,
            len,
            high: parts.iter().map(|part| part.high).fold(f64::NEG_INFINITY, f64::max),
            low: parts.iter().map(|part| part.low).fold(f64::INFINITY, f64::min),
            normal,
            spread,
            norm: parts.iter().map(|part| part.norm).fold(0.0, f64::max),
        }
    }
}

/// A shortcut over the long chains a nearly convex solid builds.
///
/// The BSP of a convex solid (a high-`$fn` sphere, a scan) is a chain: every
/// node's back child is the next node, in face order. A polygon walks that
/// chain until it meets a plane it is not wholly behind, and for a polygon
/// inside the solid (the inner wall of a hollowed sphere, a part nested in a
/// scan) or near the far side of it, that is most or all of the chain. A
/// difference of two such solids was therefore quadratic: two nested
/// 400-segment spheres took 106 s.
///
/// Each node gets, per side and per run length, a [`Skip`]: a test that is
/// passed only when the set is strictly (beyond the classification epsilon)
/// on that side of every plane of the run, so every node of the run would
/// have handed the set on unchanged. The set then jumps to the run's end.
/// Runs follow only links that exist and only nodes that have a plane, so
/// the node a jump lands on is processed exactly as before, and the result is
/// identical, polygon for polygon and in the same order, to the walk.
struct ChainSkips {
    center: Vec3,
    /// Runs computed so far, `memo[level][side]` by start node; side 0
    /// follows back links, side 1 front. Filled on demand, so only the nodes
    /// a chain walk actually reaches pay anything.
    memo: [[FastMap<u32, Skip>; 2]; SKIP_LEVELS],
}

#[cfg(test)]
thread_local! {
    /// Lets a test run the plain walk to compare the shortcut against.
    static SKIPS_DISABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// How many jumps the shortcut has taken on this thread, so a test can
    /// tell that the comparison it makes actually exercised it.
    static SKIPS_TAKEN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A ball around a set of polygons: AABB centre and the farthest vertex.
#[derive(Clone, Copy)]
struct Ball {
    center: Vec3,
    radius: f64,
}

impl ChainSkips {
    fn new(tree: &Bsp) -> Option<Self> {
        let count = tree.nodes.len();
        if count < SKIP_MIN_NODES || count > u32::MAX as usize / 2 {
            return None;
        }
        #[cfg(test)]
        if SKIPS_DISABLED.with(|disabled| disabled.get()) {
            return None;
        }
        // The middle of the tree's bounding box: inside a convex solid, and
        // unlike a vertex mean not pulled towards densely tessellated parts.
        let mut min = Vec3::new(f64::INFINITY, f64::INFINITY, f64::INFINITY);
        let mut max = Vec3::new(f64::NEG_INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for node in &tree.nodes {
            for polygon in &node.polygons {
                for vertex in &polygon.vertices {
                    min = Vec3::new(min.x.min(vertex.x), min.y.min(vertex.y), min.z.min(vertex.z));
                    max = Vec3::new(max.x.max(vertex.x), max.y.max(vertex.y), max.z.max(vertex.z));
                }
            }
        }
        let center = if min.x <= max.x { min.add(max).mul(0.5) } else { Vec3::ZERO };
        Some(Self {
            center,
            memo: Default::default(),
        })
    }

    /// The run of `level` from `start` along `side`.
    ///
    /// A run only ever follows links that exist and nodes that have a plane,
    /// and a build never changes either once set, so a memoised run stays
    /// valid while the tree grows (at worst shorter than it could now be).
    fn run(&mut self, tree: &Bsp, level: usize, side: usize, start: usize) -> Skip {
        if let Some(run) = self.memo[level][side].get(&(start as u32)) {
            return *run;
        }
        let mut parts = [Skip::empty(start); SKIP_RUN];
        let mut taken = 0;
        let mut at = start;
        while taken < SKIP_RUN {
            let part = if level == 0 {
                let node = &tree.nodes[at];
                let (Some(plane), Some(next)) =
                    (node.plane, if side == 0 { node.back } else { node.front })
                else {
                    break;
                };
                let toward = plane.distance(self.center);
                Skip {
                    target: next as u32,
                    len: 1,
                    high: toward,
                    low: toward,
                    normal: plane.normal,
                    spread: 0.0,
                    norm: plane.normal.length(),
                }
            } else {
                let part = self.run(tree, level - 1, side, at);
                if part.len == 0 {
                    break;
                }
                part
            };
            parts[taken] = part;
            taken += 1;
            at = part.target as usize;
        }
        let run = Skip::joined(start, &parts[..taken]);
        self.memo[level][side].insert(start as u32, run);
        run
    }

    fn ball(polygons: &[Polygon]) -> Ball {
        let mut min = Vec3::new(f64::INFINITY, f64::INFINITY, f64::INFINITY);
        let mut max = Vec3::new(f64::NEG_INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for polygon in polygons {
            for vertex in &polygon.vertices {
                min = Vec3::new(min.x.min(vertex.x), min.y.min(vertex.y), min.z.min(vertex.z));
                max = Vec3::new(max.x.max(vertex.x), max.y.max(vertex.y), max.z.max(vertex.z));
            }
        }
        let center = min.add(max).mul(0.5);
        let mut farthest = 0.0f64;
        for polygon in polygons {
            for vertex in &polygon.vertices {
                let offset = vertex.sub(center);
                farthest = farthest.max(offset.dot(offset));
            }
        }
        Ball { center, radius: farthest.sqrt() }
    }

    /// Whether every vertex within `ball` is strictly on `side` of every
    /// plane of `run`.
    fn passes(&self, run: &Skip, side: usize, ball: Ball) -> bool {
        if run.len == 0 {
            return false;
        }
        let offset = ball.center.sub(self.center);
        let distance = offset.length();
        let reach = distance + ball.radius;
        // Rounding in the distances these bounds stand for is far below this.
        let margin = EPSILON + 1.0e-9 * (1.0 + reach + self.center.length());
        // How far `n.v - w` can stray from `n.c - w` over the set. With
        // `n = mean + e`, `|e| <= spread`, `|n| <= norm` and
        // `v - c = (p - c) + (v - p)`:
        //   n.(v - c) <= mean.(p - c) + |mean| r + spread (d + r)
        //   n.(v - c) <= mean.(p - c) + spread d + norm r
        //   n.(v - c) <= norm (d + r)
        // and the same, negated, from below.
        let along = run.normal.dot(offset);
        let wobble = (run.normal.length() * ball.radius + run.spread * reach)
            .min(run.spread * distance + run.norm * ball.radius);
        let bound = run.norm * reach;
        if side == 0 {
            run.high + (along + wobble).min(bound) < -margin
        } else {
            run.low + (along - wobble).max(-bound) > margin
        }
    }

    /// Where a set inside `ball`, standing at `index`, ends up after every
    /// run it would pass through unchanged; `index` itself when there is
    /// none.
    fn advance(&mut self, tree: &Bsp, mut index: usize, ball: Ball) -> usize {
        'jump: loop {
            for side in 0..2 {
                // A longer run starts with the shorter one's planes, so it is
                // only worth building once the shorter one passes.
                let mut best = None;
                for level in 0..SKIP_LEVELS {
                    let run = self.run(tree, level, side, index);
                    if !self.passes(&run, side, ball) {
                        break;
                    }
                    best = Some(run.target as usize);
                }
                if let Some(target) = best {
                    #[cfg(test)]
                    SKIPS_TAKEN.with(|taken| taken.set(taken.get() + 1));
                    index = target;
                    continue 'jump;
                }
            }
            return index;
        }
    }
}

/// Smallest set the support peel in [`Bsp::build`] runs on. Below this the
/// step-by-step chain is cheap, and the trees of ordinary primitives are
/// built exactly as before.
const PEEL_MIN_POLYGONS: usize = 256;

/// Vertices per leaf of the [`VertexTree`].
const VERTEX_LEAF: usize = 16;

/// A bounding-volume hierarchy over the vertices of a polygon set, answering
/// "does any vertex lie in front of this plane, and which lie on it" in time
/// proportional to the vertices near the plane rather than the whole set.
struct VertexTree {
    /// `(vertex, index of its polygon)`, permuted so every node is a range.
    points: Vec<(Vec3, usize)>,
    nodes: Vec<VertexNode>,
}

struct VertexNode {
    min: Vec3,
    max: Vec3,
    /// An oriented box as well: `axes[k]` is orthonormal, and every point's
    /// `axes[k]` component lies in `low[k]..=high[k]`. The first axis is the
    /// mean normal of the points' polygons, so on a smooth surface the box
    /// is thin across it, and a plane tangent near the node is not crossed
    /// by the box the way an axis-aligned one is.
    axes: [Vec3; 3],
    low: [f64; 3],
    high: [f64; 3],
    start: usize,
    end: usize,
    /// Child node indices; leaves have none.
    children: Option<(usize, usize)>,
}

impl VertexTree {
    fn new(polygons: &[Polygon]) -> Self {
        let points: Vec<(Vec3, usize)> = polygons
            .iter()
            .enumerate()
            .flat_map(|(index, polygon)| {
                polygon.vertices.iter().map(move |vertex| (*vertex, index))
            })
            .collect();
        let mut tree = Self {
            points,
            nodes: Vec::new(),
        };
        let normals: Vec<Vec3> = polygons
            .iter()
            .map(|polygon| polygon.plane.normal)
            .collect();
        let count = tree.points.len();
        tree.build(0, count, &normals);
        tree
    }

    fn build(&mut self, start: usize, end: usize, normals: &[Vec3]) -> usize {
        let mut min = Vec3::new(f64::INFINITY, f64::INFINITY, f64::INFINITY);
        let mut max = Vec3::new(f64::NEG_INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        let mut mean = Vec3::ZERO;
        for (point, polygon) in &self.points[start..end] {
            min = Vec3::new(min.x.min(point.x), min.y.min(point.y), min.z.min(point.z));
            max = Vec3::new(max.x.max(point.x), max.y.max(point.y), max.z.max(point.z));
            mean = mean.add(normals[*polygon]);
        }
        let w = if mean.length() > 1e-9 {
            mean.normalized()
        } else {
            Vec3::new(0.0, 0.0, 1.0)
        };
        let helper = if w.x.abs() < 0.6 {
            Vec3::new(1.0, 0.0, 0.0)
        } else {
            Vec3::new(0.0, 1.0, 0.0)
        };
        let u = w.cross(helper).normalized();
        let v = w.cross(u);
        let axes = [w, u, v];
        let mut low = [f64::INFINITY; 3];
        let mut high = [f64::NEG_INFINITY; 3];
        for (point, _) in &self.points[start..end] {
            for k in 0..3 {
                let value = axes[k].dot(*point);
                low[k] = low[k].min(value);
                high[k] = high[k].max(value);
            }
        }
        let node = self.nodes.len();
        self.nodes.push(VertexNode {
            min,
            max,
            axes,
            low,
            high,
            start,
            end,
            children: None,
        });
        if end - start > VERTEX_LEAF {
            let size = max.sub(min);
            let axis = if size.x >= size.y && size.x >= size.z {
                0
            } else if size.y >= size.z {
                1
            } else {
                2
            };
            let key = move |point: &(Vec3, usize)| match axis {
                0 => point.0.x,
                1 => point.0.y,
                _ => point.0.z,
            };
            let middle = (start + end) / 2;
            self.points[start..end]
                .select_nth_unstable_by(middle - start, |a, b| key(a).total_cmp(&key(b)));
            let left = self.build(start, middle, normals);
            let right = self.build(middle, end, normals);
            self.nodes[node].children = Some((left, right));
        }
        node
    }

    /// The polygons owning a vertex that is not clearly behind `plane`
    /// (further than `-2 * EPSILON` from it), appended to `near` with
    /// repeats. Every polygon left out has all its vertices more than
    /// `2 * EPSILON` behind, so it — and any fragment of it, whose vertices
    /// lie on its edges — classifies `BACK`. Returns `false`, abandoning the
    /// search, once more than `limit` vertices lie more than `EPSILON` in
    /// front.
    fn near_or_front(
        &self,
        plane: Plane,
        limit: usize,
        near: &mut Vec<usize>,
        stack: &mut Vec<usize>,
    ) -> bool {
        let mut in_front = 0usize;
        stack.clear();
        stack.push(0);
        while let Some(index) = stack.pop() {
            let node = &self.nodes[index];
            // The box's furthest corner along the normal bounds every vertex
            // in it; the extra EPSILON absorbs the rounding of evaluating the
            // plane at a corner instead of at the vertex.
            let corner = Vec3::new(
                if plane.normal.x >= 0.0 { node.max.x } else { node.min.x },
                if plane.normal.y >= 0.0 { node.max.y } else { node.min.y },
                if plane.normal.z >= 0.0 { node.max.z } else { node.min.z },
            );
            if plane.distance(corner) < -3.0 * EPSILON {
                continue;
            }
            let mut support = 0.0;
            for k in 0..3 {
                let along = plane.normal.dot(node.axes[k]);
                support += (along * node.low[k]).max(along * node.high[k]);
            }
            if support - plane.offset < -3.0 * EPSILON {
                continue;
            }
            match node.children {
                Some((left, right)) => {
                    stack.push(left);
                    stack.push(right);
                }
                None => {
                    for (point, polygon) in &self.points[node.start..node.end] {
                        let distance = plane.distance(*point);
                        if distance >= -2.0 * EPSILON {
                            near.push(*polygon);
                            if distance > EPSILON {
                                in_front += 1;
                                if in_front > limit {
                                    return false;
                                }
                            }
                        }
                    }
                }
            }
        }
        true
    }
}

/// Vertices a plane may have in front of it and still head a node of the
/// chain [`peel_chain`] lays down.
const PEEL_FRONT_VERTICES: usize = 24;

/// One node of the chain [`peel_chain`] lays down.
struct ChainNode {
    plane: Plane,
    /// Polygons coplanar with `plane`: the node's payload.
    filed: Vec<Polygon>,
    /// What classified in front of `plane`: the front child's set.
    front: Vec<Polygon>,
}

/// The first steps of the autopartition of a nearly convex set, without
/// classifying the whole set at every step.
///
/// On a convex surface every face plane has the whole set behind it, and on a
/// nearly convex one (a smooth scan, float-rounded vertices, a high-`$fn`
/// sphere) all but a handful of polygons. The step-by-step build then makes a
/// chain as long as the set, re-classifying every remaining polygon at every
/// level: quadratic, minutes at 200k triangles. Here a vertex hierarchy finds
/// the few polygons a plane does not have clearly behind it, and only those
/// are classified, so a step costs what lies near its plane.
///
/// Each returned node is exactly the step `build` would take on the set that
/// reaches it with that plane: the polygons classifying `COPLANAR` are filed,
/// those in front (and front fragments of spanning ones) go to the front
/// child, and everything else continues to the back child, the next node.
/// Only the order of planes differs: input order, taking a polygon's plane
/// only when it has at most [`PEEL_FRONT_VERTICES`] vertices in front, so
/// every front child is small. `rest`, what no node took, continues below the
/// last node; it is the input unchanged when no node was made.
fn peel_chain(polygons: Vec<Polygon>) -> (Vec<ChainNode>, Vec<Polygon>) {
    let tree = VertexTree::new(&polygons);
    let mut current: Vec<Option<Polygon>> = polygons.into_iter().map(Some).collect();
    let mut chain = Vec::new();
    let mut near = Vec::new();
    let mut stack = Vec::new();
    let (mut coplanar_front, mut coplanar_back) = (Vec::new(), Vec::new());
    let (mut front, mut back) = (Vec::new(), Vec::new());
    for head in 0..current.len() {
        if head % 1024 == 1023 {
            if deadline_passed() {
                deadline_exhausted();
            }
            if MEMORY_PRESSURE.load(Ordering::Relaxed) {
                memory_exhausted();
            }
        }
        let Some(polygon) = current[head].as_ref() else {
            continue;
        };
        let plane = polygon.plane;
        // A sliver whose stored plane misses its own vertices is not filed
        // on that plane by the step-by-step build either.
        if classify_polygon(plane, polygon) != COPLANAR {
            continue;
        }
        near.clear();
        if !tree.near_or_front(plane, PEEL_FRONT_VERTICES, &mut near, &mut stack) {
            continue;
        }
        near.sort_unstable();
        near.dedup();
        let mut node = ChainNode {
            plane,
            filed: Vec::new(),
            front: Vec::new(),
        };
        for &index in &near {
            let Some(polygon) = current[index].take() else {
                continue;
            };
            split_polygon_owned(
                plane,
                polygon,
                &mut coplanar_front,
                &mut coplanar_back,
                &mut front,
                &mut back,
            );
            node.filed.append(&mut coplanar_front);
            node.filed.append(&mut coplanar_back);
            node.front.append(&mut front);
            // A whole polygon or a spanning one's back fragment stays in
            // the set; a fragment too small to keep is dropped, as `build`
            // drops it.
            current[index] = back.pop();
            debug_assert!(back.is_empty());
            back.clear();
        }
        debug_assert!(!node.filed.is_empty());
        chain.push(node);
    }
    let rest = current.into_iter().flatten().collect();
    (chain, rest)
}

/// Picks a splitting plane that keeps the tree shallow.
///
/// Sampling a bounded number of candidates and scoring them by
/// `8 * splits + |front - back|` avoids the quadratic blow-up of always taking
/// the first polygon's plane, which matters because boolean inputs here are
/// frequently long axis-aligned runs of coplanar faces.
/// Picks a splitting plane, also reporting whether the chosen plane left the whole
/// sample on one side unsplit: the step that, repeated, builds a chain.
fn choose_plane_scored(polygons: &[Polygon]) -> (Plane, bool) {
    if polygons.len() <= 2 {
        return (polygons[0].plane, false);
    }
    let stride = (polygons.len() / SPLIT_CANDIDATES).max(1);
    let mut best = polygons[0].plane;
    let mut best_score = f64::INFINITY;
    let mut one_sided = false;
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
            one_sided = splits == 0 && (front == 0 || back == 0);
        }
        index += stride;
    }
    (best, one_sided)
}

/// `FRONT` or `BACK` when every polygon classifies so, else `None`.
fn one_side(plane: Plane, polygons: &[Polygon]) -> Option<u8> {
    let first = classify_polygon(plane, polygons.first()?);
    if first != FRONT && first != BACK {
        return None;
    }
    polygons[1..]
        .iter()
        .all(|polygon| classify_polygon(plane, polygon) == first)
        .then_some(first)
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
            push_fragment(front_vertices, &polygon, front);
            push_fragment(back_vertices, &polygon, back);
        }
    }
    polygon_kind
}

fn push_fragment(vertices: Vec<Vec3>, source: &Polygon, output: &mut Vec<Polygon>) {
    if vertices.len() < 3 {
        return;
    }
    let mut fragment = Polygon::with_plane(vertices, source.plane);
    fragment.color = source.color;
    // A split can produce a zero-area sliver when the cut grazes a corner;
    // dropping it here keeps the boolean output free of degenerate facets.
    if !fragment.simplify(EPSILON) {
        return;
    }
    // Only an *exactly* flat fragment may go. This used to drop anything
    // under EPSILON in area, but area is the wrong test: a sliver 1.6e-4 mm
    // long and 1.1e-5 mm wide has an area of 1e-9 mm^2 and corners well
    // beyond welding distance of one another, so dropping it left a hole its
    // neighbours could not close. Near scanned meshes, where cuts graze
    // vertex clusters, that was the single largest source of open results
    // (10 of 21 in a sweep of a sleeve's clearance). A thin sliver kept here
    // is merged into its face, or cleaned up, on export.
    if fragment.area() <= 0.0 {
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
    fn polygon_digest(polygons: &[Polygon]) -> Vec<[u64; 3]> {
        polygons
            .iter()
            .flat_map(|polygon| polygon.vertices.iter())
            .map(|vertex| [vertex.x.to_bits(), vertex.y.to_bits(), vertex.z.to_bits()])
            .collect()
    }

    fn without_skips<R>(body: impl FnOnce() -> R) -> R {
        SKIPS_DISABLED.with(|disabled| disabled.set(true));
        let result = body();
        SKIPS_DISABLED.with(|disabled| disabled.set(false));
        result
    }

    /// The chain shortcut must change only the time a boolean takes: every
    /// operation on nested and overlapping high-resolution spheres (chains,
    /// and sets inside, outside and across them) gives the same polygons, in
    /// the same order, with and without it.
    #[test]
    fn chain_skips_leave_every_boolean_bit_identical() {
        let outer = primitives::sphere(30.0, 64).polygons;
        let inner = primitives::sphere(29.0, 64).polygons;
        let shifted = primitives::sphere(30.0, 64)
            .transformed(super::super::mesh::Matrix4::translation(Vec3::new(12.0, 3.0, -2.0)))
            .polygons;
        type Op = fn(Vec<Polygon>, Vec<Polygon>) -> Vec<Polygon>;
        let operations: [(&str, Op); 3] = [("union", union), ("difference", difference), ("intersection", intersection)];
        for (a, b) in [(&outer, &inner), (&inner, &outer), (&outer, &shifted)] {
            for (name, operation) in operations {
                let fast = operation(a.clone(), b.clone());
                let plain = without_skips(|| operation(a.clone(), b.clone()));
                assert_eq!(
                    polygon_digest(&fast),
                    polygon_digest(&plain),
                    "{name} changed with the chain shortcut"
                );
            }
        }
    }

    /// Polygons with their planes, bit for bit.
    fn full_digest(polygons: &[Polygon]) -> Vec<u64> {
        let mut digest = Vec::new();
        for polygon in polygons {
            let n = polygon.plane.normal;
            digest.extend([n.x.to_bits(), n.y.to_bits(), n.z.to_bits(), polygon.plane.offset.to_bits(), polygon.vertices.len() as u64]);
            for v in &polygon.vertices {
                digest.extend([v.x.to_bits(), v.y.to_bits(), v.z.to_bits()]);
            }
        }
        digest
    }

    /// The randomized form of the property above: many scenes mixing nested
    /// and overlapping high-resolution spheres, cubes (aligned, so faces are
    /// coplanar, and rotated), rotated cylinders, convex hulls of random
    /// points and jittered triangulated spheres standing in for imported
    /// scans, combined by every operation and chained. Each must come out
    /// identical, planes included, with the shortcut on and off; and the
    /// shortcut must actually have fired, or the comparison proves nothing.
    #[test]
    fn chain_skips_are_bit_identical_on_random_scenes() {
        use super::super::mesh::Matrix4;
        use super::super::solid::Solid;
        let mut seed = 0x5851_f42d_4c95_7f2du64;
        let mut random = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let jittered_sphere = |radius: f64, fragments: usize, amount: f64| -> Solid {
            // Per-position jitter, so shared vertices stay shared.
            let jitter = |v: Vec3| -> Vec3 {
                let mut h = v.x.to_bits() ^ v.y.to_bits().rotate_left(21) ^ v.z.to_bits().rotate_left(42);
                h ^= h >> 33;
                h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
                h ^= h >> 33;
                v.mul(1.0 + amount * ((h >> 11) as f64 / (1u64 << 53) as f64 - 0.5))
            };
            let mut faces = Vec::new();
            for polygon in primitives::sphere(radius, fragments).polygons {
                let vertices: Vec<Vec3> = polygon.vertices.iter().map(|v| jitter(*v)).collect();
                for k in 1..vertices.len() - 1 {
                    faces.push(vec![vertices[0], vertices[k], vertices[k + 1]]);
                }
            }
            Solid::from_faces(faces)
        };
        let operand = |random: &mut dyn FnMut() -> f64| -> Vec<Polygon> {
            let at = Vec3::new(random() * 16.0 - 8.0, random() * 16.0 - 8.0, random() * 16.0 - 8.0);
            let solid = match (random() * 6.0) as usize {
                0 | 1 => primitives::sphere(10.0 + random() * 25.0, 48 + (random() * 100.0) as usize),
                2 => {
                    // Integer sizes and offsets: faces land on shared planes.
                    let size = Vec3::new((4.0 + random() * 30.0).round(), (4.0 + random() * 30.0).round(), (4.0 + random() * 30.0).round());
                    let cube = primitives::cube(size, random() < 0.5);
                    if random() < 0.5 {
                        return cube.transformed(Matrix4::translation(Vec3::new(at.x.round(), at.y.round(), at.z.round()))).polygons;
                    }
                    cube.transformed(Matrix4::rotation_x(random() * 90.0)).transformed(Matrix4::rotation_z(random() * 90.0))
                }
                3 => primitives::cylinder(10.0 + random() * 30.0, 5.0 + random() * 15.0, 2.0 + random() * 15.0, true, 64 + (random() * 120.0) as usize)
                    .transformed(Matrix4::rotation_y(random() * 180.0))
                    .transformed(Matrix4::rotation_x(random() * 180.0)),
                4 => {
                    let points: Vec<Solid> = (0..1)
                        .map(|_| {
                            let cloud: Vec<Vec<Vec3>> = (0..600)
                                .map(|_| {
                                    let (u, w) = (random() * std::f64::consts::TAU, random() * 2.0 - 1.0);
                                    let r = 15.0 + random() * 2.0;
                                    let s = (1.0 - w * w).sqrt();
                                    let p = Vec3::new(r * s * u.cos(), r * s * u.sin(), r * w);
                                    vec![p, p.add(Vec3::new(1e-3, 0.0, 0.0)), p.add(Vec3::new(0.0, 1e-3, 0.0))]
                                })
                                .collect();
                            Solid::from_faces(cloud)
                        })
                        .collect();
                    primitives::hull(&points)
                }
                _ => jittered_sphere(12.0 + random() * 15.0, 40 + (random() * 60.0) as usize, 0.02),
            };
            solid.transformed(Matrix4::translation(at)).polygons
        };
        type Op = fn(Vec<Polygon>, Vec<Polygon>) -> Vec<Polygon>;
        let operations: [Op; 3] = [union, difference, intersection];
        let mut scenes = 0;
        let mut skipped = 0;
        // Nested concentric spheres first: the case the shortcut exists for.
        let mut fixed: Vec<(Vec<Polygon>, Vec<Polygon>)> = vec![
            (primitives::sphere(30.0, 160).polygons, primitives::sphere(29.0, 160).polygons),
            (primitives::sphere(29.0, 120).polygons, primitives::sphere(30.0, 120).polygons),
            (
                primitives::sphere(30.0, 128).polygons,
                primitives::cube(Vec3::new(20.0, 20.0, 20.0), true).polygons,
            ),
            (jittered_sphere(30.0, 96, 0.01).polygons, jittered_sphere(28.0, 96, 0.01).polygons),
        ];
        for _ in 0..36 {
            fixed.push((operand(&mut random), operand(&mut random)));
        }
        for (a, b) in fixed {
            let op = operations[(random() * 3.0) as usize];
            let chained = random() < 0.4;
            let c = chained.then(|| operand(&mut random));
            let second = operations[(random() * 3.0) as usize];
            let run = || {
                let first = op(a.clone(), b.clone());
                match &c {
                    Some(c) => second(first, c.clone()),
                    None => first,
                }
            };
            let before = SKIPS_TAKEN.with(|taken| taken.get());
            let fast = run();
            if SKIPS_TAKEN.with(|taken| taken.get()) > before {
                skipped += 1;
            }
            let plain = without_skips(run);
            assert_eq!(full_digest(&fast), full_digest(&plain), "scene {scenes} changed with the chain shortcut");
            scenes += 1;
        }
        assert_eq!(scenes, 40);
        assert!(skipped >= 10, "the shortcut fired in only {skipped} of {scenes} scenes");
    }

    /// Two nested 200-segment spheres: before the shortcut every polygon of
    /// the inner one walked the outer one's 20k-node chain (seconds even in
    /// release, minutes at 400 segments). Now the walk is a handful of jumps.
    #[test]
    fn a_hollow_high_resolution_sphere_is_not_quadratic() {
        let outer = primitives::sphere(30.0, 200).polygons;
        let inner = primitives::sphere(29.0, 200).polygons;
        let started = std::time::Instant::now();
        let result = difference(outer, inner);
        let elapsed = started.elapsed();
        let volume = volume_of(&result);
        assert!((volume - 4.0 / 3.0 * std::f64::consts::PI * (30f64.powi(3) - 29f64.powi(3))).abs() < 60.0, "{volume}");
        assert!(elapsed < std::time::Duration::from_secs(30), "took {elapsed:?}");
    }

    #[test]
    fn a_set_cancellation_flag_aborts_a_build_with_the_cancel_message() {
        let flag = AtomicBool::new(true);
        let caught = std::panic::catch_unwind(|| {
            with_cancellation(Some(&flag), || {
                let _ = union(
                    primitives::sphere(10.0, 32).polygons,
                    primitives::sphere(9.0, 32).polygons,
                );
            })
        })
        .expect_err("a cancelled build must abort");
        let message = caught
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default();
        assert_eq!(message, format!("{BUDGET_PANIC_PREFIX}Render cancelled."));
        // Unwinding removed the flag again.
        assert!(!deadline_passed());
    }

    #[test]
    fn worker_threads_inherit_the_flags_they_are_handed() {
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let _scope = cancellation_scope(std::sync::Arc::clone(&flag));
        flag.store(true, Ordering::Release);
        Interrupts::with_current(|interrupts| {
            std::thread::scope(|scope| {
                scope
                    .spawn(|| interrupts.install(|| assert!(deadline_passed())))
                    .join()
                    .unwrap();
                // Without the hand-over the worker sees nothing.
                scope.spawn(|| assert!(!deadline_passed())).join().unwrap();
            });
        });
    }

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
