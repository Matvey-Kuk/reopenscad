// Personalised chamfered nameplate with two countersunk screw holes.
// Print face up; change filament at the lettering layer for contrasting text.
label = "MAKER"; // Text on the plate
font_name = "Liberation Sans:style=Bold"; // Font family and style
length = 110; // Overall plate length, mm [90:2:160]
width = 30; // Overall plate width, mm [26:2:44]
plate_thickness = 3; // Solid backing thickness, mm [2:0.2:5]
letter_height = 1.2; // Raised lettering height, mm [1.2:0.2:2]
letter_size = 12; // Text size, mm [10:1:18]
corner = 4; // Corner chamfer, mm [3:1:6]
screw_diameter = 4.4; // Screw clearance diameter, mm [3.4:0.2:5]
screw_head_diameter = 8; // Countersink mouth, mm [6:0.5:9]
screw_margin = 8; // Hole centres from the plate ends, mm [7:1:12]
$fn = 48; // Hole resolution
cut_extra = 0.1; // Boolean overlap, mm
countersink_depth = (screw_head_diameter-screw_diameter)/2; // 45-degree countersink, mm

module plate_outline() {
    polygon([[-length/2+corner,-width/2],[length/2-corner,-width/2],
             [length/2,-width/2+corner],[length/2,width/2-corner],
             [length/2-corner,width/2],[-length/2+corner,width/2],
             [-length/2,width/2-corner],[-length/2,-width/2+corner]]);
}

module mounting_hole(x) {
    translate([x,0,-cut_extra])
        cylinder(h=plate_thickness+2*cut_extra,d=screw_diameter);
    translate([x,0,plate_thickness-countersink_depth])
        cylinder(h=countersink_depth+cut_extra,
                 r1=screw_diameter/2,r2=screw_head_diameter/2+cut_extra);
}

union() {
    difference() {
        linear_extrude(plate_thickness) plate_outline();
        for (side=[-1,1]) mounting_hole(side*(length/2-screw_margin));
    }
    translate([0,0,plate_thickness-cut_extra])
        linear_extrude(letter_height+cut_extra)
            text(label,size=letter_size,font=font_name,halign="center",valign="center");
}
