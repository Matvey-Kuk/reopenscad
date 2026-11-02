//! Multi-body export.
//!
//! An export is a list of [`ExportBody`]s — one per top-level object — and
//! every format writes them as *separate bodies in one file*, never as their
//! boolean union. Two touching parts stay two shells, so a slicer can split
//! them, assign them different filaments, or print one of them alone, and
//! because every body keeps world coordinates, files exported separately still
//! line up.
//!
//! | format | how bodies are kept apart                                        |
//! |--------|------------------------------------------------------------------|
//! | STL    | concatenated closed shells; ASCII uses one `solid <name>` each   |
//! | OBJ    | one `o <name>` group each, vertex colours when any body has one  |
//! | OFF    | concatenated shells, per-face RGBA when any body has a colour    |
//! | 3MF    | one named `<object>` each, grouped as components of one model    |
//! | GLB    | one named node + mesh + material each, under one root node       |
//! | STEP   | one named solid each, with an AP214 colour when stated           |
//!
//! Every body is cleaned on the way out ([`clean_mesh`]): welded with the
//! kernel's tolerance and stripped of sliver triangles (see
//! [`crate::csg::cleanup`]). Welding is per body, so nothing is ever merged
//! across two objects.

use crate::csg::cleanup::{
    remove_slivers_grouped, weld_triangles_tracked, CleanupReport, SLIVER_ALTITUDE,
};
use crate::csg::mesh::{IndexedMesh, Vec3 as KernelVec3};
use crate::engine::{FaceColors, Mesh, Triangle, Vec3};
use std::fmt::Write as _;

/// The colour bodies without a `color()` are shown in, as linear-ish RGBA.
/// Matches the viewer's default material.
pub const DEFAULT_COLOR: [f64; 4] = [0.95, 0.77, 0.29, 1.0];

/// One exported object.
#[derive(Clone, Debug)]
pub struct ExportBody {
    /// The object ID (`object-N`), or `model` for a unioned export.
    pub id: String,
    pub name: String,
    /// `color()` wrapping the whole object, RGBA in 0..1. For a body painted
    /// in several colours, the one covering the most surface.
    pub color: Option<[f64; 4]>,
    /// Per-triangle colours of `mesh`, when the body carries more than one
    /// (see [`FaceColors`]). Cleaning keeps it aligned with the triangles.
    pub face_colors: Option<FaceColors>,
    pub mesh: Mesh,
}

impl ExportBody {
    /// The colour of triangle `index`: its own when the body is painted per
    /// face, the body's otherwise.
    pub fn triangle_color(&self, index: usize) -> Option<[f64; 4]> {
        match &self.face_colors {
            Some(colors) => colors.color_of(index),
            None => self.color,
        }
    }

    /// Whether any triangle of this body states a colour.
    pub fn has_color(&self) -> bool {
        self.color.is_some()
            || self
                .face_colors
                .as_ref()
                .is_some_and(|colors| colors.palette.iter().any(Option::is_some))
    }
}

/// The formats [`encode`] understands. The id doubles as the file extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Stl,
    StlAscii,
    Off,
    Obj,
    ThreeMf,
    Step,
    Glb,
}

/// Serialise `bodies`, or `None` when the file would exceed `budget` bytes.
///
/// Callers clean the bodies first with [`clean_bodies`]; `encode` writes what
/// it is given.
pub fn encode(bodies: &[ExportBody], format: Format, title: &str, budget: usize) -> Option<Vec<u8>> {
    // Refuse a file that is certainly over budget before building any of it:
    // the text formats are several times the size of the floor, so building
    // first and measuring after would materialise hundreds of megabytes on
    // the way to a rejection.
    let triangles: usize = bodies.iter().map(|body| body.mesh.triangles.len()).sum();
    if triangles.saturating_mul(min_bytes_per_triangle(format)) > budget {
        return None;
    }
    let data = match format {
        Format::Stl => binary_stl(bodies),
        Format::StlAscii => ascii_stl(bodies, title, budget)?,
        Format::Off => off(bodies, budget)?,
        Format::Obj => obj(bodies, budget)?,
        Format::ThreeMf => crate::threemf::write_bodies(bodies, title, budget)?,
        Format::Step => crate::step::write_bodies(bodies, title, budget)?,
        Format::Glb => crate::gltf::write_bodies(bodies, title, budget)?,
    };
    (data.len() <= budget).then_some(data)
}

