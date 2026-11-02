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

const helpers = ['escapeRegExp', 'assetImportPattern', 'assetReferenced', 'insertAssetImport', 'formatScaleFactor', 'wrapAssetImportWithScale', 'sanitizeAssetName', 'assetsSummaryAction', 'sharedCloneRequest'];
const lib = new Function(`${helpers.map(sourceOf).join('\n')}\nreturn { ${helpers.join(', ')} };`)();

test('asset names are sanitised to the server pattern', () => {
  const { sanitizeAssetName } = lib;
  assert.equal(sanitizeAssetName('Mesh 2026-10-15.stl'), 'Mesh_2026-10-15.stl');
  assert.equal(sanitizeAssetName('C:\\scans\\Head Scan (final).STL'), 'Head_Scan_final.stl');
  assert.equal(sanitizeAssetName('/tmp/part.stl'), 'part.stl');
  assert.equal(sanitizeAssetName('Café crème.stl'), 'Cafe_creme.stl');
  assert.equal(sanitizeAssetName('.hidden.stl'), 'hidden.stl');
  assert.equal(sanitizeAssetName('???.stl'), 'mesh.stl');
  assert.equal(sanitizeAssetName('noext'), 'noext.stl');
  const long = sanitizeAssetName(`${'a'.repeat(200)}.stl`);
  assert.ok(long.length <= 64 && long.endsWith('.stl'));
  for (const name of ['Mesh 2026-10-15.stl', '日本語.stl', 'x'.repeat(100), 'a b/c d.stl']) {
    assert.match(sanitizeAssetName(name), /^[A-Za-z0-9._-]{1,64}$/);
    assert.match(sanitizeAssetName(name), /\.stl$/);
  }
});

test('import insertion skips referenced assets and respects statement boundaries', () => {
  const { insertAssetImport, assetReferenced } = lib;
  assert.equal(insertAssetImport('import("a.stl");\n', 'a.stl'), null);
  assert.equal(insertAssetImport('scale(2) import(file = "a.stl", convexity = 4);', 'a.stl'), null);
  // A commented-out import is not a use.
  assert.ok(!assetReferenced('// import("a.stl");', 'a.stl'));
  assert.ok(!assetReferenced('import("ab.stl");', 'a.stl'));

  // No cursor: appended on its own line.
  const appended = insertAssetImport('cube(10);', 'a.stl');
  assert.equal(appended.code, 'cube(10);\nimport("a.stl");\n');
  assert.equal(appended.code.slice(appended.start, appended.end), 'import("a.stl");');
  assert.equal(insertAssetImport('', 'a.stl').code, 'import("a.stl");\n');

  // Cursor on a blank (indented) line: placed right there.
  const code = 'union() {\n  cube(1);\n  \n}\n';
  const blank = insertAssetImport(code, 'a.stl', code.indexOf('  \n') + 2);
  assert.equal(blank.code, 'union() {\n  cube(1);\n  import("a.stl");\n}\n');
  assert.equal(blank.code.slice(blank.start, blank.end), 'import("a.stl");');

  // Cursor at the end of a finished statement: the next line, same indent.
  const after = insertAssetImport(code, 'a.stl', code.indexOf('cube(1);') + 'cube(1);'.length);
  assert.equal(after.code, 'union() {\n  cube(1);\n  import("a.stl");\n  \n}\n');

  // Cursor in the middle of an expression: never splits it, falls back to the end.
  const mid = insertAssetImport('cube(10);', 'a.stl', 5);
  assert.equal(mid.code, 'cube(10);\nimport("a.stl");\n');
});

