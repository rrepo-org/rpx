use super::support::{Fixture, RepositoryFixture, snapshot};
use std::fs;

#[test]
fn lists_in_priority_order_and_filters_without_a_lockfile() {
    let f = Fixture::new();
    f.package();
    f.set_field("Config/rpx/base-repository", "https://base.example/cran");
    f.set_field("Remotes", "github::owner/repository@main");
    f.set_field("Additional_repositories", "https://additional.example/cran");
    let nested = f.project.join("nested");
    fs::create_dir(&nested).unwrap();
    let output = f.success(&nested, &["repo", "list"]);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.find("base.example").unwrap() < text.find("github::").unwrap());
    assert!(text.find("github::").unwrap() < text.find("additional.example").unwrap());
    let output = f.success(&nested, &["repo", "list", "--type", "base"]);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("configured") && text.contains("base.example"));
    assert!(!text.contains("github::") && !text.contains("additional.example"));
    assert!(!f.project.join("rpx.lock").exists());
    f.assert_state_empty();
    f.close();
}

#[test]
fn setting_and_resetting_base_normalizes_and_relocks() {
    let f = Fixture::new();
    f.package();
    let repository = RepositoryFixture::new("");
    let url = repository.url();
    f.success(&f.project, &["repo", "base", "set", &format!("{url}/")]);
    let text = fs::read_to_string(f.project.join("DESCRIPTION")).unwrap();
    assert!(text.contains(&format!("Config/rpx/base-repository: {url}")));
    assert_eq!(f.lock()["repos"][0]["url"], format!("{url}/"));
    f.success(&f.project, &["repo", "base", "reset"]);
    assert!(
        !fs::read_to_string(f.project.join("DESCRIPTION"))
            .unwrap()
            .contains("Config/rpx/base-repository")
    );
    assert_eq!(
        f.lock()["repos"][0]["url"],
        "https://rrepo.dev/upstream/cran"
    );
    let before = f.lock_bytes();
    f.success(&f.project, &["lock"]);
    assert_eq!(before, f.lock_bytes());
    f.close();
}

#[test]
fn duplicate_additional_repository_is_a_noop() {
    let f = Fixture::new();
    f.package();
    f.set_field("Additional_repositories", "https://example.test/cran/");
    let before = snapshot(&f.project);
    f.success(&f.project, &["repo", "add", "https://example.test/cran"]);
    assert_eq!(snapshot(&f.project), before);
    assert!(!f.project.join("rpx.lock").exists());
    f.close();
}

#[test]
fn add_and_remove_additional_repository_preserves_metadata() {
    let f = Fixture::new();
    f.package();
    let repository = RepositoryFixture::new("");
    let url = repository.url();
    f.set_field("URL", "https://example.test/package");
    f.success(
        &f.project,
        &["repo", "additional", "add", &format!("{url}/")],
    );
    assert!(
        f.lock()["repos"]
            .as_array()
            .unwrap()
            .iter()
            .any(|repo| repo["url"] == format!("{url}/"))
    );
    f.success(&f.project, &["repo", "remove", &url]);
    let text = fs::read_to_string(f.project.join("DESCRIPTION")).unwrap();
    assert!(!text.contains("Additional_repositories"));
    f.r_assert(
        &f.project,
        "stopifnot(read.dcf('DESCRIPTION')[1,'URL'] == 'https://example.test/package')",
    );
    assert_eq!(f.lock()["repos"].as_array().unwrap().len(), 1);
    f.close();
}

#[test]
fn normalized_remote_removal_relocks() {
    let f = Fixture::new();
    f.package();
    f.set_field("Remotes", "github::owner/repository@main");
    f.success(
        &f.project,
        &["repo", "remote", "remove", "owner/repository@main"],
    );
    assert!(
        !fs::read_to_string(f.project.join("DESCRIPTION"))
            .unwrap()
            .contains("Remotes:")
    );
    assert_eq!(f.lock()["repos"].as_array().unwrap().len(), 1);
    f.close();
}

#[test]
fn unsupported_remote_leaves_project_unchanged() {
    let f = Fixture::new();
    f.locked_project();
    let before = snapshot(&f.project);
    f.failure(
        &f.project,
        &[
            "repo",
            "remote",
            "add",
            "archive=url::https://example.test/package.tar.gz",
        ],
        "unsupported remote",
    );
    assert_eq!(snapshot(&f.project), before);
    f.close();
}
