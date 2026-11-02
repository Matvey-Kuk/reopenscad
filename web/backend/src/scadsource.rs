//! Source-level operations on SCAD text that do not need the evaluator.
//!
//! * Top-level parameter discovery ([`parameters`]): every `name = expr;` at
//!   file scope, with its line, byte span and literal value.
//! * Variable overrides ([`apply_overrides`]), with OpenSCAD `-D` semantics:
//!   the value of a top-level assignment is replaced, every occurrence of it,
//!   so the rest of the file sees the new value exactly as if it had been
//!   written there. A name the file never assigns is prepended on line 1, so
//!   line numbers in diagnostics stay the same.
//! * Patch-style edits ([`replace_lines`], [`search_replace`]) and the
//!   [`minimal_edit`] that turns any before/after pair into one byte-range
//!   edit, which is what the workspace event log and the browser's "changed by
//!   AI" highlight want.
//!
//! Overrides are serialised from JSON values into SCAD *literals* only —
//! numbers, booleans, strings, `undef` and nested lists — never raw
//! expressions, so an override can change a value but cannot restructure the
//! program around it.

use serde_json::{json, Map, Value};

/// Most overrides accepted in one request.
pub const MAX_OVERRIDES: usize = 64;
/// Longest serialised literal for one override.
pub const MAX_LITERAL_BYTES: usize = 64 * 1024;
/// Largest source any rewrite here may produce: the workspace limit. Checked
/// *before* building, so a growing edit cannot allocate past it.
pub const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;
/// Most top-level assignments one set of overrides may rewrite. OpenSCAD
/// files reassign a name a handful of times; thousands is an attack.
pub const MAX_OVERRIDE_SITES: usize = 4096;
/// Deepest list nesting accepted in an override value.
const MAX_LITERAL_DEPTH: usize = 8;

const KEYWORDS: &[&str] = &[
    "module",
    "function",
    "if",
    "else",
    "for",
    "let",
    "each",
    "true",
    "false",
    "undef",
    "include",
    "use",
    "assert",
    "echo",
    "intersection_for",
];

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Ident(String),
    /// A single `=`.
    Assign,
    Semicolon,
    Open,
    Close,
    /// `<path>` after `include`/`use`.
    Path,
    Other,
}

#[derive(Clone, Debug)]
struct Spanned {
    token: Token,
    start: usize,
    end: usize,
}

/// A deliberately small lexer: enough to find statement boundaries and
/// nesting depth. Strings and comments are skipped as units so their contents
/// can never look like code.
fn tokenize(source: &str) -> Vec<Spanned> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        let start = cursor;
        match byte {
            b' ' | b'\t' | b'\r' | b'\n' => {
                cursor += 1;
                continue;
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                while cursor < bytes.len() && bytes[cursor] != b'\n' {
                    cursor += 1;
                }
                continue;
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor += 2;
                while cursor + 1 < bytes.len()
                    && !(bytes[cursor] == b'*' && bytes[cursor + 1] == b'/')
                {
                    cursor += 1;
                }
                cursor = (cursor + 2).min(bytes.len());
                continue;
            }
            b'"' => {
                cursor += 1;
                while cursor < bytes.len() && bytes[cursor] != b'"' {
                    if bytes[cursor] == b'\\' {
                        cursor += 1;
                    }
                    cursor += 1;
                }
                cursor = (cursor + 1).min(bytes.len());
                tokens.push(Spanned {
                    token: Token::Other,
                    start,
                    end: cursor,
                });
                continue;
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$' => {
                while cursor < bytes.len()
                    && (bytes[cursor].is_ascii_alphanumeric()
                        || matches!(bytes[cursor], b'_' | b'$'))
                {
                    cursor += 1;
                }
                let name = source[start..cursor].to_string();
                let directive = matches!(name.as_str(), "include" | "use");
                tokens.push(Spanned {
                    token: Token::Ident(name),
                    start,
                    end: cursor,
                });
                if directive {
                    let mut look = cursor;
                    while look < bytes.len() && matches!(bytes[look], b' ' | b'\t') {
                        look += 1;
                    }
                    if bytes.get(look) == Some(&b'<') {
                        let path_start = look;
                        while look < bytes.len() && bytes[look] != b'>' && bytes[look] != b'\n' {
                            look += 1;
                        }
                        cursor = (look + 1).min(bytes.len());
                        tokens.push(Spanned {
                            token: Token::Path,
                            start: path_start,
                            end: cursor,
                        });
                    }
                }
                continue;
            }
            b'=' => {
                if bytes.get(cursor + 1) == Some(&b'=') {
                    cursor += 2;
                    tokens.push(Spanned {
                        token: Token::Other,
                        start,
                        end: cursor,
                    });
                } else {
                    cursor += 1;
                    tokens.push(Spanned {
                        token: Token::Assign,
                        start,
                        end: cursor,
                    });
                }
                continue;
            }
            b'!' | b'<' | b'>' if bytes.get(cursor + 1) == Some(&b'=') => {
                cursor += 2;
                tokens.push(Spanned {
                    token: Token::Other,
                    start,
                    end: cursor,
                });
                continue;
            }
            b';' => tokens.push(Spanned {
                token: Token::Semicolon,
                start,
                end: cursor + 1,
            }),
            b'(' | b'[' | b'{' => tokens.push(Spanned {
                token: Token::Open,
                start,
                end: cursor + 1,
            }),
            b')' | b']' | b'}' => tokens.push(Spanned {
                token: Token::Close,
                start,
                end: cursor + 1,
            }),
            _ => {
                // Multi-byte UTF-8 (only legal in strings/comments, handled
                // above) is consumed whole so spans stay on char boundaries.
                let width = source[cursor..].chars().next().map_or(1, char::len_utf8);
                cursor += width;
                tokens.push(Spanned {
                    token: Token::Other,
                    start,
                    end: cursor,
                });
                continue;
            }
        }
        cursor += 1;
    }
    tokens
}

/// One top-level assignment.
#[derive(Clone, Debug, PartialEq)]
pub struct Assignment {
    pub name: String,
    /// 1-based line of the name.
    pub line: usize,
    /// Byte span of the whole statement, `name` through `;`.
    pub statement: (usize, usize),
    /// Byte span of the expression (trimmed).
    pub value: (usize, usize),
}

/// Every `name = expr;` at file scope, in source order.
pub fn top_level_assignments(source: &str) -> Vec<Assignment> {
    let tokens = tokenize(source);
    let mut assignments = Vec::new();
    // Lines are counted incrementally: assignments come in source order, so
    // one forward scan serves them all (rescanning from 0 per assignment was
    // quadratic on a long parameter block).
    let mut counted_to = 0usize;
    let mut line = 1usize;
    let mut depth = 0usize;
    let mut statement_start = true;
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        if depth == 0 && statement_start {
            if let Token::Ident(name) = &token.token {
                if !KEYWORDS.contains(&name.as_str())
                    && tokens.get(index + 1).map(|next| &next.token) == Some(&Token::Assign)
                {
                    // Expression runs to the `;` at depth 0.
                    let value_start = tokens.get(index + 2).map_or(source.len(), |t| t.start);
                    let mut inner = 0usize;
                    let mut cursor = index + 2;
                    let mut end = None;
                    while cursor < tokens.len() {
                        match tokens[cursor].token {
                            Token::Open => inner += 1,
                            Token::Close => inner = inner.saturating_sub(1),
                            Token::Semicolon if inner == 0 => {
                                end = Some(cursor);
                                break;
                            }
                            _ => {}
                        }
                        cursor += 1;
                    }
                    let Some(semicolon) = end else {
                        break;
                    };
                    let value_end = tokens[semicolon - 1].end.max(value_start);
                    if semicolon > index + 2 {
                        line += source.as_bytes()[counted_to..token.start]
                            .iter()
                            .filter(|byte| **byte == b'\n')
                            .count();
                        counted_to = token.start;
                        assignments.push(Assignment {
                            name: name.clone(),
                            line,
                            statement: (token.start, tokens[semicolon].end),
                            value: (value_start, value_end),
                        });
                    }
                    index = semicolon + 1;
                    statement_start = true;
                    continue;
                }
            }
        }
        match token.token {
            Token::Open => {
                depth += 1;
                statement_start = false;
            }
            Token::Close => {
                depth = depth.saturating_sub(1);
                statement_start = depth == 0;
            }
            Token::Semicolon => statement_start = depth == 0,
            Token::Path => statement_start = depth == 0,
            _ => statement_start = false,
        }
        index += 1;
    }
    assignments
}

