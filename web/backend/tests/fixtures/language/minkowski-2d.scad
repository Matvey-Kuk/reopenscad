// A 2D minkowski: the classic rounded-rectangle idiom.
$fn = 24;
linear_extrude(3) minkowski() { square([14, 8]); circle(2); }