/// The fewest bytes a triangle can cost in `format`, never an estimate above
/// what the writer produces: binary STL is exact (50), ASCII STL's facet
/// block is over 100 characters even with one-digit numbers, OBJ writes a
/// `vn` and an `f` line per triangle (26), OFF a `3 a b c` line (8), 3MF a
/// `<triangle v1=.. />` element (36, stored uncompressed), GLB three 4-byte
/// indices (12). STEP is bounded by faces rather than triangles and checks
/// its own floor in [`crate::step::write_bodies`].
fn min_bytes_per_triangle(format: Format) -> usize {
    match format {
        Format::Stl => 50,
        Format::StlAscii => 100,
        Format::Obj => 26,
        Format::Off => 8,
        Format::ThreeMf => 36,
        Format::Glb => 12,
        Format::Step => 0,
    }
}

/// Clean every body in place and drop the ones nothing printable is left of.
/// See [`clean_mesh`].
pub fn clean_bodies(bodies: &mut Vec<ExportBody>) -> CleanupReport {
    let mut total = CleanupReport::default();
    for body in bodies.iter_mut() {
        let groups = body.face_colors.as_ref().map(|colors| colors.faces.as_slice());
        let (mesh, report, kept) = clean_mesh_tracked(&body.mesh, groups);
        body.mesh = mesh;
        if let Some(colors) = &body.face_colors {
            body.face_colors = Some(colors.retain(&kept));
        }
        total.collapses += report.collapses;
        total.flips += report.flips;
        total.remaining += report.remaining;
    }
    bodies.retain(|body| !body.mesh.triangles.is_empty());
    total
}

/// Shells enclosing less than this are not solids, in mm³: a 10 µm cube.
/// Two orders of magnitude below any printer's smallest feature (a 0.1 mm
/// cube is 1e-3 mm³), and far above the volume rounding leaves on a shell
/// that encloses nothing.
pub const MIN_SHELL_VOLUME: f64 = 1.0e-9;

/// Shells enclosing less than this fraction of their bounding cube's volume
/// are flat: two coincident faces, or a sheet thinner than a nanometre per
/// metre. A 1 µm sheet across 100 mm still encloses 1e-8 of its cube.
const MIN_SHELL_FILL: f64 = 1.0e-9;

/// Weld a body, remove its sliver triangles (keeping it closed) and drop the
/// shells that enclose no meaningful volume.
///
/// A degenerate shell — `cube(0.0005)`, or a flat sliver left between two
/// coincident faces — is closed in the topological sense, so without this it
/// would be written and reported as a solid when it is nothing a slicer can
/// print. Facet normals are recomputed from the cleaned geometry.
#[allow(dead_code)] // the library surface; exports clean through `clean_bodies`
pub fn clean_mesh(mesh: &Mesh) -> (Mesh, CleanupReport) {
    let (mesh, report, _) = clean_mesh_tracked(mesh, None);
    (mesh, report)
}

/// [`clean_mesh`], plus the input index of every output triangle, so
/// per-triangle colours can follow the cleanup. With `groups` (one colour
/// slot per input triangle) the sliver repair never flips across a seam.
pub fn clean_mesh_tracked(mesh: &Mesh, groups: Option<&[u16]>) -> (Mesh, CleanupReport, Vec<usize>) {
    let (mut indexed, welded) = weld_triangles_tracked(mesh.triangles.iter().map(|triangle| {
        triangle
            .vertices
            .map(|point| KernelVec3::new(point.x, point.y, point.z))
    }));
    let welded_groups: Vec<u16> = groups
        .map(|groups| welded.iter().map(|index| groups.get(*index).copied().unwrap_or(0)).collect())
        .unwrap_or_default();
    let (report, repaired) = remove_slivers_grouped(&mut indexed, SLIVER_ALTITUDE, &welded_groups);
    let shells = drop_empty_shells(&mut indexed);
    let kept = shells
        .into_iter()
        .map(|index| welded[repaired[index]])
        .collect();
    (soup(&indexed), report, kept)
}

