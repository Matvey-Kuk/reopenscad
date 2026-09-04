//! The bounding-volume pruning inside `Shape::distance` is an optimization, not
//! a modelling decision: it may only skip work that provably cannot change the
//! sampled value. These tests pin that down by evaluating every shape twice —
//! once through the real `Shape::distance`, once through a deliberately naive
//! fold that visits every child — and requiring bit-identical results.

use super::*;

/// `Shape::distance` with every bounding-volume shortcut removed.
///
/// Leaves have nothing to prune, so they delegate. The n-ary nodes re-implement
/// their fold over all children unconditionally, which is exactly the behaviour
/// the pruned versions must reproduce.
fn unpruned_distance(shape: &Shape, point: Vec3) -> f64 {
    match shape {
        Shape::Transform { shape, transform } => {
            unpruned_distance(shape, Transform::point(transform.inverse, point))
                * transform.distance_scale
        }
        Shape::Offset2d { shape, delta, .. } => unpruned_distance(shape, point) - delta,
        Shape::LinearExtrude {
            shape,
            height,
            center,
            ..
        } => {
            let planar = unpruned_distance(shape, Vec3::new(point.x, point.y, 0.0));
            let minimum_z = if *center { -height / 2.0 } else { 0.0 };
            let slab = (point.z - (minimum_z + height / 2.0)).abs() - height / 2.0;
            let outside = planar.max(0.0).hypot(slab.max(0.0));
            outside + planar.max(slab).min(0.0)
        }
        Shape::MinkowskiDilation {
            shape,
            offset,
            radius,
            ..
        } => unpruned_distance(shape, point.sub(*offset)) - radius,
        Shape::Union { children, .. } => children
            .iter()
            .map(|child| unpruned_distance(&child.shape, point))
            .fold(f64::INFINITY, f64::min),
        Shape::Difference { base, subtract, .. } => subtract.iter().fold(
            unpruned_distance(base, point),
            |distance, child| distance.max(-unpruned_distance(&child.shape, point)),
        ),
        Shape::Intersection(shapes) => shapes
            .iter()
            .map(|shape| unpruned_distance(shape, point))
            .fold(f64::NEG_INFINITY, f64::max),
        leaf => leaf.distance(point),
    }
}

/// Deterministic 64-bit PRNG, so a failure is always reproducible.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let value = self.0;
        (value ^ (value >> 33)).wrapping_mul(0xff51_afd7_ed55_8ccd)
    }

    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn evaluate(source: &str) -> Shape {
    let tokens = Lexer::new(source).tokenize().expect("lex");
    let statements = Parser::new(tokens).parse_program().expect("parse");
    Evaluator::default().evaluate(&statements).expect("evaluate")
}

/// Sample points across the model's bounds padded outward, so the exterior
/// region — where pruning is most aggressive — is covered as densely as the
/// interior and the surface.
fn sample_points(bounds: Bounds, count: usize, seed: u64) -> Vec<Vec3> {
    let span = bounds.max.sub(bounds.min);
    let reach = span.x.max(span.y).max(span.z).max(1.0);
    let mut rng = Rng(seed);
    (0..count)
        .map(|_| {
            Vec3::new(
                bounds.min.x - reach + rng.unit() * (span.x + 2.0 * reach),
                bounds.min.y - reach + rng.unit() * (span.y + 2.0 * reach),
                bounds.min.z - reach + rng.unit() * (span.z + 2.0 * reach),
            )
        })
        .collect()
}

/// Every construct whose distance fold prunes, plus the shapes whose bounds
/// guarantees the pruning relies on. `hull()` matters most: its
/// max-over-planes value underestimates the true distance, so it prunes on a
/// scale factor rather than on the raw bounds distance.
fn pruning_corpus() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "difference with many subtrahends",
            "difference() { cube(20, center=true); for (i = [0:39]) translate([sin(i * 37) * 9, cos(i * 53) * 9, i * 0.4 - 8]) sphere(1.2); }",
        ),
        (
            "difference below the bvh threshold",
            "difference() { cube([20, 10, 6]); translate([3, 3, -1]) cube([4, 4, 8]); translate([12, 2, -1]) cylinder(h = 8, r = 2); }",
        ),
        (
            "nested difference of unions",
            "difference() { union() { cube([20, 20, 4]); translate([0, 0, 4]) cube([10, 10, 6]); } union() { translate([2, 2, -1]) cube([4, 4, 20]); translate([12, 12, -1]) cube([4, 4, 20]); } }",
        ),
        (
            "union above the bvh threshold",
            "for (x = [0:5]) for (y = [0:4]) translate([x * 5, y * 5, 0]) cube([3, 3, 3]);",
        ),
        (
            "union of hulls",
            "for (x = [0:4]) for (y = [0:3]) translate([x * 6, y * 6, 0]) hull() { cube([4, 4, 4]); translate([0.5, 0.5, 4]) cube([3, 3, 0.5]); }",
        ),
        (
            "difference of hulls",
            "difference() { hull() { cube([20, 20, 4]); translate([4, 4, 6]) cube([12, 12, 2]); } for (i = [0:9]) translate([2 + i * 1.8, 6, -1]) hull() { cube([1.2, 1.2, 12]); translate([0.4, 0.4, 0]) cube([1.2, 1.2, 12]); } }",
        ),
        (
            "rotated and scaled subtrees",
            "difference() { rotate([20, 35, 10]) cube([16, 12, 8], center=true); scale([1.4, 0.6, 1]) rotate([0, 0, 30]) cube([6, 6, 20], center=true); }",
        ),
        (
            "intersection inside a union",
            "union() { intersection() { cube([12, 12, 12]); translate([4, 4, 4]) sphere(7); } translate([20, 0, 0]) cube([6, 6, 6]); }",
        ),
        (
            "cones and spheres, which cannot claim a bounds bound",
            "difference() { union() { cylinder(h = 12, r1 = 6, r2 = 2); translate([14, 0, 0]) sphere(5); } translate([0, 0, -1]) cylinder(h = 14, r = 1.5); }",
        ),
        (
            "extruded 2d geometry",
            "difference() { linear_extrude(height = 6) square([20, 12]); translate([4, 4, -1]) linear_extrude(height = 8) circle(r = 3); }",
        ),
    ]
}