test('units hint wraps the import in scale()', () => {
  const { wrapAssetImportWithScale, formatScaleFactor } = lib;
  assert.equal(formatScaleFactor(25.4), '25.4');
  assert.equal(formatScaleFactor(0.1 + 0.2), '0.3');
  assert.equal(formatScaleFactor(1000), '1000');

  const wrapped = wrapAssetImportWithScale('cube(1);\nimport("scan.stl");\n', 'scan.stl', 25.4);
  assert.equal(wrapped.code, 'cube(1);\nscale(25.4) import("scan.stl");\n');
  assert.equal(wrapped.code.slice(wrapped.start, wrapped.end), 'scale(25.4) import("scan.stl");');

  // An existing scalar scale is replaced, not stacked.
  assert.equal(wrapAssetImportWithScale('scale(10) import("scan.stl");', 'scan.stl', 0.1).code, 'scale(0.1) import("scan.stl");');
  // Wrapped inside a transform chain, the rest of the chain is kept.
  assert.equal(wrapAssetImportWithScale('translate([1,0,0]) import("scan.stl");', 'scan.stl', 1000).code, 'translate([1,0,0]) scale(1000) import("scan.stl");');
  // No import yet: added, already scaled.
  assert.equal(wrapAssetImportWithScale('cube(1);', 'scan.stl', 25.4).code, 'cube(1);\nscale(25.4) import("scan.stl");\n');
  assert.equal(wrapAssetImportWithScale('x', 'scan.stl', 0), null);
  assert.equal(wrapAssetImportWithScale('x', 'scan.stl', Number.NaN), null);
});

test('asset changes are read from the /events assetsVersion', () => {
  const { assetsSummaryAction } = lib;
  assert.deepEqual(assetsSummaryAction({ revision: 3 }, null), { action: 'none' });
  assert.deepEqual(assetsSummaryAction(null, null), { action: 'none' });
  assert.deepEqual(assetsSummaryAction({ assetsVersion: 0 }, null), { action: 'track', version: 0 });
  assert.deepEqual(assetsSummaryAction({ assetsVersion: 2 }, 2), { action: 'none' });
  assert.deepEqual(assetsSummaryAction({ assetsVersion: 3 }, 2), { action: 'reload', version: 3 });
  assert.deepEqual(assetsSummaryAction({ assetsVersion: 2, assetsChanged: true }, 2), { action: 'reload', version: 2 });
});

test('asset UI is wired: picker, drop zones, panel, read-only guard, design id on renders', () => {
  assert.match(html, /id="fileInput"[^>]*accept="[^"]*\.stl/);
  assert.match(html, /id="assetsSection"/);
  assert.match(html, /id="assetRow"/);
  assert.match(html, /id="assetStatus"[^>]*aria-live="polite"/);
  assert.match(javascript, /xhr\.upload\.addEventListener\('progress'/);
  assert.match(javascript, /for \(const zone of \[\$\('\.editor-pane'\), \$\('#viewport'\)\]\)/);
  assert.match(javascript, /section\.hidden = Boolean\(readOnlyToken\)/);
  assert.match(css, /body\.read-only \.assets-section, body\.read-only \.compare-section \{ display: none; \}/);
  // The editor's render and the export.
  assert.equal((javascript.match(/designId: workspaceId/g) || []).length, 2);
  assert.match(javascript, /applyAssetsSummary\(payload\);/);
});

test('a poll ending on our own asset write is tracked, not re-rendered', () => {
  const { assetsSummaryAction } = lib;
  // Upload still in flight: the poll saw its version first.
  assert.deepEqual(assetsSummaryAction({ assetsVersion: 3, assetsChanged: true }, 2, { pending: 1, versions: [] }), { action: 'track', version: 3 });
  // Upload finished first and recorded the version it produced.
  assert.deepEqual(assetsSummaryAction({ assetsVersion: 3, assetsChanged: true }, 2, { pending: 0, versions: [3] }), { action: 'track', version: 3 });
  // Someone else's later write still reloads.
  assert.deepEqual(assetsSummaryAction({ assetsVersion: 4, assetsChanged: true }, 3, { pending: 0, versions: [3] }), { action: 'reload', version: 4 });
});

test('own asset writes are bracketed and shared views carry the link token', () => {
  for (const name of ['uploadAsset', 'deleteAsset']) assert.match(sourceOf(name), /beginOwnAssetWrite\(\)/, name);
  assert.match(sourceOf('applyAssetsSummary'), /assetsSummaryAction\(payload, assetsVersion, ownAssetWrites\)/);
  assert.match(javascript, /shareToken: readOnlyToken/);
  assert.match(javascript, /shareToken: token/);
  assert.match(javascript, /shareToken: sharedToken/);
  const { url, init } = lib.sharedCloneRequest('a/b', 'cube(1);');
  assert.equal(url, '/api/shared/a%2Fb/clone');
  assert.equal(init.method, 'POST');
  assert.deepEqual(JSON.parse(init.body), { code: 'cube(1);' });
});

