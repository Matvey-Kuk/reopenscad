const $ = (selector, root = document) => root.querySelector(selector);
const $$ = (selector, root = document) => [...root.querySelectorAll(selector)];

// Kick-start models. The catalogue lives in `examples/manifest.json` next to
// the `.scad` sources and their preview renders, rather than inline here: the
// sources are real files that the test suite compiles and the OpenSCAD binary
// checks, which a string literal in a bundle could never be.
// Absolute, because the app is served at `/workspaces/<id>` as well as at
// `/`, and a relative path would look for the catalogue inside the
// workspace route.
const EXAMPLES_BASE = '/examples/';
let exampleCatalogue = null;
let exampleCataloguePromise = null;

// --- Recently visited workspaces ------------------------------------------
//
// The workspace URL is the only handle anyone has on their work, and nothing in
// the app used to remember it: close the tab without bookmarking and the model
// was gone, still on the server and permanently unreachable. This is the
// address book — kept in localStorage, because it is a property of this browser
// and not of any workspace, and because a list of everything someone has opened
// is exactly the sort of thing that should not live on a server that has no
// accounts and no way to say who is asking.
const RECENTS_STORAGE_KEY = 'reopenscad.recentWorkspaces';
// Enough to cover "the thing I had open last week", short enough that the list
// stays a list rather than a filing system.
const RECENTS_LIMIT = 12;

function readRecents() {
  try {
    const stored = JSON.parse(localStorage.getItem(RECENTS_STORAGE_KEY) || '[]');
    if (!Array.isArray(stored)) return [];
    return stored
      .filter((entry) => entry && typeof entry.id === 'string' && entry.id)
      .map((entry) => ({ id: entry.id, visitedAt: Number(entry.visitedAt) || 0 }));
  } catch {
    return [];
  }
}

function writeRecents(entries) {
  try {
    localStorage.setItem(RECENTS_STORAGE_KEY, JSON.stringify(entries.slice(0, RECENTS_LIMIT)));
  } catch {
    // A full or blocked store costs the user their history, not their work.
  }
}

// Most recent first, which is the order the list is shown in and therefore the
// order it is stored in — no sort at render time, and no clock read either.
function rememberWorkspace(id) {
  if (!id) return;
  const rest = readRecents().filter((entry) => entry.id !== id);
  writeRecents([{ id, visitedAt: Date.now() }, ...rest]);
}

// Asked in the page rather than through a native browser dialog, for the same
// reason the source-conflict resolver is: a native one blocks the whole tab,
// looks like it came from somewhere else, and cannot say the thing that
// matters here — that the workspace itself survives. The wording is the
// safeguard, so it gets room to be read.
let pendingForget = null;

function closeForgetPrompt() {
  pendingForget = null;
  // `getElementById`, not `$`, because this element is created here rather than
  // declared in the shell — the same reason the conflict resolver does it.
  document.getElementById('recentForgetPrompt')?.remove();
}

function forgetRecentWorkspace(id, anchor) {
  if (pendingForget === id) {
    closeForgetPrompt();
    return;
  }
  closeForgetPrompt();
  pendingForget = id;
  const prompt = document.createElement('div');
  prompt.id = 'recentForgetPrompt';
  prompt.className = 'forget-prompt';
  prompt.setAttribute('role', 'dialog');
  prompt.setAttribute('aria-label', 'Remove workspace from the list');
  prompt.innerHTML = '<p>Remove workspace from the list? It will still be available '
    + 'via a direct link.</p><div><button type="button" data-forget-confirm>Remove</button>'
    + '<button type="button" data-forget-cancel>Cancel</button></div>';
  document.body.append(prompt);
  const rect = anchor.getBoundingClientRect();
  prompt.style.top = `${rect.bottom + 6}px`;
  prompt.style.left = `${Math.max(12, Math.min(rect.right - 210, innerWidth - 222))}px`;
  prompt.querySelector('[data-forget-cancel]').addEventListener('click', closeForgetPrompt);
  prompt.querySelector('[data-forget-confirm]').addEventListener('click', () => {
    writeRecents(readRecents().filter((entry) => entry.id !== id));
    closeForgetPrompt();
    renderRecents();
  });
  prompt.querySelector('[data-forget-confirm]').focus();
}

// Anywhere else, and Escape, dismisses it — a prompt nobody answered must not
// outlive the thing it is asking about.
document.addEventListener('mousedown', (event) => {
  if (!pendingForget) return;
  if (event.target.closest('#recentForgetPrompt, [data-forget]')) return;
  closeForgetPrompt();
});
document.addEventListener('keydown', (event) => {
  if (pendingForget && event.key === 'Escape') closeForgetPrompt();
});

// --- Render thumbnails -----------------------------------------------------
//
// Drawn here rather than on the server because the only thing that can produce
// one is a WebGL context with the model already on screen. Uploaded after a
// render settles, and only when the source has actually changed since the last
// upload: without that guard every preview re-render — one per keystroke, after
// the debounce — would post a fresh image for geometry nobody altered.
const PREVIEW_WIDTH = 320;
let lastPreviewUpload = { id: null, code: null };

function captureViewportPreview() {
  if (!hasMesh) return null;
  const source = renderCanvas;
  if (!source.width || !source.height) return null;
  const scale = Math.min(1, PREVIEW_WIDTH / source.width);
  const canvas = document.createElement('canvas');
  canvas.width = Math.max(1, Math.round(source.width * scale));
  canvas.height = Math.max(1, Math.round(source.height * scale));
  const context = canvas.getContext('2d');
  if (!context) return null;
  // The viewport is drawn on transparency; JPEG has no alpha, so without a
  // ground the model would be composited onto black. Paint the panel colour
  // first so a thumbnail looks like the app it came from.
  context.fillStyle = '#151a21';
  context.fillRect(0, 0, canvas.width, canvas.height);
  context.drawImage(source, 0, 0, canvas.width, canvas.height);
  // 0.72 keeps a 320-wide render near 8 KB. The server caps the stored string,
  // and a thumbnail that gets refused is not worth retrying at lower quality.
  return canvas.toDataURL('image/jpeg', 0.72);
}

async function uploadWorkspacePreview() {
  if (!workspaceId || !workspaceOnline) return;
  // A render is the strongest evidence a workspace is worth remembering, and
  // it is what promotes one that was empty when it loaded. Ahead of the
  // unchanged-source check below, so re-rendering also freshens its place in
  // the list.
  rememberWorkspace(workspaceId);
  const code = editor.value;
  if (lastPreviewUpload.id === workspaceId && lastPreviewUpload.code === code) return;
  const preview = captureViewportPreview();
  if (!preview) return;
  // Claimed before the request rather than after, so a second render settling
  // while this one is in flight does not post the same image twice.
  lastPreviewUpload = { id: workspaceId, code };
  try {
    await fetch(`/api/workspaces/${encodeURIComponent(workspaceId)}/preview`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ preview }),
    });
  } catch {
    // A thumbnail is decoration. Losing one must never surface as an error
    // beside the model it was taken of.
    lastPreviewUpload = { id: null, code: null };
  }
}

function loadCatalogue() {
  if (exampleCataloguePromise) return exampleCataloguePromise;
  exampleCataloguePromise = fetch(`${EXAMPLES_BASE}manifest.json`)
    .then((response) => {
      if (!response.ok) throw new Error(`manifest ${response.status}`);
      return response.json();
    })
    .then((manifest) => {
      exampleCatalogue = manifest;
      return manifest;
    })
    .catch(() => {
      // A missing catalogue is a deployment problem, not the user's: the empty
      // workspace still works, it just has nothing to offer. Reported in the
      // panel itself by renderKickstart, and deliberately not through
      // `console.*` — see the note on `log`.
      exampleCatalogue = null;
      return null;
    });
  return exampleCataloguePromise;
}

const icons = {
  new: '<svg viewBox="0 0 24 24"><path d="M6 3.5h8l4 4V21H6z"/><path d="M14 3.5v4h4M9 13h6M12 10v6"/></svg>',
  open: '<svg viewBox="0 0 24 24"><path d="M3.5 19.5 6 9h14.5L18 19.5z"/><path d="M5 9V5h6l2 2h5v2"/></svg>',
  save: '<svg viewBox="0 0 24 24"><path d="M5 3.5h12l2 2V20H5z"/><path d="M8 3.5V9h8V3.5M8 20v-7h8v7"/></svg>',
  preview: '<svg viewBox="0 0 24 24"><path d="m8 5 11 7-11 7z"/><path d="M4 5v14"/></svg>',
  render: '<svg viewBox="0 0 24 24"><path d="m12 2.8 8 4.6v9.2l-8 4.6-8-4.6V7.4zM4 7.4l8 4.7 8-4.7M12 12.1v9.1"/></svg>',
  stop: '<svg viewBox="0 0 24 24"><rect x="6" y="6" width="12" height="12" rx="1"/></svg>',
  top: '<svg viewBox="0 0 24 24"><path d="m12 3 9 5-9 5-9-5zM5 12l7 4 7-4M5 16l7 4 7-4"/></svg>',
  front: '<svg viewBox="0 0 24 24"><rect x="4" y="4" width="16" height="16"/><path d="M4 9h16M9 4v16"/></svg>',
  right: '<svg viewBox="0 0 24 24"><path d="M5 4h14v16H5zM13 4v16M5 10h8"/></svg>',
  reset: '<svg viewBox="0 0 24 24"><path d="M19 8a8 8 0 1 0 1 7M19 3v5h-5"/></svg>',
  export: '<svg viewBox="0 0 24 24"><path d="M12 3v12M7 10l5 5 5-5M5 19h14"/></svg>',
  agent: '<svg viewBox="0 0 24 24"><rect x="4" y="8" width="16" height="11" rx="2.5"/><path d="M12 4.5V8M9.5 13v1.6M14.5 13v1.6M2.5 12v3M21.5 12v3"/><circle cx="12" cy="3.4" r="1.3"/></svg>',
  sliders: '<svg viewBox="0 0 24 24"><path d="M4 6h9M17 6h3M4 12h3M11 12h9M4 18h10M18 18h2"/><circle cx="15" cy="6" r="2"/><circle cx="9" cy="12" r="2"/><circle cx="16" cy="18" r="2"/></svg>',
  fit: '<svg viewBox="0 0 24 24"><path d="M9 4H4v5M15 4h5v5M9 20H4v-5M15 20h5v-5"/></svg>',
  share: '<svg viewBox="0 0 24 24"><circle cx="18" cy="5.5" r="2.6"/><circle cx="6" cy="12" r="2.6"/><circle cx="18" cy="18.5" r="2.6"/><path d="M8.3 10.8 15.7 6.7M8.3 13.2l7.4 4.1"/></svg>',
  eye: '<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false"><path d="M2.5 12S6 5.75 12 5.75 21.5 12 21.5 12 18 18.25 12 18.25 2.5 12 2.5 12Z"/><circle cx="12" cy="12" r="2.9"/></svg>',
  eyeOff: '<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false"><path d="M10.2 6a9.9 9.9 0 0 1 1.8-.15c6 0 9.5 6.15 9.5 6.15a17.6 17.6 0 0 1-3.35 3.94M6.05 7.87A17.5 17.5 0 0 0 2.5 12s3.5 6.25 9.5 6.25a9.7 9.7 0 0 0 3.72-.72"/><path d="M9.9 9.98a2.95 2.95 0 0 0 4.14 4.15"/><path d="M3.6 3.6 20.4 20.4"/></svg>',
};

$$('[data-icon]').forEach((element) => {
  const icon = icons[element.dataset.icon];
  if (icon) element.insertAdjacentHTML('afterbegin', icon);
});

const editor = $('#codeEditor');
const highlight = $('#highlight');
const lineNumbers = $('#lineNumbers');
const renderCanvas = $('#renderCanvas');
const emptyState = $('#emptyState');
const renderOverlay = $('#renderOverlay');
const consoleOutput = $('#consoleOutput');
const statusText = $('#statusText');
const statusDot = $('#statusDot');
const renderTime = $('#renderTime');
const dirtyIndicator = $('#dirtyIndicator');
let activeRender = null;
let renderSequence = 0;
// Client-side backstop for the server's own render deadlines (PREVIEW_DEADLINE
// 60 s, RENDER_DEADLINE 120 s in backend/src/main.rs). The margin lets the
// server's own timeout reply — which names the limit and what to do about it —
// win whenever the server is still able to answer. This only fires when it is
// not, so the overlay can never sit on "Compiling preview" indefinitely.
//
// These MUST stay above the server's deadlines. A backstop that fires first
// does not protect the user, it lies to them: it aborts a render the server is
// still working on and reports "no response from the server" for a request that
// would have arrived. A 50 s preview against the old 40 s backstop made this
// workspace permanently unrenderable in the browser while curl returned it fine.
//
// The server is now a *queue*, so the request's total wall clock is queue wait
// plus compute, and the backstop has to clear the sum. The server's own
// deadline still measures compute alone — waiting for a slot is deliberately
// free — which is exactly why the client cannot reuse that number by itself.
// QUEUE_WAIT_ALLOWANCE_MS mirrors MAX_QUEUE_WAIT in backend/src/main.rs: past
// it the server sheds the request itself, so nothing legitimate can outlive
// deadline + allowance. tests/app.test.mjs re-derives all three from the Rust
// source and fails if this arithmetic stops holding.
const QUEUE_WAIT_ALLOWANCE_MS = 60_000;
const RENDER_TIMEOUT_MS = { preview: 130_000, render: 195_000 };
// How long a render may run before the overlay stops looking like a stall and
// says so. Purely a copy change — no work estimate is implied.
const SLOW_RENDER_NOTICE_MS = 8_000;
// While a render is in flight the server can tell us whether it is actually
// running or still waiting for a slot. Polled about once a second: often
// enough that a queue position never looks stale, rare enough to be free.
const QUEUE_POLL_MS = 1_000;
// A queue-full rejection is not a failure, it is "not yet". Retrying is
// strictly better than making the user click again, but only a bounded number
// of times and only while saying so out loud.
const QUEUE_RETRY_LIMIT = 3;
// The workspace IS the document, so no file name is surfaced in the UI. This base name
// exists purely so browser downloads land with a meaningful name on disk.
let downloadBaseName = 'model';
let projection = 'perspective';
// [panX, panY, panZ, pitch, unused, yaw, distance]. The first three are the pan offset in
// world millimetres: the view presets below are literals of this same shape, which is why
// picking one resets the pan for free.
let camera = [0, 0, 0, 58, 0, 28, 140];
let hasMesh = false;
let orbitStart = null;
let panStart = null;
let renderDebounce = null;
let workspaceSaveDebounce = null;
let workspaceSavePromise = null;
let workspaceSaveController = null;
let workspaceId = null;
let workspaceRevision = 0;
let workspaceOnline = false;
// Set when the page is a `/shared/<token>` view. Declared here with the rest of
// the workspace state rather than beside the code that uses it, because
// `updateKickstart` reads it and is defined a thousand lines earlier — a `let`
// below its first reader is a temporal-dead-zone trap waiting for someone to
// move a call.
//
// A shared page has no workspace id, deliberately: that is what stops a
// read-only link being an editable one with an extra step. The app runs
// without one — no saving, no polling, and renders go to the stateless
// `/api/render` with the source in the body, the same path a workspace that
// could not reach the server already takes.
let readOnlyToken = null;
let workspaceGeneration = 0;
let workspaceSavedCode = '';
let workspaceHasLocalChanges = false;
let workspaceConflict = null;
let applyingRemoteUpdate = false;
let aiUpdatedRanges = [];
let sceneObjects = [];
let selectedObjectId = null;
let intersectionIds = new Set();
let eventPollTimer = null;
let eventPollController = null;
let workspaceSaveFailures = 0;
let nextWorkspaceNavigation = 'replace';
// Viewer-settings PATCHes still awaiting a reply, counted per workspace id. A remote payload
// that predates them is stale for those fields, so it must not apply them back over the user
// — but only for the workspace actually being saved, or navigating away would skip the
// destination workspace's own settings.
const settingsSavesInFlight = new Map();

const funnyWords = {
  adjectives: ['brisk', 'cosmic', 'dapper', 'fuzzy', 'jaunty', 'nimble', 'plucky', 'solar', 'tidy', 'velvet'],
  nouns: ['badger', 'lathe', 'mango', 'otter', 'piston', 'quokka', 'rocket', 'spanner', 'walrus', 'wombat'],
};

// --- Ground surface ---------------------------------------------------------------
// The viewport draws a real Z=0 surface whose divisions carry a millimetre value, so the
// grid reads as a ruler for the part rather than as decoration.
// One definition of the perspective field of view: the projection matrix, the grid's
// view span and the pan scale all have to agree on it or a drag stops tracking the cursor.
const VIEW_FOV = 32 * Math.PI / 180;

const GROUND_MAJOR_EVERY = 5; // every 5th minor line is promoted to a major line
const GROUND_TARGET_CELLS = 20; // aim for ~20 minor divisions across the visible span
const GROUND_MAX_CELLS = 40; // half-count ceiling, bounds the line buffer
// Lines sit a hair below Z=0 (in cell units, so it tracks the spacing) — a model face
// lying exactly on the plane then always wins the depth test instead of z-fighting.
const GROUND_DEPTH_BIAS = 0.002;

// Snaps a raw span to the next human-readable 1-2-5 step: 0.5, 1, 2, 5, 10, 20, 50, 100 mm…
function niceStep(value) {
  if (!Number.isFinite(value) || value <= 0) return 1;
  const decade = 10 ** Math.floor(Math.log10(value));
  const mantissa = value / decade;
  const step = mantissa < 1.5 ? 1 : mantissa < 3.5 ? 2 : mantissa < 7.5 ? 5 : 10;
  return step * decade;
}

// Trims float noise so a 0.5 mm step never renders as "0.5000000001".
function formatMillimetres(value) {
  return String(Number(value.toFixed(3)));
}

// Build plates, as usable build volume in millimetres [X, Y, Z], from each manufacturer's
// published specification. Machines that lose X in dual-nozzle mode (Bambu H2D/H2C) carry
// the single-nozzle figure, which is the larger one. Models with an identical volume share
// a preset rather than repeating a number the user then has to trust twice.
// Every bed here is rectangular; the geometry assumes so.
const PRINTER_PLATES = [
  { brand: 'Bambu Lab', models: [
    { id: 'bambu-a1-mini', name: 'A1 mini', size: [180, 180, 180] },
    { id: 'bambu-a1', name: 'A1', size: [256, 256, 256] },
    { id: 'bambu-p1', name: 'P1S / P1P', size: [256, 256, 256] },
    { id: 'bambu-p2s', name: 'P2S', size: [256, 256, 256] },
    { id: 'bambu-x1', name: 'X1 Carbon / X1E', size: [256, 256, 256] },
    { id: 'bambu-x2d', name: 'X2D', size: [256, 256, 260] },
    { id: 'bambu-h2c', name: 'H2C', size: [305, 320, 325] },
    { id: 'bambu-h2d', name: 'H2D', size: [325, 320, 325] },
    { id: 'bambu-h2s', name: 'H2S', size: [340, 320, 340] },
  ] },
  { brand: 'Creality', models: [
    { id: 'creality-ender3-v3-ke', name: 'Ender-3 V3 KE', size: [220, 220, 240] },
    { id: 'creality-ender3-v3', name: 'Ender-3 V3 / V3 SE', size: [220, 220, 250] },
    { id: 'creality-k1', name: 'K1 / K1C / K1 SE', size: [220, 220, 250] },
    { id: 'creality-hi', name: 'Hi / Hi Combo', size: [260, 260, 300] },
    { id: 'creality-k1-max', name: 'K1 Max', size: [300, 300, 300] },
    { id: 'creality-k2-plus', name: 'K2 Plus', size: [350, 350, 350] },
    { id: 'creality-ender5-max', name: 'Ender-5 Max', size: [400, 400, 400] },
  ] },
  { brand: 'Prusa', models: [
    { id: 'prusa-mini', name: 'MINI+', size: [180, 180, 180] },
    { id: 'prusa-mk4s', name: 'MK4S / MK4', size: [250, 210, 220] },
    { id: 'prusa-core-one', name: 'CORE One', size: [250, 220, 270] },
    { id: 'prusa-core-one-l', name: 'CORE One L+', size: [300, 300, 330] },
    { id: 'prusa-xl', name: 'XL', size: [360, 360, 360] },
  ] },
  { brand: 'Anycubic', models: [
    { id: 'anycubic-kobra2-pro', name: 'Kobra 2 Pro', size: [220, 220, 250] },
    { id: 'anycubic-kobra-s1', name: 'Kobra S1', size: [250, 250, 250] },
    { id: 'anycubic-kobra3', name: 'Kobra 3', size: [250, 250, 260] },
    { id: 'anycubic-kobra-s1-max', name: 'Kobra S1 Max', size: [350, 350, 350] },
    { id: 'anycubic-kobra3-max', name: 'Kobra 3 Max', size: [420, 420, 500] },
  ] },
  { brand: 'Elegoo', models: [
    { id: 'elegoo-neptune4', name: 'Neptune 4 / 4 Pro', size: [225, 225, 265] },
    { id: 'elegoo-centauri-carbon', name: 'Centauri Carbon', size: [256, 256, 256] },
    { id: 'elegoo-neptune4-plus', name: 'Neptune 4 Plus', size: [320, 320, 385] },
    { id: 'elegoo-neptune4-max', name: 'Neptune 4 Max', size: [420, 420, 480] },
  ] },
];

