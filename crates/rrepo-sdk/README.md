# rrepo-sdk

Native async rrepo repository operations, independent of rpx.

```rust,no_run
use rrepo_sdk::{ClientWithMiddleware, Repository};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let http: ClientWithMiddleware = reqwest::Client::builder().build()?.into();
let repository = Repository::new("https://rrepo.dev/upstream/cran".parse()?);

let index = repository.packages(&http).await?;
let versions = repository.versions(&http, "digest").await?;
let metadata = repository.description(&http, "digest", "0.6.39").await?;
let source = repository.source(&http, "digest", "0.6.39").await?;
let source = source.error_for_status()?;
# let _ = (index, versions, metadata, source);
# Ok(())
# }
```

Enable your application's chosen TLS backend on reqwest 0.13 (for example
`default-features = false, features = ["rustls"]`). The SDK doesn't select one.
Borrow an existing `ClientWithMiddleware` to retain authentication, tracing,
request initializers, and pooling. A plain reqwest client converts via `into()`.

The repository descriptor stores only its URL. It has no hidden cache, credentials,
runtime, or process-global client. Different calls can use different clients.
The application remains responsible for scoping its own caches appropriately.

## Surface

- `packages`: native package-index response, including summary fields.
- `versions`: native version response, retaining source URLs.
- `description`: parsed version-specific DESCRIPTION.
- `source`: unconsumed source artifact response.
- `windows_binary`, `macos_binary`: unconsumed artifact responses for explicit
  repository platform and R major.minor parameters.

Metadata methods check HTTP status. Artifact methods preserve status, headers,
and streaming bodies so the application can choose its download/fallback policy.
Transport, HTTP/body, and base-URL errors remain structured; `Error::status()`
exposes HTTP status failures.

SDK operation spans use `tracing` and the caller's subscriber. HTTP request
instrumentation belongs to injected middleware; rendering/progress belongs to
the application. Clients, credentials, and full URLs are not captured as span
arguments. The SDK doesn't configure either layer.

`cargo test -p rrepo-sdk --locked` tests native routes, response models, injectable
middleware/initializers, per-call clients, and unconsumed artifacts without R.
