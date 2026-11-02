//! `color()` at any depth.
//!
//! OpenSCAD's preview colours a subtree wherever `color()` sits in it: inside
//! a module, under an `if` or a `for`, under transforms, on some children of a
//! union. One top-level statement is still one object, so an object can carry
//! several colours; the exact kernel keeps each face's colour through the
//! booleans and the object is meshed colour by colour ([`FaceColors`]).
//!
//! The rules pinned here:
//!
//! * an inner `color()` wins over an outer one;
//! * a union keeps each child's colour, and two coplanar faces of different
//!   colours stay two faces, with a closed seam between them;
//! * a difference's cut walls take the tool's colour when it states one, and
//!   the base's otherwise (so `difference() { color("red") a; b; }` is all
//!   red, as it has always been here);
//! * an object of one colour reports it in `color` with no `face_colors`, so
//!   its mesh is the same facet set an uncoloured object gets.

use super::*;

const RED: [f64; 4] = [1.0, 0.0, 0.0, 1.0];
const BLUE: [f64; 4] = [0.0, 0.0, 1.0, 1.0];
const GREEN: [f64; 4] = [0.0, 128.0 / 255.0, 0.0, 1.0];

fn parts(source: &str) -> Vec<CompiledPart> {
    compile_parts(source, Quality::Render, None)
        .unwrap_or_else(|error| panic!("compile: {error}"))
        .parts
}

fn one_part(source: &str) -> CompiledPart {
    let mut parts = parts(source);
    assert_eq!(parts.len(), 1, "expected one object for {source}");
    parts.remove(0)
}

fn close(a: [f64; 4], b: [f64; 4]) -> bool {
    a.iter().zip(b).all(|(a, b)| (a - b).abs() <= 0.5 / 255.0)
}

fn centroid(triangle: &Triangle) -> Vec3 {
    let [a, b, c] = triangle.vertices;
    Vec3::new(
        (a.x + b.x + c.x) / 3.0,
        (a.y + b.y + c.y) / 3.0,
        (a.z + b.z + c.z) / 3.0,
    )
}

fn area(triangle: &Triangle) -> f64 {
    let [a, b, c] = triangle.vertices;
    b.sub(a).cross(c.sub(a)).length() / 2.0
}

/// The triangles painted `color`, by index.
fn painted(part: &CompiledPart, color: [f64; 4]) -> Vec<&Triangle> {
    let colors = part.face_colors.as_ref().expect("per-face colours");
    part.mesh
        .triangles
        .iter()
        .enumerate()
        .filter(|(index, _)| colors.color_of(*index).is_some_and(|c| close(c, color)))
        .map(|(_, triangle)| triangle)
        .collect()
}

fn painted_area(part: &CompiledPart, color: [f64; 4]) -> f64 {
    painted(part, color).into_iter().map(area).sum()
}

fn inside(point: Vec3, min: [f64; 3], max: [f64; 3]) -> bool {
    const SLACK: f64 = 1e-6;
    [point.x, point.y, point.z]
        .iter()
        .enumerate()
        .all(|(axis, value)| *value >= min[axis] - SLACK && *value <= max[axis] + SLACK)
}

/// Every edge used by exactly two triangles, in opposite directions: the
/// colour seams must not open the solid.
fn assert_closed(mesh: &Mesh) {
    let key = |v: Vec3| [v.x, v.y, v.z].map(|c| (c * 1e6).round() as i64);
    let mut edges: std::collections::HashMap<([i64; 3], [i64; 3]), i32> =
        std::collections::HashMap::new();
    for triangle in &mesh.triangles {
        for index in 0..3 {
            let from = key(triangle.vertices[index]);
            let to = key(triangle.vertices[(index + 1) % 3]);
            *edges.entry((from, to)).or_default() += 1;
            *edges.entry((to, from)).or_default() -= 1;
        }
    }
    let open = edges.values().filter(|count| **count != 0).count();
    assert_eq!(open, 0, "the coloured mesh has {open} unmatched half-edges");
}

#[test]
fn colors_inside_a_module_paint_one_object_in_several_colors() {
    let part = one_part(
        "module m() { color(\"red\") cube(10); translate([20, 0, 0]) color(\"blue\") sphere(5); }
         m();",
    );
    let colors = part.face_colors.as_ref().expect("two colours");
    assert_eq!(colors.faces.len(), part.mesh.triangles.len());
    assert!(colors.palette.iter().any(|c| c.is_some_and(|c| close(c, RED))));
    assert!(colors.palette.iter().any(|c| c.is_some_and(|c| close(c, BLUE))));
    for triangle in painted(&part, RED) {
        assert!(inside(centroid(triangle), [0.0; 3], [10.0; 3]));
    }
    for triangle in painted(&part, BLUE) {
        assert!(inside(centroid(triangle), [15.0, -5.0, -5.0], [25.0, 5.0, 5.0]));
    }
    // The cube is all red: its full surface.
    assert!((painted_area(&part, RED) - 600.0).abs() < 1e-6);
    // Each colour's triangles are contiguous: two runs.
    assert_eq!(colors.runs().len(), 2);
    // The swatch colour is the one covering the most triangles.
    assert!(part.color.is_some());
}

