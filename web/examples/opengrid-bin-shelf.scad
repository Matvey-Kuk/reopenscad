// openGrid tray shelf for Gridfinity bins; all dimensions in mm.
// Print as shown, flat underside at Z=0, open tray upward, without supports.
// Four M3 screws/washers and four directional M3 metric screw snaps required.
// Mount the rear face (Y=0) to the board, arrows on the snaps upward. Lite
// boards are 4 mm, Full boards 6.8 mm: use matching snaps and screw lengths.
// Engagement = screw length - back thickness - washer thickness. M3x6 with
// the default back and a 0.5 mm washer engages 2.5 mm. Check snap thread depth.
// This is a flat shelf with retaining rails, not a Gridfinity locking socket.
// Original geometry from published pitches: 3 x 28 = 2 x 42 = 84 mm.
// openGrid by David D, CC BY 4.0 core parts: https://www.opengrid.world/guides/
// Dimensions: https://www.opengrid.world/guides/board/
// Hardware: https://www.opengrid.world/guides/snaps/
// Katie's library: https://handsonkatie.com/home-organisation-library/?_system=opengrid
// For light bins; physical fit and load capacity are not yet tested.

// Shelf width in 42 mm Gridfinity units (count).
bin_units = 2; // [2:1:4]
// Shelf depth in 42 mm Gridfinity units (count).
depth_units = 1; // [1:1:2]
// Flat shelf thickness (mm).
floor_thickness = 4; // [3:0.5:6]
// Side and front rail thickness (mm).
rail_thickness = 2.4; // [1.6:0.4:4]
// Rail height above the shelf floor (mm).
rail_height = 6; // [4:1:12]
// Back thickness; affects the required screw length (mm).
back_thickness = 3; // [3:0.5:5]
// Bin clearance per side; also extra screw bore diameter (mm).
tolerance = 0.3; // [0.2:0.05:0.4]

/* [Hidden] */
pitch = 28;
bin_pitch = 42;
inner_width = bin_units * bin_pitch + 2 * tolerance;
inner_depth = depth_units * bin_pitch + 2 * tolerance;
width = inner_width + 2 * rail_thickness;
depth = inner_depth + back_thickness + rail_thickness;
hole_z = floor_thickness + rail_height + 8;
back_height = hole_z + pitch + 8;
mount_x = floor((width - 16) / (2 * pitch)) * pitch;
eps = 0.02;

color("#55a9e8") difference() {
    union() {
        translate([-width / 2, 0, 0]) cube([width, depth, floor_thickness]);
        translate([-width / 2, 0, 0]) cube([width, back_thickness, back_height]);
        for (x = [-width / 2, width / 2 - rail_thickness])
            translate([x, 0, 0])
                cube([rail_thickness, depth, floor_thickness + rail_height]);
        translate([-width / 2, depth - rail_thickness, 0])
            cube([width, rail_thickness, floor_thickness + rail_height]);
    }
    // A rotated square bore prints its roof at 45 degrees, without bridging.
    for (x = [-mount_x, mount_x])
        for (z = [hole_z, hole_z + pitch])
            translate([x, back_thickness / 2, z])
                rotate([0, 45, 0])
                    cube([3 + tolerance, back_thickness + 2 * eps,
                          3 + tolerance], center = true);
}
