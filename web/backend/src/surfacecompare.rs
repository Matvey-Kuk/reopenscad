//! `compare_surfaces` and `measure_between`: scan-to-CAD agreement without
//! solid booleans, so open meshes (one-sided scans)
//! compare as well as closed ones.
//!
//! compare_surfaces samples both surfaces by area and measures each sample
//! to the other surface (exact nearest point), in both directions: how far
//! the observed scan lies from the model, and how far the model lies from
//! the scan where the scan backs it. Model surface with no scan behind it
//! (an inferred underside, a face past the scan's open edge) is reported as
//! unsupported and kept out of the error statistics; scan surface the model
//! does not reach is reported as uncovered. Pairs whose normals disagree
//! (matched against the wrong side of a wall) count as facing mismatches,
//! never as agreement.
//!
//! measure_between casts rays from one surface to another and reports
//! every distance it finds with its endpoints, classified as material
//! thickness, recess depth or air gap, as observed (both surfaces scanned)
//! or inferred (a model surface involved), and says when the other surface
//! is missing for too many rays to support a number.

use crate::assets::TriMesh;
use crate::meshanalysis::MeshIndex;
use crate::scancommon::{
    self as sc, add, design_tolerance, dot, norm, r3, round_to, scale, sub, unit, Design, Legend, Marker, Sample, SceneObject, Stats, Surface, P,
};
use crate::AppState;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const BUDGET: Duration = Duration::from_secs(60);
const DEFAULT_SAMPLES: usize = 20_000;
const MAX_SAMPLES: usize = 100_000;
/// Per-triangle heatmap queries beyond this many triangles fall back to
/// the samples on each triangle.
const MAX_CENTROID_QUERIES: usize = 1_000_000;
/// Regions listed per list by default, and with `detail`.
const DEFAULT_MAX_REGIONS: u64 = 3;
const DETAIL_MAX_REGIONS: u64 = 8;
/// Regions under this share of the compared area are not listed.
const DEFAULT_MIN_REGION_FRACTION: f64 = 0.005;

pub(crate) fn tool_definitions() -> Vec<Value> {
    let surface = sc::surface_schema();
    let image = json!({"description": "true (default), false, or render_image options shared by every picture.", "anyOf": [{"type": "boolean"}, {"type": "object"}]});
    let common = |extra: Value| {
        let mut properties = json!({
            "designId": {"type": "string"},
            "detail": sc::detail_schema(),
        });
        if let (Some(base), Some(extra)) = (properties.as_object_mut(), extra.as_object()) {
            base.extend(extra.clone());
        }
        properties
    };
    vec![
        crate::mcp_tool(
            "compare_surfaces",
            "Scan-to-CAD surface agreement, both directions, with no solid booleans; works on open scans. \
             reference is the observed surface (a scan), candidate the reconstruction. Each \
             area sample is measured to the exact nearest point of the other surface and classed: matched \
             (same facing), facingMismatch (normals opposed beyond 180 - maxNormalAngleDeg: a wall's \
             wrong side), contradicted (the nearest surface crosses it or ends before it: a missing or \
             extra feature). Not scored: hidden (candidate behind the scan, not facing its way), \
             unobserved (candidate away from the reference), pastRim (beside the other's open edge). \
             Scored samples count at their real distance. signedMean: + = outside the other surface (on \
             referenceToCandidate the model is too small there). coverage: matched within maxDistance \
             (default max(2, 5 x tolerance)). within {min, max} samples only inside that box, each still \
             measured to the whole other surface; it takes in any neighbouring surface passing through, \
             so make it tight; an empty box gives matches false. verdict.matches: coverage >= \
             minCoverage, rms <= tolerance and p95 <= 2 x tolerance both ways, at most 2% facing mismatch \
             and 2% contradicted per side; a closed, manifold model can fail it. regionsDropped counts regions left out by maxRegions and minRegionArea. Normals are never flipped automatically (flip* only for a \
             one-sided surface whose normals point inward).",
            json!({
                "type": "object",
                "required": ["designId", "reference", "candidate"],
                "properties": common(json!({
                    "within": sc::within_schema(sc::SURFACE_FRAME),
                    "reference": surface.clone(),
                    "candidate": surface.clone(),
                    "tolerance": {"type": "number", "exclusiveMinimum": 0, "description": "mm; default the design's."},
                    "maxDistance": {"type": "number", "exclusiveMinimum": 0},
                    "maxNormalAngleDeg": {"type": "number", "minimum": 0, "maximum": 85, "default": 60},
                    "ignoreNormals": {"type": "boolean", "default": false},
                    "flipReferenceNormals": {"type": "boolean", "default": false},
                    "flipCandidateNormals": {"type": "boolean", "default": false},
                    "samples": {"type": "integer", "minimum": 100, "maximum": MAX_SAMPLES, "default": DEFAULT_SAMPLES},
                    "maxRegions": {"type": "integer", "minimum": 0, "maximum": 64, "description": "Regions per list (default 3; 8 with detail)."},
                    "minRegionArea": {"type": "number", "minimum": 0, "description": "mm^2; smaller regions are not listed (default 0.5% of the side's compared area)."},
                    "minCoverage": {"type": "number", "minimum": 0, "maximum": 1, "default": 0.95},
                    "colorRange": {"type": "number", "exclusiveMinimum": 0, "description": "Heatmap saturation, mm (default max(3 x tolerance, p95))."},
                    "image": image.clone()
                })),
            }),
        ),
        crate::mcp_tool(
            "measure_between",
            "Distances from one surface to another along rays: wall thickness, recess depth, air gap, or \
             a model wall against its scan. Area-samples `from` and casts along direction (inward: \
             against from's normal, into material; outward; or a vector); the first crossing of `to` \
             within maxDistance (mm) counts. kind: material (leaves material at `to`: a thickness), gap \
             (enters `to` through air), through (leaves through the same one-sided scan: an upper bound), \
             insufficient (under minHitFraction of rays reach `to`: no thickness asserted, missing \
             patches listed). evidence: observed (both surfaces are uploaded scan data), \
             inferred (a model surface: what the model claims, not what was measured). \
             within {min, max} casts only from the part of `from` inside that box (`to` stays whole).",
            json!({
                "type": "object",
                "required": ["designId", "from", "to"],
                "properties": common(json!({
                    "within": sc::within_schema(sc::SURFACE_FRAME),
                    "from": surface.clone(),
                    "to": surface,
                    "direction": {"description": "\"inward\" (default), \"outward\" or [x, y, z].", "anyOf": [{"type": "string", "enum": ["inward", "outward"]}, {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3}]},
                    "maxDistance": {"type": "number", "exclusiveMinimum": 0, "default": 30},
                    "samples": {"type": "integer", "minimum": 10, "maximum": 20000, "default": 2000},
                    "minHitFraction": {"type": "number", "minimum": 0, "maximum": 1, "default": 0.5},
                    "expected": {"type": "object", "required": ["value"], "properties": {"value": {"type": "number"}, "tolerance": {"type": "number", "minimum": 0}}},
                    "image": image
                })),
            }),
        ),
    ]
}

pub(crate) fn mcp_dispatch(state: &AppState, name: &str, arguments: &Value) -> Value {
    match name {
        "compare_surfaces" => match compare_value(state, arguments) {
            Ok((mut value, images)) => {
                if arguments.get("detail").and_then(Value::as_bool) != Some(true) {
                    if let Some(verdict) = value.get_mut("verdict").and_then(Value::as_object_mut) {
                        verdict.remove("reasons");
                    }
                    if let Some(shape) = value.get_mut("shapeAccuracy").and_then(Value::as_object_mut) {
                        shape.remove("note");
                    }
                }
                let summary = compare_summary(&value);
                sc::summary_result(summary, sc::compact(value, arguments), images)
            }
            Err(error) => crate::mcp_tool_error(&error),
        },
        "measure_between" => match between_value(state, arguments) {
            Ok((value, images)) => sc::image_result(sc::compact(value, arguments), images),
            Err(error) => crate::mcp_tool_error(&error),
        },
        _ => crate::mcp_tool_error(&format!("Unknown tool {name:?}.")),
    }
}

/// `POST /api/designs/{id}/compare {a, b}`, the Compare parts panel: two objects of
/// the current revision through compare_surfaces, recorded in the activity log like
/// an agent's call so the browser reads one form. The caller holds a render permit.
pub(crate) fn browser_compare(state: &AppState, id: &str, input: &Value) -> Result<Value, (u16, String)> {
    let part = |key: &str| input.get(key).and_then(Value::as_str).filter(|part| !part.is_empty());
    let (Some(a), Some(b)) = (part("a"), part("b")) else {
        return Err((400, "a and b are required: the two objects to compare.".into()));
    };
    if a == b {
        return Err((400, "Choose two different parts.".into()));
    }
    let arguments = json!({"designId": id, "reference": a, "candidate": b, "image": false});
    // Read before comparing: a save in between makes the result look older than
    // it is, and the browser drops it, never the other way round.
    let revision = state.workspaces.revision_of(id).ok().flatten();
    let ticket = crate::activity::begin(state, id, "compare_surfaces", &arguments).ok_or((404, "Design not found.".to_string()))?;
    crate::activity::mark_from_browser(&ticket, revision);
    let seq = ticket.seq();
    let (result, offered) = crate::activity::capture(|| mcp_dispatch(state, "compare_surfaces", &arguments));
    crate::activity::finish(ticket, &arguments, &result, offered);
    if result["isError"] == json!(true) {
        let message = result["content"][0]["text"].as_str().unwrap_or("The comparison failed.");
        return Err((422, message.to_string()));
    }
    Ok(json!({"ok": true, "entry": crate::activity::entry_public(id, seq)}))
}

/// One line for a client that shows only `content`: the full result is
/// `structuredContent`, sent once.
fn compare_summary(value: &Value) -> String {
    if value["nothingInside"] == json!(true) {
        return "compare_surfaces: nothing inside the box, so nothing was compared; see within in structuredContent.".to_string();
    }
    let number = |value: &Value| value.as_f64().map_or("n/a".to_string(), sc::short);
    format!(
        "compare_surfaces: {}; shape accuracy rms {} p95 {} max {} mm; {:.0}% of the reference not reproduced; {} regions listed. Full result in structuredContent.",
        if value["verdict"]["matches"] == json!(true) { "matches" } else { "does not match" },
        number(&value["shapeAccuracy"]["rms"]),
        number(&value["shapeAccuracy"]["p95"]),
        number(&value["shapeAccuracy"]["max"]),
        value["missingFraction"].as_f64().unwrap_or(0.0) * 100.0,
        ["worst", "missingRegions", "facingMismatches", "unsupported", "uncovered"].iter().filter_map(|list| value[*list].as_array()).map(Vec::len).sum::<usize>(),
    )
}

fn pick_fields(value: &Value, keys: &[&str]) -> Value {
    let mut out = json!({});
    for key in keys {
        if let Some(field) = value.get(*key) {
            out[*key] = field.clone();
        }
    }
    out
}

fn without_fields(mut value: Value, keys: &[&str]) -> Value {
    if let Some(fields) = value.as_object_mut() {
        keys.iter().for_each(|key| {
            fields.remove(*key);
        });
    }
    value
}

/// `value` without `count`, `area` and any number `other` states the same.
fn without_repeats(value: Value, other: &Value) -> Value {
    let mut value = without_fields(value, &["count", "area"]);
    if let Some(fields) = value.as_object_mut() {
        fields.retain(|key, field| !(field.is_number() && other.get(key).and_then(Value::as_f64).zip(field.as_f64()).is_some_and(|(a, b)| (a - b).abs() < 1e-3)));
    }
    value
}

pub(crate) type Images = Vec<(String, Vec<u8>)>;

/// A rough spacing between samples on a surface of `area` with `count` samples.
fn spacing(area: f64, count: usize) -> f64 {
    (area / count.max(1) as f64).sqrt()
}

// ---------------------------------------------------------------------------
// compare_surfaces
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Facing the same way and lying along the other's normal (or touching
    /// it): a deviation, scored.
    Matched,
    /// Facing opposite ways: the wrong side of a wall. Scored as failing.
    FacingMismatch,
    /// Neither: the nearest surface runs across this one, or lies beyond
    /// one of its edges, and nothing explains it. Scored at its distance.
    Contradicted,
    /// Candidate only: behind the scan surface and not facing its way: a
    /// back or inner face the scan cannot see. Excluded (unsupported).
    Hidden,
    /// Beside the other surface's open edge: past where it was observed (or
    /// modelled). Excluded (unsupported / uncovered).
    PastRim,
    /// Candidate only: not facing the reference's way and beyond
    /// maxDistance of it: model surface this reference does not look at.
    /// Excluded (unsupported).
    Unobserved,
}