/// 1-based line containing byte `offset`.
pub fn line_of(source: &str, offset: usize) -> usize {
    source.as_bytes()[..offset.min(source.len())]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

/// The literal value of a SCAD expression, when it is one.
pub fn literal_value(text: &str) -> Option<Value> {
    let mut parser = LiteralParser {
        bytes: text.as_bytes(),
        cursor: 0,
        depth: 0,
    };
    let value = parser.value()?;
    parser.skip_space();
    (parser.cursor == parser.bytes.len()).then_some(value)
}

struct LiteralParser<'a> {
    bytes: &'a [u8],
    cursor: usize,
    depth: usize,
}

impl LiteralParser<'_> {
    fn skip_space(&mut self) {
        while self.cursor < self.bytes.len() && self.bytes[self.cursor].is_ascii_whitespace() {
            self.cursor += 1;
        }
    }

    fn value(&mut self) -> Option<Value> {
        self.skip_space();
        let rest = &self.bytes[self.cursor..];
        for (word, value) in [
            ("true", json!(true)),
            ("false", json!(false)),
            ("undef", Value::Null),
        ] {
            if rest.starts_with(word.as_bytes())
                && !rest
                    .get(word.len())
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            {
                self.cursor += word.len();
                return Some(value);
            }
        }
        match rest.first()? {
            b'[' => {
                self.depth += 1;
                if self.depth > MAX_LITERAL_DEPTH {
                    return None;
                }
                self.cursor += 1;
                let mut items = Vec::new();
                self.skip_space();
                if self.bytes.get(self.cursor) == Some(&b']') {
                    self.cursor += 1;
                    self.depth -= 1;
                    return Some(Value::Array(items));
                }
                loop {
                    items.push(self.value()?);
                    self.skip_space();
                    match self.bytes.get(self.cursor)? {
                        b',' => self.cursor += 1,
                        b']' => {
                            self.cursor += 1;
                            self.depth -= 1;
                            return Some(Value::Array(items));
                        }
                        _ => return None,
                    }
                }
            }
            b'"' => {
                self.cursor += 1;
                let mut text = Vec::new();
                while let Some(&byte) = self.bytes.get(self.cursor) {
                    self.cursor += 1;
                    match byte {
                        b'"' => return String::from_utf8(text).ok().map(Value::String),
                        b'\\' => {
                            let escaped = *self.bytes.get(self.cursor)?;
                            self.cursor += 1;
                            text.push(match escaped {
                                b'n' => b'\n',
                                b't' => b'\t',
                                b'r' => b'\r',
                                other => other,
                            });
                        }
                        other => text.push(other),
                    }
                }
                None
            }
            _ => {
                let start = self.cursor;
                if matches!(self.bytes.get(self.cursor), Some(b'-' | b'+')) {
                    self.cursor += 1;
                }
                while self.bytes.get(self.cursor).is_some_and(|byte| {
                    byte.is_ascii_digit()
                        || matches!(byte, b'.' | b'e' | b'E')
                        || (matches!(byte, b'-' | b'+')
                            && matches!(self.bytes.get(self.cursor - 1), Some(b'e' | b'E')))
                }) {
                    self.cursor += 1;
                }
                let text = std::str::from_utf8(&self.bytes[start..self.cursor]).ok()?;
                let number: f64 = text.parse().ok()?;
                number.is_finite().then(|| json!(number))
            }
        }
    }
}

/// A valid SCAD variable name that is not a keyword.
pub fn valid_variable_name(name: &str) -> bool {
    let body = name.strip_prefix('$').unwrap_or(name);
    name.len() <= 64
        && body
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && body.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !KEYWORDS.contains(&name)
}

/// Serialises a JSON value as a SCAD literal.
pub fn scad_literal(value: &Value) -> Result<String, String> {
    fn write(value: &Value, depth: usize, output: &mut String) -> Result<(), String> {
        if output.len() > MAX_LITERAL_BYTES {
            return Err(format!("value exceeds {MAX_LITERAL_BYTES} bytes"));
        }
        match value {
            Value::Null => output.push_str("undef"),
            Value::Bool(flag) => output.push_str(if *flag { "true" } else { "false" }),
            Value::Number(number) => {
                let number = number
                    .as_f64()
                    .filter(|n| n.is_finite())
                    .ok_or("numbers must be finite")?;
                // Rust's shortest round-trip form; never an exponent SCAD
                // cannot read, never `NaN`/`inf`.
                output.push_str(&format!("{number}"));
            }
            Value::String(text) => {
                output.push('"');
                for character in text.chars() {
                    match character {
                        '"' => output.push_str("\\\""),
                        '\\' => output.push_str("\\\\"),
                        '\n' => output.push_str("\\n"),
                        '\t' => output.push_str("\\t"),
                        '\r' => output.push_str("\\r"),
                        c if c.is_control() => {
                            return Err("strings must not contain control characters".into())
                        }
                        c => output.push(c),
                    }
                }
                output.push('"');
            }
            Value::Array(items) => {
                if depth >= MAX_LITERAL_DEPTH {
                    return Err(format!("lists nest deeper than {MAX_LITERAL_DEPTH} levels"));
                }
                output.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        output.push_str(", ");
                    }
                    write(item, depth + 1, output)?;
                }
                output.push(']');
            }
            Value::Object(_) => {
                return Err(
                    "objects are not SCAD values; use a number, boolean, string, list or null"
                        .into(),
                )
            }
        }
        Ok(())
    }
    let mut output = String::new();
    write(value, 0, &mut output)?;
    if output.len() > MAX_LITERAL_BYTES {
        return Err(format!("value exceeds {MAX_LITERAL_BYTES} bytes"));
    }
    Ok(output)
}

/// The parameter listing `get_parameters` returns, uncapped.
#[cfg(test)]
pub fn parameters(source: &str) -> Vec<Value> {
    parameters_capped(source, usize::MAX).0
}

/// Most parameters one listing returns; the rest are counted, not described.
pub const MAX_PARAMETERS: usize = 2000;

/// [`parameters`], describing at most `limit` assignments (in source order),
/// plus the total number of top-level assignments. Linear in the source size.
pub fn parameters_capped(source: &str, limit: usize) -> (Vec<Value>, usize) {
    let assignments = top_level_assignments(source);
    let mut counts = std::collections::HashMap::new();
    for assignment in &assignments {
        *counts.entry(assignment.name.as_str()).or_insert(0usize) += 1;
    }
    // The newline ending the statement's line. Assignments come in source
    // order, so one found at or past a statement's end serves every later
    // statement before it; searching afresh each time was quadratic on a
    // source with many statements on one line.
    let mut next_newline: Option<usize> = None;
    let listed = assignments
        .iter()
        .take(limit)
        .map(|assignment| {
            let expression = &source[assignment.value.0..assignment.value.1];
            // A trailing `// ...` on the same line is where Customizer-style
            // hints (`// [0:5:360]`) and units live.
            let after = assignment.statement.1;
            let line_end = match next_newline {
                Some(newline) if newline >= after => newline,
                _ => {
                    let found = source[after..]
                        .find('\n')
                        .map_or(source.len(), |offset| after + offset);
                    next_newline = Some(found);
                    found
                }
            };
            let trailing = source[after..line_end].trim();
            let comment = trailing
                .strip_prefix("//")
                .map(|text| text.trim().to_string());
            let value = literal_value(expression);
            json!({
                "name": assignment.name,
                "line": assignment.line,
                "expression": expression,
                "literal": value.is_some(),
                "value": value,
                "comment": comment,
                "start": assignment.value.0,
                "end": assignment.value.1,
                // Assigned more than once: the last assignment wins in
                // OpenSCAD, and set/override rewrite every occurrence.
                "occurrences": counts[assignment.name.as_str()],
            })
        })
        .collect();
    (listed, assignments.len())
}

/// Outcome of [`apply_overrides`].
#[derive(Clone, Debug, PartialEq)]
pub struct Overridden {
    pub source: String,
    /// The rewrites, in original-source coordinates, ascending:
    /// `(start, end, replacement_length)`. Used to map offsets in the
    /// rewritten source back to the original with [`Overridden::original_offset`].
    pub edits: Vec<(usize, usize, usize)>,
    /// One entry per override: name, literal, mode (`replaced` or `added`),
    /// and the lines of the assignments that were rewritten.
    pub applied: Vec<Value>,
}

