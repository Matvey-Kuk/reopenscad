# ReOpenSCAD native web prototype

A browser workbench and independent Rust geometry engine. It does not invoke
the OpenSCAD executable or load OpenSCAD WebAssembly.

Run it from the repository root:

```sh
cargo run --manifest-path web/backend/Cargo.toml --release
```

Then open <http://127.0.0.1:5180>.

The backend contains its own lexer, parser, expression evaluator, module and
loop expansion, exact polyhedral CSG kernel (`backend/src/csg/`), and
STL/OFF/OBJ/3MF/STEP/GLB writers, including a stored-entry ZIP/OPC packer for
3MF. A multi-object export keeps every top-level object as its own named body,
with its `color()`, in one file, and a single-object export keeps that object's
name (`backend/src/export.rs`). `color()` applies at any depth — inside a
module, an `if` or a `for`, under transforms — and an object painted in several
colours stays one object: the exact kernel carries each face's colour through
the booleans (an inner `color()` wins; a difference's cut walls take the tool's
colour, or the base's when the tool has none), and the viewer, the PNG renderer,
3MF (per-triangle materials), GLB (a primitive per colour), OBJ and OFF draw it
per face. STEP keeps one colour per body, the one covering the most surface. `text()` sets glyph
outlines from fonts bundled in the binary — Liberation Sans, Serif and Mono,
Dancing Script and Great Vibes, all OFL, in `backend/src/engine/fonts/` — read
by its own TrueType parser, so output does not depend on what the host has
installed. The older single-stroke face is still there as
`font = "ReOpenSCAD Stroke"` (`backend/src/engine/font.rs`).

## Dependencies

The geometry engine has none beyond `serde`/`serde_json`, and the default build
adds nothing else — everything above is written from scratch in this crate.

The one exception is deliberate and opt-in. The `postgres` cargo feature pulls
in the [`postgres`](https://crates.io/crates/postgres) crate to store
designs in a database instead of on local disk, which is what a
multi-instance deployment needs. It is the *synchronous* driver, chosen because
this server is blocking and thread-per-connection; `sqlx` or `tokio-postgres`
would drag an async runtime into a codebase that has none. A further
`postgres-tls` feature adds `native-tls` for a Postgres reached over plain TCP.
Neither is in the default build, so `cargo build` and `cargo test` compile
exactly what they did before.

The browser shell covers a landing page (new design, the MCP endpoint, a
public design counter from `GET /api/stats`, this browser's design
history and the example catalogue), source editing, syntax highlighting,
automatic re-rendering with a progress bar (F5 preview, F6 full render), view
presets, orbit/zoom/pan, a section view, Compare parts (a heatmap between two
parts), resizable panels, an open-issue counter in the header, SCAD open/save,
and STL/3MF/STEP/OBJ/OFF/GLB/PNG export. Language coverage is tracked by the tests in
`backend/src/engine/spec_*.rs`; the parity gaps that remain against the
OpenSCAD goldens are the `#[ignore]`d tests described in
`backend/tests/README.md`.

Designs live at `/designs/<id>` and their API under `/api/designs/...`. They
were called workspaces before; `/workspaces/<id>` redirects (308) to the new
route, `/api/workspaces/...` stays an alias of every `/api/designs/...` route
with identical behaviour and rate limiting, and responses carry `designId`
next to the old `workspaceId`. Storage was not renamed: the data directory,
the `workspaces` table and `REOPENSCAD_MAX_WORKSPACES` keep their names.

### Mesh assets (scans)

A design can hold one uploaded STL, that its code uses with
`import("name.stl")`. An upload is temporary: it is kept for 3 hours from when
it completes, with no renewal, for reverse-engineering it into a real part
(`get_reverse_engineering_guide`). A second upload, under any name, is refused
with 409 naming the file to remove; removing it and uploading again starts a
new clock. Uploads are parsed, cleaned (welded, specks
removed, winding fixed, decimated to a triangle budget, default 500k, max 1M)
and described. The mesh analysis tools read that stored mesh; detail lost to
decimation cannot be recovered. An open surface such as a one-sided scan stays open: nothing
fills it, so booleans and volume checks, which need a closed solid,
say so. A raw phone scan can be sent at full
resolution: the upload limit is
128 MiB (about 2.68M triangles of binary STL; at most 3M parsed). Names are
`[A-Za-z0-9._-]{1,64}`, end in `.stl`, never start with `.` or contain `..`.
Assets belong to the design, not to a revision: revisions snapshot only code.

Expiry is decided when metadata is read, so it is exact whatever the sweep is
doing: an expired asset is gone from every list, download and compile at once.
A background sweep (every 5 minutes on every instance, safe to run from several)
then deletes the mesh and the metadata, and
moves `assetsVersion`. Assets stored before uploads expired count from
`createdAt`. The last few expired names of a design are remembered, so
`import()` of one fails the compile with an error saying it expired and that the
import must be removed or the file uploaded again. A write that keeps such an
import is rejected the same way; a write that removes it succeeds even though
the current revision does not compile. The asset digest in every cache key names
expired files too, so no cached render outlives the expiry.

| Route | |
|---|---|
| `PUT /api/designs/{id}/assets/{name}[?budget=N]` | Raw STL body (binary or ASCII, up to 128 MiB; any content type). 200 `{ok, designId, name, sha256, bytes, triangles, expiresAt, expiresAtIso, expiresInSeconds, expiresIn, expiry, guide, report, description, assetsVersion, import}`; 400 unreadable file or bad name; 404 no design; 408 body slower than 64 KiB/s after 10 s or not done in 300 s; 409 the design already holds an STL; 413 body too large (the message states the limit and that uploads are decimated); 429 rate limit, over the hourly storage quota or over the hourly raw-upload allowance; 503 storage full or too many uploads in flight (4 per instance, 2 per client; of those, bodies over 25 MiB: 2 per instance, 1 per client, cleaned one at a time). Host, rate limit, name and design are checked on the request head, before the body is read or an upload slot is taken. |
| `PUT …/assets/{name}` with `Content-Range` | Chunked upload, for files over 24 MiB (hosts such as Cloud Run refuse bodies over 32 MiB). Each chunk is a PUT to the same route (or to `/api/uploads/{token}`) with `Content-Range: bytes first-last/total` (inclusive, total ≤ 128 MiB) and a body of at most 30 MiB (24 MiB recommended; every chunk but the last at least 1 MiB). The first chunk (start 0, no `Upload-Id`) opens a session; every later one sends its `uploadId` back as the `Upload-Id` header (or `?uploadId=`). Chunks go in order: 202 `{ok, uploadId, received, total, next}` until the last, which answers like a single PUT (200 with the report). 409 `{uploadId, received, next, total, error}` for a wrong offset or a changed total — resume from `next`; re-sending the chunk just stored is a no-op. 404 for an unknown or expired session (30 minutes without a chunk), 413 for a chunk over 30 MiB or a total over 128 MiB (on the request head, before the body). A session belongs to its target (design and name, or one-time URL); the `?budget` of the first chunk applies. Limits: 2 sessions per client and per design, 256 MiB declared per client, 1 GiB across the server (429/503 past them); each chunk is billed to the rate limits and the hourly raw-upload allowance like a PUT of its size. The same client restarting a target from byte 0 replaces its old session. A last chunk refused for its content (400, 409 over the asset limits) ends the session; one refused because the server was busy (429/5xx) keeps it, so re-send the last chunk. |
| `GET /api/designs/{id}/assets` | `{ok, designId, serverTime, assets: [{name, sha256, bytes, triangles, createdAt, expiresAt, expiresIn, report, description}], assetsVersion, limits, expired?: [{name, expiredAt}]}`. Only live assets; `expiresAt` and `serverTime` are unix ms on the server's clock. |
| `GET /api/designs/{id}/assets/{name}` | The cleaned mesh, `model/stl` (binary). |
| `DELETE /api/designs/{id}/assets/{name}` | `{ok, designId, deleted, assetsVersion, note?}`. 404 says so if it already expired. |
| `GET /api/shared/{token}/assets[/{name}]` | Read-only links may list and download (no design id in replies), never write. |
| `POST /api/shared/{token}/clone` | `{code?}` (default: the stored source). A new, editable design with the plate, tolerance and server-side copies of the assets (billed to the caller's quota). 201 like `POST /api/designs`; 404 bad link; 429 over quota. |
| `PUT /api/uploads/{token}` | One-time URL from the MCP tool `create_asset_upload` (10 minutes); same reply as the PUT above without `designId` (the URL grants one upload, not the design). Takes chunks like the design route: the first chunk binds the URL to its session (another client's second session is refused with 409), every chunk extends the URL to 30 minutes, and only the stored file uses it up: a rejected or failed upload leaves it valid. At most 4 pending per design and 8 per client. |

Every compile of a design — `/api/designs/{id}/render|export|evaluate|...`,
the MCP analysis tools, and the stateless `POST /api/render`,
`/api/export` and `/api/intersections` when the body carries `designId` (or `workspaceId`), or a read-only link's
`shareToken` — runs with that design's assets. Render-cache and
object-summary keys include the asset digest.

Assets load lazily: installing a design's assets reads nothing; the first
`import()` (or cache-key digest) inside a compile, which already holds its
render permit, reads the name/hash index, and each mesh is fetched, verified
against its hash and decoded when first imported. Decoded meshes are shared by
hash, and concurrent misses decode once. An asset that exists but cannot be
read, or was replaced while the compile ran, fails the compile instead of
silently dropping out.

Storage quotas: each client (IP) may add 512 MiB of stored assets per hour
(uploads and copies, counted after cleaning, so a 120 MiB scan stored as a
3 MB mesh costs 3 MB; `REOPENSCAD_ASSET_QUOTA_MIB_PER_HOUR`). Raw upload
bodies have their own allowance, 2 GiB per client per hour
(`REOPENSCAD_UPLOAD_RAW_MIB_PER_HOUR`), on top of the per-second byte bucket. When the
deployment total (`REOPENSCAD_MAX_ASSET_STORAGE_MIB`, 4 GiB) is reached, an
upload evicts the assets of the least recently used other designs (their code
stays; `assetsVersion` moves) rather than refusing everyone.

`GET /api/designs/{id}/events` always reports `assetsVersion`; pass
`assets=<last seen version>` and the poll also ends when an asset is uploaded,
deleted or swept after expiring (within 5 minutes of its `expiresAt`), answering
`assetsChanged: true`. The revision does not
move, so re-render the same revision.

MCP tools: `upload_asset` (inline base64, up to 1.4 MB), `create_asset_upload`
(one-time PUT URL plus a curl command for files up to 24 MiB; with
`includeScript: true` or `bytes` over 24 MiB also `chunkedScript`, a POSIX sh
loop of `dd` and `curl --data-binary @-` that sends larger files in 24 MiB
chunks; the server never fetches URLs), `list_assets` (name, short sha256,
size and bounds per asset; `detail: true` adds the cleanup report and geometry
description), `delete_asset`. Chunked uploads stage their parts in Postgres
(`asset_upload_sessions` / `asset_upload_chunks`) or, with the filesystem
store, in files under `<data dir>/.uploads/` (named only by server-minted
ids, created with O_EXCL, never through a symlink), never in memory between
requests; idle sessions are swept with their parts. A re-sent last chunk of a
finished upload (its reply was lost) is answered with the stored reply for 10
minutes. `chunkedScript` follows the server's `next`, retries a chunk after a
network error, 429 or 5xx, and if it stops prints the `RO_OFFSET` /
`RO_UPLOAD_ID` that resume it.

### Splitting (MCP)

`split_object {designId, objectId | from, plane, gap, keys, prefix,
spacing}` cuts one object (or asset) by a plane (`{axis, offset}` or
`{point, normal}`) into two printable halves and writes nothing: it returns
SCAD to paste, compiled and checked (each new object closed with positive
volume) before replying. Keys are placed on the real cut
(`meshanalysis` section): grid points of the section at least the socket
radius plus `minWall` (default 1.2 mm) from every cut edge whose sockets also
keep `minWall` of material beyond their depth on both sides (hollow spots are
skipped), spread by farthest-point sampling. Diameter defaults from the
largest inscribed circle (2–6 mm), length from the material depth (3d down to
1.5d), clearance (per side) from the design tolerance (capped at 2 mm).
Each key reaches at least max(1 mm, d/2) into each half past the glue gap
(a dowel's `length` spans the gap; a pin rises gap + length/2), and the whole
socket is checked at ≤ 1 mm steps to keep the full `minWall` on its sides
and floor. `style: "dowel"`
(default) cuts sockets into both halves and adds loose dowels, so both halves
print flat on their cut face; `"pin"` fuses pins to half B. `gap` (default
0.1 mm) is the glue gap between the cut faces. The reply has `scad` (variables
and modules `<prefix>_half(side)`, `<prefix>_print_half(side)`,
`<prefix>_dowels()`, `<prefix>_dowels_in_place()`), `usage.print` and
`usage.assembled` (named statements wrapping the object), `apply` (an
`update_design_code` edit that swaps the object statement, with its `@name`
line, for both), `keys`, `cut` and `validated`.

```json
{"name": "split_object", "arguments": {"designId": "…", "objectId": "object-body",
  "plane": {"axis": "z", "offset": 12}, "keys": {"count": 2}}}
```

### Scan-to-CAD comparison (MCP)

`get_reverse_engineering_guide` is the one home of the workflow: how to work
through a scan feature by feature, which of these tools to use, and to finish
by removing the import and the STL. Every render or
export of a design importing a live STL (`evaluate`, `render_design`,
`render_image`, `get_print_layout`, `export_print_parts`) carries `liveStl`
`{files, expiresAt, expiresIn, guide, text}` and a text line with the time left.

These tools check a reconstruction against the scan itself. A closed, manifold
model with the thickness the agent chose proves nothing about agreement. None
of them use solid booleans, so one-sided scans and crops of them compare as
they are. Every surface argument takes an object ID, a list of IDs,
`{objectIds}`, or `{asset, frameOf | matrix}`. `frameOf` names the object that
imports the scan. The pose is recovered exactly: by triangle correspondence,
or else by compiling the source with the import swapped for a marker polytope.

`within: {min: [x, y, z], max: [x, y, z]}` keeps `compare_surfaces`,
`compare_sections`, `measure_between`, `fit_curve`, `fit_surface`, `scan_report`,
`extract_features`, `section_profile` and `describe_geometry` to one box, in the
same coordinates as their other arguments (design coordinates once a surface is
posed). The reply's `within` states how much was inside; a box with nothing inside
answers `nothingInside` (for a fit, an error), never a pass. A box takes in any
neighbouring surface that passes through it.

- `compare_surfaces {reference, candidate, within, tolerance, maxDistance, maxNormalAngleDeg, samples, maxRegions, minRegionArea, detail}`
  samples both surfaces by area and measures each sample to the exact nearest
  point of the other, in both directions. It reports area-weighted
  rms/p95/max, signed mean, coverage and a histogram, plus three kinds of
  unscored area:
  - **Unsupported:** model surface whose nearest scan point is on the scan's
    open edge or beyond `maxDistance`, such as an inferred underside. It is
    kept out of the statistics.
  - **Uncovered:** scan the model does not reach.
  - **Facing mismatches:** normals that disagree, i.e. the model follows the
    wrong side of a wall. These are never counted as agreement, and normals
    are never flipped automatically.

  Regions are split where the surface creases, so each names one wall, face or
  rim. By default a list holds at most `maxRegions` (3) entries `{side, area,
  signed, at, bounds}` of at least `minRegionArea` (0.5% of the compared area,
  inside `within` when given); `regionsDropped` counts the rest with their area,
  a location is listed once (`alsoIn` names its other lists), and
  `referenceToCandidate` leaves out what repeats `shapeAccuracy`. `detail: true`
  adds `center`, `normal`, `surface`, `nearest`, `maxDeviation`, `matchedOnly`,
  `verdict.reasons` and the repeats. The verdict is `{matches, reasonCount}`. The
  result is sent once, as `structuredContent`; `content` holds a one-line summary
  and the signed heatmaps of both surfaces.
- `compare_sections {reference, candidate, planes | sweep}` cuts both
  surfaces with the same planes. Open scan contours stay open, and deviation
  is measured both ways per plane. The reply is one overlay sheet where every
  tile has the same px/mm, with a mm grid, dimensions, deviation markers and
  numbered worst points.
- `measure_between {from, to, direction, expected}` casts rays from one
  surface to another. Each hit is classed as material thickness, or as a gap
  (recess depth or air gap). The evidence is labelled:
  - `observed`: both surfaces are scan data.
  - `inferred`: a model surface is involved.
  - `insufficient`: too few rays reach `to`, as when there is no scanned
    underside.

  It returns the distributions, patches, example rays with every crossing,
  and a picture. `probe {allHits: true}` returns every crossing of a ray,
  with the material and gap spans between them.
- `fit_curve {section | points, maxControlPoints, tolerance, closed, rejectOutliers, thickness}`
  fits a B-spline within a control-point budget and returns:
  - editable SCAD (`<prefix>_ctrl` plus pure-SCAD evaluation), compiled
    before replying;
  - rejected samples;
  - unsupported spans, where the curve bridges gaps in the data;
  - an independent comparison with every sample and with the cut surface.
- `extract_features {from, objectId, within, transitionAngleDeg, creaseAngleDeg, rings, boundaryRings, minArea}`
  describes a scan's opening rims (crease or fillet bands ringing an opening in
  the main surface), wall transitions (every other band), open boundary (one band
  per loop) and the smooth surfaces between them. It stores nothing; each feature
  comes with its bounds and a `within` box for the other tools.
- `fit_surface {surface, projection, patches, continuity, maxControlPoints, tolerance, smoothness}`
  fits clamped bicubic B-spline patches over the surface's best-fit plane or a
  fitted cylinder (closing up across its seam when it goes all the way
  round). Neighbouring patches share boundary controls (C0) and, by default,
  their cross-boundary slope (G1), as single unknowns, so continuity is exact;
  a second-difference penalty keeps curvature and data-free controls in
  check. The reply gives residuals over every sample, an independent
  nearest-distance comparison of the tessellated fit with the scan both ways,
  the largest gap and tangent angle across shared boundaries, unsupported
  (data-free) patch area, and pure SCAD with a `<prefix>_shell(thickness)` slab
  (compiled and checked before replying). It stores nothing: paste the SCAD into
  the design as a revision, and `compare_surfaces` its object against the scan
  to recheck.

Validation covers the whole feature. `compare_surfaces` reports
`shapeAccuracy` (every reference sample at its real distance, unmatched and
past-the-edge ones included) apart from `matchedOnly` (where the model already
agrees), plus `missingRegions` (where, how big) and `missingFraction`.
`compare_sections` adds `shapeAccuracy` per plane and overall, and `scan_report`
carries both.
`scan_report {scan, scanObject, candidate, within, features: [{label, within}], sections, thickness}`
runs these into one verdict, one surface comparison per feature box (default the
whole scan); a box with nothing inside fails it.

Scan tool replies leave out the console, histograms, cameras, knot vectors,
pose matrices and long attempt logs unless the call passes `detail: true`.

### Revisions

Whatever an agent works on is what the person sees in the viewport. Every change is a saved, named revision, the browser renders it as soon as it lands, and no tool analyses geometry that is not the current revision.

**Names.** Every MCP write (`create_design`, `update_design_code`, `set_parameters`, `set_text`, `set_print_orientation`, `restore_revision`, `evaluate` with `code` or `edits`, and the deprecated `*_workspace*` aliases) requires `revisionName`: non-empty after trimming, at most 40 characters. A missing or invalid name fails the call and nothing is saved. Browser saves are named "Edit in browser"; a browser save replaces the one before it when that one is under a minute old and from the same 5-minute period of the clock, so a burst of typing keeps one entry per 5 minutes. Its revision number still moves, so a coalesced number has no stored copy. Revisions stored before names existed load as "Revision <n>".

**Retention.** The last 200 revisions, within 4 MiB of history per design (oldest go first; the newest is always kept). A 20 KB source keeps all 200, a 70 KB one about 58. The 4 MiB ceiling is what bounds the store, unchanged from when the count was 20.

**Tools.**

- `list_revisions {designId, limit?, before?}`: newest first, `{revision, name, source: agent|browser|unknown, timestamp, current?}`.
- `restore_revision {designId, revision, revisionName, expectedRevision}`: copies that revision's source into a new revision on top.
- `get_design_code {designId, revision?, ...}`: reads an old revision's source as text.
- `diff {designId, fromRevision?, toRevision?}`: compares two stored revisions. `toRevision` defaults to the current revision, `fromRevision` to the newest retained one before it.
- `evaluate {designId, code | edits, revisionName, expectedRevision, ...}`: saves a new revision, then evaluates it. Without `code` or `edits` it evaluates the current revision. Source that fails to compile answers `valid: false` and saves nothing.

Every other analysis, render and export tool takes `designId` and reads the current revision. `code`, `overrides`, `variants`, an override `sweep` and `revision` are refused with an error saying what to do instead, never ignored. (`sweep` on `compare_sections` and `scan_report` stays: it lays out section planes on the current revision.) `fit_curve` on given `points` and `align_to` are pure math and need no design.

**HTTP.** `GET /api/designs/{id}/revisions` and `GET /api/shared/{token}/revisions` list revisions; `POST /api/designs/{id}/revisions {revision, revisionName, expectedRevision}` restores one. The per-design analysis routes refuse the same arguments. The routes for a person's own unsaved text stay: `POST /api/render`, `/api/export` and `/api/intersections` (the last also per design) take `code`. The other stateless analysis routes are gone.

**Panel.** The left pane lists revisions newest first with name, who and when, and marks the current one. An older one has a Restore action, which saves "Restore: <old name>". A read-only link lists them but cannot restore.

### Watching the agent work

Every MCP tool call that names a design shows up in the browser that has the design open. The server keeps a short log in memory per design (`backend/src/activity.rs`). The page reads it through the `/events` long poll (`activity=<version>`) and fetches pictures and meshes from `/api/designs/<id>/activity/<seq>/<file>`.

- **Console:** each call is a violet row, shown as "running" first and then with its result. The row also holds the pictures the agent was shown and the links it made.
- **Editor:** the lines the agent reads (`get_design_code`, `list_modules`, `get_parameters`) are tinted.
- **Model:** the findings are drawn on the model:
  - boxes and dimensions;
  - rays and hit points;
  - section planes and loops;
  - fitted axes and placements;
  - scan features and deviation heatmaps.
- **Inspection panel:** an agent's intersection check fills it.
- **Revisions:** every revision an agent saves appears in the left panel and is rendered at once. An agent cannot look at anything else: see "Revisions" above.

Each effect holds for 10 s and then fades out. Calls made before the page loaded are listed but not replayed. Tool replies are never changed. The log is lost on restart, and with several server instances only the instance that served the call has it.

Run the backend and frontend contract tests with:

```sh
cargo test --manifest-path web/backend/Cargo.toml
npm --prefix web test
```

## Running it

There are three ways to run this, and they differ only in where designs are
stored. `DATABASE_URL` is the switch: set, designs go to Postgres; unset,
they go to local JSON files.

### Local development — filesystem

The default — the `cargo run` above. One JSON document per design under
`web/.reopenscad-workspaces/` (override with `REOPENSCAD_DATA_DIR`), written
atomically, with an in-process LRU so boot cost does not scale with the number
of designs. It assumes it is the only writer, which is true for one process
on one machine and false the moment the service is replicated.

### Per-client rate limits

Every client address is metered by token buckets (requests, compute for
evaluating routes, response bytes). The defaults are the production limits.
A local verification loop that keeps hitting 429 can raise them, e.g.

```sh
REOPENSCAD_IP_HEAVY_BURST=600 REOPENSCAD_IP_HEAVY_REFILL_PER_SEC=60 \
  cargo run --release --manifest-path web/backend/Cargo.toml
```

All six variables, their bounds and the 429 response contract are in
[DEPLOY.md](DEPLOY.md#per-client-limits).

### Local Docker — Postgres, two instances

`web/docker-compose.yml` runs Postgres plus **two** application containers
against it. Two rather than one on purpose: one container proves that
designs survive a restart, two prove the part that actually breaks when the
filesystem store is deployed behind a load balancer — that a concurrent edit
still resolves to one winner and one 409 when the writers are in different
processes.

```sh
docker compose -f web/docker-compose.yml up --build
```

Instance A is on <http://127.0.0.1:8080>, instance B on
<http://127.0.0.1:8081>, both showing the same designs.

### Cloud Run — Postgres on Cloud SQL

See [DEPLOY.md](DEPLOY.md) for the full sequence, the sizing rationale, and two
things that will bite otherwise: `REOPENSCAD_PUBLIC_ORIGIN` must be set to the
service URL or every request is answered 403, and long polling keeps a Cloud
Run instance billable for as long as any browser tab is open.

### Storage backends

| | Filesystem | Postgres |
| --- | --- | --- |
| Selected by | `DATABASE_URL` unset (default) | `DATABASE_URL` set |
| Built by | every build | `--features postgres` |
| Concurrency check | in-process `Mutex` | `WHERE id = $1 AND revision = $2` |
| Safe with >1 instance | **no** | yes |
| Retention | prune on write | sweeper thread, every 10 min |

Both default to the same limits (512 designs, 64 edit events per design)
and present the same API. Nothing expires on a clock: a design URL is the
only handle anyone has on their work, so age alone never takes one away — a
design goes only when the store is full, least recently touched first. The
Postgres schema is documented in `backend/sql/schema.sql` and applied
automatically at startup. The 512-design cap bounds one developer's
directory on disk and is fixed for the filesystem store, but in a shared
database it is global and evicts other users' work — the Postgres store reads
`REOPENSCAD_MAX_WORKSPACES` (values below 512 are ignored); raise it before
taking real traffic (DEPLOY.md §7).

A binary built without the `postgres` feature refuses to start when
`DATABASE_URL` is set rather than quietly falling back to local files, because
that fallback would look like a working deployment right up until it lost
somebody's work.

## Independence and attribution

ReOpenSCAD is an independent reimplementation and is not affiliated with,
sponsored by, or endorsed by the OpenSCAD project. It implements the OpenSCAD
language, but contains no OpenSCAD source code — the lexer, parser, evaluator,
mesher, and exporters in this repository were written from scratch.

OpenSCAD itself is a separate project, licensed GPL-2.0-or-later and copyright
its authors. It lives at <https://openscad.org> and
<https://github.com/openscad/openscad>.

This project's own code is licensed under the **GNU General Public License,
version 3 or later**; see `LICENSE` at the repository root. The bundled fonts
keep the SIL Open Font License (`backend/src/engine/fonts/`). None of this code
derives from OpenSCAD, so OpenSCAD's own license does not reach it.
