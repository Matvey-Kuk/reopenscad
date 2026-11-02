//! A minimal TrueType (`glyf`) reader: enough to turn a string into outlines.
//!
//! The bundled faces are fixed, in-repo files, but the reader is still written
//! defensively — every read is bounds-checked and returns `None` rather than
//! panicking, and composite glyphs are followed to a bounded depth — so a bad
//! table degrades to a missing glyph instead of taking the process down.
//!
//! What it covers:
//!
//! * the table directory, `head`, `hhea`, `maxp`, `hmtx`, `loca`, `glyf`;
//! * `cmap` subtables of format 4 (BMP) and 12 (full Unicode);
//! * simple and composite glyphs, with offset, scale, x/y scale and 2x2
//!   component transforms;
//! * pair kerning from `GPOS` (lookup type 2, formats 1 and 2, including
//!   through extension lookups) for the `kern` feature, falling back to a
//!   format-0 `kern` table.
//!
//! What it deliberately does not: hinting (outlines are used at full design
//! resolution, so hints would change nothing), `GSUB` substitutions such as
//! ligatures, mark positioning, and variation data (a variable font is read at
//! its default instance).

use std::collections::HashMap;

/// One point of a TrueType contour, in font units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutlinePoint {
    pub x: f64,
    pub y: f64,
    pub on_curve: bool,
}

/// A parsed font face. Holds the raw bytes plus the offsets it needs.
pub struct Face {
    data: &'static [u8],
    pub units_per_em: f64,
    pub ascender: f64,
    pub descender: f64,
    pub line_gap: f64,
    number_of_glyphs: u16,
    number_of_h_metrics: u16,
    long_loca: bool,
    loca: usize,
    glyf: usize,
    glyf_length: usize,
    hmtx: usize,
    cmap: CharacterMap,
    kerning: HashMap<(u16, u16), i16>,
    class_kerning: Vec<ClassPairs>,
}

enum CharacterMap {
    Format4 { offset: usize },
    Format12 { offset: usize },
    Missing,
}

/// A GPOS pair-positioning subtable of format 2, kept in its compact class
/// form rather than expanded into every glyph pair it covers.
struct ClassPairs {
    coverage: Vec<(u16, u16, u16)>,
    first_classes: Vec<(u16, u16, u16)>,
    second_classes: Vec<(u16, u16, u16)>,
    class2_count: u16,
    adjustments: Vec<i16>,
}

fn read_u8(data: &[u8], offset: usize) -> Option<u8> {
    data.get(offset).copied()
}

fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    let bytes = data.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_i16(data: &[u8], offset: usize) -> Option<i16> {
    read_u16(data, offset).map(|value| value as i16)
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// 2.14 fixed point, used by composite glyph scales.
fn read_f2dot14(data: &[u8], offset: usize) -> Option<f64> {
    read_i16(data, offset).map(|value| value as f64 / 16384.0)
}

fn table(data: &[u8], tag: &[u8; 4]) -> Option<(usize, usize)> {
    let count = read_u16(data, 4)? as usize;
    for index in 0..count {
        let record = 12 + index * 16;
        if data.get(record..record + 4)? == tag {
            let offset = read_u32(data, record + 8)? as usize;
            let length = read_u32(data, record + 12)? as usize;
            data.get(offset..offset.checked_add(length)?)?;
            return Some((offset, length));
        }
    }
    None
}

/// Maximum nesting of composite glyphs. Real fonts use one or two levels; the
/// bound only exists so a cyclic reference cannot recurse forever.
const MAX_COMPOSITE_DEPTH: usize = 8;

impl Face {
    pub fn parse(data: &'static [u8]) -> Option<Face> {
        let (head, _) = table(data, b"head")?;
        let (hhea, _) = table(data, b"hhea")?;
        let (maxp, _) = table(data, b"maxp")?;
        let (hmtx, _) = table(data, b"hmtx")?;
        let (loca, _) = table(data, b"loca")?;
        let (glyf, glyf_length) = table(data, b"glyf")?;
        let units_per_em = read_u16(data, head + 18)? as f64;
        if units_per_em <= 0.0 {
            return None;
        }
        let long_loca = read_i16(data, head + 50)? == 1;
        let mut face = Face {
            data,
            units_per_em,
            ascender: read_i16(data, hhea + 4)? as f64,
            descender: read_i16(data, hhea + 6)? as f64,
            line_gap: read_i16(data, hhea + 8)? as f64,
            number_of_glyphs: read_u16(data, maxp + 4)?,
            number_of_h_metrics: read_u16(data, hhea + 34)?,
            long_loca,
            loca,
            glyf,
            glyf_length,
            hmtx,
            cmap: CharacterMap::Missing,
            kerning: HashMap::new(),
            class_kerning: Vec::new(),
        };
        face.cmap = face.find_character_map().unwrap_or(CharacterMap::Missing);
        if !face.load_gpos_kerning() {
            face.load_kern_table();
        }
        Some(face)
    }

    fn find_character_map(&self) -> Option<CharacterMap> {
        let data = self.data;
        let (cmap, _) = table(data, b"cmap")?;
        let count = read_u16(data, cmap + 2)? as usize;
        let mut format4 = None;
        let mut format12 = None;
        for index in 0..count {
            let record = cmap + 4 + index * 8;
            let platform = read_u16(data, record)?;
            let encoding = read_u16(data, record + 2)?;
            let offset = cmap + read_u32(data, record + 4)? as usize;
            let unicode = platform == 0 || (platform == 3 && (encoding == 1 || encoding == 10));
            if !unicode {
                continue;
            }
            match read_u16(data, offset)? {
                4 => format4 = format4.or(Some(offset)),
                12 => format12 = format12.or(Some(offset)),
                _ => {}
            }
        }
        format12
            .map(|offset| CharacterMap::Format12 { offset })
            .or(format4.map(|offset| CharacterMap::Format4 { offset }))
    }

    /// The glyph index for a character, or `None` if the face lacks it.
    pub fn glyph_index(&self, character: char) -> Option<u16> {
        let code = character as u32;
        let data = self.data;
        let index = match self.cmap {
            CharacterMap::Format4 { offset } => {
                if code > 0xFFFF {
                    return None;
                }
                let code = code as u16;
                let segments = read_u16(data, offset + 6)? as usize / 2;
                let ends = offset + 14;
                let starts = ends + segments * 2 + 2;
                let deltas = starts + segments * 2;
                let range_offsets = deltas + segments * 2;
                let mut found = None;
                for segment in 0..segments {
                    if read_u16(data, ends + segment * 2)? >= code {
                        found = Some(segment);
                        break;
                    }
                }
                let segment = found?;
                let start = read_u16(data, starts + segment * 2)?;
                if code < start {
                    return None;
                }
                let delta = read_u16(data, deltas + segment * 2)?;
                let range_offset_position = range_offsets + segment * 2;
                let range_offset = read_u16(data, range_offset_position)? as usize;
                if range_offset == 0 {
                    code.wrapping_add(delta)
                } else {
                    let glyph = read_u16(
                        data,
                        range_offset_position + range_offset + (code - start) as usize * 2,
                    )?;
                    if glyph == 0 {
                        0
                    } else {
                        glyph.wrapping_add(delta)
                    }
                }
            }
            CharacterMap::Format12 { offset } => {
                let groups = read_u32(data, offset + 12)? as usize;
                let mut result = 0;
                for group in 0..groups {
                    let record = offset + 16 + group * 12;
                    let start = read_u32(data, record)?;
                    let end = read_u32(data, record + 4)?;
                    if code < start {
                        break;
                    }
                    if code <= end {
                        result = read_u32(data, record + 8)? + (code - start);
                        break;
                    }
                }
                u16::try_from(result).ok()?
            }
            CharacterMap::Missing => 0,
        };
        (index != 0 && index < self.number_of_glyphs).then_some(index)
    }

    /// Horizontal advance of a glyph, in font units.
    pub fn advance(&self, glyph: u16) -> f64 {
        let metrics = self.number_of_h_metrics.max(1);
        let index = glyph.min(metrics - 1) as usize;
        read_u16(self.data, self.hmtx + index * 4).unwrap_or(0) as f64
    }

    /// Kerning adjustment to the advance of `left` when followed by `right`.
    pub fn kerning(&self, left: u16, right: u16) -> f64 {
        if let Some(value) = self.kerning.get(&(left, right)) {
            return *value as f64;
        }
        for pairs in &self.class_kerning {
            if lookup_range(&pairs.coverage, left).is_none() {
                continue;
            }
            let first = lookup_range(&pairs.first_classes, left).unwrap_or(0);
            let second = lookup_range(&pairs.second_classes, right).unwrap_or(0);
            let index = first as usize * pairs.class2_count as usize + second as usize;
            // A class subtable that covers the first glyph decides the pair,
            // zero included; later subtables are not consulted.
            return pairs.adjustments.get(index).copied().unwrap_or(0) as f64;
        }
        0.0
    }

    fn glyph_range(&self, glyph: u16) -> Option<(usize, usize)> {
        if glyph >= self.number_of_glyphs {
            return None;
        }
        let index = glyph as usize;
        let (start, end) = if self.long_loca {
            (
                read_u32(self.data, self.loca + index * 4)? as usize,
                read_u32(self.data, self.loca + index * 4 + 4)? as usize,
            )
        } else {
            (
                read_u16(self.data, self.loca + index * 2)? as usize * 2,
                read_u16(self.data, self.loca + index * 2 + 2)? as usize * 2,
            )
        };
        if end < start || end > self.glyf_length {
            return None;
        }
        Some((self.glyf + start, self.glyf + end))
    }

    /// The glyph's contours in font units, composites resolved.
    pub fn outline(&self, glyph: u16) -> Vec<Vec<OutlinePoint>> {
        let mut contours = Vec::new();
        self.append_outline(glyph, [1.0, 0.0, 0.0, 1.0, 0.0, 0.0], 0, &mut contours);
        contours
    }

    fn append_outline(
        &self,
        glyph: u16,
        transform: [f64; 6],
        depth: usize,
        output: &mut Vec<Vec<OutlinePoint>>,
    ) -> Option<()> {
        if depth > MAX_COMPOSITE_DEPTH {
            return None;
        }
        let (start, end) = self.glyph_range(glyph)?;
        if end - start < 10 {
            // An empty glyph — a space.
            return Some(());
        }
        let data = self.data;
        let contour_count = read_i16(data, start)?;
        if contour_count >= 0 {
            for contour in self.simple_glyph(start, contour_count as usize)? {
                output.push(
                    contour
                        .into_iter()
                        .map(|point| {
                            let (x, y) = apply(transform, point.x, point.y);
                            OutlinePoint {
                                x,
                                y,
                                on_curve: point.on_curve,
                            }
                        })
                        .collect(),
                );
            }
            return Some(());
        }
        // Composite glyph.
        let mut offset = start + 10;
        loop {
            let flags = read_u16(data, offset)?;
            let component = read_u16(data, offset + 2)?;
            offset += 4;
            let (dx, dy);
            if flags & 0x0001 != 0 {
                dx = read_i16(data, offset)? as f64;
                dy = read_i16(data, offset + 2)? as f64;
                offset += 4;
            } else {
                dx = read_u8(data, offset)? as i8 as f64;
                dy = read_u8(data, offset + 1)? as i8 as f64;
                offset += 2;
            }
            let mut matrix = [1.0, 0.0, 0.0, 1.0];
            if flags & 0x0008 != 0 {
                let scale = read_f2dot14(data, offset)?;
                matrix = [scale, 0.0, 0.0, scale];
                offset += 2;
            } else if flags & 0x0040 != 0 {
                matrix = [
                    read_f2dot14(data, offset)?,
                    0.0,
                    0.0,
                    read_f2dot14(data, offset + 2)?,
                ];
                offset += 4;
            } else if flags & 0x0080 != 0 {
                matrix = [
                    read_f2dot14(data, offset)?,
                    read_f2dot14(data, offset + 2)?,
                    read_f2dot14(data, offset + 4)?,
                    read_f2dot14(data, offset + 6)?,
                ];
                offset += 8;
            }
            // ARGS_ARE_XY_VALUES; point-matching anchors are not supported and
            // fall back to no offset, which no bundled face relies on.
            let (dx, dy) = if flags & 0x0002 != 0 {
                (dx, dy)
            } else {
                (0.0, 0.0)
            };
            let component_transform = [matrix[0], matrix[1], matrix[2], matrix[3], dx, dy];
            let combined = compose(transform, component_transform);
            self.append_outline(component, combined, depth + 1, output);
            if flags & 0x0020 == 0 {
                break;
            }
        }
        Some(())
    }

    fn simple_glyph(&self, start: usize, contour_count: usize) -> Option<Vec<Vec<OutlinePoint>>> {
        let data = self.data;
        let mut end_points = Vec::with_capacity(contour_count);
        for index in 0..contour_count {
            end_points.push(read_u16(data, start + 10 + index * 2)? as usize);
        }
        let point_count = end_points.last().map_or(0, |last| last + 1);
        let instructions_offset = start + 10 + contour_count * 2;
        let instruction_length = read_u16(data, instructions_offset)? as usize;
        let mut offset = instructions_offset + 2 + instruction_length;
        let mut flags = Vec::with_capacity(point_count);
        while flags.len() < point_count {
            let flag = read_u8(data, offset)?;
            offset += 1;
            flags.push(flag);
            if flag & 0x08 != 0 {
                let repeat = read_u8(data, offset)?;
                offset += 1;
                for _ in 0..repeat {
                    if flags.len() < point_count {
                        flags.push(flag);
                    }
                }
            }
        }
        let mut xs = Vec::with_capacity(point_count);
        let mut value = 0i32;
        for flag in &flags {
            if flag & 0x02 != 0 {
                let delta = read_u8(data, offset)? as i32;
                offset += 1;
                value += if flag & 0x10 != 0 { delta } else { -delta };
            } else if flag & 0x10 == 0 {
                value += read_i16(data, offset)? as i32;
                offset += 2;
            }
            xs.push(value);
        }
        let mut ys = Vec::with_capacity(point_count);
        value = 0;
        for flag in &flags {
            if flag & 0x04 != 0 {
                let delta = read_u8(data, offset)? as i32;
                offset += 1;
                value += if flag & 0x20 != 0 { delta } else { -delta };
            } else if flag & 0x20 == 0 {
                value += read_i16(data, offset)? as i32;
                offset += 2;
            }
            ys.push(value);
        }
        let mut contours = Vec::with_capacity(contour_count);
        let mut first = 0;
        for end in end_points {
            if end < first || end >= point_count {
                return None;
            }
            contours.push(
                (first..=end)
                    .map(|index| OutlinePoint {
                        x: xs[index] as f64,
                        y: ys[index] as f64,
                        on_curve: flags[index] & 0x01 != 0,
                    })
                    .collect(),
            );
            first = end + 1;
        }
        Some(contours)
    }

    /// Loads pair kerning from `GPOS`. Returns whether a `kern` feature was
    /// found, so the legacy table is only consulted for faces without one.
    fn load_gpos_kerning(&mut self) -> bool {
        let data = self.data;
        let Some((gpos, _)) = table(data, b"GPOS") else {
            return false;
        };
        let Some(lookups) = kern_lookups(data, gpos) else {
            return false;
        };
        if lookups.is_empty() {
            return false;
        }
        let Some(lookup_list) = read_u16(data, gpos + 8).map(|offset| gpos + offset as usize)
        else {
            return false;
        };
        for lookup in lookups {
            let Some(lookup_offset) = read_u16(data, lookup_list + 2 + lookup as usize * 2) else {
                continue;
            };
            let lookup_table = lookup_list + lookup_offset as usize;
            let (Some(kind), Some(count)) = (
                read_u16(data, lookup_table),
                read_u16(data, lookup_table + 4),
            ) else {
                continue;
            };
            for index in 0..count as usize {
                let Some(subtable_offset) = read_u16(data, lookup_table + 6 + index * 2) else {
                    continue;
                };
                let mut subtable = lookup_table + subtable_offset as usize;
                let mut subtable_kind = kind;
                if kind == 9 {
                    // Extension: the real lookup type and a 32-bit offset.
                    let (Some(real), Some(extension)) =
                        (read_u16(data, subtable + 2), read_u32(data, subtable + 4))
                    else {
                        continue;
                    };
                    subtable_kind = real;
                    subtable += extension as usize;
                }
                if subtable_kind == 2 {
                    self.load_pair_subtable(subtable);
                }
            }
        }
        true
    }

    fn load_pair_subtable(&mut self, subtable: usize) -> Option<()> {
        let data = self.data;
        let format = read_u16(data, subtable)?;
        let coverage = parse_coverage(data, subtable + read_u16(data, subtable + 2)? as usize)?;
        let value_format1 = read_u16(data, subtable + 4)?;
        let value_format2 = read_u16(data, subtable + 6)?;
        let size1 = value_record_size(value_format1);
        let size2 = value_record_size(value_format2);
        match format {
            1 => {
                let set_count = read_u16(data, subtable + 8)? as usize;
                let firsts = coverage_glyphs(&coverage);
                for (set, first) in firsts.into_iter().enumerate().take(set_count) {
                    let set_offset = subtable + read_u16(data, subtable + 10 + set * 2)? as usize;
                    let pairs = read_u16(data, set_offset)? as usize;
                    for pair in 0..pairs {
                        let record = set_offset + 2 + pair * (2 + size1 + size2);
                        let second = read_u16(data, record)?;
                        let value = x_advance(data, record + 2, value_format1)?;
                        // The first subtable to define a pair wins, an
                        // explicit zero included: it is an exception to a
                        // class rule further down.
                        self.kerning.entry((first, second)).or_insert(value);
                    }
                }
            }
            2 => {
                let first_classes =
                    parse_class_def(data, subtable + read_u16(data, subtable + 8)? as usize)?;
                let second_classes =
                    parse_class_def(data, subtable + read_u16(data, subtable + 10)? as usize)?;
                let class1_count = read_u16(data, subtable + 12)? as usize;
                let class2_count = read_u16(data, subtable + 14)?;
                let mut adjustments = Vec::with_capacity(class1_count * class2_count as usize);
                let mut record = subtable + 16;
                for _ in 0..class1_count * class2_count as usize {
                    adjustments.push(x_advance(data, record, value_format1)?);
                    record += size1 + size2;
                }
                self.class_kerning.push(ClassPairs {
                    coverage,
                    first_classes,
                    second_classes,
                    class2_count,
                    adjustments,
                });
            }
            _ => {}
        }
        Some(())
    }

    fn load_kern_table(&mut self) -> Option<()> {
        let data = self.data;
        let (kern, _) = table(data, b"kern")?;
        let count = read_u16(data, kern + 2)? as usize;
        let mut subtable = kern + 4;
        for _ in 0..count {
            let length = read_u16(data, subtable + 2)? as usize;
            let coverage = read_u16(data, subtable + 4)?;
            // Format 0, horizontal, not cross-stream.
            if coverage >> 8 == 0 && coverage & 0x1 != 0 && coverage & 0x4 == 0 {
                let pairs = read_u16(data, subtable + 6)? as usize;
                for pair in 0..pairs {
                    let record = subtable + 14 + pair * 6;
                    let left = read_u16(data, record)?;
                    let right = read_u16(data, record + 2)?;
                    let value = read_i16(data, record + 4)?;
                    self.kerning.entry((left, right)).or_insert(value);
                }
            }
            subtable += length.max(6);
        }
        Some(())
    }
}

fn apply(transform: [f64; 6], x: f64, y: f64) -> (f64, f64) {
    (
        transform[0] * x + transform[2] * y + transform[4],
        transform[1] * x + transform[3] * y + transform[5],
    )
}

/// `outer ∘ inner`: the component is first placed by `inner`, then the whole
/// composite by `outer`.
fn compose(outer: [f64; 6], inner: [f64; 6]) -> [f64; 6] {
    let (tx, ty) = apply(outer, inner[4], inner[5]);
    [
        outer[0] * inner[0] + outer[2] * inner[1],
        outer[1] * inner[0] + outer[3] * inner[1],
        outer[0] * inner[2] + outer[2] * inner[3],
        outer[1] * inner[2] + outer[3] * inner[3],
        tx,
        ty,
    ]
}

fn value_record_size(format: u16) -> usize {
    (format & 0xFF).count_ones() as usize * 2
}

fn x_advance(data: &[u8], record: usize, format: u16) -> Option<i16> {
    if format & 0x0004 == 0 {
        return Some(0);
    }
    let before = (format & 0x0003).count_ones() as usize * 2;
    read_i16(data, record + before)
}

/// Coverage as sorted `(start, end, first_coverage_index)` ranges.
fn parse_coverage(data: &[u8], offset: usize) -> Option<Vec<(u16, u16, u16)>> {
    let format = read_u16(data, offset)?;
    let count = read_u16(data, offset + 2)? as usize;
    let mut ranges = Vec::with_capacity(count);
    match format {
        1 => {
            for index in 0..count {
                let glyph = read_u16(data, offset + 4 + index * 2)?;
                ranges.push((glyph, glyph, index as u16));
            }
        }
        2 => {
            for index in 0..count {
                let record = offset + 4 + index * 6;
                ranges.push((
                    read_u16(data, record)?,
                    read_u16(data, record + 2)?,
                    read_u16(data, record + 4)?,
                ));
            }
        }
        _ => return None,
    }
    Some(ranges)
}

fn coverage_glyphs(coverage: &[(u16, u16, u16)]) -> Vec<u16> {
    let mut glyphs: Vec<(u16, u16)> = Vec::new();
    for (start, end, first) in coverage {
        for glyph in *start..=*end {
            glyphs.push((first + (glyph - start), glyph));
        }
    }
    glyphs.sort_unstable();
    glyphs.into_iter().map(|(_, glyph)| glyph).collect()
}

/// Class definitions as `(start, end, class)` ranges.
fn parse_class_def(data: &[u8], offset: usize) -> Option<Vec<(u16, u16, u16)>> {
    let format = read_u16(data, offset)?;
    let mut ranges = Vec::new();
    match format {
        1 => {
            let start = read_u16(data, offset + 2)?;
            let count = read_u16(data, offset + 4)? as usize;
            for index in 0..count {
                let class = read_u16(data, offset + 6 + index * 2)?;
                let glyph = start.checked_add(index as u16)?;
                ranges.push((glyph, glyph, class));
            }
        }
        2 => {
            let count = read_u16(data, offset + 2)? as usize;
            for index in 0..count {
                let record = offset + 4 + index * 6;
                ranges.push((
                    read_u16(data, record)?,
                    read_u16(data, record + 2)?,
                    read_u16(data, record + 4)?,
                ));
            }
        }
        _ => return None,
    }
    ranges.sort_unstable();
    Some(ranges)
}

/// Value attached to the range containing `glyph`. For coverage tables this
/// is the coverage index; for class definitions, the class.
fn lookup_range(ranges: &[(u16, u16, u16)], glyph: u16) -> Option<u16> {
    let index = ranges.partition_point(|(_, end, _)| *end < glyph);
    let (start, _, value) = ranges.get(index)?;
    (*start <= glyph).then_some(*value)
}

/// Lookup indices of the `kern` feature for the Latin script (or the default
/// script, or any script, in that order of preference).
fn kern_lookups(data: &[u8], gpos: usize) -> Option<Vec<u16>> {
    let script_list = gpos + read_u16(data, gpos + 4)? as usize;
    let feature_list = gpos + read_u16(data, gpos + 6)? as usize;
    let script_count = read_u16(data, script_list)? as usize;
    let mut chosen = None;
    for preferred in [b"latn", b"DFLT"] {
        for index in 0..script_count {
            let record = script_list + 2 + index * 6;
            if data.get(record..record + 4)? == preferred {
                chosen = Some(script_list + read_u16(data, record + 4)? as usize);
                break;
            }
        }
        if chosen.is_some() {
            break;
        }
    }
    let feature_count = read_u16(data, feature_list)? as usize;
    let mut feature_indices = Vec::new();
    match chosen {
        Some(script) => {
            let default_language = read_u16(data, script)? as usize;
            if default_language != 0 {
                let language = script + default_language;
                let count = read_u16(data, language + 4)? as usize;
                for index in 0..count {
                    feature_indices.push(read_u16(data, language + 6 + index * 2)? as usize);
                }
            }
        }
        None => feature_indices.extend(0..feature_count),
    }
    let mut lookups = Vec::new();
    for feature in feature_indices {
        if feature >= feature_count {
            continue;
        }
        let record = feature_list + 2 + feature * 6;
        if data.get(record..record + 4)? != b"kern" {
            continue;
        }
        let feature_table = feature_list + read_u16(data, record + 4)? as usize;
        let count = read_u16(data, feature_table + 2)? as usize;
        for index in 0..count {
            let lookup = read_u16(data, feature_table + 4 + index * 2)?;
            if !lookups.contains(&lookup) {
                lookups.push(lookup);
            }
        }
    }
    Some(lookups)
}
