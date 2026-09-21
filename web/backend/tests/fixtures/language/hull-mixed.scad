// Mixed primitive kinds in one hull.
$fn = 20;
hull() { cube([8, 8, 2], center = true); translate([0, 0, 9]) sphere(3); }
