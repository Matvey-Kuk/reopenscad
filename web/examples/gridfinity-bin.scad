// Gridfinity divided storage bin; print feet down, opening up. Millimetres.
// The 45-degree feet and label support print without support material.
// Glue optional 6 x 2 mm magnets into the underside; M3 holes are unthreaded.
grid_x = 2; // Width in 42 mm grid units [1:1:4]
grid_y = 1; // Depth in 42 mm grid units [1:1:4]
height_units = 3; // Body height including feet, in 7 mm units [2:1:8]
wall = 1.6; // Body wall and divider thickness, mm [1.2:0.2:2.4]
stacking_lip = true; // Add a mating lip with a sturdy, shortened lead-in
x_dividers = 1; // Walls dividing the width; zero leaves it undivided [0:1:5]
y_dividers = 0; // Walls dividing the depth; zero leaves it undivided [0:1:5]
label_tab = true; // Add a flat label shelf on the rear inside wall
label_width = 28; // Label shelf width, mm; limited to the bin width [16:1:50]
label_depth = 6; // Label shelf projection, mm; shortened for shallow bins [4:1:8]
magnet_holes = false; // Add four magnet pockets beneath every grid cell
screw_holes = false; // Add coaxial blind 3 mm M3 holes, 5 mm deep
magnet_clearance = 0.4; // Added to 6 mm magnet diameter for glue fit, mm [0.2:0.1:0.4]
magnet_depth = 2.2; // Pocket depth for 2 mm thick magnets, mm [2:0.1:2.4]

// Original dimensional references (implementation written for this example):
// https://gridfinity.xyz/specification/
// https://kennetek.github.io/gridfinity-rebuilt-openscad/constants/
// https://github.com/kennetek/gridfinity-rebuilt-openscad/blob/main/src/core/standard.scad
// Foot: 35.6 -> 37.2 -> 41.5 mm; 0.8/1.8/2.15 mm high stages.
// Body footprint = 42*N - 0.5. The floor is at Z=7, leaving 2 mm
// above the joining slab at Z=5. Short bridges join adjacent rounded feet
// (0.5 mm between straight edges, up to 8 mm around corner junctions).
// Lip follows the 0.7/1.8/1.9 mm nominal profile, with its upper 1.2 mm
// omitted, leaving a 1.2 mm rim. Raise the female profile datum 0.35 mm
// relative to the body height so the foot seats at exactly 7*height_units.
// Overall height with this shortened lead-in is 7*height_units + 3.55 mm.
/* [Hidden] */
eps = 0.02;
width = grid_x*42-0.5;
depth = grid_y*42-0.5;
body_h = height_units*7;
inside_r = 3.75-wall;
lip_h = stacking_lip ? 3.55 : 0;
lip_z = body_h+0.35;
assert(grid_x>=1 && grid_y>=1 && grid_x==floor(grid_x) && grid_y==floor(grid_y),
    "Grid counts must be positive integers");
assert(height_units>=2 && height_units==floor(height_units),"Use at least two height units");
assert(wall>=1.2 && wall<=2.4,"Wall must be 1.2 to 2.4 mm");
assert(x_dividers>=0 && y_dividers>=0 && x_dividers==floor(x_dividers)
    && y_dividers==floor(y_dividers),"Divider counts must be nonnegative integers");
assert((width-2*wall-x_dividers*wall)/(x_dividers+1)>=4
    && (depth-2*wall-y_dividers*wall)/(y_dividers+1)>=4,
    "Leave at least 4 mm inside each compartment");

module cavity() {
    if (stacking_lip)
        // An inward 45-degree transition below the lip avoids a flat overhang.
        rounded_profile(width/2-3.75,depth/2-3.75,
            [[inside_r,7],[inside_r,lip_z-1.2-(inside_r-1.15)],
             [1.15,lip_z-1.2],[1.15,lip_z],[1.85,lip_z+0.7],
             [1.85,lip_z+2.5],[2.55,lip_z+3.2],[2.55,lip_z+3.2+eps]]);
    else translate([0,0,7]) linear_extrude(height=body_h-7+eps)
        rounded_outline(width-2*wall,depth-2*wall,inside_r);
}

module label_shelf() {
    tw = min(label_width,width-2*wall-4);
    // Shorten the shelf in shallow bins so its support stays above the floor.
    td = min(label_depth,body_h-10.2);
    // Local X becomes Y, Y becomes Z and the extrusion runs along X.
    translate([-tw/2,depth/2-wall,body_h-2]) rotate([90,0,90])
        linear_extrude(height=tw)
            polygon([[eps,0],[-td,0],[-td,-1.2],[eps,-td-1.2]]);
}

module gridfinity_bin() {
    difference() {
        union() {
            difference() {
                union() {
                    feet();
                    translate([0,0,5]) linear_extrude(height=body_h+lip_h-5)
                        rounded_outline(width,depth,3.75);
                }
                cavity();
            }
            // Dividers meet the floor and stop below the stacking interface.
            if (x_dividers>0) for (i=[1:x_dividers])
                translate([-width/2+wall+i*(width-2*wall)/(x_dividers+1)-wall/2,
                    -depth/2+wall-eps,7-eps])
                    cube([wall,depth-2*wall+2*eps,body_h-8+eps]);
            if (y_dividers>0) for (i=[1:y_dividers])
                translate([-width/2+wall-eps,
                    -depth/2+wall+i*(depth-2*wall)/(y_dividers+1)-wall/2,7-eps])
                    cube([width-2*wall+2*eps,wall,body_h-8+eps]);
            if (label_tab) label_shelf();
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

color("#55a9e8") gridfinity_bin();
