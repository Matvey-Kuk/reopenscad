//! Exact tessellation of OpenSCAD's built-in primitives.
//!
//! Everything here is deterministic and closed-form: no sampling, no adaptive
//! subdivision. The vertex counts follow OpenSCAD's documented fragment rule
//! (`$fn`, else `min(360/$fa, r*2*pi/$fs)` with a floor of 5), and the face
//! structure follows OpenSCAD's own conventions so that facet counts line up
//! with reference exports:
//!
//! * `cube` – 6 quads (12 triangles once exported).
//! * `cylinder` – two `n`-gon caps plus `n` side quads; a zero radius collapses
//!   the corresponding cap and turns the sides into triangles.
//! * `sphere` – `(n + 1) / 2` latitude rings of `n` points each, capped top and
//!   bottom, exactly as OpenSCAD lays them out.
//! * `polyhedron` – taken verbatim from the caller's points and faces.

use super::mesh::{Matrix4, Vec3};
use super::poly2d::{Point2, Region2d};
use super::solid::Solid;

/// Upper bound on the fragments generated for one curve, mirroring the
/// evaluator's own clamp so that pathological `$fn` values cannot explode.
pub const MAX_FRAGMENTS: usize = 10_000;

/// OpenSCAD's fragment rule.
///
/// `$fn > 0` wins outright; otherwise the count is
/// `ceil(min(360 / $fa, r * 2 * pi / $fs))` with a floor of 5 fragments.
pub fn fragments_for_radius(radius: f64, fn_value: f64, fa_value: f64, fs_value: f64) -> usize {
    if radius < 1.0e-6 {
        return 3;
    }
    let fragments = if fn_value > 0.0 {
        fn_value.max(3.0).floor()
    } else {
        let fa = fa_value.max(0.01);
        let fs = fs_value.max(0.01);
        (360.0 / fa)
            .min(radius * 2.0 * std::f64::consts::PI / fs)
            .max(5.0)
            .ceil()
    };
    (fragments as usize).clamp(3, MAX_FRAGMENTS)
}

/// The `n` points of OpenSCAD's generated circle of radius `r`, CCW from +X.
pub fn circle_points(radius: f64, fragments: usize) -> Vec<Point2> {
    (0..fragments)
        .map(|index| {
            let phi = (360.0 * index as f64) / fragments as f64;
            let radians = phi.to_radians();
            [radius * radians.cos(), radius * radians.sin()]
        })
        .collect()
}

/// `cube(size, center)`.
pub fn cube(size: Vec3, center: bool) -> Solid {
    if size.x <= 0.0 || size.y <= 0.0 || size.z <= 0.0 {
        return Solid::default();
    }
    let (min, max) = if center {
        (size.mul(-0.5), size.mul(0.5))
    } else {
        (Vec3::ZERO, size)
    };
    let corner = |x: bool, y: bool, z: bool| {
        Vec3::new(
            if x { max.x } else { min.x },
            if y { max.y } else { min.y },
            if z { max.z } else { min.z },
        )
    };
    // Each face is listed counter-clockwise seen from outside the cube.
    let faces = [
        // -Z (bottom)
        [
            corner(false, false, false),
            corner(false, true, false),
            corner(true, true, false),
            corner(true, false, false),
        ],
        // +Z (top)
        [
            corner(false, false, true),
            corner(true, false, true),
            corner(true, true, true),
            corner(false, true, true),
        ],
        // -Y (front)
        [
            corner(false, false, false),
            corner(true, false, false),
            corner(true, false, true),
            corner(false, false, true),
        ],
        // +Y (back)
        [
            corner(false, true, false),
            corner(false, true, true),
            corner(true, true, true),
            corner(true, true, false),
        ],
        // -X (left)
        [
            corner(false, false, false),
            corner(false, false, true),
            corner(false, true, true),
            corner(false, true, false),
        ],
        // +X (right)
        [
            corner(true, false, false),
            corner(true, true, false),
            corner(true, true, true),
            corner(true, false, true),
        ],
    ];
    Solid::from_faces(faces.into_iter().map(|face| face.to_vec()).collect())
}

