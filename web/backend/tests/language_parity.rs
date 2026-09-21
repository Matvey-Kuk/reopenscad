//! The language features this engine does not implement yet, held to OpenSCAD.
//!
//! Every case here is a `.scad` source that the OpenSCAD binary renders to a
//! closed solid, checked in beside the binary STL it produced. They were chosen
//! from `tests/LANGUAGE_COVERAGE.md` — the audit of what this engine is missing
//! — so the corpus is a *specification of the gap*, not a record of what
//! already works. When this file is green the gap is closed.
//!
//! Two groups, and they fail for different reasons:
//!
//! * **Modules** — `hull()` over anything but boxes, `polyhedron`, `resize`,
//!   `multmatrix`, `projection`, `render`. These are hard errors today: the
//!   compile aborts and nothing is produced.
//! * **Semantics** — `-2^2`, `str()` precision, vector comparison and addition,
//!   `each` over a string. These are worse, because they *succeed* and return a
//!   different number. Each one is written so the wrong answer changes the
//!   geometry, which is the only way an STL comparison can see it.
//!
//! What is compared is what survives a different triangulation — volume,
//! surface area, bounding box, watertightness — never facet identity. See
//! `tests/README.md`; demanding OpenSCAD's exact tessellation is what left the
//! OpenAPPA strict gate permanently red.

mod support;

use reopenscad_server::engine;
use std::fs;
use std::path::{Path, PathBuf};
use support::{metrics, parse_stl, read_stl, MeshMetrics, StlMesh};

/// Coordinate tolerance for welding while measuring, matching the other parity
/// suites.
const MEASURE_TOLERANCE: f64 = 1.0e-4;

/// Relative tolerance on volume and area.
///
/// Looser than the 1e-6 the hard-geometry gate uses, because several of these
/// cases legitimately differ in *tessellation density* rather than in shape: a
/// hull of two spheres depends on where each sphere's facets land, and two
/// correct implementations can disagree in the fourth decimal of the area
/// without either being wrong. 1e-3 is tight enough that a genuinely different
/// solid — a missing feature, an inverted winding, a factor-of-two scale — is
/// still caught by orders of magnitude.
const RELATIVE_TOLERANCE: f64 = 1.0e-3;

/// Absolute tolerance on a bounding-box corner, in millimetres.
const BOUNDS_TOLERANCE: f64 = 1.0e-3;

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/language")
}

struct Case {
    id: String,
    source: String,
    reference: String,
}

fn cases() -> Vec<Case> {
    let manifest = fs::read_to_string(corpus_root().join("cases.tsv")).expect("cases.tsv");
    manifest
        .lines()
        .filter(|line| !line.trim_start().starts_with('#') && !line.trim().is_empty())
        .map(|line| {
            // The manifest uses a visible `\t` like its neighbours, so it
            // survives an editor that helpfully converts tabs to spaces.
            let mut fields = line.split("\\t");
            Case {
                id: fields.next().unwrap_or_default().to_string(),
                source: fields.next().unwrap_or_default().to_string(),
                reference: fields.nth(1).unwrap_or_default().to_string(),
            }
        })
        .collect()
}

fn compile(path: &Path) -> Result<StlMesh, String> {
    let source =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let output = engine::compile_exact(&source, None).map_err(|error| error.to_string())?;
    parse_stl(&output.mesh.binary_stl()).map_err(|error| format!("parse our output: {error}"))
}

fn relative(actual: f64, expected: f64) -> f64 {
    if expected == 0.0 {
        return if actual == 0.0 { 0.0 } else { f64::INFINITY };
    }
    (actual - expected).abs() / expected.abs()
}

