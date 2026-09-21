# OpenSCAD language coverage audit

**Audited tree:** `/Users/matvey/Desktop/reopenscad/web/backend/src/` (Rust engine)
**Reference tree:** `/Users/matvey/Desktop/reopenscad/openscad/` — written `$OSCAD` in citations below (C++, commit `2fddda3`, `OpenSCAD version 2026.06.12` — the binary on `PATH` is the same version)
**Method:** static reading of both trees, plus ~145 differential probes compiled through *both* engines. Every "ran this and got X" below is reproducible; probe sources are throwaway and not checked in.

---

## Summary

The engine is a genuinely good *evaluator* with a genuinely narrow *module surface*. Expressions, list comprehensions (including `each`, `if/else`, nested and C-style `for`), ranges, lambdas, recursion with TCO, named/default arguments, `$`-variable dynamic scoping, all four modifier characters, and the `$fa/$fs/$fn` fragment formula are implemented and — where I could measure geometry — agree with OpenSCAD bit-for-bit on bounding boxes and often on triangle counts (`minkowski(){cube(10,center=true); sphere(2,$fn=8);}` produces **92 triangles with an identical bbox in both engines**). What is missing is concentrated in eight builtin modules (`polyhedron`, `import`, `surface`, `projection`, `resize`, `multmatrix`, `render`, `group`) that are a **hard error**, not a warning; in a `hull()` that accepts only cubes; and in a handful of evaluator divergences that silently produce different numbers (`-2^2`, `str()` formatting, vector comparison, `each "ab"`). The single most important structural finding is that **six of the eight missing modules already have complete, unit-tested implementations in `src/csg/` that the evaluator never calls** — `csg::primitives::polyhedron` (`csg/primitives.rs:217`), `csg::primitives::resize` (`csg/primitives.rs:582`), `csg::primitives::hull` (`csg/primitives.rs:509`), `csg::planar::hull` (`csg/planar.rs:111`), `csg::planar::projection_cut` (`csg/planar.rs:126`), `csg::planar::projection_shadow` (`csg/planar.rs:152`) — each has zero callers outside `src/csg/`, verified by grep. The gap is wiring, not geometry.

---

## 1. Modules

Reference registrations are in `$OSCAD/src/core/*.cc` via `Builtins::init("name", new BuiltinModule(...))`. Our dispatch is the `match name` at `engine.rs:3546`–`4349`; the fallthrough at `engine.rs:4346-4348` is a **hard error** (`EngineError`), aborting the whole compile, whereas OpenSCAD logs `WARNING: Ignoring unknown module 'x'` and carries on (`$OSCAD/src/core/Context.cc:127`).

### 2D primitives

| Feature | Status | Notes |
|---|---|---|
| `square(size, center)` | implemented | `engine.rs:3547`. Ref `primitives.cc:514`. Probe `square([4,2],center=true)` → both bbox `[-2,-1,0, 2,1,1]`, 12 tris. |
| `circle(r\|d)` | implemented | `engine.rs:3621`; `$fn/$fa/$fs` via `fragment_count` (`engine.rs:5598`). Probe `circle(d=10,$fn=8)` → both 28 tris, bbox `[-5,-5,0, 5,5,1]`. |
| `polygon(points, paths, convexity)` | implemented | `engine.rs:3576`. `convexity` accepted and ignored (`engine.rs:3595-3598`) — harmless, it is preview-only in ref too. Probe with 2 paths → both 32 tris. |
| `text(...)` | **partial** | See §6. Own single-stroke font; `font`/`direction`/`language`/`script` ignored, `em` missing. |
| `import(file, ...)` | **missing** | Ref `ImportNode.cc:301`, params `{file, layer, convexity, origin, scale}` + `{width, height, filename, layername, center, dpi, id}` (`ImportNode.cc:68-70`). Ours: hard error. Probe `import("x.stl")` → ref warns "Can't open import file", ours `Unsupported module import()`. |
| `projection(cut, convexity)` | **missing (kernel exists)** | Ref `ProjectionNode.cc:64`. Ours: hard error. **But** `csg::planar::projection_cut` (`csg/planar.rs:126`) and `projection_shadow` (`csg/planar.rs:152`) are implemented and unit-tested (`csg/planar.rs:231`) with no callers. |
| `offset(r, delta, chamfer)` | **partial** | `engine.rs:3952`. `r` (rounded) and `delta` (mitred) both honoured; `chamfer=true` is silently downgraded to mitred with `WARNING: offset(chamfer = true) is mitred here` (`engine.rs:3982-3986`). Probe `offset(r=-1) square(10)` → both bbox `[1,1,0, 9,9,1]`, 12 tris. |

### 3D primitives

| Feature | Status | Notes |
|---|---|---|
| `cube(size, center)` | implemented | `engine.rs:3635`. **Divergence:** a zero or negative component makes us emit `WARNING: Ignoring invalid cube() geometry` (`engine.rs:3650`); ref silently produces nothing. Same outcome, noisier. |
| `sphere(r\|d)` | implemented | `engine.rs:3667`. Probe `sphere(d=10)` → 252 tris ours. |
| `cylinder(h, r1, r2, r, d, d1, d2, center)` | implemented | `engine.rs:3681`, radii resolved by `cylinder_radii`. Probe `cylinder(h=2,r=5,$fn=5)` → both 16 tris, bbox `[-4.0451,-4.7553,0, 5,4.7553,2]` **exactly equal**. `r1=0` cone → both 18 tris, identical bbox. |
| `polyhedron(points, faces, convexity)` | **missing (kernel exists)** | Ref `primitives.cc:751`. Ours: hard error (probe `m_polyhedron`). `csg::primitives::polyhedron` (`csg/primitives.rs:217`) exists, is tested (`csg/primitives.rs:739`, `:761`), and has no caller. |
| `surface(file, center, convexity, invert)` | **missing** | Ref `SurfaceNode.cc:332`, params at `:69-70`. Ours: hard error. Needs a file source this service does not have (see §5 on `include`), so it is arguably out of scope; at minimum it should degrade to a warning rather than abort. |
| `linear_extrude(height, v, scale, center, twist, slices, segments, convexity, h)` | **partial** | `engine.rs:3998`. Honoured: `height`, `center`, `twist`, `scale` (scalar or 2-vector), `slices`. **Missing:** `v` (the extrusion-direction vector — probe `linear_extrude(height=5, v=[0.3,0.2,1])` gives an upright prism in ours, 12 tris, and is silently ignored), `segments` (outline resubdivision, `CurveDiscretizer::splitOutline`, `CurveDiscretizer.cc:379`), and the `h=` alias. `convexity` accepted/ignored (`engine.rs:4016`). Slice-count heuristic (`engine.rs:4060-4074`) is not OpenSCAD's `getHelixSlices`/`getConicalHelixSlices` (`CurveDiscretizer.cc:245`, `:279`), so twisted extrusions will differ in facet count even when the shape is right (probe `linear_extrude(h=10,twist=90,slices=20,scale=0.5)`: ours 164 tris, ref 500). |
| `rotate_extrude(angle, start, convexity, a)` | **partial** | `engine.rs:4135`. `angle` honoured and facet-scaled correctly — probe `rotate_extrude(angle=90,$fn=12) translate([10,0]) square(1)` → **both 28 triangles**. **Missing:** `start` (probe `rotate_extrude(angle=90,start=45)` → both 68 tris but ours ignores `start`, so the sweep begins at 0°) and the `a=` alias. Profile reaching `x<0` is refused (`engine.rs:4160-4166`) matching ref's refusal. |
| `roof(method, convexity)` | **missing** | Ref `RoofNode.cc:62`, gated behind `Feature::ExperimentalRoof` — **disabled by default in the reference binary too** (probe: ref says `WARNING: Experimental builtin module 'roof' is not enabled`). Lowest priority. |

