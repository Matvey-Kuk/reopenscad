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

// render() itself on a stand-in page. `reply` answers its request; `calls` is what it did, in order.
function renderPage({ mesh = false, expected = null, overlayHidden = !mesh, reply = () => renderReply() } = {}) {
  const calls = [];
  const record = (name, then) => (...args) => {
    calls.push({ name, args });
    return then?.(...args);
  };
  const env = {
    calls,
    count: (name) => calls.filter((call) => call.name === name).length,
    position: (name) => calls.findIndex((call) => call.name === name),
    renderDebounce: 3,
    activeRender: null,
    renderSequence: 0,
    workspaceGeneration: 1,
    workspaceId: 'happy-otter',
    workspaceRevision: 1,
    readOnlyToken: null,
    readOnlyRevision: null,
    hasMesh: mesh,
    expectedRenderMs: expected,
    renderedSource: null,
    sceneObjects: [],
    sceneObjectsSource: null,
    selectedObjectId: null,
    intersectionCheckSource: null,
    projection: 'perspective',
    camera: [],
    RENDER_TIMEOUT_MS: { preview: 1000 },
    clock: 0,
    performance: { now: () => env.clock },
    editor: { value: 'cube(1);' },
    renderOverlay: { hidden: overlayHidden },
    emptyState: { hidden: false },
    renderTime: { textContent: '' },
    meshViewport: { showEdges: false, setMesh: record('setMesh'), clearMesh: record('clearMesh'), draw() {} },
    $: () => ({ textContent: '', hidden: false }),
    isCurrentWorkspace: () => true,
    flushWorkspaceSave: async () => true,
    renderDurationSample: sample,
    formatElapsed: String,
    requestAnimationFrame() {},
    setSceneObjects() {},
    log() {},
    sendRender: async (url, body, request) => {
      calls.push({ name: 'send', overlayHidden: env.renderOverlay.hidden });
      env.clock += 400;
      return reply(request, env);
    },
  };
  for (const name of ['armRenderBackstop', 'hideRenderFailure', 'showGeometryStaleness', 'startRerenderBar', 'hideRerenderBar', 'finishRerenderBar', 'startRenderElapsed', 'stopRenderElapsed', 'startQueueWatch', 'stopQueueWatch', 'setStatus', 'markGeometryCurrent', 'flushAgentPending', 'showRenderFailure', 'clearGeometry', 'cancelRender']) {
    env[name] = record(name);
  }
  env.render = new Function('env', `with (env) { return (async ${sourceOf('render')}); }`)(env);
  return env;
}

const renderReply = (data = {}, status = 200) => ({ status, data: { ok: true, mesh: 'mesh', objects: [], console: '', triangles: 1, elapsedMs: 1, queuedMs: 0, ...data } });
const abortError = () => Object.assign(new Error('aborted'), { name: 'AbortError' });

const fraction = new Function(`return (${sourceOf('rerenderBarFraction')});`)();
const sample = new Function(`return (${sourceOf('renderDurationSample')});`)();

test('the bar runs against the last duration and reaches 90% at the estimate', () => {
  assert.equal(fraction(0, 2000), 0);
  assert.equal(fraction(1000, 2000), 0.45);
  assert.equal(fraction(2000, 2000), 0.9);
  assert.equal(fraction(-5, 2000), 0, 'a clock step never runs it backwards');
});

test('past the estimate it eases toward 95% and holds, never reaching 100% or restarting', () => {
  let previous = fraction(2000, 2000);
  for (let elapsed = 2100; elapsed < 200000; elapsed += 500) {
    const value = fraction(elapsed, 2000);
    assert.ok(value >= previous, 'never moves back');
    assert.ok(value <= 0.95, 'never past the cap');
    previous = value;
  }
  assert.ok(previous > 0.949, 'it does get there');
});

test('no previous duration: indeterminate', () => {
  for (const estimate of [null, undefined, 0, Number.NaN]) assert.equal(fraction(100, estimate), null);
});

test('only real completed work sets the expectation', () => {
  assert.equal(sample(3000, {}), 3000);
  assert.equal(sample(3000, { cached: true }), null, 'a cache hit');
  assert.equal(sample(40, {}), null, 'too quick to be real work');
  assert.equal(sample(3000, {}, true), null, 'resent after a busy server or a conflict');
  assert.equal(sample(13000, { queuedMs: 10000 }), 3000, 'the queue is not the model');
  assert.equal(sample(10100, { queuedMs: 10000 }), null);
});

