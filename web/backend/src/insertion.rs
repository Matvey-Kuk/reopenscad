//! Insertion check: does a moving mesh pass a set of fixed meshes along a
//! translation path?
//!
//! Continuous, not sampled. The path is cut into chunks and each chunk is
//! answered by one exact swept-volume query
//! ([`analysis::mesh_distance_swept`]): the minimum surface distance between
//! the fixed meshes and every triangle of the moving mesh swept along the
//! chunk (its Minkowski sum with the segment), i.e. the exact minimum
//! clearance over the whole chunk, with where it occurs.
//!
//! * A chunk whose swept clearance is 0 holds a contact; the first one is
//!   then located by conservative advancement inside the chunk: the
//!   clearance `f(s)` is 1-Lipschitz in arc length for a pure translation
//!   (every point moves exactly `ds`, the maximum-velocity bound), so from
//!   `s` it is safe to advance by `f(s)` and stepping `s += f(s)` can never
//!   jump over a contact. Contact is declared once `f <= CONTACT_EPSILON`.
//! * The first pose below tolerance is located by bisecting the first chunk
//!   whose swept clearance is below it, to `precision`.
//! * A query that runs out of work budget yields a proven lower bound; the
//!   chunk is halved until the verdicts are decided or the budget ends, and
//!   then the report says it is undetermined.
//!
//! Containment cannot change along a path whose surfaces never touch, so
//! "starts inside" is tested once, at the start pose.

use crate::vec3::{add, norm, scale, sub};
use std::time::Instant;

use crate::analysis::{self, Aabb, MeshIndex, P};

/// Surface distance (mm) treated as contact.
pub const CONTACT_EPSILON: f64 = 1e-4;
/// Most mesh distance queries (swept or at one pose, one per fixed object
/// actually queried) one check spends.
pub const MAX_EVALUATIONS: usize = 4000;
/// Most chunks the path is first cut into.
const MAX_CHUNKS: usize = 1000;
/// Triangle-pair tests one distance query may spend.
pub const QUERY_PAIR_TESTS: usize = 4_000_000;
/// Poses past the contact sampled for the penetration estimate.
const PENETRATION_POSES: usize = 8;
/// Vertices per side sampled for the penetration estimate.
const PENETRATION_VERTICES: usize = 3000;

/// A polyline of translation offsets parameterised by arc length.
pub struct Path {
    points: Vec<P>,
    /// Arc length at each point.
    lengths: Vec<f64>,
}

impl Path {
    /// A single point is a path of zero length.
    pub fn new(mut points: Vec<P>) -> Self {
        if points.len() == 1 {
            points.push(points[0]);
        }
        let mut lengths = Vec::with_capacity(points.len());
        let mut total = 0.0;
        for (index, point) in points.iter().enumerate() {
            if index > 0 {
                total += norm(sub(*point, points[index - 1]));
            }
            lengths.push(total);
        }
        Self { points, lengths }
    }

    pub fn length(&self) -> f64 {
        self.lengths.last().copied().unwrap_or(0.0)
    }

    /// Offset at arc length `s` on segment `segment`.
    fn on_segment(&self, s: f64, segment: usize) -> P {
        let (start, end) = (self.lengths[segment], self.lengths[segment + 1]);
        let span = end - start;
        let t = if span > 0.0 { ((s - start) / span).clamp(0.0, 1.0) } else { 0.0 };
        let a = self.points[segment];
        add(a, scale(sub(self.points[segment + 1], a), t))
    }

    /// Offset at arc length `s` and the index of the segment it lies on.
    pub fn at(&self, s: f64) -> (P, usize) {
        if self.points.len() < 2 {
            return (self.points.first().copied().unwrap_or([0.0; 3]), 0);
        }
        let s = s.clamp(0.0, self.length());
        let segment = (1..self.points.len())
            .find(|&index| s <= self.lengths[index])
            .unwrap_or(self.points.len() - 1)
            - 1;
        (self.on_segment(s, segment), segment)
    }
}

pub struct Options {
    pub tolerance: f64,
    /// Resolution (mm of travel) the first tolerance violation is located to.
    pub precision: f64,
    pub deadline: Instant,
    pub max_evaluations: usize,
    /// Triangle-pair tests one mesh query may spend ([`QUERY_PAIR_TESTS`]).
    pub pair_tests: usize,
}

