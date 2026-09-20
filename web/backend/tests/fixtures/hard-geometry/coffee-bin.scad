// Coffee drip bag bin — first parametric prototype. All dimensions in mm.
// Intended for used drip bags / filters, not loose coffee grounds.
// Print basket upright. Lid is one piece and needs supports.
// Integral vertical floor fins raise the contents. Export parts separately.
// assembled | exploded | basket | tray | lid
part = "assembled";
total_height = 237.6;
top_diameter = 180;
bottom_diameter = top_diameter / 2;
nozzle = 0.4;
line_width = 0.4; // Set to actual slicer extrusion width before final printing.
blade_thickness = 3 * line_width;
blade_count = 64;
blade_angle = 40; // From the local tangent.
overlap_factor = 1.35;
blade_curve_angle = 40; // Shallow circular channel across each blade.
blade_curve_segments = 5;
floor_thickness = 3;
bottom_cone_height = 10; // Interior rises towards the center; exterior stays flat.
tray_floor = 2.4;
tray_wall = 2.4;
tray_height = 38;
tray_radial_clearance = 3;
drain_space = 6;
lid_plate = 3;
lid_skirt = 5; // Internal locating skirt.
lid_clearance = 0.55;
lid_wall = 2.4;
rim_height = 6;
handle_height = 28;
handle_diameter = 48;
floor_fin_height = 6; // Height above the local conical floor.
floor_fin_pitch = 8;
floor_fin_thickness = blade_thickness;
floor_fin_radius = bottom_diameter/2-5; // Open perimeter for drainage.
stiffener_height = 1.8;
stiffener_width = 1.8;
$fn = 64;

basket_z = tray_floor + drain_space;
body_height = total_height - basket_z - lid_plate - handle_height;
rb = bottom_diameter / 2;
rt = top_diameter / 2;
slope = (rt-rb)/body_height;
function rad(z) = rb + slope*z;
function blade_width(r) = 2*PI*r/blade_count*overlap_factor/cos(blade_angle);
function curve_radius(r) = blade_width(r)/(2*sin(blade_curve_angle/2));
function arc_step() = blade_curve_angle/blade_curve_segments;
function segment_x(r,a) = curve_radius(r)*(cos(a)*cos(arc_step()/2)-cos(blade_curve_angle/2));
function segment_y(r,a) = curve_radius(r)*sin(a)*cos(arc_step()/2);
function segment_length(r) = 2*curve_radius(r)*sin(arc_step()/2)+0.10;
function tip_angle() = blade_curve_angle/2-arc_step()/2;
function local_tip_x(r) = segment_x(r,tip_angle())+blade_thickness/2*cos(tip_angle())-segment_length(r)/2*sin(tip_angle());
function local_tip_y(r) = segment_y(r,tip_angle())+blade_thickness/2*sin(tip_angle())+segment_length(r)/2*cos(tip_angle());
function tip_x(r) = local_tip_x(r)*cos(blade_angle)+local_tip_y(r)*sin(blade_angle);
function tip_y(r) = -local_tip_x(r)*sin(blade_angle)+local_tip_y(r)*cos(blade_angle);
function center_r(r) = sqrt(r*r-tip_y(r)*tip_y(r))-tip_x(r);
function tray_inner(z) = rad(z-basket_z)+tray_radial_clearance;
// Covers the whole curved blade section, with at least 1 mm inside cover.
rim_inner_radius = rt - (blade_width(rt)*sin(blade_angle)+blade_thickness+2);

assert(body_height > 80, "Increase total_height");
assert(rt > rb, "Top must be wider than base");
assert(blade_count >= 24 && blade_count <= 64, "Blade count must be between 24 and 64");
assert(blade_thickness > 0, "Invalid blade thickness");