const PLATES_BY_ID = new Map(PRINTER_PLATES.flatMap((group) =>
  group.models.map((model) => [model.id, { ...model, brand: group.brand, label: `${group.brand} ${model.name}` }])));

function findPlate(id) {
  return PLATES_BY_ID.get(String(id || '')) || null;
}

// Floating-point slack: a 256 mm cube on a 256 mm bed must read as a fit.
const PLATE_FIT_EPSILON = 1e-6;

// The plate exists to answer "will this print". The comparison is the model's SIZE against
// the build volume, not where the model happens to sit: OpenSCAD models are routinely
// centred on the origin (so half the part is below Z=0) and a slicer drops and centres them
// on the bed anyway. Judging by position would flag almost every model for a placement the
// user never has to make.
function plateFit(plate, bounds) {
  if (!plate || !bounds) return null;
  const size = [0, 1, 2].map((axis) => bounds.max[axis] - bounds.min[axis]);
  const over = ['X', 'Y', 'Z'].filter((_, axis) => size[axis] > plate.size[axis] + PLATE_FIT_EPSILON);
  return { fits: over.length === 0, over, size };
}

class MeshViewport {
  constructor(canvas, labelCanvas = null, scaleBadge = null) {
    this.canvas = canvas;
    this.labelCanvas = labelCanvas;
    this.labelContext = labelCanvas ? labelCanvas.getContext('2d') : null;
    this.scaleBadge = scaleBadge;
    this.scaleValue = scaleBadge ? scaleBadge.querySelector('b') : null;
    this.showGround = true;
    this.groundBuffer = null;
    this.groundGeometry = null;
    this.groundLabels = [];
    this.bounds = null;
    this.eye = [0, 0, 1];
    // A printer build plate swaps in here later: { name, size: [x, y] } bounds the surface
    // instead of the model bbox. Everything downstream already reads groundExtents().
    this.plate = null;
    this.gl = canvas.getContext('webgl', { antialias: true, alpha: true, preserveDrawingBuffer: true });
    this.center = [0, 0, 0];
    this.radius = 50;
    this.showEdges = false;
    this.showAxes = true;
    this.meshBatches = [];
    this.axesBuffer = null;
    if (!this.gl) throw new Error('WebGL is not available in this browser.');
    this.program = this.createProgram();
    this.locations = {
      position: this.gl.getAttribLocation(this.program, 'aPosition'),
      normal: this.gl.getAttribLocation(this.program, 'aNormal'),
      mvp: this.gl.getUniformLocation(this.program, 'uMvp'),
      color: this.gl.getUniformLocation(this.program, 'uColor'),
      lighting: this.gl.getUniformLocation(this.program, 'uLighting'),
    };
    this.resizeObserver = new ResizeObserver(() => this.draw());
    this.resizeObserver.observe(canvas);
  }

  createProgram() {
    const gl = this.gl;
    const vertex = `
      attribute vec3 aPosition;
      attribute vec3 aNormal;
      uniform mat4 uMvp;
      varying vec3 vNormal;
      void main() {
        gl_Position = uMvp * vec4(aPosition, 1.0);
        vNormal = aNormal;
      }`;
    const fragment = `
      precision mediump float;
      varying vec3 vNormal;
      uniform vec3 uColor;
      uniform float uLighting;
      void main() {
        vec3 n = normalize(vNormal);
        float key = max(dot(n, normalize(vec3(0.45, -0.35, 0.82))), 0.0);
        float fill = max(dot(n, normalize(vec3(-0.62, 0.3, 0.35))), 0.0) * 0.22;
        float shade = mix(1.0, 0.28 + key * 0.66 + fill, uLighting);
        gl_FragColor = vec4(uColor * shade, 1.0);
      }`;
    const compile = (type, source) => {
      const shader = gl.createShader(type);
      gl.shaderSource(shader, source);
      gl.compileShader(shader);
      if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(shader));
      return shader;
    };
    const program = gl.createProgram();
    gl.attachShader(program, compile(gl.VERTEX_SHADER, vertex));
    gl.attachShader(program, compile(gl.FRAGMENT_SHADER, fragment));
    gl.linkProgram(program);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(program));
    return program;
  }

  decodeStl(encoded) {
    const binary = atob(encoded);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
    const view = new DataView(bytes.buffer);
    if (bytes.length < 84) throw new Error('The native compiler returned an invalid STL file.');
    const triangles = view.getUint32(80, true);
    if (triangles === 0) throw new Error('The model contains no 3D geometry.');
    if (84 + triangles * 50 > bytes.length) throw new Error('The native compiler returned a truncated STL file.');

    const positions = new Float32Array(triangles * 9);
    const normals = new Float32Array(triangles * 9);
    const edges = new Float32Array(triangles * 18);
    const min = [Infinity, Infinity, Infinity];
    const max = [-Infinity, -Infinity, -Infinity];
    for (let triangle = 0; triangle < triangles; triangle += 1) {
      const offset = 84 + triangle * 50;
      const normal = [view.getFloat32(offset, true), view.getFloat32(offset + 4, true), view.getFloat32(offset + 8, true)];
      const vertices = [];
      for (let vertex = 0; vertex < 3; vertex += 1) {
        const vertexOffset = offset + 12 + vertex * 12;
        const point = [view.getFloat32(vertexOffset, true), view.getFloat32(vertexOffset + 4, true), view.getFloat32(vertexOffset + 8, true)];
        vertices.push(point);
        const base = triangle * 9 + vertex * 3;
        positions.set(point, base);
        normals.set(normal, base);
        for (let axis = 0; axis < 3; axis += 1) {
          min[axis] = Math.min(min[axis], point[axis]);
          max[axis] = Math.max(max[axis], point[axis]);
        }
      }
      const edgeBase = triangle * 18;
      edges.set([...vertices[0], ...vertices[1], ...vertices[1], ...vertices[2], ...vertices[2], ...vertices[0]], edgeBase);
    }
    return { positions, normals, edges, min, max, triangles };
  }

  createMeshBatch(id, encoded) {
    const decoded = this.decodeStl(encoded);
    const gl = this.gl;
    const positionBuffer = gl.createBuffer();
    const normalBuffer = gl.createBuffer();
    const edgeBuffer = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, positionBuffer);
    gl.bufferData(gl.ARRAY_BUFFER, decoded.positions, gl.STATIC_DRAW);
    gl.bindBuffer(gl.ARRAY_BUFFER, normalBuffer);
    gl.bufferData(gl.ARRAY_BUFFER, decoded.normals, gl.STATIC_DRAW);
    gl.bindBuffer(gl.ARRAY_BUFFER, edgeBuffer);
    gl.bufferData(gl.ARRAY_BUFFER, decoded.edges, gl.STATIC_DRAW);
    return { id, positionBuffer, normalBuffer, edgeBuffer, triangles: decoded.triangles, min: decoded.min, max: decoded.max };
  }

  // Drops every buffer and leaves the canvas empty.
  //
  // `hasMesh = false` alone only stops the *next* draw from being requested;
  // the last frame stays on the canvas, so starting a new workspace left the
  // previous model sitting behind the kick-start panel as though it were still
  // loaded. Releasing the buffers and painting once is what actually clears it.
  clearMesh() {
    for (const batch of this.meshBatches) {
      this.gl.deleteBuffer(batch.positionBuffer);
      this.gl.deleteBuffer(batch.normalBuffer);
      this.gl.deleteBuffer(batch.edgeBuffer);
    }
    this.meshBatches = [];
    this.bounds = null;
    this.center = [0, 0, 0];
    this.radius = 1;
    this.draw();
  }

  setMesh(encoded, objects = []) {
    for (const batch of this.meshBatches) {
      this.gl.deleteBuffer(batch.positionBuffer);
      this.gl.deleteBuffer(batch.normalBuffer);
      this.gl.deleteBuffer(batch.edgeBuffer);
    }
    // The render response carries the same geometry twice: once combined in `mesh` and
    // once split across `objects[].mesh`. Decoding both doubled every triangle's parse and
    // the peak Float32Array footprint to describe bounds we already have — the combined
    // extent is exactly the union of the parts, because `%` background geometry travels
    // under its own key and is excluded from `mesh` by the server. So decode once.
    const partMeshes = objects.length && objects.every((object) => typeof object.mesh === 'string' && object.mesh.length > 0);
    this.meshBatches = partMeshes
      ? objects.map((object) => this.createMeshBatch(object.id, object.mesh))
      : [this.createMeshBatch(objects[0]?.id || 'model', encoded)];
    const min = [Infinity, Infinity, Infinity];
    const max = [-Infinity, -Infinity, -Infinity];
    for (const batch of this.meshBatches) {
      for (let axis = 0; axis < 3; axis += 1) {
        min[axis] = Math.min(min[axis], batch.min[axis]);
        max[axis] = Math.max(max[axis], batch.max[axis]);
      }
    }
    this.bounds = { min, max };
    this.center = min.map((value, axis) => (value + max[axis]) / 2);
    this.radius = Math.max(1, Math.hypot(...max.map((value, axis) => (value - min[axis]) / 2)));
    // A new mesh re-frames: the distance is refitted, so the pan offset has to go with it,
    // or the refitted view would be aimed at empty space beside the model.
    camera[0] = camera[1] = camera[2] = 0;
    camera[6] = Math.max(this.radius * 4, 20);
    this.draw();
  }

  multiply(a, b) {
    const out = new Float32Array(16);
    for (let column = 0; column < 4; column += 1) {
      for (let row = 0; row < 4; row += 1) {
        out[column * 4 + row] = a[row] * b[column * 4] + a[4 + row] * b[column * 4 + 1] + a[8 + row] * b[column * 4 + 2] + a[12 + row] * b[column * 4 + 3];
      }
    }
    return out;
  }

  lookAt(eye, center, up) {
    const normalize = (v) => { const length = Math.hypot(...v) || 1; return v.map((n) => n / length); };
    const subtract = (a, b) => a.map((n, i) => n - b[i]);
    const cross = (a, b) => [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
    const dot = (a, b) => a.reduce((sum, n, i) => sum + n * b[i], 0);
    const z = normalize(subtract(eye, center));
    const x = normalize(cross(up, z));
    const y = cross(z, x);
    return new Float32Array([x[0], y[0], z[0], 0, x[1], y[1], z[1], 0, x[2], y[2], z[2], 0, -dot(x, eye), -dot(y, eye), -dot(z, eye), 1]);
  }

  // The camera target. camera[0..2] is the pan offset the user has dragged the view by,
  // in world millimetres; everything that framed on the model centre now frames on this,
  // so orbiting and zooming happen around what the user actually pulled into view.
  pivot() {
    return this.center.map((value, axis) => value + camera[axis]);
  }

  // Half the world height of the orthographic frame. Shared by the projection matrix, the
  // grid's view span and the pan scale so they can never drift out of agreement.
  orthoHalfHeight() {
    return Math.max(this.radius * 1.35, camera[6] * 0.36);
  }

  // The +X-on-screen and +Y-on-screen directions expressed in world space, derived from
  // yaw/pitch analytically so they match the basis lookAt() builds for the same angles.
  screenBasis() {
    const pitch = camera[3] * Math.PI / 180;
    const yaw = camera[5] * Math.PI / 180;
    return [
      [-Math.sin(yaw), Math.cos(yaw), 0],
      [-Math.sin(pitch) * Math.cos(yaw), -Math.sin(pitch) * Math.sin(yaw), Math.cos(pitch)],
    ];
  }

  // World height of the visible frame in the plane through the pivot, for the active
  // projection. The grid sizes its divisions against it and a pan drag divides by it.
  frameHeight() {
    if (projection === 'orthographic') return this.orthoHalfHeight() * 2;
    return 2 * camera[6] * Math.tan(VIEW_FOV / 2);
  }

  // World millimetres per CSS pixel in that same plane. This is what makes a pan drag 1:1 —
  // the point under the cursor stays under the cursor. Both projections are uniform in X
  // and Y (the aspect divides out), so one number covers both axes.
  worldPerPixel() {
    return this.frameHeight() / Math.max(1, this.canvas.clientHeight);
  }

  projectionMatrix(aspect) {
    // A pan is perpendicular to the view axis only at the instant of the drag: the offset is
    // stored in world space, so a later orbit turns it into depth. Both planes therefore
    // widen by the pan distance — the far one because the origin-centred grid recedes as the
    // eye moves off it, the near one because an orbited pan can bring the model to within
    // camera[6] - panned of the eye and would otherwise slice its front off.
    const panned = Math.hypot(camera[0], camera[1], camera[2]);
    const far = camera[6] + this.radius * 4 + panned + 100;
    // The floor is a fraction of the far plane rather than a fixed 0.01: once a pan pushes
    // the wanted near plane to zero, a hard 0.01 would spend the whole depth buffer on the
    // first millimetre in front of the eye and z-fight the model.
    const near = Math.max(far / 4000, camera[6] - panned - this.radius * 2.2);
    if (projection === 'orthographic') {
      const height = this.orthoHalfHeight();
      const width = height * aspect;
      return new Float32Array([1 / width, 0, 0, 0, 0, 1 / height, 0, 0, 0, 0, -2 / (far - near), 0, 0, 0, -(far + near) / (far - near), 1]);
    }
    const f = 1 / Math.tan(VIEW_FOV / 2);
    return new Float32Array([f / aspect, 0, 0, 0, 0, f, 0, 0, 0, 0, (far + near) / (near - far), -1, 0, 0, (2 * far * near) / (near - far), 0]);
  }

  scaleMatrix(scale) {
    return new Float32Array([scale, 0, 0, 0, 0, scale, 0, 0, 0, 0, scale, 0, 0, 0, 0, 1]);
  }

  createLineBuffer(data) {
    const gl = this.gl;
    const buffer = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
    gl.bufferData(gl.ARRAY_BUFFER, data, gl.STATIC_DRAW);
    return buffer;
  }

  // Shared entry point for every unlit world-space line overlay (axes today, a
  // scaled ground plane later): binds the mesh program with flat shading so the
  // colour uniform is used verbatim.
  beginLinePass(mvp) {
    const gl = this.gl;
    gl.useProgram(this.program);
    gl.uniformMatrix4fv(this.locations.mvp, false, mvp);
    gl.uniform1f(this.locations.lighting, 0);
    gl.disable(gl.CULL_FACE);
    gl.disable(gl.POLYGON_OFFSET_FILL);
    gl.enableVertexAttribArray(this.locations.position);
    gl.disableVertexAttribArray(this.locations.normal);
    gl.vertexAttrib3f(this.locations.normal, 0, 0, 1);
  }

  drawLineSegments(buffer, first, count, color) {
    const gl = this.gl;
    gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
    gl.vertexAttribPointer(this.locations.position, 3, gl.FLOAT, false, 0, 0);
    gl.uniform3f(this.locations.color, color[0], color[1], color[2]);
    gl.drawArrays(gl.LINES, first, count);
  }

  // Unit-length axis rays from the world origin; the draw call scales them so a
  // 2 mm part and a 400 mm part both get a readable indicator.
  ensureAxesBuffer() {
    if (!this.axesBuffer) {
      this.axesBuffer = this.createLineBuffer(new Float32Array([
        0, 0, 0, 1, 0, 0,
        0, 0, 0, 0, 1, 0,
        0, 0, 0, 0, 0, 1,
        0, 0, 0, -1, 0, 0,
        0, 0, 0, 0, -1, 0,
        0, 0, 0, 0, 0, -1,
      ]));
    }
    return this.axesBuffer;
  }

  // Measured from the pivot rather than the model centre: panning away from the origin has
  // to lengthen the rays, otherwise the datum they mark stops reaching the visible frame.
  axisLength() {
    const originOffset = Math.hypot(...this.pivot());
    return Math.max(this.radius * 1.6, (originOffset + this.radius) * 1.15, 1);
  }

  drawAxes(mvp) {
    const buffer = this.ensureAxesBuffer();
    this.beginLinePass(this.multiply(mvp, this.scaleMatrix(this.axisLength())));
    const colors = [[0.93, 0.31, 0.28], [0.51, 0.83, 0.35], [0.33, 0.63, 0.95]];
    colors.forEach((color, axis) => {
      this.drawLineSegments(buffer, axis * 2, 2, color);
      this.drawLineSegments(buffer, 6 + axis * 2, 2, color.map((channel) => channel * 0.4));
    });
  }

  // World size of the visible frame at the pivot distance, for the active projection.
  // Divisions are chosen against this, so a square is a comparable size on screen at any zoom.
  viewSpan() {
    const aspect = this.canvas.width / Math.max(1, this.canvas.height);
    const height = this.frameHeight();
    return Math.max(height, height * aspect);
  }

  // How much of the XY plane the surface has to cover: the part's own footprint (wherever
  // it sits relative to the origin) and the visible frame, so the grid never stops short of
  // the part. Deliberately no pan term. The surface, its origin lines and any bed outline
  // are all anchored to the world origin, so following a pan would mean sliding the grid
  // while those stay put; and feeding the pan into the span would re-pick the 1-2-5 step
  // mid-drag, flickering the divisions. A far pan therefore runs off the edge of a finite
  // grid, exactly as it does in a slicer, and Fit brings it back.
  groundSpan() {
    if (this.plate) return Math.max(this.plate.size[0], this.plate.size[1]) * 1.15;
    const footprint = this.bounds
      ? Math.max(...[0, 1].map((axis) => Math.max(Math.abs(this.bounds.max[axis]), Math.abs(this.bounds.min[axis])))) * 2.2
      : 0;
    return Math.max(footprint, (this.radius + Math.hypot(this.center[0], this.center[1])) * 2.2, this.viewSpan() * 1.3, 10);
  }

  // Divisions always land on a 1-2-5 step, so "one square" is a number a person can use.
  // Beds are rectangular, so each axis gets its own division count: a 256x256 plate and a
  // 250x210 one must not both render as the same square.
  groundExtents() {
    const span = this.groundSpan();
    const spacing = niceStep(span / GROUND_TARGET_CELLS);
    // Free grid: round the half-count up to a whole major block so the outermost line is
    // always a heavy one. A plate instead hugs its own footprint, because overshooting a
    // bed by half a major block would misrepresent where the build area ends.
    const cellsFor = (length, snap) => {
      const half = length / (2 * spacing);
      const wanted = snap ? Math.ceil(half / GROUND_MAJOR_EVERY) * GROUND_MAJOR_EVERY : Math.ceil(half);
      return Math.max(snap ? GROUND_MAJOR_EVERY * 2 : 2, Math.min(GROUND_MAX_CELLS, wanted));
    };
    const cellsX = this.plate ? cellsFor(this.plate.size[0], false) : cellsFor(span, true);
    const cellsY = this.plate ? cellsFor(this.plate.size[1], false) : cellsFor(span, true);
    // The bed rectangle and build-volume box live at exact millimetre coordinates, which are
    // not whole cells; the buffer is in cell units, so they are carried as fractions of one.
    const bed = this.plate
      ? [this.plate.size[0] / 2 / spacing, this.plate.size[1] / 2 / spacing, (this.plate.size[2] || 0) / spacing]
      : null;
    // Deliberately no single `cells`/`extent`: one square number cannot describe a
    // rectangular bed, and leaving one here would invite a caller to trust the wrong axis.
    return {
      spacing,
      cellsX,
      cellsY,
      bed,
      major: GROUND_MAJOR_EVERY,
      extentX: cellsX * spacing,
      extentY: cellsY * spacing,
    };
  }

  // Geometry is built once in CELL units and scaled by the current spacing at draw time,
  // so changing the step never reallocates a buffer.
  ensureGroundBuffer(metrics) {
    const key = `${metrics.cellsX}:${metrics.cellsY}:${metrics.bed ? metrics.bed.join(',') : ''}`;
    if (this.groundGeometry?.key === key) return this.groundGeometry;
    if (this.groundBuffer) this.gl.deleteBuffer(this.groundBuffer);
    const { cellsX, cellsY } = metrics;
    const minor = [];
    const major = [];
    const origin = [];
    for (let index = -cellsX; index <= cellsX; index += 1) {
      const bucket = index === 0 ? origin : index % GROUND_MAJOR_EVERY === 0 ? major : minor;
      bucket.push(index, -cellsY, -GROUND_DEPTH_BIAS, index, cellsY, -GROUND_DEPTH_BIAS);
    }
    for (let index = -cellsY; index <= cellsY; index += 1) {
      const bucket = index === 0 ? origin : index % GROUND_MAJOR_EVERY === 0 ? major : minor;
      bucket.push(-cellsX, index, -GROUND_DEPTH_BIAS, cellsX, index, -GROUND_DEPTH_BIAS);
    }
    // The bed outline: the printable rectangle, centred on the origin exactly as the grid is.
    const bed = [];
    // The build volume: four corner posts and a lid, so "does it fit" is answerable in Z too.
    const volume = [];
    if (metrics.bed) {
      const [x, y, z] = metrics.bed;
      const corners = [[-x, -y], [x, -y], [x, y], [-x, y]];
      for (let index = 0; index < 4; index += 1) {
        const [ax, ay] = corners[index];
        const [bx, by] = corners[(index + 1) % 4];
        bed.push(ax, ay, -GROUND_DEPTH_BIAS, bx, by, -GROUND_DEPTH_BIAS);
        if (z > 0) {
          volume.push(ax, ay, 0, ax, ay, z);
          volume.push(ax, ay, z, bx, by, z);
        }
      }
    }
    this.groundBuffer = this.createLineBuffer(new Float32Array([...minor, ...major, ...origin, ...bed, ...volume]));
    const range = (offset, values) => ({ first: offset / 3, count: values.length / 3 });
    this.groundGeometry = {
      key,
      cellsX,
      cellsY,
      buffer: this.groundBuffer,
      minor: range(0, minor),
      major: range(minor.length, major),
      origin: range(minor.length + major.length, origin),
      bed: range(minor.length + major.length + origin.length, bed),
      volume: range(minor.length + major.length + origin.length + bed.length, volume),
    };
    return this.groundGeometry;
  }

  drawGround(mvp, metrics) {
    const geometry = this.ensureGroundBuffer(metrics);
    this.beginLinePass(this.multiply(mvp, this.scaleMatrix(metrics.spacing)));
    this.drawLineSegments(geometry.buffer, geometry.minor.first, geometry.minor.count, [0.2, 0.231, 0.271]);
    this.drawLineSegments(geometry.buffer, geometry.major.first, geometry.major.count, [0.282, 0.322, 0.373]);
    this.drawLineSegments(geometry.buffer, geometry.origin.first, geometry.origin.count, [0.416, 0.463, 0.514]);
    // The bed edge reads as a datum, so it borrows the origin colour; the volume above it is
    // context rather than a boundary and stays at the major-line weight.
    if (geometry.volume.count) this.drawLineSegments(geometry.buffer, geometry.volume.first, geometry.volume.count, [0.282, 0.322, 0.373]);
    if (geometry.bed.count) this.drawLineSegments(geometry.buffer, geometry.bed.first, geometry.bed.count, [0.416, 0.463, 0.514]);
  }

  // Numbers are 2D-canvas text placed through projectPoint(), not billboarded geometry:
  // screen-space text can never end up mirrored or upside down as the camera orbits.
  drawGroundLabels(metrics) {
    const canvas = this.labelCanvas;
    const context = this.labelContext;
    this.groundLabels = [];
    // Written only on change: draw() runs on every orbit frame.
    if (this.scaleBadge && this.scaleBadge.hidden === this.showGround) this.scaleBadge.hidden = !this.showGround;
    const readout = `${formatMillimetres(metrics.spacing)} mm`;
    if (this.scaleValue && this.scaleValue.textContent !== readout) this.scaleValue.textContent = readout;
    if (!canvas || !context) return;
    if (canvas.width !== this.canvas.width || canvas.height !== this.canvas.height) {
      canvas.width = this.canvas.width;
      canvas.height = this.canvas.height;
    }
    context.clearRect(0, 0, canvas.width, canvas.height);
    if (!this.showGround) return;
    const ratio = Math.max(1, canvas.width / Math.max(1, canvas.clientWidth));
    const step = metrics.spacing * metrics.major;
    // Per axis, because a rectangular bed makes the two half-counts different. floor(), not
    // a ratio: a plate's cell count is not rounded to a whole major block.
    const ticksX = Math.floor(metrics.cellsX / metrics.major);
    const ticksY = Math.floor(metrics.cellsY / metrics.major);
    const candidates = [];
    for (let tick = -ticksX; tick <= ticksX; tick += 1) {
      const value = tick * step;
      candidates.push({ text: formatMillimetres(value), from: [value, -metrics.extentY, 0], to: [value, metrics.extentY, 0] });
    }
    for (let tick = -ticksY; tick <= ticksY; tick += 1) {
      const value = tick * step;
      candidates.push({ text: formatMillimetres(value), from: [-metrics.extentX, value, 0], to: [metrics.extentX, value, 0] });
    }
    // Keep clear of the viewport furniture: the hint/scale row along the bottom and the
    // zoom cluster in the corner. A blocked number falls back to the far end of its line.
    const margin = { left: 15 * ratio, right: 15 * ratio, top: 15 * ratio, bottom: 32 * ratio };
    const blocked = (point) => point.x > canvas.width - 46 * ratio && point.y > canvas.height - 104 * ratio;
    context.font = `${Math.round(9 * ratio)}px "IBM Plex Mono", "SFMono-Regular", Menlo, monospace`;
    context.textAlign = 'center';
    context.textBaseline = 'middle';
    context.lineJoin = 'round';
    context.lineWidth = 3 * ratio;
    context.strokeStyle = 'rgba(14, 17, 21, .82)';
    context.fillStyle = '#8b96a3';
    for (const candidate of candidates) {
      const crossings = this.gridlineAnchor(candidate.from, candidate.to, margin);
      if (!crossings) continue;
      // Crowded corners would stack numbers on top of each other; one per neighbourhood.
      const anchor = [crossings.near, crossings.far].find((point) => !blocked(point)
        && !this.groundLabels.some((placed) => Math.hypot(placed.x - point.x, placed.y - point.y) < 26 * ratio));
      if (!anchor) continue;
      context.strokeText(candidate.text, anchor.x, anchor.y);
      context.fillText(candidate.text, anchor.x, anchor.y);
      this.groundLabels.push({ text: candidate.text, x: anchor.x, y: anchor.y });
    }
  }

  // Where a major gridline enters and leaves the frame (Liang–Barsky against the inset
  // canvas rect), ordered near/far in world space. Anchoring numbers to the border keeps
  // them on screen no matter how far the surface overshoots the viewport, and drawing them
  // as screen-space text keeps them upright at every orbit angle.
  gridlineAnchor(from, to, margin) {
    const start = this.projectPoint(from);
    const end = this.projectPoint(to);
    if (!start || !end || start.behind || end.behind) return null;
    const dx = end.x - start.x;
    const dy = end.y - start.y;
    let low = 0;
    let high = 1;
    const limits = [
      [-dx, start.x - margin.left],
      [dx, this.labelCanvas.width - margin.right - start.x],
      [-dy, start.y - margin.top],
      [dy, this.labelCanvas.height - margin.bottom - start.y],
    ];
    for (const [p, q] of limits) {
      if (p === 0) { if (q < 0) return null; continue; }
      const t = q / p;
      if (p < 0) { if (t > high) return null; if (t > low) low = t; } else { if (t < low) return null; if (t < high) high = t; }
    }
    const at = (t) => ({ x: start.x + dx * t, y: start.y + dy * t });
    const startIsNearer = Math.hypot(from[0] - this.eye[0], from[1] - this.eye[1], from[2] - this.eye[2])
      <= Math.hypot(to[0] - this.eye[0], to[1] - this.eye[1], to[2] - this.eye[2]);
    return startIsNearer ? { near: at(low), far: at(high) } : { near: at(high), far: at(low) };
  }

  draw() {
    const gl = this.gl;
    const ratio = Math.min(devicePixelRatio || 1, 2);
    const width = Math.max(1, Math.floor(this.canvas.clientWidth * ratio));
    const height = Math.max(1, Math.floor(this.canvas.clientHeight * ratio));
    if (this.canvas.width !== width || this.canvas.height !== height) {
      this.canvas.width = width;
      this.canvas.height = height;
    }
    const pitch = camera[3] * Math.PI / 180;
    const yaw = camera[5] * Math.PI / 180;
    // Orbit and zoom are both defined against the pivot, so a panned view stays panned
    // through a rotation instead of snapping back to the middle of the model.
    const pivot = this.pivot();
    const eye = [
      pivot[0] + camera[6] * Math.cos(pitch) * Math.cos(yaw),
      pivot[1] + camera[6] * Math.cos(pitch) * Math.sin(yaw),
      pivot[2] + camera[6] * Math.sin(pitch),
    ];
    const view = this.lookAt(eye, pivot, [0, 0, 1]);
    const mvp = this.multiply(this.projectionMatrix(width / height), view);
    this.mvp = mvp;
    this.eye = eye;
    gl.viewport(0, 0, width, height);
    gl.clearColor(0, 0, 0, 0);
    gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);
    gl.enable(gl.DEPTH_TEST);
    gl.enable(gl.CULL_FACE);
    gl.cullFace(gl.BACK);
    gl.useProgram(this.program);
    gl.uniformMatrix4fv(this.locations.mvp, false, mvp);
    gl.enableVertexAttribArray(this.locations.position);
    gl.enableVertexAttribArray(this.locations.normal);
    gl.uniform1f(this.locations.lighting, 1);
    gl.enable(gl.POLYGON_OFFSET_FILL);
    gl.polygonOffset(1, 1);
    for (const batch of this.meshBatches) {
      const object = sceneObjects.find((item) => item.id === batch.id);
      if (object?.visible === false) continue;
      gl.bindBuffer(gl.ARRAY_BUFFER, batch.positionBuffer);
      gl.vertexAttribPointer(this.locations.position, 3, gl.FLOAT, false, 0, 0);
      gl.bindBuffer(gl.ARRAY_BUFFER, batch.normalBuffer);
      gl.vertexAttribPointer(this.locations.normal, 3, gl.FLOAT, false, 0, 0);
      const selected = batch.id === selectedObjectId;
      const intersecting = intersectionIds.has(batch.id);
      if (intersecting) gl.uniform3f(this.locations.color, 0.91, 0.25, 0.20);
      else if (selected) gl.uniform3f(this.locations.color, 0.36, 0.72, 0.95);
      else gl.uniform3f(this.locations.color, 0.98, 0.77, 0.19);
      gl.drawArrays(gl.TRIANGLES, 0, batch.triangles * 3);
    }
    gl.disable(gl.POLYGON_OFFSET_FILL);
    if (this.showEdges) {
      gl.disable(gl.CULL_FACE);
      gl.disableVertexAttribArray(this.locations.normal);
      gl.vertexAttrib3f(this.locations.normal, 0, 0, 1);
      gl.uniform3f(this.locations.color, 0.18, 0.16, 0.11);
      gl.uniform1f(this.locations.lighting, 0);
      for (const batch of this.meshBatches) {
        const object = sceneObjects.find((item) => item.id === batch.id);
        if (object?.visible === false) continue;
        gl.bindBuffer(gl.ARRAY_BUFFER, batch.edgeBuffer);
        gl.vertexAttribPointer(this.locations.position, 3, gl.FLOAT, false, 0, 0);
        gl.drawArrays(gl.LINES, 0, batch.triangles * 6);
      }
    }
    // Depth test stays on: the axes and the ground live in the scene, so solid geometry
    // occludes them exactly like a real datum would instead of floating over the model.
    const metrics = this.groundExtents();
    if (this.showGround) this.drawGround(mvp, metrics);
    if (this.showAxes) this.drawAxes(mvp);
    this.drawGroundLabels(metrics);
  }

  projectPoint(point) {
    const mvp = this.mvp;
    if (!mvp) return null;
    const clip = [0, 1, 2, 3].map((row) => mvp[row] * point[0] + mvp[4 + row] * point[1] + mvp[8 + row] * point[2] + mvp[12 + row]);
    if (!clip[3]) return null;
    return {
      x: (clip[0] / clip[3] * 0.5 + 0.5) * this.canvas.width,
      y: (0.5 - clip[1] / clip[3] * 0.5) * this.canvas.height,
      // A point behind the eye projects mirrored; callers that place screen furniture
      // (grid labels) must skip it rather than draw it in the wrong corner.
      behind: clip[3] < 0,
    };
  }

  downloadPng(name) {
    this.draw();
    this.canvas.toBlob((blob) => blob && downloadBlob(blob, name), 'image/png');
  }
}

