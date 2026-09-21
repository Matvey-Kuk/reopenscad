use super::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) type BuiltinResult = Result<Value, String>;

pub(super) fn call(name: &str, arguments: &[Value]) -> Option<BuiltinResult> {
    let result = match name {
        "abs" => unary_number(name, arguments, f64::abs),
        "sign" => unary_number(name, arguments, |value| {
            if value > 0.0 {
                1.0
            } else if value < 0.0 {
                -1.0
            } else {
                0.0
            }
        }),
        "floor" => unary_number(name, arguments, f64::floor),
        "ceil" => unary_number(name, arguments, f64::ceil),
        "round" => unary_number(name, arguments, f64::round),
        "sqrt" => unary_number(name, arguments, f64::sqrt),
        "exp" => unary_number(name, arguments, f64::exp),
        "ln" => unary_number(name, arguments, f64::ln),
        "log" => unary_number(name, arguments, f64::log10),
        "sin" => unary_number(name, arguments, |value| value.to_radians().sin()),
        "cos" => unary_number(name, arguments, |value| value.to_radians().cos()),
        "tan" => unary_number(name, arguments, |value| value.to_radians().tan()),
        "asin" => unary_number(name, arguments, |value| value.asin().to_degrees()),
        "acos" => unary_number(name, arguments, |value| value.acos().to_degrees()),
        "atan" => unary_number(name, arguments, |value| value.atan().to_degrees()),
        "pow" => binary_number(name, arguments, f64::powf),
        "atan2" => binary_number(name, arguments, |y, x| y.atan2(x).to_degrees()),
        "min" => min_max(name, arguments, f64::min, f64::INFINITY),
        "max" => min_max(name, arguments, f64::max, f64::NEG_INFINITY),
        "norm" => norm(arguments),
        "cross" => cross(arguments),
        "rands" => rands(arguments),
        "len" => length(arguments),
        "concat" => concat(arguments),
        "str" => stringify(arguments),
        "chr" => chr(arguments),
        "ord" => ord(arguments),
        "lookup" => lookup(arguments),
        "search" => search(arguments),
        "is_undef" => predicate(name, arguments, |value| matches!(value, Value::Undefined)),
        "is_bool" => predicate(name, arguments, |value| matches!(value, Value::Bool(_))),
        "is_num" => predicate(
            name,
            arguments,
            |value| matches!(value, Value::Number(number) if !number.is_nan()),
        ),
        "is_string" => predicate(name, arguments, |value| matches!(value, Value::String(_))),
        "is_list" => predicate(name, arguments, |value| matches!(value, Value::Vector(_))),
        "is_function" => predicate(name, arguments, |value| matches!(value, Value::Function(_))),
        "version" => exact(name, arguments, 0).map(|_| {
            Value::Vector(vec![
                Value::Number(2021.0),
                Value::Number(1.0),
                Value::Number(0.0),
            ])
        }),
        "version_num" => exact(name, arguments, 0).map(|_| Value::Number(20_210_100.0)),
        _ => return None,
    };
    Some(result)
}

fn invalid(name: &str, detail: impl AsRef<str>) -> String {
    format!("{name}(): {}", detail.as_ref())
}

fn exact<'a>(name: &str, arguments: &'a [Value], count: usize) -> Result<&'a [Value], String> {
    if arguments.len() == count {
        Ok(arguments)
    } else {
        Err(invalid(
            name,
            format!("expected {count} argument(s), found {}.", arguments.len()),
        ))
    }
}

fn number(name: &str, value: &Value) -> Result<f64, String> {
    match value {
        Value::Number(value) => Ok(*value),
        _ => Err(invalid(name, "expected a number.")),
    }
}

fn integer(name: &str, value: &Value) -> Result<i64, String> {
    let value = number(name, value)?;
    if value.is_finite()
        && value.fract() == 0.0
        && value >= i64::MIN as f64
        && value <= i64::MAX as f64
    {
        Ok(value as i64)
    } else {
        Err(invalid(name, "expected an integer."))
    }
}

fn unary_number(
    name: &str,
    arguments: &[Value],
    operation: impl FnOnce(f64) -> f64,
) -> BuiltinResult {
    exact(name, arguments, 1)?;
    Ok(Value::Number(operation(number(name, &arguments[0])?)))
}

fn binary_number(
    name: &str,
    arguments: &[Value],
    operation: impl FnOnce(f64, f64) -> f64,
) -> BuiltinResult {
    exact(name, arguments, 2)?;
    Ok(Value::Number(operation(
        number(name, &arguments[0])?,
        number(name, &arguments[1])?,
    )))
}