/// Removes every connected shell (triangles sharing welded vertices) whose
/// enclosed volume is below [`MIN_SHELL_VOLUME`] or [`MIN_SHELL_FILL`] of its
/// bounding cube.
fn drop_empty_shells(indexed: &mut IndexedMesh) -> Vec<usize> {
    let all = (0..indexed.triangles.len()).collect();
    if indexed.triangles.is_empty() {
        return all;
    }
    let mut parent: Vec<u32> = (0..indexed.vertices.len() as u32).collect();
    fn find(parent: &mut [u32], mut node: u32) -> u32 {
        while parent[node as usize] != node {
            parent[node as usize] = parent[parent[node as usize] as usize];
            node = parent[node as usize];
        }
        node
    }
    for triangle in &indexed.triangles {
        let root = find(&mut parent, triangle[0]);
        for corner in &triangle[1..] {
            let other = find(&mut parent, *corner);
            if other != root {
                parent[other as usize] = root;
            }
        }
    }
    // Per shell: signed volume and bounds. The volume is taken about the
    // shell's own root vertex rather than the origin, so a tiny shell far
    // from the origin is not lost in the rounding of large products.
    let mut shells: std::collections::HashMap<u32, (f64, [f64; 3], [f64; 3])> =
        std::collections::HashMap::new();
    let roots: Vec<u32> = indexed
        .triangles
        .iter()
        .map(|triangle| find(&mut parent, triangle[0]))
        .collect();
    for (triangle, root) in indexed.triangles.iter().zip(&roots) {
        let origin = indexed.vertices[*root as usize];
        let [a, b, c] = triangle.map(|index| indexed.vertices[index as usize].sub(origin));
        let entry = shells
            .entry(*root)
            .or_insert((0.0, [f64::INFINITY; 3], [f64::NEG_INFINITY; 3]));
        entry.0 += a.dot(b.cross(c)) / 6.0;
        for point in [a, b, c] {
            for (axis, value) in [point.x, point.y, point.z].into_iter().enumerate() {
                // Relative to `origin`, which shifts min and max alike.
                entry.1[axis] = entry.1[axis].min(value);
                entry.2[axis] = entry.2[axis].max(value);
            }
        }
    }
    let empty: std::collections::HashSet<u32> = shells
        .iter()
        .filter(|(_, (volume, min, max))| {
            let extent = (0..3).map(|axis| max[axis] - min[axis]).fold(0.0f64, f64::max);
            let volume = volume.abs();
            volume < MIN_SHELL_VOLUME || volume < MIN_SHELL_FILL * extent.powi(3)
        })
        .map(|(root, _)| *root)
        .collect();
    if empty.is_empty() {
        return all;
    }
    let mut kept = Vec::with_capacity(indexed.triangles.len());
    let mut sources = Vec::with_capacity(indexed.triangles.len());
    for (index, (triangle, root)) in indexed.triangles.iter().zip(&roots).enumerate() {
        if !empty.contains(root) {
            kept.push(*triangle);
            sources.push(index);
        }
    }
    indexed.triangles = kept;
    sources
}

/// Weld an engine mesh into the kernel's indexed form.
#[allow(dead_code)] // the library surface (export-quality tests)
pub fn index_mesh(mesh: &Mesh) -> IndexedMesh {
    weld_triangles_tracked(mesh.triangles.iter().map(|triangle| {
        triangle
            .vertices
            .map(|point| KernelVec3::new(point.x, point.y, point.z))
    }))
    .0
}

