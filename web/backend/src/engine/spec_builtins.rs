use super::*;

fn eval(expression: &str) -> Result<Value, EngineError> {
    let source = format!("__spec_value = {expression};");
    let tokens = Lexer::new(&source).tokenize()?;
    let statements = Parser::new(tokens).parse_program()?;
    let expression = match statements.as_slice() {
        [Stmt::Assign {
            name, expression, ..
        }] if name == "__spec_value" => expression,
        _ => return Err(EngineError::new("Expected one test expression assignment.")),
    };
    evaluate_expression(expression, &Environment::new())
}

fn assert_number(expression: &str, expected: f64, tolerance: f64) {
    let actual = eval(expression)
        .unwrap_or_else(|error| panic!("{expression} should evaluate to a number: {error}"));
    let Value::Number(actual) = actual else {
        panic!("{expression} should produce a number, got {actual:?}");
    };
    assert!(
        (actual - expected).abs() <= tolerance,
        "{expression}: expected {expected}, got {actual}"
    );
}

fn assert_true(expression: &str) {
    let actual = eval(expression)
        .unwrap_or_else(|error| panic!("{expression} should evaluate successfully: {error}"));
    assert_eq!(
        actual,
        Value::Bool(true),
        "{expression} should evaluate to true"
    );
}

fn assert_undefined(expression: &str) {
    let actual = eval(expression)
        .unwrap_or_else(|error| panic!("{expression} should yield undef, not an error: {error}"));
    assert_eq!(
        actual,
        Value::Undefined,
        "{expression} should evaluate to undef"
    );
}

fn assert_program_produces_geometry(source: &str) -> CompileOutput {
    let output = compile(source, Quality::Preview, None)
        .unwrap_or_else(|error| panic!("program should compile and produce geometry: {error}"));
    assert!(
        !output.mesh.triangles.is_empty(),
        "program should produce a non-empty mesh"
    );
    output
}

#[test]
fn math_builtins_match_openscad_2021_01() {
    let cases = [
        ("abs(-3.5)", 3.5, 0.0),
        ("sign(-9)", -1.0, 0.0),
        ("sign(0)", 0.0, 0.0),
        ("sign(9)", 1.0, 0.0),
        ("floor(2.9)", 2.0, 0.0),
        ("floor(-1.2)", -2.0, 0.0),
        ("ceil(2.1)", 3.0, 0.0),
        ("ceil(-1.2)", -1.0, 0.0),
        ("round(1.49)", 1.0, 0.0),
        ("round(1.5)", 2.0, 0.0),
        ("round(-1.5)", -2.0, 0.0),
        ("sqrt(81)", 9.0, 0.0),
        ("pow(2, 5)", 32.0, 0.0),
        ("exp(1)", std::f64::consts::E, 1e-12),
        ("ln(exp(1))", 1.0, 1e-12),
        ("log(1000)", 3.0, 1e-12),
    ];

    for (expression, expected, tolerance) in cases {
        assert_number(expression, expected, tolerance);
    }

    let Value::Number(value) = eval("sqrt(-1)").expect("negative sqrt should produce nan") else {
        panic!("sqrt(-1) should produce a number");
    };
    assert!(value.is_nan(), "sqrt(-1) should produce nan, got {value}");
}

#[test]
fn trigonometry_uses_degrees() {
    let cases = [
        ("sin(30)", 0.5),
        ("cos(60)", 0.5),
        ("tan(45)", 1.0),
        ("asin(0.5)", 30.0),
        ("acos(0.5)", 60.0),
        ("atan(1)", 45.0),
        ("atan2(1, -1)", 135.0),
    ];

    for (expression, expected) in cases {
        assert_number(expression, expected, 1e-10);
    }
}

#[test]
fn min_max_accept_vectors_and_variadic_arguments() {
    for expression in [
        "min(8, -2, 5, 3) == -2",
        "max(8, -2, 5, 3) == 8",
        "min([8, -2, 5, 3]) == -2",
        "max([8, -2, 5, 3]) == 8",
        "min([4.5]) == 4.5",
        "max([4.5]) == 4.5",
    ] {
        assert_true(expression);
    }
}

#[test]
fn norm_and_cross_cover_two_and_three_dimensions() {
    for expression in [
        "norm([3, 4]) == 5",
        "norm([1, 2, 2]) == 3",
        "norm([]) == 0",
        "cross([1, 2, 3], [4, 5, 6]) == [-3, 6, -3]",
        "cross([2, 3], [4, 5]) == -2",
    ] {
        assert_true(expression);
    }
}

#[test]
fn rands_has_the_requested_length_bounds_and_seed_stability() {
    for expression in [
        "len(rands(-2, 2, 5, 12345)) == 5",
        "rands(-2, 2, 5, 12345) == rands(-2, 2, 5, 12345)",
        "len(rands(0, 1, 0, 7)) == 0",
        "len(rands(0, 1, 3)) == 3",
    ] {
        assert_true(expression);
    }
}

