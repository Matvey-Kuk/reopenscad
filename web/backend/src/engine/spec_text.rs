//! `text()`.
//!
//! OpenSCAD fills an outline from whatever font fontconfig finds on the
//! machine; this engine sets type in the bundled OFL faces of
//! [`super::typeface`], defaulting to Liberation Sans exactly as OpenSCAD
//! does, and keeps its own monoline face ([`super::font`]) selectable as
//! `font = "ReOpenSCAD Stroke"`. The first half of this file specifies the
//! stroke face; the second half the outline faces — font selection, layout,
//! alignment, direction, kerning and diagnostics.

use super::*;

fn evaluate_source(source: &str) -> (Result<Shape, EngineError>, Vec<String>) {
    let tokens = Lexer::new(source).tokenize().expect("lex text source");
    let statements = Parser::new(tokens)
        .parse_program()
        .expect("parse text source");
    let mut evaluator = Evaluator::default();
    let shape = evaluator.evaluate(&statements);
    (shape, evaluator.diagnostics)
}

fn bounds_of(source: &str) -> Bounds {
    let (shape, _) = evaluate_source(source);
    shape.expect("text should produce a shape").bounds()
}

/// A drawn string is planar, and it starts at the origin and runs right along
/// the baseline — the same corner OpenSCAD starts from, so a source that
/// positions a label with `translate()` lands in the same place.
#[test]
fn text_is_a_planar_shape_on_the_baseline_at_the_origin() {
    let (shape, _) = evaluate_source("text(\"II\", size = 10, font = \"ReOpenSCAD Stroke\");");
    let shape = shape.expect("shape");
    assert_eq!(shape.dimension(), ShapeDimension::Planar);
    let bounds = shape.bounds();
    // `I` is a bare vertical stroke from the baseline to the cap height, and
    // the pen butt-caps a free end rather than overshooting it, so a capital
    // is *exactly* one cap height tall. A square projecting cap would make
    // every letter half a stroke taller than the size it was asked for, which
    // is the sort of thing nobody notices until two lines fail to align.
    assert!(bounds.min.x > -0.1 && bounds.min.x < 2.0, "{bounds:?}");
    assert!(bounds.min.y.abs() < 1.0e-9, "{bounds:?}");
    assert!(
        (bounds.max.y - font::CAP_HEIGHT * 10.0).abs() < 1.0e-9,
        "{bounds:?}"
    );
}

