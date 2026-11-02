// NEMA 17 L-bracket for the face of a 2020 rail; light mechanisms.
// Print as shown: motor plate flat on the bed, rail flange pointing up.
// No supports: horizontal rail holes have 45-degree teardrop roofs.
// Bolt flange to rail first with two M5 screws, washers and matching T-nuts.
// Motor body sits above the plate, shaft through the large opening;
// insert four M3 screws from underneath. Check motor thread depth (4.5 mm
// in the cited drawing); account for plate and washers when choosing length.
// Motor edge is 8 mm from the flange; gussets clear its body by 4 mm.
// Rail screw heads plus washers must project less than motor_gap.
// Sources: https://www.pololu.com/file/0J714/SY42STH38-1684A.pdf
// https://www.motedis.com/en/Profile-20x20-B-type-slot-6

motor_body = 42.3; // Motor square envelope, mm [42:0.1:43]
motor_spacing = 31; // Motor screw centre spacing, mm [31:0.1:31]
pilot_diameter = 22; // Motor pilot boss diameter, mm [22:0.1:22]
plate_thickness = 6; // Both plates' thickness, mm [5:0.5:8]
motor_gap = 8; // Motor edge to rail flange, mm [8:1:12]
side_margin = 7; // Plate extension beyond motor at each side, mm [7:1:10]
rail_width = 20; // Extrusion face width, mm [20:1:20]
rail_screw_spacing = 28; // Two screws along the same rail slot, mm [24:2:36]
rail_screw_diameter = 5; // T-nut thread diameter, mm [3:2:5]
hole_clearance = 0.4; // Added screw-hole diameter, mm [0.2:0.05:0.4]
pilot_clearance = 0.3; // Added pilot-hole diameter, mm [0.2:0.05:0.4]
gusset_thickness = 3; // Side reinforcement thickness, mm [2:0.5:4]
$fn = 48; // Circular resolution [32:8:64]

width = motor_body+2*side_margin;
length = motor_body+2*motor_gap;
motor_y = motor_gap+motor_body/2;
flange_height = plate_thickness+rail_width;

// Compensate circular facets to preserve the specified minimum clearance.
motor_hole = (3+hole_clearance)/cos(180/$fn);
pilot_hole = (pilot_diameter+pilot_clearance)/cos(180/$fn);

module teardrop(diameter, depth) {
    radius = diameter/(2*cos(180/$fn));
    linear_extrude(height=depth)
        union() {
            circle(r=radius);
            polygon([[-radius/sqrt(2),radius/sqrt(2)],
                     [0,radius*sqrt(2)], [radius/sqrt(2),radius/sqrt(2)]]);
        }
}

color("#b48ef0") difference() {
    union() {
        translate([-width/2,0,0]) cube([width,length,plate_thickness]);
        // Overlap by 0.1 mm so flange and plate are one connected solid.
        translate([-width/2,-plate_thickness,0])
            cube([width,plate_thickness+0.1,flange_height]);
        for (x = [-width/2,width/2-gusset_thickness])
            translate([x,0,plate_thickness-0.1])
                rotate([90,0,90]) linear_extrude(height=gusset_thickness)
                    polygon([[0,0],[rail_width,0],[0,rail_width]]);
    }
    translate([0,motor_y,-1])
        cylinder(h=plate_thickness+2,d=pilot_hole);
    for (x = [-motor_spacing/2,motor_spacing/2])
        for (y = [-motor_spacing/2,motor_spacing/2])
            translate([x,motor_y+y,-1])
                cylinder(h=plate_thickness+2,d=motor_hole);
    for (x = [-rail_screw_spacing/2,rail_screw_spacing/2])
        translate([x,1,plate_thickness+rail_width/2]) rotate([90,0,0])
            teardrop(rail_screw_diameter+hole_clearance,plate_thickness+2);
}