#[test]
fn len_and_concat_handle_strings_lists_and_scalar_arguments() {
    for expression in [
        "len(\"hello\") == 5",
        "len(\"\u{2603}\") == 1",
        "len([10, 20, 30]) == 3",
        "len([]) == 0",
        "concat([1, 2], [3, 4]) == [1, 2, 3, 4]",
        "concat([1, 2], 3, [4, 5]) == [1, 2, 3, 4, 5]",
        "concat([], [1], []) == [1]",
    ] {
        assert_true(expression);
    }
}

#[test]
fn string_conversion_and_code_point_builtins_match_the_spec() {
    for expression in [
        "str(\"part-\", 12, \"-\", true) == \"part-12-true\"",
        "str() == \"\"",
        "chr(65) == \"A\"",
        "chr([65, 66, 67]) == \"ABC\"",
        "chr([65:67]) == \"ABC\"",
        "ord(\"Az\") == 65",
        "ord(chr(9731)) == 9731",
    ] {
        assert_true(expression);
    }
}

#[test]
fn type_tests_recognize_each_2021_01_value_kind() {
    for expression in [
        "is_undef(undef)",
        "is_undef(missing_name)",
        "is_bool(false)",
        "is_num(42.5)",
        "!is_num(0 / 0)",
        "is_string(\"value\")",
        "is_list([1, 2, 3])",
        "is_function(function (x) x + 1)",
        "!is_num(true)",
        "!is_bool(0)",
        "!is_string([])",
        "!is_list(\"value\")",
        "!is_function(42)",
    ] {
        assert_true(expression);
    }
}

#[test]
fn lookup_interpolates_and_clamps_at_table_edges() {
    let cases = [
        ("lookup(5, [[0, 0], [10, 100]])", 50.0),
        ("lookup(2.5, [[0, 10], [5, 20], [10, 40]])", 15.0),
        ("lookup(-5, [[0, 10], [10, 20]])", 10.0),
        ("lookup(50, [[0, 10], [10, 20]])", 20.0),
        ("lookup(5, [[0, -2], [10, 2]])", 0.0),
    ];

    for (expression, expected) in cases {
        assert_number(expression, expected, 1e-12);
    }
}

#[test]
fn search_handles_string_characters_all_matches_and_table_columns() {
    for expression in [
        "search(\"ab\", \"abracadabra\") == [0, 1]",
        "search(\"ab\", \"abracadabra\", 0) == [[0, 3, 5, 7, 10], [1, 8]]",
        "search([20], [[1, 10], [2, 20], [3, 20]], 0, 1) == [[1, 2]]",
    ] {
        assert_true(expression);
    }
}

#[test]
fn version_builtins_report_the_target_release() {
    assert_true("version() == [2021, 1, 0]");
    assert_number("version_num()", 20210100.0, 0.0);
}

#[test]
fn invalid_builtin_argument_types_produce_undef_instead_of_aborting() {
    for expression in [
        "abs(\"not a number\")",
        "sqrt(\"not a number\")",
        "norm([1, \"not a number\"])",
        "cross([1, 2, 3], [4, 5])",
        "len(42)",
        "chr(\"65\")",
        "ord(\"\")",
        "lookup(0, \"not a table\")",
    ] {
        assert_undefined(expression);
    }
}

#[test]
fn invalid_builtin_types_warn_and_continue_through_compilation() {
    let output = assert_program_produces_geometry(
        r#"
          invalid = abs("not a number");
          if (is_undef(invalid)) cube(1);
        "#,
    );
    assert!(
        output
            .messages
            .iter()
            .any(|message| message.starts_with("WARNING:")),
        "an invalid builtin argument should emit a WARNING diagnostic: {:?}",
        output.messages
    );
}

#[test]
fn named_functions_support_defaults_and_named_arguments() {
    assert_program_produces_geometry(
        r#"
          function affine(x, scale = 2, offset = 1) = x * scale + offset;
          if (affine(3) == 7 && affine(3, offset = 4, scale = 5) == 19)
            cube(1);
        "#,
    );
}

#[test]
fn function_literals_capture_their_lexical_environment() {
    assert_program_produces_geometry(
        r#"
          base = 10;
          add_base = function (x) x + base;
          make_adder = function (x) function (y) x + y;
          add_five = make_adder(5);
          if (add_base(2) == 12 && add_five(7) == 12)
            cube(1);
        "#,
    );
}

#[test]
fn tail_recursive_functions_handle_deep_bounded_calls() {
    assert_program_produces_geometry(
        r#"
          function sum_to(n, acc = 0) = n <= 0 ? acc : sum_to(n - 1, acc + n);
          if (sum_to(2000) == 2001000)
            cube(1);
        "#,
    );
}
