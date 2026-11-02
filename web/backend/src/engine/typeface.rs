//! The bundled outline fonts `text()` sets type in, and how a SCAD `font`
//! string picks one.
//!
//! OpenSCAD asks fontconfig for whatever the machine has installed, so the
//! same source renders differently on two machines. A web service cannot do
//! that, so it ships a small, fixed set of faces in the binary — every one
//! under the SIL Open Font License, with its licence file next to it in
//! `engine/fonts/`. The default face is Liberation Sans, which is also
//! OpenSCAD's own default, so a `text()` that names no font renders in the
//! same family it would there.
//!
//! The Liberation and Dancing Script files are the unmodified upstream
//! releases (both carry Reserved Font Names, which the OFL only protects
//! against *modified* versions). Great Vibes carries no Reserved Font Name and
//! is subset to Latin-1 plus common punctuation to keep the binary small.
//!
//! The monoline face in [`super::font`] is still available as
//! `font = "ReOpenSCAD Stroke"`, for engraving and pen plotting where a
//! single-stroke letter is the point.

use super::truetype::{Face, OutlinePoint};
use std::sync::OnceLock;

/// One bundled face.
pub struct FontEntry {
    pub family: &'static str,
    pub style: &'static str,
    pub bold: bool,
    pub italic: bool,
    pub license: &'static str,
    pub description: &'static str,
    data: &'static [u8],
}

const OFL: &str = "SIL Open Font License 1.1";

pub const DEFAULT_FAMILY: &str = "Liberation Sans";
pub const STROKE_FAMILY: &str = "ReOpenSCAD Stroke";

pub static FONTS: &[FontEntry] = &[
    FontEntry {
        family: "Liberation Sans",
        style: "Regular",
        bold: false,
        italic: false,
        license: OFL,
        description:
            "Default. Sans serif, metric-compatible with Arial/Helvetica; OpenSCAD's default face.",
        data: include_bytes!("fonts/LiberationSans-Regular.ttf"),
    },
    FontEntry {
        family: "Liberation Sans",
        style: "Bold",
        bold: true,
        italic: false,
        license: OFL,
        description: "Sans serif, bold.",
        data: include_bytes!("fonts/LiberationSans-Bold.ttf"),
    },
    FontEntry {
        family: "Liberation Sans",
        style: "Italic",
        bold: false,
        italic: true,
        license: OFL,
        description: "Sans serif, italic.",
        data: include_bytes!("fonts/LiberationSans-Italic.ttf"),
    },
    FontEntry {
        family: "Liberation Serif",
        style: "Regular",
        bold: false,
        italic: false,
        license: OFL,
        description: "Serif, metric-compatible with Times New Roman.",
        data: include_bytes!("fonts/LiberationSerif-Regular.ttf"),
    },
    FontEntry {
        family: "Liberation Mono",
        style: "Regular",
        bold: false,
        italic: false,
        license: OFL,
        description: "Monospace, metric-compatible with Courier New.",
        data: include_bytes!("fonts/LiberationMono-Regular.ttf"),
    },
    FontEntry {
        family: "Dancing Script",
        style: "Regular",
        bold: false,
        italic: false,
        license: OFL,
        description: "Casual connected script (handwriting).",
        data: include_bytes!("fonts/DancingScript-Regular.ttf"),
    },
    FontEntry {
        family: "Great Vibes",
        style: "Regular",
        bold: false,
        italic: false,
        license: OFL,
        description: "Formal calligraphic script; Latin-1 only.",
        data: include_bytes!("fonts/GreatVibes-Regular.ttf"),
    },
];

fn faces() -> &'static [Option<Face>] {
    static FACES: OnceLock<Vec<Option<Face>>> = OnceLock::new();
    FACES.get_or_init(|| FONTS.iter().map(|entry| Face::parse(entry.data)).collect())
}

/// The parsed face for a font-table index.
pub fn face(index: usize) -> Option<&'static Face> {
    faces().get(index).and_then(Option::as_ref)
}

/// Which face a `font` argument selected, and what the author should be told.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolution {
    /// Index into [`FONTS`], or `None` for the built-in stroke face.
    pub font: Option<usize>,
    pub warning: Option<String>,
}

