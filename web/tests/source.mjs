// Pieces of app.js the shared-function helpers cannot reach, to run against a stand-in page.
import assert from 'node:assert/strict';

function closes(text, from, done) {
  const open = '([{';
  const close = ')]}';
  let depth = 0;
  let quote = null;
  for (let index = from; index < text.length; index += 1) {
    const char = text[index];
    if (quote) {
      if (char === '\\') index += 1;
      else if (char === quote) quote = null;
    } else if (char === '/' && text[index + 1] === '/') {
      index = text.indexOf('\n', index);
    } else if (char === '/' && text[index + 1] === '*') {
      index = text.indexOf('*/', index) + 1;
    } else if (char === '\'' || char === '"' || char === '`') {
      quote = char;
    } else if (open.includes(char)) {
      depth += 1;
    } else if (close.includes(char)) {
      depth -= 1;
      if (done(char, depth)) return index;
    } else if (done(char, depth)) {
      return index;
    }
  }
  return assert.fail('unbalanced source');
}

// A top-level statement, from its first characters to its semicolon.
export function statementOf(javascript, marker) {
  const start = javascript.indexOf(marker);
  assert.ok(start >= 0, `${marker} is in app.js`);
  return javascript.slice(start, closes(javascript, start, (char, depth) => char === ';' && depth === 0) + 1);
}

// Class methods, as an object literal's body: `this` is whatever calls them.
export function methodsOf(javascript, names) {
  return names.map((name) => {
    const start = javascript.indexOf(`\n  ${name}(`);
    assert.ok(start >= 0, `${name} is a method in app.js`);
    const signature = closes(javascript, start, (char, depth) => char === ')' && depth === 0);
    const body = javascript.indexOf('{', signature);
    return javascript.slice(start + 3, closes(javascript, body, (char, depth) => char === '}' && depth === 0) + 1);
  }).join(',\n');
}
