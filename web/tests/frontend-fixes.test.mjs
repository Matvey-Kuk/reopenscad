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
  const prefix = javascript.slice(start - 6, start) === 'async ' ? 'async ' : '';
  return prefix + javascript.slice(start, index + 1);
}

// Runs a shipped function against a stand-in for the page globals it reads.
function loadWith(name, env = {}) {
  return new Function('env', `with (env) { return (${sourceOf(name)}); }`)(env);
}

const escapeHtml = (value) => String(value).replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');

test('a too-small gap reads as a clearance problem, never as a zero overlap', () => {
  const classify = loadWith('classifyIntersection');
  const gap = classify({ relation: 'clearanceViolation', intersects: false, clearance: 0.1, depth: null, overlapVolume: 0 }, 0.2);
  assert.equal(gap.kind, 'clearance');
  assert.equal(gap.text, 'gap 0.10 mm < 0.20 mm tolerance');
  // A gap that would round up to the tolerance keeps a third decimal.
  assert.equal(classify({ relation: 'clearanceViolation', clearance: 0.1996 }, 0.2).text, 'gap 0.200 mm < 0.20 mm tolerance');
  // Overlap is only claimed with a positive volume, and a null depth is not "0.000".
  const overlap = classify({ relation: 'overlap', intersects: true, overlapVolume: 3.4273, depth: 0.8779 }, 0.2);
  assert.equal(overlap.kind, 'overlap');
  assert.equal(overlap.text, 'overlap 3.427 mm³ · 0.88 mm deep');
  assert.doesNotMatch(classify({ relation: 'overlap', intersects: true, overlapVolume: 2, depth: null }, 0.2).text, /0\.000|deep/);
  assert.equal(classify({ relation: 'overlap', intersects: true, overlapVolume: 0, depth: null }, 0.2).kind, 'clearance');
  assert.equal(classify({ relation: 'clear', clearance: 9 }, 0.2).kind, 'clear');
  assert.equal(classify({ relation: 'contact', contactAllowed: true }, 0.2).kind, 'clear');
  assert.equal(classify({ relation: 'contact', contactAllowed: false, toleranceViolated: true }, 0.2).kind, 'clearance');
  // Older replies without `relation` map onto the same vocabulary.
  assert.equal(classify({ intersects: false, withinTolerance: true, clearance: 0.05 }, 0.2).kind, 'clearance');
  assert.equal(classify({ intersects: true, overlapVolume: 1 }, 0.2).kind, 'overlap');
});

