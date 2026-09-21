//! The built-in single-stroke font `text()` draws with.
//!
//! # Why there is a font in here at all
//!
//! `text()` is the one OpenSCAD primitive whose output is not determined by its
//! arguments. OpenSCAD hands the string to FreeType and fills whatever outline
//! the named font gives back, so the same source renders differently on two
//! machines with different fonts installed. This server has no font files, no
//! FreeType, and no user filesystem to find one on, and shipping a copy of some
//! real typeface would put a second licence into a repository whose whole claim
//! is that it was written here.
//!
//! So `text()` draws with *this* — one monoline face, defined below, with no
//! `font` to choose. It is deliberately not a clone of anything: a nameplate
//! rendered here and the same nameplate rendered in OpenSCAD are the same
//! model with different letterforms, and no amount of work on this file would
//! change that. What it does guarantee is that the *same* source always
//! produces the *same* solid, everywhere, forever — which is what a `.scad`
//! file in a repository needs and what a system font cannot promise.
//!
//! # The design
//!
//! Every glyph is a set of strokes on a 1.0 em grid: `x` to the right from the
//! pen's start, `y` up from the baseline. Cap height is [`CAP_HEIGHT`],
//! x-height is 0.50 em, descenders reach [`DESCENDER`]. The strokes are
//! centrelines — `text()` gives them width by sweeping a pen along them — so a
//! glyph is a handful of points rather than an outline, and `O` is one ellipse
//! rather than two.
//!
//! Curves are stored as elliptical arcs rather than as baked polylines. That
//! keeps the table readable, keeps it honest about what the shape *is*, and
//! lets the caller pick the resolution: `text($fn = 8)` gets a visibly faceted
//! `O` and `text($fn = 64)` gets a smooth one, exactly as `circle()` does.

/// Height of a capital letter, in em.
pub const CAP_HEIGHT: f64 = 0.70;
/// How far below the baseline `g`, `j`, `p`, `q` and `y` reach, in em.
pub const DESCENDER: f64 = -0.20;
/// Stroke thickness, in em. Heavy enough that a 6 mm nameplate letter survives
/// a 0.4 mm nozzle, light enough that `8` and `%` do not fill in.
pub const STROKE_WIDTH: f64 = 0.085;
/// Width of the space character, in em.
pub const SPACE_ADVANCE: f64 = 0.50;

/// One piece of a glyph's skeleton.
#[derive(Clone, Copy, Debug)]
pub enum Segment {
    /// A polyline through these points.
    Line(&'static [(f64, f64)]),
    /// An elliptical arc, angles in degrees measured counter-clockwise from
    /// the `+x` axis. `from` may exceed `to`, which draws it clockwise.
    Arc {
        center: (f64, f64),
        radii: (f64, f64),
        from: f64,
        to: f64,
    },
}

/// A glyph: how far the pen travels, and what it draws on the way.
#[derive(Clone, Copy, Debug)]
pub struct Glyph {
    /// Pen advance in em, including both side bearings.
    pub advance: f64,
    pub segments: &'static [Segment],
}

use Segment::{Arc, Line};

/// Expands one glyph into polylines at the requested arc resolution.
///
/// `fragments` is a full circle's worth of facets, as everywhere else in the
/// kernel; an arc takes its pro-rated share, with a floor of two so a quarter
/// turn never collapses to a chord.
pub fn polylines(glyph: &Glyph, fragments: usize) -> Vec<Vec<(f64, f64)>> {
    let mut output = Vec::with_capacity(glyph.segments.len());
    for segment in glyph.segments {
        match segment {
            Line(points) => output.push(points.to_vec()),
            Arc {
                center,
                radii,
                from,
                to,
            } => {
                let sweep = to - from;
                let steps = ((fragments as f64 * sweep.abs() / 360.0).ceil() as usize).max(2);
                let mut points = Vec::with_capacity(steps + 1);
                for step in 0..=steps {
                    let angle = (from + sweep * step as f64 / steps as f64).to_radians();
                    points.push((
                        center.0 + radii.0 * angle.cos(),
                        center.1 + radii.1 * angle.sin(),
                    ));
                }
                output.push(points);
            }
        }
    }
    output
}

/// The glyph for one character, or `None` if this face has no such letter.
///
/// Unknown characters are the caller's problem to report: silently dropping
/// them turns a typo in a label into a label that is quietly wrong, which is
/// worse on a part than a visible error.
pub fn glyph(character: char) -> Option<&'static Glyph> {
    let index = character as usize;
    if !(0x20..=0x7e).contains(&index) {
        return None;
    }
    Some(&PRINTABLE_ASCII[index - 0x20])
}

