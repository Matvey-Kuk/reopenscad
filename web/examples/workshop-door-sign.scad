// Two-line hanging door sign with raised lettering and a chamfered border.
// Print face up; the paired top holes accept cord or small mounting screws.
title = "WORKSHOP"; // Main line
subtitle = "COME IN"; // Secondary line
font_name = "Liberation Sans:style=Bold"; // Lettering font
length = 150; // Overall sign width, mm [130:5:200]
height = 64; // Overall sign height, mm [56:2:90]
backing = 2.4; // Backplate thickness, mm [1.6:0.2:4]
relief = 1.2; // Border and lettering height, mm [1.2:0.2:2]
border_width = 1.6; // Border thickness in plan, mm [1.2:0.2:3]
corner = 8; // Plan corner chamfer, mm [6:1:12]
title_size = 14; // Main lettering size, mm [12:1:18]
subtitle_size = 12; // Secondary lettering size, mm [10:1:16]
line_spacing = 22; // Line centre separation, mm [20:1:30]
hanger_diameter = 4.6; // Hanging hole diameter, mm [3.6:0.2:6]
hanger_margin = 12; // Hole centres from top and side edges, mm [10:1:16]
$fn = 48; // Hole resolution
cut_extra = 0.1; // Boolean overlap, mm

module sign_outline() {
    polygon([[-length/2+corner,-height/2],[length/2-corner,-height/2],
             [length/2,-height/2+corner],[length/2,height/2-corner],
             [length/2-corner,height/2],[-length/2+corner,height/2],
             [-length/2,height/2-corner],[-length/2,-height/2+corner]]);
}

module raised_words(words,size,y) {
    translate([0,y,backing-cut_extra])
        linear_extrude(relief+cut_extra)
            text(words,size=size,font=font_name,halign="center",valign="center");
}

union() {
    difference() {
        linear_extrude(backing) sign_outline();
        for (side=[-1,1])
            translate([side*(length/2-hanger_margin),height/2-hanger_margin,-cut_extra])
                cylinder(h=backing+2*cut_extra,d=hanger_diameter);
    }
    translate([0,0,backing-cut_extra])
        linear_extrude(relief+cut_extra) difference() {
            sign_outline();
            offset(delta=-border_width) sign_outline();
        }
    raised_words(title,title_size,line_spacing/2);
    raised_words(subtitle,subtitle_size,-line_spacing/2);
}