const runChunkedUpload = new Function(`async ${sourceOf('runChunkedUpload')}\nreturn runChunkedUpload;`)();

// A server stand-in for the chunk protocol: in-order chunks get 202 with
// `next`, the last one 200; a wrong offset gets 409 naming where to resume.
function chunkServer(size, { dropOffset = null } = {}) {
  const calls = [];
  let received = 0;
  const send = async ({ start, end, uploadId }) => {
    calls.push({ start, end, uploadId });
    if (start !== 0 && uploadId !== 'u1') return { status: 404, body: { ok: false, error: 'unknown' } };
    if (start !== received && !(start < received && end === received)) {
      return { status: 409, body: { ok: false, uploadId: 'u1', received, next: received, total: size } };
    }
    if (dropOffset === start) { dropOffset = null; received = start; return { status: 409, body: { ok: false, uploadId: 'u1', received, next: received, total: size } }; }
    received = Math.max(received, end);
    if (received === size) return { status: 200, body: { ok: true, name: 'scan.stl', assetsVersion: 4 } };
    return { status: 202, body: { ok: true, uploadId: 'u1', received, next: received, total: size } };
  };
  return { calls, send };
}

test('large files go up in 24 MiB chunks with the upload id and Content-Range', () => {
  assert.match(javascript, /const ASSET_CHUNK_BYTES = 24 \* 1024 \* 1024;/);
  const upload = sourceOf('uploadAsset');
  assert.match(upload, /setRequestHeader\('Content-Range', `bytes \$\{start\}-\$\{end - 1\}\/\$\{size\}`\)/);
  assert.match(upload, /setRequestHeader\('Upload-Id', uploadId\)/);
  assert.match(upload, /file\.slice\(start, end\)/);
  assert.match(upload, /\(start \+ event\.loaded\) \/ size/, 'progress spans every chunk');
  assert.match(upload, /runChunkedUpload\(size, ASSET_CHUNK_BYTES, sendPart, op\)/);
});

test('a chunked upload walks the file and returns the final reply', async () => {
  const mib = 1024 * 1024;
  const size = 60 * mib + 5;
  const server = chunkServer(size);
  const reply = await runChunkedUpload(size, 24 * mib, server.send);
  assert.equal(reply.status, 200);
  assert.equal(reply.body.assetsVersion, 4);
  assert.deepEqual(server.calls, [
    { start: 0, end: 24 * mib, uploadId: null },
    { start: 24 * mib, end: 48 * mib, uploadId: 'u1' },
    { start: 48 * mib, end: size, uploadId: 'u1' },
  ]);
});

test('a chunked upload resumes from next on a 409 and stops on errors or cancel', async () => {
  const size = 100;
  const resumed = chunkServer(size, { dropOffset: 40 });
  const reply = await runChunkedUpload(size, 40, resumed.send);
  assert.equal(reply.status, 200);
  assert.deepEqual(resumed.calls.map((call) => call.start), [0, 40, 40, 80]);

  const cancelled = await runChunkedUpload(size, 40, async () => ({ status: -1, body: null }));
  assert.equal(cancelled.status, -1);
  const refused = await runChunkedUpload(size, 40, async () => ({ status: 413, body: { ok: false, error: 'too big' } }));
  assert.equal(refused.status, 413);
  let conflicts = 0;
  const stuck = await runChunkedUpload(size, 40, async () => { conflicts += 1; return { status: 409, body: { uploadId: 'u1', next: 40 } }; });
  assert.equal(stuck.status, 409, 'gives up after repeated conflicts');
  assert.equal(conflicts, 4);
});

const newAssetUploadOp = new Function(`${sourceOf('newAssetUploadOp')}\nreturn newAssetUploadOp;`)();

test('cancelling during a retry backoff stops the upload without another send', async () => {
  const op = newAssetUploadOp();
  let sends = 0;
  const started = Date.now();
  const pending = runChunkedUpload(100, 40, async () => {
    sends += 1;
    if (sends === 1) setTimeout(() => op.cancel(), 20);
    return { status: 0, body: null };
  }, op);
  const reply = await pending;
  assert.equal(reply.status, -1, 'reported as a cancel');
  assert.equal(sends, 1, 'no retry is sent after the cancel');
  assert.ok(Date.now() - started < 900, 'the backoff is cut short');
});

