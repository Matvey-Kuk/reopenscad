//! 2D regions: point sets bounded by closed polygonal contours.
//!
//! This module owns everything the kernel needs *before* a shape becomes a
//! solid: the result of `square()`, `circle()`, `polygon()`, `offset()` and
//! `projection()` all live here as a [`Region2d`]. From a `Region2d` the rest
//! of the kernel builds caps for `linear_extrude()` / `rotate_extrude()` by
//! calling [`Region2d::triangulate`].
//!
//! # Representation
//!
//! A region is a list of closed contours. Outer boundaries wind
//! counter-clockwise, holes wind clockwise. The invariant that follows from
//! that choice is used *everywhere* below:
//!
//! > with CCW outers and CW holes, the material is always on the **left** of a
//! > directed edge, so the outward normal of the edge `p -> q` with unit
//! > direction `d` is always the right-hand normal `(d.y, -d.x)`.
//!
//! That single rule makes offsetting uniform: the same code offsets an outer
//! boundary and a hole without ever asking which one it is looking at.
//!
//! # Epsilon strategy
//!
//! Like [`super::mesh`], every tolerance here is *absolute*, in scene units
//! (millimetres for SCAD sources). Three constants cover all of it:
//!
//! * [`POINT_EPSILON`] (= [`super::mesh::EPSILON`], 1e-9) — two points are the
//!   same point. Chosen to match the kernel's vertex weld tolerance so a
//!   contour that survives here also survives `VertexWelder`.
//! * [`AREA_EPSILON`] (1e-12) — a signed area (or twice-area cross product)
//!   below this counts as zero. It is roughly `(1e-6)^2`: a sliver thinner
//!   than a nanometre over a millimetre-scale feature. Deliberately much
//!   larger than `POINT_EPSILON^2` (1e-18), because cross products of
//!   millimetre-scale coordinates carry far more rounding noise than the
//!   coordinates themselves.
//! * [`COLLINEAR_EPSILON`] (1e-9) — used *relatively*: a vertex is dropped
//!   when `|cross| <= COLLINEAR_EPSILON * max(|edge_in|, |edge_out|)`, i.e.
//!   when its perpendicular deviation from the straight line through its
//!   neighbours is below a nanometre. This mirrors `mesh::remove_collinear`.
//! * [`DIRECTION_EPSILON`] (1e-12) — the one tolerance here that is *not* a
//!   length: it is compared against the sine of the angle between two unit
//!   directions, so it carries no scene units at all.
//!
//! Nothing in this module iterates a `HashMap`, so every output ordering is a
//! deterministic function of the input ordering.

use super::mesh::EPSILON;

/// A point in the XY plane.
pub type Point2 = [f64; 2];

/// Two points closer than this are the same point.
const POINT_EPSILON: f64 = EPSILON;

/// A (twice-)area smaller than this counts as zero. See the module docs.
const AREA_EPSILON: f64 = 1.0e-12;

/// Relative tolerance for the collinearity test. See the module docs.
const COLLINEAR_EPSILON: f64 = 1.0e-9;

/// Tolerance on the sine of the angle between two *unit* directions. Used where
/// the question is which side of a ray a direction falls on, so it is a pure
/// angle and carries no length scale.
const DIRECTION_EPSILON: f64 = 1.0e-12;

/// Maximum ratio of a mitre point's distance from the original corner to
/// `|delta|`. Beyond this the corner is bevelled instead of mitred, which is
/// what stops a near-180-degree corner from shooting off to infinity. 2.0 is
/// the same default the Clipper offsetting library uses; a 90-degree corner
/// needs only `sqrt(2) ~= 1.414`, so square corners are always mitred exactly.
const MITER_LIMIT: f64 = 2.0;

// ---------------------------------------------------------------------------
// Small vector helpers. Point2 is a bare array so it stays `Copy` and cheap;
// these keep the geometry below readable.
// ---------------------------------------------------------------------------

#[inline]
fn sub(a: Point2, b: Point2) -> Point2 {
    [a[0] - b[0], a[1] - b[1]]
}

#[inline]
fn cross(a: Point2, b: Point2) -> f64 {
    a[0] * b[1] - a[1] * b[0]
}

#[inline]
fn dot(a: Point2, b: Point2) -> f64 {
    a[0] * b[0] + a[1] * b[1]
}

#[inline]
fn length(a: Point2) -> f64 {
    dot(a, a).sqrt()
}

#[inline]
fn distance(a: Point2, b: Point2) -> f64 {
    length(sub(a, b))
}

