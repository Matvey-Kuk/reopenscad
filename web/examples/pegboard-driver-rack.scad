// Four-tool rack for 25.4 mm pegboard; print with the rear plate on the bed.
// Attach with M4 bolts and washers through the pegboard, then insert drivers.
tool_count = 4; // Number of tool sockets [2:1:6]
tool_spacing = 25.4; // Socket and peg pitch, mm [20:0.2:30]
socket_diameter = 12; // Clear tool-shaft bore, mm [8:1:16]
plate_height = 32; // Backplate height, mm [28:1:45]
plate_thickness = 3; // Backplate thickness, mm [2:0.2:5]
socket_depth = 24; // Projection from pegboard, mm [24:1:32]
socket_wall = 2.4; // Material around socket, mm [1.6:0.2:4]
bolt_diameter = 4.6; // M4 clearance, mm [4.4:0.1:5]
$fn = 48; // Circular resolution
cut_extra = 1; // Cutter overlap, mm
plate_width = (tool_count+1)*tool_spacing; // Derived plate width, mm
socket_outer = socket_diameter/2+socket_wall; // Derived radius, mm

module tool_socket(x) {
    // Bores run across the plate: their pointed roofs rise at 45 degrees.
    translate([x,plate_height/2,plate_thickness])
        rotate([90,0,0]) linear_extrude(2*socket_outer,center=true)
            difference() {
                polygon([[-socket_outer,0],[socket_outer,0],
                         [socket_outer,socket_depth-socket_outer],
                         [0,socket_depth],
                         [-socket_outer,socket_depth-socket_outer]]);
                translate([0,socket_depth/2])
                    union() {
                        circle(d=socket_diameter);
                        polygon([[-socket_diameter/(2*sqrt(2)),socket_diameter/(2*sqrt(2))],
                                 [0,socket_diameter/sqrt(2)],
                                 [socket_diameter/(2*sqrt(2)),socket_diameter/(2*sqrt(2))]]);
                    }
            }
}

difference() {
    union() {
        cube([plate_width,plate_height,plate_thickness]);
        for (i=[1:tool_count]) tool_socket(i*tool_spacing);
    }
    for (x=[tool_spacing/2,plate_width-tool_spacing/2])
        for (y=[(plate_height-tool_spacing)/2,(plate_height+tool_spacing)/2])
            translate([x,y,-cut_extra])
                cylinder(h=plate_thickness+2*cut_extra,d=bolt_diameter);
}