/// Parses and validates an `overrides` object.
pub fn parse_overrides(value: Option<&Value>) -> Result<Vec<(String, String)>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let map: &Map<String, Value> = value
        .as_object()
        .ok_or("overrides must be an object of {\"name\": value}.")?;
    if map.len() > MAX_OVERRIDES {
        return Err(format!(
            "At most {MAX_OVERRIDES} overrides are accepted at once."
        ));
    }
    map.iter()
        .map(|(name, value)| {
            if !valid_variable_name(name) {
                return Err(format!(
                    "Override name {name:?} is not a SCAD variable name (letters, digits, _, optional leading $; not a keyword)."
                ));
            }
            let literal = scad_literal(value).map_err(|error| format!("Override {name}: {error}."))?;
            Ok((name.clone(), literal))
        })
        .collect()
}

/// Replaces ascending, non-overlapping `ranges` of `source` in one pass,
/// refusing before allocating if the result would pass [`MAX_SOURCE_BYTES`].
fn splice(source: &str, ranges: &[(usize, usize)], replacement: &[&str]) -> Result<String, String> {
    let mut length = source.len() as i128;
    for (index, (start, end)) in ranges.iter().enumerate() {
        length += replacement[index].len() as i128 - (end - start) as i128;
    }
    if length > MAX_SOURCE_BYTES as i128 {
        return Err(format!(
            "The edit would make the source {length} bytes, over the {MAX_SOURCE_BYTES}-byte limit. No edit was applied."
        ));
    }
    let mut output = String::with_capacity(length.max(0) as usize);
    let mut cursor = 0;
    for (index, (start, end)) in ranges.iter().enumerate() {
        output.push_str(&source[cursor..*start]);
        output.push_str(replacement[index]);
        cursor = *end;
    }
    output.push_str(&source[cursor..]);
    Ok(output)
}

/// Rewrites top-level assignments, `-D` style. `allow_new` decides whether a
/// name the file never assigns is prepended (overrides) or rejected
/// (`set_parameters`, which edits the parameter block that exists).
pub fn apply_overrides(
    source: &str,
    overrides: &[(String, String)],
    allow_new: bool,
) -> Result<Overridden, String> {
    if overrides.is_empty() {
        return Ok(Overridden {
            source: source.to_string(),
            edits: Vec::new(),
            applied: Vec::new(),
        });
    }
    let assignments = top_level_assignments(source);
    // Replacements refer to their literal by override index so a value is
    // never copied per matching assignment, and the resulting length is
    // tracked as matches are found: a source of many `x=0;` lines and one
    // large literal is refused before anything proportional to the product
    // is allocated.
    let mut replacements: Vec<(usize, usize, usize)> = Vec::new();
    let mut prefix = String::new();
    let mut applied = Vec::new();
    let mut unknown = Vec::new();
    let mut length = source.len() as i128;
    let too_long = |length: i128| {
        format!(
            "The edit would make the source at least {length} bytes, over the {MAX_SOURCE_BYTES}-byte limit. No edit was applied."
        )
    };
    for (index, (name, literal)) in overrides.iter().enumerate() {
        let mut lines = Vec::new();
        for assignment in assignments.iter().filter(|assignment| &assignment.name == name) {
            if replacements.len() >= MAX_OVERRIDE_SITES {
                return Err(format!(
                    "The overrides match more than {MAX_OVERRIDE_SITES} top-level assignments. No edit was applied."
                ));
            }
            length += literal.len() as i128 - (assignment.value.1 - assignment.value.0) as i128;
            if length > MAX_SOURCE_BYTES as i128 {
                return Err(too_long(length));
            }
            replacements.push((assignment.value.0, assignment.value.1, index));
            lines.push(assignment.line);
        }
        if lines.is_empty() {
            if !allow_new {
                unknown.push(name.clone());
                continue;
            }
            length += (name.len() + literal.len() + 5) as i128;
            if length > MAX_SOURCE_BYTES as i128 {
                return Err(too_long(length));
            }
            prefix.push_str(&format!("{name} = {literal}; "));
            applied.push(json!({"name": name, "literal": literal, "mode": "added", "lines": []}));
            continue;
        }
        applied.push(json!({
            "name": name,
            "literal": literal,
            "mode": "replaced",
            "lines": lines,
        }));
    }
    if !unknown.is_empty() {
        let known: Vec<&str> = assignments.iter().map(|a| a.name.as_str()).collect();
        return Err(format!(
            "No top-level assignment named {}. Top-level parameters are: {}.",
            unknown.join(", "),
            if known.is_empty() {
                "(none)".to_string()
            } else {
                known.join(", ")
            }
        ));
    }
    replacements.sort_by_key(|replacement| replacement.0);
    let mut edits: Vec<(usize, usize, usize)> = Vec::with_capacity(replacements.len() + 1);
    let mut ranges = Vec::with_capacity(replacements.len() + 1);
    let mut texts: Vec<&str> = Vec::with_capacity(replacements.len() + 1);
    if !prefix.is_empty() {
        edits.push((0, 0, prefix.len()));
        ranges.push((0, 0));
        texts.push(&prefix);
    }
    for &(start, end, index) in &replacements {
        let literal = overrides[index].1.as_str();
        edits.push((start, end, literal.len()));
        ranges.push((start, end));
        texts.push(literal);
    }
    let output = splice(source, &ranges, &texts)?;
    Ok(Overridden {
        source: output,
        edits,
        applied,
    })
}

impl Overridden {
    /// Maps a byte offset in the rewritten source to the original source. An
    /// offset inside a rewritten value maps to the start of that value.
    pub fn original_offset(&self, offset: usize) -> usize {
        let mut shift: isize = 0;
        for &(start, end, length) in &self.edits {
            let new_start = (start as isize + shift) as usize;
            if offset < new_start {
                break;
            }
            if offset < new_start + length {
                return start;
            }
            shift += length as isize - (end - start) as isize;
        }
        (offset as isize - shift).max(0) as usize
    }
}

/// The single byte-range edit that turns `old` into `new`: common prefix and
/// suffix trimmed, snapped to char boundaries. `(start, end, text)` replaces
/// `old[start..end]` with `text`.
pub fn minimal_edit(old: &str, new: &str) -> (usize, usize, String) {
    let (old_bytes, new_bytes) = (old.as_bytes(), new.as_bytes());
    let mut prefix = old_bytes
        .iter()
        .zip(new_bytes)
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(prefix) || !new.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let max_suffix = old.len().min(new.len()) - prefix;
    let mut suffix = old_bytes
        .iter()
        .rev()
        .zip(new_bytes.iter().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
        suffix -= 1;
    }
    (
        prefix,
        old.len() - suffix,
        new[prefix..new.len() - suffix].to_string(),
    )
}

/// One changed region between two versions of a source.
///
/// Hunks are listed left to right and are meant to be applied in that order:
/// `start` is a byte offset in the *new* source, which is also the offset in
/// the document after every earlier hunk has been applied. `removed` old bytes
/// at `start` are replaced by `inserted` bytes, `new[start..start + inserted]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub start: usize,
    pub removed: usize,
    pub inserted: usize,
}

/// Lines in the differing middle beyond which `change_hunks` stops looking
/// for separate hunks and reports one prefix/suffix hunk.
const MAX_DIFF_LINES: usize = 4000;
/// Edit distance (inserted + deleted lines) beyond which the same happens.
const MAX_DIFF_DISTANCE: usize = 2000;

