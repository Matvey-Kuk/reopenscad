// Standalone openGrid-compatible N x M board; all dimensions in mm.
// Print flat as shown, Z=0 on the bed, snap-entry face upward, no supports.
// Full = 6.8 mm, Lite = 4 mm (upper 4 mm of the Full seat), on a 28 mm grid.
// Use matching Full/Lite mounting snaps to screw this board to the wall.
// There are no direct screw holes or edge-joining sockets. The strengthened
// rim adds `rim` (1.2 mm) per side: overall 28*N + 2*rim by 28*M + 2*rim mm.
// Do not butt these rims together and expect an uninterrupted 28 mm grid.
// Original implementation from dimensional data; no generator code or meshes
// incorporated. openGrid design: David D, https://www.opengrid.world/guides/
// Design and this model: CC BY 4.0, https://creativecommons.org/licenses/by/4.0/
// Dimensional source: BlackjackDuck / Andy Levesque's published openGrid model:
// https://github.com/AndyLevesque/QuackWorks/blob/main/openGrid/openGrid.scad
// That generator's CODE is CC BY-NC-SA; it has not been copied here.
// Changes from the core board: solid stronger perimeter, no connector sockets,
// cosmetic corner cuts or direct screw holes. Physical snap fit needs a trial.

// Number of snap columns on the 28 mm grid (count).
columns = 3; // [1:1:6]
// Number of snap rows on the 28 mm grid (count).
rows = 2; // [1:1:6]
// Board and snap family; Lite is not a scaled Full board.
board_type = "Lite"; // [Lite, Full]
// Solid strengthened rim added around the grid on each side.
rim = 1.2; // mm [1.2:0.2:4]

/* [Hidden] */
pitch = 28;
thickness = board_type == "Full" ? 6.8 : 4;
cut_bottom = 6.8 - thickness;
eps = 0.01;
// Height stations in the Full board; opening widths across opposite flats.
seat_z = [0, 0.4, 1.4, 2.4, 4.4, 5.4, 6.4, 6.8];
seat_width = [25.8, 25, 25, 26.4, 26.4, 25, 25, 25.8];
// Corner material extends 2.6 mm beyond a 4.2 mm diagonal intersection.
corner_inset = 2.6 + 4.2 / sqrt(2);

function diagonal_width(z) =
    pitch * sqrt(2) - 2 * (corner_inset - max(0, 1.4 - min(z, 6.8 - z)));

// A tapered square clipped by another at 45 degrees makes the octagonal seat.
// The two widths vary independently, preserving both the snap ledge and corners.
module opening_segment(z0, z1, w0, w1, d0, d1) {
    translate([0, 0, z0 - cut_bottom])
        intersection() {
            linear_extrude(height = z1 - z0, scale = w1 / w0)
                square([w0, w0], center = true);
            rotate([0, 0, 45])
                linear_extrude(height = z1 - z0, scale = d1 / d0)
                    square([d0, d0], center = true);
        }
}

module snap_opening() {
    // End caps extend the cutter beyond the slab, avoiding coplanar end faces.
    translate([0, 0, -cut_bottom - eps])
        linear_extrude(height = eps)
            intersection() {
                square([seat_width[0], seat_width[0]], center = true);
                rotate(45) square([diagonal_width(0), diagonal_width(0)], center = true);
            }
    for (i = [0:len(seat_z) - 2])
        opening_segment(seat_z[i], seat_z[i + 1], seat_width[i], seat_width[i + 1],
                        diagonal_width(seat_z[i]), diagonal_width(seat_z[i + 1]));
    translate([0, 0, thickness])
        linear_extrude(height = eps)
            intersection() {
                square([seat_width[7], seat_width[7]], center = true);
                rotate(45) square([diagonal_width(6.8), diagonal_width(6.8)], center = true);
            }
}

color("#5ccfc0") difference() {
    translate([-rim, -rim, 0])
        cube([columns * pitch + 2 * rim, rows * pitch + 2 * rim, thickness]);
    for (x = [0:columns - 1])
        for (y = [0:rows - 1])
            translate([(x + 0.5) * pitch, (y + 0.5) * pitch, 0]) snap_opening();
}