/// `sphere(r)` with `fragments` points per latitude ring.
pub fn sphere(radius: f64, fragments: usize) -> Solid {
    if radius <= 0.0 || fragments < 3 {
        return Solid::default();
    }
    let rings = (fragments + 1) / 2;
    let mut ring_points: Vec<Vec<Vec3>> = Vec::with_capacity(rings);
    for index in 0..rings {
        let phi = (180.0 * (index as f64 + 0.5)) / rings as f64;
        let radians = phi.to_radians();
        let ring_radius = radius * radians.sin();
        let z = radius * radians.cos();
        ring_points.push(
            circle_points(ring_radius, fragments)
                .into_iter()
                .map(|point| Vec3::new(point[0], point[1], z))
                .collect(),
        );
    }

    let mut faces: Vec<Vec<Vec3>> = Vec::new();
    // Top cap: ring 0 has the largest z, and must wind CCW seen from +Z.
    faces.push(ring_points[0].clone());
    // Bottom cap, reversed so it faces -Z.
    let mut bottom = ring_points[rings - 1].clone();
    bottom.reverse();
    faces.push(bottom);
    // Side bands.
    for index in 0..rings - 1 {
        let upper = &ring_points[index];
        let lower = &ring_points[index + 1];
        for step in 0..fragments {
            let next = (step + 1) % fragments;
            // Wind around the lower ring first and then climb, so the face
            // normal points radially outward (phi_hat x z_hat = r_hat).
            faces.push(vec![lower[step], lower[next], upper[next], upper[step]]);
        }
    }
    Solid::from_faces(faces)
}

/// `cylinder(h, r1, r2, center)`.
pub fn cylinder(height: f64, radius1: f64, radius2: f64, center: bool, fragments: usize) -> Solid {
    if height <= 0.0 || fragments < 3 {
        return Solid::default();
    }
    let (bottom_z, top_z) = if center {
        (-height / 2.0, height / 2.0)
    } else {
        (0.0, height)
    };
    let radius1 = radius1.max(0.0);
    let radius2 = radius2.max(0.0);
    if radius1 <= 0.0 && radius2 <= 0.0 {
        return Solid::default();
    }

    let lower: Vec<Vec3> = circle_points(radius1, fragments)
        .into_iter()
        .map(|point| Vec3::new(point[0], point[1], bottom_z))
        .collect();
    let upper: Vec<Vec3> = circle_points(radius2, fragments)
        .into_iter()
        .map(|point| Vec3::new(point[0], point[1], top_z))
        .collect();

    let mut faces: Vec<Vec<Vec3>> = Vec::new();
    if radius1 > 0.0 {
        // Bottom cap faces -Z, so reverse the CCW-from-above ring.
        let mut cap = lower.clone();
        cap.reverse();
        faces.push(cap);
    }
    if radius2 > 0.0 {
        faces.push(upper.clone());
    }
    for index in 0..fragments {
        let next = (index + 1) % fragments;
        if radius1 <= 0.0 {
            // Cone with the apex at the bottom.
            faces.push(vec![lower[0], upper[next], upper[index]]);
        } else if radius2 <= 0.0 {
            // Cone with the apex at the top.
            faces.push(vec![lower[index], lower[next], upper[0]]);
        } else {
            faces.push(vec![lower[index], lower[next], upper[next], upper[index]]);
        }
    }
    Solid::from_faces(faces)
}

/// `polyhedron(points, faces)`.
///
/// OpenSCAD's convention is that each face is listed **clockwise seen from
/// outside**, which is the opposite of the kernel's internal winding, so every
/// face is reversed on the way in.
pub fn polyhedron(points: &[Vec3], faces: &[Vec<usize>]) -> Result<Solid, String> {
    let mut output = Vec::with_capacity(faces.len());
    for (index, face) in faces.iter().enumerate() {
        if face.len() < 3 {
            return Err(format!("polyhedron face {index} has fewer than 3 points"));
        }
        let mut vertices = Vec::with_capacity(face.len());
        for point_index in face.iter().rev() {
            let point = points
                .get(*point_index)
                .ok_or_else(|| format!("polyhedron face {index} references point {point_index}"))?;
            vertices.push(*point);
        }
        output.push(vertices);
    }
    Ok(Solid::from_faces(output))
}

