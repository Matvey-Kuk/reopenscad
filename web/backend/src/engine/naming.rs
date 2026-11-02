//! Names and IDs for the top-level objects of a model.
//!
//! An object is a top-level statement that produced geometry. Its identity is
//! derived from *that statement*, never from its position among everything
//! else at the top level, so an `assert`, an `echo`, an assignment or a module
//! call that happens to produce nothing cannot shift another object's name or
//! ID.
//!
//! Naming, in order of precedence:
//!
//! 1. An annotation comment directly before the statement:
//!    `// @name: Lid` (also `/* @name: Lid */`, `@name = "Lid"`).
//! 2. A `$name` string literal on the statement: `lid($name = "Lid");`,
//!    `color("red", $name = "Lid") lid();` or `let($name = "Lid") lid();`.
//!    Both spellings are valid OpenSCAD with no effect there, so an
//!    annotated file still runs unchanged in the desktop application.
//! 3. Otherwise the module the statement calls, looking through transparent
//!    wrappers (`translate`, `rotate`, `color`, …) and through `if`, `for`,
//!    `let` and `{ }` when every geometry statement inside names the same
//!    module — plus the object's ordinal: `lid 3`.
//!
//! IDs: an explicitly named object's ID is a slug of its name
//! (`object-lid`), which survives any edit that does not rename it. Every
//! other object's ID is `object-N`, where `N` counts the top-level statements
//! that *can* make geometry, so it is unaffected by asserts, echoes,
//! assignments and calls to modules that never make geometry (or do not
//! exist), and by sibling objects that are present but currently empty.

use super::{Argument, Expr, Stmt, TopLevelMeta};
use std::collections::{HashMap, HashSet};

/// Longest object name kept from an annotation or `$name`.
pub(super) const MAX_OBJECT_NAME_CHARACTERS: usize = 80;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ObjectIdentity {
    pub id: String,
    pub name: String,
}

/// Modules that only position or decorate their single child, so an object
/// is better named after what they wrap.
fn is_transparent_wrapper(name: &str) -> bool {
    matches!(
        name,
        "translate"
            | "rotate"
            | "scale"
            | "mirror"
            | "color"
            | "render"
            | "resize"
            | "multmatrix"
            | "echo"
            | "assert"
    )
}

/// Which statements can make geometry at all, judged from the syntax of the
/// whole file: a call to a module whose body can never produce geometry (it
/// only echoes, asserts or assigns), or to a module that does not exist, is
/// no more an object than an `assert` is. Statements that *can* make geometry
/// but are empty on this run (a part selector hiding a body) still count, so
/// toggling the selector never renumbers the other objects.
pub(super) struct Candidacy<'a> {
    /// Whether each user module (by name, across every definition of that
    /// name) can make geometry.
    modules: HashMap<&'a str, bool>,
    /// An unresolved `include`/`use` may define any module, so unknown names
    /// have to be assumed to make geometry.
    opaque: bool,
}

/// Rounds of the module fixpoint before giving up and assuming the rest can
/// make geometry. Real files settle in two or three.
const MAX_CANDIDACY_ROUNDS: usize = 32;

impl<'a> Candidacy<'a> {
    pub(super) fn new(statements: &'a [Stmt]) -> Self {
        let mut definitions: Vec<(&'a str, &'a [Stmt])> = Vec::new();
        let mut opaque = false;
        collect_modules(statements, &mut definitions, &mut opaque);
        let mut candidacy = Candidacy {
            modules: definitions.iter().map(|(name, _)| (*name, false)).collect(),
            opaque,
        };
        // Least fixpoint: a module makes geometry if any statement in any of
        // its definitions does, given what is known about the others so far.
        let mut settled = false;
        for _ in 0..MAX_CANDIDACY_ROUNDS {
            let mut changed = false;
            for (name, body) in &definitions {
                if !candidacy.modules[name] && body.iter().any(|statement| candidacy.can_make(statement)) {
                    candidacy.modules.insert(name, true);
                    changed = true;
                }
            }
            if !changed {
                settled = true;
                break;
            }
        }
        if !settled {
            for value in candidacy.modules.values_mut() {
                *value = true;
            }
        }
        candidacy
    }