/// The changed regions between `old` and `new`, at most `max_hunks` of them.
///
/// Common leading and trailing text is never part of a hunk; the middle is
/// diffed line by line (Myers), so two edits far apart come back as two small
/// hunks rather than one spanning everything between them. Each hunk is then
/// trimmed to the bytes that actually differ. When the middle is too large to
/// diff cheaply a single hunk (as [`minimal_edit`]) is returned; when there
/// are more than `max_hunks` hunks, the ones separated by the least unchanged
/// text are merged. Identical inputs give no hunks.
pub fn change_hunks(old: &str, new: &str, max_hunks: usize) -> Vec<Hunk> {
    if old == new {
        return Vec::new();
    }
    let single = || {
        let (start, end, text) = minimal_edit(old, new);
        vec![Hunk { start, removed: end - start, inserted: text.len() }]
    };
    let old_lines: Vec<&str> = old.split_inclusive('\n').collect();
    let new_lines: Vec<&str> = new.split_inclusive('\n').collect();
    let prefix = old_lines
        .iter()
        .zip(&new_lines)
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = old_lines[prefix..]
        .iter()
        .rev()
        .zip(new_lines[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let old_middle = &old_lines[prefix..old_lines.len() - suffix];
    let new_middle = &new_lines[prefix..new_lines.len() - suffix];
    if old_middle.len() > MAX_DIFF_LINES || new_middle.len() > MAX_DIFF_LINES {
        return single();
    }
    // Lines as small integers, so the diff compares numbers.
    let mut ids: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    let mut a = Vec::with_capacity(old_middle.len());
    let mut b = Vec::with_capacity(new_middle.len());
    for (lines, out) in [(old_middle, &mut a), (new_middle, &mut b)] {
        for line in lines {
            let next = ids.len() as u32;
            out.push(*ids.entry(*line).or_insert(next));
        }
    }
    let Some(line_hunks) = myers_line_hunks(&a, &b, MAX_DIFF_DISTANCE) else {
        return single();
    };
    let starts = |lines: &[&str]| -> Vec<usize> {
        let mut offsets = Vec::with_capacity(lines.len() + 1);
        let mut offset = 0;
        offsets.push(0);
        for line in lines {
            offset += line.len();
            offsets.push(offset);
        }
        offsets
    };
    let (old_starts, new_starts) = (starts(&old_lines), starts(&new_lines));
    let mut hunks: Vec<Hunk> = line_hunks
        .into_iter()
        .map(|(i0, i1, j0, j1)| {
            let (i0, i1, j0, j1) = (i0 + prefix, i1 + prefix, j0 + prefix, j1 + prefix);
            let (old_start, old_end) = (old_starts[i0], old_starts[i1]);
            let (new_start, new_end) = (new_starts[j0], new_starts[j1]);
            // Trim to the bytes that differ inside the changed lines.
            let (head, old_tail, text) =
                minimal_edit(&old[old_start..old_end], &new[new_start..new_end]);
            Hunk {
                start: new_start + head,
                removed: old_tail - head,
                inserted: text.len(),
            }
        })
        .collect();
    // Cheap consistency check: the hunks must account for every byte.
    let removed: usize = hunks.iter().map(|hunk| hunk.removed).sum();
    let inserted: usize = hunks.iter().map(|hunk| hunk.inserted).sum();
    if old.len() + inserted != new.len() + removed {
        return single();
    }
    let max_hunks = max_hunks.max(1);
    if hunks.len() > max_hunks {
        // Merge across the smallest unchanged gaps until few enough remain.
        let gap = |left: &Hunk, right: &Hunk| right.start - (left.start + left.inserted);
        let mut order: Vec<usize> = (0..hunks.len() - 1).collect();
        order.sort_by_key(|&index| (gap(&hunks[index], &hunks[index + 1]), index));
        let mut join = vec![false; hunks.len() - 1];
        for &index in order.iter().take(hunks.len() - max_hunks) {
            join[index] = true;
        }
        let mut merged: Vec<Hunk> = Vec::with_capacity(max_hunks);
        for (index, hunk) in hunks.iter().enumerate() {
            if index > 0 && join[index - 1] {
                let last = merged.last_mut().expect("a hunk precedes a join");
                let between = gap(last, hunk);
                last.removed += between + hunk.removed;
                last.inserted += between + hunk.inserted;
            } else {
                merged.push(*hunk);
            }
        }
        hunks = merged;
    }
    hunks
}

/// Myers' O((N+M)D) diff over line ids: the changed line ranges as
/// `(old_start, old_end, new_start, new_end)`, or `None` when the edit
/// distance exceeds `max_distance`.
fn myers_line_hunks(
    a: &[u32],
    b: &[u32],
    max_distance: usize,
) -> Option<Vec<(usize, usize, usize, usize)>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let limit = (n + m).min(max_distance as isize);
    let offset = limit + 1;
    let mut v = vec![0isize; (2 * limit + 3) as usize];
    // trace[d]: the furthest-reaching x per diagonal k in -d..=d before step d.
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut found = false;
    'search: for d in 0..=limit {
        trace.push(v[(offset - d) as usize..=(offset + d) as usize].to_vec());
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && v[(offset + k - 1) as usize] < v[(offset + k + 1) as usize]) {
                v[(offset + k + 1) as usize]
            } else {
                v[(offset + k - 1) as usize] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[(offset + k) as usize] = x;
            if x >= n && y >= m {
                found = true;
                break 'search;
            }
            k += 2;
        }
    }
    if !found {
        return None;
    }
    // Walk back collecting the matched (equal) line pairs.
    let mut matches: Vec<(usize, usize)> = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (0..trace.len() as isize).rev() {
        if d == 0 {
            while x > 0 && y > 0 {
                x -= 1;
                y -= 1;
                matches.push((x as usize, y as usize));
            }
            break;
        }
        let snapshot = &trace[d as usize];
        let at = |k: isize| snapshot[(k + d) as usize];
        let k = x - y;
        let previous_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) { k + 1 } else { k - 1 };
        let previous_x = at(previous_k);
        let previous_y = previous_x - previous_k;
        // The snake after this step's single insertion or deletion.
        let (snake_x, snake_y) = if previous_k == k + 1 {
            (previous_x, previous_x - previous_k + 1)
        } else {
            (previous_x + 1, previous_y)
        };
        while x > snake_x && y > snake_y {
            x -= 1;
            y -= 1;
            matches.push((x as usize, y as usize));
        }
        x = previous_x;
        y = previous_y;
    }
    matches.reverse();
    let mut hunks = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    for (mi, mj) in matches {
        if mi > i || mj > j {
            hunks.push((i, mi, j, mj));
        }
        i = mi + 1;
        j = mj + 1;
    }
    if i < a.len() || j < b.len() {
        hunks.push((i, a.len(), j, b.len()));
    }
    Some(hunks)
}

/// Replaces 1-based inclusive lines `start_line..=end_line` with `text`.
/// `end_line = start_line - 1` inserts before `start_line`;
/// `start_line = line_count + 1` appends.
pub fn replace_lines(
    source: &str,
    start_line: usize,
    end_line: usize,
    text: &str,
) -> Result<String, String> {
    let mut offsets = vec![0usize];
    for (index, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            offsets.push(index + 1);
        }
    }
    // A trailing newline does not start a real line.
    let line_count = if source.ends_with('\n') {
        offsets.len() - 1
    } else {
        offsets.len()
    };
    if start_line == 0
        || start_line > line_count + 1
        || end_line.saturating_add(1) < start_line
        || end_line > line_count
    {
        return Err(format!(
            "Line range {start_line}..{end_line} is outside the document, which has {line_count} lines. \
             Use 1-based inclusive lines; endLine = startLine - 1 inserts before startLine."
        ));
    }
    let start = offsets.get(start_line - 1).copied().unwrap_or(source.len());
    let end = if end_line < start_line {
        start
    } else {
        offsets.get(end_line).copied().unwrap_or(source.len())
    };
    let mut replacement = text.to_string();
    let replaced_ends_in_newline = end > start && source.as_bytes()[end - 1] == b'\n';
    let inserting = end == start && start < source.len();
    if (replaced_ends_in_newline || inserting)
        && !replacement.is_empty()
        && !replacement.ends_with('\n')
    {
        replacement.push('\n');
    }
    if start == source.len()
        && !source.is_empty()
        && !source.ends_with('\n')
        && !replacement.is_empty()
    {
        replacement.insert(0, '\n');
    }
    let mut output = source.to_string();
    output.replace_range(start..end, &replacement);
    Ok(output)
}

/// Applies search/replace edits in order. Each edit is an object with
/// `search`, `replace`, and optionally `occurrence` (1-based) or
/// `replaceAll: true`. Without either, `search` must match exactly once.
pub fn search_replace(source: &str, edits: &Value) -> Result<String, String> {
    let edits = edits
        .as_array()
        .ok_or("edits must be an array of {search, replace} objects.")?;
    if edits.is_empty() {
        return Err("edits must contain at least one edit.".into());
    }
    if edits.len() > 200 {
        return Err("At most 200 edits are accepted at once.".into());
    }
    let mut output = source.to_string();
    for (index, edit) in edits.iter().enumerate() {
        let search = edit
            .get("search")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| format!("edits[{index}].search must be a non-empty string."))?;
        let replace = edit.get("replace").and_then(Value::as_str).ok_or_else(|| {
            format!("edits[{index}].replace must be a string (use \"\" to delete).")
        })?;
        let positions: Vec<usize> = output.match_indices(search).map(|(at, _)| at).collect();
        let lines = |positions: &[usize]| -> String {
            let mut listed: Vec<String> = positions
                .iter()
                .take(10)
                .map(|at| line_of(&output, *at).to_string())
                .collect();
            if positions.len() > 10 {
                listed.push("...".into());
            }
            listed.join(", ")
        };
        let replace_all = edit
            .get("replaceAll")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let occurrence = edit.get("occurrence").and_then(Value::as_u64);
        if positions.is_empty() {
            let hint = whitespace_insensitive_line(&output, search)
                .map(|line| format!(" A match ignoring whitespace exists at line {line}; copy the exact text from get_design_code."))
                .unwrap_or_default();
            return Err(format!(
                "edits[{index}].search matched nothing (after applying edits 0..{index}).{hint} No edit was applied."
            ));
        }
        let chosen: Vec<usize> = if replace_all {
            positions.clone()
        } else if let Some(occurrence) = occurrence {
            let Some(at) = positions.get((occurrence as usize).wrapping_sub(1)) else {
                return Err(format!(
                    "edits[{index}].occurrence {occurrence} is out of range: search matched {} times (lines {}). No edit was applied.",
                    positions.len(),
                    lines(&positions)
                ));
            };
            vec![*at]
        } else if positions.len() > 1 {
            return Err(format!(
                "edits[{index}].search is ambiguous: it matched {} times (lines {}). Add surrounding context to make it unique, or pass occurrence (1-based) or replaceAll: true. No edit was applied.",
                positions.len(),
                lines(&positions)
            ));
        } else {
            positions.clone()
        };
        let ranges: Vec<(usize, usize)> =
            chosen.iter().map(|at| (*at, at + search.len())).collect();
        let texts = vec![replace; ranges.len()];
        output =
            splice(&output, &ranges, &texts).map_err(|error| format!("edits[{index}]: {error}"))?;
    }
    Ok(output)
}

