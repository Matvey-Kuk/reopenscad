//! Dependency-free glTF 2.0 binary (`.glb`) writer.
//!
//! # Why this format and not another mesh format
//!
//! STL, OFF, OBJ and 3MF all describe the same triangles; what none of them
//! does is open in a browser. glTF is the one interchange format a web page
//! can display directly — `<model-viewer>`, three.js, Babylon — and it is what
//! macOS Quick Look, Windows 3D Viewer, Blender, Godot and the AR pipelines on
//! both mobile platforms read without a converter. For a CAD tool that lives on
//! the web, that is the difference between "download and find an app" and
//! "open the link".
//!
//! `.glb` rather than `.gltf`: the binary container holds the JSON and the
//! vertex buffer in one file, so an export is one download with nothing to
//! resolve relative to it.
//!
//! # Metres
//!
//! glTF's unit is the metre; SCAD's is the millimetre. Coordinates are scaled
//! by 1/1000 on the way out rather than being handed over raw or hidden behind
//! a node transform, so a 10 mm cube is 0.01 units in every consumer, an
//! `ARCore`/`RealityKit` scene places it at its real size, and no importer can
//! get it wrong by ignoring a transform it did not apply.
//!
//! # Normals
//!
//! The mesh is welded, which is what makes the file small, but welding a corner
//! would average the normals of the faces meeting there and turn a cube into a
//! beach ball. Positions are therefore shared while normals stay per-face: a
//! vertex is `(position, normal)`, so a corner of a cube becomes three
//! vertices, one per face, and the silhouette stays sharp. Flat shading is the
//! honest rendering of a polyhedral B-rep — the facets are the geometry, not an
//! approximation of a smooth surface.

use crate::engine::{Mesh, Vec3};
use crate::csg::mesh::FastMap;

/// Millimetres to metres, glTF's unit.
const MILLIMETRES_PER_METRE: f64 = 1000.0;

/// Quantisation grid for welding, in millimetres.
///
/// Positions are stored as `f32`, which carries about seven significant
/// decimal digits, so welding any finer than this would keep vertices apart
/// that the buffer cannot tell apart anyway. It is deliberately looser than
/// the kernel's own [`WELD_EPSILON`](crate::csg::mesh::WELD_EPSILON): this is a
/// display format, and the only thing riding on the weld is file size.
const WELD_SCALE: f64 = 1.0e5;

/// Quantisation grid for the normal half of the weld key, per unit component.
///
/// Normals are unit vectors, so they need a grid of their own rather than the
/// position grid scaled by a fudge factor. 1e-6 is two decades below the
/// `f32` the buffer stores them in, which is fine enough that no two facets a
/// viewer could shade differently ever share a vertex, and coarse enough that
/// float noise in two genuinely coplanar facets does not split their corners.
const NORMAL_SCALE: f64 = 1.0e6;

/// Chunk and file tags are four ASCII bytes read as a little-endian `u32`, so
/// the literals below are the tag spelled backwards. They are cross-checked
/// against the bytes themselves in the tests, which is the only way a
/// transposed nibble here would ever be noticed.
const GLB_MAGIC: u32 = 0x4654_6C67; // "glTF"
const GLB_VERSION: u32 = 2;
const CHUNK_JSON: u32 = 0x4E4F_534A; // "JSON"
const CHUNK_BIN: u32 = 0x004E_4942; // "BIN\0"

/// Headroom reserved above the vertex buffer for the GLB header, the two chunk
/// headers, the JSON and the padding, so the total written into the file's own
/// length field cannot reach `u32::MAX`. The JSON is a fixed skeleton plus the
/// title, and the server bounds the title long before it reaches a megabyte.
const GLB_OVERHEAD_CEILING: usize = 1 << 20;

const COMPONENT_FLOAT: u32 = 5126;
const COMPONENT_UNSIGNED_INT: u32 = 5125;
const TARGET_ARRAY_BUFFER: u32 = 34962;
const TARGET_ELEMENT_ARRAY_BUFFER: u32 = 34963;

