//! `get_reverse_engineering_guide`: the one home of the instructions for
//! turning an uploaded STL into a real SCAD part. Upload tools, the render
//! note and the server instructions only point here; tool descriptions say
//! what each tool does.

use serde_json::{json, Value};

const GUIDE: &str = "An uploaded STL is a temporary reference: it is deleted 3 hours after upload, and designs still importing it stop rendering. Rebuild it as real SCAD, then remove it.

1. Ask the user what precision they need. Plan the features, then do one at a time: model, compare, correct. Don't wait for approval between steps.
2. Colours: give the STL and the reconstruction different colours with color(). No violet.
3. Keep context small: few targeted measurements, no coordinate dumps. Pass within {min, max} to limit tools that take it to one feature; extract_features suggests boxes.
4. Pose first: describe_geometry or fit_axis, then align_to, place.
5. Measure: section_profile, probe, fit_curve, fit_surface. Round fitted values to design intent (2.03 mm is 2 mm).
6. Compare each feature: compare_surfaces, compare_sections, measure_between. Look from different angles (image eye), so you don't over-optimize for one view. Being manifold says nothing about matching the object. Run scan_report once, at the end.
7. Final quality control, once the main shape is done: walk through every possible feature, even ones that seem simple and naive. Section all corners and curves thoroughly, with each within box as small as possible, and look for smaller details: chamfers, small ornament, symmetry, round shapes. This is the one step to measure a lot; small boxes keep each reply short.
8. Then run sub-agents, if you can, to keep looking for insights: give each one a different part of the model and have it focus on that part alone.
9. Ask the user about scan gaps and weird parts you can't figure out; keep working meanwhile. Ask them to pin a comment on the 3D view if something looks off, and read those with list_feedback.
10. Finish: remove the import, call delete_asset, render to confirm.";

/// The reply of `get_reverse_engineering_guide`.
pub(crate) fn value() -> Value {
    json!({"ok": true, "guide": GUIDE})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guide_holds_every_required_point() {
        let reply = value();
        let guide = reply["guide"].as_str().unwrap();
        for needle in [
            "3 hours", "what precision", "one at a time", "Don't wait for approval", "different colours with color()", "No violet", "no coordinate dumps", "within {min, max}",
            "extract_features", "align_to", "fit_surface", "design intent", "compare_surfaces", "scan_report", "Final quality control", "as small as possible", "sub-agents", "different angles", "chamfers", "scan gaps", "weird parts", "pin a comment", "list_feedback", "delete_asset",
        ] {
            assert!(guide.contains(needle), "the guide is missing {needle:?}");
        }
        for gone in ["select_scan_region", "list_scan_regions", "crop_scan", "derive_asset"] {
            assert!(!guide.contains(gone), "{gone}: {guide}");
        }
    }
}
