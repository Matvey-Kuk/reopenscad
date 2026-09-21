// The slot idiom: two cylinders hulled into a rounded rectangle.
$fn = 32;
hull() { cylinder(h = 4, r = 5); translate([16, 0, 0]) cylinder(h = 4, r = 5); }