const meshViewport = new MeshViewport(renderCanvas, $('#groundLabels'), $('#gridScale'));

function escapeHtml(value) {
  return String(value).replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
}

function escapeAttribute(value) {
  return escapeHtml(value).replaceAll('"', '&quot;').replaceAll("'", '&#39;');
}

function highlightTokens(code) {
  const tokenPattern = /("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|\/\/[^\n]*|\/\*[\s\S]*?\*\/|\b(?:module|function|for|intersection_for|if|else|let|each|true|false|undef|use|include)\b|\b(?:cube|sphere|cylinder|polyhedron|circle|square|polygon|text|union|difference|intersection|hull|minkowski|translate|rotate|scale|resize|mirror|multmatrix|color|offset|projection|linear_extrude|rotate_extrude|surface|import|children|echo|assert)\b|(?<![\w$])-?\d+(?:\.\d+)?\b)/g;
  let cursor = 0;
  let result = '';
  for (const match of code.matchAll(tokenPattern)) {
    result += escapeHtml(code.slice(cursor, match.index));
    const token = match[0];
    let type = 'number';
    if (token.startsWith('//') || token.startsWith('/*')) type = 'comment';
    else if (token.startsWith('"') || token.startsWith("'")) type = 'string';
    else if (/^(module|function|for|intersection_for|if|else|let|each|true|false|undef|use|include)$/.test(token)) type = 'keyword';
    else if (/^[A-Za-z_]/.test(token)) type = 'builtin';
    result += `<span class="tok-${type}">${escapeHtml(token)}</span>`;
    cursor = match.index + token.length;
  }
  return result + escapeHtml(code.slice(cursor));
}

function syntaxHighlight(code) {
  const ranges = aiUpdatedRanges
    .map((range) => ({ start: Math.max(0, Number(range.start ?? range.from ?? 0)), end: Math.min(code.length, Number(range.end ?? range.to ?? 0)) }))
    .filter((range) => Number.isFinite(range.start) && Number.isFinite(range.end) && range.end > range.start)
    .sort((a, b) => a.start - b.start);
  if (!ranges.length) return `${highlightTokens(code)}\n`;
  let cursor = 0;
  let result = '';
  for (const range of ranges) {
    const start = Math.max(cursor, range.start);
    if (range.end <= start) continue;
    result += highlightTokens(code.slice(cursor, start));
    result += `<mark class="ai-code-change">${highlightTokens(code.slice(start, range.end))}</mark>`;
    cursor = range.end;
  }
  return `${result}${highlightTokens(code.slice(cursor))}\n`;
}

function syncEditor(markDirty = true) {
  highlight.innerHTML = syntaxHighlight(editor.value);
  lineNumbers.textContent = Array.from({ length: editor.value.split('\n').length }, (_, i) => i + 1).join('\n');
  if (markDirty) {
    dirtyIndicator.classList.add('visible');
    scheduleWorkspaceSave();
  }
  updateCursor();
  updateCustomizer();
  updateKickstart();
}

function syncScroll() {
  highlight.scrollTop = editor.scrollTop;
  highlight.scrollLeft = editor.scrollLeft;
  lineNumbers.scrollTop = editor.scrollTop;
}

function updateCursor() {
  const before = editor.value.slice(0, editor.selectionStart);
  const lines = before.split('\n');
  $('#cursorPosition').textContent = `Ln ${lines.length}, Col ${lines.at(-1).length + 1}`;
}

// Download names only. Nothing here reaches the header or the editor chrome.
function setDownloadBaseName(name) {
  const cleaned = String(name || '').replace(/\.scad$/i, '').replace(/[\\/:*?"<>|]+/g, '-').trim();
  downloadBaseName = cleaned || 'model';
}

function downloadFileName(extension) {
  return `${downloadBaseName}.${extension}`;
}

function setStatus(text, state = 'ready') {
  statusText.textContent = text;
  statusDot.className = `status-ready ${state === 'ready' ? '' : state}`;
}

function log(message, kind = 'info') {
  const now = new Date().toLocaleTimeString([], { hour12: false });
  const row = document.createElement('div');
  const cleaned = String(message || '').trim() || 'No compiler diagnostics.';
  row.innerHTML = `<span class="timestamp">${now}</span><span class="${kind}"></span>`;
  row.lastElementChild.textContent = cleaned;
  consoleOutput.append(row);
  consoleOutput.scrollTop = consoleOutput.scrollHeight;
}

const ID_ALPHABET = 'abcdefghijklmnopqrstuvwxyz0123456789';

function randomToken(length) {
  const bytes = new Uint8Array(length);
  if (globalThis.crypto?.getRandomValues) crypto.getRandomValues(bytes);
  else for (let index = 0; index < length; index += 1) bytes[index] = Math.floor(Math.random() * 256);
  return [...bytes].map((byte) => ID_ALPHABET[byte % ID_ALPHABET.length]).join('');
}

// The server accepts 16..=96 chars of [a-z0-9-]; a fixed-width random tail keeps every
// offline id inside that window so it can still sync once the backend returns.
function generateFunnyId() {
  const adjective = funnyWords.adjectives[Math.floor(Math.random() * funnyWords.adjectives.length)];
  const noun = funnyWords.nouns[Math.floor(Math.random() * funnyWords.nouns.length)];
  const id = `${adjective}-${noun}-${randomToken(12)}`;
  return id.length >= 16 ? id.slice(0, 96) : `${id}${randomToken(16 - id.length)}`;
}

function setSyncState(label, state = 'local') {
  const sync = $('#syncState');
  sync.textContent = label;
  sync.dataset.state = state;
}

function workspaceFromPayload(payload) {
  return payload?.workspace || payload || {};
}

function advanceWorkspaceGeneration() {
  workspaceGeneration += 1;
  clearTimeout(eventPollTimer);
  eventPollTimer = null;
  eventPollController?.abort();
  eventPollController = null;
  clearTimeout(workspaceSaveDebounce);
  workspaceSaveDebounce = null;
  workspaceSaveController?.abort();
  workspaceSaveController = null;
  workspaceSavePromise = null;
  workspaceConflict = null;
  workspaceHasLocalChanges = false;
  workspaceSaveFailures = 0;
  nextWorkspaceNavigation = 'replace';
  if (activeRender) cancelRender(activeRender);
  return workspaceGeneration;
}

function hasPendingLocalWorkspaceChanges() {
  return workspaceHasLocalChanges || workspaceSaveDebounce !== null || workspaceSavePromise !== null;
}

// navigation: 'replace' rewrites the current entry, 'push' adds one (so Back returns to the
// workspace you came from), 'none' leaves history untouched (we are already on that URL).
function setWorkspaceIdentity(id, navigation = nextWorkspaceNavigation) {
  nextWorkspaceNavigation = 'replace';
  // Whatever the Share popover is showing belongs to the workspace being left.
  // Closing it is the honest move: leaving it open would display one
  // workspace's links under another's name, and a link is a credential.
  if (id !== workspaceId) {
    const popover = document.getElementById('sharePopover');
    if (popover) popover.hidden = true;
    shareTokenCache = { id: null, promise: null };
    kickstartDismissed = false;
  }
  workspaceId = id;
  $('#workspaceName').textContent = id;
  setDownloadBaseName(id);
  const expectedPath = `/workspaces/${encodeURIComponent(id)}`;
  if (navigation === 'none' || location.pathname === expectedPath) return;
  if (navigation === 'push') history.pushState({ workspaceId: id }, '', expectedPath);
  else history.replaceState({ workspaceId: id }, '', expectedPath);
}

function workspaceStorageKey(id = workspaceId) {
  return `reopenscad.workspace.${id || 'draft'}`;
}

function storeLocalWorkspace() {
  if (!workspaceId) return;
  localStorage.setItem(workspaceStorageKey(), JSON.stringify({ code: editor.value, revision: workspaceRevision }));
}

function finalAiUpdate(payload, workspace) {
  const events = workspace.updatedRanges || payload?.updatedRanges || [];
  if (!Array.isArray(events) || !events.length) return { isAi: false, ranges: [] };
  const finalEvent = events.reduce((latest, event) => Number(event.revision) >= Number(latest.revision) ? event : latest);
  if (finalEvent.source !== 'ai') return { isAi: false, ranges: [] };
  return {
    isAi: true,
    ranges: events.filter((event) => event.source === 'ai' && Number(event.revision) === Number(finalEvent.revision)),
  };
}

function surfaceWorkspaceConflict(payload) {
  workspaceConflict = payload;
  setSyncState('Resolve conflict', 'error');
  $('#syncState').title = 'Click to choose between your local source and the newer workspace revision.';
  log('Remote changes conflict with unsaved source. Your editor and local cache were preserved; click “Resolve conflict” to choose a version.', 'warning');
}

// The workspace is authoritative for its own viewer settings — that is what makes them
// travel with the shared URL. Whatever it carries also becomes the local default, so the
// next NEW workspace opens on the same bed and clearance.
function applyWorkspaceSettings(workspace) {
  // A poll answering an AI code edit carries whatever settings the server had when it
  // replied. If the user has just changed one and that PATCH is still in flight, the reply
  // is stale by definition and would silently undo the choice they just made.
  if (settingsSavesInFlight.has(String(workspace.id ?? workspaceId))) return;
  // Every branch below is a no-op when the value already matches. This runs on every poll —
  // twice a second — so reassigning unconditionally would redraw the viewport continuously
  // and, worse, reset the select and the tolerance field out from under a user mid-edit.
  if (typeof workspace.plate === 'string' && workspace.plate !== (meshViewport.plate?.id ?? '')) {
    const plate = applyPlate(workspace.plate);
    // Only a plate the workspace actually names is promoted to the local default. Every
    // plateless workspace serializes plate:"" — writing that through would mean opening one
    // pre-existing or shared link erased the bed the user owns, and the next new workspace
    // would come up bare. The empty case is still applied to the viewport above, because the
    // workspace is authoritative for itself; it just is not a new default.
    if (plate) localStorage.setItem(PLATE_STORAGE_KEY, plate.id);
  }
  const tolerance = Number(workspace.tolerance);
  if (Number.isFinite(tolerance) && tolerance >= 0 && tolerance !== readTolerance()) {
    $('#toleranceInput').value = String(tolerance);
    localStorage.setItem(TOLERANCE_STORAGE_KEY, String(tolerance));
  }
}

function applyWorkspace(payload, { remote = false, force = false } = {}) {
  const workspace = workspaceFromPayload(payload);
  // Into the address book — but only once there is something in it. Landing on
  // "/" mints an empty workspace every time, and recording those would bury the
  // user's actual work under a stack of blanks they never typed in. A workspace
  // that gains content later is picked up by `uploadWorkspacePreview`.
  if (!remote && workspace.id && (workspace.code || '').trim()) {
    rememberWorkspace(workspace.id);
  }
  const incomingRevision = Number(workspace.revision);
  let shouldRenderAiUpdate = false;
  if (remote && !force && typeof workspace.code === 'string' && workspace.code !== editor.value && hasPendingLocalWorkspaceChanges()) {
    surfaceWorkspaceConflict(payload);
    return false;
  }
  if (workspace.id) setWorkspaceIdentity(String(workspace.id));
  if (Number.isFinite(incomingRevision)) workspaceRevision = incomingRevision;
  if (typeof workspace.code === 'string' && workspace.code !== editor.value) {
    applyingRemoteUpdate = true;
    editor.value = workspace.code;
    const aiUpdate = remote ? finalAiUpdate(payload, workspace) : { isAi: false, ranges: [] };
    aiUpdatedRanges = aiUpdate.isAi ? normalizeUpdatedRanges(workspace.code, aiUpdate.ranges) : [];
    $('#aiRevisionBadge').hidden = !aiUpdate.isAi;
    syncEditor(false);
    applyingRemoteUpdate = false;
    if (aiUpdate.isAi) {
      $('#aiRevisionBadge').hidden = false;
      log(`AI updated workspace revision ${workspaceRevision}.`, 'success');
      shouldRenderAiUpdate = true;
    }
  }
  workspaceSavedCode = typeof workspace.code === 'string' ? workspace.code : editor.value;
  workspaceHasLocalChanges = false;
  workspaceConflict = null;
  $('#syncState').title = '';
  // Not on a forced conflict resolution: that replays a payload captured when the banner
  // first appeared, so its settings are a stale snapshot of what the user may since have
  // changed. Resolving a source conflict must not also roll back the build plate.
  if (!force) applyWorkspaceSettings(workspace);
  if (Array.isArray(workspace.objects) && (workspace.objects.length || !hasMesh)) setSceneObjects(workspace.objects);
  updateKickstart();
  storeLocalWorkspace();
  if (shouldRenderAiUpdate) render('preview');
  return true;
}

function byteOffsetToStringIndex(value, byteOffset) {
  const target = Math.max(0, Number(byteOffset) || 0);
  let bytes = 0;
  let index = 0;
  const encoder = new TextEncoder();
  for (const character of value) {
    if (bytes >= target) break;
    bytes += encoder.encode(character).length;
    index += character.length;
  }
  return index;
}

function normalizeUpdatedRanges(code, ranges) {
  return (Array.isArray(ranges) ? ranges : []).map((range) => {
    const byteStart = Number(range.start ?? range.from ?? 0);
    const insertedLength = Number(range.insertedLength);
    const byteEnd = Number.isFinite(insertedLength) ? byteStart + insertedLength : Number(range.end ?? range.to ?? byteStart);
    return {
      start: byteOffsetToStringIndex(code, byteStart),
      end: byteOffsetToStringIndex(code, byteEnd),
    };
  });
}

// A brand new workspace starts genuinely empty so the kick-start options are what a first
// visitor sees. `navigation` decides whether the previous workspace stays reachable via Back.
async function createWorkspace(code = '', { navigation = 'replace' } = {}) {
  leaveReadOnly();
  const generation = advanceWorkspaceGeneration();
  nextWorkspaceNavigation = navigation;
  setSyncState('Creating', 'saving');
  try {
    const response = await fetch('/api/workspaces', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      // Seeding the new workspace with the settings on screen is what "preserve it in case
      // the user creates a new workspace" means: the bed carries over instead of resetting.
      body: JSON.stringify({
        code,
        plate: localStorage.getItem(PLATE_STORAGE_KEY) || '',
        // `|| DEFAULT` would turn a deliberate 0 mm clearance back into 0.2.
        tolerance: readTolerance() ?? DEFAULT_TOLERANCE,
      }),
    });
    if (!response.ok) throw new Error(`Workspace service returned ${response.status}`);
    const payload = await response.json();
    if (generation !== workspaceGeneration) return;
    workspaceOnline = true;
    applyWorkspace(payload);
    setSyncState('Synced', 'synced');
  } catch {
    if (generation !== workspaceGeneration) return;
    workspaceOnline = false;
    const id = generateFunnyId();
    setWorkspaceIdentity(id);
    workspaceRevision = 1;
    editor.value = code;
    syncEditor(false);
    workspaceSavedCode = '';
    storeLocalWorkspace();
    setSyncState('Local', 'local');
  }
  if (generation === workspaceGeneration) beginWorkspacePolling(generation);
}

async function loadWorkspace(id) {
  leaveReadOnly();
  const generation = advanceWorkspaceGeneration();
  setWorkspaceIdentity(id, 'none');
  setSyncState('Loading', 'saving');
  const cached = JSON.parse(localStorage.getItem(workspaceStorageKey(id)) || 'null');
  try {
    const response = await fetch(`/api/workspaces/${encodeURIComponent(id)}?allowMissing=1`);
    const payload = await response.json();
    if (response.ok && payload.missing === true) {
      await createWorkspace(cached?.code || '');
      return;
    }
    if (!response.ok) throw new Error(`Workspace service returned ${response.status}`);
    if (generation !== workspaceGeneration || id !== workspaceId) return;
    workspaceOnline = true;
    const remote = workspaceFromPayload(payload);
    const cachedRevision = Number(cached?.revision);
    const remoteRevision = Number(remote.revision);
    // Viewer settings are not part of the source, so they restore on every path — including
    // the divergent-cache and conflict branches below, which never reach applyWorkspace().
    applyWorkspaceSettings(remote);
    if (cached?.code && cached.code !== remote.code) {
      editor.value = cached.code;
      syncEditor(false);
      workspaceSavedCode = remote.code;
      workspaceRevision = Number.isFinite(cachedRevision) ? cachedRevision : remoteRevision;
      workspaceHasLocalChanges = true;
      if (Number.isFinite(remoteRevision) && remoteRevision > workspaceRevision) {
        surfaceWorkspaceConflict(payload);
      } else {
        setSyncState('Unsaved', 'saving');
        scheduleWorkspaceSave();
      }
    } else {
      applyWorkspace(payload);
      setSyncState('Synced', 'synced');
    }
  } catch {
    if (generation !== workspaceGeneration || id !== workspaceId) return;
    workspaceOnline = false;
    if (cached?.code) {
      workspaceRevision = Number(cached.revision) || 1;
      editor.value = cached.code;
    } else {
      // Never seed from another workspace's cache: an unknown workspace starts empty.
      editor.value = '';
    }
    syncEditor(false);
    workspaceSavedCode = '';
    setSyncState('Local', 'local');
  }
  if (generation === workspaceGeneration) beginWorkspacePolling(generation);
}

// Viewer settings ride their own revisionless PATCH rather than piggybacking on the source
// save: picking a plate is not an edit, so it must not bump the revision, mark the source
// dirty, or be able to lose a race with an in-flight code save.
//
// It retries, because a silently dropped settings save is the exact failure the feature
// cannot afford: the value would live on only in localStorage and never travel with the
// workspace URL, which is half of what the setting is for. The request is also bounded by a
// timeout — an unresolved fetch would otherwise pin the in-flight guard below forever and
// permanently stop this workspace from picking up remote settings.
const SETTINGS_SAVE_TIMEOUT_MS = 10_000;
const SETTINGS_SAVE_ATTEMPTS = 4;

function saveWorkspaceSettings() {
  if (applyingRemoteUpdate || !workspaceId) return;
  const id = workspaceId;
  const generation = workspaceGeneration;
  // The guard is claimed once and held across the whole retry chain, including the pauses
  // between attempts. Releasing it per attempt would leave a window in which a poll could
  // apply the very value we are still trying to overwrite.
  settingsSavesInFlight.set(id, (settingsSavesInFlight.get(id) || 0) + 1);
  const release = () => {
    const remaining = (settingsSavesInFlight.get(id) || 1) - 1;
    if (remaining > 0) settingsSavesInFlight.set(id, remaining);
    else settingsSavesInFlight.delete(id);
  };
  const attemptSave = (attempt) => {
    const body = { plate: meshViewport.plate ? meshViewport.plate.id : '', tolerance: readTolerance() ?? DEFAULT_TOLERANCE };
    fetch(`/api/workspaces/${encodeURIComponent(id)}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(SETTINGS_SAVE_TIMEOUT_MS),
    }).then((response) => {
      if (!response.ok) throw new Error(`Workspace service returned ${response.status}`);
      workspaceOnline = true;
      release();
    }).catch(() => {
      // Stop once this is no longer the workspace on screen: retrying would write settings
      // the user has already navigated away from.
      if (generation !== workspaceGeneration || id !== workspaceId) { release(); return; }
      if (attempt + 1 >= SETTINGS_SAVE_ATTEMPTS) {
        release();
        log('Build plate and tolerance could not be saved to this workspace; they are kept locally.', 'warning');
        return;
      }
      setTimeout(() => attemptSave(attempt + 1), Math.min(8000, 700 * 2 ** attempt));
    });
  };
  attemptSave(0);
}

function scheduleWorkspaceSave(delay = 650) {
  if (applyingRemoteUpdate || !workspaceId) return;
  workspaceHasLocalChanges = true;
  storeLocalWorkspace();
  clearTimeout(workspaceSaveDebounce);
  workspaceSaveDebounce = setTimeout(() => {
    workspaceSaveDebounce = null;
    saveWorkspace();
  }, delay);
}

async function saveWorkspace() {
  if (!workspaceId || workspaceConflict) return false;
  if (workspaceSavePromise) return workspaceSavePromise;
  clearTimeout(workspaceSaveDebounce);
  workspaceSaveDebounce = null;
  const id = workspaceId;
  const generation = workspaceGeneration;
  const baseRevision = workspaceRevision;
  const code = editor.value;
  const controller = new AbortController();
  workspaceSaveController = controller;
  setSyncState('Saving', 'saving');
  const operation = (async () => {
    try {
      const response = await fetch(`/api/workspaces/${encodeURIComponent(id)}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
        signal: controller.signal,
        body: JSON.stringify({ code, baseRevision, source: 'browser' }),
      });
      const payload = await response.json();
      if (generation !== workspaceGeneration || id !== workspaceId) return false;
      if (response.status === 409) {
        surfaceWorkspaceConflict(payload);
        storeLocalWorkspace();
        return false;
      }
      if (!response.ok) throw new Error(`Workspace service returned ${response.status}`);
      workspaceOnline = true;
      workspaceSaveFailures = 0;
      const workspace = workspaceFromPayload(payload);
      if (Number.isFinite(Number(workspace.revision))) workspaceRevision = Number(workspace.revision);
      workspaceSavedCode = code;
      workspaceHasLocalChanges = editor.value !== code;
      storeLocalWorkspace();
      setSyncState(workspaceHasLocalChanges ? 'Unsaved' : 'Synced', workspaceHasLocalChanges ? 'saving' : 'synced');
      if (workspaceHasLocalChanges) scheduleWorkspaceSave();
      return !workspaceHasLocalChanges;
    } catch (error) {
      if (generation !== workspaceGeneration || id !== workspaceId || error.name === 'AbortError') return false;
      workspaceOnline = false;
      // A transient failure must not strand the edit until the user happens to type again:
      // keep it queued and retry with exponential backoff (capped) until it lands.
      workspaceSaveFailures += 1;
      const backoff = Math.min(15000, 800 * 2 ** (workspaceSaveFailures - 1));
      setSyncState('Retrying', 'saving');
      scheduleWorkspaceSave(backoff);
      return false;
    }
  })();
  workspaceSavePromise = operation;
  try {
    return await operation;
  } finally {
    if (workspaceSavePromise === operation) workspaceSavePromise = null;
    if (workspaceSaveController === controller) workspaceSaveController = null;
  }
}

