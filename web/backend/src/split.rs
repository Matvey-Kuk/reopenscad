//! The `split_object` MCP tool: a split of one object by a plane into two
//! printable halves with a glue gap and registration keys, returned as SCAD
//! the agent pastes. Geometry lives in [`crate::meshops`]; this is argument
//! handling, sourcing the mesh from an asset or a compiled design object, and
//! the SCAD snippet with its validation compile.

use crate::agent;
use crate::assets::TriMesh;
use crate::meshanalysis::{self, MeshIndex};
use crate::meshops::{self, KeyPlan, KeyRequest, Mat4, PlaneFrame};
use crate::{assetstore, AppState};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// Wall-clock budget of one split_object call (the same as the analysis tools).
const BUDGET: Duration = Duration::from_secs(90);

pub(crate) fn tool_definitions() -> Vec<Value> {
    let vec3 = json!({"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3});
    vec![
        crate::mcp_tool(
            "split_object",
            "Split one object (objectId) or asset (from) by a plane (axis+offset, or point+normal) into \
             two halves for printing, with a glue gap and registration keys; half A is the side the \
             normal points to. Writes nothing: returns SCAD to paste, compiled and checked to yield two \
             closed solids. Keys sit on the actual cut section, each socket keeping minWall from cut \
             edges and of material beyond its depth. style \"dowel\": sockets in both halves plus \
             separate pins, so both halves print flat on the cut face; \"pin\": pins fused to half B (cut \
             face up). Reply: scad, usage.print / usage.assembled, and apply (an update_design_code edit \
             swapping the object for scad + usage.print).",
            json!({
                "type": "object",
                "required": ["designId", "plane"],
                "properties": {
                    "designId": {"type": "string"},
                    "objectId": {"type": "string", "description": "The object to split (default: the only object)."},
                    "from": {"type": "string", "description": "An asset name instead of a design object."},
                    "plane": {"type": "object", "properties": {
                        "axis": {"type": "string", "enum": ["x", "y", "z"]},
                        "offset": {"type": "number"},
                        "point": vec3.clone(),
                        "normal": vec3
                    }},
                    "gap": {"type": "number", "minimum": 0, "maximum": 5, "default": 0.1, "description": "Glue gap between the cut faces, mm."},
                    "keys": {"type": "object", "properties": {
                        "count": {"type": "integer", "minimum": 0, "maximum": 8, "default": 2},
                        "diameter": {"type": "number", "minimum": 0.5, "maximum": 50},
                        "length": {"type": "number", "minimum": 1, "maximum": 100, "description": "Dowel: total, spanning the gap ((length - gap) / 2 in each half). Pin: rises gap + length / 2 from half B. Each half must hold at least max(1, diameter / 2) mm."},
                        "clearance": {"type": "number", "minimum": 0, "maximum": 2, "description": "Socket clearance per side; default the design tolerance."},
                        "minWall": {"type": "number", "minimum": 0.2, "maximum": 20, "default": 1.2},
                        "style": {"type": "string", "enum": ["dowel", "pin"], "default": "dowel"}
                    }},
                    "prefix": {"type": "string", "pattern": "^[A-Za-z_][A-Za-z0-9_]{0,31}$", "description": "Default split_<object>."},
                    "spacing": {"type": "number", "minimum": 0, "maximum": 100, "default": 5, "description": "Between the laid-out print parts, mm."}
                }
            }),
        ),
    ]
}

fn ok_result(structured: Value) -> Value {
    json!({
        "content": [{"type": "text", "text": serde_json::to_string(&structured).unwrap_or_default()}],
        "structuredContent": structured,
        "isError": false
    })
}

pub(crate) fn mcp_dispatch(state: &AppState, name: &str, arguments: &Value) -> Value {
    let result = match name {
        "split_object" => split_object_value(state, arguments),
        _ => Err(format!("Unknown tool {name:?}.")),
    };
    match result {
        Ok(value) => ok_result(value),
        Err(error) => crate::mcp_tool_error(&error),
    }
}

fn number(input: &Value, key: &str) -> Result<Option<f64>, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_f64()
            .filter(|value| value.is_finite())
            .map(Some)
            .ok_or_else(|| format!("{key} must be a finite number.")),
    }
}

fn vec3(value: &Value, what: &str) -> Result<[f64; 3], String> {
    let items = value.as_array().filter(|items| items.len() == 3).ok_or_else(|| format!("{what} must be [x, y, z]."))?;
    let mut out = [0.0; 3];
    for (slot, item) in out.iter_mut().zip(items) {
        *slot = item.as_f64().filter(|v| v.is_finite()).ok_or_else(|| format!("{what} must hold finite numbers."))?;
    }
    Ok(out)
}

fn rounded(value: f64, decimals: i32) -> f64 {
    let factor = 10f64.powi(decimals);
    (value * factor).round() / factor
}

fn round3(v: [f64; 3]) -> [f64; 3] {
    v.map(|value| rounded(value, 4))
}

