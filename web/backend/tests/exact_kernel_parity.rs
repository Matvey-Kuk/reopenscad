//! Parity of the exact polyhedral kernel against the checked-in OpenSCAD
//! goldens.
//!
//! `corpus_parity.rs` measures the legacy implicit mesher and is left alone.
//! This file measures `engine::compile_exact` with the *same* comparator and
//! the *same* strict tolerances, so the two are directly comparable.
//!
//! The strict full-corpus gate is `#[ignore]`d for the same reason its sibling
//! is: it is an expensive whole-corpus run, and the kernel does not pass it
//! yet. Run it with `cargo test --test exact_kernel_parity -- --ignored
//! --nocapture` to get a per-case report.

mod support;

use reopenscad_server::engine;
use std::fs;
use std::path::Path;
use support::{
    compare_meshes, fixture_root, metrics, openappa_cases, parse_stl, read_stl,
    source_with_definitions, ComparisonTolerance, StlMesh,
};

fn compile_exact_source(source: &str) -> Result<StlMesh, String> {
    let output = engine::compile_exact(source, None).map_err(|error| error.to_string())?;
    parse_stl(&output.mesh.binary_stl()).map_err(|error| format!("parse native output: {error}"))
}

fn compile_exact_file(path: &Path, definitions: &str) -> Result<StlMesh, String> {
    let source =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    compile_exact_source(&source_with_definitions(&source, definitions))
        .map_err(|error| format!("{}: {error}", path.display()))
}

#[test]
fn a_lone_cube_matches_openscads_twelve_facet_tessellation() {
    let mesh = compile_exact_source("cube(20);").unwrap();
    assert_eq!(mesh.facets.len(), 12);
    let measured = metrics(&mesh, 1.0e-4);
    assert_eq!(measured.vertices, 8);
    assert_eq!(measured.edges, 18);
    assert_eq!(measured.euler_characteristic, 2);
    assert_eq!(measured.boundary_edges, 0);
    assert_eq!(measured.non_manifold_edges, 0);
    assert_eq!(measured.duplicate_facets, 0);
    assert!((measured.volume - 8000.0).abs() < 1.0e-9);
    assert!((measured.surface_area - 2400.0).abs() < 1.0e-9);
}

#[test]
fn exact_booleans_stay_watertight() {
    for source in [
        "difference() { cube(20, center=true); cube(10); }",
        "union() { cube(10); translate([10,0,0]) cube(10); }",
        "intersection() { cube(20, center=true); translate([5,5,5]) cube(20, center=true); }",
        "difference() { cube([20,20,5]); translate([8,8,-5]) cube([4,4,20]); }",
        "linear_extrude(height=3) polygon(points=[[0,0],[20,0],[20,20],[0,20],[8,8],[12,8],[12,12],[8,12]], paths=[[0,1,2,3],[4,5,6,7]]);",
    ] {
        let mesh = compile_exact_source(source).unwrap();
        let measured = metrics(&mesh, 1.0e-9);
        assert_eq!(measured.boundary_edges, 0, "{source}: {measured:?}");
        assert_eq!(measured.non_manifold_edges, 0, "{source}: {measured:?}");
        assert_eq!(measured.duplicate_facets, 0, "{source}: {measured:?}");
        assert!(measured.volume > 0.0, "{source}");
    }
}

/// `offset(r = ...)` rounds convex corners and `offset(delta = ...)` mitres
/// them. The shape tree used to record neither, so `r` was ignored outright and
/// every offset was mitred.
#[test]
fn offset_distinguishes_a_rounded_radius_from_a_mitred_delta() {
    let square_side = 10.0;
    let grow = 2.0;
    let rounded = compile_exact_source(&format!(
        "linear_extrude(height=1) offset(r={grow}, $fn=64) square([{square_side},{square_side}]);"
    ))
    .unwrap();
    let mitred = compile_exact_source(&format!(
        "linear_extrude(height=1) offset(delta={grow}) square([{square_side},{square_side}]);"
    ))
    .unwrap();
    let rounded = metrics(&rounded, 1.0e-4);
    let mitred = metrics(&mitred, 1.0e-4);

    // Mitring keeps the four square corners; rounding replaces each with a
    // quarter disc, so the rounded area is smaller by exactly what the corners
    // lose: 4r^2 - pi r^2.
    let mitred_area = (square_side + 2.0 * grow) * (square_side + 2.0 * grow);
    let rounded_area = mitred_area - (4.0 - std::f64::consts::PI) * grow * grow;
    assert!(
        (mitred.volume - mitred_area).abs() < 1.0e-9,
        "mitred {}",
        mitred.volume
    );
    assert!(
        (rounded.volume - rounded_area).abs() < rounded_area * 1.0e-3,
        "rounded {} vs {rounded_area}",
        rounded.volume
    );
    assert_eq!(mitred.facets, 12, "a mitred square is still a box");
    assert!(rounded.facets > 100, "a rounded square is not");
}

