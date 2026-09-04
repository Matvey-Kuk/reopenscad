//! Native, dependency-free SCAD evaluator and implicit-surface mesher.
//!
//! This is intentionally implemented in-process: it does not invoke OpenSCAD,
//! load OpenSCAD WASM, or shell out to any CAD engine.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

mod builtins;

#[derive(Debug, Clone)]
pub struct EngineError {
    message: String,
}

impl EngineError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for EngineError {}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Vec3 {
    fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    fn add(self, other: Self) -> Self {
        Self::new(self.x + other.x, self.y + other.y, self.z + other.z)
    }

    fn sub(self, other: Self) -> Self {
        Self::new(self.x - other.x, self.y - other.y, self.z - other.z)
    }

    fn mul(self, scalar: f64) -> Self {
        Self::new(self.x * scalar, self.y * scalar, self.z * scalar)
    }

    fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    fn cross(self, other: Self) -> Self {
        Self::new(
            self.y * other.z - self.z * other.y,
            self.z * other.x - self.x * other.z,
            self.x * other.y - self.y * other.x,
        )
    }

    fn length(self) -> f64 {
        self.dot(self).sqrt()
    }

    fn normalized(self) -> Self {
        let length = self.length();
        if length <= 1e-12 {
            Self::new(0.0, 0.0, 1.0)
        } else {
            self.mul(1.0 / length)
        }
    }

    fn component(self, axis: usize) -> f64 {
        match axis {
            0 => self.x,
            1 => self.y,
            _ => self.z,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Triangle {
    pub vertices: [Vec3; 3],
    pub normal: Vec3,
}

#[derive(Clone, Debug, Default)]
pub struct Mesh {
    pub triangles: Vec<Triangle>,
}

impl Mesh {
    pub fn binary_stl(&self) -> Vec<u8> {
        let mut output = vec![0u8; 84 + self.triangles.len() * 50];
        let header = b"ReOpenSCAD native Rust geometry engine";
        output[..header.len()].copy_from_slice(header);
        output[80..84].copy_from_slice(&(self.triangles.len() as u32).to_le_bytes());
        for (index, triangle) in self.triangles.iter().enumerate() {
            let offset = 84 + index * 50;
            let values = [
                triangle.normal.x,
                triangle.normal.y,
                triangle.normal.z,
                triangle.vertices[0].x,
                triangle.vertices[0].y,
                triangle.vertices[0].z,
                triangle.vertices[1].x,
                triangle.vertices[1].y,
                triangle.vertices[1].z,
                triangle.vertices[2].x,
                triangle.vertices[2].y,
                triangle.vertices[2].z,
            ];
            for (value_index, value) in values.into_iter().enumerate() {
                let start = offset + value_index * 4;
                output[start..start + 4].copy_from_slice(&(value as f32).to_le_bytes());
            }
        }
        output
    }

    pub fn obj(&self) -> Vec<u8> {
        let mut output = String::from("# ReOpenSCAD native mesh\n");
        for triangle in &self.triangles {
            for vertex in triangle.vertices {
                output.push_str(&format!("v {} {} {}\n", vertex.x, vertex.y, vertex.z));
            }
        }
        for triangle in &self.triangles {
            output.push_str(&format!(
                "vn {} {} {}\n",
                triangle.normal.x, triangle.normal.y, triangle.normal.z
            ));
        }
        for index in 0..self.triangles.len() {
            let vertex = index * 3 + 1;
            let normal = index + 1;
            output.push_str(&format!(
                "f {vertex}//{normal} {}//{normal} {}//{normal}\n",
                vertex + 1,
                vertex + 2
            ));
        }
        output.into_bytes()
    }

    pub fn off(&self) -> Vec<u8> {
        let mut output = format!(
            "OFF\n{} {} 0\n",
            self.triangles.len() * 3,
            self.triangles.len()
        );
        for triangle in &self.triangles {
            for vertex in triangle.vertices {
                output.push_str(&format!("{} {} {}\n", vertex.x, vertex.y, vertex.z));
            }
        }
        for index in 0..self.triangles.len() {
            let vertex = index * 3;
            output.push_str(&format!("3 {vertex} {} {}\n", vertex + 1, vertex + 2));
        }
        output.into_bytes()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    Preview,
    Render,
}

impl Quality {
    fn resolution(self) -> usize {
        match self {
            Self::Preview => 48,
            Self::Render => 72,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Render => "render",
        }
    }
}

#[derive(Debug)]
pub struct CompileOutput {
    pub mesh: Mesh,
    pub messages: Vec<String>,
}

/// Byte offsets into the submitted SCAD source. Multipart callers can retain
/// this optional field in their wire format while parser-level spans are added
/// incrementally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceRange {
    pub start: usize,
    pub end: usize,
}

/// Axis-aligned world-space bounds for a compiled object.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PartBounds {
    pub min: Vec3,
    pub max: Vec3,
}

/// One independently addressable top-level object.
#[derive(Clone, Debug)]
pub struct CompiledPart {
    pub id: String,
    pub name: String,
    pub mesh: Mesh,
    pub bounds: PartBounds,
    pub source_range: Option<SourceRange>,
    pub visible: bool,
    pub selectable: bool,
    /// `color()` applied to this whole object, as `[r, g, b, a]` in 0..1.
    ///
    /// `None` means the object states no color and the viewer should use its
    /// default material. Per §15 of the language spec color is preview data:
    /// it never alters the geometry, and STL cannot carry it, but 3MF/AMF can.
    /// Only a color wrapping the entire object is reported here; colors on
    /// individual sub-shapes inside one top-level object are not split out.
    pub color: Option<[f64; 4]>,
    /// True for a `%`-modified subtree: previewable, but excluded from the
    /// CSG result and from every export.
    pub background: bool,
    quality: Quality,
    shape: Shape,
}

/// Multipart compilation retains independently rendered objects while also
/// providing the historical implicit-union mesh for whole-model export.
#[derive(Clone, Debug)]
pub struct MultipartCompileOutput {
    pub mesh: Mesh,
    pub parts: Vec<CompiledPart>,
    /// `%`-modified objects. Deliberately kept out of `parts` so they can never
    /// reach `mesh`, an export, or the clearance check; a preview may draw
    /// them transparently alongside `parts`.
    pub background_parts: Vec<CompiledPart>,
    pub messages: Vec<String>,
}

/// Read-only access to a workspace/project virtual file system.
///
/// Paths passed to [`SourceLoader::load`] are normalized, relative virtual
/// paths without `.` or `..` components. Implementations must not interpret
/// them as host filesystem paths.
pub trait SourceLoader {
    fn load(&self, virtual_path: &str) -> Result<Option<String>, String>;

    fn library_paths(&self) -> Vec<String> {
        Vec::new()
    }
}

/// The complete set of sources `include <...>` / `use <...>` can reach.
///
/// SECURITY POLICY: this service has no user filesystem, so a directive is
/// resolved by *exact string equality* against this compile-time table and
/// nothing else. There is no path normalization, no directory walk, no
/// host-filesystem access and no network fetch, so `../`, absolute paths,
/// symlinks and URL-encoded traversal have no code path to exploit — an
/// unknown name simply fails. Adding a library means adding a literal entry
/// here, which is a code review, not a runtime decision.
const BUNDLED_LIBRARY_SOURCES: &[(&str, &str)] = &[
    ("units.scad", UNITS_SCAD),
    ("MCAD/units.scad", UNITS_SCAD),
    ("shapes.scad", SHAPES_SCAD),
    ("MCAD/shapes.scad", SHAPES_SCAD),
];

const UNITS_SCAD: &str = r#"
// Bundled unit constants and conversions. Lengths are in millimetres, which
// is the unit STL and 3MF consumers assume.
mm = 1;
cm = 10;
dm = 100;
m = 1000;
um = 0.001;
micron = 0.001;
inch = 25.4;
mil = 0.0254;
thou = 0.0254;
foot = 304.8;
feet = 304.8;
yard = 914.4;

function mm_to_inch(value) = value / 25.4;
function inch_to_mm(value) = value * 25.4;
function deg_to_rad(value) = value * PI / 180;
function rad_to_deg(value) = value * 180 / PI;
"#;

const SHAPES_SCAD: &str = r#"
include <units.scad>

// A hollow cylinder. `inner` is the bore radius. The bore is centred on the
// wall it cuts and made three times as tall, so it overshoots both end caps
// and the difference leaves no zero-thickness face at either end.
module tube(height = 1, outer = 1, inner = 0.5, center = false) {
    difference() {
        cylinder(h = height, r = outer, center = center);
        translate([0, 0, center ? 0 : height / 2])
            cylinder(h = height * 3, r = inner, center = true);
    }
}

// A flat annulus: `tube` with a thickness rather than a height.
module washer(thickness = 1, outer = 1, inner = 0.5) {
    tube(height = thickness, outer = outer, inner = inner);
}

// 2D regular polygon inscribed in a circle of `radius`, first vertex on +X.
module regular_polygon(sides = 6, radius = 1) {
    polygon([for (i = [0 : sides - 1])
        [radius * cos(i * 360 / sides), radius * sin(i * 360 / sides)]]);
}

// 2D hexagon measured across flats, the way hardware is specified.
module hexagon(across_flats = 1) {
    regular_polygon(sides = 6, radius = across_flats / sqrt(3));
}

// A hex prism, e.g. a nut blank or a captive-nut pocket.
module hex_prism(across_flats = 1, height = 1, center = false) {
    translate([0, 0, center ? -height / 2 : 0])
        linear_extrude(height = height) hexagon(across_flats = across_flats);
}

// A rectangular frame: `size` outer, walls `wall` thick.
module rectangle_frame(size = [10, 10], wall = 1) {
    difference() {
        square(size, center = true);
        square([size[0] - 2 * wall, size[1] - 2 * wall], center = true);
    }
}
"#;

/// Serves [`BUNDLED_LIBRARY_SOURCES`] and nothing else. See the policy note on
/// that table.
struct BundledLibrary;

static BUNDLED_LIBRARY: BundledLibrary = BundledLibrary;

impl SourceLoader for BundledLibrary {
    fn load(&self, virtual_path: &str) -> Result<Option<String>, String> {
        // Exact match only. `strip_prefix("./")` is the single spelling
        // tolerance granted, and it cannot introduce a new reachable name.
        let requested = virtual_path
            .trim()
            .strip_prefix("./")
            .unwrap_or_else(|| virtual_path.trim());
        Ok(BUNDLED_LIBRARY_SOURCES
            .iter()
            .find(|(name, _)| *name == requested)
            .map(|(_, source)| (*source).to_string()))
    }

    fn library_paths(&self) -> Vec<String> {
        let mut paths = BUNDLED_LIBRARY_SOURCES
            .iter()
            .map(|(name, _)| (*name).to_string())
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        paths
    }
}

/// How many files one program may pull in, and how deep the chain may go.
/// Both are DoS guards: the table is finite, but a cycle is not.
const MAX_INCLUDED_FILES: usize = 32;
const MAX_INCLUDE_DEPTH: usize = 8;

/// Splice `include`/`use` directives into `statements` before evaluation.
///
/// `include` behaves textually: the file's definitions *and* its top-level
/// assignments and geometry are spliced in place, so the including file can
/// override an included variable (§17). `use` contributes function and module
/// definitions only.
fn resolve_file_directives(
    statements: Vec<Stmt>,
    loader: &dyn SourceLoader,
    active: &mut Vec<String>,
    loaded: &mut usize,
) -> Result<Vec<Stmt>, EngineError> {
    if !statements
        .iter()
        .any(|statement| matches!(statement, Stmt::FileDirective { .. }))
    {
        return Ok(statements);
    }
    let mut resolved = Vec::with_capacity(statements.len());
    for statement in statements {
        let Stmt::FileDirective { path, line, mode } = statement else {
            resolved.push(statement);
            continue;
        };
        // A file already being expanded higher up the chain would loop
        // forever; OpenSCAD's own behavior here is to not re-enter. The key is
        // the same spelling `load` matches on, so `x` and `./x` cannot slip
        // past each other into a cycle.
        let key = path.trim().strip_prefix("./").unwrap_or(path.trim()).to_string();
        if active.contains(&key) {
            continue;
        }
        let Some(source) = loader
            .load(&path)
            .map_err(|error| EngineError::new(format!("Line {line}: {error}")))?
        else {
            let available = loader.library_paths().join(", ");
            return Err(EngineError::new(format!(
                "Line {line}: {path:?} is not a bundled library. \
                 This service has no user file system; available libraries are: {available}."
            )));
        };
        *loaded += 1;
        if *loaded > MAX_INCLUDED_FILES || active.len() >= MAX_INCLUDE_DEPTH {
            return Err(EngineError::new(format!(
                "Line {line}: include/use exceeds the {MAX_INCLUDED_FILES}-file, \
                 {MAX_INCLUDE_DEPTH}-deep limit."
            )));
        }
        let tokens = Lexer::new(&source).tokenize()?;
        let parsed = Parser::new(tokens).parse_program()?;
        active.push(key);
        let expanded = resolve_file_directives(parsed, loader, active, loaded)?;
        active.pop();
        match mode {
            FileDirectiveMode::Include => resolved.extend(expanded.into_iter().map(
                |statement| match statement {
                    // Marking the origin keeps an included default from
                    // tripping the "assigned more than once" warning when the
                    // including file overrides it.
                    Stmt::Assign {
                        name, expression, ..
                    } => Stmt::Assign {
                        name,
                        expression,
                        from_include: true,
                    },
                    statement => statement,
                },
            )),
            FileDirectiveMode::Use => resolved.extend(
                expanded
                    .into_iter()
                    .filter(|statement| {
                        matches!(statement, Stmt::Function { .. } | Stmt::Module { .. })
                    }),
            ),
        }
    }
    Ok(resolved)
}

/// Parse `source` and expand its `include`/`use` directives against the
/// bundled library.
fn parse_resolved_program(source: &str) -> Result<Vec<Stmt>, EngineError> {
    let tokens = Lexer::new(source).tokenize()?;
    let statements = Parser::new(tokens).parse_program()?;
    resolve_file_directives(statements, &BUNDLED_LIBRARY, &mut Vec::new(), &mut 0)
}

/// Tolerance-aware relationship between two visible objects.
///
/// A pair of IDs and a depth is not actionable on its own, so the report also
/// carries *where* the extremum sits: `location` is the sampled world-space
/// point of deepest penetration (or of closest approach when the parts are
/// merely near each other), and `overlap` is the intersection of the two
/// axis-aligned bounds, present only when those bounds actually overlap.
#[derive(Clone, Debug, PartialEq)]
pub struct PartIntersection {
    pub a: String,
    pub b: String,
    pub intersects: bool,
    pub within_tolerance: bool,
    pub clearance: f64,
    pub depth: Option<f64>,
    pub location: Option<Vec3>,
    pub overlap: Option<PartBounds>,
}

/// One meshed shape, and what it cost in fidelity to mesh it.
struct MeshedShape {
    mesh: Mesh,
    /// `None` when the exact kernel built the mesh. `Some(reason)` when it
    /// declined and the sampled mesher stood in.
    approximated: Option<String>,
}

/// Meshes one evaluated shape with the exact polyhedral kernel, falling back to
/// the sampled mesher only for what the kernel cannot build.
///
/// The exact kernel is the default because it is the only one of the two that
/// can reproduce OpenSCAD's geometry at all: the sampled mesher quantises every
/// edge onto a grid, so a flat face comes back faceted and a 12-triangle box
/// comes back with thousands. Where the kernel genuinely cannot proceed, a
/// sampled mesh is still better than no mesh — but the substitution is
/// *reported*, because an approximation the user cannot see is worse than a
/// slow exact answer.
fn mesh_shape_exact_first(
    shape: &Shape,
    resolution: usize,
    cancellation: Option<&AtomicBool>,
) -> Result<MeshedShape, EngineError> {
    let declined = match exact::shape_to_solid(shape, cancellation) {
        Ok(solid) => {
            let mesh = exact::solid_to_mesh(&solid);
            if !mesh.triangles.is_empty() {
                return Ok(MeshedShape {
                    mesh,
                    approximated: None,
                });
            }
            "it produced no geometry".to_string()
        }
        Err(error) => {
            // A cancelled render must abort, not quietly restart on the other
            // path and blow through the deadline twice.
            check_cancelled(cancellation)?;
            error.to_string()
        }
    };
    let mesh = mesh_shape(shape, resolution, cancellation)?;
    Ok(MeshedShape {
        mesh,
        approximated: Some(format!(
            "WARNING: the exact kernel declined this model ({declined}); it was \
             meshed by the sampled approximation at resolution {resolution}, so \
             its edges are quantised onto a grid and it will not match OpenSCAD."
        )),
    })
}

pub fn compile(
    source: &str,
    quality: Quality,
    cancellation: Option<&AtomicBool>,
) -> Result<CompileOutput, EngineError> {
    let statements = parse_resolved_program(source)?;
    let mut evaluator = Evaluator {
        cancellation,
        preview: matches!(quality, Quality::Preview),
        exact: true,
        ..Evaluator::default()
    };
    let shape = evaluator.evaluate(&statements)?;
    if shape.dimension() != ShapeDimension::Solid {
        return Err(EngineError::new("The model contains no 3D geometry."));
    }
    check_cancelled(cancellation)?;
    let resolution = quality.resolution();
    let meshed = mesh_shape_exact_first(&shape, resolution, cancellation)?;
    if meshed.mesh.triangles.is_empty() {
        return Err(EngineError::new("The model contains no 3D geometry."));
    }
    let mut messages = evaluator.diagnostics;
    messages.push(format!("Parsed {} top-level statements.", statements.len()));
    messages.extend(kernel_report(&meshed, resolution));
    Ok(CompileOutput {
        mesh: meshed.mesh,
        messages,
    })
}

/// Console lines describing which kernel produced a mesh, and why if it was not
/// the exact one.
fn kernel_report(meshed: &MeshedShape, resolution: usize) -> Vec<String> {
    match &meshed.approximated {
        None => vec![format!(
            "Exact polyhedral kernel generated {} triangles.",
            meshed.mesh.triangles.len()
        )],
        Some(warning) => vec![
            warning.clone(),
            format!(
                "Native implicit mesher generated {} triangles at quality {}.",
                meshed.mesh.triangles.len(),
                resolution
            ),
        ],
    }
}

/// The sampled path on its own: evaluate, then mesh by sampling the distance
/// field, never consulting the exact kernel.
///
/// Kept public deliberately. It is the fallback [`compile`] reaches for, the
/// baseline the parity suite measures the exact kernel against, and the escape
/// hatch if a model turns out to be one the kernel handles badly.
pub fn compile_sampled(
    source: &str,
    quality: Quality,
    cancellation: Option<&AtomicBool>,
) -> Result<CompileOutput, EngineError> {
    let statements = parse_resolved_program(source)?;
    let mut evaluator = Evaluator {
        cancellation,
        preview: matches!(quality, Quality::Preview),
        ..Evaluator::default()
    };
    let shape = evaluator.evaluate(&statements)?;
    if shape.dimension() != ShapeDimension::Solid {
        return Err(EngineError::new("The model contains no 3D geometry."));
    }
    check_cancelled(cancellation)?;
    let resolution = quality.resolution();
    let mesh = mesh_shape(&shape, resolution, cancellation)?;
    if mesh.triangles.is_empty() {
        return Err(EngineError::new("The model contains no 3D geometry."));
    }
    let mut messages = evaluator.diagnostics;
    messages.extend([
        format!("Parsed {} top-level statements.", statements.len()),
        format!(
            "Native implicit mesher generated {} triangles at quality {}.",
            mesh.triangles.len(),
            resolution
        ),
    ]);
    Ok(CompileOutput { mesh, messages })
}

/// Parse and evaluate SCAD without requiring renderable 3D output.
///
/// This is appropriate for validating workspace edits containing 2D geometry
/// or definitions-only libraries. It still reports syntax, expression, module,
/// assertion, and evaluation-limit errors.
pub fn validate(source: &str) -> Result<Vec<String>, EngineError> {
    let statements = parse_resolved_program(source)?;
    // Nothing is meshed here, so the kernel choice cannot change the answer —
    // but it does decide which approximation warnings are emitted, and the
    // approximations validation should describe are the default path's.
    let mut evaluator = Evaluator {
        exact: true,
        ..Evaluator::default()
    };
    evaluator.evaluate_objects(&statements)?;
    let mut messages = evaluator.diagnostics;
    messages.push(format!("Parsed {} top-level statements.", statements.len()));
    Ok(messages)
}

/// Meshes the union of every top-level object *and* each object on its own,
/// converting each object's geometry through the exact kernel exactly once.
///
/// Meshing the combined shape and then meshing every part separately does the
/// same boolean work twice: the combined shape *is* the union of the parts, so
/// its conversion re-derives every part solid on the way. This builds each
/// top-level child's solid once — in parallel, since the children are
/// independent — and then unions those solids into the combined result.
///
/// The child list and its order mirror [`Shape::union`]'s one-level
/// flattening, and the per-part grouping mirrors what `shape_to_solid` does for
/// a `Shape::Union`, so `union_all` sees exactly the operands, in exactly the
/// order, that the two-pass version fed it. The solids — and therefore every
/// triangle — are bit-identical; only the work is halved.
///
/// Returns `Ok(None)` when the exact kernel declines any child or yields an
/// empty mesh, leaving the caller to fall back to the per-shape path that can
/// substitute the sampled mesher for individual objects.
fn exact_multipart_meshes(
    solid_shapes: &[Shape],
    cancellation: Option<&AtomicBool>,
) -> Result<Option<(Mesh, Vec<Mesh>)>, EngineError> {
    let mut groups: Vec<std::ops::Range<usize>> = Vec::with_capacity(solid_shapes.len());
    let mut children: Vec<&Shape> = Vec::new();
    for shape in solid_shapes {
        let start = children.len();
        match shape {
            Shape::Union {
                children: nested, ..
            } => children.extend(nested.iter().map(|child| &child.shape)),
            other => children.push(other),
        }
        groups.push(start..children.len());
    }

    let mut solids = Vec::with_capacity(children.len());
    for solid in exact::shapes_to_solids(&children, cancellation) {
        match solid {
            Ok(solid) => solids.push(solid),
            Err(_) => {
                // A cancelled render must abort, not quietly restart on the
                // sampled path and blow through the deadline twice.
                check_cancelled(cancellation)?;
                return Ok(None);
            }
        }
    }
    check_cancelled(cancellation)?;

    // One top-level object *is* the union, so the combined solid is its solid;
    // unioning the group a second time would repeat the whole boolean pass.
    let single = groups.len() == 1;
    let part_solids = if single {
        Vec::new()
    } else {
        groups
            .iter()
            .map(|range| exact::union_all_parallel(solids[range.clone()].to_vec()))
            .collect::<Vec<_>>()
    };
    // The combined fold is the whole model's union, which on a grid-built
    // model is hundreds of operands, so it takes the parallel reduction too.
    let combined_solid = exact::union_all_parallel(solids);
    let combined = exact::solid_to_mesh(&combined_solid);
    if combined.triangles.is_empty() {
        return Ok(None);
    }
    let parts = if single {
        vec![combined.clone()]
    } else {
        part_solids.iter().map(exact::solid_to_mesh).collect()
    };
    if parts.iter().any(|mesh| mesh.triangles.is_empty()) {
        return Ok(None);
    }
    Ok(Some((combined, parts)))
}

/// Memoised results of [`compile_parts`], keyed by exactly what determines
/// them: the source and the quality.
///
/// One user action compiles a workspace several times over. A preview render
/// is followed by the clearance check, which compiles the same source again;
/// an export compiles it a third time; an unchanged re-render a fourth. Each
/// of those paid the full evaluation and CSG cost for a result the previous
/// one had already produced, which on a model that takes seconds to compile is
/// the difference between an instant download and another full wait.
///
/// The cache is deliberately tiny — the last few distinct compiles — because
/// entries hold meshes and shape trees, and it is keyed by the whole source
/// text rather than a digest so a hash collision cannot serve the wrong
/// geometry.
static COMPILED_PARTS: Mutex<Vec<CompiledPartsEntry>> = Mutex::new(Vec::new());

/// How many distinct compiles are kept. Two covers the common pairing of a
/// preview and a render of the same source.
const COMPILED_PARTS_CACHE_ENTRIES: usize = 2;

/// Triangle budget above which a result is served but not retained, so a
/// pathologically large model cannot pin hundreds of megabytes.
const COMPILED_PARTS_CACHE_MAX_TRIANGLES: usize = 1_000_000;

struct CompiledPartsEntry {
    source: String,
    quality: Quality,
    output: Arc<MultipartCompileOutput>,
}

fn cached_compiled_parts(source: &str, quality: Quality) -> Option<Arc<MultipartCompileOutput>> {
    let mut cache = COMPILED_PARTS.lock().ok()?;
    let index = cache
        .iter()
        .position(|entry| entry.quality == quality && entry.source == source)?;
    // Most-recently-used first, so the eviction below drops the coldest entry.
    let entry = cache.remove(index);
    let output = Arc::clone(&entry.output);
    cache.insert(0, entry);
    Some(output)
}

fn remember_compiled_parts(source: &str, quality: Quality, output: &Arc<MultipartCompileOutput>) {
    let triangles = output.mesh.triangles.len()
        + output
            .parts
            .iter()
            .chain(output.background_parts.iter())
            .map(|part| part.mesh.triangles.len())
            .sum::<usize>();
    if triangles > COMPILED_PARTS_CACHE_MAX_TRIANGLES {
        return;
    }
    let Ok(mut cache) = COMPILED_PARTS.lock() else {
        return;
    };
    cache.retain(|entry| entry.quality != quality || entry.source != source);
    cache.insert(
        0,
        CompiledPartsEntry {
            source: source.to_string(),
            quality,
            output: Arc::clone(output),
        },
    );
    cache.truncate(COMPILED_PARTS_CACHE_ENTRIES);
}

/// Compiles top-level objects, reusing an identical earlier compile.
///
/// Prefer this over [`compile_parts`] wherever the result is only read: it
/// hands back a shared result instead of a private copy, so a repeat compile
/// costs a reference-count bump rather than a full evaluation and CSG pass.
pub fn compile_parts_shared(
    source: &str,
    quality: Quality,
    cancellation: Option<&AtomicBool>,
) -> Result<Arc<MultipartCompileOutput>, EngineError> {
    if let Some(output) = cached_compiled_parts(source, quality) {
        return Ok(output);
    }
    let output = Arc::new(compile_parts(source, quality, cancellation)?);
    remember_compiled_parts(source, quality, &output);
    Ok(output)
}

/// Memoised results of [`compile`], on the same terms as [`COMPILED_PARTS`].
///
/// The whole-model export path compiles the implicit union directly rather
/// than through the multipart route, so it needs its own entry: two exports of
/// an unchanged workspace would otherwise each pay the full CSG cost.
static COMPILED_WHOLE: Mutex<Vec<CompiledWholeEntry>> = Mutex::new(Vec::new());

struct CompiledWholeEntry {
    source: String,
    quality: Quality,
    output: Arc<CompileOutput>,
}

/// Compiles the whole model, reusing an identical earlier compile.
pub fn compile_shared(
    source: &str,
    quality: Quality,
    cancellation: Option<&AtomicBool>,
) -> Result<Arc<CompileOutput>, EngineError> {
    if let Ok(mut cache) = COMPILED_WHOLE.lock() {
        if let Some(index) = cache
            .iter()
            .position(|entry| entry.quality == quality && entry.source == source)
        {
            let entry = cache.remove(index);
            let output = Arc::clone(&entry.output);
            cache.insert(0, entry);
            return Ok(output);
        }
    }
    let output = Arc::new(compile(source, quality, cancellation)?);
    if output.mesh.triangles.len() <= COMPILED_PARTS_CACHE_MAX_TRIANGLES {
        if let Ok(mut cache) = COMPILED_WHOLE.lock() {
            cache.retain(|entry| entry.quality != quality || entry.source != source);
            cache.insert(
                0,
                CompiledWholeEntry {
                    source: source.to_string(),
                    quality,
                    output: Arc::clone(&output),
                },
            );
            cache.truncate(COMPILED_PARTS_CACHE_ENTRIES);
        }
    }
    Ok(output)
}

/// Compile top-level statements as independently addressable objects.
///
/// IDs and output ordering are deterministic for a given source. `mesh` keeps
/// the exact behavior of [`compile`]: it is meshed from the implicit union, not
/// formed by concatenating potentially overlapping part meshes.
pub fn compile_parts(
    source: &str,
    quality: Quality,
    cancellation: Option<&AtomicBool>,
) -> Result<MultipartCompileOutput, EngineError> {
    let statements = parse_resolved_program(source)?;
    let mut evaluator = Evaluator {
        cancellation,
        preview: matches!(quality, Quality::Preview),
        exact: true,
        ..Evaluator::default()
    };
    let shapes = evaluator.evaluate_objects(&statements)?;
    let solid_shapes = shapes
        .into_iter()
        .filter(|shape| shape.dimension() == ShapeDimension::Solid)
        .collect::<Vec<_>>();
    if solid_shapes.is_empty() {
        return Err(EngineError::new("The model contains no 3D geometry."));
    }
    check_cancelled(cancellation)?;
    let resolution = quality.resolution();

    // The exact path builds every object's solid once and reuses those solids
    // for the union; only when the kernel declines does the slower per-shape
    // path run, because only it can substitute the sampled mesher per object.
    let exact_meshes = exact_multipart_meshes(&solid_shapes, cancellation)?;
    let (mesh, part_meshes, mut approximated) = match exact_meshes {
        Some((combined, parts)) => (combined, Some(parts), None),
        None => {
            let combined_shape = Shape::union(solid_shapes.clone())
                .ok_or_else(|| EngineError::new("The model contains no 3D geometry."))?;
            let combined = mesh_shape_exact_first(&combined_shape, resolution, cancellation)?;
            if combined.mesh.triangles.is_empty() {
                return Err(EngineError::new("The model contains no 3D geometry."));
            }
            (combined.mesh, None, combined.approximated)
        }
    };

    let mut names = top_level_object_names(&statements);
    if names.len() != solid_shapes.len() {
        names = (1..=solid_shapes.len())
            .map(|index| format!("object {index}"))
            .collect();
    }
    // A single top-level object *is* the union, so meshing it again would
    // repeat the whole boolean evaluation for a bit-identical answer. That
    // matters: with the exact kernel it is the difference between one and two
    // full CSG passes on every render of a one-object model, which is most of
    // them.
    let single = solid_shapes.len() == 1;
    let mut part_meshes = part_meshes.map(Vec::into_iter);
    let mut parts = Vec::with_capacity(solid_shapes.len());
    for (index, (shape, name)) in solid_shapes.into_iter().zip(names).enumerate() {
        check_cancelled(cancellation)?;
        let bounds = shape.bounds();
        let part_mesh = match part_meshes.as_mut().and_then(Iterator::next) {
            Some(part_mesh) => part_mesh,
            None if single => mesh.clone(),
            None => {
                let meshed = mesh_shape_exact_first(&shape, resolution, cancellation)?;
                if meshed.approximated.is_some() && approximated.is_none() {
                    approximated = meshed.approximated.clone();
                }
                meshed.mesh
            }
        };
        if part_mesh.triangles.is_empty() {
            continue;
        }
        parts.push(CompiledPart {
            id: format!("object-{}", index + 1),
            name,
            mesh: part_mesh,
            bounds: PartBounds {
                min: bounds.min,
                max: bounds.max,
            },
            source_range: None,
            visible: true,
            selectable: true,
            color: shape.stated_color(),
            background: false,
            quality,
            shape,
        });
    }

    // `%` subtrees are meshed so a preview can draw them, but they stay out of
    // `parts` — and therefore out of exports, selection and clearance checks.
    let mut background_parts = Vec::new();
    for (index, shape) in evaluator
        .background_shapes
        .drain(..)
        .filter(|shape| shape.dimension() == ShapeDimension::Solid)
        .enumerate()
    {
        check_cancelled(cancellation)?;
        let bounds = shape.bounds();
        let meshed = mesh_shape_exact_first(&shape, resolution, cancellation)?;
        if meshed.approximated.is_some() && approximated.is_none() {
            approximated = meshed.approximated.clone();
        }
        let part_mesh = meshed.mesh;
        if part_mesh.triangles.is_empty() {
            continue;
        }
        background_parts.push(CompiledPart {
            id: format!("background-{}", index + 1),
            name: format!("background {}", index + 1),
            mesh: part_mesh,
            bounds: PartBounds {
                min: bounds.min,
                max: bounds.max,
            },
            source_range: None,
            visible: true,
            selectable: false,
            color: shape.stated_color(),
            background: true,
            quality,
            shape,
        });
    }

    let mut messages = evaluator.diagnostics;
    messages.push(format!("Parsed {} top-level statements.", statements.len()));
    match approximated {
        None => messages.push(format!(
            "Exact polyhedral kernel generated {} triangles across {} objects.",
            mesh.triangles.len(),
            parts.len()
        )),
        Some(warning) => {
            messages.push(warning);
            messages.push(format!(
                "Native implicit mesher generated {} triangles across {} objects at quality {}.",
                mesh.triangles.len(),
                parts.len(),
                resolution
            ));
        }
    }
    Ok(MultipartCompileOutput {
        mesh,
        parts,
        background_parts,
        messages,
    })
}

/// Mesh a non-empty selection of compiled top-level objects as one implicit
/// union.
///
/// This deliberately rebuilds the union from the retained shapes instead of
/// concatenating the per-part triangle meshes. Concatenation would leave
/// internal faces wherever selected objects overlap and would not represent
/// the same solid as SCAD `union()` semantics.
pub fn mesh_selected_parts(
    parts: &[CompiledPart],
    selected_object_ids: &[String],
    quality: Quality,
    cancellation: Option<&AtomicBool>,
) -> Result<Mesh, EngineError> {
    if selected_object_ids.is_empty() {
        return Err(EngineError::new(
            "Selected-part export requires at least one object ID.",
        ));
    }

    let available = parts
        .iter()
        .map(|part| (part.id.as_str(), part))
        .collect::<HashMap<_, _>>();
    let mut requested = HashSet::with_capacity(selected_object_ids.len());
    let mut selected_parts = Vec::with_capacity(selected_object_ids.len());
    let mut unknown = Vec::new();
    for object_id in selected_object_ids {
        check_cancelled(cancellation)?;
        if !requested.insert(object_id.as_str()) {
            return Err(EngineError::new(format!(
                "Duplicate selected object ID: {object_id}."
            )));
        }
        match available.get(object_id.as_str()) {
            Some(part) => selected_parts.push(*part),
            None => unknown.push(object_id.as_str()),
        }
    }
    if !unknown.is_empty() {
        return Err(EngineError::new(format!(
            "Unknown selected object IDs: {}.",
            unknown.join(", ")
        )));
    }

    if let Some(part) = selected_parts.iter().find(|part| part.quality != quality) {
        return Err(EngineError::new(format!(
            "Selected object {} was evaluated at {} quality and cannot be meshed at {} quality; compile parts at the requested quality.",
            part.id,
            part.quality.name(),
            quality.name()
        )));
    }

    let selected_shape = Shape::union(
        selected_parts
            .into_iter()
            .map(|part| part.shape.clone())
            .collect(),
    )
    .ok_or_else(|| {
        EngineError::new("Internal error: selected object shapes could not be unioned.")
    })?;
    check_cancelled(cancellation)?;
    let resolution = quality.resolution();
    let mesh = mesh_shape_exact_first(&selected_shape, resolution, cancellation)?.mesh;
    if mesh.triangles.is_empty() {
        return Err(EngineError::new(
            "The selected objects contain no 3D geometry.",
        ));
    }
    Ok(mesh)
}

/// Check every visible pair. Hidden IDs are intentionally request state rather
/// than mutations of compiled parts, so one compiled result can serve multiple
/// UI or MCP clients safely.
pub fn check_part_clearances(
    parts: &[CompiledPart],
    tolerance: f64,
    hidden_object_ids: &[String],
) -> Result<Vec<PartIntersection>, EngineError> {
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err(EngineError::new(
            "Intersection tolerance must be finite and non-negative.",
        ));
    }
    let hidden = hidden_object_ids
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let visible = parts
        .iter()
        .filter(|part| part.visible && !hidden.contains(part.id.as_str()))
        .collect::<Vec<_>>();
    // Every pair is an independent sweep of one part's surface against the
    // other's distance field, and that sweep is the whole cost of this check:
    // tens of thousands of sample points, each evaluated against a full shape
    // tree. Running the pairs concurrently is the difference between a check
    // that outlasts the render that produced it and one that follows it.
    let mut pairs = Vec::with_capacity(visible.len().saturating_sub(1) * visible.len() / 2);
    for (left_index, left) in visible.iter().enumerate() {
        for right in visible.iter().skip(left_index + 1) {
            pairs.push((*left, *right));
        }
    }
    let results = exact::map_indexed(pairs.len(), |index| {
        let (left, right) = pairs[index];
        let (signed_distance, location) = sampled_part_distance(left, right);
        let scale = part_maximum_span(left).max(part_maximum_span(right));
        let numerical_epsilon = (scale * 1e-7).max(1e-8);
        let intersects = signed_distance < -numerical_epsilon;
        let clearance = if intersects {
            0.0
        } else {
            signed_distance.max(0.0)
        };
        PartIntersection {
            a: left.id.clone(),
            b: right.id.clone(),
            intersects,
            within_tolerance: !intersects && clearance <= tolerance + numerical_epsilon,
            clearance,
            depth: intersects.then_some(-signed_distance),
            location,
            overlap: bounds_overlap(left.bounds, right.bounds),
        }
    });
    Ok(results)
}

/// The sampled signed distance between two parts, together with the point that
/// realised it.
///
/// The extremum point is what callers need in order to say *where* a clash or
/// a tolerance violation happens, so it is tracked alongside the minimum
/// instead of being recomputed by a second sweep.
fn sampled_part_distance(left: &CompiledPart, right: &CompiledPart) -> (f64, Option<Vec3>) {
    let bounds_distance = bounds_clearance(left.bounds, right.bounds);
    let mut sampled = f64::INFINITY;
    let mut witness = None;
    let mut consider = |distance: f64, point: Vec3| {
        if distance < sampled {
            sampled = distance;
            witness = Some(point);
        }
    };
    // How short a facet has to get before its interior stops hiding anything.
    // Relative to the pair's own size, so it costs nothing on meshes whose
    // facets are already small.
    let scale = part_maximum_span(left).max(part_maximum_span(right));
    let target_edge = (scale / 16.0).max(1e-6);
    let mut samples = Vec::new();
    surface_samples(&left.mesh, target_edge, &mut samples);
    for point in samples.drain(..) {
        consider(right.shape.distance(point), point);
    }
    surface_samples(&right.mesh, target_edge, &mut samples);
    for point in samples.drain(..) {
        consider(left.shape.distance(point), point);
    }
    if bounds_distance <= f64::EPSILON {
        let centers = bounds_overlap(left.bounds, right.bounds)
            .into_iter()
            .map(bounds_center)
            .chain([bounds_center(left.bounds), bounds_center(right.bounds)]);
        for point in centers {
            let shared_interior_distance =
                left.shape.distance(point).max(right.shape.distance(point));
            consider(shared_interior_distance, point);
        }
    }
    if sampled < 0.0 {
        (sampled, witness)
    } else {
        // The AABB distance is a strict lower bound and guards against tiny
        // negative/positive drift in approximate meshes of distant objects.
        (sampled.max(bounds_distance), witness)
    }
}

/// Ceiling on how many points one mesh may contribute to a clearance sweep.
///
/// Reached only by a mesh with a few very large facets; a finely tessellated
/// one never subdivides at all and stays at three points per facet.
const MAX_CLEARANCE_SAMPLES: usize = 40_000;

/// Points on a mesh's surface, dense enough that no facet interior can hide the
/// deepest penetration.
///
/// Sampling mesh *vertices* alone was adequate while every mesh came off a
/// sampling grid and carried thousands of them. It is not adequate for the
/// exact kernel: a box is eight vertices, every one of them on the rim of any
/// overlap, so a box buried halfway inside its neighbour reports a penetration
/// of zero. Splitting each facet until its edges are short relative to the
/// pair's size puts samples in the middle of large flat faces, which is exactly
/// where the deepest penetration is.
fn surface_samples(mesh: &Mesh, target_edge: f64, output: &mut Vec<Vec3>) {
    for triangle in &mesh.triangles {
        let [a, b, c] = triangle.vertices;
        output.extend([a, b, c]);
        subdivide_samples(a, b, c, target_edge, 5, output);
    }
}

fn subdivide_samples(a: Vec3, b: Vec3, c: Vec3, target_edge: f64, depth: usize, output: &mut Vec<Vec3>) {
    if output.len() >= MAX_CLEARANCE_SAMPLES {
        return;
    }
    output.push(a.add(b).add(c).mul(1.0 / 3.0));
    let longest = b
        .sub(a)
        .length()
        .max(c.sub(b).length())
        .max(a.sub(c).length());
    if depth == 0 || longest <= target_edge {
        return;
    }
    let ab = a.add(b).mul(0.5);
    let bc = b.add(c).mul(0.5);
    let ca = c.add(a).mul(0.5);
    output.extend([ab, bc, ca]);
    subdivide_samples(a, ab, ca, target_edge, depth - 1, output);
    subdivide_samples(ab, b, bc, target_edge, depth - 1, output);
    subdivide_samples(ca, bc, c, target_edge, depth - 1, output);
    subdivide_samples(ab, bc, ca, target_edge, depth - 1, output);
}

/// The shared region of two axis-aligned boxes, or `None` when they are apart
/// on any axis.
fn bounds_overlap(left: PartBounds, right: PartBounds) -> Option<PartBounds> {
    let min = Vec3::new(
        left.min.x.max(right.min.x),
        left.min.y.max(right.min.y),
        left.min.z.max(right.min.z),
    );
    let max = Vec3::new(
        left.max.x.min(right.max.x),
        left.max.y.min(right.max.y),
        left.max.z.min(right.max.z),
    );
    (min.x <= max.x && min.y <= max.y && min.z <= max.z).then_some(PartBounds { min, max })
}

fn bounds_center(bounds: PartBounds) -> Vec3 {
    bounds.min.add(bounds.max).mul(0.5)
}

fn bounds_clearance(left: PartBounds, right: PartBounds) -> f64 {
    let dx = (left.min.x - right.max.x)
        .max(right.min.x - left.max.x)
        .max(0.0);
    let dy = (left.min.y - right.max.y)
        .max(right.min.y - left.max.y)
        .max(0.0);
    let dz = (left.min.z - right.max.z)
        .max(right.min.z - left.max.z)
        .max(0.0);
    Vec3::new(dx, dy, dz).length()
}

fn part_maximum_span(part: &CompiledPart) -> f64 {
    let span = part.bounds.max.sub(part.bounds.min);
    span.x.max(span.y).max(span.z)
}

fn top_level_object_names(statements: &[Stmt]) -> Vec<String> {
    statements
        .iter()
        .filter_map(top_level_statement_name)
        .enumerate()
        .map(|(index, name)| format!("{name} {}", index + 1))
        .collect()
}

fn top_level_statement_name(statement: &Stmt) -> Option<String> {
    match statement {
        Stmt::Call { .. } => statement_object_name(statement),
        Stmt::Block(_) => Some("group".into()),
        Stmt::For { .. } => Some("for".into()),
        Stmt::Let { .. } => Some("let".into()),
        Stmt::If { .. } => Some("if".into()),
        // `!` keeps producing the marked subtree, so it is named after it.
        Stmt::Root(inner) => top_level_statement_name(inner),
        // `%` contributes no object to the CSG result; its geometry is named
        // separately as a background object.
        Stmt::Background(_)
        | Stmt::Disabled
        | Stmt::Assign { .. }
        | Stmt::FileDirective { .. }
        | Stmt::Function { .. }
        | Stmt::Module { .. } => None,
    }
}

fn statement_object_name(statement: &Stmt) -> Option<String> {
    let Stmt::Call { name, children, .. } = statement else {
        return None;
    };
    let transparent_wrapper = matches!(
        name.as_str(),
        "translate" | "rotate" | "scale" | "mirror" | "color" | "render" | "resize" | "multmatrix"
    );
    if transparent_wrapper && children.len() == 1 {
        statement_object_name(&children[0]).or_else(|| Some(name.clone()))
    } else {
        Some(name.clone())
    }
}

fn check_cancelled(cancellation: Option<&AtomicBool>) -> Result<(), EngineError> {
    if cancellation.is_some_and(|signal| signal.load(Ordering::Acquire)) {
        Err(EngineError::new("Render cancelled."))
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Ident(String),
    Number(f64),
    String(String),
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Semicolon,
    Equal,
    EqualEqual,
    Bang,
    BangEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    AndAnd,
    OrOr,
    Colon,
    Dot,
    Question,
    Caret,
    Hash,
    FilePath { path: String, line: usize },
}

struct Lexer<'a> {
    source: &'a [u8],
    cursor: usize,
    line: usize,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source: source.as_bytes(),
            cursor: 0,
            line: 1,
        }
    }

    fn tokenize(mut self) -> Result<Vec<Token>, EngineError> {
        let mut tokens = Vec::new();
        while let Some(byte) = self.peek() {
            match byte {
                b' ' | b'\t' | b'\r' => self.cursor += 1,
                b'\n' => {
                    self.line += 1;
                    self.cursor += 1;
                }
                b'/' if self.peek_next() == Some(b'/') => self.skip_line_comment(),
                b'/' if self.peek_next() == Some(b'*') => self.skip_block_comment()?,
                b'0'..=b'9' | b'.'
                    if byte != b'.'
                        || self.peek_next().is_some_and(|next| next.is_ascii_digit()) =>
                {
                    tokens.push(self.number()?);
                }
                b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$' => tokens.push(self.identifier()),
                b'"' => tokens.push(self.string()?),
                b'(' => self.single(&mut tokens, Token::LParen),
                b')' => self.single(&mut tokens, Token::RParen),
                b'{' => self.single(&mut tokens, Token::LBrace),
                b'}' => self.single(&mut tokens, Token::RBrace),
                b'[' => self.single(&mut tokens, Token::LBracket),
                b']' => self.single(&mut tokens, Token::RBracket),
                b',' => self.single(&mut tokens, Token::Comma),
                b';' => self.single(&mut tokens, Token::Semicolon),
                b'=' if self.peek_next() == Some(b'=') => {
                    self.double(&mut tokens, Token::EqualEqual)
                }
                b'=' => self.single(&mut tokens, Token::Equal),
                b'!' if self.peek_next() == Some(b'=') => {
                    self.double(&mut tokens, Token::BangEqual)
                }
                b'!' => self.single(&mut tokens, Token::Bang),
                b'<' if matches!(tokens.last(), Some(Token::Ident(name)) if matches!(name.as_str(), "include" | "use")) => {
                    tokens.push(self.file_path()?);
                }
                b'<' if self.peek_next() == Some(b'=') => {
                    self.double(&mut tokens, Token::LessEqual)
                }
                b'<' => self.single(&mut tokens, Token::Less),
                b'>' if self.peek_next() == Some(b'=') => {
                    self.double(&mut tokens, Token::GreaterEqual)
                }
                b'>' => self.single(&mut tokens, Token::Greater),
                b'+' => self.single(&mut tokens, Token::Plus),
                b'-' => self.single(&mut tokens, Token::Minus),
                b'*' => self.single(&mut tokens, Token::Star),
                b'/' => self.single(&mut tokens, Token::Slash),
                b'%' => self.single(&mut tokens, Token::Percent),
                b'&' if self.peek_next() == Some(b'&') => self.double(&mut tokens, Token::AndAnd),
                b'|' if self.peek_next() == Some(b'|') => self.double(&mut tokens, Token::OrOr),
                b':' => self.single(&mut tokens, Token::Colon),
                b'.' => self.single(&mut tokens, Token::Dot),
                b'?' => self.single(&mut tokens, Token::Question),
                b'^' => self.single(&mut tokens, Token::Caret),
                b'#' => self.single(&mut tokens, Token::Hash),
                other => {
                    return Err(EngineError::new(format!(
                        "Line {}: unsupported character {:?}.",
                        self.line, other as char
                    )))
                }
            }
        }
        Ok(tokens)
    }

    fn peek(&self) -> Option<u8> {
        self.source.get(self.cursor).copied()
    }

    fn peek_next(&self) -> Option<u8> {
        self.source.get(self.cursor + 1).copied()
    }

    fn single(&mut self, tokens: &mut Vec<Token>, token: Token) {
        tokens.push(token);
        self.cursor += 1;
    }

    fn double(&mut self, tokens: &mut Vec<Token>, token: Token) {
        tokens.push(token);
        self.cursor += 2;
    }

    fn skip_line_comment(&mut self) {
        while self.peek().is_some_and(|byte| byte != b'\n') {
            self.cursor += 1;
        }
    }

    fn skip_block_comment(&mut self) -> Result<(), EngineError> {
        self.cursor += 2;
        while self.cursor + 1 < self.source.len() {
            if self.peek() == Some(b'*') && self.peek_next() == Some(b'/') {
                self.cursor += 2;
                return Ok(());
            }
            if self.peek() == Some(b'\n') {
                self.line += 1;
            }
            self.cursor += 1;
        }
        Err(EngineError::new(format!(
            "Line {}: unterminated block comment.",
            self.line
        )))
    }

    fn number(&mut self) -> Result<Token, EngineError> {
        let start = self.cursor;
        let mut seen_dot = false;
        while let Some(byte) = self.peek() {
            if byte == b'.' && !seen_dot {
                seen_dot = true;
                self.cursor += 1;
            } else if byte.is_ascii_digit() {
                self.cursor += 1;
            } else {
                break;
            }
        }
        if self.peek().is_some_and(|byte| matches!(byte, b'e' | b'E')) {
            self.cursor += 1;
            if self.peek().is_some_and(|byte| matches!(byte, b'+' | b'-')) {
                self.cursor += 1;
            }
            let exponent_start = self.cursor;
            while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                self.cursor += 1;
            }
            if self.cursor == exponent_start {
                let text = String::from_utf8_lossy(&self.source[start..self.cursor]);
                return Err(EngineError::new(format!(
                    "Line {}: invalid number {text}.",
                    self.line
                )));
            }
        }
        let text = std::str::from_utf8(&self.source[start..self.cursor]).unwrap_or_default();
        text.parse::<f64>()
            .map(Token::Number)
            .map_err(|_| EngineError::new(format!("Line {}: invalid number {text}.", self.line)))
    }

    fn identifier(&mut self) -> Token {
        let start = self.cursor;
        while self
            .peek()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$'))
        {
            self.cursor += 1;
        }
        Token::Ident(String::from_utf8_lossy(&self.source[start..self.cursor]).into_owned())
    }

    fn file_path(&mut self) -> Result<Token, EngineError> {
        let line = self.line;
        self.cursor += 1;
        let start = self.cursor;
        while let Some(byte) = self.peek() {
            if byte == b'>' {
                let path = std::str::from_utf8(&self.source[start..self.cursor])
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                self.cursor += 1;
                if path.is_empty() {
                    return Err(EngineError::new(format!(
                        "Line {line}: include/use path cannot be empty."
                    )));
                }
                return Ok(Token::FilePath { path, line });
            }
            if byte == b'\n' {
                return Err(EngineError::new(format!(
                    "Line {line}: unterminated include/use path."
                )));
            }
            self.cursor += 1;
        }
        Err(EngineError::new(format!(
            "Line {line}: unterminated include/use path."
        )))
    }

    fn string(&mut self) -> Result<Token, EngineError> {
        self.cursor += 1;
        let mut output = String::new();
        while let Some(byte) = self.peek() {
            match byte {
                b'"' => {
                    self.cursor += 1;
                    return Ok(Token::String(output));
                }
                b'\\' => {
                    self.cursor += 1;
                    let Some(_) = self.peek() else { break };
                    let remaining = std::str::from_utf8(&self.source[self.cursor..])
                        .expect("lexer source is valid UTF-8");
                    let escaped = remaining.chars().next().expect("source is non-empty");
                    self.cursor += escaped.len_utf8();
                    match escaped {
                        '"' => output.push('"'),
                        '\\' => output.push('\\'),
                        't' => output.push('\t'),
                        'n' => output.push('\n'),
                        'r' => output.push('\r'),
                        'x' => output.push(self.unicode_escape(2)?),
                        'u' => output.push(self.unicode_escape(4)?),
                        'U' => output.push(self.unicode_escape(6)?),
                        other => output.push(other),
                    }
                }
                b'\n' => {
                    output.push('\n');
                    self.line += 1;
                    self.cursor += 1;
                }
                _ => {
                    let remaining = std::str::from_utf8(&self.source[self.cursor..])
                        .expect("lexer source is valid UTF-8");
                    let character = remaining.chars().next().expect("source is non-empty");
                    output.push(character);
                    self.cursor += character.len_utf8();
                }
            }
        }
        Err(EngineError::new(format!(
            "Line {}: unterminated string.",
            self.line
        )))
    }

    fn unicode_escape(&mut self, digits: usize) -> Result<char, EngineError> {
        let end = self.cursor.saturating_add(digits);
        let bytes = self.source.get(self.cursor..end).ok_or_else(|| {
            EngineError::new(format!("Line {}: incomplete string escape.", self.line))
        })?;
        let text = std::str::from_utf8(bytes).unwrap_or_default();
        if !bytes.iter().all(u8::is_ascii_hexdigit) {
            return Err(EngineError::new(format!(
                "Line {}: invalid hexadecimal string escape {text:?}.",
                self.line
            )));
        }
        self.cursor = end;
        let codepoint = u32::from_str_radix(text, 16).expect("validated hexadecimal digits");
        char::from_u32(codepoint).ok_or_else(|| {
            EngineError::new(format!(
                "Line {}: invalid Unicode code point {codepoint:#x}.",
                self.line
            ))
        })
    }
}