### Transforms

| Feature | Status | Notes |
|---|---|---|
| `translate(v)` | implemented | `engine.rs:3799`. Short vectors padded with 0 — probe `translate([1,2]) cube(1)` → both bbox `[1,2,0, 2,3,1]`. |
| `rotate(a, v)` | implemented | `engine.rs:3810`. All three forms work: `rotate([x,y,z])`, `rotate(a,v)`, `rotate(a=45)`. Probe `rotate(a=90,v=[1,1,0]) cube([1,2,3])` → both bbox `[0,-2.1213,-0.7071, 3.6213,1.5,1.4142]`; `rotate([10,20,30])` → both `[-0.8819,0,-0.342, 1.9494,2.2891,3.1026]`. |
| `scale(v)` | **partial — bug** | `engine.rs:3908`. Scalar works (`[one] =>` at `:3914`), 3-vector works, but a **2-element vector is rejected**: `_ => ignore_geometry("scale", "value must be a number or 3-vector")` (`engine.rs:3917`). Ref pads the missing component with **1.0** (`TransformNode.cc:60`, `getVec3(...,1.0)`). Probe `scale([1,2]) cube(1)`: ref bbox `[0,0,0, 1,2,1]`; **ours errors out with "The program produced no geometry"**. |
| `resize(newsize, auto, convexity)` | **missing (kernel exists)** | Ref `CgalAdvNode.cc:152`, params `:80`. Ours: hard error. `csg::primitives::resize` (`csg/primitives.rs:582`) implements it, uncalled. |
| `mirror(v)` | implemented | `engine.rs:3932`. Probe `mirror([1,1]) translate([1,1,0]) cube(1)` → both bbox `[-2,-2,0, -1,-1,1]`. |
| `multmatrix(m)` | **missing** | Ref `TransformNode.cc:308`, accepts 4×4 / 4×3 / 3×3 (`:233-260`). Ours: hard error. The internal `Transform` type already carries a general 4×4 (`engine.rs`, `Transform::point`), so this is a small arm, not new geometry. |
| `color(c, alpha)` | implemented | `engine.rs:4188`. Named colours, `#rrggbb`, and vectors all accepted (`parse_color_value`). Bad values warn and pass children through, matching ref's non-geometric treatment. |
| `hull()` | **partial — severely** | See §5. |
| `minkowski(convexity)` | implemented (exact kernel) | `engine.rs:3738` → `csg::primitives::minkowski` (`csg/primitives.rs:539`). Probe `minkowski(){cube(10,center=true); sphere(2,$fn=8);}` → **92 triangles and bbox `[-6.8478,…,6.8478]` in both engines**. 2D minkowski is **not** supported: probe `minkowski(){square(10); circle(2);}` → ours `ERROR: This shape cannot be used where 2D geometry is expected` (`engine/exact.rs:534`); ref gives 36 tris. Ref routes 2D minkowski to `applyMinkowski2D` (`GeometryEvaluator.cc:421`). |

### CSG operators

| Feature | Status | Notes |
|---|---|---|
| `union()` | implemented | `engine.rs:3714`. |
| `difference()` | implemented | `engine.rs:3719`. Single child is a no-op in both. |
| `intersection()` | implemented | `engine.rs:3729`. Empty intersection handled. |
| `render(convexity)` | **missing** | Ref `RenderNode.cc:60`. Semantically a no-op for a mesher that always renders exactly — but ours **hard-errors**, so any model containing `render()` (a very common "force CGAL" idiom) fails outright. One-line fix: treat as `union`. |
| `group()` | **missing** | Ref `GroupModule.cc:46`. Same story — should be `union`. Probe `group(){cube(1);}` → ours hard error. |
| `fill()` | **missing** | Ref `CgalAdvNode.cc:147`, 2D-only (`applyFill2D`, `GeometryEvaluator.cc:271`); 3D is a warning in ref too (`GeometryEvaluator.cc:152-156`). Ours: hard error. |
| 2D/3D mixing | **divergent** | Ref warns `Mixing 2D and 3D objects is not supported` + `Ignoring 2D child object for 3D operation` and proceeds with the 3D children (`GeometryEvaluator.cc:115-125`, `:400-404`). Ours hard-errors: probe `union(){cube(5); square(5);}` → `union() cannot mix 2D and 3D geometry` (`require_same_dimension`). |

### Flow control

