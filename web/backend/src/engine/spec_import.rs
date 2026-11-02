//! `import()`: STL meshes uploaded to the workspace as assets.
//!
//! The server installs the design's assets with `assets::with_assets` and the
//! evaluator resolves `import("name.stl")` against them; nothing is read from
//! disk. See [`crate::assets`].

use super::*;
use crate::assets::{self, Asset, AssetSet, TriMesh};
use std::time::Instant;

/// An axis-aligned box from `min` to `max`, wound counter-clockwise seen
/// from outside, as an STL asset is.
fn box_mesh(min: [f64; 3], max: [f64; 3]) -> TriMesh {
    let [x0, y0, z0] = min;
    let [x1, y1, z1] = max;
    TriMesh {
        points: vec![
            [x0, y0, z0],
            [x1, y0, z0],
            [x1, y1, z0],
            [x0, y1, z0],
            [x0, y0, z1],
            [x1, y0, z1],
            [x1, y1, z1],
            [x0, y1, z1],
        ],
        triangles: vec![
            [0, 2, 1],
            [0, 3, 2],
            [4, 5, 6],
            [4, 6, 7],
            [0, 1, 5],
            [0, 5, 4],
            [1, 2, 6],
            [1, 6, 5],
            [2, 3, 7],
            [2, 7, 6],
            [3, 0, 4],
            [3, 4, 7],
        ],
    }
}

/// A UV sphere of `2 * stacks * slices` triangles (less the pole fans'
/// degenerate halves), counter-clockwise outward.
fn sphere_mesh(radius: f64, stacks: usize, slices: usize) -> TriMesh {
    let mut points = vec![[0.0, 0.0, radius]];
    for stack in 1..stacks {
        let phi = std::f64::consts::PI * stack as f64 / stacks as f64;
        for slice in 0..slices {
            let theta = std::f64::consts::TAU * slice as f64 / slices as f64;
            points.push([
                radius * phi.sin() * theta.cos(),
                radius * phi.sin() * theta.sin(),
                radius * phi.cos(),
            ]);
        }
    }
    points.push([0.0, 0.0, -radius]);
    let bottom = (points.len() - 1) as u32;
    let ring = |stack: usize, slice: usize| (1 + (stack - 1) * slices + slice % slices) as u32;
    let mut triangles = Vec::new();
    for slice in 0..slices {
        triangles.push([0, ring(1, slice), ring(1, slice + 1)]);
    }
    for stack in 1..stacks - 1 {
        for slice in 0..slices {
            let (a, b) = (ring(stack, slice), ring(stack, slice + 1));
            let (c, d) = (ring(stack + 1, slice), ring(stack + 1, slice + 1));
            triangles.push([a, c, d]);
            triangles.push([a, d, b]);
        }
    }
    for slice in 0..slices {
        triangles.push([bottom, ring(stacks - 1, slice + 1), ring(stacks - 1, slice)]);
    }
    TriMesh { points, triangles }
}

fn asset_set(assets: Vec<(&str, &str, TriMesh)>) -> Arc<AssetSet> {
    Arc::new(AssetSet::new(assets.into_iter().map(|(name, sha, mesh)| Asset {
        name: name.into(),
        sha256: sha.into(),
        mesh: Arc::new(mesh),
    })))
}

fn compile_with(set: &Arc<AssetSet>, source: &str) -> Result<MultipartCompileOutput, EngineError> {
    assets::with_assets(Some(Arc::clone(set)), || {
        compile_parts(source, Quality::Render, None)
    })
}

fn volume(mesh: &Mesh) -> f64 {
    mesh.triangles
        .iter()
        .map(|triangle| {
            let [a, b, c] = triangle.vertices;
            a.dot(b.cross(c)) / 6.0
        })
        .sum()
}

fn bounds(mesh: &Mesh) -> ([f64; 3], [f64; 3]) {
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for point in mesh.triangles.iter().flat_map(|triangle| triangle.vertices) {
        for axis in 0..3 {
            min[axis] = min[axis].min(point.component(axis));
            max[axis] = max[axis].max(point.component(axis));
        }
    }
    (min, max)
}

fn assert_close(actual: f64, expected: f64, what: &str) {
    assert!(
        (actual - expected).abs() <= 1e-6 * expected.abs().max(1.0),
        "{what}: expected {expected}, got {actual}"
    );
}

fn assert_bounds(mesh: &Mesh, min: [f64; 3], max: [f64; 3]) {
    let (actual_min, actual_max) = bounds(mesh);
    for axis in 0..3 {
        assert_close(actual_min[axis], min[axis], "min");
        assert_close(actual_max[axis], max[axis], "max");
    }
}

