// fill() closes the holes in a 2D region, so a washer becomes a disc.
$fn = 32;
linear_extrude(3) fill() difference() { circle(10); circle(6); }