| Feature | Status | Notes |
|---|---|---|
| `for(a=..., b=...)` | implemented | Statement form parsed at `engine.rs:1974`; multi-generator works (probe `for(i=[0:1], j=[0:1])` → 48 tris = 4 cubes). Cap `MAX_FOR_ITERATIONS = 10_000` (`engine.rs:3110`) vs ref's 1_000_000 (`Expression.cc:893`) — **a legitimate 20 000-iteration loop fails here and works in OpenSCAD**. |
| `intersection_for(...)` | implemented | `engine.rs:1977`. Probe → 20 tris ours, ref renders the same solid. |
| `if / else / else if` | implemented | `engine.rs:1983`; `else if` chains verified. |
| `let(...)` as module | implemented | `engine.rs:1980`. |
| `assign(...)` | **missing** | Removed from modern OpenSCAD too — ref 2026.06 says `WARNING: Ignoring unknown module 'assign'`. Not worth implementing; but ours aborts where ref continues. |
| `echo(...)` | implemented | `engine.rs:4256`; also an expression (`engine.rs:2293`). **Ordering divergence:** for `x = echo("hi") 5; echo(x);` ref prints `hi` then `5`, ours prints `5` then `hi` (expression-echo messages are buffered and flushed after statement echoes). |
| `assert(cond, msg)` | implemented | Module form `engine.rs:4239`, expression form `engine.rs:2277`. Failure is a hard error in both. Message text differs: ref `Assertion 'false' failed: "boom"`, ours `Assertion failed. "boom"` — ref quotes the *source text* of the condition, which is materially more useful. |
| `children()` / `children(i)` / `children([a:b])` / `children([i,j])` | implemented | `engine.rs:4271`, `selected_children` at `engine.rs:4352`. **Divergence:** out-of-range or negative index is a **hard error** in ours (`children() indices must be non-negative whole numbers`), a warning in ref (`Children index (-1) out of bounds (1 children)`, `control.cc:55-65`). |
| `$children` | implemented | Set per user-module call at `engine.rs:4283`. Correctly *not* inherited (ref makes it lexical deliberately, `ContextFrame.cc:150-153`). |
| Module recursion | **implemented but capped at 16** | `MAX_MODULE_RECURSION_DEPTH = 16` (`engine.rs:3109`), enforced `engine.rs:4272`. Ref has **no count limit at all** — it checks remaining stack (`UserModule.cc:94-98`, `StackCheck.h:21`, 8 MB default). Probe: a 17-deep recursive module fails here, a 200-deep one renders in OpenSCAD. This is the single most likely-to-bite limit in the whole engine. |

---

## 2. Builtin functions

Reference list: `$OSCAD/src/core/builtin_functions.cc:1115-1341`. Ours: `engine/builtins.rs:8-72`.

| Function | Status | Notes |
|---|---|---|
| `abs sign floor ceil round sqrt exp ln log pow` | implemented | `builtins.rs:10-33`. Values match. |
| `sin cos tan asin acos atan atan2` | implemented | `builtins.rs:25-33`, degrees. |
| `min max` | implemented | `builtins.rs:34-35`. Probe `min(1,2), min([3,1,2]), max([1,5]), max(1,2,3)` → both `1, 1, 5, 3`. |
| `norm` | implemented | `builtins.rs:36`. `norm([1,"a"])` → both `undef` (+warning in both). |
| `cross` | implemented | `builtins.rs:37`. 2D form returns the scalar z-component in both: probe `cross([1,0],[0,1]), cross([1,2],[3,4])` → both `1, -2`. |
| `rands(min,max,n[,seed])` | implemented, **different sequence** | `builtins.rs:38`. `rands(0,1,3,42)` → ref `[0.796543, 0.183435, 0.779691]`, ours `[0.60605…, 0.22592…, 0.89452…]`. A seeded `rands` is the only way to get *reproducible* randomness in SCAD, so any model relying on it produces different geometry here. Worth documenting rather than "fixing" (matching ref would mean copying its PRNG, which the clean-room rule forbids — but a note in the docs is cheap). |
| `len` | implemented | `builtins.rs:39`. Strings counted by codepoint (probe `len("☃☃")` → 2 in both). `len(range)` → `undef` in both (ref `builtin_functions.cc:368-381`). |
| `concat` | implemented | `builtins.rs:40`. Probe `concat([1,2],[3],4,"x")` → both `[1,2,3,4,"x"]`; `concat([0:2],[1])` → both keep the range as one element. |
| `str` | implemented, **formats numbers differently** | `builtins.rs:41`. See §4 "number formatting" — this is a *geometric* divergence via `text(str(x))`. |
| `chr` | implemented | `builtins.rs:42`. Probe `chr(65), chr([66,67]), chr(9731)` → both `"A", "BC", "☃"`. |
| `ord` | implemented | `builtins.rs:43`. Probe `ord("A"), ord("☃"), ord("")` → both `65, 9731, undef`. |
| `lookup` | implemented | `builtins.rs:44`. Interpolation and clamping match: `lookup(2.5,[[0,0],[1,10],[3,30]])` → both `25`; out-of-range clamps in both. |
| `search` | implemented | `builtins.rs:45`. All four probe forms agree exactly with ref, including `num_returns_per_match=0`. |
| `is_undef is_bool is_num is_string is_list is_function` | implemented | `builtins.rs:46-56`. |
| `version` / `version_num` | implemented, **wrong value** | `builtins.rs:57-63`: hard-codes `[2021,1,0]` / `20210100`. Ref returns the real version (`[2026,6,12]`). Feature-detection idioms (`version_num() >= 20190500`) therefore see a 2021 engine even though the language level implemented is closer to 2021.01 — arguably correct, but a model gating on `each` or `function` literals will take the wrong branch if it checks for a newer version. |
| `parent_module(n)` | **missing** | Ref `builtin_functions.cc:1290`, impl `:863-886`. Probe: ref `"a", "b"`, ours `undef, undef` + `WARNING: Unsupported function parent_module()`. |
| `textmetrics` / `fontmetrics` | missing (also off by default in ref) | Ref gates them behind `Feature::ExperimentalTextMetricsFunctions` (`builtin_functions.cc:1236-1247`); the shipped binary reports them unknown too. Not a real gap. |
| `is_object` / `object` / `has_key` | missing (experimental in ref) | `builtin_functions.cc:1325-1339`. Not a real gap. |
| `import` (function form) | missing (experimental in ref) | `builtin_functions.cc:1341`. |
| `dxf_dim` / `dxf_cross` | **missing** | Ref `io/dxfdim.cc:253`, `:258`. Legacy DXF-dimension lookup; needs file access. Not a practical gap for a hosted service. |
| Named args to builtins | **narrower than ref** | Ours accepts named arguments only for `rands` and `search` (`engine.rs:5068-5077`), otherwise errors `Built-in function x() does not accept named arguments.` Ref binds named args for every builtin from its declared parameter list (`Parameters.cc:163-231`). |

