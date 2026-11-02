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

function loadFunction(name, scope = {}) {
  return new Function(...Object.keys(scope), `${sourceOf(name)}; return ${name};`)(...Object.values(scope));
}

const rayTriangle = loadFunction('rayTriangle');
const rayHitsBox = loadFunction('rayHitsBox');
const invertMatrix4 = loadFunction('invertMatrix4');
const viewRay = loadFunction('viewRay', { invertMatrix4 });

test('a ray hits a triangle from either side and misses outside it', () => {
  // Triangle in the z = 5 plane.
  const triangle = [0, 0, 5, 10, 0, 5, 0, 10, 5];
  assert.equal(rayTriangle([1, 1, 0], [0, 0, 1], ...triangle), 5);
  assert.equal(rayTriangle([1, 1, 10], [0, 0, -1], ...triangle), 5);
  assert.equal(rayTriangle([9, 9, 0], [0, 0, 1], ...triangle), null);
  // Behind the origin does not count.
  assert.equal(rayTriangle([1, 1, 6], [0, 0, 1], ...triangle), null);
  // Parallel to the plane.
  assert.equal(rayTriangle([1, 1, 0], [1, 0, 0], ...triangle), null);
});

test('the box test culls parts the ray cannot reach', () => {
  const ray = { origin: [0, 0, -10], direction: [0, 0, 1] };
  assert.equal(rayHitsBox(ray, [-1, -1, -1], [1, 1, 1]), true);
  assert.equal(rayHitsBox(ray, [2, 2, -1], [3, 3, 1]), false);
  assert.equal(rayHitsBox(ray, [-1, -1, -20], [1, 1, -15]), false, 'behind the eye');
});

test('the view ray unprojects through the inverse view-projection', () => {
  const identity = [1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1];
  assert.deepEqual(invertMatrix4(identity), identity);
  assert.equal(invertMatrix4(new Array(16).fill(0)), null);
  const scale = [2, 0, 0, 0, 0, 4, 0, 0, 0, 0, 1, 0, 1, 0, 0, 1];
  const inverse = invertMatrix4(scale);
  // inverse * scale = identity
  const product = Array.from({ length: 16 }, (_, k) => {
    const column = Math.floor(k / 4);
    const row = k % 4;
    let sum = 0;
    for (let i = 0; i < 4; i += 1) sum += inverse[i * 4 + row] * scale[column * 4 + i];
    return Math.round(sum * 1e9) / 1e9;
  });
  assert.deepEqual(product, identity);
  // With an identity "camera" the ray through the centre runs along +z from z = -1.
  const ray = viewRay(identity, 0, 0);
  assert.deepEqual(ray.origin, [0, 0, -1]);
  assert.deepEqual(ray.direction, [0, 0, 1]);
});

test('a comment records the line where its object starts, counting UTF-8 bytes', () => {
  const byteOffsetToStringIndex = loadFunction('byteOffsetToStringIndex');
  const sourceLineAt = loadFunction('sourceLineAt', { byteOffsetToStringIndex });
  const code = '// é\ncube(1);\nsphere(2);\n';
  assert.equal(sourceLineAt(code, 0), 1);
  // "// é\n" is 6 bytes: offset 6 is the start of line 2.
  assert.equal(sourceLineAt(code, 6), 2);
  assert.equal(sourceLineAt(code, code.indexOf('sphere') + 1), 3);
});

test('the event poll carries the feedback version and applies the summary', async () => {
  const urls = [];
  const summaries = [];
  const noop = () => {};
  let answer = { ok: true, status: 200, json: async () => ({ feedback: { version: 4 } }) };
  const scope = {
    eventPollTimer: null, eventPollController: null, pollLoopToken: 0, workspaceGeneration: 1,
    agentActivity: { design: 'happy-otter' }, workspaceId: 'happy-otter', workspaceRevision: 7, readOnlyToken: null,
    feedbackVersion: 3, agentActivityVersion: () => 0, assetsEnabled: () => false, assetsVersion: null, document: { hidden: false }, workspaceOnline: true,
    workspaceSavePromise: null, workspaceLocalOnly: false, workspaceFromPayload: () => ({}), workspacePollAction: () => 'none',
    setTimeout: () => 1, clearTimeout: noop, resetAgentActivity: noop, renderFeedback: noop, loadAssets: noop, refreshRevisions: noop,
    applyWorkspaceSettings: noop, applyAssetsSummary: noop, applyWorkspace: noop, applyActivitySummary: noop, refreshSyncState: noop,
    applyFeedbackSummary: (summary) => summaries.push(summary),
    fetch: async (url) => { urls.push(url); return answer; },
  };
  loadFunction('beginWorkspacePolling', scope)();
  await new Promise(setImmediate);
  assert.match(urls[0], /\/events\?since=7&feedback=3(&|$)/);
  assert.deepEqual(summaries, [{ version: 4 }]);
  // Nothing new: nothing to apply.
  answer = { ok: true, status: 204 };
  loadFunction('beginWorkspacePolling', scope)();
  await new Promise(setImmediate);
  assert.equal(urls.length, 2);
  assert.equal(summaries.length, 1);
});

