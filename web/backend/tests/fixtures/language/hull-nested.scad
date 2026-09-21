// A hull whose child is itself a hull.
$fn = 16;
hull() {
  hull() { sphere(3); translate([8, 0, 0]) sphere(3); }
  translate([4, 12, 0]) sphere(2);
}
