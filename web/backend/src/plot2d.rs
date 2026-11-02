//! A small 2D plotting canvas for measured drawings: section overlays and
//! curve fits drawn in true millimetre coordinates at one fixed scale, with
//! a labelled millimetre grid, a scale bar, polylines, markers and text,
//! encoded as PNG. Several plots of one scale tile into a sheet with
//! [`sheet`].

use crate::raster::{self, Rgba};

pub const INK: Rgba = [30, 30, 30, 255];
pub const GRID: Rgba = [225, 225, 225, 255];
pub const GRID_MAJOR: Rgba = [195, 195, 195, 255];
pub const BACKGROUND: Rgba = [255, 255, 255, 255];
/// Most grid lines drawn per axis.
const MAX_GRID_LINES: f64 = 400.0;

/// A canvas mapping a millimetre window onto pixels at `px_per_mm`
/// (y up, as in the plane basis).
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    /// Model-space (plane) coordinates at the pixel centre of the plot area.
    pub center: [f64; 2],
    pub px_per_mm: f64,
    /// Font scale (1 or 2).
    pub text_scale: usize,
}

impl Canvas {
    pub fn new(width: u32, height: u32, center: [f64; 2], px_per_mm: f64) -> Self {
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        for chunk in pixels.chunks_exact_mut(4) {
            chunk.copy_from_slice(&BACKGROUND);
        }
        Self { width, height, pixels, center, px_per_mm, text_scale: if width >= 700 { 2 } else { 1 } }
    }

    /// Pixel position of a plane point.
    pub fn to_px(&self, p: [f64; 2]) -> [f64; 2] {
        [
            self.width as f64 * 0.5 + (p[0] - self.center[0]) * self.px_per_mm,
            self.height as f64 * 0.5 - (p[1] - self.center[1]) * self.px_per_mm,
        ]
    }

    /// Plane coordinates visible: (min, max).
    pub fn window(&self) -> ([f64; 2], [f64; 2]) {
        let half = [self.width as f64 * 0.5 / self.px_per_mm, self.height as f64 * 0.5 / self.px_per_mm];
        ([self.center[0] - half[0], self.center[1] - half[1]], [self.center[0] + half[0], self.center[1] + half[1]])
    }

    fn blend(&mut self, x: i64, y: i64, color: Rgba, coverage: f64) {
        if x < 0 || y < 0 || x >= self.width as i64 || y >= self.height as i64 {
            return;
        }
        let a = (color[3] as f64 / 255.0) * coverage.clamp(0.0, 1.0);
        let offset = (y as usize * self.width as usize + x as usize) * 4;
        for k in 0..3 {
            let old = self.pixels[offset + k] as f64;
            self.pixels[offset + k] = (old + (color[k] as f64 - old) * a).round() as u8;
        }
        self.pixels[offset + 3] = 255;
    }

    /// Anti-aliased line of `width` pixels between pixel positions.
    pub fn line_px(&mut self, a: [f64; 2], b: [f64; 2], width: f64, color: Rgba) {
        let (min_x, max_x) = (a[0].min(b[0]) - width - 1.0, a[0].max(b[0]) + width + 1.0);
        let (min_y, max_y) = (a[1].min(b[1]) - width - 1.0, a[1].max(b[1]) + width + 1.0);
        if max_x < 0.0 || max_y < 0.0 || min_x > self.width as f64 || min_y > self.height as f64 {
            return;
        }
        let (min_x, max_x) = (min_x.max(0.0) as i64, max_x.min(self.width as f64) as i64);
        let (min_y, max_y) = (min_y.max(0.0) as i64, max_y.min(self.height as f64) as i64);
        // Very long lines off a zoomed plot are clipped by the box above.
        if (max_x - min_x) * (max_y - min_y) > 40_000_000 {
            return;
        }
        let d = [b[0] - a[0], b[1] - a[1]];
        let len2 = d[0] * d[0] + d[1] * d[1];
        let half = width * 0.5;
        for y in min_y..=max_y {
            for x in min_x..=max_x {
                let p = [x as f64 + 0.5, y as f64 + 0.5];
                let t = if len2 > 0.0 { (((p[0] - a[0]) * d[0] + (p[1] - a[1]) * d[1]) / len2).clamp(0.0, 1.0) } else { 0.0 };
                let q = [a[0] + d[0] * t - p[0], a[1] + d[1] * t - p[1]];
                let dist = (q[0] * q[0] + q[1] * q[1]).sqrt();
                let coverage = half + 0.5 - dist;
                if coverage > 0.0 {
                    self.blend(x, y, color, coverage);
                }
            }
        }
    }

