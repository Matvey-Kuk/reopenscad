// ==========================================================
//  Tablet / pill box  —  INTERNAL volume 75 x 50 x 25 mm
//  Print-in-place, both halves flat, folded closed.
//
//  Everything is driven from the interior. iL/iW/iH are the
//  clear cavity; the outer shell is derived from them.
//
//  Hinge: 11 narrow knuckles instead of 5 wide ones. Same
//  span, twice the bearings across it, so the lid cannot
//  rack. No pins: each lid barrel ends in a conical boss in
//  a conical socket, slope 1.15, so boss, socket wall and
//  socket ceiling all stay under 45 deg. A 45 deg buttress
//  carries the barrel down into the wall.
//
//  Latch: the tooth has a FLAT bottom face, passes right
//  through the skirt and stands 0.3 mm proud. Flat on flat
//  cannot cam, so pulling the lid cannot open it.
//
//  The spring is a horizontal bridge anchored at BOTH ends.
//  Orientation is the whole point. An upright cantilever
//  anchored at the floor is loaded ACROSS the layer planes
//  (~35 MPa) and splits. This bridge bends about Z, so it is
//  loaded ALONG them (~55 MPa) and runs at 15 MPa.
//
//  Do not make the skirt the spring instead. It has to be
//  thinned to survive, which drags the latch down towards
//  the floor and leaves no material under the window — the
//  catch strip is what actually carries the load.
// ==========================================================

$fn = 48;

/* ---------- interior: the actual specification ---------- */
iL = 75;         // X, clear
iW = 50;         // Y, clear
iH = 25;         // Z, clear

/* ---------------- shell ------------------- */
wall    = 1.2;
floor_t = 1.2;
corner  = 3.5;
edge_c  = 1.8;
seam_c  = 0.5;
in_c    = 1.2;

/* ---------------- hinge ------------------- */
kn_n      = 11;     // narrow and many: half the width, twice the count
kn_gap    = 0.35;
cone_s    = 1.15;
cone_eng  = 0.80;
cone_tip  = 0.35;
cone_c    = 0.35;
kn_wall   = 0.70;
body_gap  = 0.35;

/* ---------------- sealing lip ------------- */
lip_h     = 2.2;
lip_t     = 0.8;
lip_c     = 0.22;
lip_shelf = 1.7;

/* ---------------- latch ------------------- */
rec       = 1.5;   // front recess: the skirt sits flush in it
btn_t     = 1.0;   // bridge thickness = the wall behind the recess
rec_w     = 40;
rec_ch    = 3.0;
rec_z0    = 1.2;
btn_w     = 30;    // bridge span, anchored both ends, pressed mid-span
btn_h     = 5.0;
btn_z0    = 4.0;
slot      = 0.5;
hook_w    = 14;
hook_out  = 2.7;   // through the skirt and 0.3 mm proud of the front face
hook_z    = 5.0;   // height of the flat retaining face
hook_face = 1.0;
skirt_w   = 32;
skirt_ch  = 2.5;
skirt_z0  = 1.6;
win_w     = 16;    // the tooth passes through this
win_ch    = 1.0;
win_z0    = 4.8;   // the catch edge
win_z1    = 8.9;

/* ---------------- interior ---------------- */
compartments = 1;
div_t = 1.0;

mode = "print";                // "print" | "closed"

/* ---------------- derived ----------------- */
boss_d    = rec + btn_t;
L         = iL + 2 * wall;
H         = iH + 2 * floor_t;
split     = H / 2;
D         = iW + 2 * wall + (boss_d - wall);

cone_l    = kn_gap + cone_eng;
cone_r0   = cone_tip + cone_s * cone_l;
sock_r0   = cone_r0 + cone_c - cone_s * kn_gap;
sock_d    = cone_eng + 0.3;
sock_r1   = sock_r0 - cone_s * sock_d;
kn_r      = sock_r0 + kn_wall;

ax_y      = D + body_gap + kn_r;
W         = ax_y + kn_r;
span      = L - 28;
seg       = span / kn_n;
hx0       = (L - span) / 2;
kw        = seg - kn_gap;
xc        = L / 2;

