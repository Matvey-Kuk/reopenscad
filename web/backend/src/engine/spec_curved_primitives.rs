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
