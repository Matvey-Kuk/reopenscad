# OpenSCAD Language Specification

Target conformance: **OpenSCAD 2021.01** (latest stable release). Features only
available in newer development snapshots are marked **[dev]**; features tied to
an earlier release are marked with the release that introduced them (e.g.
**[2019.05]**). Deprecated constructs are collected in §19 and must still be
accepted (with a warning) for compatibility.

Sources: openscad.org cheat sheet (v2021.01), OpenSCAD User Manual (wikibooks),
OpenSCAD source semantics.

---

## 1. Execution model

- A program is a sequence of statements. Statements either bind values
  (assignments, `module`/`function` definitions) or **instantiate geometry**
  (module instantiations such as `cube(5);`).
- Evaluation is **declarative and compile-time**: the whole tree is evaluated
  once; there is no mutable state. Variables are set at compile time, not run
  time.
- All geometry instantiated at the top level is implicitly unioned into the
  final model.
- Two evaluation products: **preview** (F5, fast CSG preview) and **render**
  (F6, full mesh evaluation). `$preview` distinguishes them.
- Numbers that are the result of errors (division by zero etc.) follow IEEE 754
  (`inf`, `nan`); they do not abort evaluation, but geometry using them is
  ignored with a warning.

## 2. Lexical structure

- **Whitespace** is insignificant except as a token separator.
- **Comments**: `// line comment` and `/* block comment */` (block comments do
  not nest).
- **Identifiers**: `[A-Za-z0-9_]+` (may start with a digit-free character in
  practice; leading `$` denotes a *special variable*, §14).
- **Numbers**: IEEE 754 double literals: `42`, `3.5`, `.5`, `5.`, `1e3`,
  `1.5e-3`. No integer type — all numbers are 64-bit floats.
- **Strings**: double-quoted, Unicode, with escapes `\"` `\\` `\t` `\n` `\r`
  `\x??` (hex byte) `\u????` `\U??????` (Unicode code points).
- **Booleans**: `true`, `false`.
- **Undefined**: `undef`.
- **Constants**: `PI` (3.141592653589793).

## 3. Values and types

| Type | Literal | Notes |
|---|---|---|
| number | `1.5` | IEEE 754 double; `inf`/`nan` possible |
| boolean | `true` / `false` | |
| string | `"abc"` | indexable: `s[0]` → `"a"`; `len(s)` |
| vector (list) | `[1, "a", [2,3]]` | heterogeneous, nestable |
| range | `[start:end]`, `[start:step:end]` | colon-separated, in brackets |
| undef | `undef` | value of unset variables, bad indexes, type errors |
| function value | `function (x) x+1` | **[2021.01]** first-class lambdas |
| object | `object(a=1)` | **[dev]** only — key/value collection; not in 2021.01 |

- **Vectors**: 0-indexed with `v[i]`; nested indexing `v[i][j]`; out-of-range
  or non-integer index → `undef`. Dot notation `v.x`, `v.y`, `v.z` aliases
  indices 0, 1, 2.
- **Ranges**: `[0:5]` iterates 0,1,2,3,4,5 (inclusive ends, default step 1);
  `[0:2:9]` → 0,2,4,6,8. A negative step counts down. If `begin > end` with a
  positive step, the range is empty (historic versions auto-swapped with a
  deprecation warning — accept, warn, treat as empty). Step 0 is an error when
  iterated. Ranges are values: they can be stored and passed around.
- **Truthiness** (for `if`, `? :`, `&&`, `||`, `!`): the false values are
  `false`, `0`, `-0`, `""` (empty string), `[]` (empty vector), and `undef`.
  Everything else is true (note: `nan != 0`, so `nan` is truthy).

## 4. Variables, assignment, scope

```scad
name = expression;
```

- **Last assignment wins**: within one scope, if a variable is assigned more
  than once, *every* use sees only the final value (even uses textually before
  the assignment). Reassigning in the same scope emits a warning. Exceptions
  (no warning, intentional override): values re-assigned by `include`d files
  and `-D name=value` command-line definitions.
- **Scopes**: every brace pair `{}` and every `for`/`let`/module body creates a
  nested lexical scope. Inner scopes read outer variables; an assignment in an
  inner scope creates a *new* variable shadowing the outer one — it never
  mutates the outer scope.
