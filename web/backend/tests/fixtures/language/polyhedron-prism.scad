// A closed prism with quad side faces, to check faces of more than three
// vertices and the winding rule together.
polyhedron(
  points = [[0,0,0],[10,0,0],[5,9,0], [0,0,12],[10,0,12],[5,9,12]],
  faces  = [[0,1,2], [3,5,4], [0,3,4,1], [1,4,5,2], [2,5,3,0]]
);