    /// Whether a statement can make geometry at all.
    pub(super) fn can_make(&self, statement: &Stmt) -> bool {
        match statement {
            Stmt::Call { name, children, .. } => match name.as_str() {
                "echo" | "assert" => children.iter().any(|child| self.can_make(child)),
                name if super::builtin_module_parameters(name).is_some() => true,
                name => match self.modules.get(name) {
                    Some(makes) => *makes,
                    None => self.opaque,
                },
            },
            Stmt::Block(body) | Stmt::For { body, .. } | Stmt::Let { body, .. } => {
                body.iter().any(|statement| self.can_make(statement))
            }
            Stmt::If {
                then_body,
                else_body,
                ..
            } => then_body
                .iter()
                .chain(else_body)
                .any(|statement| self.can_make(statement)),
            Stmt::Root(inner) => self.can_make(inner),
            Stmt::Background(_)
            | Stmt::Disabled
            | Stmt::Assign { .. }
            | Stmt::FileDirective { .. }
            | Stmt::Function { .. }
            | Stmt::Module { .. } => false,
        }
    }
}

fn collect_modules<'a>(
    statements: &'a [Stmt],
    definitions: &mut Vec<(&'a str, &'a [Stmt])>,
    opaque: &mut bool,
) {
    for statement in statements {
        match statement {
            Stmt::Module { name, body, .. } => {
                definitions.push((name.as_str(), body.as_slice()));
                collect_modules(body, definitions, opaque);
            }
            Stmt::FileDirective { .. } => *opaque = true,
            Stmt::Block(body) | Stmt::For { body, .. } | Stmt::Let { body, .. } => {
                collect_modules(body, definitions, opaque)
            }
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                collect_modules(then_body, definitions, opaque);
                collect_modules(else_body, definitions, opaque);
            }
            Stmt::Call { children, .. } => collect_modules(children, definitions, opaque),
            Stmt::Root(inner) => collect_modules(std::slice::from_ref(inner.as_ref()), definitions, opaque),
            _ => {}
        }
    }
}

/// The one name every geometry statement in `body` agrees on, if any.
fn unanimous<'a>(
    body: impl Iterator<Item = &'a Stmt>,
    candidacy: &Candidacy,
    name_of: &dyn Fn(&Stmt, &Candidacy) -> Option<String>,
) -> Option<String> {
    let mut agreed: Option<String> = None;
    for statement in body.filter(|statement| candidacy.can_make(statement)) {
        let name = name_of(statement, candidacy)?;
        match &agreed {
            Some(previous) if *previous != name => return None,
            Some(_) => {}
            None => agreed = Some(name),
        }
    }
    agreed
}

fn string_literal(expression: &Expr) -> Option<String> {
    match expression {
        Expr::String(value) if !value.trim().is_empty() => Some(
            value
                .trim()
                .chars()
                .take(MAX_OBJECT_NAME_CHARACTERS)
                .collect(),
        ),
        _ => None,
    }
}

/// A `$name = "…"` given on the statement or on a wrapper around it.
pub(super) fn dollar_name(statement: &Stmt, candidacy: &Candidacy) -> Option<String> {
    match statement {
        Stmt::Call {
            name,
            arguments,
            children,
        } => {
            if let Some(found) = arguments
                .iter()
                .find(|argument| argument.name.as_deref() == Some("$name"))
                .and_then(|argument| string_literal(&argument.value))
            {
                return Some(found);
            }
            if is_transparent_wrapper(name) {
                unanimous(children.iter(), candidacy, &dollar_name)
            } else {
                None
            }
        }
        Stmt::Let { bindings, body } => bindings
            .iter()
            .rev()
            .find(|(name, _)| name == "$name")
            .and_then(|(_, expression)| string_literal(expression))
            .or_else(|| unanimous(body.iter(), candidacy, &dollar_name)),
        Stmt::Block(body) | Stmt::For { body, .. } => unanimous(body.iter(), candidacy, &dollar_name),
        Stmt::If {
            then_body,
            else_body,
            ..
        } => unanimous(then_body.iter().chain(else_body), candidacy, &dollar_name),
        Stmt::Root(inner) => dollar_name(inner, candidacy),
        _ => None,
    }
}

