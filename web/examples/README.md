# Kick-start models

The twelve models an empty workspace offers. Each one is three files:

| | |
| --- | --- |
| `<slug>.scad` | the source, opened into the editor when the card is clicked |
| `<slug>.png` | the card's preview |
| `manifest.json` | the catalogue: groups, names, blurbs, feature hints |

Served straight out of the web root at `/examples/`, which is why
`web/Dockerfile` copies this directory into the image. They are **not** inlined
in `app.js`: as real files they can be compiled by the test suite and rendered
by the OpenSCAD binary, which a string literal in a bundle never could be.

## What a model here has to be

* **Printable, and honest about it.** Real millimetres, walls at least 1.2 mm,
  clearances 0.2–0.4 mm, no overhang a printer would refuse. The opening
  comment says what the thing is and which way up it goes on the bed.
* **Parametric from the top.** Named variables with units and a comment each,
  Customizer ranges where a value has sensible bounds. The empty workspace
  tells people to "edit the numbers at the top", so that has to be true —
  `examples_compile.rs` counts them.
* **Inside the engine's language.** No `polyhedron()`, `import()`, `surface()`,
  `projection()`, `multmatrix()` or `resize()`; `hull()` only over cubes.
  `web/tests/examples.test.mjs` refuses the ones this engine cannot render.
* **Quick.** Under a second, ideally. A first click that spins is worse than a
  simpler model.

## Adding one

1. Write the `.scad` here and add it to `manifest.json` under a category.
2. Check it renders in the real OpenSCAD binary — `openscad --export-format
   binstl -o /tmp/check.stl <slug>.scad` — with `Status: NoError` and no
   warnings. That is a second opinion on the *model*, not on this engine.
3. Render the preview (below) and compress it.
4. `cargo test --manifest-path web/backend/Cargo.toml --test examples_compile`
   and `npm --prefix web test`.

## How the previews are made

The app's own viewer, not OpenSCAD's — a card should look like what the card
opens. Each one is the `#renderCanvas` contents after a final render, captured
with `toDataURL` over the DevTools protocol at a fixed camera:

```js
camera[0] = camera[1] = camera[2] = 0;
camera[3] = 27;                                     // pitch
camera[5] = -58;                                    // yaw: text reads forwards
camera[6] = Math.max(meshViewport.radius * 2.9, 20);
```

Axes and ground are switched off, so the PNG is the model on transparency. Then
`sips -Z 720` and `pngquant --quality=65-88`, which keeps all twelve inside
about 160 KB.

The yaw matters and is not arbitrary: at the app's default the `+x` axis runs
away from the viewer and lettering on a flat plate comes out mirrored.
