//! 2D regions evaluated through the exact 3D kernel.
//!
//! OpenSCAD's 2D subsystem supports the same booleans as its 3D one. Rather
//! than write (and have to make robust) a second, planar boolean engine, the
//! kernel lifts a 2D region into a unit-thickness prism centred on `z = 0`,
//! runs the exact BSP boolean, and reads the region back off the prism's
//! bottom face. The lift is exact — every wall of the prism is a vertical
//! plane through an input edge — so the recovered contours are exactly the
//! contours of the true 2D result.

use super::mesh::{Vec3, EPSILON};
use super::poly2d::{Point2, Region2d};
use super::primitives;
use super::solid::Solid;

/// Half-height of the lifted prism.
const HALF_THICKNESS: f64 = 0.5;

/// Lifts a 2D region into a prism spanning `z = -0.5 .. 0.5`.
pub fn to_prism(region: &Region2d) -> Solid {
    primitives::linear_extrude(region, 2.0 * HALF_THICKNESS, true, 0.0, [1.0, 1.0], 1)
}

/// Reads a 2D region back off a prism produced by [`to_prism`].
///
/// Only the faces lying in the bottom plane are considered; they face `-Z`, so
/// their loops are clockwise when viewed from `+Z` and get reversed on the way
/// out.
pub fn from_prism(solid: &Solid) -> Region2d {
    // `offset` is measured along the face normal, so the bottom plane at
    // z = -0.5 with normal (0, 0, -1) has offset +0.5.
    let bottom = super::mesh::Plane::new(Vec3::new(0.0, 0.0, -1.0), HALF_THICKNESS);
    let faces: Vec<_> = solid
        .polygons
        .iter()
        .filter(|polygon| polygon.plane.same_as(bottom))
        .cloned()
        .collect();
    if faces.is_empty() {
        return Region2d::default();
    }
    let contours = super::solid::boundary_loops(&faces, bottom)
        .into_iter()
        .map(|loop_points| {
            let mut contour: Vec<Point2> = loop_points
                .into_iter()
                .map(|vertex| [vertex.x, vertex.y])
                .collect();
            // The loops wind CCW around (0, 0, -1), i.e. clockwise in XY.
            contour.reverse();
            contour
        })
        .filter(|contour| contour.len() >= 3)
        .collect();
    let mut region = Region2d::new(contours);
    region.normalize();
    region
}

/// Unions many regions, lifting each one to a prism exactly once.
///
/// Every `to_prism`/`from_prism` round trip costs a triangulation, so doing the
/// whole reduction in the lifted domain — and balancing it — is the difference
/// between seconds and hours on a silhouette assembled from hundreds of tiles.
pub fn union_all(regions: &[Region2d]) -> Region2d {
    let prisms: Vec<Solid> = regions
        .iter()
        .filter(|region| !region.is_empty())
        .map(to_prism)
        .collect();
    if prisms.is_empty() {
        return Region2d::default();
    }
    from_prism(&super::solid::union_all(prisms))
}

/// Intersects many regions, lifting each one to a prism exactly once.
pub fn intersection_all(regions: &[Region2d]) -> Region2d {
    if regions.is_empty() || regions.iter().any(Region2d::is_empty) {
        return Region2d::default();
    }
    let prisms: Vec<Solid> = regions.iter().map(to_prism).collect();
    from_prism(&super::solid::intersection_all(prisms))
}

pub fn union(left: &Region2d, right: &Region2d) -> Region2d {
    if left.is_empty() {
        return right.clone();
    }
    if right.is_empty() {
        return left.clone();
    }
    from_prism(&to_prism(left).union(&to_prism(right)))
}

pub fn difference(left: &Region2d, right: &Region2d) -> Region2d {
    if left.is_empty() || right.is_empty() {
        return left.clone();
    }
    from_prism(&to_prism(left).difference(&to_prism(right)))
}

pub fn intersection(left: &Region2d, right: &Region2d) -> Region2d {
    if left.is_empty() || right.is_empty() {
        return Region2d::default();
    }
    from_prism(&to_prism(left).intersection(&to_prism(right)))
}

/// The convex hull of a 2D region, as `hull()` defines it in 2D.
pub fn hull(regions: &[Region2d]) -> Region2d {
    let mut points: Vec<Point2> = Vec::new();
    for region in regions {
        for contour in &region.contours {
            points.extend(contour.iter().copied());
        }
    }
    let contour = super::hull::convex_hull_2d(&points);
    if contour.len() < 3 {
        return Region2d::default();
    }
    Region2d::new(vec![contour])
}