fn whitespace_insensitive_line(source: &str, search: &str) -> Option<usize> {
    let needle: String = search.chars().filter(|c| !c.is_whitespace()).collect();
    if needle.is_empty() {
        return None;
    }
    let mut squeezed = String::with_capacity(source.len());
    let mut origin = Vec::with_capacity(source.len());
    for (offset, character) in source.char_indices() {
        if !character.is_whitespace() {
            squeezed.push(character);
            origin.extend(std::iter::repeat_n(offset, character.len_utf8()));
        }
    }
    squeezed.find(&needle).map(|at| line_of(source, origin[at]))
}

// ---------------------------------------------------------------------------
// Instrumentation: assert margins and echoed values for `evaluate`.
// ---------------------------------------------------------------------------

/// Marker of the ECHO lines instrumentation adds; a model's own echo output
/// never starts with it.
pub const PROBE_MARKER: &str = "__reopenscad_probe__";
/// Most asserts one instrumented source reports on.
pub const MAX_ASSERT_SITES: usize = 500;
/// Most names one `echo` request evaluates.
pub const MAX_ECHO_NAMES: usize = 50;

/// One comparison of an assert's condition: `lhs op rhs`.
#[derive(Clone, Debug, PartialEq)]
pub struct Comparison {
    pub lhs: String,
    pub op: String,
    pub rhs: String,
}

/// One `assert(...)` in the source.
#[derive(Clone, Debug, PartialEq)]
pub struct AssertSite {
    /// Byte offset of `assert` in the instrumented input.
    pub start: usize,
    pub line: usize,
    /// The condition's source text.
    pub condition: String,
    /// The comparisons it is the `&&` of; empty when it is anything else
    /// (`||`, a function call, a ternary...), which reports pass/fail only.
    pub comparisons: Vec<Comparison>,
}

/// A source rewritten so that it reports every assert's operands instead of
/// stopping at the first failing one, and echoes requested values.
#[derive(Clone, Debug, PartialEq)]
pub struct Instrumented {
    pub source: String,
    /// The rewrites, in input-source coordinates, ascending:
    /// `(start, end, replacement_length)` like [`Overridden::edits`].
    pub edits: Vec<(usize, usize, usize)>,
    pub asserts: Vec<AssertSite>,
}

fn token_text<'a>(source: &'a str, token: &Spanned) -> &'a str {
    &source[token.start..token.end]
}

/// Line numbers by binary search over line starts: one pass over the
/// source, then O(log n) per lookup (`line_of` rescans the prefix).
struct Lines(Vec<usize>);

impl Lines {
    fn new(source: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(source.match_indices('\n').map(|(at, _)| at + 1));
        Self(starts)
    }

    /// 1-based line of `offset`.
    fn of(&self, offset: usize) -> usize {
        self.0.partition_point(|&start| start <= offset)
    }
}