- An unset variable reads as `undef`.
- **Special variables** (`$`-prefixed, §14) use *dynamic* scoping: they
  propagate down the instantiation call chain, and can be set per-call:
  `sphere(5, $fn=64);` or `mymod($fn=32);`.

## 5. Operators

Precedence, highest to lowest (higher binds tighter):

| # | Operators | Notes |
|---|---|---|
| 1 | `f(...)`, `v[i]`, `v.x` | call, index, member |
| 2 | `!` `+` `-` (unary) | |
| 3 | `^` | exponent **[2021.01]**, right-associative |
| 4 | `*` `/` `%` | `%` is fmod (sign of dividend) |
| 5 | `+` `-` | |
| 6 | `<` `<=` `>=` `>` | |
| 7 | `==` `!=` | |
| 8 | `&&` | short-circuits |
| 9 | `\|\|` | short-circuits |
| 10 | `?:` | ternary conditional, right-associative |

Expression-wrapping keywords (lowest precedence): `let (...) expr`,
`assert(...) expr`, `echo(...) expr`, `function (...) expr`.

**Arithmetic on non-numbers:**

- `vector + vector`, `vector - vector`: element-wise; operands of unequal
  length or non-numeric elements → `undef` for the whole result (warn).
- `number * vector`, `vector * number`, `vector / number`: element-wise scale.
- `vector * vector` (same length): **dot product** (number).
- `matrix * vector`, `vector * matrix`, `matrix * matrix`: linear-algebra
  products (matrix = vector of equal-length vectors).
- Unary `-` on a vector negates element-wise. Any unsupported combination
  yields `undef` with a warning, never an abort.

**Comparisons:**

- `==`/`!=` work on all types; vectors compare deep element-wise; values of
  different types are simply not equal (`true != 1`, `"1" != 1`). `nan`
  compares unequal to everything including itself. `undef == undef` is true.
- `<` `<=` `>` `>=` are defined for number–number, string–string
  (lexicographic), bool–bool, and mixed bool/number (bool coerced to 0/1).
  Other combinations return `false` and warn.

**Logical:** `&&`, `||` short-circuit and return a **boolean** (not the
operand value). `!x` returns boolean.

## 6. Conditional and iteration statements (geometry context)

```scad
if (cond) { ... }
if (cond) { ... } else { ... }
if (a) { } else if (b) { } else { }

for (i = [0:10])        { ... }        // range
for (i = [0:0.5:10])    { ... }
for (x = [1, 2, "a"])   { ... }        // over vector elements
for (x = v, y = w)      { ... }        // multiple assignments = nested loops
for (o = obj)           { ... }        // [dev] over object member names

intersection_for (i = ...) { ... }     // like for, but intersects the results
                                       // instead of unioning them

let (a = 1, b = a + 1) { ... }         // [2019.05] scoped bindings; later
                                       // bindings see earlier ones
```

- A `for` loop unions the geometry of all iterations. Bodies with one
  statement may omit braces.
- `intersection_for` exists because `intersection() for(...)` would intersect
  the single unioned child; `intersection_for` intersects per-iteration.
- These are *statements* (geometry context). In *expression* context use the
  ternary operator and list comprehensions instead.

## 7. Modules (user-defined)

```scad
module name(param1, param2 = default, ...) { body }
name(arg1, param2 = arg2);              // instantiation
```

- Parameters may have default expressions (evaluated in the caller's
  environment at call time). Arguments match positionally, then by name; named
  arguments may appear in any order. Missing parameter without default →
  `undef`.
- Modules may be defined anywhere (including nested inside other modules,
  visible in that scope) and instantiated before their textual definition.
- **Recursion is allowed** (guard with `if`); implementations should cap
  recursion depth with a clear error.
- **Operator modules** (modules that transform their children):

```scad
mymod() cube(5);                 // one child
mymod() { cube(5); sphere(3); }  // child list

module mymod() {
  children();          // instantiate all children
  children(0);         // first child (0-indexed)
  children([0:2]);     // range of children
  children([0, 2]);    // vector of indices
}
```

- `$children` — number of children passed to the current module.
- Index out of range is a runtime error (implementations commonly warn +
  ignore).
- A module call terminated by `;` has zero children.
- Modifier characters (§15) may prefix any instantiation.

## 8. Functions (user-defined)

```scad
function name(params) = expression;
r = name(args);
```

- Same parameter/default/named-argument rules as modules. Body is a single
  expression; use `? :`, `let`, list comprehensions and recursion for logic.
