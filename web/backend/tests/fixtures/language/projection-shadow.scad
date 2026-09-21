// projection() flattens the silhouette; extruded so it is a solid to measure.
$fn = 28;
linear_extrude(2) projection()
  rotate([20, 0, 0]) cylinder(h = 20, r1 = 8, r2 = 3);