/// Nothing any letter is made of reaches further from its centreline than
/// half a pen.
///
/// This is the property a round nib has and a square one does not. A square
/// nib is axis-aligned however the stroke runs, so its corners sit at
/// `sqrt(2)` half-widths from the centre and stick out past the stroke's own
/// edge wherever the stroke is diagonal or curved — a spike at every joint,
/// and a sawtooth all down the side of an `S`. Every vertex of an inscribed
/// polygon is exactly one half-width out, so it can only fill the wedge
/// between two quads, never escape it.
///
/// Swept over the whole face, because the nib is only emitted at joints: a
/// letter like `X`, which is two straight strokes and no joint at all, cannot
/// show the defect however badly the nib is drawn.
#[test]
fn no_part_of_any_letter_reaches_past_its_own_stroke() {
    const SIZE: f64 = 100.0;
    let half = font::STROKE_WIDTH * SIZE / 2.0;

    fn collect(shape: &Shape, into: &mut Vec<[f64; 2]>) {
        match shape {
            Shape::Polygon2d { contours } => into.extend(contours.iter().flatten().copied()),
            Shape::Union { children, .. } => {
                for child in children {
                    collect(&child.shape, into);
                }
            }
            other => panic!("text() built something unexpected: {other:?}"),
        }
    }
    let to_segment = |point: [f64; 2], from: (f64, f64), to: (f64, f64)| -> f64 {
        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
        let length_squared = dx * dx + dy * dy;
        let along = if length_squared <= f64::EPSILON {
            0.0
        } else {
            (((point[0] - from.0) * dx + (point[1] - from.1) * dy) / length_squared).clamp(0.0, 1.0)
        };
        (point[0] - (from.0 + dx * along)).hypot(point[1] - (from.1 + dy * along))
    };

    let mut worst = (0.0f64, ' ');
    for code in 0x21u8..=0x7e {
        let character = code as char;
        let escaped = match character {
            '"' => "\\\"".to_string(),
            '\\' => "\\\\".to_string(),
            other => other.to_string(),
        };
        let glyph = font::glyph(character).expect("glyph");
        // The same resolution `text()` will use, pinned with `$fn` so the
        // measurement is against the polygon the stroke was actually built on
        // and not a finer one — a chord of a coarser arc sits inside a finer
        // one, and half a pen out from it lands a little further from the
        // fine curve. That is a difference in *where the curve is*, not a
        // fault in the pen, and it would drown the thing being tested.
        let centrelines: Vec<Vec<(f64, f64)>> = font::polylines(glyph, 64)
            .into_iter()
            .map(|run| run.into_iter().map(|(x, y)| (x * SIZE, y * SIZE)).collect())
            .collect();
        let mut outline = Vec::new();
        collect(
            &evaluate_source(&format!(
                "text(\"{escaped}\", size = 100, font = \"ReOpenSCAD Stroke\", $fn = 64);"
            ))
            .0
            .unwrap_or_else(|error| panic!("{character:?} did not draw: {error}")),
            &mut outline,
        );
        assert!(!outline.is_empty(), "{character:?} has no geometry");
        for point in outline {
            let nearest = centrelines
                .iter()
                .flat_map(|run| run.windows(2))
                .map(|pair| to_segment(point, pair[0], pair[1]))
                // A single-point run is a dot, and its nib is measured from
                // the point itself.
                .chain(
                    centrelines
                        .iter()
                        .filter(|run| run.len() == 1)
                        .map(|run| to_segment(point, run[0], run[0])),
                )
                .fold(f64::INFINITY, f64::min);
            if nearest > worst.0 {
                worst = (nearest, character);
            }
        }
    }
    // A hair over one half-width for the arithmetic, and nowhere near the
    // 1.414 a square nib reports.
    assert!(
        worst.0 <= half * 1.001,
        "{:?} reaches {:.4} from its centreline; the pen allows {half:.4} ({:.3}x)",
        worst.1,
        worst.0,
        worst.0 / half
    );
}

/// `size` scales the whole face, so twice the size is twice the box in both
/// directions — the property every label on a part depends on.
#[test]
fn size_scales_the_whole_face() {
    let small = bounds_of("text(\"ABC\", size = 6, font = \"ReOpenSCAD Stroke\");");
    let large = bounds_of("text(\"ABC\", size = 12, font = \"ReOpenSCAD Stroke\");");
    let ratio_x = (large.max.x - large.min.x) / (small.max.x - small.min.x);
    let ratio_y = (large.max.y - large.min.y) / (small.max.y - small.min.y);
    assert!((ratio_x - 2.0).abs() < 0.02, "width scaled by {ratio_x}");
    assert!((ratio_y - 2.0).abs() < 0.02, "height scaled by {ratio_y}");
}

/// `spacing` stretches the gaps without stretching the letters. Checking the
/// *width* alone would pass for a face that simply got wider, so the letter
/// height has to come back unchanged too.
#[test]
fn spacing_opens_the_gaps_and_leaves_the_letters_alone() {
    let tight = bounds_of("text(\"MMMM\", size = 10, font = \"ReOpenSCAD Stroke\");");
    let loose = bounds_of("text(\"MMMM\", size = 10, font = \"ReOpenSCAD Stroke\", spacing = 2);");
    assert!(
        loose.max.x - loose.min.x > (tight.max.x - tight.min.x) * 1.6,
        "spacing = 2 should open the line out: {} vs {}",
        loose.max.x - loose.min.x,
        tight.max.x - tight.min.x
    );
    assert!(
        ((loose.max.y - loose.min.y) - (tight.max.y - tight.min.y)).abs() < 1.0e-9,
        "spacing must not change the letter height"
    );
}

