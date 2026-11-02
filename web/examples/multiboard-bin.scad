// Multiboard bolt-on small-parts bin; dimensions in mm.
// Print upright as shown, base at Z=0, opening up, no supports.
// Use two official Small Thread plain-head bolts through the rear holes into
// the board's SMALL holes, not standard M7 screws. Use washers at least 12 mm
// OD with a bore clearing 7.4 mm. Engagement = bolt length - back - washer:
// a 9 mm bolt, default 3 mm back and 1 mm washer engage 5 mm, not 6 mm.
// Choose length for the actual tile and washer; keep tips clear of the wall.
// Independent bin with clearance holes: no tiles or thread geometry reproduced.
// Multiboard by Keep Making / MultiBuild: 25 mm grid, threaded large/small holes.
// https://docs.multibuild.io/beginner-section/core-parts-documentation
// Small thread: measured 7 mm major, 6 mm minor, 3 mm pitch; bores clear 7 mm.
// Dimensional reference (original 6.4 mm tiles, not an official tolerance spec):
// https://github.com/asciipip/multiboard-parametric-stacked/blob/master/multiboard_base.scad
// Original system's custom licence: https://multibuild.io/license
// No downloaded model or code incorporated. Physical fit/load not yet tested.

// Outside bin width in 25 mm Multiboard units (count).
width_units = 3; // [2:1:6]
// Outside depth, from the rear face (mm).
bin_depth = 45; // [30:5:80]
// Height of the open bin rim above the bed (mm).
bin_height = 35; // [20:5:70]
// Side, front and floor thickness (mm).
wall = 2.4; // [1.6:0.4:4]
// Rear wall thickness; adjust bolt length with this (mm).
back_thickness = 3; // [2.4:0.2:4]
// Extra diametral clearance around the small-thread bolt (mm).
tolerance = 0.3; // [0.2:0.05:0.4]

/* [Hidden] */
pitch = 25;
thread_major = 7;
width = width_units * pitch;
rear_height = bin_height + pitch;
mount_z = bin_height + pitch / 2;
mount_x = (width - pitch) / 2;
eps = 0.02;

color("#b48ef0") difference() {
    union() {
        translate([-width / 2, 0, 0]) cube([width, bin_depth, bin_height]);
        translate([-width / 2, 0, 0]) cube([width, back_thickness, rear_height]);
    }
    // Open cavity leaves a full floor and the thicker mounting back.
    translate([-width / 2 + wall, back_thickness, wall])
        cube([width - 2 * wall, bin_depth - back_thickness - wall,
              bin_height + eps]);
    // Diamond openings fully clear round bolts; roofs print at 45 degrees.
    for (x = [-mount_x, mount_x])
        translate([x, back_thickness / 2, mount_z])
            rotate([0, 45, 0])
                cube([thread_major + tolerance, back_thickness + 2 * eps,
                      thread_major + tolerance], center = true);
}