---

## 3. Special variables

Ref defaults: `$OSCAD/src/core/Builtins.cc:110-130`. Ours: seeded at `engine.rs:3125-3133`.

| Variable | Ref default | Ours | Status |
|---|---|---|---|
| `$fn` | `0` (`Builtins.cc:112`) | `0` (`engine.rs:3127`) | implemented |
| `$fa` | `12` (`Builtins.cc:118`) | `12` (`engine.rs:3128`) | implemented |
| `$fs` | `2` (`Builtins.cc:117`) | `2` (`engine.rs:3129`) | implemented |
| `$t` | `0` (`Builtins.cc:119`) | `0` (`engine.rs:3130`) | implemented (no animation driver, so always 0 — same as a single non-animated render) |
| `$preview` | `undef`, overwritten per render (`Builtins.cc:120`, `RenderVariables.cc:6-19`) | `self.preview` bool (`engine.rs:3131`) | implemented |
| `$children` | set per user-module call, **lexical** (`ScopeContext.cc:70`, `ContextFrame.cc:152`) | same (`engine.rs:4283`) | implemented |
| `$vpr $vpt $vpd $vpf` | `[0,0,0] / [0,0,0] / 500 / 22.5` (`Builtins.cc:122-129`), overwritten from the camera | **absent** | **missing** — zero occurrences in `src/`. Probe `echo($vpr,$vpt,$vpd,$vpf)` → ref `[55,0,25], [0,0,0], 140, 22.5`; ours `undef, undef, undef, undef`. Models that place labels or cutaways relative to the viewport silently degrade. Since this backend does render server-side with a known camera, plausible values could be supplied. |
| `$fe` | experimental (`Feature.cc:46`) | absent | not a real gap |
| `$parent_modules` | dynamic (`ScopeContext.cc:71`) | absent | same gap as `parent_module()` |
| `PI` | `M_PI` (`BuiltinContext.cc:23`) | implemented (`engine.rs:4719`) | implemented |

**Fragment formula.** Ref `CurveDiscretizer::getCircularSegmentCount` (`CurveDiscretizer.cc:100-154`):

```cpp
if (fn > 0.0) result = std::ceil(fn >= 3 ? fn : 3) * fabs(angle)/360.0;
else          result = std::ceil(max(min(360.0/fa, r*2*M_PI/fs), 5.0)) * fabs(angle)/360.0;
return std::max(1, (int)std::ceil(result));
```

Ours (`engine.rs:5598-5620`):

```rust
if radius < 1e-6 { return 3; }
let fragments = if special("$fn", 0.0) > 0.0 { special("$fn", 0.0).max(3.0).floor() }
else { (360.0/fa).min(radius*2.0*PI/fs).max(5.0).ceil() };
(fragments as usize).clamp(3, MAX_CURVE_FRAGMENTS)
```

Two differences: (a) ref `ceil`s a fractional `$fn`, ours `floor`s it — `$fn=6.5` gives 7 facets there and 6 here; (b) ref's small-radius cutoff is `r < GRID_FINE` (`9.5367e-7`, `Grid.h:19`) returning *no* segments, ours is `radius < 1e-6` returning 3. Both are immaterial in practice; the `$fn` rounding is not, if anyone computes `$fn` arithmetically. Ref clamps `$fn<0` to 0 with a warning (`CurveDiscretizer.cc:36-39`); ours takes `$fn=-3` as "not positive" and falls through to `$fa/$fs`, same effective result.

---

## 4. Language semantics