/// Everything wrong with one case, or an empty list if it matches.
fn differences(actual: &MeshMetrics, expected: &MeshMetrics) -> Vec<String> {
    let mut problems = Vec::new();
    if actual.boundary_edges != 0 {
        problems.push(format!("{} boundary edges", actual.boundary_edges));
    }
    let volume = relative(actual.volume, expected.volume);
    if volume > RELATIVE_TOLERANCE {
        problems.push(format!(
            "volume {:.4} vs {:.4} ({volume:.1e})",
            actual.volume, expected.volume
        ));
    }
    let area = relative(actual.surface_area, expected.surface_area);
    if area > RELATIVE_TOLERANCE {
        problems.push(format!(
            "area {:.4} vs {:.4} ({area:.1e})",
            actual.surface_area, expected.surface_area
        ));
    }
    // The bounding box is what catches a case whose *volume* is right but whose
    // position is not — `translate([-2^2, 0, 0])` moves a cube without
    // resizing it, so volume alone calls the precedence bug a pass.
    for axis in 0..3 {
        let name = ["x", "y", "z"][axis];
        if (actual.bounds_min[axis] - expected.bounds_min[axis]).abs() > BOUNDS_TOLERANCE {
            problems.push(format!(
                "min {name} {:.4} vs {:.4}",
                actual.bounds_min[axis], expected.bounds_min[axis]
            ));
        }
        if (actual.bounds_max[axis] - expected.bounds_max[axis]).abs() > BOUNDS_TOLERANCE {
            problems.push(format!(
                "max {name} {:.4} vs {:.4}",
                actual.bounds_max[axis], expected.bounds_max[axis]
            ));
        }
    }
    if actual.components != expected.components {
        problems.push(format!(
            "components {} vs {}",
            actual.components, expected.components
        ));
    }
    problems
}

#[test]
fn every_language_case_matches_the_openscad_solid() {
    let root = corpus_root();
    let mut failures = Vec::new();
    for case in cases() {
        let expected = read_stl(&root.join(&case.reference))
            .map(|mesh| metrics(&mesh, MEASURE_TOLERANCE))
            .unwrap_or_else(|error| panic!("{}: unreadable golden: {error}", case.id));
        match compile(&root.join(&case.source)) {
            Err(error) => {
                eprintln!("FAIL\t{}\tdid not compile: {error}", case.id);
                failures.push(format!("{}: did not compile: {error}", case.id));
            }
            Ok(mesh) => {
                let actual = metrics(&mesh, MEASURE_TOLERANCE);
                let problems = differences(&actual, &expected);
                if problems.is_empty() {
                    eprintln!(
                        "OK\t{}\tvolume {:.3}, {} facets",
                        case.id, actual.volume, actual.facets
                    );
                } else {
                    eprintln!("FAIL\t{}\t{}", case.id, problems.join("; "));
                    failures.push(format!("{}: {}", case.id, problems.join("; ")));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} language cases differ from OpenSCAD:\n{}",
        failures.len(),
        cases().len(),
        failures.join("\n")
    );
}

/// The corpus has to stay in step with its manifest, and the goldens have to be
/// real closed solids — otherwise the gate above measures against nothing.
#[test]
fn the_language_corpus_resolves_and_its_goldens_are_closed_solids() {
    let root = corpus_root();
    let cases = cases();
    assert!(!cases.is_empty(), "the language manifest is empty");
    for case in &cases {
        let source = root.join(&case.source);
        assert!(source.is_file(), "{}: missing {}", case.id, case.source);
        let golden = read_stl(&root.join(&case.reference))
            .unwrap_or_else(|error| panic!("{}: unreadable golden: {error}", case.id));
        let measured = metrics(&golden, MEASURE_TOLERANCE);
        assert_eq!(
            measured.boundary_edges, 0,
            "{}: the golden is not closed, so it cannot be a target",
            case.id
        );
        assert!(
            measured.volume > 0.0,
            "{}: the golden encloses {} — check the face winding in the source",
            case.id,
            measured.volume
        );
    }
    // Every `.scad` in the directory is listed: a fixture nothing measures is
    // a fixture that silently stops being true.
    let listed: Vec<&str> = cases.iter().map(|case| case.source.as_str()).collect();
    for entry in fs::read_dir(&root).expect("corpus directory") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|value| value.to_str()) == Some("scad") {
            let name = path.file_name().and_then(|v| v.to_str()).unwrap_or_default();
            assert!(listed.contains(&name), "{name} is not in cases.tsv");
        }
    }
}
