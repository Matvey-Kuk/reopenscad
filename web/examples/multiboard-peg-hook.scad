// Multiboard small-hole peg hook for light cables; dimensions in mm.
// Print as shown on its broad side at Z=0, no supports, 4 perimeters.
// In use +Y is up, +X faces away from the board. Hold the hook 1.5 mm high,
// push both pegs through, then lower so the downward return catches the rear.
// Lift before removal. The thin, flat pegs are intended for light cables only.
// Requires at least 6.25 mm behind the board; not for flush-mounted tiles.
// Independent accessory, not a reproduction of a tile, thread or Peg Click.
// Keep Making / MultiBuild: https://docs.multibuild.io/beginner-section/core-parts-documentation
// Official grid: 25 mm. Measured original tile: 6.4 mm thick; small thread
// 6 mm minor / 7 mm major diameter, 3 mm pitch (not a metric screw thread).
// Dimensional reference, not an official tolerance specification:
// https://github.com/asciipip/multiboard-parametric-stacked/blob/master/multiboard_base.scad
// Only dimensions used, no source or model geometry copied.
// Original system licence: https://multibuild.io/license (conditional remixes).
// This hook uses smooth clearance pegs, not the proprietary thread profile.
// Fit, insertion and load capacity require a physical trial print.

// Usable projection from the upright (mm).
reach = 25; // [15:1:40]
// Arm thickness in its bending direction (mm).
arm_thickness = 6; // [4:1:9]
// Front retaining lip above the arm (mm).
lip_height = 7; // [4:1:12]
// Upright thickness against the board (mm).
back_thickness = 4; // [3:0.5:6]
// Actual tile thickness; default is the original octagonal tile (mm).
board_thickness = 6.4; // [6:0.1:8]
// Fit allowance: reduces the pegs and increases the board gap (mm).
tolerance = 0.3; // [0.2:0.05:0.4]

/* [Hidden] */
pitch = 25;
hole_minor = 6;
// Size the ENTIRE return to pass through the throat when shifted upward.
// Its rectangular envelope is (peg_side + return_drop) by peg_side;
// the diagonal equals hole_minor - tolerance, including the return.
return_drop = 3;
peg_side = (sqrt(2 * (hole_minor - tolerance) * (hole_minor - tolerance)
                - return_drop * return_drop) - return_drop) / 2;
gap = board_thickness + tolerance;
lower_y = 10;
upper_y = lower_y + pitch;
tip = back_thickness + reach;
eps = 0.02;

color("#89c454") linear_extrude(height = peg_side)
    union() {
        polygon(points = [
            [0, 0], [tip + arm_thickness, 0],
            [tip + arm_thickness, arm_thickness + lip_height],
            [tip, arm_thickness + lip_height], [tip, arm_thickness],
            [back_thickness + arm_thickness, arm_thickness],
            [back_thickness, 2 * arm_thickness],
            [back_thickness, upper_y + 7], [0, upper_y + 7]
        ]);
        // Short locating peg enters the second small hole on the same column.
        translate([-3, lower_y - peg_side / 2])
            square([3 + eps, peg_side]);
        // Neck through the board and a downward return behind the lower rim.
        translate([-gap - peg_side, upper_y - peg_side / 2])
            square([gap + peg_side + eps, peg_side]);
        translate([-gap - peg_side, upper_y - peg_side / 2 - return_drop])
            square([peg_side, peg_side + return_drop]);
    }
