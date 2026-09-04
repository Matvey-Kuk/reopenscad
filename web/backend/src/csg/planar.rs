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

/// `projection(cut = true)`: the slice of a solid at `z = 0`.
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
    from_prism(&solid.intersection(&slab))
}

/// `projection(cut = false)`: the shadow of a solid on the XY plane.
///
/// Computed as the union of the vertical prisms over every downward-facing
/// facet, which is exactly the silhouette of a closed solid.
pub fn projection_shadow(solid: &Solid) -> Region2d {
    let Some(bounds) = solid.bounds() else {
        return Region2d::default();
    };
    let height = bounds.size().z.max(1.0);
    let mut result = Region2d::default();
    let mesh = solid.to_indexed_mesh();
    for index in 0..mesh.triangles.len() {
        let [a, b, c] = mesh.triangle_points(index);
        let normal = b.sub(a).cross(c.sub(a));
        if normal.z.abs() <= EPSILON {
            continue;
        }
        let mut contour = vec![[a.x, a.y], [b.x, b.y], [c.x, c.y]];
        if super::poly2d::signed_area(&contour) < 0.0 {
            contour.reverse();
        }
        if super::poly2d::signed_area(&contour).abs() <= EPSILON {
            continue;
        }
        let mut piece = Region2d::new(vec![contour]);
        piece.normalize();
        result = union(&result, &piece);
    }
    let _ = height;
    result
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

    #[test]
    fn projection_cut_slices_a_cube() {
        let solid = primitives::cube(Vec3::new(10.0, 10.0, 10.0), true);
        let region = projection_cut(&solid);
        assert!((region.area() - 100.0).abs() < 1e-9, "{}", region.area());
    }

    #[test]
    fn hull_of_two_squares_spans_both() {
        let result = hull(&[square(2.0, 0.0), square(2.0, 10.0)]);
        assert!(result.area() > 4.0);
    }
}