/// Clearance at one pose, or the minimum over a stretch and where it is.
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    /// Arc length travelled.
    pub travel: f64,
    pub offset: P,
    pub segment: usize,
    /// Smallest distance found (a real pair at this distance exists).
    pub clearance: f64,
    /// Proven lower bound (equals `clearance` when the query was exact).
    pub lower_bound: f64,
    /// Fixed object realising it.
    pub object: usize,
    /// Closest point on the moving mesh (at this pose) and on the fixed one.
    pub point_a: P,
    pub point_b: P,
}

#[derive(Clone, Copy, Debug)]
pub struct Penetration {
    pub depth: f64,
    pub travel: f64,
    pub offset: P,
    pub point: P,
}

#[derive(Debug, Default)]
pub struct Report {
    pub length: f64,
    /// No contact anywhere along the path. `None`: undetermined.
    pub passes: Option<bool>,
    /// Clearance >= tolerance along the whole path (up to a collision).
    /// `None`: undetermined.
    pub clearance_ok: Option<bool>,
    /// First contact (`clearance <= CONTACT_EPSILON`).
    pub collision: Option<Sample>,
    /// The collision is at the start: the meshes already touch or overlap.
    pub starts_blocked: bool,
    /// At the start the moving mesh is inside a fixed one or vice versa.
    pub starts_inside: bool,
    pub penetration: Option<Penetration>,
    /// Smallest clearance along the path (to the collision, if any).
    pub min_clearance: Option<Sample>,
    /// Proven lower bound on the clearance over the checked stretch.
    pub min_lower_bound: f64,
    /// First pose (to `precision`) whose clearance is below tolerance.
    pub first_violation: Option<Sample>,
    /// How far along the path the check got.
    pub checked_to: f64,
    pub evaluations: usize,
    /// Why the result is incomplete, when it is.
    pub undetermined: Option<String>,
}

struct Checker<'a> {
    moving: &'a MeshIndex,
    fixed: &'a [MeshIndex],
    bounds: Vec<Aabb>,
    moving_bounds: Aabb,
    path: &'a Path,
    options: &'a Options,
    evaluations: usize,
}

fn box_distance(a: &Aabb, b: &Aabb) -> f64 {
    let mut total = 0.0;
    for axis in 0..3 {
        let gap = (a.min[axis] - b.max[axis]).max(b.min[axis] - a.max[axis]).max(0.0);
        total += gap * gap;
    }
    total.sqrt()
}