fn min_max(
    name: &str,
    arguments: &[Value],
    operation: impl Fn(f64, f64) -> f64,
    initial: f64,
) -> BuiltinResult {
    if arguments.is_empty() {
        return Err(invalid(name, "expected at least one number."));
    }
    let values: &[Value] = match arguments {
        [Value::Vector(values)] => values,
        values => values,
    };
    if values.is_empty() {
        return Err(invalid(name, "expected at least one number."));
    }
    let mut result = initial;
    for value in values {
        let value = number(name, value)?;
        if value.is_nan() {
            return Ok(Value::Number(f64::NAN));
        }
        result = operation(result, value);
    }
    Ok(Value::Number(result))
}

fn numeric_vector(name: &str, value: &Value) -> Result<Vec<f64>, String> {
    let Value::Vector(values) = value else {
        return Err(invalid(name, "expected a numeric vector."));
    };
    values.iter().map(|value| number(name, value)).collect()
}

fn norm(arguments: &[Value]) -> BuiltinResult {
    exact("norm", arguments, 1)?;
    let values = numeric_vector("norm", &arguments[0])?;
    Ok(Value::Number(
        values.iter().map(|value| value * value).sum::<f64>().sqrt(),
    ))
}

fn cross(arguments: &[Value]) -> BuiltinResult {
    exact("cross", arguments, 2)?;
    let left = numeric_vector("cross", &arguments[0])?;
    let right = numeric_vector("cross", &arguments[1])?;
    match (left.as_slice(), right.as_slice()) {
        ([ax, ay], [bx, by]) => Ok(Value::Number(ax * by - ay * bx)),
        ([ax, ay, az], [bx, by, bz]) => Ok(Value::Vector(vec![
            Value::Number(ay * bz - az * by),
            Value::Number(az * bx - ax * bz),
            Value::Number(ax * by - ay * bx),
        ])),
        _ => Err(invalid(
            "cross",
            "expected two vectors with matching lengths of 2 or 3.",
        )),
    }
}

fn rands(arguments: &[Value]) -> BuiltinResult {
    const MAX_RANDS_COUNT: usize = 1_000_000;

    if !(3..=4).contains(&arguments.len()) {
        return Err(invalid("rands", "expected 3 or 4 arguments."));
    }
    let minimum = number("rands", &arguments[0])?;
    let maximum = number("rands", &arguments[1])?;
    let count = integer("rands", &arguments[2])?;
    if count < 0 {
        return Err(invalid("rands", "count cannot be negative."));
    }
    let count = usize::try_from(count).map_err(|_| invalid("rands", "count is too large."))?;
    if count > MAX_RANDS_COUNT {
        return Err(invalid(
            "rands",
            format!("count exceeds the {MAX_RANDS_COUNT} value safety limit."),
        ));
    }
    let mut state = if let Some(seed) = arguments.get(3) {
        number("rands", seed)?.to_bits()
    } else {
        unseeded_random_state()
    };
    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|_| invalid("rands", "could not allocate the requested result."))?;
    for _ in 0..count {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let unit = ((state >> 11) as f64) * (1.0 / ((1_u64 << 53) as f64));
        values.push(Value::Number(minimum + (maximum - minimum) * unit));
    }
    Ok(Value::Vector(values))
}

fn unseeded_random_state() -> u64 {
    static PROCESS_SEED: OnceLock<u64> = OnceLock::new();
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let process_seed = *PROCESS_SEED.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0x4d59_5df4_d0f3_3173)
    });
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    process_seed ^ sequence.wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

fn length(arguments: &[Value]) -> BuiltinResult {
    exact("len", arguments, 1)?;
    let length = match &arguments[0] {
        Value::String(value) => value.chars().count(),
        Value::Vector(value) => value.len(),
        _ => return Err(invalid("len", "expected a string or list.")),
    };
    Ok(Value::Number(length as f64))
}

fn concat(arguments: &[Value]) -> BuiltinResult {
    let mut result = Vec::new();
    for argument in arguments {
        match argument {
            Value::Vector(values) => result.extend(values.iter().cloned()),
            value => result.push(value.clone()),
        }
    }
    Ok(Value::Vector(result))
}

fn stringify(arguments: &[Value]) -> BuiltinResult {
    let mut result = String::new();
    for argument in arguments {
        result.push_str(&value_string(argument));
    }
    Ok(Value::String(result))
}

