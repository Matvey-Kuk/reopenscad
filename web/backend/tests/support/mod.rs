use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point(pub [f64; 3]);

#[derive(Clone, Debug, PartialEq)]
pub struct Facet {
    pub normal: Point,
    pub vertices: [Point; 3],
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StlMesh {
    pub facets: Vec<Facet>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MeshMetrics {
    pub facets: usize,
    pub vertices: usize,
    pub edges: usize,
    pub boundary_edges: usize,
    pub non_manifold_edges: usize,
    pub components: usize,
    pub euler_characteristic: isize,
    pub bounds_min: [f64; 3],
    pub bounds_max: [f64; 3],
    pub surface_area: f64,
    pub volume: f64,
    pub duplicate_facets: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct ComparisonTolerance {
    pub coordinate: f64,
    pub bounds: f64,
    pub relative_area: f64,
    pub relative_volume: f64,
    pub unmatched_facets: usize,
}

impl Default for ComparisonTolerance {
    fn default() -> Self {
        Self {
            coordinate: 1.0e-4,
            bounds: 1.0e-4,
            relative_area: 1.0e-4,
            relative_volume: 1.0e-4,
            unmatched_facets: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ComparisonReport {
    pub actual: MeshMetrics,
    pub expected: MeshMetrics,
    pub actual_only_facets: usize,
    pub expected_only_facets: usize,
    pub failures: Vec<String>,
}

impl ComparisonReport {
    pub fn is_match(&self) -> bool {
        self.failures.is_empty()
    }
}

impl fmt::Display for ComparisonReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "mesh parity check failed:")?;
        for failure in &self.failures {
            writeln!(formatter, "  - {failure}")?;
        }
        writeln!(
            formatter,
            "  actual:   facets={}, vertices={}, edges={}, boundary={}, non-manifold={}, components={}, euler={}, bounds={:?}..{:?}, area={:.9}, volume={:.9}, duplicate facets={}",
            self.actual.facets,
            self.actual.vertices,
            self.actual.edges,
            self.actual.boundary_edges,
            self.actual.non_manifold_edges,
            self.actual.components,
            self.actual.euler_characteristic,
            self.actual.bounds_min,
            self.actual.bounds_max,
            self.actual.surface_area,
            self.actual.volume,
            self.actual.duplicate_facets,
        )?;
        writeln!(
            formatter,
            "  expected: facets={}, vertices={}, edges={}, boundary={}, non-manifold={}, components={}, euler={}, bounds={:?}..{:?}, area={:.9}, volume={:.9}, duplicate facets={}",
            self.expected.facets,
            self.expected.vertices,
            self.expected.edges,
            self.expected.boundary_edges,
            self.expected.non_manifold_edges,
            self.expected.components,
            self.expected.euler_characteristic,
            self.expected.bounds_min,
            self.expected.bounds_max,
            self.expected.surface_area,
            self.expected.volume,
            self.expected.duplicate_facets,
        )?;
        write!(
            formatter,
            "  normalized facet delta: {} actual-only, {} expected-only",
            self.actual_only_facets, self.expected_only_facets
        )
    }
}

pub fn read_stl(path: &Path) -> Result<StlMesh, String> {
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    parse_stl(&bytes).map_err(|error| format!("parse {}: {error}", path.display()))
}

/// Parse binary or ASCII STL. Detection deliberately does not rely on the
/// `solid` prefix because valid binary STL headers commonly start with it.
pub fn parse_stl(bytes: &[u8]) -> Result<StlMesh, String> {
    if let Some(expected_len) = binary_stl_length(bytes) {
        if expected_len == bytes.len() {
            return parse_binary_stl(bytes);
        }
    }
    parse_ascii_stl(bytes).or_else(|ascii_error| {
        if binary_stl_length(bytes).is_some() {
            parse_binary_stl(bytes).map_err(|binary_error| {
                format!("not valid ASCII ({ascii_error}) or binary STL ({binary_error})")
            })
        } else {
            Err(ascii_error)
        }
    })
}

fn binary_stl_length(bytes: &[u8]) -> Option<usize> {
    let count = u32::from_le_bytes(bytes.get(80..84)?.try_into().ok()?) as usize;
    count.checked_mul(50)?.checked_add(84)
}

fn parse_binary_stl(bytes: &[u8]) -> Result<StlMesh, String> {
    let expected = binary_stl_length(bytes)
        .ok_or_else(|| "binary STL is shorter than its 84-byte header".to_owned())?;
    if expected != bytes.len() {
        return Err(format!(
            "binary facet count requires {expected} bytes, file has {}",
            bytes.len()
        ));
    }
    let facet_count = (bytes.len() - 84) / 50;
    let mut facets = Vec::with_capacity(facet_count);
    for index in 0..facet_count {
        let start = 84 + index * 50;
        let mut values = [0.0; 12];
        for (value_index, value) in values.iter_mut().enumerate() {
            let offset = start + value_index * 4;
            *value = f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as f64;
            if !value.is_finite() {
                return Err(format!("facet {index} contains a non-finite coordinate"));
            }
        }
        facets.push(Facet {
            normal: Point([values[0], values[1], values[2]]),
            vertices: [
                Point([values[3], values[4], values[5]]),
                Point([values[6], values[7], values[8]]),
                Point([values[9], values[10], values[11]]),
            ],
        });
    }
    Ok(StlMesh { facets })
}

fn parse_ascii_stl(bytes: &[u8]) -> Result<StlMesh, String> {
    let input = std::str::from_utf8(bytes).map_err(|_| "ASCII STL is not UTF-8".to_owned())?;
    let mut words = input.split_whitespace();
    expect_word(&mut words, "solid")?;

    // The optional solid name occupies the rest of the first line. Token-level
    // parsing cannot identify it, so advance until the first facet or endsolid.
    let mut token = words
        .next()
        .ok_or_else(|| "ASCII STL is truncated".to_owned())?;
    while !token.eq_ignore_ascii_case("facet") && !token.eq_ignore_ascii_case("endsolid") {
        token = words
            .next()
            .ok_or_else(|| "ASCII STL has no facets or endsolid".to_owned())?;
    }

    let mut facets = Vec::new();
    loop {
        if token.eq_ignore_ascii_case("endsolid") {
            break;
        }
        expect_word(&mut words, "normal")?;
        let normal = parse_point(&mut words, "facet normal")?;
        expect_word(&mut words, "outer")?;
        expect_word(&mut words, "loop")?;
        let mut vertices = [Point([0.0; 3]); 3];
        for vertex in &mut vertices {
            expect_word(&mut words, "vertex")?;
            *vertex = parse_point(&mut words, "vertex")?;
        }
        expect_word(&mut words, "endloop")?;
        expect_word(&mut words, "endfacet")?;
        facets.push(Facet { normal, vertices });
        token = words
            .next()
            .ok_or_else(|| "ASCII STL is missing endsolid".to_owned())?;
    }
    Ok(StlMesh { facets })
}

fn expect_word<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    expected: &str,
) -> Result<(), String> {
    let actual = words
        .next()
        .ok_or_else(|| format!("expected {expected}, reached end of ASCII STL"))?;
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!("expected {expected}, found {actual}"))
    }
}

