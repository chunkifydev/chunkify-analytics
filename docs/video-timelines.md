# Video timelines

## Engagement along the video

`GET /v1/retention?video_id=VIDEO_ID` returns the number of playback attempts that
played each section of a video. The usual date and audience filters select
retained starts, just as they do for summary reports. A read key is required.

The player sends cumulative `played_ranges` on snapshots and final events. Each
range has `start_seconds` and `end_seconds`. Ranges are sorted and merged;
replaying a section within one attempt does not count as another view. Seeking
past a section, pausing or buffering does not create watched coverage. Range
boundaries come from observed media positions, so a seek can leave a short
unmeasured gap after the last time-update event.

Use one final event per attempt, or its latest snapshot when no final exists.
Never sum cumulative snapshots. A missing or null range array is unmeasured.
An empty array is a measured attempt with no watched section. The player reports
null rather than partial coverage after more than 128 disjoint ranges.

The response contains:

- `views`: matching attempts, including those without range measurements.
- `sample_views`: attempts with a range array and a finite positive duration.
- `duration_seconds`: longest measured duration in this selection.
- `interval_seconds`: `max(1, ceil(duration_seconds / 200))`.
- `buckets`: consecutive sections, with start/end positions, distinct attempt
  count and `ratio = views / sample_views`. A view counts when its played range
  overlaps any part of the section. Empty sections are included with zero counts.

Without measured attempts, duration and interval are null and buckets is empty.
Unknown-duration live streams do not contribute. The duration and interval make
bucket resolution explicit; a multi-second bucket is not an exact second count.
Do not label the denominator unique viewers or infer coverage from furthest progress.

MCP exposes the same report through `get_metrics` with `kind=retention` and a
required `video_id`. CSV uses `resource=retention` and the same query. Both include
all buckets. Grouping, pagination and sorting are not supported for this report.

## Tab visibility

Events may include `visibility_state`: `visible`, `hidden`, or null when the
player cannot read tab visibility. Older events omit it. This is the observed
visibility when the event was recorded, independent of playback state. Hidden
playback can keep accumulating playing time. A transition to `paused` while
hidden can be displayed as "Paused in background"; it does not establish who
caused the pause. Keep that label with the pause that began in the background,
even if the tab is visible by the time playback resumes. Do not infer background
pauses for historical events without visibility measurements. With snapshots
enabled, the SDK records visibility changes immediately. A hidden playing
interval can be labeled "Playing in background" and remains part of measured
playing time.

## Renditions within one playback attempt

Use `GET /v1/views/{view_id}` or MCP `get_view` and follow its cursor to inspect
all retained events for one attempt. The API retains ingestion order. Use the
SDK sequence to display playback order after collecting the event pages, and
ignore repeated event IDs when drawing a chart.

`chunkify.video.rendition_changed` records the media position, selected rendition
and bitrate. The player emits the first selection too, with null previous fields.
A replay begins a new attempt and emits its own initial selection. A backward
seek can revisit the same position with a different rendition: do not flatten
these into a single rendition per second or connect unrelated playback passes.
Older attempts may have changes without an initial-selection event. Do not invent
an initial rendition or infer its start position from the first recorded change.
