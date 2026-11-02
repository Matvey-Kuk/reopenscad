//! Print orientation per object and the bed layout it implies.
//!
//! The orientation travels with the design: it is a comment directive in the
//! SCAD source, so the single file stays the whole design and still renders
//! unchanged in OpenSCAD (which ignores comments):
//!
//! ```text
//! // @print <object> rotate=[rx, ry, rz]
//! // @print <object> face=-z            (or +x, -y, ... or [nx, ny, nz])
//! ```
//!
//! `<object>` is an object ID (`object-lid`) or name; a name with spaces is
//! quoted (`"Lid 2"`). `rotate` has OpenSCAD's `rotate([x, y, z])` meaning
//! (about X, then Y, then Z, degrees). `face` names the outward normal of the
//! face that goes down on the bed; the part turns the shortest way to put it
//! there. Either way the part is then dropped onto z = 0. An object without a
//! directive prints as modelled.
//!
//! On top of the orientations, [`layout`] packs every object's footprint onto
//! the printer's plate (shelf packing, first fit by decreasing depth,
//! a quarter turn when it helps, `spacing` mm apart), spilling onto further
//! plates and listing what cannot fit at all.

use crate::vec3::{cross as cross3, dot as dot3};
use super::*;
use crate::agent::{ApiError, EvalSource};

/// A printer's build volume (mm).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PrinterPlate {
    pub id: &'static str,
    pub brand: &'static str,
    pub name: &'static str,
    pub size: [f64; 3],
}

const fn plate(id: &'static str, brand: &'static str, name: &'static str, size: [f64; 3]) -> PrinterPlate {
    PrinterPlate { id, brand, name, size }
}

/// The plates the `plate` argument names by ID.
pub(crate) const PRINTER_PLATES: &[PrinterPlate] = &[
    plate("bambu-a1-mini", "Bambu Lab", "A1 mini", [180.0, 180.0, 180.0]),
    plate("bambu-a1", "Bambu Lab", "A1", [256.0, 256.0, 256.0]),
    plate("bambu-p1", "Bambu Lab", "P1S / P1P", [256.0, 256.0, 256.0]),
    plate("bambu-p2s", "Bambu Lab", "P2S", [256.0, 256.0, 256.0]),
    plate("bambu-x1", "Bambu Lab", "X1 Carbon / X1E", [256.0, 256.0, 256.0]),
    plate("bambu-x2d", "Bambu Lab", "X2D", [256.0, 256.0, 260.0]),
    plate("bambu-h2c", "Bambu Lab", "H2C", [305.0, 320.0, 325.0]),
    plate("bambu-h2d", "Bambu Lab", "H2D", [325.0, 320.0, 325.0]),
    plate("bambu-h2s", "Bambu Lab", "H2S", [340.0, 320.0, 340.0]),
    plate("creality-ender3-v3-ke", "Creality", "Ender-3 V3 KE", [220.0, 220.0, 240.0]),
    plate("creality-ender3-v3", "Creality", "Ender-3 V3 / V3 SE", [220.0, 220.0, 250.0]),
    plate("creality-k1", "Creality", "K1 / K1C / K1 SE", [220.0, 220.0, 250.0]),
    plate("creality-hi", "Creality", "Hi / Hi Combo", [260.0, 260.0, 300.0]),
    plate("creality-k1-max", "Creality", "K1 Max", [300.0, 300.0, 300.0]),
    plate("creality-k2-plus", "Creality", "K2 Plus", [350.0, 350.0, 350.0]),
    plate("creality-ender5-max", "Creality", "Ender-5 Max", [400.0, 400.0, 400.0]),
    plate("prusa-mini", "Prusa", "MINI+", [180.0, 180.0, 180.0]),
    plate("prusa-mk4s", "Prusa", "MK4S / MK4", [250.0, 210.0, 220.0]),
    plate("prusa-core-one", "Prusa", "CORE One", [250.0, 220.0, 270.0]),
    plate("prusa-core-one-l", "Prusa", "CORE One L+", [300.0, 300.0, 330.0]),
    plate("prusa-xl", "Prusa", "XL", [360.0, 360.0, 360.0]),
    plate("anycubic-kobra2-pro", "Anycubic", "Kobra 2 Pro", [220.0, 220.0, 250.0]),
    plate("anycubic-kobra-s1", "Anycubic", "Kobra S1", [250.0, 250.0, 250.0]),
    plate("anycubic-kobra3", "Anycubic", "Kobra 3", [250.0, 250.0, 260.0]),
    plate("anycubic-kobra-s1-max", "Anycubic", "Kobra S1 Max", [350.0, 350.0, 350.0]),
    plate("anycubic-kobra3-max", "Anycubic", "Kobra 3 Max", [420.0, 420.0, 500.0]),
    plate("elegoo-neptune4", "Elegoo", "Neptune 4 / 4 Pro", [225.0, 225.0, 265.0]),
    plate("elegoo-centauri-carbon", "Elegoo", "Centauri Carbon", [256.0, 256.0, 256.0]),
    plate("elegoo-neptune4-plus", "Elegoo", "Neptune 4 Plus", [320.0, 320.0, 385.0]),
    plate("elegoo-neptune4-max", "Elegoo", "Neptune 4 Max", [420.0, 420.0, 480.0]),
];

/// The bed a layout assumes with no `plate` argument and no stored value.
const DEFAULT_BED: [f64; 3] = [256.0, 256.0, 256.0];
pub(crate) const DEFAULT_SPACING: f64 = 5.0;
/// Most plates one layout opens before the rest is reported as overflow.
const MAX_PLATES: usize = 32;
/// Most objects one layout places.
const MAX_LAYOUT_OBJECTS: usize = 256;

/// The bed a layout packs onto.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Bed {
    pub id: Option<String>,
    pub label: String,
    pub size: [f64; 3],
}

impl Bed {
    fn public(&self) -> Value {
        json!({"id": self.id, "name": self.label, "size": self.size})
    }
}

/// The bed named by `plate` in a request (a plate ID, or `[x, y]` /
/// `[x, y, z]` mm), else the design's stored plate, else a 256 mm cube.
pub(crate) fn bed_for(requested: Option<&Value>, selected: Option<&str>) -> Result<Bed, String> {
    let by_id = |id: &str| {
        PRINTER_PLATES.iter().find(|plate| plate.id == id).map(|plate| Bed {
            id: Some(plate.id.to_string()),
            label: format!("{} {}", plate.brand, plate.name),
            size: plate.size,
        })
    };
    match requested {
        None | Some(Value::Null) => {}
        Some(Value::String(id)) => {
            return by_id(id).ok_or_else(|| {
                format!(
                    "Unknown plate {id:?}. Known plates: {}; or pass [x, y] or [x, y, z] in mm.",
                    PRINTER_PLATES.iter().map(|plate| plate.id).collect::<Vec<_>>().join(", ")
                )
            })
        }
        Some(Value::Array(values)) => {
            let numbers: Vec<f64> = values.iter().filter_map(Value::as_f64).collect();
            if !(2..=3).contains(&values.len())
                || numbers.len() != values.len()
                || numbers.iter().any(|value| !(value.is_finite() && *value > 0.0 && *value <= 10_000.0))
            {
                return Err("plate must be a plate ID or [x, y] / [x, y, z] in mm (each 0 to 10000).".into());
            }
            let z = numbers.get(2).copied().unwrap_or(10_000.0);
            return Ok(Bed { id: None, label: "custom".into(), size: [numbers[0], numbers[1], z] });
        }
        Some(_) => return Err("plate must be a plate ID or [x, y] / [x, y, z] in mm.".into()),
    }
    if let Some(bed) = selected.filter(|id| !id.is_empty()).and_then(by_id) {
        return Ok(bed);
    }
    Ok(Bed { id: None, label: "default 256 mm".into(), size: DEFAULT_BED })
}

// ---------------------------------------------------------------------------
// Directives
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Orientation {
    /// OpenSCAD `rotate([x, y, z])`, degrees.
    Rotate([f64; 3]),
    /// Outward normal of the face that goes down.
    Face([f64; 3]),
}

impl Orientation {
    pub fn rotation(&self) -> Mat3 {
        match *self {
            Self::Rotate(angles) => rotation_from_euler(angles),
            Self::Face(normal) => rotation_between(normal, [0.0, 0.0, -1.0]),
        }
    }

    fn directive_value(&self) -> String {
        match *self {
            Self::Rotate(angles) => format!("rotate={}", vector_text(angles)),
            Self::Face(normal) => match axis_name(normal) {
                Some(name) => format!("face={name}"),
                None => format!("face={}", vector_text(normal)),
            },
        }
    }

    fn public(&self) -> Value {
        match *self {
            Self::Rotate(angles) => json!({"rotate": angles}),
            Self::Face(normal) => json!({"face": axis_name(normal).map_or_else(|| json!(normal), |name| json!(name))}),
        }
    }
}

/// One `// @print` line of the source.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PrintDirective {
    /// 1-based line.
    pub line: usize,
    /// Byte range of the whole line, its newline included.
    pub start: usize,
    pub end: usize,
    pub object: String,
    pub orientation: Orientation,
}

const DIRECTIVE: &str = "@print";

/// Every `// @print` directive of `source` (outside block comments and
/// strings is not checked: a directive is a whole line that is a `//`
/// comment starting with `@print`), and a message per malformed one.
pub(crate) fn parse_directives(source: &str) -> (Vec<PrintDirective>, Vec<Value>) {
    let mut directives = Vec::new();
    let mut issues = Vec::new();
    let mut offset = 0usize;
    let in_code = lines_starting_in_code(source);
    for (number, line) in source.split_inclusive('\n').enumerate() {
        let start = offset;
        offset += line.len();
        // A `// @print` inside a string literal or a block comment is text.
        if !in_code.get(number).copied().unwrap_or(false) {
            continue;
        }
        let Some(body) = line.trim().strip_prefix("//").map(str::trim) else {
            continue;
        };
        let Some(rest) = body.strip_prefix(DIRECTIVE) else {
            continue;
        };
        if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
            continue;
        }
        match parse_directive_body(rest.trim()) {
            Ok((object, orientation)) => directives.push(PrintDirective {
                line: number + 1,
                start,
                end: offset,
                object,
                orientation,
            }),
            Err(error) => issues.push(json!({"line": number + 1, "error": error})),
        }
    }
    (directives, issues)
}

