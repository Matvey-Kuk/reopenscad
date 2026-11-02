//! Export mesh quality: what reaches a slicer must be closed and free of
//! sliver triangles.
//!
//! The kernel's booleans cut faces exactly along the other operand's planes,
//! which leaves zero-area caps and needle triangles wherever construction
//! edges are collinear or coincident. The export path removes them
//! (`export::clean_mesh`); these tests hold it to "no triangle with an
//! altitude under 1 µm, still watertight, same volume" on real models.

use reopenscad_server::csg::cleanup::{min_altitude, SLIVER_ALTITUDE};
use reopenscad_server::engine::{self, Mesh};
use reopenscad_server::export::{clean_mesh, index_mesh};
use std::fs;
use std::path::PathBuf;

fn read(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn slivers(mesh: &Mesh) -> usize {
    let indexed = index_mesh(mesh);
    (0..indexed.triangles.len())
        .filter(|index| {
            let [a, b, c] = indexed.triangle_points(*index);
            min_altitude(a, b, c) < SLIVER_ALTITUDE
        })
        .count()
}

struct Measured {
    slivers_before: usize,
    slivers_after: usize,
}

/// Compile every object of `source` at export quality, clean it, and assert
/// the cleaned body is closed, sliver-free and the same solid.
fn check(name: &str, source: &str) -> Measured {
    let output = engine::compile_parts(source, engine::Quality::Render, None)
        .unwrap_or_else(|error| panic!("{name}: {error}"));
    let mut measured = Measured {
        slivers_before: 0,
        slivers_after: 0,
    };
    for part in &output.parts {
        let before = index_mesh(&part.mesh);
        let (cleaned, report) = clean_mesh(&part.mesh);
        let after = index_mesh(&cleaned);
        measured.slivers_before += slivers(&part.mesh);
        measured.slivers_after += slivers(&cleaned);
        let label = format!("{name}/{}", part.id);
        assert_eq!(
            before.is_manifold(),
            after.is_manifold(),
            "{label}: cleanup changed watertightness ({} -> {}) {report:?}",
            before.statistics(),
            after.statistics()
        );
        assert_eq!(
            before.statistics().euler_characteristic,
            after.statistics().euler_characteristic,
            "{label}: cleanup changed topology"
        );
        let (v0, v1) = (before.volume(), after.volume());
        assert!(
            (v0 - v1).abs() <= 1e-6 * v0.abs().max(1.0),
            "{label}: volume {v0} -> {v1}"
        );
    }
    measured
}

/// Models that showed slivers before cleanup and must have none after.
///
/// Not every example qualifies: threads and engraved text carry vertices a
/// few micrometres apart along a crease, real detail that only moving a
/// vertex by more than 1 µm could remove, and that stays.
const CLEAN_CASES: &[&str] = &[
    "../examples/hinged-pen-case.scad",
    "../examples/hinged-magnetic-octagon.scad",
    "../examples/hinged-utility-chest.scad",
    "../examples/raised-nameplate.scad",
    "tests/fixtures/hard-geometry/hinge-box-narrow.scad",
    "tests/fixtures/hard-geometry/louvre-pair.scad",
];

#[test]
fn exported_bodies_have_no_sliver_triangles_and_stay_closed() {
    for case in CLEAN_CASES {
        let measured = check(case, &read(case));
        eprintln!(
            "{case}: slivers {} -> {}",
            measured.slivers_before, measured.slivers_after
        );
        assert_eq!(measured.slivers_after, 0, "{case} still has slivers");
    }
}

/// The cleanup must never break a model: every kick-start example keeps its
/// watertightness, topology and volume, and never gains slivers.
#[test]
fn every_example_survives_cleanup() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../examples");
    let mut entries: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension()? == "scad").then_some(path)
        })
        .collect();
    entries.sort();
    for path in entries {
        let source = fs::read_to_string(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let measured = check(&name, &source);
        eprintln!(
            "{name}: slivers {} -> {}",
            measured.slivers_before, measured.slivers_after
        );
        assert!(measured.slivers_after <= measured.slivers_before, "{name}");
    }
}

