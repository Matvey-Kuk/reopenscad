// OpenAPPA mascot, multipart — separately printed face colours that snap into
// a structural body.
//
// This keeps the detailed model's chamfered pixel grid, but changes how its
// gray cells are made. The muzzle is a pair of shallow plates, one for each
// face. Both plates snap independently into sockets in the body core.
// Each shoe replaces its complete bottom-row volume, so its front, back,
// bottom and exposed side faces are all gray. Matching split tabs on the shoes
// snap upward into the dark legs.
//
// The body itself splits into a front and a back half at the mid-depth block
// boundary, so each half prints flat without supports. The back half carries
// snap tabs on its cut face; the front half carries the matching sockets. The
// seam lands inside the chamfer V-groove that already exists between the two
// middle block layers, so the assembled body shows no extra line.
//
// Every export has a support-free orientation. The back half lies on its back
// with its tabs upward, so the eyes, nose and muzzle sockets are vertical
// holes. The front half lies on its cut face with the mascot's front upward;
// its sockets are blind vertical holes that open at the bed. Both muzzle
// plates print visible-face down with their connectors upward. The shoes print
// on their bottoms with their pins upward. The retained dark muzzle core uses
// the original pixel pattern instead of separate floating receiver frames.
//
// The original mark's eyes and nose show the background. They remain empty
// through-holes here, exactly as they are in `openappa.scad`.
//
// Preview the assembled mascot:
//   openscad website/public/brand/openappa-multipart.scad
//
// Export registered parts (useful for inspection or a slicer that will orient
// them). `part="body"` contains both body halves in their assembled position;
// `part="body_front"` and `part="body_back"` export one half each:
//   openscad --export-format binstl -D 'part="body"'      -o openappa-multipart-body.stl website/public/brand/openappa-multipart.scad
//   openscad --export-format binstl -D 'part="secondary"' -o openappa-multipart-gray.stl website/public/brand/openappa-multipart.scad
//
// Export each part already oriented for FDM printing:
//   openscad --export-format binstl -D 'part="body_back"'  -D 'print_orientation=true' -o openappa-multipart-body-back.stl website/public/brand/openappa-multipart.scad
//   openscad --export-format binstl -D 'part="body_front"' -D 'print_orientation=true' -o openappa-multipart-body-front.stl website/public/brand/openappa-multipart.scad
//   openscad --export-format binstl -D 'part="secondary"'  -D 'print_orientation=true' -o openappa-multipart-gray.stl website/public/brand/openappa-multipart.scad
//
// `part="print_plate"` shows every export together in its print orientation.
// The gray export contains six objects: two muzzle faces and four shoes.
//
// Export the two-piece muzzle fit/support coupon:
//   openscad --export-format binstl -D 'part="nose_test_body"'   -o openappa-multipart-nose-test-body.stl website/public/brand/openappa-multipart.scad
//   openscad --export-format binstl -D 'part="nose_test_insert"' -o openappa-multipart-nose-test-insert.stl website/public/brand/openappa-multipart.scad
// `part="nose_test_plate"` previews both test pieces on one build plate.

/* ------------------------------------------------------------ parameters --- */

mascot_height = 100;  // mm, paws to horn tips; 2x the original model

pixel_depth   = 4;    // blocks front to back

chamfer       = 0.15; // chamfer on every cube edge, as a fraction of a cell
bond          = 0.02; // hidden overlap across shared block faces

// One physical fit tolerance for every mating surface, in millimetres. The 2x
// model leaves 0.2 mm per side at outlines and around pins. Rendering epsilon
// and the hidden material-boundary shift below are mesh safeguards, not fits.
tolerance = 0.2;

// Move boolean cuts off the block construction planes. The shift is hidden in
// the existing V-groove at each material boundary.
material_boundary_shift = 0.01; // fraction of one cell

part = "assembly";   // [assembly, body, body_front, body_back, secondary, muzzle_front, muzzle_back, shoes, print_plate, nose_test_body, nose_test_insert, nose_test_plate]

