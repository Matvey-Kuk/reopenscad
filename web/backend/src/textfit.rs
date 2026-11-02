//! `set_text`: printable lettering as one tool call.
//!
//! An agent placing a label or a dedication used to hand-write
//! `linear_extrude(...) offset(...) text(...)`, guess a size, then discover
//! with a mesh check that a script font's hairlines were thinner than a
//! nozzle or its counters had closed up. This module does that loop on the
//! server: it fits the size to a box, measures the narrowest stroke and the
//! narrowest gap of the lettering with the wall-thickness estimator (on a
//! tall extrusion of the glyphs, and of their complement), thickens strokes
//! with `offset(r)` up to the requested minimum while keeping the requested
//! minimum gap, and writes the result as one marked block of SCAD that later
//! calls replace in place.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::analysis::{self, MeshIndex};
use crate::engine;
use crate::scadsource;

/// How long one `set_text` layout may search candidates and spacings; past
/// it the best layout so far is returned.
/// (Unoptimised debug builds run several times slower; their budgets scale.)
const SEARCH_BUDGET: Duration = Duration::from_secs(if cfg!(debug_assertions) { 150 } else { 25 });
/// Hard limit for one `set_text` call: every compile, resolve and scan runs
/// under this deadline, and a step that would start after it fails.
pub(crate) const TOTAL_BUDGET: Duration = Duration::from_secs(if cfg!(debug_assertions) { 240 } else { 40 });

fn time_left(started: Instant) -> Result<(), String> {
    if started.elapsed() > TOTAL_BUDGET {
        Err(format!(
            "set_text used its {} s budget before finishing; try a shorter text, a simpler font, or a given size instead of fit.",
            TOTAL_BUDGET.as_secs()
        ))
    } else {
        Ok(())
    }
}

/// Marker comments around a block `set_text` owns.
const BLOCK_START: &str = "// reopenscad:text id=";
const BLOCK_END: &str = "// reopenscad:end-text id=";

/// The lettering a call describes.
pub(crate) struct TextSpec {
    pub id: String,
    pub text: String,
    pub font: Option<String>,
    pub size: Option<f64>,
    pub fit: Option<[f64; 2]>,
    pub halign: String,
    pub valign: String,
    pub direction: Option<String>,
    pub language: Option<String>,
    pub height: f64,
    pub min_stroke: f64,
    pub min_gap: f64,
    pub translate: [f64; 3],
    pub rotate: [f64; 3],
    pub color: Option<String>,
    pub name: Option<String>,
    /// `text(spacing = ...)`; 1 is the font's own.
    pub spacing: f64,
    /// Whether `layout` may widen the spacing to separate letters that
    /// would otherwise fuse through a thin bridge.
    pub auto_spacing: bool,
}

fn number(input: &Value, key: &str, default: f64, low: f64, high: f64) -> Result<f64, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_f64()
            .filter(|number| number.is_finite() && *number >= low && *number <= high)
            .ok_or_else(|| format!("{key} must be a number from {low} to {high}.")),
    }
}

fn vector<const N: usize>(input: &Value, key: &str, default: [f64; N]) -> Result<[f64; N], String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Array(items)) if items.len() == N => {
            let mut out = default;
            for (slot, item) in out.iter_mut().zip(items) {
                *slot = item
                    .as_f64()
                    .filter(|number| number.is_finite())
                    .ok_or_else(|| format!("{key} must be {N} numbers."))?;
            }
            Ok(out)
        }
        Some(_) => Err(format!("{key} must be {N} numbers.")),
    }
}

fn word(input: &Value, key: &str) -> Result<Option<String>, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.len() <= 200 => Ok(Some(text.clone())),
        Some(_) => Err(format!("{key} must be a string of at most 200 characters.")),
    }
}

