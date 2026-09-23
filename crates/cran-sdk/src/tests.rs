use super::*;
use reqwest_middleware::{ClientBuilder, Middleware, Next};
use std::sync::{Arc, Mutex};
use tracing::{Instrument, instrument::WithSubscriber};
use tracing_subscriber::{Layer, layer::SubscriberExt, registry::LookupSpan};

fn parse(input: &str) -> Result<Packages, Box<PackagesParseError>> {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let mut server = mockito::Server::new_async().await;
        let response = server
            .mock("GET", "/src/contrib/PACKAGES")
            .with_body(input)
            .create_async()
            .await;
        let result = Repository::new(server.url().parse().unwrap())
            .unwrap()
            .packages(&reqwest::Client::new().into())
            .await;
        response.assert_async().await;
        match result {
            Ok(packages) => Ok(packages),
            Err(PackagesError::Invalid(error)) => Err(error),
            Err(error) => panic!("unexpected transport error: {error}"),
        }
    })
}

#[test]
fn parses_azurestor_trailing_depends_comma() {
    let index = parse("Package: AzureStor\nVersion: 3.7.1\nDepends: R (>= 3.3),\n").unwrap();
    assert_eq!(index.len(), 1);
    let record = index.record(0).unwrap();
    assert_eq!(record.package().unwrap().as_str(), "AzureStor");
    assert_eq!(
        record.parsed_depends().unwrap().entries()[0]
            .value
            .to_string(),
        "R (>= 3.3)"
    );
}

#[test]
fn packages_fields_are_case_sensitive_and_last_relation_field_wins() {
    let index =
        parse("package: ignored\nPackage: first\nVersion: 2.0.0\nImports: old\nImports: current\n")
            .unwrap();
    let record = index.record(0).unwrap();
    assert_eq!(record.package().unwrap().as_str(), "first");
    assert_eq!(record.parsed_version().unwrap().unwrap().as_str(), "2.0.0");
    assert_eq!(
        record.parsed_imports().unwrap().entries()[0]
            .value
            .package(),
        "current"
    );
    assert!(
        parse("package: wrong-case\nVersion: 1.0.0\n")
            .unwrap_err()
            .findings
            .iter()
            .any(|finding| finding.kind() == r_packages::FindingKind::MissingPackage)
    );
}

#[test]
fn rejects_duplicate_scalar_fields() {
    assert!(
        parse("Package: example\nVersion: 1.0.0\nVersion: 2.0.0\n")
            .unwrap_err()
            .findings
            .iter()
            .any(|finding| finding.kind() == r_packages::FindingKind::DuplicateScalarField)
    );
}

#[test]
fn surfaces_malformed_nonempty_cran_package_relations() {
    let error =
        parse("Package: example\nVersion: 1.0.0\nImports: cli (>= invalid),\n").unwrap_err();
    assert_eq!(error.findings.len(), 1);
    assert_eq!(
        error.findings[0].kind(),
        r_packages::FindingKind::InvalidRelation
    );
    assert_eq!(error.findings[0].field_name(), Some("Imports"));
}

#[test]
fn rejects_empty_relations_except_for_one_trailing_comma() {
    let error = parse("Package: first\nVersion: 1.0.0\nImports: cli,,digest\n\nPackage: second\nVersion: 2.0.0\nImports: cli,,\n").unwrap_err();
    assert_eq!(
        error
            .findings
            .iter()
            .filter(
                |finding| finding.kind() == r_packages::FindingKind::InvalidRelation
                    && finding.field_name() == Some("Imports")
            )
            .count(),
        2
    );
}

#[test]
fn aggregates_invalid_fields_across_package_records() {
    let error = parse("Version: invalid\nImports: cli,,digest\nSuggests: testthat\nSuggests: knitr\n\nPackage: valid\nVersion: 2.0.0\n").unwrap_err();
    assert_eq!(error.findings.len(), 3);
    assert!(
        error
            .findings
            .iter()
            .any(|finding| finding.kind() == r_packages::FindingKind::MissingPackage)
    );
    assert!(
        error
            .findings
            .iter()
            .any(|finding| finding.kind() == r_packages::FindingKind::InvalidVersion)
    );
    assert_eq!(
        error
            .findings
            .iter()
            .filter(|finding| finding.kind() == r_packages::FindingKind::InvalidRelation)
            .count(),
        1
    );
}