| Feature | Status | Notes |
|---|---|---|
| List comprehension `each` | **partial** | `engine.rs:2386`, eval `4951-4966`. Vectors and ranges splice correctly. **`each "ab"` does not iterate a string:** probe → ref `["a","b"]` (`Expression.cc:842-847`), ours `["ab"]` (`engine.rs:4959` pushes any non-vector/non-range as one element). |
| `for` over a string | **missing** | Probe `[for(c="abc") c]` → ref `["a","b","c"]` (`Expression.cc:923-931`); ours `ERROR: for() expects a range or vector.` (`engine.rs:3500`) — a **hard failure**, not a degraded value. |
| `for` over an object | missing | Ref iterates keys (`Expression.cc:914-922`); objects are experimental in ref, so not a real gap. |
| Comprehension `if` / `if…else` | implemented | `engine.rs:2424-2439`. Probes match ref exactly. |
| Nested / multi-variable `for` generators | implemented | `engine.rs:2389`, `5013-5036`. `[for(i=[0:2]) for(j=[0:1]) [i,j]]` and `[for(i=[0:2], j=[0:1]) [i,j]]` both match ref. |
| C-style `for(init; cond; next)` comprehension | implemented | `engine.rs:2397-2416`, eval `4967-4989`. Matches ref (`Expression.cc:974-1008`). |
| `let` inside comprehension | implemented | `engine.rs:2440`, sequential bindings like ref (`Expression.cc:735-753`). |
| Range `[a:b]`, `[a:s:b]` | implemented | `engine.rs:2323-2335`. Fractional and negative steps match ref exactly. |
| Range with **step 0** | **divergent — hard error** | Ours: `ERROR: for() range step cannot be zero.` (`engine.rs:3478`), aborting the compile. Ref: `WARNING: Bad range parameter in for statement: too many elements (4294967295)` and the loop simply produces nothing (`RangeType.h:150-171`, `Expression.cc:893-896`). A computed step that hits 0 for one iteration kills the whole model here. |
| Reversed range with positive step | implemented (warns) | Both yield `[]`; ref warns only for literal ranges (`Expression.cc:283-289`), ours always (`engine.rs:4711-4715`). |
| Range indexing `r[0] r[1] r[2]` | **missing** | Ref: begin/step/end (`Value.cc:1385-1394`); probe `r=[0:2:10]; r[0],r[1],r[2]` → ref `0, 2, 10`, ours `undef, undef, undef` (`engine.rs:4795`). `.begin/.step/.end` members likewise missing (ref `Expression.cc:416-420`). |
| Range iteration cap | narrower | Ours 10 000 (`engine.rs:3110`), ref 1 000 000 (`Expression.cc:893`); `children(range)` and `chr(range)` use 10 000 in ref (`RangeType.h:18`). |
| String escapes | implemented, **slightly wider** | Ours `\" \\ \t \n \r \xNN \uNNNN \UNNNNNN` (`engine.rs:1683-1692`), plus unknown escapes pass the char through. Ref (`lexer.l:183-190`) is the same set but `\x` accepts only `[0-7][0-9a-f]` (ASCII only) and `\x00`→space, invalid `\u`→space, and an unknown escape **warns** and drops the backslash. Ours also **keeps a raw newline inside a string literal**; ref silently drops it (`lexer.l:193`). Minor. |
| Unicode in strings / indexing | implemented | Probe `"héllo"[1], len("héllo")` → both `"é", 5`. Codepoint-indexed in both. |
| **Hex literals `0x…`** | **missing** | Ref lexes them (`lexer.l:287-300`); probe `echo(0x10)` → ref `16`, ours `ERROR: Expected RParen, found Some(Ident("x10"))` — a **parse failure**, so the file will not compile at all. |
| **Bitwise / shift operators `& \| ~ << >>`** | **missing** | Ref grammar levels 6–8 (`parser.y:410-436`) and `Value.cc:1279-1348`. Probe `echo(5\|2, 5&3, 1<<3, 8>>2, ~0)` → ref `7, 1, 8, 2, -1`; ours `ERROR: Line 1: unsupported character '\|'` — the **lexer** rejects the character, so the whole file fails. |
| Operator precedence | **one real bug** | Ours `engine.rs:2185-2199`. `^` is level 8, unary is level 9 — so `-2^2` parses as `(-2)^2 = 4`. Ref puts `exponent` *tighter* than `unary` (`parser.y:467-500`), giving `-(2^2) = -4`. **Probe confirms: ref `-4`, ours `4`.** (`2^3^2 = 512` right-associative and `2^-2 = 0.25` both match.) Also, in ref `&`/`\|` bind tighter than comparisons — moot while they are unimplemented. |
| `?:` | implemented | `engine.rs:2209-2214`, right-associative, lazy, matches ref. |
| `%` modulus | implemented | Level 7, `fmod` semantics, matches ref (`Value.cc:1271`). |
| Unary `+`, `!` | implemented | `+"abc"` → `"abc"` in both. |
| Vector `+`/`-` with mismatched lengths | **divergent** | Ref truncates to the shorter vector (`Value.cc:1052-1058`): `[1,2]+[1,2,3]` → `[2,4]`. Ours returns `undef` + warning (`engine.rs:5320-5323`). |
| Vector / mixed-type **ordering** comparison | **divergent** | Ref compares vectors lexicographically: `[1,2]<[1,3]` → `true`, `[1,2]<[1,2,3]` → `true` (`Value.cc:845-859`); mixed types → `undef` (`Value.cc:922-940`). Ours returns `false` for all of them + a warning (`engine.rs:5469-5493`). Probe: ref `true, true, true, undef, undef`; ours `false, false, true, false, false`. Sorting helpers written in SCAD (a common library idiom) will silently mis-sort. |
| `undef` propagation | mostly implemented | Arithmetic → `undef` + warning in both; out-of-range index → `undef` in both; `undef==undef` → `true` in both; `!undef` → `true` in both. Only the ordering case above differs. |
| **Number formatting in `str()`/`echo`** | **divergent** | Ref prints 6 significant digits (`DC_PRECISION_REQUESTED = 6`, `Value.cc:76`); ours prints Rust's shortest round-trip. Probe `echo(1/3, 2.718281828459045, 3628800, 1e11, 1e-10, 0.1+0.2)`: ref `0.333333, 2.71828, 3.6288e+6, 1e+11, 1e-10, 0.3`; ours `0.3333333333333333, 2.718281828459045, 3628800, 100000000000, 0.0000000001, 0.30000000000000004`. This is **not cosmetic**: `text(str(width))` on a dimension label renders a different string, and therefore different geometry. |
| `$`-variable scoping | implemented (dynamic) | `special_variables()` at `engine.rs:2592-2612`; injected on module call (`engine.rs:4280`), function call (`engine.rs:5202-5206`) and `children()` (`engine.rs:4384-4388`). Matches ref's `is_config_variable` split (`ContextFrame.cc:150-153`). Probes `s_dynscope`, `s_dynscope_fn2` agree with ref. |
| Module recursion limit | **16 vs unbounded** | See §1. |
| Function recursion | implemented | Self-tail-recursion optimized to a loop with a 10 000 bound (`engine.rs:5120`, `5251-5283`); non-tail capped at 256 (`engine.rs:5121`). Ref: stack-bounded, tail form capped at 1 000 000 (`Expression.cc:626-630`). `fac(10)` matches. Deep non-tail recursion (mutual `even`/`odd` over a few hundred) will fail here and work there. |
| Modifier `%` (background) | implemented | `engine.rs:1931`, geometry diverted at `engine.rs:3314-3321`. Matches ref's exclusion (`GeometryEvaluator.cc:309` etc.). |
| Modifier `#` (highlight) | implemented (no-op) | `engine.rs:1897-1900` — kept in geometry, discarded as a tag. Ref also keeps it in geometry; only the preview overlay differs. Acceptable for an STL-producing backend, though the UI could use the tag. |
| Modifier `!` (root) | implemented | `engine.rs:1936`, outermost wins (`engine.rs:3322-3340`). Ref picks the *first found in the instantiated tree* (`node.cc:184-210`) and warns on a second distinct one; ours warns unconditionally. Equivalent for single-`!` files. |
| Modifier `*` (disable) | implemented | `engine.rs:1919-1922`, subtree never evaluated. Ref deletes it at **parse** time (`parser.y:250-254`) — so `*` in ref also suppresses `echo`/`assert` side effects; ours does too (`engine.rs:3313`). Match. |
| `include <...>` | **stubbed** | `engine.rs:400-472`. Not a filesystem — an exact-match allowlist (`BundledLibrary`, `engine.rs:360-386`) containing exactly four names: `units.scad`, `MCAD/units.scad`, `shapes.scad`, `MCAD/shapes.scad` (`engine.rs:282-287`). Anything else is a **hard error**. Ref resolves via `$OPENSCADPATH`, the user library dir, and the resources dir (`parsersettings.cc:86-172`). For a hosted service with no user FS this is a defensible design, but note the `MCAD/*` aliases are 2 hand-written files, not MCAD — a model doing `include <MCAD/involute_gears.scad>` fails. |
| `use <...>` | implemented (same allowlist) | `engine.rs:465-468` — definitions only, matching ref's semantics (`ScopeContext.cc:83-136`). Correctly rejected below top level (`engine.rs:3341-3355`). |
| Assignment hoisting / last-wins | implemented | `engine.rs:2939-2985`, `3229-3268`; warns on duplicates like ref (`parser.y:735-792`). Probe `a=1; echo(a); a=2;` → both echo `2`. |
| Named/default/positional argument binding | implemented | `engine.rs:5209-5249` (functions), `4286-4331` (modules). Probe `f(x=1,y=2)` with all four call shapes matches ref exactly. Unknown named arg: ref warns (`Parameters.cc:200-203`), ours warns for modules/functions but is silent for builtin modules (probe `cube(size=5, bogus=1)` → ref warns, ours silent). |
| Lambdas / function literals | implemented | `engine.rs:2253-2259`; first-class, curryable. |
| `let` / `assert` / `echo` as expressions | implemented | `engine.rs:2260`, `2277`, `2293`. |
| Vector member `.x .y .z` | implemented | `engine.rs:4799-4813`. Swizzling (`v.xyz`) is experimental in ref; not a gap. |
| List-element assignment `a[1]=9` | correctly rejected | Both are parse errors. |

