mod support;

use reopenscad_server::engine::{self, Quality};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use support::{
    compare_meshes, discover_scad_files, fixture_root, metrics, openappa_cases, parse_stl,
    read_stl, source_with_definitions, ComparisonTolerance, Facet, Point, StlMesh,
};

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn compile_file(path: &Path, definitions: &str) -> Result<StlMesh, String> {
    let source =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let source = source_with_definitions(&source, definitions);
    let output = engine::compile(&source, Quality::Render, None)
        .map_err(|error| format!("compile {}: {error}", path.display()))?;
    parse_stl(&output.mesh.binary_stl())
        .map_err(|error| format!("parse native output for {}: {error}", path.display()))
}

/// Reads the sibling `openscad/` checkout, which is GPL reference material
/// kept locally and deliberately gitignored — a clean clone does not have it.
/// Ignored for the same reason as the other upstream-corpus gates below, so
/// that `cargo test` passes without upstream present rather than requiring it.
#[test]
#[ignore = "requires the local upstream reference checkout, which is gitignored and absent from a clean clone"]
fn discovers_the_complete_upstream_example_corpus_deterministically() {
    let examples = repository_root().join("openscad/examples");
    let first = discover_scad_files(&examples).unwrap();
    let second = discover_scad_files(&examples).unwrap();
    assert_eq!(first, second, "corpus discovery order must be reproducible");
    assert_eq!(
        first.len(),
        50,
        "the pinned upstream checkout should contain exactly 50 SCAD examples"
    );
    assert!(first.iter().all(|path| path.starts_with(&examples)));
    assert!(first.iter().all(|path| path.extension().unwrap() == "scad"));
}

#[test]
fn openappa_manifest_resolves_all_copied_sources_and_goldens() {
    let root = fixture_root();
    let cases = openappa_cases();
    assert_eq!(
        cases.len(),
        8,
        "all specified OpenAPPA oracle exports must be listed"
    );
    let sources = cases
        .iter()
        .map(|case| case.source.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        sources,
        BTreeSet::from([
            "openappa-fast-printing.scad",
            "openappa-multipart.scad",
            "openappa.scad",
        ])
    );
    for case in cases {
        assert!(
            root.join(&case.source).is_file(),
            "{} source is missing",
            case.id
        );
        assert!(
            root.join(&case.reference).is_file(),
            "{} golden is missing",
            case.id
        );
        let mesh = read_stl(&root.join(&case.reference)).unwrap();
        assert!(
            !mesh.facets.is_empty(),
            "{} golden contains no facets",
            case.id
        );
    }
}

#[test]
fn binary_and_ascii_stl_parsers_agree() {
    let ascii = b"solid tetra\n\
facet normal 0 0 -1\nouter loop\nvertex 0 0 0\nvertex 0 1 0\nvertex 1 0 0\nendloop\nendfacet\n\
facet normal 0 -1 0\nouter loop\nvertex 0 0 0\nvertex 1 0 0\nvertex 0 0 1\nendloop\nendfacet\n\
facet normal -1 0 0\nouter loop\nvertex 0 0 0\nvertex 0 0 1\nvertex 0 1 0\nendloop\nendfacet\n\
facet normal 1 1 1\nouter loop\nvertex 1 0 0\nvertex 0 1 0\nvertex 0 0 1\nendloop\nendfacet\nendsolid tetra\n";
    let ascii_mesh = parse_stl(ascii).unwrap();
    let native_mesh = engine::Mesh {
        triangles: ascii_mesh
            .facets
            .iter()
            .map(|facet| engine::Triangle {
                normal: engine::Vec3 {
                    x: facet.normal.0[0],
                    y: facet.normal.0[1],
                    z: facet.normal.0[2],
                },
                vertices: facet.vertices.map(|vertex| engine::Vec3 {
                    x: vertex.0[0],
                    y: vertex.0[1],
                    z: vertex.0[2],
                }),
            })
            .collect(),
    };
    let binary_mesh = parse_stl(&native_mesh.binary_stl()).unwrap();
    let report = compare_meshes(&binary_mesh, &ascii_mesh, ComparisonTolerance::default());
    assert!(report.is_match(), "{report}");
}