#[derive(Clone, Debug)]
enum Expr {
    Number(f64),
    Bool(bool),
    String(String),
    Vector(Vec<Expr>),
    Range(Box<Expr>, Box<Expr>, Box<Expr>),
    Variable(String),
    Unary(UnaryOp, Box<Expr>),
    Binary(Box<Expr>, BinaryOp, Box<Expr>),
    Conditional(Box<Expr>, Box<Expr>, Box<Expr>),
    Index(Box<Expr>, Box<Expr>),
    Member(Box<Expr>, String),
    Let(Vec<(String, Expr)>, Box<Expr>),
    Assert(Box<Expr>, Option<Box<Expr>>, Box<Expr>),
    Echo(Vec<Argument>, Box<Expr>),
    Call(Box<Expr>, Vec<Argument>),
    Lambda(Vec<(String, Option<Expr>)>, Box<Expr>),
    Comprehension(Vec<ListElement>),
}

#[derive(Clone, Debug)]
enum ListElement {
    Value(Expr),
    Each(Expr),
    For {
        bindings: Vec<(String, Expr)>,
        body: Box<ListElement>,
    },
    CStyleFor {
        initial: Vec<(String, Expr)>,
        condition: Expr,
        updates: Vec<(String, Expr)>,
        body: Box<ListElement>,
    },
    If {
        condition: Expr,
        then_element: Box<ListElement>,
        else_element: Option<Box<ListElement>>,
    },
    Let {
        bindings: Vec<(String, Expr)>,
        body: Box<ListElement>,
    },
}

#[derive(Clone, Copy, Debug)]
enum BinaryOp {
    Or,
    And,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Power,
}

#[derive(Clone, Copy, Debug)]
enum UnaryOp {
    Negate,
    Not,
}

#[derive(Clone, Debug)]
struct Argument {
    name: Option<String>,
    value: Expr,
}

#[derive(Clone, Debug)]
enum Stmt {
    Block(Vec<Stmt>),
    Disabled,
    /// `%subtree`: evaluated for its side effects and available to preview,
    /// but kept out of the CSG result and out of every export.
    Background(Box<Stmt>),
    /// `!subtree`: this subtree alone becomes the model. Everything else —
    /// including the transformations wrapping it — is discarded.
    Root(Box<Stmt>),
    Assign {
        name: String,
        expression: Expr,
        from_include: bool,
    },
    FileDirective {
        path: String,
        line: usize,
        mode: FileDirectiveMode,
    },
    Function {
        name: String,
        parameters: Vec<(String, Option<Expr>)>,
        body: Expr,
    },
    Module {
        name: String,
        parameters: Vec<(String, Option<Expr>)>,
        body: Vec<Stmt>,
    },
    For {
        bindings: Vec<(String, Expr)>,
        body: Vec<Stmt>,
        intersection: bool,
    },
    Let {
        bindings: Vec<(String, Expr)>,
        body: Vec<Stmt>,
    },
    If {
        condition: Expr,
        then_body: Vec<Stmt>,
        else_body: Vec<Stmt>,
    },
    Call {
        name: String,
        arguments: Vec<Argument>,
        children: Vec<Stmt>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileDirectiveMode {
    Include,
    Use,
}

struct Parser {
    tokens: Vec<Token>,
    cursor: usize,
    expression_depth: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            cursor: 0,
            expression_depth: 0,
        }
    }

    fn parse_program(&mut self) -> Result<Vec<Stmt>, EngineError> {
        let mut statements = Vec::new();
        while self.cursor < self.tokens.len() {
            if self.consume(&Token::Semicolon) {
                continue;
            }
            statements.push(self.statement()?);
        }
        Ok(statements)
    }

    fn statement(&mut self) -> Result<Stmt, EngineError> {
        let mut disabled = false;
        let mut background = false;
        let mut root = false;
        loop {
            if self.consume(&Token::Star) {
                disabled = true;
            } else if self.consume(&Token::Percent) {
                background = true;
            } else if self.consume(&Token::Bang) {
                root = true;
            } else if self.consume(&Token::Hash) {
                // `#` highlights the subtree in preview and leaves it in the
                // geometry, so it needs no representation in the tree.
            } else {
                break;
            }
        }

        let statement = if self.check(&Token::LBrace) {
            Stmt::Block(self.block()?)
        } else {
            self.unmodified_statement()?
        };
        // `*` wins over everything: the subtree is ignored outright, so there
        // is nothing left for `%` or `!` to mark.
        if disabled {
            return Ok(Stmt::Disabled);
        }
        // Per §21 `%` and `!` prefix a module instantiation or a control
        // statement. Applying them to a definition or an assignment is not
        // expressible in the grammar, and treating one as geometry would let a
        // stray `!` in front of an assignment blank the whole model.
        if !matches!(
            statement,
            Stmt::Call { .. }
                | Stmt::Block(_)
                | Stmt::For { .. }
                | Stmt::Let { .. }
                | Stmt::If { .. }
        ) {
            return Ok(statement);
        }
        let statement = if background {
            Stmt::Background(Box::new(statement))
        } else {
            statement
        };
        Ok(if root {
            Stmt::Root(Box::new(statement))
        } else {
            statement
        })
    }

    fn unmodified_statement(&mut self) -> Result<Stmt, EngineError> {
        let name = self.expect_identifier()?;
        if matches!(name.as_str(), "include" | "use") {
            let (path, line) = match self.tokens.get(self.cursor).cloned() {
                Some(Token::FilePath { path, line }) => {
                    self.cursor += 1;
                    (path, line)
                }
                token => {
                    return Err(EngineError::new(format!(
                        "Expected an angle-bracket path after {name}, found {token:?}."
                    )))
                }
            };
            self.consume(&Token::Semicolon);
            return Ok(Stmt::FileDirective {
                path,
                line,
                mode: if name == "include" {
                    FileDirectiveMode::Include
                } else {
                    FileDirectiveMode::Use
                },
            });
        }
        if name == "function" {
            return self.function_definition();
        }
        if name == "module" {
            return self.module_definition();
        }
        if name == "for" {
            return self.for_statement(false);
        }
        if name == "intersection_for" {
            return self.for_statement(true);
        }
        if name == "let" {
            return self.let_statement();
        }
        if name == "if" {
            return self.if_statement();
        }
        if self.consume(&Token::Equal) {
            let value = self.expression(0)?;
            self.expect(&Token::Semicolon)?;
            return Ok(Stmt::Assign {
                name,
                expression: value,
                from_include: false,
            });
        }
        self.expect(&Token::LParen)?;
        let arguments = self.arguments()?;
        self.expect(&Token::RParen)?;
        let children = self.children()?;
        Ok(Stmt::Call {
            name,
            arguments,
            children,
        })
    }

    fn module_definition(&mut self) -> Result<Stmt, EngineError> {
        let name = self.expect_identifier()?;
        self.expect(&Token::LParen)?;
        let parameters = self.parameters()?;
        self.expect(&Token::RParen)?;
        let body = self.children()?;
        Ok(Stmt::Module {
            name,
            parameters,
            body,
        })
    }

    fn function_definition(&mut self) -> Result<Stmt, EngineError> {
        let name = self.expect_identifier()?;
        self.expect(&Token::LParen)?;
        let parameters = self.parameters()?;
        self.expect(&Token::RParen)?;
        self.expect(&Token::Equal)?;
        let body = self.expression(0)?;
        self.expect(&Token::Semicolon)?;
        Ok(Stmt::Function {
            name,
            parameters,
            body,
        })
    }

    fn parameters(&mut self) -> Result<Vec<(String, Option<Expr>)>, EngineError> {
        let mut parameters = Vec::new();
        if self.check(&Token::RParen) {
            return Ok(parameters);
        }
        loop {
            let parameter = self.expect_identifier()?;
            let default = if self.consume(&Token::Equal) {
                Some(self.expression(0)?)
            } else {
                None
            };
            parameters.push((parameter, default));
            if !self.consume(&Token::Comma) {
                break;
            }
        }
        Ok(parameters)
    }

    fn for_statement(&mut self, intersection: bool) -> Result<Stmt, EngineError> {
        self.expect(&Token::LParen)?;
        let bindings = self.control_bindings()?;
        self.expect(&Token::RParen)?;
        let body = self.children()?;
        Ok(Stmt::For {
            bindings,
            body,
            intersection,
        })
    }

    fn let_statement(&mut self) -> Result<Stmt, EngineError> {
        self.expect(&Token::LParen)?;
        let bindings = self.control_bindings()?;
        self.expect(&Token::RParen)?;
        let body = self.children()?;
        Ok(Stmt::Let { bindings, body })
    }

    fn control_bindings(&mut self) -> Result<Vec<(String, Expr)>, EngineError> {
        let mut bindings = Vec::new();
        if self.check(&Token::RParen) {
            return Ok(bindings);
        }
        loop {
            let name = self.expect_identifier()?;
            self.expect(&Token::Equal)?;
            bindings.push((name, self.expression(0)?));
            if !self.consume(&Token::Comma) {
                break;
            }
        }
        Ok(bindings)
    }

    fn if_statement(&mut self) -> Result<Stmt, EngineError> {
        self.expect(&Token::LParen)?;
        let condition = self.expression(0)?;
        self.expect(&Token::RParen)?;
        let then_body = self.children()?;
        let else_body = if self.consume_identifier("else") {
            self.children()?
        } else {
            Vec::new()
        };
        Ok(Stmt::If {
            condition,
            then_body,
            else_body,
        })
    }

    fn arguments(&mut self) -> Result<Vec<Argument>, EngineError> {
        let mut arguments = Vec::new();
        if self.check(&Token::RParen) {
            return Ok(arguments);
        }
        loop {
            let named = match (
                self.tokens.get(self.cursor),
                self.tokens.get(self.cursor + 1),
            ) {
                (Some(Token::Ident(name)), Some(Token::Equal)) => Some(name.clone()),
                _ => None,
            };
            if named.is_some() {
                self.cursor += 2;
            }
            arguments.push(Argument {
                name: named,
                value: self.expression(0)?,
            });
            if !self.consume(&Token::Comma) {
                break;
            }
        }
        Ok(arguments)
    }

    fn children(&mut self) -> Result<Vec<Stmt>, EngineError> {
        if self.consume(&Token::Semicolon) {
            Ok(Vec::new())
        } else if self.check(&Token::LBrace) {
            self.block()
        } else {
            Ok(vec![self.statement()?])
        }
    }

    fn block(&mut self) -> Result<Vec<Stmt>, EngineError> {
        self.expect(&Token::LBrace)?;
        let mut statements = Vec::new();
        while !self.check(&Token::RBrace) {
            if self.cursor >= self.tokens.len() {
                return Err(EngineError::new("Unterminated statement block."));
            }
            if self.consume(&Token::Semicolon) {
                continue;
            }
            statements.push(self.statement()?);
        }
        self.expect(&Token::RBrace)?;
        Ok(statements)
    }

    fn expression(&mut self, minimum_precedence: u8) -> Result<Expr, EngineError> {
        const MAX_EXPRESSION_NESTING: usize = 128;
        if self.expression_depth >= MAX_EXPRESSION_NESTING {
            return Err(EngineError::new(format!(
                "Expression nesting depth exceeds the limit of {MAX_EXPRESSION_NESTING}."
            )));
        }
        self.expression_depth += 1;
        let result = self.expression_inner(minimum_precedence);
        self.expression_depth -= 1;
        result
    }

    fn expression_inner(&mut self, minimum_precedence: u8) -> Result<Expr, EngineError> {
        let mut left = if self.consume(&Token::Minus) {
            Expr::Unary(UnaryOp::Negate, Box::new(self.expression(9)?))
        } else if self.consume(&Token::Plus) {
            self.expression(9)?
        } else if self.consume(&Token::Bang) {
            Expr::Unary(UnaryOp::Not, Box::new(self.expression(9)?))
        } else {
            self.postfix()?
        };
        loop {
            let (operator, precedence) = match self.tokens.get(self.cursor) {
                Some(Token::OrOr) => (BinaryOp::Or, 2),
                Some(Token::AndAnd) => (BinaryOp::And, 3),
                Some(Token::EqualEqual) => (BinaryOp::Equal, 4),
                Some(Token::BangEqual) => (BinaryOp::NotEqual, 4),
                Some(Token::Less) => (BinaryOp::Less, 5),
                Some(Token::LessEqual) => (BinaryOp::LessEqual, 5),
                Some(Token::Greater) => (BinaryOp::Greater, 5),
                Some(Token::GreaterEqual) => (BinaryOp::GreaterEqual, 5),
                Some(Token::Plus) => (BinaryOp::Add, 6),
                Some(Token::Minus) => (BinaryOp::Subtract, 6),
                Some(Token::Star) => (BinaryOp::Multiply, 7),
                Some(Token::Slash) => (BinaryOp::Divide, 7),
                Some(Token::Percent) => (BinaryOp::Modulo, 7),
                Some(Token::Caret) => (BinaryOp::Power, 8),
                _ => break,
            };
            if precedence < minimum_precedence {
                break;
            }
            self.cursor += 1;
            let right = self.expression(if matches!(operator, BinaryOp::Power) {
                precedence
            } else {
                precedence + 1
            })?;
            left = Expr::Binary(Box::new(left), operator, Box::new(right));
        }
        if minimum_precedence <= 1 && self.consume(&Token::Question) {
            let then_value = self.expression(0)?;
            self.expect(&Token::Colon)?;
            let else_value = self.expression(1)?;
            left = Expr::Conditional(Box::new(left), Box::new(then_value), Box::new(else_value));
        }
        Ok(left)
    }

    fn postfix(&mut self) -> Result<Expr, EngineError> {
        let mut expression = self.primary()?;
        loop {
            if self.consume(&Token::LParen) {
                let arguments = self.arguments()?;
                self.expect(&Token::RParen)?;
                expression = Expr::Call(Box::new(expression), arguments);
            } else if self.consume(&Token::LBracket) {
                let index = self.expression(0)?;
                self.expect(&Token::RBracket)?;
                expression = Expr::Index(Box::new(expression), Box::new(index));
            } else if self.consume(&Token::Dot) {
                expression = Expr::Member(Box::new(expression), self.expect_identifier()?);
            } else {
                break;
            }
        }
        Ok(expression)
    }

    fn primary(&mut self) -> Result<Expr, EngineError> {
        match self.tokens.get(self.cursor).cloned() {
            Some(Token::Number(value)) => {
                self.cursor += 1;
                Ok(Expr::Number(value))
            }
            Some(Token::String(value)) => {
                self.cursor += 1;
                Ok(Expr::String(value))
            }
            Some(Token::Ident(name)) => {
                self.cursor += 1;
                if name == "function" {
                    self.expect(&Token::LParen)?;
                    let parameters = self.parameters()?;
                    self.expect(&Token::RParen)?;
                    let body = self.expression(0)?;
                    return Ok(Expr::Lambda(parameters, Box::new(body)));
                }
                if name == "let" {
                    self.expect(&Token::LParen)?;
                    let bindings = self.control_bindings()?;
                    self.expect(&Token::RParen)?;
                    let value = self.expression(0)?;
                    return Ok(Expr::Let(bindings, Box::new(value)));
                }
                if name == "true" {
                    return Ok(Expr::Bool(true));
                }
                if name == "false" {
                    return Ok(Expr::Bool(false));
                }
                if name == "undef" {
                    return Ok(Expr::Variable(name));
                }
                if matches!(name.as_str(), "assert" | "echo") && self.consume(&Token::LParen) {
                    let arguments = self.arguments()?;
                    self.expect(&Token::RParen)?;
                    if name == "assert" {
                        let mut arguments = arguments.into_iter();
                        let condition = arguments
                            .next()
                            .map(|argument| argument.value)
                            .ok_or_else(|| EngineError::new("assert() requires a condition."))?;
                        let message = arguments.next().map(|argument| Box::new(argument.value));
                        if arguments.next().is_some() {
                            return Err(EngineError::new(
                                "assert() accepts only a condition and optional message.",
                            ));
                        }
                        let value = self.expression(0)?;
                        Ok(Expr::Assert(Box::new(condition), message, Box::new(value)))
                    } else if name == "echo" {
                        let value = self.expression(0)?;
                        Ok(Expr::Echo(arguments, Box::new(value)))
                    } else {
                        unreachable!("only expression-prefix built-ins enter this branch")
                    }
                } else {
                    Ok(Expr::Variable(name))
                }
            }
            Some(Token::LParen) => {
                self.cursor += 1;
                let expression = self.expression(0)?;
                self.expect(&Token::RParen)?;
                Ok(expression)
            }
            Some(Token::LBracket) => self.vector_or_range(),
            token => Err(EngineError::new(format!(
                "Expected an expression, found {token:?}."
            ))),
        }
    }

    fn vector_or_range(&mut self) -> Result<Expr, EngineError> {
        self.expect(&Token::LBracket)?;
        if self.consume(&Token::RBracket) {
            return Ok(Expr::Vector(Vec::new()));
        }

        let first = if self.list_element_starts_control() {
            self.list_element()?
        } else {
            let first = self.expression(0)?;
            if self.consume(&Token::Colon) {
                let second = self.expression(0)?;
                let (step, end) = if self.consume(&Token::Colon) {
                    (second, self.expression(0)?)
                } else {
                    (Expr::Number(1.0), second)
                };
                self.expect(&Token::RBracket)?;
                return Ok(Expr::Range(Box::new(first), Box::new(step), Box::new(end)));
            }
            ListElement::Value(first)
        };
        let mut elements = vec![first];
        while self.consume(&Token::Comma) {
            if self.check(&Token::RBracket) {
                break;
            }
            elements.push(self.list_element()?);
        }
        self.expect(&Token::RBracket)?;
        if elements
            .iter()
            .all(|element| matches!(element, ListElement::Value(_)))
        {
            Ok(Expr::Vector(
                elements
                    .into_iter()
                    .map(|element| match element {
                        ListElement::Value(value) => value,
                        _ => unreachable!("all list elements were checked as values"),
                    })
                    .collect(),
            ))
        } else {
            Ok(Expr::Comprehension(elements))
        }
    }

    fn list_element_starts_control(&self) -> bool {
        matches!(
            self.tokens.get(self.cursor),
            Some(Token::Ident(name))
                if matches!(name.as_str(), "each" | "for" | "if" | "let")
        )
    }

    fn list_element(&mut self) -> Result<ListElement, EngineError> {
        const MAX_EXPRESSION_NESTING: usize = 128;
        if self.expression_depth >= MAX_EXPRESSION_NESTING {
            return Err(EngineError::new(format!(
                "Expression nesting depth exceeds the limit of {MAX_EXPRESSION_NESTING}."
            )));
        }
        self.expression_depth += 1;
        let result = self.list_element_inner();
        self.expression_depth -= 1;
        result
    }

    fn list_element_inner(&mut self) -> Result<ListElement, EngineError> {
        if self.consume_identifier("each") {
            return Ok(ListElement::Each(self.expression(0)?));
        }
        if self.consume_identifier("for") {
            self.expect(&Token::LParen)?;
            let bindings = self.control_bindings()?;
            if bindings.is_empty() {
                return Err(EngineError::new(
                    "List comprehension for() requires at least one binding.",
                ));
            }
            if self.consume(&Token::Semicolon) {
                let condition = self.expression(0)?;
                self.expect(&Token::Semicolon)?;
                let updates = self.control_bindings()?;
                if updates.is_empty() {
                    return Err(EngineError::new(
                        "C-style list comprehension for() requires an update binding.",
                    ));
                }
                self.expect(&Token::RParen)?;
                return Ok(ListElement::CStyleFor {
                    initial: bindings,
                    condition,
                    updates,
                    body: Box::new(self.list_element()?),
                });
            }
            self.expect(&Token::RParen)?;
            return Ok(ListElement::For {
                bindings,
                body: Box::new(self.list_element()?),
            });
        }
        if self.consume_identifier("if") {
            self.expect(&Token::LParen)?;
            let condition = self.expression(0)?;
            self.expect(&Token::RParen)?;
            let then_element = Box::new(self.list_element()?);
            let else_element = self
                .consume_identifier("else")
                .then(|| self.list_element())
                .transpose()?
                .map(Box::new);
            return Ok(ListElement::If {
                condition,
                then_element,
                else_element,
            });
        }
        if self.consume_identifier("let") {
            self.expect(&Token::LParen)?;
            let bindings = self.control_bindings()?;
            self.expect(&Token::RParen)?;
            return Ok(ListElement::Let {
                bindings,
                body: Box::new(self.list_element()?),
            });
        }
        Ok(ListElement::Value(self.expression(0)?))
    }

    fn expect_identifier(&mut self) -> Result<String, EngineError> {
        match self.tokens.get(self.cursor).cloned() {
            Some(Token::Ident(value)) => {
                self.cursor += 1;
                Ok(value)
            }
            token => Err(EngineError::new(format!(
                "Expected an identifier, found {token:?}."
            ))),
        }
    }

    fn check(&self, expected: &Token) -> bool {
        self.tokens.get(self.cursor) == Some(expected)
    }