/// Alignment moves the drawn string relative to the origin, which is what
/// makes a centred legend on a keycap a one-liner instead of arithmetic.
#[test]
fn halign_and_valign_move_the_string_relative_to_the_origin() {
    let left = bounds_of("text(\"WIDE\", size = 10, font = \"ReOpenSCAD Stroke\");");
    let centred =
        bounds_of("text(\"WIDE\", size = 10, font = \"ReOpenSCAD Stroke\", halign = \"center\");");
    let right =
        bounds_of("text(\"WIDE\", size = 10, font = \"ReOpenSCAD Stroke\", halign = \"right\");");
    let width = left.max.x - left.min.x;
    assert!(left.min.x > -1.0, "left-aligned text starts at the origin");
    assert!(
        (centred.min.x + centred.max.x).abs() < 1.0,
        "centred text straddles the origin: {centred:?}"
    );
    assert!(right.max.x < 1.0, "right-aligned text ends at the origin");
    // The three are the same drawing, just moved.
    assert!(((centred.max.x - centred.min.x) - width).abs() < 1.0e-9);
    assert!(((right.max.x - right.min.x) - width).abs() < 1.0e-9);

    let baseline = bounds_of("text(\"E\", size = 10, font = \"ReOpenSCAD Stroke\");");
    let top = bounds_of("text(\"E\", size = 10, font = \"ReOpenSCAD Stroke\", valign = \"top\");");
    assert!(
        top.max.y < baseline.max.y,
        "valign = top drops the string below the origin"
    );
}

/// A character this face has no glyph for is *named*, not silently dropped: a
/// label with a letter quietly missing is a part that has to be reprinted.
#[test]
fn a_character_with_no_glyph_is_reported_by_name() {
    let (shape, diagnostics) =
        evaluate_source("text(\"Café\", size = 10, font = \"ReOpenSCAD Stroke\");");
    assert!(shape.is_ok(), "the rest of the string still draws");
    let warning = diagnostics
        .iter()
        .find(|line| line.contains("no glyph"))
        .unwrap_or_else(|| panic!("nothing said about the missing glyph: {diagnostics:?}"));
    assert!(warning.contains('é'), "the warning must name it: {warning}");

    // A string with nothing drawable at all produces no geometry, and says so.
    let (shape, diagnostics) =
        evaluate_source("text(\"→→\", size = 10, font = \"ReOpenSCAD Stroke\");");
    assert!(
        shape.is_err() || shape.is_ok_and(|shape| shape.bounds().max.x <= shape.bounds().min.x),
        "a string of unknown characters draws nothing"
    );
    assert!(diagnostics.iter().any(|line| line.contains("no glyph")));
}

/// The stroke face has no vertical layout; asking for one is said, not
/// silently ignored.
#[test]
fn the_stroke_face_reports_a_vertical_direction() {
    let (shape, diagnostics) = evaluate_source(
        "text(\"AB\", size = 10, font = \"ReOpenSCAD Stroke\", direction = \"ttb\");",
    );
    assert!(shape.is_ok(), "the string still draws");
    assert!(
        diagnostics
            .iter()
            .any(|line| line.contains("direction ignored")),
        "{diagnostics:?}"
    );
}

/// The label on a part is usually computed, and OpenSCAD stringifies whatever
/// it is handed. `text(7)` and `text("7")` have to agree.
#[test]
fn a_non_string_argument_is_stringified() {
    let number = bounds_of("text(7, size = 10);");
    let string = bounds_of("text(\"7\", size = 10);");
    assert!(
        (number.max.x - string.max.x).abs() < 1.0e-9,
        "{number:?} vs {string:?}"
    );
    assert!((number.max.y - string.max.y).abs() < 1.0e-9);
}

/// Arc resolution is the caller's, exactly as it is for `circle()`.
#[test]
fn fragment_settings_reach_the_letterforms() {
    let coarse = evaluate_source("text(\"O\", size = 10, font = \"ReOpenSCAD Stroke\", $fn = 6);")
        .0
        .expect("shape");
    let fine = evaluate_source("text(\"O\", size = 10, font = \"ReOpenSCAD Stroke\", $fn = 64);")
        .0
        .expect("shape");
    let count = |shape: &Shape| -> usize {
        match shape {
            Shape::Union { children, .. } => children.len(),
            _ => 1,
        }
    };
    assert!(
        count(&fine) > count(&coarse) * 4,
        "a finer O is built from more strokes: {} vs {}",
        count(&fine),
        count(&coarse)
    );
}