// The real bar code, on a stand-in element and clock. Timers are real in shape: each
// has an id, clearing one cancels only it, and elapse() runs what is due.
function barPage() {
  const classes = new Set();
  const attributes = {};
  const fill = { style: { transform: '', transition: '' }, offsetWidth: 0 };
  const percent = { textContent: '' };
  const progress = { setAttribute: (k, v) => { attributes[k] = v; }, removeAttribute: (k) => { delete attributes[k]; } };
  const bar = { hidden: true, classList: { add: (...c) => c.forEach((x) => classes.add(x)), remove: (...c) => c.forEach((x) => classes.delete(x)), toggle: (c, on) => (on ? classes.add(c) : classes.delete(c)), contains: (c) => classes.has(c) } };
  const timers = new Map();
  let nextTimer = 1;
  const page = {
    bar, fill, percent, attributes, classes, now: 0, tick: null,
    elapse(ms) {
      page.now += ms;
      for (const [id, timer] of [...timers]) {
        if (timer.at <= page.now) { timers.delete(id); timer.fn(); }
      }
    },
    pendingTimers: () => timers.size,
  };
  const elements = { '#rerenderBar': bar, '#rerenderProgress': progress, '#rerenderPercent': percent, i: fill };
  const api = new Function('page', 'elements', 'timers', 'nextId', `
    const $ = (selector) => elements[selector];
    const performance = { now: () => page.now };
    const setInterval = (fn) => { page.tick = fn; return -1; };
    const clearInterval = () => { page.tick = null; };
    const setTimeout = (fn, ms) => { const id = nextId(); timers.set(id, { fn, at: page.now + ms }); return id; };
    const clearTimeout = (id) => { timers.delete(id); };
    const RERENDER_BAR_DELAY_MS = 250;
    let rerenderBarTimer = null;
    let rerenderBarHide = null;
    let rerenderBarShow = null;
    ${['rerenderBarFraction', 'paintRerenderBar', 'startRerenderBar', 'finishRerenderBar', 'hideRerenderBar'].map(sourceOf).join('\n')}
    return { startRerenderBar, finishRerenderBar, hideRerenderBar };
  `)(page, elements, timers, () => nextTimer++);
  return { page, ...api };
}

test('the bar waits before it appears, so a fast or cached re-render never flashes it', () => {
  const { page, startRerenderBar, finishRerenderBar } = barPage();
  startRerenderBar(1000);
  assert.equal(page.bar.hidden, true);
  page.elapse(100);
  finishRerenderBar();
  page.elapse(5000);
  assert.equal(page.bar.hidden, true, 'it never showed');
  assert.equal(page.tick, null);
});

test('it is seen to run from 0 against the estimate, with the same number as text and value', () => {
  const { page, startRerenderBar } = barPage();
  startRerenderBar(1000);
  assert.equal(page.percent.textContent, '0%');
  assert.equal(page.attributes['aria-valuenow'], '0');
  assert.equal(page.tick, null, 'no clock while it waits');
  page.elapse(250);
  assert.equal(page.bar.hidden, false);
  assert.equal(page.percent.textContent, '0%', 'the delay is not counted');
  page.elapse(900);
  page.tick();
  assert.equal(page.fill.style.transform, 'scaleX(0.81)');
  assert.equal(page.percent.textContent, '81%');
  assert.equal(page.attributes['aria-valuenow'], '81');
});

test('however long it runs the number holds at 95; 100 only comes with the result', () => {
  const { page, startRerenderBar, finishRerenderBar } = barPage();
  startRerenderBar(1000);
  page.elapse(250);
  page.elapse(600000);
  page.tick();
  assert.equal(page.percent.textContent, '95%');
  finishRerenderBar();
  assert.equal(page.percent.textContent, '100%');
  assert.equal(page.attributes['aria-valuenow'], '100');
});

test('a superseding render restarts the visible bar from 0 without taking it down', () => {
  const { page, startRerenderBar } = barPage();
  startRerenderBar(1000);
  page.elapse(250);
  page.elapse(900);
  page.tick();
  assert.equal(page.fill.style.transform, 'scaleX(0.81)');
  startRerenderBar(1000);
  assert.equal(page.bar.hidden, false);
  assert.equal(page.fill.style.transform, 'scaleX(0)');
  assert.equal(page.percent.textContent, '0%');
  page.elapse(500);
  page.tick();
  assert.equal(page.fill.style.transform, 'scaleX(0.45)');
});

