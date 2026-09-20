# Rendering corpus and parity tests

`corpus_parity.rs` supplies the reproducible test infrastructure required by
the project specification:

- validation of the three copied OpenAPPA sources and eight STL goldens;
- binary and ASCII STL parsing, including binary headers beginning with
  `solid`;
- strict comparison of normalized facets, bounds, area, volume, connected
  components, Euler characteristic, boundary/non-manifold edges, and duplicate
  facets;
- small passing native-engine smoke tests.

Run the normal suite with:

```sh
cargo test --manifest-path web/backend/Cargo.toml
```

Everything in that suite runs from a clean clone. Nothing in it reads the
sibling `openscad/` checkout, which is GPL reference material kept locally,
gitignored, and never built, linked, or shipped by this project.

The tests that *do* read `openscad/examples` — corpus discovery and the
full upstream compilation gate — are `#[ignore]`d, so they are opt-in and only
meaningful on a machine that has that reference checkout. The OpenAPPA parity
tests are ignored for a different reason: known language and implicit-mesher
differences still make them fail. All of them retain strict expectations and
can be run while closing those gaps:

```sh
cargo test --manifest-path web/backend/Cargo.toml --test corpus_parity -- --ignored --nocapture
```

`exact_kernel_parity.rs` measures `engine::compile_exact` against the same
goldens with the same comparator, so the two mesher paths are directly
comparable. Its whole-corpus gates are `#[ignore]`d for cost as well:

```sh
cargo test --manifest-path web/backend/Cargo.toml --test exact_kernel_parity -- --ignored --nocapture
```

## `hard_geometry_parity.rs` — the corpus that used to break the kernel

OpenAPPA is a corpus the kernel *passes*, to nine significant figures. That is
what makes it a poor net: it has no non-planar polygons and no near-tangent
unions, so "goldens unchanged" stayed green through three separate kernel
defects that broke real user models.

`hard_geometry_parity.rs` measures the models that did break, against goldens
the OpenSCAD binary produced from the same sources
(`tests/fixtures/hard-geometry/`). It was written red and is green; it runs in
the ordinary suite, unignored and cheap (a few seconds), so a regression is
noticed rather than looked up:

```sh
cargo test --manifest-path web/backend/Cargo.toml --release \
    --test hard_geometry_parity -- --nocapture
```

Unlike the OpenAPPA strict gate, this one does **not** assert facet identity.
OpenSCAD's triangulation is not ours and demanding it is what left that gate
permanently red and therefore switched off. These assertions are on what is
intrinsic to the solid and survives a different triangulation: watertightness,
volume, area, component count, and Euler characteristic where the golden is
itself a manifold surface. `tests/fixtures/hard-geometry/README.md` records
what each fixture caught and why the numbers are what they are.

`oracle.sh check` is the only test utility that invokes the original OpenSCAD
binary. It is manual, never called by Cargo or production code, and renders to
a temporary directory. Fixture replacement additionally requires the explicit
`ALLOW_ORACLE_REGENERATION=1 tests/oracle.sh regenerate` command. Binary STL
bytes are not guaranteed to be stable between OpenSCAD releases, so the Rust
parity test is the authoritative geometry comparison.