/// Splits `tokens` (one argument list's contents) at depth-0 commas.
fn split_arguments<'a>(source: &str, tokens: &'a [Spanned]) -> Vec<&'a [Spanned]> {
    let mut parts = Vec::new();
    let (mut depth, mut from) = (0usize, 0usize);
    for (index, token) in tokens.iter().enumerate() {
        match token.token {
            Token::Open => depth += 1,
            Token::Close => depth = depth.saturating_sub(1),
            Token::Other if depth == 0 && token_text(source, token) == "," => {
                parts.push(&tokens[from..index]);
                from = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&tokens[from..]);
    parts
}

/// `condition` as an `&&` of comparisons, or empty when it is not one.
fn comparisons(source: &str, tokens: &[Spanned]) -> Vec<Comparison> {
    let text = |tokens: &[Spanned]| match (tokens.first(), tokens.last()) {
        (Some(first), Some(last)) => source[first.start..last.end].trim().to_string(),
        _ => String::new(),
    };
    // Split at depth-0 `&&`; any depth-0 `||`, `?` or `!` (not `!=`)
    // makes the condition opaque.
    let mut conjuncts = Vec::new();
    let (mut depth, mut from, mut index) = (0usize, 0usize, 0usize);
    while index < tokens.len() {
        let token = &tokens[index];
        match token.token {
            Token::Open => depth += 1,
            Token::Close => depth = depth.saturating_sub(1),
            Token::Other if depth == 0 => {
                let piece = token_text(source, token);
                let next = tokens.get(index + 1).map(|next| token_text(source, next));
                match (piece, next) {
                    ("&", Some("&")) if tokens[index + 1].start == token.end => {
                        conjuncts.push(&tokens[from..index]);
                        index += 2;
                        from = index;
                        continue;
                    }
                    ("|", _) | ("?", _) | ("!", _) => return Vec::new(),
                    _ => {}
                }
            }
            _ => {}
        }
        index += 1;
    }
    conjuncts.push(&tokens[from..]);
    let mut found = Vec::new();
    for conjunct in conjuncts {
        let mut depth = 0usize;
        let mut split = None;
        for (index, token) in conjunct.iter().enumerate() {
            match token.token {
                Token::Open => depth += 1,
                Token::Close => depth = depth.saturating_sub(1),
                Token::Other if depth == 0 => {
                    let op = token_text(source, token);
                    if matches!(op, "<" | ">" | "<=" | ">=" | "==" | "!=") {
                        if split.is_some() {
                            return Vec::new();
                        }
                        split = Some((index, op.to_string()));
                    }
                }
                _ => {}
            }
        }
        let Some((at, op)) = split else {
            return Vec::new();
        };
        let (lhs, rhs) = (text(&conjunct[..at]), text(&conjunct[at + 1..]));
        if lhs.is_empty() || rhs.is_empty() {
            return Vec::new();
        }
        found.push(Comparison { lhs, op, rhs });
    }
    found
}

/// Rewrites every `assert(condition, message)` into an `echo` of the
/// condition's value and of each comparison's two sides, tagged with the
/// assert's index. `echo` takes the same places in the grammar as `assert`
/// (a statement prefix with children, or an expression prefix), so the
/// program runs exactly as before when every assert holds, and when one does
/// not it carries on instead of stopping, so every assert is reported. Then
/// appends a file-scope `echo` of each of `echo_names` (top-level variables,
/// or simple expressions of them).
pub fn instrument(source: &str, rewrite_asserts: bool, echo_names: &[String]) -> Result<Instrumented, String> {
    if echo_names.len() > MAX_ECHO_NAMES {
        return Err(format!("echo takes at most {MAX_ECHO_NAMES} names."));
    }
    for name in echo_names {
        let allowed = name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " _$+-*/%.()[],<>=!&|?:^".contains(c));
        let balanced = name.chars().try_fold(0i32, |depth, c| match c {
            '(' | '[' => Some(depth + 1),
            ')' | ']' => (depth > 0).then(|| depth - 1),
            _ => Some(depth),
        }) == Some(0);
        if name.trim().is_empty() || name.len() > 200 || !allowed || !balanced {
            return Err(format!(
                "echo entries must be top-level variable names or simple expressions of them (letters, digits, operators, parentheses, brackets); {name:?} is not."
            ));
        }
    }
    let tokens = tokenize(source);
    let lines = Lines::new(source);
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut asserts = Vec::new();
    if rewrite_asserts {
        let mut index = 0;
        while index + 1 < tokens.len() {
            let is_assert = matches!(&tokens[index].token, Token::Ident(name) if name == "assert")
                && tokens[index + 1].token == Token::Open
                && token_text(source, &tokens[index + 1]) == "(";
            if !is_assert {
                index += 1;
                continue;
            }
            // The matching `)`.
            let mut depth = 0usize;
            let mut close = None;
            for (offset, token) in tokens[index + 1..].iter().enumerate() {
                match token.token {
                    Token::Open => depth += 1,
                    Token::Close => {
                        depth -= 1;
                        if depth == 0 {
                            close = Some(index + 1 + offset);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let Some(close) = close else {
                break;
            };
            if asserts.len() >= MAX_ASSERT_SITES {
                break;
            }
            let arguments = split_arguments(source, &tokens[index + 2..close]);
            // The condition: the first positional argument, or the one named
            // `condition` (`assert(condition = x > 0, message = "...")`).
            let value_of = |argument: &'_ [Spanned]| -> (Option<String>, usize) {
                match argument {
                    [Spanned { token: Token::Ident(name), .. }, Spanned { token: Token::Assign, .. }, ..] => {
                        (Some(name.clone()), 2)
                    }
                    _ => (None, 0),
                }
            };
            let condition_tokens = arguments
                .iter()
                .find(|argument| value_of(argument).0.as_deref() == Some("condition"))
                .or_else(|| arguments.iter().find(|argument| value_of(argument).0.is_none()))
                .map(|argument| &argument[value_of(argument).1..])
                .unwrap_or(&[]);
            let (Some(first), Some(last)) = (condition_tokens.first(), condition_tokens.last()) else {
                index = close + 1;
                continue;
            };
            let condition = source[first.start..last.end].trim().to_string();
            let site = asserts.len();
            let found = comparisons(source, condition_tokens);
            // `? true : false` reports the engine's own truthiness, so
            // `assert(1)` or `assert("x")` reads as passing.
            let mut replacement = format!("echo(\"{PROBE_MARKER}\", \"assert\", {site}, (({condition}) ? true : false)");
            for comparison in &found {
                replacement.push_str(&format!(", ({}), ({})", comparison.lhs, comparison.rhs));
            }
            replacement.push(')');
            edits.push((tokens[index].start, tokens[close].end, replacement));
            asserts.push(AssertSite {
                start: tokens[index].start,
                line: lines.of(tokens[index].start),
                condition,
                comparisons: found,
            });
            index = close + 1;
        }
    }
    let mut appended = String::new();
    for (position, name) in echo_names.iter().enumerate() {
        appended.push_str(&format!("\necho(\"{PROBE_MARKER}\", \"echo\", {position}, ({name}));"));
    }
    if !appended.is_empty() {
        edits.push((source.len(), source.len(), appended));
    }
    let mut output = String::with_capacity(source.len() + 256);
    let mut cursor = 0;
    let mut spans = Vec::with_capacity(edits.len());
    for (start, end, text) in &edits {
        output.push_str(&source[cursor..*start]);
        output.push_str(text);
        cursor = *end;
        spans.push((*start, *end, text.len()));
    }
    output.push_str(&source[cursor..]);
    if output.len() > MAX_SOURCE_BYTES {
        return Err("The instrumented source would exceed the source size limit.".into());
    }
    Ok(Instrumented {
        source: output,
        edits: spans,
        asserts,
    })
}

/// Splits an ECHO line's display text at depth-0 `, ` separators.
pub fn split_echo_values(text: &str) -> Vec<String> {
    let mut values = Vec::new();
    let (mut depth, mut in_string, mut escaped) = (0i32, false, false);
    let mut current = String::new();
    for c in text.chars() {
        if in_string {
            current.push(c);
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                current.push(c);
            }
            '[' | '(' | '{' => {
                depth += 1;
                current.push(c);
            }
            ']' | ')' | '}' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => values.push(std::mem::take(&mut current).trim().to_string()),
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() || !values.is_empty() {
        values.push(current.trim().to_string());
    }
    values
}

// ---------------------------------------------------------------------------
// Definitions: modules, functions and section banners for `list_modules`.
// ---------------------------------------------------------------------------

/// A `module` or `function` definition.
#[derive(Clone, Debug, PartialEq)]
pub struct Definition {
    pub kind: &'static str,
    pub name: String,
    /// The parameter list as written, without the parentheses.
    pub parameters: String,
    pub start_line: usize,
    pub end_line: usize,
    /// Nesting depth: 0 at file scope.
    pub depth: usize,
    /// The comment directly above (consecutive `//` lines or one block
    /// comment), else a trailing `//` on the first line, first line only.
    pub comment: Option<String>,
}

/// A banner comment line at file scope (`// ---- Hinge ----`,
/// `// === Lid ===`), the usual way a long SCAD file is sectioned.
#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    pub line: usize,
    pub title: String,
}

fn comment_above(source: &str, start: usize) -> Option<String> {
    let line_start = source[..start].rfind('\n').map_or(0, |at| at + 1);
    // Trailing comment on the definition's own first line.
    let line_end = source[start..].find('\n').map_or(source.len(), |at| start + at);
    let own_line = &source[start..line_end];
    let mut above: Vec<&str> = Vec::new();
    let mut cursor = line_start;
    while cursor > 0 {
        let previous_end = cursor - 1;
        let previous_start = source[..previous_end].rfind('\n').map_or(0, |at| at + 1);
        let line = source[previous_start..previous_end].trim();
        if let Some(text) = line.strip_prefix("//") {
            // A banner ends the definition's own comment paragraph.
            if is_banner(text) {
                break;
            }
            above.push(text.trim());
        } else if line.ends_with("*/") {
            let block_start = source[..previous_end].rfind("/*")?;
            let block = source[block_start + 2..previous_end].trim_end().trim_end_matches("*/");
            let first = block
                .lines()
                .map(|line| line.trim().trim_start_matches('*').trim())
                .find(|line| !line.is_empty());
            return first.map(str::to_string);
        } else {
            break;
        }
        cursor = previous_start;
    }
    // The paragraph's first line (the lines were collected bottom-up).
    if let Some(text) = above.iter().rev().find(|text| text.chars().any(char::is_alphanumeric)) {
        return Some(text.to_string());
    }
    own_line
        .find("//")
        .map(|at| own_line[at + 2..].trim().to_string())
        .filter(|text| !text.is_empty())
}

/// Every module and function definition (at any depth) and the file-scope
/// banner comments, in source order.
pub fn definitions(source: &str) -> (Vec<Definition>, Vec<Section>) {
    let tokens = tokenize(source);
    let lines = Lines::new(source);
    let mut found = Vec::new();
    let mut depth = 0usize;
    let mut index = 0;
    while index < tokens.len() {
        match &tokens[index].token {
            Token::Open if token_text(source, &tokens[index]) == "{" => depth += 1,
            Token::Close if token_text(source, &tokens[index]) == "}" => depth = depth.saturating_sub(1),
            Token::Ident(keyword) if keyword == "module" || keyword == "function" => {
                let name = match tokens.get(index + 1) {
                    Some(Spanned { token: Token::Ident(name), .. }) => name.clone(),
                    _ => {
                        index += 1;
                        continue;
                    }
                };
                // Parameters: the balanced (...) after the name.
                let open = index + 2;
                if tokens.get(open).is_none_or(|token| token_text(source, token) != "(") {
                    index += 1;
                    continue;
                }
                let mut level = 0usize;
                let mut close = None;
                for (offset, token) in tokens[open..].iter().enumerate() {
                    match token.token {
                        Token::Open => level += 1,
                        Token::Close => {
                            level -= 1;
                            if level == 0 {
                                close = Some(open + offset);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let Some(close) = close else {
                    break;
                };
                let parameters = source[tokens[open].end..tokens[close].start]
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                // Body: a module's `{...}` block or single statement; a
                // function's expression up to its `;`. Both end at the first
                // depth-0 `;` or the `}` closing a leading `{`.
                let mut level = 0usize;
                let mut end = tokens.len() - 1;
                for (offset, token) in tokens[close + 1..].iter().enumerate() {
                    let at = close + 1 + offset;
                    match token.token {
                        Token::Open => level += 1,
                        Token::Close => {
                            level = level.saturating_sub(1);
                            if level == 0 && token_text(source, token) == "}" && keyword == "module" {
                                end = at;
                                break;
                            }
                        }
                        Token::Semicolon if level == 0 => {
                            end = at;
                            break;
                        }
                        _ => {}
                    }
                }
                let start = tokens[index].start;
                found.push(Definition {
                    kind: if keyword == "module" { "module" } else { "function" },
                    name,
                    parameters,
                    start_line: lines.of(start),
                    end_line: lines.of(tokens[end].start),
                    depth,
                    comment: comment_above(source, start),
                });
                index = close + 1;
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    let mut sections = Vec::new();
    let mut offset = 0;
    // Nesting depth at each line start, advanced through the tokens once.
    let (mut next_token, mut depth) = (0usize, 0i64);
    for (number, line) in source.split('\n').enumerate() {
        let trimmed = line.trim();
        let start = offset;
        offset += line.len() + 1;
        while next_token < tokens.len() && tokens[next_token].start < start {
            match tokens[next_token].token {
                Token::Open => depth += 1,
                Token::Close => depth -= 1,
                _ => {}
            }
            next_token += 1;
        }
        let Some(text) = trimmed.strip_prefix("//") else {
            continue;
        };
        let banner = is_banner(text);
        let title = text
            .trim_matches(|c: char| c.is_whitespace() || "-=#*~/".contains(c))
            .to_string();
        // File scope only: no enclosing bracket open at this line.
        if banner && !title.is_empty() && depth <= 0 {
            sections.push(Section {
                line: number + 1,
                title,
            });
        }
    }
    (found, sections)
}

fn is_banner(comment: &str) -> bool {
    ["---", "===", "###", "***", "~~~"].iter().any(|run| comment.contains(run))
}


#[cfg(test)]
mod tests {

    #[test]
    fn asserts_become_echoes_of_their_operands() {
        let source = "w = 2; // wall\nassert(w >= 1.2 && w < 5, \"wall\");\nfunction f(x) = assert(x > 0) x;\nmodule m() assert(is_num(w)) cube(w);\ns = \"assert(no)\";\n";
        let instrumented = instrument(source, true, &["w".into(), "w * 2".into()]).unwrap();
        assert_eq!(instrumented.asserts.len(), 3, "{instrumented:?}");
        let first = &instrumented.asserts[0];
        assert_eq!(first.line, 2);
        assert_eq!(first.comparisons.len(), 2);
        assert_eq!((first.comparisons[0].lhs.as_str(), first.comparisons[0].op.as_str(), first.comparisons[0].rhs.as_str()), ("w", ">=", "1.2"));
        assert_eq!(first.comparisons[1].op, "<");
        assert_eq!(instrumented.asserts[1].comparisons[0].rhs, "0");
        // A call condition reports pass/fail only.
        assert!(instrumented.asserts[2].comparisons.is_empty());
        let out = &instrumented.source;
        assert!(!out.contains("assert(w"), "{out}");
        assert!(out.contains(&format!("echo(\"{PROBE_MARKER}\", \"assert\", 0, ((w >= 1.2 && w < 5) ? true : false), (w), (1.2), (w), (5))")), "{out}");
        assert!(out.contains("= echo("), "{out}");
        // Strings are untouched; echoes are appended.
        assert!(out.contains("s = \"assert(no)\";"));
        assert!(out.ends_with(&format!("echo(\"{PROBE_MARKER}\", \"echo\", 1, (w * 2));")), "{out}");
        assert!(instrument(source, false, &["x); cube(9); echo(1".into()]).is_err());
        let named = instrument("assert(message = \"m\", condition = w > 1);\n", true, &[]).unwrap();
        assert_eq!(named.asserts[0].condition, "w > 1");
        assert!(named.source.contains("((w > 1) ? true : false), (w), (1))"), "{}", named.source);
        assert_eq!(split_echo_values("\"m\", 1, [2, 3], \"a, b\", true"), vec!["\"m\"", "1", "[2, 3]", "\"a, b\"", "true"]);
    }

    #[test]
    fn definitions_list_modules_functions_and_banners() {
        let source = "// ---- Hinge ----\n// The pin that holds the lid.\nmodule pin(d = 3, h = 10) {\n  cylinder(d = d, h = h);\n}\nfunction r(d) = d / 2; // radius\nmodule outer() {\n  module inner() cube(1);\n  inner();\n}\n";
        let (found, sections) = definitions(source);
        assert_eq!(sections, vec![Section { line: 1, title: "Hinge".into() }]);
        assert_eq!(found.len(), 4, "{found:?}");
        assert_eq!((found[0].name.as_str(), found[0].kind, found[0].start_line, found[0].end_line), ("pin", "module", 3, 5));
        assert_eq!(found[0].parameters, "d = 3, h = 10");
        assert_eq!(found[0].comment.as_deref(), Some("The pin that holds the lid."));
        assert_eq!((found[1].name.as_str(), found[1].kind, found[1].comment.as_deref()), ("r", "function", Some("radius")));
        assert_eq!((found[3].name.as_str(), found[3].depth, found[3].start_line, found[3].end_line), ("inner", 1, 8, 8));
    }

    /// Applies hunks left to right, taking inserted text from `new`.
    fn apply_hunks(old: &str, new: &str, hunks: &[Hunk]) -> String {
        let mut document = old.to_string();
        for hunk in hunks {
            document.replace_range(
                hunk.start..hunk.start + hunk.removed,
                &new[hunk.start..hunk.start + hunk.inserted],
            );
        }
        document
    }

    fn numbered(lines: usize) -> String {
        (1..=lines).map(|line| format!("value_{line} = {line};\n")).collect()
    }

    #[test]
    fn change_hunks_reports_two_distant_edits_as_two_small_hunks() {
        let old = numbered(150);
        let new = old
            .replace("value_10 = 10;", "value_10 = 99;")
            .replace("value_140 = 140;", "value_140 = 7;");
        let hunks = change_hunks(&old, &new, 16);
        assert_eq!(hunks.len(), 2, "{hunks:?}");
        assert_eq!(apply_hunks(&old, &new, &hunks), new);
        for (hunk, line) in hunks.iter().zip([10, 140]) {
            assert_eq!(line_of(&new, hunk.start), line);
            assert_eq!(line_of(&new, hunk.start + hunk.inserted.max(1) - 1), line);
            assert!(hunk.inserted <= 3, "{hunk:?}");
        }
        assert!(change_hunks(&old, &old, 16).is_empty());
    }

    #[test]
    fn change_hunks_handle_insertions_deletions_and_utf8() {
        let old = numbered(40);
        let inserted = old.replace("value_20 = 20;\n", "value_20 = 20;\nextra = 1;\n");
        let hunks = change_hunks(&old, &inserted, 16);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].removed, 0);
        assert_eq!(&inserted[hunks[0].start..hunks[0].start + hunks[0].inserted], "extra = 1;\n");
        assert_eq!(apply_hunks(&old, &inserted, &hunks), inserted);

        let deleted = old.replace("value_5 = 5;\n", "").replace("value_30 = 30;\n", "");
        let hunks = change_hunks(&old, &deleted, 16);
        assert_eq!(hunks.len(), 2, "{hunks:?}");
        assert!(hunks.iter().all(|hunk| hunk.inserted == 0));
        assert_eq!(apply_hunks(&old, &deleted, &hunks), deleted);

        let old = "// Größe\nlabel = \"Grüße\";\nx = 1;\n// ünïcödé\ny = \"日本\";\n";
        let new = "// Größe\nlabel = \"Grüßé\";\nx = 1;\n// ünïcödé\ny = \"日本語\";\n";
        let hunks = change_hunks(old, new, 16);
        assert_eq!(hunks.len(), 2, "{hunks:?}");
        for hunk in &hunks {
            assert!(new.is_char_boundary(hunk.start));
            assert!(new.is_char_boundary(hunk.start + hunk.inserted));
        }
        assert_eq!(apply_hunks(old, new, &hunks), new);
        // No trailing newline, edit on the last line.
        assert_eq!(apply_hunks("a\nb", "a\nc", &change_hunks("a\nb", "a\nc", 4)), "a\nc");
        assert_eq!(apply_hunks("", "x\n", &change_hunks("", "x\n", 4)), "x\n");
        assert_eq!(apply_hunks("x\n", "", &change_hunks("x\n", "", 4)), "");
    }

    #[test]
    fn change_hunks_merge_the_closest_when_over_the_cap() {
        let old = numbered(100);
        let mut new = old.clone();
        for line in [3, 5, 50, 90, 92] {
            new = new.replace(&format!("value_{line} = {line};"), &format!("value_{line} = 0;"));
        }
        assert_eq!(change_hunks(&old, &new, 16).len(), 5);
        let capped = change_hunks(&old, &new, 3);
        assert_eq!(capped.len(), 3, "{capped:?}");
        assert_eq!(apply_hunks(&old, &new, &capped), new);
        // The pairs 3/5 and 90/92 merged; 50 stands alone.
        assert_eq!(line_of(&new, capped[0].start), 3);
        assert_eq!(line_of(&new, capped[1].start), 50);
        assert_eq!(line_of(&new, capped[2].start), 90);
        assert_eq!(change_hunks(&old, &new, 1).len(), 1);
    }

    #[test]
    fn change_hunks_reconstruct_pseudo_random_edits() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..300 {
            let old: Vec<String> = (0..(next() % 30)).map(|_| format!("l{}\n", next() % 6)).collect();
            let mut new = old.clone();
            for _ in 0..(next() % 6) {
                let at = if new.is_empty() { 0 } else { (next() as usize) % (new.len() + 1) };
                match next() % 3 {
                    0 if at < new.len() => {
                        new.remove(at);
                    }
                    1 if at < new.len() => new[at] = format!("c{}\n", next() % 6),
                    _ => new.insert(at, format!("n{}\n", next() % 6)),
                }
            }
            let (old, new) = (old.concat(), new.concat());
            let hunks = change_hunks(&old, &new, 64);
            assert_eq!(apply_hunks(&old, &new, &hunks), new, "{old:?} -> {new:?}: {hunks:?}");
        }
    }

    #[test]
    fn huge_one_line_parameter_blocks_list_in_linear_time() {
        // Review regression: 420k statements on one line took 13 s.
        let source = "a=1;".repeat(420_000);
        let started = std::time::Instant::now();
        let (listed, total) = parameters_capped(&source, MAX_PARAMETERS);
        let elapsed = started.elapsed();
        assert_eq!(total, 420_000);
        assert_eq!(listed.len(), MAX_PARAMETERS);
        assert_eq!(listed[0]["occurrences"], json!(420_000));
        if !cfg!(debug_assertions) {
            assert!(elapsed.as_secs_f64() < 1.0, "{elapsed:?}");
        }
    }
    use super::*;

    const SOURCE: &str = "// box\nopen_angle = 90; // [0:360]\npart = \"all\";\nsize = [10, 20, 3];\n$fn = 32;\nfunction f(x) = x * 2;\nmodule m(a = 1) { inner = 5; cube(a); }\ninclude <units.scad>\nwall = f(2);\nif (part == \"all\") { cube(size); }\nlate = 1;\n";

    #[test]
    fn finds_only_top_level_assignments() {
        let names: Vec<String> = top_level_assignments(SOURCE)
            .into_iter()
            .map(|a| a.name)
            .collect();
        assert_eq!(names, ["open_angle", "part", "size", "$fn", "wall", "late"]);
    }

    #[test]
    fn parameters_carry_values_lines_and_hints() {
        let listed = parameters(SOURCE);
        assert_eq!(listed[0]["name"], "open_angle");
        assert_eq!(listed[0]["line"], 2);
        assert_eq!(listed[0]["value"], 90.0);
        assert_eq!(listed[0]["comment"], "[0:360]");
        assert_eq!(listed[2]["value"], json!([10.0, 20.0, 3.0]));
        assert_eq!(listed[4]["literal"], false);
        assert_eq!(listed[4]["expression"], "f(2)");
    }

    #[test]
    fn overrides_replace_values_and_add_new_names_on_line_one() {
        let overrides =
            parse_overrides(Some(&json!({"open_angle": 35, "part": "inner", "$fa": 2}))).unwrap();
        let result = apply_overrides(SOURCE, &overrides, true).unwrap();
        assert!(result.source.contains("open_angle = 35;"));
        assert!(result.source.contains("part = \"inner\";"));
        assert!(result.source.starts_with("$fa = 2; // box\n"));
        assert_eq!(
            line_of(&result.source, result.source.find("late").unwrap()),
            11
        );
        assert!(apply_overrides(SOURCE, &overrides, false).is_err());
        let late = result.source.find("late").unwrap();
        assert_eq!(result.original_offset(late), SOURCE.find("late").unwrap());
        let part = result.source.find("part").unwrap();
        assert_eq!(result.original_offset(part), SOURCE.find("part").unwrap());
    }

    #[test]
    fn overrides_reject_bad_names_and_values() {
        assert!(parse_overrides(Some(&json!({"a b": 1}))).is_err());
        assert!(parse_overrides(Some(&json!({"module": 1}))).is_err());
        assert!(parse_overrides(Some(&json!({"x": {"y": 1}}))).is_err());
        assert_eq!(scad_literal(&json!("a\"b\\")).unwrap(), "\"a\\\"b\\\\\"");
        assert_eq!(
            scad_literal(&json!([1, true, null])).unwrap(),
            "[1, true, undef]"
        );
    }

    #[test]
    fn line_replacement_and_insertion() {
        let source = "a;\nb;\nc;\n";
        assert_eq!(replace_lines(source, 2, 2, "B;").unwrap(), "a;\nB;\nc;\n");
        assert_eq!(
            replace_lines(source, 2, 1, "x;").unwrap(),
            "a;\nx;\nb;\nc;\n"
        );
        assert_eq!(replace_lines(source, 4, 3, "d;").unwrap(), "a;\nb;\nc;\nd;");
        assert_eq!(replace_lines(source, 1, 3, "").unwrap(), "");
        assert!(replace_lines(source, 5, 5, "").is_err());
    }

    #[test]
    fn search_replace_requires_unique_matches() {
        let source = "cube(1);\ncube(1);\nsphere(2);\n";
        let error = search_replace(
            source,
            &json!([{"search": "cube(1)", "replace": "cube(2)"}]),
        )
        .unwrap_err();
        assert!(error.contains("matched 2 times (lines 1, 2)"), "{error}");
        assert_eq!(
            search_replace(
                source,
                &json!([{"search": "cube(1)", "replace": "cube(2)", "occurrence": 2}])
            )
            .unwrap(),
            "cube(1);\ncube(2);\nsphere(2);\n"
        );
        let error =
            search_replace(source, &json!([{"search": "sphere( 2)", "replace": ""}])).unwrap_err();
        assert!(error.contains("line 3"), "{error}");
    }

    #[test]
    fn rewrites_are_linear_and_bounded() {
        let big = "a=1;\n".repeat(400_000);
        let started = std::time::Instant::now();
        assert_eq!(top_level_assignments(&big).last().unwrap().line, 400_000);
        let replaced = search_replace(
            &big,
            &json!([{"search": "1", "replace": "2", "replaceAll": true}]),
        )
        .unwrap();
        assert_eq!(replaced.len(), big.len());
        assert!(
            started.elapsed().as_secs_f64() < 2.0,
            "{:?}",
            started.elapsed()
        );
        let error = search_replace(
            "a",
            &json!(vec![
                json!({"search": "a", "replace": "aa", "replaceAll": true});
                30
            ]),
        )
        .unwrap_err();
        assert!(error.contains("limit"), "{error}");
        assert!(replace_lines("a\n", 1, usize::MAX, "").is_err());
    }

    #[test]
    fn overrides_refuse_amplification_before_allocating() {
        let literal = format!("\"{}\"", "a".repeat(60 * 1024));
        let overrides = vec![("x".to_string(), literal.clone())];
        let started = std::time::Instant::now();
        // 100k reassignments and a 60 KiB literal: refused by length after
        // a few dozen matches; with a short literal, by the site cap.
        let many = "x=0;\n".repeat(100_000);
        let error = apply_overrides(&many, &overrides, true).unwrap_err();
        assert!(error.contains("limit"), "{error}");
        let error = apply_overrides(&many, &[("x".to_string(), "1".to_string())], true)
            .unwrap_err();
        assert!(error.contains("4096"), "{error}");
        // Under the site cap, the growth is refused by length long before
        // the 4000 * 60 KiB product could be built.
        let some = "x=0;\n".repeat(4_000);
        let error = apply_overrides(&some, &overrides, true).unwrap_err();
        assert!(error.contains("limit"), "{error}");
        assert!(started.elapsed().as_secs_f64() < 1.0, "{:?}", started.elapsed());
        // A legitimate handful still works and reports every line.
        let few = "x=0;\nx=1;\ncube(x);\n";
        let done = apply_overrides(few, &[("x".to_string(), "5".to_string())], false).unwrap();
        assert_eq!(done.source, "x=5;\nx=5;\ncube(x);\n");
        assert_eq!(done.applied[0]["lines"], json!([1, 2]));
    }

    #[test]
    fn minimal_edit_spans_only_the_change() {
        assert_eq!(minimal_edit("abcdef", "abXYef"), (2, 4, "XY".into()));
        assert_eq!(minimal_edit("aaa", "aaaa"), (3, 3, "a".into()));
        assert_eq!(minimal_edit("é1", "é2"), (2, 3, "2".into()));
    }
}
