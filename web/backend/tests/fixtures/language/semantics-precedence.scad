// `^` binds tighter than unary minus, so -2^2 is -4. Reading it as (-2)^2 = 4
// mirrors the model about the origin, which a bounding box notices.
translate([-2^2, 0, 0]) cube([6, 6, 6]);