K         = (ax_y - split) + kn_r * 1.4142;
but_root  = (D - wall) - K;
but_edge  = W - K;

hook_tip  = rec - hook_out;        // y of the tooth's tip, negative = proud
travel    = wall - hook_tip;       // press needed to clear the skirt
strip     = win_z0 - skirt_z0;     // the catch strip: this carries the load

assert(sock_r1 > 0.2, "socket cone inverts: raise kn_wall or lower cone_eng");
assert(kw - 2 * sock_d > 0.8, "knuckles too narrow: sockets meet in the middle");
assert(but_root > 0.5, "buttress starts below the floor: barrel is too low");
assert(but_root < split, "buttress never reaches the wall");
assert(sqrt(pow(W - ax_y, 2) + pow(but_edge - split, 2)) < ax_y - D,
       "knuckle reaches past the fold radius and will hit the other half");
assert(hook_tip < 0, "tooth must stand proud of the front face");
// This is the one that was violated when the skirt was made the spring:
assert(strip > 2.0 + win_ch, "catch strip under the window is too thin to hold");
assert(win_z0 < hook_z, "window's catch edge sits above the tooth");
assert(win_z1 > hook_z + hook_face + hook_out, "window too short for the tooth");
assert(win_w > hook_w + 1, "tooth fouls the sides of the window");
assert(skirt_w > win_w + 8, "skirt too narrow around the window");
assert(btn_z0 < hook_z && btn_z0 + btn_h > hook_z + hook_face,
       "tooth hangs off the end of the button");
assert(rec_w > btn_w + 6, "button has no wall left to anchor into");

function kx(i) = hx0 + i * seg + kn_gap / 2;

// ---------- 2D helpers ----------

module cham_rect(sx, sy, cb, ct) {
  polygon([[max(cb, 0.001), 0],
           [sx - max(cb, 0.001), 0],
           [sx, max(cb, 0.001)],
           [sx, sy - max(ct, 0.001)],
           [sx - max(ct, 0.001), sy],
           [max(ct, 0.001), sy],
           [0, sy - max(ct, 0.001)],
           [0, max(cb, 0.001)]]);
}

module pad(w, h, c) {
  translate([-w/2, -h/2])
    cham_rect(w, h, min(c, w/2 - 0.05, h/2 - 0.05), min(c, w/2 - 0.05, h/2 - 0.05));
}

module xz(y0, t) {
  translate([0, y0 + t, 0]) rotate([90, 0, 0]) linear_extrude(t) children();
}

module yz(x0, t) {
  translate([x0, 0, 0]) rotate([90, 0, 90]) linear_extrude(t) children();
}

module cbox(sx, sy, sz, cv, cb, ct) {
  intersection() {
    linear_extrude(sz) cham_rect(sx, sy, cv, cv);
    translate([0, sy + 1, 0]) rotate([90, 0, 0])
      linear_extrude(sy + 2) cham_rect(sx, sz, cb, ct);
    yz(-1, sx + 2) cham_rect(sy, sz, cb, ct);
  }
}

// ---------- hinge ----------

module knuckle(top) {
  s = top ? -1 : 1;
  yz(0, kw) union() {
    translate([ax_y, split]) circle(r = kn_r);
    polygon([[D - wall, split],
             [D - wall, split + s * (but_root - split)],
             [W,        split + s * (but_edge - split)],
             [W,        split]]);
  }
}

module cone_boss(x0, dir) {
  translate([x0, ax_y, split]) rotate([0, 90 * dir, 0])
    cylinder(h = cone_l, r1 = cone_r0, r2 = cone_tip);
}

module cone_socket(x0, dir) {
  translate([x0, ax_y, split]) rotate([0, 90 * dir, 0])
    cylinder(h = sock_d, r1 = sock_r0, r2 = sock_r1);
}

