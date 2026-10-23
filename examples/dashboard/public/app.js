'use strict';
const $ = id => document.getElementById(id);
const format = new Intl.NumberFormat('en', { maximumFractionDigits: 1 });
const count = value => format.format(value);
const percent = value => value == null ? 'Not available' : `${format.format(value * 100)}%`;
const watch = ms => ms == null ? 'Not available' : ms >= 3600000 ? `${format.format(ms / 3600000)}h` : ms >= 60000 ? `${format.format(ms / 60000)}m` : `${format.format(ms / 1000)}s`;
const startup = ms => ms == null ? 'Not available' : `${format.format(ms)} ms`;
let filters = new URLSearchParams();
let cursors = [null], pageIndex = 0, nextCursor = null, busy = false;
let series = [], projects = [], demo = null;
const descriptions = {
  overview: ['Playback overview', 'An overview of your audience and their viewing experience.'],
  quality: ['Playback quality', 'Understand startup delays, buffering and observed playback errors.'],
  engagement: ['Viewer engagement', 'See how long viewers watch and how often they reach the end.'],
  events: ['Event explorer', 'Look inside the latest events received by your collector.'],
};
function navigate() {
  const page = location.hash.slice(1);
  const active = Object.hasOwn(descriptions, page) ? page : 'overview';
  for (const [id, [title, description]] of Object.entries(descriptions)) {
    $(id).hidden = id !== active;
    const link = document.querySelector(`.tabs a[href="#${id}"]`);
    if (id === active) { link.setAttribute('aria-current', 'page'); $('page-title').textContent = title; $('page-description').textContent = description; }
    else link.removeAttribute('aria-current');
  }
}
window.addEventListener('hashchange', navigate);
navigate();
function rows(id, values, columns) {
  const body = $(id); body.replaceChildren();
  if (!values.length) {
    const row = document.createElement('tr'), cell = document.createElement('td');
    cell.colSpan = columns; cell.className = 'empty'; cell.textContent = 'No matching records.'; row.append(cell); body.append(row);
  }
  for (const valuesInRow of values) {
    const row = document.createElement('tr');
    for (const value of valuesInRow) { const cell = document.createElement('td'); cell.textContent = value == null ? 'Not supplied' : String(value); row.append(cell); }
    body.append(row);
  }
}
function buttons() {
  $('previous').disabled = busy || pageIndex === 0; $('next').disabled = busy || !nextCursor;
  for (const element of $('filters').elements) element.disabled = busy;
  for (const button of document.querySelectorAll('#videos button')) button.disabled = busy;
  $('refresh').textContent = busy ? 'Loading…' : '↻ Refresh';
}
async function read(route, cursor) {
  const query = new URLSearchParams(filters);
  if (cursor) query.set('cursor', cursor);
  if (route === 'timeseries') query.set('interval', 'day');
  const response = await fetch(`/api/${route}?${query}`, { cache: 'no-store', signal: AbortSignal.timeout(30000) });
  const data = await response.json();
  if (!response.ok) throw new Error(data.error || 'Dashboard read failed.');
  return data;
}
function showVideos(data) {
  rows('videos', data.videos.map(v => [v.video_id, count(v.views), count(v.unique_viewers), watch(v.total_playing_time_ms), '']), 5);
  [...$('videos').rows].forEach((row, i) => {
    const video = data.videos[i]; if (!video) return;
    const button = document.createElement('button'); button.className = 'text-button'; button.textContent = 'Explore ↗';
    button.setAttribute('aria-label', `Explore video ${video.video_id}`);
    button.addEventListener('click', () => { if (busy) return; $('video_id').value = video.video_id; $('filters').requestSubmit(); });
    row.lastElementChild.append(button);
  });
  nextCursor = data.next_cursor; $('page').textContent = `Page ${pageIndex + 1}`;
  $('video-coverage').textContent = `${data.videos.length} videos on this page. ${nextCursor ? 'More available.' : 'End of results.'} Totals include every matching video.`;
}
function svgElement(tag, attrs = {}, text) {
  const element = document.createElementNS('http://www.w3.org/2000/svg', tag);
  for (const [key, value] of Object.entries(attrs)) element.setAttribute(key, value);
  if (text != null) element.textContent = text;
  return element;
}
function chart(id, metric) {
  const root = $(id); root.replaceChildren();
  if (!series.length) return;
  const values = series.map(b => b[metric]), isTime = metric === 'total_playing_time_ms';
  const maximum = Math.max(...values, 1);
  const max = isTime ? maximum : Math.ceil(maximum / 4) * 4;
  const svg = svgElement('svg', { viewBox: '0 0 1000 230', role: 'img', 'aria-label': `${isTime ? 'Watch time' : 'Views'} by view start day in UTC. ${series.length} ${series.length === 1 ? 'day' : 'days'}. ${values.every(v => v === 0) ? 'No matching activity.' : 'Exact daily values are available in the chart data table on Overview.'}` });
  const left = 65, width = 915, top = 15, height = 170, slot = width / values.length;
  for (let i = 0; i <= 4; i++) {
    const y = top + height * i / 4;
    svg.append(svgElement('line', { x1: left, x2: left + width, y1: y, y2: y, class: 'gridline' }));
    svg.append(svgElement('text', { x: left - 12, y: y + 4, 'text-anchor': 'end', class: 'axis-label' }, isTime ? watch(max * (4 - i) / 4) : count(max * (4 - i) / 4)));
  }
  values.forEach((value, i) => {
    const barHeight = height * value / max;
    const bar = svgElement('rect', { x: left + slot * i + slot * .19, y: top + height - barHeight, width: Math.max(.5, slot * .62), height: barHeight, rx: Math.min(4, slot * .15), class: isTime ? 'bar time' : 'bar' });
    const day = series[i].bucket_start.slice(0, 10);
    bar.append(svgElement('title', {}, `${day} UTC: ${isTime ? watch(value) : `${count(value)} views`}`)); svg.append(bar);
    if (i % Math.max(1, Math.ceil(values.length / 10)) === 0 || i === values.length - 1) svg.append(svgElement('text', { x: left + slot * (i + .5), y: 214, 'text-anchor': 'middle', class: 'axis-label' }, new Date(series[i].bucket_start).toLocaleDateString('en', { month: 'short', day: 'numeric', timeZone: 'UTC' })));
  });
  root.append(svg);
}
function fillMetrics(summary, quality, engagement, timeline) {
  const values = {
    views: count(summary.views), unique_viewers: count(summary.unique_viewers), playing_time: watch(summary.total_playing_time_ms), average_overview: watch(engagement.average_playing_time_ms),
    startup_preview: startup(quality.startup_time_ms.median), startup_preview_coverage: `${count(quality.startup_time_ms.sample_views)} / ${count(quality.views)} attempts`, buffer_preview_coverage: `${count(quality.buffering.sample_views)} / ${count(quality.views)} attempts`, buffer_preview: percent(quality.buffering.ratio), error_preview: percent(quality.fatal_errors.ratio),
    startup_median: startup(quality.startup_time_ms.median), startup_p95: startup(quality.startup_time_ms.p95), buffer_ratio: percent(quality.buffering.ratio), error_ratio: percent(quality.fatal_errors.ratio),
    startup_coverage: `${count(quality.startup_time_ms.sample_views)} / ${count(quality.views)} attempts`, buffer_coverage: `${count(quality.buffering.sample_views)} / ${count(quality.views)} attempts`,
    quality_views: count(quality.views), fatal_views: count(quality.fatal_errors.affected_views), buffer_time: watch(quality.buffering.total_rebuffer_duration_ms), buffer_playing: watch(quality.buffering.total_playing_time_ms),
    average_time: watch(engagement.average_playing_time_ms), engagement_time: watch(engagement.total_playing_time_ms), end_ratio: percent(engagement.reached_end_ratio), end_views: count(engagement.reached_end_views),
    end_denominator: `Out of ${count(engagement.views)} attempts`, end_observed: `${count(engagement.reached_end_views)} / ${count(engagement.views)}`, end_remaining: `${count(engagement.views - engagement.reached_end_views)} attempts have no observed end-of-media event. They may be ongoing, stopped early or missing final delivery.`,
  };
  for (const [id, value] of Object.entries(values)) $(id).textContent = value;
  $('startup-progress').value = quality.views ? quality.startup_time_ms.sample_views / quality.views : 0;
  $('buffer-progress').value = quality.views ? quality.buffering.sample_views / quality.views : 0;
  $('end-progress').value = engagement.reached_end_ratio ?? 0;
  series = timeline.buckets;
  rows('timeline-data', series.map(b => [b.bucket_start.slice(0, 10), count(b.views), count(b.unique_viewers), watch(b.total_playing_time_ms)]), 4);
  chart('overview-chart', $('chart-metric').value); chart('engagement-chart', 'total_playing_time_ms');
}
async function refresh() {
  if (busy) return;
  busy = true; cursors = [null]; pageIndex = 0; nextCursor = null; buttons();
  $('status').className = ''; $('status').textContent = 'Loading collector data…'; $('results').hidden = true; $('updated').textContent = ''; $('applied').textContent = '';
  if ($('event-detail').open) $('event-detail').close();
  try {
    const [summary, quality, engagement, timeline, videos, events] = await Promise.all([...['summary', 'quality', 'engagement', 'timeseries', 'videos'].map(route => read(route)), read('events').catch(error => ({ error: error.message }))]);
    fillMetrics(summary, quality, engagement, timeline); showVideos(videos);
    if (events.error) {
      $('events-body').replaceChildren(); rows('events-body', [], 5);
      $('event-coverage').textContent = `Events unavailable: ${events.error} Press Refresh to retry. Metrics on other pages are available.`;
    } else showEvents(events);
    const project = projects.find(p => p.id === filters.get('project'));
    $('source-badge').textContent = project.synthetic ? 'Synthetic demo data' : project.name;
    $('applied').textContent = `${project.name} · ${filters.get('from').slice(0, 10)} to ${new Date(Date.parse(filters.get('to')) - 86400000).toISOString().slice(0, 10)} UTC · ${filters.get('video_id') || 'All videos'}${['device_type', 'browser_name', 'os_name', 'viewer_id'].filter(k => filters.has(k)).map(k => ` · ${k.replaceAll('_', ' ')}: ${filters.get(k)}`).join('')}`;
    $('status').textContent = summary.views ? (events.error ? 'Metrics updated. Events are unavailable; see the Events page.' : 'Up to date. Refresh to load new events.') : 'No views match these filters.';
    $('updated').textContent = `Updated ${new Date().toLocaleTimeString('en', { hour: '2-digit', minute: '2-digit', second: '2-digit' })}`;
    $('results').hidden = false;
  } catch (error) { $('status').className = 'error'; $('status').textContent = `${error.message} Change the filters or try Refresh again.`; }
  finally { busy = false; buttons(); }
}
function setDays(days) {
  const today = new Date(); today.setUTCHours(0, 0, 0, 0);
  $('to').value = today.toISOString().slice(0, 10); today.setUTCDate(today.getUTCDate() - days + 1); $('from').value = today.toISOString().slice(0, 10);
}
$('filters').addEventListener('submit', event => {
  event.preventDefault(); if (busy) return;
  const from = new Date(`${$('from').value}T00:00:00Z`), through = new Date(`${$('to').value}T00:00:00Z`);
  const to = new Date(through.getTime() + 86400000);
  if (!Number.isFinite(from.getTime()) || !Number.isFinite(to.getTime()) || from >= to || (to - from) / 86400000 > 1000) {
    $('status').className = 'error'; $('status').textContent = 'Choose a date range of 1 to 1,000 days. Displayed data still uses the applied filters.'; return;
  }
  filters = new URLSearchParams({ project: $('project').value, from: from.toISOString(), to: to.toISOString() });
  for (const key of ['video_id', 'viewer_id', 'device_type', 'browser_name', 'os_name']) if ($(key).value !== '') filters.set(key, $(key).value);
  refresh();
});
$('filters').addEventListener('input', () => { if (!busy) { $('status').className = ''; $('status').textContent = 'Filters changed. Press Refresh to apply them.'; } });
for (const button of document.querySelectorAll('[data-days]')) button.addEventListener('click', () => { setDays(Number(button.dataset.days)); $('filters').requestSubmit(); });
$('chart-metric').addEventListener('change', () => chart('overview-chart', $('chart-metric').value));
async function move(direction) {
  if (busy) return;
  const targetIndex = pageIndex + direction, cursor = direction > 0 ? nextCursor : cursors[targetIndex];
  busy = true; buttons();
  try { const data = await read('videos', cursor); cursors[targetIndex] = cursor; pageIndex = targetIndex; showVideos(data); }
  catch (error) { $('status').className = 'error'; $('status').textContent = error.message; }
  finally { busy = false; buttons(); }
}
$('previous').addEventListener('click', () => move(-1)); $('next').addEventListener('click', () => move(1));
async function initialize() {
  try {
    const response = await fetch('/api/projects', { signal: AbortSignal.timeout(10000) });
    if (!response.ok) throw new Error('Cannot load project configuration.');
    ({ projects, demo = null } = await response.json());
    if (!projects.length) throw new Error('Configure a project read token on the dashboard server, then reload.');
    $('project').replaceChildren(...projects.map(p => new Option(p.name, p.id)));
    const requested = new URLSearchParams(location.search).get('project');
    if (projects.some(project => project.id === requested)) $('project').value = requested;
    if (demo) {
      $('try-player').hidden = false;
      $('sample-dates').hidden = false;
      $('sample-dates').addEventListener('click', () => {
        $('project').value = 'demo'; $('from').value = demo.from; $('to').value = demo.to;
        $('filters').requestSubmit();
      });
      $('project').addEventListener('change', () => {
        if ($('project').value === 'demo') { $('from').value = demo.from; $('to').value = demo.to; }
        else setDays(7);
      });
    }
    setDays(7);
    if (demo && $('project').value === 'demo') { $('from').value = demo.from; $('to').value = demo.to; }
    $('filters').requestSubmit();
  } catch (error) { $('status').className = 'error'; $('status').textContent = error.message; }
}
initialize();