/// The name a statement gets from what it calls.
pub(super) fn derived_name(statement: &Stmt, candidacy: &Candidacy) -> Option<String> {
    match statement {
        Stmt::Call {
            name,
            arguments,
            children,
        } => {
            // `import("scans/bracket.stl")` is named after the file, `bracket`:
            // every import would otherwise be called `import`.
            if name == "import" {
                if let Some(stem) = import_stem(arguments) {
                    return Some(stem);
                }
            }
            if is_transparent_wrapper(name) {
                unanimous(children.iter(), candidacy, &derived_name).or_else(|| Some(name.clone()))
            } else {
                Some(name.clone())
            }
        }
        Stmt::Block(body) => unanimous(body.iter(), candidacy, &derived_name).or(Some("group".into())),
        Stmt::For { body, .. } => unanimous(body.iter(), candidacy, &derived_name).or(Some("for".into())),
        Stmt::Let { body, .. } => unanimous(body.iter(), candidacy, &derived_name).or(Some("let".into())),
        Stmt::If {
            then_body,
            else_body,
            ..
        } => unanimous(then_body.iter().chain(else_body), candidacy, &derived_name).or(Some("if".into())),
        Stmt::Root(inner) => derived_name(inner, candidacy),
        _ => None,
    }
}

/// The file stem of an `import()`'s literal file name, if it has one.
fn import_stem(arguments: &[Argument]) -> Option<String> {
    let file = arguments
        .iter()
        .find(|argument| matches!(argument.name.as_deref(), Some("file" | "filename")))
        .or_else(|| arguments.iter().find(|argument| argument.name.is_none()))?;
    let path = string_literal(&file.value)?;
    let stem = std::path::Path::new(&path).file_stem()?.to_str()?.trim();
    (!stem.is_empty()).then(|| stem.to_string())
}

fn slug(name: &str) -> String {
    let mut slug = String::new();
    for character in name.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

/// Identity of each produced object, given the top-level statement each one
/// came from (`origins`, one entry per object, in object order).
pub(super) fn object_identities(
    statements: &[Stmt],
    metas: &[TopLevelMeta],
    origins: &[usize],
) -> Vec<ObjectIdentity> {
    let produced: HashSet<usize> = origins.iter().copied().collect();
    let candidacy = Candidacy::new(statements);
    // Ordinal of each top-level statement among those that can make
    // geometry (or did).
    let mut ordinals = vec![0usize; statements.len()];
    let mut count = 0;
    for (index, statement) in statements.iter().enumerate() {
        if candidacy.can_make(statement) || produced.contains(&index) {
            count += 1;
        }
        ordinals[index] = count;
    }

    let mut used_ids: HashSet<String> = HashSet::new();
    let mut used_names: HashSet<String> = HashSet::new();
    let mut seen_origins: Vec<usize> = Vec::new();
    let mut identities = Vec::with_capacity(origins.len());
    for &origin in origins {
        let repeat = seen_origins.iter().filter(|seen| **seen == origin).count();
        seen_origins.push(origin);
        let statement = statements.get(origin);
        let ordinal = ordinals.get(origin).copied().unwrap_or(origin + 1).max(1);
        let explicit = metas
            .get(origin)
            .and_then(|meta| meta.annotation.clone())
            .or_else(|| statement.and_then(|statement| dollar_name(statement, &candidacy)));
        let (mut name, base_id) = match explicit {
            Some(explicit) => {
                let slug = slug(&explicit);
                let id = if slug.is_empty() {
                    format!("object-{ordinal}")
                } else {
                    format!("object-{slug}")
                };
                (explicit, id)
            }
            None => {
                let derived = statement
                    .and_then(|statement| derived_name(statement, &candidacy))
                    .unwrap_or_else(|| "object".into());
                (format!("{derived} {ordinal}"), format!("object-{ordinal}"))
            }
        };
        if repeat > 0 {
            name = format!("{name} ({})", repeat + 1);
        }
        if used_names.contains(&name) {
            let mut suffix = 2;
            while used_names.contains(&format!("{name} {suffix}")) {
                suffix += 1;
            }
            name = format!("{name} {suffix}");
        }
        let mut id = base_id.clone();
        let mut suffix = 2;
        while used_ids.contains(&id) {
            id = format!("{base_id}-{suffix}");
            suffix += 1;
        }
        used_names.insert(name.clone());
        used_ids.insert(id.clone());
        identities.push(ObjectIdentity { id, name });
    }
    identities
}
