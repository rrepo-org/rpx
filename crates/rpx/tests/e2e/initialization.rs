use std::fs;

use super::support::{Fixture, diagnostic, snapshot};

#[test]
fn development_setup_is_ready_for_tests_and_documentation() {
    let fixture = Fixture::new();
    fixture.success(
        &fixture.project,
        &[
            "init",
            "--name",
            "example.pkg",
            "--with",
            "testthat",
            "--with",
            "roxygen2",
            "--with",
            "testthat",
        ],
    );
    assert!(
        fixture
            .project
            .join("tests/testthat/test-example.R")
            .is_file()
    );
    assert!(fixture.project.join("R").is_dir());
    fixture.success(&fixture.project, &["status"]);
    fixture.r_assert(
        &fixture.project,
        r#"
        metadata <- read.dcf("DESCRIPTION")
        stopifnot(metadata[1, "Config/testthat/edition"] == "3")
        stopifnot(metadata[1, "Roxygen"] == "list(markdown = TRUE)")
        stopifnot(metadata[1, "Encoding"] == "UTF-8")
        stopifnot(packageVersion("testthat") >= "3.0.0")
        setwd("tests")
        source("testthat.R")
    "#,
    );
    fs::write(
        fixture.project.join("R/hello.R"),
        "#' Say hello\n#' @export\nhello <- function() \"hello\"\n",
    )
    .unwrap();
    fixture.r_assert(
        &fixture.project,
        r#"
        roxygen2::roxygenise()
        stopifnot(file.exists("man/hello.Rd"))
        stopifnot(any(grepl('export(hello)', readLines("NAMESPACE"), fixed = TRUE)))
        stopifnot(any(c("RoxygenNote", "Config/roxygen2/version") %in% colnames(read.dcf("DESCRIPTION"))))
    "#,
    );
    fixture.close();
}

#[test]
fn legacy_project_rejects_testthat_before_creating_target() {
    invalid_metadata(
        &["--type", "project", "--with", "testthat"],
        "rpx::init::testthat_requires_package",
    );
}

#[test]
fn default_package_is_immediately_usable() {
    let fixture = Fixture::new();
    fixture.success(&fixture.project, &["init"]);
    for name in [
        "DESCRIPTION",
        "NAMESPACE",
        "rpx.lock",
        ".Rbuildignore",
        "LICENSE",
        "LICENSE.md",
    ] {
        assert!(fixture.project.join(name).is_file(), "missing {name}");
    }
    assert!(
        fixture
            .cache
            .join("artifacts/source/v1/sample.package")
            .is_dir()
    );
    assert!(fixture.cache.join("installer/v1/objects").is_dir());
    let lock: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.project.join("rpx.lock")).unwrap()).unwrap();
    assert!(lock["packages"].as_object().unwrap().is_empty());
    assert!(lock["requirements"].as_array().unwrap().is_empty());
    let before = snapshot(&fixture.project);
    fixture.success(&fixture.project, &["status"]);
    fixture.r_assert(
        &fixture.project,
        r#"
        normalized <- function(path) normalizePath(path, winslash = "/", mustWork = TRUE)
        within <- function(path, root) startsWith(normalized(path), paste0(normalized(root), "/"))
        metadata <- read.dcf("DESCRIPTION")
        stopifnot(metadata[1, "Package"] == "sample.package")
        stopifnot(metadata[1, "Title"] == "Sample Package")
        stopifnot(metadata[1, "License"] == "MIT + file LICENSE")
        library_path <- .libPaths()[1L]
        stopifnot(within(library_path, Sys.getenv("RPX_DATA_DIR")))
        stopifnot(within(tempdir(), Sys.getenv("TMPDIR")))
        library("sample.package", lib.loc = library_path)
        stopifnot(within(find.package("sample.package"), library_path))
    "#,
    );
    fixture.failure(
        &fixture.project,
        &["init", "--title", "Replacement"],
        "rpx::init::target_not_empty",
    );
    assert_eq!(snapshot(&fixture.project), before);
    fixture.close();
}