/// An indexed mesh with per-face normals, ready to become glTF accessors.
#[derive(Default)]
struct Primitive {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    indices: Vec<u32>,
    min: [f32; 3],
    max: [f32; 3],
}

/// Welds on `(position, normal)`, so a shared corner still gets one vertex per
/// face that meets there.
fn build(mesh: &Mesh) -> Primitive {
    let mut primitive = Primitive::default();
    let mut lookup: FastMap<[i64; 6], u32> = FastMap::default();
    for triangle in &mesh.triangles {
        if !triangle
            .vertices
            .iter()
            .all(|vertex| vertex.x.is_finite() && vertex.y.is_finite() && vertex.z.is_finite())
        {
            continue;
        }
        // The stored normal can be stale or zero on a degenerate facet, so the
        // geometric one is recomputed and the facet is dropped when it has no
        // area — a zero normal makes a renderer shade the triangle black.
        let normal = geometric_normal(triangle.vertices);
        let Some(normal) = normal else { continue };
        let mut corners = [0u32; 3];
        for (slot, vertex) in triangle.vertices.iter().enumerate() {
            let position = [
                vertex.x / MILLIMETRES_PER_METRE,
                vertex.y / MILLIMETRES_PER_METRE,
                vertex.z / MILLIMETRES_PER_METRE,
            ];
            let key = [
                quantize(vertex.x),
                quantize(vertex.y),
                quantize(vertex.z),
                quantize_on(normal[0], NORMAL_SCALE),
                quantize_on(normal[1], NORMAL_SCALE),
                quantize_on(normal[2], NORMAL_SCALE),
            ];
            let next = primitive.positions.len() as u32;
            corners[slot] = *lookup.entry(key).or_insert_with(|| {
                primitive
                    .positions
                    .push([position[0] as f32, position[1] as f32, position[2] as f32]);
                primitive
                    .normals
                    .push([normal[0] as f32, normal[1] as f32, normal[2] as f32]);
                next
            });
        }
        if corners[0] == corners[1] || corners[1] == corners[2] || corners[0] == corners[2] {
            continue;
        }
        primitive.indices.extend_from_slice(&corners);
    }
    // A vertex is only real once some triangle indexes it. Facets are welded
    // corner by corner and only then tested for degeneracy, so a facet that
    // collapses leaves its corners behind; with every facet collapsed that
    // would be a `BIN` chunk of vertices the JSON never declares, since the
    // empty-geometry JSON below keys off the *index* count. Clearing here
    // keeps the two in step whatever the input was.
    if primitive.indices.is_empty() {
        primitive.positions.clear();
        primitive.normals.clear();
    }

    // `POSITION` is the one accessor glTF *requires* min/max on, because
    // viewers use it to frame the camera and to build the bounding volume.
    primitive.min = [f32::INFINITY; 3];
    primitive.max = [f32::NEG_INFINITY; 3];
    for position in &primitive.positions {
        for ((low, high), value) in primitive
            .min
            .iter_mut()
            .zip(primitive.max.iter_mut())
            .zip(position)
        {
            *low = low.min(*value);
            *high = high.max(*value);
        }
    }
    if primitive.positions.is_empty() {
        primitive.min = [0.0; 3];
        primitive.max = [0.0; 3];
    }
    primitive
}

fn quantize(value: f64) -> i64 {
    quantize_on(value, WELD_SCALE)
}

fn quantize_on(value: f64, scale: f64) -> i64 {
    if !value.is_finite() {
        return 0;
    }
    (value * scale).round() as i64
}

fn geometric_normal(vertices: [Vec3; 3]) -> Option<[f64; 3]> {
    let [a, b, c] = vertices;
    let (ux, uy, uz) = (b.x - a.x, b.y - a.y, b.z - a.z);
    let (vx, vy, vz) = (c.x - a.x, c.y - a.y, c.z - a.z);
    let normal = [
        uy * vz - uz * vy,
        uz * vx - ux * vz,
        ux * vy - uy * vx,
    ];
    let length = (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]).sqrt();
    if !length.is_finite() || length <= 0.0 {
        return None;
    }
    Some([
        normal[0] / length,
        normal[1] / length,
        normal[2] / length,
    ])
}

