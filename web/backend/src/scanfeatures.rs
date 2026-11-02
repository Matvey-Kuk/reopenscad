//! `extract_features`: a scan's rims, wall transitions, boundaries and smooth
//! surfaces, described so each can be reused through a `within` box. It stores
//! nothing: the only mesh of a scan kept on the server is the scan itself.

use crate::assets::TriMesh;
use crate::meshops::{self, Mat4};
use crate::scancommon::{
    self as sc, add, dot, face_normal_area, norm, r3, round_to, scale, sub, unit, Design, SceneObject, P,
};
use crate::AppState;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const BUDGET: Duration = Duration::from_secs(60);

pub(crate) fn tool_definitions() -> Vec<Value> {
    let asset_name = json!({"type": "string", "pattern": "^[A-Za-z0-9_-][A-Za-z0-9._-]{0,59}\\.[sS][tT][lL]$"});
    let matrix = json!({"type": "array", "minItems": 3, "maxItems": 4, "items": {"type": "array", "items": {"type": "number"}, "minItems": 4, "maxItems": 4}});
    vec![crate::mcp_tool(
        "extract_features",
        "Find a scan's features, storing nothing: opening rims (a crease or fillet band ringing an \
         opening), wall transitions (other crease or fillet bands), the open boundary and the smooth \
         surfaces between them. A face is bent when its normal turns more than transitionAngleDeg from \
         its neighbourhood's within rings steps, or sits on an edge sharper than creaseAngleDeg. A \
         feature's within is its bounds with a small margin, ready to pass as another tool's within; \
         being a box it can also hold other faces of the scan. within {min, max} here finds features on \
         the whole scan, then keeps to that box: area, bounds, the suggested within, minArea and the \
         per-kind limit count only the part inside. Coordinates and pictures are in the \
         design frame when objectId names the object importing the scan, or with matrix.",
        json!({
            "type": "object",
            "required": ["designId", "from"],
            "properties": {
                "designId": {"type": "string"},
                "from": asset_name,
                "objectId": {"type": "string"},
                "matrix": matrix,
                "within": sc::within_schema("the same frame as the reply's features: design coordinates with objectId or matrix, else the scan's own"),
                "transitionAngleDeg": {"type": "number", "minimum": 1, "maximum": 90, "default": 15},
                "creaseAngleDeg": {"type": "number", "minimum": 1, "maximum": 180, "default": 40},
                "rings": {"type": "integer", "minimum": 1, "maximum": 8, "default": 3},
                "boundaryRings": {"type": "integer", "minimum": 1, "maximum": 20, "default": 2},
                "minArea": {"type": "number", "minimum": 0, "description": "mm^2; smaller pieces are dropped (default 0.2% of the scan's area, or of the area inside within)."},
                "image": {"anyOf": [{"type": "boolean"}, {"type": "object"}]},
                "detail": sc::detail_schema()
            }
        }),
    )]
}

pub(crate) fn mcp_dispatch(state: &AppState, arguments: &Value) -> Value {
    match features_value(state, arguments) {
        Ok((value, images)) => sc::image_result(sc::compact(value, arguments), images),
        Err(error) => crate::mcp_tool_error(&error),
    }
}

// ---------------------------------------------------------------------------
// Topology
// ---------------------------------------------------------------------------

struct Topology {
    normals: Vec<P>,
    areas: Vec<f64>,
    /// Faces sharing an edge.
    neighbours: Vec<Vec<u32>>,
    /// Faces with a vertex on an open edge.
    boundary_touch: Vec<bool>,
}

impl Topology {
    fn new(mesh: &TriMesh) -> Self {
        let count = mesh.triangles.len();
        let mut edges: HashMap<(u32, u32), Vec<u32>> = HashMap::with_capacity(count * 3 / 2 + 1);
        for (t, tri) in mesh.triangles.iter().enumerate() {
            for k in 0..3 {
                let (a, b) = (tri[k], tri[(k + 1) % 3]);
                edges.entry((a.min(b), a.max(b))).or_default().push(t as u32);
            }
        }
        let mut neighbours = vec![Vec::new(); count];
        let mut open_vertices = HashSet::new();
        for (&(a, b), faces) in &edges {
            if faces.len() == 1 {
                open_vertices.insert(a);
                open_vertices.insert(b);
            }
            // Only manifold edges join faces: a non-manifold fan (many faces
            // on one edge) stops growth there and cannot blow up the lists.
            if let [a, b] = faces[..] {
                neighbours[a as usize].push(b);
                neighbours[b as usize].push(a);
            }
        }
        let (normals, areas): (Vec<P>, Vec<f64>) = (0..count).map(|t| face_normal_area(mesh, t)).unzip();
        let boundary_touch = mesh.triangles.iter().map(|t| t.iter().any(|v| open_vertices.contains(v))).collect();
        Self { normals, areas, neighbours, boundary_touch }
    }