/// `"Family:style=Bold"` → family and requested style.
struct Request {
    families: Vec<String>,
    bold: bool,
    italic: bool,
    style_text: Option<String>,
}

fn normalize(name: &str) -> String {
    name.chars()
        .filter(|character| !matches!(character, ' ' | '-' | '_' | '\'' | '"'))
        .flat_map(char::to_lowercase)
        .collect()
}

fn apply_style_words(text: &str, request: &mut Request) {
    let lower = text.to_ascii_lowercase();
    if lower.contains("bold") || lower.contains("black") || lower.contains("heavy") {
        request.bold = true;
    }
    if lower.contains("italic") || lower.contains("oblique") {
        request.italic = true;
    }
}

fn parse_request(spec: &str) -> Request {
    let mut parts = spec.split(':');
    let family_part = parts.next().unwrap_or_default();
    let mut request = Request {
        families: family_part
            .split(',')
            .map(|family| family.trim().to_string())
            .filter(|family| !family.is_empty())
            .collect(),
        bold: false,
        italic: false,
        style_text: None,
    };
    for property in parts {
        let property = property.trim();
        if property.is_empty() {
            continue;
        }
        let (key, value) = match property.split_once('=') {
            Some((key, value)) => (key.trim().to_ascii_lowercase(), value.trim()),
            None => (String::new(), property),
        };
        match key.as_str() {
            "style" | "" | "weight" | "slant" => {
                apply_style_words(value, &mut request);
                if key == "style" {
                    request.style_text = Some(value.to_string());
                }
            }
            _ => {}
        }
    }
    request
}

/// Family aliases. The `bool` says whether the substitution is worth a
/// warning: CSS-style generic names are a request for "some face of this
/// kind" and are satisfied silently; a named commercial face is not here, and
/// the author should know the metric-compatible stand-in was used.
fn alias(normalized: &str) -> Option<(&'static str, bool)> {
    Some(match normalized {
        "sans" | "sansserif" | "default" => ("Liberation Sans", false),
        "serif" => ("Liberation Serif", false),
        "mono" | "monospace" | "monospaced" => ("Liberation Mono", false),
        "cursive" | "script" | "handwriting" => ("Dancing Script", false),
        "calligraphy" | "calligraphic" => ("Great Vibes", false),
        "arial" | "helvetica" | "arimo" | "helveticaneue" => ("Liberation Sans", true),
        "times" | "timesnewroman" | "tinos" => ("Liberation Serif", true),
        "courier" | "couriernew" | "cousine" => ("Liberation Mono", true),
        _ => return None,
    })
}

fn is_stroke(normalized: &str) -> bool {
    matches!(
        normalized,
        "reopenscadstroke" | "stroke" | "reopenscad" | "monoline" | "singlestroke"
    )
}

/// All family names, in table order, without repeats.
pub fn families() -> Vec<&'static str> {
    let mut families: Vec<&'static str> = Vec::new();
    for entry in FONTS {
        if !families.contains(&entry.family) {
            families.push(entry.family);
        }
    }
    families.push(STROKE_FAMILY);
    families
}

fn styles_of(family: &str) -> Vec<usize> {
    (0..FONTS.len())
        .filter(|index| FONTS[*index].family == family)
        .collect()
}

/// Best style of `family` for the request, and whether it is exact.
fn pick_style(family: &str, bold: bool, italic: bool) -> Option<(usize, bool)> {
    let candidates = styles_of(family);
    if let Some(exact) = candidates
        .iter()
        .find(|index| FONTS[**index].bold == bold && FONTS[**index].italic == italic)
    {
        return Some((*exact, true));
    }
    // Keep weight over slant, then fall back to the first (Regular) face.
    let near = candidates
        .iter()
        .find(|index| FONTS[**index].bold == bold)
        .or_else(|| candidates.first())?;
    Some((*near, false))
}

fn family_of(normalized: &str) -> Option<&'static str> {
    FONTS
        .iter()
        .find(|entry| normalize(entry.family) == normalized)
        .map(|entry| entry.family)
}

