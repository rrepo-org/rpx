use super::support::{Fixture, RepositoryFixture, diagnostic, snapshot};
use std::fs;

#[test]
fn failed_package_installation_cleans_up_and_can_be_retried() {
    let f = Fixture::new();
    f.package();
    let sources = f.project.join("R");
    fs::create_dir(&sources).unwrap();
    let broken = sources.join("broken.R");
    fs::write(
        &broken,
        "stop('intentional fixture installation failure')\n",
    )
    .unwrap();
    f.success(&f.project, &["lock"]);
    let before = snapshot(&f.project);
    f.failure(
        &f.project,
        &["sync"],
        "intentional fixture installation failure",
    );
    assert_eq!(before, snapshot(&f.project));
    f.assert_no_staging();
    fs::write(&broken, "fixture_value <- 42L\n").unwrap();
    let before_retry = snapshot(&f.project);
    f.success(&f.project, &["sync"]);
    f.assert_package("fixturepkg", true);
    f.r_assert(
        &f.project,
        "stopifnot(get('fixture_value',asNamespace('fixturepkg')) == 42L)",
    );
    assert_eq!(before_retry, snapshot(&f.project));
    f.assert_no_staging();
    f.close();
}

#[test]
fn compiled_source_installation_does_not_modify_project_sources() {
    let f = Fixture::new();
    f.package();
    let source = f.project.join("src");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("native.c"), "void fixture_native(void) {}\n").unwrap();
    f.success(&f.project, &["lock"]);
    let before = snapshot(&f.project);
    f.success(&f.project, &["sync"]);
    assert_eq!(before, snapshot(&f.project));
    f.assert_no_staging();
    f.assert_package("fixturepkg", true);
    f.close();
}

#[test]
fn dependency_cycle_is_rejected_without_installing_packages() {
    let f = Fixture::new();
    f.package();
    f.set_field("Imports", "R6, digest");
    f.success(&f.project, &["lock"]);
    let mut lock = f.lock();
    lock["packages"]["R6"]["dependencies"] = serde_json::json!(["digest"]);
    lock["packages"]["digest"]["dependencies"] = serde_json::json!(["R6"]);
    f.write_lock(&lock);
    let before = f.lock_bytes();
    f.failure(
        &f.project,
        &["sync", "--no-install-project"],
        "rpx::sync::dependency_cycle",
    );
    assert_eq!(before, f.lock_bytes());
    assert_eq!(fs::read_dir(f.library()).unwrap().count(), 0);
    f.assert_no_staging();
    f.close();
}

#[test]
fn clean_is_scoped_to_one_fixture_and_is_repeatable_outside_a_project() {
    let a = Fixture::new();
    let b = Fixture::new();
    for f in [&a, &b] {
        f.package();
        f.success(&f.project, &["lock"]);
        f.success(&f.project, &["sync"]);
        fs::create_dir_all(f.data.join("libraries/orphan/library")).unwrap();
    }
    let b_data = snapshot(&b.data);
    let b_cache = snapshot(&b.cache);
    let a_project = snapshot(&a.project);
    a.success(&a.root, &["clean"]);
    assert!(!a.data.join("libraries").exists());
    assert!(!a.cache.exists());
    assert_eq!(snapshot(&a.project), a_project);
    assert_eq!(snapshot(&b.data), b_data);
    assert_eq!(snapshot(&b.cache), b_cache);
    b.assert_package("fixturepkg", true);
    a.success(&a.root, &["clean"]);
    a.close();
    b.close();
}

#[test]
fn independent_processes_can_use_separate_roots_concurrently() {
    use std::process::Stdio;
    let a = Fixture::new();
    let b = Fixture::new();
    for f in [&a, &b] {
        f.package();
        f.success(&f.project, &["lock"]);
    }
    let spawn = |f: &Fixture| {
        f.rpx_command(&f.project)
            .arg("sync")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let first = spawn(&a);
    let second = spawn(&b);
    for child in [first, second] {
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", diagnostic(&output));
    }
    for f in [&a, &b] {
        f.assert_package("fixturepkg", true);
        f.assert_no_staging();
    }
    a.close();
    b.close();
}

#[test]
fn failed_dependency_install_waits_for_active_installs_and_retry_succeeds() {
    let f = Fixture::new();
    f.package();
    let state = f.root.join("install-state");
    fs::create_dir(&state).unwrap();
    let mut repository = RepositoryFixture::new(
        "Package: slowpkg\nVersion: 1.0.0\n\nPackage: failpkg\nVersion: 1.0.0\n\n",
    );
    // Real packages coordinate through fixture files during their load checks.
    // This exercises the installer without replacing R or patching application code.
    for (name, body) in [
        (
            "slowpkg",
            r#"
            writeLines('active', file.path(state, 'active'))
            wait_for('failed')
            writeLines('completed', file.path(state, 'completed'))
        "#,
        ),
        (
            "failpkg",
            r#"
            wait_for('active')
            writeLines('failed', file.path(state, 'failed'))
            stop('intentional concurrent dependency failure')
        "#,
        ),
    ] {
        let source = f.root.join(name);
        fs::create_dir_all(source.join("R")).unwrap();
        fs::write(source.join("DESCRIPTION"), format!("Package: {name}\nVersion: 1.0.0\nTitle: Install Fixture\nDescription: A coordinated installation fixture.\nLicense: GPL-3\nAuthor: Test Author\nMaintainer: Test Author <test@example.com>\n")).unwrap();
        fs::write(source.join("NAMESPACE"), "").unwrap();
        fs::write(
            source.join("R/load.R"),
            format!(
                r#"
            .onLoad <- function(libname, pkgname) {{
                if (Sys.getenv('RPX_E2E_FAIL') != '1') return(invisible(NULL))
                state <- Sys.getenv('RPX_E2E_STATE')
                wait_for <- function(name) {{
                    deadline <- Sys.time() + 30
                    while (!file.exists(file.path(state, name))) {{
                        if (Sys.time() > deadline) stop('fixture coordination timed out')
                        Sys.sleep(0.05)
                    }}
                }}
                {body}
            }}
        "#
            ),
        )
        .unwrap();
        repository.serve_package(name, &source);
    }
    f.set_field("Config/rpx/base-repository", &repository.url());
    f.set_field("Imports", "slowpkg, failpkg");
    f.success(&f.project, &["lock"]);
    let before = snapshot(&f.project);
    let output = f
        .rpx_command(&f.project)
        .args(["sync", "--no-install-project"])
        .env("RPX_E2E_FAIL", "1")
        .env("RPX_E2E_STATE", &state)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("intentional concurrent dependency failure"),
        "{}",
        diagnostic(&output)
    );
    assert!(
        state.join("completed").is_file(),
        "active install did not finish"
    );
    f.assert_no_staging();
    f.success(&f.project, &["sync", "--no-install-project"]);
    f.assert_package("slowpkg", true);
    f.assert_package("failpkg", true);
    assert_eq!(snapshot(&f.project), before);
    f.assert_no_staging();
    f.close();
}