fn soup(indexed: &IndexedMesh) -> Mesh {
    let triangles = indexed
        .triangles
        .iter()
        .map(|triangle| {
            let [a, b, c] = triangle.map(|index| {
                let point = indexed.vertices[index as usize];
                Vec3 {
                    x: point.x,
                    y: point.y,
                    z: point.z,
                }
            });
            Triangle {
                vertices: [a, b, c],
                normal: unit_normal(a, b, c),
            }
        })
        .collect();
    Mesh { triangles }
}

fn unit_normal(a: Vec3, b: Vec3, c: Vec3) -> Vec3 {
    let (ux, uy, uz) = (b.x - a.x, b.y - a.y, b.z - a.z);
    let (vx, vy, vz) = (c.x - a.x, c.y - a.y, c.z - a.z);
    let (x, y, z) = (uy * vz - uz * vy, uz * vx - ux * vz, ux * vy - uy * vx);
    let length = (x * x + y * y + z * z).sqrt();
    if !length.is_finite() || length <= 0.0 {
        return Vec3::default();
    }
    Vec3 {
        x: x / length,
        y: y / length,
        z: z / length,
    }
}

/// An 8-bit sRGB channel.
pub fn channel(value: f64) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// `#RRGGBB`, or `#RRGGBBAA` when not opaque — 3MF's `ST_ColorValue`.
pub fn hex_color(color: [f64; 4]) -> String {
    let [r, g, b, a] = color.map(channel);
    if a == 255 {
        format!("#{r:02X}{g:02X}{b:02X}")
    } else {
        format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
    }
}

/// Distinct stated colours in first-use order, and each body's 1-based slot
/// among them: a filament/extruder number. Bodies without a colour take slot 1,
/// and stated colours are numbered after it when any such body exists, so an
/// uncoloured body never shares a filament with a coloured one.
pub fn filament_slots(bodies: &[ExportBody]) -> Vec<usize> {
    let mut palette: Vec<[u8; 4]> = Vec::new();
    let offset = usize::from(bodies.iter().any(|body| body.color.is_none()));
    bodies
        .iter()
        .map(|body| match body.color {
            None => 1,
            Some(color) => {
                let key = color.map(channel);
                let index = palette.iter().position(|entry| *entry == key).unwrap_or_else(|| {
                    palette.push(key);
                    palette.len() - 1
                });
                index + 1 + offset
            }
        })
        .collect()
}

/// Binary STL: every body's facets in turn. Bodies are separate closed shells
/// that a slicer's "split to objects" recovers.
pub fn binary_stl(bodies: &[ExportBody]) -> Vec<u8> {
    let count: usize = bodies.iter().map(|body| body.mesh.triangles.len()).sum();
    let mut output = vec![0u8; 84 + count * 50];
    let header = b"ReOpenSCAD native Rust geometry engine";
    output[..header.len()].copy_from_slice(header);
    output[80..84].copy_from_slice(&(count as u32).to_le_bytes());
    let triangles = bodies.iter().flat_map(|body| body.mesh.triangles.iter());
    for (index, triangle) in triangles.enumerate() {
        let offset = 84 + index * 50;
        let values = [
            triangle.normal,
            triangle.vertices[0],
            triangle.vertices[1],
            triangle.vertices[2],
        ];
        for (value_index, value) in values.iter().flat_map(|v| [v.x, v.y, v.z]).enumerate() {
            let start = offset + value_index * 4;
            output[start..start + 4].copy_from_slice(&(value as f32).to_le_bytes());
        }
    }
    output
}

