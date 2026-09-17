use super::support::{Fixture, RepositoryFixture};
use std::fs;

#[test]
fn add_use_and_remove_pure_r_and_compiled_packages_from_nested_directory() {
    let f = Fixture::new();
    f.package();
    let nested = f.project.join("scripts/nested");
    fs::create_dir_all(&nested).unwrap();
    f.success(&nested, &["add", "R6", "digest"]);
    f.assert_package("R6", true);
    f.assert_package("digest", true);
    f.assert_package("fixturepkg", true);
    f.r_assert(&nested, "stopifnot(nzchar(digest::digest('hello'))); Thing <- R6::R6Class('Thing'); stopifnot(inherits(Thing$new(), 'Thing'))");
    let lock = f.lock();
    assert!(lock["packages"].get("R6").is_some());
    assert!(lock["packages"].get("digest").is_some());
    assert!(lock["packages"].get("fixturepkg").is_none());
    f.success(&nested, &["remove", "R6", "digest"]);
    f.assert_package("R6", false);
    f.assert_package("digest", false);
    f.success(&f.project, &["status"]);
    assert!(f.lock()["packages"].as_object().unwrap().is_empty());
    f.close();
}

#[test]
fn explicit_constraint_replaces_generated_bounds() {
    let f = Fixture::new();
    f.package();
    f.success(&f.project, &["add", "R6"]);
    f.success(&f.project, &["add", "R6@>=2.5.0"]);
    f.r_assert(
        &f.project,
        "d <- read.dcf('DESCRIPTION'); stopifnot(d[1,'Imports'] == 'R6 (>= 2.5.0)')",
    );
    f.assert_package("R6", true);
    f.close();
}

fn selected_field(flag: &str, field: &str) {
    let f = Fixture::new();
    f.package();
    f.set_field("Depends", "R (>= 4.3), R6");
    f.set_field("Enhances", "R6");
    f.set_field("URL", "https://example.test/project");
    f.success(&f.project, &["add", flag, "R6"]);
    f.r_assert(&f.project, &format!(r#"
        d <- read.dcf('DESCRIPTION')
        fields <- intersect(c('Depends','Imports','LinkingTo','Suggests'), colnames(d))
        containing <- fields[vapply(fields, function(x) grepl('R6', d[1,x], fixed=TRUE), logical(1))]
        stopifnot(identical(containing, '{field}'))
        stopifnot(grepl('R (>= 4.3)', d[1,'Depends'], fixed=TRUE))
        stopifnot(d[1,'Enhances'] == 'R6', d[1,'URL'] == 'https://example.test/project')
    "#));
    f.success(&f.project, &["remove", "R6"]);
    f.r_assert(&f.project, "d <- read.dcf('DESCRIPTION'); stopifnot(grepl('R (>= 4.3)',d[1,'Depends'],fixed=TRUE)); stopifnot(d[1,'URL']=='https://example.test/project')");
    f.close();
}

#[test]
fn add_to_depends() {
    selected_field("--depends", "Depends");
}
#[test]
fn add_to_imports() {
    selected_field("--imports", "Imports");
}
#[test]
fn add_to_linking_to() {
    selected_field("--linking-to", "LinkingTo");
}
#[test]
fn add_to_suggests() {
    selected_field("--suggests", "Suggests");
}

#[test]
fn dependency_only_project_adds_and_removes_without_installing_root() {
    let f = Fixture::new();
    f.locked_project();
    f.success(&f.project, &["add", "R6"]);
    f.assert_package("R6", true);
    f.assert_package("fixturepkg", false);
    f.success(&f.project, &["remove", "R6"]);
    f.assert_package("R6", false);
    f.assert_package("fixturepkg", false);
    f.close();
}

#[test]
fn add_and_remove_can_skip_root_installation() {
    let f = Fixture::new();
    f.package();
    f.success(&f.project, &["add", "--no-install-project", "R6"]);
    f.assert_package("R6", true);
    f.assert_package("fixturepkg", false);
    f.success(&f.project, &["remove", "--no-install-project", "R6"]);
    f.assert_package("R6", false);
    f.assert_package("fixturepkg", false);
    f.close();
}

#[test]
fn repeated_add_repairs_missing_package_without_relocking() {
    let f = Fixture::new();
    f.package();
    f.success(&f.project, &["add", "R6"]);
    let lock = f.lock_bytes();
    fs::remove_dir_all(f.library().join("R6")).unwrap();
    f.success(&f.project, &["add", "R6"]);
    f.assert_package("R6", true);
    assert_eq!(lock, f.lock_bytes());
    f.close();
}

#[test]
fn base_packages_are_runtime_requirements_not_downloads() {
    let f = Fixture::new();
    f.locked_project();
    f.success(&f.project, &["add", "grid"]);
    assert_eq!(f.lock()["requirements"], serde_json::json!(["grid"]));
    assert!(f.lock()["packages"].as_object().unwrap().is_empty());
    assert!(!f.cache.join("artifacts").exists());
    f.r_assert(&f.project, "library(grid); stopifnot(!('grid' %in% rownames(installed.packages(lib.loc=.libPaths()[1L]))))");
    f.close();
}

#[test]
fn unavailable_package_version_reports_no_solution() {
    let f = Fixture::new();
    f.package();
    let repository = RepositoryFixture::new("Package: versionedpkg\nVersion: 1.0.0\n\n");
    f.set_field("Config/rpx/base-repository", &repository.url());
    f.set_field("Imports", "versionedpkg (>= 2.0.0)");
    f.failure(&f.project, &["lock"], "rpx::lock::no_solution");
    assert!(!f.project.join("rpx.lock").exists());
    f.close();
}

#[test]
fn dependency_only_project_cannot_depend_on_its_own_namespace() {
    let f = Fixture::new();
    f.package();
    f.set_field("Config/rpx/type", "project");
    f.set_field("Imports", "fixturepkg");
    f.failure(&f.project, &["lock"], "rpx::lock::no_solution");
    f.close();
}
