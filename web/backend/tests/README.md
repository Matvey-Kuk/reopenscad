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

`oracle.sh check` is the only test utility that invokes the original OpenSCAD
binary. It is manual, never called by Cargo or production code, and renders to
a temporary directory. Fixture replacement additionally requires the explicit
`ALLOW_ORACLE_REGENERATION=1 tests/oracle.sh regenerate` command. Binary STL
bytes are not guaranteed to be stable between OpenSCAD releases, so the Rust
parity test is the authoritative geometry comparison.
