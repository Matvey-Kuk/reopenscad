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

const env = { PANEL_LIST_MIN: 56, PANEL_LIST_FLOOR: 28, PANEL_OBJECTS_SHARE: 0.4, PANEL_KEY_STEP: 14 };
const load = (name) => new Function('env', `with (env) { return (${sourceOf(name)}); }`)(env);
env.panelListLayout = load('panelListLayout');
const layoutOf = env.panelListLayout;
const resize = load('resizePanelList');
const parse = load('parsePanelHeights');
const keyTarget = load('panelKeyTarget');
const none = { objects: null, revisions: null };

test('defaults: Objects fits up to 40% of the pane, Revisions takes the rest', () => {
  const few = layoutOf({ paneHeight: 700, free: 400, objectsContent: 56, revisionsContent: 1000 }, none);
  assert.equal(few.objects, 56, 'a short list is as tall as its rows');
  assert.equal(few.revisions, 344);
  const many = layoutOf({ paneHeight: 700, free: 400, objectsContent: 784, revisionsContent: 1000 }, none);
  assert.equal(many.objects, 280, 'a long one stops at 40%, so the tools stay in sight');
  assert.equal(many.revisions, 120);
  const shortHistory = layoutOf({ paneHeight: 700, free: 400, objectsContent: 56, revisionsContent: 48 }, none);
  assert.equal(shortHistory.objects, 56);
  assert.equal(shortHistory.revisions, 344, 'Revisions fills the room, however few its rows');
});

test('limits: two rows each, down to one in a short window, and the pane never scrolls', () => {
  const metrics = { paneHeight: 700, free: 300, objectsContent: 784, revisionsContent: 1000 };
  assert.equal(layoutOf(metrics, { objects: 10, revisions: null }).objects, 56);
  assert.equal(layoutOf(metrics, { objects: 5000, revisions: null }).objects, 244, 'room is left for Revisions at its minimum');
  assert.equal(layoutOf(metrics, { objects: 244, revisions: 5000 }).revisions, 56);
  const cramped = layoutOf({ paneHeight: 200, free: 80, objectsContent: 784, revisionsContent: 1000 }, none);
  assert.equal(cramped.objects + cramped.revisions, 80, 'a short window takes the lists past two rows');
  assert.ok(cramped.revisions >= 28);
  const tiny = layoutOf({ paneHeight: 100, free: -40, objectsContent: 784, revisionsContent: 1000 }, none);
  assert.deepEqual([tiny.objects, tiny.revisions], [28, 28], 'one row each is the floor; the tools give way below it');
  // A stored height outlives a window or design too small for it.
  const stored = { objects: 300, revisions: null };
  assert.equal(layoutOf({ ...metrics, free: 100 }, stored).objects, 72);
  assert.equal(layoutOf(metrics, stored).objects, 244);
  assert.equal(layoutOf({ ...metrics, free: 500 }, stored).objects, 300);
});

test('dragging Objects pushes Revisions; dragging Revisions takes from Objects and gives it back', () => {
  const metrics = { paneHeight: 700, free: 400, objectsContent: 784, revisionsContent: 1000 };
  const grown = resize('objects', 300, none, metrics);
  assert.equal(grown.objects, 300);
  assert.equal(layoutOf(metrics, grown).revisions, 100);
  assert.equal(resize('objects', 9000, none, metrics).objects, 344, 'stops at the limit');
  const start = { objects: 280, revisions: null };
  const up = resize('revisions', 200, start, metrics);
  assert.deepEqual(up, { objects: 200, revisions: 200 });
  const back = resize('revisions', 100, start, metrics);
  assert.deepEqual(back, { objects: 280, revisions: 100 }, 'same gesture: Objects returns to its height');
  assert.equal(resize('revisions', 9000, start, metrics).revisions, 344);
  // A smaller Revisions stays small, leaving the room empty below it.
  assert.equal(layoutOf({ ...metrics, objectsContent: 56 }, { objects: null, revisions: 90 }).revisions, 90);
});

test('persistence: only sane numbers are believed', () => {
  assert.deepEqual(parse('{"objects":200,"revisions":120}'), { objects: 200, revisions: 120 });
  assert.deepEqual(parse('{"objects":"x","revisions":-3}'), none);
  assert.deepEqual(parse('not json'), none);
  assert.deepEqual(parse(null), none);
  assert.match(javascript, /const PANEL_HEIGHTS_STORAGE_KEY = 'reopenscad\.panelHeights';/);
  const binding = sourceOf('bindPanelDivider');
  assert.match(binding, /if \(drag\.moved\) savePanelHeights\(\);/, 'a click without a move stores nothing');
  assert.match(binding, /'keydown'[\s\S]*savePanelHeights\(\);/);
});