#[test]
fn binary_stl_detection_handles_a_solid_prefixed_header() {
    let mesh = engine::Mesh {
        triangles: vec![engine::Triangle {
            normal: engine::Vec3 {
                x: 0.0,
                y: 0.0,
                z: 1.0,
            },
            vertices: [
                engine::Vec3 {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                },
                engine::Vec3 {
                    x: 1.0,
                    y: 0.0,
                    z: 0.0,
                },
                engine::Vec3 {
                    x: 0.0,
                    y: 1.0,
                    z: 0.0,
                },
            ],
        }],
    };
    let mut bytes = mesh.binary_stl();
    bytes[..5].copy_from_slice(b"solid");
    assert_eq!(parse_stl(&bytes).unwrap().facets.len(), 1);
}

#[test]
fn mesh_metrics_report_closed_tetrahedron_topology_and_volume() {
    let point = |x, y, z| Point([x, y, z]);
    let mesh = StlMesh {
        facets: vec![
            Facet {
                normal: point(0.0, 0.0, -1.0),
                vertices: [
                    point(0.0, 0.0, 0.0),
                    point(0.0, 1.0, 0.0),
                    point(1.0, 0.0, 0.0),
                ],
            },
            Facet {
                normal: point(0.0, -1.0, 0.0),
                vertices: [
                    point(0.0, 0.0, 0.0),
                    point(1.0, 0.0, 0.0),
                    point(0.0, 0.0, 1.0),
                ],
            },
            Facet {
                normal: point(-1.0, 0.0, 0.0),
                vertices: [
                    point(0.0, 0.0, 0.0),
                    point(0.0, 0.0, 1.0),
                    point(0.0, 1.0, 0.0),
                ],
            },
            Facet {
                normal: point(1.0, 1.0, 1.0),
                vertices: [
                    point(1.0, 0.0, 0.0),
                    point(0.0, 1.0, 0.0),
                    point(0.0, 0.0, 1.0),
                ],
            },
        ],
    };
    let measured = metrics(&mesh, 1.0e-8);
    assert_eq!(measured.vertices, 4);
    assert_eq!(measured.edges, 6);
    assert_eq!(measured.boundary_edges, 0);
    assert_eq!(measured.non_manifold_edges, 0);
    assert_eq!(measured.components, 1);
    assert_eq!(measured.euler_characteristic, 2);
    assert!((measured.volume - 1.0 / 6.0).abs() < 1.0e-12);
}

#[test]
fn comparison_is_independent_of_facet_and_vertex_order() {
    let point = |x, y, z| Point([x, y, z]);
    let left = StlMesh {
        facets: vec![Facet {
            normal: point(0.0, 0.0, 1.0),
            vertices: [
                point(0.0, 0.0, 0.0),
                point(1.0, 0.0, 0.0),
                point(0.0, 1.0, 0.0),
            ],
        }],
    };
    let right = StlMesh {
        facets: vec![Facet {
            normal: point(0.0, 0.0, -1.0),
            vertices: [
                point(0.0, 1.0, 0.0),
                point(1.0, 0.0, 0.0),
                point(0.0, 0.0, 0.0),
            ],
        }],
    };
    let report = compare_meshes(&left, &right, ComparisonTolerance::default());
    assert!(report.is_match(), "{report}");
}

#[test]
fn supported_native_smoke_cases_compile_to_valid_binary_stl() {
    let cases = [
        ("cube", "cube([8, 6, 4], center=true);"),
        (
            "transformed sphere",
            "translate([2,3,4]) sphere(r=3, $fn=12);",
        ),
        (
            "boolean",
            "difference() { cube(8, center=true); cylinder(h=12, r=2, center=true, $fn=16); }",
        ),
        (
            "extrusion",
            "linear_extrude(height=3) square([5,7], center=true);",
        ),
    ];
    for (name, source) in cases {
        let output = engine::compile(source, Quality::Preview, None)
            .unwrap_or_else(|error| panic!("{name} failed to compile: {error}"));
        let parsed = parse_stl(&output.mesh.binary_stl())
            .unwrap_or_else(|error| panic!("{name} emitted invalid binary STL: {error}"));
        assert!(!parsed.facets.is_empty(), "{name} produced an empty mesh");
        let measured = metrics(&parsed, 1.0e-4);
        assert!(measured.surface_area > 0.0, "{name} has no surface area");
        assert!(measured.bounds_min.iter().all(|value| value.is_finite()));
        assert!(measured.bounds_max.iter().all(|value| value.is_finite()));
    }
}

