//! The embedded MCP app: a self-contained HTML page that previews a workspace
//! in 3D and links its STL / 3MF exports.
//!
//! MCP hosts render `ui://` resources in a sandboxed frame on an origin that
//! is not this server's, and the server sends no CORS headers, so the page
//! cannot fetch anything back. It therefore ships the geometry inline and
//! keeps only the download links as absolute URLs, which a click navigates to
//! rather than fetches.
//!
//! Geometry is inlined as a compact quantised, indexed mesh: raw STL would be
//! 50 bytes per triangle and marching-cubes output routinely runs to six
//! figures of triangles, which no host wants to carry in a resource payload.

use crate::engine::{self, Mesh};
use crate::threemf::IndexedMesh;

/// Triangle budget for the inline preview.
///
/// At 18 bytes of vertex data and 12 bytes of index data per triangle this
/// caps the encoded payload at roughly one megabyte before base64.
const PREVIEW_TRIANGLE_BUDGET: usize = 40_000;

/// Preview meshes are welded onto progressively coarser grids until they fit
/// the budget. Starting near the mesher's own resolution keeps the first
/// attempt lossless for small models.
const FINEST_PREVIEW_CELLS: f64 = 256.0;
const COARSEST_PREVIEW_CELLS: f64 = 8.0;

/// An inline-ready preview: a base64 `RMSH` blob plus what it cost.
pub struct Preview {
    pub blob_base64: String,
    pub vertices: usize,
    pub triangles: usize,
    /// True when the preview was decimated to fit [`PREVIEW_TRIANGLE_BUDGET`].
    pub simplified: bool,
}

/// Weld and quantise `mesh` into the `RMSH` blob the viewer decodes.
///
/// Layout (little-endian):
/// `"RMSH"`, version `u16`, reserved `u16`, vertex count `u32`,
/// triangle count `u32`, origin `3 x f32`, span `3 x f32`, then
/// `vertices x 3 x u16` positions normalised into `[0, 65535]` over the span,
/// then `triangles x 3 x u32` indices.
pub fn encode_preview(mesh: &Mesh) -> Preview {
    let (origin, span) = mesh_extent(mesh);
    let longest = span[0].max(span[1]).max(span[2]).max(1e-9);
    let mut indexed = IndexedMesh::weld(mesh);
    let mut simplified = false;
    let mut cells = FINEST_PREVIEW_CELLS;
    while indexed.triangles.len() > PREVIEW_TRIANGLE_BUDGET && cells >= COARSEST_PREVIEW_CELLS {
        indexed = IndexedMesh::weld_on_grid(mesh, cells / longest);
        simplified = true;
        cells /= 2.0;
    }
    // Index slots are u32, so the vertex count has to fit; the budget above
    // makes this unreachable in practice but the cast must stay sound.
    let vertices = indexed.vertices.len().min(u32::MAX as usize);
    let triangles: Vec<_> = indexed
        .triangles
        .iter()
        .filter(|triangle| triangle.iter().all(|index| (*index as usize) < vertices))
        .collect();

    let mut blob = Vec::with_capacity(16 + 24 + vertices * 6 + triangles.len() * 12);
    blob.extend_from_slice(b"RMSH");
    blob.extend_from_slice(&1u16.to_le_bytes());
    blob.extend_from_slice(&0u16.to_le_bytes());
    blob.extend_from_slice(&(vertices as u32).to_le_bytes());
    blob.extend_from_slice(&(triangles.len() as u32).to_le_bytes());
    for axis in origin {
        blob.extend_from_slice(&(axis as f32).to_le_bytes());
    }
    for axis in span {
        blob.extend_from_slice(&(axis as f32).to_le_bytes());
    }
    for vertex in indexed.vertices.iter().take(vertices) {
        for (axis, (base, length)) in [vertex.x, vertex.y, vertex.z]
            .into_iter()
            .zip(origin.into_iter().zip(span))
        {
            let normalized = if length > 1e-12 {
                ((axis - base) / length).clamp(0.0, 1.0)
            } else {
                0.0
            };
            blob.extend_from_slice(&((normalized * 65535.0).round() as u16).to_le_bytes());
        }
    }
    for triangle in &triangles {
        for index in triangle.iter() {
            blob.extend_from_slice(&index.to_le_bytes());
        }
    }
    Preview {
        blob_base64: base64_encode(&blob),
        vertices,
        triangles: triangles.len(),
        simplified,
    }
}

