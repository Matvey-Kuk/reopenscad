// OpenAPPA mascot — the pixel-grid beast, built out of chamfered blocks.
//
// The mascot is not invented here. It is the `BEAST` bitmap that draws the
// mark on screen (website/components/Logo.tsx and components/pixel-marks.ts):
// a 24-wide by 22-tall grid, horns on rows 0-1, head on rows 2-13, body on
// rows 14-19, legs and paws on rows 20-21.
//
// Every lit pixel of that bitmap becomes a stack of cubes, `pixel_depth` of
// them front to back. Each cube is exactly one grid cell on every side and is
// chamfered on all twelve edges, so blocks sit against their neighbours rather
// than into them, and the grid reads from every direction: on the face, on the
// flanks, across the top and under the paws.
//
// The original bitmap's primary and dim cells form two printable parts. Where
// it shows the background through the beast — the two eyes and nostril pair —
// there are no blocks, so those read as holes exactly as they do on screen.
//
// This file lives beside the meshes it produces, in website/public/brand, so
// that the source and everything exported from it ship together. Run the
// commands below from the repository root; the names they write are the names
// the site serves.
//
// Preview both colours:
//   openscad website/public/brand/openappa.scad
//
// Export the whole beast as one piece:
//   openscad --export-format binstl -D 'part="solid"' -o website/public/brand/openappa-full-body.stl website/public/brand/openappa.scad
//
// Export two registered parts, then import both files together as one
// multi-part object in the slicer:
//   openscad --export-format binstl -D 'part="primary"'   -o website/public/brand/openappa-multi-color-part-1.stl website/public/brand/openappa.scad
//   openscad --export-format binstl -D 'part="secondary"' -o website/public/brand/openappa-multi-color-part-2.stl website/public/brand/openappa.scad
//
// Or export a colour-aware 3MF (slicer support for its material assignments
// varies):
//   openscad -o openappa.3mf -O export-3mf/color-mode=model website/public/brand/openappa.scad

/* ------------------------------------------------------------ parameters --- */

mascot_height = 50;   // mm, paws to horn tips; everything else follows

pixel_depth   = 4;    // blocks front to back

chamfer       = 0.15; // chamfer on every cube edge, as a fraction of a cell.
                      // It also sets the seam: the V-groove between two block
                      // faces is twice this wide and this deep.

bond          = 0.02; // how far a block's core reaches past a shared face into
                      // its neighbour, as a fraction of a cell. Cores are
                      // interior, so this never shows; without it every part
                      // of the model meets its neighbour on an exactly
                      // coplanar face, and CGAL fragments that into 241
                      // pieces instead of one solid.

part = "assembly";    // [assembly, primary, secondary, solid]

primary_color   = "#202124";
secondary_color = "#8a8d91";

material_boundary_shift = 0.01; // shift the internal two-colour boundary into
                                // the primary cells, as a fraction of a cell.
                                // This avoids degenerate STL triangles from a
                                // cut exactly on the block construction plane.

/* ------------------------------------------------------------------ grid --- */

// Original BEAST palette: `1` primary body, `3` dim muzzle/paws, `4` eyes,
// `2` nose, and `.` outside background. Only `1` and `3` become blocks.
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

// Is there a block at this bitmap position? Off-grid reads as empty, so the
// silhouette needs no special casing.
function lit(row, col) =
    row >= 0 && row < GRID_ROWS && col >= 0 && col < GRID_COLS
    && (BEAST[row][col] == "1" || BEAST[row][col] == "3");

// Is there a block at this 3D grid position? Model-space Z increases as bitmap
// rows decrease, while X and Y follow columns and depth layers respectively.
function occupied(row, col, layer, dx = 0, dy = 0, dz = 0) =
    layer + dy >= 0 && layer + dy < pixel_depth
    && lit(row - dz, col + dx);

/* ----------------------------------------------------------------- parts --- */

// One block: a cube with all twelve edges chamfered, as the convex hull of
// three boxes that are each full length on one axis and shortened on the other
// two. The hull bridges them with the 45-degree faces, which is the chamfer.
module block() {
    a = c - 2 * ch;
    hull() {
        cube([c, a, a], center = true);
        cube([a, c, a], center = true);
        cube([a, a, c], center = true);
    }
}

// Chamfering every edge takes the corners off every block. In a lattice, the
// missing corners line up: four blocks around a shared edge leave a diamond
// tunnel, and eight around a shared vertex leave a pocket.
//
// The filler follows the topology around each join instead of expanding one
// box independently toward every neighbour. Every block gets a recessed core.
// A shared face gets a bridge, an edge gets one only when all four surrounding
// blocks exist, and a vertex gets one only when all eight exist. This closes
// internal tunnels without exposing square filler at a concave silhouette
// corner where an orthogonal neighbour exists but the diagonal block does not.
module core(row, col, layer) {
    a      = c - 2 * ch;
    bridge = 2 * (ch + bond * c);

