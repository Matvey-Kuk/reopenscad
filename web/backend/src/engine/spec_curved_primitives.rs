use super::*;

fn evaluate_source(source: &str) -> Shape {
    let tokens = Lexer::new(source).tokenize().expect("lex curved primitive");
    let statements = Parser::new(tokens)
        .parse_program()
        .expect("parse curved primitive");
    Evaluator::default()
        .evaluate(&statements)
        .expect("evaluate curved primitive")
}

fn fragments(shape: &Shape) -> usize {
    match shape {
        Shape::Circle2d { fragments, .. }
        | Shape::Sphere { fragments, .. }
        | Shape::Cylinder { fragments, .. } => *fragments,
        other => panic!("expected curved primitive, found {other:?}"),
    }
}

#[test]
fn fragment_count_follows_fn_fa_fs_formula() {
    assert_eq!(
        fragments(&evaluate_source("circle(r=0.0000001, $fn=99);")),
        3
    );
    assert_eq!(fragments(&evaluate_source("circle(r=10, $fn=4);")), 4);
    assert_eq!(fragments(&evaluate_source("circle(r=10);")), 30);
    assert_eq!(
        fragments(&evaluate_source("circle(r=10, $fa=60, $fs=100);")),
        5
    );
    assert_eq!(
        fragments(&evaluate_source("circle(r=10, $fa=20, $fs=1);")),
        18
    );
    assert_eq!(fragments(&evaluate_source("circle(r=10, $fn=4.9);")), 4);
}

#[test]
fn diameter_aliases_match_radius_geometry() {
    let circle_r = evaluate_source("circle(r=5, $fn=8);");
    let circle_d = evaluate_source("circle(d=10, $fn=8);");
    assert_eq!(circle_r.bounds().min, circle_d.bounds().min);
    assert_eq!(circle_r.bounds().max, circle_d.bounds().max);
    assert_eq!(
        circle_r.distance(Vec3::new(3.0, 2.0, 0.0)),
        circle_d.distance(Vec3::new(3.0, 2.0, 0.0))
    );

    let sphere_r = evaluate_source("sphere(r=5, $fn=8);");
    let sphere_d = evaluate_source("sphere(d=10, $fn=8);");
    assert_eq!(sphere_r.bounds().min, sphere_d.bounds().min);
    assert_eq!(sphere_r.bounds().max, sphere_d.bounds().max);
    assert_eq!(
        sphere_r.distance(Vec3::new(3.0, 2.0, 1.0)),
        sphere_d.distance(Vec3::new(3.0, 2.0, 1.0))
    );

    let uniform = evaluate_source("cylinder(h=7, d=10, $fn=8);");
    let tapered = evaluate_source("cylinder(h=7, d1=10, d2=4, $fn=8);");
    let Shape::Cylinder {
        radius1, radius2, ..
    } = uniform
    else {
        panic!("expected cylinder")
    };
    assert_eq!((radius1, radius2), (5.0, 5.0));
    let Shape::Cylinder {
        radius1, radius2, ..
    } = tapered
    else {
        panic!("expected tapered cylinder")
    };
    assert_eq!((radius1, radius2), (5.0, 2.0));
}

#[test]
fn fragment_specials_are_dynamically_scoped_through_modules() {
    let shape = evaluate_source(
        r#"
          module inner() { sphere(r=10); }
          module outer() { inner(); }
          outer($fn=4);
        "#,
    );
    assert_eq!(fragments(&shape), 4);

    let locally_overridden = evaluate_source(
        r#"
          $fn = 24;
          module curved() { cylinder(h=5, r=3); }
          curved($fn=7);
        "#,
    );
    assert_eq!(fragments(&locally_overridden), 7);
}

#[test]
fn low_fragment_geometry_is_factually_faceted() {
    let circle_low = evaluate_source("circle(r=10, $fn=4);");
    let circle_high = evaluate_source("circle(r=10, $fn=64);");
    let diagonal = Vec3::new(7.0, 7.0, 0.0);
    assert!(circle_low.distance(diagonal) > 0.0);
    assert!(circle_high.distance(diagonal) < 0.0);

    let sphere_low = evaluate_source("sphere(r=10, $fn=4);");
    let sphere_high = evaluate_source("sphere(r=10, $fn=64);");
    assert!(sphere_low.distance(diagonal) > 0.0);
    assert!(sphere_high.distance(diagonal) < 0.0);

    let cylinder_low = evaluate_source("cylinder(h=5, r=10, $fn=4);");
    let cylinder_high = evaluate_source("cylinder(h=5, r=10, $fn=64);");
    let diagonal_mid = Vec3::new(7.0, 7.0, 2.5);
    assert!(cylinder_low.distance(diagonal_mid) > 0.0);
    assert!(cylinder_high.distance(diagonal_mid) < 0.0);
}

