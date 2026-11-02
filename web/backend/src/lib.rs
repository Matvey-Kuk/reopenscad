//! ReOpenSCAD's native engine as a reusable library.
//!
//! The HTTP server is one consumer of this crate. Keeping the engine public
//! also lets integration and corpus tests exercise the same in-process Rust
//! implementation without starting a server or invoking another CAD engine.

pub mod analysis;
pub mod assets;
pub mod csg;
pub mod engine;
pub mod export;
pub mod gltf;
pub mod insertion;
pub mod mcpapp;
pub mod meshanalysis;
pub mod meshimport;
pub mod meshops;
pub mod raster;
pub mod scadsource;
pub mod support;
pub mod step;
pub mod threemf;
mod vec3;
