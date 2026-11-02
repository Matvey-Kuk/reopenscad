import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const html = await readFile(new URL('../index.html', import.meta.url), 'utf8');
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

function loadWith(name, env = {}) {
  return new Function('env', `with (env) { return (${sourceOf(name)}); }`)(env);
}

const mobileQuery = javascript.match(/const MOBILE_MEDIA = '([^']+)';/)?.[1];

// The phone block, from its media query to the closing brace at column 0.
function mobileBlock() {
  const start = css.indexOf(`@media ${mobileQuery} {`);
  assert.ok(start >= 0, 'styles.css has a block for MOBILE_MEDIA');
  return css.slice(start, css.indexOf('\n}', start));
}

test('phones get a viewer instead of a desktop-only gate', () => {
  assert.doesNotMatch(html, /mobile-gate/);
  assert.doesNotMatch(css, /mobile-gate/);
  assert.ok(mobileQuery, 'MOBILE_MEDIA is declared');
  assert.match(mobileQuery, /max-width: 760px/);
  assert.match(javascript, /window\.matchMedia\(MOBILE_MEDIA\)/);
});

test('the phone layout hides the editor and the console, and keeps the viewport', () => {
  const block = mobileBlock();
  assert.match(block, /\.editor-pane, \.splitter, \.console-pane \{ display: none; \}/);
  assert.match(block, /\.app-shell > \.status-bar \{ display: none; \}/);
  assert.match(block, /\.viewer-pane \{ grid-column: 1; grid-row: 1; \}/);
  // The toolbar keeps Share and Export and sheds the editing tools.
  assert.match(block, /\.toolbar > \.tool-group, \.toolbar > \.agent-button \{ display: none; \}/);
  assert.match(html, /id="shareButton"/);
  assert.match(html, /id="exportButton"/);
});

test('every phone-only element stays out of the desktop shell', () => {
  // Hidden outside the query, shown only inside it.
  assert.match(css, /^\.mobile-only \{ display: none; \}/m);
  assert.match(mobileBlock(), /\.mobile-only:not\(\[hidden\]\) \{ display: flex; \}/);
  assert.match(html, /class="mobile-tool mobile-only" data-sheet="objects"/);
});

test('objects open as a bottom sheet that keeps its desktop behaviour', () => {
  const block = mobileBlock();
  assert.match(block, /body\[data-mobile-sheet="objects"\] \.objects-pane \{ transform: none;/);
  // The sheets are the same panels, so visibility toggles and per-object export ride along.
  assert.match(javascript, /class="visibility-toggle"/);
  assert.match(javascript, /class="object-export"/);
  // The badge on the Objects button tracks the list.
  assert.match(sourceOf('renderObjectList'), /\$\('#mobileObjectCount'\)\.textContent = String\(sceneObjects\.length\);/);
});

test('one sheet is open at a time and every way out closes it', () => {
  const body = { dataset: {} };
  const scrim = { hidden: true };
  const buttons = [{ dataset: { sheet: 'objects' }, attrs: {} }, { dataset: { sheet: 'other' }, attrs: {} }]
    .map((button) => ({ ...button, setAttribute(name, value) { this.attrs[name] = value; } }));
  const env = {
    document: { body },
    $: () => scrim,
    $$: () => buttons,
  };
  const open = loadWith('openMobileSheet', env);
  const close = loadWith('closeMobileSheet', env);
  open('objects');
  assert.equal(body.dataset.mobileSheet, 'objects');
  assert.equal(scrim.hidden, false);
  assert.deepEqual(buttons.map((button) => button.attrs['aria-expanded']), ['true', 'false']);
  open('other');
  assert.deepEqual(buttons.map((button) => button.attrs['aria-expanded']), ['false', 'true']);
  close();
  assert.equal(body.dataset.mobileSheet, undefined);
  assert.equal(scrim.hidden, true);
  assert.deepEqual(buttons.map((button) => button.attrs['aria-expanded']), ['false', 'false']);
  // Scrim, handle, Escape, the landing page, and
  // widening past the breakpoint.
  assert.match(javascript, /\$\('#mobileSheetScrim'\)\.addEventListener\('click', closeMobileSheet\)/);
  assert.match(javascript, /\$\$\('\[data-close-sheet\]'\)\.forEach\(\(button\) => button\.addEventListener\('click', closeMobileSheet\)\)/);
  assert.match(javascript, /event\.key === 'Escape' && document\.body\.dataset\.mobileSheet\) closeMobileSheet\(\)/);
  assert.match(sourceOf('showLanding'), /closeMobileSheet\(\);/);
  assert.match(javascript, /if \(!mobileLayout\.matches\) closeMobileSheet\(\);/);
});

