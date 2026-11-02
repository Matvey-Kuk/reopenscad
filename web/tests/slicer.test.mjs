import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { methodsOf } from './source.mjs';

const html = await readFile(new URL('../index.html', import.meta.url), 'utf8');
const javascript = await readFile(new URL('../app.js', import.meta.url), 'utf8');

function sourceOf(name) {
  const start = javascript.indexOf(`function ${name}(`);
  assert.ok(start >= 0, `${name} is defined`);
  let depth = 0;
  let index = javascript.indexOf('{', javascript.indexOf(')', start));
  for (; index < javascript.length; index += 1) {
    if (javascript[index] === '{') depth += 1;
    if (javascript[index] === '}' && --depth === 0) break;
  }
  return javascript.slice(start, index + 1);
}

const slicer = new Function(`
  const SLICE_AXES = ['x', 'y', 'z'];
  ${['sliceRange', 'clampSlicePosition', 'slicePlane', 'sliceRectangle', 'sliceCapColor', 'hasOpenEdges'].map(sourceOf).join('\n')}
  return { sliceRange, clampSlicePosition, slicePlane, sliceRectangle, sliceCapColor, hasOpenEdges };
`)();

const bounds = { min: [-10, 0, 2], max: [30, 5, 2] };

test('the slider spans exactly the model bounds on the chosen axis', () => {
  assert.deepEqual(slicer.sliceRange(bounds, 'x'), { min: -10, max: 30, step: 0.1 });
  assert.deepEqual(slicer.sliceRange(bounds, 'y'), { min: 0, max: 5, step: 0.01 });
  // A flat model still has a (zero-width) range rather than none.
  assert.deepEqual(slicer.sliceRange(bounds, 'z'), { min: 2, max: 2, step: 0.001 });
  assert.equal(slicer.sliceRange(null, 'x'), null);
  assert.equal(slicer.sliceRange(bounds, 'w'), null);
  assert.equal(slicer.sliceRange({ min: [Infinity, 0, 0], max: [-Infinity, 1, 1] }, 'x'), null);
});

test('a cut survives a recompile, pulled onto the new bounds only when it falls outside', () => {
  const range = { min: 0, max: 20, step: 0.1 };
  assert.equal(slicer.clampSlicePosition(12.5, range), 12.5);
  assert.equal(slicer.clampSlicePosition(40, range), 20);
  assert.equal(slicer.clampSlicePosition(-3, range), 0);
  // An unset position starts mid-model.
  assert.equal(slicer.clampSlicePosition(NaN, range), 10);
  // No model: keep what we have rather than inventing one.
  assert.equal(slicer.clampSlicePosition(7, null), 7);
});

test('the clip plane discards the far side, or the near side when flipped', () => {
  const keeps = (plane, point) => point[0] * plane[0] + point[1] * plane[1] + point[2] * plane[2] <= plane[3];
  const plane = slicer.slicePlane('y', 3, false);
  assert.deepEqual(plane, [0, 1, 0, 3]);
  assert.ok(keeps(plane, [0, 2, 0]));
  assert.ok(!keeps(plane, [0, 4, 0]));
  const flipped = slicer.slicePlane('y', 3, true);
  assert.deepEqual(flipped, [0, -1, 0, -3]);
  assert.ok(!keeps(flipped, [0, 2, 0]));
  assert.ok(keeps(flipped, [0, 4, 0]));
  assert.deepEqual(slicer.slicePlane('x', -2, false), [1, 0, 0, -2]);
  assert.deepEqual(slicer.slicePlane('z', 5, false), [0, 0, 1, 5]);
});

test('the cap rectangle lies on the plane and covers the padded footprint', () => {
  const corners = slicer.sliceRectangle(bounds, 'x', 4, 1);
  assert.equal(corners.length, 4);
  for (const corner of corners) assert.equal(corner[0], 4);
  assert.deepEqual(corners[0], [4, -1, 1]);
  assert.deepEqual(corners[2], [4, 6, 3]);
  const z = slicer.sliceRectangle(bounds, 'z', 2, 0);
  assert.deepEqual(z, [[-10, 0, 2], [30, 0, 2], [30, 5, 2], [-10, 5, 2]]);
  assert.equal(slicer.sliceRectangle(null, 'x', 0), null);
});

test('the cap colour is distinct from the part but stays in gamut', () => {
  const cap = slicer.sliceCapColor([0.98, 0.77, 0.19]);
  assert.notDeepEqual(cap, [0.98, 0.77, 0.19]);
  for (const channel of cap) assert.ok(channel >= 0 && channel <= 1);
});