// Individual exports are registered in the assembled coordinate system by
// default. Turn this on to put a body on its back or inserts on their faces.
// It has no effect on `assembly` or `print_plate`.
print_orientation = false;

primary_color   = "#202124";
secondary_color = "#8a8d91";

/* ------------------------------------------------------------------ grid --- */

// Original BEAST palette: `1` primary body, `3` dim muzzle/paws, `4` eyes,
// `2` nose, and `.` outside background. Only `1` and `3` are solid. The eye and
// nose cells remain holes, as in the original detailed model.
BEAST = [
    ".....11..........11.....",
    ".....11..........11.....",
    "....1111111111111111....",
    "...111111111111111111...",
    "...111111111111111111...",
    "...111111111111111111...",
    "...111444111111444111...",
    "...111444111111444111...",
    "...111444111111444111...",
    "...111111111111111111...",
    "...111111133331111111...",
    "...111111132231111111...",
    "...111111111111111111...",
    "....1111111111111111....",
    ".1111111111111111111111.",
    "111111111111111111111111",
    "111111111111111111111111",
    "111111111111111111111111",
    "111111111111111111111111",
    "111111111111111111111111",
    "11111..1111..1111..11111",
    "33333..3333..3333..33333",
];

GRID_COLS = 24;
GRID_ROWS = 22;

c  = mascot_height / GRID_ROWS;
ch = chamfer * c;
d  = pixel_depth * c;

// Each muzzle plate is deep enough to include the face layer's recessed core,
// so its six gray pixels remain one connected printable part.
muzzle_plate_depth = 0.65 * c;

// Every male connector is this same rectangular snap tab. Its dimensions are
// absolute so scaling a mascot cannot make it smaller than a 0.4 mm nozzle can
// resolve. The tab expands across X and stays a constant 2.2 mm through Y.
snap_tab_length          = 6.0;
snap_tab_shaft_width     = 4.8;
snap_tab_barb_width      = 5.6;
snap_tab_tip_width       = 2.4;
snap_tab_height          = 2.2;
snap_tab_split_width     = 1.0;
snap_tab_split_start     = 1.6;
snap_tab_shoulder_start  = 3.2;
snap_tab_shoulder_length = 0.8;

snap_inner_height         = snap_tab_height + 2 * tolerance;
snap_neck_width           = snap_tab_shaft_width + 2 * tolerance;
snap_chamber_width        = snap_tab_barb_width + 2 * tolerance;
snap_lead_length          = 0.7;
snap_pocket_start         = snap_tab_shoulder_start - tolerance;
snap_socket_depth         = snap_tab_length + tolerance;

muzzle_part_outline_offset = -tolerance / 2;
muzzle_body_outline_offset =  tolerance / 2;
muzzle_body_recess_depth   = muzzle_plate_depth + tolerance;

shoe_part_outline_offset = material_boundary_shift * c;
shoe_body_outline_offset = shoe_part_outline_offset + tolerance;

muzzle_gap        = d - 2 * muzzle_plate_depth;
muzzle_core_depth = d - 2 * muzzle_body_recess_depth;

// The body splits into a front and a back half at the first block boundary in
// front of the mid-depth plane. The halves mate flush — the snap clearances
// alone set the fit — and the seam hides in the chamfer V-groove already
// present at every block boundary.
body_half_split_y = material_boundary_shift * c;

// The muzzle shaft is wider than one grid cell. Inset it enough that its
// outer edge still has the full tolerance from the expanded body recess.
muzzle_connector_inset_mm =
    (tolerance + snap_tab_shaft_width - c) / 2;

epsilon = 0.02;
fit_check_epsilon = 0.000001;

assert(chamfer >= 0 && chamfer < 0.5,
       "chamfer must be at least 0 and less than 0.5 of a cell");
assert(tolerance >= 0,
       "tolerance must not be negative");
