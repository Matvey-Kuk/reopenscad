// DIN rail label holder with a recessed bed for an adhesive paper label.
// PETG recommended. Print exactly as shown, flat cross-section on Z=0;
// no supports, 0.2 mm layers, at least 4 perimeters, latch gap kept clear.
// Apply a label to the front recess (normal to Y). Default label: 32 x 16 mm.
// A 45-degree pocket roof avoids an unsupported lip in this orientation.
// Fits TS35 / EN 60715 / IEC 60715 top-hat rail, 7.5 or 15 mm deep.
// Default interface: 35 mm overall, 1 mm flange; rail_inner locates the seats.
// Depth does not enter the fit: only the outer flange edges are gripped.
// For 1.5 mm sheet (e.g. TS35 x 15), set flange_thickness = 1.5.
// Nominal fit: 0.30 mm TOTAL slot play, 0.25 mm at each outer edge,
// and 1.35 mm retaining overlap per lip (hook_overlap - edge_clearance).
// The beam deflects during engagement; the seated rail has clearance,
// not spring preload. Use rail end stops where sliding must be prevented.
// Checked on the dev server with separate 7.5/1 and 15/1.5 mm rail sections;
// geometry checks do not establish printed spring strength or fatigue life.
// Hook the rigid left lip first, then press the right ramp over the rail.
// Pull the right-hand tab outward to release; do not bend the whole carrier.
// Original geometry; fit-test the clip before fitting electronics.
// Rail references:
// https://eshop.weidmueller.com/en/ts-35x7-5ll-1mstzn/p/0514510000
// https://assets.dam.weidmueller.com/assets/api/ab2b4e3c-189d-4eae-b435-7c5a77e1ac25/Original/MB_BUILDING_EN_2025_WEB.pdf

/* [Label] */
label_width = 32; // Adhesive label width across rail, mm [24:1:34]
label_height = 16; // Adhesive label height along rail, mm [10:1:18]
recess_depth = 0.8; // Label recess into the mounting face, mm [0.4:0.2:1.2]
label_clearance = 0.3; // Clearance around label edges, mm [0.2:0.1:0.4]

/* [Rail fit] */
rail_width = 35; // Outside flange width, mm [34.8:0.1:35.2]
rail_inner = 25; // Seat-position reference; need not match the rail web, mm [24:0.5:27]
flange_thickness = 1; // Measured sheet thickness, mm [1:0.1:1.5]
fit_clearance = 0.3; // Total flange slot clearance, mm [0.2:0.05:0.4]
edge_clearance = 0.25; // Clearance at each outer rail edge, mm [0.2:0.05:0.4]
spring_wall = 1.4; // Flexible beam thickness, mm [1.2:0.1:1.6]
spring_length = 18; // Beam root distance above the rail face, mm [18:1:24]
hook_overlap = 1.6; // Tooth reach inward from the side wall, mm [1.4:0.1:1.8]
clip_width = 24; // Contact length along the rail, mm [24:2:32]
bridge_thickness = 4; // Front mounting bridge thickness, mm [3:0.5:5]

/* [Hidden] */
$fn = 24; // Facets per circular hole / standoff
cut_extra = 0.1; // Boolean cutter overlap, mm
rigid_wall = 2.4; // Rigid hook side wall thickness, mm
tooth_thickness = 1.6; // Minimum retaining lip thickness, mm
seat_gap = 1.8; // Air gap beside the spring, mm
edge = rail_width/2 + edge_clearance; // Outer side-wall datum, mm
slot = flange_thickness + fit_clearance; // Flange capture slot height, mm
front = spring_length + bridge_thickness; // Mounting face Y coordinate, mm
seat_inner = rail_inner/2 + 1.1; // Keep seats outside the inner opening, mm
seat_outer = edge - seat_gap; // Spring-side rigid seat outer edge, mm

// Cross-section lies in XY. Rail length is Z; rail front is Y=0.
// All material below Y=0 stays within the outer 1.8 mm of each flange.
// The independent right beam flexes in XY, along continuous print layers.
module clip_profile() {
    union() {
        translate([-edge-rigid_wall,spring_length])
            square([2*edge+rigid_wall+spring_wall,bridge_thickness]);
        // Rigid side and the left flange's front seat.
        translate([-edge-rigid_wall,0])
            square([edge+rigid_wall-seat_inner,spring_length+0.1]);
        polygon([[-edge-rigid_wall,0],[-edge,0],[-edge,-slot],
                 [-edge+hook_overlap,-slot],
                 [-edge+hook_overlap,-slot-tooth_thickness],
                 [-edge,-slot-tooth_thickness-hook_overlap],
                 [-edge-rigid_wall,-slot-tooth_thickness-hook_overlap]]);
        // Right seat is separated from the beam all the way to its root.
        translate([seat_inner,0])
            square([seat_outer-seat_inner,spring_length+0.1]);
        polygon([[edge,-slot-tooth_thickness-hook_overlap],
                 [edge+spring_wall,-slot-tooth_thickness-hook_overlap],
                 [edge+spring_wall,spring_length+0.1],
                 [edge,spring_length+0.1],[edge,-slot],
                 [edge-hook_overlap,-slot],
                 [edge-hook_overlap,-slot-tooth_thickness]]);
        // Solid thumb / screwdriver tab, with a chamfered outside corner.
        translate([edge+spring_wall-0.1,0])
            polygon([[0,0],[4,0],[4,2],[2,4],[0,4]]);
    }
}

// The cutter is drawn in its local (Y,Z) plane and extruded across X.
// The top rises toward the front at 45 degrees, so each new layer is supported.
module label_pocket() {
    pocket_half = (label_width+2*label_clearance)/2;
    bottom = (clip_width-label_height)/2-label_clearance;
    top = (clip_width+label_height)/2+label_clearance;
    translate([-pocket_half,0,0])
        rotate([90,0,90])
            linear_extrude(2*pocket_half)
                polygon([[front-recess_depth,bottom],
                         [front+cut_extra,bottom],
                         [front+cut_extra,top+recess_depth+cut_extra],
                         [front-recess_depth,top]]);
}

color("#f0965a") difference() {
    linear_extrude(clip_width) clip_profile();
    label_pocket();
}
