# Changelog

## [0.9.0](https://github.com/chunkifydev/chunkify-analytics/compare/v0.8.0...v0.9.0) (2026-10-30)


### ⚠ BREAKING CHANGES

* **analytics:** Existing session and video pagination cursors are invalid. Start a new pagination sequence after upgrading.

### Features

* **analytics:** add CSV column selection and report sorting ([#52](https://github.com/chunkifydev/chunkify-analytics/issues/52)) ([a093f5b](https://github.com/chunkifydev/chunkify-analytics/commit/a093f5ba6fb9bb51065cc70e3c705d707bd6a4a4))
* **metrics:** add playback scores across analytics reports ([#50](https://github.com/chunkifydev/chunkify-analytics/issues/50)) ([7233767](https://github.com/chunkifydev/chunkify-analytics/commit/72337676842a19bea815568290cdbf6ed0982fc0))

## [0.8.0](https://github.com/chunkifydev/chunkify-analytics/compare/v0.7.0...v0.8.0) (2026-10-30)


### ⚠ BREAKING CHANGES

* **views:** Views default to descending start time instead of ascending view ID. Existing view cursors must be restarted.

### Bug Fixes

* **views:** sort playback attempts newest first ([#48](https://github.com/chunkifydev/chunkify-analytics/issues/48)) ([1b1948c](https://github.com/chunkifydev/chunkify-analytics/commit/1b1948c96ea7784e486073d9821ccb12968da079))

## [0.7.0](https://github.com/chunkifydev/chunkify-analytics/compare/v0.6.0...v0.7.0) (2026-10-30)


### Features

* **analytics:** add page origin and path reports ([#45](https://github.com/chunkifydev/chunkify-analytics/issues/45)) ([c9054e1](https://github.com/chunkifydev/chunkify-analytics/commit/c9054e1d1523a3198e79a9c38fada34af030a8bf))
* **analytics:** report watched sections along a video ([#47](https://github.com/chunkifydev/chunkify-analytics/issues/47)) ([9b7d9a2](https://github.com/chunkifydev/chunkify-analytics/commit/9b7d9a228174b94bd0840d8354c07e4aaf81d904))

## [0.6.0](https://github.com/chunkifydev/chunkify-analytics/compare/v0.5.0...v0.6.0) (2026-10-30)


### ⚠ BREAKING CHANGES

* **analytics:** remove server.store_ip and request IP context fields. Serving now requires a City MMDB, included in Docker images and native release archives. Existing stored events are not rewritten.

### Features

* **analytics:** add local geolocation and location reports ([#43](https://github.com/chunkifydev/chunkify-analytics/issues/43)) ([e108aa9](https://github.com/chunkifydev/chunkify-analytics/commit/e108aa93f48fe8b617beb9db54109c0764f966f3))

## [0.5.0](https://github.com/chunkifydev/chunkify-analytics/compare/v0.4.0...v0.5.0) (2026-10-29)


### Features

* **analytics:** add rendition watch-time and bitrate reports ([#41](https://github.com/chunkifydev/chunkify-analytics/issues/41)) ([d0ebde6](https://github.com/chunkifydev/chunkify-analytics/commit/d0ebde6664ac18b74837c88aa8918da49498c040))

## [0.4.0](https://github.com/chunkifydev/chunkify-analytics/compare/v0.3.0...v0.4.0) (2026-10-29)


### Features

* **logging:** report ingestion batch outcomes ([#39](https://github.com/chunkifydev/chunkify-analytics/issues/39)) ([df3cc7b](https://github.com/chunkifydev/chunkify-analytics/commit/df3cc7b7d2f7cde1692c79382686221c5603ce39))

## [0.3.0](https://github.com/chunkifydev/chunkify-analytics/compare/v0.2.0...v0.3.0) (2026-10-29)


### ⚠ BREAKING CHANGES

* **auth:** Only the hash-only project registry format is accepted; previous development registries must be reinitialized.

### Bug Fixes

* **auth:** persist only hashed project credentials ([#37](https://github.com/chunkifydev/chunkify-analytics/issues/37)) ([4469734](https://github.com/chunkifydev/chunkify-analytics/commit/4469734fb2184e353acbed36933aa9e5bf8131a3))

## [0.2.0](https://github.com/chunkifydev/chunkify-analytics/compare/v0.1.0...v0.2.0) (2026-10-29)


### Features

* **api:** add project retention and key creation ([#34](https://github.com/chunkifydev/chunkify-analytics/issues/34)) ([9994b76](https://github.com/chunkifydev/chunkify-analytics/commit/9994b763df76a707da013af22057688772056188))

## 0.1.0 (2026-10-28)


### ⚠ BREAKING CHANGES

* **mcp:** Replace CHUNKIFY_MCP_COLLECTOR_URL with CHUNKIFY_MCP_URL and mcp.collector_url with mcp.url in existing stdio adapter configurations.
* **config:** Replace COLLECTOR_* application overrides with CHUNKIFY_* equivalents. Use CHUNKIFY_URL for dashboard connections and CHUNKIFY_BINARY for test and demo executables. Copy .env.example instead of collector.env.example; the former application overrides are no longer recognized.
* **api:** The default configuration filename is now config.toml and the template is config.toml.example. Rename deployment configuration files and Docker bind mounts, or select an existing file explicitly with --config.
* **analytics:** Replace the get_summary, get_quality and get_engagement MCP tools with get_metrics. Callers must supply kind=summary, kind=quality or kind=engagement. HTTP routes remain unchanged.
* **api:** Video pagination cursors issued by earlier versions are no longer accepted. Restart video pagination after upgrading.
* **api:** Remove POST and OPTIONS /v1/logs, GET /v1/ingestion, and server.max_otlp_records. Clients must use createChunkifyAdapter with POST /v1/events and remove the obsolete setting from collector.toml.
* **cli:** replace flat administration command names with project, key, and viewer subcommands.
* the executable and Cargo package are now named chunkify-analytics.
* **cli:** replace flat administration command names with project, key, and viewer subcommands.
* the executable and Cargo package are now named chunkify-analytics.

### Features

* add a local analytics dashboard demo ([8003d49](https://github.com/chunkifydev/chunkify-analytics/commit/8003d49188a417b55125476903e6ec907c1de327))
* add analytics collector retention and administration ([547dbff](https://github.com/chunkifydev/chunkify-analytics/commit/547dbff3b9ab629e128cf7eabf458fc2378c40e5))
* add first stage of self-hosted analytics collector ([e91500c](https://github.com/chunkifydev/chunkify-analytics/commit/e91500c610c7e7f28bd6a3f50d6c7c4d4ad60f06))
* add native coworker demo launcher and shareable bundle ([b2b8bb6](https://github.com/chunkifydev/chunkify-analytics/commit/b2b8bb6cc56ffd50b58e82eaceeff45ec0b07b92))
* add OTLP JSON ingestion to analytics collector ([7e3a9f6](https://github.com/chunkifydev/chunkify-analytics/commit/7e3a9f6f24fcbe545f6abf44cefb63839931ddcc))
* add playback quality summaries through HTTP and MCP ([b09f18b](https://github.com/chunkifydev/chunkify-analytics/commit/b09f18b0f1920f78bee7227ea5c0041eb095f703))
* add watch time and reached-end engagement summaries ([2ddad6d](https://github.com/chunkifydev/chunkify-analytics/commit/2ddad6d9e981ea69b32bd1fc25e05eb95d8c8d15))
* administer collector keys without restarting ([db6334b](https://github.com/chunkifydev/chunkify-analytics/commit/db6334b0639bccf0542883ebb35d386697d357b8))
* **analytics:** add breakdowns and unify MCP metrics ([#23](https://github.com/chunkifydev/chunkify-analytics/issues/23)) ([9d45514](https://github.com/chunkifydev/chunkify-analytics/commit/9d455144d04344f8a8ff6fe66c18cc017ac39581))
* **analytics:** add exploration and streaming CSV exports ([#24](https://github.com/chunkifydev/chunkify-analytics/issues/24)) ([cf05073](https://github.com/chunkifydev/chunkify-analytics/commit/cf05073fe8173ebdd4dc6bb700d62aebcea625c9))
* **analytics:** add progress and playback outcome reports ([#32](https://github.com/chunkifydev/chunkify-analytics/issues/32)) ([aa5c44f](https://github.com/chunkifydev/chunkify-analytics/commit/aa5c44f7e3062a246c1e34acb4998bfa7583b7fa))
* **api:** add admin recovery for lost project keys ([#31](https://github.com/chunkifydev/chunkify-analytics/issues/31)) ([5be1c5a](https://github.com/chunkifydev/chunkify-analytics/commit/5be1c5a9e89280b4d07a053c43bfe0e9e1d3eee0))
* **api:** add configurable rate limits and rename config file ([#25](https://github.com/chunkifydev/chunkify-analytics/issues/25)) ([379227f](https://github.com/chunkifydev/chunkify-analytics/commit/379227fe3df5b688a8295128333d739ec4056407))
* **api:** add video rankings and expand MCP video pages ([#22](https://github.com/chunkifydev/chunkify-analytics/issues/22)) ([713d109](https://github.com/chunkifydev/chunkify-analytics/commit/713d109395865d28269f5ad380503650ece48cbe))
* **cli:** add structured administration subcommands ([129be23](https://github.com/chunkifydev/chunkify-analytics/commit/129be23b48625d4cfaee4f8d870542e5c867fee5))
* **cli:** add structured administration subcommands ([#9](https://github.com/chunkifydev/chunkify-analytics/issues/9)) ([1b2e759](https://github.com/chunkifydev/chunkify-analytics/commit/1b2e75998e3c163f9b0034b1702fdaafcfdac750))
* **collector:** add opt-in durable ingestion queue ([419d472](https://github.com/chunkifydev/chunkify-analytics/commit/419d4722e2f1af63a1b7211893ff8292f85a1636))
* **collector:** add read-only MCP companion for dashboards ([0b8fe68](https://github.com/chunkifydev/chunkify-analytics/commit/0b8fe687dfd89c92d99a1b0449aa813b28503b12))
* **collector:** filter analytics by browser OS and device type ([3e2e4e5](https://github.com/chunkifydev/chunkify-analytics/commit/3e2e4e5b3cf7b52c083039ed66263b1ab5e948d8))
* **collector:** isolate analytics by named project ([999825e](https://github.com/chunkifydev/chunkify-analytics/commit/999825ecc9a7c5054c36003ad3db25953b2cd22c))
* **collector:** publish and verify the OpenAPI contract ([636de32](https://github.com/chunkifydev/chunkify-analytics/commit/636de3210d13afe8d49411e98526d580dcc781cf))
* **collector:** store event scalars without duplicate JSON ([c092c1a](https://github.com/chunkifydev/chunkify-analytics/commit/c092c1a955e80d1bae02af607d3a22d451de8293))
* **collector:** store flat browser and trusted IP context ([17d8d4f](https://github.com/chunkifydev/chunkify-analytics/commit/17d8d4f1ac07ea5fb0a4fa0a555a0ba6011f8884))
* **config:** add layered TOML configuration ([ea54e61](https://github.com/chunkifydev/chunkify-analytics/commit/ea54e61a1259697c87b0b6ca575b1cbe4067d6a5))
* **config:** add layered TOML configuration ([#11](https://github.com/chunkifydev/chunkify-analytics/issues/11)) ([07b3229](https://github.com/chunkifydev/chunkify-analytics/commit/07b32293501c9d65762428eb7172c28bff6376cf))
* **config:** load collector and MCP settings from dotenv ([ba6e4eb](https://github.com/chunkifydev/chunkify-analytics/commit/ba6e4eb82a1dc4d46e281686286854450afbe19c))
* **config:** load collector and MCP settings from dotenv ([#10](https://github.com/chunkifydev/chunkify-analytics/issues/10)) ([368ecbb](https://github.com/chunkifydev/chunkify-analytics/commit/368ecbb7e811d2362f1ad9e1f9732591d94a97b8))
* **config:** standardize Chunkify deployment variables ([#26](https://github.com/chunkifydev/chunkify-analytics/issues/26)) ([0bac54c](https://github.com/chunkifydev/chunkify-analytics/commit/0bac54c7b4fc8d92834a93c4658c998d9b38363b))
* demonstrate cumulative rendition metrics in dashboard ([46d0030](https://github.com/chunkifydev/chunkify-analytics/commit/46d0030640cdc8830685148311eb488f9cada0f6))
* expose hourly and daily analytics through HTTP and MCP ([b4ce252](https://github.com/chunkifydev/chunkify-analytics/commit/b4ce2527a86b29415ea6aaa0f7a885adb51c01d0))
* **keys:** add read-only private keys ([#21](https://github.com/chunkifydev/chunkify-analytics/issues/21)) ([f0e50b6](https://github.com/chunkifydev/chunkify-analytics/commit/f0e50b6d541558c074f1d9330b3377c093116ae1))
* **mcp:** serve MCP over HTTP in the collector process ([47dcf02](https://github.com/chunkifydev/chunkify-analytics/commit/47dcf020683b36699f4fc49e59c8d2f4cbc448e8))
* **mcp:** serve MCP over HTTP in the collector process ([#12](https://github.com/chunkifydev/chunkify-analytics/issues/12)) ([28a269b](https://github.com/chunkifydev/chunkify-analytics/commit/28a269b93ab627394aac7654a8c2395cd493e8cf))
* package analytics collector for Docker and measure capacity ([14a2433](https://github.com/chunkifydev/chunkify-analytics/commit/14a24335a2f3f31f55520e582711c344a1ef7d95))
* **projects:** add live creation with an admin key ([4411afa](https://github.com/chunkifydev/chunkify-analytics/commit/4411afa65a5b247f8ca866e8eb52a04258ab59ba))
* **projects:** add project creation with an admin key ([6c0e5a4](https://github.com/chunkifydev/chunkify-analytics/commit/6c0e5a4e79468c0581fe1205da7b629a2ec04bb3))
* **projects:** add scoped project updates and deletion ([70debb1](https://github.com/chunkifydev/chunkify-analytics/commit/70debb19156653813d9ba711b951aa1da38cd9a6))
* **projects:** add scoped project updates and deletion ([fb44889](https://github.com/chunkifydev/chunkify-analytics/commit/fb44889ddc798ac9151e2a81c33b864b1c995289))
* **projects:** generate project IDs automatically ([d26d94a](https://github.com/chunkifydev/chunkify-analytics/commit/d26d94af33283d167c75790902b664eced6ab17b))
* **projects:** manage origins and keys through the API ([9d24fbe](https://github.com/chunkifydev/chunkify-analytics/commit/9d24fbe1fda0ebc2bf727cfd398da1e27891fd6f))
* **storage:** add local and S3 storage backends ([69494bd](https://github.com/chunkifydev/chunkify-analytics/commit/69494bda6c3e7abb068abbce05723f5f0e6ac8ae))
* **storage:** add S3 storage and project management ([dba339c](https://github.com/chunkifydev/chunkify-analytics/commit/dba339cf0f199b35ae0f6fd9d499fb3741bbd7fb))
* support live retention policies per project ([f834538](https://github.com/chunkifydev/chunkify-analytics/commit/f8345380ce7e982d7f12e4012e789bcfb76578ea))


### Bug Fixes

* bound the dashboard event scan duration ([870e746](https://github.com/chunkifydev/chunkify-analytics/commit/870e746740ea6fc5d5a13ef06c3776bcb7f76baf))
* **build:** refresh the collector when reusing local caches ([b52a9c7](https://github.com/chunkifydev/chunkify-analytics/commit/b52a9c7479b791f2a76d3b673d9755a72d754630))
* bundle JSON queries and reject overflowing time buckets ([6737978](https://github.com/chunkifydev/chunkify-analytics/commit/6737978b646ba819bb94bccd746497352e623ad1))
* **collector:** bound benchmark generator heap ([cec8589](https://github.com/chunkifydev/chunkify-analytics/commit/cec8589e99edb5565f3f5796c1fae10a9e97d544))
* exclude private data from native demo bundles ([5609f67](https://github.com/chunkifydev/chunkify-analytics/commit/5609f67d46938361b4b1c011f28f68070da96e1a))
* fetch latest dashboard events in one request ([a0c3831](https://github.com/chunkifydev/chunkify-analytics/commit/a0c38315e2eadadf459402f5d5d7067315ff14bb))
* keep quality ratios finite for extreme durations ([6ceb54e](https://github.com/chunkifydev/chunkify-analytics/commit/6ceb54e09066b83dbbb7625e87b9583e853ac5b6))
* leave the demo video URL empty for user input ([4dc59e5](https://github.com/chunkifydev/chunkify-analytics/commit/4dc59e5575b3576ab63166b86082821c51e19032))
* preserve dashboard metrics when event reads fail ([4579d86](https://github.com/chunkifydev/chunkify-analytics/commit/4579d86e92e692a44c67e49075eab78170768336))
* support private live administration with long data paths ([1613862](https://github.com/chunkifydev/chunkify-analytics/commit/161386257f0d83b09ad3b23fced74122d6d1d90a))


### Performance Improvements

* avoid copying whole OTLP batches per event ([fc340c3](https://github.com/chunkifydev/chunkify-analytics/commit/fc340c3ea860456d5be33f2b9651d7a95ddb194e))
* **collector:** batch writes with bounded buffering and profiling ([a7f5326](https://github.com/chunkifydev/chunkify-analytics/commit/a7f5326d92ab708ed19f99635650b9aee9f4dd64))
* **collector:** isolate dashboard reads from ingestion ([1882950](https://github.com/chunkifydev/chunkify-analytics/commit/1882950154e77334da8f178db845aef0f5df8b69))
* pipeline durable replay with bounded memory and thread tuning ([b9e897f](https://github.com/chunkifydev/chunkify-analytics/commit/b9e897f6bff5c6ebb38a10de0a03f6c8849c76df))
* reconstruct view summaries in one event scan ([91adac1](https://github.com/chunkifydev/chunkify-analytics/commit/91adac1285042bea99aef170d89c48e9a1ce4528))


### Code Refactoring

* **api:** remove redundant ingestion routes ([#20](https://github.com/chunkifydev/chunkify-analytics/issues/20)) ([fd7afff](https://github.com/chunkifydev/chunkify-analytics/commit/fd7afff83fa0aae86a1340a8525e7b79b864f527))
* **mcp:** simplify URL configuration names ([#27](https://github.com/chunkifydev/chunkify-analytics/issues/27)) ([ba15c51](https://github.com/chunkifydev/chunkify-analytics/commit/ba15c511f8a370796713f8c74caf9826d0441d98))
* shorten executable name to chunkify-analytics ([de9c669](https://github.com/chunkifydev/chunkify-analytics/commit/de9c6693e310c3ef5d7d316e809dc6922f75583f))
* shorten executable name to chunkify-analytics ([#8](https://github.com/chunkifydev/chunkify-analytics/issues/8)) ([d7f94e2](https://github.com/chunkifydev/chunkify-analytics/commit/d7f94e25a32411e8a99c3ff55123fc54e2ec30a8))
