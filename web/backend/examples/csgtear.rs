//! Localises open seams in the exact CSG kernel's output. Developer tool.
//!
//!   cargo run --release --example csgtear -- <file.scad> [definitions] [limit]
//!
//! Not part of the server. It compiles one source and reports, for the kernel
//! *solid* (polygons, before triangulation), for the merged solid, and for the
//! exported triangle mesh, every edge that is not used exactly once in each
//! direction — which is what a tear is. For each such edge it also prints the
//! nearest other vertex to the edge's interior: when that distance is at the
//! noise floor, the seam is a T-junction and the vertex is the one the face on
//! the other side has a corner at.
//!
//! `limit` caps how many edges are listed (default 20; 0 lists none, which is
//! what you want when only the counts matter).
use std::collections::HashMap;
use std::fs;

use reopenscad_server::csg::mesh::Vec3 as KVec3;
use reopenscad_server::csg::Solid;
use reopenscad_server::engine;

/// Vertices closer than this count as one while measuring. Ten times finer
/// than the kernel's own `WELD_EPSILON`, so a cluster the kernel failed to
/// weld still shows up here as the several vertices it really is.
const WELD: f64 = 3.0e-6;

#[derive(Default)]
struct Welder {
    ids: HashMap<[i64; 3], u32>,
    points: Vec<KVec3>,
}

impl Welder {
    fn insert(&mut self, point: KVec3) -> u32 {
        let key = [
            (point.x / WELD).round() as i64,
            (point.y / WELD).round() as i64,
            (point.z / WELD).round() as i64,
        ];
        if let Some(id) = self.ids.get(&key) {
            return *id;
        }
        let id = self.points.len() as u32;
        self.points.push(point);
        self.ids.insert(key, id);
        id
    }
}

fn report(label: &str, rings: &[Vec<u32>], points: &[KVec3], limit: usize) {
    let mut edges: HashMap<(u32, u32), (i32, i32)> = HashMap::new();
    for ring in rings {
        for index in 0..ring.len() {
            let (from, to) = (ring[index], ring[(index + 1) % ring.len()]);
            if from == to {
                continue;
            }
            let slot = edges.entry((from.min(to), from.max(to))).or_insert((0, 0));
            if from < to {
                slot.0 += 1;
            } else {
                slot.1 += 1;
            }
        }
    }
    let mut unbalanced: Vec<(u32, u32, i32, i32)> = edges
        .iter()
        .filter(|(_, (forward, backward))| forward != backward || *forward > 1)
        .map(|((low, high), (forward, backward))| (*low, *high, *forward, *backward))
        .collect();
    unbalanced.sort_unstable();
    let boundary = unbalanced
        .iter()
        .filter(|(_, _, forward, backward)| forward + backward == 1)
        .count();
    println!(
        "{label}: faces {}, edges {}, unbalanced {} (of which boundary {boundary})",
        rings.len(),
        edges.len(),
        unbalanced.len(),
    );
    for (low, high, forward, backward) in unbalanced.iter().take(limit) {
        let start = points[*low as usize];
        let end = points[*high as usize];
        let direction = end.sub(start);
        let length_squared = direction.dot(direction);
        let mut nearest = f64::INFINITY;
        let mut nearest_at = 0.0;
        for (index, point) in points.iter().enumerate() {
            if index as u32 == *low || index as u32 == *high {
                continue;
            }
            let along = point.sub(start).dot(direction) / length_squared;
            if along <= 0.0 || along >= 1.0 {
                continue;
            }
            let distance = start.add(direction.mul(along)).sub(*point).length();
            if distance < nearest {
                nearest = distance;
                nearest_at = along;
            }
        }
        println!(
            "   [{:.7} {:.7} {:.7}] -> [{:.7} {:.7} {:.7}] forward={forward} backward={backward} \
             length={:.3e} nearest={nearest:.3e} at {nearest_at:.6}",
            start.x,
            start.y,
            start.z,
            end.x,
            end.y,
            end.z,
            direction.length(),
        );
    }
}

fn rings_of(solid: &Solid, welder: &mut Welder) -> Vec<Vec<u32>> {
    solid
        .polygons
        .iter()
        .map(|polygon| {
            let mut ring: Vec<u32> = polygon
                .vertices
                .iter()
                .map(|point| welder.insert(*point))
                .collect();
            ring.dedup();
            while ring.len() > 1 && ring[0] == ring[ring.len() - 1] {
                ring.pop();
            }
            ring
        })
        .filter(|ring| ring.len() >= 3)
        .collect()
}

fn measure(label: &str, rings: &[Vec<u32>], points: &[KVec3]) {
    let mut volume = 0.0;
    let mut area = 0.0;
    for ring in rings {
        for index in 1..ring.len() - 1 {
            let a = points[ring[0] as usize];
            let b = points[ring[index] as usize];
            let c = points[ring[index + 1] as usize];
            volume += a.dot(b.cross(c)) / 6.0;
            area += b.sub(a).cross(c.sub(a)).length() / 2.0;
        }
    }
    println!("{label}: volume {volume:.6} area {area:.6}");
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let path = arguments
        .next()
        .expect("usage: csgtear <file.scad> [definitions] [limit]");
    let definitions = arguments.next().unwrap_or_default();
    let limit: usize = arguments
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20);
    let mut source = fs::read_to_string(&path).expect("source");
    if !definitions.trim().is_empty() {
        source.push_str(&format!(
            "\n// Command-line definition overrides.\n{definitions};\n"
        ));
    }
    let solid = engine::exact::compile_exact_solid(&source, None).expect("compile");

    for (label, solid) in [("raw    ", solid.clone()), ("merged ", solid.sealed())] {
        let mut welder = Welder::default();
        let rings = rings_of(&solid, &mut welder);
        report(label, &rings, &welder.points, limit);
        measure(label, &rings, &welder.points);
    }

    let mesh = engine::exact::solid_to_mesh(&solid);
    let mut welder = Welder::default();
    let rings: Vec<Vec<u32>> = mesh
        .triangles
        .iter()
        .map(|triangle| {
            triangle
                .vertices
                .iter()
                .map(|point| welder.insert(KVec3::new(point.x, point.y, point.z)))
                .collect()
        })
        .collect();
    report("exported", &rings, &welder.points, limit);
    measure("exported", &rings, &welder.points);
}