/// `square(size, center)` as a 2D region.
pub fn square(size: [f64; 2], center: bool) -> Region2d {
    if size[0] <= 0.0 || size[1] <= 0.0 {
        return Region2d::default();
    }
    let (min, max) = if center {
        (
            [-size[0] / 2.0, -size[1] / 2.0],
            [size[0] / 2.0, size[1] / 2.0],
        )
    } else {
        ([0.0, 0.0], [size[0], size[1]])
    };
    Region2d::new(vec![vec![
        [min[0], min[1]],
        [max[0], min[1]],
        [max[0], max[1]],
        [min[0], max[1]],
    ]])
}

/// `circle(r)` as a 2D region.
pub fn circle(radius: f64, fragments: usize) -> Region2d {
    if radius <= 0.0 || fragments < 3 {
        return Region2d::default();
    }
    Region2d::new(vec![circle_points(radius, fragments)])
}

/// `polygon(points, paths)`.
///
/// With no `paths` the point list is a single closed contour. With `paths`,
/// each path indexes into `points`; the first path is the outline and the rest
/// are holes. Winding is normalised by nesting depth, so callers do not have to
/// get it right — this is what the SDF evaluator could not do at all.
pub fn polygon(points: &[Point2], paths: Option<&[Vec<usize>]>) -> Result<Region2d, String> {
    let contours = match paths {
        None => {
            if points.len() < 3 {
                return Ok(Region2d::default());
            }
            vec![points.to_vec()]
        }
        Some(paths) => {
            let mut contours = Vec::with_capacity(paths.len());
            for (index, path) in paths.iter().enumerate() {
                if path.len() < 3 {
                    continue;
                }
                let mut contour = Vec::with_capacity(path.len());
                for point_index in path {
                    let point = points.get(*point_index).ok_or_else(|| {
                        format!("polygon path {index} references point {point_index}")
                    })?;
                    contour.push(*point);
                }
                contours.push(contour);
            }
            contours
        }
    };
    let mut region = Region2d::new(contours);
    region.normalize();
    Ok(region)
}

/// `linear_extrude(height, center, twist, scale, slices)` of a 2D region.
pub fn linear_extrude(
    region: &Region2d,
    height: f64,
    center: bool,
    twist: f64,
    scale: [f64; 2],
    slices: usize,
) -> Solid {
    if height <= 0.0 || region.is_empty() {
        return Solid::default();
    }
    let mut region = region.clone();
    region.normalize();
    if region.is_empty() {
        return Solid::default();
    }
    let (bottom_z, top_z) = if center {
        (-height / 2.0, height / 2.0)
    } else {
        (0.0, height)
    };
    let slices = if twist != 0.0 || scale != [1.0, 1.0] {
        slices.max(1)
    } else {
        1
    };

    let level = |contour: &[Point2], step: usize| -> Vec<Vec3> {
        let t = step as f64 / slices as f64;
        let z = bottom_z + (top_z - bottom_z) * t;
        let sx = 1.0 + (scale[0] - 1.0) * t;
        let sy = 1.0 + (scale[1] - 1.0) * t;
        let angle = (-twist * t).to_radians();
        let (sin, cos) = angle.sin_cos();
        contour
            .iter()
            .map(|point| {
                let x = point[0] * sx;
                let y = point[1] * sy;
                Vec3::new(x * cos - y * sin, x * sin + y * cos, z)
            })
            .collect()
    };

    let mut faces: Vec<Vec<Vec3>> = Vec::new();

    // Bottom cap, wound so it faces -Z.
    for triangle in region.triangulate() {
        faces.push(vec![
            Vec3::new(triangle[0][0], triangle[0][1], bottom_z),
            Vec3::new(triangle[2][0], triangle[2][1], bottom_z),
            Vec3::new(triangle[1][0], triangle[1][1], bottom_z),
        ]);
    }
    // Top cap, transformed by the final twist/scale.
    let top_scale = scale;
    let top_angle = (-twist).to_radians();
    let (top_sin, top_cos) = top_angle.sin_cos();
    for triangle in region.triangulate() {
        let mapped: Vec<Vec3> = triangle
            .iter()
            .map(|point| {
                let x = point[0] * top_scale[0];
                let y = point[1] * top_scale[1];
                Vec3::new(x * top_cos - y * top_sin, x * top_sin + y * top_cos, top_z)
            })
            .collect();
        faces.push(mapped);
    }

    // Walls. Contours are already oriented (outer CCW, holes CW) so the same
    // winding rule produces outward normals for both.
    for contour in &region.contours {
        for step in 0..slices {
            let lower = level(contour, step);
            let upper = level(contour, step + 1);
            for index in 0..contour.len() {
                let next = (index + 1) % contour.len();
                push_wall(
                    &mut faces,
                    [lower[index], lower[next], upper[next], upper[index]],
                );
            }
        }
    }

    Solid::from_faces(faces)
}

