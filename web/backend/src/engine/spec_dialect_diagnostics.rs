//! Dialect differences are never silent.
//!
//! Whenever this engine drops a statement or ignores an argument, it says so in
//! the console. A part that quietly ignores what its author asked for is worse
//! than one that refuses to build, because the defect only shows up after the
//! print.

use super::*;

fn compiled(source: &str) -> MultipartCompileOutput {
    compile_parts(source, Quality::Preview, None).unwrap_or_else(|error| panic!("{error}"))
}

fn messages(source: &str) -> Vec<String> {
    compiled(source).messages
}

fn bounds(source: &str) -> PartBounds {
    compiled(source).parts[0].bounds
}

fn close(actual: Vec3, expected: [f64; 3]) -> bool {
    (actual.x - expected[0]).abs() < 1.0e-6
        && (actual.y - expected[1]).abs() < 1.0e-6
        && (actual.z - expected[2]).abs() < 1.0e-6
}

/// A 2D `scale()` under `linear_extrude` used to produce nothing at all, with
/// no warning. A 2-vector scales x and y and leaves z alone, as in OpenSCAD.
#[test]
fn two_dimensional_transforms_work_under_extrusions() {
    let scaled = bounds("linear_extrude(5) scale([2, 1]) square(3);");
    assert!(close(scaled.max, [6.0, 3.0, 5.0]), "{scaled:?}");
    let on_a_solid = bounds("scale([1, 2]) cube(1);");
    assert!(close(on_a_solid.max, [1.0, 2.0, 1.0]), "{on_a_solid:?}");

    for (source, max) in [
        (
            "linear_extrude(5) translate([2, 1]) square(3);",
            [5.0, 4.0, 5.0],
        ),
        (
            "linear_extrude(5) mirror([0, 1]) translate([0, -3]) square(3);",
            [3.0, 3.0, 5.0],
        ),
        (
            "linear_extrude(5) resize([6, 6]) square(3);",
            [6.0, 6.0, 5.0],
        ),
        (
            "linear_extrude(5) multmatrix([[2, 0, 0, 0], [0, 1, 0, 0]]) square(3);",
            [6.0, 3.0, 5.0],
        ),
        (
            "linear_extrude(5) offset(delta = 1) scale([2, 1]) square(3);",
            [7.0, 4.0, 5.0],
        ),
        (
            "linear_extrude(2) scale([2, 1]) color(\"red\") square(3);",
            [6.0, 3.0, 2.0],
        ),
    ] {
        let result = bounds(source);
        assert!(close(result.max, max), "{source}: {result:?}");
    }
    let revolved = bounds("rotate_extrude($fn = 4) translate([5, 0]) scale([1, 2]) square(1);");
    assert!((revolved.max.z - 2.0).abs() < 1.0e-6, "{revolved:?}");
}

/// When the model ends up empty, the error carries the warnings that explain
/// why rather than a bare "no geometry".
#[test]
fn an_empty_model_explains_itself() {
    let error = compile_parts("cube(size = [1, 1]);", Quality::Preview, None)
        .expect_err("a 2-vector cube is invalid");
    let text = error.to_string();
    assert!(text.contains("no 3D geometry"), "{text}");
    assert!(text.contains("cube()"), "{text}");

    let planar = compile_parts("square(5);", Quality::Preview, None).expect_err("2D only");
    assert!(planar.to_string().contains("2D"), "{planar}");
}

/// A 2D object beside 3D ones is left out of the exported model, and says so.
#[test]
fn a_top_level_2d_object_beside_solids_is_reported() {
    let output = compiled("cube(2);\nsquare(5);");
    assert_eq!(output.parts.len(), 1);
    assert!(
        output
            .messages
            .iter()
            .any(|line| line.contains("2D object on line 2")),
        "{:?}",
        output.messages
    );
}

