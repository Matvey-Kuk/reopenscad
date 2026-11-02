// openGrid screw-snap hook for cables and light hand tools; dimensions in mm.
// Print as shown, broad side at Z=0, no supports; 4 perimeters recommended.
// Install with +Y upward and +X away from the board. Two M3 directional metric
// screw snaps, 28 mm apart vertically, and two M3 screws/washers are required.
// Use Lite snaps for a 4 mm board, Full snaps for a 6.8 mm board, arrows up.
// Engagement = screw length - back thickness - washer thickness; M3x6 with
// the default back and a 0.5 mm washer engages 2.5 mm. Check the snap's actual
// thread depth; do not let screw tips bottom out or press against the wall.
// Original accessory geometry; no downloaded meshes or snap code incorporated.
// openGrid by David D: CC BY 4.0 core parts, https://www.opengrid.world/guides/
// Interface: https://www.opengrid.world/guides/snaps/
// Ecosystem: https://handsonkatie.com/home-organisation-library/?_system=opengrid
// Fit and load capacity require a physical trial print.

// Usable reach in front of the backplate (mm).
reach = 32; // [20:2:60]
// Width of the hook, also its printed height (mm).
hook_width = 12; // [10:1:20]
// Arm thickness in its bending direction (mm).
arm_thickness = 6; // [4:1:10]
// Retaining lip above the arm (mm).
lip_height = 8; // [4:1:16]
// Backplate thickness; add this to screw engagement length (mm).
back_thickness = 3; // [3:0.5:5]
// Extra diametral clearance for the M3 screw shanks (mm).
tolerance = 0.3; // [0.2:0.05:0.4]

/* [Hidden] */
pitch = 28;
screw_diameter = 3;
lower_hole = 2 * arm_thickness + lip_height + 6;
back_height = lower_hole + pitch + 8;
tip = back_thickness + reach;
eps = 0.02;

// One extruded profile keeps the arm continuous with the upright and lip.
color("#f0965a") difference() {
    linear_extrude(height = hook_width)
        polygon(points = [
            [0, 0], [tip + arm_thickness, 0],
            [tip + arm_thickness, arm_thickness + lip_height],
            [tip, arm_thickness + lip_height], [tip, arm_thickness],
            [back_thickness + arm_thickness, arm_thickness],
            [back_thickness, 2 * arm_thickness],
            [back_thickness, back_height], [0, back_height]
        ]);
    // Diamond bores enclose a round M3 screw and have 45-degree roofs.
    for (y = [lower_hole, lower_hole + pitch])
        translate([back_thickness / 2, y, hook_width / 2])
            rotate([45, 0, 0])
                cube([back_thickness + 2 * eps,
                      screw_diameter + tolerance,
                      screw_diameter + tolerance], center = true);
}
