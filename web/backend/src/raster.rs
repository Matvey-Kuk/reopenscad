//! CPU software renderer that turns triangle meshes into PNG images.
//!
//! Agents without a browser cannot look at the WebGL viewer, so this module
//! draws the same scene on the CPU and returns PNG bytes plus the metadata an
//! agent needs to reason about the picture (camera, bounds, millimetres per
//! pixel). It has no dependencies beyond `serde`: rasterisation, shading and
//! the PNG/zlib encoder are all implemented here.
//!
//! # Pipeline
//!
//! 1. Visible objects are measured and the camera is resolved: a named view is
//!    auto-framed to the visible bounds, an explicit eye/target is used as is.
//! 2. Every triangle is transformed to view space, near-clipped (perspective),
//!    and rasterised into a supersampled depth + triangle-id buffer. Both front
//!    and back faces are drawn, so a section plane that cuts a solid open shows
//!    the inside, which is shaded as a hatched "cap" and reads as the cut face.
//! 3. A deferred pass shades each subpixel (ambient + two Lambert lights + a
//!    mild Blinn specular, flat per facet), outlines feature edges found from
//!    id/normal/depth discontinuities, overlays the ground grid and axes, and
//!    box-filters down to the output size.
//! 4. A scale label is stamped on and the image is PNG-encoded with a
//!    fixed-Huffman LZ77 deflate stream.
//!
//! # Conventions
//!
//! World space is OpenSCAD's: millimetres, Z up. A section plane removes the
//! half-space its normal points into: `dot(p - point, normal) > 0` is cut
//! away.

use crate::vec3::{add, cross, dot, norm as length, scale, sub};
use serde::{Deserialize, Deserializer, Serialize};

use crate::engine::{FaceColors, Mesh};

/// OpenSCAD's default face colour (#f9d72c).
pub const OPENSCAD_YELLOW: Rgba = [0xf9, 0xd7, 0x2c, 0xff];
/// OpenSCAD's "Cornfield" background (#ffffe5).
pub const DEFAULT_BACKGROUND: Rgba = [0xff, 0xff, 0xe5, 0xff];

/// Smallest and largest accepted output edge, in pixels.
pub const MIN_SIZE: u32 = 64;
pub const MAX_SIZE: u32 = 2048;
/// Largest accepted supersampling factor per axis.
pub const MAX_SUPERSAMPLE: u32 = 4;
/// Cap on supersampled pixels (depth + id buffers cost 8 bytes each), which
/// bounds memory at ~100 MB per render. Large images get a lower factor.
pub const MAX_SUBPIXELS: u64 = 2048 * 1536 * 4;
/// Cap on triangles across all visible objects.
pub const MAX_TRIANGLES: usize = 4_000_000;
/// Cap on objects passed in one scene.
pub const MAX_OBJECTS: usize = 10_000;
/// Cap on rasteriser work: the subpixels the triangles' covered row spans
/// visit, plus one per row touched. Normal models use a small fraction of this; it
/// stops inputs made of many huge overlapping triangles from pinning a core.
pub const MAX_RASTER_WORK: u64 = 400_000_000;

/// 8-bit sRGB colour with alpha.
pub type Rgba = [u8; 4];

/// One mesh in the scene.
#[derive(Clone, Copy, Debug)]
pub struct RenderObject<'a> {
    pub name: &'a str,
    pub mesh: &'a Mesh,
    /// Face colour; `None` draws OpenSCAD yellow. Alpha below 255 draws the
    /// object translucent (front faces blended back to front over the opaque
    /// scene, without edge outlines); alpha 0 hides it but keeps it framed.
    pub color: Option<Rgba>,
    /// Per-triangle colours of an object painted in several (`color()` inside
    /// a module or on some of a union's children), aligned with `mesh`. Each
    /// triangle then takes its own colour and alpha; `color` still names the
    /// object in the metadata. Ignored when `None`.
    pub face_colors: Option<&'a FaceColors>,
    pub visible: bool,
}

impl RenderObject<'_> {
    /// The 8-bit colour of every palette slot of [`Self::face_colors`].
    fn face_palette(&self) -> Option<Vec<Rgba>> {
        let colors = self.face_colors?;
        Some(
            colors
                .palette
                .iter()
                .map(|color| color.and_then(|color| color_from_numbers(&color)).unwrap_or(OPENSCAD_YELLOW))
                .collect(),
        )
    }
}

/// A colour accepted from JSON as `"#rgb"`, `"#rrggbb"`, `"#rrggbbaa"`, or an
/// array of 3–4 numbers. Arrays whose values are all `<= 1.0` are read as
/// OpenSCAD-style fractions, otherwise as 0–255 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Color(pub Rgba);

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Text(String),
            Numbers(Vec<f64>),
        }
        let parsed = match Raw::deserialize(deserializer)? {
            Raw::Text(text) => parse_color(&text),
            Raw::Numbers(values) => color_from_numbers(&values),
        };
        parsed
            .map(Color)
            .ok_or_else(|| serde::de::Error::custom("expected a colour like \"#f9d72c\" or [r, g, b]"))
    }
}

/// Parses `#rgb`, `#rrggbb` or `#rrggbbaa` (the `#` is optional).
pub fn parse_color(text: &str) -> Option<Rgba> {
    let hex = text.trim().trim_start_matches('#');
    let digit = |index: usize| u8::from_str_radix(hex.get(index..index + 1)?, 16).ok();
    let byte = |index: usize| u8::from_str_radix(hex.get(index..index + 2)?, 16).ok();
    match hex.len() {
        3 => Some([digit(0)? * 17, digit(1)? * 17, digit(2)? * 17, 255]),
        6 => Some([byte(0)?, byte(2)?, byte(4)?, 255]),
        8 => Some([byte(0)?, byte(2)?, byte(4)?, byte(6)?]),
        _ => None,
    }
}

/// Converts OpenSCAD `[r, g, b(, a)]` fractions (or 0–255 bytes) to [`Rgba`].
pub fn color_from_numbers(values: &[f64]) -> Option<Rgba> {
    if !(3..=4).contains(&values.len()) || values.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let fractions = values.iter().all(|value| *value <= 1.0);
    let channel = |value: f64| {
        let scaled = if fractions { value * 255.0 } else { value };
        scaled.round().clamp(0.0, 255.0) as u8
    };
    Some([
        channel(values[0]),
        channel(values[1]),
        channel(values[2]),
        values.get(3).map_or(255, |alpha| channel(*alpha)),
    ])
}

/// Preset camera directions. All are auto-framed to the visible bounds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NamedView {
    /// Three-quarter view from front-right, 35 degrees above the horizon,
    /// like OpenSCAD's startup view.
    #[default]
    #[serde(alias = "reset", alias = "diagonal")]
    Default,
    /// True isometric from the front-right-top octant (direction 1, -1, 1).
    #[serde(alias = "isometric")]
    Iso,
    /// From -Y looking toward +Y; X right, Z up.
    Front,
    /// From +Y; X left, Z up.
    Back,
    /// From -X; -Y to the right, Z up.
    Left,
    /// From +X; Y right, Z up.
    Right,
    /// From +Z looking down; X right, Y up.
    Top,
    /// From -Z looking up; X right, Y down.
    Bottom,
}