/// `minkowski()`'s second operand used to be replaced by a sphere of its
/// bounding radius before the kernel ever saw it. A cube summed with a cube is
/// a cube, and no sphere approximation can produce one.
#[test]
fn minkowski_uses_its_real_second_operand() {
    let mesh = compile_exact_source("minkowski() { cube(10); cube(4); }").unwrap();
    let measured = metrics(&mesh, 1.0e-4);
    assert_eq!(measured.facets, 12, "the sum of two boxes is a box");
    assert!(
        (measured.volume - 14.0f64.powi(3)).abs() < 1.0e-6,
        "{}",
        measured.volume
    );
    assert_eq!(measured.boundary_edges, 0);
    assert_eq!(measured.non_manifold_edges, 0);
}

/// `twist` and `scale` were read off the call and thrown away, so a tapered
/// extrusion came out as a plain prism.
#[test]
fn linear_extrude_honours_twist_and_scale() {
    let frustum =
        compile_exact_source("linear_extrude(height=10, scale=0.5) square(5, center=true);")
            .unwrap();
    let measured = metrics(&frustum, 1.0e-4);
    // h/3 * (A1 + A2 + sqrt(A1 A2)) for a square frustum halving in each axis.
    let expected = 10.0 / 3.0 * (25.0 + 6.25 + 12.5);
    assert!(
        (measured.volume - expected).abs() < 1.0e-9,
        "{} vs {expected}",
        measured.volume
    );

    let twisted =
        compile_exact_source("linear_extrude(height=10, twist=90, $fn=16) square(5, center=true);")
            .unwrap();
    let twisted = metrics(&twisted, 1.0e-4);
    assert!(
        twisted.facets > 12,
        "a twisted prism needs more than a box's facets: {}",
        twisted.facets
    );
    assert_eq!(twisted.boundary_edges, 0);
    assert_eq!(twisted.non_manifold_edges, 0);
}

#[test]
fn the_exact_kernel_beats_the_sampled_one_on_facet_economy() {
    // The whole point of the exact kernel: a box is 12 facets, not thousands.
    // `engine::compile` now routes through the kernel itself, so the sampled
    // baseline has to be asked for by name.
    let exact = compile_exact_source("cube(20);").unwrap();
    let sampled = {
        let output = engine::compile_sampled("cube(20);", engine::Quality::Render, None).unwrap();
        parse_stl(&output.mesh.binary_stl()).unwrap()
    };
    assert!(
        exact.facets.len() * 10 < sampled.facets.len(),
        "exact {} vs sampled {}",
        exact.facets.len(),
        sampled.facets.len()
    );
}

/// Cases whose geometry the exact kernel already reproduces: watertight, same
/// Euler characteristic, same component count, and surface area and enclosed
/// volume equal to the reference to better than one part in 10^9.
///
/// They still fail the strict gate below, because that also demands an
/// identical facet *set* and the kernel merges coplanar faces more
/// aggressively than the reference tessellator does.
const GEOMETRICALLY_EXACT_CASES: &[&str] = &[
    "full-body",
    "primary",
    "secondary",
    "multipart-body",
    "multipart-body-back",
    "multipart-body-front",
    "multipart-secondary",
];