    fn consume(&mut self, expected: &Token) -> bool {
        if self.check(expected) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn consume_identifier(&mut self, expected: &str) -> bool {
        if matches!(self.tokens.get(self.cursor), Some(Token::Ident(value)) if value == expected) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: &Token) -> Result<(), EngineError> {
        if self.consume(expected) {
            Ok(())
        } else {
            Err(EngineError::new(format!(
                "Expected {expected:?}, found {:?}.",
                self.tokens.get(self.cursor)
            )))
        }
    }
}

#[derive(Clone, Debug)]
enum Value {
    Number(f64),
    Bool(bool),
    String(String),
    Vector(Vec<Value>),
    Range { start: f64, step: f64, end: f64 },
    Function(Rc<FunctionValue>),
    Undefined,
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Number(left), Value::Number(right)) => left == right,
            (Value::Bool(left), Value::Bool(right)) => left == right,
            (Value::String(left), Value::String(right)) => left == right,
            (Value::Vector(left), Value::Vector(right)) => left == right,
            (
                Value::Range {
                    start: left_start,
                    step: left_step,
                    end: left_end,
                },
                Value::Range {
                    start: right_start,
                    step: right_step,
                    end: right_end,
                },
            ) => left_start == right_start && left_step == right_step && left_end == right_end,
            (Value::Function(left), Value::Function(right)) => Rc::ptr_eq(left, right),
            (Value::Undefined, Value::Undefined) => true,
            _ => false,
        }
    }
}

impl Value {
    fn has_non_finite_number(&self) -> bool {
        match self {
            Value::Number(value) => !value.is_finite(),
            Value::Vector(values) => values.iter().any(Value::has_non_finite_number),
            Value::Range { start, step, end } => {
                !start.is_finite() || !step.is_finite() || !end.is_finite()
            }
            _ => false,
        }
    }
}

/// A lexical scope: the names it binds itself, layered over a shared parent.
///
/// A function call binds a handful of names — its parameters, any `$`
/// overrides — on top of a captured scope that can hold every global in the
/// file. Copying that captured scope on every call was, by measurement, the
/// largest single cost in evaluating a module-heavy model: a third of the time
/// went into cloning the map and another third into dropping the copy. A scope
/// therefore stores only what it binds and reads everything else through a
/// reference-counted parent, so entering a frame costs the names it binds
/// rather than the names it can see.
///
/// Lookups shadow outward, nearest scope first, which is exactly the
/// precedence the flat map gave when a copy was made and then overwritten.
#[derive(Clone, Debug, Default)]
struct Environment {
    /// `None` for a self-contained scope. Never mutated once shared.
    parent: Option<Rc<Environment>>,
    own: HashMap<String, Value>,
}

impl Environment {
    fn new() -> Self {
        Self::default()
    }

    /// An empty scope that reads through to `parent`.
    fn layered_on(parent: &Rc<Environment>) -> Self {
        Self {
            parent: Some(Rc::clone(parent)),
            own: HashMap::new(),
        }
    }

    fn get(&self, name: &str) -> Option<&Value> {
        match self.own.get(name) {
            Some(value) => Some(value),
            None => self.parent.as_deref().and_then(|parent| parent.get(name)),
        }
    }

    fn insert(&mut self, name: String, value: Value) {
        self.own.insert(name, value);
    }

    fn extend(&mut self, bindings: impl IntoIterator<Item = (String, Value)>) {
        self.own.extend(bindings);
    }

    /// Every visible `$`-prefixed binding, nearest scope winning.
    ///
    /// `$` variables are the one thing that follows the dynamic call chain
    /// rather than the lexical one, so entering a module or a function copies
    /// them across. That is the only place anything needs to look at a whole
    /// scope, and there are only ever a handful of these names, so the
    /// shadowing check stays a linear scan of what has been found so far.
    fn special_variables(&self) -> Vec<(&String, &Value)> {
        let mut visible: Vec<(&String, &Value)> = Vec::new();
        let mut scope = Some(self);
        while let Some(current) = scope {
            for (name, value) in &current.own {
                if name.starts_with('$') && !visible.iter().any(|(seen, _)| *seen == name) {
                    visible.push((name, value));
                }
            }
            scope = current.parent.as_deref();
        }
        visible
    }

    /// This scope collapsed into one that owns every binding it can see.
    ///
    /// Capturing a scope for later re-entry flattens it so the captured value
    /// is a snapshot rather than a view onto scopes that may still be
    /// extended, and so the chain never deepens with nesting.
    fn flattened(&self) -> Self {
        if self.parent.is_none() {
            return self.clone();
        }
        let mut own = HashMap::new();
        let mut scope = Some(self);
        while let Some(current) = scope {
            for (name, value) in &current.own {
                // Nearest scope wins, so an outer binding never overwrites the
                // one already collected from an inner one.
                own.entry(name.clone()).or_insert_with(|| value.clone());
            }
            scope = current.parent.as_deref();
        }
        Self { parent: None, own }
    }

    /// Removes a binding this scope owns. Inherited bindings are untouched, so
    /// this is only meaningful on a [`Self::flattened`] scope.
    fn remove_own(&mut self, name: &str) {
        self.own.remove(name);
    }
}

#[derive(Clone, Debug)]
struct FunctionDefinition {
    parameters: Vec<(String, Option<Expr>)>,
    body: Expr,
}

#[derive(Clone, Debug)]
struct FunctionValue {
    name: Option<String>,
    parameters: Vec<(String, Option<Expr>)>,
    body: Expr,
    /// The defining scope, shared rather than copied: every call to this
    /// function layers its own bindings over this one map.
    environment: Rc<Environment>,
    same_scope_functions: Rc<HashMap<String, FunctionDefinition>>,
}

fn install_scope_functions(
    environment: &mut Environment,
    definitions: &Rc<HashMap<String, FunctionDefinition>>,
) {
    // Flattened so the capture is a snapshot of the scope as it stands now,
    // and shared so installing twenty functions costs one copy rather than
    // twenty-one. The scope's own function names are dropped from it because
    // `bind_function_arguments` reinstates them per call from
    // `same_scope_functions`; keeping them would pin a stale generation.
    let mut captured = environment.flattened();
    for name in definitions.keys() {
        captured.remove_own(name);
    }
    let captured = Rc::new(captured);
    for (name, definition) in definitions.iter() {
        environment.insert(
            name.clone(),
            Value::Function(Rc::new(FunctionValue {
                name: Some(name.clone()),
                parameters: definition.parameters.clone(),
                body: definition.body.clone(),
                environment: Rc::clone(&captured),
                same_scope_functions: definitions.clone(),
            })),
        );
    }
}

fn collect_assignment_dependencies(
    expression: &Expr,
    assignment_names: &HashSet<String>,
    function_definitions: &HashMap<String, FunctionDefinition>,
    bound_names: &HashSet<String>,
    visiting_functions: &mut HashSet<String>,
    dependencies: &mut HashSet<String>,
) {
    let mut recurse = |expression: &Expr| {
        collect_assignment_dependencies(
            expression,
            assignment_names,
            function_definitions,
            bound_names,
            visiting_functions,
            dependencies,
        )
    };
    match expression {
        Expr::Variable(name) if assignment_names.contains(name) && !bound_names.contains(name) => {
            dependencies.insert(name.clone());
        }
        Expr::Vector(values) => {
            for value in values {
                recurse(value);
            }
        }
        Expr::Range(start, step, end) => {
            recurse(start);
            recurse(step);
            recurse(end);
        }
        Expr::Unary(_, value) | Expr::Member(value, _) => recurse(value),
        Expr::Binary(left, _, right) | Expr::Index(left, right) => {
            recurse(left);
            recurse(right);
        }
        Expr::Conditional(condition, then_value, else_value) => {
            recurse(condition);
            recurse(then_value);
            recurse(else_value);
        }
        Expr::Let(bindings, value) => {
            let mut local_bound_names = bound_names.clone();
            for (name, binding) in bindings {
                collect_assignment_dependencies(
                    binding,
                    assignment_names,
                    function_definitions,
                    &local_bound_names,
                    visiting_functions,
                    dependencies,
                );
                local_bound_names.insert(name.clone());
            }
            collect_assignment_dependencies(
                value,
                assignment_names,
                function_definitions,
                &local_bound_names,
                visiting_functions,
                dependencies,
            );
        }
        Expr::Assert(condition, message, value) => {
            recurse(condition);
            if let Some(message) = message {
                recurse(message);
            }
            recurse(value);
        }
        Expr::Echo(arguments, value) => {
            for argument in arguments {
                recurse(&argument.value);
            }
            recurse(value);
        }
        Expr::Call(callee, arguments) => {
            for argument in arguments {
                recurse(&argument.value);
            }
            if let Expr::Variable(name) = callee.as_ref() {
                if visiting_functions.insert(name.clone()) {
                    if let Some(function) = function_definitions.get(name) {
                        let mut function_bound_names = bound_names.clone();
                        function_bound_names.extend(
                            function
                                .parameters
                                .iter()
                                .map(|(parameter, _)| parameter.clone()),
                        );
                        collect_assignment_dependencies(
                            &function.body,
                            assignment_names,
                            function_definitions,
                            &function_bound_names,
                            visiting_functions,
                            dependencies,
                        );
                    } else {
                        collect_assignment_dependencies(
                            callee,
                            assignment_names,
                            function_definitions,
                            bound_names,
                            visiting_functions,
                            dependencies,
                        );
                    }
                    visiting_functions.remove(name);
                }
            } else {
                collect_assignment_dependencies(
                    callee,
                    assignment_names,
                    function_definitions,
                    bound_names,
                    visiting_functions,
                    dependencies,
                );
            }
        }
        Expr::Comprehension(elements) => {
            for element in elements {
                collect_list_element_dependencies(
                    element,
                    assignment_names,
                    function_definitions,
                    bound_names,
                    visiting_functions,
                    dependencies,
                );
            }
        }
        Expr::Lambda(parameters, body) => {
            let mut lambda_bound_names = bound_names.clone();
            lambda_bound_names.extend(parameters.iter().map(|(parameter, _)| parameter.clone()));
            collect_assignment_dependencies(
                body,
                assignment_names,
                function_definitions,
                &lambda_bound_names,
                visiting_functions,
                dependencies,
            );
        }
        Expr::Number(_) | Expr::Bool(_) | Expr::String(_) | Expr::Variable(_) => {}
    }
}

fn collect_list_element_dependencies(
    element: &ListElement,
    assignment_names: &HashSet<String>,
    function_definitions: &HashMap<String, FunctionDefinition>,
    bound_names: &HashSet<String>,
    visiting_functions: &mut HashSet<String>,
    dependencies: &mut HashSet<String>,
) {
    let mut collect = |expression: &Expr, local_bound_names: &HashSet<String>| {
        collect_assignment_dependencies(
            expression,
            assignment_names,
            function_definitions,
            local_bound_names,
            visiting_functions,
            dependencies,
        );
    };
    match element {
        ListElement::Value(value) | ListElement::Each(value) => collect(value, bound_names),
        ListElement::For { bindings, body } => {
            let mut local_bound_names = bound_names.clone();
            for (name, iterable) in bindings {
                collect(iterable, &local_bound_names);
                local_bound_names.insert(name.clone());
            }
            collect_list_element_dependencies(
                body,
                assignment_names,
                function_definitions,
                &local_bound_names,
                visiting_functions,
                dependencies,
            );
        }
        ListElement::CStyleFor {
            initial,
            condition,
            updates,
            body,
        } => {
            let mut local_bound_names = bound_names.clone();
            for (name, value) in initial {
                collect(value, &local_bound_names);
                local_bound_names.insert(name.clone());
            }
            collect(condition, &local_bound_names);
            for (_, value) in updates {
                collect(value, &local_bound_names);
            }
            collect_list_element_dependencies(
                body,
                assignment_names,
                function_definitions,
                &local_bound_names,
                visiting_functions,
                dependencies,
            );
        }
        ListElement::If {
            condition,
            then_element,
            else_element,
        } => {
            collect(condition, bound_names);
            collect_list_element_dependencies(
                then_element,
                assignment_names,
                function_definitions,
                bound_names,
                visiting_functions,
                dependencies,
            );
            if let Some(else_element) = else_element {
                collect_list_element_dependencies(
                    else_element,
                    assignment_names,
                    function_definitions,
                    bound_names,
                    visiting_functions,
                    dependencies,
                );
            }
        }
        ListElement::Let { bindings, body } => {
            let mut local_bound_names = bound_names.clone();
            for (name, value) in bindings {
                collect(value, &local_bound_names);
                local_bound_names.insert(name.clone());
            }
            collect_list_element_dependencies(
                body,
                assignment_names,
                function_definitions,
                &local_bound_names,
                visiting_functions,
                dependencies,
            );
        }
    }
}

fn resolve_scope_assignment(
    name: &str,
    assignments: &HashMap<String, Expr>,
    assignment_names: &HashSet<String>,
    function_definitions: &Rc<HashMap<String, FunctionDefinition>>,
    states: &mut HashMap<String, u8>,
    environment: &mut Environment,
) -> Result<(), EngineError> {
    match states.get(name).copied() {
        Some(2) | Some(1) => return Ok(()),
        _ => {}
    }
    states.insert(name.to_owned(), 1);
    let expression = assignments
        .get(name)
        .expect("assignment name and expression map stay in sync");
    let mut dependencies = HashSet::new();
    collect_assignment_dependencies(
        expression,
        assignment_names,
        function_definitions,
        &HashSet::new(),
        &mut HashSet::new(),
        &mut dependencies,
    );
    for dependency in dependencies {
        resolve_scope_assignment(
            &dependency,
            assignments,
            assignment_names,
            function_definitions,
            states,
            environment,
        )?;
    }
    install_scope_functions(environment, function_definitions);
    let value = evaluate_expression(expression, environment)?;
    environment.insert(name.to_owned(), value);
    states.insert(name.to_owned(), 2);
    Ok(())
}

thread_local! {
    static EXPRESSION_WARNINGS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static FUNCTION_CALL_DEPTH: RefCell<usize> = const { RefCell::new(0) };
}

fn expression_warning(message: impl Into<String>) {
    EXPRESSION_WARNINGS.with(|warnings| {
        let message = format!("WARNING: {}", message.into());
        let mut warnings = warnings.borrow_mut();
        if !warnings.contains(&message) {
            warnings.push(message);
        }
    });
}

fn expression_message(message: impl Into<String>) {
    EXPRESSION_WARNINGS.with(|messages| messages.borrow_mut().push(message.into()));
}

fn take_expression_warnings() -> Vec<String> {
    EXPRESSION_WARNINGS.with(|warnings| std::mem::take(&mut *warnings.borrow_mut()))
}

/// Module scopes are entered once per statement list and once per module call,
/// so every field a table shares with its parent is behind an `Rc`. Cloning a
/// `ModuleDefinition` — or a whole table — then costs reference-count bumps
/// instead of deep-copying parameter lists, bodies, environments, and the
/// recursively nested table of enclosing modules.
type ModuleTable = HashMap<String, ModuleDefinition>;

#[derive(Clone)]
struct ModuleSyntax {
    parameters: Rc<Vec<(String, Option<Expr>)>>,
    body: Rc<Vec<Stmt>>,
    environment: Rc<Environment>,
}

#[derive(Clone)]
struct ModuleDefinition {
    parameters: Rc<Vec<(String, Option<Expr>)>>,
    body: Rc<Vec<Stmt>>,
    environment: Rc<Environment>,
    outer_modules: Rc<ModuleTable>,
    same_scope_modules: Rc<HashMap<String, ModuleSyntax>>,
}

fn install_scope_modules(
    modules: &mut Rc<ModuleTable>,
    outer_modules: &Rc<ModuleTable>,
    definitions: &Rc<HashMap<String, ModuleSyntax>>,
) {
    if definitions.is_empty() {
        // Same result as rebuilding the table from `outer_modules` and adding
        // nothing to it, without touching a single entry.
        *modules = Rc::clone(outer_modules);
        return;
    }
    let mut table = ModuleTable::clone(outer_modules);
    for (name, definition) in definitions.iter() {
        table.insert(
            name.clone(),
            ModuleDefinition {
                parameters: Rc::clone(&definition.parameters),
                body: Rc::clone(&definition.body),
                environment: Rc::clone(&definition.environment),
                outer_modules: Rc::clone(outer_modules),
                same_scope_modules: Rc::clone(definitions),
            },
        );
    }
    *modules = Rc::new(table);
}

#[derive(Clone)]
struct ChildContext {
    statements: Vec<Stmt>,
    environment: Environment,
}

struct Evaluator<'a> {
    modules: Rc<ModuleTable>,
    diagnostics: Vec<String>,
    child_contexts: Vec<ChildContext>,
    hull_cache: HashMap<Vec<[u64; 3]>, Shape>,
    module_depth: usize,
    cancellation: Option<&'a AtomicBool>,
    work_steps: usize,
    preview: bool,
    /// Whether the shape tree is being built for the exact polyhedral kernel.
    ///
    /// The tree itself is the same either way; this only decides which
    /// approximation warnings are true. `minkowski()` is approximated by the
    /// sampled mesher and computed exactly by the kernel, so the warning about
    /// it belongs to one path and not the other.
    exact: bool,
    /// Shapes gathered from `%`-modified subtrees. They are kept out of the
    /// evaluated geometry entirely and handed to the caller separately so a
    /// preview can still draw them.
    background_shapes: Vec<Shape>,
    /// The outermost `!`-modified subtree, which replaces the whole model
    /// once evaluation finishes.
    root_shapes: Option<Vec<Shape>>,
    /// Set as soon as a `!` is entered, before its subtree is walked, so the
    /// outermost `!` — not the first one reached — owns the root.
    root_claimed: bool,
}

impl Default for Evaluator<'_> {
    fn default() -> Self {
        Self {
            modules: Rc::new(ModuleTable::new()),
            diagnostics: Vec::new(),
            child_contexts: Vec::new(),
            hull_cache: HashMap::new(),
            module_depth: 0,
            cancellation: None,
            work_steps: 0,
            preview: true,
            exact: false,
            background_shapes: Vec::new(),
            root_shapes: None,
            root_claimed: false,
        }
    }
}

// Module evaluation clones lexical environments and module tables per frame,
// so keep this below the smallest stack used by Rust's test/runtime threads.
const MAX_MODULE_RECURSION_DEPTH: usize = 16;
const MAX_FOR_ITERATIONS: usize = 10_000;
const MAX_EVALUATION_WORK_STEPS: usize = 1_000_000;
const MAX_HULL_CACHE_ENTRIES: usize = 1_024;
const MAX_CURVE_FRAGMENTS: usize = 100_000;

