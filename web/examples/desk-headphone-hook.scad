// Screw-on desk-edge headphone hook; print on its broad side as laid out.
// The teardrop screw bores print without support; use two 4 mm wood screws.
reach = 44; // Hook projection from desk, mm [35:1:60]
height = 52; // Mounting flange height, mm [45:1:70]
width = 20; // Headband bearing width, mm [16:1:30]
wall = 6; // Hook and flange thickness, mm [4:0.5:8]
lip_height = 16; // Retaining lip height, mm [12:1:22]
gusset = 18; // Brace length, mm [12:1:24]
screw_diameter = 4.6; // Screw clearance, mm [4.2:0.2:5]
screw_spacing = 14; // Screw centre spacing, mm [12:1:18]
top_margin = 8; // Upper screw edge margin, mm [7:1:12]
$fn = 48; // Bore resolution
cut_extra = 1; // Cutter overlap, mm

module hook_outline() {
    polygon([[0,0],[reach,0],[reach,lip_height],
             [reach-wall,lip_height],[reach-wall,wall],
             [wall+gusset,wall],[wall,wall+gusset],
             [wall,height],[0,height]]);
}

module screw_bore(y) {
    radius = screw_diameter/2;
    translate([-cut_extra,y,width/2])
        rotate([90,0,90]) linear_extrude(wall+2*cut_extra)
            union() {
                circle(r=radius);
                polygon([[-radius/sqrt(2),radius/sqrt(2)],
                         [0,radius*sqrt(2)],
                         [radius/sqrt(2),radius/sqrt(2)]]);
            }
}

difference() {
    linear_extrude(width) hook_outline();
    for (y = [height-top_margin-screw_spacing,height-top_margin])
        screw_bore(y);
}
