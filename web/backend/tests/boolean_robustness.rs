//! Booleans against imported scan-like meshes must come out closed.
//!
//! A user designing a drill adapter around a scanned whisk handle saw
//! validity flip on the third decimal of a clearance parameter: 0.15 and 0.22
//! valid, 0.17 and 0.2 not. Two kernel defects were behind it — the BSP
//! dropped sliver fragments by area, leaving holes, and corners reached by
//! different split sequences landed just beyond welding distance of each
//! other — plus genuinely touching geometry (a thread root exactly on a
//! cone's rim). `examples/boolfuzz.rs` is the full harness; this is the part
//! cheap enough to run on every `cargo test`.
//!
//! The scan here is synthetic and deterministic: a lumpy, noisy capsule of a
//! few thousand f32 triangles, cleaned like an upload, which reproduces the
//! failure rate of the real one before the fix.

use std::sync::Arc;

use reopenscad_server::assets::{self, Asset, AssetSet};
use reopenscad_server::{analysis, engine, meshimport};

/// Deterministic xorshift noise in [-1, 1].
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 52) as f64 * 2.0 - 1.0
    }
}

/// A lumpy capsule along X: `rings` x `segments` quads, radius ~6 mm with a
/// few-percent lobing and 0.05 mm of noise, closed with fan caps.
fn scan_like(rings: usize, segments: usize) -> Vec<[[f64; 3]; 3]> {
    let mut noise = Noise(0x9e37_79b9_7f4a_7c15);
    let length = 60.0;
    let mut grid = Vec::with_capacity(rings + 1);
    for i in 0..=rings {
        let s = i as f64 / rings as f64;
        let x = -length * s;
        // Tapered ends, so the profile is not a cylinder.
        let taper = (std::f64::consts::PI * s).sin().powf(0.3).max(0.15);
        let mut ring = Vec::with_capacity(segments);
        for j in 0..segments {
            let a = std::f64::consts::TAU * j as f64 / segments as f64;
            let lobe = 1.0 + 0.06 * (3.0 * a + 4.0 * s).cos() + 0.03 * (5.0 * a).sin();
            let r = 6.0 * taper * lobe + 0.05 * noise.next();
            ring.push([x, r * a.cos() * 1.3, 10.95 + r * a.sin()]);
        }
        grid.push(ring);
    }
    let mut out = Vec::new();
    for i in 0..rings {
        for j in 0..segments {
            let k = (j + 1) % segments;
            let (a, b, c, d) = (grid[i][j], grid[i][k], grid[i + 1][k], grid[i + 1][j]);
            out.push([a, d, c]);
            out.push([a, c, b]);
        }
    }
    for (index, flip) in [(0usize, false), (rings, true)] {
        let ring = &grid[index];
        let mut centre = [0.0; 3];
        for p in ring {
            for axis in 0..3 {
                centre[axis] += p[axis] / segments as f64;
            }
        }
        for j in 0..segments {
            let k = (j + 1) % segments;
            out.push(if flip { [centre, ring[j], ring[k]] } else { [centre, ring[k], ring[j]] });
        }
    }
    out
}

fn asset(name: &str, triangles: &[[[f64; 3]; 3]]) -> Asset {
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
    let raw = meshimport::parse_stl(&bytes).expect("synthetic stl parses");
    let (mesh, _) = meshimport::clean(raw, meshimport::CleanOptions::default());
    Asset {
        name: name.to_string(),
        sha256: format!("synthetic-{name}"),
        mesh: Arc::new(mesh),
    }
}

fn scan_assets() -> Arc<AssetSet> {
    Arc::new(AssetSet::new([asset("scan.stl", &scan_like(90, 64))]))
}

/// Compiles `source` and returns, per object, a description of what is wrong
/// with it (empty when every object is a valid closed solid), plus messages.
fn check(set: &Arc<AssetSet>, source: &str) -> (Vec<String>, Vec<String>) {
    let output = assets::with_assets(Some(Arc::clone(set)), || {
        engine::compile_parts(source, engine::Quality::Render, None)
    })
    .unwrap_or_else(|error| panic!("compile failed: {error}\n{source}"));
    let mut problems = Vec::new();
    for part in &output.parts {
        let m = analysis::measure_mesh(&part.mesh);
        if m.boundary_edges + m.non_manifold_edges + m.inconsistent_edges > 0 || m.volume <= 0.0 {
            problems.push(format!(
                "{}: boundary={} non-manifold={} inconsistent={} volume={}",
                part.id, m.boundary_edges, m.non_manifold_edges, m.inconsistent_edges, m.volume
            ));
        }
    }
    (problems, output.messages)
}