assert(c + snap_socket_depth < 3 * c,
       "shoe sockets are too long for the material above them");
assert(muzzle_core_depth < 2 * snap_socket_depth,
       "opposing muzzle sockets must meet in the middle");
assert(muzzle_gap > 2 * snap_tab_length,
       "opposing muzzle tabs collide");
assert(snap_socket_depth < d / 2 - body_half_split_y,
       "body half sockets must not pierce the front face");
assert(abs(muzzle_body_outline_offset - muzzle_part_outline_offset
           - tolerance) < fit_check_epsilon,
       "muzzle outline clearance is not tolerance");
assert(abs(muzzle_body_recess_depth - muzzle_plate_depth
           - tolerance) < fit_check_epsilon,
       "muzzle depth clearance is not tolerance");
assert(abs(shoe_body_outline_offset - shoe_part_outline_offset
           - tolerance) < fit_check_epsilon,
       "shoe outline clearance is not tolerance");
assert(abs((snap_neck_width - snap_tab_shaft_width) / 2
           - tolerance) < fit_check_epsilon,
       "snap shaft clearance is not tolerance per side");
assert(abs((snap_chamber_width - snap_tab_barb_width) / 2
           - tolerance) < fit_check_epsilon,
       "snap barb clearance is not tolerance per side");
assert(abs((snap_tab_barb_width - snap_neck_width) / 2
           - tolerance) < fit_check_epsilon,
       "snap barb interference is not tolerance per side");
assert(abs((snap_inner_height - snap_tab_height) / 2
           - tolerance) < fit_check_epsilon,
       "snap height clearance is not tolerance per side");
assert(abs(snap_socket_depth - snap_tab_length
           - tolerance) < fit_check_epsilon,
       "snap tip clearance is not tolerance");
assert(abs(muzzle_connector_inset_mm + c / 2
           - snap_tab_shaft_width / 2
           + muzzle_body_outline_offset
           - tolerance) < fit_check_epsilon,
       "muzzle tab outer clearance is not tolerance");

function filled(row, col) =
    row >= 0 && row < GRID_ROWS && col >= 0 && col < GRID_COLS
    && (BEAST[row][col] == "1" || BEAST[row][col] == "3");

function occupied(row, col, layer, dx = 0, dy = 0, dz = 0) =
    layer + dy >= 0 && layer + dy < pixel_depth
    && filled(row - dz, col + dx);

function pixel_x(col) = (col + 0.5 - GRID_COLS / 2) * c;
function pixel_z(row) = (GRID_ROWS - row - 0.5) * c;

/* ---------------------------------------------------------- solid mascot --- */

module block() {
    a = c - 2 * ch;
    hull() {
        cube([c, a, a], center = true);
        cube([a, c, a], center = true);
        cube([a, a, c], center = true);
    }
}

// Recessed cores and topology-aware bridges close the tunnels left where
// chamfered blocks meet. This is the same construction as the detailed model.
module core(row, col, layer) {
    a      = c - 2 * ch;
    bridge = 2 * (ch + bond * c);

    cube([a, a, a], center = true);

    if (occupied(row, col, layer, dx = 1))
        translate([c / 2, 0, 0]) cube([bridge, a, a], center = true);
    if (occupied(row, col, layer, dy = 1))
        translate([0, c / 2, 0]) cube([a, bridge, a], center = true);
    if (occupied(row, col, layer, dz = 1))
        translate([0, 0, c / 2]) cube([a, a, bridge], center = true);

    if (occupied(row, col, layer, dx = 1)
        && occupied(row, col, layer, dy = 1)
        && occupied(row, col, layer, dx = 1, dy = 1))
        translate([c / 2, c / 2, 0])
            cube([bridge, bridge, a], center = true);

    if (occupied(row, col, layer, dx = 1)
        && occupied(row, col, layer, dz = 1)
        && occupied(row, col, layer, dx = 1, dz = 1))
        translate([c / 2, 0, c / 2])
            cube([bridge, a, bridge], center = true);