/// The drawn string has to be a usable 2D region: extruding it must give a
/// closed solid, because a label is almost always `linear_extrude(text(...))`
/// and a self-intersecting region there poisons every later boolean.
#[test]
fn an_extruded_label_is_a_closed_solid() {
    let source =
        "linear_extrude(2) text(\"Ag8%\", size = 10, font = \"ReOpenSCAD Stroke\", $fn = 24);";
    let output = crate::engine::exact::compile_exact(source, None).expect("compile the label");
    let mut edges: std::collections::HashMap<([i64; 3], [i64; 3]), i32> =
        std::collections::HashMap::new();
    let key = |point: Vec3| -> [i64; 3] {
        [
            (point.x * 1.0e6).round() as i64,
            (point.y * 1.0e6).round() as i64,
            (point.z * 1.0e6).round() as i64,
        ]
    };
    for triangle in &output.mesh.triangles {
        for index in 0..3 {
            let from = key(triangle.vertices[index]);
            let to = key(triangle.vertices[(index + 1) % 3]);
            if from == to {
                continue;
            }
            *edges.entry((from.min(to), from.max(to))).or_insert(0) +=
                if from < to { 1 } else { -1 };
        }
    }
    let open = edges.values().filter(|balance| **balance != 0).count();
    assert_eq!(open, 0, "the extruded label has {open} unpaired edges");
}

// ---------------------------------------------------------------------------
// Outline faces
// ---------------------------------------------------------------------------

fn diagnostics_of(source: &str) -> Vec<String> {
    evaluate_source(source).1
}

fn width_of(source: &str) -> f64 {
    let bounds = bounds_of(source);
    bounds.max.x - bounds.min.x
}

fn unpaired_edges(source: &str) -> usize {
    let output = crate::engine::exact::compile_exact(source, None).expect("compile");
    let mut edges: HashMap<([i64; 3], [i64; 3]), i32> = HashMap::new();
    let key = |point: Vec3| -> [i64; 3] {
        [
            (point.x * 1.0e6).round() as i64,
            (point.y * 1.0e6).round() as i64,
            (point.z * 1.0e6).round() as i64,
        ]
    };
    for triangle in &output.mesh.triangles {
        for index in 0..3 {
            let from = key(triangle.vertices[index]);
            let to = key(triangle.vertices[(index + 1) % 3]);
            if from == to {
                continue;
            }
            *edges.entry((from.min(to), from.max(to))).or_insert(0) +=
                if from < to { 1 } else { -1 };
        }
    }
    edges.values().filter(|balance| **balance != 0).count()
}

/// With no `font`, text is set in Liberation Sans — OpenSCAD's own default —
/// at an em of `size / 0.72`, so a capital is a little under `size` tall, as
/// OpenSCAD documents ("an ascent of approximately the given value").
#[test]
fn the_default_face_is_liberation_sans_sized_like_openscad() {
    let (shape, diagnostics) = evaluate_source("text(\"H\", size = 10);");
    assert!(
        diagnostics.iter().all(|line| !line.contains("WARNING")),
        "{diagnostics:?}"
    );
    let bounds = shape.expect("shape").bounds();
    // Liberation Sans cap height is 1409 units of a 2048 em.
    let expected = 1409.0 / 2048.0 * 10.0 / 0.72;
    assert!(
        (bounds.max.y - expected).abs() < 1.0e-6,
        "{bounds:?} vs {expected}"
    );
    assert!(bounds.min.y.abs() < 1.0e-9, "{bounds:?}");
    let named = bounds_of("text(\"H\", size = 10, font = \"Liberation Sans\");");
    assert_eq!(format!("{named:?}"), format!("{bounds:?}"));
}

