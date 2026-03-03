const CACHE_NAME = 'boox-optimizer-v56';

const APP_SHELL = [
  './',
  'index.html',
  'manifest.json',
  'icon-192.png',
  'icon-512.png',
  'pkg/boox_optimizer.js',
  'pkg/boox_optimizer_bg.wasm',
  'empty.note',
];

self.addEventListener('install', (event) => {
  event.waitUntil(
    caches.open(CACHE_NAME).then((cache) =>
      Promise.all(APP_SHELL.map((url) =>
        fetch(url, { cache: 'reload' }).then((resp) => cache.put(url, resp))
      ))
    )
  );
  self.skipWaiting();
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches.keys().then((keys) =>
      Promise.all(keys.filter((k) => k !== CACHE_NAME && k !== 'share-target').map((k) => caches.delete(k)))
    )
  );
  self.clients.claim();
});

self.addEventListener('fetch', (event) => {
  const url = new URL(event.request.url);

  // Handle Web Share Target POST — stash file and redirect to app
  const sharePath = url.pathname.replace(/\/$/, '');
  if (sharePath.endsWith('/share') && event.request.method === 'POST') {
    event.respondWith((async () => {
      try {
        const formData = await event.request.formData();
        const file = formData.get('file');
        if (file) {
          const cache = await caches.open('share-target');
          await cache.put('/shared-file', new Response(file, {
            headers: { 'X-Filename': file.name, 'Content-Type': 'application/octet-stream' }
          }));
        }
      } catch (e) {
        console.error('Share target stash failed:', e);
      }
      return Response.redirect('./', 303);
    })());
    return;
  }

  // Skip demo.note — large and not essential for offline
  if (url.pathname.endsWith('demo.note')) return;

  // CDN resources (e.g. Tailwind): network-first, fall back to cache
  if (url.origin !== self.location.origin) {
    event.respondWith(
      fetch(event.request)
        .then((response) => {
          const clone = response.clone();
          caches.open(CACHE_NAME).then((cache) => cache.put(event.request, clone));
          return response;
        })
        .catch(() => caches.match(event.request))
    );
    return;
  }

  // Local assets: network-first, fall back to cache (ensures fresh deploys)
  event.respondWith(
    fetch(event.request)
      .then((response) => {
        const clone = response.clone();
        caches.open(CACHE_NAME).then((cache) => cache.put(event.request, clone));
        return response;
      })
      .catch(() => caches.match(event.request))
  );
});