#[test]
fn surfaces_invalid_package_and_version_with_source_spans() {
    let text = "Package: _bad\nVersion: nope\n";
    let error = parse(text).unwrap_err();
    assert_eq!(error.text, text);
    assert!(error.findings.iter().any(|finding| finding.kind()
        == r_packages::FindingKind::InvalidPackageName
        && finding.span().start == 8
        && finding.span().end == 13));
    assert!(error.findings.iter().any(|finding| finding.kind()
        == r_packages::FindingKind::InvalidVersion
        && finding.span().start == 22
        && finding.span().end == 27));
}

#[test]
fn rejects_structurally_invalid_packages_before_reading_records() {
    assert!(
        !parse("Package example\nVersion: 1.0.0\n")
            .unwrap_err()
            .findings
            .is_empty()
    );
}

#[tokio::test]
async fn archive_listing_uses_only_requested_package_files() {
    let mut server = mockito::Server::new_async().await;
    let repo = Repository::new(server.url().parse().unwrap()).unwrap();
    let response = server
        .mock("GET", "/src/contrib/Archive/example/")
        .with_body(
            r#"<h1>Index of archive</h1><pre>
          <a href="example_1%2E0.tar.gz">truncated name...</a>
          <a href="example_1.0.0.tar.gz">equivalent version</a>
          <a href="example_2&#46;0.tar.gz">second</a>
          <a href="other_invalid.tar.gz">other package</a>
          <a href="example_3.0.tar.gz/">directory</a>
          <a href="../example_4.0.tar.gz">outside</a>
          <script>example_invalid.tar.gz</script>example_invalid.tar.gz
        </pre>"#,
        )
        .create_async()
        .await;
    assert_eq!(
        repo.archive_listing(&reqwest::Client::new().into(), "example")
            .await
            .unwrap(),
        vec!["1.0".parse().unwrap(), "2.0".parse().unwrap()]
    );
    response.assert_async().await;
}

#[tokio::test]
async fn archive_listings_handle_redirects_json_and_structured_failures() {
    let mut server = mockito::Server::new_async().await;
    let repo = Repository::new(server.url().parse().unwrap()).unwrap();
    let client = reqwest::Client::new().into();
    let redirect = server
        .mock("GET", "/src/contrib/Archive/example/")
        .with_status(302)
        .with_header("location", "/mirror/example/")
        .create_async()
        .await;
    let listing = server.mock("GET", "/mirror/example/")
        .with_body("<h1>Index of example</h1><pre><a href='/mirror/example/example_1.0.tar.gz'>example</a></pre>").create_async().await;
    assert_eq!(
        repo.archive_listing(&client, "example").await.unwrap(),
        vec!["1.0".parse().unwrap()]
    );
    redirect.assert_async().await;
    listing.assert_async().await;
    for (package, content_type, body) in [
        (
            "json",
            "application/json; charset=utf-8",
            r#"[{"name":"json_2.0.tar.gz","type":"file"},{"name":"other_invalid.tar.gz","type":"file"}]"#,
        ),
        (
            "invalid",
            "text/html",
            "<h1>Index of archive</h1><pre><a href='invalid_nope.tar.gz'>invalid</a></pre>",
        ),
        ("shell", "text/html", "<div id='app'></div>"),
        (
            "partial",
            "text/html",
            "<p class='warning'>Listing truncated</p><table id='list'></table>",
        ),
        (
            "empty",
            "text/html",
            "<h1>Index of empty</h1><pre><a href='../'>parent</a></pre>",
        ),
    ] {
        let response = server
            .mock("GET", format!("/src/contrib/Archive/{package}/").as_str())
            .with_header("content-type", content_type)
            .with_body(body)
            .create_async()
            .await;
        let result = repo.archive_listing(&client, package).await;
        match package {
            "json" => assert_eq!(result.unwrap(), vec!["2.0".parse().unwrap()]),
            "invalid" => assert!(matches!(
                result,
                Err(ListingError::Version {
                    source: r_metadata::VersionParseError::InvalidComponent { .. },
                    ..
                })
            )),
            "shell" => assert!(matches!(
                result,
                Err(ListingError::Directory(
                    directory_listing::Error::Unrecognized
                ))
            )),
            "partial" => assert!(matches!(result, Err(ListingError::Truncated))),
            "empty" => assert!(result.unwrap().is_empty()),
            _ => unreachable!(),
        }
        response.assert_async().await;
    }
}