/// The selected parts' meshes, by objectIds (default all), with the
/// evaluated source.
fn object_meshes(
    state: &AppState,
    id: Option<&str>,
    input: &Value,
    ids: Option<Vec<String>>,
) -> Result<(agent::EvalSource, std::sync::Arc<crate::engine::MultipartCompileOutput>, Vec<usize>), String> {
    let source = agent::eval_source(state, id, input).map_err(|error| error.message)?;
    let compiled = agent::compile_eval(&source).map_err(|error| error.message)?;
    let available = || compiled.parts.iter().map(|part| part.id.as_str()).collect::<Vec<_>>().join(", ");
    let selected: Vec<usize> = match ids {
        None => (0..compiled.parts.len()).collect(),
        Some(ids) => ids
            .iter()
            .map(|wanted| {
                compiled
                    .parts
                    .iter()
                    .position(|part| &part.id == wanted || &part.name == wanted)
                    .ok_or_else(|| format!("Unknown object {wanted:?}. Objects are: {}.", available()))
            })
            .collect::<Result<_, _>>()?,
    };
    if selected.is_empty() {
        return Err("The design has no solid objects.".into());
    }
    Ok((source, compiled, selected))
}

fn source_asset(name: &str) -> Result<TriMesh, String> {
    match crate::assets::try_lookup(name)? {
        Some(asset) => Ok((*asset.mesh).clone()),
        None => Err(crate::assets::no_asset_message(name)),
    }
}

// ---------------------------------------------------------------------------
// split_object
// ---------------------------------------------------------------------------

fn plane_input(input: &Value) -> Result<([f64; 3], [f64; 3]), String> {
    let plane = input
        .get("plane")
        .filter(|plane| plane.is_object())
        .ok_or("plane is required: {\"axis\": \"x\"|\"y\"|\"z\", \"offset\": mm} or {\"point\": [x,y,z], \"normal\": [x,y,z]}.")?;
    if let Some(axis) = plane.get("axis").filter(|axis| !axis.is_null()) {
        let index = match axis.as_str() {
            Some("x" | "X") => 0,
            Some("y" | "Y") => 1,
            Some("z" | "Z") => 2,
            _ => return Err("plane.axis must be \"x\", \"y\" or \"z\".".into()),
        };
        let offset = number(plane, "offset")?.unwrap_or(0.0);
        let mut point = [0.0; 3];
        let mut normal = [0.0; 3];
        point[index] = offset;
        normal[index] = 1.0;
        return Ok((point, normal));
    }
    let point = vec3(plane.get("point").unwrap_or(&Value::Null), "plane.point")?;
    let normal = vec3(plane.get("normal").unwrap_or(&Value::Null), "plane.normal")?;
    let length = (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]).sqrt();
    if !(length > 1e-12) {
        return Err("plane.normal must be non-zero.".into());
    }
    Ok((point, normal.map(|v| v / length)))
}

/// A number as SCAD source: at most 5 decimals, no "-0".
fn num(value: f64) -> String {
    let text = format!("{:.5}", value);
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text == "-0" || text.is_empty() {
        "0".into()
    } else {
        text.to_string()
    }
}

fn scad_matrix(m: &Mat4) -> String {
    format!("[{}]", m.iter().map(|row| format!("[{}]", row.iter().map(|v| num(*v)).collect::<Vec<_>>().join(", "))).collect::<Vec<_>>().join(", "))
}

/// An object name as one safe line for SCAD comments and `@name`
/// annotations: control characters (newlines above all, which would end
/// the comment and let the rest run as code) become spaces, runs of
/// whitespace collapse, and it keeps at most 60 characters.
fn safe_label(text: &str) -> String {
    let cleaned: String = text.chars().map(|c| if c.is_control() || c == '\u{2028}' || c == '\u{2029}' { ' ' } else { c }).collect();
    let label: String = cleaned.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(60).collect();
    let label = label.trim().to_string();
    if label.is_empty() { "part".into() } else { label }
}

fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let out = out.trim_matches('_').chars().take(24).collect::<String>();
    if out.is_empty() { "part".into() } else { out }
}

/// Bounds, in plane coordinates, of the part of `mesh` on `side` of the cut
/// face at `side * gap / 2`: vertices on that side plus every edge crossing.
fn half_bounds(mesh: &TriMesh, frame: &PlaneFrame, side: f64, gap: f64) -> Option<([f64; 3], [f64; 3])> {
    let level = side * gap / 2.0;
    let local: Vec<[f64; 3]> = mesh.points.iter().map(|p| frame.to_plane(*p)).collect();
    let on_side = |p: &[f64; 3]| side * (p[2] - level) >= 0.0;
    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    let mut take = |p: [f64; 3]| {
        for axis in 0..3 {
            lo[axis] = lo[axis].min(p[axis]);
            hi[axis] = hi[axis].max(p[axis]);
        }
    };
    for triangle in &mesh.triangles {
        for k in 0..3 {
            let (a, b) = (local[triangle[k] as usize], local[triangle[(k + 1) % 3] as usize]);
            if on_side(&a) {
                take(a);
            }
            if on_side(&a) != on_side(&b) {
                let t = (level - a[2]) / (b[2] - a[2]);
                take([a[0] + t * (b[0] - a[0]), a[1] + t * (b[1] - a[1]), level]);
            }
        }
    }
    (lo[0] <= hi[0]).then_some((lo, hi))
}

