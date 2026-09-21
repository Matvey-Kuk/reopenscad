// One non-convex child: the hull is its convex closure.
linear_extrude(4) hull()
  polygon([[0,0],[20,0],[20,6],[8,6],[8,14],[0,14]]);