    if (occupied(row, col, layer, dy = 1)
        && occupied(row, col, layer, dz = 1)
        && occupied(row, col, layer, dy = 1, dz = 1))
        translate([0, c / 2, c / 2])
            cube([a, bridge, bridge], center = true);

    if (occupied(row, col, layer, dx = 1)
        && occupied(row, col, layer, dy = 1)
        && occupied(row, col, layer, dz = 1)
        && occupied(row, col, layer, dx = 1, dy = 1)
        && occupied(row, col, layer, dx = 1, dz = 1)
        && occupied(row, col, layer, dy = 1, dz = 1)
        && occupied(row, col, layer, dx = 1, dy = 1, dz = 1))
        translate([c / 2, c / 2, c / 2])
            cube([bridge, bridge, bridge], center = true);
}

module mascot_solid() {
    for (row   = [0 : GRID_ROWS - 1],
         col   = [0 : GRID_COLS - 1],
         layer = [0 : pixel_depth - 1])
        if (filled(row, col))
            translate([pixel_x(col),
                       (layer + 0.5 - pixel_depth / 2) * c,
                       pixel_z(row)]) {
                block();
                core(row, col, layer);
            }
}

/* ---------------------------------------------------------- colour masks --- */

function in_region(row, col, region) =
    region == "muzzle" ? (BEAST[row][col] == "3"
                          && row < GRID_ROWS - 1)
  : region == "shoes"  ? (BEAST[row][col] == "3"
                          && row == GRID_ROWS - 1)
  : false;

module pixel_region(region) {
    union()
        for (row = [0 : GRID_ROWS - 1], col = [0 : GRID_COLS - 1])
            if (in_region(row, col, region))
                translate([pixel_x(col), pixel_z(row)])
                    square([c, c], center = true);
}

// Extrude an X/Z pixel region along Y. `delta` expands the body's muzzle
// opening or contracts a muzzle plate by half the shared tolerance.
module region_prism(region, delta, center_y, depth) {
    translate([0, center_y, 0])
        rotate([90, 0, 0])
            linear_extrude(height = depth, center = true, convexity = 10)
                offset(delta = delta)
                    pixel_region(region);
}

module front_region(region, delta, depth) {
    region_prism(region,
                 delta,
                 d / 2 - depth / 2,
                 depth + 2 * epsilon);
}

module back_region(region, delta, depth) {
    region_prism(region,
                 delta,
                 -d / 2 + depth / 2,
                 depth + 2 * epsilon);
}

module full_region(region, delta = 0) {
    region_prism(region, delta, 0, d + 2 * epsilon);
}

/* --------------------------------------------------------------- snaps --- */

// Both muzzle plates use the same two tabs in the outer gray pixels. Each shoe
// gets two identical tabs across its width.
muzzle_connector_inset = muzzle_connector_inset_mm / c;
muzzle_connectors = [
    [10, 10 + muzzle_connector_inset],
    [10, 13 - muzzle_connector_inset],
];

shoe_connectors = [
    [1,     0], [3,     0],
    [7.75,  0], [9.25,  0],
    [13.75, 0], [15.25, 0],
    [20,    0], [22,    0],
];

// Six tabs across the body split plane: one pair over the eyes, one pair
// beside the muzzle and one pair in the torso. Every position keeps solid
// cells around the socket and stays clear of the eye holes, the muzzle
// recesses, the muzzle body sockets and the shoe sockets.
body_half_connectors = [
    [ 4,  7], [ 4, 16],
    [12,  5], [12, 18],
    [17,  8], [17, 15],
];

// The shared rectangular tab has 1.9 mm prongs and a 1.0 mm split. Thin boxes
// at each profile station hull into printable tapers along local +Z.
module snap_tab() {
    module station(width, z) {
        translate([-width / 2, -snap_tab_height / 2, z])
            cube([width, snap_tab_height, 2 * epsilon]);
    }

