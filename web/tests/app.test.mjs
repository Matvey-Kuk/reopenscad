import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const html = await readFile(new URL('../index.html', import.meta.url), 'utf8');
const javascript = await readFile(new URL('../app.js', import.meta.url), 'utf8');
const css = await readFile(new URL('../styles.css', import.meta.url), 'utf8');

test('every statically referenced element exists in the application shell', () => {
  const declaredIds = new Set([...html.matchAll(/\sid="([^"]+)"/g)].map((match) => match[1]));
  const referencedIds = new Set([...javascript.matchAll(/\$\('#([A-Za-z][\w-]*)/g)].map((match) => match[1]));
  const missing = [...referencedIds].filter((id) => !declaredIds.has(id));
  assert.deepEqual(missing, []);
});

test('toolbar icon names have matching SVG definitions', () => {
  const requestedIcons = new Set([...html.matchAll(/data-icon="([^"]+)"/g)].map((match) => match[1]));
  const iconBlock = javascript.slice(javascript.indexOf('const icons = {'), javascript.indexOf("$$('[data-icon]')"));
  const definedIcons = new Set([...iconBlock.matchAll(/^\s{2}([a-z]+):/gm)].map((match) => match[1]));
  const missing = [...requestedIcons].filter((icon) => !definedIcons.has(icon));
  assert.deepEqual(missing, []);
});

test('the viewport uses the WebGL canvas mesh path', () => {
  assert.match(html, /<canvas id="renderCanvas"/);
  assert.match(javascript, /getContext\('webgl'/);
  assert.match(javascript, /meshViewport\.setMesh\(data\.mesh, sceneObjects\)/);
  assert.doesNotMatch(javascript, /renderImage|data:image\/png;base64/);
});

test('web shell omits desktop menus and multi-file tab chrome', () => {
  assert.doesNotMatch(html, /data-menu=|class="menus"|menu-popover/);
  assert.doesNotMatch(html, /class="tab active"/);
  // The editor pane carries no header chrome at all: one source buffer, no
  // file name, no tab strip, no dead "editor settings" affordance.
  assert.doesNotMatch(html, /class="file-heading"|id="editorSettings"/);
  assert.equal(html.match(/<textarea[^>]*id="codeEditor"/g).length, 1);
});

test('scene inspection controls expose objects, visibility, selection, and tolerance', () => {
  assert.match(html, /id="objectList"[^>]*role="listbox"/);
  assert.match(html, /id="toleranceInput"[^>]*type="number"/);
  assert.match(html, /id="checkIntersections"/);
  assert.match(javascript, /class="visibility-toggle"/);
  assert.match(javascript, /selectedObjectId/);
  assert.match(javascript, /hiddenObjectIds/);
  assert.match(javascript, /\/intersections/);
});

test('workspace client persists revisions and listens for AI updates', () => {
  assert.match(javascript, /fetch\('\/api\/workspaces'/);
  assert.match(javascript, /method: 'PATCH'/);
  assert.match(javascript, /\/events\?since=/);
  assert.match(javascript, /source === 'ai'/);
  assert.match(javascript, /updatedRanges/);
  assert.match(html, /id="aiRevisionBadge"/);
});

test('unchanged polls and local edits cannot overwrite each other', () => {
  const polling = javascript.slice(javascript.indexOf('function beginWorkspacePolling'), javascript.indexOf('function normalizeObjects'));
  assert.match(polling, /payload\.changed === true/);
  assert.ok(polling.indexOf('payload.changed === true') < polling.indexOf('applyWorkspace(payload, { remote: true })'));
  assert.match(javascript, /remote && !force[^\n]+hasPendingLocalWorkspaceChanges\(\)/);
  assert.match(javascript, /surfaceWorkspaceConflict\(payload\)/);
  assert.match(javascript, /workspace-conflict-resolver/);
  assert.doesNotMatch(javascript, /window\.confirm/);
});

test('render and export flush source and bind requests to a revision', () => {
  const rendering = javascript.slice(javascript.indexOf("async function render(mode"), javascript.indexOf('function cancelRender'));
  const exporting = javascript.slice(javascript.indexOf('async function exportModel'), javascript.indexOf('function updateCustomizer'));
  assert.match(rendering, /await flushWorkspaceSave\(\)/);
  assert.match(rendering, /revision: workspaceRevision/);
  assert.match(rendering, /requestId: request\.id/);
  assert.match(rendering, /activeRender !== request/);
  assert.match(exporting, /await flushWorkspaceSave\(\)/);
  assert.match(exporting, /revision: workspaceRevision/);
  assert.match(exporting, /scope === 'selected'/);
  assert.match(html, /data-format="stl" data-scope="selected"/);
});

test('exports never filter on placeholder object IDs', () => {
  const selecting = javascript.slice(javascript.indexOf('function exportObjectIds'), javascript.indexOf('async function exportModel'));
  assert.match(javascript, /placeholder: true/);
  assert.match(selecting, /sceneObjects\.some\(\(object\) => object\.placeholder\)\) return null/);
  assert.match(selecting, /visible\.length === sceneObjects\.length \? null/);
  const exporting = javascript.slice(javascript.indexOf('async function exportModel'), javascript.indexOf('function updateCustomizer'));
  assert.match(exporting, /\.\.\.\(objectIds \? \{ objectIds \} : \{\}\)/);
});

test('async workspace operations are generation scoped', () => {
  assert.match(javascript, /function advanceWorkspaceGeneration\(\)/);
  assert.match(javascript, /generation !== workspaceGeneration \|\| id !== workspaceId/);
  assert.match(javascript, /eventPollController\?\.abort\(\)/);
  assert.match(javascript, /workspaceSaveController\?\.abort\(\)/);
});

test('unknown legacy workspace URLs are migrated instead of stranded locally', () => {
  const loading = javascript.slice(javascript.indexOf('async function loadWorkspace'), javascript.indexOf('function scheduleWorkspaceSave'));
  assert.match(loading, /\?allowMissing=1/);
  assert.match(loading, /response\.ok && payload\.missing === true/);
  assert.doesNotMatch(loading, /response\.status === 404/);
  assert.match(loading, /localStorage\.getItem\(workspaceStorageKey\(id\)\)/);
  // A migrated workspace keeps its own cached source and never falls back to a seeded example.
  assert.match(loading, /await createWorkspace\(cached\?\.code \|\| ''\)/);
});

test('the workspace is the document: no file name is surfaced anywhere', () => {
  assert.doesNotMatch(html, /Untitled/);
  assert.doesNotMatch(javascript, /Untitled/);
  assert.doesNotMatch(html, /id="documentName"|id="editorTabName"|class="document-name"/);
  assert.doesNotMatch(javascript, /document\.title\s*=/);
  // Downloads still need a real file name on disk.
  assert.match(javascript, /function downloadFileName\(extension\)/);
  assert.match(javascript, /downloadFileName\('scad'\)/);
});

test('examples are kick-start options above an empty editor, not a toolbar dropdown', () => {
  assert.doesNotMatch(html, /example-picker|id="exampleSelect"/);
  assert.doesNotMatch(css, /example-picker/);
  assert.match(html, /id="kickstart"/);
  assert.match(html, /id="kickstartGrid"/);
  // The grid sits inside the editor pane, above the edit field.
  const editorPane = html.slice(html.indexOf('class="editor-pane"'), html.indexOf('class="splitter vertical"'));
  assert.match(editorPane, /id="kickstart"/);
  assert.match(javascript, /const kickstartOptions = \[/);
  assert.match(javascript, /function updateKickstart\(\)[\s\S]*?editor\.value\.trim\(\)\.length > 0/);
  assert.match(javascript, /data-example=/);
  // A new workspace must actually be empty, or the kick-start options can never appear.
  assert.doesNotMatch(javascript, /createWorkspace\(examples\.starter/);
  assert.match(javascript, /async function createWorkspace\(code = ''/);
});

test('intersection tolerance defaults to 0.2 mm, persists, and never coerces to zero', () => {
  assert.match(html, /id="toleranceInput"[^>]*value="0\.2"/);
  assert.match(javascript, /const DEFAULT_TOLERANCE = 0\.2;/);
  assert.match(javascript, /localStorage\.setItem\(TOLERANCE_STORAGE_KEY/);
  assert.match(javascript, /function restoreTolerance\(\)/);
  assert.doesNotMatch(javascript, /Number\(\$\('#toleranceInput'\)\.value\) \|\| 0/);
  const checking = javascript.slice(javascript.indexOf('async function checkIntersections'), javascript.indexOf("$('#checkIntersections').addEventListener"));
  assert.match(checking, /const tolerance = readTolerance\(\)/);
  assert.match(checking, /tolerance === null/);
  // The server recompiles the STORED source, so pending edits must land first.
  assert.ok(checking.indexOf('await flushWorkspaceSave()') < checking.indexOf('/intersections'));
});

test('offline workspace ids satisfy the server id rules and history stays navigable', () => {
  const generator = javascript.slice(javascript.indexOf('function generateFunnyId'), javascript.indexOf('function setSyncState'));
  assert.match(generator, /randomToken\(12\)/);
  assert.match(generator, /id\.length >= 16/);
  assert.doesNotMatch(generator, /Math\.random\(\)\.toString\(36\)/);
  assert.match(javascript, /history\.pushState\(\{ workspaceId: id \}/);
  assert.match(javascript, /addEventListener\('popstate'/);
  assert.match(javascript, /createWorkspace\('', \{ navigation: 'push' \}\)/);
});

test('a failed workspace save retries with backoff instead of waiting for the next keystroke', () => {
  const saving = javascript.slice(javascript.indexOf('async function saveWorkspace'), javascript.indexOf('async function flushWorkspaceSave'));
  assert.match(saving, /workspaceSaveFailures \+= 1/);
  assert.match(saving, /Math\.min\(15000, 800 \* 2 \*\* \(workspaceSaveFailures - 1\)\)/);
  assert.match(saving, /scheduleWorkspaceSave\(backoff\)/);
  assert.match(javascript, /function scheduleWorkspaceSave\(delay = 650\)/);
});

test('no unscoped source cache can leak one workspace into another', () => {
  const writes = [...javascript.matchAll(/localStorage\.setItem\('reopenscad\.code'/g)];
  assert.deepEqual(writes, []);
  assert.doesNotMatch(javascript, /localStorage\.getItem\('reopenscad\.code'\)/);
  assert.match(javascript, /localStorage\.removeItem\('reopenscad\.code'\)/);
});

test('only the final AI revision supplies source highlights', () => {
  const selection = javascript.slice(javascript.indexOf('function finalAiUpdate'), javascript.indexOf('function surfaceWorkspaceConflict'));
  assert.match(selection, /finalEvent\.source !== 'ai'/);
  assert.match(selection, /event\.source === 'ai'/);
  assert.match(selection, /event\.revision.*finalEvent\.revision/);
  const applying = javascript.slice(javascript.indexOf('function applyWorkspace'), javascript.indexOf('function byteOffsetToStringIndex'));
  assert.ok(applying.indexOf('workspaceSavedCode =') < applying.indexOf('if (shouldRenderAiUpdate) render'));
});

test('the ground surface is real scene geometry, not a CSS backdrop', () => {
  // The old decorative grid was a div with a faked perspective transform: it could not
  // orbit, sit at Z=0, or mean anything dimensionally.
  assert.doesNotMatch(html, /viewport-grid/);
  assert.doesNotMatch(css, /viewport-grid/);
  assert.match(javascript, /drawGround\(mvp, metrics\)/);
  // The buffer takes the whole metrics record: a rectangular bed needs both half-counts
  // plus the bed rectangle, none of which fit through a single `cells` number.
  assert.match(javascript, /ensureGroundBuffer\(metrics\)/);
  // It reuses the shared world-space line pass rather than a second GL pipeline.
  const drawing = javascript.slice(javascript.indexOf('  drawGround(mvp, metrics) {'), javascript.indexOf('  drawGroundLabels(metrics) {'));
  assert.match(drawing, /this\.beginLinePass\(this\.multiply\(mvp, this\.scaleMatrix\(metrics\.spacing\)\)\)/);
  assert.equal((drawing.match(/this\.drawLineSegments\(/g) || []).length, 5, 'minor, major, origin, build volume and bed outline are drawn separately');
});

test('grid divisions snap to a human-readable 1-2-5 progression', () => {
  const source = javascript.slice(javascript.indexOf('function niceStep('), javascript.indexOf('// Trims float noise'));
  const niceStep = new Function(`${source}\nreturn niceStep;`)();
  assert.equal(niceStep(0.9), 1);
  assert.equal(niceStep(1.8), 2);
  assert.equal(niceStep(4), 5);
  assert.equal(niceStep(9), 10);
  assert.equal(niceStep(26), 20);
  assert.equal(niceStep(60), 50);
  assert.equal(niceStep(0.3), 0.2);
  assert.equal(niceStep(0), 1);
  assert.equal(niceStep(Number.NaN), 1);
});

test('the grid step is derived from the model bounds and the visible frame', () => {
  const extents = javascript.slice(javascript.indexOf('  groundExtents() {'), javascript.indexOf('  ensureGroundBuffer('));
  assert.match(extents, /niceStep\(span \/ GROUND_TARGET_CELLS\)/);
  const span = javascript.slice(javascript.indexOf('  groundSpan() {'), javascript.indexOf('  groundExtents() {'));
  assert.match(span, /this\.bounds/);
  assert.match(span, /this\.viewSpan\(\) \* 1\.3/);
  // The bounds still come from the decoded geometry itself — now as the union of the
  // per-object batch extents rather than a second decode of the same triangles.
  const setMesh = javascript.slice(javascript.indexOf('  setMesh(encoded, objects = []) {'), javascript.indexOf('  multiply(a, b) {'));
  assert.match(setMesh, /min\[axis\] = Math\.min\(min\[axis\], batch\.min\[axis\]\)/);
  assert.match(setMesh, /max\[axis\] = Math\.max\(max\[axis\], batch\.max\[axis\]\)/);
  assert.match(setMesh, /this\.bounds = \{ min, max \}/);
  // Major lines every Nth division give the grid a visual hierarchy.
  assert.match(javascript, /const GROUND_MAJOR_EVERY = 5;/);
});

test('grid values are labelled in millimetres as upright screen-space text', () => {
  assert.match(html, /id="groundLabels"/);
  assert.match(html, /id="gridScale"/);
  const labels = javascript.slice(javascript.indexOf('  drawGroundLabels(metrics) {'), javascript.indexOf('  gridlineAnchor(from, to, margin) {'));
  // 2D canvas text via the world->screen helper: it can never be mirrored or upside down.
  assert.match(labels, /this\.gridlineAnchor\(candidate\.from, candidate\.to, margin\)/);
  assert.match(labels, /context\.fillText\(candidate\.text/);
  assert.match(labels, /formatMillimetres\(metrics\.spacing\)\} mm/);
  assert.match(javascript, /gridlineAnchor\(from, to, margin\) \{/);
  assert.match(javascript, /const projected = this\.projectPoint\(point\)|const start = this\.projectPoint\(from\)/);
  // Points behind the eye project mirrored and must never anchor a label.
  assert.match(javascript, /behind: clip\[3\] < 0/);
  assert.match(javascript, /start\.behind \|\| end\.behind/);
});

test('the ground sits under the model: depth tested and biased off Z=0', () => {
  const drawEnd = javascript.slice(javascript.indexOf('    if (this.showGround) this.drawGround'), javascript.indexOf('  projectPoint(point) {'));
  assert.match(drawEnd, /if \(this\.showGround\) this\.drawGround\(mvp, metrics\)/);
  // No gl.disable(gl.DEPTH_TEST) anywhere: world datums are occluded by real geometry.
  assert.doesNotMatch(javascript, /disable\(gl\.DEPTH_TEST\)/);
  // A face lying exactly on the plane must win the depth test instead of z-fighting.
  assert.match(javascript, /const GROUND_DEPTH_BIAS = 0\.002;/);
  const buffer = javascript.slice(javascript.indexOf('  ensureGroundBuffer(metrics) {'), javascript.indexOf('  drawGround(mvp, metrics) {'));
  assert.match(buffer, /-GROUND_DEPTH_BIAS/);
});

test('the ground plane has a toggle whose preference outlives the workspace', () => {
  assert.match(html, /id="groundToggle"[^>]*type="checkbox" checked/);
  assert.match(javascript, /const GROUND_STORAGE_KEY = 'reopenscad\.ground';/);
  assert.match(javascript, /function restoreGroundPreference\(\)/);
  assert.match(javascript, /localStorage\.setItem\(GROUND_STORAGE_KEY/);
  const restoring = javascript.slice(javascript.indexOf('function restoreGroundPreference()'), javascript.indexOf("$('#groundToggle').addEventListener"));
  assert.match(restoring, /stored === null \? true : stored === 'true'/);
  const bootstrapping = javascript.slice(javascript.indexOf('async function bootstrap()'), javascript.indexOf('// Back/forward moves between workspaces'));
  assert.match(bootstrapping, /restoreGroundPreference\(\)/);
});

test('a printer build plate can bound the same surface later', () => {
  // The plate task swaps a bounded, labelled bed in where the model bbox is read today.
  assert.match(javascript, /this\.plate = null;/);
  const span = javascript.slice(javascript.indexOf('  groundSpan() {'), javascript.indexOf('  groundExtents() {'));
  assert.match(span, /if \(this\.plate\) return Math\.max\(this\.plate\.size\[0\], this\.plate\.size\[1\]\)/);
});

// --- 3D printer build plate ---------------------------------------------------------
// The catalogue and the fit test are pure functions, so they are exercised for real rather
// than pattern-matched: a wrong build volume is the one failure mode that silently misleads.
const plateModule = new Function(`${javascript.slice(
  javascript.indexOf('const PRINTER_PLATES = ['),
  javascript.indexOf('  return { fits: over.length === 0, over, size };\n}') + '  return { fits: over.length === 0, over, size };\n}'.length,
)}\nreturn { PRINTER_PLATES, findPlate, plateFit };`)();

test('the build plate catalogue covers the brands the spec names, with sane volumes', () => {
  const { PRINTER_PLATES, findPlate } = plateModule;
  const brands = PRINTER_PLATES.map((group) => group.brand);
  for (const required of ['Bambu Lab', 'Creality', 'Prusa']) assert.ok(brands.includes(required), `${required} missing`);
  const models = PRINTER_PLATES.flatMap((group) => group.models);
  assert.ok(models.length >= 20, 'a useful catalogue, not a token three');
  const ids = new Set();
  for (const model of models) {
    assert.equal(ids.has(model.id), false, `duplicate plate id ${model.id}`);
    ids.add(model.id);
    // The server sanitizes plate ids to this charset; an id outside it would be silently
    // dropped on save and the plate would not survive a reload.
    assert.match(model.id, /^[a-z0-9_-]{1,64}$/, `${model.id} would not survive sanitize_plate`);
    assert.equal(model.size.length, 3, `${model.id} needs X, Y and Z`);
    for (const value of model.size) {
      assert.ok(Number.isFinite(value) && value >= 100 && value <= 1000, `${model.id} has an implausible dimension ${value}`);
    }
  }
  // Spot-check three published build volumes, one per named brand.
  assert.deepEqual(findPlate('bambu-x1').size, [256, 256, 256]);
  assert.deepEqual(findPlate('prusa-mk4s').size, [250, 210, 220]);
  assert.deepEqual(findPlate('creality-k1-max').size, [300, 300, 300]);
  assert.equal(findPlate(''), null);
  assert.equal(findPlate('not-a-printer'), null);
});

test('the fit check measures the model, not where it sits', () => {
  const { plateFit, findPlate } = plateModule;
  const plate = findPlate('prusa-mk4s'); // 250 x 210 x 220
  // Centred on the origin is the OpenSCAD norm: half the part is below Z=0 and it still fits.
  assert.equal(plateFit(plate, { min: [-20, -12, -9], max: [20, 12, 9] }).fits, true);
  // Exactly the bed is a fit, not a miss.
  assert.equal(plateFit(plate, { min: [0, 0, 0], max: [250, 210, 220] }).fits, true);
  const over = plateFit(plate, { min: [0, 0, 0], max: [300, 100, 400] });
  assert.equal(over.fits, false);
  assert.deepEqual(over.over, ['X', 'Z'], 'names the axes that actually overflow');
  assert.deepEqual(over.size, [300, 100, 400]);
  // No plate, or nothing rendered yet, is "no verdict" rather than a false pass.
  assert.equal(plateFit(null, { min: [0, 0, 0], max: [1, 1, 1] }), null);
  assert.equal(plateFit(plate, null), null);
});

test('a selected bed drives the surface per axis and draws its own outline', () => {
  const extents = javascript.slice(javascript.indexOf('  groundExtents() {'), javascript.indexOf('  ensureGroundBuffer('));
  // Rectangular beds: each axis gets its own half-count, off the bed dimension not the span.
  assert.match(extents, /cellsX = this\.plate \? cellsFor\(this\.plate\.size\[0\], false\)/);
  assert.match(extents, /cellsY = this\.plate \? cellsFor\(this\.plate\.size\[1\], false\)/);
  assert.match(extents, /this\.plate\.size\[0\] \/ 2 \/ spacing/);
  const buffer = javascript.slice(javascript.indexOf('  ensureGroundBuffer(metrics) {'), javascript.indexOf('  drawGround(mvp, metrics) {'));
  // The bed rectangle and the build-volume box are separate ranges in the same buffer.
  assert.match(buffer, /const bed = \[\];/);
  assert.match(buffer, /const volume = \[\];/);
  const drawing = javascript.slice(javascript.indexOf('  drawGround(mvp, metrics) {'), javascript.indexOf('  drawGroundLabels(metrics) {'));
  // The bed edge is a datum, so it shares the origin line colour.
  assert.match(drawing, /geometry\.bed\.count.*0\.416, 0\.463, 0\.514/);
  // Labels follow the rectangle rather than a single square extent.
  const labels = javascript.slice(javascript.indexOf('  drawGroundLabels(metrics) {'), javascript.indexOf('  gridlineAnchor(from, to, margin) {'));
  assert.match(labels, /metrics\.extentY/);
  assert.match(labels, /metrics\.extentX/);
});

test('the plate is optional, defaults to none, and is offered grouped by brand', () => {
  assert.match(html, /id="plateSelect"/);
  // No hard-coded options: the catalogue is the single source of truth for both.
  assert.doesNotMatch(html, /<option[^>]*value="bambu/);
  const populating = javascript.slice(javascript.indexOf('function populatePlateSelect()'), javascript.indexOf('// Reflects the current bed'));
  assert.match(populating, /none\.value = '';/);
  assert.match(populating, /createElement\('optgroup'\)/);
  assert.match(populating, /optgroup\.label = group\.brand/);
  const restoring = javascript.slice(javascript.indexOf('function restorePlatePreference()'), javascript.indexOf("$('#plateSelect').addEventListener"));
  // Nothing stored means no plate, so the viewport opens on the model-driven grid.
  assert.match(restoring, /localStorage\.getItem\(PLATE_STORAGE_KEY\) \|\| ''/);
});

test('the plate and tolerance persist in the workspace AND into new workspaces', () => {
  // localStorage: carried into every NEW workspace.
  assert.match(javascript, /const PLATE_STORAGE_KEY = 'reopenscad\.plate';/);
  assert.match(javascript, /localStorage\.setItem\(PLATE_STORAGE_KEY/);
  const creating = javascript.slice(javascript.indexOf('async function createWorkspace('), javascript.indexOf('async function loadWorkspace('));
  assert.match(creating, /plate: localStorage\.getItem\(PLATE_STORAGE_KEY\) \|\| ''/);
  // `|| DEFAULT_TOLERANCE` would resurrect 0.2 from a deliberate 0 mm clearance.
  assert.match(creating, /tolerance: readTolerance\(\) \?\? DEFAULT_TOLERANCE/);
  assert.doesNotMatch(creating, /Number\(localStorage\.getItem\(TOLERANCE_STORAGE_KEY\)\) \|\|/);

  // Server side: travels with the workspace URL.
  const settings = javascript.slice(javascript.indexOf('function saveWorkspaceSettings()'), javascript.indexOf('function scheduleWorkspaceSave('));
  assert.match(settings, /method: 'PATCH'/);
  assert.match(settings, /plate: meshViewport\.plate \? meshViewport\.plate\.id : ''/);
  assert.match(settings, /tolerance: readTolerance\(\) \?\? DEFAULT_TOLERANCE/);
  // Settings are not an edit: no baseRevision, so they can never conflict with a code save.
  assert.doesNotMatch(settings, /baseRevision/);
  const applying = javascript.slice(javascript.indexOf('function applyWorkspaceSettings('), javascript.indexOf('function applyWorkspace('));
  assert.match(applying, /typeof workspace\.plate === 'string'/);
  assert.match(applying, /Number\(workspace\.tolerance\)/);
  assert.match(javascript, /applyWorkspaceSettings\(workspace\);/);
  // Both settings write through to localStorage, so what the URL carried becomes the default.
  assert.match(applying, /localStorage\.setItem\(PLATE_STORAGE_KEY/);
  assert.match(applying, /localStorage\.setItem\(TOLERANCE_STORAGE_KEY/);
  // Changing the tolerance now reaches the server too, not just localStorage.
  const tolerance = javascript.slice(javascript.indexOf("$('#toleranceInput').addEventListener"), javascript.indexOf('async function checkIntersections()'));
  assert.match(tolerance, /saveWorkspaceSettings\(\)/);
  const bootstrapping = javascript.slice(javascript.indexOf('async function bootstrap()'), javascript.indexOf('// Back/forward moves between workspaces'));
  assert.match(bootstrapping, /populatePlateSelect\(\)/);
  assert.match(bootstrapping, /restorePlatePreference\(\)/);
});

test('an in-flight settings save cannot be undone by a stale remote payload', () => {
  // A poll answering an AI code edit carries the settings the server held when it replied.
  // Applying those over a plate the user just picked would silently revert the choice.
  const settings = javascript.slice(javascript.indexOf('function saveWorkspaceSettings()'), javascript.indexOf('function scheduleWorkspaceSave('));
  // Claimed once, outside the per-attempt function, so it is held across the retry pauses
  // too — releasing per attempt would reopen the window the guard exists to close.
  assert.match(settings, /settingsSavesInFlight\.set\(id, \(settingsSavesInFlight\.get\(id\) \|\| 0\) \+ 1\);\n {2}const release = \(\)/);
  assert.ok(settings.indexOf('settingsSavesInFlight.set(id,') < settings.indexOf('const attemptSave = (attempt)'));
  assert.match(settings, /settingsSavesInFlight\.delete\(id\)/);
  // Every terminal path releases it, or one dropped request freezes the workspace forever.
  assert.equal((settings.match(/release\(\);/g) || []).length, 3, 'success, abandoned, and exhausted all release');
  // An unresolved fetch would pin the guard, so the request is bounded.
  assert.match(settings, /signal: AbortSignal\.timeout\(SETTINGS_SAVE_TIMEOUT_MS\)/);
  // A silently dropped settings save would strand the plate in localStorage only.
  assert.match(settings, /attemptSave\(attempt \+ 1\)/);
  assert.match(settings, /SETTINGS_SAVE_ATTEMPTS/);
  const applying = javascript.slice(javascript.indexOf('function applyWorkspaceSettings('), javascript.indexOf('function applyWorkspace('));
  // Scoped to the workspace being saved: navigating elsewhere must still pick up the
  // destination's own settings.
  assert.match(applying, /settingsSavesInFlight\.has\(String\(workspace\.id \?\? workspaceId\)\)/);
});

test('opening a plateless workspace never erases the plate the user owns', () => {
  const applying = javascript.slice(javascript.indexOf('function applyWorkspaceSettings('), javascript.indexOf('function applyWorkspace('));
  // Every legacy and every plateless workspace serializes plate:"" via the serde default.
  // Writing that through would mean visiting one shared link wiped the local default, and
  // the next NEW workspace would come up bare — the exact thing the spec asks us to keep.
  assert.match(applying, /if \(plate\) localStorage\.setItem\(PLATE_STORAGE_KEY, plate\.id\)/);
  assert.doesNotMatch(applying, /localStorage\.setItem\(PLATE_STORAGE_KEY, plate \? plate\.id : ''\)/);
  // The workspace is still authoritative for its own viewport: the empty plate IS applied.
  assert.match(applying, /const plate = applyPlate\(workspace\.plate\)/);
  // A forced conflict resolution replays a snapshot captured when the banner appeared, so
  // its settings are stale and must not roll back a plate changed since.
  assert.match(javascript, /if \(!force\) applyWorkspaceSettings\(workspace\);/);
  // A preference naming a printer this build dropped must not be forwarded to the server.
  const restoring = javascript.slice(javascript.indexOf('function restorePlatePreference()'), javascript.indexOf('function frameForPlate('));
  assert.match(restoring, /if \(stored && !plate\) localStorage\.removeItem\(PLATE_STORAGE_KEY\)/);
});

test('choosing a plate frames it, and a settings PATCH is never charged an evaluator permit', () => {
  // Selecting a 350 mm bed while zoomed onto a 20 mm part would leave the bed off-screen,
  // so the only visible effect would be the grid getting coarser.
  const framing = javascript.slice(javascript.indexOf('function frameForPlate(plate)'), javascript.indexOf("$('#plateSelect').addEventListener"));
  assert.match(framing, /Math\.max\(plate\.size\[0\], plate\.size\[1\], plate\.size\[2\]\)/);
  assert.match(framing, /Math\.max\(span \* 1\.9, meshViewport\.radius \* 4, 20\)/);
  const listener = javascript.slice(javascript.indexOf("$('#plateSelect').addEventListener"), javascript.indexOf("$('#viewport').addEventListener('wheel'"));
  assert.match(listener, /if \(plate\) frameForPlate\(plate\)/);
  // Settings sync on every poll: they are revisionless, so `changed` says nothing about them.
  const polling = javascript.slice(javascript.indexOf('function beginWorkspacePolling'), javascript.indexOf('function normalizeObjects'));
  assert.match(polling, /applyWorkspaceSettings\(\{ \.\.\.workspaceFromPayload\(payload\), id \}\)/);
  assert.ok(polling.indexOf('applyWorkspaceSettings(') < polling.indexOf('payload.changed === true'));
});

test('settings arriving on every poll are applied only when they actually changed', () => {
  // The poll runs twice a second. Reassigning unconditionally would redraw the viewport
  // continuously and reset the select and tolerance field out from under a user mid-edit.
  const applying = javascript.slice(javascript.indexOf('function applyWorkspaceSettings('), javascript.indexOf('function applyWorkspace('));
  assert.match(applying, /workspace\.plate !== \(meshViewport\.plate\?\.id \?\? ''\)/);
  assert.match(applying, /tolerance !== readTolerance\(\)/);
});

test('the MCP panel is reachable from the toolbar and never invents its own endpoint', () => {
  // The whole AI surface used to be undiscoverable: one incidental comment mentioned MCP
  // and nothing in the UI did. The toolbar button is the entry point.
  assert.match(html, /id="mcpButton"[^>]*data-icon="agent"/);
  assert.match(html, /id="mcpModal"[^>]*hidden/);
  assert.match(html, /id="mcpEndpoint"/);
  assert.match(html, /id="mcpWorkspaceId"/);
  assert.match(html, /id="mcpTest"/);
  // Derived from the origin the page was actually served from, so a proxy, a different
  // port or REOPENSCAD_PUBLIC_ORIGIN are all correct without touching this file.
  assert.match(javascript, /function mcpEndpointUrl\(\) \{\s*return `\$\{location\.origin\}\/mcp`;/);
  // A hardcoded dev host would be wrong the moment this is deployed. The one surviving
  // mention is prose explaining why Claude Desktop's connector UI rejects loopback.
  assert.equal(javascript.match(/127\.0\.0\.1:5173/g), null);
  assert.equal(html.match(/127\.0\.0\.1|localhost/g), null);
});

test('the smoke test and the tool list come from the live server, not from a copy of it', () => {
  const probe = javascript.slice(javascript.indexOf('async function probeMcp()'), javascript.indexOf('async function copyForMcp('));
  // A real handshake, then tools/list — so a green light means the endpoint genuinely
  // answers rather than that the page believes it should. The current, stateless era is
  // tried first because that is what a current client uses; the initialize handshake is
  // only the fallback for a server that predates it.
  assert.ok(probe.indexOf("mcpRpc('server/discover'") < probe.indexOf("mcpRpc('initialize'"));
  assert.ok(probe.indexOf("mcpRpc('initialize'") < probe.indexOf("mcpRpc('tools/list'"));
  assert.match(probe, /era = 'legacy';/);
  // Modern requests carry the version and capabilities per request plus the method
  // mirrored into a header; getting any of that wrong is a 400, not a soft failure.
  const rpcModern = javascript.slice(javascript.indexOf('async function mcpRpc('), javascript.indexOf('// A real handshake'));
  assert.match(rpcModern, /headers\['Mcp-Method'\] = method;/);
  assert.match(rpcModern, /'io\.modelcontextprotocol\/protocolVersion': protocol/);
  assert.match(rpcModern, /'io\.modelcontextprotocol\/clientCapabilities': \{\}/);
  assert.match(probe, /renderMcpTools\(tools\)/);
  assert.match(probe, /tools\.length/);
  // Post-initialize calls carry the negotiated version, which is what the server checks.
  assert.match(rpcModern, /if \(method !== 'initialize'\) headers\['MCP-Protocol-Version'\] = protocol;/);
  // Tool names and descriptions are rendered from the response, never transcribed here:
  // a hardcoded list silently rots the first time the server grows a tool.
  const panel = javascript.slice(javascript.indexOf('// ---- Agent / MCP connection panel'), javascript.indexOf("const syncControl = $('#syncState')"));
  for (const tool of ['create_workspace', 'render_workspace', 'check_intersections', 'list_workspace_objects']) {
    assert.equal(panel.includes(`"${tool}"`), false, `${tool} should not be transcribed into the panel`);
  }
  assert.match(javascript, /function renderMcpTools\(tools\) \{/);
});

test('per-client setup snippets match the shape each client actually documents', () => {
  const clients = javascript.slice(javascript.indexOf('const MCP_CLIENTS = ['), javascript.indexOf('function selectedMcpClient()'));
  // Claude Code's CLI transport keyword is `http`, not `streamable-http`.
  assert.match(clients, /claude mcp add --scope user --transport http reopenscad/);
  // VS Code's top-level key is `servers`; using mcpServers there silently does nothing.
  assert.match(clients, /JSON\.stringify\(\{ servers: \{ reopenscad: \{ type: 'http', url: endpoint \} \} \}/);
  // Cursor identifies a remote server by the presence of `url`; `type` is stdio-only.
  assert.match(clients, /JSON\.stringify\(\{ mcpServers: \{ reopenscad: \{ url: endpoint \} \} \}/);
  // Windsurf and Gemini CLI each use their own key name for the same URL.
  assert.match(clients, /serverUrl: endpoint/);
  assert.match(clients, /httpUrl: endpoint/);
  // Claude Desktop's config file has no url field at all, so it needs a stdio bridge —
  // and the panel has to say that the bridge is not an Anthropic-documented tool.
  assert.match(clients, /'mcp-remote'/);
  assert.match(clients, /community bridge/);
  // Every snippet is built from the live endpoint.
  assert.equal(clients.match(/https?:\/\/(?!code\.claude|cursor|code\.visualstudio|modelcontextprotocol)/g), null);
});

test('the workspace id is surfaced so it can be handed to an agent', () => {
  // Most tools are scoped by workspaceId; without it in the UI the user has to dig it
  // out of the address bar and guess which part is the id.
  assert.match(javascript, /\$\('#mcpWorkspaceId'\)\.textContent = workspaceId \|\| 'not created yet'/);
  assert.match(javascript, /function mcpAgentBriefing\(endpoint, workspace\)/);
  assert.match(javascript, /My workspace id is \$\{workspace\}/);
});

test('the header carries workspace identity and nothing else', () => {
  const header = html.slice(html.indexOf('<header'), html.indexOf('</header>'));
  // The header credit is deliberate (kept on request) but must stay quiet: it is
  // secondary chrome, so it may not shout in the brand's own 700 weight, and it
  // yields entirely once the header is tight.
  assert.match(header, /class="brand-credit"/);
  assert.match(css, /\.brand-credit \{[^}]*font: 400 8px/);
  assert.match(css, /\.brand-credit \{ display: none; \}/);
  // The workspace explainer sentence and its animated "point up here" arrow explained
  // the address bar to the address bar. Nobody asked for either.
  assert.doesNotMatch(header, /workspace-explainer|explainer-more/);
  assert.doesNotMatch(css, /workspace-explainer|explainer-more|@keyframes pointUp/);
  // What is left: the brand, the copyable workspace link, the sync state, the engine pill.
  assert.match(header, /class="brand"/);
  assert.match(header, /id="copyWorkspaceLink"/);
  assert.match(header, /id="syncState"/);
  assert.match(header, /id="enginePill"/);
});

test('the status bar states, in visible text, that this is not the OpenSCAD project', () => {
  const footer = html.slice(html.indexOf('<footer class="status-bar">'), html.indexOf('</footer>'));
  // "Inspired by OpenSCAD" in the header reads as a nod; it does not deny a
  // relationship. The disclaimer is its own always-on-screen line, and it is
  // text content, not a title= tooltip nobody hovers.
  assert.match(footer, /class="status-attribution"/);
  assert.match(footer, /Independent reimplementation[^<]*not affiliated with or endorsed by the/);
  assert.match(footer, /<a href="https:\/\/openscad\.org"[^>]*>OpenSCAD project<\/a>/);
  assert.match(footer, /contains no OpenSCAD source code/);
  assert.doesNotMatch(footer, /class="status-attribution"[^>]*title=/);
  // The notice is the item that gives way on a narrow window, so the readouts
  // beside it never get pushed off the 25px row.
  assert.match(css, /\.status-attribution \{[^}]*flex: 0 1 auto/);
  assert.match(css, /\.status-attribution > span \{[^}]*text-overflow: ellipsis/);
});

test('the engine pill stays silent while the compiler is healthy', () => {
  const health = javascript.slice(javascript.indexOf('async function checkEngine()'), javascript.indexOf('async function render(mode'));
  // /api/health still returns `version`; the header simply has no use for it.
  assert.doesNotMatch(health, /data\.version/);
  // A working compiler is the expected case and says nothing at all.
  assert.doesNotMatch(javascript, /Compiler ready/);
  assert.match(health, /pill\.hidden = data\.ok/);
  // Failures still surface, because those the user can act on.
  assert.match(health, /'Compiler offline'/);
  assert.match(health, /'Backend offline'/);
  assert.match(html, /id="enginePill"[^>]*hidden/);
  assert.doesNotMatch(html, /ReOpenSCAD 0\.1|id="engineVersion"/);
});

test('the header elides the workspace id instead of growing past its 42px row', () => {
  // Nothing here may make the header taller: the shell pins the row.
  assert.match(css, /\.app-shell \{ display: grid; grid-template-rows: 42px /);
  assert.match(css, /\.workspace-identity \{[^}]*min-width: 0/);
  assert.match(css, /\.engine-pill \{ flex: none/);
  // A 40-character workspace id would otherwise push the engine pill off the window.
  assert.match(css, /\.workspace-link \{ flex: 0 1 auto; min-width: 0/);
  assert.match(css, /\.workspace-link #workspaceName \{[^}]*text-overflow: ellipsis/);
  // The sync state is what gives way first on a narrow window; the link itself survives.
  const narrow = css.slice(css.indexOf('@media (max-width: 900px)'), css.indexOf('@media (max-width: 720px)'));
  assert.match(narrow, /\.sync-state \{ display: none; \}/);
});

test('no orphaned element ids or unreachable CSS classes accumulate in the shell', () => {
  const shell = `${html}\n${javascript}`;
  // Every id in the markup is used by a script, a stylesheet, or an ARIA relationship.
  const orphanIds = [...html.matchAll(/\sid="([^"]+)"/g)]
    .map((match) => match[1])
    .filter((id) => !javascript.includes(id) && !css.includes(`#${id}`) && !html.includes(`labelledby="${id}"`) && !html.includes(`describedby="${id}"`));
  assert.deepEqual(orphanIds, [], 'ids nothing references');
  // Every class the stylesheet targets is emitted somewhere. `tok-*` is built by template
  // (`tok-${type}`) and is exempted by prefix rather than by name.
  const orphanClasses = [...new Set([...css.matchAll(/\.([A-Za-z][\w-]*)/g)].map((match) => match[1]))]
    .filter((name) => !name.startsWith('tok-') && !shell.includes(name));
  assert.deepEqual(orphanClasses, [], 'CSS rules that can never match');
});

test('the copy-link button survived the header rework', () => {
  assert.match(html, /id="copyWorkspaceLink" class="workspace-link" title="[^"]+"/);
  assert.match(html, /id="workspaceName"/);
  assert.match(javascript, /navigator\.clipboard\.writeText\(location\.href\)/);
  assert.match(javascript, /label\.textContent = 'link copied'/);
});

// --- Long renders -------------------------------------------------------------------
// An exact-kernel preview of a real workspace can run the better part of a minute. Three
// things have to hold for that wait to be survivable: the client must not give up before
// the server does, the wait must visibly be a wait rather than a hang, and it must be
// abortable from where the user is looking.

test('the client render backstop never fires before the server deadline it backs up', async () => {
  const timeouts = javascript.match(/const RENDER_TIMEOUT_MS = \{ preview: ([\d_]+), render: ([\d_]+) \};/);
  assert.ok(timeouts, 'RENDER_TIMEOUT_MS must stay greppable — this invariant depends on it');
  const clientPreview = Number(timeouts[1].replace(/_/g, ''));
  const clientRender = Number(timeouts[2].replace(/_/g, ''));

  const allowance = javascript.match(/const QUEUE_WAIT_ALLOWANCE_MS = ([\d_]+);/);
  assert.ok(allowance, 'QUEUE_WAIT_ALLOWANCE_MS must stay greppable');
  const clientQueueWait = Number(allowance[1].replace(/_/g, ''));

  // Cross-check against the server's real deadlines where they can be read. A client
  // backstop shorter than the server's own limit aborts renders that were still coming and
  // blames the server for it — that bug made a 50 s workspace unrenderable in the browser
  // while curl returned it fine. Fall back to the documented figures if the backend moves.
  //
  // The server is a queue now, so its deadline is not the whole story: it measures compute
  // only — waiting for a slot is deliberately free, which is the point of having a queue at
  // all — and the request's wall clock is queue wait PLUS compute. A backstop set to the
  // compute deadline alone would abort every render that had to wait, which is the same bug
  // in a new disguise. So the client must clear the sum, and MAX_QUEUE_WAIT is what bounds
  // the wait half: past it the server sheds the request itself.
  let serverPreview = 60_000;
  let serverRender = 120_000;
  let serverQueueWait = 60_000;
  try {
    const rust = await readFile(new URL('../backend/src/main.rs', import.meta.url), 'utf8');
    const preview = rust.match(/const PREVIEW_DEADLINE: Duration = Duration::from_secs\((\d+)\)/);
    const render = rust.match(/const RENDER_DEADLINE: Duration = Duration::from_secs\((\d+)\)/);
    const queue = rust.match(/const MAX_QUEUE_WAIT: Duration = Duration::from_secs\((\d+)\)/);
    if (preview) serverPreview = Number(preview[1]) * 1000;
    if (render) serverRender = Number(render[1]) * 1000;
    if (queue) serverQueueWait = Number(queue[1]) * 1000;
  } catch {}

  assert.equal(clientQueueWait, serverQueueWait, 'the client must mirror the server MAX_QUEUE_WAIT, not guess it');
  assert.ok(clientPreview > serverPreview + serverQueueWait,
    `preview backstop ${clientPreview}ms must exceed queue wait ${serverQueueWait}ms + compute ${serverPreview}ms`);
  assert.ok(clientRender > serverRender + serverQueueWait,
    `render backstop ${clientRender}ms must exceed queue wait ${serverQueueWait}ms + compute ${serverRender}ms`);
  // Each attempt is re-armed rather than sharing one timer across retries, so a
  // "server busy" wait can never be charged to the render that eventually ran.
  assert.match(javascript, /function armRenderBackstop\(request, mode\)/);
  assert.match(javascript, /armRenderBackstop\(request, mode\);\n    const response = await fetch\(url/);
});

test('a queued render says it is queued instead of looking like a hang', () => {
  // The wait is real and can be long, so the overlay has to distinguish "third in line"
  // from "compiling". The render response arrives once, at the end, so this state cannot
  // ride on it — a separate probe is the only place it can come from.
  const watch = javascript.slice(javascript.indexOf('function startQueueWatch('), javascript.indexOf('// Elapsed-time readout'));
  assert.match(watch, /\/api\/queue\?requestId=\$\{encodeURIComponent\(request\.id\)\}/);
  assert.match(watch, /status\.state === 'queued'/);
  assert.match(watch, /Waiting for a free slot — position \$\{status\.position\} of \$\{status\.waiting\}/);
  // And it says the wait is free, because on this server it genuinely is.
  assert.match(watch, /does not count against its time limit/);
  // A failed probe must never disturb the render it merely describes.
  assert.match(watch, /catch \{[\s\S]*?return;\n    \}/);
  // The probe belongs to the request, so a superseded render cannot blind its successor.
  assert.match(javascript, /request\.queueWatch = setInterval\(poll, QUEUE_POLL_MS\)/);
  assert.match(javascript, /stopQueueWatch\(request\);\n    if \(activeRender === request\)/);
  // Queue time is reported apart from compute time so a busy server is never
  // mistaken for a slow model.
  assert.match(javascript, /data\.queuedMs > 500/);
});

test('a busy server is retried with its own backoff, not reported as a failed render', () => {
  const send = javascript.slice(javascript.indexOf('async function sendRender('), javascript.indexOf('async function render(mode'));
  assert.match(send, /response\.status === 429 && data && data\.retryable/);
  // The server's own Retry-After wins over any number invented here.
  assert.match(send, /Number\(response\.headers\.get\('Retry-After'\)\) \|\| data\.retryAfterSeconds/);
  // Bounded: retrying forever is its own kind of hang.
  assert.match(send, /attempt >= QUEUE_RETRY_LIMIT/);
  assert.match(javascript, /const QUEUE_RETRY_LIMIT = \d+;/);
  // And it is said out loud rather than hidden behind a spinner.
  assert.match(send, /Server busy — retrying in \$\{wait\} s/);
  assert.match(send, /Retrying in \$\{wait\} s \(attempt/);
});

test('the render overlay counts real elapsed time and invents no progress', () => {
  assert.match(html, /id="renderElapsed"/);
  assert.match(html, /id="renderNote"/);
  const ticker = javascript.slice(javascript.indexOf('function startRenderElapsed('), javascript.indexOf('async function render(mode'));
  // Measured from a real clock, not a guessed completion fraction.
  assert.match(ticker, /performance\.now\(\) - started/);
  assert.match(ticker, /elapsed\.textContent = `\$\{formatElapsed\(milliseconds \/ 1000\)\} elapsed`/);
  assert.match(ticker, /setInterval\(tick, 100\)/);
  // A superseded render must not keep writing over the current one's clock.
  assert.match(ticker, /if \(activeRender !== request\) \{ stopRenderElapsed\(\); return; \}/);
  // No fabricated completion signal anywhere in the overlay.
  assert.doesNotMatch(javascript, /progressBar|percentComplete|\.progress\s*=\s*\d/);
  assert.doesNotMatch(html, /<progress|role="progressbar"/);
  // The ticker is always retired with the render that owns it.
  assert.match(javascript, /stopRenderElapsed\(\);\n      renderOverlay\.hidden = true;/);
});

test('a long render is abortable from the overlay that is covering the viewport', () => {
  assert.match(html, /id="overlayCancel"/);
  assert.match(javascript, /\$\('#overlayCancel'\)\.addEventListener\('click', \(\) => cancelRender\(\)\)/);
  // Both affordances reach the same abort: the local fetch AND the server-side flag.
  const cancel = javascript.slice(javascript.indexOf('function cancelRender('), javascript.indexOf('function downloadBlob('));
  assert.match(cancel, /request\.controller\.abort\(\)/);
  assert.match(cancel, /'\/api\/cancel'/);
  assert.match(cancel, /requestId: request\.id/);
  // Cancelling reports the wait it actually saved.
  assert.match(javascript, /Render cancelled after \$\{formatElapsed\(\(request\.elapsedMs \|\| 0\) \/ 1000\)\}/);
});

test('render response geometry is decoded once, not once per copy', () => {
  const setMesh = javascript.slice(javascript.indexOf('  setMesh(encoded, objects = []) {'), javascript.indexOf('  multiply(a, b) {'));
  // The response ships the same triangles twice (combined `mesh` plus `objects[].mesh`).
  // Exactly one decode path may run: the per-object batches, or the combined fallback.
  assert.equal((setMesh.match(/decodeStl\(/g) || []).length, 0);
  assert.equal((setMesh.match(/createMeshBatch\(/g) || []).length, 2);
  assert.doesNotMatch(setMesh, /const combined = this\.decodeStl/);
  // Batch extents are what the union above is built from.
  assert.match(javascript, /triangles: decoded\.triangles, min: decoded\.min, max: decoded\.max/);
});

test('the first-use disclaimer states the terms it exists to state', () => {
  const dialog = html.slice(html.indexOf('<div class="mcp-modal disclaimer-modal"'), html.indexOf('<script src="/app.js"'));
  assert.match(dialog, /hobby project/i);
  assert.match(dialog, /demonstration purposes/i);
  assert.match(dialog, /<b>as is<\/b>/);
  assert.match(dialog, /<b>no warranty<\/b>/);
  assert.match(dialog, /<b>no liability<\/b>/);
  assert.match(dialog, /<b>Workspaces are not private\.<\/b>/);
  assert.match(dialog, /the URL is the only key/);
  // The retention claims must be the server's real numbers, not a guess:
  // WORKSPACE_TTL_MS = 14 days and MAX_WORKSPACES = 512 in backend/src/main.rs.
  assert.match(dialog, /deleted 14 days after its last change/);
  assert.match(dialog, /only the 512 most recent are kept/);
  assert.match(dialog, /Verify it before you print or manufacture/);
});

test('the disclaimer reuses the status bar non-affiliation sentence instead of restating it', () => {
  const dialog = html.slice(html.indexOf('<div class="mcp-modal disclaimer-modal"'), html.indexOf('<script src="/app.js"'));
  // The dialog ships an empty slot; the wording is lifted out of the status bar
  // at open time, so there is exactly one copy of the sentence in the product.
  assert.match(dialog, /<p class="disclaimer-affiliation" id="disclaimerAffiliation"><\/p>/);
  assert.doesNotMatch(dialog, /not affiliated/i);
  assert.match(javascript, /\$\('#disclaimerAffiliation'\)\.textContent = \$\('\.status-attribution > span'\)\?\.textContent\.trim\(\)/);
  assert.equal((html.match(/not affiliated with or endorsed by/g) || []).length, 1);
});

test('the disclaimer is acknowledged once, under a versioned key', () => {
  assert.match(javascript, /const DISCLAIMER_REVISION = \d+;/);
  assert.match(javascript, /const DISCLAIMER_STORAGE_KEY = `reopenscad\.disclaimerAck\.v\$\{DISCLAIMER_REVISION\}`;/);
  const gate = javascript.slice(javascript.indexOf('function disclaimerAcknowledged'), javascript.indexOf('function openDisclaimer'));
  assert.match(gate, /localStorage\.getItem\(DISCLAIMER_STORAGE_KEY\) === '1'/);
  // Storage is unavailable in some contexts; neither reading nor writing it may
  // throw out of bootstrap.
  assert.match(gate, /catch \{\s*return false;/);
  const accept = javascript.slice(javascript.indexOf('function acceptDisclaimer'), javascript.indexOf('function disclaimerFocusables'));
  assert.match(accept, /try \{\s*localStorage\.setItem\(DISCLAIMER_STORAGE_KEY, '1'\);\s*\} catch/);
  assert.match(accept, /closeDisclaimer\(\)/);
});

test('dismissing the disclaimer takes a deliberate press of its one button', () => {
  const dialog = html.slice(html.indexOf('<div class="mcp-modal disclaimer-modal"'), html.indexOf('<script src="/app.js"'));
  assert.match(dialog, /id="acceptDisclaimer">I understand</);
  // No close glyph and, unlike #mcpScrim, no id on the scrim to hang a
  // click-to-dismiss listener from.
  assert.doesNotMatch(dialog, /mcp-close/);
  assert.match(dialog, /<div class="mcp-scrim"><\/div>/);
  assert.match(javascript, /\$\('#acceptDisclaimer'\)\.addEventListener\('click', acceptDisclaimer\)/);
  // Escape closes the MCP panel and must not close this one: a reflex keypress
  // is not an acknowledgement.
  const wiring = javascript.slice(javascript.indexOf('// ---- First-use disclaimer'), javascript.indexOf("const syncControl = $('#syncState')"));
  assert.doesNotMatch(wiring, /'Escape'/);
});

test('the disclaimer is a labelled modal that traps focus and gives it back', () => {
  const dialog = html.slice(html.indexOf('<div class="mcp-modal disclaimer-modal"'), html.indexOf('<script src="/app.js"'));
  assert.match(dialog, /role="dialog" aria-modal="true" aria-labelledby="disclaimerTitle" aria-describedby="disclaimerBody"/);
  assert.match(dialog, /<h2 id="disclaimerTitle">/);
  const open = javascript.slice(javascript.indexOf('function openDisclaimer'), javascript.indexOf('function closeDisclaimer'));
  assert.match(open, /disclaimerReturnFocus = document\.activeElement/);
  assert.match(open, /\$\('#acceptDisclaimer'\)\.focus\(\)/);
  const close = javascript.slice(javascript.indexOf('function closeDisclaimer'), javascript.indexOf('function acceptDisclaimer'));
  assert.match(close, /restore\.isConnected && restore !== document\.body\) restore\.focus\(\)/);
  assert.match(close, /else editor\.focus\(\)/);
  // Tab wraps inside the dialog, and a focusin backstop catches focus the app
  // moves programmatically while the dialog is up.
  const wiring = javascript.slice(javascript.indexOf('// ---- First-use disclaimer'), javascript.indexOf("const syncControl = $('#syncState')"));
  assert.match(wiring, /if \(event\.key !== 'Tab'\) return;/);
  assert.match(wiring, /event\.preventDefault\(\); last\.focus\(\);/);
  assert.match(wiring, /event\.preventDefault\(\); first\.focus\(\);/);
  assert.match(wiring, /document\.addEventListener\('focusin'/);
});

test('the disclaimer never blocks the boot path, and can be reopened later', () => {
  const boot = javascript.slice(javascript.indexOf('async function bootstrap()'), javascript.indexOf('// Back/forward moves between workspaces'));
  // Fire-and-forget: not awaited, and it precedes nothing it could reorder.
  assert.match(boot, /^ {2}showDisclaimerIfUnacknowledged\(\);$/m);
  assert.doesNotMatch(boot, /await showDisclaimerIfUnacknowledged/);
  assert.ok(boot.indexOf('showDisclaimerIfUnacknowledged()') < boot.indexOf('createWorkspace'));
  assert.match(boot, /await createWorkspace\(''\)/);
  assert.match(boot, /setTimeout\(\(\) => render\('preview'\), 180\)/);
  // Dismissal is not irreversible: the status bar keeps a way back to it.
  assert.match(html, /id="showDisclaimer"/);
  assert.match(javascript, /\$\('#showDisclaimer'\)\.addEventListener\('click', openDisclaimer\)/);
});

test('the disclaimer borrows the MCP modal shell rather than adding a second one', () => {
  const dialog = html.slice(html.indexOf('<div class="mcp-modal disclaimer-modal"'), html.indexOf('<script src="/app.js"'));
  assert.match(dialog, /class="mcp-modal disclaimer-modal"/);
  assert.match(dialog, /class="mcp-dialog disclaimer-dialog"/);
  assert.match(dialog, /class="mcp-body disclaimer-body"/);
  assert.match(css, /\.disclaimer-dialog \{ grid-template-rows: auto minmax\(0, 1fr\) auto;/);
  // It sits above the MCP modal's z-index so it can never be buried.
  assert.match(css, /\.disclaimer-modal \{ z-index: 60; \}/);
  // Reading width, not the MCP panel's 960px workbench.
  assert.match(css, /\.disclaimer-dialog \{[^}]*width: min\(520px, 100%\)/);
});

// The camera maths is pulled out of the source and run for real rather than pattern
// matched: a pan that is off by a factor is the difference between "feels like Fusion"
// and "feels broken", and only arithmetic can catch that.
function cameraModule() {
  const methods = javascript.slice(javascript.indexOf('  multiply(a, b) {'), javascript.indexOf('  scaleMatrix(scale) {'));
  const panning = javascript.slice(javascript.indexOf('function panView('), javascript.indexOf("$('#viewport').addEventListener('wheel'"));
  // Taken from the source too, so the harness cannot drift from the field of view the
  // projection matrix actually uses.
  const fov = javascript.match(/const VIEW_FOV = ([^;]+);/)[1];
  return new Function('camera', 'projection', `
    const VIEW_FOV = ${fov};
    class View { ${methods} }
    const view = new View();
    view.center = [4, -6, 3];
    view.radius = 50;
    view.draw = () => {};
    view.canvas = { width: 800, height: 600, clientWidth: 800, clientHeight: 600 };
    const meshViewport = view;
    ${panning}
    return { view, panView };
  `);
}

// Mirrors draw()'s eye placement so a projected point can be checked without a GL context.
// The static assertions below pin draw() to this same formula.
function projectThrough(view, camera, point) {
  const pitch = camera[3] * Math.PI / 180;
  const yaw = camera[5] * Math.PI / 180;
  const pivot = view.pivot();
  const eye = [
    pivot[0] + camera[6] * Math.cos(pitch) * Math.cos(yaw),
    pivot[1] + camera[6] * Math.cos(pitch) * Math.sin(yaw),
    pivot[2] + camera[6] * Math.sin(pitch),
  ];
  const mvp = view.multiply(view.projectionMatrix(view.canvas.width / view.canvas.height), view.lookAt(eye, pivot, [0, 0, 1]));
  const clip = [0, 1, 2, 3].map((row) => mvp[row] * point[0] + mvp[4 + row] * point[1] + mvp[8 + row] * point[2] + mvp[12 + row]);
  return {
    x: (clip[0] / clip[3] * 0.5 + 0.5) * view.canvas.width,
    y: (0.5 - clip[1] / clip[3] * 0.5) * view.canvas.height,
  };
}

test('a pan drag moves the world 1:1 under the cursor, in both projections', () => {
  const make = cameraModule();
  for (const projection of ['perspective', 'orthographic']) {
    for (const angles of [[58, 28, 140], [-23, 137, 88], [0, -90, 300]]) {
      const camera = [0, 0, 0, angles[0], 0, angles[1], angles[2]];
      const { view, panView } = make(camera, projection);
      // Witnesses in the plane through the pivot perpendicular to the view axis: that is
      // the plane a pan is defined against, so tracking there must be exact.
      const [right, up] = view.screenBasis();
      const pivot = view.pivot();
      const witnesses = [[0, 0], [21, 0], [0, -17], [-33, 12]]
        .map(([a, b]) => [0, 1, 2].map((i) => pivot[i] + right[i] * a + up[i] * b));
      const before = witnesses.map((point) => projectThrough(view, camera, point));
      const dx = 62;
      const dy = -41;
      panView([camera[0], camera[1], camera[2]], dx, dy);
      const after = witnesses.map((point) => projectThrough(view, camera, point));
      const label = `${projection} ${angles.join('/')}`;
      // Tolerance is float32 noise from the Float32Array matrices, not slop in the mapping:
      // a thousandth of a pixel is three orders of magnitude tighter than "feels 1:1".
      after.forEach((q, index) => {
        assert.ok(Math.abs(q.x - before[index].x - dx) < 1e-3, `${label}: x moved ${q.x - before[index].x}, wanted ${dx}`);
        assert.ok(Math.abs(q.y - before[index].y - dy) < 1e-3, `${label}: y moved ${q.y - before[index].y}, wanted ${dy}`);
      });
    }
  }
});

test('the screen basis a pan translates along is the one lookAt builds', () => {
  const make = cameraModule();
  for (const angles of [[58, 28], [-23, 137], [12, -90]]) {
    const camera = [0, 0, 0, angles[0], 0, angles[1], 140];
    const { view } = make(camera, 'perspective');
    const pitch = camera[3] * Math.PI / 180;
    const yaw = camera[5] * Math.PI / 180;
    const pivot = view.pivot();
    const eye = [
      pivot[0] + camera[6] * Math.cos(pitch) * Math.cos(yaw),
      pivot[1] + camera[6] * Math.cos(pitch) * Math.sin(yaw),
      pivot[2] + camera[6] * Math.sin(pitch),
    ];
    const m = view.lookAt(eye, pivot, [0, 0, 1]);
    // Column-major view matrix: row 0 is screen right, row 1 is screen up, in world space.
    const [right, up] = view.screenBasis();
    [0, 1, 2].forEach((axis) => {
      assert.ok(Math.abs(right[axis] - m[axis * 4]) < 1e-6, `right[${axis}] ${right[axis]} vs ${m[axis * 4]}`);
      assert.ok(Math.abs(up[axis] - m[axis * 4 + 1]) < 1e-6, `up[${axis}] ${up[axis]} vs ${m[axis * 4 + 1]}`);
    });
  }
});

test('each projection supplies its own screen-to-world scale', () => {
  const make = cameraModule();
  const camera = [0, 0, 0, 58, 0, 28, 140];
  const perspective = make(camera, 'perspective').view;
  // 32 degree vertical field of view, measured in the plane through the pivot.
  assert.ok(Math.abs(perspective.worldPerPixel() - 2 * 140 * Math.tan(16 * Math.PI / 180) / 600) < 1e-9);
  const orthographic = make(camera, 'orthographic').view;
  assert.ok(Math.abs(orthographic.worldPerPixel() - 2 * Math.max(50 * 1.35, 140 * 0.36) / 600) < 1e-9);
  // Getting the two confused is the classic pan bug, so they must actually differ here.
  assert.notEqual(perspective.worldPerPixel(), orthographic.worldPerPixel());
  // Each frame constant has exactly one definition, shared by the projection matrix, the
  // grid's view span and the pan scale: three copies of the field of view is how a pan
  // silently stops tracking the cursor.
  const viewport = javascript.slice(javascript.indexOf('class MeshViewport'), javascript.indexOf('const meshViewport = new MeshViewport'));
  assert.equal((viewport.match(/this\.radius \* 1\.35, camera\[6\] \* 0\.36/g) || []).length, 1);
  assert.equal((javascript.match(/32 \* Math\.PI \/ 180/g) || []).length, 1, 'the field of view is a named constant, not a repeated literal');
  assert.match(viewport, /orthoHalfHeight\(\) \{/);
  assert.match(viewport, /frameHeight\(\) \{/);
  // viewSpan and worldPerPixel are the same frame measured two ways, so they share it.
  const span = javascript.slice(javascript.indexOf('  viewSpan() {'), javascript.indexOf('  groundSpan() {'));
  assert.match(span, /const height = this\.frameHeight\(\);/);
});

test('the whole view frames on the panned target, not the model centre', () => {
  const make = cameraModule();
  const camera = [0, 0, 0, 58, 0, 28, 140];
  const { view, panView } = make(camera, 'perspective');
  assert.deepEqual(view.pivot(), [4, -6, 3]);
  panView([0, 0, 0], 40, -20);
  assert.deepEqual(view.pivot(), [4 + camera[0], -6 + camera[1], 3 + camera[2]]);
  assert.notDeepEqual(view.pivot(), view.center);
  // camera[0..2] is the pan offset, so orbit and zoom stay untouched by a pan.
  assert.equal(camera[3], 58);
  assert.equal(camera[5], 28);
  assert.equal(camera[6], 140);

  // Orbit and zoom are both defined against the pivot, so a panned view survives them.
  const drawing = javascript.slice(javascript.indexOf('  draw() {'), javascript.indexOf('  projectPoint(point) {'));
  assert.match(drawing, /const pivot = this\.pivot\(\);/);
  assert.match(drawing, /pivot\[0\] \+ camera\[6\] \* Math\.cos\(pitch\) \* Math\.cos\(yaw\)/);
  assert.match(drawing, /this\.lookAt\(eye, pivot, \[0, 0, 1\]\)/);
  assert.doesNotMatch(drawing, /this\.lookAt\(eye, this\.center/);
  // The axes are world geometry: their reach is measured from the pivot too, or panning
  // away from the origin would leave the datum short of the frame.
  assert.match(javascript, /const originOffset = Math\.hypot\(\.\.\.this\.pivot\(\)\);/);
});

test('pan uses the bindings CAD users already have, and orbit keeps the plain drag', () => {
  assert.match(javascript, /function isPanGesture\(event\) \{[\s\S]*?event\.button === 1 \|\| event\.button === 2 \|\| \(event\.button === 0 && event\.shiftKey\)/);
  const pointer = javascript.slice(javascript.indexOf("renderCanvas.addEventListener('pointerdown'"), javascript.indexOf("$('#customizerButton')"));
  // One press can only ever mean one gesture.
  assert.match(pointer, /if \(isPanGesture\(event\)\) \{/);
  assert.match(pointer, /\} else if \(event\.button === 0\) \{\n\s+orbitStart = \{/);
  assert.match(pointer, /if \(panStart\) \{\n\s+panView\(panStart\.offset/);
  // A drag that ends outside the canvas, or is cancelled, must not leave a gesture latched.
  assert.match(pointer, /renderCanvas\.addEventListener\('pointerup', endViewportDrag\)/);
  assert.match(pointer, /renderCanvas\.addEventListener\('pointercancel', endViewportDrag\)/);
  assert.match(pointer, /renderCanvas\.setPointerCapture\(event\.pointerId\)/);
  // A right drag is a pan, so the browser menu must not land on top of the model.
  assert.match(pointer, /addEventListener\('contextmenu', \(event\) => event\.preventDefault\(\)\)/);
  // Shift + wheel is the trackpad pan; a plain wheel is still the zoom every mouse expects.
  const wheel = javascript.slice(javascript.indexOf("$('#viewport').addEventListener('wheel'"), javascript.indexOf('function isPanGesture'));
  assert.match(wheel, /if \(event\.shiftKey\) \{\n\s+panView\(\[camera\[0\], camera\[1\], camera\[2\]\], -event\.deltaX, -event\.deltaY\);/);
  assert.match(wheel, /camera\[6\] = Math\.max\(meshViewport\.radius \* 1\.15/);
  // Discoverable, and the mode is visible while it is happening.
  const viewHint = html.slice(html.indexOf('<div class="view-hint">'), html.indexOf('<div class="viewport-fab">'));
  assert.match(viewHint, /<span>DRAG<\/span> orbit <span>SCROLL<\/span> zoom/);
  assert.match(viewHint, /<span>MIDDLE \/ SHIFT\+DRAG<\/span> pan/);
  // One line would run under the scale badge and the zoom cluster at the pane's width.
  assert.match(css, /\.view-hint \{[^}]*display: grid;/);
  assert.match(css, /\.render-canvas\.panning[^{]*\{ cursor: grabbing; \}/);
  assert.match(javascript, /renderCanvas\.classList\.add\('panning'\)/);
  assert.match(javascript, /renderCanvas\.classList\.remove\('panning'\)/);
});

test('anything that restores a known framing clears the pan', () => {
  // The presets are literals of the camera tuple, so their zero pan offset is the reset.
  const presets = javascript.slice(javascript.indexOf('const viewCameras = {'), javascript.indexOf("$('#zoomIn')"));
  assert.match(presets, /reset: \[0, 0, 0,/);
  assert.match(presets, /top: \[0, 0, 0,/);
  assert.match(presets, /front: \[0, 0, 0,/);
  assert.match(presets, /right: \[0, 0, 0,/);
  assert.match(presets, /camera = \[\.\.\.viewCameras\[button\.dataset\.view\]\]/);
  // Fit cannot honour "show me the model" while the view is aimed beside it.
  assert.match(presets, /#fitView'\)\.addEventListener\('click', \(\) => \{\n\s+camera\[0\] = camera\[1\] = camera\[2\] = 0;/);
  // A new mesh refits the distance, so the pan has to go with it.
  const setMesh = javascript.slice(javascript.indexOf('  setMesh(encoded, objects = []) {'), javascript.indexOf('  multiply(a, b) {'));
  assert.match(setMesh, /camera\[0\] = camera\[1\] = camera\[2\] = 0;/);
  assert.ok(setMesh.indexOf('camera[0] = camera[1] = camera[2] = 0;') < setMesh.indexOf('this.draw();'));
});

test('panning does not disturb the grid step or the far plane', () => {
  // Grid spacing is derived from the model and the visible frame only: no pan term, so
  // dragging the view cannot make the divisions flicker between steps.
  const span = javascript.slice(javascript.indexOf('  groundSpan() {'), javascript.indexOf('  groundExtents() {'));
  assert.doesNotMatch(span, /camera\[0\]|this\.pivot\(\)/);
  // The far plane does grow with the pan, or a far-panned grid corner would be clipped.
  const projectionMatrix = javascript.slice(javascript.indexOf('  projectionMatrix(aspect) {'), javascript.indexOf('  scaleMatrix(scale) {'));
  assert.match(projectionMatrix, /const panned = Math\.hypot\(camera\[0\], camera\[1\], camera\[2\]\);/);
  assert.match(projectionMatrix, /const far = camera\[6\] \+ this\.radius \* 4 \+ panned \+ 100;/);
});
