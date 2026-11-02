import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const html = await readFile(new URL('../index.html', import.meta.url), 'utf8');
const javascript = await readFile(new URL('../app.js', import.meta.url), 'utf8');

function sourceOf(name) {
  const start = javascript.indexOf(`function ${name}(`);
  assert.ok(start >= 0, `${name} is defined`);
  let depth = 0;
  let index = javascript.indexOf('{', javascript.indexOf(')', start));
  for (; index < javascript.length; index += 1) {
    if (javascript[index] === '{') depth += 1;
    if (javascript[index] === '}' && --depth === 0) break;
  }
  return javascript.slice(start, index + 1);
}

const pattern = javascript.match(/^const PRINT_DIRECTIVE_PATTERN = (.*);$/m)[1];
const helpers = ['parsePrintDirectives', 'printDirectiveFor', 'printDirectiveLabel'];
const lib = new Function(`const PRINT_DIRECTIVE_PATTERN = ${pattern};\n${sourceOf('linesStartingInCode')}\n${helpers.map(sourceOf).join('\n')}\nreturn { ${helpers.join(', ')} };`)();

test('print directives parse like the server reads them', () => {
  const code = '// Requirements: x\n// @print object-lid rotate=[180, 0, 0]\n//@print "Base plate" face=+x\n// @print bad\n// @printer no\ncube(1);\n';
  const directives = lib.parsePrintDirectives(code);
  assert.deepEqual(directives, [
    { line: 2, object: 'object-lid', key: 'rotate', value: '[180, 0, 0]' },
    { line: 3, object: 'Base plate', key: 'face', value: '+x' },
  ]);
  assert.equal(lib.printDirectiveFor(directives, { id: 'object-lid', name: 'Lid' }).line, 2);
  assert.equal(lib.printDirectiveFor(directives, { id: 'object-2', name: 'base plate' }).line, 3);
  assert.equal(lib.printDirectiveFor(directives, { id: 'object-3', name: 'Other' }), null);
  assert.equal(lib.printDirectiveLabel(directives[0]), 'rotated [180, 0, 0]');
  assert.equal(lib.printDirectiveLabel(directives[1]), '+x face down');
  assert.equal(lib.printDirectiveLabel({ key: 'face', value: 'Z' }), '+z face down');
  // Malformed lines the server ignores do not hide a valid later one; CRLF is fine.
  const crlf = lib.parsePrintDirectives('// @print a rotate=[1,2]\r\n// @print a face=[0,0,0]\r\n// @print a face=-z\r\n');
  assert.deepEqual(crlf.map((directive) => directive.line), [3]);
});

test('directives inside strings and block comments are text, as on the server', () => {
  const code = 'label = "x\\"\n// @print a rotate=[1, 2, 3]\n";\n/* note\n// @print a face=+x\n*/\n// @print a face=-y\ncube(1); // @print b face=+z\n';
  assert.deepEqual(lib.parsePrintDirectives(code).map((directive) => directive.line), [7]);
});

test('the object list reads the source the new objects were compiled from', () => {
  const at = javascript.indexOf('setSceneObjects(Array.isArray(data.objects) ? data.objects : sceneObjects, { placeholder: true });');
  const assign = javascript.lastIndexOf('if (Array.isArray(data.objects)) sceneObjectsSource = source;', at);
  assert.ok(at > 0 && assign > 0 && at - assign < 200, 'source assigned right before the list renders');
});

test('the print layout preview is gone; print tags on object rows stay', () => {
  assert.doesNotMatch(html, /layoutToggle|layout-field/);
  assert.doesNotMatch(javascript, /layoutToggle|printLayout|PrintLayout|layoutBatches|print-layout/);
  assert.match(javascript, /class="object-print"/);
});
