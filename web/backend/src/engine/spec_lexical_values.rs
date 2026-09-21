use super::*;

fn evaluate_source(source: &str) -> Result<Shape, EngineError> {
    let tokens = Lexer::new(source).tokenize()?;
    let statements = Parser::new(tokens).parse_program()?;
    Evaluator::default().evaluate(&statements)
}

fn assert_source_condition(expression: &str) {
    let source = format!(
        "if ({expression}) cube(2, center=true); else translate([10, 0, 0]) cube(2, center=true);"
    );
    let shape = evaluate_source(&source)
        .unwrap_or_else(|error| panic!("failed to evaluate `{expression}`: {error}"));
    assert!(
        shape.distance(Vec3::default()) < 0.0,
        "expected `{expression}` to be true"
    );
}

fn assert_source_conditions(expressions: &[&str]) {
    for expression in expressions {
        assert_source_condition(expression);
    }
}

#[test]
fn decodes_every_specified_string_escape() {
    let tokens = Lexer::new(r#""\"\\\t\n\r\x41\u03A9\U01F642""#)
        .tokenize()
        .expect("escaped string should lex");

    assert_eq!(tokens, vec![Token::String("\"\\\t\n\rAΩ🙂".into())]);
}

#[test]
fn preserves_unicode_string_literals() {
    let tokens = Lexer::new(r#""Grüße 世界 🙂""#)
        .tokenize()
        .expect("Unicode string should lex");

    assert_eq!(tokens, vec![Token::String("Grüße 世界 🙂".into())]);
}

#[test]
fn exposes_pi_at_its_2021_01_value() {
    assert_source_condition("PI == 3.141592653589793");
}

#[test]
fn accepts_all_specified_number_literal_forms() {
    assert_source_conditions(&[".5 == 0.5", "5. == 5", "1e3 == 1000", "1.5E-3 == 0.0015"]);
}

#[test]
fn supports_heterogeneous_nested_vectors_and_deep_equality() {
    assert_source_condition(r#"[1, "two", [true, undef, [3]]] == [1, "two", [true, undef, [3]]]"#);
    assert_source_condition(r#"[1, "two"] != [1, "three"]"#);
}

#[test]
fn indexes_vectors_strings_and_nested_values() {
    assert_source_conditions(&[
        "[10, 20, 30][1] == 20",
        "[[1, 2], [3, 4]][1][0] == 3",
        r#""🙂x"[0] == "🙂""#,
    ]);
}

#[test]
fn member_access_aliases_vector_coordinates() {
    assert_source_conditions(&["[7, 8, 9].x == 7", "[7, 8, 9].y == 8", "[7, 8, 9].z == 9"]);
}

#[test]
fn bad_indexes_and_missing_members_yield_undef() {
    assert_source_conditions(&[
        "[1, 2][-1] == undef",
        "[1, 2][2] == undef",
        "[1, 2][0.5] == undef",
        "[1, 2].z == undef",
        r#""x"[1] == undef"#,
    ]);
}

#[test]
fn ternary_is_right_associative_lowest_precedence_and_lazy() {
    assert_source_conditions(&[
        "(true ? 1 : true ? 2 : 3) == 1",
        "(false ? 1 : true ? 2 : 3) == 2",
        "(true || false ? 7 : 9) == 7",
        "(true ? 42 : assert(false) 0) == 42",
        "(false ? assert(false) 0 : 24) == 24",
    ]);
}

#[test]
fn exponentiation_is_right_associative_with_specified_precedence() {
    assert_source_conditions(&[
        "2 ^ 3 ^ 2 == 512",
        // `^` binds tighter than unary minus: the grammar reads `'-' unary`
        // over `call '^' unary`, so this is `-(2^2)` and not `(-2)^2`.
        "-2 ^ 2 == -4",
        "-2 ^ 3 == -8",
        "-2 ^ -2 == -0.25",
        "!1 ^ 0 == false",
        "2 ^ -2 == 0.25",
        "2 * 3 ^ 2 == 18",
        "2 ^ 3 * 2 == 16",
        // Unary minus still loses to multiplication, which is what would
        // break if its operand were parsed any tighter than the exponent.
        "-2 * 3 == -6",
    ]);
}

/// Vectors order lexicographically, and a prefix orders before what extends
/// it. SCAD sorting helpers are written against exactly this; answering
/// `false` to every pair mis-sorts them with no diagnostic.
#[test]
fn vectors_compare_lexicographically() {
    assert_source_conditions(&[
        "[1, 2] < [1, 3]",
        "[1, 2] <= [1, 3]",
        "[1, 3] > [1, 2]",
        "[1, 2] < [1, 2, 3]",
        "[1, 2, 3] > [1, 2]",
        "!([1, 2] < [1, 2])",
        "[1, 2] <= [1, 2]",
        "[1, 2] >= [1, 2]",
        "[2] > [1, 9, 9]",
        "[[1, 2], [3]] < [[1, 2], [4]]",
        // An element pair that cannot be ordered makes the whole comparison
        // unsupported rather than silently deciding it.
        r#"!([1, "a"] < [1, 2])"#,
    ]);
}

/// Adding vectors of different lengths keeps the common prefix, so
/// `[1,2] + [1,2,3]` is `[2,4]` rather than undef.
#[test]
fn vector_arithmetic_uses_the_common_prefix() {
    assert_source_conditions(&[
        "[1, 2] + [1, 2, 3] == [2, 4]",
        "[1, 2, 3] - [1, 1] == [0, 1]",
        "[1, 2] + [] == []",
    ]);
}

/// A string iterates over its characters, in `for` and in `each` alike. This
/// was a hard error, which is worse than a wrong value.
#[test]
fn strings_iterate_by_character() {
    assert_source_conditions(&[
        r#"[for (c = "ab") c] == ["a", "b"]"#,
        r#"[each "ab"] == ["a", "b"]"#,
        r#"[each ""] == []"#,
        // Codepoints, not bytes, like every other string operation here.
        r#"len([for (c = "hé") c]) == 2"#,
        // `each` of a non-iterable still splices as a single element.
        "[each 5] == [5]",
    ]);
}

#[test]
fn modulo_is_fmod_with_dividend_sign_and_fractional_operands() {
    assert_source_conditions(&[
        "-7 % 3 == -1",
        "7 % -3 == 1",
        "-7 % -3 == -1",
        "5.5 % 2 == 1.5",
    ]);
}

#[test]
fn logical_operators_return_booleans_and_short_circuit() {
    assert_source_conditions(&[
        "(1 && 2) == true",
        r#"("" || 5) == true"#,
        r#"(0 || "") == false"#,
        "(false && assert(false) true) == false",
        "(true || assert(false) false) == true",
    ]);
}

#[test]
fn performs_vector_arithmetic_scaling_and_dot_product() {
    assert_source_conditions(&[
        "[1, 2, 3] + [4, 5, 6] == [5, 7, 9]",
        "[4, 5, 6] - [1, 2, 3] == [3, 3, 3]",
        "2 * [1, 2, 3] == [2, 4, 6]",
        "[1, 2, 3] * 2 == [2, 4, 6]",
        "[2, 4, 6] / 2 == [1, 2, 3]",
        "-[1, 2, 3] == [-1, -2, -3]",
        "[1, 2, 3] * [4, 5, 6] == 32",
    ]);
}

#[test]
fn arithmetic_type_errors_and_undef_propagate_without_aborting() {
    let cases = [
        "undef + 1 == undef",
        "-undef == undef",
        "undef * [1, 2] == undef",
        r#"[1, "x"] - [1, 2] == undef"#,
        r#""x" * 2 == undef"#,
    ];
    assert_source_conditions(&cases);

    let expression = cases.join(" && ");

    let output = compile(
        &format!("if ({expression}) cube(1); else sphere(1);"),
        Quality::Preview,
        None,
    )
    .expect("type errors should warn and yield undef, not abort evaluation");
    assert!(
        output
            .messages
            .iter()
            .any(|message| message.to_ascii_lowercase().contains("warning")),
        "unsupported arithmetic should emit a warning"
    );
}

#[test]
fn implements_all_specified_false_values_and_nan_truthiness() {
    assert_source_conditions(&[
        "!false",
        "!0",
        "!(-0)",
        r#"!"""#,
        "![]",
        "!undef",
        "!!true",
        "!!1",
        "!!-1",
        r#"!!"0""#,
        "!![0]",
        "!![0:0]",
        "!!(0 / 0)",
    ]);
}

#[test]
fn equality_is_deep_type_sensitive_and_ieee_aware() {
    assert_source_conditions(&[
        "true != 1",
        r#""1" != 1"#,
        "[1, [2, 3]] == [1, [2, 3]]",
        "[1, [2, 3]] != [1, [2, 4]]",
        "undef == undef",
        "(0 / 0) != (0 / 0)",
    ]);
}

#[test]
fn ordering_supports_numbers_strings_bools_and_mixed_bool_numbers() {
    assert_source_conditions(&[
        "1 < 2",
        "2 <= 2",
        "3 > 2",
        "3 >= 3",
        r#""alpha" < "beta""#,
        r#""beta" >= "beta""#,
        "false < true",
        "false <= 0",
        "true <= 1",
        "1 >= true",
        "true > 0",
    ]);
}

#[test]
fn unsupported_ordering_returns_false_and_warns() {
    assert_source_conditions(&["!(undef >= 0)", r#"!("1" < 1)"#]);

    let output = compile(
        r#"if ("1" < 1) translate([10, 0, 0]) cube(1); else cube(1);"#,
        Quality::Preview,
        None,
    )
    .expect("unsupported ordering should return false rather than abort");
    assert!(
        output
            .messages
            .iter()
            .any(|message| message.to_ascii_lowercase().contains("warning")),
        "unsupported ordering should emit a warning"
    );
}

/// Hex literals and the bitwise operators, held to the binary.
///
/// Every expected value below was read off `openscad -o out.echo` on this
/// machine, not off the documentation. They matter twice over: the operators
/// used to fail in the *lexer*, so a single `&` rejected the whole file.
#[test]
fn hex_literals_and_bitwise_operators_match_openscad() {
    assert_source_conditions(&[
        // Literals. Only the lowercase `0x` prefix is a number.
        "0x10 == 16",
        "0xff == 255",
        "0xFF == 255",
        // The operators themselves.
        "(0xFF & 0x0F) == 15",
        "(12 | 3) == 15",
        "~5 == -6",
        "(1 << 2) == 4",
        "(256 >> 3) == 32",
        // Operands are truncated toward zero and read as two's complement.
        "(5.7 & 3) == 1",
        "(-1 & 255) == 255",
        "(2.5 | 1) == 3",
        "~2.7 == -3",
        "(-3.5 & 7) == 5",
        "~0 == -1",
        // `>>` is arithmetic: the sign bit is replicated.
        "(-8 >> 1) == -4",
        "(-1 >> 1) == -1",
        "(-5 >> 2) == -2",
        // Shifts take an integer distance and wrap at 64 bits.
        "(1 << 0.5) == 1",
        "(1 << 63) == -9223372036854775808",
        // Precedence: OpenSCAD puts the bitwise group *above* the comparisons
        // rather than below them as C does.
        "(3 & 1 == 1) == true",
        "(1 | 2 == 2) == false",
        "(1 & 2 | 4) == 4",
        "(1 | 2 & 4) == 1",
        "(1 << 2 + 1) == 8",
        "(1 | 2 << 2) == 9",
        "~1 + 1 == -1",
        "(2 * 3 & 4) == 4",
        "(2 ^ 3 & 4) == 0",
        "(8 >> 1 >> 1) == 2",
        "(true && 1 & 2) == false",
    ]);
}

/// A bitwise operator on anything but two numbers is undef with a warning,
/// which is what the binary does — it never coerces the way `+` does.
#[test]
fn bitwise_operators_on_non_numbers_are_undef() {
    assert_source_conditions(&[
        "is_undef(\"a\" & 1)",
        "is_undef(true & 1)",
        "is_undef(undef & 1)",
        "is_undef([1, 2] & 1)",
        "is_undef(~\"a\")",
        // Out-of-range shift distances, both ends.
        "is_undef(1 << 64)",
        "is_undef(1 << -1)",
        "is_undef(8 >> -1)",
    ]);

    let output = compile(
        "if (is_undef(\"a\" & 1)) cube(1); else translate([9, 0, 0]) cube(1);",
        Quality::Preview,
        None,
    )
    .expect("an undefined bitwise operation is not fatal");
    assert!(
        output
            .messages
            .iter()
            .any(|message| message == "WARNING: undefined operation (string & number)"),
        "expected the undefined operation to be named, got {:?}",
        output.messages
    );
}
