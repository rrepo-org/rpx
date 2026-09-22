//! Exercise artifact results through the real download -> install task edge.
#[cfg(any(target_os = "macos", windows))]
use super::support::diagnostic;
use super::support::{Fixture, RepositoryFixture};
use std::{fs, path::Path};

const DESCRIPTION: &str = "Package: artifactpkg\nVersion: 1.0.0\nTitle: Artifact Fixture\nDescription: A fixture for artifact result handoff.\nLicense: GPL-3\nAuthor: Test Author\nMaintainer: Test Author <test@example.com>\n";
const SOURCE_ENDPOINT: &str = "/packages/artifactpkg/versions/1.0.0/source";

fn write_source(path: &Path, value: &str) {
    fs::create_dir_all(path.join("R")).unwrap();
    fs::write(path.join("DESCRIPTION"), DESCRIPTION).unwrap();
    fs::write(path.join("NAMESPACE"), "export(artifact_value)\n").unwrap();
    fs::write(
        path.join("R/value.R"),
        format!("artifact_value <- function() '{value}'\n"),
    )
    .unwrap();
}

fn source_archive(path: &Path) -> Vec<u8> {
    let compressed = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut archive = tar::Builder::new(compressed);
    archive.append_dir_all("artifactpkg", path).unwrap();
    archive.into_inner().unwrap().finish().unwrap()
}

fn configure(f: &Fixture, server: &mut mockito::ServerGuard) -> Vec<mockito::Mock> {
    let mocks = vec![
        server.mock("GET", "/packages").with_status(200).with_body(
            r#"{"repositorySlug":"fixture","packages":[{"name":"artifactpkg","latestVersion":"1.0.0"}]}"#,
        ).create(),
        server.mock("GET", "/packages/artifactpkg/versions/1.0.0/description")
            .with_status(200).with_body(DESCRIPTION).create(),
    ];
    f.package();
    f.set_field("Config/rpx/base-repository", &server.url());
    f.set_field("Imports", "artifactpkg");
    f.success(&f.project, &["lock"]);
    mocks
}

fn binary_matcher() -> mockito::Matcher {
    mockito::Matcher::Regex("^/packages/artifactpkg/versions/1[.]0[.]0/binaries/".into())
}

#[test]
fn source_fallback_and_cached_source_feed_installation() {
    let f = Fixture::new();
    let mut server = mockito::Server::new();
    let _metadata = configure(&f, &mut server);
    let source = f.root.join("source");
    write_source(&source, "source");
    let binary = server
        .mock("GET", binary_matcher())
        .with_status(404)
        .expect(if cfg!(any(target_os = "macos", windows)) {
            2
        } else {
            0
        })
        .create();
    let archive = server
        .mock("GET", SOURCE_ENDPOINT)
        .with_status(200)
        .with_body(source_archive(&source))
        .expect(1)
        .create();
    let lock = f.lock_bytes();
    for attempt in 0..2 {
        if attempt != 0 {
            fs::remove_dir_all(f.library().join("artifactpkg")).unwrap();
        }
        f.success(&f.project, &["sync", "--no-install-project"]);
        f.r_assert(
            &f.project,
            "stopifnot(artifactpkg::artifact_value() == 'source')",
        );
        f.assert_no_staging();
    }
    assert_eq!(lock, f.lock_bytes());
    binary.assert();
    archive.assert();
    f.close();
}

#[test]
#[cfg(any(target_os = "macos", windows))]
fn binary_and_cached_binary_feed_installation_without_source_requests() {
    let f = Fixture::new();
    let mut server = mockito::Server::new();
    let _metadata = configure(&f, &mut server);
    let source = f.root.join("source");
    write_source(&source, "binary");
    let library = f.root.join("binary-library");
    fs::create_dir(&library).unwrap();
    let output = f
        .command("R", &f.root)
        .args([
            "CMD",
            "INSTALL",
            "--build",
            "--no-docs",
            "--no-help",
            "--no-demo",
            "-l",
        ])
        .arg(&library)
        .arg(&source)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", diagnostic(&output));
    let suffix = if cfg!(windows) { "zip" } else { "tgz" };
    let bytes = fs::read(f.root.join(format!("artifactpkg_1.0.0.{suffix}"))).unwrap();
    let binary = server
        .mock("GET", binary_matcher())
        .with_status(200)
        .with_body(bytes)
        .expect(1)
        .create();
    let archive = server
        .mock("GET", SOURCE_ENDPOINT)
        .with_status(500)
        .expect(0)
        .create();
    for attempt in 0..2 {
        if attempt != 0 {
            fs::remove_dir_all(f.library().join("artifactpkg")).unwrap();
        }
        f.success(&f.project, &["sync", "--no-install-project"]);
        f.r_assert(
            &f.project,
            "stopifnot(artifactpkg::artifact_value() == 'binary')",
        );
        f.assert_no_staging();
    }
    binary.assert();
    archive.assert();
    f.close();
}

#[test]
fn failed_artifact_acquisition_never_starts_installation() {
    let f = Fixture::new();
    let mut server = mockito::Server::new();
    let _metadata = configure(&f, &mut server);
    let _binary = server
        .mock("GET", binary_matcher())
        .with_status(404)
        .create();
    let archive = server
        .mock("GET", SOURCE_ENDPOINT)
        .with_status(500)
        .expect(1)
        .create();
    f.failure(
        &f.project,
        &["sync", "--no-install-project"],
        "rpx::sync::package_artifact_download_failed",
    );
    assert_eq!(fs::read_dir(f.library()).unwrap().count(), 0);
    f.assert_no_staging();
    archive.assert();
    f.close();
}

#[test]
fn installation_waits_for_dependency_installation_not_just_download() {
    let f = Fixture::new();
    let mut repository = RepositoryFixture::new(
        "Package: aconsumer\nVersion: 1.0.0\nImports: zprovider\n\nPackage: zprovider\nVersion: 1.0.0\n\n",
    );
    for name in ["aconsumer", "zprovider"] {
        let source = f.root.join(name);
        write_source(&source, name);
        let description = DESCRIPTION.replace("artifactpkg", name);
        fs::write(
            source.join("DESCRIPTION"),
            if name == "aconsumer" {
                format!("{description}Imports: zprovider\n")
            } else {
                description
            },
        )
        .unwrap();
        if name == "aconsumer" {
            fs::write(source.join("R/value.R"),
                "artifact_value <- function() zprovider::artifact_value()\n.onLoad <- function(...) stopifnot(zprovider::artifact_value() == 'zprovider')\n",
            ).unwrap();
        }
        repository.serve_package(name, &source);
    }
    f.package();
    f.set_field("Config/rpx/base-repository", &repository.url());
    f.set_field("Imports", "aconsumer");
    f.success(&f.project, &["lock"]);
    f.success(&f.project, &["sync", "--no-install-project"]);
    f.r_assert(
        &f.project,
        "stopifnot(aconsumer::artifact_value() == 'zprovider')",
    );
    f.assert_no_staging();
    f.close();
}