fn parse_point<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    context: &str,
) -> Result<Point, String> {
    let mut point = [0.0; 3];
    for coordinate in &mut point {
        let word = words
            .next()
            .ok_or_else(|| format!("{context} is missing a coordinate"))?;
        *coordinate = word
            .parse::<f64>()
            .map_err(|_| format!("invalid {context} coordinate: {word}"))?;
        if !coordinate.is_finite() {
            return Err(format!("{context} contains a non-finite coordinate"));
        }
    }
    Ok(Point(point))
}

pub fn metrics(mesh: &StlMesh, coordinate_tolerance: f64) -> MeshMetrics {
    assert!(coordinate_tolerance.is_finite() && coordinate_tolerance > 0.0);
    let mut vertex_ids = BTreeMap::<[i64; 3], usize>::new();
    let mut edges = HashMap::<(usize, usize), usize>::new();
    let mut parent = Vec::<usize>::new();
    let mut normalized_facets = HashSet::new();
    let mut bounds_min = [f64::INFINITY; 3];
    let mut bounds_max = [f64::NEG_INFINITY; 3];
    let mut surface_area = 0.0;
    let mut signed_volume = 0.0;

    for facet in &mesh.facets {
        let mut ids = [0; 3];
        for (index, vertex) in facet.vertices.iter().enumerate() {
            for axis in 0..3 {
                bounds_min[axis] = bounds_min[axis].min(vertex.0[axis]);
                bounds_max[axis] = bounds_max[axis].max(vertex.0[axis]);
            }
            let key = quantize(vertex.0, coordinate_tolerance);
            ids[index] = if let Some(id) = vertex_ids.get(&key) {
                *id
            } else {
                let id = parent.len();
                vertex_ids.insert(key, id);
                parent.push(id);
                id
            };
        }
        union(&mut parent, ids[0], ids[1]);
        union(&mut parent, ids[1], ids[2]);
        union(&mut parent, ids[2], ids[0]);
        for (left, right) in [(ids[0], ids[1]), (ids[1], ids[2]), (ids[2], ids[0])] {
            let edge = if left <= right {
                (left, right)
            } else {
                (right, left)
            };
            *edges.entry(edge).or_default() += 1;
        }
        let a = facet.vertices[0].0;
        let b = facet.vertices[1].0;
        let c = facet.vertices[2].0;
        let face_cross = cross(sub(b, a), sub(c, a));
        surface_area += length(face_cross) * 0.5;
        signed_volume += dot(a, cross(b, c)) / 6.0;

        let mut triangle = [
            quantize(a, coordinate_tolerance),
            quantize(b, coordinate_tolerance),
            quantize(c, coordinate_tolerance),
        ];
        triangle.sort_unstable();
        normalized_facets.insert(triangle);
    }

    if mesh.facets.is_empty() {
        bounds_min = [0.0; 3];
        bounds_max = [0.0; 3];
    }
    let components = (0..parent.len())
        .map(|index| find(&mut parent, index))
        .collect::<BTreeSet<_>>()
        .len();
    let boundary_edges = edges.values().filter(|count| **count == 1).count();
    let non_manifold_edges = edges.values().filter(|count| **count > 2).count();
    MeshMetrics {
        facets: mesh.facets.len(),
        vertices: vertex_ids.len(),
        edges: edges.len(),
        boundary_edges,
        non_manifold_edges,
        components,
        euler_characteristic: vertex_ids.len() as isize - edges.len() as isize
            + mesh.facets.len() as isize,
        bounds_min,
        bounds_max,
        surface_area,
        volume: signed_volume.abs(),
        duplicate_facets: mesh.facets.len() - normalized_facets.len(),
    }
}

