// 75-to-100 mm VESA adapter plate; print flat with the ribs facing up.
// For light accessories; check fastener length before attaching equipment.
plate_size = 124; // Overall width, mm [116:2:150]
plate_thickness = 4; // Plate thickness, mm [3:0.5:8]
small_spacing = 75; // Inner mounting pattern, mm [60:5:90]
large_spacing = 100; // Outer mounting pattern, mm [95:5:120]
hole_diameter = 4.6; // M4 screw clearance, mm [4.4:0.1:5]
corner_cut = 8; // Corner chamfer, mm [4:1:12]
window_size = 42; // Central cable opening, mm [20:2:50]
rib_width = 6; // Reinforcement width, mm [4:1:10]
rib_height = 2; // Reinforcement height, mm [1.2:0.2:4]
$fn = 48; // Circular resolution
cut_extra = 1; // Boolean cutter overlap, mm

module chamfered_square(size, chamfer) {
    half = size / 2;
    polygon([[-half+chamfer,-half], [half-chamfer,-half],
             [half,-half+chamfer], [half,half-chamfer],
             [half-chamfer,half], [-half+chamfer,half],
             [-half,half-chamfer], [-half,-half+chamfer]]);
}

module mounting_holes(spacing) {
    for (x = [-spacing/2, spacing/2])
        for (y = [-spacing/2, spacing/2])
            translate([x,y,-cut_extra])
                cylinder(h=plate_thickness+rib_height+2*cut_extra, d=hole_diameter);
}

difference() {
    union() {
        linear_extrude(plate_thickness) chamfered_square(plate_size,corner_cut);
        for (angle = [45,135])
            rotate([0,0,angle])
                translate([-plate_size/2,-rib_width/2,plate_thickness-cut_extra])
                    cube([plate_size,rib_width,rib_height+cut_extra]);
    }
    translate([0,0,-cut_extra])
        linear_extrude(plate_thickness+rib_height+2*cut_extra)
            chamfered_square(window_size,corner_cut/2);
    mounting_holes(small_spacing);
    mounting_holes(large_spacing);
}
