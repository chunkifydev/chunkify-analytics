const $ = id => document.getElementById(id);
let session = null;
let emitted = 0;
let monitor, createChunkifyAdapter, config;
function status(message, error = false) { $('player-status').textContent = message; $('player-status').className = error ? 'error' : ''; }
try {
  const response = await fetch('/api/player-config');
  if (!response.ok) throw new Error('Local demo configuration is unavailable.');
  config = await response.json();
  const modules = await Promise.all([
    import('https://cdn.jsdelivr.net/npm/@chunkify/player@1.3.1/+esm'),
    import('https://cdn.jsdelivr.net/npm/@chunkify/analytics@0.4.0/+esm'),
    import('https://cdn.jsdelivr.net/npm/@chunkify/analytics@0.4.0/chunkify/+esm'),
  ]);
  ({ monitor } = modules[1]); ({ createChunkifyAdapter } = modules[2]);
  await customElements.whenDefined('chunkify-player');
  $('integration-code').textContent = `import { monitor } from "https://cdn.jsdelivr.net/npm/@chunkify/analytics@0.4.0/+esm";
import { createChunkifyAdapter } from "https://cdn.jsdelivr.net/npm/@chunkify/analytics@0.4.0/chunkify/+esm";

const session = monitor(document.querySelector("#player"), {
  videoId: "live-demo-video",
  viewerId: "demo-viewer",
  snapshotIntervalMs: 5000,
  destinations: [createChunkifyAdapter(${JSON.stringify(config.endpoint)}, {
    ingestKey: ${JSON.stringify(config.ingestKey)},
  })],
});

// When your page finishes monitoring this view:
// session.destroy();`;
  $('start').disabled = false; $('copy').disabled = false;
  status('Ready. Enter your video URL and start a view, then refresh Player test analytics to see it arrive.');
} catch (error) { status(`Could not load the demo: ${error.message} Check your internet connection and reload.`, true); }
$('play-form').addEventListener('submit', async event => {
  event.preventDefault();
  const source = new URL($('video-url').value);
  if (!['http:', 'https:'].includes(source.protocol)) { status('Use an HTTP or HTTPS video URL.', true); return; }
  const player = $('player');
  player.media.pause(); session?.destroy(); emitted = 0;
  session = monitor(player, {
    videoId: $('video-id').value, viewerId: 'demo-viewer', snapshotIntervalMs: 5000,
    destinations: [
      createChunkifyAdapter(config.endpoint, { ingestKey: config.ingestKey }),
      { send(event) { $('last-event').textContent = `${++emitted} events emitted · Latest: ${event.event_name}`; } },
    ],
  });
  player.src = source.href;
  $('finish').disabled = false;
  status('Monitoring this view. Press Refresh in the dashboard to check received events.');
  const current = session;
  $('start').disabled = true;
  try {
    if (player.media.localName.includes('-')) await customElements.whenDefined(player.media.localName);
    const media = player.media;
    if (media.readyState < 1) await new Promise((resolve, reject) => {
      const done = error => { clearTimeout(timer); media.removeEventListener('loadedmetadata', loaded); error ? reject(error) : resolve(); };
      const loaded = () => done();
      const timer = setTimeout(() => done(new Error('Media is not ready.')), 15000);
      media.addEventListener('loadedmetadata', loaded, { once: true });
    });
    if (session === current) await media.play();
  } catch { if (session === current) status('Use the player Play control to start playback. Check the video URL if it does not load.'); }
  finally { $('start').disabled = false; }
});
$('finish').addEventListener('click', () => {
  $('player').media.pause(); session?.destroy(); session = null;
  $('finish').disabled = true;
  status('View finished. Refresh Player test analytics to inspect its final summary.');
});
$('copy').addEventListener('click', async () => {
  try { await navigator.clipboard.writeText($('integration-code').textContent); $('copy-status').textContent = 'Copied.'; }
  catch { $('copy-status').textContent = 'Select the code above to copy it.'; }
});
