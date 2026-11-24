// vocsend service worker: keeps a copy of the app on the device so that it
// opens with no network at all. It only ever fetches the site's own files.
// Online, the newest version is always used; the copy is for when the
// network is missing or slow.

const CACHE = "vocsend-v1";
const FILES = [
  "./", "cli.html", "style.css", "app.js", "worker.js", "tap.js",
  "pkg/vocsend_web.js", "pkg/vocsend_web_bg.wasm",
  "manifest.webmanifest", "favicon.svg", "touch-icon.png", "icon-192.png", "icon-512.png", "icon-maskable.png",
];

self.addEventListener("install", event => {
  event.waitUntil(caches.open(CACHE).then(cache => cache.addAll(FILES)).then(() => self.skipWaiting()));
});

self.addEventListener("activate", event => {
  event.waitUntil(
    caches.keys()
      .then(names => Promise.all(names.filter(n => n !== CACHE).map(n => caches.delete(n))))
      .then(() => self.clients.claim()),
  );
});

function fresh(request, cache) {
  return fetch(request).then(response => {
    if (response.ok) cache.put(request, response.clone());
    return response;
  });
}

self.addEventListener("fetch", event => {
  const request = event.request;
  if (request.method !== "GET" || new URL(request.url).origin !== location.origin) return;
  event.respondWith((async () => {
    const cache = await caches.open(CACHE);
    const saved = await cache.match(request, { ignoreSearch: true });
    const network = fresh(request, cache);
    if (!saved) return network;
    network.catch(() => {});
    // Give the network three seconds, then fall back to the saved copy.
    const slow = new Promise(resolve => setTimeout(() => resolve(saved), 3000));
    return Promise.race([network.catch(() => saved), slow]);
  })());
});