test('a render superseded before the bar appeared restarts the wait', () => {
  const { page, startRerenderBar } = barPage();
  startRerenderBar(1000);
  page.elapse(200);
  startRerenderBar(1000);
  page.elapse(200);
  assert.equal(page.bar.hidden, true, 'the old wait does not count');
  page.elapse(50);
  assert.equal(page.bar.hidden, false);
});

test('with no estimate it is indeterminate with no number; the result still fills it', () => {
  const { page, startRerenderBar, finishRerenderBar } = barPage();
  startRerenderBar(null);
  page.elapse(250);
  assert.equal(page.bar.hidden, false);
  assert.ok(page.classes.has('indeterminate'));
  assert.equal(page.percent.textContent, '');
  assert.equal(page.attributes['aria-valuenow'], undefined);
  assert.equal(page.fill.style.transform, '');
  assert.equal(page.tick, null);
  finishRerenderBar();
  assert.ok(!page.classes.has('indeterminate'));
  assert.equal(page.fill.style.transform, 'scaleX(1)');
  assert.equal(page.percent.textContent, '100%');
});

test('the result fills and fades the bar; only then is it gone', () => {
  const { page, startRerenderBar, finishRerenderBar } = barPage();
  startRerenderBar(1000);
  page.elapse(300);
  finishRerenderBar();
  assert.equal(page.fill.style.transform, 'scaleX(1)');
  assert.ok(page.classes.has('finishing'));
  assert.equal(page.bar.hidden, false);
  assert.equal(page.tick, null);
  page.elapse(400);
  assert.equal(page.bar.hidden, true);
});

test('a new render during the fade takes the bar back', () => {
  const { page, startRerenderBar, finishRerenderBar } = barPage();
  startRerenderBar(1000);
  page.elapse(300);
  finishRerenderBar();
  startRerenderBar(1000);
  assert.ok(!page.classes.has('finishing'));
  assert.equal(page.percent.textContent, '0%');
  page.elapse(250);
  assert.equal(page.bar.hidden, false);
  page.elapse(1000);
  assert.equal(page.bar.hidden, false, 'the old fade cannot hide the new bar');
});

test('failure and cancel hide the bar at once, and it cannot come back', () => {
  for (const shownFirst of [true, false]) {
    const { page, startRerenderBar, hideRerenderBar } = barPage();
    startRerenderBar(1000);
    if (shownFirst) page.elapse(300);
    hideRerenderBar();
    assert.equal(page.bar.hidden, true);
    assert.equal(page.tick, null);
    assert.equal(page.pendingTimers(), 0);
    page.elapse(5000);
    assert.equal(page.bar.hidden, true);
  }
});

// The real cancel, on stand-ins for everything it reaches.
function cancelPage({ active = true, hasMesh = true, renderedSource = 'old', code = 'new', debounce = 7 } = {}) {
  const calls = [];
  const request = {};
  const overlay = { hidden: false };
  const state = new Function('env', `
    let activeRender = env.active ? env.request : null;
    let renderDebounce = env.debounce;
    let renderedSource = env.renderedSource;
    const hasMesh = env.hasMesh;
    const editor = { value: env.code };
    const renderOverlay = env.overlay;
    const clearTimeout = (id) => env.calls.push(['clearTimeout', id]);
    const markGeometryStale = () => env.calls.push(['stale']);
    const showGeometryStaleness = () => env.calls.push(['showStaleness']);
    const hideRerenderBar = () => env.calls.push(['hide']);
    const stopRenderElapsed = () => env.calls.push(['stopClock']);
    const formatElapsed = (seconds) => seconds + ' s';
    const log = (message, kind) => env.calls.push(['log', message, kind]);
    const setStatus = (text) => env.calls.push(['status', text]);
    const cancelRender = (r) => env.calls.push(['cancel', r]);
    ${sourceOf('cancelRenderByUser')}
    return { run: cancelRenderByUser, debounce: () => renderDebounce, active: () => activeRender };
  `)({ active, request, debounce, renderedSource, hasMesh, code, calls, overlay });
  return { state, calls, request, overlay };
}

test('Cancel aborts the render and the edit waiting behind it, so nothing restarts', () => {
  const { state, calls, request } = cancelPage();
  state.run();
  assert.deepEqual(calls.find(([name]) => name === 'cancel'), ['cancel', request]);
  assert.deepEqual(calls.find(([name]) => name === 'clearTimeout'), ['clearTimeout', 7]);
  assert.equal(state.debounce(), null);
  assert.ok(calls.some(([name]) => name === 'hide'));
});

