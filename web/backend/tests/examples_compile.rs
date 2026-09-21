//! The kick-start models ship with the app, so the engine owes them.
//!
//! These are the twelve `.scad` sources behind the cards on an empty
//! workspace. A user's first click lands on one of them, which makes a
//! regression here worse than a regression in the golden corpora: the corpora
//! are measurements, these are the product.
//!
//! What is asserted is what a person would notice — it compiles, it is
//! watertight, it is not empty, and it is quick enough that the first click
//! does not look broken. Facet-level agreement with OpenSCAD is deliberately
//! not asserted; a quarter of these use `text()`, which draws with this engine's
//! own font and never matches. `tests/oracle.sh` renders the same sources with
//! the OpenSCAD binary when a second opinion is wanted.

mod support;

use reopenscad_server::engine;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The examples directory, which is served straight out of the web root.
fn examples_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../examples")
}

/// Vertices closer than this are the same vertex while measuring.
///
/// The same 1e-4 mm `hard_geometry_parity.rs` uses, and for the same reason:
/// it is roughly where a slicer stops distinguishing two points, so it is the
/// tolerance at which "watertight" means what a person printing this would
/// mean by it. Measuring finer than the kernel's own `WELD_EPSILON` would be
/// asking it for a promise it never made.
const MEASURE_TOLERANCE: f64 = 1.0e-4;

/// A first click that takes longer than this looks broken, whatever the
/// progress bar says. Generous against a debug build and a loaded machine;
/// the slowest of these is well under a second in release.
const BUDGET: Duration = Duration::from_secs(20);

fn sources() -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = fs::read_dir(examples_root())
        .expect("the examples directory should exist")
        .filter_map(|entry| {
            let path = entry.expect("readable directory entry").path();
            if path.extension()?.to_str()? != "scad" {
                return None;
            }
            let name = path.file_stem()?.to_str()?.to_string();
            Some((name, fs::read_to_string(&path).expect("readable source")))
        })
        .collect();
    found.sort_by(|left, right| left.0.cmp(&right.0));
    found
}

/// Counts directed edge use over the exported triangles. A closed, consistently
/// oriented surface uses every edge exactly once in each direction.
fn unpaired_edges(mesh: &engine::Mesh) -> usize {
    let key = |point: engine::Vec3| -> [i64; 3] {
        [
            (point.x / MEASURE_TOLERANCE).round() as i64,
            (point.y / MEASURE_TOLERANCE).round() as i64,
            (point.z / MEASURE_TOLERANCE).round() as i64,
        ]
    };
    let mut edges: HashMap<([i64; 3], [i64; 3]), i32> = HashMap::new();
    for triangle in &mesh.triangles {
        for index in 0..3 {
            let from = key(triangle.vertices[index]);
            let to = key(triangle.vertices[(index + 1) % 3]);
            if from == to {
                continue;
            }
            let forward = from < to;
            let slot = edges
                .entry(if forward { (from, to) } else { (to, from) })
                .or_insert(0);
            *slot += if forward { 1 } else { -1 };
        }
    }
    edges.values().filter(|balance| **balance != 0).count()
}

#[test]
fn the_examples_directory_is_the_twelve_the_catalogue_promises() {
    let found = sources();
    assert_eq!(
        found.len(),
        12,
        "found {} sources: {:?}",
        found.len(),
        found.iter().map(|(name, _)| name).collect::<Vec<_>>()
    );
    // Every one has the preview its card shows. `web/tests/examples.test.mjs`
    // checks the manifest agrees with both; this is the half that survives
    // without Node.
    for (name, _) in &found {
        let preview = examples_root().join(format!("{name}.png"));
        assert!(preview.is_file(), "{name} has no preview render next to it");
    }
}

#[test]
fn every_kick_start_model_compiles_to_a_watertight_solid() {
    let mut failures = Vec::new();
    for (name, source) in sources() {
        let started = Instant::now();
        let output = match engine::compile_exact(&source, None) {
            Ok(output) => output,
            Err(error) => {
                failures.push(format!("{name}: did not compile: {error}"));
                continue;
            }
        };
        let elapsed = started.elapsed();
        let open = unpaired_edges(&output.mesh);
        eprintln!(
            "{:<30} {:>7} triangles  {:>7.2?}  {}",
            name,
            output.mesh.triangles.len(),
            elapsed,
            if open == 0 {
                "closed".to_string()
            } else {
                format!("{open} UNPAIRED EDGES")
            }
        );
        if output.mesh.triangles.is_empty() {
            failures.push(format!("{name}: compiled to nothing"));
        }
        if open != 0 {
            failures.push(format!("{name}: {open} unpaired edges"));
        }
        if elapsed > BUDGET {
            failures.push(format!(
                "{name}: took {elapsed:?}, over the {BUDGET:?} budget"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of the shipped examples are broken:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn every_kick_start_model_leads_with_parameters_a_reader_can_edit() {
    // The empty workspace tells people to "edit the numbers at the top". That
    // is a promise about the files, so it is checked against them.
    let mut failures = Vec::new();
    for (name, source) in sources() {
        let mut documented = 0;
        for line in source.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.is_empty() {
                continue;
            }
            let Some((assignment, comment)) = line.split_once("//") else {
                continue;
            };
            if assignment.contains('=') && !assignment.starts_with(char::is_whitespace) {
                if !comment.trim().is_empty() {
                    documented += 1;
                }
            }
        }
        if documented < 4 {
            failures.push(format!("{name}: only {documented} documented parameters"));
        }
        if !source.starts_with("//") {
            failures.push(format!("{name}: does not open with a description"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
