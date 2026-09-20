# Hard-geometry regression fixtures

The OpenAPPA corpus next door is a *passing* corpus: it is smooth organic
geometry with no non-planar polygons and no near-tangent unions, and the exact
kernel reproduces its volume and topology exactly. That is precisely why it
stopped being a useful net — every defect found in the kernel so far was
invisible to it, and a "goldens unchanged" result kept coming back green while
real models came out cracked.

These are the models that broke things. They were added red — every row in
bold below was a defect — and they are green now; what follows records both,
because the numbers on the left are what the kernel has to keep earning.

| Source | Definitions | Reference | Boundary edges, then / now | Volume error, then / now |
| --- | --- | --- | --- | --- |
| `coffee-bin.scad` | `part="basket"` | `coffee-bin-basket.stl` | **293** / 0 | **1.6e-3** / 2.2e-9 |
| `coffee-bin.scad` | `part="tray"` | `coffee-bin-tray.stl` | 0 / 0 | 1.7e-8 / 7.0e-9 |
| `coffee-bin.scad` | `part="lid"` | `coffee-bin-lid.stl` | 0 / 0 | 2.3e-10 / 1.4e-10 |
| `louvre-pair.scad` | none | `louvre-pair.stl` | **2** / 0 | 2.2e-8 / 3.9e-8 |
| `hinge-box-narrow.scad` | none | `hinge-box-narrow.stl` | **87** / 0 | **8.4e-3** / 2.7e-10 |
| `hinge-box-wide.scad` | none | `hinge-box-wide.stl` | **18** / 0 | 1.1e-8 / 6.3e-9 |

Two distinct failures hid in that table, and keeping both was the point.

*The surface tears but the solid is right.* `louvre-pair` and `hinge-box-wide`
reproduced OpenSCAD's volume to eight significant figures and still came out
with holes in them. Whatever was wrong happened after the solid was decided:
the ear clipper, asked to triangulate a sliver-ridden merged face, stalled,
dropped flat vertices to get moving again, and handed back a cap with a slit
in it. Each dropped vertex was a corner of the face on the other side of that
edge, so losing it left one long edge facing two short ones. It now puts them
back — see `restore_dropped_vertices` in `csg/poly2d.rs`.

*The solid itself is wrong.* `coffee-bin-basket` and `hinge-box-narrow` lost
material — 0.16% and 0.84% — on top of tearing. `hinge-box-narrow` was the
sharper of the two: same author, same generator, same hinge design as
`hinge-box-wide`, which was fine. A pair that differs only in proportions and
disagrees only on one side was a better experiment than either alone, and it
paid off: the narrow box's loss traces to a single union of two knuckles that
meet face to face, which returned **less volume than either operand** because
both operands had been through the merge pass and come back with a pinhole in
them. A BSP decides "inside" by classifying against the other solid's faces,
and a surface with a hole in it has no reliable inside. The merge now runs
once, on the export path, and never as input to another boolean.

`coffee-bin.scad` is a real user workspace: a louvred coffee drip-bag bin whose
basket unions 64 curved blades, each five thin extruded trapezoids, into a
conical wall, then caps it with a top rim the blades pass through.

`louvre-pair.scad` is that model cut down to the smallest thing that still
broke: **two** adjacent blades. One blade alone closed; two did not. It is the
case to debug against — 424 facets rather than 20,000, with volume, area,
Euler characteristic and component count all already correct, so the only
thing left to explain was the two torn edges. `cargo run --release --example
csgtear -- <file.scad> [definitions]` prints exactly that: the edges a solid
fails to pair up, and for each one the nearest vertex to its interior, which
is how a T-junction announces itself.

`hinge-box-narrow.scad` and `hinge-box-wide.scad` are user workspaces too:
tablet boxes with a printed-in-place knuckle hinge, seventeen knuckles
interleaving cones and sockets across a seam. They are two-part assemblies —
base and lid in one output — so their shells legitimately touch.

## What the goldens are

Binary STL written by the OpenSCAD binary (2026.06.12) from these exact
sources, via `tests/oracle.sh`. They are oracles only — the server never
executes or links OpenSCAD.

OpenSCAD closes every one of these — the basket included (genus 192, volume
261580.951) — and reports every one as manifold. So the models are sound and
the reference is real geometry, not an aspiration.

`tests/oracle.sh check` re-renders all six and compares them byte for byte, so
a golden can be confirmed rather than trusted.

## What to assert against them, and what not to

Do **not** assert facet identity. OpenSCAD's triangulation of a given surface
is not ours and never will be; the OpenAPPA strict gate is red for exactly that
reason and has been ignored into uselessness as a result.

Assert what is intrinsic to the solid and independent of how it is cut into
triangles:

* watertightness — zero boundary edges;
* volume and surface area against the golden;
* component count.

Non-manifold edges and duplicate facets are compared *against the golden*
rather than against zero. The hinge boxes emit base and lid as one file, and an
edge shared where two solids touch belongs to the model, not to a defect;
asserting zero there would fail the fixture instead of the kernel. As it turns
out the kernel now beats the golden on both: OpenSCAD leaves 27 non-manifold
edges on the narrow box and 18 on the wide one, and our output has none.

Euler characteristic is a consequence rather than a cause here — a torn
surface changes `V - E + F` on its own, so it confirms a tear but never
localises one — and it is asserted only where the golden is itself manifold.
On the hinge boxes it is not: `V - E + F` over a surface whose two shells share
edges counts how many contacts the tessellator happened to weld, and the
golden's own figures give it away, two components at `chi = 16` working out to
genus -6.

Measured while these fixtures were added, the split was unambiguous: with the
face-merging pass disabled the basket's volume was 261580.959 against
OpenSCAD's 261580.951 — eight significant figures — while with it enabled the
same model came out at 262006.062 and cracked. The boolean kernel was right;
the merge pass was what lost the geometry. That measurement is what pointed at
the fix: the pass was not made lossless, it was moved. Booleans hand back raw
BSP fragments and the merge runs once on the way out, where nothing downstream
can be hurt by a surface that is a fraction of a micron from closed.

Two smaller things came with it. `emit_faces` used to rebuild a triangulated
face's corners through a 2D basis of its own choosing, which moved them a few
parts in 1e8 — above the welder's tolerance, so the neighbouring face stopped
sharing the edge; it now looks the original vertex up instead. And
`WELD_EPSILON` went from 1e-6 to 3e-5, because the widest spread an
ill-conditioned blade crossing produces on this 200 mm basket is 2.5e-5 mm,
and anything tighter leaves one corner as a cluster of three.
