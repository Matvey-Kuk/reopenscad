// SKADIS cable keeper: a sideways-opening C cradle for one flexible cable.
// Print flat on its broad side as laid out; no supports. Use PETG and
// 4 perimeters. Feed the cable along the bore or gently through the mouth.
// Insert the rear tooth in the board, then lower until its neck seats.
// Lift before removing. This guides a cable; it is not a strain relief.
// Dimensions: nominal 5 x 15 mm obround slots, 5 mm board thickness.
// Adjacent columns: 20 mm; alternate columns offset vertically by 20 mm.
// Same-height slots and same-column slots repeat every 40 mm.
// Measurement references (geometry below is original, not copied):
// https://www.reddit.com/r/ikeahacks/comments/rk2qk0/
// https://www.printables.com/model/109009-ikea-skadis-hook-hacek-10mm-100mm
// https://github.com/sebasdv/skadisgen (staggered grid only).
// Print the small hook first to tune fit; physical fit/load not yet tested.

cable_diameter = 6; // Cable diameter before running clearance, mm [4:0.5:10]
cable_clearance = 0.4; // Added bore diameter, mm [0.2:0.1:0.6]
mouth_ratio = 0.8; // Mouth width as a fraction of cable diameter [0.7:0.05:1]
ring_wall = 2; // Material around cable, mm [1.6:0.2:2.4]
board_thickness = 5; // Measured board thickness, mm [4.6:0.1:5.4]
tolerance = 0.3; // Clearance per slot side and behind board, mm [0.2:0.05:0.4]

$fn = 32; // Circle facets
slot_width = 5; // Nominal slot width, mm
clip_width = slot_width-2*tolerance; // Blade thickness across slot, mm
board_gap = board_thickness+tolerance; // Clearance behind front bearing face, mm

bore_radius = (cable_diameter+cable_clearance)/2; // Cable bore radius, mm
outer_radius = bore_radius+ring_wall; // Outside cradle radius, mm
mouth = cable_diameter*mouth_ratio; // Entry opening, mm

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

color("#b48ef0") translate([board_gap+3,0,0])
    linear_extrude(clip_width)
        union() {
            skadis_clip_outline();
            translate([outer_radius+2,outer_radius])
                difference() {
                    circle(r=outer_radius);
                    circle(r=bore_radius);
                    translate([0,-mouth/2])
                        square([outer_radius+1,mouth]);
                }
        }

