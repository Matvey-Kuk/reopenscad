//! Exact polyhedral CSG kernel.
//!
//! This module is an explicit boundary-representation geometry kernel: solids
//! are polygon soups with exact planes, booleans are computed by BSP-tree
//! classification rather than by sampling a distance field, and every output
//! edge is the exact intersection of two input planes. It exists to replace the
//! implicit/marching-tetrahedra mesher in `engine.rs`, which cannot reproduce
//! OpenSCAD's tessellation by construction.
//!
//! Module map:
//!
//! * [`mesh`] – vectors, planes, polygons, matrices, indexed meshes.
//! * [`poly2d`] – 2D regions: triangulation, offsetting.
//! * [`hull`] – 3D and 2D convex hulls.

pub mod bsp;
pub mod hull;
pub mod mesh;
pub mod planar;
pub mod poly2d;
pub mod prof;
pub mod primitives;
pub mod solid;

#[allow(unused_imports)]
pub use mesh::{Bounds, IndexedMesh, Matrix4, Plane, Polygon, Vec3};
#[allow(unused_imports)]
pub use poly2d::Region2d;
#[allow(unused_imports)]
pub use solid::Solid;
