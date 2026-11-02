// Flat corner gusset for two coplanar 2020 rails meeting at 90 degrees.
// Print as shown, broad face on the bed; no supports. Units: mm.
// Use four M5 screws + washers and metal T-nuts matched to your profile.
// Set screw_diameter=3 for M3 T-nuts. Rails occupy x=0..20 and y=0..20;
// one rail butts against the other. Holes start beyond the butt joint.
// Fits the outer faces of 20 mm V-slot and B-type T-slot (nominal slot 6).
// V-slot drawing: 6.25 mm throat; B-type: 6 mm slot.
// Nut cavities differ: this plate does not assume interchangeable T-nuts.
// Sources: https://www.motedis.com/en/Profile-20x20-B-type-slot-6
// https://bitfab.parts/wp-content/uploads/2023/11/2020V-20x20mm-V-Slot-Profile-Drawing-v2.pdf
// https://www.motedis.com/en/T-nut-B-type-slot-6-M5
// https://www.motedis.com/en/T-nut-B-type-slot-6-M3

profile_width = 20; // Rail face width, mm [20:1:20]
arm_length = 60; // Length of each arm, mm [55:1:85]
plate_thickness = 5; // Plate thickness, mm [4:0.5:8]
screw_diameter = 5; // Metal T-nut thread diameter, mm [3:2:5]
hole_clearance = 0.4; // Added screw-hole diameter, mm [0.2:0.05:0.4]
edge_margin = 10; // Far hole distance from arm end, mm [8:1:12]
web_size = 14; // Diagonal reinforcement across inner corner, mm [8:1:20]
corner_cut = 3; // Exposed corner chamfer, mm [1:0.5:5]
$fn = 32; // Screw-hole facets [24:8:64]

// Compensate polygon flats so the minimum bore has the stated clearance.
hole_diameter = (screw_diameter+hole_clearance)/cos(180/$fn);

// A single extruded outline keeps the plate flat and the boolean work small.
color("#55a9e8") difference() {
    linear_extrude(height=plate_thickness)
        polygon([[corner_cut,0], [arm_length-corner_cut,0],
                 [arm_length,corner_cut], [arm_length,profile_width-corner_cut],
                 [arm_length-corner_cut,profile_width],
                 [profile_width+web_size,profile_width],
                 [profile_width,profile_width+web_size],
                 [profile_width,arm_length-corner_cut],
                 [profile_width-corner_cut,arm_length], [corner_cut,arm_length],
                 [0,arm_length-corner_cut], [0,corner_cut]]);
    for (distance = [profile_width+10, arm_length-edge_margin]) {
        translate([distance,profile_width/2,-1])
            cylinder(h=plate_thickness+2,d=hole_diameter);
        translate([profile_width/2,distance,-1])
            cylinder(h=plate_thickness+2,d=hole_diameter);
    }
}