impl TextSpec {
    pub(crate) fn from_input(input: &Value) -> Result<Self, String> {
        let text = input
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty() && text.chars().count() <= 500)
            .ok_or("text is required (1 to 500 characters).")?
            .to_string();
        let id = word(input, "id")?.unwrap_or_else(|| "text-1".into());
        if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') || id.is_empty() {
            return Err("id may contain only letters, digits, - and _.".into());
        }
        let fit = match input.get("fit") {
            None | Some(Value::Null) => None,
            Some(fit) => {
                let width = number(fit, "width", 0.0, 0.0, 10_000.0)?;
                let height = number(fit, "height", 0.0, 0.0, 10_000.0)?;
                if width <= 0.0 && height <= 0.0 {
                    return Err("fit needs width and/or height (mm).".into());
                }
                Some([width, height])
            }
        };
        let align = |key: &str, allowed: &[&str], default: &str| -> Result<String, String> {
            let value = word(input, key)?.unwrap_or_else(|| default.to_string());
            if allowed.contains(&value.as_str()) {
                Ok(value)
            } else {
                Err(format!("{key} must be one of {allowed:?}."))
            }
        };
        Ok(Self {
            id,
            text,
            font: word(input, "font")?,
            size: match input.get("size") {
                None | Some(Value::Null) => None,
                Some(_) => Some(number(input, "size", 10.0, 0.5, 1000.0)?),
            },
            fit,
            halign: align("halign", &["left", "center", "right"], "center")?,
            valign: align("valign", &["top", "center", "baseline", "bottom"], "center")?,
            direction: word(input, "direction")?,
            language: word(input, "language")?,
            height: number(input, "height", 1.0, 0.01, 1000.0)?,
            min_stroke: number(input, "minStroke", 0.84, 0.0, 50.0)?,
            min_gap: number(input, "minGap", 0.4, 0.0, 50.0)?,
            translate: vector(input, "translate", [0.0; 3])?,
            rotate: vector(input, "rotate", [0.0; 3])?,
            color: word(input, "color")?,
            name: word(input, "name")?,
            spacing: number(input, "spacing", 1.0, 0.5, 3.0)?,
            auto_spacing: input.get("autoSpacing").and_then(Value::as_bool).unwrap_or(input.get("spacing").is_none()),
        })
    }

    /// `text(...)` at `size`.
    fn text_call(&self, size: f64) -> String {
        let literal = |text: &str| scadsource::scad_literal(&json!(text)).unwrap_or_else(|_| "\"\"".into());
        let mut call = format!(
            "text({}, size = {}, halign = \"{}\", valign = \"{}\"",
            literal(&self.text),
            format_number(size),
            self.halign,
            self.valign
        );
        if let Some(font) = &self.font {
            call.push_str(&format!(", font = {}", literal(font)));
        }
        if let Some(direction) = &self.direction {
            call.push_str(&format!(", direction = {}", literal(direction)));
        }
        if let Some(language) = &self.language {
            call.push_str(&format!(", language = {}", literal(language)));
        }
        if (self.spacing - 1.0).abs() > 1e-9 {
            call.push_str(&format!(", spacing = {}", format_number(self.spacing)));
        }
        call.push(')');
        call
    }

    /// The 2D lettering: strokes grown by `grow` (`offset(r)`), then gaps
    /// narrower than `close` fused by a morphological closing (grow by
    /// close / 2, shrink by close / 2), so every remaining gap is at least
    /// `close` wide and strokes that nearly touch merge with a full-width
    /// join instead of a thin waist.
    fn profile(&self, size: f64, grow: f64, close: f64) -> String {
        self.profile_opened(size, grow, close, 0.0)
    }

    /// [`Self::profile`], then a morphological opening by `open` (shrink by
    /// open / 2, grow back): removes material thinner than `open` (the
    /// flaps a barely surviving counter leaves next to a closed gap), and
    /// only ever widens gaps.
    fn profile_opened(&self, size: f64, grow: f64, close: f64, open: f64) -> String {
        let mut profile = self.text_call(size);
        if grow > 0.0 {
            profile = format!("offset(r = {}, $fn = 24) {profile}", format_number(grow));
        }
        if close > 0.0 {
            // Shrink back 0.01 mm less than grown: polygonal arcs do not
            // cancel exactly, and an exact closing can leave hairline slits.
            let (out, back) = (format_number(close / 2.0), format_number((close / 2.0 - 0.01).max(0.0)));
            profile = format!("offset(r = -{back}, $fn = 24) offset(r = {out}, $fn = 24) {profile}");
        }
        if open > 0.0 {
            let half = format_number(open / 2.0);
            profile = format!("offset(r = {half}, $fn = 24) offset(r = -{half}, $fn = 24) {profile}");
            if close > 0.0 {
                // Close again: parts the opening separated can meet again
                // when grown back, leaving hairline slits.
                let (out, back) = (format_number(close / 2.0), format_number((close / 2.0 - 0.01).max(0.0)));
                profile = format!("offset(r = -{back}, $fn = 24) offset(r = {out}, $fn = 24) {profile}");
            }
        }
        profile
    }

    /// The block written into the design.
    fn block(&self, size: f64, grow: f64, close: f64, open: f64, report: &str) -> String {
        let mut body = format!(
            "linear_extrude(height = {}) {};",
            format_number(self.height),
            self.profile_opened(size, grow, close, open)
        );
        if self.rotate != [0.0; 3] {
            body = format!("rotate({}) {body}", vector_literal(self.rotate));
        }
        if self.translate != [0.0; 3] {
            body = format!("translate({}) {body}", vector_literal(self.translate));
        }
        if let Some(color) = &self.color {
            let literal = scadsource::scad_literal(&json!(color)).unwrap_or_else(|_| "\"gold\"".into());
            body = format!("color({literal}) {body}");
        }
        let name = self
            .name
            .as_ref()
            .map(|name| format!(" {}", name.replace('\n', " ")))
            .unwrap_or_default();
        format!(
            "{BLOCK_START}{}{name}\n// {report}\n{body}\n{BLOCK_END}{}\n",
            self.id, self.id
        )
    }
}