    /// Faces within `rings` neighbour steps of a face in `start`.
    fn rings(&self, start: &[bool], rings: usize) -> Vec<bool> {
        let mut out = start.to_vec();
        let mut frontier: Vec<usize> = (0..out.len()).filter(|&t| out[t]).collect();
        for _ in 0..rings {
            let mut next = Vec::new();
            for t in frontier {
                for &n in &self.neighbours[t] {
                    if !out[n as usize] {
                        out[n as usize] = true;
                        next.push(n as usize);
                    }
                }
            }
            frontier = next;
        }
        out
    }

    /// Per face, in degrees, how far its normal turns from the area-weighted
    /// mean normal of the faces within `rings` neighbour steps: high on
    /// fillets and rounded rims at any tessellation, ~0 on flat or gently
    /// curved surface. A face on an edge sharper than `sharp` (cos) counts
    /// as 180 (its neighbours 90); the neighbourhood never reaches across
    /// such an edge, so a sharp crease marks a narrow band, not a wide one.
    fn bend(&self, rings: usize, sharp: f64, deadline: Instant) -> Result<Vec<f64>, String> {
        let count = self.normals.len();
        let mut out = vec![0.0; count];
        let mut mark = vec![u32::MAX; count];
        let mut frontier = Vec::new();
        let mut next = Vec::new();
        for t in 0..count {
            if t % 16_384 == 0 {
                meshops::check(Some(deadline))?;
            }
            if self.neighbours[t].iter().any(|&n| dot(self.normals[n as usize], self.normals[t]) < sharp) {
                out[t] = 180.0;
                continue;
            }
            let mut sum = scale(self.normals[t], self.areas[t]);
            mark[t] = t as u32;
            frontier.clear();
            frontier.push(t);
            for _ in 0..rings {
                next.clear();
                for &f in &frontier {
                    for &n in &self.neighbours[f] {
                        if mark[n as usize] != t as u32 && dot(self.normals[n as usize], self.normals[f]) >= sharp {
                            mark[n as usize] = t as u32;
                            sum = add(sum, scale(self.normals[n as usize], self.areas[n as usize]));
                            next.push(n as usize);
                        }
                    }
                }
                std::mem::swap(&mut frontier, &mut next);
            }
            out[t] = dot(unit(sum), self.normals[t]).clamp(-1.0, 1.0).acos().to_degrees();
        }
        // Along a crease only one triangle of each quad touches the edge:
        // their mates (next to a crease face) count as bent too, so a crease
        // is one connected band rather than a string of separate faces.
        let crease: Vec<bool> = out.iter().map(|&b| b >= 180.0).collect();
        for t in 0..count {
            if !crease[t] && self.neighbours[t].iter().any(|&n| crease[n as usize]) {
                out[t] = out[t].max(90.0);
            }
        }
        Ok(out)
    }

    /// Connected pieces of `selected` where faces sharing only a vertex
    /// connect too: a crease band's triangles often meet that way at corners.
    fn vertex_components(&self, mesh: &TriMesh, selected: &[bool]) -> Vec<Vec<usize>> {
        let mut parent: Vec<usize> = (0..selected.len()).collect();
        fn root(parent: &mut [usize], mut t: usize) -> usize {
            while parent[t] != t {
                parent[t] = parent[parent[t]];
                t = parent[t];
            }
            t
        }
        let mut first: HashMap<u32, usize> = HashMap::new();
        for (t, tri) in mesh.triangles.iter().enumerate() {
            if !selected[t] {
                continue;
            }
            for &v in tri {
                match first.get(&v) {
                    Some(&other) => {
                        let (a, b) = (root(&mut parent, t), root(&mut parent, other));
                        if a != b {
                            parent[a] = b;
                        }
                    }
                    None => {
                        first.insert(v, t);
                    }
                }
            }
        }
        let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
        for t in (0..selected.len()).filter(|&t| selected[t]) {
            groups.entry(root(&mut parent, t)).or_default().push(t);
        }
        let mut out: Vec<Vec<usize>> = groups.into_values().collect();
        out.sort_by_key(|group| group[0]);
        out
    }

}

// ---------------------------------------------------------------------------
// Face identity across a scan and its regions
// ---------------------------------------------------------------------------

fn submesh(mesh: &TriMesh, faces: &[usize]) -> TriMesh {
    let mut remap: HashMap<u32, u32> = HashMap::new();
    let mut out = TriMesh::default();
    for &t in faces {
        let tri = mesh.triangles[t].map(|v| {
            *remap.entry(v).or_insert_with(|| {
                out.points.push(mesh.points[v as usize]);
                (out.points.len() - 1) as u32
            })
        });
        out.triangles.push(tri);
    }
    out
}

// ---------------------------------------------------------------------------
// A scan's pose in the design
// ---------------------------------------------------------------------------

struct Pose {
    /// Scan (asset) coordinates to design coordinates.
    to_design: Mat4,
    source: Value,
}

