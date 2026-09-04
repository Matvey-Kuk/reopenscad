//! Byte-exact output digest for the exact CSG kernel.
//!
//! Not part of the server. It compiles every checked-in golden case (and any
//! extra `.scad` files given on the command line) and prints a digest of the
//! binary STL each one produces. Two runs that print identical digests
//! produced bit-identical geometry, which is the check an optimisation inside
//! `src/csg` has to pass.
//!
//!   cargo run --release --example csgdigest -- [extra.scad ...]
use std::fs;
use std::path::PathBuf;

use reopenscad_server::engine;

fn digest(bytes: &[u8]) -> String {
    // FNV-1a over the payload, plus the length, printed as one hex word. Any
    // change to a single coordinate bit changes it.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{hash:016x}/{}", bytes.len())
}

fn compile(id: &str, source: &str) {
    match engine::compile_exact(source, None) {
        Ok(output) => println!(
            "{id}\t{}\ttris={}",
            digest(&output.mesh.binary_stl()),
            output.mesh.triangles.len()
        ),
        Err(error) => println!("{id}\tERROR\t{error}"),
    }
}

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/openappa");
    let table = fs::read_to_string(root.join("cases.tsv")).expect("cases.tsv");
    for line in table.lines() {
        if line.trim_start().starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let mut fields = line.split("\\t");
        let id = fields.next().unwrap_or_default();
        let file = fields.next().unwrap_or_default();
        let definitions = fields.next().unwrap_or_default();
        let source = fs::read_to_string(root.join(file)).expect("case source");
        let source = if definitions.trim().is_empty() {
            source
        } else {
            format!("{source}\n\n// Test-only command-line definition overrides.\n{definitions};\n")
        };
        compile(id, &source);
    }
    for path in std::env::args().skip(1) {
        let source = fs::read_to_string(&path).expect("extra source");
        compile(&path, &source);
    }
}