    /// A polyline in plane coordinates; `closed` joins the last point to the first.
    pub fn polyline(&mut self, points: &[[f64; 2]], closed: bool, width: f64, color: Rgba) {
        let px: Vec<[f64; 2]> = points.iter().map(|p| self.to_px(*p)).collect();
        for pair in px.windows(2) {
            self.line_px(pair[0], pair[1], width, color);
        }
        if closed && px.len() > 2 {
            self.line_px(px[px.len() - 1], px[0], width, color);
        }
    }

    /// A dashed polyline in plane coordinates.
    pub fn dashed(&mut self, points: &[[f64; 2]], closed: bool, width: f64, dash_px: f64, color: Rgba) {
        let mut px: Vec<[f64; 2]> = points.iter().map(|p| self.to_px(*p)).collect();
        if closed && px.len() > 2 {
            px.push(px[0]);
        }
        let mut phase = 0.0;
        for pair in px.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let len = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
            let mut s = 0.0;
            while s < len {
                let on = ((phase + s) / dash_px) as i64 % 2 == 0;
                let e = (s + dash_px - (phase + s) % dash_px).min(len);
                if on {
                    let f = |t: f64| [a[0] + (b[0] - a[0]) * t / len, a[1] + (b[1] - a[1]) * t / len];
                    self.line_px(f(s), f(e), width, color);
                }
                s = e + 1e-9;
            }
            phase += len;
        }
    }

    pub fn dot(&mut self, p: [f64; 2], radius: f64, color: Rgba) {
        let c = self.to_px(p);
        let r = radius.ceil() as i64 + 1;
        for dy in -r..=r {
            for dx in -r..=r {
                let x = c[0].floor() as i64 + dx;
                let y = c[1].floor() as i64 + dy;
                let d = ((x as f64 + 0.5 - c[0]).powi(2) + (y as f64 + 0.5 - c[1]).powi(2)).sqrt();
                let coverage = radius + 0.5 - d;
                if coverage > 0.0 {
                    self.blend(x, y, color, coverage);
                }
            }
        }
    }

    pub fn cross_mark(&mut self, p: [f64; 2], size: f64, color: Rgba) {
        let c = self.to_px(p);
        self.line_px([c[0] - size, c[1] - size], [c[0] + size, c[1] + size], 1.6, color);
        self.line_px([c[0] - size, c[1] + size], [c[0] + size, c[1] - size], 1.6, color);
    }

    pub fn ring(&mut self, p: [f64; 2], radius: f64, width: f64, color: Rgba) {
        let c = self.to_px(p);
        let steps = 32;
        for i in 0..steps {
            let a0 = i as f64 / steps as f64 * std::f64::consts::TAU;
            let a1 = (i + 1) as f64 / steps as f64 * std::f64::consts::TAU;
            self.line_px(
                [c[0] + radius * a0.cos(), c[1] + radius * a0.sin()],
                [c[0] + radius * a1.cos(), c[1] + radius * a1.sin()],
                width,
                color,
            );
        }
    }

    /// Text at a pixel position, on a translucent white backdrop.
    pub fn text_px(&mut self, x: f64, y: f64, text: &str, color: Rgba) {
        let s = self.text_scale;
        let w = raster::text_width(text, s) as i64;
        let (x, y) = (x.round() as i64, y.round() as i64);
        let (x, y) = (x.clamp(0, (self.width as i64 - w - 2).max(0)), y.clamp(0, (self.height as i64 - 9 * s as i64).max(0)));
        for yy in y - 1..y + 8 * s as i64 + 1 {
            for xx in x - 1..x + w + 1 {
                self.blend(xx, yy, [255, 255, 255, 255], 0.8);
            }
        }
        raster::draw_text_colored(&mut self.pixels, self.width as usize, self.height as usize, x as usize, y as usize, text, s, color);
    }


    /// Millimetre grid with labelled major lines and a scale bar; returns the
    /// grid spacing used.
    pub fn grid(&mut self, axis_names: (&str, &str)) -> f64 {
        let (lo, hi) = self.window();
        let spacing = nice_step(40.0 / self.px_per_mm);
        // Integer-indexed lines: a float accumulator stalls once spacing
        // drops below the coordinates' ulp (far-off plots), and the count is
        // bounded whatever the window.
        let first = [(lo[0] / spacing).floor(), (lo[1] / spacing).floor()];
        let count = [((hi[0] - lo[0]) / spacing).ceil().min(MAX_GRID_LINES) as i64, ((hi[1] - lo[1]) / spacing).ceil().min(MAX_GRID_LINES) as i64];
        for k in 0..=count[0] {
            let index = first[0] + k as f64;
            let x = index * spacing;
            let is_major = index.rem_euclid(5.0) == 0.0;
            let a = self.to_px([x, lo[1]]);
            let b = self.to_px([x, hi[1]]);
            self.line_px(a, b, 1.0, if is_major { GRID_MAJOR } else { GRID });
            if is_major {
                let label = raster_label(x);
                self.text_px(a[0] + 2.0, self.height as f64 - 10.0 * self.text_scale as f64, &label, [110, 110, 110, 255]);
            }
        }
        for k in 0..=count[1] {
            let index = first[1] + k as f64;
            let y = index * spacing;
            let is_major = index.rem_euclid(5.0) == 0.0;
            let a = self.to_px([lo[0], y]);
            let b = self.to_px([hi[0], y]);
            self.line_px(a, b, 1.0, if is_major { GRID_MAJOR } else { GRID });
            if is_major {
                self.text_px(2.0, a[1] - 9.0 * self.text_scale as f64, &raster_label(y), [110, 110, 110, 255]);
            }
        }
        // Axes through the plane origin, when visible.
        let o = self.to_px([0.0, 0.0]);
        if o[0] >= 0.0 && o[0] <= self.width as f64 {
            self.line_px([o[0], 0.0], [o[0], self.height as f64], 1.2, [150, 150, 150, 255]);
        }
        if o[1] >= 0.0 && o[1] <= self.height as f64 {
            self.line_px([0.0, o[1]], [self.width as f64, o[1]], 1.2, [150, 150, 150, 255]);
        }
        // Scale bar bottom-right.
        let bar_mm = nice_step(self.width as f64 / 5.0 / self.px_per_mm);
        let bar_px = bar_mm * self.px_per_mm;
        let s = self.text_scale as f64;
        let y = self.height as f64 - 22.0 * s;
        let x1 = self.width as f64 - 10.0 * s;
        let x0 = x1 - bar_px;
        self.line_px([x0, y], [x1, y], 2.0 * s, INK);
        self.line_px([x0, y - 4.0 * s], [x0, y + 4.0 * s], 1.5 * s, INK);
        self.line_px([x1, y - 4.0 * s], [x1, y + 4.0 * s], 1.5 * s, INK);
        let label = format!("{} mm  {} u, {} v", raster_label(bar_mm), axis_names.0, axis_names.1);
        let w = raster::text_width(&label, self.text_scale) as f64;
        self.text_px(x1 - w, y - 13.0 * s, &label, INK);
        spacing
    }

    /// A dimension line between two plane points with its length written on it.
    pub fn dimension(&mut self, a: [f64; 2], b: [f64; 2], offset_px: [f64; 2], color: Rgba) {
        let pa = self.to_px(a);
        let pb = self.to_px(b);
        let qa = [pa[0] + offset_px[0], pa[1] + offset_px[1]];
        let qb = [pb[0] + offset_px[0], pb[1] + offset_px[1]];
        self.line_px(pa, qa, 1.0, color);
        self.line_px(pb, qb, 1.0, color);
        self.line_px(qa, qb, 1.2, color);
        let length = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
        let label = format!("{} mm", raster_label(length));
        let mid = [(qa[0] + qb[0]) * 0.5, (qa[1] + qb[1]) * 0.5];
        let w = raster::text_width(&label, self.text_scale) as f64;
        self.text_px(mid[0] - w * 0.5, mid[1] - 4.0 * self.text_scale as f64, &label, color);
    }

    pub fn png(&self) -> Vec<u8> {
        raster::encode_png(self.width, self.height, &self.pixels, false)
    }
}

