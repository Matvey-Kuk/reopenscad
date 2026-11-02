// Gridfinity baseplate; print flat backing down, sockets facing up. Millimetres.
// A closed backing keeps it rigid and supports optional glued 6 x 2 mm magnets.
// Magnet pockets open into the sockets; check polarity before gluing.
grid_x = 2; // Number of 42 mm columns [1:1:5]
grid_y = 2; // Number of 42 mm rows [1:1:5]
magnet_pockets = false; // Add four upward-facing magnet pockets in each cell
screw_holes = false; // Add coaxial 3.2 mm through holes with countersinks for M3 screws
backing = 1.6; // Minimum solid backing below the sockets or magnets, mm [1.2:0.2:3]
magnet_clearance = 0.4; // Added to 6 mm magnet diameter for glue fit, mm [0.2:0.1:0.4]
magnet_depth = 2.2; // Pocket depth for 2 mm thick magnets, mm [2:0.1:2.4]

// Original dimensional references (implementation written for this example):
// https://gridfinity.xyz/specification/
// https://kennetek.github.io/gridfinity-rebuilt-openscad/constants/
// https://github.com/kennetek/gridfinity-rebuilt-openscad/blob/main/src/core/standard.scad
// https://github.com/kennetek/gridfinity-rebuilt-openscad/blob/main/src/core/gridfinity-baseplate.scad
// Socket: 36.3 -> 37.7 -> 42 mm, radii 1.15 -> 1.85 -> 4 mm.
// The standard 5 mm socket has 0.35 mm relief below its 0.7/1.8/2.15 profile.
// Our added backing is below that datum, preserving the mating geometry.
// Seat the bin foot bottom at socket_z; its joining slab starts 5 mm above it.
/* [Hidden] */
eps = 0.02;
socket_z = backing + (magnet_pockets ? magnet_depth : 0) + (screw_holes ? 1.6 : 0);
plate_height = socket_z+5;
assert(grid_x>=1 && grid_y>=1 && grid_x==floor(grid_x) && grid_y==floor(grid_y),
    "Grid counts must be positive integers");
assert(backing>=1.2,"Use at least 1.2 mm of solid backing");

module socket() {
    rounded_profile(17,17,[[1.15,0],[1.15,0.35],[1.85,1.05],
        [1.85,2.85],[4,5],[4,5+eps]]);
}

module gridfinity_baseplate() {
    difference() {
        // Square perimeter tiles exactly on the grid. The backing supports
        // the intentionally tapered socket ridges at the top of the plate.
        translate([-grid_x*21,-grid_y*21,0])
            cube([grid_x*42,grid_y*42,plate_height]);
        for (ix=[0:grid_x-1], iy=[0:grid_y-1])
            translate([(ix-(grid_x-1)/2)*42,(iy-(grid_y-1)/2)*42,0]) {
                translate([0,0,socket_z]) socket();
                for (sx=[-1,1], sy=[-1,1]) translate([sx*13,sy*13,0]) {
                    if (magnet_pockets)
                        translate([0,0,socket_z-magnet_depth])
                            cylinder(h=magnet_depth+eps,d=6+magnet_clearance,$fn=24);
                    if (screw_holes) {
                        translate([0,0,-eps])
                            cylinder(h=socket_z+2*eps,d=3.2,$fn=20);
                        // A 90-degree countersunk head sits flush below the
                        // socket floor, or below the magnet when both are used.
                        translate([0,0,socket_z-(magnet_pockets ? magnet_depth : 0)-1.6])
                            cylinder(h=1.6+eps,d1=3.2,d2=6.4+2*eps,$fn=24);
                    }
                }
            }
    }
}

// Fill a rounded rectangle from radius/height stations. Corner centres stay
// fixed: changing radius by the same amount as Z makes a true 45-degree slope.
// Two extruded sections and four turned corners avoid hulls and external libs.
module rounded_profile(cx, cy, stations) {
    union() {
        rotate([90,0,0]) linear_extrude(height=2*cy, center=true)
            polygon(concat(
                [for (p=stations) [cx+p[0],p[1]]],
                [for (i=[len(stations)-1:-1:0])
                    [-cx-stations[i][0],stations[i][1]]]));
        rotate([90,0,90]) linear_extrude(height=2*cx, center=true)
            polygon(concat(
                [for (p=stations) [cy+p[0],p[1]]],
                [for (i=[len(stations)-1:-1:0])
                    [-cy-stations[i][0],stations[i][1]]]));
        for (x=[-cx,cx], y=[-cy,cy]) translate([x,y,0])
            rotate_extrude($fn=24)
                polygon(concat([[0,stations[0][1]]], stations,
                    [[0,stations[len(stations)-1][1]]]));
    }
}

color("#d9dde2") gridfinity_baseplate();
