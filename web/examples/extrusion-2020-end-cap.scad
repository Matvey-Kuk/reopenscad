// Slip-over 2020 end cap for V-slot or T-slot; outer sleeve avoids core/slot
// differences. Print closed face on the bed and open sleeve upward; no supports.
// Units: mm. Nominal clearance is 0.3 mm across the whole rail, not per side.
// Push over a deburred free rail end. For a snug fit reduce fit_clearance.
// V-slot throat is 6.25 mm in the cited drawing; B-type is 6 mm.
// The sleeve fits the 20 mm outer envelope, without entering either slot.
// The cap is a cover, not a structural retainer. Sleeve adds to outside width.
// Sources: https://www.motedis.com/en/Profile-20x20-B-type-slot-6
// https://bitfab.parts/wp-content/uploads/2023/11/2020V-20x20mm-V-Slot-Profile-Drawing-v2.pdf

profile_width = 20; // Extrusion outer width, mm [20:0.1:20]
fit_clearance = 0.3; // Total added cavity width, mm [0.2:0.05:0.4]
wall = 1.8; // Sleeve wall thickness, mm [1.8:0.2:3]
face_thickness = 2; // Closed end thickness, mm [1.2:0.2:4]
overlap_length = 8; // Sleeve engagement over the rail, mm [5:1:15]
outer_chamfer = 0.8; // Outside corner cut, mm [0.4:0.2:0.8]
lead_in = 0.4; // Internal mouth chamfer depth, mm [0.2:0.1:0.4]

// These ranges retain >=1.4 mm even at the diagonal mouth corners.
inside = profile_width+fit_clearance;
outside = inside+2*wall;
height = face_thickness+overlap_length;
half = outside/2;

color("#e4665f") difference() {
    linear_extrude(height=height)
        polygon([[-half+outer_chamfer,-half], [half-outer_chamfer,-half],
                 [half,-half+outer_chamfer], [half,half-outer_chamfer],
                 [half-outer_chamfer,half], [-half+outer_chamfer,half],
                 [-half,half-outer_chamfer], [-half,-half+outer_chamfer]]);
    translate([-inside/2,-inside/2,face_thickness])
        cube([inside,inside,overlap_length+1]);
    // Hull of two thin cubes makes a square 45-degree entry chamfer.
    hull() {
        translate([-inside/2,-inside/2,height-lead_in])
            cube([inside,inside,0.01]);
        translate([-inside/2-lead_in,-inside/2-lead_in,height])
            cube([inside+2*lead_in,inside+2*lead_in,0.01]);
    }
}
