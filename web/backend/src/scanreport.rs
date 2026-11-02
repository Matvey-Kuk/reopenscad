//! `scan_report`: one call that checks a reconstruction against its scan end
//! to end: surface deviation per feature box (heatmaps), shared-scale section
//! overlays, and thickness or depth evidence, rolled into one verdict. It
//! composes compare_surfaces, compare_sections and measure_between.

use crate::scancommon as sc;
use crate::{sectioncompare, surfacecompare, AppState};
use serde_json::{json, Value};
use std::time::Instant;

/// Most feature boxes one report compares (each costs a full comparison).
const MAX_FEATURES: usize = 6;
const MAX_THICKNESS: usize = 6;

pub(crate) fn tool_definitions() -> Vec<Value> {
    let within = sc::within_schema("design coordinates, the scan as scanObject poses it");
    let surface = json!({"description": "An asset name (posed by scanObject), or a surface as in compare_surfaces.", "anyOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}, {"type": "object"}]});
    vec![crate::mcp_tool(
        "scan_report",
        "One report checking a reconstruction against its scan: (1) compare_surfaces per features entry \
         (default the whole scan, or the top-level within); (2) compare_sections on shared-scale sheets \
         (default 4 planes across the longest horizontal extent, or sections); (3) measure_between per \
         thickness entry. verdict.matches only when every comparison, section plane and expected \
         thickness passes, and an empty box fails it; verified is false while any thickness has \
         insufficient or only inferred evidence (listed in unverified), so an unmeasured wall is never \
         reported as confirmed. Parts of a scan that are not the object (foam, a fixture, the table) \
         count unless within leaves them out. A closed, manifold model that does not follow the scan \
         fails here.",
        json!({
            "type": "object",
            "required": ["designId", "scan", "candidate"],
            "properties": {
                "designId": {"type": "string"},
                "scan": {"type": "string", "description": "The scan asset, e.g. \"vent.stl\"."},
                "scanObject": {"type": "string", "description": "The design object that imports the scan (poses it)."},
                "within": within.clone(),
                "candidate": surface.clone(),
                "features": {"type": "array", "maxItems": MAX_FEATURES, "items": {"type": "object", "required": ["within"], "properties": {"label": {"type": "string"}, "within": within.clone()}}, "description": "Boxes to compare the candidate in, one result each."},
                "sections": {"type": "object", "description": "{planes} or {sweep} as in compare_sections; reference (default: the scan); within (default: the top-level within)."},
                "thickness": {"type": "array", "maxItems": MAX_THICKNESS, "items": {"type": "object", "required": ["from", "to"], "properties": {
                    "label": {"type": "string"},
                    "from": surface.clone(),
                    "to": surface,
                    "direction": {"anyOf": [{"type": "string", "enum": ["inward", "outward"]}, {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3}]},
                    "expected": {"type": "object", "required": ["value"], "properties": {"value": {"type": "number"}, "tolerance": {"type": "number", "minimum": 0}}},
                    "maxDistance": {"type": "number", "exclusiveMinimum": 0},
                    "within": within.clone()
                }}},
                "tolerance": {"type": "number", "exclusiveMinimum": 0},
                "image": {"description": "true (default), false, or render_image options.", "anyOf": [{"type": "boolean"}, {"type": "object"}]},
                "detail": sc::detail_schema()
            }
        }),
    )]
}

pub(crate) fn mcp_dispatch(state: &AppState, name: &str, arguments: &Value) -> Value {
    if name != "scan_report" {
        return crate::mcp_tool_error(&format!("Unknown tool {name:?}."));
    }
    match report_value(state, arguments) {
        Ok((value, images)) => sc::image_result(sc::compact(value, arguments), images),
        Err(error) => crate::mcp_tool_error(&error),
    }
}

/// The base arguments every composed call shares.
fn base(input: &Value) -> Value {
    let mut out = json!({});
    for key in ["designId", "workspaceId", "tolerance"] {
        if let Some(value) = input.get(key).filter(|v| !v.is_null()) {
            out[key] = value.clone();
        }
    }
    // The tool bodies read the design under either name.
    if let Some(id) = crate::input_design_id(input) {
        out["designId"] = json!(id);
        out["workspaceId"] = json!(id);
    }
    out
}

fn with(base: &Value, extra: Value) -> Value {
    let mut out = base.clone();
    if let (Some(out), Some(extra)) = (out.as_object_mut(), extra.as_object()) {
        out.extend(extra.clone());
    }
    out
}

/// An asset name becomes a posed asset surface; anything else passes.
fn surface_arg(value: &Value, assets: &[String], scan_object: Option<&str>) -> Value {
    match value.as_str() {
        Some(name) if assets.iter().any(|a| a == name) => match scan_object {
            Some(object) => json!({"asset": name, "frameOf": object}),
            None => json!({"asset": name}),
        },
        _ => value.clone(),
    }
}

fn pick(value: &Value, keys: &[&str]) -> Value {
    let mut out = json!({});
    for key in keys {
        if let Some(v) = value.get(*key) {
            out[*key] = v.clone();
        }
    }
    out
}

fn report_value(state: &AppState, input: &Value) -> Result<(Value, Vec<(String, Vec<u8>)>), String> {
    let started = Instant::now();
    // One budget for the whole report; each composed call's own deadline is
    // nested inside it (the earlier one wins).
    let _scope = crate::csg::bsp::deadline_scope(started + std::time::Duration::from_secs(150));
    let id = crate::argument_workspace_id(input)?;
    let scan = input.get("scan").and_then(Value::as_str).ok_or("scan is required: the scan asset name, e.g. \"vent.stl\".")?;
    let candidate = input.get("candidate").cloned().ok_or("candidate is required: the model objects.")?;
    let scan_object = input.get("scanObject").and_then(Value::as_str);
    let image = sc::image_wanted(input, true)?;
    let base = base(input);
    let metas = state.workspaces.asset_list(id)?;
    if !metas.iter().any(|meta| meta.name == scan) {
        return Err(crate::assets::no_asset_message(scan));
    }
    let assets: Vec<String> = metas.iter().map(|meta| meta.name.clone()).collect();
    let mut images = Vec::new();
    let mut reasons: Vec<String> = Vec::new();
    // What could not be checked at all (no evidence), as opposed to what
    // was checked and disagrees.
    let mut gaps: Vec<String> = Vec::new();
    let mut verified = true;

    // 1. Surface deviation per feature box (default the whole scan, or within).
    let within = sc::Within::parse(input)?;
    let mut features: Vec<(String, Option<Value>)> = Vec::new();
    for (i, feature) in input.get("features").and_then(Value::as_array).into_iter().flatten().take(MAX_FEATURES).enumerate() {
        sc::Within::parse(feature)?.ok_or_else(|| format!("features[{i}] needs a within box."))?;
        let label = feature.get("label").and_then(Value::as_str).map_or_else(|| format!("feature {}", i + 1), str::to_string);
        features.push((label, feature.get("within").cloned()));
    }
    if features.is_empty() {
        let whole = if within.is_some() { "inside the box" } else { "whole scan" };
        features.push((whole.to_string(), input.get("within").cloned()));
    }
    let mut surfaces = Vec::new();
    for (i, (label, feature_box)) in features.iter().enumerate() {
        let reference = surface_arg(&json!(scan), &assets, scan_object);
        let mut args = with(&base, json!({"reference": reference, "candidate": candidate.clone(), "image": image.clone().map_or(json!(false), |v| v)}));
        if let Some(feature_box) = feature_box {
            args["within"] = feature_box.clone();
        }
        if input.get("detail").and_then(Value::as_bool) == Some(true) {
            args["detail"] = json!(true);
        }
        let (value, mut pictures) = surfacecompare::compare_value(state, &args).map_err(|error| format!("compare_surfaces of {label}: {error}"))?;
        // The scan-side heatmap for every feature; the model-side one once.
        if i > 0 && pictures.len() > 1 {
            pictures.truncate(1);
        }
        images.extend(pictures.into_iter().map(|(caption, png)| (format!("{label}: {caption}"), png)));
        if value["verdict"]["matches"] != json!(true) {
            for reason in value["verdict"]["reasons"].as_array().into_iter().flatten() {
                reasons.push(format!("{label}: {}", reason.as_str().unwrap_or_default()));
            }
        }
        let mut entry = json!({
            "feature": label,
            "within": value.get("within"),
            "nothingInside": value.get("nothingInside"),
            "matches": value["verdict"]["matches"],
            "shapeAccuracy": pick(&value["shapeAccuracy"], &["rms", "p95", "max", "withinTolerance"]),
            "missingFraction": value["missingFraction"],
            "missingRegions": value["missingRegions"].as_array().map(|m| m.iter().take(3).map(|p| pick(p, &["area", "fraction", "signed", "at", "bounds", "alsoIn"])).collect::<Vec<_>>()),
            "referenceToCandidate": pick(&value["referenceToCandidate"], &["rms", "p95", "max", "signedMean", "coverage", "facingMismatch", "uncovered"]),
            "candidateToReference": pick(&value["candidateToReference"], &["rms", "p95", "max", "signedMean", "unsupportedFraction", "facingMismatch"]),
            "worst": value["worst"].as_array().map(|w| w.iter().take(3).cloned().collect::<Vec<_>>()),
        });
        // Only compare_surfaces with detail has this.
        if value.get("matchedOnly").is_some() {
            entry["matchedOnly"] = pick(&value["matchedOnly"], &["rms", "p95", "max", "areaFraction"]);
        }
        surfaces.push(entry);
    }

    // 2. Sections.
    let mut sections_input = input.get("sections").cloned().unwrap_or(json!({}));
    if !sections_input.is_object() {
        return Err("sections must be {planes} or {sweep} as in compare_sections.".into());
    }
    if sections_input.get("planes").is_none() && sections_input.get("sweep").is_none() {
        let reference = surface_arg(&json!(scan), &assets, scan_object);
        let mut design = sc::Design::new(state, &base);
        let surface = design.surface(&reference, "scan")?;
        let (lo, hi) = surface.bounds().ok_or("The scan has no triangles.")?;
        // Inside the box the sections use, when there is one: planes elsewhere
        // would see nothing.
        let section_box = match sections_input.get("within") {
            Some(_) => sc::Within::parse(&sections_input)?,
            None => within,
        };
        let (lo, hi) = section_box.map_or((lo, hi), |within| ([0, 1, 2].map(|k| lo[k].max(within.min[k])), [0, 1, 2].map(|k| hi[k].min(within.max[k]))));
        let axis = if hi[0] - lo[0] >= hi[1] - lo[1] { 0 } else { 1 };
        let span = hi[axis] - lo[axis];
        let axis_name = ["x", "y"][axis];
        sections_input["sweep"] = json!({"axis": axis_name, "from": lo[axis] + 0.15 * span, "to": hi[axis] - 0.15 * span, "count": 4});
    }
    let section_reference = match sections_input.get("reference") {
        Some(reference) => surface_arg(reference, &assets, scan_object),
        None => surface_arg(&json!(scan), &assets, scan_object),
    };
    let mut section_args = with(&base, json!({
        "reference": section_reference,
        "candidate": candidate.clone(),
        "image": image.is_some(),
    }));
    if let Some(value) = input.get("within").filter(|value| !value.is_null()) {
        section_args["within"] = value.clone();
    }
    for key in ["planes", "sweep", "maxDistance", "markers", "within"] {
        if let Some(value) = sections_input.get(key) {
            section_args[key] = value.clone();
        }
    }
    let (sections, section_images) = sectioncompare::compare_sections(state, &section_args).map_err(|error| format!("compare_sections: {error}"))?;
    images.extend(section_images);
    let planes: Vec<Value> = sections["planes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|plane| {
            json!({
                "plane": pick(&plane["plane"], &["point", "normal"]),
                "passes": plane["passes"],
                "shapeAccuracy": pick(&plane["shapeAccuracy"], &["rms", "p95", "max", "withinTolerance", "missingLength"]),
                "referenceToCandidate": pick(&plane["referenceToCandidate"], &["rms", "p95", "max", "coverage"]),
                "candidateToReference": pick(&plane["candidateToReference"], &["rms", "p95", "max", "supportedFraction"]),
                "note": plane.get("note"),
            })
        })
        .collect();
    let failing_planes = planes.iter().filter(|p| p["passes"] != json!(true)).count();
    if failing_planes > 0 {
        reasons.push(format!("{failing_planes} of {} section planes deviate beyond tolerance (see the overlay sheet)", planes.len()));
    }
    let skipped_planes: Vec<Value> = sections["skipped"].as_array().into_iter().flatten().filter(|s| s.get("plane").is_some()).cloned().collect();
    if let Some(gap) = unfinished_sections(&sections, planes.len()) {
        verified = false;
        gaps.push(gap);
    }

    // 3. Thickness and depth evidence.
    let mut thickness = Vec::new();
    for (i, entry) in input.get("thickness").and_then(Value::as_array).into_iter().flatten().take(MAX_THICKNESS).enumerate() {
        let label = entry.get("label").and_then(Value::as_str).map_or_else(|| format!("thickness {}", i + 1), str::to_string);
        let mut args = with(&base, json!({
            "from": surface_arg(entry.get("from").unwrap_or(&Value::Null), &assets, scan_object),
            "to": surface_arg(entry.get("to").unwrap_or(&Value::Null), &assets, scan_object),
            "image": image.clone().map_or(json!(false), |v| v),
        }));
        for key in ["direction", "expected", "maxDistance", "within"] {
            if let Some(value) = entry.get(key) {
                args[key] = value.clone();
            }
        }
        let (value, pictures) = surfacecompare::between_value(state, &args).map_err(|error| format!("{label}: {error}"))?;
        images.extend(pictures.into_iter().map(|(caption, png)| (format!("{label}: {caption}"), png)));
        let evidence = value["evidence"].as_str().unwrap_or_default().to_string();
        if evidence != "observed" {
            // Insufficient: nothing measured; inferred: only what the model
            // claims. Neither confirms the object.
            verified = false;
            gaps.push(format!("{label}: {evidence} evidence ({})", value["evidenceNote"].as_str().unwrap_or_default()));
        }
        if evidence != "insufficient" && value["expected"]["passes"] == json!(false) {
            reasons.push(format!(
                "{label}: only {:.0}% of the measurements are within {} ± {} mm",
                value["expected"]["withinFraction"].as_f64().unwrap_or(0.0) * 100.0,
                value["expected"]["value"],
                value["expected"]["tolerance"]
            ));
        }
        thickness.push(json!({
            "label": label,
            "evidence": evidence,
            "evidenceNote": value["evidenceNote"],
            "hitFraction": value["hitFraction"],
            "byKind": value["byKind"].as_object().map(|kinds| kinds.iter().map(|(k, v)| (k.clone(), pick(v, &["p50", "p95", "min", "max", "fraction"]))).collect::<serde_json::Map<_, _>>()),
            "expected": value.get("expected"),
            "misses": value["misses"].as_array().map(|m| m.iter().take(3).cloned().collect::<Vec<_>>()),
        }));
    }

    let matches = reasons.is_empty();
    let value = json!({
        "ok": true,
        "designId": id,
        "scan": scan,
        "scanObject": scan_object,
        "candidate": candidate,
        "surfaces": surfaces,
        "sections": {"summary": sections["summary"], "verdict": sections["verdict"], "scale": sections["scale"], "planes": planes, "complete": sections["complete"].as_bool().unwrap_or(true), "skipped": skipped_planes},
        "thickness": thickness,
        "verdict": {
            "matches": matches,
            "verified": verified && matches,
            "reasons": reasons,
            "unverified": gaps,
            "warnings": [],
            "note": "matches: everything that was measured agrees within tolerance; verified: matches, and every thickness had evidence (unverified lists what could not be measured).",
        },
        "images": images.iter().map(|(caption, _)| caption.clone()).collect::<Vec<_>>(),
        "elapsedMs": started.elapsed().as_millis() as u64,
    });
    Ok((value, images))
}

/// Planes compare_sections did not get to (time budget) were never judged:
/// a report with any cannot be verified, whatever the others say.
fn unfinished_sections(sections: &Value, compared: usize) -> Option<String> {
    let skipped: Vec<&Value> = sections["skipped"].as_array().into_iter().flatten().filter(|s| s.get("plane").is_some()).collect();
    // `complete: false` also follows tiles that were not drawn; only a plane
    // that was not compared leaves a check undone.
    (!skipped.is_empty()).then(|| {
        format!(
            "sections: {} of {} planes were not compared ({})",
            skipped.len(),
            compared + skipped.len(),
            skipped.first().and_then(|s| s["reason"].as_str()).unwrap_or("incomplete")
        )
    })
}

#[cfg(test)]
mod tests {
    use crate::assets::TriMesh;
    use crate::scancommon::testkit::{call, state};
    use serde_json::json;

    #[test]
    fn unfinished_section_comparisons_are_unverified() {
        let partial = json!({"complete": false, "skipped": [{"plane": {"label": "x=3"}, "reason": "time budget spent"}, {"image": "tiles not drawn"}]});
        // Tiles that were not drawn leave nothing unchecked.
        assert!(super::unfinished_sections(&json!({"complete": false, "skipped": [{"image": "tiles not drawn"}]}), 4).is_none());
        let gap = super::unfinished_sections(&partial, 3).expect("unverified");
        assert!(gap.contains("1 of 4 planes") && gap.contains("time budget"), "{gap}");
        assert!(super::unfinished_sections(&json!({"skipped": [{"image": "tiles not drawn"}]}), 4).is_none());
        assert!(super::unfinished_sections(&json!({}), 4).is_none());
    }

    /// Five faces of a 20 mm cube (no top), outward normals: a scan that saw
    /// the outside but not the inside.
    fn open_box() -> TriMesh {
        let p = |x: f64, y: f64, z: f64| [x * 20.0, y * 20.0, z * 20.0];
        let points = vec![p(0., 0., 0.), p(1., 0., 0.), p(1., 1., 0.), p(0., 1., 0.), p(0., 0., 1.), p(1., 0., 1.), p(1., 1., 1.), p(0., 1., 1.)];
        let triangles = vec![[0, 2, 1], [0, 3, 2], [0, 1, 5], [0, 5, 4], [1, 2, 6], [1, 6, 5], [2, 3, 7], [2, 7, 6], [3, 0, 4], [3, 4, 7]];
        TriMesh { points, triangles }
    }

    #[test]
    fn one_report_passes_the_right_model_and_fails_a_manifold_wrong_one() {
        let (state, root) = state();
        let id = state.workspaces.create("size = 20;\n%import(\"box.stl\");\ncube(size);\n".into(), None).unwrap().id;
        crate::scancommon::testkit::upload(&state, &id, "box.stl", &open_box());
        let args = json!({
            "designId": id, "scan": "box.stl", "scanObject": "background-1", "candidate": "object-1",
            "sections": {"planes": [{"axis": "z", "offset": 10}, {"axis": "x", "offset": 10}]},
            "thickness": [
                {"label": "across the box", "from": "box.stl", "to": "box.stl", "direction": "inward"},
                {"label": "outside", "from": "box.stl", "to": "box.stl", "direction": "outward"}
            ]
        });
        let reply = call(&state, "scan_report", args.clone());
        assert_eq!(reply["isError"], json!(false), "{reply}");
        let s = &reply["structuredContent"];
        assert_eq!(s["surfaces"][0]["matches"], json!(true), "{s}");
        assert_eq!(s["sections"]["planes"].as_array().unwrap().len(), 2);
        assert_eq!(s["thickness"][0]["evidence"], json!("observed"), "{s}");
        // Across a one-sided scan of a box: the whole object, not a wall.
        assert!(s["thickness"][0]["byKind"].get("through").is_some(), "{s}");
        assert_eq!(s["thickness"][1]["evidence"], json!("insufficient"), "{s}");
        // Everything agrees, but one thickness has no evidence: not verified.
        assert_eq!(s["verdict"]["matches"], json!(true), "{s}");
        assert_eq!(s["verdict"]["verified"], json!(false), "{s}");
        assert_eq!(s["verdict"]["unverified"].as_array().unwrap().len(), 1, "{s}");
        let images = reply["content"].as_array().unwrap().iter().filter(|c| c["type"] == "image").count();
        assert!(images >= 5, "{images}");
        // A closed, manifold cube of the wrong size fails the same report.
        let mut wrong = args.clone();
        crate::scancommon::testkit::save_revision(&state, Some(wrong["designId"].as_str().unwrap()), None, Some(&json!({"size": 21})));
        wrong["image"] = json!(false);
        wrong["thickness"] = json!([]);
        let reply = call(&state, "scan_report", wrong);
        let w = &reply["structuredContent"];
        assert_eq!(w["verdict"]["matches"], json!(false), "{w}");
        assert_eq!(w["surfaces"][0]["matches"], json!(false), "{w}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_auto_sweep_stays_inside_the_sections_box() {
        let (state, root) = state();
        let id = state.workspaces.create("%import(\"box.stl\");\ncube(20);\n".into(), None).unwrap().id;
        crate::scancommon::testkit::upload(&state, &id, "box.stl", &open_box());
        let offsets = |sections: serde_json::Value| {
            let reply = call(&state, "scan_report", json!({
                "designId": id, "scan": "box.stl", "scanObject": "background-1", "candidate": "object-1", "image": false,
                "within": {"min": [-1, -1, -1], "max": [21, 21, 21]}, "sections": sections
            }));
            assert_eq!(reply["isError"], json!(false), "{reply}");
            let planes = reply["structuredContent"]["sections"]["planes"].as_array().unwrap().clone();
            assert_eq!(planes.len(), 4, "{reply}");
            planes.iter().map(|p| p["plane"]["point"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect::<Vec<f64>>()).collect::<Vec<_>>()
        };
        // Longer along y in the box: the sweep follows that axis, inside it.
        let boxed = offsets(json!({"within": {"min": [1, 12, -1], "max": [3, 18, 21]}}));
        for point in &boxed {
            assert!((12.0..=18.0).contains(&point[1]), "{boxed:?}");
        }
        // Without one, the top-level box decides: the whole scan here.
        let whole = offsets(json!({}));
        assert!(whole.iter().any(|point| point[0] < 12.0) && whole.iter().any(|point| point[0] > 12.0), "{whole:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn feature_boxes_replace_labelled_regions() {
        let (state, root) = state();
        // Right where z < 15, 6 mm too short above it.
        let id = state.workspaces.create("%import(\"box.stl\");\ncube([20, 20, 14]);\n".into(), None).unwrap().id;
        crate::scancommon::testkit::upload(&state, &id, "box.stl", &open_box());
        let surface = |boxes: serde_json::Value| json!({
            "designId": id, "scan": "box.stl", "scanObject": "background-1", "candidate": "object-1", "image": false,
            "sections": {"planes": [{"axis": "z", "offset": 5}]}, "features": boxes
        });
        let reply = call(&state, "scan_report", surface(json!([
            {"label": "bottom", "within": {"min": [-1, -1, -1], "max": [21, 21, 10]}},
            {"label": "top", "within": {"min": [-1, -1, 15], "max": [21, 21, 21]}},
            {"label": "air", "within": {"min": [100, 100, 100], "max": [101, 101, 101]}}
        ])));
        assert_eq!(reply["isError"], json!(false), "{reply}");
        let s = &reply["structuredContent"];
        assert!(s.get("regions").is_none(), "{s}");
        assert_eq!(s["surfaces"][0]["feature"], json!("bottom"), "{s}");
        assert_eq!(s["surfaces"][0]["matches"], json!(true), "{s}");
        assert_eq!(s["surfaces"][1]["matches"], json!(false), "{s}");
        // Nothing inside fails the verdict rather than passing it.
        assert_eq!(s["surfaces"][2]["nothingInside"], json!(true), "{s}");
        assert_eq!(s["verdict"]["matches"], json!(false), "{s}");
        assert!(s["verdict"]["reasons"].to_string().contains("air: Nothing inside"), "{s}");
        // detail reaches the compare_surfaces step, which alone produces matchedOnly.
        assert!(s["surfaces"][0].get("matchedOnly").is_none(), "{s}");
        let mut detailed = surface(json!([{"label": "bottom", "within": {"min": [-1, -1, -1], "max": [21, 21, 10]}}]));
        detailed["detail"] = json!(true);
        let reply = call(&state, "scan_report", detailed);
        assert_eq!(reply["isError"], json!(false), "{reply}");
        assert!(reply["structuredContent"]["surfaces"][0]["matchedOnly"]["rms"].is_number(), "{reply}");
        let bad = call(&state, "scan_report", surface(json!([{"label": "no box"}])));
        assert_eq!(bad["isError"], json!(true), "{bad}");
        let _ = std::fs::remove_dir_all(root);
    }
}
