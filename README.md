# ReOpenSCAD

OpenSCAD in the browser, rebuilt from scratch — a modern web workbench on top
of an independent Rust geometry engine. No OpenSCAD binary, no OpenSCAD
WebAssembly, no OpenSCAD source code: the lexer, parser, evaluator, CSG kernel
and exporters in this repository were all written here.

![The ReOpenSCAD workbench: objects panel, code editor, and a 3D view of a louvred coffee bin sitting on a Bambu Lab X1 Carbon build plate](docs/screenshot.png)

## What it does

Everything the desktop application does with a `.scad` file, minus the parts a
web app has no business keeping: no menu bar, no tabs, no file name. The
workspace *is* the document.

It also does a few things the original does not:

* **Objects panel.** Every top-level solid listed on the left, each one able to
  be hidden on its own.
* **Build plates.** Put the model on the bed it will actually be printed on —
  current Bambu Lab, Creality, Prusa, Anycubic and Elegoo machines — and be
  told whether it fits. The choice follows you into new workspaces.
* **Intersection and clearance checking.** Find where parts collide, or where
  they are closer than a fabrication tolerance (0.2 mm by default), and see
  exactly where.
* **An MCP server.** Eight tools that let an AI agent create a workspace, query
  and patch the SCAD source, render, export, check clearances, and hand back a
  link. The browser picks up an agent's edits live and highlights what changed.

Export is STL, OFF, OBJ, 3MF or PNG.

## Running it

```sh
cargo run --manifest-path web/backend/Cargo.toml --release
```

Then open <http://127.0.0.1:5173>. That is the whole setup — the default build
has no dependencies beyond `serde`, and workspaces go to local JSON files.

For Docker, Postgres-backed storage and Cloud Run, see
[`web/README.md`](web/README.md) and [`web/DEPLOY.md`](web/DEPLOY.md).

## The geometry engine

An exact polyhedral CSG kernel: solids are polygon soups with exact planes,
booleans are computed by BSP classification rather than by sampling a distance
field, and every output edge is the exact intersection of two input planes.
Sharp edges survive exactly and `polyhedron()` is a trivial constructor.

It is held to OpenSCAD's own answers. Two corpora of `.scad` sources are
checked in alongside binary STL goldens that the OpenSCAD binary rendered from
them, and the tests compare what survives a different triangulation — volume,
surface area, watertightness, component count:

| | What it is | Where |
| --- | --- | --- |
| OpenAPPA | Smooth organic geometry the kernel already reproduced | `web/backend/tests/fixtures/openappa/` |
| Hard geometry | Near-tangent unions and sliver faces that used to break it | `web/backend/tests/fixtures/hard-geometry/` |

Both come out closed, with volume and surface area matching to eight or nine
significant figures. The hard-geometry fixtures each record the defect they
caught; their [README](web/backend/tests/fixtures/hard-geometry/README.md) is
the best account of how the kernel actually behaves.

```sh
cargo test --manifest-path web/backend/Cargo.toml   # engine and server
npm --prefix web test                               # frontend contracts
```

Language coverage lives in [`SPEC_LANG.md`](SPEC_LANG.md) and the
`web/backend/src/engine/spec_*.rs` tests. The remaining parity gaps are
described in [`web/backend/tests/README.md`](web/backend/tests/README.md).

## Workspaces

A workspace is one SCAD file with a funny ID in its URL, and that URL is the
only credential: anyone holding the link has full read and write. There are no
accounts.

They are **scratch space, not storage.** A workspace is deleted after a period
without edits, and sooner when the server is at capacity — rendering or reading
one does not keep it alive, only editing does. Keep the authoritative copy of
anything you care about in a local `.scad` file.

## Independence and attribution

ReOpenSCAD is an independent reimplementation and is not affiliated with,
sponsored by, or endorsed by the OpenSCAD project. It implements the OpenSCAD
language and contains no OpenSCAD source code.

OpenSCAD itself is a separate project, licensed GPL-2.0-or-later and copyright
its authors. It lives at <https://openscad.org> and
<https://github.com/openscad/openscad>.

This project's own code is licensed under the **GNU General Public License,
version 3 or later** — see [`LICENSE`](LICENSE).