---

## 5. Deep dive: `hull()`

### What OpenSCAD actually does

`hull` is registered at `$OSCAD/src/core/CgalAdvNode.cc:142` with **no parameters at all** (`Parameters::parse(..., {})` at `CgalAdvNode.cc:58`) — so `hull()` takes no `convexity`, no `$fn`, nothing.

Dispatch is dimension-driven (`$OSCAD/src/geometry/GeometryEvaluator.cc:128-138`): `applyToChildren` computes `dim` from the children and calls `applyToChildren2D` or `applyToChildren3D`.

* **3D** (`GeometryEvaluator.cc:151-153`, `:211-222`): `applyHull3D` on the collected 3D children. Crucially this happens **before** the `children.size() == 1` no-op shortcut at `:161`, so `hull()` of a *single* non-convex child really does convexify it. Probe `hull() difference(){cube(10,center=true); cube([20,4,4],center=true);}` renders a solid 10-cube in the reference.
* **2D** (`GeometryEvaluator.cc:420-423`, `:232-272`): `applyHull2D` collects every vertex of every outline of every 2D child into one point cloud and runs `CGAL::convex_hull_2`, emitting **one** outline. Disjoint 2D children are therefore bridged into a single convex region — probe `hull(){square(2); translate([20,10]) square(2);}` gives 20 triangles after `linear_extrude`, i.e. one connected quad-ish prism.
* **Mixed 2D/3D**: `isValidDim` (`GeometryEvaluator.cc:115-125`) warns `Mixing 2D and 3D objects is not supported` and stops scanning, so `dim` is whatever the *first* non-background child had. If that is 3D, `collectChildren3D` additionally warns `Ignoring 2D child object for 3D operation` and replaces the 2D child with empty geometry (`:400-404`). Probe `hull(){cube(5); circle(10);}` → both warnings, then the hull of just the cube.
* **`$fn`**: `hull()` itself ignores it; it only affects how the *children* were tessellated before hulling. Probe `hull($fn=6){sphere(5); …}` differs from `$fn=64` only because the spheres do.
* **Non-convex input**: irrelevant — the hull discards concavity by construction.

### What we do

The evaluator arm is `engine.rs:3784-3798`:

```rust
"hull" => {
    let shapes = self.child_shapes(children, environment)?;
    if shapes.is_empty() { return Ok(None); }
    if shapes.iter().any(|shape| shape.dimension() != ShapeDimension::Solid) {
        return Err(EngineError::new("hull() currently expects only 3D child geometry."));
    }
    self.cached_convex_hull(&shapes).map(Some)
}
```

`cached_convex_hull` (`engine.rs:4612`) → `Shape::hull_vertices` (`engine.rs:7142`) → `Shape::append_hull_vertices` (`engine.rs:7167-7191`), which accepts **only** `Shape::Box`, `Shape::Transform`, `Shape::Union` and `Shape::Color`, and errors on everything else:

```rust
_ => return Err(EngineError::new(
        "hull() currently supports cubes and unions/transforms of cubes.")),
```

The result is not a mesh but a set of support planes (`Shape::Hull { planes, bounds, .. }`, `engine.rs:6658`, built by `convex_support_planes` at `engine.rs:7270`), consumed by the exact kernel at `engine/exact.rs:384`.

### Measured divergence

| Probe | OpenSCAD | Ours |
|---|---|---|
| `hull(){cube(5); translate([20,0,0]) cube(5);}` | 16 tris, bbox `[0,0,0, 25,5,5]` | **12 tris, same bbox** — correct |
| `hull(){rotate([0,0,30]) cube(5); …}` | 18 tris | **18 tris, identical bbox** — correct |
| `hull(){scale([2,1,1]) cube(5); …}` | 18 tris | **18 tris, identical bbox** — correct |
| `hull(){color("red") cube(1); …}` | 16 tris | 12 tris, correct bbox |
| `hull(){for(i=[0:2]) translate([i*5,0,0]) cube(1);}` | 12 tris | 12 tris — correct |
| `hull(){ cylinder(h=2,r=5); translate([0,0,10]) cylinder(h=2,r=1); }` | 34 tris | **ERROR** `hull() currently supports cubes and unions/transforms of cubes.` |
| `hull(){ sphere(5); translate([20,0,0]) sphere(3); }` | renders | **ERROR** |
| `hull(){ cube(5); translate([9,0,0]) sphere(2); }` | 44 tris | **ERROR** |
| `hull() difference(){…}` (single non-convex child) | 18 tris | **ERROR** |
| `hull() intersection(){…}` | 24 tris | **ERROR** |
| `hull(){ hull(){…} … }` (nested) | 16 tris | **ERROR** |
| `hull(){ linear_extrude(1) square(5); translate([0,0,10]) cube(1); }` | renders | **ERROR** |
| `hull(){ polyhedron(…); … }` | renders | ERROR (no `polyhedron` at all) |
| `hull(){ circle(5); translate([20,0]) circle(3); }` (2D) | 52 tris after extrude | **ERROR** `hull() currently expects only 3D child geometry.` |
| `hull(){ square(2); translate([20,10]) square(2); }` (2D disjoint) | 20 tris | **ERROR** |
| `hull() text("A")` (2D) | 12 tris | **ERROR** |
| `hull(){ cube(5); circle(10); }` (mixed) | 2 warnings, hulls the cube | **ERROR** |