/// Where the statement making the object starts and ends in the original
/// source, widened to a `@name` annotation line directly above it.
fn statement_span(base: &str, start: usize, end: usize) -> (usize, usize) {
    let line_start = base[..start].rfind('\n').map_or(0, |i| i + 1);
    let mut span_start = start;
    if base[line_start..start].trim().is_empty() && line_start > 0 {
        let previous_start = base[..line_start - 1].rfind('\n').map_or(0, |i| i + 1);
        let previous = &base[previous_start..line_start - 1];
        if previous.contains("@name") && (previous.trim_start().starts_with("//") || previous.trim_start().starts_with("/*")) {
            span_start = previous_start;
        }
    }
    (span_start, end)
}

struct Snippet {
    modules: String,
    print: String,
    assembled: String,
    names: [String; 3],
}

#[allow(clippy::too_many_arguments)]
fn build_snippet(
    prefix: &str,
    label: &str,
    statement: &str,
    frame: &PlaneFrame,
    plane: ([f64; 3], [f64; 3]),
    gap: f64,
    block: f64,
    keys: &KeyPlan,
    clearance: f64,
    pins: bool,
    prints: [Mat4; 2],
    dowel_row: ([f64; 2], f64),
) -> Snippet {
    let has_keys = !keys.positions.is_empty();
    let p = prefix;
    let mut s = String::new();
    s.push_str(&format!(
        "// --- split_object: \"{label}\" cut by the plane through [{}] with normal [{}].\n",
        plane.0.map(num).join(", "),
        plane.1.map(num).join(", ")
    ));
    s.push_str(&format!("// Half A is on the normal side. Glue gap {} mm between the cut faces.\n", num(gap)));
    if has_keys {
        s.push_str(&format!(
            "// {} {} Ø{} x {} mm in Ø{} sockets {} mm deep (clearance {} mm per side).\n",
            keys.positions.len(),
            if pins { "pins on half B" } else { "loose dowels" },
            num(keys.diameter),
            num(keys.length),
            num(keys.socket_diameter),
            num(keys.socket_depth),
            num(clearance)
        ));
    }
    s.push_str(&format!("{p}_frame = {}; // plane coordinates -> model\n", scad_matrix(&frame.matrix())));
    s.push_str(&format!("{p}_gap = {};\n{p}_block = {};\n", num(gap), num(block)));
    s.push_str(&format!("{p}_print = [{}, {}]; // each half laid out for printing\n", scad_matrix(&prints[0]), scad_matrix(&prints[1])));
    if has_keys {
        s.push_str(&format!(
            "{p}_keys = [{}];\n{p}_key_d = {};\n{p}_key_l = {};\n{p}_socket_d = {};\n{p}_socket_depth = {};\n",
            keys.positions.iter().map(|k| format!("[{}, {}]", num(k[0]), num(k[1]))).collect::<Vec<_>>().join(", "),
            num(keys.diameter),
            num(keys.length),
            num(keys.socket_diameter),
            num(keys.socket_depth),
        ));
    }
    let cut = format!(
        "intersection() {{\n            children();\n            multmatrix({p}_frame) translate([-{p}_block / 2, -{p}_block / 2, side > 0 ? {p}_gap / 2 : -{p}_block - {p}_gap / 2]) cube({p}_block);\n        }}"
    );
    let sockets = |condition: &str| {
        format!(
            "{condition}multmatrix({p}_frame) for (k = {p}_keys) translate([k[0], k[1], side > 0 ? {p}_gap / 2 - 0.01 : -{p}_gap / 2 - {p}_socket_depth]) cylinder(d = {p}_socket_d, h = {p}_socket_depth + 0.01, $fn = 32);"
        )
    };
    s.push_str("// side = 1: half A, -1: half B, in place.\n");
    match (has_keys, pins) {
        (false, _) => s.push_str(&format!("module {p}_half(side) {cut}\n").replace("\n        ", "\n    ")),
        (true, false) => s.push_str(&format!(
            "module {p}_half(side) {{\n    difference() {{\n        {cut}\n        {}\n    }}\n}}\n",
            sockets("")
        )),
        (true, true) => s.push_str(&format!(
            "module {p}_half(side) {{\n    difference() {{\n        {cut}\n        {}\n    }}\n    if (side < 0) multmatrix({p}_frame) for (k = {p}_keys) translate([k[0], k[1], -{p}_gap / 2 - 0.01]) cylinder(d = {p}_key_d, h = {p}_gap + {p}_key_l / 2 + 0.01, $fn = 32);\n}}\n",
            sockets("if (side > 0) ")
        )),
    }
    s.push_str(&format!("module {p}_print_half(side) multmatrix({p}_print[side > 0 ? 0 : 1]) {p}_half(side) children();\n"));
    if has_keys && !pins {
        let ([x0, y], step) = dowel_row;
        s.push_str(&format!(
            "module {p}_dowels() for (i = [0 : len({p}_keys) - 1]) translate([{} + i * {}, {}, 0]) cylinder(d = {p}_key_d, h = {p}_key_l, $fn = 32);\n",
            num(x0),
            num(step),
            num(y)
        ));
        s.push_str(&format!(
            "module {p}_dowels_in_place() multmatrix({p}_frame) for (k = {p}_keys) translate([k[0], k[1], -{p}_key_l / 2]) cylinder(d = {p}_key_d, h = {p}_key_l, $fn = 32);\n"
        ));
    }
    s.push_str("// --- end split_object\n");
    let names = [format!("{label} A"), format!("{label} B"), format!("{label} dowels")];
    let statement = statement.trim();
    let usage = |half: &str, dowels: &str| {
        let mut text = format!(
            "// @name: {}\n{p}_{half}(1) {statement}\n// @name: {}\n{p}_{half}(-1) {statement}\n",
            names[0], names[1]
        );
        if has_keys && !pins {
            text.push_str(&format!("// @name: {}\n{p}_{dowels}();\n", names[2]));
        }
        text
    };
    Snippet { modules: s, print: usage("print_half", "dowels"), assembled: usage("half", "dowels_in_place"), names }
}

