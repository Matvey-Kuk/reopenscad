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
blade_curve_segments = 6;
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

union() { blade(); rotate([0,0,360/blade_count]) blade(); }
