// Deep divided utility chest with a bevelled lid and elastic-cord closure.
// Print open; lace 2 mm elastic through the four front eyelets and tie inside.
inner_length = 86; // Clear length, mm [76:2:110]
inner_width = 62; // Clear depth, mm [52:2:80]
half_height = 22; // Height of each shell, mm [20:1:30]
corner = 8; // Plan corner chamfer, mm [6:1:12]
knuckle_count = 9; // Odd number of alternating knuckles [7:2:13]
lid_bevel = 6; // Wide 45-degree lid shoulder, mm [4:1:8]
lid_inner_bevel = 6; // Matching interior shoulder, mm [4:1:8]
wall = 2.4; // Shell wall thickness, mm [1.2:0.2:3]
floor_thickness = 2.4; // Floor and lid skin, mm [1.2:0.2:3]
knuckle_gap = 0.35; // Axial knuckle clearance, mm [0.2:0.05:0.4]
bearing_clearance = 0.35; // Conical radial clearance, mm [0.3:0.05:0.4]
body_gap = 0.35; // Barrel-to-shell folding clearance, mm [0.3:0.05:0.4]
cone_slope = 1.15; // Cone radial change per axial millimetre
cone_engagement = 0.8; // Boss engagement into adjacent knuckle, mm [0.7:0.05:0.9]
cone_tip = 0.4; // Boss tip radius, mm [0.35:0.05:0.5]
socket_end_gap = 0.3; // Additional socket depth, mm [0.2:0.05:0.3]
knuckle_wall = 1.2; // Material outside socket, mm [1.2:0.1:1.4]
hinge_margin = 12; // Hinge end margin, mm [10:1:16]
edge_bevel = 1.2; // Exterior lower edge bevel, mm [1.2:0.2:2]
inner_bevel = 1.2; // Cavity floor bevel, mm [1.2:0.2:2]
$fn = 48; // Knuckle and cone resolution
cut_extra = 0.1; // Boolean overlap, mm

cord_diameter = 3.6; // Eyelet bore for 2 mm elastic, mm [3:0.2:4]
eyelet_spacing = 14; // Horizontal eyelet separation, mm [12:1:20]
eyelet_drop = 5.5; // Eyelet distance below each rim, mm [5:0.5:7]
compartments = 2; // Interior storage bays [1:1:4]
divider_thickness = 2.4; // Divider wall thickness, mm [1.2:0.2:3.2]
divider_drop = 3; // Divider top below the rim, mm [2:1:6]
front_extra = 0; // No additional front-wall depth, mm

// Same alternating conical boss/socket hinge and 45-degree buttress as
// web/backend/tests/fixtures/hard-geometry/hinge-box-narrow.scad.
outer_length = inner_length+2*wall; // Outside shell length, mm
outer_depth = inner_width+2*wall+front_extra; // Outside shell depth, mm
total_height = 2*half_height; // Closed height, mm
cone_length = knuckle_gap+cone_engagement; // Conical boss length, mm
boss_radius = cone_tip+cone_slope*cone_length; // Boss root radius, mm
socket_radius = boss_radius+bearing_clearance-cone_slope*knuckle_gap; // Socket mouth radius, mm
socket_depth = cone_engagement+socket_end_gap; // Socket depth, mm
socket_tip = socket_radius-cone_slope*socket_depth; // Socket end radius, mm
knuckle_radius = socket_radius+knuckle_wall; // Outside barrel radius, mm
axis_y = outer_depth+body_gap+knuckle_radius; // Hinge centre behind the front edge, mm
hinge_edge = axis_y+knuckle_radius; // Farthest barrel extent, mm
hinge_span = outer_length-2*hinge_margin; // Total active hinge length, mm
segment = hinge_span/knuckle_count; // Knuckle pitch along hinge, mm
knuckle_width = segment-knuckle_gap; // Solid knuckle length, mm
buttress_offset = axis_y-half_height+knuckle_radius*sqrt(2); // 45-degree support-line offset, mm
buttress_root = outer_depth-wall-buttress_offset; // Support height at shell, mm
buttress_edge = hinge_edge-buttress_offset; // Support height at barrel edge, mm
centre_x = outer_length/2; // Latch and shell centre, mm

assert(knuckle_count%2 == 1, "Use an odd knuckle count so both ends are captured");
assert(socket_tip > 0.2, "Socket cone must not invert");
assert(knuckle_width-2*socket_depth >= 1.2, "Knuckle sockets need a solid web");
assert(buttress_root > floor_thickness && buttress_root < half_height,
       "Increase half height to make room for the hinge buttress");
assert(sqrt(pow(hinge_edge-axis_y,2)+pow(buttress_edge-half_height,2))
       < axis_y-outer_depth, "Buttress would collide when folded");


assert(eyelet_drop-cord_diameter/sqrt(2) >= 1.2, "Eyelet must leave a solid rim");

