const { test } = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { runInNewContext } = require('node:vm');

function setup(viewport) {
  const properties = new Map();
  let resizes = 0;
  runInNewContext(readFileSync(`${__dirname}/../viewport.js`, 'utf8'), {
    window: { visualViewport: viewport, dispatchEvent: () => resizes++ },
    document: { documentElement: { style: { setProperty: (key, value) => properties.set(key, value) } } },
    Event,
  });
  return { properties, resizes: () => resizes };
}

test('mobile keyboard opening and closing refits the terminal after changing height', () => {
  let resize;
  const viewport = { height: 844, scale: 1, addEventListener: (name, fn) => { if (name === 'resize') resize = fn; } };
  const app = setup(viewport);
  assert.equal(app.properties.get('--app-height'), '844px');
  viewport.height = 360;
  resize();
  assert.equal(app.properties.get('--app-height'), '360px');
  viewport.height = 844;
  resize();
  assert.equal(app.properties.get('--app-height'), '844px');
  assert.equal(app.resizes(), 3);
  resize();
  assert.equal(app.resizes(), 3, 'unchanged dimensions must not cause resize loops');
});

test('pinch zoom and zero-sized viewports do not resize the terminal', () => {
  let resize;
  const viewport = { height: 844, scale: 1, addEventListener: (_, fn) => { resize = fn; } };
  const app = setup(viewport);
  viewport.height = 400;
  viewport.scale = 2;
  resize();
  assert.equal(app.properties.get('--app-height'), '844px');
  viewport.scale = 1;
  viewport.height = 0;
  resize();
  assert.equal(app.resizes(), 1);
});

test('keyboard viewport panning moves the app without changing terminal dimensions', () => {
  let scroll;
  const viewport = { height: 360, scale: 1, offsetTop: 0, addEventListener: (name, fn) => { if (name === 'scroll') scroll = fn; } };
  const app = setup(viewport);
  viewport.offsetTop = 80;
  scroll();
  assert.equal(app.properties.get('--app-top'), '80px');
  assert.equal(app.resizes(), 1);
});

test('browsers without VisualViewport retain CSS sizing', () => {
  const app = setup(undefined);
  assert.equal(app.properties.size, 0);
  assert.equal(app.resizes(), 0);
});
