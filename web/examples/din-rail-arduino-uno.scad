// Arduino Uno R3 DIN rail carrier: a flat plate and a separate spring clip.
// PETG recommended. Print both parts as laid out on Z=0, standoffs upward;
// no supports, 0.2 mm layers, at least 4 perimeters. Keep the latch gap clear.
// Join the clip front to the plate UNDERSIDE with two M3x12 bolts + nuts
// (default dimensions); fit nuts inside the open clip bridge before snapping.
// Board hardware: four M2.5 x 16 nylon bolts and nuts below the plate.
// Put heads at the PCB; use heads at most 4.2 mm wide by the upper-left header.
// The four holes are NOT rectangular. Coordinates below are in mm, viewed
// from the component side, USB at left, origin at the PCB lower-left corner:
// (13.97,2.54), (15.24,50.80), (66.04,7.62), (66.04,35.56).
// These correspond to (550,100),(600,2000),(2600,300),(2600,1400) mil.
// PCB nominal outline is 68.58 x 53.34 mm; mounting holes are 3.2 mm.
// Sized for the Uno R3; verify clone layouts. Ports and headers stay open.
// https://docs.arduino.cc/resources/datasheets/A000066-datasheet.pdf
// https://docs.arduino.cc/hardware/uno-rev3/ (official CAD files)
// Verified against UNO-TH_Rev3e.brd; datasheet section 5.4 omits the
// lower-left X dimension, which the CAD file supplies as 13.97 mm.
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

/* [Carrier] */
plate_thickness = 3; // Flat carrier thickness, mm [2.4:0.2:4]
standoff_height = 8; // Board underside clearance above plate, mm [6:1:12]
standoff_diameter = 5.4; // Narrow boss clears the upper-left header solder, mm [5.4:0.1:5.6]
board_hole = 2.8; // M2.5 clearance through carrier (PCB holes are 3.2), mm [2.7:0.1:3]
edge_margin = 4.5; // Plate border beyond mounting-hole centres, mm [4:0.5:7]
vent_width = 22; // Width of each cooling window, mm [16:1:26]
vent_height = 10; // Height of each cooling window, mm [6:1:12]
part_gap = 8; // Separation between the two printed parts, mm [6:2:16]
hole_lower_left = [13.97,2.54]; // Official Uno R3 lower-left hole XY, mm
hole_upper_left = [15.24,50.80]; // Official Uno R3 upper-left hole XY, mm
hole_lower_right = [66.04,7.62]; // Official Uno R3 lower-right hole XY, mm
hole_upper_right = [66.04,35.56]; // Official Uno R3 upper-right hole XY, mm

/* [Rail fit] */
rail_width = 35; // Outside flange width, mm [34.8:0.1:35.2]
rail_inner = 25; // Seat-position reference; need not match the rail web, mm [24:0.5:27]
flange_thickness = 1; // Measured sheet thickness, mm [1:0.1:1.5]
fit_clearance = 0.3; // Total flange slot clearance, mm [0.2:0.05:0.4]
edge_clearance = 0.25; // Clearance at each outer rail edge, mm [0.2:0.05:0.4]
spring_wall = 1.4; // Flexible beam thickness, mm [1.2:0.1:1.6]
spring_length = 18; // Beam root distance above the rail face, mm [18:1:24]
hook_overlap = 1.6; // Tooth reach inward from the side wall, mm [1.4:0.1:1.8]
clip_width = 18; // Contact length along the rail, mm [14:2:30]
bridge_thickness = 4; // Front mounting bridge thickness, mm [3:0.5:5]
mount_spacing = 16; // Two M3 attachment centres across rail, mm [12:2:20]
mount_hole = 3.4; // M3 shaft clearance, mm [3.3:0.1:3.6]

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

hole_min = [min(hole_lower_left[0],hole_upper_left[0],hole_lower_right[0],hole_upper_right[0]),
            min(hole_lower_left[1],hole_upper_left[1],hole_lower_right[1],hole_upper_right[1])]; // Hole bounds, mm
hole_max = [max(hole_lower_left[0],hole_upper_left[0],hole_lower_right[0],hole_upper_right[0]),
            max(hole_lower_left[1],hole_upper_left[1],hole_lower_right[1],hole_upper_right[1])]; // Hole bounds, mm
pattern_centre = (hole_min+hole_max)/2; // Follow edits to the mounting coordinates, mm
holes = [hole_lower_left-pattern_centre,hole_upper_left-pattern_centre,
         hole_lower_right-pattern_centre,hole_upper_right-pattern_centre]; // Centred hole coordinates, mm
plate_x = hole_max[0]-hole_min[0] + 2*edge_margin; // Hole bounding width plus borders, mm
plate_y = hole_max[1]-hole_min[1] + 2*edge_margin; // Hole bounding height plus borders, mm

// Diamond bores have 45-degree roofs and accept a round M3 shaft.
// Their across-flats dimension is mount_hole, not the corner-to-corner size.
module attachment_bores() {
    for (x = [-mount_spacing/2,mount_spacing/2])
        translate([x,front+cut_extra,clip_width/2])
            rotate([90,0,0])
                linear_extrude(bridge_thickness+2*cut_extra)
                    rotate(45) square([mount_hole,mount_hole],center=true);
}

module rail_clip() {
    difference() {
        linear_extrude(clip_width) clip_profile();
        attachment_bores();
    }
}

module board_carrier() {
    difference() {
        union() {
            translate([-plate_x/2,-plate_y/2,0])
                cube([plate_x,plate_y,plate_thickness]);
            for (p = holes)
                translate([p[0],p[1],plate_thickness-cut_extra])
                    cylinder(h=standoff_height+cut_extra,d=standoff_diameter);
        }
        for (p = holes)
            translate([p[0],p[1],-cut_extra])
                cylinder(h=plate_thickness+standoff_height+2*cut_extra,d=board_hole);
        // Leave a solid centre band for the clip and room for bolt heads.
        for (y = [-14,14])
            translate([-vent_width/2,y-vent_height/2,-cut_extra])
                cube([vent_width,vent_height,plate_thickness+2*cut_extra]);
        for (x = [-mount_spacing/2,mount_spacing/2])
            translate([x,0,-cut_extra])
                cylinder(h=plate_thickness+2*cut_extra,d=mount_hole);
    }
}

color("#5ccfc0") board_carrier();
color("#d9dde2") translate([0,plate_y/2+part_gap+slot+tooth_thickness+hook_overlap,0])
    rail_clip();
