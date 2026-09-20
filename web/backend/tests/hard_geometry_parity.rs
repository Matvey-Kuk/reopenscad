//! Regression gate for the geometry the exact kernel gets *wrong*.
//!
//! `exact_kernel_parity.rs` measures the OpenAPPA corpus, which the kernel
//! reproduces to nine significant figures. That corpus has no non-planar
//! polygons and no near-tangent unions, so it cannot see the defects that
//! actually reach users: a "goldens unchanged" result stayed green through
//! three separate kernel bugs. This file exists to be the net that was
//! missing — see `tests/fixtures/hard-geometry/README.md`.
//!
//! These tests were written red and are green as of the merge-pass rework: the
//! booleans no longer merge, the merge runs once on the export path, and the
//! ear clipper puts back the flat vertices it drops. All six fixtures now come
//! out closed and manifold, matching OpenSCAD's volume and area to eight
//! significant figures. Keep them in the ordinary suite; a red result here is
//! a real regression, not a known gap.
//!
//! Facet identity is deliberately *not* asserted. OpenSCAD's triangulation of
//! a surface is not ours and never will be, and demanding it is what made the
//! OpenAPPA strict gate permanently red. Everything asserted here is intrinsic
//! to the solid: watertightness, volume, area, components, and — where the
//! golden is a manifold surface and the number therefore means something —
//! Euler characteristic.

mod support;

use reopenscad_server::engine;
use std::fs;
use std::path::Path;
use support::{
    hard_geometry_cases, hard_geometry_root, metrics, parse_stl, read_stl, source_with_definitions,
    MeshMetrics, StlMesh,
};

/// Coordinate tolerance for welding vertices while measuring. Loose enough
/// that a differently-split surface still reports the same topology, tight
/// enough that a real crack does not close by accident.
const MEASURE_TOLERANCE: f64 = 1.0e-4;

/// Relative tolerance on volume and area against the golden.
///
/// The kernel matches OpenSCAD to eight or nine significant figures on
/// geometry it handles, so 1e-6 is three orders looser than a passing case
/// needs and still catches the 0.16% the merge pass currently loses.
const RELATIVE_TOLERANCE: f64 = 1.0e-6;

fn compile(path: &Path, definitions: &str) -> Result<StlMesh, String> {
    let source =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let output = engine::compile_exact(&source_with_definitions(&source, definitions), None)
        .map_err(|error| error.to_string())?;
    parse_stl(&output.mesh.binary_stl()).map_err(|error| format!("parse native output: {error}"))
}

fn relative_difference(actual: f64, expected: f64) -> f64 {
    if expected == 0.0 {
        return if actual == 0.0 { 0.0 } else { f64::INFINITY };
    }
    (actual - expected).abs() / expected.abs()
}

/// Compiles one case and reports it, returning the measured pair.
fn measure(case_id: &str, source: &Path, definitions: &str, reference: &Path) -> (MeshMetrics, MeshMetrics) {
    let actual = compile(source, definitions)
        .unwrap_or_else(|error| panic!("{case_id}: the kernel could not compile this at all: {error}"));
    let expected = read_stl(reference)
        .unwrap_or_else(|error| panic!("{case_id}: unreadable golden: {error}"));
    (
        metrics(&actual, MEASURE_TOLERANCE),
        metrics(&expected, MEASURE_TOLERANCE),
    )
}