#[inline]
fn same_point(a: Point2, b: Point2) -> bool {
    distance(a, b) <= POINT_EPSILON
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Signed area of one contour; positive when counter-clockwise.
///
/// The contour is treated as closed: the edge from the last point back to the
/// first is included whether or not the caller repeated the first point.
pub fn signed_area(contour: &[Point2]) -> f64 {
    if contour.len() < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    for index in 0..contour.len() {
        let current = contour[index];
        let next = contour[(index + 1) % contour.len()];
        sum += current[0] * next[1] - next[0] * current[1];
    }
    sum * 0.5
}

/// Even-odd containment test.
///
/// Uses the standard half-open crossing rule (`a.y > py` differs from
/// `b.y > py`), which counts a ray passing exactly through a vertex once
/// rather than twice or zero times. Points *on* the boundary are not
/// classified reliably — callers pick representative points that are known not
/// to lie on the contour being tested.
pub fn point_in_contour(point: Point2, contour: &[Point2]) -> bool {
    if contour.len() < 3 {
        return false;
    }
    let mut inside = false;
    let count = contour.len();
    let mut previous = count - 1;
    for current in 0..count {
        let a = contour[current];
        let b = contour[previous];
        if (a[1] > point[1]) != (b[1] > point[1]) {
            let t = (point[1] - a[1]) / (b[1] - a[1]);
            if point[0] < a[0] + t * (b[0] - a[0]) {
                inside = !inside;
            }
        }
        previous = current;
    }
    inside
}

/// Inclusive point-in-triangle test for a CCW triangle.
///
/// Inclusive on purpose: a vertex sitting exactly on an ear's edge would make
/// the clipped polygon non-simple, so it must block the ear just like an
/// interior vertex does.
fn point_in_triangle(point: Point2, a: Point2, b: Point2, c: Point2) -> bool {
    cross(sub(b, a), sub(point, a)) >= -AREA_EPSILON
        && cross(sub(c, b), sub(point, b)) >= -AREA_EPSILON
        && cross(sub(a, c), sub(point, c)) >= -AREA_EPSILON
}

/// Drops points that repeat their predecessor (and the closing repeat of the
/// first point), so that the ring carries each vertex exactly once.
fn dedup_ring(points: &mut Vec<Point2>, tolerance: f64) {
    let mut output: Vec<Point2> = Vec::with_capacity(points.len());
    for point in points.drain(..) {
        if let Some(last) = output.last() {
            if distance(*last, point) <= tolerance {
                continue;
            }
        }
        output.push(point);
    }
    while output.len() >= 2 {
        let first = output[0];
        let last = output[output.len() - 1];
        if distance(first, last) <= tolerance {
            output.pop();
        } else {
            break;
        }
    }
    *points = output;
}

/// Drops vertices whose perpendicular deviation from the line through their
/// neighbours is below `tolerance` (relative to the longer adjacent edge).
/// Never reduces the ring below three points.
fn remove_collinear(points: &mut Vec<Point2>, tolerance: f64) {
    let mut changed = true;
    while changed && points.len() > 3 {
        changed = false;
        let mut index = 0;
        while index < points.len() && points.len() > 3 {
            let count = points.len();
            let previous = points[(index + count - 1) % count];
            let current = points[index];
            let next = points[(index + 1) % count];
            let incoming = sub(current, previous);
            let outgoing = sub(next, current);
            let deviation = cross(incoming, outgoing).abs();
            let base = length(incoming).max(length(outgoing));
            if base > 0.0 && deviation <= tolerance * base {
                points.remove(index);
                changed = true;
            } else {
                index += 1;
            }
        }
    }
}

/// Representative point for nesting tests: the contour's first vertex.
///
/// A *vertex* is the right choice rather than an interior point. Contours of a
/// well-formed region never intersect, so a vertex of contour A is
/// unambiguously inside or outside every other contour B. An interior point is
/// not safe: the centroid of an ear of a big outer square can easily land
/// inside a small hole, which would report the outer contour as nested.
fn representative_point(contour: &[Point2]) -> Option<Point2> {
    contour.first().copied()
}

/// Even-odd nesting depth of every contour: how many *other* contours contain
/// it. Depth 0 and every even depth is an outer boundary, odd depths are holes.
fn nesting_depths(contours: &[Vec<Point2>]) -> Vec<usize> {
    let representatives: Vec<Option<Point2>> =
        contours.iter().map(|c| representative_point(c)).collect();
    (0..contours.len())
        .map(|index| {
            let Some(point) = representatives[index] else {
                return 0;
            };
            (0..contours.len())
                .filter(|other| *other != index && point_in_contour(point, &contours[*other]))
                .count()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Region2d
// ---------------------------------------------------------------------------

/// A 2D point set as a list of closed contours.
/// Outer boundaries wind counter-clockwise, holes wind clockwise.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Region2d {
    pub contours: Vec<Vec<Point2>>,
}

impl Region2d {
    pub fn new(contours: Vec<Vec<Point2>>) -> Self {
        // Deliberately *not* normalised: callers that already know their
        // winding (a `circle()` primitive, say) should not pay for a nesting
        // analysis, and `normalize` stays an explicit, testable step.
        Self { contours }
    }

    pub fn is_empty(&self) -> bool {
        self.contours.iter().all(|contour| contour.len() < 3)
    }

    /// Total signed area (positive for a well-formed region).
    ///
    /// Holes wind clockwise and therefore contribute negatively, so this is
    /// the true covered area of a normalised region.
    pub fn area(&self) -> f64 {
        self.contours
            .iter()
            .map(|contour| signed_area(contour))
            .sum()
    }

    pub fn bounds(&self) -> Option<(Point2, Point2)> {
        let mut minimum: Option<Point2> = None;
        let mut maximum: Option<Point2> = None;
        for contour in &self.contours {
            for point in contour {
                minimum = Some(match minimum {
                    None => *point,
                    Some(current) => [current[0].min(point[0]), current[1].min(point[1])],
                });
                maximum = Some(match maximum {
                    None => *point,
                    Some(current) => [current[0].max(point[0]), current[1].max(point[1])],
                });
            }
        }
        Some((minimum?, maximum?))
    }

    /// Removes duplicate/collinear points and degenerate contours, then fixes
    /// each contour's winding from its even-odd nesting depth: contours at
    /// even depth become CCW (outer), odd depth become CW (holes).
    pub fn normalize(&mut self) {
        let mut cleaned: Vec<Vec<Point2>> = Vec::with_capacity(self.contours.len());
        for contour in self.contours.drain(..) {
            let mut contour = contour;
            dedup_ring(&mut contour, POINT_EPSILON);
            if contour.len() < 3 {
                continue;
            }
            remove_collinear(&mut contour, COLLINEAR_EPSILON);
            if contour.len() < 3 {
                continue;
            }
            if signed_area(&contour).abs() <= AREA_EPSILON {
                continue;
            }
            cleaned.push(contour);
        }

        let depths = nesting_depths(&cleaned);
        for (contour, depth) in cleaned.iter_mut().zip(depths) {
            let wants_counter_clockwise = depth % 2 == 0;
            let is_counter_clockwise = signed_area(contour) > 0.0;
            if is_counter_clockwise != wants_counter_clockwise {
                contour.reverse();
            }
        }
        self.contours = cleaned;
    }

    /// Ear-clipping triangulation honouring holes. Triangles wind CCW.
    pub fn triangulate(&self) -> Vec<[Point2; 3]> {
        // Work on a normalised copy: the ear clipper relies on CCW outers and
        // CW holes, and the caller's region may be raw user input.
        let mut region = self.clone();
        region.normalize();
        let depths = nesting_depths(&region.contours);
        let representatives: Vec<Option<Point2>> = region
            .contours
            .iter()
            .map(|contour| representative_point(contour))
            .collect();

        let mut triangles = Vec::new();
        for (index, outer) in region.contours.iter().enumerate() {
            if depths[index] % 2 != 0 {
                continue;
            }
            // A hole of `outer` is any contour one level deeper that `outer`
            // contains. Because contours never intersect, that pair of
            // conditions identifies the immediate parent uniquely.
            let mut holes: Vec<Vec<Point2>> = Vec::new();
            for (other, candidate) in region.contours.iter().enumerate() {
                if other == index || depths[other] != depths[index] + 1 {
                    continue;
                }
                let Some(point) = representatives[other] else {
                    continue;
                };
                if point_in_contour(point, outer) {
                    holes.push(candidate.clone());
                }
            }
            triangles.extend(triangulate_with_holes(outer, &holes));
        }
        triangles
    }

    /// Grow (delta > 0) or shrink (delta < 0) the region.
    ///
    /// `rounded == true` reproduces OpenSCAD `offset(r=...)`: convex corners
    /// become circular arcs approximated with `fragments` segments per full
    /// circle. `rounded == false` reproduces `offset(delta=..., chamfer=false)`:
    /// convex corners are mitred (extended to the intersection of the offset
    /// edges), with a miter limit to avoid spikes.
    ///
    /// # Limitations
    ///
    /// This is a *local* offset: every contour is offset independently and no
    /// global polygon-clipping pass resolves the result. A large positive
    /// delta on a thin concave feature can therefore leave a self-overlapping
    /// contour, and a large negative delta on a shape with a narrow waist can
    /// leave two lobes joined by a crossed neck.
    ///
    /// Two guards do catch the common collapses:
    ///
    /// * **Consumed edges.** An offset edge that ends up running *backwards*
    ///   relative to its source edge was eaten by the offset. Its far endpoint
    ///   is pruned from the source contour and the offset is retried, so a
    ///   feature thinner than `2 * |delta|` erodes away instead of turning
    ///   into a crossed spike. `offset(-r)` on a shape with inradius `<= r`
    ///   therefore returns an empty region.
    /// * **Inverted contours.** If a contour's signed area still flips sign
    ///   relative to its source, or falls to zero, it is dropped outright.
    ///
    /// What is *not* handled is a contour that self-intersects without any
    /// single edge reversing — two separate lobes overlapping after a large
    /// grow, say. Resolving that needs a real polygon union and belongs in a
    /// clipping pass above this module.
    pub fn offset(&self, delta: f64, rounded: bool, fragments: usize) -> Region2d {
        let mut source = self.clone();
        source.normalize();
        if delta.abs() <= POINT_EPSILON {
            return source;
        }

        let mut result = Region2d::default();
        for contour in &source.contours {
            let original_area = signed_area(contour);
            let mut offsetted = offset_contour_resolved(contour, delta, rounded, fragments);
            dedup_ring(&mut offsetted, POINT_EPSILON);
            if offsetted.len() < 3 {
                continue;
            }
            remove_collinear(&mut offsetted, COLLINEAR_EPSILON);
            if offsetted.len() < 3 {
                continue;
            }
            let area = signed_area(&offsetted);
            if area.abs() <= AREA_EPSILON {
                continue;
            }
            // Winding flipped => the contour turned itself inside out, which
            // means the feature was consumed by the offset. Drop it.
            if (area > 0.0) != (original_area > 0.0) {
                continue;
            }
            result.contours.push(offsetted);
        }
        result
    }
}

// ---------------------------------------------------------------------------
// Offsetting
// ---------------------------------------------------------------------------

/// Offsets a contour, pruning features the offset consumed.
///
/// [`offset_contour`] reports, per source edge, whether that edge survived the
/// corner joins. A reversed edge means the offset ate straight through the
/// feature it belonged to, and leaving it in would produce a crossed contour.
/// Rather than emit garbage, *both* endpoints of every consumed edge are
/// deleted from the source contour (an edge that vanished takes its two
/// corners with it), the remainder is re-simplified, and the offset is
/// retried. A thin arm therefore erodes away over a few rounds — the seam it
/// leaves behind is a run of collinear points that `remove_collinear` folds
/// back into the surviving wall — and a shape whose inradius is smaller than
/// `|delta|` erodes to nothing.
///
/// Each round deletes at least one source vertex, so the loop is bounded by
/// the vertex count and cannot spin.
fn offset_contour_resolved(
    contour: &[Point2],
    delta: f64,
    rounded: bool,
    fragments: usize,
) -> Vec<Point2> {
    let mut work: Vec<Point2> = contour.to_vec();
    for _ in 0..=contour.len() {
        if work.len() < 3 {
            return Vec::new();
        }
        let (points, survives) = offset_contour(&work, delta, rounded, fragments);
        if survives.iter().all(|kept| *kept) {
            return points;
        }
        let mut keep = vec![true; work.len()];
        for (index, kept) in survives.iter().enumerate() {
            if !kept {
                keep[index] = false;
                keep[(index + 1) % work.len()] = false;
            }
        }
        let mut pruned: Vec<Point2> = work
            .iter()
            .enumerate()
            .filter(|(index, _)| keep[*index])
            .map(|(_, point)| *point)
            .collect();
        if pruned.len() == work.len() {
            return Vec::new(); // no progress: give up rather than loop
        }
        dedup_ring(&mut pruned, POINT_EPSILON);
        remove_collinear(&mut pruned, COLLINEAR_EPSILON);
        work = pruned;
    }
    Vec::new()
}

/// Offsets a single contour, preserving its winding.
///
/// Returns the offset points and a per-source-edge flag: `false` means the
/// corner joins at the edge's two ends crossed past each other, i.e. the edge
/// was completely consumed by a shrink.
fn offset_contour(
    contour: &[Point2],
    delta: f64,
    rounded: bool,
    fragments: usize,
) -> (Vec<Point2>, Vec<bool>) {
    let count = contour.len();
    if count < 3 {
        return (Vec::new(), Vec::new());
    }

    // Unit direction and outward normal of the edge leaving each vertex.
    // Material is always on the left of a directed edge (CCW outers, CW
    // holes), so the outward normal is the right-hand normal.
    let mut directions = vec![[0.0f64, 0.0f64]; count];
    let mut normals = vec![[0.0f64, 0.0f64]; count];
    for index in 0..count {
        let edge = sub(contour[(index + 1) % count], contour[index]);
        let size = length(edge);
        if size > POINT_EPSILON {
            let unit = [edge[0] / size, edge[1] / size];
            directions[index] = unit;
            normals[index] = [unit[1], -unit[0]];
        }
    }

    let mut output: Vec<Point2> = Vec::with_capacity(count * 2);
    // Index of the first and last output point contributed by each source
    // vertex. The offset of source edge `i` is then the segment from the last
    // point of vertex `i` to the first point of vertex `i + 1`.
    let mut first_of_vertex = vec![0usize; count];
    let mut last_of_vertex = vec![0usize; count];

    for index in 0..count {
        let previous = (index + count - 1) % count;
        let incoming_normal = normals[previous];
        let outgoing_normal = normals[index];
        let corner = contour[index];
        let begin = output.len();

        // A zero-length adjacent edge leaves one normal undefined; fall back to
        // the defined one so the vertex still contributes something sane.
        let incoming_valid = length(incoming_normal) > 0.5;
        let outgoing_valid = length(outgoing_normal) > 0.5;
        if !incoming_valid && !outgoing_valid {
            output.push(corner);
        } else {
            let incoming_normal = if incoming_valid {
                incoming_normal
            } else {
                outgoing_normal
            };
            let outgoing_normal = if outgoing_valid {
                outgoing_normal
            } else {
                incoming_normal
            };

            let start = [
                corner[0] + delta * incoming_normal[0],
                corner[1] + delta * incoming_normal[1],
            ];
            let end = [
                corner[0] + delta * outgoing_normal[0],
                corner[1] + delta * outgoing_normal[1],
            ];
            if same_point(start, end) {
                output.push(start);
            } else {
                let turn = cross(directions[previous], directions[index]);
                let along = dot(directions[previous], directions[index]);

                // `turn * delta > 0` is exactly "the two offset edges pull
                // apart at this corner and leave a wedge to fill": a left turn
                // while growing, or a right turn while shrinking. Otherwise the
                // offset edges overlap and their intersection is the join.
                if turn * delta > 0.0 && rounded {
                    push_arc(
                        &mut output,
                        corner,
                        start,
                        end,
                        turn,
                        along,
                        delta.abs(),
                        fragments,
                    );
                } else {
                    match miter_point(corner, incoming_normal, outgoing_normal, delta) {
                        Some(point) => output.push(point),
                        // Mitre limit hit (or a 180-degree spike): bevel the
                        // corner by keeping both offset endpoints.
                        None => {
                            output.push(start);
                            output.push(end);
                        }
                    }
                }
            }
        }

        // Every branch above pushes at least one point, so this range is valid.
        first_of_vertex[index] = begin;
        last_of_vertex[index] = output.len() - 1;
    }

    // An edge survives when its offset still runs in the original direction.
    // A reversed offset edge means the two corner joins crossed past each
    // other and the edge was eaten; that is the local signature of a collapse,
    // and unlike the signed-area test it also catches the symmetric case (a
    // square shrunk past its centre maps onto itself and keeps its winding).
    let mut survives = vec![true; count];
    for index in 0..count {
        let direction = directions[index];
        if length(direction) < 0.5 {
            continue;
        }
        let from = output[last_of_vertex[index]];
        let to = output[first_of_vertex[(index + 1) % count]];
        if dot(sub(to, from), direction) < -POINT_EPSILON {
            survives[index] = false;
        }
    }

    (output, survives)
}

/// Intersection of the two offset edge lines at a corner.
///
/// Both offset lines sit at signed distance `delta` from the corner along
/// their normals, so the intersection `q = p + v` satisfies `v . n1 = delta`
/// and `v . n2 = delta`. That 2x2 system has the closed form
/// `v = delta * (n1 + n2) / (1 + n1 . n2)`, whose magnitude is
/// `|delta| / cos(half the turn angle)` — i.e. exactly the mitre ratio.
///
/// Returns `None` when the corner reverses direction (denominator vanishes) or
/// when the mitre ratio exceeds [`MITER_LIMIT`]; the caller bevels instead.
fn miter_point(corner: Point2, first: Point2, second: Point2, delta: f64) -> Option<Point2> {
    let denominator = 1.0 + dot(first, second);
    if denominator <= 1.0e-12 {
        return None;
    }
    let scale = delta / denominator;
    let vector = [
        (first[0] + second[0]) * scale,
        (first[1] + second[1]) * scale,
    ];
    if length(vector) > MITER_LIMIT * delta.abs() {
        return None;
    }
    Some([corner[0] + vector[0], corner[1] + vector[1]])
}

/// Appends the polygonal arc that rounds a corner, from `start` to `end`
/// around `center`.
///
/// The sweep magnitude is the turn angle between the two edges (always in
/// `(0, pi)`), and the sweep direction is the sign of the turn: a left turn
/// sweeps CCW, a right turn CW. That single rule is correct for both signs of
/// delta, because for a shrink the rounded corners are the reflex ones.
///
/// The endpoints lie exactly on the offset circle (they must, to meet the
/// straight offset edges), but the interior samples are pushed out to the
/// *circumscribed* radius `radius / cos(step / 2)`. The polygonal arc
/// therefore encloses slightly more area than the exact circular arc — and
/// still less than the mitred corner — so `offset(r=)` never under-reports
/// material the way an inscribed approximation does. (It is not a strict outer
/// bound: the two chords touching the endpoints dip inside the true arc by
/// `O(step^2)` before crossing out.)
#[allow(clippy::too_many_arguments)]
fn push_arc(
    output: &mut Vec<Point2>,
    center: Point2,
    start: Point2,
    end: Point2,
    turn: f64,
    along: f64,
    radius: f64,
    fragments: usize,
) {
    output.push(start);

    let sweep = turn.abs().atan2(along);
    let full_circle = fragments.max(3) as f64;
    let steps = (((full_circle * sweep) / (2.0 * std::f64::consts::PI)).round() as i64).max(1) as usize;
    if steps > 1 && sweep > POINT_EPSILON && radius > POINT_EPSILON {
        let step = sweep / steps as f64;
        let outer_radius = radius / (step * 0.5).cos();
        let first_angle = (start[1] - center[1]).atan2(start[0] - center[0]);
        let direction = if turn > 0.0 { 1.0 } else { -1.0 };
        for sample in 1..steps {
            let angle = first_angle + direction * step * sample as f64;
            output.push([
                center[0] + outer_radius * angle.cos(),
                center[1] + outer_radius * angle.sin(),
            ]);
        }
    }

    output.push(end);
}

// ---------------------------------------------------------------------------
// Triangulation
// ---------------------------------------------------------------------------

/// Ear-clipping triangulation of one CCW outer contour with CW holes.
/// Triangles wind CCW. Holes are bridged into the outer contour first.
pub fn triangulate_with_holes(outer: &[Point2], holes: &[Vec<Point2>]) -> Vec<[Point2; 3]> {
    super::prof::scope("poly2d:triangulate", || {
        triangulate_with_holes_inner(outer, holes)
    })
}

fn triangulate_with_holes_inner(outer: &[Point2], holes: &[Vec<Point2>]) -> Vec<[Point2; 3]> {
    if outer.len() < 3 {
        return Vec::new();
    }

    let mut ring: Vec<Point2> = outer.to_vec();
    if signed_area(&ring) < 0.0 {
        ring.reverse();
    }

    let mut prepared: Vec<Vec<Point2>> = holes
        .iter()
        .filter(|hole| hole.len() >= 3)
        .map(|hole| {
            let mut hole = hole.clone();
            if signed_area(&hole) > 0.0 {
                hole.reverse();
            }
            hole
        })
        .collect();

    // Bridging works right-to-left: a hole further right can only ever be
    // bridged to something to its right, so processing in descending order of
    // rightmost vertex keeps every bridge valid as the ring grows. The extra
    // tie-breakers make the order a deterministic function of the input.
    prepared.sort_by(|left, right| {
        let a = hole_sort_key(left);
        let b = hole_sort_key(right);
        b.0.total_cmp(&a.0)
            .then(b.1.total_cmp(&a.1))
            .then(a.2.total_cmp(&b.2))
            .then(a.3.total_cmp(&b.3))
    });

    // A ring without holes gets the same treatment as one with them: clip,
    // then *check*, then sweep if the check fails. Skipping the check here on
    // the grounds that a hole is what makes a ring hard was wrong — a merged
    // face of a louvred wall is a sliver-ridden 20-gon with no holes at all,
    // the clipper stalls on it, and what comes back is a cap with a slit in it.
    if prepared.is_empty() {
        let clipped = ear_clip(&ring);
        if triangulation_conforms(&clipped, &ring, &[]) {
            return clipped;
        }
        if let Some(pieces) = monotone_pieces(&ring, &[]) {
            let mut swept: Vec<[Point2; 3]> = Vec::new();
            for piece in &pieces {
                swept.extend(ear_clip(piece));
            }
            if triangulation_conforms(&swept, &ring, &[]) {
                return swept;
            }
        }
        return clipped;
    }

    // With holes there are two constructions, and neither is right everywhere,
    // so the output is *checked* rather than assumed.
    //
    // Bridging leads because it is what the rest of the kernel was measured
    // against, and it is exact whenever it works at all. It does not always
    // work: two holes whose bridges land on the same outer vertex leave a
    // *pinch* there, and a pinched ring can have no ear at all — every convex
    // corner is blocked by the lobe hanging off the pinch — at which point the
    // clipper falls back to a fan and emits overlapping triangles.
    //
    // The plane sweep has no pinch failure mode, but its diagonals can run
    // exactly through a third vertex, which leaves a T-junction between two
    // pieces. So each result is accepted only if it is *conforming*: covers the
    // right area, and its unmatched directed edges are exactly the input
    // contours. Whichever passes wins; bridging wins ties.
    let mut bridged = ring.clone();
    for hole in &prepared {
        bridge_hole(&mut bridged, hole);
    }
    let bridged = ear_clip(&bridged);
    if triangulation_conforms(&bridged, &ring, &prepared) {
        return bridged;
    }

    if let Some(pieces) = monotone_pieces(&ring, &prepared) {
        let mut swept: Vec<[Point2; 3]> = Vec::new();
        for piece in &pieces {
            swept.extend(ear_clip(piece));
        }
        if triangulation_conforms(&swept, &ring, &prepared) {
            return swept;
        }
    }

    bridged
}

/// True when `triangles` is a conforming triangulation of `outer` minus
/// `holes`: it covers exactly the right area, and the only edges it leaves
/// unpaired are the input contour edges themselves.
///
/// The second half is what catches the failures the first half cannot see. A
/// fan over a pinched ring and a diagonal drawn through a third vertex both
/// keep the total area right while leaving the mesh open, and an open cap is
/// exactly what shows up downstream as a boundary edge in the exported solid.
fn triangulation_conforms(
    triangles: &[[Point2; 3]],
    outer: &[Point2],
    holes: &[Vec<Point2>],
) -> bool {
    let expected =
        signed_area(outer) + holes.iter().map(|hole| signed_area(hole)).sum::<f64>();
    let produced: f64 = triangles.iter().map(|triangle| signed_area(triangle)).sum();
    if (produced - expected).abs() > expected.abs() * 1.0e-9 + AREA_EPSILON {
        return false;
    }

    // Coordinates are copied verbatim from the input rings by every step above,
    // so the bit pattern is a sound identity for a vertex here.
    type Key = ([u64; 2], [u64; 2]);
    let key = |from: Point2, to: Point2| -> Key {
        (
            [from[0].to_bits(), from[1].to_bits()],
            [to[0].to_bits(), to[1].to_bits()],
        )
    };
    let mut balance: std::collections::BTreeMap<Key, i32> = std::collections::BTreeMap::new();
    for triangle in triangles {
        for index in 0..3 {
            *balance
                .entry(key(triangle[index], triangle[(index + 1) % 3]))
                .or_insert(0) += 1;
        }
    }
    for contour in std::iter::once(outer).chain(holes.iter().map(|hole| hole.as_slice())) {
        for index in 0..contour.len() {
            *balance
                .entry(key(contour[index], contour[(index + 1) % contour.len()]))
                .or_insert(0) -= 1;
        }
    }
    // What is left must pair up: every interior edge used once in each
    // direction, every contour edge already cancelled to zero.
    for (edge, count) in &balance {
        if *count < 0 {
            return false;
        }
        let reverse = (edge.1, edge.0);
        if balance.get(&reverse).copied().unwrap_or(0) != *count {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Plane-sweep decomposition
// ---------------------------------------------------------------------------

/// How the boundary turns at a vertex, as the downward sweep sees it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SweepVertex {
    Start,
    End,
    Split,
    Merge,
    Regular,
}

/// Sweep order key: the sweep runs downward, and vertices at the same height
/// are visited left to right. Compared exactly — the coordinates come from one
/// welded vertex set, so equal heights are bit-equal, and an epsilon here would
/// make the comparator intransitive.
fn sweep_key(point: Point2) -> (f64, f64) {
    (-point[1], point[0])
}

fn sweep_earlier(a: Point2, b: Point2) -> bool {
    let (ay, ax) = sweep_key(a);
    let (by, bx) = sweep_key(b);
    ay < by || (ay == by && ax < bx)
}

/// The x where the edge `from -> to` meets height `y`.
fn edge_x_at(from: Point2, to: Point2, y: f64) -> f64 {
    let rise = to[1] - from[1];
    if rise.abs() <= POINT_EPSILON {
        return from[0].min(to[0]);
    }
    let t = ((y - from[1]) / rise).clamp(0.0, 1.0);
    from[0] + t * (to[0] - from[0])
}

/// Cuts one CCW outer contour with CW holes into simple, hole-free pieces.
///
/// This is the textbook downward plane sweep: every *split* vertex is joined to
/// the helper of the edge on its left and every *merge* vertex to whatever
/// arrives below it, which is exactly the set of diagonals that removes both
/// kinds of local non-monotonicity. Holes need no special handling — wound
/// clockwise, their edges already carry the interior on their left, so they
/// enter the sweep as ordinary boundary.
///
/// The diagonals it draws can pass exactly through a third vertex, which leaves
/// a T-junction between two pieces, so the caller checks the result with
/// [`triangulation_conforms`] rather than trusting it.
///
/// Returns `None` when the sweep meets something it cannot classify (a split
/// vertex with no edge to its left, i.e. an input whose contours touch or are
/// mis-nested). The caller falls back rather than exporting a guess.
fn monotone_pieces(outer: &[Point2], holes: &[Vec<Point2>]) -> Option<Vec<Vec<Point2>>> {
    let mut points: Vec<Point2> = Vec::new();
    let mut next: Vec<usize> = Vec::new();
    let mut previous: Vec<usize> = Vec::new();
    for contour in std::iter::once(outer).chain(holes.iter().map(|hole| hole.as_slice())) {
        if contour.len() < 3 {
            return None;
        }
        let base = points.len();
        let count = contour.len();
        for (offset, point) in contour.iter().enumerate() {
            points.push(*point);
            next.push(base + (offset + 1) % count);
            previous.push(base + (offset + count - 1) % count);
        }
    }

    let kinds: Vec<SweepVertex> = (0..points.len())
        .map(|index| {
            let before = points[previous[index]];
            let current = points[index];
            let after = points[next[index]];
            let before_above = sweep_earlier(before, current);
            let after_above = sweep_earlier(after, current);
            let convex = cross(sub(current, before), sub(after, current)) > 0.0;
            match (before_above, after_above) {
                (false, false) if convex => SweepVertex::Start,
                (false, false) => SweepVertex::Split,
                (true, true) if convex => SweepVertex::End,
                (true, true) => SweepVertex::Merge,
                _ => SweepVertex::Regular,
            }
        })
        .collect();

    let mut order: Vec<usize> = (0..points.len()).collect();
    order.sort_by(|left, right| {
        let a = sweep_key(points[*left]);
        let b = sweep_key(points[*right]);
        a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1))
    });

    // Sweep status: the edges the sweep line currently crosses that carry the
    // interior on their right, each with its helper. Small enough everywhere in
    // this kernel that a linear scan beats a balanced tree.
    let mut status: Vec<(usize, usize)> = Vec::new();
    let mut diagonals: Vec<(usize, usize)> = Vec::new();

    for &vertex in &order {
        let height = points[vertex][1];
        let left_of = |status: &Vec<(usize, usize)>| -> Option<usize> {
            let mut best: Option<(f64, usize)> = None;
            for (slot, (edge, _)) in status.iter().enumerate() {
                let x = edge_x_at(points[*edge], points[next[*edge]], height);
                if x > points[vertex][0] + POINT_EPSILON {
                    continue;
                }
                if best.map_or(true, |(current, _)| x > current) {
                    best = Some((x, slot));
                }
            }
            best.map(|(_, slot)| slot)
        };
        match kinds[vertex] {
            SweepVertex::Start => status.push((vertex, vertex)),
            SweepVertex::End => {
                let edge = previous[vertex];
                let slot = status.iter().position(|(id, _)| *id == edge)?;
                if kinds[status[slot].1] == SweepVertex::Merge {
                    diagonals.push((vertex, status[slot].1));
                }
                status.remove(slot);
            }
            SweepVertex::Split => {
                let slot = left_of(&status)?;
                diagonals.push((vertex, status[slot].1));
                status[slot].1 = vertex;
                status.push((vertex, vertex));
            }
            SweepVertex::Merge => {
                let edge = previous[vertex];
                let slot = status.iter().position(|(id, _)| *id == edge)?;
                if kinds[status[slot].1] == SweepVertex::Merge {
                    diagonals.push((vertex, status[slot].1));
                }
                status.remove(slot);
                let slot = left_of(&status)?;
                if kinds[status[slot].1] == SweepVertex::Merge {
                    diagonals.push((vertex, status[slot].1));
                }
                status[slot].1 = vertex;
            }
            SweepVertex::Regular => {
                // The interior lies to the right exactly when the boundary is
                // descending here, which for a CCW outer is its left side.
                let descending = sweep_earlier(points[previous[vertex]], points[vertex])
                    && sweep_earlier(points[vertex], points[next[vertex]]);
                if descending {
                    let edge = previous[vertex];
                    let slot = status.iter().position(|(id, _)| *id == edge)?;
                    if kinds[status[slot].1] == SweepVertex::Merge {
                        diagonals.push((vertex, status[slot].1));
                    }
                    status.remove(slot);
                    status.push((vertex, vertex));
                } else {
                    let slot = left_of(&status)?;
                    if kinds[status[slot].1] == SweepVertex::Merge {
                        diagonals.push((vertex, status[slot].1));
                    }
                    status[slot].1 = vertex;
                }
            }
        }
    }

    Some(subdivision_faces(&points, &next, &diagonals))
}

/// The bounded faces of the planar subdivision made by the contours plus the
/// sweep's diagonals.
///
/// Every contour edge is traversed once (its interior is already on its left)
/// and every diagonal once per direction, so walking always-leftmost from each
/// unused edge visits each face exactly once. Faces enclosing negative area are
/// the outside and are dropped.
fn subdivision_faces(
    points: &[Point2],
    next: &[usize],
    diagonals: &[(usize, usize)],
) -> Vec<Vec<Point2>> {
    let mut edges: Vec<(usize, usize)> = (0..points.len()).map(|index| (index, next[index])).collect();
    for (from, to) in diagonals {
        edges.push((*from, *to));
        edges.push((*to, *from));
    }
    let mut outgoing: Vec<Vec<usize>> = vec![Vec::new(); points.len()];
    for (id, (from, _)) in edges.iter().enumerate() {
        outgoing[*from].push(id);
    }

    let mut used = vec![false; edges.len()];
    let mut faces: Vec<Vec<Point2>> = Vec::new();
    for start in 0..edges.len() {
        if used[start] {
            continue;
        }
        let origin = edges[start].0;
        let mut chain: Vec<usize> = Vec::new();
        let mut current = start;
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
            let base = sub(points[edges[current].0], points[vertex]);
            let Some(step) = next_edge_clockwise(&outgoing, &edges, &used, points, vertex, base)
            else {
                chain.clear();
                break;
            };
            current = step;
        }
        if chain.len() < 3 {
            continue;
        }
        let face: Vec<Point2> = chain.into_iter().map(|index| points[index]).collect();
        if signed_area(&face) > 0.0 {
            faces.push(face);
        }
    }
    faces
}

/// The outgoing edge making the smallest clockwise turn away from `base`, which
/// is the reversed direction the walk arrived from. Keeps the interior on the
/// left of the traversal.
fn next_edge_clockwise(
    outgoing: &[Vec<usize>],
    edges: &[(usize, usize)],
    used: &[bool],
    points: &[Point2],
    vertex: usize,
    base: Point2,
) -> Option<usize> {
    let mut best: Option<(f64, usize)> = None;
    for candidate in &outgoing[vertex] {
        if used[*candidate] {
            continue;
        }
        let direction = sub(points[edges[*candidate].1], points[vertex]);
        let mut turn = -cross(base, direction).atan2(dot(base, direction));
        if turn <= 0.0 {
            turn += 2.0 * std::f64::consts::PI;
        }
        match best {
            Some((score, _)) if score <= turn => {}
            _ => best = Some((turn, *candidate)),
        }
    }
    best.map(|(_, id)| id)
}

fn hole_sort_key(hole: &[Point2]) -> (f64, f64, f64, f64) {
    let index = rightmost_index(hole);
    let point = hole[index];
    let first = hole[0];
    (point[0], point[1], first[0], first[1])
}

fn rightmost_index(contour: &[Point2]) -> usize {
    let mut best = 0;
    for index in 1..contour.len() {
        let candidate = contour[index];
        let current = contour[best];
        if candidate[0] > current[0]
            || (candidate[0] == current[0] && candidate[1] > current[1])
        {
            best = index;
        }
    }
    best
}

/// True when vertex `index` of a CCW ring turns right (i.e. is reflex).
fn is_reflex(ring: &[Point2], index: usize) -> bool {
    let count = ring.len();
    let previous = ring[(index + count - 1) % count];
    let current = ring[index];
    let next = ring[(index + 1) % count];
    cross(sub(current, previous), sub(next, current)) < -AREA_EPSILON
}

/// Splices one CW hole into a CCW ring with a doubled bridge edge.
///
/// The classic construction:
///
/// 1. Take `m`, the hole's rightmost vertex.
/// 2. Cast a ray from `m` towards +x and find the nearest ring edge it hits.
/// 3. Take `p`, the endpoint of that edge with the larger x — the mutually
///    visible candidate. If the hit point *is* a ring vertex, use it directly.
/// 4. If any reflex ring vertex lies inside the triangle `(m, hit, p)` it
///    blocks visibility; replace `p` by the blocker whose direction from `m` is
///    closest to +x (ties broken by proximity).
/// 5. Rewrite the ring as `... p, m, hole..., m, p, ...` so the hole becomes
///    part of one simple polygon, traversed in its own CW order.
///
/// Returns false when no edge lies to the right of the hole, which can only
/// happen for a hole that is not actually inside the ring; such a hole is
/// skipped rather than corrupting the ring.
fn bridge_hole(ring: &mut Vec<Point2>, hole: &[Point2]) -> bool {
    if ring.len() < 3 || hole.len() < 3 {
        return false;
    }
    let hole_start = rightmost_index(hole);
    let origin = hole[hole_start];
    let count = ring.len();

    // Step 2: nearest rightward crossing.
    let mut best: Option<(f64, usize)> = None;
    for index in 0..count {
        let a = ring[index];
        let b = ring[(index + 1) % count];
        let rise = b[1] - a[1];
        if rise.abs() <= POINT_EPSILON {
            // Horizontal edge: it cannot be crossed transversally, and its
            // endpoints are picked up by the neighbouring edges.
            continue;
        }
        let t = (origin[1] - a[1]) / rise;
        if !(0.0..=1.0).contains(&t) {
            continue;
        }
        let x = a[0] + t * (b[0] - a[0]);
        if x < origin[0] - POINT_EPSILON {
            continue;
        }
        if best.map_or(true, |(current, _)| x < current) {
            best = Some((x, index));
        }
    }
    let Some((hit_x, edge)) = best else {
        return false;
    };
    let hit = [hit_x, origin[1]];

    // Step 3: candidate bridge vertex.
    let a = ring[edge];
    let b = ring[(edge + 1) % count];
    let mut target = if same_point(hit, a) {
        edge
    } else if same_point(hit, b) {
        (edge + 1) % count
    } else if a[0] >= b[0] {
        edge
    } else {
        (edge + 1) % count
    };

    // Step 4: reflex vertices inside (origin, hit, target) block visibility.
    //
    // Two degeneracies have to be handled explicitly, because `point_in_triangle`
    // is deliberately *inclusive* and therefore accepts every point of a
    // collapsed triangle's supporting line:
    //
    // * When the ray lands exactly on a ring vertex, `hit == candidate` and the
    //   visibility triangle collapses to the segment `origin -> hit`. That
    //   vertex is then visible by construction and nothing can block it, but the
    //   inclusive test would report *every* collinear ring vertex — including
    //   ones far beyond the hit — as a blocker, and the bridge would be drawn
    //   through the vertex it was supposed to stop at. Skip the scan.
    // * Otherwise the candidate itself seeds the search. A genuine blocker lies
    //   inside the wedge between `origin -> hit` (cosine 1) and
    //   `origin -> candidate`, so its cosine can never be *worse* than the
    //   candidate's; seeding stops a merely-collinear, more distant vertex from
    //   winning on a tie.
    let candidate = ring[target];
    let visibility_area = cross(sub(hit, origin), sub(candidate, origin));
    let (ta, tb, tc) = if visibility_area >= 0.0 {
        (origin, hit, candidate)
    } else {
        (origin, candidate, hit)
    };
    let candidate_offset = sub(candidate, origin);
    let candidate_distance = length(candidate_offset);
    let mut best_cosine = if candidate_distance > POINT_EPSILON {
        candidate_offset[0] / candidate_distance
    } else {
        f64::NEG_INFINITY
    };
    let mut best_distance = candidate_distance;
    let degenerate = visibility_area.abs() <= AREA_EPSILON;
    for index in 0..count {
        if degenerate || index == target {
            continue;
        }
        let point = ring[index];
        if same_point(point, origin) || same_point(point, candidate) || same_point(point, hit) {
            continue;
        }
        if !is_reflex(ring, index) {
            continue;
        }
        if !point_in_triangle(point, ta, tb, tc) {
            continue;
        }
        let offset = sub(point, origin);
        let size = length(offset);
        if size <= POINT_EPSILON {
            continue;
        }
        let cosine = offset[0] / size;
        if cosine > best_cosine + POINT_EPSILON
            || (cosine > best_cosine - POINT_EPSILON && size < best_distance)
        {
            best_cosine = cosine;
            best_distance = size;
            target = index;
        }
    }

    // Step 5: splice.
    let bridge = ring[target];
    let mut merged: Vec<Point2> = Vec::with_capacity(ring.len() + hole.len() + 2);
    merged.extend_from_slice(&ring[..=target]);
    for step in 0..hole.len() {
        merged.push(hole[(hole_start + step) % hole.len()]);
    }
    merged.push(origin);
    merged.push(bridge);
    merged.extend_from_slice(&ring[target + 1..]);
    *ring = merged;
    true
}

/// True when direction `d` lies strictly inside the cone swept
/// counter-clockwise from `from` to `to`.
///
/// The cone is assumed convex (less than a straight angle), which is what the
/// interior angle at the corner of a counter-clockwise triangle always is. A
/// direction lying exactly along either bounding edge is *not* inside: that is
/// the case of a bridge segment running along an ear edge, which is legitimate
/// and must stay clippable.
fn direction_inside_cone(d: Point2, from: Point2, to: Point2) -> bool {
    let d = normalized(d);
    let from = normalized(from);
    let to = normalized(to);
    cross(from, d) > DIRECTION_EPSILON && cross(d, to) > DIRECTION_EPSILON
}

fn normalized(a: Point2) -> Point2 {
    let size = length(a);
    if size <= 0.0 {
        [0.0, 0.0]
    } else {
        [a[0] / size, a[1] / size]
    }
}

/// True when the ring lobe attached at `position` reaches into the ear
/// `(a, b, c)`.
///
/// `position` holds a second occurrence of one of the ear's corners — the pinch
/// that hole bridging leaves behind. The two rings that meet there are distinct
/// pieces of boundary, so the ear is only valid while the *other* piece stays
/// outside it. Since the pinch point itself is shared, containment says
/// nothing; what decides it is whether either edge leaving that occurrence
/// points into the ear's interior angle at the shared corner.
fn pinch_reaches_into_ear(vertices: &[Point2], position: usize, a: Point2, b: Point2, c: Point2) -> bool {
    let count = vertices.len();
    let corner = vertices[position];
    let (from, to) = if same_point(corner, a) {
        (sub(b, a), sub(c, a))
    } else if same_point(corner, b) {
        (sub(c, b), sub(a, b))
    } else {
        (sub(a, c), sub(b, c))
    };
    let previous = vertices[(position + count - 1) % count];
    let next = vertices[(position + 1) % count];
    direction_inside_cone(sub(previous, corner), from, to)
        || direction_inside_cone(sub(next, corner), from, to)
}

/// Ear clipping for a simple (possibly bridged) polygon.
///
/// Robustness rules, in order of application per iteration:
///
/// 1. Look for a strictly convex vertex whose triangle contains no other
///    vertex. A vertex *coincident* with one of the ear's corners is a pinch
///    from hole bridging rather than an ordinary blocker; it blocks the ear
///    only when the boundary attached to it turns into the ear
///    ([`pinch_reaches_into_ear`]). Treating those as unconditional blockers
///    would make bridged rings untriangulable; ignoring them outright lets the
///    clipper cut across a neighbouring lobe.
/// 2. If no ear exists, drop one duplicate or collinear vertex — those are the
///    usual culprits and removing one always makes progress.
/// 3. If neither is possible, stop and emit a triangle fan over whatever is
///    left. The fan may be geometrically wrong for a badly non-simple input,
///    but it terminates and never panics.
///
/// Every iteration either removes a vertex or breaks, so the loop is bounded
/// by the vertex count; an explicit iteration cap guards against that
/// reasoning being invalidated by a future edit.
fn ear_clip(polygon: &[Point2]) -> Vec<[Point2; 3]> {
    let mut vertices: Vec<Point2> = polygon.to_vec();
    if vertices.len() < 3 {
        return Vec::new();
    }
    if signed_area(&vertices) < 0.0 {
        vertices.reverse();
    }

    let mut triangles: Vec<[Point2; 3]> = Vec::new();
    let maximum_iterations = vertices.len() * 3 + 16;
    let mut iterations = 0usize;
    // Flat vertices the loop below drops to keep making progress, as
    // `[before, dropped, after]`. Dropping one is harmless for *area* and
    // fatal for *conformity*: such a vertex is on this ring because a face in
    // another plane has a corner on this edge, so losing it leaves that face's
    // two short edges facing this face's one long one — a slit. They go back
    // at the end.
    let mut dropped: Vec<[Point2; 3]> = Vec::new();

    while vertices.len() > 3 {
        iterations += 1;
        if iterations > maximum_iterations {
            break;
        }
        let count = vertices.len();
        let mut clipped = false;

        for index in 0..count {
            let before = (index + count - 1) % count;
            let after = (index + 1) % count;
            let a = vertices[before];
            let b = vertices[index];
            let c = vertices[after];
            if cross(sub(b, a), sub(c, b)) <= AREA_EPSILON {
                continue; // reflex or degenerate
            }
            let mut valid = true;
            for other in 0..count {
                if other == before || other == index || other == after {
                    continue;
                }
                let point = vertices[other];
                if same_point(point, a) || same_point(point, b) || same_point(point, c) {
                    // A *second occurrence* of one of the ear's own corners: a
                    // pinch left behind by hole bridging. Ignoring it outright
                    // is what used to let the clipper cut an ear straight
                    // across the lobe hanging off that pinch, turning the ring
                    // self-intersecting. Ask instead whether that lobe reaches
                    // into the ear.
                    if pinch_reaches_into_ear(&vertices, other, a, b, c) {
                        valid = false;
                        break;
                    }
                    continue;
                }
                if point_in_triangle(point, a, b, c) {
                    valid = false;
                    break;
                }
            }
            if valid {
                triangles.push([a, b, c]);
                vertices.remove(index);
                clipped = true;
                break;
            }
        }
        if clipped {
            continue;
        }

        let mut removed = false;
        for index in 0..count {
            let before = (index + count - 1) % count;
            let after = (index + 1) % count;
            let a = vertices[before];
            let b = vertices[index];
            let c = vertices[after];
            if same_point(a, b) || same_point(b, c) {
                vertices.remove(index);
                removed = true;
                break;
            }
            if cross(sub(b, a), sub(c, b)).abs() <= AREA_EPSILON {
                dropped.push([a, b, c]);
                vertices.remove(index);
                removed = true;
                break;
            }
        }
        if !removed {
            break;
        }
    }

    if vertices.len() == 3 {
        push_triangle(&mut triangles, vertices[0], vertices[1], vertices[2]);
    } else if vertices.len() > 3 {
        // Fan fallback: guarantees termination for pathological input.
        for index in 1..vertices.len() - 1 {
            push_triangle(
                &mut triangles,
                vertices[0],
                vertices[index],
                vertices[index + 1],
            );
        }
    }
    restore_dropped_vertices(&mut triangles, &dropped);
    triangles
}

/// Puts back the flat vertices [`ear_clip`] dropped, without moving anything.
///
/// Dropping `b` from `a, b, c` left the edge `a -> c` in the triangulation,
/// used exactly once, because it was a boundary edge of the ring as it stood
/// at that moment. Splitting the triangle carrying it at `b` re-creates
/// `a -> b` and `b -> c`, covers the same area with the same winding, and is
/// not itself flat — the apex is off the line through `a` and `c`.
///
/// Reverse order matters: a vertex dropped later can sit on an edge that an
/// earlier drop created, so the later ones have to be back first.
fn restore_dropped_vertices(triangles: &mut Vec<[Point2; 3]>, dropped: &[[Point2; 3]]) {
    let carries = |triangle: &[Point2; 3], from: Point2, to: Point2| -> Option<usize> {
        (0..3).find(|corner| {
            same_point(triangle[*corner], from) && same_point(triangle[(corner + 1) % 3], to)
        })
    };
    for [a, b, c] in dropped.iter().rev() {
        let Some(position) = triangles
            .iter()
            .position(|triangle| carries(triangle, *a, *c).is_some())
        else {
            continue;
        };
        let triangle = triangles[position];
        let corner = carries(&triangle, *a, *c).expect("the edge was just located");
        let apex = triangle[(corner + 2) % 3];
        triangles[position] = [*a, *b, apex];
        triangles.push([*b, *c, apex]);
    }
}

/// Emits a triangle, dropping degenerate ones and flipping to CCW.
fn push_triangle(triangles: &mut Vec<[Point2; 3]>, a: Point2, b: Point2, c: Point2) {
    let twice_area = cross(sub(b, a), sub(c, a));
    if twice_area > AREA_EPSILON {
        triangles.push([a, b, c]);
    } else if twice_area < -AREA_EPSILON {
        triangles.push([a, c, b]);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const UNIT_SQUARE: [Point2; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];

    fn square(size: f64) -> Vec<Point2> {
        vec![[0.0, 0.0], [size, 0.0], [size, size], [0.0, size]]
    }

    fn rectangle(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<Point2> {
        vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]]
    }

    fn triangle_area(triangle: &[Point2; 3]) -> f64 {
        cross(
            sub(triangle[1], triangle[0]),
            sub(triangle[2], triangle[0]),
        ) * 0.5
    }

    fn total_area(triangles: &[[Point2; 3]]) -> f64 {
        triangles.iter().map(triangle_area).sum()
    }

    #[test]
    fn signed_area_of_unit_square() {
        assert!((signed_area(&UNIT_SQUARE) - 1.0).abs() < 1.0e-12);
        let mut reversed = UNIT_SQUARE.to_vec();
        reversed.reverse();
        assert!((signed_area(&reversed) + 1.0).abs() < 1.0e-12);
    }

    #[test]
    fn point_in_contour_basics() {
        assert!(point_in_contour([0.5, 0.5], &UNIT_SQUARE));
        assert!(!point_in_contour([1.5, 0.5], &UNIT_SQUARE));
        assert!(!point_in_contour([-0.5, 0.5], &UNIT_SQUARE));
    }

    #[test]
    fn triangulates_square_into_two_triangles() {
        let region = Region2d::new(vec![square(10.0)]);
        let triangles = region.triangulate();
        assert_eq!(triangles.len(), 2);
        assert!((total_area(&triangles) - 100.0).abs() < 1.0e-9);
        for triangle in &triangles {
            assert!(triangle_area(triangle) > 0.0);
        }
    }

    #[test]
    fn triangulates_square_with_centred_hole() {
        let outer = square(10.0);
        // Same winding as the outer: `normalize` must flip it into a hole.
        let hole = rectangle(3.0, 3.0, 7.0, 7.0);
        let region = Region2d::new(vec![outer, hole]);
        let triangles = region.triangulate();

        let expected = 100.0 - 16.0;
        assert!(
            (total_area(&triangles) - expected).abs() < 1.0e-9,
            "got {}",
            total_area(&triangles)
        );
        for triangle in &triangles {
            assert!(
                triangle_area(triangle) > 0.0,
                "triangle is not CCW: {triangle:?}"
            );
        }
    }

    #[test]
    fn normalize_flips_inner_contour_to_clockwise() {
        let mut region = Region2d::new(vec![square(10.0), rectangle(3.0, 3.0, 7.0, 7.0)]);
        assert!(signed_area(&region.contours[0]) > 0.0);
        assert!(signed_area(&region.contours[1]) > 0.0);

        region.normalize();

        assert_eq!(region.contours.len(), 2);
        assert!(signed_area(&region.contours[0]) > 0.0, "outer stays CCW");
        assert!(signed_area(&region.contours[1]) < 0.0, "hole becomes CW");
        assert!((region.area() - 84.0).abs() < 1.0e-9);
    }

    #[test]
    fn convex_polygon_yields_n_minus_two_triangles() {
        for count in [3usize, 4, 5, 7, 12, 32] {
            let contour: Vec<Point2> = (0..count)
                .map(|index| {
                    let angle =
                        2.0 * std::f64::consts::PI * index as f64 / count as f64;
                    [5.0 * angle.cos(), 5.0 * angle.sin()]
                })
                .collect();
            let triangles = Region2d::new(vec![contour.clone()]).triangulate();
            assert_eq!(triangles.len(), count - 2, "n = {count}");
            assert!((total_area(&triangles) - signed_area(&contour)).abs() < 1.0e-9);
        }
    }

    #[test]
    fn non_convex_l_shape_triangulates_to_its_own_area() {
        let shape: Vec<Point2> = vec![
            [0.0, 0.0],
            [4.0, 0.0],
            [4.0, 2.0],
            [2.0, 2.0],
            [2.0, 4.0],
            [0.0, 4.0],
        ];
        let expected = signed_area(&shape);
        assert!((expected - 12.0).abs() < 1.0e-12);

        let triangles = Region2d::new(vec![shape]).triangulate();
        assert_eq!(triangles.len(), 4);
        assert!((total_area(&triangles) - expected).abs() < 1.0e-9);
        for triangle in &triangles {
            assert!(triangle_area(triangle) > 0.0);
        }
    }

    #[test]
    fn mitred_grow_of_square() {
        let region = Region2d::new(vec![square(10.0)]);
        let grown = region.offset(1.0, false, 32);
        assert_eq!(grown.contours.len(), 1);
        assert_eq!(grown.contours[0].len(), 4);
        assert!((grown.area() - 144.0).abs() < 1.0e-9, "got {}", grown.area());
        let (minimum, maximum) = grown.bounds().unwrap();
        assert!((minimum[0] + 1.0).abs() < 1.0e-9);
        assert!((minimum[1] + 1.0).abs() < 1.0e-9);
        assert!((maximum[0] - 11.0).abs() < 1.0e-9);
        assert!((maximum[1] - 11.0).abs() < 1.0e-9);
    }

    #[test]
    fn mitred_shrink_of_square() {
        let region = Region2d::new(vec![square(10.0)]);
        let shrunk = region.offset(-1.0, false, 32);
        assert_eq!(shrunk.contours.len(), 1);
        assert_eq!(shrunk.contours[0].len(), 4);
        assert!((shrunk.area() - 64.0).abs() < 1.0e-9, "got {}", shrunk.area());
        let (minimum, maximum) = shrunk.bounds().unwrap();
        assert!((minimum[0] - 1.0).abs() < 1.0e-9);
        assert!((maximum[0] - 9.0).abs() < 1.0e-9);
    }

    #[test]
    fn over_shrink_empties_the_region() {
        let region = Region2d::new(vec![square(10.0)]);
        let shrunk = region.offset(-10.0, false, 32);
        assert!(shrunk.is_empty(), "got {:?}", shrunk.contours);
        assert!(region.offset(-6.0, false, 32).is_empty());
        assert!(region.offset(-5.0, false, 32).is_empty());
        assert!(!region.offset(-4.0, false, 32).is_empty());
    }

    #[test]
    fn rounded_grow_of_square() {
        let region = Region2d::new(vec![square(10.0)]);
        let grown = region.offset(1.0, true, 32);
        assert_eq!(grown.contours.len(), 1);
        assert!(
            grown.contours[0].len() > 4,
            "rounded corners must add points, got {}",
            grown.contours[0].len()
        );

        let mitred = 144.0;
        let exact = 100.0 + 40.0 + std::f64::consts::PI;
        let area = grown.area();
        assert!(
            area > exact && area < mitred,
            "area {area} must lie strictly between the exact rounded area {exact} and the mitred area {mitred}"
        );
    }

    #[test]
    fn rounded_shrink_keeps_more_area_than_mitred_shrink() {
        // A notched pentagon: the vertex at (5, 6) is reflex.
        let shape: Vec<Point2> = vec![
            [0.0, 0.0],
            [10.0, 0.0],
            [10.0, 10.0],
            [5.0, 6.0],
            [0.0, 10.0],
        ];
        let region = Region2d::new(vec![shape]);
        let rounded = region.offset(-1.0, true, 32);
        let mitred = region.offset(-1.0, false, 32);
        assert!(!rounded.is_empty());
        assert!(!mitred.is_empty());
        // Under erosion the *reflex* corners are the ones that round, and a
        // rounded corner sits at distance |delta| from the source vertex while
        // the mitre point sits at |delta| / cos(half angle) — further into the
        // material. So the mitred erosion removes strictly more.
        assert!(
            rounded.area() > mitred.area(),
            "rounded {} mitred {}",
            rounded.area(),
            mitred.area()
        );
    }

    #[test]
    fn shrink_erodes_a_thin_arm_without_crossing() {
        // A T with a 2mm-wide stem: shrinking by 1.5 must consume the stem
        // rather than fold it inside out.
        let shape: Vec<Point2> = vec![
            [0.0, 0.0],
            [20.0, 0.0],
            [20.0, 10.0],
            [11.0, 10.0],
            [11.0, 30.0],
            [9.0, 30.0],
            [9.0, 10.0],
            [0.0, 10.0],
        ];
        let region = Region2d::new(vec![shape]);
        let shrunk = region.offset(-1.5, false, 32);
        assert!(!shrunk.is_empty());
        for contour in &shrunk.contours {
            assert!(signed_area(contour) > 0.0, "contour turned inside out");
        }
        // Only the 20x10 bar survives, eroded to 17x7 = 119.
        assert!(
            (shrunk.area() - 119.0).abs() < 1.0e-6,
            "got {}",
            shrunk.area()
        );
    }

    #[test]
    fn offset_preserves_holes() {
        let mut region = Region2d::new(vec![square(20.0), rectangle(8.0, 8.0, 12.0, 12.0)]);
        region.normalize();
        let grown = region.offset(1.0, false, 32);
        assert_eq!(grown.contours.len(), 2);
        // Outer grows to 22x22, hole shrinks to 2x2.
        assert!((grown.area() - (484.0 - 4.0)).abs() < 1.0e-9, "got {}", grown.area());
        assert!(signed_area(&grown.contours[1]) < 0.0, "hole stays CW");
    }

    /// A ring with a vertex exactly mid-edge — the shape the T-junction repair
    /// leaves behind — must come back triangulated *through* that vertex. The
    /// clipper is allowed to drop it while it works; it is not allowed to hand
    /// it back missing, because the face on the other side of that edge has a
    /// corner there and the two would stop meeting.
    #[test]
    fn a_vertex_sitting_mid_edge_survives_triangulation() {
        let ring = vec![
            [0.0, 0.0],
            [5.0, 0.0], // collinear: the seam vertex
            [10.0, 0.0],
            [10.0, 10.0],
            [0.0, 10.0],
        ];
        let triangles = triangulate_with_holes(&ring, &[]);
        assert!((total_area(&triangles) - 100.0).abs() < 1.0e-9);
        assert!(
            triangulation_conforms(&triangles, &ring, &[]),
            "the triangulation left an edge the ring does not have"
        );
        assert!(
            triangles
                .iter()
                .any(|triangle| triangle.iter().any(|point| same_point(*point, [5.0, 0.0]))),
            "the mid-edge vertex was dropped"
        );
    }

    /// The restorer has to survive a run of them, and it has to put them back
    /// in the right order — a later drop can sit on an edge an earlier one
    /// created.
    #[test]
    fn several_mid_edge_vertices_on_one_side_all_survive() {
        let seam: Vec<Point2> = (1..5).map(|step| [step as f64 * 2.0, 0.0]).collect();
        let mut ring = vec![[0.0, 0.0]];
        ring.extend(seam.iter().copied());
        ring.extend([[10.0, 0.0], [10.0, 10.0], [0.0, 10.0]]);
        let triangles = triangulate_with_holes(&ring, &[]);
        assert!((total_area(&triangles) - 100.0).abs() < 1.0e-9);
        assert!(triangulation_conforms(&triangles, &ring, &[]));
        for point in &seam {
            assert!(
                triangles
                    .iter()
                    .any(|triangle| triangle.iter().any(|corner| same_point(*corner, *point))),
                "mid-edge vertex {point:?} was dropped"
            );
        }
    }

    #[test]
    fn triangulate_with_holes_direct_call() {
        let outer = square(10.0);
        let mut hole = rectangle(3.0, 3.0, 7.0, 7.0);
        hole.reverse(); // CW
        let triangles = triangulate_with_holes(&outer, &[hole]);
        assert!((total_area(&triangles) - 84.0).abs() < 1.0e-9);
        for triangle in &triangles {
            assert!(triangle_area(triangle) > 0.0);
        }
    }

    /// Every triangulation of a region has to be *conforming*: the only edges
    /// it leaves unpaired are the region's own contour edges. An open cap
    /// travels straight through to the exported solid as a boundary edge, and
    /// area alone does not catch it — the two failures below both had the right
    /// area and a torn mesh.
    fn assert_conforming(outer: &[Point2], holes: &[Vec<Point2>]) -> usize {
        let triangles = triangulate_with_holes(outer, holes);
        let expected =
            signed_area(outer) + holes.iter().map(|hole| signed_area(hole)).sum::<f64>();
        let produced = total_area(&triangles);
        assert!(
            (produced - expected).abs() < 1.0e-9,
            "area {produced} vs {expected}"
        );
        assert!(
            triangulation_conforms(&triangles, outer, holes),
            "triangulation is not edge-conforming"
        );
        triangles.len()
    }

    /// Two holes side by side at the same height used to make the hole-bridging
    /// ray land exactly on a ring vertex. The visibility triangle collapsed to a
    /// segment, every collinear vertex beyond the hit passed the inclusive
    /// point-in-triangle test, and the bridge was drawn *through* the vertex it
    /// should have stopped at — leaving a T-junction and an open cap.
    #[test]
    fn two_holes_at_the_same_height_stay_conforming() {
        let outer = square(10.0);
        let mut left = rectangle(2.0, 2.0, 4.0, 4.0);
        let mut right = rectangle(6.0, 2.0, 8.0, 4.0);
        left.reverse();
        right.reverse();
        // 12 vertices, 2 holes: n + 2h - 2 triangles when nothing is lost.
        assert_eq!(assert_conforming(&outer, &[left, right]), 14);
    }

    /// Two holes that bridge to the *same* outer vertex leave a pinch there,
    /// and a pinched ring can have no ear at all: every convex corner is
    /// blocked by the lobe hanging off the pinch. Ear clipping used to give up
    /// and emit a fan of overlapping triangles.
    #[test]
    fn holes_that_bridge_to_one_vertex_stay_conforming() {
        let outer = rectangle(-20.0, -20.0, 20.0, 20.0);
        let mut near = rectangle(-1.0, -1.0, 1.0, 1.0);
        let mut far = rectangle(6.0, -1.0, 8.0, 1.0);
        near.reverse();
        far.reverse();
        assert_conforming(&outer, &[near, far]);
    }

    /// Five holes at three different heights: the openappa material mask, which
    /// is where this class of failure was found.
    #[test]
    fn a_mask_with_five_holes_stays_conforming() {
        let outer = rectangle(-32.0, -5.0, 32.0, 55.0);
        let mut holes = Vec::new();
        for (x0, x1, y0, y1) in [
            (-27.3, -25.0, 0.0, 2.3),
            (-16.0, -9.0, 0.0, 2.3),
            (-2.0, 5.0, 0.0, 2.3),
            (11.0, 27.3, 0.0, 2.3),
            (-2.3, 2.3, 24.9, 27.3),
        ] {
            let mut hole = rectangle(x0, y0, x1, y1);
            hole.reverse();
            holes.push(hole);
        }
        assert_conforming(&outer, &holes);
    }

    /// A hole that touches another hole at a single point is a legitimate
    /// region — booleans produce them constantly — and must still triangulate.
    #[test]
    fn holes_touching_at_a_point_stay_conforming() {
        let outer = rectangle(-15.0, -15.0, 15.0, 15.0);
        let mut block = rectangle(-5.0, -5.0, 5.0, 5.0);
        block.reverse();
        let notch = vec![[5.0, 5.0], [5.0, 8.0], [8.0, 5.0]];
        assert_conforming(&outer, &[block, notch]);
    }

    #[test]
    fn triangulation_terminates_on_degenerate_input() {
        // Repeated and collinear points, plus a zero-area spike.
        let contour: Vec<Point2> = vec![
            [0.0, 0.0],
            [0.0, 0.0],
            [5.0, 0.0],
            [10.0, 0.0],
            [10.0, 10.0],
            [5.0, 10.0],
            [0.0, 10.0],
            [0.0, 5.0],
        ];
        let triangles = Region2d::new(vec![contour]).triangulate();
        assert!((total_area(&triangles) - 100.0).abs() < 1.0e-9);
    }

    #[test]
    fn empty_and_degenerate_regions() {
        assert!(Region2d::default().is_empty());
        assert!(Region2d::new(vec![vec![[0.0, 0.0], [1.0, 0.0]]]).is_empty());
        assert!(Region2d::default().bounds().is_none());
        assert!(Region2d::default().triangulate().is_empty());
        let mut collapsed = Region2d::new(vec![vec![[0.0, 0.0], [1.0, 0.0], [2.0, 0.0]]]);
        collapsed.normalize();
        assert!(collapsed.contours.is_empty());
    }
}