module knuckles(is_lid) {
  for (i = [0 : kn_n - 1])
    if (((i % 2) == 1) == is_lid) {
      translate([kx(i), 0, 0]) knuckle(is_lid);
      if (is_lid) {
        cone_boss(kx(i), -1);
        cone_boss(kx(i) + kw, 1);
      }
    }
}

module sockets() {
  for (i = [0 : kn_n - 1])
    if ((i % 2) == 0) {
      if (i > 0)          cone_socket(kx(i), 1);
      if (i < kn_n - 1)   cone_socket(kx(i) + kw, -1);
    }
}

// ---------- latch ----------

module recess_cut() {
  xz(-1, rec + 1)
    translate([xc, (rec_z0 + split + 3) / 2])
      pad(rec_w, split + 3 - rec_z0, rec_ch);
}

// Two horizontal slots free the bridge. Horizontal is deliberate: the
// bridge then bends about Z and is stressed along the layers.
module button_slots() {
  for (z = [btn_z0 - slot, btn_z0 + btn_h])
    translate([xc - btn_w/2, rec - 0.2, z])
      cube([btn_w, btn_t + 0.4, slot]);
}

// The tooth: flat bottom face — this is what holds — vertical front, then
// a 45 deg ramp on top so the lid still cams itself shut.
module hook() {
  yz(xc - hook_w/2, hook_w)
    polygon([[rec,       hook_z],
             [hook_tip,  hook_z],
             [hook_tip,  hook_z + hook_face],
             [rec,       hook_z + hook_face + hook_out]]);
}

module skirt() {
  difference() {
    xz(0, wall)
      translate([xc, (skirt_z0 + split + 3) / 2])
        pad(skirt_w, split + 3 - skirt_z0, skirt_ch);
    xz(-1, wall + 2)
      translate([xc, (win_z0 + win_z1) / 2])
        pad(win_w, win_z1 - win_z0, win_ch);
  }
}

// ---------- base ----------

module base_lip() {
  difference() {
    translate([0, 0, split]) linear_extrude(lip_h) difference() {
      offset(delta = -(wall + lip_c))         cham_rect(L, D, corner, corner);
      offset(delta = -(wall + lip_c + lip_t)) cham_rect(L, D, corner, corner);
    }
    translate([xc - rec_w/2 - 1, -1, split - 1])
      cube([rec_w + 2, boss_d + 2, lip_h + 2]);
    // cbox's top chamfer insets only the X and Y faces, never the 45 deg
    // corner facets, so there is no shelf at the corners and the lip would
    // hang there. Break the lip across all four.
    for (sx = [0, 1])
      for (sy = [0, 1])
        translate([sx * (L - 4.8), sy * (D - 4.8), split - 1])
          cube([4.8, 4.8, lip_h + 2]);
  }
}

module dividers() {
  if (compartments > 1)
    for (i = [1 : compartments - 1])
      translate([i * L / compartments - div_t/2, wall, floor_t])
        cube([div_t, D - 2*wall, split - floor_t]);
}

module base() {
  union() {
    difference() {
      union() {
        cbox(L, D, split, corner, edge_c, seam_c);
        knuckles(false);
      }
      difference() {
        translate([wall, wall, floor_t])
          cbox(L - 2*wall, D - 2*wall, split - floor_t + 0.5,
               corner - wall, in_c, lip_shelf + 0.5);
        translate([xc - rec_w/2 - 1.5, -1, -1])
          cube([rec_w + 3, boss_d + 1, split + 2]);
      }
      sockets();
      recess_cut();
      button_slots();
    }
    base_lip();
    hook();
    dividers();
  }
}

// ---------- lid ----------

module lid() {
  difference() {
    union() {
      translate([0, 0, split]) cbox(L, D, H - split, corner, seam_c, edge_c);
      knuckles(true);
      skirt();
    }
    translate([wall, wall, split - 1])
      cbox(L - 2*wall, D - 2*wall, H - split - floor_t + 1, corner - wall, 0, in_c);
  }
}

// ---------- assembly ----------

base();
if (mode == "print")
  translate([0, 2 * ax_y, 2 * split]) rotate([180, 0, 0]) lid();
else
  lid();