fn source_body() -> Vec<u8> {
    let body = b"Package: example\nVersion: 1.0\n";
    let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::default(),
    ));
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(body.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();
    archive
        .append_data(&mut header, "example/DESCRIPTION", body.as_slice())
        .unwrap();
    archive.into_inner().unwrap().finish().unwrap()
}

struct Mark;
#[async_trait::async_trait]
impl Middleware for Mark {
    async fn handle(
        &self,
        mut request: reqwest::Request,
        extensions: &mut http::Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<reqwest::Response> {
        request
            .headers_mut()
            .insert("x-middleware", "yes".parse().unwrap());
        next.run(request, extensions)
            .instrument(tracing::debug_span!("injected_http"))
            .await
    }
}

#[tokio::test]
async fn all_endpoints_preserve_prefixes_and_native_statuses() {
    let mut server = mockito::Server::new_async().await;
    let client = ClientBuilder::new(reqwest::Client::new())
        .with(Mark)
        .with_init(|request: reqwest_middleware::RequestBuilder| {
            request.header("x-initialized", "yes")
        })
        .build();
    let repo = Repository::new(format!("{}/repo/", server.url()).parse().unwrap()).unwrap();
    let packages = server
        .mock("GET", "/repo/src/contrib/PACKAGES")
        .match_header("x-middleware", "yes")
        .match_header("x-initialized", "yes")
        .with_body("Package: example\nVersion: 1.0\n")
        .create_async()
        .await;
    let root = server
        .mock("GET", "/repo/src/contrib/Archive/")
        .with_status(403)
        .create_async()
        .await;
    let listing = server
        .mock("GET", "/repo/src/contrib/Archive/example/")
        .with_body("<h1>Index of archive</h1><pre><a href='example_1.0.tar.gz'>example</a></pre>")
        .create_async()
        .await;
    let latest = server
        .mock("GET", "/repo/web/packages/example/DESCRIPTION")
        .with_body("Package: example\nVersion: 2.0\n")
        .create_async()
        .await;
    let current = server
        .mock("GET", "/repo/src/contrib/example_1.0.tar.gz")
        .with_body(source_body())
        .expect(2)
        .create_async()
        .await;
    let archive = server
        .mock(
            "GET",
            "/repo/src/contrib/Archive/example/example_1.0.tar.gz",
        )
        .with_body(source_body())
        .expect(2)
        .create_async()
        .await;
    let windows = server
        .mock("GET", "/repo/bin/windows/contrib/4.5/example_1.0.zip")
        .with_body("zip")
        .create_async()
        .await;
    let macos = server
        .mock(
            "GET",
            "/repo/bin/macosx/big-sur-arm64/contrib/4.5/example_1.0.tgz",
        )
        .with_body("tgz")
        .create_async()
        .await;
    assert_eq!(repo.packages(&client).await.unwrap().len(), 1);
    assert_eq!(
        repo.archive_root(&client).await.unwrap().status(),
        reqwest::StatusCode::FORBIDDEN
    );
    assert_eq!(
        repo.archive_listing(&client, "example").await.unwrap(),
        vec!["1.0".parse().unwrap()]
    );
    assert_eq!(
        repo.latest_description(&client, "example")
            .await
            .unwrap()
            .version()
            .unwrap()
            .as_str(),
        "2.0"
    );
    assert_eq!(
        repo.current_source(&client, "example", "1.0")
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        source_body()
    );
    assert_eq!(
        repo.archive_source(&client, "example", "1.0")
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        source_body()
    );
    assert_eq!(
        repo.current_description(&client, "example", "1.0")
            .await
            .unwrap()
            .version()
            .unwrap()
            .as_str(),
        "1.0"
    );
    assert_eq!(
        repo.archive_description(&client, "example", "1.0")
            .await
            .unwrap()
            .version()
            .unwrap()
            .as_str(),
        "1.0"
    );
    assert_eq!(
        repo.binary(
            &client,
            "example",
            "1.0",
            &"x86_64-pc-windows-msvc".parse().unwrap(),
            &"4.5".parse().unwrap()
        )
        .await
        .unwrap()
        .text()
        .await
        .unwrap(),
        "zip"
    );
    assert_eq!(
        repo.binary(
            &client,
            "example",
            "1.0",
            &"aarch64-apple-darwin".parse().unwrap(),
            &"4.5".parse().unwrap()
        )
        .await
        .unwrap()
        .text()
        .await
        .unwrap(),
        "tgz"
    );
    packages.assert_async().await;
    root.assert_async().await;
    listing.assert_async().await;
    latest.assert_async().await;
    current.assert_async().await;
    archive.assert_async().await;
    windows.assert_async().await;
    macos.assert_async().await;
}

#[tokio::test]
async fn binary_routes_follow_r_version_and_preserve_version_spelling() {
    let mut server = mockito::Server::new_async().await;
    let client = reqwest::Client::new().into();
    let repo = Repository::new(format!("{}/prefix/", server.url()).parse().unwrap()).unwrap();
    for (triple, r, path) in [
        (
            "aarch64-apple-darwin",
            "4.5.2",
            "bin/macosx/big-sur-arm64/contrib/4.5/example_01.0-2.tgz",
        ),
        (
            "aarch64-apple-darwin",
            "4.6.0",
            "bin/macosx/sonoma-arm64/contrib/4.6/example_01.0-2.tgz",
        ),
        (
            "x86_64-apple-darwin",
            "4.6.0",
            "bin/macosx/big-sur-x86_64/contrib/4.6/example_01.0-2.tgz",
        ),
        (
            "x86_64-apple-darwin",
            "4.2.3",
            "bin/macosx/contrib/4.2/example_01.0-2.tgz",
        ),
        (
            "x86_64-apple-darwin",
            "4.3.0",
            "bin/macosx/big-sur-x86_64/contrib/4.3/example_01.0-2.tgz",
        ),
        (
            "x86_64-apple-darwin",
            "3.6.3",
            "bin/macosx/el-capitan/contrib/3.6/example_01.0-2.tgz",
        ),
        (
            "x86_64-pc-windows-msvc",
            "4.5.1",
            "bin/windows/contrib/4.5/example_01.0-2.zip",
        ),
    ] {
        let response = server
            .mock("GET", format!("/prefix/{path}").as_str())
            .with_status(410)
            .with_body("gone")
            .create_async()
            .await;
        let version: Version = "01.0-2".parse().unwrap();
        let artifact = repo
            .binary(
                &client,
                "example",
                &version,
                &triple.parse().unwrap(),
                &r.parse().unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(artifact.status(), reqwest::StatusCode::GONE);
        assert_eq!(artifact.text().await.unwrap(), "gone");
        response.assert_async().await;
    }
}

#[tokio::test]
async fn binary_index_uses_triple_and_rejects_unsupported_targets() {
    let mut server = mockito::Server::new_async().await;
    let repo = Repository::new(server.url().parse().unwrap()).unwrap();
    let client = reqwest::Client::new().into();
    let target = "aarch64-apple-darwin".parse().unwrap();
    let r = "4.5.1".parse().unwrap();
    let index = server
        .mock("GET", "/bin/macosx/big-sur-arm64/contrib/4.5/PACKAGES")
        .with_body("Package: example\nVersion: 1.0\n")
        .create_async()
        .await;
    assert_eq!(
        repo.binary_packages(&client, &target, &r)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(matches!(
        repo.binary_packages(&client, &"x86_64-unknown-linux-gnu".parse().unwrap(), &r)
            .await,
        Err(BinaryPackagesError::Routing(RoutingError))
    ));
    assert!(matches!(
        repo.binary(
            &client,
            "example",
            "1.0",
            &"x86_64-unknown-linux-gnu".parse().unwrap(),
            &r
        )
        .await,
        Err(BinaryError::Routing(RoutingError))
    ));
    assert!(matches!(
        repo.binary(
            &client,
            "example",
            "1.0",
            &"aarch64-pc-windows-msvc".parse().unwrap(),
            &r
        )
        .await,
        Err(BinaryError::Routing(RoutingError))
    ));
    index.assert_async().await;
}

#[tokio::test]
async fn normalizes_prefix_once_and_encodes_dynamic_segments() {
    let mut server = mockito::Server::new_async().await;
    for suffix in ["", "/", "/prefix", "/prefix/"] {
        let repo = Repository::new(format!("{}{suffix}", server.url()).parse().unwrap()).unwrap();
        assert_eq!(
            repo.base_url().path(),
            if suffix.starts_with("/prefix") {
                "/prefix"
            } else {
                "/"
            }
        );
        let path = format!(
            "{}/src/contrib/odd%2Fname_1.0.tar.gz",
            suffix.trim_end_matches('/')
        );
        let response = server
            .mock("GET", path.as_str())
            .with_body("archive")
            .create_async()
            .await;
        repo.current_source(&reqwest::Client::new().into(), "odd/name", "1.0")
            .await
            .unwrap();
        response.assert_async().await;
    }
}

#[tokio::test]
async fn source_description_distinguishes_missing_and_corrupt_archives() {
    let mut server = mockito::Server::new_async().await;
    let repo = Repository::new(server.url().parse().unwrap()).unwrap();
    let client = reqwest::Client::new().into();
    let missing = server
        .mock("GET", "/src/contrib/other_1.0.tar.gz")
        .with_body(source_body())
        .create_async()
        .await;
    assert!(matches!(
        repo.current_description(&client, "other", "1.0").await,
        Err(DescriptionError::DescriptionNotFound { .. })
    ));
    let corrupt = server
        .mock("GET", "/src/contrib/bad_1.0.tar.gz")
        .with_body("not gzip")
        .create_async()
        .await;
    assert!(matches!(
        repo.current_description(&client, "bad", "1.0").await,
        Err(DescriptionError::Archive(_))
    ));
    missing.assert_async().await;
    corrupt.assert_async().await;
}

type SpanRecords = Vec<(String, Option<String>)>;
#[derive(Clone)]
struct Spans(Arc<Mutex<SpanRecords>>);
impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Spans {
    fn on_new_span(
        &self,
        _: &tracing::span::Attributes<'_>,
        id: &tracing::Id,
        context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let span = context.span(id).unwrap();
        self.0.lock().unwrap().push((
            span.name().into(),
            span.parent().map(|parent| parent.name().into()),
        ));
    }
}

#[tokio::test]
async fn sdk_and_injected_http_spans_inherit_the_callers_subscriber() {
    let mut server = mockito::Server::new_async().await;
    let _response = server
        .mock("GET", "/web/packages/example/DESCRIPTION")
        .with_body("Package: example\nVersion: 1.0\n")
        .create_async()
        .await;
    let client = ClientBuilder::new(reqwest::Client::new())
        .with(Mark)
        .build();
    let repo = Repository::new(server.url().parse().unwrap()).unwrap();
    let spans = Spans(Arc::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::registry().with(spans.clone());
    async {
        repo.latest_description(&client, "example")
            .instrument(tracing::info_span!("caller"))
            .await
            .unwrap();
    }
    .with_subscriber(subscriber)
    .await;
    let spans = spans.0.lock().unwrap();
    assert!(spans.contains(&("cran.latest_description".into(), Some("caller".into()))));
    assert!(spans.contains(&(
        "injected_http".into(),
        Some("cran.latest_description".into())
    )));
}

#[tokio::test]
async fn typed_metadata_errors_keep_status_and_parser_findings() {
    let mut server = mockito::Server::new_async().await;
    let client = reqwest::Client::new().into();
    let repo = Repository::new(server.url().parse().unwrap()).unwrap();
    let _listing = server
        .mock("GET", "/src/contrib/Archive/example/")
        .with_status(403)
        .create_async()
        .await;
    assert!(matches!(repo.archive_listing(&client, "example").await,
        Err(ListingError::Fetch(FetchError::Response(error))) if error.status() == Some(reqwest::StatusCode::FORBIDDEN)));
    let _index = server
        .mock("GET", "/src/contrib/PACKAGES")
        .with_body("Package: example\nVersion: invalid\n")
        .create_async()
        .await;
    let PackagesError::Invalid(error) = repo.packages(&client).await.unwrap_err() else {
        panic!("expected index findings")
    };
    assert!(!error.findings.is_empty());
    assert!(error.text.contains("Version: invalid"));
    let invalid = Repository::new("mailto:packages@example.test".parse().unwrap());
    assert!(matches!(invalid, Err(InvalidBaseUrl)));
}