module cord_eyelets(is_lid) {
    for (side=[-1,1])
        translate([centre_x+side*eyelet_spacing/2,wall+cut_extra,
                   half_height+(is_lid ? eyelet_drop : -eyelet_drop)])
            rotate([90,0,0]) linear_extrude(wall+2*cut_extra)
                rotate(is_lid ? 180 : 0) union() {
                    circle(d=cord_diameter);
                    polygon([[-cord_diameter/(2*sqrt(2)),cord_diameter/(2*sqrt(2))],
                             [0,cord_diameter/sqrt(2)],
                             [cord_diameter/(2*sqrt(2)),cord_diameter/(2*sqrt(2))]]);
                }
}

module dividers() {
    if (compartments>1)
        for (i=[1:compartments-1])
            translate([wall+i*inner_length/compartments-divider_thickness/2,
                       wall-cut_extra,floor_thickness-cut_extra])
                cube([divider_thickness,inner_width+2*cut_extra,
                      half_height-floor_thickness-divider_drop+cut_extra]);
}

module base() {
    difference() {
        union() {
            hollow_base();
            knuckles(false);
            dividers();
        }
        socket_cuts();
        cord_eyelets(false);
    }
}

module lid() {
    difference() {
        union() {
            hollow_lid();
            knuckles(true);
        }
        cord_eyelets(true);
    }
}

function knuckle_x(i) = hinge_margin+i*segment+knuckle_gap/2;

module chamfer_rectangle(length,depth,lower,upper) {
    polygon([[lower,0],[length-lower,0],[length,lower],[length,depth-upper],
             [length-upper,depth],[upper,depth],[0,depth-upper],[0,lower]]);
}

module side_polygon(x,length,points) {
    translate([x,0,0]) rotate([90,0,90])
        linear_extrude(length) polygon(points);
}

module shell_solid(length,depth,height,plan_chamfer,bottom_bevel,top_bevel) {
    intersection() {
        linear_extrude(height)
            chamfer_rectangle(length,depth,plan_chamfer,plan_chamfer);
        translate([0,depth+cut_extra,0]) rotate([90,0,0])
            linear_extrude(depth+2*cut_extra)
                chamfer_rectangle(length,height,bottom_bevel,top_bevel);
        translate([-cut_extra,0,0]) rotate([90,0,90])
            linear_extrude(length+2*cut_extra)
                chamfer_rectangle(depth,height,bottom_bevel,top_bevel);
    }
}

module knuckle(is_lid) {
    direction = is_lid ? -1 : 1;
    rotate([90,0,90]) linear_extrude(knuckle_width) union() {
        translate([axis_y,half_height]) circle(r=knuckle_radius);
        polygon([[outer_depth-wall,half_height],
                 [outer_depth-wall,half_height+direction*(buttress_root-half_height)],
                 [hinge_edge,half_height+direction*(buttress_edge-half_height)],
                 [hinge_edge,half_height]]);
    }
}

module cone_boss(x,direction) {
    translate([x,axis_y,half_height]) rotate([0,90*direction,0])
        cylinder(h=cone_length,r1=boss_radius,r2=cone_tip);
}

module cone_socket(x,direction) {
    translate([x,axis_y,half_height]) rotate([0,90*direction,0])
        cylinder(h=socket_depth,r1=socket_radius,r2=socket_tip);
}

module knuckles(is_lid) {
    for (i=[0:knuckle_count-1])
        if (((i%2)==1)==is_lid) {
            translate([knuckle_x(i),0,0]) knuckle(is_lid);
            if (is_lid) {
                cone_boss(knuckle_x(i),-1);
                cone_boss(knuckle_x(i)+knuckle_width,1);
            }
        }
}

module socket_cuts() {
    for (i=[0:knuckle_count-1])
        if (i%2==0) {
            if (i>0) cone_socket(knuckle_x(i),1);
            if (i<knuckle_count-1) cone_socket(knuckle_x(i)+knuckle_width,-1);
        }
}

module hollow_base() {
    difference() {
        shell_solid(outer_length,outer_depth,half_height,corner,edge_bevel,0);
        translate([wall,wall,floor_thickness])
            shell_solid(outer_length-2*wall,outer_depth-2*wall,
                        half_height,corner-wall,inner_bevel,0);
    }
}

module hollow_lid() {
    difference() {
        translate([0,0,half_height])
            shell_solid(outer_length,outer_depth,half_height,corner,0,lid_bevel);
        translate([wall,wall,half_height-cut_extra])
            shell_solid(outer_length-2*wall,outer_depth-2*wall,
                        half_height-floor_thickness+cut_extra,
                        corner-wall,0,lid_inner_bevel);
    }
}

module print_layout() {
    base();
    translate([0,2*axis_y,total_height]) rotate([180,0,0]) lid();
}

print_layout();