test('Cancel settles at once, without waiting for a save the render is parked on', () => {
  const { state, calls, overlay } = cancelPage();
  state.run();
  assert.equal(state.active(), null, 'a late end of that render finds it is no longer current');
  assert.equal(overlay.hidden, true);
  assert.ok(calls.some(([name]) => name === 'stopClock'));
  assert.ok(calls.some(([name, message]) => name === 'log' && /^Render cancelled after/.test(message)));
});

test('the status says what a cancel left behind', () => {
  const status = (options) => {
    const { state, calls } = cancelPage(options);
    state.run();
    return calls.find(([name]) => name === 'status')[1];
  };
  assert.equal(status({ renderedSource: 'old', code: 'new' }), 'Cancelled, model is out of date');
  assert.equal(status({ renderedSource: 'same', code: 'same' }), 'Cancelled');
  assert.equal(status({ hasMesh: false, renderedSource: null }), 'Cancelled');
});

test('Cancel marks the model out of date only when the editor has moved past it', () => {
  const moved = cancelPage({ renderedSource: 'old', code: 'new' });
  moved.state.run();
  assert.ok(moved.calls.some(([name]) => name === 'stale'));
  const same = cancelPage({ renderedSource: 'same', code: 'same' });
  same.state.run();
  assert.ok(!same.calls.some(([name]) => name === 'stale'));
  const first = cancelPage({ hasMesh: false, renderedSource: null });
  first.state.run();
  assert.ok(!first.calls.some(([name]) => name === 'stale'), 'a first render has no model to mark');
});

test('Cancel with nothing running leaves a waiting edit alone', () => {
  const { state, calls } = cancelPage({ active: false });
  state.run();
  assert.equal(calls.length, 0);
  assert.equal(state.debounce(), 7);
});

test('both loaders share the cancel, and a result arriving after it is dropped', () => {
  assert.match(javascript, /\$\('#rerenderCancel'\)\.addEventListener\('click', cancelRenderByUser\)/);
  assert.match(javascript, /\$\('#overlayCancel'\)\.addEventListener\('click', cancelRenderByUser\)/);
  const rendering = sourceOf('render');
  assert.match(sourceOf('cancelRender'), /request\.controller\.abort\(\);/);
  // A result that arrives anyway is dropped before it touches the viewport.
  assert.ok(rendering.indexOf('if (superseded()) return false;\n    if (status < 200') < rendering.indexOf('meshViewport.setMesh('));
  assert.match(rendering, /renderedSource = source;/);
  assert.match(rendering, /code: \(request\.source = editor\.value\),/);
  assert.match(sourceOf('clearGeometry'), /renderedSource = null;/);
  // The next edit brightens the model at once, as its render is now on the way.
  assert.match(sourceOf('syncEditor'), /scheduleSourceRender\(EDIT_RENDER_DELAY_MS\);\s*\/\/[^\n]*\n\s*showGeometryStaleness\(\);/);
});

// The report helpers on a stand-in browser.
function reportPage({ clipboard = async () => {}, code = 'cube(1);', active = true } = {}) {
  const opened = [];
  const logged = [];
  const copied = [];
  const names = ['issueReportBody', 'issueReportTitle', 'issueReportUrl', 'formatElapsed'];
  const report = new Function('env', `
    const { editor, log, window, navigator, performance } = env;
    let activeRender = env.activeRender;
    const ISSUE_NEW_URL = 'https://github.com/Matvey-Kuk/reopenscad/issues/new';
    const ISSUE_URL_LIMIT = 7000;
    ${names.map(sourceOf).join('\n')}
    async ${sourceOf('openIssueReport').replace(/^async /, '')}
    ${sourceOf('reportRunningRender')}
    return { reportRunningRender, openIssueReport };
  `)({
    activeRender: active ? { startedAt: 0, mode: 'preview', source: code } : null,
    editor: { value: 'an edit still waiting for its render' },
    log: (message, kind) => logged.push([message, kind]),
    window: { open: (...args) => opened.push(args) },
    navigator: { userAgent: 'test', clipboard: { writeText: async (text) => { copied.push(text); return clipboard(text); } } },
    performance: { now: () => 12300 },
  });
  return { ...report, opened, logged, copied };
}