#[test]
fn a_color_under_a_top_level_if_colors_the_object() {
    let part = one_part("if (true) color(\"green\") cube(5);");
    assert!(close(part.color.expect("green"), GREEN));
    assert!(part.face_colors.is_none(), "one colour needs no per-face table");

    // A module whose body is one colour reads as that colour too.
    let part = one_part("module m() color(\"green\") cube(5); m();");
    assert!(close(part.color.expect("green"), GREEN));
    assert!(part.face_colors.is_none());
}

#[test]
fn a_top_level_for_paints_each_iteration_its_own_color() {
    let part = one_part(
        "for (i = [0 : 2]) translate([i * 10, 0, 0]) color(i == 1 ? \"red\" : \"blue\") cube(5);",
    );
    for triangle in painted(&part, RED) {
        assert!(inside(centroid(triangle), [10.0, 0.0, 0.0], [15.0, 5.0, 5.0]));
    }
    assert!((painted_area(&part, RED) - 150.0).abs() < 1e-6);
    assert!((painted_area(&part, BLUE) - 300.0).abs() < 1e-6);
}

#[test]
fn the_inner_color_wins_over_the_outer_one() {
    let part = one_part(
        "color(\"red\") { cube(5); color(\"blue\") translate([10, 0, 0]) cube(5); }",
    );
    for triangle in painted(&part, RED) {
        assert!(inside(centroid(triangle), [0.0; 3], [5.0; 3]));
    }
    for triangle in painted(&part, BLUE) {
        assert!(inside(centroid(triangle), [10.0, 0.0, 0.0], [15.0, 5.0, 5.0]));
    }
    assert!((painted_area(&part, RED) - 150.0).abs() < 1e-6);
    assert!((painted_area(&part, BLUE) - 150.0).abs() < 1e-6);
}

#[test]
fn alpha_travels_with_the_face_color() {
    let part = one_part(
        "module m() { color(\"red\", 0.5) cube(5); translate([10, 0, 0]) color([0, 0, 1]) cube(5); }
         m();",
    );
    let colors = part.face_colors.as_ref().expect("two colours");
    let translucent = colors
        .palette
        .iter()
        .flatten()
        .find(|color| color[3] < 1.0)
        .expect("a translucent colour");
    assert!(close(*translucent, [1.0, 0.0, 0.0, 0.5]), "{translucent:?}");
}

#[test]
fn colors_follow_their_shapes_through_translate_and_rotate() {
    let part = one_part(
        "module m() { rotate([0, 0, 90]) translate([10, 0, 0]) color(\"red\") cube(2); color(\"blue\") cube(2); }
         m();",
    );
    // rotate 90 about z maps [10, 12] x [0, 2] to [-2, 0] x [10, 12].
    let red = painted(&part, RED);
    assert!(!red.is_empty());
    for triangle in red {
        assert!(inside(centroid(triangle), [-2.0, 10.0, 0.0], [0.0, 12.0, 2.0]));
    }
    for triangle in painted(&part, BLUE) {
        assert!(inside(centroid(triangle), [0.0; 3], [2.0; 3]));
    }
}

#[test]
fn a_union_keeps_each_child_color_and_a_closed_seam() {
    // Overlapping: only the surface each child contributes keeps its colour.
    let part = one_part(
        "union() { color(\"red\") cube(10); color(\"blue\") translate([5, 5, 5]) cube(10); }",
    );
    for triangle in painted(&part, RED) {
        assert!(inside(centroid(triangle), [0.0; 3], [10.0; 3]));
    }
    for triangle in painted(&part, BLUE) {
        assert!(inside(centroid(triangle), [5.0; 3], [15.0; 3]));
    }
    // Each cube loses three 5 x 5 squares to the other.
    assert!((painted_area(&part, RED) - 525.0).abs() < 1e-6);
    assert!((painted_area(&part, BLUE) - 525.0).abs() < 1e-6);
    assert_closed(&part.mesh);

    // Touching: the shared top and bottom planes split exactly at x = 10, and
    // the internal wall is gone.
    let part = one_part(
        "union() { color(\"red\") cube(10); color(\"blue\") translate([10, 0, 0]) cube(10); }",
    );
    for triangle in painted(&part, RED) {
        assert!(centroid(triangle).x < 10.0);
    }
    for triangle in painted(&part, BLUE) {
        assert!(centroid(triangle).x > 10.0);
    }
    assert!((painted_area(&part, RED) - 500.0).abs() < 1e-6);
    assert!((painted_area(&part, BLUE) - 500.0).abs() < 1e-6);
    assert_closed(&part.mesh);
}