async function flushWorkspaceSave() {
  clearTimeout(workspaceSaveDebounce);
  workspaceSaveDebounce = null;
  if (workspaceSavePromise) await workspaceSavePromise;
  if (workspaceConflict) return false;
  if (workspaceHasLocalChanges) await saveWorkspace();
  return workspaceOnline
    && !workspaceConflict
    && !workspaceHasLocalChanges
    && workspaceSavedCode === editor.value;
}

function beginWorkspacePolling(generation = workspaceGeneration) {
  clearTimeout(eventPollTimer);
  const poll = async () => {
    if (generation !== workspaceGeneration) return;
    if (!workspaceId || document.hidden || workspaceConflict) { eventPollTimer = setTimeout(poll, 2500); return; }
    const id = workspaceId;
    const sinceRevision = workspaceRevision;
    const controller = new AbortController();
    eventPollController = controller;
    const timeout = setTimeout(() => controller.abort(), 25000);
    try {
      const response = await fetch(`/api/workspaces/${encodeURIComponent(id)}/events?since=${sinceRevision}`, { signal: controller.signal });
      if (response.ok && response.status !== 204) {
        const payload = await response.json();
        if (generation !== workspaceGeneration || id !== workspaceId) return;
        workspaceOnline = true;
        // Viewer settings are revisionless, so `changed` says nothing about them: sync them
        // on every poll, which is how a plate picked in another tab (or by an MCP agent)
        // reaches this one. Guarded against our own in-flight save inside the helper.
        applyWorkspaceSettings({ ...workspaceFromPayload(payload), id });
        const incomingRevision = Number(workspaceFromPayload(payload).revision);
        if (payload.changed === true && Number.isFinite(incomingRevision) && incomingRevision > workspaceRevision) {
          const applied = applyWorkspace(payload, { remote: true });
          if (applied) setSyncState('Synced', 'synced');
        } else if (!hasPendingLocalWorkspaceChanges()) {
          setSyncState('Synced', 'synced');
        }
      }
    } catch {
      // Local work remains available while the workspace/event service is absent.
    } finally {
      clearTimeout(timeout);
      if (eventPollController === controller) eventPollController = null;
      if (generation === workspaceGeneration) eventPollTimer = setTimeout(poll, workspaceOnline ? 500 : 4000);
    }
  };
  poll();
}

function normalizeObjects(objects = []) {
  const previous = new Map(sceneObjects.map((object) => [object.id, object]));
  const normalized = objects.map((object, index) => {
    const id = String(object.id ?? object.name ?? `object-${index + 1}`);
    return {
      ...object,
      id,
      name: object.name || `Object ${index + 1}`,
      kind: object.kind || object.type || 'part',
      visible: previous.get(id)?.visible ?? object.visible !== false,
    };
  });
  return normalized.length ? normalized : [{ id: 'model', name: 'Model', kind: 'compiled mesh', placeholder: true, visible: previous.get('model')?.visible !== false }];
}

function setSceneObjects(objects) {
  sceneObjects = normalizeObjects(objects);
  if (!sceneObjects.some((object) => object.id === selectedObjectId)) selectedObjectId = sceneObjects[0]?.id || null;
  renderObjectList();
  meshViewport.draw();
}

function renderObjectList() {
  $('#objectCount').textContent = String(sceneObjects.length);
  const list = $('#objectList');
  if (!sceneObjects.length) {
    list.innerHTML = '<div class="objects-empty"><i></i><span>Objects appear after preview.</span></div>';
    return;
  }
  list.innerHTML = sceneObjects.map((object) => `<div class="object-row${object.id === selectedObjectId ? ' selected' : ''}${object.visible === false ? ' hidden-object' : ''}${intersectionIds.has(object.id) ? ' intersecting' : ''}" role="option" aria-selected="${object.id === selectedObjectId}" data-object-id="${escapeAttribute(object.id)}"><button class="visibility-toggle" aria-label="${object.visible === false ? 'Show' : 'Hide'} ${escapeAttribute(object.name)}" title="${object.visible === false ? 'Show' : 'Hide'} object" aria-pressed="${object.visible === false}">${object.visible === false ? icons.eyeOff : icons.eye}</button><span class="object-copy"><b>${escapeHtml(object.name)}</b><small>${escapeHtml(object.kind)}</small></span><i class="object-swatch"></i></div>`).join('');
}

function selectObject(id) {
  selectedObjectId = id;
  renderObjectList();
  meshViewport.draw();
  const object = sceneObjects.find((item) => item.id === id);
  const range = object?.sourceRange;
  if (range && Number.isFinite(range.start ?? range.from)) {
    editor.focus();
    editor.setSelectionRange(range.start ?? range.from, range.end ?? range.to ?? range.start ?? range.from);
    updateCursor();
  }
}

async function checkEngine() {
  try {
    const response = await fetch('/api/health');
    const data = await response.json();
    const pill = $('#enginePill');
    // A healthy compiler is the expected case, so it says nothing: the pill
    // appears only when rendering is actually unavailable and the user needs
    // to know why.
    pill.hidden = data.ok;
    pill.classList.toggle('offline', !data.ok);
    pill.querySelector('b').textContent = data.ok ? '' : 'Compiler offline';
  } catch {
    const pill = $('#enginePill');
    pill.hidden = false;
    pill.classList.add('offline');
    pill.querySelector('b').textContent = 'Backend offline';
  }
}

// Queue awareness for the render overlay.
//
// This is a shared server, so a render can spend real time waiting for a free
// compute slot before it starts. That wait is invisible in the render response,
// which arrives once, at the end — so without a second channel a queued render
// looks exactly like a stuck one, which is the "it hung" report we already got
// once. `GET /api/queue` answers the one question the render cannot: are we
// running, or still in line, and how far back?
//
// Saying "you are third in line" is also the honest thing: the wait is not the
// user's model being slow, and it costs their render nothing — the server's
// deadline only starts once the job is actually admitted.
const DEFAULT_RENDER_NOTE = $('#renderNote').textContent;