fn cube_assets() -> Arc<AssetSet> {
    asset_set(vec![("cube.stl", "c0ffee", box_mesh([0.0; 3], [10.0, 20.0, 30.0]))])
}

#[test]
fn an_imported_asset_is_a_solid_with_its_own_bounds_and_volume() {
    let output = compile_with(&cube_assets(), r#"import("cube.stl");"#).expect("import");
    assert_bounds(&output.mesh, [0.0; 3], [10.0, 20.0, 30.0]);
    assert_close(volume(&output.mesh), 6000.0, "volume");
    assert!(
        !output.messages.iter().any(|message| message.contains("WARNING")),
        "{:?}",
        output.messages
    );
    // `./` is the same file, and `file =` the same parameter.
    let dotted = compile_with(&cube_assets(), r#"import(file = "./cube.stl", convexity = 3);"#)
        .expect("dotted import");
    assert_close(volume(&dotted.mesh), 6000.0, "./ volume");
}

#[test]
fn center_moves_the_bounding_box_centre_to_the_origin() {
    let output =
        compile_with(&cube_assets(), r#"import("cube.stl", center = true);"#).expect("import");
    assert_bounds(&output.mesh, [-5.0, -10.0, -15.0], [5.0, 10.0, 15.0]);
    assert_close(volume(&output.mesh), 6000.0, "volume");
}

#[test]
fn imported_geometry_transforms_and_takes_part_in_booleans() {
    let output = compile_with(
        &cube_assets(),
        r#"
            difference() {
                translate([5, 0, 0]) rotate([0, 0, 90]) import("cube.stl");
                translate([-15, 5, 10]) cube([10, 10, 10]);
            }
        "#,
    )
    .expect("boolean with import");
    // Rotated 90° about Z the 10x20 footprint spans x -15..-5 (plus the 5
    // shift: -15..5), y 0..10.
    assert_bounds(&output.mesh, [-15.0, 0.0, 0.0], [5.0, 10.0, 30.0]);
    // The tool overlaps the body in x -15..-5, y 5..10, z 10..20.
    assert_close(volume(&output.mesh), 6000.0 - 10.0 * 5.0 * 10.0, "volume");

    let colored = compile_with(&cube_assets(), r#"color("red") import("cube.stl");"#)
        .expect("colored import");
    assert_eq!(colored.parts.len(), 1);
    assert_eq!(colored.parts[0].color, Some([1.0, 0.0, 0.0, 1.0]));
}

#[test]
fn an_imported_object_is_named_after_its_file() {
    let output = compile_with(
        &cube_assets(),
        r#"
            import("cube.stl");
            translate([40, 0, 0]) import(file = "scans/cube.stl");
        "#,
    )
    .expect("named import");
    let names: Vec<&str> = output.parts.iter().map(|part| part.name.as_str()).collect();
    // The second resolves to nothing, so only the first is an object.
    assert_eq!(names, ["cube 1"]);
}

#[test]
fn an_open_surface_imports_as_uploaded_with_one_honest_warning() {
    // A box without its top: an upload kept open.
    let mut open_box = box_mesh([0.0; 3], [10.0, 10.0, 10.0]);
    let points = open_box.points.clone();
    open_box.triangles.retain(|t| !t.iter().all(|&i| points[i as usize][2] == 10.0));
    let set = asset_set(vec![("open.stl", "0be7", open_box)]);
    let output = compile_with(&set, r#"import("open.stl");"#).expect("an open surface still renders");
    assert!(!output.mesh.triangles.is_empty());
    let warnings: Vec<&String> = output.messages.iter().filter(|message| message.contains("WARNING")).collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].starts_with("WARNING: import(\"open.stl\") is an open surface (4 open edges)"), "{warnings:?}");
}

#[test]
fn a_part_inside_an_open_surface_is_not_swallowed_by_the_union() {
    // A modelled part sitting in a scanned open tray.
    let mut tray = box_mesh([0.0; 3], [10.0, 10.0, 10.0]);
    let points = tray.points.clone();
    tray.triangles.retain(|t| !t.iter().all(|&i| points[i as usize][2] == 10.0));
    let tray_triangles = tray.triangles.len();
    let set = asset_set(vec![("tray.stl", "7a1", tray)]);
    let output = compile_with(&set, r#"import("tray.stl"); translate([3, 3, 3]) cube(2);"#).expect("compiles");
    assert_eq!(output.mesh.triangles.len(), tray_triangles + 12, "the tray as is plus the whole cube");
    let warnings: Vec<&String> = output.messages.iter().filter(|message| message.contains("WARNING")).collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
}

#[test]
fn a_missing_asset_warns_and_leaves_the_rest_standing() {
    let output = compile_with(
        &cube_assets(),
        r#"
            import("nowhere.stl");
            cube(4);
        "#,
    )
    .expect("a missing import must not abort the compile");
    assert_close(volume(&output.mesh), 64.0, "volume");
    assert!(
        output
            .messages
            .iter()
            .any(|message| message == "WARNING: Can't open import file 'nowhere.stl'."),
        "{:?}",
        output.messages
    );
    // Without any asset set the same holds.
    let bare = compile_parts(r#"import("cube.stl"); cube(4);"#, Quality::Render, None)
        .expect("no assets");
    assert_close(volume(&bare.mesh), 64.0, "volume without assets");
    assert!(bare
        .messages
        .iter()
        .any(|message| message == "WARNING: Can't open import file 'cube.stl'."));
}

#[test]
fn only_stl_can_be_imported() {
    let output = compile_with(&cube_assets(), r#"import("logo.svg"); cube(4);"#)
        .expect("an unsupported format must not abort the compile");
    assert_close(volume(&output.mesh), 64.0, "volume");
    assert!(
        output.messages.iter().any(|message| message.starts_with(
            "WARNING: Unsupported file format while trying to import file 'logo.svg'"
        )),
        "{:?}",
        output.messages
    );
}

/// Replacing `scan.stl` under the same name must not serve the old geometry
/// from the in-process memo.
#[test]
fn replacing_an_asset_under_the_same_name_invalidates_the_memo() {
    let source = "import(\"scan.stl\"); // replacing_an_asset";
    let small = asset_set(vec![("scan.stl", "1111", box_mesh([0.0; 3], [1.0, 1.0, 1.0]))]);
    let large = asset_set(vec![("scan.stl", "2222", box_mesh([0.0; 3], [2.0, 2.0, 2.0]))]);
    let first = assets::with_assets(Some(small), || {
        compile_parts_shared(source, Quality::Render, None)
    })
    .expect("small");
    let second = assets::with_assets(Some(large), || {
        compile_parts_shared(source, Quality::Render, None)
    })
    .expect("large");
    assert_close(volume(&first.mesh), 1.0, "first");
    assert_close(volume(&second.mesh), 8.0, "second");

    let whole = assets::with_assets(
        Some(asset_set(vec![("scan.stl", "3333", box_mesh([0.0; 3], [3.0, 1.0, 1.0]))])),
        || compile_shared(source, Quality::Render, None),
    )
    .expect("whole");
    assert_close(volume(&whole.mesh), 3.0, "whole");
}

#[test]
fn compiles_without_assets_are_unaffected() {
    assert_eq!(assets::current_digest(), "");
    let output = compile_parts("cube(3);", Quality::Render, None).expect("plain");
    assert_close(volume(&output.mesh), 27.0, "volume");
}

/// A scan-sized asset goes from asset to kernel solid without a trip
/// through SCAD values. Run with `--nocapture` for the timings.
#[test]
fn a_200k_triangle_scan_imports_quickly() {
    let mesh = sphere_mesh(20.0, 317, 316);
    let triangles = mesh.triangles.len();
    assert!(triangles >= 199_000, "{triangles}");
    let set = asset_set(vec![("scan.stl", "big", mesh)]);

    let started = Instant::now();
    let shape = assets::with_assets(Some(Arc::clone(&set)), || {
        let tokens = Lexer::new(r#"import("scan.stl");"#).tokenize().unwrap();
        let statements = Parser::new(tokens).parse_program().unwrap();
        Evaluator::default().evaluate(&statements)
    })
    .expect("evaluate");
    let evaluated = started.elapsed();
    let solid = exact::shape_to_solid(&shape, None).expect("kernel solid");
    let imported = started.elapsed();
    assert_eq!(solid.polygons.len(), triangles);
    println!("import of {triangles} triangles: evaluate {evaluated:?}, to kernel solid {imported:?}");
    if !cfg!(debug_assertions) {
        assert!(imported.as_secs_f64() < 1.0, "importing took {imported:?}");
    }
}

/// An imported scan differenced with a cube. The import itself is
/// milliseconds; what this times is the BSP kernel. A smooth scan is convex
/// almost everywhere, and an autopartition of a convex surface is a chain as
/// long as the operand (quadratic to build: minutes at 200k triangles) unless
/// the build splits such sets with a free plane, which `csg::bsp` does.
fn difference_with_a_cube(stacks: usize, slices: usize, budget_secs: f64) {
    let mesh = sphere_mesh(20.0, stacks, slices);
    let triangles = mesh.triangles.len();
    let set = asset_set(vec![("scan.stl", "diff", mesh)]);
    let started = Instant::now();
    let output = compile_with(
        &set,
        r#"difference() { import("scan.stl"); translate([0, 0, 10]) cube([50, 50, 30], center = true); }"#,
    )
    .expect("difference");
    println!(
        "difference of a {triangles}-triangle scan with a cube: {:?}, {} output triangles",
        started.elapsed(),
        output.mesh.triangles.len()
    );
    // Generous against a loaded machine; the quadratic build this guards
    // against took minutes.
    if !cfg!(debug_assertions) {
        let elapsed = started.elapsed().as_secs_f64();
        assert!(elapsed < budget_secs, "difference took {elapsed:.1}s");
    }
    // The cube removes everything above z = -5.
    let (min, max) = bounds(&output.mesh);
    assert_close(max[2], -5.0, "cut plane");
    assert_close(min[2], -20.0, "bottom pole");
}

#[test]
fn a_20k_triangle_scan_differences_with_a_cube() {
    difference_with_a_cube(101, 100, 5.0);
}

#[test]
#[cfg_attr(debug_assertions, ignore = "seconds in release, slow unoptimised")]
fn a_200k_triangle_scan_differences_with_a_cube() {
    difference_with_a_cube(317, 316, 10.0);
}

/// The sampled fallback mesher on a scan-sized import: each distance sample
/// goes through a BVH (it was a linear scan of every triangle, so a 200k
/// scan took minutes), and cancellation is honoured per row, so a cancelled
/// render stops promptly.
#[test]
fn the_sampled_mesher_handles_a_200k_triangle_scan() {
    let mesh = sphere_mesh(20.0, 317, 316);
    let set = asset_set(vec![("scan.stl", "sampled", mesh)]);
    let shape = assets::with_assets(Some(Arc::clone(&set)), || {
        let tokens = Lexer::new(r#"import("scan.stl");"#).tokenize().unwrap();
        let statements = Parser::new(tokens).parse_program().unwrap();
        Evaluator::default().evaluate(&statements)
    })
    .expect("evaluate");

    let started = Instant::now();
    let meshed = mesh_shape(&shape, Quality::Render.resolution(), None).expect("sampled mesh");
    let elapsed = started.elapsed();
    println!("sampled mesh of a 200k-triangle scan: {elapsed:?}");
    let sphere = 4.0 / 3.0 * std::f64::consts::PI * 8000.0;
    let error = (volume(&meshed) - sphere).abs() / sphere;
    assert!(error < 0.03, "sampled volume off by {:.2}%", error * 100.0);
    if !cfg!(debug_assertions) {
        assert!(elapsed.as_secs_f64() < 15.0, "sampled meshing took {elapsed:?}");
    }

    // Cancelled shortly after it starts: it must stop well before finishing.
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let started = Instant::now();
    let result = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            cancel.store(true, std::sync::atomic::Ordering::Release);
        });
        // A fresh shape: its BVH is not built yet, as in a real render.
        let fresh = assets::with_assets(Some(Arc::clone(&set)), || {
            let tokens = Lexer::new(r#"import("scan.stl");"#).tokenize().unwrap();
            let statements = Parser::new(tokens).parse_program().unwrap();
            Evaluator::default().evaluate(&statements)
        })
        .expect("evaluate");
        mesh_shape(&fresh, 160, Some(&cancel))
    });
    let stopped = started.elapsed();
    println!("cancelled sampled mesh stopped after {stopped:?}");
    assert!(result.is_err());
    if !cfg!(debug_assertions) {
        assert!(stopped.as_secs_f64() < 2.0, "cancellation took {stopped:?}");
    }
}

/// An asset that is listed but cannot be read (storage down, or replaced
/// mid-compile) fails the compile with the storage error instead of quietly
/// dropping out of the model; a name that is not listed still only warns.
#[test]
fn an_unreadable_asset_fails_the_compile() {
    struct Broken;
    impl assets::AssetSource for Broken {
        fn index(&self) -> Result<Vec<(String, String)>, String> {
            Ok(vec![("scan.stl".into(), "abc".into())])
        }
        fn mesh(&self, _: &str, _: &str) -> Result<Arc<TriMesh>, String> {
            Err("storage is down".into())
        }
    }
    let set = Arc::new(AssetSet::lazy(Broken));
    let error = compile_with(&set, r#"import("scan.stl"); cube(1);"#).err().expect("must fail");
    assert!(error.to_string().contains("storage is down"), "{error}");
    let error = compile_with(&set, r#"import("./scan.stl");"#).err().expect("must fail");
    assert!(error.to_string().contains("storage is down"), "{error}");

    let output = compile_with(&set, r#"import("other.stl"); cube(1);"#).expect("missing only warns");
    assert!(output.messages.iter().any(|d| d.contains("Can't open import file 'other.stl'")), "{:?}", output.messages);
}