/// Per line of `source`: whether it starts as code, outside any string
/// literal or `/* */` comment (OpenSCAD strings may span lines).
fn lines_starting_in_code(source: &str) -> Vec<bool> {
    #[derive(PartialEq)]
    enum State {
        Code,
        Text,
        Block,
        Line,
    }
    let mut state = State::Code;
    let mut starts = vec![true];
    let bytes = source.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        let next = bytes.get(i + 1).copied();
        match state {
            State::Code => match (byte, next) {
                (b'"', _) => state = State::Text,
                (b'/', Some(b'*')) => {
                    state = State::Block;
                    i += 1;
                }
                (b'/', Some(b'/')) => state = State::Line,
                _ => {}
            },
            State::Text => match byte {
                b'\\' => i += 1,
                b'"' => state = State::Code,
                _ => {}
            },
            State::Block => {
                if byte == b'*' && next == Some(b'/') {
                    state = State::Code;
                    i += 1;
                }
            }
            State::Line => {}
        }
        // `bytes[i]`, not `byte`: an escape may have stepped onto a newline.
        if bytes.get(i) == Some(&b'\n') {
            if state == State::Line {
                state = State::Code;
            }
            starts.push(state == State::Code);
        }
        i += 1;
    }
    starts
}

fn parse_directive_body(text: &str) -> Result<(String, Orientation), String> {
    let usage = "expected `// @print <object> rotate=[x, y, z]` or `// @print <object> face=-z|+x|...|[nx, ny, nz]`";
    let (object, rest) = if let Some(quoted) = text.strip_prefix('"') {
        let close = quoted.find('"').ok_or_else(|| format!("unclosed quote; {usage}"))?;
        (quoted[..close].to_string(), &quoted[close + 1..])
    } else {
        let split = text.find(char::is_whitespace).unwrap_or(text.len());
        (text[..split].to_string(), &text[split..])
    };
    if object.is_empty() {
        return Err(format!("no object; {usage}"));
    }
    let rest = rest.trim();
    let (key, value) = rest.split_once('=').ok_or_else(|| format!("no orientation; {usage}"))?;
    let value = value.trim();
    let orientation = match key.trim() {
        "rotate" => Orientation::Rotate(parse_vector(value).ok_or_else(|| format!("rotate needs [x, y, z] degrees; {usage}"))?),
        "face" => {
            let normal = match parse_axis(value) {
                Some(normal) => normal,
                None => parse_vector(value).ok_or_else(|| format!("face needs -z, +x, ... or [nx, ny, nz]; {usage}"))?,
            };
            Orientation::Face(unit(normal).ok_or("face normal must not be zero")?)
        }
        other => return Err(format!("unknown key {other:?}; {usage}")),
    };
    Ok((object, orientation))
}

fn parse_vector(text: &str) -> Option<[f64; 3]> {
    let inner = text.trim().strip_prefix('[')?.strip_suffix(']')?;
    let values: Vec<f64> = inner
        .split(',')
        .map(|part| part.trim().parse::<f64>().ok().filter(|value| value.is_finite()))
        .collect::<Option<Vec<_>>>()?;
    (values.len() == 3).then(|| [values[0], values[1], values[2]])
}

fn parse_axis(text: &str) -> Option<[f64; 3]> {
    let text = text.trim().to_ascii_lowercase();
    let (sign, axis) = match text.as_bytes() {
        [b'-', axis] => (-1.0, *axis),
        [b'+', axis] | [axis] => (1.0, *axis),
        _ => return None,
    };
    let mut normal = [0.0; 3];
    normal[match axis {
        b'x' => 0,
        b'y' => 1,
        b'z' => 2,
        _ => return None,
    }] = sign;
    Some(normal)
}

fn axis_name(normal: [f64; 3]) -> Option<&'static str> {
    const NAMES: [(&str, [f64; 3]); 6] = [
        ("+x", [1.0, 0.0, 0.0]),
        ("-x", [-1.0, 0.0, 0.0]),
        ("+y", [0.0, 1.0, 0.0]),
        ("-y", [0.0, -1.0, 0.0]),
        ("+z", [0.0, 0.0, 1.0]),
        ("-z", [0.0, 0.0, -1.0]),
    ];
    NAMES
        .iter()
        .find(|(_, axis)| dot3(*axis, normal) > 1.0 - 1e-9)
        .map(|(name, _)| *name)
}

fn number_text(value: f64) -> String {
    let rounded = (value * 1e4).round() / 1e4;
    let rounded = if rounded == 0.0 { 0.0 } else { rounded };
    let text = format!("{rounded:.4}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn vector_text(values: [f64; 3]) -> String {
    format!("[{}, {}, {}]", number_text(values[0]), number_text(values[1]), number_text(values[2]))
}

/// The directive line for `object` (an ID, or a name, quoted when needed).
/// Control characters (a newline would end the comment and inject code) are
/// dropped and double quotes become single ones.
pub(crate) fn directive_line(object: &str, orientation: &Orientation) -> String {
    let object: String = object.chars().filter(|c| !c.is_control()).map(|c| if c == '"' { '\'' } else { c }).collect();
    let object = if object.chars().any(char::is_whitespace) || object.is_empty() {
        format!("\"{object}\"")
    } else {
        object
    };
    format!("// {DIRECTIVE} {object} {}\n", orientation.directive_value())
}

/// Where a new directive goes: after the last existing one, else after the
/// file's leading comment block (the requirements header) when a blank line
/// ends it, else at the top. Never between a comment and the statement it
/// annotates (`// @name: Lid` must stay directly above its statement).
fn insertion_point(source: &str, directives: &[PrintDirective]) -> (usize, &'static str) {
    if let Some(last) = directives.iter().max_by_key(|directive| directive.end) {
        return (last.end, if source[..last.end].ends_with('\n') { "" } else { "\n" });
    }
    let mut offset = 0usize;
    let mut header_end = None;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            header_end = Some(offset + line.len());
            offset += line.len();
            continue;
        }
        if trimmed.is_empty() && header_end.is_none() {
            offset += line.len();
            continue;
        }
        // A blank line closes the header; code right after it means the
        // comments annotate that code.
        if !trimmed.is_empty() {
            header_end = None;
        }
        break;
    }
    match header_end {
        Some(end) => (end, if source[..end].ends_with('\n') { "" } else { "\n" }),
        None => (0, ""),
    }
}

/// The edit that sets (or with `None`, removes) `object`'s directive:
/// `(start, end, text)`. `aliases` are every way the object may already be
/// referred to (its ID and name); every directive naming one of them is
/// replaced (the first) or removed (the rest), so none stays in effect. The
/// edit spans from the first such line to the last, keeping the lines
/// between them that are not this object's.
pub(crate) fn directive_edit(
    source: &str,
    aliases: &[&str],
    write_as: &str,
    orientation: Option<&Orientation>,
) -> Option<(usize, usize, String)> {
    let (directives, _) = parse_directives(source);
    let existing: Vec<&PrintDirective> = directives
        .iter()
        .filter(|directive| aliases.iter().any(|alias| refers_to(&directive.object, alias)))
        .collect();
    match (existing.first(), existing.last(), orientation) {
        (Some(first), Some(last), orientation) => {
            let mut text = String::new();
            let mut cursor = first.start;
            for (position, directive) in existing.iter().enumerate() {
                text.push_str(&source[cursor..directive.start]);
                if position == 0 {
                    if let Some(orientation) = orientation {
                        let mut line = directive_line(write_as, orientation);
                        if !source[..directive.end].ends_with('\n') {
                            line.pop();
                        }
                        text.push_str(&line);
                    }
                }
                cursor = directive.end;
            }
            Some((first.start, last.end, text))
        }
        (_, _, Some(orientation)) => {
            let (at, prefix) = insertion_point(source, &directives);
            Some((at, at, format!("{prefix}{}", directive_line(write_as, orientation))))
        }
        _ => None,
    }
}

fn refers_to(reference: &str, alias: &str) -> bool {
    reference == alias || reference.eq_ignore_ascii_case(alias)
}

/// The directive that applies to a part: the first naming its ID, else its
/// name.
fn directive_for<'a>(directives: &'a [PrintDirective], id: &str, name: &str) -> Option<&'a PrintDirective> {
    directives
        .iter()
        .find(|directive| directive.object == id)
        .or_else(|| directives.iter().find(|directive| refers_to(&directive.object, name)))
}

// ---------------------------------------------------------------------------
// Rotations (row-major 3x3, applied as R * p)
// ---------------------------------------------------------------------------

pub(crate) type Mat3 = [[f64; 3]; 3];

const IDENTITY: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

fn unit(v: [f64; 3]) -> Option<[f64; 3]> {
    let length = dot3(v, v).sqrt();
    (length > 1e-12 && length.is_finite()).then(|| [v[0] / length, v[1] / length, v[2] / length])
}