/// Every case closes, including the one whose *tessellation* still differs.
///
/// Separate from the parity gate below on purpose: watertightness is not a
/// question of matching OpenSCAD, it is the difference between an STL a slicer
/// accepts and one it does not, and it is what the face-merging pass is most
/// likely to break. `fast-printing` is here even though its Euler
/// characteristic still differs, because an open mesh would be a far worse
/// regression than a differently-split one.
#[test]
#[ignore = "whole-corpus compilation is a deliberately expensive gate"]
fn every_corpus_case_exports_a_closed_surface() {
    let root = fixture_root();
    let mut failures = Vec::new();
    for case in openappa_cases() {
        let actual = match compile_exact_file(&root.join(&case.source), &case.definitions) {
            Ok(mesh) => metrics(&mesh, 1.0e-4),
            Err(error) => {
                failures.push(format!("{}: {error}", case.id));
                continue;
            }
        };
        if actual.boundary_edges != 0 || actual.duplicate_facets != 0 {
            failures.push(format!(
                "{}: {} boundary edges, {} duplicate facets",
                case.id, actual.boundary_edges, actual.duplicate_facets
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Locks in the level of parity the exact kernel has actually reached, so a
/// regression in the boolean or face-merging passes is caught even while the
/// strict facet-identity gate below is still red.
#[test]
#[ignore = "whole-corpus compilation is a deliberately expensive gate"]
fn the_exact_kernel_reproduces_the_geometry_of_the_cases_it_already_handles() {
    let root = fixture_root();
    let mut failures = Vec::new();
    for case in openappa_cases() {
        if !GEOMETRICALLY_EXACT_CASES.contains(&case.id.as_str()) {
            continue;
        }
        let actual = match compile_exact_file(&root.join(&case.source), &case.definitions) {
            Ok(mesh) => mesh,
            Err(error) => {
                failures.push(format!("{}: {error}", case.id));
                continue;
            }
        };
        let expected = read_stl(&root.join(&case.reference)).unwrap();
        let actual = metrics(&actual, 1.0e-4);
        let expected = metrics(&expected, 1.0e-4);
        let mut problems = Vec::new();
        if actual.boundary_edges != 0 {
            problems.push(format!("{} boundary edges", actual.boundary_edges));
        }
        if actual.non_manifold_edges != 0 {
            problems.push(format!("{} non-manifold edges", actual.non_manifold_edges));
        }
        if actual.duplicate_facets != 0 {
            problems.push(format!("{} duplicate facets", actual.duplicate_facets));
        }
        if actual.euler_characteristic != expected.euler_characteristic {
            problems.push(format!(
                "euler {} vs {}",
                actual.euler_characteristic, expected.euler_characteristic
            ));
        }
        if actual.components != expected.components {
            problems.push(format!(
                "components {} vs {}",
                actual.components, expected.components
            ));
        }
        let area_error = (actual.surface_area - expected.surface_area).abs() / expected.surface_area;
        if area_error > 1.0e-9 {
            problems.push(format!("surface area relative error {area_error:e}"));
        }
        let volume_error = (actual.volume - expected.volume).abs() / expected.volume;
        if volume_error > 1.0e-9 {
            problems.push(format!("volume relative error {volume_error:e}"));
        }
        for axis in 0..3 {
            for (name, actual_value, expected_value) in [
                ("min", actual.bounds_min[axis], expected.bounds_min[axis]),
                ("max", actual.bounds_max[axis], expected.bounds_max[axis]),
            ] {
                if (actual_value - expected_value).abs() > 1.0e-6 {
                    problems.push(format!(
                        "{name} bound axis {axis}: {actual_value} vs {expected_value}"
                    ));
                }
            }
        }
        if !problems.is_empty() {
            failures.push(format!("{}: {}", case.id, problems.join(", ")));
        }
    }
    assert!(
        failures.is_empty(),
        "{} cases regressed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Full-corpus strict gate. Reports every case's numbers before asserting.
#[test]
#[ignore = "whole-corpus oracle parity for the exact kernel is not met yet; do not loosen these assertions"]
fn exact_kernel_outputs_match_all_copied_openscad_goldens() {
    let root = fixture_root();
    let mut failures = Vec::new();
    for case in openappa_cases() {
        eprintln!("RUN\t{}", case.id);
        let started = std::time::Instant::now();
        let compiled = compile_exact_file(&root.join(&case.source), &case.definitions);
        eprintln!("TIME\t{}\t{:.1}s", case.id, started.elapsed().as_secs_f64());
        let actual = match compiled {
            Ok(mesh) => mesh,
            Err(error) => {
                eprintln!("ERR\t{}\t{error}", case.id);
                failures.push(format!("{}: {error}", case.id));
                continue;
            }
        };
        let expected = read_stl(&root.join(&case.reference)).unwrap();
        let report = compare_meshes(&actual, &expected, ComparisonTolerance::default());
        if report.is_match() {
            eprintln!("OK\t{}", case.id);
        } else {
            eprintln!("FAIL\t{}\t{report}", case.id);
            failures.push(format!(
                "{} ({}; -D {}):\n{report}",
                case.id, case.source, case.definitions
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} exact-kernel parity cases failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