/// Build a complete `.glb` for `mesh`, or `None` when it would exceed `budget`.
///
/// `title` names the scene's node, which is what a viewer shows in its outline.
///
/// The budget is checked against the computed buffer size *before* the bytes
/// are laid out, so an oversized model costs one pass over the triangles
/// rather than a full-size allocation that is then thrown away. It also keeps
/// the three `u32` sizes in the GLB header from wrapping: a header claiming a
/// 4 GiB file is twelve bytes long is a corrupt download, where a rejection is
/// an error the caller can report.
pub fn write(mesh: &Mesh, title: &str, budget: usize) -> Option<Vec<u8>> {
    let primitive = build(mesh);

    // Buffer layout: positions, normals, then indices. Every accessor's offset
    // has to be a multiple of its component size (4 bytes here) and every
    // buffer view a multiple of 4, which holds because all three arrays are
    // made of 4-byte components.
    let positions_length = primitive.positions.len() * 12;
    let normals_length = primitive.normals.len() * 12;
    let indices_length = primitive.indices.len() * 4;
    if positions_length + normals_length + indices_length
        > budget.min(u32::MAX as usize - GLB_OVERHEAD_CEILING)
    {
        return None;
    }
    let mut binary = Vec::with_capacity(positions_length + normals_length + indices_length);
    for position in &primitive.positions {
        for value in position {
            binary.extend_from_slice(&value.to_le_bytes());
        }
    }
    for normal in &primitive.normals {
        for value in normal {
            binary.extend_from_slice(&value.to_le_bytes());
        }
    }
    for index in &primitive.indices {
        binary.extend_from_slice(&index.to_le_bytes());
    }

    let vertex_count = primitive.positions.len();
    let index_count = primitive.indices.len();
    let json = if vertex_count == 0 || index_count == 0 {
        // glTF forbids an empty `meshes`/`accessors` array, so a model with no
        // geometry becomes a scene with an empty node rather than a file no
        // viewer will open.
        format!(
            "{{\"asset\":{{\"version\":\"2.0\",\"generator\":\"ReOpenSCAD\"}},\
             \"scene\":0,\"scenes\":[{{\"nodes\":[0]}}],\
             \"nodes\":[{{\"name\":{}}}]}}",
            json_string(title)
        )
    } else {
        format!(
            "{{\"asset\":{{\"version\":\"2.0\",\"generator\":\"ReOpenSCAD\"}},\
             \"scene\":0,\
             \"scenes\":[{{\"nodes\":[0]}}],\
             \"nodes\":[{{\"name\":{name},\"mesh\":0}}],\
             \"meshes\":[{{\"name\":{name},\"primitives\":[{{\"attributes\":\
             {{\"POSITION\":0,\"NORMAL\":1}},\"indices\":2,\"material\":0}}]}}],\
             \"materials\":[{{\"name\":\"ReOpenSCAD\",\"doubleSided\":false,\
             \"pbrMetallicRoughness\":{{\"baseColorFactor\":[0.95,0.77,0.29,1],\
             \"metallicFactor\":0,\"roughnessFactor\":0.55}}}}],\
             \"accessors\":[\
             {{\"bufferView\":0,\"componentType\":{COMPONENT_FLOAT},\"count\":{vertex_count},\
             \"type\":\"VEC3\",\"min\":{min},\"max\":{max}}},\
             {{\"bufferView\":1,\"componentType\":{COMPONENT_FLOAT},\"count\":{vertex_count},\
             \"type\":\"VEC3\"}},\
             {{\"bufferView\":2,\"componentType\":{COMPONENT_UNSIGNED_INT},\"count\":{index_count},\
             \"type\":\"SCALAR\"}}],\
             \"bufferViews\":[\
             {{\"buffer\":0,\"byteOffset\":0,\"byteLength\":{positions_length},\
             \"target\":{TARGET_ARRAY_BUFFER}}},\
             {{\"buffer\":0,\"byteOffset\":{positions_length},\"byteLength\":{normals_length},\
             \"target\":{TARGET_ARRAY_BUFFER}}},\
             {{\"buffer\":0,\"byteOffset\":{normals_offset},\"byteLength\":{indices_length},\
             \"target\":{TARGET_ELEMENT_ARRAY_BUFFER}}}],\
             \"buffers\":[{{\"byteLength\":{buffer_length}}}]}}",
            name = json_string(title),
            min = float_array(&primitive.min),
            max = float_array(&primitive.max),
            normals_offset = positions_length + normals_length,
            buffer_length = binary.len(),
        )
    };

    // Both chunks are padded to four bytes: JSON with spaces (still valid
    // JSON), BIN with zeros (unreferenced trailing bytes).
    let mut json_bytes = json.into_bytes();
    while json_bytes.len() % 4 != 0 {
        json_bytes.push(b' ');
    }
    while binary.len() % 4 != 0 {
        binary.push(0);
    }

    let mut output = Vec::with_capacity(28 + json_bytes.len() + binary.len());
    output.extend_from_slice(&GLB_MAGIC.to_le_bytes());
    output.extend_from_slice(&GLB_VERSION.to_le_bytes());
    let total = 12 + 8 + json_bytes.len() + if binary.is_empty() { 0 } else { 8 + binary.len() };
    output.extend_from_slice(&(total as u32).to_le_bytes());
    output.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    output.extend_from_slice(&CHUNK_JSON.to_le_bytes());
    output.extend_from_slice(&json_bytes);
    if !binary.is_empty() {
        output.extend_from_slice(&(binary.len() as u32).to_le_bytes());
        output.extend_from_slice(&CHUNK_BIN.to_le_bytes());
        output.extend_from_slice(&binary);
    }
    Some(output)
}