test('double-click resets that divider alone', () => {
  const binding = sourceOf('bindPanelDivider');
  assert.match(binding, /'dblclick', \(\) => \{\s*panelHeights = \{ \.\.\.panelHeights, \[which\]: null \};\s*savePanelHeights\(\);/);
});

test('keyboard: the divider moves the way the arrow points; Home and End go to the limits', () => {
  const layout = { objects: 100, objectsMin: 56, objectsMax: 300, revisions: 80, revisionsMin: 56, revisionsMax: 200 };
  assert.equal(keyTarget('objects', 'ArrowDown', layout), 114);
  assert.equal(keyTarget('objects', 'ArrowUp', layout), 86);
  assert.equal(keyTarget('revisions', 'ArrowUp', layout), 94);
  assert.equal(keyTarget('revisions', 'ArrowDown', layout), 66);
  assert.equal(keyTarget('objects', 'Home', layout), 56);
  assert.equal(keyTarget('revisions', 'End', layout), 200);
  assert.equal(keyTarget('objects', 'a', layout), null);
});

test('the dividers are separators that use the app\'s splitter and capture the pointer', () => {
  for (const [id, list] of [['objectsDivider', 'objectList'], ['revisionsDivider', 'revisionList']]) {
    const tag = html.match(new RegExp(`<div[^>]*id="${id}"[^>]*>`))[0];
    assert.match(tag, /class="splitter horizontal"/);
    assert.match(tag, /role="separator"/);
    assert.match(tag, /aria-orientation="horizontal"/);
    assert.match(tag, /tabindex="0"/);
    assert.match(tag, new RegExp(`aria-controls="${list}"`));
  }
  assert.match(sourceOf('applyPanelLayout'), /aria-valuenow[\s\S]*aria-valuemin[\s\S]*aria-valuemax/);
  const binding = sourceOf('bindPanelDivider');
  assert.match(binding, /setPointerCapture\(event\.pointerId\)/);
  assert.match(binding, /event\.preventDefault\(\);/);
  assert.match(css, /\.splitter\.horizontal \{[^}]*cursor: row-resize;[^}]*user-select: none;/);
  assert.match(css, /\.object-list, \.revision-list \{ flex: none; height: var\(--list-height, auto\); overflow: hidden auto;/);
  // Only one frame of layout however many moves.
  assert.match(sourceOf('schedulePanelLayout'), /panelLayoutFrame \?\?= requestAnimationFrame\(applyPanelLayout\)/);
});

test('the pane never scrolls: only the two lists, and the tools as the last resort', () => {
  assert.match(css, /\.objects-pane \{ grid-column: 1; grid-row: 1 \/ 3;[^}]*overflow: hidden;/);
  assert.match(css, /^\.tools \{ flex: 0 1 auto; min-height: 0; overflow: hidden auto; \}/m);
  assert.match(sourceOf('measurePanelLists'), /\$\('#panelTools'\)\.scrollHeight/, 'natural size, even while squeezed');
});

// The reported bug: "the divider barely works". With 2 parts Objects' max was its content, so
// that divider was frozen (min = max = 56), and Revisions' list shrank from the bottom with its
// divider left where it was.
test('regression: with few parts both dividers have room to move, and each follows the pointer', () => {
  const metrics = { paneHeight: 780, free: 448, objectsContent: 56, revisionsContent: 480 };
  const start = layoutOf(metrics, none);
  assert.deepEqual([start.objects, start.revisions], [56, 392], 'the default still fits the parts');
  assert.ok(start.objectsMax > start.objects, 'Objects can grow past its content');
  assert.equal(start.objectsMax, 392);
  const grown = resize('objects', 156, none, metrics);
  assert.equal(layoutOf(metrics, grown).objects, 156);
  assert.equal(layoutOf(metrics, grown).revisions, 292, 'taken from Revisions');
  // Up from the default: Revisions has to take it from Objects, down to its minimum.
  assert.deepEqual([start.revisionsMin, start.revisionsMax], [56, 392]);
  assert.match(css, /#revisionsDivider \{ margin-top: auto; \}/, 'so shrinking Revisions moves its divider down with the pointer');
});

test('the room is measured again when a tool grows, even while the tools are squeezed', () => {
  assert.match(javascript, /for \(const tool of \$\$\('#panelTools > \.tool'\)\) panelResizeObserver\.observe\(tool\);/);
});
