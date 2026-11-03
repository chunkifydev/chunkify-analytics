import { attachAnalytics } from '@chunkify/player-analytics';
import { createHash } from 'node:crypto';

// Synthetic engine measurements, including native playback without rendition tracking.
const ladder = [
  { width: 854, height: 480, codec: 'avc1.64001f', bitrate_bps: 900000, frame_rate: 30 },
  { width: 1280, height: 720, codec: 'avc1.64001f', bitrate_bps: 2500000, frame_rate: 30 },
  { width: 1920, height: 1080, codec: 'avc1.640028', bitrate_bps: 5000000, frame_rate: 30 },
];
function renditionMetrics(index, native, elapsed, total) {
  const stages = index % 4 === 0
    ? [{ ...ladder[1], playing_time_ms: total }]
    : [
        { ...ladder[0], playing_time_ms: Math.floor(total * 0.2) },
        { ...ladder[1], playing_time_ms: Math.floor(total * 0.45) },
        { ...ladder[2], playing_time_ms: total - Math.floor(total * 0.2) - Math.floor(total * 0.45) },
      ];
  const times = [];
  let remaining = elapsed;
  for (const stage of stages) {
    const played = Math.min(stage.playing_time_ms, remaining);
    if (played > 0) times.push({ ...stage, playing_time_ms: played });
    remaining -= played;
  }
  const selected = native ? null : times.at(-1);
  const resolution = elapsed > 0 ? native ? ladder[1] : selected : null;
  return {
    connector_name: native ? 'html-media' : 'hls.js',
    connector_version: null, player_name: native ? null : 'hls.js', player_version: null,
    media_width_px: resolution?.width ?? null,
    media_height_px: resolution?.height ?? null,
    player_width_px: native ? 390 : 960, player_height_px: native ? 219 : 540,
    selected_rendition_width: selected?.width ?? null,
    selected_rendition_height: selected?.height ?? null,
    selected_rendition_codec: selected?.codec ?? null,
    selected_rendition_bitrate_bps: selected?.bitrate_bps ?? null,
    rendition_switch_count: native ? null : Math.max(0, times.length - 1),
    rendition_playing_time: native ? null : times,
  };
}

export function buildDemoEvents(date = new Date()) {
  const lastDay = new Date(date); lastDay.setUTCHours(0, 0, 0, 0);
  const counts = [21, 38, 29, 44, 36, 53, 47];
  const videos = ['getting-started', 'product-tour', 'launch-film', 'adaptive-streaming', 'customer-story', 'live-replay', 'quick-tips', 'workflow-demo', 'behind-the-scenes'];
  const identity = value => { const s = createHash('sha256').update(`chunkify-dashboard-v1:${lastDay.toISOString()}:${value}`).digest('hex'); return `${s.slice(0,8)}-${s.slice(8,12)}-4${s.slice(13,16)}-a${s.slice(17,20)}-${s.slice(20,32)}`; };
  let total = 0;
  const events = [];
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
      const context = { schema_version: 1, user_agent: null, browser_name: mobile ? 'Safari' : n % 5 === 0 ? 'Firefox' : 'Chrome', browser_version: mobile ? '26.0' : '142.0', os_name: mobile ? 'iOS' : 'macOS', os_version: '26.0', device_type: mobile ? 'mobile' : 'desktop', device_family: mobile ? 'iPhone' : 'Mac', language: 'en-US', language_prefix: 'en', timezone: 'Europe/Paris', timezone_offset_minutes: -120, screen_width_px: mobile ? 390 : 1920, screen_height_px: mobile ? 844 : 1080, viewport_width_px: mobile ? 390 : 1440, viewport_height_px: mobile ? 720 : 900, pixel_ratio: mobile ? 3 : 2 };
      const base = { ...template[0], ...renditionMetrics(n, mobile, 0, playing), view_id: identity(`view-${n}`), browser_context: context };
      const end = { ...template[1], ...base, ...renditionMetrics(n, mobile, playing, playing), event_name: 'chunkify.video.view_ended', playing_time_ms: playing, rebuffer_duration_ms: buffering, rebuffer_count: buffering ? 1 : 0, startup_time_ms: failed || missing ? null : 170 + n * 37 % 1850, startup_outcome: failed ? 'failed_before_start' : 'started', maximum_playhead_seconds: failed ? 0 : n % 3 !== 0 ? 600 : playing / 1000, reason: !failed && n % 3 !== 0 ? 'ended' : 'destroyed', furthest_progress_ratio: failed ? 0 : n % 3 !== 0 ? 1 : playing / 600000, rebuffer_ratio: playing + buffering > 0 ? buffering / (playing + buffering) : null, media_duration_seconds: 600, playhead_seconds: failed ? 0 : n % 3 !== 0 ? 600 : playing / 1000, sequence: 3 };
      const records = [
        { ...base, sequence: 1, occurred_at: start.toISOString(), event_id: identity(`start-${n}`) },
        { ...end, event_name: 'chunkify.video.view_snapshot', reason: null, sequence: 2, startup_outcome: null, ...renditionMetrics(n, mobile, playing / 2, playing), playing_time_ms: playing / 2, rebuffer_duration_ms: buffering / 2, maximum_playhead_seconds: end.maximum_playhead_seconds / 2, furthest_progress_ratio: end.furthest_progress_ratio / 2, playhead_seconds: end.playhead_seconds / 2, occurred_at: new Date(start.getTime() + playing / 2 + 1000).toISOString(), event_id: identity(`snapshot-${n}`) },
        { ...end, occurred_at: new Date(start.getTime() + playing + buffering + 2000).toISOString(), event_id: identity(`end-${n}`) },
      ];
      if (failed) records.splice(2, 0, { ...end, event_name: 'chunkify.video.playback_error', reason: null, startup_outcome: null, sequence: 3, error_code: 'DEMO_NETWORK_FAILURE', error_message: 'Synthetic playback failure for the dashboard demo', error_fatal: true, error_category: 'network', occurred_at: new Date(start.getTime() + 1000).toISOString(), event_id: identity(`error-${n}`) });
      if (failed) records.at(-1).sequence = 4;
      events.push(...records);
    }
  }
  return events;
}
