use super::*;

fn evaluate_source(source: &str) -> Result<Shape, EngineError> {
    let tokens = Lexer::new(source).tokenize()?;
    let statements = Parser::new(tokens).parse_program()?;
    Evaluator::default().evaluate(&statements)
}

fn evaluate_source_with_diagnostics(source: &str) -> Result<(Shape, Vec<String>), EngineError> {
    let tokens = Lexer::new(source).tokenize()?;
    let statements = Parser::new(tokens).parse_program()?;
    let mut evaluator = Evaluator::default();
    let shape = evaluator.evaluate(&statements)?;
    Ok((shape, evaluator.diagnostics))
}

fn assert_inside(shape: &Shape, point: Vec3) {
    assert!(
        shape.distance(point) < 0.0,
        "expected {point:?} to be inside the evaluated shape"
    );
}

fn assert_outside(shape: &Shape, point: Vec3) {
    assert!(
        shape.distance(point) > 0.0,
        "expected {point:?} to be outside the evaluated shape"
    );
}

#[test]
fn for_with_multiple_assignments_forms_nested_loops() {
    let shape = evaluate_source(
        r#"
            for (x = [-10, 10], y = [-6, 6])
                translate([x, y, 0]) cube(2, center=true);
        "#,
    )
    .expect("multiple for assignments should parse and evaluate as nested loops");

    for x in [-10.0, 10.0] {
        for y in [-6.0, 6.0] {
            assert_inside(&shape, Vec3::new(x, y, 0.0));
        }
    }
    assert_outside(&shape, Vec3::new(0.0, 0.0, 0.0));
}

#[test]
fn for_iterates_heterogeneous_vector_values_without_numeric_coercion() {
    let shape = evaluate_source(
        r#"
            for (item = [2, "label", true]) {
                if (item == 2)
                    translate([-10, 0, 0]) cube(2, center=true);
                else if (item == "label")
                    cube(2, center=true);
                else if (item == true)
                    translate([10, 0, 0]) cube(2, center=true);
            }
        "#,
    )
    .expect("for should preserve each heterogeneous vector element's value and type");

    assert_inside(&shape, Vec3::new(-10.0, 0.0, 0.0));
    assert_inside(&shape, Vec3::new(0.0, 0.0, 0.0));
    assert_inside(&shape, Vec3::new(10.0, 0.0, 0.0));
}

#[test]
fn intersection_for_intersects_each_iteration_instead_of_its_union() {
    let shape = evaluate_source(
        "intersection_for (x = [-2, 2]) translate([x, 0, 0]) cube(6, center=true);",
    )
    .expect("intersection_for should be accepted in geometry context");

    assert_inside(&shape, Vec3::new(0.0, 0.0, 0.0));
    assert_inside(&shape, Vec3::new(0.9, 2.9, 0.0));
    assert_outside(&shape, Vec3::new(1.1, 0.0, 0.0));
    assert_outside(&shape, Vec3::new(0.0, 3.1, 0.0));
}

#[test]
fn let_statement_binds_left_to_right_and_does_not_leak() {
    let shape = evaluate_source(
        r#"
            a = 30;
            let (a = 3, b = a + 4)
                translate([b, 0, 0]) cube(a * 2, center=true);

            if (b)
                translate([-30, 0, 0]) sphere(2);
            translate([a, 0, 0]) cube(2, center=true);
        "#,
    )
    .expect("let bindings should be ordered and scoped to their child");

    assert_inside(&shape, Vec3::new(7.0, 0.0, 0.0));
    assert_inside(&shape, Vec3::new(30.0, 0.0, 0.0));
    assert_outside(&shape, Vec3::new(-30.0, 0.0, 0.0));
}

#[test]
fn module_defaults_use_the_caller_environment_and_named_args_can_be_reordered() {
    let shape = evaluate_source(
        r#"
            fallback = 40;
            module marker(x = fallback, size = 2) {
                translate([x, 0, 0]) cube(size, center=true);
            }
            module caller(fallback) {
                marker(size=4);
            }

            caller(9);
            marker(size=2, x=-9);
        "#,
    )
    .expect("module defaults and reordered named arguments should evaluate");

    assert_inside(&shape, Vec3::new(9.0, 1.5, 0.0));
    assert_inside(&shape, Vec3::new(-9.0, 0.0, 0.0));
    assert_outside(&shape, Vec3::new(-9.0, 1.5, 0.0));
    assert_outside(&shape, Vec3::new(40.0, 0.0, 0.0));
}