/// 1, 2 or 5 times a power of ten, at least `raw`.
pub fn nice_step(raw: f64) -> f64 {
    if !(raw > 0.0) || !raw.is_finite() {
        return 1.0;
    }
    let power = 10f64.powf(raw.log10().floor());
    for factor in [1.0, 2.0, 5.0, 10.0] {
        if factor * power >= raw {
            return factor * power;
        }
    }
    10.0 * power
}

fn raster_label(value: f64) -> String {
    let rounded = (value * 1000.0).round() / 1000.0;
    let text = format!("{rounded}");
    if text == "-0" { "0".into() } else { text }
}

/// Tiles same-size canvases into one sheet, `columns` wide, with a 2-pixel
/// grey gutter; returns (png, width, height, tile origins).
pub fn sheet(tiles: &[Canvas], columns: usize) -> (Vec<u8>, u32, u32, Vec<[u32; 2]>) {
    let columns = columns.clamp(1, tiles.len().max(1));
    let rows = tiles.len().div_ceil(columns);
    let (tw, th) = tiles.first().map_or((1, 1), |t| (t.width, t.height));
    let gutter = 2u32;
    let width = columns as u32 * tw + (columns as u32 - 1) * gutter;
    let height = rows as u32 * th + (rows.max(1) as u32 - 1) * gutter;
    let mut pixels = vec![160u8; width as usize * height as usize * 4];
    let mut origins = Vec::new();
    for (i, tile) in tiles.iter().enumerate() {
        let (cx, cy) = ((i % columns) as u32 * (tw + gutter), (i / columns) as u32 * (th + gutter));
        origins.push([cx, cy]);
        for y in 0..th.min(tile.height) {
            let src = (y as usize * tile.width as usize) * 4;
            let dst = ((cy + y) as usize * width as usize + cx as usize) * 4;
            let n = tw.min(tile.width) as usize * 4;
            pixels[dst..dst + n].copy_from_slice(&tile.pixels[src..src + n]);
        }
    }
    for chunk in pixels.chunks_exact_mut(4) {
        chunk[3] = 255;
    }
    (raster::encode_png(width, height, &pixels, false), width, height, origins)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canvas_maps_millimetres_and_draws() {
        let mut canvas = Canvas::new(200, 100, [5.0, 0.0], 10.0);
        assert_eq!(canvas.to_px([5.0, 0.0]), [100.0, 50.0]);
        assert_eq!(canvas.to_px([6.0, 1.0]), [110.0, 40.0]);
        canvas.grid(("x", "y"));
        canvas.polyline(&[[0.0, 0.0], [10.0, 0.0]], false, 2.0, [255, 0, 0, 255]);
        let offset = (50 * 200 + 100) * 4;
        assert!(canvas.pixels[offset] > 200 && canvas.pixels[offset + 1] < 80);
        let png = canvas.png();
        assert_eq!(&png[1..4], b"PNG");
        let (sheet_png, w, h, origins) = sheet(&[Canvas::new(50, 40, [0.0, 0.0], 1.0), Canvas::new(50, 40, [0.0, 0.0], 1.0), Canvas::new(50, 40, [0.0, 0.0], 1.0)], 2);
        assert_eq!((w, h), (102, 82));
        assert_eq!(origins[2], [0, 42]);
        assert_eq!(&sheet_png[1..4], b"PNG");
        assert_eq!(nice_step(3.2), 5.0);
        // Far from the origin (spacing below the ulp) the grid still ends.
        let mut far = Canvas::new(100, 100, [1e17, 1e17], 10.0);
        far.grid(("x", "y"));
        assert_eq!(nice_step(0.07), 0.1);
    }
}