/// Resolves an OpenSCAD/fontconfig-style font name to a bundled face.
pub fn resolve(spec: &str) -> Resolution {
    let mut request = parse_request(spec);
    if request.families.is_empty() {
        request.families.push(DEFAULT_FAMILY.to_string());
    }
    let mut substitution: Option<String> = None;
    let mut found: Option<&'static str> = None;
    for family in request.families.clone().iter() {
        let normalized = normalize(family);
        if is_stroke(&normalized) {
            return Resolution {
                font: None,
                warning: None,
            };
        }
        if let Some(name) = family_of(&normalized) {
            found = Some(name);
            break;
        }
        if let Some((name, warn)) = alias(&normalized) {
            if warn {
                substitution = Some(format!(
                    "WARNING: text(): font family \"{family}\" is not bundled; using the \
                     metric-compatible \"{name}\" instead."
                ));
            }
            found = Some(name);
            break;
        }
        // "Liberation Sans Bold" written as one family name.
        let words: Vec<&str> = family.split_whitespace().collect();
        for cut in (1..words.len()).rev() {
            let (head, tail) = words.split_at(cut);
            let tail = tail.join(" ");
            let tail_lower = tail.to_ascii_lowercase();
            let all_style = tail_lower.split_whitespace().all(|word| {
                matches!(
                    word,
                    "bold" | "italic" | "oblique" | "regular" | "book" | "normal" | "black"
                )
            });
            if !all_style {
                continue;
            }
            if let Some(name) = family_of(&normalize(&head.join(" "))) {
                apply_style_words(&tail, &mut request);
                found = Some(name);
                break;
            }
        }
        if found.is_some() {
            break;
        }
    }
    let requested_style = describe_style(request.bold, request.italic);
    let Some(family) = found else {
        let (index, _) = pick_style(DEFAULT_FAMILY, request.bold, request.italic)
            .expect("the default family is bundled");
        return Resolution {
            font: Some(index),
            warning: Some(format!(
                "WARNING: text(): font \"{spec}\" is not available; using fallback \
                 \"{}:style={}\". Available fonts: {}.",
                FONTS[index].family,
                FONTS[index].style,
                families().join(", ")
            )),
        };
    };
    let (index, exact) =
        pick_style(family, request.bold, request.italic).expect("family has at least one style");
    let mut warning = substitution;
    if !exact {
        let available: Vec<&str> = styles_of(family)
            .into_iter()
            .map(|index| FONTS[index].style)
            .collect();
        let message = format!(
            "WARNING: text(): style \"{}\" of \"{family}\" is not available; using \"{}\". \
             Available styles: {}.",
            request.style_text.clone().unwrap_or(requested_style),
            FONTS[index].style,
            available.join(", ")
        );
        warning = Some(match warning {
            Some(previous) => format!("{previous}\n{message}"),
            None => message,
        });
    }
    Resolution {
        font: Some(index),
        warning,
    }
}

fn describe_style(bold: bool, italic: bool) -> String {
    match (bold, italic) {
        (false, false) => "Regular",
        (true, false) => "Bold",
        (false, true) => "Italic",
        (true, true) => "Bold Italic",
    }
    .to_string()
}