pub fn compare_meshes(
    actual: &StlMesh,
    expected: &StlMesh,
    tolerance: ComparisonTolerance,
) -> ComparisonReport {
    assert!(tolerance.coordinate.is_finite() && tolerance.coordinate > 0.0);
    let actual_metrics = metrics(actual, tolerance.coordinate);
    let expected_metrics = metrics(expected, tolerance.coordinate);
    let actual_facets = normalized_facets(actual, tolerance.coordinate);
    let expected_facets = normalized_facets(expected, tolerance.coordinate);
    let actual_only = actual_facets.difference(&expected_facets).count();
    let expected_only = expected_facets.difference(&actual_facets).count();
    let mut failures = Vec::new();

    if actual_metrics.facets != expected_metrics.facets {
        failures.push(format!(
            "facet count differs by {} ({} vs {})",
            actual_metrics.facets.abs_diff(expected_metrics.facets),
            actual_metrics.facets,
            expected_metrics.facets
        ));
    }
    for axis in 0..3 {
        check_absolute(
            &mut failures,
            &format!("minimum bound axis {axis}"),
            actual_metrics.bounds_min[axis],
            expected_metrics.bounds_min[axis],
            tolerance.bounds,
        );
        check_absolute(
            &mut failures,
            &format!("maximum bound axis {axis}"),
            actual_metrics.bounds_max[axis],
            expected_metrics.bounds_max[axis],
            tolerance.bounds,
        );
    }
    check_relative(
        &mut failures,
        "surface area",
        actual_metrics.surface_area,
        expected_metrics.surface_area,
        tolerance.relative_area,
    );
    check_relative(
        &mut failures,
        "enclosed volume",
        actual_metrics.volume,
        expected_metrics.volume,
        tolerance.relative_volume,
    );
    for (name, actual_value, expected_value) in [
        (
            "boundary edge count",
            actual_metrics.boundary_edges,
            expected_metrics.boundary_edges,
        ),
        (
            "non-manifold edge count",
            actual_metrics.non_manifold_edges,
            expected_metrics.non_manifold_edges,
        ),
        (
            "connected component count",
            actual_metrics.components,
            expected_metrics.components,
        ),
        (
            "duplicate facet count",
            actual_metrics.duplicate_facets,
            expected_metrics.duplicate_facets,
        ),
    ] {
        if actual_value != expected_value {
            failures.push(format!(
                "{name} differs: {actual_value} vs {expected_value}"
            ));
        }
    }
    if actual_metrics.euler_characteristic != expected_metrics.euler_characteristic {
        failures.push(format!(
            "Euler characteristic differs: {} vs {}",
            actual_metrics.euler_characteristic, expected_metrics.euler_characteristic
        ));
    }
    if actual_only > tolerance.unmatched_facets || expected_only > tolerance.unmatched_facets {
        failures.push(format!(
            "normalized facets differ: {actual_only} actual-only and {expected_only} expected-only (allowed {})",
            tolerance.unmatched_facets
        ));
    }

    ComparisonReport {
        actual: actual_metrics,
        expected: expected_metrics,
        actual_only_facets: actual_only,
        expected_only_facets: expected_only,
        failures,
    }
}

