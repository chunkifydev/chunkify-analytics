const missing = 'Not provided';
let selectedRow = null;
let selectedTrigger = null;
function display(value) {
  if (value == null) return missing;
  if (typeof value === 'object') return JSON.stringify(value);
  return value === '' ? '""' : String(value);
}
function duration(ms) {
  if (ms == null) return missing;
  if (typeof ms !== 'number' || !Number.isFinite(ms) || ms < 0) return display(ms);
  if (ms === 0) return '0s';
  if (ms < 1000) return `${format.format(ms)} ms`;
  const seconds = Math.round(ms) / 1000;
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const remainder = Math.round((seconds % 60) * 1000) / 1000;
  return [hours ? `${hours}h` : '', minutes ? `${minutes}m` : '', remainder ? `${format.format(remainder)}s` : ''].filter(Boolean).join(' ');
}
function timestamp(value) {
  if (value == null) return missing;
  const date = new Date(value);
  return Number.isFinite(date.getTime()) ? date.toISOString().replace('T', ' ').replace('Z', ' UTC') : display(value);
}
function dimensions(width, height) {
  return width == null || height == null ? missing : `${display(width)} × ${display(height)} px`;
}
function version(name, value) {
  return name == null && value == null ? missing : [name, value].filter(v => v != null).map(display).join(' ');
}
function eventName(value) {
  const names = { view_started: 'View started', view_snapshot: 'Snapshot', view_ended: 'View ended', playback_error: 'Playback error' };
  const key = typeof value === 'string' ? value.replace(/^chunkify\.video\./, '') : '';
  return Object.hasOwn(names, key) ? names[key] : display(value);
}
function shortId(value) {
  return typeof value === 'string' && value.length > 18 ? `${value.slice(0, 8)}…${value.slice(-4)}` : display(value);
}
function detailSection(title, fields) {
  const section = document.createElement('section');
  const heading = document.createElement('h3'); heading.textContent = title;
  const list = document.createElement('dl');
  for (const [label, value] of fields) {
    const term = document.createElement('dt'); term.textContent = label;
    const description = document.createElement('dd'); description.textContent = display(value);
    if (value == null || value === missing) description.className = 'missing';
    list.append(term, description);
  }
  section.append(heading, list); return section;
}
function renditionSection(event) {
  const section = document.createElement('section'); section.id = 'rendition-details';
  const heading = document.createElement('h3'); heading.textContent = 'Playing time by rendition';
  const note = document.createElement('p'); note.className = 'rendition-note';
  section.append(heading, note);
  const renditions = event.rendition_playing_time;
  if (!Array.isArray(renditions) || !renditions.length) {
    note.textContent = Array.isArray(renditions)
      ? 'No playing time recorded for a known rendition yet.'
      : 'Not provided. Rendition tracking requires a supported player connector.';
    return section;
  }
  note.textContent = 'Cumulative for this view at this event. Excludes pauses, buffering, seeks and playback before a rendition was known.';
  const wrap = document.createElement('div'); wrap.className = 'table-wrap';
  const table = document.createElement('table');
  table.setAttribute('aria-label', 'Cumulative playing time by rendition');
  const head = table.createTHead().insertRow();
  for (const label of ['Resolution', 'Codec', 'Bitrate', 'Frame rate', 'Playing time']) {
    const cell = document.createElement('th'); cell.scope = 'col'; cell.textContent = label; head.append(cell);
  }
  const body = table.createTBody();
  for (const rendition of renditions) {
    const r = rendition || {};
    const row = body.insertRow();
    for (const value of [dimensions(r.width, r.height), display(r.codec),
      r.bitrate_bps == null ? missing : `${format.format(r.bitrate_bps / 1000000)} Mbps`,
      r.frame_rate == null ? missing : `${display(r.frame_rate)} fps`, duration(r.playing_time_ms)]) {
      row.insertCell().textContent = value;
    }
  }
  wrap.append(table); section.append(wrap);
  // Only sum known numeric durations. Preserve unavailable measurements as unavailable.
  if (renditions.every(r => r && Number.isFinite(r.playing_time_ms) && r.playing_time_ms >= 0)) {
    const total = renditions.reduce((sum, r) => sum + r.playing_time_ms, 0);
    if (Number.isFinite(total)) {
      const label = document.createElement('p'); label.className = 'rendition-total';
      label.textContent = `Total tracked across renditions: ${duration(total)}`;
      section.append(label);
    }
  }
  return section;
}
function openEvent(record, row, trigger) {
  const e = record.event || {};
  const context = record.event_context || {};
  if (selectedRow) selectedRow.classList.remove('selected');
  selectedRow = row; selectedTrigger = trigger; row.classList.add('selected');
  $('detail-title').textContent = eventName(e.event_name);
  $('detail-subtitle').textContent = `Video ${display(e.video_id)}`;
  $('detail-fields').replaceChildren(
    detailSection('Event', [
      ['Event name', e.event_name], ['Event ID', e.event_id], ['View ID', e.view_id],
      ['Viewer ID', e.viewer_id], ['Video ID', e.video_id], ['Sequence', e.sequence],
      ['Occurred at', timestamp(e.occurred_at)], ['Received at', timestamp(record.received_at)],
      ['Project', record.project_id], ['Ingest key ID', record.ingest_key_id],
    ]),
    detailSection('Playback', [
      ['Playing time', duration(e.playing_time_ms)], ['Buffering time', duration(e.rebuffer_duration_ms)],
      ['Buffering count', e.rebuffer_count], ['Seek time', duration(e.seek_duration_ms)], ['Seek count', e.seek_count],
      ['Media resolution', dimensions(e.media_width_px, e.media_height_px)],
      ['Selected rendition', dimensions(e.selected_rendition_width, e.selected_rendition_height)],
      ['Rendition switches', e.rendition_switch_count],
      ['Player size', dimensions(e.player_width_px, e.player_height_px)],
      ['Startup time', duration(e.startup_time_ms)], ['End reason', e.reason],
      ['Error category', e.error_category], ['Error code', e.error_code], ['Fatal error', e.error_fatal],
    ]),
    renditionSection(e),
    detailSection('Context', [
      ['Browser', version(context.browser_name, context.browser_version)],
      ['Operating system', version(context.os_name, context.os_version)],
      ['Device type', context.device_type], ['Device family', context.device_family],
      ['Language', context.language], ['Time zone', context.timezone],
      ['Screen', dimensions(context.screen_width_px, context.screen_height_px)],
      ['Viewport', dimensions(context.viewport_width_px, context.viewport_height_px)],
      ['Client IP', context.client_ip], ['IP source', context.client_ip_source],
      ['SDK', version(e.sdk_name, e.sdk_version)], ['Connector', version(e.connector_name, e.connector_version)],
      ['Player', version(e.player_name, e.player_version)],
    ]),
  );
  $('event-json').textContent = JSON.stringify(record, null, 2);
  $('copy-status').textContent = ''; $('raw-detail').open = false;
  if (!$('event-detail').open) $('event-detail').showModal();
  $('event-detail').scrollTop = 0;
}
function showEvents(data) {
  const body = $('events-body'); body.replaceChildren();
  const complete = data.coverage.complete;
  $('event-coverage').textContent = complete
    ? `Showing ${data.events.length} of the last 10 matching events, newest received first. Click a row for details. Date filters use the event occurrence time.`
    : `The latest events could not be determined within ${format.format(data.coverage.scanned)} records. Narrow the date or video filter, then refresh.`;
  if (!data.events.length) {
    rows('events-body', [], 5);
    if (!complete) body.firstElementChild.firstElementChild.textContent = 'Narrow the filters to see the latest events.';
    return;
  }
  for (const record of data.events) {
    const e = record.event || {}; const row = document.createElement('tr');
    const time = document.createElement('td'); time.textContent = timestamp(e.occurred_at).replace(' UTC', '');
    const name = document.createElement('td'); const trigger = document.createElement('button');
    trigger.type = 'button'; trigger.className = 'event-link'; trigger.textContent = eventName(e.event_name);
    trigger.setAttribute('aria-haspopup', 'dialog'); trigger.setAttribute('aria-controls', 'event-detail');
    trigger.setAttribute('aria-label', `View ${eventName(e.event_name)} details for video ${display(e.video_id)}`);
    name.append(trigger);
    const video = document.createElement('td'); video.textContent = display(e.video_id);
    const view = document.createElement('td'); view.className = 'mono'; view.textContent = shortId(e.view_id); view.title = display(e.view_id);
    const playing = document.createElement('td'); playing.textContent = duration(e.playing_time_ms);
    row.append(time, name, video, view, playing); row.className = 'event-row';
    row.addEventListener('click', () => openEvent(record, row, trigger)); body.append(row);
  }
}
$('close-detail').addEventListener('click', () => $('event-detail').close());
$('event-detail').addEventListener('close', () => {
  selectedRow?.classList.remove('selected');
  if (selectedTrigger?.isConnected) selectedTrigger.focus({ preventScroll: true });
  selectedRow = null; selectedTrigger = null;
});
$('event-detail').addEventListener('click', event => {
  const bounds = $('event-detail').getBoundingClientRect();
  if (event.target === $('event-detail') && (event.clientX < bounds.left || event.clientX > bounds.right || event.clientY < bounds.top || event.clientY > bounds.bottom)) $('event-detail').close();
});
$('copy-json').addEventListener('click', async () => {
  try { await navigator.clipboard.writeText($('event-json').textContent); $('copy-status').textContent = 'Copied.'; }
  catch { $('copy-status').textContent = 'Copy unavailable. Select the JSON below.'; $('event-json').focus(); }
});