impl Evaluator<'_> {
    fn evaluate(&mut self, statements: &[Stmt]) -> Result<Shape, EngineError> {
        self.evaluate_objects(statements).and_then(|shapes| {
            Shape::union(shapes)
                .ok_or_else(|| EngineError::new("The program produced no geometry."))
        })
    }

    fn evaluate_objects(&mut self, statements: &[Stmt]) -> Result<Vec<Shape>, EngineError> {
        take_expression_warnings();
        let mut environment = Environment::new();
        environment.extend([
            ("$fn".into(), Value::Number(0.0)),
            ("$fa".into(), Value::Number(12.0)),
            ("$fs".into(), Value::Number(2.0)),
            ("$t".into(), Value::Number(0.0)),
            ("$preview".into(), Value::Bool(self.preview)),
            ("$children".into(), Value::Number(0.0)),
        ]);
        let result = self.statements(statements, &environment, true);
        self.diagnostics.extend(take_expression_warnings());
        let mut shapes = result?;
        // A `!` anywhere in the program overrides everything else, so it is
        // resolved once here rather than at each nesting level.
        if let Some(root) = self.root_shapes.take() {
            self.diagnostics.push(
                "WARNING: root modifier (!) in effect; only that subtree is rendered.".into(),
            );
            self.background_shapes.clear();
            shapes = root;
        }
        if !self.background_shapes.is_empty() {
            self.diagnostics.push(format!(
                "Background modifier (%) excluded {} object(s) from the geometry.",
                self.background_shapes.len()
            ));
        }
        Ok(shapes)
    }

    /// Evaluates a lexical scope.
    ///
    /// The environment is borrowed, not owned: a scope that declares no
    /// variables and no functions cannot change what its caller sees, so it
    /// reads the caller's map directly. Only a scope that actually binds
    /// something pays for a copy, and it makes that copy here rather than at
    /// every call site. On a model whose geometry comes out of nested `for`
    /// loops that is the difference between one hash-map clone per loop and
    /// one per iteration per nesting level.
    fn statements(
        &mut self,
        statements: &[Stmt],
        environment: &Environment,
        group_top_level: bool,
    ) -> Result<Vec<Shape>, EngineError> {
        self.consume_work(1)?;
        // OpenSCAD assignments and module definitions apply throughout their
        // lexical scope, including statements that appear before the definition.
        let mut final_assignments = Vec::<(String, Expr)>::new();
        let mut assignment_indices = HashMap::<String, usize>::new();
        let mut assigned_names = HashMap::<&str, usize>::new();
        for statement in statements {
            if let Stmt::Assign {
                name,
                expression,
                from_include,
            } = statement
            {
                let local_assignments = assigned_names.entry(name.as_str()).or_default();
                if !from_include {
                    *local_assignments += 1;
                }
                if *local_assignments == 2 {
                    self.diagnostics.push(format!(
                        "WARNING: Variable {name} was assigned more than once in the same scope."
                    ));
                }
                if let Some(index) = assignment_indices.get(name).copied() {
                    final_assignments[index].1 = expression.clone();
                } else {
                    assignment_indices.insert(name.clone(), final_assignments.len());
                    final_assignments.push((name.clone(), expression.clone()));
                }
            }
        }

        // Installing an empty definition set leaves the environment untouched
        // but still deep-copies it twice, so only build and install when this
        // scope actually declares functions.
        let declares_functions = statements
            .iter()
            .any(|statement| matches!(statement, Stmt::Function { .. }));
        let same_scope_functions = Rc::new(if declares_functions {
            statements
                .iter()
                .filter_map(|statement| match statement {
                    Stmt::Function {
                        name,
                        parameters,
                        body,
                    } => Some((
                        name.clone(),
                        FunctionDefinition {
                            parameters: parameters.clone(),
                            body: body.clone(),
                        },
                    )),
                    _ => None,
                })
                .collect::<HashMap<_, _>>()
        } else {
            HashMap::new()
        });

        // A binding's expression sees the final bindings in its scope. Resolve
        // forward dependencies before evaluating each final expression once;
        // single evaluation matters for intentionally nondeterministic rands().
        // Nothing below writes to the environment unless this scope binds a
        // name or a function, so only then is a private copy made.
        let scope_environment;
        let environment: &Environment = if final_assignments.is_empty() && !declares_functions {
            environment
        } else {
            let mut scope = environment.clone();
            let assignment_names = assignment_indices.keys().cloned().collect::<HashSet<_>>();
            let assignments = final_assignments.iter().cloned().collect::<HashMap<_, _>>();
            for name in &assignment_names {
                scope.insert(name.clone(), Value::Undefined);
            }
            if declares_functions {
                install_scope_functions(&mut scope, &same_scope_functions);
            }
            let mut assignment_states = HashMap::new();
            for (name, _) in &final_assignments {
                resolve_scope_assignment(
                    name,
                    &assignments,
                    &assignment_names,
                    &same_scope_functions,
                    &mut assignment_states,
                    &mut scope,
                )?;
            }
            for (name, _) in &final_assignments {
                if let Some(value) = scope.get(name).cloned() {
                    self.note_value(&value);
                }
            }

            // Rebuild once more from the settled assignment environment so every
            // named function captures identical values and resolves mutual peers.
            if declares_functions {
                install_scope_functions(&mut scope, &same_scope_functions);
            }
            scope_environment = scope;
            &scope_environment
        };

        let outer_modules = Rc::clone(&self.modules);
        // A scope that declares no modules inherits the caller's table
        // unchanged, so skip snapshotting the environment and rebuilding it.
        if statements
            .iter()
            .any(|statement| matches!(statement, Stmt::Module { .. }))
        {
            let scope_environment = Rc::new(environment.flattened());
            let same_scope_modules = Rc::new(
                statements
                    .iter()
                    .filter_map(|statement| match statement {
                        Stmt::Module {
                            name,
                            parameters,
                            body,
                        } => Some((
                            name.clone(),
                            ModuleSyntax {
                                parameters: Rc::new(parameters.clone()),
                                body: Rc::new(body.clone()),
                                environment: Rc::clone(&scope_environment),
                            },
                        )),
                        _ => None,
                    })
                    .collect::<HashMap<_, _>>(),
            );
            install_scope_modules(&mut self.modules, &outer_modules, &same_scope_modules);
        }

        let result = (|| {
            let mut shapes = Vec::new();
            for statement in statements {
                self.consume_work(1)?;
                let statement_start = shapes.len();
                match statement {
                    Stmt::Block(body) => {
                        shapes.extend(self.statements(body, environment, false)?);
                    }
                    Stmt::Disabled => {}
                    // `%`: still evaluated, so its errors and side effects are
                    // real, but its geometry is diverted away from `shapes`
                    // and therefore out of the CSG result and every export.
                    Stmt::Background(inner) => {
                        let inner = std::slice::from_ref(inner.as_ref());
                        let produced = self.statements(inner, environment, false)?;
                        self.background_shapes.extend(produced);
                    }
                    // `!`: the marked subtree becomes the entire model. It is
                    // captured here rather than appended, which is what drops
                    // the transformations that wrap it.
                    Stmt::Root(inner) => {
                        let inner = std::slice::from_ref(inner.as_ref());
                        // The claim is staked *before* descending, so an
                        // enclosing `!` beats a nested one — OpenSCAD picks
                        // the outermost root tag, not the first one reached.
                        // A `!` inside an already-claimed subtree is then just
                        // an ordinary node within it.
                        if self.root_claimed {
                            shapes.extend(self.statements(inner, environment, false)?);
                        } else {
                            self.root_claimed = true;
                            let produced = self.statements(inner, environment, false)?;
                            self.root_shapes = Some(produced);
                        }
                    }
                    Stmt::Assign { .. } | Stmt::Function { .. } | Stmt::Module { .. } => {}
                    // `resolve_file_directives` splices away every top-level
                    // directive before evaluation, so reaching one here means
                    // it was nested inside a block, module or loop — where
                    // §17 does not allow it.
                    Stmt::FileDirective { path, line, mode } => {
                        let keyword = match mode {
                            FileDirectiveMode::Include => "include",
                            FileDirectiveMode::Use => "use",
                        };
                        self.diagnostics.push(format!(
                            "WARNING: line {line}: {keyword} <{path}> must appear at the top \
                             level; ignoring it."
                        ));
                    }
                    Stmt::For {
                        bindings,
                        body,
                        intersection,
                    } => {
                        let mut iteration_count = 0;
                        let iteration_shapes = self.for_iteration_shapes(
                            bindings,
                            0,
                            body,
                            environment,
                            &mut iteration_count,
                        )?;
                        if *intersection && !iteration_shapes.is_empty() {
                            shapes.push(Shape::Intersection(iteration_shapes));
                        } else {
                            shapes.extend(iteration_shapes);
                        }
                    }
                    Stmt::Let { bindings, body } => {
                        let mut local = environment.clone();
                        for (name, expression) in bindings {
                            let value = evaluate_expression(expression, &local)?;
                            self.note_value(&value);
                            local.insert(name.clone(), value);
                        }
                        shapes.extend(self.statements(body, &local, false)?);
                    }
                    Stmt::If {
                        condition,
                        then_body,
                        else_body,
                    } => {
                        let condition = evaluate_expression(condition, environment)?;
                        self.note_value(&condition);
                        let selected = if truthy(&condition) {
                            then_body
                        } else {
                            else_body
                        };
                        shapes.extend(self.statements(selected, environment, false)?);
                    }
                    Stmt::Call {
                        name,
                        arguments,
                        children,
                    } => {
                        if let Some(shape) = self.call(name, arguments, children, environment)? {
                            shapes.push(shape);
                        }
                    }
                }
                if group_top_level && shapes.len() > statement_start + 1 {
                    let statement_shapes = shapes.drain(statement_start..).collect();
                    if let Some(shape) = Shape::union(statement_shapes) {
                        shapes.push(shape);
                    }
                }
            }
            Ok(shapes)
        })();
        self.modules = outer_modules;
        result
    }

    fn for_iteration_shapes(
        &mut self,
        bindings: &[(String, Expr)],
        binding_index: usize,
        body: &[Stmt],
        environment: &Environment,
        iteration_count: &mut usize,
    ) -> Result<Vec<Shape>, EngineError> {
        if binding_index == bindings.len() {
            self.consume_work(1)?;
            *iteration_count += 1;
            if *iteration_count > MAX_FOR_ITERATIONS {
                return Err(EngineError::new(format!(
                    "for() exceeds the {MAX_FOR_ITERATIONS} iteration limit."
                )));
            }
            return Ok(Shape::union(self.statements(body, environment, false)?)
                .into_iter()
                .collect());
        }

        let (name, expression) = &bindings[binding_index];
        let iterable = evaluate_expression(expression, environment)?;
        self.note_value(&iterable);
        let values = Self::iterable_values(iterable)?;
        let mut shapes = Vec::new();
        // The loop variable is the only binding that changes between
        // iterations, and nothing below this point writes to `local`, so one
        // copy serves the whole loop instead of one per iteration.
        let mut local = environment.clone();
        for value in values {
            local.insert(name.clone(), value);
            shapes.extend(self.for_iteration_shapes(
                bindings,
                binding_index + 1,
                body,
                &local,
                iteration_count,
            )?);
        }
        Ok(shapes)
    }

    fn consume_work(&mut self, amount: usize) -> Result<(), EngineError> {
        check_cancelled(self.cancellation)?;
        self.work_steps = self.work_steps.saturating_add(amount);
        if self.work_steps > MAX_EVALUATION_WORK_STEPS {
            Err(EngineError::new(format!(
                "Evaluation exceeded the global work limit of {MAX_EVALUATION_WORK_STEPS} steps."
            )))
        } else {
            Ok(())
        }
    }

    fn iterable_values(value: Value) -> Result<Vec<Value>, EngineError> {
        match value {
            Value::Range { start, step, end } => {
                if step == 0.0 {
                    return Err(EngineError::new("for() range step cannot be zero."));
                }
                let mut values = Vec::new();
                let mut value = start;
                while if step > 0.0 {
                    value <= end + 1e-9
                } else {
                    value >= end - 1e-9
                } {
                    if values.len() >= MAX_FOR_ITERATIONS {
                        return Err(EngineError::new(format!(
                            "for() exceeds the {MAX_FOR_ITERATIONS} iteration limit."
                        )));
                    }
                    values.push(Value::Number(value));
                    value += step;
                }
                Ok(values)
            }
            Value::Vector(values) => Ok(values),
            _ => Err(EngineError::new("for() expects a range or vector.")),
        }
    }

    fn call(
        &mut self,
        name: &str,
        arguments: &[Argument],
        children: &[Stmt],
        environment: &Environment,
    ) -> Result<Option<Shape>, EngineError> {
        // `$`-prefixed arguments are the only thing that can differ between the
        // caller's environment and the callee's, and most calls carry none.
        // Cloning the environment regardless costs a hash-map copy per call
        // node, which on a model built from tens of thousands of primitives is
        // the single largest allocation source in evaluation.
        // `$`-prefixed arguments are the only thing that can differ between the
        // caller's environment and the callee's, and most calls carry none.
        // Cloning the environment regardless costs a hash-map copy per call
        // node, which on a model built from tens of thousands of primitives is
        // the single largest allocation source in evaluation.
        let has_special_arguments = arguments.iter().any(|argument| {
            argument
                .name
                .as_deref()
                .is_some_and(|name| name.starts_with('$'))
        });
        let call_environment;
        let environment: &Environment = if has_special_arguments {
            let mut overridden = environment.clone();
            for argument in arguments {
                if let Some(special) = argument
                    .name
                    .as_deref()
                    .filter(|name| name.starts_with('$'))
                {
                    let value = evaluate_expression(&argument.value, environment)?;
                    self.note_value(&value);
                    overridden.insert(special.to_owned(), value);
                }
            }
            call_environment = overridden;
            &call_environment
        } else {
            environment
        };

        match name {
            "square" => {
                let size = argument_vector(arguments, "size", 0, environment, vec![1.0]);
                let Some(size) = self.geometry_argument("square", size) else {
                    return Ok(None);
                };
                let size = match size.as_slice() {
                    [one] => [*one, *one],
                    [width, height] => [*width, *height],
                    _ => {
                        self.ignore_geometry("square", "size must be a number or 2-vector");
                        return Ok(None);
                    }
                };
                if !size
                    .into_iter()
                    .all(|value| value.is_finite() && value > 0.0)
                {
                    self.ignore_geometry(
                        "square",
                        "size components must be finite and positive",
                    );
                    return Ok(None);
                }
                let center = argument_bool(arguments, "center", 1, environment, false);
                let Some(center) = self.geometry_argument("square", center) else {
                    return Ok(None);
                };
                Ok(Some(Shape::Square2d { size, center }))
            }
            "polygon" => {
                let points = argument_points2(arguments, "points", 0, environment);
                let Some(points) = self.geometry_argument("polygon", points) else {
                    return Ok(None);
                };
                if points.len() < 3 {
                    self.ignore_geometry("polygon", "points must contain at least 3 vertices");
                    return Ok(None);
                }
                // `paths` selects which points form each closed contour. The
                // first contour is the outline; every later one cuts against
                // what is already there under the even-odd rule, which is how
                // holes (and islands inside holes) are expressed.
                let paths = argument_index_paths(arguments, "paths", 1, environment, points.len());
                let Some(paths) = self.geometry_argument("polygon", paths) else {
                    return Ok(None);
                };
                // `convexity` is a preview-only depth hint; this engine meshes
                // the implicit solid directly, so it is accepted and ignored.
                if let Some(convexity) = argument(arguments, "convexity", 2) {
                    let value = evaluate_expression(convexity, environment)?;
                    self.note_value(&value);
                }
                let contours = match paths {
                    None => vec![points],
                    Some(paths) => {
                        let contours = paths
                            .into_iter()
                            .map(|path| {
                                path.into_iter().map(|index| points[index]).collect::<Vec<_>>()
                            })
                            .filter(|contour: &Vec<[f64; 2]>| contour.len() >= 3)
                            .collect::<Vec<_>>();
                        if contours.is_empty() {
                            self.ignore_geometry(
                                "polygon",
                                "paths must contain at least one contour of 3 or more vertices",
                            );
                            return Ok(None);
                        }
                        contours
                    }
                };
                Ok(Some(Shape::Polygon2d { contours }))
            }
            "circle" => {
                let radius = radius_or_diameter_argument(arguments, 0, environment, 1.0);
                let Some(radius) = self.geometry_argument("circle", radius) else {
                    return Ok(None);
                };
                if !radius.is_finite() || radius <= 0.0 {
                    self.ignore_geometry("circle", "radius must be finite and positive");
                    return Ok(None);
                }
                Ok(Some(Shape::Circle2d {
                    radius,
                    fragments: fragment_count(radius, environment),
                }))
            }
            "cube" => {
                let size = argument_vector(
                    arguments,
                    "size",
                    0,
                    environment,
                    vec![1.0, 1.0, 1.0],
                );
                let Some(size) = self.geometry_argument("cube", size) else {
                    return Ok(None);
                };
                let size = match size.as_slice() {
                    [one] => Vec3::new(*one, *one, *one),
                    [x, y, z, ..] => Vec3::new(*x, *y, *z),
                    _ => {
                        self.ignore_geometry("cube", "size must be a number or 3-vector");
                        return Ok(None);
                    }
                };
                if ![size.x, size.y, size.z]
                    .into_iter()
                    .all(|value| value.is_finite() && value > 0.0)
                {
                    self.ignore_geometry("cube", "size components must be finite and positive");
                    return Ok(None);
                }
                let center = argument_bool(arguments, "center", 1, environment, false);
                let Some(center) = self.geometry_argument("cube", center) else {
                    return Ok(None);
                };
                Ok(Some(Shape::Box { size, center }))
            }
            "sphere" => {
                let radius = radius_or_diameter_argument(arguments, 0, environment, 1.0);
                let Some(radius) = self.geometry_argument("sphere", radius) else {
                    return Ok(None);
                };
                if !radius.is_finite() || radius <= 0.0 {
                    self.ignore_geometry("sphere", "radius must be finite and positive");
                    return Ok(None);
                }
                Ok(Some(Shape::Sphere {
                    radius,
                    fragments: fragment_count(radius, environment),
                }))
            }
            "cylinder" => {
                let height = argument_number(arguments, "h", 0, environment, 1.0);
                let Some(height) = self.geometry_argument("cylinder", height) else {
                    return Ok(None);
                };
                let radii = cylinder_radii(arguments, environment);
                let Some((radius1, radius2)) = self.geometry_argument("cylinder", radii) else {
                    return Ok(None);
                };
                let center = argument_bool(arguments, "center", 3, environment, false);
                let Some(center) = self.geometry_argument("cylinder", center) else {
                    return Ok(None);
                };
                let valid_height = height.is_finite() && height > 0.0;
                let valid_radii = [radius1, radius2]
                    .into_iter()
                    .all(|value| value.is_finite() && value >= 0.0)
                    && (radius1 > 0.0 || radius2 > 0.0);
                if !valid_height || !valid_radii {
                    self.ignore_geometry(
                        "cylinder",
                        "height must be finite and positive, and at least one finite radius must be positive",
                    );
                    return Ok(None);
                }
                Ok(Some(Shape::Cylinder {
                    height,
                    radius1,
                    radius2,
                    center,
                    fragments: fragment_count(radius1.max(radius2), environment),
                }))
            }
            "union" => {
                let shapes = self.child_shapes(children, environment)?;
                self.require_same_dimension("union", &shapes)?;
                Ok(Shape::union(shapes))
            }
            "difference" => {
                let mut shapes = self.child_shapes(children, environment)?;
                if shapes.is_empty() {
                    Ok(None)
                } else {
                    self.require_same_dimension("difference", &shapes)?;
                    let first = shapes.remove(0);
                    Ok(Some(Shape::difference(first, shapes)))
                }
            }
            "intersection" => {
                let shapes = self.child_shapes(children, environment)?;
                if shapes.is_empty() {
                    Ok(None)
                } else {
                    self.require_same_dimension("intersection", &shapes)?;
                    Ok(Some(Shape::Intersection(shapes)))
                }
            }
            "minkowski" => {
                if let Some(convexity) = argument(arguments, "convexity", 0) {
                    let value = evaluate_expression(convexity, environment)?;
                    self.note_value(&value);
                }
                let mut shapes = self.child_shapes(children, environment)?;
                if shapes.is_empty() {
                    return Ok(None);
                }
                self.require_same_dimension("minkowski", &shapes)?;
                let mut result = shapes.remove(0);
                for kernel in shapes {
                    let bounds = kernel.bounds();
                    let offset = bounds.min.add(bounds.max).mul(0.5);
                    let half_span = bounds.max.sub(bounds.min).mul(0.5);
                    let radius = match kernel.dimension() {
                        ShapeDimension::Planar => half_span.x.max(half_span.y),
                        ShapeDimension::Solid => {
                            half_span.x.max(half_span.y).max(half_span.z)
                        }
                    };
                    if !radius.is_finite() || radius <= 0.0 || !vec3_is_finite(offset) {
                        self.ignore_geometry(
                            "minkowski",
                            "child bounds must be finite and have positive extent",
                        );
                        return Ok(None);
                    }
                    result = Shape::MinkowskiDilation {
                        shape: Box::new(result),
                        offset,
                        radius,
                        kernel: Some(Box::new(kernel)),
                    };
                }
                if !self.exact {
                    // Only the sampled path approximates. Saying so
                    // unconditionally would be wrong now that the polyhedral
                    // kernel computes the real sum from the operand above.
                    self.diagnostics.push(
                        "WARNING: minkowski() is approximated by a bounded dilation in the sampled kernel."
                            .to_string(),
                    );
                }
                Ok(Some(result))
            }
            "hull" => {
                let shapes = self.child_shapes(children, environment)?;
                if shapes.is_empty() {
                    return Ok(None);
                }
                if shapes
                    .iter()
                    .any(|shape| shape.dimension() != ShapeDimension::Solid)
                {
                    return Err(EngineError::new(
                        "hull() currently expects only 3D child geometry.",
                    ));
                }
                self.cached_convex_hull(&shapes).map(Some)
            }
            "translate" => {
                let vector = vec3_argument(arguments, 0, environment, Vec3::default());
                let Some(vector) = self.geometry_argument("translate", vector) else {
                    return Ok(None);
                };
                if !vec3_is_finite(vector) {
                    self.ignore_geometry("translate", "vector components must be finite");
                    return Ok(None);
                }
                self.transform_children(children, environment, Transform::translation(vector))
            }
            "rotate" => {
                // OpenSCAD's signature is `rotate(a, v)`. `a` is either a
                // scalar angle or a vector of Euler angles; `v` is an optional
                // rotation axis that is only meaningful when `a` is a scalar.
                // Binding `a` by name matters: `rotate(a = [0, 0, 45])` is a
                // very common spelling and used to silently resolve to the
                // zero rotation.
                // A scalar and a one-element vector mean different things
                // (`rotate(45)` is about Z, `rotate([45])` is about X), so the
                // raw value is inspected rather than a flattened number list.
                let angle = match argument(arguments, "a", 0) {
                    None => Value::Undefined,
                    Some(expression) => evaluate_expression(expression, environment)?,
                };
                self.note_value(&angle);
                // An explicit `undef` axis reads as "no axis given", which is
                // what forwarding an unset module parameter produces:
                // `module m(a, v) { rotate(a, v) children(); } m(45);`.
                // Treating it as a type error would delete the children.
                let axis = match argument(arguments, "v", 1) {
                    Some(expression)
                        if matches!(
                            evaluate_expression(expression, environment)?,
                            Value::Undefined
                        ) =>
                    {
                        Some(Vec::new())
                    }
                    _ => self.geometry_argument(
                        "rotate",
                        argument_vector(arguments, "v", 1, environment, Vec::new()),
                    ),
                };
                let Some(axis) = axis else {
                    return Ok(None);
                };
                let axis = match axis.as_slice() {
                    [] => None,
                    [x, y] => Some(Vec3::new(*x, *y, 0.0)),
                    [x, y, z, ..] => Some(Vec3::new(*x, *y, *z)),
                    _ => {
                        self.ignore_geometry("rotate", "axis must be a 2- or 3-vector");
                        return Ok(None);
                    }
                };
                if axis.is_some_and(|axis| !vec3_is_finite(axis)) {
                    self.ignore_geometry("rotate", "axis components must be finite");
                    return Ok(None);
                }
                let transform = match (&angle, axis) {
                    (Value::Undefined, _) => Transform::rotation_xyz(Vec3::default()),
                    // `rotate(a, v)` is an axis-angle rotation. A degenerate
                    // axis names no rotation plane, so OpenSCAD leaves the
                    // children untouched rather than emitting NaNs.
                    (Value::Number(degrees), Some(axis)) => {
                        if !degrees.is_finite() {
                            self.ignore_geometry("rotate", "angle must be finite");
                            return Ok(None);
                        }
                        Transform::rotation_axis_angle(axis, *degrees)
                    }
                    // `rotate(a)` with a scalar and no axis spins about Z.
                    (Value::Number(degrees), None) => {
                        if !degrees.is_finite() {
                            self.ignore_geometry("rotate", "angle must be finite");
                            return Ok(None);
                        }
                        Transform::rotation_xyz(Vec3::new(0.0, 0.0, *degrees))
                    }
                    // A vector `a` is Euler XYZ with the missing trailing
                    // components taken as zero; any `v` alongside it is
                    // ignored, as in OpenSCAD.
                    (Value::Vector(components), _) => {
                        let mut degrees = [0.0f64; 3];
                        for (slot, component) in degrees.iter_mut().zip(components) {
                            let Some(value) = numeric_value(component) else {
                                self.ignore_geometry(
                                    "rotate",
                                    "angle components must be numbers",
                                );
                                return Ok(None);
                            };
                            *slot = value;
                        }
                        let degrees = Vec3::new(degrees[0], degrees[1], degrees[2]);
                        if !vec3_is_finite(degrees) {
                            self.ignore_geometry("rotate", "angle components must be finite");
                            return Ok(None);
                        }
                        Transform::rotation_xyz(degrees)
                    }
                    _ => {
                        self.ignore_geometry("rotate", "angle must be a number or vector");
                        return Ok(None);
                    }
                };
                self.transform_children(children, environment, transform)
            }
            "scale" => {
                let values =
                    argument_vector(arguments, "v", 0, environment, vec![1.0, 1.0, 1.0]);
                let Some(values) = self.geometry_argument("scale", values) else {
                    return Ok(None);
                };
                let vector = match values.as_slice() {
                    [one] => Vec3::new(*one, *one, *one),
                    [x, y, z, ..] => Vec3::new(*x, *y, *z),
                    _ => {
                        self.ignore_geometry("scale", "value must be a number or 3-vector");
                        return Ok(None);
                    }
                };
                if !vec3_is_finite(vector)
                    || vector.x == 0.0
                    || vector.y == 0.0
                    || vector.z == 0.0
                {
                    self.ignore_geometry("scale", "components must be finite and non-zero");
                    return Ok(None);
                }
                self.transform_children(children, environment, Transform::scale(vector))
            }
            "mirror" => {
                let vector = vec3_argument(
                    arguments,
                    0,
                    environment,
                    Vec3::new(1.0, 0.0, 0.0),
                );
                let Some(vector) = self.geometry_argument("mirror", vector) else {
                    return Ok(None);
                };
                if !vec3_is_finite(vector) {
                    self.ignore_geometry("mirror", "normal components must be finite");
                    return Ok(None);
                }
                let transform = Transform::mirror(vector);
                let Some(transform) = self.geometry_argument("mirror", transform) else {
                    return Ok(None);
                };
                self.transform_children(children, environment, transform)
            }
            "offset" => {
                // OpenSCAD's signature is `offset(r, delta, chamfer)`: the sole
                // positional argument is `r`, `delta` is named-only, and `delta`
                // wins when both are given. The two are not interchangeable —
                // `r` rounds convex corners into arcs of radius `r`, `delta`
                // pushes the walls out and mitres the corner where they meet —
                // so the choice has to reach the mesher, not just the distance.
                let radius = argument_number_optional(arguments, "r", 0, environment);
                let Some(radius) = self.geometry_argument("offset", radius) else {
                    return Ok(None);
                };
                let mitred =
                    argument_number_optional(arguments, "delta", usize::MAX, environment);
                let Some(mitred) = self.geometry_argument("offset", mitred) else {
                    return Ok(None);
                };
                let chamfer =
                    argument_bool(arguments, "chamfer", usize::MAX, environment, false);
                let Some(chamfer) = self.geometry_argument("offset", chamfer) else {
                    return Ok(None);
                };
                let (delta, rounded) = match (mitred, radius) {
                    (Some(delta), _) => (delta, false),
                    (None, Some(radius)) => (radius, true),
                    (None, None) => (1.0, true),
                };
                if !delta.is_finite() {
                    self.ignore_geometry("offset", "r/delta must be finite");
                    return Ok(None);
                }
                if chamfer {
                    self.diagnostics.push(
                        "WARNING: offset(chamfer = true) is mitred here; chamfered joins are not implemented."
                            .to_string(),
                    );
                }
                let fragments = fragment_count(delta.abs(), environment);
                let shapes = self.child_shapes(children, environment)?;
                self.require_planar_children("offset", &shapes)?;
                Ok(Shape::union(shapes).map(|shape| Shape::Offset2d {
                    shape: Box::new(shape),
                    delta,
                    rounded,
                    fragments,
                }))
            }
            "linear_extrude" => {
                let height = argument_number(arguments, "height", 0, environment, 1.0);
                let Some(height) = self.geometry_argument("linear_extrude", height) else {
                    return Ok(None);
                };
                if !height.is_finite() || height <= 0.0 {
                    self.ignore_geometry("linear_extrude", "height must be finite and positive");
                    return Ok(None);
                }
                let center = argument_bool(arguments, "center", 1, environment, false);
                let Some(center) = self.geometry_argument("linear_extrude", center) else {
                    return Ok(None);
                };
                // `convexity` controls legacy preview depth ordering only. This
                // engine directly meshes the final implicit solid, so accepting
                // it without changing geometry is intentional.
                if let Some(convexity) = argument(arguments, "convexity", 2) {
                    let value = evaluate_expression(convexity, environment)?;
                    self.note_value(&value);
                }
                let twist = argument_number(arguments, "twist", usize::MAX, environment, 0.0);
                let Some(twist) = self.geometry_argument("linear_extrude", twist) else {
                    return Ok(None);
                };
                let scale = argument_vector(arguments, "scale", usize::MAX, environment, vec![1.0]);
                let Some(scale) = self.geometry_argument("linear_extrude", scale) else {
                    return Ok(None);
                };
                let scale = match scale.as_slice() {
                    [] => [1.0, 1.0],
                    [uniform] => [*uniform, *uniform],
                    [x, y, ..] => [*x, *y],
                };
                if !twist.is_finite() || !scale[0].is_finite() || !scale[1].is_finite() {
                    self.ignore_geometry("linear_extrude", "twist and scale must be finite");
                    return Ok(None);
                }
                let requested_slices =
                    argument_number_optional(arguments, "slices", usize::MAX, environment);
                let Some(requested_slices) = self.geometry_argument("linear_extrude", requested_slices)
                else {
                    return Ok(None);
                };
                let shapes = self.child_shapes(children, environment)?;
                self.require_planar_children("linear_extrude", &shapes)?;
                if !self.exact && (twist != 0.0 || scale != [1.0, 1.0]) {
                    self.diagnostics.push(
                        "WARNING: linear_extrude() twist and scale are not represented by the \
                         sampled kernel; it extrudes the untwisted profile."
                            .to_string(),
                    );
                }
                Ok(Shape::union(shapes).map(|shape| {
                    // A twisted or tapered extrusion is a swept surface, and how
                    // finely it is sliced is a resolution choice like `$fn`.
                    // Left implicit, take one slice per facet the widest ring of
                    // the profile would get, pro-rated by how far it turns.
                    let bounds = shape.bounds();
                    let radius = bounds
                        .min
                        .x
                        .abs()
                        .max(bounds.max.x.abs())
                        .max(bounds.min.y.abs())
                        .max(bounds.max.y.abs());
                    let slices = match requested_slices {
                        Some(value) if value.is_finite() && value >= 1.0 => {
                            (value as usize).min(MAX_CURVE_FRAGMENTS)
                        }
                        _ if twist != 0.0 => {
                            let per_turn = fragment_count(radius, environment) as f64;
                            ((per_turn * twist.abs() / 360.0).ceil() as usize)
                                .clamp(1, MAX_CURVE_FRAGMENTS)
                        }
                        _ if scale != [1.0, 1.0] => 1,
                        _ => 1,
                    };
                    Shape::LinearExtrude {
                        shape: Box::new(shape),
                        height,
                        center,
                        twist,
                        scale,
                        slices,
                    }
                }))
            }
            "color" => {
                // `color()` never changes geometry, so an unusable argument is
                // a diagnostic rather than a reason to drop the children.
                let rgba = match argument(arguments, "c", 0) {
                    None => None,
                    Some(expression) => {
                        let value = evaluate_expression(expression, environment)?;
                        self.note_value(&value);
                        match parse_color_value(&value) {
                            Ok(rgba) => rgba,
                            Err(reason) => {
                                self.diagnostics
                                    .push(format!("WARNING: color() ignored: {reason}"));
                                None
                            }
                        }
                    }
                };
                let alpha = match argument_number_optional(arguments, "alpha", 1, environment) {
                    Ok(alpha) => alpha.filter(|alpha| alpha.is_finite()),
                    Err(error) => {
                        self.diagnostics
                            .push(format!("WARNING: color() alpha ignored: {error}"));
                        None
                    }
                };
                // `color(alpha = a)` with no color keeps the viewer's default
                // material and only changes its opacity. There is no default
                // to attach the alpha to here, so say so rather than drop it
                // silently.
                if alpha.is_some() && rgba.is_none() {
                    self.diagnostics.push(
                        "WARNING: color() alpha without a color is ignored; give a color too."
                            .into(),
                    );
                }
                let rgba = rgba.map(|mut rgba| {
                    if let Some(alpha) = alpha {
                        rgba[3] = alpha.clamp(0.0, 1.0);
                    }
                    rgba
                });
                let shape = Shape::union(self.child_shapes(children, environment)?);
                Ok(match (shape, rgba) {
                    (Some(shape), Some(rgba)) => Some(Shape::Color {
                        shape: Box::new(shape),
                        rgba,
                    }),
                    (shape, _) => shape,
                })
            }
            "assert" => {
                let condition = arguments
                    .first()
                    .ok_or_else(|| EngineError::new("assert() requires a condition."))?;
                let condition = evaluate_expression(&condition.value, environment)?;
                self.note_value(&condition);
                if !truthy(&condition) {
                    let detail = arguments
                        .get(1)
                        .map(|argument| evaluate_expression(&argument.value, environment))
                        .transpose()?
                        .map(|value| format!(" {}", display_value(&value)))
                        .unwrap_or_default();
                    return Err(EngineError::new(format!("Assertion failed.{detail}")));
                }
                Ok(Shape::union(self.child_shapes(children, environment)?))
            }
            "echo" => {
                let mut values = Vec::with_capacity(arguments.len());
                for argument in arguments {
                    let value = evaluate_expression(&argument.value, environment)?;
                    self.note_value(&value);
                    let value = display_value(&value);
                    values.push(match &argument.name {
                        Some(name) => format!("{name} = {value}"),
                        None => value,
                    });
                }
                self.diagnostics
                    .push(format!("ECHO: {}", values.join(", ")));
                Ok(Shape::union(self.child_shapes(children, environment)?))
            }
            "children" => self.selected_children(arguments, environment),
            module_name if self.modules.contains_key(module_name) => {
                if self.module_depth >= MAX_MODULE_RECURSION_DEPTH {
                    return Err(EngineError::new(format!(
                        "Module recursion depth exceeded the limit of {MAX_MODULE_RECURSION_DEPTH}."
                    )));
                }
                let module = self.modules.get(module_name).cloned().unwrap();
                let mut local = Environment::layered_on(&module.environment);
                for (name, value) in environment.special_variables() {
                    local.insert(name.clone(), value.clone());
                }
                local.insert("$children".into(), Value::Number(children.len() as f64));

                let mut bound = vec![None; module.parameters.len()];
                let mut positional_index = 0;
                for argument in arguments {
                    match argument.name.as_deref() {
                        None => {
                            let value = evaluate_expression(&argument.value, environment)?;
                            if let Some(slot) = bound.get_mut(positional_index) {
                                *slot = Some(value);
                            } else {
                                self.diagnostics.push(format!(
                                    "WARNING: Module {module_name}() received too many positional arguments."
                                ));
                            }
                            positional_index += 1;
                        }
                        Some(name) if name.starts_with('$') => {}
                        Some(name) => {
                            let value = evaluate_expression(&argument.value, environment)?;
                            if let Some(index) = module
                                .parameters
                                .iter()
                                .position(|(parameter, _)| parameter == name)
                            {
                                bound[index] = Some(value);
                            } else {
                                self.diagnostics.push(format!(
                                    "WARNING: Module {module_name}() has no parameter named {name}; ignoring it."
                                ));
                            }
                        }
                    }
                }
                for ((parameter, default), value) in module.parameters.iter().zip(bound) {
                    let value = match value {
                        Some(value) => value,
                        None => match default {
                            Some(default) => evaluate_expression(default, environment)?,
                            None => Value::Undefined,
                        },
                    };
                    self.note_value(&value);
                    local.insert(parameter.clone(), value);
                }

                self.module_depth += 1;
                self.child_contexts.push(ChildContext {
                    statements: children.to_vec(),
                    environment: environment.clone(),
                });
                let caller_modules = Rc::clone(&self.modules);
                install_scope_modules(
                    &mut self.modules,
                    &module.outer_modules,
                    &module.same_scope_modules,
                );
                let result = self.statements(&module.body, &local, false);
                self.modules = caller_modules;
                self.child_contexts.pop();
                self.module_depth -= 1;
                result.map(Shape::union)
            }
            unsupported => Err(EngineError::new(format!(
                "Unsupported module {unsupported}(). The native engine currently supports square, circle, polygon, cube, sphere, cylinder, transforms, modules, loops, union, difference, and intersection."
            ))),
        }
    }

    fn selected_children(
        &mut self,
        arguments: &[Argument],
        environment: &Environment,
    ) -> Result<Option<Shape>, EngineError> {
        let context = self
            .child_contexts
            .last()
            .cloned()
            .ok_or_else(|| EngineError::new("children() may only be used inside a module."))?;
        let indices = if arguments.is_empty() {
            (0..context.statements.len()).collect()
        } else if arguments.len() == 1 && arguments[0].name.is_none() {
            let selector = evaluate_expression(&arguments[0].value, environment)?;
            self.child_indices(selector)?
        } else {
            return Err(EngineError::new(
                "children() expects zero arguments or one index selector.",
            ));
        };

        let mut selected = Vec::new();
        for index in indices {
            if let Some(statement) = context.statements.get(index) {
                selected.push(statement.clone());
            } else {
                self.diagnostics.push(format!(
                    "WARNING: children() index {index} is out of range for {} children; ignoring it.",
                    context.statements.len()
                ));
            }
        }

        let mut child_environment = context.environment;
        for (name, value) in environment.special_variables() {
            child_environment.insert(name.clone(), value.clone());
        }
        // The selected statements belong to the caller. Temporarily remove the
        // callee's child context so a forwarded `children()` resolves against
        // the caller's context instead of recursively selecting itself.
        let active_context = self
            .child_contexts
            .pop()
            .expect("selected_children requires an active child context");
        let result = self.statements(&selected, &child_environment, false);
        self.child_contexts.push(active_context);
        result.map(Shape::union)
    }

    fn child_indices(&mut self, selector: Value) -> Result<Vec<usize>, EngineError> {
        let values = match selector {
            Value::Number(value) => vec![Value::Number(value)],
            Value::Vector(values) => values,
            Value::Range { start, step, end } => {
                Self::iterable_values(Value::Range { start, step, end })?
            }
            _ => {
                return Err(EngineError::new(
                    "children() selector must be a number, vector, or range.",
                ));
            }
        };
        values
            .into_iter()
            .map(|value| match value {
                Value::Number(index)
                    if index.is_finite() && index >= 0.0 && index.fract() == 0.0 =>
                {
                    Ok(index as usize)
                }
                _ => Err(EngineError::new(
                    "children() indices must be non-negative whole numbers.",
                )),
            })
            .collect()
    }

    fn geometry_argument<T>(&mut self, module: &str, value: Result<T, EngineError>) -> Option<T> {
        match value {
            Ok(value) => Some(value),
            Err(error) => {
                self.ignore_geometry(module, &error.to_string());
                None
            }
        }
    }

    fn require_same_dimension(&self, module: &str, shapes: &[Shape]) -> Result<(), EngineError> {
        if shapes.first().is_some_and(|first| {
            shapes
                .iter()
                .any(|shape| shape.dimension() != first.dimension())
        }) {
            Err(EngineError::new(format!(
                "{module}() cannot mix 2D and 3D geometry."
            )))
        } else {
            Ok(())
        }
    }

    fn require_planar_children(&self, module: &str, shapes: &[Shape]) -> Result<(), EngineError> {
        self.require_same_dimension(module, shapes)?;
        if shapes
            .iter()
            .any(|shape| shape.dimension() != ShapeDimension::Planar)
        {
            Err(EngineError::new(format!(
                "{module}() expects only 2D child geometry."
            )))
        } else {
            Ok(())
        }
    }

    fn cached_convex_hull(&mut self, shapes: &[Shape]) -> Result<Shape, EngineError> {
        let points = Shape::hull_vertices(shapes)?;
        let key = points
            .iter()
            .map(|point| [point.x.to_bits(), point.y.to_bits(), point.z.to_bits()])
            .collect::<Vec<_>>();
        if let Some(shape) = self.hull_cache.get(&key) {
            return Ok(shape.clone());
        }
        let shape = Shape::convex_hull_from_points(&points)?;
        if self.hull_cache.len() < MAX_HULL_CACHE_ENTRIES {
            self.hull_cache.insert(key, shape.clone());
        }
        Ok(shape)
    }

    fn ignore_geometry(&mut self, module: &str, reason: &str) {
        self.diagnostics.push(format!(
            "WARNING: Ignoring invalid {module}() geometry: {reason}."
        ));
    }

    fn note_value(&mut self, value: &Value) {
        if value.has_non_finite_number()
            && !self
                .diagnostics
                .iter()
                .any(|message| message.contains("Non-finite arithmetic result"))
        {
            self.diagnostics.push(
                "WARNING: Non-finite arithmetic result (inf or nan); geometry using it may be ignored."
                    .into(),
            );
        }
    }

    fn child_shapes(
        &mut self,
        children: &[Stmt],
        environment: &Environment,
    ) -> Result<Vec<Shape>, EngineError> {
        self.statements(children, environment, false)
    }

    fn transform_children(
        &mut self,
        children: &[Stmt],
        environment: &Environment,
        transform: Transform,
    ) -> Result<Option<Shape>, EngineError> {
        Ok(
            Shape::union(self.child_shapes(children, environment)?).map(|shape| Shape::Transform {
                shape: Box::new(shape),
                transform,
            }),
        )
    }
}