- Recursion allowed; implementations impose a stack limit (~thousands of
  frames) with tail-call optimization for tail-recursive functions
  (recommended for parity: OpenSCAD does TCO).
- **Function literals** **[2021.01]**:

```scad
f = function (x) x * x;
echo(f(5));                       // 25
g = function (x) function (y) x + y;   // closures capture their environment
```

## 9. Expression-level constructs

```scad
x = cond ? a : b;                          // ternary
y = let (a = 1, b = 2) a + b;              // let-expression
z = assert(v > 0, "message") v * 2;        // [2019.05] assert then yield expr
w = echo("dbg", v) v * 2;                  // [2019.05] echo then yield expr
```

**List comprehensions** (expression context, produce vectors):

```scad
[ for (i = [0:9]) i * i ]                  // generate
[ for (i = list) if (test(i)) i ]          // filter
[ for (i = list) if (test(i)) f(i) else g(i) ]   // [2019.05] if/else
[ for (i = list) let (a = f(i)) a + a ]    // bindings inside
[ for (i = init; cond; next) i ]           // [2019.05] C-style generator
[ each v ]                                 // [2019.05] splice: flatten one level
[ for (i = v) each f(i) ]                  // flatten inside a for
[ a, for (i = v) i, b ]                    // mixed literal + generator parts
[ for (i = [0:3]) [i, i*i] ]               // nested results stay nested
```

- `each x` splices the elements of vector/range `x` into the enclosing list;
  `each` on a non-list yields the value itself.
- Generators may nest arbitrarily: `for` inside `for`, `if` inside `let`, etc.
- A comprehension yielding nothing produces `[]`.

## 10. 2D primitives

All 2D shapes lie in the XY plane. 2D and 3D geometry cannot be mixed in one
boolean/hull operation (implementations must error or warn).

```scad
square(size);                       // size number → size×size, corner at origin
square([w, h], center = false);
circle(r);                          // resolution via $fa/$fs/$fn (§14)
circle(d = diameter);
polygon(points);                    // points = [[x,y], ...], implicit closed path
polygon(points, paths, convexity);  // paths: index lists; 2nd+ paths cut holes
text(text, size = 10, font = "Liberation Sans",
     halign = "left",               // "left" | "center" | "right"
     valign = "baseline",           // "top" | "center" | "baseline" | "bottom"
     spacing = 1,                   // letter-spacing multiplier
     direction = "ltr",             // "ltr" | "rtl" | "ttb" | "btt"
     language = "en", script = "latin");   // [2015.03]; glyph curves use $fn/$fa/$fs
import("file.dxf", convexity);      // also SVG [2019.05]; layer= for DXF,
                                    // center=, dpi= for SVG [2019.05]
projection(cut = false) { 3D... }   // 3D→2D: shadow (cut=false) or z=0 slice (cut=true)
```

## 11. 3D primitives

```scad
cube(size, center = false);            // size number or [w, d, h]
sphere(r);  sphere(d = diameter);
cylinder(h, r, center = false);        // uniform radius
cylinder(h, r1, r2, center = false);   // cone/frustum; r1 or r2 may be 0
cylinder(h, d = ..., d1 = ..., d2 = ...);
                                       // center=false: base at z=0; center=true:
                                       // spans -h/2..h/2. Radial resolution $fa/$fs/$fn
polyhedron(points, faces, convexity = 1);
   // points: [[x,y,z], ...]
   // faces:  [[i0, i1, i2, ...], ...] each face's vertices listed CLOCKWISE
   //         when viewed from outside; faces may have 3+ coplanar vertices
import("file.stl", convexity);         // STL, OFF, AMF [2015.03], 3MF [2019.05]
surface(file, center = false, invert = false, convexity = 1);
   // heightmap: text file (rows of space/tab-separated heights, rows = Y,
   // cols = X) or PNG [2015.03] (grayscale Y = 0.2126R+0.7152G+0.0722B,
   // mapped to height 0..100; invert flips PNG heights)
```

## 12. Extrusions (2D → 3D)