/// Pushes one wall panel, splitting it when its four corners do not share a
/// plane.
///
/// A swept wall is a quadrilateral only by accident. Twist a profile, taper it,
/// or revolve one whose two ends sit at different radii *and* different
/// heights, and the panel's corners stop being coplanar — by a fraction of a
/// millimetre, which is exactly enough to matter. Every polygon in this kernel
/// carries a plane, booleans decide "inside" by classifying points against
/// those planes, and a plane fitted to four points that do not share one is
/// wrong everywhere except at the fit. The result is a solid that looks right
/// and tears the moment anything is subtracted from it: a helical thread built
/// this way came out of the kernel with 47000 unpaired edges.
///
/// Two triangles are always exactly planar, so a non-planar panel is split
/// along its shorter diagonal — the one that leaves the better-conditioned
/// pair, and the one a well-behaved mesher would pick.
fn push_wall(faces: &mut Vec<Vec<Vec3>>, corners: [Vec3; 4]) {
    let [a, b, c, d] = corners;
    // Distance of the fourth corner from the plane of the first three, relative
    // to the panel's own size, so the test means the same thing on a 0.1 mm
    // thread crest and a 300 mm wall.
    let normal = b.sub(a).cross(c.sub(a));
    let scale = normal.length();
    let out_of_plane = if scale > 0.0 {
        normal.dot(d.sub(a)).abs() / scale
    } else {
        0.0
    };
    let extent = b.sub(a).length().max(c.sub(b).length()).max(1.0);
    if scale > 0.0 && out_of_plane <= extent * PLANAR_WALL_TOLERANCE {
        faces.push(vec![a, b, c, d]);
        return;
    }
    if a.sub(c).length() <= b.sub(d).length() {
        faces.push(vec![a, b, c]);
        faces.push(vec![a, c, d]);
    } else {
        faces.push(vec![a, b, d]);
        faces.push(vec![b, c, d]);
    }
}

/// How far a wall panel's fourth corner may sit from the plane of the other
/// three, as a fraction of the panel's own edge length, before it is split.
///
/// Tight enough that a genuinely curved panel is always split, loose enough
/// that a flat one built by floating-point rotation is not — an untwisted
/// extrusion's walls are exactly planar in exact arithmetic and a few ulps out
/// in practice, and splitting those would change the facet set of every
/// straight-sided model in the corpus for no benefit.
const PLANAR_WALL_TOLERANCE: f64 = 1.0e-12;