fn mat_mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut out = [[0.0; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    out
}

pub(crate) fn apply(m: &Mat3, p: [f64; 3]) -> [f64; 3] {
    [dot3(m[0], p), dot3(m[1], p), dot3(m[2], p)]
}

/// Sine and cosine, exact at multiples of 90 degrees (so a quarter turn
/// leaves no 6e-17 residue in coordinates or in the directive).
fn sin_cos(degrees: f64) -> (f64, f64) {
    let quarter = degrees / 90.0;
    if (quarter - quarter.round()).abs() < 1e-12 {
        match (quarter.round() as i64).rem_euclid(4) {
            0 => (0.0, 1.0),
            1 => (1.0, 0.0),
            2 => (0.0, -1.0),
            _ => (-1.0, 0.0),
        }
    } else {
        degrees.to_radians().sin_cos()
    }
}

/// OpenSCAD `rotate([x, y, z])`: Rz * Ry * Rx.
pub(crate) fn rotation_from_euler(angles: [f64; 3]) -> Mat3 {
    let (sx, cx) = sin_cos(angles[0]);
    let (sy, cy) = sin_cos(angles[1]);
    let (sz, cz) = sin_cos(angles[2]);
    let rx = [[1.0, 0.0, 0.0], [0.0, cx, -sx], [0.0, sx, cx]];
    let ry = [[cy, 0.0, sy], [0.0, 1.0, 0.0], [-sy, 0.0, cy]];
    let rz = [[cz, -sz, 0.0], [sz, cz, 0.0], [0.0, 0.0, 1.0]];
    mat_mul(&rz, &mat_mul(&ry, &rx))
}

/// The `rotate([x, y, z])` angles of a rotation matrix, snapped to whole
/// multiples of 90 degrees within a micro-degree.
pub(crate) fn euler_from_rotation(r: &Mat3) -> [f64; 3] {
    let sy = (-r[2][0]).clamp(-1.0, 1.0);
    let y = sy.asin();
    let (x, z) = if y.cos().abs() > 1e-9 {
        (r[2][1].atan2(r[2][2]), r[1][0].atan2(r[0][0]))
    } else {
        (0.0, (-r[0][1]).atan2(r[1][1]))
    };
    [x, y, z].map(|angle| {
        let degrees = angle.to_degrees();
        let snapped = (degrees / 90.0).round() * 90.0;
        let degrees = if (degrees - snapped).abs() < 1e-6 { snapped } else { degrees };
        let degrees = if degrees <= -180.0 + 1e-9 { degrees + 360.0 } else { degrees };
        if degrees == 0.0 { 0.0 } else { degrees }
    })
}

/// The shortest rotation taking unit vector `from` onto `to`.
fn rotation_between(from: [f64; 3], to: [f64; 3]) -> Mat3 {
    let (Some(a), Some(b)) = (unit(from), unit(to)) else {
        return IDENTITY;
    };
    let cosine = dot3(a, b).clamp(-1.0, 1.0);
    if cosine > 1.0 - 1e-12 {
        return IDENTITY;
    }
    let axis = if cosine < -1.0 + 1e-12 {
        // Opposite: a half turn about any perpendicular (X when possible, so
        // "+z down" is the familiar rotate([180, 0, 0])).
        let candidate = if a[0].abs() < 0.9 { [1.0, 0.0, 0.0] } else { [0.0, 1.0, 0.0] };
        let projected = cross3(cross3(a, candidate), a);
        unit(projected).unwrap_or([1.0, 0.0, 0.0])
    } else {
        unit(cross3(a, b)).unwrap_or([1.0, 0.0, 0.0])
    };
    axis_angle(axis, cosine.acos())
}

fn axis_angle(axis: [f64; 3], angle: f64) -> Mat3 {
    let (s, c) = if (angle - std::f64::consts::PI).abs() < 1e-12 { (0.0, -1.0) } else { angle.sin_cos() };
    let t = 1.0 - c;
    let [x, y, z] = axis;
    [
        [t * x * x + c, t * x * y - s * z, t * x * z + s * y],
        [t * x * y + s * z, t * y * y + c, t * y * z - s * x],
        [t * x * z - s * y, t * y * z + s * x, t * z * z + c],
    ]
}

const QUARTER_TURN: Mat3 = [[0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]];

// ---------------------------------------------------------------------------
// Poses and packing
// ---------------------------------------------------------------------------

/// An object turned into its print orientation.
#[derive(Clone, Debug)]
pub(crate) struct Pose {
    pub id: String,
    pub name: String,
    pub orientation: Option<Orientation>,
    pub directive_line: Option<usize>,
    pub rotation: Mat3,
    /// Bounds after the rotation (before dropping onto the bed).
    pub min: [f64; 3],
    pub max: [f64; 3],
    /// Downward-facing area lying on the bed plane once dropped (mm^2).
    pub contact_area: f64,
}

impl Pose {
    pub fn size(&self) -> [f64; 3] {
        [0, 1, 2].map(|axis| self.max[axis] - self.min[axis])
    }
}

fn pose_mesh(id: &str, name: &str, mesh: &engine::Mesh, directive: Option<&PrintDirective>) -> Pose {
    let orientation = directive.map(|directive| directive.orientation);
    let rotation = orientation.map_or(IDENTITY, |orientation| orientation.rotation());
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for triangle in &mesh.triangles {
        for vertex in &triangle.vertices {
            let p = apply(&rotation, [vertex.x, vertex.y, vertex.z]);
            for axis in 0..3 {
                min[axis] = min[axis].min(p[axis]);
                max[axis] = max[axis].max(p[axis]);
            }
        }
    }
    if mesh.triangles.is_empty() {
        min = [0.0; 3];
        max = [0.0; 3];
    }
    // Contact: facing down within 5 degrees, every corner within a micron
    // (or a millionth of the part) of the lowest point.
    let span = (0..3).map(|axis| max[axis] - min[axis]).fold(0.0, f64::max);
    let epsilon = (span * 1e-6).max(1e-3);
    let facing = 5f64.to_radians().cos();
    let mut contact_area = 0.0;
    for triangle in &mesh.triangles {
        let corners = triangle.vertices.map(|vertex| apply(&rotation, [vertex.x, vertex.y, vertex.z]));
        let normal = cross3(
            [corners[1][0] - corners[0][0], corners[1][1] - corners[0][1], corners[1][2] - corners[0][2]],
            [corners[2][0] - corners[0][0], corners[2][1] - corners[0][1], corners[2][2] - corners[0][2]],
        );
        let doubled = dot3(normal, normal).sqrt();
        if doubled > 0.0
            && normal[2] / doubled < -facing
            && corners.iter().all(|corner| corner[2] <= min[2] + epsilon)
        {
            contact_area += doubled * 0.5;
        }
    }
    Pose {
        id: id.to_string(),
        name: name.to_string(),
        orientation,
        directive_line: directive.map(|directive| directive.line),
        rotation,
        min,
        max,
        contact_area,
    }
}

/// Poses every part by the source's directives.
pub(crate) fn poses<'a>(
    source: &str,
    parts: impl IntoIterator<Item = (&'a str, &'a str, &'a engine::Mesh)>,
) -> Vec<Pose> {
    let (directives, _) = parse_directives(source);
    parts
        .into_iter()
        .map(|(id, name, mesh)| pose_mesh(id, name, mesh, directive_for(&directives, id, name)))
        .collect()
}

/// Where a pose sits on a plate.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Placement {
    pub index: usize,
    /// 1-based plate number.
    pub plate: usize,
    /// Quarter turn about Z added by the packer.
    pub turned: bool,
    /// Footprint's front-left corner on the plate (plate origin at its
    /// front-left corner).
    pub corner: [f64; 2],
    pub footprint: [f64; 2],
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Overflow {
    pub index: usize,
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Packing {
    pub placements: Vec<Placement>,
    pub overflow: Vec<Overflow>,
    pub plates: usize,
}

/// Shelf packing of footprints `[width, depth]` on a `bed` (`[x, y]`),
/// `spacing` apart: first fit by decreasing depth, a footprint turned a
/// quarter when that is the only way it fits or makes it wider than deep,
/// further plates when one is full. Each plate's arrangement is centred.
pub(crate) fn pack(footprints: &[[f64; 2]], heights: &[f64], bed: [f64; 3], spacing: f64) -> Packing {
    const SLACK: f64 = 1e-6;
    let (width, depth) = (bed[0] + spacing, bed[1] + spacing);
    let mut packing = Packing::default();
    let mut items: Vec<(usize, [f64; 2], bool)> = Vec::new();
    for (index, footprint) in footprints.iter().enumerate() {
        if heights.get(index).is_some_and(|height| *height > bed[2] + SLACK) {
            packing.overflow.push(Overflow {
                index,
                reason: format!("{:.1} mm tall; the build height is {:.0} mm", heights[index], bed[2]),
            });
            continue;
        }
        let fits = |w: f64, d: f64| w <= bed[0] + SLACK && d <= bed[1] + SLACK;
        let [w, d] = *footprint;
        let choice = match (fits(w, d), fits(d, w)) {
            (true, true) => Some(if d > w && w <= bed[1] + SLACK && d <= bed[0] + SLACK { ([d, w], true) } else { ([w, d], false) }),
            (true, false) => Some(([w, d], false)),
            (false, true) => Some(([d, w], true)),
            (false, false) => None,
        };
        match choice {
            Some((size, turned)) => items.push((index, size, turned)),
            None => packing.overflow.push(Overflow {
                index,
                reason: format!("footprint {w:.1} x {d:.1} mm does not fit the {:.0} x {:.0} mm plate in either direction", bed[0], bed[1]),
            }),
        }
    }
    items.sort_by(|a, b| b.1[1].total_cmp(&a.1[1]).then(b.1[0].total_cmp(&a.1[0])).then(a.0.cmp(&b.0)));
    // Shelves: (plate, y, height, x used).
    let mut shelves: Vec<(usize, f64, f64, f64)> = Vec::new();
    let mut used_depth: Vec<f64> = Vec::new();
    for (index, [w, d], turned) in items {
        let (w2, d2) = (w + spacing, d + spacing);
        let shelf = shelves
            .iter_mut()
            .find(|shelf| shelf.3 + w2 <= width + SLACK && d2 <= shelf.2 + SLACK);
        let (plate, x, y) = match shelf {
            Some(shelf) => {
                let x = shelf.3;
                shelf.3 += w2;
                (shelf.0, x, shelf.1)
            }
            None => {
                let plate = match used_depth.iter().position(|used| used + d2 <= depth + SLACK) {
                    Some(plate) => plate,
                    None if used_depth.len() < MAX_PLATES => {
                        used_depth.push(0.0);
                        used_depth.len() - 1
                    }
                    None => {
                        packing.overflow.push(Overflow { index, reason: format!("more than {MAX_PLATES} plates") });
                        continue;
                    }
                };
                let y = used_depth[plate];
                used_depth[plate] += d2;
                shelves.push((plate, y, d2, w2));
                (plate, 0.0, y)
            }
        };
        packing.placements.push(Placement { index, plate: plate + 1, turned, corner: [x, y], footprint: [w, d] });
    }
    packing.plates = used_depth.len();
    // Centre each plate's arrangement.
    for plate in 1..=packing.plates {
        let on: Vec<usize> = (0..packing.placements.len()).filter(|&i| packing.placements[i].plate == plate).collect();
        let mut low = [f64::INFINITY; 2];
        let mut high = [f64::NEG_INFINITY; 2];
        for &i in &on {
            let placement = &packing.placements[i];
            for axis in 0..2 {
                low[axis] = low[axis].min(placement.corner[axis]);
                high[axis] = high[axis].max(placement.corner[axis] + placement.footprint[axis]);
            }
        }
        let shift = [0, 1].map(|axis| (bed[axis] - (high[axis] - low[axis])) / 2.0 - low[axis]);
        for &i in &on {
            for axis in 0..2 {
                packing.placements[i].corner[axis] += shift[axis];
            }
        }
    }
    packing.placements.sort_by_key(|placement| placement.index);
    packing.overflow.sort_by_key(|overflow| overflow.index);
    packing
}

/// A part's full print transform, `p -> rotation * p + translation`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PrintTransform {
    pub rotation: Mat3,
    pub translation: [f64; 3],
}

impl PrintTransform {
    /// The pose alone, dropped onto z = 0 and centred on the origin in X/Y.
    pub fn standalone(pose: &Pose) -> Self {
        Self {
            rotation: pose.rotation,
            translation: [
                -(pose.min[0] + pose.max[0]) / 2.0,
                -(pose.min[1] + pose.max[1]) / 2.0,
                -pose.min[2],
            ],
        }
    }

    /// The pose at its placement on the plate (plate origin at the front-
    /// left corner).
    pub fn placed(pose: &Pose, placement: &Placement) -> Self {
        let rotation = if placement.turned { mat_mul(&QUARTER_TURN, &pose.rotation) } else { pose.rotation };
        // The quarter turn maps the box [min, max] to x in [-max.y, -min.y],
        // y in [min.x, max.x].
        let (low_x, low_y) = if placement.turned { (-pose.max[1], pose.min[0]) } else { (pose.min[0], pose.min[1]) };
        Self {
            rotation,
            translation: [placement.corner[0] - low_x, placement.corner[1] - low_y, -pose.min[2]],
        }
    }

    pub fn apply_mesh(&self, mesh: &mut engine::Mesh) {
        for triangle in &mut mesh.triangles {
            for vertex in &mut triangle.vertices {
                let p = apply(&self.rotation, [vertex.x, vertex.y, vertex.z]);
                vertex.x = p[0] + self.translation[0];
                vertex.y = p[1] + self.translation[1];
                vertex.z = p[2] + self.translation[2];
            }
            let n = apply(&self.rotation, [triangle.normal.x, triangle.normal.y, triangle.normal.z]);
            triangle.normal.x = n[0];
            triangle.normal.y = n[1];
            triangle.normal.z = n[2];
        }
    }

    /// Row-major 4x4 OpenSCAD `multmatrix`.
    pub fn matrix(&self) -> [[f64; 4]; 4] {
        let r = &self.rotation;
        let t = &self.translation;
        [
            [r[0][0], r[0][1], r[0][2], t[0]],
            [r[1][0], r[1][1], r[1][2], t[1]],
            [r[2][0], r[2][1], r[2][2], t[2]],
            [0.0, 0.0, 0.0, 1.0],
        ]
    }
}

fn round4(value: f64) -> f64 {
    let rounded = (value * 1e4).round() / 1e4;
    if rounded == 0.0 { 0.0 } else { rounded }
}

fn round_vec<const N: usize>(values: [f64; N]) -> [f64; N] {
    values.map(round4)
}

fn transform_public(transform: &PrintTransform) -> Value {
    json!({
        "rotate": round_vec(euler_from_rotation(&transform.rotation)),
        "translate": round_vec(transform.translation),
        "matrix": transform.matrix().map(round_vec),
    })
}

/// The layout of `poses` on `bed`, as JSON, and the packing.
pub(crate) fn layout_public(poses: &[Pose], bed: &Bed, spacing: f64) -> (Value, Packing) {
    let footprints: Vec<[f64; 2]> = poses.iter().map(|pose| [pose.size()[0], pose.size()[1]]).collect();
    let heights: Vec<f64> = poses.iter().map(|pose| pose.size()[2]).collect();
    let packing = pack(&footprints, &heights, bed.size, spacing);
    let objects: Vec<Value> = poses
        .iter()
        .enumerate()
        .map(|(index, pose)| {
            let placement = packing.placements.iter().find(|placement| placement.index == index);
            let overflow = packing.overflow.iter().find(|overflow| overflow.index == index);
            json!({
                "id": pose.id,
                "name": pose.name,
                "orientation": pose.orientation.map_or_else(|| json!("as modelled"), |orientation| orientation.public()),
                "directiveLine": pose.directive_line,
                "rotate": round_vec(euler_from_rotation(&pose.rotation)),
                "size": round_vec(pose.size()),
                "contactArea": round4(pose.contact_area),
                "standalone": transform_public(&PrintTransform::standalone(pose)),
                "placement": placement.map(|placement| {
                    let transform = PrintTransform::placed(pose, placement);
                    json!({
                        "plate": placement.plate,
                        "turnedQuarter": placement.turned,
                        "corner": round_vec(placement.corner),
                        "footprint": round_vec(placement.footprint),
                        "center": round_vec([placement.corner[0] + placement.footprint[0] / 2.0, placement.corner[1] + placement.footprint[1] / 2.0]),
                        "transform": transform_public(&transform),
                    })
                }),
                "overflow": overflow.map(|overflow| overflow.reason.clone()),
            })
        })
        .collect();
    (
        json!({
            "plate": bed.public(),
            "spacing": spacing,
            "plateOrigin": "front-left corner of the plate at [0, 0], Z up; transform maps model coordinates onto it (p -> R p + t, R = rotate([x, y, z]))",
            "platesNeeded": packing.plates,
            "fits": packing.overflow.is_empty() && packing.plates <= 1,
            "objects": objects,
            "overflow": packing.overflow.iter().map(|overflow| json!({
                "id": poses[overflow.index].id,
                "reason": overflow.reason,
            })).collect::<Vec<_>>(),
        }),
        packing,
    )
}

// ---------------------------------------------------------------------------
// Tool bodies
// ---------------------------------------------------------------------------

fn api_error(status: u16, message: impl Into<String>) -> ApiError {
    ApiError { status, message: message.into() }
}

fn spacing_input(input: &Value) -> Result<f64, String> {
    match input.get("spacing") {
        None | Some(Value::Null) => Ok(DEFAULT_SPACING),
        Some(value) => value
            .as_f64()
            .filter(|spacing| (0.0..=100.0).contains(spacing))
            .ok_or_else(|| "spacing must be a number from 0 to 100 (mm).".into()),
    }
}

/// The parts a print request covers: `objectIds` when given, else every
/// part that exports (background `%` parts never print).
fn print_parts<'a>(
    parts: &'a [engine::CompiledPart],
    input: &Value,
) -> Result<Vec<&'a engine::CompiledPart>, String> {
    let printable = |part: &&engine::CompiledPart| !part.background && !part.mesh.triangles.is_empty();
    let selected: Vec<&engine::CompiledPart> = match input.get("objectIds") {
        None | Some(Value::Null) => parts.iter().filter(printable).collect(),
        Some(Value::Array(ids)) => {
            let ids: Vec<&str> = ids.iter().filter_map(Value::as_str).collect();
            let unknown: Vec<&str> = ids
                .iter()
                .copied()
                .filter(|id| !parts.iter().any(|part| part.id == *id))
                .collect();
            if !unknown.is_empty() {
                return Err(format!("Unknown object IDs: {}.", unknown.join(", ")));
            }
            parts.iter().filter(|part| ids.contains(&part.id.as_str())).filter(printable).collect()
        }
        Some(_) => return Err("objectIds must be an array of object IDs.".into()),
    };
    if selected.is_empty() {
        return Err("No printable (non-empty, non-%) objects selected.".into());
    }
    if selected.len() > MAX_LAYOUT_OBJECTS {
        return Err(format!("{} objects exceed the {MAX_LAYOUT_OBJECTS}-object layout limit; pass objectIds.", selected.len()));
    }
    Ok(selected)
}