#[test]
fn last_assignment_wins_with_a_warning_and_inner_scope_shadows_outer() {
    let (shape, diagnostics) = evaluate_source_with_diagnostics(
        r#"
            position = 2;
            translate([position, 0, 0]) cube(2, center=true);
            position = 12;

            if (true) {
                position = -12;
                translate([position, 0, 0]) cube(2, center=true);
            }
        "#,
    )
    .expect("assignments should be resolved lexically before geometry evaluation");

    assert_inside(&shape, Vec3::new(12.0, 0.0, 0.0));
    assert_inside(&shape, Vec3::new(-12.0, 0.0, 0.0));
    assert_outside(&shape, Vec3::new(2.0, 0.0, 0.0));
    assert!(
        diagnostics.iter().any(|message| {
            let message = message.to_lowercase();
            message.contains("warning")
                && (message.contains("reassign") || message.contains("assigned more than once"))
        }),
        "reassignment should emit a warning, got {diagnostics:?}"
    );
}

#[test]
fn forward_and_nested_module_definitions_are_lexically_scoped() {
    let shape = evaluate_source(
        r#"
            outer();

            module marker() {
                translate([-20, 0, 0]) cube(2, center=true);
            }

            module outer() {
                marker();
                inner();

                module marker() {
                    translate([12, 0, 0]) sphere(3);
                }
                module inner() {
                    translate([20, 0, 0]) cube(2, center=true);
                }
            }

            marker();
        "#,
    )
    .expect("forward and nested module definitions should resolve in lexical scope");

    assert_inside(&shape, Vec3::new(-20.0, 0.0, 0.0));
    assert_inside(&shape, Vec3::new(12.0, 2.5, 0.0));
    assert_inside(&shape, Vec3::new(20.0, 0.0, 0.0));
    assert_outside(&shape, Vec3::new(-20.0, 2.0, 0.0));
}

#[test]
fn missing_module_parameter_is_undef_instead_of_a_hard_error() {
    let shape = evaluate_source(
        r#"
            module optional(value) {
                if (value)
                    sphere(8);
                else
                    cube(2, center=true);
            }
            optional();
        "#,
    )
    .expect("a missing parameter without a default should bind to undef");

    assert_inside(&shape, Vec3::new(0.0, 0.0, 0.0));
    assert_outside(&shape, Vec3::new(2.0, 0.0, 0.0));
}

#[test]
fn guarded_module_recursion_succeeds() {
    let shape = evaluate_source(
        r#"
            module countdown(n) {
                translate([n * 5, 0, 0]) cube(2, center=true);
                if (n > 1) countdown(n - 1);
            }
            countdown(3);
        "#,
    )
    .expect("guarded recursive modules should evaluate");

    assert_inside(&shape, Vec3::new(5.0, 0.0, 0.0));
    assert_inside(&shape, Vec3::new(10.0, 0.0, 0.0));
    assert_inside(&shape, Vec3::new(15.0, 0.0, 0.0));
    assert_outside(&shape, Vec3::new(20.0, 0.0, 0.0));
}

#[test]
fn recursion_depth_exceeded_is_a_clear_error() {
    const CHILD_PROCESS: &str = "REOPENSCAD_RECURSION_LIMIT_CHILD";
    const CHILD_VALUE: &str = "run-isolated-recursion-depth-check";
    const EXACT_TEST: &str =
        "engine::spec_control_modules::recursion_depth_exceeded_is_a_clear_error";
    let selected_exactly = std::env::args().any(|argument| argument == EXACT_TEST);
    if std::env::var(CHILD_PROCESS).as_deref() == Ok(CHILD_VALUE) && selected_exactly {
        let error = evaluate_source(
            r#"
                module descend(n) {
                    if (n > 0) descend(n - 1);
                    else cube(1, center=true);
                }
                descend(200);
            "#,
        )
        .expect_err("module recursion must stop at an implementation depth limit");
        let message = error.to_string().to_lowercase();
        assert!(
            message.contains("recursion") && message.contains("depth"),
            "recursion-limit error should name recursion depth, got {message:?}"
        );
        return;
    }

    let output = std::process::Command::new(std::env::current_exe().expect("current test binary"))
        .args(["--exact", EXACT_TEST, "--nocapture"])
        .env(CHILD_PROCESS, CHILD_VALUE)
        .output()
        .expect("spawn isolated recursion-limit test");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "recursion-limit check failed in its isolated process\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn special_variables_propagate_dynamically_and_can_be_overridden_per_call() {
    let shape = evaluate_source(
        r#"
            $shift = 11;
            module leaf() {
                translate([$shift, 0, 0]) cube(2, center=true);
            }
            module wrapper() {
                leaf();
            }

            wrapper($shift=-7);
            leaf();
        "#,
    )
    .expect("special variables should follow the instantiation chain");

    assert_inside(&shape, Vec3::new(-7.0, 0.0, 0.0));
    assert_inside(&shape, Vec3::new(11.0, 0.0, 0.0));
    assert_outside(&shape, Vec3::new(0.0, 0.0, 0.0));
}