fn display_value(value: &Value) -> String {
    match value {
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::String(value) => format!("{value:?}"),
        Value::Vector(values) => format!(
            "[{}]",
            values
                .iter()
                .map(display_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Range { start, step, end } => format!("[{start}:{step}:{end}]"),
        Value::Function(_) => "<function>".into(),
        Value::Undefined => "undef".into(),
    }
}

fn evaluate_expression(expression: &Expr, environment: &Environment) -> Result<Value, EngineError> {
    match expression {
        Expr::Number(value) => Ok(Value::Number(*value)),
        Expr::Bool(value) => Ok(Value::Bool(*value)),
        Expr::String(value) => Ok(Value::String(value.clone())),
        Expr::Vector(values) => values
            .iter()
            .map(|value| evaluate_expression(value, environment))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Vector),
        Expr::Range(start, step, end) => {
            let start = evaluate_expression(start, environment)?;
            let step = evaluate_expression(step, environment)?;
            let end = evaluate_expression(end, environment)?;
            let (Value::Number(start), Value::Number(step), Value::Number(end)) =
                (start, step, end)
            else {
                expression_warning("Range endpoints and step must be numbers; returning undef.");
                return Ok(Value::Undefined);
            };
            if step > 0.0 && start > end {
                expression_warning(
                    "Deprecated reversed range with a positive step; treating it as empty.",
                );
            }
            Ok(Value::Range { start, step, end })
        }
        Expr::Variable(name) => Ok(match name.as_str() {
            "undef" => Value::Undefined,
            "PI" => Value::Number(std::f64::consts::PI),
            _ => environment.get(name).cloned().unwrap_or(Value::Undefined),
        }),
        Expr::Unary(operator, value) => {
            let value = evaluate_expression(value, environment)?;
            match operator {
                UnaryOp::Negate if matches!(value, Value::Undefined) => {
                    expression_warning("Unary minus received undef; returning undef.");
                    Ok(Value::Undefined)
                }
                UnaryOp::Negate => Ok(negate_value(value)),
                UnaryOp::Not => Ok(Value::Bool(!truthy(&value))),
            }
        }
        Expr::Binary(left, operator, right) => {
            let left = evaluate_expression(left, environment)?;
            if matches!(operator, BinaryOp::And) && !truthy(&left) {
                return Ok(Value::Bool(false));
            }
            if matches!(operator, BinaryOp::Or) && truthy(&left) {
                return Ok(Value::Bool(true));
            }
            let right = evaluate_expression(right, environment)?;
            match operator {
                BinaryOp::Or | BinaryOp::And => Ok(Value::Bool(truthy(&right))),
                BinaryOp::Equal => Ok(Value::Bool(left == right)),
                BinaryOp::NotEqual => Ok(Value::Bool(left != right)),
                BinaryOp::Less
                | BinaryOp::LessEqual
                | BinaryOp::Greater
                | BinaryOp::GreaterEqual => {
                    let (supported, result) = compare_values(&left, &right, *operator);
                    if !supported {
                        expression_warning(
                            "Unsupported operands for ordering comparison; returning false.",
                        );
                    }
                    Ok(Value::Bool(result))
                }
                BinaryOp::Add
                | BinaryOp::Subtract
                | BinaryOp::Multiply
                | BinaryOp::Divide
                | BinaryOp::Modulo
                | BinaryOp::Power => {
                    if matches!(left, Value::Undefined) || matches!(right, Value::Undefined) {
                        expression_warning("Arithmetic received undef; returning undef.");
                        return Ok(Value::Undefined);
                    }
                    Ok(arithmetic_values(left, right, *operator))
                }
            }
        }
        Expr::Conditional(condition, then_value, else_value) => {
            if truthy(&evaluate_expression(condition, environment)?) {
                evaluate_expression(then_value, environment)
            } else {
                evaluate_expression(else_value, environment)
            }
        }
        Expr::Index(value, index) => {
            let value = evaluate_expression(value, environment)?;
            let index = evaluate_expression(index, environment)?;
            let Value::Number(index) = index else {
                return Ok(Value::Undefined);
            };
            if !index.is_finite() || index < 0.0 || index.fract() != 0.0 {
                return Ok(Value::Undefined);
            }
            let index = index as usize;
            Ok(match value {
                Value::Vector(values) => values.get(index).cloned().unwrap_or(Value::Undefined),
                Value::String(value) => value
                    .chars()
                    .nth(index)
                    .map(|character| Value::String(character.to_string()))
                    .unwrap_or(Value::Undefined),
                _ => Value::Undefined,
            })
        }
        Expr::Member(value, member) => {
            let value = evaluate_expression(value, environment)?;
            let index = match member.as_str() {
                "x" => Some(0),
                "y" => Some(1),
                "z" => Some(2),
                _ => None,
            };
            Ok(match (value, index) {
                (Value::Vector(values), Some(index)) => {
                    values.get(index).cloned().unwrap_or(Value::Undefined)
                }
                _ => Value::Undefined,
            })
        }
        Expr::Let(bindings, value) => {
            let mut local = environment.clone();
            for (name, binding) in bindings {
                let binding = evaluate_expression(binding, &local)?;
                local.insert(name.clone(), binding);
            }
            evaluate_expression(value, &local)
        }
        Expr::Assert(condition, message, value) => {
            if truthy(&evaluate_expression(condition, environment)?) {
                evaluate_expression(value, environment)
            } else {
                let detail = message
                    .as_deref()
                    .map(|message| evaluate_expression(message, environment))
                    .transpose()?
                    .map(|message| format!(" {}", display_value(&message)))
                    .unwrap_or_default();
                Err(EngineError::new(format!("Assertion failed.{detail}")))
            }
        }
        Expr::Echo(arguments, value) => {
            let arguments = evaluate_call_arguments(arguments, environment)?;
            let values = arguments
                .into_iter()
                .map(|argument| match argument.name {
                    Some(name) => format!("{name} = {}", display_value(&argument.value)),
                    None => display_value(&argument.value),
                })
                .collect::<Vec<_>>();
            expression_message(format!("ECHO: {}", values.join(", ")));
            evaluate_expression(value, environment)
        }
        Expr::Call(callee, arguments) => {
            let arguments = evaluate_call_arguments(arguments, environment)?;
            if let Expr::Variable(name) = callee.as_ref() {
                if let Some(Value::Function(function)) = environment.get(name) {
                    return evaluate_user_function(function.clone(), arguments, environment);
                }
                let values = match bind_builtin_arguments(name, arguments) {
                    Ok(values) => values,
                    Err(warning) => {
                        expression_warning(warning);
                        return Ok(Value::Undefined);
                    }
                };
                return match builtins::call(name, &values) {
                    Some(Ok(value)) => Ok(value),
                    Some(Err(warning)) => {
                        expression_warning(warning);
                        Ok(Value::Undefined)
                    }
                    None => {
                        expression_warning(format!("Unsupported function {name}()."));
                        Ok(Value::Undefined)
                    }
                };
            }
            match evaluate_expression(callee, environment)? {
                Value::Function(function) => {
                    evaluate_user_function(function, arguments, environment)
                }
                _ => {
                    expression_warning("Attempted to call a non-function value; returning undef.");
                    Ok(Value::Undefined)
                }
            }
        }
        Expr::Lambda(parameters, body) => Ok(Value::Function(Rc::new(FunctionValue {
            name: None,
            parameters: parameters.clone(),
            body: body.as_ref().clone(),
            environment: Rc::new(environment.flattened()),
            same_scope_functions: Rc::new(HashMap::new()),
        }))),
        Expr::Comprehension(elements) => {
            let mut values = Vec::new();
            let mut budget = ComprehensionBudget::default();
            for element in elements {
                evaluate_list_element(element, environment, &mut budget, &mut values)?;
            }
            Ok(Value::Vector(values))
        }
    }
}

const MAX_COMPREHENSION_STEPS: usize = 100_000;

#[derive(Default)]
struct ComprehensionBudget {
    steps: usize,
}

impl ComprehensionBudget {
    fn consume(&mut self) -> Result<(), EngineError> {
        self.consume_amount(1)
    }

    fn consume_value(&mut self, value: &Value) -> Result<(), EngineError> {
        self.consume_amount(value_work_cost(value))
    }

    fn consume_amount(&mut self, amount: usize) -> Result<(), EngineError> {
        self.steps = self.steps.saturating_add(amount);
        if self.steps > MAX_COMPREHENSION_STEPS {
            Err(EngineError::new(format!(
                "List comprehension exceeded the {MAX_COMPREHENSION_STEPS} step limit."
            )))
        } else {
            Ok(())
        }
    }
}

fn value_work_cost(value: &Value) -> usize {
    match value {
        Value::Vector(values) => values.iter().fold(1usize, |cost, value| {
            cost.saturating_add(value_work_cost(value))
        }),
        _ => 1,
    }
}

fn evaluate_list_element(
    element: &ListElement,
    environment: &Environment,
    budget: &mut ComprehensionBudget,
    output: &mut Vec<Value>,
) -> Result<(), EngineError> {
    budget.consume()?;
    match element {
        ListElement::Value(value) => {
            let value = evaluate_expression(value, environment)?;
            budget.consume_value(&value)?;
            output.push(value);
        }
        ListElement::Each(value) => {
            let values = match evaluate_expression(value, environment)? {
                Value::Vector(values) => values,
                Value::Range { start, step, end } => {
                    Evaluator::iterable_values(Value::Range { start, step, end })?
                }
                value => vec![value],
            };
            for value in values {
                budget.consume_value(&value)?;
                output.push(value);
            }
        }
        ListElement::For { bindings, body } => {
            evaluate_comprehension_bindings(bindings, 0, body, environment, budget, output)?;
        }
        ListElement::CStyleFor {
            initial,
            condition,
            updates,
            body,
        } => {
            let mut local = environment.clone();
            for (name, value) in initial {
                let value = evaluate_expression(value, &local)?;
                local.insert(name.clone(), value);
            }
            loop {
                budget.consume()?;
                if !truthy(&evaluate_expression(condition, &local)?) {
                    break;
                }
                evaluate_list_element(body, &local, budget, output)?;
                let next = updates
                    .iter()
                    .map(|(name, value)| Ok((name.clone(), evaluate_expression(value, &local)?)))
                    .collect::<Result<Vec<_>, EngineError>>()?;
                local.extend(next);
            }
        }
        ListElement::If {
            condition,
            then_element,
            else_element,
        } => {
            if truthy(&evaluate_expression(condition, environment)?) {
                evaluate_list_element(then_element, environment, budget, output)?;
            } else if let Some(else_element) = else_element {
                evaluate_list_element(else_element, environment, budget, output)?;
            }
        }
        ListElement::Let { bindings, body } => {
            let mut local = environment.clone();
            for (name, value) in bindings {
                let value = evaluate_expression(value, &local)?;
                local.insert(name.clone(), value);
            }
            evaluate_list_element(body, &local, budget, output)?;
        }
    }
    Ok(())
}

fn evaluate_comprehension_bindings(
    bindings: &[(String, Expr)],
    binding_index: usize,
    body: &ListElement,
    environment: &Environment,
    budget: &mut ComprehensionBudget,
    output: &mut Vec<Value>,
) -> Result<(), EngineError> {
    if binding_index == bindings.len() {
        return evaluate_list_element(body, environment, budget, output);
    }
    let (name, iterable) = &bindings[binding_index];
    let values = Evaluator::iterable_values(evaluate_expression(iterable, environment)?)?;
    // As in `for_iteration_shapes`, only the loop variable differs between
    // iterations and nothing downstream writes to `local`.
    let mut local = environment.clone();
    for value in values {
        budget.consume()?;
        local.insert(name.clone(), value);
        evaluate_comprehension_bindings(bindings, binding_index + 1, body, &local, budget, output)?;
    }
    Ok(())
}

#[derive(Clone)]
struct EvaluatedArgument {
    name: Option<String>,
    value: Value,
}

fn evaluate_call_arguments(
    arguments: &[Argument],
    environment: &Environment,
) -> Result<Vec<EvaluatedArgument>, EngineError> {
    arguments
        .iter()
        .map(|argument| {
            Ok(EvaluatedArgument {
                name: argument.name.clone(),
                value: evaluate_expression(&argument.value, environment)?,
            })
        })
        .collect()
}

fn bind_builtin_arguments(
    name: &str,
    arguments: Vec<EvaluatedArgument>,
) -> Result<Vec<Value>, String> {
    if arguments.iter().all(|argument| argument.name.is_none()) {
        return Ok(arguments
            .into_iter()
            .map(|argument| argument.value)
            .collect());
    }

    let parameter_names: &[&str] = match name {
        "rands" => &["min", "max", "count", "seed"],
        "search" => &["match", "target", "num_returns_per_match", "index_col_num"],
        _ => {
            return Err(format!(
                "Built-in function {name}() does not accept named arguments."
            ));
        }
    };
    let mut bound = vec![None; parameter_names.len()];
    let mut next_positional = 0;
    for argument in arguments {
        let index = if let Some(argument_name) = argument.name.as_deref() {
            parameter_names
                .iter()
                .position(|parameter| *parameter == argument_name)
                .ok_or_else(|| format!("{name}(): unknown named argument {argument_name}."))?
        } else {
            if next_positional >= parameter_names.len() {
                return Err(format!("{name}(): too many positional arguments."));
            }
            let index = next_positional;
            next_positional += 1;
            index
        };
        if bound[index].replace(argument.value).is_some() {
            return Err(format!(
                "{name}(): argument {} was supplied more than once.",
                parameter_names[index]
            ));
        }
    }

    if bound[0].is_none() || bound[1].is_none() || (name == "rands" && bound[2].is_none()) {
        return Err(format!("{name}(): missing required argument."));
    }
    if name == "search" && bound[3].is_some() && bound[2].is_none() {
        bound[2] = Some(Value::Number(1.0));
    }

    let final_index = bound
        .iter()
        .rposition(Option::is_some)
        .expect("required built-in arguments are present");
    Ok(bound
        .into_iter()
        .take(final_index + 1)
        .map(|value| value.expect("optional argument gaps have defaults"))
        .collect())
}

const MAX_FUNCTION_RECURSION_DEPTH: usize = 10_000;
const MAX_NON_TAIL_FUNCTION_DEPTH: usize = 256;

struct FunctionCallGuard;

impl Drop for FunctionCallGuard {
    fn drop(&mut self) {
        FUNCTION_CALL_DEPTH.with(|depth| *depth.borrow_mut() -= 1);
    }
}

fn enter_function_call() -> Result<FunctionCallGuard, EngineError> {
    FUNCTION_CALL_DEPTH.with(|depth| {
        let mut depth = depth.borrow_mut();
        if *depth >= MAX_NON_TAIL_FUNCTION_DEPTH {
            Err(EngineError::new(format!(
                "Function recursion exceeded the {MAX_NON_TAIL_FUNCTION_DEPTH} non-tail call limit."
            )))
        } else {
            *depth += 1;
            Ok(FunctionCallGuard)
        }
    })
}

enum FunctionStep {
    Return(Value),
    TailCall(Vec<EvaluatedArgument>),
}

fn evaluate_user_function(
    function: Rc<FunctionValue>,
    mut arguments: Vec<EvaluatedArgument>,
    caller_environment: &Environment,
) -> Result<Value, EngineError> {
    let _call_guard = enter_function_call()?;
    // Only a self tail call needs its caller's environment to outlive the
    // frame that made it, and the environment it then needs is the callee's
    // own — which this loop already owns. Copying the caller's map up front
    // charged every ordinary call for a case almost none of them reach, and a
    // SCAD program evaluates far more function calls than anything else.
    let mut tail_environment: Option<Environment> = None;
    for _ in 0..MAX_FUNCTION_RECURSION_DEPTH {
        let local = {
            let caller = tail_environment.as_ref().unwrap_or(caller_environment);
            bind_function_arguments(&function, &arguments, caller)?
        };
        match evaluate_function_tail(&function.body, &function, &local)? {
            FunctionStep::Return(value) => return Ok(value),
            FunctionStep::TailCall(next_arguments) => {
                arguments = next_arguments;
                tail_environment = Some(local);
            }
        }
    }
    Err(EngineError::new(format!(
        "Function recursion exceeded the {MAX_FUNCTION_RECURSION_DEPTH} call limit."
    )))
}

fn bind_function_arguments(
    function: &Rc<FunctionValue>,
    arguments: &[EvaluatedArgument],
    caller_environment: &Environment,
) -> Result<Environment, EngineError> {
    let mut local = Environment::layered_on(&function.environment);
    for (name, definition) in function.same_scope_functions.iter() {
        local.insert(
            name.clone(),
            Value::Function(Rc::new(FunctionValue {
                name: Some(name.clone()),
                parameters: definition.parameters.clone(),
                body: definition.body.clone(),
                environment: Rc::clone(&function.environment),
                same_scope_functions: function.same_scope_functions.clone(),
            })),
        );
    }
    if let Some(name) = &function.name {
        local.insert(name.clone(), Value::Function(function.clone()));
    }

    // Ordinary values are lexically captured; special variables deliberately
    // follow the dynamic call chain and per-call named overrides win last.
    for (name, value) in caller_environment.special_variables() {
        local.insert(name.clone(), value.clone());
    }

    let mut bound = vec![None; function.parameters.len()];
    let mut positional_index = 0;
    for argument in arguments {
        match argument.name.as_deref() {
            None => {
                if let Some(slot) = bound.get_mut(positional_index) {
                    *slot = Some(argument.value.clone());
                } else {
                    expression_warning("Function call has too many positional arguments.");
                }
                positional_index += 1;
            }
            Some(name) if name.starts_with('$') => {
                local.insert(name.to_owned(), argument.value.clone());
            }
            Some(name) => {
                if let Some(index) = function
                    .parameters
                    .iter()
                    .position(|(parameter, _)| parameter == name)
                {
                    bound[index] = Some(argument.value.clone());
                } else {
                    expression_warning(format!(
                        "Function call has no parameter named {name}; ignoring it."
                    ));
                }
            }
        }
    }
    for ((parameter, default), value) in function.parameters.iter().zip(bound) {
        let value = match value {
            Some(value) => value,
            None => match default {
                Some(default) => evaluate_expression(default, caller_environment)?,
                None => Value::Undefined,
            },
        };
        local.insert(parameter.clone(), value);
    }
    Ok(local)
}

fn evaluate_function_tail(
    expression: &Expr,
    function: &Rc<FunctionValue>,
    environment: &Environment,
) -> Result<FunctionStep, EngineError> {
    match expression {
        Expr::Conditional(condition, then_value, else_value) => {
            let selected = if truthy(&evaluate_expression(condition, environment)?) {
                then_value
            } else {
                else_value
            };
            evaluate_function_tail(selected, function, environment)
        }
        Expr::Call(callee, arguments)
            if matches!(callee.as_ref(), Expr::Variable(name)
            if function.name.as_deref().is_some_and(|own_name| own_name == name)
                && matches!(
                    environment.get(name),
                    Some(Value::Function(bound)) if Rc::ptr_eq(bound, function)
                )) =>
        {
            Ok(FunctionStep::TailCall(evaluate_call_arguments(
                arguments,
                environment,
            )?))
        }
        _ => Ok(FunctionStep::Return(evaluate_expression(
            expression,
            environment,
        )?)),
    }
}

fn negate_value(value: Value) -> Value {
    match value {
        Value::Number(value) => Value::Number(-value),
        Value::Vector(values) => {
            let values = values.into_iter().map(negate_value).collect::<Vec<_>>();
            if values.iter().any(|value| matches!(value, Value::Undefined)) {
                expression_warning(
                    "Unsupported vector component for unary minus; returning undef.",
                );
                Value::Undefined
            } else {
                Value::Vector(values)
            }
        }
        Value::Undefined => Value::Undefined,
        _ => {
            expression_warning("Unsupported operand for unary minus; returning undef.");
            Value::Undefined
        }
    }
}

fn arithmetic_values(left: Value, right: Value, operator: BinaryOp) -> Value {
    let result = match (left, right, operator) {
        (Value::Number(left), Value::Number(right), operator) => {
            Some(Value::Number(match operator {
                BinaryOp::Add => left + right,
                BinaryOp::Subtract => left - right,
                BinaryOp::Multiply => left * right,
                BinaryOp::Divide => left / right,
                BinaryOp::Modulo => left % right,
                BinaryOp::Power => left.powf(right),
                _ => unreachable!(),
            }))
        }
        (Value::Vector(left), Value::Vector(right), BinaryOp::Add)
        | (Value::Vector(left), Value::Vector(right), BinaryOp::Subtract) => {
            if left.len() != right.len() {
                None
            } else {
                let values = left
                    .into_iter()
                    .zip(right)
                    .map(|(left, right)| arithmetic_values(left, right, operator))
                    .collect::<Vec<_>>();
                (!values.iter().any(|value| matches!(value, Value::Undefined)))
                    .then_some(Value::Vector(values))
            }
        }
        (Value::Number(scalar), Value::Vector(values), BinaryOp::Multiply) => {
            scale_vector(values, scalar, false)
        }
        (Value::Vector(values), Value::Number(scalar), BinaryOp::Multiply) => {
            scale_vector(values, scalar, false)
        }
        (Value::Vector(values), Value::Number(scalar), BinaryOp::Divide) => {
            scale_vector(values, scalar, true)
        }
        (Value::Vector(left), Value::Vector(right), BinaryOp::Multiply) => {
            multiply_vectors(left, right)
        }
        _ => None,
    };
    result.unwrap_or_else(|| {
        expression_warning("Unsupported arithmetic operands; returning undef.");
        Value::Undefined
    })
}

fn scale_vector(values: Vec<Value>, scalar: f64, divide: bool) -> Option<Value> {
    values
        .into_iter()
        .map(|value| {
            Some(match value {
                Value::Number(value) => Value::Number(if divide {
                    value / scalar
                } else {
                    value * scalar
                }),
                Value::Vector(values) => scale_vector(values, scalar, divide)?,
                _ => return None,
            })
        })
        .collect::<Option<Vec<_>>>()
        .map(Value::Vector)
}

fn multiply_vectors(left: Vec<Value>, right: Vec<Value>) -> Option<Value> {
    match (number_vector(&left), number_vector(&right)) {
        (Some(left), Some(right)) => dot_product(&left, &right).map(Value::Number),
        (None, Some(right)) => {
            let left = number_matrix(&left)?;
            left.iter()
                .map(|row| dot_product(row, &right).map(Value::Number))
                .collect::<Option<Vec<_>>>()
                .map(Value::Vector)
        }
        (Some(left), None) => {
            let right = number_matrix(&right)?;
            let columns = right.first().map_or(0, Vec::len);
            if left.len() != right.len() {
                return None;
            }
            let values = (0..columns)
                .map(|column| {
                    Some(Value::Number(
                        left.iter()
                            .zip(&right)
                            .map(|(left, row)| left * row[column])
                            .sum(),
                    ))
                })
                .collect::<Option<Vec<_>>>()?;
            Some(Value::Vector(values))
        }
        (None, None) => {
            let left = number_matrix(&left)?;
            let right = number_matrix(&right)?;
            let inner = left.first().map_or(0, Vec::len);
            if inner != right.len() {
                return None;
            }
            let columns = right.first().map_or(0, Vec::len);
            let rows = left
                .iter()
                .map(|left_row| {
                    (0..columns)
                        .map(|column| {
                            Value::Number(
                                left_row
                                    .iter()
                                    .zip(&right)
                                    .map(|(left, right_row)| left * right_row[column])
                                    .sum(),
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .map(Value::Vector)
                .collect::<Vec<_>>();
            Some(Value::Vector(rows))
        }
    }
}

fn number_vector(values: &[Value]) -> Option<Vec<f64>> {
    values.iter().map(numeric_value).collect()
}

fn number_matrix(values: &[Value]) -> Option<Vec<Vec<f64>>> {
    let rows = values
        .iter()
        .map(|value| match value {
            Value::Vector(row) => number_vector(row),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let columns = rows.first().map_or(0, Vec::len);
    rows.iter().all(|row| row.len() == columns).then_some(rows)
}

fn dot_product(left: &[f64], right: &[f64]) -> Option<f64> {
    (left.len() == right.len()).then(|| left.iter().zip(right).map(|(a, b)| a * b).sum())
}

fn numeric_value(value: &Value) -> Option<f64> {
    match value {
        Value::Number(value) => Some(*value),
        _ => None,
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Bool(value) => *value,
        Value::Number(value) => *value != 0.0,
        Value::String(value) => !value.is_empty(),
        Value::Vector(value) => !value.is_empty(),
        Value::Range { .. } => true,
        Value::Function(_) => true,
        Value::Undefined => false,
    }
}

fn compare_values(left: &Value, right: &Value, operator: BinaryOp) -> (bool, bool) {
    let (supported, ordering) = match (left, right) {
        (Value::Number(left), Value::Number(right)) => (true, left.partial_cmp(right)),
        (Value::String(left), Value::String(right)) => (true, Some(left.cmp(right))),
        (Value::Bool(left), Value::Bool(right)) => (true, Some(left.cmp(right))),
        // OpenSCAD coerces booleans to 0/1 only for mixed ordering comparisons;
        // equality remains type-sensitive, so true != 1.
        (Value::Bool(left), Value::Number(right)) => {
            (true, (*left as u8 as f64).partial_cmp(right))
        }
        (Value::Number(left), Value::Bool(right)) => {
            (true, left.partial_cmp(&(*right as u8 as f64)))
        }
        _ => (false, None),
    };
    let result = match (operator, ordering) {
        (BinaryOp::Less, Some(ordering)) => ordering.is_lt(),
        (BinaryOp::LessEqual, Some(ordering)) => ordering.is_le(),
        (BinaryOp::Greater, Some(ordering)) => ordering.is_gt(),
        (BinaryOp::GreaterEqual, Some(ordering)) => ordering.is_ge(),
        _ => false,
    };
    (supported, result)
}

fn number(value: Value, context: &str) -> Result<f64, EngineError> {
    match value {
        Value::Number(value) => Ok(value),
        Value::String(value) => Err(EngineError::new(format!(
            "Expected a number for {context}, found string {value:?}."
        ))),
        _ => Err(EngineError::new(format!(
            "Expected a number for {context}."
        ))),
    }
}

fn argument<'a>(arguments: &'a [Argument], name: &str, index: usize) -> Option<&'a Expr> {
    arguments
        .iter()
        .find(|argument| argument.name.as_deref() == Some(name))
        .map(|argument| &argument.value)
        .or_else(|| {
            arguments
                .iter()
                .filter(|argument| argument.name.is_none())
                .nth(index)
                .map(|argument| &argument.value)
        })
}

fn named_argument<'a>(arguments: &'a [Argument], name: &str) -> Option<&'a Expr> {
    arguments
        .iter()
        .find(|argument| argument.name.as_deref() == Some(name))
        .map(|argument| &argument.value)
}

fn positional_argument(arguments: &[Argument], index: usize) -> Option<&Expr> {
    arguments
        .iter()
        .filter(|argument| argument.name.is_none())
        .nth(index)
        .map(|argument| &argument.value)
}

fn evaluated_number(
    expression: Option<&Expr>,
    environment: &Environment,
) -> Result<Option<f64>, EngineError> {
    expression
        .map(|expression| number(evaluate_expression(expression, environment)?, "radius"))
        .transpose()
}

fn radius_or_diameter_argument(
    arguments: &[Argument],
    positional_index: usize,
    environment: &Environment,
    default: f64,
) -> Result<f64, EngineError> {
    if let Some(radius) = evaluated_number(named_argument(arguments, "r"), environment)? {
        return Ok(radius);
    }
    if let Some(diameter) = evaluated_number(named_argument(arguments, "d"), environment)? {
        return Ok(diameter / 2.0);
    }
    Ok(evaluated_number(
        positional_argument(arguments, positional_index),
        environment,
    )?
    .unwrap_or(default))
}

fn cylinder_radii(
    arguments: &[Argument],
    environment: &Environment,
) -> Result<(f64, f64), EngineError> {
    let second_positional = evaluated_number(positional_argument(arguments, 2), environment)?;
    let first_positional = evaluated_number(positional_argument(arguments, 1), environment)?;
    let named_radius = evaluated_number(named_argument(arguments, "r"), environment)?;
    let named_diameter = evaluated_number(named_argument(arguments, "d"), environment)?
        .map(|diameter| diameter / 2.0);
    let uniform = named_radius.or(named_diameter).or_else(|| {
        second_positional
            .is_none()
            .then_some(first_positional)
            .flatten()
    });

    let radius1 = evaluated_number(named_argument(arguments, "r1"), environment)?
        .or(
            evaluated_number(named_argument(arguments, "d1"), environment)?
                .map(|diameter| diameter / 2.0),
        )
        .or(uniform)
        .or(first_positional)
        .unwrap_or(1.0);
    let radius2 = evaluated_number(named_argument(arguments, "r2"), environment)?
        .or(
            evaluated_number(named_argument(arguments, "d2"), environment)?
                .map(|diameter| diameter / 2.0),
        )
        .or(uniform)
        .or(second_positional)
        .unwrap_or(1.0);
    Ok((radius1, radius2))
}

fn fragment_count(radius: f64, environment: &Environment) -> usize {
    if radius < 1e-6 {
        return 3;
    }
    let special = |name: &str, default: f64| {
        environment
            .get(name)
            .and_then(numeric_value)
            .filter(|value| value.is_finite())
            .unwrap_or(default)
    };
    let fragments = if special("$fn", 0.0) > 0.0 {
        special("$fn", 0.0).max(3.0).floor()
    } else {
        let fa = special("$fa", 12.0).max(0.01);
        let fs = special("$fs", 2.0).max(0.01);
        (360.0 / fa)
            .min(radius * 2.0 * std::f64::consts::PI / fs)
            .max(5.0)
            .ceil()
    };
    (fragments as usize).clamp(3, MAX_CURVE_FRAGMENTS)
}

fn argument_number(
    arguments: &[Argument],
    name: &str,
    index: usize,
    environment: &Environment,
    default: f64,
) -> Result<f64, EngineError> {
    argument_number_optional(arguments, name, index, environment)
        .map(|value| value.unwrap_or(default))
}

fn argument_number_optional(
    arguments: &[Argument],
    name: &str,
    index: usize,
    environment: &Environment,
) -> Result<Option<f64>, EngineError> {
    argument(arguments, name, index)
        .map(|expression| number(evaluate_expression(expression, environment)?, name))
        .transpose()
}

fn argument_bool(
    arguments: &[Argument],
    name: &str,
    index: usize,
    environment: &Environment,
    default: bool,
) -> Result<bool, EngineError> {
    let Some(expression) = argument(arguments, name, index) else {
        return Ok(default);
    };
    match evaluate_expression(expression, environment)? {
        Value::Bool(value) => Ok(value),
        Value::Number(value) => Ok(value != 0.0),
        _ => Err(EngineError::new(format!("Expected a boolean for {name}."))),
    }
}

fn argument_vector(
    arguments: &[Argument],
    name: &str,
    index: usize,
    environment: &Environment,
    default: Vec<f64>,
) -> Result<Vec<f64>, EngineError> {
    let Some(expression) = argument(arguments, name, index) else {
        return Ok(default);
    };
    match evaluate_expression(expression, environment)? {
        Value::Vector(values) => values
            .iter()
            .map(|value| {
                numeric_value(value).ok_or_else(|| {
                    EngineError::new(format!("Expected numeric components for {name}."))
                })
            })
            .collect(),
        Value::Number(value) => Ok(vec![value]),
        _ => Err(EngineError::new(format!("Expected a vector for {name}."))),
    }
}

fn argument_points2(
    arguments: &[Argument],
    name: &str,
    index: usize,
    environment: &Environment,
) -> Result<Vec<[f64; 2]>, EngineError> {
    let expression = argument(arguments, name, index)
        .ok_or_else(|| EngineError::new(format!("Expected {name}.")))?;
    let Value::Vector(points) = evaluate_expression(expression, environment)? else {
        return Err(EngineError::new(format!(
            "Expected a vector of 2D points for {name}."
        )));
    };
    points
        .into_iter()
        .map(|point| {
            let Value::Vector(components) = point else {
                return Err(EngineError::new(format!(
                    "Expected a vector of 2D points for {name}."
                )));
            };
            let [x, y] = components.as_slice() else {
                return Err(EngineError::new(format!(
                    "Expected exactly 2 components per point for {name}."
                )));
            };
            let (Some(x), Some(y)) = (numeric_value(x), numeric_value(y)) else {
                return Err(EngineError::new(format!(
                    "Expected numeric point components for {name}."
                )));
            };
            if !x.is_finite() || !y.is_finite() {
                return Err(EngineError::new(format!(
                    "Expected finite point components for {name}."
                )));
            }
            Ok([x, y])
        })
        .collect()
}

/// Read `polygon(paths = ...)`: a list of closed contours, each given as
/// indices into the shared point list.
///
/// Returns `Ok(None)` when the argument is absent or `undef`, which is the
/// "one contour using every point in order" default.
fn argument_index_paths(
    arguments: &[Argument],
    name: &str,
    index: usize,
    environment: &Environment,
    point_count: usize,
) -> Result<Option<Vec<Vec<usize>>>, EngineError> {
    let Some(expression) = argument(arguments, name, index) else {
        return Ok(None);
    };
    let value = evaluate_expression(expression, environment)?;
    if matches!(value, Value::Undefined) {
        return Ok(None);
    }
    let Value::Vector(paths) = value else {
        return Err(EngineError::new(format!(
            "Expected a vector of index lists for {name}."
        )));
    };
    paths
        .into_iter()
        .map(|path| {
            let Value::Vector(indices) = path else {
                return Err(EngineError::new(format!(
                    "Expected a vector of index lists for {name}."
                )));
            };
            indices
                .into_iter()
                .map(|entry| {
                    let index = numeric_value(&entry).ok_or_else(|| {
                        EngineError::new(format!("Expected numeric indices for {name}."))
                    })?;
                    if !index.is_finite() || index < 0.0 || index.fract() != 0.0 {
                        return Err(EngineError::new(format!(
                            "Expected non-negative whole indices for {name}."
                        )));
                    }
                    let index = index as usize;
                    if index >= point_count {
                        return Err(EngineError::new(format!(
                            "Index {index} in {name} is outside the {point_count} supplied points."
                        )));
                    }
                    Ok(index)
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// CSS/W3C extended color keywords, the set OpenSCAD's `color("name")` accepts.
/// Lower-case and sorted so lookup can binary-search.
const NAMED_COLORS: &[(&str, u32)] = &[
    ("aliceblue", 0xF0F8FF),
    ("antiquewhite", 0xFAEBD7),
    ("aqua", 0x00FFFF),
    ("aquamarine", 0x7FFFD4),
    ("azure", 0xF0FFFF),
    ("beige", 0xF5F5DC),
    ("bisque", 0xFFE4C4),
    ("black", 0x000000),
    ("blanchedalmond", 0xFFEBCD),
    ("blue", 0x0000FF),
    ("blueviolet", 0x8A2BE2),
    ("brown", 0xA52A2A),
    ("burlywood", 0xDEB887),
    ("cadetblue", 0x5F9EA0),
    ("chartreuse", 0x7FFF00),
    ("chocolate", 0xD2691E),
    ("coral", 0xFF7F50),
    ("cornflowerblue", 0x6495ED),
    ("cornsilk", 0xFFF8DC),
    ("crimson", 0xDC143C),
    ("cyan", 0x00FFFF),
    ("darkblue", 0x00008B),
    ("darkcyan", 0x008B8B),
    ("darkgoldenrod", 0xB8860B),
    ("darkgray", 0xA9A9A9),
    ("darkgreen", 0x006400),
    ("darkgrey", 0xA9A9A9),
    ("darkkhaki", 0xBDB76B),
    ("darkmagenta", 0x8B008B),
    ("darkolivegreen", 0x556B2F),
    ("darkorange", 0xFF8C00),
    ("darkorchid", 0x9932CC),
    ("darkred", 0x8B0000),
    ("darksalmon", 0xE9967A),
    ("darkseagreen", 0x8FBC8F),
    ("darkslateblue", 0x483D8B),
    ("darkslategray", 0x2F4F4F),
    ("darkslategrey", 0x2F4F4F),
    ("darkturquoise", 0x00CED1),
    ("darkviolet", 0x9400D3),
    ("deeppink", 0xFF1493),
    ("deepskyblue", 0x00BFFF),
    ("dimgray", 0x696969),
    ("dimgrey", 0x696969),
    ("dodgerblue", 0x1E90FF),
    ("firebrick", 0xB22222),
    ("floralwhite", 0xFFFAF0),
    ("forestgreen", 0x228B22),
    ("fuchsia", 0xFF00FF),
    ("gainsboro", 0xDCDCDC),
    ("ghostwhite", 0xF8F8FF),
    ("gold", 0xFFD700),
    ("goldenrod", 0xDAA520),
    ("gray", 0x808080),
    ("green", 0x008000),
    ("greenyellow", 0xADFF2F),
    ("grey", 0x808080),
    ("honeydew", 0xF0FFF0),
    ("hotpink", 0xFF69B4),
    ("indianred", 0xCD5C5C),
    ("indigo", 0x4B0082),
    ("ivory", 0xFFFFF0),
    ("khaki", 0xF0E68C),
    ("lavender", 0xE6E6FA),
    ("lavenderblush", 0xFFF0F5),
    ("lawngreen", 0x7CFC00),
    ("lemonchiffon", 0xFFFACD),
    ("lightblue", 0xADD8E6),
    ("lightcoral", 0xF08080),
    ("lightcyan", 0xE0FFFF),
    ("lightgoldenrodyellow", 0xFAFAD2),
    ("lightgray", 0xD3D3D3),
    ("lightgreen", 0x90EE90),
    ("lightgrey", 0xD3D3D3),
    ("lightpink", 0xFFB6C1),
    ("lightsalmon", 0xFFA07A),
    ("lightseagreen", 0x20B2AA),
    ("lightskyblue", 0x87CEFA),
    ("lightslategray", 0x778899),
    ("lightslategrey", 0x778899),
    ("lightsteelblue", 0xB0C4DE),
    ("lightyellow", 0xFFFFE0),
    ("lime", 0x00FF00),
    ("limegreen", 0x32CD32),
    ("linen", 0xFAF0E6),
    ("magenta", 0xFF00FF),
    ("maroon", 0x800000),
    ("mediumaquamarine", 0x66CDAA),
    ("mediumblue", 0x0000CD),
    ("mediumorchid", 0xBA55D3),
    ("mediumpurple", 0x9370DB),
    ("mediumseagreen", 0x3CB371),
    ("mediumslateblue", 0x7B68EE),
    ("mediumspringgreen", 0x00FA9A),
    ("mediumturquoise", 0x48D1CC),
    ("mediumvioletred", 0xC71585),
    ("midnightblue", 0x191970),
    ("mintcream", 0xF5FFFA),
    ("mistyrose", 0xFFE4E1),
    ("moccasin", 0xFFE4B5),
    ("navajowhite", 0xFFDEAD),
    ("navy", 0x000080),
    ("oldlace", 0xFDF5E6),
    ("olive", 0x808000),
    ("olivedrab", 0x6B8E23),
    ("orange", 0xFFA500),
    ("orangered", 0xFF4500),
    ("orchid", 0xDA70D6),
    ("palegoldenrod", 0xEEE8AA),
    ("palegreen", 0x98FB98),
    ("paleturquoise", 0xAFEEEE),
    ("palevioletred", 0xDB7093),
    ("papayawhip", 0xFFEFD5),
    ("peachpuff", 0xFFDAB9),
    ("peru", 0xCD853F),
    ("pink", 0xFFC0CB),
    ("plum", 0xDDA0DD),
    ("powderblue", 0xB0E0E6),
    ("purple", 0x800080),
    ("rebeccapurple", 0x663399),
    ("red", 0xFF0000),
    ("rosybrown", 0xBC8F8F),
    ("royalblue", 0x4169E1),
    ("saddlebrown", 0x8B4513),
    ("salmon", 0xFA8072),
    ("sandybrown", 0xF4A460),
    ("seagreen", 0x2E8B57),
    ("seashell", 0xFFF5EE),
    ("sienna", 0xA0522D),
    ("silver", 0xC0C0C0),
    ("skyblue", 0x87CEEB),
    ("slateblue", 0x6A5ACD),
    ("slategray", 0x708090),
    ("slategrey", 0x708090),
    ("snow", 0xFFFAFA),
    ("springgreen", 0x00FF7F),
    ("steelblue", 0x4682B4),
    ("tan", 0xD2B48C),
    ("teal", 0x008080),
    ("thistle", 0xD8BFD8),
    ("tomato", 0xFF6347),
    ("turquoise", 0x40E0D0),
    ("violet", 0xEE82EE),
    ("wheat", 0xF5DEB3),
    ("white", 0xFFFFFF),
    ("whitesmoke", 0xF5F5F5),
    ("yellow", 0xFFFF00),
    ("yellowgreen", 0x9ACD32),
];

/// Parse the first argument of `color()` into linear-ish RGBA in 0..1.
///
/// `Ok(None)` means "no color stated" (`undef`), which leaves the children's
/// existing appearance alone.
fn parse_color_value(value: &Value) -> Result<Option<[f64; 4]>, String> {
    match value {
        Value::Undefined => Ok(None),
        Value::String(name) => parse_color_name(name).map(Some),
        Value::Vector(components) => {
            let components = components
                .iter()
                .map(|component| {
                    numeric_value(component)
                        .filter(|value| value.is_finite())
                        .ok_or_else(|| "color components must be finite numbers".to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            match components.as_slice() {
                [r, g, b] => Ok(Some([
                    r.clamp(0.0, 1.0),
                    g.clamp(0.0, 1.0),
                    b.clamp(0.0, 1.0),
                    1.0,
                ])),
                [r, g, b, a] => Ok(Some([
                    r.clamp(0.0, 1.0),
                    g.clamp(0.0, 1.0),
                    b.clamp(0.0, 1.0),
                    a.clamp(0.0, 1.0),
                ])),
                _ => Err("expected a 3- or 4-component color vector".into()),
            }
        }
        _ => Err("expected a color name, \"#rrggbb\" string, or [r, g, b] vector".into()),
    }
}

/// Resolve a `color()` string: a CSS keyword or a `#rgb` / `#rgba` /
/// `#rrggbb` / `#rrggbbaa` literal.
fn parse_color_name(name: &str) -> Result<[f64; 4], String> {
    let trimmed = name.trim();
    if let Some(hex) = trimmed.strip_prefix('#') {
        if !hex.chars().all(|character| character.is_ascii_hexdigit()) {
            return Err(format!("{name:?} is not a valid hex color"));
        }
        let expand = |digit: char| {
            let value = digit.to_digit(16).unwrap_or(0) as f64;
            (value * 17.0) / 255.0
        };
        let pair = |text: &str| u8::from_str_radix(text, 16).map(|byte| byte as f64 / 255.0);
        let digits = hex.chars().collect::<Vec<_>>();
        return match digits.len() {
            3 => Ok([expand(digits[0]), expand(digits[1]), expand(digits[2]), 1.0]),
            4 => Ok([
                expand(digits[0]),
                expand(digits[1]),
                expand(digits[2]),
                expand(digits[3]),
            ]),
            6 | 8 => {
                let mut rgba = [0.0, 0.0, 0.0, 1.0];
                for (index, chunk) in hex.as_bytes().chunks(2).enumerate() {
                    let text = std::str::from_utf8(chunk)
                        .map_err(|_| format!("{name:?} is not a valid hex color"))?;
                    rgba[index] =
                        pair(text).map_err(|_| format!("{name:?} is not a valid hex color"))?;
                }
                Ok(rgba)
            }
            _ => Err(format!("{name:?} is not a valid hex color")),
        };
    }
    if trimmed.eq_ignore_ascii_case("transparent") {
        return Ok([0.0, 0.0, 0.0, 0.0]);
    }
    let lowercase = trimmed.to_ascii_lowercase();
    NAMED_COLORS
        .binary_search_by(|(candidate, _)| (*candidate).cmp(lowercase.as_str()))
        .map(|index| {
            let packed = NAMED_COLORS[index].1;
            [
                ((packed >> 16) & 0xFF) as f64 / 255.0,
                ((packed >> 8) & 0xFF) as f64 / 255.0,
                (packed & 0xFF) as f64 / 255.0,
                1.0,
            ]
        })
        .map_err(|_| format!("unknown color name {name:?}"))
}

fn vec3_argument(
    arguments: &[Argument],
    index: usize,
    environment: &Environment,
    default: Vec3,
) -> Result<Vec3, EngineError> {
    let values = argument_vector(
        arguments,
        "v",
        index,
        environment,
        vec![default.x, default.y, default.z],
    )?;
    match values.as_slice() {
        [x, y] => Ok(Vec3::new(*x, *y, 0.0)),
        [x, y, z, ..] => Ok(Vec3::new(*x, *y, *z)),
        _ => Err(EngineError::new("Expected a 2- or 3-vector.")),
    }
}

fn vec3_is_finite(value: Vec3) -> bool {
    value.x.is_finite() && value.y.is_finite() && value.z.is_finite()
}

#[derive(Clone, Copy, Debug)]
struct Bounds {
    min: Vec3,
    max: Vec3,
}

impl Bounds {
    fn union(self, other: Self) -> Self {
        Self {
            min: Vec3::new(
                self.min.x.min(other.min.x),
                self.min.y.min(other.min.y),
                self.min.z.min(other.min.z),
            ),
            max: Vec3::new(
                self.max.x.max(other.max.x),
                self.max.y.max(other.max.y),
                self.max.z.max(other.max.z),
            ),
        }
    }

    fn intersection(self, other: Self) -> Self {
        Self {
            min: Vec3::new(
                self.min.x.max(other.min.x),
                self.min.y.max(other.min.y),
                self.min.z.max(other.min.z),
            ),
            max: Vec3::new(
                self.max.x.min(other.max.x),
                self.max.y.min(other.max.y),
                self.max.z.min(other.max.z),
            ),
        }
    }

    fn corners(self) -> [Vec3; 8] {
        let min = self.min;
        let max = self.max;
        [
            Vec3::new(min.x, min.y, min.z),
            Vec3::new(max.x, min.y, min.z),
            Vec3::new(max.x, max.y, min.z),
            Vec3::new(min.x, max.y, min.z),
            Vec3::new(min.x, min.y, max.z),
            Vec3::new(max.x, min.y, max.z),
            Vec3::new(max.x, max.y, max.z),
            Vec3::new(min.x, max.y, max.z),
        ]
    }

    fn outside_distance(self, point: Vec3, dimension: ShapeDimension) -> f64 {
        let dx = (self.min.x - point.x).max(point.x - self.max.x).max(0.0);
        let dy = (self.min.y - point.y).max(point.y - self.max.y).max(0.0);
        let dz = if dimension == ShapeDimension::Planar {
            0.0
        } else {
            (self.min.z - point.z).max(point.z - self.max.z).max(0.0)
        };
        Vec3::new(dx, dy, dz).length()
    }
}

#[derive(Clone, Copy, Debug)]
struct Transform {
    forward: [[f64; 4]; 4],
    inverse: [[f64; 4]; 4],
    distance_scale: f64,
    preserves_euclidean_distance: bool,
}

impl Transform {
    fn translation(value: Vec3) -> Self {
        let mut forward = identity();
        forward[0][3] = value.x;
        forward[1][3] = value.y;
        forward[2][3] = value.z;
        let mut inverse = identity();
        inverse[0][3] = -value.x;
        inverse[1][3] = -value.y;
        inverse[2][3] = -value.z;
        Self {
            forward,
            inverse,
            distance_scale: 1.0,
            preserves_euclidean_distance: true,
        }
    }

    fn scale(value: Vec3) -> Self {
        let mut forward = identity();
        forward[0][0] = value.x;
        forward[1][1] = value.y;
        forward[2][2] = value.z;
        let mut inverse = identity();
        inverse[0][0] = 1.0 / value.x;
        inverse[1][1] = 1.0 / value.y;
        inverse[2][2] = 1.0 / value.z;
        Self {
            forward,
            inverse,
            distance_scale: value.x.abs().min(value.y.abs()).min(value.z.abs()),
            preserves_euclidean_distance: (value.x.abs() - value.y.abs()).abs() <= 1e-12
                && (value.y.abs() - value.z.abs()).abs() <= 1e-12,
        }
    }

    fn rotation_xyz(degrees: Vec3) -> Self {
        let rx = rotation_x(degrees.x.to_radians());
        let ry = rotation_y(degrees.y.to_radians());
        let rz = rotation_z(degrees.z.to_radians());
        let forward = matrix_multiply(rz, matrix_multiply(ry, rx));
        let inverse = transpose_rotation(forward);
        Self {
            forward,
            inverse,
            distance_scale: 1.0,
            preserves_euclidean_distance: true,
        }
    }

    /// `rotate(a, v)`: right-handed rotation of `degrees` about `axis`.
    ///
    /// A zero-length or non-finite axis has no rotation plane, so the
    /// transform degrades to the identity instead of producing NaNs — the same
    /// visible outcome as OpenSCAD, which leaves such children unrotated.
    fn rotation_axis_angle(axis: Vec3, degrees: f64) -> Self {
        let length = axis.length();
        if !length.is_finite() || length <= 1e-12 || !degrees.is_finite() {
            return Self::rotation_xyz(Vec3::default());
        }
        let axis = axis.mul(1.0 / length);
        let (sin, cos) = degrees.to_radians().sin_cos();
        let one_minus_cos = 1.0 - cos;
        let (x, y, z) = (axis.x, axis.y, axis.z);
        let forward = [
            [
                cos + x * x * one_minus_cos,
                x * y * one_minus_cos - z * sin,
                x * z * one_minus_cos + y * sin,
                0.0,
            ],
            [
                y * x * one_minus_cos + z * sin,
                cos + y * y * one_minus_cos,
                y * z * one_minus_cos - x * sin,
                0.0,
            ],
            [
                z * x * one_minus_cos - y * sin,
                z * y * one_minus_cos + x * sin,
                cos + z * z * one_minus_cos,
                0.0,
            ],
            [0.0, 0.0, 0.0, 1.0],
        ];
        let inverse = transpose_rotation(forward);
        Self {
            forward,
            inverse,
            distance_scale: 1.0,
            preserves_euclidean_distance: true,
        }
    }

    fn mirror(normal: Vec3) -> Result<Self, EngineError> {
        let length = normal.length();
        if length <= 1e-12 {
            return Err(EngineError::new("mirror() normal cannot be zero."));
        }
        let n = normal.mul(1.0 / length);
        let mut matrix = identity();
        for (row, matrix_row) in matrix.iter_mut().take(3).enumerate() {
            for (column, cell) in matrix_row.iter_mut().take(3).enumerate() {
                let a = n.component(row);
                let b = n.component(column);
                *cell -= 2.0 * a * b;
            }
        }
        Ok(Self {
            forward: matrix,
            inverse: matrix,
            distance_scale: 1.0,
            preserves_euclidean_distance: true,
        })
    }

    fn point(matrix: [[f64; 4]; 4], point: Vec3) -> Vec3 {
        Vec3::new(
            matrix[0][0] * point.x + matrix[0][1] * point.y + matrix[0][2] * point.z + matrix[0][3],
            matrix[1][0] * point.x + matrix[1][1] * point.y + matrix[1][2] * point.z + matrix[1][3],
            matrix[2][0] * point.x + matrix[2][1] * point.y + matrix[2][2] * point.z + matrix[2][3],
        )
    }
}

fn identity() -> [[f64; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

fn rotation_x(angle: f64) -> [[f64; 4]; 4] {
    let (sin, cos) = angle.sin_cos();
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, cos, -sin, 0.0],
        [0.0, sin, cos, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

fn rotation_y(angle: f64) -> [[f64; 4]; 4] {
    let (sin, cos) = angle.sin_cos();
    [
        [cos, 0.0, sin, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [-sin, 0.0, cos, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

fn rotation_z(angle: f64) -> [[f64; 4]; 4] {
    let (sin, cos) = angle.sin_cos();
    [
        [cos, -sin, 0.0, 0.0],
        [sin, cos, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

fn matrix_multiply(left: [[f64; 4]; 4], right: [[f64; 4]; 4]) -> [[f64; 4]; 4] {
    let mut output = [[0.0; 4]; 4];
    for (output_row, left_row) in output.iter_mut().zip(left) {
        for (column, output_cell) in output_row.iter_mut().enumerate() {
            for (left_value, right_row) in left_row.into_iter().zip(right) {
                *output_cell += left_value * right_row[column];
            }
        }
    }
    output
}

fn transpose_rotation(matrix: [[f64; 4]; 4]) -> [[f64; 4]; 4] {
    [
        [matrix[0][0], matrix[1][0], matrix[2][0], 0.0],
        [matrix[0][1], matrix[1][1], matrix[2][1], 0.0],
        [matrix[0][2], matrix[1][2], matrix[2][2], 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShapeDimension {
    Planar,
    Solid,
}

#[derive(Clone, Copy, Debug)]
struct HullPlane {
    normal: Vec3,
    offset: f64,
}

/// A child of an n-ary CSG node, carrying the cached bounding volume that lets
/// the fold skip it.
///
/// `bounds_distance_scale` is a factor `s` for which
///
/// ```text
/// shape.distance(p) >= s * bounds.outside_distance(p)   whenever the right side is > 0
/// ```
///
/// is guaranteed. `s == 1` means the bounds distance is directly a lower bound
/// on the value the shape reports; `s == 0` records that nothing beyond the
/// sign of the child's contribution can be inferred from its bounds. Shapes
/// whose distance function systematically underestimates the true Euclidean
/// distance — a convex hull's max-over-planes, for one — sit in between.
#[derive(Clone, Debug)]
struct BoundedChild {
    shape: Shape,
    bounds: Bounds,
    dimension: ShapeDimension,
    bounds_distance_scale: f64,
}

impl BoundedChild {
    fn new(shape: Shape) -> Self {
        Self {
            bounds: shape.bounds(),
            dimension: shape.dimension(),
            bounds_distance_scale: shape.bounds_distance_scale(),
            shape,
        }
    }
}

#[derive(Clone, Debug)]
enum ChildBvh {
    Leaf {
        bounds: Bounds,
        dimension: ShapeDimension,
        bounds_distance_scale: f64,
        indices: Vec<usize>,
    },
    Branch {
        bounds: Bounds,
        dimension: ShapeDimension,
        bounds_distance_scale: f64,
        left: Box<ChildBvh>,
        right: Box<ChildBvh>,
    },
}

impl ChildBvh {
    fn build(children: &[BoundedChild]) -> Self {
        Self::build_indices(children, (0..children.len()).collect())
    }

    fn build_indices(children: &[BoundedChild], mut indices: Vec<usize>) -> Self {
        let first = &children[indices[0]];
        let bounds = indices.iter().skip(1).fold(first.bounds, |bounds, index| {
            bounds.union(children[*index].bounds)
        });
        let dimension = first.dimension;
        // A node's bounds enclose every child's, so its outside distance never
        // exceeds a child's; the weakest child scale therefore still holds for
        // the whole node. A node mixing dimensions cannot compare the two at
        // all, because `outside_distance` drops the z extent for planar boxes.
        let bounds_distance_scale = if indices
            .iter()
            .all(|index| children[*index].dimension == dimension)
        {
            indices.iter().fold(f64::INFINITY, |scale, index| {
                scale.min(children[*index].bounds_distance_scale)
            })
        } else {
            0.0
        };
        if indices.len() <= 8 {
            return Self::Leaf {
                bounds,
                dimension,
                bounds_distance_scale,
                indices,
            };
        }

        let span = bounds.max.sub(bounds.min);
        let axis = if span.x >= span.y && span.x >= span.z {
            0
        } else if span.y >= span.z {
            1
        } else {
            2
        };
        indices.sort_unstable_by(|left, right| {
            let left_bounds = children[*left].bounds;
            let right_bounds = children[*right].bounds;
            let left_center =
                (left_bounds.min.component(axis) + left_bounds.max.component(axis)) / 2.0;
            let right_center =
                (right_bounds.min.component(axis) + right_bounds.max.component(axis)) / 2.0;
            left_center.total_cmp(&right_center)
        });
        let right_indices = indices.split_off(indices.len() / 2);
        Self::Branch {
            bounds,
            dimension,
            bounds_distance_scale,
            left: Box::new(Self::build_indices(children, indices)),
            right: Box::new(Self::build_indices(children, right_indices)),
        }
    }

    fn metadata(&self) -> (Bounds, ShapeDimension, f64) {
        match self {
            Self::Leaf {
                bounds,
                dimension,
                bounds_distance_scale,
                ..
            }
            | Self::Branch {
                bounds,
                dimension,
                bounds_distance_scale,
                ..
            } => (*bounds, *dimension, *bounds_distance_scale),
        }
    }

    fn distance(&self, children: &[BoundedChild], point: Vec3, minimum: &mut f64) {
        let (bounds, dimension, scale) = self.metadata();
        let bounds_distance = bounds.outside_distance(point, dimension);
        if bounds_distance > 0.0 && (*minimum <= 0.0 || bounds_distance * scale >= *minimum) {
            return;
        }

        match self {
            Self::Leaf { indices, .. } => {
                for index in indices {
                    bounded_child_minimum(&children[*index], point, minimum);
                }
            }
            Self::Branch { left, right, .. } => {
                let (left_bounds, left_dimension, _) = left.metadata();
                let (right_bounds, right_dimension, _) = right.metadata();
                let left_distance = left_bounds.outside_distance(point, left_dimension);
                let right_distance = right_bounds.outside_distance(point, right_dimension);
                if left_distance <= right_distance {
                    left.distance(children, point, minimum);
                    right.distance(children, point, minimum);
                } else {
                    right.distance(children, point, minimum);
                    left.distance(children, point, minimum);
                }
            }
        }
    }

    /// Fold `maximum = max(maximum, -child.distance(point))` over the subtree,
    /// skipping any node whose bounds prove it cannot raise `maximum`.
    ///
    /// This is [`Self::distance`] mirrored through the negation that
    /// `difference()` applies to its subtrahends, and it prunes on exactly the
    /// same guarantees — see [`subtracted_child_is_ineffective`].
    fn subtracted_maximum(&self, children: &[BoundedChild], point: Vec3, maximum: &mut f64) {
        let (bounds, dimension, scale) = self.metadata();
        let bounds_distance = bounds.outside_distance(point, dimension);
        if bounds_distance > 0.0 && (*maximum >= 0.0 || bounds_distance * scale >= -*maximum) {
            return;
        }

        match self {
            Self::Leaf { indices, .. } => {
                for index in indices {
                    bounded_child_subtracted_maximum(&children[*index], point, maximum);
                }
            }
            Self::Branch { left, right, .. } => {
                let (left_bounds, left_dimension, _) = left.metadata();
                let (right_bounds, right_dimension, _) = right.metadata();
                let left_distance = left_bounds.outside_distance(point, left_dimension);
                let right_distance = right_bounds.outside_distance(point, right_dimension);
                if left_distance <= right_distance {
                    left.subtracted_maximum(children, point, maximum);
                    right.subtracted_maximum(children, point, maximum);
                } else {
                    right.subtracted_maximum(children, point, maximum);
                    left.subtracted_maximum(children, point, maximum);
                }
            }
        }
    }
}

fn bounded_child_minimum(child: &BoundedChild, point: Vec3, minimum: &mut f64) {
    let bounds_distance = child.bounds.outside_distance(point, child.dimension);
    if bounds_distance > 0.0
        && (*minimum <= 0.0 || bounds_distance * child.bounds_distance_scale >= *minimum)
    {
        return;
    }
    *minimum = (*minimum).min(child.shape.distance(point));
}

/// Whether `max(running, -child.distance(point))` provably equals `running`,
/// so the subtrahend can be skipped without changing a single bit of the result.
///
/// Two guarantees make this exact. First, a point outside a shape's bounds is
/// outside the shape, so every distance function here returns a strictly
/// positive value there: when `running >= 0`, `-child.distance(point) < 0 <=
/// running` and the `max` is already settled. Second, when
/// `bounds_distance_scale` holds, `child.distance(point) >=
/// bounds_distance`, so `bounds_distance >= -running` gives
/// `-child.distance(point) <= running` and the `max` is settled again — this
/// time for points that sit inside the minuend.
fn subtracted_child_is_ineffective(child: &BoundedChild, point: Vec3, running: f64) -> bool {
    let bounds_distance = child.bounds.outside_distance(point, child.dimension);
    bounds_distance > 0.0
        && (running >= 0.0 || bounds_distance * child.bounds_distance_scale >= -running)
}

fn bounded_child_subtracted_maximum(child: &BoundedChild, point: Vec3, maximum: &mut f64) {
    if subtracted_child_is_ineffective(child, point, *maximum) {
        return;
    }
    *maximum = (*maximum).max(-child.shape.distance(point));
}

#[derive(Clone, Debug)]
enum Shape {
    Square2d {
        size: [f64; 2],
        center: bool,
    },
    /// Closed 2D contours combined by the even-odd rule: the first is the
    /// outline and later ones alternately cut holes and re-fill islands, which
    /// is exactly what `polygon(points, paths)` means. A plain
    /// `polygon(points)` is the single-contour case.
    Polygon2d {
        contours: Vec<Vec<[f64; 2]>>,
    },
    /// `color()`: geometrically transparent, carrying preview/appearance data
    /// only. Every geometric query delegates straight to `shape`, so wrapping
    /// or unwrapping a `Color` never changes the solid it describes.
    Color {
        shape: Box<Shape>,
        rgba: [f64; 4],
    },
    Circle2d {
        radius: f64,
        fragments: usize,
    },
    Sphere {
        radius: f64,
        fragments: usize,
    },
    Box {
        size: Vec3,
        center: bool,
    },
    Cylinder {
        height: f64,
        radius1: f64,
        radius2: f64,
        center: bool,
        fragments: usize,
    },
    Transform {
        shape: Box<Shape>,
        transform: Transform,
    },
    Offset2d {
        shape: Box<Shape>,
        delta: f64,
        /// `offset(r = ...)` rounds convex corners into arcs; `offset(delta =
        /// ...)` mitres them. Which of the two the user wrote decides the
        /// output geometry, so it is recorded rather than assumed.
        rounded: bool,
        /// Segments per full circle for a rounded join, resolved from
        /// `$fn`/`$fa`/`$fs` exactly as `circle()` resolves them.
        fragments: usize,
    },
    LinearExtrude {
        shape: Box<Shape>,
        height: f64,
        center: bool,
        /// Degrees of rotation from bottom to top, `linear_extrude(twist=)`.
        twist: f64,
        /// Per-axis scale factor applied at the top, `linear_extrude(scale=)`.
        scale: [f64; 2],
        /// How many layers a twisted or tapered extrusion is built from.
        slices: usize,
    },
    MinkowskiDilation {
        shape: Box<Shape>,
        /// Centre of the second operand's bounding box.
        offset: Vec3,
        /// Bounding radius of the second operand.
        radius: f64,
        /// The second operand of `minkowski()` itself.
        ///
        /// `offset` and `radius` describe the ball the *sampled* path dilates
        /// by, because a distance field cannot express a general Minkowski sum
        /// — only a dilation by a sphere. The exact kernel has a real
        /// `csg::primitives::minkowski`, so the operand is carried here
        /// alongside the approximation instead of being thrown away at
        /// evaluation time. `None` only where no operand was recorded.
        kernel: Option<Box<Shape>>,
    },
    Hull {
        planes: Vec<HullPlane>,
        bounds: Bounds,
        bounds_distance_scale: f64,
    },
    Union {
        children: Vec<BoundedChild>,
        bounds: Bounds,
        bvh: Option<Box<ChildBvh>>,
        bounds_distance_scale: f64,
    },
    Difference {
        base: Box<Shape>,
        subtract: Vec<BoundedChild>,
        bvh: Option<Box<ChildBvh>>,
    },
    Intersection(Vec<Shape>),
}

/// Child count at which an n-ary node stops folding linearly and builds a BVH.
const BVH_CHILD_THRESHOLD: usize = 16;

impl Shape {
    fn union(shapes: Vec<Shape>) -> Option<Shape> {
        let mut children = Vec::with_capacity(shapes.len());
        for shape in shapes {
            match shape {
                Shape::Union {
                    children: nested, ..
                } => children.extend(nested),
                shape => children.push(BoundedChild::new(shape)),
            }
        }
        match children.len() {
            0 => None,
            1 => children.pop().map(|child| child.shape),
            _ => {
                let bounds = children
                    .iter()
                    .skip(1)
                    .fold(children[0].bounds, |bounds, child| {
                        bounds.union(child.bounds)
                    });
                // `Shape::dimension` reports the first child's dimension for a
                // union, and `outside_distance` measures a planar box without
                // its z extent. Only claim the bound when every child agrees on
                // the dimension, so the union's own bounds distance is compared
                // against child guarantees that were made the same way.
                let dimension = children[0].dimension;
                let bounds_distance_scale =
                    if children.iter().all(|child| child.dimension == dimension) {
                        children.iter().fold(f64::INFINITY, |scale, child| {
                            scale.min(child.bounds_distance_scale)
                        })
                    } else {
                        0.0
                    };
                let bvh = (children.len() >= BVH_CHILD_THRESHOLD)
                    .then(|| Box::new(ChildBvh::build(&children)));
                Some(Shape::Union {
                    children,
                    bounds,
                    bvh,
                    bounds_distance_scale,
                })
            }
        }
    }

    /// Build `difference()`: `base` minus every shape in `subtract`.
    fn difference(base: Shape, subtract: Vec<Shape>) -> Shape {
        let subtract = subtract.into_iter().map(BoundedChild::new).collect::<Vec<_>>();
        let bvh = (subtract.len() >= BVH_CHILD_THRESHOLD)
            .then(|| Box::new(ChildBvh::build(&subtract)));
        Shape::Difference {
            base: Box::new(base),
            subtract,
            bvh,
        }
    }

    /// The `color()` that applies to this whole shape, if any.
    ///
    /// Only wrappers that cannot change which sub-shape is coloured are looked
    /// through, so `color("red") translate(...) cube()` reports red while a
    /// union of differently coloured children reports nothing.
    fn stated_color(&self) -> Option<[f64; 4]> {
        match self {
            Shape::Color { rgba, .. } => Some(*rgba),
            Shape::Transform { shape, .. }
            | Shape::Offset2d { shape, .. }
            | Shape::LinearExtrude { shape, .. }
            | Shape::MinkowskiDilation { shape, .. } => shape.stated_color(),
            Shape::Difference { base, .. } => base.stated_color(),
            _ => None,
        }
    }

    /// The factor `s` documented on [`BoundedChild::bounds_distance_scale`].
    fn bounds_distance_scale(&self) -> f64 {
        match self {
            Shape::Square2d { .. }
            | Shape::Polygon2d { .. }
            | Shape::Circle2d { .. }
            | Shape::Box { .. } => 1.0,
            Shape::Sphere { .. } => 0.0,
            Shape::Cylinder {
                radius1, radius2, ..
            } => {
                if radius1 == radius2 {
                    1.0
                } else {
                    0.0
                }
            }
            Shape::Transform { shape, transform } => {
                if transform.preserves_euclidean_distance {
                    shape.bounds_distance_scale()
                } else {
                    0.0
                }
            }
            Shape::Offset2d { shape, .. }
            | Shape::LinearExtrude { shape, .. }
            | Shape::Color { shape, .. }
            | Shape::MinkowskiDilation { shape, .. } => shape.bounds_distance_scale(),
            // A hull reports `max(n_i·p - o_i)`, which underestimates the true
            // distance near edges and corners. `Shape::convex_hull_from_points`
            // derives how far it can fall short.
            Shape::Hull {
                bounds_distance_scale,
                ..
            } => *bounds_distance_scale,
            // A union's bounds enclose every child's bounds, so the union's
            // bounds distance never exceeds any child's — and therefore never
            // exceeds their minimum. `Shape::union` decides this once.
            Shape::Union {
                bounds_distance_scale,
                ..
            } => *bounds_distance_scale,
            // A difference reports the minuend's bounds and dimension, and
            // subtracting only raises the value, so the minuend's guarantee
            // carries over unchanged.
            Shape::Difference { base, .. } => base.bounds_distance_scale(),
            // An intersection's bounds are the intersection of its children's,
            // which can sit arbitrarily far from a point that is close to every
            // individual child, so `max` of the children may fall below the
            // bounds distance by an unbounded factor.
            Shape::Intersection(..) => 0.0,
        }
    }

    fn dimension(&self) -> ShapeDimension {
        match self {
            Shape::Square2d { .. }
            | Shape::Polygon2d { .. }
            | Shape::Circle2d { .. }
            | Shape::Offset2d { .. } => ShapeDimension::Planar,
            Shape::Sphere { .. }
            | Shape::Box { .. }
            | Shape::Cylinder { .. }
            | Shape::LinearExtrude { .. }
            | Shape::Hull { .. } => ShapeDimension::Solid,
            Shape::Transform { shape, .. }
            | Shape::Color { shape, .. }
            | Shape::MinkowskiDilation { shape, .. } => shape.dimension(),
            Shape::Union { children, .. } => children
                .first()
                .map(|child| child.dimension)
                .unwrap_or(ShapeDimension::Solid),
            Shape::Intersection(shapes) => shapes
                .first()
                .map(Shape::dimension)
                .unwrap_or(ShapeDimension::Solid),
            Shape::Difference { base, .. } => base.dimension(),
        }
    }

    fn distance(&self, point: Vec3) -> f64 {
        match self {
            Shape::Square2d { size, center } => {
                let center_x = if *center { 0.0 } else { size[0] / 2.0 };
                let center_y = if *center { 0.0 } else { size[1] / 2.0 };
                rectangle_distance(
                    point.x - center_x,
                    point.y - center_y,
                    size[0] / 2.0,
                    size[1] / 2.0,
                )
            }
            Shape::Polygon2d { contours } => polygon_distance(contours, [point.x, point.y]),
            Shape::Color { shape, .. } => shape.distance(point),
            Shape::Circle2d { radius, fragments } => {
                regular_polygon_distance(point.x, point.y, *radius, *fragments)
            }
            Shape::Sphere { radius, fragments } => {
                faceted_sphere_distance(point, *radius, *fragments)
            }
            Shape::Box { size, center } => {
                let center_point = if *center {
                    Vec3::default()
                } else {
                    size.mul(0.5)
                };
                let half = size.mul(0.5);
                let q = Vec3::new(
                    (point.x - center_point.x).abs() - half.x,
                    (point.y - center_point.y).abs() - half.y,
                    (point.z - center_point.z).abs() - half.z,
                );
                let outside = Vec3::new(q.x.max(0.0), q.y.max(0.0), q.z.max(0.0)).length();
                outside + q.x.max(q.y).max(q.z).min(0.0)
            }
            Shape::Cylinder {
                height,
                radius1,
                radius2,
                center,
                fragments,
            } => {
                let base = if *center { -height / 2.0 } else { 0.0 };
                let relative_z = point.z - base;
                let interpolation = (relative_z / height).clamp(0.0, 1.0);
                let radius = radius1 + (radius2 - radius1) * interpolation;
                let radial = regular_polygon_distance(point.x, point.y, radius, *fragments);
                let vertical = (relative_z - height / 2.0).abs() - height / 2.0;
                Vec3::new(radial.max(0.0), vertical.max(0.0), 0.0).length()
                    + radial.max(vertical).min(0.0)
            }
            Shape::Transform { shape, transform } => {
                shape.distance(Transform::point(transform.inverse, point))
                    * transform.distance_scale
            }
            // Subtracting the offset from the distance is exactly
            // `offset(r = delta)`: it rounds every convex corner. A mitred
            // `offset(delta = ...)` has no closed form in a distance field, so
            // the sampled path rounds that one too; `rounded` is carried for
            // the exact kernel, which does honour it.
            Shape::Offset2d { shape, delta, .. } => shape.distance(point) - delta,
            // A distance field cannot express a swept profile, so the sampled
            // path meshes the *untwisted, unscaled* prism. `twist` and `scale`
            // are carried for the exact kernel, which sweeps them properly, and
            // the sampled path says so rather than dropping them in silence.
            Shape::LinearExtrude {
                shape,
                height,
                center,
                ..
            } => {
                let planar = shape.distance(Vec3::new(point.x, point.y, 0.0));
                let minimum_z = if *center { -height / 2.0 } else { 0.0 };
                let slab = (point.z - (minimum_z + height / 2.0)).abs() - height / 2.0;
                let outside = planar.max(0.0).hypot(slab.max(0.0));
                outside + planar.max(slab).min(0.0)
            }
            Shape::MinkowskiDilation {
                shape,
                offset,
                radius,
                ..
            } => shape.distance(point.sub(*offset)) - radius,
            Shape::Hull { planes, .. } => planes
                .iter()
                .map(|plane| plane.normal.dot(point) - plane.offset)
                .fold(f64::NEG_INFINITY, f64::max),
            Shape::Union { children, bvh, .. } => {
                let mut minimum = f64::INFINITY;
                if let Some(bvh) = bvh {
                    bvh.distance(children, point, &mut minimum);
                } else {
                    for child in children {
                        bounded_child_minimum(child, point, &mut minimum);
                    }
                }
                minimum
            }
            Shape::Difference {
                base,
                subtract,
                bvh,
            } => {
                let mut maximum = base.distance(point);
                if let Some(bvh) = bvh {
                    bvh.subtracted_maximum(subtract, point, &mut maximum);
                } else {
                    for child in subtract {
                        bounded_child_subtracted_maximum(child, point, &mut maximum);
                    }
                }
                maximum
            }
            Shape::Intersection(shapes) => shapes
                .iter()
                .map(|shape| shape.distance(point))
                .fold(f64::NEG_INFINITY, f64::max),
        }
    }

    fn bounds(&self) -> Bounds {
        match self {
            Shape::Square2d { size, center } => {
                let minimum = if *center {
                    Vec3::new(-size[0] / 2.0, -size[1] / 2.0, 0.0)
                } else {
                    Vec3::default()
                };
                Bounds {
                    min: minimum,
                    max: Vec3::new(minimum.x + size[0], minimum.y + size[1], 0.0),
                }
            }
            Shape::Color { shape, .. } => shape.bounds(),
            Shape::Polygon2d { contours } => {
                let first = contours[0][0];
                contours.iter().flatten().skip(1).fold(
                    Bounds {
                        min: Vec3::new(first[0], first[1], 0.0),
                        max: Vec3::new(first[0], first[1], 0.0),
                    },
                    |bounds, point| {
                        bounds.union(Bounds {
                            min: Vec3::new(point[0], point[1], 0.0),
                            max: Vec3::new(point[0], point[1], 0.0),
                        })
                    },
                )
            }
            Shape::Circle2d { radius, fragments } => {
                let (minimum, maximum) = regular_polygon_xy_bounds(*radius, *fragments);
                Bounds {
                    min: Vec3::new(minimum[0], minimum[1], 0.0),
                    max: Vec3::new(maximum[0], maximum[1], 0.0),
                }
            }
            Shape::Sphere { radius, fragments } => faceted_sphere_bounds(*radius, *fragments),
            Shape::Box { size, center } => {
                if *center {
                    Bounds {
                        min: size.mul(-0.5),
                        max: size.mul(0.5),
                    }
                } else {
                    Bounds {
                        min: Vec3::default(),
                        max: *size,
                    }
                }
            }
            Shape::Cylinder {
                height,
                radius1,
                radius2,
                center,
                fragments,
            } => {
                let radius = radius1.max(*radius2);
                let minimum_z = if *center { -height / 2.0 } else { 0.0 };
                let (minimum, maximum) = regular_polygon_xy_bounds(radius, *fragments);
                Bounds {
                    min: Vec3::new(minimum[0], minimum[1], minimum_z),
                    max: Vec3::new(maximum[0], maximum[1], minimum_z + height),
                }
            }
            Shape::Transform { shape, transform } => {
                let corners = shape
                    .bounds()
                    .corners()
                    .map(|point| Transform::point(transform.forward, point));
                corners.into_iter().skip(1).fold(
                    Bounds {
                        min: corners[0],
                        max: corners[0],
                    },
                    |bounds, point| {
                        bounds.union(Bounds {
                            min: point,
                            max: point,
                        })
                    },
                )
            }
            Shape::Offset2d { shape, delta, .. } => {
                let bounds = shape.bounds();
                Bounds {
                    min: Vec3::new(bounds.min.x - delta, bounds.min.y - delta, 0.0),
                    max: Vec3::new(bounds.max.x + delta, bounds.max.y + delta, 0.0),
                }
            }
            Shape::LinearExtrude {
                shape,
                height,
                center,
                twist,
                scale,
                ..
            } => {
                let bounds = shape.bounds();
                let minimum_z = if *center { -height / 2.0 } else { 0.0 };
                // A twisted profile sweeps every point around the origin, so
                // the only bound that holds for all of it is the circumscribed
                // radius; a scaled one reaches out by its largest factor.
                let grow = scale[0].abs().max(scale[1].abs()).max(1.0);
                let (min_x, max_x, min_y, max_y) = if *twist == 0.0 {
                    (
                        bounds.min.x.min(bounds.min.x * grow),
                        bounds.max.x.max(bounds.max.x * grow),
                        bounds.min.y.min(bounds.min.y * grow),
                        bounds.max.y.max(bounds.max.y * grow),
                    )
                } else {
                    let radius = bounds
                        .min
                        .x
                        .abs()
                        .max(bounds.max.x.abs())
                        .hypot(bounds.min.y.abs().max(bounds.max.y.abs()))
                        * grow;
                    (-radius, radius, -radius, radius)
                };
                Bounds {
                    min: Vec3::new(min_x, min_y, minimum_z),
                    max: Vec3::new(max_x, max_y, minimum_z + height),
                }
            }
            Shape::MinkowskiDilation {
                shape,
                offset,
                radius,
                ..
            } => {
                let bounds = shape.bounds();
                let z_radius = if shape.dimension() == ShapeDimension::Planar {
                    0.0
                } else {
                    *radius
                };
                let expansion = Vec3::new(*radius, *radius, z_radius);
                Bounds {
                    min: bounds.min.add(*offset).sub(expansion),
                    max: bounds.max.add(*offset).add(expansion),
                }
            }
            Shape::Hull { bounds, .. } => *bounds,
            Shape::Union { bounds, .. } => *bounds,
            Shape::Difference { base, .. } => base.bounds(),
            Shape::Intersection(shapes) => shapes
                .iter()
                .skip(1)
                .fold(shapes[0].bounds(), |bounds, shape| {
                    bounds.intersection(shape.bounds())
                }),
        }
    }

    fn hull_vertices(shapes: &[Shape]) -> Result<Vec<Vec3>, EngineError> {
        let mut points = Vec::new();
        for shape in shapes {
            shape.append_hull_vertices(&mut points)?;
        }
        deduplicate_points(&mut points);
        Ok(points)
    }

    fn convex_hull_from_points(points: &[Vec3]) -> Result<Shape, EngineError> {
        let bounds = point_bounds(points)
            .ok_or_else(|| EngineError::new("hull() requires at least one supported 3D child."))?;
        let planes = convex_support_planes(points);
        if planes.len() < 4 {
            return Err(EngineError::new(
                "hull() child vertices do not span a 3D volume.",
            ));
        }
        let bounds_distance_scale = hull_bounds_distance_scale(&planes, points);
        Ok(Shape::Hull {
            planes,
            bounds,
            bounds_distance_scale,
        })
    }

    fn append_hull_vertices(&self, output: &mut Vec<Vec3>) -> Result<(), EngineError> {
        match self {
            Shape::Box { .. } => output.extend(self.bounds().corners()),
            Shape::Transform { shape, transform } => {
                let mut local = Vec::new();
                shape.append_hull_vertices(&mut local)?;
                output.extend(
                    local
                        .into_iter()
                        .map(|point| Transform::point(transform.forward, point)),
                );
            }
            Shape::Union { children, .. } => {
                for child in children {
                    child.shape.append_hull_vertices(output)?;
                }
            }
            Shape::Color { shape, .. } => shape.append_hull_vertices(output)?,
            _ => {
                return Err(EngineError::new(
                    "hull() currently supports cubes and unions/transforms of cubes.",
                ));
            }
        }
        Ok(())
    }
}

fn point_bounds(points: &[Vec3]) -> Option<Bounds> {
    let first = *points.first()?;
    Some(points.iter().copied().skip(1).fold(
        Bounds {
            min: first,
            max: first,
        },
        |bounds, point| {
            bounds.union(Bounds {
                min: point,
                max: point,
            })
        },
    ))
}

fn deduplicate_points(points: &mut Vec<Vec3>) {
    let Some(bounds) = point_bounds(points) else {
        return;
    };
    let span = bounds.max.sub(bounds.min);
    let tolerance = span.x.max(span.y).max(span.z).max(1.0) * 1e-10;
    let mut unique = Vec::with_capacity(points.len());
    for point in points.drain(..) {
        if unique
            .iter()
            .all(|existing: &Vec3| existing.sub(point).length() > tolerance)
        {
            unique.push(point);
        }
    }
    *points = unique;
}

/// Safety margin absorbing the `side_tolerance` slack that
/// [`convex_support_planes`] allows in each plane offset.
const HULL_SCALE_SAFETY: f64 = 1.0 - 1e-6;

/// How far `Shape::Hull`'s max-over-planes value can fall below the distance to
/// the hull's bounding box.
///
/// Write `H = {x : n_i·x <= o_i}` and `v(p) = max_i(n_i·p - o_i)`. Pick any
/// interior point `c`, let `ρ = min_i(o_i - n_i·c)` (the inradius about `c`)
/// and `R = max{|x - c| : x ∈ H}` (the circumradius about `c`).
///
/// Every plane offset by `v` outward satisfies `o_i + v <= (o_i - n_i·c)(1 +
/// v/ρ) + n_i·c`, so the outward-offset polytope `H_v` is contained in `c + (1
/// + v/ρ)(H - c)`. Each point of that dilation is within `(v/ρ)R` of the
/// matching point of `H`, so `dist(y, H) <= v·R/ρ` for every `y ∈ H_v`. Since
/// `p ∈ H_{v(p)}` by construction, `v(p) >= (ρ/R)·dist(p, H)`, and `dist(p, H)`
/// is at least the distance to any box enclosing `H`.
///
/// `c` is the centroid of the hull's own vertices, which is interior by
/// convexity, and `R` is measured against those same vertices.
fn hull_bounds_distance_scale(planes: &[HullPlane], points: &[Vec3]) -> f64 {
    if points.is_empty() {
        return 0.0;
    }
    let centroid = points
        .iter()
        .fold(Vec3::default(), |total, point| total.add(*point))
        .mul(1.0 / points.len() as f64);
    let inradius = planes.iter().fold(f64::INFINITY, |radius, plane| {
        radius.min(plane.offset - plane.normal.dot(centroid))
    });
    let circumradius = points.iter().fold(0.0f64, |radius, point| {
        radius.max(point.sub(centroid).length())
    });
    if !inradius.is_finite() || inradius <= 0.0 || !(circumradius > 0.0) {
        return 0.0;
    }
    (inradius / circumradius * HULL_SCALE_SAFETY).clamp(0.0, 1.0)
}

fn convex_support_planes(points: &[Vec3]) -> Vec<HullPlane> {
    let Some(bounds) = point_bounds(points) else {
        return Vec::new();
    };
    let span = bounds.max.sub(bounds.min);
    let scale = span.x.max(span.y).max(span.z).max(1.0);
    let side_tolerance = scale * 1e-9;
    let area_tolerance = scale * scale * 1e-12;
    let mut planes: Vec<HullPlane> = Vec::new();

    for first in 0..points.len() {
        for second in first + 1..points.len() {
            for third in second + 1..points.len() {
                let cross = points[second]
                    .sub(points[first])
                    .cross(points[third].sub(points[first]));
                let length = cross.length();
                if length <= area_tolerance {
                    continue;
                }
                let mut normal = cross.mul(1.0 / length);
                let mut offset = normal.dot(points[first]);
                let (minimum, maximum) = points.iter().fold(
                    (f64::INFINITY, f64::NEG_INFINITY),
                    |(minimum, maximum), point| {
                        let distance = normal.dot(*point) - offset;
                        (minimum.min(distance), maximum.max(distance))
                    },
                );
                if maximum <= side_tolerance {
                    // `normal` already points away from the other vertices.
                } else if minimum >= -side_tolerance {
                    normal = normal.mul(-1.0);
                    offset = -offset;
                } else {
                    continue;
                }
                if planes.iter().any(|plane| {
                    plane.normal.dot(normal) > 1.0 - 1e-9
                        && (plane.offset - offset).abs() <= side_tolerance
                }) {
                    continue;
                }
                planes.push(HullPlane { normal, offset });
            }
        }
    }
    planes
}

fn rectangle_distance(x: f64, y: f64, half_width: f64, half_height: f64) -> f64 {
    let dx = x.abs() - half_width;
    let dy = y.abs() - half_height;
    dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0)
}

fn regular_polygon_distance(x: f64, y: f64, radius: f64, fragments: usize) -> f64 {
    if radius <= 1e-12 {
        return x.hypot(y);
    }
    let sector = std::f64::consts::TAU / fragments as f64;
    let angle = y.atan2(x).rem_euclid(std::f64::consts::TAU);
    let edge_index = (angle / sector).floor();
    let start_angle = edge_index * sector;
    let end_angle = start_angle + sector;
    let start = [radius * start_angle.cos(), radius * start_angle.sin()];
    let end = [radius * end_angle.cos(), radius * end_angle.sin()];
    let edge = [end[0] - start[0], end[1] - start[1]];
    let relative = [x - start[0], y - start[1]];
    let edge_length_squared = edge[0] * edge[0] + edge[1] * edge[1];
    let amount =
        ((relative[0] * edge[0] + relative[1] * edge[1]) / edge_length_squared).clamp(0.0, 1.0);
    let distance = (relative[0] - edge[0] * amount).hypot(relative[1] - edge[1] * amount);
    let edge_normal_angle = start_angle + sector / 2.0;
    let signed_plane =
        x * edge_normal_angle.cos() + y * edge_normal_angle.sin() - radius * (sector / 2.0).cos();
    distance * if signed_plane <= 0.0 { -1.0 } else { 1.0 }
}

fn regular_polygon_xy_bounds(radius: f64, fragments: usize) -> ([f64; 2], [f64; 2]) {
    let sector = std::f64::consts::TAU / fragments as f64;
    let sampled = |target: f64| {
        let angle = (target / sector).round() * sector;
        [radius * angle.cos(), radius * angle.sin()]
    };
    let positive_x = sampled(0.0)[0];
    let negative_x = sampled(std::f64::consts::PI)[0];
    let positive_y = sampled(std::f64::consts::FRAC_PI_2)[1];
    let negative_y = sampled(3.0 * std::f64::consts::FRAC_PI_2)[1];
    ([negative_x, negative_y], [positive_x, positive_y])
}

fn sphere_latitude_segments(fragments: usize) -> usize {
    fragments.div_ceil(2).max(2)
}

fn spherical_point(radius: f64, polar: f64, azimuth: f64) -> Vec3 {
    let radial = radius * polar.sin();
    Vec3::new(
        radial * azimuth.cos(),
        radial * azimuth.sin(),
        radius * polar.cos(),
    )
}

fn triangle_support_plane(a: Vec3, b: Vec3, c: Vec3) -> HullPlane {
    let mut normal = b.sub(a).cross(c.sub(a)).normalized();
    let centroid = a.add(b).add(c).mul(1.0 / 3.0);
    if normal.dot(centroid) < 0.0 {
        normal = normal.mul(-1.0);
    }
    HullPlane {
        normal,
        offset: normal.dot(a),
    }
}

fn point_triangle_distance(point: Vec3, a: Vec3, b: Vec3, c: Vec3) -> f64 {
    let ab = b.sub(a);
    let ac = c.sub(a);
    let ap = point.sub(a);
    let d1 = ab.dot(ap);
    let d2 = ac.dot(ap);
    if d1 <= 0.0 && d2 <= 0.0 {
        return ap.length();
    }

    let bp = point.sub(b);
    let d3 = ab.dot(bp);
    let d4 = ac.dot(bp);
    if d3 >= 0.0 && d4 <= d3 {
        return bp.length();
    }

    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let amount = d1 / (d1 - d3);
        return point.sub(a.add(ab.mul(amount))).length();
    }

    let cp = point.sub(c);
    let d5 = ab.dot(cp);
    let d6 = ac.dot(cp);
    if d6 >= 0.0 && d5 <= d6 {
        return cp.length();
    }

    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let amount = d2 / (d2 - d6);
        return point.sub(a.add(ac.mul(amount))).length();
    }

    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && d4 - d3 >= 0.0 && d5 - d6 >= 0.0 {
        let amount = (d4 - d3) / ((d4 - d3) + (d5 - d6));
        return point.sub(b.add(c.sub(b).mul(amount))).length();
    }

    let normal = ab.cross(ac).normalized();
    normal.dot(ap).abs()
}

fn sphere_cell_triangles(
    radius: f64,
    fragments: usize,
    latitude_segments: usize,
    latitude: usize,
    longitude: usize,
) -> ([Vec3; 3], Option<[Vec3; 3]>) {
    let polar_step = std::f64::consts::PI / latitude_segments as f64;
    let azimuth_step = std::f64::consts::TAU / fragments as f64;
    let polar0 = latitude as f64 * polar_step;
    let polar1 = (latitude + 1) as f64 * polar_step;
    let azimuth0 = longitude as f64 * azimuth_step;
    let azimuth1 = (longitude + 1) as f64 * azimuth_step;
    let p00 = spherical_point(radius, polar0, azimuth0);
    let p01 = spherical_point(radius, polar0, azimuth1);
    let p10 = spherical_point(radius, polar1, azimuth0);
    let p11 = spherical_point(radius, polar1, azimuth1);
    if latitude == 0 {
        ([p00, p10, p11], None)
    } else if latitude + 1 == latitude_segments {
        ([p00, p10, p01], None)
    } else {
        ([p00, p10, p11], Some([p00, p11, p01]))
    }
}

fn faceted_sphere_distance(point: Vec3, radius: f64, fragments: usize) -> f64 {
    let latitude_segments = sphere_latitude_segments(fragments);
    let polar_step = std::f64::consts::PI / latitude_segments as f64;
    let azimuth_step = std::f64::consts::TAU / fragments as f64;
    let length = point.length();
    let polar = if length <= 1e-15 {
        0.0
    } else {
        (point.z / length).clamp(-1.0, 1.0).acos()
    };
    let azimuth = point.y.atan2(point.x).rem_euclid(std::f64::consts::TAU);
    let latitude = ((polar / polar_step).floor() as usize).min(latitude_segments - 1);
    let longitude = ((azimuth / azimuth_step).floor() as usize).min(fragments - 1);
    let mut distance = f64::INFINITY;
    let mut maximum_plane_distance = f64::NEG_INFINITY;
    let latitude_start = latitude.saturating_sub(1);
    let latitude_end = (latitude + 1).min(latitude_segments - 1);
    for candidate_latitude in latitude_start..=latitude_end {
        for longitude_delta in -1..=1 {
            let candidate_longitude =
                (longitude as isize + longitude_delta).rem_euclid(fragments as isize) as usize;
            let (first, second) = sphere_cell_triangles(
                radius,
                fragments,
                latitude_segments,
                candidate_latitude,
                candidate_longitude,
            );
            let plane = triangle_support_plane(first[0], first[1], first[2]);
            maximum_plane_distance =
                maximum_plane_distance.max(plane.normal.dot(point) - plane.offset);
            distance = distance.min(point_triangle_distance(point, first[0], first[1], first[2]));
            if let Some(second) = second {
                let plane = triangle_support_plane(second[0], second[1], second[2]);
                maximum_plane_distance =
                    maximum_plane_distance.max(plane.normal.dot(point) - plane.offset);
                distance = distance.min(point_triangle_distance(
                    point, second[0], second[1], second[2],
                ));
            }
        }
    }
    let inside = maximum_plane_distance <= 1e-12;
    distance * if inside { -1.0 } else { 1.0 }
}

fn faceted_sphere_bounds(radius: f64, fragments: usize) -> Bounds {
    let latitude_segments = sphere_latitude_segments(fragments);
    let latitude_scale = if latitude_segments.is_multiple_of(2) {
        1.0
    } else {
        (std::f64::consts::PI / (2.0 * latitude_segments as f64)).cos()
    };
    let (minimum, maximum) = regular_polygon_xy_bounds(radius * latitude_scale, fragments);
    Bounds {
        min: Vec3::new(minimum[0], minimum[1], -radius),
        max: Vec3::new(maximum[0], maximum[1], radius),
    }
}

/// Signed distance to a set of closed contours under the even-odd fill rule.
///
/// The unsigned distance is the closest edge across *all* contours, so a hole's
/// boundary correctly pushes the surface inward; the sign comes from the total
/// crossing parity, which is what makes the second and later `paths` cut holes.
fn polygon_distance(contours: &[Vec<[f64; 2]>], point: [f64; 2]) -> f64 {
    let mut squared_distance = f64::INFINITY;
    let mut inside = false;
    for points in contours {
        for index in 0..points.len() {
            let start = points[index];
            let end = points[(index + 1) % points.len()];
            let edge = [end[0] - start[0], end[1] - start[1]];
            let relative = [point[0] - start[0], point[1] - start[1]];
            let edge_length_squared = edge[0] * edge[0] + edge[1] * edge[1];
            if edge_length_squared > 0.0 {
                let amount = ((relative[0] * edge[0] + relative[1] * edge[1])
                    / edge_length_squared)
                    .clamp(0.0, 1.0);
                let nearest = [
                    relative[0] - edge[0] * amount,
                    relative[1] - edge[1] * amount,
                ];
                squared_distance =
                    squared_distance.min(nearest[0] * nearest[0] + nearest[1] * nearest[1]);
            }
            if (start[1] > point[1]) != (end[1] > point[1])
                && point[0]
                    < (end[0] - start[0]) * (point[1] - start[1]) / (end[1] - start[1]) + start[0]
            {
                inside = !inside;
            }
        }
    }
    squared_distance.sqrt() * if inside { -1.0 } else { 1.0 }
}

fn mesh_shape(
    shape: &Shape,
    resolution: usize,
    cancellation: Option<&AtomicBool>,
) -> Result<Mesh, EngineError> {
    let raw_bounds = shape.bounds();
    let span = raw_bounds.max.sub(raw_bounds.min);
    let maximum_span = span.x.max(span.y).max(span.z);
    if !maximum_span.is_finite() || maximum_span <= 0.0 || maximum_span > 1_000_000.0 {
        return Err(EngineError::new(
            "Model bounds are invalid or exceed 1,000,000 units.",
        ));
    }
    let padding = maximum_span * 0.04 + 0.01;
    let bounds = Bounds {
        min: raw_bounds.min.sub(Vec3::new(padding, padding, padding)),
        max: raw_bounds.max.add(Vec3::new(padding, padding, padding)),
    };
    let span = bounds.max.sub(bounds.min);
    let step = maximum_span / resolution as f64;
    let nx = ((span.x / step).ceil() as usize).clamp(4, resolution + 10);
    let ny = ((span.y / step).ceil() as usize).clamp(4, resolution + 10);
    let nz = ((span.z / step).ceil() as usize).clamp(4, resolution + 10);
    let dx = span.x / nx as f64;
    let dy = span.y / ny as f64;
    let dz = span.z / nz as f64;
    let index = |x: usize, y: usize, z: usize| (z * (ny + 1) + y) * (nx + 1) + x;
    let field = sample_distance_field(shape, bounds, [nx, ny, nz], [dx, dy, dz], cancellation)?;

    const TETRAHEDRA: [[usize; 4]; 6] = [
        [0, 5, 1, 6],
        [0, 1, 2, 6],
        [0, 2, 3, 6],
        [0, 3, 7, 6],
        [0, 7, 4, 6],
        [0, 4, 5, 6],
    ];
    let mut triangles = Vec::new();
    for z in 0..nz {
        if z % 3 == 0 {
            check_cancelled(cancellation)?;
        }
        for y in 0..ny {
            for x in 0..nx {
                let points = [
                    Vec3::new(
                        bounds.min.x + x as f64 * dx,
                        bounds.min.y + y as f64 * dy,
                        bounds.min.z + z as f64 * dz,
                    ),
                    Vec3::new(
                        bounds.min.x + (x + 1) as f64 * dx,
                        bounds.min.y + y as f64 * dy,
                        bounds.min.z + z as f64 * dz,
                    ),
                    Vec3::new(
                        bounds.min.x + (x + 1) as f64 * dx,
                        bounds.min.y + (y + 1) as f64 * dy,
                        bounds.min.z + z as f64 * dz,
                    ),
                    Vec3::new(
                        bounds.min.x + x as f64 * dx,
                        bounds.min.y + (y + 1) as f64 * dy,
                        bounds.min.z + z as f64 * dz,
                    ),
                    Vec3::new(
                        bounds.min.x + x as f64 * dx,
                        bounds.min.y + y as f64 * dy,
                        bounds.min.z + (z + 1) as f64 * dz,
                    ),
                    Vec3::new(
                        bounds.min.x + (x + 1) as f64 * dx,
                        bounds.min.y + y as f64 * dy,
                        bounds.min.z + (z + 1) as f64 * dz,
                    ),
                    Vec3::new(
                        bounds.min.x + (x + 1) as f64 * dx,
                        bounds.min.y + (y + 1) as f64 * dy,
                        bounds.min.z + (z + 1) as f64 * dz,
                    ),
                    Vec3::new(
                        bounds.min.x + x as f64 * dx,
                        bounds.min.y + (y + 1) as f64 * dy,
                        bounds.min.z + (z + 1) as f64 * dz,
                    ),
                ];
                let values = [
                    field[index(x, y, z)],
                    field[index(x + 1, y, z)],
                    field[index(x + 1, y + 1, z)],
                    field[index(x, y + 1, z)],
                    field[index(x, y, z + 1)],
                    field[index(x + 1, y, z + 1)],
                    field[index(x + 1, y + 1, z + 1)],
                    field[index(x, y + 1, z + 1)],
                ];
                if values.iter().all(|value| *value > 0.0)
                    || values.iter().all(|value| *value <= 0.0)
                {
                    continue;
                }
                let gradient = cell_gradient(&values, dx, dy, dz);
                for tetrahedron in TETRAHEDRA {
                    polygonize_tetrahedron(&points, &values, tetrahedron, gradient, &mut triangles);
                }
            }
        }
    }
    Ok(Mesh { triangles })
}

fn sample_distance_field(
    shape: &Shape,
    bounds: Bounds,
    cells: [usize; 3],
    spacing: [f64; 3],
    cancellation: Option<&AtomicBool>,
) -> Result<Vec<f64>, EngineError> {
    let [nx, ny, nz] = cells;
    let [dx, dy, dz] = spacing;
    let layer_size = (nx + 1) * (ny + 1);
    let mut field = vec![0.0f64; layer_size * (nz + 1)];
    let worker_count = std::thread::available_parallelism()
        .map(|workers| workers.get())
        .unwrap_or(1)
        .min(4)
        .min(nz + 1);
    let layers_per_worker = (nz + 1).div_ceil(worker_count);

    std::thread::scope(|scope| -> Result<(), EngineError> {
        let mut workers = Vec::with_capacity(worker_count);
        for (chunk_index, layers) in field.chunks_mut(layer_size * layers_per_worker).enumerate() {
            let start_z = chunk_index * layers_per_worker;
            workers.push(scope.spawn(move || -> Result<(), EngineError> {
                for (local_z, layer) in layers.chunks_mut(layer_size).enumerate() {
                    check_cancelled(cancellation)?;
                    let z = start_z + local_z;
                    for y in 0..=ny {
                        for x in 0..=nx {
                            let point = Vec3::new(
                                bounds.min.x + x as f64 * dx,
                                bounds.min.y + y as f64 * dy,
                                bounds.min.z + z as f64 * dz,
                            );
                            layer[y * (nx + 1) + x] = shape.distance(point);
                        }
                    }
                }
                Ok(())
            }));
        }
        for worker in workers {
            worker.join().map_err(|_| {
                EngineError::new("A native distance-field worker terminated unexpectedly.")
            })??;
        }
        Ok(())
    })?;
    Ok(field)
}

fn cell_gradient(values: &[f64; 8], dx: f64, dy: f64, dz: f64) -> Vec3 {
    Vec3::new(
        (values[1] + values[2] + values[5] + values[6]
            - values[0]
            - values[3]
            - values[4]
            - values[7])
            / (4.0 * dx),
        (values[2] + values[3] + values[6] + values[7]
            - values[0]
            - values[1]
            - values[4]
            - values[5])
            / (4.0 * dy),
        (values[4] + values[5] + values[6] + values[7]
            - values[0]
            - values[1]
            - values[2]
            - values[3])
            / (4.0 * dz),
    )
}

fn polygonize_tetrahedron(
    cube_points: &[Vec3; 8],
    cube_values: &[f64; 8],
    indices: [usize; 4],
    gradient: Vec3,
    output: &mut Vec<Triangle>,
) {
    let mut inside = [0usize; 4];
    let mut outside = [0usize; 4];
    let mut inside_count = 0;
    let mut outside_count = 0;
    for index in indices {
        if cube_values[index] <= 0.0 {
            inside[inside_count] = index;
            inside_count += 1;
        } else {
            outside[outside_count] = index;
            outside_count += 1;
        }
    }
    let inside = &inside[..inside_count];
    let outside = &outside[..outside_count];
    let interpolate = |a: usize, b: usize| {
        let value_a = cube_values[a];
        let value_b = cube_values[b];
        let denominator = value_a - value_b;
        let amount = if denominator.abs() < 1e-12 {
            0.5
        } else {
            (value_a / denominator).clamp(0.0, 1.0)
        };
        cube_points[a].add(cube_points[b].sub(cube_points[a]).mul(amount))
    };
    match inside_count {
        0 | 4 => {}
        1 => add_triangle(
            [
                interpolate(inside[0], outside[0]),
                interpolate(inside[0], outside[1]),
                interpolate(inside[0], outside[2]),
            ],
            gradient,
            output,
        ),
        3 => add_triangle(
            [
                interpolate(outside[0], inside[0]),
                interpolate(outside[0], inside[1]),
                interpolate(outside[0], inside[2]),
            ],
            gradient,
            output,
        ),
        2 => {
            let a = interpolate(inside[0], outside[0]);
            let b = interpolate(inside[0], outside[1]);
            let c = interpolate(inside[1], outside[0]);
            let d = interpolate(inside[1], outside[1]);
            add_triangle([a, b, d], gradient, output);
            add_triangle([a, d, c], gradient, output);
        }
        _ => unreachable!(),
    }
}

fn add_triangle(mut vertices: [Vec3; 3], gradient: Vec3, output: &mut Vec<Triangle>) {
    let edge_a = vertices[1].sub(vertices[0]);
    let edge_b = vertices[2].sub(vertices[0]);
    let mut normal = edge_a.cross(edge_b);
    if normal.length() <= 1e-10 {
        return;
    }
    if normal.dot(gradient) < 0.0 {
        vertices.swap(1, 2);
        normal = vertices[1]
            .sub(vertices[0])
            .cross(vertices[2].sub(vertices[0]));
    }
    output.push(Triangle {
        vertices,
        normal: normal.normalized(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evaluate_source(source: &str) -> Result<Shape, EngineError> {
        let statements = parse_resolved_program(source)?;
        Evaluator::default().evaluate(&statements)
    }

    /// Point transformed by whatever `rotate(...)` the source applies, read
    /// straight off the resulting transform matrix.
    fn rotated_point(source: &str, point: Vec3) -> Vec3 {
        let shape = evaluate_source(source).expect("rotate should evaluate");
        let Shape::Transform { transform, .. } = &shape else {
            panic!("rotate should produce a transform, got {shape:?}");
        };
        Transform::point(transform.forward, point)
    }

    fn assert_close(actual: Vec3, expected: Vec3) {
        let error = actual.sub(expected).length();
        assert!(
            error < 1e-9,
            "expected {expected:?}, got {actual:?} (error {error})"
        );
    }

    #[test]
    fn rotate_accepts_a_positional_euler_vector() {
        // +X spun 45 deg about Z.
        assert_close(
            rotated_point("rotate([0,0,45]) cube(1);", Vec3::new(1.0, 0.0, 0.0)),
            Vec3::new(std::f64::consts::FRAC_1_SQRT_2, std::f64::consts::FRAC_1_SQRT_2, 0.0),
        );
    }

    #[test]
    fn rotate_binds_the_euler_vector_by_its_name() {
        // `a = [...]` used to miss the "v" lookup and silently rotate by zero.
        assert_close(
            rotated_point("rotate(a=[0,0,90]) cube(1);", Vec3::new(1.0, 0.0, 0.0)),
            Vec3::new(0.0, 1.0, 0.0),
        );
        assert_close(
            rotated_point("rotate(a=[90,0,0]) cube(1);", Vec3::new(0.0, 1.0, 0.0)),
            Vec3::new(0.0, 0.0, 1.0),
        );
    }

    #[test]
    fn rotate_with_a_scalar_angle_spins_about_z() {
        // This used to hard-error with "Expected a 2- or 3-vector."
        assert_close(
            rotated_point("rotate(90) cube(1);", Vec3::new(1.0, 0.0, 0.0)),
            Vec3::new(0.0, 1.0, 0.0),
        );
        assert_close(
            rotated_point("rotate(a=90) cube(1);", Vec3::new(1.0, 0.0, 0.0)),
            Vec3::new(0.0, 1.0, 0.0),
        );
    }

    #[test]
    fn rotate_supports_axis_angle() {
        // 90 deg about +X maps +Y to +Z, and about -X maps +Y to -Z.
        assert_close(
            rotated_point("rotate(a=90, v=[1,0,0]) cube(1);", Vec3::new(0.0, 1.0, 0.0)),
            Vec3::new(0.0, 0.0, 1.0),
        );
        assert_close(
            rotated_point("rotate(90, [-1,0,0]) cube(1);", Vec3::new(0.0, 1.0, 0.0)),
            Vec3::new(0.0, 0.0, -1.0),
        );
        // A non-axis-aligned axis: 120 deg about [1,1,1] cycles X -> Y -> Z.
        assert_close(
            rotated_point("rotate(a=120, v=[1,1,1]) cube(1);", Vec3::new(1.0, 0.0, 0.0)),
            Vec3::new(0.0, 1.0, 0.0),
        );
        // The axis need not be normalized.
        assert_close(
            rotated_point("rotate(a=90, v=[0,0,7]) cube(1);", Vec3::new(1.0, 0.0, 0.0)),
            Vec3::new(0.0, 1.0, 0.0),
        );
    }

    #[test]
    fn rotate_euler_order_is_x_then_y_then_z() {
        // Rx(90) sends +Z to -Y; Rz(90) then sends -Y to +X.
        assert_close(
            rotated_point("rotate([90,0,90]) cube(1);", Vec3::new(0.0, 0.0, 1.0)),
            Vec3::new(1.0, 0.0, 0.0),
        );
    }

    #[test]
    fn rotate_distinguishes_a_scalar_from_a_one_element_vector() {
        // `rotate(45)` spins about Z, but `rotate([45])` is Euler with the
        // missing components zero, i.e. about X.
        assert_close(
            rotated_point("rotate(45) cube(1);", Vec3::new(0.0, 0.0, 1.0)),
            Vec3::new(0.0, 0.0, 1.0),
        );
        assert_close(
            rotated_point("rotate([90]) cube(1);", Vec3::new(0.0, 1.0, 0.0)),
            Vec3::new(0.0, 0.0, 1.0),
        );
    }

    #[test]
    fn rotate_with_a_degenerate_axis_is_the_identity_not_nan() {
        let point = rotated_point("rotate(a=90, v=[0,0,0]) cube(1);", Vec3::new(1.0, 2.0, 3.0));
        assert_close(point, Vec3::new(1.0, 2.0, 3.0));
    }

    #[test]
    fn polygon_paths_cut_holes() {
        let source = "linear_extrude(height=1) polygon(
            points=[[0,0],[10,0],[10,10],[0,10], [3,3],[7,3],[7,7],[3,7]],
            paths=[[0,1,2,3],[4,5,6,7]]);";
        let shape = evaluate_source(source).expect("polygon with paths should evaluate");
        // Inside the ring wall.
        assert!(shape.distance(Vec3::new(1.0, 5.0, 0.5)) < 0.0);
        // Inside the hole: must be empty.
        assert!(
            shape.distance(Vec3::new(5.0, 5.0, 0.5)) > 0.0,
            "the second path must cut a hole"
        );
        // Outside the outline.
        assert!(shape.distance(Vec3::new(20.0, 5.0, 0.5)) > 0.0);
    }

    #[test]
    fn polygon_paths_support_several_holes_and_a_convexity_argument() {
        let source = "linear_extrude(height=1) polygon(
            [[0,0],[20,0],[20,10],[0,10], [2,2],[6,2],[6,8],[2,8], [14,2],[18,2],[18,8],[14,8]],
            [[0,1,2,3],[4,5,6,7],[8,9,10,11]], 10);";
        let shape = evaluate_source(source).expect("multi-hole polygon should evaluate");
        assert!(shape.distance(Vec3::new(10.0, 5.0, 0.5)) < 0.0);
        assert!(shape.distance(Vec3::new(4.0, 5.0, 0.5)) > 0.0);
        assert!(shape.distance(Vec3::new(16.0, 5.0, 0.5)) > 0.0);
    }

    #[test]
    fn polygon_without_paths_keeps_the_single_contour_behavior() {
        let shape = evaluate_source("linear_extrude(height=1) polygon([[0,0],[10,0],[0,10]]);")
            .expect("polygon should evaluate");
        assert!(shape.distance(Vec3::new(1.0, 1.0, 0.5)) < 0.0);
        assert!(shape.distance(Vec3::new(9.0, 9.0, 0.5)) > 0.0);
    }

    #[test]
    fn polygon_rejects_out_of_range_path_indices() {
        let mut evaluator = Evaluator::default();
        let statements = parse_resolved_program(
            "polygon(points=[[0,0],[1,0],[1,1]], paths=[[0,1,9]]);",
        )
        .expect("parse");
        evaluator.evaluate_objects(&statements).expect("evaluate");
        assert!(evaluator
            .diagnostics
            .iter()
            .any(|message| message.contains("outside the 3 supplied points")));
    }

    #[test]
    fn every_named_color_resolves_and_the_table_stays_binary_searchable() {
        let names: Vec<&str> = NAMED_COLORS.iter().map(|(name, _)| *name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "NAMED_COLORS must stay sorted for binary search");
        for name in names {
            parse_color_name(name)
                .unwrap_or_else(|error| panic!("{name} should resolve: {error}"));
        }
        // Spot-check a few against their CSS definitions, including the
        // British spellings and case insensitivity.
        assert_eq!(parse_color_name("Gold").unwrap(), [1.0, 215.0 / 255.0, 0.0, 1.0]);
        assert_eq!(
            parse_color_name("darkgrey").unwrap(),
            parse_color_name("darkgray").unwrap()
        );
        assert_eq!(parse_color_name("transparent").unwrap(), [0.0, 0.0, 0.0, 0.0]);
        assert!(parse_color_name("#gg0000").is_err());
        assert!(parse_color_name("#12345").is_err());
    }

    #[test]
    fn color_reaches_the_compiled_part() {
        let output = compile_parts("color(\"red\") cube(2);", Quality::Preview, None)
            .expect("compile");
        assert_eq!(output.parts[0].color, Some([1.0, 0.0, 0.0, 1.0]));
    }

    #[test]
    fn color_accepts_every_openscad_spelling() {
        let cases: [(&str, [f64; 4]); 6] = [
            ("color(\"red\")", [1.0, 0.0, 0.0, 1.0]),
            ("color(\"red\", 0.5)", [1.0, 0.0, 0.0, 0.5]),
            ("color(\"#00ff00\")", [0.0, 1.0, 0.0, 1.0]),
            ("color(\"#00f\")", [0.0, 0.0, 1.0, 1.0]),
            ("color([0.25,0.5,0.75])", [0.25, 0.5, 0.75, 1.0]),
            ("color([0.25,0.5,0.75,0.25])", [0.25, 0.5, 0.75, 0.25]),
        ];
        for (prefix, expected) in cases {
            let shape = evaluate_source(&format!("{prefix} cube(2);"))
                .unwrap_or_else(|error| panic!("{prefix} should evaluate: {error}"));
            assert_eq!(shape.stated_color(), Some(expected), "for {prefix}");
            // Color must never change the solid.
            assert!(shape.distance(Vec3::new(1.0, 1.0, 1.0)) < 0.0, "for {prefix}");
        }
    }

    #[test]
    fn color_survives_transforms_and_warns_on_unknown_names() {
        let shape = evaluate_source("color(\"blue\") translate([5,0,0]) cube(2);")
            .expect("evaluate");
        assert_eq!(shape.stated_color(), Some([0.0, 0.0, 1.0, 1.0]));

        let statements = parse_resolved_program("color(\"nosuchcolor\") cube(2);").expect("parse");
        let mut evaluator = Evaluator::default();
        let shape = evaluator.evaluate(&statements).expect("evaluate");
        assert_eq!(shape.stated_color(), None);
        assert!(evaluator
            .diagnostics
            .iter()
            .any(|message| message.contains("unknown color name")));
    }

    #[test]
    fn include_pulls_in_a_bundled_library() {
        let shape = evaluate_source("include <units.scad>\ncube(inch);").expect("include");
        // 1 inch = 25.4 mm, so the far corner is inside and 30 mm is not.
        assert!(shape.distance(Vec3::new(25.0, 25.0, 25.0)) < 0.0);
        assert!(shape.distance(Vec3::new(30.0, 1.0, 1.0)) > 0.0);
    }

    #[test]
    fn use_imports_definitions_only() {
        let shape = evaluate_source("use <shapes.scad>\nhex_prism(across_flats=10, height=4);")
            .expect("use");
        assert!(shape.distance(Vec3::new(0.0, 0.0, 2.0)) < 0.0);
        assert!(shape.distance(Vec3::new(0.0, 0.0, 10.0)) > 0.0);
    }

    #[test]
    fn every_bundled_library_module_evaluates() {
        let cases = [
            "washer(thickness=2, outer=10, inner=4);",
            "linear_extrude(height=1) regular_polygon(sides=5, radius=6);",
            "linear_extrude(height=1) hexagon(across_flats=8);",
            "hex_prism(across_flats=8, height=3, center=true);",
            "linear_extrude(height=1) rectangle_frame(size=[20,10], wall=2);",
            "tube(height=5, outer=8, inner=3, center=true);",
        ];
        for case in cases {
            evaluate_source(&format!("include <shapes.scad>\n{case}"))
                .unwrap_or_else(|error| panic!("{case} should evaluate: {error}"));
        }
        // And the unit constants are usable as plain numbers.
        evaluate_source("include <units.scad>\ncube([inch, cm, mm_to_inch(25.4)]);")
            .expect("units should evaluate");
    }

    #[test]
    fn include_rejects_anything_outside_the_bundled_allowlist() {
        for path in [
            "../../../etc/passwd",
            "/etc/passwd",
            "MCAD/../../secret.scad",
            "nonexistent.scad",
            "UNITS.SCAD",
        ] {
            let error = evaluate_source(&format!("include <{path}>\ncube(1);"))
                .expect_err("only bundled libraries may be included")
                .to_string();
            assert!(
                error.contains("not a bundled library"),
                "unexpected error for {path}: {error}"
            );
        }
    }

    #[test]
    fn include_cycles_terminate() {
        // `shapes.scad` includes `units.scad`; including both must not loop
        // and must leave both sets of definitions usable.
        let shape = evaluate_source(
            "include <units.scad>\ninclude <shapes.scad>\ntube(height=inch, outer=10, inner=5);",
        )
        .expect("include");
        assert!(shape.distance(Vec3::new(7.0, 0.0, 5.0)) < 0.0);
        assert!(shape.distance(Vec3::new(0.0, 0.0, 5.0)) > 0.0);
    }

    #[test]
    fn parses_and_meshes_a_cube() {
        let output = compile("cube([10, 12, 14], center=true);", Quality::Preview, None).unwrap();
        // Twelve, not thousands: the default path is the exact kernel, and a
        // box really is two triangles per face.
        assert_eq!(output.mesh.triangles.len(), 12);
        let stl = output.mesh.binary_stl();
        assert_eq!(stl.len(), 84 + output.mesh.triangles.len() * 50);
    }

    #[test]
    fn validation_accepts_2d_and_definition_only_sources_without_meshing() {
        assert!(validate("square([10, 20]);").is_ok());
        assert!(validate("module reusable(size=1) { cube(size); }").is_ok());
        assert!(validate("cube([1, 2, 3]);").is_ok());
        assert!(validate("cube([1, 2, 3]").is_err());
        assert!(validate("unsupported_geometry();").is_err());
    }

    #[test]
    fn multipart_compile_retains_deterministic_top_level_objects() {
        let source = r#"
          module peg() { cylinder(h=4, r=1); }
          cube([2, 3, 4]);
          translate([8, 0, 0]) peg();
        "#;
        let first = compile_parts(source, Quality::Preview, None).unwrap();
        let second = compile_parts(source, Quality::Preview, None).unwrap();

        assert_eq!(first.parts.len(), 2);
        assert_eq!(
            first
                .parts
                .iter()
                .map(|part| (part.id.as_str(), part.name.as_str()))
                .collect::<Vec<_>>(),
            vec![("object-1", "cube 1"), ("object-2", "peg 2")]
        );
        assert!(first.parts.iter().all(|part| {
            part.visible
                && part.selectable
                && part.source_range.is_none()
                && !part.mesh.triangles.is_empty()
        }));
        assert_eq!(
            first
                .parts
                .iter()
                .map(|part| part.mesh.triangles.len())
                .collect::<Vec<_>>(),
            second
                .parts
                .iter()
                .map(|part| part.mesh.triangles.len())
                .collect::<Vec<_>>()
        );
        assert_eq!(first.parts[0].bounds.min, Vec3::new(0.0, 0.0, 0.0));
        assert_eq!(first.parts[0].bounds.max, Vec3::new(2.0, 3.0, 4.0));
    }

    #[test]
    fn selected_part_mesh_validates_ids_and_uses_union_semantics() {
        let output = compile_parts(
            "cube(10); cube(10); translate([8, 0, 0]) cube(6);",
            Quality::Preview,
            None,
        )
        .unwrap();

        let empty: Vec<String> = Vec::new();
        assert!(
            mesh_selected_parts(&output.parts, &empty, Quality::Preview, None)
                .unwrap_err()
                .to_string()
                .contains("at least one")
        );
        assert!(mesh_selected_parts(
            &output.parts,
            &["object-missing".into()],
            Quality::Preview,
            None,
        )
        .unwrap_err()
        .to_string()
        .contains("Unknown selected object IDs: object-missing"));
        assert!(mesh_selected_parts(
            &output.parts,
            &["object-1".into(), "object-1".into()],
            Quality::Preview,
            None,
        )
        .unwrap_err()
        .to_string()
        .contains("Duplicate selected object ID: object-1"));

        let union = mesh_selected_parts(
            &output.parts,
            &["object-1".into(), "object-2".into()],
            Quality::Preview,
            None,
        )
        .unwrap();
        assert_eq!(union.triangles.len(), output.parts[0].mesh.triangles.len());
        assert!(
            union.triangles.len()
                < output.parts[0].mesh.triangles.len() + output.parts[1].mesh.triangles.len()
        );

        let overlapping_union = mesh_selected_parts(
            &output.parts,
            &["object-1".into(), "object-3".into()],
            Quality::Preview,
            None,
        )
        .unwrap();
        let maximum_x = overlapping_union
            .triangles
            .iter()
            .flat_map(|triangle| triangle.vertices)
            .map(|vertex| vertex.x)
            .fold(f64::NEG_INFINITY, f64::max);
        assert!(maximum_x > 13.5, "union dropped the translated object");
        let expected = compile(
            "union() { cube(10); translate([8, 0, 0]) cube(6); }",
            Quality::Preview,
            None,
        )
        .unwrap();
        assert_eq!(
            overlapping_union.triangles.len(),
            expected.mesh.triangles.len()
        );
    }

    #[test]
    fn selected_part_mesh_honors_quality_and_cancellation() {
        let preview_output = compile_parts("sphere(10);", Quality::Preview, None).unwrap();
        let render_output = compile_parts("sphere(10);", Quality::Render, None).unwrap();
        let selection = vec!["object-1".into()];
        let preview =
            mesh_selected_parts(&preview_output.parts, &selection, Quality::Preview, None).unwrap();
        let render =
            mesh_selected_parts(&render_output.parts, &selection, Quality::Render, None).unwrap();
        // The exact kernel does not sample, so quality no longer changes the
        // mesh: what a preview shows is what an export writes. The quality tag
        // still has to be honoured, which is what the guard below checks.
        assert_eq!(render.triangles.len(), preview.triangles.len());
        assert!(
            mesh_selected_parts(&preview_output.parts, &selection, Quality::Render, None,)
                .unwrap_err()
                .to_string()
                .contains("evaluated at preview quality")
        );

        let cancellation = AtomicBool::new(true);
        let error = mesh_selected_parts(
            &render_output.parts,
            &selection,
            Quality::Render,
            Some(&cancellation),
        )
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
    }

    #[test]
    fn part_clearances_distinguish_overlap_touching_tolerance_and_hidden_state() {
        let output = compile_parts(
            r#"
              cube([10, 10, 10]);
              translate([8, 0, 0]) cube([10, 10, 10]);
              translate([20, 0, 0]) cube([10, 10, 10]);
              translate([10, 0, 0]) cube([10, 10, 10]);
            "#,
            Quality::Preview,
            None,
        )
        .unwrap();

        let checks = check_part_clearances(&output.parts, 0.25, &[]).unwrap();
        let overlap = checks
            .iter()
            .find(|pair| pair.a == "object-1" && pair.b == "object-2")
            .unwrap();
        assert!(overlap.intersects);
        assert!(!overlap.within_tolerance);
        assert!(overlap.depth.is_some_and(|depth| depth > 1.9));

        let touching = checks
            .iter()
            .find(|pair| pair.a == "object-1" && pair.b == "object-4")
            .unwrap();
        assert!(!touching.intersects);
        assert!(touching.within_tolerance);
        assert!(touching.clearance <= 1e-6);

        let clear = checks
            .iter()
            .find(|pair| pair.a == "object-1" && pair.b == "object-3")
            .unwrap();
        assert!(!clear.intersects);
        assert!(!clear.within_tolerance);
        assert!((clear.clearance - 10.0).abs() <= 1e-9);

        let tolerance_checks = check_part_clearances(&output.parts, 10.0, &[]).unwrap();
        let within_tolerance = tolerance_checks
            .iter()
            .find(|pair| pair.a == "object-1" && pair.b == "object-3")
            .unwrap();
        assert!(within_tolerance.within_tolerance);

        let hidden = check_part_clearances(&output.parts, 10.0, &["object-2".into()]).unwrap();
        assert!(hidden
            .iter()
            .all(|pair| pair.a != "object-2" && pair.b != "object-2"));

        let mut coincident = output.parts[0].clone();
        coincident.id = "object-copy".into();
        let coincident_check =
            check_part_clearances(&[output.parts[0].clone(), coincident], 0.0, &[]).unwrap();
        assert!(coincident_check[0].intersects);
        assert!(coincident_check[0].depth.is_some_and(|depth| depth > 4.9));

        let mut visibility = output.parts;
        visibility[3].visible = false;
        let visible_checks = check_part_clearances(&visibility, 0.25, &[]).unwrap();
        assert!(visible_checks
            .iter()
            .all(|pair| pair.a != "object-4" && pair.b != "object-4"));
        assert!(check_part_clearances(&visibility, -0.1, &[]).is_err());
    }

    #[test]
    fn supports_mirrored_zero_apex_frusta_used_for_bipyramids() {
        let shape = evaluate_source(
            r#"
              size = 4;
              union() {
                cylinder(h=size, r1=size, r2=0, $fn=4);
                mirror([0, 0, 1]) cylinder(h=size, r1=size, r2=0, $fn=4);
              }
            "#,
        )
        .expect("one zero cylinder radius forms a valid pointed frustum");

        assert!(shape.distance(Vec3::new(0.0, 0.0, 3.0)) < 0.0);
        assert!(shape.distance(Vec3::new(0.0, 0.0, -3.0)) < 0.0);
        assert!(shape.distance(Vec3::new(0.0, 0.0, 4.5)) > 0.0);
        assert!(shape.distance(Vec3::new(0.0, 0.0, -4.5)) > 0.0);

        let error = evaluate_source("cylinder(h=4, r1=0, r2=0);")
            .expect_err("a cylinder with two zero radii has no geometry");
        assert!(error.to_string().contains("no geometry"));
    }

    #[test]
    fn approximates_the_corpus_extrusion_and_chamfer_tool_minkowski_sum() {
        let shape = evaluate_source(
            r#"
              chamfer = 0.5;
              minkowski(convexity=10) {
                linear_extrude(height=2 - 2 * chamfer, center=true, convexity=10)
                  offset(delta=-chamfer) square([4, 4], center=true);
                union() {
                  cylinder(h=chamfer, r1=chamfer, r2=0, $fn=4);
                  mirror([0, 0, 1])
                    cylinder(h=chamfer, r1=chamfer, r2=0, $fn=4);
                }
              }
            "#,
        )
        .expect("the corpus chamfer construction should evaluate");

        assert!(shape.distance(Vec3::new(1.9, 0.0, 0.0)) < 0.0);
        assert!(shape.distance(Vec3::new(2.2, 0.0, 0.0)) > 0.0);
        assert!(shape.distance(Vec3::new(0.0, 0.0, 0.9)) < 0.0);
        assert!(shape.distance(Vec3::new(0.0, 0.0, 1.2)) > 0.0);
    }

    #[test]
    fn evaluates_modules_loops_and_difference() {
        let source = r#"
          count = 6;
          module hole(a) { rotate([0,0,a]) translate([8,0,0]) cylinder(h=12, r=1.5, center=true); }
          difference() {
            cylinder(h=10, r=12, center=true);
            for (a = [0 : 360 / count : 359]) hole(a);
          }
        "#;
        let output = compile(source, Quality::Preview, None).unwrap();
        // Six holes through a faceted cylinder: enough facets to be the real
        // shape, few enough to show the mesh is exact rather than sampled.
        assert!(output.mesh.triangles.len() > 100, "{}", output.mesh.triangles.len());
        assert!(output.mesh.triangles.len() < 2000, "{}", output.mesh.triangles.len());
    }

    #[test]
    fn evaluates_if_and_else_branches() {
        let true_shape = evaluate_source("if (true) cube(10, center=true); else sphere(1);")
            .expect("true branch");
        assert!(true_shape.distance(Vec3::new(4.0, 0.0, 0.0)) < 0.0);

        let false_shape = evaluate_source("if (false) cube(2, center=true); else sphere(6);")
            .expect("false branch");
        assert!(false_shape.distance(Vec3::new(5.0, 0.0, 0.0)) < 0.0);
    }

    #[test]
    fn binds_else_to_the_nearest_if() {
        let shape = evaluate_source(
            "if (true) if (false) cube(2, center=true); else sphere(6); else cube(20, center=true);",
        )
        .expect("nested conditional");
        assert!(shape.distance(Vec3::new(5.0, 0.0, 0.0)) < 0.0);
        assert!(shape.distance(Vec3::new(7.0, 0.0, 0.0)) > 0.0);
    }

    #[test]
    fn applies_operator_precedence_and_short_circuits() {
        let shape = evaluate_source(
            "if (1 + 2 * 3 == 7 && false || true) cube(4, center=true); else sphere(1);",
        )
        .expect("precedence expression");
        assert!(shape.distance(Vec3::new(1.5, 0.0, 0.0)) < 0.0);

        evaluate_source(
            "if (false && no_such_function(1) > 0) sphere(1); else if (true || no_such_function(1) > 0) cube(2);",
        )
        .expect("short-circuited expression");
    }

    #[test]
    fn preserves_ieee_division_and_modulo_values() {
        let output = compile(
            "infinity = 1 / 0; nan = 4 % 0; cube(1);",
            Quality::Preview,
            None,
        )
        .expect("unused non-finite values do not poison geometry");
        assert!(output
            .messages
            .iter()
            .any(|message| message.contains("Non-finite arithmetic result")));
        evaluate_source("nan = 0 / 0; if (nan != nan) cube(1);")
            .expect("nan does not equal itself");
    }

    #[test]
    fn supports_else_if_unary_not_and_mixed_comparisons() {
        let shape = evaluate_source(
            "if (false) sphere(1); else if (!false == true && true > 0 && false < 1 && true != 1) cube(4, center=true); else sphere(1);",
        )
        .expect("else-if expression");
        assert!(shape.distance(Vec3::new(1.5, 0.0, 0.0)) < 0.0);
    }

    #[test]
    fn treats_undefined_values_as_false_and_isolates_branch_scope() {
        let shape = evaluate_source(
            "flag = 0; if (true) { flag = 1; } if (missing_flag) sphere(1); else if (flag == 0) cube(4, center=true);",
        )
        .expect("undefined flag and isolated scope");
        assert!(shape.distance(Vec3::new(1.5, 0.0, 0.0)) < 0.0);
    }

    #[test]
    fn supports_scientific_notation_and_signed_modulo() {
        let shape = evaluate_source(
            "distance = 1.2e1; if (-7 % 3 == -1 && 10 % 4 * 2 == 4) translate([distance, 0, 0]) cube(2, center=true);",
        )
        .expect("scientific literal and modulo");
        assert!(shape.distance(Vec3::new(12.0, 0.0, 0.0)) < 0.0);
    }

    #[test]
    fn evaluates_conditional_geometry_inside_a_loop() {
        let shape = evaluate_source(
            r#"
              for (a = [0 : 45 : 315])
                if (a % 90 == 0)
                  rotate([0, 0, a]) translate([12, 0, 0]) cube(4, center=true);
                else
                  rotate([0, 0, a]) translate([12, 0, 0]) sphere(2);
            "#,
        )
        .expect("conditional loop");
        assert!(shape.distance(Vec3::new(12.0, 0.0, 0.0)) < 0.0);
        assert!(shape.distance(Vec3::new(8.5, 8.5, 0.0)) < 0.0);
    }

    #[test]
    fn iterates_numeric_vectors() {
        let shape = evaluate_source(
            "for (a = [0, 90]) rotate([0, 0, a]) translate([12, 0, 0]) cube(2, center=true);",
        )
        .expect("vector loop");
        assert!(shape.distance(Vec3::new(12.0, 0.0, 0.0)) < 0.0);
        assert!(shape.distance(Vec3::new(0.0, 12.0, 0.0)) < 0.0);
    }

    #[test]
    fn registers_modules_in_their_lexical_scope() {
        let local_module = evaluate_source(
            "if (true) { module local_shape() { cube(4, center=true); } local_shape(); }",
        )
        .expect("branch-local module");
        assert!(local_module.distance(Vec3::new(1.5, 0.0, 0.0)) < 0.0);

        let lexical_environment = evaluate_source(
            "i = 10; module placed() { translate([i, 0, 0]) cube(2, center=true); } for (i = [2]) placed();",
        )
        .expect("definition-scope module environment");
        assert!(lexical_environment.distance(Vec3::new(10.0, 0.0, 0.0)) < 0.0);
        assert!(lexical_environment.distance(Vec3::new(2.0, 0.0, 0.0)) > 0.0);
    }

    #[test]
    fn module_calls_resolve_helpers_from_the_definition_scope() {
        let shape = evaluate_source(
            r#"
              module helper() { translate([-12, 0, 0]) cube(2, center=true); }
              module target() { helper(); }
              module caller() {
                module helper() { translate([12, 0, 0]) sphere(3); }
                target();
              }
              caller();
            "#,
        )
        .expect("target should retain its lexical helper despite caller shadowing");
        assert!(shape.distance(Vec3::new(-12.0, 0.0, 0.0)) < 0.0);
        assert!(shape.distance(Vec3::new(12.0, 0.0, 0.0)) > 0.0);
    }

    #[test]
    fn applies_the_last_assignment_throughout_a_scope() {
        let shape = evaluate_source("size = 2; cube(size, center=true); size = 8;")
            .expect("scope assignment prepass");
        assert!(shape.distance(Vec3::new(3.0, 0.0, 0.0)) < 0.0);
    }

    #[test]
    fn forwards_children_through_nested_modules_without_recursing() {
        let shape = evaluate_source(
            r#"
              module relay() { children(); }
              module wrapper() { relay() children(); }
              wrapper() cube(4, center=true);
            "#,
        )
        .expect("forwarded children should resolve against the caller context");
        assert!(shape.distance(Vec3::new(1.5, 0.0, 0.0)) < 0.0);
    }

    #[test]
    fn seeds_standard_special_variables_for_preview_and_render() {
        let source =
            "if ($fn == 0 && $fa == 12 && $fs == 2 && $t == 0 && $preview) cube(2, center=true);";
        let shape = evaluate_source(source).expect("default preview special variables");
        assert!(shape.distance(Vec3::default()) < 0.0);

        let tokens = Lexer::new("if (!$preview) sphere(2);").tokenize().unwrap();
        let statements = Parser::new(tokens).parse_program().unwrap();
        let mut evaluator = Evaluator {
            preview: false,
            ..Evaluator::default()
        };
        evaluator
            .evaluate(&statements)
            .expect("render-mode $preview should be false");
    }

    #[test]
    fn statement_assert_and_echo_wrap_geometry_and_report_messages() {
        let tokens = Lexer::new(r#"echo("ready", answer=42) assert(true, "ok") cube(2);"#)
            .tokenize()
            .unwrap();
        let statements = Parser::new(tokens).parse_program().unwrap();
        let mut evaluator = Evaluator::default();
        evaluator
            .evaluate(&statements)
            .expect("echo and successful assert should preserve child geometry");
        assert!(evaluator
            .diagnostics
            .iter()
            .any(|message| message.contains("ECHO: \"ready\", answer = 42")));

        let error = evaluate_source(r#"assert(false, "boom") cube(1);"#).unwrap_err();
        assert!(error.to_string().contains("boom"));
    }

    #[test]
    fn invalid_geometry_warns_and_does_not_abort_valid_siblings() {
        let tokens =
            Lexer::new("sphere(0); cube(undef); scale([1, 0, 1]) cube(20); cube(2, center=true);")
                .tokenize()
                .unwrap();
        let statements = Parser::new(tokens).parse_program().unwrap();
        let mut evaluator = Evaluator::default();
        let shape = evaluator
            .evaluate(&statements)
            .expect("invalid geometry should be dropped while valid siblings remain");
        assert!(shape.distance(Vec3::default()) < 0.0);
        assert!(
            evaluator
                .diagnostics
                .iter()
                .filter(|message| message.contains("Ignoring invalid"))
                .count()
                >= 3
        );
    }

    #[test]
    fn bundled_tube_bores_through_its_whole_height() {
        // Regression: the bore used to be translated to -height, so a
        // non-centred tube stayed solid near its top face.
        let shape = evaluate_source("include <shapes.scad>\ntube(height=10, outer=8, inner=3);")
            .expect("tube should evaluate");
        for z in [0.5, 5.0, 9.5] {
            assert!(
                shape.distance(Vec3::new(0.0, 0.0, z)) > 0.0,
                "the bore must be empty at z={z}"
            );
            assert!(
                shape.distance(Vec3::new(5.5, 0.0, z)) < 0.0,
                "the wall must be solid at z={z}"
            );
        }
        // `washer` delegates to `tube`, so it inherits the fix.
        let shape = evaluate_source("include <shapes.scad>\nwasher(thickness=4, outer=8, inner=3);")
            .expect("washer should evaluate");
        assert!(shape.distance(Vec3::new(0.0, 0.0, 3.5)) > 0.0);
        assert!(shape.distance(Vec3::new(5.5, 0.0, 3.5)) < 0.0);
    }

    #[test]
    fn the_outermost_root_modifier_wins_over_a_nested_one() {
        // OpenSCAD promotes the outermost `!`, not the first one an evaluator
        // happens to reach; the inner `!` is then an ordinary node inside it.
        let shape = evaluate_source("!translate([10,0,0]) { !cube(2, center=true); }")
            .expect("nested root should parse");
        assert!(
            shape.distance(Vec3::new(10.0, 0.0, 0.0)) < 0.0,
            "the outer ! keeps its translate"
        );
        assert!(
            shape.distance(Vec3::default()) > 0.0,
            "the inner ! must not steal the root"
        );
    }

    #[test]
    fn the_first_of_two_sibling_root_modifiers_wins() {
        let shape = evaluate_source("!cube(2, center=true); !translate([10,0,0]) cube(2, center=true);")
            .expect("sibling roots should parse");
        assert!(shape.distance(Vec3::default()) < 0.0);
        assert!(shape.distance(Vec3::new(10.0, 0.0, 0.0)) > 0.0);
    }

    #[test]
    fn an_undef_rotate_axis_reads_as_no_axis_given() {
        // Forwarding an unset module parameter yields `undef`. Treating that
        // as a type error used to delete the children's geometry outright,
        // so `rotate(a, v)` wrappers silently rendered nothing.
        let shape = evaluate_source(
            "module spin(a, v) { rotate(a, v) children(); }\nspin(90) translate([10,0,0]) cube(2, center=true);",
        )
        .expect("undef axis should fall back to a Z rotation");
        assert!(shape.distance(Vec3::new(0.0, 10.0, 0.0)) < 0.0);
        assert!(shape.distance(Vec3::new(10.0, 0.0, 0.0)) > 0.0);
        // The same wrapper still honours an axis when one is supplied.
        let shape = evaluate_source(
            "module spin(a, v) { rotate(a, v) children(); }\nspin(90, [1,0,0]) translate([0,10,0]) cube(2, center=true);",
        )
        .expect("an explicit axis should still apply");
        assert!(shape.distance(Vec3::new(0.0, 0.0, 10.0)) < 0.0);
    }

    #[test]
    fn color_alpha_without_a_color_is_reported_rather_than_dropped() {
        let statements = parse_resolved_program("color(alpha=0.5) cube(2);").expect("parse");
        let mut evaluator = Evaluator::default();
        let shape = evaluator.evaluate(&statements).expect("evaluate");
        // Geometry is untouched either way.
        assert!(shape.distance(Vec3::new(1.0, 1.0, 1.0)) < 0.0);
        assert_eq!(shape.stated_color(), None);
        assert!(evaluator
            .diagnostics
            .iter()
            .any(|message| message.contains("alpha without a color")));
    }

    #[test]
    fn an_unusable_color_alpha_never_drops_the_children() {
        let statements =
            parse_resolved_program("color(\"red\", alpha=\"opaque\") cube(2);").expect("parse");
        let mut evaluator = Evaluator::default();
        let shape = evaluator.evaluate(&statements).expect("evaluate");
        assert!(shape.distance(Vec3::new(1.0, 1.0, 1.0)) < 0.0);
        assert_eq!(shape.stated_color(), Some([1.0, 0.0, 0.0, 1.0]));
        assert!(evaluator
            .diagnostics
            .iter()
            .any(|message| message.contains("alpha ignored")));
    }

    #[test]
    fn accepts_bare_blocks_and_geometry_modifiers() {
        // `!` overrides everything: only the marked subtree survives, so the
        // origin cube, the background sphere and the disabled sphere are all
        // gone and just the translated cube remains.
        let shape = evaluate_source(
            "{ #cube(2, center=true); %sphere(1); !translate([4,0,0]) cube(2, center=true); *sphere(100); }",
        )
        .expect("blocks and modifiers should parse");
        assert!(shape.distance(Vec3::default()) > 0.0);
        assert!(shape.distance(Vec3::new(4.0, 0.0, 0.0)) < 0.0);
        assert!(shape.distance(Vec3::new(50.0, 0.0, 0.0)) > 0.0);
    }

    #[test]
    fn highlight_modifier_keeps_its_subtree_in_the_geometry() {
        let shape = evaluate_source("#cube(2, center=true);").expect("highlight should parse");
        assert!(shape.distance(Vec3::default()) < 0.0);
    }

    #[test]
    fn background_modifier_removes_its_subtree_from_the_csg_result() {
        // The `%` sphere would swallow the cube if it were unioned in.
        let shape = evaluate_source("cube(2, center=true); %sphere(20);")
            .expect("background should parse");
        assert!(shape.distance(Vec3::default()) < 0.0);
        assert!(
            shape.distance(Vec3::new(10.0, 0.0, 0.0)) > 0.0,
            "a %-modified sphere must not contribute geometry"
        );
        // And it must not resurface through a difference either.
        let shape = evaluate_source("difference() { cube(10, center=true); %cube(30, center=true); }")
            .expect("background should parse");
        assert!(shape.distance(Vec3::default()) < 0.0);
    }

    #[test]
    fn background_subtrees_are_reported_separately_and_excluded_from_parts() {
        let output = compile_parts(
            "cube(2, center=true); %translate([20,0,0]) cube(2, center=true);",
            Quality::Preview,
            None,
        )
        .expect("compile");
        assert_eq!(output.parts.len(), 1);
        assert_eq!(output.background_parts.len(), 1);
        assert!(output.background_parts[0].background);
        assert!(!output.background_parts[0].selectable);
        // The whole-model mesh must not reach the background object.
        assert!(output
            .mesh
            .triangles
            .iter()
            .flat_map(|triangle| triangle.vertices)
            .all(|vertex| vertex.x < 10.0));
    }

    #[test]
    fn root_modifier_discards_the_transformations_above_it() {
        // OpenSCAD promotes the marked node to the root, so the enclosing
        // translate does not apply to it.
        let shape = evaluate_source("translate([100,0,0]) !cube(2, center=true);")
            .expect("root should parse");
        assert!(shape.distance(Vec3::default()) < 0.0);
        assert!(shape.distance(Vec3::new(100.0, 0.0, 0.0)) > 0.0);
    }

    #[test]
    fn enforces_a_global_evaluation_work_budget() {
        let tokens = Lexer::new("cube(1);").tokenize().unwrap();
        let statements = Parser::new(tokens).parse_program().unwrap();
        let mut evaluator = Evaluator {
            work_steps: MAX_EVALUATION_WORK_STEPS,
            ..Evaluator::default()
        };
        let error = evaluator.evaluate(&statements).unwrap_err();
        assert!(error.to_string().contains("global work limit"));
    }

    #[test]
    fn function_defaults_are_evaluated_in_the_caller_scope() {
        let shape = evaluate_source(
            r#"
              fallback = 2;
              function use_default(value = fallback) = value;
              module caller() {
                fallback = 7;
                if (use_default() == 7) cube(2, center=true);
              }
              caller();
            "#,
        )
        .expect("caller-scoped function default");
        assert!(shape.distance(Vec3::default()) < 0.0);
    }

    #[test]
    fn resolves_assignment_dependencies_and_same_scope_functions_declaratively() {
        let shape = evaluate_source(
            r#"
              first = second;
              second = final_position;
              final_position = 3;
              final_position = 13;

              function even(n) = n == 0 ? true : odd(n - 1);
              function odd(n) = n == 0 ? false : even(n - 1);
              function captured() = first;

              if (even(12) && !odd(12) && captured() == 13)
                translate([first, 0, 0]) cube(2, center=true);
            "#,
        )
        .expect("forward value dependencies and mutually recursive functions");
        assert!(shape.distance(Vec3::new(13.0, 0.0, 0.0)) < 0.0);
        assert!(shape.distance(Vec3::new(3.0, 0.0, 0.0)) > 0.0);
    }

    #[test]
    fn function_calls_use_dynamic_specials_named_binding_and_shadow_safe_tco() {
        let shape = evaluate_source(
            r#"
              $factor = 2;
              function scaled(value) = value * $factor;
              function invoke(invoke, value) = invoke(value);
              plus_three = function (value) value + 3;

              if (scaled(4) == 8 &&
                  scaled(4, $factor=5) == 20 &&
                  invoke(plus_three, 4) == 7)
                cube(2, center=true);
            "#,
        )
        .expect("dynamic specials and a parameter shadowing the function name");
        assert!(shape.distance(Vec3::default()) < 0.0);
    }

    #[test]
    fn named_builtin_arguments_follow_the_spec_signatures() {
        let shape = evaluate_source(
            r#"
              samples = rands(count=4, max=1, min=0);
              samples_copy = samples;
              matches = search(index_col_num=0,
                               target=[[1, 9], [2, 8], [2, 7]],
                               match=[2],
                               num_returns_per_match=0);
              if (len(samples) == 4 && samples == samples_copy && matches == [[1, 2]])
                cube(2, center=true);
            "#,
        )
        .expect("named rands() and search() arguments");
        assert!(shape.distance(Vec3::default()) < 0.0);
    }

    #[test]
    fn positional_module_arguments_bind_before_named_overrides() {
        let shape = evaluate_source(
            r#"
              module marker(x, y=20) {
                translate([x, y, 0]) cube(2, center=true);
              }
              marker(3, x=9);
            "#,
        )
        .expect("named arguments override their parameter without shifting positional slots");
        assert!(shape.distance(Vec3::new(9.0, 20.0, 0.0)) < 0.0);
        assert!(shape.distance(Vec3::new(9.0, 3.0, 0.0)) > 0.0);
    }

    #[test]
    fn reports_unsupported_modules() {
        let error = compile("torus(10);", Quality::Preview, None).unwrap_err();
        assert!(error.to_string().contains("Unsupported module torus"));
    }

    #[test]
    fn lexes_non_ascii_characters_after_a_backslash_without_panicking() {
        let tokens = Lexer::new(r#""\é""#)
            .tokenize()
            .expect("unknown escapes preserve their Unicode character");
        assert_eq!(tokens, vec![Token::String("é".to_string())]);
    }

    #[test]
    fn rejects_hostile_expression_nesting_before_the_native_stack_is_exhausted() {
        let nested_sources = [
            format!("value = {}1{};", "(".repeat(1_000), ")".repeat(1_000)),
            format!("value = {}1{};", "[".repeat(1_000), "]".repeat(1_000)),
            format!("value = {}1;", "-".repeat(1_000)),
        ];
        for source in nested_sources {
            let tokens = Lexer::new(&source).tokenize().expect("valid tokens");
            let error = Parser::new(tokens)
                .parse_program()
                .expect_err("excessive nesting must be rejected");
            assert!(
                error.to_string().contains("Expression nesting depth"),
                "unexpected parser error: {error}"
            );
        }
    }

    #[test]
    fn invalid_and_reversed_ranges_produce_warnings() {
        let invalid = compile("values = [\"bad\":2]; cube(1);", Quality::Preview, None)
            .expect("a range type error should not stop compilation");
        assert!(invalid.messages.iter().any(|message| {
            message.contains("Range endpoints and step must be numbers")
                && message.contains("returning undef")
        }));

        let reversed = compile("values = [5:1]; cube(1);", Quality::Preview, None)
            .expect("a reversed positive range is an empty range");
        assert!(reversed
            .messages
            .iter()
            .any(|message| message.contains("Deprecated reversed range")));
    }

    #[test]
    fn supports_matrix_products_recursive_scaling_and_expression_wrappers() {
        let source = r#"
            matrix = [[1, 2], [3, 4]];
            wrapped = echo("matrix", named=matrix)
                      assert(true, "matrix assertion")
                      let (a=2, b=a+3) b;
            if (wrapped == 5 &&
                matrix * [5, 6] == [17, 39] &&
                [5, 6] * matrix == [23, 34] &&
                matrix * [[5, 6], [7, 8]] == [[19, 22], [43, 50]] &&
                2 * matrix == [[2, 4], [6, 8]] &&
                matrix / 2 == [[0.5, 1], [1.5, 2]])
                cube(2, center=true);
        "#;
        let result = compile(source, Quality::Preview, None)
            .expect("matrix arithmetic and wrapping expressions should compile");
        assert!(result
            .messages
            .iter()
            .any(|message| { message.contains("ECHO: \"matrix\", named = [[1, 2], [3, 4]]") }));

        let tokens = Lexer::new(source).tokenize().unwrap();
        let statements = Parser::new(tokens).parse_program().unwrap();
        let mut evaluator = Evaluator::default();
        let shape = evaluator.evaluate(&statements).unwrap();
        assert!(shape.distance(Vec3::default()) < 0.0);

        let error = evaluate_source(r#"value = assert(false, "detail") 1; cube(value);"#)
            .expect_err("a failed expression assertion should retain its message");
        assert!(error.to_string().contains("detail"));
    }

    #[test]
    fn accepts_trailing_commas_in_vector_literals() {
        let shape =
            evaluate_source(r#"values = [1, 2, [3, 4,],]; if (values == [1, 2, [3, 4]]) cube(1);"#)
                .expect("trailing vector commas are valid");
        assert!(shape.distance(Vec3::new(0.5, 0.5, 0.5)) <= 0.0);
    }

    #[test]
    fn evaluates_planar_square_polygon_and_two_component_translate() {
        let shape = evaluate_source(
            r#"
                union() {
                    square([4, 2], center=true);
                    translate([6, 0]) polygon([[0, -1], [3, 0], [0, 1]]);
                }
            "#,
        )
        .expect("2D primitives and a two-component translation");
        assert_eq!(shape.dimension(), ShapeDimension::Planar);
        assert!(shape.distance(Vec3::new(1.5, 0.0, 0.0)) < 0.0);
        assert!(shape.distance(Vec3::new(7.0, 0.0, 0.0)) < 0.0);
        assert!(shape.distance(Vec3::new(10.0, 0.0, 0.0)) > 0.0);
    }

    #[test]
    fn extrudes_planar_differences_with_through_holes() {
        let shape = evaluate_source(
            r#"
                linear_extrude(height=4, center=true, convexity=10)
                    difference() {
                        square(10, center=true);
                        square(4, center=true);
                    }
            "#,
        )
        .expect("extruded planar difference");
        assert_eq!(shape.dimension(), ShapeDimension::Solid);
        assert!(shape.distance(Vec3::new(0.0, 0.0, 0.0)) > 0.0);
        assert!(shape.distance(Vec3::new(3.0, 0.0, 0.0)) < 0.0);
        assert!(shape.distance(Vec3::new(3.0, 0.0, 2.5)) > 0.0);
    }

    #[test]
    fn erodes_planar_geometry_with_negative_offset_before_extrusion() {
        let shape =
            evaluate_source("linear_extrude(height=2) offset(delta=-1) square(10, center=true);")
                .expect("negative planar offset");
        assert!(shape.distance(Vec3::new(3.9, 0.0, 1.0)) < 0.0);
        assert!(shape.distance(Vec3::new(4.1, 0.0, 1.0)) > 0.0);
    }

    #[test]
    fn rejects_unextruded_planar_output_and_mixed_dimension_csg() {
        let planar = compile("square(5);", Quality::Preview, None).unwrap_err();
        assert!(planar.to_string().contains("no 3D geometry"));

        let mixed = evaluate_source("union() { square(5); cube(5); }").unwrap_err();
        assert!(mixed.to_string().contains("cannot mix 2D and 3D"));
    }

    fn unpruned_union_distance(shape: &Shape, point: Vec3) -> f64 {
        match shape {
            Shape::Union { children, .. } => children
                .iter()
                .map(|child| child.shape.distance(point))
                .fold(f64::INFINITY, f64::min),
            shape => shape.distance(point),
        }
    }

    #[test]
    fn flattens_nested_unions_and_preserves_translated_cube_distances() {
        let shape = evaluate_source(
            r#"
                union() {
                    translate([-20, 0, 0]) cube(4, center=true);
                    union() {
                        cube(6, center=true);
                        translate([20, 0, 0]) cube(8, center=true);
                    }
                }
            "#,
        )
        .expect("nested cube union");
        let Shape::Union { children, .. } = &shape else {
            panic!("three children should remain a union");
        };
        assert_eq!(children.len(), 3, "nested unions must be flattened once");

        for x in -30..=30 {
            for y in [-5.0, 0.0, 5.0] {
                let point = Vec3::new(x as f64, y, 0.25);
                assert_eq!(
                    shape.distance(point),
                    unpruned_union_distance(&shape, point),
                    "pruning changed the translated-cube distance at {point:?}"
                );
            }
        }
    }

    #[test]
    fn nonuniform_transform_bounds_never_prune_a_closer_child() {
        let shape = evaluate_source(
            r#"
                union() {
                    translate([139, 0, 0]) sphere(1, $fn=4);
                    scale([100, 1, 1]) sphere(1, $fn=4);
                }
            "#,
        )
        .expect("nonuniform scaled sphere union");
        let point = Vec3::new(150.0, 0.0, 0.0);
        assert_eq!(
            shape.distance(point),
            unpruned_union_distance(&shape, point)
        );
        assert!((shape.distance(point) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn large_union_bvh_preserves_randomized_signs_and_distances() {
        let shape = evaluate_source(
            r#"
                union() {
                    for (x = [0:19])
                        translate([x * 4 - 38, 0, 0]) cube(2, center=true);
                    translate([0, 8, 0]) scale([8, 1, 2]) sphere(1);
                    translate([0, -8, 0]) difference() {
                        cube([12, 4, 4], center=true);
                        cube([4, 6, 2], center=true);
                    }
                }
            "#,
        )
        .expect("large mixed union should evaluate");
        let Shape::Union { bvh, .. } = &shape else {
            panic!("mixed children should form a union");
        };
        assert!(bvh.is_some(), "large unions should prepare a BVH");

        let mut random = 0x9e37_79b9_7f4a_7c15u64;
        let mut next_coordinate = || {
            random = random
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (random >> 11) as f64 / ((1u64 << 53) as f64) * 100.0 - 50.0
        };
        for _ in 0..2_000 {
            let point = Vec3::new(
                next_coordinate(),
                next_coordinate() / 4.0,
                next_coordinate() / 8.0,
            );
            let prepared = shape.distance(point);
            let unprepared = unpruned_union_distance(&shape, point);
            assert_eq!(
                prepared <= 0.0,
                unprepared <= 0.0,
                "sign changed at {point:?}"
            );
            assert_eq!(
                prepared.to_bits(),
                unprepared.to_bits(),
                "distance changed at {point:?}"
            );
        }
    }

    #[test]
    #[ignore = "corpus-scale microbenchmark; run explicitly with --ignored --nocapture"]
    fn benchmark_many_translated_cubes_union_distance() {
        use std::time::Instant;

        let mut source = String::from("union() {");
        for row in 0..20 {
            for column in 0..20 {
                source.push_str(&format!(
                    "translate([{}, {}, 0]) cube(1, center=true);",
                    column * 3,
                    row * 3
                ));
            }
        }
        source.push('}');
        let shape = evaluate_source(&source).expect("400 translated cubes");
        let Shape::Union { children, .. } = &shape else {
            panic!("many cubes should produce a union");
        };
        assert_eq!(children.len(), 400);
        let points = (0..48)
            .flat_map(|y| (0..48).map(move |x| Vec3::new(x as f64 * 1.25, y as f64 * 1.25, 0.0)))
            .collect::<Vec<_>>();

        let accelerated_started = Instant::now();
        let accelerated = points
            .iter()
            .map(|point| shape.distance(*point))
            .sum::<f64>();
        let accelerated_elapsed = accelerated_started.elapsed();

        let baseline_started = Instant::now();
        let baseline = points
            .iter()
            .map(|point| unpruned_union_distance(&shape, *point))
            .sum::<f64>();
        let baseline_elapsed = baseline_started.elapsed();

        assert!((accelerated - baseline).abs() < 1e-9);
        eprintln!(
            "400-cube/{}-sample union: accelerated={accelerated_elapsed:?}, unpruned={baseline_elapsed:?}",
            points.len()
        );
    }

    #[test]
    fn hulls_centered_boxes_into_a_chamfered_block() {
        let shape = evaluate_source(
            r#"
                hull() {
                    cube([10, 6, 6], center=true);
                    cube([6, 10, 6], center=true);
                    cube([6, 6, 10], center=true);
                }
            "#,
        )
        .expect("the multipart mascot's chamfered block hull should evaluate");

        assert!(shape.distance(Vec3::new(3.5, 3.5, 0.0)) < 0.0);
        assert!(shape.distance(Vec3::new(4.5, 4.5, 0.0)) > 0.0);
        assert!(shape.distance(Vec3::new(0.0, 0.0, 4.5)) < 0.0);
        let bounds = shape.bounds();
        assert_eq!(bounds.min, Vec3::new(-5.0, -5.0, -5.0));
        assert_eq!(bounds.max, Vec3::new(5.0, 5.0, 5.0));
    }

    #[test]
    fn hulls_transformed_station_cubes_into_a_snap_taper() {
        let shape = evaluate_source(
            r#"
                module station(width, z) {
                    translate([-width / 2, -1, z]) cube([width, 2, 0.04]);
                }
                hull() {
                    station(4, 0);
                    station(6, 3);
                }
            "#,
        )
        .expect("the multipart snap station hull should evaluate");

        assert!(shape.distance(Vec3::new(2.4, 0.0, 1.5)) < 0.0);
        assert!(shape.distance(Vec3::new(2.8, 0.0, 1.5)) > 0.0);
        assert!(shape.distance(Vec3::new(0.0, 0.0, 3.02)) < 0.0);
        let bounds = shape.bounds();
        assert_eq!(bounds.min, Vec3::new(-3.0, -1.0, 0.0));
        assert_eq!(bounds.max, Vec3::new(3.0, 1.0, 3.04));
    }

    #[test]
    fn rejects_curved_hull_operands_until_they_are_implemented() {
        let error = evaluate_source("hull() { cube(2); sphere(2); }")
            .expect_err("curved hull operands must not silently produce incorrect geometry");
        assert!(error
            .to_string()
            .contains("supports cubes and unions/transforms of cubes"));
    }

    #[test]
    fn caches_repeated_identical_hulls_within_one_compilation() {
        let source = r#"
            module chamfered() {
                hull() {
                    cube([10, 6, 6], center=true);
                    cube([6, 10, 6], center=true);
                    cube([6, 6, 10], center=true);
                }
            }
            for (x = [0:39]) translate([x * 12, 0, 0]) chamfered();
        "#;
        let tokens = Lexer::new(source).tokenize().unwrap();
        let statements = Parser::new(tokens).parse_program().unwrap();
        let mut evaluator = Evaluator::default();
        let shape = evaluator
            .evaluate(&statements)
            .expect("repeated chamfer hulls should evaluate");

        assert_eq!(evaluator.hull_cache.len(), 1);
        assert!(shape.distance(Vec3::new(0.0, 0.0, 0.0)) < 0.0);
        assert!(shape.distance(Vec3::new(39.0 * 12.0, 0.0, 0.0)) < 0.0);
    }

    #[test]
    fn parallel_field_sampling_matches_direct_dense_union_evaluation() {
        let shape = evaluate_source(
            r#"
                for (x = [0:3], y = [0:3], z = [0:3])
                    translate([x * 3, y * 3, z * 3]) cube(2, center=true);
            "#,
        )
        .expect("dense benchmark union should evaluate");
        let bounds = Bounds {
            min: Vec3::new(-2.0, -2.0, -2.0),
            max: Vec3::new(11.0, 11.0, 11.0),
        };
        let cells = [12, 10, 8];
        let spacing = [
            (bounds.max.x - bounds.min.x) / cells[0] as f64,
            (bounds.max.y - bounds.min.y) / cells[1] as f64,
            (bounds.max.z - bounds.min.z) / cells[2] as f64,
        ];
        let field = sample_distance_field(&shape, bounds, cells, spacing, None)
            .expect("parallel distance sampling should finish");
        let mut index = 0;
        for z in 0..=cells[2] {
            for y in 0..=cells[1] {
                for x in 0..=cells[0] {
                    let point = Vec3::new(
                        bounds.min.x + x as f64 * spacing[0],
                        bounds.min.y + y as f64 * spacing[1],
                        bounds.min.z + z as f64 * spacing[2],
                    );
                    assert_eq!(field[index].to_bits(), shape.distance(point).to_bits());
                    index += 1;
                }
            }
        }
    }

    #[test]
    fn cached_cell_gradient_matches_a_linear_scalar_field() {
        let dx = 0.75;
        let dy = 1.25;
        let dz = 2.5;
        let scalar = |x: f64, y: f64, z: f64| 2.0 * x - 3.0 * y + 4.0 * z + 7.0;
        let values = [
            scalar(0.0, 0.0, 0.0),
            scalar(dx, 0.0, 0.0),
            scalar(dx, dy, 0.0),
            scalar(0.0, dy, 0.0),
            scalar(0.0, 0.0, dz),
            scalar(dx, 0.0, dz),
            scalar(dx, dy, dz),
            scalar(0.0, dy, dz),
        ];
        assert_eq!(
            cell_gradient(&values, dx, dy, dz),
            Vec3::new(2.0, -3.0, 4.0)
        );
    }

    #[test]
    fn cancellation_is_observed() {
        let cancellation = AtomicBool::new(true);
        let error = compile("sphere(10);", Quality::Render, Some(&cancellation)).unwrap_err();
        assert!(error.to_string().contains("cancelled"));
    }
}

#[cfg(test)]
#[path = "engine/spec_lexical_values.rs"]
mod spec_lexical_values;

#[cfg(test)]
#[path = "engine/spec_control_modules.rs"]
mod spec_control_modules;

#[cfg(test)]
#[path = "engine/spec_builtins.rs"]
mod spec_builtins;

#[cfg(test)]
#[path = "engine/spec_curved_primitives.rs"]
mod spec_curved_primitives;

#[cfg(test)]
#[path = "engine/spec_comprehensions.rs"]
mod spec_comprehensions;

#[cfg(test)]
#[path = "engine/spec_bounds_pruning.rs"]
mod spec_bounds_pruning;

/// The exact polyhedral CSG kernel bridge.
///
/// `compile` above meshes the shape tree by sampling `Shape::distance`;
/// `exact::compile_exact` meshes the same tree exactly. The sampled path stays
/// the default so nothing regresses; callers opt in through
/// [`exact::Kernel::Exact`].
#[allow(dead_code)]
#[path = "engine/exact.rs"]
pub mod exact;

#[allow(unused_imports)]
pub use exact::{compile_exact, compile_with_kernel, Kernel};