#[test]
fn a_difference_cut_takes_the_tool_color_or_else_the_base_color() {
    // An uncoloured tool: the whole object is the base's colour.
    let part = one_part("difference() { color(\"red\") cube(10); translate([5, 5, 5]) cube(10); }");
    assert!(close(part.color.expect("red"), RED));
    assert!(part.face_colors.is_none());

    // A coloured tool: the walls it cuts are its colour.
    let part = one_part(
        "difference() { color(\"red\") cube(10); color(\"blue\") translate([5, 5, 5]) cube(10); }",
    );
    // Three 5 x 5 walls of the notch.
    assert!((painted_area(&part, BLUE) - 75.0).abs() < 1e-6);
    for triangle in painted(&part, BLUE) {
        assert!(inside(centroid(triangle), [5.0; 3], [10.0; 3]));
    }
    assert_closed(&part.mesh);

    // An outer colour paints everything, cut walls included.
    let part = one_part("color(\"blue\") difference() { cube(10); translate([5, 5, 5]) cube(10); }");
    assert!(close(part.color.expect("blue"), BLUE));
    assert!(part.face_colors.is_none());
}

#[test]
fn a_profile_color_paints_the_extruded_solid() {
    let part = one_part(
        "module m() { linear_extrude(2) color(\"red\") square(5); translate([10, 0, 0]) color(\"blue\") cube(5); }
         m();",
    );
    assert!((painted_area(&part, RED) - (2.0 * 25.0 + 4.0 * 10.0)).abs() < 1e-6);
}

#[test]
fn separate_top_level_objects_keep_one_color_each() {
    let parts = parts("color(\"red\") cube(5); translate([10, 0, 0]) color(\"blue\") cube(5);");
    assert_eq!(parts.len(), 2);
    assert!(close(parts[0].color.unwrap(), RED));
    assert!(close(parts[1].color.unwrap(), BLUE));
    assert!(parts.iter().all(|part| part.face_colors.is_none()));
}

#[test]
fn a_one_color_object_meshes_exactly_as_an_uncolored_one() {
    let plain = one_part("module m() { cube(10); translate([5, 5, 5]) sphere(4); } m();");
    let colored = one_part(
        "module m() { color(\"red\") cube(10); color(\"red\") translate([5, 5, 5]) sphere(4); } m();",
    );
    assert!(colored.face_colors.is_none());
    assert!(close(colored.color.unwrap(), RED));
    assert_eq!(plain.mesh.binary_stl(), colored.mesh.binary_stl());
}

#[test]
fn exports_carry_per_face_colors() {
    let output = compile_parts(
        "module m() { color(\"red\") cube(10); translate([20, 0, 0]) color(\"blue\") cube(5); } m();",
        Quality::Render,
        None,
    )
    .expect("compile");
    let mut bodies: Vec<crate::export::ExportBody> = output
        .parts
        .iter()
        .map(|part| crate::export::ExportBody {
            id: part.id.clone(),
            name: part.name.clone(),
            color: part.color,
            face_colors: part.face_colors.clone(),
            mesh: part.mesh.clone(),
        })
        .collect();
    crate::export::clean_bodies(&mut bodies);
    let colors = bodies[0].face_colors.as_ref().expect("kept through cleanup");
    assert_eq!(colors.faces.len(), bodies[0].mesh.triangles.len());

    // 3MF: two base materials, and per-triangle material indices.
    let package = crate::export::encode(&bodies, crate::export::Format::ThreeMf, "t", usize::MAX)
        .expect("3mf");
    let text = String::from_utf8_lossy(&package);
    assert!(text.contains("displaycolor=\"#FF0000\""));
    assert!(text.contains("displaycolor=\"#0000FF\""));
    assert!(text.contains(" pid=\"2\" p1=\""));

    // GLB: one mesh, a primitive and a material per colour.
    let glb = crate::export::encode(&bodies, crate::export::Format::Glb, "t", usize::MAX).expect("glb");
    let json = String::from_utf8_lossy(&glb);
    assert_eq!(json.matches("\"baseColorFactor\"").count(), 2);
    assert_eq!(json.matches("\"primitives\"").count(), 1);

    // OFF: per-face colours, both of them.
    let off = crate::export::encode(&bodies, crate::export::Format::Off, "t", usize::MAX).expect("off");
    let off = String::from_utf8_lossy(&off);
    assert!(off.contains(" 255 0 0 255"));
    assert!(off.contains(" 0 0 255 255"));
}

#[test]
fn a_uniform_color_the_tree_cannot_name_is_still_reported() {
    // `stated_color` cannot read through an intersection; the faces can.
    let part = one_part(
        "intersection() { color(\"red\") cube(10); color(\"red\") translate([5, 0, 0]) cube(10); }",
    );
    assert!(part.face_colors.is_none());
    assert!(close(part.color.expect("red"), RED));
}