/// `rotate_extrude(angle)` of a 2D region living in the XZ half-plane `x >= 0`.
pub fn rotate_extrude(region: &Region2d, angle: f64, fragments: usize) -> Solid {
    if region.is_empty() || fragments < 3 {
        return Solid::default();
    }
    let mut region = region.clone();
    region.normalize();
    if region.is_empty() {
        return Solid::default();
    }
    let full = angle.abs() >= 360.0 - 1.0e-9;
    let sweep = if full { 360.0 } else { angle };
    let steps = if full {
        fragments
    } else {
        ((fragments as f64 * sweep.abs() / 360.0).ceil() as usize).max(1)
    };

    let place = |point: Point2, step: usize| -> Vec3 {
        let theta = (sweep * step as f64 / steps as f64).to_radians();
        let (sin, cos) = theta.sin_cos();
        Vec3::new(point[0] * cos, point[0] * sin, point[1])
    };

    let mut faces: Vec<Vec<Vec3>> = Vec::new();
    let ring_count = if full { steps } else { steps + 1 };
    for contour in &region.contours {
        for step in 0..steps {
            let next_step = (step + 1) % ring_count;
            for index in 0..contour.len() {
                let next = (index + 1) % contour.len();
                // (u, v, theta) is a left-handed frame, so the profile order
                // has to be reversed for the walls to face outward.
                push_wall(
                    &mut faces,
                    [
                        place(contour[index], next_step),
                        place(contour[next], next_step),
                        place(contour[next], step),
                        place(contour[index], step),
                    ],
                );
            }
        }
    }
    if !full {
        // Close the two open ends with the profile itself.
        for triangle in region.triangulate() {
            // A CCW profile triangle in (u, v) already faces -Y at theta = 0,
            // which is the outward direction for the start cap.
            faces.push(vec![
                place(triangle[0], 0),
                place(triangle[1], 0),
                place(triangle[2], 0),
            ]);
            faces.push(vec![
                place(triangle[0], steps),
                place(triangle[2], steps),
                place(triangle[1], steps),
            ]);
        }
    }
    Solid::from_faces(faces)
}

/// The convex hull of a set of solids, as `hull()` defines it in 3D.
pub fn hull(solids: &[Solid]) -> Solid {
    let mut points: Vec<Vec3> = Vec::new();
    for solid in solids {
        for polygon in &solid.polygons {
            points.extend(polygon.vertices.iter().copied());
        }
    }
    let faces = super::hull::convex_hull(&points);
    Solid { polygons: faces }
}

/// Largest number of convex-piece pairs `minkowski` will evaluate.
///
/// The Minkowski sum of two non-convex bodies costs one hull plus one union per
/// pair of convex pieces, so the work is the *product* of the two
/// decompositions. Without a ceiling a moderately concave operand turns into
/// hours of unionising; the ceiling turns that into an actionable error.
pub const MAX_MINKOWSKI_PAIRS: usize = 256;

/// `minkowski()` of two solids.
///
/// Computed exactly: the Minkowski sum of two convex bodies is the convex hull
/// of the pairwise vertex sums, and for non-convex operands the identity
/// `A (+) B = union over convex pieces Ai, Bj of (Ai (+) Bj)` reduces the
/// general case to that one. Both operands are decomposed with
/// [`Solid::convex_decomposition`], which reads convex cells straight off a BSP
/// tree, so the decomposition itself is exact.
///
/// Returns an error rather than running unbounded when the decompositions are
/// large enough that the pairwise sum would not finish in reasonable time.
pub fn minkowski(left: &Solid, right: &Solid) -> Result<Solid, String> {
    if left.polygons.is_empty() {
        return Ok(right.clone());
    }
    if right.polygons.is_empty() {
        return Ok(left.clone());
    }
    let left_pieces = left.convex_decomposition();
    let right_pieces = right.convex_decomposition();
    let pairs = left_pieces.len().saturating_mul(right_pieces.len());
    if pairs > MAX_MINKOWSKI_PAIRS {
        return Err(format!(
            "minkowski() would need {pairs} convex-piece sums ({} x {}), above the {MAX_MINKOWSKI_PAIRS} limit",
            left_pieces.len(),
            right_pieces.len()
        ));
    }
    let mut result = Solid::default();
    for left_piece in &left_pieces {
        for right_piece in &right_pieces {
            let mut points = Vec::with_capacity(left_piece.len() * right_piece.len());
            for a in left_piece {
                for b in right_piece {
                    points.push(a.add(*b));
                }
            }
            let piece = Solid {
                polygons: super::hull::convex_hull(&points),
            };
            if piece.polygons.is_empty() {
                continue;
            }
            result = if result.polygons.is_empty() {
                piece
            } else {
                result.union(&piece)
            };
        }
    }
    Ok(result)
}

