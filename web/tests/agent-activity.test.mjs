import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { statementOf } from './source.mjs';

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

test('every agent effect holds for ten seconds, then fades out', () => {
  const level = loadWith('agentFadeLevel', { AGENT_HOLD_MS: 10000, AGENT_FADE_MS: 2500, performance: { now: () => 0 } });
  assert.equal(level(1000, 1000), 1);
  assert.equal(level(0, 10000), 1);
  assert.equal(level(0, 11250), 0.5);
  assert.equal(level(0, 12500), 0);
  assert.equal(level(0, 60000), 0);
  // Nothing to count from (a read still running, a kept variant): it holds.
  assert.equal(level(null, 60000), 1);
});

test('history from before the page loaded is listed, never acted out', () => {
  const acted = [];
  const agentActivity = { design: 'd', version: 0, loaded: false, entries: new Map(), quietSurfaces: new Set() };
  const env = {
    agentActivity,
    AGENT_FEED_LIMIT: 60,
    resetAgentActivity: (id) => Object.assign(agentActivity, { design: id, version: 0, loaded: false, entries: new Map(), openedAt: 0 }),
    actOutAgentEntry: (entry, previous) => acted.push([entry.seq, previous?.state ?? null]),
    renderAgentFeed: () => {},
    renderCompareControls: () => {},
    agentComparison: () => false,
    endAgentSurfaceSelection: () => {},
  };
  const apply = loadWith('applyActivitySummary', env);
  agentActivity.openedAt = 10000;
  apply({ version: 5, entries: [{ seq: 1, state: 'done', startedAt: 2000 }, { seq: 2, state: 'done', startedAt: 9500 }, { seq: 3, state: 'running', startedAt: 1000 }] }, 'd');
  // Old finished calls are history; a running one, and one started as the page opened, are live.
  assert.deepEqual(acted, [[2, null], [3, null]]);
  acted.length = 0;
  assert.equal(agentActivity.entries.size, 3);
  apply({ version: 9, entries: [{ seq: 3, state: 'done' }, { seq: 7, state: 'done' }] }, 'd');
  assert.deepEqual(acted, [[3, 'running'], [7, null]]);
  assert.equal(agentActivity.version, 9);
  // Another design's answer starts that design's log over.
  apply({ version: 2, entries: [{ seq: 8, state: 'done' }] }, 'other');
  assert.equal(agentActivity.design, 'other');
  assert.deepEqual([...agentActivity.entries.keys()], [8]);
});

test('only http(s) and same-origin paths from the server become links or images', () => {
  const safe = loadWith('agentSafeUrl');
  assert.equal(safe('/api/designs/x/activity/3/0.png'), '/api/designs/x/activity/3/0.png');
  assert.equal(safe('http://127.0.0.1:5180/shared/abc'), 'http://127.0.0.1:5180/shared/abc');
  assert.equal(safe('javascript:alert(1)'), null);
  assert.equal(safe('//evil.example/x'), null);
  assert.equal(safe(42), null);
});

test('the event poll asks for activity and one console holds every message', () => {
  assert.match(javascript, /&activity=\$\{agentActivityVersion\(id\)\}/);
  assert.match(javascript, /applyActivitySummary\(payload\.activity, id\);/);
  assert.doesNotMatch(html, /console-tabs|problemCount|agentFeed/);
  assert.match(html, /id="agentReadLayer"/);
  assert.match(javascript, /\$\('#firstPreview'\)\.addEventListener\('click', \(\) => render\('preview'\)\);/);
});

test('no ghost is left: what an agent looks at is the model on screen', () => {
  assert.doesNotMatch(javascript, /previewAgentVariant|endAgentVariant|showVariant|clearVariant|frameGhost|variantBatches|variantFade|AGENT_GHOST|AGENT_VARIANT|userRender/);
  assert.doesNotMatch(sourceOf('agentRowHtml'), /Preview/);
  assert.doesNotMatch(sourceOf('agentRowHtml'), /variant/);
});

// ---- Revisions ----

function revisionEnv(extra = {}) {
  const env = {
    readOnlyToken: null,
    revisionRestoring: false,
    escapeHtml: (value) => String(value).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;').replace(/'/g, '&#39;'),
    escapeAttribute: (value) => env.escapeHtml(value),
    ...extra,
  };
  env.revisionAuthor = loadWith('revisionAuthor', env);
  env.revisionClock = () => '11:03';
  return env;
}