/// An exported solid must be closed. This is the assertion that catches what
/// users actually report: a mesh with boundary edges has holes in it, and a
/// slicer either refuses it or silently invents a lid.
///
///
/// The goldens prove the models deserve it — OpenSCAD closes every one,
/// the 64-blade basket included.
#[test]
fn every_hard_geometry_case_exports_a_closed_surface() {
    let root = hard_geometry_root();
    let mut failures = Vec::new();
    for case in hard_geometry_cases() {
        let (actual, expected) = measure(
            &case.id,
            &root.join(&case.source),
            &case.definitions,
            &root.join(&case.reference),
        );
        assert_eq!(
            expected.boundary_edges, 0,
            "{}: the golden itself is not closed, so the fixture is wrong",
            case.id
        );
        // Boundary edges are absolute: a hole is a hole, and OpenSCAD leaves
        // none on any of these. Non-manifold edges are *relative* to the
        // golden, because several of these cases are two-part assemblies whose
        // shells touch — base against lid — and an edge shared by two solids
        // that meet is a property of the model, not a defect. Demanding zero
        // there would fail the fixture rather than the kernel.
        let mut problems = Vec::new();
        if actual.boundary_edges != 0 {
            problems.push(format!("{} boundary edges", actual.boundary_edges));
        }
        if actual.non_manifold_edges > expected.non_manifold_edges {
            problems.push(format!(
                "{} non-manifold edges against the golden's {}",
                actual.non_manifold_edges, expected.non_manifold_edges
            ));
        }
        if actual.duplicate_facets > expected.duplicate_facets {
            problems.push(format!(
                "{} duplicate facets against the golden's {}",
                actual.duplicate_facets, expected.duplicate_facets
            ));
        }
        if problems.is_empty() {
            eprintln!("OK\t{}\tclosed", case.id);
        } else {
            eprintln!("FAIL\t{}\t{}", case.id, problems.join(", "));
            failures.push(format!("{}: {}", case.id, problems.join(", ")));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of these models came out of the kernel open:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Volume and area are what survive a different triangulation, so they are
/// how this kernel is held to OpenSCAD's answer.
///
/// Volume is the sharper of the two: the merge pass used to *add* 425 mm^3 to
/// the basket while cracking it, and a facet-count check would have called
/// that a triangulation difference and shrugged.
#[test]
fn every_hard_geometry_case_matches_the_openscad_solid() {
    let root = hard_geometry_root();
    let mut failures = Vec::new();
    for case in hard_geometry_cases() {
        let (actual, expected) = measure(
            &case.id,
            &root.join(&case.source),
            &case.definitions,
            &root.join(&case.reference),
        );
        let volume = relative_difference(actual.volume, expected.volume);
        let area = relative_difference(actual.surface_area, expected.surface_area);
        eprintln!(
            "{}\tvolume {:.6} vs {:.6} ({:.3e})\tarea {:.6} vs {:.6} ({:.3e})\teuler {} vs {}\tcomponents {} vs {}",
            case.id,
            actual.volume,
            expected.volume,
            volume,
            actual.surface_area,
            expected.surface_area,
            area,
            actual.euler_characteristic,
            expected.euler_characteristic,
            actual.components,
            expected.components,
        );
        let mut problems = Vec::new();
        if volume > RELATIVE_TOLERANCE {
            problems.push(format!(
                "volume {} vs {} (relative {volume:.3e})",
                actual.volume, expected.volume
            ));
        }
        if area > RELATIVE_TOLERANCE {
            problems.push(format!(
                "area {} vs {} (relative {area:.3e})",
                actual.surface_area, expected.surface_area
            ));
        }
        if actual.components != expected.components {
            problems.push(format!(
                "components {} vs {}",
                actual.components, expected.components
            ));
        }
        // Only against a golden that is itself a manifold surface. Two of
        // these fixtures are base-and-lid assemblies that OpenSCAD emits with
        // their shells sharing edges — 27 non-manifold edges on the narrow
        // box, 18 on the wide one — and `V - E + F` over a surface like that
        // counts how many contacts the tessellator happened to weld, not the
        // genus of anything. The golden's own numbers say so: two components
        // at chi = 16 works out to genus -6. Ours are fully manifold there,
        // with the same volume to ten significant figures, so demanding the
        // golden's chi would be demanding its triangulation.
        if expected.non_manifold_edges == 0
            && actual.euler_characteristic != expected.euler_characteristic
        {
            problems.push(format!(
                "euler {} vs {}",
                actual.euler_characteristic, expected.euler_characteristic
            ));
        }
        if !problems.is_empty() {
            failures.push(format!("{}: {}", case.id, problems.join("; ")));
        }
    }
    assert!(
        failures.is_empty(),
        "{} hard-geometry cases differ from the OpenSCAD solid:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The manifest and its goldens have to stay in step, and the goldens have to
/// be real closed solids — otherwise the two gates above are measuring
/// against nothing. Cheap, so it runs in the normal suite.
#[test]
fn hard_geometry_manifest_resolves_and_its_goldens_are_closed_solids() {
    let root = hard_geometry_root();
    let cases = hard_geometry_cases();
    assert!(!cases.is_empty(), "the hard-geometry manifest is empty");
    for case in cases {
        let source = root.join(&case.source);
        assert!(source.is_file(), "{}: missing source {}", case.id, case.source);
        let reference = root.join(&case.reference);
        let golden = read_stl(&reference)
            .unwrap_or_else(|error| panic!("{}: unreadable golden: {error}", case.id));
        let measured = metrics(&golden, MEASURE_TOLERANCE);
        assert_eq!(
            measured.boundary_edges, 0,
            "{}: golden has {} boundary edges; a fixture that is not closed cannot be a target",
            case.id, measured.boundary_edges
        );
        assert!(measured.volume > 0.0, "{}: golden encloses no volume", case.id);
    }
}