impl Checker<'_> {
    fn out_of_budget(&self) -> Option<String> {
        if self.evaluations >= self.options.max_evaluations {
            return Some(format!("query budget ({}) exhausted", self.options.max_evaluations));
        }
        if let Some(message) = crate::csg::bsp::interrupt_message() {
            return Some(if message.starts_with("Render cancelled") { "cancelled".into() } else { "time budget exhausted".into() });
        }
        if Instant::now() >= self.options.deadline {
            return Some("time budget exhausted".into());
        }
        None
    }

    /// Minimum clearance over arc lengths `[from, to]` of one segment (a
    /// single pose when they are equal).
    fn swept(&mut self, segment: usize, from: f64, to: f64) -> Sample {
        let start = self.path.on_segment(from, segment);
        let delta = sub(self.path.on_segment(to, segment), start);
        let mut swept_box = Aabb {
            min: add(self.moving_bounds.min, start),
            max: add(self.moving_bounds.max, start),
        };
        swept_box.grow(add(self.moving_bounds.min, add(start, delta)));
        swept_box.grow(add(self.moving_bounds.max, add(start, delta)));
        let mut order: Vec<(f64, usize)> = self
            .bounds
            .iter()
            .enumerate()
            .map(|(index, bounds)| (box_distance(&swept_box, bounds), index))
            .collect();
        order.sort_by(|x, y| x.0.total_cmp(&y.0));
        let mut best = Sample {
            travel: from,
            offset: start,
            segment,
            clearance: f64::INFINITY,
            lower_bound: f64::INFINITY,
            object: 0,
            point_a: start,
            point_b: start,
        };
        for (box_gap, index) in order {
            // Nothing in this box can beat the best real pair, and its box
            // gap is itself a valid lower bound for it.
            if box_gap >= best.clearance {
                best.lower_bound = best.lower_bound.min(box_gap);
                continue;
            }
            // Every mesh query is charged, so a query over many fixed objects
            // spends the budget it really uses.
            if self.evaluations >= self.options.max_evaluations {
                // Out of queries: only the box gap is proven for this one.
                best.lower_bound = best.lower_bound.min(box_gap);
                continue;
            }
            self.evaluations += 1;
            let Some(found) =
                analysis::mesh_distance_swept(self.moving, &self.fixed[index], start, delta, self.options.pair_tests)
            else {
                continue;
            };
            best.lower_bound = best.lower_bound.min(found.distance.lower_bound);
            if found.distance.distance < best.clearance {
                best.clearance = found.distance.distance;
                best.object = index;
                best.point_a = found.distance.point_a;
                best.point_b = found.distance.point_b;
                best.travel = from + found.at * (to - from);
                best.offset = add(start, scale(delta, found.at));
            }
        }
        best.lower_bound = best.lower_bound.min(best.clearance);
        best
    }

    /// First contact in `[from, to]` of `segment` by conservative
    /// advancement. `Err` when the budget ended first.
    fn first_contact(&mut self, segment: usize, from: f64, to: f64) -> Result<Option<Sample>, String> {
        let mut travel = from;
        loop {
            let sample = self.swept(segment, travel, travel);
            if sample.clearance <= CONTACT_EPSILON {
                return Ok(Some(sample));
            }
            if sample.lower_bound <= CONTACT_EPSILON {
                return Err("a clearance query ran out of its work budget near contact".into());
            }
            if travel >= to {
                return Ok(None);
            }
            if let Some(reason) = self.out_of_budget() {
                return Err(format!("{reason} while closing in on a contact"));
            }
            travel = (travel + sample.lower_bound).min(to);
        }
    }

    /// First pose in `[from, to]` whose clearance is below `tolerance`, to
    /// `precision`, given that the stretch holds one (`minimum`). The flag is
    /// false when it could not be narrowed to `precision` (budget, or a
    /// query too inexact to rule a half out).
    fn first_below(&mut self, segment: usize, mut from: f64, mut to: f64, minimum: Sample) -> (Sample, bool) {
        let tolerance = self.options.tolerance;
        // Invariant: [from, to] holds a pose below tolerance.
        let mut narrowed = false;
        while to - from > self.options.precision {
            if self.out_of_budget().is_some() {
                return (minimum, false);
            }
            let middle = 0.5 * (from + to);
            let left = self.swept(segment, from, middle);
            if left.clearance < tolerance {
                to = middle;
            } else if left.lower_bound >= tolerance {
                // Only a proven bound may discard the left half.
                from = middle;
            } else {
                return (minimum, false);
            }
            narrowed = true;
        }
        if !narrowed {
            return (minimum, true);
        }
        // The narrowest stretch's own minimum: its pose is within the
        // stretch, so within `precision` of the first violation.
        let last = self.swept(segment, from, to);
        (if last.clearance < tolerance { last } else { minimum }, true)
    }
}

/// Deepest vertex penetration over poses past the contact (a lower bound on
/// the interference if the motion were forced to the end of the path).
fn penetration(
    moving: &MeshIndex,
    fixed: &[MeshIndex],
    path: &Path,
    from: f64,
    object: usize,
    deadline: Instant,
) -> Option<Penetration> {
    let other = &fixed[object];
    let sample = |index: &MeshIndex| -> Vec<P> {
        let corners: Vec<P> = index.triangles.iter().flat_map(|t| t.iter().copied()).collect();
        let stride = corners.len().div_ceil(PENETRATION_VERTICES).max(1);
        corners.into_iter().step_by(stride).collect()
    };
    let (mine, theirs) = (sample(moving), sample(other));
    let remaining = path.length() - from;
    let mut best: Option<Penetration> = None;
    for step in 1..=PENETRATION_POSES {
        if Instant::now() >= deadline || crate::csg::bsp::deadline_passed() {
            break;
        }
        let travel = if remaining > 1e-9 {
            from + remaining * step as f64 / PENETRATION_POSES as f64
        } else {
            from
        };
        let (offset, _) = path.at(travel);
        let mut polled = 0usize;
        let mut stop = || {
            polled += 1;
            polled % 256 == 0 && (Instant::now() >= deadline || crate::csg::bsp::deadline_passed())
        };
        let mut consider = |depth: f64, point: P| {
            if best.is_none_or(|best| depth > best.depth) {
                best = Some(Penetration { depth, travel, offset, point });
            }
        };
        for vertex in &mine {
            if stop() {
                return best;
            }
            let point = add(*vertex, offset);
            if other.contains(point) {
                if let Some((depth, _)) = other.nearest(point) {
                    consider(depth, point);
                }
            }
        }
        for vertex in &theirs {
            if stop() {
                return best;
            }
            if moving.contains(sub(*vertex, offset)) {
                if let Some((depth, _)) = moving.nearest(sub(*vertex, offset)) {
                    consider(depth, *vertex);
                }
            }
        }
        if remaining <= 1e-9 {
            break;
        }
    }
    best
}