/// Table shorthand, so a glyph reads as its advance and its strokes.
const fn pen(advance: f64, segments: &'static [Segment]) -> Glyph {
    Glyph { advance, segments }
}

/// Every printable ASCII character, in code-point order from `' '` (0x20).
///
/// Laid out by hand and kept that way: this is a drawing, and one glyph per
/// line with its code point beside it is how a missing or misplaced letter
/// gets spotted. Left to itself rustfmt turns 250 readable lines into 800
/// unreadable ones.
#[rustfmt::skip]
static PRINTABLE_ASCII: [Glyph; 95] = [
    // 0x20 ' '
    pen(SPACE_ADVANCE, &[]),
    // 0x21 '!'
    pen(0.30, &[Line(&[(0.15, 0.70), (0.15, 0.20)]), Line(&[(0.15, 0.06), (0.15, 0.00)])]),
    // 0x22 '"'
    pen(0.34, &[Line(&[(0.10, 0.70), (0.10, 0.52)]), Line(&[(0.24, 0.70), (0.24, 0.52)])]),
    // 0x23 '#'
    pen(0.66, &[
        Line(&[(0.18, 0.70), (0.12, 0.00)]),
        Line(&[(0.42, 0.70), (0.36, 0.00)]),
        Line(&[(0.04, 0.24), (0.52, 0.24)]),
        Line(&[(0.06, 0.46), (0.54, 0.46)]),
    ]),
    // 0x24 '$'
    pen(0.60, &[
        Line(&[(0.28, 0.78), (0.28, -0.08)]),
        Line(&[(0.52, 0.58), (0.42, 0.66), (0.22, 0.68), (0.10, 0.60), (0.08, 0.48), (0.18, 0.40),
               (0.38, 0.34), (0.48, 0.26), (0.48, 0.13), (0.36, 0.04), (0.16, 0.03), (0.04, 0.11)]),
    ]),
    // 0x25 '%'
    pen(0.72, &[
        Line(&[(0.06, 0.00), (0.62, 0.70)]),
        Arc { center: (0.16, 0.56), radii: (0.12, 0.12), from: 0.0, to: 360.0 },
        Arc { center: (0.52, 0.14), radii: (0.12, 0.12), from: 0.0, to: 360.0 },
    ]),
    // 0x26 '&'
    pen(0.68, &[
        Line(&[(0.62, 0.00), (0.22, 0.44), (0.14, 0.54), (0.16, 0.64), (0.26, 0.70), (0.36, 0.66),
               (0.36, 0.56), (0.28, 0.46), (0.10, 0.30), (0.06, 0.18), (0.12, 0.05), (0.26, 0.00),
               (0.42, 0.06), (0.54, 0.22)]),
    ]),
    // 0x27 '\''
    pen(0.20, &[Line(&[(0.10, 0.70), (0.10, 0.52)])]),
    // 0x28 '('
    pen(0.32, &[Line(&[(0.24, 0.76), (0.10, 0.56), (0.06, 0.32), (0.10, 0.08), (0.24, -0.12)])]),
    // 0x29 ')'
    pen(0.32, &[Line(&[(0.08, 0.76), (0.22, 0.56), (0.26, 0.32), (0.22, 0.08), (0.08, -0.12)])]),
    // 0x2a '*'
    pen(0.46, &[
        Line(&[(0.22, 0.70), (0.22, 0.38)]),
        Line(&[(0.08, 0.62), (0.36, 0.46)]),
        Line(&[(0.36, 0.62), (0.08, 0.46)]),
    ]),
    // 0x2b '+'
    pen(0.56, &[Line(&[(0.26, 0.54), (0.26, 0.12)]), Line(&[(0.05, 0.33), (0.47, 0.33)])]),
    // 0x2c ','
    pen(0.26, &[Line(&[(0.16, 0.08), (0.14, 0.00), (0.06, -0.12)])]),
    // 0x2d '-'
    pen(0.46, &[Line(&[(0.06, 0.33), (0.40, 0.33)])]),
    // 0x2e '.'
    pen(0.26, &[Line(&[(0.13, 0.06), (0.13, 0.00)])]),
    // 0x2f '/'
    pen(0.46, &[Line(&[(0.02, -0.08), (0.44, 0.76)])]),
    // 0x30 '0'
    pen(0.62, &[
        Arc { center: (0.28, 0.35), radii: (0.25, 0.35), from: 0.0, to: 360.0 },
        Line(&[(0.14, 0.14), (0.42, 0.56)]),
    ]),
    // 0x31 '1'
    pen(0.46, &[Line(&[(0.06, 0.56), (0.24, 0.70), (0.24, 0.00)]), Line(&[(0.06, 0.00), (0.42, 0.00)])]),
    // 0x32 '2'
    pen(0.58, &[
        Arc { center: (0.27, 0.50), radii: (0.22, 0.20), from: 200.0, to: 0.0 },
        Line(&[(0.49, 0.50), (0.04, 0.00), (0.52, 0.00)]),
    ]),
    // 0x33 '3'
    pen(0.58, &[
        Arc { center: (0.27, 0.52), radii: (0.21, 0.18), from: 190.0, to: -80.0 },
        Arc { center: (0.27, 0.18), radii: (0.23, 0.18), from: 80.0, to: -190.0 },
    ]),
    // 0x34 '4'
    pen(0.60, &[Line(&[(0.38, 0.00), (0.38, 0.70), (0.03, 0.20), (0.55, 0.20)])]),
    // 0x35 '5'
    pen(0.58, &[
        Line(&[(0.50, 0.70), (0.13, 0.70), (0.142, 0.400)]),
        Arc { center: (0.28, 0.22), radii: (0.24, 0.22), from: 125.0, to: -155.0 },
    ]),
    // 0x36 '6'
    pen(0.60, &[
        Line(&[(0.50, 0.66), (0.32, 0.70), (0.16, 0.60), (0.08, 0.38), (0.07, 0.20)]),
        Arc { center: (0.30, 0.20), radii: (0.23, 0.20), from: 180.0, to: -180.0 },
    ]),
    // 0x37 '7'
    pen(0.56, &[Line(&[(0.04, 0.70), (0.50, 0.70), (0.20, 0.00)])]),
    // 0x38 '8'
    pen(0.60, &[
        Arc { center: (0.29, 0.53), radii: (0.20, 0.17), from: -90.0, to: 270.0 },
        Arc { center: (0.29, 0.19), radii: (0.24, 0.19), from: 90.0, to: -270.0 },
    ]),
    // 0x39 '9'
    pen(0.60, &[
        Arc { center: (0.30, 0.50), radii: (0.23, 0.20), from: 0.0, to: 360.0 },
        Line(&[(0.53, 0.50), (0.52, 0.30), (0.44, 0.10), (0.28, 0.00), (0.10, 0.04)]),
    ]),
    // 0x3a ':'
    pen(0.26, &[Line(&[(0.13, 0.50), (0.13, 0.44)]), Line(&[(0.13, 0.06), (0.13, 0.00)])]),
    // 0x3b ';'
    pen(0.26, &[Line(&[(0.15, 0.50), (0.15, 0.44)]), Line(&[(0.16, 0.08), (0.14, 0.00), (0.06, -0.12)])]),
    // 0x3c '<'
    pen(0.54, &[Line(&[(0.46, 0.60), (0.06, 0.33), (0.46, 0.06)])]),
    // 0x3d '='
    pen(0.56, &[Line(&[(0.05, 0.44), (0.47, 0.44)]), Line(&[(0.05, 0.22), (0.47, 0.22)])]),
    // 0x3e '>'
    pen(0.54, &[Line(&[(0.08, 0.60), (0.48, 0.33), (0.08, 0.06)])]),
    // 0x3f '?'
    pen(0.54, &[
        Arc { center: (0.26, 0.52), radii: (0.20, 0.18), from: 200.0, to: -20.0 },
        Line(&[(0.26, 0.34), (0.26, 0.20)]),
        Line(&[(0.26, 0.06), (0.26, 0.00)]),
    ]),
    // 0x40 '@'
    pen(0.76, &[
        Arc { center: (0.36, 0.33), radii: (0.32, 0.33), from: 20.0, to: 340.0 },
        Arc { center: (0.36, 0.30), radii: (0.14, 0.15), from: 0.0, to: 360.0 },
        Line(&[(0.50, 0.30), (0.50, 0.18), (0.62, 0.16)]),
    ]),
    // 0x41 'A'
    pen(0.64, &[Line(&[(0.02, 0.00), (0.30, 0.70), (0.58, 0.00)]), Line(&[(0.11, 0.22), (0.49, 0.22)])]),
    // 0x42 'B'
    pen(0.62, &[
        Line(&[(0.08, 0.00), (0.08, 0.70), (0.36, 0.70)]),
        Arc { center: (0.36, 0.545), radii: (0.16, 0.155), from: 90.0, to: -90.0 },
        Line(&[(0.36, 0.39), (0.08, 0.39)]),
        Line(&[(0.36, 0.39), (0.40, 0.39)]),
        Arc { center: (0.40, 0.195), radii: (0.18, 0.195), from: 90.0, to: -90.0 },
        Line(&[(0.40, 0.00), (0.08, 0.00)]),
    ]),
    // 0x43 'C'
    pen(0.64, &[
        Arc { center: (0.32, 0.35), radii: (0.27, 0.35), from: 55.0, to: 305.0 },
    ]),
    // 0x44 'D'
    pen(0.66, &[
        Line(&[(0.08, 0.00), (0.08, 0.70), (0.30, 0.70)]),
        Arc { center: (0.30, 0.35), radii: (0.28, 0.35), from: 90.0, to: -90.0 },
        Line(&[(0.30, 0.00), (0.08, 0.00)]),
    ]),
    // 0x45 'E'
    pen(0.58, &[
        Line(&[(0.50, 0.70), (0.08, 0.70), (0.08, 0.00), (0.52, 0.00)]),
        Line(&[(0.08, 0.37), (0.42, 0.37)]),
    ]),
    // 0x46 'F'
    pen(0.56, &[
        Line(&[(0.50, 0.70), (0.08, 0.70), (0.08, 0.00)]),
        Line(&[(0.08, 0.37), (0.42, 0.37)]),
    ]),
    // 0x47 'G'
    pen(0.68, &[
        Arc { center: (0.33, 0.35), radii: (0.27, 0.35), from: 55.0, to: 332.7 },
        Line(&[(0.57, 0.1896), (0.57, 0.30), (0.38, 0.30)]),
    ]),
    // 0x48 'H'
    pen(0.66, &[
        Line(&[(0.08, 0.00), (0.08, 0.70)]),
        Line(&[(0.58, 0.00), (0.58, 0.70)]),
        Line(&[(0.08, 0.37), (0.58, 0.37)]),
    ]),
    // 0x49 'I'
    pen(0.34, &[Line(&[(0.17, 0.00), (0.17, 0.70)])]),
    // 0x4a 'J'
    pen(0.54, &[
        Line(&[(0.42, 0.70), (0.42, 0.20)]),
        Arc { center: (0.23, 0.20), radii: (0.19, 0.20), from: 0.0, to: -180.0 },
    ]),
    // 0x4b 'K'
    pen(0.62, &[
        Line(&[(0.08, 0.00), (0.08, 0.70)]),
        Line(&[(0.56, 0.70), (0.08, 0.30)]),
        Line(&[(0.26, 0.45), (0.58, 0.00)]),
    ]),
    // 0x4c 'L'
    pen(0.54, &[Line(&[(0.08, 0.70), (0.08, 0.00), (0.50, 0.00)])]),
    // 0x4d 'M'
    pen(0.76, &[Line(&[(0.08, 0.00), (0.08, 0.70), (0.34, 0.22), (0.60, 0.70), (0.60, 0.00)])]),
    // 0x4e 'N'
    pen(0.68, &[Line(&[(0.08, 0.00), (0.08, 0.70), (0.58, 0.00), (0.58, 0.70)])]),
    // 0x4f 'O'
    pen(0.70, &[Arc { center: (0.35, 0.35), radii: (0.29, 0.35), from: 0.0, to: 360.0 }]),
    // 0x50 'P'
    pen(0.60, &[
        Line(&[(0.08, 0.00), (0.08, 0.70), (0.34, 0.70)]),
        Arc { center: (0.34, 0.535), radii: (0.18, 0.165), from: 90.0, to: -90.0 },
        Line(&[(0.34, 0.37), (0.08, 0.37)]),
    ]),
    // 0x51 'Q'
    pen(0.70, &[
        Arc { center: (0.35, 0.35), radii: (0.29, 0.35), from: 0.0, to: 360.0 },
        Line(&[(0.40, 0.16), (0.64, -0.08)]),
    ]),
    // 0x52 'R'
    pen(0.64, &[
        Line(&[(0.08, 0.00), (0.08, 0.70), (0.34, 0.70)]),
        Arc { center: (0.34, 0.535), radii: (0.18, 0.165), from: 90.0, to: -90.0 },
        Line(&[(0.34, 0.37), (0.08, 0.37)]),
        Line(&[(0.30, 0.37), (0.58, 0.00)]),
    ]),
    // 0x53 'S'
    // Two bowls that share their waist point exactly: the upper ends at 270
    // degrees and the lower starts at 90, both at (0.31, 0.37).
    pen(0.62, &[
        Arc { center: (0.31, 0.535), radii: (0.23, 0.165), from: 20.0, to: 270.0 },
        Arc { center: (0.31, 0.185), radii: (0.23, 0.185), from: 90.0, to: -160.0 },
    ]),
    // 0x54 'T'
    pen(0.60, &[Line(&[(0.02, 0.70), (0.58, 0.70)]), Line(&[(0.30, 0.70), (0.30, 0.00)])]),
    // 0x55 'U'
    pen(0.66, &[
        Line(&[(0.08, 0.70), (0.08, 0.22)]),
        Arc { center: (0.33, 0.22), radii: (0.25, 0.22), from: 180.0, to: 360.0 },
        Line(&[(0.58, 0.22), (0.58, 0.70)]),
    ]),
    // 0x56 'V'
    pen(0.64, &[Line(&[(0.02, 0.70), (0.32, 0.00), (0.62, 0.70)])]),
    // 0x57 'W'
    pen(0.86, &[Line(&[(0.02, 0.70), (0.18, 0.00), (0.42, 0.48), (0.66, 0.00), (0.82, 0.70)])]),
    // 0x58 'X'
    pen(0.62, &[Line(&[(0.04, 0.00), (0.56, 0.70)]), Line(&[(0.04, 0.70), (0.56, 0.00)])]),
    // 0x59 'Y'
    pen(0.62, &[Line(&[(0.03, 0.70), (0.30, 0.36), (0.57, 0.70)]), Line(&[(0.30, 0.36), (0.30, 0.00)])]),
    // 0x5a 'Z'
    pen(0.60, &[Line(&[(0.05, 0.70), (0.55, 0.70), (0.05, 0.00), (0.55, 0.00)])]),
    // 0x5b '['
    pen(0.32, &[Line(&[(0.26, 0.76), (0.09, 0.76), (0.09, -0.12), (0.26, -0.12)])]),
    // 0x5c '\\'
    pen(0.46, &[Line(&[(0.02, 0.76), (0.44, -0.08)])]),
    // 0x5d ']'
    pen(0.32, &[Line(&[(0.06, 0.76), (0.23, 0.76), (0.23, -0.12), (0.06, -0.12)])]),
    // 0x5e '^'
    pen(0.52, &[Line(&[(0.06, 0.48), (0.26, 0.70), (0.46, 0.48)])]),
    // 0x5f '_'
    pen(0.52, &[Line(&[(0.00, -0.14), (0.52, -0.14)])]),
    // 0x60 '`'
    pen(0.26, &[Line(&[(0.05, 0.72), (0.20, 0.56)])]),
    // 0x61 'a'
    pen(0.58, &[
        Arc { center: (0.28, 0.25), radii: (0.20, 0.25), from: 0.0, to: 360.0 },
        Line(&[(0.48, 0.25), (0.48, 0.00)]),
    ]),
    // 0x62 'b'
    pen(0.58, &[
        Line(&[(0.08, 0.70), (0.08, 0.00)]),
        Arc { center: (0.29, 0.25), radii: (0.21, 0.25), from: 180.0, to: -180.0 },
    ]),
    // 0x63 'c'
    pen(0.54, &[Arc { center: (0.27, 0.25), radii: (0.20, 0.25), from: 55.0, to: 305.0 }]),
    // 0x64 'd'
    pen(0.58, &[
        Line(&[(0.50, 0.70), (0.50, 0.00)]),
        Arc { center: (0.29, 0.25), radii: (0.21, 0.25), from: 0.0, to: 360.0 },
    ]),
    // 0x65 'e'
    pen(0.56, &[
        Line(&[(0.07, 0.27), (0.49, 0.27)]),
        Arc { center: (0.28, 0.25), radii: (0.21, 0.25), from: 5.0, to: 300.0 },
    ]),
    // 0x66 'f'
    pen(0.40, &[
        Line(&[(0.38, 0.70), (0.26, 0.70), (0.18, 0.60), (0.18, 0.00)]),
        Line(&[(0.04, 0.44), (0.34, 0.44)]),
    ]),
    // 0x67 'g'
    pen(0.58, &[
        Arc { center: (0.29, 0.25), radii: (0.21, 0.25), from: 0.0, to: 360.0 },
        Line(&[(0.50, 0.50), (0.50, -0.02)]),
        Arc { center: (0.29, -0.02), radii: (0.21, 0.18), from: 0.0, to: -180.0 },
    ]),
    // 0x68 'h'
    pen(0.58, &[
        Line(&[(0.08, 0.70), (0.08, 0.00)]),
        Arc { center: (0.29, 0.29), radii: (0.21, 0.21), from: 180.0, to: 0.0 },
        Line(&[(0.50, 0.29), (0.50, 0.00)]),
    ]),
    // 0x69 'i'
    pen(0.26, &[Line(&[(0.13, 0.50), (0.13, 0.00)]), Line(&[(0.13, 0.68), (0.13, 0.62)])]),
    // 0x6a 'j'
    pen(0.34, &[
        Line(&[(0.22, 0.50), (0.22, -0.02)]),
        Arc { center: (0.13, -0.02), radii: (0.09, 0.09), from: 0.0, to: -180.0 },
        Line(&[(0.22, 0.68), (0.22, 0.62)]),
    ]),
    // 0x6b 'k'
    pen(0.54, &[
        Line(&[(0.08, 0.70), (0.08, 0.00)]),
        Line(&[(0.48, 0.50), (0.08, 0.18)]),
        Line(&[(0.22, 0.29), (0.50, 0.00)]),
    ]),
    // 0x6c 'l'
    pen(0.26, &[Line(&[(0.13, 0.70), (0.13, 0.00)])]),
    // 0x6d 'm'
    pen(0.82, &[
        Line(&[(0.08, 0.50), (0.08, 0.00)]),
        Arc { center: (0.26, 0.29), radii: (0.18, 0.21), from: 180.0, to: 0.0 },
        Line(&[(0.44, 0.29), (0.44, 0.00)]),
        Arc { center: (0.62, 0.29), radii: (0.18, 0.21), from: 180.0, to: 0.0 },
        Line(&[(0.80, 0.29), (0.80, 0.00)]),
    ]),
    // 0x6e 'n'
    pen(0.58, &[
        Line(&[(0.08, 0.50), (0.08, 0.00)]),
        Arc { center: (0.29, 0.29), radii: (0.21, 0.21), from: 180.0, to: 0.0 },
        Line(&[(0.50, 0.29), (0.50, 0.00)]),
    ]),
    // 0x6f 'o'
    pen(0.58, &[Arc { center: (0.29, 0.25), radii: (0.21, 0.25), from: 0.0, to: 360.0 }]),
    // 0x70 'p'
    pen(0.58, &[
        Line(&[(0.08, 0.50), (0.08, -0.20)]),
        Arc { center: (0.29, 0.25), radii: (0.21, 0.25), from: 180.0, to: -180.0 },
    ]),
    // 0x71 'q'
    pen(0.58, &[
        Line(&[(0.50, 0.50), (0.50, -0.20)]),
        Arc { center: (0.29, 0.25), radii: (0.21, 0.25), from: 0.0, to: 360.0 },
    ]),
    // 0x72 'r'
    pen(0.42, &[
        Line(&[(0.08, 0.50), (0.08, 0.00)]),
        Arc { center: (0.30, 0.28), radii: (0.22, 0.22), from: 180.0, to: 60.0 },
    ]),
    // 0x73 's'
    pen(0.52, &[
        Arc { center: (0.26, 0.3825), radii: (0.19, 0.1175), from: 20.0, to: 270.0 },
        Arc { center: (0.26, 0.1325), radii: (0.19, 0.1325), from: 90.0, to: -160.0 },
    ]),
    // 0x74 't'
    pen(0.40, &[
        Line(&[(0.16, 0.70), (0.16, 0.12), (0.26, 0.00), (0.36, 0.02)]),
        Line(&[(0.02, 0.50), (0.32, 0.50)]),
    ]),
    // 0x75 'u'
    pen(0.58, &[
        Line(&[(0.08, 0.50), (0.08, 0.21)]),
        Arc { center: (0.29, 0.21), radii: (0.21, 0.21), from: 180.0, to: 360.0 },
        Line(&[(0.50, 0.21), (0.50, 0.00)]),
    ]),
    // 0x76 'v'
    pen(0.54, &[Line(&[(0.03, 0.50), (0.27, 0.00), (0.51, 0.50)])]),
    // 0x77 'w'
    pen(0.74, &[Line(&[(0.03, 0.50), (0.16, 0.00), (0.37, 0.34), (0.58, 0.00), (0.71, 0.50)])]),
    // 0x78 'x'
    pen(0.54, &[Line(&[(0.05, 0.00), (0.49, 0.50)]), Line(&[(0.05, 0.50), (0.49, 0.00)])]),
    // 0x79 'y'
    pen(0.54, &[
        Line(&[(0.03, 0.50), (0.28, 0.00)]),
        Line(&[(0.51, 0.50), (0.18, -0.20)]),
    ]),
    // 0x7a 'z'
    pen(0.52, &[Line(&[(0.05, 0.50), (0.47, 0.50), (0.05, 0.00), (0.47, 0.00)])]),
    // 0x7b '{'
    pen(0.38, &[
        Line(&[(0.30, 0.76), (0.18, 0.72), (0.18, 0.38), (0.06, 0.32), (0.18, 0.26), (0.18, -0.08),
               (0.30, -0.12)]),
    ]),
    // 0x7c '|'
    pen(0.24, &[Line(&[(0.12, 0.76), (0.12, -0.12)])]),
    // 0x7d '}'
    pen(0.38, &[
        Line(&[(0.08, 0.76), (0.20, 0.72), (0.20, 0.38), (0.32, 0.32), (0.20, 0.26), (0.20, -0.08),
               (0.08, -0.12)]),
    ]),
    // 0x7e '~'
    pen(0.58, &[Line(&[(0.04, 0.34), (0.16, 0.44), (0.30, 0.34), (0.42, 0.24), (0.54, 0.34)])]),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every printable ASCII character has to resolve, and nothing outside the
    /// range may. A missing glyph in the middle of the table would show up as a
    /// neighbouring letter, which is the one failure mode nobody would spot.
    #[test]
    fn every_printable_ascii_character_has_a_glyph() {
        for code in 0x20u8..=0x7e {
            let character = code as char;
            let glyph = glyph(character)
                .unwrap_or_else(|| panic!("no glyph for {character:?} (0x{code:02x})"));
            assert!(
                glyph.advance > 0.0,
                "{character:?} advances by {}",
                glyph.advance
            );
            assert_eq!(
                glyph.segments.is_empty(),
                character == ' ',
                "{character:?} is the wrong kind of blank"
            );
        }
        assert!(glyph('\n').is_none());
        assert!(glyph('é').is_none());
        assert!(glyph('\u{7f}').is_none());
    }

    /// The table is indexed by code point, so a miscount anywhere shifts every
    /// later letter by one. Spot-check both ends and a few in the middle.
    #[test]
    fn the_table_is_aligned_with_the_code_points_it_is_indexed_by() {
        let single_stroke = |character: char| -> Vec<(f64, f64)> {
            match glyph(character).expect("glyph").segments {
                [Line(points)] => points.to_vec(),
                other => panic!("{character:?} is not one polyline: {other:?}"),
            }
        };
        // 'I' is one vertical stroke, 'L' turns once, 'V' turns once the other way.
        assert_eq!(single_stroke('I').len(), 2);
        assert_eq!(single_stroke('L').len(), 3);
        assert_eq!(single_stroke('V').len(), 3);
        assert_eq!(single_stroke('Z').len(), 4);
        assert_eq!(single_stroke('l').len(), 2);
        // 'O' and 'o' are each a single closed arc.
        for round in ['O', 'o'] {
            assert!(
                matches!(glyph(round).expect("glyph").segments, [Arc { .. }]),
                "{round:?} should be one arc"
            );
        }
    }

    /// No glyph may stray outside the em box the layout code reserves for it,
    /// or letters would collide with their neighbours and with the plate they
    /// are engraved into.
    #[test]
    fn no_glyph_escapes_its_own_advance_or_the_vertical_band() {
        for code in 0x21u8..=0x7e {
            let character = code as char;
            let glyph = glyph(character).expect("glyph");
            for polyline in polylines(glyph, 32) {
                for (x, y) in polyline {
                    assert!(
                        (-0.02..=glyph.advance + 0.02).contains(&x),
                        "{character:?} reaches x = {x} outside its advance {}",
                        glyph.advance
                    );
                    assert!((-0.22..=0.80).contains(&y), "{character:?} reaches y = {y}");
                }
            }
        }
    }

    /// Consecutive segments of a glyph are either *joined* or *separate*, and
    /// nothing in between.
    ///
    /// A gap smaller than the pen is invisible — the stroke covers it — and a
    /// gap much wider than the pen is a deliberate lift, the dot over an `i`
    /// or the arms of a `k`. The band between the two is where the bugs live:
    /// an arc that was supposed to flow into the next stroke and misses it by
    /// two thirds of a pen width, which renders as a step or a notch in the
    /// side of the letter rather than as anything anyone intended. `S`, `s`,
    /// `5` and `G` were all drawn that way.
    #[test]
    fn a_glyph_segment_either_meets_the_next_or_is_clearly_a_separate_stroke() {
        // Joined: covered by the pen, so no seam shows. Separate: far enough
        // apart that no reader would expect them to touch.
        let joined = STROKE_WIDTH * 0.3;
        let separate = STROKE_WIDTH * 1.5;
        let mut ragged = Vec::new();
        for code in 0x21u8..=0x7e {
            let character = code as char;
            let runs = polylines(glyph(character).expect("glyph"), 64);
            for pair in runs.windows(2) {
                let (from, to) = (*pair[0].last().expect("end"), pair[1][0]);
                let gap = (from.0 - to.0).hypot(from.1 - to.1);
                if gap > joined && gap < separate {
                    ragged.push(format!(
                        "{character:?}: {gap:.4} ({:.2}x the pen)",
                        gap / STROKE_WIDTH
                    ));
                }
            }
        }
        assert!(
            ragged.is_empty(),
            "{} ragged joins:\n{}",
            ragged.len(),
            ragged.join("\n")
        );
    }

    /// Arc resolution is the caller's to choose, exactly as it is for
    /// `circle()`: more fragments, more points, same shape.
    #[test]
    fn arc_resolution_follows_the_fragment_count() {
        let round = glyph('O').expect("glyph");
        let coarse = polylines(round, 8);
        let fine = polylines(round, 64);
        assert_eq!(coarse.len(), 1);
        assert_eq!(fine.len(), 1);
        assert!(
            fine[0].len() > coarse[0].len() * 4,
            "{} vs {}",
            fine[0].len(),
            coarse[0].len()
        );
        // A quarter turn still gets more than a single chord at any resolution.
        let quarter = Glyph {
            advance: 1.0,
            segments: &[Arc {
                center: (0.5, 0.5),
                radii: (0.4, 0.4),
                from: 0.0,
                to: 90.0,
            }],
        };
        assert!(polylines(&quarter, 3)[0].len() >= 3);
    }
}
