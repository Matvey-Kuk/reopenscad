// Reusable threaded bottle neck and screw-on pouring spout; print upright.
// Custom matched thread: this pair does not claim a commercial bottle fit.
neck_diameter = 30; // Male thread-root diameter, mm [26:2:40]
flange_diameter = 42; // Glue/clamp flange diameter, mm [40:2:54]
wall = 2.4; // Minimum shell and flange thickness, mm [1.6:0.2:3.2]
thread_depth = 1.2; // Radial crest height and flank run, mm [1.2:0.1:1.6]
crest_width = 1.2; // Axial crest width, mm [1.2:0.1:1.6]
pitch = 5.2; // Thread pitch, mm [5.2:0.2:6.4]
turns = 1.75; // Thread engagement revolutions [1.5:0.25:2.5]
outlet_diameter = 12; // Clear pouring bore, mm [10:1:18]
spout_height = 12; // Straight outlet height, mm [8:1:20]
radial_clearance = 0.3; // Mating radial clearance, mm [0.2:0.05:0.4]
axial_clearance = 0.15; // Each flank allowance; total 0.3 mm [0.1:0.025:0.2]
part_gap = 10; // Bed spacing, mm [6:2:18]
profile_points = 64; // Thread polygon samples
slices_per_turn = 32; // Rotated sweep slices per turn
$fn = 64; // Shell resolution
cut_extra = 0.1; // Boolean overlap, mm
root_radius = neck_diameter/2; // Derived thread root, mm
thread_height = turns*pitch; // Derived threaded height, mm
socket_radius = root_radius+thread_depth+radial_clearance; // Groove envelope, mm
cap_radius = socket_radius+wall; // Spout skirt radius, mm
outlet_radius = outlet_diameter/2; // Derived outlet bore, mm
transition_height = socket_radius-outlet_radius; // 45-degree shoulder rise, mm

assert(transition_height > 0, "Outlet must be smaller than the neck");
assert(flange_diameter/2 >= root_radius+thread_depth+1.2, "Flange too small");
assert(pitch >= 2*thread_depth+crest_width+2*axial_clearance+1.2,
       "Pitch too small for printable thread lands");

// The polar radius encodes a trapezoid in an axial section. Twisting stacked
// polygon slices advances that profile one pitch per revolution.
function tooth_radius(angle, root, radial_clearance, axial_clearance) =
    root + radial_clearance + thread_depth *
        max(0, min(1, (thread_depth + crest_width/2 + axial_clearance
                      - abs(angle-180)*pitch/360) / thread_depth));

module helical_thread(root, length, radial_clearance=0, axial_clearance=0) {
    linear_extrude(height=length, twist=-360*length/pitch,
                   slices=ceil(slices_per_turn*length/pitch), convexity=10)
        polygon([for (i=[0:profile_points-1])
            let(angle=i*360/profile_points,
                radius=tooth_radius(angle,root,radial_clearance,axial_clearance))
                [radius*cos(angle),radius*sin(angle)]]);
}

module neck() {
    difference() {
        union() {
            cylinder(h=wall,d=flange_diameter);
            translate([0,0,wall-cut_extra])
                helical_thread(root_radius,thread_height+cut_extra);
        }
        translate([0,0,-cut_extra])
            cylinder(h=wall+thread_height+2*cut_extra,r=root_radius-wall);
    }
}

module pouring_spout() {
    difference() {
        union() {
            cylinder(h=thread_height,r=cap_radius);
            translate([0,0,thread_height])
                cylinder(h=transition_height,r1=cap_radius,r2=outlet_radius+wall);
            translate([0,0,thread_height+transition_height])
                cylinder(h=spout_height,r=outlet_radius+wall);
        }
        translate([0,0,-cut_extra])
            rotate([0,0,360*cut_extra/pitch])
                helical_thread(root_radius,thread_height+2*cut_extra,
                               radial_clearance,axial_clearance);
        translate([0,0,thread_height])
            cylinder(h=transition_height,r1=socket_radius,r2=outlet_radius);
        translate([0,0,thread_height+transition_height-cut_extra])
            cylinder(h=spout_height+2*cut_extra,r=outlet_radius);
        translate([0,0,-cut_extra])
            cylinder(h=thread_depth+cut_extra,
                     r1=socket_radius+cut_extra,r2=root_radius+radial_clearance);
    }
}

neck();
translate([flange_diameter/2+cap_radius+part_gap,0,0]) pouring_spout();