#[test]
fn pruned_distance_matches_the_unpruned_fold_bit_for_bit() {
    for (name, source) in pruning_corpus() {
        let shape = evaluate(source);
        let mut checked = 0usize;
        for point in sample_points(shape.bounds(), 4_000, 0x5eed_1234) {
            let pruned = shape.distance(point);
            let reference = unpruned_distance(&shape, point);
            assert_eq!(
                pruned.to_bits(),
                reference.to_bits(),
                "{name}: pruning changed the sampled distance at {point:?} \
                 ({pruned} vs {reference})"
            );
            checked += 1;
        }
        assert_eq!(checked, 4_000);
    }
}

/// Also cover the model that motivated the pruning work, at the exact grid the
/// preview mesher samples.
#[test]
fn pruned_distance_matches_the_unpruned_fold_for_the_multipart_mascot() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/openappa/openappa-multipart.scad"
    ))
    .expect("read openappa-multipart.scad");
    let shape = evaluate(&source);
    for point in sample_points(shape.bounds(), 300, 0xc0ff_ee01) {
        assert_eq!(
            shape.distance(point).to_bits(),
            unpruned_distance(&shape, point).to_bits(),
            "pruning changed the sampled distance at {point:?}"
        );
    }
}

/// The contract every prune rests on: outside a shape's bounds, the value the
/// shape reports is at least `bounds_distance_scale` times the distance to
/// those bounds. Violating it would let a fold skip a child that mattered.
#[test]
fn reported_distances_respect_the_bounds_distance_scale() {
    fn check(shape: &Shape, name: &str) {
        let scale = shape.bounds_distance_scale();
        if scale <= 0.0 {
            return;
        }
        let bounds = shape.bounds();
        let dimension = shape.dimension();
        for point in sample_points(bounds, 3_000, 0x1234_abcd) {
            let bounds_distance = bounds.outside_distance(point, dimension);
            if bounds_distance <= 0.0 {
                continue;
            }
            let reported = shape.distance(point);
            // `Shape::Box` and `Bounds::outside_distance` compute the same
            // quantity through different expression orders, so a shape sitting
            // exactly on its own bounds can land an ulp low. Allow that much
            // and no more: the pruning it enables is off by the same amount.
            let promised = bounds_distance * scale;
            assert!(
                reported >= promised - promised.abs() * 1e-12,
                "{name}: reported {reported} at {point:?} is below the promised \
                 lower bound {promised} (bounds distance {bounds_distance}, scale {scale})"
            );
        }
    }

    for (name, source) in pruning_corpus() {
        check(&evaluate(source), name);
    }

    // Hulls are the shapes whose scale is neither 0 nor 1, so exercise a
    // deliberately awkward spread of them.
    for source in [
        "hull() { cube([30, 2, 2], center=true); rotate([0, 0, 90]) cube([30, 2, 2], center=true); }",
        "hull() { cube([1, 1, 1]); translate([25, 0, 0]) cube([1, 1, 1]); translate([0, 1, 20]) cube([0.5, 0.5, 0.5]); }",
        "hull() { cube([24, 24, 0.5], center=true); translate([0, 0, 18]) cube([1, 1, 0.5], center=true); }",
        "rotate([31, 17, 53]) hull() { cube([8, 3, 1]); translate([1, 1, 9]) cube([2, 1, 1]); }",
    ] {
        let shape = evaluate(source);
        assert!(
            shape.bounds_distance_scale() > 0.0,
            "expected a usable pruning scale for {source}"
        );
        check(&shape, source);
    }
}

/// A hull whose scale were too optimistic would silently drop geometry, so
/// compare the pruned union fold against the unpruned one for hull shapes that
/// are far from round.
#[test]
fn awkward_hulls_still_prune_without_changing_values() {
    let source = "union() {
        hull() { cube([40, 1.5, 1.5], center=true); rotate([0, 45, 0]) cube([40, 1.5, 1.5], center=true); }
        translate([0, 30, 0]) hull() { cube([28, 28, 0.4], center=true); translate([0, 0, 22]) cube([1.6, 1.6, 0.4], center=true); }
        translate([0, -30, 0]) rotate([25, 40, 15]) hull() { cube([1.6, 1.6, 1.6], center=true); translate([18, 0.5, 0]) cube([1.6, 1.6, 1.6], center=true); translate([0, 0.5, 12]) cube([1.6, 1.6, 1.6], center=true); }
    }";
    let shape = evaluate(source);
    for point in sample_points(shape.bounds(), 6_000, 0x0bad_c0de) {
        assert_eq!(
            shape.distance(point).to_bits(),
            unpruned_distance(&shape, point).to_bits(),
            "pruning changed the sampled distance at {point:?}"
        );
    }
}
