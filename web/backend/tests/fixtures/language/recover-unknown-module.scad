// An unknown module is a warning in OpenSCAD, not the end of the compile. The
// cube below must still be produced.
notamodule(1, 2);
cube([12, 8, 4]);