test('a cancelled op aborts its request and sends nothing more', async () => {
  const op = newAssetUploadOp();
  let aborted = 0;
  op.xhr = { abort: () => { aborted += 1; } };
  op.cancel();
  op.cancel();
  assert.equal(aborted, 1);
  assert.equal(op.xhr, null);
  let sends = 0;
  const reply = await runChunkedUpload(100, 40, async () => { sends += 1; return { status: 202, body: {} }; }, op);
  assert.equal(reply.status, -1);
  assert.equal(sends, 0);
  // A part that completes after the cancel is not followed by another.
  const late = newAssetUploadOp();
  const calls = [];
  const lateReply = await runChunkedUpload(100, 40, async ({ start }) => {
    calls.push(start);
    late.cancel();
    return { status: 202, body: { uploadId: 'u1', next: 40 } };
  }, late);
  assert.equal(lateReply.status, -1);
  assert.deepEqual(calls, [0]);
});

test('Cancel and a design switch cancel the current op; a second upload does not', () => {
  const upload = sourceOf('uploadAsset');
  const inFlight = upload.indexOf('if (assetUploadOp)');
  assert.ok(inFlight >= 0 && inFlight < upload.indexOf('newAssetUploadOp()'), 'one in flight refuses a second');
  assert.doesNotMatch(upload, /assetUploadOp\?\.cancel\(\)/);
  assert.match(upload, /const op = newAssetUploadOp\(\);\s*assetUploadOp = op;/);
  assert.match(upload, /if \(op\.cancelled \|\| !current\(\)\)/, 'every part checks the cancel flag first');
  assert.match(upload, /runChunkedUpload\(size, ASSET_CHUNK_BYTES, sendPart, op\)/);
  assert.match(sourceOf('resetAssets'), /assetUploadOp\?\.cancel\(\)/);
  assert.match(javascript, /action === 'cancel'\) assetUploadOp\?\.cancel\(\)/);
  assert.doesNotMatch(javascript, /assetUploadXhr/);
});

test('a busy reply (429/503) waits as told and sends the same part again', async () => {
  const calls = [];
  let busy = 1;
  const reply = await runChunkedUpload(100, 40, async ({ start }) => {
    calls.push(start);
    if (start === 80 && busy > 0) { busy -= 1; return { status: 503, body: { ok: false, retryable: true, retryAfterSeconds: 0.01, reason: 'upload_finishing' } }; }
    if (start === 80) return { status: 200, body: { ok: true } };
    return { status: 202, body: { uploadId: 'u1', next: start + 40 } };
  });
  assert.equal(reply.status, 200);
  assert.deepEqual(calls, [0, 40, 80, 80]);
  const refused = await runChunkedUpload(100, 40, async () => ({ status: 503, body: { ok: false, retryable: false } }));
  assert.equal(refused.status, 503, 'not retried when the server says it is not retryable');
});

// --- The STL row -------------------------------------------------------------

const MINUTE = 60000;
const HOUR = 60 * MINUTE;

const rowLib = new Function(`${['assetExpired', 'currentAssetUpload', 'formatAssetCountdown', 'assetRowView', 'assetRefusalMessage'].map(sourceOf).join('\n')}
return { assetExpired, currentAssetUpload, formatAssetCountdown, assetRowView, assetRefusalMessage };`)();