/// `font` really selects the face: the same string comes out a different
/// width in each family, and the positional form (`text(t, size, font)`) is
/// the same call as the named one.
#[test]
fn font_selects_a_different_face() {
    let mut widths = Vec::new();
    for font in [
        "Liberation Sans",
        "Liberation Sans:style=Bold",
        "Liberation Sans:style=Italic",
        "Liberation Serif",
        "Liberation Mono",
        "Dancing Script",
        "Great Vibes",
        "ReOpenSCAD Stroke",
    ] {
        let source = format!("text(\"Wavy Lines\", size = 10, font = \"{font}\");");
        let diagnostics = diagnostics_of(&source);
        assert!(
            diagnostics.iter().all(|line| !line.contains("WARNING")),
            "{font}: {diagnostics:?}"
        );
        widths.push((font, width_of(&source)));
    }
    for (index, (font, width)) in widths.iter().enumerate() {
        for (other, other_width) in &widths[index + 1..] {
            assert!(
                (width - other_width).abs() > 0.05,
                "{font} and {other} set the same width {width}"
            );
        }
    }
    assert_eq!(
        format!("{:?}", bounds_of("text(\"Ag\", 8, \"Liberation Serif\");")),
        format!(
            "{:?}",
            bounds_of("text(\"Ag\", size = 8, font = \"Liberation Serif\");")
        )
    );
    // Bold is heavier: same string, wider.
    assert!(
        width_of("text(\"MMM\", font = \"Liberation Sans:style=Bold\");")
            > width_of("text(\"MMM\", font = \"Liberation Sans\");")
    );
}

/// An unknown font is not silently swapped: the warning names what was asked
/// for, what was used instead, and what there is to choose from.
#[test]
fn an_unknown_font_names_its_fallback() {
    let (shape, diagnostics) = evaluate_source("text(\"A\", font = \"Comic Sans MS:style=Bold\");");
    assert!(shape.is_ok());
    let warning = diagnostics
        .iter()
        .find(|line| line.contains("Comic Sans MS"))
        .unwrap_or_else(|| panic!("{diagnostics:?}"));
    assert!(warning.contains("Liberation Sans:style=Bold"), "{warning}");
    assert!(warning.contains("Dancing Script"), "{warning}");

    let missing_style = diagnostics_of("text(\"A\", font = \"Great Vibes:style=Bold\");");
    assert!(
        missing_style
            .iter()
            .any(|line| line.contains("style \"Bold\"") && line.contains("Great Vibes")),
        "{missing_style:?}"
    );
    let brand = diagnostics_of("text(\"A\", font = \"Arial\");");
    assert!(
        brand.iter().any(|line| line.contains("metric-compatible")),
        "{brand:?}"
    );
    assert!(diagnostics_of("text(\"A\", font = \"monospace\");")
        .iter()
        .all(|line| !line.contains("WARNING")));
}

/// Pair kerning is applied: "AV" sets tighter than an "A" and a "V" side by
/// side, which is the difference between a nameplate that looks typeset and
/// one that looks typed.
#[test]
fn pairs_are_kerned() {
    let pair = width_of("text(\"AV\");");
    let a = bounds_of("text(\"A\");");
    let v = bounds_of("text(\"V\");");
    let advance_a = 1366.0 / 2048.0 * 10.0 / 0.72;
    let unkerned = advance_a + v.max.x - a.min.x;
    assert!(pair < unkerned - 0.1, "{pair} vs unkerned {unkerned}");
}

/// Latin-1 is covered now: an accented letter draws, and draws with its
/// accent (a composite glyph), rather than vanishing with a warning.
#[test]
fn accented_letters_draw() {
    let (shape, diagnostics) = evaluate_source("text(\"é\");");
    assert!(
        diagnostics.iter().all(|line| !line.contains("no glyph")),
        "{diagnostics:?}"
    );
    let accented = shape.expect("shape").bounds();
    let plain = bounds_of("text(\"e\");");
    assert!(
        accented.max.y > plain.max.y + 1.0,
        "{accented:?} vs {plain:?}"
    );

    let (_, diagnostics) = evaluate_source("text(\"A漢\");");
    assert!(
        diagnostics
            .iter()
            .any(|line| line.contains("no glyph") && line.contains('漢')),
        "{diagnostics:?}"
    );
}