fn pose(design: &mut Design, input: &Value, from: &str, mesh: &TriMesh) -> Result<Option<Pose>, String> {
    let object = input.get("objectId").and_then(Value::as_str);
    let given = input.get("matrix").filter(|v| !v.is_null());
    let m = match (object, given) {
        (Some(_), Some(_)) => return Err("Give objectId or matrix, not both.".into()),
        (Some(object), None) => {
            let (m, residual, _) = design.frame_of(object, from, mesh)?;
            (m, json!({"objectId": object, "residual": residual}))
        }
        (None, Some(m)) => (sc::matrix(m)?, json!({"matrix": true})),
        (None, None) => return Ok(None),
    };
    let mut source = m.1;
    source["matrix"] = sc::rounded_matrix(&m.0);
    Ok(Some(Pose { to_design: m.0, source }))
}

fn dir_to_design(pose: &Option<Pose>, d: P) -> P {
    match pose {
        None => unit(d),
        Some(pose) => {
            let o = meshops::apply(&pose.to_design, [0.0; 3]);
            unit(sub(meshops::apply(&pose.to_design, d), o))
        }
    }
}

pub(crate) type Images = Vec<(String, Vec<u8>)>;

/// How many faces use each undirected edge.
fn edge_uses(mesh: &TriMesh) -> HashMap<(u32, u32), u32> {
    let mut uses: HashMap<(u32, u32), u32> = HashMap::new();
    for t in &mesh.triangles {
        for k in 0..3 {
            let (a, b) = (t[k], t[(k + 1) % 3]);
            *uses.entry((a.min(b), a.max(b))).or_insert(0) += 1;
        }
    }
    uses
}

fn border_length(mesh: &TriMesh) -> f64 {
    edge_uses(mesh).iter().filter(|(_, n)| **n == 1).map(|((a, b), _)| norm(sub(mesh.points[*a as usize], mesh.points[*b as usize]))).sum()
}

/// Open-boundary loops of a mesh (connected chains of single-use edges).
fn border_loops(mesh: &TriMesh) -> usize {
    let uses = edge_uses(mesh);
    let mut parent: HashMap<u32, u32> = HashMap::new();
    fn root(parent: &mut HashMap<u32, u32>, v: u32) -> u32 {
        let mut r = v;
        while let Some(&p) = parent.get(&r).filter(|&&p| p != r) {
            r = p;
        }
        parent.insert(v, r);
        r
    }
    for (&(a, b), &n) in &uses {
        if n == 1 {
            parent.entry(a).or_insert(a);
            parent.entry(b).or_insert(b);
            let (ra, rb) = (root(&mut parent, a), root(&mut parent, b));
            if ra != rb {
                parent.insert(ra, rb);
            }
        }
    }
    let vertices: Vec<u32> = parent.keys().copied().collect();
    vertices.into_iter().map(|v| root(&mut parent, v)).collect::<HashSet<_>>().len()
}

/// Vertices on the inner borders of the faces in `selected` (every border
/// loop but the longest, the outer one): the rims of its openings.
fn inner_border_vertices(mesh: &TriMesh, selected: &[bool]) -> HashSet<u32> {
    let mut uses: HashMap<(u32, u32), u32> = HashMap::new();
    for (t, tri) in mesh.triangles.iter().enumerate() {
        if selected[t] {
            for k in 0..3 {
                let (a, b) = (tri[k], tri[(k + 1) % 3]);
                *uses.entry((a.min(b), a.max(b))).or_insert(0) += 1;
            }
        }
    }
    let edges: Vec<(u32, u32)> = uses.into_iter().filter(|(_, n)| *n == 1).map(|(e, _)| e).collect();
    let mut parent: HashMap<u32, u32> = HashMap::new();
    fn root(parent: &mut HashMap<u32, u32>, v: u32) -> u32 {
        let mut r = v;
        while let Some(&p) = parent.get(&r).filter(|&&p| p != r) {
            r = p;
        }
        parent.insert(v, r);
        r
    }
    for &(a, b) in &edges {
        parent.entry(a).or_insert(a);
        parent.entry(b).or_insert(b);
        let (ra, rb) = (root(&mut parent, a), root(&mut parent, b));
        if ra != rb {
            parent.insert(ra, rb);
        }
    }
    let mut length: HashMap<u32, f64> = HashMap::new();
    for &(a, b) in &edges {
        let r = root(&mut parent, a);
        *length.entry(r).or_insert(0.0) += norm(sub(mesh.points[a as usize], mesh.points[b as usize]));
    }
    let outer = length.iter().max_by(|a, b| a.1.total_cmp(b.1)).map(|(r, _)| *r);
    let vertices: Vec<u32> = parent.keys().copied().collect();
    vertices.into_iter().filter(|&v| Some(root(&mut parent, v)) != outer).collect()
}

// ---------------------------------------------------------------------------
// extract_features
// ---------------------------------------------------------------------------

/// Most features of each kind, and in all, one call stores.
const MAX_PER_KIND: usize = 6;

