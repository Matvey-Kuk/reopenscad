//! Locates where a boolean's output opens: in the raw BSP polygon soup, or in
//! the export-time face merge.
//!
//!   cargo run --release --example booldiag -- <assets-dir> <file.scad>
use std::sync::Arc;

use reopenscad_server::assets::{self, Asset, AssetSet};
use reopenscad_server::csg::{self, Vec3};
use reopenscad_server::csg::mesh::VertexWelder;
use reopenscad_server::engine::exact;
use reopenscad_server::{analysis, meshimport};

fn load_assets(dir: &str) -> AssetSet {
    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(format!("{dir}/index.json")).unwrap()).unwrap();
    let mut loaded = Vec::new();
    for entry in index["assets"].as_array().unwrap() {
        let name = entry["name"].as_str().unwrap().to_string();
        let sha256 = entry["sha256"].as_str().unwrap().to_string();
        let bytes = std::fs::read(format!("{dir}/{sha256}.stl")).unwrap();
        let mesh = meshimport::parse_stl(&bytes).unwrap();
        loaded.push(Asset { name, sha256, mesh: Arc::new(mesh) });
    }
    AssetSet::new(loaded)
}

/// Directed-edge balance of a polygon soup after splitting every edge at every
/// welded vertex lying on it (within `tol`). Returns unbalanced elementary
/// edges with their midpoints.
fn soup_balance(polygons: &[csg::Polygon], tol: f64) -> Vec<(Vec3, Vec3, i32)> {
    let mut welder = VertexWelder::default();
    let rings: Vec<Vec<u32>> = polygons
        .iter()
        .map(|p| {
            let mut r: Vec<u32> = p.vertices.iter().map(|v| welder.insert(*v)).collect();
            r.dedup();
            while r.len() > 1 && r[0] == r[r.len() - 1] {
                r.pop();
            }
            r
        })
        .collect();
    let vertices = welder.vertices;
    // grid
    let cell = 0.05;
    let key = |p: Vec3| [(p.x / cell).floor() as i64, (p.y / cell).floor() as i64, (p.z / cell).floor() as i64];
    let mut grid: std::collections::HashMap<[i64; 3], Vec<u32>> = Default::default();
    for (i, v) in vertices.iter().enumerate() {
        grid.entry(key(*v)).or_default().push(i as u32);
    }
    let mut ledger: std::collections::HashMap<(u32, u32), i32> = Default::default();
    for ring in &rings {
        if ring.len() < 3 {
            continue;
        }
        for i in 0..ring.len() {
            let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
            let (pa, pb) = (vertices[a as usize], vertices[b as usize]);
            let d = pb.sub(pa);
            let l2 = d.dot(d);
            let lo = key(pa.min(pb).sub(Vec3::new(tol, tol, tol)));
            let hi = key(pa.max(pb).add(Vec3::new(tol, tol, tol)));
            let mut on: Vec<(f64, u32)> = Vec::new();
            if (hi[0] - lo[0] + 1) * (hi[1] - lo[1] + 1) * (hi[2] - lo[2] + 1) < 200_000 {
                for x in lo[0]..=hi[0] {
                    for y in lo[1]..=hi[1] {
                        for z in lo[2]..=hi[2] {
                            if let Some(list) = grid.get(&[x, y, z]) {
                                for &c in list {
                                    if c == a || c == b {
                                        continue;
                                    }
                                    let p = vertices[c as usize];
                                    let t = p.sub(pa).dot(d) / l2;
                                    if t <= 0.0 || t >= 1.0 {
                                        continue;
                                    }
                                    if pa.add(d.mul(t)).sub(p).length() <= tol {
                                        on.push((t, c));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            on.sort_by(|x, y| x.0.total_cmp(&y.0));
            let mut chain = vec![a];
            chain.extend(on.iter().map(|x| x.1));
            chain.push(b);
            for w in chain.windows(2) {
                let (f, t) = (w[0], w[1]);
                if f < t {
                    *ledger.entry((f, t)).or_default() += 1;
                } else {
                    *ledger.entry((t, f)).or_default() -= 1;
                }
            }
        }
    }
    let mut bad: Vec<(Vec3, Vec3, i32)> = ledger
        .iter()
        .filter(|(_, n)| **n != 0)
        .map(|((f, t), n)| (vertices[*f as usize], vertices[*t as usize], *n))
        .collect();
    bad.sort_by(|a, b| a.0.x.total_cmp(&b.0.x));
    bad
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().unwrap();
    let path = args.next().unwrap();
    let source = std::fs::read_to_string(path).unwrap();
    let set = Arc::new(load_assets(&dir));
    assets::with_assets(Some(set), || {
        let solid = exact::compile_exact_solid(&source, None).expect("compile");
        println!("raw polygons: {}", solid.polygons.len());
        for tol in [1e-9, 1e-7, 1e-5] {
            let bad = soup_balance(&solid.polygons, tol);
            println!("raw soup tol={tol:e}: {} unbalanced elementary edges", bad.len());
            for (a, b, n) in bad.iter().take(12) {
                println!("   {n:+} [{:.6},{:.6},{:.6}] -> [{:.6},{:.6},{:.6}] len={:.3e}", a.x, a.y, a.z, b.x, b.y, b.z, b.sub(*a).length());
            }
        }
        if let Some(probe) = std::env::var("PROBE").ok() {
            let c: Vec<f64> = probe.split(',').map(|x| x.parse().unwrap()).collect();
            let at = Vec3::new(c[0], c[1], c[2]);
            for (i, p) in solid.polygons.iter().enumerate() {
                if p.vertices.iter().any(|v| v.sub(at).length() < 1e-3) {
                    println!("poly {i} n=[{:.9},{:.9},{:.9}] d={:.9}", p.plane.normal.x, p.plane.normal.y, p.plane.normal.z, p.plane.offset);
                    for v in &p.vertices {
                        println!("    [{:.9},{:.9},{:.9}] dist={:.3e}", v.x, v.y, v.z, p.plane.distance(*v));
                    }
                }
            }
        }
        let sealed = solid.sealed();
        let bad = soup_balance(&sealed.polygons, 1e-7);
        println!("sealed soup: {} polygons, {} unbalanced", sealed.polygons.len(), bad.len());
        for (a, b, n) in bad.iter().take(12) {
            println!("   {n:+} [{:.6},{:.6},{:.6}] -> [{:.6},{:.6},{:.6}] len={:.3e}", a.x, a.y, a.z, b.x, b.y, b.z, b.sub(*a).length());
        }
        let mesh = exact::solid_to_mesh(&solid);
        let m = analysis::measure_mesh(&mesh);
        println!(
            "mesh: tris={} boundary={} nonmanifold={} inconsistent={} vol={:.4}",
            m.triangles, m.boundary_edges, m.non_manifold_edges, m.inconsistent_edges, m.volume
        );
        for d in m.defects.iter().take(4) {
            println!("  defect {} edges={} at {:?}", d.kind, d.edges, d.centroid);
            for s in &d.samples {
                println!("     {:?} -> {:?}", s[0], s[1]);
            }
        }
        let mut indexed = solid.sealed().to_indexed_mesh();
        let census_edges = |mesh: &csg::IndexedMesh| {
            let mut map: std::collections::HashMap<(u32, u32), (u32, u32, Vec<usize>)> = Default::default();
            for (i, t) in mesh.triangles.iter().enumerate() {
                for k in 0..3 {
                    let (a, b) = (t[k], t[(k + 1) % 3]);
                    let e = map.entry((a.min(b), a.max(b))).or_default();
                    if a < b { e.0 += 1 } else { e.1 += 1 }
                    e.2.push(i);
                }
            }
            map
        };
        for ((a, b), (f, r, tris)) in census_edges(&indexed) {
            if (f, r) != (1, 1) {
                println!("edge {a}-{b} fwd={f} bwd={r} {:?} -> {:?}", indexed.vertices[a as usize], indexed.vertices[b as usize]);
                for t in tris {
                    let tri = indexed.triangles[t];
                    println!("    tri {t} {:?} pts {:?}", tri, tri.map(|x| indexed.vertices[x as usize]));
                }
            }
        }
        let report = csg::heal::heal(&mut indexed, &mut Vec::new());
        println!("heal: {report:?}");
        for ((a, b), (f, r, _)) in census_edges(&indexed) {
            if (f, r) != (1, 1) {
                println!("after: edge {a}-{b} fwd={f} bwd={r} {:?} -> {:?}", indexed.vertices[a as usize], indexed.vertices[b as usize]);
            }
        }
    });
}