/// A misspelt or unknown argument to a built-in is reported by name, and the
/// message lists what the module does take. `$`-variables are always fine.
#[test]
fn unknown_named_arguments_to_builtins_are_reported() {
    let lines = messages("cube(5, centre = true);");
    assert!(
        lines.iter().any(
            |line| line.contains("cube() has no parameter named 'centre'")
                && line.contains("center")
        ),
        "{lines:?}"
    );
    let lines = messages("cylinder(h = 5, r = 2, $fn = 12, $name = \"Pin\", $custom = 3);");
    assert!(
        lines.iter().all(|line| !line.contains("no parameter")),
        "{lines:?}"
    );
    let lines = messages("translate([1, 0, 0], [5, 5, 5]) cube(1);");
    assert!(
        lines
            .iter()
            .any(|line| line.contains("translate() takes 1 positional argument")),
        "{lines:?}"
    );
    // Every documented parameter of the common modules is accepted quietly.
    let lines = messages(
        "linear_extrude(height = 2, center = true, convexity = 4, twist = 10, slices = 3, \
         scale = 1) square(2);
         translate([5, 0, 0]) rotate_extrude(angle = 90, convexity = 2) translate([1, 0]) square(1);
         translate([10, 0, 0]) cylinder(h = 1, r1 = 1, r2 = 2, center = false);
         translate([15, 0, 0]) sphere(d = 2);
         translate([20, 0, 0]) resize(newsize = [1, 1, 1], auto = true) cube(3);
         translate([25, 0, 0]) color(c = \"red\", alpha = 0.5) cube(1);
         translate([30, 0, 0]) linear_extrude(1) offset(delta = 1, chamfer = false) square(1);
         translate([35, 0, 0]) linear_extrude(1) text(text = \"A\", size = 3, font = \"Liberation Sans\", \
         halign = \"center\", valign = \"center\", spacing = 1, direction = \"ltr\", \
         language = \"en\", script = \"latin\");",
    );
    assert!(
        lines.iter().all(|line| !line.contains("WARNING")),
        "{lines:?}"
    );
}

/// Children handed to a primitive are dropped, and the drop is said.
#[test]
fn children_of_a_primitive_are_reported() {
    let lines = messages("cube(2) translate([5, 0, 0]) sphere(1);");
    assert!(
        lines
            .iter()
            .any(|line| line.contains("cube() does not take children")),
        "{lines:?}"
    );
}

/// A user module with a built-in's name replaces the built-in, as in
/// OpenSCAD, instead of the definition being silently ignored.
#[test]
fn a_user_module_shadows_a_builtin() {
    let result = bounds("module cube(s) { sphere(r = s, $fn = 8); }\ncube(3);");
    assert!(
        result.min.x < -2.0,
        "the user's cube() is a sphere: {result:?}"
    );
}

/// Development-snapshot `linear_extrude` parameters this engine does not
/// model are named rather than dropped.
#[test]
fn unsupported_extrusion_parameters_are_reported() {
    let lines = messages("linear_extrude(height = 2, v = [1, 0, 1]) square(1);");
    assert!(
        lines
            .iter()
            .any(|line| line.contains("v (extrusion direction)")),
        "{lines:?}"
    );
    let lines = messages("linear_extrude(height = 2, v = [0, 0, 3]) square(1);");
    assert!(
        lines.iter().all(|line| !line.contains("WARNING")),
        "{lines:?}"
    );
    let lines = messages("linear_extrude(height = 2, segments = 4) square(1);");
    assert!(
        lines.iter().any(|line| line.contains("segments")),
        "{lines:?}"
    );
}

/// `rotate_extrude(start = …)` turns where the sweep begins.
#[test]
fn rotate_extrude_honours_start() {
    // Measured on the mesh: a sweep's shape bounds are the full turn's.
    let mesh_bounds = |source: &str| {
        let output = compiled(source);
        let mut min = Vec3::new(f64::INFINITY, f64::INFINITY, f64::INFINITY);
        let mut max = Vec3::new(f64::NEG_INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for triangle in &output.parts[0].mesh.triangles {
            for vertex in triangle.vertices {
                min = Vec3::new(
                    min.x.min(vertex.x),
                    min.y.min(vertex.y),
                    min.z.min(vertex.z),
                );
                max = Vec3::new(
                    max.x.max(vertex.x),
                    max.y.max(vertex.y),
                    max.z.max(vertex.z),
                );
            }
        }
        PartBounds { min, max }
    };
    let plain = mesh_bounds("rotate_extrude(angle = 90, $fn = 32) translate([2, 0]) square(1);");
    assert!(plain.min.x > -1.0e-9 && plain.min.y > -1.0e-9, "{plain:?}");
    let turned = mesh_bounds(
        "rotate_extrude(angle = 90, start = 90, $fn = 32) translate([2, 0]) square(1);",
    );
    assert!(turned.max.x < 1.0e-9 && turned.min.y > 0.0, "{turned:?}");
}

/// `minkowski()` is exact on the polyhedral kernel, so it carries no
/// approximation warning there.
#[test]
fn exact_minkowski_is_not_flagged_as_approximate() {
    let output = compiled("minkowski() { cube(2); sphere(1, $fn = 8); }");
    assert!(
        output
            .messages
            .iter()
            .all(|line| !line.contains("approximat")),
        "{:?}",
        output.messages
    );
    assert!((output.parts[0].bounds.max.x - 3.0).abs() < 1.0e-6);
}