fn format_number(value: f64) -> String {
    let rounded = (value * 1e4).round() / 1e4;
    let text = format!("{rounded}");
    if text == "-0" { "0".into() } else { text }
}

fn vector_literal(values: [f64; 3]) -> String {
    format!(
        "[{}, {}, {}]",
        format_number(values[0]),
        format_number(values[1]),
        format_number(values[2])
    )
}

/// Bounds of the compiled 2D profile (via a thin extrusion): (min, max),
/// and how many enclosed holes (counters) it has.
fn profile_bounds(profile: &str) -> Result<([f64; 2], [f64; 2], i64), String> {
    profile_stats(profile).map(|(low, high, holes, _)| (low, high, holes))
}

/// [`profile_bounds`] plus whether the extrusion is a valid closed solid.
fn profile_stats(profile: &str) -> Result<([f64; 2], [f64; 2], i64, bool), String> {
    let compiled = crate::compile_parts_guarded(
        &format!("linear_extrude(1) {profile};"),
        engine::Quality::Preview,
    )?;
    let mut low = [f64::INFINITY; 2];
    let mut high = [f64::NEG_INFINITY; 2];
    let mut holes = 0i64;
    let mut valid = true;
    for part in &compiled.parts {
        // An extruded disc with h holes is a closed surface of genus h:
        // V - E + F = 2 (components - holes).
        let measure = analysis::measure_mesh(&part.mesh);
        if !measure.complete {
            return Err(crate::csg::bsp::interrupt_message().unwrap_or("The time budget ran out.").to_string());
        }
        valid &= measure.watertight() && measure.manifold() && measure.winding_consistent();
        let faces = measure.triangles as i64;
        let euler = measure.vertices as i64 - faces * 3 / 2 + faces;
        holes += measure.components as i64 - euler / 2;
    }
    for part in &compiled.parts {
        for vertex in part.mesh.triangles.iter().flat_map(|triangle| triangle.vertices.iter()) {
            low[0] = low[0].min(vertex.x);
            low[1] = low[1].min(vertex.y);
            high[0] = high[0].max(vertex.x);
            high[1] = high[1].max(vertex.y);
        }
    }
    if !low[0].is_finite() {
        return Err("The text produced no outline (unknown font, or only spaces?).".into());
    }
    let declined = compiled.messages.iter().any(|line| line.contains("declined"));
    Ok((low, high, holes, valid && !declined))
}