// The real renderAssetRow against just enough of a page.
function rowPage({ readOnly = false, enabled = true } = {}) {
  const element = (name) => {
    const attributes = {};
    const classes = new Set();
    const node = {
      name,
      hidden: false,
      disabled: false,
      textContent: '',
      title: '',
      writes: 0,
      setAttribute: (key, value) => { attributes[key] = value; },
      getAttribute: (key) => attributes[key],
      classList: { toggle: (cls, on) => (on ? classes.add(cls) : classes.delete(cls)), contains: (cls) => classes.has(cls) },
      focus() { page.document.activeElement = node; },
    };
    return node;
  };
  const nodes = {};
  for (const id of ['assetUploadButton', 'assetHint', 'assetCurrent', 'assetName', 'assetCountdown', 'assetRemoveButton', 'assetRow', 'assetRowStatus', 'assetAnnounce', 'assetStatus']) nodes[`#${id}`] = element(id);
  const section = element('assetsSection');
  section.contains = (node) => Object.values(nodes).includes(node);
  const page = {
    nodes,
    section,
    now: 1_000_000_000,
    assets: [],
    op: null,
    renders: [],
    logs: [],
    confirmCancels: [],
    timer: null,
    timerStarts: 0,
    code: 'import("scan.stl");',
    document: { activeElement: null, getElementById: (id) => (id === 'assetsSection' ? section : nodes[`#${id}`]) },
  };
  const api = new Function('page', 'readOnlyToken', 'assetsEnabled', `
    const document = page.document;
    const $ = (selector) => page.nodes[selector];
    const Date = { now: () => page.now };
    const setInterval = (fn) => { page.timer = fn; page.timerStarts += 1; return page.timerStarts; };
    const clearInterval = () => { page.timer = null; };
    const log = (message, tone) => page.logs.push([message, tone]);
    const render = (mode) => page.renders.push(mode);
    const editor = { get value() { return page.code; } };
    let assetRowShown = null;
    let assetCountdownTimer = null;
    let assetConfirmResolve = null;
    let designAssets = [];
    let assetUploadOp = null;
    let assetClockOffsetMs = page.offset || 0;
    ${['assetNow', 'assetExpired', 'currentAssetUpload', 'formatAssetCountdown', 'assetRowView', 'clearAssetStatus', 'renderAssetRow'].map(sourceOf).join('\n')}
    return {
      show(assets) { designAssets = assets; renderAssetRow(); },
      uploading(op) { assetUploadOp = op; },
      confirm(fn) { assetConfirmResolve = fn; },
      render: renderAssetRow,
      tick: () => page.timer?.(),
    };
  `)(page, readOnly ? 'token' : null, () => enabled && !readOnly);
  return { page, nodes, ...api };
}

const uploadAt = (page, msLeft, extra = {}) => ({ name: 'scan.stl', expiresAt: page.now + msLeft, ...extra });

test('the countdown reads in hours and minutes, rounded up, and never reads zero', () => {
  const { formatAssetCountdown } = rowLib;
  assert.equal(formatAssetCountdown(3 * HOUR), 'expires in 3h');
  assert.equal(formatAssetCountdown(2 * HOUR + 14 * MINUTE), 'expires in 2h 14m');
  assert.equal(formatAssetCountdown(2 * HOUR + 13 * MINUTE + 1), 'expires in 2h 14m');
  assert.equal(formatAssetCountdown(59 * MINUTE + 1), 'expires in 1h');
  assert.equal(formatAssetCountdown(14 * MINUTE), 'expires in 14m');
  assert.equal(formatAssetCountdown(MINUTE), 'expires in 1m');
  assert.equal(formatAssetCountdown(MINUTE - 1), 'expires in 60s');
  assert.equal(formatAssetCountdown(42000), 'expires in 42s');
  assert.equal(formatAssetCountdown(1), 'expires in 1s');
  assert.equal(formatAssetCountdown(Number.NaN), '');
});

test('only a live upload counts: expired entries never do', () => {
  const { currentAssetUpload, assetRowView } = rowLib;
  const scan = { name: 'scan.stl', expiresAt: 2000 };
  assert.equal(currentAssetUpload([], 1000), null);
  assert.equal(currentAssetUpload(null, 1000), null);
  assert.equal(currentAssetUpload([scan], 1000), scan);
  assert.equal(currentAssetUpload([scan], 2000), null, 'gone at the very moment it expires');
  // Just uploaded, before the list comes back: no end known yet, still present.
  const fresh = assetRowView([{ name: 'scan.stl' }], 1000);
  assert.equal(fresh.name, 'scan.stl');
  assert.equal(fresh.countdown, 'expiry unknown', 'never a silent blank');
  assert.equal(fresh.counting, false);
  assert.equal(fresh.stressed, false);
});