test('Report an issue opens the prefilled issue for the running render, with its source', () => {
  const page = reportPage();
  page.reportRunningRender();
  assert.equal(page.opened.length, 1);
  const [url, target, features] = page.opened[0];
  assert.equal(target, '_blank');
  assert.equal(features, 'noopener,noreferrer');
  const params = new URL(url).searchParams;
  assert.equal(params.get('title'), 'fix: render stuck — Still rendering after 12.3 s');
  assert.match(params.get('body'), /still running/);
  assert.match(params.get('body'), /- \*\*Mode:\*\* preview/);
  assert.match(params.get('body'), /cube\(1\);/, 'the source being compiled');
  assert.doesNotMatch(params.get('body'), /still waiting/, 'not a newer edit');
});

test('a failure report keeps its wording', () => {
  const page = reportPage();
  page.openIssueReport({ summary: 'Parse error. Check it.', mode: 'render', code: 'x', at: 'now' });
  const params = new URL(page.opened[0][0]).searchParams;
  assert.equal(params.get('title'), 'fix: render failed — Parse error');
  assert.match(params.get('body'), /^Rendering failed in the ReOpenSCAD web app\./);
});

test('a source too long for the link goes on the clipboard in full', async () => {
  const code = 'cube(1);\n'.repeat(2000);
  const page = reportPage({ code });
  page.reportRunningRender();
  await new Promise(setImmediate);
  assert.equal(page.opened.length, 1);
  assert.ok(page.opened[0][0].length <= 7000);
  assert.ok(page.copied[0].includes(code));
  assert.match(page.logged[0][0], /copied to your clipboard/);
});

test('a refused clipboard does not point at a button this loader lacks', async () => {
  const code = 'cube(1);\n'.repeat(2000);
  const page = reportPage({ code, clipboard: async () => { throw new Error('denied'); } });
  await page.openIssueReport({ summary: 'Still rendering', mode: 'preview', code, at: 'now', running: true });
  assert.match(page.logged[0][0], /trimmed copy/);
  assert.doesNotMatch(page.logged[0][0], /Copy report/);
  const failure = reportPage({ code, clipboard: async () => { throw new Error('denied'); } });
  await failure.openIssueReport({ summary: 'Boom', mode: 'preview', code, at: 'now' });
  assert.match(failure.logged[0][0], /Copy report/);
});

test('Report does nothing with no render running, and never stops one', () => {
  const page = reportPage({ active: false });
  page.reportRunningRender();
  assert.equal(page.opened.length, 0);
  assert.doesNotMatch(sourceOf('reportRunningRender'), /cancelRender|abort/);
  assert.match(javascript, /\$\('#rerenderReport'\)\.addEventListener\('click', reportRunningRender\)/);
  assert.match(javascript, /if \(lastRenderFailure\) openIssueReport\(lastRenderFailure\);/);
});

test('render(): a model on screen keeps the overlay off; no model keeps the first-render loader', async () => {
  const again = renderPage({ mesh: true, expected: 1200, overlayHidden: false });
  await again.render();
  assert.equal(again.calls.find((call) => call.name === 'send').overlayHidden, true);
  assert.deepEqual(again.calls.find((call) => call.name === 'startRerenderBar').args, [1200]);
  assert.equal(again.count('hideRerenderBar'), 0);

  const first = renderPage({ mesh: false, overlayHidden: true });
  await first.render();
  assert.equal(first.calls.find((call) => call.name === 'send').overlayHidden, false);
  assert.equal(first.count('startRerenderBar'), 0);
  const hidden = first.position('hideRerenderBar');
  assert.ok(hidden >= 0 && hidden < first.position('send'), 'a bar left by another render goes first');
  assert.equal(first.renderOverlay.hidden, true, 'the loader goes with the result');
});

test('render(): a result that lands fills the bar and becomes the expectation', async () => {
  const page = renderPage({ mesh: true, expected: 1200 });
  assert.equal(await page.render(), true);
  const swapped = page.position('setMesh');
  assert.ok(swapped >= 0 && swapped < page.position('finishRerenderBar'), 'after the swap');
  assert.equal(page.count('hideRerenderBar'), 0);
  assert.equal(page.expectedRenderMs, 400);
  assert.equal(page.activeRender, null);
});

test('render(): a failure or a cancel takes the bar down and leaves the expectation', async () => {
  const replies = {
    failed: () => renderReply({ ok: false, error: 'boom' }, 500),
    cancelled: (request) => { request.controller.abort(); throw abortError(); },
    'timed out': (request) => { request.timedOut = true; throw abortError(); },
  };
  for (const [what, reply] of Object.entries(replies)) {
    const page = renderPage({ mesh: true, expected: 1200, reply });
    assert.equal(await page.render(), false, what);
    assert.equal(page.count('finishRerenderBar'), 0, what);
    assert.equal(page.count('hideRerenderBar'), 1, what);
    assert.equal(page.expectedRenderMs, 1200, what);
  }
  // Nothing has landed until the model is on screen.
  const page = renderPage({ mesh: true, expected: 1200 });
  page.meshViewport.setMesh = () => { throw new Error('bad mesh'); };
  assert.equal(await page.render(), false);
  assert.equal(page.count('finishRerenderBar'), 0);
  assert.equal(page.expectedRenderMs, 1200);
});

