import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const javascript = await readFile(new URL('../app.js', import.meta.url), 'utf8');
const css = await readFile(new URL('../styles.css', import.meta.url), 'utf8');

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

const featureEdgeLines = new Function(`const FEATURE_EDGE_MIN_DEGREES = 20; return (${sourceOf('featureEdgeLines')});`)();

const positions = (triangles) => Float32Array.from(triangles.flat(2));
const segments = (lines) => lines.length / 6;
const horizontal = (lines) => {
  let count = 0;
  for (let i = 0; i < lines.length; i += 6) if (lines[i + 2] === lines[i + 5]) count += 1;
  return count;
};

function box() {
  const v = [[0, 0, 0], [1, 0, 0], [1, 1, 0], [0, 1, 0], [0, 0, 1], [1, 0, 1], [1, 1, 1], [0, 1, 1]];
  const faces = [[0, 2, 1], [0, 3, 2], [4, 5, 6], [4, 6, 7], [0, 1, 5], [0, 5, 4], [1, 2, 6], [1, 6, 5], [2, 3, 7], [2, 7, 6], [3, 0, 4], [3, 4, 7]];
  return faces.map((face) => face.map((i) => v[i]));
}

// A closed cylinder as an extrusion: a fan on each end, two triangles per facet.
function cylinder(n) {
  const rim = (i, z) => [Math.cos(2 * Math.PI * (i % n) / n), Math.sin(2 * Math.PI * (i % n) / n), z];
  const triangles = [];
  for (let i = 0; i < n; i += 1) {
    triangles.push([rim(0, 1), rim(i, 1), rim(i + 1, 1)], [rim(0, 0), rim(i + 1, 0), rim(i, 0)]);
    triangles.push([rim(i, 0), rim(i + 1, 0), rim(i + 1, 1)], [rim(i, 0), rim(i + 1, 1), rim(i, 1)]);
  }
  return triangles;
}

test('Edges draws a cube\'s 12 edges, not the diagonals of its triangulated faces', () => {
  const lines = featureEdgeLines(positions(box()));
  assert.equal(segments(lines), 12);
  for (let i = 0; i < lines.length; i += 6) {
    const changed = [0, 1, 2].filter((axis) => lines[i + axis] !== lines[i + 3 + axis]);
    assert.equal(changed.length, 1, 'an edge runs along one axis; a diagonal along two');
  }
});

test('Edges skips the facets of a smooth curve and keeps the facets of a coarse one', () => {
  const smooth = featureEdgeLines(positions(cylinder(64)));
  assert.equal(segments(smooth), 128, 'the two rims only');
  assert.equal(horizontal(smooth), 128);
  const hexagon = featureEdgeLines(positions(cylinder(6)));
  assert.equal(segments(hexagon), 18, 'two rims and six corners');
});

test('Edges draws an open boundary and an edge more than two faces share', () => {
  const flat = [[[0, 0, 0], [1, 0, 0], [1, 1, 0]], [[0, 0, 0], [1, 1, 0], [0, 1, 0]]];
  assert.equal(segments(featureEdgeLines(positions(flat))), 4, 'the outline, not the diagonal');
  const fin = [...flat, [[1, 0, 0], [1, 1, 0], [1, 1, 1]]];
  assert.equal(segments(featureEdgeLines(positions(fin))), 6, 'the fin stands on the outline: a crease and its own two open edges');
  const three = [...flat, [[0, 0, 0], [1, 1, 0], [0, 0, 1]]];
  const lines = featureEdgeLines(positions(three));
  assert.equal(segments(lines), 7, 'the diagonal shared by three faces draws');
});

test('-0 and 0 are one vertex to Edges, and a zero-area triangle adds nothing', () => {
  const flat = [[[0, 0, 0], [1, 0, 0], [1, 1, 0]], [[-0, 0, 0], [1, 1, 0], [0, 1, 0]], [[2, 2, 2], [2, 2, 2], [3, 3, 3]]];
  assert.equal(segments(featureEdgeLines(positions(flat))), 4);
});

test('the viewport builds edge lines from the mesh, only when Edges is on', () => {
  assert.doesNotMatch(javascript, /triangles \* 18/);
  assert.match(javascript, /featureEdgeLines\(batch\.positions\)/);
  assert.match(javascript, /gl\.drawArrays\(gl\.LINES, 0, batch\.edgeVertices\)/);
});

test('a revision\'s Restore takes the click: its hidden meta cannot sit over the button', () => {
  // opacity below 1 makes a stacking context, painted above the button in the same cell.
  const rule = /\.revision-row:hover small:has\(\+ \.revision-restore\)[^{]*\{([^}]*)\}/.exec(css);
  assert.ok(rule);
  assert.match(rule[1], /visibility: hidden/);
  assert.doesNotMatch(rule[1], /opacity/);
});

test('the app is never wider than the window, and a short window takes it from the console', () => {
  assert.match(css, /\.app-shell \{[^}]*grid-template-columns: minmax\(0, 1fr\)/);
  assert.match(css, /\.workspace \{[^}]*grid-template-rows: minmax\(200px, 1fr\) minmax\(0, var\(--console-height\)\)/);
});

test('a refused save does not retry the same source', () => {
  const save = sourceOf('saveWorkspace');
  const refused = save.indexOf('response.status === 422');
  assert.ok(refused > 0 && refused < save.indexOf('if (!response.ok) throw'));
  const branch = save.slice(refused, save.indexOf('if (!response.ok) throw'));
  assert.match(branch, /workspaceOnline = true/);
  assert.match(branch, /return false/);
  // Only newer source goes again; the same source is not resent.
  assert.match(branch, /if \(editor\.value !== code \|\| workspaceSaveQueued\) scheduleWorkspaceSave\(\)/);
  assert.match(save, /workspaceSaveQueued = true;\s*return workspaceSavePromise/);
  // A link minted now would show older source than the editor's.
  assert.match(sourceOf('ensureShareToken'), /const saved = await flushWorkspaceSave\(\);\s*if \(!workspaceId \|\| !saved\) return null/);
  // An upload can make the parked source valid again.
  assert.match(sourceOf('applyAssetsSummary'), /if \(workspaceHasLocalChanges\) scheduleWorkspaceSave\(\)/);
});

test('a render replaced by a newer one is not logged as cancelled', () => {
  const render = sourceOf('render');
  assert.match(render, /if \(activeRender === request\) \{\s*log\(`Render cancelled after/);
});
