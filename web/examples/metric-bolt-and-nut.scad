// ISO metric coarse bolt and nut for light-duty jigs; print on their hex faces.
// M12 is a useful FDM starting point; M3-M5 need fine layers and a small nozzle.
// Radial allowance 0.2 mm gives about 0.1 mm normal clearance at the flanks.
// Use <= pitch/8 layer height. Basic-profile geometry, not a 6g/6H tolerance claim.
size = 12; // Nominal major diameter, mm [3:M3,4:M4,5:M5,6:M6,8:M8,10:M10,12:M12,16:M16,20:M20,24:M24]
thread_turns = 6; // Bolt length in pitches [4:1:12]
nut_turns = 3; // Nut thickness in pitches [3:1:5]
radial_clearance = 0.2; // Added to female radii; diameter allowance is twice this, mm [0.15:0.05:0.3]
head_height = 4; // Thickness of the printable hex head, mm [3:0.5:8]
part_gap = 8; // Clear bed spacing between hex corners, mm [4:1:16]
assembled = false; // Show engaged parts instead of the printable bed layout
resolution = 32; // Profile vertices and slices per turn; multiples of 16 [32:16:64]

// ISO 262 first-choice coarse series (M3-M24):
// https://www.westfieldfasteners.co.uk/Standards/Nut_Hex_M.html
// ISO 68-1:2023 basic profile: https://www.iso.org/standard/85107.html
// H = sqrt(3)*P/2; radial depth = 5H/8; crest = P/8; root flat = P/4.
// Thus d2 = d - 3H/4 and d1 = d - 5H/4. Both flanks are 60 degrees included.
// This is the flat-root BASIC profile, not the rounded external design profile.
/* [Hidden] */
sizes = [3,4,5,6,8,10,12,16,20,24];
pitches = [0.5,0.7,0.8,1,1.25,1.5,1.75,2,2.5,3];
function coarse_pitch(d) =
    let(matches = [for (i=[0:9]) if (sizes[i]==d) pitches[i]])
    assert(len(matches)==1,"Choose a listed metric size") matches[0];
pitch = coarse_pitch(size);
depth = 5*sqrt(3)*pitch/16;
root = size/2-depth;
length = thread_turns*pitch;
nut_height = nut_turns*pitch;
// Utility hexes are deliberately enlarged where needed for >=1.2 mm walls.
hex_flats = max(1.5*size,size+2*radial_clearance+2.8);
hex_corners = hex_flats/cos(30);
$fn = resolution;
assert(resolution >= 32 && resolution <= 64 && resolution%16==0,
       "Use 32, 48 or 64 samples to preserve crest/root flats");
assert(radial_clearance > 0 && radial_clearance < depth*0.8,"Reduce clearance for this fine pitch");
assert(thread_turns >= nut_turns+1,"Bolt needs at least one spare thread turn");
assert(head_height >= pitch/4,"Head must contain the thread overlap");

function thread_radius(a, allowance) = size/2+allowance
    - min(depth,max(0,(abs(a-180)*pitch/360-pitch/16)*sqrt(3)));

// An axial tooth is encoded in polar radius. Negative OpenSCAD twist gives
// a right-hand helix: angle increases by 360 degrees for each +pitch in Z.
module metric_thread(height, allowance=0) {
    linear_extrude(height=height,twist=-360*height/pitch,
                   slices=ceil(resolution*height/pitch-0.000001),convexity=10)
        polygon([for (i=[0:resolution-1])
            let(a=i*360/resolution,r=thread_radius(a,allowance))
            [r*cos(a),r*sin(a)]]);
}

module bolt() {
    union() {
        cylinder(h=head_height,d=hex_corners,$fn=6);
        // Bury a quarter turn in the head, with the matching -90 degree phase.
        translate([0,0,head_height-pitch/4]) rotate([0,0,-90])
            metric_thread(length+pitch/4);
    }
}

module nut() {
    difference() {
        cylinder(h=nut_height,d=hex_corners,$fn=6);
        // Face-aligned cutter preserves clean thread ends in the exact kernel.
        metric_thread(nut_height,radial_clearance);
    }
}

color("#55a9e8") bolt();
color("#e4665f") translate(assembled ? [0,0,head_height+pitch] : [hex_corners+part_gap,0,0]) nut();