#[test]
fn children_all_single_vector_and_range_forms_select_call_children() {
    let shape = evaluate_source(
        r#"
            module all() { children(); }
            module single() { children(1); }
            module vector() { children([0, 2]); }
            module range() { children([1:2]); }

            all() {
                translate([-40, 0, 0]) sphere(1);
                translate([-36, 0, 0]) sphere(1);
            }
            single() {
                translate([-20, 0, 0]) sphere(1);
                translate([-16, 0, 0]) sphere(1);
            }
            vector() {
                translate([0, 0, 0]) sphere(1);
                translate([4, 0, 0]) sphere(1);
                translate([8, 0, 0]) sphere(1);
            }
            range() {
                translate([20, 0, 0]) sphere(1);
                translate([24, 0, 0]) sphere(1);
                translate([28, 0, 0]) sphere(1);
            }
        "#,
    )
    .expect("children() should support all documented selector forms");

    assert_inside(&shape, Vec3::new(-40.0, 0.0, 0.0));
    assert_inside(&shape, Vec3::new(-36.0, 0.0, 0.0));

    assert_outside(&shape, Vec3::new(-20.0, 0.0, 0.0));
    assert_inside(&shape, Vec3::new(-16.0, 0.0, 0.0));

    assert_inside(&shape, Vec3::new(0.0, 0.0, 0.0));
    assert_outside(&shape, Vec3::new(4.0, 0.0, 0.0));
    assert_inside(&shape, Vec3::new(8.0, 0.0, 0.0));

    assert_outside(&shape, Vec3::new(20.0, 0.0, 0.0));
    assert_inside(&shape, Vec3::new(24.0, 0.0, 0.0));
    assert_inside(&shape, Vec3::new(28.0, 0.0, 0.0));
}

#[test]
fn dollar_children_reports_the_number_of_call_children() {
    let shape = evaluate_source(
        r#"
            module gated() {
                if ($children == 2)
                    cube(6, center=true);
                else
                    sphere(1);
            }
            gated() {
                cube(1);
                sphere(1);
            }
        "#,
    )
    .expect("$children should be set for each user-module instantiation");

    assert_inside(&shape, Vec3::new(2.5, 0.0, 0.0));
    assert_outside(&shape, Vec3::new(3.5, 0.0, 0.0));
}

/// A module this engine does not implement costs that call and nothing else.
///
/// OpenSCAD warns `Ignoring unknown module 'x'` and carries on; probing the
/// binary with `notamodule(1, 2); cube([12, 8, 4]);` gives six facets, one
/// warning and exit 0. It does not evaluate the call's arguments or children
/// either — `notamodule() { echo("x"); }` prints nothing — so neither do we.
#[test]
fn an_unknown_module_warns_and_leaves_the_rest_of_the_model_standing() {
    let (shape, diagnostics) = evaluate_source_with_diagnostics(
        r#"
            notamodule(1, 2);
            import("nowhere.stl");
            surface(file = "nowhere.dat") { echo("never evaluated"); }
            cube([12, 8, 4]);
        "#,
    )
    .expect("an unknown module must not abort the compile");

    assert_inside(&shape, Vec3::new(6.0, 4.0, 2.0));
    for expected in [
        "WARNING: Ignoring unknown module 'notamodule'.",
        "WARNING: Ignoring unknown module 'import'.",
        "WARNING: Ignoring unknown module 'surface'.",
    ] {
        assert!(
            diagnostics.iter().any(|message| message == expected),
            "expected {expected:?} among {diagnostics:?}"
        );
    }
    assert!(
        !diagnostics.iter().any(|message| message.contains("never evaluated")),
        "an ignored module's children must stay unevaluated, got {diagnostics:?}"
    );
}

