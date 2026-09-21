//! The kick-start catalogue and the directory it describes have to agree.
//!
//! The app fetches `examples/manifest.json` and builds every card from it, so a
//! slug with no `.scad` behind it is a card that opens nothing and a `.scad`
//! with no entry is a model nobody can find. Neither failure shows up until
//! someone clicks, which is why it is checked here instead.
//!
//! What this file cannot check is whether the models are *correct* — that is
//! `cargo test`'s job on the Rust side, and `tests/oracle.sh`'s against the
//! OpenSCAD binary.

import assert from 'node:assert/strict';
import { readFile, readdir, stat } from 'node:fs/promises';
import test from 'node:test';

const directory = new URL('../examples/', import.meta.url);
const manifest = JSON.parse(await readFile(new URL('manifest.json', directory), 'utf8'));
const entries = await readdir(directory);

const examples = manifest.categories.flatMap((category) =>
  category.examples.map((example) => ({ ...example, category: category.id })),
);

test('every catalogue entry has a source and a preview next to it', async () => {
  for (const example of examples) {
    for (const extension of ['.scad', '.png']) {
      const file = `${example.slug}${extension}`;
      assert.ok(entries.includes(file), `${example.slug} is listed but ${file} is missing`);
      const { size } = await stat(new URL(file, directory));
      assert.ok(size > 0, `${file} is empty`);
    }
  }
});

test('every source in the directory is listed in the catalogue', () => {
  const listed = new Set(examples.map((example) => example.slug));
  const present = entries.filter((name) => name.endsWith('.scad')).map((name) => name.slice(0, -5));
  const unlisted = present.filter((slug) => !listed.has(slug));
  assert.deepEqual(unlisted, [], 'these models exist but no card opens them');
  // And nothing is left behind: a preview whose source was deleted would keep
  // being shipped in the image.
  const orphans = entries
    .filter((name) => name.endsWith('.png'))
    .map((name) => name.slice(0, -4))
    .filter((slug) => !listed.has(slug));
  assert.deepEqual(orphans, [], 'these previews have no catalogue entry');
});

test('every entry is described well enough to choose from', () => {
  const slugs = new Set();
  for (const example of examples) {
    assert.match(example.slug, /^[a-z0-9]+(-[a-z0-9]+)*$/, `${example.slug} is not a clean slug`);
    assert.ok(!slugs.has(example.slug), `${example.slug} is listed twice`);
    slugs.add(example.slug);
    assert.ok(example.name?.length > 2, `${example.slug} has no name`);
    // Long enough to say something, short enough for a card two lines high.
    assert.ok(
      example.blurb?.length > 15 && example.blurb.length <= 90,
      `${example.slug}: blurb is ${example.blurb?.length} characters`,
    );
    assert.ok(example.hint?.length > 0, `${example.slug} has no feature hint`);
  }
});

test('the feature hint on a card names something the model actually calls', async () => {
  for (const example of examples) {
    const source = await readFile(new URL(`${example.slug}.scad`, directory), 'utf8');
    // Hints read like `text() · offset()` or `linear_extrude(twist=)`; take the
    // identifier from each part and require the source to call it. A hint that
    // advertises a feature the model does not use is a lie on the card.
    const called = example.hint
      .split('·')
      .map((part) => part.trim().match(/^([a-z_][a-z0-9_]*)\s*\(/i)?.[1])
      .filter(Boolean);
    for (const name of called) {
      assert.match(
        source,
        new RegExp(`\\b${name}\\s*\\(`),
        `${example.slug} advertises ${name}() but never calls it`,
      );
    }
  }
});

test('every model is parametric and says what it is', async () => {
  for (const example of examples) {
    const source = await readFile(new URL(`${example.slug}.scad`, directory), 'utf8');
    const lines = source.split('\n');
    assert.match(lines[0], /^\/\//, `${example.slug} does not open with a description`);
    // At least a handful of named, commented parameters before any geometry:
    // the cards promise "edit the numbers at the top", and that has to be true.
    const parameters = lines.filter((line) => /^\w+\s*=\s*[^;]+;\s*\/\//.test(line));
    assert.ok(
      parameters.length >= 4,
      `${example.slug} has ${parameters.length} documented parameters`,
    );
    // Features the engine does not implement, which would make a card open a
    // model that cannot render.
    for (const unsupported of ['polyhedron', 'import', 'surface', 'projection', 'multmatrix']) {
      assert.doesNotMatch(
        source,
        new RegExp(`\\b${unsupported}\\s*\\(`),
        `${example.slug} calls ${unsupported}(), which this engine cannot render`,
      );
    }
  }
});

test('the catalogue covers the categories the empty workspace advertises', () => {
  const ids = manifest.categories.map((category) => category.id);
  assert.deepEqual(ids, ['mounts', 'threads', 'hinges', 'text']);
  for (const category of manifest.categories) {
    assert.ok(category.name?.length > 2, `${category.id} has no name`);
    assert.ok(category.blurb?.length > 10, `${category.id} has no blurb`);
    assert.equal(category.examples.length, 3, `${category.id} should offer three models`);
  }
});