/// Flattens one TrueType contour (quadratic B-splines with implied on-curve
/// midpoints) into a polyline, closing implicitly.
///
/// `fragments` is a full turn's worth of segments, as for `circle()`: each
/// quadratic piece gets the share of it that its own turning angle is, so a
/// letter `O` made of four quarter curves comes out with about as many sides
/// as a circle at the same `$fn`.
pub fn flatten_contour(points: &[OutlinePoint], fragments: usize) -> Vec<[f64; 2]> {
    let count = points.len();
    if count < 2 {
        return Vec::new();
    }
    // Start on an on-curve point, or on the implied midpoint of two
    // off-curve points if the contour has none.
    let start_index = points.iter().position(|point| point.on_curve);
    let start = match start_index {
        Some(index) => [points[index].x, points[index].y],
        None => midpoint(points[0], points[1]),
    };
    let first = start_index.unwrap_or(0);
    let mut output = vec![start];
    let mut current = start;
    let mut control: Option<[f64; 2]> = None;
    for step in 1..=count {
        let point = points[(first + step) % count];
        // Without an on-curve start, step `count` wraps onto points[0], whose
        // midpoint with points[1] is where we began.
        let position = [point.x, point.y];
        if point.on_curve {
            match control.take() {
                Some(control) => quadratic(&mut output, current, control, position, fragments),
                None => output.push(position),
            }
            current = position;
        } else {
            if let Some(previous) = control {
                let middle = [
                    (previous[0] + position[0]) / 2.0,
                    (previous[1] + position[1]) / 2.0,
                ];
                quadratic(&mut output, current, previous, middle, fragments);
                current = middle;
            }
            control = Some(position);
        }
    }
    if let Some(control) = control {
        quadratic(&mut output, current, control, start, fragments);
    }
    // Drop the closing duplicate and any zero-length steps.
    output.dedup_by(|a, b| (a[0] - b[0]).abs() < 1.0e-9 && (a[1] - b[1]).abs() < 1.0e-9);
    if output.len() > 1 {
        let (first, last) = (output[0], output[output.len() - 1]);
        if (first[0] - last[0]).abs() < 1.0e-9 && (first[1] - last[1]).abs() < 1.0e-9 {
            output.pop();
        }
    }
    output
}

fn midpoint(a: OutlinePoint, b: OutlinePoint) -> [f64; 2] {
    [(a.x + b.x) / 2.0, (a.y + b.y) / 2.0]
}

fn quadratic(
    output: &mut Vec<[f64; 2]>,
    from: [f64; 2],
    control: [f64; 2],
    to: [f64; 2],
    fragments: usize,
) {
    let incoming = [control[0] - from[0], control[1] - from[1]];
    let outgoing = [to[0] - control[0], to[1] - control[1]];
    let turn = (incoming[0] * outgoing[1] - incoming[1] * outgoing[0])
        .atan2(incoming[0] * outgoing[0] + incoming[1] * outgoing[1])
        .abs();
    let steps = ((fragments as f64 * turn / std::f64::consts::TAU).ceil() as usize).clamp(1, 64);
    for step in 1..=steps {
        let t = step as f64 / steps as f64;
        let u = 1.0 - t;
        output.push([
            u * u * from[0] + 2.0 * u * t * control[0] + t * t * to[0],
            u * u * from[1] + 2.0 * u * t * control[1] + t * t * to[1],
        ]);
    }
}

/// Signed area (positive counter-clockwise).
pub fn signed_area(contour: &[[f64; 2]]) -> f64 {
    let mut area = 0.0;
    for index in 0..contour.len() {
        let a = contour[index];
        let b = contour[(index + 1) % contour.len()];
        area += a[0] * b[1] - b[0] * a[1];
    }
    area / 2.0
}

pub fn point_in_contour(point: [f64; 2], contour: &[[f64; 2]]) -> bool {
    let mut inside = false;
    let mut previous = contour[contour.len() - 1];
    for &current in contour {
        if (current[1] > point[1]) != (previous[1] > point[1]) {
            let x = previous[0]
                + (point[1] - previous[1]) / (current[1] - previous[1])
                    * (current[0] - previous[0]);
            if point[0] < x {
                inside = !inside;
            }
        }
        previous = current;
    }
    inside
}

