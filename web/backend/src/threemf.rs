//! Dependency-free 3MF (3D Manufacturing Format) writer.
//!
//! A 3MF file is an OPC package: a ZIP archive that carries
//! `[Content_Types].xml`, `_rels/.rels`, and the model payload at
//! `3D/3dmodel.model`. The crate deliberately has no third-party
//! dependencies, so this module also contains a minimal ZIP writer that emits
//! *stored* (uncompressed) entries. Stored entries still need correct CRC-32
//! values, local file headers, a central directory and an end-of-central
//! directory record, which is all that slicers require to open the package.
//!
//! The writer welds the triangle soup produced by the geometry engine into an
//! indexed mesh, because 3MF's `<triangles>` element references vertices by
//! index and consumers reject meshes whose facets do not share vertices.

use crate::engine::{Mesh, Vec3};
use std::collections::HashMap;

/// Coordinate quantisation used for both vertex welding and text output.
///
/// Welding and printing must agree: if two vertices are collapsed they have to
/// serialise identically, otherwise the emitted mesh would not be watertight.
const COORDINATE_SCALE: f64 = 1.0e6;

const CONTENT_TYPES_PATH: &str = "[Content_Types].xml";
const RELS_PATH: &str = "_rels/.rels";
const MODEL_PATH: &str = "3D/3dmodel.model";

const CONTENT_TYPES_XML: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n",
    "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">",
    "<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\" />",
    "<Default Extension=\"model\" ContentType=\"application/vnd.ms-package.3dmanufacturing-3dmodel+xml\" />",
    "</Types>\r\n"
);

const RELS_XML: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n",
    "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    "<Relationship Id=\"rel0\" Target=\"/3D/3dmodel.model\" ",
    "Type=\"http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel\" />",
    "</Relationships>\r\n"
);

/// An indexed, welded mesh: exactly what 3MF's `<mesh>` element expresses.
#[derive(Debug, Default)]
pub struct IndexedMesh {
    pub vertices: Vec<Vec3>,
    pub triangles: Vec<[u32; 3]>,
}

impl IndexedMesh {
    /// Weld a triangle soup into shared vertices.
    ///
    /// Degenerate facets (two or more corners collapsing onto the same welded
    /// vertex) are dropped: they carry no volume and make consumers flag the
    /// mesh as non-manifold. Winding is normalised against the facet normal
    /// recorded by the engine so that 3MF's "counter-clockwise seen from
    /// outside" rule holds.
    pub fn weld(mesh: &Mesh) -> Self {
        Self::weld_on_grid(mesh, COORDINATE_SCALE)
    }

    /// Weld onto an arbitrary grid, where `scale` is the reciprocal of the cell
    /// size.
    ///
    /// A coarser grid collapses neighbouring vertices, which turns the facets
    /// between them degenerate and drops them: vertex clustering, i.e. a cheap
    /// decimation for previews. 3MF export always uses [`COORDINATE_SCALE`] so
    /// that welding and text serialisation agree exactly.
    pub fn weld_on_grid(mesh: &Mesh, scale: f64) -> Self {
        let mut vertices = Vec::new();
        let mut triangles = Vec::new();
        let mut lookup: HashMap<[i64; 3], u32> = HashMap::new();
        for triangle in &mesh.triangles {
            let mut indices = [0u32; 3];
            for (slot, vertex) in triangle.vertices.iter().enumerate() {
                let key = quantized_key(*vertex, scale);
                let next = lookup.len() as u32;
                let index = *lookup.entry(key).or_insert_with(|| {
                    vertices.push(Vec3 {
                        x: dequantize(key[0], scale),
                        y: dequantize(key[1], scale),
                        z: dequantize(key[2], scale),
                    });
                    next
                });
                indices[slot] = index;
            }
            if indices[0] == indices[1] || indices[1] == indices[2] || indices[0] == indices[2] {
                continue;
            }
            let a = vertices[indices[0] as usize];
            let b = vertices[indices[1] as usize];
            let c = vertices[indices[2] as usize];
            if geometric_normal(a, b, c).dot(triangle.normal) < 0.0 {
                indices.swap(1, 2);
            }
            triangles.push(indices);
        }
        Self {
            vertices,
            triangles,
        }
    }
}

