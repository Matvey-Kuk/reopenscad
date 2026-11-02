// 608 pillow block for a light spool axle: bearing 8 x 22 x 7 mm.
// Print as shown, bearing axis vertical and pocket mouth up; no supports.
// In service rotate onto its long bottom edge; two M5 bolts secure the feet.
// Print two blocks for an 8 mm axle. Use shaft collars/spacers at the INNER
// races to retain the axle; this open press-fit seat is for radial loads.
// Insert bearing from above, pressing only its outer race onto the shoulder.
// Tune seat_allowance with your printer/material: negative = interference,
// positive = clearance. Default: 0.05 mm diametral interference at facet
// flats (less at vertices); print a fit sample before pressing in a bearing.
// Shoulder opening clears the seal/inner race; top cone guides insertion.
// Source: SKF 608 drawing, https://docs.rs-online.com/9931/A700000008717780.pdf

bearing_od = 22; // 608 outside diameter, mm [22:0.1:22]
bearing_width = 7; // 608 width, mm [7:0.1:7]
seat_allowance = -0.05; // Added pocket diameter, mm [-0.1:0.05:0.4]
shoulder_opening = 19.4; // Through opening under outer race, mm [19.4:0.1:19.4]
shoulder_thickness = 2; // Axial retaining shoulder, mm [1.6:0.2:3]
lead_in = 0.6; // 45-degree insertion chamfer depth, mm [0.4:0.1:1]
radial_wall = 4; // Material outside bearing seat, mm [3:0.5:6]
axis_height = 20; // Axle centre above mounting surface, mm [19:1:26]
foot_extension = 12; // Foot width beyond bearing body, mm [11:1:16]
foot_height = 8; // Height of mounting feet in service, mm [7:1:9]
mount_screw = 5; // Mount screw nominal diameter, mm [3:2:5]
hole_clearance = 0.4; // Added mount-hole diameter, mm [0.2:0.05:0.4]
$fn = 64; // Bearing seat facets [48:8:96]

half_body = bearing_od/2+radial_wall;
half_width = half_body+foot_extension;
// Centre the asymmetric teardrop by its full height, keeping both skins >=1.2.
mount_radius = (mount_screw+hole_clearance)/(2*cos(180/32));
depth = max(shoulder_thickness+bearing_width+lead_in,
            2.4+mount_radius*(1+sqrt(2)));
mount_axis_z = (depth-mount_radius*(sqrt(2)-1))/2;
top = axis_height+half_body;
// Circumscribed polygon compensates for facet flats at the bearing seat.
seat_diameter = (bearing_od+seat_allowance)/cos(180/$fn);

module mounting_hole() {
    radius = mount_radius;
    linear_extrude(height=foot_height+2)
        union() {
            circle(r=radius,$fn=32);
            polygon([[-radius/sqrt(2),radius/sqrt(2)],
                     [0,radius*sqrt(2)], [radius/sqrt(2),radius/sqrt(2)]]);
        }
}

color("#f0965a") difference() {
    linear_extrude(height=depth)
        polygon([[-half_width,0], [half_width,0], [half_width,foot_height],
                 [half_body,foot_height], [half_body,top-8],
                 [half_body-8,top], [-half_body+8,top],
                 [-half_body,top-8], [-half_body,foot_height],
                 [-half_width,foot_height]]);
    translate([0,axis_height,-1])
        cylinder(h=depth+2,d=shoulder_opening/cos(180/$fn));
    translate([0,axis_height,shoulder_thickness])
        cylinder(h=depth-shoulder_thickness+1,d=seat_diameter);
    translate([0,axis_height,depth-lead_in])
        cylinder(h=lead_in+0.1,d1=seat_diameter,d2=seat_diameter+2*(lead_in+0.1));
    for (x = [-half_body-foot_extension/2,half_body+foot_extension/2])
        translate([x,foot_height+1,mount_axis_z]) rotate([90,0,0]) mounting_hole();
}
