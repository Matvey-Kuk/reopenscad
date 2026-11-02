# Example models

The catalogue on the landing page: thirty printable models in eight
categories, each opening as a new design. Each one is two files plus its
manifest entry:

| | |
| --- | --- |
| `<slug>.scad` | the source, opened into the editor when the card is clicked |
| `<slug>.png` | the card's preview |
| `manifest.json` | the catalogue: categories in display order, names, blurbs, feature hints |

Served straight out of the web root at `/examples/`, which is why
`web/Dockerfile` copies this directory into the image. They are **not** inlined
in `app.js`: as real files they can be compiled by the test suite and rendered
by the OpenSCAD binary, which a string literal in a bundle never could be.

## What a model here has to be

* **Printable, and honest about it.** Real millimetres, walls at least 1.2 mm,
  clearances 0.2–0.4 mm, no overhang a printer would refuse. The opening
  comment says what the thing is and which way up it goes on the bed.
* **Parametric from the top.** Named variables with units and a comment each,
  `[min:step:max]` ranges in the comment where a value has sensible bounds. Someone who opens one
  from the landing page changes it by editing the numbers at the top, so they
  have to be there — `examples_compile.rs` and `examples.test.mjs` require at
  least four top-level assignments with a trailing comment.
* **Plain constructs.** No `polyhedron()`, `import()`, `surface()`,
  `projection()`, `multmatrix()` or `resize()`; `hull()` only over cubes.
  `web/tests/examples.test.mjs` refuses all but the `hull()` rule.
* **Quick.** Under a second, ideally. A first click that spins is worse than a
  simpler model.

## Adding one

1. Write the `.scad` here and add it to `manifest.json` under a category. A
   hint is two or three calls joined by ` · `, each one the model really
   makes, e.g. `for() · difference()`.
2. Check it renders in the real OpenSCAD binary — `openscad --export-format
   binstl -o /tmp/check.stl <slug>.scad` — with `Status: NoError` and no
   warnings. That is a second opinion on the *model*, not on this engine.
3. Render the preview (below) and compress it.
4. `cargo test --manifest-path web/backend/Cargo.toml --test examples_compile`
   and `npm --prefix web test`.

## How the previews are made

The app's own viewer, not OpenSCAD's — a card should look like what the card
opens. A Playwright script opens one design in the running app (2x device
scale, wide viewport), and for each example puts the source in the editor,
runs `render('render')`, and then sets a fixed camera before reading
`#renderCanvas`:

```js
selectedObjectId = null;                            // no selection highlight
meshViewport.drawAxes = meshViewport.drawGround = () => {};  // no UI toggle for these
camera[0] = camera[1] = camera[2] = 0;
camera[3] = 27;                                     // pitch
camera[5] = -58;                                    // yaw: text reads forwards
camera[6] = Math.max(meshViewport.radius * 4, 20);  // further out if it clips
meshViewport.draw();
```

The canvas is transparent with axes and ground stubbed out, so the capture is the model
on transparency in the viewer's own colours. Every example colours its parts
from a small soft palette (`#e4665f #f0965a #89c454 #5ccfc0 #55a9e8 #b48ef0
#f59ab8 #d9dde2`). `color()` works at any depth, so a layout module can colour
the parts it places (base and lid, body and brackets) and a single printed part
can carry a second colour (raised lettering) while staying one object. The script crops it to the model's alpha bounds, centres
it on a 4:3 frame with a small margin (the card's aspect), and saves it. Then
`sips -Z 720` (720 x 540) and `pngquant --quality=65-88`, which keeps all
thirty inside about 350 KB. One design is reused for every capture, and
renders are paced, so the run stays inside the per-IP rate limit.

The yaw matters and is not arbitrary: at the app's default the `+x` axis runs
away from the viewer and lettering on a flat plate comes out mirrored.