/// `f32` rendered so that JSON keeps it a number: `Display` on a whole float
/// prints `1`, which is a valid JSON number, and never reaches for exponent
/// notation, so nothing else needs fixing up.
fn float_array(values: &[f32; 3]) -> String {
    let render = |value: f32| {
        if value.is_finite() {
            format!("{value}")
        } else {
            "0".into()
        }
    };
    format!(
        "[{},{},{}]",
        render(values[0]),
        render(values[1]),
        render(values[2])
    )
}

/// A JSON string literal. The title is arbitrary user text, so it is escaped
/// here rather than trusted; control characters take the `\u` form the JSON
/// grammar requires.
fn json_string(text: &str) -> String {
    let mut output = String::with_capacity(text.len() + 2);
    output.push('"');
    for character in text.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                output.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => output.push(c),
        }
    }
    output.push('"');
    output
}

#[cfg(test)]
mod tests {
    /// `write` takes an output budget and refuses anything past it, which is
    /// the server's protection rather than a property these tests are about.
    /// They all hand it an unlimited one and unwrap — a test model that did
    /// not fit would be a bug in the test, not a case to handle.
    fn rendered(mesh: &Mesh, title: &str) -> Vec<u8> {
        write(mesh, title, usize::MAX).expect("the test model fits any budget")
    }

    use super::*;
    use crate::engine::{self, Quality};
    use serde_json::Value;

    fn compile(source: &str) -> Mesh {
        engine::compile(source, Quality::Render, None)
            .expect("compiles")
            .mesh
    }