impl Class {
    fn scored(self) -> bool {
        matches!(self, Class::Matched | Class::FacingMismatch | Class::Contradicted)
    }
}

struct Measured {
    sample: Sample,
    signed: f64,
    nearest: P,
    class: Class,
}

struct Side<'a> {
    name: &'static str,
    surface: &'a Surface,
    measured: Vec<Measured>,
}

/// How a side is measured against the other.
#[derive(Clone, Copy)]
struct Rules {
    /// cos of maxNormalAngleDeg; `None` ignores normals.
    cos_limit: Option<f64>,
    /// Samples within this of the other surface touch it whatever the angle.
    touch: f64,
    /// The candidate side: a sample hidden behind the scan is unsupported.
    hidden_allowed: bool,
    /// Candidate side: unmatched samples beyond this are unobserved.
    max_distance: f64,
    /// Candidate side: even same-facing surface farther than this (a
    /// quarter of the reference's size, at least max_distance) is not what
    /// the reference looks at.
    max_reach: f64,
}

/// cos of the angle within which an offset counts as along a normal.
const ALONG_NORMAL: f64 = 0.866;

#[allow(clippy::too_many_arguments)]
fn classify(normal: P, distance: f64, offset: P, other: &Surface, nearest: P, other_normal: P, rules: Rules) -> Class {
    let cosine = dot(normal, other_normal);
    let same = rules.cos_limit.is_none_or(|limit| cosine >= limit);
    let opposite = rules.cos_limit.is_some_and(|limit| cosine <= -limit);
    let touching = distance <= rules.touch;
    let along = touching || dot(unit(offset), other_normal).abs() >= ALONG_NORMAL;
    let in_front = dot(offset, other_normal) > 0.0;
    if along {
        if same {
            if rules.hidden_allowed && distance > rules.max_reach {
                return Class::Unobserved;
            }
            return Class::Matched;
        }
        if rules.hidden_allowed && !in_front && !touching {
            return Class::Hidden;
        }
        if opposite && !(rules.hidden_allowed && distance > rules.max_distance) {
            return Class::FacingMismatch;
        }
        if touching && !opposite {
            // Meeting at a corner.
            return Class::Matched;
        }
    } else if other.rim_distance(nearest) <= distance + rules.touch {
        return Class::PastRim;
    }
    // A same-facing surface displaced by any distance stays scored (an
    // extra boss's top); anything else that far from the reference is not
    // what it looks at. The reference side keeps everything: observed
    // surface must be explained.
    if rules.hidden_allowed && distance > rules.max_distance && !(along && same) {
        return Class::Unobserved;
    }
    if opposite && along {
        return Class::FacingMismatch;
    }
    Class::Contradicted
}

/// Measures `samples` of one surface against `other`.
fn measure_side(samples: &[Sample], own_flip: bool, other_index: &MeshIndex, other: &Surface, other_flip: bool, rules: Rules, deadline: Instant) -> Result<Vec<Measured>, String> {
    let mut out = Vec::with_capacity(samples.len());
    for (i, s) in samples.iter().enumerate() {
        if i % 2048 == 0 {
            crate::meshops::check(Some(deadline))?;
        }
        let normal = if own_flip { scale(s.normal, -1.0) } else { s.normal };
        let Some((distance, nearest, _, other_normal)) = other_index.nearest_face(s.point) else {
            return Err("A surface has no faces.".into());
        };
        let other_normal = if other_flip { scale(other_normal, -1.0) } else { other_normal };
        let offset = sub(s.point, nearest);
        let signed = if dot(offset, other_normal) >= 0.0 { distance } else { -distance };
        let class = classify(normal, distance, offset, other, nearest, other_normal, rules);
        out.push(Measured { sample: *s, signed, nearest, class });
    }
    Ok(out)
}

fn area_of(measured: &[Measured], class: Class) -> f64 {
    measured.iter().filter(|m| m.class == class).map(|m| m.sample.weight).fold(0.0, |a, b| a + b)
}

fn patches(side: &Side, filter: impl Fn(&Measured) -> bool, cell: f64) -> Vec<Value> {
    let picked: Vec<&Measured> = side.measured.iter().filter(|m| filter(m)).collect();
    let samples: Vec<(P, P, f64)> = picked.iter().map(|m| (m.sample.point, m.sample.normal, m.sample.weight)).collect();
    sc::surface_regions(&samples, cell)
        .into_iter()
        .map(|group| {
            let area: f64 = group.iter().map(|&i| picked[i].sample.weight).sum();
            let worst = group
                .iter()
                .copied()
                .max_by(|&a, &b| picked[a].signed.abs().total_cmp(&picked[b].signed.abs()))
                .expect("non-empty cluster");
            let center = scale(group.iter().fold([0.0; 3], |acc, &i| add(acc, scale(picked[i].sample.point, picked[i].sample.weight))), 1.0 / area.max(1e-12));
            let normal = unit(group.iter().fold([0.0; 3], |acc, &i| add(acc, scale(picked[i].sample.normal, picked[i].sample.weight))));
            let (mut lo, mut hi) = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
            for &i in &group {
                for k in 0..3 {
                    lo[k] = lo[k].min(picked[i].sample.point[k]);
                    hi[k] = hi[k].max(picked[i].sample.point[k]);
                }
            }
            let m = picked[worst];
            json!({
                "side": side.name,
                "surface": side.surface.label,
                "area": round_to(area, 2),
                "maxDeviation": round_to(m.signed.abs(), 4),
                "signed": round_to(m.signed, 4),
                "at": r3(m.sample.point),
                "nearest": r3(m.nearest),
                "center": r3(center),
                "normal": r3(normal),
                "bounds": {"min": r3(lo), "max": r3(hi)},
            })
        })
        .collect()
}

/// A region's bounds, for comparing locations.
fn region_box(region: &Value) -> Option<(P, P)> {
    Some((sc::vec3(&region["bounds"]["min"], "min").ok()?, sc::vec3(&region["bounds"]["max"], "max").ok()?))
}

/// Whether two regions are one place: the same worst point, or boxes (padded
/// by `pad`, as a flat wall's has no volume) that mostly coincide.
fn same_location(a: &Value, b: &Value, pad: f64) -> bool {
    if a["at"] == b["at"] {
        return true;
    }
    let (Some((a_lo, a_hi)), Some((b_lo, b_hi))) = (region_box(a), region_box(b)) else { return false };
    let volume = |lo: P, hi: P| (0..3).map(|k| (hi[k] - lo[k] + 2.0 * pad).max(0.0)).product::<f64>();
    let overlap: f64 = (0..3).map(|k| (a_hi[k].min(b_hi[k]) - a_lo[k].max(b_lo[k]) + 2.0 * pad).max(0.0)).product();
    let union = volume(a_lo, a_hi) + volume(b_lo, b_hi) - overlap;
    overlap >= 0.5 * union
}

/// Fields of a region only `detail` returns: what bounds and area already say.
const REGION_DETAIL_FIELDS: [&str; 5] = ["center", "normal", "surface", "nearest", "maxDeviation"];

/// While lists are shaped: the list and side an entry's numbers came from.
const MEASURED_IN: &str = "measuredIn";

/// A region's list and side, as alsoIn names them.
fn region_tag(list: &str, region: &Value) -> Value {
    json!(format!("{list} ({})", region["side"].as_str().unwrap_or_default()))
}

/// How many regions each list shows, and which it leaves out.
struct RegionLimits {
    max_regions: usize,
    /// mm^2; `None`: [`DEFAULT_MIN_REGION_FRACTION`] of what was compared.
    min_area: Option<f64>,
    /// Compared area per side: reference, candidate.
    compared: (f64, f64),
    detail: bool,
    pad: f64,
}

impl RegionLimits {
    /// The smallest region listed for this entry's side.
    fn min_area_of(&self, region: &Value) -> f64 {
        self.min_area.unwrap_or_else(|| {
            let compared = if region["side"] == "candidate" { self.compared.1 } else { self.compared.0 };
            DEFAULT_MIN_REGION_FRACTION * compared
        })
    }

