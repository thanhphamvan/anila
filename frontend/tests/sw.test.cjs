// Notification routing can be verified without starting a browser or sockets.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { runInNewContext } = require('node:vm');

function worker(app) {
  const handlers = {};
  const calls = [];
  const self = {
    location: { origin: 'https://anila.example' },
    addEventListener: (name, fn) => { handlers[name] = fn; },
    clients: {
      matchAll: async () => app ? [app] : [],
      openWindow: async url => calls.push(url),
    },
    registration: { showNotification: async (title, options) => calls.push({ title, options }) },
  };
  runInNewContext(readFileSync(`${__dirname}/../sw.js`, 'utf8'), { self, URL, encodeURIComponent });
  return {
    calls,
    async fire(name, event) {
      let done;
      handlers[name]({ ...event, waitUntil: p => { done = p; } });
      await done;
    },
  };
}

function notification(data) { return { notification: { data, close() {} } }; }

test('an open app receives both the host and session', async () => {
  let message;
  let focused = false;
  const w = worker({ url: 'https://anila.example/', focus: async () => { focused = true; }, postMessage: data => { message = data; } });
  await w.fire('notificationclick', notification({ host: 'h-a', session: 'build' }));
  assert.equal(focused, true);
  assert.equal(message.host, 'h-a');
  assert.equal(message.open, 'build');
  assert.equal(w.calls.length, 0);
});

test('a closed app URL encodes each target and omits empty parameters', async () => {
  const w = worker();
  await w.fire('notificationclick', notification({ host: 'h/a', session: 'a&b' }));
  await w.fire('notificationclick', notification({ host: 'h-a' }));
  await w.fire('notificationclick', notification({ session: 'build' }));
  await w.fire('notificationclick', notification({}));
  assert.deepEqual(w.calls, ['/?host=h%2Fa&session=a%26b', '/?host=h-a', '/?session=build', '/']);
});

test('push titles include the display name and preserve the routing id', async () => {
  const w = worker();
  await w.fire('push', { data: { json: () => ({ title: 'Done', host_name: 'mac-mini', host: 'h-a', session: 'build' }) } });
  assert.equal(w.calls[0].title, 'mac-mini: Done');
  assert.equal(w.calls[0].options.data.host, 'h-a');
  assert.equal(w.calls[0].options.data.session, 'build');
  await w.fire('push', { data: { json: () => ({ title: 'Legacy' }) } });
  assert.equal(w.calls[1].title, 'Legacy');
});