/// ASCII STL with one `solid <name>` block per body, or `None` once the text
/// passes `budget` bytes. ASCII runs about five times the size of binary, so
/// the budget is enforced while writing rather than after.
pub fn ascii_stl(bodies: &[ExportBody], title: &str, budget: usize) -> Option<Vec<u8>> {
    let mut output = String::new();
    let solid_name = |name: &str| -> String {
        let cleaned: String = name
            .chars()
            .map(|c| if c.is_ascii_graphic() { c } else { '_' })
            .collect();
        if cleaned.is_empty() {
            "object".into()
        } else {
            cleaned
        }
    };
    let fallback = [ExportBody {
        id: String::new(),
        name: title.to_string(),
        color: None,
        face_colors: None,
        mesh: Mesh::default(),
    }];
    let bodies = if bodies.is_empty() { &fallback[..] } else { bodies };
    for body in bodies {
        let name = solid_name(&body.name);
        let _ = writeln!(output, "solid {name}");
        for triangle in &body.mesh.triangles {
            let n = triangle.normal;
            let _ = writeln!(output, "  facet normal {} {} {}", n.x, n.y, n.z);
            output.push_str("    outer loop\n");
            for v in triangle.vertices {
                let _ = writeln!(output, "      vertex {} {} {}", v.x, v.y, v.z);
            }
            output.push_str("    endloop\n  endfacet\n");
            if output.len() > budget {
                return None;
            }
        }
        let _ = writeln!(output, "endsolid {name}");
    }
    Some(output.into_bytes())
}

/// One welded vertex/triangle list per body, with exact (bit-equal) welding:
/// the bodies are already cleaned, so their corners are shared by value.
///
/// With `by_color`, a corner is shared only between triangles of the same
/// colour, so a per-vertex colour (OBJ) can change across a colour seam. The
/// seam is then open in the OBJ: per-vertex colour is the only colour a
/// single-file OBJ can carry, and 3MF is the format to print a painted body
/// from.
/// Each vertex then also carries the colour of the triangles using it.
type IndexedBody = (Vec<(Vec3, Option<[f64; 4]>)>, Vec<[usize; 3]>);

fn indexed_bodies(bodies: &[ExportBody], by_color: bool) -> Vec<IndexedBody> {
    bodies
        .iter()
        .map(|body| {
            let mut lookup = std::collections::HashMap::new();
            let mut vertices = Vec::new();
            let mut triangles = Vec::with_capacity(body.mesh.triangles.len());
            for (index, triangle) in body.mesh.triangles.iter().enumerate() {
                let color = body.triangle_color(index);
                let slot = match (&body.face_colors, by_color) {
                    (Some(colors), true) => colors.faces.get(index).copied().unwrap_or(0),
                    _ => 0,
                };
                let corners = triangle.vertices.map(|v| {
                    let key = ([v.x.to_bits(), v.y.to_bits(), v.z.to_bits()], slot);
                    *lookup.entry(key).or_insert_with(|| {
                        vertices.push((v, color));
                        vertices.len() - 1
                    })
                });
                triangles.push(corners);
            }
            (vertices, triangles)
        })
        .collect()
}

/// Wavefront OBJ: one `o <name>` per body, vertices indexed per body (never
/// shared between bodies), facet normals as `vn`, and — when any body states a
/// colour — the widely read `v x y z r g b` vertex-colour extension, since an
/// `.mtl` cannot travel inside a single-file download.
/// `None` once the text passes `budget` bytes (checked per body).
pub fn obj(bodies: &[ExportBody], budget: usize) -> Option<Vec<u8>> {
    let colored = bodies.iter().any(ExportBody::has_color);
    let mut output = String::from("# ReOpenSCAD native mesh\n");
    let mut vertex_base = 1usize;
    let mut normal_base = 1usize;
    let single = bodies.len() == 1;
    for (body, (vertices, triangles)) in bodies.iter().zip(indexed_bodies(bodies, true)) {
        if !single || colored {
            let _ = writeln!(output, "o {}", obj_name(&body.name));
        }
        for (v, color) in &vertices {
            if colored {
                let color = color.unwrap_or(DEFAULT_COLOR);
                let _ = writeln!(
                    output,
                    "v {} {} {} {} {} {}",
                    v.x, v.y, v.z, color[0], color[1], color[2]
                );
            } else {
                let _ = writeln!(output, "v {} {} {}", v.x, v.y, v.z);
            }
        }
        for triangle in &body.mesh.triangles {
            let n = triangle.normal;
            let _ = writeln!(output, "vn {} {} {}", n.x, n.y, n.z);
        }
        for (index, triangle) in triangles.iter().enumerate() {
            let normal = normal_base + index;
            let _ = writeln!(
                output,
                "f {}//{normal} {}//{normal} {}//{normal}",
                triangle[0] + vertex_base,
                triangle[1] + vertex_base,
                triangle[2] + vertex_base
            );
        }
        vertex_base += vertices.len();
        normal_base += triangles.len();
        if output.len() > budget {
            return None;
        }
    }
    Some(output.into_bytes())
}