/// `split_object`.
pub(crate) fn split_object_value(state: &AppState, input: &Value) -> Result<Value, String> {
    let started = Instant::now();
    let deadline = started + BUDGET;
    let _scope = crate::csg::bsp::deadline_scope(deadline);
    let (plane_point, plane_normal) = plane_input(input)?;
    let gap = number(input, "gap")?.unwrap_or(0.1);
    if !(0.0..=5.0).contains(&gap) {
        return Err("gap must be from 0 to 5 mm.".into());
    }
    let spacing = number(input, "spacing")?.unwrap_or(5.0).clamp(0.0, 100.0);
    let keys_input = input.get("keys").cloned().unwrap_or(Value::Null);
    if !(keys_input.is_null() || keys_input.is_object() || keys_input == json!(false)) {
        return Err("keys must be an object ({count, diameter, length, clearance, minWall, style}) or false.".into());
    }
    let count = if keys_input == json!(false) {
        0
    } else {
        match keys_input.get("count") {
            None | Some(Value::Null) => 2,
            Some(value) => value.as_u64().filter(|c| *c <= 8).ok_or("keys.count must be an integer from 0 to 8.")? as usize,
        }
    };
    let pins = match keys_input.get("style").and_then(Value::as_str) {
        None | Some("dowel") => false,
        Some("pin") => true,
        Some(other) => return Err(format!("keys.style {other:?} is not \"dowel\" or \"pin\".")),
    };
    let diameter = number(&keys_input, "diameter")?;
    let length = number(&keys_input, "length")?;
    if diameter.is_some_and(|d| !(0.5..=50.0).contains(&d)) || length.is_some_and(|l| !(1.0..=100.0).contains(&l)) {
        return Err("keys.diameter must be 0.5-50 mm and keys.length 1-100 mm.".into());
    }
    let min_wall = number(&keys_input, "minWall")?.unwrap_or(1.2);
    if !(0.2..=20.0).contains(&min_wall) {
        return Err("keys.minWall must be 0.2-20 mm.".into());
    }
    let prefix_input = match input.get("prefix") {
        None | Some(Value::Null) => None,
        Some(Value::String(prefix))
            if !prefix.is_empty()
                && prefix.len() <= 32
                && prefix.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
        {
            Some(prefix.clone())
        }
        Some(_) => return Err("prefix must be an identifier: a letter or _, then letters, digits or _ (at most 32).".into()),
    };

    // The object: a compiled design object, or an asset imported on its own.
    let from = input.get("from").and_then(Value::as_str);
    let design_id = crate::input_design_id(input);
    struct Target {
        mesh: TriMesh,
        label: String,
        identity: Value,
        statement: String,
        /// (start, end) of the statement span in the base source; None for an asset.
        span: Option<(usize, usize)>,
        base: String,
        tolerance: Option<f64>,
    }
    let target = if let Some(asset) = from {
        if input.get("objectId").is_some_and(|v| !v.is_null()) {
            return Err("Give objectId or from, not both.".into());
        }
        if !assetstore::valid_asset_name(asset) {
            return Err(format!("from {asset:?} is not an asset name (name.stl)."));
        }
        let tolerance = match design_id {
            Some(id) => state.workspaces.get(id).map_err(|e| e)?.map(|w| w.tolerance),
            None => None,
        };
        Target {
            mesh: source_asset(asset)?,
            label: safe_label(asset.trim_end_matches(".stl").trim_end_matches(".STL")),
            identity: json!({"asset": asset}),
            statement: format!("import(\"{asset}\");"),
            span: None,
            base: String::new(),
            tolerance,
        }
    } else {
        let wanted = input.get("objectId").and_then(Value::as_str).map(|id| vec![id.to_string()]);
        let (source, compiled, selected) = object_meshes(state, None, input, wanted.clone())?;
        if wanted.is_none() && selected.len() > 1 {
            return Err(format!(
                "The design has {} objects; pass objectId (one of {}).",
                selected.len(),
                compiled.parts.iter().map(|p| p.id.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        let part = &compiled.parts[selected[0]];
        let range = part.source_range.ok_or("This object has no source statement to wrap; give it its own top-level statement.")?;
        let siblings = compiled.parts.iter().filter(|other| other.source_range == part.source_range).count();
        if siblings > 1 {
            return Err(format!(
                "Object {} comes from a statement that makes {siblings} objects (a loop or block); give it its own top-level \
                 statement.",
                part.id
            ));
        }
        let start = source.overridden.original_offset(range.start).min(source.base.len());
        let end = source.overridden.original_offset(range.end).clamp(start, source.base.len());
        let span = statement_span(&source.base, start, end);
        let lines = [crate::scadsource::line_of(&source.base, start), crate::scadsource::line_of(&source.base, end.saturating_sub(1).max(start))];
        Target {
            mesh: meshanalysis::from_engine_mesh(&part.mesh),
            label: safe_label(&part.name),
            identity: json!({"id": part.id, "name": part.name, "lines": lines, "revision": source.revision}),
            statement: source.base[start..end].to_string(),
            span: Some(span),
            base: source.base.clone(),
            tolerance: Some(source.workspace.as_ref().map_or(0.2, |w| w.tolerance)),
        }
    };
    let clearance = match number(&keys_input, "clearance")? {
        Some(clearance) if (0.0..=2.0).contains(&clearance) => clearance,
        Some(_) => return Err("keys.clearance must be 0-2 mm.".into()),
        // The design tolerance, kept to a sane socket play.
        None => target.tolerance.unwrap_or(0.2).clamp(0.0, 2.0),
    };

    // The cut.
    let mut work = meshanalysis::Work::new(meshanalysis::ANALYSIS_WORK);
    let section = meshanalysis::section_within(&target.mesh, plane_point, plane_normal, 0.02, &mut work)?;
    let (frame, loops) = meshops::section_frame(&section, plane_normal);
    if loops.is_empty() || section.area <= 0.0 {
        let along: Vec<f64> = target.mesh.points.iter().map(|p| (0..3).map(|a| (p[a] - plane_point[a]) * plane_normal[a]).sum()).collect();
        let (lo, hi) = along.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
        return Err(format!(
            "The plane does not cut {} into two closed sections: along the normal the object spans {:.3} to {:.3} mm from plane.point.",
            target.label, lo, hi
        ));
    }
    let index = MeshIndex::new(&target.mesh);
    let request = KeyRequest { count, diameter, length, clearance, min_wall, gap, pins };
    let keys = meshops::plan_keys(&loops, &frame, &index, request, Some(deadline))?;

    // Print layout: each half on its cut face (pin style: half B cut face up).
    let sizes = meshops::bounds(&target.mesh).ok_or("The object is empty.")?;
    let diagonal = (0..3).map(|a| (sizes.1[a] - sizes.0[a]).powi(2)).sum::<f64>().sqrt();
    let centre: [f64; 3] = [0, 1, 2].map(|a| (sizes.0[a] + sizes.1[a]) / 2.0);
    let offset_from_origin = (0..3).map(|a| (centre[a] - frame.origin[a]).powi(2)).sum::<f64>().sqrt();
    let block = ((2.0 * (diagonal + offset_from_origin) + 10.0) / 10.0).ceil() * 10.0;
    let a_bounds = half_bounds(&target.mesh, &frame, 1.0, gap).ok_or("Half A is empty.")?;
    let b_bounds = half_bounds(&target.mesh, &frame, -1.0, gap).ok_or("Half B is empty.")?;
    let shift_down = meshops::translation([0.0, 0.0, -gap / 2.0]);
    let a_base = meshops::mat_mul(&shift_down, &frame.inverse());
    let flip: Mat4 = [[1.0, 0.0, 0.0, 0.0], [0.0, -1.0, 0.0, 0.0], [0.0, 0.0, -1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];
    let b_base = if pins { frame.inverse() } else { meshops::mat_mul(&shift_down, &meshops::mat_mul(&flip, &frame.inverse())) };
    // Bounds after the base maps (plane coordinates in, axis-aligned maps).
    let a_box = ([a_bounds.0[0], a_bounds.0[1], a_bounds.0[2] - gap / 2.0], [a_bounds.1[0], a_bounds.1[1], a_bounds.1[2] - gap / 2.0]);
    let b_box = if pins {
        let top = (gap / 2.0 + keys.length / 2.0).max(b_bounds.1[2]);
        (b_bounds.0, [b_bounds.1[0], b_bounds.1[1], if keys.positions.is_empty() { b_bounds.1[2] } else { top }])
    } else {
        ([b_bounds.0[0], -b_bounds.1[1], -b_bounds.1[2] - gap / 2.0], [b_bounds.1[0], -b_bounds.0[1], -b_bounds.0[2] - gap / 2.0])
    };
    let a_move = [-spacing / 2.0 - a_box.1[0], -(a_box.0[1] + a_box.1[1]) / 2.0, -a_box.0[2]];
    let b_move = [spacing / 2.0 - b_box.0[0], -(b_box.0[1] + b_box.1[1]) / 2.0, -b_box.0[2]];
    let prints = [
        meshops::mat_mul(&meshops::translation(a_move), &a_base),
        meshops::mat_mul(&meshops::translation(b_move), &b_base),
    ];
    let low_y = (a_box.0[1] + a_move[1]).min(b_box.0[1] + b_move[1]);
    let step = keys.diameter + 3.0;
    let dowel_row = ([-(keys.positions.len().saturating_sub(1) as f64) * step / 2.0, low_y - spacing - keys.diameter / 2.0], step);

    // Names already in the code the snippet joins (for an asset, the stored
    // design's code): a second split must not reuse the first one's
    // variables, since the last top-level assignment wins.
    let existing = if target.span.is_some() {
        target.base.clone()
    } else {
        design_id.and_then(|id| state.workspaces.get(id).ok().flatten()).map(|w| w.code).unwrap_or_default()
    };
    let taken = |prefix: &str| existing.contains(&format!("{prefix}_half")) || existing.contains(&format!("{prefix}_frame"));
    let prefix = match prefix_input {
        Some(prefix) if taken(&prefix) => {
            return Err(format!("prefix {prefix:?} is already used by the design's code; pick another."));
        }
        Some(prefix) => prefix,
        None => {
            let base = format!("split_{}", slug(&target.label));
            let mut candidate = base.clone();
            let mut n = 2;
            while taken(&candidate) {
                candidate = format!("{base}{n}");
                n += 1;
            }
            candidate
        }
    };
    let snippet = build_snippet(
        &prefix,
        &target.label,
        &target.statement,
        &frame,
        (plane_point, plane_normal),
        gap,
        block,
        &keys,
        clearance,
        pins,
        prints,
        dowel_row,
    );

    // Validate: compile with the object statement swapped for the snippet
    // (or, for an asset, the snippet alone) and check the new objects.
    let (apply, validation_code) = match target.span {
        Some((start, end)) => {
            let search = target.base[start..end].to_string();
            let replace = format!("{}{}", snippet.modules, snippet.print.trim_end());
            let total = target.base.matches(&search).count();
            let occurrence = target.base[..start].matches(&search).count() + 1;
            let mut apply = json!({"search": search, "replace": replace});
            if total > 1 {
                apply["occurrence"] = json!(occurrence);
            }
            let code = format!("{}{}{}", &target.base[..start], replace, &target.base[end..]);
            (Some(apply), code)
        }
        None => (None, format!("{}{}", snippet.modules, snippet.print)),
    };
    let mut check_input = json!({"code": validation_code});
    if let Some(id) = design_id {
        check_input["designId"] = json!(id);
    }
    let source = agent::eval_source(state, None, &check_input).map_err(|e| format!("Validation could not start: {}", e.message))?;
    let compiled = agent::compile_eval(&source).map_err(|e| format!("The generated split did not compile: {}", e.message))?;
    let mut validated = Vec::new();
    let mut all_valid = true;
    let wanted = if keys.positions.is_empty() || pins { 2 } else { 3 };
    for name in snippet.names.iter().take(wanted) {
        match compiled.parts.iter().find(|part| &part.name == name) {
            Some(part) => {
                let summary = meshops::summary(&meshanalysis::from_engine_mesh(&part.mesh));
                all_valid &= summary["valid"] == json!(true);
                validated.push(json!({
                    "id": part.id,
                    "name": part.name,
                    "volume": summary["volume"],
                    "size": summary["size"],
                    "shells": summary["shells"],
                    "valid": summary["valid"],
                }));
            }
            None => {
                all_valid = false;
                validated.push(json!({"name": name, "missing": true}));
            }
        }
    }
    if !all_valid {
        return Err(format!(
            "The generated split compiled but did not give valid closed solids: {}. Try another plane or keys: false.",
            serde_json::to_string(&validated).unwrap_or_default()
        ));
    }
    let original_volume = meshops::signed_volume(&target.mesh);

    let mut value = json!({
        "ok": true,
        "object": target.identity,
        "plane": {"point": round3(plane_point), "normal": round3(plane_normal)},
        "gap": gap,
        "cut": {
            "area": rounded(section.area, 3),
            "loops": loops.len(),
            "maxInscribedRadius": keys.max_inscribed_radius,
        },
        "keys": {
            "style": if pins { "pin" } else { "dowel" },
            "count": keys.positions.len(),
            "diameter": keys.diameter,
            "length": keys.length,
            "socketDiameter": keys.socket_diameter,
            "socketDepth": keys.socket_depth,
            "clearance": clearance,
            "minWall": min_wall,
            "positions": keys.positions.iter().map(|k| round3(frame.to_model([k[0], k[1], 0.0]))).collect::<Vec<_>>(),
            "edgeDistance": keys.edge_distance,
        },
        "prefix": prefix,
        "scad": snippet.modules,
        "usage": {"print": snippet.print, "assembled": snippet.assembled},
        "validated": {"objects": validated, "originalVolume": rounded(original_volume, 3)},
        "elapsedMs": started.elapsed().as_millis() as u64,
    });
    if !keys.notes.is_empty() {
        value["keys"]["notes"] = json!(keys.notes);
    }
    match apply {
        Some(apply) => {
            value["apply"] = apply;
            value["next"] = json!(
                "Apply with update_design_code {designId, expectedRevision, edits: [apply]}; for an assembled preview use usage.assembled in place of usage.print."
            );
        }
        None => {
            value["next"] = json!("Append scad and usage.print (or usage.assembled) to the design with update_design_code.");
        }
    }
    if let Some(id) = design_id {
        value["designId"] = json!(id);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_slugs_are_scad_safe() {
        assert_eq!(num(-0.0000001), "0");
        assert_eq!(num(2.5), "2.5");
        assert_eq!(num(10.0), "10");
        assert_eq!(slug("Body 3"), "body_3");
        assert_eq!(slug("--"), "part");
    }

    #[test]
    fn statement_span_takes_a_name_annotation() {
        let source = "a = 1;\n// @name: Body\ncube(10);\n";
        let start = source.find("cube").unwrap();
        let (s, e) = statement_span(source, start, start + 9);
        assert_eq!(&source[s..e], "// @name: Body\ncube(10);");
        let plain = "a = 1;\ncube(10);\n";
        let start = plain.find("cube").unwrap();
        assert_eq!(statement_span(plain, start, start + 9), (start, start + 9));
    }
}

#[cfg(test)]
mod tool_tests {
    use crate::{
        JobLimiter, RateLimiter, RenderQueue, Request, Store, WorkspaceStore, MAX_CONCURRENT_JOBS, MAX_CONCURRENT_POLLS,
        MAX_QUEUED_JOBS, MAX_QUEUED_PER_CLIENT, MAX_QUEUE_WAIT,
    };
    use crate::AppState;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    fn state() -> (AppState, PathBuf) {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let root = std::env::temp_dir().join(format!("reopenscad-split-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let state = AppState {
            web_root: PathBuf::new(),
            render_queue: RenderQueue::new(MAX_CONCURRENT_JOBS, MAX_QUEUED_JOBS, MAX_QUEUED_PER_CLIENT, MAX_QUEUE_WAIT),
            poll_limiter: JobLimiter::new(MAX_CONCURRENT_POLLS),
            rate_limiter: RateLimiter::new(),
            cancellations: Arc::new(Mutex::new(HashMap::new())),
            workspaces: Store::Filesystem(WorkspaceStore::open(root.clone()).unwrap()),
            public_origin: None,
        };
        (state, root)
    }

    fn request() -> Request {
        Request { method: "POST".into(), path: "/mcp".into(), query: String::new(), headers: vec![("host".into(), "127.0.0.1:5173".into())], body: vec![] }
    }

    fn call(state: &AppState, name: &str, arguments: Value) -> Value {
        let result = crate::mcp_call_tool(state, &request(), name, arguments);
        result["structuredContent"].clone()
    }

    fn close(value: &Value, expected: f64, tolerance: f64) -> bool {
        value.as_f64().is_some_and(|v| (v - expected).abs() <= tolerance)
    }

    #[test]
    fn split_object_returns_validated_scad_that_applies_as_an_edit() {
        let (state, root) = state();
        let code = "module body() cube([40, 20, 30]);\n// @name: Body\nbody();\n// @name: Other\ntranslate([100, 0, 0]) cube(5);\n";
        let workspace = state.workspaces.create(code.into(), None).unwrap();
        let split = call(&state, "split_object", json!({"designId": workspace.id, "objectId": "object-body", "plane": {"axis": "z", "offset": 12}}));
        assert_eq!(split["ok"], json!(true), "{split}");
        assert_eq!(split["keys"]["count"], json!(2), "{split}");
        assert_eq!(split["keys"]["clearance"], json!(0.2));
        assert!(close(&split["cut"]["area"], 800.0, 1e-6), "{split}");
        let objects = split["validated"]["objects"].as_array().unwrap();
        assert_eq!(objects.len(), 3, "{split}");
        // Halves lie on their cut faces: A is 30 - 12 - gap/2 tall, B 12 - gap/2.
        assert!(close(&objects[0]["size"][2], 17.95, 1e-3), "{split}");
        assert!(close(&objects[1]["size"][2], 11.95, 1e-3), "{split}");
        for key in split["keys"]["positions"].as_array().unwrap() {
            assert!(close(&key[2], 12.0, 1e-9), "{key}");
        }
        let apply = split["apply"].clone();
        assert!(apply["search"].as_str().unwrap().starts_with("// @name: Body"), "{apply}");
        let updated = call(
            &state,
            "update_design_code",
            json!({
                "designId": workspace.id,
                "expectedRevision": workspace.revision,
                "edits": [apply],
                "revisionName": "Test revision",
            }));
        assert_eq!(updated["ok"], json!(true), "{updated}");
        let listed = call(&state, "list_design_objects", json!({"designId": workspace.id}));
        let ids: Vec<&str> = listed["objects"].as_array().unwrap().iter().map(|o| o["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["object-body-a", "object-body-b", "object-body-dowels", "object-other"], "{listed}");
        // Assembled, the halves keep the glue gap and the dowels fit their sockets.
        let assembled = format!("{}{}", split["scad"].as_str().unwrap(), split["usage"]["assembled"].as_str().unwrap()).replace("body();", "cube([40, 20, 30]);");
        let checked = call(&state, "check_intersections", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(&assembled), None), "tolerance": 0.05}));
        assert_eq!(checked["ok"], json!(true), "{checked}");
        assert_eq!(checked["summary"]["clear"], json!(3), "{checked}");
        assert!(close(&checked["summary"]["minClearance"], 0.1, 1e-6), "{checked}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn split_object_handles_assets_pins_and_misses() {
        let (state, root) = state();
        let id = state.workspaces.create("cylinder(r = 15, h = 40, $fn = 48);\n".into(), None).unwrap().id;
        let rod = crate::scancommon::testkit::compiled_mesh(&state, &id);
        crate::scancommon::testkit::upload_with(&state, &id, "rod.stl", &rod, json!({}));
        let split = call(
            &state,
            "split_object",
            json!({"designId": id, "from": "rod.stl", "plane": {"point": [0, 0, 20], "normal": [0, 0, -1]}, "keys": {"style": "pin", "count": 3}}),
        );
        assert_eq!(split["ok"], json!(true), "{split}");
        assert_eq!(split["keys"]["style"], json!("pin"));
        assert!(split["keys"]["count"].as_u64().unwrap() >= 2, "{split}");
        assert!(split.get("apply").is_none());
        assert!(split["usage"]["print"].as_str().unwrap().contains("import(\"rod.stl\")"));
        assert_eq!(split["validated"]["objects"].as_array().unwrap().len(), 2);

        let plain = call(&state, "split_object", json!({"designId": id, "plane": {"axis": "x", "offset": 0}, "keys": false}));
        assert_eq!(plain["ok"], json!(true), "{plain}");
        assert_eq!(plain["keys"]["count"], json!(0));
        assert!(!plain["scad"].as_str().unwrap().contains("_keys"));

        // A second split of the same asset, once the first is in the code,
        // gets fresh names.
        let current = state.workspaces.get(&id).unwrap().unwrap();
        let appended = format!("{}{}{}", current.code, split["scad"].as_str().unwrap(), split["usage"]["print"].as_str().unwrap());
        let written = call(&state, "update_design_code", json!({"designId": id, "expectedRevision": current.revision, "code": appended, "revisionName": "Test revision"}));
        assert_eq!(written["ok"], json!(true), "{written}");
        let again = call(&state, "split_object", json!({"designId": id, "from": "rod.stl", "plane": {"axis": "z", "offset": 10}}));
        assert_eq!(again["ok"], json!(true), "{again}");
        assert_ne!(again["prefix"], split["prefix"]);

        let missed = call(&state, "split_object", json!({"designId": id, "objectId": "object-1", "plane": {"axis": "z", "offset": 50}}));
        assert_eq!(missed["ok"], json!(false));
        assert!(missed["error"].as_str().unwrap().contains("does not cut"), "{missed}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn object_names_cannot_inject_code_into_the_snippet() {
        let (state, root) = state();
        let code = "cube([40, 20, 30], $name = \"Body\\nassert(false, \\\"X\\\"); //\");\n";
        let id = state.workspaces.create(code.into(), None).unwrap().id;
        let split = call(&state, "split_object", json!({"designId": id, "plane": {"axis": "z", "offset": 12}}));
        assert_eq!(split["ok"], json!(true), "{split}");
        for text in [split["scad"].as_str().unwrap(), split["usage"]["print"].as_str().unwrap()] {
            for line in text.lines() {
                assert!(!line.trim_start().starts_with("assert"), "{text}");
            }
        }
        assert!(split["usage"]["print"].as_str().unwrap().contains("// @name: Body assert(false"), "{split}");
        let _ = std::fs::remove_dir_all(root);
    }
}