test('an incomplete, skipped or undetermined check is never reported as clear', () => {
  const env = {};
  env.classifyIntersection = loadWith('classifyIntersection');
  const summarize = loadWith('summarizeIntersections', env);
  const clear = { relation: 'clear', determined: true, clearance: 5 };
  assert.equal(summarize({ complete: true, intersections: [clear] }, 0.2).state, 'clear');
  // A pair the server ran out of time for.
  const skipped = summarize({ complete: false, intersections: [clear, { a: 'a', b: 'b', relation: 'skipped', skipped: true, determined: false }] }, 0.2);
  assert.equal(skipped.state, 'incomplete');
  assert.equal(skipped.unresolved.length, 1);
  // An undetermined verdict.
  assert.equal(summarize({ intersections: [{ relation: 'undetermined', determined: false, intersects: null }] }, 0.2).state, 'incomplete');
  // A reply that admits it is incomplete even though every listed pair looks clean.
  assert.equal(summarize({ complete: false, intersections: [clear] }, 0.2).state, 'incomplete');
  assert.equal(summarize({ summary: { complete: false }, intersections: [] }, 0.2).state, 'incomplete');
  // Real problems win, but the unresolved pairs are still listed beside them.
  const mixed = summarize({ complete: false, intersections: [{ relation: 'overlap', intersects: true, overlapVolume: 1 }, { relation: 'skipped', skipped: true }] }, 0.2);
  assert.equal(mixed.state, 'collision');
  assert.equal(mixed.findings.length, 2);
  // The panel only says "Clear" from the summarised verdict.
  // The user's check and an agent's findings share one presenter.
  assert.match(sourceOf('checkIntersections'), /showIntersectionResults\(data, tolerance, source\);/);
  const showing = sourceOf('showIntersectionResults');
  assert.match(showing, /if \(verdict\.state === 'clear'\) \{\s*state\.textContent = `\$\{by\}Clear`;/);
  assert.equal((showing.match(/Clear`/g) || []).length, 1);
});

test('the intersection check measures the parts as they are now, also in a shared view', () => {
  const checking = sourceOf('checkIntersections');
  // Ids are reconciled with the source before they are sent: a stale object list
  // is recompiled first, and the source itself travels with the request.
  assert.ok(checking.indexOf('await flushWorkspaceSave()') < checking.indexOf("if (!sceneObjectsCurrent())"));
  assert.ok(checking.indexOf("await render('preview')") < checking.indexOf('const visibleObjectIds'));
  assert.match(checking, /body: JSON\.stringify\(\{ code: source, tolerance/);
  // A read-only link has no workspace id: the stateless route, never /workspaces/null.
  assert.match(checking, /const url = workspaceId && !token \? `\/api\/designs\/\$\{encodeURIComponent\(workspaceId\)\}\/intersections` : '\/api\/intersections';/);
  // Findings become markers at the reported place, with the overlap box or the gap's closest points.
  const showing = sourceOf('showIntersectionResults');
  assert.match(showing, /meshViewport\.markers = verdict\.findings\.map/);
  assert.match(showing, /item\.overlapBounds/);
  assert.match(showing, /item\.closestPoints/);
  assert.match(javascript, /drawMarkers\(\) \{/);
});

test('a revision conflict adopts the server revision before rendering or exporting again', async () => {
  const rendering = sourceOf('render');
  const exporting = sourceOf('exportModel');
  // The old fallback re-sent the superseded source through the stateless route.
  assert.doesNotMatch(rendering, /sendRender\('\/api\/render', requestBody/);
  assert.doesNotMatch(exporting, /postExport\('\/api\/export'\)/);
  assert.match(rendering, /attempt\.status === 409[\s\S]{0,120}adoptServerRevision\(workspaceId, request\.generation\)/);
  assert.match(exporting, /response\.status === 409[\s\S]{0,300}adoptServerRevision\(workspaceId, generation\)/);
  assert.match(exporting, /if \(!objectIds\) scheduleSourceRender\(\);/);
  assert.match(exporting, /requestBody = \{ \.\.\.requestBody, code: editor\.value, revision: workspaceRevision \};/);

  const applied = [];
  const env = {
    workspaceGeneration: 3,
    workspaceId: 'ws',
    workspaceRevision: 4,
    workspaceOnline: false,
    workspaceLoadSignal: () => new AbortController().signal,
    workspaceFromPayload: (payload) => payload?.workspace || payload || {},
    refreshSyncState: () => {},
    fetchWorkspaceChanges: async () => ({ changed: true, revision: 6, code: 'cube(6);', updatedRanges: [] }),
    applyWorkspace: (payload, options) => { applied.push([payload.revision, options]); env.workspaceRevision = payload.revision; },
    fetch: async () => { throw new Error('unused'); },
  };
  const adopt = loadWith('adoptServerRevision', env);
  assert.equal(await adopt('ws', 3), true);
  assert.deepEqual(applied, [[6, { remote: true, rerender: false }]]);
  // Navigated away meanwhile: nothing is applied to the workspace now on screen.
  applied.length = 0;
  assert.equal(await adopt('ws', 2), false);
  assert.deepEqual(applied, []);
  // Never rewinds to an older revision.
  env.fetchWorkspaceChanges = async () => null;
  env.fetch = async () => ({ ok: true, json: async () => ({ workspace: { revision: 2, code: 'x' } }) });
  assert.equal(await adopt('ws', 3), false);
});

test('geometry of replaced source stays, marked out of date, when its refresh fails', () => {
  const rendering = sourceOf('render');
  assert.doesNotMatch(rendering, /clearGeometry/);
  assert.match(rendering, /activeRender = null;\s*showGeometryStaleness\(\);/);
  assert.match(css, /\.render-canvas\.stale \{/);
});

test('a workspace without compiled objects shows no phantom "Model" row', () => {
  const normalize = loadWith('normalizeObjects', { sceneObjects: [] });
  assert.deepEqual(normalize([]), []);
  assert.equal(normalize([], { placeholder: true })[0].placeholder, true);
  // Only a render that actually produced a mesh asks for the placeholder.
  assert.match(sourceOf('render'), /setSceneObjects\(Array\.isArray\(data\.objects\) \? data\.objects : sceneObjects, \{ placeholder: true \}\)/);
});

test('AI change ranges are merged and never split a token', () => {
  const env = { escapeHtml, aiUpdatedRanges: [] };
  env.TOKEN_PATTERN = new Function(`return ${javascript.match(/^const TOKEN_PATTERN = (.*);$/m)[1]}`)();
  env.tokenSpans = loadWith('tokenSpans', env);
  env.mergeRanges = loadWith('mergeRanges', env);
  const highlight = loadWith('syntaxHighlight', env);
  const code = 'cube(10); // note\nsphere(2);';
  // Two overlapping ranges plus one on the next line: two marks, in order.
  const html = highlight(code, [{ start: 13, end: 15 }, { start: 14, end: 17 }, { start: 25, end: 26 }]);
  assert.equal((html.match(/<mark class="ai-code-change">/g) || []).length, 2);
  assert.equal((html.match(/<\/mark>/g) || []).length, 2);
  // The comment stays a comment on both sides of the boundary.
  assert.match(html, /<span class="tok-comment">\/\/ <\/span><mark class="ai-code-change"><span class="tok-comment">note<\/span><\/mark>/);
  assert.match(html, /<mark class="ai-code-change"><span class="tok-number">2<\/span><\/mark>/);
  // Without ranges the text round-trips unchanged.
  assert.equal(highlight(code, []).replace(/<[^>]+>/g, ''), `${code}\n`);
  // The badge pushes the text down instead of covering line 1.
  assert.match(css, /\.editor-wrap\.ai-revised \.highlight, \.editor-wrap\.ai-revised #codeEditor, \.editor-wrap\.ai-revised \.line-numbers \{ padding-top: 38px; \}/);
  assert.match(css, /\.ai-code-change \.tok-comment \{/);
});

test('empty workspaces are listed too, labelled, and an offline id moves to the server id', () => {
  let stored = '[]';
  const localStorage = { getItem: () => stored, setItem: (_, value) => { stored = value; } };
  const env = { localStorage, RECENTS_STORAGE_KEY: 'k', Date };
  env.readRecents = loadWith('readRecents', env);
  env.writeRecents = loadWith('writeRecents', env);
  const remember = loadWith('rememberWorkspace', env);
  const note = loadWith('noteRecentContent', env);
  const rename = loadWith('renameRecentWorkspace', env);
  remember('empty-one', { empty: true });
  remember('full-one', { empty: false });
  assert.deepEqual(env.readRecents().map((entry) => [entry.id, entry.empty]), [['full-one', false], ['empty-one', true]]);
  // Revisiting without knowing the content keeps the previous answer.
  remember('empty-one');
  assert.equal(env.readRecents()[0].empty, true);
  note('empty-one', false);
  assert.equal(env.readRecents()[0].empty, false);
  rename('empty-one', 'server-id');
  assert.deepEqual(env.readRecents().map((entry) => entry.id), ['server-id', 'full-one']);
  assert.match(sourceOf('renderLandingRecents'), /landing-thumb empty/);
});

test('a workspace created offline is created on the server once it is reachable', async () => {
  const calls = [];
  const env = {
    workspaceLocalOnly: true,
    workspaceId: 'brisk-otter-localid000000',
    readOnlyToken: null,
    workspaceGeneration: 1,
    localPromotion: null,
    workspaceOnline: false,
    workspaceSaveFailures: 2,
    workspaceRevision: 0,
    workspaceSavedCode: '',
    workspaceHasLocalChanges: true,
    editor: { value: 'cube(1);' },
    readTolerance: () => 0.2,
    DEFAULT_TOLERANCE: 0.2,
    workspaceLoadSignal: () => new AbortController().signal,
    workspaceFromPayload: (payload) => payload?.workspace || payload || {},
    workspaceStorageKey: (id) => `w.${id}`,
    displacedStorageKey: (id) => `d.${id}`,
    localStorage: { removeItem: (key) => calls.push(`remove ${key}`), getItem: () => null, setItem: () => {} },
    renameRecentWorkspace: (from, to) => calls.push(`rename ${from} ${to}`),
    noteRecentContent: () => {},
    promotedWorkspaceIds: new Map(),
    setWorkspaceIdentity: (id, navigation) => { env.workspaceId = id; calls.push(`identity ${id} ${navigation}`); },
    storeLocalWorkspace: () => calls.push('store'),
    scheduleWorkspaceSave: () => calls.push('save'),
    refreshSyncState: () => {},
    beginWorkspacePolling: () => calls.push('poll'),
    log: () => {},
    fetch: async (url, init) => {
      calls.push(`${init.method} ${url} ${JSON.parse(init.body).code}`);
      return { ok: true, json: async () => ({ workspace: { id: 'server-id-0000000000', revision: 1 } }) };
    },
  };
  const promote = loadWith('promoteLocalWorkspace', env);
  assert.equal(await promote(), true);
  assert.equal(env.workspaceLocalOnly, false);
  assert.equal(env.workspaceId, 'server-id-0000000000');
  assert.equal(env.workspaceSavedCode, 'cube(1);');
  assert.deepEqual(calls.slice(0, 3), ['POST /api/designs cube(1);', 'remove w.brisk-otter-localid000000', 'rename brisk-otter-localid000000 server-id-0000000000']);
  assert.ok(calls.includes('identity server-id-0000000000 replace'));
  assert.ok(!calls.includes('save'));
  // Actions that began under the offline id still belong to this workspace.
  assert.equal(env.promotedWorkspaceIds.get('brisk-otter-localid000000'), 'server-id-0000000000');
  // Not local-only: nothing to promote.
  assert.equal(await promote(), false);
  // Every path that meets the 404 promotes instead of retrying it forever.
  assert.match(sourceOf('saveWorkspace'), /response\.status === 404 && workspaceLocalOnly[\s\S]{0,200}promoteLocalWorkspace\(\)/);
  assert.match(sourceOf('beginWorkspacePolling'), /response\.status === 404 && workspaceLocalOnly/);
  assert.match(sourceOf('loadWorkspace'), /payload\.missing === true && cached\?\.localOnly/);
  assert.match(sourceOf('storeLocalWorkspace'), /localOnly: true/);
});

test('landing cards keep units, dimensions and hint calls together', () => {
  const keep = loadWith('keepUnitsTogether');
  assert.equal(keep('four 6 x 2 mm magnets.'), 'four 6 x 2 mm magnets.');
  assert.equal(keep('a 31 mm M3 pattern'), 'a 31 mm M3 pattern');
  const hint = loadWith('exampleHintHtml', { escapeHtml });
  assert.equal(hint('polygon() · linear_extrude()'), '<span>polygon()</span> <span>· linear_extrude()</span>');
  assert.match(css, /\.example-card code span \{ white-space: nowrap; \}/);
});

test('popovers and badges cannot hide or misplace themselves', () => {
  // The "source changed" hint has its own display rule, so [hidden] needs one too.
  assert.match(css, /\.export-popover-hint\[hidden\] \{ display: none; \}/);
  // Both toolbar menus open from their button's left edge and stay inside the window.
  const anchor = (left) => ({ getBoundingClientRect: () => ({ left, bottom: 40 }) });
  const exportPopover = { offsetWidth: 240, style: {} };
  const placeExport = loadWith('placeExportPopover', { exportPopover, innerWidth: 1000 });
  for (const [left, expected] of [[100, '100px'], [900, '752px'], [-20, '8px']]) {
    placeExport(anchor(left));
    assert.equal(exportPopover.style.left, expected);
    assert.equal(exportPopover.style.top, '45px');
  }
  // The toolbar holds the view toggles and wraps rather than clipping them.
  assert.match(css, /\.toolbar \{ display: flex; flex-wrap: wrap;/);
  // Control by control, from the left: groups dissolve into the row, and nothing pushes right.
  assert.match(css, /\.tool-group, \.view-mode, \.viewport-options \{ display: contents; \}/);
  assert.doesNotMatch(html, /toolbar-spacer/);
});

test('the share popover opens from its button and stays inside the window', async () => {
  const sharePopover = { hidden: true, style: {} };
  const open = new Function('env', `with (env) { return (${sourceOf('openSharePopover')}); }`)({
    sharePopover,
    innerWidth: 1000,
    $: () => ({}),
    workspaceEditUrl: () => '',
    ensureShareToken: async () => null,
    location: { origin: '' },
  });
  for (const [left, expected] of [[100, '100px'], [900, '688px'], [-20, '12px']]) {
    await open({ getBoundingClientRect: () => ({ left, bottom: 40 }) });
    assert.equal(sharePopover.style.left, expected);
    assert.equal(sharePopover.style.top, '45px');
    assert.equal(sharePopover.hidden, false);
  }
});

test('Share and Export lead the toolbar, then a group gap, with Connect AI still last', () => {
  const toolbar = html.slice(html.indexOf('<div class="toolbar"'), html.indexOf('id="readonlyBanner"'));
  const order = ['id="shareButton"', 'id="exportButton"', 'data-sheet="objects"', 'id="newButton"', 'data-view="top"', 'data-projection="perspective"', 'id="edgesToggle"', 'id="feedbackButton"', 'id="mcpButton"'];
  const positions = order.map((marker) => toolbar.indexOf(marker));
  assert.ok(positions.every((position) => position >= 0));
  assert.deepEqual(positions, [...positions].sort((a, b) => a - b));
  assert.match(toolbar, /^<div class="toolbar"[^>]*>\s*<button class="share-button"/);
  assert.match(css, /\.export-button, [^{]*\{ margin-right: var\(--tb-group-extra\); \}/);
});

test('every toolbar toggle shows and exposes its state the same way', () => {
  // Projection and Edges are buttons with aria-pressed, like the section and comment toggles.
  assert.match(html, /class="view-toggle" data-projection="perspective" aria-pressed="true"/);
  assert.match(html, /class="view-toggle" data-projection="orthographic" aria-pressed="false"/);
  assert.match(html, /class="view-toggle" id="edgesToggle" aria-pressed="false"/);
  let draws = 0;
  const button = (projection) => {
    const element = { dataset: { projection }, attributes: {}, setAttribute: (key, value) => { element.attributes[key] = value; }, addEventListener: (_, listener) => { element.click = () => listener({ currentTarget: element }); } };
    return element;
  };
  const buttons = [button('perspective'), button('orthographic')];
  const edges = button();
  const env = { projection: 'perspective', meshViewport: { showEdges: false, draw: () => { draws += 1; } }, $$: () => buttons, $: () => edges };
  for (const marker of ["$$('[data-projection]').forEach((button) => button.addEventListener", "$('#edgesToggle').addEventListener("]) {
    new Function('env', `with (env) { ${statementOf(javascript, marker)} }`)(env);
  }
  buttons[1].click();
  assert.equal(env.projection, 'orthographic');
  assert.deepEqual(buttons.map((item) => item.attributes['aria-pressed']), ['false', 'true']);
  buttons[0].click();
  assert.equal(env.projection, 'perspective');
  assert.deepEqual(buttons.map((item) => item.attributes['aria-pressed']), ['true', 'false']);
  edges.click();
  assert.equal(env.meshViewport.showEdges, true);
  assert.equal(edges.attributes['aria-pressed'], 'true');
  edges.click();
  assert.equal(edges.attributes['aria-pressed'], 'false');
  assert.equal(draws, 4);
  assert.match(css, /\.view-toggle\[aria-pressed="true"\], \.viewport-options \.pane-action\[aria-pressed="true"\], \.viewport-options \.pane-action\[aria-expanded="true"\] \{/);
  // The group gap trails, so a wrapped row still starts flush left.
  assert.match(css, /--tb-group-extra: 8px;/);
  assert.match(css, /margin-right: var\(--tb-group-extra\)/);
  assert.doesNotMatch(css, /\.tool-divider/);
});

test('framing centres the projected model and labels avoid the viewport furniture', () => {
  assert.match(javascript, /fitView\(fill = 0\.78\) \{/);
  assert.match(sourceOf('render'), /meshViewport\.setMesh\(data\.mesh, sceneObjects\)/);
  assert.match(javascript, /overlayObstacles\(ratio\) \{/);
  assert.match(javascript, /\['\.view-hint', '#gridScale', '\.viewport-fab', '#slicerPanel'\]/);
});

test('the recents list keeps every design and folds the long tail on the landing page', () => {
  let stored = '[]';
  const localStorage = { getItem: () => stored, setItem: (_, value) => { stored = value; } };
  const env = { localStorage, RECENTS_STORAGE_KEY: 'k', Date };
  env.readRecents = loadWith('readRecents', env);
  env.writeRecents = loadWith('writeRecents', env);
  const remember = loadWith('rememberWorkspace', env);
  for (let index = 0; index < 40; index += 1) remember(`design-${index}`);
  assert.equal(env.readRecents().length, 40);
  assert.equal(env.readRecents()[0].id, 'design-39');
  assert.doesNotMatch(javascript, /RECENTS_LIMIT/);
  const rendering = sourceOf('renderLandingRecents');
  assert.match(rendering, /recentsExpanded \? entries : entries\.slice\(0, RECENTS_SHOWN\)/);
  assert.match(rendering, /Show all \(\$\{entries\.length\}\)/);
  assert.match(javascript, /closest\('\[data-recents-toggle\]'\)/);
});

test('only a design\'s first mesh frames the camera; later renders keep it', () => {
  const start = javascript.indexOf('  setMesh(encoded, objects = []) {');
  const body = javascript.slice(start, javascript.indexOf('\n  multiply(a, b) {', start)).trim().replace(/^setMesh/, 'function');
  const camera = [0, 0, 0, 58, 0, 28, 140];
  const setMesh = new Function('env', `with (env) { return (${body}); }`)({ camera, Math, Infinity, clampSlicePosition: (position) => position, sliceRange: () => null });
  let fits = 0;
  const viewport = {
    framed: false,
    slice: { axis: 2, position: 0 },
    meshBatches: [],
    center: [0, 0, 0],
    gl: { deleteBuffer() {} },
    createMeshBatch: (id, mesh) => ({ id, min: mesh.min, max: mesh.max }),
    pivot() { return this.center.map((value, axis) => value + camera[axis]); },
    fitView() { fits += 1; camera[6] = 99; },
    draw() {},
  };
  setMesh.call(viewport, { min: [0, 0, 0], max: [10, 10, 10] });
  assert.equal(fits, 1);
  assert.equal(viewport.framed, true);
  // The user zooms and pans, then an edit grows the model.
  camera[6] = 42;
  camera[0] = 3;
  const pivot = viewport.pivot();
  setMesh.call(viewport, { min: [0, 0, 0], max: [40, 20, 10] });
  assert.equal(fits, 1, 'a re-render does not refit');
  assert.equal(camera[6], 42, 'zoom is kept');
  assert.deepEqual(viewport.pivot(), pivot, 'the camera target stays put in world space');
  // Opening another design frames again.
  assert.match(sourceOf('resetScene'), /meshViewport\.framed = false;/);
  // The explicit fit button still refits.
  assert.match(javascript, /#fitView'\)\.addEventListener\('click', \(\) => \{\n\s+camera\[0\] = camera\[1\] = camera\[2\] = 0;\n\s+meshViewport\.fitView\(\);/);
});

test('the viewport defaults to two thirds of the width, the editor to one third', () => {
  assert.match(css, /--editor-share: calc\(\(100% - var\(--objects-width\) - 4px\) \/ 3\);/);
  assert.doesNotMatch(css, /--editor-share: \d+%/);
});

test('show all and hide all flip every object at once', () => {
  assert.match(html, /id="showAllObjects"/);
  assert.match(html, /id="hideAllObjects"/);
  const calls = [];
  const sceneObjects = [{ name: 'a', visible: true }, { name: 'b', visible: false }, { name: 'c' }];
  const setAll = loadWith('setAllObjectsVisible', {
    sceneObjects,
    renderObjectList: () => calls.push('list'),
    meshViewport: { draw: () => calls.push('draw') },
    log: (message) => calls.push(message),
  });
  setAll(false);
  assert.deepEqual(sceneObjects.map((object) => object.visible), [false, false, false]);
  setAll(true);
  assert.deepEqual(sceneObjects.map((object) => object.visible), [true, true, true]);
  assert.deepEqual(calls, ['list', 'draw', 'All objects hidden.', 'list', 'draw', 'All objects shown.']);
  const listing = sourceOf('renderObjectList');
  assert.match(listing, /\$\('#showAllObjects'\)\.disabled = !sceneObjects\.some\(\(object\) => object\.visible === false\)/);
  assert.match(listing, /\$\('#hideAllObjects'\)\.disabled = !sceneObjects\.some\(\(object\) => object\.visible !== false\)/);
});