```scad
linear_extrude(height, center = false, convexity = 10,
               twist = 0,        // degrees of rotation over the height (left-hand)
               slices = ...,     // subdivisions along the axis (auto from twist/$fn)
               scale = 1,        // number or [sx, sy] applied at the top
               segments = ...,   // [dev] extra vertices around the outline
               v = [0,0,1])      // [dev] extrusion direction vector
    { 2D children }

rotate_extrude(angle = 360,      // [2019.05] partial sweep; negative = clockwise
               start = 0,        // [dev] start angle from +X
               convexity = 2)
    { 2D children }
   // Sweeps the child 2D profile around the Z axis. The profile must lie
   // entirely on one side of the Y axis (all x >= 0 or all x <= 0).
   // Sweep resolution follows $fa/$fs/$fn.
```

## 13. Transformations and CSG operations

Transformations apply to all children (implicit union of the child list).

```scad
translate([x, y, z]) ...
rotate([ax, ay, az]) ...        // rotate about X, then Y, then Z (degrees)
rotate(a) ...                   // scalar: rotate a° about Z
rotate(a, [x, y, z]) ...        // a° about arbitrary axis vector
scale([x, y, z]) ...            // or scale(s) uniform
resize([x, y, z], auto = false, convexity) ...
   // scale to absolute size; a 0 component = leave that axis unchanged,
   // unless auto (bool or [bx,by,bz]) says to keep proportions
mirror([x, y, z]) ...           // reflect across plane through origin with this normal
multmatrix(m) ...               // 4×4 (or 3×4) affine matrix applied to children
color("name", alpha = 1.0) ...  // CSS color names
color("#rgb" | "#rgba" | "#rrggbb" | "#rrggbbaa") ...   // [2019.05]
color([r, g, b], alpha) ...     // or [r,g,b,a], components 0..1
   // color affects PREVIEW only; F6 render/export output is uncolored
offset(r = 1) ...               // 2D only [2015.03]: round-cornered grow/shrink
offset(delta = 1, chamfer = false) ...  // straight-edged; chamfer cuts corners
hull() { ... }                  // convex hull of children (all-2D or all-3D)
minkowski(convexity) { ... }    // Minkowski sum of children

union()        { ... }          // merge all children
difference()   { ... }          // first child minus all remaining children
intersection() { ... }          // common volume of all children
render(convexity = 1) { ... }   // force full mesh evaluation even in preview
```

## 14. Special variables (dynamic scope)

| Variable | Meaning | Default |
|---|---|---|
| `$fn` | if > 0: exact fragment count for arcs (overrides `$fa`/`$fs`) | 0 |
| `$fa` | minimum fragment angle, degrees (floor 0.01) | 12 |
| `$fs` | minimum fragment length, units (floor 0.01) | 2 |
| `$t` | animation time in [0, 1) | 0 |
| `$vpr` | viewport rotation [x,y,z]° (writable at top level) | — |
| `$vpt` | viewport translation (writable at top level) | — |
| `$vpd` | viewport camera distance **[2015.03]** | — |
| `$vpf` | viewport FOV **[2021.01]** | — |
| `$children` | number of children of the current module | — |
| `$preview` | `true` in F5 preview, `false` in F6 render **[2019.05]** | — |
| `$parent_modules` | depth of the module instantiation stack | — |

**Fragment count formula** (number of segments for a circle of radius r) —
required for exact parity of circles, spheres, cylinders, arcs:

```
if (r < 1e-6)      fragments = 3;
else if ($fn > 0)  fragments = max($fn, 3);
else               fragments = ceil(max(min(360 / $fa, r * 2 * PI / $fs), 5));
```

User-defined `$`-variables are allowed (`$my_flag = true;`) and propagate
dynamically like the built-ins. Setting any special variable in a call's
argument list (`sphere(1, $fn=64)`) scopes it to that subtree.

## 15. Modifier characters

Single-character prefixes on a module instantiation:

| Char | Name | Effect |
|---|---|---|
| `*` | disable | subtree ignored entirely |
| `!` | show only / root | render only this subtree (parent transforms above it still apply-ignored: subtree becomes the root); overrides everything else |
| `#` | highlight / debug | subtree rendered normally **and** shown highlighted (transparent red) in preview; still part of the geometry |
| `%` | background / transparent | subtree shown transparent gray in preview but **excluded** from the actual geometry/CSG result and from F6 render |

Modifiers may be combined (e.g. `%*`), and only affect preview display except
`*` and `!`, which affect the produced geometry tree.

## 16. Built-in functions

Trigonometry works in **degrees**.

