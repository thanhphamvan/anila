// Bump this version when the shell changes. Hashed Trunk assets fill at runtime.
const CACHE = "anila-shell-v2";
const SHELL = ["/", "/index.html", "/manifest.webmanifest", "/icons/icon.svg",
  "/icons/icon-192.png", "/icons/icon-512.png", "/icons/apple-touch-icon.png",
  "/vendor/xterm/xterm.js", "/vendor/xterm/addon-fit.js", "/vendor/xterm/xterm.css"];

self.addEventListener("install", event => {
  event.waitUntil((async () => {
    const cache = await caches.open(CACHE);
    await cache.addAll(SHELL);
    // The first page loaded before we controlled it: seed its hashed assets too.
    const html = await (await cache.match("/index.html")).text();
    const assets = [...html.matchAll(/(?:src|href)=["']([^"']+\.(?:js|css|wasm))["']/g)]
      .map(match => new URL(match[1], self.location.origin + "/"))
      .filter(url => url.origin === self.location.origin && !/^\/(api|ws)(\/|$)/.test(url.pathname))
      .map(url => url.href);
    await cache.addAll([...new Set(assets)]);
  })());
});

self.addEventListener("activate", event => {
  event.waitUntil((async () => {
    for (const name of await caches.keys()) {
      if (name.startsWith("anila-shell-") && name !== CACHE) await caches.delete(name);
    }
    await self.clients.claim();
  })());
});

self.addEventListener("fetch", event => {
  const request = event.request;
  const url = new URL(request.url);
  if (request.method !== "GET" || url.origin !== self.location.origin ||
      /^\/(api|ws)(\/|$)/.test(url.pathname)) return;
  const update = (async () => {
    const response = await fetch(request);
    if (response.ok && !response.redirected && response.type === "basic") {
      const cache = await caches.open(CACHE);
      await cache.put(request, response.clone());
    }
    return response;
  })();
  // Keep background revalidation alive, including when the cached response wins.
  event.waitUntil(update.catch(() => {}));
  event.respondWith((async () => {
    const cache = await caches.open(CACHE);
    // Pages: network first, so a deploy shows up at once; fall back to the
    // cached shell when offline or after 3 s on a bad link.
    if (request.mode === "navigate") {
      const timeout = new Promise(resolve => setTimeout(resolve, 3000));
      const fresh = await Promise.race([update.catch(() => undefined), timeout]);
      if (fresh) return fresh;
      return (await cache.match(request)) || (await cache.match("/")) || update;
    }
    const cached = await cache.match(request);
    if (cached) return cached;
    // Notification query strings should still open the cached app when offline.
    if (request.mode === "navigate" && url.pathname === "/") {
      const shell = await cache.match("/");
      if (shell) return shell;
    }
    return update;
  })());
});

self.addEventListener("push", event => {
  event.waitUntil((async () => {
    let payload;
    try { payload = event.data.json(); } catch (_) { return; }
    if (!payload || typeof payload.title !== "string") return;
    const { title, body, session, host, host_name } = payload;
    await self.registration.showNotification(host_name ? `${host_name}: ${title}` : title, {
      body, icon: "/icons/icon-192.png", badge: "/icons/icon-192.png",
      tag: session || "anila", renotify: true, data: { session, host }
    });
  })());
});

self.addEventListener("notificationclick", event => {
  event.notification.close();
  event.waitUntil((async () => {
    const session = event.notification.data?.session || "";
    const host = event.notification.data?.host || "";
    const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
    const app = windows.find(client => new URL(client.url).origin === self.location.origin);
    if (app) {
      await app.focus();
      app.postMessage({ open: session, host });
    } else {
      const params = [];
      if (host) params.push("host=" + encodeURIComponent(host));
      if (session) params.push("session=" + encodeURIComponent(session));
      await self.clients.openWindow("/" + (params.length ? "?" + params.join("&") : ""));
    }
  })());
});