/// The convex pieces [`minkowski`] sums pairwise.
///
/// A region that is already a single convex contour is its own piece. That
/// fast path is worth having on its own: `square() (+) circle()` — the
/// rounded-rectangle idiom, and very nearly every 2D `minkowski()` anyone
/// writes — then costs one convex hull instead of `(n - 2)` hulls plus a union
/// of as many pieces, where `n` is the circle's fragment count.
///
/// Everything else falls back to the ear-clipping triangulation. Its triangles
/// are convex by construction and cover the region exactly, holes included, so
/// it is a valid convex decomposition — just not a minimal one. A coarser
/// decomposition (Hertel-Mehlhorn, say) would cut the pair count, and the
/// pair count is what [`minkowski`] has to ration; it is not implemented here
/// because the ceiling has not yet been the binding constraint on any real
/// model.
fn convex_pieces(region: &Region2d) -> Vec<Vec<Point2>> {
    let mut normalised = region.clone();
    normalised.normalize();
    if let [contour] = normalised.contours.as_slice() {
        if super::poly2d::is_convex_ring(contour) {
            return vec![contour.clone()];
        }
    }
    normalised
        .triangulate()
        .into_iter()
        .map(|triangle| triangle.to_vec())
        .collect()
}

/// `minkowski()` of two 2D regions.
///
/// Computed exactly, by the same identity the 3D sum uses: the Minkowski sum
/// of two convex polygons is the convex hull of the pairwise vertex sums, and
/// `A (+) B = union over convex pieces Ai, Bj of (Ai (+) Bj)` reduces the
/// general case to that one. Both halves are exact — the hull is exact and the
/// union is the same prism-lifted BSP every other 2D boolean runs through — so
/// this is the sum itself, not a dilation standing in for it.
///
/// What it does not cover: the piece count is the *product* of the two
/// decompositions, so two genuinely concave operands are refused above
/// [`primitives::MAX_MINKOWSKI_PAIRS`] rather than run unbounded. That is the
/// same ceiling, and the same trade-off, as `primitives::minkowski` makes in
/// 3D, and it is a refusal with a message rather than a wrong answer.
pub fn minkowski(left: &Region2d, right: &Region2d) -> Result<Region2d, String> {
    // An absent operand, not the empty set: `minkowski()` with one child is
    // that child, and `primitives::minkowski` reads a childless operand the
    // same way. Treating it as the empty set would instead annihilate the
    // result, which is never what the source meant.
    if left.is_empty() {
        return Ok(right.clone());
    }
    if right.is_empty() {
        return Ok(left.clone());
    }
    let left_pieces = convex_pieces(left);
    let right_pieces = convex_pieces(right);
    let pairs = left_pieces.len().saturating_mul(right_pieces.len());
    if pairs > primitives::MAX_MINKOWSKI_PAIRS {
        return Err(format!(
            "minkowski() would need {pairs} convex-piece sums ({} x {}), above the {} limit",
            left_pieces.len(),
            right_pieces.len(),
            primitives::MAX_MINKOWSKI_PAIRS
        ));
    }

    let mut sums: Vec<Region2d> = Vec::with_capacity(pairs);
    for left_piece in &left_pieces {
        for right_piece in &right_pieces {
            let mut points = Vec::with_capacity(left_piece.len() * right_piece.len());
            for a in left_piece {
                for b in right_piece {
                    points.push([a[0] + b[0], a[1] + b[1]]);
                }
            }
            // `convex_hull_2d` already hands back a CCW contour with its
            // collinear vertices popped, which is a normalised single-contour
            // region.
            let contour = super::hull::convex_hull_2d(&points);
            if contour.len() < 3 {
                continue;
            }
            sums.push(Region2d::new(vec![contour]));
        }
    }
    // One piece is the convex-by-convex case above. Handing it to `union_all`
    // would be correct but would pay for a prism round trip — an extrusion, a
    // BSP walk and a triangulation — to re-derive a contour that is already
    // the answer.
    if sums.len() == 1 {
        return Ok(sums.pop().expect("length was just checked"));
    }
    Ok(union_all(&sums))
}

/// `fill()`: the 2D region with every hole closed.
///
/// A one-line forward to [`Region2d::filled`], so that every 2D module the
/// evaluator dispatches has its entry point in this module rather than half of
/// them reaching into `poly2d` directly.
pub fn fill(region: &Region2d) -> Region2d {
    region.filled()
}

