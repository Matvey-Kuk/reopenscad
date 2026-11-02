//! Top-level object identity: names and IDs.
//!
//! An object's name and ID come from the top-level statement that produced
//! it, so statements that produce no geometry cannot disturb them. See
//! [`super::naming`] for the rules.

use super::*;

fn objects(source: &str) -> Vec<(String, String)> {
    compile_parts(source, Quality::Preview, None)
        .unwrap_or_else(|error| panic!("compile: {error}"))
        .parts
        .into_iter()
        .map(|part| (part.id, part.name))
        .collect()
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(id, name)| (id.to_string(), name.to_string()))
        .collect()
}

/// Twenty-five asserts at the top level used to wipe every name to
/// "object N". Asserts, echoes and assignments are not objects and do not
/// count.
#[test]
fn non_geometry_statements_do_not_shift_names() {
    let plain = objects(
        "module base() cube(4);
         module lid() translate([0, 0, 5]) cube(4);
         base();
         lid();",
    );
    assert_eq!(
        plain,
        pairs(&[("object-1", "base 1"), ("object-2", "lid 2")])
    );

    let noisy = objects(
        "module base() cube(4);
         module lid() translate([0, 0, 5]) cube(4);
         wall = 2;
         assert(wall > 0);
         echo(\"building\");
         base();
         assert(true, \"still fine\");
         lid();
         echo(wall = wall);",
    );
    assert_eq!(noisy, plain);
}

/// A part selector hides bodies by making them empty. The remaining objects
/// keep their names *and* their IDs, so a hidden-object list or an export
/// selection made earlier still points at the same bodies.
#[test]
fn an_empty_sibling_does_not_rename_or_renumber_the_others() {
    let source = |part: &str| {
        format!(
            "part = \"{part}\";
             module skin() if (part != \"core\") cube(4);
             module core() if (part != \"skin\") translate([6, 0, 0]) cube(4);
             module lid() translate([0, 0, 6]) cube(4);
             skin();
             core();
             lid();"
        )
    };
    assert_eq!(
        objects(&source("all")),
        pairs(&[
            ("object-1", "skin 1"),
            ("object-2", "core 2"),
            ("object-3", "lid 3")
        ])
    );
    assert_eq!(
        objects(&source("core")),
        pairs(&[("object-2", "core 2"), ("object-3", "lid 3")])
    );
}

/// `if`, `for` and `let` are named after the module inside them when every
/// geometry statement inside agrees, rather than "if 1".
#[test]
fn control_statements_take_the_name_of_what_they_wrap() {
    let named = objects(
        "module peg() cylinder(h = 3, r = 1);
         module lid() cube(4);
         show_lid = true;
         if (show_lid) translate([0, 0, 10]) lid();
         for (x = [0, 5]) translate([x, 10, 0]) peg();
         let (h = 2) { echo(h); color(\"red\") translate([20, 0, 0]) lid(); }
         if (show_lid) translate([30, 0, 0]) lid(); else translate([30, 0, 0]) peg();",
    );
    assert_eq!(
        named,
        pairs(&[
            ("object-1", "lid 1"),
            ("object-2", "peg 2"),
            ("object-3", "lid 3"),
            ("object-4", "if 4"),
        ])
    );
}

/// `// @name: …` directly before a statement names it, and its ID follows
/// the name rather than its position.
#[test]
fn a_comment_annotation_names_an_object() {
    let source = "// @name: Base plate
         cube([10, 10, 2]);
         /* @name = \"Lid\" */
         translate([0, 0, 5]) cube([10, 10, 2]);
         translate([0, 0, 10]) cube(2);";
    assert_eq!(
        objects(source),
        pairs(&[
            ("object-base-plate", "Base plate"),
            ("object-lid", "Lid"),
            ("object-3", "cube 3"),
        ])
    );
    // Moving the lid to the front changes nothing about its identity.
    let reordered = "/* @name: Lid */
         translate([0, 0, 5]) cube([10, 10, 2]);
         // @name: Base plate
         cube([10, 10, 2]);";
    assert_eq!(
        objects(reordered),
        pairs(&[("object-lid", "Lid"), ("object-base-plate", "Base plate")])
    );
}

/// `$name` is the in-language spelling: valid OpenSCAD, ignored there.
#[test]
fn a_dollar_name_argument_names_an_object() {
    let source = "module lid() cube(4);
         lid($name = \"Lid\");
         color(\"red\", $name = \"Knob\") translate([10, 0, 0]) sphere(2);
         let ($name = \"Hinge pin\") translate([20, 0, 0]) cylinder(h = 4, r = 1);
         translate([0, 10, 0]) cube(2, $name = \"Foot\");";
    let compiled = compile_parts(source, Quality::Preview, None).expect("compile");
    assert_eq!(
        compiled
            .parts
            .iter()
            .map(|part| (part.id.clone(), part.name.clone()))
            .collect::<Vec<_>>(),
        pairs(&[
            ("object-lid", "Lid"),
            ("object-knob", "Knob"),
            ("object-hinge-pin", "Hinge pin"),
            ("object-foot", "Foot"),
        ])
    );
    assert!(
        compiled
            .messages
            .iter()
            .all(|message| !message.contains("$name")),
        "{:?}",
        compiled.messages
    );
}

/// An annotation in front of something that makes no object is reported,
/// not silently dropped.
#[test]
fn a_misplaced_annotation_is_reported() {
    let compiled = compile_parts(
        "// @name: Wall
         wall = 2;
         cube(wall);",
        Quality::Preview,
        None,
    )
    .expect("compile");
    assert_eq!(compiled.parts[0].name, "cube 1");
    assert!(
        compiled
            .messages
            .iter()
            .any(|message| message.contains("@name: Wall") && message.contains("ignored")),
        "{:?}",
        compiled.messages
    );
}

/// Duplicate names stay distinguishable, in both name and ID.
#[test]
fn duplicate_explicit_names_are_disambiguated() {
    let source = "// @name: Leg
         cube(1);
         // @name: Leg
         translate([5, 0, 0]) cube(1);";
    assert_eq!(
        objects(source),
        pairs(&[("object-leg", "Leg"), ("object-leg-2", "Leg 2")])
    );
}

/// Each object carries the byte range of its statement, so a client can map
/// an object back to its source.
#[test]
fn objects_carry_their_source_range() {
    let source = "x = 1;\ncube(2);\n// @name: Ball\ntranslate([5, 0, 0]) sphere(1);\n";
    let compiled = compile_parts(source, Quality::Preview, None).expect("compile");
    let ranges: Vec<&str> = compiled
        .parts
        .iter()
        .map(|part| {
            let range = part.source_range.expect("range");
            &source[range.start..range.end]
        })
        .collect();
    assert_eq!(ranges, vec!["cube(2);", "translate([5, 0, 0]) sphere(1);"]);
}

/// `!` still names the object after the statement that holds it.
#[test]
fn the_root_modifier_keeps_the_statement_name() {
    let source = "cube(1);
         // @name: Only this
         translate([5, 0, 0]) !sphere(1);";
    assert_eq!(objects(source), pairs(&[("object-only-this", "Only this")]));
}

/// Agent gap 10: twenty-five asserts, calls to modules that can never make
/// geometry (or do not exist), and a top-level part selector together. None
/// of them may shift the automatic names or IDs of the real objects, and a
/// hidden part keeps its slot so the others stay stable across selections.
#[test]
fn asserts_empty_module_calls_and_a_part_selector_keep_ids_stable() {
    let source = |part: &str| {
        let asserts = (0..25)
            .map(|index| format!("assert({index} >= 0, \"check {index}\");\n"))
            .collect::<String>();
        format!(
            "part = \"{part}\";
             module base() cube([10, 10, 2]);
             module lid() translate([0, 0, 5]) cube([10, 10, 2]);
             module knob() translate([5, 5, 8]) cylinder(h = 3, r = 1);
             module report() {{ echo(\"part\", part); assert(true); }}
             module checks() report();
             {asserts}
             report();
             checks();
             if (part == \"all\" || part == \"base\") base();
             report();
             if (part == \"all\" || part == \"lid\") lid();
             not_defined_anywhere();
             knob();"
        )
    };
    let all = objects(&source("all"));
    assert_eq!(
        all,
        pairs(&[
            ("object-1", "base 1"),
            ("object-2", "lid 2"),
            ("object-3", "knob 3")
        ])
    );
    assert_eq!(
        objects(&source("lid")),
        pairs(&[("object-2", "lid 2"), ("object-3", "knob 3")])
    );
    assert_eq!(
        objects(&source("base")),
        pairs(&[("object-1", "base 1"), ("object-3", "knob 3")])
    );
    // Without any of the noise the identities are the same.
    let quiet = objects(
        "module base() cube([10, 10, 2]);
         module lid() translate([0, 0, 5]) cube([10, 10, 2]);
         module knob() translate([5, 5, 8]) cylinder(h = 3, r = 1);
         base(); lid(); knob();",
    );
    assert_eq!(quiet, all);
}

/// Mutual recursion that bottoms out in geometry is still an object; one
/// that never does is not.
#[test]
fn recursive_modules_are_judged_by_whether_they_can_reach_geometry() {
    let objects = objects(
        "module a(n) if (n > 0) b(n - 1); else cube(1);
         module b(n) a(n);
         module loop_only(n) if (n > 0) loop_only(n - 1);
         loop_only(3);
         b(2);",
    );
    assert_eq!(objects, pairs(&[("object-1", "b 1")]));
}
