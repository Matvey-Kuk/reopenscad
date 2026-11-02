-- ReOpenSCAD workspace schema.
--
-- This file is documentation, not the deployment mechanism: the server applies
-- exactly these statements itself at startup (see `MIGRATIONS` in
-- `backend/src/pgstore.rs`), idempotently, under a Postgres advisory lock so
-- that a fleet of instances cold-starting at once cannot race each other. It
-- is reproduced here so the schema can be reviewed, diffed, or applied by hand
-- against a restored backup without reading Rust.
--
-- Applying it manually is safe and makes the startup migration a no-op.

-- Which migrations have been applied. Versions are appended, never edited.
CREATE TABLE IF NOT EXISTS reopenscad_schema (
    version    INTEGER PRIMARY KEY,
    applied_at BIGINT NOT NULL          -- unix milliseconds
);

-- One row per workspace. A workspace is a single SCAD document plus the viewer
-- settings and the recent edit log; the URL containing `id` is the only access
-- control there is, so `id` carries 112 bits of entropy.
CREATE TABLE IF NOT EXISTS workspaces (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    code       TEXT NOT NULL,           -- bounded by MAX_BODY (2 MiB) in the server

    -- The optimistic-concurrency token. Every edit is
    --   UPDATE ... SET revision = revision + 1 WHERE id = $1 AND revision = $2
    -- so the compare-and-swap happens under Postgres's row lock rather than in
    -- one process's mutex. That is what makes multi-instance serving safe: a
    -- second writer holding the same base revision matches zero rows and is
    -- answered with 409 instead of silently overwriting the first.
    revision   BIGINT NOT NULL,

    created_at BIGINT NOT NULL,         -- unix milliseconds
    updated_at BIGINT NOT NULL,         -- unix milliseconds; drives TTL pruning

    -- Viewer settings. Revisionless: they are written unconditionally, so
    -- picking a build plate can never conflict with an in-flight code save.
    plate      TEXT NOT NULL DEFAULT '',
    tolerance  DOUBLE PRECISION NOT NULL DEFAULT 0.2,

    -- A small render thumbnail as a base64 `data:` URL, written by the browser
    -- that drew it and shown in the "recent workspaces" list. Empty until a
    -- render has happened; bounded by MAX_PREVIEW_BYTES (96 KiB) in the server.
    preview    TEXT NOT NULL DEFAULT '',

    -- Capability for the read-only view, or '' until the workspace is shared.
    -- Independent of `id` rather than derived from it: holding the read-only
    -- link must not get you the editable one.
    share_token TEXT NOT NULL DEFAULT '',

    -- The recent edit log the `/events` long poll replays to other tabs and to
    -- the browser after an MCP agent writes. Capped at 64 entries by the
    -- UPDATE statement itself, so the bound holds across instances. Kept in the
    -- workspace row rather than a child table because every read of it is
    -- "the tail of the log for this one workspace", which is exactly what a
    -- single row already answers.
    events     JSONB NOT NULL DEFAULT '[]'::jsonb
);

-- Pruning orders by `updated_at` (TTL sweep and the count cap), and without
-- this both are sequential scans of the whole table every ten minutes.
-- Workspace reads need no index: they are primary-key lookups.
-- Unique where it exists, so two workspaces can never answer to the same
-- read-only link. Partial, so every unshared row can keep the same '' default.
CREATE UNIQUE INDEX IF NOT EXISTS workspaces_share_token_idx
    ON workspaces (share_token) WHERE share_token <> '';

CREATE INDEX IF NOT EXISTS workspaces_updated_at_idx
    ON workspaces (updated_at DESC, id DESC);

