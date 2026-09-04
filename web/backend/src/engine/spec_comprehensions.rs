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

fn assert_value(expression: &str, expected: Value) {
    let actual = eval(expression)
        .unwrap_or_else(|error| panic!("failed to evaluate `{expression}`: {error}"));
    assert_eq!(actual, expected, "unexpected value for `{expression}`");
}

fn numbers(values: &[f64]) -> Value {
    Value::Vector(values.iter().copied().map(Value::Number).collect())
}

#[test]
fn comprehensions_generate_mix_and_nest_values() {
    assert_value(
        "[for (i = [0:4]) i * i]",
        numbers(&[0.0, 1.0, 4.0, 9.0, 16.0]),
    );
    assert_value(
        "[0, for (i = [1:3]) i, 4]",
        numbers(&[0.0, 1.0, 2.0, 3.0, 4.0]),
    );
    assert_value(
        "[for (i = [0:2], j = [10:11]) i + j]",
        numbers(&[10.0, 11.0, 11.0, 12.0, 12.0, 13.0]),
    );
    assert_value(
        "[for (i = [0:2]) for (j = [0:1]) [i, j]]",
        Value::Vector(vec![
            numbers(&[0.0, 0.0]),
            numbers(&[0.0, 1.0]),
            numbers(&[1.0, 0.0]),
            numbers(&[1.0, 1.0]),
            numbers(&[2.0, 0.0]),
            numbers(&[2.0, 1.0]),
        ]),
    );
    assert_value(
        "[for (i = [0:2]) [i, i * i]]",
        Value::Vector(vec![
            numbers(&[0.0, 0.0]),
            numbers(&[1.0, 1.0]),
            numbers(&[2.0, 4.0]),
        ]),
    );
}

#[test]
fn comprehension_if_else_and_let_are_scoped_and_composable() {
    assert_value(
        "[for (i = [0:5]) if (i % 2 == 0) i]",
        numbers(&[0.0, 2.0, 4.0]),
    );
    assert_value(
        "[for (i = [0:3]) if (i < 2) i else -i]",
        numbers(&[0.0, 1.0, -2.0, -3.0]),
    );
    assert_value(
        "[for (i = [1:3]) let (a = i * 2, b = a + 1) b]",
        numbers(&[3.0, 5.0, 7.0]),
    );
    assert_value("[let (a = 2) if (a > 1) a else 0]", numbers(&[2.0]));
    assert_value("let (v = [for (i = [0:1]) i]) i", Value::Undefined);
    assert_value("[for (i = []) i]", Value::Vector(Vec::new()));
}

#[test]
fn each_splices_one_level_and_preserves_non_lists() {
    assert_value(
        "[each [1, 2], each [3:4], each 5]",
        numbers(&[1.0, 2.0, 3.0, 4.0, 5.0]),
    );
    assert_value(
        "[for (i = [1:3]) each [i, -i]]",
        numbers(&[1.0, -1.0, 2.0, -2.0, 3.0, -3.0]),
    );
    assert_value("[each undef]", Value::Vector(vec![Value::Undefined]));
    assert_value(
        "[each [[1, 2], [3, 4]]]",
        Value::Vector(vec![numbers(&[1.0, 2.0]), numbers(&[3.0, 4.0])]),
    );
}

#[test]
fn c_style_generators_initialize_test_update_and_stop() {
    assert_value(
        "[for (i = 0; i < 5; i = i + 1) i * i]",
        numbers(&[0.0, 1.0, 4.0, 9.0, 16.0]),
    );
    assert_value(
        "[for (i = 0, j = 10; i < 3; i = i + 1, j = j + 2) [i, j]]",
        Value::Vector(vec![
            numbers(&[0.0, 10.0]),
            numbers(&[1.0, 12.0]),
            numbers(&[2.0, 14.0]),
        ]),
    );
    let error = eval("[for (i = 0; true; i = i + 1) i]")
        .expect_err("an unbounded C-style generator must hit its safety budget");
    assert!(error.to_string().contains("step limit"));

    let nested = eval("[for (i = [0:100]) [for (j = [0:999]) j]]")
        .expect_err("nested comprehensions must share an aggregate output budget");
    assert!(nested.to_string().contains("step limit"));
}

#[test]
fn postfix_calls_accept_closures_and_arbitrary_function_values() {
    assert_value("(function (x) x * x)(5)", Value::Number(25.0));
    assert_value(
        "let (make = function (x) function (y) x + y) make(2)(3)",
        Value::Number(5.0),
    );
    assert_value(
        "[function (x) x + 1, function (x) x * 2][1](4)",
        Value::Number(8.0),
    );
    assert_value(
        "(true ? function (x) x + 1 : function (x) x - 1)(3)",
        Value::Number(4.0),
    );
    assert_value(
        "let (f = function (x, y = 2) x * y) f(y = 3, x = 4)",
        Value::Number(12.0),
    );
}

#[test]
fn calling_a_non_function_yields_undef_and_warns() {
    take_expression_warnings();
    assert_value("([1, 2][0])(4)", Value::Undefined);
    let warnings = take_expression_warnings();
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("non-function")));
}

#[test]
fn dependency_resolution_understands_generator_bindings_and_calls() {
    let output = compile(
        r#"
          values = [for (i = [0:2]) transform(i)];
          offset = 10;
          function transform(value) = value + offset;
          if (values == [10, 11, 12] && i == undef)
            cube(2, center=true);
        "#,
        Quality::Preview,
        None,
    )
    .expect("comprehension dependencies should resolve without leaking bindings");
    assert!(!output.mesh.triangles.is_empty());
}

#[test]
fn malformed_generators_are_rejected_during_parsing() {
    assert!(eval("[for () 1]").is_err());
    assert!(eval("[for (i = 0; i < 3;) i]").is_err());
    assert!(eval("[if (true) 1 else]").is_err());
    assert!(eval("[for (i = undef) i]")
        .unwrap_err()
        .to_string()
        .contains("range or vector"));

    let deeply_nested = format!("[{}0]", "for (i = [0]) ".repeat(140));
    assert!(eval(&deeply_nested)
        .unwrap_err()
        .to_string()
        .contains("nesting depth"));
}
