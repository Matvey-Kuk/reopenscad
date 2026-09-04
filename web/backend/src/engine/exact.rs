//! Bridge from the evaluator's shape tree to the exact polyhedral kernel.
//!
//! The evaluator in `engine.rs` builds a [`Shape`] tree and, historically, fed
//! it to a marching-tetrahedra mesher that sampled `Shape::distance`. That
//! sampling is what makes the implicit path structurally unable to reproduce
//! OpenSCAD's output: it quantises every edge onto a grid.
//!
//! This module keeps the evaluator and replaces only the meshing stage. Each
//! `Shape` node is turned into either a [`csg::Solid`] (an explicit boundary
//! representation) or a [`csg::Region2d`] (a 2D point set), and CSG operations
//! run through the exact BSP kernel in [`crate::csg`].
//!
//! # What is exact here and what is not
//!
//! Primitives, transforms, `linear_extrude` (including `twist` and `scale`),
//! `offset` (both the rounded `r=` and the mitred `delta=`), `polygon`
//! (including `paths=`), `minkowski` and the three booleans are all exact. One
//! node is still only as good as the information the shared shape tree carries:
//!
//! * [`Shape::Hull`] stores the hull as a set of support planes rather than the
//!   original point cloud, so the kernel rebuilds the polytope by intersecting
//!   half-spaces. That is exact for the polytope, but it inherits whatever the
//!   evaluator's plane enumeration produced.
//!
//! That limitation lives in the shape tree, not in the kernel.
//!
//! [`Shape::MinkowskiDilation`] carries both: the true second operand, which is
//! what the kernel sums, and the bounding ball the sampled mesher substitutes
//! because a distance field cannot express a general Minkowski sum.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::{
    check_cancelled, CompileOutput, EngineError, Evaluator, Lexer, Mesh, Parser, Quality, Shape,
    ShapeDimension, Transform, Triangle, Vec3,
};
use crate::csg;
use crate::csg::mesh::Matrix4;

/// Which geometry kernel a compile should use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kernel {
    /// The legacy implicit-surface sampler. Fast, approximate, grid-limited.
    #[default]
    Sampled,
    /// The exact polyhedral CSG kernel.
    Exact,
}

/// Facets on the ball used for the legacy dilation fallback, which is reached
/// only when a `MinkowskiDilation` node arrives without its second operand.
const MAX_DILATION_FRAGMENTS: usize = 24;

/// Extra worker threads this process will hand out to intra-request geometry
/// work, shared across every render in flight.
///
/// `MAX_CONCURRENT_JOBS` bounds how many *requests* compile at once; this
/// bounds how much of the machine a single request may claim on top of its own
/// thread. Keeping the pool global means two concurrent renders split the
/// cores instead of each spawning a full machine's worth of threads.
static IDLE_WORKERS: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Number of extra threads available at startup: every core but the one the
/// request is already running on, capped so a many-core host does not spend
/// more time in scheduling than in geometry.
fn worker_capacity() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get().saturating_sub(1).min(7))
        .unwrap_or(0)
}

/// Claims up to `wanted` extra workers from the shared pool.
///
/// The claim is a plain counter rather than a thread pool: the threads
/// themselves are scoped, so they cannot outlive the borrowed shapes they read.
struct WorkerClaim {
    count: usize,
}

