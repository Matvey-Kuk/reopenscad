//! Release-mode timing of the wall-thickness check on example models.
//!
//! Not part of the server. Run with:
//!   cargo run --release --example thickprof -- <file.scad>... [--threshold 0.8]
use std::time::Instant;

use reopenscad_server::analysis::{self, MeshIndex};
use reopenscad_server::engine;

fn main() {
    let mut threshold = 0.8;
    let mut paths = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--threshold" {
            threshold = args.next().and_then(|v| v.parse().ok()).expect("--threshold <mm>");
        } else {
            paths.push(arg);
        }
    }
    for path in paths {
        let source = std::fs::read_to_string(&path).expect("read source");
        let output = engine::compile_parts(&source, engine::Quality::Render, None).expect("compile");
        for part in &output.parts {
            let triangles = analysis::mesh_triangles(&part.mesh);
            let count = triangles.len();
            let index = MeshIndex::new(triangles);
            let start = Instant::now();
            let check = analysis::thickness_check(&index, threshold, analysis::MAX_THICKNESS_SAMPLES);
            println!(
                "{:<40} {:>7} tris {:>8.1} ms  samples={:<6} min={:<8} below={:<5} regions={:<3} refined={:<5} complete={}",
                format!("{}:{}", path.rsplit('/').next().unwrap_or(&path), part.name),
                count,
                start.elapsed().as_secs_f64() * 1000.0,
                check.samples,
                check.min.map(|m| format!("{:.3}", m.thickness)).unwrap_or_default(),
                check.below_threshold,
                check.region_count,
                check.refined_samples,
                check.skipped == 0,
            );
            if std::env::var("THICK_VERBOSE").is_ok() {
                for region in check.regions.iter().take(5) {
                    println!(
                        "    region min {:.4} {} at {:?} area {:.2} samples {}",
                        region.min.thickness, region.min.method, region.min.from, region.area, region.samples
                    );
                }
            }
        }
    }
}
