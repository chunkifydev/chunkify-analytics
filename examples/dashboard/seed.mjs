// Synthetic events for a separate demo project. Never use a production ingest key.
import { attachAnalytics } from '@chunkify/analytics';
import { createHash } from 'node:crypto';
import { setTimeout as delay } from 'node:timers/promises';
const endpoint = new URL('/v1/events', process.env.COLLECTOR_URL || 'http://127.0.0.1:4174');
const ingestKey = process.env.CHUNKIFY_DEMO_INGEST_KEY;
if (!ingestKey) throw new Error('Set CHUNKIFY_DEMO_INGEST_KEY for an isolated demo project.');
const lastDay = new Date(); lastDay.setUTCHours(0, 0, 0, 0);
const counts = [21, 38, 29, 44, 36, 53, 47];
const videos = ['getting-started', 'product-tour', 'launch-film', 'adaptive-streaming', 'customer-story', 'live-replay', 'quick-tips', 'workflow-demo', 'behind-the-scenes'];
const identity = value => { const s = createHash('sha256').update(`chunkify-dashboard-v1:${lastDay.toISOString()}:${value}`).digest('hex'); return `${s.slice(0,8)}-${s.slice(8,12)}-4${s.slice(13,16)}-a${s.slice(17,20)}-${s.slice(20,32)}`; };
let total = 0, events = 0;
for (let day = 0; day < counts.length; day++) {
  for (let index = 0; index < counts[day]; index++) {
    const n = total++, mobile = n % 3 === 0, failed = n % 23 === 0, missing = n % 13 === 0;
    const videoId = videos[n % videos.length];
    const media = Object.assign(new EventTarget(), { duration: 600, currentTime: 0, src: '', currentSrc: '', ownerDocument: { defaultView: null } });
    const template = [];
    const monitor = attachAnalytics(media, { videoId, viewerId: `demo-viewer-${n % 91}`, includeEvents: ['view_started', 'view_ended'], destinations: [{ send(event) { template.push(event); } }] });
    monitor.startAttempt(); monitor.destroy();
    const start = new Date(lastDay.getTime() - (6 - day) * 86400000 + index * 240000);
    const playing = failed ? 0 : 45000 + (n * 7901 % 420000);
    const buffering = failed ? 0 : n % 7 === 0 ? 8000 + n * 21 : n % 4 === 0 ? 1200 : 0;
    const context = { schema_version: 1, browser_name: mobile ? 'Safari' : n % 5 === 0 ? 'Firefox' : 'Chrome', browser_version: mobile ? '26.0' : '142.0', os_name: mobile ? 'iOS' : 'macOS', os_version: '26.0', device_type: mobile ? 'mobile' : 'desktop', device_family: mobile ? 'iPhone' : 'Mac', language: 'en-US', language_prefix: 'en', timezone: 'Europe/Paris', timezone_offset_minutes: -120, screen_width_px: mobile ? 390 : 1920, screen_height_px: mobile ? 844 : 1080, viewport_width_px: mobile ? 390 : 1440, viewport_height_px: mobile ? 720 : 900, pixel_ratio: mobile ? 3 : 2 };
    const base = { ...template[0], view_id: identity(`view-${n}`), browser_context: context };
    const end = { ...template[1], ...base, event_name: 'chunkify.video.view_ended', playing_time_ms: playing, rebuffer_duration_ms: buffering, rebuffer_count: buffering ? 1 : 0, startup_time_ms: failed || missing ? null : 170 + n * 37 % 1850, reason: !failed && n % 3 !== 0 ? 'ended' : 'destroyed', media_duration_seconds: 600, playhead_seconds: n % 3 !== 0 ? 600 : playing / 1000, sequence: 3 };
    const records = [
      { ...base, sequence: 1, occurred_at: start.toISOString(), event_id: identity(`start-${n}`) },
      { ...end, event_name: 'chunkify.video.view_snapshot', reason: null, sequence: 2, playing_time_ms: playing / 2, occurred_at: new Date(start.getTime() + playing / 2 + 1000).toISOString(), event_id: identity(`snapshot-${n}`) },
      { ...end, occurred_at: new Date(start.getTime() + playing + buffering + 2000).toISOString(), event_id: identity(`end-${n}`) },
    ];
    if (failed) records.push({ ...end, event_name: 'chunkify.video.playback_error', sequence: 4, error_code: 'DEMO_NETWORK_FAILURE', error_message: 'Synthetic playback failure for the dashboard demo', error_fatal: true, error_category: 'network', occurred_at: new Date(start.getTime() + 1000).toISOString(), event_id: identity(`error-${n}`) });
    for (const event of records) {
      await delay(100);
      const response = await fetch(endpoint, { method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Chunkify-Ingest-Key': ingestKey }, body: JSON.stringify(event), signal: AbortSignal.timeout(15000) });
      if (!response.ok) throw new Error(`Demo ingestion failed: ${response.status} ${await response.text()}`);
      events++;
    }
  }
}
console.log(JSON.stringify({ synthetic: true, views: total, events, from: new Date(lastDay.getTime() - 6 * 86400000).toISOString(), to: new Date(lastDay.getTime() + 86400000).toISOString() }));