**Math**: `abs(x)`, `sign(x)` (→ -1/0/1), `floor(x)`, `ceil(x)`, `round(x)`
(half away from zero), `sqrt(x)` (negative → nan), `pow(b, e)`, `exp(x)`,
`ln(x)`, `log(x)` (base 10), `min(a, b, ...)` / `min(vector)`,
`max(a, b, ...)` / `max(vector)`, `norm(v)` (Euclidean length; also works on
numbers-only vectors of any length), `cross(a, b)` (3D vector cross product;
for 2D vectors returns the scalar z-component).

**Trig**: `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2(y, x)`.

**Rounding/other**: `rands(min, max, count, seed)` — vector of `count` uniform
random numbers; `seed` optional (omit = nondeterministic).

**String**: `str(a, b, ...)` — convert and concatenate to one string;
`chr(code | vector | range)` — code point(s) → string; `ord(c)` **[2019.05]**
— first char → code point; `len(s)`; indexing `s[i]` (single-character string,
out of range → undef).

**List/vector**: `len(v)`; `concat(v1, v2, ...)` **[2015.03]** — concatenates
lists (non-list arguments are appended as elements); indexing and `.x/.y/.z`.

**Search/lookup**:
- `lookup(key, [[k0, v0], [k1, v1], ...])` — linear interpolation between the
  two nearest keys; clamps outside the table's key range.
- `search(match, target, num_returns_per_match = 1, index_col_num = 0)` —
  find occurrences in a string, vector, or vector-of-vectors. When `match` is
  a string, each character is searched separately (vector of index-vectors);
  `num_returns_per_match = 0` returns all matches; when searching a table,
  `index_col_num` selects which column to compare.

**Type tests** **[2019.05]**: `is_undef(x)`, `is_bool(x)`, `is_num(x)`
(false for nan), `is_string(x)`, `is_list(x)`, `is_function(x)`
**[2021.01]**.

**Meta**: `version()` → e.g. `[2021, 1, 0]`; `version_num()` → e.g.
`20210100`; `parent_module(i)` → name of the module `i` levels up the
instantiation stack (`parent_module(0)` is the current module).

**Statement-form built-ins**: `echo(a, b, name = value, ...);` — prints
`ECHO: ...` to the console; numbers shown with 5 significant digits; also
usable as an expression (§9). `assert(cond)` / `assert(cond, message)`
**[2019.05]** — abort compilation with an error when `cond` is false; usable
as statement and as expression prefix.

## 17. Include / use

```scad
include <filename.scad>   // textual inclusion: definitions AND top-level
                          // geometry/assignments of the file are executed;
                          // the including file may override included variables
use <filename.scad>       // imports module and function definitions ONLY;
                          // the used file's top-level geometry is NOT executed.
                          // Must appear at top level (not inside blocks).
```

Search paths: relative to the current file, then the library path(s). `<>` is
the literal syntax (not optional). A web implementation should map this to its
own virtual file system / project storage.

## 18. Customizer parameter annotations

Structured comments turn top-level assignments into UI controls (needed for
feature parity with the desktop Customizer panel):

```scad
/* [Section name] */          // groups following parameters into a tab
/* [Hidden] */                // parameters after this are not shown

diameter = 30;                // plain description comment shown as tooltip
// The wall thickness
wall = 2;         // [1:5]            → slider 1..5 (step 1)
res   = 0.5;      // [0.1:0.1:2]      → slider with step
kind  = "arm";    // [arm, leg, head] → dropdown
label = "hello";  // 10               → max length 10 text box
flag  = true;                         // checkbox
vec   = [1, 2, 3]; // [0:10]          → per-component sliders
part  = 2;        // [1:Small, 2:Medium, 3:Large]  → labeled dropdown
```

Only top-level assignments *before* the first module/function definition or
geometry statement participate. Values set by the customizer override the
source defaults at evaluation time.

## 19. Deprecated / legacy constructs (accept + warn)

| Construct | Replacement |
|---|---|
| `assign (a = 1) { ... }` | plain assignment / `let` |
| `child(i)` | `children(i)` |
| `polyhedron(points, triangles = ...)` | `faces =` parameter |
| `import_stl()`, `import_dxf()`, `import_off()` | `import()` |
| `dxf_linear_extrude()`, `dxf_rotate_extrude()` | `linear_extrude()` + `import()` |
| `dxf_cross()`, `dxf_dim()` | no direct replacement (DXF probing) |
| Range `[begin:end]` with `begin > end` | empty range (older versions swapped) |

