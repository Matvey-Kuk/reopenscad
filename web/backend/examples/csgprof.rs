//! Release-mode phase profiler for the exact CSG kernel (src/csg).
//!
//! Not part of the server. Run with:
//!   REOPENSCAD_PROFILE=1 cargo run --release --example csgprof -- <file.scad> [iters] [render]
use std::time::Instant;

use reopenscad_server::csg::prof;
use reopenscad_server::engine;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: csgprof <file.scad> [iters]");
    let iterations: usize = args.next().and_then(|value| value.parse().ok()).unwrap_or(1);
    let quality = match args.next().as_deref() {
        Some("render") => engine::Quality::Render,
        _ => engine::Quality::Preview,
    };
    let source = std::fs::read_to_string(&path).expect("read source");
    for round in 0..iterations {
        let start = Instant::now();
        let output = engine::compile_parts(&source, quality, None)
            .expect("compile");
        let triangles: usize = output.parts.iter().map(|p| p.mesh.triangles.len()).sum();
        println!(
            "round {round}: {:.3}s  parts={} tris={triangles}",
            start.elapsed().as_secs_f64(),
            output.parts.len()
        );
        prof::report();
    }
}
