# rrepo-sdk

Native async rrepo repository operations, independent of rpx.

```rust,no_run
use rrepo_sdk::{ClientWithMiddleware, Repository};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let http: ClientWithMiddleware = reqwest::Client::builder().build()?.into();
let repository = Repository::new("https://rrepo.dev/upstream/cran".parse()?)?;

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
- `binary`: an unconsumed Windows/macOS artifact response selected by a
  `&target_lexicon::Triple` and `&r_metadata::Version` runtime version.

Metadata methods check HTTP status. Artifact methods preserve status, headers,
and streaming bodies so the application can choose its download/fallback policy.
Construction validates HTTP URLs and normalizes their trailing slash once.
Metadata methods return `FetchError` with transport or HTTP/body/JSON causes;
raw source requests return middleware errors, and binary requests additionally
expose unsupported-target failures. Callers match variants and inspect the native
reqwest error for status information; there are no status-forwarding methods.

Package versions accept `impl AsRef<str>`: strings and metadata `Version` retain
their original spelling. Binary requests derive the R major/minor series inside
the SDK. Windows x86_64 uses `binaries/windows/<series>`; macOS uses
`binaries/macos/<platform>/<series>`. ARM64 selects Big Sur for R 4.1–4.5 and Sonoma
from R 4.6; Intel selects Big Sur from R 4.3. Earlier macOS builds and Linux are
unsupported. The supplied triple describes the R installation, not the SDK host.

```rust,no_run
use rrepo_sdk::{Repository, Version};
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let repository = Repository::new("https://rrepo.dev/upstream/cran".parse()?)?;
let client = reqwest::Client::new().into();
let triple = "aarch64-apple-darwin".parse().expect("valid target triple");
let r: Version = "4.6.1".parse()?;
let version: Version = "0.6.39".parse()?;
let response = repository.binary(&client, "digest", &version, &triple, &r).await?;
# let _ = response;
# Ok(())
# }
```

SDK operation spans use `tracing` and the caller's subscriber. HTTP request
instrumentation belongs to injected middleware; rendering/progress belongs to
the application. Clients, credentials, and full URLs are not captured as span
arguments. The SDK doesn't configure either layer.

`cargo test -p rrepo-sdk --locked` tests native routes, response models, injectable
middleware/initializers, per-call clients, and unconsumed artifacts without R.