test('switching designs drops the previous design\'s comments and comment mode', () => {
  const panel = { hidden: false };
  const composer = { hidden: false };
  const noop = () => {};
  const env = {
    workspaceGeneration: 1, workspaceLoadController: null, eventPollTimer: null, eventPollController: null, workspaceSaveDebounce: null,
    workspaceSaveController: null, workspaceSavePromise: null, displacedEdits: null, workspaceHasLocalChanges: false, workspaceSaveFailures: 0,
    workspaceLocalOnly: false, localPromotion: null, nextWorkspaceNavigation: 'push', activeRender: null,
    feedbackLoadToken: 0, feedbackLastSeq: 9, feedbackVersion: 3, feedbackComments: [{ id: 'c1' }], feedbackRequest: { id: 'r1' }, feedbackEndedRequestId: 'r0',
    activeCommentId: 'c1', pendingComment: { x: 1 }, commentMode: true,
    document: { getElementById: () => null }, clearTimeout: noop, cancelRender: noop, resetAssets: noop, resetRevisions: noop, hidePinCursor: noop, renderFeedback: noop,
    $: (selector) => (selector === '#feedbackPanel' ? panel : composer),
  };
  const loaded = (name) => new Function('env', `with (env) { return (${sourceOf(name)}); }`)(env);
  env.resetFeedback = loaded('resetFeedback');
  assert.equal(loaded('advanceWorkspaceGeneration')(), 2);
  assert.deepEqual(env.feedbackComments, []);
  assert.equal(env.commentMode, false);
  assert.equal(env.pendingComment, null);
  assert.equal(env.activeCommentId, null);
  assert.equal(env.feedbackRequest, null);
  assert.equal(env.feedbackVersion, 0);
  assert.equal(panel.hidden, true);
  assert.equal(composer.hidden, true);
});

test('comments exist only on editable designs, never on a read-only link', () => {
  const enabled = (workspaceId, readOnlyToken, workspaceLocalOnly) => loadFunction('feedbackEnabled', { workspaceId, readOnlyToken, workspaceLocalOnly })();
  assert.equal(enabled('happy-otter-1', null, false), true);
  assert.equal(enabled('happy-otter-1', 'share-token', false), false);
  assert.equal(enabled('happy-otter-1', null, true), false, 'a local-only design has no server to keep comments');
  assert.equal(enabled(null, null, false), false);
  // The button ships hidden; renderFeedback reveals it only when enabled.
  assert.match(html, /id="feedbackButton"[^>]* hidden>/);
  assert.match(javascript, /button\.hidden = !enabled;/);
});