/// Splits one glyph's contours into filled regions under the non-zero rule
/// TrueType uses: each outer contour with the holes nested directly in it.
///
/// Even-odd and non-zero agree for a face whose contours never overlap, but a
/// variable font such as Dancing Script keeps overlapping strokes, and even-odd
/// would punch a hole wherever two of them cross. Returning one region per
/// outer contour lets the caller union them instead.
pub fn nonzero_regions(contours: Vec<Vec<[f64; 2]>>) -> Vec<Vec<Vec<[f64; 2]>>> {
    let contours: Vec<(Vec<[f64; 2]>, f64)> = contours
        .into_iter()
        .filter(|contour| contour.len() >= 3)
        .map(|contour| {
            let area = signed_area(&contour);
            (contour, area)
        })
        .filter(|(_, area)| area.abs() > 1.0e-12)
        .collect();
    let Some(largest) = contours
        .iter()
        .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        .map(|(_, area)| area.signum())
    else {
        return Vec::new();
    };
    let mut regions: Vec<(Vec<Vec<[f64; 2]>>, f64)> = Vec::new();
    let mut holes = Vec::new();
    for (contour, area) in contours {
        if area.signum() == largest {
            regions.push((vec![contour], area.abs()));
        } else {
            holes.push(contour);
        }
    }
    for hole in holes {
        let probe = hole[0];
        let owner = regions
            .iter()
            .enumerate()
            .filter(|(_, (region, _))| point_in_contour(probe, &region[0]))
            .min_by(|a, b| a.1 .1.total_cmp(&b.1 .1))
            .map(|(index, _)| index);
        if let Some(owner) = owner {
            regions[owner].0.push(hole);
        }
    }
    regions.into_iter().map(|(region, _)| region).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_face_parses_and_maps_basic_latin() {
        for (index, entry) in FONTS.iter().enumerate() {
            let face =
                face(index).unwrap_or_else(|| panic!("{} {} parses", entry.family, entry.style));
            for character in ('A'..='Z').chain('a'..='z').chain('0'..='9') {
                let glyph = face
                    .glyph_index(character)
                    .unwrap_or_else(|| panic!("{} has {character}", entry.family));
                assert!(
                    !face.outline(glyph).is_empty(),
                    "{} {character}",
                    entry.family
                );
                assert!(face.advance(glyph) > 0.0);
            }
            assert!(face.glyph_index('é').is_some(), "{} has é", entry.family);
        }
    }

    #[test]
    fn composite_accented_glyphs_have_more_contours_than_their_base() {
        let face = face(0).unwrap();
        let base = face.outline(face.glyph_index('e').unwrap()).len();
        let accented = face.outline(face.glyph_index('é').unwrap()).len();
        assert!(accented > base, "{accented} vs {base}");
    }

    #[test]
    fn liberation_sans_kerns_av() {
        let face = face(0).unwrap();
        let a = face.glyph_index('A').unwrap();
        let v = face.glyph_index('V').unwrap();
        assert!(face.kerning(a, v) < 0.0, "{}", face.kerning(a, v));
        let o = face.glyph_index('o').unwrap();
        let n = face.glyph_index('n').unwrap();
        assert_eq!(face.kerning(n, o), 0.0);
    }

    #[test]
    fn font_names_resolve_like_fontconfig_patterns() {
        let style = |spec: &str| {
            let resolution = resolve(spec);
            resolution
                .font
                .map(|index| format!("{}:{}", FONTS[index].family, FONTS[index].style))
                .unwrap_or_else(|| STROKE_FAMILY.into())
        };
        assert_eq!(style(""), "Liberation Sans:Regular");
        assert_eq!(style("Liberation Sans"), "Liberation Sans:Regular");
        assert_eq!(style("Liberation Sans:style=Bold"), "Liberation Sans:Bold");
        assert_eq!(
            style("liberation sans:style=italic"),
            "Liberation Sans:Italic"
        );
        assert_eq!(style("Liberation Sans Bold"), "Liberation Sans:Bold");
        assert_eq!(style(":bold"), "Liberation Sans:Bold");
        assert_eq!(style("DancingScript"), "Dancing Script:Regular");
        assert_eq!(style("serif"), "Liberation Serif:Regular");
        assert_eq!(style("ReOpenSCAD Stroke"), STROKE_FAMILY);
        assert!(resolve("serif").warning.is_none());
        assert!(resolve("Liberation Sans:style=Bold").warning.is_none());
        let arial = resolve("Arial");
        assert_eq!(arial.font, Some(0));
        assert!(arial.warning.unwrap().contains("metric-compatible"));
        let missing_style = resolve("Liberation Serif:style=Bold").warning.unwrap();
        assert!(missing_style.contains("\"Bold\""), "{missing_style}");
        let unknown = resolve("Comic Sans MS").warning.unwrap();
        assert!(unknown.contains("Comic Sans MS"), "{unknown}");
        assert!(
            unknown.contains("Liberation Sans:style=Regular"),
            "{unknown}"
        );
        assert!(unknown.contains("Great Vibes"), "{unknown}");
    }
}