test('the section view is wired into the viewport', () => {
  assert.match(html, /id="slicerButton"[^>]*aria-pressed="false"/);
  assert.match(html, /id="slicerPanel"[^>]*hidden/);
  for (const axis of ['x', 'y', 'z']) assert.match(html, new RegExp(`data-slice-axis="${axis}"`));
  assert.match(html, /id="slicerPosition"/);
  assert.match(html, /id="slicerFlip"/);
  // Caps need a stencil buffer, and the plane has to reach the fragment shader.
  assert.match(javascript, /getContext\('webgl', \{[^}]*stencil: true/);
  assert.match(javascript, /uniform vec4 uClip;/);
  // The cut outlives a re-render: setMesh clamps it instead of resetting it.
  assert.match(sourceOf('syncSlicerControls'), /sliceRange\(meshViewport\.bounds/);
  assert.match(javascript, /this\.slice\.position = clampSlicePosition\(this\.slice\.position/);
  // Toggling must not move the camera.
  const toggle = javascript.slice(javascript.indexOf("$('#slicerButton').addEventListener"), javascript.indexOf("slicerPosition.addEventListener('input'"));
  assert.doesNotMatch(toggle, /camera|fitView/);
});

test('open surfaces are told apart from closed ones, so a cut caps only solids', () => {
  // A tetrahedron as a triangle soup, then the same minus one face.
  const v = [[0, 0, 0], [1, 0, 0], [0, 1, 0], [0, 0, 1]];
  const faces = [[0, 2, 1], [0, 1, 3], [1, 2, 3], [0, 3, 2]];
  const soup = (list) => new Float32Array(list.flatMap((face) => face.flatMap((i) => v[i])));
  assert.equal(slicer.hasOpenEdges(soup(faces), 4), false);
  assert.equal(slicer.hasOpenEdges(soup(faces.slice(1)), 3), true);
  assert.equal(slicer.hasOpenEdges(null, 0), false);
});

test('section caps skip open parts and never take the back-face tint', () => {
  const log = [];
  const gl = new Proxy({}, { get: (_, name) => (...args) => { log.push({ name, args }); } });
  const edgeChecks = [];
  const env = {
    SLICE_AXES: ['x', 'y', 'z'],
    DEFAULT_MESH_COLOR: [1, 1, 1],
    hasOpenEdges: (positions) => { edgeChecks.push(positions); return positions === 'open'; },
    sliceRectangle: () => [[0, 0, 0], [1, 0, 0], [1, 1, 0], [0, 1, 0]],
    sliceCapColor: (color) => color,
  };
  const viewport = new Function('env', `with (env) { return ({ ${methodsOf(javascript, ['drawSliceCaps'])} }); }`)(env);
  Object.assign(viewport, {
    gl,
    hasStencil: true,
    radius: 10,
    slice: { axis: 'x', position: 0 },
    locations: { backTint: 'backTint', normal: 'normal', position: 'position' },
    applyClip() {},
    setMaterial() {},
  });
  const batch = (positions, triangles) => ({ positions, triangles, min: [-1, -1, -1], max: [1, 1, 1] });
  const closed = batch('closed', 4);
  const open = batch('open', 5);
  const material = { color: [1, 0, 0] };
  const draw = () => viewport.drawSliceCaps([{ batch: open, material }, { batch: closed, material }], [1, 0, 0, 0]);
  draw();
  const draws = log.filter((call) => call.name === 'drawArrays');
  assert.deepEqual(draws.map((call) => call.args[2]), [12, 6], 'the stencil pass and the cap of the closed part, nothing of the open one');
  // Off before the first draw, back on after the last.
  const tints = log.filter((call) => call.name === 'uniform1f' && call.args[0] === 'backTint');
  assert.deepEqual(tints.map((call) => call.args[1]), [0, 1]);
  assert.ok(log.indexOf(tints[0]) < log.indexOf(draws[0]));
  assert.ok(log.indexOf(tints[1]) > log.indexOf(draws[1]));
  // Worked out once per part, however often the cut moves.
  draw();
  assert.deepEqual(edgeChecks, ['open', 'closed']);
  // A shader only a real GPU can run, so this one stays on its text.
  assert.match(javascript, /bool back = uBackTint > 0\.5 && uLighting > 0\.5 && !gl_FrontFacing;/);
});