#[test]
fn fragment_counts_change_generated_meshes() {
    let low_sphere = compile("sphere(r=10, $fn=4);", Quality::Preview, None).unwrap();
    let high_sphere = compile("sphere(r=10, $fn=48);", Quality::Preview, None).unwrap();
    assert!(high_sphere.mesh.triangles.len() > low_sphere.mesh.triangles.len());

    let low_cylinder = compile("cylinder(h=10, r=10, $fn=4);", Quality::Preview, None).unwrap();
    let high_cylinder = compile("cylinder(h=10, r=10, $fn=48);", Quality::Preview, None).unwrap();
    assert!(high_cylinder.mesh.triangles.len() > low_cylinder.mesh.triangles.len());

    let low_circle = compile(
        "linear_extrude(height=2) circle(r=10, $fn=4);",
        Quality::Preview,
        None,
    )
    .unwrap();
    let high_circle = compile(
        "linear_extrude(height=2) circle(r=10, $fn=48);",
        Quality::Preview,
        None,
    )
    .unwrap();
    assert!(high_circle.mesh.triangles.len() > low_circle.mesh.triangles.len());
}

// ---------------------------------------------------------------------------
// rotate_extrude()
// ---------------------------------------------------------------------------

fn rotate_extrude_parts(source: &str) -> (f64, usize) {
    match evaluate_source(source) {
        Shape::RotateExtrude {
            angle, fragments, ..
        } => (angle, fragments),
        other => panic!("expected a rotate_extrude, found {other:?}"),
    }
}

/// The bare call sweeps a full turn, which is what nearly every use of it is.
#[test]
fn rotate_extrude_sweeps_the_whole_turn_by_default() {
    assert_eq!(
        rotate_extrude_parts("rotate_extrude() translate([10, 0]) circle(2);").0,
        360.0
    );
    assert_eq!(
        rotate_extrude_parts("rotate_extrude(angle = 90) translate([10, 0]) circle(2);").0,
        90.0
    );
    assert_eq!(
        rotate_extrude_parts("rotate_extrude(270) translate([10, 0]) circle(2);").0,
        270.0
    );
}

/// Facets follow the radius the sweep actually has to approximate — the
/// profile's *outermost* reach — not the profile's own size. A thread or an
/// O-ring groove is a small profile held far out from the axis, and sizing its
/// facets by the profile would visibly polygonise the ring it travels on.
#[test]
fn rotate_extrude_facets_follow_the_outer_radius_not_the_profile() {
    // A 2 mm circle 50 mm out: the sweep is a 52 mm ring and is faceted as one.
    let (_, far) = rotate_extrude_parts("rotate_extrude() translate([50, 0]) circle(2);");
    // The same profile sitting on the axis sweeps a 2 mm ring and needs far less.
    let (_, near) = rotate_extrude_parts("rotate_extrude() translate([2, 0]) circle(2);");
    assert!(
        far > near * 2,
        "a far-flung profile must be faceted for its own radius: {far} vs {near}"
    );
    assert_eq!(
        rotate_extrude_parts("rotate_extrude($fn = 12) translate([50, 0]) circle(2);").1,
        12,
        "$fn still wins outright"
    );
}

/// A profile that reaches across the axis would sweep through itself. OpenSCAD
/// refuses it outright ("may not lie across the Y axis") and so does this,
/// because the alternative is a self-intersecting solid that every later
/// boolean has to cope with.
#[test]
fn rotate_extrude_refuses_a_profile_that_crosses_the_axis() {
    let tokens = Lexer::new("rotate_extrude() translate([-2, 0]) circle(5);")
        .tokenize()
        .expect("lex");
    let statements = Parser::new(tokens).parse_program().expect("parse");
    let mut evaluator = Evaluator::default();
    let shape = evaluator.evaluate(&statements);
    assert!(
        shape.is_err()
            || shape
                .as_ref()
                .is_ok_and(|shape| shape.bounds().max.x <= 0.0),
        "a profile crossing the axis must not produce a solid"
    );
    assert!(
        evaluator
            .diagnostics
            .iter()
            .any(|line| line.contains("rotate_extrude") && line.contains("x < 0")),
        "the refusal has to say why: {:?}",
        evaluator.diagnostics
    );
}

/// A revolved profile is a solid, and the profile itself has to be planar.
#[test]
fn rotate_extrude_is_a_solid_built_from_a_planar_profile() {
    assert_eq!(
        evaluate_source("rotate_extrude() translate([10, 0]) circle(2);").dimension(),
        ShapeDimension::Solid
    );
    let tokens = Lexer::new("rotate_extrude() cube(3);")
        .tokenize()
        .expect("lex");
    let statements = Parser::new(tokens).parse_program().expect("parse");
    assert!(
        Evaluator::default().evaluate(&statements).is_err(),
        "a 3D child is not a profile"
    );
}