    difference() {
        union() {
            hull() {
                station(snap_tab_shaft_width, 0);
                station(snap_tab_shaft_width,
                        snap_tab_shoulder_start);
            }
            hull() {
                station(snap_tab_shaft_width,
                        snap_tab_shoulder_start - epsilon);
                station(snap_tab_barb_width,
                        snap_tab_shoulder_start
                        + snap_tab_shoulder_length);
            }
            hull() {
                station(snap_tab_barb_width,
                        snap_tab_shoulder_start
                        + snap_tab_shoulder_length - epsilon);
                station(snap_tab_tip_width,
                        snap_tab_length - 2 * epsilon);
            }
        }

        translate([-snap_tab_split_width / 2,
                   -snap_tab_height,
                   snap_tab_split_start])
            cube([snap_tab_split_width,
                  2 * snap_tab_height,
                  snap_tab_length - snap_tab_split_start + epsilon]);
    }
}

// A wide tapered mouth guides the barb into a straight throat. The dedicated
// pocket begins just behind the tab shoulder, so the prongs expand into a real
// recess at full insertion instead of floating behind an early sharp ledge.
module snap_socket() {
    module station(width, z) {
        translate([-width / 2, -snap_inner_height / 2, z])
            cube([width, snap_inner_height, 2 * epsilon]);
    }

    union() {
        hull() {
            station(snap_chamber_width, 0);
            station(snap_neck_width, snap_lead_length);
        }

        translate([-snap_neck_width / 2,
                   -snap_inner_height / 2,
                   snap_lead_length - epsilon])
            cube([snap_neck_width,
                  snap_inner_height,
                  snap_pocket_start - snap_lead_length + 2 * epsilon]);

        translate([-snap_chamber_width / 2,
                   -snap_inner_height / 2,
                   snap_pocket_start - epsilon])
            cube([snap_chamber_width,
                  snap_inner_height,
                  snap_socket_depth - snap_pocket_start + 2 * epsilon]);
    }
}

// The two socket cuts enter the retained muzzle core from opposite faces and
// meet in the middle. They therefore print as open vertical channels when the
// body lies on its back, with no floating receiver boxes or chamber ceilings.
module muzzle_body_sockets() {
    for (point = muzzle_connectors) {
        translate([pixel_x(point[1]),
                   muzzle_core_depth / 2 + epsilon,
                   pixel_z(point[0])])
            rotate([90, 0, 0])
                snap_socket();

        translate([pixel_x(point[1]),
                   -muzzle_core_depth / 2 - epsilon,
                   pixel_z(point[0])])
            rotate([-90, 0, 0])
                snap_socket();
    }
}

module muzzle_front_pins() {
    for (point = muzzle_connectors)
        translate([pixel_x(point[1]),
                   d / 2 - muzzle_plate_depth + epsilon,
                   pixel_z(point[0])])
            rotate([90, 0, 0])
                snap_tab();
}

module muzzle_back_pins() {
    for (point = muzzle_connectors)
        translate([pixel_x(point[1]),
                   -d / 2 + muzzle_plate_depth - epsilon,
                   pixel_z(point[0])])
            rotate([-90, 0, 0])
                snap_tab();
}

module shoe_pins() {
    for (point = shoe_connectors)
        translate([pixel_x(point[0]), point[1], c - epsilon])
            snap_tab();
}

module shoe_snap_sockets() {
    for (point = shoe_connectors)
        translate([pixel_x(point[0]), point[1], c - epsilon])
            snap_socket();
}

// The back half's tabs point forward across the split plane; the matching
// sockets are cut into the front half from its cut face. Both use the shared
// snap geometry, so the joint has the same 0.2 mm clearances as every other
// mating surface.
module body_half_tabs() {
    for (point = body_half_connectors)
        translate([pixel_x(point[1]),
                   body_half_split_y - epsilon,
                   pixel_z(point[0])])
            rotate([-90, 0, 0])
                snap_tab();
}