## 20. Error and warning behavior

- Type errors in expressions generally produce `undef` plus a console
  **WARNING**, not a hard stop (e.g. `"a" + 1` → undef).
- Hard **ERRORS** stop compilation: syntax errors, `assert()` failures,
  recursion depth exceeded.
- Geometry-level problems (self-intersecting polyhedra, zero-size primitives,
  mixing 2D and 3D in one boolean) produce warnings and the offending subtree
  is dropped or the operation errors, matching desktop behavior.
- `echo`/warnings/errors go to a console log the UI must display.

## 21. Grammar sketch (informative EBNF)

```
program        = { statement } ;
statement      = ";"
               | "{" { statement } "}"
               | assignment
               | module_def | function_def
               | include | use
               | module_instantiation ;

assignment     = identifier "=" expr ";" ;
module_def     = "module" identifier "(" [ parameters ] ")" statement ;
function_def   = "function" identifier "(" [ parameters ] ")" "=" expr ";" ;
parameters     = parameter { "," parameter } ;
parameter      = identifier [ "=" expr ] ;

module_instantiation
               = { modifier } ident_or_ctrl "(" [ call_args ] ")" child ;
modifier       = "!" | "#" | "%" | "*" ;
ident_or_ctrl  = identifier | "if" ... | "for" ... | "let" ...
                 | "intersection_for" ... ;      (* control forms have their
                                                    own header syntax, §6 *)
child          = ";" | statement | "{" { statement } "}"
               | [ "else" child ] ;              (* for if *)
call_args      = call_arg { "," call_arg } ;
call_arg       = [ identifier "=" ] expr ;

expr           = "function" "(" [ parameters ] ")" expr
               | "let"    "(" [ call_args ] ")" expr
               | "assert" "(" [ call_args ] ")" [ expr ]
               | "echo"   "(" [ call_args ] ")" [ expr ]
               | ternary ;
ternary        = or_expr [ "?" expr ":" expr ] ;
or_expr        = and_expr { "||" and_expr } ;
and_expr       = eq_expr { "&&" eq_expr } ;
eq_expr        = rel_expr { ("==" | "!=") rel_expr } ;
rel_expr       = add_expr { ("<" | "<=" | ">" | ">=") add_expr } ;
add_expr       = mul_expr { ("+" | "-") mul_expr } ;
mul_expr       = pow_expr { ("*" | "/" | "%") pow_expr } ;
pow_expr       = unary { "^" unary } ;                    (* right-assoc *)
unary          = [ "!" | "+" | "-" ] postfix ;
postfix        = primary { "(" [ call_args ] ")" | "[" expr "]" | "." identifier } ;
primary        = number | string | "true" | "false" | "undef"
               | identifier | "(" expr ")"
               | "[" expr ":" expr [ ":" expr ] "]"        (* range *)
               | "[" [ lc_or_exprs ] "]" ;                 (* vector / comprehension *)
lc_or_exprs    = lc_element { "," lc_element } ;
lc_element     = expr | "each" expr
               | "for" "(" (call_args | c_style_for) ")" lc_element
               | "if" "(" expr ")" lc_element [ "else" lc_element ]
               | "let" "(" call_args ")" lc_element ;
c_style_for    = call_args ";" expr ";" call_args ;
```

## 22. Parity checklist for the implementation

Minimum bar for "exact feature parity" with desktop OpenSCAD 2021.01:

1. Full lexer/parser per §2 and §21, including modifiers, comprehensions,
   function literals, `let`/`each`, ternary, `^`.
2. Value semantics of §3–§5 exactly (truthiness set, undef propagation,
   vector/matrix arithmetic, string comparison, 5-digit echo formatting).
3. All primitives, transforms, extrusions and CSG ops of §10–§13, with the
   fragment formula of §14 governing every curved surface.
4. Dynamic scoping of `$` variables, `$preview`, `$t` animation hook.
5. `children()` in all four call forms, `$children`, recursion with TCO.
6. `include`/`use` against the project's virtual file system.
7. Console with ECHO/WARNING/ERROR streams; `assert` aborts.
8. Modifier characters affecting both preview display and geometry per §15.
9. Customizer annotations (§18) driving the variable-controls UI.
10. Deprecated forms of §19 accepted with warnings.