    // Recessed block center.
    cube([a, a, a], center = true);

    // Face bridges. Positive directions make every shared face exactly once.
    if (occupied(row, col, layer, dx = 1))
        translate([c / 2, 0, 0]) cube([bridge, a, a], center = true);
    if (occupied(row, col, layer, dy = 1))
        translate([0, c / 2, 0]) cube([a, bridge, a], center = true);
    if (occupied(row, col, layer, dz = 1))
        translate([0, 0, c / 2]) cube([a, a, bridge], center = true);

    // Edge bridges. Each requires the complete 2 x 2 voxel neighbourhood.
    if (occupied(row, col, layer, dx = 1)
        && occupied(row, col, layer, dy = 1)
        && occupied(row, col, layer, dx = 1, dy = 1))
        translate([c / 2, c / 2, 0])
            cube([bridge, bridge, a], center = true);

    if (occupied(row, col, layer, dx = 1)
        && occupied(row, col, layer, dz = 1)
        && occupied(row, col, layer, dx = 1, dz = 1))
        translate([c / 2, 0, c / 2])
            cube([bridge, a, bridge], center = true);

    if (occupied(row, col, layer, dy = 1)
        && occupied(row, col, layer, dz = 1)
        && occupied(row, col, layer, dy = 1, dz = 1))
        translate([0, c / 2, c / 2])
            cube([a, bridge, bridge], center = true);

    // Vertex bridge. The three faces, three diagonals, and opposite corner
    // must all be occupied, so the bridge can never reach an exposed corner.
    if (occupied(row, col, layer, dx = 1)
        && occupied(row, col, layer, dy = 1)
        && occupied(row, col, layer, dz = 1)
        && occupied(row, col, layer, dx = 1, dy = 1)
        && occupied(row, col, layer, dx = 1, dz = 1)
        && occupied(row, col, layer, dy = 1, dz = 1)
        && occupied(row, col, layer, dx = 1, dy = 1, dz = 1))
        translate([c / 2, c / 2, c / 2])
            cube([bridge, bridge, bridge], center = true);
}

// The bitmap counts rows downward from the horns; the model counts z upward
// from the paws. X is centred on the mascot's spine and Y on the middle of the
// stack, so the piece sits over the origin with its paws at z = 0.
module mascot_solid() {
    for (row   = [0 : GRID_ROWS - 1],
         col   = [0 : GRID_COLS - 1],
         layer = [0 : pixel_depth - 1])
        if (lit(row, col))
            translate([(col + 0.5 - GRID_COLS / 2) * c,
                       (layer + 0.5 - pixel_depth / 2) * c,
                       (GRID_ROWS - row - 0.5) * c]) {
                block();
                core(row, col, layer);
            }
}

module pixel_region(material) {
    union()
        for (row = [0 : GRID_ROWS - 1], col = [0 : GRID_COLS - 1])
            if (material == "solid" ? lit(row, col)
                                     : BEAST[row][col] == material)
                translate([(col + 0.5 - GRID_COLS / 2) * c,
                           (GRID_ROWS - row - 0.5) * c])
                    square([c, c], center = true);
}

// The secondary region grows imperceptibly into the primary region, moving
// the material boundary off the blocks' coplanar construction faces. The
// primary region is its exact complement within the solid bitmap. Extruding
// these complementary regions through the complete depth partitions every
// bridge and block with no gap or overlap, while producing clean STL meshes.
module material_region(material) {
    shift = material_boundary_shift * c;

    if (material == "3")
        offset(delta = shift) pixel_region("3");

    if (material == "1")
        difference() {
            // Oversized rather than silhouette-shaped: mascot_solid() supplies
            // the exterior, so this mask creates only the internal colour cut.
            translate([0, mascot_height / 2])
                square([(GRID_COLS + 4) * c, mascot_height + 4 * c],
                       center = true);
            offset(delta = shift) pixel_region("3");
        }
}

module material_mask(material) {
    rotate([90, 0, 0])
        linear_extrude(height = pixel_depth * c, center = true)
            material_region(material);
}

module mascot_part(material) {
    intersection() {
        mascot_solid();
        material_mask(material);
    }
}

module openappa_mascot(selected_part = part) {
    assert(selected_part == "assembly"
           || selected_part == "primary"
           || selected_part == "secondary"
           || selected_part == "solid",
           str("Unknown part: ", selected_part));

    if (selected_part == "assembly" || selected_part == "primary")
        color(primary_color) mascot_part("1");

    if (selected_part == "assembly" || selected_part == "secondary")
        color(secondary_color) mascot_part("3");

    if (selected_part == "solid")
        mascot_solid();
}

openappa_mascot();