/// `projection(cut = true)`: the slice of a solid at `z = 0`.
///
/// The solid is lowered by [`HALF_THICKNESS`] before it meets the slab,
/// because [`from_prism`] reads the *bottom* plane of the prism it is given.
/// Without the shift this returns the cross-section half a unit below `z = 0`,
/// which is invisible for a body of constant section — a cylinder, a prism —
/// and wrong for every other one.
pub fn projection_cut(solid: &Solid) -> Region2d {
    let Some(bounds) = solid.bounds() else {
        return Region2d::default();
    };
    let size = bounds.size();
    let pad = size.x.max(size.y).max(1.0);
    let slab = primitives::cube(
        Vec3::new(
            size.x + 2.0 * pad,
            size.y + 2.0 * pad,
            2.0 * HALF_THICKNESS,
        ),
        false,
    )
    .transformed(super::mesh::Matrix4::translation(Vec3::new(
        bounds.min.x - pad,
        bounds.min.y - pad,
        -HALF_THICKNESS,
    )));
    let lowered = solid.transformed(super::mesh::Matrix4::translation(Vec3::new(
        0.0,
        0.0,
        -HALF_THICKNESS,
    )));
    from_prism(&lowered.intersection(&slab))
}

/// `projection(cut = false)`: the shadow of a solid on the XY plane.
///
/// Computed as the union of the footprints of the solid's **downward-facing**
/// facets. Every vertical line that meets a closed body enters it through one
/// of those, so they already cover the whole silhouette; including the
/// upward-facing facets as well covers it a second time, and the second copy
/// of a cylinder's cap is exactly coincident with the first — the one input a
/// BSP union has no stable answer for, and what made the shadow of any hollow
/// body come out with its hole partly filled in.
///
/// The footprints are also reduced together rather than folded one at a time,
/// because each `union` is a prism round trip and a tessellated body has
/// hundreds of facets.
pub fn projection_shadow(solid: &Solid) -> Region2d {
    let mesh = solid.to_indexed_mesh();
    let mut footprints = Vec::new();
    for index in 0..mesh.triangles.len() {
        let [a, b, c] = mesh.triangle_points(index);
        // The cross product is unnormalised, so its `z` is twice the signed
        // area of the projected triangle: one test covers both "faces down"
        // and "projects to something with area".
        if b.sub(a).cross(c.sub(a)).z >= -EPSILON {
            continue;
        }
        // Facing down means the footprint comes out clockwise.
        let mut contour = vec![[a.x, a.y], [b.x, b.y], [c.x, c.y]];
        contour.reverse();
        let mut piece = Region2d::new(vec![contour]);
        piece.normalize();
        footprints.push(piece);
    }
    union_all(&footprints)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(size: f64, offset: f64) -> Region2d {
        let mut region = Region2d::new(vec![vec![
            [offset, offset],
            [offset + size, offset],
            [offset + size, offset + size],
            [offset, offset + size],
        ]]);
        region.normalize();
        region
    }

    #[test]
    fn a_region_survives_a_prism_round_trip() {
        let region = square(10.0, 0.0);
        let back = from_prism(&to_prism(&region));
        assert_eq!(back.contours.len(), 1);
        assert!((back.area() - 100.0).abs() < 1e-9, "{}", back.area());
    }

    #[test]
    fn union_of_overlapping_squares_has_the_right_area() {
        let result = union(&square(10.0, 0.0), &square(10.0, 5.0));
        assert!((result.area() - (200.0 - 25.0)).abs() < 1e-9, "{}", result.area());
    }

    #[test]
    fn difference_of_squares_leaves_an_l() {
        let result = difference(&square(10.0, 0.0), &square(10.0, 5.0));
        assert!((result.area() - 75.0).abs() < 1e-9, "{}", result.area());
    }

    #[test]
    fn intersection_of_squares_is_the_overlap() {
        let result = intersection(&square(10.0, 0.0), &square(10.0, 5.0));
        assert!((result.area() - 25.0).abs() < 1e-9, "{}", result.area());
    }

    #[test]
    fn difference_can_produce_a_hole() {
        let outer = square(10.0, 0.0);
        let inner = square(2.0, 4.0);
        let result = difference(&outer, &inner);
        assert_eq!(result.contours.len(), 2);
        assert!((result.area() - 96.0).abs() < 1e-9, "{}", result.area());
    }

    /// A hollow body is what separates a silhouette from a filled outline, and
    /// it is where unioning both faces of every cap used to lose the hole.
    #[test]
    fn projection_shadow_of_a_tube_keeps_its_hole() {
        let outer = primitives::cylinder(10.0, 10.0, 10.0, false, 32);
        let inner = primitives::cylinder(12.0, 5.0, 5.0, false, 32).transformed(
            super::super::mesh::Matrix4::translation(Vec3::new(0.0, 0.0, -1.0)),
        );
        let region = projection_shadow(&outer.difference(&inner));
        let expected = primitives::circle(10.0, 32).area() - primitives::circle(5.0, 32).area();
        assert_eq!(region.contours.len(), 2);
        assert!(
            (region.area() - expected).abs() < 1e-6,
            "{} vs {expected}",
            region.area()
        );
    }

    /// The silhouette of a body that overhangs itself is one region, not the
    /// upper outline alone.
    #[test]
    fn projection_shadow_covers_every_overhang() {
        let lower = primitives::cube(Vec3::new(10.0, 10.0, 2.0), false);
        let upper = primitives::cube(Vec3::new(4.0, 4.0, 2.0), false).transformed(
            super::super::mesh::Matrix4::translation(Vec3::new(-2.0, -2.0, 4.0)),
        );
        let region = projection_shadow(&lower.union(&upper));
        // 100 for the slab, plus the quarter of the raised block hanging off
        // its corner.
        assert!((region.area() - 112.0).abs() < 1e-6, "{}", region.area());
    }

    #[test]
    fn projection_cut_slices_a_cube() {
        let solid = primitives::cube(Vec3::new(10.0, 10.0, 10.0), true);
        let region = projection_cut(&solid);
        assert!((region.area() - 100.0).abs() < 1e-9, "{}", region.area());
    }

    /// A body whose section varies with height is the only thing that can see
    /// which plane the cut is actually taken at.
    #[test]
    fn projection_cut_takes_the_section_at_z_zero_not_below_it() {
        // A cone of height 4 and base radius 4, apex up, based at z = -2: its
        // section at z = 0 has radius 2, and half a unit lower it would have
        // radius 2.5.
        let solid = primitives::cylinder(4.0, 4.0, 0.0, false, 64).transformed(
            super::super::mesh::Matrix4::translation(Vec3::new(0.0, 0.0, -2.0)),
        );
        let region = projection_cut(&solid);
        let expected = primitives::circle(2.0, 64).area();
        assert!(
            (region.area() - expected).abs() < 1e-6,
            "{} vs {expected}",
            region.area()
        );
    }

    /// Facets of the prism `linear_extrude(height)` builds over `region`,
    /// counted the way the exporter counts them.
    fn extruded_facets(region: &Region2d, height: f64) -> usize {
        primitives::linear_extrude(region, height, false, 0.0, [1.0, 1.0], 1)
            .sealed()
            .to_indexed_mesh()
            .triangles
            .len()
    }

    /// The `minkowski-2d` fixture, at kernel level.
    ///
    /// `square([14, 8]) (+) circle(2)` is a rectangle with rounded corners.
    /// Its area is fixed by the mixed-area identity
    /// `A(P (+) Q) = A(P) + A(Q) + sum over P's edges of len(e) * h_Q(n_e)`,
    /// which for an axis-aligned rectangle is `w*h + A(Q) + 2*(w + h)*r` when
    /// `Q` reaches `r` in all four axis directions — as a `$fn = 24` circle
    /// does, having vertices at 0, 90, 180 and 270 degrees. With an ideal
    /// circle that is `14*8 + 2*(14+8)*2 + pi*4`; here `A(Q)` is the 24-gon's
    /// area rather than `pi*4`.
    #[test]
    fn minkowski_rounds_a_rectangle() {
        let tool = primitives::circle(2.0, 24);
        let result = minkowski(&primitives::square([14.0, 8.0], false), &tool).unwrap();
        assert_eq!(result.contours.len(), 1);

        let expected = 14.0 * 8.0 + 2.0 * (14.0 + 8.0) * 2.0 + tool.area();
        assert!((result.area() - expected).abs() < 1.0e-9, "{}", result.area());
        // Same figure with a true circle, to 0.2%.
        let ideal = 14.0 * 8.0 + 2.0 * (14.0 + 8.0) * 2.0 + std::f64::consts::PI * 4.0;
        assert!((result.area() - ideal).abs() < 0.5, "{} vs {ideal}", result.area());

        let (min, max) = result.bounds().unwrap();
        for (got, want) in [(min, [-2.0, -2.0]), (max, [16.0, 10.0])] {
            assert!((got[0] - want[0]).abs() < 1.0e-9 && (got[1] - want[1]).abs() < 1.0e-9);
        }

        // 24 circle directions, plus the four axis directions where two corners
        // of the rectangle are both extreme and so contribute twice: 28
        // vertices, hence 56 side facets and two 26-triangle caps. 108 is what
        // the OpenSCAD binary put in `tests/fixtures/language/minkowski-2d.stl`.
        assert_eq!(result.contours[0].len(), 28);
        assert_eq!(extruded_facets(&result, 3.0), 108);
    }

    /// The non-convex path, checked against a route that does not use it.
    ///
    /// Minkowski distributes over union, so an L built from two rectangles
    /// summed with a square must equal the union of the two rectangles each
    /// grown by that square — which this computes with plain 2D booleans.
    #[test]
    fn minkowski_of_a_nonconvex_operand_matches_the_distributed_sum() {
        let arm = |w: f64, h: f64| {
            let mut region = Region2d::new(vec![vec![[0.0, 0.0], [w, 0.0], [w, h], [0.0, h]]]);
            region.normalize();
            region
        };
        let ell = union(&arm(10.0, 4.0), &arm(4.0, 10.0));
        assert!(ell.contours[0].len() > 4, "the L must actually be concave");

        let tool = primitives::square([2.0, 2.0], false);
        let result = minkowski(&ell, &tool).unwrap();
        let expected = union(&arm(12.0, 6.0), &arm(6.0, 12.0));

        assert_eq!(result.contours.len(), 1);
        assert!(
            (result.area() - expected.area()).abs() < 1.0e-9,
            "{} vs {}",
            result.area(),
            expected.area()
        );
        // 12*6 + 6*12 - 6*6, independently of the union above.
        assert!((result.area() - 108.0).abs() < 1.0e-9, "{}", result.area());
        let (min, max) = result.bounds().unwrap();
        assert!(min[0].abs() < 1.0e-9 && min[1].abs() < 1.0e-9);
        assert!((max[0] - 12.0).abs() < 1.0e-9 && (max[1] - 12.0).abs() < 1.0e-9);
    }

    /// A childless operand is absent, not empty: the sum is the other operand.
    #[test]
    fn minkowski_with_an_empty_operand_is_the_other_operand() {
        let disc = primitives::circle(3.0, 16);
        assert_eq!(minkowski(&Region2d::default(), &disc).unwrap(), disc);
        assert_eq!(minkowski(&disc, &Region2d::default()).unwrap(), disc);
    }

    /// Two concave operands multiply their piece counts, and the ceiling turns
    /// that into an error instead of an unbounded run.
    #[test]
    fn minkowski_refuses_a_pair_count_above_the_ceiling() {
        let points = (0..40)
            .map(|index| {
                let radians = (index as f64 * 9.0f64).to_radians();
                let radius = if index % 2 == 0 { 10.0 } else { 4.0 };
                [radius * radians.cos(), radius * radians.sin()]
            })
            .collect::<Vec<_>>();
        let mut star = Region2d::new(vec![points]);
        star.normalize();
        let error = minkowski(&star, &star).unwrap_err();
        assert!(error.contains("convex-piece sums"), "{error}");
    }

    /// The `fill-ring` fixture, at kernel level: a washer becomes a disc, and
    /// the disc extrudes to the 124 facets the binary produced.
    #[test]
    fn fill_turns_a_washer_into_a_disc() {
        let ring = difference(&primitives::circle(10.0, 32), &primitives::circle(6.0, 32));
        assert_eq!(ring.contours.len(), 2);

        let filled = fill(&ring);
        assert_eq!(filled.contours.len(), 1);
        let expected = primitives::circle(10.0, 32).area();
        assert!(
            (filled.area() - expected).abs() < 1.0e-9,
            "{} vs {expected}",
            filled.area()
        );
        assert_eq!(extruded_facets(&filled, 3.0), 124);
    }

    #[test]
    fn hull_of_two_squares_spans_both() {
        let result = hull(&[square(2.0, 0.0), square(2.0, 10.0)]);
        assert!(result.area() > 4.0);
    }
}
