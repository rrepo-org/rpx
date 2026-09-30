use super::*;
use flate2::{Compression, write::GzEncoder};
use std::{fs, path::Path};

fn archive(path: &Path, name: &str, version: &str) {
    let file = fs::File::create(path).unwrap();
    let mut tar = tar::Builder::new(GzEncoder::new(file, Compression::default()));
    let text = format!("Package: {name}\nVersion: {version}\n");
    let mut header = tar::Header::new_gnu();
    header.set_size(text.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, format!("{name}/DESCRIPTION"), text.as_bytes())
        .unwrap();
    tar.into_inner().unwrap().finish().unwrap();
}

fn request(path: &Path) -> PublishRequest {
    PublishRequest {
        artifact: path.to_path_buf(),
        package: "example".into(),
        version: "1.0.0".parse().unwrap(),
        repository: "acme/internal".parse().unwrap(),
        api_key: "fixture-secret".into(),
    }
}

#[test]
fn only_namespace_qualified_slugs_are_accepted() {
    assert_eq!(
        "acme/internal"
            .parse::<RepositorySlug>()
            .unwrap()
            .to_string(),
        "acme/internal"
    );
    for invalid in [
        "CRAN",
        "acme",
        "acme/",
        "/internal",
        "acme/internal/other",
        "acme//internal",
        "acme/Internal",
        "acme/in_ternal",
        "acme/-internal",
        "acme/internal-",
    ] {
        assert!(invalid.parse::<RepositorySlug>().is_err(), "{invalid}");
    }
}

#[tokio::test]
async fn uploads_a_source_tarball_as_authenticated_multipart() {
    let mut server = mockito::Server::new_async().await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("example_1.0.0.tar.gz");
    archive(&path, "example", "1.0.0");
    let upload = server.mock("POST", "/acme/internal/upload")
        .match_header("authorization", "Bearer fixture-secret")
        .match_header("content-type", mockito::Matcher::Regex("^multipart/form-data; boundary=".into()))
        .match_body(mockito::Matcher::Regex("name=\"file\"; filename=\"example_1.0.0.tar.gz\"".into()))
        .with_status(202)
        .with_body(r#"{"package":"example","version":"1.0.0","sourceUrl":"https://rrepo.dev/acme/internal/packages/example/versions/1.0.0/source"}"#)
        .create_async().await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let url = request(&path)
        .repository
        .upload_url(&server.url().parse().unwrap());
    let result = publish_to(&client, url, request(&path)).await.unwrap();
    assert_eq!(result.package, "example");
    assert_eq!(result.version, "1.0.0");
    assert!(result.source_url.ends_with("/source"));
    upload.assert_async().await;
}

#[tokio::test]
async fn wrong_archive_identity_is_rejected_before_upload() {
    let mut server = mockito::Server::new_async().await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("example_1.0.0.tar.gz");
    archive(&path, "example", "2.0.0");
    let upload = server
        .mock("POST", "/acme/internal/upload")
        .expect(0)
        .create_async()
        .await;
    let client = reqwest::Client::new();
    let url = request(&path)
        .repository
        .upload_url(&server.url().parse().unwrap());
    assert!(matches!(
        publish_to(&client, url, request(&path)).await,
        Err(PublishError::IdentityMismatch { .. })
    ));
    upload.assert_async().await;
}

#[tokio::test]
async fn rejected_key_and_redirect_are_not_retried() {
    let mut server = mockito::Server::new_async().await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("example_1.0.0.tar.gz");
    archive(&path, "example", "1.0.0");
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let url = request(&path)
        .repository
        .upload_url(&server.url().parse().unwrap());
    let forbidden = server
        .mock("POST", "/acme/internal/upload")
        .with_status(403)
        .expect(1)
        .create_async()
        .await;
    assert!(matches!(
        publish_to(&client, url.clone(), request(&path)).await,
        Err(PublishError::Authorization {
            status: StatusCode::FORBIDDEN
        })
    ));
    forbidden.assert_async().await;
    forbidden.remove_async().await;
    let redirect = server
        .mock("POST", "/acme/internal/upload")
        .with_status(307)
        .with_header("location", format!("{}/other", server.url()).as_str())
        .expect(1)
        .create_async()
        .await;
    let target = server.mock("POST", "/other").expect(0).create_async().await;
    assert!(matches!(
        publish_to(&client, url, request(&path)).await,
        Err(PublishError::Status {
            status: StatusCode::TEMPORARY_REDIRECT
        })
    ));
    redirect.assert_async().await;
    target.assert_async().await;
}
