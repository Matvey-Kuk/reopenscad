// Mixing a 2D child into a 3D union warns and drops the 2D child.
union() { cube([10, 10, 5]); square(4); }