-- Superseded revisions, for list_revisions, restore_revision and `diff`. The
-- current source stays in `workspaces.code`; each save inserts the source it
-- replaced here, in the same transaction as its compare-and-swap, and deletes
-- rows beyond the newest 200 (MAX_REVISION_HISTORY) or past 4 MiB in total
-- (MAX_REVISION_HISTORY_BYTES, newest kept). A browser autosave replacing the
-- autosave before it stores no row. Evicting a workspace cascades to its history.
CREATE TABLE IF NOT EXISTS workspace_revisions (
    workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    revision     BIGINT NOT NULL,
    code         TEXT NOT NULL,
    created_at   BIGINT NOT NULL,       -- unix milliseconds the revision was written
    PRIMARY KEY (workspace_id, revision)
);

-- Creation statistics for the landing page (`GET /api/stats`). Neither table
-- references `workspaces`, so evicting a workspace lowers neither number. Each
-- create bumps both in the same transaction as its INSERT. Seeded from the
-- rows already present so a populated deployment does not restart at zero.
CREATE TABLE IF NOT EXISTS reopenscad_counters (
    name  TEXT PRIMARY KEY,             -- 'workspaces_created'
    value BIGINT NOT NULL
);

INSERT INTO reopenscad_counters (name, value)
    SELECT 'workspaces_created', COUNT(*) FROM workspaces
    ON CONFLICT (name) DO NOTHING;

-- Workspaces created per hour (hour = unix millis / 3600000). "Last 24 hours"
-- sums the current hour and the 23 before it; the sweeper deletes buckets
-- older than 48 hours, so the table stays a few dozen rows.
CREATE TABLE IF NOT EXISTS workspace_creation_hours (
    hour    BIGINT PRIMARY KEY,
    created BIGINT NOT NULL
);

INSERT INTO workspace_creation_hours (hour, created)
    SELECT created_at / 3600000, COUNT(*) FROM workspaces
    WHERE created_at >= (EXTRACT(EPOCH FROM now()) * 1000)::BIGINT - 2 * 86400000
    GROUP BY created_at / 3600000
    ON CONFLICT (hour) DO NOTHING;

-- Workspaces created per minute (minute = unix millis / 60000), so "last 24
-- hours" is a rolling window accurate to a minute: it sums the current minute
-- and the 1439 before it. Supersedes workspace_creation_hours (migration 5),
-- which is no longer read and is emptied by the sweeper. "Last 7 days" sums
-- the 10080 most recent minutes. The sweeper deletes minute buckets older than
-- 8 days, and also raises both counters to at
-- least what the workspace rows prove (covering creates by binaries that
-- predate the counters during a rolling deploy).
CREATE TABLE IF NOT EXISTS workspace_creation_minutes (
    minute  BIGINT PRIMARY KEY,
    created BIGINT NOT NULL
);

INSERT INTO workspace_creation_minutes (minute, created)
    SELECT created_at / 60000, COUNT(*) FROM workspaces
    WHERE created_at >= (EXTRACT(EPOCH FROM now()) * 1000)::BIGINT - 2 * 86400000
    GROUP BY created_at / 60000
    ON CONFLICT (minute) DO NOTHING;

-- Stored render responses (`POST /api/designs/{id}/render`), so reopening an
-- unchanged design does not recompile it. `cache_key` is a SHA-256 over the
-- engine version, mode, source, overrides and object selection (see
-- `backend/src/rendercache.rs`). Bounded: bodies over 16 MiB are not stored,
-- each save keeps a design's 4 most recently used entries, and the sweeper
-- deletes least-recently-used rows past 512 MiB in total. Evicting a
-- workspace cascades to its renders.
CREATE TABLE IF NOT EXISTS render_cache (
    workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    cache_key    TEXT NOT NULL,
    body         BYTEA NOT NULL,        -- the response JSON, minus per-request fields
    bytes        BIGINT NOT NULL,
    created_at   BIGINT NOT NULL,       -- unix milliseconds
    last_used    BIGINT NOT NULL,       -- unix milliseconds; bumped on every hit
    PRIMARY KEY (workspace_id, cache_key)
);

CREATE INDEX IF NOT EXISTS render_cache_last_used_idx
    ON render_cache (last_used DESC, workspace_id, cache_key);

