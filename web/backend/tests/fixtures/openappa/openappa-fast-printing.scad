// OpenAPPA mascot, fast-printing — the silhouette, one solid, chamfered on
// every edge it really has.
//
// The mascot is not invented here. It is the `BEAST` bitmap that draws the
// mark on screen (website/components/Logo.tsx and components/pixel-marks.ts):
// a 24-wide by 22-tall grid, horns on rows 0-1, head on rows 2-13, body on
// rows 14-19, legs and paws on rows 20-21. Where the bitmap shows the
// background through the beast — the two eyes and the nostril pair — there is
// no material, so those read as holes exactly as they do on screen.
//
// This is the print-in-an-evening companion to `openappa.scad`. Same
// bitmap, same size, same thickness; three things taken out, each of them a
// print-time cost rather than part of the mascot:
//
//   1. No per-cell chamfers. In the full model every cell is a separately
//      chamfered block, so the surface is corrugated and each layer's
//      perimeter weaves in and out 24 times across. Without them, neighbouring
//      cells fuse into flat faces and the slicer walks long straight lines.
//      The trade is real: the pixel grid stops reading on the face, and only
//      the silhouette — horns, ears, eyes, mouth, legs — carries the mascot.
//      The chamfer survives everywhere the beast has a real edge: the corners
//      of the outline, the corners of the eyes and the mouth, and the rims
//      where the front and back faces meet the walls. Those cost a handful of
//      short moves and keep it from looking cut out with scissors; a chamfer
//      between two cells of the same limb would cost a groove per column.
//   2. No two-colour split. One body, one filament, one print, no registration
//      between parts and no filament change.
//   3. Laid down, not standing. Face up on its flat back: nine millimetres of
//      layers instead of fifty, and every opening in the bitmap is a
//      through-hole in a flat part rather than something to bridge, so there
//      is nothing to support.
//
// With no per-cell chamfer and no colour boundary the model is what it looks
// like — the lit cells of the bitmap, unioned into one region, cornered, and
// extruded. That is why this file is short where the full model is long: there
// are no blocks to hold apart, so none of its block, core and material
// machinery is needed.
//
// This file lives beside the mesh it produces, in website/public/brand, so
// that the source and everything exported from it ship together. Run the
// commands below from the repository root; the name they write is the name the
// site serves.
//
// Preview:
//   openscad website/public/brand/openappa-fast-printing.scad
//
// Export:
//   openscad --export-format binstl -o website/public/brand/openappa-fast-printing.stl website/public/brand/openappa-fast-printing.scad

/* ------------------------------------------------------------ parameters --- */

mascot_height = 50;   // mm, paws to horn tips; everything else follows

pixel_depth   = 4;    // cells front to back — the same thickness as the full
                      // model. Printed lying down, this costs layers and
                      // nothing else, so it is cheap to keep.

lay_flat      = true; // deliver the mascot face-up on the bed rather than
                      // standing on its paws. Off leaves the full model's
                      // orientation, for comparing the two side by side.

chamfer       = 0.3;  // chamfer, as a fraction of a cell, cut back the same
                      // distance on every edge the beast actually has: the
                      // corners of the outline, the corners of the eyes and
                      // the mouth, and the rims where the front and back faces
                      // meet the walls. At the default that is 0.68 mm, and
                      // all four measure it. What it is never taken off is a
                      // boundary between two cells — that is the corrugation
                      // this variant exists to avoid. 0 leaves every edge
                      // square. Half a cell is the ceiling: at that point two
                      // cuts meet across the mouth, which is one cell tall.

body_color    = "#202124";

/* ------------------------------------------------------------------ grid --- */

// Original BEAST palette: `1` primary body, `3` dim muzzle/paws, `4` eyes,
// `2` nose, and `.` outside background. Here `1` and `3` are one material.
BEAST = [
    ".....11..........11.....",
    ".....11..........11.....",
    "....1111111111111111....",
    "...111111111111111111...",
    "...111111111111111111...",
    "...111111111111111111...",
    "...111444111111444111...",
    "...111444111111444111...",
    "...111444111111444111...",
    "...111111111111111111...",
    "...111111133331111111...",
    "...111111132231111111...",
    "...111111111111111111...",
    "....1111111111111111....",
    ".1111111111111111111111.",
    "111111111111111111111111",
    "111111111111111111111111",
    "111111111111111111111111",
    "111111111111111111111111",
    "111111111111111111111111",
    "11111..1111..1111..11111",
    "33333..3333..3333..33333",
];

GRID_COLS = 24;
GRID_ROWS = 22;

c  = mascot_height / GRID_ROWS;  // one cell, in mm
ch = chamfer * c;                // chamfer, in mm

// Every cut takes `ch` off a corner, and the smallest thing there is to cut is
// one cell — the mouth's height, the width of a horn. Past half a cell the
// cuts from either end meet and the feature is gone rather than chamfered.
assert(chamfer >= 0 && chamfer < 0.5,
       "chamfer must be at least 0 and less than 0.5 of a cell");

