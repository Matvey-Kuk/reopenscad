// The minimal polyhedron: four points, four faces, wound clockwise seen from
// outside as OpenSCAD requires.
polyhedron(
  points = [[0,0,0], [20,0,0], [0,20,0], [0,0,20]],
  faces  = [[0,1,2], [0,3,1], [1,3,2], [0,2,3]]
);
