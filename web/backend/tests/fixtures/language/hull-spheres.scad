// hull() over curved solids. OpenSCAD hulls any 3D children; this engine
// historically accepted only boxes.
$fn = 24;
hull() { sphere(5); translate([14, 0, 0]) sphere(3); }