**What that rules out.** `hull()` is one of the two or three most-used operators in real OpenSCAD (rounded slots as `hull()` of two cylinders; capsules as `hull()` of two spheres; tapered brackets as `hull()` of extruded profiles; rounded 2D outlines as `hull()` of circles then `linear_extrude`). Essentially **none of those idioms work here**. The only idiom that does work is hull-of-boxes — which is exactly the idiom used by the one fixture that exercises it: `tests/fixtures/openappa/openappa.scad:117-124` and `openappa-multipart.scad:218-225` chamfer a cube by hulling three shortened boxes. The implementation was fitted to the corpus.

**Is `csg/hull.rs` more capable than the evaluator lets through? Yes, by a lot.**

* `csg::hull::convex_hull(&[Vec3]) -> Vec<Polygon>` (`csg/hull.rs:84`) is a full deterministic quickhull with coplanar-face merging, tested against cubes, spheres, random clouds and degenerate inputs (`csg/hull.rs:514-760`).
* `csg::hull::convex_hull_2d(&[[f64;2]])` (`csg/hull.rs:122`) is a full 2D hull.
* `csg::primitives::hull(&[Solid]) -> Solid` (`csg/primitives.rs:509`) — doc-commented *"The convex hull of a set of solids, as `hull()` defines it in 3D"* — collects every polygon vertex of every solid and calls `convex_hull`. **This is exactly OpenSCAD's `applyHull3D` semantics, already written.**
* `csg::planar::hull(&[Region2d]) -> Region2d` (`csg/planar.rs:111`) — doc-commented *"as `hull()` defines it in 2D"* — collects every contour vertex and calls `convex_hull_2d`. **This is exactly `applyHull2D`, already written.**

Neither `primitives::hull` nor `planar::hull` has a single caller outside `src/csg/` (verified by grep across `src/` and `tests/`). The evaluator instead re-implements a *separate, weaker* support-plane hull (`engine.rs:7270-7390`) restricted to box corners. Wiring the existing kernel functions — a `Shape::Hull { children }` variant lowered in `engine/exact.rs` alongside `Shape::Union`/`Shape::Intersection`, plus dropping the dimension check in favour of a 2D/3D split — would take `hull()` from "cubes only" to full parity in one change, with no new geometry code.

---

## 6. Deep dive: `text()`

Reference signature, verbatim from `$OSCAD/src/core/TextNode.cc:77`:

```
text(text = "", size = 10, font = "", direction = "ltr", language = "en",
     script = "latin", halign = "left", valign = "baseline", spacing = 1,
     em = 13.9 [, $fn])
```

with positional order `{text, size, font}` and named-only `{direction, language, script, halign, valign, spacing, em}` (`TextNode.cc:46-48`). Ours is `engine.rs:4085-4133`, drawing with the built-in single-stroke font in `engine/font.rs`.

| Parameter | Reference | Ours | Status |
|---|---|---|---|
| `text` | positional 0, string | positional 0, **stringifies non-strings** (`engine.rs:4091-4095`) | implemented (superset — ref would need `str()`) |
| `size` | positional 1, default 10 | positional 1, default 10 (`engine.rs:4100`) | **honoured** — probe `size=8` vs default changes the mesh |
| `font` | positional 2, FreeType face + style | **named-only**, accepted then warned (`engine.rs:4117-4129`) | **ignored.** Worse: given *positionally* (`text("Ag", 8, "Liberation Sans")`) it is **silently dropped with no warning** — the probe produced byte-identical geometry to `text("Ag", size=8)` and emitted nothing. |
| `halign` | `left` / `center` / `right` | `engine.rs:4504-4508` | **honoured.** Probe bboxes: left `[-0.19 … 11.83]`, center `[-6.29 … 5.73]`, right `[-12.39 … -0.38]`. Unknown values fall back to `left` silently. |
| `valign` | `baseline` / `top` / `center` / `bottom`, driven by the *font's* ascent/descent | `engine.rs:4509-4514`, driven by `font::CAP_HEIGHT = 0.70` and `font::DESCENDER = -0.20` | **honoured but numerically different** — the offsets come from this font's metrics, so a `valign="top"` label sits at a different y than OpenSCAD's. Unavoidable given a different face. |
| `spacing` | advance multiplier | `engine.rs:4105-4113` | **honoured** — probe `spacing=2` widens 11.83 → 18.23. |
| `direction` (`ltr`/`rtl`/`ttb`/`btt`) | HarfBuzz shaping direction | accepted, **warned, ignored** (`engine.rs:4117-4129`) | **ignored.** RTL and vertical text render left-to-right. |
| `language` | HarfBuzz language tag | accepted, **warned, ignored** | **ignored** |
| `script` | HarfBuzz script tag | accepted, **warned, ignored** | **ignored** |
| `em` | default 13.9, the em size that yields `size=10` (`TextNode.cc:74-75`) | **absent** | **missing and silent** — probe `text("Ag", em=13.9)` produced geometry byte-identical to `text("Ag")` with no diagnostic at all. |
| `$fn` | tessellates the glyph outlines | `fragment_count(size * 0.35, …)` (`engine.rs:4131`), and the pen nib is clamped to 8–16 sides (`engine.rs:4533`) | **honoured** — probe `$fn=6` → 252 tris, `$fn=64` → 1876 tris. |
| `$fa` / `$fs` | used when `$fn` is 0 | used, via `fragment_count` | implemented |
| Glyph coverage | any installed face, full Unicode | **printable ASCII only** (`engine/font.rs:108`); missing characters are dropped with `WARNING: text() has no glyph for …` (`engine.rs:4491-4499`) | **partial** — accented Latin, Greek, Cyrillic, CJK and most punctuation beyond ASCII simply vanish from the part. |

The letterforms themselves will never match OpenSCAD (the file says so at `engine/font.rs:12-20`, and that is the right call for a service with no font files). The actionable items are the three *silent* ones: positional `font` dropped without a warning, `em` accepted-and-ignored without a warning, and unknown `halign`/`valign` values falling back without a warning.

---

## 7. What to implement next, in priority order

