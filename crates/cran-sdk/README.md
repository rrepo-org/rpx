# cran-sdk

Native async operations for CRAN-like repositories, independent of rpx.

```rust,no_run
use cran_sdk::{ClientWithMiddleware, Repository};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
// Configure TLS, proxies, timeouts, default headers, and middleware once in
// the application. Reusing this client reuses its connection pool.
let http: ClientWithMiddleware = reqwest::Client::builder().build()?.into();
let cran = Repository::new("https://cloud.r-project.org".parse()?);

let index = cran.packages(&http).await?;
let latest = cran.latest_description(&http, "digest").await?;
let archived = cran.archive_description(&http, "digest", "0.6.39").await?;

// Artifact responses retain headers/status and an unconsumed body. The caller
// can use bytes_stream(), write to disk, or apply a source/binary fallback policy.
let response = cran.current_source(&http, "digest", "0.6.39").await?;
let response = response.error_for_status()?;
# let _ = (index, latest, archived, response);
# Ok(())
# }
```

Enable your chosen TLS backend on your application's reqwest 0.13 dependency
(for example `default-features = false, features = ["rustls"]`). SDK dependencies
do not choose a TLS backend. A plain reqwest client converts to the re-exported
`ClientWithMiddleware`; a configured middleware client can be borrowed directly.
Calls use the middleware request builder, preserving both middleware and request
initializers. The SDK never unwraps it to bypass the middleware chain.

## Surface

- `packages`: parsed and validated source PACKAGES index.
- `archive_root`: raw archive-root response, with availability interpretation left
  to the caller.
- `archive_listing`: parsed per-package archive versions.
- `latest_description`: the web/package DESCRIPTION endpoint, independently of
  current index or version-pinned source lookup.
- `current_source`, `archive_source`: streaming artifact responses.
- `current_description`, `archive_description`: DESCRIPTION from the requested
  source archive, without writing files.
- `windows_binary`, `macos_binary`: streaming responses for an explicit R series
  and repository platform, which need not match the current host.
- `parse_packages`, `ArchiveListing::from_str`, `description_from_source`: native
  parsing operations, also usable separately.

Metadata methods check HTTP status before parsing. Raw artifact/archive-root
methods leave status handling to the caller. `Error::status()` exposes status
failures from typed metadata methods. Index errors retain response URL, original
text, and positioned parser findings for application diagnostics.

The SDK has no implicit cache or authentication state. Callers own credentials,
retry/caching middleware, concurrency, and persistence. The rpx adapter supplies
its Moka layer and existing archive-support policy.

## Tracing

SDK operations emit ordinary `tracing` spans. They inherit the caller's subscriber
and parent span; they do not install a subscriber, HTTP tracing middleware, or
terminal progress UI. Metadata spans include response consumption/parsing.
For raw artifact responses, body-download progress belongs to the consumer.
Clients/headers/full URLs are not automatically recorded as span fields.

`cargo test -p cran-sdk --locked` runs local-server protocol, parser, client
injection, and tracing-parentage tests without requiring R.
