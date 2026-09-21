//! ReOpenSCAD's native engine as a reusable library.
//!
//! The HTTP server is one consumer of this crate. Keeping the engine public
//! also lets integration and corpus tests exercise the same in-process Rust
//! implementation without starting a server or invoking another CAD engine.

pub mod csg;
pub mod engine;
pub mod gltf;
pub mod mcpapp;
pub mod step;
pub mod threemf;