// The probe belongs to its request, not to the module: a superseded render
// tearing down "the" watcher would blind the render that replaced it.
function stopQueueWatch(request) {
  if (!request) return;
  clearInterval(request.queueWatch);
  request.queueWatch = null;
}

function startQueueWatch(request, mode) {
  const label = $('#renderLabel');
  const note = $('#renderNote');
  const compiling = mode === 'render' ? 'Rendering final geometry' : 'Compiling preview';
  const poll = async () => {
    if (activeRender !== request) { stopQueueWatch(request); return; }
    let status;
    try {
      const response = await fetch(`/api/queue?requestId=${encodeURIComponent(request.id)}`, { signal: request.controller.signal });
      status = await response.json();
    } catch {
      // The queue probe is decoration; a failed probe must never disturb the
      // render it is describing.
      return;
    }
    if (activeRender !== request || request.retrying) { return; }
    if (status && status.state === 'queued') {
      request.queued = true;
      label.textContent = `Waiting for a free slot — position ${status.position} of ${status.waiting}`;
      note.textContent = `${status.running} render${status.running === 1 ? '' : 's'} ahead of you on this server. Yours has not started yet, so the wait does not count against its time limit.`;
      note.hidden = false;
    } else if (request.queued) {
      // Admitted: hand the overlay back to the compile copy and the clock.
      request.queued = false;
      note.textContent = DEFAULT_RENDER_NOTE;
      note.hidden = true;
      label.textContent = compiling;
    }
  };
  poll();
  request.queueWatch = setInterval(poll, QUEUE_POLL_MS);
}

// Elapsed-time readout for the render overlay.
//
// The compile is a single opaque round trip: the engine publishes no progress and the
// response arrives all at once, so there is no honest percentage to show. Inventing one
// would be worse than nothing — a bar that creeps to 90% and sits there is exactly how a
// long wait comes to feel broken. A real clock cannot be wrong: it counts what actually
// elapsed, and its moving tenths digit is the signal that the tab is alive.
let renderElapsedTimer = null;

function stopRenderElapsed() {
  clearInterval(renderElapsedTimer);
  renderElapsedTimer = null;
}

function formatElapsed(seconds) {
  // Tenths while the number is small enough to read them; whole seconds past a minute,
  // where a flickering decimal is just noise.
  return seconds < 60 ? `${seconds.toFixed(1)} s` : `${Math.floor(seconds / 60)} m ${String(Math.floor(seconds % 60)).padStart(2, '0')} s`;
}

function startRenderElapsed(request, mode) {
  stopRenderElapsed();
  const started = performance.now();
  const elapsed = $('#renderElapsed');
  const note = $('#renderNote');
  note.hidden = true;
  const tick = () => {
    // A superseded render must not keep writing over the current one's clock.
    if (activeRender !== request) { stopRenderElapsed(); return; }
    const milliseconds = performance.now() - started;
    request.elapsedMs = milliseconds;
    elapsed.textContent = `${formatElapsed(milliseconds / 1000)} elapsed`;
    if (milliseconds >= SLOW_RENDER_NOTICE_MS) note.hidden = false;
  };
  tick();
  renderElapsedTimer = setInterval(tick, 100);
}

// Waits, but gives up the moment the render it belongs to is abandoned.
function delay(milliseconds, signal) {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, milliseconds);
    signal?.addEventListener('abort', () => { clearTimeout(timer); resolve(); }, { once: true });
  });
}

// Arms this attempt's backstop. Re-armed per attempt rather than run once for
// the whole call: each attempt is a fresh request with its own server-side
// budget, and a shared timer would count a retry's wait against the render
// that eventually ran — the false "render timed out" this backstop exists to
// prevent.
function armRenderBackstop(request, mode) {
  clearTimeout(request.timeout);
  request.timeout = setTimeout(() => {
    request.timedOut = true;
    request.controller.abort();
  }, RENDER_TIMEOUT_MS[mode] ?? RENDER_TIMEOUT_MS.preview);
}

// Posts one render, waiting out a "server busy" rejection instead of reporting
// it as a failure. Being turned away because every slot and every queue place
// is taken is not something the user can fix by reading about it, and it is
// not a reason to lose their click. It is bounded — QUEUE_RETRY_LIMIT attempts
// on the server's own Retry-After — and every wait is announced rather than
// hidden behind a spinner that looks like progress.
// A render response is not always JSON. A proxy in front of the backend, or a
// backend that is restarting, answers `503 Service Unavailable` as plain text —
// and parsing that blindly reported "Unexpected token 'S'", a message about our
// parser rather than about the server that went away. Read the body once, parse
// it if it is JSON, and otherwise synthesise the failure the caller expects.
async function readRenderResponse(response) {
  const text = await response.text();
  try {
    const parsed = JSON.parse(text);
    if (parsed && typeof parsed === 'object') return parsed;
  } catch {
    // Falls through to the synthesised failure below.
  }
  const status = `${response.status}${response.statusText ? ` ${response.statusText}` : ''}`.trim();
  let snippet = text.replace(/<[^>]*>/g, ' ').replace(/\s+/g, ' ').trim().slice(0, 160);
  // A bare proxy page usually just repeats its own status line; printing it twice
  // reads like two separate failures.
  if (snippet && status.toLowerCase().includes(snippet.toLowerCase())) snippet = '';
  return {
    ok: false,
    error: response.ok
      ? `The server answered with something other than a render result${snippet ? `: ${snippet}` : '.'}`
      : `The render server responded ${status}${snippet ? ` — ${snippet}` : ''}. It may be restarting or overloaded; try again in a moment.`,
  };
}

// Where a failed render lands.
//
// The console line is for the user; this panel is for the project. A render that
// fails on someone else's model is unreproducible unless the source travels with
// the report, so the button hands GitHub a prefilled issue carrying the exact
// code that broke. What it never carries is the workspace URL: that link is the
// only credential this app has, and a public issue is the last place for it.
const ISSUE_NEW_URL = 'https://github.com/Matvey-Kuk/reopenscad/issues/new';
// GitHub and browsers both cap a prefilled URL well before the model does, so an
// oversized source is trimmed to fit and the untrimmed report stays one click
// away on the clipboard.
const ISSUE_URL_LIMIT = 7000;
let lastRenderFailure = null;

function issueReportBody(failure, code, truncated) {
  return [
    'Rendering failed in the ReOpenSCAD web app.',
    '',
    `- **Error:** ${failure.summary}`,
    `- **Mode:** ${failure.mode}`,
    `- **When:** ${failure.at}`,
    `- **Browser:** ${navigator.userAgent}`,
    '',
    '### OpenSCAD source',
    truncated ? '_Trimmed to fit the issue link — the full source is on the reporter\'s clipboard._' : null,
    '```openscad',
    code,
    '```',
  ].filter((line) => line !== null).join('\n');
}

// The first sentence is the diagnosis; anything after it is advice to the user,
// which belongs in the body rather than in a title cut off mid-word.
function issueReportTitle(summary) {
  const sentence = summary.split('. ')[0].trim() || summary;
  const short = sentence.length > 80 ? `${sentence.slice(0, 79).trimEnd()}…` : sentence;
  return `fix: render failed — ${short}`;
}

function issueReportUrl(failure) {
  const title = issueReportTitle(failure.summary);
  let code = failure.code;
  let truncated = false;
  for (;;) {
    const url = `${ISSUE_NEW_URL}?title=${encodeURIComponent(title)}&body=${encodeURIComponent(issueReportBody(failure, code, truncated))}`;
    if (url.length <= ISSUE_URL_LIMIT || code.length <= 200) return { url, truncated };
    code = code.slice(0, Math.floor(code.length * 0.7)).trimEnd();
    truncated = true;
  }
}

function showRenderFailure(summary, mode) {
  lastRenderFailure = {
    summary: String(summary || 'Render failed').trim() || 'Render failed',
    mode,
    code: editor.value,
    at: new Date().toISOString(),
  };
  $('#renderFailureMessage').textContent = lastRenderFailure.summary;
  $('#renderFailure').hidden = false;
}

function hideRenderFailure() {
  $('#renderFailure').hidden = true;
}

async function sendRender(url, body, request, mode) {
  for (let attempt = 0; ; attempt += 1) {
    armRenderBackstop(request, mode);
    const response = await fetch(url, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      signal: request.controller.signal,
      body: JSON.stringify(body),
    });
    const data = await readRenderResponse(response);
    const busy = response.status === 429 && data && data.retryable;
    if (!busy || attempt >= QUEUE_RETRY_LIMIT) return { status: response.status, data };
    const wait = Math.min(Number(response.headers.get('Retry-After')) || data.retryAfterSeconds || 5, 30);
    request.retrying = true;
    setStatus('Server busy', 'busy');
    $('#renderLabel').textContent = `Server busy — retrying in ${wait} s`;
    $('#renderNote').textContent = data.error;
    $('#renderNote').hidden = false;
    log(`Server at capacity. Retrying in ${wait} s (attempt ${attempt + 2} of ${QUEUE_RETRY_LIMIT + 1}).`, 'warning');
    await delay(wait * 1000, request.controller.signal);
    request.retrying = false;
    $('#renderNote').textContent = DEFAULT_RENDER_NOTE;
    $('#renderNote').hidden = true;
    if (request.controller.signal.aborted || activeRender !== request) return { status: response.status, data };
    // Hand the overlay back at once rather than leaving "retrying in 15 s" on
    // screen until the next queue poll contradicts it.
    $('#renderLabel').textContent = mode === 'render' ? 'Rendering final geometry' : 'Compiling preview';
    setStatus(mode === 'render' ? 'Rendering' : 'Previewing', 'busy');
  }
}

async function render(mode = 'preview') {
  // An empty workspace has nothing to compile; the kick-start options are the call to action.
  if (!editor.value.trim()) {
    log('Nothing to render yet — pick a kick-start model or write some OpenSCAD.', 'warning');
    setStatus('Ready');
    return false;
  }
  if (activeRender) cancelRender(activeRender);
  const request = {
    controller: new AbortController(),
    id: `render-${Date.now()}-${++renderSequence}`,
    generation: workspaceGeneration,
    workspaceId,
  };
  activeRender = request;
  armRenderBackstop(request, mode);
  hideRenderFailure();
  renderOverlay.hidden = false;
  $('#renderLabel').textContent = mode === 'render' ? 'Rendering final geometry' : 'Compiling preview';
  startRenderElapsed(request, mode);
  startQueueWatch(request, mode);
  $('#previewButton').disabled = true;
  $('#renderButton').disabled = true;
  $('#stopButton').disabled = false;
  setStatus(mode === 'render' ? 'Rendering' : 'Previewing', 'busy');
  log(`${mode === 'render' ? 'F6' : 'F5'} — ${mode} started`, 'info');

  try {
    const workspaceReady = await flushWorkspaceSave();
    if (request.controller.signal.aborted || activeRender !== request || request.generation !== workspaceGeneration || request.workspaceId !== workspaceId) return false;
    const source = editor.value;
    const useWorkspace = workspaceReady && workspaceId;
    const renderUrl = useWorkspace ? `/api/workspaces/${encodeURIComponent(workspaceId)}/render` : '/api/render';
    const requestBody = {
      code: source,
      mode,
      requestId: request.id,
      revision: workspaceRevision,
      projection,
      camera,
      view: [$('#edgesToggle').checked && 'edges', $('#axesToggle').checked && 'axes'].filter(Boolean),
      workspaceId,
      hiddenObjectIds: sceneObjects.filter((object) => object.visible === false).map((object) => object.id),
      selectedObjectId,
    };
    let attempt = await sendRender(renderUrl, requestBody, request, mode);
    if (useWorkspace && attempt.status === 409) {
      attempt = await sendRender('/api/render', requestBody, request, mode);
    }
    const { status, data } = attempt;
    if (request.controller.signal.aborted || activeRender !== request || request.generation !== workspaceGeneration || request.workspaceId !== workspaceId) return false;
    if (status < 200 || status >= 300 || !data.ok) throw Object.assign(new Error(data.error || 'Render failed'), { console: data.console });
    setSceneObjects(Array.isArray(data.objects) ? data.objects : sceneObjects);
    meshViewport.setMesh(data.mesh, sceneObjects);
    // New bounds, so the fit verdict on the badge is now answerable (or has changed).
    renderPlateBadge();
    hasMesh = true;
    emptyState.hidden = true;
    log(data.console, /WARNING|DEPRECATED/.test(data.console) ? 'warning' : 'success');
    // `elapsedMs` is compute only; the server reports the queue wait apart from
    // it so a busy server is never mistaken for a slow model.
    const queued = data.queuedMs > 500 ? ` after ${formatElapsed(data.queuedMs / 1000)} queued` : '';
    log(`${mode} finished in ${(data.elapsedMs / 1000).toFixed(2)} s · ${data.triangles.toLocaleString()} triangles${queued}`, 'success');
    renderTime.textContent = `${mode} · ${(data.elapsedMs / 1000).toFixed(2)} s`;
    $('#problemCount').textContent = '0';
    setStatus('Ready');
    // After the frame is on screen and the status has settled, so the capture
    // never lands in front of the user as a pause. Fire-and-forget: a
    // thumbnail is decoration and must not hold up the render path.
    requestAnimationFrame(() => {
      uploadWorkspacePreview();
    });
    return true;
  } catch (error) {
    if (error.name === 'AbortError' && request.timedOut) {
      log(`Render timed out after ${(RENDER_TIMEOUT_MS[mode] ?? RENDER_TIMEOUT_MS.preview) / 1000} s with no response from the server. Simplify the model, reduce $fn, or render fewer objects at once.`, 'error');
      $('#problemCount').textContent = '1';
      if (activeRender === request) {
        setStatus('Render timed out', 'error');
        showRenderFailure(`Timed out after ${(RENDER_TIMEOUT_MS[mode] ?? RENDER_TIMEOUT_MS.preview) / 1000} s with no response from the server.`, mode);
      }
    } else if (error.name === 'AbortError') {
      // Naming the time it saved is the point of the button: it turns "I gave up" into a
      // measurable win and makes cancelling feel like a tool rather than a retreat.
      log(`Render cancelled after ${formatElapsed((request.elapsedMs || 0) / 1000)}.`, 'warning');
      if (activeRender === request) setStatus('Cancelled');
    } else {
      log(error.message, 'error');
      if (error.console) log(error.console, 'error');
      $('#problemCount').textContent = '1';
      if (activeRender === request) {
        setStatus('Render failed', 'error');
        showRenderFailure(error.message, mode);
      }
    }
    return false;
  } finally {
    // Always retire this request's watchdog, including when a newer render has
    // already taken over as `activeRender`.
    clearTimeout(request.timeout);
    stopQueueWatch(request);
    if (activeRender === request) {
      stopRenderElapsed();
      renderOverlay.hidden = true;
      $('#previewButton').disabled = false;
      $('#renderButton').disabled = false;
      $('#stopButton').disabled = true;
      activeRender = null;
    }
  }
}

function cancelRender(request = activeRender) {
  if (!request) return;
  request.controller.abort();
  fetch('/api/cancel', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ requestId: request.id }),
  }).catch(() => {});
}

function downloadBlob(blob, name) {
  const link = document.createElement('a');
  link.href = URL.createObjectURL(blob);
  link.download = name;
  link.click();
  setTimeout(() => URL.revokeObjectURL(link.href), 1000);
}

function saveScad() {
  const name = downloadFileName('scad');
  downloadBlob(new Blob([editor.value], { type: 'text/plain' }), name);
  dirtyIndicator.classList.remove('visible');
  log(`${name} saved to Downloads.`, 'success');
}

// The engine only accepts object IDs that came out of a completed render. Until one lands the
// object list holds a placeholder entry, so an unfiltered export is the only honest request.
function exportObjectIds(scope) {
  if (sceneObjects.some((object) => object.placeholder)) return null;
  if (scope === 'selected') return selectedObjectId ? [selectedObjectId] : null;
  const visible = sceneObjects.filter((object) => object.visible !== false);
  return visible.length === sceneObjects.length ? null : visible.map((object) => object.id);
}

async function exportModel(format, scope = 'visible') {
  const generation = workspaceGeneration;
  const id = workspaceId;
  if (scope === 'selected' && !exportObjectIds(scope) && !(await render('render'))) return;
  if (generation !== workspaceGeneration || id !== workspaceId) return;
  setStatus(`Exporting ${format.toUpperCase()}`, 'busy');
  log(`Exporting final geometry as ${format.toUpperCase()}…`);
  try {
    const workspaceReady = await flushWorkspaceSave();
    if (generation !== workspaceGeneration || id !== workspaceId) return;
    const source = editor.value;
    const useWorkspace = workspaceReady && workspaceId;
    const exportUrl = useWorkspace ? `/api/workspaces/${encodeURIComponent(workspaceId)}/export` : '/api/export';
    const objectIds = exportObjectIds(scope);
    const requestBody = {
      code: source,
      format,
      revision: workspaceRevision,
      ...(objectIds ? { objectIds } : {}),
    };
    let response = await fetch(exportUrl, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(requestBody),
    });
    if (useWorkspace && response.status === 409) {
      response = await fetch('/api/export', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(requestBody),
      });
    }
    if (generation !== workspaceGeneration || id !== workspaceId) return;
    if (!response.ok) {
      const data = await response.json();
      throw Object.assign(new Error(data.error), { console: data.console });
    }
    const suffix = scope === 'selected' ? '-selected' : '';
    downloadBlob(await response.blob(), `${downloadBaseName}${suffix}.${format}`);
    log(`${format.toUpperCase()} ${scope === 'selected' ? 'selected-object ' : ''}export complete.`, 'success');
    setStatus('Ready');
  } catch (error) {
    log(error.console || error.message, 'error');
    setStatus('Export failed', 'error');
  }
}

// Set once the user opens the panel by hand, so an auto-hide never yanks it
// back shut underneath them. Cleared when they close it again.
let customizerPinned = false;

function updateCustomizer() {
  const fields = [];
  const pattern = /^\s*([A-Za-z_$][\w$]*)\s*=\s*(-?\d+(?:\.\d+)?)\s*;\s*\/\/\s*\[\s*(-?\d+(?:\.\d+)?)\s*:\s*(-?\d+(?:\.\d+)?)\s*:\s*(-?\d+(?:\.\d+)?)\s*\]/gm;
  for (const match of editor.value.matchAll(pattern)) {
    fields.push({ name: match[1], value: Number(match[2]), min: Number(match[3]), step: Number(match[4]), max: Number(match[5]) });
  }
  const body = $('#customizerBody');
  // Nothing in the source is customizable, so the panel and its toggle have
  // nothing to show or do — hide both rather than parking an empty column.
  // The panel hides itself when the model has nothing to customize, but the
  // toggle always stays available: opening it deliberately must still work,
  // and the empty state is where the feature is explained.
  const hasFields = fields.length > 0;
  $('#customizer').hidden = !hasFields && !customizerPinned;
  if (!hasFields) {
    body.innerHTML = '<div class="customizer-empty"><span>{ }</span><p>Add a range comment like<br><code>// [1:1:20]</code> after a variable.</p></div>';
    if (!customizerPinned) $('#customizer').classList.add('closed');
    return;
  }
  $('#customizer').classList.remove('closed');
  body.innerHTML = fields.map((field) => `<div class="custom-field" data-variable="${field.name}"><label><span>${field.name}</span><input type="number" min="${field.min}" max="${field.max}" step="${field.step}" value="${field.value}"></label><input type="range" min="${field.min}" max="${field.max}" step="${field.step}" value="${field.value}"></div>`).join('');
  $$('.custom-field', body).forEach((container) => {
    const range = $('input[type="range"]', container);
    const number = $('input[type="number"]', container);
    const commit = (value) => {
      range.value = value;
      number.value = value;
      const variable = container.dataset.variable.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
      editor.value = editor.value.replace(new RegExp(`(^\\s*${variable}\\s*=\\s*)-?\\d+(?:\\.\\d+)?`, 'm'), `$1${value}`);
      syncEditor();
      clearTimeout(renderDebounce);
      renderDebounce = setTimeout(() => render('preview'), 350);
    };
    range.addEventListener('input', () => { number.value = range.value; });
    range.addEventListener('change', () => commit(range.value));
    number.addEventListener('change', () => commit(number.value));
  });
}

