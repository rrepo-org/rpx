use super::*;
use reqwest_middleware::{ClientBuilder, Middleware, Next};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Mark(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl Middleware for Mark {
    async fn handle(
        &self,
        mut request: reqwest::Request,
        extensions: &mut http::Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<reqwest::Response> {
        self.0.fetch_add(1, Ordering::SeqCst);
        request
            .headers_mut()
            .insert("x-middleware", "yes".parse().unwrap());
        next.run(request, extensions).await
    }
}

#[tokio::test]
async fn native_endpoints_keep_prefixes_and_use_injected_middleware() {
    let mut server = mockito::Server::new_async().await;
    let count = Arc::new(AtomicUsize::new(0));
    let client = ClientBuilder::new(reqwest::Client::new())
        .with(Mark(count.clone()))
        .with_init(|request: reqwest_middleware::RequestBuilder| {
            request.header("x-initialized", "yes")
        })
        .build();
    let repo =
        Repository::new(format!("{}/upstream/cran/", server.url()).parse().unwrap()).unwrap();
    let packages = server.mock("GET", "/upstream/cran/packages").match_header("x-middleware", "yes")
        .match_header("x-initialized", "yes").with_status(200)
        .with_body(r#"{"repositorySlug":"cran","packages":[{"name":"example","latestVersion":"1.0","latestUploadedAt":"today","versionCount":2}]}"#)
        .expect(1).create_async().await;
    let versions = server.mock("GET", "/upstream/cran/packages/example/versions").match_header("x-middleware", "yes")
        .with_body(r#"{"package":"example","versions":[{"version":"1.0","sourceUrl":"https://example.test/source"}]}"#).create_async().await;
    let description = server
        .mock(
            "GET",
            "/upstream/cran/packages/example/versions/1.0/description",
        )
        .with_body("Package: example\nVersion: 1.0\n")
        .create_async()
        .await;
    let source = server
        .mock("GET", "/upstream/cran/packages/example/versions/1.0/source")
        .with_status(200)
        .with_header("x-artifact", "source")
        .with_body("source bytes")
        .create_async()
        .await;
    let windows = server
        .mock(
            "GET",
            "/upstream/cran/packages/example/versions/1.0/binaries/windows/4.5",
        )
        .with_body("zip bytes")
        .create_async()
        .await;
    let macos = server
        .mock(
            "GET",
            "/upstream/cran/packages/example/versions/1.0/binaries/macos/big-sur-arm64/4.5",
        )
        .with_body("tgz bytes")
        .create_async()
        .await;
    let index = repo.packages(&client).await.unwrap();
    assert_eq!(index.repository_slug, "cran");
    assert_eq!(index.packages[0].version_count, Some(2));
    assert_eq!(
        repo.versions(&client, "example").await.unwrap().versions[0].source_url,
        "https://example.test/source"
    );
    assert_eq!(
        repo.description(&client, "example", "1.0")
            .await
            .unwrap()
            .package()
            .unwrap()
            .as_str(),
        "example"
    );
    let response = repo.source(&client, "example", "1.0").await.unwrap();
    assert_eq!(response.headers()["x-artifact"], "source");
    assert_eq!(response.text().await.unwrap(), "source bytes");
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
        "zip bytes"
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
        "tgz bytes"
    );
    packages.assert_async().await;
    versions.assert_async().await;
    description.assert_async().await;
    source.assert_async().await;
    windows.assert_async().await;
    macos.assert_async().await;
    assert_eq!(count.load(Ordering::SeqCst), 6);
}

#[tokio::test]
async fn clients_can_be_changed_without_sharing_hidden_sdk_cache_state() {
    let mut server = mockito::Server::new_async().await;
    let repo = Repository::new(server.url().parse().unwrap()).unwrap();
    let first = server
        .mock("GET", "/packages")
        .match_header("x-client", "first")
        .with_body(r#"{"repositorySlug":"first","packages":[]}"#)
        .create_async()
        .await;
    let second = server
        .mock("GET", "/packages")
        .match_header("x-client", "second")
        .with_body(r#"{"repositorySlug":"second","packages":[]}"#)
        .create_async()
        .await;
    let client = |value: &'static str| -> ClientWithMiddleware {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-client", value.parse().unwrap());
        reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .unwrap()
            .into()
    };
    assert_eq!(
        repo.packages(&client("first"))
            .await
            .unwrap()
            .repository_slug,
        "first"
    );
    assert_eq!(
        repo.packages(&client("second"))
            .await
            .unwrap()
            .repository_slug,
        "second"
    );
    first.assert_async().await;
    second.assert_async().await;
}

#[tokio::test]
async fn status_errors_and_raw_artifact_responses_remain_distinct() {
    let mut server = mockito::Server::new_async().await;
    let client: ClientWithMiddleware = reqwest::Client::new().into();
    let repo = Repository::new(server.url().parse().unwrap()).unwrap();
    let _metadata = server
        .mock("GET", "/packages")
        .with_status(403)
        .create_async()
        .await;
    let error = repo.packages(&client).await.unwrap_err();
    assert!(
        matches!(error, FetchError::Response(error) if error.status() == Some(reqwest::StatusCode::FORBIDDEN))
    );
    let _artifact = server
        .mock("GET", "/packages/example/versions/1.0/source")
        .with_status(404)
        .with_body("missing")
        .create_async()
        .await;
    let response = repo.source(&client, "example", "1.0").await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    assert_eq!(response.text().await.unwrap(), "missing");
    let invalid = Repository::new("mailto:packages@example.test".parse().unwrap());
    assert!(matches!(invalid, Err(InvalidBaseUrl)));
}

#[tokio::test]
async fn endpoint_arguments_are_encoded_as_path_segments() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("GET", "/prefix/packages/a%2Fb/versions/1%2F2/source")
        .with_body("encoded")
        .create_async()
        .await;
    let repo = Repository::new(format!("{}/prefix", server.url()).parse().unwrap()).unwrap();
    let client = reqwest::Client::new().into();
    assert_eq!(
        repo.source(&client, "a/b", "1/2")
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "encoded"
    );
    mock.assert_async().await;
}

#[tokio::test]
async fn binary_routing_uses_runtime_version_and_rejects_linux() {
    let mut server = mockito::Server::new_async().await;
    let repo = Repository::new(format!("{}/prefix/", server.url()).parse().unwrap()).unwrap();
    assert_eq!(repo.base_url().path(), "/prefix");
    let client = reqwest::Client::new().into();
    let version: Version = "01.0-2".parse().unwrap();
    for (triple, r, suffix) in [
        ("aarch64-apple-darwin", "4.5.2", "macos/big-sur-arm64/4.5"),
        ("aarch64-apple-darwin", "4.6.0", "macos/sonoma-arm64/4.6"),
        ("x86_64-apple-darwin", "4.6.1", "macos/big-sur-x86_64/4.6"),
        ("x86_64-pc-windows-msvc", "4.6.1", "windows/4.6"),
    ] {
        let response = server
            .mock(
                "GET",
                format!("/prefix/packages/example/versions/01.0-2/binaries/{suffix}").as_str(),
            )
            .with_status(404)
            .with_body("absent")
            .create_async()
            .await;
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
        assert_eq!(artifact.status(), reqwest::StatusCode::NOT_FOUND);
        assert_eq!(artifact.text().await.unwrap(), "absent");
        response.assert_async().await;
    }
    for triple in ["x86_64-unknown-linux-gnu", "aarch64-pc-windows-msvc"] {
        assert!(matches!(
            repo.binary(
                &client,
                "example",
                &version,
                &triple.parse().unwrap(),
                &"4.6".parse().unwrap()
            )
            .await,
            Err(BinaryError::UnsupportedTarget)
        ));
    }
}
