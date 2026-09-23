# cran-sdk

Native async operations for CRAN-like repositories, independent of rpx.

```rust,no_run
use cran_sdk::{ClientWithMiddleware, Repository};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
// Configure TLS, proxies, timeouts, default headers, and middleware once in
// the application. Reusing this client reuses its connection pool.
let http: ClientWithMiddleware = reqwest::Client::builder().build()?.into();
let cran = Repository::new("https://cloud.r-project.org".parse()?)?;

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
- `archive_listing`: a deduplicated `Vec<Version>` for the requested package.
- `latest_description`: the web/package DESCRIPTION endpoint, independently of
  current index or version-pinned source lookup.
- `current_source`, `archive_source`: streaming artifact responses.
- `current_description`, `archive_description`: DESCRIPTION from the requested
  source archive, without writing files.
- `binary`, `binary_packages`: artifact responses and validated indexes for a
  `&target_lexicon::Triple` and `&r_metadata::Version` runtime version.
- `description_from_source`: independently usable streamed DESCRIPTION parsing.

Metadata methods check HTTP status before parsing. Raw artifact/archive-root
methods leave status handling to the caller. Clients match typed error variants
and inspect the underlying reqwest error when they need HTTP status information.
`PackagesError`, `ListingError`, and `DescriptionError` expose operation-specific
variants around shared `FetchError` causes. Index validation errors retain original
text and positioned findings, without embedding a URL in the parser error.
Construction rejects non-HTTP/non-hierarchical URLs and normalizes the trailing
slash once. Prefixes, query parameters, and encoded path segments are preserved.

## Binary routing

```rust,no_run
use cran_sdk::{Repository, Version};
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let cran = Repository::new("https://cloud.r-project.org".parse()?)?;
let client = reqwest::Client::new().into();
let triple = "aarch64-apple-darwin".parse().expect("valid target triple");
let r: Version = "4.6.1".parse()?;
let version: Version = "0.6-39".parse()?;
let index = cran.binary_packages(&client, &triple, &r).await?;
let artifact = cran.binary(&client, "digest", &version, &triple, &r).await?;
# let _ = (index, artifact);
# Ok(())
# }
```

Package versions accept `impl AsRef<str>` (strings or metadata `Version`) and keep
their spelling. R versions are numeric metadata `Version` values; routing derives
the major/minor series. Targets describe the R installation, not the SDK host.

Official CRAN routing supports x86_64 Windows from R 3.0; macOS ARM64 from R 4.1
(Big Sur through 4.5, Sonoma from 4.6); and Intel macOS from R 3.4 (El Capitan
through 3.6, the unqualified macOS directory for 4.0–4.2, Big Sur from 4.3).
Historical series may require the CRAN archive mirror as the repository base.
See [CRAN macOS](https://cran.r-project.org/bin/macosx/).

Linux binary routing is deferred. Unsupported triples return a routing error
before making a request.

## Archive listings

The transport-independent `directory-listing` crate parses recognized HTML
autoindex layouts using `scraper`/`html5ever`, or nginx JSON when the response
content type is `application/json`. Links resolve against the final response URL
after redirects. Only file entries with the requested package's exact
`<package>_<version>.tar.gz` pattern become versions. Native version parse errors
remain structured. Unrecognized pages and explicitly truncated listings are
errors, rather than silently becoming empty/complete version sets. See the
[directory-listing README](../directory-listing/README.md) for supported formats
and the mirror survey.

Source DESCRIPTION extraction uses the internal `archive-stream` crate. It reads
only through the first matching regular file, buffers that file, and never writes
archive contents to disk. It does not validate the unconsumed archive tail.

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