impl WorkerClaim {
    fn take(wanted: usize) -> Self {
        if wanted == 0 {
            return Self { count: 0 };
        }
        // First caller in the process learns the machine's size.
        let _ = IDLE_WORKERS.compare_exchange(
            usize::MAX,
            worker_capacity(),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        let mut available = IDLE_WORKERS.load(Ordering::Acquire);
        loop {
            let granted = wanted.min(available);
            if granted == 0 {
                return Self { count: 0 };
            }
            match IDLE_WORKERS.compare_exchange_weak(
                available,
                available - granted,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Self { count: granted },
                Err(current) => available = current,
            }
        }
    }
}

impl Drop for WorkerClaim {
    fn drop(&mut self) {
        if self.count > 0 {
            IDLE_WORKERS.fetch_add(self.count, Ordering::AcqRel);
        }
    }
}

/// Runs `task` for every index below `count`, in parallel where the machine has
/// cores to spare, collecting results in index order.
///
/// The tasks must be independent and order-free: results are placed back in
/// input order, so a caller that folds them sees exactly the sequence a serial
/// run would have produced. Threads come out of the process-wide budget above,
/// so nesting one of these inside another degrades to serial rather than
/// oversubscribing the machine.
pub(super) fn map_indexed<T, F>(count: usize, task: F) -> Vec<T>
where
    T: Send,
    F: Fn(usize) -> T + Sync,
{
    if count <= 1 {
        return (0..count).map(task).collect();
    }
    let claim = WorkerClaim::take(count - 1);
    if claim.count == 0 {
        return (0..count).map(task).collect();
    }
    let results: std::sync::Mutex<Vec<Option<T>>> =
        std::sync::Mutex::new((0..count).map(|_| None).collect());
    let next = AtomicUsize::new(0);
    let run = || loop {
        let index = next.fetch_add(1, Ordering::AcqRel);
        if index >= count {
            return;
        }
        let value = task(index);
        results.lock().expect("task slots are never poisoned")[index] = Some(value);
    };
    std::thread::scope(|scope| {
        for _ in 0..claim.count {
            scope.spawn(run);
        }
        // The requesting thread is a worker too, so a claim of one extra
        // thread still gives two-way parallelism.
        run();
    });
    results
        .into_inner()
        .expect("task slots are never poisoned")
        .into_iter()
        .map(|slot| slot.expect("every task slot is filled before the scope ends"))
        .collect()
}

/// [`csg::solid::union_all`]'s balanced reduction with each level's pairs run
/// concurrently.
///
/// The pairing is deliberately identical to the kernel's — empties dropped
/// once up front, then neighbours folded and the level halved, an odd tail
/// carried forward untouched — so this produces the same solid, built from the
/// same operands, in the same order. Only the waiting differs. A model built
/// from a grid of small parts is one enormous union, and folding it is often
/// the single longest step in a render.
///
/// `parallel_unions_match_the_kernels_reduction` pins the equivalence.
pub(super) fn union_all_parallel(solids: Vec<csg::Solid>) -> csg::Solid {
    let mut level: Vec<csg::Solid> = solids
        .into_iter()
        .filter(|solid| !solid.is_empty())
        .collect();
    while level.len() > 1 {
        // An odd level carries its last solid forward untouched, exactly as the
        // kernel's fold does, so the pairing stays identical.
        let carried = (level.len() % 2 == 1).then(|| level.pop()).flatten();
        let mut operands = Vec::with_capacity(level.len() / 2);
        let mut remaining = level.into_iter();
        while let (Some(left), Some(right)) = (remaining.next(), remaining.next()) {
            operands.push((left, right));
        }
        let mut next = map_indexed(operands.len(), |pair| {
            let (left, right) = &operands[pair];
            left.union(right)
        });
        next.extend(carried);
        level = next;
    }
    level.pop().unwrap_or_default()
}

/// Converts many independent shapes to solids, in parallel where the machine
/// has cores to spare.
///
/// Each shape is converted by exactly the same code as the serial path, so the
/// solids — and therefore the meshes — are bit-identical to what a sequential
/// run produces. Only the wall-clock cost changes. Results come back in input
/// order regardless of the order the workers finished in.
pub(super) fn shapes_to_solids(
    shapes: &[&Shape],
    cancellation: Option<&AtomicBool>,
) -> Vec<Result<csg::Solid, EngineError>> {
    map_indexed(shapes.len(), |index| {
        shape_to_solid(shapes[index], cancellation)
    })
}


/// Compiles `source` with the exact polyhedral kernel.
pub fn compile_exact(
    source: &str,
    cancellation: Option<&AtomicBool>,
) -> Result<CompileOutput, EngineError> {
    let tokens = Lexer::new(source).tokenize()?;
    let statements = Parser::new(tokens).parse_program()?;
    let mut evaluator = Evaluator {
        cancellation,
        preview: false,
        exact: true,
        ..Evaluator::default()
    };
    let shape = evaluator.evaluate(&statements)?;
    if shape.dimension() != ShapeDimension::Solid {
        return Err(EngineError::new("The model contains no 3D geometry."));
    }
    check_cancelled(cancellation)?;
    let solid = shape_to_solid(&shape, cancellation)?;
    let mesh = solid_to_mesh(&solid);
    if mesh.triangles.is_empty() {
        return Err(EngineError::new("The model contains no 3D geometry."));
    }
    let mut messages = evaluator.diagnostics;
    messages.extend([
        format!("Parsed {} top-level statements.", statements.len()),
        format!(
            "Exact polyhedral kernel produced {} triangles from {} faces.",
            mesh.triangles.len(),
            solid.polygons.len()
        ),
    ]);
    Ok(CompileOutput { mesh, messages })
}

/// Converts a kernel solid into the engine's export mesh.
pub fn solid_to_mesh(solid: &csg::Solid) -> Mesh {
    let indexed = solid.to_indexed_mesh();
    let mut triangles = Vec::with_capacity(indexed.triangles.len());
    for index in 0..indexed.triangles.len() {
        let [a, b, c] = indexed.triangle_points(index);
        let normal = b.sub(a).cross(c.sub(a)).normalized();
        triangles.push(Triangle {
            vertices: [engine_vec(a), engine_vec(b), engine_vec(c)],
            normal: engine_vec(normal),
        });
    }
    Mesh { triangles }
}

fn engine_vec(point: csg::Vec3) -> Vec3 {
    Vec3::new(point.x, point.y, point.z)
}

fn kernel_vec(point: Vec3) -> csg::Vec3 {
    csg::Vec3::new(point.x, point.y, point.z)
}

fn matrix_of(transform: &Transform) -> Matrix4 {
    Matrix4(transform.forward)
}

/// Turns a solid `Shape` into an exact boundary representation.
pub(super) fn shape_to_solid(
    shape: &Shape,
    cancellation: Option<&AtomicBool>,
) -> Result<csg::Solid, EngineError> {
    check_cancelled(cancellation)?;
    match shape {
        Shape::Box { size, center } => Ok(csg::primitives::cube(kernel_vec(*size), *center)),
        Shape::Sphere { radius, fragments } => Ok(csg::primitives::sphere(*radius, *fragments)),
        Shape::Cylinder {
            height,
            radius1,
            radius2,
            center,
            fragments,
        } => Ok(csg::primitives::cylinder(
            *height, *radius1, *radius2, *center, *fragments,
        )),
        Shape::Color { shape, .. } => shape_to_solid(shape, cancellation),
        Shape::Transform { shape, transform } => {
            Ok(shape_to_solid(shape, cancellation)?.transformed(matrix_of(transform)))
        }
        Shape::LinearExtrude {
            shape,
            height,
            center,
            twist,
            scale,
            slices,
        } => {
            let region = shape_to_region(shape, cancellation)?;
            Ok(csg::primitives::linear_extrude(
                &region, *height, *center, *twist, *scale, *slices,
            ))
        }
        Shape::Hull { planes, bounds, .. } => {
            // The evaluator has already reduced the hull to its support
            // planes, so the polytope is the intersection of their back
            // half-spaces. Clipping a bounding box face by face is exact and
            // costs nothing like a BSP boolean per plane.
            let kernel_bounds = csg::Bounds {
                min: kernel_vec(bounds.min),
                max: kernel_vec(bounds.max),
            };
            let mut faces = csg::solid::bounding_box_polygons(kernel_bounds, 0.5);
            for plane in planes {
                let plane = csg::Plane::new(kernel_vec(plane.normal), plane.offset);
                faces = csg::solid::clip_convex(&faces, plane);
                if faces.is_empty() {
                    break;
                }
            }
            Ok(csg::Solid { polygons: faces }.simplified())
        }
        Shape::MinkowskiDilation {
            shape,
            offset,
            radius,
            kernel,
        } => {
            let base = shape_to_solid(shape, cancellation)?;
            // The real sum whenever the evaluator kept the operand. Only the
            // sampled path is forced to substitute a ball for it.
            if let Some(kernel) = kernel {
                let tool = shape_to_solid(kernel, cancellation)?;
                return csg::primitives::minkowski(&base, &tool).map_err(EngineError::new);
            }
            let translated = base.transformed(Matrix4::translation(kernel_vec(*offset)));
            if *radius <= 0.0 {
                return Ok(translated);
            }
            let ball = csg::primitives::sphere(*radius, MAX_DILATION_FRAGMENTS);
            csg::primitives::minkowski(&translated, &ball).map_err(EngineError::new)
        }
        Shape::Union { children, .. } => {
            // A union's children are independent of one another, and a model
            // assembled from a grid of small parts arrives here as one union
            // of hundreds. Converting and folding them concurrently is the
            // difference between minutes and seconds on such a model, and the
            // operand order is preserved so the solid is unchanged.
            let shapes = children.iter().map(|child| &child.shape).collect::<Vec<_>>();
            let mut solids = Vec::with_capacity(shapes.len());
            for solid in shapes_to_solids(&shapes, cancellation) {
                solids.push(solid?);
            }
            Ok(union_all_parallel(solids))
        }
        Shape::Difference { base, subtract, .. } => {
            let mut result = shape_to_solid(base, cancellation)?;
            for child in subtract {
                if result.is_empty() {
                    break;
                }
                let tool = shape_to_solid(&child.shape, cancellation)?;
                result = result.difference(&tool);
            }
            Ok(result)
        }
        Shape::Intersection(children) => {
            let shapes = children.iter().collect::<Vec<_>>();
            let mut solids = Vec::with_capacity(shapes.len());
            for solid in shapes_to_solids(&shapes, cancellation) {
                solids.push(solid?);
            }
            Ok(csg::solid::intersection_all(solids))
        }
        Shape::Square2d { .. }
        | Shape::Circle2d { .. }
        | Shape::Polygon2d { .. }
        | Shape::Offset2d { .. } => Err(EngineError::new(
            "2D geometry needs an extrusion before it can be rendered as a solid.",
        )),
    }
}

/// Turns a planar `Shape` into an exact 2D region.
pub(super) fn shape_to_region(
    shape: &Shape,
    cancellation: Option<&AtomicBool>,
) -> Result<csg::Region2d, EngineError> {
    check_cancelled(cancellation)?;
    match shape {
        Shape::Square2d { size, center } => Ok(csg::primitives::square(*size, *center)),
        Shape::Circle2d { radius, fragments } => Ok(csg::primitives::circle(*radius, *fragments)),
        Shape::Polygon2d { contours } => {
            let mut region = csg::Region2d::new(contours.clone());
            region.normalize();
            Ok(region)
        }
        Shape::Color { shape, .. } => shape_to_region(shape, cancellation),
        Shape::Offset2d {
            shape,
            delta,
            rounded,
            fragments,
        } => {
            let region = shape_to_region(shape, cancellation)?;
            Ok(region.offset(*delta, *rounded, *fragments))
        }
        Shape::Transform { shape, transform } => {
            let region = shape_to_region(shape, cancellation)?;
            let matrix = matrix_of(transform);
            let contours = region
                .contours
                .into_iter()
                .map(|contour| {
                    contour
                        .into_iter()
                        .map(|point| {
                            let mapped = matrix.apply(csg::Vec3::new(point[0], point[1], 0.0));
                            [mapped.x, mapped.y]
                        })
                        .collect()
                })
                .collect();
            let mut region = csg::Region2d::new(contours);
            region.normalize();
            Ok(region)
        }
        Shape::Union { children, .. } => {
            let mut regions = Vec::with_capacity(children.len());
            for child in children {
                regions.push(shape_to_region(&child.shape, cancellation)?);
            }
            Ok(csg::planar::union_all(&regions))
        }
        Shape::Difference { base, subtract, .. } => {
            let mut result = shape_to_region(base, cancellation)?;
            for child in subtract {
                let tool = shape_to_region(&child.shape, cancellation)?;
                result = csg::planar::difference(&result, &tool);
            }
            Ok(result)
        }
        Shape::Intersection(children) => {
            let mut regions = Vec::with_capacity(children.len());
            for child in children {
                regions.push(shape_to_region(child, cancellation)?);
            }
            Ok(csg::planar::intersection_all(&regions))
        }
        _ => Err(EngineError::new(
            "This shape cannot be used where 2D geometry is expected.",
        )),
    }
}

/// Compiles with the requested kernel, keeping the sampled path available.
pub fn compile_with_kernel(
    source: &str,
    quality: Quality,
    cancellation: Option<&AtomicBool>,
    kernel: Kernel,
) -> Result<CompileOutput, EngineError> {
    match kernel {
        Kernel::Sampled => super::compile_sampled(source, quality, cancellation),
        Kernel::Exact => compile_exact(source, cancellation),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mesh_of(source: &str) -> Mesh {
        compile_exact(source, None).unwrap().mesh
    }

    fn volume(mesh: &Mesh) -> f64 {
        mesh.triangles
            .iter()
            .map(|triangle| {
                let [a, b, c] = triangle.vertices;
                a.dot(b.cross(c)) / 6.0
            })
            .sum::<f64>()
            .abs()
    }

    fn triangle_listing(mesh: &Mesh) -> Vec<String> {
        mesh.triangles
            .iter()
            .map(|triangle| format!("{:?} {:?}", triangle.vertices, triangle.normal))
            .collect()
    }

    /// The parallel fold in [`union_all_parallel`] mirrors the kernel's own
    /// reduction rather than calling it, so this pins the two together: if the
    /// kernel ever changes how it pairs operands, this fails instead of the
    /// two silently producing different tessellations of the same solid.
    #[test]
    fn parallel_unions_match_the_kernels_reduction() {
        for count in [1usize, 2, 3, 5, 8, 9, 16, 17] {
            let mut solids = (0..count)
                .map(|index| {
                    csg::primitives::cube(csg::Vec3::new(10.0, 10.0, 10.0), false).transformed(
                        Matrix4::translation(csg::Vec3::new(index as f64 * 6.0, 0.0, 0.0)),
                    )
                })
                .collect::<Vec<_>>();
            // An empty operand is dropped before pairing, which shifts every
            // pair after it; both folds must shift it the same way.
            if count > 3 {
                solids.insert(2, csg::Solid::default());
            }
            let sequential = csg::solid::union_all(solids.clone());
            let parallel = union_all_parallel(solids);
            assert_eq!(
                triangle_listing(&solid_to_mesh(&sequential)),
                triangle_listing(&solid_to_mesh(&parallel)),
                "union of {count} overlapping cubes"
            );
        }
    }

    #[test]
    fn a_lone_cube_compiles_to_twelve_exact_triangles() {
        let mesh = mesh_of("cube(20);");
        assert_eq!(mesh.triangles.len(), 12);
        assert!((volume(&mesh) - 8000.0).abs() < 1e-9);
    }

    #[test]
    fn a_translated_union_merges_into_one_box() {
        let mesh = mesh_of("union() { cube(10); translate([0,0,10]) cube(10); }");
        assert_eq!(mesh.triangles.len(), 12);
        assert!((volume(&mesh) - 2000.0).abs() < 1e-9);
    }

    #[test]
    fn a_difference_keeps_exact_coordinates() {
        let mesh = mesh_of("difference() { cube(10); translate([2,2,5]) cube(3); }");
        assert!((volume(&mesh) - (1000.0 - 27.0)).abs() < 1e-9, "{}", volume(&mesh));
    }

    #[test]
    fn an_extruded_polygon_with_a_hole_keeps_the_hole() {
        let mesh = mesh_of(
            "linear_extrude(height=2) polygon(points=[[0,0],[10,0],[10,10],[0,10],[4,4],[6,4],[6,6],[4,6]], paths=[[0,1,2,3],[4,5,6,7]]);",
        );
        assert!((volume(&mesh) - (100.0 - 4.0) * 2.0).abs() < 1e-9, "{}", volume(&mesh));
    }

    #[test]
    fn a_faceted_cylinder_has_the_expected_facet_count() {
        let mesh = mesh_of("cylinder(h=10, r=5, $fn=8);");
        assert_eq!(mesh.triangles.len(), 2 * 6 + 16);
    }

    #[test]
    fn two_dimensional_geometry_is_rejected_without_an_extrusion() {
        assert!(compile_exact("square(10);", None).is_err());
    }

    #[test]
    fn the_kernel_selector_still_reaches_the_sampled_path() {
        let sampled =
            compile_with_kernel("cube(20);", Quality::Preview, None, Kernel::Sampled).unwrap();
        let exact =
            compile_with_kernel("cube(20);", Quality::Preview, None, Kernel::Exact).unwrap();
        assert!(sampled.mesh.triangles.len() > exact.mesh.triangles.len());
        assert_eq!(exact.mesh.triangles.len(), 12);
    }
}