test('stress starts as the label first reads 30m', () => {
  const { assetRowView } = rowLib;
  const view = (msLeft) => assetRowView([{ name: 'a.stl', expiresAt: 1000 + msLeft }], 1000);
  assert.equal(view(30 * MINUTE + 1).stressed, false);
  assert.equal(view(30 * MINUTE + 1).countdown, 'expires in 31m');
  assert.equal(view(30 * MINUTE).stressed, true);
  assert.equal(view(30 * MINUTE).countdown, 'expires in 30m');
  assert.equal(view(1).stressed, true);
});

test('row with no STL: Upload STL and the hint, no timer', () => {
  const { nodes, show, page } = rowPage();
  show([]);
  assert.equal(nodes['#assetUploadButton'].hidden, false);
  assert.equal(nodes['#assetHint'].hidden, false);
  assert.equal(nodes['#assetHint'].textContent, 'Kept for 3 hours, for reverse engineering');
  assert.equal(nodes['#assetCurrent'].hidden, true);
  assert.equal(nodes['#assetRemoveButton'].hidden, true);
  assert.equal(page.timer, null);
});

test('row with an STL: name, countdown, a remove button named after the file', () => {
  const { nodes, show, page } = rowPage();
  show([uploadAt(page, 2 * HOUR + 14 * MINUTE)]);
  assert.equal(nodes['#assetUploadButton'].hidden, true);
  assert.equal(nodes['#assetHint'].hidden, true);
  assert.equal(nodes['#assetCurrent'].hidden, false);
  assert.equal(nodes['#assetName'].textContent, 'scan.stl');
  assert.equal(nodes['#assetCountdown'].textContent, 'expires in 2h 14m');
  assert.equal(nodes['#assetRemoveButton'].hidden, false);
  assert.equal(nodes['#assetRemoveButton'].getAttribute('aria-label'), 'Remove scan.stl');
  assert.equal(nodes['#assetRow'].classList.contains('stressed'), false);
  assert.equal(page.timerStarts, 1);
});

test('the row counts down, stresses at 30 minutes and tells screen readers once', () => {
  const { nodes, show, tick, page } = rowPage();
  show([uploadAt(page, 31 * MINUTE)]);
  assert.equal(nodes['#assetCountdown'].textContent, 'expires in 31m');
  page.now += MINUTE;
  tick();
  assert.equal(nodes['#assetCountdown'].textContent, 'expires in 30m');
  assert.equal(nodes['#assetRow'].classList.contains('stressed'), true);
  assert.equal(nodes['#assetAnnounce'].textContent, 'scan.stl expires in 30m');
  page.now += MINUTE;
  tick();
  assert.equal(nodes['#assetCountdown'].textContent, 'expires in 29m');
  assert.equal(nodes['#assetAnnounce'].textContent, 'scan.stl expires in 30m', 'not every tick');
  assert.equal(page.timerStarts, 1, 'one timer, however many renders');
});

test('at zero the row returns to no-STL without a reload, and the design re-renders once', () => {
  const { nodes, show, tick, page } = rowPage();
  show([uploadAt(page, 1000)]);
  page.now += 999;
  tick();
  assert.equal(nodes['#assetCurrent'].hidden, false);
  assert.equal(page.renders.length, 0);
  page.now += 1;
  tick();
  assert.equal(nodes['#assetCurrent'].hidden, true);
  assert.equal(nodes['#assetUploadButton'].hidden, false);
  assert.equal(nodes['#assetRow'].classList.contains('stressed'), false);
  assert.equal(page.timer, null, 'the countdown stops');
  assert.deepEqual(page.renders, ['preview'], 'the failed import shows in the diagnostics');
  assert.equal(nodes['#assetAnnounce'].textContent, 'scan.stl expired');
  tick();
  assert.equal(page.renders.length, 1);
});

test('expiry noticed by a list refresh, ahead of the tick, still re-renders', () => {
  const { nodes, show, page } = rowPage();
  const scan = uploadAt(page, 1000);
  show([scan]);
  page.now += 1000;
  show([]);
  assert.deepEqual(page.renders, ['preview']);
  assert.equal(nodes['#assetAnnounce'].textContent, 'scan.stl expired');
  assert.equal(page.timer, null);
});

