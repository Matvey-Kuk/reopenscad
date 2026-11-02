// Gridfinity quarter-inch hex-bit holder; print feet down, sockets facing up.
// Blind hex sockets hold 6.35 mm shanks; 25 mm bits protrude 17.2 mm by default.
// Print with normal infill. This solid insert has no upper stacking lip.
// Counts and insertion depth are capped to preserve printable webs and floors.
grid_x = 1; // Width in 42 mm grid units [1:1:3]
grid_y = 1; // Depth in 42 mm grid units [1:1:3]
height_units = 2; // Overall height including feet, in 7 mm units [2:1:5]
bit_columns = 3; // Requested sockets across width; capped to leave 1.2 mm webs [1:1:9]
bit_rows = 3; // Requested sockets across depth; capped to leave 1.2 mm webs [1:1:9]
bit_flats = 6.35; // Bit shank size measured across opposite flats, mm [4:0.05:8]
bit_clearance = 0.3; // Added to the socket across-flats size, mm [0.2:0.05:0.4]
socket_depth = 7.8; // Bit insertion depth, mm; limited to preserve the base [4:0.2:20]
magnet_holes = false; // Add four underside pockets per cell for 6 x 2 mm magnets
screw_holes = false; // Add coaxial blind 3 mm M3 holes, 5 mm deep
magnet_clearance = 0.4; // Added to 6 mm magnet diameter for glue fit, mm [0.2:0.1:0.4]
magnet_depth = 2.2; // Pocket depth for 2 mm thick magnets, mm [2:0.1:2.4]

// Original dimensional references (implementation written for this example):
// https://gridfinity.xyz/specification/
// https://kennetek.github.io/gridfinity-rebuilt-openscad/constants/
// https://github.com/kennetek/gridfinity-rebuilt-openscad/blob/main/src/core/standard.scad
// Each cell has the standard 0.8/1.8/2.15 mm stepped foot and 26 mm
// fastener spacing. Hole centres are distributed evenly inside a 5 mm margin.
/* [Hidden] */
eps = 0.02;
width = grid_x*42-0.5;
depth = grid_y*42-0.5;
body_h = height_units*7;
hole_d = (bit_flats+bit_clearance)/cos(30);
entry_d = (bit_flats+bit_clearance+0.8)/cos(30);
actual_columns = min(bit_columns,max(1,floor((width-10)/(entry_d+1.2))));
actual_rows = min(bit_rows,max(1,floor((depth-10)/(entry_d+1.2))));
step_x = (width-10)/actual_columns;
step_y = (depth-10)/actual_rows;
// Retain 1.2 mm above the joining slab and optional blind screw holes.
actual_depth = min(socket_depth,body_h-6.2);
assert(grid_x>=1 && grid_y>=1 && grid_x==floor(grid_x) && grid_y==floor(grid_y),
    "Grid counts must be positive integers");
assert(height_units>=2 && height_units==floor(height_units),"Use at least two height units");
assert(bit_columns>=1 && bit_rows>=1 && bit_columns==floor(bit_columns)
    && bit_rows==floor(bit_rows),"Socket counts must be positive integers");
assert(min(step_x,step_y)>=entry_d+1.2,"Use fewer sockets or a larger grid: leave 1.2 mm webs");
assert(socket_depth>=4,"Socket depth must be at least 4 mm");
if (actual_columns<bit_columns || actual_rows<bit_rows || actual_depth<socket_depth)
    echo("Effective sockets (columns, rows, depth mm):",
        actual_columns,actual_rows,actual_depth);

module gridfinity_hex_bit_holder() {
    difference() {
        union() {
            feet();
            translate([0,0,5]) linear_extrude(height=body_h-5)
                rounded_outline(width,depth,3.75);
        }
        for (ix=[0:actual_columns-1], iy=[0:actual_rows-1])
            translate([(ix-(actual_columns-1)/2)*step_x,
                       (iy-(actual_rows-1)/2)*step_y,body_h-actual_depth]) {
                cylinder(h=actual_depth+eps,d=hole_d,$fn=6);
                // A 0.4 mm 45-degree lead-in eases insertion at each flat.
                translate([0,0,actual_depth-0.4])
                    cylinder(h=0.4+eps,d1=hole_d,
                        d2=entry_d+2*eps/cos(30),$fn=6);
            }
        fastener_holes();
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

// Six chords per quarter-circle: small files, with real rounded corners.
module rounded_outline(w, d, r) {
    polygon([for (q=[0:3], j=[0:6])
        let(a=q*90+j*15,
            x=(q==0 || q==3) ? w/2-r : -w/2+r,
            y=q<2 ? d/2-r : -d/2+r)
        [x+r*cos(a),y+r*sin(a)]]);
}

// A 0.25 mm vertical collar keeps the joining slab above the 5 mm socket
// ridges. The standard 4.75 mm stepped mating profile stays unchanged.
module feet() {
    for (ix=[0:grid_x-1], iy=[0:grid_y-1])
        translate([(ix-(grid_x-1)/2)*42,(iy-(grid_y-1)/2)*42,0])
            rounded_profile(17,17,
                [[0.8,0],[1.6,0.8],[1.6,2.6],[3.75,4.75],[3.75,5]]);
}

// Four positions per cell, +/-13 mm: 26 mm centre spacing in both axes.
module fastener_holes() {
    for (ix=[0:grid_x-1], iy=[0:grid_y-1], sx=[-1,1], sy=[-1,1])
        translate([(ix-(grid_x-1)/2)*42+sx*13,
                   (iy-(grid_y-1)/2)*42+sy*13,-eps]) {
            if (magnet_holes)
                cylinder(h=magnet_depth+eps,d=6+magnet_clearance,$fn=24);
            if (screw_holes)
                cylinder(h=5+eps,d=3,$fn=20);
        }
}

color("#e4665f") gridfinity_hex_bit_holder();