async function loadExample(slug) {
  const card = $(`[data-example="${CSS.escape(slug)}"]`);
  card?.setAttribute('aria-busy', 'true');
  try {
    const response = await fetch(`${EXAMPLES_BASE}${encodeURIComponent(slug)}.scad`);
    if (!response.ok) throw new Error(`example ${response.status}`);
    editor.value = await response.text();
  } catch {
    // The console pane is where every other failure is reported, so this one
    // goes there too rather than into an alert the user has to dismiss.
    log(`Could not open "${slug}". It may not be deployed with this build.`, 'error');
    return;
  } finally {
    card?.removeAttribute('aria-busy');
  }
  syncEditor();
  editor.focus();
  editor.setSelectionRange(0, 0);
  editor.scrollTop = 0;
  render('preview');
}

// The catalogue lives above the edit field and only while it is empty — no
// toolbar dropdown. Grouped by what the model *is*, because eighteen cards in
// one undifferentiated grid is a wall rather than a menu.
// The recents group is rebuilt on its own, because it changes — a visit, a
// removal — while the catalogue below it never does.
function renderRecents() {
  const host = $('#kickstartRecents');
  const entries = readRecents();
  // "New workspace" is always the first card, whether or not there is any
  // history behind it — it is the one action an empty workspace always offers,
  // and putting it in the grid rather than above it means there is one place
  // to look instead of two.
  const blank = '<button type="button" class="kickstart-card kickstart-new" role="listitem" '
    + 'data-blank-editor aria-label="Blank workspace">'
    + '<span class="kickstart-shot"><span class="kickstart-plus" aria-hidden="true">+</span></span>'
    + '<b>Blank workspace</b><small>Start typing in an empty editor.</small></button>';
  const cards = entries
    .map((entry) => {
      const id = escapeAttribute(entry.id);
      // The ids are `adjective-animal-<hex>`. The two words are the half a
      // person actually recognises — "the zippy zebra one" — so they are the
      // card's title and the full id is the line underneath. No document name
      // is involved: the workspace is the document.
      const label = entry.id.split('-').slice(0, 2).join(' ') || entry.id;
      return `<div class="kickstart-card kickstart-recent" role="listitem">`
        + `<button type="button" class="kickstart-open" data-workspace="${id}">`
        + `<span class="kickstart-shot" data-preview-for="${id}"></span>`
        + `<b>${escapeHtml(label)}</b><small>${escapeHtml(entry.id)}</small></button>`
        + `<button type="button" class="kickstart-forget" data-forget="${id}" `
        + `aria-label="Remove ${escapeAttribute(label)} from the list" title="Remove from this list">&times;</button>`
        + `</div>`;
    })
    .join('');
  const heading = entries.length
    ? '<h3>Your workspaces</h3><p>Kept in this browser only. Removing one here does not delete it.</p>'
    : '<h3>Your workspaces</h3><p>Nothing here yet.</p>';
  host.innerHTML = `<section class="kickstart-group">${heading}`
    + `<div class="kickstart-grid" role="list">${blank}${cards}</div></section>`;
  // The preview route answers JSON rather than image bytes, so the thumbnails
  // are fetched and set as `src` afterwards. A workspace that has never been
  // rendered simply has none, and its box stays the empty placeholder.
  for (const slot of $$('[data-preview-for]', host)) {
    const id = slot.dataset.previewFor;
    fetch(`/api/workspaces/${encodeURIComponent(id)}/preview`)
      .then((response) => (response.ok ? response.json() : null))
      .then((payload) => {
        if (!payload?.preview || !slot.isConnected) return;
        const image = document.createElement('img');
        image.alt = '';
        image.decoding = 'async';
        image.src = payload.preview;
        slot.append(image);
      })
      .catch(() => {});
  }
}

async function renderKickstart() {
  renderRecents();
  const grid = $('#kickstartGrid');
  if (grid.dataset.rendered === '1') return;
  const manifest = await loadCatalogue();
  if (!manifest) {
    grid.innerHTML = '<p class="kickstart-empty">The example catalogue is not available in this build.</p>';
    grid.dataset.rendered = '1';
    return;
  }
  grid.innerHTML = manifest.categories
    .map((category) => {
      const cards = category.examples
        .map((example) => {
          const slug = escapeAttribute(example.slug);
          return `<button type="button" class="kickstart-card" role="listitem" data-example="${slug}">`
            + `<span class="kickstart-shot"><img src="${EXAMPLES_BASE}${slug}.png" alt="" loading="lazy" decoding="async" onerror="this.dataset.failed='1'" /></span>`
            + `<b>${escapeHtml(example.name)}</b>`
            + `<small>${escapeHtml(example.blurb)}</small>`
            + `<code>${escapeHtml(example.hint)}</code></button>`;
        })
        .join('');
      return `<section class="kickstart-group"><h3>${escapeHtml(category.name)}</h3>`
        + `<p>${escapeHtml(category.blurb)}</p>`
        + `<div class="kickstart-grid" role="list">${cards}</div></section>`;
    })
    .join('');
  grid.dataset.rendered = '1';
}

// Set by "Blank workspace", cleared whenever the workspace changes. In memory
// rather than in storage on purpose: an empty workspace *should* offer help,
// so coming back to one later starts the catalogue over. Dismissing it is a
// statement about this sitting, not about the workspace.
let kickstartDismissed = false;

function updateKickstart() {
  const empty = editor.value.trim().length === 0;
  // A read-only view never offers the catalogue. Every card in it either edits
  // this editor or makes a workspace, and neither is a thing a reader came
  // here to do — a shared blank model should just show the blank editor.
  const hidden = !empty || kickstartDismissed || Boolean(readOnlyToken);
  $('#kickstart').hidden = hidden;
  // Built the first time it is actually shown, so a workspace that opens with
  // code in it never pays for the manifest or the twelve preview requests.
  if (!hidden) renderKickstart();
  // An empty editor has nothing to preview, so the offer to preview it is a
  // button that can only produce an error. The heading beside it still reads
  // as an invitation, which is the right thing for an empty workspace to say.
  $('#firstPreview').hidden = empty;
  $('#emptyHint').hidden = empty;
}

editor.addEventListener('input', () => {
  if (aiUpdatedRanges.length) {
    aiUpdatedRanges = [];
    $('#aiRevisionBadge').hidden = true;
  }
  syncEditor();
});
editor.addEventListener('scroll', syncScroll);
editor.addEventListener('click', updateCursor);
editor.addEventListener('keyup', updateCursor);
editor.addEventListener('keydown', (event) => {
  if (event.key === 'Tab') {
    event.preventDefault();
    const start = editor.selectionStart;
    editor.setRangeText('  ', start, editor.selectionEnd, 'end');
    syncEditor();
  }
});

$('#previewButton').addEventListener('click', () => render('preview'));
$('#firstPreview').addEventListener('click', () => render('preview'));
$('#renderButton').addEventListener('click', () => render('render'));
$('#stopButton').addEventListener('click', () => cancelRender());
// The same abort as the toolbar's stop icon, but where the user is actually looking during
// a long render: on the overlay that is covering the viewport.
$('#overlayCancel').addEventListener('click', () => cancelRender());
$('#dismissRenderFailure').addEventListener('click', hideRenderFailure);
$('#reportRenderFailure').addEventListener('click', async () => {
  if (!lastRenderFailure) return;
  const { url, truncated } = issueReportUrl(lastRenderFailure);
  window.open(url, '_blank', 'noopener,noreferrer');
  // The trimmed body promises the full source is on the clipboard, so put it
  // there rather than leaving the reporter to notice the second button.
  if (!truncated) return;
  try {
    await navigator.clipboard.writeText(issueReportBody(lastRenderFailure, lastRenderFailure.code, false));
    log('Source too long for an issue link — the full report was copied to your clipboard. Paste it over the trimmed source in the issue.', 'warning');
  } catch {
    log('Source too long for an issue link, so the issue carries a trimmed copy. Use "Copy report" to get the whole thing.', 'warning');
  }
});
$('#copyRenderFailure').addEventListener('click', async (event) => {
  if (!lastRenderFailure) return;
  const button = event.currentTarget;
  const label = $('span', button);
  try {
    await navigator.clipboard.writeText(issueReportBody(lastRenderFailure, lastRenderFailure.code, false));
    label.textContent = 'Copied';
    button.classList.add('copied');
    clearTimeout(Number(button.dataset.copyTimer));
    button.dataset.copyTimer = String(setTimeout(() => {
      label.textContent = 'Copy report';
      button.classList.remove('copied');
    }, 1200));
  } catch {
    log('Could not copy the report to the clipboard.', 'warning');
  }
});
$('#saveButton').addEventListener('click', saveScad);
$('#openButton').addEventListener('click', () => $('#fileInput').click());
$('#newButton').addEventListener('click', async () => {
  // Push, so the workspace the user came from stays reachable with the Back button.
  await createWorkspace('', { navigation: 'push' });
  aiUpdatedRanges = [];
  $('#aiRevisionBadge').hidden = true;
  sceneObjects = [];
  intersectionIds.clear();
  renderObjectList();
  hasMesh = false;
  meshViewport.clearMesh();
  emptyState.hidden = false;
});
$('#fileInput').addEventListener('change', async (event) => {
  const file = event.target.files[0];
  if (!file) return;
  editor.value = await file.text();
  setDownloadBaseName(file.name);
  syncEditor(false);
  scheduleWorkspaceSave();
  dirtyIndicator.classList.remove('visible');
  log(`${file.name} opened.`, 'success');
  render('preview');
});
// Delegated from the panel, not from the catalogue: the recents list is a
// sibling of it, and binding to the catalogue alone left every card in the
// recents group inert.
$('#kickstart').addEventListener('click', (event) => {
  const remove = event.target.closest('[data-forget]');
  if (remove) {
    forgetRecentWorkspace(remove.dataset.forget, remove);
    return;
  }
  if (event.target.closest('[data-blank-editor]')) {
    // Not a new workspace: the catalogue is only ever on screen when this one
    // is already empty, so minting a second empty workspace would change the
    // URL and leave the screen looking identical — which is exactly what it
    // used to do, and why clicking this appeared to do nothing at all.
    kickstartDismissed = true;
    updateKickstart();
    editor.focus();
    return;
  }
  const recent = event.target.closest('[data-workspace]');
  if (recent) {
    location.href = `/workspaces/${encodeURIComponent(recent.dataset.workspace)}`;
    return;
  }
  const card = event.target.closest('[data-example]');
  if (card) loadExample(card.dataset.example);
});
// The panel takes pointer events so that it can scroll — eighteen cards did not
// fit, and a wheel over a `pointer-events: none` layer goes to the editor
// underneath, which does not scroll either. Taking them back means a click on
// the background no longer reaches the editor, so it is forwarded by hand: the
// panel covers the edit field, and clicking it to start typing is the obvious
// thing to try.
$('#kickstart').addEventListener('mousedown', (event) => {
  if (event.target.closest('button, a, [data-example], [data-workspace], [data-blank-editor]')) return;
  event.preventDefault();
  editor.focus();
});

$$('[data-projection]').forEach((button) => button.addEventListener('click', () => {
  projection = button.dataset.projection;
  $$('[data-projection]').forEach((item) => item.classList.toggle('active', item === button));
  meshViewport.draw();
}));

const viewCameras = {
  reset: [0, 0, 0, 58, 0, 28, 140],
  top: [0, 0, 0, 89.9, 0, 0, 140],
  front: [0, 0, 0, 0, 0, -90, 140],
  right: [0, 0, 0, 0, 0, 0, 140],
};
// A named view is a promise about framing, so it restores the standard one whole: the
// preset literals carry a zero pan offset, which puts the model back in the middle.
$$('[data-view]').forEach((button) => button.addEventListener('click', () => {
  const distance = camera[6];
  camera = [...viewCameras[button.dataset.view]];
  camera[6] = distance;
  meshViewport.draw();
}));
// Fit means "show me the model", which it cannot honour while the view is panned off it.
$('#fitView').addEventListener('click', () => {
  camera[0] = camera[1] = camera[2] = 0;
  camera[6] = Math.max(meshViewport.radius * 4, 20);
  meshViewport.draw();
});
$('#zoomIn').addEventListener('click', () => { camera[6] = Math.max(meshViewport.radius * 1.15, camera[6] * .82); meshViewport.draw(); });
$('#zoomOut').addEventListener('click', () => { camera[6] = Math.min(meshViewport.radius * 30, camera[6] * 1.2); meshViewport.draw(); });
$('#edgesToggle').addEventListener('change', (event) => { meshViewport.showEdges = event.target.checked; meshViewport.draw(); });
$('#axesToggle').addEventListener('change', (event) => { meshViewport.showAxes = event.target.checked; meshViewport.draw(); });

const GROUND_STORAGE_KEY = 'reopenscad.ground';

// Like the tolerance, the ground preference is a viewer setting rather than model data:
// localStorage carries it across reloads and into every new workspace.
function restoreGroundPreference() {
  const stored = localStorage.getItem(GROUND_STORAGE_KEY);
  const enabled = stored === null ? true : stored === 'true';
  $('#groundToggle').checked = enabled;
  meshViewport.showGround = enabled;
  meshViewport.draw();
}

$('#groundToggle').addEventListener('change', (event) => {
  meshViewport.showGround = event.target.checked;
  localStorage.setItem(GROUND_STORAGE_KEY, String(event.target.checked));
  meshViewport.draw();
});

const PLATE_STORAGE_KEY = 'reopenscad.plate';

// The plate is optional, so "None" is the default and the first option.
function populatePlateSelect() {
  const select = $('#plateSelect');
  const none = document.createElement('option');
  none.value = '';
  none.textContent = 'No build plate';
  select.append(none);
  for (const group of PRINTER_PLATES) {
    const optgroup = document.createElement('optgroup');
    optgroup.label = group.brand;
    for (const model of group.models) {
      const option = document.createElement('option');
      option.value = model.id;
      // The dimensions are the reason to pick one, so they are in the option itself.
      option.textContent = `${model.name} — ${model.size.join(' × ')} mm`;
      optgroup.append(option);
    }
    select.append(optgroup);
  }
}

// Reflects the current bed into the viewport and the badge. `bounds` is only known after a
// render, so the fit line stays absent until there is a model to judge.
function renderPlateBadge() {
  const badge = $('#plateBadge');
  const plate = meshViewport.plate;
  badge.hidden = !plate;
  if (!plate) return;
  $('#plateBadgeName').textContent = `${plate.label} · ${plate.size.join(' × ')} mm`;
  const fit = plateFit(plate, meshViewport.bounds);
  if (!fit) {
    badge.removeAttribute('data-fit');
    $('#plateBadgeFit').textContent = 'Build plate';
    return;
  }
  badge.dataset.fit = fit.fits ? 'fits' : 'over';
  const size = fit.size.map((value) => formatMillimetres(Number(value.toFixed(1)))).join(' × ');
  $('#plateBadgeFit').textContent = fit.fits ? `Model fits · ${size} mm` : `Too large in ${fit.over.join(', ')} · ${size} mm`;
}

function applyPlate(id, { draw = true } = {}) {
  const plate = findPlate(id);
  meshViewport.plate = plate;
  $('#plateSelect').value = plate ? plate.id : '';
  renderPlateBadge();
  if (draw) meshViewport.draw();
  return plate;
}

// Two persistence layers, and they answer different questions. localStorage is "the bed I
// own", so it carries into every NEW workspace; the workspace field is "the bed this design
// was made for", so it travels with the URL to anyone the link is shared with.
function restorePlatePreference() {
  const stored = localStorage.getItem(PLATE_STORAGE_KEY) || '';
  const plate = applyPlate(stored, { draw: false });
  // A preference naming a printer this build no longer offers would otherwise be forwarded
  // verbatim into every new workspace and stored server-side as an id nothing can resolve.
  if (stored && !plate) localStorage.removeItem(PLATE_STORAGE_KEY);
}

// Selecting a 350 mm bed while framed on a 20 mm part would put the whole plate off-screen,
// so the only visible effect would be the grid getting coarser. Pull back far enough to see
// the bed, but never so close that the model itself stops fitting.
function frameForPlate(plate) {
  const span = Math.max(plate.size[0], plate.size[1], plate.size[2]);
  camera[6] = Math.max(span * 1.9, meshViewport.radius * 4, 20);
}

$('#plateSelect').addEventListener('change', (event) => {
  const plate = applyPlate(event.target.value, { draw: false });
  if (plate) frameForPlate(plate);
  meshViewport.draw();
  localStorage.setItem(PLATE_STORAGE_KEY, plate ? plate.id : '');
  saveWorkspaceSettings();
  log(plate ? `Build plate: ${plate.label} (${plate.size.join(' × ')} mm).` : 'Build plate cleared.', 'info');
});

// Panning translates the camera target along the two screen axes. The screen->world scale
// is taken from the live camera, so the drag is 1:1 in either projection: the world point
// that was under the cursor when the drag began is still under it when it ends.
function panView(from, dxPixels, dyPixels) {
  const [right, up] = meshViewport.screenBasis();
  const scale = meshViewport.worldPerPixel();
  for (let axis = 0; axis < 3; axis += 1) {
    camera[axis] = from[axis] - right[axis] * dxPixels * scale + up[axis] * dyPixels * scale;
  }
  meshViewport.draw();
}

// How far a horizontal swipe turns the model. A trackpad reports a swipe as many
// small deltas, so this is tuned against the drag sensitivity (.45 deg/px) rather
// than against a wheel notch.
const SWIPE_ORBIT_DEGREES_PER_PIXEL = .25;

$('#viewport').addEventListener('wheel', (event) => {
  if (!hasMesh) return;
  event.preventDefault();
  // Firefox reports wheel deltas in lines, and a page-scroll wheel in pages; panView and
  // the orbit below are defined in pixels, so a raw delta would move ~16x too slowly there.
  const unit = event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? renderCanvas.clientHeight : 1;
  // Shift + wheel pans: the trackpad answer to a middle mouse button.
  if (event.shiftKey) {
    panView([camera[0], camera[1], camera[2]], -event.deltaX * unit, -event.deltaY * unit);
    return;
  }
  // A horizontal two-finger swipe orbits, and turns the model with the fingers — the same
  // motion as a sideways drag, so both share one sign. Before any of this, a horizontal
  // swipe reached the zoom line below with deltaY at 0, which reads as "not greater than
  // zero" — so a sideways gesture silently zoomed in instead of turning anything.
  if (Math.abs(event.deltaX) > Math.abs(event.deltaY)) {
    camera[5] += event.deltaX * unit * SWIPE_ORBIT_DEGREES_PER_PIXEL;
    meshViewport.draw();
    return;
  }
  if (!event.deltaY) return;
  camera[6] = Math.max(meshViewport.radius * 1.15, Math.min(meshViewport.radius * 30, camera[6] * (event.deltaY > 0 ? 1.1 : .9)));
  meshViewport.draw();
}, { passive: false });

// Fusion 360's bindings, which is what CAD users already have in their fingers: the middle
// mouse button pans, Shift + middle orbits. A right-button drag also pans, as it does in
// PrusaSlicer, Cura and Bambu Studio. Shift + left drag is deliberately not a pan — in
// Fusion that chord orbits, and a trackpad with no third button pans with Shift + wheel.
// Orbit keeps the plain left drag, so no single press can ever mean both gestures.
function isPanGesture(event) {
  // Alt/Option + left drag pans. Measured against what was already here: every
  // left-drag variant orbited, so the only way to pan was a middle or right
  // button — and a laptop trackpad has neither, which left pan reachable only
  // through Shift + wheel. Alt is the free modifier: Shift is already the
  // orbit chord on the middle button (as it is in Fusion 360), and Cmd/Ctrl +
  // click is a right-click on macOS, which would collide with the right-drag
  // pan below.
  return (event.button === 0 && event.altKey)
    || (event.button === 1 && !event.shiftKey)
    || event.button === 2;
}