test('an upload removed before it expired is not reported as expired', () => {
  const { nodes, show, page } = rowPage();
  show([uploadAt(page, HOUR)]);
  show([]);
  assert.equal(nodes['#assetAnnounce'].textContent, '');
  assert.equal(page.logs.length, 0);
});

test('a different upload clears the old status; a fresh one learning its expiry keeps it', () => {
  const { nodes, show, page } = rowPage();
  show([uploadAt(page, HOUR)]);
  nodes['#assetStatus'].hidden = false;
  show([uploadAt(page, HOUR, { name: 'other.stl' })]);
  assert.equal(nodes['#assetStatus'].hidden, true, 'its Scale button named the old file');

  const fresh = rowPage();
  fresh.show([{ name: 'scan.stl' }]);
  fresh.nodes['#assetStatus'].hidden = false;
  fresh.show([uploadAt(fresh.page, HOUR)]);
  assert.equal(fresh.nodes['#assetStatus'].hidden, false, 'the units hint of this very upload stays');
});

test('a failed write re-renders only if the list turned out to change', async () => {
  const reloader = (first, next, renders) => new Function('first', 'next', 'renders', `
    const workspaceId = 'd';
    const editor = { value: 'cube(1);' };
    const render = (mode) => renders.push(mode);
    let assetRowShown = first;
    const loadAssets = async () => { assetRowShown = next; };
    return async ${sourceOf('reloadAssetsAfterFailedWrite')};
  `)(first, next, renders);
  const unchanged = [];
  await reloader({ key: 'a' }, { key: 'a' }, unchanged)();
  assert.deepEqual(unchanged, []);
  const changed = [];
  await reloader({ key: 'a' }, { key: 'agent.stl|5' }, changed)();
  assert.deepEqual(changed, ['preview']);
});

test('removal re-reads the list first and refuses if the file changed under the question', () => {
  const remove = sourceOf('deleteAsset');
  const reread = remove.indexOf('await loadAssets()');
  const guard = remove.indexOf('assetRowShown?.key !== key');
  const request = remove.indexOf("method: 'DELETE'");
  assert.ok(reread >= 0 && reread < guard && guard < request);
});

test('a stale list cannot bring an expired upload back', () => {
  const { nodes, show, page } = rowPage();
  show([uploadAt(page, -1)]);
  assert.equal(nodes['#assetCurrent'].hidden, true);
  assert.equal(page.timer, null);
});

test('an agent uploading or removing moves the row, and the focus with it', () => {
  const { nodes, show, confirm, page } = rowPage();
  show([]);
  nodes['#assetUploadButton'].focus();
  show([uploadAt(page, HOUR)]);
  assert.equal(page.document.activeElement, nodes['#assetRemoveButton'], 'focus is not lost on a hidden button');
  let answer = null;
  confirm((value) => { answer = value; });
  nodes['#assetStatus'].hidden = false;
  nodes['#assetStatus'].innerHTML = 'already has scan.stl';
  show([]);
  assert.equal(answer, false, 'an open Remove question is withdrawn');
  assert.equal(nodes['#assetStatus'].hidden, true, 'a refusal naming the file is gone with it');
  assert.equal(nodes['#assetUploadButton'].hidden, false);
  assert.equal(page.document.activeElement, nodes['#assetUploadButton']);
  assert.equal(page.renders.length, 0, 'removal over MCP re-renders through applyAssetsSummary, not here');
});

test('a list refresh of the same file leaves focus and status alone', () => {
  const { nodes, show, confirm, page } = rowPage();
  const scan = uploadAt(page, HOUR);
  show([scan]);
  nodes['#assetRemoveButton'].focus();
  let answer = null;
  confirm((value) => { answer = value; });
  nodes['#assetStatus'].hidden = false;
  show([{ ...scan }]);
  assert.equal(answer, null);
  assert.equal(nodes['#assetStatus'].hidden, false);
  assert.equal(page.document.activeElement, nodes['#assetRemoveButton']);
});

