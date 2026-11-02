//! Boolean robustness harness: compiles a battery of booleans that are known
//! to stress the kernel (imported scan meshes, offset shells, coplanar and
//! nearly coplanar faces, parameter sweeps) and reports how many come out as
//! valid closed 2-manifold solids.
//!
//!   cargo run --release --example boolfuzz -- <assets-dir> [filter] [penguin.scad]
//!
//! `<assets-dir>` is a design's asset folder (`index.json` + `<sha256>.stl`)
//! providing `scan.stl` and `scan_lo.stl`. A part counts as valid when it has
//! no boundary, non-manifold or inconsistently wound edges and a positive
//! volume — the same test the `evaluate` tool reports as `meshes.valid`.
use std::sync::Arc;
use std::time::Instant;

use reopenscad_server::assets::{self, Asset, AssetSet};
use reopenscad_server::{analysis, engine, meshimport};

fn load_assets(dir: &str) -> AssetSet {
    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(format!("{dir}/index.json")).expect("index.json"))
            .expect("parse index.json");
    let mut loaded = Vec::new();
    for entry in index["assets"].as_array().expect("assets") {
        let name = entry["name"].as_str().unwrap().to_string();
        let sha256 = entry["sha256"].as_str().unwrap().to_string();
        let bytes = std::fs::read(format!("{dir}/{sha256}.stl")).expect("stl");
        let mesh = meshimport::parse_stl(&bytes).expect("parse stl");
        loaded.push(Asset { name, sha256, mesh: Arc::new(mesh) });
    }
    loaded.push(synthetic("box.stl", &tessellated_box(20.0, 10)));
    AssetSet::new(loaded)
}

/// A 20 mm box whose faces are each a grid of triangles, written as f32 STL
/// and run through the upload cleaner, like a CAD export would arrive.
fn tessellated_box(size: f64, n: usize) -> Vec<[[f64; 3]; 3]> {
    let mut out = Vec::new();
    let step = size / n as f64;
    // (axis normal, sign)
    for axis in 0..3 {
        for &positive in &[false, true] {
            let (u, v) = ((axis + 1) % 3, (axis + 2) % 3);
            for i in 0..n {
                for j in 0..n {
                    let corner = |a: usize, b: usize| {
                        let mut p = [0.0; 3];
                        p[axis] = if positive { size } else { 0.0 };
                        p[u] = a as f64 * step;
                        p[v] = b as f64 * step;
                        p
                    };
                    let (p00, p10, p11, p01) = (corner(i, j), corner(i + 1, j), corner(i + 1, j + 1), corner(i, j + 1));
                    if positive {
                        out.push([p00, p10, p11]);
                        out.push([p00, p11, p01]);
                    } else {
                        out.push([p00, p11, p10]);
                        out.push([p00, p01, p11]);
                    }
                }
            }
        }
    }
    out
}

fn synthetic(name: &str, triangles: &[[[f64; 3]; 3]]) -> Asset {
    let mut bytes = vec![0u8; 80];
    bytes.extend((triangles.len() as u32).to_le_bytes());
    for t in triangles {
        bytes.extend([0u8; 12]);
        for p in t {
            for c in p {
                bytes.extend((*c as f32).to_le_bytes());
            }
        }
        bytes.extend([0u8; 2]);
    }
    let raw = meshimport::parse_stl(&bytes).expect("synthetic stl");
    let (mesh, _) = meshimport::clean(raw, meshimport::CleanOptions::default());
    Asset { name: name.to_string(), sha256: format!("synthetic-{name}"), mesh: Arc::new(mesh) }
}

/// Transform that places the scan as the repro design does (along X, on z=0).
const HANDLE: &str = r#"
module handle(f = "scan_lo.stl") {
  translate([-0.119, 0.994, 1.585])
  multmatrix([[0.99954, 0.0003, 0.03029, 0], [0.00287, 0.99452, -0.10452, 0],
              [-0.03015, 0.10455, 0.99406, 0], [0, 0, 0, 1]])
  multmatrix([[0.9982, 0.0547, 0.0240, 12.9958], [-0.0286, 0.0840, 0.9961, 223.8318],
              [0.0525, -0.9950, 0.0854, 38.0170], [0, 0, 0, 1]])
  import(f);
}
cz = 10.95;
"#;

