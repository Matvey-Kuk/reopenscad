const $ = (selector, root = document) => root.querySelector(selector);
const $$ = (selector, root = document) => [...root.querySelectorAll(selector)];

// Kick-start models. The catalogue lives in `examples/manifest.json` next to
// the `.scad` sources and their preview renders, rather than inline here: the
// sources are real files that the test suite compiles and the OpenSCAD binary
// checks, which a string literal in a bundle could never be.
// Absolute, because the app is served at `/designs/<id>` as well as at
// `/`, and a relative path would look for the catalogue inside the
// design route.
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
const RECENTS_STORAGE_KEY = 'reopenscad.recentDesigns';
// Per-design source cache, keyed `${DESIGN_CACHE_PREFIX}${id}`.
const DESIGN_CACHE_PREFIX = 'reopenscad.design.';
// Keys from before workspaces were renamed to designs. Read once, copied to the
// new keys and removed, so an upgrade keeps every recent and every cached
// source without anything having to know about the old names afterwards.
const LEGACY_RECENTS_STORAGE_KEY = 'reopenscad.recentWorkspaces';
const LEGACY_CACHE_PREFIX = 'reopenscad.workspace.';

// Both keys exist only when a tab still running the pre-rename code kept
// writing the old one. Its copy wins when it holds unsaved edits the new one
// does not, or a later revision, so those edits are not dropped silently.
function legacyCacheWins(legacy, current) {
  if (current === null) return true;
  const read = (text) => {
    try {
      return JSON.parse(text) || {};
    } catch {
      return {};
    }
  };
  const [old, now] = [read(legacy), read(current)];
  if (typeof old.code !== 'string') return false;
  if (typeof now.code !== 'string') return true;
  if ((Number(old.revision) || 0) !== (Number(now.revision) || 0)) return (Number(old.revision) || 0) > (Number(now.revision) || 0);
  return old.dirty === true && now.dirty !== true;
}

function migrateLegacyStorage(storage = localStorage) {
  try {
    const legacyRecents = storage.getItem(LEGACY_RECENTS_STORAGE_KEY);
    if (legacyRecents !== null) {
      // Parsed one at a time, so a corrupt list under one key cannot cost the
      // entries under the other.
      const parsed = (text) => {
        try {
          const value = JSON.parse(text || '[]');
          return Array.isArray(value) ? value : [];
        } catch {
          return [];
        }
      };
      let merged = [...parsed(storage.getItem(RECENTS_STORAGE_KEY)), ...parsed(legacyRecents)];
      const seen = new Set();
      merged = merged.filter((entry) => entry && typeof entry.id === 'string' && !seen.has(entry.id) && seen.add(entry.id));
      if (merged.length) {
        merged.sort((a, b) => (Number(b.visitedAt) || 0) - (Number(a.visitedAt) || 0));
        storage.setItem(RECENTS_STORAGE_KEY, JSON.stringify(merged));
      }
      storage.removeItem(LEGACY_RECENTS_STORAGE_KEY);
    }
    const legacyKeys = [];
    for (let index = 0; index < storage.length; index += 1) {
      const key = storage.key(index);
      if (key && key.startsWith(LEGACY_CACHE_PREFIX)) legacyKeys.push(key);
    }
    for (const key of legacyKeys) {
      const target = DESIGN_CACHE_PREFIX + key.slice(LEGACY_CACHE_PREFIX.length);
      const legacy = storage.getItem(key);
      if (legacyCacheWins(legacy, storage.getItem(target))) storage.setItem(target, legacy);
      storage.removeItem(key);
    }
  } catch {
    // Blocked or full storage: the old keys stay where they are, and the
    // readers below still fall back to them.
  }
}
// The stored list is unbounded: dropping the oldest entry used to lose the only
// link this browser had to that design. The landing page shows the newest
// RECENTS_SHOWN and folds the rest behind a "Show all" toggle instead.
const RECENTS_SHOWN = 8;
let recentsExpanded = false;
migrateLegacyStorage();

function readRecents() {
  try {
    const stored = JSON.parse(localStorage.getItem(RECENTS_STORAGE_KEY) ?? localStorage.getItem(LEGACY_RECENTS_STORAGE_KEY) ?? '[]');
    if (!Array.isArray(stored)) return [];
    return stored
      .filter((entry) => entry && typeof entry.id === 'string' && entry.id)
      .map((entry) => ({ id: entry.id, visitedAt: Number(entry.visitedAt) || 0, empty: entry.empty === true }));
  } catch {
    return [];
  }
}

function writeRecents(entries) {
  try {
    localStorage.setItem(RECENTS_STORAGE_KEY, JSON.stringify(entries));
  } catch {
    // A full or blocked store costs the user their history, not their work.
  }
}

// Most recent first, which is the order the list is shown in and therefore the
// order it is stored in — no sort at render time, and no clock read either.
// Every workspace this browser creates or opens is listed, empty ones included:
// an empty workspace is still a real one with a URL, and leaving it out made the
// list contradict its own "workspaces you open" copy. `empty` only changes how
// the row is labelled; left undefined, the previous answer is kept.
function rememberWorkspace(id, { empty } = {}) {
  if (!id) return;
  const previous = readRecents();
  const known = previous.find((entry) => entry.id === id);
  const rest = previous.filter((entry) => entry.id !== id);
  writeRecents([{ id, visitedAt: Date.now(), empty: empty ?? known?.empty ?? false }, ...rest]);
}

// Keeps a listed workspace's label honest when its content changes underneath
// (an agent filling an empty one) without promoting it in the list.
function noteRecentContent(id, empty) {
  const entries = readRecents();
  const entry = entries.find((item) => item.id === id);
  if (!entry || entry.empty === empty) return;
  entry.empty = empty;
  writeRecents(entries);
}

// Moves a listed workspace to a new id, keeping its place: an offline-created
// workspace gets its real id from the server once it is reachable again.
function renameRecentWorkspace(from, to) {
  const entries = readRecents().filter((entry) => entry.id !== to);
  const entry = entries.find((item) => item.id === from);
  if (entry) entry.id = to;
  else entries.unshift({ id: to, visitedAt: Date.now(), empty: false });
  writeRecents(entries);
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
  prompt.setAttribute('aria-label', 'Remove design from the list');
  prompt.innerHTML = '<p>Remove design from the list? It will still be available '
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
    renderLandingRecents();
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
  rememberWorkspace(workspaceId, { empty: false });
  const code = editor.value;
  if (lastPreviewUpload.id === workspaceId && lastPreviewUpload.code === code) return;
  const preview = captureViewportPreview();
  if (!preview) return;
  // Claimed before the request rather than after, so a second render settling
  // while this one is in flight does not post the same image twice.
  lastPreviewUpload = { id: workspaceId, code };
  try {
    await fetch(`/api/designs/${encodeURIComponent(workspaceId)}/preview`, {
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
      // landing page by renderLandingExamples, and deliberately not through
      // `console.*` — see the note on `log`.
      exampleCatalogue = null;
      return null;
    });
  return exampleCataloguePromise;
}

const icons = {
  trash: '<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false"><path d="M4 7h16M9 7V4h6v3M6.5 7l1 13h9l1-13M10 11v6M14 11v6"/></svg>',
  new: '<svg viewBox="0 0 24 24"><path d="M6 3.5h8l4 4V21H6z"/><path d="M14 3.5v4h4M9 13h6M12 10v6"/></svg>',
  open: '<svg viewBox="0 0 24 24"><path d="M3.5 19.5 6 9h14.5L18 19.5z"/><path d="M5 9V5h6l2 2h5v2"/></svg>',
  save: '<svg viewBox="0 0 24 24"><path d="M5 3.5h12l2 2V20H5z"/><path d="M8 3.5V9h8V3.5M8 20v-7h8v7"/></svg>',
  top: '<svg viewBox="0 0 24 24"><path d="m12 3 9 5-9 5-9-5zM5 12l7 4 7-4M5 16l7 4 7-4"/></svg>',
  front: '<svg viewBox="0 0 24 24"><rect x="4" y="4" width="16" height="16"/><path d="M4 9h16M9 4v16"/></svg>',
  right: '<svg viewBox="0 0 24 24"><path d="M5 4h14v16H5zM13 4v16M5 10h8"/></svg>',
  reset: '<svg viewBox="0 0 24 24"><path d="M19 8a8 8 0 1 0 1 7M19 3v5h-5"/></svg>',
  export: '<svg viewBox="0 0 24 24"><path d="M12 3v12M7 10l5 5 5-5M5 19h14"/></svg>',
  agent: '<svg viewBox="0 0 24 24"><rect x="4" y="8" width="16" height="11" rx="2.5"/><path d="M12 4.5V8M9.5 13v1.6M14.5 13v1.6M2.5 12v3M21.5 12v3"/><circle cx="12" cy="3.4" r="1.3"/></svg>',
  fit: '<svg viewBox="0 0 24 24"><path d="M9 4H4v5M15 4h5v5M9 20H4v-5M15 20h5v-5"/></svg>',
  slice: '<svg viewBox="0 0 24 24"><path d="M4 12V7.5L12 3l8 4.5V12" opacity=".4"/><path d="m4 12 8-4.5 8 4.5-8 4.5zM4 12v4.5l8 4.5 8-4.5V12M12 16.5V21"/></svg>',
  flip: '<svg viewBox="0 0 24 24"><path d="M12 3v18" stroke-dasharray="2 2"/><path d="M9 7 4 12l5 5M15 7l5 5-5 5"/></svg>',
  share: '<svg viewBox="0 0 24 24"><circle cx="18" cy="5.5" r="2.6"/><circle cx="6" cy="12" r="2.6"/><circle cx="18" cy="18.5" r="2.6"/><path d="M8.3 10.8 15.7 6.7M8.3 13.2l7.4 4.1"/></svg>',
  comment: '<svg viewBox="0 0 24 24"><path d="M4.5 5.5h15v10h-8l-4.5 4v-4h-2.5z"/><path d="M8.5 9.5h7M8.5 12.5h4"/></svg>',
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
// How long the last real render of this design took here (ms), or null. What the
// re-render bar expects of the next one.
let expectedRenderMs = null;
// The source of the model on screen, to tell whether a cancel leaves it behind the editor.
let renderedSource = null;
let rerenderBarTimer = null;
let rerenderBarHide = null;
let rerenderBarShow = null;
let renderSequence = 0;
// Client-side backstop for the server's own render deadlines (PREVIEW_DEADLINE
// and RENDER_DEADLINE, both 120 s, in backend/src/main.rs). The margin lets the
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
const RENDER_TIMEOUT_MS = { preview: 195_000, render: 195_000 };
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
const EDIT_RENDER_DELAY_MS = 600;
let workspaceSaveDebounce = null;
let workspaceSavePromise = null;
let workspaceSaveController = null;
let workspaceId = null;
let workspaceRevision = 0;
let workspaceOnline = false;
// True for a workspace minted in this browser while the server was unreachable:
// its id exists nowhere but here, so the server answers 404 for it. Once the
// server is back it is created there for real and the page moves to its new id.
let workspaceLocalOnly = false;
let localPromotion = null;
// Offline id -> the server id it was promoted to. Promotion keeps the workspace
// generation, so an action that started under the offline id (a render whose
// save flush is what triggered the promotion) is still the same workspace.
const promotedWorkspaceIds = new Map();

function isCurrentWorkspace(id) {
  return id === workspaceId || (id !== null && promotedWorkspaceIds.get(id) === workspaceId);
}
// Set when the page is a `/shared/<token>` view. Declared here with the rest of
// the workspace state rather than beside the code that uses it, because
// functions defined far earlier read it, and a thousand lines earlier — a `let`
// below its first reader is a temporal-dead-zone trap waiting for someone to
// move a call.
//
// A shared page has no workspace id, deliberately: that is what stops a
// read-only link being an editable one with an extra step. The app runs
// without one — no saving, no polling. Renders go to
// `/api/shared/<token>/render`, which compiles the stored source per part (so
// the parts panel and colours match the editable view) and never answers the id.
let readOnlyToken = null;
// The revision the read-only view loaded; the shared render route checks it the
// way the design route checks `workspaceRevision`.
let readOnlyRevision = 0;
// Uploaded STL meshes of the current design (`import("name.stl")`). Declared
// here for the same temporal-dead-zone reason: resetAssets runs on every
// workspace switch, including the first one at boot.
// The Revisions panel: the rows on screen, what they were loaded for, and the
// token that retires an answer for a design or revision no longer shown.
let revisionRows = [];
let revisionsLoadToken = 0;
let revisionsTimer = null;
let revisionRestoring = false;
let designAssets = [];
let assetsVersion = null;
let assetsLoadToken = 0;
// The upload in flight (see newAssetUploadOp): one at a time; Cancel or a design
// switch cancels it, including during a retry backoff.
let assetUploadOp = null;
let assetConfirmResolve = null;
// What the STL row shows (see assetRowView) and the timer that counts it down.
let assetRowShown = null;
let assetCountdownTimer = null;
// Server clock minus this one, from the asset list: expiry is the server's to decide.
let assetClockOffsetMs = 0;
// Compare parts: the two parts chosen, and the run in flight or its error. Up here for the
// same reason: the part list and a workspace switch reach them at boot.
const comparePicks = { a: '', b: '' };
// The comparison on its way, so that changing a pick can abort it.
let compareController = null;
let compareRun = 0;
let compareRunning = false;
let compareError = '';
let workspaceGeneration = 0;
// One controller per workspace generation: every load-time request (open, shared
// view, refresh) rides its signal, and leaving the workspace aborts them all, so
// a slow answer for the workspace being left can never land in the next one.
let workspaceLoadController = null;
// The source the current object list was compiled from. Object ids are only
// meaningful for that source: once the editor moves on, a per-object export
// has to recompile first or it would name parts that no longer exist.
let sceneObjectsSource = null;
let workspaceSavedCode = '';
let workspaceHasLocalChanges = false;
// Unsaved browser edits that a newer server revision replaced. The server always
// wins the editor — that is what keeps an AI agent's MCP edits from being shown
// stale — but the edits it displaced are kept here (and in localStorage) until
// the user restores or discards them, so nothing typed is ever lost silently.
let displacedEdits = null;
// Bumped whenever the event-poll loop is (re)started, so at most one loop is
// alive. Without it every visibility change spawned another loop alongside the
// one still waiting on its long poll.
let pollLoopToken = 0;
let applyingRemoteUpdate = false;
// Set when the source on screen was replaced from outside (an agent, another tab,
// a conflict) and the geometry still belongs to the code that is gone. A render
// that then fails clears the old model instead of leaving it up as if current.
let geometryStale = false;
let aiUpdatedRanges = [];
let sceneObjects = [];
let selectedObjectId = null;
let intersectionIds = new Set();
// Comments pinned to the model for the agent (see "Model feedback" below).
let feedbackVersion = 0;
let feedbackComments = [];
let feedbackRequest = null;
let feedbackEndedRequestId = null;
let commentMode = false;
let feedbackTab = 'open';
let activeCommentId = null;
let pendingComment = null;
let feedbackLoadToken = 0;
let feedbackSaving = null;
let feedbackFinishing = false;
let feedbackLastSeq = 0;
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

// Section view. A cut is an axis, a position along it in world millimetres and which side
// survives: unflipped keeps everything at or below the position (x <= p), flipped keeps
// everything at or above it. The helpers are pure so the clamping and plane maths are
// testable without a GL context.
const SLICE_AXES = ['x', 'y', 'z'];

// The slider's span along one axis: exactly the model bounds, with a step fine enough for
// ~200 stops but never below a micron. Null when there is nothing to cut.
function sliceRange(bounds, axis) {
  const index = SLICE_AXES.indexOf(axis);
  if (!bounds || index < 0) return null;
  const min = bounds.min[index];
  const max = bounds.max[index];
  if (!Number.isFinite(min) || !Number.isFinite(max) || max < min) return null;
  const span = max - min;
  const step = span > 0 ? Math.max(0.001, 10 ** Math.floor(Math.log10(span / 200))) : 0.001;
  return { min, max, step };
}

// A recompile can shrink or move the model; the cut stays where it was when it still lies
// inside the new bounds and is pulled onto the nearest face when it does not. A position
// that was never set starts mid-model, where a section shows the most.
function clampSlicePosition(position, range) {
  if (!range) return Number.isFinite(position) ? position : 0;
  if (!Number.isFinite(position)) return (range.min + range.max) / 2;
  return Math.min(range.max, Math.max(range.min, position));
}

// The clip plane as [nx, ny, nz, d]: a fragment at p is discarded when dot(n, p) > d, so the
// normal points into the removed half and the cap faces whoever looks at the cut.
function slicePlane(axis, position, flip) {
  const sign = flip ? -1 : 1;
  const plane = [0, 0, 0, sign * position];
  plane[SLICE_AXES.indexOf(axis)] = sign;
  return plane;
}

// Four corners of the cut rectangle: the bounds' footprint on the plane, padded so the cap
// quad always covers every pixel of the section and the outline reads as a frame around it.
// Whether some edge of a triangle soup (9 floats per triangle) has a single
// face: an open surface, such as a one-sided scan.
// Pure: the line segments of what looks like an edge, from unwelded triangle positions: a
// crease of at least FEATURE_EDGE_MIN_DEGREES, a boundary, or an edge more than two faces
// share. A flat face's triangulation draws nothing, nor do the facets of a smooth curve.
const FEATURE_EDGE_MIN_DEGREES = 20;
function featureEdgeLines(positions) {
  const corners = positions.length / 3;
  const triangles = corners / 3;
  // Equal floats are one vertex; `+ 0` makes -0 and 0 the same bits.
  const welded = Float32Array.from(positions, (value) => value + 0);
  const bits = new Uint32Array(welded.buffer);
  const slots = new Int32Array(2 ** Math.ceil(Math.log2(corners * 2 + 2))).fill(-1);
  const vertex = new Int32Array(corners);
  for (let corner = 0; corner < corners; corner += 1) {
    const base = corner * 3;
    let slot = (Math.imul(bits[base], 73856093) ^ Math.imul(bits[base + 1], 19349663) ^ Math.imul(bits[base + 2], 83492791)) & (slots.length - 1);
    for (; ; slot = (slot + 1) & (slots.length - 1)) {
      const other = slots[slot] * 3;
      if (other < 0) {
        slots[slot] = corner;
        vertex[corner] = corner;
        break;
      }
      if (bits[other] === bits[base] && bits[other + 1] === bits[base + 1] && bits[other + 2] === bits[base + 2]) {
        vertex[corner] = other / 3;
        break;
      }
    }
  }
  const normals = new Float32Array(triangles * 3);
  const minCos = Math.cos(FEATURE_EDGE_MIN_DEGREES * Math.PI / 180);
  const FLAT = -1;
  const DRAWN = -2;
  // Per edge: the one face seen so far, or the verdict once there are two.
  const edges = new Map();
  for (let triangle = 0; triangle < triangles; triangle += 1) {
    const base = triangle * 9;
    const ux = welded[base + 3] - welded[base];
    const uy = welded[base + 4] - welded[base + 1];
    const uz = welded[base + 5] - welded[base + 2];
    const vx = welded[base + 6] - welded[base];
    const vy = welded[base + 7] - welded[base + 1];
    const vz = welded[base + 8] - welded[base + 2];
    const nx = uy * vz - uz * vy;
    const ny = uz * vx - ux * vz;
    const nz = ux * vy - uy * vx;
    const length = Math.hypot(nx, ny, nz);
    if (!length) continue;
    normals[triangle * 3] = nx / length;
    normals[triangle * 3 + 1] = ny / length;
    normals[triangle * 3 + 2] = nz / length;
    for (let side = 0; side < 3; side += 1) {
      const a = vertex[triangle * 3 + side];
      const b = vertex[triangle * 3 + (side + 1) % 3];
      if (a === b) continue;
      const key = Math.min(a, b) * corners + Math.max(a, b);
      const seen = edges.get(key);
      if (seen === undefined) {
        edges.set(key, triangle);
      } else if (seen >= 0) {
        const cos = normals[seen * 3] * normals[triangle * 3] + normals[seen * 3 + 1] * normals[triangle * 3 + 1] + normals[seen * 3 + 2] * normals[triangle * 3 + 2];
        edges.set(key, cos > minCos ? FLAT : DRAWN);
      } else {
        edges.set(key, DRAWN);
      }
    }
  }
  const lines = [];
  for (const [key, verdict] of edges) {
    if (verdict === FLAT) continue;
    const a = Math.floor(key / corners) * 3;
    const b = (key % corners) * 3;
    lines.push(welded[a], welded[a + 1], welded[a + 2], welded[b], welded[b + 1], welded[b + 2]);
  }
  return new Float32Array(lines);
}

function hasOpenEdges(positions, triangles) {
  if (!positions) return false;
  const bits = new Uint32Array(positions.buffer, positions.byteOffset, triangles * 9);
  const ids = new Map();
  const id = (o) => {
    const key = `${bits[o]},${bits[o + 1]},${bits[o + 2]}`;
    let value = ids.get(key);
    if (value === undefined) { value = ids.size; ids.set(key, value); }
    return value;
  };
  const edges = new Map();
  for (let triangle = 0; triangle < triangles; triangle += 1) {
    const corners = [id(triangle * 9), id(triangle * 9 + 3), id(triangle * 9 + 6)];
    for (let slot = 0; slot < 3; slot += 1) {
      const a = corners[slot];
      const b = corners[(slot + 1) % 3];
      const key = a < b ? a * 0x100000000 + b : b * 0x100000000 + a;
      edges.set(key, (edges.get(key) || 0) + 1);
    }
  }
  for (const count of edges.values()) if (count === 1) return true;
  return false;
}

function sliceRectangle(bounds, axis, position, padding = 0) {
  const index = SLICE_AXES.indexOf(axis);
  if (!bounds || index < 0) return null;
  const [u, v] = [0, 1, 2].filter((other) => other !== index);
  const corner = (a, b) => {
    const point = [0, 0, 0];
    point[index] = position;
    point[u] = a;
    point[v] = b;
    return point;
  };
  const [u0, u1] = [bounds.min[u] - padding, bounds.max[u] + padding];
  const [v0, v1] = [bounds.min[v] - padding, bounds.max[v] + padding];
  return [corner(u0, v0), corner(u1, v0), corner(u1, v1), corner(u0, v1)];
}

// The cap is the part's own colour pushed toward a warm off-white, so a cut face reads as
// material of that part yet never blends into an outside wall of the same colour.
function sliceCapColor(color) {
  return color.map((channel, index) => channel * 0.72 + [1, 0.86, 0.66][index] * 0.28);
}

// Print orientation travels in the source as `// @print <object> rotate=[x, y, z]` or
// `// @print <object> face=-z|+x|...|[nx, ny, nz]` comment lines (OpenSCAD ignores them;
// the server's get_print_layout reads the same syntax). Pure: code -> directives.
const PRINT_DIRECTIVE_PATTERN = /^[ \t]*\/\/[ \t]*@print[ \t]+("([^"]*)"|\S+)[ \t]+(rotate|face)[ \t]*=[ \t]*(\[[^\]]*\]|[+-]?[xyzXYZ])[ \t\r]*$/;

// Pure: per line, whether it starts as code (outside a string literal or a /* */ comment;
// OpenSCAD strings may span lines). A `// @print` inside either is text.
function linesStartingInCode(code) {
  const starts = [true];
  let state = 'code';
  for (let i = 0; i < code.length; i += 1) {
    const char = code[i];
    const next = code[i + 1];
    if (state === 'code') {
      if (char === '"') state = 'text';
      else if (char === '/' && next === '*') { state = 'block'; i += 1; }
      else if (char === '/' && next === '/') state = 'line';
    } else if (state === 'text') {
      if (char === '\\') i += 1;
      else if (char === '"') state = 'code';
    } else if (state === 'block' && char === '*' && next === '/') {
      state = 'code';
      i += 1;
    }
    if (code[i] === '\n') {
      if (state === 'line') state = 'code';
      starts.push(state === 'code');
    }
  }
  return starts;
}

function parsePrintDirectives(code) {
  const directives = [];
  const source = String(code ?? '');
  const inCode = linesStartingInCode(source);
  source.split('\n').forEach((line, index) => {
    if (!inCode[index]) return;
    const match = PRINT_DIRECTIVE_PATTERN.exec(line);
    if (!match) return;
    // The server ignores a vector that is not three finite numbers, or a zero face normal.
    if (match[4].startsWith('[')) {
      const numbers = match[4].slice(1, -1).split(',').map((part) => (part.trim() === '' ? NaN : Number(part)));
      if (numbers.length !== 3 || !numbers.every(Number.isFinite)) return;
      if (match[3] === 'face' && numbers.every((value) => value === 0)) return;
    }
    directives.push({ line: index + 1, object: match[2] ?? match[1], key: match[3], value: match[4].replace(/\s+/g, ' ') });
  });
  return directives;
}

// The directive for one object: by id first, then by name (case-insensitive), as the server matches.
function printDirectiveFor(directives, object) {
  if (!object) return null;
  const name = String(object.name ?? '').toLowerCase();
  return directives.find((directive) => directive.object === object.id)
    || directives.find((directive) => directive.object.toLowerCase() === name)
    || null;
}

// Short text for the objects panel: "+x face down" or "rotated [180, 0, 0]".
function printDirectiveLabel(directive) {
  if (!directive) return '';
  if (directive.key === 'face') {
    const face = /^[xyz]$/i.test(directive.value) ? `+${directive.value}` : directive.value;
    return `${face.toLowerCase()} face down`;
  }
  return `rotated ${directive.value}`;
}

// OpenSCAD's preview colours: the default material is its yellow and `%`
// background geometry is a translucent grey. A part flagged by the intersection
// check keeps its own colour — repainting it red hid the very colour() the user
// set and made two flagged parts indistinguishable — and is tinted red at its
// silhouette instead, with a marker drawn where the problem actually is.
const DEFAULT_MESH_COLOR = [0.98, 0.77, 0.19];
const BACKGROUND_MESH_COLOR = [0.72, 0.74, 0.78];
const SELECTION_HIGHLIGHT = [0.62, 0.88, 1.0, 0.55];
// The colour of an agent's surfaces, and the colour roles of its overlay items.
// Violet is the agent's own colour throughout: nothing else in the viewer uses it.
const AGENT_MESH_COLOR = [0.7, 0.58, 0.98];
const AGENT_TONES = { agent: '#b794ff', bad: '#ff5b4f', warn: '#f3a23c', good: '#5fd38d', info: '#59b8e6' };
const INTERSECTION_HIGHLIGHT = [1.0, 0.16, 0.1, 0.9];

// `faceColor` is one entry of an object's per-triangle palette (`faceColors`): an
// `[r, g, b, a]`, or null for the default material. Left out, the object's own
// `color` applies.
function meshMaterial(object, id, faceColor) {
  let color = DEFAULT_MESH_COLOR;
  let alpha = 1;
  if (object?.background) {
    color = BACKGROUND_MESH_COLOR;
    alpha = 0.32;
  }
  const stated = faceColor === undefined ? object?.color : faceColor;
  const own = Array.isArray(stated) ? stated.map(Number) : null;
  if (own && own.length >= 3 && own.slice(0, 3).every(Number.isFinite)) {
    color = own.slice(0, 3).map((channel) => Math.min(1, Math.max(0, channel)));
    if (Number.isFinite(own[3])) alpha = Math.min(1, Math.max(0.05, own[3]));
  }
  if (intersectionIds.has(id)) return { color, alpha, highlight: INTERSECTION_HIGHLIGHT };
  // Only a scene with more than one part has a selection worth showing.
  const selected = id === selectedObjectId && sceneObjects.length > 1;
  return { color, alpha, highlight: selected ? SELECTION_HIGHLIGHT : null };
}

// Inverse of a column-major 4x4 matrix, or null when it is singular.
function invertMatrix4(m) {
  const [a00, a01, a02, a03, a10, a11, a12, a13, a20, a21, a22, a23, a30, a31, a32, a33] = m;
  const b00 = a00 * a11 - a01 * a10; const b01 = a00 * a12 - a02 * a10; const b02 = a00 * a13 - a03 * a10;
  const b03 = a01 * a12 - a02 * a11; const b04 = a01 * a13 - a03 * a11; const b05 = a02 * a13 - a03 * a12;
  const b06 = a20 * a31 - a21 * a30; const b07 = a20 * a32 - a22 * a30; const b08 = a20 * a33 - a23 * a30;
  const b09 = a21 * a32 - a22 * a31; const b10 = a21 * a33 - a23 * a31; const b11 = a22 * a33 - a23 * a32;
  const det = b00 * b11 - b01 * b10 + b02 * b09 + b03 * b08 - b04 * b07 + b05 * b06;
  if (!det || !Number.isFinite(det)) return null;
  const d = 1 / det;
  return [
    (a11 * b11 - a12 * b10 + a13 * b09) * d, (a02 * b10 - a01 * b11 - a03 * b09) * d, (a31 * b05 - a32 * b04 + a33 * b03) * d, (a22 * b04 - a21 * b05 - a23 * b03) * d,
    (a12 * b08 - a10 * b11 - a13 * b07) * d, (a00 * b11 - a02 * b08 + a03 * b07) * d, (a32 * b02 - a30 * b05 - a33 * b01) * d, (a20 * b05 - a22 * b02 + a23 * b01) * d,
    (a10 * b10 - a11 * b08 + a13 * b06) * d, (a01 * b08 - a00 * b10 - a03 * b06) * d, (a30 * b04 - a31 * b02 + a33 * b00) * d, (a21 * b02 - a20 * b04 - a23 * b00) * d,
    (a11 * b07 - a10 * b09 - a12 * b06) * d, (a00 * b09 - a01 * b07 + a02 * b06) * d, (a31 * b01 - a30 * b03 - a32 * b00) * d, (a20 * b03 - a21 * b01 + a22 * b00) * d,
  ];
}

// The world-space ray through normalised device coordinates (x, y) for a view-projection
// matrix: origin on the near plane, unit direction toward the far plane.
function viewRay(mvp, x, y) {
  const inverse = invertMatrix4(mvp);
  if (!inverse) return null;
  const unproject = (z) => {
    const v = [0, 1, 2, 3].map((row) => inverse[row] * x + inverse[4 + row] * y + inverse[8 + row] * z + inverse[12 + row]);
    return v[3] ? [v[0] / v[3], v[1] / v[3], v[2] / v[3]] : null;
  };
  const near = unproject(-1);
  const far = unproject(1);
  if (!near || !far) return null;
  const direction = far.map((value, axis) => value - near[axis]);
  const length = Math.hypot(...direction);
  if (!length) return null;
  return { origin: near, direction: direction.map((value) => value / length) };
}

// Slab test: does the ray pass through the axis-aligned box at all?
function rayHitsBox(ray, min, max) {
  let low = -Infinity;
  let high = Infinity;
  for (let axis = 0; axis < 3; axis += 1) {
    const origin = ray.origin[axis];
    const direction = ray.direction[axis];
    if (Math.abs(direction) < 1e-12) {
      if (origin < min[axis] || origin > max[axis]) return false;
      continue;
    }
    let t0 = (min[axis] - origin) / direction;
    let t1 = (max[axis] - origin) / direction;
    if (t0 > t1) [t0, t1] = [t1, t0];
    low = Math.max(low, t0);
    high = Math.min(high, t1);
    if (low > high) return false;
  }
  return high >= 0;
}

// Möller–Trumbore, both faces: the distance along the ray to triangle (a, b, c), or null.
function rayTriangle(origin, direction, ax, ay, az, bx, by, bz, cx, cy, cz) {
  const e1 = [bx - ax, by - ay, bz - az];
  const e2 = [cx - ax, cy - ay, cz - az];
  const p = [direction[1] * e2[2] - direction[2] * e2[1], direction[2] * e2[0] - direction[0] * e2[2], direction[0] * e2[1] - direction[1] * e2[0]];
  const det = e1[0] * p[0] + e1[1] * p[1] + e1[2] * p[2];
  if (Math.abs(det) < 1e-12) return null;
  const inverse = 1 / det;
  const s = [origin[0] - ax, origin[1] - ay, origin[2] - az];
  const u = (s[0] * p[0] + s[1] * p[1] + s[2] * p[2]) * inverse;
  if (u < 0 || u > 1) return null;
  const q = [s[1] * e1[2] - s[2] * e1[1], s[2] * e1[0] - s[0] * e1[2], s[0] * e1[1] - s[1] * e1[0]];
  const v = (direction[0] * q[0] + direction[1] * q[1] + direction[2] * q[2]) * inverse;
  if (v < 0 || u + v > 1) return null;
  const t = (e2[0] * q[0] + e2[1] * q[1] + e2[2] * q[2]) * inverse;
  return t > 1e-9 ? t : null;
}

class MeshViewport {
  constructor(canvas, labelCanvas = null, scaleBadge = null) {
    this.canvas = canvas;
    this.labelCanvas = labelCanvas;
    this.labelContext = labelCanvas ? labelCanvas.getContext('2d') : null;
    this.scaleBadge = scaleBadge;
    this.scaleValue = scaleBadge ? scaleBadge.querySelector('b') : null;
    this.groundBuffer = null;
    this.groundGeometry = null;
    this.groundLabels = [];
    this.bounds = null;
    this.eye = [0, 0, 1];
    // The stencil buffer is what caps a section cut (see drawSliceCaps); without one the
    // cut still works, it just shows the part hollow.
    this.gl = canvas.getContext('webgl', { antialias: true, alpha: true, preserveDrawingBuffer: true, stencil: true });
    this.center = [0, 0, 0];
    // Section view: { enabled, axis, position (world mm), flip }. Survives setMesh — only
    // the position is clamped to the new bounds — so a recompile never drops the cut.
    this.slice = { enabled: false, axis: 'z', position: NaN, flip: false };
    this.sliceBuffer = null;
    // Called after the model bounds change, so the section controls can re-range.
    this.onBoundsChange = null;
    this.radius = 50;
    // The model radius the orthographic scale was last set against. Kept apart from
    // radius so a recompile that grows or shrinks the model leaves the ortho zoom alone;
    // only a zoom, a fit or a design's first frame moves it (syncOrthoScale).
    this.orthoRadius = 50;
    this.showEdges = false;
    // Intersection findings, drawn as screen-space overlays on top of the model
    // (deliberately not depth tested: the place two parts collide is usually
    // inside one of them). `activeMarker` is the one picked in the results list.
    this.markers = [];
    this.activeMarker = -1;
    // What an AI agent's last tool call found, drawn like the markers (see the
    // "Agent activity" section): overlay items in design coordinates, coloured
    // surfaces it measured (scan features, deviation heatmaps).
    // Overlay layers, one per agent call: [{ items, fade }], oldest first.
    this.agentOverlay = [];
    this.agentMeshes = [];
    // 1 while shown, falling to 0 as they fade out (see agentFadeLevel): the
    // intersection markers an agent's check put there.
    this.markerFade = 1;
    this.meshBatches = [];
    // Whether the current design has been auto-framed yet. Only its first mesh is;
    // every later render (an edit, an agent's update) keeps the user's camera.
    // Cleared by resetScene when a different design is opened.
    this.framed = false;
    this.axesBuffer = null;
    if (!this.gl) throw new Error('WebGL is not available in this browser.');
    this.program = this.createProgram();
    this.locations = {
      position: this.gl.getAttribLocation(this.program, 'aPosition'),
      normal: this.gl.getAttribLocation(this.program, 'aNormal'),
      mvp: this.gl.getUniformLocation(this.program, 'uMvp'),
      color: this.gl.getUniformLocation(this.program, 'uColor'),
      lighting: this.gl.getUniformLocation(this.program, 'uLighting'),
      alpha: this.gl.getUniformLocation(this.program, 'uAlpha'),
      highlight: this.gl.getUniformLocation(this.program, 'uHighlight'),
      eye: this.gl.getUniformLocation(this.program, 'uEye'),
      clip: this.gl.getUniformLocation(this.program, 'uClip'),
      clipOn: this.gl.getUniformLocation(this.program, 'uClipOn'),
      backTint: this.gl.getUniformLocation(this.program, 'uBackTint'),
    };
    this.hasStencil = Boolean(this.gl.getContextAttributes()?.stencil);
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
      varying vec3 vPosition;
      void main() {
        gl_Position = uMvp * vec4(aPosition, 1.0);
        vNormal = aNormal;
        vPosition = aPosition;
      }`;
    // uHighlight tints toward a colour (rgb) by an amount (a), almost entirely at
    // grazing angles: a rim glow marks the selection while the faces turned to
    // the viewer keep the object's own color(). The falloff is steep on purpose —
    // with flat normals a gentle one washed whole side faces out to pale blue.
    const fragment = `
      #ifdef GL_FRAGMENT_PRECISION_HIGH
      precision highp float;
      #else
      precision mediump float;
      #endif
      varying vec3 vNormal;
      varying vec3 vPosition;
      uniform vec3 uColor;
      uniform float uLighting;
      uniform float uAlpha;
      uniform vec4 uHighlight;
      uniform vec3 uEye;
      uniform vec4 uClip;
      uniform float uClipOn;
      uniform float uBackTint;
      void main() {
        if (uClipOn > 0.5 && dot(vPosition, uClip.xyz) > uClip.w) discard;
        vec3 n = normalize(vNormal);
        // The back of a face (an open surface seen from behind, or the inside
        // of a cut): lit from its own side, tinted the server renderer's red.
        bool back = uBackTint > 0.5 && uLighting > 0.5 && !gl_FrontFacing;
        if (back) n = -n;
        // Seen from below, the lights sit behind a back face: light it as if
        // they shone through, or the underside of a scan goes black.
        float key = dot(n, normalize(vec3(0.45, -0.35, 0.82)));
        float fill = dot(n, normalize(vec3(-0.62, 0.3, 0.35)));
        key = back ? abs(key) : max(key, 0.0);
        fill = (back ? abs(fill) : max(fill, 0.0)) * 0.22;
        float shade = mix(1.0, 0.28 + key * 0.66 + fill, uLighting);
        vec3 base = (back ? mix(uColor * 0.7, vec3(0.55, 0.06, 0.05), 0.45) : uColor) * shade;
        float rim = 0.0;
        if (uHighlight.a > 0.0) {
          rim = pow(1.0 - abs(dot(n, normalize(uEye - vPosition))), 3.0);
        }
        gl_FragColor = vec4(mix(base, uHighlight.rgb, uHighlight.a * (0.1 + 0.9 * rim)), uAlpha);
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
    // Base64 from the render routes, or raw bytes fetched as a file.
    let bytes;
    if (typeof encoded === 'string') {
      const binary = atob(encoded);
      bytes = new Uint8Array(binary.length);
      for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
    } else {
      bytes = new Uint8Array(encoded);
    }
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    if (bytes.length < 84) throw new Error('The native compiler returned an invalid STL file.');
    const triangles = view.getUint32(80, true);
    if (triangles === 0) throw new Error('The model contains no 3D geometry.');
    if (84 + triangles * 50 > bytes.length) throw new Error('The native compiler returned a truncated STL file.');

    const positions = new Float32Array(triangles * 9);
    const normals = new Float32Array(triangles * 9);
    const min = [Infinity, Infinity, Infinity];
    const max = [-Infinity, -Infinity, -Infinity];
    for (let triangle = 0; triangle < triangles; triangle += 1) {
      const offset = 84 + triangle * 50;
      const normal = [view.getFloat32(offset, true), view.getFloat32(offset + 4, true), view.getFloat32(offset + 8, true)];
      for (let vertex = 0; vertex < 3; vertex += 1) {
        const vertexOffset = offset + 12 + vertex * 12;
        const point = [view.getFloat32(vertexOffset, true), view.getFloat32(vertexOffset + 4, true), view.getFloat32(vertexOffset + 8, true)];
        const base = triangle * 9 + vertex * 3;
        positions.set(point, base);
        normals.set(normal, base);
        for (let axis = 0; axis < 3; axis += 1) {
          min[axis] = Math.min(min[axis], point[axis]);
          max[axis] = Math.max(max[axis], point[axis]);
        }
      }
    }
    return { positions, normals, min, max, triangles };
  }

  // An object painted in several colours arrives with `faceColors`: a palette and
  // `[first, count, slot]` triangle runs over its STL. Each run becomes its own draw
  // call with its own material; a one-colour object stays one draw.
  static colorRuns(faceColors, triangles) {
    const palette = Array.isArray(faceColors?.palette) ? faceColors.palette : null;
    const runs = Array.isArray(faceColors?.runs) ? faceColors.runs : null;
    if (!palette || !runs || !runs.length) return null;
    const valid = runs
      .map(([first, count, slot]) => ({ first: Number(first), count: Number(count), color: palette[slot] ?? null }))
      .filter(({ first, count }) => Number.isInteger(first) && Number.isInteger(count) && first >= 0 && count > 0 && first + count <= triangles);
    return valid.length ? valid : null;
  }

  createMeshBatch(id, encoded, faceColors = null) {
    const decoded = this.decodeStl(encoded);
    const gl = this.gl;
    const positionBuffer = gl.createBuffer();
    const normalBuffer = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, positionBuffer);
    gl.bufferData(gl.ARRAY_BUFFER, decoded.positions, gl.STATIC_DRAW);
    gl.bindBuffer(gl.ARRAY_BUFFER, normalBuffer);
    gl.bufferData(gl.ARRAY_BUFFER, decoded.normals, gl.STATIC_DRAW);
    const runs = MeshViewport.colorRuns(faceColors, decoded.triangles);
    // `positions` stays on the CPU for picking (comment pins), see pick().
    return { id, positionBuffer, normalBuffer, edgeBuffer: null, edgeVertices: 0, positions: decoded.positions, triangles: decoded.triangles, min: decoded.min, max: decoded.max, runs };
  }

  releaseBatches(batches) {
    for (const batch of batches || []) {
      this.gl.deleteBuffer(batch.positionBuffer);
      this.gl.deleteBuffer(batch.normalBuffer);
      this.gl.deleteBuffer(batch.edgeBuffer);
    }
  }

  // Coloured surfaces from an agent's call: [{ bytes, palette, runs }].
  setAgentMeshes(meshes) {
    this.clearAgentMeshes(false);
    this.agentMeshes = meshes.map((mesh, index) => this.createMeshBatch(`agent-mesh-${index}`, mesh.bytes, { palette: mesh.palette, runs: mesh.runs }));
    this.draw();
  }

  clearAgentMeshes(redraw = true) {
    this.releaseBatches(this.agentMeshes);
    this.agentMeshes = [];
    if (redraw) this.draw();
  }

  // Drops every buffer and leaves the canvas empty.
  //
  // `hasMesh = false` alone only stops the *next* draw from being requested;
  // the last frame stays on the canvas, so starting a new workspace left the
  // previous model sitting behind the next workspace's empty editor as though it were still
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
    this.onBoundsChange?.();
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
      ? objects.map((object) => this.createMeshBatch(object.id, object.mesh, object.faceColors))
      : [this.createMeshBatch(objects[0]?.id || 'model', encoded)];
    const min = [Infinity, Infinity, Infinity];
    const max = [-Infinity, -Infinity, -Infinity];
    for (const batch of this.meshBatches) {
      for (let axis = 0; axis < 3; axis += 1) {
        min[axis] = Math.min(min[axis], batch.min[axis]);
        max[axis] = Math.max(max[axis], batch.max[axis]);
      }
    }
    // The pivot is stored relative to the model centre, so it is captured in world
    // space before the centre moves and restored after: a re-render that grows the
    // model must not drag the view along with its new bounding box.
    const pivot = this.pivot();
    this.bounds = { min, max };
    this.center = min.map((value, axis) => (value + max[axis]) / 2);
    this.radius = Math.max(1, Math.hypot(...max.map((value, axis) => (value - min[axis]) / 2)));
    if (this.framed) {
      for (let axis = 0; axis < 3; axis += 1) camera[axis] = pivot[axis] - this.center[axis];
    } else {
      // A design's first mesh frames: the distance is refitted, so any pan offset goes
      // with it, or the fitted view would be aimed at empty space beside the model.
      camera[0] = camera[1] = camera[2] = 0;
      this.fitView();
      this.framed = true;
    }
    this.slice.position = clampSlicePosition(this.slice.position, sliceRange(this.bounds, this.slice.axis));
    this.onBoundsChange?.();
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
    return Math.max(this.orthoRadius * 0.007, camera[6] * 0.36);
  }

  syncOrthoScale() {
    this.orthoRadius = this.radius;
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
  // Opaque and untinted unless a mesh pass says otherwise; every pass that
  // changes these puts them back, because the line passes share the program.
  setMaterial(color, alpha = 1, highlight = null) {
    const gl = this.gl;
    gl.uniform3f(this.locations.color, color[0], color[1], color[2]);
    gl.uniform1f(this.locations.alpha, alpha);
    if (highlight) gl.uniform4f(this.locations.highlight, highlight[0], highlight[1], highlight[2], highlight[3]);
    else gl.uniform4f(this.locations.highlight, 0, 0, 0, 0);
  }

  beginLinePass(mvp) {
    const gl = this.gl;
    gl.useProgram(this.program);
    gl.uniformMatrix4fv(this.locations.mvp, false, mvp);
    gl.uniform1f(this.locations.lighting, 0);
    gl.uniform1f(this.locations.alpha, 1);
    gl.uniform4f(this.locations.highlight, 0, 0, 0, 0);
    gl.uniform1f(this.locations.clipOn, 0);
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
  // the part. Deliberately no pan term. The surface and its origin lines
  // are all anchored to the world origin, so following a pan would mean sliding the grid
  // while those stay put; and feeding the pan into the span would re-pick the 1-2-5 step
  // mid-drag, flickering the divisions. A far pan therefore runs off the edge of a finite
  // grid, exactly as it does in a slicer, and Fit brings it back.
  groundSpan() {
    const footprint = this.bounds
      ? Math.max(...[0, 1].map((axis) => Math.max(Math.abs(this.bounds.max[axis]), Math.abs(this.bounds.min[axis])))) * 2.2
      : 0;
    return Math.max(footprint, (this.radius + Math.hypot(this.center[0], this.center[1])) * 2.2, this.viewSpan() * 1.3, 10);
  }

  // Divisions always land on a 1-2-5 step, so "one square" is a number a person can use.
  groundExtents() {
    const span = this.groundSpan();
    const spacing = niceStep(span / GROUND_TARGET_CELLS);
    // Round the half-count up to a whole major block so the outermost line is always a heavy one.
    const half = span / (2 * spacing);
    const cells = Math.max(GROUND_MAJOR_EVERY * 2, Math.min(GROUND_MAX_CELLS, Math.ceil(half / GROUND_MAJOR_EVERY) * GROUND_MAJOR_EVERY));
    return {
      spacing,
      cellsX: cells,
      cellsY: cells,
      major: GROUND_MAJOR_EVERY,
      extentX: cells * spacing,
      extentY: cells * spacing,
    };
  }

  // Geometry is built once in CELL units and scaled by the current spacing at draw time,
  // so changing the step never reallocates a buffer.
  ensureGroundBuffer(metrics) {
    const key = `${metrics.cellsX}:${metrics.cellsY}`;
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
    this.groundBuffer = this.createLineBuffer(new Float32Array([...minor, ...major, ...origin]));
    const range = (offset, values) => ({ first: offset / 3, count: values.length / 3 });
    this.groundGeometry = {
      key,
      cellsX,
      cellsY,
      buffer: this.groundBuffer,
      minor: range(0, minor),
      major: range(minor.length, major),
      origin: range(minor.length + major.length, origin),
    };
    return this.groundGeometry;
  }

  drawGround(mvp, metrics) {
    const geometry = this.ensureGroundBuffer(metrics);
    this.beginLinePass(this.multiply(mvp, this.scaleMatrix(metrics.spacing)));
    this.drawLineSegments(geometry.buffer, geometry.minor.first, geometry.minor.count, [0.2, 0.231, 0.271]);
    this.drawLineSegments(geometry.buffer, geometry.major.first, geometry.major.count, [0.282, 0.322, 0.373]);
    this.drawLineSegments(geometry.buffer, geometry.origin.first, geometry.origin.count, [0.416, 0.463, 0.514]);
  }

  // Numbers are 2D-canvas text placed through projectPoint(), not billboarded geometry:
  // screen-space text can never end up mirrored or upside down as the camera orbits.
  drawGroundLabels(metrics) {
    const canvas = this.labelCanvas;
    const context = this.labelContext;
    this.groundLabels = [];
    // Written only on change: draw() runs on every orbit frame.
    if (this.scaleBadge?.hidden) this.scaleBadge.hidden = false;
    const readout = `${formatMillimetres(metrics.spacing)} mm`;
    if (this.scaleValue && this.scaleValue.textContent !== readout) this.scaleValue.textContent = readout;
    if (!canvas || !context) return;
    if (canvas.width !== this.canvas.width || canvas.height !== this.canvas.height) {
      canvas.width = this.canvas.width;
      canvas.height = this.canvas.height;
    }
    context.clearRect(0, 0, canvas.width, canvas.height);
    const ratio = Math.max(1, canvas.width / Math.max(1, canvas.clientWidth));
    const step = metrics.spacing * metrics.major;
    // floor(), not a ratio.
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
    // Keep clear of the viewport furniture — the gesture hint, the grid badge,
    // the zoom cluster — measured from the DOM rather than guessed, because each of them
    // moves with the pane width. A blocked number falls back to the far end of its line.
    const margin = { left: 15 * ratio, right: 15 * ratio, top: 15 * ratio, bottom: 15 * ratio };
    const obstacles = this.overlayObstacles(ratio);
    const blocked = (point) => obstacles.some((box) => point.x + 18 * ratio > box.left && point.x - 18 * ratio < box.right
      && point.y + 8 * ratio > box.top && point.y - 8 * ratio < box.bottom);
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
    this.drawMarkers();
    this.drawAgentOverlay();
  }

  // Screen rectangles (in drawing-buffer pixels) of the overlays sitting on the canvas.
  overlayObstacles(ratio) {
    const frame = this.labelCanvas.getBoundingClientRect();
    return ['.view-hint', '#gridScale', '.viewport-fab', '#slicerPanel']
      .map((selector) => document.querySelector(selector))
      .filter((element) => element && !element.hidden && element.offsetParent !== null)
      .map((element) => {
        const box = element.getBoundingClientRect();
        return {
          left: (box.left - frame.left - 4) * ratio,
          right: (box.right - frame.left + 4) * ratio,
          top: (box.top - frame.top - 4) * ratio,
          bottom: (box.bottom - frame.top + 4) * ratio,
        };
      })
      .filter((box) => box.right > box.left && box.bottom > box.top);
  }

  // One marker per intersection finding: the overlap region as a dashed box, the closest
  // points of a too-small gap joined by a line, and a ring at the reported location.
  drawMarkers() {
    const context = this.labelContext;
    if (!context || !this.markers.length) return;
    const canvas = this.labelCanvas;
    const ratio = Math.max(1, canvas.width / Math.max(1, canvas.clientWidth));
    const visible = (id) => sceneObjects.find((object) => object.id === id)?.visible !== false;
    const screen = (point) => {
      const projected = Array.isArray(point) && point.length === 3 ? this.projectPoint(point.map(Number)) : null;
      return projected && !projected.behind ? projected : null;
    };
    this.markers.forEach((marker, index) => {
      if (!visible(marker.a) || !visible(marker.b)) return;
      const active = index === this.activeMarker;
      const color = marker.kind === 'overlap' ? '#ff5b4f' : '#f3a23c';
      context.save();
      context.globalAlpha = (this.activeMarker < 0 || active ? 1 : 0.4) * this.markerFade;
      context.lineWidth = (active ? 2 : 1.5) * ratio;
      context.strokeStyle = color;
      context.fillStyle = color;
      if (marker.bounds) {
        const { min, max } = marker.bounds;
        const corner = (bits) => screen([bits & 1 ? max[0] : min[0], bits & 2 ? max[1] : min[1], bits & 4 ? max[2] : min[2]]);
        const points = Array.from({ length: 8 }, (_, bits) => corner(bits));
        context.setLineDash([4 * ratio, 3 * ratio]);
        context.beginPath();
        for (let bits = 0; bits < 8; bits += 1) {
          for (const flip of [1, 2, 4]) {
            if (bits & flip || !points[bits] || !points[bits | flip]) continue;
            context.moveTo(points[bits].x, points[bits].y);
            context.lineTo(points[bits | flip].x, points[bits | flip].y);
          }
        }
        context.stroke();
        context.setLineDash([]);
      }
      if (marker.closest) {
        const a = screen(marker.closest.a);
        const b = screen(marker.closest.b);
        if (a && b) {
          context.beginPath();
          context.moveTo(a.x, a.y);
          context.lineTo(b.x, b.y);
          context.stroke();
          for (const point of [a, b]) {
            context.beginPath();
            context.arc(point.x, point.y, 2.5 * ratio, 0, Math.PI * 2);
            context.fill();
          }
        }
      }
      const at = screen(marker.location);
      if (at) {
        const radius = (active ? 9 : 7) * ratio;
        context.lineWidth = 3.5 * ratio;
        context.strokeStyle = 'rgba(14, 17, 21, .85)';
        context.beginPath();
        context.arc(at.x, at.y, radius, 0, Math.PI * 2);
        context.stroke();
        context.lineWidth = (active ? 2 : 1.5) * ratio;
        context.strokeStyle = color;
        context.beginPath();
        context.arc(at.x, at.y, radius, 0, Math.PI * 2);
        for (const [x, y] of [[1, 0], [-1, 0], [0, 1], [0, -1]]) {
          context.moveTo(at.x + x * radius * 0.45, at.y + y * radius * 0.45);
          context.lineTo(at.x + x * radius * 1.6, at.y + y * radius * 1.6);
        }
        context.stroke();
        if (active && marker.label) {
          context.font = `${Math.round(10 * ratio)}px "IBM Plex Mono", "SFMono-Regular", Menlo, monospace`;
          context.textAlign = 'left';
          context.textBaseline = 'middle';
          context.lineJoin = 'round';
          context.lineWidth = 3 * ratio;
          context.strokeStyle = 'rgba(14, 17, 21, .9)';
          // To the right of the ring, unless that would run off the canvas.
          const width = context.measureText(marker.label).width;
          const left = at.x + radius * 1.9 + width > canvas.width - 8 * ratio;
          context.textAlign = left ? 'right' : 'left';
          const x = left ? at.x - radius * 1.9 : at.x + radius * 1.9;
          context.strokeText(marker.label, x, at.y);
          context.fillText(marker.label, x, at.y);
        }
      }
      context.restore();
    });
  }

  // An agent's findings: boxes, segments, points, section loops and plane patches in
  // design coordinates, in screen space over the model (not depth tested, like the
  // intersection markers: what an agent measured is often inside a part).
  drawAgentOverlay() {
    const context = this.labelContext;
    if (!context || !this.agentOverlay.some((layer) => layer.items.length)) return;
    const canvas = this.labelCanvas;
    const ratio = Math.max(1, canvas.width / Math.max(1, canvas.clientWidth));
    const screen = (point) => {
      const projected = Array.isArray(point) && point.length === 3 ? this.projectPoint(point.map(Number)) : null;
      return projected && !projected.behind ? projected : null;
    };
    const labels = [];
    context.save();
    context.lineJoin = 'round';
    context.lineCap = 'round';
    // Each layer fades on its own clock: a run of calls stays on the model together.
    for (const { item, fade } of this.agentOverlay.flatMap((layer) => layer.items.map((item) => ({ item, fade: Math.max(0, Math.min(1, layer.fade)) })))) {
      context.globalAlpha = fade;
      const color = AGENT_TONES[item.tone] || AGENT_TONES.agent;
      context.strokeStyle = color;
      context.fillStyle = color;
      context.lineWidth = 1.6 * ratio;
      context.setLineDash(item.dashed ? [4 * ratio, 3 * ratio] : []);
      const path = (points, closed) => {
        const projected = points.map(screen);
        context.beginPath();
        let open = false;
        projected.forEach((point) => {
          if (!point) { open = false; return; }
          if (open) context.lineTo(point.x, point.y); else context.moveTo(point.x, point.y);
          open = true;
        });
        if (closed && projected.every(Boolean)) context.closePath();
        context.stroke();
        return projected.filter(Boolean);
      };
      if (item.kind === 'box' && item.min && item.max) {
        const { min, max } = item;
        const corner = (bits) => screen([bits & 1 ? max[0] : min[0], bits & 2 ? max[1] : min[1], bits & 4 ? max[2] : min[2]]);
        const points = Array.from({ length: 8 }, (_, bits) => corner(bits));
        context.beginPath();
        for (let bits = 0; bits < 8; bits += 1) {
          for (const flip of [1, 2, 4]) {
            if (bits & flip || !points[bits] || !points[bits | flip]) continue;
            context.moveTo(points[bits].x, points[bits].y);
            context.lineTo(points[bits | flip].x, points[bits | flip].y);
          }
        }
        context.stroke();
        if (item.label && points[7]) labels.push({ text: item.label, at: points[7], color, fade });
      } else if (item.kind === 'segment' && item.a && item.b) {
        const [a, b] = path([item.a, item.b], false);
        context.setLineDash([]);
        for (const point of [a, b].filter(Boolean)) {
          context.beginPath();
          context.arc(point.x, point.y, 2.2 * ratio, 0, Math.PI * 2);
          context.fill();
        }
        if (item.label && a && b) labels.push({ text: item.label, at: { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 }, color, fade });
      } else if (item.kind === 'point' && item.at) {
        const at = screen(item.at);
        if (!at) continue;
        context.setLineDash([]);
        context.lineWidth = 3.2 * ratio;
        context.strokeStyle = 'rgba(14, 17, 21, .85)';
        context.beginPath();
        context.arc(at.x, at.y, 5.5 * ratio, 0, Math.PI * 2);
        context.stroke();
        context.lineWidth = 1.6 * ratio;
        context.strokeStyle = color;
        context.stroke();
        context.beginPath();
        context.arc(at.x, at.y, 1.8 * ratio, 0, Math.PI * 2);
        context.fill();
        if (item.label) labels.push({ text: item.label, at: { x: at.x + 8 * ratio, y: at.y }, color, fade });
      } else if (item.kind === 'polyline' && Array.isArray(item.points)) {
        const drawn = path(item.points, item.closed);
        if (item.label && drawn.length) labels.push({ text: item.label, at: drawn[0], color, fade });
      } else if (item.kind === 'plane' && Array.isArray(item.corners)) {
        const corners = item.corners.map(screen);
        if (corners.every(Boolean)) {
          context.globalAlpha = 0.12 * fade;
          context.beginPath();
          corners.forEach((point, index) => (index ? context.lineTo(point.x, point.y) : context.moveTo(point.x, point.y)));
          context.closePath();
          context.fill();
          context.globalAlpha = 0.8 * fade;
          context.setLineDash([5 * ratio, 4 * ratio]);
          context.stroke();
          context.globalAlpha = fade;
          if (item.label) labels.push({ text: item.label, at: corners[2], color, fade });
        }
      }
    }
    // Labels last, haloed, and never more than a screenful.
    context.setLineDash([]);
    context.font = `${Math.round(10 * ratio)}px "IBM Plex Mono", "SFMono-Regular", Menlo, monospace`;
    context.textBaseline = 'middle';
    context.lineWidth = 3 * ratio;
    const placed = [];
    // Newest first, so the latest call's labels win the space.
    for (const label of labels.reverse().slice(0, 40)) {
      if (placed.some((other) => Math.abs(other.x - label.at.x) < 50 * ratio && Math.abs(other.y - label.at.y) < 11 * ratio)) continue;
      context.globalAlpha = label.fade;
      const width = context.measureText(label.text).width;
      const left = label.at.x + width > canvas.width - 6 * ratio;
      context.textAlign = left ? 'right' : 'left';
      context.strokeStyle = 'rgba(14, 17, 21, .9)';
      context.strokeText(label.text, label.at.x, label.at.y);
      context.fillStyle = label.color;
      context.fillText(label.text, label.at.x, label.at.y);
      placed.push(label.at);
    }
    context.restore();
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
    const { width, height, mvp, eye } = this.updateView();
    gl.viewport(0, 0, width, height);
    gl.clearColor(0, 0, 0, 0);
    gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);
    gl.enable(gl.DEPTH_TEST);
    // Opaque parts draw both sides: on a closed solid the depth test hides the
    // backs anyway, and an open surface (a one-sided scan) stays visible from
    // behind. Translucent parts below cull them, so they blend only once.
    gl.disable(gl.CULL_FACE);
    gl.cullFace(gl.BACK);
    gl.useProgram(this.program);
    gl.uniformMatrix4fv(this.locations.mvp, false, mvp);
    gl.enableVertexAttribArray(this.locations.position);
    gl.enableVertexAttribArray(this.locations.normal);
    gl.uniform1f(this.locations.lighting, 1);
    gl.uniform1f(this.locations.backTint, 1);
    gl.uniform3f(this.locations.eye, eye[0], eye[1], eye[2]);
    gl.enable(gl.POLYGON_OFFSET_FILL);
    gl.polygonOffset(1, 1);
    const plane = this.activeSlicePlane();
    this.applyClip(plane);
    // Opaque parts first with depth writes, then translucent ones (color() with
    // alpha, `%` background) blended over them without writing depth, so a
    // see-through part never hides what is behind it.
    // One pass per object, or per colour run of an object painted in several.
    const passes = this.meshBatches
      .flatMap((batch) => {
        const object = sceneObjects.find((item) => item.id === batch.id);
        if (!batch.runs) return [{ batch, object, material: meshMaterial(object, batch.id), first: 0, count: batch.triangles }];
        return batch.runs.map((run) => ({ batch, object, material: meshMaterial(object, batch.id, run.color), first: run.first, count: run.count }));
      })
      .filter(({ object }) => object?.visible !== false);
    const drawPass = ({ batch, material, first, count }) => {
      gl.bindBuffer(gl.ARRAY_BUFFER, batch.positionBuffer);
      gl.vertexAttribPointer(this.locations.position, 3, gl.FLOAT, false, 0, 0);
      gl.bindBuffer(gl.ARRAY_BUFFER, batch.normalBuffer);
      gl.vertexAttribPointer(this.locations.normal, 3, gl.FLOAT, false, 0, 0);
      this.setMaterial(material.color, material.alpha, material.highlight);
      gl.drawArrays(gl.TRIANGLES, first * 3, count * 3);
    };
    passes.filter(({ material }) => material.alpha >= 1).forEach(drawPass);
    const translucent = passes.filter(({ material }) => material.alpha < 1);
    this.setMaterial(DEFAULT_MESH_COLOR);
    gl.disable(gl.POLYGON_OFFSET_FILL);
    // Caps right after the opaque parts and without polygon offset, so a cut face wins over
    // a wall that happens to lie exactly on the plane. Translucent parts are cut uncapped.
    // A cap covers its whole object, so a part in several colours is capped once, in the
    // colour of its first opaque run — and not at all when any of it is translucent, whose
    // cross-section a solid cap would paint over.
    const capped = new Set(passes.filter(({ material }) => material.alpha < 1).map(({ batch }) => batch));
    const capPasses = passes.filter(({ batch, material }) => material.alpha >= 1 && !capped.has(batch) && capped.add(batch));
    if (plane) this.drawSliceCaps(capPasses, plane);
    // An agent's coloured surfaces lie on (or near) the parts they describe; pulled
    // toward the eye so they win over a coincident face instead of z-fighting it.
    if (this.agentMeshes.length) {
      gl.useProgram(this.program);
      gl.uniformMatrix4fv(this.locations.mvp, false, mvp);
      this.applyClip(plane);
      gl.enableVertexAttribArray(this.locations.position);
      gl.enableVertexAttribArray(this.locations.normal);
      gl.uniform1f(this.locations.lighting, 1);
      gl.enable(gl.POLYGON_OFFSET_FILL);
      gl.polygonOffset(-1, -2);
      for (const batch of this.agentMeshes) {
        const runs = batch.runs || [{ first: 0, count: batch.triangles, color: AGENT_MESH_COLOR }];
        for (const run of runs) {
          const color = Array.isArray(run.color) ? run.color.slice(0, 3).map(Number) : AGENT_MESH_COLOR;
          drawPass({ batch, material: { color, alpha: 1, highlight: null }, first: run.first, count: run.count });
        }
      }
      gl.disable(gl.POLYGON_OFFSET_FILL);
      this.setMaterial(DEFAULT_MESH_COLOR);
    }
    // Everything after the parts culls as it always has.
    gl.enable(gl.CULL_FACE);
    if (this.showEdges) {
      gl.disable(gl.CULL_FACE);
      gl.disableVertexAttribArray(this.locations.normal);
      gl.vertexAttrib3f(this.locations.normal, 0, 0, 1);
      this.setMaterial([0.18, 0.16, 0.11]);
      gl.uniform1f(this.locations.lighting, 0);
      for (const batch of this.meshBatches) {
        const object = sceneObjects.find((item) => item.id === batch.id);
        if (object?.visible === false) continue;
        // Built on first use: most models never turn Edges on, and a scan has many triangles.
        if (!batch.edgeBuffer) {
          const lines = featureEdgeLines(batch.positions);
          batch.edgeBuffer = gl.createBuffer();
          batch.edgeVertices = lines.length / 3;
          gl.bindBuffer(gl.ARRAY_BUFFER, batch.edgeBuffer);
          gl.bufferData(gl.ARRAY_BUFFER, lines, gl.STATIC_DRAW);
        }
        gl.bindBuffer(gl.ARRAY_BUFFER, batch.edgeBuffer);
        gl.vertexAttribPointer(this.locations.position, 3, gl.FLOAT, false, 0, 0);
        gl.drawArrays(gl.LINES, 0, batch.edgeVertices);
      }
    }
    // Depth test stays on: the axes and the ground live in the scene, so solid geometry
    // occludes them exactly like a real datum would instead of floating over the model.
    const metrics = this.groundExtents();
    this.drawGround(mvp, metrics);
    this.drawAxes(mvp);
    if (plane) this.drawSliceOutline(mvp);
    // Translucent parts go last, over the ground and axes as well as the opaque
    // parts, so the grid reads *through* them instead of being stamped on top.
    if (translucent.length) {
      gl.useProgram(this.program);
      gl.uniformMatrix4fv(this.locations.mvp, false, mvp);
      gl.uniform1f(this.locations.lighting, 1);
      this.applyClip(plane);
      gl.enableVertexAttribArray(this.locations.position);
      gl.enableVertexAttribArray(this.locations.normal);
      gl.enable(gl.CULL_FACE);
      gl.enable(gl.BLEND);
      // Separate alpha factors keep the canvas premultiplied, so a translucent
      // part over empty background composites correctly (and in PNG export).
      gl.blendFuncSeparate(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA, gl.ONE, gl.ONE_MINUS_SRC_ALPHA);
      gl.depthMask(false);
      translucent.forEach(drawPass);
      gl.depthMask(true);
      gl.disable(gl.BLEND);
      this.setMaterial(DEFAULT_MESH_COLOR);
    }
    this.drawGroundLabels(metrics);
    this.onDraw?.();
  }

  // The surface under a client-space point: casts the view ray through every visible
  // part's triangles (as drawn: hidden parts and the cut-away side of a section are
  // skipped) and returns the nearest hit with its geometric normal, or null.
  pick(clientX, clientY) {
    if (!this.mvp || !this.meshBatches.length) return null;
    const rect = this.canvas.getBoundingClientRect();
    if (!rect.width || !rect.height) return null;
    const ray = viewRay(this.mvp, ((clientX - rect.left) / rect.width) * 2 - 1, 1 - ((clientY - rect.top) / rect.height) * 2);
    if (!ray) return null;
    const plane = this.activeSlicePlane();
    let best = null;
    for (const batch of this.meshBatches) {
      const object = sceneObjects.find((item) => item.id === batch.id);
      if (object?.visible === false || !batch.positions) continue;
      if (!rayHitsBox(ray, batch.min, batch.max)) continue;
      const p = batch.positions;
      for (let triangle = 0; triangle < batch.triangles; triangle += 1) {
        const o = triangle * 9;
        const hit = rayTriangle(ray.origin, ray.direction, p[o], p[o + 1], p[o + 2], p[o + 3], p[o + 4], p[o + 5], p[o + 6], p[o + 7], p[o + 8]);
        if (hit === null || (best && hit >= best.distance)) continue;
        const point = ray.origin.map((value, axis) => value + ray.direction[axis] * hit);
        if (plane && point[0] * plane[0] + point[1] * plane[1] + point[2] * plane[2] > plane[3]) continue;
        best = { distance: hit, point, objectId: batch.id, face: triangle };
      }
    }
    if (!best) return null;
    const p = this.meshBatches.find((batch) => batch.id === best.objectId).positions;
    const o = best.face * 9;
    const u = [p[o + 3] - p[o], p[o + 4] - p[o + 1], p[o + 5] - p[o + 2]];
    const v = [p[o + 6] - p[o], p[o + 7] - p[o + 1], p[o + 8] - p[o + 2]];
    const n = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
    const length = Math.hypot(...n) || 1;
    return { ...best, normal: n.map((value) => value / length) };
  }

  // The clip plane for the current section, or null when the view is uncut.
  activeSlicePlane() {
    if (!this.slice.enabled || !this.bounds || !Number.isFinite(this.slice.position)) return null;
    return slicePlane(this.slice.axis, this.slice.position, this.slice.flip);
  }

  applyClip(plane) {
    const gl = this.gl;
    gl.uniform1f(this.locations.clipOn, plane ? 1 : 0);
    if (plane) gl.uniform4f(this.locations.clip, plane[0], plane[1], plane[2], plane[3]);
  }

  // Solid caps for the cut, one part at a time. Drawing a part's surviving surfaces with
  // depth and colour off and INVERT on the stencil leaves bit 0 set exactly where the
  // ray crosses an odd number of them, i.e. where the plane lies inside that solid; a quad
  // on the plane drawn through that mask is the cross-section. Parity rather than
  // front/back counting, so a mesh with flipped winding still caps correctly.
  drawSliceCaps(passes, plane) {
    const gl = this.gl;
    if (!this.hasStencil || !passes.length) return;
    const { axis, position } = this.slice;
    const index = SLICE_AXES.indexOf(axis);
    const padding = Math.max(1, this.radius * 0.05);
    if (!this.sliceBuffer) this.sliceBuffer = gl.createBuffer();
    gl.disable(gl.CULL_FACE);
    gl.enable(gl.STENCIL_TEST);
    // A cap's flat normal faces the viewer whichever way the quad is wound.
    gl.uniform1f(this.locations.backTint, 0);
    for (const { batch, material } of passes) {
      if (position < batch.min[index] || position > batch.max[index]) continue;
      // Parity says "inside" only for a closed part: behind one open sheet
      // it is odd everywhere, and the cap would paint over all of it.
      batch.open ??= hasOpenEdges(batch.positions, batch.triangles);
      if (batch.open) continue;
      gl.stencilMask(0xff);
      gl.clear(gl.STENCIL_BUFFER_BIT);
      gl.stencilMask(1);
      gl.stencilFunc(gl.ALWAYS, 0, 1);
      gl.stencilOp(gl.KEEP, gl.KEEP, gl.INVERT);
      gl.colorMask(false, false, false, false);
      // Every surviving surface counts, hidden or not: depth always passes and is not written.
      gl.depthMask(false);
      gl.depthFunc(gl.ALWAYS);
      this.applyClip(plane);
      gl.enableVertexAttribArray(this.locations.normal);
      gl.bindBuffer(gl.ARRAY_BUFFER, batch.positionBuffer);
      gl.vertexAttribPointer(this.locations.position, 3, gl.FLOAT, false, 0, 0);
      gl.bindBuffer(gl.ARRAY_BUFFER, batch.normalBuffer);
      gl.vertexAttribPointer(this.locations.normal, 3, gl.FLOAT, false, 0, 0);
      gl.drawArrays(gl.TRIANGLES, 0, batch.triangles * 3);

      gl.colorMask(true, true, true, true);
      gl.depthMask(true);
      gl.depthFunc(gl.LESS);
      gl.stencilFunc(gl.EQUAL, 1, 1);
      gl.stencilOp(gl.KEEP, gl.KEEP, gl.KEEP);
      this.applyClip(null);
      const [a, b, c, d] = sliceRectangle({ min: batch.min, max: batch.max }, axis, position, padding);
      gl.bindBuffer(gl.ARRAY_BUFFER, this.sliceBuffer);
      gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([...a, ...b, ...c, ...a, ...c, ...d]), gl.DYNAMIC_DRAW);
      gl.vertexAttribPointer(this.locations.position, 3, gl.FLOAT, false, 0, 0);
      // The cap faces into the removed half, toward whoever is looking at the cut.
      gl.disableVertexAttribArray(this.locations.normal);
      gl.vertexAttrib3f(this.locations.normal, plane[0], plane[1], plane[2]);
      this.setMaterial(sliceCapColor(material.color), 1, material.highlight);
      gl.drawArrays(gl.TRIANGLES, 0, 6);
    }
    gl.stencilMask(0xff);
    gl.disable(gl.STENCIL_TEST);
    gl.uniform1f(this.locations.backTint, 1);
    gl.enableVertexAttribArray(this.locations.normal);
    gl.enable(gl.CULL_FACE);
    this.setMaterial(DEFAULT_MESH_COLOR);
    this.applyClip(plane);
  }

  // A faint frame where the plane crosses the whole model's bounds, so the cut position
  // reads even where it falls between parts.
  drawSliceOutline(mvp) {
    const gl = this.gl;
    const corners = sliceRectangle(this.bounds, this.slice.axis, this.slice.position, Math.max(1, this.radius * 0.06));
    if (!corners) return;
    if (!this.sliceBuffer) this.sliceBuffer = gl.createBuffer();
    const [a, b, c, d] = corners;
    gl.bindBuffer(gl.ARRAY_BUFFER, this.sliceBuffer);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([...a, ...b, ...b, ...c, ...c, ...d, ...d, ...a]), gl.DYNAMIC_DRAW);
    this.beginLinePass(mvp);
    this.drawLineSegments(this.sliceBuffer, 0, 8, [0.29, 0.5, 0.66]);
  }

  // Sizes the drawing buffer to the canvas and derives the eye and the combined matrix
  // for the current camera. Separate from draw() so framing can measure a view
  // without painting it.
  updateView() {
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
    return { width, height, mvp, eye };
  }

  // Frames the model so its silhouette is centred and fills a fixed share of the frame.
  // Aiming at the bounding-box centre alone left models sitting toward the top right: in
  // perspective the near corners loom larger and a box seen from above projects off its
  // own centre. So the eight corners are projected and the pan and distance corrected
  // from where the pixels actually land; a few passes absorb the perspective drift.
  fitView(fill = 0.78) {
    this.syncOrthoScale();
    camera[0] = camera[1] = camera[2] = 0;
    const reach = this.radius;
    const { bounds } = this;
    camera[6] = Math.max(reach * 4, 20);
    if (!bounds || !this.canvas.clientWidth || !this.canvas.clientHeight) return;
    const { min, max } = bounds;
    const corners = [];
    for (const x of [min[0], max[0]]) for (const y of [min[1], max[1]]) for (const z of [min[2], max[2]]) corners.push([x, y, z]);
    for (let pass = 0; pass < 4; pass += 1) {
      const { width, height } = this.updateView();
      const points = corners.map((corner) => this.projectPoint(corner));
      if (points.some((point) => !point || point.behind)) return;
      const xs = points.map((point) => point.x);
      const ys = points.map((point) => point.y);
      const [x0, x1, y0, y1] = [Math.min(...xs), Math.max(...xs), Math.min(...ys), Math.max(...ys)];
      const [right, up] = this.screenBasis();
      // World millimetres per drawing-buffer pixel in the pivot plane.
      const scale = this.worldPerPixel() * (this.canvas.clientHeight / height);
      const dx = (x0 + x1) / 2 - width / 2;
      const dy = (y0 + y1) / 2 - height / 2;
      for (let axis = 0; axis < 3; axis += 1) camera[axis] += right[axis] * dx * scale - up[axis] * dy * scale;
      const occupancy = Math.max((x1 - x0) / width, (y1 - y0) / height);
      if (occupancy > 0) {
        camera[6] = Math.max(reach * 0.02, Math.min(reach * 30, camera[6] * (occupancy / fill)));
      }
    }
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

const TOKEN_PATTERN = /("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|\/\/[^\n]*|\/\*[\s\S]*?\*\/|\b(?:module|function|for|intersection_for|if|else|let|each|true|false|undef|use|include)\b|\b(?:cube|sphere|cylinder|polyhedron|circle|square|polygon|text|union|difference|intersection|hull|minkowski|sweep_path|translate|rotate|scale|resize|mirror|multmatrix|color|offset|projection|linear_extrude|rotate_extrude|surface|import|children|echo|assert)\b|(?<![\w$])-?\d+(?:\.\d+)?\b)/g;

// The whole source as consecutive spans, each with its token class (or null for
// plain text). Tokenised once over the full text, so a highlight boundary that
// falls inside a comment or a string can never change how that token reads.
function tokenSpans(code) {
  const spans = [];
  let cursor = 0;
  for (const match of code.matchAll(TOKEN_PATTERN)) {
    if (match.index > cursor) spans.push({ start: cursor, end: match.index, type: null });
    const token = match[0];
    let type = 'number';
    if (token.startsWith('//') || token.startsWith('/*')) type = 'comment';
    else if (token.startsWith('"') || token.startsWith("'")) type = 'string';
    else if (/^(module|function|for|intersection_for|if|else|let|each|true|false|undef|use|include)$/.test(token)) type = 'keyword';
    else if (/^[A-Za-z_]/.test(token)) type = 'builtin';
    spans.push({ start: match.index, end: match.index + token.length, type });
    cursor = match.index + token.length;
  }
  if (cursor < code.length) spans.push({ start: cursor, end: code.length, type: null });
  return spans;
}

// Sorted, clamped and merged, so overlapping or touching ranges from one
// revision's several edits read as one change instead of stacked marks.
function mergeRanges(ranges, length) {
  const sorted = ranges
    .map((range) => ({ start: Math.max(0, Number(range.start ?? range.from ?? 0)), end: Math.min(length, Number(range.end ?? range.to ?? 0)) }))
    .filter((range) => Number.isFinite(range.start) && Number.isFinite(range.end) && range.end > range.start)
    .sort((a, b) => a.start - b.start);
  const merged = [];
  for (const range of sorted) {
    const last = merged.at(-1);
    if (last && range.start <= last.end) last.end = Math.max(last.end, range.end);
    else merged.push({ ...range });
  }
  return merged;
}

function syntaxHighlight(code, ranges = aiUpdatedRanges) {
  const marks = mergeRanges(ranges, code.length);
  let html = '';
  let open = false;
  let next = 0;
  for (const span of tokenSpans(code)) {
    let position = span.start;
    while (position < span.end) {
      while (next < marks.length && marks[next].end <= position) next += 1;
      const mark = marks[next];
      const inside = Boolean(mark) && mark.start <= position;
      const stop = Math.min(span.end, mark ? (inside ? mark.end : mark.start) : span.end);
      if (inside !== open) {
        html += inside ? '<mark class="ai-code-change">' : '</mark>';
        open = inside;
      }
      const text = escapeHtml(code.slice(position, stop));
      html += span.type ? `<span class="tok-${span.type}">${text}</span>` : text;
      position = stop;
    }
  }
  if (open) html += '</mark>';
  return `${html}\n`;
}

// The badge sits over the top-right of the editor. While it shows, the text is
// pushed down by one row so a long first line is never hidden underneath it.
function setAiBadge(visible) {
  $('#aiRevisionBadge').hidden = !visible;
  $('.editor-wrap').classList.toggle('ai-revised', visible);
}

function syncEditor(markDirty = true) {
  highlight.innerHTML = syntaxHighlight(editor.value);
  lineNumbers.textContent = Array.from({ length: editor.value.split('\n').length }, (_, i) => i + 1).join('\n');
  if (markDirty) {
    dirtyIndicator.classList.add('visible');
    scheduleWorkspaceSave();
    // There is no render button: an edit is what asks for one.
    scheduleSourceRender(EDIT_RENDER_DELAY_MS);
    // A render is on its way again, so a cancelled model stops reading as out of date.
    showGeometryStaleness();
  }
  updateCursor();
  updateEmptyState();
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
}

// The console follows new output only while it is scrolled to the bottom:
// scrolled up to read something, it stays put. Any growth counts — a new
// line, an agent row finishing, a thumbnail loading, the pane opening.
let consoleFollowing = true;
consoleOutput.addEventListener('scroll', () => {
  consoleFollowing = consoleOutput.scrollHeight - consoleOutput.scrollTop - consoleOutput.clientHeight < 24;
});
function followConsole() {
  if (consoleFollowing) consoleOutput.scrollTop = consoleOutput.scrollHeight;
}
new MutationObserver(followConsole).observe(consoleOutput, { childList: true, subtree: true, characterData: true });
consoleOutput.addEventListener('load', followConsole, true);
new ResizeObserver(followConsole).observe(consoleOutput);

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

function workspaceLoadSignal() {
  workspaceLoadController ??= new AbortController();
  return workspaceLoadController.signal;
}

function advanceWorkspaceGeneration() {
  workspaceGeneration += 1;
  workspaceLoadController?.abort();
  workspaceLoadController = null;
  clearTimeout(eventPollTimer);
  eventPollTimer = null;
  eventPollController?.abort();
  eventPollController = null;
  clearTimeout(workspaceSaveDebounce);
  workspaceSaveDebounce = null;
  workspaceSaveController?.abort();
  workspaceSaveController = null;
  workspaceSavePromise = null;
  displacedEdits = null;
  document.getElementById('workspaceConflictResolver')?.remove();
  workspaceHasLocalChanges = false;
  workspaceSaveFailures = 0;
  workspaceLocalOnly = false;
  localPromotion = null;
  nextWorkspaceNavigation = 'replace';
  if (activeRender) cancelRender(activeRender);
  resetFeedback();
  resetAssets();
  resetRevisions();
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
    document.getElementById('sharePopover').hidden = true;
    shareTokenCache = { id: null, promise: null };
  }
  workspaceId = id;
  setDownloadBaseName(id);
  const expectedPath = `/designs/${encodeURIComponent(id)}`;
  if (navigation === 'none' || location.pathname === expectedPath) return;
  if (navigation === 'push') history.pushState({ workspaceId: id }, '', expectedPath);
  else history.replaceState({ workspaceId: id }, '', expectedPath);
}

function workspaceStorageKey(id = workspaceId) {
  return `${DESIGN_CACHE_PREFIX}${id || 'draft'}`;
}

function displacedStorageKey(id = workspaceId) {
  return `reopenscad.displaced.${id || 'draft'}`;
}

// The cache records whether it holds edits the server has not acknowledged.
// Without that flag a clean copy of an old revision was indistinguishable from
// unsaved work, so reopening a workspace an agent had edited in the meantime
// put the stale cached source in the editor and called it a conflict.
function storeLocalWorkspace() {
  if (!workspaceId) return;
  try {
    localStorage.setItem(workspaceStorageKey(), JSON.stringify({
      code: editor.value,
      revision: workspaceRevision,
      dirty: editor.value !== workspaceSavedCode,
      ...(workspaceLocalOnly ? { localOnly: true } : {}),
    }));
  } catch {
    // A full or blocked store only costs the offline copy.
  }
}

function readCachedWorkspace(id) {
  try {
    const cached = JSON.parse(localStorage.getItem(workspaceStorageKey(id))
      ?? localStorage.getItem(`${LEGACY_CACHE_PREFIX}${id || 'draft'}`) ?? 'null');
    return cached && typeof cached.code === 'string' ? cached : null;
  } catch {
    return null;
  }
}

// Pure: what opening a workspace should show, given this browser's cached copy
// and the server's current state. The server is authoritative; the cache only
// survives as the editor contents when it holds unsaved edits made against the
// very revision the server still has. Unsaved edits against any other revision
// are handed back as `displaced` so they can be offered for restore.
function reconcileCachedWorkspace(cached, remote) {
  if (!cached || typeof cached.code !== 'string' || cached.code === remote?.code) return { action: 'remote' };
  // Entries written before the flag existed always mirrored the editor, so a
  // difference from the server can only have been unsaved work.
  if (cached.dirty === false) return { action: 'remote' };
  const base = Number(cached.revision);
  const remoteRevision = Number(remote?.revision);
  if (Number.isFinite(base) && Number.isFinite(remoteRevision) && base === remoteRevision) return { action: 'resume-local' };
  // A flagless entry behind the server is far more likely a clean copy from
  // before an agent's edit than unsaved work; offering it back would be noise.
  if (cached.dirty === undefined && Number.isFinite(base) && Number.isFinite(remoteRevision) && base < remoteRevision) return { action: 'remote' };
  return { action: 'remote', displaced: cached.code };
}

// Pure: what one `/events` answer means. `sinceRevision` is what the poll asked
// with, `localRevision` what the client holds now (a save may have landed while
// the long poll was parked).
function workspacePollAction(payload, sinceRevision, localRevision) {
  const revision = Number(payload?.revision);
  if (!Number.isFinite(revision)) return 'none';
  // The server is behind what we asked for: its store was restored or reset, or
  // our revision came from a stale cache. `changed` can never become true again
  // from here, so the long poll alone would stay silent forever.
  if (revision < sinceRevision) return 'resync';
  if (payload.changed === true && revision > localRevision && typeof payload.code === 'string') return 'apply';
  return 'none';
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

function refreshSyncState() {
  if (displacedEdits) setSyncState('Edits set aside', 'error');
  else if (!workspaceOnline) setSyncState('Local', 'local');
  else if (hasPendingLocalWorkspaceChanges()) setSyncState('Unsaved', 'saving');
  else setSyncState('Synced', 'synced');
}

// A newer server revision replaced edits the browser had not saved yet. They are
// parked, not merged and not dropped: the user decides from the sync pill.
function displaceLocalEdits(code, { announce = true } = {}) {
  if (typeof code !== 'string') return;
  displacedEdits = { code, at: Date.now() };
  try {
    localStorage.setItem(displacedStorageKey(), JSON.stringify(displacedEdits));
  } catch {
    // Kept in memory for this session at least.
  }
  $('#syncState').title = 'A newer revision from the server replaced your unsaved edits. Click to restore or discard them.';
  refreshSyncState();
  if (announce) log('A newer design revision arrived and replaced your unsaved edits. They were set aside — click “Edits set aside” to restore them.', 'warning');
}

function clearDisplacedEdits() {
  displacedEdits = null;
  try {
    localStorage.removeItem(displacedStorageKey());
  } catch {
    // Nothing to clean up.
  }
  $('#syncState').title = '';
  document.getElementById('workspaceConflictResolver')?.remove();
  refreshSyncState();
}

function restoreDisplacedEditsFromStorage(id) {
  try {
    const stored = JSON.parse(localStorage.getItem(displacedStorageKey(id)) || 'null');
    displacedEdits = stored && typeof stored.code === 'string' ? stored : null;
  } catch {
    displacedEdits = null;
  }
}

// The workspace is authoritative for its own viewer settings — that is what makes them
// travel with the shared URL. Whatever it carries also becomes the local default, so the
// next NEW workspace opens on the same clearance.
function applyWorkspaceSettings(workspace) {
  // A poll answering an AI code edit carries whatever settings the server had when it
  // replied. If the user has just changed one and that PATCH is still in flight, the reply
  // is stale by definition and would silently undo the choice they just made.
  if (settingsSavesInFlight.has(String(workspace.id ?? workspaceId))) return;
  // Every branch below is a no-op when the value already matches. This runs on every poll —
  // twice a second — so reassigning unconditionally would redraw the viewport continuously
  // and, worse, reset the select and the tolerance field out from under a user mid-edit.
  const tolerance = Number(workspace.tolerance);
  if (Number.isFinite(tolerance) && tolerance >= 0 && tolerance !== readTolerance()) {
    $('#toleranceInput').value = String(tolerance);
    localStorage.setItem(TOLERANCE_STORAGE_KEY, String(tolerance));
  }
}

// Brings the viewport in line with source that changed underneath it. Empty
// source has nothing to compile, so its stale geometry is simply cleared.
function scheduleSourceRender(delayMs = 60) {
  clearTimeout(renderDebounce);
  renderDebounce = null;
  if (!editor.value.trim()) {
    if (activeRender) cancelRender(activeRender);
    clearGeometry();
    return;
  }
  renderDebounce = setTimeout(() => {
    renderDebounce = null;
    render('preview');
  }, delayMs);
}

// The change ranges for every revision after `sinceRevision`, read from the
// events feed. A 409 reply carries the winning source but not what changed in
// it; adopting its revision without them would let the poll skip past that
// revision's AI highlight for good. Resolves null when unavailable.
async function fetchWorkspaceChanges(id, sinceRevision, signal) {
  try {
    const response = await fetch(`/api/designs/${encodeURIComponent(id)}/events?since=${sinceRevision}`, {
      cache: 'no-store',
      signal: AbortSignal.any([signal, AbortSignal.timeout(5000)]),
    });
    if (!response.ok || response.status === 204) return null;
    const payload = await response.json();
    return payload?.changed === true && typeof payload.code === 'string' ? payload : null;
  } catch {
    return null;
  }
}

function applyWorkspace(payload, { remote = false, rerender = true } = {}) {
  const workspace = workspaceFromPayload(payload);
  // Into the address book, empty or not: it is a real workspace with a URL.
  // Empty ones are labelled as such on the landing page rather than hidden.
  const incomingEmpty = typeof workspace.code === 'string' ? !workspace.code.trim() : undefined;
  if (!remote && workspace.id) rememberWorkspace(String(workspace.id), { empty: incomingEmpty });
  else if (remote && workspace.id && incomingEmpty !== undefined) noteRecentContent(String(workspace.id), incomingEmpty);
  const incomingRevision = Number(workspace.revision);
  let sourceChanged = false;
  // The server wins. Unsaved local edits that differ from the incoming source are
  // set aside for an explicit restore instead of holding the editor on a stale
  // revision — and a save still queued for them is dropped, because it was based
  // on a revision that no longer exists.
  if (remote && typeof workspace.code === 'string' && workspace.code !== editor.value && hasPendingLocalWorkspaceChanges()) {
    displaceLocalEdits(editor.value);
    clearTimeout(workspaceSaveDebounce);
    workspaceSaveDebounce = null;
  }
  // A remote payload never touches history: it can land while Back/Forward is
  // already on its way elsewhere, and a replaceState then would undo that.
  if (workspace.id) setWorkspaceIdentity(String(workspace.id), remote ? 'none' : nextWorkspaceNavigation);
  if (Number.isFinite(incomingRevision)) workspaceRevision = incomingRevision;
  if (typeof workspace.code === 'string' && workspace.code !== editor.value) {
    sourceChanged = true;
    applyingRemoteUpdate = true;
    editor.value = workspace.code;
    const aiUpdate = remote ? finalAiUpdate(payload, workspace) : { isAi: false, ranges: [] };
    aiUpdatedRanges = aiUpdate.isAi ? normalizeUpdatedRanges(workspace.code, aiUpdate.ranges) : [];
    setAiBadge(aiUpdate.isAi);
    syncEditor(false);
    applyingRemoteUpdate = false;
    if (aiUpdate.isAi) {
      log(`AI updated design revision ${workspaceRevision}.`, 'success');
    }
  }
  workspaceSavedCode = typeof workspace.code === 'string' ? workspace.code : editor.value;
  workspaceHasLocalChanges = false;
  if (!displacedEdits) $('#syncState').title = '';
  applyWorkspaceSettings(workspace);
  if (Array.isArray(workspace.objects) && (workspace.objects.length || !hasMesh)) {
    setSceneObjects(workspace.objects);
    sceneObjectsSource = workspace.objects.length && typeof workspace.code === 'string' ? workspace.code : null;
  } else if (sourceChanged && hasMesh) {
    // The list on screen was compiled from the source that was just replaced.
    sceneObjectsSource = null;
  }
  updateEmptyState();
  storeLocalWorkspace();
  // Whatever replaced the source — an agent over MCP, another tab, a save
  // conflict — the geometry on screen now belongs to code that is gone. Opening
  // a workspace (not remote) is rendered by its caller instead.
  // First: a render on its way keeps the model from being marked out of date.
  if (remote && sourceChanged && rerender) scheduleSourceRender();
  if (sourceChanged && hasMesh) markGeometryStale();
  refreshRevisions();
  return true;
}

function markGeometryStale() {
  geometryStale = true;
  showGeometryStaleness();
}

function markGeometryCurrent() {
  geometryStale = false;
  showGeometryStaleness();
}

// The model is only drawn as out of date when nothing is on its way to replace it:
// after a failed or cancelled render, or a change that does not re-render.
function showGeometryStaleness() {
  renderCanvas.classList.toggle('stale', geometryStale && !activeRender && renderDebounce === null);
}

// A workspace render or export answered 409: the server holds a newer revision
// than the one this request was bound to. Re-running the old source through the
// stateless route would render (or export) code that is no longer the
// workspace's, so the newer revision is adopted first — with its change ranges,
// so an agent's edit is still highlighted — and the caller retries against it.
// Resolves false when the revision could not be fetched.
async function adoptServerRevision(id, generation) {
  const current = () => generation === workspaceGeneration && id === workspaceId;
  let payload = await fetchWorkspaceChanges(id, workspaceRevision, workspaceLoadSignal());
  if (!current()) return false;
  if (!payload) {
    try {
      const response = await fetch(`/api/designs/${encodeURIComponent(id)}?allowMissing=1`, { cache: 'no-store', signal: workspaceLoadSignal() });
      payload = response.ok ? await response.json() : null;
    } catch {
      payload = null;
    }
    if (!current()) return false;
  }
  const remote = workspaceFromPayload(payload);
  if (!payload || payload.missing === true || typeof remote.code !== 'string' || !Number.isFinite(Number(remote.revision))) return false;
  if (Number(remote.revision) < workspaceRevision) return false;
  applyWorkspace(payload, { remote: true, rerender: false });
  workspaceOnline = true;
  refreshSyncState();
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

// A brand new workspace starts genuinely empty: an empty editor with a placeholder, the
// examples live on the landing page. `navigation` decides whether the previous workspace
// stays reachable via Back.
async function createWorkspace(code = '', { navigation = 'replace' } = {}) {
  leaveReadOnly();
  hideLanding();
  const generation = advanceWorkspaceGeneration();
  nextWorkspaceNavigation = navigation;
  setSyncState('Creating', 'saving');
  try {
    const response = await fetch('/api/designs', {
      method: 'POST',
      signal: workspaceLoadSignal(),
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        code,
        // `|| DEFAULT` would turn a deliberate 0 mm clearance back into 0.2.
        tolerance: readTolerance() ?? DEFAULT_TOLERANCE,
      }),
    });
    if (!response.ok) throw new Error(`Design service returned ${response.status}`);
    const payload = await response.json();
    if (generation !== workspaceGeneration) return;
    workspaceOnline = true;
    applyWorkspace(payload);
    setSyncState('Synced', 'synced');
  } catch {
    if (generation !== workspaceGeneration) return;
    workspaceOnline = false;
    const id = generateFunnyId();
    workspaceLocalOnly = true;
    setWorkspaceIdentity(id);
    workspaceRevision = 1;
    editor.value = code;
    syncEditor(false);
    workspaceSavedCode = '';
    storeLocalWorkspace();
    rememberWorkspace(id, { empty: !code.trim() });
    setSyncState('Local', 'local');
    log('The server could not be reached, so this design lives in this browser for now. It is created on the server as soon as it is reachable again.', 'warning');
  }
  if (generation === workspaceGeneration) beginWorkspacePolling(generation);
}

async function loadWorkspace(id) {
  leaveReadOnly();
  const generation = advanceWorkspaceGeneration();
  setWorkspaceIdentity(id, 'none');
  setSyncState('Loading', 'saving');
  const cached = readCachedWorkspace(id);
  restoreDisplacedEditsFromStorage(id);
  try {
    const response = await fetch(`/api/designs/${encodeURIComponent(id)}?allowMissing=1`, { cache: 'no-store', signal: workspaceLoadSignal() });
    if (generation !== workspaceGeneration || id !== workspaceId) return;
    const payload = await response.json();
    if (generation !== workspaceGeneration || id !== workspaceId) return;
    if (response.ok && payload.missing === true && cached?.localOnly) {
      // Started offline in this browser: the server never had it. Bring it back
      // from the cache and create it there now, rather than calling it "gone".
      hideLanding();
      workspaceLocalOnly = true;
      workspaceOnline = false;
      workspaceRevision = Number(cached.revision) || 1;
      editor.value = cached.code;
      workspaceSavedCode = '';
      syncEditor(false);
      setSyncState('Local', 'local');
      await promoteLocalWorkspace();
      if (generation === workspaceGeneration) beginWorkspacePolling(generation);
      return;
    }
    if (response.ok && payload.missing === true) {
      // Never mint a replacement behind the user's back: that silently swapped
      // the URL for a new id. Say what happened and let them choose. Captured
      // before showLanding, which clears the set-aside edits it may include.
      const recoverable = cached?.code || displacedEdits?.code || '';
      showLanding({ missing: { id, code: recoverable } });
      return;
    }
    // Only now: a missing workspace goes straight back to the landing page
    // without flashing the editor shell first.
    hideLanding();
    if (!response.ok) throw new Error(`Design service returned ${response.status}`);
    workspaceOnline = true;
    const remote = workspaceFromPayload(payload);
    const decision = reconcileCachedWorkspace(cached, remote);
    if (decision.action === 'resume-local') {
      // Unsaved edits made against the revision the server still holds: nothing
      // newer exists to lose, so they pick up where they left off and save.
      applyWorkspace(payload);
      editor.value = cached.code;
      syncEditor(false);
      setSyncState('Unsaved', 'saving');
      scheduleWorkspaceSave();
    } else {
      applyWorkspace(payload);
      if (decision.displaced && decision.displaced !== displacedEdits?.code) displaceLocalEdits(decision.displaced);
      refreshSyncState();
    }
  } catch {
    if (generation !== workspaceGeneration || id !== workspaceId) return;
    hideLanding();
    workspaceOnline = false;
    workspaceLocalOnly = cached?.localOnly === true;
    if (cached?.code !== undefined && (cached.code || cached.localOnly)) {
      workspaceRevision = Number(cached.revision) || 1;
      editor.value = cached.code;
      // The cache's own flag says whether this was saved; anything unsaved is
      // re-queued the moment the server is reachable again.
      workspaceSavedCode = cached.dirty === false ? cached.code : '';
    } else {
      // Never seed from another workspace's cache: an unknown workspace starts empty.
      editor.value = '';
      workspaceSavedCode = '';
    }
    syncEditor(false);
    setSyncState('Local', 'local');
    // Unsaved edits from the cache go back in the save queue, whose retry and
    // backoff land them once the server answers (or set them aside on a 409).
    if (cached?.code && cached.dirty !== false) scheduleWorkspaceSave();
  }
  if (generation === workspaceGeneration) beginWorkspacePolling(generation);
}

// Creates an offline-minted workspace on the server and moves this page, its
// cache and its entry in "Your workspaces" over to the id the server assigns.
// The server issues ids itself, so the local one cannot simply be claimed.
// Resolves true once the page is on the server-backed id.
function promoteLocalWorkspace() {
  if (!workspaceLocalOnly || !workspaceId || readOnlyToken) return Promise.resolve(false);
  if (localPromotion) return localPromotion;
  const localId = workspaceId;
  const generation = workspaceGeneration;
  const code = editor.value;
  const current = () => generation === workspaceGeneration && localId === workspaceId;
  const promotion = (async () => {
    try {
      const response = await fetch('/api/designs', {
        method: 'POST',
        signal: workspaceLoadSignal(),
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          code,
          tolerance: readTolerance() ?? DEFAULT_TOLERANCE,
        }),
      });
      if (!response.ok) return false;
      const workspace = workspaceFromPayload(await response.json());
      if (!current() || !workspace.id) return false;
      const serverId = String(workspace.id);
      try {
        localStorage.removeItem(workspaceStorageKey(localId));
        const displaced = localStorage.getItem(displacedStorageKey(localId));
        if (displaced) {
          localStorage.setItem(displacedStorageKey(serverId), displaced);
          localStorage.removeItem(displacedStorageKey(localId));
        }
      } catch {
        // The new cache entry below is what matters; a stale one only wastes space.
      }
      renameRecentWorkspace(localId, serverId);
      noteRecentContent(serverId, !code.trim());
      promotedWorkspaceIds.set(localId, serverId);
      workspaceLocalOnly = false;
      workspaceOnline = true;
      workspaceSaveFailures = 0;
      setWorkspaceIdentity(serverId, 'replace');
      workspaceRevision = Number(workspace.revision) || 1;
      workspaceSavedCode = code;
      workspaceHasLocalChanges = editor.value !== code;
      storeLocalWorkspace();
      log('Back online: the design started offline is now saved on the server, under a new link (now in the address bar).', 'success');
      if (workspaceHasLocalChanges) scheduleWorkspaceSave();
      refreshSyncState();
      beginWorkspacePolling(generation);
      return true;
    } catch {
      return false;
    } finally {
      if (localPromotion === promotion) localPromotion = null;
    }
  })();
  localPromotion = promotion;
  return promotion;
}

// Asks the server for the workspace as it is now. Called whenever the page may
// have been away — tab shown again, window focused, restored from the
// back/forward cache, network back — because none of those guarantee the
// long poll saw everything. A newer revision is left to the event poll, which
// is restarted here and answers immediately with the change ranges the AI
// highlight needs; this call catches the cases the poll cannot.
let lastWorkspaceRefresh = 0;
async function refreshWorkspaceFromServer({ force = false } = {}) {
  if (!workspaceId || readOnlyToken) return;
  if (!force && Date.now() - lastWorkspaceRefresh < 1500) return;
  lastWorkspaceRefresh = Date.now();
  const id = workspaceId;
  const generation = workspaceGeneration;
  const startRevision = workspaceRevision;
  const savingAtStart = workspaceSavePromise !== null;
  try {
    const response = await fetch(`/api/designs/${encodeURIComponent(id)}?allowMissing=1`, { cache: 'no-store', signal: workspaceLoadSignal() });
    if (!response.ok) throw new Error(`Design service returned ${response.status}`);
    const payload = await response.json();
    if (generation !== workspaceGeneration || id !== workspaceId) return;
    if (payload.missing === true) {
      if (workspaceLocalOnly) await promoteLocalWorkspace();
      return;
    }
    // A save or a poll moved the revision while this GET was out, so its answer
    // may predate that and would rewind it. The restarted poll covers the rest.
    if (savingAtStart || workspaceSavePromise || workspaceRevision !== startRevision) return;
    workspaceOnline = true;
    const remote = workspaceFromPayload(payload);
    const remoteRevision = Number(remote.revision);
    if (!Number.isFinite(remoteRevision) || typeof remote.code !== 'string') return;
    applyWorkspaceSettings({ ...remote, id });
    if (remoteRevision < workspaceRevision) {
      // The server is behind us (store restored, or a stale cached revision).
      // Its numbering is the only one a save can be based on.
      if (hasPendingLocalWorkspaceChanges() && editor.value !== remote.code) {
        workspaceRevision = remoteRevision;
        workspaceSavedCode = remote.code;
        scheduleWorkspaceSave();
      } else {
        applyWorkspace(payload, { remote: true });
      }
    } else if (remoteRevision === workspaceRevision && remote.code !== editor.value && !hasPendingLocalWorkspaceChanges()) {
      applyWorkspace(payload, { remote: true });
    }
    refreshSyncState();
  } catch {
    // Offline: the poll loop below keeps retrying.
  } finally {
    if (generation === workspaceGeneration && id === workspaceId) beginWorkspacePolling(generation);
  }
}

// Viewer settings ride their own revisionless PATCH rather than piggybacking on the source
// save: changing the tolerance is not an edit, so it must not bump the revision, mark the source
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
    const body = { tolerance: readTolerance() ?? DEFAULT_TOLERANCE };
    fetch(`/api/designs/${encodeURIComponent(id)}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(SETTINGS_SAVE_TIMEOUT_MS),
    }).then((response) => {
      if (!response.ok) throw new Error(`Design service returned ${response.status}`);
      workspaceOnline = true;
      release();
    }).catch(() => {
      // Stop once this is no longer the workspace on screen: retrying would write settings
      // the user has already navigated away from.
      if (generation !== workspaceGeneration || id !== workspaceId) { release(); return; }
      if (attempt + 1 >= SETTINGS_SAVE_ATTEMPTS) {
        release();
        log('The tolerance could not be saved to this design; it is kept locally.', 'warning');
        return;
      }
      setTimeout(() => attemptSave(attempt + 1), Math.min(8000, 700 * 2 ** attempt));
    });
  };
  attemptSave(0);
}

let workspaceSaveQueued = false;
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
  if (!workspaceId) return false;
  if (workspaceSavePromise) {
    // Asked for while one is on its way: if that one is refused, this one still gets its turn.
    workspaceSaveQueued = true;
    return workspaceSavePromise;
  }
  workspaceSaveQueued = false;
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
      const response = await fetch(`/api/designs/${encodeURIComponent(id)}`, {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json' },
        signal: controller.signal,
        body: JSON.stringify({ code, baseRevision, source: 'browser' }),
      });
      const payload = await response.json();
      if (generation !== workspaceGeneration || id !== workspaceId) return false;
      if (response.status === 409) {
        // Someone — usually an agent over MCP — saved a newer revision first. It
        // wins; this edit is set aside for the user to restore on top of it.
        workspaceOnline = true;
        workspaceSaveFailures = 0;
        const changes = await fetchWorkspaceChanges(id, baseRevision, controller.signal);
        if (generation !== workspaceGeneration || id !== workspaceId) return false;
        const conflictRevision = Number(workspaceFromPayload(payload).revision);
        const winner = changes && !(Number(changes.revision) < conflictRevision) ? changes : payload;
        // Never rewind past a revision something else already applied.
        if (!(Number(workspaceFromPayload(winner).revision) < workspaceRevision)) applyWorkspace(winner, { remote: true });
        refreshSyncState();
        return false;
      }
      if (response.status === 404 && workspaceLocalOnly) {
        // The server is back and has never heard of this offline id: create it.
        // The retry loop below would otherwise PATCH a 404 forever.
        const promoted = await promoteLocalWorkspace();
        if (!promoted) throw new Error('Design could not be created on the server yet');
        return !workspaceHasLocalChanges;
      }
      if (response.status === 422) {
        // The source was refused: it does not compile, or keeps an expired import. Sending
        // it again gets the same answer, and the render already says why. It stays unsaved
        // until the next edit, or an upload that brings the import back.
        workspaceOnline = true;
        workspaceSaveFailures = 0;
        refreshSyncState();
        // Edited (or an upload came in) while this one was on its way: that gets its own try.
        if (editor.value !== code || workspaceSaveQueued) scheduleWorkspaceSave();
        return false;
      }
      if (!response.ok) throw new Error(`Design service returned ${response.status}`);
      workspaceOnline = true;
      workspaceSaveFailures = 0;
      // A newer server revision was applied while this request was in flight. It
      // already owns the editor and the revision counter; adopting this reply's
      // older revision would rewind both.
      if (workspaceRevision !== baseRevision) {
        refreshSyncState();
        return false;
      }
      const workspace = workspaceFromPayload(payload);
      if (Number.isFinite(Number(workspace.revision))) workspaceRevision = Number(workspace.revision);
      workspaceSavedCode = code;
      workspaceHasLocalChanges = editor.value !== code;
      storeLocalWorkspace();
      // Saved content is worth finding again from the landing page, rendered or not.
      rememberWorkspace(id, { empty: !code.trim() });
      refreshSyncState();
      // Saves come in a burst while typing; the list waits for it to pause.
      refreshRevisions(1200);
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
  if (workspaceHasLocalChanges) await saveWorkspace();
  return workspaceOnline
    && !workspaceHasLocalChanges
    && workspaceSavedCode === editor.value;
}

// One long-poll loop per page. Restarting aborts the request the previous loop
// is parked on, so a restart always means "ask again now" rather than "add
// another subscriber" — the server caps those, and a pile of orphaned loops from
// repeated tab switches used to exhaust the cap and leave every poll failing.
function beginWorkspacePolling(generation = workspaceGeneration) {
  clearTimeout(eventPollTimer);
  eventPollTimer = null;
  eventPollController?.abort();
  eventPollController = null;
  const token = ++pollLoopToken;
  const alive = () => token === pollLoopToken && generation === workspaceGeneration;
  // Before the first poll, so calls that start while it is in flight count as live.
  if (agentActivity.design !== workspaceId) resetAgentActivity(workspaceId);
  renderFeedback();
  loadAssets();
  refreshRevisions(0);
  const schedule = (milliseconds) => {
    if (alive()) eventPollTimer = setTimeout(poll, milliseconds);
  };
  const poll = async () => {
    if (!alive() || !workspaceId || readOnlyToken) return;
    // A hidden tab stops asking; `visibilitychange` restarts the loop with a full refresh.
    if (document.hidden) return;
    const id = workspaceId;
    const sinceRevision = workspaceRevision;
    const controller = new AbortController();
    eventPollController = controller;
    const timeout = setTimeout(() => controller.abort(), 25000);
    let next = 4000;
    try {
      const response = await fetch(`/api/designs/${encodeURIComponent(id)}/events?since=${sinceRevision}&feedback=${feedbackVersion}&activity=${agentActivityVersion(id)}${assetsEnabled() && assetsVersion !== null ? `&assets=${assetsVersion}` : ''}`, { signal: controller.signal, cache: 'no-store' });
      if (!alive() || id !== workspaceId) return;
      if (response.ok && response.status !== 204) {
        const payload = await response.json();
        if (!alive() || id !== workspaceId) return;
        workspaceOnline = true;
        next = 500;
        // Viewer settings are revisionless, so `changed` says nothing about them: sync them
        // on every poll, which is how a tolerance set in another tab reaches this one. Guarded against our own in-flight save inside the helper.
        applyWorkspaceSettings({ ...workspaceFromPayload(payload), id });
        applyFeedbackSummary(payload.feedback);
        applyAssetsSummary(payload);
        // The server wakes pollers on commit, so the answer to our own save can
        // beat the save's reply. Let the reply land first, or our own revision
        // would look like someone else's and set the newest keystrokes aside.
        if (workspaceSavePromise) await workspaceSavePromise;
        if (!alive() || id !== workspaceId) return;
        const action = workspacePollAction(payload, sinceRevision, workspaceRevision);
        if (action === 'apply') applyWorkspace(payload, { remote: true });
        // After the revision it may describe: an agent's edit and its check of
        // that edit often arrive in the same answer.
        applyActivitySummary(payload.activity, id);
        if (action === 'resync') {
          refreshWorkspaceFromServer({ force: true });
          return;
        }
        refreshSyncState();
      } else if (response.status === 404 && workspaceLocalOnly) {
        if (await promoteLocalWorkspace()) return;
        next = 8000;
      } else if (response.status === 404) {
        setSyncState('Not found', 'error');
        $('#syncState').title = 'This design no longer exists on the server. Your source is still in the editor — save it or copy it to a new design.';
        next = 15000;
      }
    } catch {
      // Offline or aborted: local work stays available and the loop retries.
    } finally {
      clearTimeout(timeout);
      if (eventPollController === controller) eventPollController = null;
    }
    schedule(next);
  };
  poll();
}

function normalizeObjects(objects = [], { placeholder = false } = {}) {
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
  // The placeholder stands for a mesh that came back without an object list. It
  // is only ever asked for by a render that has such a mesh: a workspace payload
  // with no objects means "not compiled yet", and turning that into a phantom
  // "Model" row is what showed an object beside an empty editor after Back.
  if (normalized.length || !placeholder) return normalized;
  return [{ id: 'model', name: 'Model', kind: 'compiled mesh', placeholder: true, visible: previous.get('model')?.visible !== false }];
}

function setSceneObjects(objects, options) {
  sceneObjects = normalizeObjects(objects, options);
  if (!sceneObjects.some((object) => object.id === selectedObjectId)) selectedObjectId = sceneObjects[0]?.id || null;
  renderObjectList();
  meshViewport.draw();
}

// `color()` reaches the object list as `[r, g, b, a]` in 0..1, or null for the
// default material. Returned as a CSS colour so the swatch matches the viewport.
function objectCssColor(object, stated = object?.color) {
  const color = Array.isArray(stated) ? stated.map(Number) : null;
  if (!color || color.length < 3 || color.slice(0, 3).some((channel) => !Number.isFinite(channel))) return null;
  const [r, g, b] = color.slice(0, 3).map((channel) => Math.round(Math.min(1, Math.max(0, channel)) * 255));
  const alpha = Number.isFinite(color[3]) ? Math.min(1, Math.max(0, color[3])) : 1;
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}

// An object painted in several colours shows them all in its swatch, as equal slices.
// Null for a one-colour object, whose swatch is plain `--swatch`.
function objectSwatchFill(object) {
  const palette = object?.faceColors?.palette ?? object?.colors;
  if (!Array.isArray(palette) || palette.length < 2) return null;
  const stops = palette.map((color, index) => {
    const css = objectCssColor(object, color) || 'var(--yellow)';
    return `${css} ${(index / palette.length) * 100}% ${((index + 1) / palette.length) * 100}%`;
  });
  return `conic-gradient(${stops.join(', ')})`;
}

function objectSwatchHtml(object) {
  const swatch = objectCssColor(object);
  const fill = objectSwatchFill(object);
  return `<i class="object-swatch${swatch || fill ? ' custom' : ''}"${swatch || fill ? ` style="--swatch: ${swatch || 'var(--yellow)'}${fill ? `; background: ${fill}` : ''}"` : ''}></i>`;
}

function renderObjectList() {
  syncCompareParts();
  $('#objectCount').textContent = String(sceneObjects.length);
  $('#mobileObjectCount').textContent = String(sceneObjects.length);
  // Each bulk button is live only while it would change something.
  $('#showAllObjects').disabled = !sceneObjects.some((object) => object.visible === false);
  $('#hideAllObjects').disabled = !sceneObjects.some((object) => object.visible !== false);
  const list = $('#objectList');
  if (!sceneObjects.length) {
    list.innerHTML = '<div class="objects-empty">Objects appear after preview.</div>';
    return;
  }
  const directives = parsePrintDirectives(sceneObjectsSource ?? editor.value);
  // The rows are rebuilt: the keyboard stays on the same button of the same part.
  const active = list.contains(document.activeElement) ? document.activeElement : null;
  const focusedRow = active?.closest('[data-object-id]')?.dataset.objectId;
  const focusedControl = active?.classList.contains('object-export') ? '.object-export' : '.visibility-toggle';
  list.innerHTML = sceneObjects.map((object) => {
    const name = escapeAttribute(object.name);
    // Background (`%`) geometry is never exported, and a placeholder has no id the server knows.
    const exportable = !object.placeholder && !object.background;
    const print = printDirectiveFor(directives, object);
    const printTag = print
      ? `<span class="object-print" title="Print orientation (line ${print.line}): ${escapeAttribute(`${print.key}=${print.value}`)}">${escapeHtml(printDirectiveLabel(print))}</span>`
      : '';
    // Only what differs from a plain part earns words on the row.
    const kind = object.kind && object.kind !== 'part' ? `<small>${escapeHtml(object.kind)}</small>` : '';
    return `<div class="object-row${object.id === selectedObjectId ? ' selected' : ''}${object.visible === false ? ' hidden-object' : ''}${intersectionIds.has(object.id) ? ' intersecting' : ''}" role="option" aria-selected="${object.id === selectedObjectId}" data-object-id="${escapeAttribute(object.id)}">`
      + objectSwatchHtml(object)
      + `<span class="object-copy"><b title="${name}">${escapeHtml(object.name)}</b>${kind}${printTag}</span>`
      + (exportable ? `<button class="object-export" aria-label="Export ${name}" title="Export this object" aria-haspopup="menu">${icons.export}</button>` : '<span></span>')
      + `<button class="visibility-toggle" aria-label="${object.visible === false ? 'Show' : 'Hide'} ${name}" title="${object.visible === false ? 'Show' : 'Hide'} object" aria-pressed="${object.visible === false}">${object.visible === false ? icons.eyeOff : icons.eye}</button></div>`;
  }).join('');
  if (focusedRow !== undefined) {
    [...list.children].find((row) => row.dataset.objectId === focusedRow)?.querySelector(focusedControl)?.focus();
  }
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

// --- Mesh assets -----------------------------------------------------------------
//
// An STL uploaded to the design as a temporary reference for reverse engineering
// and used from SCAD as `import("name.stl");`. One per design, and it expires 3
// hours after upload. Names are [A-Za-z0-9._-]{1,64} and end in `.stl`. Editable designs
// only: a shared view never uploads or removes.

const ASSET_MAX_BYTES = 128 * 1024 * 1024;
// Files larger than this go up in chunks of this size (Content-Range), under
// the 32 MiB request cap of hosts such as Cloud Run.
const ASSET_CHUNK_BYTES = 24 * 1024 * 1024;
const ASSET_NAME_PATTERN = /^[A-Za-z0-9._-]{1,64}$/;

// Pure: past its `expiresAt`. An entry without one has no known end.
function assetExpired(asset, now) {
  const expiresAt = Number(asset?.expiresAt);
  return expiresAt > 0 && expiresAt <= now;
}

// The time on the server's clock, as well as this page knows it.
function assetNow() {
  return Date.now() + assetClockOffsetMs;
}

// Pure: the upload the design holds now.
function currentAssetUpload(assets, now) {
  return (Array.isArray(assets) ? assets : []).find((asset) => asset && !assetExpired(asset, now)) || null;
}

// Pure: "expires in 2h 14m". Rounds up, so it reads 0 only once the time is over;
// seconds in the last minute.
function formatAssetCountdown(msLeft) {
  if (!Number.isFinite(msLeft)) return '';
  if (msLeft < 60000) return `expires in ${Math.max(1, Math.ceil(msLeft / 1000))}s`;
  const minutes = Math.ceil(msLeft / 60000);
  const hours = Math.floor(minutes / 60);
  const rest = minutes % 60;
  return `expires in ${hours ? `${hours}h${rest ? ` ${rest}m` : ''}` : `${rest}m`}`;
}

// Pure: what the row shows at `now`. `key` changes when the upload does. Stress
// starts as the label first reads 30m.
function assetRowView(assets, now) {
  const upload = currentAssetUpload(assets, now);
  if (!upload) return { upload: null, key: null, name: '', countdown: '', counting: false, stressed: false };
  const expiresAt = Number(upload.expiresAt);
  const msLeft = expiresAt > 0 ? expiresAt - now : Number.NaN;
  return {
    upload,
    key: `${upload.name}|${upload.expiresAt}`,
    name: String(upload.name || ''),
    // Not blank: a row that says nothing about time reads as a bug. A server too old to
    // state an expiry is the one way here.
    countdown: Number.isFinite(msLeft) ? formatAssetCountdown(msLeft) : 'expiry unknown',
    counting: Number.isFinite(msLeft),
    stressed: msLeft <= 30 * 60 * 1000,
  };
}

// Pure: why a second upload is refused. The server's 409 says this itself; this
// is for the refusal made here, before a large file goes up for nothing.
function assetRefusalMessage(name) {
  return `This design already has ${name ? `“${name}”` : 'an STL'}. Each design keeps one: remove it first, then upload again.`;
}

// Pure: any file name -> one the server accepts. "Mesh 2026-10-15.stl" ->
// "Mesh_2026-10-15.stl"; path parts go, every other character becomes `_`,
// runs collapse, and the extension is always a lowercase `.stl`.
function sanitizeAssetName(raw) {
  const base = String(raw ?? '').split(/[\\/]/).pop().replace(/\.stl$/i, '');
  let stem = base
    .normalize('NFKD').replace(/[̀-ͯ]/g, '')
    .replace(/[^A-Za-z0-9._-]+/g, '_')
    .replace(/_{2,}/g, '_')
    .replace(/^[._-]+/, '')
    .replace(/[._]+$/, '');
  if (!stem) stem = 'mesh';
  stem = stem.slice(0, 60).replace(/[._]+$/, '') || 'mesh';
  return `${stem}.stl`;
}

function escapeRegExp(text) {
  return String(text).replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

// The `import("name")` call for one asset, with or without `file =`.
function assetImportPattern(name, flags = '') {
  return new RegExp(`\\bimport\\s*\\(\\s*(?:file\\s*=\\s*)?"${escapeRegExp(name)}"[^;]*?\\)\\s*;`, flags);
}

// Pure: comments out, so a commented-out import does not count as a use.
function assetReferenced(code, name) {
  const live = String(code ?? '').replace(/\/\*[\s\S]*?\*\//g, ' ').replace(/\/\/[^\n]*/g, '');
  return assetImportPattern(name).test(live);
}

// Pure: where `import("name");` goes. At the cursor when that is a safe
// statement boundary (a blank line, or the end of a line that closes a
// statement); otherwise at the end of the source, on its own line. Null when
// the asset is already imported. Answers the new source and the inserted span.
function insertAssetImport(code, name, cursor = null) {
  const source = String(code ?? '');
  if (assetReferenced(source, name)) return null;
  const statement = `import("${name}");`;
  if (Number.isInteger(cursor) && cursor >= 0 && cursor <= source.length) {
    const lineStart = source.lastIndexOf('\n', cursor - 1) + 1;
    const nextBreak = source.indexOf('\n', cursor);
    const lineEnd = nextBreak < 0 ? source.length : nextBreak;
    const line = source.slice(lineStart, lineEnd);
    if (!line.trim()) {
      const indent = line.match(/^[ \t]*/)[0];
      const text = indent + statement;
      return { code: source.slice(0, lineStart) + text + source.slice(lineEnd), start: lineStart + indent.length, end: lineStart + text.length };
    }
    if (cursor === lineEnd && /[;\x7d]\s*$/.test(line)) {
      const indent = line.match(/^[ \t]*/)[0];
      const text = `\n${indent}${statement}`;
      return { code: source.slice(0, lineEnd) + text + source.slice(lineEnd), start: lineEnd + 1 + indent.length, end: lineEnd + text.length };
    }
  }
  const trimmed = source.replace(/\s+$/, '');
  const prefix = trimmed ? `${trimmed}\n` : '';
  return { code: `${prefix}${statement}\n`, start: prefix.length, end: prefix.length + statement.length };
}

// Pure: a scale factor as SCAD writes it (25.4, 0.1, 1000), never 25.400000000000002.
function formatScaleFactor(factor) {
  const value = Number(factor);
  return String(Number(value.toPrecision(6)));
}

// Pure: wraps the asset's import in `scale(f)`. An existing `scale(...)` right
// before it is replaced rather than stacked; a missing import is added at the
// end, already scaled. Null for a factor that is not a positive finite number.
function wrapAssetImportWithScale(code, name, factor) {
  const value = Number(factor);
  if (!Number.isFinite(value) || value <= 0) return null;
  const source = String(code ?? '');
  const scale = `scale(${formatScaleFactor(value)}) `;
  const match = assetImportPattern(name).exec(source);
  if (!match) {
    const trimmed = source.replace(/\s+$/, '');
    const prefix = trimmed ? `${trimmed}\n` : '';
    const text = `${scale}import("${name}");`;
    return { code: `${prefix}${text}\n`, start: prefix.length, end: prefix.length + text.length };
  }
  const before = source.slice(0, match.index);
  const existing = before.match(/scale\s*\(\s*[-+0-9.eE]+\s*\)\s*$/);
  const start = existing ? match.index - existing[0].length : match.index;
  const end = match.index + match[0].length;
  const text = scale + match[0];
  return { code: source.slice(0, start) + text + source.slice(end), start, end: start + text.length };
}

// Pure: what an `/events` answer says about assets. The server reports
// `assetsVersion` on every poll and sets `assetsChanged` when an upload or removal
// ended it; the revision does not move, so a change means re-render.
//
// `own` describes this tab's asset writes: `pending` counts uploads/deletes
// still in flight and `versions` holds the versions they produced. A poll can
// end on our own write before its response lands; that change was already
// rendered by the writer, so it is tracked rather than reloaded.
function assetsSummaryAction(payload, knownVersion, own = null) {
  const version = payload?.assetsVersion;
  if (version === undefined || version === null) return { action: 'none' };
  if (knownVersion === null || knownVersion === undefined) return { action: 'track', version };
  if (payload.assetsChanged || String(version) !== String(knownVersion)) {
    const ownWrite = (own?.pending || 0) > 0
      || (own?.versions || []).some((seen) => String(seen) === String(version));
    return { action: ownWrite ? 'track' : 'reload', version };
  }
  return { action: 'none' };
}

// This tab's asset writes, for `assetsSummaryAction`.
const ownAssetWrites = { pending: 0, versions: [] };

function beginOwnAssetWrite() {
  ownAssetWrites.pending += 1;
  let finished = false;
  return (version) => {
    if (finished) return;
    finished = true;
    ownAssetWrites.pending = Math.max(0, ownAssetWrites.pending - 1);
    if (version !== undefined && version !== null) {
      ownAssetWrites.versions = [...ownAssetWrites.versions, version].slice(-16);
    }
  };
}

function formatBytes(bytes) {
  const value = Number(bytes) || 0;
  if (value < 1024) return `${value} B`;
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(value < 10240 ? 1 : 0)} KB`;
  return `${(value / 1024 / 1024).toFixed(1)} MB`;
}

function formatCount(value) {
  return Number(value || 0).toLocaleString('en-US');
}

function assetsEnabled() {
  return Boolean(workspaceId) && !readOnlyToken && !workspaceLocalOnly;
}

function assetUrl(name = '') {
  return `/api/designs/${encodeURIComponent(workspaceId)}/assets${name ? `/${encodeURIComponent(name)}` : ''}`;
}

// ---- Revisions -------------------------------------------------------------
// Every change is a saved, named revision, so this list is the design's history:
// an agent's new one appears here as the /events poll applies it, and Restore
// copies an old one on top as a new revision; history is never rewritten. Names
// come from agents: they only ever reach the page through escapeHtml.
const REVISION_NAME_LIMIT = 40;

function revisionClock(ms) {
  const time = Number(ms);
  if (!Number.isFinite(time) || time <= 0) return '';
  const date = new Date(time);
  const today = date.toDateString() === new Date().toDateString();
  return today
    ? date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', hour12: false })
    : date.toLocaleDateString([], { month: 'short', day: 'numeric' });
}

function revisionAuthor(source) {
  if (source === 'agent') return 'agent';
  if (source === 'browser') return readOnlyToken ? 'browser' : 'you';
  return '';
}

// One line: name, number and time. Restore takes the place of those two while the row is in play.
function revisionRowHtml(row) {
  const current = row.current === true;
  const author = revisionAuthor(row.source);
  const clock = revisionClock(row.timestamp);
  const meta = [`#${Number(row.revision)}`, author, clock].filter(Boolean).join(' · ');
  const action = current || readOnlyToken
    ? ''
    : `<button type="button" class="revision-restore" data-revision-restore="${Number(row.revision)}"${revisionRestoring ? ' disabled' : ''} aria-label="Restore revision ${Number(row.revision)}">Restore</button>`;
  const spoken = [author, current ? 'current' : ''].filter(Boolean).map((word) => `<span class="sr-only">${escapeHtml(word)}</span>`).join('');
  return `<li class="revision-row${current ? ' current' : ''}${row.source === 'agent' ? ' by-agent' : ''}"${current ? ' aria-current="true"' : ''} title="${escapeAttribute(meta)}"><b title="${escapeAttribute(row.name)}">${escapeHtml(row.name)}</b>${spoken}<small>#${Number(row.revision)}${clock ? ` ${escapeHtml(clock)}` : ''}</small>${action}</li>`;
}

function renderRevisions() {
  const newest = revisionRows.find((row) => row.current === true) || revisionRows[0];
  $('#revisionCount').textContent = newest ? `#${Number(newest.revision)}` : '';
  const list = $('#revisionList');
  const focused = list.contains(document.activeElement) ? document.activeElement.dataset.revisionRestore : undefined;
  list.innerHTML = revisionRows.length
    ? revisionRows.map(revisionRowHtml).join('')
    : '<li class="revision-empty">Revisions appear once the design is saved.</li>';
  // Rebuilt under the keyboard: back to that row's Restore while there is one.
  if (focused !== undefined) list.querySelector(`[data-revision-restore="${focused}"]:not(:disabled)`)?.focus();
  // A heatmap of an older revision would sit on geometry that moved.
  const surfaces = agentActivity.surfaces;
  if (surfaces?.revision !== null && surfaces?.revision !== undefined && Number(surfaces.revision) < workspaceRevision) switchOffAgentSurface();
}

function resetRevisions() {
  clearTimeout(revisionsTimer);
  revisionsTimer = null;
  revisionsLoadToken += 1;
  revisionRows = [];
  renderRevisions();
}

// `delay` lets a burst of revisions (typing, an agent's writes) load once.
function refreshRevisions(delay = 150) {
  clearTimeout(revisionsTimer);
  revisionsTimer = setTimeout(loadRevisions, delay);
}

async function loadRevisions() {
  revisionsTimer = null;
  const token = ++revisionsLoadToken;
  const url = readOnlyToken
    ? `/api/shared/${encodeURIComponent(readOnlyToken)}/revisions`
    : workspaceId ? `/api/designs/${encodeURIComponent(workspaceId)}/revisions` : null;
  if (!url) {
    revisionRows = [];
    renderRevisions();
    return;
  }
  try {
    const response = await fetch(url, { cache: 'no-store', signal: workspaceLoadSignal() });
    if (token !== revisionsLoadToken) return;
    // An offline or not yet created design has no history to list.
    const data = response.ok ? await response.json() : null;
    if (token !== revisionsLoadToken) return;
    revisionRows = Array.isArray(data?.revisions) ? data.revisions : [];
    renderRevisions();
    // A save or an agent's write landed while this was on its way.
    if (!readOnlyToken && Number(data?.current) < workspaceRevision) refreshRevisions();
  } catch {
    // Aborted or offline: the list on screen stays until the next revision.
  }
}

// "Restore: <old name>", cut to what a revision name may hold, by whole characters.
function restoredRevisionName(name) {
  return Array.from(`Restore: ${name}`).slice(0, REVISION_NAME_LIMIT).join('').trim();
}

async function restoreRevision(revision) {
  if (!workspaceId || readOnlyToken || revisionRestoring) return;
  const row = revisionRows.find((candidate) => Number(candidate.revision) === revision);
  if (!row) return;
  const id = workspaceId;
  const generation = workspaceGeneration;
  const stale = () => generation !== workspaceGeneration || id !== workspaceId;
  revisionRestoring = true;
  renderRevisions();
  try {
    // Edits still on their way are saved first; if that fails, the restore
    // sets them aside like any edit a newer revision replaces.
    await flushWorkspaceSave();
    if (stale()) return;
    const response = await fetch(`/api/designs/${encodeURIComponent(id)}/revisions`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ revision, revisionName: restoredRevisionName(row.name), expectedRevision: workspaceRevision }),
    });
    const payload = await response.json().catch(() => ({}));
    if (stale()) return;
    if (response.status === 409 && payload.workspace) {
      // Someone saved first: their revision wins and the list shows it; Restore is
      // still there for another try.
      applyWorkspace(payload.workspace, { remote: true });
      log(`Revision ${revision} was not restored: a newer revision arrived first.`, 'warning');
      return;
    }
    if (!response.ok || payload.ok === false) throw new Error(payload.error || `Design service returned ${response.status}`);
    applyWorkspace(payload, { remote: true });
    refreshSyncState();
    log(`Restored revision ${revision} as revision ${workspaceRevision}.`, 'success');
  } catch (error) {
    if (!stale()) log(`Revision ${revision} could not be restored: ${error.message}`, 'error');
  } finally {
    revisionRestoring = false;
    if (!stale()) renderRevisions();
  }
}

$('#revisionList').addEventListener('click', (event) => {
  const button = event.target.closest('[data-revision-restore]');
  if (button) restoreRevision(Number(button.dataset.revisionRestore));
});

function resetAssets() {
  assetsLoadToken += 1;
  assetConfirmResolve?.(false);
  assetUploadOp?.cancel();
  assetUploadOp = null;
  designAssets = [];
  assetsVersion = null;
  ownAssetWrites.versions = [];
  const status = document.getElementById('assetStatus');
  status.hidden = true;
  status.innerHTML = '';
  // Starts over: the last design's upload neither expired nor went away here.
  assetRowShown = null;
  renderAssetRow();
}

async function loadAssets() {
  if (!assetsEnabled()) { renderAssetRow(); return; }
  const token = ++assetsLoadToken;
  const id = workspaceId;
  try {
    const response = await fetch(assetUrl(), { cache: 'no-store' });
    if (token !== assetsLoadToken || id !== workspaceId) return;
    if (!response.ok) { renderAssetRow(); return; }
    const payload = await response.json();
    if (token !== assetsLoadToken || id !== workspaceId) return;
    designAssets = Array.isArray(payload?.assets) ? payload.assets : [];
    if (payload?.assetsVersion !== undefined) assetsVersion = payload.assetsVersion;
    if (Number(payload?.serverTime) > 0) assetClockOffsetMs = Number(payload.serverTime) - Date.now();
  } catch {
    // Assets are optional chrome; the row just stays as it was.
  }
  renderAssetRow();
}

// A poll during a write takes any change for that write. When the write then
// fails or is cancelled nobody has rendered the change (an agent's upload, or a
// cancel that landed after the server stored the file), so look now.
async function reloadAssetsAfterFailedWrite() {
  const id = workspaceId;
  const before = assetRowShown?.key;
  await loadAssets();
  if (id === workspaceId && assetRowShown?.key !== before && editor.value.trim()) render('preview');
}

function applyAssetsSummary(payload) {
  if (!assetsEnabled()) return;
  const result = assetsSummaryAction(payload, assetsVersion, ownAssetWrites);
  if (result.action === 'track') {
    assetsVersion = result.version;
  } else if (result.action === 'reload') {
    // Another tab or an MCP agent changed a mesh: same source, new geometry.
    assetsVersion = result.version;
    loadAssets();
    if (editor.value.trim()) render('preview');
    if (workspaceHasLocalChanges) scheduleWorkspaceSave();
  }
}

// Idempotent, and it writes only what changed: this runs every second while an
// upload is shown, and on every agent change, so focus stays where it is.
function renderAssetRow() {
  const section = document.getElementById('assetsSection');
  // A shared view has no design id to list against, and must not offer uploads.
  section.hidden = Boolean(readOnlyToken);
  const previous = assetRowShown;
  const now = assetNow();
  const view = assetRowView(designAssets, now);
  assetRowShown = view;
  const present = Boolean(view.upload);
  const flipped = Boolean(previous?.upload) !== present;
  const focusInside = section.contains(document.activeElement);
  if (previous?.key !== view.key) {
    // A question about a file that is gone, or was replaced, has no answer.
    assetConfirmResolve?.(false);
    // Nor does a refusal naming it, or its units hint, once it is gone. A fresh
    // upload only learning its expiry from the list is still the same file.
    const replaced = previous?.upload && (previous.counting || previous.name !== view.name);
    if (replaced && !assetUploadOp) clearAssetStatus();
  }
  const enabled = assetsEnabled();
  const upload = $('#assetUploadButton');
  upload.hidden = present;
  upload.disabled = !enabled;
  upload.title = enabled ? 'Upload an STL (or drop it on the editor or viewport)' : 'STL uploads need a design saved on the server';
  // Not `disabled`: that would drop keyboard focus for the length of the upload.
  upload.setAttribute('aria-disabled', String(!enabled || Boolean(assetUploadOp)));
  const hint = $('#assetHint');
  hint.hidden = present;
  const hintText = enabled ? 'Kept for 3 hours, for reverse engineering' : 'STL uploads need a saved design';
  if (hint.textContent !== hintText) hint.textContent = hintText;
  $('#assetCurrent').hidden = !present;
  const remove = $('#assetRemoveButton');
  remove.hidden = !present;
  $('#assetRow').classList.toggle('stressed', view.stressed);
  if (previous?.name !== view.name) {
    const name = $('#assetName');
    name.textContent = view.name;
    name.title = view.name;
    remove.setAttribute('aria-label', `Remove ${view.name}`);
    remove.title = `Remove ${view.name}`;
  }
  // Not a live region: screen readers hear the 30-minute mark and the expiry
  // through #assetAnnounce instead of every tick.
  if (previous?.countdown !== view.countdown) $('#assetCountdown').textContent = view.countdown;
  if (view.counting && !assetCountdownTimer) {
    assetCountdownTimer = setInterval(renderAssetRow, 1000);
  } else if (!view.counting && assetCountdownTimer) {
    clearInterval(assetCountdownTimer);
    assetCountdownTimer = null;
  }
  // The button under the keyboard focus just went away: hand it to its counterpart.
  if (flipped && focusInside && (!section.contains(document.activeElement) || document.activeElement.hidden)) {
    (present ? remove : upload).focus();
  }
  if (previous?.upload && previous.key !== view.key && assetExpired(previous.upload, now)) {
    $('#assetAnnounce').textContent = `${previous.name} expired`;
    log(`${previous.name} expired: uploads are kept for 3 hours.`, 'warning');
    // Whatever imports it fails now; the render's diagnostics say so.
    if (editor.value.trim()) render('preview');
  } else if (previous?.key === view.key && view.stressed && !previous.stressed) {
    $('#assetAnnounce').textContent = `${view.name} ${view.countdown}`;
  }
}

// Replaces the source the way a keystroke does: dirty, saved, then previewed.
function applyAssetCodeEdit(edit) {
  if (!edit || editor.readOnly) return false;
  if (aiUpdatedRanges.length) {
    aiUpdatedRanges = [];
    setAiBadge(false);
  }
  editor.value = edit.code;
  editor.setSelectionRange(edit.end, edit.end);
  syncEditor();
  render('preview');
  return true;
}

function insertAssetImportIntoEditor(name, { atCursor = false } = {}) {
  const cursor = atCursor ? editor.selectionStart : null;
  const edit = insertAssetImport(editor.value, name, cursor);
  if (!edit) {
    log(`${name} is already imported in the code.`, 'info');
    return false;
  }
  return applyAssetCodeEdit(edit);
}

function setAssetStatus(html, tone = 'busy') {
  const status = $('#assetStatus');
  status.hidden = false;
  status.dataset.tone = tone;
  // Progress rewrites this many times a second; Cancel stays under the keyboard.
  const focused = status.contains(document.activeElement) ? document.activeElement.dataset.assetStatus : undefined;
  status.innerHTML = html;
  if (focused) status.querySelector(`[data-asset-status="${focused}"]`)?.focus();
}

function clearAssetStatus() {
  const status = $('#assetStatus');
  status.hidden = true;
  status.innerHTML = '';
}

function showAssetProgress(name, fraction, phase) {
  const percent = Math.round(Math.max(0, Math.min(1, fraction)) * 100);
  const processing = phase === 'processing';
  setAssetStatus(
    `<div class="asset-status-line"><b>${escapeHtml(name)}</b><span>${processing ? 'Processing…' : `Uploading ${percent}%`}</span></div>`
    + `<div class="asset-progress${processing ? ' indeterminate' : ''}" role="progressbar" aria-label="${processing ? 'Processing' : 'Uploading'} ${escapeAttribute(name)}" aria-valuemin="0" aria-valuemax="100"${processing ? '' : ` aria-valuenow="${percent}"`}><i style="width: ${processing ? 100 : percent}%"></i></div>`
    + '<button type="button" class="asset-status-cancel" data-asset-status="cancel">Cancel</button>',
  );
}

// After an upload the row says it all, except that the mesh may need scaling.
function showAssetUnitsHint(result) {
  const units = result?.description?.units || {};
  const name = String(result?.name || '');
  const scale = Number(units.suggestedScale);
  if (!(units.likely && units.likely !== 'mm' && Number.isFinite(scale) && scale > 0 && scale !== 1)) {
    clearAssetStatus();
    return;
  }
  setAssetStatus(
    `<div class="asset-units"><span>Looks like <b>${escapeHtml(units.likely)}</b>${units.reason ? ` — ${escapeHtml(units.reason)}` : ''}</span>`
    + `<button type="button" data-asset-status="scale" data-asset-name="${escapeAttribute(name)}" data-scale="${escapeAttribute(formatScaleFactor(scale))}">Scale ×${escapeHtml(formatScaleFactor(scale))}</button></div>`
    + '<button type="button" class="asset-status-close" data-asset-status="dismiss" aria-label="Dismiss units hint">×</button>',
    'warning',
  );
}

function showAssetError(name, message) {
  setAssetStatus(
    `<div class="asset-status-line"><b>${escapeHtml(name)}</b><span class="asset-badge warn">failed</span></div><p class="asset-error">${escapeHtml(message)}</p>`
    + '<button type="button" class="asset-status-close" data-asset-status="dismiss" aria-label="Dismiss upload error">×</button>',
    'error',
  );
  log(`${name}: ${message}`, 'error');
}

function assetUploadErrorMessage(status, body) {
  const stated = body && typeof body === 'object' ? body.error || body.message : null;
  if (stated) return String(stated);
  if (status === 413) return 'The file is larger than the 128 MB upload limit. Large scans are decimated on the server, so a smaller export loses nothing.';
  if (status === 409) return assetRefusalMessage();
  if (status === 429) return 'Too many uploads right now. Try again in a minute.';
  if (status === 400) return 'That file is not a readable STL.';
  return status ? `Upload failed (HTTP ${status}).` : 'Upload failed: the server could not be reached.';
}

// Asked in the panel, not through a native dialog (which blocks the tab and
// looks foreign — see the recents prompt). Resolves true on confirm; Escape,
// Cancel or a second question resolve false.
function askAssetConfirm(message, confirmLabel) {
  assetConfirmResolve?.(false);
  document.getElementById('assetConfirm')?.remove();
  const returnFocus = document.activeElement;
  const prompt = document.createElement('div');
  prompt.id = 'assetConfirm';
  prompt.className = 'asset-confirm';
  prompt.setAttribute('role', 'alertdialog');
  prompt.setAttribute('aria-label', confirmLabel);
  prompt.innerHTML = '<p></p><div><button type="button" data-asset-confirm="yes"></button><button type="button" data-asset-confirm="no">Cancel</button></div>';
  prompt.querySelector('p').textContent = message;
  prompt.querySelector('[data-asset-confirm="yes"]').textContent = confirmLabel;
  $('#assetStatus').before(prompt);
  return new Promise((resolve) => {
    const finish = (answer) => {
      if (assetConfirmResolve !== finish) return;
      assetConfirmResolve = null;
      prompt.remove();
      if (returnFocus?.isConnected) returnFocus.focus?.();
      resolve(answer);
    };
    assetConfirmResolve = finish;
    prompt.addEventListener('click', (event) => {
      const button = event.target.closest('[data-asset-confirm]');
      if (button) finish(button.dataset.assetConfirm === 'yes');
    });
    prompt.addEventListener('keydown', (event) => {
      if (event.key === 'Escape') { event.stopPropagation(); finish(false); }
    });
    prompt.querySelector('[data-asset-confirm="no"]').focus();
  });
}

// Uploads one STL with progress, then imports it into the code. `atCursor`
// places the import at the editor caret (a drop on the editor) instead of the end.
async function uploadAsset(file, { atCursor = false } = {}) {
  if (readOnlyToken) { log('This is a read-only view: copy it to a new design to import meshes.', 'warning'); return false; }
  if (!assetsEnabled()) { log('Mesh uploads need a design saved on the server. Try again once it is synced.', 'warning'); return false; }
  const name = sanitizeAssetName(file.name);
  if (assetUploadOp) { log(`${name} was not uploaded: another upload is running. Cancel it first.`, 'warning'); return false; }
  // Also refused by the server, which may know of an agent's upload this tab has not seen.
  const existing = currentAssetUpload(designAssets, assetNow());
  if (existing) { showAssetError(name, assetRefusalMessage(existing.name)); return false; }
  if (!ASSET_NAME_PATTERN.test(name)) { showAssetError(file.name, 'That file name cannot be used.'); return false; }
  if (file.size > ASSET_MAX_BYTES) { showAssetError(name, `${formatBytes(file.size)} is over the 128 MB upload limit.`); return false; }
  if (name !== file.name) log(`Uploading ${file.name} as ${name}.`, 'info');
  const op = newAssetUploadOp();
  assetUploadOp = op;
  const id = workspaceId;
  const generation = workspaceGeneration;
  const current = () => id === workspaceId && generation === workspaceGeneration;
  const finishOwnWrite = beginOwnAssetWrite();
  const size = file.size;
  const chunked = size > ASSET_CHUNK_BYTES;
  // One PUT: the whole file, or the chunk [start, end) of a chunked upload.
  // Resolves { status, body }; status 0 is a network error, -1 a cancel.
  const sendPart = ({ start, end, uploadId }) => new Promise((resolve) => {
    // Cancelled, replaced or switched designs between chunks: stop.
    if (op.cancelled || !current()) { resolve({ status: -1, body: null }); return; }
    const xhr = new XMLHttpRequest();
    op.xhr = xhr;
    xhr.open('PUT', assetUrl(name));
    xhr.setRequestHeader('Content-Type', 'application/octet-stream');
    if (chunked) {
      xhr.setRequestHeader('Content-Range', `bytes ${start}-${end - 1}/${size}`);
      if (uploadId) xhr.setRequestHeader('Upload-Id', uploadId);
    }
    xhr.upload.addEventListener('progress', (event) => {
      if (current() && event.lengthComputable) showAssetProgress(name, (start + event.loaded) / size, 'upload');
    });
    // The last byte is out: the server now cleans the mesh.
    xhr.upload.addEventListener('load', () => { if (current() && end >= size) showAssetProgress(name, 1, 'processing'); });
    const settle = (status) => {
      if (op.xhr === xhr) op.xhr = null;
      let body = null;
      try { body = JSON.parse(xhr.responseText); } catch { body = null; }
      resolve({ status, body });
    };
    xhr.addEventListener('load', () => settle(xhr.status));
    xhr.addEventListener('error', () => settle(0));
    xhr.addEventListener('abort', () => settle(-1));
    xhr.send(chunked ? file.slice(start, end) : file);
  });
  showAssetProgress(name, 0, 'upload');
  renderAssetRow();
  const { status, body } = chunked
    ? await runChunkedUpload(size, ASSET_CHUNK_BYTES, sendPart, op)
    : await sendPart({ start: 0, end: size, uploadId: null });
  if (assetUploadOp === op) assetUploadOp = null;
  const ok = status >= 200 && status < 300;
  finishOwnWrite(ok ? body?.assetsVersion : undefined);
  if (!current()) return false;
  renderAssetRow();
  if (status === -1 || !ok) {
    if (status === -1) clearAssetStatus();
    else showAssetError(name, assetUploadErrorMessage(status, body));
    reloadAssetsAfterFailedWrite();
    return false;
  }
  const result = { name, ...(body || {}) };
  // Our own write: don't let the poll it wakes re-render a second time.
  if (body?.assetsVersion !== undefined) assetsVersion = body.assetsVersion;
  designAssets = [...designAssets.filter((asset) => asset.name !== result.name), result];
  renderAssetRow();
  showAssetUnitsHint(result);
  log(`${result.name} uploaded: ${formatCount(result.triangles ?? result.report?.outputTriangles)} triangles. It expires in 3 hours.`, 'success');
  for (const warning of Array.isArray(result.report?.warnings) ? result.report.warnings : []) log(`${result.name}: ${warning}`, 'warning');
  // Already imported (uploaded again after one expired): the source is unchanged,
  // so re-render for the new geometry.
  if (!insertAssetImportIntoEditor(result.name, { atCursor }) && editor.value.trim()) render('preview');
  loadAssets();
  return true;
}

// One upload's cancel handle. cancel() marks it cancelled, aborts the request
// in flight and cuts a retry backoff short; every send checks `cancelled`
// first, so nothing is sent after it.
function newAssetUploadOp() {
  const op = {
    cancelled: false,
    xhr: null,
    wake: null,
    cancel() {
      if (op.cancelled) return;
      op.cancelled = true;
      op.xhr?.abort();
      op.xhr = null;
      op.wake?.();
    },
    // Resolves after `ms`, or at once when cancelled.
    sleep(ms) {
      return new Promise((resolve) => {
        if (op.cancelled) { resolve(); return; }
        const timer = setTimeout(() => { op.wake = null; resolve(); }, ms);
        op.wake = () => { clearTimeout(timer); op.wake = null; resolve(); };
      });
    },
  };
  return op;
}

// Pure driver of a chunked upload: sends [start, end) parts of `size` bytes
// through `send({ start, end, uploadId })` -> { status, body } until a reply
// is not 202. Follows the server's `next` (also on a 409, which names where
// to resume), retries a part twice after a network error (the server
// takes a re-sent chunk as a no-op, and answers a re-sent last chunk with the
// stored reply) and up to four times after a retryable 429/503. Returns the
// last reply: 200 with the upload result, an error, or -1 when cancelled. `op` (from
// newAssetUploadOp) is checked before every part and after every backoff.
async function runChunkedUpload(size, chunkBytes, send, op = null) {
  const cancelledReply = { status: -1, body: null };
  let start = 0;
  let uploadId = null;
  let conflicts = 0;
  let retries = 0;
  let waits = 0;
  const pause = (ms) => (op ? op.sleep(ms) : new Promise((resume) => { setTimeout(resume, ms); }));
  for (;;) {
    if (op?.cancelled) return cancelledReply;
    const end = Math.min(size, start + chunkBytes);
    const reply = await send({ start, end, uploadId });
    if (op?.cancelled) return cancelledReply;
    const next = Number(reply.body?.next);
    const resumable = Number.isInteger(next) && next >= 0 && next < size && typeof reply.body?.uploadId === 'string';
    if (reply.status === 202 && resumable) {
      ({ uploadId } = reply.body);
      start = next;
      conflicts = 0;
      retries = 0;
      waits = 0;
    } else if (reply.status === 409 && resumable && conflicts < 3) {
      ({ uploadId } = reply.body);
      start = next;
      conflicts += 1;
    } else if (reply.status === 0 && retries < 2) {
      retries += 1;
      await pause(1000 * retries);
    } else if ((reply.status === 429 || reply.status === 503) && reply.body?.retryable !== false && waits < 4) {
      // Busy (rate limit, or the last chunk is still being processed by an
      // earlier send of it): wait as told and send the same part again.
      waits += 1;
      const told = Number(reply.body?.retryAfterSeconds);
      await pause(1000 * Math.min(10, Number.isFinite(told) && told > 0 ? told : 2 * waits));
    } else {
      return reply;
    }
  }
}

async function deleteAsset(name) {
  if (!assetsEnabled()) return;
  const consequences = assetReferenced(editor.value, name) ? 'It is still imported in the code; renders will fail until that import() is removed.' : '';
  const key = assetRowShown?.key;
  if (!(await askAssetConfirm(`Remove “${name}”?${consequences ? ` ${consequences}` : ''}`, 'Remove'))) return;
  const id = workspaceId;
  // DELETE goes by name: never remove a same-named upload made while this was asked.
  await loadAssets();
  if (id !== workspaceId) return;
  if (assetRowShown?.key !== key) {
    log(`${name} was not removed: the uploaded STL changed. Check the row and try again.`, 'warning');
    return;
  }
  const finishOwnWrite = beginOwnAssetWrite();
  try {
    const response = await fetch(assetUrl(name), { method: 'DELETE' });
    const body = await response.json().catch(() => null);
    finishOwnWrite(response.ok ? body?.assetsVersion : undefined);
    if (id !== workspaceId) return;
    if (!response.ok && response.status !== 404) {
      log(`Could not remove ${name}: ${body?.error || `HTTP ${response.status}`}`, 'error');
      reloadAssetsAfterFailedWrite();
      return;
    }
    if (body?.assetsVersion !== undefined) assetsVersion = body.assetsVersion;
    designAssets = designAssets.filter((asset) => asset.name !== name);
    renderAssetRow();
    log(`${name} removed.`, 'info');
    if (editor.value.trim()) render('preview');
    loadAssets();
  } catch {
    finishOwnWrite();
    log(`Could not remove ${name}: the server could not be reached.`, 'error');
    reloadAssetsAfterFailedWrite();
  }
}

function isStlFile(file) {
  return /\.stl$/i.test(file?.name || '');
}

// Opens what the picker or a drop hands over: STLs upload as assets, the first
// .scad (or other text) file replaces the source.
async function openLocalFiles(fileList, { atCursor = false } = {}) {
  const files = [...(fileList || [])];
  const meshes = files.filter(isStlFile);
  const scad = files.find((file) => !isStlFile(file));
  if (scad) await openScadFile(scad);
  for (const mesh of meshes) await uploadAsset(mesh, { atCursor: atCursor && !scad });
}

$('#assetUploadButton').addEventListener('click', (event) => {
  if (event.currentTarget.getAttribute('aria-disabled') !== 'true') $('#fileInput').click();
});
$('#assetRemoveButton').addEventListener('click', () => {
  if (assetRowShown?.name) deleteAsset(assetRowShown.name);
});
$('#assetStatus').addEventListener('click', (event) => {
  const button = event.target.closest('[data-asset-status]');
  if (!button) return;
  const action = button.dataset.assetStatus;
  if (action === 'dismiss') clearAssetStatus();
  if (action === 'cancel') assetUploadOp?.cancel();
  if (action === 'scale') {
    const edit = wrapAssetImportWithScale(editor.value, button.dataset.assetName, Number(button.dataset.scale));
    if (applyAssetCodeEdit(edit)) {
      log(`Scaled ${button.dataset.assetName} by ${button.dataset.scale}.`, 'success');
      button.disabled = true;
      button.textContent = `Scaled ×${button.dataset.scale}`;
    }
  }
});
$('#assetStatus').addEventListener('keydown', (event) => {
  if (event.key === 'Escape' && !event.target.closest('.asset-status-cancel')) clearAssetStatus();
});

// Drag-and-drop onto the editor or the viewport. A drop anywhere else is
// swallowed, so a missed target never navigates the tab away to the file.
function dragHasFiles(event) {
  return [...(event.dataTransfer?.types || [])].includes('Files');
}
for (const zone of [$('.editor-pane'), $('#viewport')]) {
  let depth = 0;
  zone.addEventListener('dragenter', (event) => {
    if (!dragHasFiles(event)) return;
    event.preventDefault();
    depth += 1;
    zone.classList.add('file-drop-target');
  });
  zone.addEventListener('dragover', (event) => {
    if (!dragHasFiles(event)) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = readOnlyToken ? 'none' : 'copy';
  });
  zone.addEventListener('dragleave', () => {
    depth = Math.max(0, depth - 1);
    if (!depth) zone.classList.remove('file-drop-target');
  });
  zone.addEventListener('drop', (event) => {
    if (!dragHasFiles(event)) return;
    event.preventDefault();
    event.stopPropagation();
    depth = 0;
    zone.classList.remove('file-drop-target');
    if (readOnlyToken) { log('This is a read-only view: copy it to a new design to import files.', 'warning'); return; }
    openLocalFiles(event.dataTransfer.files, { atCursor: zone.classList.contains('editor-pane') });
  });
}
document.addEventListener('dragover', (event) => { if (dragHasFiles(event)) event.preventDefault(); });
document.addEventListener('drop', (event) => { if (dragHasFiles(event)) event.preventDefault(); });

async function checkEngine() {
  try {
    const response = await fetch('/api/health');
    const data = await response.json();
    const pill = $('#enginePill');
    // A healthy compiler is the expected case, so it says nothing: the pill
    // appears only when rendering is actually unavailable and the user needs
    // to know why.
    pill.hidden = data.ok;
    pill.querySelector('b').textContent = data.ok ? '' : 'Compiler offline';
  } catch {
    const pill = $('#enginePill');
    pill.hidden = false;
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
      // The overlay carrying that is not up during a re-render.
      if (request.rerender) setStatus(`Waiting for a free slot — position ${status.position} of ${status.waiting}`, 'busy');
    } else if (request.queued) {
      // Admitted: hand the overlay back to the compile copy and the clock.
      request.queued = false;
      note.textContent = DEFAULT_RENDER_NOTE;
      note.hidden = true;
      label.textContent = compiling;
      if (request.rerender) setStatus(mode === 'render' ? 'Rendering' : 'Previewing', 'busy');
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

function startRenderElapsed(request) {
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

// ---- The re-render bar -------------------------------------------------------
// The overlay above stays honest for a first render, when there is nothing else to
// look at. A re-render keeps the model on screen, so a small box at the bottom is
// enough. Its percentage runs against how long the last render took, not against the
// work: it holds short of 100 until the result is here, and with no last render it
// has no number at all.

// Long enough that a fast or cached re-render never flashes the box.
const RERENDER_BAR_DELAY_MS = 250;

// Pure: how far along the bar is, or null with no estimate to run against (it is
// indeterminate). Linear to 90% at the estimate; past it, eases toward 95% and holds.
function rerenderBarFraction(elapsedMs, estimateMs) {
  if (!(estimateMs > 0)) return null;
  const t = Math.max(0, elapsedMs) / estimateMs;
  return t <= 1 ? t * 0.9 : Math.min(0.95, 0.9 + 0.05 * (1 - Math.exp(-3 * (t - 1))));
}

// Pure: the expectation one finished render leaves behind, or null when it says
// nothing about the model: a cache hit, one too quick to be real work, or one that
// spent its time resending. Waiting for a server slot is not the model's cost either.
function renderDurationSample(durationMs, data, distorted = false) {
  if (distorted || data?.cached) return null;
  const work = durationMs - (Number(data?.queuedMs) || 0);
  return work >= 150 ? work : null;
}

// Fill, number and accessible value together; null is indeterminate.
function paintRerenderBar(fraction) {
  const bar = $('#rerenderBar');
  const progress = $('#rerenderProgress');
  $('i', bar).style.transform = fraction === null ? '' : `scaleX(${fraction})`;
  $('#rerenderPercent').textContent = fraction === null ? '' : `${Math.round(fraction * 100)}%`;
  if (fraction === null) progress.removeAttribute('aria-valuenow');
  else progress.setAttribute('aria-valuenow', String(Math.round(fraction * 100)));
}

function startRerenderBar(estimateMs) {
  clearInterval(rerenderBarTimer);
  clearTimeout(rerenderBarHide);
  clearTimeout(rerenderBarShow);
  const bar = $('#rerenderBar');
  const fill = $('i', bar);
  // The clock starts when the bar shows, so it is seen to start from 0.
  const run = () => {
    if (!(estimateMs > 0)) return;
    const started = performance.now();
    rerenderBarTimer = setInterval(() => {
      paintRerenderBar(rerenderBarFraction(performance.now() - started, estimateMs));
    }, 100);
  };
  // Already up for the render this one replaced: it stays and restarts. Otherwise it
  // waits, so a re-render that is over at once never shows it.
  const waits = bar.hidden || bar.classList.contains('finishing');
  if (waits) {
    bar.hidden = true;
    rerenderBarShow = setTimeout(() => { bar.hidden = false; run(); }, RERENDER_BAR_DELAY_MS);
  }
  bar.classList.remove('finishing');
  bar.classList.toggle('indeterminate', rerenderBarFraction(0, estimateMs) === null);
  // Back to the start at once, not sliding there: this is a newer render's bar.
  fill.style.transition = 'none';
  paintRerenderBar(rerenderBarFraction(0, estimateMs));
  void fill.offsetWidth;
  fill.style.transition = '';
  if (!waits) run();
}

// The result is on screen: fills, fades, gone.
function finishRerenderBar() {
  clearInterval(rerenderBarTimer);
  clearTimeout(rerenderBarShow);
  const bar = $('#rerenderBar');
  if (bar.hidden) return;
  bar.classList.remove('indeterminate');
  paintRerenderBar(1);
  bar.classList.add('finishing');
  clearTimeout(rerenderBarHide);
  rerenderBarHide = setTimeout(() => {
    bar.hidden = true;
    bar.classList.remove('finishing');
  }, 400);
}

function hideRerenderBar() {
  clearInterval(rerenderBarTimer);
  clearTimeout(rerenderBarHide);
  clearTimeout(rerenderBarShow);
  const bar = $('#rerenderBar');
  bar.hidden = true;
  bar.classList.remove('finishing', 'indeterminate');
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
    failure.running ? 'A render was still running when this was reported from the ReOpenSCAD web app.' : 'Rendering failed in the ReOpenSCAD web app.',
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
function issueReportTitle(summary, problem = 'render failed') {
  const sentence = summary.split('. ')[0].trim() || summary;
  const short = sentence.length > 80 ? `${sentence.slice(0, 79).trimEnd()}…` : sentence;
  return `fix: ${problem} — ${short}`;
}

function issueReportUrl(failure) {
  const title = issueReportTitle(failure.summary, failure.running ? 'render stuck' : undefined);
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

async function openIssueReport(failure) {
  const { url, truncated } = issueReportUrl(failure);
  window.open(url, '_blank', 'noopener,noreferrer');
  // The trimmed body promises the full source is on the clipboard, so put it
  // there rather than leaving the reporter to notice the second button.
  if (!truncated) return;
  try {
    await navigator.clipboard.writeText(issueReportBody(failure, failure.code, false));
    log('Source too long for an issue link — the full report was copied to your clipboard. Paste it over the trimmed source in the issue.', 'warning');
  } catch {
    log(`Source too long for an issue link, so the issue carries a trimmed copy.${failure.running ? '' : ' Use "Copy report" to get the whole thing.'}`, 'warning');
  }
}

// The re-render box's report: what is running now, with the source it is compiling.
function reportRunningRender() {
  const request = activeRender;
  if (!request) return;
  openIssueReport({
    summary: `Still rendering after ${formatElapsed((performance.now() - request.startedAt) / 1000)}`,
    mode: request.mode,
    code: request.source,
    at: new Date().toISOString(),
    running: true,
  });
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
    request.distorted = true;
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
  // Whatever was waiting to render is covered by this one.
  clearTimeout(renderDebounce);
  renderDebounce = null;
  // An empty workspace has nothing to compile.
  if (!editor.value.trim()) {
    log('Nothing to render yet — write some OpenSCAD, or connect an AI agent over MCP.', 'warning');
    setStatus('Ready');
    return false;
  }
  if (activeRender) cancelRender(activeRender);
  const request = {
    controller: new AbortController(),
    id: `render-${Date.now()}-${++renderSequence}`,
    generation: workspaceGeneration,
    workspaceId,
    startedAt: performance.now(),
    mode,
    source: editor.value,
    // A model on screen stays there, in full, until this one replaces it.
    rerender: hasMesh,
  };
  activeRender = request;
  armRenderBackstop(request, mode);
  hideRenderFailure();
  showGeometryStaleness();
  renderOverlay.hidden = request.rerender;
  if (request.rerender) startRerenderBar(expectedRenderMs);
  else hideRerenderBar();
  $('#renderLabel').textContent = mode === 'render' ? 'Rendering final geometry' : 'Compiling preview';
  startRenderElapsed(request);
  startQueueWatch(request, mode);
  setStatus(mode === 'render' ? 'Rendering' : 'Previewing', 'busy');
  log(`${mode === 'render' ? 'F6' : 'F5'} — ${mode} started`, 'info');

  const superseded = () => request.controller.signal.aborted || activeRender !== request || request.generation !== workspaceGeneration || !isCurrentWorkspace(request.workspaceId);
  try {
    const sharedToken = readOnlyToken;
    const workspaceReady = sharedToken ? false : await flushWorkspaceSave();
    if (superseded()) return false;
    const useWorkspace = workspaceReady && workspaceId;
    const requestFor = () => ({
      code: (request.source = editor.value),
      mode,
      requestId: request.id,
      revision: sharedToken ? readOnlyRevision : workspaceRevision,
      projection,
      camera,
      view: [meshViewport.showEdges && 'edges', 'axes'].filter(Boolean),
      workspaceId,
      // Lets the stateless route resolve `import("mesh.stl")` against this design's assets.
      ...(workspaceId ? { designId: workspaceId } : {}),
      // A read-only view has no design id; its link token resolves the meshes.
      ...(!workspaceId && sharedToken ? { shareToken: sharedToken } : {}),
      hiddenObjectIds: sceneObjects.filter((object) => object.visible === false).map((object) => object.id),
      selectedObjectId,
    });
    let requestBody = requestFor();
    // A read-only view renders the stored design through its token: the stateless
    // `/api/render` answers one merged mesh with no parts and no colours.
    const sharedRoute = Boolean(sharedToken);
    const renderUrl = sharedRoute
      ? `/api/shared/${encodeURIComponent(sharedToken)}/render`
      : useWorkspace ? `/api/designs/${encodeURIComponent(workspaceId)}/render` : '/api/render';
    let attempt = await sendRender(renderUrl, requestBody, request, mode);
    // The shared design moved on since this view loaded it: show the new source
    // before rendering it, so the editor never disagrees with the model.
    if (sharedRoute && attempt.status === 409 && attempt.data?.currentRevision !== undefined) {
      if (!(await reloadSharedSource(sharedToken, request, superseded)) || superseded()) {
        if (superseded()) return false;
        throw new Error('This design changed and the newer revision could not be loaded. Reload the page to continue.');
      }
      log(`Rendering revision ${readOnlyRevision}, which replaced the source this ${mode} started from.`, 'info');
      request.distorted = true;
      requestBody = requestFor();
      attempt = await sendRender(renderUrl, requestBody, request, mode);
    }
    // A revision conflict means the server moved on (an agent's edit, another
    // tab). Adopt that revision and render it — never the superseded source.
    if (useWorkspace && attempt.status === 409 && attempt.data?.currentRevision !== undefined) {
      if (!(await adoptServerRevision(workspaceId, request.generation)) || superseded()) {
        if (superseded()) return false;
        throw new Error('This design changed on the server and the newer revision could not be loaded. Reload the page to continue.');
      }
      log(`Rendering revision ${workspaceRevision}, which replaced the source this ${mode} started from.`, 'info');
      request.distorted = true;
      requestBody = requestFor();
      attempt = await sendRender(renderUrl, requestBody, request, mode);
    }
    const source = requestBody.code;
    const { status, data } = attempt;
    if (superseded()) return false;
    if (status < 200 || status >= 300 || !data.ok) throw Object.assign(new Error(data.error || 'Render failed'), { console: data.console });
    // Before the list renders, so its print tags read the source these objects came from.
    if (Array.isArray(data.objects)) sceneObjectsSource = source;
    setSceneObjects(Array.isArray(data.objects) ? data.objects : sceneObjects, { placeholder: true });
    meshViewport.setMesh(data.mesh, sceneObjects);
    markGeometryCurrent();
    flushAgentPending();
    // Markers from a check of other source would point at parts that moved.
    if (intersectionCheckSource !== null && intersectionCheckSource !== source) {
      resetIntersectionResults('The source changed since the last check. Run it again.');
      renderObjectList();
      meshViewport.draw();
    }
    // New bounds, so the fit verdict on the badge is now answerable (or has changed).
    hasMesh = true;
    renderedSource = source;
    request.landed = true;
    const sample = renderDurationSample(performance.now() - request.startedAt, data, request.distorted);
    if (sample !== null) expectedRenderMs = sample;
    emptyState.hidden = true;
    log(data.console, /WARNING|DEPRECATED/.test(data.console) ? 'warning' : 'success');
    // `elapsedMs` is compute only; the server reports the queue wait apart from
    // it so a busy server is never mistaken for a slow model.
    const queued = data.queuedMs > 500 ? ` after ${formatElapsed(data.queuedMs / 1000)} queued` : '';
    // `cached`: the server had this exact revision's result stored and sent
    // it without compiling, so there is no compute time to report.
    if (data.cached) {
      log(`${mode} loaded from cache · ${data.triangles.toLocaleString()} triangles`, 'success');
      renderTime.textContent = `${mode} · cached`;
    } else {
      log(`${mode} finished in ${(data.elapsedMs / 1000).toFixed(2)} s · ${data.triangles.toLocaleString()} triangles${queued}`, 'success');
      renderTime.textContent = `${mode} · ${(data.elapsedMs / 1000).toFixed(2)} s`;
    }
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
      if (activeRender === request) {
        setStatus('Render timed out', 'error');
        if (!request.rerender) showRenderFailure(`Timed out after ${(RENDER_TIMEOUT_MS[mode] ?? RENDER_TIMEOUT_MS.preview) / 1000} s with no response from the server.`, mode);
      }
    } else if (error.name === 'AbortError') {
      // Naming the time it saved is the point of the button: it turns "I gave up" into a
      // measurable win and makes cancelling feel like a tool rather than a retreat.
      // A render replaced by a newer one (every edit during a render) was not a cancel.
      if (activeRender === request) {
        log(`Render cancelled after ${formatElapsed((request.elapsedMs || 0) / 1000)}.`, 'warning');
        setStatus('Cancelled');
      }
    } else {
      log(error.message, 'error');
      if (error.console) log(error.console, 'error');
      if (activeRender === request) {
        setStatus('Render failed', 'error');
        // A re-render keeps the last good model in view, with nothing over it: the
        // error is in the console. If the source was replaced from outside, the
        // model is drawn as out of date once this render is retired below.
        if (!request.rerender) showRenderFailure(error.message, mode);
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
      if (request.landed) finishRerenderBar();
      else hideRerenderBar();
      activeRender = null;
      showGeometryStaleness();
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

// The person's Cancel, from either loader. An edit that is still waiting for its render
// goes too, or the render they just stopped would come straight back; the next edit
// asks again. The model stays, drawn as out of date if the editor has moved past it.
// Retired here rather than in render()'s finally: a render still waiting for its save
// does not notice the abort until that save is done, which can take seconds.
function cancelRenderByUser() {
  const request = activeRender;
  if (!request) return;
  clearTimeout(renderDebounce);
  renderDebounce = null;
  cancelRender(request);
  activeRender = null;
  stopRenderElapsed();
  renderOverlay.hidden = true;
  hideRerenderBar();
  const outOfDate = hasMesh && renderedSource !== editor.value;
  if (outOfDate) markGeometryStale();
  else showGeometryStaleness();
  log(`Render cancelled after ${formatElapsed((request.elapsedMs || 0) / 1000)}.`, 'warning');
  setStatus(outOfDate ? 'Cancelled, model is out of date' : 'Cancelled');
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

// Every format the export routes accept (EXPORT_FORMATS in the backend). The
// workspace menu and each object's menu are both built from this one list.
const EXPORT_FORMATS = [
  { id: 'stl', note: 'Mesh for fabrication' },
  { id: '3mf', note: 'Slicer package with units' },
  { id: 'step', note: 'Solid B-rep for CAD' },
  { id: 'obj', note: 'Indexed mesh exchange' },
  { id: 'off', note: 'Geometry exchange' },
  { id: 'glb', note: 'glTF for web and AR' },
];

// `%` background geometry is drawn, never exported, and the backend rejects ids
// it did not compile, so only real parts are ever named.
function exportableObjects() {
  return sceneObjects.filter((object) => !object.placeholder && !object.background);
}

// The workspace export ('all') names nothing: the server's default is every
// top-level object, each its own body in one file, `%` background excluded.
// That needs no object list at all, so it can never be stale. Per-object
// exports name an id, which only means something for the source it was
// compiled from — see `sceneObjectsCurrent`.
function exportObjectIds(scope) {
  if (scope === 'selected') return selectedObjectId ? [selectedObjectId] : null;
  if (scope && typeof scope === 'object' && scope.objectId) return [scope.objectId];
  return null;
}

// True when the object list was compiled from exactly the source in the editor.
function sceneObjectsCurrent() {
  return sceneObjectsSource !== null
    && sceneObjectsSource === editor.value
    && !sceneObjects.some((object) => object.placeholder);
}

function exportFileSuffix(scope) {
  if (scope === 'selected') return '-selected';
  if (scope && typeof scope === 'object' && scope.objectId) {
    const object = sceneObjects.find((item) => item.id === scope.objectId);
    const slug = String(object?.name || scope.objectId).toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '');
    return `-${slug || 'object'}`;
  }
  return '';
}

// Export progress and failures as a toast over the viewport. The console and the
// status bar already carry them on a desktop, but a phone hides both, so without
// this an export there either downloads or silently does nothing.
let exportToastTimer = 0;
let exportToastToken = 0;
function showExportToast(message, { tone = 'busy', retry = null } = {}) {
  let toast = document.getElementById('exportToast');
  if (!toast) {
    toast = document.createElement('div');
    toast.id = 'exportToast';
    toast.className = 'export-toast';
    toast.innerHTML = '<span class="export-toast-message"></span><button type="button" class="export-toast-retry">Retry</button><button type="button" class="export-toast-close" aria-label="Dismiss">×</button>';
    toast.querySelector('.export-toast-close').addEventListener('click', hideExportToast);
    document.body.append(toast);
  }
  clearTimeout(exportToastTimer);
  toast.hidden = false;
  toast.dataset.tone = tone;
  // An error interrupts; progress and success only announce.
  toast.setAttribute('role', tone === 'error' ? 'alert' : 'status');
  toast.querySelector('.export-toast-message').textContent = message;
  const retryButton = toast.querySelector('.export-toast-retry');
  retryButton.hidden = !retry;
  retryButton.onclick = retry ? () => { hideExportToast(); retry(); } : null;
  if (tone === 'success') exportToastTimer = setTimeout(hideExportToast, 2500);
  exportToastToken += 1;
  return exportToastToken;
}
function hideExportToast() {
  clearTimeout(exportToastTimer);
  const toast = document.getElementById('exportToast');
  if (toast) toast.hidden = true;
}
// An export abandoned by a navigation must not leave "Exporting…" up for good.
function settleExportToast(token) {
  const toast = document.getElementById('exportToast');
  if (token === exportToastToken && toast?.dataset.tone === 'busy') hideExportToast();
}

async function exportModel(format, scope = 'all') {
  const generation = workspaceGeneration;
  const id = workspaceId;
  const objectIds = exportObjectIds(scope);
  if (scope !== 'all') {
    if (!objectIds) {
      log('Nothing to export: no object is selected.', 'warning');
      showExportToast('Nothing to export: no object is selected.', { tone: 'warning' });
      return;
    }
    // The ids came from the last render. If the source moved since, recompile
    // first so the id names the part as it exists now.
    if (!sceneObjectsCurrent()) {
      log('Source changed since the last preview; refreshing objects before export.', 'info');
      const rendered = await render('preview');
      if (generation !== workspaceGeneration || !isCurrentWorkspace(id)) return;
      if (!rendered) {
        log('Export cancelled: the model has to preview successfully before one object can be exported.', 'warning');
        showExportToast('Export cancelled: the model has to preview successfully first.', { tone: 'error', retry: () => exportModel(format, scope) });
        return;
      }
    }
    if (!exportableObjects().some((object) => object.id === objectIds[0])) {
      log('That object is no longer in the model after the latest change. Pick it again from Objects.', 'warning');
      showExportToast('That object is no longer in the model. Pick it again from Objects.', { tone: 'warning' });
      return;
    }
  }
  if (generation !== workspaceGeneration || !isCurrentWorkspace(id)) return;
  const label = format.toUpperCase();
  setStatus(`Exporting ${label}`, 'busy');
  log(`Exporting final geometry as ${label}…`);
  let toastToken = showExportToast(`Exporting ${label}…`);
  try {
    const workspaceReady = await flushWorkspaceSave();
    if (generation !== workspaceGeneration || !isCurrentWorkspace(id)) return;
    const useWorkspace = workspaceReady && workspaceId;
    const exportUrl = useWorkspace ? `/api/designs/${encodeURIComponent(workspaceId)}/export` : '/api/export';
    let requestBody = {
      code: editor.value,
      format,
      revision: workspaceRevision,
      ...(workspaceId ? { designId: workspaceId } : {}),
      ...(!workspaceId && readOnlyToken ? { shareToken: readOnlyToken } : {}),
      ...(objectIds ? { objectIds } : {}),
    };
    // A throttled export is retried like a throttled render: the request was
    // never evaluated, and asking for the bodies right after a refresh preview
    // is exactly when the per-client budget runs low.
    const postExport = async (url) => {
      for (let attempt = 0; ; attempt += 1) {
        const response = await fetch(url, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(requestBody),
        });
        if (response.status !== 429 || attempt >= QUEUE_RETRY_LIMIT) return response;
        const data = await response.clone().json().catch(() => null);
        if (!data?.retryable) return response;
        const wait = Math.min(Number(response.headers.get('Retry-After')) || data.retryAfterSeconds || 5, 30);
        log(`Server at capacity. Retrying export in ${wait} s (attempt ${attempt + 2} of ${QUEUE_RETRY_LIMIT + 1}).`, 'warning');
        toastToken = showExportToast(`Server busy. Retrying ${label} export in ${wait} s…`);
        await delay(wait * 1000);
        if (generation !== workspaceGeneration || !isCurrentWorkspace(id)) return response;
      }
    };
    let response = await postExport(exportUrl);
    // The server holds a newer revision than this export was bound to. Exporting
    // the old source statelessly would hand the user a file of code that is no
    // longer the workspace's; adopt the newer revision and export that instead.
    if (useWorkspace && response.status === 409) {
      const conflict = await response.clone().json().catch(() => null);
      if (conflict?.currentRevision !== undefined) {
        if (!(await adoptServerRevision(workspaceId, generation))) {
          if (generation !== workspaceGeneration || !isCurrentWorkspace(id)) return;
          throw new Error('This design changed on the server and the newer revision could not be loaded, so nothing was exported. Reload the page and export again.');
        }
        if (generation !== workspaceGeneration || !isCurrentWorkspace(id)) return;
        log(`The design moved to revision ${workspaceRevision} on the server; exporting that revision.`, 'info');
        // A per-object export names an id from the old source: recompile so the
        // id is checked against the parts of the revision being exported.
        // The model on screen is the superseded revision's: bring it up to date.
        if (!objectIds) scheduleSourceRender();
        if (objectIds) {
          const rendered = await render('preview');
          if (generation !== workspaceGeneration || !isCurrentWorkspace(id)) return;
          if (!rendered || !exportableObjects().some((object) => object.id === objectIds[0])) {
            throw new Error('That object is not in the newer revision. Pick it again from Objects.');
          }
        }
        requestBody = { ...requestBody, code: editor.value, revision: workspaceRevision };
        response = await postExport(exportUrl);
      }
    }
    if (generation !== workspaceGeneration || !isCurrentWorkspace(id)) return;
    if (!response.ok) {
      const data = await response.json();
      throw Object.assign(new Error(data.error), { console: data.console });
    }
    downloadBlob(await response.blob(), `${downloadBaseName}${exportFileSuffix(scope)}.${format}`);
    const what = objectIds ? '1 object' : 'all objects, one body each';
    log(`${label} export complete (${what}).`, 'success');
    showExportToast(`${label} downloaded.`, { tone: 'success' });
    setStatus('Ready');
  } catch (error) {
    if (generation !== workspaceGeneration || !isCurrentWorkspace(id)) return;
    log(error.console || error.message, 'error');
    showExportToast(`${label} export failed: ${error.message || 'unknown error'}`, { tone: 'error', retry: () => exportModel(format, scope) });
    setStatus('Export failed', 'error');
  } finally {
    settleExportToast(toastToken);
  }
}

// An example opens as a workspace of its own, seeded with the example's source:
// the landing page has no editor to load it into, and a template that edits
// someone's existing workspace would be the wrong default anyway.
async function loadExample(slug) {
  const card = $(`[data-example="${CSS.escape(slug)}"]`);
  if (card?.getAttribute('aria-busy') === 'true') return;
  card?.setAttribute('aria-busy', 'true');
  let code;
  try {
    const response = await fetch(`${EXAMPLES_BASE}${encodeURIComponent(slug)}.scad`);
    if (!response.ok) throw new Error(`example ${response.status}`);
    code = await response.text();
  } catch {
    // The console pane is where every other failure is reported, so this one
    // goes there too rather than into an alert the user has to dismiss.
    log(`Could not open "${slug}". It may not be deployed with this build.`, 'error');
    card?.removeAttribute('aria-busy');
    return;
  }
  try {
    await startNewWorkspace(code);
  } finally {
    card?.removeAttribute('aria-busy');
  }
  editor.focus();
  editor.setSelectionRange(0, 0);
  editor.scrollTop = 0;
  updateCursor();
  render('preview');
}

// "6 x 2 mm" must not break into "6 x 2" / "mm" at a line end: a number keeps
// its unit, and a dimension keeps its "x"/"×" and both factors.
function keepUnitsTogether(text) {
  return String(text)
    .replace(/(\d)\s+([x×])\s+(?=\d)/g, '$1\u00a0$2\u00a0')
    .replace(/(\d)\s+(mm|cm|m|°)(?=$|[\s.,;:)])/g, '$1\u00a0$2');
}

// A card's feature hint is a few calls joined by middle dots. Each call is one
// unbreakable token and the dot rides at the start of the token after it, so a
// wrap can only ever happen between calls — never leaving a dot dangling.
function exampleHintHtml(hint) {
  return String(hint)
    .split(/\s*·\s*/)
    .filter(Boolean)
    .map((call, index) => `<span>${index ? '· ' : ''}${escapeHtml(call)}</span>`)
    .join(' ');
}

// The examples on the landing page, grouped by what the model *is*. Built once:
// the catalogue is static, unlike the recents list above it.
async function renderLandingExamples() {
  const host = $('#landingExamples');
  if (host.dataset.rendered === '1') return;
  host.dataset.rendered = '1';
  const manifest = await loadCatalogue();
  if (!manifest) {
    host.innerHTML = '<p class="example-empty">The example catalogue is not available in this build.</p>';
    return;
  }
  host.innerHTML = manifest.categories
    .map((category) => {
      const cards = category.examples
        .map((example) => {
          const slug = escapeAttribute(example.slug);
          // Eager, not lazy: the landing page is its own scroll container, and a
          // lazy image below the fold there could stay an empty placeholder.
          return `<button type="button" class="example-card" role="listitem" data-example="${slug}">`
            + `<span class="example-shot"><img src="${EXAMPLES_BASE}${slug}.png" alt="" decoding="async" onerror="this.dataset.failed='1'" /></span>`
            + `<span class="example-copy"><b>${escapeHtml(keepUnitsTogether(example.name))}</b>`
            + `<small>${escapeHtml(keepUnitsTogether(example.blurb))}</small></span>`
            + `<code>${exampleHintHtml(example.hint)}</code></button>`;
        })
        .join('');
      return `<section class="example-group"><h3>${escapeHtml(category.name)}</h3>`
        + `<p>${escapeHtml(keepUnitsTogether(category.blurb))}</p>`
        + `<div class="example-grid" role="list" data-count="${category.examples.length}">${cards}</div></section>`;
    })
    .join('');
}

// An empty editor has nothing to preview, so the viewport's offer to preview it
// is a button that can only produce an error. The heading beside it still reads
// as an invitation, which is the right thing for an empty workspace to say.
function updateEmptyState() {
  const empty = editor.value.trim().length === 0;
  $('#firstPreview').hidden = empty;
  $('#emptyHint').hidden = empty;
  $('#emptyMobileHint').hidden = !empty;
}

editor.addEventListener('input', () => {
  if (aiUpdatedRanges.length) {
    aiUpdatedRanges = [];
    setAiBadge(false);
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

$('#firstPreview').addEventListener('click', () => render('preview'));
// The same abort as the toolbar's stop icon, but where the user is actually looking during
// a long render: on the overlay that is covering the viewport.
$('#overlayCancel').addEventListener('click', cancelRenderByUser);
$('#rerenderCancel').addEventListener('click', cancelRenderByUser);
$('#rerenderReport').addEventListener('click', reportRunningRender);
$('#dismissRenderFailure').addEventListener('click', hideRenderFailure);
$('#reportRenderFailure').addEventListener('click', () => {
  if (lastRenderFailure) openIssueReport(lastRenderFailure);
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
// Everything on screen that belongs to the workspace being left: the mesh, the
// object list, the AI highlight. Without it, switching workspaces left the
// previous model behind the next one's empty editor.
function resetScene() {
  // The next design's first mesh frames itself; renders after that keep the camera.
  meshViewport.framed = false;
  // A section cut belongs to the design it was set on: recompiles keep it, but a
  // different design starts uncut, on the default axis.
  Object.assign(meshViewport.slice, { enabled: false, axis: 'z', position: NaN, flip: false });
  aiUpdatedRanges = [];
  setAiBadge(false);
  resetAgentActivity();
  clearGeometry();
  syncSlicerControls();
}

function clearGeometry() {
  // Whatever comes next is not this model's re-render.
  expectedRenderMs = null;
  renderedSource = null;
  sceneObjects = [];
  sceneObjectsSource = null;
  selectedObjectId = null;
  resetIntersectionResults();
  markGeometryCurrent();
  renderObjectList();
  hasMesh = false;
  meshViewport.clearMesh();
  emptyState.hidden = false;
  hideRenderFailure();
}

// Leaving a workspace (New, Back, a recent) must not strand a debounced edit:
// the generation bump that follows aborts any save still queued. Bounded, so a
// dead network never holds the navigation hostage — the dirty local cache
// still resumes the edit next time the workspace opens.
async function leaveCurrentWorkspace() {
  if (!workspaceId || !hasPendingLocalWorkspaceChanges()) return;
  await Promise.race([flushWorkspaceSave(), delay(2000)]);
}

async function startNewWorkspace(code = '') {
  await leaveCurrentWorkspace();
  // Push, so the page the user came from (a workspace or the landing page)
  // stays reachable with the Back button.
  await createWorkspace(code, { navigation: 'push' });
  resetScene();
}

$('#newButton').addEventListener('click', () => startNewWorkspace(''));
async function openScadFile(file) {
  editor.value = await file.text();
  setDownloadBaseName(file.name);
  syncEditor(false);
  scheduleWorkspaceSave();
  dirtyIndicator.classList.remove('visible');
  log(`${file.name} opened.`, 'success');
  render('preview');
}
$('#fileInput').addEventListener('change', async (event) => {
  const files = [...event.target.files];
  // Cleared, so picking the same file again (a re-exported scan) still fires.
  event.target.value = '';
  if (!files.length) return;
  await openLocalFiles(files);
});
$$('[data-projection]').forEach((button) => button.addEventListener('click', () => {
  projection = button.dataset.projection;
  $$('[data-projection]').forEach((item) => item.setAttribute('aria-pressed', String(item === button)));
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
  meshViewport.fitView();
  meshViewport.draw();
});
$('#zoomIn').addEventListener('click', () => { camera[6] = Math.max(meshViewport.radius * 0.02, camera[6] * .82); meshViewport.syncOrthoScale(); meshViewport.draw(); });
$('#zoomOut').addEventListener('click', () => { camera[6] = Math.min(meshViewport.radius * 30, camera[6] * 1.2); meshViewport.syncOrthoScale(); meshViewport.draw(); });
$('#edgesToggle').addEventListener('click', (event) => { meshViewport.showEdges = !meshViewport.showEdges; event.currentTarget.setAttribute('aria-pressed', String(meshViewport.showEdges)); meshViewport.draw(); });

// Section view. The cut itself lives on meshViewport.slice (it has to outlive every
// re-render); these controls only mirror and edit it. Toggling never touches the camera.
const slicerPosition = $('#slicerPosition');
function syncSlicerControls() {
  const { slice } = meshViewport;
  const range = sliceRange(meshViewport.bounds, slice.axis);
  const toggle = $('#slicerButton');
  toggle.setAttribute('aria-pressed', String(slice.enabled));
  $('#slicerPanel').hidden = !slice.enabled;
  $$('[data-slice-axis]').forEach((button) => button.setAttribute('aria-checked', String(button.dataset.sliceAxis === slice.axis)));
  $('#slicerFlip').setAttribute('aria-pressed', String(slice.flip));
  slicerPosition.disabled = !range;
  if (range) {
    // min/max/step before value: the browser snaps the value against whatever is set.
    slicerPosition.min = String(range.min);
    slicerPosition.max = String(range.max);
    slicerPosition.step = String(range.step);
    slicerPosition.value = String(slice.position);
  }
  $('#slicerValue').textContent = range && Number.isFinite(slice.position) ? `${formatMillimetres(slice.position)} mm` : '—';
}
function updateSlice(changes) {
  const slice = Object.assign(meshViewport.slice, changes);
  slice.position = clampSlicePosition(slice.position, sliceRange(meshViewport.bounds, slice.axis));
  syncSlicerControls();
  meshViewport.draw();
}
meshViewport.onBoundsChange = syncSlicerControls;
$('#slicerButton').addEventListener('click', () => updateSlice({ enabled: !meshViewport.slice.enabled }));
// A new axis has an unrelated range, so the cut restarts mid-model on it.
$$('[data-slice-axis]').forEach((button) => button.addEventListener('click', () => {
  if (button.dataset.sliceAxis !== meshViewport.slice.axis) updateSlice({ axis: button.dataset.sliceAxis, position: NaN });
}));
slicerPosition.addEventListener('input', () => updateSlice({ position: Number(slicerPosition.value) }));
$('#slicerFlip').addEventListener('click', () => updateSlice({ flip: !meshViewport.slice.flip }));
syncSlicerControls();

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
  camera[6] = Math.max(meshViewport.radius * 0.02, Math.min(meshViewport.radius * 30, camera[6] * (event.deltaY > 0 ? 1.1 : .9)));
  meshViewport.syncOrthoScale();
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
  // Fingers have their own gesture set (below): one orbits, two pinch and pan.
  if (event.pointerType === 'touch') { beginTouchGesture(event); return; }
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
  if (event.pointerType === 'touch') { moveTouchGesture(event); return; }
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

// --- Touch gestures ----------------------------------------------------------
//
// One finger orbits exactly as a left drag does. Two fingers pinch to zoom and
// move together to pan, both at once, the way every map and model viewer on a
// phone behaves. Each change in the number of fingers re-anchors the gesture at
// the current camera, so lifting one finger of a pinch carries straight on as an
// orbit instead of jumping. A third finger is ignored rather than re-anchoring.
const touchPoints = new Map();
let touchGesture = null;

// Centroid and spread of the fingers, in client pixels.
function touchFrame(points) {
  const x = points.reduce((sum, point) => sum + point.x, 0) / points.length;
  const y = points.reduce((sum, point) => sum + point.y, 0) / points.length;
  const spread = points.length < 2 ? 0 : Math.hypot(points[0].x - points[1].x, points[0].y - points[1].y);
  return { x, y, spread };
}

// Spreading the fingers apart moves the camera in, in proportion, within the
// same distance bounds the scroll wheel keeps.
function pinchDistance(startDistance, startSpread, spread, radius) {
  if (!(startSpread > 0) || !(spread > 0)) return startDistance;
  return Math.max(radius * 0.02, Math.min(radius * 30, startDistance * startSpread / spread));
}

function anchorTouchGesture() {
  const points = [...touchPoints.values()].slice(0, 2);
  if (!points.length) { touchGesture = null; return; }
  touchGesture = {
    fingers: points.length,
    frame: touchFrame(points),
    rx: camera[3],
    rz: camera[5],
    distance: camera[6],
    offset: [camera[0], camera[1], camera[2]],
  };
}

function beginTouchGesture(event) {
  if (touchPoints.size >= 2) return;
  try {
    renderCanvas.setPointerCapture(event.pointerId);
  } catch { return; }
  event.preventDefault();
  touchPoints.set(event.pointerId, { x: event.clientX, y: event.clientY });
  anchorTouchGesture();
}

function moveTouchGesture(event) {
  if (!touchPoints.has(event.pointerId) || !touchGesture) return;
  touchPoints.set(event.pointerId, { x: event.clientX, y: event.clientY });
  const points = [...touchPoints.values()].slice(0, 2);
  const frame = touchFrame(points);
  const start = touchGesture;
  if (start.fingers === 1) {
    camera[3] = Math.max(-89.5, Math.min(89.5, start.rx + (frame.y - start.frame.y) * .45));
    camera[5] = start.rz - (frame.x - start.frame.x) * .45;
    meshViewport.draw();
    return;
  }
  camera[6] = pinchDistance(start.distance, start.frame.spread, frame.spread, meshViewport.radius);
  meshViewport.syncOrthoScale();
  // panView draws, after the zoom above, so a pinch and a pan land in one frame.
  panView(start.offset, frame.x - start.frame.x, frame.y - start.frame.y);
}

function endTouchGesture(event) {
  if (event.pointerType !== 'touch' || !touchPoints.delete(event.pointerId)) return;
  anchorTouchGesture();
}
renderCanvas.addEventListener('pointerup', endTouchGesture);
renderCanvas.addEventListener('pointercancel', endTouchGesture);
renderCanvas.addEventListener('lostpointercapture', endTouchGesture);

$('#clearConsole').addEventListener('click', () => {
  consoleOutput.innerHTML = '';
  // Agent calls cleared with the rest stay cleared (see renderAgentFeed).
  agentRowsClearedSeq = Math.max(agentRowsClearedSeq, ...agentActivity.entries.keys());
});
$('#collapseConsole').addEventListener('click', () => $('#consolePane').classList.toggle('collapsed'));

const exportPopover = $('#exportPopover');
// Left-aligned to the button, and kept inside the window by the popover's own
// measured width (it is wider than its min-width once padding and border count).
function placeExportPopover(anchor) {
  const rect = anchor.getBoundingClientRect();
  const width = exportPopover.offsetWidth;
  exportPopover.style.left = `${Math.max(8, Math.min(rect.left, innerWidth - width - 8))}px`;
  exportPopover.style.top = `${rect.bottom + 5}px`;
}

$('#exportButton').addEventListener('click', (event) => {
  exportPopover.hidden = !exportPopover.hidden;
  if (!exportPopover.hidden) placeExportPopover(event.currentTarget);
});
$$('[data-format]', exportPopover).forEach((button) => button.addEventListener('click', () => {
  exportPopover.hidden = true;
  exportModel(button.dataset.format, 'all');
}));
$('#exportPng').addEventListener('click', async () => {
  exportPopover.hidden = true;
  // No mesh yet, the one on screen is about to be replaced by an edit's render, or its
  // render was cancelled and it is out of date.
  if ((!hasMesh || geometryStale || renderDebounce !== null || activeRender) && !(await render('render'))) return;
  meshViewport.downloadPng(downloadFileName('png'));
});

document.addEventListener('pointerdown', (event) => {
  if (!event.target.closest('#exportButton') && !event.target.closest('.export-popover')) exportPopover.hidden = true;
  if (!event.target.closest('.object-export') && !event.target.closest('#objectExportPopover')) closeObjectExportMenu();
  if (!event.target.closest('#syncState') && !event.target.closest('#workspaceConflictResolver')) {
    document.getElementById('workspaceConflictResolver')?.remove();
  }
});

// Per-object export: the same formats as the toolbar's menu, for one part.
const objectExportPopover = $('#objectExportPopover');
$('#objectExportFormats').innerHTML = EXPORT_FORMATS
  .map((format) => `<button type="button" role="menuitem" data-object-format="${format.id}"><span>${format.id.toUpperCase()}</span><small>${escapeHtml(format.note)}</small></button>`)
  .join('');

function closeObjectExportMenu() {
  objectExportPopover.hidden = true;
  delete objectExportPopover.dataset.objectId;
}

function openObjectExportMenu(object, anchor) {
  if (!objectExportPopover.hidden && objectExportPopover.dataset.objectId === object.id) {
    closeObjectExportMenu();
    return;
  }
  exportPopover.hidden = true;
  objectExportPopover.dataset.objectId = object.id;
  $('#objectExportTitle').textContent = `Export ${object.name}`;
  $('#objectExportHint').hidden = sceneObjectsCurrent();
  objectExportPopover.hidden = false;
  const rect = anchor.getBoundingClientRect();
  const height = objectExportPopover.offsetHeight;
  objectExportPopover.style.left = `${Math.min(rect.right + 6, innerWidth - objectExportPopover.offsetWidth - 8)}px`;
  objectExportPopover.style.top = `${Math.max(8, Math.min(rect.top - 8, innerHeight - height - 8))}px`;
  objectExportPopover.querySelector('button')?.focus();
}

objectExportPopover.addEventListener('click', (event) => {
  const button = event.target.closest('[data-object-format]');
  const objectId = objectExportPopover.dataset.objectId;
  if (!button || !objectId) return;
  closeObjectExportMenu();
  exportModel(button.dataset.objectFormat, { objectId });
});
document.addEventListener('keydown', (event) => {
  if (event.key === 'Escape' && !objectExportPopover.hidden) closeObjectExportMenu();
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
  return `${location.origin}/designs/${encodeURIComponent(workspaceId || '')}`;
}

async function ensureShareToken() {
  // A workspace the server has not seen yet cannot be shared, and the first
  // thing anyone does is type and immediately hit Share — so land the pending
  // save before asking rather than reporting "unavailable" at them.
  // Source the server refused stays unsaved, so a link now would show something else.
  const saved = await flushWorkspaceSave();
  if (!workspaceId || !saved) return null;
  const id = workspaceId;
  if (shareTokenCache.id === id && shareTokenCache.promise) return shareTokenCache.promise;
  const promise = fetch(`/api/designs/${encodeURIComponent(id)}/share`, {
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
  sharePopover.style.left = `${Math.max(12, Math.min(rect.left, innerWidth - 312))}px`;
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
    view.placeholder = 'Unavailable — this design is not saved on the server.';
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
  const exportButton = event.target.closest('.object-export');
  if (exportButton) {
    openObjectExportMenu(object, exportButton);
    return;
  }
  if (event.target.closest('.visibility-toggle')) {
    object.visible = !object.visible;
    renderObjectList();
    meshViewport.draw();
    log(`${object.name} ${object.visible ? 'shown' : 'hidden'}.`, 'info');
    return;
  }
  selectObject(object.id);
});

// Show all / Hide all in the Objects header: the same per-object flag the eye
// buttons flip, set on every object at once.
function setAllObjectsVisible(visible) {
  if (!sceneObjects.length) return;
  for (const object of sceneObjects) object.visible = visible;
  renderObjectList();
  meshViewport.draw();
  log(`All objects ${visible ? 'shown' : 'hidden'}.`, 'info');
}
$('#showAllObjects').innerHTML = icons.eye;
$('#hideAllObjects').innerHTML = icons.eyeOff;
$('#showAllObjects').addEventListener('click', () => setAllObjectsVisible(true));
$('#hideAllObjects').addEventListener('click', () => setAllObjectsVisible(false));

// --- Phone layout ------------------------------------------------------------
//
// The same query as the phone block in styles.css. Below it there is no editor:
// the viewport fills the screen and the object list opens as a bottom sheet
// from a button on the bottom toolbar.
const MOBILE_MEDIA = '(max-width: 760px) and (pointer: coarse), (pointer: coarse) and (max-height: 500px)';
const mobileLayout = window.matchMedia(MOBILE_MEDIA);

function openMobileSheet(name) {
  document.body.dataset.mobileSheet = name;
  $('#mobileSheetScrim').hidden = false;
  $$('[data-sheet]').forEach((button) => button.setAttribute('aria-expanded', String(button.dataset.sheet === name)));
}

function closeMobileSheet() {
  delete document.body.dataset.mobileSheet;
  $('#mobileSheetScrim').hidden = true;
  $$('[data-sheet]').forEach((button) => button.setAttribute('aria-expanded', 'false'));
}

$$('[data-sheet]').forEach((button) => button.addEventListener('click', () => {
  if (document.body.dataset.mobileSheet === button.dataset.sheet) closeMobileSheet();
  else openMobileSheet(button.dataset.sheet);
}));
$$('[data-close-sheet]').forEach((button) => button.addEventListener('click', closeMobileSheet));
$('#mobileSheetScrim').addEventListener('click', closeMobileSheet);
document.addEventListener('keydown', (event) => {
  if (event.key === 'Escape' && document.body.dataset.mobileSheet) closeMobileSheet();
});
// Widening past the breakpoint (a rotated tablet, a resized window) puts the
// panels back beside the viewport; a sheet left "open" would mean nothing there.
mobileLayout.addEventListener('change', () => {
  if (!mobileLayout.matches) closeMobileSheet();
  else forceMobileProjection();
});
// Phones have no projection tabs, so they always view in the default perspective.
function forceMobileProjection() {
  if (mobileLayout.matches && projection !== 'perspective') $('[data-projection="perspective"]').click();
}
forceMobileProjection();

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

// What one pair from `/intersections` means for the panel. The server says it in
// `relation`; older replies only carried `intersects`/`withinTolerance`, so those
// are mapped onto the same vocabulary. A gap that is merely too small is a
// clearance problem, never an "overlap": calling a 0.1 mm gap a "0.000 mm
// overlap" told the user their parts collide when they do not.
function classifyIntersection(item, tolerance) {
  const finite = (value) => value !== null && value !== undefined && value !== '' && Number.isFinite(Number(value));
  const mm = (value) => `${Number(value).toFixed(2)} mm`;
  const relation = item?.relation
    ?? (item?.skipped ? 'skipped' : item?.intersects === true ? 'overlap' : item?.withinTolerance ? 'clearanceViolation' : 'clear');
  if (relation === 'skipped' || item?.skipped === true) {
    return { kind: 'unresolved', text: 'Not checked: the check ran out of time before this pair' };
  }
  if (relation === 'undetermined' || item?.determined === false) {
    return { kind: 'unresolved', text: `Undetermined: could not prove a gap of at least ${mm(tolerance)}` };
  }
  if (relation === 'overlap') {
    const volume = finite(item.overlapVolume) ? Number(item.overlapVolume) : null;
    const depth = finite(item.depth) ? Number(item.depth) : null;
    // Surfaces that cross without enclosing any volume are touching, not overlapping.
    if (volume !== null && volume <= 0) {
      return { kind: 'clearance', text: `touching: gap 0.00 mm < ${mm(tolerance)} tolerance` };
    }
    const parts = [volume !== null ? `overlap ${volume < 0.01 ? volume.toExponential(1) : volume.toFixed(volume < 10 ? 3 : 1)} mm³` : 'overlap'];
    if (depth !== null && depth > 0) parts.push(`${depth.toFixed(2)} mm deep`);
    return { kind: 'overlap', text: parts.join(' · ') };
  }
  if (relation === 'contact') {
    if (item.contactAllowed === true || item.toleranceViolated === false) return { kind: 'clear', text: '' };
    return { kind: 'clearance', text: `touching: gap 0.00 mm < ${mm(tolerance)} tolerance` };
  }
  if (relation === 'clearanceViolation') {
    if (!finite(item.clearance)) return { kind: 'clearance', text: `gap below the ${mm(tolerance)} tolerance` };
    const gap = Number(item.clearance);
    // Two decimals unless they would round the gap up to the tolerance itself.
    const shown = gap.toFixed(2) === Number(tolerance).toFixed(2) ? `${gap.toFixed(3)} mm` : mm(gap);
    return { kind: 'clearance', text: `gap ${shown} < ${mm(tolerance)} tolerance` };
  }
  return { kind: 'clear', text: '' };
}

// The verdict for a whole check. "Clear" is only ever said about a check that
// actually decided every pair: skipped, undetermined or an incomplete reply is
// "Incomplete", because a pair nobody measured is not a pair that fits.
function summarizeIntersections(data, tolerance) {
  const pairs = Array.isArray(data?.intersections) ? data.intersections : [];
  const findings = pairs
    .map((item) => ({ item, ...classifyIntersection(item, tolerance) }))
    .filter((finding) => finding.kind !== 'clear');
  const problems = findings.filter((finding) => finding.kind !== 'unresolved');
  const unresolved = findings.filter((finding) => finding.kind === 'unresolved');
  const incomplete = unresolved.length > 0 || data?.complete === false || data?.summary?.complete === false;
  const state = problems.length ? 'collision' : incomplete ? 'incomplete' : 'clear';
  return { pairs: pairs.length, findings, problems, unresolved, incomplete, state };
}

function intersectionPoint(value) {
  return Array.isArray(value) && value.length === 3 && value.every((entry) => Number.isFinite(Number(entry)))
    ? value.map(Number)
    : null;
}

// The source the last check measured; a render of anything else retires its markers.
let intersectionCheckSource = null;
// The check on its way. While there is one, Check is Cancel.
let intersectionCheck = null;

function resetIntersectionResults(message = '') {
  intersectionCheckSource = null;
  intersectionIds = new Set();
  meshViewport.markers = [];
  meshViewport.activeMarker = -1;
  meshViewport.markerFade = 1;
  agentActivity.markersSince = null;
  const state = $('#inspectionState');
  state.className = 'inspection-state';
  state.textContent = 'Not checked';
  $('#intersectionResults').innerHTML = message && !intersectionCheck ? `<div class="inspection-empty">${escapeHtml(message)}</div>` : '';
  // A preview landing in the middle of a check (it compiles first) does not end that check.
  if (intersectionCheck) showIntersectionProgress();
}

// Says so from the moment of the press, with the time running so it never reads as stuck.
function showIntersectionProgress() {
  const state = $('#inspectionState');
  const seconds = Math.floor((performance.now() - intersectionCheck.startedAt) / 1000);
  state.className = 'inspection-state running';
  state.textContent = `Checking… ${seconds} s`;
  if (!$('#intersectionResults').textContent) $('#intersectionResults').innerHTML = '<div class="inspection-empty">Measuring visible parts…</div>';
}

// Fills the inspection panel and the viewport markers from an intersection reply.
// `agent` marks findings an AI agent's check_intersections or evaluate produced,
// shown the same way as the user's own check.
function showIntersectionResults(data, tolerance, source, { agent = false } = {}) {
  const results = $('#intersectionResults');
  const state = $('#inspectionState');
  state.className = 'inspection-state';
  const by = agent ? 'Agent: ' : '';
  const verdict = summarizeIntersections(data, tolerance);
  const nameOf = (side, item) => item[`${side}Name`] || item[side]?.name
    || sceneObjects.find((object) => object.id === String(item[side]?.id ?? item[side]))?.name || String(item[side]?.id ?? item[side]);
  const idOf = (side, item) => String(item[side]?.id ?? item[side]);
  intersectionCheckSource = source;
  intersectionIds = new Set(verdict.problems.flatMap(({ item }) => [idOf('a', item), idOf('b', item)]));
  meshViewport.markers = verdict.findings.map(({ item, kind, text }) => ({
    a: idOf('a', item),
    b: idOf('b', item),
    kind,
    label: text,
    location: intersectionPoint(item.location),
    bounds: kind === 'overlap' && item.overlapBounds && intersectionPoint(item.overlapBounds.min) && intersectionPoint(item.overlapBounds.max)
      ? { min: intersectionPoint(item.overlapBounds.min), max: intersectionPoint(item.overlapBounds.max) }
      : null,
    closest: kind === 'clearance' && item.closestPoints && intersectionPoint(item.closestPoints.a) && intersectionPoint(item.closestPoints.b)
      ? { a: intersectionPoint(item.closestPoints.a), b: intersectionPoint(item.closestPoints.b) }
      : null,
  })).map((marker) => (marker.kind === 'unresolved' ? { ...marker, location: null } : marker));
  meshViewport.activeMarker = -1;
  // An agent's markers fade like its other findings; the user's own check stays.
  meshViewport.markerFade = 1;
  agentActivity.markersSince = agent ? performance.now() : null;
  if (agent) startAgentFades();
  renderObjectList();
  meshViewport.draw();
  const at = tolerance.toFixed(2);
  if (verdict.state === 'clear') {
    state.textContent = `${by}Clear`;
    state.classList.add('clear');
    results.innerHTML = verdict.pairs
      ? `<div class="inspection-empty">No overlaps, and every gap is at least ${at} mm.</div>`
      : '<div class="inspection-empty">Fewer than two visible parts, so there is nothing to compare.</div>';
    log(`${agent ? 'Agent\'s intersection' : 'Intersection'} check clear at ${at} mm tolerance.`, 'success');
    return;
  }
  if (verdict.state === 'collision') {
    state.textContent = `${by}${verdict.problems.length} found${verdict.unresolved.length ? ', incomplete' : ''}`;
    state.classList.add('collision');
  } else {
    state.textContent = `${by}Incomplete`;
    state.classList.add('incomplete');
  }
  const rows = verdict.findings.map(({ item, kind, text }, index) => {
    const location = kind === 'unresolved' ? null : intersectionPoint(item.location);
    const where = location ? `<small class="intersection-where">${location.map((value) => value.toFixed(2)).join(', ')}</small>` : '';
    return `<button class="intersection-result ${kind}" data-intersection-index="${index}"><i></i><span><b>${escapeHtml(nameOf('a', item))} × ${escapeHtml(nameOf('b', item))}</b><small>${escapeHtml(text)}</small>${where}</span></button>`;
  });
  if (!verdict.unresolved.length && verdict.incomplete) {
    rows.push('<div class="inspection-empty">The server did not finish every pair, so the result is not a clean bill.</div>');
  }
  results.innerHTML = rows.join('');
  $$('.intersection-result', results).forEach((resultButton) => resultButton.addEventListener('click', () => {
    const index = Number(resultButton.dataset.intersectionIndex);
    const { item } = verdict.findings[index];
    meshViewport.activeMarker = index;
    $$('.intersection-result', results).forEach((other) => other.classList.toggle('active', other === resultButton));
    selectObject(idOf('a', item));
  }));
  if (verdict.problems.length) {
    log(`${verdict.problems.length} part clearance problem${verdict.problems.length === 1 ? '' : 's'} found at ${at} mm tolerance.`, 'warning');
  }
  if (verdict.incomplete) log('The intersection check is incomplete: some pairs were not decided, so it cannot report the model as clear.', 'warning');
}

function showIntersectionCheckRunning(running) {
  const button = $('#checkIntersections');
  button.textContent = running ? 'Cancel' : 'Check';
  button.setAttribute('aria-label', running ? 'Cancel intersection check' : 'Check intersections');
  button.classList.toggle('running', running);
}

// Aborting closes the connection, and the server stops the work of a client that hung up.
function cancelIntersectionCheck() {
  const check = intersectionCheck;
  if (!check) return;
  intersectionCheck = null;
  clearInterval(check.clock);
  check.controller.abort();
  showIntersectionCheckRunning(false);
  resetIntersectionResults();
  renderObjectList();
  meshViewport.draw();
}

async function checkIntersections() {
  if (intersectionCheck) {
    cancelIntersectionCheck();
    return;
  }
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
  const generation = workspaceGeneration;
  const id = workspaceId;
  const token = readOnlyToken;
  const check = { controller: new AbortController(), startedAt: performance.now(), clock: null };
  const stale = () => intersectionCheck !== check || generation !== workspaceGeneration || !isCurrentWorkspace(id) || token !== readOnlyToken;
  intersectionCheck = check;
  showIntersectionCheckRunning(true);
  results.innerHTML = '';
  showIntersectionProgress();
  check.clock = setInterval(showIntersectionProgress, 1000);
  try {
    if (!token) await flushWorkspaceSave();
    if (stale()) throw Object.assign(new Error('stale'), { stale: true });
    // Object ids only mean something for the source they were compiled from. If
    // the editor moved on since the last preview (or an agent replaced the source),
    // compile it first, so the visible set below names the parts as they are now.
    if (!sceneObjectsCurrent()) {
      const rendered = await render('preview');
      if (stale()) throw Object.assign(new Error('stale'), { stale: true });
      if (!rendered) throw new Error('The model has to preview successfully before its parts can be checked.');
    }
    const source = editor.value;
    const visibleObjectIds = sceneObjects.some((object) => object.placeholder)
      ? undefined
      : sceneObjects.filter((object) => object.visible !== false).map((object) => object.id);
    // The source travels with the request, so the server measures exactly what the
    // object list was compiled from. A read-only view has no workspace id at all
    // and uses the stateless route: `/api/designs/null/...` is a 404.
    const url = workspaceId && !token ? `/api/designs/${encodeURIComponent(workspaceId)}/intersections` : '/api/intersections';
    const response = await fetch(url, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ code: source, tolerance, ...(token ? { shareToken: token } : {}), ...(visibleObjectIds ? { visibleObjectIds } : {}) }),
      signal: AbortSignal.any([check.controller.signal, workspaceLoadSignal()]),
    });
    if (stale()) throw Object.assign(new Error('stale'), { stale: true });
    const data = await response.json().catch(() => ({}));
    if (stale()) throw Object.assign(new Error('stale'), { stale: true });
    if (!response.ok || data.ok === false) {
      throw new Error(data.error || (response.status === 404 ? 'Intersection service is not available.' : `Intersection check failed (${response.status}).`));
    }
    // Edited while it ran: its answer is about a model no longer on screen.
    if (editor.value !== source) throw Object.assign(new Error('stale'), { stale: true });
    clearInterval(check.clock);
    showIntersectionResults(data, tolerance, source);
  } catch (error) {
    // The workspace this check was for is gone; its answer must not paint
    // the next one's object list.
    if (error.stale || stale()) {
      // Cancelled: that already reset the panel, and a newer check may own it by now.
      if (intersectionCheck === check) {
        clearInterval(check.clock);
        intersectionCheck = null;
        showIntersectionCheckRunning(false);
        resetIntersectionResults();
        renderObjectList();
        meshViewport.draw();
      }
      return;
    }
    clearInterval(check.clock);
    intersectionIds.clear();
    meshViewport.markers = [];
    renderObjectList();
    meshViewport.draw();
    state.className = 'inspection-state';
    state.textContent = 'Unavailable';
    results.innerHTML = `<div class="inspection-empty">${escapeHtml(error.message)}</div>`;
  } finally {
    clearInterval(check.clock);
    if (intersectionCheck === check) {
      intersectionCheck = null;
      showIntersectionCheckRunning(false);
    }
  }
}

$('#checkIntersections').addEventListener('click', checkIntersections);
// ---- Agent activity --------------------------------------------------------
// Every MCP tool call an agent makes on this design arrives through the /events
// poll (`activity=<version>`, see backend/src/activity.rs): a running line, then a
// summary with whatever it found. Live entries are acted out here: the source
// lines it read are tinted in the editor, its findings (boxes, rays, sections,
// coloured scan surfaces) are drawn on the model, and its intersection findings
// fill the inspection panel. An agent can only look at the current revision, so
// everything drawn here describes the model on screen. Each call is also a row of
// the console, with the pictures the agent was shown. History from before this page loaded is listed, never
// replayed.
//
// Every effect holds for AGENT_HOLD_MS, then fades out over AGENT_FADE_MS
// (agentFadeTick): the read tint and the findings and markers on the model.
// Showing an entry again restarts it. Coloured surfaces work differently:
// a comparison stays in the Compare parts section for good (renderCompareControls), others
// in the console (Show), and what is on the model stays until it is switched off or the
// agent's next call starts.
const AGENT_HOLD_MS = 10000;
const AGENT_FADE_MS = 2500;
const AGENT_FEED_LIMIT = 60;

const agentActivity = {
  design: null,
  version: 0,
  loaded: false,
  entries: new Map(),
  // The entry whose findings are drawn on the model, and a token that retires
  // mesh downloads for an entry no longer shown.
  shown: null,
  meshToken: 0,
  // The newest two-part comparison, an agent's or the panel's, kept here because the
  // feed trims its entries and the server drops old blobs: { seq, finishedAt, revision,
  // summary, meshes, loaded, seen }, with `loaded` filled per mesh on demand and `seen`
  // the parts found in the object list so far. Null until there is one.
  surfaces: null,
  // Two-mesh comparisons from a poll whose revision's object list has yet to arrive, oldest
  // first. Only that list can say whether their labels are parts.
  comparisonsWaiting: [],
  // Which side's heatmap is on the model, or null. Two-way, and the two overlap, so
  // never both. Not saved: a reload starts with none selected.
  surfacePick: null,
  // Another tool's coloured surfaces (fit_surface), put on the model
  // by the console's Show and off again by Hide or the next call: { seq }, or null.
  side: null,
  // The newest call this page has heard of.
  lastSeq: 0,
  // Calls already finished in the first answer: a reload lists them, never selects them.
  quietSurfaces: new Set(),
  read: null,
  // When each effect started holding (performance.now()); null holds it until
  // set (a read still running).
  // Overlay layers of recent calls, [{ seq, items, since }].
  layers: [],
  markersSince: null,
  readSince: null,
  fadeFrame: null,
  // When this page started following the design (Date.now(), like startedAt).
  openedAt: 0,
  // Entries whose findings wait for the render of the revision they measured.
  pending: [],
};

// 1 while an effect holds, then down to 0 over the fade.
function agentFadeLevel(since, now = performance.now()) {
  if (since === null || since === undefined) return 1;
  const fading = now - since - AGENT_HOLD_MS;
  return fading <= 0 ? 1 : Math.max(0, 1 - fading / AGENT_FADE_MS);
}

// One animation loop for every effect, running only while something is shown.
function startAgentFades() {
  if (agentActivity.fadeFrame === null) agentActivity.fadeFrame = requestAnimationFrame(agentFadeTick);
}

function agentFadeTick() {
  agentActivity.fadeFrame = null;
  const now = performance.now();
  let active = false;
  let redraw = false;
  const set = (key, value) => {
    if (meshViewport[key] !== value) {
      meshViewport[key] = value;
      redraw = true;
    }
  };
  if (agentActivity.layers.length) {
    const before = agentActivity.layers.length;
    agentActivity.layers = agentActivity.layers.filter((layer) => agentFadeLevel(layer.since, now) > 0);
    const layers = agentActivity.layers.map((layer) => ({ items: layer.items, fade: agentFadeLevel(layer.since, now) }));
    if (layers.length !== before || layers.some((layer) => layer.fade < 1)) redraw = true;
    meshViewport.agentOverlay = layers;
    if (layers.length) active = true;
  }
  // Nothing of the shown call left on the model: the console lets go of it.
  const surfaceShown = (agentActivity.surfacePick !== null && agentActivity.surfaces?.seq === agentActivity.shown) || agentActivity.side?.seq === agentActivity.shown;
  if (agentActivity.shown !== null && !agentActivity.layers.length && !surfaceShown) {
    agentActivity.shown = null;
    renderAgentFeed();
  }
  if (agentActivity.markersSince !== null) {
    const level = agentFadeLevel(agentActivity.markersSince, now);
    if (level <= 0) {
      agentActivity.markersSince = null;
      intersectionIds = new Set();
      meshViewport.markers = [];
      meshViewport.markerFade = 1;
      renderObjectList();
      redraw = true;
    } else { set('markerFade', level); active = true; }
  }
  if (agentActivity.read) {
    const level = agentFadeLevel(agentActivity.readSince, now);
    if (level <= 0) clearAgentRead();
    else {
      $('#agentReadLayer').style.opacity = String(level);
      active = true;
    }
  }
  if (redraw) meshViewport.draw();
  if (active) startAgentFades();
}

function resetAgentActivity(design = null) {
  clearAgentFindings();
  clearAgentRead();
  agentActivity.design = design;
  agentActivity.version = 0;
  agentActivity.loaded = false;
  agentActivity.openedAt = Date.now();
  agentActivity.pending = [];
  agentActivity.entries = new Map();
  agentActivity.lastSeq = 0;
  agentActivity.quietSurfaces = new Set();
  // Rows already in the console stay as a log; they just stop updating.
  agentRows.clear();
  agentRowsClearedSeq = 0;
  renderAgentFeed();
  renderCompareControls();
}

function agentActivityVersion(id) {
  return agentActivity.design === id ? agentActivity.version : 0;
}

// Applies the `activity` part of an /events answer.
function applyActivitySummary(activity, id) {
  if (!activity || !Array.isArray(activity.entries)) return;
  if (agentActivity.design !== id) resetAgentActivity(id);
  const version = Number(activity.version) || 0;
  // The server forgot this design's log (a restart): start over from what it has.
  if (version < agentActivity.version) resetAgentActivity(id);
  const history = !agentActivity.loaded;
  agentActivity.loaded = true;
  agentActivity.version = Math.max(agentActivity.version, version);
  for (const entry of [...activity.entries].sort((a, b) => Number(a?.seq) - Number(b?.seq))) {
    if (!Number.isFinite(Number(entry?.seq))) continue;
    const previous = agentActivity.entries.get(entry.seq);
    agentActivity.entries.set(entry.seq, entry);
    const started = Number(entry.seq) > agentActivity.lastSeq;
    agentActivity.lastSeq = Math.max(agentActivity.lastSeq, Number(entry.seq));
    // The poll answers only once something has moved, so a first answer can be news: a call
    // from after this page opened is no history.
    if (history && entry.state !== 'running' && Number(entry.startedAt) < agentActivity.openedAt) agentActivity.quietSurfaces.add(entry.seq);
    // The first answer is history, except calls still running or started since
    // this page began following the design (a second of slack for clocks).
    const live = !history || entry.state === 'running' || Number(entry.startedAt) >= agentActivity.openedAt - 1000;
    // A new call ends the selection; that call's own surfaces are selected below,
    // only once it has finished, so it never switches off its own result.
    if (live && started) endAgentSurfaceSelection(entry);
    // The person's own comparison is taken from its reply (runCompareParts): a poll must not
    // bring back one they have since aborted by changing a pick. Each is taken once, as it
    // finishes: the poll repeats recent entries, and by then the list may have moved on.
    if (entry.state === 'done' && !entry.fromBrowser && agentComparison(entry) && previous?.state !== 'done') {
      agentActivity.comparisonsWaiting = [...agentActivity.comparisonsWaiting, entry].slice(-AGENT_LAYER_LIMIT);
      settleWaitingComparisons();
    }
    if (live) actOutAgentEntry(entry, previous);
  }
  const seqs = [...agentActivity.entries.keys()].sort((a, b) => a - b);
  for (const seq of seqs.slice(0, Math.max(0, seqs.length - AGENT_FEED_LIMIT))) agentActivity.entries.delete(seq);
  renderAgentFeed();
  renderCompareControls();
}

function actOutAgentEntry(entry, previous) {
  if (entry.read && (entry.state === 'running' || previous?.state === 'running' || !previous)) showAgentRead(entry);
  const finished = entry.state !== 'running' && previous?.state !== entry.state;
  if (!finished) return;
  if (entry.state !== 'done') return;
  showAgentEntry(entry, { fromPoll: true });
}

// Shows what one entry found: on the model, in the editor, in the inspection panel.
function showAgentEntry(entry, { fromPoll = false, deferred = false } = {}) {
  if (entry.read && !fromPoll) showAgentRead(entry);
  let panel = false;
  const pairs = entry.intersections;
  // The saved revision the findings describe.
  const measured = Number(entry.revision ?? pairs?.revision);
  const spatial = Number.isFinite(measured);
  // Its model is not on screen yet (being loaded or rendered): show it once
  // that render lands. Displayed geometry that is stale waits too.
  const waiting = spatial && (measured > workspaceRevision || geometryStale || (!sceneObjectsCurrent() && activeRender));
  if (fromPoll && !deferred && waiting && measured >= workspaceRevision) {
    agentActivity.pending = agentActivity.pending.filter((other) => other.seq !== entry.seq).concat([entry]).slice(-AGENT_LAYER_LIMIT);
    return;
  }
  // Findings about an older revision would sit on geometry that moved: they
  // stay in the console (Show draws them on request).
  if (fromPoll && spatial && measured < workspaceRevision) return;
  if (pairs && measured === workspaceRevision && !hasPendingLocalWorkspaceChanges() && sceneObjectsCurrent()) {
    const tolerance = Number.isFinite(Number(pairs.tolerance)) ? Number(pairs.tolerance) : (readTolerance() ?? 0.2);
    showIntersectionResults(pairs, tolerance, editor.value, { agent: true });
    panel = true;
  }
  const overlay = Array.isArray(entry.overlay) ? entry.overlay : [];
  const meshes = agentSurfaceMeshes(entry);
  // Pairs already on the model as intersection markers are not drawn twice.
  if ((overlay.length && !panel) || meshes.length) showAgentFindings(entry, panel ? [] : overlay, meshes, { fromPoll });
}

// Called after a render lands: findings that waited for it.
function flushAgentPending() {
  const waiting = agentActivity.pending;
  agentActivity.pending = [];
  for (const entry of waiting) {
    if (!agentActivity.entries.has(entry.seq)) continue;
    // Still ahead of the model on screen: keep waiting for its render.
    if (Number(entry.revision ?? entry.intersections?.revision) > workspaceRevision) agentActivity.pending.push(entry);
    else showAgentEntry(entry, { fromPoll: true, deferred: true });
  }
}

// The newest overlay layers kept on the model at once.
const AGENT_LAYER_LIMIT = 12;

function showAgentFindings(entry, overlay, meshes, { fromPoll = false } = {}) {
  const now = performance.now();
  agentActivity.shown = entry.seq;
  if (overlay.length) {
    agentActivity.layers = agentActivity.layers.filter((layer) => layer.seq !== entry.seq)
      .concat([{ seq: entry.seq, items: overlay, since: now }])
      .slice(-AGENT_LAYER_LIMIT);
    meshViewport.agentOverlay = agentActivity.layers.map((layer) => ({ items: layer.items, fade: agentFadeLevel(layer.since, now) }));
  }
  meshViewport.draw();
  startAgentFades();
  // A call without surfaces leaves the selection to its own rules.
  if (!meshes.length) return;
  if (!agentComparison(entry) || !comparedParts(entry)) {
    // Not a comparison of two parts: never takes the panel's place, and only the console's
    // Show draws it. The two sides of a comparison overlap, so only the first goes on.
    if (!fromPoll) showSideSurfaces(entry, agentComparison(entry) ? meshes.slice(0, 1) : meshes);
    return;
  }
  if (!fromPoll) {
    // The console's Show: an explicit ask, for this call's surfaces whatever came since.
    if (agentActivity.surfaces?.seq !== entry.seq) keepAgentSurfaces(entry);
    pickAgentSurface(0);
    return;
  }
  // The person's own comparison is shown from its reply; picks may have moved on since.
  if (entry.fromBrowser) return;
  // The panel already holds the newest surfaces (applyActivitySummary). These may have
  // waited for a render while newer ones arrived, or the agent moved on: nothing to show.
  // Nor did a call that was already over when this page opened, even within the slack.
  if (agentActivity.surfaces?.seq !== entry.seq || agentActivity.quietSurfaces.has(entry.seq)) return;
  if (agentMovedOnFrom(entry)) return;
  pickAgentSurface(0);
}

// ---- Surfaces: the panel's selection ----

function agentSurfaceMeshes(entry) {
  return Array.isArray(entry?.meshes) ? entry.meshes.filter((mesh) => typeof mesh?.url === 'string') : [];
}

// Pure: when a finished call ended, on the server's clock.
function agentCallFinishedAt(entry) {
  return Number(entry?.startedAt) + (Number(entry?.elapsedMs) || 0);
}

// Pure: `call` started after that moment. A call running alongside does not count:
// an agent often issues a comparison and a picture together, and the comparison
// that ends last should still appear.
function agentCallFollows(call, finishedAt) {
  return Number(call?.startedAt) >= finishedAt;
}

// A later call started after this one finished: its result no longer gets to appear.
function agentMovedOnFrom(entry) {
  const finishedAt = agentCallFinishedAt(entry);
  return [...agentActivity.entries.values()].some((other) => other.seq > entry.seq && agentCallFollows(other, finishedAt));
}

// Pure: a two-part comparison, the only thing the Compare parts section holds.
function agentComparison(entry) {
  return entry?.tool === 'compare_surfaces' && agentSurfaceMeshes(entry).length === 2;
}

// A compare reply's label as the part it is, by id or by name, or, for what is no part (an
// uploaded asset, an agent's own label), its plain name. Older servers say "scan object-1"
// and "model object-2".
function comparePartId(label) {
  const plain = String(label ?? '').replace(/^(?:scan|model|reference|candidate)\s+/i, '');
  const named = (name) => sceneObjects.find((object) => object.id === name || object.name === name)?.id;
  return named(label) ?? named(plain) ?? plain;
}

// The two listed parts a comparison was between, or null: the panel holds nothing else.
function comparedParts(entry) {
  const ids = agentSurfaceMeshes(entry).map((mesh) => comparePartId(mesh.label));
  const listed = (id) => sceneObjects.some((object) => object.id === id);
  return ids.length === 2 && ids[0] !== ids[1] && ids.every(listed) ? ids : null;
}

// The object list for this comparison's revision is on screen, so what it does not name is no part.
function comparisonListAnswered(entry) {
  return sceneObjectsCurrent() && !(Number(entry.revision) > workspaceRevision);
}

// Those the list on screen can answer leave the queue; the newest of two listed parts takes the
// panel. True if one did: keepAgentSurfaces has then followed the list as well.
function settleWaitingComparisons() {
  const answered = agentActivity.comparisonsWaiting.filter(comparisonListAnswered);
  if (!answered.length) return false;
  agentActivity.comparisonsWaiting = agentActivity.comparisonsWaiting.filter((entry) => !answered.includes(entry));
  const newest = answered.filter((entry) => comparedParts(entry) && Number(entry.seq) > (agentActivity.surfaces?.seq ?? -Infinity)).at(-1);
  if (!newest) return false;
  keepAgentSurfaces(newest);
  return true;
}

// The panel now describes this call, with its two parts selected. Whatever was on the
// model described another.
function keepAgentSurfaces(entry) {
  switchOffAgentSurface();
  const revision = Number(entry.revision);
  const meshes = agentSurfaceMeshes(entry);
  agentActivity.surfaces = {
    seq: entry.seq,
    finishedAt: agentCallFinishedAt(entry),
    revision: Number.isFinite(revision) ? revision : null,
    meshes,
    loaded: [],
    seen: new Set(),
  };
  comparePicks.a = comparePartId(meshes[0].label);
  comparePicks.b = comparePartId(meshes[1].label);
  compareError = '';
  syncCompareParts();
}

// An agent call started: what is on the model from before it leaves. The panel and
// its data stay. Another tool's surfaces were shown by hand, so any call ends them.
function endAgentSurfaceSelection(call) {
  switchOffSideSurfaces();
  const surfaces = agentActivity.surfaces;
  if (agentActivity.surfacePick !== null && surfaces && agentCallFollows(call, surfaces.finishedAt)) switchOffAgentSurface();
}

function pickAgentSurface(index) {
  const surfaces = agentActivity.surfaces;
  const mesh = surfaces?.meshes[index];
  if (!mesh) return;
  const token = ++agentActivity.meshToken;
  agentActivity.side = null;
  agentActivity.surfacePick = index;
  agentActivity.shown = surfaces.seq;
  renderCompareControls();
  renderAgentFeed();
  if (surfaces.loaded[index]) {
    meshViewport.setAgentMeshes([surfaces.loaded[index]]);
    return;
  }
  // The other surface goes now, not when this one finishes loading.
  meshViewport.clearAgentMeshes();
  fetch(mesh.url, { cache: 'force-cache' })
    .then(async (response) => {
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      surfaces.loaded[index] = { bytes: await response.arrayBuffer(), palette: mesh.palette, runs: mesh.runs };
      if (token === agentActivity.meshToken) meshViewport.setAgentMeshes([surfaces.loaded[index]]);
    })
    .catch((error) => {
      if (token !== agentActivity.meshToken) return;
      log(`Could not load the agent's surfaces: ${error.message}`, 'warning');
      switchOffAgentSurface();
    });
}

function switchOffAgentSurface() {
  if (agentActivity.surfacePick === null) return;
  agentActivity.meshToken += 1;
  agentActivity.surfacePick = null;
  meshViewport.clearAgentMeshes();
  if (agentActivity.shown === agentActivity.surfaces?.seq && !agentActivity.layers.length) agentActivity.shown = null;
  renderCompareControls();
  renderAgentFeed();
}

// Another tool's surfaces, at the console's Show.
function showSideSurfaces(entry, meshes) {
  switchOffAgentSurface();
  const token = ++agentActivity.meshToken;
  agentActivity.side = { seq: entry.seq };
  agentActivity.shown = entry.seq;
  meshViewport.clearAgentMeshes();
  renderAgentFeed();
  Promise.all(meshes.map(async (mesh) => {
    const response = await fetch(mesh.url, { cache: 'force-cache' });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    return { bytes: await response.arrayBuffer(), palette: mesh.palette, runs: mesh.runs };
  }))
    .then((loaded) => { if (token === agentActivity.meshToken) meshViewport.setAgentMeshes(loaded); })
    .catch((error) => {
      if (token !== agentActivity.meshToken) return;
      log(`Could not load the agent's surfaces: ${error.message}`, 'warning');
      switchOffSideSurfaces();
    });
}

function switchOffSideSurfaces() {
  if (!agentActivity.side) return;
  agentActivity.meshToken += 1;
  if (agentActivity.shown === agentActivity.side.seq && !agentActivity.layers.length) agentActivity.shown = null;
  agentActivity.side = null;
  meshViewport.clearAgentMeshes();
  renderAgentFeed();
}

// ---- Compare parts: choosing two parts and running it ----

function resetCompareParts() {
  comparePicks.a = '';
  comparePicks.b = '';
  cancelCompareRun();
  compareError = '';
}

// Whatever comes back from the run on its way, if anything, is no longer wanted.
function cancelCompareRun() {
  compareRun += 1;
  compareController?.abort();
  compareController = null;
  compareRunning = false;
}

// Follows the part list: a chosen part that is gone stops being chosen, and a comparison
// of a part that is gone leaves the model and the section. A comparison that came ahead of its
// parts takes the panel here, when the list for its revision arrives.
function syncCompareParts() {
  if (settleWaitingComparisons()) return;
  const present = new Set(sceneObjects.map((object) => object.id));
  const surfaces = agentActivity.surfaces;
  const labels = surfaces ? surfaces.meshes.map((mesh) => comparePartId(mesh.label)) : [];
  for (const id of labels) if (present.has(id)) surfaces.seen.add(id);
  // Its result goes too: there is no part left to paint it on.
  if (surfaces && [...surfaces.seen].some((id) => !present.has(id))) {
    switchOffAgentSurface();
    agentActivity.surfaces = null;
    renderCompareControls();
  }
  for (const key of ['a', 'b']) {
    if (comparePicks[key] && !present.has(comparePicks[key])) {
      comparePicks[key] = '';
      cancelCompareRun();
      switchOffAgentSurface();
    }
  }
  renderCompareControls();
}

// Pure: the pickers' state. Every part by its plain name, and nothing else. `status` is the one short word on the section's own row.
function compareControlsView(objects, picks, { enabled = true, running = false, error = '' } = {}) {
  const options = objects.map((object) => ({ value: object.id, label: object.name || object.id }));
  const enough = enabled && objects.length >= 2;
  const ready = enough && Boolean(picks.a) && Boolean(picks.b) && picks.a !== picks.b;
  let status = '';
  if (running) status = 'Comparing…';
  else if (error) status = 'Failed';
  else if (!enabled) status = 'Needs a saved design';
  else if (!enough) status = 'Needs 2 parts';
  return { options, enough, ready, status, detail: running ? '' : error, tone: running ? 'running' : error ? 'error' : 'muted' };
}

// The placeholder is a prompt, not a choice: gone once a part is chosen. While it is there,
// `hidden` keeps it out of the open list, and where a browser ignores that, `disabled` still
// makes it unpickable.
function compareOptionsHtml(view, placeholder, own) {
  return `${own ? '' : `<option value="" disabled hidden selected>${placeholder}</option>`}${view.options.map((option) => `<option value="${escapeAttribute(option.value)}"${option.value === own ? ' selected' : ''}>${escapeHtml(option.label)}</option>`).join('')}`;
}

// Pure: a heatmap cannot be read without its key. The bar and its ends; hovering or focusing
// it tells what the colours outside the scale are.
function compareScaleHtml(legend = {}) {
  const scale = surfaceScaleHtml(legend);
  if (!scale) return '';
  const keys = surfaceKeysHtml(legend);
  return `${scale}${keys ? `<span class="compare-scale-tip" role="tooltip">${keys}</span>` : ''}`;
}

function compareHeatmapShowing() {
  return Boolean(agentActivity.surfaces) && agentActivity.surfacePick !== null;
}

function renderCompareControls() {
  const status = document.getElementById('compareStatus');
  const view = compareControlsView(sceneObjects, comparePicks, { enabled: assetsEnabled(), running: compareRunning, error: compareError });
  for (const [id, own, placeholder] of [['#comparePartA', comparePicks.a, 'First part'], ['#comparePartB', comparePicks.b, 'Second part']]) {
    const select = $(id);
    const html = compareOptionsHtml(view, placeholder, own);
    // Rebuilt only on a change: it would close a list someone has open.
    if (select.dataset.html !== html) {
      select.innerHTML = html;
      select.dataset.html = html;
    }
    select.value = own;
    select.disabled = !view.enough;
  }
  status.textContent = view.status;
  status.dataset.tone = view.tone;
  const failure = $('#compareError');
  failure.textContent = view.detail;
  failure.hidden = !view.detail;
  const button = $('#compareButton');
  const showing = compareHeatmapShowing();
  button.disabled = !compareRunning && !showing && !view.ready;
  button.textContent = compareRunning ? 'Cancel' : showing ? 'Hide' : 'Compare';
  button.setAttribute('aria-label', compareRunning ? 'Cancel comparison' : showing ? 'Hide heatmap' : 'Compare parts');
  button.classList.toggle('running', compareRunning);
  for (const [id, own] of [['#compareSwatchA', comparePicks.a], ['#compareSwatchB', comparePicks.b]]) {
    const part = sceneObjects.find((object) => object.id === own);
    const dot = part ? objectSwatchHtml(part) : '';
    const swatch = $(id);
    if (swatch.dataset.html !== dot) {
      swatch.innerHTML = dot;
      swatch.dataset.html = dot;
    }
  }
  const surfaces = agentActivity.surfaces;
  const pick = agentActivity.surfacePick;
  const scale = $('#compareScale');
  const html = surfaces && pick !== null ? compareScaleHtml(surfaces.meshes[pick]?.legend) : '';
  if (scale.dataset.html !== html) {
    scale.innerHTML = html;
    scale.dataset.html = html;
  }
  scale.hidden = !html;
}

// Compare runs what is chosen, and the heatmap goes on the first. A choice changes what is
// shown, so it ends a run and the heatmap. Choosing the part the other picker holds swaps the
// two, which is how it moves to the other.
function chooseComparePart(key, value) {
  const other = key === 'a' ? 'b' : 'a';
  if (value === comparePicks[other]) comparePicks[other] = comparePicks[key];
  comparePicks[key] = value;
  cancelCompareRun();
  compareError = '';
  switchOffAgentSurface();
  renderCompareControls();
}

// Pure: says why in words, and tells an older server from a wrong request.
function compareFailureMessage(status, body) {
  if (status === 404) return `This server cannot compare parts (HTTP 404${body?.error ? `: ${body.error}` : ''}). It may be an older build.`;
  return body?.error || `The comparison failed (HTTP ${status}).`;
}

async function runCompareParts() {
  const { a, b } = comparePicks;
  if (!assetsEnabled() || !a || !b || a === b) return;
  cancelCompareRun();
  const run = compareRun;
  const id = workspaceId;
  const controller = new AbortController();
  const current = () => run === compareRun && id === workspaceId;
  compareController = controller;
  compareRunning = true;
  compareError = '';
  switchOffAgentSurface();
  renderCompareControls();
  try {
    await flushWorkspaceSave();
    if (!current()) return;
    const response = await fetch(`/api/designs/${encodeURIComponent(id)}/compare`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ a, b }),
      signal: controller.signal,
    });
    const body = await response.json().catch(() => null);
    if (!current()) return;
    if (!response.ok || !body?.entry) throw new Error(compareFailureMessage(response.status, body));
    const { entry } = body;
    // The poll may have brought it already; either way the next one is no news.
    agentActivity.entries.set(entry.seq, entry);
    agentActivity.lastSeq = Math.max(agentActivity.lastSeq, Number(entry.seq));
    renderAgentFeed();
    // Saved over while it ran: its heatmap would sit on geometry that moved. Not a failure,
    // so back to the idle button.
    if (Number(entry.revision) < workspaceRevision) {
      log('The design changed during the comparison. Compare again to see the new revision.', 'info');
      return;
    }
    if (!agentComparison(entry)) throw new Error('These parts are too large to paint. The comparison itself ran: see the console.');
    if (Number(entry.seq) > (agentActivity.surfaces?.seq ?? -Infinity)) keepAgentSurfaces(entry);
    if (agentActivity.surfaces?.seq === entry.seq && agentActivity.surfacePick === null && !agentMovedOnFrom(entry)) pickAgentSurface(0);
  } catch (error) {
    if (current()) {
      compareError = error.message;
      log(`Compare parts failed: ${error.message}`, 'error');
    }
  } finally {
    if (run === compareRun) {
      compareController = null;
      compareRunning = false;
      renderCompareControls();
    }
  }
}

$('#comparePartA').addEventListener('change', (event) => chooseComparePart('a', event.target.value));
$('#comparePartB').addEventListener('change', (event) => chooseComparePart('b', event.target.value));
$('#compareButton').addEventListener('click', () => {
  if (compareRunning) {
    cancelCompareRun();
    renderCompareControls();
  } else if (compareHeatmapShowing()) {
    switchOffAgentSurface();
  } else {
    runCompareParts();
  }
});

function clearAgentFindings() {
  agentActivity.meshToken += 1;
  agentActivity.shown = null;
  agentActivity.layers = [];
  agentActivity.surfaces = null;
  agentActivity.comparisonsWaiting = [];
  agentActivity.surfacePick = null;
  agentActivity.side = null;
  resetCompareParts();
  meshViewport.agentOverlay = [];
  meshViewport.clearAgentMeshes();
}

// ---- Reads, tinted in the editor ----

function showAgentRead(entry) {
  const ranges = (entry.read?.ranges || [])
    .map((range) => [Number(range[0]), Number(range[1])])
    .filter(([start, end]) => Number.isInteger(start) && Number.isInteger(end) && start >= 1 && end >= start)
    .slice(0, 200);
  if (!ranges.length) return;
  const hits = new Set((entry.read?.hits || []).map(Number));
  agentActivity.read = { seq: entry.seq, ranges, hits, kind: entry.read.kind || 'read' };
  // A read still running holds; its fade starts when it finishes.
  agentActivity.readSince = entry.state === 'running' ? null : performance.now();
  $('#agentReadLayer').style.opacity = '1';
  renderAgentRead();
  startAgentFades();
  // Follow the agent through the file, but never pull the view away from someone typing.
  if (document.activeElement !== editor) {
    const metrics = editorLineMetrics();
    const top = metrics.padding + (ranges[0][0] - 1) * metrics.line;
    const bottom = metrics.padding + ranges[0][1] * metrics.line;
    if (top < editor.scrollTop || bottom > editor.scrollTop + editor.clientHeight) {
      editor.scrollTo({ top: Math.max(0, top - metrics.line * 2), behavior: 'smooth' });
    }
  }
}

function clearAgentRead() {
  agentActivity.read = null;
  agentActivity.readSince = null;
  renderAgentRead();
}

function editorLineMetrics() {
  const style = getComputedStyle(highlight);
  return { line: parseFloat(style.lineHeight) || 19.8, padding: parseFloat(style.paddingTop) || 16 };
}

function renderAgentRead() {
  const layer = $('#agentReadLayer');
  const read = agentActivity.read;
  if (!read) {
    layer.innerHTML = '';
    return;
  }
  const { line, padding } = editorLineMetrics();
  const total = editor.value.split('\n').length;
  layer.innerHTML = read.ranges
    .filter(([start]) => start <= total)
    .map(([start, end]) => `<i class="agent-read-band ${read.kind}" style="top:${padding + (start - 1) * line}px;height:${(Math.min(end, total) - start + 1) * line}px"></i>`)
    .concat([...read.hits].filter((hit) => hit >= 1 && hit <= total).slice(0, 200)
      .map((hit) => `<i class="agent-read-band hit" style="top:${padding + (hit - 1) * line}px;height:${line}px"></i>`))
    .join('');
  syncAgentReadScroll();
}

function syncAgentReadScroll() {
  $('#agentReadLayer').style.transform = `translateY(${-editor.scrollTop}px)`;
}

// ---- The surfaces panel, and the feed ----

const AGENT_TOOL_ICONS = { running: '', done: '✓', error: '!' };

function agentClock(ms) {
  return Number.isFinite(Number(ms)) ? new Date(Number(ms)).toLocaleTimeString([], { hour12: false }) : '';
}

function agentElapsed(entry) {
  const ms = Number(entry.elapsedMs);
  if (!Number.isFinite(ms)) return '';
  return ms < 1000 ? `${Math.round(ms)} ms` : formatElapsed(ms / 1000);
}

// Only http(s) and same-origin paths become links.
function agentSafeUrl(url) {
  return typeof url === 'string' && /^(https?:\/\/|\/(?!\/))/i.test(url) ? url : null;
}

// Pure: the gradient and its two ends.
function surfaceScaleHtml(legend = {}) {
  const range = Number(legend.range);
  if (!Number.isFinite(range) || range <= 0) return '';
  const shown = escapeHtml(String(Number(range.toPrecision(3))));
  return `<span class="agent-legend-bar"><i></i><small>−${shown}</small><small>+${shown} mm</small></span>`;
}

// Pure: the colours that are no distance.
function surfaceKeysHtml(legend = {}) {
  return (legend.keys || []).slice(0, 4).map((key) => {
    const color = Array.isArray(key.color) ? `rgb(${key.color.slice(0, 3).map(Number).join(',')})` : '#888';
    return `<span class="agent-legend-key"><i style="background:${color}"></i>${escapeHtml(key.text || '')}</span>`;
  }).join('');
}

// Pure: a surface's scale and colour keys, with its title if it has one.
function surfaceLegendHtml(legend = {}) {
  return `${legend.title ? `<b>${escapeHtml(legend.title)}</b>` : ''}${surfaceScaleHtml(legend)}${surfaceKeysHtml(legend)}`;
}

// Agent calls are lines of the console like every other message: one row per
// call, updated in place as it runs and finishes, with the pictures the agent
// was shown and the links it made. Rows cleared with Clear stay cleared.
const agentRows = new Map();
let agentRowsClearedSeq = 0;

// Pure: the row's one line. A comparison says what it compared, by the labels of its reply.
function agentRowSummary(entry) {
  const summary = entry.summary || entry.tool;
  const labels = entry.tool === 'compare_surfaces' && Array.isArray(entry.meshes) ? entry.meshes.map((mesh) => mesh?.label).filter(Boolean) : [];
  return labels.length === 2 && summary.startsWith('Compared:') ? `Compared ${labels[0]} with ${labels[1]}:${summary.slice('Compared:'.length)}` : summary;
}

function agentRowHtml(entry) {
  const images = (entry.images || []).filter((image) => agentSafeUrl(image.url)).map((image, index) => `<button type="button" class="agent-thumb" data-agent-image="${entry.seq}:${index}" title="${escapeAttribute(image.title || '')}"><img src="${escapeAttribute(image.url)}" alt="${escapeAttribute(image.title || 'What the agent saw')}" loading="lazy" decoding="async" /></button>`).join('');
  const links = (entry.links || []).map((link) => ({ ...link, url: agentSafeUrl(link.url) })).filter((link) => link.url)
    .map((link) => `<a href="${escapeAttribute(link.url)}" target="_blank" rel="noreferrer noopener">${escapeHtml(link.label || 'Open')}</a>`).join('');
  const showable = entry.state === 'done' && (entry.overlay?.length || entry.meshes?.length || entry.read || entry.intersections);
  const onModel = agentActivity.side?.seq === entry.seq;
  const show = showable ? `<button type="button" class="agent-show" data-agent-show="${entry.seq}">${onModel ? 'Hide' : 'Show'}</button>` : '';
  // A comparison of parts reads in its own section; any other surfaces read here while shown.
  const legend = onModel && entry.meshes?.[0] ? `<div class="agent-legend">${surfaceLegendHtml({ title: entry.meshes[0].label, ...entry.meshes[0].legend })}</div>` : '';
  const error = entry.error ? `<p class="agent-error">${escapeHtml(entry.error)}</p>` : '';
  const extra = images || links ? `<div class="agent-entry-extra">${images}${links}</div>` : '';
  return `<span class="timestamp">${agentClock(entry.startedAt)}</span><div class="agent-line"><div class="agent-entry-head"><i>${AGENT_TOOL_ICONS[entry.state] ?? ''}</i><b>${escapeHtml(agentRowSummary(entry))}</b><small>${entry.fromBrowser ? 'you' : 'agent'} · ${escapeHtml(entry.tool)}${agentElapsed(entry) ? ` · ${agentElapsed(entry)}` : ''}</small>${show}</div>${error}${legend}${extra}</div>`;
}

function renderAgentFeed() {
  for (const entry of [...agentActivity.entries.values()].sort((a, b) => a.seq - b.seq)) {
    let known = agentRows.get(entry.seq);
    if (known && !known.row.isConnected) {
      agentRows.delete(entry.seq);
      continue;
    }
    if (!known) {
      if (entry.seq <= agentRowsClearedSeq) continue;
      const row = document.createElement('div');
      row.dataset.agentSeq = entry.seq;
      consoleOutput.append(row);
      known = { row, html: '' };
      agentRows.set(entry.seq, known);
    }
    const html = agentRowHtml(entry);
    known.row.className = `agent-row ${entry.state}${agentActivity.shown === entry.seq ? ' shown' : ''}`;
    if (known.html !== html) {
      known.row.innerHTML = html;
      known.html = html;
    }
  }
}

function openAgentImage(seq, index) {
  const entry = agentActivity.entries.get(seq);
  const image = entry?.images?.[index];
  const url = agentSafeUrl(image?.url);
  if (!url) return;
  const box = $('#agentLightbox');
  $('img', box).src = url;
  $('img', box).alt = image.title || '';
  $('figcaption', box).textContent = `${image.title || ''} — ${entry.summary || entry.tool}`;
  box.hidden = false;
}

consoleOutput.addEventListener('click', (event) => {
  const show = event.target.closest('[data-agent-show]');
  if (show) {
    const entry = agentActivity.entries.get(Number(show.dataset.agentShow));
    if (entry && agentActivity.side?.seq === entry.seq) switchOffSideSurfaces();
    else if (entry) showAgentEntry(entry);
    renderAgentFeed();
    return;
  }
  const thumb = event.target.closest('[data-agent-image]');
  if (thumb) {
    const [seq, index] = thumb.dataset.agentImage.split(':').map(Number);
    openAgentImage(seq, index);
  }
});
$('#agentLightbox').addEventListener('click', () => { $('#agentLightbox').hidden = true; });
document.addEventListener('keydown', (event) => {
  if (event.key === 'Escape' && !$('#agentLightbox').hidden) $('#agentLightbox').hidden = true;
});
editor.addEventListener('scroll', syncAgentReadScroll);
// Typing moves lines under the tint; the agent's read no longer lines up.
editor.addEventListener('input', () => { if (agentActivity.read) clearAgentRead(); });

// ---- Agent / MCP connection panel ---------------------------------------
// Everything here is derived at runtime: the endpoint comes from the origin the
// page was actually served from (so REOPENSCAD_PUBLIC_ORIGIN, a reverse proxy or
// a different port are all correct for free), and the tool list is read from the
// live `tools/list` so it can never drift from what the server implements.
//
// The panel itself is deliberately non-technical: a grid of agents, and for the
// one picked a few numbered steps with copy buttons. Raw config and the
// protocol-level facts live behind "Show details".
// --- Model feedback ------------------------------------------------------------
//
// Comments pinned to points on the model, for the AI agent: list_feedback reads them
// over MCP, resolve_feedback answers them, and request_pointing switches this tab
// into comment mode with a one-line hint naming what the agent needs located. The
// /events long poll carries a summary (version, open count, active request); the list
// itself is fetched only when the version moves. Editable designs only.

function feedbackEnabled() {
  return Boolean(workspaceId) && !readOnlyToken && !workspaceLocalOnly;
}

function feedbackUrl(suffix = '') {
  return `/api/designs/${encodeURIComponent(workspaceId)}/feedback${suffix}`;
}

// 1-based line of a byte offset into the source.
function sourceLineAt(code, byteOffset) {
  const index = byteOffsetToStringIndex(code, byteOffset);
  let line = 1;
  for (let i = 0; i < index && i < code.length; i += 1) if (code.charCodeAt(i) === 10) line += 1;
  return line;
}

function commentNumber(comment) {
  return String(comment?.id || '').replace(/^c/, '');
}

function resetFeedback() {
  feedbackLoadToken += 1;
  feedbackLastSeq = 0;
  hidePinCursor();
  feedbackVersion = 0;
  feedbackComments = [];
  feedbackRequest = null;
  feedbackEndedRequestId = null;
  activeCommentId = null;
  pendingComment = null;
  commentMode = false;
  $('#feedbackPanel').hidden = true;
  $('#feedbackComposer').hidden = true;
  renderFeedback();
}

// Applies the summary every /events poll carries.
function applyFeedbackSummary(summary) {
  if (!summary || !feedbackEnabled()) return;
  syncFeedbackRequest(summary.request || null);
  if (Number(summary.version) !== feedbackVersion) loadFeedback();
  else renderFeedback();
}

// An agent's request_pointing puts this tab into comment mode; when it ends anywhere
// (dismissed here or in another tab, superseded, expired) the banner goes.
function syncFeedbackRequest(request) {
  const live = request && request.id !== feedbackEndedRequestId ? request : null;
  if (live && live.id !== feedbackRequest?.id) {
    feedbackRequest = live;
    if (typeof closeMobileSheet === 'function') closeMobileSheet();
    enterCommentMode();
    log(live.what ? `Agent: show where ${live.what}` : 'Agent: show a spot on the model', 'info');
    return;
  }
  if (!live && feedbackRequest) {
    feedbackRequest = null;
    exitCommentMode();
  }
}

async function loadFeedback() {
  if (!feedbackEnabled()) return;
  const token = ++feedbackLoadToken;
  const id = workspaceId;
  try {
    const response = await fetch(feedbackUrl(), { cache: 'no-store' });
    if (!response.ok || token !== feedbackLoadToken || id !== workspaceId) return;
    const payload = await response.json();
    if (token !== feedbackLoadToken || id !== workspaceId) return;
    feedbackComments = Array.isArray(payload.comments) ? payload.comments : [];
    feedbackLastSeq = Math.max(feedbackLastSeq, Number(payload.lastSeq) || 0);
    feedbackVersion = Number(payload.version) || 0;
    syncFeedbackRequest(payload.request || null);
    renderFeedback();
  } catch {
    // Offline: the next poll tries again.
  }
}

async function feedbackWrite(suffix, method, body) {
  const id = workspaceId;
  const response = await fetch(feedbackUrl(suffix), {
    method,
    headers: body ? { 'Content-Type': 'application/json' } : {},
    body: body ? JSON.stringify(body) : undefined,
  });
  const payload = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(payload.error || `Request failed (${response.status}).`);
  if (id === workspaceId) await loadFeedback();
  return payload;
}

function enterCommentMode() {
  if (!feedbackEnabled()) return;
  commentMode = true;
  renderFeedback();
}

function exitCommentMode() {
  commentMode = false;
  hidePinCursor();
  pendingComment = null;
  $('#feedbackText').value = '';
  $('#feedbackComposer').hidden = true;
  renderFeedback();
}

// Errors stay in view on phones, where the console is hidden.
function feedbackNotice(message) {
  log(message, 'error');
  setStatus(message, 'error');
  if (!$('#feedbackBanner').hidden) $('#feedbackBannerText').textContent = message;
}

// Leaving comment mode. During an agent's request_pointing it also tells the agent the
// person dismissed it; the banner stays until the server has acknowledged that.
async function leaveCommentMode() {
  const request = feedbackRequest;
  if (!request) { exitCommentMode(); return; }
  if (feedbackFinishing) return;
  feedbackFinishing = true;
  $('#feedbackExit').disabled = true;
  const generation = workspaceGeneration;
  try {
    if (feedbackSaving) await feedbackSaving.catch(() => {});
    // Switched designs meanwhile: this request belongs to the other one, and
    // feedbackWrite would address the new design.
    if (generation !== workspaceGeneration) return;
    await feedbackWrite('/dismiss', 'POST', { requestId: request.id });
    if (feedbackRequest?.id === request.id || !feedbackRequest) {
      feedbackEndedRequestId = request.id;
      feedbackRequest = null;
      exitCommentMode();
    }
  } catch (error) {
    feedbackNotice(`Could not reach the agent: ${error.message}. Try again.`);
  } finally {
    feedbackFinishing = false;
    $('#feedbackExit').disabled = false;
  }
}

function objectSourceLine(objectId) {
  const object = sceneObjects.find((item) => item.id === objectId);
  const start = object?.sourceRange?.start ?? object?.sourceRange?.from;
  return Number.isFinite(start) ? sourceLineAt(editor.value, start) : null;
}

// A click or tap in comment mode: pin a draft where the ray meets the model.
function placeComment(clientX, clientY) {
  const hit = meshViewport.pick(clientX, clientY);
  if (!hit) {
    setStatus('Click on the model surface to pin a comment', 'ready');
    return;
  }
  const object = sceneObjects.find((item) => item.id === hit.objectId);
  pendingComment = {
    point: hit.point,
    normal: hit.normal,
    objectId: object?.placeholder ? null : hit.objectId,
    objectName: object?.placeholder ? null : object?.name || null,
    face: hit.face,
    sourceLine: objectSourceLine(hit.objectId),
    // Captured at the click: the mesh under the cursor is this revision's, and the
    // draft answers the pointing request showing now, whatever arrives while typing.
    revision: workspaceRevision,
    requestId: feedbackRequest?.id || null,
    clientKey: `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`,
  };
  const target = [pendingComment.objectName, pendingComment.sourceLine ? `line ${pendingComment.sourceLine}` : null].filter(Boolean).join(' · ');
  $('#feedbackComposerTarget').textContent = target ? `On ${target}` : '';
  $('#feedbackComposer').hidden = false;
  renderFeedbackPins();
  // For an agent's request the box starts with the answer's framing as real text, caret
  // at the end, so the person can save it as is or append to it.
  const asked = pendingComment.requestId ? feedbackRequest : null;
  const box = $('#feedbackText');
  box.placeholder = 'What should change here?';
  if (asked) box.value = `${asked.what ? `This point is an answer to the question: ${asked.what}` : "This point answers the agent's request"} `;
  box.focus({ preventScroll: true });
  box.setSelectionRange(box.value.length, box.value.length);
}

async function submitComment(event) {
  event?.preventDefault();
  if (feedbackSaving) return feedbackSaving;
  const text = $('#feedbackText').value.trim();
  const draft = pendingComment;
  // A bare pin answers an agent's request_pointing; a comment of your own needs words.
  if (!draft || (!text && !draft.requestId)) return undefined;
  const generation = workspaceGeneration;
  const save = $('#feedbackComposerSave');
  save.disabled = true;
  feedbackSaving = (async () => {
    try {
      const payload = await feedbackWrite('', 'POST', { ...draft, text });
      // A save that finishes after the user moved on (another design, another draft)
      // must not clear what they are doing now.
      if (generation !== workspaceGeneration || pendingComment !== draft) return;
      $('#feedbackText').value = '';
      pendingComment = null;
      $('#feedbackComposer').hidden = true;
      activeCommentId = payload.comment?.id || null;
      // A pin for the agent's request answers it: the server has closed the request,
      // so leave comment mode here too. A comment of your own keeps the mode on.
      if (draft.requestId && feedbackRequest?.id === draft.requestId) {
        feedbackEndedRequestId = draft.requestId;
        feedbackRequest = null;
        exitCommentMode();
        log('Pinned. The agent has your answer.', 'info');
      }
      renderFeedback();
    } catch (error) {
      if (generation === workspaceGeneration) feedbackNotice(`Comment not saved: ${error.message}`);
      throw error;
    } finally {
      save.disabled = false;
      feedbackSaving = null;
    }
  })();
  return feedbackSaving.catch(() => {});
}

function cancelComposer() {
  pendingComment = null;
  $('#feedbackText').value = '';
  $('#feedbackComposer').hidden = true;
  renderFeedback();
}

function renderFeedback() {
  const enabled = feedbackEnabled();
  const open = feedbackComments.filter((comment) => comment.status === 'open');
  const resolved = feedbackComments.filter((comment) => comment.status !== 'open');
  const button = $('#feedbackButton');
  button.hidden = !enabled;
  button.setAttribute('aria-pressed', String(enabled && commentMode));
  const badge = $('#feedbackBadge');
  badge.textContent = String(open.length);
  badge.hidden = !open.length;
  $('#viewport').classList.toggle('commenting', enabled && commentMode);
  if (!enabled) {
    $('#feedbackPanel').hidden = true;
    $('#feedbackBanner').hidden = true;
    $('#feedbackComposer').hidden = true;
  }
  button.setAttribute('aria-expanded', String(!$('#feedbackPanel').hidden));

  const banner = $('#feedbackBanner');
  banner.hidden = !(enabled && commentMode);
  if (!banner.hidden) {
    const asked = feedbackRequest;
    banner.classList.toggle('agent', Boolean(asked));
    const verb = mobileLayout.matches ? 'tap' : 'click';
    const Verb = verb[0].toUpperCase() + verb.slice(1);
    $('#feedbackBannerText').textContent = asked
      ? (asked.what ? `Show the agent where: ${asked.what} — ${verb} the model` : `${Verb} the model to show the agent a spot`)
      : `${Verb} the model to pin a comment`;
  }

  $('#feedbackOpenCount').textContent = String(open.length);
  $('#feedbackResolvedCount').textContent = String(resolved.length);
  $$('[data-feedback-tab]').forEach((tab) => tab.setAttribute('aria-selected', String(tab.dataset.feedbackTab === feedbackTab)));
  const shown = feedbackTab === 'open' ? open : resolved;
  const listHtml = shown.length
    ? shown.slice().reverse().map((comment) => {
      const meta = [comment.objectName || comment.objectId, comment.sourceLine ? `line ${comment.sourceLine}` : null, `rev ${comment.revision}`].filter(Boolean).map(escapeHtml).join(' · ');
      return `<li class="feedback-item${comment.id === activeCommentId ? ' active' : ''}" data-comment-id="${escapeAttribute(comment.id)}">`
        + `<button type="button" class="feedback-item-main" data-feedback-focus><i>${escapeHtml(commentNumber(comment))}</i><span><b>${escapeHtml(comment.text || 'Pinned spot')}</b><small>${meta}</small></span></button>`
        + (comment.reply ? `<p class="feedback-reply"><span>Agent</span>${escapeHtml(comment.reply)}</p>` : '')
        + `<div class="feedback-item-actions"><button type="button" data-feedback-status="${comment.status === 'open' ? 'resolved' : 'open'}">${comment.status === 'open' ? 'Resolve' : 'Reopen'}</button><button type="button" data-feedback-delete aria-label="Delete comment ${escapeAttribute(commentNumber(comment))}">Delete</button></div>`
        + '</li>';
    }).join('')
    : `<li class="feedback-empty">${feedbackTab === 'open' ? 'Pin a comment to show your AI agent what should change.' : 'Nothing resolved yet.'}</li>`;
  // Rebuilt only when it changed: every poll renders, and replacing the rows would
  // drop keyboard focus from the button the user is on.
  const list = $('#feedbackList');
  if (list.dataset.html !== listHtml) {
    const focusedId = document.activeElement?.closest?.('#feedbackList [data-comment-id]')?.dataset.commentId;
    list.innerHTML = listHtml;
    list.dataset.html = listHtml;
    if (focusedId) list.querySelector(`[data-comment-id="${CSS.escape(focusedId)}"] button`)?.focus();
  }
  renderFeedbackPins();
}

// Pins are DOM buttons over the canvas, repositioned on every draw.
function renderFeedbackPins() {
  const layer = $('#feedbackPins');
  if (!feedbackEnabled()) { layer.innerHTML = ''; return; }
  const showResolved = feedbackTab === 'resolved' && !$('#feedbackPanel').hidden;
  const visible = feedbackComments.filter((comment) => comment.status === 'open' || comment.id === activeCommentId || showResolved);
  const wanted = new Set(visible.map((comment) => comment.id));
  for (const pin of [...layer.children]) if (!wanted.has(pin.dataset.commentId) && pin.dataset.commentId !== 'draft') pin.remove();
  for (const comment of visible) {
    let pin = layer.querySelector(`[data-comment-id="${CSS.escape(comment.id)}"]`);
    if (!pin) {
      pin = document.createElement('button');
      pin.type = 'button';
      pin.className = 'feedback-pin';
      pin.dataset.commentId = comment.id;
      layer.append(pin);
    }
    pin.textContent = commentNumber(comment);
    pin.title = comment.text;
    pin.setAttribute('aria-label', `Comment ${commentNumber(comment)}: ${comment.text}`);
    pin.classList.toggle('resolved', comment.status !== 'open');
    pin.classList.toggle('active', comment.id === activeCommentId);
  }
  let draft = layer.querySelector('[data-comment-id="draft"]');
  if (pendingComment && !draft) {
    draft = document.createElement('span');
    draft.className = 'feedback-pin draft';
    draft.dataset.commentId = 'draft';
    draft.textContent = '+';
    layer.append(draft);
  } else if (!pendingComment && draft) {
    draft.remove();
  }
  positionFeedbackOverlays();
}

function positionFeedbackOverlays() {
  const layer = $('#feedbackPins');
  if (!layer.children.length && $('#feedbackComposer').hidden) return;
  const ratio = renderCanvas.width / Math.max(1, renderCanvas.clientWidth);
  // Pins live in their own layer: measure the canvas against it rather than assume
  // they share an origin, so a layout change cannot shift every pin.
  const canvasBox = renderCanvas.getBoundingClientRect();
  const layerBox = layer.getBoundingClientRect();
  const dx = canvasBox.left - layerBox.left;
  const dy = canvasBox.top - layerBox.top;
  const place = (point) => {
    if (!hasMesh || !Array.isArray(point)) return null;
    const projected = meshViewport.projectPoint(point.map(Number));
    if (!projected || projected.behind) return null;
    const x = projected.x / ratio + dx;
    const y = projected.y / ratio + dy;
    return x < -20 || y < -20 || x > renderCanvas.clientWidth + 20 || y > renderCanvas.clientHeight + 20 ? null : { x, y };
  };
  const visibleObject = (id) => !id || sceneObjects.find((object) => object.id === id)?.visible !== false;
  for (const pin of layer.children) {
    const comment = pin.dataset.commentId === 'draft' ? pendingComment : feedbackComments.find((item) => item.id === pin.dataset.commentId);
    const at = comment && visibleObject(comment.objectId) ? place(comment.point) : null;
    pin.hidden = !at;
    if (at) pin.style.transform = `translate(${at.x}px, ${at.y}px)`;
    // A pin on a surface turned away from the camera is behind the model: dim it.
    const normal = comment?.normal;
    const eye = meshViewport.eye;
    const away = at && Array.isArray(normal) && Array.isArray(comment.point)
      && normal.reduce((sum, value, axis) => sum + value * (eye[axis] - comment.point[axis]), 0) < 0;
    pin.classList.toggle('behind', Boolean(away));
  }
  const composer = $('#feedbackComposer');
  if (!composer.hidden && pendingComment) {
    const at = place(pendingComment.point) || { x: renderCanvas.clientWidth / 2, y: renderCanvas.clientHeight / 2 };
    const width = composer.offsetWidth || 260;
    const height = composer.offsetHeight || 140;
    const left = Math.max(8, Math.min(renderCanvas.clientWidth - width - 8, at.x + 14));
    const below = at.y + 14 + height < renderCanvas.clientHeight - 8;
    const top = Math.max(8, below ? at.y + 14 : at.y - height - 14);
    composer.style.left = `${left}px`;
    composer.style.top = `${top}px`;
  }
}

meshViewport.onDraw = () => positionFeedbackOverlays();

// The number the next comment will get: comment numbers are never reused.
function nextCommentNumber() {
  const seen = feedbackComments.reduce((highest, comment) => Math.max(highest, Number(commentNumber(comment)) || 0), 0);
  return Math.max(feedbackLastSeq, seen) + 1;
}

// The pin cursor: a ghost of the pin that the click will place, its tip (the square
// bottom-left corner, as on a placed pin) exactly at the pointer. A DOM element rather
// than a CSS cursor image, so it is the pin itself at any pixel ratio and size.
function showPinCursor(clientX, clientY) {
  const ghost = $('#feedbackPinCursor');
  const layer = $('#viewport').getBoundingClientRect();
  ghost.textContent = String(nextCommentNumber());
  ghost.style.transform = `translate(${clientX - layer.left}px, ${clientY - layer.top}px)`;
  ghost.hidden = false;
  $('#viewport').classList.add('ghosting');
}

function hidePinCursor() {
  $('#feedbackPinCursor').hidden = true;
  $('#viewport').classList.remove('ghosting');
}

renderCanvas.addEventListener('pointermove', (event) => {
  // Mouse and pen only; a finger has no hover. Any held button is an orbit or pan.
  if (!commentMode || event.pointerType === 'touch' || event.buttons !== 0) { hidePinCursor(); return; }
  showPinCursor(event.clientX, event.clientY);
});
renderCanvas.addEventListener('pointerleave', hidePinCursor);
renderCanvas.addEventListener('pointerdown', (event) => { if (event.pointerType !== 'touch' && event.buttons !== 0) hidePinCursor(); });

function openFeedbackPanel(open = $('#feedbackPanel').hidden) {
  $('#feedbackPanel').hidden = !open;
  renderFeedback();
}

function focusComment(id) {
  activeCommentId = id;
  const comment = feedbackComments.find((item) => item.id === id);
  if (comment) feedbackTab = comment.status === 'open' ? 'open' : 'resolved';
  $('#feedbackPanel').hidden = false;
  renderFeedback();
  $(`#feedbackList [data-comment-id="${CSS.escape(id)}"]`)?.scrollIntoView({ block: 'nearest' });
}

$('#feedbackButton').addEventListener('click', () => openFeedbackPanel());
$('#feedbackPanelClose').addEventListener('click', () => openFeedbackPanel(false));
$('#feedbackAdd').addEventListener('click', () => {
  if (mobileLayout.matches) $('#feedbackPanel').hidden = true;
  enterCommentMode();
});
$('#feedbackExit').addEventListener('click', () => leaveCommentMode());
$('#feedbackComposer').addEventListener('submit', submitComment);
$('#feedbackComposerCancel').addEventListener('click', cancelComposer);
$('#feedbackText').addEventListener('keydown', (event) => {
  if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
    event.preventDefault();
    $('#feedbackComposer').requestSubmit();
  }
  if (event.key === 'Escape') {
    event.stopPropagation();
    cancelComposer();
  }
});
$$('[data-feedback-tab]').forEach((tab) => tab.addEventListener('click', () => {
  feedbackTab = tab.dataset.feedbackTab;
  renderFeedback();
}));
$('#feedbackList').addEventListener('click', async (event) => {
  const item = event.target.closest('[data-comment-id]');
  if (!item) return;
  const id = item.dataset.commentId;
  const status = event.target.closest('[data-feedback-status]')?.dataset.feedbackStatus;
  try {
    if (status) await feedbackWrite(`/${encodeURIComponent(id)}`, 'PATCH', { status });
    else if (event.target.closest('[data-feedback-delete]')) await feedbackWrite(`/${encodeURIComponent(id)}`, 'DELETE');
    else if (event.target.closest('[data-feedback-focus]')) {
      activeCommentId = activeCommentId === id ? null : id;
      renderFeedback();
    }
  } catch (error) {
    log(`Comment not updated: ${error.message}`, 'error');
  }
});
$('#feedbackPins').addEventListener('click', (event) => {
  const pin = event.target.closest('.feedback-pin[data-comment-id]');
  if (pin && pin.dataset.commentId !== 'draft') focusComment(pin.dataset.commentId);
});
document.addEventListener('keydown', (event) => {
  if (event.key !== 'Escape' || !commentMode) return;
  if (!$('#feedbackComposer').hidden) { cancelComposer(); return; }
  leaveCommentMode();
});

// A click (or tap) that did not turn into a drag places a comment in comment mode.
// Orbit, pan and pinch keep working: only a press that stays put is a click.
const COMMENT_CLICK_SLOP = 6;
let commentPress = null;
renderCanvas.addEventListener('pointerdown', (event) => {
  commentPress = commentMode && event.isPrimary && (event.pointerType === 'touch' || (event.button === 0 && !event.altKey))
    ? { id: event.pointerId, x: event.clientX, y: event.clientY, at: performance.now() }
    : null;
});
renderCanvas.addEventListener('pointerup', (event) => {
  const press = commentPress;
  commentPress = null;
  if (!press || !commentMode || event.pointerId !== press.id) return;
  if (Math.hypot(event.clientX - press.x, event.clientY - press.y) > COMMENT_CLICK_SLOP || performance.now() - press.at > 800) return;
  if (touchPoints.size > 0) return;
  placeComment(event.clientX, event.clientY);
});
renderCanvas.addEventListener('pointercancel', () => { commentPress = null; });

const MCP_MODERN_VERSION = '2026-07-28';
const MCP_PROTOCOL_VERSION = '2025-06-18';
const MCP_CLIENT_STORAGE_KEY = 'reopenscad.mcpClient';
let mcpRequestId = 0;
let mcpToolsLoaded = false;
let mcpProbeInFlight = false;
let mcpReturnFocus = null;

function mcpEndpointUrl() {
  return `${location.origin}/mcp`;
}

// Plain-language starter prompts: the agent already knows the server and its
// tools once connected, so the prompt only says what to make. `endpoint` is
// kept for callers but no longer spelled out.
function mcpAgentBriefing(endpoint, workspace) {
  if (!workspace) return 'Use reopenscad to design a toy car, then give me the design link so I can check it.';
  return `Use reopenscad to open my design. My design id is ${workspace}. Suggest one improvement, make it, then give me the design link so I can check it.`;
}

// base64 of UTF-8 JSON, the encoding Cursor's install deeplink expects in `config`.
function mcpBase64Json(value) {
  const bytes = new TextEncoder().encode(JSON.stringify(value));
  let binary = '';
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}

// One entry per agent tile. `steps[].text` is trusted HTML written here;
// `code` strings are what lands on the clipboard, so they stay literal — no
// HTML, no smart quotes, no leading indentation to strip. Anything that is not
// documented by the vendor is said to be so instead of presented as fact.
const MCP_CLIENTS = [
  {
    id: 'archestra',
    label: 'Archestra',
    tagline: 'Add to your registry',
    logo: '/logos/archestra.svg',
    doc: 'https://archestra.ai/docs/mcp/servers/adding',
    build: (ctx) => ({
      lead: 'Add ReOpenSCAD to your Archestra MCP Registry once, then give it to any of your agents.',
      steps: [
        { text: 'In Archestra open <b>MCP Registry → Add MCP Server → Start from scratch</b>. Name it <b>reopenscad</b>, choose <b>Remote</b>, and paste this address:', code: ctx.endpoint },
        { text: 'Set sign-in to <b>None</b>, click <b>Add Server</b>, then install it so its tools show up.' },
        { text: 'Add the reopenscad tools to an agent (or your MCP Gateway) and its guardrail policy.' },
      ],
      footnote: 'Adding a server needs the <em>mcpRegistry:create</em> permission in your Archestra organization.',
    }),
  },
  {
    id: 'claude-code',
    label: 'Claude Code',
    tagline: 'One command',
    logo: '/logos/claude.svg',
    doc: 'https://code.claude.com/docs/en/mcp',
    build: (ctx) => ({
      lead: 'Paste one command into your terminal. It works in every project from then on.',
      steps: [
        { text: 'Run this in a terminal:', code: `claude mcp add --scope user --transport http reopenscad ${ctx.endpoint}` },
        { text: 'Start <b>claude</b>. Type <kbd>/mcp</kbd> and you should see <b>reopenscad ✓ connected</b>.' },
      ],
      details: [
        {
          title: 'Or check it into the repo',
          caption: '.mcp.json in the project root (--scope project)',
          code: JSON.stringify({ mcpServers: { reopenscad: { type: 'http', url: ctx.endpoint } } }, null, 2),
        },
      ],
      footnote: 'The CLI transport keyword is <em>http</em>; in JSON, <em>type</em> is required and <em>streamable-http</em> is accepted as an alias.',
    }),
  },
  {
    id: 'claude',
    label: 'Claude',
    tagline: 'Desktop app and claude.ai',
    logo: '/logos/claude.svg',
    doc: 'https://support.claude.com/en/articles/11175166-getting-started-with-custom-connectors-using-remote-mcp',
    docLabel: 'Claude custom connector',
    build: (ctx) => ({
      lead: 'Add it as a custom connector. It then works in the Claude desktop app, claude.ai and the mobile apps.',
      steps: [
        { text: 'In Claude open <b>Customize → Connectors</b> (<b>Settings → Connectors</b> in older versions), click <b>+ Add</b> and choose <b>Add custom connector</b>.', action: { label: 'Open claude.ai connectors', href: 'https://claude.ai/settings/connectors', external: true, secondary: true } },
        { text: 'Name it <b>ReOpenSCAD</b>, paste this address, and click <b>Add</b>. No sign-in is needed.', code: ctx.endpoint },
        { text: 'In a chat, open the <b>+</b> menu and make sure ReOpenSCAD is switched on.' },
      ],
      details: [
        {
          title: 'Running ReOpenSCAD locally?',
          caption: 'claude_desktop_config.json, then restart Claude Desktop',
          code: JSON.stringify(
            { mcpServers: { reopenscad: { command: 'npx', args: ['-y', 'mcp-remote', ctx.endpoint, '--allow-http'] } } },
            null,
            2,
          ),
        },
      ],
      footnote: 'Custom connectors work on Free (one connector), Pro, Max, Team and Enterprise; on Team and Enterprise an owner adds it under Organization settings first. The desktop config file only speaks stdio, so a server on your own machine needs a bridge: <b>mcp-remote</b> is a community bridge, not an Anthropic-documented tool, and its flags may drift.',
    }),
  },
  {
    id: 'codex',
    label: 'Codex',
    tagline: 'One command',
    logo: '/logos/openai.svg',
    doc: 'https://developers.openai.com/codex/mcp',
    docLabel: 'Codex MCP',
    build: (ctx) => ({
      lead: 'Paste one command into your terminal. The Codex CLI and IDE extension share it.',
      steps: [
        { text: 'Run this in a terminal:', code: `codex mcp add reopenscad --url ${ctx.endpoint}` },
        { text: 'Check it is there with <b>codex mcp list</b>, then start <b>codex</b>.' },
      ],
      details: [
        { title: 'Or edit the config', caption: '~/.codex/config.toml', code: `[mcp_servers.reopenscad]\nurl = "${ctx.endpoint}"` },
      ],
    }),
  },
  {
    id: 'opencode',
    label: 'OpenCode',
    tagline: 'Paste into config',
    logo: '/logos/opencode.svg',
    doc: 'https://opencode.ai/docs/mcp-servers/',
    build: (ctx) => ({
      lead: 'Add a few lines to your OpenCode config.',
      steps: [
        {
          text: 'Open <b>opencode.json</b> (in your project, or <b>~/.config/opencode/opencode.json</b> for everywhere) and add:',
          code: JSON.stringify({ $schema: 'https://opencode.ai/config.json', mcp: { reopenscad: { type: 'remote', url: ctx.endpoint, enabled: true, oauth: false } } }, null, 2),
        },
        { text: 'Restart <b>opencode</b>. <b>opencode mcp list</b> should show reopenscad.' },
      ],
      footnote: 'If the file already exists, merge the <em>mcp</em> block into it. <em>oauth: false</em> skips sign-in detection, since this server needs none.',
    }),
  },
  {
    id: 'amp',
    label: 'Amp',
    tagline: 'One command',
    logo: '/logos/amp.svg',
    doc: 'https://ampcode.com/docs/customize/mcp',
    docLabel: 'Amp MCP',
    build: (ctx) => ({
      lead: 'Paste one command into your terminal.',
      steps: [
        { text: 'Run this in a terminal:', code: `amp mcp add reopenscad ${ctx.endpoint}` },
        { text: 'Start <b>amp</b>. <b>amp mcp doctor</b> shows whether reopenscad connected.' },
      ],
      details: [
        { title: 'Or edit the settings', caption: '~/.config/amp/settings.json', code: JSON.stringify({ 'amp.mcpServers': { reopenscad: { url: ctx.endpoint } } }, null, 2) },
      ],
      footnote: 'A server added to a project\'s <em>.amp/settings.json</em> instead waits for approval: run <b>amp mcp approve reopenscad</b>.',
    }),
  },
  {
    id: 'pi',
    label: 'Pi',
    tagline: 'Via an MCP extension',
    logo: '/logos/pi.svg',
    doc: 'https://pi.dev/packages/pi-mcp-adapter',
    docLabel: 'pi-mcp-adapter',
    build: (ctx) => ({
      lead: 'Pi has no MCP support of its own by design. A community extension, pi-mcp-adapter, adds it.',
      steps: [
        { text: 'Install the extension, then restart <b>pi</b>:', code: 'pi install npm:pi-mcp-adapter' },
        { text: 'Open <b>~/.config/mcp/mcp.json</b> (or <b>.mcp.json</b> in your project) and add:', code: JSON.stringify({ mcpServers: { reopenscad: { url: ctx.endpoint } } }, null, 2) },
        { text: 'Type <kbd>/mcp-adapter tools</kbd> in Pi to check the reopenscad tools are listed.' },
      ],
      footnote: 'pi-mcp-adapter is maintained by its own author, not by the Pi project, and Pi extensions run code on your machine. Servers connect lazily, on the first tool call.',
    }),
  },
  {
    id: 'grok',
    label: 'Grok',
    tagline: 'grok.com connector',
    logo: '/logos/grok.svg',
    doc: 'https://docs.x.ai/grok/connectors',
    docLabel: 'Grok connectors',
    build: (ctx) => ({
      lead: 'Add it as a custom connector on grok.com, or to the Grok CLI.',
      steps: [
        { text: 'On grok.com open <b>Connectors → New Connector → Custom</b>.', action: { label: 'Open Grok connectors', href: 'https://grok.com/connectors', external: true, secondary: true } },
        { text: 'Paste this address and save:', code: ctx.endpoint },
      ],
      details: [
        { title: 'Grok CLI', caption: 'terminal', code: `grok mcp add --transport http reopenscad ${ctx.endpoint}` },
        { title: 'Or edit the CLI config', caption: '~/.grok/config.toml', code: `[mcp_servers.reopenscad]\nurl = "${ctx.endpoint}"` },
      ],
      footnote: 'xAI does not document which plans include custom connectors, or whether a connector without sign-in is accepted; on Business and Enterprise an admin enables it first. CLI steps follow <a href="https://docs.x.ai/build/features/mcp-servers" target="_blank" rel="noreferrer noopener">xAI\'s MCP server docs</a>.',
    }),
  },
  {
    id: 'dot',
    label: 'Dot',
    tagline: 'Via ChatGPT plugins',
    logo: '/logos/openai.svg',
    doc: 'https://developers.openai.com/plugins/deploy/connect-chatgpt',
    docLabel: 'ChatGPT custom MCP',
    build: (ctx) => ({
      lead: 'OpenAI\'s dots use your ChatGPT plugins, so add ReOpenSCAD to ChatGPT as a custom MCP plugin.',
      steps: [
        { text: 'In ChatGPT open <b>Plugins → + → Add custom MCP server</b>.', action: { label: 'Open ChatGPT plugins', href: 'https://chatgpt.com/plugins', external: true, secondary: true } },
        { text: 'Name it <b>ReOpenSCAD</b>, paste this address, set Authentication to <b>No authentication</b>, and create it:', code: ctx.endpoint },
        { text: 'Install the plugin. Your dot can then use it; set how much it may do under <b>Settings → Plugins</b>.' },
      ],
      footnote: 'Custom MCP plugins need a paid ChatGPT plan, and on workspace plans an admin may have to allow them. That a dot picks up ChatGPT plugins without an extra step comes from third-party reports; OpenAI\'s dots help page does not spell it out yet.',
    }),
  },
  {
    id: 'cursor',
    label: 'Cursor',
    tagline: 'One click',
    logo: '/logos/cursor.svg',
    doc: 'https://cursor.com/docs/context/mcp',
    build: (ctx) => ({
      lead: 'One click installs it. Cursor asks you to confirm.',
      steps: [
        {
          text: 'Click to add ReOpenSCAD to Cursor:',
          action: { label: 'Add to Cursor', href: `cursor://anysphere.cursor-deeplink/mcp/install?name=reopenscad&config=${encodeURIComponent(mcpBase64Json({ url: ctx.endpoint }))}` },
        },
        { text: 'In Cursor, click <b>Install</b>, then check it is enabled under <b>Settings → Tools &amp; MCP</b>. Use it from Agent chat.' },
      ],
      details: [
        {
          title: 'Or edit the config',
          caption: '~/.cursor/mcp.json (everywhere) · .cursor/mcp.json (one project)',
          code: JSON.stringify({ mcpServers: { reopenscad: { url: ctx.endpoint } } }, null, 2),
        },
      ],
      footnote: 'The button is Cursor\'s documented install link; it only works with Cursor installed. Cursor treats any entry with a <em>url</em> as a remote server, no <em>type</em> needed.',
    }),
  },
  {
    id: 'hermes',
    label: 'Hermes Agent',
    tagline: 'Paste into config',
    logo: '/logos/hermes.svg',
    doc: 'https://hermes-agent.nousresearch.com/docs/user-guide/features/mcp',
    docLabel: 'Hermes Agent MCP',
    build: (ctx) => ({
      lead: 'Add a few lines to the Hermes config file.',
      steps: [
        { text: 'Open <b>~/.hermes/config.yaml</b> and add:', code: `mcp_servers:\n  reopenscad:\n    url: "${ctx.endpoint}"` },
        { text: 'Type <kbd>/reload-mcp</kbd> in Hermes (or restart it). <b>hermes mcp test reopenscad</b> checks the connection.' },
      ],
      footnote: 'If <em>mcp_servers</em> already exists, add only the <em>reopenscad</em> entry under it.',
    }),
  },
  {
    id: 'openclaw',
    label: 'OpenClaw',
    tagline: 'Formerly Clawdbot',
    logo: '/logos/openclaw.svg',
    doc: 'https://docs.openclaw.ai/tools/mcp',
    build: (ctx) => ({
      lead: 'OpenClaw (formerly Clawdbot, then Moltbot) adds it with one command.',
      steps: [
        { text: 'Run this where OpenClaw is installed:', code: `openclaw mcp add reopenscad --url ${ctx.endpoint} --transport streamable-http` },
        { text: 'Check it with <b>openclaw mcp doctor reopenscad --probe</b>.' },
      ],
      details: [
        {
          title: 'Or edit the config',
          caption: '~/.openclaw/openclaw.json',
          code: JSON.stringify({ mcp: { servers: { reopenscad: { url: ctx.endpoint, transport: 'streamable-http', enabled: true } } } }, null, 2),
        },
      ],
      footnote: 'The Control UI does the same under <b>Settings → MCP → Add server → Streamable HTTP</b>.',
    }),
  },
  {
    id: 'other',
    label: 'Other agents',
    title: 'Connect any MCP client',
    tagline: 'Any MCP client',
    logo: '/logos/mcp.svg',
    doc: 'https://modelcontextprotocol.io/specification/2025-06-18/basic/transports',
    docLabel: 'MCP transport',
    build: (ctx) => ({
      lead: 'Any agent that supports remote MCP servers can use ReOpenSCAD. Look for "Add MCP server" or "custom connector" in its settings.',
      steps: [
        { text: 'Give it this address. It is plain HTTP MCP: nothing to install, no sign-in.', code: ctx.endpoint },
        { text: 'If it wants a config file instead, most agents take this shape:', code: JSON.stringify({ mcpServers: { reopenscad: { url: ctx.endpoint } } }, null, 2) },
      ],
      details: [
        {
          title: 'VS Code',
          caption: '.vscode/mcp.json — the key is servers, and type is required',
          code: JSON.stringify({ servers: { reopenscad: { type: 'http', url: ctx.endpoint } } }, null, 2),
        },
        {
          title: 'Windsurf',
          caption: '~/.codeium/windsurf/mcp_config.json — note serverUrl, not url',
          code: JSON.stringify({ mcpServers: { reopenscad: { serverUrl: ctx.endpoint } } }, null, 2),
        },
        {
          title: 'Zed',
          caption: 'settings file — top-level key context_servers',
          code: JSON.stringify({ context_servers: { reopenscad: { url: ctx.endpoint } } }, null, 2),
        },
        {
          title: 'Gemini CLI',
          caption: '~/.gemini/settings.json — note httpUrl',
          code: JSON.stringify({ mcpServers: { reopenscad: { httpUrl: ctx.endpoint, timeout: 5000 } } }, null, 2),
        },
        {
          title: 'Cline',
          caption: 'MCP panel → Configure — note the camelCase transport',
          code: JSON.stringify({ mcpServers: { reopenscad: { type: 'streamableHttp', url: ctx.endpoint, disabled: false, autoApprove: [] } } }, null, 2),
        },
        {
          title: 'Or wire it by hand',
          caption: `current era (${MCP_MODERN_VERSION}): stateless, version and capabilities per request`,
          code: `curl -s ${ctx.endpoint} \\\n  -H 'Content-Type: application/json' \\\n  -H 'MCP-Protocol-Version: ${MCP_MODERN_VERSION}' \\\n  -H 'Mcp-Method: server/discover' \\\n  -d '{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"${MCP_MODERN_VERSION}","io.modelcontextprotocol/clientCapabilities":{}}}}'`,
        },
        {
          title: 'Older clients still work',
          caption: `initialize-based era (${MCP_PROTOCOL_VERSION} and earlier): handshake, then MCP-Protocol-Version on every later call`,
          code: `curl -s ${ctx.endpoint} \\\n  -H 'Content-Type: application/json' \\\n  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"${MCP_PROTOCOL_VERSION}","capabilities":{},"clientInfo":{"name":"demo","version":"1"}}}'`,
        },
      ],
      footnote: 'This server is dual-era: it answers the current stateless revision <em>and</em> the older <em>initialize</em> handshake on the same endpoint. POST only: a <em>GET</em> answers 405 by design, since there is no SSE stream.',
    }),
  },
];

function storedMcpClientId() {
  try {
    return localStorage.getItem(MCP_CLIENT_STORAGE_KEY);
  } catch {
    return null;
  }
}

function rememberMcpClient(id) {
  try {
    localStorage.setItem(MCP_CLIENT_STORAGE_KEY, id);
  } catch {
    // Only costs the "last used" highlight next time.
  }
}

function findMcpClient(id) {
  return MCP_CLIENTS.find((client) => client.id === id) || null;
}

function agentLogoHtml(client) {
  return `<span class="agent-logo${client.photo ? ' photo' : ''}" aria-hidden="true"><img src="${escapeHtml(client.logo)}" alt="" loading="lazy" decoding="async" /></span>`;
}

function renderMcpTabs() {
  const last = storedMcpClientId();
  $('#mcpTabs').innerHTML = MCP_CLIENTS
    .map((client) => `<li><button type="button" class="connect-tile${client.id === last ? ' last-used' : ''}" data-client="${client.id}">${agentLogoHtml(client)}<span>${escapeHtml(client.label)}<small>${escapeHtml(client.tagline)}</small></span></button></li>`)
    .join('');
}

function mcpCopyBox(code) {
  return `<div class="connect-copybox"><pre>${escapeHtml(code)}</pre><button type="button" class="mcp-copy"><i></i><span>Copy</span></button></div>`;
}

function renderMcpClient(client) {
  const endpoint = mcpEndpointUrl();
  const workspace = workspaceId || '';
  const view = client.build({ endpoint, workspace });
  const steps = view.steps
    .map((step) => `<li class="connect-step"><div>
        <p>${step.text}</p>
        ${step.action ? `<a class="connect-action${step.action.secondary ? ' secondary' : ''}" href="${escapeHtml(step.action.href)}"${step.action.external ? ' target="_blank" rel="noreferrer noopener"' : ''}>${escapeHtml(step.action.label)}</a>` : ''}
        ${step.code ? mcpCopyBox(step.code) : ''}
      </div></li>`)
    .join('');
  // Every walkthrough ends the same way: a first message that names this design.
  const hello = view.hello === false
    ? ''
    : `<li class="connect-step"><div><p>${workspace ? 'Ask it to work on this design:' : 'Ask it to make something:'}</p>${mcpCopyBox(mcpAgentBriefing(endpoint, workspace))}</div></li>`;
  const snippets = (view.details || [])
    .map((snippet) => `<div class="mcp-snippet">
        <div class="mcp-snippet-head"><b>${escapeHtml(snippet.title)}</b><small>${escapeHtml(snippet.caption)}</small><button type="button" class="mcp-copy"><i></i><span>Copy</span></button></div>
        <pre>${escapeHtml(snippet.code)}</pre>
      </div>`)
    .join('');
  const notes = (view.notes || []).map((note) => `<p class="connect-note">${note}</p>`).join('');
  $('#mcpClient').innerHTML = `<button type="button" class="connect-back" data-connect-back>← All agents</button>
    <div class="connect-guide-head">${agentLogoHtml(client)}<div><h3>${escapeHtml(client.label)}</h3><p>${view.lead}</p></div></div>
    ${notes}
    <ol class="connect-steps">${steps}${hello}</ol>
    <details class="connect-more">
      <summary>Show details</summary>
      <div>
        ${snippets}
        ${view.footnote ? `<p class="mcp-note">${view.footnote}</p>` : ''}
        <span class="mcp-verified">Steps follow <a href="${escapeHtml(client.doc)}" target="_blank" rel="noreferrer noopener">the official ${escapeHtml(client.docLabel || client.label)} documentation</a>.</span>
      </div>
    </details>`;
}

function showMcpPicker() {
  renderMcpTabs();
  $('#connectPicker').hidden = false;
  $('#mcpClient').hidden = true;
  $('#mcpTitle').textContent = 'Let your AI agent build with you';
}

function showMcpClient(id) {
  const client = findMcpClient(id);
  if (!client) return showMcpPicker();
  rememberMcpClient(client.id);
  renderMcpClient(client);
  $('#connectPicker').hidden = true;
  $('#mcpClient').hidden = false;
  $('#mcpTitle').textContent = client.title || `Connect ${client.label}`;
  $('#mcpClient').scrollIntoView?.({ block: 'nearest' });
}

// "Connect your agent" grid, the landing call to action: every agent in MCP_CLIENTS,
// most asked-for first, each opening its own walkthrough.
const LANDING_AGENT_IDS = ['claude-code', 'claude', 'codex', 'cursor', 'archestra', 'opencode', 'amp', 'pi', 'grok', 'hermes', 'openclaw', 'dot', 'other'];
const LANDING_AGENT_NAMES = { claude: 'Claude app', hermes: 'Hermes', other: 'Any MCP client' };

function renderLandingAgents() {
  $('#landingAgents').innerHTML = LANDING_AGENT_IDS
    .map((id) => MCP_CLIENTS.find((client) => client.id === id))
    .filter(Boolean)
    .map((client) => {
      const logo = client.logo.replace(/^.*\/|\.[a-z]+$/g, '');
      return `<li><button type="button" data-agent="${client.id}" title="Connect ${escapeHtml(client.label)}"><span class="agent-mark" data-logo="${escapeHtml(logo)}" aria-hidden="true"><img src="${escapeHtml(client.logo)}" alt="" decoding="async" /></span><span>${escapeHtml(LANDING_AGENT_NAMES[client.id] || client.label)}</span></button></li>`;
    })
    .join('');
}

function renderMcpTools(tools) {
  const grid = $('#mcpToolGrid');
  if (!tools.length) {
    grid.innerHTML = '<div class="mcp-tools-empty">The server advertised no tools.</div>';
    return;
  }
  // Pre-rename tool names are still served for older agents, but a person
  // setting up a new one only needs the current names.
  grid.innerHTML = tools
    .filter((tool) => !String(tool.description || '').startsWith('Deprecated alias'))
    .map((tool) => `<div class="mcp-tool"><b>${escapeHtml(tool.name)}</b><span>${escapeHtml(tool.description || '')}</span></div>`)
    .join('');
}

function setMcpStatus(state, message) {
  const status = $('#mcpStatus');
  status.dataset.state = state;
  $('span', status).textContent = message;
  // The details summary carries a one-word verdict so nobody has to open it to know.
  const summary = $('#mcpStatusSummary');
  summary.dataset.state = state;
  summary.textContent = state === 'ok' ? '· server online' : state === 'error' ? '· server not reachable' : 'for developers';
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
  // Either a named element (the endpoint, the workspace id) or the snippet / copy box this button sits in.
  const targetId = button.dataset.copy;
  const text = targetId
    ? document.getElementById(targetId)?.textContent ?? ''
    : button.closest('.mcp-snippet, .connect-copybox')?.querySelector('pre')?.textContent ?? '';
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

// `agentId` opens straight on that agent's walkthrough (the landing strip);
// without it the panel starts at the picker.
function openMcpPanel(agentId) {
  $('#mcpEndpoint').textContent = mcpEndpointUrl();
  $('#mcpWorkspaceId').textContent = workspaceId || 'not created yet';
  if (agentId) showMcpClient(agentId);
  else showMcpPicker();
  mcpReturnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  $('#mcpModal').hidden = false;
  $('#closeMcp').focus();
  // The page is served by the same process that answers /mcp, so a first probe
  // on open costs one round trip and turns the panel's claims into evidence.
  if (!mcpToolsLoaded) probeMcp();
}

function closeMcpPanel() {
  if ($('#mcpModal').hidden) return;
  $('#mcpModal').hidden = true;
  const restore = mcpReturnFocus;
  mcpReturnFocus = null;
  // Opened from the landing strip, the toolbar button is not on screen.
  if (restore && restore.isConnected && restore.offsetParent !== null) restore.focus();
  else if ($('#mcpButton').offsetParent !== null) $('#mcpButton').focus();
}

$('#mcpButton').addEventListener('click', () => {
  openMcpPanel();
});
$('#closeMcp').addEventListener('click', closeMcpPanel);
$('#mcpScrim').addEventListener('click', closeMcpPanel);
$('#mcpTest').addEventListener('click', probeMcp);
$('#mcpTabs').addEventListener('click', (event) => {
  const tile = event.target.closest('[data-client]');
  if (!tile) return;
  showMcpClient(tile.dataset.client);
  $('[data-connect-back]', $('#mcpClient'))?.focus();
});
$('#mcpModal').addEventListener('click', (event) => {
  if (event.target.closest('[data-connect-back]')) {
    showMcpPicker();
    $('.connect-tile', $('#mcpTabs'))?.focus();
    return;
  }
  const button = event.target.closest('.mcp-copy');
  if (button) copyForMcp(button);
});
renderLandingAgents();
$('#landingAgents').addEventListener('click', (event) => {
  const agent = event.target.closest('[data-agent]');
  if (agent) openMcpPanel(agent.dataset.agent);
});
window.addEventListener('keydown', (event) => {
  if (event.key === 'Escape') closeMcpPanel();
});

const syncControl = $('#syncState');
syncControl.setAttribute('role', 'button');
syncControl.tabIndex = 0;
// Offered from the sync pill once a newer server revision has set unsaved edits
// aside. Restoring is an explicit choice to write them over that revision, so it
// goes through the normal save path against the revision now on screen.
function resolveWorkspaceConflict() {
  if (!displacedEdits) return;
  let resolver = document.getElementById('workspaceConflictResolver');
  if (resolver) {
    resolver.remove();
    return;
  }
  resolver = document.createElement('div');
  resolver.id = 'workspaceConflictResolver';
  resolver.className = 'workspace-conflict-resolver';
  resolver.setAttribute('role', 'dialog');
  resolver.setAttribute('aria-label', 'Unsaved edits set aside');
  resolver.innerHTML = '<b>Your edits were set aside</b><p>A newer revision of this design was saved on the server (by an AI agent or another tab) and is now in the editor. Your unsaved edits are kept here.</p><div><button type="button" data-resolution="discard">Discard mine</button><button type="button" data-resolution="restore">Restore mine</button></div>';
  document.body.append(resolver);
  resolver.addEventListener('click', (event) => {
    const resolution = event.target.closest('[data-resolution]')?.dataset.resolution;
    if (!resolution || !displacedEdits) return;
    const { code } = displacedEdits;
    clearDisplacedEdits();
    if (resolution === 'restore') {
      aiUpdatedRanges = [];
      setAiBadge(false);
      editor.value = code;
      syncEditor();
      log('Restored your edits on top of the latest revision.', 'info');
      render('preview');
    }
  });
  const rect = syncControl.getBoundingClientRect();
  resolver.style.top = `${rect.bottom + 8}px`;
  resolver.style.right = `${Math.max(12, innerWidth - rect.right)}px`;
  resolver.querySelector('[data-resolution="restore"]').focus();
}
syncControl.addEventListener('click', resolveWorkspaceConflict);
syncControl.addEventListener('keydown', (event) => {
  if (displacedEdits && (event.key === 'Enter' || event.key === ' ')) {
    event.preventDefault();
    resolveWorkspaceConflict();
  }
});
$('#dismissAiHighlight').addEventListener('click', () => {
  aiUpdatedRanges = [];
  setAiBadge(false);
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

// --- Left pane: the two lists' heights ---------------------------------------------
//
// Objects and Revisions are sized by hand with a divider each; the tools between them keep
// their natural size. A per-browser view setting, like the tolerance.

const PANEL_HEIGHTS_STORAGE_KEY = 'reopenscad.panelHeights';
const PANEL_LIST_MIN = 56; // About two rows.
const PANEL_LIST_FLOOR = 28; // About one, when the window is too short for two.
const PANEL_OBJECTS_SHARE = 0.4;
const PANEL_KEY_STEP = 14;

// Pure: stored values that make sense, null for "the default".
function parsePanelHeights(raw) {
  let stored = null;
  try { stored = JSON.parse(raw); } catch { /* Not ours: the defaults stand. */ }
  const height = (value) => (Number.isFinite(value) && value > 0 ? value : null);
  return { objects: height(stored?.objects), revisions: height(stored?.revisions) };
}

// Pure: what each list gets. `free` is the pane's height less everything but the lists.
// By default Objects fits its content up to a share of the pane and Revisions takes the rest.
// Set by hand either can be any height the other's minimum leaves, past its content too: a
// divider that cannot move reads as broken. A window too short for two rows each gets down to
// one; shorter still, the tools give way (they scroll).
function panelListLayout({ paneHeight, free, objectsContent, revisionsContent }, stored) {
  const cramped = free < Math.min(PANEL_LIST_MIN, objectsContent) + Math.min(PANEL_LIST_MIN, revisionsContent);
  const least = cramped ? PANEL_LIST_FLOOR : PANEL_LIST_MIN;
  const objectsMin = Math.min(least, objectsContent);
  const revisionsMin = Math.min(least, revisionsContent);
  const room = Math.max(free, objectsMin + revisionsMin);
  const objectsMax = room - revisionsMin;
  const revisionsMax = room - objectsMin;
  const clamp = (value, min, max) => Math.round(Math.min(max, Math.max(min, value)));
  const objects = clamp(stored.objects ?? Math.min(objectsContent, paneHeight * PANEL_OBJECTS_SHARE), objectsMin, objectsMax);
  const revisions = clamp(Math.min(stored.revisions ?? Infinity, room - objects), revisionsMin, revisionsMax);
  return { room, objects, objectsMin, objectsMax, revisions, revisionsMin, revisionsMax };
}

// Pure: the stored heights once a list is asked to be `target` tall, from those it had when
// the gesture began. Objects pushes Revisions down, as its own height stays stored for when
// there is room again. Revisions growing into Objects takes from it, and gives back within
// the same gesture.
function resizePanelList(which, target, start, metrics) {
  const layout = panelListLayout(metrics, start);
  if (which === 'objects') {
    return { ...start, objects: Math.round(Math.min(layout.objectsMax, Math.max(layout.objectsMin, target))) };
  }
  const revisions = Math.round(Math.min(layout.revisionsMax, Math.max(layout.revisionsMin, target)));
  const objects = panelListLayout(metrics, { objects: start.objects, revisions: null }).objects;
  return { objects: objects + revisions > layout.room ? layout.room - revisions : start.objects, revisions };
}

let panelHeights = { objects: null, revisions: null };
try { panelHeights = parsePanelHeights(localStorage.getItem(PANEL_HEIGHTS_STORAGE_KEY)); } catch { /* Storage is off. */ }
let panelLayoutFrame = null;

function savePanelHeights() {
  try { localStorage.setItem(PANEL_HEIGHTS_STORAGE_KEY, JSON.stringify(panelHeights)); } catch { /* Lasts until a reload. */ }
}

// From the first row's top to the last row's bottom: unlike scrollHeight, not floored by the
// height the list has now.
function panelListContent(list) {
  const { firstElementChild: first, lastElementChild: last } = list;
  return first ? last.offsetTop + last.offsetHeight - first.offsetTop : 0;
}

// Null where the dividers are not in the layout: a phone's sheet, or no pane at all.
function measurePanelLists() {
  const pane = $('.objects-pane');
  const dividers = [$('#objectsDivider'), $('#revisionsDivider')];
  if (!dividers[0].offsetParent) return null;
  const section = $('#revisionsSection');
  const list = $('#revisionList');
  // scrollHeight: what the tools need, even while they are giving way.
  const fixed = $('.objects-title').offsetHeight + $('#panelTools').scrollHeight + dividers[0].offsetHeight + dividers[1].offsetHeight + section.offsetHeight - list.offsetHeight;
  return {
    paneHeight: pane.clientHeight,
    free: pane.clientHeight - fixed,
    objectsContent: panelListContent($('#objectList')),
    revisionsContent: panelListContent(list),
  };
}

function applyPanelLayout() {
  panelLayoutFrame = null;
  const metrics = measurePanelLists();
  if (!metrics) return;
  const layout = panelListLayout(metrics, panelHeights);
  for (const [divider, list, height, min, max] of [
    ['#objectsDivider', '#objectList', layout.objects, layout.objectsMin, layout.objectsMax],
    ['#revisionsDivider', '#revisionList', layout.revisions, layout.revisionsMin, layout.revisionsMax],
  ]) {
    $(list).style.setProperty('--list-height', `${height}px`);
    $(divider).setAttribute('aria-valuenow', String(height));
    $(divider).setAttribute('aria-valuemin', String(min));
    $(divider).setAttribute('aria-valuemax', String(max));
  }
}

// One layout a frame, however many pointer moves or re-rendered lists asked for it.
function schedulePanelLayout() {
  panelLayoutFrame ??= requestAnimationFrame(applyPanelLayout);
}

// Pure: the height a key asks for, or null if the key is not for this. A divider moves the way
// the arrow points, so Down makes the list above it taller and the list below it shorter.
function panelKeyTarget(which, key, layout, step = PANEL_KEY_STEP) {
  const growsDown = which === 'objects';
  const current = layout[which];
  if (key === 'ArrowDown') return current + (growsDown ? step : -step);
  if (key === 'ArrowUp') return current + (growsDown ? -step : step);
  if (key === 'Home') return layout[`${which}Min`];
  if (key === 'End') return layout[`${which}Max`];
  return null;
}

function bindPanelDivider(which, selector) {
  const divider = $(selector);
  const growsDown = which === 'objects';
  let drag = null;
  divider.addEventListener('pointerdown', (event) => {
    const metrics = measurePanelLists();
    if (!metrics || event.button !== 0) return;
    // No text selection starting here; that also skips the focus a press gives.
    event.preventDefault();
    divider.focus({ preventScroll: true });
    divider.setPointerCapture(event.pointerId);
    divider.classList.add('dragging');
    drag = { y: event.clientY, height: panelListLayout(metrics, panelHeights)[which], start: panelHeights, moved: false };
  });
  divider.addEventListener('pointermove', (event) => {
    if (!drag || !divider.hasPointerCapture(event.pointerId)) return;
    const metrics = measurePanelLists();
    if (!metrics) return;
    const delta = event.clientY - drag.y;
    panelHeights = resizePanelList(which, drag.height + (growsDown ? delta : -delta), drag.start, metrics);
    drag.moved = true;
    schedulePanelLayout();
  });
  const finish = () => {
    if (!drag) return;
    if (drag.moved) savePanelHeights();
    drag = null;
    divider.classList.remove('dragging');
  };
  divider.addEventListener('pointerup', finish);
  divider.addEventListener('pointercancel', finish);
  divider.addEventListener('lostpointercapture', finish);
  divider.addEventListener('dblclick', () => {
    panelHeights = { ...panelHeights, [which]: null };
    savePanelHeights();
    schedulePanelLayout();
  });
  divider.addEventListener('keydown', (event) => {
    const metrics = measurePanelLists();
    const target = metrics && panelKeyTarget(which, event.key, panelListLayout(metrics, panelHeights));
    if (target === null || target === undefined) return;
    event.preventDefault();
    panelHeights = resizePanelList(which, target, panelHeights, metrics);
    savePanelHeights();
    schedulePanelLayout();
  });
}

bindPanelDivider('objects', '#objectsDivider');
bindPanelDivider('revisions', '#revisionsDivider');
// What moves the room: the window, a tool growing (findings, the scale, an upload), and rows.
const panelResizeObserver = new ResizeObserver(schedulePanelLayout);
panelResizeObserver.observe($('.objects-pane'));
// Each section, as #panelTools keeps its size while it is the part giving way.
for (const tool of $$('#panelTools > .tool')) panelResizeObserver.observe(tool);
const panelRowObserver = new MutationObserver(schedulePanelLayout);
panelRowObserver.observe($('#objectList'), { childList: true });
panelRowObserver.observe($('#revisionList'), { childList: true });
schedulePanelLayout();

window.addEventListener('keydown', (event) => {
  // The landing page offers exactly one action, and it has its own button.
  if (landingVisible()) return;
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

// Resolves true only if this view is still the one on screen when the answer
// lands. Navigating away bumps the generation, which aborts the request; a
// reply (or failure) that arrives after that is dropped, never written into
// whatever editor the user moved on to.
async function loadSharedWorkspace(token) {
  readOnlyToken = token;
  const generation = advanceWorkspaceGeneration();
  const current = () => generation === workspaceGeneration && readOnlyToken === token;
  // No id, so nothing can save, poll, upload a preview or mint a share link.
  workspaceId = null;
  workspaceOnline = false;
  document.body.classList.add('read-only');
  $('#readonlyBanner').hidden = false;
  editor.readOnly = true;
  $('#shareButton').hidden = true;
  setSyncState('Read only', 'local');
  try {
    const response = await fetch(`/api/shared/${encodeURIComponent(token)}`, { signal: workspaceLoadSignal() });
    if (!current()) return false;
    if (!response.ok) throw new Error(`shared ${response.status}`);
    const payload = await response.json();
    if (!current()) return false;
    applySharedPayload(payload);
    return true;
  } catch {
    if (!current()) return false;
    log('This read-only link is not valid. It may have been removed.', 'error');
    setStatus('Ready');
    return true;
  }
}

function applySharedPayload(payload) {
  const code = typeof payload.code === 'string' ? payload.code : '';
  if (hasMesh && code !== editor.value) markGeometryStale();
  editor.value = code;
  readOnlyRevision = Number(payload.revision) || 0;
  syncEditor(false);
  applyWorkspaceSettings(payload);
  refreshRevisions(0);
}

// Re-reads a shared design after its render route reported a newer revision.
// True only when the reply landed while that render is still the live one, so
// a slow reload can never rewind a newer one.
async function reloadSharedSource(token, request, superseded) {
  const current = () => !superseded() && readOnlyToken === token;
  try {
    const response = await fetch(`/api/shared/${encodeURIComponent(token)}`, { cache: 'no-store', signal: request.controller.signal });
    if (!current() || !response.ok) return false;
    const payload = await response.json();
    if (!current()) return false;
    applySharedPayload(payload);
    return true;
  } catch {
    return false;
  }
}

// Every way out of a read-only view goes through here — the fork button, but
// also New and Open, which are still live and would otherwise leave the banner
// up over a workspace that is perfectly editable.
function leaveReadOnly() {
  if (!readOnlyToken) return;
  readOnlyToken = null;
  readOnlyRevision = 0;
  document.body.classList.remove('read-only');
  $('#readonlyBanner').hidden = true;
  editor.readOnly = false;
  $('#shareButton').hidden = false;
}

// The way out a reader is offered: the same source in a workspace of their
// own. Nothing is copied beyond what they can already see, so this needs no
// permission they do not have.
// The copy is made server side so the design's imported meshes come along
// (the reader cannot upload them: a read-only link never names the design).
function sharedCloneRequest(token, code) {
  return {
    url: `/api/shared/${encodeURIComponent(token)}/clone`,
    init: {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ code }),
    },
  };
}

let forkInFlight = false;
$('#forkWorkspace').addEventListener('click', async () => {
  const token = readOnlyToken;
  if (!token || forkInFlight) return;
  forkInFlight = true;
  const code = editor.value;
  const generation = workspaceGeneration;
  try {
    const { url, init } = sharedCloneRequest(token, code);
    let payload = null;
    let response = null;
    try {
      response = await fetch(url, init);
      payload = await response.json().catch(() => null);
    } catch {
      response = null;
    }
    // The reader moved elsewhere meanwhile; the copy exists but is not opened.
    if (generation !== workspaceGeneration || token !== readOnlyToken) return;
    if (!response || !response.ok || payload?.ok === false || !workspaceFromPayload(payload || {}).id) {
      const reason = payload?.error || (response ? `HTTP ${response.status}` : 'the server could not be reached');
      log(`Could not copy this design: ${reason}`, 'error');
      return;
    }
    leaveReadOnly();
    hideLanding();
    const opened = advanceWorkspaceGeneration();
    nextWorkspaceNavigation = 'push';
    workspaceOnline = true;
    applyWorkspace(payload);
    setSyncState('Synced', 'synced');
    if (opened === workspaceGeneration) beginWorkspacePolling(opened);
    if (editor.value.trim()) render('preview');
  } finally {
    forkInFlight = false;
  }
});

// `/designs/<id>` is the canonical route. Links from before the rename
// (/workspaces/<id>, ?workspace=<id>) open the same design, and loading it
// replaces the address with the canonical form.
function workspaceIdFromLocation() {
  const routeMatch = location.pathname.match(/^\/(?:designs|workspaces)\/([^/]+)\/?$/);
  if (routeMatch) return decodeURIComponent(routeMatch[1]);
  const query = new URLSearchParams(location.search);
  return query.get('design') || query.get('workspace');
}

// --- Landing page ----------------------------------------------------------
//
// "/" used to mint a fresh workspace on every visit, which filled the server
// with empties and gave nobody a way back to the work they already had. It is
// now a front door: what this is, the workspaces this browser has opened, and
// one button that creates a workspace — the only thing that does.

// The source cache from before workspaces had ids. It used to be cloned into a
// brand new workspace on every visit to "/", and later simply deleted; neither
// is right for the one copy of someone's work. It is offered on the landing
// page instead and removed only once it is safely in a workspace.
const LEGACY_DRAFT_KEY = 'reopenscad.code';

function readLegacyDraft() {
  try {
    const code = localStorage.getItem(LEGACY_DRAFT_KEY);
    return code && code.trim() ? code : null;
  } catch {
    return null;
  }
}

function landingVisible() {
  return document.documentElement.classList.contains('landing-mode');
}

function formatVisited(timestamp) {
  if (!timestamp) return '';
  const minutes = Math.round((Date.now() - timestamp) / 60000);
  if (minutes < 1) return 'just now';
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours} h ago`;
  const days = Math.round(hours / 24);
  if (days < 30) return `${days} d ago`;
  return new Date(timestamp).toLocaleDateString();
}

function renderLandingRecents() {
  const list = $('#landingRecents');
  const entries = readRecents();
  const draft = readLegacyDraft();
  const rows = [];
  if (draft) {
    const firstLine = draft.trim().split('\n')[0].slice(0, 80);
    rows.push('<li class="landing-draft"><button type="button" class="landing-open" data-landing-draft>'
      + '<span class="landing-thumb" aria-hidden="true">{ }</span>'
      + '<span class="landing-copy"><b>Unsaved draft from an earlier version</b>'
      + `<small>${escapeHtml(firstLine)}</small></span>`
      + '<span class="landing-when">open as new design</span></button></li>');
  }
  const shown = recentsExpanded ? entries : entries.slice(0, RECENTS_SHOWN);
  for (const entry of shown) {
    const id = escapeAttribute(entry.id);
    const label = entry.id.split('-').slice(0, 2).join(' ') || entry.id;
    // An empty workspace has no render to show, so it says what it is instead
    // of leaving a blank thumbnail that reads as a failed image.
    const thumb = entry.empty
      ? '<span class="landing-thumb empty" aria-hidden="true">{ }</span>'
      : `<span class="landing-thumb" data-landing-preview="${id}" aria-hidden="true"></span>`;
    rows.push(`<li${entry.empty ? ' class="landing-empty-workspace"' : ''}><button type="button" class="landing-open" data-landing-workspace="${id}">`
      + thumb
      + `<span class="landing-copy"><b>${escapeHtml(label)}</b><small>${entry.empty ? '<i>Empty</i> · ' : ''}${escapeHtml(entry.id)}</small></span>`
      + `<span class="landing-when">${escapeHtml(formatVisited(entry.visitedAt))}</span></button>`
      + `<button type="button" class="landing-forget" data-forget="${id}" aria-label="Remove ${escapeAttribute(label)} from the list" title="Remove from this list">&times;</button></li>`);
  }
  list.innerHTML = rows.length
    ? rows.join('')
    : '<li class="landing-empty">Nothing here yet. Designs you create or open in this browser are listed here.</li>';
  // Folded rows are not rendered at all, so their previews are not fetched
  // until the user asks for them.
  if (entries.length > RECENTS_SHOWN) {
    list.insertAdjacentHTML('beforeend', `<li class="landing-more"><button type="button" data-recents-toggle aria-expanded="${recentsExpanded}">`
      + (recentsExpanded ? 'Show fewer' : `Show all (${entries.length})`) + '</button></li>');
  }
  for (const slot of $$('[data-landing-preview]', list)) {
    const id = slot.dataset.landingPreview;
    fetch(`/api/designs/${encodeURIComponent(id)}/preview`)
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

let landingMissing = null;

function showLanding({ missing = null } = {}) {
  closeForgetPrompt();
  closeMobileSheet();
  leaveReadOnly();
  advanceWorkspaceGeneration();
  workspaceId = null;
  workspaceRevision = 0;
  workspaceSavedCode = '';
  resetScene();
  document.documentElement.classList.add('landing-mode');
  editor.value = '';
  syncEditor(false);
  setSyncState('Local', 'local');
  landingMissing = missing;
  $('#landingMissing').hidden = !missing;
  if (missing) {
    $('#landingMissingId').textContent = missing.id;
    $('#landingRecoverMissing').hidden = !missing.code?.trim();
    // The landing page lives at "/", so the address bar says so too.
    if (location.pathname !== '/') history.replaceState(null, '', '/');
  }
  renderLandingRecents();
  renderLandingExamples();
  loadLandingStats();
}

// The public creation counters, the hero subhead's last sentence. Any
// failure — an older server without the route, a malformed body — leaves the
// sentence hidden, so the paragraph ends cleanly. The reply is cacheable for
// 30 s, so revisiting "/" is free. Only the latest request may touch it, so a
// slow earlier one cannot overwrite or hide what a newer one showed.
let landingStatsToken = 0;
async function loadLandingStats() {
  const line = $('#landingStats');
  const token = ++landingStatsToken;
  try {
    const response = await fetch('/api/stats', { headers: { Accept: 'application/json' } });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const stats = await response.json();
    if (token !== landingStatsToken) return;
    // `designs*` since the rename; `workspaces*` from a server that predates it.
    const total = stats.designsCreated ?? stats.workspacesCreated;
    const day = stats.designsCreatedLast24h ?? stats.workspacesCreatedLast24h;
    // Servers from before the weekly count omit it; the line drops that part.
    const week = stats.designsCreatedLast7d;
    const valid = (count) => Number.isSafeInteger(count) && count >= 0;
    if (!valid(total) || !valid(day) || (week !== undefined && !valid(week))) throw new Error('Malformed stats');
    const format = new Intl.NumberFormat();
    $('#landingStatsDay').textContent = format.format(day);
    $('#landingStatsWeek').textContent = week === undefined ? '' : format.format(week);
    for (const part of document.querySelectorAll('[data-stats-week]')) part.hidden = week === undefined;
    $('#landingStatsDayLabel').textContent = day === 1 ? 'design' : 'designs';
    $('#landingStatsTotal').textContent = format.format(total);
    line.hidden = false;
  } catch {
    if (token === landingStatsToken) line.hidden = true;
  }
}

function hideLanding() {
  if (!landingVisible()) return;
  document.documentElement.classList.remove('landing-mode');
  landingMissing = null;
  $('#landingMissing').hidden = true;
  // The shell was display:none while the landing page was up, so the canvas
  // has no size until it is laid out again.
  requestAnimationFrame(() => meshViewport.draw());
}

async function openWorkspaceRoute() {
  const sharedToken = shareTokenFromLocation();
  const id = workspaceIdFromLocation();
  if (sharedToken) {
    hideLanding();
    resetScene();
    if (!(await loadSharedWorkspace(sharedToken))) return false;
  } else if (id) {
    resetScene();
    await loadWorkspace(id);
    // Superseded by a later navigation: that one renders its own workspace.
    if (workspaceId !== id || landingVisible()) return false;
  } else {
    showLanding();
    return false;
  }
  return Boolean(editor.value.trim());
}

// The brand in the workspace header goes back to the landing page. A real link,
// so a modifier-click still opens "/" in a new tab; a plain click stays in-page
// and leaves the workspace one Back press away.
$('#brandHome').addEventListener('click', async (event) => {
  if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
  event.preventDefault();
  if (landingVisible()) return;
  await leaveCurrentWorkspace();
  history.pushState(null, '', '/');
  await openWorkspaceRoute();
});

$('#landingNew').addEventListener('click', async (event) => {
  const button = event.currentTarget;
  button.disabled = true;
  try {
    await startNewWorkspace('');
  } finally {
    button.disabled = false;
  }
});

$('#landingExamples').addEventListener('click', (event) => {
  const card = event.target.closest('[data-example]');
  if (card) loadExample(card.dataset.example);
});

$('#landingRecents').addEventListener('click', async (event) => {
  if (event.target.closest('[data-recents-toggle]')) {
    recentsExpanded = !recentsExpanded;
    renderLandingRecents();
    // The rebuild replaced the button that had focus; put it back on its successor.
    $('[data-recents-toggle]', $('#landingRecents'))?.focus();
    return;
  }
  const remove = event.target.closest('[data-forget]');
  if (remove) {
    forgetRecentWorkspace(remove.dataset.forget, remove);
    return;
  }
  if (event.target.closest('[data-landing-draft]')) {
    const draft = readLegacyDraft();
    if (!draft) return;
    await startNewWorkspace(draft);
    if (workspaceOnline) {
      try { localStorage.removeItem(LEGACY_DRAFT_KEY); } catch { /* kept for another try */ }
    }
    render('preview');
    return;
  }
  const open = event.target.closest('[data-landing-workspace]');
  if (!open) return;
  const id = open.dataset.landingWorkspace;
  history.pushState({ workspaceId: id }, '', `/designs/${encodeURIComponent(id)}`);
  if (await openWorkspaceRoute()) render('preview');
});

$('#landingRecoverMissing').addEventListener('click', async () => {
  const code = landingMissing?.code;
  if (!code) return;
  await startNewWorkspace(code);
  render('preview');
});

// The hero's demo video. Until web/media/ holds the ad, or when it fails to
// load, the frame keeps its placeholder and drops the play badge. It starts
// muted and bare; the first click plays it with sound and native controls.
function initLandingVideo() {
  const frame = $('#landingVideoFrame');
  const video = $('#landingVideo');
  const sources = [...video.querySelectorAll('source')];
  const missing = () => { frame.dataset.state = 'missing'; };
  // A <source> that fails fires its own error; the last one failing means none loaded.
  sources.at(-1)?.addEventListener('error', missing);
  video.addEventListener('error', missing);
  // Never demotes 'playing': a tap can start playback before metadata arrives
  // (iOS skips preloading in Low Power Mode).
  video.addEventListener('loadedmetadata', () => {
    if (frame.dataset.state === 'loading' || frame.dataset.state === 'missing') frame.dataset.state = 'ready';
  });
  video.addEventListener('playing', () => { frame.dataset.state = 'playing'; });
  if (video.readyState >= 1) frame.dataset.state = 'ready';
  // Not networkState: WebKit reports NETWORK_NO_SOURCE while it is still
  // choosing a <source>, which marked the video missing and skipped autoplay.
  else if (!sources.length) missing();
  // Works from 'loading' too: on iOS without preload, play() inside the tap is
  // what starts the download at all.
  const play = () => {
    if (frame.dataset.state === 'missing' || frame.dataset.state === 'ended') return;
    frame.dataset.state = 'playing';
    video.muted = false;
    video.controls = true;
    video.play().catch(() => {
      if (frame.dataset.state !== 'playing' || !video.paused) return;
      video.controls = false;
      frame.dataset.state = video.readyState >= 1 ? 'ready' : 'loading';
    });
  };
  $('#landingVideoPlay').addEventListener('click', play);
  // Plays once; the last frame dims behind a centred "Play again".
  video.addEventListener('ended', () => {
    video.controls = false;
    frame.dataset.state = 'ended';
  });
  $('#landingVideoReplay').addEventListener('click', () => {
    video.currentTime = 0;
    if (!video.muted) video.controls = true;
    frame.dataset.state = 'playing';
    video.play().catch(() => {});
  });
  video.addEventListener('click', () => { if (!video.controls) play(); });
  // Plays muted and looping while the landing page is on screen, and pauses
  // once a design hides it. Reduced-motion users keep the poster and the button.
  if (matchMedia('(prefers-reduced-motion: reduce)').matches || !('IntersectionObserver' in window)) return;
  // Resumes only what it paused itself, never a video the viewer paused.
  let observerPaused = false;
  new IntersectionObserver(([entry]) => {
    if (entry.isIntersecting) {
      if (frame.dataset.state === 'ended' || !video.paused) return;
      if (video.currentTime > 0 && !observerPaused) return;
      observerPaused = false;
      video.play().then(() => {
        if (frame.dataset.state !== 'playing') frame.dataset.state = 'playing';
      }).catch(() => {});
    } else if (!video.paused) {
      observerPaused = true;
      video.pause();
    }
  }, { threshold: 0.25 }).observe(frame);
}

initLandingVideo();

// ---- Open issues, next to "Report issues" -----------------------------------
// Search, not the repo's open_issues_count, which counts pull requests too. Without a
// token GitHub allows few requests, so one attempt per 10 minutes across all tabs: the
// check is recorded before it goes out, and a failure just keeps what was known.
const ISSUE_COUNT_KEY = 'reopenscad.openIssues';
const ISSUE_COUNT_TTL_MS = 10 * 60 * 1000;
const ISSUE_COUNT_URL = 'https://api.github.com/search/issues?q=repo:Matvey-Kuk/reopenscad+type:issue+state:open&per_page=1';

// Pure: what localStorage holds -> { count, checkedAt }; count null when unknown.
function parseIssueCount(raw) {
  try {
    const stored = JSON.parse(raw);
    return { count: Number.isInteger(stored?.count) && stored.count >= 0 ? stored.count : null, checkedAt: Number(stored?.checkedAt) || 0 };
  } catch {
    return { count: null, checkedAt: 0 };
  }
}

function showIssueCount(count) {
  for (const badge of $$('[data-issue-count]')) {
    badge.hidden = count === null;
    badge.textContent = count === null ? '' : `(${count})`;
    badge.classList.toggle('open', count > 0);
    const link = badge.closest('a');
    if (count === null) link.removeAttribute('aria-label');
    else link.setAttribute('aria-label', `Report issues, ${count} open`);
  }
}

function storeIssueCount(count, checkedAt) {
  try {
    localStorage.setItem(ISSUE_COUNT_KEY, JSON.stringify({ count, checkedAt }));
  } catch {
    // Private mode or a full store: this tab just does not share it.
  }
}

async function refreshIssueCount() {
  let raw = null;
  try {
    raw = localStorage.getItem(ISSUE_COUNT_KEY);
  } catch {
    // As above.
  }
  const known = parseIssueCount(raw);
  showIssueCount(known.count);
  const now = Date.now();
  if (now - known.checkedAt < ISSUE_COUNT_TTL_MS) return;
  storeIssueCount(known.count, now);
  try {
    const response = await fetch(ISSUE_COUNT_URL, { credentials: 'omit', referrerPolicy: 'no-referrer', signal: AbortSignal.timeout(8000) });
    if (!response.ok) return;
    const count = (await response.json())?.total_count;
    if (!Number.isInteger(count) || count < 0) return;
    storeIssueCount(count, now);
    showIssueCount(count);
  } catch {
    // Offline or blocked: the last known count stays, or no counter at all.
  }
}

window.addEventListener('storage', (event) => {
  if (event.key === ISSUE_COUNT_KEY) showIssueCount(parseIssueCount(event.newValue).count);
});
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'visible') refreshIssueCount();
});

async function bootstrap() {
  restoreTolerance();
  editor.value = '';
  syncEditor(false);
  checkEngine();
  refreshIssueCount();
  // "/" shows the landing page. Nothing here creates a workspace: only the
  // "New workspace" action does.
  if (await openWorkspaceRoute()) setTimeout(() => render('preview'), 180);
}

// Back/forward moves between workspaces and the landing page instead of
// stranding them.
window.addEventListener('popstate', async () => {
  const id = workspaceIdFromLocation();
  if (!shareTokenFromLocation() && id && id === workspaceId && !landingVisible()) return;
  await leaveCurrentWorkspace();
  if (await openWorkspaceRoute()) render('preview');
});

// Every way the page can come back from being away re-reads the server, which
// is authoritative: a tab shown again, a window refocused, a page restored from
// the back/forward cache (whose timers and state are frozen mid-flight), and a
// network that returns.
document.addEventListener('visibilitychange', () => {
  if (!document.hidden) refreshWorkspaceFromServer({ force: true });
});
window.addEventListener('focus', () => refreshWorkspaceFromServer());
window.addEventListener('online', () => refreshWorkspaceFromServer({ force: true }));
window.addEventListener('pageshow', (event) => {
  if (!event.persisted) return;
  if (landingVisible()) renderLandingRecents();
  else refreshWorkspaceFromServer({ force: true });
});

bootstrap();
