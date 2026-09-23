//! Exercise artifact results through the real download -> install task edge.
#[cfg(any(target_os = "macos", windows))]
use super::support::diagnostic;
use super::support::{Fixture, RepositoryFixture};
use std::{fs, path::Path};

fn built_archives(f: &Fixture) -> Vec<std::path::PathBuf> {
    super::support::snapshot(&f.cache)
        .into_keys()
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name == "artifact.tar.gz")
        })
        .map(|path| f.cache.join(path))
        .collect()
}

#[test]
fn local_archives_are_reused_invalidated_and_repaired() {
    let f = Fixture::new();
    write_source(&f.project, "first");
    f.success(&f.project, &["lock"]);
    f.success(&f.project, &["sync"]);
    let archives = built_archives(&f);
    assert_eq!(archives.len(), 1);
    let archive = &archives[0];
    let modified = fs::metadata(archive).unwrap().modified().unwrap();
    // Make an accidental rebuild observable even on coarse timestamp filesystems.
    std::thread::sleep(std::time::Duration::from_secs(1));
    fs::remove_dir_all(f.library().join("artifactpkg")).unwrap();
    f.success(&f.project, &["sync"]);
    assert_eq!(fs::metadata(archive).unwrap().modified().unwrap(), modified);
    f.r_assert(
        &f.project,
        "stopifnot(artifactpkg::artifact_value() == 'first')",
    );

    fs::write(archive, "corrupt").unwrap();
    f.success(&f.project, &["sync"]);
    assert_ne!(fs::read(archive).unwrap(), b"corrupt");
    fs::remove_file(archive).unwrap();
    f.success(&f.project, &["sync"]);
    assert!(archive.is_file());

    write_source(&f.project, "other");
    f.success(&f.project, &["sync"]);
    assert_eq!(built_archives(&f).len(), 2);
    f.r_assert(
        &f.project,
        "stopifnot(artifactpkg::artifact_value() == 'other')",
    );
    f.assert_no_staging();
    f.close();
}

#[test]
fn concurrent_local_builds_share_one_completed_archive() {
    let f = Fixture::new();
    write_source(&f.project, "concurrent");
    f.success(&f.project, &["lock"]);
    let first = f
        .rpx_command(&f.project)
        .arg("sync")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let second = f
        .rpx_command(&f.project)
        // Share the artifact cache, but materialize into independent libraries.
        .env("RPX_DATA_DIR", f.root.join("other-data"))
        .arg("sync")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    for child in [first, second] {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            super::support::diagnostic(&output)
        );
    }
    assert_eq!(built_archives(&f).len(), 1);
    f.r_assert(
        &f.project,
        "stopifnot(artifactpkg::artifact_value() == 'concurrent')",
    );
    f.assert_no_staging();
    f.close();
}

#[cfg(unix)]
#[test]
fn local_source_changes_during_build_are_not_published() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    write_source(&f.project, "before");
    let cleanup = f.project.join("cleanup");
    // R CMD build runs cleanup in its copied source tree. Modify an original
    // input to simulate an editor changing the package while R is building it.
    fs::write(
        &cleanup,
        format!(
            "#!/bin/sh\necho changed > '{}'\n",
            f.project.join("changed-during-build").display()
        ),
    )
    .unwrap();
    fs::set_permissions(&cleanup, fs::Permissions::from_mode(0o755)).unwrap();
    f.success(&f.project, &["lock"]);
    f.failure(&f.project, &["sync"], "package source changed during build");
    assert!(built_archives(&f).is_empty());
    f.assert_no_staging();
    fs::remove_file(cleanup).unwrap();
    f.success(&f.project, &["sync"]);
    assert_eq!(built_archives(&f).len(), 1);
    f.close();
}

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