/// `get_print_layout`: each object's print orientation (from the source's
/// `// @print` directives), its size and bed contact once oriented, and an
/// automatic layout on the plate.
pub(crate) fn layout_value(state: &AppState, id: Option<&str>, input: &Value) -> Result<Value, ApiError> {
    let started = Instant::now();
    let _deadline = csg::bsp::deadline_scope(started + agent::BATCH_BUDGET);
    let source = agent::eval_source(state, id, input)?;
    let spacing = spacing_input(input).map_err(|error| api_error(400, error))?;
    let bed = bed_for(input.get("plate"), source.workspace.as_ref().map(|workspace| workspace.plate.as_str()))
        .map_err(|error| api_error(400, error))?;
    let compiled = compile_for_layout(&source, input)?;
    let parts = print_parts(&compiled.parts, input).map_err(|error| api_error(422, error))?;
    let poses = poses(&source.source, parts.iter().map(|part| (part.id.as_str(), part.name.as_str(), &part.mesh)));
    let (layout, _) = layout_public(&poses, &bed, spacing);
    let mut value = source.header();
    value["ok"] = json!(true);
    merge_into(&mut value, layout);
    let (directives, issues) = parse_directives(&source.base);
    let unmatched: Vec<Value> = directives
        .iter()
        .filter(|directive| {
            !compiled.parts.iter().any(|part| directive.object == part.id || refers_to(&directive.object, &part.name))
        })
        .map(|directive| json!({"line": directive.line, "object": directive.object, "error": "no object with this ID or name"}))
        .collect();
    if !issues.is_empty() || !unmatched.is_empty() {
        value["directiveIssues"] = json!(issues.into_iter().chain(unmatched).collect::<Vec<_>>());
    }
    value["elapsedMs"] = json!(started.elapsed().as_millis() as u64);
    Ok(value)
}

/// Compiles for a layout: `render` (default) is the geometry exports write,
/// so a layout and its downloads agree even where `$preview` changes the
/// model; `preview` matches what the viewer shows after a preview.
fn compile_for_layout(source: &EvalSource, input: &Value) -> Result<Arc<engine::MultipartCompileOutput>, ApiError> {
    match input.get("quality").and_then(Value::as_str) {
        None | Some("render") => compile_parts_guarded(&source.source, engine::Quality::Render).map_err(|error| api_error(422, error)),
        Some("preview") => agent::compile_eval(source),
        Some(_) => Err(api_error(400, "quality must be \"render\" or \"preview\".")),
    }
}

fn merge_into(into: &mut Value, from: Value) {
    if let (Some(into), Value::Object(from)) = (into.as_object_mut(), from) {
        into.extend(from);
    }
}