/// The shape of the user's design: a sleeve around the scan, with the scan
/// scaled and nudged by a clearance `t`, a bolt hole and cube cuts through it.
fn sleeve(t: f64) -> String {
    format!(
        "t = {t};
difference() {{
  intersection() {{
    translate([-55, 0, 10.95]) rotate([0, 90, 0]) cylinder(r = 12, h = 50, $fn = 64);
    translate([-70, -30, 10.95 - 11]) cube([80, 60, 21.5]);
  }}
  translate([-t, 0, 0]) translate([0, 0, 10.95]) scale([1, 1 + t / 6, 1 + t / 6])
    translate([0, 0, -10.95]) import(\"scan.stl\");
  translate([-42, 0, -1]) cylinder(r = 2.75 + t, h = 40, $fn = 36);
  translate([-8 - t, -30, -10]) cube([20, 60, 40]);
  translate([-70, -30, 10.95 - 12]) cube([80, 60, 1 + t]);
}}"
    )
}

#[test]
fn a_clearance_sweep_around_a_scan_stays_closed() {
    let set = scan_assets();
    let mut failures = Vec::new();
    for step in 0..=20 {
        let t = 0.10 + 0.01 * step as f64;
        let (problems, _) = check(&set, &sleeve(t));
        if !problems.is_empty() {
            failures.push(format!("t={t:.2}: {}", problems.join("; ")));
        }
    }
    assert!(failures.is_empty(), "open or non-manifold results:\n{}", failures.join("\n"));
}

#[test]
fn an_offset_shell_of_a_scan_stays_closed() {
    let set = scan_assets();
    for scale in [1.001, 1.01, 1.025, 1.05] {
        let source = format!(
            "difference() {{ translate([0,0,10.95]) scale([1,{scale},{scale}]) translate([0,0,-10.95]) \
             import(\"scan.stl\"); import(\"scan.stl\"); }}"
        );
        let (problems, messages) = check(&set, &source);
        assert!(problems.is_empty(), "scale {scale}: {problems:?} {messages:?}");
    }
}

/// A thread root exactly on a cone's rim: the two cutters touch along an
/// edge, so the exact result has an edge shared by four faces. It must come
/// out closed anyway, and say where it was repaired.
#[test]
fn cutters_touching_along_an_edge_are_separated_and_reported() {
    let set = scan_assets();
    let source = "
r = 2.05;
difference() {
  translate([-10, -10, 0]) cube([20, 20, 10]);
  translate([0, 0, -1]) linear_extrude(height = 12, twist = -360 * 8, slices = 192)
    polygon([for (i = [0 : 35]) let(a = i * 10, u = i / 36)
      (r + 0.8 * (u < 0.35 ? u / 0.35 : u < 0.5 ? 1 : u < 0.85 ? 1 - (u - 0.5) / 0.35 : 0)) * [cos(a), sin(a)]]);
  translate([0, 0, -0.01]) cylinder(r1 = r + 1.8, r2 = r, h = 0.8, $fn = 64);
}";
    let (problems, messages) = check(&set, source);
    assert!(problems.is_empty(), "{problems:?}");
    // The repair, when it runs, names a location.
    for message in messages.iter().filter(|m| m.contains("auto-healed")) {
        assert!(message.contains("near ["), "{message}");
    }
}

/// Exactly and nearly coplanar faces between an imported box and primitives.
#[test]
fn coplanar_faces_with_an_imported_mesh_stay_closed() {
    let mut box_triangles = Vec::new();
    let n = 8;
    let size = 20.0;
    let step = size / n as f64;
    for axis in 0..3 {
        for positive in [false, true] {
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
                        box_triangles.extend([[p00, p10, p11], [p00, p11, p01]]);
                    } else {
                        box_triangles.extend([[p00, p11, p10], [p00, p01, p11]]);
                    }
                }
            }
        }
    }
    let set = Arc::new(AssetSet::new([asset("box.stl", &box_triangles)]));
    for d in [0.0, 1e-6, 1e-5, 1e-4, 1e-3, -1e-6] {
        for source in [
            format!("difference() {{ import(\"box.stl\"); translate([5,5,{d}]) cube([10,10,20]); }}"),
            format!("difference() {{ cube([30,20,20]); translate([{d},{d},0]) import(\"box.stl\"); }}"),
            format!(
                "rotate([13,27,5]) difference() {{ import(\"box.stl\"); translate([5,-1,15+{d}]) cube([10,22,5]); }}"
            ),
            format!("rotate([41,3,77]) union() {{ import(\"box.stl\"); translate([20+{d},5,5]) cube(10); }}"),
        ] {
            let (problems, _) = check(&set, &source);
            assert!(problems.is_empty(), "{source}: {problems:?}");
        }
    }
}

/// A part painted in several colours is repaired as one surface, and every
/// triangle keeps a colour slot.
#[test]
fn a_multicoloured_part_is_repaired_with_its_colours() {
    let set = scan_assets();
    let source = format!(
        "difference() {{ color(\"red\") {} color(\"blue\") translate([0,0,0]) cube(0.001); }}",
        sleeve(0.17).trim_start_matches("t = 0.17;").replacen("difference()", "union()", 0)
    );
    let source = format!("t = 0.17;\n{source}");
    let output = assets::with_assets(Some(Arc::clone(&set)), || {
        engine::compile_parts(&source, engine::Quality::Render, None)
    })
    .unwrap_or_else(|error| panic!("{error}\n{source}"));
    for part in &output.parts {
        let m = analysis::measure_mesh(&part.mesh);
        assert_eq!(m.boundary_edges + m.non_manifold_edges + m.inconsistent_edges, 0, "{}", part.id);
        if let Some(colors) = &part.face_colors {
            assert_eq!(colors.faces.len(), part.mesh.triangles.len());
            assert!(colors.faces.iter().all(|slot| (*slot as usize) < colors.palette.len()));
        }
    }
}
