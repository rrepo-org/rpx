use super::support::Fixture;
use std::fs;

#[test]
fn lock_resolves_without_installing_then_sync_preserves_the_lockfile() {
    let f = Fixture::new();
    f.package();
    f.set_field("Imports", "R6");
    let nested = f.project.join("nested");
    fs::create_dir(&nested).unwrap();
    f.success(&nested, &["lock"]);
    assert!(f.lock()["packages"].get("R6").is_some());
    assert!(!f.data.join("libraries").exists());
    f.failure(&nested, &["status"], "rpx::status::out_of_sync");
    let before = f.lock_bytes();
    f.success(&nested, &["sync"]);
    f.assert_package("R6", true);
    f.assert_package("fixturepkg", true);
    assert_eq!(before, f.lock_bytes());
    f.close();
}

#[test]
fn sync_restores_missing_packages_and_prunes_extras_without_relocking() {
    let f = Fixture::new();
    f.package();
    f.success(&f.project, &["add", "R6"]);
    let before = f.lock_bytes();
    f.install_extra();
    fs::remove_dir_all(f.library().join("R6")).unwrap();
    f.failure(&f.project, &["status"], "rpx::status::out_of_sync");
    f.assert_run_blocked("rpx::run::library_out_of_sync");
    f.success(&f.project, &["sync"]);
    f.assert_package("R6", true);
    f.assert_package("extrapkg", false);
    f.success(&f.project, &["status"]);
    assert_eq!(before, f.lock_bytes());
    f.assert_no_staging();
    f.close();
}

#[test]
fn extra_packages_fail_status_but_do_not_block_run_or_enter_the_lockfile() {
    let f = Fixture::new();
    f.locked_project();
    f.success(&f.project, &["sync"]);
    f.install_extra();
    f.failure(&f.project, &["status"], "rpx::status::out_of_sync");
    f.assert_package("extrapkg", true);
    let lock = f.lock_bytes();
    f.success(&f.project, &["lock"]);
    assert_eq!(lock, f.lock_bytes());
    f.success(&f.project, &["remove", "extrapkg"]);
    f.assert_package("extrapkg", false);
    assert_eq!(lock, f.lock_bytes());
    f.close();
}

#[test]
fn sync_restores_the_exact_locked_version() {
    let f = Fixture::new();
    f.package();
    f.success(&f.project, &["add", "R6@==2.5.1"]);
    let lock = f.lock_bytes();
    // Mutate the installed package's metadata to simulate a different installed version.
    let description = f.library().join("R6/DESCRIPTION");
    let contents = fs::read_to_string(&description).unwrap();
    fs::write(
        &description,
        contents.replace("Version: 2.5.1", "Version: 0.0.1"),
    )
    .unwrap();
    f.failure(&f.project, &["status"], "rpx::status::out_of_sync");
    f.assert_run_blocked("rpx::run::library_out_of_sync");
    f.success(&f.project, &["sync"]);
    f.r_assert(
        &f.project,
        "stopifnot(as.character(packageVersion('R6',lib.loc=.libPaths()[1L])) == '2.5.1')",
    );
    assert_eq!(lock, f.lock_bytes());
    f.close();
}

#[test]
fn sync_can_toggle_root_installation_without_relocking() {
    let f = Fixture::new();
    f.package();
    f.success(&f.project, &["add", "--no-install-project", "R6"]);
    let lock = f.lock_bytes();
    f.assert_package("fixturepkg", false);
    f.success(&f.project, &["sync"]);
    f.assert_package("fixturepkg", true);
    f.success(&f.project, &["sync", "--no-install-project"]);
    f.assert_package("fixturepkg", false);
    f.assert_package("R6", true);
    assert_eq!(lock, f.lock_bytes());
    f.close();
}

#[test]
fn requirement_drift_blocks_execution_before_the_command_starts() {
    let f = Fixture::new();
    f.locked_project();
    f.set_field("Imports", "R6");
    f.failure(
        &f.project,
        &["status"],
        "rpx::project::requirements_changed",
    );
    f.failure(&f.project, &["sync"], "rpx::project::requirements_changed");
    f.assert_run_blocked("rpx::project::requirements_changed");
    f.close();
}

#[test]
fn repository_drift_blocks_execution_and_sync() {
    let f = Fixture::new();
    f.locked_project();
    f.set_field("Additional_repositories", "https://example.test/cran");
    f.failure(
        &f.project,
        &["status"],
        "rpx::project::repositories_changed",
    );
    f.failure(&f.project, &["sync"], "rpx::project::repositories_changed");
    f.assert_run_blocked("rpx::project::repositories_changed");
    f.close();
}

#[test]
fn runtime_drift_blocks_execution_and_sync() {
    let f = Fixture::new();
    f.locked_project();
    let mut lock = f.lock();
    lock["r"] = serde_json::json!("0.0.1");
    f.write_lock(&lock);
    f.failure(&f.project, &["status"], "rpx::project::r_version_changed");
    f.failure(&f.project, &["sync"], "rpx::project::r_version_changed");
    f.assert_run_blocked("rpx::project::r_version_changed");
    f.close();
}

fn unsupported_schema(version: u32, code: &str) {
    let f = Fixture::new();
    f.package();
    f.write_lock(&serde_json::json!({"version": version}));
    let before = f.lock_bytes();
    for command in ["status", "sync"] {
        f.failure(&f.project, &[command], code);
    }
    f.assert_run_blocked(code);
    assert_eq!(before, f.lock_bytes());
    f.close();
}

#[test]
fn old_lockfile_is_rejected() {
    unsupported_schema(0, "rpx::project::lockfile_outdated");
}
#[test]
fn newer_lockfile_is_rejected() {
    unsupported_schema(999, "rpx::project::lockfile_from_newer_rpx");
}

#[test]
fn reordered_requirements_are_accepted_without_rewriting() {
    let f = Fixture::new();
    f.locked_project();
    f.success(&f.project, &["add", "grid", "utils"]);
    let mut lock = f.lock();
    lock["requirements"].as_array_mut().unwrap().reverse();
    f.write_lock(&lock);
    let before = f.lock_bytes();
    f.success(&f.project, &["sync"]);
    f.success(&f.project, &["status"]);
    assert_eq!(before, f.lock_bytes());
    f.close();
}

#[test]
fn missing_lockfile_prevents_command_execution() {
    let f = Fixture::new();
    f.package();
    f.assert_run_blocked("rpx::project::lockfile_read_failed");
    f.close();
}
