import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const html = await readFile(new URL('../index.html', import.meta.url), 'utf8');
const javascript = await readFile(new URL('../app.js', import.meta.url), 'utf8');
const css = await readFile(new URL('../styles.css', import.meta.url), 'utf8');

// Pure helpers are lifted out of the shipped script and run, so the sync rules
// are checked by behaviour rather than by spelling.
function loadFunction(name, scope = {}) {
  const start = javascript.indexOf(`function ${name}(`);
  assert.ok(start >= 0, `${name} is defined`);
  let depth = 0;
  let index = javascript.indexOf('{', javascript.indexOf(')', start));
  for (; index < javascript.length; index += 1) {
    if (javascript[index] === '{') depth += 1;
    if (javascript[index] === '}' && --depth === 0) break;
  }
  const prefix = javascript.slice(start - 6, start) === 'async ' ? 'async ' : '';
  const source = prefix + javascript.slice(start, index + 1);
  return new Function(...Object.keys(scope), `${source}; return ${name};`)(...Object.values(scope));
}

test('the server copy wins on open unless the cache holds edits against that same revision', () => {
  const reconcile = loadFunction('reconcileCachedWorkspace');
  const remote = { code: 'cube(2);', revision: 7 };
  assert.deepEqual(reconcile(null, remote), { action: 'remote' });
  assert.deepEqual(reconcile({ code: 'cube(2);', revision: 3, dirty: true }, remote), { action: 'remote' });
  // A clean copy of an older revision is just stale: it used to win the editor and
  // show up as a conflict after an agent edited the workspace in the background.
  assert.deepEqual(reconcile({ code: 'cube(1);', revision: 6, dirty: false }, remote), { action: 'remote' });
  // Unsaved edits made against the revision the server still has carry on.
  assert.deepEqual(reconcile({ code: 'cube(9);', revision: 7, dirty: true }, remote), { action: 'resume-local' });
  // Unsaved edits against any other revision lose the editor but are kept.
  assert.deepEqual(reconcile({ code: 'cube(9);', revision: 6, dirty: true }, remote), { action: 'remote', displaced: 'cube(9);' });
  assert.deepEqual(reconcile({ code: 'cube(9);', revision: 9, dirty: true }, remote), { action: 'remote', displaced: 'cube(9);' });
  // Entries written before the flag existed mirrored the editor.
  assert.deepEqual(reconcile({ code: 'cube(9);', revision: 7 }, remote), { action: 'resume-local' });
  // ...and one behind the server is a stale clean copy, not edits to set aside.
  assert.deepEqual(reconcile({ code: 'cube(9);', revision: 5 }, remote), { action: 'remote' });
});

test('a poll applies newer revisions and resyncs when the server went backwards', () => {
  const action = loadFunction('workspacePollAction');
  assert.equal(action({ changed: true, revision: 5, code: 'a' }, 4, 4), 'apply');
  // Our own save landed while the long poll was parked: nothing to apply.
  assert.equal(action({ changed: true, revision: 5, code: 'a' }, 4, 5), 'none');
  assert.equal(action({ changed: false, revision: 4 }, 4, 4), 'none');
  // `changed` can never become true again once the server is behind `since`.
  assert.equal(action({ changed: false, revision: 2 }, 4, 4), 'resync');
  assert.equal(action({ ok: false }, 4, 4), 'none');
});

test('the cache records whether it holds unsaved edits', () => {
  const storing = javascript.slice(javascript.indexOf('function storeLocalWorkspace()'), javascript.indexOf('function readCachedWorkspace'));
  assert.match(storing, /dirty: editor\.value !== workspaceSavedCode/);
});