fn normalized_facets(mesh: &StlMesh, tolerance: f64) -> HashSet<[[i64; 3]; 3]> {
    mesh.facets
        .iter()
        .map(|facet| {
            let mut vertices = facet.vertices.map(|vertex| quantize(vertex.0, tolerance));
            vertices.sort_unstable();
            vertices
        })
        .collect()
}

fn quantize(point: [f64; 3], tolerance: f64) -> [i64; 3] {
    point.map(|coordinate| (coordinate / tolerance).round() as i64)
}

fn check_absolute(failures: &mut Vec<String>, name: &str, actual: f64, expected: f64, limit: f64) {
    let delta = (actual - expected).abs();
    if delta > limit {
        failures.push(format!(
            "{name} differs by {delta:.9}: {actual:.9} vs {expected:.9} (allowed {limit:.9})"
        ));
    }
}

fn check_relative(failures: &mut Vec<String>, name: &str, actual: f64, expected: f64, limit: f64) {
    let scale = actual.abs().max(expected.abs()).max(f64::EPSILON);
    let relative = (actual - expected).abs() / scale;
    if relative > limit {
        failures.push(format!(
            "{name} relative error is {relative:.9}: {actual:.9} vs {expected:.9} (allowed {limit:.9})"
        ));
    }
}

fn sub(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

fn cross(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

fn dot(left: [f64; 3], right: [f64; 3]) -> f64 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

fn length(vector: [f64; 3]) -> f64 {
    dot(vector, vector).sqrt()
}

fn find(parent: &mut [usize], value: usize) -> usize {
    if parent[value] != value {
        parent[value] = find(parent, parent[value]);
    }
    parent[value]
}

fn union(parent: &mut [usize], left: usize, right: usize) {
    let left_root = find(parent, left);
    let right_root = find(parent, right);
    if left_root != right_root {
        parent[right_root] = left_root;
    }
}

/// Only the upstream-corpus gates in `corpus_parity.rs` walk a directory; this
/// module is compiled into every test binary, so the other one sees it unused.
#[allow(dead_code)]
pub fn discover_scad_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    discover_scad_files_into(root, &mut files)?;
    files.sort();
    Ok(files)
}

#[allow(dead_code)]
fn discover_scad_files_into(root: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        fs::read_dir(root).map_err(|error| format!("read {}: {error}", root.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read entry in {}: {error}", root.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("inspect {}: {error}", path.display()))?;
        if file_type.is_dir() {
            discover_scad_files_into(&path, files)?;
        } else if file_type.is_file()
            && path.extension().and_then(|value| value.to_str()) == Some("scad")
        {
            files.push(path);
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParityCase {
    pub id: String,
    pub source: String,
    pub definitions: String,
    pub reference: String,
}

pub fn parse_cases(input: &str) -> Result<Vec<ParityCase>, String> {
    let mut cases = Vec::new();
    let mut ids = HashSet::new();
    for (index, line) in input.lines().enumerate() {
        let line_number = index + 1;
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        // The committed manifest uses visible `\\t` delimiters so it remains
        // readable in diffs; also accept conventional literal tabs.
        let fields = if line.contains('\t') {
            line.split('\t').collect::<Vec<_>>()
        } else {
            line.split("\\t").collect::<Vec<_>>()
        };
        if fields.len() != 4 {
            return Err(format!(
                "cases.tsv line {line_number}: expected 4 tab-separated fields, found {}",
                fields.len()
            ));
        }
        if fields[0].is_empty() || fields[1].is_empty() || fields[3].is_empty() {
            return Err(format!(
                "cases.tsv line {line_number}: id, source, and reference are required"
            ));
        }
        if !ids.insert(fields[0].to_owned()) {
            return Err(format!(
                "cases.tsv line {line_number}: duplicate case id {}",
                fields[0]
            ));
        }
        cases.push(ParityCase {
            id: fields[0].to_owned(),
            source: fields[1].to_owned(),
            definitions: fields[2].to_owned(),
            reference: fields[3].to_owned(),
        });
    }
    Ok(cases)
}

/// Directory holding the copied OpenAPPA sources and their OpenSCAD goldens.
pub fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/openappa")
}

/// The eight golden cases, read from the committed manifest.
pub fn openappa_cases() -> Vec<ParityCase> {
    let manifest = fs::read_to_string(fixture_root().join("cases.tsv")).unwrap();
    parse_cases(&manifest).unwrap()
}

pub fn source_with_definitions(source: &str, definitions: &str) -> String {
    if definitions.trim().is_empty() {
        source.to_owned()
    } else {
        // OpenSCAD and the native evaluator use the final assignment in a
        // lexical scope, even when that assignment appears after geometry.
        format!("{source}\n\n// Test-only command-line definition overrides.\n{definitions};\n")
    }
}