Priority is by how often an arbitrary real-world `.scad` file would hit the gap, not by how much code it takes. Note a sampling caveat: `web/examples/` (12 files) is a **curated corpus authored against this engine** — all 12 compile here, and none of them uses `hull`, `minkowski`, `polyhedron`, `import`, `surface`, `resize`, `multmatrix`, `render`, `rotate_extrude`, `children` or `intersection_for`. It therefore proves nothing about coverage. `tests/fixtures/` (7 files) is closer to real work and does use `hull` (9 times, in 2 files) and `minkowski` (once) — and the `hull` uses are *all* the hull-of-boxes chamfer idiom that the current implementation was built around (`openappa.scad:117-124`, `openappa-multipart.scad:218-225`, `:364-410`). Both corpora are biased toward what already works, so the ranking below is driven by what OpenSCAD models in the wild actually contain.

1. **General `hull()` — 2D and 3D, any child.** The highest-frequency operator that is broken. Rounded slots (`hull()` of two cylinders), capsules (two spheres), tapered brackets (two extrusions), rounded outlines (circles + `linear_extrude`) are all dead. Every one of the probes above that fails is a one-line SCAD idiom. **The kernel is already written and tested** — `csg::primitives::hull` (`csg/primitives.rs:509`) and `csg::planar::hull` (`csg/planar.rs:111`) — so this is a `Shape` variant plus a lowering arm in `engine/exact.rs`, not new geometry.

2. **`render()` and `group()` as aliases for `union()`.** Two arms, ~6 lines, and they turn a whole-file hard error into a no-op. `render()` is ubiquitous in models written for the CGAL backend; `group()` shows up in machine-generated SCAD. Nothing about the exact kernel makes them meaningful, which is exactly why they should be free.

3. **Stop hard-erroring on unknown modules and on soft failures generally.** OpenSCAD's contract is "warn and carry on"; ours is "abort the compile". Today a single `assign()`, `render()`, `surface()`, an out-of-range `children(-1)`, a 2D child inside a 3D `union()`, or a computed range step that momentarily hits 0 destroys the entire model instead of one object. Changing `engine.rs:4346` and the sites listed in §1/§4 to diagnostics would convert a large class of "your file doesn't work" into "one object is missing" — which is both what users expect and much better for the MCP/AI feedback loop, since a partial render plus a warning is far more actionable than a single error string.

4. **`polyhedron()`.** The escape hatch every non-trivial library uses for lofts, custom fillets and imported point data. `csg::primitives::polyhedron` (`csg/primitives.rs:217`) is written and tested (`csg/primitives.rs:739`, `:761`) with no caller. Same wiring-only story as hull.

5. **`projection(cut)`.** Common for generating 2D outlines, base plates and laser-cut faces from 3D bodies. `csg::planar::projection_cut` (`csg/planar.rs:126`) and `projection_shadow` (`csg/planar.rs:152`) exist, tested (`csg/planar.rs:231`), uncalled.

6. **Raise `MAX_MODULE_RECURSION_DEPTH` from 16.** `engine.rs:3109`. Recursive modules (fractals, trees, tiling, "repeat N times" helpers) are routine, and 16 is far below any plausible model. OpenSCAD's limit is the 8 MB stack, not a counter (`UserModule.cc:94-98`). Even 256 would remove nearly all real failures; the comment at `engine.rs:3107` explains the constraint (per-frame environment clones) — if the frames are the problem, that is the thing to fix, not the depth.

7. **The `-2^2` precedence bug.** `engine.rs:2185-2199` vs `parser.y:467-500`. Silently produces a *different number* with no diagnostic, which is the worst failure mode there is. One-line fix: parse unary's operand at the exponent level, or give `^` a higher binding power than unary.

8. **`str()` / `echo` number formatting to 6 significant digits.** `Value.cc:76` (`DC_PRECISION_REQUESTED = 6`). Affects `text(str(x))` geometry on any dimension label, and makes every echoed diagnostic differ from OpenSCAD's, which undermines side-by-side debugging.

9. **`scale([x,y])` padding with 1.0, and `multmatrix()`.** `scale([1,2])` currently kills the model (`engine.rs:3917`) — pad like `TransformNode.cc:60`. `multmatrix` is a small arm over the existing 4×4 `Transform` and is the canonical way to express shears and custom projections.

10. **String iteration (`for(c = "abc")`, `each "ab"`) and range indexing (`r[0]`).** `engine.rs:3500`, `4959`, `4795`. Cheap; string iteration is a hard error today, which is worse than a wrong value.

11. **Vector ordering comparison and mismatched-length vector arithmetic.** `engine.rs:5469`, `5320`. SCAD sorting/search libraries rely on lexicographic vector `<`; returning `false` mis-sorts silently.

12. **Hex literals and bitwise/shift operators.** `engine.rs:1581`, `1528`. Both currently fail at *lex/parse* time, so the whole file is rejected — high blast radius, low frequency. Hex shows up in colour and bit-flag constants; `&`/`|`/`<<` in bitmask-style parameter encoding.

13. **`$vpr` / `$vpt` / `$vpd` / `$vpf`.** Absent entirely (§3). Models that orient cutaways or labels toward the camera degrade to `undef` silently. This backend knows its render camera, so plausible values are available.

14. **`linear_extrude(v=…)` and `segments=`; `rotate_extrude(start=…)`.** Newer parameters (`LinearExtrudeNode.cc:48-50`, `RotateExtrudeNode.cc:51`) that are currently accepted-and-ignored — again silent wrong geometry rather than a diagnostic. At minimum, warn.

15. **`text()`: warn on positional `font`, on `em`, and on unrecognised `halign`/`valign`.** §6. Cheap honesty; the letterforms can never match anyway, but silence about a dropped `font` argument is avoidable.

16. **`parent_module()`, `fill()`, 2D `minkowski()`, `import()`, `surface()`, `roof()`, `dxf_dim`/`dxf_cross`.** Long tail. `fill()` and 2D `minkowski()` are real 2D gaps (`GeometryEvaluator.cc:271`, `:421`); `import`/`surface`/`dxf_*` need a file source this service deliberately does not have and should degrade to warnings rather than be implemented; `roof` is disabled by default in the reference binary too.

Not worth doing: `assign()` (removed upstream), `textmetrics`/`fontmetrics`/`is_object`/`object`/`has_key` (experimental and off by default in the reference binary), `$fe` (same), matching `rands()`'s PRNG stream (would require copying the reference implementation — document the difference instead).
