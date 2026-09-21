// projection(cut = true) takes the slice at z = 0 instead of the silhouette.
$fn = 28;
linear_extrude(2) projection(cut = true)
  translate([0, 0, -5]) difference() {
    cylinder(h = 20, r = 10);
    cylinder(h = 21, r = 6);
  }
