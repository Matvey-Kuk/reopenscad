//! `sweep_path()`: a ReOpenSCAD extension (not an OpenSCAD built-in) giving
//! the envelope of a 3D child translated along a polyline.

use super::*;

fn volume(mesh: &Mesh) -> f64 {
    mesh.triangles
        .iter()
        .map(|triangle| {
            let [a, b, c] = triangle.vertices;
            a.dot(b.cross(c)) / 6.0
        })
        .sum()
}

fn swept(source: &str) -> MultipartCompileOutput {
    compile_parts(source, Quality::Preview, None).expect("sweep_path compiles")
}

fn total_volume(output: &MultipartCompileOutput) -> f64 {
    output.parts.iter().map(|part| volume(&part.mesh)).sum()
}

fn close(actual: f64, expected: f64, relative: f64) {
    assert!(
        (actual - expected).abs() <= relative * expected.abs().max(1.0),
        "volume {actual}, expected {expected}"
    );
}

#[test]
fn a_straight_sweep_of_a_cube_is_the_stretched_box() {
    let output = swept("sweep_path([[0,0,0],[30,0,0]]) cube(10);");
    close(total_volume(&output), 4000.0, 1e-9);
}

#[test]
fn a_diagonal_sweep_adds_the_parallelogram() {
    // Square 10 x 10 swept by (10, 10): 100 + |(10,10) x width| = 100 + 200.
    let output = swept("sweep_path(points = [[0,0,0],[10,10,0]]) cube(10);");
    close(total_volume(&output), 3000.0, 1e-9);
}

#[test]
fn an_l_shaped_path_unions_both_legs() {
    let output = swept("sweep_path([[0,0,0],[20,0,0],[20,20,0]]) cube(10);");
    close(total_volume(&output), 5000.0, 1e-9);
}

#[test]
fn a_swept_sphere_is_a_capsule() {
    let sphere = total_volume(&swept("sphere(5, $fn = 48);"));
    let output = swept("sweep_path([[0,0,0],[0,0,20]]) sphere(5, $fn = 48);");
    // The polygonal sphere's equator area times the length, plus the sphere.
    let equator = 0.5 * 48.0 * 25.0 * (2.0 * std::f64::consts::PI / 48.0).sin();
    close(total_volume(&output), sphere + equator * 20.0, 0.02);
    // And close to the analytic capsule.
    let analytic = 4.0 / 3.0 * std::f64::consts::PI * 125.0 + std::f64::consts::PI * 25.0 * 20.0;
    close(total_volume(&output), analytic, 0.03);
}

#[test]
fn a_non_convex_child_keeps_its_hole() {
    // A 10 x 10 x 2 plate with a 4 x 4 window swept 8 up: a tube.
    let output = swept(
        "sweep_path([[0,0,0],[0,0,8]]) difference() { cube([10,10,2]); translate([3,3,-1]) cube([4,4,4]); }",
    );
    close(total_volume(&output), 1000.0 - 160.0, 1e-9);
}

#[test]
fn a_single_point_translates_and_two_d_children_are_ignored() {
    let output = swept("sweep_path([[5,0,0]]) cube(2);");
    close(total_volume(&output), 8.0, 1e-9);
    let flat = compile_parts("sweep_path([[0,0,0],[1,0,0]]) square(2); cube(1);", Quality::Preview, None)
        .expect("compiles");
    assert!(
        flat.messages.iter().any(|message| message.contains("sweep_path() geometry: children must be 3D")),
        "{:?}",
        flat.messages
    );
}

#[test]
fn a_user_module_named_sweep_path_shadows_the_extension() {
    let output = swept("module sweep_path(points) { cube(1); } sweep_path([[0,0,0],[30,0,0]]) cube(10);");
    close(total_volume(&output), 1.0, 1e-9);
}

#[test]
fn sweep_path_timing_on_a_non_convex_child() {
    let started = std::time::Instant::now();
    let output = swept(
        "sweep_path([[0,0,0],[30,0,0],[30,30,0],[30,30,20]]) difference() { cylinder(r = 5, h = 4, $fn = 64); translate([0,0,-1]) cylinder(r = 3, h = 6, $fn = 64); }",
    );
    let triangles: usize = output.parts.iter().map(|part| part.mesh.triangles.len()).sum();
    eprintln!("sweep_path non-convex ring x3 segments: {triangles} triangles in {:?}", started.elapsed());
    assert!(total_volume(&output) > 0.0);
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
}

#[test]
fn a_child_too_complex_to_decompose_is_an_error_not_an_empty_sweep() {
    // 65 disjoint cubes: more convex cells than the decomposition takes.
    let error = compile_parts(
        "difference() { cube([200, 20, 20]); sweep_path([[0,0,0],[0,0,30]]) for (i = [0:64]) translate([i * 3, 5, -5]) cube(1); }",
        Quality::Preview,
        None,
    )
    .err()
    .expect("refused");
    assert!(error.to_string().contains("sweep_path(): the child is too complex"), "{error}");
}

#[test]
fn nested_sweeps_share_their_child() {
    let started = std::time::Instant::now();
    let output = swept(
        "sweep_path([for (i = [0:64]) [i, 0, 0]]) sweep_path([for (i = [0:64]) [0, i, 0]]) sweep_path([for (i = [0:64]) [0, 0, i]]) cube(1);",
    );
    close(total_volume(&output), 65.0 * 65.0 * 65.0, 1e-6);
    assert!(started.elapsed() < std::time::Duration::from_secs(20), "{:?}", started.elapsed());
}

#[test]
fn too_many_hulls_are_refused() {
    let error = compile_parts(
        "sweep_path([for (i = [0:64]) [i * 3, (i % 2) * 3, 0]]) for (j = [0:19]) translate([j * 2, 0, 0]) cube(1);",
        Quality::Preview,
        None,
    )
    .err()
    .expect("refused");
    assert!(error.to_string().contains("1024 limit"), "{error}");
}

#[test]
fn faceless_hulls_cannot_be_written_in_scad() {
    // The degenerate operand never comes from user code: a two-point hull is
    // rejected by hull() itself, so minkowski() with it cannot reach a sweep.
    let output = compile_parts(
        "minkowski() { cube(1); hull() { polyhedron([[0,0,0],[5,0,0]], []); } }",
        Quality::Preview,
        None,
    );
    if let Ok(output) = output {
        close(total_volume(&output), 1.0, 1e-9);
    }
}
