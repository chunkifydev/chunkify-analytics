import assert from 'node:assert/strict';
import { test } from 'node:test';
import { buildDemoEvents } from './seed-events.mjs';

const events = buildDemoEvents(new Date('2026-10-23T12:00:00Z'));
const name = suffix => `chunkify.video.${suffix}`;

test('demo keeps its expected cohort and stable same-day event identities', () => {
  assert.equal(events.length, 816);
  assert.equal(new Set(events.map(e => e.event_id)).size, events.length);
  assert.equal(new Set(events.map(e => e.view_id)).size, 268);
  assert.equal(new Set(events.map(e => e.video_id)).size, 9);
  assert.equal(new Set(events.map(e => e.viewer_id)).size, 91);
  assert.equal(events.filter(e => e.event_name === name('view_ended')).reduce((sum, e) => sum + e.playing_time_ms, 0), 65068260);
  assert.deepEqual(buildDemoEvents(new Date('2026-10-23T23:00:00Z')).map(e => e.event_id), events.map(e => e.event_id));
});

test('rendition times accumulate per view and match the selected rendition and media resolution', () => {
  const views = Map.groupBy(events, e => e.view_id);
  let switched = 0, single = 0, native = 0;
  for (const records of views.values()) {
    const start = records[0], snapshot = records[1], end = records.at(-1);
    assert.equal(start.media_width_px, null);
    assert.equal(start.selected_rendition_width, null);
    assert.equal(snapshot.startup_outcome, null);
    assert.equal(end.event_name, name('view_ended'));
    assert.ok(end.player_width_px > 0 && end.player_height_px > 0);
    assert.ok(end.playing_time_ms === 0 || end.media_width_px > 0 && end.media_height_px > 0);
    assert.deepEqual(records.map(e => e.sequence), records.map((_, i) => i + 1));
    if (end.connector_name === 'html-media') {
      native++;
      for (const event of records) {
        assert.equal(event.rendition_playing_time, null);
        assert.equal(event.selected_rendition_width, null);
        assert.equal(event.rendition_switch_count, null);
      }
      continue;
    }
    assert.deepEqual(start.rendition_playing_time, []);
    for (const event of [snapshot, end]) {
      const times = event.rendition_playing_time;
      assert.equal(times.reduce((sum, r) => sum + r.playing_time_ms, 0), event.playing_time_ms);
      for (const r of times) {
        assert.ok(r.width > 0 && r.height > 0 && r.bitrate_bps > 0 && r.frame_rate > 0 && r.codec);
        assert.ok(r.playing_time_ms <= end.rendition_playing_time.find(final => final.height === r.height).playing_time_ms);
      }
      const selected = times.at(-1);
      assert.equal(event.selected_rendition_width, selected?.width ?? null);
      assert.equal(event.media_height_px, selected?.height ?? null);
    }
    if (end.rendition_playing_time.length > 1) {
      switched++;
      assert.deepEqual(end.rendition_playing_time.map(r => r.height), [480, 720, 1080]);
      assert.equal(end.rendition_switch_count, 2);
      assert.equal(snapshot.rendition_switch_count, 1);
    } else if (end.playing_time_ms > 0) single++;
  }
  assert.ok(switched > 0 && single > 0 && native > 0);
});