fn quantized_key(vertex: Vec3, scale: f64) -> [i64; 3] {
    [
        quantize(vertex.x, scale),
        quantize(vertex.y, scale),
        quantize(vertex.z, scale),
    ]
}

/// Non-finite coordinates cannot be expressed in 3MF's numeric type; clamping
/// them to zero keeps the package parseable instead of emitting `NaN`.
fn quantize(value: f64, scale: f64) -> i64 {
    if !value.is_finite() {
        return 0;
    }
    (value * scale).round() as i64
}

fn dequantize(value: i64, scale: f64) -> f64 {
    value as f64 / scale
}

#[derive(Clone, Copy)]
struct Normal {
    x: f64,
    y: f64,
    z: f64,
}

impl Normal {
    fn dot(self, other: Vec3) -> f64 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }
}

fn geometric_normal(a: Vec3, b: Vec3, c: Vec3) -> Normal {
    let (ux, uy, uz) = (b.x - a.x, b.y - a.y, b.z - a.z);
    let (vx, vy, vz) = (c.x - a.x, c.y - a.y, c.z - a.z);
    Normal {
        x: uy * vz - uz * vy,
        y: uz * vx - ux * vz,
        z: ux * vy - uy * vx,
    }
}

/// Render one welded coordinate. The grid is 1e-6 mm, so six decimals are
/// lossless here, and trailing zeros are trimmed to keep the XML small.
fn format_coordinate(value: f64) -> String {
    let mut text = format!("{value:.6}");
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    if text == "-0" || text.is_empty() {
        text = "0".into();
    }
    text
}

/// Serialise `3D/3dmodel.model` for a welded mesh.
pub fn model_xml(mesh: &IndexedMesh, title: &str) -> String {
    let mut xml = String::with_capacity(64 * mesh.vertices.len() + 48 * mesh.triangles.len() + 512);
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n");
    xml.push_str(
        "<model unit=\"millimeter\" xml:lang=\"en-US\" \
         xmlns=\"http://schemas.microsoft.com/3dmanufacturing/core/2015/02\">\r\n",
    );
    xml.push_str("<metadata name=\"Application\">ReOpenSCAD</metadata>\r\n");
    xml.push_str("<metadata name=\"Title\">");
    xml.push_str(&escape_xml(title));
    xml.push_str("</metadata>\r\n");
    xml.push_str("<resources>\r\n<object id=\"1\" type=\"model\">\r\n<mesh>\r\n<vertices>\r\n");
    for vertex in &mesh.vertices {
        xml.push_str("<vertex x=\"");
        xml.push_str(&format_coordinate(vertex.x));
        xml.push_str("\" y=\"");
        xml.push_str(&format_coordinate(vertex.y));
        xml.push_str("\" z=\"");
        xml.push_str(&format_coordinate(vertex.z));
        xml.push_str("\" />\r\n");
    }
    xml.push_str("</vertices>\r\n<triangles>\r\n");
    for triangle in &mesh.triangles {
        xml.push_str("<triangle v1=\"");
        xml.push_str(&triangle[0].to_string());
        xml.push_str("\" v2=\"");
        xml.push_str(&triangle[1].to_string());
        xml.push_str("\" v3=\"");
        xml.push_str(&triangle[2].to_string());
        xml.push_str("\" />\r\n");
    }
    xml.push_str("</triangles>\r\n</mesh>\r\n</object>\r\n</resources>\r\n");
    xml.push_str("<build>\r\n<item objectid=\"1\" />\r\n</build>\r\n</model>\r\n");
    xml
}

