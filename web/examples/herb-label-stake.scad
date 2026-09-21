// Raised-text herb label with an integral garden stake; print lettering up.
// The wide shoulder and blunt spear tip resist bending while pushing into soil.
label = "BASIL"; // Plant name
font_name = "Liberation Sans:style=Bold"; // Font family and style
label_width = 64; // Sign width, mm [56:2:90]
label_height = 26; // Sign height, mm [24:2:36]
stake_length = 75; // Stake length below sign, mm [55:5:110]
stake_width = 12; // Stake shaft width, mm [10:1:16]
tip_length = 15; // Spear tip length, mm [10:1:20]
tip_width = 2.4; // Blunt tip width, mm [1.2:0.2:4]
thickness = 2.4; // Plate and stake thickness, mm [1.6:0.2:4]
letter_size = 12; // Text size, mm [10:1:16]
letter_height = 1.2; // Raised letter thickness, mm [1.2:0.2:2]
corner = 4; // Label corner chamfer, mm [3:1:6]
shoulder_length = 10; // Stake shoulder transition, mm [8:1:16]
cut_extra = 0.1; // Letter overlap, mm

module label_outline() {
    polygon([[-label_width/2+corner,0],[label_width/2-corner,0],
             [label_width/2,corner],[label_width/2,label_height-corner],
             [label_width/2-corner,label_height],[-label_width/2+corner,label_height],
             [-label_width/2,label_height-corner],[-label_width/2,corner]]);
}

module stake_outline() {
    polygon([[-stake_width,cut_extra],[stake_width,cut_extra],
             [stake_width/2,-shoulder_length],
             [stake_width/2,-stake_length+tip_length],[tip_width/2,-stake_length],
             [-tip_width/2,-stake_length],[-stake_width/2,-stake_length+tip_length],
             [-stake_width/2,-shoulder_length]]);
}

union() {
    linear_extrude(thickness) union() {
        label_outline();
        stake_outline();
    }
    translate([0,label_height/2,thickness-cut_extra])
        linear_extrude(letter_height+cut_extra)
            text(label,size=letter_size,font=font_name,halign="center",valign="center");
}