fn explicit_target_and_metadata(absolute: bool) {
    let fixture = Fixture::new();
    let relative = std::path::Path::new("nested/path with spaces");
    let target = fixture.project.join(relative);
    let argument = if absolute { target.as_path() } else { relative };
    fixture.success(
        &fixture.project,
        &[
            "init",
            argument.to_str().unwrap(),
            "--name",
            "example.pkg",
            "--title",
            "Example Package",
            "--description",
            "An example package.",
            "--author-name",
            "Test Author",
            "--author-email",
            "test@example.com",
            "--license",
            "gpl-3",
        ],
    );
    assert!(!fixture.project.join("DESCRIPTION").exists());
    assert!(!fixture.project.join("rpx.lock").exists());
    assert!(target.join("LICENSE.md").is_file());
    assert!(
        fs::read_to_string(target.join("LICENSE.md"))
            .unwrap()
            .contains("GNU General Public License")
    );
    fixture.success(&target, &["status"]);
    fixture.r_assert(
        &target,
        r#"
        metadata <- read.dcf("DESCRIPTION")
        expected <- c(Package = "example.pkg", Title = "Example Package",
            Description = "An example package.", Author = "Test Author [aut, cre]",
            Maintainer = "Test Author <test@example.com>", License = "GPL-3")
        stopifnot(all(metadata[1, names(expected)] == expected))
        authors <- eval(parse(text = metadata[1, "Authors@R"]))
        stopifnot(authors$given == "Test Author", authors$email == "test@example.com")
        stopifnot(all(c("aut", "cre") %in% authors$role))
        library("example.pkg", lib.loc = .libPaths()[1L])
        installed <- packageDescription("example.pkg", lib.loc = .libPaths()[1L])
        stopifnot(installed$Package == "example.pkg", installed$Title == "Example Package")
    "#,
    );
    fixture.close();
}

#[test]
fn relative_target_and_explicit_metadata() {
    explicit_target_and_metadata(false);
}

#[test]
fn absolute_target_and_explicit_metadata() {
    explicit_target_and_metadata(true);
}

#[test]
fn dependency_only_project_does_not_install_itself() {
    let fixture = Fixture::new();
    fixture.success(
        &fixture.project,
        &["init", "--type", "project", "--name", "analysis"],
    );
    let lock: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.project.join("rpx.lock")).unwrap()).unwrap();
    assert!(lock["packages"].as_object().unwrap().is_empty());
    fixture.success(&fixture.project, &["status"]);
    fixture.r_assert(
        &fixture.project,
        r#"
        metadata <- read.dcf("DESCRIPTION")
        stopifnot(metadata[1, "Config/rpx/type"] == "project")
        library_path <- normalizePath(.libPaths()[1L], winslash = "/")
        data_path <- normalizePath(Sys.getenv("RPX_DATA_DIR"), winslash = "/")
        stopifnot(startsWith(library_path, paste0(data_path, "/")))
        stopifnot(!("analysis" %in% rownames(installed.packages(lib.loc = library_path))))
    "#,
    );
    fixture.close();
}

#[test]
fn refuses_directory_with_existing_file() {
    let fixture = Fixture::new();
    fs::write(fixture.project.join("keep.txt"), "existing contents\n").unwrap();
    let before = snapshot(&fixture.project);
    fixture.failure(&fixture.project, &["init"], "rpx::init::target_not_empty");
    assert_eq!(snapshot(&fixture.project), before);
    fixture.assert_state_empty();
    fixture.close();
}

#[test]
fn refuses_directory_with_only_hidden_content() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.project.join(".git")).unwrap();
    let before = snapshot(&fixture.project);
    fixture.failure(&fixture.project, &["init"], "rpx::init::target_not_empty");
    assert_eq!(snapshot(&fixture.project), before);
    fixture.assert_state_empty();
    fixture.close();
}

#[test]
fn refuses_regular_file_as_target() {
    let fixture = Fixture::new();
    fs::write(fixture.project.join("existing"), "keep me").unwrap();
    let before = snapshot(&fixture.project);
    fixture.failure(
        &fixture.project,
        &["init", "existing"],
        "rpx::init::target_not_directory",
    );
    assert_eq!(snapshot(&fixture.project), before);
    fixture.assert_state_empty();
    fixture.close();
}

fn invalid_metadata(args: &[&str], code: &str) {
    let fixture = Fixture::new();
    let mut command = vec!["init", "new-project"];
    command.extend_from_slice(args);
    fixture.failure(&fixture.project, &command, code);
    assert_eq!(fs::read_dir(&fixture.project).unwrap().count(), 0);
    fixture.assert_state_empty();
    fixture.close();
}

#[test]
fn invalid_package_name_does_not_create_target() {
    invalid_metadata(&["--name", "123invalid"], "rpx::init::invalid_package_name");
}

#[test]
fn invalid_author_email_does_not_create_target() {
    invalid_metadata(
        &["--author-email", "not-an-email"],
        "rpx::init::invalid_author_email",
    );
}

#[test]
fn generated_package_passes_r_check() {
    let fixture = Fixture::new();
    fixture.success(&fixture.project, &["init"]);
    let check_dir = fixture.root.join("check");
    fs::create_dir(&check_dir).unwrap();
    let output = fixture
        .command("R", &check_dir)
        .args(["CMD", "check", "--no-manual"])
        .arg(&fixture.project)
        .output()
        .expect("R CMD check should start");
    let report = check_dir.join("sample-package.Rcheck/00check.log");
    let log = fs::read_to_string(&report)
        .unwrap_or_else(|error| format!("cannot read {report:?}: {error}"));
    assert!(
        output.status.success(),
        "{}\nR check report:\n{log}",
        diagnostic(&output)
    );
    assert!(report.is_file(), "R check should produce a report: {log}");
    fixture.close();
}
