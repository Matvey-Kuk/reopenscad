//! Release-mode profiler for designs that `import()` stored assets.
//!
//! Not part of the server. Loads a design's asset directory (the
//! `<id>.assets/` folder with `index.json` and `<sha256>.stl` files) into an
//! in-memory set and compiles the design against it, printing phase timings.
//!
//!   REOPENSCAD_PROFILE=1 cargo run --release --example assetprof -- \
//!       <file.scad> <assets-dir> [preview|render|both] [deadline-seconds]
//!
//! With a deadline, the compile runs under the same cancellation flag and
//! kernel deadline the server uses and reports how long it took to stop.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use reopenscad_server::assets::{self, Asset, AssetSet};
use reopenscad_server::csg::prof;
use reopenscad_server::{engine, meshimport};

fn load_assets(dir: &str) -> AssetSet {
    let index: serde_json::Value = serde_json::from_slice(
        &std::fs::read(format!("{dir}/index.json")).expect("read index.json"),
    )
    .expect("parse index.json");
    let mut loaded = Vec::new();
    for entry in index["assets"].as_array().expect("assets array") {
        let name = entry["name"].as_str().expect("name").to_string();
        let sha256 = entry["sha256"].as_str().expect("sha256").to_string();
        let bytes = std::fs::read(format!("{dir}/{sha256}.stl")).expect("read stl");
        let mesh = meshimport::parse_stl(&bytes).expect("parse stl");
        println!("asset {name}: {} triangles", mesh.triangles.len());
        loaded.push(Asset {
            name,
            sha256,
            mesh: Arc::new(mesh),
        });
    }
    AssetSet::new(loaded)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: assetprof <file.scad> <assets-dir> [preview|render|both] [deadline-s]");
    let dir = args.next().expect("assets dir");
    let mode = args.next().unwrap_or_else(|| "both".to_string());
    let deadline: Option<f64> = args.next().and_then(|value| value.parse().ok());
    let source = std::fs::read_to_string(&path).expect("read source");
    let set = Arc::new(load_assets(&dir));
    let qualities: Vec<engine::Quality> = match mode.as_str() {
        "preview" => vec![engine::Quality::Preview],
        "render" => vec![engine::Quality::Render],
        _ => vec![engine::Quality::Preview, engine::Quality::Render],
    };
    for quality in qualities {
        let cancellation = Arc::new(AtomicBool::new(false));
        let watchdog = deadline.map(|seconds| {
            let flag = Arc::clone(&cancellation);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs_f64(seconds));
                flag.store(true, Ordering::Release);
            })
        });
        let start = Instant::now();
        let result = assets::with_assets(Some(Arc::clone(&set)), || {
            engine::compile_parts(&source, quality, Some(&cancellation))
        });
        let elapsed = start.elapsed().as_secs_f64();
        match result {
            Ok(output) => {
                let triangles: usize = output.parts.iter().map(|p| p.mesh.triangles.len()).sum();
                // FNV-1a over every part's binary STL: equal digests mean
                // bit-identical meshes, in the same triangle order.
                let mut digest = 0xcbf2_9ce4_8422_2325u64;
                for byte in output.parts.iter().flat_map(|part| part.mesh.binary_stl()) {
                    digest = (digest ^ byte as u64).wrapping_mul(0x0100_0000_01b3);
                }
                println!(
                    "{quality:?}: {elapsed:.3}s  parts={} tris={triangles} digest={digest:016x}",
                    output.parts.len()
                );
            }
            Err(error) => println!("{quality:?}: {elapsed:.3}s  error: {error}"),
        }
        prof::report();
        drop(watchdog);
    }
}
