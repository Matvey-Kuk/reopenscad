// 1/4-20 UNC female mounting plate for a phone cradle or small camera accessory.
// Print flat as shown; the tripod screw enters the centre from below.
// Radial allowance 0.2 mm gives about 0.1 mm normal clearance at the flanks.
// Attach your cradle through the two M3 clearance holes. Use 0.10-0.15 mm layers.
plate_length = 44; // Plate length along mounting holes, mm [36:2:70]
plate_width = 24; // Plate width, mm [18:2:40]
thread_turns = 5; // Plate thickness in 1.27 mm pitches [4:1:7]
mount_spacing = 28; // Centre distance between the two M3 holes, mm [20:2:54]
mount_hole = 3.4; // M3 screw clearance hole diameter, mm [3.2:0.1:4]
radial_clearance = 0.2; // Female radial allowance; adds twice this to diameters, mm [0.15:0.05:0.3]
corner_cut = 3; // Straight 45-degree corner chamfer, mm [1:0.5:5]
resolution = 48; // Profile vertices and slices per turn [32:16:64]

// NBS Circular 479, Unified profile and coarse-series Table 2:
// https://nvlpubs.nist.gov/nistpubs/Legacy/circ/nbscircular479.pdf
// 1/4 inch = 6.35 mm; 20 TPI = 1.27 mm pitch; included angle = 60 degrees.
// Basic pitch diameter 5.525111 mm; internal minor diameter 4.975185 mm.
// Printing allowance is additional; this does not claim a gauged 2B fit.
/* [Hidden] */
major = 25.4/4;
pitch = 25.4/20;
depth = 5*sqrt(3)*pitch/16;
root = major/2-depth;
plate_thickness = thread_turns*pitch;
x = plate_length/2;
y = plate_width/2;
$fn = resolution;
assert(resolution >= 32 && resolution <= 64 && resolution%16==0,
       "Use 32, 48 or 64 samples to preserve crest/root flats");
assert(radial_clearance > 0 && radial_clearance < depth/2,"Keep thread engagement");
assert(corner_cut > 0 && corner_cut < min(x,y),"Chamfer must fit the plate");
assert(mount_spacing > major+2*radial_clearance+mount_hole+2.4,
       "Leave 1.2 mm between mounting holes and the thread");
assert(plate_length >= mount_spacing+mount_hole+4,"Leave 2 mm beyond each mounting hole");
assert(plate_width >= major+2*radial_clearance+4,"Plate too narrow");

function unc_radius(a) = major/2+radial_clearance
    - min(depth,max(0,(abs(a-180)*pitch/360-pitch/16)*sqrt(3)));

module unc_cutter(height) {
    linear_extrude(height=height,twist=-360*height/pitch,
                   slices=ceil(resolution*height/pitch),convexity=10)
        polygon([for (i=[0:resolution-1])
            let(a=i*360/resolution,r=unc_radius(a)) [r*cos(a),r*sin(a)]]);
}

module tripod_plate() {
    difference() {
        linear_extrude(height=plate_thickness)
            polygon([[-x+corner_cut,-y],[x-corner_cut,-y],[x,-y+corner_cut],
                     [x,y-corner_cut],[x-corner_cut,y],[-x+corner_cut,y],
                     [-x,y-corner_cut],[-x,-y+corner_cut]]);
        // A small phase offset avoids coincident cutter and plate facet planes.
        rotate([0,0,3]) unc_cutter(plate_thickness);
        for (side=[-1,1]) translate([side*mount_spacing/2,0,0])
            cylinder(h=plate_thickness,d=mount_hole,$fn=32);
    }
}

color("#89c454") tripod_plate();