    /// Splits a `.glb` into its header fields and chunks, the way a loader
    /// does, so a wrong length or a missing pad is a test failure rather than
    /// a viewer's silent refusal.
    fn chunks(glb: &[u8]) -> (Value, Vec<u8>) {
        // Compared as bytes, not against the constant the writer used, so a
        // transposed tag cannot agree with itself.
        assert_eq!(&glb[0..4], b"glTF", "glTF magic");
        assert_eq!(u32::from_le_bytes(glb[0..4].try_into().unwrap()), GLB_MAGIC);
        assert_eq!(u32::from_le_bytes(glb[4..8].try_into().unwrap()), 2);
        assert_eq!(
            u32::from_le_bytes(glb[8..12].try_into().unwrap()) as usize,
            glb.len(),
            "declared length must be the file length"
        );
        let mut cursor = 12;
        let mut json = None;
        let mut binary = Vec::new();
        while cursor < glb.len() {
            let length = u32::from_le_bytes(glb[cursor..cursor + 4].try_into().unwrap()) as usize;
            let kind = u32::from_le_bytes(glb[cursor + 4..cursor + 8].try_into().unwrap());
            assert_eq!(length % 4, 0, "chunks are four-byte aligned");
            let body = &glb[cursor + 8..cursor + 8 + length];
            match &glb[cursor + 4..cursor + 8] {
                b"JSON" => {
                    assert_eq!(kind, CHUNK_JSON);
                    json = Some(serde_json::from_slice(body).expect("chunk 0 is JSON"));
                }
                b"BIN\0" => {
                    assert_eq!(kind, CHUNK_BIN);
                    binary = body.to_vec();
                }
                other => panic!("unknown chunk tag {other:?}"),
            }
            cursor += 8 + length;
        }
        assert_eq!(cursor, glb.len());
        (json.expect("JSON chunk"), binary)
    }

    #[test]
    fn a_cube_is_a_valid_glb_with_flat_shaded_corners() {
        let (json, binary) = chunks(&rendered(&compile("cube(10);"), "cube"));
        assert_eq!(json["asset"]["version"], "2.0");
        assert_eq!(json["scene"], 0);
        assert_eq!(json["nodes"][0]["name"], "cube");
        // Eight geometric corners, three faces each: 24 vertices, and 12
        // triangles' worth of indices.
        assert_eq!(json["accessors"][0]["count"], 24);
        assert_eq!(json["accessors"][1]["count"], 24);
        assert_eq!(json["accessors"][2]["count"], 36);
        assert_eq!(binary.len(), 24 * 12 + 24 * 12 + 36 * 4);
    }

    /// Every accessor has to stay inside the buffer view it names, and every
    /// view inside the buffer: this is exactly what a loader checks before it
    /// gives up on the file.
    #[test]
    fn accessors_and_views_stay_inside_the_buffer() {
        for source in ["cube(10);", "sphere(r=5,$fn=24);", "cylinder(h=4,r=3,$fn=9);"] {
            let (json, binary) = chunks(&rendered(&compile(source), "unit test"));
            let buffer_length = json["buffers"][0]["byteLength"].as_u64().unwrap() as usize;
            assert!(buffer_length <= binary.len(), "{source}");
            // The BIN chunk may carry up to three padding bytes past the
            // declared buffer length, never more.
            assert!(binary.len() - buffer_length < 4, "{source}");
            for view in json["bufferViews"].as_array().unwrap() {
                assert_eq!(view["buffer"], 0);
                let offset = view["byteOffset"].as_u64().unwrap() as usize;
                let length = view["byteLength"].as_u64().unwrap() as usize;
                assert_eq!(offset % 4, 0, "{source}: accessor alignment");
                assert!(offset + length <= buffer_length, "{source}");
            }
            for accessor in json["accessors"].as_array().unwrap() {
                let view = accessor["bufferView"].as_u64().unwrap() as usize;
                let count = accessor["count"].as_u64().unwrap() as usize;
                let stride = match accessor["componentType"].as_u64().unwrap() as u32 {
                    COMPONENT_FLOAT | COMPONENT_UNSIGNED_INT => 4,
                    other => panic!("unexpected component type {other}"),
                };
                let components = match accessor["type"].as_str().unwrap() {
                    "VEC3" => 3,
                    "SCALAR" => 1,
                    other => panic!("unexpected accessor type {other}"),
                };
                let needed = count * stride * components;
                let available = json["bufferViews"][view]["byteLength"].as_u64().unwrap() as usize;
                assert_eq!(needed, available, "{source}: accessor {accessor} vs view");
            }
        }
    }