    /// Shapes the lists, in order: at default detail a location an earlier list
    /// holds is not listed again. That entry is whichever of the two has the
    /// larger extent, and alsoIn names the list and side of each other
    /// measurement of the place, the one it displaced included. In every case
    /// what falls under minRegionArea or past maxRegions is counted in
    /// `dropped`, by list.
    fn apply(&self, lists: Vec<(&'static str, Vec<Value>)>) -> (HashMap<&'static str, Vec<Value>>, Value) {
        let mut kept: Vec<(&'static str, Vec<Value>)> = Vec::new();
        let mut dropped = json!({});
        for (list, regions) in lists {
            let mut shown: Vec<Value> = Vec::new();
            let (mut left_out, mut left_out_area) = (0usize, 0.0);
            for region in regions {
                let area = region["area"].as_f64().unwrap_or(0.0);
                if !self.detail {
                    // Across lists only: two sides' patches in one list are
                    // two measurements.
                    let earlier = kept.iter_mut().flat_map(|(_, regions)| regions.iter_mut()).find(|other| same_location(other, &region, self.pad));
                    if let Some(other) = earlier {
                        let mut also = other["alsoIn"].as_array().cloned().unwrap_or_default();
                        let mut mention = |tag: Value| {
                            if !also.contains(&tag) {
                                also.push(tag);
                            }
                        };
                        let tag = region_tag(list, &region);
                        // One entry stays whole, its numbers from one
                        // measurement: the one with the larger extent. What it
                        // measured before is then named like any other.
                        if other["area"].as_f64().unwrap_or(0.0) < area {
                            mention(other[MEASURED_IN].clone());
                            *other = region;
                            other[MEASURED_IN] = tag.clone();
                        }
                        mention(tag);
                        other["alsoIn"] = json!(also);
                        continue;
                    }
                }
                if shown.len() >= self.max_regions || area < self.min_area_of(&region) {
                    left_out += 1;
                    left_out_area += area;
                    continue;
                }
                let mut region = region;
                if !self.detail {
                    region[MEASURED_IN] = region_tag(list, &region);
                }
                shown.push(region);
            }
            if left_out > 0 {
                dropped[list] = json!({"regions": left_out, "area": round_to(left_out_area, 2)});
            }
            kept.push((list, shown));
        }
        let mut out: HashMap<&'static str, Vec<Value>> = kept.into_iter().collect();
        if !self.detail {
            for region in out.values_mut().flatten() {
                if let Some(fields) = region.as_object_mut() {
                    fields.remove(MEASURED_IN);
                    REGION_DETAIL_FIELDS.iter().for_each(|key| {
                        fields.remove(*key);
                    });
                }
            }
        }
        (out, dropped)
    }
}

/// Whether more than 70% of an open surface's matched area faces against
/// the other's normals: then its normals point inward.
fn looks_flipped(surface: &Surface, measured: &[Measured]) -> bool {
    if surface.open_edges == 0 {
        return false;
    }
    let (mut against, mut total) = (0.0, 0.0);
    for m in measured.iter().filter(|m| matches!(m.class, Class::Matched | Class::FacingMismatch)) {
        total += m.sample.weight;
        if m.class == Class::FacingMismatch {
            against += m.sample.weight;
        }
    }
    total > 0.0 && against > 0.7 * total
}

const CONTRADICTED_ORANGE: crate::raster::Rgba = [235, 140, 0, 255];

/// Per-triangle signed deviation colours of `surface` against `other`.
#[allow(clippy::too_many_arguments)]
fn heat_colors(surface: &Surface, flip: bool, samples: &[Measured], other: &Surface, other_index: &MeshIndex, other_flip: bool, rules: Rules, range: f64, tolerance: f64, deadline: Instant) -> Result<Vec<crate::raster::Rgba>, String> {
    let color = |m: &Measured| match m.class {
        Class::Matched => sc::diverging(m.signed, range, tolerance),
        Class::FacingMismatch => sc::WRONG_SIDE_PURPLE,
        Class::Contradicted => CONTRADICTED_ORANGE,
        Class::Hidden | Class::PastRim | Class::Unobserved => sc::UNSUPPORTED_GREY,
    };
    let count = surface.mesh.triangles.len();
    if count <= MAX_CENTROID_QUERIES {
        let centroids: Vec<Sample> = (0..count)
            .map(|t| {
                let [a, b, c] = sc::corners(&surface.mesh, t);
                let (normal, area) = sc::face_normal_area(&surface.mesh, t);
                Sample { point: scale(add(add(a, b), c), 1.0 / 3.0), normal, tri: t, weight: area }
            })
            .collect();
        Ok(measure_side(&centroids, flip, other_index, other, other_flip, rules, deadline)?.iter().map(color).collect())
    } else {
        let mut colors = vec![sc::UNSUPPORTED_GREY; count];
        for m in samples {
            colors[m.sample.tri] = color(m);
        }
        Ok(colors)
    }
}

pub(crate) fn compare_value(state: &AppState, input: &Value) -> Result<(Value, Images), String> {
    let started = Instant::now();
    let _scope = crate::csg::bsp::deadline_scope(started + BUDGET);
    let tolerance = sc::number(input, "tolerance", design_tolerance(state, input), 1e-6, 100.0)?;
    let max_distance = sc::number(input, "maxDistance", (5.0 * tolerance).max(2.0), 1e-6, 1e5)?;
    let angle = sc::number(input, "maxNormalAngleDeg", 60.0, 0.0, 85.0)?;
    let ignore_normals = sc::flag(input, "ignoreNormals", false)?;
    let samples_per_side = sc::integer(input, "samples", DEFAULT_SAMPLES, 100, MAX_SAMPLES)?;
    let min_coverage = sc::number(input, "minCoverage", 0.95, 0.0, 1.0)?;
    let image = sc::image_wanted(input, true)?;
    let mut design = Design::new(state, input);
    let reference = design.surface(input.get("reference").ok_or("reference is required: the observed scan.")?, "reference")?;
    let candidate = design.surface(input.get("candidate").ok_or("candidate is required: the model objects.")?, "candidate")?;
    let ref_index = reference.index();
    let cand_index = candidate.index();
    let keep_to = sc::Within::parse(input)?;
    let inside = keep_to.as_ref().map(|within| (within.stats(&reference.mesh), within.stats(&candidate.mesh)));
    if let (Some(within), Some((in_reference, in_candidate))) = (&keep_to, inside) {
        // Nothing to compare is no agreement: it must not read as a pass.
        let empty: Vec<&str> = [("reference", in_reference.triangles), ("candidate", in_candidate.triangles)].iter().filter(|(_, n)| *n == 0).map(|(side, _)| *side).collect();
        if !empty.is_empty() {
            let reason = format!("Nothing inside the box: the {} has no surface within it, so nothing was compared.", empty.join(" and the "));
            let mut value = design.header();
            value["ok"] = json!(true);
            value["nothingInside"] = json!(true);
            value["within"] = within.public(json!({"reference": in_reference.public(), "candidate": in_candidate.public()}));
            value["reference"] = reference.brief();
            value["candidate"] = candidate.brief();
            value["verdict"] = json!({"matches": false, "reasons": [reason]});
            value["elapsedMs"] = json!(started.elapsed().as_millis() as u64);
            return Ok((value, Vec::new()));
        }
    }
    let ref_samples = sc::sample_surface_in(&reference.mesh, samples_per_side, keep_to.as_ref());
    let cand_samples = sc::sample_surface_in(&candidate.mesh, samples_per_side, keep_to.as_ref());
    crate::meshops::check(Some(started + BUDGET))?;
    let cos_limit = (!ignore_normals).then(|| angle.to_radians().cos());
    // Normals are taken as given: flipping them automatically would turn a
    // model matched to the wrong side of a wall into a match.
    let ref_flip = sc::flag(input, "flipReferenceNormals", false)?;
    let cand_flip = sc::flag(input, "flipCandidateNormals", false)?;
    let deadline = started + BUDGET;
    let touch = tolerance * 0.5;
    let reach = reference.bounds().map_or(max_distance, |(lo, hi)| (0.25 * norm(sub(hi, lo))).max(max_distance));
    let ref_rules = Rules { cos_limit, touch, hidden_allowed: false, max_distance, max_reach: f64::INFINITY };
    let cand_rules = Rules { cos_limit, touch, hidden_allowed: true, max_distance, max_reach: reach };
    let forward = measure_side(&ref_samples, ref_flip, &cand_index, &candidate, cand_flip, ref_rules, deadline)?;
    let backward = measure_side(&cand_samples, cand_flip, &ref_index, &reference, ref_flip, cand_rules, deadline)?;
    let mostly_against = cos_limit.is_some() && (looks_flipped(&reference, &forward) || looks_flipped(&candidate, &backward));
    let forward_side = Side { name: "reference", surface: &reference, measured: forward };
    let backward_side = Side { name: "candidate", surface: &candidate, measured: backward };
    struct Summary {
        value: Value,
        stats: Stats,
        coverage: f64,
        mismatch: f64,
        contradicted: f64,
        excluded: f64,
    }
    let summarize = |side: &Side| -> Summary {
        let total: f64 = side.measured.iter().map(|m| m.sample.weight).fold(0.0, |a, b| a + b);
        let fraction = |class: Class| area_of(&side.measured, class) / total.max(1e-12);
        // Every sample that is not explained away is scored, however far.
        let scored: Vec<(f64, f64)> = side.measured.iter().filter(|m| m.class.scored()).map(|m| (m.signed, m.sample.weight)).collect();
        let stats = Stats::of(&scored);
        let covered: f64 = side
            .measured
            .iter()
            .filter(|m| m.class == Class::Matched && m.signed.abs() <= max_distance)
            .map(|m| m.sample.weight)
            .fold(0.0, |a, b| a + b);
        let coverage = covered / total.max(1e-12);
        let (mismatch, contradicted) = (fraction(Class::FacingMismatch), fraction(Class::Contradicted));
        let excluded = fraction(Class::Hidden) + fraction(Class::PastRim) + fraction(Class::Unobserved);
        let mut value = stats.public();
        value["coverage"] = json!(round_to(coverage, 4));
        value["facingMismatch"] = json!(round_to(mismatch, 4));
        value["contradicted"] = json!(round_to(contradicted, 4));
        value["histogram"] = sc::histogram(&scored, &[tolerance * 0.5, tolerance, tolerance * 2.0, tolerance * 4.0]);
        value["withinTolerance"] = json!(round_to(scored.iter().filter(|(v, _)| v.abs() <= tolerance).map(|(_, w)| w).fold(0.0, |a, b| a + b) / total.max(1e-12), 4));
        if side.name == "reference" {
            value["uncovered"] = json!(round_to(excluded, 4));
        } else {
            value["unsupportedFraction"] = json!(round_to(excluded, 4));
            value["unsupportedArea"] = json!(round_to(excluded * total, 2));
            value["hidden"] = json!(round_to(fraction(Class::Hidden), 4));
        }
        Summary { value, stats, coverage, mismatch, contradicted, excluded }
    };
    let forward_summary = summarize(&forward_side);
    let backward_summary = summarize(&backward_side);
    let (forward_value, forward_stats, ref_coverage) = (forward_summary.value.clone(), forward_summary.stats.clone(), forward_summary.coverage);
    let (backward_value, backward_stats) = (backward_summary.value.clone(), backward_summary.stats.clone());
    let compared_area = match inside {
        Some((in_reference, in_candidate)) => in_reference.area.max(in_candidate.area),
        None => reference.area().max(candidate.area()),
    };
    let cell = (2.5 * spacing(compared_area, samples_per_side)).max(tolerance * 2.0);
    let scored_bad = |m: &Measured| m.class.scored() && (m.class != Class::Matched || m.signed.abs() > tolerance);
    let by_area = |list: &mut Vec<Value>| list.sort_by(|a, b| b["area"].as_f64().unwrap_or(0.0).total_cmp(&a["area"].as_f64().unwrap_or(0.0)));
    let mut worst = patches(&forward_side, scored_bad, cell);
    worst.extend(patches(&backward_side, scored_bad, cell));
    by_area(&mut worst);
    let uncovered = patches(&forward_side, |m| !m.class.scored(), cell);
    let unsupported = patches(&backward_side, |m| !m.class.scored(), cell);
    let mut mismatches = patches(&forward_side, |m| m.class == Class::FacingMismatch, cell);
    mismatches.extend(patches(&backward_side, |m| m.class == Class::FacingMismatch, cell));
    by_area(&mut mismatches);
    // Shape accuracy is over the whole reference (the intended feature):
    // every sample at its real distance to the model, whatever its class.
    // Matched samples alone describe only where the model already agrees.
    let reference_area: f64 = forward_side.measured.iter().map(|m| m.sample.weight).fold(0.0, |a, b| a + b);
    let everything: Vec<(f64, f64)> = forward_side.measured.iter().map(|m| (m.signed, m.sample.weight)).collect();
    let shape = Stats::of(&everything);
    let within = |m: &Measured| m.class == Class::Matched && m.signed.abs() <= tolerance;
    let shape_within = forward_side.measured.iter().filter(|m| within(m)).map(|m| m.sample.weight).fold(0.0, |a, b| a + b) / reference_area.max(1e-12);
    let matched: Vec<(f64, f64)> = forward_side.measured.iter().filter(|m| m.class == Class::Matched).map(|m| (m.signed, m.sample.weight)).collect();
    let matched_stats = Stats::of(&matched);
    let mut shape_value = shape.public();
    shape_value["withinTolerance"] = json!(round_to(shape_within, 4));
    shape_value["note"] = json!(
        "Every sample of the reference, each at its real distance to the model (unmatched, contradicted and past-the-edge ones included): the accuracy of the whole feature."
    );
    let mut matched_value = matched_stats.public();
    matched_value["areaFraction"] = json!(round_to(matched_stats.area / reference_area.max(1e-12), 4));
    matched_value["note"] = json!("Only samples with a same-facing model surface along their normal: where the model already agrees. Not the shape accuracy.");
    // Missing: reference surface the model does not reproduce within 2 x
    // tolerance, as patches (where, how big).
    let missing_cell = cell.max(tolerance * 4.0);
    let missing = |m: &Measured| !(m.class == Class::Matched && m.signed.abs() <= 2.0 * tolerance);
    let missing_area = forward_side.measured.iter().filter(|m| missing(m)).map(|m| m.sample.weight).fold(0.0, |a, b| a + b);
    let missing_regions: Vec<Value> = patches(&forward_side, missing, missing_cell)
        .into_iter()
        .map(|mut patch| {
            let area = patch["area"].as_f64().unwrap_or(0.0);
            patch["fraction"] = json!(round_to(area / reference_area.max(1e-12), 4));
            patch
        })
        .collect();
    // Which regions are listed. The fractions of a within are of what is inside it.
    let detail = sc::flag(input, "detail", false)?;
    let candidate_area: f64 = backward_side.measured.iter().map(|m| m.sample.weight).fold(0.0, |a, b| a + b);
    let limits = RegionLimits {
        max_regions: sc::integer(input, "maxRegions", if detail { DETAIL_MAX_REGIONS } else { DEFAULT_MAX_REGIONS } as usize, 0, 64)?,
        min_area: match input.get("minRegionArea").filter(|value| !value.is_null()) {
            Some(_) => Some(sc::number(input, "minRegionArea", 0.0, 0.0, 1e12)?),
            None => None,
        },
        compared: (reference_area, candidate_area),
        detail,
        pad: cell * 0.5,
    };
    let (mut listed, regions_dropped) = limits.apply(vec![
        ("worst", worst),
        ("missingRegions", missing_regions),
        ("facingMismatches", mismatches),
        ("unsupported", unsupported),
        ("uncovered", uncovered),
    ]);
    let mut take = |list: &str| listed.remove(list).unwrap_or_default();
    let (worst, missing_regions, mismatches, unsupported, uncovered) = (take("worst"), take("missingRegions"), take("facingMismatches"), take("unsupported"), take("uncovered"));
    let mut reasons = Vec::new();
    if shape.count > 0 && shape.p95 > 2.0 * tolerance {
        reasons.push(format!(
            "shape accuracy over the whole reference: p95 {} mm > 2 x tolerance (only {:.1}% within tolerance)",
            sc::short(shape.p95),
            shape_within * 100.0
        ));
    }
    if missing_area / reference_area.max(1e-12) > 1.0 - min_coverage {
        reasons.push(format!(
            "{:.1}% of the reference is not reproduced within 2 x tolerance (see missingRegions)",
            100.0 * missing_area / reference_area.max(1e-12)
        ));
    }
    if ref_coverage < min_coverage {
        reasons.push(format!("only {:.1}% of the reference has a model surface facing its way within {} mm (need {:.0}%)", ref_coverage * 100.0, sc::short(max_distance), min_coverage * 100.0));
    }
    for (name, stats) in [("referenceToCandidate", &forward_stats), ("candidateToReference", &backward_stats)] {
        if stats.count == 0 {
            reasons.push(format!("{name}: nothing to score"));
            continue;
        }
        if stats.rms > tolerance {
            reasons.push(format!("{name} rms {} mm > tolerance {}", sc::short(stats.rms), sc::short(tolerance)));
        }
        if stats.p95 > 2.0 * tolerance {
            reasons.push(format!("{name} p95 {} mm > 2 x tolerance", sc::short(stats.p95)));
        }
    }
    for (name, summary) in [("reference", &forward_summary), ("candidate", &backward_summary)] {
        if summary.mismatch > 0.02 {
            reasons.push(format!("{:.1}% of the {name} faces the wrong way against its nearest surface (wrong side of a wall or misidentified surface)", summary.mismatch * 100.0));
        }
        if summary.contradicted > 0.02 {
            reasons.push(format!(
                "{:.1}% of the {name} has no counterpart that explains it (the other surface runs across it or ends before it: a missing or extra feature)",
                summary.contradicted * 100.0
            ));
        }
    }
    if backward_summary.excluded > 0.98 {
        reasons.push("almost none of the candidate faces the reference: check that the right surfaces are compared".into());
    }
    if mostly_against {
        reasons.push(
            "most matched pairs face opposite ways: either the model follows the other side of the wall, or an open \
             surface's normals point inward (if so, and only then, pass flipReferenceNormals or flipCandidateNormals)"
                .into(),
        );
    }
    let matches = reasons.is_empty();
    let reason_count = reasons.len();
    let range = sc::number(input, "colorRange", (3.0 * tolerance).max(forward_stats.p95.max(backward_stats.p95)), 1e-6, 1e5)?;
    let mut value = design.header();
    value["ok"] = json!(true);
    value["reference"] = reference.brief();
    value["candidate"] = candidate.brief();
    if let (Some(within), Some((in_reference, in_candidate))) = (&keep_to, inside) {
        value["within"] = within.public(json!({"reference": in_reference.public(), "candidate": in_candidate.public()}));
    }
    value["tolerance"] = json!(tolerance);
    value["maxDistance"] = json!(round_to(max_distance, 4));
    // At default detail, a number that only repeats shapeAccuracy goes.
    let forward_value = if detail { forward_value } else { without_repeats(forward_value, &shape_value) };
    let shape_value = if detail { shape_value } else { pick_fields(&shape_value, &["rms", "p50", "p95", "max", "withinTolerance"]) };
    let backward_value = if detail { backward_value } else { without_fields(backward_value, &["count", "area"]) };
    value["shapeAccuracy"] = shape_value;
    if detail {
        value["matchedOnly"] = matched_value;
    }
    value["missingRegions"] = json!(missing_regions);
    value["missingFraction"] = json!(round_to(missing_area / reference_area.max(1e-12), 4));
    value["referenceToCandidate"] = forward_value;
    value["candidateToReference"] = backward_value;
    value["worst"] = json!(worst);
    value["uncovered"] = json!(uncovered);
    value["unsupported"] = json!(unsupported);
    if !mismatches.is_empty() {
        value["facingMismatches"] = json!(mismatches);
    }
    if regions_dropped.as_object().is_some_and(|lists| !lists.is_empty()) {
        value["regionsDropped"] = regions_dropped;
    }
    if ref_flip || cand_flip {
        value["normalsFlipped"] = json!({"reference": ref_flip, "candidate": cand_flip});
    }
    value["verdict"] = json!({
        "matches": matches,
        "reasons": reasons,
        "reasonCount": reason_count,
    });
    let mut images = Vec::new();
    // The same heatmaps go to the browser's activity view (design frame).
    let shown = crate::activity::collecting();
    let paint = || -> Result<_, String> {
        Ok((
            heat_colors(&reference, ref_flip, &forward_side.measured, &candidate, &cand_index, cand_flip, ref_rules, range, tolerance, deadline)?,
            heat_colors(&candidate, cand_flip, &backward_side.measured, &reference, &ref_index, ref_flip, cand_rules, range, tolerance, deadline)?,
        ))
    };
    // Colouring only for the browser is best effort: running out of time
    // there must not fail a comparison that is already complete.
    let colors = match (image.is_some(), shown) {
        (true, _) => Some(paint()?),
        (false, true) => paint().ok(),
        (false, false) => None,
    };
    // Only what was compared is coloured.
    let colors = colors.map(|(mut ref_colors, mut cand_colors)| {
        if let Some(within) = &keep_to {
            within.grey_outside(&reference.mesh, &mut ref_colors);
            within.grey_outside(&candidate.mesh, &mut cand_colors);
        }
        (ref_colors, cand_colors)
    });
    if let Some((ref_colors, cand_colors)) = &colors {
        if shown {
            // Neutral words for the browser: either surface may be any part. Grey is what
            // the comparison left out, orange a surface with no counterpart it can be
            // measured along.
            let keys = [(sc::UNSUPPORTED_GREY, "not compared"), (CONTRADICTED_ORANGE, "no matching surface"), (sc::WRONG_SIDE_PURPLE, "facing mismatch")]
                .map(|(color, text)| json!({"color": color, "text": text}));
            let legend = json!({"range": round_to(range, 4), "dead": tolerance, "signed": true, "keys": keys});
            crate::activity::offer_painted(&reference.label, &reference.mesh, ref_colors, legend.clone());
            crate::activity::offer_painted(&candidate.label, &candidate.mesh, cand_colors, legend);
        }
    }
    if let (Some(image), Some((ref_colors, cand_colors))) = (image, colors) {
        let markers: Vec<Marker> = worst
            .iter()
            .enumerate()
            .filter_map(|(i, patch)| {
                let at = sc::vec3(&patch["at"], "at").ok()?;
                Some(Marker { point: at, label: format!("{}", i + 1), color: [20, 20, 20, 255] })
            })
            .collect();
        let legend = Legend {
            title: "deviation mm".into(),
            range,
            signed: true,
            dead: tolerance,
            keys: vec![(sc::UNSUPPORTED_GREY, "not observed".into()), (CONTRADICTED_ORANGE, "unexplained".into()), (sc::WRONG_SIDE_PURPLE, "facing mismatch".into())],
        };
        // Both pictures frame both surfaces (the other invisible), so they
        // share one camera.
        let hidden = |mesh: &TriMesh| SceneObject::plain("frame", mesh, [0, 0, 0, 0]);
        let ref_scene = vec![SceneObject::painted(&reference.label, &reference.mesh, &ref_colors), hidden(&candidate.mesh)];
        let cand_scene = vec![SceneObject::painted(&candidate.label, &candidate.mesh, &cand_colors), hidden(&reference.mesh)];
        let title_ref = format!("reference {}: rms {} p95 {} cov {:.0}%", reference.label, sc::short(forward_stats.rms), sc::short(forward_stats.p95), ref_coverage * 100.0);
        let title_cand = format!(
            "candidate {}: rms {} p95 {} unsupported {:.0}%",
            candidate.label,
            sc::short(backward_stats.rms),
            sc::short(backward_stats.p95),
            value["candidateToReference"]["unsupportedFraction"].as_f64().unwrap_or(0.0) * 100.0
        );
        let ref_markers: Vec<Marker> = markers.iter().zip(&worst).filter(|(_, p)| p["side"] == "reference").map(|(m, _)| Marker { point: m.point, label: m.label.clone(), color: m.color }).collect();
        let cand_markers: Vec<Marker> = markers.iter().zip(&worst).filter(|(_, p)| p["side"] == "candidate").map(|(m, _)| Marker { point: m.point, label: m.label.clone(), color: m.color }).collect();
        let (png_ref, meta) = sc::render_scene(&ref_scene, &image, Some(&legend), &ref_markers, Some(&title_ref))?;
        let (png_cand, _) = sc::render_scene(&cand_scene, &image, Some(&legend), &cand_markers, Some(&title_cand))?;
        value["image"] = json!({"camera": meta.get("camera"), "modelArea": meta.get("model_area"), "colorRange": round_to(range, 4)});
        images.push((format!("Reference (scan) coloured by its distance to the model ({title_ref}); numbered rings = worst[k-1]."), png_ref));
        images.push((
            format!("Candidate (model) coloured by its distance to the scan ({title_cand}); grey = not observed by the scan (hidden or past its edge), orange = contradicted, numbered rings = worst[k-1]."),
            png_cand,
        ));
    }
    value["console"] = json!(design.messages());
    value["elapsedMs"] = json!(started.elapsed().as_millis() as u64);
    Ok((value, images))
}

// ---------------------------------------------------------------------------
// measure_between
// ---------------------------------------------------------------------------

enum Direction {
    Inward,
    Outward,
    Fixed(P),
}

struct Ray {
    sample: Sample,
    direction: P,
    /// (distance, kind is material, end point) of the first crossing of `to`.
    hit: Option<(f64, bool, P)>,
    /// Crossings of `from` itself before the hit.
    self_crossings: usize,
    /// A material hit whose ray runs along a wall (its midpoint closer to a
    /// surface than a quarter of its length): a lengthwise run, not a
    /// thickness. Excluded from the statistics.
    along_wall: bool,
}

pub(crate) fn between_value(state: &AppState, input: &Value) -> Result<(Value, Images), String> {
    let started = Instant::now();
    let _scope = crate::csg::bsp::deadline_scope(started + BUDGET);
    let direction = match input.get("direction") {
        None | Some(Value::Null) => Direction::Inward,
        Some(Value::String(s)) if s == "inward" => Direction::Inward,
        Some(Value::String(s)) if s == "outward" => Direction::Outward,
        Some(value @ Value::Array(_)) => {
            let d = sc::vec3(value, "direction")?;
            if norm(d) < 1e-12 {
                return Err("direction must be non-zero.".into());
            }
            Direction::Fixed(unit(d))
        }
        Some(_) => return Err("direction must be \"inward\", \"outward\" or [x, y, z].".into()),
    };
    let max_distance = sc::number(input, "maxDistance", 30.0, 1e-6, 1e5)?;
    let count = sc::integer(input, "samples", 2000, 10, 20_000)?;
    let min_hit = sc::number(input, "minHitFraction", 0.5, 0.0, 1.0)?;
    let image = sc::image_wanted(input, true)?;
    let expected = match input.get("expected") {
        None | Some(Value::Null) => None,
        Some(e) => Some((
            e.get("value").and_then(Value::as_f64).filter(|v| v.is_finite()).ok_or("expected.value must be a number of mm.")?,
            sc::number(e, "tolerance", 0.2, 0.0, 1e5)?,
        )),
    };
    let mut design = Design::new(state, input);
    let from = design.surface(input.get("from").ok_or("from is required: the surface to measure from.")?, "from")?;
    let to = design.surface(input.get("to").ok_or("to is required: the surface to measure to.")?, "to")?;
    let within = sc::Within::parse(input)?;
    let inside = within.as_ref().map(|within| within.stats(&from.mesh));
    if let (Some(within), Some(stats)) = (&within, inside) {
        if stats.triangles == 0 {
            let mut value = design.header();
            value["ok"] = json!(true);
            value["nothingInside"] = json!(true);
            value["within"] = within.public(json!({"from": stats.public()}));
            value["from"] = from.brief();
            value["to"] = to.brief();
            value["evidence"] = json!("insufficient");
            value["evidenceNote"] = json!("Nothing inside the box: the from surface has no surface within it, so nothing was measured.");
            value["hitFraction"] = json!(0.0);
            value["elapsedMs"] = json!(started.elapsed().as_millis() as u64);
            return Ok((value, Vec::new()));
        }
    }
    let to_index = to.index();
    let from_index = from.index();
    let samples = sc::sample_surface_in(&from.mesh, count, within.as_ref());
    let epsilon = 1e-4 * from.bounds().map_or(1.0, |(lo, hi)| norm(sub(hi, lo)).max(1.0));
    let deadline = started + BUDGET;
    let mut rays: Vec<Ray> = Vec::with_capacity(samples.len());
    for (i, s) in samples.iter().enumerate() {
        if i % 256 == 0 {
            crate::meshops::check(Some(deadline))?;
        }
        let d = match direction {
            Direction::Inward => scale(s.normal, -1.0),
            Direction::Outward => s.normal,
            Direction::Fixed(d) => d,
        };
        let origin = add(s.point, scale(d, epsilon));
        // First crossings only: bounded work per ray whatever the mesh.
        let hit = to_index.raycast(origin, d).filter(|h| h.distance > 0.0 && h.distance <= max_distance).map(|h| {
            (h.distance + epsilon, dot(h.normal, d) > 0.0, h.point)
        });
        let reach = hit.map_or(max_distance, |h| h.0);
        let self_crossings = usize::from(from_index.raycast(origin, d).is_some_and(|h| h.distance > epsilon && h.distance < reach - 3.0 * epsilon));
        let along_wall = hit.is_some_and(|(t, material, _)| {
            let mid = add(s.point, scale(d, t * 0.5));
            material && from_index.distance(mid).min(to_index.distance(mid)) < 0.25 * t
        });
        rays.push(Ray { sample: *s, direction: d, hit: if along_wall { None } else { hit }, self_crossings, along_wall });
    }
    crate::meshops::check(Some(started + BUDGET))?;
    let along_wall: f64 = rays.iter().filter(|r| r.along_wall).map(|r| r.sample.weight).sum();
    let total: f64 = rays.iter().filter(|r| !r.along_wall).map(|r| r.sample.weight).sum();
    let hit_area: f64 = rays.iter().filter(|r| r.hit.is_some()).map(|r| r.sample.weight).fold(0.0, |a, b| a + b);
    let hit_fraction = hit_area / total.max(1e-12);
    // Measured only when both sides are scan data as uploaded.
    let observed = from.observed && to.observed;
    let evidence = if hit_fraction < min_hit { "insufficient" } else if observed { "observed" } else { "inferred" };
    // A ray that leaves through the same one-sided scan it started on has
    // crossed the whole object: with no interior surface selected, whether
    // that span is solid material is not known.
    let through = from.scan.is_some()
        && from.scan == to.scan
        && (to.source.get("asset").and_then(Value::as_str) == to.scan.as_deref() || from.source.get("asset") == to.source.get("asset"));
    let kind_name = |material: bool| match (material, through) {
        (true, false) => "material",
        (true, true) => "through",
        (false, _) => "gap",
    };
    let mut by_kind = serde_json::Map::new();
    for material in [true, false] {
        let values: Vec<(f64, f64)> = rays.iter().filter_map(|r| r.hit.filter(|h| h.1 == material).map(|h| (h.0, r.sample.weight))).collect();
        if values.is_empty() {
            continue;
        }
        let mut stats = Stats::of(&values).public();
        stats["min"] = json!(round_to(values.iter().map(|v| v.0).fold(f64::INFINITY, f64::min), 4));
        stats["fraction"] = json!(round_to(values.iter().map(|v| v.1).sum::<f64>() / total.max(1e-12), 4));
        let p50 = stats["p50"].as_f64().unwrap_or(1.0).max(1e-3);
        stats["histogram"] = sc::histogram(&values, &[p50 * 0.5, p50 * 0.8, p50 * 0.95, p50 * 1.05, p50 * 1.25, p50 * 2.0]);
        by_kind.insert(kind_name(material).into(), stats);
    }
    let all: Vec<(f64, f64)> = rays.iter().filter_map(|r| r.hit.map(|h| (h.0, r.sample.weight))).collect();
    let cell = (3.0 * spacing(from.area(), count)).max(1e-3);
    // Patches of measured rays with their local distribution.
    let measured_points: Vec<(P, f64)> = rays.iter().filter(|r| r.hit.is_some()).map(|r| (r.sample.point, r.sample.weight)).collect();
    let measured_rays: Vec<&Ray> = rays.iter().filter(|r| r.hit.is_some()).collect();
    let patch_list: Vec<Value> = sc::cluster_points(&measured_points, cell)
        .into_iter()
        .take(12)
        .map(|group| {
            let mut values: Vec<f64> = group.iter().map(|&i| measured_rays[i].hit.expect("hit").0).collect();
            values.sort_by(f64::total_cmp);
            let area: f64 = group.iter().map(|&i| measured_rays[i].sample.weight).sum();
            let center = scale(group.iter().fold([0.0; 3], |acc, &i| add(acc, scale(measured_rays[i].sample.point, measured_rays[i].sample.weight))), 1.0 / area.max(1e-12));
            let material = group.iter().filter(|&&i| measured_rays[i].hit.expect("hit").1).count();
            json!({
                "center": r3(center),
                "area": round_to(area, 2),
                "median": round_to(values[values.len() / 2], 4),
                "min": round_to(values[0], 4),
                "max": round_to(values[values.len() - 1], 4),
                "kind": kind_name(material * 2 >= group.len()),
            })
        })
        .collect();
    let miss_points: Vec<(P, f64)> = rays.iter().filter(|r| r.hit.is_none() && !r.along_wall).map(|r| (r.sample.point, r.sample.weight)).collect();
    let misses: Vec<Value> = sc::cluster_points(&miss_points, cell)
        .into_iter()
        .take(6)
        .map(|group| {
            let area: f64 = group.iter().map(|&i| miss_points[i].1).sum();
            let center = scale(group.iter().fold([0.0; 3], |acc, &i| add(acc, scale(miss_points[i].0, miss_points[i].1))), 1.0 / area.max(1e-12));
            json!({"center": r3(center), "area": round_to(area, 2)})
        })
        .collect();
    // Example rays at the quantiles, each with every crossing on its way.
    let mut ordered: Vec<&Ray> = measured_rays.clone();
    ordered.sort_by(|a, b| a.hit.expect("hit").0.total_cmp(&b.hit.expect("hit").0));
    let mut examples = Vec::new();
    if !ordered.is_empty() {
        let mut picks: Vec<(&str, usize)> = vec![("min", 0), ("p5", ordered.len() * 5 / 100), ("median", ordered.len() / 2), ("p95", ordered.len() * 95 / 100), ("max", ordered.len() - 1)];
        picks.dedup_by_key(|p| p.1);
        for (name, i) in picks {
            let ray = ordered[i.min(ordered.len() - 1)];
            let (distance, material, end) = ray.hit.expect("hit");
            let origin = ray.sample.point;
            let mut crossings: Vec<Value> = Vec::new();
            for (label, index) in [(&from.label, &from_index), (&to.label, &to_index)] {
                for (t, _, normal) in index.raycast_all(add(origin, scale(ray.direction, epsilon)), ray.direction, max_distance) {
                    crossings.push(json!({"surface": label, "distance": round_to(t + epsilon, 4), "entering": dot(normal, ray.direction) < 0.0}));
                }
            }
            crossings.sort_by(|a, b| a["distance"].as_f64().unwrap_or(0.0).total_cmp(&b["distance"].as_f64().unwrap_or(0.0)));
            crossings.truncate(32);
            examples.push(json!({
                "at": name,
                "start": r3(origin),
                "end": r3(end),
                "direction": r3(ray.direction),
                "distance": round_to(distance, 4),
                "kind": kind_name(material),
                "crossesFromFirst": ray.self_crossings > 0,
                "crossings": crossings,
            }));
        }
    }
    let crossing_fraction = rays.iter().filter(|r| r.hit.is_some() && r.self_crossings > 0).map(|r| r.sample.weight).sum::<f64>() / total.max(1e-12);
    let mut value = design.header();
    if let (Some(within), Some(stats)) = (&within, inside) {
        value["within"] = within.public(json!({"from": stats.public()}));
    }
    value["ok"] = json!(true);
    value["from"] = from.brief();
    value["to"] = to.brief();
    value["direction"] = match direction {
        Direction::Inward => json!("inward"),
        Direction::Outward => json!("outward"),
        Direction::Fixed(d) => json!(r3(d)),
    };
    value["rays"] = json!(rays.len());
    if along_wall > 0.0 {
        value["alongWall"] = json!({
            "fraction": round_to(along_wall / (total + along_wall).max(1e-12), 4),
            "note": "Rays that ran lengthwise inside a wall (from near an edge) measure its length, not its thickness; they are left out.",
        });
    }
    value["hitFraction"] = json!(round_to(hit_fraction, 4));
    value["evidence"] = json!(evidence);
    value["evidenceNote"] = json!(match evidence {
        "insufficient" => format!(
            "Only {:.0}% of the rays reach {} within {} mm: the opposite surface is missing there (an unscanned underside?), so no distance is asserted for the misses.",
            hit_fraction * 100.0,
            to.label,
            sc::short(max_distance)
        ),
        "observed" => "Both surfaces are scan data: these distances were measured.".to_string(),
        _ => "A model surface is involved: these distances are what the model claims, not measurements of the object.".to_string(),
    });
    value["byKind"] = Value::Object(by_kind);
    value["overall"] = Stats::of(&all).public();
    value["patches"] = json!(patch_list);
    value["misses"] = json!(misses);
    value["examples"] = json!(examples);
    if through {
        value["throughNote"] = json!(
            "These rays left through the same one-sided scan they started on, so they span the whole object (kind \
             through): an upper bound on any wall, not a thickness: a one-sided scan does not show the wall's other side."
        );
    }
    if crossing_fraction > 0.01 {
        value["crossesFrom"] = json!({
            "fraction": round_to(crossing_fraction, 4),
            "note": "These rays cross the from surface again before reaching to: from folds over (a lip, a second wall) and the distance may span more than one wall.",
        });
    }
    if let Some((target, tol)) = expected {
        let within: f64 = rays.iter().filter(|r| r.hit.is_some_and(|h| (h.0 - target).abs() <= tol)).map(|r| r.sample.weight).sum();
        value["expected"] = json!({
            "value": target,
            "tolerance": tol,
            "withinFraction": round_to(within / hit_area.max(1e-12), 4),
            "passes": evidence != "insufficient" && within >= 0.95 * hit_area,
        });
    }
    let mut images = Vec::new();
    if let Some(image) = image {
        let measured = hit_area > 0.0;
        let p50 = value["overall"]["p50"].as_f64().unwrap_or(1.0).max(1e-3);
        let range = value["overall"]["max"].as_f64().unwrap_or(p50).max(p50 * 1.5);
        let mut colors = vec![sc::UNSUPPORTED_GREY; from.mesh.triangles.len()];
        let mut sums = vec![(0.0, 0.0); from.mesh.triangles.len()];
        for ray in &rays {
            if let Some(h) = ray.hit {
                sums[ray.sample.tri].0 += h.0 * ray.sample.weight;
                sums[ray.sample.tri].1 += ray.sample.weight;
            }
        }
        for (t, (sum, weight)) in sums.iter().enumerate() {
            if *weight > 0.0 {
                colors[t] = sequential(sum / weight, range);
            }
        }
        let markers: Vec<Marker> = examples
            .iter()
            .filter(|e| e["at"] == "min" || e["at"] == "max")
            .filter_map(|e| Some(Marker { point: sc::vec3(&e["start"], "start").ok()?, label: e["at"].as_str()?.to_string(), color: [20, 20, 20, 255] }))
            .collect();
        let scene = vec![SceneObject::painted(&from.label, &from.mesh, &colors), SceneObject::plain(&to.label, &to.mesh, [120, 140, 200, 70])];
        let legend = Legend { title: "distance mm".into(), range, signed: false, dead: 0.0, keys: vec![(sc::UNSUPPORTED_GREY, "no hit".into())] };
        let title = if measured {
            format!("{} -> {}: {} p50 {} hits {:.0}%", from.label, to.label, evidence, sc::short(p50), hit_fraction * 100.0)
        } else {
            format!("{} -> {}: nothing measured (no ray reached {})", from.label, to.label, to.label)
        };
        let (png, _) = if measured {
            sequential_render(&scene, &image, &legend, &markers, &title)?
        } else {
            sc::render_scene(&scene, &image, Some(&Legend { title: "distance".into(), range: 0.0, signed: false, dead: 0.0, keys: legend.keys.clone() }), &[], Some(&title))?
        };
        images.push((format!("{} coloured by the measured distance to {} (grey: no hit; {title}); rings = examples, labelled by their `at`.", from.label, to.label), png));
    }
    value["console"] = json!(design.messages());
    value["elapsedMs"] = json!(started.elapsed().as_millis() as u64);
    Ok((value, images))
}

/// Pale yellow (0) to dark red (range).
fn sequential(value: f64, range: f64) -> crate::raster::Rgba {
    let t = (value / range.max(1e-9)).clamp(0.0, 1.0);
    let a = [250.0, 240.0, 170.0];
    let b = [150.0, 20.0, 40.0];
    [
        (a[0] + (b[0] - a[0]) * t).round() as u8,
        (a[1] + (b[1] - a[1]) * t).round() as u8,
        (a[2] + (b[2] - a[2]) * t).round() as u8,
        255,
    ]
}

/// render_scene with the legend bar in the sequential palette.
fn sequential_render(scene: &[SceneObject], image: &Value, legend: &Legend, markers: &[Marker], title: &str) -> Result<(Vec<u8>, Value), String> {
    // The shared legend draws the diverging bar; for distances a swatch list
    // at the quartiles reads as well and keeps one legend renderer.
    let keys: Vec<(crate::raster::Rgba, String)> = [0.0, 0.25, 0.5, 0.75, 1.0]
        .iter()
        .map(|f| (sequential(f * legend.range, legend.range), format!("{} mm", sc::short(f * legend.range))))
        .chain(legend.keys.iter().cloned())
        .collect();
    let swatches = Legend { title: legend.title.clone(), range: 0.0, signed: false, dead: 0.0, keys };
    sc::render_scene(scene, image, Some(&swatches), markers, Some(title))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scancommon::testkit::{call, state};

    fn images(reply: &Value) -> usize {
        reply["content"].as_array().map_or(0, |c| c.iter().filter(|c| c["type"] == "image").count())
    }

    #[test]
    fn heatmaps_keep_a_wide_part_out_of_the_legend_column() {
        let (state, root) = state();
        let id = state.workspaces.create("cube([400, 40, 10]);\ntranslate([0.3, 0, 0]) cube([400, 40, 10]);\n".into(), None).unwrap().id;
        let reply = call(&state, "compare_surfaces", json!({
            "designId": id, "reference": "object-1", "candidate": "object-2", "samples": 4000,
            "image": {"view": "top", "projection": "orthographic", "width": 1024, "height": 700, "axes": false, "grid": false, "edges": false, "background": "#ffffff"},
        }));
        assert_eq!(reply["isError"], json!(false), "{reply}");
        let area = &reply["structuredContent"]["image"]["modelArea"];
        assert_eq!((area["width"].as_u64(), area["height"].as_u64()), (Some(1024), Some(700)), "{area}");
        let pictures: Vec<&Value> = reply["content"].as_array().unwrap().iter().filter(|c| c["type"] == "image").collect();
        assert_eq!(pictures.len(), 2);
        for picture in pictures {
            let png = crate::base64_decode(picture["data"].as_str().unwrap()).unwrap();
            let (w, h, rgba) = crate::raster::tests::decode_png(&png);
            assert!(w > 1024, "the legend has its own column: {w}");
            assert!(h >= 700);
            let at = |x: usize, y: usize| -> [u8; 4] { rgba[(y * w + x) * 4..(y * w + x) * 4 + 4].try_into().unwrap() };
            let painted = |p: [u8; 4]| p[..3] != [255, 255, 255];
            // The whole part (and its rings) in the model area, with room to spare.
            let columns: Vec<usize> = (0..1024).filter(|x| (0..700).any(|y| painted(at(*x, y)))).collect();
            let (first, last) = (columns[0], columns[columns.len() - 1]);
            assert!(first > 0 && last < 1023, "clipped: {first}..{last}");
            assert!(last - first > 900, "the part fills the area: {first}..{last}");
            // The column's bottom, under the legend, is bare: nothing runs in.
            assert!((1025..w).all(|x| ((h - 40)..h).all(|y| !painted(at(x, y)))));
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_angled_eye_keeps_its_camera_and_the_legend_its_column() {
        let (state, root) = state();
        let id = state.workspaces.create("cube([400, 40, 10]);\ntranslate([0.3, 0, 0]) cube([400, 40, 10]);\n".into(), None).unwrap().id;
        let reply = call(&state, "compare_surfaces", json!({
            "designId": id, "reference": "object-1", "candidate": "object-2", "samples": 4000, "detail": true,
            "image": {"eye": [200, -500, 400], "target": [200, 20, 5], "width": 800, "height": 500, "axes": false, "grid": false, "edges": false, "background": "#ffffff"},
        }));
        assert_eq!(reply["isError"], json!(false), "{reply}");
        let image = &reply["structuredContent"]["image"];
        assert_eq!(image["camera"]["view"], json!("custom"), "{image}");
        assert_eq!(image["camera"]["eye"], json!([200.0, -500.0, 400.0]), "{image}");
        assert_eq!(image["camera"]["target"], json!([200.0, 20.0, 5.0]), "{image}");
        assert_eq!((image["modelArea"]["width"].as_u64(), image["modelArea"]["height"].as_u64()), (Some(800), Some(500)), "{image}");
        for picture in reply["content"].as_array().unwrap().iter().filter(|c| c["type"] == "image") {
            let png = crate::base64_decode(picture["data"].as_str().unwrap()).unwrap();
            let (w, h, rgba) = crate::raster::tests::decode_png(&png);
            assert!(w > 800 && h >= 500);
            let at = |x: usize, y: usize| -> &[u8] { &rgba[(y * w + x) * 4..(y * w + x) * 4 + 3] };
            assert!((0..800).any(|x| (0..500).any(|y| at(x, y) != [255, 255, 255])), "the part is in the model area");
            // The legend column starts with its separator, bare below the legend.
            assert!((0..h).all(|y| at(800, y) == [190, 190, 190]));
            assert!((801..w).all(|x| ((h - 20)..h).all(|y| at(x, y) == [255, 255, 255])));
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn identical_surfaces_match_and_offsets_fail() {
        let (state, root) = state();
        let id = state.workspaces.create("cube(20);\ntranslate([0.5, 0, 0]) cube(20);\ncube(20);\n".into(), None).unwrap().id;
        let same = call(&state, "compare_surfaces", json!({"designId": id, "reference": "object-1", "candidate": "object-3", "samples": 4000}));
        assert_eq!(same["isError"], json!(false), "{same}");
        let s = &same["structuredContent"];
        assert_eq!(s["verdict"]["matches"], json!(true), "{s}");
        assert!(s["shapeAccuracy"]["max"].as_f64().unwrap() < 1e-6);
        assert_eq!(images(&same), 2);
        let shifted = call(&state, "compare_surfaces", json!({"designId": id, "reference": "object-1", "candidate": "object-2", "samples": 4000, "image": false}));
        let s = &shifted["structuredContent"];
        assert_eq!(s["verdict"]["matches"], json!(false), "{s}");
        let p95 = s["shapeAccuracy"]["p95"].as_f64().unwrap();
        assert!((p95 - 0.5).abs() < 0.05, "{p95}");
        assert!(!s["worst"].as_array().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn open_scan_leaves_unsupported_model_out_and_catches_the_wrong_side() {
        let (state, root) = state();
        // The "scan" is only the top face of the box (an open surface); the
        // model is the whole box: its other faces are unsupported, not error.
        let sheet = |z: f64, up: bool| TriMesh {
            points: vec![[0.0, 0.0, z], [20.0, 0.0, z], [20.0, 20.0, z], [0.0, 20.0, z]],
            triangles: if up { vec![[0, 1, 2], [0, 2, 3]] } else { vec![[0, 2, 1], [0, 3, 2]] },
        };
        let code = "import(\"top.stl\");\ncube([20, 20, 10]);\nimport(\"lid.stl\");\n";
        let id = state.workspaces.create(code.into(), None).unwrap().id;
        crate::scancommon::testkit::upload(&state, &id, "top.stl", &sheet(10.0, true));
        crate::scancommon::testkit::upload(&state, &id, "lid.stl", &sheet(10.05, false));
        let reply = call(&state, "compare_surfaces", json!({"designId": id, "reference": "object-1", "candidate": "object-2", "samples": 6000, "image": false}));
        let s = &reply["structuredContent"];
        assert_eq!(reply["isError"], json!(false), "{reply}");
        assert!(s["referenceToCandidate"]["coverage"].as_f64().unwrap() > 0.99, "{s}");
        let unsupported = s["candidateToReference"]["unsupportedFraction"].as_f64().unwrap();
        assert!(unsupported > 0.6, "{unsupported}");
        // Side-wall samples within half the tolerance of the scan's edge
        // touch it and count as matched; nothing farther does.
        assert!(s["candidateToReference"]["max"].as_f64().unwrap() <= 0.1 + 1e-9, "{s}");
        assert_eq!(s["verdict"]["matches"], json!(true), "{s}");
        // A model surface facing down (the underside of a lid) matched to
        // the upward scan is the wrong side, even 0.05 mm away.
        let wrong = call(&state, "compare_surfaces", json!({"designId": id, "reference": "object-1", "candidate": "object-3", "samples": 2000, "image": false}));
        let w = &wrong["structuredContent"];
        assert_eq!(w["verdict"]["matches"], json!(false), "{w}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn shape_accuracy_spans_the_whole_feature_of_an_open_scan() {
        let (state, root) = state();
        let p = |x: f64, y: f64, z: f64| [x * 20.0, y * 20.0, z * 20.0];
        // The outside of an open-topped box: the upload keeps the top open.
        let scan = TriMesh {
            points: vec![p(0., 0., 0.), p(1., 0., 0.), p(1., 1., 0.), p(0., 1., 0.), p(0., 0., 1.), p(1., 0., 1.), p(1., 1., 1.), p(0., 1., 1.)],
            triangles: vec![[0, 2, 1], [0, 3, 2], [0, 1, 5], [0, 5, 4], [1, 2, 6], [1, 6, 5], [2, 3, 7], [2, 7, 6], [3, 0, 4], [3, 4, 7]],
        };
        // The model is 6 mm short: its walls stop at z = 14.
        let code = "%import(\"box.stl\");\ncube([20, 20, 14]);\n";
        let id = state.workspaces.create(code.into(), None).unwrap().id;
        let uploaded = crate::scancommon::testkit::upload_with(&state, &id, "box.stl", &scan, json!({}));
        assert_eq!(uploaded["structuredContent"]["report"]["watertight"], json!(false), "{uploaded}");
        assert!(uploaded["structuredContent"]["report"].get("sealed").is_none(), "{uploaded}");
        let reply = call(&state, "compare_surfaces", json!({"designId": id, "reference": {"asset": "box.stl"}, "candidate": "object-1", "samples": 8000, "image": false, "detail": true}));
        let s = &reply["structuredContent"];
        assert_eq!(reply["isError"], json!(false), "{reply}");
        assert_eq!(s["reference"]["source"]["provenance"], json!("upload"), "{s}");
        assert!((s["reference"]["area"].as_f64().unwrap() - 2000.0).abs() < 1e-6, "{s}");
        // Where the model has a counterpart it agrees exactly; over the
        // whole feature it is off by up to 6 mm, and the missing band of
        // wall is listed.
        assert!(s["matchedOnly"]["p95"].as_f64().unwrap() < 1e-6, "{s}");
        assert!(s["shapeAccuracy"]["max"].as_f64().unwrap() > 4.0, "{s}");
        let missing = s["missingFraction"].as_f64().unwrap();
        assert!(missing > 0.15 && missing < 0.35, "{missing}");
        let regions = s["missingRegions"].as_array().unwrap();
        assert!(!regions.is_empty() && regions.iter().all(|r| r["center"][2].as_f64().unwrap() > 13.0), "{regions:?}");
        assert_eq!(s["verdict"]["matches"], json!(false), "{s}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// The five faces of an open-topped 40 mm box, finely meshed: a scan. It
    /// keeps every face's normal apart from its neighbours'.
    fn scanned_box(size: f64, step: f64) -> TriMesh {
        let n = (size / step).round() as usize;
        let mut mesh = TriMesh::default();
        let mut quad = |at: &dyn Fn(f64, f64) -> [f64; 3], flip: bool| {
            let base = mesh.points.len() as u32;
            for i in 0..=n {
                for j in 0..=n {
                    mesh.points.push(at(i as f64 * step, j as f64 * step));
                }
            }
            let w = n as u32 + 1;
            for i in 0..n as u32 {
                for j in 0..n as u32 {
                    let (a, b, c, d) = (base + i * w + j, base + (i + 1) * w + j, base + (i + 1) * w + j + 1, base + i * w + j + 1);
                    if flip {
                        mesh.triangles.extend([[a, c, b], [a, d, c]]);
                    } else {
                        mesh.triangles.extend([[a, b, c], [a, c, d]]);
                    }
                }
            }
        };
        quad(&|u, v| [u, v, 0.0], true);
        quad(&|u, v| [u, v, size], false);
        quad(&|u, v| [u, 0.0, v], false);
        quad(&|u, v| [u, size, v], true);
        quad(&|u, v| [0.0, u, v], true);
        quad(&|u, v| [size, u, v], false);
        mesh
    }

    /// Reply bytes as a client sees them, and the design holding a box scan
    /// against a model that misses a corner of it.
    fn corner_missing(state: &AppState) -> String {
        let code = "%import(\"box.stl\");\ndifference() { cube(40); translate([25, 25, 25]) cube(20); }\n";
        let id = state.workspaces.create(code.into(), None).unwrap().id;
        crate::scancommon::testkit::upload(state, &id, "box.stl", &scanned_box(40.0, 1.0));
        id
    }

    fn at_of(region: &Value) -> [f64; 3] {
        sc::vec3(&region["at"], "at").unwrap()
    }

    #[test]
    fn a_missing_area_over_several_faces_comes_back_one_region_per_face() {
        let (state, root) = state();
        let id = corner_missing(&state);
        let reply = call(&state, "compare_surfaces", json!({"designId": id, "reference": {"asset": "box.stl"}, "candidate": "object-1", "samples": 12000, "image": false, "maxRegions": 8, "detail": true}));
        assert_eq!(reply["isError"], json!(false), "{reply}");
        let regions = reply["structuredContent"]["missingRegions"].as_array().unwrap().clone();
        // The cut corner shows on the three faces that meet there.
        let on_wall = |axis: usize, value: f64| regions.iter().filter(|r| {
            r["bounds"]["min"][axis].as_f64().unwrap() > value - 0.6 && r["bounds"]["max"][axis].as_f64().unwrap() < value + 0.6
        }).count();
        for axis in 0..3 {
            assert!(on_wall(axis, 40.0) >= 1, "axis {axis}: {regions:?}");
        }
        for region in regions.iter().take(3) {
            let size: Vec<f64> = (0..3).map(|k| region["bounds"]["max"][k].as_f64().unwrap() - region["bounds"]["min"][k].as_f64().unwrap()).collect();
            // Tight: flat on its face, and the corner's 15 mm, not the box's 40.
            assert!(size.iter().any(|s| *s < 1.2), "{size:?}");
            assert!(size.iter().all(|s| *s < 20.0), "{size:?}");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_default_reply_is_small_and_lists_each_location_once() {
        let (state, root) = state();
        let id = corner_missing(&state);
        let arguments = |extra: Value| {
            let mut value = json!({"designId": id, "reference": {"asset": "box.stl"}, "candidate": "object-1", "samples": 12000, "image": false});
            value.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            value
        };
        let small = call(&state, "compare_surfaces", arguments(json!({})));
        let s = &small["structuredContent"];
        let bytes = s.to_string().len();
        let detailed = call(&state, "compare_surfaces", arguments(json!({"detail": true})));
        let d = &detailed["structuredContent"];
        println!("compare_surfaces reply: default {bytes} bytes; detail {} bytes (and the old form sent it twice)", d.to_string().len());
        assert!(bytes < 3500, "{bytes} bytes: {s}");
        // Sent once: the text is one line, the data structured.
        let text = small["content"][0]["text"].as_str().unwrap();
        assert!(text.len() < 300 && !text.contains('{') && text.contains("structuredContent"), "{text}");
        // Moved behind detail, and back with it.
        for list in ["worst", "missingRegions", "unsupported", "uncovered"] {
            for region in s[list].as_array().unwrap() {
                for key in ["center", "normal", "surface", "nearest", "maxDeviation"] {
                    assert!(region.get(key).is_none(), "{list}: {region}");
                }
                assert!(region["bounds"].is_object() && region["area"].is_number(), "{region}");
            }
        }
        assert!(s.get("matchedOnly").is_none() && s["verdict"].get("reasons").is_none(), "{s}");
        assert_eq!(s["verdict"]["matches"], json!(false), "{s}");
        assert!(s["verdict"]["reasonCount"].as_u64().unwrap() > 0, "{s}");
        assert!(d["matchedOnly"].is_object() && d["verdict"]["reasons"].is_array(), "{d}");
        let first = &d["worst"][0];
        for key in ["center", "normal", "surface", "nearest", "maxDeviation"] {
            assert!(first.get(key).is_some(), "{key}: {first}");
        }
        // No number repeats shapeAccuracy; one that differs stays.
        assert!(s["shapeAccuracy"]["rms"].is_number(), "{s}");
        for key in s["shapeAccuracy"].as_object().unwrap().keys() {
            if let (Some(a), Some(b)) = (s["shapeAccuracy"][key].as_f64(), s["referenceToCandidate"][key].as_f64()) {
                assert!((a - b).abs() >= 1e-3, "{key} repeats: {s}");
            }
        }
        assert!(s["referenceToCandidate"]["coverage"].is_number(), "{s}");
        // No location twice, whichever the lists.
        let lists: Vec<&Vec<Value>> = ["worst", "missingRegions", "facingMismatches", "unsupported", "uncovered"].iter().filter_map(|list| s[*list].as_array()).collect();
        assert!(lists.iter().any(|list| !list.is_empty()), "{s}");
        for (i, list) in lists.iter().enumerate() {
            for other in &lists[i + 1..] {
                for a in list.iter() {
                    for b in other.iter() {
                        assert_ne!(at_of(a), at_of(b), "{a} {b}");
                        assert!(!same_location(a, b, 0.5), "{a} {b}");
                    }
                }
            }
        }
        // The overlap is recorded, not lost.
        assert!(s.to_string().contains("alsoIn"), "{s}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_larger_later_region_keeps_both_attributions() {
        let region = |side: &str, area: f64, at: f64| {
            json!({"side": side, "area": area, "signed": at, "at": [at, 0.0, 0.0], "bounds": {"min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 1.0]}})
        };
        let limits = RegionLimits { max_regions: 8, min_area: Some(0.0), compared: (1.0, 1.0), detail: false, pad: 0.5 };
        let (listed, _) = limits.apply(vec![
            ("worst", vec![region("candidate", 1.0, 0.1)]),
            ("missingRegions", vec![region("reference", 5.0, 0.2)]),
            ("uncovered", vec![region("reference", 9.0, 0.3)]),
            ("unsupported", vec![region("candidate", 2.0, 0.4)]),
        ]);
        let worst = &listed["worst"];
        assert_eq!(worst.len(), 1, "{listed:?}");
        // The numbers of the largest, and every other measurement named once.
        assert_eq!(worst[0]["side"], json!("reference"), "{worst:?}");
        assert_eq!(worst[0]["area"], json!(9.0), "{worst:?}");
        assert_eq!(worst[0]["alsoIn"], json!(["worst (candidate)", "missingRegions (reference)", "uncovered (reference)", "unsupported (candidate)"]), "{worst:?}");
        assert!(worst[0].get(MEASURED_IN).is_none(), "{worst:?}");
        // The entry keeps its own side when it stays.
        let (listed, _) = limits.apply(vec![("worst", vec![region("candidate", 5.0, 0.1)]), ("missingRegions", vec![region("reference", 1.0, 0.2)])]);
        assert_eq!(listed["worst"][0]["side"], json!("candidate"), "{listed:?}");
        assert_eq!(listed["worst"][0]["alsoIn"], json!(["missingRegions (reference)"]), "{listed:?}");
    }

    #[test]
    fn max_regions_and_min_region_area_drop_regions_and_say_so() {
        let (state, root) = state();
        let id = corner_missing(&state);
        let arguments = |extra: Value| {
            let mut value = json!({"designId": id, "reference": {"asset": "box.stl"}, "candidate": "object-1", "samples": 12000, "image": false});
            value.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            value
        };
        let one = call(&state, "compare_surfaces", arguments(json!({"maxRegions": 1})));
        let s = &one["structuredContent"];
        assert!(s["worst"].as_array().unwrap().len() <= 1, "{s}");
        let dropped = &s["regionsDropped"];
        assert!(dropped["worst"]["regions"].as_u64().unwrap() >= 1 && dropped["worst"]["area"].as_f64().unwrap() > 0.0, "{s}");
        let all = call(&state, "compare_surfaces", arguments(json!({"maxRegions": 64, "minRegionArea": 0})));
        assert!(all["structuredContent"]["worst"].as_array().unwrap().len() >= 2, "{all}");
        let none = call(&state, "compare_surfaces", arguments(json!({"minRegionArea": 1e9})));
        let n = &none["structuredContent"];
        assert_eq!(n["worst"], json!([]), "{n}");
        assert!(n["regionsDropped"]["worst"]["regions"].as_u64().unwrap() >= 1, "{n}");
        // Bad values are refused, not guessed at.
        assert_eq!(call(&state, "compare_surfaces", arguments(json!({"minRegionArea": -1})))["isError"], json!(true));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn within_restricts_a_comparison_and_keeps_its_own_area_as_the_scale() {
        let (state, root) = state();
        let id = corner_missing(&state);
        let arguments = |within: Value| json!({"designId": id, "reference": {"asset": "box.stl"}, "candidate": "object-1", "samples": 4000, "image": false, "within": within});
        // A sound patch of the bottom face: agrees, whatever the corner does.
        let sound = call(&state, "compare_surfaces", arguments(json!({"min": [5, 5, -1], "max": [15, 15, 1]})));
        let s = &sound["structuredContent"];
        assert_eq!(sound["isError"], json!(false), "{sound}");
        assert_eq!(s["verdict"]["matches"], json!(true), "{s}");
        assert!(s["shapeAccuracy"]["max"].as_f64().unwrap() < 0.01, "{s}");
        let inside = &s["within"]["reference"];
        assert!((inside["area"].as_f64().unwrap() - 100.0).abs() < 1.0, "{s}");
        assert!((inside["areaFraction"].as_f64().unwrap() - 100.0 / 9600.0).abs() < 1e-3, "{s}");
        // Around the corner: it does not.
        let corner = call(&state, "compare_surfaces", arguments(json!({"min": [20, 20, 20], "max": [41, 41, 41]})));
        let c = &corner["structuredContent"];
        assert_eq!(c["verdict"]["matches"], json!(false), "{c}");
        // A small feature's regions are judged against the box, not the whole
        // part: 0.5% of the box here, far under 0.5% of the box's 9600 mm^2.
        let small = call(&state, "compare_surfaces", arguments(json!({"min": [24, 24, 39.5], "max": [26, 26, 40.5]})));
        let m = &small["structuredContent"];
        assert_eq!(m["within"]["reference"]["area"].as_f64().map(|a| a.round()), Some(4.0), "{m}");
        assert!(4.0 < 0.005 * 9600.0, "the feature is under the whole part's threshold");
        let on_top = m["missingRegions"].as_array().unwrap().iter().chain(m["worst"].as_array().unwrap()).any(|r| r["area"].as_f64().unwrap() < 2.0);
        assert!(on_top, "a 1 mm^2 hole in a 4 mm^2 box is listed: {m}");
        // Empty: says so, and is no pass.
        let empty = call(&state, "compare_surfaces", arguments(json!({"min": [100, 100, 100], "max": [101, 101, 101]})));
        let e = &empty["structuredContent"];
        assert_eq!(empty["isError"], json!(false), "{empty}");
        assert_eq!(e["nothingInside"], json!(true), "{e}");
        assert_eq!(e["verdict"]["matches"], json!(false), "{e}");
        assert!(empty["content"][0]["text"].as_str().unwrap().contains("nothing inside"), "{empty}");
        assert!(e.get("shapeAccuracy").is_none(), "{e}");
        // Malformed: refused with the reason.
        for (bad, says) in [
            (json!("box"), "within must be"),
            (json!({"min": [0, 0], "max": [1, 1, 1]}), "min"),
            (json!({"min": [0, 0, 0]}), "max"),
            (json!({"min": [0, 0, 0], "max": [1, "x", 1]}), "finite"),
            (json!({"min": [1, 0, 0], "max": [0, 1, 1]}), "empty box"),
            (json!({"min": [0, 0, 0], "max": [0, 1, 1]}), "empty box"),
        ] {
            let reply = call(&state, "compare_surfaces", arguments(bad.clone()));
            assert_eq!(reply["isError"], json!(true), "{bad}: {reply}");
            assert!(reply.to_string().contains(says), "{bad}: {reply}");
        }
        // measure_between keeps to the part of `from` inside.
        let measured = call(&state, "measure_between", json!({"designId": id, "from": {"asset": "box.stl"}, "to": "object-1", "direction": "outward", "samples": 500, "image": false, "within": {"min": [5, 5, -1], "max": [15, 15, 1]}}));
        assert_eq!(measured["isError"], json!(false), "{measured}");
        assert!((measured["structuredContent"]["within"]["from"]["area"].as_f64().unwrap() - 100.0).abs() < 1.0, "{measured}");
        let nothing = call(&state, "measure_between", json!({"designId": id, "from": {"asset": "box.stl"}, "to": "object-1", "image": false, "within": {"min": [100, 100, 100], "max": [101, 101, 101]}}));
        assert_eq!(nothing["structuredContent"]["nothingInside"], json!(true), "{nothing}");
        assert_eq!(nothing["structuredContent"]["evidence"], json!("insufficient"), "{nothing}");
        // compare_sections clips both contours, and an empty box says so.
        let cut = call(&state, "compare_sections", json!({"designId": id, "reference": {"asset": "box.stl"}, "candidate": "object-1", "planes": [{"axis": "z", "offset": 10}], "image": false, "within": {"min": [-1, -1, 9], "max": [20, 41, 11]}}));
        assert_eq!(cut["isError"], json!(false), "{cut}");
        let whole = call(&state, "compare_sections", json!({"designId": id, "reference": {"asset": "box.stl"}, "candidate": "object-1", "planes": [{"axis": "z", "offset": 10}], "image": false}));
        let length = |reply: &Value| reply["structuredContent"]["planes"][0]["reference"]["length"].as_f64().unwrap();
        assert!(length(&cut) < 0.7 * length(&whole), "{} vs {}", length(&cut), length(&whole));
        let none = call(&state, "compare_sections", json!({"designId": id, "reference": {"asset": "box.stl"}, "candidate": "object-1", "planes": [{"axis": "z", "offset": 10}], "image": false, "within": {"min": [100, 100, 100], "max": [101, 101, 101]}}));
        assert_eq!(none["structuredContent"]["nothingInside"], json!(true), "{none}");
        assert!(none["structuredContent"]["verdict"].as_str().unwrap().contains("Nothing inside"), "{none}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn section_profile_and_describe_geometry_keep_to_the_box() {
        let (state, root) = state();
        let id = state.workspaces.create("cube([40, 20, 10]);\n".into(), None).unwrap().id;
        let cut = |within: Value| call(&state, "section_profile", json!({"designId": id, "plane": {"axis": "z", "offset": 5}, "within": within}));
        // The rectangle is 120 mm round; the box keeps its left end, 20 + 2 x 10.
        let left = cut(json!({"min": [-1, -1, 0], "max": [10, 21, 10]}));
        assert_eq!(left["isError"], json!(false), "{left}");
        let s = &left["structuredContent"];
        assert!((s["within"]["contourLength"].as_f64().unwrap() - 40.0).abs() < 1e-6, "{s}");
        assert!((s["within"]["contourFraction"].as_f64().unwrap() - 40.0 / 120.0).abs() < 1e-3, "{s}");
        assert_eq!(s["within"]["loopsCut"], json!(1), "{s}");
        let loops = s["objects"][0]["loops"].as_array().unwrap();
        assert_eq!(loops.len(), 1, "{s}");
        assert_eq!(loops[0]["closed"], json!(false), "{s}");
        assert_eq!(s["objects"][0]["area"], json!(0.0), "a cut-open loop claims no area: {s}");
        // A box around all of it keeps the closed loop and its area.
        let all = cut(json!({"min": [-1, -1, 0], "max": [41, 21, 10]}));
        let a = &all["structuredContent"];
        assert_eq!(a["objects"][0]["loops"][0]["closed"], json!(true), "{a}");
        assert_eq!(a["objects"][0]["area"], json!(800.0), "{a}");
        assert_eq!(a["within"]["contourFraction"], json!(1.0), "{a}");
        // Nothing inside: said so.
        let none = cut(json!({"min": [100, 100, 100], "max": [101, 101, 101]}));
        assert_eq!(none["isError"], json!(false), "{none}");
        assert_eq!(none["structuredContent"]["nothingInside"], json!(true), "{none}");
        // A plane outside the box, too.
        let off = cut(json!({"min": [0, 0, 6], "max": [40, 20, 8]}));
        assert_eq!(off["structuredContent"]["nothingInside"], json!(true), "{off}");
        // Malformed: refused.
        let bad = cut(json!({"min": [0, 0, 0], "max": [0, 1, 1]}));
        assert_eq!(bad["isError"], json!(true), "{bad}");
        assert!(bad.to_string().contains("empty box"), "{bad}");

        let described = call(&state, "describe_geometry", json!({"designId": id, "within": {"min": [-1, -1, -1], "max": [41, 21, 1]}}));
        assert_eq!(described["isError"], json!(false), "{described}");
        let object = &described["structuredContent"]["objects"][0];
        assert!(object["within"]["areaFraction"].as_f64().unwrap() < 0.6, "{object}");
        let nothing = call(&state, "describe_geometry", json!({"designId": id, "within": {"min": [100, 100, 100], "max": [101, 101, 101]}}));
        assert_eq!(nothing["structuredContent"]["nothingInside"], json!(true), "{nothing}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn fits_keep_to_the_box() {
        let (state, root) = state();
        let id = corner_missing(&state);
        let surface = json!({"asset": "box.stl"});
        let curve = call(&state, "fit_curve", json!({"designId": id, "section": {"surface": surface, "plane": {"axis": "z", "offset": 10}}, "tolerance": 0.2, "image": false, "within": {"min": [-1, -1, 9], "max": [41, 1, 11]}}));
        assert_eq!(curve["isError"], json!(false), "{curve}");
        assert!(curve.to_string().contains("contourFraction"), "{curve}");
        let empty = call(&state, "fit_curve", json!({"designId": id, "section": {"surface": surface, "plane": {"axis": "z", "offset": 10}}, "image": false, "within": {"min": [100, 100, 100], "max": [101, 101, 101]}}));
        assert_eq!(empty["isError"], json!(true), "{empty}");
        assert!(empty.to_string().contains("Nothing inside"), "{empty}");
        let fit = call(&state, "fit_surface", json!({"designId": id, "surface": surface, "image": false, "within": {"min": [2, 2, -1], "max": [38, 38, 1]}}));
        assert_eq!(fit["isError"], json!(false), "{fit}");
        assert!(fit["structuredContent"]["within"]["areaFraction"].as_f64().unwrap() < 0.2, "{fit}");
        let empty = call(&state, "fit_surface", json!({"designId": id, "surface": surface, "image": false, "within": {"min": [100, 100, 100], "max": [101, 101, 101]}}));
        assert!(empty.to_string().contains("Nothing inside"), "{empty}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn removed_region_tools_are_not_listed() {
        let names: Vec<String> = crate::mcp_tool_definitions().iter().filter_map(|tool| tool["name"].as_str().map(str::to_string)).collect();
        for gone in ["select_scan_region", "list_scan_regions", "derive_asset", "crop_scan"] {
            assert!(!names.iter().any(|name| name == gone), "{gone}");
        }
        for kept in ["extract_features", "get_reverse_engineering_guide"] {
            assert!(names.iter().any(|name| name == kept), "{kept}");
        }
        let all = serde_json::to_string(&crate::mcp_tool_definitions()).unwrap();
        for gone in ["select_scan_region", "list_scan_regions", "derive_asset", "crop_scan"] {
            assert!(!all.contains(gone), "a tool's text still names {gone}");
        }
    }

    #[test]
    fn a_repeated_object_counts_once() {
        let (state, root) = state();
        let id = state.workspaces.create("cube(20);\n".into(), None).unwrap().id;
        let once = call(&state, "compare_surfaces", json!({"designId": id, "reference": "object-1", "candidate": "object-1", "samples": 200, "image": false}));
        let many = call(&state, "compare_surfaces", json!({"designId": id, "reference": vec!["object-1"; 64], "candidate": "object-1", "samples": 200, "image": false}));
        assert_eq!(many["isError"], json!(false), "{many}");
        assert_eq!(many["structuredContent"]["reference"]["triangles"], once["structuredContent"]["reference"]["triangles"], "{many}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_or_extra_features_fail_however_large() {
        let (state, root) = state();
        let code = "cube([40, 40, 10]);\nunion() { cube([40, 40, 10]); translate([18, 18, 0]) cube([4, 4, 21]); }\n";
        let id = state.workspaces.create(code.into(), None).unwrap().id;
        // The model lacks the scan's 11 mm boss, then has one the scan lacks.
        for (reference, candidate) in [("object-2", "object-1"), ("object-1", "object-2")] {
            let reply = call(&state, "compare_surfaces", json!({"designId": id, "reference": reference, "candidate": candidate, "samples": 6000, "image": false}));
            let s = &reply["structuredContent"];
            assert_eq!(s["verdict"]["matches"], json!(false), "{reference} vs {candidate}: {s}");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_correct_thin_walled_model_of_a_one_sided_scan_matches() {
        let (state, root) = state();
        let p = |x: f64, y: f64, z: f64| [x * 20.0, y * 20.0, z * 20.0];
        // The outside of an open-topped box, as scanned.
        let scan = TriMesh {
            points: vec![p(0., 0., 0.), p(1., 0., 0.), p(1., 1., 0.), p(0., 1., 0.), p(0., 0., 1.), p(1., 0., 1.), p(1., 1., 1.), p(0., 1., 1.)],
            triangles: vec![[0, 2, 1], [0, 3, 2], [0, 1, 5], [0, 5, 4], [1, 2, 6], [1, 6, 5], [2, 3, 7], [2, 7, 6], [3, 0, 4], [3, 4, 7]],
        };
        let code = "%import(\"box.stl\");\ndifference() { cube(20); translate([1.5, 1.5, 1.5]) cube([17, 17, 20]); }\n";
        let id = state.workspaces.create(code.into(), None).unwrap().id;
        crate::scancommon::testkit::upload(&state, &id, "box.stl", &scan);
        let reply = call(&state, "compare_surfaces", json!({"designId": id, "reference": {"asset": "box.stl"}, "candidate": "object-1", "samples": 8000, "image": false}));
        let s = &reply["structuredContent"];
        assert_eq!(s["verdict"]["matches"], json!(true), "{s}");
        assert!(s["candidateToReference"]["hidden"].as_f64().unwrap() > 0.3, "{s}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn thickness_between_walls_and_insufficient_evidence() {
        let (state, root) = state();
        let code = "difference() { cube([30, 30, 10]); translate([2, 2, 2]) cube([26, 26, 10]); }\nimport(\"sheet.stl\");\n";
        let id = state.workspaces.create(code.into(), None).unwrap().id;
        let sheet = TriMesh { points: vec![[5.0, 5.0, 20.0], [25.0, 5.0, 20.0], [25.0, 25.0, 20.0], [5.0, 25.0, 20.0]], triangles: vec![[0, 1, 2], [0, 2, 3]] };
        crate::scancommon::testkit::upload(&state, &id, "sheet.stl", &sheet);
        // From the box's own surface inward to itself: the walls and floor
        // read 2 mm; rays starting near an edge run along a wall and are
        // left out as lengthwise runs.
        let walls = call(&state, "measure_between", json!({"designId": id, "from": "object-1", "to": "object-1", "samples": 3000, "expected": {"value": 2.0, "tolerance": 0.01}}));
        assert_eq!(walls["isError"], json!(false), "{walls}");
        let s = &walls["structuredContent"];
        assert_eq!(s["evidence"], json!("inferred"), "{s}");
        let p50 = s["byKind"]["material"]["p50"].as_f64().unwrap();
        assert!((p50 - 2.0).abs() < 1e-3, "{p50}");
        assert!(s["expected"]["withinFraction"].as_f64().unwrap() > 0.97, "{s}");
        assert!(s["alongWall"]["fraction"].as_f64().unwrap() > 0.05, "{s}");
        assert!(s.get("crossesFrom").is_none(), "{s}");
        assert!(!s["examples"].as_array().unwrap().is_empty());
        assert_eq!(images(&walls), 1);
        // A sheet floating over the cavity, measured straight down: an air
        // gap of 18 mm to the floor (a recess depth).
        let gap = call(&state, "measure_between", json!({"designId": id, "from": "object-2", "to": "object-1", "direction": [0, 0, -1], "samples": 2000, "image": false}));
        let g = &gap["structuredContent"];
        assert!((g["byKind"]["gap"]["p50"].as_f64().unwrap() - 18.0).abs() < 1e-3, "{g}");
        // Upward from the sheet nothing is there: insufficient evidence.
        let none = call(&state, "measure_between", json!({"designId": id, "from": "object-2", "to": "object-1", "direction": [0, 0, 1], "image": false}));
        assert_eq!(none["structuredContent"]["evidence"], json!("insufficient"), "{none}");
        let _ = std::fs::remove_dir_all(root);
    }
}