fn value_string(value: &Value) -> String {
    match value {
        Value::Number(value) => super::format_openscad_number(*value),
        Value::Bool(value) => value.to_string(),
        Value::String(value) => value.clone(),
        Value::Vector(values) => format!(
            "[{}]",
            values
                .iter()
                .map(value_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Range { start, step, end } => format!(
            "[{}:{}:{}]",
            super::format_openscad_number(*start),
            super::format_openscad_number(*step),
            super::format_openscad_number(*end)
        ),
        Value::Undefined => "undef".into(),
        Value::Function(_) => "function".into(),
    }
}

fn chr(arguments: &[Value]) -> BuiltinResult {
    exact("chr", arguments, 1)?;
    let values = match &arguments[0] {
        Value::Vector(values) => values.clone(),
        Value::Range { start, step, end } => range_values("chr", *start, *step, *end)?
            .into_iter()
            .map(Value::Number)
            .collect(),
        value => vec![value.clone()],
    };
    let mut output = String::new();
    for value in &values {
        let code = integer("chr", value)?;
        let code = u32::try_from(code)
            .ok()
            .and_then(char::from_u32)
            .ok_or_else(|| invalid("chr", "invalid Unicode code point."))?;
        output.push(code);
    }
    Ok(Value::String(output))
}

fn ord(arguments: &[Value]) -> BuiltinResult {
    exact("ord", arguments, 1)?;
    let Value::String(value) = &arguments[0] else {
        return Err(invalid("ord", "expected a non-empty string."));
    };
    let code = value
        .chars()
        .next()
        .ok_or_else(|| invalid("ord", "expected a non-empty string."))?;
    Ok(Value::Number(code as u32 as f64))
}

fn lookup(arguments: &[Value]) -> BuiltinResult {
    exact("lookup", arguments, 2)?;
    let key = number("lookup", &arguments[0])?;
    let Value::Vector(rows) = &arguments[1] else {
        return Err(invalid("lookup", "expected a table."));
    };
    let mut table = Vec::with_capacity(rows.len());
    for row in rows {
        let Value::Vector(columns) = row else {
            return Err(invalid("lookup", "expected two-column table rows."));
        };
        if columns.len() < 2 {
            return Err(invalid("lookup", "expected two-column table rows."));
        }
        table.push((
            number("lookup", &columns[0])?,
            number("lookup", &columns[1])?,
        ));
    }
    if table.is_empty() {
        return Err(invalid("lookup", "expected a non-empty table."));
    }
    if key.is_nan() || table.iter().any(|(table_key, _)| table_key.is_nan()) {
        return Ok(Value::Number(f64::NAN));
    }
    table.sort_by(|left, right| left.0.total_cmp(&right.0));
    if key <= table[0].0 {
        return Ok(Value::Number(table[0].1));
    }
    if key >= table[table.len() - 1].0 {
        return Ok(Value::Number(table[table.len() - 1].1));
    }
    for pair in table.windows(2) {
        let (lower_key, lower_value) = pair[0];
        let (upper_key, upper_value) = pair[1];
        if key <= upper_key {
            let amount = (key - lower_key) / (upper_key - lower_key);
            return Ok(Value::Number(
                lower_value + (upper_value - lower_value) * amount,
            ));
        }
    }
    unreachable!()
}

fn search(arguments: &[Value]) -> BuiltinResult {
    if !(2..=4).contains(&arguments.len()) {
        return Err(invalid("search", "expected 2 to 4 arguments."));
    }
    let returns = arguments
        .get(2)
        .map(|value| integer("search", value))
        .transpose()?
        .unwrap_or(1);
    let column = arguments
        .get(3)
        .map(|value| integer("search", value))
        .transpose()?
        .unwrap_or(0);
    if returns < 0 || column < 0 {
        return Err(invalid(
            "search",
            "counts and column indexes cannot be negative.",
        ));
    }

    let matches: Vec<Value> = match &arguments[0] {
        Value::String(value) => value
            .chars()
            .map(|c| Value::String(c.to_string()))
            .collect(),
        Value::Vector(values) => values.clone(),
        value => vec![value.clone()],
    };
    let mut result = Vec::with_capacity(matches.len());
    for needle in matches {
        let indexes = search_one(&needle, &arguments[1], column as usize)?;
        let indexes: Vec<Value> = if returns == 0 {
            indexes
                .into_iter()
                .map(|index| Value::Number(index as f64))
                .collect()
        } else {
            indexes
                .into_iter()
                .take(returns as usize)
                .map(|index| Value::Number(index as f64))
                .collect()
        };
        if returns == 1 {
            if let Some(index) = indexes.into_iter().next() {
                result.push(index);
            }
        } else {
            result.push(Value::Vector(indexes));
        }
    }
    Ok(Value::Vector(result))
}

fn search_one(needle: &Value, target: &Value, column: usize) -> Result<Vec<usize>, String> {
    match target {
        Value::String(target) => {
            let Value::String(needle) = needle else {
                return Err(invalid("search", "string targets require string matches."));
            };
            let Some(needle) = needle.chars().next() else {
                return Ok(Vec::new());
            };
            Ok(target
                .chars()
                .enumerate()
                .filter_map(|(index, value)| (value == needle).then_some(index))
                .collect())
        }
        Value::Vector(target) => Ok(target
            .iter()
            .enumerate()
            .filter_map(|(index, value)| {
                let candidate = match value {
                    Value::Vector(columns) => columns.get(column)?,
                    value if column == 0 => value,
                    _ => return None,
                };
                (candidate == needle).then_some(index)
            })
            .collect()),
        _ => Err(invalid("search", "expected a string or list target.")),
    }
}

fn predicate(
    name: &str,
    arguments: &[Value],
    predicate: impl FnOnce(&Value) -> bool,
) -> BuiltinResult {
    exact(name, arguments, 1)?;
    Ok(Value::Bool(predicate(&arguments[0])))
}

fn range_values(name: &str, start: f64, step: f64, end: f64) -> Result<Vec<f64>, String> {
    if step == 0.0 {
        return Err(invalid(name, "range step cannot be zero."));
    }
    let mut values = Vec::new();
    let mut value = start;
    while if step > 0.0 {
        value <= end
    } else {
        value >= end
    } {
        if values.len() >= 1_000_000 {
            return Err(invalid(name, "range is too large."));
        }
        values.push(value);
        value += step;
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unseeded_rands_changes_between_calls() {
        let arguments = [Value::Number(0.0), Value::Number(1.0), Value::Number(8.0)];
        let first = call("rands", &arguments)
            .expect("known builtin")
            .expect("valid arguments");
        let second = call("rands", &arguments)
            .expect("known builtin")
            .expect("valid arguments");
        assert_ne!(first, second);
    }

    #[test]
    fn rands_rejects_counts_above_the_safety_limit() {
        let arguments = [
            Value::Number(0.0),
            Value::Number(1.0),
            Value::Number(1_000_001.0),
        ];
        let error = call("rands", &arguments)
            .expect("known builtin")
            .expect_err("oversized result should be rejected");
        assert!(error.contains("safety limit"));
    }

    #[test]
    fn min_and_max_propagate_nan() {
        for (name, arguments) in [
            ("min", vec![Value::Number(2.0), Value::Number(f64::NAN)]),
            (
                "max",
                vec![Value::Vector(vec![
                    Value::Number(f64::NAN),
                    Value::Number(2.0),
                ])],
            ),
        ] {
            let Value::Number(result) = call(name, &arguments)
                .expect("known builtin")
                .expect("valid numeric arguments")
            else {
                panic!("{name}() should return a number");
            };
            assert!(result.is_nan(), "{name}() should propagate nan");
        }
    }

    #[test]
    fn lookup_propagates_nan_without_panicking() {
        let table = Value::Vector(vec![
            Value::Vector(vec![Value::Number(0.0), Value::Number(1.0)]),
            Value::Vector(vec![Value::Number(1.0), Value::Number(2.0)]),
        ]);
        let Value::Number(result) = call("lookup", &[Value::Number(f64::NAN), table])
            .expect("known builtin")
            .expect("nan is a numeric lookup key")
        else {
            panic!("lookup() should return a number");
        };
        assert!(result.is_nan());

        let table_with_nan_key = Value::Vector(vec![
            Value::Vector(vec![Value::Number(0.0), Value::Number(1.0)]),
            Value::Vector(vec![Value::Number(f64::NAN), Value::Number(2.0)]),
        ]);
        let Value::Number(result) = call("lookup", &[Value::Number(0.5), table_with_nan_key])
            .expect("known builtin")
            .expect("nan table keys are handled without panicking")
        else {
            panic!("lookup() should return a number");
        };
        assert!(result.is_nan());
    }
}
