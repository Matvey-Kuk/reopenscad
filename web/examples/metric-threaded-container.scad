// M24x3 small-parts container and lid; print both open ends up as laid out.
// Store spare screws, beads or nozzle tips. Use <=0.2 mm layers; no supports.
// Basic-profile M24x3 with printing allowance; not a gauged 6g/6H fit.
// Radial allowance 0.25 mm gives about 0.125 mm normal flank clearance.
// Standard metric fastening thread, not a beverage-bottle closure or liquid seal.
body_height = 30; // Total container height, mm [20:2:50]
wall = 2.4; // Nominal wall at the thread root, and floor thickness, mm [1.6:0.2:3.2]
thread_turns = 2; // Engaged M24x3 revolutions [2:0.5:3]
radial_clearance = 0.25; // Female radial allowance, mm [0.15:0.05:0.3]
end_clearance = 0.4; // Extra depth before the lid floor, mm [0.2:0.1:0.8]
grip_flats = 30; // Lid width across its six grip flats, mm [30:2:36]
part_gap = 8; // Bed gap between body and lid, mm [4:1:16]
assembled = false; // Show lid engaged, 0.2 mm above its seated shoulder
resolution = 48; // Profile vertices and slices per turn [32:16:64]

// ISO 262 M24 coarse pitch = 3 mm:
// https://www.westfieldfasteners.co.uk/Standards/Nut_Hex_M.html
// ISO 68-1 basic 60-degree profile, H/8 crest and H/4 root truncation:
// https://www.iso.org/standard/85107.html
/* [Hidden] */
major = 24;
pitch = 3;
depth = 5*sqrt(3)*pitch/16;
root = major/2-depth;
thread_height = thread_turns*pitch;
neck_start = body_height-thread_height;
lid_height = wall+end_clearance+depth+thread_height;
lid_corners = grip_flats/cos(30);
$fn = resolution;
assert(resolution >= 32 && resolution <= 64 && resolution%16==0,
       "Use 32, 48 or 64 samples to preserve crest/root flats");
assert(wall >= 1.6 && root > wall,"Keep a printable wall and an open cavity");
assert(radial_clearance > 0 && radial_clearance < depth/2,"Keep thread engagement");
assert(end_clearance > 0,"Leave clearance above the neck");
assert(neck_start > wall+2,"Container too short for this neck");
assert(grip_flats/2-major/2-radial_clearance >= 1.2,"Lid wall too thin");

function metric_radius(a,allowance) = major/2+allowance
    - min(depth,max(0,(abs(a-180)*pitch/360-pitch/16)*sqrt(3)));

module metric_thread(height,allowance=0) {
    linear_extrude(height=height,twist=-360*height/pitch,
                   slices=ceil(resolution*height/pitch),convexity=10)
        polygon([for (i=[0:resolution-1])
            let(a=i*360/resolution,r=metric_radius(a,allowance))
            [r*cos(a),r*sin(a)]]);
}

module container() {
    difference() {
        union() {
            cylinder(h=neck_start,r=major/2);
            translate([0,0,neck_start]) metric_thread(thread_height);
        }
        translate([0,0,wall]) cylinder(h=body_height-wall,r=root-wall,$fn=64);
    }
}

module lid() {
    difference() {
        cylinder(h=lid_height,d=lid_corners,$fn=6);
        // Full-diameter relief at the closed end lets the neck seat on the rim.
        translate([0,0,wall]) cylinder(h=end_clearance,r=major/2+radial_clearance);
        // A 45-degree cone narrows the relief to the thread crest, so the first
        // crest above it is not a flat ceiling; it adds headroom, not engagement.
        translate([0,0,wall+end_clearance])
            cylinder(h=depth,r1=major/2+radial_clearance,r2=root+radial_clearance);
        translate([0,0,lid_height-thread_height]) metric_thread(thread_height,radial_clearance);
    }
}

color("#5ccfc0") container();
// Flipping the lid reverses its local angle and Z. Compensate its phase,
// including fractional turns; turn and rise together by pitch per revolution.
color("#e4665f") lid_placed();
module lid_placed() {
    if (assembled)
        translate([0,0,neck_start+lid_height+0.2])
            rotate([0,0,360*(thread_turns+0.2/pitch)]) rotate([180,0,0]) lid();
    else translate([major/2+lid_corners/2+part_gap,0,0]) lid();
}