module body_half_sockets() {
    for (point = body_half_connectors)
        translate([pixel_x(point[1]),
                   body_half_split_y - epsilon,
                   pixel_z(point[0])])
            rotate([-90, 0, 0])
                snap_socket();
}

/* ----------------------------------------------------------------- parts --- */

module body_part() {
    difference() {
        mascot_solid();

        // Recess each gray face but retain the original patterned material
        // between them as one continuous part of the dark body.
        front_region("muzzle",
                     muzzle_body_outline_offset,
                     muzzle_body_recess_depth);
        back_region("muzzle",
                    muzzle_body_outline_offset,
                    muzzle_body_recess_depth);

        // Compensate for the shoe mask's boolean-only expansion so the actual
        // separation between the shoe and body remains exactly `tolerance`.
        full_region("shoes",
                    shoe_body_outline_offset);

        muzzle_body_sockets();
        shoe_snap_sockets();
    }
}

// A cut prism reaching past the mascot on every side selects the front half
// of the finished body without touching its other surfaces.
module front_half_space() {
    translate([0, body_half_split_y + d / 2, mascot_height / 2])
        cube([(GRID_COLS + 2) * c, d, mascot_height + 2 * c],
             center = true);
}

module body_back_part() {
    union() {
        difference() {
            body_part();
            front_half_space();
        }
        body_half_tabs();
    }
}

module body_front_part() {
    difference() {
        intersection() {
            body_part();
            front_half_space();
        }
        body_half_sockets();
    }
}

module muzzle_front_part() {
    union() {
        intersection() {
            mascot_solid();
            front_region("muzzle",
                         muzzle_part_outline_offset,
                         muzzle_plate_depth);
        }
        muzzle_front_pins();
    }
}

module muzzle_back_part() {
    union() {
        intersection() {
            mascot_solid();
            back_region("muzzle",
                        muzzle_part_outline_offset,
                        muzzle_plate_depth);
        }
        muzzle_back_pins();
    }
}

module shoes_part() {
    union() {
        intersection() {
            mascot_solid();
            full_region("shoes", shoe_part_outline_offset);
        }
        shoe_pins();
    }
}

module secondary_part() {
    muzzle_front_part();
    muzzle_back_part();
    shoes_part();
}

/* --------------------------------------------------------- muzzle coupon --- */

// Six columns by four rows retain one complete ring of dark pixels around the
// muzzle. Intersecting the finished body preserves the exact face recesses,
// patterned central bridge, two sockets and 0.2 mm production clearances.
nose_test_width    = 6 * c;
nose_test_height   = 4 * c;
nose_test_center_z = 11 * c;
nose_test_z_min    = nose_test_center_z - nose_test_height / 2;
nose_test_z_max    = nose_test_center_z + nose_test_height / 2;
nose_insert_z_min  = 10 * c;

module nose_test_body_part() {
    intersection() {
        body_part();
        translate([0, 0, nose_test_center_z])
            cube([nose_test_width,
                  d + 2 * epsilon,
                  nose_test_height], center = true);
    }
}

// The production front and rear inserts have the same mating geometry. One
// front insert is enough to test both the fit and the connector print quality.
module nose_test_insert_part() {
    muzzle_front_part();
}

/* ----------------------------------------------------------- orientations --- */

// The original detailed model stands with its paws at Z=0, back at -Y and
// front at +Y. The back half rotates onto its back, tabs up. The front half
// rotates onto its cut face, so its sockets open at the bed and the mascot's
// front faces up. Each muzzle plate rotates onto its visible face, in
// opposite directions. Shoes already have flat undersides at Z=0; they only
// need shifting into positive Y for a slicer-friendly plate.
module body_on_bed() {
    translate([0, mascot_height, d / 2])
        rotate([90, 0, 0])
            children();
}