test('export menus become bottom action sheets on a phone with every format', () => {
  const block = mobileBlock();
  assert.match(block, /\.export-popover, \.share-popover \{ z-index: 45; left: 8px !important; right: 8px; top: auto !important;/);
  for (const format of ['stl', '3mf', 'step', 'obj', 'off', 'glb']) {
    assert.match(html, new RegExp(`data-format="${format}"`));
  }
  assert.match(html, /id="exportPng"/);
});

test('touch drives the camera: one finger orbits, two pinch and pan', () => {
  // The canvas claims touches, so the page neither scrolls nor zooms under a gesture.
  assert.match(css, /@media \(any-pointer: coarse\) \{[^}]*\.render-canvas \{ touch-action: none; \}/);
  // Touch takes its own path before the mouse bindings see it.
  assert.match(javascript, /if \(event\.pointerType === 'touch'\) \{ beginTouchGesture\(event\); return; \}/);
  assert.match(javascript, /if \(event\.pointerType === 'touch'\) \{ moveTouchGesture\(event\); return; \}/);
  for (const type of ['pointerup', 'pointercancel', 'lostpointercapture']) {
    assert.match(javascript, new RegExp(`renderCanvas\\.addEventListener\\('${type}', endTouchGesture\\)`));
  }

  const frame = loadWith('touchFrame');
  assert.deepEqual(frame([{ x: 10, y: 20 }]), { x: 10, y: 20, spread: 0 });
  assert.deepEqual(frame([{ x: 0, y: 0 }, { x: 30, y: 40 }]), { x: 15, y: 20, spread: 50 });

  const pinch = loadWith('pinchDistance');
  // Fingers twice as far apart halve the camera distance.
  assert.equal(pinch(400, 100, 200, 100), 200);
  assert.equal(pinch(400, 100, 50, 100), 800);
  // Bounded like the wheel, and a degenerate spread changes nothing.
  assert.equal(pinch(400, 100, 1000000, 10), 0.2);
  assert.equal(pinch(400, 100, 1, 10), 300);
  assert.equal(pinch(400, 0, 50, 10), 400);
});

test('a finger lifted mid-pinch carries on as an orbit from where the camera is', () => {
  const touchPoints = new Map();
  const camera = [1, 2, 3, 30, 0, 45, 400];
  const env = { touchPoints, camera, touchFrame: loadWith('touchFrame'), touchGesture: null };
  const anchor = loadWith('anchorTouchGesture', env);
  touchPoints.set(1, { x: 0, y: 0 });
  touchPoints.set(2, { x: 100, y: 0 });
  anchor();
  assert.equal(env.touchGesture.fingers, 2);
  assert.equal(env.touchGesture.frame.spread, 100);
  camera[6] = 250;
  touchPoints.delete(2);
  anchor();
  assert.equal(env.touchGesture.fingers, 1);
  assert.equal(env.touchGesture.distance, 250);
  assert.deepEqual(env.touchGesture.frame, { x: 0, y: 0, spread: 0 });
  touchPoints.clear();
  anchor();
  assert.equal(env.touchGesture, null);
});

test('the phone landing page keeps recents and drops what only leads into the editor', () => {
  const block = mobileBlock();
  assert.match(block, /\.landing-new, \.landing-examples \{ display: none; \}/);
  assert.match(block, /\.landing-touch-note \{ display: block; \}/);
  assert.doesNotMatch(block, /\.landing-recents[^{]*\{ display: none/);
  // Connecting an agent works from a phone, so the hero keeps it.
  assert.doesNotMatch(block, /\.landing-(?:connect|agents)[^{]*\{ display: none/);
  assert.match(html, /class="landing-touch-note">[^<]*3D viewer[^<]*</);
  // An empty design says what to do instead of "press F5".
  assert.match(sourceOf('updateEmptyState'), /\$\('#emptyMobileHint'\)\.hidden = !empty;/);
  assert.match(block, /#emptyHint \{ display: none; \}/);
});