test('request_pointing is a mode, not a form: a one-line hint and a way out', () => {
  const sync = javascript.slice(javascript.indexOf('function syncFeedbackRequest('), javascript.indexOf('async function loadFeedback('));
  assert.match(sync, /enterCommentMode\(\)/);
  assert.match(sync, /exitCommentMode\(\)/);
  // No Done / Cancel answer flow: the banner is the hint plus an exit.
  assert.doesNotMatch(html, /id="feedbackDone"|id="feedbackCancel"/);
  assert.match(html, /id="feedbackBanner"[^>]*>\s*<b id="feedbackBannerText">[^<]*<\/b>\s*<button[^>]*id="feedbackExit"/);
  assert.match(javascript, /Show the agent where: \$\{asked\.what\} — \$\{verb\} the model/);
  // Leaving the mode (x or Esc) tells a waiting agent it was dismissed.
  const leave = javascript.slice(javascript.indexOf('async function leaveCommentMode('), javascript.indexOf('function objectSourceLine('));
  assert.match(leave, /feedbackWrite\('\/dismiss', 'POST', \{ requestId: request\.id \}\)/);
  assert.match(javascript, /if \(!draft \|\| \(!text && !draft\.requestId\)\) return undefined;/, 'a bare pin answers a pointing request');
  // Saving that pin answers the request and leaves comment mode.
  const submit = javascript.slice(javascript.indexOf('async function submitComment('), javascript.indexOf('function cancelComposer('));
  assert.match(submit, /if \(draft\.requestId && feedbackRequest\?\.id === draft\.requestId\) \{[\s\S]*?exitCommentMode\(\);/);
  // Pre-filled as the box's value (not a placeholder), caret at the end.
  const place = javascript.slice(javascript.indexOf('function placeComment('), javascript.indexOf('async function submitComment('));
  assert.match(place, /if \(asked\) box\.value = `\$\{asked\.what \? `This point is an answer to the question: \$\{asked\.what\}` : "This point answers the agent's request"\} `;/);
  assert.match(place, /box\.setSelectionRange\(box\.value\.length, box\.value\.length\)/);
});

test('a click that turns into a drag does not drop a comment', () => {
  const block = javascript.slice(javascript.indexOf('const COMMENT_CLICK_SLOP'), javascript.indexOf('const MCP_MODERN_VERSION'));
  const page = ({ commentMode = true, fingers = 0 } = {}) => {
    const listeners = {};
    const env = {
      commentMode,
      touchPoints: { size: fingers },
      clock: 0,
      performance: { now: () => env.clock },
      placed: [],
      placeComment: (x, y) => env.placed.push([x, y]),
      renderCanvas: { addEventListener: (type, listener) => { listeners[type] = listener; } },
    };
    new Function('env', `with (env) { ${block} }`)(env);
    return {
      env,
      listeners,
      press: (event = {}) => listeners.pointerdown({ isPrimary: true, pointerType: 'mouse', button: 0, altKey: false, pointerId: 1, clientX: 100, clientY: 100, ...event }),
      release: (event = {}) => listeners.pointerup({ pointerId: 1, clientX: 100, clientY: 100, ...event }),
    };
  };
  const placedAfter = (act, options) => {
    const current = page(options);
    current.press();
    act(current);
    current.release();
    return current.env.placed.length;
  };
  const click = page();
  click.press();
  click.release({ clientX: 103, clientY: 104 });
  assert.deepEqual(click.env.placed, [[103, 104]], 'a press that stays put places a comment');

  const drag = page();
  drag.press();
  drag.release({ clientX: 110, clientY: 100 });
  assert.equal(drag.env.placed.length, 0, 'moved past the slop');

  assert.equal(placedAfter((current) => { current.env.clock = 801; }), 0, 'held too long');
  assert.equal(placedAfter((current) => current.listeners.pointercancel()), 0, 'cancelled');
  assert.equal(placedAfter(() => {}, { commentMode: false }), 0, 'out of comment mode');
  assert.equal(placedAfter(() => {}, { fingers: 1 }), 0, 'another finger is still down');

  const secondary = page();
  secondary.press({ button: 2 });
  secondary.release();
  assert.equal(secondary.env.placed.length, 0, 'the right button pans');
  const other = page();
  other.press();
  other.release({ pointerId: 2 });
  assert.equal(other.env.placed.length, 0, 'another pointer');
});

test('the comment UI fits a phone', () => {
  const mobile = css.slice(css.lastIndexOf('@media (max-width: 760px) and (pointer: coarse)'));
  // The banner is an in-flow strip (never over the viewport controls), below the viewport on phones.
  assert.match(mobile, /\.feedback-banner \{ order: 2;/);
  assert.match(html, /<section class="viewer-pane"[^>]*>\s*(?:<!--[\s\S]*?-->\s*)?<div class="feedback-banner" id="feedbackBanner"[\s\S]*?<\/div>\s*<div class="viewport" id="viewport">/);
  assert.match(mobile, /\.feedback-panel, [^{]*\{ top: 64px; left: 10px; right: 10px;/);
  // 16px stops iOS zooming the page when the composer takes focus.
  assert.match(mobile, /\.feedback-composer textarea \{ font-size: 16px; \}/);
  assert.match(css, /\.feedback-panel \{[^}]*top: 8px; right: 8px; width: min\(300px, calc\(100% - 16px\)\)/);
});

test("a pin's tip, its square bottom-left corner, sits exactly on the projected point", () => {
  // The element is placed with translate(x, y); its bottom-left corner must be that point,
  // or every pin drifts from where it was clicked (it was 11 px left, 4 px up).
  assert.match(css, /\.feedback-pin \{[^}]*height: 22px; margin: -22px 0 0 0;[^}]*border-radius: 11px 11px 11px 0;/);
  const mobile = css.slice(css.lastIndexOf('@media (max-width: 760px) and (pointer: coarse)'));
  assert.match(mobile, /\.feedback-pin \{[^}]*height: 28px; margin: -28px 0 0 0;/);
});

test('in comment mode a mouse carries the next pin as its cursor, tip at the pointer', () => {
  const show = javascript.slice(javascript.indexOf('function showPinCursor('), javascript.indexOf('function hidePinCursor('));
  // Same element as a placed pin, positioned exactly like one (translate to the point).
  assert.match(html, /<span class="feedback-pin ghost" id="feedbackPinCursor" aria-hidden="true" hidden><\/span>/);
  assert.match(show, /translate\(\$\{clientX - layer\.left\}px, \$\{clientY - layer\.top\}px\)/);
  assert.match(show, /nextCommentNumber\(\)/);
  assert.match(css, /\.viewport\.commenting\.ghosting \.render-canvas \{ cursor: none; \}/);
  assert.match(css, /\.viewport\.commenting \.render-canvas:active \{ cursor: grabbing; \}/);
  // Touch and drags never get the ghost; leaving the mode removes it.
  assert.match(javascript, /if \(!commentMode \|\| event\.pointerType === 'touch' \|\| event\.buttons !== 0\) \{ hidePinCursor\(\); return; \}/);
  const exit = javascript.slice(javascript.indexOf('function exitCommentMode('), javascript.indexOf('function feedbackNotice('));
  assert.match(exit, /hidePinCursor\(\)/);
  const next = loadFunction('nextCommentNumber', { feedbackComments: [{ id: 'c3' }, { id: 'c7' }], feedbackLastSeq: 9, commentNumber: (comment) => comment.id.slice(1) });
  assert.equal(next(), 10, 'numbers are never reused');
});