/// Narrowest wall of a tall extrusion of `profile`, i.e. the narrowest
/// width of the 2D region (strokes, or gaps when `profile` is the
/// complement), as the smallest width three samples agree on so a stray
/// bevelled corner cannot set it. Sharp tips (serif points, stroke ends) are left out by the
/// estimator; `None` when nothing was measured.
fn narrowest(profile: &str, tall: f64) -> Result<Option<(f64, [f64; 2])>, String> {
    let compiled = crate::compile_parts_guarded(
        &format!("linear_extrude({}) {profile};", format_number(tall)),
        engine::Quality::Preview,
    )?;
    let mut best: Option<(f64, [f64; 2])> = None;
    for part in &compiled.parts {
        let index = MeshIndex::from_mesh(&part.mesh);
        // A tiny threshold keeps the sample spacing fine.
        // Side-wall samples in the middle half of the height only: a sample
        // on a cap or near one is pinched by the cap edge and reads the
        // extrusion, not the 2D width.
        let check = analysis::thickness_check_where(&index, 0.05, &|point| {
            (point[2] - tall / 2.0).abs() < tall / 4.0
        });
        // Seams under 0.02 mm are numerical (exactly cancelling offsets
        // meeting) and fuse in any slicer; they are not a gap or a stroke.
        if let Some(spot) = check.robust_min_above(0.02) {
            if best.is_none_or(|(width, _)| spot.thickness < width) {
                best = Some((spot.thickness, [spot.from[0], spot.from[1]]));
            }
        }
    }
    Ok(best)
}

/// What one profile measures: narrowest stroke and gap (width and where),
/// 2D bounds, and the number of counters.
struct Measured {
    stroke: Option<(f64, [f64; 2])>,
    gap: Option<(f64, [f64; 2])>,
    low: [f64; 2],
    high: [f64; 2],
    holes: i64,
    /// The extruded lettering is a closed manifold solid.
    valid: bool,
}

fn measure(profile: &str, size: f64, with_gap: bool, started: Instant) -> Result<Measured, String> {
    time_left(started)?;
    let (low, high, holes, valid) = profile_stats(profile)?;
    let tall = size.max(2.0);
    let stroke = narrowest(profile, tall)?;
    if !with_gap {
        return Ok(Measured { stroke, gap: None, low, high, holes, valid });
    }
    // The complement inside a frame one size wider than the lettering: its
    // narrowest width is the narrowest gap (counters, letter spacing).
    let margin = size.max(2.0);
    let frame = format!(
        "difference() {{ translate([{}, {}]) square([{}, {}]); {profile}; }}",
        format_number(low[0] - margin),
        format_number(low[1] - margin),
        format_number(high[0] - low[0] + 2.0 * margin),
        format_number(high[1] - low[1] + 2.0 * margin)
    );
    let gap = narrowest(&frame, tall)?;
    Ok(Measured { stroke, gap, low, high, holes, valid })
}

/// Lays out the lettering ([`layout_at`]); when that leaves a stroke or gap
/// problem and the caller did not fix the spacing, retries with letters a
/// little further apart (block letters that nearly touch fuse through thin
/// bridges; joined script is clean at its own spacing and never gets here)
/// and keeps the first clean result.
pub(crate) fn layout(spec: &mut TextSpec) -> Result<Value, String> {
    let started = Instant::now();
    // Every compile, outline resolve and thickness scan below stops at this.
    let _deadline = crate::csg::bsp::deadline_scope(started + TOTAL_BUDGET);
    let mut first = layout_at(spec, started)?;
    if first["ok"] == json!(true) || !spec.auto_spacing {
        return Ok(first);
    }
    let base = spec.spacing;
    for factor in [1.05, 1.1, 1.2] {
        if started.elapsed() > SEARCH_BUDGET {
            first["searchStopped"] = json!("time budget used; letter-spacing retries skipped");
            break;
        }
        spec.spacing = base * factor;
        let mut candidate = layout_at(spec, started)?;
        if candidate["ok"] == json!(true) {
            candidate["spacingAdjusted"] = json!({"from": base, "to": spec.spacing, "why": "letters fused through a bridge thinner than minStroke at the font's own spacing"});
            return Ok(candidate);
        }
    }
    spec.spacing = base;
    Ok(first)
}