/// A script face's glyphs overlap themselves and their neighbours. Under the
/// even-odd rule every crossing would become a hole; under TrueType's
/// non-zero rule, which is what this engine applies, an extruded word is one
/// closed solid.
#[test]
fn overlapping_script_strokes_extrude_closed() {
    for font in ["Dancing Script", "Great Vibes"] {
        let source = format!("linear_extrude(1) text(\"Hello\", size = 10, font = \"{font}\");");
        assert_eq!(unpaired_edges(&source), 0, "{font}");
    }
    assert_eq!(
        unpaired_edges("linear_extrude(2) text(\"Ag8%é\", size = 10, $fn = 24);"),
        0
    );
}

/// `direction`: rtl reverses the order glyphs are set in, ttb and btt stack
/// them vertically.
#[test]
fn direction_controls_the_writing_order() {
    let rtl = evaluate_source("text(\"AB\", direction = \"rtl\");")
        .0
        .expect("rtl");
    let reversed = evaluate_source("text(\"BA\");").0.expect("ltr");
    assert_eq!(
        format!("{:?}", rtl.bounds()),
        format!("{:?}", reversed.bounds())
    );

    let vertical = bounds_of("text(\"ABC\", direction = \"ttb\");");
    assert!(
        vertical.max.y - vertical.min.y > 2.0 * (vertical.max.x - vertical.min.x),
        "{vertical:?}"
    );
    assert!(
        vertical.max.y <= 1.0e-9,
        "ttb hangs from the origin: {vertical:?}"
    );
    let upward = bounds_of("text(\"ABC\", direction = \"btt\");");
    assert!(((upward.max.y - upward.min.y) - (vertical.max.y - vertical.min.y)).abs() < 1.0);

    let unknown = diagnostics_of("text(\"A\", direction = \"sideways\");");
    assert!(
        unknown
            .iter()
            .any(|line| line.contains("unknown direction")),
        "{unknown:?}"
    );
}

/// halign uses the advance width, valign the inked extent.
#[test]
fn outline_alignment() {
    let centred = bounds_of("text(\"OXO\", halign = \"center\", valign = \"center\");");
    assert!((centred.min.x + centred.max.x).abs() < 0.5, "{centred:?}");
    assert!(
        (centred.min.y + centred.max.y).abs() < 1.0e-6,
        "{centred:?}"
    );
    let top = bounds_of("text(\"Hg\", valign = \"top\");");
    assert!(top.max.y.abs() < 1.0e-6, "{top:?}");
    let bottom = bounds_of("text(\"Hg\", valign = \"bottom\");");
    assert!(bottom.min.y.abs() < 1.0e-6, "{bottom:?}");
    let right = bounds_of("text(\"H\", halign = \"right\");");
    assert!(right.max.x < 0.0, "the right side bearing stays: {right:?}");
    let unknown = diagnostics_of("text(\"A\", halign = \"middle\");");
    assert!(
        unknown.iter().any(|line| line.contains("unknown halign")),
        "{unknown:?}"
    );
}

/// `em` sets the em size directly; `size = 7.2` and `em = 10` are the same
/// text.
#[test]
fn em_is_the_em_size() {
    let em = bounds_of("text(\"Hx\", em = 10);");
    let size = bounds_of("text(\"Hx\", size = 7.2);");
    assert!((em.max.x - size.max.x).abs() < 1.0e-9 && (em.max.y - size.max.y).abs() < 1.0e-9);
}

/// Curve resolution follows `$fn`, like `circle()`.
#[test]
fn outline_curves_follow_fn() {
    fn count(shape: &Shape) -> usize {
        match shape {
            Shape::Polygon2d { contours } => contours.iter().map(Vec::len).sum(),
            Shape::Union { children, .. } => children.iter().map(|child| count(&child.shape)).sum(),
            _ => 0,
        }
    }
    let points = |source: &str| count(&evaluate_source(source).0.expect("shape"));
    let coarse = points("text(\"O\", $fn = 8);");
    let fine = points("text(\"O\", $fn = 96);");
    assert!(fine > coarse * 4, "{fine} vs {coarse}");
}