fn escape_xml(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&apos;"),
            // XML 1.0 forbids most C0 controls outright; drop them rather than
            // emit a package no parser will accept.
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' && c != '\r' => {}
            c => output.push(c),
        }
    }
    output
}

/// Build a complete 3MF package for `mesh`.
pub fn write(mesh: &Mesh, title: &str) -> Vec<u8> {
    let indexed = IndexedMesh::weld(mesh);
    let model = model_xml(&indexed, title);
    let mut zip = ZipWriter::default();
    zip.add(CONTENT_TYPES_PATH, CONTENT_TYPES_XML.as_bytes());
    zip.add(RELS_PATH, RELS_XML.as_bytes());
    zip.add(MODEL_PATH, model.as_bytes());
    zip.finish()
}

// ---------------------------------------------------------------------------
// Minimal ZIP (stored entries only)
// ---------------------------------------------------------------------------

/// Fixed MS-DOS timestamp (1980-01-01 00:00:00).
///
/// A constant stamp keeps exports byte-for-byte reproducible, which makes the
/// packages diffable in tests and caches.
const DOS_TIME: u16 = 0;
const DOS_DATE: u16 = 0x0021;

const LOCAL_HEADER_SIGNATURE: u32 = 0x0403_4b50;
const CENTRAL_HEADER_SIGNATURE: u32 = 0x0201_4b50;
const EOCD_SIGNATURE: u32 = 0x0605_4b50;

struct ZipEntry {
    name: String,
    crc32: u32,
    size: u32,
    offset: u32,
}

#[derive(Default)]
struct ZipWriter {
    buffer: Vec<u8>,
    entries: Vec<ZipEntry>,
}

impl ZipWriter {
    fn add(&mut self, name: &str, data: &[u8]) {
        let offset = self.buffer.len() as u32;
        let crc32 = crc32(data);
        let size = data.len() as u32;
        push_u32(&mut self.buffer, LOCAL_HEADER_SIGNATURE);
        push_u16(&mut self.buffer, 20); // version needed to extract
        push_u16(&mut self.buffer, 0); // general purpose flags
        push_u16(&mut self.buffer, 0); // compression method: stored
        push_u16(&mut self.buffer, DOS_TIME);
        push_u16(&mut self.buffer, DOS_DATE);
        push_u32(&mut self.buffer, crc32);
        push_u32(&mut self.buffer, size); // compressed size
        push_u32(&mut self.buffer, size); // uncompressed size
        push_u16(&mut self.buffer, name.len() as u16);
        push_u16(&mut self.buffer, 0); // extra field length
        self.buffer.extend_from_slice(name.as_bytes());
        self.buffer.extend_from_slice(data);
        self.entries.push(ZipEntry {
            name: name.to_string(),
            crc32,
            size,
            offset,
        });
    }

    fn finish(mut self) -> Vec<u8> {
        let directory_offset = self.buffer.len() as u32;
        for entry in &self.entries {
            push_u32(&mut self.buffer, CENTRAL_HEADER_SIGNATURE);
            push_u16(&mut self.buffer, 20); // version made by
            push_u16(&mut self.buffer, 20); // version needed to extract
            push_u16(&mut self.buffer, 0); // flags
            push_u16(&mut self.buffer, 0); // stored
            push_u16(&mut self.buffer, DOS_TIME);
            push_u16(&mut self.buffer, DOS_DATE);
            push_u32(&mut self.buffer, entry.crc32);
            push_u32(&mut self.buffer, entry.size);
            push_u32(&mut self.buffer, entry.size);
            push_u16(&mut self.buffer, entry.name.len() as u16);
            push_u16(&mut self.buffer, 0); // extra length
            push_u16(&mut self.buffer, 0); // comment length
            push_u16(&mut self.buffer, 0); // disk number start
            push_u16(&mut self.buffer, 0); // internal attributes
            push_u32(&mut self.buffer, 0); // external attributes
            push_u32(&mut self.buffer, entry.offset);
            self.buffer.extend_from_slice(entry.name.as_bytes());
        }
        let directory_size = self.buffer.len() as u32 - directory_offset;
        let count = self.entries.len() as u16;
        push_u32(&mut self.buffer, EOCD_SIGNATURE);
        push_u16(&mut self.buffer, 0); // this disk
        push_u16(&mut self.buffer, 0); // disk with central directory
        push_u16(&mut self.buffer, count);
        push_u16(&mut self.buffer, count);
        push_u32(&mut self.buffer, directory_size);
        push_u32(&mut self.buffer, directory_offset);
        push_u16(&mut self.buffer, 0); // comment length
        self.buffer
    }
}