renderCanvas.addEventListener('pointerdown', (event) => {
  // A second pointer (or a second button) must not hijack a gesture already in progress:
  // the drag that owns the capture is the one that gets to finish.
  if (orbitStart || panStart) return;
  const pan = isPanGesture(event);
  // Middle-button orbit (Shift + middle) is a gesture too; without this the button
  // would fall through as "not a pan and not the left button" and do nothing.
  if (!pan && event.button !== 0 && event.button !== 1) return;
  // Capture first. If the pointer is already gone this throws, and a gesture started
  // anyway would latch: no capture means no pointerup here, and the guard above would
  // then refuse every later drag for the life of the page.
  try {
    renderCanvas.setPointerCapture(event.pointerId);
  } catch { return; }
  // Stops the middle-click autoscroll widget from stealing the drag on Windows, whichever
  // gesture that button was pressed for.
  if (pan || event.button === 1) event.preventDefault();
  if (pan) {
    panStart = { x: event.clientX, y: event.clientY, offset: [camera[0], camera[1], camera[2]] };
    renderCanvas.classList.add('panning');
  } else {
    orbitStart = { x: event.clientX, y: event.clientY, rx: camera[3], rz: camera[5] };
  }
});
renderCanvas.addEventListener('pointermove', (event) => {
  // A button released where no pointerup reaches us (a native drag, a devtools pause)
  // would otherwise leave the view following a button-less mouse around the canvas.
  if ((panStart || orbitStart) && event.buttons === 0) { endViewportDrag(); return; }
  if (panStart) {
    panView(panStart.offset, event.clientX - panStart.x, event.clientY - panStart.y);
    return;
  }
  if (!orbitStart) return;
  const dx = event.clientX - orbitStart.x;
  const dy = event.clientY - orbitStart.y;
  // Both axes turn the model with the hand, as if the cursor had hold of the surface
  // facing it: drag right and that surface goes right, drag down and it goes down,
  // rolling the top into view. Azimuth needs the minus because it turns the camera
  // around the model rather than the model itself — without it the two axes obeyed
  // opposite conventions and a sideways drag pushed the model away from the cursor.
  camera[3] = Math.max(-89.5, Math.min(89.5, orbitStart.rx + dy * .45));
  camera[5] = orbitStart.rz - dx * .45;
  meshViewport.draw();
});
function endViewportDrag() {
  orbitStart = null;
  panStart = null;
  renderCanvas.classList.remove('panning');
}
renderCanvas.addEventListener('pointerup', endViewportDrag);
renderCanvas.addEventListener('pointercancel', endViewportDrag);
// Capture can be taken away without a pointerup ever arriving; the gesture ends with it.
renderCanvas.addEventListener('lostpointercapture', endViewportDrag);
// A right-button drag is a pan here, as it is in every slicer, so the canvas must not
// raise the browser menu on top of the model mid-gesture.
renderCanvas.addEventListener('contextmenu', (event) => event.preventDefault());

$('#customizerButton').addEventListener('click', () => {
  const panel = $('#customizer');
  const opening = panel.hidden || panel.classList.contains('closed');
  customizerPinned = opening;
  panel.hidden = false;
  panel.classList.toggle('closed', !opening);
});
$('#closeCustomizer').addEventListener('click', () => {
  customizerPinned = false;
  $('#customizer').classList.add('closed');
  updateCustomizer();
});
$('#clearConsole').addEventListener('click', () => { consoleOutput.innerHTML = ''; });
$('#collapseConsole').addEventListener('click', () => $('#consolePane').classList.toggle('collapsed'));

const exportPopover = $('#exportPopover');
$('#exportButton').addEventListener('click', (event) => {
  const rect = event.currentTarget.getBoundingClientRect();
  exportPopover.style.left = `${rect.right - 230}px`;
  exportPopover.style.top = `${rect.bottom + 5}px`;
  exportPopover.hidden = !exportPopover.hidden;
});
$$('[data-format]', exportPopover).forEach((button) => button.addEventListener('click', () => {
  exportPopover.hidden = true;
  exportModel(button.dataset.format, button.dataset.scope || 'visible');
}));
$('#exportPng').addEventListener('click', async () => {
  exportPopover.hidden = true;
  if (!hasMesh && !(await render('render'))) return;
  meshViewport.downloadPng(downloadFileName('png'));
});

document.addEventListener('pointerdown', (event) => {
  if (!event.target.closest('#exportButton') && !event.target.closest('.export-popover')) exportPopover.hidden = true;
});

// --- Sharing ---------------------------------------------------------------
//
// Two links, and the difference between them is the whole feature. The one in
// the address bar is a capability to *edit*: there is no login, so handing it
// to somebody hands them write access. The read-only one is an independent
// random token the server maps to this workspace in one direction only — a
// holder of it cannot derive the editable link, which is what makes it safe to
// paste somewhere the editable link would not be.
const sharePopover = $('#sharePopover');
// Keyed by workspace, because the cache outlives the workspace otherwise: open
// Share, press New, open Share again, and the popover would hand out the
// *previous* workspace's read-only link under the new workspace's name.
let shareTokenCache = { id: null, promise: null };

function workspaceEditUrl() {
  return `${location.origin}/workspaces/${encodeURIComponent(workspaceId || '')}`;
}

async function ensureShareToken() {
  // A workspace the server has not seen yet cannot be shared, and the first
  // thing anyone does is type and immediately hit Share — so land the pending
  // save before asking rather than reporting "unavailable" at them.
  await flushWorkspaceSave();
  if (!workspaceId || !workspaceOnline) return null;
  const id = workspaceId;
  if (shareTokenCache.id === id && shareTokenCache.promise) return shareTokenCache.promise;
  const promise = fetch(`/api/workspaces/${encodeURIComponent(id)}/share`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: '{}',
  })
    .then((response) => (response.ok ? response.json() : null))
    .then((payload) => payload?.shareToken || null)
    .catch(() => null)
    .then((token) => {
      // Never cache a failure. The usual cause is a workspace that was still
      // being created, which fixes itself a second later — remembering "no"
      // would make one unlucky click permanent for the life of the page.
      if (!token && shareTokenCache.id === id) shareTokenCache = { id: null, promise: null };
      return token;
    });
  shareTokenCache = { id, promise };
  return promise;
}

async function openSharePopover(anchor) {
  const rect = anchor.getBoundingClientRect();
  sharePopover.style.left = `${Math.max(12, Math.min(rect.right - 300, innerWidth - 312))}px`;
  sharePopover.style.top = `${rect.bottom + 5}px`;
  sharePopover.hidden = false;
  $('#shareEditLink').value = workspaceEditUrl();
  const view = $('#shareViewLink');
  // Minted on first ask rather than at creation: most workspaces are never
  // shared, and a capability that does not exist cannot leak.
  view.value = '';
  view.placeholder = 'Creating…';
  const token = await ensureShareToken();
  if (sharePopover.hidden) return;
  if (token) {
    view.value = `${location.origin}/shared/${token}`;
  } else {
    view.placeholder = 'Unavailable — this workspace is not saved on the server.';
  }
}

$('#shareButton').addEventListener('click', (event) => {
  if (!sharePopover.hidden) {
    sharePopover.hidden = true;
    return;
  }
  openSharePopover(event.currentTarget);
});

$$('[data-copy]', sharePopover).forEach((button) => button.addEventListener('click', async () => {
  const field = document.getElementById(button.dataset.copy);
  if (!field.value) return;
  field.select();
  try {
    await navigator.clipboard.writeText(field.value);
  } catch {
    // Clipboard permission can be refused; the text is selected either way, so
    // the manual copy is one keystroke rather than a dead end.
    document.execCommand?.('copy');
  }
  const previous = button.textContent;
  button.textContent = 'Copied';
  setTimeout(() => { button.textContent = previous; }, 1400);
}));

document.addEventListener('pointerdown', (event) => {
  if (!event.target.closest('#shareButton') && !event.target.closest('.share-popover')) {
    sharePopover.hidden = true;
  }
});

$('#objectList').addEventListener('click', (event) => {
  const row = event.target.closest('[data-object-id]');
  if (!row) return;
  const object = sceneObjects.find((item) => item.id === row.dataset.objectId);
  if (!object) return;
  if (event.target.closest('.visibility-toggle')) {
    object.visible = !object.visible;
    renderObjectList();
    meshViewport.draw();
    log(`${object.name} ${object.visible ? 'shown' : 'hidden'}.`, 'info');
    return;
  }
  selectObject(object.id);
});

const DEFAULT_TOLERANCE = 0.2;
const TOLERANCE_STORAGE_KEY = 'reopenscad.tolerance';

// Returns null (not 0) for anything that is not a usable clearance.
function readTolerance() {
  const raw = String($('#toleranceInput').value ?? '').trim();
  if (!raw) return null;
  const parsed = Number(raw);
  return Number.isFinite(parsed) && parsed >= 0 ? parsed : null;
}

// localStorage carries the tolerance across reloads and into NEW workspaces; the workspace's
// own `tolerance` field (applied by applyWorkspaceSettings) carries it along the shared URL.
function restoreTolerance() {
  const stored = Number(localStorage.getItem(TOLERANCE_STORAGE_KEY));
  $('#toleranceInput').value = String(Number.isFinite(stored) && stored >= 0 && localStorage.getItem(TOLERANCE_STORAGE_KEY) !== null ? stored : DEFAULT_TOLERANCE);
}

$('#toleranceInput').addEventListener('change', () => {
  const tolerance = readTolerance();
  if (tolerance === null) {
    $('#toleranceInput').value = String(DEFAULT_TOLERANCE);
    localStorage.setItem(TOLERANCE_STORAGE_KEY, String(DEFAULT_TOLERANCE));
    saveWorkspaceSettings();
    return;
  }
  localStorage.setItem(TOLERANCE_STORAGE_KEY, String(tolerance));
  saveWorkspaceSettings();
});

async function checkIntersections() {
  const button = $('#checkIntersections');
  const results = $('#intersectionResults');
  const state = $('#inspectionState');
  const tolerance = readTolerance();
  // Never silently coerce a bad tolerance to 0 — that would report a clean model as clean
  // for entirely the wrong reason.
  if (tolerance === null) {
    state.className = 'inspection-state';
    state.textContent = 'Invalid';
    results.innerHTML = '<div class="inspection-empty">Enter a tolerance of 0 mm or more.</div>';
    $('#toleranceInput').focus();
    return;
  }
  const visibleObjectIds = sceneObjects.filter((object) => object.visible !== false).map((object) => object.id);
  button.disabled = true;
  state.className = 'inspection-state';
  state.textContent = 'Checking';
  results.innerHTML = '<div class="inspection-empty">Measuring visible parts…</div>';
  try {
    // The server re-compiles from the STORED source, so a check right after typing would
    // otherwise measure the previous revision.
    await flushWorkspaceSave();
    const response = await fetch(`/api/workspaces/${encodeURIComponent(workspaceId)}/intersections`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ tolerance, visibleObjectIds }),
    });
    if (!response.ok) throw new Error(response.status === 404 ? 'Intersection service is not available yet.' : `Intersection check failed (${response.status}).`);
    const data = await response.json();
    const intersections = Array.isArray(data.intersections)
      ? data.intersections.filter((item) => item.intersects || item.withinTolerance)
      : [];
    intersectionIds = new Set(intersections.flatMap((item) => [String(item.a?.id ?? item.a), String(item.b?.id ?? item.b)]));
    renderObjectList();
    meshViewport.draw();
    if (!intersections.length) {
      state.textContent = 'Clear';
      state.classList.add('clear');
      results.innerHTML = `<div class="inspection-empty">No intersections within ${tolerance.toFixed(2)} mm.</div>`;
      log(`Intersection check clear at ${tolerance.toFixed(2)} mm tolerance.`, 'success');
    } else {
      state.textContent = `${intersections.length} found`;
      state.classList.add('collision');
      results.innerHTML = intersections.map((item, index) => {
        const a = item.a?.name || sceneObjects.find((object) => object.id === String(item.a?.id ?? item.a))?.name || item.a;
        const b = item.b?.name || sceneObjects.find((object) => object.id === String(item.b?.id ?? item.b))?.name || item.b;
        const depth = Number.isFinite(Number(item.depth))
          ? `${Number(item.depth).toFixed(3)} mm overlap`
          : `${Number(item.clearance || 0).toFixed(3)} mm clearance`;
        // The engine reports the world-space point of deepest penetration (or of
        // closest approach), which is what makes a result actionable.
        const at = Array.isArray(item.location) && item.location.every((value) => Number.isFinite(Number(value)))
          ? ` at ${item.location.map((value) => Number(value).toFixed(2)).join(', ')}`
          : '';
        return `<button class="intersection-result" data-intersection-index="${index}"><i></i><span><b>${escapeHtml(a)} × ${escapeHtml(b)}</b><small>${depth}${escapeHtml(at)}</small></span></button>`;
      }).join('');
      $$('.intersection-result', results).forEach((resultButton) => resultButton.addEventListener('click', () => {
        const intersection = intersections[Number(resultButton.dataset.intersectionIndex)];
        selectObject(String(intersection.a?.id ?? intersection.a));
      }));
      log(`${intersections.length} part intersection${intersections.length === 1 ? '' : 's'} found.`, 'warning');
    }
  } catch (error) {
    intersectionIds.clear();
    renderObjectList();
    meshViewport.draw();
    state.textContent = 'Unavailable';
    results.innerHTML = `<div class="inspection-empty">${escapeHtml(error.message)}</div>`;
  } finally {
    button.disabled = false;
  }
}

$('#checkIntersections').addEventListener('click', checkIntersections);
$('#copyWorkspaceLink').addEventListener('click', async () => {
  try {
    await navigator.clipboard.writeText(location.href);
    const label = $('#workspaceName');
    const original = label.textContent;
    label.textContent = 'link copied';
    setTimeout(() => { label.textContent = original; }, 1200);
  } catch {
    log('Could not copy the workspace link.', 'warning');
  }
});
// ---- Agent / MCP connection panel ---------------------------------------
// Everything here is derived at runtime: the endpoint comes from the origin the
// page was actually served from (so REOPENSCAD_PUBLIC_ORIGIN, a reverse proxy or
// a different port are all correct for free), and the tool list is read from the
// live `tools/list` so it can never drift from what the server implements.
const MCP_MODERN_VERSION = '2026-07-28';
const MCP_PROTOCOL_VERSION = '2025-06-18';
const MCP_CLIENT_STORAGE_KEY = 'reopenscad.mcpClient';
let mcpRequestId = 0;
let mcpToolsLoaded = false;
let mcpProbeInFlight = false;

function mcpEndpointUrl() {
  return `${location.origin}/mcp`;
}

function mcpAgentBriefing(endpoint, workspace) {
  return [
    `Use the "reopenscad" MCP server at ${endpoint}.`,
    `My workspace id is ${workspace}.`,
    'Read the current source with get_workspace_code, then edit it with update_workspace_code',
    '(pass the revision it returns as expectedRevision). Check the result with check_intersections',
    'at a 0.2 mm tolerance, and give me the workspace link when you are done.',
  ].join(' ');
}

// Each entry renders one tab. `code` strings are what lands on the clipboard, so
// they stay literal — no HTML, no smart quotes, no leading indentation to strip.
const MCP_CLIENTS = [
  {
    id: 'claude-code',
    label: 'Claude Code',
    doc: 'https://code.claude.com/docs/en/mcp',
    build: (endpoint) => ({
      lead: 'One command, from any directory. <b>--scope user</b> registers the server for all your projects; drop it and it stays local to the current one.',
      snippets: [
        { title: 'Add it', caption: 'terminal', code: `claude mcp add --scope user --transport http reopenscad ${endpoint}` },
        { title: 'Confirm it', caption: 'look for “reopenscad ✓ Connected”', code: 'claude mcp list' },
        {
          title: 'Or check it into the repo',
          caption: '.mcp.json in the project root (--scope project)',
          code: JSON.stringify({ mcpServers: { reopenscad: { type: 'http', url: endpoint } } }, null, 2),
        },
      ],
      footnote: 'The CLI transport keyword is <em>http</em>; in JSON, <em>streamable-http</em> is accepted as an alias for it.',
    }),
  },
  {
    id: 'claude-desktop',
    label: 'Claude Desktop',
    doc: 'https://modelcontextprotocol.io/docs/develop/connect-local-servers',
    build: (endpoint) => ({
      lead: '<b>claude_desktop_config.json</b> only speaks stdio — it has no <em>url</em> field — so a local HTTP server needs a bridge. Add this, then restart Claude Desktop.',
      snippets: [
        {
          title: 'Desktop config',
          caption: 'macOS ~/Library/Application Support/Claude/claude_desktop_config.json · Windows %APPDATA%\\Claude\\claude_desktop_config.json',
          code: JSON.stringify(
            { mcpServers: { reopenscad: { command: 'npx', args: ['-y', 'mcp-remote', endpoint, '--allow-http'] } } },
            null,
            2,
          ),
        },
        { title: 'Already set up in Claude Code?', caption: 'imports the other direction, macOS and WSL', code: 'claude mcp add-from-claude-desktop' },
      ],
      footnote: 'Settings → Connectors → <b>Add custom connector</b> also takes a URL, but Anthropic documents custom connectors as needing to be reachable from their servers over the public internet — a <em>127.0.0.1</em> endpoint generally is not. <b>mcp-remote</b> is a community bridge, not an Anthropic-documented tool; the flags may drift.',
    }),
  },
  {
    id: 'cursor',
    label: 'Cursor',
    doc: 'https://cursor.com/docs/mcp',
    build: (endpoint) => ({
      lead: 'Cursor treats any entry with a <b>url</b> as a remote server and tries streamable HTTP first — no <em>type</em> field needed.',
      snippets: [
        {
          title: 'Cursor config',
          caption: '~/.cursor/mcp.json (everywhere) · .cursor/mcp.json (one project)',
          code: JSON.stringify({ mcpServers: { reopenscad: { url: endpoint } } }, null, 2),
        },
      ],
      footnote: 'Enable <b>reopenscad</b> under Settings → MCP, then use it from Agent mode.',
    }),
  },
  {
    id: 'vscode',
    label: 'VS Code',
    doc: 'https://code.visualstudio.com/docs/copilot/customization/mcp-servers',
    build: (endpoint) => ({
      lead: 'VS Code reads <b>.vscode/mcp.json</b>. Watch the key: it is <b>servers</b>, not <em>mcpServers</em>, and <em>type</em> is required for remote servers.',
      snippets: [
        {
          title: 'Workspace config',
          caption: '.vscode/mcp.json',
          code: JSON.stringify({ servers: { reopenscad: { type: 'http', url: endpoint } } }, null, 2),
        },
        {
          title: 'Same thing, but for every workspace',
          caption: 'Command Palette → “MCP: Open User Configuration”',
          code: JSON.stringify({ servers: { reopenscad: { type: 'http', url: endpoint } } }, null, 2),
        },
      ],
      footnote: 'Open Chat, switch to <b>Agent</b> mode, and select the tools from the tools picker.',
    }),
  },
  {
    id: 'other',
    label: 'More clients',
    doc: 'https://modelcontextprotocol.io/specification/2025-06-18/basic/transports',
    build: (endpoint) => ({
      lead: 'Any client that speaks the MCP <b>streamable-http</b> transport works — but they disagree about the key name, so here are the verified shapes:',
      snippets: [
        {
          title: 'Windsurf',
          caption: '~/.codeium/windsurf/mcp_config.json — note serverUrl, not url',
          code: JSON.stringify({ mcpServers: { reopenscad: { serverUrl: endpoint } } }, null, 2),
        },
        {
          title: 'Zed',
          caption: 'zed: open settings file — top-level key context_servers',
          code: JSON.stringify({ context_servers: { reopenscad: { url: endpoint } } }, null, 2),
        },
        {
          title: 'Gemini CLI',
          caption: '~/.gemini/settings.json — note httpUrl',
          code: JSON.stringify({ mcpServers: { reopenscad: { httpUrl: endpoint, timeout: 5000 } } }, null, 2),
        },
        {
          title: 'Cline',
          caption: 'MCP panel → Configure — note the camelCase transport',
          code: JSON.stringify({ mcpServers: { reopenscad: { type: 'streamableHttp', url: endpoint, disabled: false, autoApprove: [] } } }, null, 2),
        },
        {
          title: 'Codex CLI',
          caption: '~/.codex/config.toml',
          code: `[mcp_servers.reopenscad]\nurl = "${endpoint}"`,
        },
        {
          title: 'Or wire it by hand',
          caption: `current era (${MCP_MODERN_VERSION}): stateless, version and capabilities per request`,
          code: `curl -s ${endpoint} \\\n  -H 'Content-Type: application/json' \\\n  -H 'MCP-Protocol-Version: ${MCP_MODERN_VERSION}' \\\n  -H 'Mcp-Method: server/discover' \\\n  -d '{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"${MCP_MODERN_VERSION}","io.modelcontextprotocol/clientCapabilities":{}}}}'`,
        },
        {
          title: 'Older clients still work',
          caption: `initialize-based era (${MCP_PROTOCOL_VERSION} and earlier): handshake, then MCP-Protocol-Version on every later call`,
          code: `curl -s ${endpoint} \\\n  -H 'Content-Type: application/json' \\\n  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"${MCP_PROTOCOL_VERSION}","capabilities":{},"clientInfo":{"name":"demo","version":"1"}}}'`,
        },
      ],
      footnote: 'This server is dual-era: it answers the current stateless revision <em>and</em> the older <em>initialize</em> handshake on the same endpoint, so a client from either generation connects. The connection check on the left names the revision actually negotiated — read from a live handshake, never a stale claim. POST only: a <em>GET</em> answers 405 by design, which is what the spec asks of a server with no SSE stream, and there is no binary to install.',
    }),
  },
];