test('the event poll is a single loop, refreshed whenever the page comes back', () => {
  const polling = javascript.slice(javascript.indexOf('function beginWorkspacePolling'), javascript.indexOf('function normalizeObjects'));
  assert.match(polling, /eventPollController\?\.abort\(\)/);
  assert.match(polling, /const token = \+\+pollLoopToken/);
  assert.match(polling, /cache: 'no-store'/);
  assert.match(javascript, /addEventListener\('visibilitychange', \(\) => \{\s*if \(!document\.hidden\) refreshWorkspaceFromServer/);
  assert.match(javascript, /addEventListener\('pageshow', \(event\) => \{\s*if \(!event\.persisted\) return;/);
  assert.match(javascript, /addEventListener\('focus', \(\) => refreshWorkspaceFromServer\(\)\)/);
  assert.match(javascript, /addEventListener\('online', \(\) => refreshWorkspaceFromServer/);
  // A save rejected because a newer revision exists hands the editor to that revision.
  const saving = javascript.slice(javascript.indexOf('async function saveWorkspace'), javascript.indexOf('async function flushWorkspaceSave'));
  // ...after fetching what changed in it, so the AI highlight is not skipped.
  assert.match(saving, /response\.status === 409\) \{[\s\S]*?await fetchWorkspaceChanges\(id, baseRevision, controller\.signal\)[\s\S]*?applyWorkspace\(winner, \{ remote: true \}\)/);
  // A save reply that predates a revision applied meanwhile cannot rewind it.
  assert.match(saving, /if \(workspaceRevision !== baseRevision\)/);
});

test('"/" is a landing page and only its button creates a workspace', () => {
  assert.match(html, /<main class="landing" id="landing"/);
  assert.match(html, /id="landingNew"/);
  assert.match(html, /id="landingRecents"/);
  // The hero leads to per-agent walkthroughs, not a raw endpoint to copy.
  assert.doesNotMatch(html, /id="landingMcpEndpoint"/);
  assert.match(javascript, /\$\('#landingAgents'\)\.addEventListener\('click'[\s\S]{0,160}openMcpPanel\(agent\.dataset\.agent\)/);
  // Decided before first paint so the editor shell never flashes on "/".
  assert.match(html, /document\.documentElement\.classList\.add\('landing-mode'\)/);
  assert.match(css, /html\.landing-mode \.app-shell \{ display: none; \}/);
  const routing = javascript.slice(javascript.indexOf('async function openWorkspaceRoute()'), javascript.indexOf("$('#landingNew')"));
  assert.doesNotMatch(routing, /createWorkspace\(/);
  assert.match(routing, /showLanding\(\);/);
  // Every workspace creation sits behind a user action: the landing button, the
  // toolbar's New, an example card, the legacy-draft and missing-workspace
  // recoveries. Fork copies server side through the shared clone route.
  assert.match(javascript, /\$\('#landingNew'\)\.addEventListener\('click'[\s\S]{0,200}startNewWorkspace\(''\)/);
  const creators = [...javascript.matchAll(/await (?:createWorkspace|startNewWorkspace)\(/g)].length;
  assert.equal(creators, 5);
  assert.match(javascript, /\$\('#forkWorkspace'\)\.addEventListener\('click'[\s\S]{0,400}sharedCloneRequest\(token, code\)/);
});

test('the landing page shows designs created today, this week and in total', () => {
  // A trailing sentence of the hero subhead, not a separate line.
  assert.match(html, /<div class="landing-pitch">[\s\S]*?<p>AI agents[^<]*<span class="landing-stats" id="landingStats" hidden> <b id="landingStatsDay"><\/b>[\s\S]*?in total\.<\/span><\/p>/);
  for (const id of ['landingStatsDay', 'landingStatsWeek', 'landingStatsTotal']) assert.match(html, new RegExp(`id="${id}"`));
  const showing = javascript.slice(javascript.indexOf('function showLanding('), javascript.indexOf('function hideLanding('));
  assert.match(showing, /loadLandingStats\(\);/);
  assert.match(showing, /fetch\('\/api\/stats'/);
  assert.match(showing, /designsCreatedLast24h/);
  assert.match(showing, /designsCreatedLast7d/);
  // A failed or malformed reply hides the line instead of printing NaN.
  assert.match(showing, /catch \{\s*if \(token === landingStatsToken\) line\.hidden = true;/);
  // A slow earlier request cannot overwrite what a newer one showed.
  assert.match(showing, /if \(token !== landingStatsToken\) return;/);
  assert.match(css, /\.landing-stats\[hidden\], \.landing-stats \[hidden\] \{ display: none; \}/);
  // An older server without the weekly count still shows today and total.
  assert.match(showing, /week !== undefined && !valid\(week\)/);
});

test('the header brand leads back to the landing page', () => {
  assert.match(html, /<a class="brand-home" id="brandHome" href="\/"/);
  const handler = javascript.slice(javascript.indexOf("$('#brandHome').addEventListener"), javascript.indexOf("$('#landingNew').addEventListener"));
  assert.match(handler, /await leaveCurrentWorkspace\(\);\s*history\.pushState\(null, '', '\/'\);\s*await openWorkspaceRoute\(\);/);
});

test('every export format is offered per object, and the workspace export names its parts', () => {
  const formats = javascript.slice(javascript.indexOf('const EXPORT_FORMATS = ['), javascript.indexOf('function exportableObjects'));
  for (const format of ['stl', '3mf', 'step', 'obj', 'off', 'glb']) {
    assert.match(formats, new RegExp(`id: '${format}'`));
    assert.match(html, new RegExp(`data-format="${format}"`));
  }
  assert.match(html, /id="objectExportPopover"/);
  assert.match(javascript, /class="object-export"/);
  assert.match(javascript, /exportModel\(button\.dataset\.objectFormat, \{ objectId \}\)/);
  const exportIds = loadFunction('exportObjectIds', { selectedObjectId: 'object-2' });
  // The workspace export names nothing: one file, every object a separate body.
  assert.equal(exportIds('all'), null);
  assert.deepEqual(exportIds({ objectId: 'object-1' }), ['object-1']);
  assert.deepEqual(exportIds('selected'), ['object-2']);
});

test('color() reaches the viewport, and selection tints rather than repaints', () => {
  const material = loadFunction('meshMaterial', {
    intersectionIds: new Set(['object-3']),
    selectedObjectId: 'object-2',
    sceneObjects: [{ id: 'object-1' }, { id: 'object-2' }, { id: 'object-3' }],
    DEFAULT_MESH_COLOR: [0.98, 0.77, 0.19],
    BACKGROUND_MESH_COLOR: [0.72, 0.74, 0.78],
    SELECTION_HIGHLIGHT: [0.62, 0.88, 1.0, 0.55],
    INTERSECTION_HIGHLIGHT: [1.0, 0.16, 0.1, 0.9],
  });
  assert.deepEqual(material({ color: [1, 0, 0, 1] }, 'object-1'), { color: [1, 0, 0], alpha: 1, highlight: null });
  assert.deepEqual(material({ color: null }, 'object-1').color, [0.98, 0.77, 0.19]);
  const selected = material({ color: [0, 0.5, 0, 0.5] }, 'object-2');
  assert.deepEqual(selected.color, [0, 0.5, 0]);
  assert.equal(selected.alpha, 0.5);
  assert.ok(selected.highlight);
  assert.ok(material({ background: true }, 'object-1').alpha < 1);
  // A part flagged by the intersection check keeps the colour the user gave it and
  // is tinted instead; repainting it red made two flagged parts indistinguishable.
  const flagged = material({ color: [0, 0, 1, 1] }, 'object-3');
  assert.deepEqual(flagged.color, [0, 0, 1]);
  assert.deepEqual(flagged.highlight, [1.0, 0.16, 0.1, 0.9]);
  assert.match(javascript, /uniform vec4 uHighlight;/);
  assert.match(javascript, /gl\.blendFuncSeparate\(gl\.SRC_ALPHA, gl\.ONE_MINUS_SRC_ALPHA, gl\.ONE, gl\.ONE_MINUS_SRC_ALPHA\)/);
});

// Like loadFunction, but every free variable resolves on `env`, so stubs can
// mutate shared state (the generation counter, the editor) the way the page does.
function loadWith(name, env) {
  const start = javascript.indexOf(`function ${name}(`);
  assert.ok(start >= 0, `${name} is defined`);
  let depth = 0;
  let index = javascript.indexOf('{', javascript.indexOf(')', start));
  for (; index < javascript.length; index += 1) {
    if (javascript[index] === '{') depth += 1;
    if (javascript[index] === '}' && --depth === 0) break;
  }
  const prefix = javascript.slice(start - 6, start) === 'async ' ? 'async ' : '';
  const source = prefix + javascript.slice(start, index + 1);
  return new Function('env', `with (env) { return (${source}); }`)(env);
}

test('per-object export ids are valid only for the source they were compiled from', () => {
  const env = { sceneObjectsSource: 'cube(1);', sceneObjects: [{ id: 'a' }], editor: { value: 'cube(1);' } };
  const current = loadWith('sceneObjectsCurrent', env);
  assert.equal(current(), true);
  env.editor.value = 'cube(1); sphere(2);';
  assert.equal(current(), false);
  env.editor.value = 'cube(1);';
  env.sceneObjects = [{ id: 'model', placeholder: true }];
  assert.equal(current(), false);
  env.sceneObjectsSource = null;
  env.sceneObjects = [{ id: 'a' }];
  assert.equal(current(), false);
});

test('a source change from the server re-renders, and empty source clears the geometry', () => {
  const calls = [];
  const env = {
    renderDebounce: null,
    activeRender: { id: 'r1' },
    editor: { value: '' },
    clearTimeout: () => {},
    setTimeout: (callback) => { calls.push('timer'); callback(); return 1; },
    cancelRender: () => calls.push('cancel'),
    clearGeometry: () => calls.push('clear'),
    render: (mode) => calls.push(`render:${mode}`),
  };
  const schedule = loadWith('scheduleSourceRender', env);
  schedule();
  assert.deepEqual(calls, ['cancel', 'clear']);
  calls.length = 0;
  env.editor.value = 'cube(1);';
  schedule();
  assert.deepEqual(calls, ['timer', 'render:preview']);
  const applying = javascript.slice(javascript.indexOf('function applyWorkspace'), javascript.indexOf('function byteOffsetToStringIndex'));
  assert.match(applying, /if \(remote && sourceChanged && rerender\) scheduleSourceRender\(\);/);
  // ...and until that render lands, the old model is marked as not current.
  assert.match(applying, /if \(sourceChanged && hasMesh\) markGeometryStale\(\);/);
});

test('a save conflict reads the change ranges for the winning revision', async () => {
  const requested = [];
  const payload = { changed: true, revision: 9, code: 'cube(2);', updatedRanges: [{ revision: 9, source: 'ai', start: 0, insertedLength: 8 }] };
  const env = {
    fetch: async (url) => { requested.push(url); return { ok: true, status: 200, json: async () => payload }; },
    AbortSignal,
  };
  const fetchChanges = loadWith('fetchWorkspaceChanges', env);
  assert.deepEqual(await fetchChanges('ws-1', 7, new AbortController().signal), payload);
  assert.deepEqual(requested, ['/api/designs/ws-1/events?since=7']);
  env.fetch = async () => ({ ok: true, status: 200, json: async () => ({ changed: false, revision: 7 }) });
  assert.equal(await fetchChanges('ws-1', 7, new AbortController().signal), null);
  env.fetch = async () => { throw new Error('offline'); };
  assert.equal(await fetchChanges('ws-1', 7, new AbortController().signal), null);
});

test('a shared view answer that lands after navigation is dropped', async () => {
  let resolveFetch;
  const element = () => ({ hidden: false, textContent: '', classList: { add() {} } });
  const elements = {};
  const env = {
    readOnlyToken: null,
    workspaceGeneration: 0,
    workspaceId: 'previous',
    workspaceOnline: true,
    editor: { value: 'other workspace', readOnly: false },
    document: { body: { classList: { add() {} } } },
    $: (selector) => (elements[selector] ??= element()),
    setSyncState: () => {},
    syncEditor: () => {},
    applyWorkspaceSettings: () => {},
    workspaceLoadSignal: () => new AbortController().signal,
    log: (message) => env.logged.push(message),
    logged: [],
    setStatus: () => {},
    fetch: () => new Promise((resolve) => { resolveFetch = resolve; }),
  };
  env.advanceWorkspaceGeneration = () => (env.workspaceGeneration += 1);
  env.readOnlyRevision = 0;
  env.hasMesh = false;
  env.markGeometryStale = () => {};
  env.applySharedPayload = loadWith('applySharedPayload', env);
  const loadShared = loadWith('loadSharedWorkspace', env);
  const pending = loadShared('a'.repeat(32));
  // The user navigates to another workspace while the request is out.
  env.workspaceGeneration += 1;
  env.readOnlyToken = null;
  env.editor.value = 'cube(5);';
  resolveFetch({ ok: true, json: async () => ({ code: 'sphere(1);' }) });
  assert.equal(await pending, false);
  assert.equal(env.editor.value, 'cube(5);');
  // A late failure is not reported against the workspace now on screen either.
  const failing = loadShared('b'.repeat(32));
  env.workspaceGeneration += 1;
  resolveFetch({ ok: false, status: 404, json: async () => ({}) });
  assert.equal(await failing, false);
  assert.deepEqual(env.logged, []);
  // ...while a current answer is applied.
  const current = loadShared('c'.repeat(32));
  resolveFetch({ ok: true, json: async () => ({ code: 'sphere(1);' }) });
  assert.equal(await current, true);
  assert.equal(env.editor.value, 'sphere(1);');
});

test('a slow shared reload never rewinds the source of a newer render', async () => {
  const pending = [];
  const env = {
    readOnlyToken: 't', readOnlyRevision: 1, hasMesh: true,
    editor: { value: 'cube(1);' }, stale: 0,
    markGeometryStale: () => { env.stale += 1; },
    syncEditor: () => {}, applyWorkspaceSettings: () => {}, refreshRevisions: () => {},
    fetch: () => new Promise((resolve) => pending.push(resolve)),
  };
  env.applySharedPayload = loadWith('applySharedPayload', env);
  const reload = loadWith('reloadSharedSource', env);
  let oldSuperseded = false;
  const request = { controller: new AbortController() };
  const old = reload('t', request, () => oldSuperseded);
  const newer = reload('t', request, () => false);
  pending[1]({ ok: true, json: async () => ({ revision: 3, code: 'cube(3);' }) });
  assert.equal(await newer, true);
  oldSuperseded = true;
  pending[0]({ ok: true, json: async () => ({ revision: 2, code: 'cube(2);' }) });
  assert.equal(await old, false);
  assert.equal(env.editor.value, 'cube(3);');
  assert.equal(env.readOnlyRevision, 3);
  // The model on screen was compiled from the replaced source.
  assert.equal(env.stale, 1);
});

test('leaving a workspace aborts its in-flight loads', () => {
  const advancing = javascript.slice(javascript.indexOf('function advanceWorkspaceGeneration()'), javascript.indexOf('function hasPendingLocalWorkspaceChanges'));
  assert.match(advancing, /workspaceLoadController\?\.abort\(\)/);
  for (const loader of ['async function loadWorkspace', 'async function createWorkspace', 'async function refreshWorkspaceFromServer', 'async function loadSharedWorkspace', 'async function checkIntersections']) {
    const start = javascript.indexOf(loader);
    const body = javascript.slice(start, javascript.indexOf('\n}\n', start));
    assert.match(body, /workspaceLoadSignal\(\)/, loader);
  }
  const routing = javascript.slice(javascript.indexOf('async function openWorkspaceRoute()'), javascript.indexOf("$('#brandHome')"));
  assert.match(routing, /if \(!\(await loadSharedWorkspace\(sharedToken\)\)\) return false;/);
});

// Workspaces were renamed to designs. Every key and route from before the
// rename has to keep working, or an upgrade silently loses someone's list.
function memoryStorage(entries = {}) {
  const map = new Map(Object.entries(entries));
  return {
    map,
    get length() { return map.size; },
    key: (index) => [...map.keys()][index] ?? null,
    getItem: (key) => (map.has(key) ? map.get(key) : null),
    setItem: (key, value) => map.set(key, String(value)),
    removeItem: (key) => map.delete(key),
  };
}

const storageScope = {
  RECENTS_STORAGE_KEY: 'reopenscad.recentDesigns',
  DESIGN_CACHE_PREFIX: 'reopenscad.design.',
  LEGACY_RECENTS_STORAGE_KEY: 'reopenscad.recentWorkspaces',
  LEGACY_CACHE_PREFIX: 'reopenscad.workspace.',
};

test('pre-rename recents and source caches move to the design keys without loss', () => {
  for (const [name, value] of Object.entries(storageScope)) {
    assert.match(javascript, new RegExp(`const ${name} = ${typeof value === 'string' ? `'${value.replace(/\./g, '\\.')}'` : value};`));
  }
  const legacyCacheWins = loadFunction('legacyCacheWins');
  const migrate = loadFunction('migrateLegacyStorage', { ...storageScope, legacyCacheWins, localStorage: null });
  const cached = JSON.stringify({ code: 'cube(1);', revision: 3, dirty: true });
  const storage = memoryStorage({
    'reopenscad.recentWorkspaces': JSON.stringify([
      { id: 'old-otter-1', visitedAt: 100 },
      { id: 'shared-heron-2', visitedAt: 50, empty: true },
    ]),
    'reopenscad.recentDesigns': JSON.stringify([{ id: 'shared-heron-2', visitedAt: 300 }]),
    'reopenscad.workspace.old-otter-1': cached,
    'reopenscad.workspace.shared-heron-2': 'stale',
    'reopenscad.design.shared-heron-2': 'kept',
    'reopenscad.plate': 'bambu-x1',
  });
  migrate(storage);
  const recents = JSON.parse(storage.getItem('reopenscad.recentDesigns'));
  assert.deepEqual(recents.map((entry) => entry.id), ['shared-heron-2', 'old-otter-1']);
  assert.equal(recents[0].visitedAt, 300, 'the newer design-key entry wins a duplicate');
  assert.equal(storage.getItem('reopenscad.recentWorkspaces'), null);
  assert.equal(storage.getItem('reopenscad.design.old-otter-1'), cached);
  assert.equal(storage.getItem('reopenscad.design.shared-heron-2'), 'kept', 'an existing design cache is never overwritten');
  assert.equal(storage.getItem('reopenscad.workspace.old-otter-1'), null);
  assert.equal(storage.getItem('reopenscad.plate'), 'bambu-x1');
  // A stale pre-rename tab's unsaved edits win over a clean newer-key copy;
  // a corrupt new recents list does not cost the old one.
  const dirty = JSON.stringify({ code: 'cube(9);', revision: 3, dirty: true });
  const clean = JSON.stringify({ code: 'cube(3);', revision: 3, dirty: false });
  const racing = memoryStorage({
    'reopenscad.workspace.a': dirty,
    'reopenscad.design.a': clean,
    'reopenscad.recentDesigns': '{corrupt',
    'reopenscad.recentWorkspaces': JSON.stringify([{ id: 'a', visitedAt: 1 }]),
  });
  migrate(racing);
  assert.equal(racing.getItem('reopenscad.design.a'), dirty);
  assert.deepEqual(JSON.parse(racing.getItem('reopenscad.recentDesigns')).map((entry) => entry.id), ['a']);
  // Idempotent, and harmless on a browser that never had the old keys.
  migrate(storage);
  assert.deepEqual(JSON.parse(storage.getItem('reopenscad.recentDesigns')), recents);
  const fresh = memoryStorage();
  migrate(fresh);
  assert.equal(fresh.length, 0);
  // Blocked storage must not throw out of the boot path.
  migrate({ getItem: () => { throw new Error('blocked'); } });
  // It runs before anything reads the recents.
  assert.ok(javascript.indexOf('migrateLegacyStorage();') < javascript.indexOf('function readRecents'));
  // Readers still fall back to the old keys if the move could not happen.
  assert.match(javascript, /localStorage\.getItem\(RECENTS_STORAGE_KEY\) \?\? localStorage\.getItem\(LEGACY_RECENTS_STORAGE_KEY\)/);
  assert.match(javascript, /\?\? localStorage\.getItem\(`\$\{LEGACY_CACHE_PREFIX\}\$\{id \|\| 'draft'\}`\)/);
});

test('designs live at /designs/<id>, and pre-rename links open the same design', () => {
  const resolve = (pathname, search = '') => loadFunction('workspaceIdFromLocation', { location: { pathname, search } })();
  assert.equal(resolve('/designs/brisk-otter-1'), 'brisk-otter-1');
  assert.equal(resolve('/workspaces/brisk-otter-1'), 'brisk-otter-1');
  assert.equal(resolve('/workspaces/brisk-otter-1/'), 'brisk-otter-1');
  assert.equal(resolve('/', '?workspace=brisk-otter-1'), 'brisk-otter-1');
  assert.equal(resolve('/', '?design=brisk-otter-1'), 'brisk-otter-1');
  assert.equal(resolve('/'), null);
  // Loading one rewrites the address to the canonical route.
  assert.match(javascript, /const expectedPath = `\/designs\/\$\{encodeURIComponent\(id\)\}`;/);
  assert.doesNotMatch(javascript, /\/api\/workspaces|`\/workspaces\//);
  // The landing guard in the shell treats ?design= like ?workspace=.
  assert.match(html, /has\('design'\)/);
  // Deprecated MCP alias tools are served but not advertised in the panel.
  assert.match(javascript, /\.filter\(\(tool\) => !String\(tool\.description \|\| ''\)\.startsWith\('Deprecated alias'\)\)/);
});