/// Lays out the lettering at the spec's spacing: size (given, or fitted to
/// `fit`), strokes grown to `minStroke`, gaps under `minGap` fused where
/// that is clean, and the measured result. Pure analysis; nothing is saved.
fn layout_at(spec: &TextSpec, started: Instant) -> Result<Value, String> {
    let mut size = spec.size.unwrap_or(10.0);
    let mut grow = 0.0;
    let mut raw = measure(&spec.profile(size, 0.0, 0.0), size, false, started)?;
    // Fit to the box (the offsets add a fixed margin that does not scale),
    // then re-measure at the fitted size; twice converges.
    for _ in 0..2 {
        grow = ((spec.min_stroke - raw.stroke.map_or(0.0, |(width, _)| width)) / 2.0).max(0.0);
        let Some([width, height]) = spec.fit else {
            break;
        };
        let (current_width, current_height) = (raw.high[0] - raw.low[0], raw.high[1] - raw.low[1]);
        let mut factor = f64::INFINITY;
        if width > 0.0 {
            factor = factor.min((width - 2.0 * grow).max(1e-6) / current_width.max(1e-9));
        }
        if height > 0.0 {
            factor = factor.min((height - 2.0 * grow).max(1e-6) / current_height.max(1e-9));
        }
        if !factor.is_finite() || (factor - 1.0).abs() < 1e-3 {
            break;
        }
        size = (size * factor).clamp(0.5, 1000.0);
        raw = measure(&spec.profile(size, 0.0, 0.0), size, false, started)?;
    }
    // Gaps and flaps. Candidates, simplest first: the grown lettering;
    // with gaps under minGap fused (closing, which suits joined script); and
    // closed then opened by minStroke (removing flaps thinner than a stroke
    // that a barely surviving counter leaves). The first that keeps every
    // stroke, gap and counter wins; otherwise the one with fewest problems.
    let slack = 0.05;
    let problems = |measured: &Measured, counters: i64| -> usize {
        usize::from(measured.stroke.is_some_and(|(width, _)| width + slack < spec.min_stroke))
            + usize::from(measured.gap.is_some_and(|(width, _)| width + slack < spec.min_gap))
            + usize::from(measured.holes < counters)
            + 3 * usize::from(!measured.valid)
    };
    let grown = measure(&spec.profile(size, grow, 0.0), size, true, started)?;
    let counters = grown.holes.min(raw.holes);
    let describe = |close: f64, open: f64, measured: &Measured, count: usize| {
        json!({
            "gapClosing": close,
            "flapOpening": open,
            "stroke": measured.stroke.map(|(width, _)| width),
            "gap": measured.gap.map(|(width, _)| width),
            "counters": measured.holes,
            "validMesh": measured.valid,
            "problems": count,
        })
    };
    let mut tried = vec![describe(0.0, 0.0, &grown, problems(&grown, counters))];
    let mut best = (problems(&grown, counters), 0.0, 0.0, grown);
    if best.0 > 0 && spec.min_gap > 0.0 {
        // Wider closing (minGap + minStroke) also fills the waist of a bridge
        // where fused script strokes meet, up to a full stroke width.
        let wide = spec.min_gap + spec.min_stroke;
        for (close, open) in [
            (spec.min_gap, 0.0),
            (spec.min_gap, spec.min_stroke),
            (wide, 0.0),
            (wide, spec.min_stroke),
        ] {
            if started.elapsed() > SEARCH_BUDGET {
                break;
            }
            let candidate = measure(&spec.profile_opened(size, grow, close, open), size, true, started)?;
            let count = problems(&candidate, counters);
            tried.push(describe(close, open, &candidate, count));
            if count < best.0 {
                best = (count, close, open, candidate);
            }
            if best.0 == 0 {
                break;
            }
        }
    }
    let (_, close, open, done) = best;
    let mut warnings: Vec<String> = Vec::new();
    let stroke_width = done.stroke.map(|(width, _)| width);
    let gap_width = done.gap.map(|(width, _)| width);
    if stroke_width.is_some_and(|width| width + slack < spec.min_stroke) {
        warnings.push(format!(
            "The narrowest stroke is {:.2} mm (< minStroke {:.2}), where strokes that nearly touch were fused into a bridge. At this size the lettering cannot have both minStroke strokes and minGap gaps there; try a size around {:.1} (gaps scale with the size, the stroke growth does not), a bolder font, or lower minStroke / minGap.",
            stroke_width.unwrap_or(0.0),
            spec.min_stroke,
            size * 1.3
        ));
    }
    if gap_width.is_some_and(|width| width + slack < spec.min_gap) {
        // Gaps scale with the size, the stroke offset does not.
        let raw_needed = spec.min_gap + 2.0 * grow;
        let at_size = gap_width.unwrap_or(0.0) + 2.0 * grow;
        let hint = (at_size > 1e-3)
            .then(|| size * raw_needed / at_size * 1.05)
            .filter(|suggested| *suggested <= size * 4.0)
            .map(|suggested| format!(" Size {suggested:.1} or more would keep it."))
            .unwrap_or_default();
        warnings.push(format!(
            "The narrowest gap is {:.2} mm (< minGap {:.2}); fusing it would have made a thin bridge, so it was left open.{hint} Or add letter spacing, use a bolder font, or lower minGap.",
            gap_width.unwrap_or(0.0),
            spec.min_gap
        ));
    }
    if done.stroke.is_none() || done.gap.is_none() {
        warnings.push("The stroke or gap width could not be measured (too little surface sampled); treat printability as unknown.".into());
    }
    if let Some([width, height]) = spec.fit {
        let (actual_width, actual_height) = (done.high[0] - done.low[0], done.high[1] - done.low[1]);
        let over = |limit: f64, actual: f64| limit > 0.0 && actual > limit * 1.02 + 0.05;
        if over(width, actual_width) || over(height, actual_height) {
            warnings.push(format!(
                "The lettering is {actual_width:.1} x {actual_height:.1} mm and does not fit the requested {width} x {height} mm box (the smallest usable size, or the stroke growth, is larger than the box allows)."
            ));
        }
    }
    if !done.valid {
        warnings.push("The lettering does not extrude to a valid closed solid (outlines touching at a point); change the size slightly or the spacing.".into());
    }
    let closed = raw.holes - done.holes;
    let mut suggested_size = None;
    if closed > 0 {
        // Counters scale with the size, the offsets do not. Try a few larger
        // sizes (fixed offsets) for one that keeps every counter.
        for factor in [1.25, 1.5, 2.0, 3.0] {
            let candidate = size * factor;
            if started.elapsed() > SEARCH_BUDGET {
                break;
            }
            let (_, _, holes) = profile_bounds(&spec.profile(candidate, grow, close))?;
            let (_, _, raw_holes) = profile_bounds(&spec.profile(candidate, 0.0, 0.0))?;
            if holes >= raw_holes {
                suggested_size = Some((candidate * 10.0).round() / 10.0);
                break;
            }
        }
        warnings.push(format!(
            "{closed} counter(s) (enclosed holes such as the eye of an e) closed up: at this size the strokes cannot reach minStroke and keep minGap. Use a larger size{}, a bolder font, or lower minStroke / minGap.",
            suggested_size
                .map(|suggested| format!(" (about {suggested:.1} or more)"))
                .unwrap_or_default()
        ));
    }
    let point = |spot: Option<(f64, [f64; 2])>| spot.map(|(_, at)| json!(at));
    let report = format!(
        "size {}, strokes grown {} mm per side, gaps under {} mm fused; narrowest stroke {}, gap {}",
        format_number(size),
        format_number(grow),
        format_number(close),
        stroke_width.map_or("?".into(), |width| format!("{width:.2}")),
        gap_width.map_or("?".into(), |width| format!("{width:.2}")),
    );
    Ok(json!({
        "size": (size * 1e4).round() / 1e4,
        "offset": (grow * 1e4).round() / 1e4,
        "gapClosing": close,
        "flapOpening": open,
        "candidates": tried,
        "bounds2d": {"min": done.low, "max": done.high, "width": done.high[0] - done.low[0], "height": done.high[1] - done.low[1]},
        "narrowestStroke": {"before": raw.stroke.map(|(width, _)| width), "after": stroke_width, "at": point(done.stroke)},
        "narrowestGap": {"after": gap_width, "at": point(done.gap)},
        "counters": {"before": raw.holes, "after": done.holes},
        "minStroke": spec.min_stroke,
        "minGap": spec.min_gap,
        "ok": warnings.is_empty(),
        "warnings": warnings,
        "suggestedSize": suggested_size,
        "scad": spec.block(size, grow, close, open, &report),
        "note": "Widths are measured in the text's own plane (before translate/rotate) by sampling, with sharp stroke tips left out. Strokes are grown with offset(r) and gaps narrower than minGap are fused (closing), so cursive joins become full-width; outlines are resolved to clean, non-overlapping contours.",
    }))
}