impl NamedView {
    pub fn as_str(self) -> &'static str {
        match self {
            NamedView::Default => "default",
            NamedView::Iso => "iso",
            NamedView::Front => "front",
            NamedView::Back => "back",
            NamedView::Left => "left",
            NamedView::Right => "right",
            NamedView::Top => "top",
            NamedView::Bottom => "bottom",
        }
    }

    /// Unit direction from the target toward the eye, and the screen-up hint.
    fn direction(self) -> ([f64; 3], [f64; 3]) {
        const Z: [f64; 3] = [0.0, 0.0, 1.0];
        match self {
            NamedView::Default => {
                let (elevation, azimuth) = (35f64.to_radians(), (-65f64).to_radians());
                (
                    [
                        elevation.cos() * azimuth.cos(),
                        elevation.cos() * azimuth.sin(),
                        elevation.sin(),
                    ],
                    Z,
                )
            }
            NamedView::Iso => (normalize([1.0, -1.0, 1.0]), Z),
            NamedView::Front => ([0.0, -1.0, 0.0], Z),
            NamedView::Back => ([0.0, 1.0, 0.0], Z),
            NamedView::Left => ([-1.0, 0.0, 0.0], Z),
            NamedView::Right => ([1.0, 0.0, 0.0], Z),
            NamedView::Top => ([0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
            NamedView::Bottom => ([0.0, 0.0, -1.0], [0.0, -1.0, 0.0]),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Projection {
    #[default]
    Perspective,
    #[serde(alias = "ortho")]
    Orthographic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    X,
    Y,
    Z,
}

/// Which half of an axis-aligned section is removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CutSide {
    /// Remove coordinates greater than the offset.
    Positive,
    /// Remove coordinates less than the offset.
    Negative,
}

/// A cutting plane. Geometry on one side is removed and the cut-open
/// interior is drawn as a hatched cap so the cross-section reads clearly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SectionPlane {
    /// `{"axis": "y", "offset": 0}`. Without `remove`, the half facing the
    /// camera is removed so the cut face points at the viewer.
    Axis {
        axis: Axis,
        #[serde(default)]
        offset: f64,
        #[serde(default)]
        remove: Option<CutSide>,
    },
    /// `{"point": [..], "normal": [..]}`; the half the normal points into is
    /// removed.
    Plane { point: [f64; 3], normal: [f64; 3] },
}

/// Everything about how to draw the scene. All fields have defaults, so
/// `{}` deserialises to a 1024x768 default view.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RenderOptions {
    /// Size of the model area; clamped to `MIN_SIZE..=MAX_SIZE`. The stamped
    /// label (and a caller's legend column) go in margins outside it, so the
    /// PNG can be larger: `RenderMeta::width`/`height` give its size, up to
    /// `MAX_SIZE`, and `RenderMeta::model_area` the rect the model drew in.
    /// Contact sheets are the exception: this is the size of the whole sheet,
    /// and their tiles fit in it, margins included.
    pub width: u32,
    pub height: u32,
    /// Preset direction used when `eye` is absent.
    pub view: NamedView,
    /// Explicit camera position. When set, `view` is ignored and the camera
    /// is used as given (no auto-framing, apart from orthographic scale when
    /// neither the eye distance nor `view_height_mm` is meaningful).
    pub eye: Option<[f64; 3]>,
    /// Point looked at; defaults to the centre of the visible bounds.
    pub target: Option<[f64; 3]>,
    /// Screen-up hint; defaults to +Z (+Y when looking straight down/up).
    pub up: Option<[f64; 3]>,
    pub projection: Projection,
    /// Vertical field of view for perspective, degrees (clamped 5..120).
    pub fov_deg: f64,
    /// Magnification applied after framing (2 = twice as close).
    pub zoom: f64,
    /// Orthographic only: height of the visible frame at the target, in mm.
    /// Overrides auto-framing.
    pub view_height_mm: Option<f64>,
    pub background: Option<Color>,
    pub section: Option<SectionPlane>,
    /// Outline silhouettes and feature edges.
    pub edges: bool,
    /// Draw the X (red), Y (green) and Z (blue) axes.
    pub axes: bool,
    /// Draw a ground grid on z = 0 sized to the model.
    pub grid: bool,
    /// Stamp a "grid N mm" / scale-bar label in a band under the model area.
    pub label: bool,
    /// Supersampling factor per axis (1..=4); lowered automatically for
    /// large images to respect `MAX_SUBPIXELS`.
    pub supersample: u32,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            width: 1024,
            height: 768,
            view: NamedView::Default,
            eye: None,
            target: None,
            up: None,
            projection: Projection::Perspective,
            fov_deg: 30.0,
            zoom: 1.0,
            view_height_mm: None,
            background: None,
            section: None,
            edges: true,
            axes: true,
            grid: true,
            label: true,
            supersample: 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Bounds {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

impl Bounds {
    pub fn size(&self) -> [f64; 3] {
        sub(self.max, self.min)
    }
    pub fn center(&self) -> [f64; 3] {
        scale(add(self.min, self.max), 0.5)
    }
}

/// The camera actually used, so an agent can repeat or adjust it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CameraInfo {
    /// Preset name, or `"custom"` for an explicit eye.
    pub view: String,
    pub projection: Projection,
    pub eye: [f64; 3],
    pub target: [f64; 3],
    pub up: [f64; 3],
    /// Vertical field of view (perspective only), narrowed by `zoom` under a
    /// custom eye.
    pub fov_deg: Option<f64>,
    /// Height of the visible frame in the plane through the target, mm.
    pub view_height_mm: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ObjectInfo {
    pub name: String,
    pub visible: bool,
    pub triangles: usize,
    pub color: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SectionInfo {
    pub point: [f64; 3],
    /// Unit normal pointing into the removed half.
    pub normal: [f64; 3],
}

/// The pixels of an image the model is drawn in; nothing else draws there
/// but overlays placed on the model on purpose (rings).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ModelArea {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Room a caller wants beside the model area for its own overlay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reserve {
    /// Width of a column to the right of the model area.
    pub right: u32,
    /// Least height of the whole image, for what that column has to hold.
    pub min_height: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RenderMeta {
    /// Size of the whole PNG (of the tile, in a contact sheet).
    pub width: u32,
    pub height: u32,
    /// Where the model is: every camera figure here refers to this rect.
    pub model_area: ModelArea,
    pub supersample: u32,
    pub camera: CameraInfo,
    /// Bounds of all visible objects (before sectioning); `None` if empty.
    pub visible_bounds: Option<Bounds>,
    /// Millimetres per pixel in the plane through the target (exact
    /// everywhere for orthographic, at the target depth for perspective).
    pub mm_per_pixel: f64,
    pub grid_spacing_mm: Option<f64>,
    pub section: Option<SectionInfo>,
    pub triangles_drawn: usize,
    pub objects: Vec<ObjectInfo>,
}

#[derive(Clone, Debug)]
pub struct RenderOutput {
    pub png: Vec<u8>,
    pub meta: RenderMeta,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenderError {
    TooManyObjects(usize),
    TooManyTriangles(usize),
    /// The scene exceeded `MAX_RASTER_WORK` even at supersample 1 (many large
    /// overlapping triangles).
    TooComplex,
    InvalidCamera(String),
    InvalidSection(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::TooManyObjects(count) => {
                write!(formatter, "{count} objects exceed the limit of {MAX_OBJECTS}")
            }
            RenderError::TooManyTriangles(count) => write!(
                formatter,
                "{count} visible triangles exceed the render limit of {MAX_TRIANGLES}"
            ),
            RenderError::TooComplex => write!(
                formatter,
                "the visible triangles cover more than {MAX_RASTER_WORK} subpixels of rasteriser work even without supersampling (typically thousands of large, overlapping triangles); reduce the image size or hide objects"
            ),
            RenderError::InvalidCamera(reason) => write!(formatter, "invalid camera: {reason}"),
            RenderError::InvalidSection(reason) => write!(formatter, "invalid section plane: {reason}"),
        }
    }
}

impl std::error::Error for RenderError {}

// ---------------------------------------------------------------------------
// Vector helpers
// ---------------------------------------------------------------------------

fn normalize(a: [f64; 3]) -> [f64; 3] {
    let len = length(a);
    if len > 0.0 {
        scale(a, 1.0 / len)
    } else {
        a
    }
}
fn finite(a: [f64; 3]) -> bool {
    a.iter().all(|value| value.is_finite())
}

fn hex(color: Rgba) -> String {
    format!("#{:02x}{:02x}{:02x}", color[0], color[1], color[2])
}

// ---------------------------------------------------------------------------
// Camera
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Camera {
    eye: [f64; 3],
    right: [f64; 3],
    up: [f64; 3],
    forward: [f64; 3],
    perspective: bool,
    /// Perspective: focal length in subpixels. Orthographic: subpixels per mm.
    focal: f64,
    cx: f64,
    cy: f64,
    near: f64,
}

impl Camera {
    fn world_to_view(&self, point: [f64; 3]) -> [f64; 3] {
        let relative = sub(point, self.eye);
        [dot(relative, self.right), dot(relative, self.up), dot(relative, self.forward)]
    }

    fn rotate(&self, direction: [f64; 3]) -> [f64; 3] {
        [dot(direction, self.right), dot(direction, self.up), dot(direction, self.forward)]
    }

    /// Screen position (subpixels) and the interpolated depth attribute.
    fn project(&self, view: [f64; 3]) -> [f64; 3] {
        if self.perspective {
            let inv = 1.0 / view[2];
            [
                self.cx + view[0] * self.focal * inv,
                self.cy - view[1] * self.focal * inv,
                inv,
            ]
        } else {
            [self.cx + view[0] * self.focal, self.cy - view[1] * self.focal, view[2]]
        }
    }

    /// View-space ray through a subpixel centre: (origin, direction) where the
    /// direction has unit forward component, so the ray parameter equals depth.
    fn ray(&self, sx: f64, sy: f64) -> ([f64; 3], [f64; 3]) {
        if self.perspective {
            ([0.0; 3], [(sx - self.cx) / self.focal, -(sy - self.cy) / self.focal, 1.0])
        } else {
            ([(sx - self.cx) / self.focal, -(sy - self.cy) / self.focal, 0.0], [0.0, 0.0, 1.0])
        }
    }

    fn view_to_world_dir(&self, v: [f64; 3]) -> [f64; 3] {
        add(add(scale(self.right, v[0]), scale(self.up, v[1])), scale(self.forward, v[2]))
    }

    /// World size of one subpixel at a given depth.
    fn footprint(&self, depth: f64) -> f64 {
        if self.perspective {
            depth.max(self.near) / self.focal
        } else {
            1.0 / self.focal
        }
    }
}

fn basis(eye_direction: [f64; 3], up_hint: [f64; 3]) -> Option<([f64; 3], [f64; 3], [f64; 3])> {
    let forward = normalize(scale(eye_direction, -1.0));
    let mut right = cross(forward, up_hint);
    if length(right) < 1e-9 {
        // Looking along the hint: fall back to +Y (or +X) as screen-up.
        let fallback = if forward[1].abs() < 0.9 { [0.0, 1.0, 0.0] } else { [1.0, 0.0, 0.0] };
        right = cross(forward, fallback);
    }
    let right = normalize(right);
    let up = cross(right, forward);
    if !finite(forward) || !finite(right) || length(forward) < 0.5 {
        return None;
    }
    Some((right, up, forward))
}

// ---------------------------------------------------------------------------
// Rasteriser
// ---------------------------------------------------------------------------

const NO_TRIANGLE: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct TriInfo {
    /// View-space unit normal (front side) and plane offset `dot(n, p)`.
    normal: [f32; 3],
    offset: f32,
    object: u32,
    /// Index into the scene's colour table: the object's, or the triangle's
    /// own when the object is painted per face.
    color: u32,
    back: bool,
}

struct Target {
    width: usize,
    height: usize,
    depth: Vec<f32>,
    id: Vec<u32>,
    /// Translucent layers composited over the opaque surface, per subpixel:
    /// premultiplied linear colour and the remaining transmittance. Only
    /// allocated when a visible object has `alpha < 255`.
    overlay: Vec<[f32; 4]>,
    work: Work,
}

/// Rasteriser work accounting: subpixels actually visited (row spans plus a
/// per-row overhead), capped at `MAX_RASTER_WORK`.
struct Work {
    left: u64,
    exhausted: bool,
    /// Work the scene asked for in total, also after the budget ran out (rows
    /// are then measured but not filled), so the caller can tell whether a
    /// lower supersampling factor would fit.
    demanded: u64,
}

struct Cut {
    /// View-space plane: removed where `dot(normal, p) + offset > 0`.
    normal: [f64; 3],
    offset: f64,
}

impl Cut {
    fn keeps(&self, camera: &Camera, x: f64, y: f64, depth: f64) -> bool {
        let (origin, dir) = camera.ray(x, y);
        let point = add(origin, scale(dir, depth));
        dot(self.normal, point) + self.offset <= 0.0
    }
}

/// Visits the subpixels a screen-space triangle (x, y, depth attribute)
/// covers, calling `fragment(index, x, y, depth)` for each one in front of the
/// camera. Only each row's covered span is walked (and charged to `work`), so
/// a long thin diagonal triangle costs its area, not its bounding box.
fn rasterize(
    work: &mut Work,
    width: usize,
    height: usize,
    camera: &Camera,
    s: [[f64; 3]; 3],
    mut fragment: impl FnMut(usize, f64, f64, f64),
) {
    let [p0, p1, p2] = s;
    let area = (p1[0] - p0[0]) * (p2[1] - p0[1]) - (p1[1] - p0[1]) * (p2[0] - p0[0]);
    if !area.is_finite() || area.abs() < 1e-12 {
        return;
    }
    let min_x = p0[0].min(p1[0]).min(p2[0]);
    let max_x = p0[0].max(p1[0]).max(p2[0]);
    let min_y = p0[1].min(p1[1]).min(p2[1]);
    let max_y = p0[1].max(p1[1]).max(p2[1]);
    let x0 = (min_x - 0.5).ceil().max(0.0);
    let x1 = (max_x - 0.5).floor().min(width as f64 - 1.0);
    let y0 = (min_y - 0.5).ceil().max(0.0);
    let y1 = (max_y - 0.5).floor().min(height as f64 - 1.0);
    if !(x0 <= x1 && y0 <= y1) {
        return;
    }
    let (x0, x1, y0, y1) = (x0 as usize, x1 as usize, y0 as usize, y1 as usize);
    let inv_area = 1.0 / area;
    // Barycentric weight of vertex k as an affine function A x + B y + C.
    let edge = |a: [f64; 3], b: [f64; 3]| {
        let ea = -(b[1] - a[1]) * inv_area;
        let eb = (b[0] - a[0]) * inv_area;
        let ec = ((b[1] - a[1]) * a[0] - (b[0] - a[0]) * a[1]) * inv_area;
        (ea, eb, ec)
    };
    let weights = [edge(p1, p2), edge(p2, p0), edge(p0, p1)];
    let attr = (
        weights[0].0 * p0[2] + weights[1].0 * p1[2] + weights[2].0 * p2[2],
        weights[0].1 * p0[2] + weights[1].1 * p1[2] + weights[2].1 * p2[2],
        weights[0].2 * p0[2] + weights[1].2 * p1[2] + weights[2].2 * p2[2],
    );
    const EPS: f64 = -1e-9;
    for y in y0..=y1 {
        let py = y as f64 + 0.5;
        // The covered span of this row: where every weight is >= EPS, solved
        // per edge, widened by one subpixel against rounding (the per-pixel
        // test below stays authoritative).
        let (mut lo, mut hi) = (x0 as f64, x1 as f64);
        for (a, b, c) in weights {
            // a * (x + 0.5) + (b * py + c) >= EPS
            let rest = b * py + c + a * 0.5;
            if a > 0.0 {
                lo = lo.max(((EPS - rest) / a).ceil() - 1.0);
            } else if a < 0.0 {
                hi = hi.min(((EPS - rest) / a).floor() + 1.0);
            } else if rest < EPS {
                hi = lo - 1.0;
            }
        }
        let lo = lo.max(x0 as f64);
        let hi = hi.min(x1 as f64);
        let span = if lo <= hi { (hi - lo) as u64 + 1 } else { 0 };
        let cost = span + 1;
        work.demanded = work.demanded.saturating_add(cost);
        if work.exhausted || cost > work.left {
            work.exhausted = true;
            work.left = 0;
            continue;
        }
        work.left -= cost;
        if span == 0 {
            continue;
        }
        let (lo, hi) = (lo as usize, hi as usize);
        let px0 = lo as f64 + 0.5;
        let [w0, w1, w2] = weights;
        let mut b0 = w0.0 * px0 + w0.1 * py + w0.2;
        let mut b1 = w1.0 * px0 + w1.1 * py + w1.2;
        let mut b2 = w2.0 * px0 + w2.1 * py + w2.2;
        let mut a = attr.0 * px0 + attr.1 * py + attr.2;
        let row = y * width;
        for x in lo..=hi {
            if b0 >= EPS && b1 >= EPS && b2 >= EPS {
                let depth = if camera.perspective { 1.0 / a } else { a };
                if depth > 0.0 || !camera.perspective {
                    fragment(row + x, x as f64 + 0.5, py, depth);
                }
            }
            b0 += w0.0;
            b1 += w1.0;
            b2 += w2.0;
            a += attr.0;
        }
    }
}

impl Target {
    /// Rasterises one opaque screen-space triangle into the depth/id buffers.
    fn triangle(&mut self, camera: &Camera, s: [[f64; 3]; 3], id: u32, cut: Option<&Cut>) {
        let Target { width, height, depth, id: ids, work, .. } = self;
        rasterize(work, *width, *height, camera, s, |index, x, y, d| {
            let d32 = d as f32;
            if d32 < depth[index] && cut.is_none_or(|cut| cut.keeps(camera, x, y, d)) {
                depth[index] = d32;
                ids[index] = id;
            }
        });
    }

    /// Composites one translucent triangle (drawn back to front) over what is
    /// already there, behind nothing opaque.
    fn translucent(&mut self, camera: &Camera, s: [[f64; 3]; 3], normal: [f64; 3], color: [f32; 3], alpha: f32, cut: Option<&Cut>) {
        let Target { width, height, depth, overlay, work, .. } = self;
        rasterize(work, *width, *height, camera, s, |index, x, y, d| {
            if (d as f32) < depth[index] && cut.is_none_or(|cut| cut.keeps(camera, x, y, d)) {
                let lit = lit_color(camera, color, normal, x, y);
                let layer = &mut overlay[index];
                for channel in 0..3 {
                    layer[channel] = layer[channel] * (1.0 - alpha) + lit[channel] * alpha;
                }
                layer[3] *= 1.0 - alpha;
            }
        });
    }
}

/// Front-face lighting: ambient + two Lambert lights + a mild Blinn specular.
fn lit_color(camera: &Camera, base: [f32; 3], normal: [f64; 3], sx: f64, sy: f64) -> [f32; 3] {
    let key = normalize([-0.35, 0.55, -0.75]);
    let fill = normalize([0.6, -0.3, -0.5]);
    let (_, dir) = camera.ray(sx, sy);
    let view = normalize(scale(dir, -1.0));
    let lambert_key = dot(normal, key).max(0.0);
    let lambert_fill = dot(normal, fill).max(0.0);
    let half = normalize(add(key, view));
    let specular = dot(normal, half).max(0.0).powi(48);
    let light = (0.28 + 0.62 * lambert_key + 0.22 * lambert_fill) as f32;
    let spec = (0.18 * specular) as f32;
    [base[0] * light + spec, base[1] * light + spec, base[2] * light + spec]
}

/// Clips a view-space polygon to `z >= near` (Sutherland–Hodgman).
fn clip_near(points: &[[f64; 3]; 3], near: f64) -> Vec<[f64; 3]> {
    let mut out = Vec::with_capacity(4);
    for index in 0..3 {
        let current = points[index];
        let next = points[(index + 1) % 3];
        let current_in = current[2] >= near;
        let next_in = next[2] >= near;
        if current_in {
            out.push(current);
        }
        if current_in != next_in {
            let t = (near - current[2]) / (next[2] - current[2]);
            out.push(add(current, scale(sub(next, current), t)));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Renders the visible objects to a PNG.
///
/// A scene whose rasterisation would exceed `MAX_RASTER_WORK` at the requested
/// supersampling is retried at the largest lower factor its measured work fits
/// (work scales with the square of the factor) before it is refused.
pub fn render(objects: &[RenderObject<'_>], options: &RenderOptions) -> Result<RenderOutput, RenderError> {
    let raw = render_pixels(objects, options, Reserve::default())?;
    Ok(RenderOutput {
        png: encode_png(raw.meta.width, raw.meta.height, &raw.pixels, raw.has_alpha),
        meta: raw.meta,
    })
}

/// [`render`] with `reserve` kept free of the model, letting `overlay` draw on
/// the finished RGBA pixels (image width, image height, metadata) before
/// encoding: legends in the reserved column, markers in `meta.model_area`.
pub fn render_with_overlay(
    objects: &[RenderObject<'_>],
    options: &RenderOptions,
    reserve: Reserve,
    overlay: impl FnOnce(&mut [u8], u32, u32, &RenderMeta),
) -> Result<RenderOutput, RenderError> {
    let mut raw = render_pixels(objects, options, reserve)?;
    overlay(&mut raw.pixels, raw.meta.width, raw.meta.height, &raw.meta);
    Ok(RenderOutput {
        png: encode_png(raw.meta.width, raw.meta.height, &raw.pixels, raw.has_alpha),
        meta: raw.meta,
    })
}

/// Image pixel position of a model point in a finished render, `None` behind
/// a perspective camera. It can fall outside `meta.model_area` when the camera
/// does not frame the point.
pub fn project_point(meta: &RenderMeta, point: [f64; 3]) -> Option<[f64; 2]> {
    let camera = &meta.camera;
    let (right, up, forward) = basis(sub(camera.eye, camera.target), camera.up)?;
    let relative = sub(point, camera.eye);
    let view = [dot(relative, right), dot(relative, up), dot(relative, forward)];
    let area = meta.model_area;
    let (w, h) = (area.width as f64, area.height as f64);
    let (cx, cy) = (area.x as f64 + w * 0.5, area.y as f64 + h * 0.5);
    match camera.fov_deg {
        Some(fov) => {
            if view[2] <= 1e-9 {
                return None;
            }
            let focal = h * 0.5 / (fov.to_radians() * 0.5).tan();
            Some([cx + view[0] * focal / view[2], cy - view[1] * focal / view[2]])
        }
        None => {
            let focal = h / camera.view_height_mm;
            Some([cx + view[0] * focal, cy - view[1] * focal])
        }
    }
}

/// Unencoded result of one render: RGBA pixels plus the metadata.
struct RenderPixels {
    pixels: Vec<u8>,
    has_alpha: bool,
    meta: RenderMeta,
}

fn render_pixels(objects: &[RenderObject<'_>], options: &RenderOptions, reserve: Reserve) -> Result<RenderPixels, RenderError> {
    if objects.len() > MAX_OBJECTS {
        return Err(RenderError::TooManyObjects(objects.len()));
    }
    // The margins grow the image, so the model keeps the requested size and
    // scale; only at MAX_SIZE do they come out of the model area.
    let right = reserve.right.min(MAX_SIZE - MIN_SIZE);
    let width = options.width.clamp(MIN_SIZE, MAX_SIZE - right);
    let band = label_band(width, options);
    let height = options.height.clamp(MIN_SIZE, MAX_SIZE - band);
    let translucent = objects
        .iter()
        .any(|object| {
            object.visible
                && match object.face_palette() {
                    Some(palette) => palette.iter().any(|color| color[3] < 255),
                    None => object.color.is_some_and(|color| color[3] < 255),
                }
        });
    // Translucency adds a 16-byte overlay per subpixel to the 8 bytes of depth
    // and id, so it gets a third of the subpixels to stay within the same memory.
    let max_subpixels = if translucent { MAX_SUBPIXELS / 3 } else { MAX_SUBPIXELS };
    let mut ss = options.supersample.clamp(1, MAX_SUPERSAMPLE);
    while ss > 1 && (width as u64 * ss as u64) * (height as u64 * ss as u64) > max_subpixels {
        ss -= 1;
    }
    loop {
        match render_at(objects, options, width, height, ss, translucent) {
            Err(Attempt::TooMuchWork { demanded }) => {
                // Work per factor squared, measured at this one; the largest
                // lower factor whose estimate fits with some margin, if any.
                let per_unit = demanded as f64 / (ss * ss) as f64;
                let fits = (1..ss).rev().find(|lower| per_unit * (*lower * *lower) as f64 <= MAX_RASTER_WORK as f64 * 0.9);
                match fits {
                    Some(lower) => ss = lower,
                    None => return Err(RenderError::TooComplex),
                }
            }
            Err(Attempt::Failed(error)) => return Err(error),
            Ok(output) => return Ok(with_margins(output, options, right, band, reserve.min_height)),
        }
    }
}

/// Moves a rendered model area into an image with the `right` column and the
/// label `band` around it, stamping the label. The margins are filled with the
/// background; the column is the caller's to draw.
fn with_margins(mut raw: RenderPixels, options: &RenderOptions, right: u32, band: u32, min_height: u32) -> RenderPixels {
    let (model_w, model_h) = (raw.meta.width as usize, raw.meta.height as usize);
    let width = model_w + right as usize;
    let height = (model_h + band as usize).max(min_height as usize).min(MAX_SIZE as usize);
    if (width, height) == (model_w, model_h) {
        return raw;
    }
    let background = options.background.map(|c| c.0).unwrap_or(DEFAULT_BACKGROUND);
    let mut pixels = Vec::with_capacity(width * height * 4);
    for _ in 0..width * height {
        pixels.extend_from_slice(&background);
    }
    for y in 0..model_h {
        let row = &raw.pixels[y * model_w * 4..(y + 1) * model_w * 4];
        pixels[y * width * 4..y * width * 4 + row.len()].copy_from_slice(row);
    }
    if band > 0 {
        let ortho = raw.meta.camera.fov_deg.is_none().then_some(raw.meta.mm_per_pixel);
        stamp_label(&mut pixels, width, height, model_w, model_h, raw.meta.grid_spacing_mm, ortho);
    }
    raw.pixels = pixels;
    raw.meta.width = width as u32;
    raw.meta.height = height as u32;
    raw
}

/// Most tiles one contact sheet holds.
pub const MAX_SHEET_VIEWS: usize = 6;

/// One tile of a contact sheet: where it sits and what it shows.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SheetTile {
    pub view: String,
    /// Top-left pixel of the tile in the sheet.
    pub x: u32,
    pub y: u32,
    pub meta: RenderMeta,
}

#[derive(Clone, Debug)]
pub struct SheetOutput {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub columns: u32,
    pub rows: u32,
    pub tiles: Vec<SheetTile>,
}

/// Columns and rows of a sheet of `count` tiles: one row up to three, then
/// two rows (2x2 for four, 3x2 for five or six).
pub fn sheet_grid(count: usize) -> (u32, u32) {
    match count {
        0 | 1 => (1, 1),
        2 => (2, 1),
        3 => (3, 1),
        4 => (2, 2),
        _ => (3, 2),
    }
}

/// Renders the scene once per named view and tiles the results into one
/// PNG of `options.width` x `options.height` (each tile auto-framed on its
/// own, with the view name on a strip above its model area, the label on a
/// band below it, and a thin separator between tiles). Every other option
/// applies to every tile.
pub fn render_sheet(
    objects: &[RenderObject<'_>],
    options: &RenderOptions,
    views: &[NamedView],
) -> Result<SheetOutput, RenderError> {
    if views.is_empty() || views.len() > MAX_SHEET_VIEWS {
        return Err(RenderError::InvalidCamera(format!(
            "views must list 1 to {MAX_SHEET_VIEWS} named views"
        )));
    }
    if options.eye.is_some() {
        return Err(RenderError::InvalidCamera("views cannot be combined with an explicit eye".into()));
    }
    let width = options.width.clamp(MIN_SIZE, MAX_SIZE);
    let height = options.height.clamp(MIN_SIZE, MAX_SIZE);
    let (columns, rows) = sheet_grid(views.len());
    let tile_width = (width / columns).max(MIN_SIZE);
    // The view name's strip comes on top of the least model height.
    let label_scale = if tile_width >= 450 { 2 } else { 1 };
    let strip = 16 * label_scale as u32;
    let tile_height = (height / rows).max(MIN_SIZE + strip);
    let (width, height) = (tile_width * columns, tile_height * rows);
    let background = options.background.map(|c| c.0).unwrap_or(DEFAULT_BACKGROUND);
    let mut sheet = vec![0u8; width as usize * height as usize * 4];
    for pixel in sheet.chunks_exact_mut(4) {
        pixel.copy_from_slice(&background);
    }
    let mut has_alpha = background[3] < 255;
    let mut tiles = Vec::with_capacity(views.len());
    for (index, view) in views.iter().enumerate() {
        let (column, row) = (index as u32 % columns, index as u32 / columns);
        let (x0, y0) = (column * tile_width, row * tile_height);
        let mut tile_options = options.clone();
        tile_options.view = *view;
        tile_options.width = tile_width;
        let room = tile_height - strip;
        // A tile too small for its label band goes without it.
        let band = label_band(tile_width, &tile_options);
        tile_options.label = options.label && room >= MIN_SIZE + band;
        tile_options.height = room.saturating_sub(label_band(tile_width, &tile_options));
        let mut raw = render_pixels(objects, &tile_options, Reserve::default())?;
        has_alpha |= raw.has_alpha;
        let (tw, th) = (raw.meta.width as usize, raw.meta.height as usize);
        for y in 0..th.min(room as usize) {
            let source = &raw.pixels[y * tw * 4..(y * tw + tw.min(tile_width as usize)) * 4];
            let offset = ((y0 as usize + strip as usize + y) * width as usize + x0 as usize) * 4;
            sheet[offset..offset + source.len()].copy_from_slice(source);
        }
        raw.meta.model_area.y = strip;
        raw.meta.height = tile_height;
        let label = view.as_str();
        let (lx, ly) = (x0 as usize + 6 * label_scale, y0 as usize + 6 * label_scale);
        let backdrop_w = label.len() * 6 * label_scale + 6 * label_scale;
        let backdrop_h = 7 * label_scale + 6 * label_scale;
        for y in (ly - 3 * label_scale)..(ly - 3 * label_scale + backdrop_h).min(height as usize) {
            for x in (lx - 3 * label_scale)..(lx - 3 * label_scale + backdrop_w).min(width as usize) {
                let offset = (y * width as usize + x) * 4;
                sheet[offset..offset + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
        draw_text(&mut sheet, width as usize, height as usize, lx, ly, label, label_scale);
        tiles.push(SheetTile {
            view: label.to_string(),
            x: x0,
            y: y0,
            meta: raw.meta,
        });
    }
    // Separators between tiles.
    let line = [150u8, 150, 150, 255];
    for column in 1..columns {
        let x = (column * tile_width) as usize;
        for y in 0..height as usize {
            let offset = (y * width as usize + x) * 4;
            sheet[offset..offset + 4].copy_from_slice(&line);
        }
    }
    for row in 1..rows {
        let y = (row * tile_height) as usize;
        for x in 0..width as usize {
            let offset = (y * width as usize + x) * 4;
            sheet[offset..offset + 4].copy_from_slice(&line);
        }
    }
    Ok(SheetOutput {
        png: encode_png(width, height, &sheet, has_alpha),
        width,
        height,
        columns,
        rows,
        tiles,
    })
}

enum Attempt {
    TooMuchWork { demanded: u64 },
    Failed(RenderError),
}

impl From<RenderError> for Attempt {
    fn from(error: RenderError) -> Self {
        Attempt::Failed(error)
    }
}

fn render_at(
    objects: &[RenderObject<'_>],
    options: &RenderOptions,
    width: u32,
    height: u32,
    ss: u32,
    translucent: bool,
) -> Result<RenderPixels, Attempt> {
    let (sw, sh) = ((width * ss) as usize, (height * ss) as usize);

    // Visible bounds and triangle budget.
    let mut total = 0usize;
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for object in objects.iter().filter(|object| object.visible) {
        total += object.mesh.triangles.len();
        if total > MAX_TRIANGLES {
            return Err(RenderError::TooManyTriangles(total).into());
        }
        for triangle in &object.mesh.triangles {
            for vertex in &triangle.vertices {
                let p = [vertex.x, vertex.y, vertex.z];
                if finite(p) {
                    for axis in 0..3 {
                        min[axis] = min[axis].min(p[axis]);
                        max[axis] = max[axis].max(p[axis]);
                    }
                }
            }
        }
    }
    let bounds = (min[0] <= max[0]).then_some(Bounds { min, max });
    // Framing box: the model, or a 20 mm cube at the origin for an empty scene.
    let frame_box = bounds.clone().unwrap_or(Bounds { min: [-10.0; 3], max: [10.0; 3] });
    let radius = (length(frame_box.size()) * 0.5).max(1e-3);

    // Camera.
    let aspect = width as f64 / height as f64;
    let fov = options.fov_deg.clamp(5.0, 120.0);
    let tan_v = (fov.to_radians() * 0.5).tan();
    let tan_h = tan_v * aspect;
    let zoom = if options.zoom.is_finite() && options.zoom > 0.0 { options.zoom.clamp(0.01, 1000.0) } else { 1.0 };
    let perspective = options.projection == Projection::Perspective;
    let target = options.target.unwrap_or_else(|| frame_box.center());
    if !finite(target) {
        return Err(RenderError::InvalidCamera("target must be finite".into()).into());
    }
    let up_hint = options.up.unwrap_or([0.0, 0.0, 1.0]);
    if !finite(up_hint) || length(up_hint) < 1e-12 {
        return Err(RenderError::InvalidCamera("up must be a non-zero vector".into()).into());
    }
    let corners: Vec<[f64; 3]> = (0..8)
        .map(|bits| {
            [
                if bits & 1 == 0 { frame_box.min[0] } else { frame_box.max[0] },
                if bits & 2 == 0 { frame_box.min[1] } else { frame_box.max[1] },
                if bits & 4 == 0 { frame_box.min[2] } else { frame_box.max[2] },
            ]
        })
        .collect();

    // A custom eye stays where it was asked to be, so perspective zooms by the
    // lens; preset views move the eye instead.
    let mut lens_zoom = 1.0;
    let (view_name, eye, right, up, forward, view_height) = if let Some(eye) = options.eye {
        if !finite(eye) {
            return Err(RenderError::InvalidCamera("eye must be finite".into()).into());
        }
        let to_eye = sub(eye, target);
        let distance = length(to_eye);
        if distance < 1e-9 {
            return Err(RenderError::InvalidCamera("eye and target coincide".into()).into());
        }
        let (right, up, forward) = basis(to_eye, up_hint)
            .ok_or_else(|| RenderError::InvalidCamera("degenerate camera basis".into()))?;
        // viewHeightMm is the orthographic scale, as for the presets.
        let height = if perspective {
            lens_zoom = zoom;
            2.0 * distance * tan_v / zoom
        } else {
            options
                .view_height_mm
                .filter(|value| value.is_finite() && *value > 0.0)
                .unwrap_or(2.0 * distance * tan_v)
                / zoom
        };
        ("custom".to_string(), eye, right, up, forward, height)
    } else {
        let (direction, default_up) = options.view.direction();
        let up_hint = options.up.unwrap_or(default_up);
        let (right, up, forward) = basis(direction, up_hint)
            .ok_or_else(|| RenderError::InvalidCamera("degenerate camera basis".into()))?;
        let margin = 1.08;
        let mut half_w: f64 = 0.0;
        let mut half_h: f64 = 0.0;
        let mut distance: f64 = 0.0;
        for corner in &corners {
            let relative = sub(*corner, target);
            let (x, y, z) = (dot(relative, right).abs(), dot(relative, up).abs(), dot(relative, direction));
            half_w = half_w.max(x);
            half_h = half_h.max(y);
            distance = distance.max(z + (x / tan_h).max(y / tan_v) * margin);
        }
        let distance = distance.max(radius * 0.5) / if perspective { zoom } else { 1.0 };
        let height = if perspective {
            2.0 * distance * tan_v
        } else {
            options
                .view_height_mm
                .filter(|value| value.is_finite() && *value > 0.0)
                .unwrap_or_else(|| (2.0 * half_h).max(2.0 * half_w / aspect).max(1e-3) * margin)
                / zoom
        };
        // An orthographic eye still needs a position for depth; put it well
        // outside the model so nothing lands behind it.
        let distance = if perspective { distance } else { distance.max(radius * 3.0) };
        let eye = add(target, scale(direction, distance));
        (options.view.as_str().to_string(), eye, right, up, forward, height)
    };

    let focal = if perspective { (sh as f64 * 0.5) * lens_zoom / tan_v } else { sh as f64 / view_height };
    let eye_distance = length(sub(eye, target));
    let camera = Camera {
        eye,
        right,
        up,
        forward,
        perspective,
        focal,
        cx: sw as f64 * 0.5,
        cy: sh as f64 * 0.5,
        near: (radius * 1e-4).max(eye_distance * 1e-5).max(1e-6),
    };
    let mm_per_pixel = view_height / height as f64;
    if !(camera.focal.is_finite() && camera.focal > 0.0 && finite(camera.eye) && view_height.is_finite()) {
        return Err(RenderError::InvalidCamera("scene or camera coordinates are out of range".into()).into());
    }

    // Section plane in world space: removed where dot(p - point, normal) > 0.
    let section = match &options.section {
        None => None,
        Some(SectionPlane::Plane { point, normal }) => {
            if !finite(*point) || !finite(*normal) || length(*normal) < 1e-12 {
                return Err(RenderError::InvalidSection("point and normal must be finite, normal non-zero".into()).into());
            }
            Some(SectionInfo { point: *point, normal: normalize(*normal) })
        }
        Some(SectionPlane::Axis { axis, offset, remove }) => {
            if !offset.is_finite() {
                return Err(RenderError::InvalidSection("offset must be finite".into()).into());
            }
            let index = match axis {
                Axis::X => 0,
                Axis::Y => 1,
                Axis::Z => 2,
            };
            let mut point = frame_box.center();
            point[index] = *offset;
            let mut normal = [0.0; 3];
            normal[index] = 1.0;
            let side = remove.unwrap_or(if eye[index] >= *offset { CutSide::Positive } else { CutSide::Negative });
            if side == CutSide::Negative {
                normal[index] = -1.0;
            }
            Some(SectionInfo { point, normal })
        }
    };
    let cut_view = section.as_ref().map(|plane| Cut {
        normal: camera.rotate(plane.normal),
        offset: dot(plane.normal, sub(eye, plane.point)),
    });

    // Rasterise.
    let mut target_buffer = Target {
        width: sw,
        height: sh,
        depth: vec![f32::INFINITY; sw * sh],
        id: vec![NO_TRIANGLE; sw * sh],
        overlay: if translucent { vec![[0.0, 0.0, 0.0, 1.0]; sw * sh] } else { Vec::new() },
        work: Work { left: MAX_RASTER_WORK, exhausted: false, demanded: 0 },
    };
    // Translucent triangles wait for the opaque pass, then go back to front.
    struct Layer {
        screen: Vec<[[f64; 3]; 3]>,
        far: f64,
        normal: [f64; 3],
        color: [f32; 3],
        alpha: f32,
        cut: bool,
    }
    let mut layers: Vec<Layer> = Vec::new();
    let mut infos: Vec<TriInfo> = Vec::with_capacity(total.min(1 << 20));
    let mut colors: Vec<[f32; 3]> = Vec::new();
    let mut object_infos = Vec::with_capacity(objects.len());
    let mut drawn = 0usize;
    for object in objects {
        let color = object.color.unwrap_or(OPENSCAD_YELLOW);
        object_infos.push(ObjectInfo {
            name: object.name.to_string(),
            visible: object.visible,
            triangles: object.mesh.triangles.len(),
            color: hex(color),
        });
        if !object.visible {
            continue;
        }
        let object_index = (object_infos.len() - 1) as u32;
        // One colour-table entry for the object, or one per palette slot when
        // it is painted per face: `(table index, linear colour, alpha)`.
        let first_color = colors.len() as u32;
        let palette: Vec<Rgba> = object.face_palette().unwrap_or_else(|| vec![color]);
        let shades: Vec<(u32, [f32; 3], f32)> = palette
            .iter()
            .enumerate()
            .map(|(slot, color)| {
                let linear = [srgb_to_linear(color[0]), srgb_to_linear(color[1]), srgb_to_linear(color[2])];
                colors.push(linear);
                (first_color + slot as u32, linear, color[3] as f32 / 255.0)
            })
            .collect();
        if shades.iter().all(|(_, _, alpha)| *alpha <= 0.0) {
            // Fully transparent: nothing to draw, but it still frames the view.
            continue;
        }
        for (triangle_index, triangle) in object.mesh.triangles.iter().enumerate() {
            let slot = object
                .face_colors
                .and_then(|colors| colors.faces.get(triangle_index))
                .map_or(0, |slot| usize::from(*slot));
            let Some(&(color_index, linear, alpha)) = shades.get(slot).or(shades.first()) else {
                continue;
            };
            if alpha <= 0.0 {
                continue;
            }
            let world = triangle.vertices.map(|v| [v.x, v.y, v.z]);
            if !world.iter().all(|p| finite(*p)) {
                continue;
            }
            let normal = cross(sub(world[1], world[0]), sub(world[2], world[0]));
            let normal_length = length(normal);
            if normal_length.is_nan() || normal_length <= 0.0 {
                continue;
            }
            let normal = scale(normal, 1.0 / normal_length);
            let mut partial_cut = false;
            if let Some(plane) = &section {
                let distances = world.map(|p| dot(sub(p, plane.point), plane.normal));
                if distances.iter().all(|d| *d > 0.0) {
                    continue;
                }
                partial_cut = distances.iter().any(|d| *d > 0.0);
            }
            let view = world.map(|p| camera.world_to_view(p));
            let view_normal = camera.rotate(normal);
            let back = if perspective { dot(view_normal, view[0]) > 0.0 } else { view_normal[2] > 0.0 };
            if alpha < 1.0 {
                // Front faces only: a translucent solid reads as one veil per
                // surface it puts between the eye and what is behind it.
                if back {
                    continue;
                }
                let screen: Vec<[[f64; 3]; 3]> = if perspective && view.iter().any(|p| p[2] < camera.near) {
                    let polygon = clip_near(&view, camera.near);
                    (1..polygon.len().saturating_sub(1))
                        .map(|k| [polygon[0], polygon[k], polygon[k + 1]].map(|p| camera.project(p)))
                        .collect()
                } else {
                    vec![view.map(|p| camera.project(p))]
                };
                drawn += 1;
                layers.push(Layer {
                    screen,
                    far: (view[0][2] + view[1][2] + view[2][2]) / 3.0,
                    normal: view_normal,
                    color: linear,
                    alpha,
                    cut: partial_cut,
                });
                continue;
            }
            let id = infos.len() as u32;
            infos.push(TriInfo {
                normal: view_normal.map(|value| value as f32),
                offset: dot(view_normal, view[0]) as f32,
                object: object_index,
                color: color_index,
                back,
            });
            drawn += 1;
            let cut = if partial_cut { cut_view.as_ref() } else { None };
            if perspective && view.iter().any(|p| p[2] < camera.near) {
                let polygon = clip_near(&view, camera.near);
                for k in 1..polygon.len().saturating_sub(1) {
                    let s = [polygon[0], polygon[k], polygon[k + 1]].map(|p| camera.project(p));
                    target_buffer.triangle(&camera, s, id, cut);
                }
            } else {
                let s = view.map(|p| camera.project(p));
                target_buffer.triangle(&camera, s, id, cut);
            }
        }
    }

    layers.sort_by(|a, b| b.far.total_cmp(&a.far));
    for layer in &layers {
        let cut = if layer.cut { cut_view.as_ref() } else { None };
        for s in &layer.screen {
            target_buffer.translucent(&camera, *s, layer.normal, layer.color, layer.alpha, cut);
        }
    }

    if target_buffer.work.exhausted {
        return Err(Attempt::TooMuchWork { demanded: target_buffer.work.demanded });
    }

    // Grid.
    let grid = if options.grid || options.axes {
        let size = frame_box.size();
        let extent = size[0].max(size[1]).max(size[2]).max(1e-3);
        let spacing = nice_step(extent / 10.0);
        let pad = spacing * 2.0;
        let lo_x = ((frame_box.min[0].min(0.0) - pad) / spacing).floor() * spacing;
        let hi_x = ((frame_box.max[0].max(0.0) + pad) / spacing).ceil() * spacing;
        let lo_y = ((frame_box.min[1].min(0.0) - pad) / spacing).floor() * spacing;
        let hi_y = ((frame_box.max[1].max(0.0) + pad) / spacing).ceil() * spacing;
        Some(Grid {
            spacing,
            lo: [lo_x, lo_y],
            hi: [hi_x, hi_y],
            lines: options.grid,
            axes: options.axes,
            z_axis_top: frame_box.max[2].max(spacing) + spacing,
        })
    } else {
        None
    };

    let background = options.background.map(|c| c.0).unwrap_or(DEFAULT_BACKGROUND);
    let pixels = shade(&camera, &target_buffer, &infos, &colors, ss as usize, width as usize, height as usize, background, options.edges, grid.as_ref());

    let grid_spacing = grid.as_ref().filter(|g| g.lines).map(|g| g.spacing);

    let has_alpha = background[3] < 255;
    // What the picture actually spans, which projecting back relies on.
    let fov_deg = perspective.then_some(if lens_zoom == 1.0 { fov } else { 2.0 * (tan_v / lens_zoom).atan().to_degrees() });
    Ok(RenderPixels {
        pixels,
        has_alpha,
        meta: RenderMeta {
            width,
            height,
            model_area: ModelArea { x: 0, y: 0, width, height },
            supersample: ss,
            camera: CameraInfo {
                view: view_name,
                projection: options.projection,
                eye,
                target,
                up,
                fov_deg,
                view_height_mm: view_height,
            },
            visible_bounds: bounds,
            mm_per_pixel,
            grid_spacing_mm: grid_spacing,
            section,
            triangles_drawn: drawn,
            objects: object_infos,
        },
    })
}

/// Smallest of 1, 2, 5 x 10^k that is >= `value`.
fn nice_step(value: f64) -> f64 {
    let exponent = value.log10().floor();
    let base = 10f64.powf(exponent);
    for factor in [1.0, 2.0, 5.0, 10.0] {
        if base * factor >= value * 0.999 {
            return base * factor;
        }
    }
    base * 10.0
}

// ---------------------------------------------------------------------------
// Deferred shading
// ---------------------------------------------------------------------------

struct Grid {
    spacing: f64,
    lo: [f64; 2],
    hi: [f64; 2],
    lines: bool,
    axes: bool,
    z_axis_top: f64,
}

fn srgb_to_linear(value: u8) -> f32 {
    let c = value as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(value: f32) -> f32 {
    let c = value.clamp(0.0, 1.0);
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

/// Whether two neighbouring subpixels lie on different surfaces.
fn discontinuity(camera: &Camera, target: &Target, infos: &[TriInfo], a: usize, b: usize) -> bool {
    let (ia, ib) = (target.id[a], target.id[b]);
    if ia == ib {
        return false;
    }
    if ia == NO_TRIANGLE || ib == NO_TRIANGLE {
        return true;
    }
    let (ta, tb) = (&infos[ia as usize], &infos[ib as usize]);
    if ta.object != tb.object || ta.back != tb.back {
        return true;
    }
    if ta.back {
        // Inside a cut cap: draw it as one flat region.
        return false;
    }
    let cos = ta.normal[0] * tb.normal[0] + ta.normal[1] * tb.normal[1] + ta.normal[2] * tb.normal[2];
    if cos < 0.82 {
        return true;
    }
    // Same orientation but a different plane (a step): predict b's depth from
    // a's plane and compare.
    let (bx, by) = ((b % target.width) as f64 + 0.5, (b / target.width) as f64 + 0.5);
    let (origin, dir) = camera.ray(bx, by);
    let n = ta.normal.map(|value| value as f64);
    let denominator = dot(n, dir);
    if denominator.abs() < 1e-6 {
        return false;
    }
    let predicted = (ta.offset as f64 - dot(n, origin)) / denominator;
    let actual = target.depth[b] as f64;
    (predicted - actual).abs() > camera.footprint(actual) * 3.0
}

#[allow(clippy::too_many_arguments)]
fn shade(
    camera: &Camera,
    target: &Target,
    infos: &[TriInfo],
    colors: &[[f32; 3]],
    ss: usize,
    width: usize,
    height: usize,
    background: Rgba,
    edges: bool,
    grid: Option<&Grid>,
) -> Vec<u8> {
    let background_linear = [
        srgb_to_linear(background[0]),
        srgb_to_linear(background[1]),
        srgb_to_linear(background[2]),
    ];
    let background_alpha = background[3] as f32 / 255.0;
    let edge_color = [0.02f32, 0.02, 0.025];
    let sw = target.width;
    let sh = target.height;

    // Z axis as a projected screen segment with depth at both ends.
    let z_axis = grid.filter(|g| g.axes).and_then(|g| {
        let a = camera.world_to_view([0.0, 0.0, 0.0]);
        let b = camera.world_to_view([0.0, 0.0, g.z_axis_top]);
        if camera.perspective && (a[2] < camera.near || b[2] < camera.near) {
            return None;
        }
        let pa = camera.project(a);
        let pb = camera.project(b);
        Some(([pa[0], pa[1], a[2]], [pb[0], pb[1], b[2]]))
    });
    let line_half_width = 0.75 * ss as f64; // subpixels, ~1.5 px lines

    let mut out = vec![0u8; width * height * 4];
    let inv_samples = 1.0 / (ss * ss) as f32;
    for oy in 0..height {
        for ox in 0..width {
            let mut sum = [0f32; 4];
            for sy in oy * ss..oy * ss + ss {
                for sx in ox * ss..ox * ss + ss {
                    let index = sy * sw + sx;
                    let id = target.id[index];
                    let depth = target.depth[index] as f64;
                    let (mut color, mut alpha) = if id == NO_TRIANGLE {
                        (background_linear, background_alpha)
                    } else {
                        let info = &infos[id as usize];
                        let base = colors[info.color as usize];
                        let color = if info.back {
                            // Cut-open interior: flat, darkened and pushed toward red, hatched.
                            let cap = mix(scale3(base, 0.55), [0.55, 0.06, 0.05], 0.45);
                            let stripe = ((sx + sy) / ss) % 7 < 2;
                            if stripe {
                                scale3(cap, 0.45)
                            } else {
                                cap
                            }
                        } else {
                            lit_color(camera, base, info.normal.map(|v| v as f64), sx as f64 + 0.5, sy as f64 + 0.5)
                        };
                        (color, 1.0)
                    };

                    if let Some(grid) = grid {
                        if let Some((grid_color, grid_alpha)) = grid_sample(camera, grid, sx, sy, depth, id == NO_TRIANGLE, line_half_width) {
                            color = mix(color, grid_color, grid_alpha);
                            alpha = alpha.max(grid_alpha);
                        }
                    }
                    if let Some((a, b)) = z_axis {
                        let (px, py) = (sx as f64 + 0.5, sy as f64 + 0.5);
                        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
                        let len2 = dx * dx + dy * dy;
                        let t = if len2 > 0.0 { (((px - a[0]) * dx + (py - a[1]) * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
                        let (qx, qy) = (a[0] + dx * t, a[1] + dy * t);
                        let distance = ((px - qx).powi(2) + (py - qy).powi(2)).sqrt();
                        let line_depth = a[2] + (b[2] - a[2]) * t;
                        if distance <= line_half_width && line_depth <= depth {
                            color = [0.05, 0.12, 0.85];
                            alpha = 1.0;
                        }
                    }

                    if edges && id != NO_TRIANGLE || edges && has_triangle_neighbour(target, sx, sy) {
                        let mut edge = false;
                        if sx + 1 < sw {
                            edge |= discontinuity(camera, target, infos, index, index + 1);
                        }
                        if sy + 1 < sh {
                            edge |= discontinuity(camera, target, infos, index, index + sw);
                        }
                        if sx > 0 {
                            edge |= discontinuity(camera, target, infos, index, index - 1);
                        }
                        if sy > 0 {
                            edge |= discontinuity(camera, target, infos, index, index - sw);
                        }
                        if edge {
                            color = mix(color, edge_color, 0.85);
                            alpha = 1.0;
                        }
                    }
                    // Translucent layers, already composited back to front:
                    // what lies behind shows through by their transmittance.
                    if let Some(layer) = target.overlay.get(index) {
                        let transmittance = layer[3];
                        for channel in 0..3 {
                            color[channel] = color[channel] * transmittance + layer[channel];
                        }
                        alpha = 1.0 - (1.0 - alpha) * transmittance;
                    }

                    sum[0] += color[0];
                    sum[1] += color[1];
                    sum[2] += color[2];
                    sum[3] += alpha;
                }
            }
            let offset = (oy * width + ox) * 4;
            for channel in 0..3 {
                out[offset + channel] = (linear_to_srgb(sum[channel] * inv_samples) * 255.0 + 0.5) as u8;
            }
            out[offset + 3] = ((sum[3] * inv_samples).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
    }
    out
}

fn has_triangle_neighbour(target: &Target, sx: usize, sy: usize) -> bool {
    let sw = target.width;
    let index = sy * sw + sx;
    (sx + 1 < sw && target.id[index + 1] != NO_TRIANGLE)
        || (sx > 0 && target.id[index - 1] != NO_TRIANGLE)
        || (sy + 1 < target.height && target.id[index + sw] != NO_TRIANGLE)
        || (sy > 0 && target.id[index - sw] != NO_TRIANGLE)
}

fn scale3(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

/// Grid/axis line colour and coverage where the subpixel's ray meets z = 0.
fn grid_sample(camera: &Camera, grid: &Grid, sx: usize, sy: usize, depth: f64, background: bool, half_width: f64) -> Option<([f32; 3], f32)> {
    let (origin, dir) = camera.ray(sx as f64 + 0.5, sy as f64 + 0.5);
    let world_origin = add(camera.eye, camera.view_to_world_dir(origin));
    let world_dir = camera.view_to_world_dir(dir);
    if world_dir[2].abs() < 1e-12 {
        return None;
    }
    let t = -world_origin[2] / world_dir[2];
    if t <= 0.0 {
        return None;
    }
    let hit = add(world_origin, scale(world_dir, t));
    if hit[0] < grid.lo[0] || hit[0] > grid.hi[0] || hit[1] < grid.lo[1] || hit[1] > grid.hi[1] {
        return None;
    }
    let occluded = !background && t > depth;
    // Lines fade out at grazing angles where they would alias into a smear.
    let grazing = (world_dir[2].abs() / length(world_dir)).min(1.0) as f32;
    let fade = (grazing * 4.0).min(1.0);
    let width = camera.footprint(t) * half_width;
    let near_line = |value: f64| {
        let r = (value / grid.spacing).round() * grid.spacing;
        (value - r).abs() <= width
    };
    let mut result = None;
    if grid.lines && (near_line(hit[0]) || near_line(hit[1])) {
        result = Some(([0.35f32, 0.35, 0.33], 0.45 * fade));
    }
    if grid.axes {
        let axis_width = width * 1.6;
        if hit[1].abs() <= axis_width && hit[0] >= 0.0 {
            result = Some(([0.85, 0.08, 0.06], 0.95 * fade));
        } else if hit[0].abs() <= axis_width && hit[1] >= 0.0 {
            result = Some(([0.05, 0.6, 0.08], 0.95 * fade));
        }
    }
    if occluded {
        return None;
    }
    result
}

// ---------------------------------------------------------------------------
// Label
// ---------------------------------------------------------------------------

/// 5x7 glyphs, one byte per row, low 5 bits used (bit 4 = leftmost).
fn glyph(c: char) -> [u8; 7] {
    match c {
        '0' => [0x0e, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0e],
        '1' => [0x04, 0x0c, 0x04, 0x04, 0x04, 0x04, 0x0e],
        '2' => [0x0e, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1f],
        '3' => [0x1f, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0e],
        '4' => [0x02, 0x06, 0x0a, 0x12, 0x1f, 0x02, 0x02],
        '5' => [0x1f, 0x10, 0x1e, 0x01, 0x01, 0x11, 0x0e],
        '6' => [0x06, 0x08, 0x10, 0x1e, 0x11, 0x11, 0x0e],
        '7' => [0x1f, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0e, 0x11, 0x11, 0x0e, 0x11, 0x11, 0x0e],
        '9' => [0x0e, 0x11, 0x11, 0x0f, 0x01, 0x02, 0x0c],
        '.' => [0, 0, 0, 0, 0, 0x0c, 0x0c],
        'g' => [0, 0x0f, 0x11, 0x11, 0x0f, 0x01, 0x0e],
        'r' => [0, 0, 0x16, 0x19, 0x10, 0x10, 0x10],
        'i' => [0x04, 0, 0x0c, 0x04, 0x04, 0x04, 0x0e],
        'd' => [0x01, 0x01, 0x0d, 0x13, 0x11, 0x11, 0x0f],
        'm' => [0, 0, 0x1a, 0x15, 0x15, 0x15, 0x15],
        'p' => [0, 0, 0x1e, 0x11, 0x1e, 0x10, 0x10],
        'x' => [0, 0, 0x11, 0x0a, 0x04, 0x0a, 0x11],
        '=' => [0, 0, 0x1f, 0, 0x1f, 0, 0],
        '/' => [0, 0x01, 0x02, 0x04, 0x08, 0x10, 0],
        'a' => [0, 0, 0x0e, 0x01, 0x0f, 0x11, 0x0f],
        'b' => [0x10, 0x10, 0x16, 0x19, 0x11, 0x11, 0x1e],
        'c' => [0, 0, 0x0e, 0x10, 0x10, 0x11, 0x0e],
        'e' => [0, 0, 0x0e, 0x11, 0x1f, 0x10, 0x0e],
        'f' => [0x06, 0x09, 0x08, 0x1c, 0x08, 0x08, 0x08],
        'h' => [0x10, 0x10, 0x16, 0x19, 0x11, 0x11, 0x11],
        'k' => [0x10, 0x10, 0x12, 0x14, 0x18, 0x14, 0x12],
        'l' => [0x0c, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0e],
        'n' => [0, 0, 0x16, 0x19, 0x11, 0x11, 0x11],
        'o' => [0, 0, 0x0e, 0x11, 0x11, 0x11, 0x0e],
        's' => [0, 0, 0x0e, 0x10, 0x0e, 0x01, 0x1e],
        't' => [0x08, 0x08, 0x1c, 0x08, 0x08, 0x09, 0x06],
        'u' => [0, 0, 0x11, 0x11, 0x11, 0x13, 0x0d],
        'v' => [0, 0, 0x11, 0x11, 0x11, 0x0a, 0x04],
        'w' => [0, 0, 0x11, 0x11, 0x15, 0x15, 0x0a],
        'y' => [0, 0, 0x11, 0x11, 0x0f, 0x01, 0x0e],
        'z' => [0, 0, 0x1f, 0x02, 0x04, 0x08, 0x1f],
        '-' => [0, 0, 0, 0x1f, 0, 0, 0],
        '+' => [0, 0x04, 0x04, 0x1f, 0x04, 0x04, 0],
        ':' => [0, 0x0c, 0x0c, 0, 0x0c, 0x0c, 0],
        ',' => [0, 0, 0, 0, 0x0c, 0x04, 0x08],
        '%' => [0x18, 0x19, 0x02, 0x04, 0x08, 0x13, 0x03],
        '(' => [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02],
        ')' => [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08],
        '<' => [0x02, 0x04, 0x08, 0x10, 0x08, 0x04, 0x02],
        '>' => [0x08, 0x04, 0x02, 0x01, 0x02, 0x04, 0x08],
        '#' => [0x0a, 0x0a, 0x1f, 0x0a, 0x1f, 0x0a, 0x0a],
        '_' => [0, 0, 0, 0, 0, 0, 0x1f],
        '|' => [0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        '[' => [0x0e, 0x08, 0x08, 0x08, 0x08, 0x08, 0x0e],
        ']' => [0x0e, 0x02, 0x02, 0x02, 0x02, 0x02, 0x0e],
        'j' => [0x02, 0, 0x06, 0x02, 0x02, 0x12, 0x0c],
        'q' => [0, 0, 0x0d, 0x13, 0x0f, 0x01, 0x01],
        '\'' => [0x04, 0x04, 0x08, 0, 0, 0, 0],
        c if c.is_ascii_uppercase() => glyph(c.to_ascii_lowercase()),
        _ => [0; 7],
    }
}

fn format_mm(value: f64) -> String {
    if value >= 1.0 && (value - value.round()).abs() < 1e-9 {
        format!("{}", value.round() as i64)
    } else {
        let text = format!("{value:.4}");
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn draw_text(pixels: &mut [u8], width: usize, height: usize, x: usize, y: usize, text: &str, scale: usize) {
    draw_text_colored(pixels, width, height, x, y, text, scale, [40, 40, 40, 255]);
}

/// Width in pixels of `text` drawn by [`draw_text_colored`] at `scale`.
pub fn text_width(text: &str, scale: usize) -> usize {
    text.chars().count() * 6 * scale
}

/// Stamps `text` in the built-in 5x7 font (digits, letters and common
/// punctuation; anything else draws blank) with its top-left at (x, y).
#[allow(clippy::too_many_arguments)]
pub fn draw_text_colored(pixels: &mut [u8], width: usize, height: usize, x: usize, y: usize, text: &str, scale: usize, color: Rgba) {
    let mut cursor = x;
    for c in text.chars() {
        let rows = glyph(c);
        for (row, bits) in rows.iter().enumerate() {
            for column in 0..5 {
                if bits & (0x10 >> column) == 0 {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let px = cursor + column * scale + dx;
                        let py = y + row * scale + dy;
                        if px < width && py < height {
                            let offset = (py * width + px) * 4;
                            pixels[offset..offset + 4].copy_from_slice(&color);
                        }
                    }
                }
            }
        }
        cursor += 6 * scale;
    }
}

/// Font scale of the text stamped on an image with a model area this wide.
pub fn text_scale(width: u32) -> usize {
    if width >= 900 { 2 } else { 1 }
}

/// Whether the scale bar and the grid label stand on separate rows: the bar
/// alone can take 5/12 of a narrow width.
fn label_rows_stack(width: u32) -> bool {
    width < 400 * text_scale(width) as u32
}

/// Height of the band under the model area that holds the label, 0 if nothing
/// is stamped: 13 per row plus 4, in font-scale units.
fn label_band(width: u32, options: &RenderOptions) -> u32 {
    if !options.label {
        return 0;
    }
    let bar = options.projection != Projection::Perspective;
    let rows = match (bar, options.grid) {
        (false, false) => return 0,
        (true, true) if label_rows_stack(width) => 2,
        _ => 1,
    };
    (rows * 13 + 4) * text_scale(width) as u32
}

/// Draws the scale bar and the grid label on the band below the model area
/// (`top` is its first row), within the `model_width` columns.
fn stamp_label(pixels: &mut [u8], width: usize, height: usize, model_width: usize, top: usize, grid_spacing: Option<f64>, ortho_mm_per_pixel: Option<f64>) {
    let scale = text_scale(model_width as u32);
    let line = 9 * scale;
    let x = 8 * scale;
    let mut y = top + 4 * scale;
    // Where the grid label goes: after the bar on its row, or on its own row.
    let mut grid_at = (x, y);
    let stack = ortho_mm_per_pixel.is_some() && grid_spacing.is_some() && label_rows_stack(model_width as u32);
    if stack {
        y += line + 4 * scale;
    }
    if let Some(mm_per_pixel) = ortho_mm_per_pixel {
        // Scale bar spanning a nice length near 1/5 of the model area.
        let bar_mm = nice_step(mm_per_pixel * model_width as f64 / 6.0);
        let bar_px = (bar_mm / mm_per_pixel).round() as usize;
        let text = format!("{} mm", format_mm(bar_mm));
        // A bar is no use without its length, nor a length cut off: both go.
        if bar_px > 4 && x + bar_px + 6 * scale + text_width(&text, scale) <= model_width {
            let bar_y = y + line / 2 - scale;
            for dy in 0..(2 * scale) {
                for px in x..(x + bar_px).min(model_width) {
                    let py = bar_y + dy;
                    if py < height {
                        let offset = (py * width + px) * 4;
                        pixels[offset..offset + 4].copy_from_slice(&[40, 40, 40, 255]);
                    }
                }
            }
            for tick in [x, (x + bar_px - 1).min(model_width - 1)] {
                for py in bar_y.saturating_sub(3 * scale)..(bar_y + 5 * scale).min(height) {
                    for dx in 0..scale {
                        let offset = (py * width + (tick + dx).min(model_width - 1)) * 4;
                        pixels[offset..offset + 4].copy_from_slice(&[40, 40, 40, 255]);
                    }
                }
            }
            draw_text(pixels, width, height, x + bar_px + 6 * scale, y, &text, scale);
            if !stack {
                grid_at = (x + bar_px + 6 * scale + text_width(&text, scale) + 12 * scale, y);
            }
        }
    }
    if let Some(spacing) = grid_spacing {
        let text = format!("grid {} mm", format_mm(spacing));
        if grid_at.0 + text_width(&text, scale) <= model_width {
            draw_text(pixels, width, height, grid_at.0, grid_at.1, &text, scale);
        }
    }
}

// ---------------------------------------------------------------------------
// PNG encoder
// ---------------------------------------------------------------------------

/// Encodes 8-bit RGBA pixels (row-major, `width * height * 4` bytes) as PNG.
/// With `keep_alpha == false` the alpha channel is dropped (colour type 2).
pub fn encode_png(width: u32, height: u32, rgba: &[u8], keep_alpha: bool) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    assert_eq!(rgba.len(), w * h * 4, "pixel buffer does not match the image size");
    let channels = if keep_alpha { 4 } else { 3 };
    let stride = w * channels;

    // Scanlines with an adaptive filter (minimum sum of absolute differences).
    let mut raw = Vec::with_capacity((stride + 1) * h);
    let mut previous = vec![0u8; stride];
    let mut current = vec![0u8; stride];
    let mut candidate = vec![0u8; stride];
    let mut best = vec![0u8; stride];
    for y in 0..h {
        for x in 0..w {
            let source = &rgba[(y * w + x) * 4..(y * w + x) * 4 + channels];
            current[x * channels..x * channels + channels].copy_from_slice(source);
        }
        let mut best_score = u64::MAX;
        let mut best_filter = 0u8;
        for filter in 0..5u8 {
            let mut score = 0u64;
            for i in 0..stride {
                let left = if i >= channels { current[i - channels] } else { 0 };
                let up = previous[i];
                let up_left = if i >= channels { previous[i - channels] } else { 0 };
                let predictor = match filter {
                    0 => 0,
                    1 => left,
                    2 => up,
                    3 => ((left as u16 + up as u16) / 2) as u8,
                    _ => paeth(left, up, up_left),
                };
                let value = current[i].wrapping_sub(predictor);
                candidate[i] = value;
                score += (value as i8).unsigned_abs() as u64;
            }
            if score < best_score {
                best_score = score;
                best_filter = filter;
                std::mem::swap(&mut best, &mut candidate);
            }
        }
        raw.push(best_filter);
        raw.extend_from_slice(&best);
        std::mem::swap(&mut previous, &mut current);
    }

    let mut png = Vec::with_capacity(raw.len() / 8 + 64);
    png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, if keep_alpha { 6 } else { 2 }, 0, 0, 0]);
    write_chunk(&mut png, b"IHDR", &header);
    write_chunk(&mut png, b"IDAT", &zlib_compress(&raw));
    write_chunk(&mut png, b"IEND", &[]);
    png
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = ((p - a as i16).abs(), (p - b as i16).abs(), (p - c as i16).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// CRC-32 (IEEE, reflected), as PNG chunks require.
pub fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (n, entry) in table.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *entry = c;
        }
        table
    });
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc = table[((crc ^ *byte as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc ^ 0xffff_ffff
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for byte in chunk {
            a += *byte as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

struct BitWriter {
    out: Vec<u8>,
    bits: u64,
    count: u32,
}

impl BitWriter {
    fn write(&mut self, value: u32, count: u32) {
        self.bits |= (value as u64) << self.count;
        self.count += count;
        while self.count >= 8 {
            self.out.push(self.bits as u8);
            self.bits >>= 8;
            self.count -= 8;
        }
    }

    /// Huffman codes are sent most-significant bit first.
    fn write_code(&mut self, code: u32, length: u32) {
        self.write(code.reverse_bits() >> (32 - length), length);
    }

    fn finish(mut self) -> Vec<u8> {
        if self.count > 0 {
            self.out.push(self.bits as u8);
        }
        self.out
    }
}

const LENGTH_BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LENGTH_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

fn write_literal(writer: &mut BitWriter, symbol: u32) {
    match symbol {
        0..=143 => writer.write_code(0x30 + symbol, 8),
        144..=255 => writer.write_code(0x190 + symbol - 144, 9),
        256..=279 => writer.write_code(symbol - 256, 7),
        _ => writer.write_code(0xc0 + symbol - 280, 8),
    }
}

fn write_match(writer: &mut BitWriter, length: usize, distance: usize) {
    let code = LENGTH_BASE.iter().rposition(|base| *base as usize <= length).expect("length >= 3");
    write_literal(writer, 257 + code as u32);
    writer.write((length - LENGTH_BASE[code] as usize) as u32, LENGTH_EXTRA[code] as u32);
    let code = DIST_BASE.iter().rposition(|base| *base as usize <= distance).expect("distance >= 1");
    writer.write_code(code as u32, 5);
    writer.write((distance - DIST_BASE[code] as usize) as u32, DIST_EXTRA[code] as u32);
}

/// zlib stream holding one fixed-Huffman deflate block with LZ77 matches.
pub fn zlib_compress(data: &[u8]) -> Vec<u8> {
    const WINDOW: usize = 32 * 1024;
    const HASH_BITS: u32 = 15;
    const MAX_CHAIN: usize = 24;
    const MAX_MATCH: usize = 258;
    let mut writer = BitWriter { out: Vec::with_capacity(data.len() / 4 + 16), bits: 0, count: 0 };
    writer.out.extend_from_slice(&[0x78, 0x01]);
    writer.write(1, 1); // BFINAL
    writer.write(1, 2); // BTYPE = fixed Huffman

    let mut head = vec![usize::MAX; 1 << HASH_BITS];
    let mut chain = vec![usize::MAX; WINDOW];
    let hash = |i: usize| {
        let v = (data[i] as u32) | (data[i + 1] as u32) << 8 | (data[i + 2] as u32) << 16;
        (v.wrapping_mul(0x9e37_79b1) >> (32 - HASH_BITS)) as usize
    };
    let insert = |head: &mut Vec<usize>, chain: &mut Vec<usize>, i: usize| {
        if i + 2 < data.len() {
            let h = hash(i);
            chain[i % WINDOW] = head[h];
            head[h] = i;
        }
    };

    let mut i = 0;
    while i < data.len() {
        let mut best_length = 0;
        let mut best_distance = 0;
        if i + 2 < data.len() {
            let mut candidate = head[hash(i)];
            let limit = (data.len() - i).min(MAX_MATCH);
            let mut steps = 0;
            while candidate != usize::MAX && i - candidate <= WINDOW && steps < MAX_CHAIN {
                if data[candidate + best_length.min(limit - 1)] == data[i + best_length.min(limit - 1)] {
                    let mut length = 0;
                    while length < limit && data[candidate + length] == data[i + length] {
                        length += 1;
                    }
                    if length > best_length {
                        best_length = length;
                        best_distance = i - candidate;
                        if length == limit {
                            break;
                        }
                    }
                }
                let next = chain[candidate % WINDOW];
                if next == usize::MAX || next >= candidate {
                    break;
                }
                candidate = next;
                steps += 1;
            }
        }
        if best_length >= 3 {
            write_match(&mut writer, best_length, best_distance);
            // Index every position of short matches; for long runs only the
            // ends, which keeps flat image regions linear-time.
            if best_length <= 32 {
                for k in i..i + best_length {
                    insert(&mut head, &mut chain, k);
                }
            } else {
                for k in i..i + 4 {
                    insert(&mut head, &mut chain, k);
                }
                for k in i + best_length - 4..i + best_length {
                    insert(&mut head, &mut chain, k);
                }
            }
            i += best_length;
        } else {
            write_literal(&mut writer, data[i] as u32);
            insert(&mut head, &mut chain, i);
            i += 1;
        }
    }
    write_literal(&mut writer, 256);
    let mut out = writer.finish();
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::engine::{Triangle, Vec3};

    // --- A minimal inflate (stored + fixed + dynamic Huffman) to verify output.

    struct BitReader<'a> {
        data: &'a [u8],
        position: usize,
    }

    impl BitReader<'_> {
        fn bit(&mut self) -> u32 {
            let byte = self.data[self.position / 8];
            let bit = (byte >> (self.position % 8)) & 1;
            self.position += 1;
            bit as u32
        }
        fn bits(&mut self, count: u32) -> u32 {
            (0..count).fold(0, |acc, k| acc | (self.bit() << k))
        }
    }

    struct Huffman {
        counts: [u16; 16],
        symbols: Vec<u16>,
    }

    impl Huffman {
        fn new(lengths: &[u8]) -> Self {
            let mut counts = [0u16; 16];
            for &l in lengths {
                counts[l as usize] += 1;
            }
            counts[0] = 0;
            let mut offsets = [0u16; 16];
            for l in 1..16 {
                offsets[l] = offsets[l - 1] + counts[l - 1];
            }
            let mut symbols = vec![0u16; lengths.len()];
            for (symbol, &l) in lengths.iter().enumerate() {
                if l != 0 {
                    symbols[offsets[l as usize] as usize] = symbol as u16;
                    offsets[l as usize] += 1;
                }
            }
            Self { counts, symbols }
        }
        fn decode(&self, reader: &mut BitReader) -> u16 {
            let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
            for l in 1..16 {
                code |= reader.bit() as i32;
                let count = self.counts[l] as i32;
                if code - count < first {
                    return self.symbols[(index + code - first) as usize];
                }
                index += count;
                first = (first + count) << 1;
                code <<= 1;
            }
            panic!("bad huffman code");
        }
    }

    fn inflate(data: &[u8]) -> Vec<u8> {
        let mut reader = BitReader { data, position: 0 };
        let mut out = Vec::new();
        loop {
            let last = reader.bit();
            let kind = reader.bits(2);
            match kind {
                0 => {
                    reader.position = reader.position.div_ceil(8) * 8;
                    let p = reader.position / 8;
                    let len = u16::from_le_bytes([data[p], data[p + 1]]) as usize;
                    let nlen = u16::from_le_bytes([data[p + 2], data[p + 3]]);
                    assert_eq!(len as u16, !nlen);
                    out.extend_from_slice(&data[p + 4..p + 4 + len]);
                    reader.position = (p + 4 + len) * 8;
                }
                1 | 2 => {
                    let (lit, dist) = if kind == 1 {
                        let mut lengths = [0u8; 288];
                        lengths[..144].fill(8);
                        lengths[144..256].fill(9);
                        lengths[256..280].fill(7);
                        lengths[280..].fill(8);
                        (Huffman::new(&lengths), Huffman::new(&[5u8; 30]))
                    } else {
                        let hlit = reader.bits(5) as usize + 257;
                        let hdist = reader.bits(5) as usize + 1;
                        let hclen = reader.bits(4) as usize + 4;
                        const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
                        let mut code_lengths = [0u8; 19];
                        for &o in ORDER.iter().take(hclen) {
                            code_lengths[o] = reader.bits(3) as u8;
                        }
                        let code = Huffman::new(&code_lengths);
                        let mut lengths = Vec::new();
                        while lengths.len() < hlit + hdist {
                            let symbol = code.decode(&mut reader);
                            match symbol {
                                0..=15 => lengths.push(symbol as u8),
                                16 => {
                                    let prev = *lengths.last().unwrap();
                                    for _ in 0..3 + reader.bits(2) {
                                        lengths.push(prev);
                                    }
                                }
                                17 => lengths.extend(std::iter::repeat_n(0, 3 + reader.bits(3) as usize)),
                                _ => lengths.extend(std::iter::repeat_n(0, 11 + reader.bits(7) as usize)),
                            }
                        }
                        (Huffman::new(&lengths[..hlit]), Huffman::new(&lengths[hlit..]))
                    };
                    loop {
                        let symbol = lit.decode(&mut reader) as usize;
                        if symbol < 256 {
                            out.push(symbol as u8);
                        } else if symbol == 256 {
                            break;
                        } else {
                            let code = symbol - 257;
                            let length = LENGTH_BASE[code] as usize + reader.bits(LENGTH_EXTRA[code] as u32) as usize;
                            let dcode = dist.decode(&mut reader) as usize;
                            let distance = DIST_BASE[dcode] as usize + reader.bits(DIST_EXTRA[dcode] as u32) as usize;
                            assert!(distance <= out.len() && distance <= 32768);
                            for _ in 0..length {
                                out.push(out[out.len() - distance]);
                            }
                        }
                    }
                }
                _ => panic!("reserved block type"),
            }
            if last == 1 {
                break;
            }
        }
        out
    }

    fn zlib_decompress(data: &[u8]) -> Vec<u8> {
        assert_eq!(data[0] & 0x0f, 8, "deflate method");
        assert_eq!(((data[0] as u16) << 8 | data[1] as u16) % 31, 0, "FCHECK");
        let body = inflate(&data[2..data.len() - 4]);
        let expected = u32::from_be_bytes(data[data.len() - 4..].try_into().unwrap());
        assert_eq!(adler32(&body), expected, "adler32");
        body
    }

    /// Decodes our own PNG output: validates chunk CRCs and returns RGBA.
    pub(crate) fn decode_png(png: &[u8]) -> (usize, usize, Vec<u8>) {
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        let mut position = 8;
        let (mut width, mut height, mut color_type) = (0, 0, 0);
        let mut idat = Vec::new();
        let mut kinds = Vec::new();
        while position < png.len() {
            let length = u32::from_be_bytes(png[position..position + 4].try_into().unwrap()) as usize;
            let kind = &png[position + 4..position + 8];
            let body = &png[position + 8..position + 8 + length];
            let crc = u32::from_be_bytes(png[position + 8 + length..position + 12 + length].try_into().unwrap());
            assert_eq!(crc32(&png[position + 4..position + 8 + length]), crc, "chunk CRC");
            kinds.push(String::from_utf8(kind.to_vec()).unwrap());
            match kind {
                b"IHDR" => {
                    width = u32::from_be_bytes(body[0..4].try_into().unwrap()) as usize;
                    height = u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize;
                    assert_eq!(body[8], 8);
                    color_type = body[9];
                }
                b"IDAT" => idat.extend_from_slice(body),
                _ => {}
            }
            position += 12 + length;
        }
        assert_eq!(kinds.first().map(String::as_str), Some("IHDR"));
        assert_eq!(kinds.last().map(String::as_str), Some("IEND"));
        let channels = if color_type == 6 { 4 } else { 3 };
        let raw = zlib_decompress(&idat);
        let stride = width * channels;
        assert_eq!(raw.len(), (stride + 1) * height);
        let mut pixels = vec![0u8; stride * height];
        for y in 0..height {
            let filter = raw[y * (stride + 1)];
            for i in 0..stride {
                let x = raw[y * (stride + 1) + 1 + i];
                let a = if i >= channels { pixels[y * stride + i - channels] } else { 0 };
                let b = if y > 0 { pixels[(y - 1) * stride + i] } else { 0 };
                let c = if y > 0 && i >= channels { pixels[(y - 1) * stride + i - channels] } else { 0 };
                let predictor = match filter {
                    0 => 0,
                    1 => a,
                    2 => b,
                    3 => ((a as u16 + b as u16) / 2) as u8,
                    4 => paeth(a, b, c),
                    _ => panic!("bad filter"),
                };
                pixels[y * stride + i] = x.wrapping_add(predictor);
            }
        }
        let rgba = pixels
            .chunks(channels)
            .flat_map(|p| [p[0], p[1], p[2], if channels == 4 { p[3] } else { 255 }])
            .collect();
        (width, height, rgba)
    }

    fn v(x: f64, y: f64, z: f64) -> Vec3 {
        Vec3 { x, y, z }
    }

    fn cube(min: [f64; 3], max: [f64; 3]) -> Mesh {
        let p = |i: usize| {
            v(
                if i & 1 == 0 { min[0] } else { max[0] },
                if i & 2 == 0 { min[1] } else { max[1] },
                if i & 4 == 0 { min[2] } else { max[2] },
            )
        };
        // Outward CCW quads.
        let quads = [[0, 2, 3, 1], [4, 5, 7, 6], [0, 1, 5, 4], [2, 6, 7, 3], [0, 4, 6, 2], [1, 3, 7, 5]];
        let mut triangles = Vec::new();
        for q in quads {
            for tri in [[q[0], q[1], q[2]], [q[0], q[2], q[3]]] {
                triangles.push(Triangle { vertices: tri.map(p), normal: v(0.0, 0.0, 0.0) });
            }
        }
        Mesh { triangles }
    }

    fn sphere(center: [f64; 3], r: f64, segments: usize) -> Mesh {
        let rings = segments / 2;
        let point = |i: usize, j: usize| {
            let theta = std::f64::consts::PI * i as f64 / rings as f64;
            let phi = 2.0 * std::f64::consts::PI * j as f64 / segments as f64;
            v(
                center[0] + r * theta.sin() * phi.cos(),
                center[1] + r * theta.sin() * phi.sin(),
                center[2] + r * theta.cos(),
            )
        };
        let mut triangles = Vec::new();
        for i in 0..rings {
            for j in 0..segments {
                let (a, b, c, d) = (point(i, j), point(i + 1, j), point(i + 1, j + 1), point(i, j + 1));
                triangles.push(Triangle { vertices: [a, b, c], normal: v(0.0, 0.0, 0.0) });
                triangles.push(Triangle { vertices: [a, c, d], normal: v(0.0, 0.0, 0.0) });
            }
        }
        Mesh { triangles }
    }

    fn plain(width: u32, height: u32) -> RenderOptions {
        RenderOptions {
            width,
            height,
            grid: false,
            axes: false,
            label: false,
            edges: false,
            background: Some(Color([255, 255, 255, 255])),
            ..RenderOptions::default()
        }
    }

    fn pixel(rgba: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
        rgba[(y * width + x) * 4..(y * width + x) * 4 + 4].try_into().unwrap()
    }

    fn is_background(p: [u8; 4]) -> bool {
        p[0] == 255 && p[1] == 255 && p[2] == 255
    }

    #[test]
    fn zlib_round_trips_varied_data() {
        let mut data = Vec::new();
        for i in 0..200_000u32 {
            data.push(if i % 1000 < 600 { 7 } else { (i.wrapping_mul(2_654_435_761) >> 13) as u8 });
        }
        data.extend_from_slice(b"abcabcabcabcabcabc hello hello hello");
        assert_eq!(zlib_decompress(&zlib_compress(&data)), data);
        assert_eq!(zlib_decompress(&zlib_compress(&[])), Vec::<u8>::new());
        assert_eq!(zlib_decompress(&zlib_compress(b"ab")), b"ab");
        assert!(zlib_compress(&vec![0u8; 100_000]).len() < 2_000);
    }

    #[test]
    fn crc_and_adler_match_known_values() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b"IEND"), 0xae42_6082);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn png_is_well_formed_and_decodes() {
        let mut rgba = Vec::new();
        for y in 0..70u32 {
            for x in 0..90u32 {
                rgba.extend_from_slice(&[(x * 3) as u8, (y * 3) as u8, ((x ^ y) * 7) as u8, (x + y) as u8]);
            }
        }
        for alpha in [false, true] {
            let png = encode_png(90, 70, &rgba, alpha);
            let (w, h, decoded) = decode_png(&png);
            assert_eq!((w, h), (90, 70));
            for (a, b) in decoded.chunks(4).zip(rgba.chunks(4)) {
                assert_eq!(&a[..3], &b[..3]);
                assert_eq!(a[3], if alpha { b[3] } else { 255 });
            }
        }
    }

    /// Red-dominant pixels' extent (x0, y0, x1, y1) within the first
    /// `width` x `height` pixels of an image `stride` wide.
    fn red_extent(rgba: &[u8], stride: usize, width: usize, height: usize) -> Option<(usize, usize, usize, usize)> {
        let mut extent: Option<(usize, usize, usize, usize)> = None;
        for y in 0..height {
            for x in 0..width {
                if is_red(pixel(rgba, stride, x, y)) {
                    let e = extent.get_or_insert((x, y, x, y));
                    *e = (e.0.min(x), e.1.min(y), e.2.max(x), e.3.max(y));
                }
            }
        }
        extent
    }

    fn is_red(p: [u8; 4]) -> bool {
        p[0] as i32 > p[1] as i32 + 60 && p[0] as i32 > p[2] as i32 + 60
    }

    const RED: Rgba = [230, 20, 20, 255];

    fn wide_top_view(width: u32, height: u32) -> RenderOptions {
        RenderOptions {
            view: NamedView::Top,
            projection: Projection::Orthographic,
            grid: true,
            label: true,
            ..plain(width, height)
        }
    }

    #[test]
    fn a_reserved_column_and_the_label_band_hold_no_model_and_the_whole_part_fits() {
        let mesh = cube([-200.0, -20.0, 0.0], [200.0, 20.0, 10.0]);
        let objects = [RenderObject { name: "wide", mesh: &mesh, color: Some(RED), face_colors: None, visible: true }];
        let options = wide_top_view(800, 600);
        let bare = render(&objects, &options).unwrap();
        let output = render_with_overlay(&objects, &options, Reserve { right: 170, min_height: 0 }, |_, _, _, _| {}).unwrap();
        // Same camera and scale as without the column: the image grows instead.
        assert_eq!(output.meta.camera, bare.meta.camera);
        assert_eq!(output.meta.mm_per_pixel, bare.meta.mm_per_pixel);
        assert_eq!(output.meta.model_area, ModelArea { x: 0, y: 0, width: 800, height: 600 });
        let (w, h, rgba) = decode_png(&output.png);
        assert_eq!((w as u32, h as u32), (output.meta.width, output.meta.height));
        assert_eq!(w, 970);
        assert!(h > 600, "the label has a band below the model area: {h}");
        // The part's full width lands inside the model area, centred.
        let (x0, _, x1, _) = red_extent(&rgba, w, 800, 600).expect("the part is drawn");
        let left = project_point(&output.meta, [-200.0, 0.0, 10.0]).unwrap()[0];
        let right = project_point(&output.meta, [200.0, 0.0, 10.0]).unwrap()[0];
        assert!(left > 1.0 && right < 799.0, "{left}..{right}");
        assert!((x0 as f64 - left).abs() <= 2.0 && (x1 as f64 - right).abs() <= 2.0, "{x0}..{x1} vs {left}..{right}");
        assert!(((x0 + x1) as i64 - 799).abs() <= 2, "centred: {x0}..{x1}");
        // No model pixel anywhere else, and the margins are bare background.
        for y in 0..h {
            for x in 800..w {
                assert!(is_background(pixel(&rgba, w, x, y)), "column at {x},{y}");
            }
        }
        let mut ink = 0;
        for y in 600..h {
            for x in 0..800 {
                let p = pixel(&rgba, w, x, y);
                assert!(!is_red(p), "band at {x},{y}");
                ink += usize::from(!is_background(p));
            }
        }
        assert!(ink > 100, "the label is in the band");
    }

    #[test]
    fn explicit_cameras_keep_their_framing_and_stay_out_of_the_margins() {
        let mesh = cube([-200.0, -20.0, 0.0], [200.0, 20.0, 10.0]);
        let objects = [RenderObject { name: "wide", mesh: &mesh, color: Some(RED), face_colors: None, visible: true }];
        let variants = [
            RenderOptions { zoom: 3.0, ..wide_top_view(640, 480) },
            RenderOptions { view_height_mm: Some(100.0), ..wide_top_view(640, 480) },
            RenderOptions { view: NamedView::Iso, projection: Projection::Perspective, ..wide_top_view(640, 480) },
            RenderOptions { eye: Some([50.0, -300.0, 400.0]), target: Some([0.0, 0.0, 0.0]), projection: Projection::Perspective, ..wide_top_view(640, 480) },
            RenderOptions { eye: Some([0.0, 0.0, 300.0]), up: Some([0.0, 1.0, 0.0]), target: Some([0.0, 0.0, 0.0]), zoom: 4.0, ..wide_top_view(640, 480) },
        ];
        for options in variants {
            let bare = render(&objects, &options).unwrap();
            let output = render_with_overlay(&objects, &options, Reserve { right: 120, min_height: 0 }, |_, _, _, _| {}).unwrap();
            assert_eq!(output.meta.camera, bare.meta.camera);
            assert_eq!(output.meta.mm_per_pixel, bare.meta.mm_per_pixel);
            let (w, h, rgba) = decode_png(&output.png);
            let area = output.meta.model_area;
            assert_eq!((area.width, area.height), (640, 480));
            assert!(red_extent(&rgba, w, 640, 480).is_some());
            for y in 0..h {
                for x in 0..w {
                    if x >= 640 || y >= 480 {
                        assert!(!is_red(pixel(&rgba, w, x, y)), "margin at {x},{y}: {options:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn at_the_size_limit_the_margins_come_out_of_the_model_area() {
        let mesh = cube([-200.0, -20.0, 0.0], [200.0, 20.0, 10.0]);
        let objects = [RenderObject { name: "wide", mesh: &mesh, color: Some(RED), face_colors: None, visible: true }];
        let options = RenderOptions { supersample: 1, ..wide_top_view(MAX_SIZE, MAX_SIZE) };
        let output = render_with_overlay(&objects, &options, Reserve { right: 300, min_height: 0 }, |_, _, _, _| {}).unwrap();
        assert_eq!((output.meta.width, output.meta.height), (MAX_SIZE, MAX_SIZE));
        let area = output.meta.model_area;
        assert_eq!(area.width, MAX_SIZE - 300);
        assert!(area.height < MAX_SIZE);
        let (w, _, rgba) = decode_png(&output.png);
        let (x0, _, x1, _) = red_extent(&rgba, w, area.width as usize, area.height as usize).unwrap();
        assert!(x0 > 0 && x1 + 1 < area.width as usize, "{x0}..{x1}");
        assert!(red_extent(&rgba, w, w, w).unwrap().2 == x1, "nothing beyond the area");
    }

    #[test]
    fn the_label_never_lands_on_the_model_at_any_aspect_ratio() {
        // Tall enough to fill the frame whichever way the area is narrow.
        let mesh = cube([-100.0, -100.0, 0.0], [100.0, 100.0, 10.0]);
        let objects = [RenderObject { name: "block", mesh: &mesh, color: Some(RED), face_colors: None, visible: true }];
        for (width, height) in [(64, 64), (120, 700), (200, 900), (399, 300), (400, 300), (899, 500), (900, 500), (1600, 300), (300, 1600), (2048, 200)] {
            let options = RenderOptions { supersample: 1, ..wide_top_view(width, height) };
            let output = render(&objects, &options).unwrap();
            let (w, h, rgba) = decode_png(&output.png);
            let area = output.meta.model_area;
            assert_eq!((area.width, area.height), (width, height));
            assert!(h as u32 > height, "{width}x{height}");
            let ink = [40, 40, 40, 255];
            let mut rightmost = 0;
            for y in 0..h {
                for x in 0..w {
                    let p = pixel(&rgba, w, x, y);
                    if (y as u32) < height {
                        assert_ne!(p, ink, "label on the model area at {x},{y} of {width}x{height}");
                    } else {
                        assert!(!is_red(p), "model on the band at {x},{y} of {width}x{height}");
                        if p == ink {
                            rightmost = rightmost.max(x);
                        }
                    }
                }
            }
            // Texts that do not fit are left out, never cut off.
            assert!(rightmost < width as usize, "{width}x{height}: {rightmost}");
            if width >= 200 {
                assert!(rightmost > 0, "{width}x{height}: no label");
            }
        }
    }

    #[test]
    fn perspective_without_a_grid_has_no_band_and_label_off_keeps_the_exact_size() {
        let mesh = cube([0.0; 3], [10.0; 3]);
        let objects = [RenderObject { name: "cube", mesh: &mesh, color: None, face_colors: None, visible: true }];
        let perspective = RenderOptions { label: true, grid: false, ..plain(200, 150) };
        let (w, h, _) = decode_png(&render(&objects, &perspective).unwrap().png);
        assert_eq!((w, h), (200, 150));
        let labelled = RenderOptions { label: true, grid: true, ..plain(200, 150) };
        let output = render(&objects, &labelled).unwrap();
        assert_eq!((output.meta.width, output.meta.height), (200, 150 + 17));
        assert_eq!((output.meta.model_area.width, output.meta.model_area.height), (200, 150));
    }

    #[test]
    fn a_short_contact_sheet_grows_rather_than_crop_its_tiles() {
        let mesh = cube([-10.0; 3], [10.0; 3]);
        let objects = [RenderObject { name: "cube", mesh: &mesh, color: Some(RED), face_colors: None, visible: true }];
        let options = RenderOptions { projection: Projection::Orthographic, grid: true, label: true, ..plain(128, 64) };
        let sheet = render_sheet(&objects, &options, &[NamedView::Front, NamedView::Top]).unwrap();
        let (_, h, _) = decode_png(&sheet.png);
        assert_eq!(h as u32, sheet.height);
        for tile in &sheet.tiles {
            let area = tile.meta.model_area;
            assert_eq!(area.height, MIN_SIZE);
            assert!(area.y + area.height <= tile.meta.height && tile.meta.height == sheet.height, "{area:?} in {}", tile.meta.height);
        }
    }

    #[test]
    fn a_contact_sheet_keeps_its_size_and_every_tile_its_margins_free_of_the_model() {
        let mesh = cube([-100.0, -100.0, -100.0], [100.0, 100.0, 100.0]);
        let objects = [RenderObject { name: "block", mesh: &mesh, color: Some(RED), face_colors: None, visible: true }];
        let options = RenderOptions { projection: Projection::Orthographic, grid: true, label: true, background: Some(Color([255, 255, 255, 255])), ..plain(800, 400) };
        let sheet = render_sheet(&objects, &options, &[NamedView::Front, NamedView::Top]).unwrap();
        let (w, h, rgba) = decode_png(&sheet.png);
        assert_eq!((w, h), (800, 400));
        for tile in &sheet.tiles {
            let area = tile.meta.model_area;
            assert_eq!((tile.meta.width, tile.meta.height), (400, 400));
            assert!(area.y > 0 && area.y + area.height < 400, "{area:?}");
            let mut inside = 0;
            for y in 0..400usize {
                for x in tile.x as usize..(tile.x + 400) as usize {
                    if is_red(pixel(&rgba, w, x, y)) {
                        assert!(y as u32 >= area.y && (y as u32) < area.y + area.height, "tile {} at {x},{y}", tile.view);
                        inside += 1;
                    }
                }
            }
            assert!(inside > 1000, "{}", tile.view);
        }
    }

    #[test]
    fn zoom_narrows_a_custom_perspective_eye_and_the_scale_follows() {
        let mesh = cube([-5.0, -5.0, -5.0], [5.0, 5.0, 5.0]);
        let objects = [RenderObject { name: "cube", mesh: &mesh, color: None, face_colors: None, visible: true }];
        let shot = |zoom: f64| {
            let options = RenderOptions { eye: Some([0.0, -100.0, 0.0]), target: Some([0.0; 3]), zoom, view_height_mm: Some(7.0), ..plain(200, 200) };
            assert_eq!(options.projection, Projection::Perspective);
            render(&objects, &options).unwrap()
        };
        let drawn = |output: &RenderOutput| {
            let (w, h, rgba) = decode_png(&output.png);
            let background = pixel(&rgba, w as usize, 0, 0);
            (0..h as usize).filter(|&y| pixel(&rgba, w as usize, w as usize / 2, y) != background).count() as f64
        };
        let (one, two) = (shot(1.0), shot(2.0));
        // The cube spans twice the pixels, seen from the same place.
        let ratio = drawn(&two) / drawn(&one);
        assert!((ratio - 2.0).abs() < 0.1, "{ratio}");
        assert_eq!(one.meta.camera.eye, two.meta.camera.eye);
        // The scale says so, and is the plain perspective scale: viewHeightMm
        // sets the orthographic one only.
        assert!((one.meta.mm_per_pixel / two.meta.mm_per_pixel - 2.0).abs() < 1e-9);
        let fov = (one.meta.camera.fov_deg.unwrap().to_radians() * 0.5).tan();
        assert!((one.meta.mm_per_pixel - 2.0 * 100.0 * fov / 200.0).abs() < 1e-9);
        // A front face's width in pixels matches the scale at its depth.
        let front = drawn(&two);
        let expected = 10.0 / (two.meta.mm_per_pixel * 95.0 / 100.0);
        assert!((front - expected).abs() < 3.0, "{front} {expected}");
        // Projecting back follows the narrowed lens.
        let [_, y] = project_point(&two.meta, [0.0, 0.0, 5.0]).unwrap();
        assert!((100.0 - y - 5.0 / two.meta.mm_per_pixel).abs() < 0.5, "{y}");
    }

    #[test]
    fn front_ortho_cube_covers_expected_region() {
        let mesh = cube([-10.0, -10.0, -10.0], [10.0, 10.0, 10.0]);
        let objects = [RenderObject { name: "cube", mesh: &mesh, color: None, face_colors: None, visible: true }];
        let options = RenderOptions {
            view: NamedView::Front,
            projection: Projection::Orthographic,
            view_height_mm: Some(40.0),
            ..plain(200, 200)
        };
        let output = render(&objects, &options).unwrap();
        let (w, h, rgba) = decode_png(&output.png);
        assert_eq!((w, h), (200, 200));
        assert!((output.meta.mm_per_pixel - 0.2).abs() < 1e-9);
        // The cube spans 20 mm = 100 px centred: pixels 50..150.
        for y in 0..200 {
            for x in 0..200 {
                let inside = (52..148).contains(&x) && (52..148).contains(&y);
                let outside = !(48..152).contains(&x) || !(48..152).contains(&y);
                let p = pixel(&rgba, w, x, y);
                if inside {
                    assert!(!is_background(p), "expected cube at {x},{y}");
                    // Yellow-ish: red and green well above blue.
                    assert!(p[0] > p[2] + 60 && p[1] > p[2] + 40, "colour {p:?}");
                } else if outside {
                    assert!(is_background(p), "expected background at {x},{y}: {p:?}");
                }
            }
        }
        assert_eq!(output.meta.camera.view, "front");
        assert_eq!(output.meta.visible_bounds.as_ref().unwrap().min, [-10.0; 3]);
    }

    #[test]
    fn an_object_painted_per_face_draws_each_triangle_in_its_colour() {
        let left = cube([-20.0, -5.0, -5.0], [-10.0, 5.0, 5.0]);
        let right = cube([10.0, -5.0, -5.0], [20.0, 5.0, 5.0]);
        let mut mesh = left.clone();
        mesh.triangles.extend(right.triangles.iter().cloned());
        let colors = FaceColors {
            palette: vec![Some([1.0, 0.0, 0.0, 1.0]), Some([0.0, 0.0, 1.0, 1.0])],
            faces: (0..mesh.triangles.len())
                .map(|index| u16::from(index >= left.triangles.len()))
                .collect(),
        };
        let options = RenderOptions {
            view: NamedView::Front,
            projection: Projection::Orthographic,
            view_height_mm: Some(50.0),
            target: Some([0.0; 3]),
            ..plain(200, 200)
        };
        let objects = [RenderObject {
            name: "two-tone",
            mesh: &mesh,
            color: Some([255, 0, 0, 255]),
            face_colors: Some(&colors),
            visible: true,
        }];
        let output = render(&objects, &options).unwrap();
        let (w, _, rgba) = decode_png(&output.png);
        let red = pixel(&rgba, w, 40, 100);
        let blue = pixel(&rgba, w, 160, 100);
        assert!(red[0] > 100 && red[1] < 30 && red[2] < 30, "{red:?}");
        assert!(blue[2] > 100 && blue[0] < 30 && blue[1] < 30, "{blue:?}");
        assert_eq!(output.meta.objects.len(), 1);
    }

    #[test]
    fn colours_are_honoured_and_hidden_objects_skipped() {
        let left = cube([-20.0, -5.0, -5.0], [-10.0, 5.0, 5.0]);
        let right = cube([10.0, -5.0, -5.0], [20.0, 5.0, 5.0]);
        let options = RenderOptions {
            view: NamedView::Front,
            projection: Projection::Orthographic,
            view_height_mm: Some(50.0),
            // Framing follows the visible bounds; pin it so pixels line up.
            target: Some([0.0; 3]),
            ..plain(200, 200)
        };
        let objects = [
            RenderObject { name: "red", mesh: &left, color: Some([255, 0, 0, 255]), face_colors: None, visible: true },
            RenderObject { name: "blue", mesh: &right, color: Some([0, 0, 255, 255]), face_colors: None, visible: true },
        ];
        let output = render(&objects, &options).unwrap();
        let (w, _, rgba) = decode_png(&output.png);
        // x = -15 mm -> px 100 - 15 / 0.25 = 40; x = 15 -> 160.
        let red = pixel(&rgba, w, 40, 100);
        let blue = pixel(&rgba, w, 160, 100);
        assert!(red[0] > 100 && red[1] < 30 && red[2] < 30, "{red:?}");
        assert!(blue[2] > 100 && blue[0] < 30 && blue[1] < 30, "{blue:?}");

        let objects = [
            RenderObject { name: "red", mesh: &left, color: Some([255, 0, 0, 255]), face_colors: None, visible: true },
            RenderObject { name: "blue", mesh: &right, color: Some([0, 0, 255, 255]), face_colors: None, visible: false },
        ];
        let output = render(&objects, &options).unwrap();
        let (w, _, rgba) = decode_png(&output.png);
        assert!(is_background(pixel(&rgba, w, 160, 100)));
        assert!(!is_background(pixel(&rgba, w, 40, 100)));
        assert_eq!(output.meta.triangles_drawn, 12);
        assert!(!output.meta.objects[1].visible);
        assert_eq!(output.meta.objects[1].color, "#0000ff");
    }

    #[test]
    fn section_plane_removes_half_and_shows_cap() {
        let mesh = cube([-10.0, -10.0, -10.0], [10.0, 10.0, 10.0]);
        let objects = [RenderObject { name: "cube", mesh: &mesh, color: None, face_colors: None, visible: true }];
        let base = RenderOptions {
            view: NamedView::Top,
            projection: Projection::Orthographic,
            view_height_mm: Some(40.0),
            ..plain(200, 200)
        };
        // Remove x > 0 seen from the top: the right half disappears.
        let options = RenderOptions {
            section: Some(SectionPlane::Axis { axis: Axis::X, offset: 0.0, remove: Some(CutSide::Positive) }),
            ..base.clone()
        };
        let output = render(&objects, &options).unwrap();
        let (w, _, rgba) = decode_png(&output.png);
        assert!(!is_background(pixel(&rgba, w, 70, 100)));
        assert!(is_background(pixel(&rgba, w, 130, 100)));
        assert_eq!(output.meta.section.as_ref().unwrap().normal, [1.0, 0.0, 0.0]);

        // Cut the top off (auto side faces the camera): the cap shows instead
        // of the yellow top face.
        let options = RenderOptions {
            section: Some(SectionPlane::Axis { axis: Axis::Z, offset: 0.0, remove: None }),
            ..base
        };
        let output = render(&objects, &options).unwrap();
        let (w, _, rgba) = decode_png(&output.png);
        assert_eq!(output.meta.section.as_ref().unwrap().normal, [0.0, 0.0, 1.0]);
        let cap = pixel(&rgba, w, 100, 101);
        assert!(!is_background(cap));
        assert!(cap[1] < 150 && cap[0] > cap[2], "cap should be dark red-ish: {cap:?}");
    }

    #[test]
    fn options_deserialize_from_json() {
        let options: RenderOptions = serde_json::from_str(
            r##"{"width": 640, "view": "iso", "projection": "ortho", "background": "#102030",
                 "section": {"axis": "y", "offset": 2.5}}"##,
        )
        .unwrap();
        assert_eq!(options.width, 640);
        assert_eq!(options.view, NamedView::Iso);
        assert_eq!(options.background, Some(Color([0x10, 0x20, 0x30, 255])));
        assert_eq!(options.section, Some(SectionPlane::Axis { axis: Axis::Y, offset: 2.5, remove: None }));
        let options: RenderOptions =
            serde_json::from_str(r#"{"eye": [1,2,3], "background": [1, 0.5, 0], "section": {"point": [0,0,0], "normal": [0,0,1]}}"#).unwrap();
        assert_eq!(options.background, Some(Color([255, 128, 0, 255])));
        assert!(serde_json::from_str::<RenderOptions>(r#"{"bogus": 1}"#).is_err());
        assert_eq!(parse_color("#abc"), Some([0xaa, 0xbb, 0xcc, 255]));
    }

    #[test]
    fn custom_camera_and_limits() {
        let mesh = sphere([0.0, 0.0, 0.0], 10.0, 24);
        let objects = [RenderObject { name: "ball", mesh: &mesh, color: None, face_colors: None, visible: true }];
        let options = RenderOptions { eye: Some([50.0, -50.0, 30.0]), width: 10, height: 99999, ..plain(0, 0) };
        let output = render(&objects, &options).unwrap();
        assert_eq!((output.meta.width, output.meta.height), (MIN_SIZE, MAX_SIZE));
        assert_eq!(output.meta.camera.view, "custom");
        assert!(output.meta.supersample <= 3);
        // Eye inside the sphere: near clipping must not panic and draws the inside.
        let options = RenderOptions { eye: Some([0.0, 0.0, 0.0]), target: Some([1.0, 0.0, 0.0]), ..plain(100, 100) };
        let output = render(&objects, &options).unwrap();
        let (w, _, rgba) = decode_png(&output.png);
        assert!(!is_background(pixel(&rgba, w, 50, 50)));
        let bad = RenderOptions { eye: Some([0.0, 0.0, 0.0]), target: Some([0.0, 0.0, 0.0]), ..plain(100, 100) };
        assert!(render(&objects, &bad).is_err());
        // Empty scenes still render.
        let output = render(&[], &RenderOptions::default()).unwrap();
        assert!(output.meta.visible_bounds.is_none());
    }

    #[test]
    fn pathological_inputs_are_rejected_cheaply() {
        // Thousands of triangles that each cover the whole screen.
        let big = Triangle { vertices: [v(-1e3, 0.0, -1e3), v(1e3, 0.0, -1e3), v(0.0, 0.0, 1e3)], normal: v(0.0, 0.0, 0.0) };
        let mut triangles = vec![big; 20_000];
        triangles.push(Triangle { vertices: [v(0.0, 0.0, 0.0), v(1.0, 0.0, 0.0), v(0.0, 0.0, 1.0)], normal: v(0.0, 0.0, 0.0) });
        let mesh = Mesh { triangles };
        let objects = [RenderObject { name: "wall", mesh: &mesh, color: None, face_colors: None, visible: true }];
        let options = RenderOptions { view: NamedView::Front, target: Some([0.0; 3]), view_height_mm: Some(1.0), projection: Projection::Orthographic, ..RenderOptions::default() };
        let start = std::time::Instant::now();
        assert_eq!(render(&objects, &options).unwrap_err(), RenderError::TooComplex);
        if !cfg!(debug_assertions) {
            assert!(start.elapsed().as_secs_f64() < 5.0);
        }
        // Non-finite and absurd coordinates neither panic nor hang.
        let mesh = Mesh {
            triangles: vec![
                Triangle { vertices: [v(f64::NAN, 0.0, 0.0), v(1.0, 0.0, 0.0), v(0.0, 1.0, 0.0)], normal: v(0.0, 0.0, 0.0) },
                Triangle { vertices: [v(1e300, 0.0, 0.0), v(-1e300, 0.0, 0.0), v(0.0, 1e300, 1.0)], normal: v(0.0, 0.0, 0.0) },
            ],
        };
        let objects = [RenderObject { name: "bad", mesh: &mesh, color: None, face_colors: None, visible: true }];
        let _ = render(&objects, &RenderOptions::default());
    }

    #[test]
    fn long_thin_diagonal_parts_are_charged_their_area_not_their_bounding_box() {
        // Review regression: `rotate([0,45,0]) cylinder(r=1,h=100,$fn=200)`
        // was refused as too expensive because every sliver facet was
        // charged its whole screen bounding box.
        let mesh = crate::engine::compile("rotate([0,45,0]) cylinder(r=1,h=100,$fn=200);", crate::engine::Quality::Preview, None)
            .expect("compiles")
            .mesh;
        let objects = [RenderObject { name: "rod", mesh: &mesh, color: None, face_colors: None, visible: true }];
        for options in [RenderOptions::default(), RenderOptions { width: 800, height: 600, ..RenderOptions::default() }] {
            let output = render(&objects, &options).expect("a thin rod renders at default options");
            assert_eq!(output.meta.supersample, options.supersample, "rendered without falling back");
            let (w, h, rgba) = decode_png(&output.png);
            assert!((0..w * h).any(|i| rgba[i * 4..i * 4 + 3] != DEFAULT_BACKGROUND[..3]), "the rod is drawn");
        }
    }

    #[test]
    fn scenes_over_budget_fall_back_to_a_lower_supersample() {
        // 50 screen-filling slabs (front and back faces both drawn): over the
        // work cap at ss 2, within it at ss 1.
        let mut triangles = Vec::new();
        for layer in 0..50 {
            let y = layer as f64 * 0.01;
            let quad = cube([-10.0, y, -10.0], [10.0, y + 0.001, 10.0]);
            triangles.extend(quad.triangles);
        }
        let mesh = Mesh { triangles };
        let objects = [RenderObject { name: "stack", mesh: &mesh, color: None, face_colors: None, visible: true }];
        let options = RenderOptions {
            width: 2048,
            height: 1536,
            supersample: 2,
            view: NamedView::Front,
            projection: Projection::Orthographic,
            target: Some([0.0; 3]),
            view_height_mm: Some(1.0),
            ..RenderOptions::default()
        };
        let output = render(&objects, &options).expect("renders at a lower factor");
        assert_eq!(output.meta.supersample, 1);
    }

    #[test]
    fn translucent_colours_show_what_is_behind_them() {
        // Seen from the front, a translucent red slab sits in front of an
        // opaque blue cube; with alpha the blue shows through.
        let slab = cube([-5.0, -20.0, -5.0], [5.0, -15.0, 5.0]);
        let back = cube([-5.0, 0.0, -5.0], [5.0, 10.0, 5.0]);
        let options = RenderOptions {
            view: NamedView::Front,
            projection: Projection::Orthographic,
            view_height_mm: Some(20.0),
            target: Some([0.0; 3]),
            ..plain(100, 100)
        };
        let shot = |alpha: u8| {
            let objects = [
                RenderObject { name: "slab", mesh: &slab, color: Some([255, 0, 0, alpha]), face_colors: None, visible: true },
                RenderObject { name: "back", mesh: &back, color: Some([0, 0, 255, 255]), face_colors: None, visible: true },
            ];
            let output = render(&objects, &options).unwrap();
            let (w, _, rgba) = decode_png(&output.png);
            pixel(&rgba, w, 50, 50)
        };
        let opaque = shot(255);
        let half = shot(128);
        let clear = shot(0);
        assert!(opaque[0] > 100 && opaque[2] < 30, "opaque slab hides the cube: {opaque:?}");
        assert!(half[0] > 60 && half[2] > 60, "both colours mix: {half:?}");
        assert!(clear[2] > 100 && clear[0] < 30, "alpha 0 shows only the cube: {clear:?}");
        // Over an empty background the slab still tints it.
        let objects = [RenderObject { name: "slab", mesh: &slab, color: Some([255, 0, 0, 128]), face_colors: None, visible: true }];
        let output = render(&objects, &options).unwrap();
        let (w, _, rgba) = decode_png(&output.png);
        let tinted = pixel(&rgba, w, 50, 50);
        assert!(tinted[0] > 200 && tinted[1] < 220 && tinted[1] > 60, "{tinted:?}");
    }

    #[test]
    fn renders_200k_triangles_quickly() {
        let mesh = sphere([0.0, 0.0, 0.0], 20.0, 448); // 448 * 224 * 2 = 200,704 triangles
        assert!(mesh.triangles.len() > 200_000);
        let objects = [RenderObject { name: "ball", mesh: &mesh, color: None, face_colors: None, visible: true }];
        let start = std::time::Instant::now();
        let output = render(&objects, &RenderOptions::default()).unwrap();
        let elapsed = start.elapsed();
        eprintln!("200k triangles at 1024x768 ss{}: {:?}, png {} bytes", output.meta.supersample, elapsed, output.png.len());
        if !cfg!(debug_assertions) {
            assert!(elapsed.as_secs_f64() < 2.0, "render took {elapsed:?}");
        }
    }

    /// Writes sample images when RASTER_PREVIEW_DIR is set, for eyeballing.
    #[test]
    fn write_preview_images() {
        let Ok(dir) = std::env::var("RASTER_PREVIEW_DIR") else { return };
        std::fs::create_dir_all(&dir).unwrap();
        let compile = |source: &str| {
            crate::engine::compile(source, crate::engine::Quality::Render, None).expect("compiles").mesh
        };
        let base = compile("difference() { cube([40, 30, 12]); translate([20, 15, -1]) cylinder(h = 20, r = 8, $fn = 48); translate([4, 4, 6]) cube([8, 22, 10]); }");
        let lid = compile("translate([0, 0, 14]) difference() { cube([40, 30, 4]); translate([2, 2, -1]) cube([36, 26, 3]); }");
        let knob = compile("translate([20, 15, 18]) sphere(r = 6, $fn = 40);");
        let peg = compile("translate([-15, 15, 0]) rotate([0, 0, 30]) cylinder(h = 25, r1 = 6, r2 = 3, $fn = 6);");
        let objects = [
            RenderObject { name: "base", mesh: &base, color: None, face_colors: None, visible: true },
            RenderObject { name: "lid", mesh: &lid, color: Some([0x3a, 0x86, 0xd6, 255]), face_colors: None, visible: true },
            RenderObject { name: "knob", mesh: &knob, color: Some([0xd6, 0x3a, 0x5a, 255]), face_colors: None, visible: true },
            RenderObject { name: "peg", mesh: &peg, color: Some([0x4c, 0xb0, 0x5a, 255]), face_colors: None, visible: true },
        ];
        let shots = [
            ("default.png", RenderOptions::default()),
            ("iso_ortho.png", RenderOptions { view: NamedView::Iso, projection: Projection::Orthographic, ..RenderOptions::default() }),
            ("front_section.png", RenderOptions {
                view: NamedView::Front,
                projection: Projection::Orthographic,
                section: Some(SectionPlane::Axis { axis: Axis::Y, offset: 15.0, remove: None }),
                ..RenderOptions::default()
            }),
            ("iso_section.png", RenderOptions {
                view: NamedView::Iso,
                section: Some(SectionPlane::Axis { axis: Axis::Y, offset: 15.0, remove: None }),
                ..RenderOptions::default()
            }),
            ("top.png", RenderOptions { view: NamedView::Top, projection: Projection::Orthographic, width: 600, height: 600, ..RenderOptions::default() }),
            ("custom.png", RenderOptions { eye: Some([-60.0, -40.0, 50.0]), target: Some([20.0, 15.0, 10.0]), width: 640, height: 480, ..RenderOptions::default() }),
        ];
        for (name, options) in shots {
            let output = render(&objects, &options).unwrap();
            std::fs::write(format!("{dir}/{name}"), &output.png).unwrap();
            eprintln!("{name}: {}", serde_json::to_string(&output.meta.camera).unwrap());
        }
    }
}