fn cases(penguin: Option<&str>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut add = |name: String, body: String| out.push((name, format!("{HANDLE}\n{body}")));

    // 1. cube minus scan, cube placed so its faces cut the scan at various
    //    heights, including exactly through scan vertices' plane z = 0.
    for (i, z) in [-1.0, 0.0, 0.5, 5.0, 10.95, 21.9].iter().enumerate() {
        add(
            format!("cube-minus-scan/{i}"),
            format!("difference() {{ translate([-70,-15,{z}]) cube([80,30,15]); handle(); }}"),
        );
    }
    // Cube minus scan, scan's own coplanar-ish faces: cut the scan with a cube
    // whose face is exactly at a mesh face plane (z of the bottom flat).
    for z in [0.0f64, 1e-6, 1e-5, 1e-4, 1e-3] {
        add(
            format!("scan-cut-at/{z:e}"),
            format!("difference() {{ handle(); translate([-80,-30,-50]) cube([100,60,50 + {z}]); }}"),
        );
    }
    // 2. offset shells: scan minus scaled scan.
    for s in [1.0, 1.001, 1.01, 1.025, 1.05] {
        add(
            format!("shell/{s}"),
            format!(
                "difference() {{ translate([0,0,cz]) scale([1,{s},{s}]) translate([0,0,-cz]) handle(); handle(); }}"
            ),
        );
    }
    // 3. sleeve-like sweep: cylinder/hull minus scaled+nudged import, plus
    //    cube cuts coplanar with each other and through the mesh.
    for step in 0..=20 {
        let t = 0.10 + 0.01 * step as f64;
        add(
            format!("sleeve/t={t:.2}"),
            format!(
                "t={t};
difference() {{
  intersection() {{
    translate([-60,0,cz]) rotate([0,90,0]) cylinder(r=17.5, h=52, $fn=64);
    translate([-70,-30,cz-16.5]) cube([80,60,31.5]);
  }}
  translate([-t,0,0]) translate([0,0,cz]) scale([1,1+t/6,1+t/6]) translate([0,0,-cz]) handle();
  translate([-42,0,-1]) cylinder(r=2.75+t, h=40, $fn=36);
  translate([-8-t,-30,-10]) cube([20,60,40]);
  translate([-70,-30,cz-16.5-1]) cube([80,60,1+t]);
}}"
            ),
        );
    }
    // 4. exactly coplanar / nearly coplanar primitive booleans (no mesh).
    for d in [0.0f64, 1e-7, 1e-6, 1e-5, 1e-4, 1e-3] {
        add(
            format!("coplanar-cubes/{d:e}"),
            format!("difference() {{ cube(10); translate([2,2,{d}]) cube([6,6,10 - {d}]); }}"),
        );
        add(
            format!("coplanar-rot/{d:e}"),
            format!(
                "rotate([17,23,41]) difference() {{ cube(10); translate([2,2,10-3+{d}]) cube([6,6,3]); \
                 translate([-1,4,{d}]) cube([12,2,2]); }}"
            ),
        );
        add(
            format!("coplanar-union/{d:e}"),
            format!("rotate([31,7,13]) union() {{ cube(10); translate([10+{d},2,2]) cube(6); translate([3,3,10-{d}]) cylinder(r=2,h=4,$fn=30); }}"),
        );
        add(
            format!("scan-minus-scan-shifted/{d:e}"),
            format!("difference() {{ handle(); translate([{d},0,0]) handle(); translate([-40,-20,-5]) cube(40); }}"),
        );
        add(
            format!("cube-minus-scan-nudged/{d:e}"),
            format!("difference() {{ translate([-50,-12,{d}]) cube([30,24,20]); handle(); translate([-45,-20,-1]) cube([10,40,1+{d}]); }}"),
        );
    }
    // 5. imported mesh with faces exactly / nearly coplanar with primitives.
    for d in [0.0f64, 1e-6, 1e-5, 1e-4, 1e-3, -1e-6, -1e-4] {
        add(
            format!("box-minus-cube/{d:e}"),
            format!("difference() {{ import(\"box.stl\"); translate([5,5,{d}]) cube([10,10,20]); translate([-1,8,10+{d}]) cube([22,4,10]); }}"),
        );
        add(
            format!("cube-minus-box/{d:e}"),
            format!("difference() {{ cube([30,20,20]); translate([{d},{d},0]) import(\"box.stl\"); }}"),
        );
        add(
            format!("rot-box-minus-cube/{d:e}"),
            format!("rotate([13,27,5]) difference() {{ import(\"box.stl\"); translate([5,-1,20-5+{d}]) cube([10,22,5]); translate([20-3+{d},5,5]) cube([3,10,10]); }}"),
        );
        add(
            format!("box-union-cube/{d:e}"),
            format!("rotate([41,3,77]) union() {{ import(\"box.stl\"); translate([20+{d},5,5]) cube(10); translate([3,3,20-{d}]) cylinder(r=2,h=5,$fn=24); }}"),
        );
        add(
            format!("box-minus-scaled-box/{d:e}"),
            format!("difference() {{ import(\"box.stl\"); translate([10,10,10]) scale(1 - 0.0001 + {d}) translate([-10,-10,-10]) import(\"box.stl\"); translate([5,5,-1]) cube([10,10,30]); }}"),
        );
    }
    // 6. the real design, swept over the parameter that flipped validity.
    if let Some(source) = penguin {
        for step in 0..=20 {
            let t = 0.10 + 0.01 * step as f64;
            let patched = source.replacen("t_tol = 0.1;", &format!("t_tol = {t:.2};"), 1);
            assert_ne!(patched, source, "t_tol line not found");
            out.push((format!("penguin/t_tol={t:.2}"), patched));
        }
        for fit in [1.02, 1.025, 1.03] {
            for fx in [0.15, 0.17, 0.2] {
                let patched = source
                    .replacen("fit = 1.025;", &format!("fit = {fit};"), 1)
                    .replacen("fit_x = 0.17;", &format!("fit_x = {fx};"), 1);
                out.push((format!("penguin/fit={fit},fit_x={fx}"), patched));
            }
        }
    }
    out
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: boolfuzz <assets-dir> [filter] [penguin.scad]");
    let filter = args.next().unwrap_or_default();
    let penguin = args.next().map(|path| std::fs::read_to_string(path).expect("penguin"));
    let set = Arc::new(load_assets(&dir));
    let mut total = 0;
    let mut failed = 0;
    let mut by_group: std::collections::BTreeMap<String, (usize, usize, f64)> = Default::default();
    let start_all = Instant::now();
    for (name, source) in cases(penguin.as_deref()) {
        if !filter.is_empty() && !name.contains(&filter) {
            continue;
        }
        if let Ok(dump) = std::env::var("BOOLFUZZ_DUMP") {
            let file = name.replace(['/', '=', ','], "_");
            std::fs::write(format!("{dump}/{file}.scad"), &source).expect("dump");
        }
        let start = Instant::now();
        let result = assets::with_assets(Some(Arc::clone(&set)), || {
            engine::compile_parts(&source, engine::Quality::Render, None)
        });
        let elapsed = start.elapsed().as_secs_f64();
        total += 1;
        let group = name.split('/').next().unwrap().to_string();
        let entry = by_group.entry(group).or_default();
        entry.0 += 1;
        entry.2 += elapsed;
        let mut ok = true;
        let mut detail = String::new();
        match result {
            Ok(output) => {
                for part in &output.parts {
                    let m = analysis::measure_mesh(&part.mesh);
                    let part_ok = m.boundary_edges == 0
                        && m.non_manifold_edges == 0
                        && m.inconsistent_edges == 0
                        && (m.triangles == 0 || m.volume > 0.0);
                    if !part_ok {
                        ok = false;
                        let near = m
                            .defects
                            .first()
                            .map(|d| format!(" near [{:.3},{:.3},{:.3}]", d.centroid[0], d.centroid[1], d.centroid[2]))
                            .unwrap_or_default();
                        detail.push_str(&format!(
                            " {}: boundary={} nonmanifold={} inconsistent={} vol={:.3}{near};",
                            part.id, m.boundary_edges, m.non_manifold_edges, m.inconsistent_edges, m.volume
                        ));
                    }
                }
                for message in &output.messages {
                    if message.contains("healed") || message.contains("coincide") {
                        detail.push_str(&format!(" msg: {message}"));
                    }
                }
            }
            // A minus itself is empty, and saying so is the right answer.
            Err(error)
                if (name == "shell/1"
                    || name == "scan-minus-scan-shifted/0e0"
                    || name == "box-minus-scaled-box/1e-4"
                    || name == "box-minus-scaled-box/1e-3")
                    && error.to_string().contains("no 3D geometry") => {}
            Err(error) => {
                ok = false;
                detail = format!(" error: {error}");
            }
        }
        if !ok {
            failed += 1;
            entry.1 += 1;
        }
        println!("{} {name} {elapsed:.3}s{detail}", if ok { "ok  " } else { "FAIL" });
    }
    println!("--- per group (cases, failures, seconds)");
    for (group, (cases, failures, seconds)) in &by_group {
        println!("{group}: {cases} cases, {failures} failed, {seconds:.2}s");
    }
    println!(
        "TOTAL {total} cases, {failed} failed ({:.1}%), {:.2}s",
        100.0 * failed as f64 / total.max(1) as f64,
        start_all.elapsed().as_secs_f64()
    );
}
