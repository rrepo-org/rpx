use super::support::Fixture;
use std::fs;

fn index(server: &mut mockito::ServerGuard, packages: &[(&str, &str)]) -> mockito::Mock {
    server
        .mock("GET", "/packages")
        .with_status(200)
        .with_body(
            serde_json::json!({
                "repositorySlug":"fixture", "packages":packages.iter().map(|(name, version)|
                    serde_json::json!({"name":name,"latestVersion":version})).collect::<Vec<_>>()
            })
            .to_string(),
        )
        .create()
}

fn description(
    server: &mut mockito::ServerGuard,
    package: &str,
    version: &str,
    dependencies: &str,
) -> mockito::Mock {
    server
        .mock(
            "GET",
            format!("/packages/{package}/versions/{version}/description").as_str(),
        )
        .with_status(200)
        .with_body(format!(
            "Package: {package}\nVersion: {version}\n{dependencies}"
        ))
        .expect(1)
        .create()
}

fn configure(f: &Fixture, url: &str) {
    f.package();
    f.set_field("Config/rpx/base-repository", url);
    f.set_field("Imports", "selected");
}

#[test]
fn lock_refreshes_actual_dependencies_after_a_hand_edited_version() {
    let f = Fixture::new();
    let mut server = mockito::Server::new();
    let _index = index(
        &mut server,
        &[
            ("selected", "2.0.0"),
            ("newdep", "1.0.0"),
            ("olddep", "1.0.0"),
        ],
    );
    let _versions = server.mock("GET", "/packages/selected/versions").with_status(200).with_body(
        r#"{"package":"selected","versions":[{"version":"2.0.0","sourceUrl":"unused"},{"version":"1.0.0","sourceUrl":"unused"}]}"#
    ).expect(1).create();
    let latest = description(&mut server, "selected", "2.0.0", "Imports: newdep\n");
    let preferred = description(&mut server, "selected", "1.0.0", "Imports: olddep\n");
    let newer_dependency = description(&mut server, "newdep", "1.0.0", "");
    let older_dependency = description(&mut server, "olddep", "1.0.0", "");
    configure(&f, &server.url());
    f.success(&f.project, &["lock"]);
    let mut locked = f.lock();
    assert_eq!(locked["packages"]["selected"]["version"], "2.0.0");
    locked["packages"]["selected"]["version"] = "1.0.0".into();
    locked["packages"]["selected"]["dependencies"] = serde_json::json!(["invented"]);
    f.write_lock(&locked);
    f.success(&f.project, &["lock"]);
    let refreshed = f.lock();
    assert_eq!(refreshed["packages"]["selected"]["version"], "1.0.0");
    assert_eq!(
        refreshed["packages"]["selected"]["dependencies"],
        serde_json::json!(["olddep"])
    );
    assert!(refreshed["packages"].get("olddep").is_some());
    assert!(refreshed["packages"].get("newdep").is_none());
    assert!(refreshed["packages"].get("invented").is_none());
    latest.assert();
    preferred.assert();
    newer_dependency.assert();
    older_dependency.assert();
    f.close();
}

fn source_archive(name: &str) -> Vec<u8> {
    use std::io::Write;
    let compressed = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut archive = tar::Builder::new(compressed);
    let description = format!(
        "Package: {name}\nVersion: 1.0.0\nTitle: Locked Replay Fixture\nDescription: Tests locked replay without metadata.\nLicense: GPL-3\nAuthor: Test Author\nMaintainer: Test Author <test@example.com>\n"
    );
    [
        ("DESCRIPTION", description.as_bytes()),
        ("NAMESPACE", b"".as_slice()),
    ]
    .into_iter()
    .for_each(|(file, body)| {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mtime(0);
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, format!("{name}/{file}"), body)
            .unwrap();
    });
    let mut compressed = archive.into_inner().unwrap();
    compressed.flush().unwrap();
    compressed.finish().unwrap()
}

#[test]
fn sync_replays_locked_dependencies_without_requesting_repository_metadata() {
    let f = Fixture::new();
    let mut server = mockito::Server::new();
    let _index = index(&mut server, &[("selected", "1.0.0")]);
    let metadata = description(&mut server, "selected", "1.0.0", "");
    configure(&f, &server.url());
    f.success(&f.project, &["lock"]);
    metadata.assert();
    let mut locked = f.lock();
    locked["packages"]["selected"]["dependencies"] = serde_json::json!(["lockeddep"]);
    locked["packages"]["lockeddep"] =
        serde_json::json!({"version":"1.0.0", "repository":server.url(), "dependencies":[]});
    f.write_lock(&locked);
    let before = f.lock_bytes();
    server.reset();
    let no_metadata = server
        .mock(
            "GET",
            mockito::Matcher::Regex("^/packages$|/description$|/versions$".into()),
        )
        .with_status(500)
        .expect(0)
        .create();
    let _binary = server
        .mock("GET", mockito::Matcher::Regex("/binaries/".into()))
        .with_status(404)
        .create();
    let archives: Vec<_> = ["selected", "lockeddep"]
        .into_iter()
        .map(|name| {
            server
                .mock(
                    "GET",
                    format!("/packages/{name}/versions/1.0.0/source").as_str(),
                )
                .with_status(200)
                .with_body(source_archive(name))
                .expect(1)
                .create()
        })
        .collect();
    f.success(&f.project, &["sync", "--no-install-project"]);
    f.assert_package("selected", true);
    f.assert_package("lockeddep", true);
    assert_eq!(f.lock_bytes(), before);
    assert!(fs::read_dir(f.library()).unwrap().count() >= 2);
    no_metadata.assert();
    archives.iter().for_each(mockito::Mock::assert);
    f.assert_no_staging();
    f.close();
}
