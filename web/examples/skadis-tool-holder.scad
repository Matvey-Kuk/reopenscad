// SKADIS screwdriver holder: shafts pass through; wider handles rest on top.
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

tool_count = 3; // Number of tool bores [2:1:5]
tool_spacing = 22; // Centre spacing between tool bores, mm [20:1:28]
shaft_diameter = 12; // Clear bore diameter; smaller than handle, mm [6:1:16]
depth = 36; // Holder depth including rear assembly apron, mm [32:1:48]
floor_thickness = 4; // Tool-bearing plate thickness, mm [3:0.5:6]
wall = 2.4; // Guide rib thickness, mm [1.6:0.2:3.2]
board_thickness = 5; // Measured board thickness, mm [4.6:0.1:5.4]
tolerance = 0.3; // Clearance per slot side and behind board, mm [0.2:0.05:0.4]
joint_clearance = 0.3; // Sliding joint clearance, mm [0.2:0.05:0.4]
assembled = 0; // View mode: 0 print layout, 1 mounted assembly [0:1:1]

$fn = 24; // Bore facets (diameter below compensates for the polygon flats)
slot_width = 5; // Nominal slot width, mm
clip_width = slot_width-2*tolerance; // Blade thickness across slot, mm
board_gap = board_thickness+tolerance; // Clearance behind front bearing face, mm

mount_span = 40*ceil(((tool_count-1)*tool_spacing+shaft_diameter+12)/40); // Hook spacing, mm
width = mount_span+20; // Holder width including hook margins, mm
bore_y = (13+depth)/2; // Bore row ahead of assembly apron, mm
bore_cut_diameter = shaft_diameter/cos(180/$fn); // Preserve clear diameter across flats, mm
cut_extra = 1; // Through-cutter overlap, mm

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
    difference() {
        union() {
            cube([width,depth,floor_thickness]);
            apron_guides();
        }
        for (i=[0:tool_count-1])
            translate([width/2+(i-(tool_count-1)/2)*tool_spacing,
                       bore_y,-cut_extra])
                cylinder(h=floor_thickness+2*cut_extra,d=bore_cut_diameter);
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
    color("#e4665f") translate(assembled == 1 ? [0,3,8] : [0,0,0]) body();
    color("#d9dde2") brackets();
}

layout();

