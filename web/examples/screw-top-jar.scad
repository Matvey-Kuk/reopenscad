// Small screw-top jar and matching lid; print both open ends up as laid out.
// Coarse trapezoidal threads have 45-degree flanks and a chamfered lead-in.
body_diameter = 42; // Thread-root outside diameter, mm [36:2:60]
body_height = 34; // Jar height, mm [24:2:60]
wall = 2.4; // Shell and floor thickness, mm [1.6:0.2:4]
thread_depth = 1.2; // Radial thread height and flank run, mm [1.2:0.1:1.6]
crest_width = 1.2; // Flat axial thread crest, mm [1.2:0.1:1.6]
pitch = 5.2; // Axial advance per turn, mm [5.2:0.2:6.4]
turns = 2; // Engaged thread revolutions [1.5:0.25:2.5]
radial_clearance = 0.3; // Mating radial clearance, mm [0.2:0.05:0.4]
axial_clearance = 0.15; // Each flank allowance; total 0.3 mm [0.1:0.025:0.2]
part_gap = 12; // Bed spacing between parts, mm [8:2:20]
profile_points = 64; // Samples around the thread profile
slices_per_turn = 32; // Rotated slices per revolution
$fn = 64; // Shell resolution
cut_extra = 0.1; // Boolean overlap, mm
root_radius = body_diameter/2; // Derived thread root, mm
thread_height = turns*pitch; // Derived engagement length, mm
lid_radius = root_radius+thread_depth+radial_clearance+wall; // Lid envelope, mm

assert(pitch >= 2*thread_depth+crest_width+2*axial_clearance+1.2,
       "Pitch must leave at least 1.2 mm between female thread grooves");
assert(body_height > thread_height+wall, "Jar too short for threads");

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

module jar() {
    difference() {
        union() {
            cylinder(h=body_height,r=root_radius);
            translate([0,0,body_height-thread_height])
                intersection() {
                    helical_thread(root_radius,thread_height);
                    cylinder(h=thread_height,r1=root_radius,r2=root_radius+thread_height);
                }
        }
        translate([0,0,wall])
            cylinder(h=body_height,r=root_radius-wall);
    }
}

module lid() {
    difference() {
        cylinder(h=wall+thread_height,r=lid_radius);
        translate([0,0,wall])
            helical_thread(root_radius,thread_height+cut_extra,
                           radial_clearance,axial_clearance);
        translate([0,0,wall+thread_height-thread_depth])
            cylinder(h=thread_depth+cut_extra,
                     r1=root_radius+radial_clearance,
                     r2=root_radius+radial_clearance+thread_depth+cut_extra);
    }
}

jar();
translate([root_radius+thread_depth+lid_radius+part_gap,0,0]) lid();