test('render(): a render with a newer one on its way touches neither the model nor the bar', async () => {
  const newer = { controller: new AbortController() };
  const page = renderPage({ mesh: true, expected: 1200, reply: (request, env) => { env.activeRender = newer; return renderReply(); } });
  assert.equal(await page.render(), false);
  assert.equal(page.count('setMesh'), 0);
  assert.equal(page.count('finishRerenderBar'), 0);
  assert.equal(page.count('hideRerenderBar'), 0);
  assert.equal(page.expectedRenderMs, 1200);
  assert.equal(page.activeRender, newer);
});

test('a render that had to wait or start over sets no expectation', async () => {
  const distorted = renderPage({ mesh: true, expected: 1200, reply: (request) => { request.distorted = true; return renderReply(); } });
  assert.equal(await distorted.render(), true);
  assert.equal(distorted.count('finishRerenderBar'), 1);
  assert.equal(distorted.expectedRenderMs, 1200);

  // The real retry: a busy server flags the request, and then answers.
  const replies = [
    { status: 429, headers: { get: () => '1' }, data: { retryable: true, error: 'busy' } },
    { status: 200, headers: { get: () => null }, data: { ok: true } },
  ];
  const env = {
    QUEUE_RETRY_LIMIT: 3,
    DEFAULT_RENDER_NOTE: '',
    activeRender: null,
    armRenderBackstop() {},
    fetch: async () => replies.shift(),
    readRenderResponse: async (response) => response.data,
    setStatus() {},
    log() {},
    delay: async () => {},
    $: () => ({ textContent: '', hidden: false }),
  };
  const send = new Function('env', `with (env) { return (async ${sourceOf('sendRender')}); }`)(env);
  const request = { controller: new AbortController() };
  env.activeRender = request;
  assert.equal((await send('/api/render', {}, request, 'preview')).status, 200);
  assert.equal(request.distorted, true);
  const quick = { controller: new AbortController() };
  replies.push({ status: 200, headers: { get: () => null }, data: { ok: true } });
  env.activeRender = quick;
  await send('/api/render', {}, quick, 'preview');
  assert.equal(quick.distorted, undefined);
});

test('a failed re-render keeps the model with nothing over it; a first render keeps its card', async () => {
  const replies = {
    'Render failed': () => renderReply({ ok: false, error: 'boom' }, 500),
    'Render timed out': (request) => { request.timedOut = true; throw abortError(); },
  };
  for (const [status, reply] of Object.entries(replies)) {
    const again = renderPage({ mesh: true, reply });
    await again.render();
    assert.equal(again.count('showRenderFailure'), 0, status);
    assert.equal(again.count('clearGeometry'), 0, status);
    assert.equal(again.count('clearMesh'), 0, status);
    assert.equal(again.hasMesh, true, status);
    assert.ok(again.calls.some((call) => call.name === 'setStatus' && call.args[0] === status), status);

    const first = renderPage({ mesh: false, reply });
    await first.render();
    assert.equal(first.count('showRenderFailure'), 1, status);
    assert.equal(first.count('clearGeometry'), 0, status);
  }
  const page = renderPage({ mesh: false, reply: replies['Render failed'] });
  await page.render();
  assert.deepEqual(page.calls.find((call) => call.name === 'showRenderFailure').args, ['boom', 'preview']);
});

test('the model is drawn out of date only with no render on its way', () => {
  const show = (geometryStale, activeRender, renderDebounce) => {
    let stale = null;
    new Function('geometryStale', 'activeRender', 'renderDebounce', 'renderCanvas', `(${sourceOf('showGeometryStaleness')})();`)(
      geometryStale, activeRender, renderDebounce, { classList: { toggle: (_, on) => { stale = on; } } },
    );
    return stale;
  };
  assert.equal(show(true, null, null), true, 'failed or cancelled: marked');
  assert.equal(show(true, {}, null), false, 'rendering: not dimmed');
  assert.equal(show(true, null, 7), false, 'about to render: not dimmed');
  assert.equal(show(false, null, null), false);
});