fn obj_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string();
    if cleaned.is_empty() {
        "object".into()
    } else {
        cleaned
    }
}

/// Geomview OFF: all bodies' shells in one vertex/face list (each body's
/// vertices are its own). When any body states a colour every face carries
/// `r g b a` as 0..255 integers, the form OpenSCAD itself writes.
/// `None` once the text passes `budget` bytes.
pub fn off(bodies: &[ExportBody], budget: usize) -> Option<Vec<u8>> {
    let colored = bodies.iter().any(ExportBody::has_color);
    let indexed = indexed_bodies(bodies, false);
    let vertex_count: usize = indexed.iter().map(|(vertices, _)| vertices.len()).sum();
    let face_count: usize = indexed.iter().map(|(_, triangles)| triangles.len()).sum();
    let mut output = format!("OFF\n{vertex_count} {face_count} 0\n");
    for (vertices, _) in &indexed {
        for (v, _) in vertices {
            let _ = writeln!(output, "{} {} {}", v.x, v.y, v.z);
        }
        if output.len() > budget {
            return None;
        }
    }
    let mut base = 0usize;
    for (body, (vertices, triangles)) in bodies.iter().zip(&indexed) {
        for (index, triangle) in triangles.iter().enumerate() {
            let suffix = if colored {
                let [r, g, b, a] = body.triangle_color(index).unwrap_or(DEFAULT_COLOR).map(channel);
                format!(" {r} {g} {b} {a}")
            } else {
                String::new()
            };
            let _ = writeln!(
                output,
                "3 {} {} {}{suffix}",
                triangle[0] + base,
                triangle[1] + base,
                triangle[2] + base
            );
        }
        base += vertices.len();
        if output.len() > budget {
            return None;
        }
    }
    Some(output.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_render_as_3mf_hex_and_filament_slots_are_stable() {
        assert_eq!(hex_color([1.0, 0.0, 0.5, 1.0]), "#FF0080");
        assert_eq!(hex_color([0.0, 0.0, 0.0, 0.5]), "#00000080");
        let body = |color| ExportBody {
            id: String::new(),
            name: "x".into(),
            color,
            face_colors: None,
            mesh: Mesh::default(),
        };
        let red = Some([1.0, 0.0, 0.0, 1.0]);
        let blue = Some([0.0, 0.0, 1.0, 1.0]);
        assert_eq!(filament_slots(&[body(red), body(blue), body(red)]), vec![1, 2, 1]);
        assert_eq!(filament_slots(&[body(None), body(blue)]), vec![1, 2]);
    }

    fn compiled(source: &str) -> Vec<ExportBody> {
        let output = crate::engine::compile_parts(source, crate::engine::Quality::Render, None)
            .expect("test model compiles");
        output
            .parts
            .iter()
            .map(|part| ExportBody {
                id: part.id.clone(),
                name: part.name.clone(),
                color: part.color,
                face_colors: part.face_colors.clone(),
                mesh: part.mesh.clone(),
            })
            .collect()
    }

    fn volume(mesh: &Mesh) -> f64 {
        let o = mesh.triangles.first().map_or(Vec3::default(), |t| t.vertices[0]);
        mesh.triangles
            .iter()
            .map(|t| {
                let [a, b, c] = t.vertices.map(|v| KernelVec3::new(v.x - o.x, v.y - o.y, v.z - o.z));
                a.dot(b.cross(c)) / 6.0
            })
            .sum()
    }

    #[test]
    fn a_shell_enclosing_no_volume_is_dropped_and_small_real_parts_survive() {
        // A 0.5 µm cube is a closed shell around nothing printable.
        let mut bodies = compiled("cube(0.0005);");
        clean_bodies(&mut bodies);
        assert!(bodies.is_empty(), "the degenerate body must not be exported");

        // A 0.1 mm cube is small but real, also far from the origin.
        let mut bodies = compiled("translate([500, 500, 500]) cube(0.1);");
        clean_bodies(&mut bodies);
        assert_eq!(bodies.len(), 1);
        assert!((volume(&bodies[0].mesh) - 1.0e-3).abs() < 1e-9);

        // In one body, only the degenerate shell goes.
        let mut bodies = compiled("union() { cube(10); translate([20, 0, 0]) cube(0.0005); }");
        let before = bodies[0].mesh.triangles.len();
        clean_bodies(&mut bodies);
        assert_eq!(bodies.len(), 1);
        assert!(bodies[0].mesh.triangles.len() < before);
        assert!((volume(&bodies[0].mesh) - 1000.0).abs() < 1e-6);
        let max_x = bodies[0]
            .mesh
            .triangles
            .iter()
            .flat_map(|t| t.vertices)
            .map(|v| v.x)
            .fold(f64::MIN, f64::max);
        assert!(max_x <= 10.0 + 1e-9, "the speck at x = 20 is gone");

        // A hollow part's inner (negative) shell is real and stays.
        let mut bodies = compiled("difference() { cube(10, center = true); cube(5, center = true); }");
        clean_bodies(&mut bodies);
        assert!((volume(&bodies[0].mesh) - 875.0).abs() < 1e-6);
    }

    #[test]
    fn every_format_refuses_a_certainly_oversized_file_before_building_it() {
        let mut bodies = compiled("cube(1); translate([2, 0, 0]) sphere(1, $fn = 24);");
        clean_bodies(&mut bodies);
        for format in [
            Format::Stl,
            Format::StlAscii,
            Format::Obj,
            Format::Off,
            Format::ThreeMf,
            Format::Glb,
            Format::Step,
        ] {
            let full = encode(&bodies, format, "t", usize::MAX).expect("unbounded export");
            // The floor never exceeds what the writer really produces...
            let triangles: usize = bodies.iter().map(|b| b.mesh.triangles.len()).sum();
            assert!(
                triangles * min_bytes_per_triangle(format) <= full.len(),
                "{format:?}: floor above the real size"
            );
            // ...and a budget below the real size is refused, not overshot.
            assert!(encode(&bodies, format, "t", full.len() - 1).is_none(), "{format:?}");
            assert_eq!(encode(&bodies, format, "t", full.len()).map(|d| d.len()), Some(full.len()));
        }
        // The text writers stop as soon as they cross the budget.
        assert!(obj(&bodies, 64).is_none());
        assert!(off(&bodies, 64).is_none());
        assert!(crate::threemf::write_bodies(&bodies, "t", 64).is_none());
    }

    #[test]
    fn a_single_body_keeps_its_name_in_3mf_glb_and_step() {
        let bodies = vec![ExportBody {
            id: "object-1".into(),
            name: "lid".into(),
            color: Some([1.0, 0.0, 0.0, 1.0]),
            face_colors: None,
            mesh: compiled("cube(2);").remove(0).mesh,
        }];
        let text = |format| {
            String::from_utf8_lossy(&encode(&bodies, format, "Design title", usize::MAX).unwrap())
                .to_string()
        };
        let three_mf = text(Format::ThreeMf);
        assert!(three_mf.contains("<object id=\"1\" type=\"model\" name=\"lid\""), "{three_mf}");
        assert!(three_mf.contains("<base name=\"lid\""));
        assert!(three_mf.contains("<metadata name=\"Title\">Design title</metadata>"));
        let glb = text(Format::Glb);
        assert!(glb.contains("\"nodes\":[{\"name\":\"lid\",\"mesh\":0}]"), "{glb}");
        assert!(glb.contains("\"meshes\":[{\"name\":\"lid\""));
        assert!(glb.contains("\"materials\":[{\"name\":\"lid\""));
        let step = text(Format::Step);
        assert!(step.contains("MANIFOLD_SOLID_BREP('lid'"), "{step}");
        assert!(step.contains("PRODUCT('Design title'"));
    }
}
