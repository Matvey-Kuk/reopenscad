// Coarse M12-scale utility bolt and matching hex nut; print on their hex faces.
// The helical trapezoid is a functional custom thread, not a metric standard.
root_diameter = 10; // Thread-root diameter, mm [8:1:16]
thread_depth = 1.2; // Thread height and 45-degree flank run, mm [1.2:0.1:1.6]
crest_width = 1.2; // Flat axial crest, mm [1.2:0.1:1.6]
pitch = 5.2; // Thread advance per revolution, mm [5.2:0.2:6.4]
bolt_turns = 3; // Shaft length in revolutions [2:0.5:4]
nut_turns = 1.5; // Nut thickness in revolutions [1:0.25:2]
head_thickness = 4; // Hex head height, mm [3:0.5:6]
hex_diameter = 22; // Across hex corners, mm [20:1:28]
radial_clearance = 0.3; // Mating radial clearance, mm [0.2:0.05:0.4]
axial_clearance = 0.15; // Each flank allowance; total 0.3 mm [0.1:0.025:0.2]
part_gap = 8; // Gap between hex parts, mm [6:1:16]
profile_points = 64; // Thread polygon samples
slices_per_turn = 32; // Sweep slices per revolution
$fn = 64; // Circular resolution
cut_extra = 0.1; // Boolean overlap, mm
root_radius = root_diameter/2; // Derived core radius, mm
bolt_length = bolt_turns*pitch; // Derived threaded length, mm
nut_height = nut_turns*pitch; // Derived nut height, mm

assert(hex_diameter*cos(30)/2 > root_radius+thread_depth+radial_clearance+1.2,
       "Hex wall must be at least 1.2 mm");
assert(pitch >= 2*thread_depth+crest_width+2*axial_clearance+1.2,
       "Increase pitch to leave a printable female thread land");

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

module bolt() {
    union() {
        cylinder(h=head_thickness,d=hex_diameter,$fn=6);
        translate([0,0,head_thickness-cut_extra])
            intersection() {
                helical_thread(root_radius,bolt_length+cut_extra);
                // The upper end tapers down to the root to start the nut.
                cylinder(h=bolt_length+cut_extra,
                         r1=root_radius+bolt_length,r2=root_radius);
            }
    }
}

module nut() {
    difference() {
        cylinder(h=nut_height,d=hex_diameter,$fn=6);
        // Keep the phase continuous through the lower cutter extension.
        translate([0,0,-cut_extra])
            rotate([0,0,360*cut_extra/pitch])
                helical_thread(root_radius,nut_height+2*cut_extra,
                               radial_clearance,axial_clearance);
        translate([0,0,nut_height-thread_depth])
            cylinder(h=thread_depth+cut_extra,
                     r1=root_radius+radial_clearance,
                     r2=root_radius+radial_clearance+thread_depth+cut_extra);
        translate([0,0,-cut_extra])
            cylinder(h=thread_depth+cut_extra,
                     r1=root_radius+radial_clearance+thread_depth+cut_extra,
                     r2=root_radius+radial_clearance);
    }
}

bolt();
translate([hex_diameter+part_gap,0,0]) nut();