/// `resize(newsize, auto)`.
///
/// The transform is a plain scaling about the origin, not about the child's
/// own box: `resize()` in OpenSCAD moves a part that is not already centred.
pub fn resize(solid: &Solid, newsize: Vec3, auto: [bool; 3]) -> Solid {
    let Some(bounds) = solid.bounds() else {
        return solid.clone();
    };
    solid.transformed(Matrix4::scaling(resize_factors(
        bounds.size(),
        newsize,
        auto,
    )))
}

/// The per-axis scale factors `resize(newsize, auto)` applies to a body of
/// extent `size`.
///
/// A zero (or negative) component means "leave that axis alone", and an `auto`
/// axis without a size of its own copies the factor of the axis with the
/// *largest requested* size — not the first one given, which is what
/// `resize([0, 5, 20], auto = true)` distinguishes.
pub fn resize_factors(size: Vec3, newsize: Vec3, auto: [bool; 3]) -> Vec3 {
    let mut factors = [1.0f64; 3];
    for (axis, factor) in factors.iter_mut().enumerate() {
        let target = newsize.component(axis);
        let current = size.component(axis);
        if target > 0.0 && current > 0.0 {
            *factor = target / current;
        }
    }
    let largest = (0..3).fold(0, |largest, axis| {
        if newsize.component(axis) > newsize.component(largest) {
            axis
        } else {
            largest
        }
    });
    for axis in 0..3 {
        if auto[axis] && newsize.component(axis) <= 0.0 {
            factors[axis] = factors[largest];
        }
    }
    Vec3::new(factors[0], factors[1], factors[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cube_is_exactly_six_quads_and_twelve_triangles() {
        let solid = cube(Vec3::new(20.0, 20.0, 20.0), false);
        assert_eq!(solid.polygons.len(), 6);
        assert!(solid.polygons.iter().all(|face| face.vertices.len() == 4));
        let mesh = solid.to_indexed_mesh();
        assert_eq!(mesh.triangles.len(), 12);
        assert_eq!(mesh.vertices.len(), 8);
        assert!((mesh.volume() - 8000.0).abs() < 1e-9);
        assert!((mesh.surface_area() - 2400.0).abs() < 1e-9);
        assert!(mesh.is_manifold());
        assert_eq!(mesh.statistics().euler_characteristic, 2);
    }

    #[test]
    fn a_cube_has_the_expected_corner_positions() {
        let solid = cube(Vec3::new(2.0, 3.0, 4.0), false);
        let mesh = solid.to_indexed_mesh();
        let bounds = mesh.bounds().unwrap();
        assert_eq!(bounds.min, Vec3::ZERO);
        assert_eq!(bounds.max, Vec3::new(2.0, 3.0, 4.0));

        let centered = cube(Vec3::new(2.0, 3.0, 4.0), true).to_indexed_mesh();
        let bounds = centered.bounds().unwrap();
        assert_eq!(bounds.min, Vec3::new(-1.0, -1.5, -2.0));
        assert_eq!(bounds.max, Vec3::new(1.0, 1.5, 2.0));
    }

    #[test]
    fn cube_faces_point_outward() {
        let solid = cube(Vec3::new(1.0, 1.0, 1.0), true);
        for polygon in &solid.polygons {
            let centroid = polygon
                .vertices
                .iter()
                .fold(Vec3::ZERO, |sum, point| sum.add(*point))
                .mul(1.0 / polygon.vertices.len() as f64);
            assert!(
                polygon.plane.normal.dot(centroid) > 0.0,
                "face normal {:?} is not outward",
                polygon.plane.normal
            );
        }
    }

    #[test]
    fn the_fragment_rule_matches_openscad() {
        assert_eq!(fragments_for_radius(10.0, 8.0, 12.0, 2.0), 8);
        assert_eq!(fragments_for_radius(10.0, 2.0, 12.0, 2.0), 3);
        // $fa-limited: 360/12 = 30 fragments, circumference rule gives 31.4.
        assert_eq!(fragments_for_radius(10.0, 0.0, 12.0, 2.0), 30);
        // $fs-limited.
        assert_eq!(fragments_for_radius(1.0, 0.0, 12.0, 2.0), 5);
        assert_eq!(fragments_for_radius(0.0, 0.0, 12.0, 2.0), 3);
        assert_eq!(fragments_for_radius(100.0, 0.0, 12.0, 2.0), 30);
        assert_eq!(fragments_for_radius(5.0, 0.0, 1.0, 0.1), 315);
    }

    #[test]
    fn a_faceted_cylinder_has_the_openscad_face_structure() {
        let solid = cylinder(10.0, 5.0, 5.0, false, 8);
        // Two 8-gon caps plus 8 side quads.
        assert_eq!(solid.polygons.len(), 10);
        let mesh = solid.to_indexed_mesh();
        assert_eq!(mesh.vertices.len(), 16);
        // Caps fan into 6 triangles each, sides into 16.
        assert_eq!(mesh.triangles.len(), 2 * 6 + 16);
        assert!(mesh.is_manifold());
        // Exact volume of a regular octagonal prism.
        let expected = 8.0 * 0.5 * 25.0 * (2.0 * std::f64::consts::PI / 8.0).sin() * 10.0;
        assert!((mesh.volume() - expected).abs() < 1e-9, "{}", mesh.volume());
    }

    #[test]
    fn a_cone_collapses_the_zero_radius_cap() {
        let solid = cylinder(10.0, 5.0, 0.0, false, 8);
        // One cap plus 8 side triangles.
        assert_eq!(solid.polygons.len(), 9);
        let mesh = solid.to_indexed_mesh();
        assert_eq!(mesh.vertices.len(), 9);
        assert_eq!(mesh.triangles.len(), 6 + 8);
        assert!(mesh.is_manifold());
        let base = 8.0 * 0.5 * 25.0 * (2.0 * std::f64::consts::PI / 8.0).sin();
        assert!((mesh.volume() - base * 10.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn a_centred_cylinder_straddles_the_origin() {
        let mesh = cylinder(10.0, 5.0, 5.0, true, 6).to_indexed_mesh();
        let bounds = mesh.bounds().unwrap();
        assert!((bounds.min.z + 5.0).abs() < 1e-12);
        assert!((bounds.max.z - 5.0).abs() < 1e-12);
    }

    #[test]
    fn a_faceted_sphere_has_the_openscad_ring_structure() {
        let solid = sphere(10.0, 8);
        // rings = (8 + 1) / 2 = 4; two caps plus 3 bands of 8 quads.
        assert_eq!(solid.polygons.len(), 2 + 3 * 8);
        let mesh = solid.to_indexed_mesh();
        assert_eq!(mesh.vertices.len(), 4 * 8);
        assert!(mesh.is_manifold());
        assert_eq!(mesh.statistics().euler_characteristic, 2);
        assert!(mesh.volume() > 0.0 && mesh.volume() < 4.0 / 3.0 * std::f64::consts::PI * 1000.0);
    }

    #[test]
    fn a_finer_sphere_converges_on_the_analytic_volume() {
        // The inscribed polyhedron converges quadratically in the fragment
        // count, and stays watertight at every resolution.
        let exact = 4.0 / 3.0 * std::f64::consts::PI * 1000.0;
        let mut previous = f64::INFINITY;
        for fragments in [16usize, 32, 64, 128] {
            let mesh = sphere(10.0, fragments).to_indexed_mesh();
            assert!(mesh.is_manifold(), "$fn={fragments}: {}", mesh.statistics());
            assert_eq!(mesh.statistics().euler_characteristic, 2);
            let error = (exact - mesh.volume()) / exact;
            assert!(error > 0.0, "the faceted sphere must be inscribed");
            assert!(error < previous / 3.0, "$fn={fragments}: {error:e}");
            previous = error;
        }
        let mesh = sphere(10.0, 128).to_indexed_mesh();
        assert!((mesh.volume() - exact).abs() / exact < 1.1e-3);
    }

    #[test]
    fn polyhedron_round_trips_a_tetrahedron() {
        let points = [
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(0.0, 0.0, 1.0),
        ];
        // OpenSCAD lists faces clockwise seen from outside.
        let faces = vec![vec![0, 1, 2], vec![0, 2, 3], vec![0, 3, 1], vec![1, 3, 2]];
        let solid = polyhedron(&points, &faces).unwrap();
        let mesh = solid.to_indexed_mesh();
        assert_eq!(mesh.triangles.len(), 4);
        assert_eq!(mesh.vertices.len(), 4);
        assert!(mesh.is_manifold());
        assert!(
            (mesh.volume() - 1.0 / 6.0).abs() < 1e-12,
            "{}",
            mesh.volume()
        );
    }

    #[test]
    fn polyhedron_rejects_a_bad_index() {
        let points = [
            Vec3::ZERO,
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(0.0, 1.0, 0.0),
        ];
        assert!(polyhedron(&points, &[vec![0, 1, 9]]).is_err());
    }

    #[test]
    fn linear_extrude_of_a_square_is_a_box() {
        let region = square([10.0, 10.0], false);
        let solid = linear_extrude(&region, 5.0, false, 0.0, [1.0, 1.0], 1);
        let mesh = solid.to_indexed_mesh();
        assert!(mesh.is_manifold(), "{}", mesh.statistics());
        assert!((mesh.volume() - 500.0).abs() < 1e-9, "{}", mesh.volume());
        assert!((mesh.surface_area() - (2.0 * 100.0 + 4.0 * 50.0)).abs() < 1e-9);
    }

    #[test]
    fn linear_extrude_of_a_ring_keeps_the_hole() {
        let outer = square([10.0, 10.0], true);
        let inner = square([4.0, 4.0], true);
        let mut region = Region2d::new(
            outer
                .contours
                .into_iter()
                .chain(inner.contours)
                .collect::<Vec<_>>(),
        );
        region.normalize();
        let solid = linear_extrude(&region, 2.0, false, 0.0, [1.0, 1.0], 1);
        let mesh = solid.to_indexed_mesh();
        assert!(mesh.is_manifold(), "{}", mesh.statistics());
        assert!(
            (mesh.volume() - (100.0 - 16.0) * 2.0).abs() < 1e-9,
            "{}",
            mesh.volume()
        );
        // A solid with one through-hole is a torus: Euler characteristic 0.
        assert_eq!(mesh.statistics().euler_characteristic, 0);
    }

    #[test]
    fn hull_of_two_cubes_is_convex() {
        let a = cube(Vec3::new(10.0, 10.0, 10.0), false);
        let b = cube(Vec3::new(10.0, 10.0, 10.0), false)
            .transformed(Matrix4::translation(Vec3::new(20.0, 0.0, 0.0)));
        let solid = hull(&[a, b]);
        let mesh = solid.to_indexed_mesh();
        assert!(mesh.is_manifold(), "{}", mesh.statistics());
        assert!((mesh.volume() - 3000.0).abs() < 1e-6, "{}", mesh.volume());
    }

    #[test]
    fn rotate_extrude_of_an_offset_square_is_a_ring() {
        let mut region = Region2d::new(vec![vec![
            [10.0, -1.0],
            [12.0, -1.0],
            [12.0, 1.0],
            [10.0, 1.0],
        ]]);
        region.normalize();
        let mesh = rotate_extrude(&region, 360.0, 64).to_indexed_mesh();
        assert!(mesh.is_manifold(), "{}", mesh.statistics());
        // Pappus: volume = 2*pi*centroid_radius*area, approached from below by
        // the faceted approximation.
        let exact = 2.0 * std::f64::consts::PI * 11.0 * 4.0;
        assert!(
            (mesh.volume() - exact).abs() / exact < 1e-2,
            "{}",
            mesh.volume()
        );
    }
}