test('an upload in flight keeps its status and marks the button without dropping focus', () => {
  const { nodes, show, uploading, render, page } = rowPage();
  show([uploadAt(page, HOUR)]);
  uploading({});
  nodes['#assetStatus'].hidden = false;
  show([]);
  assert.equal(nodes['#assetStatus'].hidden, false, 'progress stays');
  assert.equal(nodes['#assetUploadButton'].getAttribute('aria-disabled'), 'true');
  assert.equal(nodes['#assetUploadButton'].disabled, false);
  uploading(null);
  render();
  assert.equal(nodes['#assetUploadButton'].getAttribute('aria-disabled'), 'false');
});

test('a read-only view hides the row; an unsaved design cannot upload', () => {
  const shared = rowPage({ readOnly: true });
  shared.show([]);
  assert.equal(shared.page.section.hidden, true);
  const unsaved = rowPage({ enabled: false });
  unsaved.show([]);
  assert.equal(unsaved.nodes['#assetUploadButton'].disabled, true);
  assert.equal(unsaved.nodes['#assetHint'].textContent, 'STL uploads need a saved design');
});

test('a second upload is refused here and by the server, whose message wins', () => {
  const upload = sourceOf('uploadAsset');
  const refusal = upload.indexOf('currentAssetUpload(designAssets');
  assert.ok(refusal >= 0 && refusal < upload.indexOf('newAssetUploadOp()'), 'refused before anything is sent');
  assert.match(upload, /showAssetError\(name, assetRefusalMessage\(existing\.name\)\)/);
  assert.match(rowLib.assetRefusalMessage('scan.stl'), /“scan\.stl”.*remove it/);
  const message = new Function(`${sourceOf('assetRefusalMessage')}\n${sourceOf('assetUploadErrorMessage')}\nreturn assetUploadErrorMessage;`)();
  assert.equal(message(409, { ok: false, error: 'scan.stl is already here; delete_asset removes it.' }), 'scan.stl is already here; delete_asset removes it.');
  assert.match(message(409, null), /remove it/);
  // A refused upload re-reads the list, so the row shows what the agent uploaded.
  assert.match(upload, /showAssetError\(name, assetUploadErrorMessage\(status, body\)\);[\s\S]*?reloadAssetsAfterFailedWrite\(\);\s*return false;/);
});

test('removal goes through DELETE and takes the file off the list', () => {
  const remove = sourceOf('deleteAsset');
  assert.match(remove, /method: 'DELETE'/);
  assert.match(remove, /asset\.name !== name\)/);
});

test('the row has accessible names, and the countdown is outside every live region', () => {
  assert.match(html, /<button[^>]*id="assetUploadButton"[^>]*>Upload STL<\/button>/);
  assert.match(html, /id="assetAnnounce" role="status"/);
  assert.doesNotMatch(html, /id="assetCountdown"[^>]*aria-live/);
  assert.doesNotMatch(html, /aria-live[^>]*>\s*<[^>]*id="assetCountdown"/);
  assert.match(sourceOf('renderAssetRow'), /remove\.setAttribute\('aria-label', `Remove \$\{view\.name\}`\)/);
});

test('sealing, the list and its per-file actions are gone', () => {
  for (const [file, text] of [['index.html', html], ['app.js', javascript], ['styles.css', css]]) {
    for (const dead of ['assetSealInput', 'asset-seal', 'seal=1', 'holesFilled', 'report.sealed', 'assetList', '"assetCount"', '#assetCount\'', 'asset-list', 'assets-empty', 'asset-action', 'asset-facts', 'renderAssetList', 'downloadAsset', 'showAssetResult', 'ASSET_MAX_COUNT']) {
      assert.ok(!text.includes(dead), `${file} still has ${dead}`);
    }
  }
});

test('an unknown expiry says so and starts no timer', () => {
  const { nodes, show, page } = rowPage();
  show([{ name: 'scan.stl' }]);
  assert.equal(nodes['#assetCountdown'].textContent, 'expiry unknown');
  assert.equal(page.timerStarts, 0);
});

test('the countdown runs on the server clock from the asset list', () => {
  assert.match(sourceOf('loadAssets'), /assetClockOffsetMs = Number\(payload\.serverTime\) - Date\.now\(\)/);
  assert.match(sourceOf('renderAssetRow'), /const now = assetNow\(\);/);
  assert.match(sourceOf('uploadAsset'), /currentAssetUpload\(designAssets, assetNow\(\)\)/);
});