/// Runs the check. `path` offsets are applied to `moving`.
pub fn check(moving: &MeshIndex, fixed: &[MeshIndex], path: &Path, options: &Options) -> Report {
    let mut report = Report {
        length: path.length(),
        min_lower_bound: f64::INFINITY,
        ..Default::default()
    };
    if moving.is_empty() || fixed.iter().all(MeshIndex::is_empty) || path.points.is_empty() {
        report.passes = Some(true);
        report.clearance_ok = Some(true);
        report.checked_to = report.length;
        return report;
    }
    let mut checker = Checker {
        moving,
        fixed,
        bounds: fixed.iter().map(MeshIndex::bounds).collect(),
        moving_bounds: moving.bounds(),
        path,
        options,
        evaluations: 0,
    };
    let tolerance = options.tolerance;
    let points = path.points.len();

    // Start pose: already touching, overlapping or nested?
    let start = checker.swept(0, 0.0, 0.0);
    let offset0 = path.points[0];
    // One point-in-solid probe per shell and pair, capped and polled.
    let mut probes = 0usize;
    let mut probe_limit_hit = false;
    let mut probe = |solid: &MeshIndex, point: P| -> bool {
        probes += 1;
        if probes > analysis::MAX_CONTAINMENT_PROBES
            || (probes % 64 == 0 && (crate::csg::bsp::deadline_passed() || Instant::now() >= options.deadline))
        {
            probe_limit_hit = true;
            return false;
        }
        solid.contains(point)
    };
    let mut inside = None;
    'objects: for (index, other) in fixed.iter().enumerate() {
        let start_box = Aabb {
            min: add(checker.moving_bounds.min, offset0),
            max: add(checker.moving_bounds.max, offset0),
        };
        if box_distance(&start_box, &checker.bounds[index]) > 0.0 {
            continue;
        }
        for point in moving.shell_points() {
            if probe(other, add(*point, offset0)) {
                inside = Some(index);
                break 'objects;
            }
        }
        for point in other.shell_points() {
            if probe(moving, sub(*point, offset0)) {
                inside = Some(index);
                break 'objects;
            }
        }
    }
    checker.evaluations += probes / 64;
    let mut minimum = start;
    let mut lower = start.lower_bound;
    if let Some(index) = inside {
        report.starts_inside = true;
        report.starts_blocked = true;
        let mut collision = start;
        collision.object = index;
        collision.clearance = 0.0;
        collision.lower_bound = 0.0;
        report.collision = Some(collision);
    } else if start.clearance <= CONTACT_EPSILON {
        report.starts_blocked = true;
        report.collision = Some(start);
    } else if probe_limit_hit {
        report.undetermined = Some("the start-pose containment check ran out of its probe or time budget".into());
    }
    if start.clearance < tolerance {
        report.first_violation = Some(start);
    }

    // Chunks in path order, each split while its query is inconclusive.
    let chunk = (norm(sub(checker.moving_bounds.max, checker.moving_bounds.min)) / 4.0)
        .max(path.length() / MAX_CHUNKS as f64)
        .max(1e-6);
    let mut pending: Vec<(usize, f64, f64)> = Vec::new();
    for segment in (0..points.saturating_sub(1)).rev() {
        let (from, to) = (path.lengths[segment], path.lengths[segment + 1]);
        if to - from <= 0.0 {
            continue;
        }
        let pieces = ((to - from) / chunk).ceil().max(1.0) as usize;
        for piece in (0..pieces).rev() {
            let a = from + (to - from) * piece as f64 / pieces as f64;
            let b = from + (to - from) * (piece + 1) as f64 / pieces as f64;
            pending.push((segment, a, b));
        }
    }
    if !pending.is_empty() {
        // The chunks cover the start pose again, with their own bounds.
        lower = f64::INFINITY;
    }
    let mut checked_to = 0.0;
    let mut unresolved_violation = false;
    while report.collision.is_none() && report.undetermined.is_none() {
        let Some((segment, from, to)) = pending.pop() else { break };
        if let Some(reason) = checker.out_of_budget() {
            report.undetermined = Some(format!("{reason} at travel {from:.3} of {:.3} mm", path.length()));
            break;
        }
        let sample = checker.swept(segment, from, to);
        // Contact needs a strict margin; tolerance is met at equality.
        let clear_of_contact = sample.lower_bound > CONTACT_EPSILON;
        let tolerance_known = report.first_violation.is_some()
            || sample.lower_bound >= tolerance
            || sample.clearance < tolerance;
        let decided = sample.clearance <= CONTACT_EPSILON || (clear_of_contact && tolerance_known);
        if !decided {
            if to - from > options.precision {
                let middle = 0.5 * (from + to);
                pending.push((segment, middle, to));
                pending.push((segment, from, middle));
                continue;
            }
            report.undetermined = Some(format!(
                "clearance near travel {from:.3} mm could not be decided within the work budget (in [{:.4}, {:.4}] mm)",
                sample.lower_bound, sample.clearance
            ));
            lower = lower.min(sample.lower_bound);
            break;
        }
        if sample.clearance <= CONTACT_EPSILON {
            match checker.first_contact(segment, from, to) {
                Ok(Some(contact)) => {
                    if report.first_violation.is_none() && tolerance > CONTACT_EPSILON {
                        // The clearance crossed tolerance before the contact,
                        // somewhere in [from, contact].
                        let (violation, resolved) = checker.first_below(segment, from, contact.travel, contact);
                        report.first_violation = Some(violation);
                        if !resolved {
                            unresolved_violation = true;
                        }
                    }
                    report.collision = Some(contact);
                    checked_to = contact.travel;
                    break;
                }
                // The swept surfaces touch only at an end the advancement
                // reached without contact (rounding): treat as touching.
                Ok(None) => {}
                Err(reason) => {
                    report.undetermined = Some(reason);
                    break;
                }
            }
        }
        lower = lower.min(sample.lower_bound);
        if sample.clearance < minimum.clearance {
            minimum = sample;
        }
        if sample.clearance < tolerance && report.first_violation.is_none() {
            let (violation, resolved) = checker.first_below(segment, from, to, sample);
            report.first_violation = Some(violation);
            if !resolved {
                unresolved_violation = true;
            }
        }
        checked_to = to;
    }
    if report.collision.is_none() && report.undetermined.is_none() && !pending.is_empty() {
        report.undetermined = Some("stopped early".into());
    }
    if unresolved_violation && report.undetermined.is_none() {
        report.undetermined = Some(format!(
            "the first tolerance violation could not be located to {} mm of travel",
            options.precision
        ));
    }
    report.checked_to = checked_to;
    if let Some(collision) = report.collision {
        minimum = collision;
        lower = 0.0;
        report.first_violation.get_or_insert(collision);
        report.penetration = penetration(moving, fixed, path, collision.travel, collision.object, options.deadline);
    }
    report.evaluations = checker.evaluations;
    report.min_clearance = Some(minimum);
    report.min_lower_bound = lower.max(0.0);
    // The same conservative rules for every stretch, the start pose included
    // (a zero-length path is nothing but the start pose).
    let unfinished = report.undetermined.is_some();
    report.passes = if report.collision.is_some() {
        Some(false)
    } else if lower > CONTACT_EPSILON && !unfinished {
        Some(true)
    } else {
        None
    };
    report.clearance_ok = if report.collision.is_some() || minimum.clearance < tolerance {
        Some(false)
    } else if lower >= tolerance && !unfinished {
        Some(true)
    } else {
        None
    };
    if (report.passes.is_none() || report.clearance_ok.is_none()) && report.undetermined.is_none() {
        report.undetermined = Some(format!(
            "the clearance could not be proven: it lies in [{:.4}, {:.4}] mm",
            report.min_lower_bound, minimum.clearance
        ));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn index(source: &str) -> MeshIndex {
        let output = crate::engine::compile_parts(source, crate::engine::Quality::Preview, None).expect("compiles");
        let triangles: Vec<[P; 3]> = output
            .parts
            .iter()
            .flat_map(|part| {
                part.mesh
                    .triangles
                    .iter()
                    .map(|t| t.vertices.map(|v| [v.x, v.y, v.z]))
                    .collect::<Vec<_>>()
            })
            .collect();
        MeshIndex::new(triangles)
    }

    fn options(tolerance: f64) -> Options {
        Options {
            tolerance,
            precision: 0.01,
            deadline: Instant::now() + Duration::from_secs(60),
            max_evaluations: MAX_EVALUATIONS,
            pair_tests: QUERY_PAIR_TESTS,
        }
    }

    /// A 20 mm block with a square through-hole of side `hole` along Z.
    fn block(hole: f64) -> MeshIndex {
        index(&format!(
            "difference() {{ translate([-10,-10,0]) cube([20,20,20]); translate([-{h},-{h},-1]) cube([{s},{s},22]); }}",
            h = hole / 2.0,
            s = hole
        ))
    }

    /// A square peg of side `side`, 10 long, starting above the block.
    fn peg(side: f64) -> MeshIndex {
        index(&format!("translate([-{h},-{h},25]) cube([{s},{s},10]);", h = side / 2.0, s = side))
    }

    #[test]
    fn a_peg_with_clearance_passes_and_reports_the_gap() {
        let path = Path::new(vec![[0.0; 3], [0.0, 0.0, -40.0]]);
        let report = check(&peg(9.6), &[block(10.0)], &path, &options(0.15));
        assert_eq!(report.passes, Some(true), "{report:?}");
        assert_eq!(report.clearance_ok, Some(true), "{report:?}");
        let min = report.min_clearance.unwrap();
        assert!((min.clearance - 0.2).abs() < 0.011, "{min:?}");
        assert!(report.min_lower_bound >= 0.19 - 1e-9, "{report:?}");
        // The tight stretch is where the peg is inside the hole.
        assert!(min.travel > 5.0 && min.travel < 35.0, "{min:?}");
    }

    #[test]
    fn the_same_peg_violates_a_larger_tolerance_inside_the_hole() {
        let path = Path::new(vec![[0.0; 3], [0.0, 0.0, -40.0]]);
        let report = check(&peg(9.6), &[block(10.0)], &path, &options(0.3));
        assert_eq!(report.passes, Some(true));
        assert_eq!(report.clearance_ok, Some(false));
        let violation = report.first_violation.expect("violation");
        // It starts as the peg's bottom enters the hole (travel 5).
        assert!(violation.travel > 4.6 && violation.travel < 5.5, "{violation:?}");
        assert!((violation.point_b[2] - 20.0).abs() < 0.5 || violation.point_b[2] < 20.0, "{violation:?}");
    }

    #[test]
    fn a_slightly_large_peg_collides_at_the_hole_mouth() {
        let path = Path::new(vec![[0.0; 3], [0.0, 0.0, -40.0]]);
        let report = check(&peg(10.2), &[block(10.0)], &path, &options(0.2));
        assert_eq!(report.passes, Some(false));
        let collision = report.collision.unwrap();
        assert!((collision.travel - 5.0).abs() < 1e-3, "{collision:?}");
        assert!((collision.point_b[2] - 20.0).abs() < 1e-3, "{collision:?}");
        let depth = report.penetration.expect("penetration").depth;
        // 0.1 per side; the peg's corners sit sqrt(2) * 0.1 inside the
        // hole's corner edges.
        assert!(depth > 0.099 && depth < 0.1415, "{depth}");
        assert!(!report.starts_blocked);
    }

    #[test]
    fn an_l_shaped_path_is_needed_to_reach_a_side_opening() {
        // An L-shaped channel: in from +Y at z 0..10, then down through a hole.
        let fixed = index(
            "difference() { translate([-20,-20,-20]) cube([40,40,30]);
               translate([-3,-3,-30]) cube([6,6,50]);
               translate([-3,-3,0]) cube([6,40,20]); }",
        );
        // A 5 mm cube, to the side of the block, at the slot height.
        let moving = index("translate([-2.5, 30, 2]) cube(5);");
        let straight = Path::new(vec![[0.0; 3], [0.0, -32.5, -22.0]]);
        let report = check(&moving, std::slice::from_ref(&fixed), &straight, &options(0.2));
        assert_eq!(report.passes, Some(false), "{report:?}");
        let l_path = Path::new(vec![[0.0; 3], [0.0, -32.5, 0.0], [0.0, -32.5, -22.0]]);
        let report = check(&moving, std::slice::from_ref(&fixed), &l_path, &options(0.2));
        assert_eq!(report.passes, Some(true), "{report:?}");
        assert_eq!(report.clearance_ok, Some(true), "{report:?}");
        let min = report.min_clearance.unwrap();
        assert!((min.clearance - 0.5).abs() < 0.011, "{min:?}");
    }

    #[test]
    fn a_round_peg_in_a_round_hole_is_proven_within_budget() {
        let fixed = index("difference() { cylinder(r = 10, h = 30, $fn = 64); translate([0,0,-1]) cylinder(r = 5, h = 32, $fn = 64); }");
        let moving = index("translate([0,0,35]) cylinder(r = 4.8, h = 20, $fn = 64);");
        let path = Path::new(vec![[0.0; 3], [0.0, 0.0, -50.0]]);
        let started = Instant::now();
        let report = check(&moving, std::slice::from_ref(&fixed), &path, &options(0.15));
        eprintln!("round peg: {} evaluations, {:?}", report.evaluations, started.elapsed());
        assert_eq!(report.passes, Some(true), "{report:?}");
        assert_eq!(report.clearance_ok, Some(true), "{report:?}");
        assert!(report.undetermined.is_none());
        let min = report.min_clearance.unwrap().clearance;
        assert!((min - 0.2).abs() < 0.012, "{min}");
    }

    #[test]
    fn the_swept_distance_is_the_minimum_over_the_motion() {
        let moving = index("rotate([10, 20, 30]) cube([3, 1, 2]);");
        let fixed = index("translate([6, 1, -1]) rotate([0, 0, 45]) cylinder(r = 1.5, h = 3, $fn = 12);");
        for delta in [[8.0, 0.0, 0.0], [5.0, 4.0, 1.0], [0.0, 0.0, 5.0], [10.0, 2.0, -0.5]] {
            let swept = analysis::mesh_distance_swept(&moving, &fixed, [0.0; 3], delta, usize::MAX).unwrap();
            let steps = 2000;
            let sampled = (0..=steps)
                .map(|step| {
                    let t = step as f64 / steps as f64;
                    analysis::mesh_distance_shifted(&moving, &fixed, scale(delta, t), usize::MAX).unwrap().distance
                })
                .fold(f64::INFINITY, f64::min);
            let resolution = norm(delta) / steps as f64;
            assert!(swept.distance.distance <= sampled + 1e-9, "{delta:?}: {swept:?} vs {sampled}");
            assert!(swept.distance.distance >= sampled - resolution, "{delta:?}: {swept:?} vs {sampled}");
            // The reported pose realises it.
            let at = analysis::mesh_distance_shifted(&moving, &fixed, scale(delta, swept.at), usize::MAX).unwrap();
            assert!((at.distance - swept.distance.distance).abs() < 1e-6, "{delta:?}: {swept:?} vs {at:?}");
        }
    }

    #[test]
    fn an_unresolved_start_pose_is_not_a_pass() {
        // A zero-length path is the start pose alone; with queries too
        // starved to prove the gap, nothing may be claimed.
        let path = Path::new(vec![[0.0, 0.0, -20.0]]);
        let mut starved = options(0.15);
        starved.pair_tests = 1;
        let report = check(&peg(9.6), &[block(10.0)], &path, &starved);
        assert_eq!(report.passes, None, "{report:?}");
        assert_eq!(report.clearance_ok, None, "{report:?}");
        assert!(report.undetermined.is_some());
        // Unstarved, the same pose is proven.
        let report = check(&peg(9.6), &[block(10.0)], &path, &options(0.15));
        assert_eq!((report.passes, report.clearance_ok), (Some(true), Some(true)), "{report:?}");
    }

    #[test]
    fn a_long_path_with_starved_queries_is_incomplete() {
        let path = Path::new(vec![[0.0; 3], [0.0, 0.0, -40.0]]);
        let mut starved = options(0.15);
        starved.pair_tests = 1;
        let report = check(&peg(9.6), &[block(10.0)], &path, &starved);
        assert!(report.passes != Some(true) || report.undetermined.is_none(), "{report:?}");
        assert!(report.undetermined.is_some(), "{report:?}");
    }

    #[test]
    fn a_gap_exactly_at_tolerance_is_decided() {
        let path = Path::new(vec![[0.0; 3], [0.0, 0.0, -40.0]]);
        let report = check(&peg(9.6), &[block(10.0)], &path, &options(0.2));
        assert!(report.undetermined.is_none(), "{report:?}");
        assert_eq!(report.clearance_ok, Some(true), "{report:?}");
    }

    #[test]
    fn the_tolerance_crossing_before_a_contact_is_located() {
        // Contact at travel 5 (the 10.2 peg lands on the hole mouth); the
        // gap drops below 0.2 at travel 4.8, in the same chunk.
        let path = Path::new(vec![[0.0; 3], [0.0, 0.0, -40.0]]);
        let report = check(&peg(10.2), &[block(10.0)], &path, &options(0.2));
        assert!((report.collision.unwrap().travel - 5.0).abs() < 1e-3);
        let violation = report.first_violation.unwrap();
        assert!((violation.travel - 4.8).abs() <= 0.011, "{violation:?}");
    }

    #[test]
    fn a_thin_part_cannot_tunnel_through_a_thin_wall() {
        // A 0.1 mm plate 40 mm wide (chunks ~10 mm) crossing a 0.1 mm wall
        // in one chunk: no pose sampled at chunk ends touches the wall.
        let moving = index("translate([-20,-20,0]) cube([40,40,0.1]);");
        let wall = index("translate([-30,-30,4.95]) cube([60,60,0.1]);");
        let path = Path::new(vec![[0.0; 3], [0.0, 0.0, 10.0]]);
        let report = check(&moving, std::slice::from_ref(&wall), &path, &options(0.0));
        assert_eq!(report.passes, Some(false), "{report:?}");
        assert!((report.collision.unwrap().travel - 4.85).abs() < 1e-3, "{report:?}");
    }

    #[test]
    fn ending_inside_or_swallowing_a_fixed_part_is_a_collision() {
        let small = index("cube(2);");
        let big = index("translate([-10,-10,-10]) cube(22);");
        // Small moving cube driven fully into the big fixed one.
        let path = Path::new(vec![[0.0, 0.0, 30.0], [0.0; 3]]);
        let report = check(&small, std::slice::from_ref(&big), &path, &options(0.0));
        assert_eq!(report.passes, Some(false), "{report:?}");
        assert!((report.collision.unwrap().travel - 18.0).abs() < 1e-3, "{report:?}");
        // Big moving cube swallowing the small fixed one.
        let path = Path::new(vec![[0.0, 0.0, 30.0], [0.0; 3]]);
        let report = check(&big, std::slice::from_ref(&small), &path, &options(0.0));
        assert_eq!(report.passes, Some(false), "{report:?}");
        assert!((report.collision.unwrap().travel - 18.0).abs() < 1e-3, "{report:?}");
    }

    #[test]
    fn every_fixed_object_query_is_charged() {
        // A bar threaded through ten rings: every ring's box overlaps the
        // sweep, so every ring is queried in each (single-chunk) sweep.
        let fixed: Vec<MeshIndex> = (0..10)
            .map(|i| index(&format!("translate([0,0,{}]) difference() {{ cylinder(r=10,h=1,$fn=16); translate([0,0,-1]) cylinder(r=6,h=3,$fn=16); }}", i * 2)))
            .collect();
        let moving = index("translate([-2,-2,-5]) cube([4,4,30]);");
        let path = Path::new(vec![[0.0; 3], [0.5, 0.0, 0.0]]);
        let report = check(&moving, &fixed, &path, &options(0.2));
        assert_eq!(report.passes, Some(true), "{report:?}");
        assert!(report.evaluations >= 20, "{report:?}");
        let mut capped = options(0.2);
        capped.max_evaluations = 12;
        let report = check(&moving, &fixed, &path, &capped);
        assert!(report.undetermined.is_some() && report.passes.is_none(), "{report:?}");
    }

    #[test]
    fn a_start_inside_is_blocked_at_once() {
        let path = Path::new(vec![[0.0; 3], [10.0, 0.0, 0.0]]);
        let report = check(&index("cube(2);"), &[index("translate([-5,-5,-5]) cube(20);")], &path, &options(0.2));
        assert_eq!(report.passes, Some(false));
        assert!(report.starts_inside && report.starts_blocked);
    }
}