/// Where the block with `id` sits in `source`: byte range of the whole
/// block, start marker line through end marker line. Markers count only as
/// real line comments (not inside a block comment or a string). `Err` when
/// the id's block appears more than once or is not closed.
pub(crate) fn find_block(source: &str, id: &str) -> Result<Option<(usize, usize)>, String> {
    let start_marker = format!("{BLOCK_START}{id}");
    let end_marker = format!("{BLOCK_END}{id}");
    let mut found: Vec<(usize, usize)> = Vec::new();
    let mut open: Option<usize> = None;
    // Whether each line starts inside a block comment or a string.
    let (mut in_block, mut in_string) = (false, false);
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let starts_in_code = !in_block && !in_string;
        let trimmed = line.trim();
        if starts_in_code {
            let is_start = trimmed == start_marker
                || trimmed.strip_prefix(&start_marker).is_some_and(|rest| rest.starts_with(' '));
            if is_start {
                if open.is_some() {
                    return Err(format!("The text block {id} is opened twice without being closed."));
                }
                open = Some(offset);
            } else if trimmed == end_marker {
                if let Some(begin) = open.take() {
                    found.push((begin, offset + line.len()));
                }
            }
        }
        // Advance the comment/string state through this line.
        let bytes = line.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            match (in_block, in_string, bytes[index]) {
                (true, _, b'*') if bytes.get(index + 1) == Some(&b'/') => {
                    in_block = false;
                    index += 1;
                }
                (false, true, b'\\') => index += 1,
                (false, true, b'"') => in_string = false,
                (false, false, b'"') => in_string = true,
                (false, false, b'/') if bytes.get(index + 1) == Some(&b'*') => {
                    in_block = true;
                    index += 1;
                }
                (false, false, b'/') if bytes.get(index + 1) == Some(&b'/') => break,
                _ => {}
            }
            index += 1;
        }
        offset += line.len();
    }
    if open.is_some() {
        return Err(format!("The text block {id} has no // reopenscad:end-text id={id} line."));
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(Some(found[0])),
        count => Err(format!("The text block {id} appears {count} times; remove the extra copies or use another id.")),
    }
}

#[cfg(test)]
mod tests {
    use super::find_block;

    #[test]
    fn markers_count_only_as_real_line_comments() {
        let live = "// reopenscad:text id=a\ncube(1);\n// reopenscad:end-text id=a\n";
        let commented = format!("/*\n{live}*/\nx = \"\n{live}\";\n{live}");
        let (start, end) = find_block(&commented, "a").unwrap().unwrap();
        assert_eq!(&commented[start..end], live);
        assert!(find_block(&format!("{live}{live}"), "a").is_err());
        assert!(find_block("// reopenscad:text id=a\ncube(1);\n", "a").is_err());
        assert_eq!(find_block("cube(1);\n", "a").unwrap(), None);
        // Another id's markers are not this block.
        assert_eq!(find_block(live, "b").unwrap(), None);
    }
}
