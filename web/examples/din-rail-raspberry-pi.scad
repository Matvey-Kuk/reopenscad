// Raspberry Pi 4 / 5 DIN rail carrier: a flat plate and a separate spring clip.
// PETG recommended. Print both parts as laid out on Z=0, standoffs upward;
// no supports, 0.2 mm layers, at least 4 perimeters. Keep the latch gap clear.
// Join the clip front to the plate UNDERSIDE using two M3x12 bolts + nuts
// (default dimensions); fit nuts inside the open clip bridge before snapping.
// Board hardware: four M2.5 through-bolts + nuts, length chosen for the
// 3 mm plate + 8 mm standoff + PCB + washers/nut (about 16 mm default).
// Put bolt heads below the plate; use insulating washers against the PCB.
// Official Pi 4B / Pi 5 holes: (3.5,3.5), (61.5,3.5), (3.5,52.5), (61.5,52.5)
// from the lower-left, component side up, USB/Ethernet at right,
// on the 85 x 56 mm board. Pattern is 58 x 49 mm; PCB hole diameter 2.7 mm.
// The compact plate ends before the Ethernet/USB overhang; ports stay open.
// https://datasheets.raspberrypi.com/rpi4/raspberry-pi-4-mechanical-drawing.pdf
// https://datasheets.raspberrypi.com/rpi5/raspberry-pi-5-mechanical-drawing.pdf
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
standoff_diameter = 6; // Boss diameter around each board hole, mm [5.8:0.2:7]
board_hole = 2.8; // Board screw clearance through the carrier, mm [2.7:0.1:3]
edge_margin = 4.5; // Plate border beyond mounting-hole centres, mm [4:0.5:7]
vent_width = 22; // Width of each cooling window, mm [16:1:26]
vent_height = 10; // Height of each cooling window, mm [6:1:12]
part_gap = 8; // Separation between the two printed parts, mm [6:2:16]
hole_pitch_x = 58; // Official Pi 4/5 hole pitch along board, mm (standard dimension)
hole_pitch_y = 49; // Official Pi 4/5 hole pitch across board, mm (standard dimension)

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

holes = [[-hole_pitch_x/2,-hole_pitch_y/2],[hole_pitch_x/2,-hole_pitch_y/2],
         [-hole_pitch_x/2,hole_pitch_y/2],[hole_pitch_x/2,hole_pitch_y/2]]; // Centred PCB hole coordinates, mm
plate_x = hole_pitch_x + 2*edge_margin; // Derived plate width, mm
plate_y = hole_pitch_y + 2*edge_margin; // Derived plate depth, mm

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

color("#f59ab8") board_carrier();
color("#d9dde2") translate([0,plate_y/2+part_gap+slot+tooth_thickness+hook_overlap,0])
    rail_clip();