#[test]
#[ignore = "requires full OpenSCAD language and geometry parity; keep strict while implementation catches up"]
fn every_upstream_example_compiles_with_the_native_engine() {
    let examples_root = repository_root().join("openscad/examples");
    let paths = discover_scad_files(&examples_root).unwrap();
    // These upstream examples intentionally produce diagnostics, functions,
    // or 2D geometry rather than an STL-compatible 3D solid. They must still
    // parse and evaluate successfully, but asking `compile()` to emit an STL
    // would incorrectly turn their intended output into a parity failure.
    let non_stl_examples = BTreeSet::from([
        Path::new("Advanced/assert.scad"),
        Path::new("Functions/echo.scad"),
        Path::new("Functions/functions.scad"),
        Path::new("Functions/list_comprehensions.scad"),
        Path::new("Functions/polygon_areas.scad"),
    ]);
    assert!(non_stl_examples.iter().all(|relative| paths
        .iter()
        .any(|path| path.strip_prefix(&examples_root).unwrap() == *relative)));
    let mut failures = Vec::new();
    for path in paths {
        let relative_path = path.strip_prefix(&examples_root).unwrap();
        let relative = relative_path.display();
        let result = if non_stl_examples.contains(relative_path) {
            let source = fs::read_to_string(&path)
                .map_err(|error| format!("read {}: {error}", path.display()));
            source.and_then(|source| {
                engine::validate(&source)
                    .map(|_| StlMesh::default())
                    .map_err(|error| format!("validate {}: {error}", path.display()))
            })
        } else {
            compile_file(&path, "")
        };
        match result {
            Ok(_) if non_stl_examples.contains(relative_path) => {
                eprintln!("OK-NON-STL\t{relative}")
            }
            Ok(mesh) => eprintln!("OK\t{relative}\t{} facets", mesh.facets.len()),
            Err(error) => {
                eprintln!("FAIL\t{relative}\t{error}");
                failures.push(format!("{relative}: {error}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} corpus examples failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
#[ignore = "OpenAPPA compilation is a deliberately expensive full-corpus gate"]
fn all_copied_openappa_sources_compile_with_the_native_engine() {
    let root = fixture_root();
    let sources = openappa_cases()
        .into_iter()
        .map(|case| case.source)
        .collect::<BTreeSet<_>>();
    let mut failures = Vec::new();
    for source in sources {
        if let Err(error) = compile_file(&root.join(&source), "") {
            failures.push(format!("{source}: {error}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} OpenAPPA sources failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
#[ignore = "strict oracle parity is not yet met by the implicit mesher; do not loosen these assertions"]
fn openappa_outputs_match_all_copied_openscad_goldens() {
    let root = fixture_root();
    let mut failures = Vec::new();
    for case in openappa_cases() {
        eprintln!("RUN\t{}", case.id);
        let actual = match compile_file(&root.join(&case.source), &case.definitions) {
            Ok(mesh) => mesh,
            Err(error) => {
                failures.push(format!("{}: {error}", case.id));
                continue;
            }
        };
        let expected = read_stl(&root.join(&case.reference)).unwrap();
        let report = compare_meshes(&actual, &expected, ComparisonTolerance::default());
        if !report.is_match() {
            eprintln!("FAIL\t{}\t{report}", case.id);
            failures.push(format!(
                "{} ({}; -D {}):\n{report}",
                case.id, case.source, case.definitions
            ));
        } else {
            eprintln!("OK\t{}", case.id);
        }
    }
    assert!(
        failures.is_empty(),
        "{} OpenAPPA parity cases failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