-- Comments pinned to the 3D model (`backend/src/feedback.rs`): one JSON
-- document per design holding at most 100 comments and the active
-- request_feedback question. `version` and `open_count` mirror the document
-- so polls probe without parsing it; `viewer_seen_at` is when a browser last
-- long-polled the design. Evicting a workspace cascades to its comments.
CREATE TABLE IF NOT EXISTS workspace_feedback (
    workspace_id   TEXT PRIMARY KEY REFERENCES workspaces (id) ON DELETE CASCADE,
    state          JSONB NOT NULL DEFAULT '{}'::jsonb,
    version        BIGINT NOT NULL DEFAULT 0,
    open_count     INTEGER NOT NULL DEFAULT 0,
    viewer_seen_at BIGINT NOT NULL DEFAULT 0  -- unix milliseconds
);

-- Mesh assets a design can `import("name.stl")` (`backend/src/assetstore.rs`):
-- the cleaned mesh as binary STL, its SHA-256 (part of every render-cache
-- key), and the upload report {"clean": ..., "description": ...}. One uploaded
-- STL per design, 4 GiB in total by default
-- (REOPENSCAD_MAX_ASSET_STORAGE_MIB); past the total, an upload evicts the
-- assets of the least recently used other designs. Assets belong to the design, not to a
-- revision: workspace_revisions snapshots only code. Evicting a workspace
-- cascades to its assets.
CREATE TABLE IF NOT EXISTS workspace_assets (
    workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name         TEXT NOT NULL,         -- [A-Za-z0-9._-]{1,64}, ends in .stl
    sha256       TEXT NOT NULL,
    mesh         BYTEA NOT NULL,
    report       JSONB NOT NULL DEFAULT '{}'::jsonb,
    bytes        BIGINT NOT NULL,
    triangles    BIGINT NOT NULL DEFAULT 0,
    created_at   BIGINT NOT NULL,       -- unix milliseconds
    PRIMARY KEY (workspace_id, name)
);

-- When an asset expires (unix ms): 3 hours after its upload completed.
-- Checked at read time; the sweep frees the
-- space. A row from before expiry existed counts from created_at.
ALTER TABLE workspace_assets ADD COLUMN IF NOT EXISTS expires_at BIGINT NOT NULL DEFAULT 0;
UPDATE workspace_assets SET expires_at = created_at + 10800000 WHERE expires_at = 0;
CREATE INDEX IF NOT EXISTS workspace_assets_expires ON workspace_assets (expires_at);
-- Names that expired, a few per design, so import() can say why it fails.
CREATE TABLE IF NOT EXISTS workspace_assets_expired (
    workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    expired_at   BIGINT NOT NULL,
    PRIMARY KEY (workspace_id, name)
);

-- Moves on every asset write, so `/events` polls (and browsers) notice an
-- asset change even though the revision does not.
ALTER TABLE workspaces ADD COLUMN IF NOT EXISTS assets_version BIGINT NOT NULL DEFAULT 0;

-- One-time upload URLs from the MCP tool create_asset_upload (10 minutes,
-- single use: taken with DELETE ... RETURNING). A table rather than memory so
-- any instance can accept the PUT. The sweeper deletes expired rows.
CREATE TABLE IF NOT EXISTS asset_upload_tokens (
    token        TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    budget       BIGINT NOT NULL,
    expires_at   BIGINT NOT NULL        -- unix milliseconds
);
-- Who asked for each URL, so pending URLs are capped per client (8) and per
-- design (4) as well as across the deployment.
ALTER TABLE asset_upload_tokens ADD COLUMN IF NOT EXISTS client TEXT NOT NULL DEFAULT '';