function selectedMcpClient() {
  const stored = localStorage.getItem(MCP_CLIENT_STORAGE_KEY);
  return MCP_CLIENTS.find((client) => client.id === stored) || MCP_CLIENTS[0];
}

function renderMcpTabs() {
  const active = selectedMcpClient();
  $('#mcpTabs').innerHTML = MCP_CLIENTS
    .map((client) => `<button type="button" role="tab" data-client="${client.id}" aria-selected="${client.id === active.id}">${escapeHtml(client.label)}</button>`)
    .join('');
}

function renderMcpClient() {
  const endpoint = mcpEndpointUrl();
  const workspace = workspaceId || 'your-workspace-id';
  const client = selectedMcpClient();
  const view = client.build(endpoint, workspace);
  const snippets = view.snippets
    .map((snippet) => `<div class="mcp-snippet">
        <div class="mcp-snippet-head"><b>${escapeHtml(snippet.title)}</b><small>${escapeHtml(snippet.caption)}</small><button type="button" class="mcp-copy"><i></i><span>Copy</span></button></div>
        <pre>${escapeHtml(snippet.code)}</pre>
      </div>`)
    .join('');
  $('#mcpClient').innerHTML = `<p>${view.lead}</p>${snippets}${view.footnote ? `<p>${view.footnote}</p>` : ''}
    <div class="mcp-snippet prose">
      <div class="mcp-snippet-head"><b>First message to the agent</b><small>includes this workspace</small><button type="button" class="mcp-copy"><i></i><span>Copy</span></button></div>
      <pre>${escapeHtml(mcpAgentBriefing(endpoint, workspace))}</pre>
    </div>
    <span class="mcp-verified">Setup steps follow <a href="${escapeHtml(client.doc)}" target="_blank" rel="noreferrer noopener">the official ${escapeHtml(client.label)} documentation</a>.</span>`;
}

function renderMcpTools(tools) {
  const grid = $('#mcpToolGrid');
  if (!tools.length) {
    grid.innerHTML = '<div class="mcp-tools-empty">The server advertised no tools.</div>';
    return;
  }
  grid.innerHTML = tools
    .map((tool) => `<div class="mcp-tool"><b>${escapeHtml(tool.name)}</b><span>${escapeHtml(tool.description || '')}</span></div>`)
    .join('');
}

function setMcpStatus(state, message) {
  const status = $('#mcpStatus');
  status.dataset.state = state;
  $('span', status).textContent = message;
}

async function mcpRpc(method, params, protocol, era = 'legacy') {
  const headers = { 'Content-Type': 'application/json' };
  if (method !== 'initialize') headers['MCP-Protocol-Version'] = protocol;
  const body = { jsonrpc: '2.0', id: (mcpRequestId += 1), method, params: { ...(params || {}) } };
  if (era === 'modern') {
    // The modern era carries version and capabilities per request instead of in a
    // handshake, and mirrors the method in a header so proxies can route on it.
    headers['Mcp-Method'] = method;
    body.params._meta = {
      'io.modelcontextprotocol/protocolVersion': protocol,
      'io.modelcontextprotocol/clientCapabilities': {},
      'io.modelcontextprotocol/clientInfo': { name: 'reopenscad-web', version: '1' },
    };
  }
  const response = await fetch(mcpEndpointUrl(), { method: 'POST', headers, body: JSON.stringify(body) });
  let payload = null;
  try {
    payload = await response.json();
  } catch {
    payload = null;
  }
  if (payload?.error) throw new Error(payload.error.message || `MCP error ${payload.error.code}`);
  if (!payload || typeof payload.result !== 'object') throw new Error(`Unexpected reply from ${method} (HTTP ${response.status}).`);
  return payload.result;
}

// A real handshake against the live endpoint, doubling as the source of the tool
// cards — so a green light and the list below it always describe the same server.
// The modern (stateless, per-request `_meta`) era is tried first because that is
// what a current client will use; a server that only speaks the older
// initialize-based revisions answers the fallback instead.
async function probeMcp() {
  if (mcpProbeInFlight) return;
  mcpProbeInFlight = true;
  const button = $('#mcpTest');
  const facts = $('#mcpFacts');
  button.disabled = true;
  let era = 'modern';
  let identity = null;
  let negotiated = MCP_MODERN_VERSION;
  let advertised = null;
  try {
    setMcpStatus('testing', 'Calling server/discover…');
    try {
      const discovered = await mcpRpc('server/discover', {}, MCP_MODERN_VERSION, 'modern');
      identity = discovered._meta?.['io.modelcontextprotocol/serverInfo'] ?? null;
      advertised = Array.isArray(discovered.supportedVersions) ? discovered.supportedVersions : null;
    } catch {
      era = 'legacy';
      setMcpStatus('testing', 'Falling back to initialize…');
      const handshake = await mcpRpc('initialize', {
        protocolVersion: MCP_PROTOCOL_VERSION,
        capabilities: {},
        clientInfo: { name: 'reopenscad-web', version: '1' },
      });
      identity = handshake.serverInfo ?? null;
      negotiated = handshake.protocolVersion || MCP_PROTOCOL_VERSION;
    }
    setMcpStatus('testing', 'Listing tools…');
    const listed = await mcpRpc('tools/list', {}, negotiated, era);
    const tools = Array.isArray(listed.tools) ? listed.tools : [];
    renderMcpTools(tools);
    mcpToolsLoaded = true;
    $('#mcpToolCount').textContent = `${tools.length} tool${tools.length === 1 ? '' : 's'}, read live from tools/list`;
    setMcpStatus('ok', `Connected. ${tools.length} tool${tools.length === 1 ? '' : 's'} available.`);
    facts.hidden = false;
    facts.innerHTML = `<dt>Server</dt><dd>${escapeHtml(`${identity?.name ?? 'unknown'} ${identity?.version ?? ''}`.trim())}</dd>
      <dt>Protocol</dt><dd>${escapeHtml(negotiated)}${era === 'modern' ? ' <b>current</b>' : ''}</dd>
      ${advertised ? `<dt>Speaks</dt><dd>${escapeHtml(advertised.join(', '))}</dd>` : ''}
      <dt>Tools</dt><dd>${tools.length}</dd>`;
  } catch (error) {
    facts.hidden = true;
    setMcpStatus('error', error.message || 'The endpoint did not answer.');
    if (!mcpToolsLoaded) $('#mcpToolGrid').innerHTML = '<div class="mcp-tools-empty">Could not reach the MCP endpoint, so there is nothing verified to list.</div>';
  } finally {
    button.disabled = false;
    mcpProbeInFlight = false;
  }
}

async function copyForMcp(button) {
  // Either a named element (the endpoint, the workspace id) or the snippet this button sits in.
  const targetId = button.dataset.copy;
  const text = targetId
    ? document.getElementById(targetId)?.textContent ?? ''
    : button.closest('.mcp-snippet')?.querySelector('pre')?.textContent ?? '';
  if (!text) return;
  const label = $('span', button);
  if (!button.dataset.idleLabel) button.dataset.idleLabel = label.textContent;
  try {
    await navigator.clipboard.writeText(text);
    label.textContent = 'Copied';
    button.classList.add('copied');
    clearTimeout(Number(button.dataset.copyTimer));
    button.dataset.copyTimer = String(setTimeout(() => {
      label.textContent = button.dataset.idleLabel;
      button.classList.remove('copied');
    }, 1200));
  } catch {
    log('Could not copy to the clipboard.', 'warning');
  }
}

function openMcpPanel() {
  $('#mcpEndpoint').textContent = mcpEndpointUrl();
  $('#mcpWorkspaceId').textContent = workspaceId || 'not created yet';
  renderMcpTabs();
  renderMcpClient();
  $('#mcpModal').hidden = false;
  $('#closeMcp').focus();
  // The page is served by the same process that answers /mcp, so a first probe
  // on open costs one round trip and turns the panel's claims into evidence.
  if (!mcpToolsLoaded) probeMcp();
}

function closeMcpPanel() {
  if ($('#mcpModal').hidden) return;
  $('#mcpModal').hidden = true;
  $('#mcpButton').focus();
}

// The MCP surface is the least discoverable thing in the app, so the button
// asks for attention until it has been opened once — then never again.
const AGENT_BUTTON_SEEN_KEY = 'reopenscad.mcpButtonSeen';

function markAgentButtonSeen() {
  $('#mcpButton').classList.remove('unseen');
  try {
    localStorage.setItem(AGENT_BUTTON_SEEN_KEY, '1');
  } catch {
    // Blocked storage only costs the user a repeat nudge next session.
  }
}

function restoreAgentButtonNudge() {
  let seen = false;
  try {
    seen = localStorage.getItem(AGENT_BUTTON_SEEN_KEY) === '1';
  } catch {
    seen = false;
  }
  $('#mcpButton').classList.toggle('unseen', !seen);
}

$('#mcpButton').addEventListener('click', () => {
  markAgentButtonSeen();
  openMcpPanel();
});
$('#closeMcp').addEventListener('click', closeMcpPanel);
$('#mcpScrim').addEventListener('click', closeMcpPanel);
$('#mcpTest').addEventListener('click', probeMcp);
$('#mcpTabs').addEventListener('click', (event) => {
  const tab = event.target.closest('[data-client]');
  if (!tab) return;
  localStorage.setItem(MCP_CLIENT_STORAGE_KEY, tab.dataset.client);
  renderMcpTabs();
  renderMcpClient();
});
$('#mcpModal').addEventListener('click', (event) => {
  const button = event.target.closest('.mcp-copy');
  if (button) copyForMcp(button);
});
window.addEventListener('keydown', (event) => {
  if (event.key === 'Escape') closeMcpPanel();
});

// ---- First-use disclaimer ---------------------------------------------------
// The acknowledgement key carries a revision. Bump DISCLAIMER_REVISION whenever
// the terms in #disclaimerModal materially change and everyone sees the dialog
// again, because their stored consent was to the previous text, not this one.
const DISCLAIMER_REVISION = 1;
const DISCLAIMER_STORAGE_KEY = `reopenscad.disclaimerAck.v${DISCLAIMER_REVISION}`;
let disclaimerReturnFocus = null;

// Storage throws in a blocked-cookie or private context. A disclaimer that
// cannot remember itself is a nuisance; one that throws out of bootstrap would
// take the whole app down with it, so both sides are guarded.
function disclaimerAcknowledged() {
  try {
    return localStorage.getItem(DISCLAIMER_STORAGE_KEY) === '1';
  } catch {
    return false;
  }
}

function openDisclaimer() {
  const modal = $('#disclaimerModal');
  if (!modal.hidden) return;
  // Lifted verbatim out of the status bar rather than restated, so the
  // non-affiliation sentence has exactly one author and cannot drift.
  $('#disclaimerAffiliation').textContent = $('.status-attribution > span')?.textContent.trim() ?? '';
  disclaimerReturnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  modal.hidden = false;
  $('#acceptDisclaimer').focus();
}

function closeDisclaimer() {
  const modal = $('#disclaimerModal');
  if (modal.hidden) return;
  modal.hidden = true;
  const restore = disclaimerReturnFocus;
  disclaimerReturnFocus = null;
  // On first use nothing was focused yet (activeElement was <body>), so hand the
  // caret to the editor — the thing the reader came here to type in.
  if (restore && restore.isConnected && restore !== document.body) restore.focus();
  else editor.focus();
}

function acceptDisclaimer() {
  try {
    localStorage.setItem(DISCLAIMER_STORAGE_KEY, '1');
  } catch {
    // Unable to persist: the dialog will return next load. Better than failing.
  }
  closeDisclaimer();
}

function disclaimerFocusables() {
  return $$('button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])', $('#disclaimerModal'))
    .filter((element) => !element.disabled && element.tabIndex !== -1);
}

$('#acceptDisclaimer').addEventListener('click', acceptDisclaimer);
$('#showDisclaimer').addEventListener('click', openDisclaimer);
// Deliberately no Escape handler and no scrim click: acknowledging is the point
// of the dialog, and a reflex keypress is not an acknowledgement. The button is
// the only exit, which is also why it is the element that takes focus on open.
$('#disclaimerModal').addEventListener('keydown', (event) => {
  if (event.key !== 'Tab') return;
  const focusable = disclaimerFocusables();
  if (!focusable.length) { event.preventDefault(); return; }
  const first = focusable[0];
  const last = focusable[focusable.length - 1];
  const inside = $('#disclaimerModal').contains(document.activeElement);
  if (event.shiftKey && (!inside || document.activeElement === first)) { event.preventDefault(); last.focus(); }
  else if (!event.shiftKey && (!inside || document.activeElement === last)) { event.preventDefault(); first.focus(); }
});
// Backstop for focus the trap above cannot see: programmatic focus from the
// booting app, or the browser restoring focus after the address bar.
document.addEventListener('focusin', (event) => {
  const modal = $('#disclaimerModal');
  if (modal.hidden || modal.contains(event.target)) return;
  $('#acceptDisclaimer').focus();
});

function showDisclaimerIfUnacknowledged() {
  if (!disclaimerAcknowledged()) openDisclaimer();
}

const syncControl = $('#syncState');
syncControl.setAttribute('role', 'button');
syncControl.tabIndex = 0;
function resolveWorkspaceConflict() {
  if (!workspaceConflict) return;
  const remote = workspaceFromPayload(workspaceConflict);
  const remoteRevision = Number(remote.revision);
  if (!Number.isFinite(remoteRevision)) {
    log('The remote conflict did not include a valid revision. Reload the workspace to resolve it.', 'error');
    return;
  }
  let resolver = document.getElementById('workspaceConflictResolver');
  if (!resolver) {
    resolver = document.createElement('div');
    resolver.id = 'workspaceConflictResolver';
    resolver.className = 'workspace-conflict-resolver';
    resolver.innerHTML = '<b>Source conflict</b><p>The workspace changed remotely while you were editing.</p><div><button data-resolution="remote">Load remote</button><button data-resolution="local">Keep mine</button></div>';
    document.body.append(resolver);
    resolver.addEventListener('click', async (event) => {
      const resolution = event.target.closest('[data-resolution]')?.dataset.resolution;
      if (!resolution || !workspaceConflict) return;
      const conflict = workspaceConflict;
      const currentRemoteRevision = Number(workspaceFromPayload(conflict).revision);
      workspaceConflict = null;
      resolver.remove();
      if (resolution === 'local') {
        workspaceRevision = currentRemoteRevision;
        workspaceHasLocalChanges = true;
        setSyncState('Saving', 'saving');
        await saveWorkspace();
      } else {
        applyWorkspace(conflict, { remote: true, force: true });
        setSyncState('Synced', 'synced');
        render('preview');
      }
      beginWorkspacePolling(workspaceGeneration);
    });
  }
  const rect = syncControl.getBoundingClientRect();
  resolver.style.top = `${rect.bottom + 8}px`;
  resolver.style.right = `${Math.max(12, innerWidth - rect.right)}px`;
}
syncControl.addEventListener('click', resolveWorkspaceConflict);
syncControl.addEventListener('keydown', (event) => {
  if (workspaceConflict && (event.key === 'Enter' || event.key === ' ')) {
    event.preventDefault();
    resolveWorkspaceConflict();
  }
});
$('#dismissAiHighlight').addEventListener('click', () => {
  aiUpdatedRanges = [];
  $('#aiRevisionBadge').hidden = true;
  syncEditor(false);
});

const splitter = $('#mainSplitter');
splitter.addEventListener('pointerdown', (event) => {
  splitter.setPointerCapture(event.pointerId);
  splitter.classList.add('dragging');
});
splitter.addEventListener('pointermove', (event) => {
  if (!splitter.hasPointerCapture(event.pointerId)) return;
  const objectsWidth = $('#objectList').closest('.objects-pane')?.getBoundingClientRect().width || 0;
  const available = Math.max(600, window.innerWidth - objectsWidth);
  const editorWidth = Math.max(280, Math.min(available * .68, event.clientX - objectsWidth));
  document.documentElement.style.setProperty('--editor-share', `${editorWidth}px`);
});
splitter.addEventListener('pointerup', (event) => {
  splitter.releasePointerCapture(event.pointerId);
  splitter.classList.remove('dragging');
});

window.addEventListener('keydown', (event) => {
  if (event.key === 'F5') { event.preventDefault(); render('preview'); }
  if (event.key === 'F6') { event.preventDefault(); render('render'); }
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 's') { event.preventDefault(); saveScad(); }
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'o') { event.preventDefault(); $('#fileInput').click(); }
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'n') { event.preventDefault(); $('#newButton').click(); }
});

function shareTokenFromLocation() {
  const match = location.pathname.match(/^\/shared\/([0-9a-f]{32})\/?$/);
  return match ? match[1] : null;
}

async function loadSharedWorkspace(token) {
  readOnlyToken = token;
  advanceWorkspaceGeneration();
  // No id, so nothing can save, poll, upload a preview or mint a share link.
  workspaceId = null;
  workspaceOnline = false;
  document.body.classList.add('read-only');
  $('#readonlyBanner').hidden = false;
  editor.readOnly = true;
  $('#shareButton').hidden = true;
  setSyncState('Read only', 'local');
  try {
    const response = await fetch(`/api/shared/${encodeURIComponent(token)}`);
    if (!response.ok) throw new Error(`shared ${response.status}`);
    const payload = await response.json();
    editor.value = typeof payload.code === 'string' ? payload.code : '';
    syncEditor(false);
    applyWorkspaceSettings(payload);
    $('#workspaceName').textContent = 'Shared view';
  } catch {
    log('This read-only link is not valid. It may have been removed.', 'error');
    setStatus('Ready');
  }
}

// Every way out of a read-only view goes through here — the fork button, but
// also New and Open, which are still live and would otherwise leave the banner
// up over a workspace that is perfectly editable.
function leaveReadOnly() {
  if (!readOnlyToken) return;
  readOnlyToken = null;
  document.body.classList.remove('read-only');
  $('#readonlyBanner').hidden = true;
  editor.readOnly = false;
  $('#shareButton').hidden = false;
}

// The way out a reader is offered: the same source in a workspace of their
// own. Nothing is copied beyond what they can already see, so this needs no
// permission they do not have.
$('#forkWorkspace').addEventListener('click', async () => {
  const code = editor.value;
  leaveReadOnly();
  await createWorkspace(code, { navigation: 'push' });
  if (editor.value.trim()) render('preview');
});

function workspaceIdFromLocation() {
  const routeMatch = location.pathname.match(/^\/workspaces\/([^/]+)\/?$/);
  return routeMatch ? decodeURIComponent(routeMatch[1]) : new URLSearchParams(location.search).get('workspace');
}

async function bootstrap() {
  // Synchronous, awaits nothing, and nothing below awaits it: the workspace is
  // created and the first preview is rendered underneath the dialog exactly as
  // it would be without one.
  showDisclaimerIfUnacknowledged();
  const requestedId = workspaceIdFromLocation();
  // Legacy unscoped cache: it belonged to whichever workspace was edited last, so visiting
  // "/" used to clone that source into a brand new workspace. Drop it for good.
  localStorage.removeItem('reopenscad.code');
  populatePlateSelect();
  restoreTolerance();
  restoreAgentButtonNudge();
  // Before the workspace loads, so the locally preferred bed is already on screen and is
  // what createWorkspace seeds a new workspace with.
  restorePlatePreference();
  restoreGroundPreference();
  editor.value = '';
  syncEditor(false);
  checkEngine();
  const sharedToken = shareTokenFromLocation();
  if (sharedToken) await loadSharedWorkspace(sharedToken);
  else if (requestedId) await loadWorkspace(requestedId);
  else await createWorkspace('');
  if (editor.value.trim()) setTimeout(() => render('preview'), 180);
}

// Back/forward moves between workspaces instead of stranding them.
window.addEventListener('popstate', async () => {
  const id = workspaceIdFromLocation();
  if (!id || id === workspaceId) return;
  await loadWorkspace(id);
  if (editor.value.trim()) render('preview');
});

document.addEventListener('visibilitychange', () => {
  if (!document.hidden) beginWorkspacePolling();
});

bootstrap();
