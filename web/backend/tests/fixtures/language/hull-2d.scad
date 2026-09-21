// A 2D hull, extruded. OpenSCAD hulls 2D children into a 2D region.
$fn = 32;
linear_extrude(3) hull() { circle(4); translate([12, 0]) circle(2); }
