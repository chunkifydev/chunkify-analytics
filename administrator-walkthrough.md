# Administrator and Docker acceptance, 2026-10-21

## Result

Passed the administrator walkthrough and Docker lifecycle acceptance in
[issue #381](https://github.com/chunkifydev/chunkify/issues/381). The checks covered
two projects, real browser playback, credentials, container replacement,
backup/restore, and upgrading a retained pre-projects database to schema 5.
No collector or SDK code change was needed.

Capacity validation and the final release documentation/contract check remain
pending. These small fixtures establish behavior, not a requests-per-second limit.

## Environment and evidence

- Source baseline: `48a72bd4e53e4ddc6596e299bc94f254751f5a05`.
- Existing Linux arm64 image: `sha256:38dff245041d410fe55b0c04b4528b285b40674216b4f8223cdc4b63b62a953b`.
  Its recorded production-source manifest matched the baseline. Later test-only
  module declarations were excluded from that comparison. No image rebuild was
  needed for these checks.
- Docker Desktop on macOS; each collector had two CPUs, 1 GiB RAM, one DuckDB
  thread and direct ingestion. Retention was disabled for the fixture. These
  settings are not a capacity recommendation.
- Containers ran as UID 10001, with a read-only root filesystem, a temporary
  `/tmp`, dropped capabilities and `no-new-privileges`. Ports bound to loopback.
- Published `@chunkify/analytics@0.4.0`; headless Chrome 155 played a real MP4
  through a native video element, `monitor` and `createChunkifyAdapter`.
- Temporary Node assertions exercised the public CLI and HTTP API. They compared
  full event records, context, original OTLP data, summaries and video metrics.
  These were local acceptance checks, not new CI tests.
- The retained legacy executable was macOS arm64, saved before project support.
  SHA-256: `5bec697ce7258791b606a7cd7e382f0966bd68c20d63305f1744cee3e2eb1c0a`.
  Its stopped data directory was restored into a fresh Docker volume, then
  opened by the current image. This checks a legacy database upgrade across
  platforms; it is not an old-image-to-new-image build test.

## Verified behavior

| Check | Result |
| --- | --- |
| Create `live` and `staging` with names and named credentials | Passed. Administrator listings included safe key IDs and names, without secrets. |
| Browser SDK ingestion | Passed. A start and end produced one view and about 1,032.9 ms of playing time. Browser context, request IP, project and source key ID were present. |
| Project isolation | Passed with overlapping event, view, video and viewer IDs. Staging used OTLP and reported its own 7,000 ms view. A payload claiming the live project could not override the staging key's ownership. |
| Read and ingest permissions | Passed. Missing or wrong credentials returned 401. Ingest keys could not read, read tokens could not ingest, and another project's cursor returned 400. |
| Browser origins | Passed. Live accepted its configured origin; an unrelated origin and the staging project's empty origin policy returned 403. |
| Replace a container | Passed. Graceful shutdown exited with code 0. A new container using the same volume returned identical records, context and metrics. |
| Rotate and revoke credentials | Passed after stopping and restarting the service. Old ingest/read secrets and a revoked named read token returned 401. Replacement secrets worked and the source key ID stayed stable. |
| Rename and viewer deletion | Passed. The project name changed without changing ownership. Deleting the shared viewer from live left staging's matching viewer intact. |
| Backup and restore | Passed using the README's non-root `tar` commands and a new, unused volume, without `init`. Both projects' records, metrics, settings and credential registry matched the backup. New writes worked after restore. |
| Upgrade to schema 5 | Passed. Four legacy events, including native and OTLP input, retained their payloads, context, receipt times and old pagination cursor. Their owner became `default`, with unknown historical key IDs. Existing credentials still worked. |
| Retry and write after upgrade | Passed. Duplicate native/OTLP delivery left records and totals unchanged. Two new events raised the fixture from two views / 3,500 ms to three views / 4,000 ms. Container replacement preserved all six events. |
| Roll back an upgrade | Passed by restoring the pre-upgrade backup and starting the old executable. The old executable rejected the upgraded database with `Unsupported database schema version`. |
| Secrets in logs | No generated ingest or read secret appeared in the captured server logs. Key files retained mode 0600. |

The OTLP fixture supplied non-null browser fields explicitly. The generic OTLP
adapter does not collect browser context and omits null attributes during
encoding, including nested null values. This walkthrough did not change that SDK
behavior. Native ingestion retained the Chunkify adapter's complete payload.

## Backup consequences

The administrator backup was 19,755 bytes for this tiny fixture. This compressed
archive size is not an estimate of per-event storage at scale.

After taking the backup, the walkthrough rotated a read key, deleted a viewer
and inserted a new view. Restoring the backup brought back its earlier key and
viewer data, and omitted the later view. The post-backup key no longer worked.
Administrators must reapply subsequent key rotations/revocations and required
deletions after a restore. A backup contains credentials and must stay private.

## Reproduce the administrator flow

1. Initialize a new persistent volume and create two named projects using the
   offline CLI. Create a named ingest key for the website and a named read key
   for its dashboard backend. Follow [project setup](README.md#projects-and-named-keys).
2. Configure the website origin for one project. Start the container and use
   `createChunkifyAdapter` with that project's public ingest key. Keep read tokens
   in the backend or terminal. Play a video and end monitoring so its final
   cumulative playing time is sent.
3. Read `/v1/events`, `/v1/summary` and `/v1/videos` separately with each project's
   read token. Send overlapping IDs to the second project and verify that totals
   remain separate. Check rejected credentials, origins and cross-project cursors.
4. Stop the service, rotate its named ingest/read keys, revoke a temporary key
   and restart. Verify old secrets fail and new secrets work. Replace the
   container while keeping its volume and repeat the reads.
5. Follow [container backup/restore](README.md#container-administration-and-backups).
   Compare complete records and totals before backup and after restoration into
   a new volume. Verify a new event can be written after restore.
6. For an upgrade, first write a small fixture using a retained old executable.
   Stop it and archive the entire data directory. Open a restored copy with the
   current image and compare records, metrics, credentials, cursors and retries.
   Keep the pre-upgrade archive for a rollback check with the old executable.

## Cleanup and limits

All containers, volumes, legacy test data and credential-bearing backup archives
created by these checks were removed. The existing player/dashboard demo and its
data were left running and unchanged. The retained old executable was kept for
future migration checks.

The local assertion scripts and nonsecret result summaries remain under
`/private/tmp/chunkify-admin-acceptance-20261021`. No customer credentials or
customer events were used. A few runner expectations were corrected while
preparing the checks: CLI `init` returns text, OTLP omits null attributes, event
selection must match the intended fixture, and the legacy schema error does not
print a version number. The completed runs passed without changing product code.

This checks one retained legacy executable and the current image, direct-mode
graceful lifecycle operations and small fixtures. It does not exercise every
historical schema, pending experimental durable-queue upgrades, abrupt power
loss, high-load recovery, or production capacity. Those claims require their
own tests. CSV export and S3 archiving remain deferred.