// Each curved-channel facet is a single extruded trapezoid.
// End faces sit inside the bottom and upper rim for flat print surfaces.
function section_center_x(r,a) = center_r(r)+segment_x(r,a)*cos(blade_angle)+segment_y(r,a)*sin(blade_angle);
function section_center_y(r,a) = -segment_x(r,a)*sin(blade_angle)+segment_y(r,a)*cos(blade_angle);
module blade_facet(a) {
    z0 = floor_thickness/2;
    z1 = body_height-rim_height/2;
    r0 = rad(z0);
    r1 = rad(z1);
    beta = a-blade_angle;
    dx = section_center_x(r1,a)-section_center_x(r0,a);
    dy = section_center_y(r1,a)-section_center_y(r0,a);
    dn = dx*cos(beta)+dy*sin(beta);
    dt = -dx*sin(beta)+dy*cos(beta);
    h = z1-z0;
    length = sqrt(h*h+dn*dn);
    tilt = atan(dn/h);
    translate([section_center_x(r0,a),section_center_y(r0,a),z0])
        rotate([0,0,beta])
            rotate([0,tilt,0])
                rotate([90,0,90])
                    linear_extrude(height=blade_thickness,center=true)
                        polygon(points=[
                            [-segment_length(r0)/2,0],
                            [segment_length(r0)/2,0],
                            [dt+segment_length(r1)/2,length],
                            [dt-segment_length(r1)/2,length]
                        ]);
}
module blade() {
    union() {
        for(j=[0:blade_curve_segments-1])
            blade_facet(-blade_curve_angle/2+(j+0.5)*arc_step());
    }
}
module band(z,h,w) {
    translate([0,0,z])
        difference() {
            cylinder(h=h,r1=rad(z),r2=rad(z+h));
            translate([0,0,-0.1])
                cylinder(h=h+0.2,r1=rad(z-0.1)-w,r2=rad(z+h+0.1)-w);
        }
}
module basket() {
    union() {
        // Solid outside floor; convex inside sheds liquid to the open louvers.
        union() {
            cylinder(h=floor_thickness,r1=rb,r2=rad(floor_thickness));
            translate([0,0,floor_thickness-0.1])
                cylinder(h=bottom_cone_height+0.1,r1=rad(floor_thickness),r2=0);
        }
        for(i=[0:blade_count-1]) rotate([0,0,i*360/blade_count]) blade();
        band(body_height/3,stiffener_height,stiffener_width);
        band(body_height*2/3,stiffener_height,stiffener_width);
        top_rim();
        bottom_fins();
    }
}
module tray() {
    union() {
        difference() {
            cylinder(h=tray_height,r1=tray_inner(0)+tray_wall,r2=tray_inner(tray_height)+tray_wall);
            translate([0,0,tray_floor])
                cylinder(h=tray_height-tray_floor+0.1,
                    r1=tray_inner(tray_floor),r2=tray_inner(tray_height+0.1));
        }
        // Integral tray supports: basket itself prints with a flat bottom.
        for(a=[0:120:240])
            rotate([0,0,a])
                translate([rb-10,0,tray_floor-0.1])
                    cylinder(h=drain_space+0.1,r1=4.5,r2=3.5);
    }
}
module top_rim() {
    translate([0,0,body_height-rim_height])
        difference() {
            union() {
                cylinder(h=rim_height-0.8,r1=rad(body_height-rim_height),r2=rad(body_height-0.8));
                translate([0,0,rim_height-0.8])
                    cylinder(h=0.8,r1=rad(body_height-0.8),r2=rt-0.8);
            }
            translate([0,0,-0.1]) cylinder(h=rim_height+0.2,r=rim_inner_radius);
            translate([0,0,rim_height-0.8])
                cylinder(h=0.9,r1=rim_inner_radius,r2=rim_inner_radius+0.9);
        }
}
module lid_print() {
    // Upright, internal skirt on the plate: use supports under the lid.
    plug_r = rim_inner_radius-lid_clearance;
    union() {
        difference() {
            union() {
                cylinder(h=0.7,r1=plug_r-0.6,r2=plug_r);
                translate([0,0,0.7]) cylinder(h=lid_skirt-0.6,r=plug_r);
            }
            translate([0,0,-0.1])
                cylinder(h=lid_skirt+0.3,r=plug_r-lid_wall);
        }
        translate([0,0,lid_skirt]) {
            cylinder(h=lid_plate-0.8,r=rt+0.8);
            translate([0,0,lid_plate-0.8])
                cylinder(h=0.8,r1=rt+0.8,r2=rt);
        }
        // Large rounded grip with an undercut for fingertips.
        translate([0,0,lid_skirt+lid_plate]) {
            translate([0,0,-0.1]) cylinder(h=3.1,r1=12,r2=9);
            translate([0,0,2.9]) cylinder(h=handle_height-12.9,r=9);
            translate([0,0,handle_height-9])
                scale([1,1,18/handle_diameter])
                    sphere(d=handle_diameter,$fn=96);
        }
    }
}
module bottom_fins() {
    // Parallel vertical ribs, integral with the solid cone below them.
    // Every channel opens into the unobstructed peripheral drainage strip.
    intersection() {
        union() {
            for(x=[-4*floor_fin_pitch:floor_fin_pitch:4*floor_fin_pitch])
                translate([x-floor_fin_thickness/2,-floor_fin_radius,floor_thickness-0.1])
                    cube([floor_fin_thickness,2*floor_fin_radius,bottom_cone_height+floor_fin_height+0.2]);
        }
        translate([0,0,floor_thickness-0.1])
            cylinder(h=bottom_cone_height+floor_fin_height+0.2,r=floor_fin_radius);
        // Cone shifted upwards: rib tops follow the conical floor.
        translate([0,0,floor_thickness-0.1])
            union() {
                cylinder(h=floor_fin_height+0.1,r=rad(floor_thickness));
                translate([0,0,floor_fin_height])
                    cylinder(h=bottom_cone_height+0.1,r1=rad(floor_thickness),r2=0);
            }
    }
}
if(part=="tray" || part=="assembled" || part=="exploded")
    color([0.27,0.32,0.28]) tray();
if(part=="basket" || part=="assembled" || part=="exploded")
    color([0.74,0.61,0.43])
        translate([0,0,part=="basket" ? 0 : basket_z]) basket();
if(part=="lid") color([0.27,0.32,0.28]) lid_print();
if(part=="assembled" || part=="exploded")
    color([0.27,0.32,0.28])
        translate([0,0,basket_z+body_height-lid_skirt+(part=="exploded" ? 35 : 0)])
            lid_print();