// Is there material at this bitmap position? Off-grid reads as empty, so the
// silhouette needs no special casing.
function lit(row, col) =
    row >= 0 && row < GRID_ROWS && col >= 0 && col < GRID_COLS
    && (BEAST[row][col] == "1" || BEAST[row][col] == "3");

/* ----------------------------------------------------------------- parts --- */

// The lit cells as one 2D region. Squares that share an edge union into a
// single polygon, so what comes out is the outline of the beast and the
// outlines of its holes — not 378 separate squares. The bitmap counts rows
// downward from the horns; the model counts up from the paws.
module silhouette() {
    union()
        for (row = [0 : GRID_ROWS - 1], col = [0 : GRID_COLS - 1])
            if (lit(row, col))
                translate([(col + 0.5 - GRID_COLS / 2) * c,
                           (GRID_ROWS - row - 0.5) * c])
                    square([c, c], center = true);
}

// One right triangle per corner of the eyes and the mouth, to be cut away.
//
// A hole's corner is concave for the body, and the sweep below only chamfers
// convex ones, so these have to be taken out by hand. Doing it as an explicit
// cut rather than with a pair of offsets is what keeps `chamfer` free: an
// offset pass has to grow the region before it shrinks it, and growing by half
// a cell shuts the mouth — which is one cell tall — before it can be chamfered
// at all. Nothing here grows anything.
//
// A grid vertex with three lit cells around it is a hole's corner, and the
// empty one says which way the hole opens. The wedge comes off the opposite
// side: legs of `ch` along both edges, hypotenuse across the corner.
module hole_wedges() {
    for (row = [0 : GRID_ROWS], col = [0 : GRID_COLS]) {
        nw = lit(row - 1, col - 1) ? 1 : 0;
        ne = lit(row - 1, col)     ? 1 : 0;
        sw = lit(row, col - 1)     ? 1 : 0;
        se = lit(row, col)         ? 1 : 0;

        if (nw + ne + sw + se == 3) {
            // The empty quadrant is where the hole is; cut back the other way.
            dx = (ne == 0 || se == 0) ? -1 : 1;
            dy = (ne == 0 || nw == 0) ? -1 : 1;
            x  = (col - GRID_COLS / 2) * c;
            y  = (GRID_ROWS - row) * c;

            translate([x, y])
                polygon([[0, 0], [dx * ch, 0], [0, dy * ch]]);
        }
    }
}

// A square bipyramid: an eight-sided tool whose faces all sit at 45 degrees.
// Its equator is a diamond, its apexes are `s` above and below.
module chamfer_tool(s) {
    union() {
        cylinder(r1 = s, r2 = 0, h = s, $fn = 4);
        mirror([0, 0, 1]) cylinder(r1 = s, r2 = 0, h = s, $fn = 4);
    }
}

// Standing: paws at z = 0, spine on x = 0, stack centred on y.
//
// Sweeping the tool over an undersized prism chamfers every edge of that prism
// at once — the equator's diamond takes the vertical corners, the two apexes
// take the front and back rims. Undersized by exactly `ch` in all three
// directions, so the sweep returns the beast to its stated size.
//
// Shrinking and regrowing is a morphological opening, and an opening does not
// give the region back exactly — it rounds off small features and it lets the
// holes drift. Intersecting the sweep with a prism of the plain silhouette
// pins the holes back to their nominal size; the sweep only ever removes
// material, so everywhere else the two agree and the intersection is free.
//
// `convexity` is not decoration. Preview draws with OpenCSG, which peels a
// fixed number of depth layers, and a sight line across this beast crosses far
// more surfaces than the default allows — legs, ears, the far wall. Left at
// the default the preview shows a handful of stray blocks and the model looks
// broken until you press F6. It changes nothing about the geometry: preview
// and render agree, and the exported solid is the same to the last cubic
// micron.
module mascot_solid() {
    rotate([90, 0, 0])
        if (ch > 0)
            difference() {
                intersection() {
                    linear_extrude(height = pixel_depth * c, center = true, convexity = 10)
                        silhouette();

                    minkowski() {
                        linear_extrude(height = pixel_depth * c - 2 * ch, center = true, convexity = 10)
                            offset(delta = -ch)
                                silhouette();
                        chamfer_tool(ch);
                    }
                }

                // Through the whole depth and out both faces, so the cut is
                // one clean plane rather than something that stops inside.
                linear_extrude(height = pixel_depth * c + 4 * ch, center = true, convexity = 10)
                    hole_wedges();
            }
        else
            linear_extrude(height = pixel_depth * c, center = true, convexity = 10)
                silhouette();
}

// Face up, back on the bed. +90 about X sends the model's -y — the back —
// downward; the translate is outside the rotate, so it moves the already
// turned part onto z = 0 and into +y.
module orient() {
    if (lay_flat)
        translate([0, mascot_height, pixel_depth * c / 2])
            rotate([90, 0, 0])
                children();
    else
        children();
}

module openappa_mascot() {
    orient() color(body_color) mascot_solid();
}

openappa_mascot();
