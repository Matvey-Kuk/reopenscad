// A computed zero step yields an empty range rather than killing the compile.
step = 0;
for (i = [0 : step : 3]) translate([i * 5, 0, 0]) cube(1);
cube([9, 3, 3]);