-- Chunked uploads: Cloud Run caps a request body at 32 MiB, so files over
-- one 24 MiB chunk arrive as several PUTs with Content-Range. A session per
-- upload (30 minutes idle, then the sweeper drops it) and its staged parts,
-- so any instance can append a chunk and the last one assembles the file.
-- At most 2 sessions per client and per design, 256 MiB declared per client
-- and 1 GiB across the deployment.
CREATE TABLE IF NOT EXISTS asset_upload_sessions (
    upload_id    TEXT PRIMARY KEY,       -- 32 hex digits, minted by the server
    target       TEXT NOT NULL,          -- design:<id>/<name> or token:<token>
    workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    budget       BIGINT NOT NULL,
    total        BIGINT NOT NULL,        -- declared file size, <= 128 MiB
    received     BIGINT NOT NULL DEFAULT 0,
    client       TEXT NOT NULL DEFAULT '',
    created_at   BIGINT NOT NULL,        -- unix milliseconds
    updated_at   BIGINT NOT NULL         -- unix milliseconds; drives expiry
);
CREATE TABLE IF NOT EXISTS asset_upload_chunks (
    upload_id   TEXT NOT NULL REFERENCES asset_upload_sessions (upload_id) ON DELETE CASCADE,
    byte_offset BIGINT NOT NULL,
    bytes       BYTEA NOT NULL,          -- at most 30 MiB
    created_at  BIGINT NOT NULL,
    PRIMARY KEY (upload_id, byte_offset)
);
-- A finish claim (unix ms, 0 = none): one request on any instance processes a
-- complete upload; a claim older than 3 minutes is taken for a crashed one.
ALTER TABLE asset_upload_sessions ADD COLUMN IF NOT EXISTS finishing_at BIGINT NOT NULL DEFAULT 0;
-- Sealing is gone: uploads keep open surfaces open.
ALTER TABLE asset_upload_tokens DROP COLUMN IF EXISTS seal;
ALTER TABLE asset_upload_sessions DROP COLUMN IF EXISTS seal;
-- The reply of each finished chunked upload, kept 10 minutes (at most 1024
-- rows), so a re-sent last chunk whose reply was lost is answered with it
-- instead of a 404.
CREATE TABLE IF NOT EXISTS asset_upload_receipts (
    upload_id   TEXT PRIMARY KEY,
    target      TEXT NOT NULL,
    total       BIGINT NOT NULL,
    final_start BIGINT NOT NULL,           -- where the last chunk started
    response    JSONB NOT NULL,
    created_at  BIGINT NOT NULL            -- unix milliseconds; drives expiry
);
CREATE INDEX IF NOT EXISTS asset_upload_receipts_created ON asset_upload_receipts (created_at);

-- Named revisions (migration 14). Rows from before keep '' and are labelled
-- "Revision <n>" on read. source is 'ai', 'browser' or '' (unrecorded).
ALTER TABLE workspaces ADD COLUMN IF NOT EXISTS revision_name TEXT NOT NULL DEFAULT '';
ALTER TABLE workspaces ADD COLUMN IF NOT EXISTS revision_source TEXT NOT NULL DEFAULT '';
ALTER TABLE workspace_revisions ADD COLUMN IF NOT EXISTS name TEXT NOT NULL DEFAULT '';
ALTER TABLE workspace_revisions ADD COLUMN IF NOT EXISTS source TEXT NOT NULL DEFAULT '';

INSERT INTO reopenscad_schema (version, applied_at)
SELECT version, (EXTRACT(EPOCH FROM now()) * 1000)::BIGINT
FROM (VALUES (1), (2), (3), (4), (5), (6), (7), (8), (9), (10), (11), (12), (13), (14)) AS migrations (version)
ON CONFLICT (version) DO NOTHING;

-- Retention, for reference. The server runs both statements on a ten-minute
-- sweeper thread; neither loads a row into the process.
--
--   -- 14-day idle TTL
--   DELETE FROM workspaces WHERE updated_at < $ttl_cutoff_millis;
--
--   -- 512-workspace cap, oldest first
--   DELETE FROM workspaces WHERE id IN (
--       SELECT id FROM workspaces ORDER BY updated_at DESC, id DESC OFFSET 512
--   );
