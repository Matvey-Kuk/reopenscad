// A shear, which no translate/rotate/scale combination can express.
multmatrix([[1, 0, 0.5, 0], [0, 1, 0, 0], [0, 0, 1, 0]])
  cube([10, 10, 20], center = true);