test('a revision row escapes the name an agent chose and marks the current one', () => {
  const row = loadWith('revisionRowHtml', revisionEnv());
  const name = '<img src=x onerror=alert(1)>"\'&';
  const html = row({ revision: 7, name, source: 'agent', timestamp: 1, current: true });
  assert.doesNotMatch(html, /<img/);
  assert.match(html, /&lt;img src=x onerror=alert\(1\)&gt;/);
  assert.match(html, /title="&lt;img[^"]*&quot;&#39;&amp;"/);
  assert.match(html, /revision-row current by-agent/);
  assert.match(html, /aria-current="true"/);
  assert.match(html, />current</);
  assert.match(html, /#7 /, 'number and time on the one line');
  assert.doesNotMatch(html, /data-revision-restore/);
});

test('older revisions restore, read-only links only list, and authors read as agent or you', () => {
  const row = { revision: 3, name: 'Old', source: 'browser', timestamp: 1 };
  const editable = loadWith('revisionRowHtml', revisionEnv())(row);
  assert.match(editable, /data-revision-restore="3"/);
  assert.match(editable, /#3 · you · 11:03/);
  const shared = loadWith('revisionRowHtml', revisionEnv({ readOnlyToken: 't' }))(row);
  assert.doesNotMatch(shared, /data-revision-restore|Restore/);
  assert.match(shared, /#3 · browser/);
  assert.match(loadWith('revisionRowHtml', revisionEnv())({ ...row, source: 'unknown' }), /#3 · 11:03/);
  assert.match(sourceOf('restoreRevision'), /if \(!workspaceId \|\| readOnlyToken \|\| revisionRestoring\) return;/);
});

test('a restored revision is named after the old one and clipped to 40 characters', () => {
  const name = loadWith('restoredRevisionName', { REVISION_NAME_LIMIT: 40 });
  assert.equal(name('Hinge'), 'Restore: Hinge');
  const long = name('x'.repeat(40));
  assert.equal(long.length, 40);
  // By whole characters: an emoji at the cut is not split in half.
  assert.equal(Array.from(name('😀'.repeat(40))).length, 40);
  assert.ok(!name('😀'.repeat(40)).includes('\ufffd'));
});

test('the Revisions panel sits in the left pane and follows every new revision', () => {
  const pane = html.slice(html.indexOf('<aside class="objects-pane"'), html.indexOf('</aside>'));
  assert.match(pane, /id="revisionsTitle">Revisions</);
  assert.match(pane, /id="revisionList"/);
  assert.match(sourceOf('applyWorkspace'), /refreshRevisions\(\);/);
  assert.match(sourceOf('beginWorkspacePolling'), /refreshRevisions\(0\);/);
  assert.match(sourceOf('loadRevisions'), /\/api\/shared\/\$\{encodeURIComponent\(readOnlyToken\)\}\/revisions/);
  assert.match(sourceOf('restoreRevision'), /expectedRevision: workspaceRevision/);
});

// --- The surfaces panel ------------------------------------------------------

// The real applyActivitySummary and selection code, with a stand-in viewport.
function surfacePage() {
  const viewport = { shown: null, clears: 0, setAgentMeshes(meshes) { this.shown = meshes[0]; }, clearAgentMeshes() { this.shown = null; this.clears += 1; }, draw() {} };
  const fetched = [];
  const env = {
    AGENT_FEED_LIMIT: 60,
    AGENT_LAYER_LIMIT: 12,
    meshViewport: viewport,
    log: () => {},
    performance: { now: () => 0 },
    startAgentFades: () => {},
    agentFadeLevel: () => 1,
    renderAgentFeed: () => {},
    renderCompareControls: () => {},
    sceneObjects: [{ id: 'object-1', name: 'Bracket' }, { id: 'object-2', name: 'Lid' }],
    sceneObjectsCurrent: () => true,
    workspaceRevision: 3,
    comparePicks: { a: '', b: '' },
    compareError: '',
    cancelCompareRun: () => {},
    fetch: async (url) => { fetched.push(url); return { ok: !url.includes('gone'), status: 404, arrayBuffer: async () => url }; },
    agentActivity: { design: 'd', version: 0, loaded: true, openedAt: 0, entries: new Map(), layers: [], surfaces: null, surfacePick: null, lastSeq: 0, quietSurfaces: new Set(), comparisonsWaiting: [], side: null, meshToken: 0, shown: null, pending: [] },
    resetAgentActivity() { throw new Error('not expected'); },
    actOutAgentEntry(entry, previous) {
      if (entry.state === 'done' && previous?.state !== 'done') env.showAgentFindings(entry, [], env.agentSurfaceMeshes(entry), { fromPoll: true });
    },
  };
  for (const name of ['agentSurfaceMeshes', 'agentMovedOnFrom', 'agentComparison', 'comparePartId', 'comparedParts', 'comparisonListAnswered', 'settleWaitingComparisons', 'syncCompareParts', 'showSideSurfaces', 'switchOffSideSurfaces', 'agentCallFinishedAt', 'agentCallFollows', 'keepAgentSurfaces', 'endAgentSurfaceSelection', 'pickAgentSurface', 'switchOffAgentSurface', 'showAgentFindings']) {
    env[name] = new Function('env', `with (env) { return (${sourceOf(name)}); }`)(env);
  }
  const apply = new Function('env', `with (env) { return (${sourceOf('applyActivitySummary')}); }`)(env);
  let version = 0;
  const settle = () => new Promise((resolve) => { setTimeout(resolve, 0); });
  return { env, viewport, fetched, settle, poll: async (...entries) => { apply({ version: ++version, entries }, 'd'); await settle(); } };
}

const comparison = (seq, startedAt = 1000, elapsedMs = 50) => ({
  seq,
  tool: 'compare_surfaces',
  state: 'done',
  startedAt,
  elapsedMs,
  revision: 3,
  meshes: [{ url: `/m/${seq}/0`, label: 'object-1', legend: { range: 0.9157 } }, { url: `/m/${seq}/1`, label: 'object-2' }],
});

test('the call that produced a heatmap shows it, running first or arriving finished', async () => {
  const stepwise = surfacePage();
  await stepwise.poll({ seq: 5, state: 'running', startedAt: 1000 });
  assert.equal(stepwise.viewport.shown, null);
  await stepwise.poll(comparison(5));
  assert.equal(stepwise.env.agentActivity.surfacePick, 0, 'it did not switch itself off');
  assert.equal(stepwise.viewport.shown.bytes, '/m/5/0');

  const coalesced = surfacePage();
  await coalesced.poll(comparison(5));
  assert.equal(coalesced.env.agentActivity.surfacePick, 0);
  assert.equal(coalesced.viewport.shown.bytes, '/m/5/0');
  // Even one that took under a millisecond is not its own next call.
  const instant = surfacePage();
  await instant.poll(comparison(5, 1000, 0));
  assert.equal(instant.env.agentActivity.surfacePick, 0);
});

test('the next agent call switches the heatmap off and keeps the panel and its data', async () => {
  const { env, viewport, poll } = surfacePage();
  await poll(comparison(5));
  await poll({ seq: 6, state: 'running', tool: 'measure', startedAt: 2000 });
  assert.equal(env.agentActivity.surfacePick, null);
  assert.equal(viewport.shown, null);
  assert.equal(env.agentActivity.surfaces.seq, 5, 'the last comparison stays');
  // That call finishing changes nothing, and it can be selected again.
  await poll({ seq: 6, state: 'done', tool: 'measure', startedAt: 2000, elapsedMs: 5 });
  assert.equal(env.agentActivity.surfaces.seq, 5);
  env.pickAgentSurface(1);
  await new Promise((resolve) => { setTimeout(resolve, 0); });
  assert.equal(viewport.shown.bytes, '/m/5/1');
});

test('a selection made by hand is switched off by the next call too, and an update is not a call', async () => {
  const { env, poll } = surfacePage();
  await poll(comparison(5));
  await poll({ seq: 6, state: 'done', startedAt: 2000, elapsedMs: 5 });
  env.pickAgentSurface(0);
  // A late update of a call this page already knows, such as a trimmed entry.
  await poll({ seq: 6, state: 'done', startedAt: 2000, elapsedMs: 5 });
  await poll({ seq: 5, state: 'done', startedAt: 1000, elapsedMs: 50 });
  assert.equal(env.agentActivity.surfacePick, 0);
  await poll({ seq: 7, state: 'running', startedAt: 3000 });
  assert.equal(env.agentActivity.surfacePick, null);
});

test('a new comparison replaces the data and shows itself', async () => {
  const { env, viewport, poll } = surfacePage();
  await poll(comparison(5));
  await poll({ seq: 6, state: 'running', startedAt: 2000 });
  await poll(comparison(6, 2000));
  assert.equal(env.agentActivity.surfaces.seq, 6);
  assert.equal(viewport.shown.bytes, '/m/6/0');
});

test('a call running alongside the comparison does not end it', async () => {
  const { env, viewport, poll } = surfacePage();
  await poll({ seq: 5, state: 'running', startedAt: 1000 });
  await poll({ seq: 6, state: 'running', startedAt: 1010 });
  await poll({ seq: 6, state: 'done', startedAt: 1010, elapsedMs: 5 });
  await poll(comparison(5, 1000, 500));
  assert.equal(viewport.shown.bytes, '/m/5/0');
  // Started while it ran, heard of only now: still not its next call.
  await poll({ seq: 7, state: 'running', startedAt: 1400 });
  assert.equal(env.agentActivity.surfacePick, 0);
  await poll({ seq: 8, state: 'running', startedAt: 1600 });
  assert.equal(env.agentActivity.surfacePick, null);
});

test('a batch holding the comparison and the call after it ends with the heatmap off', async () => {
  const { env, viewport, poll } = surfacePage();
  await poll({ seq: 6, state: 'running', startedAt: 2000 }, comparison(5));
  assert.equal(env.agentActivity.surfacePick, null);
  assert.equal(viewport.shown, null);
  assert.equal(env.agentActivity.surfaces.seq, 5);
});

test('surfaces that waited for a render stay in the panel when the agent has moved on', async () => {
  const { env, viewport, settle, poll } = surfacePage();
  const late = comparison(5);
  env.actOutAgentEntry = () => {};
  await poll(late);
  assert.equal(env.agentActivity.surfaces.seq, 5, 'in the panel before any render');
  await poll({ seq: 6, state: 'running', startedAt: 2000 });
  env.showAgentFindings(late, [], env.agentSurfaceMeshes(late), { fromPoll: true });
  await settle();
  assert.equal(env.agentActivity.surfacePick, null);
  assert.equal(viewport.shown, null);
  // The console's Show is an explicit ask, and is honoured.
  env.showAgentFindings(late, [], env.agentSurfaceMeshes(late));
  await settle();
  assert.equal(viewport.shown.bytes, '/m/5/0');
});

test('history fills the panel after a reload, with nothing selected', async () => {
  const { env, viewport, fetched, poll } = surfacePage();
  env.agentActivity.loaded = false;
  env.agentActivity.openedAt = 100000;
  await poll(comparison(5));
  assert.equal(env.agentActivity.surfaces.seq, 5);
  assert.equal(env.agentActivity.surfacePick, null);
  assert.equal(viewport.shown, null);
  assert.equal(fetched.length, 0, 'nothing is downloaded until it is selected');
});

test('a reload right after a comparison, inside the live slack, still selects nothing', async () => {
  const { env, viewport, poll } = surfacePage();
  env.agentActivity.loaded = false;
  env.agentActivity.openedAt = 1000;
  await poll(comparison(5, 500, 400));
  assert.equal(env.agentActivity.surfaces.seq, 5);
  assert.equal(env.agentActivity.surfacePick, null);
  assert.equal(viewport.shown, null);
  // A call still running as the page opened does appear when it ends.
  const running = surfacePage();
  running.env.agentActivity.loaded = false;
  running.env.agentActivity.openedAt = 1000;
  await running.poll({ seq: 5, state: 'running', startedAt: 500 });
  await running.poll(comparison(5, 500, 900));
  assert.equal(running.env.agentActivity.surfacePick, 0);
});

test('an older comparison that waited for a render never displaces a newer one', async () => {
  const { env, viewport, settle, poll } = surfacePage();
  const older = comparison(5);
  await poll(comparison(6, 2000));
  assert.equal(viewport.shown.bytes, '/m/6/0');
  env.showAgentFindings(older, [], env.agentSurfaceMeshes(older), { fromPoll: true });
  await settle();
  assert.equal(env.agentActivity.surfaces.seq, 6);
  assert.equal(viewport.shown.bytes, '/m/6/0');
});

test('an entry whose heavy parts were dropped leaves the panel as it was', async () => {
  const { env, poll } = surfacePage();
  await poll(comparison(5));
  await poll({ seq: 5, state: 'done', startedAt: 1000, elapsedMs: 50 });
  assert.equal(env.agentActivity.surfaces.meshes.length, 2);
  assert.equal(env.agentActivity.surfacePick, 0);
});

test('switching off retires a download still on its way; reselecting uses what loaded', async () => {
  const { env, viewport, fetched, settle } = surfacePage();
  env.keepAgentSurfaces(comparison(5));
  env.pickAgentSurface(0);
  env.switchOffAgentSurface();
  await settle();
  assert.equal(viewport.shown, null);
  env.pickAgentSurface(0);
  assert.equal(viewport.shown.bytes, '/m/5/0', 'from memory, at once');
  assert.equal(fetched.length, 1);
});

test('a surface that cannot be loaded lets go of the selection', async () => {
  const { env, settle } = surfacePage();
  env.keepAgentSurfaces({ seq: 5, tool: 'compare_surfaces', startedAt: 1, meshes: [{ url: '/gone', label: 'object-1' }, { url: '/gone', label: 'object-2' }] });
  env.pickAgentSurface(0);
  await settle();
  assert.equal(env.agentActivity.surfacePick, null);
  assert.equal(env.agentActivity.surfaces.seq, 5);
});

test('an agent comparison fills the section with its two parts chosen', async () => {
  const { env, viewport, poll } = surfacePage();
  await poll(comparison(5));
  assert.deepEqual(env.comparePicks, { a: 'object-1', b: 'object-2' });
  assert.equal(viewport.shown.bytes, '/m/5/0');
  // By name as well as by id.
  const named = surfacePage();
  await named.poll({ ...comparison(6), meshes: [{ url: '/x', label: 'Lid' }, { url: '/y', label: 'Bracket' }] });
  assert.deepEqual(named.env.comparePicks, { a: 'object-2', b: 'object-1' });
});

test('a part that no longer exists takes its heatmap and its choice with it', async () => {
  const { env, viewport, poll } = surfacePage();
  await poll(comparison(5));
  env.sceneObjects = [{ id: 'object-1', name: 'Bracket' }];
  env.syncCompareParts();
  assert.equal(env.agentActivity.surfacePick, null);
  assert.equal(viewport.shown, null);
  assert.equal(env.agentActivity.surfaces, null, 'nothing left to paint it on');
  assert.equal(env.comparePicks.a, 'object-1');
  assert.equal(env.comparePicks.b, '', 'chosen but gone');
});

test('a comparison of what is no part leaves the pickers alone and is drawn only by Show', async () => {
  const onePart = surfacePage();
  onePart.env.sceneObjects = [{ id: 'object-1', name: 'helmet 1' }];
  const mirror = { ...comparison(5), meshes: [{ url: '/m/5/0', label: 'scan', legend: { range: 2 } }, { url: '/m/5/1', label: 'mirror' }] };
  await onePart.poll(mirror);
  assert.deepEqual(onePart.env.comparePicks, { a: '', b: '' }, 'the pickers stay empty');
  const view = loadWith('compareControlsView')(onePart.env.sceneObjects, onePart.env.comparePicks);
  assert.deepEqual(view.options.map((option) => option.label), ['helmet 1'], 'no option added');
  assert.equal(view.status, 'Needs 2 parts');
  assert.equal(onePart.env.agentActivity.surfaces, null, 'not offered through the panel');
  assert.equal(onePart.env.agentActivity.surfacePick, null);
  assert.equal(onePart.viewport.shown, null, 'nothing drawn by itself');
  onePart.env.showAgentFindings(mirror, [], onePart.env.agentSurfaceMeshes(mirror));
  await onePart.settle();
  assert.deepEqual(onePart.env.agentActivity.side, { seq: 5 });
  assert.deepEqual(onePart.env.comparePicks, { a: '', b: '' });
  assert.equal(onePart.viewport.shown.bytes, '/m/5/0', 'the first side only: the two overlap');
  await onePart.poll({ seq: 6, state: 'running', startedAt: 5000 });
  assert.equal(onePart.viewport.shown, null, 'the next call ends it');
  // One side a part is still not two parts, nor is one part twice; earlier picks stay.
  for (const labels of [['object-1', 'scan.stl'], ['object-1', 'Bracket']]) {
    const { env, poll } = surfacePage();
    env.comparePicks.a = 'object-2';
    await poll({ ...comparison(7), meshes: labels.map((label) => ({ url: '/x', label })) });
    assert.deepEqual(env.comparePicks, { a: 'object-2', b: '' }, labels.join());
    assert.equal(env.agentActivity.surfaces, null, labels.join());
  }
});

test('a comparison row says what it compared', () => {
  const summary = loadWith('agentRowSummary');
  const meshes = [{ label: 'scan' }, { label: 'mirror' }];
  assert.equal(summary({ tool: 'compare_surfaces', summary: 'Compared: matches · rms 0 mm', meshes }), 'Compared scan with mirror: matches · rms 0 mm');
  assert.equal(summary({ tool: 'compare_surfaces', summary: 'Compared: matches' }), 'Compared: matches', 'too large to paint: no labels to name');
  assert.equal(summary({ tool: 'fit_surface', summary: 'Fitted', meshes }), 'Fitted');
  assert.equal(summary({ tool: 'measure' }), 'measure');
});

test('a comparison that came ahead of its parts takes the panel when they are listed, and not after', async () => {
  const early = surfacePage();
  early.env.sceneObjects = [];
  early.env.sceneObjectsCurrent = () => false;
  await early.poll(comparison(5));
  assert.deepEqual(early.env.comparePicks, { a: '', b: '' });
  assert.equal(early.viewport.shown, null);
  early.env.syncCompareParts();
  assert.equal(early.env.agentActivity.comparisonsWaiting.length, 1, 'the list has not answered yet');
  early.env.sceneObjects = [{ id: 'object-1', name: 'Bracket' }, { id: 'object-2', name: 'Lid' }];
  early.env.sceneObjectsCurrent = () => true;
  early.env.syncCompareParts();
  assert.deepEqual(early.env.comparePicks, { a: 'object-1', b: 'object-2' });
  assert.equal(early.env.agentActivity.surfaces.seq, 5);
  assert.equal(early.env.agentActivity.comparisonsWaiting.length, 0);

  // Its list came and held no such parts: objects named that way later are another matter.
  const labelled = { ...comparison(6), meshes: [{ url: '/x', label: 'scan' }, { url: '/y', label: 'mirror' }] };
  const late = surfacePage();
  await late.poll(labelled);
  assert.equal(late.env.agentActivity.comparisonsWaiting.length, 0, 'the list on screen already answered');
  late.env.sceneObjects = [{ id: 'object-1', name: 'scan' }, { id: 'object-2', name: 'mirror' }];
  late.env.syncCompareParts();
  // Nor does the poll, repeating the entry, bring it back.
  await late.poll(labelled);
  assert.deepEqual(late.env.comparePicks, { a: '', b: '' });
  assert.equal(late.env.agentActivity.surfaces, null);
});

test('only the list for its revision can answer a comparison, and a later one does not displace it', async () => {
  const { env, poll } = surfacePage();
  env.workspaceRevision = 2;
  // The list on screen happens to hold these ids, but it is an older revision's.
  await poll(comparison(5));
  assert.equal(env.agentActivity.surfaces, null);
  assert.deepEqual(env.comparePicks, { a: '', b: '' });
  await poll({ ...comparison(6), meshes: [{ url: '/x', label: 'scan' }, { url: '/y', label: 'mirror' }] });
  env.workspaceRevision = 3;
  env.syncCompareParts();
  assert.equal(env.agentActivity.surfaces.seq, 5, 'the comparison of parts, though a newer one is no such thing');
  assert.deepEqual(env.comparePicks, { a: 'object-1', b: 'object-2' });
  assert.equal(env.agentActivity.comparisonsWaiting.length, 0);
});

test('other tools never take the section; Show draws them, and the next call ends them', async () => {
  const { env, viewport, settle, poll } = surfacePage();
  await poll(comparison(5));
  const fit = { seq: 6, tool: 'fit_surface', state: 'done', startedAt: 2000, elapsedMs: 5, meshes: [{ url: '/m/6/0', label: 'fit', legend: { title: 'distance to the fit, mm' } }] };
  await poll(fit);
  assert.equal(env.agentActivity.surfaces.seq, 5, 'the comparison stays');
  assert.equal(viewport.shown, null, 'nothing drawn by itself');
  env.showAgentFindings(fit, [], env.agentSurfaceMeshes(fit));
  await settle();
  assert.deepEqual(env.agentActivity.side, { seq: 6 });
  assert.equal(env.agentActivity.surfaces.seq, 5);
  assert.equal(viewport.shown.bytes, '/m/6/0');
  await poll({ seq: 7, state: 'running', startedAt: 3000 });
  assert.equal(env.agentActivity.side, null);
  assert.equal(viewport.shown, null);
  // A fit is one surface; a single compare_surfaces mesh is no two-part result either.
  assert.equal(env.agentComparison({ tool: 'fit_surface', meshes: [{ url: 'a' }, { url: 'b' }] }), false);
  assert.equal(env.agentComparison({ tool: 'compare_surfaces', meshes: [{ url: 'a' }] }), false);
});

test('the section is in the left pane with its pickers; nothing is left in the viewport', () => {
  const pane = html.slice(html.indexOf('class="objects-pane"'), html.indexOf('</aside>', html.indexOf('class="objects-pane"')));
  assert.match(pane, /<h3 id="compareTitle">Compare parts<\/h3>/);
  assert.match(pane, /id="comparePartA" aria-label="First part" disabled/);
  assert.match(pane, /id="comparePartB" aria-label="Second part" disabled/);
  assert.match(pane, /id="compareStatus"[^>]*>Needs 2 parts</);
  assert.doesNotMatch(pane, /eyebrow|Scan overlay|No comparison yet/);
  assert.doesNotMatch(html, /agentStrip|agent-strip/);
  assert.doesNotMatch(javascript, /agentStrip|renderAgentStrip|reconcileAgentCards|agentLegendHtml|agentFade\b|meshesSince|meshSeq|meshPick/);
});

test('the tools are plain sections under the parts, with nothing to open', () => {
  const pane = html.slice(html.indexOf('class="objects-pane"'), html.indexOf('</aside>', html.indexOf('class="objects-pane"')));
  assert.equal((pane.match(/<section class="tool /g) || []).length, 4);
  assert.doesNotMatch(pane, /<details|<summary/);
  assert.ok(pane.indexOf('id="objectList"') < pane.indexOf('id="inspectionState"'));
  // Findings and tools keep their natural size; only the two lists scroll, from JS-set heights.
  for (const selector of ['\\.intersection-results', '\\.tool']) {
    for (const rule of css.matchAll(new RegExp(`^${selector} \\{([^}]*)\\}`, 'gm'))) {
      assert.doesNotMatch(rule[1], /max-height|overflow/, rule[0]);
    }
  }
});

test('the pickers: plain names, a prompt that cannot be picked, and at least two parts to choose from', () => {
  const view = loadWith('compareControlsView');
  const parts = [{ id: 'object-1', name: 'Bracket' }, { id: 'object-2', name: 'Lid' }];
  const none = view(parts, { a: '', b: '' });
  assert.deepEqual(none.options.map((option) => option.label), ['Bracket', 'Lid']);
  assert.equal(none.enough, true);
  assert.equal(none.status, '', 'nothing to say while idle');
  assert.equal(none.ready, false);
  assert.equal(view(parts, { a: 'object-1', b: 'object-2' }, { running: true }).status, 'Comparing…');
  const failed = view(parts, { a: 'object-1', b: 'object-2' }, { error: 'Too big' });
  assert.equal(failed.status, 'Failed', 'one word on the row');
  assert.equal(failed.detail, 'Too big', 'the reason in full under it');
  assert.equal(failed.tone, 'error');
  const one = view(parts.slice(0, 1), { a: '', b: '' });
  assert.equal(one.enough, false);
  assert.equal(one.status, 'Needs 2 parts');
  assert.deepEqual(view(parts, { a: 'scan.stl', b: 'object-1' }).options.map((option) => option.value), ['object-1', 'object-2'], 'only parts are ever listed');
  const options = loadWith('compareOptionsHtml', { escapeAttribute: String, escapeHtml: String });
  const html2 = options(none, 'First part', 'object-1');
  assert.doesNotMatch(html2, /First part/, 'a chosen picker has no placeholder to pick');
  assert.match(options(none, 'First part', ''), /^<option value="" disabled hidden selected>First part<\/option>/);
  assert.match(html2, /value="object-1" selected>Bracket</);
  assert.match(html2, /value="object-2">Lid</, 'the other picker\'s part can be chosen: that swaps them');
});

test('a choice ends a run and the heatmap but never starts one; the other part swaps', () => {
  const picks = { a: '', b: '' };
  const calls = [];
  const choose = loadWith('chooseComparePart', {
    comparePicks: picks,
    cancelCompareRun: () => calls.push('cancel'),
    switchOffAgentSurface: () => calls.push('off'),
    renderCompareControls: () => {},
    compareError: 'old',
  });
  choose('a', 'x');
  choose('b', 'y');
  assert.deepEqual(calls, ['cancel', 'off', 'cancel', 'off'], 'nothing runs by itself');
  choose('a', 'y');
  assert.deepEqual([picks.a, picks.b], ['y', 'x'], 'the part the other held swaps them');
  for (const dead of ['compareChange', 'agentSurfaces', 'data-agent-mesh', 'compareChanging', 'agentSurfacesHtml']) {
    assert.doesNotMatch(`${html}${javascript}`, new RegExp(dead));
  }
});

test('picks already there can be run: Compare is live on any two different parts, and Cancel while it runs', () => {
  const view = loadWith('compareControlsView');
  const parts = [{ id: 'object-1', name: 'Bracket' }, { id: 'object-2', name: 'Lid' }];
  // Filled by an agent or an earlier run, with no change event to start anything.
  assert.equal(view(parts, { a: 'object-1', b: 'object-2' }).ready, true);
  assert.equal(view(parts, { a: 'object-1', b: '' }).ready, false);
  assert.equal(view(parts, { a: 'object-1', b: 'object-1' }).ready, false);
  assert.equal(view(parts, { a: 'object-1', b: 'object-2' }, { enabled: false }).ready, false);
  // The real button, drawn in each state, and what pressing it does in each.
  const parts2 = parts;
  const element = () => {
    const node = { dataset: {}, attributes: {}, classes: {}, hidden: false, setAttribute: (key, value) => { node.attributes[key] = value; }, classList: { toggle: (name, on) => { node.classes[name] = on; } } };
    return node;
  };
  const nodes = new Map();
  const called = [];
  const env = {
    sceneObjects: parts2,
    comparePicks: { a: 'object-1', b: 'object-2' },
    compareRunning: false,
    compareError: '',
    agentActivity: { surfaces: null, surfacePick: null },
    assetsEnabled: () => true,
    compareControlsView: loadWith('compareControlsView'),
    compareOptionsHtml: () => '',
    objectSwatchHtml: () => '',
    compareScaleHtml: () => '',
    compareHeatmapShowing: loadWith('compareHeatmapShowing', { get agentActivity() { return env.agentActivity; } }),
    document: { getElementById: (id) => env.$(`#${id}`) },
    $: (selector) => { if (!nodes.has(selector)) nodes.set(selector, element()); return nodes.get(selector); },
    cancelCompareRun: () => { called.push('cancel'); env.compareRunning = false; },
    switchOffAgentSurface: () => called.push('off'),
    runCompareParts: () => called.push('run'),
  };
  env.renderCompareControls = loadWith('renderCompareControls', env);
  const button = env.$('#compareButton');
  let press;
  button.addEventListener = (_, listener) => { press = listener; };
  new Function('env', `with (env) { ${statementOf(javascript, "$('#compareButton').addEventListener(")} }`)(env);
  const draw = (state) => {
    Object.assign(env, { compareRunning: false, comparePicks: { a: 'object-1', b: 'object-2' }, agentActivity: { surfaces: null, surfacePick: null } }, state);
    env.renderCompareControls();
    return [button.textContent, button.disabled, button.attributes['aria-label'], Boolean(button.classes.running)];
  };
  assert.deepEqual(draw({}), ['Compare', false, 'Compare parts', false]);
  press();
  assert.deepEqual(called.splice(0), ['run']);
  assert.deepEqual(draw({ comparePicks: { a: 'object-1', b: '' } }), ['Compare', true, 'Compare parts', false]);
  assert.deepEqual(draw({ compareRunning: true, comparePicks: { a: 'object-1', b: '' } }), ['Cancel', false, 'Cancel comparison', true]);
  press();
  assert.deepEqual(called.splice(0), ['cancel']);
  assert.equal(button.textContent, 'Compare', 'cancelling redraws');
  const heatmap = { surfaces: { meshes: [{}] }, surfacePick: 0 };
  assert.deepEqual(draw({ agentActivity: heatmap, comparePicks: { a: 'object-1', b: '' } }), ['Hide', false, 'Hide heatmap', false]);
  press();
  assert.deepEqual(called.splice(0), ['off']);
});

test('a failure is said in words on the panel, an old server included', () => {
  const message = loadWith('compareFailureMessage');
  assert.match(message(404, { error: 'Design endpoint not found.' }), /cannot compare parts.*older build/);
  assert.equal(message(400, { error: 'Choose two different parts.' }), 'Choose two different parts.');
  assert.match(message(500, null), /HTTP 500/);
  assert.match(html, /id="compareError" role="alert" hidden/);
  assert.match(sourceOf('renderCompareControls'), /failure\.textContent = view\.detail;\s*failure\.hidden = !view\.detail;/);
});

test('reply labels become the parts themselves, whichever server wrote them', () => {
  const sceneObjects = [{ id: 'object-1', name: 'water_filter 1' }, { id: 'object-2', name: 'reconstruction 2' }];
  const id = loadWith('comparePartId', { sceneObjects });
  assert.equal(id('object-1'), 'object-1');
  assert.equal(id('scan object-1'), 'object-1');
  assert.equal(id('model object-2'), 'object-2');
  assert.equal(id('water_filter 1'), 'object-1');
  assert.equal(id('scan nozzle.stl'), 'nozzle.stl', 'not a part: its plain name');
  const parts = loadWith('compareControlsView')(sceneObjects, { a: id('scan object-1'), b: id('model object-2') });
  assert.deepEqual(parts.options.map((option) => option.label), ['water_filter 1', 'reconstruction 2'], 'the same names, in the same order, nothing extra');
  assert.match(html, /id="compareSwatchA"/);
  assert.match(sourceOf('renderCompareControls'), /objectSwatchHtml\(part\)/);
  assert.match(sourceOf('renderObjectList'), /objectSwatchHtml\(object\)/);
});

test('the heatmap key is one bar with its two ends, and the class colours are its tooltip', () => {
  const env = { escapeHtml: String };
  env.surfaceScaleHtml = loadWith('surfaceScaleHtml', env);
  env.surfaceKeysHtml = loadWith('surfaceKeysHtml', env);
  const scale = loadWith('compareScaleHtml', env);
  assert.equal(scale({}), '', 'no range: nothing to show');
  const on = scale({ range: 0.9157, keys: [{ color: [1, 2, 3], text: 'not compared' }, { color: [4, 5, 6], text: 'facing mismatch' }] });
  assert.match(on, /−0\.916/, 'three figures are what a colour can tell');
  assert.match(on, /\+0\.916 mm/);
  assert.match(on, /role="tooltip".*not compared.*facing mismatch/);
  assert.match(on, /rgb\(1,2,3\)/);
  assert.match(css, /\.compare-scale:hover \.compare-scale-tip, \.compare-scale:focus \.compare-scale-tip \{ display: grid; \}/);
  // Only while one is showing.
  assert.match(sourceOf('renderCompareControls'), /surfaces && pick !== null \? compareScaleHtml\(surfaces\.meshes\[pick\]\?\.legend\) : ''/);
  assert.match(html, /id="compareScale" tabindex="0"[^>]*hidden/);
});

test('a heatmap of an older revision does not stay on the moved geometry', () => {
  const off = [];
  const render = (revision, workspaceRevision) => {
    off.length = 0;
    loadWith('renderRevisions', {
      revisionRows: [],
      $: () => ({ contains: () => false, innerHTML: '', textContent: '' }),
      document: {},
      revisionRowHtml: String,
      agentActivity: { surfaces: revision === undefined ? null : { revision } },
      workspaceRevision,
      switchOffAgentSurface: () => off.push(true),
    })();
    return off.length;
  };
  assert.equal(render(2, 3), 1);
  assert.equal(render(3, 3), 0);
  assert.equal(render(undefined, 3), 0);
});

test('running from the panel posts the two parts and takes its own result', async () => {
  const page = ({ reply, moved = false } = {}) => {
    const calls = [];
    const env = {
      calls,
      comparePicks: { a: 'object-1', b: 'object-2' },
      workspaceId: 'happy otter',
      workspaceRevision: 3,
      compareRun: 0,
      compareController: null,
      compareRunning: false,
      compareError: '',
      agentActivity: { entries: new Map(), lastSeq: 0, surfaces: null, surfacePick: null },
      assetsEnabled: () => true,
      flushWorkspaceSave: async () => { calls.push('flush'); },
      fetch: async (url, options) => { calls.push('fetch'); env.url = url; env.options = options; return reply ? reply(env) : { ok: true, status: 200, json: async () => ({ entry: { seq: 5 } }) }; },
      compareFailureMessage: (status) => `failed ${status}`,
      switchOffAgentSurface() {},
      renderCompareControls() {},
      renderAgentFeed() {},
      agentComparison: () => true,
      agentMovedOnFrom: () => moved,
      keepAgentSurfaces: (entry) => { env.agentActivity.surfaces = { seq: entry.seq }; },
      pickAgentSurface: (index) => calls.push(`pick ${index}`),
      log(message, level) { calls.push(`log ${level}`); },
    };
    env.cancelCompareRun = loadWith('cancelCompareRun', env);
    env.run = new Function('env', `with (env) { return (async ${sourceOf('runCompareParts')}); }`)(env);
    return env;
  };
  const run = page();
  await run.run();
  assert.deepEqual(run.calls, ['flush', 'fetch', 'pick 0'], 'saved first, and its own result painted');
  assert.equal(run.url, '/api/designs/happy%20otter/compare');
  assert.equal(run.options.method, 'POST');
  assert.deepEqual(JSON.parse(run.options.body), { a: 'object-1', b: 'object-2' });
  assert.equal(run.agentActivity.lastSeq, 5);
  assert.equal(run.compareRunning, false);

  const moved = page({ moved: true });
  await moved.run();
  assert.deepEqual(moved.calls, ['flush', 'fetch'], 'a call since keeps it off');

  const failed = page({ reply: () => ({ ok: false, status: 500, json: async () => null }) });
  await failed.run();
  assert.equal(failed.compareError, 'failed 500');

  // Saved over while it ran: the old revision's reply stays off the new model, the button
  // goes back to idle, and it is no error. A reply of this revision still paints.
  const reply = (revision) => () => ({ ok: true, status: 200, json: async () => ({ entry: { seq: 5, revision } }) });
  const stale = page({ reply: reply(2) });
  await stale.run();
  assert.deepEqual(stale.calls, ['flush', 'fetch', 'log info'], 'no pick');
  assert.equal(stale.agentActivity.surfaces, null);
  assert.equal(stale.compareError, '');
  assert.equal(stale.compareRunning, false);
  const same = page({ reply: reply(3) });
  await same.run();
  assert.deepEqual(same.calls, ['flush', 'fetch', 'pick 0']);

  // Cancel aborts the request, and the reply it still gets is dropped.
  const cancelled = page({ reply: (env) => { env.cancelCompareRun(); return { ok: false, status: 500, json: async () => null }; } });
  await cancelled.run();
  assert.equal(cancelled.options.signal.aborted, true);
  assert.equal(cancelled.compareError, '');

  // Tagged by who ran it, and refreshed with the part list: rendered inside functions too large to stand up here.
  assert.match(javascript, /<small>\$\{entry\.fromBrowser \? 'you' : 'agent'\}/);
  assert.match(sourceOf('renderObjectList'), /syncCompareParts\(\);/);
});

test('Check is Cancel while a check runs, and cancelling drops the reply', async () => {
  const page = ({ reply, flush } = {}) => {
    const log = [];
    const nodes = {};
    const node = (selector) => {
      nodes[selector] ??= { attributes: {}, setAttribute(key, value) { this.attributes[key] = value; }, classList: { toggle() {} }, focus() {} };
      return nodes[selector];
    };
    const loadController = new AbortController();
    const env = {
      log,
      nodes,
      loadController,
      intersectionCheck: null,
      intersectionCheckSource: null,
      intersectionIds: new Set(),
      workspaceGeneration: 1,
      workspaceId: 'happy-otter',
      readOnlyToken: null,
      sceneObjects: [],
      editor: { value: 'cube(1);' },
      agentActivity: {},
      clock: 3500,
      performance: { now: () => env.clock },
      meshViewport: { draw: () => log.push('draw') },
      $: node,
      setInterval: () => 1,
      clearInterval() {},
      readTolerance: () => 0.2,
      escapeHtml: String,
      isCurrentWorkspace: () => true,
      sceneObjectsCurrent: () => true,
      workspaceLoadSignal: () => loadController.signal,
      renderObjectList: () => log.push('list'),
      showIntersectionResults: () => log.push('shown'),
      flushWorkspaceSave: async () => { log.push(`flush while ${node('#checkIntersections').textContent}`); await flush?.(env); },
      fetch: async (url, options) => { log.push('fetch'); env.options = options; return reply ? reply(env) : { ok: true, status: 200, json: async () => ({ ok: true }) }; },
    };
    for (const name of ['showIntersectionCheckRunning', 'cancelIntersectionCheck', 'resetIntersectionResults', 'showIntersectionProgress']) env[name] = loadWith(name, env);
    env.check = new Function('env', `with (env) { return (async ${sourceOf('checkIntersections')}); }`)(env);
    Object.defineProperty(node('#checkIntersections'), 'disabled', { set() { assert.fail('the button stays pressable while it runs'); } });
    return env;
  };
  const button = (env) => [env.nodes['#checkIntersections'].textContent, env.nodes['#checkIntersections'].attributes['aria-label']];

  const plain = page();
  await plain.check();
  assert.deepEqual(plain.log, ['flush while Cancel', 'fetch', 'shown'], 'it says so before anything can wait');
  assert.deepEqual(button(plain), ['Check', 'Check intersections']);
  assert.equal(plain.intersectionCheck, null);

  // Pressed again mid-run, it cancels; the reply to the first press is dropped.
  let during;
  const cancelled = page({ reply: async (env) => {
    if (during) assert.fail('a second press starts nothing');
    during = button(env);
    await env.check();
    return { ok: true, status: 200, json: async () => ({ ok: true }) };
  } });
  await cancelled.check();
  assert.deepEqual(during, ['Cancel', 'Cancel intersection check']);
  assert.equal(cancelled.options.signal.aborted, true);
  assert.equal(cancelled.log.filter((entry) => entry === 'fetch').length, 1, 'a second press starts nothing');
  assert.ok(!cancelled.log.includes('shown'));
  assert.deepEqual(button(cancelled), ['Check', 'Check intersections']);
  assert.ok(cancelled.log.includes('list') && cancelled.log.includes('draw'), 'earlier findings leave the rows and the model too');
  assert.equal(cancelled.nodes['#inspectionState'].textContent, 'Not checked');

  // Leaving the design aborts it as well.
  const left = page({ reply: async (env) => { env.loadController.abort(); return { ok: true, status: 200, json: async () => ({ ok: true }) }; } });
  await left.check();
  assert.equal(left.options.signal.aborted, true);

  // Source edited while it ran: the reply describes a model no longer on screen.
  const edited = page({ reply: (env) => { env.editor.value = 'cube(2);'; return { ok: true, status: 200, json: async () => ({ ok: true }) }; } });
  await edited.check();
  assert.ok(!edited.log.includes('shown'));
  assert.deepEqual(button(edited), ['Check', 'Check intersections']);

  // A switch of design while it waits leaves no Cancel behind and clears the findings.
  const switched = page({ flush: (env) => { env.workspaceGeneration += 1; } });
  await switched.check();
  assert.ok(!switched.log.includes('fetch'));
  assert.equal(switched.intersectionCheck, null);
  assert.deepEqual(button(switched), ['Check', 'Check intersections']);
  assert.equal(switched.nodes['#inspectionState'].textContent, 'Not checked');
  assert.ok(switched.log.includes('list') && switched.log.includes('draw'));

  // A preview landing mid-check keeps its progress, and says no "run it again".
  const running = page();
  running.intersectionCheck = { startedAt: 0 };
  running.$('#intersectionResults').textContent = 'Measuring visible parts…';
  running.resetIntersectionResults('The source changed since the last check. Run it again.');
  assert.equal(running.nodes['#inspectionState'].textContent, 'Checking… 3 s');
  assert.doesNotMatch(running.nodes['#intersectionResults'].innerHTML, /Run it again/);
  running.intersectionCheck = null;
  running.resetIntersectionResults('The source changed since the last check. Run it again.');
  assert.match(running.nodes['#intersectionResults'].innerHTML, /Run it again/);
});

test('a call from after the page opened counts as news even in the poll\'s first answer', () => {
  const summary = sourceOf('applyActivitySummary');
  assert.match(summary, /history && entry\.state !== 'running' && Number\(entry\.startedAt\) < agentActivity\.openedAt/);
  assert.match(summary, /entry\.state === 'done' && !entry\.fromBrowser && agentComparison\(entry\)/, 'an aborted comparison cannot come back through the poll');
  assert.match(sourceOf('showAgentFindings'), /if \(entry\.fromBrowser\) return;/, 'nor can a finished one repaint over newer picks');
});