    /// Indices that run past the vertex array are the other way a glTF file
    /// fails to load, and the one a hand-written writer gets wrong.
    #[test]
    fn every_index_addresses_a_real_vertex() {
        let (json, binary) = chunks(&rendered(&compile("sphere(r=6,$fn=20);"), "sphere"));
        let vertices = json["accessors"][0]["count"].as_u64().unwrap() as u32;
        let view = json["accessors"][2]["bufferView"].as_u64().unwrap() as usize;
        let offset = json["bufferViews"][view]["byteOffset"].as_u64().unwrap() as usize;
        let length = json["bufferViews"][view]["byteLength"].as_u64().unwrap() as usize;
        assert_eq!(length % 4, 0);
        for chunk in binary[offset..offset + length].chunks_exact(4) {
            let index = u32::from_le_bytes(chunk.try_into().unwrap());
            assert!(index < vertices, "index {index} past {vertices} vertices");
        }
    }

    /// glTF is metres. A 10 mm cube must be 0.01 units across, or it lands in
    /// an AR scene ten metres wide.
    #[test]
    fn coordinates_are_metres_and_the_bounds_match_them() {
        let (json, binary) = chunks(&rendered(&compile("cube(10);"), "cube"));
        let corner = |key: &str| -> Vec<f64> {
            json["accessors"][0][key]
                .as_array()
                .expect("bounds")
                .iter()
                .map(|value| value.as_f64().expect("number"))
                .collect()
        };
        assert_eq!(corner("min"), vec![0.0, 0.0, 0.0]);
        assert_eq!(corner("max"), vec![0.01, 0.01, 0.01]);
        let length = json["bufferViews"][0]["byteLength"].as_u64().unwrap() as usize;
        for chunk in binary[..length].chunks_exact(4) {
            let value = f32::from_le_bytes(chunk.try_into().unwrap());
            assert!((0.0..=0.01).contains(&value), "{value} is not in metres");
        }
    }

    /// The reason positions are not welded on their own: a shared corner with
    /// one averaged normal renders a cube as a ball.
    #[test]
    fn normals_stay_per_face_so_edges_render_sharp() {
        let (json, binary) = chunks(&rendered(&compile("cube(10);"), "cube"));
        let view = json["accessors"][1]["bufferView"].as_u64().unwrap() as usize;
        let offset = json["bufferViews"][view]["byteOffset"].as_u64().unwrap() as usize;
        let length = json["bufferViews"][view]["byteLength"].as_u64().unwrap() as usize;
        let mut seen = std::collections::HashSet::new();
        for normal in binary[offset..offset + length].chunks_exact(12) {
            let axis: Vec<i32> = normal
                .chunks_exact(4)
                .map(|value| f32::from_le_bytes(value.try_into().unwrap()).round() as i32)
                .collect();
            // Every normal is exactly one axis of the cube, never a blend.
            assert_eq!(
                axis.iter().map(|value| value.abs()).sum::<i32>(),
                1,
                "{axis:?} is an averaged normal"
            );
            seen.insert(axis);
        }
        assert_eq!(seen.len(), 6, "one normal per cube face");
    }

    #[test]
    fn titles_are_json_escaped_and_the_file_still_parses() {
        let (json, _) = chunks(&rendered(&compile("cube(1);"), "a\"b\\c\td"));
        assert_eq!(json["nodes"][0]["name"], "a\"b\\c\td");
    }

    #[test]
    fn a_model_with_no_geometry_still_loads() {
        let glb = rendered(&Mesh::default(), "nothing");
        let (json, binary) = chunks(&glb);
        assert!(binary.is_empty());
        assert_eq!(json["scene"], 0);
        assert!(json["meshes"].is_null(), "an empty meshes array is invalid");
        assert_eq!(json["nodes"][0]["name"], "nothing");
    }

    #[test]
    fn the_export_is_deterministic() {
        let mesh = compile("difference(){ cube(10); sphere(r=6,$fn=16); }");
        assert_eq!(rendered(&mesh, "unit test"), rendered(&mesh, "unit test"));
    }
}