fn mesh_extent(mesh: &Mesh) -> ([f64; 3], [f64; 3]) {
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for triangle in &mesh.triangles {
        for vertex in triangle.vertices {
            for (axis, value) in [vertex.x, vertex.y, vertex.z].into_iter().enumerate() {
                if !value.is_finite() {
                    continue;
                }
                min[axis] = min[axis].min(value);
                max[axis] = max[axis].max(value);
            }
        }
    }
    let mut origin = [0.0; 3];
    let mut span = [0.0; 3];
    for axis in 0..3 {
        if min[axis].is_finite() && max[axis].is_finite() {
            origin[axis] = min[axis];
            span[axis] = max[axis] - min[axis];
        }
    }
    (origin, span)
}

/// Render the app for one workspace.
///
/// `export_url` is a closure so the caller keeps ownership of URL building
/// (scheme, host and revision pinning all live in the server module).
pub fn viewer_html(
    workspace_id: &str,
    workspace_name: &str,
    revision: u64,
    parts: &[engine::CompiledPart],
    preview: &Preview,
    workspace_url: &str,
    stl_url: &str,
    threemf_url: &str,
) -> String {
    let objects = parts
        .iter()
        .map(|part| {
            format!(
                "{{\"id\":{},\"name\":{},\"triangles\":{}}}",
                json_string(&part.id),
                json_string(&part.name),
                part.mesh.triangles.len()
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    VIEWER_TEMPLATE
        .replace("__TITLE__", &escape_html(workspace_name))
        .replace("__WORKSPACE_ID__", &escape_html(workspace_id))
        .replace("__REVISION__", &revision.to_string())
        .replace("__WORKSPACE_URL__", &escape_html(workspace_url))
        .replace("__STL_URL__", &escape_html(stl_url))
        .replace("__THREEMF_URL__", &escape_html(threemf_url))
        .replace("__OBJECTS__", &format!("[{objects}]"))
        .replace("__MESH_B64__", &preview.blob_base64)
        .replace(
            "__SIMPLIFIED__",
            if preview.simplified { "true" } else { "false" },
        )
        .replace("__PREVIEW_TRIANGLES__", &preview.triangles.to_string())
}

/// Escape for HTML text and double-quoted attribute contexts.
fn escape_html(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            c => output.push(c),
        }
    }
    output
}

/// A JSON string literal that is also safe inside `<script>`.
///
/// `<` and `>` are escaped as `\uXXXX` so a name containing `</script>` cannot
/// terminate the block early.
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
            '<' => output.push_str("\\u003c"),
            '>' => output.push_str("\\u003e"),
            '&' => output.push_str("\\u0026"),
            c if (c as u32) < 0x20 => output.push_str(&format!("\\u{:04x}", c as u32)),
            c => output.push(c),
        }
    }
    output.push('"');
    output
}

pub fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let bytes = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let packed = u32::from(bytes[0]) << 16 | u32::from(bytes[1]) << 8 | u32::from(bytes[2]);
        for slot in 0..4 {
            if slot <= chunk.len() {
                output.push(ALPHABET[(packed >> (18 - 6 * slot) & 0x3F) as usize] as char);
            } else {
                output.push('=');
            }
        }
    }
    output
}

const VIEWER_TEMPLATE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<!-- This page carries absolute workspace URLs (the exports, the editor link),
     and it renders on the host's origin, where this server's own
     `Referrer-Policy` header does not reach. The one policy the page can set
     for itself goes here. -->
