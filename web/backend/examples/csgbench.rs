//! Kernel-only benchmark for `src/csg`.
//!
//! Deliberately does not go through the evaluator or `engine::compile_*`: it
//! drives `csg::Solid` directly, so its numbers are unaffected by changes to
//! the layers above the kernel. The workload mirrors the shape of the mascot
//! model that motivated this work — a pixel grid of chamfered blocks unioned
//! together, then cut and clipped.
use std::time::Instant;

use reopenscad_server::csg::mesh::{Matrix4, Vec3};
use reopenscad_server::csg::{primitives, prof, solid, Solid};

fn block(x: f64, y: f64, z: f64, size: f64) -> Solid {
    // A chamfered block: a cube minus four corner wedges, which is what makes
    // each cell of the grid non-trivial for the boolean kernel.
    let cube = primitives::cube(Vec3::new(size, size, size), false);
    let chamfer = size * 0.18;
    let mut result = cube;
    for (dx, dy) in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
        let tool = primitives::cube(Vec3::new(chamfer, chamfer, size * 3.0), false).transformed(
            Matrix4::translation(Vec3::new(
                dx * size - chamfer * 0.5,
                dy * size - chamfer * 0.5,
                -size,
            ))
            .multiply(Matrix4::rotation_z(45.0)),
        );
        result = result.difference(&tool);
    }
    result.transformed(Matrix4::translation(Vec3::new(x, y, z)))
}

fn main() {
    let side: i32 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(12);
    let size = 10.0;
    let start = Instant::now();

    let mut cells = Vec::new();
    for row in 0..side {
        for column in 0..side {
            // A checkerboard of two heights, so the grid is not one flat slab.
            let height = if (row + column) % 3 == 0 { 1.0 } else { 2.0 };
            let mut cell = block(column as f64 * size, row as f64 * size, 0.0, size);
            if height > 1.0 {
                cell = cell.union(&block(
                    column as f64 * size,
                    row as f64 * size,
                    size,
                    size,
                ));
            }
            cells.push(cell);
        }
    }
    let built = start.elapsed();

    let start_union = Instant::now();
    let grid = solid::union_all(cells);
    let unioned = start_union.elapsed();

    let start_cut = Instant::now();
    let mut cut = grid;
    for index in 0..6 {
        let hole = primitives::cylinder(size * 4.0, size * 1.3, size * 1.3, false, 24).transformed(
            Matrix4::translation(Vec3::new(
                (index as f64 * 2.3 + 1.0) * size,
                (index as f64 * 1.7 + 1.0) * size,
                -size,
            )),
        );
        cut = cut.difference(&hole);
    }
    let clip = primitives::cube(
        Vec3::new(side as f64 * size * 0.8, side as f64 * size * 0.9, size * 5.0),
        false,
    )
    .transformed(Matrix4::translation(Vec3::new(size, size, -size)));
    let result = cut.intersection(&clip);
    let boolean = start_cut.elapsed();

    let mesh = result.to_indexed_mesh();
    println!(
        "build {:.3}s  union_all {:.3}s  cut/clip {:.3}s  total {:.3}s",
        built.as_secs_f64(),
        unioned.as_secs_f64(),
        boolean.as_secs_f64(),
        start.elapsed().as_secs_f64()
    );
    println!(
        "faces={} vertices={} triangles={}",
        result.polygons.len(),
        mesh.vertices.len(),
        mesh.triangles.len()
    );
    // A digest that changes if any coordinate or any facet changes.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for triangle in &mesh.triangles {
        for index in triangle {
            for word in [
                mesh.vertices[*index as usize].x.to_bits(),
                mesh.vertices[*index as usize].y.to_bits(),
                mesh.vertices[*index as usize].z.to_bits(),
            ] {
                hash ^= word;
                hash = hash.wrapping_mul(0x1000_0000_01b3);
            }
        }
    }
    println!("digest {hash:016x}");
    prof::report();
}