/// A 2D child under a 3D operation is dropped, not fatal.
///
/// The dimension comes from the *first* child, which is how OpenSCAD decides
/// it: `union(){cube(5); square(3);}` warns twice and yields the cube, while
/// `union(){square(3); cube(5);}` yields the square. An extrude is the same
/// story in reverse — a solid handed to `linear_extrude` is ignored.
#[test]
fn mixing_dimensions_drops_the_odd_child_out_with_a_warning() {
    let (shape, diagnostics) =
        evaluate_source_with_diagnostics("union() { cube([10, 10, 5]); square(4); }")
            .expect("a 2D child must not abort a 3D union");
    assert_eq!(shape.dimension(), ShapeDimension::Solid);
    assert_inside(&shape, Vec3::new(5.0, 5.0, 2.5));
    for expected in [
        "WARNING: Mixing 2D and 3D objects is not supported.",
        "WARNING: Ignoring 2D child object for 3D operation.",
    ] {
        assert!(
            diagnostics.iter().any(|message| message == expected),
            "expected {expected:?} among {diagnostics:?}"
        );
    }

    let (planar, _) =
        evaluate_source_with_diagnostics("union() { square(4); cube([10, 10, 5]); }")
            .expect("a 3D child must not abort a 2D union");
    assert_eq!(planar.dimension(), ShapeDimension::Planar);

    let (extruded, diagnostics) = evaluate_source_with_diagnostics(
        "linear_extrude(height = 2) { square(4); cube([10, 10, 5]); }",
    )
    .expect("a solid under an extrude must not abort the compile");
    assert_inside(&extruded, Vec3::new(2.0, 2.0, 1.0));
    assert!(
        diagnostics
            .iter()
            .any(|message| message == "WARNING: Ignoring 3D child object for 2D operation."),
        "expected the ignored solid to be named, got {diagnostics:?}"
    );
}

/// A range this engine will not walk is empty, never an error.
///
/// A step that computes to zero for one iteration used to take the whole model
/// with it. OpenSCAD calls such a range 4294967295 elements long, warns `Bad
/// range parameter in for statement`, produces nothing and renders the rest —
/// probed with exactly the source below, which still gives the 9 mm cube.
#[test]
fn a_zero_or_unwalkable_range_step_is_an_empty_loop_not_an_error() {
    let (shape, diagnostics) = evaluate_source_with_diagnostics(
        r#"
            step = 0;
            for (i = [0 : step : 3]) translate([i * 5, 0, 0]) cube(1);
            for (j = [0 : 0 / 0 : 3]) translate([0, 50, 0]) cube(1);
            for (k = [0 : 1e-300 : 1]) translate([0, 0, 50]) cube(1);
            cube([9, 3, 3]);
        "#,
    )
    .expect("a bad range step must not abort the compile");

    assert_inside(&shape, Vec3::new(4.5, 1.5, 1.5));
    assert_outside(&shape, Vec3::new(0.5, 50.5, 0.5));
    assert!(
        diagnostics
            .iter()
            .any(|message| message.contains("Bad range parameter in for statement")),
        "expected the bad range to be named, got {diagnostics:?}"
    );
}

/// Recursion deep enough to be ordinary SCAD, and a ceiling that still holds.
///
/// 120 levels is what `tests/fixtures/language/deep-recursion.scad` asks for
/// and what OpenSCAD renders without complaint; the old limit of 16 rejected
/// it. The ceiling is checked by `recursion_depth_exceeded_is_a_clear_error`
/// above, in its own process, because the point of the limit is that the
/// alternative is a stack overflow that aborts this one.
#[test]
fn a_hundred_and_twenty_levels_of_module_recursion_still_render() {
    let shape = evaluate_source(
        r#"
            module tower(n) {
                if (n > 0) {
                    translate([0, 0, n]) cube(1);
                    tower(n - 1);
                }
            }
            tower(120);
        "#,
    )
    .expect("120 levels is ordinary recursive SCAD, not an abuse of the evaluator");

    assert_inside(&shape, Vec3::new(0.5, 0.5, 120.5));
    assert_inside(&shape, Vec3::new(0.5, 0.5, 1.5));
    assert_outside(&shape, Vec3::new(0.5, 0.5, 121.5));
}
