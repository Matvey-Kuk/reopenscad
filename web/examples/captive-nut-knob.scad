// Hand knob for a real DIN 934 hex nut; print flat, hex pocket facing up.
// Drop in a metal nut after printing and retain with a dab of glue.
// These are DIN 934 heights; ISO 4032 M5/M6/M8 nuts can be taller.
// The nut supplies the durable thread; the pocket prevents it from turning.
size = 6; // Metal nut size [3:M3,4:M4,5:M5,6:M6,8:M8]
knob_diameter = 30; // Grip diameter, mm [24:2:44]
base_thickness = 3; // Solid material below the hex pocket, mm [2:0.5:5]
pocket_clearance = 0.2; // Clearance at EACH hex flat, mm [0.15:0.05:0.3]
hole_clearance = 0.4; // Added to nominal screw DIAMETER, mm [0.3:0.1:0.6]
grip_count = 8; // Number of finger scallops [6:1:12]
grip_depth = 1.2; // Scallop intrusion into the rim, mm [0.6:0.2:2]
nut_recess = 0.3; // Nut top below the knob face, mm [0.2:0.1:0.6]

// DIN 934 dimensions (across flats, thickness), mm:
// https://www.westfieldfasteners.co.uk/Standards/Nut_Hex_M.html
// M3: 5.5,2.4; M4: 7,3.2; M5: 8,4; M6: 10,5; M8: 13,6.5.
/* [Hidden] */
sizes = [3,4,5,6,8];
flats = [5.5,7,8,10,13];
heights = [2.4,3.2,4,5,6.5];
indices = [for (i=[0:4]) if (sizes[i]==size) i];
assert(len(indices)==1,"Choose M3, M4, M5, M6 or M8");
index = indices[0];
pocket_flats = flats[index]+2*pocket_clearance;
pocket_corners = pocket_flats/cos(30);
height = base_thickness+heights[index]+nut_recess;
scallop_radius = 3;
eps = 0.02;
$fn = 64;
assert(base_thickness >= 1.2,"Keep at least 1.2 mm beneath the nut");
assert(pocket_clearance > 0 && hole_clearance > 0,"Clearances must be positive");
assert(size+hole_clearance < flats[index],"Keep a seating ledge under the nut");
assert(knob_diameter/2-grip_depth-pocket_corners/2 >= 2,"Leave 2 mm outside the hex corners");

module hand_knob() {
    difference() {
        cylinder(h=height,d=knob_diameter);
        translate([0,0,base_thickness])
            cylinder(h=height,d=pocket_corners,$fn=6);
        translate([0,0,-eps])
            cylinder(h=height+2*eps,d=size+hole_clearance);
        for (i=[0:grip_count-1]) rotate([0,0,i*360/grip_count])
            translate([knob_diameter/2+scallop_radius-grip_depth,0,-eps])
                cylinder(h=height+2*eps,r=scallop_radius,$fn=24);
    }
}

color("#e4665f") hand_knob();