/// The orientation a `set_print_orientation` call asks for (`None`: clear).
fn requested_orientation(input: &Value, part: &engine::CompiledPart) -> Result<(Option<Orientation>, Value), String> {
    let given: Vec<&str> = ["rotate", "layFlat", "face", "clear"]
        .into_iter()
        .filter(|key| input.get(*key).is_some_and(|value| !value.is_null() && value != &json!(false)))
        .collect();
    if given.len() != 1 {
        return Err("Give exactly one of rotate [x, y, z], layFlat <candidate index from describe_geometry>, face (\"-z\", \"+x\", ... or [nx, ny, nz]: the outward normal that goes down), or clear: true.".into());
    }
    match given[0] {
        "rotate" => {
            let angles = input["rotate"]
                .as_array()
                .filter(|values| values.len() == 3)
                .and_then(|values| {
                    let numbers: Vec<f64> = values.iter().filter_map(Value::as_f64).filter(|value| value.is_finite()).collect();
                    (numbers.len() == 3).then(|| [numbers[0], numbers[1], numbers[2]])
                })
                .ok_or("rotate must be [x, y, z] in degrees (OpenSCAD rotate order).")?;
            Ok((Some(Orientation::Rotate(angles)), json!({"from": "rotate"})))
        }
        "face" => {
            let normal = match &input["face"] {
                Value::String(name) => parse_axis(name).ok_or("face must be -z, +z, -x, +x, -y, +y or [nx, ny, nz].")?,
                Value::Array(values) => {
                    let numbers: Vec<f64> = values.iter().filter_map(Value::as_f64).collect();
                    if numbers.len() != 3 || values.len() != 3 {
                        return Err("face must be [nx, ny, nz].".into());
                    }
                    [numbers[0], numbers[1], numbers[2]]
                }
                _ => return Err("face must be -z, +z, -x, +x, -y, +y or [nx, ny, nz].".into()),
            };
            let normal = unit(normal).ok_or("face normal must not be zero.")?;
            Ok((Some(Orientation::Face(normal)), json!({"from": "face"})))
        }
        "layFlat" => {
            let candidate = input["layFlat"]
                .as_u64()
                .filter(|candidate| *candidate < 32)
                .ok_or("layFlat must be a candidate index (0 = the best) from describe_geometry's layFlat list.")?
                as usize;
            let mesh = crate::meshanalysis::from_engine_mesh(&part.mesh);
            let description = crate::meshanalysis::describe(
                &mesh,
                crate::meshanalysis::DescribeOptions { features: false, max_features: 0, max_lay_flat: (candidate + 1).max(4) },
            );
            let chosen = description.lay_flat.get(candidate).ok_or_else(|| {
                format!("{} has {} lay-flat candidates (0..{}).", part.id, description.lay_flat.len(), description.lay_flat.len().saturating_sub(1))
            })?;
            let rotation: Mat3 = [0, 1, 2].map(|row| [chosen.matrix[row][0], chosen.matrix[row][1], chosen.matrix[row][2]]);
            Ok((
                Some(Orientation::Rotate(round_vec(euler_from_rotation(&rotation)))),
                json!({"from": "layFlat", "candidate": candidate, "normal": chosen.normal, "contactArea": round4(chosen.contact_area), "stable": chosen.stable}),
            ))
        }
        _ => Ok((None, json!({"from": "clear"}))),
    }
}

/// Positional IDs (`object-3`) follow the statement's position; a name
/// (`// @name: Lid`) keeps the directive attached through edits.
fn positional_id(id: &str) -> bool {
    id.strip_prefix("object-").is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()))
}

/// `set_print_orientation`: writes (or clears) one object's `// @print`
/// directive as a new revision.
pub(crate) fn mcp_set_print_orientation(state: &AppState, request: &Request, arguments: &Value) -> Result<Value, String> {
    let id = argument_workspace_id(arguments)?.to_string();
    let expected = arguments
        .get("expectedRevision")
        .and_then(Value::as_u64)
        .ok_or("expectedRevision is required (the revision you last read or wrote).")?;
    let object = arguments
        .get("object")
        .and_then(Value::as_str)
        .filter(|object| !object.trim().is_empty())
        .ok_or("object is required: an object ID or name from list_design_objects.")?;
    let workspace = state.workspaces.get(&id)?.ok_or("Design not found.")?;
    if workspace.revision != expected {
        return Err(format!("Revision conflict: current revision is {}.", workspace.revision));
    }
    let _deadline = csg::bsp::deadline_scope(Instant::now() + agent::BATCH_BUDGET);
    let source = agent::eval_source(state, Some(&id), &json!({})).map_err(|error| error.message)?;
    let compiled = agent::compile_eval(&source).map_err(|error| error.message)?;
    let part = compiled
        .parts
        .iter()
        .find(|part| part.id == object)
        .or_else(|| compiled.parts.iter().find(|part| refers_to(&part.name, object)))
        .ok_or_else(|| {
            format!(
                "No object {object:?}. Objects: {}.",
                compiled.parts.iter().map(|part| format!("{} ({})", part.id, part.name)).collect::<Vec<_>>().join(", ")
            )
        })?;
    let (orientation, how) = requested_orientation(arguments, part)?;
    let aliases = [part.id.as_str(), part.name.as_str()];
    let Some((start, end, text)) = directive_edit(&workspace.code, &aliases, &part.id, orientation.as_ref()) else {
        return Ok(json!({
            "ok": true,
            "unchanged": true,
            "designId": id,
            "revision": workspace.revision,
            "note": format!("{} had no print orientation to clear.", part.id),
        }));
    };
    let mut reply = mcp_apply_update(
        state,
        request,
        &id,
        json!({
            "expectedRevision": expected,
            "start": start,
            "end": end,
            "text": text,
            "source": "ai",
            "revisionName": arguments.get("revisionName").cloned().unwrap_or(Value::Null),
            "returnCode": arguments.get("returnCode").cloned().unwrap_or(Value::Null),
            "objects": arguments.get("objects").cloned().unwrap_or(Value::Null),
        }),
    )?;
    let directive = orientation.as_ref().map(|orientation| PrintDirective {
        line: 0,
        start: 0,
        end: 0,
        object: part.id.clone(),
        orientation: *orientation,
    });
    let pose = pose_mesh(&part.id, &part.name, &part.mesh, directive.as_ref());
    reply["printOrientation"] = json!({
        "object": part.id,
        "name": part.name,
        "directive": orientation.as_ref().map(|orientation| directive_line(&part.id, orientation).trim_end().to_string()),
        "request": how,
        "rotate": round_vec(euler_from_rotation(&pose.rotation)),
        "sizeOnBed": round_vec(pose.size()),
        "contactArea": round4(pose.contact_area),
    });
    if positional_id(&part.id) {
        reply["printOrientation"]["hint"] = json!(format!(
            "{} is a positional ID; name the object (`// @name: ...` above it, or $name) so the directive stays attached when statements move.",
            part.id
        ));
    }
    Ok(reply)
}

/// The export URL of a print-oriented file.
fn print_export_url(
    state: &AppState,
    request: &Request,
    id: &str,
    revision: u64,
    format: &str,
    object_ids: Option<&[&str]>,
    extra: &str,
) -> String {
    let selection = object_ids.map(|ids| format!("&objectIds={}", ids.join(","))).unwrap_or_default();
    format!(
        "{}/api/designs/{id}/export?format={format}&revision={revision}{selection}&print=1{extra}",
        request_base_url(state, request),
    )
}

