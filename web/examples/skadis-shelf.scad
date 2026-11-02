// SKADIS small tray with removable, flat-printed hook brackets.
// Print all three pieces as laid out: body base down, brackets on broad sides.
// No supports, glue or hardware. Use 4 perimeters; PETG or PLA.
// Mount both brackets in same-height slots: insert teeth, then lower until
// the necks seat. Slide the body's rear apron under the upper fingers,
// between the guide ribs, until its rear edge meets the vertical spines.
// The ledges carry the load; slide the body forward to remove it.
// This is a gravity mount, not a locking latch. Use for light items.
// Dimensions: nominal 5 x 15 mm obround slots, 5 mm board thickness.
// Adjacent columns: 20 mm; alternate columns offset vertically by 20 mm.
// Same-height slots and same-column slots repeat every 40 mm.
// Measurement references (geometry below is original, not copied):
// https://www.reddit.com/r/ikeahacks/comments/rk2qk0/
// https://www.printables.com/model/109009-ikea-skadis-hook-hacek-10mm-100mm
// https://github.com/sebasdv/skadisgen (staggered grid only).
// Print the small hook first to tune fit; physical fit/load not yet tested.

mount_steps = 2; // Hook spacing in 40 mm same-height grid steps [1:1:3]
depth = 40; // Tray depth including rear assembly apron, mm [30:1:55]
rim_height = 8; // Rim height above tray floor, mm [4:1:14]
wall = 2.4; // Rim and guide rib thickness, mm [1.6:0.2:3.2]
floor_thickness = 3; // Tray floor thickness, mm [2.4:0.2:4]
board_thickness = 5; // Measured board thickness, mm [4.6:0.1:5.4]
tolerance = 0.3; // Clearance per slot side and behind board, mm [0.2:0.05:0.4]
joint_clearance = 0.3; // Sliding joint clearance, mm [0.2:0.05:0.4]
assembled = 0; // View mode: 0 print layout, 1 mounted assembly [0:1:1]

slot_width = 5; // Nominal slot width, mm
clip_width = slot_width-2*tolerance; // Blade thickness across slot, mm
board_gap = board_thickness+tolerance; // Clearance behind front bearing face, mm

width = 40*mount_steps+20; // Tray width, with 10 mm hook margins, mm
apron_depth = 13; // Rear joint area, outside the tray pocket, mm

// Shared SKADIS profile, in [depth from board, height] coordinates.
// Front bearing face is depth 0; board occupies depth -board_thickness..0.
// A 3 mm neck at height 23..26 passes through the slot; the rear tooth
// reaches height 17, extending 6 mm below the neck to catch behind the board.
// Centre the 9 mm tall tooth in the slot's 10 mm straight middle to insert.
// At default tolerance its 4.4 mm width leaves 0.3 mm on each slot side;
// board_gap leaves another 0.3 mm behind the board before pulling forward.
// Lower until the neck corners bear on the rounded slot bottom (contact is
// intentional). Lift about 8 mm at default fit to centre the tooth for removal.
// Verified against a separate 5 x 15 mm obround-slot board via the dev API;
// Insertion/lowering clear; withdrawal catches. See docs/example-feedback/skadis.md.
module skadis_clip_outline() {
    polygon([[0,0],[3,0],[3,30],[0,30],[0,26],
             [-board_gap-3,26],[-board_gap-3,17.6],
             [-board_gap-2.4,17],[-board_gap,17],
             [-board_gap,23],[0,23]]);
}

module body() {
    union() {
        cube([width,depth,floor_thickness]);
        // Three outside rims and a back rim leave the mounting apron clear.
        translate([0,apron_depth,floor_thickness-0.1])
            cube([width,wall,rim_height+0.1]);
        translate([0,depth-wall,floor_thickness-0.1])
            cube([width,wall,rim_height+0.1]);
        for (x=[0,width-wall])
            translate([x,apron_depth,floor_thickness-0.1])
                cube([wall,depth-apron_depth,rim_height+0.1]);
        apron_guides();
    }
}

// Flat side plate: lower ledge supports the body; upper finger captures
// its rear apron. All intersections overlap, rather than just touching.
module support_bracket() {
    linear_extrude(clip_width)
        union() {
            skadis_clip_outline();
            polygon([[2,0],[depth+3,4],[depth+3,8],[2,8]]);
            translate([2,8+floor_thickness+joint_clearance])
                square([11,3]);
        }
}

// Guide ribs capture the upper fingers sideways, leaving the front open
// for removal. The body slides toward the board until it meets the spines.
module apron_guides() {
    for (x=[10,width-10])
        for (side=[-1,1])
            translate([x+side*(clip_width/2+joint_clearance+wall/2)-wall/2,
                       0,floor_thickness-0.1])
                cube([wall,10,3.1]);
}

module brackets() {
    if (assembled == 1) {
        for (x=[10,width-10])
            translate([x-clip_width/2,0,0])
                rotate([90,0,90]) support_bracket();
    } else {
        for (i=[0:1])
            translate([board_gap+3+i*(depth+board_gap+14),depth+8,0])
                support_bracket();
    }
}

// Body and brackets print together, each in its own preview colour.
module layout() {
    color("#89c454") translate(assembled == 1 ? [0,3,8] : [0,0,0]) body();
    color("#d9dde2") brackets();
}

layout();