/// The bounds of `faces` of a posed mesh, of only their part inside `within`
/// when there is a box.
fn extent(posed: &TriMesh, faces: &[usize], within: Option<&sc::Within>) -> Option<(P, P)> {
    let mut bounds: Option<(P, P)> = None;
    for &t in faces {
        let corners = sc::corners(posed, t);
        let points = within.map_or_else(|| corners.to_vec(), |within| within.clip_triangle(corners));
        for p in points {
            let (lo, hi) = bounds.get_or_insert((p, p));
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
    }
    bounds
}

struct Feature {
    kind: &'static str,
    label: String,
    area: f64,
    faces: Vec<usize>,
}

fn features_value(state: &AppState, input: &Value) -> Result<(Value, Images), String> {
    let started = Instant::now();
    let deadline = started + BUDGET;
    let _scope = crate::csg::bsp::deadline_scope(deadline);
    let id = crate::argument_workspace_id(input)?;
    let from = input.get("from").and_then(Value::as_str).ok_or("from is required: the scan to analyse, e.g. \"vent.stl\".")?;
    let metas = state.workspaces.asset_list(id)?;
    metas.iter().find(|meta| meta.name == from).ok_or_else(|| crate::assets::no_asset_message(from))?;
    let transition = sc::number(input, "transitionAngleDeg", 15.0, 1.0, 90.0)?;
    let sharp_deg = sc::number(input, "creaseAngleDeg", 40.0, 1.0, 180.0)?;
    let rings = sc::integer(input, "rings", 3, 1, 8)?;
    let boundary_rings = sc::integer(input, "boundaryRings", 2, 1, 20)?;
    let image = sc::image_wanted(input, true)?;
    let within = sc::Within::parse(input)?;
    let parent_asset = crate::assets::try_lookup(from)?.ok_or_else(|| format!("This design has no asset {from:?}."))?;
    let mut design = Design::new(state, input);
    let pose = pose(&mut design, input, from, &parent_asset.mesh)?;
    let mesh: &TriMesh = &parent_asset.mesh;
    let topo = Topology::new(mesh);
    let count = mesh.triangles.len();
    // Features are found on the whole scan, so the box's walls invent no
    // boundary; then only what lies inside counts, by the area inside.
    let posed_parent = match &pose {
        Some(pose) => meshops::transform(mesh, &pose.to_design)?,
        None => mesh.clone(),
    };
    let counted: Vec<f64> = match &within {
        Some(within) => (0..count).map(|t| within.clipped_area(sc::corners(&posed_parent, t))).collect(),
        None => topo.areas.clone(),
    };
    let min_area = sc::number(input, "minArea", counted.iter().sum::<f64>() * 0.002, 0.0, 1e12)?;
    let band = topo.rings(&topo.boundary_touch, boundary_rings - 1);
    let bend = topo.bend(rings, sharp_deg.to_radians().cos(), deadline)?;
    let usable: Vec<bool> = (0..count).map(|t| !band[t]).collect();
    let bent: Vec<bool> = (0..count).map(|t| usable[t] && bend[t] > transition).collect();
    let smooth: Vec<bool> = (0..count).map(|t| usable[t] && bend[t] <= transition).collect();
    let area_of = |faces: &[usize]| faces.iter().map(|&t| counted[t]).sum::<f64>();
    // The pieces worth listing, largest first: each down to its faces inside
    // the box, then judged by the area inside.
    let by_area = |groups: Vec<Vec<usize>>| {
        let mut groups: Vec<Vec<usize>> = groups
            .into_iter()
            .map(|mut faces| {
                if within.is_some() {
                    faces.retain(|&t| counted[t] > 0.0);
                }
                faces
            })
            .filter(|faces| !faces.is_empty())
            .collect();
        groups.sort_by(|a, b| area_of(b).total_cmp(&area_of(a)));
        groups.into_iter().filter(|g| area_of(g) >= min_area).collect::<Vec<_>>()
    };
    let mut features: Vec<Feature> = Vec::new();
    // Smooth surfaces. The largest on the whole scan, whatever the box holds,
    // is the main surface rims are judged by.
    let mut surfaces = topo.vertex_components(mesh, &smooth);
    let whole_area = |faces: &[usize]| faces.iter().map(|&t| topo.areas[t]).sum::<f64>();
    surfaces.sort_by(|a, b| whole_area(b).total_cmp(&whole_area(a)));
    let mut main = vec![false; count];
    if let Some(first) = surfaces.first() {
        for &t in first {
            main[t] = true;
        }
    }
    let main_normal = surfaces.first().map(|faces| unit(faces.iter().fold([0.0; 3], |acc, &t| add(acc, scale(topo.normals[t], topo.areas[t])))));
    let mut others = 1;
    for faces in by_area(surfaces).into_iter().take(MAX_PER_KIND) {
        let label = if main[faces[0]] {
            "main surface".to_string()
        } else {
            others += 1;
            format!("surface {others}")
        };
        features.push(Feature { kind: "surface", label, area: area_of(&faces), faces });
    }
    // Bent bands next to the main surface ring its openings: rims (a band
    // that is not a ring is the main surface's edge, a transition); bands
    // between the other surfaces are wall transitions. Corners where both
    // meet split at `rings` steps from the main surface.
    let close = topo.rings(&main, rings);
    let openings = inner_border_vertices(mesh, &main);
    let (mut rims, mut transitions) = (0, 0);
    for side in [true, false] {
        let mask: Vec<bool> = (0..count).map(|t| bent[t] && close[t] == side).collect();
        // A band is a rim as a whole: the box may hold only a stretch of it
        // away from the opening.
        let mut on_rim = vec![false; count];
        let bands = topo.vertex_components(mesh, &mask);
        for faces in &bands {
            if side && faces.iter().any(|&t| mesh.triangles[t].iter().any(|v| openings.contains(v))) {
                faces.iter().for_each(|&t| on_rim[t] = true);
            }
        }
        for faces in by_area(bands) {
            let rim = on_rim[faces[0]];
            let (kind, n) = if rim { ("rim", &mut rims) } else { ("transition", &mut transitions) };
            if *n >= MAX_PER_KIND {
                continue;
            }
            *n += 1;
            let label = if rim { format!("opening rim {n}") } else { format!("wall transition {n}") };
            features.push(Feature { kind, label, area: area_of(&faces), faces });
        }
    }
    // The scan's open boundary, one band per loop.
    let boundary_faces: Vec<bool> = band.clone();
    for (i, faces) in by_area(topo.vertex_components(mesh, &boundary_faces)).into_iter().take(MAX_PER_KIND).enumerate() {
        let label = if i == 0 { "scan boundary".to_string() } else { format!("scan boundary {}", i + 1) };
        features.push(Feature { kind: "boundary", label, area: area_of(&faces), faces });
    }
    let inside = within.as_ref().map(|within| within.stats(&posed_parent));
    let mut listed = Vec::new();
    let mut colors = vec![sc::UNSUPPORTED_GREY; count];
    let mut keys = Vec::new();
    for (i, feature) in features.iter().enumerate() {
        let color = match feature.kind {
            "boundary" => [90, 90, 90, 255],
            _ => sc::REGION_COLORS[i % sc::REGION_COLORS.len()],
        };
        for &t in &feature.faces {
            colors[t] = color;
        }
        keys.push((color, feature.label.clone()));
        let mut item = json!({
            "kind": feature.kind,
            "label": feature.label,
            "triangles": feature.faces.len(),
            "area": round_to(feature.area, 2),
            "meanNormal": r3(dir_to_design(&pose, unit(feature.faces.iter().fold([0.0; 3], |acc, &t| add(acc, scale(topo.normals[t], counted[t])))))),
        });
        // Inside a box a piece's border is mostly where the box cut it.
        if within.is_none() {
            let piece = submesh(mesh, &feature.faces);
            item["borderLoops"] = json!(border_loops(&piece));
            item["borderLength"] = json!(round_to(border_length(&piece), 2));
        }
        if let Some((lo, hi)) = extent(&posed_parent, &feature.faces, within.as_ref()) {
            item["bounds"] = json!({"min": r3(lo), "max": r3(hi), "size": r3(sub(hi, lo))});
            // A flat feature still needs a box with depth.
            let margin = (0.02 * norm(sub(hi, lo))).max(0.1);
            item["within"] = json!({"min": r3(sub(lo, [margin; 3])), "max": r3(add(hi, [margin; 3]))});
        }
        listed.push(item);
    }
    let counts = |kind: &str| features.iter().filter(|f| f.kind == kind).count();
    let mut value = json!({
        "ok": true,
        "designId": id,
        "from": from,
        "counts": {"rims": counts("rim"), "transitions": counts("transition"), "boundaries": counts("boundary"), "surfaces": counts("surface")},
        "features": listed,
        "parameters": {"transitionAngleDeg": transition, "creaseAngleDeg": sharp_deg, "rings": rings, "boundaryRings": boundary_rings, "minArea": round_to(min_area, 3)},
        "unclassifiedArea": round_to((0..count).filter(|&t| colors[t] == sc::UNSUPPORTED_GREY).map(|t| counted[t]).sum::<f64>(), 2),
        "frame": pose.as_ref().map(|pose| pose.source.clone()),
        "note": "Rims: crease or fillet bands around an opening in the main surface; transitions: every other crease or fillet band; boundaries: the scan's open edge. Pass a feature's within to another tool to keep to it.",
    });
    if let (Some(within), Some(stats)) = (&within, inside) {
        value["within"] = within.public(stats.public());
        if stats.triangles == 0 {
            value["nothingInside"] = json!(true);
            value["note"] = json!("Nothing inside the box: no feature of the scan lies within it.");
        } else if features.is_empty() {
            value["note"] = json!(format!("The scan is inside the box, but every piece there is under minArea ({} mm^2).", round_to(min_area, 3)));
        }
    }
    if crate::activity::collecting() && pose.is_some() {
        let legend_keys: Vec<Value> = keys.iter().map(|(color, text)| json!({"color": color, "text": text})).collect();
        crate::activity::offer_painted(&format!("features of {from}"), &posed_parent, &colors, json!({"title": "scan features", "keys": legend_keys}));
    }
    let mut images = Vec::new();
    if let Some(image) = image {
        let scene = vec![SceneObject::painted(from, &posed_parent, &colors)];
        let (lo, hi) = meshops::bounds(&posed_parent).unwrap_or(([0.0; 3], [1.0; 3]));
        let normal = main_normal.map_or([0.0, 0.0, 1.0], |normal| dir_to_design(&pose, normal));
        let view = sc::facing_view(&image, scale(add(lo, hi), 0.5), normal, norm(sub(hi, lo)) * 2.2);
        // Pictures carry geometry and the colour legend only; the numbers are in the reply.
        let legend = sc::Legend { title: "features".into(), range: 0.0, signed: false, dead: 0.0, keys: keys.clone() };
        let (png, _) = sc::render_scene(&scene, &view, Some(&legend), &[], None)?;
        images.push((format!("Features of {from}: each in its own colour (legend), dark = scan boundary, grey = unclassified."), png));
    }
    value["elapsedMs"] = json!(started.elapsed().as_millis() as u64);
    Ok((value, images))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scancommon::testkit::{call, state};

    /// A flat 40 x 30 mm sheet (open edge all round) with a 20 x 8 mm slot
    /// recessed 4 mm: vertical walls and a floor, 1 mm cells, outward up.
    fn slotted_sheet() -> TriMesh {
        let mut mesh = TriMesh::default();
        let mut index = std::collections::HashMap::new();
        let mut vertex = |mesh: &mut TriMesh, p: [i32; 3]| -> u32 {
            *index.entry(p).or_insert_with(|| {
                mesh.points.push([p[0] as f64, p[1] as f64, p[2] as f64]);
                mesh.points.len() as u32 - 1
            })
        };
        let in_slot = |x: i32, y: i32| (-10..10).contains(&x) && (-4..4).contains(&y);
        for x in -20..20 {
            for y in -15..15 {
                let z = if in_slot(x, y) { -4 } else { 0 };
                let (a, b, c, d) = (vertex(&mut mesh, [x, y, z]), vertex(&mut mesh, [x + 1, y, z]), vertex(&mut mesh, [x + 1, y + 1, z]), vertex(&mut mesh, [x, y + 1, z]));
                mesh.triangles.extend([[a, b, c], [a, c, d]]);
            }
        }
        // The slot's walls, facing into the slot.
        let mut wall = |mesh: &mut TriMesh, from: (i32, i32), to: (i32, i32)| {
            let (a, b) = (vertex(mesh, [from.0, from.1, -4]), vertex(mesh, [to.0, to.1, -4]));
            let (c, d) = (vertex(mesh, [to.0, to.1, 0]), vertex(mesh, [from.0, from.1, 0]));
            mesh.triangles.extend([[a, b, c], [a, c, d]]);
        };
        for x in -10..10 {
            wall(&mut mesh, (x + 1, 4), (x, 4));
            wall(&mut mesh, (x, -4), (x + 1, -4));
        }
        for y in -4..4 {
            wall(&mut mesh, (10, y), (10, y + 1));
            wall(&mut mesh, (-10, y + 1), (-10, y));
        }
        mesh
    }

    #[test]
    fn features_separate_rims_transitions_and_boundaries_and_store_nothing() {
        let (state, root) = state();
        let id = state.workspaces.create("%translate([0, 0, 5]) import(\"slot.stl\");\ntranslate([0, 0, -20]) cube(1);\n".into(), None).unwrap().id;
        crate::scancommon::testkit::upload(&state, &id, "slot.stl", &slotted_sheet());
        let reply = call(&state, "extract_features", json!({"designId": id, "from": "slot.stl", "objectId": "background-1"}));
        assert_eq!(reply["isError"], json!(false), "{reply}");
        let s = &reply["structuredContent"];
        assert_eq!(s["counts"]["boundaries"], json!(1), "{s}");
        assert!(s["counts"]["rims"].as_u64().unwrap() >= 1, "{s}");
        assert!(s["counts"]["transitions"].as_u64().unwrap() >= 1, "{s}");
        let features = s["features"].as_array().unwrap();
        let find = |kind: &str| features.iter().find(|f| f["kind"] == kind).unwrap().clone();
        let (rim, transition, boundary, main) = (find("rim"), find("transition"), find("boundary"), find("surface"));
        // The rim rings the slot at the sheet's level (z = 5 in the design);
        // the transition is the floor's edge, 4 mm down; the boundary is the
        // sheet's outer edge.
        assert!((rim["bounds"]["max"][2].as_f64().unwrap() - 5.0).abs() < 1e-6, "{rim}");
        assert!(rim["bounds"]["size"][0].as_f64().unwrap() < 30.0, "{rim}");
        assert!(transition["bounds"]["min"][2].as_f64().unwrap() < 2.0, "{transition}");
        assert!(boundary["bounds"]["size"][0].as_f64().unwrap() > 39.0, "{boundary}");
        assert_eq!(main["label"], json!("main surface"));
        // It stored nothing.
        assert_eq!(state.workspaces.asset_list(&id).unwrap().len(), 1, "only the upload");
        assert!(rim.get("select").is_none() && rim.get("surface").is_none() && rim.get("name").is_none(), "{rim}");
        // A feature's box is valid input to another tool, in the same frame.
        let compared = call(&state, "compare_surfaces", json!({
            "designId": id, "reference": {"asset": "slot.stl", "frameOf": "background-1"}, "candidate": {"asset": "slot.stl", "frameOf": "background-1"},
            "within": rim["within"], "samples": 500, "image": false
        }));
        assert_eq!(compared["isError"], json!(false), "{compared}");
        let inside = &compared["structuredContent"]["within"]["reference"];
        assert!(inside["areaFraction"].as_f64().unwrap() < 1.0 && inside["triangles"].as_u64().unwrap() > 0, "{compared}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A flat sheet of 1 mm cells, `cells` wide, with a 2 x 2 mm pit 1 mm deep
    /// at (10, 10): the pit's band is far under 0.2% of the scan.
    fn pitted_sheet(cells: i32) -> TriMesh {
        let mut mesh = TriMesh::default();
        let mut index = std::collections::HashMap::new();
        let mut vertex = |mesh: &mut TriMesh, p: [i32; 3]| -> u32 {
            *index.entry(p).or_insert_with(|| {
                mesh.points.push(p.map(f64::from));
                mesh.points.len() as u32 - 1
            })
        };
        let in_pit = |x: i32, y: i32| (10..12).contains(&x) && (10..12).contains(&y);
        for x in 0..cells {
            for y in 0..cells {
                let z = if in_pit(x, y) { -1 } else { 0 };
                let (a, b, c, d) = (vertex(&mut mesh, [x, y, z]), vertex(&mut mesh, [x + 1, y, z]), vertex(&mut mesh, [x + 1, y + 1, z]), vertex(&mut mesh, [x, y + 1, z]));
                mesh.triangles.extend([[a, b, c], [a, c, d]]);
            }
        }
        for (from, to) in [((11, 12), (10, 12)), ((12, 12), (11, 12)), ((10, 10), (11, 10)), ((11, 10), (12, 10)), ((12, 10), (12, 11)), ((12, 11), (12, 12)), ((10, 11), (10, 10)), ((10, 12), (10, 11))] {
            let (a, b) = (vertex(&mut mesh, [from.0, from.1, -1]), vertex(&mut mesh, [to.0, to.1, -1]));
            let (c, d) = (vertex(&mut mesh, [to.0, to.1, 0]), vertex(&mut mesh, [from.0, from.1, 0]));
            mesh.triangles.extend([[a, b, c], [a, c, d]]);
        }
        mesh
    }

    #[test]
    fn a_small_feature_survives_the_default_min_area_inside_a_small_box() {
        let (state, root) = state();
        let id = state.workspaces.create("cube(1);\n".into(), None).unwrap().id;
        crate::scancommon::testkit::upload(&state, &id, "pit.stl", &pitted_sheet(110));
        // On the whole scan the pit's band is under the default.
        let whole = call(&state, "extract_features", json!({"designId": id, "from": "pit.stl", "image": false}));
        let s = &whole["structuredContent"];
        assert_eq!(s["counts"]["rims"], json!(0), "{s}");
        assert_eq!(s["counts"]["transitions"], json!(0), "{s}");
        // Inside a box around it, the default follows the box and it is listed.
        let boxed = json!({"min": [8, 8, -2], "max": [14, 14, 1]});
        let small = call(&state, "extract_features", json!({"designId": id, "from": "pit.stl", "image": false, "within": boxed}));
        let s = &small["structuredContent"];
        assert!(s.get("nothingInside").is_none(), "{s}");
        assert!(s["counts"]["rims"].as_u64().unwrap() >= 1, "{s}");
        let inside = s["within"]["area"].as_f64().unwrap();
        assert!((s["parameters"]["minArea"].as_f64().unwrap() - 0.002 * inside).abs() < 1e-2, "{s}");
        let surface = s["features"].as_array().unwrap().iter().find(|f| f["kind"] == "surface").unwrap();
        assert_eq!(surface["label"], json!("main surface"), "{s}");
        // A box holding only a stretch of the band, its wall faces that do not
        // reach the opening's edge, still calls it what the whole band is.
        let wall = call(&state, "extract_features", json!({"designId": id, "from": "pit.stl", "image": false, "within": {"min": [10.2, 9.9, -0.9], "max": [11.8, 10.1, -0.1]}}));
        assert_eq!(wall["structuredContent"]["counts"]["rims"], json!(1), "{wall}");
        // An explicit minArea above everything inside says so: this is not "nothing inside".
        let above = call(&state, "extract_features", json!({"designId": id, "from": "pit.stl", "image": false, "within": boxed, "minArea": 1000.0}));
        let s = &above["structuredContent"];
        assert_eq!(s["features"], json!([]), "{s}");
        assert!(s.get("nothingInside").is_none(), "{s}");
        assert!(s["note"].as_str().unwrap().contains("minArea"), "{s}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn more_pieces_than_the_limit_elsewhere_do_not_crowd_out_the_one_in_the_box() {
        // Eight blocks of falling size: all creases, one transition each. The
        // largest fill MAX_PER_KIND on the whole scan; the limit applies to
        // what is inside.
        let mut mesh = TriMesh::default();
        for (i, size) in [12, 11, 10, 9, 8, 7, 6, 3].into_iter().enumerate() {
            let offset = (i as f64) * 40.0;
            let base = mesh.points.len() as u32;
            let s = size as f64;
            for z in [0.0, s] {
                for (x, y) in [(0.0, 0.0), (s, 0.0), (s, s), (0.0, s)] {
                    mesh.points.push([x + offset, y, z]);
                }
            }
            for [a, b, c] in [[0, 2, 1], [0, 3, 2], [4, 5, 6], [4, 6, 7], [0, 1, 5], [0, 5, 4], [1, 2, 6], [1, 6, 5], [2, 3, 7], [2, 7, 6], [3, 0, 4], [3, 4, 7]] {
                mesh.triangles.push([base + a, base + b, base + c]);
            }
        }
        let (state, root) = state();
        let id = state.workspaces.create("cube(1);\n".into(), None).unwrap().id;
        crate::scancommon::testkit::upload(&state, &id, "blocks.stl", &mesh);
        let reply = call(&state, "extract_features", json!({"designId": id, "from": "blocks.stl", "image": false, "minArea": 0.0}));
        let all = reply["structuredContent"]["features"].as_array().unwrap();
        assert!(all.iter().all(|f| f["bounds"]["min"][0].as_f64().unwrap() < 280.0), "the eighth block misses the limit: {reply}");
        let boxed = call(&state, "extract_features", json!({"designId": id, "from": "blocks.stl", "image": false, "within": {"min": [279, -1, -1], "max": [284, 4, 4]}}));
        let s = &boxed["structuredContent"];
        assert!(s.get("nothingInside").is_none(), "{s}");
        assert!(!s["features"].as_array().unwrap().is_empty(), "{s}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_small_box_on_a_big_face_reports_the_part_inside() {
        let (state, root) = state();
        let id = state.workspaces.create("cube(1);\n".into(), None).unwrap().id;
        // One big face of two triangles, 100 x 100 mm.
        let big = TriMesh {
            points: vec![[0.0, 0.0, 0.0], [100.0, 0.0, 0.0], [100.0, 100.0, 0.0], [0.0, 100.0, 0.0]],
            triangles: vec![[0, 1, 2], [0, 2, 3]],
        };
        crate::scancommon::testkit::upload(&state, &id, "face.stl", &big);
        let reply = call(&state, "extract_features", json!({"designId": id, "from": "face.stl", "image": false, "within": {"min": [10, 40, -1], "max": [20, 45, 1]}}));
        let s = &reply["structuredContent"];
        let feature = &s["features"][0];
        // 10 x 5 mm, whatever the triangles that touch it span.
        assert!((feature["area"].as_f64().unwrap() - 50.0).abs() < 0.01, "{feature}");
        assert_eq!(feature["bounds"]["size"], json!([10.0, 5.0, 0.0]), "{feature}");
        let suggested = &feature["within"];
        let (lo, hi) = (suggested["min"].as_array().unwrap(), suggested["max"].as_array().unwrap());
        assert!(lo[0].as_f64().unwrap() > 9.0 && hi[0].as_f64().unwrap() < 21.0, "{suggested}");
        assert!(lo[1].as_f64().unwrap() > 39.0 && hi[1].as_f64().unwrap() < 46.0, "{suggested}");
        assert!(lo[2].as_f64().unwrap() < 0.0 && hi[2].as_f64().unwrap() > 0.0, "a flat feature still gets depth: {suggested}");
        assert!(feature.get("borderLength").is_none(), "{feature}");
        assert!((s["unclassifiedArea"].as_f64().unwrap()).abs() < 0.01, "{s}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn within_lists_only_what_is_inside_the_box() {
        let (state, root) = state();
        let id = state.workspaces.create("%import(\"slot.stl\");\ntranslate([0, 0, -20]) cube(1);\n".into(), None).unwrap().id;
        crate::scancommon::testkit::upload(&state, &id, "slot.stl", &slotted_sheet());
        // Far corner of the sheet: only surface and the boundary.
        let corner = call(&state, "extract_features", json!({"designId": id, "from": "slot.stl", "image": false, "within": {"min": [14, 9, -1], "max": [21, 16, 1]}}));
        let s = &corner["structuredContent"];
        assert_eq!(s["counts"]["rims"], json!(0), "{s}");
        assert_eq!(s["counts"]["boundaries"], json!(1), "{s}");
        assert!(s["within"]["areaFraction"].as_f64().unwrap() < 0.1, "{s}");
        let nothing = call(&state, "extract_features", json!({"designId": id, "from": "slot.stl", "image": false, "within": {"min": [100, 100, 100], "max": [101, 101, 101]}}));
        assert_eq!(nothing["structuredContent"]["nothingInside"], json!(true), "{nothing}");
        assert_eq!(nothing["structuredContent"]["features"], json!([]), "{nothing}");
        let bad = call(&state, "extract_features", json!({"designId": id, "from": "slot.stl", "within": {"min": [1, 1, 1], "max": [0, 2, 2]}}));
        assert_eq!(bad["isError"], json!(true), "{bad}");
        let _ = std::fs::remove_dir_all(root);
    }
}
