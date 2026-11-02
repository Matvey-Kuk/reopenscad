// SKADIS J-hook for keys, scissors or a small cable coil.
// Print flat on its broad side as laid out, without supports; 4 perimeters.
// Insert the rear tooth through a slot, then lower until the neck seats.
// Lift before withdrawing. PETG or PLA; intended for light items.
// Dimensions: nominal 5 x 15 mm obround slots, 5 mm board thickness.
// Adjacent columns: 20 mm; alternate columns offset vertically by 20 mm.
// Same-height slots and same-column slots repeat every 40 mm.
// Measurement references (geometry below is original, not copied):
// https://www.reddit.com/r/ikeahacks/comments/rk2qk0/
// https://www.printables.com/model/109009-ikea-skadis-hook-hacek-10mm-100mm
// https://github.com/sebasdv/skadisgen (staggered grid only).
// Print the small hook first to tune fit; physical fit/load not yet tested.

reach = 28; // Clear depth from front spine to outer lip, mm [18:1:40]
lip_height = 12; // Height of the retaining lip, mm [9:1:18]
arm_thickness = 4; // Lower arm thickness in side profile, mm [3:0.5:6]
board_thickness = 5; // Measured board thickness, mm [4.6:0.1:5.4]
tolerance = 0.3; // Clearance per slot side and behind board, mm [0.2:0.05:0.4]

slot_width = 5; // Nominal slot width, mm
clip_width = slot_width-2*tolerance; // Blade thickness across slot, mm
board_gap = board_thickness+tolerance; // Clearance behind front bearing face, mm


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

color("#5ccfc0") translate([board_gap+3,0,0])
    linear_extrude(clip_width)
        union() {
            skadis_clip_outline();
            polygon([[2,0],[reach+7,0],[reach+7,lip_height-1],
                     [reach+6,lip_height],[reach+3,lip_height],
                     [reach+3,arm_thickness],[9,arm_thickness],
                     [3,arm_thickness+6],[2,arm_thickness+6]]);
        }