/// `export_print_parts`: one file per object in its print orientation
/// (dropped onto z = 0, centred on the origin), plus one file per plate of
/// the automatic layout, as download URLs.
pub(crate) fn mcp_export_print_parts(state: &AppState, request: &Request, arguments: &Value) -> Result<Value, String> {
    let id = argument_workspace_id(arguments)?.to_string();
    let workspace = state.workspaces.get(&id)?.ok_or("Design not found.")?;
    let revision = workspace.revision;
    let format = arguments.get("format").and_then(Value::as_str).unwrap_or("stl");
    if !is_supported_export_format(format) {
        return Err(supported_export_formats_sentence());
    }
    let plate_format = arguments.get("plateFormat").and_then(Value::as_str).unwrap_or("3mf");
    if !is_supported_export_format(plate_format) {
        return Err(format!("plateFormat: {}", supported_export_formats_sentence()));
    }
    let spacing = spacing_input(arguments)?;
    let bed = bed_for(arguments.get("plate"), Some(workspace.plate.as_str()))?;
    let _deadline = csg::bsp::deadline_scope(Instant::now() + agent::BATCH_BUDGET);
    let source = agent::eval_source(state, Some(&id), &json!({})).map_err(|error| error.message)?;
    // The geometry the download URLs write.
    let compiled = compile_parts_guarded(&source.source, engine::Quality::Render)?;
    let parts = print_parts(&compiled.parts, arguments)?;
    let poses = poses(&source.source, parts.iter().map(|part| (part.id.as_str(), part.name.as_str(), &part.mesh)));
    let (layout, packing) = layout_public(&poses, &bed, spacing);
    let plate_query = |sheet: usize| {
        let mut query = format!("&layout=1&sheet={sheet}&spacing={spacing}");
        match (&bed.id, arguments.get("plate")) {
            (Some(plate), _) => query.push_str(&format!("&printer={plate}")),
            (None, Some(Value::Array(_))) => query.push_str(&format!("&printer={}x{}x{}", bed.size[0], bed.size[1], bed.size[2])),
            _ => {}
        }
        query
    };
    let files: Vec<Value> = poses
        .iter()
        .map(|pose| {
            json!({
                "id": pose.id,
                "name": pose.name,
                "rotate": round_vec(euler_from_rotation(&pose.rotation)),
                "size": round_vec(pose.size()),
                "url": print_export_url(state, request, &id, revision, format, Some(&[pose.id.as_str()]), ""),
            })
        })
        .collect();
    let include_plates = arguments.get("plates").and_then(Value::as_bool).unwrap_or(true);
    // A plate file lays out the same selection as this call (the layout is
    // recomputed on download), then keeps that plate's objects.
    let selection: Option<Vec<&str>> = arguments
        .get("objectIds")
        .filter(|ids| !ids.is_null())
        .map(|_| poses.iter().map(|pose| pose.id.as_str()).collect());
    let plates: Vec<Value> = if include_plates {
        (1..=packing.plates)
            .map(|sheet| {
                let on: Vec<&str> = packing
                    .placements
                    .iter()
                    .filter(|placement| placement.plate == sheet)
                    .map(|placement| poses[placement.index].id.as_str())
                    .collect();
                json!({
                    "plate": sheet,
                    "objects": on,
                    "url": print_export_url(state, request, &id, revision, plate_format, selection.as_deref(), &plate_query(sheet)),
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(json!({
        "ok": true,
        "designId": id,
        "revision": revision,
        "format": format,
        "files": files,
        "plateFiles": plates,
        "layout": layout,
        "note": "Each file is the object in its print orientation (// @print directive, else as modelled), resting on z = 0 and centred on X/Y = 0; plate files hold one plate of the layout in plate coordinates (front-left corner at the origin). URLs compile the saved revision on download.",
        "designUrl": workspace_url(state, request, &id),
    }))
}

/// Print options of an export request (`print=1`, `layout=1`, `sheet`,
/// `printer`, `spacing`), from the JSON body or the query string.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExportPrint {
    pub layout: bool,
    pub sheet: usize,
    pub printer: Option<Value>,
    pub spacing: f64,
}

pub(crate) fn export_print_options(input: &Value, parameters: &HashMap<String, String>) -> Result<Option<ExportPrint>, String> {
    let flag = |key: &str| -> Result<bool, String> {
        match input.get(key) {
            Some(Value::Bool(value)) => Ok(*value),
            Some(Value::Null) | None => match parameters.get(key).map(String::as_str) {
                None | Some("false") | Some("0") => Ok(false),
                Some("true") | Some("1") => Ok(true),
                Some(_) => Err(format!("{key} must be true or false.")),
            },
            Some(_) => Err(format!("{key} must be a boolean.")),
        }
    };
    let layout = flag("layout")?;
    if !flag("print")? && !layout {
        return Ok(None);
    }
    let sheet = match input.get("sheet").and_then(Value::as_u64).or_else(|| parameters.get("sheet").and_then(|value| value.parse().ok())) {
        Some(sheet) if sheet >= 1 => sheet as usize,
        Some(_) => return Err("sheet must be a plate number from 1.".into()),
        None => 1,
    };
    let printer = match input.get("printer").filter(|value| !value.is_null()).cloned() {
        Some(value) => Some(value),
        None => parameters.get("printer").map(|value| {
            let dims: Vec<f64> = value.split('x').filter_map(|part| part.parse().ok()).collect();
            if (2..=3).contains(&dims.len()) && dims.len() == value.split('x').count() {
                json!(dims)
            } else {
                json!(value)
            }
        }),
    };
    let spacing = match input.get("spacing").and_then(Value::as_f64).or_else(|| parameters.get("spacing").and_then(|value| value.parse().ok())) {
        Some(spacing) if (0.0..=100.0).contains(&spacing) => spacing,
        Some(_) => return Err("spacing must be a number from 0 to 100 (mm).".into()),
        None => DEFAULT_SPACING,
    };
    Ok(Some(ExportPrint { layout, sheet, printer, spacing }))
}

/// Turns export bodies into their print poses: each dropped onto z = 0 and
/// centred, or (with `layout`) placed where the layout puts it on plate
/// `sheet`, keeping only that plate's bodies.
pub(crate) fn apply_print_poses(
    source: &str,
    bodies: &mut Vec<export::ExportBody>,
    options: &ExportPrint,
    selected_plate: &str,
) -> Result<(), String> {
    // The layout tools skip empty parts; so must the file, or an empty
    // footprint shifts the packing away from what get_print_layout reported.
    bodies.retain(|body| !body.mesh.triangles.is_empty());
    let poses = poses(source, bodies.iter().map(|body| (body.id.as_str(), body.name.as_str(), &body.mesh)));
    if !options.layout {
        for (body, pose) in bodies.iter_mut().zip(&poses) {
            PrintTransform::standalone(pose).apply_mesh(&mut body.mesh);
        }
        return Ok(());
    }
    let bed = bed_for(options.printer.as_ref(), Some(selected_plate))?;
    let footprints: Vec<[f64; 2]> = poses.iter().map(|pose| [pose.size()[0], pose.size()[1]]).collect();
    let heights: Vec<f64> = poses.iter().map(|pose| pose.size()[2]).collect();
    let packing = pack(&footprints, &heights, bed.size, options.spacing);
    if options.sheet > packing.plates.max(1) {
        return Err(format!("The layout has {} plate(s); sheet {} does not exist.", packing.plates, options.sheet));
    }
    let mut kept = Vec::new();
    for (index, (mut body, pose)) in std::mem::take(bodies).into_iter().zip(&poses).enumerate() {
        if let Some(placement) = packing
            .placements
            .iter()
            .find(|placement| placement.index == index && placement.plate == options.sheet)
        {
            PrintTransform::placed(pose, placement).apply_mesh(&mut body.mesh);
            kept.push(body);
        }
    }
    if kept.is_empty() {
        return Err("No selected object fits on this plate; see get_print_layout's overflow.".into());
    }
    *bodies = kept;
    Ok(())
}

/// The MCP tools of this module.
pub(crate) fn tool_definitions() -> Vec<Value> {
    let ids = json!({"type": "array", "items": {"type": "string"}});
    let plate = json!({
        "description": "A catalogue ID (e.g. bambu-x1, prusa-mk4s, creality-k1) or [x, y] / [x, y, z] mm. Default: the stored value if there is one, otherwise a 256 mm bed.",
        "oneOf": [{"type": "string"}, {"type": "array", "items": {"type": "number"}, "minItems": 2, "maxItems": 3}]
    });
    vec![
        mcp_tool(
            "set_print_orientation",
            "Record how one object goes on the print bed, as a `// @print <object> rotate=[x, y, z]` (or \
             `face=...`) comment in the source, written as a new revision. Give exactly one of rotate \
             (degrees), layFlat, face or clear: true. The object is then dropped onto z = 0. Name objects \
             (`// @name: Lid`) so the directive survives edits; positional IDs (object-3) follow \
             statement order.",
            json!({
                "type": "object",
                "required": ["designId", "expectedRevision", "revisionName", "object"],
                "properties": {
                    "designId": {"type": "string"},
                    "expectedRevision": crate::expected_revision_schema(),
                    "revisionName": crate::revision_name_schema(),
                    "object": {"type": "string", "description": "Object ID or name (list_design_objects)."},
                    "rotate": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
                    "layFlat": {"type": "integer", "minimum": 0, "description": "describe_geometry layFlat candidate index (0 = largest stable face); that face goes down."},
                    "face": {"description": "Outward normal of the face to put down: -z (as modelled), +z (upside down), +x, -x, +y, -y, or [nx, ny, nz].", "oneOf": [{"type": "string"}, {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3}]},
                    "clear": {"type": "boolean", "description": "Remove the object's directive (prints as modelled)."},
                    "returnCode": crate::return_code_schema(),
                    "objects": crate::objects_mode_schema()
                }
            }),
        ),
        mcp_tool(
            "get_print_layout",
            "Per object: print orientation (`// @print` directive, else as modelled) and size on the bed. \
             Plus an automatic layout on the plate (footprints packed spacing mm apart, a quarter turn \
             when it helps): transform (plate front-left corner at the origin), platesNeeded and overflow \
             (too tall, too large, or beyond 32 plates). Reports malformed directives.",
            json!({
                "type": "object",
                "required": ["designId"],
                "properties": {
                    "designId": {"type": "string"},
                    "objectIds": ids.clone(),
                    "plate": plate.clone(),
                    "spacing": {"type": "number", "minimum": 0, "maximum": 100, "default": DEFAULT_SPACING},
                    "quality": {"type": "string", "enum": ["render", "preview"], "default": "render", "description": "render: what exports write; preview: what a viewer preview shows ($preview true)."}
                }
            }),
        ),
        mcp_tool(
            "export_print_parts",
            "Export every object (or objectIds) in print orientation: one file per object, resting on z = \
             0 and centred on X/Y = 0, plus one file per plate of the automatic layout (objects as \
             separate named parts). Returns the layout and download URLs bound to the current revision; \
             no inline bytes.",
            json!({
                "type": "object",
                "required": ["designId"],
                "properties": {
                    "designId": {"type": "string"},
                    "format": {"type": "string", "enum": EXPORT_FORMATS, "default": "stl"},
                    "plateFormat": {"type": "string", "enum": EXPORT_FORMATS, "default": "3mf"},
                    "plates": {"type": "boolean", "default": true, "description": "Also return per-plate files."},
                    "objectIds": ids,
                    "plate": plate,
                    "spacing": {"type": "number", "minimum": 0, "maximum": 100, "default": DEFAULT_SPACING}
                }
            }),
        ),
    ]
}

/// Runs one of this module's MCP tools.
pub(crate) fn mcp_dispatch(state: &AppState, request: &Request, name: &str, arguments: &Value) -> Result<Value, String> {
    match name {
        "set_print_orientation" => mcp_set_print_orientation(state, request, arguments),
        "get_print_layout" => agent::mcp_result(layout_value(state, None, arguments)),
        "export_print_parts" => mcp_export_print_parts(state, request, arguments),
        _ => Err(format!("Unknown tool {name:?}.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cube_mesh(size: [f64; 3]) -> engine::Mesh {
        let v = |x: f64, y: f64, z: f64| engine::Vec3 { x: x * size[0], y: y * size[1], z: z * size[2] };
        let quads = [
            [(0., 0., 0.), (0., 1., 0.), (1., 1., 0.), (1., 0., 0.)],
            [(0., 0., 1.), (1., 0., 1.), (1., 1., 1.), (0., 1., 1.)],
            [(0., 0., 0.), (1., 0., 0.), (1., 0., 1.), (0., 0., 1.)],
            [(0., 1., 0.), (0., 1., 1.), (1., 1., 1.), (1., 1., 0.)],
            [(0., 0., 0.), (0., 0., 1.), (0., 1., 1.), (0., 1., 0.)],
            [(1., 0., 0.), (1., 1., 0.), (1., 1., 1.), (1., 0., 1.)],
        ];
        let mut triangles = Vec::new();
        for quad in quads {
            let p = quad.map(|(x, y, z)| v(x, y, z));
            for [a, b, c] in [[0, 1, 2], [0, 2, 3]] {
                triangles.push(engine::Triangle { vertices: [p[a], p[b], p[c]], normal: engine::Vec3 { x: 0.0, y: 0.0, z: 0.0 } });
            }
        }
        engine::Mesh { triangles }
    }

    #[test]
    fn directives_parse_and_round_trip() {
        let source = "// Requirements: a lid\n// @print object-lid rotate=[180, 0, 0]\n// @print \"Base plate\" face=+x\n// @print bad rotate=[1,2]\n//@printer not a directive\ncube(1);\n";
        let (directives, issues) = parse_directives(source);
        assert_eq!(directives.len(), 2, "{directives:?}");
        assert_eq!(directives[0].object, "object-lid");
        assert_eq!(directives[0].orientation, Orientation::Rotate([180.0, 0.0, 0.0]));
        assert_eq!(directives[0].line, 2);
        assert_eq!(&source[directives[0].start..directives[0].end], "// @print object-lid rotate=[180, 0, 0]\n");
        assert_eq!(directives[1].object, "Base plate");
        assert_eq!(directives[1].orientation, Orientation::Face([1.0, 0.0, 0.0]));
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0]["line"], json!(4));
        assert_eq!(directive_line("Base plate", &Orientation::Face([1.0, 0.0, 0.0])), "// @print \"Base plate\" face=+x\n");
        assert_eq!(directive_line("lid", &Orientation::Rotate([0.5, -90.0, 0.0])), "// @print lid rotate=[0.5, -90, 0]\n");
        // A name can never break out of the comment.
        assert_eq!(directive_line("a\nb\"c d\r", &Orientation::Face([0.0, 0.0, -1.0])), "// @print \"ab'c d\" face=-z\n");
        // Inside a multi-line string or a block comment it is text, not a
        // directive, and set/clear never rewrite it.
        let quoted = "label = \"x\\\"\n// @print a rotate=[1, 2, 3]\n\";\n/* note\n// @print a face=+x\n*/\n// @print a face=-y\ncube(1);\n";
        let (directives, _) = parse_directives(quoted);
        // A backslash right before a newline inside a string still counts the line.
        assert_eq!(parse_directives("s = \"a\\\n\";\n// @print a face=-z\n").0[0].line, 3);
        assert_eq!(directives.iter().map(|d| d.line).collect::<Vec<_>>(), vec![7], "{directives:?}");
        let (start, end, _) = directive_edit(quoted, &["a"], "a", None).unwrap();
        assert_eq!(&quoted[start..end], "// @print a face=-y\n");
    }

    #[test]
    fn directive_edits_replace_insert_and_clear() {
        let header = "// Requirements: a box\n// Decisions: none\n\ncube(1);\n";
        let (start, end, text) = directive_edit(header, &["object-1", "cube 1"], "object-1", Some(&Orientation::Rotate([90.0, 0.0, 0.0]))).unwrap();
        let edited = format!("{}{}{}", &header[..start], text, &header[end..]);
        assert_eq!(edited, "// Requirements: a box\n// Decisions: none\n// @print object-1 rotate=[90, 0, 0]\n\ncube(1);\n");
        // Replacing keeps one line per object; a second object goes after it.
        let (start, end, text) = directive_edit(&edited, &["object-1"], "object-1", Some(&Orientation::Face([0.0, 0.0, 1.0]))).unwrap();
        let replaced = format!("{}{}{}", &edited[..start], text, &edited[end..]);
        assert!(replaced.contains("// @print object-1 face=+z\n") && !replaced.contains("rotate"), "{replaced}");
        let (start, end, text) = directive_edit(&replaced, &["object-2"], "object-2", Some(&Orientation::Rotate([0.0, 0.0, 45.0]))).unwrap();
        let both = format!("{}{}{}", &replaced[..start], text, &replaced[end..]);
        assert!(both.contains("face=+z\n// @print object-2 rotate=[0, 0, 45]\n"), "{both}");
        // Every line naming the object goes, by ID or by name.
        let both = format!("{both}// @print Lid rotate=[1, 2, 3]\n");
        let (start, end, text) = directive_edit(&both, &["object-1", "lid"], "object-1", None).unwrap();
        let cleared = format!("{}{}{}", &both[..start], text, &both[end..]);
        assert!(!cleared.contains("object-1") && !cleared.contains("Lid") && cleared.contains("object-2"), "{cleared}");
        assert!(directive_edit("cube(1);\n", &["object-1"], "object-1", None).is_none());
        // No header, or comments that annotate the first statement: at the top.
        for source in ["cube(1);\n", "// @name: Lid\ncube(1);\n", "\n// header\ncube(1);\n"] {
            let (start, _, text) = directive_edit(source, &["object-1"], "object-1", Some(&Orientation::Rotate([0.0; 3]))).unwrap();
            assert_eq!((start, text.as_str()), (0, "// @print object-1 rotate=[0, 0, 0]\n"), "{source:?}");
        }
        // A header with no newline at the end of the file.
        let (start, _, text) = directive_edit("// only a header", &["a"], "a", Some(&Orientation::Rotate([0.0; 3]))).unwrap();
        assert_eq!((start, text.as_str()), (16, "\n// @print a rotate=[0, 0, 0]\n"));
    }

    #[test]
    fn euler_round_trips_and_faces_point_down() {
        for angles in [[0.0, 0.0, 0.0], [180.0, 0.0, 0.0], [90.0, 0.0, 0.0], [12.5, -33.0, 140.0], [0.0, 90.0, 30.0]] {
            let r = rotation_from_euler(angles);
            let back = rotation_from_euler(euler_from_rotation(&r));
            for i in 0..3 {
                for j in 0..3 {
                    assert!((r[i][j] - back[i][j]).abs() < 1e-9, "{angles:?}");
                }
            }
        }
        assert_eq!(euler_from_rotation(&rotation_from_euler([180.0, 0.0, 0.0])), [180.0, 0.0, 0.0]);
        for normal in [[0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, -1.0, 0.0], [0.6, 0.0, 0.8], [0.0, 0.0, -1.0]] {
            let down = apply(&Orientation::Face(normal).rotation(), normal);
            assert!((down[2] + 1.0).abs() < 1e-9, "{normal:?} -> {down:?}");
        }
        assert_eq!(Orientation::Face([0.0, 0.0, 1.0]).rotation(), rotation_from_euler([180.0, 0.0, 0.0]));
    }

    #[test]
    fn poses_drop_onto_the_bed_with_contact_area() {
        let mesh = cube_mesh([10.0, 20.0, 2.0]);
        let directive = PrintDirective { line: 1, start: 0, end: 0, object: "a".into(), orientation: Orientation::Face([1.0, 0.0, 0.0]) };
        let pose = pose_mesh("a", "a", &mesh, Some(&directive));
        let size = pose.size();
        assert!((size[2] - 10.0).abs() < 1e-9, "{size:?}");
        assert!((pose.contact_area - 40.0).abs() < 1e-6, "{}", pose.contact_area);
        let mut placed = mesh.clone();
        PrintTransform::standalone(&pose).apply_mesh(&mut placed);
        let min_z = placed.triangles.iter().flat_map(|t| t.vertices).map(|v| v.z).fold(f64::INFINITY, f64::min);
        assert!(min_z.abs() < 1e-9);
        let flat = pose_mesh("a", "a", &mesh, None);
        assert!((flat.contact_area - 200.0).abs() < 1e-6);
    }

    #[test]
    fn packing_fills_plates_turns_and_overflows() {
        let bed = [100.0, 100.0, 50.0];
        // Four 45 x 45 squares fill one plate at 5 mm spacing; a fifth needs
        // a second plate; a 120 x 10 bar fits only turned... it does not fit
        // at all (120 > 100 both ways); a 90 x 20 fits; a tall one overflows.
        let footprints = [[45.0, 45.0]; 5];
        let packing = pack(&footprints, &[10.0; 5], bed, 5.0);
        assert_eq!(packing.plates, 2, "{packing:?}");
        assert_eq!(packing.placements.iter().filter(|p| p.plate == 1).count(), 4);
        let packing = pack(&[[120.0, 10.0], [20.0, 90.0], [10.0, 10.0]], &[1.0, 1.0, 80.0], bed, 5.0);
        assert_eq!(packing.overflow.iter().map(|o| o.index).collect::<Vec<_>>(), vec![0, 2]);
        let bar = &packing.placements[0];
        assert!(bar.turned && bar.footprint == [90.0, 20.0], "{bar:?}");
        // Centred and inside the plate.
        assert!((bar.corner[0] - 5.0).abs() < 1e-9 && (bar.corner[1] - 40.0).abs() < 1e-9, "{bar:?}");
        // Placed transforms land the box exactly on the footprint.
        let mesh = cube_mesh([20.0, 90.0, 1.0]);
        let pose = pose_mesh("bar", "bar", &mesh, None);
        let mut placed = mesh.clone();
        PrintTransform::placed(&pose, bar).apply_mesh(&mut placed);
        let xs = placed.triangles.iter().flat_map(|t| t.vertices).map(|v| v.x);
        let (lo, hi) = xs.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), x| (lo.min(x), hi.max(x)));
        assert!((lo - 5.0).abs() < 1e-9 && (hi - 95.0).abs() < 1e-9, "{lo} {hi}");
    }

    #[test]
    fn beds_resolve_from_ids_sizes_and_the_selection() {
        assert_eq!(bed_for(None, Some("prusa-mk4s")).unwrap().size, [250.0, 210.0, 220.0]);
        assert_eq!(bed_for(Some(&json!([200, 150])), None).unwrap().size[..2], [200.0, 150.0]);
        assert_eq!(bed_for(None, Some("")).unwrap().size, DEFAULT_BED);
        assert!(bed_for(Some(&json!("nope")), None).is_err());
    }

    fn state() -> (AppState, PathBuf) {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let root = std::env::temp_dir().join(format!(
            "reopenscad-print-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let state = AppState {
            web_root: PathBuf::new(),
            render_queue: RenderQueue::new(MAX_CONCURRENT_JOBS, MAX_QUEUED_JOBS, MAX_QUEUED_PER_CLIENT, MAX_QUEUE_WAIT),
            poll_limiter: JobLimiter::new(MAX_CONCURRENT_POLLS),
            rate_limiter: RateLimiter::new(),
            cancellations: Arc::new(Mutex::new(HashMap::new())),
            workspaces: Store::Filesystem(WorkspaceStore::open(root.clone()).unwrap()),
            public_origin: None,
        };
        (state, root)
    }

    fn call(state: &AppState, name: &str, arguments: Value) -> Value {
        let request = Request {
            method: "POST".into(),
            path: "/mcp".into(),
            query: String::new(),
            headers: vec![("host".into(), "127.0.0.1:5173".into())],
            body: vec![],
        };
        let result = crate::mcp_call_tool(state, &request, name, arguments);
        assert_eq!(result["isError"], json!(false), "{name}: {result}");
        result["structuredContent"].clone()
    }

    /// GET `path` through the real request pipeline: status and body.
    fn get(state: &AppState, path: &str) -> (u16, Vec<u8>) {
        let raw = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:5173\r\nContent-Length: 0\r\n\r\n");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(address).unwrap();
            stream.write_all(raw.as_bytes()).unwrap();
            let mut response = Vec::new();
            let _ = stream.read_to_end(&mut response);
            response
        });
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        serve_request(&mut stream, state, &mut None).unwrap();
        drop(stream);
        let response = client.join().unwrap();
        let split = response.windows(4).position(|window| window == b"\r\n\r\n").unwrap();
        let status = String::from_utf8_lossy(&response[9..12]).parse().unwrap();
        (status, response[split + 4..].to_vec())
    }

    /// Bounds of a binary STL.
    fn stl_bounds(data: &[u8]) -> ([f64; 3], [f64; 3]) {
        let count = u32::from_le_bytes(data[80..84].try_into().unwrap()) as usize;
        let mut low = [f64::INFINITY; 3];
        let mut high = [f64::NEG_INFINITY; 3];
        for triangle in 0..count {
            for corner in 0..3 {
                for axis in 0..3 {
                    let at = 84 + triangle * 50 + 12 + corner * 12 + axis * 4;
                    let value = f32::from_le_bytes(data[at..at + 4].try_into().unwrap()) as f64;
                    low[axis] = low[axis].min(value);
                    high[axis] = high[axis].max(value);
                }
            }
        }
        (low, high)
    }

    #[test]
    fn orientation_round_trip_through_the_tools_and_exports() {
        let (state, root) = state();
        let code = "// Requirements: a lid and a base\n// Decisions: none\n\n// @name: Lid\ntranslate([0, 0, 20]) cube([40, 30, 4]);\n// @name: Post\ncube([4, 6, 50]);\n";
        let id = state.workspaces.create(code.into(), None).unwrap().id;
        state.workspaces.update_settings(&id, Some("prusa-mk4s"), None).unwrap();

        // The post lies down on its +x face; the lid goes upside down.
        let set = call(&state, "set_print_orientation", json!({"designId": id, "expectedRevision": 1, "object": "Post", "face": "+x", "revisionName": "Test revision"}));
        assert_eq!(set["revision"], json!(2), "{set}");
        let oriented = &set["printOrientation"];
        assert_eq!(oriented["directive"], json!("// @print object-post face=+x"), "{set}");
        assert_eq!(oriented["sizeOnBed"][2], json!(4.0), "{set}");
        assert_eq!(oriented["contactArea"], json!(300.0), "{set}");
        let set = call(&state, "set_print_orientation", json!({"designId": id, "expectedRevision": 2, "object": "object-lid", "layFlat": 0, "revisionName": "Test revision"}));
        assert_eq!(set["printOrientation"]["sizeOnBed"][2], json!(4.0), "{set}");
        let code = state.workspaces.get(&id).unwrap().unwrap().code;
        assert!(code.starts_with("// Requirements: a lid and a base\n// Decisions: none\n// @print object-post face=+x\n// @print object-lid rotate="), "{code}");
        // Rewriting replaces the line instead of adding one.
        let set = call(&state, "set_print_orientation", json!({"designId": id, "expectedRevision": 3, "object": "Lid", "rotate": [180, 0, 0], "revisionName": "Test revision"}));
        assert_eq!(set["revision"], json!(4));
        let code = state.workspaces.get(&id).unwrap().unwrap().code;
        assert_eq!(code.matches("@print").count(), 2, "{code}");
        assert!(code.contains("// @print object-lid rotate=[180, 0, 0]\n"), "{code}");

        let layout = call(&state, "get_print_layout", json!({"designId": id}));
        assert_eq!(layout["plate"]["id"], json!("prusa-mk4s"), "{layout}");
        assert_eq!(layout["platesNeeded"], json!(1));
        assert_eq!(layout["fits"], json!(true));
        let objects = layout["objects"].as_array().unwrap();
        let post = objects.iter().find(|object| object["id"] == "object-post").unwrap();
        assert_eq!(post["orientation"], json!({"face": "+x"}));
        assert_eq!(post["size"], json!([50.0, 6.0, 4.0]), "{post}");
        let lid = objects.iter().find(|object| object["id"] == "object-lid").unwrap();
        assert_eq!(lid["rotate"], json!([180.0, 0.0, 0.0]));
        // Placements stay on the plate and apart.
        for object in objects {
            let placement = &object["placement"];
            let corner = [placement["corner"][0].as_f64().unwrap(), placement["corner"][1].as_f64().unwrap()];
            let footprint = [placement["footprint"][0].as_f64().unwrap(), placement["footprint"][1].as_f64().unwrap()];
            assert!(corner[0] >= 0.0 && corner[1] >= 0.0 && corner[0] + footprint[0] <= 250.0 && corner[1] + footprint[1] <= 210.0, "{placement}");
        }
        // Too small a plate overflows instead of pretending.
        let tight = call(&state, "get_print_layout", json!({"designId": id, "plate": [30, 30]}));
        assert_eq!(tight["fits"], json!(false));
        assert_eq!(tight["overflow"].as_array().unwrap().len(), 2, "{tight}");

        let exported = call(&state, "export_print_parts", json!({"designId": id}));
        let files = exported["files"].as_array().unwrap();
        assert_eq!(files.len(), 2, "{exported}");
        let post_url = files.iter().find(|file| file["id"] == "object-post").unwrap()["url"].as_str().unwrap().to_string();
        let path = &post_url[post_url.find("/api/").unwrap()..];
        let (status, body) = get(&state, path);
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        let (low, high) = stl_bounds(&body);
        assert!(low[2].abs() < 1e-4 && (high[2] - 4.0).abs() < 1e-4, "{low:?} {high:?}");
        assert!((high[0] - low[0] - 50.0).abs() < 1e-4 && (low[0] + high[0]).abs() < 1e-4, "{low:?} {high:?}");
        let plate_url = exported["plateFiles"][0]["url"].as_str().unwrap().to_string();
        assert!(plate_url.contains("format=3mf") && plate_url.contains("layout=1") && plate_url.contains("printer=prusa-mk4s"), "{plate_url}");
        let (status, body) = get(&state, &plate_url[plate_url.find("/api/").unwrap()..]);
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        assert_eq!(&body[..2], b"PK");
        // The same layout as one STL lands both parts inside the plate.
        let (status, body) = get(&state, &format!("/api/designs/{id}/export?format=stl&revision=4&layout=1"));
        assert_eq!(status, 200);
        let (low, high) = stl_bounds(&body);
        assert!(low.iter().all(|value| *value > -1e-4) && high[0] <= 250.0 && high[1] <= 210.0, "{low:?} {high:?}");
        // The HTTP route answers like the tool.
        let (status, body) = get(&state, &format!("/api/designs/{id}/print-layout?spacing=10"));
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["spacing"], json!(10.0));

        // The layout is of the geometry the downloads write (render), unless
        // the viewer asks for its preview.
        let preview_id = state.workspaces.create("cube($preview ? [10, 10, 10] : [300, 300, 10]);\n".into(), None).unwrap().id;
        state.workspaces.update_settings(&preview_id, Some("prusa-mk4s"), None).unwrap();
        let rendered = call(&state, "get_print_layout", json!({"designId": preview_id}));
        assert_eq!(rendered["fits"], json!(false), "{rendered}");
        let previewed = call(&state, "get_print_layout", json!({"designId": preview_id, "quality": "preview"}));
        assert_eq!(previewed["fits"], json!(true), "{previewed}");
        let exported = call(&state, "export_print_parts", json!({"designId": preview_id}));
        assert_eq!(exported["layout"]["fits"], json!(false), "{exported}");
        assert_eq!(exported["plateFiles"], json!([]), "{exported}");

        // Clearing removes the line; a stale revision is refused.
        let cleared = call(&state, "set_print_orientation", json!({"designId": id, "expectedRevision": 4, "object": "Post", "clear": true, "revisionName": "Test revision"}));
        assert_eq!(cleared["revision"], json!(5));
        assert!(!state.workspaces.get(&id).unwrap().unwrap().code.contains("object-post"));
        let request = Request { method: "POST".into(), path: "/mcp".into(), query: String::new(), headers: vec![], body: vec![] };
        let stale = crate::mcp_call_tool(&state, &request, "set_print_orientation", json!({"designId": id, "expectedRevision": 4, "object": "Post", "face": "-z", "revisionName": "Test revision"}));
        assert_eq!(stale["isError"], json!(true), "{stale}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// The layout routes compile, so they are metered and admitted like the
    /// other evaluator routes; export parameters are bounded.
    #[test]
    fn layout_routes_are_metered_and_export_options_bounded() {
        for (method, path) in [("POST", "/api/designs/happy-otter-0123456789/print-layout"), ("GET", "/api/designs/happy-otter-0123456789/print-layout")] {
            assert_eq!(route_cost(method, path), RouteCost::Evaluator, "{method} {path}");
        }
        let query = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>();
        assert_eq!(export_print_options(&json!({}), &query(&[])).unwrap(), None);
        let options = export_print_options(&json!({}), &query(&[("layout", "1"), ("sheet", "2"), ("printer", "200x150"), ("spacing", "3")])).unwrap().unwrap();
        assert_eq!((options.sheet, options.spacing, options.printer), (2, 3.0, Some(json!([200.0, 150.0]))));
        for bad in [("spacing", "101"), ("spacing", "NaN"), ("sheet", "0"), ("print", "maybe")] {
            assert!(export_print_options(&json!({}), &query(&[("print", "1"), bad])).is_err(), "{bad:?}");
        }
        assert!(bed_for(Some(&json!([0, 100])), None).is_err());
        assert!(bed_for(Some(&json!([1e9, 100])), None).is_err());
    }

}