module body_front_on_bed() {
    translate([0, mascot_height, -body_half_split_y])
        rotate([90, 0, 0])
            children();
}

module body_halves_on_bed() {
    translate([-(GRID_COLS / 2 + 2) * c, 0, 0])
        body_on_bed() body_back_part();
    translate([(GRID_COLS / 2 + 2) * c, 0, 0])
        body_front_on_bed() body_front_part();
}

module muzzle_front_on_bed() {
    translate([0, 0, d / 2])
        rotate([-90, 0, 0])
            children();
}

module muzzle_back_on_bed() {
    translate([0, mascot_height, d / 2])
        rotate([90, 0, 0])
            children();
}

module shoes_on_bed() {
    translate([0, d / 2, 0])
        children();
}

module secondary_on_bed() {
    shoes_on_bed() shoes_part();
    translate([-3 * c, 0, 0])
        muzzle_front_on_bed() muzzle_front_part();
    translate([3 * c, 0, 0])
        muzzle_back_on_bed() muzzle_back_part();
}

module nose_test_body_on_bed() {
    translate([0, nose_test_z_max, d / 2])
        rotate([90, 0, 0])
            nose_test_body_part();
}

module nose_test_insert_on_bed() {
    translate([0, -nose_insert_z_min, d / 2])
        rotate([-90, 0, 0])
            nose_test_insert_part();
}

module nose_test_plate_on_bed() {
    color(primary_color)
        translate([-4 * c, 0, 0]) nose_test_body_on_bed();
    color(secondary_color)
        translate([4 * c, 0, 0]) nose_test_insert_on_bed();
}

module selected_part(selected = part) {
    assert(selected == "assembly"
           || selected == "body"
           || selected == "body_front"
           || selected == "body_back"
           || selected == "secondary"
           || selected == "muzzle_front"
           || selected == "muzzle_back"
           || selected == "shoes"
           || selected == "print_plate"
           || selected == "nose_test_body"
           || selected == "nose_test_insert"
           || selected == "nose_test_plate",
           str("Unknown part: ", selected));

    if (selected == "assembly") {
        color(primary_color) {
            body_back_part();
            body_front_part();
        }
        color(secondary_color) secondary_part();
    }

    if (selected == "body")
        color(primary_color)
            if (print_orientation) body_halves_on_bed();
            else {
                body_back_part();
                body_front_part();
            }

    if (selected == "body_front")
        color(primary_color)
            if (print_orientation)
                body_front_on_bed() body_front_part();
            else body_front_part();

    if (selected == "body_back")
        color(primary_color)
            if (print_orientation) body_on_bed() body_back_part();
            else body_back_part();

    if (selected == "secondary")
        color(secondary_color)
            if (print_orientation) secondary_on_bed();
            else secondary_part();

    if (selected == "muzzle_front")
        color(secondary_color)
            if (print_orientation)
                muzzle_front_on_bed() muzzle_front_part();
            else muzzle_front_part();

    if (selected == "muzzle_back")
        color(secondary_color)
            if (print_orientation)
                muzzle_back_on_bed() muzzle_back_part();
            else muzzle_back_part();

    if (selected == "shoes")
        color(secondary_color)
            if (print_orientation) shoes_on_bed() shoes_part();
            else shoes_part();

    if (selected == "print_plate") {
        color(primary_color) body_on_bed() body_back_part();
        translate([(GRID_COLS + 3) * c, 0, 0])
            color(primary_color) body_front_on_bed() body_front_part();
        translate([(2 * GRID_COLS + 10) * c, 0, 0])
            color(secondary_color) secondary_on_bed();
    }

    if (selected == "nose_test_body")
        color(primary_color) nose_test_body_on_bed();

    if (selected == "nose_test_insert")
        color(secondary_color) nose_test_insert_on_bed();

    if (selected == "nose_test_plate")
        nose_test_plate_on_bed();
}

selected_part();
