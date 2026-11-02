import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { methodsOf, statementOf } from './source.mjs';

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

// Comments and strings out, so a brace inside either cannot skew the count.
const cssCode = css.replace(/\/\*[\s\S]*?\*\//g, '').replace(/"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'/g, '""');

test('styles.css braces balance, so no rule after a stray } is dropped', () => {
  let depth = 0;
  cssCode.split('\n').forEach((line, index) => {
    for (const char of line) {
      if (char === '{') depth += 1;
      if (char === '}') depth -= 1;
      assert.ok(depth >= 0, `unmatched } on styles.css line ${index + 1} (comment-stripped)`);
    }
  });
  assert.equal(depth, 0, 'every { in styles.css is closed');
});

test('the landing page is hidden at the top level, outside every media query', () => {
  const at = cssCode.indexOf('.landing { display: none; }');
  assert.ok(at >= 0);
  const before = cssCode.slice(0, at);
  assert.equal(before.split('{').length - before.split('}').length, 0);
});

test('a recompile leaves the orthographic zoom alone; zoom and fit move it', () => {
  const page = ({ framed = true } = {}) => {
    const env = {
      camera: [0, 0, 0, 0, 0, 0, 100],
      clampSlicePosition: (position) => position,
      sliceRange: () => [0, 1],
    };
    const viewport = new Function('env', `with (env) { return ({ ${methodsOf(javascript, ['orthoHalfHeight', 'syncOrthoScale', 'setMesh', 'fitView'])} }); }`)(env);
    Object.assign(viewport, {
      framed,
      radius: 10,
      orthoRadius: 10,
      meshBatches: [],
      slice: { axis: 'x', position: 0 },
      canvas: { clientWidth: 0, clientHeight: 0 },
      gl: {},
      pivot: () => [0, 0, 0],
      draw() {},
      createMeshBatch: () => ({ min: [-30, -40, 0], max: [30, 40, 0] }),
    });
    env.meshViewport = viewport;
    return { env, viewport };
  };
  // The scale follows the model only through syncOrthoScale, never on a mesh swap.
  const recompile = page();
  const before = recompile.viewport.orthoHalfHeight();
  recompile.viewport.setMesh('mesh');
  assert.equal(recompile.viewport.radius, 50, 'the model did grow');
  assert.equal(recompile.viewport.orthoRadius, 10);
  assert.equal(recompile.viewport.orthoHalfHeight(), before);
  // A design's first mesh is framed, and a fit moves it.
  const first = page({ framed: false });
  first.viewport.setMesh('mesh');
  assert.equal(first.viewport.orthoRadius, 50);
  const fit = page();
  fit.viewport.radius = 50;
  fit.viewport.fitView();
  assert.equal(fit.viewport.orthoRadius, 50);
  // Half the height follows the camera distance, down to a floor set by the scale.
  const scale = page();
  scale.env.camera[6] = 100;
  assert.equal(scale.viewport.orthoHalfHeight(), 36);
  scale.env.camera[6] = 0;
  assert.equal(scale.viewport.orthoHalfHeight(), 10 * 0.007);
  // Every way of zooming moves it.
  const listeners = {};
  const element = (name) => ({ addEventListener: (_, listener) => { listeners[name] = listener; } });
  for (const [handler, distance] of [['#zoomIn', 82], ['#zoomOut', 120]]) {
    const { env, viewport } = page();
    viewport.radius = 50;
    env.$ = () => element(handler);
    new Function('env', `with (env) { ${statementOf(javascript, `$('${handler}').addEventListener(`)} }`)(env);
    listeners[handler]();
    assert.equal(env.camera[6], distance, handler);
    assert.equal(viewport.orthoRadius, 50, handler);
  }
  const wheel = page();
  wheel.viewport.radius = 50;
  wheel.env.$ = () => element('wheel');
  wheel.env.hasMesh = true;
  wheel.env.SWIPE_ORBIT_DEGREES_PER_PIXEL = 0;
  wheel.env.panView = () => {};
  new Function('env', `with (env) { ${statementOf(javascript, "$('#viewport').addEventListener('wheel'")} }`)(wheel.env);
  listeners.wheel({ preventDefault() {}, deltaX: 0, deltaY: 1, deltaMode: 0, ctrlKey: false, shiftKey: false });
  assert.ok(Math.abs(wheel.env.camera[6] - 110) < 1e-9, 'wheel');
  assert.equal(wheel.viewport.orthoRadius, 50, 'wheel');
  const pinch = page();
  pinch.viewport.radius = 50;
  Object.assign(pinch.env, {
    touchPoints: new Map([[1, {}]]),
    touchGesture: { fingers: 2, distance: 100, frame: { spread: 10, x: 0, y: 0 }, offset: [0, 0, 0] },
    touchFrame: () => ({ spread: 20, x: 0, y: 0 }),
    pinchDistance: (distance, start, spread) => distance * start / spread,
    panView() {},
  });
  new Function('env', `with (env) { return (${sourceOf('moveTouchGesture')}); }`)(pinch.env)({ pointerId: 1, clientX: 0, clientY: 0 });
  assert.equal(pinch.env.camera[6], 50);
  assert.equal(pinch.viewport.orthoRadius, 50, 'pinch');
});

test('opening another design turns the section view off; recompiles keep it', () => {
  const calls = [];
  const meshViewport = { framed: true, slice: { enabled: true, axis: 'x', position: 12, flip: true } };
  const env = {
    meshViewport,
    aiUpdatedRanges: [1],
    setAiBadge: () => {},
    resetAgentActivity: () => {},
    clearGeometry: () => calls.push('clear'),
    syncSlicerControls: () => calls.push('sync'),
  };
  loadWith('resetScene', env)();
  assert.equal(meshViewport.framed, false);
  assert.equal(meshViewport.slice.enabled, false);
  assert.equal(meshViewport.slice.axis, 'z');
  assert.ok(Number.isNaN(meshViewport.slice.position));
  assert.equal(meshViewport.slice.flip, false);
  assert.deepEqual(calls, ['clear', 'sync']);
  // clearGeometry (the recompile-failure path) must not touch the cut.
  assert.doesNotMatch(sourceOf('clearGeometry'), /slice/);
});

const mobileQuery = javascript.match(/const MOBILE_MEDIA = '([^']+)';/)?.[1];
function mobileBlock() {
  const start = css.indexOf(`@media ${mobileQuery} {`);
  assert.ok(start >= 0);
  return css.slice(start, css.indexOf('\n}', start));
}

test('the viewer-only layout is for touch devices, not narrow desktop windows', () => {
  for (const clause of mobileQuery.split(',')) assert.match(clause, /\(pointer: coarse\)/, clause);
});

test('a 320px phone toolbar wraps instead of pushing Export off-screen', () => {
  const block = mobileBlock();
  assert.match(block, /\.app-shell \{[^}]*grid-template-columns: minmax\(0, 1fr\);/);
  // Wraps by the desktop rule, which the phone block must leave alone.
  assert.match(css, /^\.toolbar \{ display: flex; flex-wrap: wrap;/m);
  assert.doesNotMatch(block, /flex-wrap: nowrap/);
  assert.match(block, /\.toolbar \{ row-gap: 6px; \}/);
  const narrow = css.slice(css.indexOf('@media (max-width: 400px) and (pointer: coarse) {'));
  assert.ok(narrow.startsWith('@media'), 'a very-narrow phone block exists');
  assert.match(narrow, /\.export-button kbd \{ display: none; \}/);
  // Icon-only, but the label stays in the accessibility tree.
  assert.match(narrow, /\.export-button span \{ position: absolute;[^}]*clip-path: inset\(50%\)/);
});

function fakeDom() {
  const byId = new Map();
  const make = () => {
    const parts = {};
    const element = {
      hidden: false,
      dataset: {},
      attributes: {},
      setAttribute(name, value) { this.attributes[name] = value; },
      addEventListener(type, fn) { this[`on${type}`] = fn; },
      querySelector(selector) {
        parts[selector] ||= { hidden: false, textContent: '', addEventListener(type, fn) { this[`on${type}`] = fn; } };
        return parts[selector];
      },
    };
    return element;
  };
  const document = {
    getElementById: (id) => byId.get(id) || null,
    createElement: () => make(),
    body: { append(element) { byId.set(element.id, element); } },
  };
  return { document, byId };
}

test('export progress and failures show in a toast with a retry, which a phone can see', () => {
  const { document, byId } = fakeDom();
  const env = { document, setTimeout: () => 1, clearTimeout: () => {}, exportToastTimer: 0, exportToastToken: 0 };
  env.hideExportToast = loadWith('hideExportToast', env);
  const show = loadWith('showExportToast', env);
  const settle = loadWith('settleExportToast', env);

  const token = show('Exporting STL…');
  const toast = byId.get('exportToast');
  assert.equal(toast.hidden, false);
  assert.equal(toast.attributes.role, 'status');
  assert.equal(toast.querySelector('.export-toast-retry').hidden, true);
  // An export abandoned mid-flight clears its own progress toast.
  settle(token);
  assert.equal(toast.hidden, true);

  let retried = 0;
  show('STL export failed: boom', { tone: 'error', retry: () => { retried += 1; } });
  assert.equal(toast.attributes.role, 'alert');
  const retry = toast.querySelector('.export-toast-retry');
  assert.equal(retry.hidden, false);
  retry.onclick();
  assert.equal(retried, 1);
  assert.equal(toast.hidden, true);

  const exportSource = sourceOf('exportModel');
  assert.match(exportSource, /showExportToast\(`Exporting \$\{label\}…`\)/);
  assert.match(exportSource, /tone: 'error', retry: \(\) => exportModel\(format, scope\)/);
  assert.match(exportSource, /finally \{\n\s*settleExportToast\(toastToken\);/);
  assert.match(css, /\.export-toast\[hidden\] \{ display: none; \}/);
});

test('agent tiles keep the button role inside real list items', () => {
  assert.doesNotMatch(javascript, /<button[^>]*role="listitem"[^>]*data-(?:agent|client)/);
  assert.match(html, /<ul class="landing-agent-grid" id="landingAgents"/);
  assert.match(html, /<ul class="connect-grid" id="mcpTabs"/);
  const clients = [{ id: 'claude', label: 'Claude', tagline: 'x', logo: '/logos/claude.svg' }];
  const env = { MCP_CLIENTS: clients, LANDING_AGENT_IDS: ['claude'], LANDING_AGENT_NAMES: {}, agentLogoHtml: () => '', escapeHtml: String, storedMcpClientId: () => null };
  for (const [name, id] of [['renderLandingAgents', '#landingAgents'], ['renderMcpTabs', '#mcpTabs']]) {
    const host = {};
    loadWith(name, { ...env, $: (selector) => (selector === id ? host : {}) })();
    assert.match(host.innerHTML, /^<li>(?:<span class="agent-mark"[^>]*><\/span>)?<button type="button"[^>]*>.*<\/button><\/li>$/, name);
  }
});

test('the recent designs Show all toggle reports its state and keeps focus', () => {
  assert.match(sourceOf('renderLandingRecents'), /data-recents-toggle aria-expanded="\$\{recentsExpanded\}"/);
  assert.match(javascript, /renderLandingRecents\(\);\n[^\n]*\n\s*\$\('\[data-recents-toggle\]', \$\('#landingRecents'\)\)\?\.focus\(\);/);
});