test('an agent update does not dim the model before its render starts', () => {
  const apply = sourceOf('applyWorkspace');
  assert.ok(apply.indexOf('scheduleSourceRender()') < apply.indexOf('markGeometryStale()'));
});

test('the expectation belongs to the model on screen', () => {
  const env = {
    expectedRenderMs: 1200,
    hasMesh: true,
    emptyState: { hidden: true },
    meshViewport: { clearMesh() {} },
    resetIntersectionResults() {},
    markGeometryCurrent() {},
    renderObjectList() {},
    hideRenderFailure() {},
  };
  new Function('env', `with (env) { return (${sourceOf('clearGeometry')}); }`)(env)();
  assert.equal(env.expectedRenderMs, null);
  assert.equal(env.hasMesh, false);
});

test('the box sits at the bottom, clear of the viewport controls, with the same buttons', () => {
  assert.match(html, /class="rerender-bar" id="rerenderBar" hidden/);
  assert.match(html, /<div class="rerender-progress" id="rerenderProgress" role="progressbar" aria-label="Re-rendering" aria-valuemin="0" aria-valuemax="100"><i><\/i><\/div>/);
  assert.match(html, /<button type="button" class="overlay-cancel" id="rerenderCancel">Cancel<\/button>/);
  assert.match(html, /<button type="button" class="overlay-cancel" id="rerenderReport">Report an issue<\/button>/);
  // Never spanning the viewport, so the model around it stays under the pointer.
  const box = css.match(/\.rerender-bar \{[^}]*\}/)[0];
  assert.match(box, /bottom: 12px;/);
  assert.match(box, /width: min\(480px, calc\(100% - 216px\)\);/);
  assert.doesNotMatch(box, /pointer-events|inset: 0|backdrop-filter/);
  assert.match(css, /@container viewer \(max-width: 460px\) \{\s*\.rerender-bar \{ left: 12px; right: 52px; bottom: 44px;/);
  // A fading box cannot be clicked.
  assert.match(css, /\.rerender-bar\.finishing \{[^}]*pointer-events: none;/);
  assert.match(css, /prefers-reduced-motion: reduce\) \{\s*\.rerender-bar, \.rerender-progress i \{ transition: none; \}\s*\.rerender-bar\.indeterminate i \{ animation: none;/);
  assert.match(css, /\.render-canvas\.stale \{/);
});

test('with no render buttons an edit asks for a render itself, and any render covers it', () => {
  assert.match(sourceOf('syncEditor'), /scheduleWorkspaceSave\(\);[\s\S]*scheduleSourceRender\(EDIT_RENDER_DELAY_MS\);/);
  assert.match(javascript, /editor\.addEventListener\('input', \(\) => \{[\s\S]{0,120}syncEditor\(\);/);
  assert.match(sourceOf('render'), /^async function render|^function render/);
  const rendering = sourceOf('render');
  assert.ok(rendering.indexOf('clearTimeout(renderDebounce)') < rendering.indexOf('activeRender = request'));
  assert.doesNotMatch(html, /id="(previewButton|renderButton|stopButton)"/);
  assert.doesNotMatch(javascript, /#(previewButton|renderButton|stopButton)/);
  // F5 and F6 still work, and the empty state's button recovers a failed first render.
  assert.match(javascript, /event\.key === 'F5'/);
  assert.match(html, /id="firstPreview"/);
});

test('the debounced render waits for the given time; empty source just clears', () => {
  const timers = [];
  const calls = [];
  const schedule = (code) => new Function('editor', 'setTimeout', 'clearTimeout', 'render', 'clearGeometry', 'activeRender', 'cancelRender', `
    let renderDebounce = null;
    return (${sourceOf('scheduleSourceRender')});
  `)({ value: code }, (fn, ms) => { timers.push(ms); return 1; }, () => {}, () => calls.push('render'), () => calls.push('clear'), null, () => {});
  schedule('cube(1);')(600);
  schedule('cube(1);')();
  assert.deepEqual(timers, [600, 60]);
  schedule('  ')(600);
  assert.deepEqual(calls, ['clear']);
});

// --- Open issues counter --------------------------------------------------------

function issuePage({ stored = null, reply = null, now = 1_000_000_000 } = {}) {
  const store = new Map(stored === null ? [] : [['reopenscad.openIssues', stored]]);
  const fetched = [];
  const attributes = {};
  const classes = new Set();
  const badge = { hidden: true, textContent: '', classList: { toggle: (c, on) => (on ? classes.add(c) : classes.delete(c)) }, closest: () => ({ setAttribute: (k, v) => { attributes[k] = v; }, removeAttribute: (k) => { delete attributes[k]; } }) };
  const names = ['parseIssueCount', 'showIssueCount', 'storeIssueCount', 'refreshIssueCount'];
  const refresh = new Function('env', `
    const { $$, localStorage, fetch, Date, AbortSignal } = env;
    const ISSUE_COUNT_KEY = 'reopenscad.openIssues';
    const ISSUE_COUNT_TTL_MS = 600000;
    const ISSUE_COUNT_URL = 'https://api.github.com/search';
    ${names.map(sourceOf).join('\n').replace('function refreshIssueCount', 'async function refreshIssueCount')}
    return refreshIssueCount;
  `)({
    $$: () => [badge],
    localStorage: { getItem: (key) => store.get(key) ?? null, setItem: (key, value) => store.set(key, value) },
    fetch: async (url, init) => { fetched.push([url, init]); if (reply instanceof Error) throw reply; return reply; },
    Date: { now: () => now },
    AbortSignal: { timeout: () => null },
  });
  return { refresh, store, fetched, badge, attributes, classes };
}

const ok = (total) => ({ ok: true, json: async () => ({ total_count: total }) });

test('an open count shows in red and in the accessible name', async () => {
  const page = issuePage({ reply: ok(3) });
  await page.refresh();
  assert.equal(page.badge.hidden, false);
  assert.equal(page.badge.textContent, '(3)');
  assert.ok(page.classes.has('open'));
  assert.equal(page.attributes['aria-label'], 'Report issues, 3 open');
  assert.equal(JSON.parse(page.store.get('reopenscad.openIssues')).count, 3);
});

test('zero shows in the normal colour', async () => {
  const page = issuePage({ reply: ok(0) });
  await page.refresh();
  assert.equal(page.badge.textContent, '(0)');
  assert.ok(!page.classes.has('open'));
  assert.equal(page.attributes['aria-label'], 'Report issues, 0 open');
});

test('a fresh count is not fetched again, in this tab or another', async () => {
  const page = issuePage({ stored: JSON.stringify({ count: 2, checkedAt: 1_000_000_000 - 60000 }), reply: ok(9) });
  await page.refresh();
  assert.equal(page.fetched.length, 0);
  assert.equal(page.badge.textContent, '(2)');
});

test('failure keeps the last count, or shows none; nothing retries', async () => {
  const stale = JSON.stringify({ count: 2, checkedAt: 1 });
  for (const reply of [new Error('offline'), { ok: false, status: 403, json: async () => ({}) }, { ok: true, json: async () => ({ message: 'odd' }) }]) {
    const kept = issuePage({ stored: stale, reply });
    await kept.refresh();
    assert.equal(kept.badge.textContent, '(2)');
    assert.equal(kept.fetched.length, 1);
    // The attempt is on record, so a reload does not try again for ten minutes.
    assert.equal(JSON.parse(kept.store.get('reopenscad.openIssues')).checkedAt, 1_000_000_000);
    await kept.refresh();
    assert.equal(kept.fetched.length, 1);

    const none = issuePage({ reply });
    await none.refresh();
    assert.equal(none.badge.hidden, true);
    assert.equal(none.attributes['aria-label'], undefined);
  }
  const corrupt = issuePage({ stored: 'not json', reply: new Error('offline') });
  await corrupt.refresh();
  assert.equal(corrupt.badge.hidden, true);
});

test('the count comes from search, sends no credentials or referrer, and never holds up the load', () => {
  assert.match(javascript, /api\.github\.com\/search\/issues\?q=repo:Matvey-Kuk\/reopenscad\+type:issue\+state:open/);
  assert.doesNotMatch(sourceOf('refreshIssueCount'), /open_issues_count/);
  assert.match(sourceOf('refreshIssueCount'), /credentials: 'omit', referrerPolicy: 'no-referrer'/);
  assert.match(sourceOf('bootstrap'), /\n  refreshIssueCount\(\);/);
  assert.equal((html.match(/Report issues<span class="issue-count" data-issue-count hidden><\/span>/g) || []).length, 2);
  assert.match(css, /\.issue-count\.open \{ color: var\(--red\); \}/);
});

test('a PNG export waits for the render an edit has asked for', () => {
  const start = javascript.indexOf("$('#exportPng').addEventListener");
  assert.ok(start >= 0);
  assert.match(javascript.slice(start, start + 340), /\(!hasMesh \|\| geometryStale \|\| renderDebounce !== null \|\| activeRender\) && !\(await render\('render'\)\)/);
});