fn push_u16(buffer: &mut Vec<u8>, value: u16) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(buffer: &mut Vec<u8>, value: u32) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

/// CRC-32/ISO-HDLC, the checksum ZIP central directories carry.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{self, Quality};

    /// Read back the stored entries of a package produced by [`ZipWriter`].
    ///
    /// The reader walks the central directory (not the local headers) exactly
    /// as an unzip implementation does, so it catches wrong offsets, wrong
    /// sizes and wrong CRCs rather than trusting what the writer believed.
    fn unzip(package: &[u8]) -> Vec<(String, Vec<u8>)> {
        let eocd = (0..package.len().saturating_sub(21))
            .rev()
            .find(|offset| package[*offset..*offset + 4] == EOCD_SIGNATURE.to_le_bytes())
            .expect("end of central directory record");
        let count = u16::from_le_bytes([package[eocd + 10], package[eocd + 11]]) as usize;
        let directory_offset =
            u32::from_le_bytes(package[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
        let mut cursor = directory_offset;
        let mut entries = Vec::new();
        for _ in 0..count {
            assert_eq!(
                package[cursor..cursor + 4],
                CENTRAL_HEADER_SIGNATURE.to_le_bytes()
            );
            let crc = u32::from_le_bytes(package[cursor + 16..cursor + 20].try_into().unwrap());
            let size =
                u32::from_le_bytes(package[cursor + 24..cursor + 28].try_into().unwrap()) as usize;
            let name_length =
                u16::from_le_bytes([package[cursor + 28], package[cursor + 29]]) as usize;
            let extra_length =
                u16::from_le_bytes([package[cursor + 30], package[cursor + 31]]) as usize;
            let comment_length =
                u16::from_le_bytes([package[cursor + 32], package[cursor + 33]]) as usize;
            let local_offset =
                u32::from_le_bytes(package[cursor + 42..cursor + 46].try_into().unwrap()) as usize;
            let name =
                String::from_utf8(package[cursor + 46..cursor + 46 + name_length].to_vec()).unwrap();
            cursor += 46 + name_length + extra_length + comment_length;

            assert_eq!(
                package[local_offset..local_offset + 4],
                LOCAL_HEADER_SIGNATURE.to_le_bytes(),
                "local header signature for {name}"
            );
            let local_name_length =
                u16::from_le_bytes([package[local_offset + 26], package[local_offset + 27]])
                    as usize;
            let local_extra_length =
                u16::from_le_bytes([package[local_offset + 28], package[local_offset + 29]])
                    as usize;
            let data_offset = local_offset + 30 + local_name_length + local_extra_length;
            let data = package[data_offset..data_offset + size].to_vec();
            assert_eq!(crc32(&data), crc, "crc32 mismatch for {name}");
            entries.push((name, data));
        }
        assert_eq!(cursor, eocd, "central directory must end at the EOCD record");
        entries
    }

    fn unit_cube() -> engine::Mesh {
        engine::compile("cube(10);", Quality::Render, None)
            .unwrap()
            .mesh
    }

    #[test]
    fn crc32_matches_the_reference_check_value() {
        // The published CRC-32/ISO-HDLC check value for "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn package_contains_the_three_required_opc_parts() {
        let package = write(&unit_cube(), "cube");
        let entries = unzip(&package);
        let names: Vec<_> = entries.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            vec![CONTENT_TYPES_PATH, RELS_PATH, MODEL_PATH],
            "3MF requires content types, package relationships and the model part"
        );
        let model = String::from_utf8(entries[2].1.clone()).unwrap();
        assert!(model.contains(
            "xmlns=\"http://schemas.microsoft.com/3dmanufacturing/core/2015/02\""
        ));
        assert!(model.contains("unit=\"millimeter\""));
        assert!(model.contains("<build>"));
        assert!(model.contains("<item objectid=\"1\" />"));
    }

    #[test]
    fn welding_produces_an_indexed_manifold_cube() {
        let mesh = unit_cube();
        let indexed = IndexedMesh::weld(&mesh);
        assert_eq!(indexed.triangles.len(), mesh.triangles.len());
        // A closed cube surface has V - E + F = 2; with F triangles and every
        // edge shared by exactly two facets, E = 3F/2, so V = F/2 + 2.
        assert_eq!(indexed.vertices.len(), indexed.triangles.len() / 2 + 2);
        // Every directed edge must appear exactly once for a closed, coherently
        // oriented (manifold) surface.
        let mut directed = std::collections::HashSet::new();
        for triangle in &indexed.triangles {
            for pair in [
                (triangle[0], triangle[1]),
                (triangle[1], triangle[2]),
                (triangle[2], triangle[0]),
            ] {
                assert!(directed.insert(pair), "duplicated directed edge {pair:?}");
            }
        }
        for triangle in &indexed.triangles {
            for pair in [
                (triangle[0], triangle[1]),
                (triangle[1], triangle[2]),
                (triangle[2], triangle[0]),
            ] {
                assert!(
                    directed.contains(&(pair.1, pair.0)),
                    "edge {pair:?} has no opposite twin"
                );
            }
        }
    }

    #[test]
    fn triangle_indices_stay_inside_the_vertex_array() {
        let indexed = IndexedMesh::weld(&unit_cube());
        for triangle in &indexed.triangles {
            for index in triangle {
                assert!((*index as usize) < indexed.vertices.len());
            }
            assert!(triangle[0] != triangle[1] && triangle[1] != triangle[2]);
        }
    }

    #[test]
    fn degenerate_and_non_finite_facets_are_dropped() {
        let point = Vec3 {
            x: f64::NAN,
            y: 0.0,
            z: 0.0,
        };
        let mesh = Mesh {
            triangles: vec![engine::Triangle {
                vertices: [point, point, point],
                normal: Vec3 {
                    x: 0.0,
                    y: 0.0,
                    z: 1.0,
                },
            }],
        };
        let indexed = IndexedMesh::weld(&mesh);
        assert!(indexed.triangles.is_empty());
        let model = model_xml(&indexed, "empty");
        assert!(!model.contains("NaN"), "coordinates must stay numeric");
    }

    #[test]
    fn titles_are_xml_escaped() {
        let indexed = IndexedMesh::weld(&unit_cube());
        let model = model_xml(&indexed, "a<b>&\"'");
        assert!(model.contains("a&lt;b&gt;&amp;&quot;&apos;"));
        assert!(!model.contains("<b>"));
    }

    #[test]
    fn coordinates_never_use_exponent_notation() {
        assert_eq!(format_coordinate(0.0), "0");
        assert_eq!(format_coordinate(-0.0), "0");
        assert_eq!(format_coordinate(1.0e-7), "0");
        assert_eq!(format_coordinate(12.5), "12.5");
        assert_eq!(format_coordinate(-3.25), "-3.25");
        assert!(!format_coordinate(1.0e12).contains('e'));
    }
}
