// A recursive module 120 deep. OpenSCAD handles hundreds; this engine capped
// at 16, which rejects the ordinary recursive-tree idiom outright.
module tower(n) { if (n > 0) { translate([0, 0, n]) cube(1); tower(n - 1); } }
tower(120);
