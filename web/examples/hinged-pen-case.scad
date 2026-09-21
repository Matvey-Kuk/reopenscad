// Long pen case with a spring-button detent latch and captive conical knuckle hinge.
// Print fully open, both outer faces on the bed; flex the gaps free when cool.
inner_length = 154; // Clear length, mm [120:2:180]
inner_width = 26; // Clear width, mm [24:2:40]
half_height = 14; // Height of each shell, mm [13:1:20]
corner = 5; // Plan corner chamfer, mm [4:1:8]
knuckle_count = 13; // Odd number of alternating knuckles [9:2:17]
lid_bevel = 1.2; // Lid outside edge chamfer, mm [1.2:0.2:2]
lid_inner_bevel = 1.2; // Lid inside edge chamfer, mm [1.2:0.2:2]
wall = 1.6; // Shell wall thickness, mm [1.2:0.2:3]
floor_thickness = 1.6; // Floor and lid skin, mm [1.2:0.2:3]
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

button_thickness = 1.2; // Spring thickness, mm [1.2:0.2:1.6]
button_width = 30; // Flexible bridge span, mm [28:2:40]
button_height = 8; // Bridge height, mm [7:0.5:9]
button_bottom = 3; // Bridge lower elevation, mm [2.5:0.5:4]
button_gap = 0.4; // Slots above and below bridge, mm [0.2:0.05:0.4]
latch_clearance = 0.3; // Skirt-to-button clearance, mm [0.2:0.05:0.4]
recess_width = 42; // Recess width, mm [40:2:50]
recess_corner = 3; // Recess chamfer, mm [2:0.5:4]
recess_bottom = 1.6; // Solid material below recess, mm [1.2:0.2:2]
hook_width = 14; // Catch tooth width, mm [12:1:18]
hook_projection = 2.7; // Tooth projection from spring face, mm [2.5:0.1:3]
hook_bottom = 5.7; // Tooth tip lower elevation, mm [5.5:0.1:6]
hook_face = 1.2; // Vertical tooth tip thickness, mm [1.2:0.2:1.6]
skirt_width = 34; // Lid latch skirt width, mm [32:2:40]
skirt_bottom = 1.6; // Skirt lower elevation when closed, mm [1.2:0.2:2]
skirt_corner = 2.5; // Skirt corner chamfer, mm [2:0.5:3]
window_width = 16; // Tooth window width, mm [16:1:20]
window_corner = 0.8; // Tooth window chamfer, mm [0.4:0.2:1]
window_bottom = 5.4; // Catch window lower edge, mm [5.2:0.1:5.5]
window_top = 10.2; // Catch window upper edge, mm [10:0.2:11]
latch_side_web = 1.6; // Material beside recessed latch, mm [1.2:0.2:2]
recess_depth = wall+latch_clearance; // Derived spring face position, mm
front_depth = recess_depth+button_thickness; // Front boss thickness, mm
front_extra = front_depth-wall; // Extra front clearance, mm

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


assert(window_bottom < hook_bottom && window_top > hook_bottom+hook_face+hook_projection,
       "Window must clear the tooth");
assert(button_bottom+button_height+button_gap < half_height-wall,
       "Button slots must not reach the rim");
assert(window_bottom-skirt_bottom > 2+window_corner, "Catch strip is too thin");

module front_panel(width,bottom,top,chamfer,depth,y=0) {
    translate([centre_x-width/2,y+depth,bottom]) rotate([90,0,0])
        linear_extrude(depth) chamfer_rectangle(width,top-bottom,chamfer,chamfer);
}

module spring_slots() {
    for (z=[button_bottom-button_gap,button_bottom+button_height])
        translate([centre_x-button_width/2,recess_depth-cut_extra,z])
            cube([button_width,button_thickness+2*cut_extra,button_gap]);
}

module catch_tooth() {
    side_polygon(centre_x-hook_width/2,hook_width,
        [[recess_depth+cut_extra,hook_bottom-hook_projection],[recess_depth-hook_projection,hook_bottom],
         [recess_depth-hook_projection,hook_bottom+hook_face],
         [recess_depth+cut_extra,hook_bottom+hook_face+hook_projection]]);
}

module catch_skirt() {
    difference() {
        front_panel(skirt_width,skirt_bottom,half_height+skirt_corner,skirt_corner,wall);
        front_panel(window_width,window_bottom,window_top,window_corner,
                    wall+2*cut_extra,-cut_extra);
    }
}

module base() {
    union() {
        difference() {
            union() {
                shell_solid(outer_length,outer_depth,half_height,corner,edge_bevel,0);
                knuckles(false);
            }
            difference() {
                translate([wall,wall,floor_thickness])
                    shell_solid(outer_length-2*wall,outer_depth-2*wall,
                                half_height,corner-wall,inner_bevel,0);
                translate([centre_x-recess_width/2-latch_side_web,-cut_extra,-cut_extra])
                    cube([recess_width+2*latch_side_web,front_depth+cut_extra,
                          half_height+2*cut_extra]);
            }
            socket_cuts();
            front_panel(recess_width,recess_bottom,half_height+recess_corner,
                        recess_corner,recess_depth+cut_extra,-cut_extra);
            spring_slots();
        }
        catch_tooth();
    }
}

module lid() {
    union() {
        hollow_lid();
        knuckles(true);
        catch_skirt();
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
