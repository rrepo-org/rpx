use super::*;
use reqwest_middleware::{ClientBuilder, Middleware, Next};
use std::sync::{Arc, Mutex};
use tracing::{Instrument, instrument::WithSubscriber};
use tracing_subscriber::{Layer, layer::SubscriberExt, registry::LookupSpan};

fn parse(input: &str) -> Result<Packages, Box<PackagesParseError>> {
    parse_packages(
        "https://example.test/src/contrib/PACKAGES".parse().unwrap(),
        input.into(),
    )
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

#[test]
fn archive_listing_handles_links_escaped_text_and_duplicate_versions() {
    let listing: ArchiveListing = "<a href=\"example_1.0.tar.gz\">example_1.0.tar.gz</a>\nhttps://example.test/example_2.0.tar.gz\n".parse().unwrap();
    assert_eq!(
        listing.versions,
        vec!["1.0".parse().unwrap(), "2.0".parse().unwrap()]
    );
    assert!("example_invalid.tar.gz".parse::<ArchiveListing>().is_err());
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
    let repo = Repository::new(format!("{}/repo/", server.url()).parse().unwrap());
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
        .with_body("example_1.0.tar.gz")
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
        repo.archive_listing(&client, "example")
            .await
            .unwrap()
            .versions,
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
        repo.windows_binary(&client, "example", "1.0", "4.5")
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "zip"
    );
    assert_eq!(
        repo.macos_binary(&client, "example", "1.0", "big-sur-arm64", "4.5")
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
    let repo = Repository::new(server.url().parse().unwrap());
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
    let repo = Repository::new(server.url().parse().unwrap());
    let _listing = server
        .mock("GET", "/src/contrib/Archive/example/")
        .with_status(403)
        .create_async()
        .await;
    assert_eq!(
        repo.archive_listing(&client, "example")
            .await
            .unwrap_err()
            .status(),
        Some(reqwest::StatusCode::FORBIDDEN)
    );
    let _index = server
        .mock("GET", "/src/contrib/PACKAGES")
        .with_body("Package: example\nVersion: invalid\n")
        .create_async()
        .await;
    let Error::Packages(error) = repo.packages(&client).await.unwrap_err() else {
        panic!("expected index findings")
    };
    assert!(!error.findings.is_empty());
    assert!(error.text.contains("Version: invalid"));
    let invalid = Repository::new("mailto:packages@example.test".parse().unwrap());
    assert!(matches!(
        invalid.packages(&client).await,
        Err(Error::InvalidBaseUrl)
    ));
}
