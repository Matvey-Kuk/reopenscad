// render() is a caching hint, not geometry: the result must equal the union.
render() { cube([10,10,4], center = true); translate([6,0,0]) cube(6, center = true); }