<meta name="referrer" content="no-referrer" />
<meta name="robots" content="noindex, nofollow, noarchive" />
<title>__TITLE__ — ReOpenSCAD</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body {
    margin: 0; height: 100vh; display: flex; flex-direction: column;
    font: 13px/1.5 ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif;
    background: #14161c; color: #e6e8ee;
  }
  header {
    display: flex; align-items: baseline; gap: 10px; flex-wrap: wrap;
    padding: 10px 14px; border-bottom: 1px solid #262a35; background: #191c24;
  }
  header h1 { margin: 0; font-size: 14px; font-weight: 600; }
  header .meta { color: #8d94a6; font-size: 12px; }
  main { flex: 1; display: flex; min-height: 0; }
  #stage { flex: 1; position: relative; min-width: 0; }
  canvas { display: block; width: 100%; height: 100%; touch-action: none; cursor: grab; }
  canvas:active { cursor: grabbing; }
  #hint { position: absolute; left: 12px; bottom: 10px; color: #737b8d; font-size: 11px; }
  aside {
    width: 232px; border-left: 1px solid #262a35; background: #191c24;
    padding: 12px; overflow: auto; display: flex; flex-direction: column; gap: 12px;
  }
  h2 { margin: 0; font-size: 11px; text-transform: uppercase; letter-spacing: .07em; color: #8d94a6; }
  ul { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 4px; }
  li { display: flex; justify-content: space-between; gap: 8px; padding: 5px 7px; border-radius: 5px; background: #21252f; }
  li b { font-weight: 500; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  li span { color: #8d94a6; font-variant-numeric: tabular-nums; }
  .downloads { display: flex; flex-direction: column; gap: 6px; }
  a.download {
    display: flex; justify-content: space-between; align-items: center; gap: 8px;
    padding: 8px 10px; border-radius: 6px; text-decoration: none;
    background: #2b6cb0; color: #fff; font-weight: 600;
  }
  a.download small { font-weight: 400; opacity: .8; }
  a.download.secondary { background: #21252f; color: #e6e8ee; }
  a.open { color: #78a9ff; font-size: 12px; }
  .note { color: #c8a24a; font-size: 11px; }
  .error { padding: 16px; color: #f08a8a; }
</style>
</head>
<body>
<header>
  <h1>__TITLE__</h1>
  <span class="meta">workspace __WORKSPACE_ID__ &middot; revision __REVISION__</span>
</header>
<main>
  <div id="stage"><canvas id="view"></canvas><div id="hint">Drag to orbit &middot; scroll to zoom</div></div>
  <aside>
    <div>
      <h2>Objects</h2>
      <ul id="objects"></ul>
    </div>
    <div class="downloads">
      <h2>Export</h2>
      <a class="download" href="__STL_URL__" download>STL <small>binary mesh</small></a>
      <a class="download secondary" href="__THREEMF_URL__" download>3MF <small>OPC package</small></a>
      <a class="open" href="__WORKSPACE_URL__" target="_blank" rel="noreferrer">Open full editor &rarr;</a>
    </div>
    <div id="previewNote"></div>
  </aside>
</main>
<script>
(function () {
  var OBJECTS = __OBJECTS__;
  var MESH_B64 = "__MESH_B64__";
  var SIMPLIFIED = __SIMPLIFIED__;
  var PREVIEW_TRIANGLES = __PREVIEW_TRIANGLES__;

  var list = document.getElementById('objects');
  list.innerHTML = OBJECTS.length
    ? OBJECTS.map(function (object) {
        var name = document.createElement('b');
        name.textContent = object.name || object.id;
        var count = document.createElement('span');
        count.textContent = object.triangles.toLocaleString() + ' tris';
        var row = document.createElement('li');
        row.appendChild(name);
        row.appendChild(count);
        return row.outerHTML;
      }).join('')
    : '<li><b>No solids</b></li>';
  if (SIMPLIFIED) {
    document.getElementById('previewNote').innerHTML =
      '<p class="note">Preview simplified to ' + PREVIEW_TRIANGLES.toLocaleString() +
      ' triangles. Downloads carry the full-resolution mesh.</p>';
  }

  function decodeBase64(text) {
    var binary = atob(text);
    var bytes = new Uint8Array(binary.length);
    for (var i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
    return bytes;
  }

  // Mirrors the RMSH layout written by src/mcpapp.rs.
  function decodeMesh(bytes) {
    var view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    if (String.fromCharCode(bytes[0], bytes[1], bytes[2], bytes[3]) !== 'RMSH') throw new Error('bad mesh');
    var vertexCount = view.getUint32(8, true);
    var triangleCount = view.getUint32(12, true);
    var origin = [view.getFloat32(16, true), view.getFloat32(20, true), view.getFloat32(24, true)];
    var span = [view.getFloat32(28, true), view.getFloat32(32, true), view.getFloat32(36, true)];
    var cursor = 40;
    var positions = new Float32Array(vertexCount * 3);
    for (var v = 0; v < vertexCount; v += 1) {
      for (var axis = 0; axis < 3; axis += 1) {
        positions[v * 3 + axis] = origin[axis] + (view.getUint16(cursor, true) / 65535) * span[axis];
        cursor += 2;
      }
    }
    var indices = new Uint32Array(triangleCount * 3);
    for (var i = 0; i < triangleCount * 3; i += 1) {
      indices[i] = view.getUint32(cursor, true);
      cursor += 4;
    }
    return { positions: positions, indices: indices, origin: origin, span: span };
  }

  // Area-weighted vertex normals: accumulating the un-normalised face cross
  // products gives smooth shading without a second pass over the topology.
  function vertexNormals(positions, indices) {
    var normals = new Float32Array(positions.length);
    for (var t = 0; t < indices.length; t += 3) {
      var a = indices[t] * 3, b = indices[t + 1] * 3, c = indices[t + 2] * 3;
      var ux = positions[b] - positions[a], uy = positions[b + 1] - positions[a + 1], uz = positions[b + 2] - positions[a + 2];
      var vx = positions[c] - positions[a], vy = positions[c + 1] - positions[a + 1], vz = positions[c + 2] - positions[a + 2];
      var nx = uy * vz - uz * vy, ny = uz * vx - ux * vz, nz = ux * vy - uy * vx;
      for (var k = 0; k < 3; k += 1) {
        var base = indices[t + k] * 3;
        normals[base] += nx; normals[base + 1] += ny; normals[base + 2] += nz;
      }
    }
    for (var n = 0; n < normals.length; n += 3) {
      var length = Math.hypot(normals[n], normals[n + 1], normals[n + 2]) || 1;
      normals[n] /= length; normals[n + 1] /= length; normals[n + 2] /= length;
    }
    return normals;
  }

  function perspective(fovy, aspect, near, far) {
    var f = 1 / Math.tan(fovy / 2), d = near - far;
    return new Float32Array([f / aspect, 0, 0, 0, 0, f, 0, 0, 0, 0, (far + near) / d, -1, 0, 0, (2 * far * near) / d, 0]);
  }

  function lookAt(eye, target, up) {
    var zx = eye[0] - target[0], zy = eye[1] - target[1], zz = eye[2] - target[2];
    var zl = Math.hypot(zx, zy, zz) || 1; zx /= zl; zy /= zl; zz /= zl;
    var xx = up[1] * zz - up[2] * zy, xy = up[2] * zx - up[0] * zz, xz = up[0] * zy - up[1] * zx;
    var xl = Math.hypot(xx, xy, xz) || 1; xx /= xl; xy /= xl; xz /= xl;
    var yx = zy * xz - zz * xy, yy = zz * xx - zx * xz, yz = zx * xy - zy * xx;
    return new Float32Array([
      xx, yx, zx, 0,
      xy, yy, zy, 0,
      xz, yz, zz, 0,
      -(xx * eye[0] + xy * eye[1] + xz * eye[2]),
      -(yx * eye[0] + yy * eye[1] + yz * eye[2]),
      -(zx * eye[0] + zy * eye[1] + zz * eye[2]),
      1
    ]);
  }

  function fail(message) {
    document.getElementById('stage').innerHTML = '<p class="error">' + message + '</p>';
  }

  var mesh;
  try {
    mesh = decodeMesh(decodeBase64(MESH_B64));
  } catch (error) {
    fail('Could not decode the preview mesh.');
    return;
  }
  if (!mesh.indices.length) { fail('This workspace has no 3D geometry to preview.'); return; }

  var canvas = document.getElementById('view');
  var gl = canvas.getContext('webgl', { antialias: true, alpha: false });
  if (!gl) { fail('WebGL is unavailable in this frame.'); return; }

  function compile(type, source) {
    var shader = gl.createShader(type);
    gl.shaderSource(shader, source);
    gl.compileShader(shader);
    if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(shader));
    return shader;
  }
  var program = gl.createProgram();
  gl.attachShader(program, compile(gl.VERTEX_SHADER, [
    'attribute vec3 aPosition;',
    'attribute vec3 aNormal;',
    'uniform mat4 uProjection;',
    'uniform mat4 uView;',
    'varying vec3 vNormal;',
    'varying vec3 vEye;',
    'void main() {',
    '  vec4 eye = uView * vec4(aPosition, 1.0);',
    '  vNormal = mat3(uView) * aNormal;',
    '  vEye = -eye.xyz;',
    '  gl_Position = uProjection * eye;',
    '}'
  ].join('\n')));
  gl.attachShader(program, compile(gl.FRAGMENT_SHADER, [
    'precision mediump float;',
    'varying vec3 vNormal;',
    'varying vec3 vEye;',
    'void main() {',
    '  vec3 normal = normalize(vNormal);',
    '  vec3 eye = normalize(vEye);',
    '  if (!gl_FrontFacing) normal = -normal;',
    '  float key = max(dot(normal, normalize(vec3(0.4, 0.7, 0.9))), 0.0);',
    '  float fill = max(dot(normal, normalize(vec3(-0.6, -0.2, 0.3))), 0.0);',
    '  float rim = pow(1.0 - max(dot(normal, eye), 0.0), 2.5);',
    '  vec3 color = vec3(0.30, 0.55, 0.85) * (0.22 + 0.75 * key + 0.20 * fill) + rim * 0.18;',
    '  gl_FragColor = vec4(color, 1.0);',
    '}'
  ].join('\n')));
  gl.linkProgram(program);
  if (!gl.getProgramParameter(program, gl.LINK_STATUS)) { fail('Could not link the preview shader.'); return; }
  gl.useProgram(program);

  var normals = vertexNormals(mesh.positions, mesh.indices);
  function buffer(target, data) {
    var handle = gl.createBuffer();
    gl.bindBuffer(target, handle);
    gl.bufferData(target, data, gl.STATIC_DRAW);
    return handle;
  }
  var positionBuffer = buffer(gl.ARRAY_BUFFER, mesh.positions);
  var normalBuffer = buffer(gl.ARRAY_BUFFER, normals);

  // WebGL1 needs OES_element_index_uint for 32-bit indices; without it the
  // preview falls back to 16-bit, which the triangle budget keeps viable.
  var uintIndices = !!gl.getExtension('OES_element_index_uint');
  var indexData = uintIndices ? mesh.indices : new Uint16Array(mesh.indices.length);
  var indexCount = mesh.indices.length;
  if (!uintIndices) {
    indexCount = 0;
    for (var i = 0; i < mesh.indices.length; i += 3) {
      if (mesh.indices[i] > 65535 || mesh.indices[i + 1] > 65535 || mesh.indices[i + 2] > 65535) continue;
      indexData[indexCount] = mesh.indices[i];
      indexData[indexCount + 1] = mesh.indices[i + 1];
      indexData[indexCount + 2] = mesh.indices[i + 2];
      indexCount += 3;
    }
  }
  buffer(gl.ELEMENT_ARRAY_BUFFER, indexData);

  var positionLocation = gl.getAttribLocation(program, 'aPosition');
  var normalLocation = gl.getAttribLocation(program, 'aNormal');
  gl.bindBuffer(gl.ARRAY_BUFFER, positionBuffer);
  gl.enableVertexAttribArray(positionLocation);
  gl.vertexAttribPointer(positionLocation, 3, gl.FLOAT, false, 0, 0);
  gl.bindBuffer(gl.ARRAY_BUFFER, normalBuffer);
  gl.enableVertexAttribArray(normalLocation);
  gl.vertexAttribPointer(normalLocation, 3, gl.FLOAT, false, 0, 0);
  gl.enable(gl.DEPTH_TEST);
  gl.clearColor(0.078, 0.086, 0.110, 1);

  var center = [
    mesh.origin[0] + mesh.span[0] / 2,
    mesh.origin[1] + mesh.span[1] / 2,
    mesh.origin[2] + mesh.span[2] / 2
  ];
  var radius = Math.max(Math.hypot(mesh.span[0], mesh.span[1], mesh.span[2]) / 2, 1e-3);
  var yaw = -0.9, pitch = 0.55, distance = radius * 3.2;
  var projectionLocation = gl.getUniformLocation(program, 'uProjection');
  var viewLocation = gl.getUniformLocation(program, 'uView');

  function draw() {
    var ratio = Math.min(window.devicePixelRatio || 1, 2);
    var width = Math.max(1, Math.round(canvas.clientWidth * ratio));
    var height = Math.max(1, Math.round(canvas.clientHeight * ratio));
    if (canvas.width !== width || canvas.height !== height) {
      canvas.width = width; canvas.height = height;
    }
    gl.viewport(0, 0, canvas.width, canvas.height);
    gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);
    var eye = [
      center[0] + distance * Math.cos(pitch) * Math.cos(yaw),
      center[1] + distance * Math.cos(pitch) * Math.sin(yaw),
      center[2] + distance * Math.sin(pitch)
    ];
    gl.uniformMatrix4fv(projectionLocation, false,
      perspective(0.9, canvas.width / canvas.height, radius * 0.02, distance + radius * 8));
    gl.uniformMatrix4fv(viewLocation, false, lookAt(eye, center, [0, 0, 1]));
    gl.drawElements(gl.TRIANGLES, indexCount, uintIndices ? gl.UNSIGNED_INT : gl.UNSIGNED_SHORT, 0);
  }

  var dragging = null;
  canvas.addEventListener('pointerdown', function (event) {
    dragging = { x: event.clientX, y: event.clientY };
    canvas.setPointerCapture(event.pointerId);
  });
  canvas.addEventListener('pointermove', function (event) {
    if (!dragging) return;
    yaw -= (event.clientX - dragging.x) * 0.01;
    pitch = Math.max(-1.5, Math.min(1.5, pitch + (event.clientY - dragging.y) * 0.01));
    dragging = { x: event.clientX, y: event.clientY };
    draw();
  });
  canvas.addEventListener('pointerup', function () { dragging = null; });
  canvas.addEventListener('pointercancel', function () { dragging = null; });
  canvas.addEventListener('wheel', function (event) {
    event.preventDefault();
    distance = Math.max(radius * 0.35, Math.min(radius * 40, distance * Math.exp(event.deltaY * 0.001)));
    draw();
  }, { passive: false });
  window.addEventListener('resize', draw);
  draw();
})();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Quality;

    fn cube_mesh() -> Mesh {
        engine::compile("cube(10);", Quality::Preview, None)
            .unwrap()
            .mesh
    }

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn preview_fits_the_triangle_budget_and_carries_a_readable_header() {
        let preview = encode_preview(&cube_mesh());
        assert!(preview.triangles > 0);
        assert!(
            preview.triangles <= PREVIEW_TRIANGLE_BUDGET,
            "{} triangles exceeds the budget",
            preview.triangles
        );
        let blob = decode_base64(&preview.blob_base64);
        assert_eq!(&blob[..4], b"RMSH");
        assert_eq!(u32::from_le_bytes(blob[8..12].try_into().unwrap()) as usize, preview.vertices);
        assert_eq!(
            u32::from_le_bytes(blob[12..16].try_into().unwrap()) as usize,
            preview.triangles
        );
        assert_eq!(blob.len(), 40 + preview.vertices * 6 + preview.triangles * 12);
        // Every index has to address a vertex that was actually written.
        for slot in 0..preview.triangles * 3 {
            let offset = 40 + preview.vertices * 6 + slot * 4;
            let index = u32::from_le_bytes(blob[offset..offset + 4].try_into().unwrap());
            assert!((index as usize) < preview.vertices);
        }
    }

    #[test]
    fn viewer_html_is_self_contained_and_substituted() {
        let output = engine::compile_parts("cube(10);", Quality::Preview, None).unwrap();
        let preview = encode_preview(&output.mesh);
        let html = viewer_html(
            "silly-otter-1",
            "Model </script><img src=x>",
            7,
            &output.parts,
            &preview,
            "http://host/workspaces/silly-otter-1",
            "http://host/export.stl",
            "http://host/export.3mf",
        );
        assert!(!html.contains("__"), "every placeholder must be substituted");
        assert!(html.contains("silly-otter-1"));
        assert!(html.contains("revision 7"));
        assert!(html.contains("http://host/export.3mf"));
        // No external subresources: the frame cannot reach this server.
        assert!(!html.contains("src=\"http"));
        assert!(!html.contains("<link"));
        // The workspace URL is a credential, and the host's origin — not this
        // server's headers — governs the frame, so the page states its own
        // policy: never send it as a referrer, never let it be indexed.
        assert!(html.contains("<meta name=\"referrer\" content=\"no-referrer\" />"));
        assert!(html.contains("<meta name=\"robots\" content=\"noindex, nofollow, noarchive\" />"));
        // The only outbound link leaves without one either way.
        assert!(html.contains("href=\"http://host/workspaces/silly-otter-1\" target=\"_blank\" rel=\"noreferrer\""));
        // The hostile name must not be able to close the script or the markup.
        assert!(!html.contains("</script><img"));
        assert!(!html.contains("<img src=x>"));
        assert_eq!(html.matches("</script>").count(), 1);
    }

    #[test]
    fn json_strings_neutralize_script_terminators() {
        assert_eq!(json_string("a</script>"), "\"a\\u003c/script\\u003e\"");
        assert_eq!(json_string("q\"\\"), "\"q\\\"\\\\\"");
    }

    fn decode_base64(text: &str) -> Vec<u8> {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut output = Vec::new();
        let mut accumulator = 0u32;
        let mut bits = 0;
        for byte in text.bytes().filter(|byte| *byte != b'=') {
            let value = ALPHABET.iter().position(|slot| *slot == byte).unwrap() as u32;
            accumulator = accumulator << 6 | value;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                output.push((accumulator >> bits) as u8);
            }
        }
        output
    }
}
