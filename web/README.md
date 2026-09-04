# ReOpenSCAD native web prototype

A browser workbench and independent Rust geometry engine. It does not invoke
the OpenSCAD executable or load OpenSCAD WebAssembly.

Run it from the workspace root:

```sh
cargo run --manifest-path web/backend/Cargo.toml --release
```

Then open <http://127.0.0.1:5173>.

The backend contains its own lexer, parser, expression evaluator, module and
loop expansion, implicit CSG scene evaluator, marching-tetrahedra mesher, and
STL/OFF/OBJ/3MF writers, including a stored-entry ZIP/OPC packer for 3MF.

## Dependencies

The geometry engine has none beyond `serde`/`serde_json`, and the default build
adds nothing else — everything above is written from scratch in this crate.

The one exception is deliberate and opt-in. The `postgres` cargo feature pulls
in the [`postgres`](https://crates.io/crates/postgres) crate to store
workspaces in a database instead of on local disk, which is what a
multi-instance deployment needs. It is the *synchronous* driver, chosen because
this server is blocking and thread-per-connection; `sqlx` or `tokio-postgres`
would drag an async runtime into a codebase that has none. A further
`postgres-tls` feature adds `native-tls` for a Postgres reached over plain TCP.
Neither is in the default build, so `cargo build` and `cargo test` compile
exactly what they did before.

The browser shell covers source editing, syntax highlighting, preview/final
rendering, view presets, orbit/zoom, variable controls, SCAD open/save, and
STL/OFF/OBJ/3MF/PNG export. Language coverage is tracked by the tests in
`backend/src/engine/spec_*.rs`; the parity gaps that remain against the
OpenSCAD goldens are the `#[ignore]`d tests described in
`backend/tests/README.md`.

Run the backend and frontend contract tests with:

```sh
cargo test --manifest-path web/backend/Cargo.toml
npm --prefix web test
```

## Running it

There are three ways to run this, and they differ only in where workspaces are
stored. `DATABASE_URL` is the switch: set, workspaces go to Postgres; unset,
they go to local JSON files.

### Local development — filesystem

The default — the `cargo run` above. One JSON document per workspace under
`web/.reopenscad-workspaces/` (override with `REOPENSCAD_DATA_DIR`), written
atomically, with an in-process LRU so boot cost does not scale with the number
of workspaces. It assumes it is the only writer, which is true for one process
on one machine and false the moment the service is replicated.

### Local Docker — Postgres, two instances

`web/docker-compose.yml` runs Postgres plus **two** application containers
against it. Two rather than one on purpose: one container proves that
workspaces survive a restart, two prove the part that actually breaks when the
filesystem store is deployed behind a load balancer — that a concurrent edit
still resolves to one winner and one 409 when the writers are in different
processes.

```sh
docker compose -f web/docker-compose.yml up --build
```

Instance A is on <http://127.0.0.1:8080>, instance B on
<http://127.0.0.1:8081>, both showing the same workspaces.

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

Both enforce the same limits (14-day idle TTL, 512 workspaces, 64 edit events
per workspace) and present the same API; the schema is documented in
`backend/sql/schema.sql` and applied automatically at startup. The 512-workspace
cap bounds one developer's directory on disk, but is global and evicts other
users' work in a shared database — raise `REOPENSCAD_MAX_WORKSPACES` before
taking real traffic (DEPLOY.md §6).

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

This project's own code is MIT-licensed; see `LICENSE` at the repository root.
Because none of it derives from OpenSCAD, GPL copyleft does not reach it.
