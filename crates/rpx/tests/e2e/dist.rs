use super::support::{Fixture, RepositoryFixture, diagnostic, snapshot};
use std::{fs, io::Read, path::Path};

const PACKAGE: &str = "fixturepkg";
const VERSION: &str = "0.1.0";

fn archive_path(f: &Fixture) -> std::path::PathBuf {
    f.project
        .join("dist")
        .join(format!("{PACKAGE}_{VERSION}.tar.gz"))
}

fn archive_file(path: &Path, file: &str) -> Option<String> {
    let gzip = flate2::read::GzDecoder::new(fs::File::open(path).unwrap());
    let mut archive = tar::Archive::new(gzip);
    archive.entries().unwrap().find_map(|entry| {
        let mut entry = entry.unwrap();
        (entry.path().unwrap().as_ref() == Path::new(file)).then(|| {
            let mut contents = String::new();
            entry.read_to_string(&mut contents).unwrap();
            contents
        })
    })
}

fn archive_paths(path: &Path) -> Vec<String> {
    let gzip = flate2::read::GzDecoder::new(fs::File::open(path).unwrap());
    tar::Archive::new(gzip)
        .entries()
        .unwrap()
        .map(|entry| entry.unwrap().path().unwrap().display().to_string())
        .collect()
}

fn package_source(f: &Fixture) {
    f.package();
    fs::write(f.project.join(".Rbuildignore"), "^dist$\n").unwrap();
    fs::create_dir(f.project.join("R")).unwrap();
    fs::write(f.project.join("R/value.R"), "value <- function() 42\n").unwrap();
}

fn assert_no_dist_staging(f: &Fixture) {
    let output = f.project.join("dist");
    if output.exists() {
        assert!(
            snapshot(&output)
                .keys()
                .all(|path| !path.components().any(|component| component
                    .as_os_str()
                    .to_string_lossy()
                    .starts_with(".rpx-dist-"))),
            "temporary distribution files remain"
        );
    }
    assert!(
        fs::read_dir(&f.root).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".rpx-dist-")),
        "temporary build workspace remains next to the project"
    );
}

fn builder_repository(f: &Fixture) -> RepositoryFixture {
    let mut repository = RepositoryFixture::new("Package: fixturebuilder\nVersion: 1.0.0\n\n");
    let source = f.root.join("fixturebuilder-source");
    fs::create_dir_all(source.join("R")).unwrap();
    fs::write(
        source.join("DESCRIPTION"),
        "Package: fixturebuilder\nVersion: 1.0.0\nTitle: Fixture Vignette Engine\nDescription: A deterministic vignette engine for distribution tests.\nLicense: MIT\nAuthor: Test Author\nMaintainer: Test Author <test@example.com>\n",
    )
    .unwrap();
    fs::write(source.join("NAMESPACE"), "").unwrap();
    fs::write(
        source.join("R/engine.R"),
        r##".onLoad <- function(libname, pkgname) {
    tools::vignetteEngine(
        "fixture", package = pkgname, pattern = "[.]Rfixture$",
        weave = function(file, ...) {
            lines <- readLines(file)
            eval(parse(text = lines[!startsWith(lines, "%")]), envir = new.env(parent = baseenv()))
            output <- sub("[.]Rfixture$", ".html", file)
            writeLines(paste(
                normalizePath(find.package("fixturebuilder"), winslash = "/"),
                as.character(packageVersion("fixturebuilder")), sep = "\n"
            ), output)
            output
        },
        tangle = function(file, ...) {
            output <- sub("[.]Rfixture$", ".R", file)
            writeLines("# fixture vignette", output)
            output
        }
    )
}
"##,
    )
    .unwrap();
    repository.serve_package("fixturebuilder", &source);
    f.set_field("Config/rpx/base-repository", &repository.url());
    f.set_field("Suggests", "fixturebuilder");
    f.set_field("VignetteBuilder", "fixturebuilder");
    fs::create_dir(f.project.join("vignettes")).unwrap();
    vignette(f, "invisible(TRUE)");
    repository
}

fn vignette(f: &Fixture, code: &str) {
    fs::write(
        f.project.join("vignettes/using.Rfixture"),
        format!("%\\VignetteIndexEntry{{Fixture vignette}}\n%\\VignetteEngine{{fixturebuilder::fixture}}\n{code}\n"),
    )
    .unwrap();
}

#[test]
fn minimal_package_builds_a_source_archive_after_explicit_sync() {
    let f = Fixture::new();
    package_source(&f);
    f.success(&f.project, &["lock"]);
    f.success(&f.project, &["sync"]);
    let lock = f.lock_bytes();
    let library_before = snapshot(&f.data);
    let output = f.success(&f.project, &["dist", "build"]);
    let path = archive_path(&f);
    assert!(path.is_file(), "{}", diagnostic(&output));
    let description = archive_file(&path, "fixturepkg/DESCRIPTION").unwrap();
    assert!(description.contains("Package: fixturepkg"));
    assert!(description.contains("Version: 0.1.0"));
    assert_eq!(
        archive_file(&path, "fixturepkg/R/value.R").as_deref(),
        Some("value <- function() 42\n")
    );
    assert!(f.project.join("dist").is_dir());
    assert_eq!(f.lock_bytes(), lock);
    assert_eq!(snapshot(&f.data), library_before);
    assert_no_dist_staging(&f);
    f.close();
}

#[test]
fn build_requires_an_explicit_sync_and_does_not_install_missing_packages() {
    let f = Fixture::new();
    package_source(&f);
    let _repository = builder_repository(&f);
    f.success(&f.project, &["lock"]);
    let lock = f.lock_bytes();
    let before = snapshot(&f.data);
    let marker = f.root.join("build-started");
    vignette(
        &f,
        &format!("writeLines('started', '{}')", marker.display()),
    );
    let output = f.rpx(&f.project, &["dist", "build"]);
    assert!(!output.status.success(), "{}", diagnostic(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("rpx::dist::library_out_of_sync"),
        "{}",
        diagnostic(&output)
    );
    assert!(
        !marker.exists(),
        "R CMD build started before sync validation"
    );
    assert!(!archive_path(&f).exists());
    assert_eq!(lock, f.lock_bytes());
    assert_eq!(before, snapshot(&f.data));
    assert_no_dist_staging(&f);
    f.close();
}

#[test]
fn missing_imports_require_sync_even_without_vignettes() {
    let f = Fixture::new();
    package_source(&f);
    let _repository = builder_repository(&f);
    f.set_field("Imports", "fixturebuilder");
    f.set_field("Suggests", "");
    f.set_field("VignetteBuilder", "");
    fs::remove_dir_all(f.project.join("vignettes")).unwrap();
    f.success(&f.project, &["lock"]);
    let lock = f.lock_bytes();
    let before = snapshot(&f.data);
    let output = f.rpx(&f.project, &["dist", "build"]);
    assert!(!output.status.success(), "{}", diagnostic(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("rpx::dist::library_out_of_sync"),
        "{}",
        diagnostic(&output)
    );
    assert!(!archive_path(&f).exists());
    assert_eq!(lock, f.lock_bytes());
    assert_eq!(before, snapshot(&f.data));
    assert_no_dist_staging(&f);
    f.close();
}

#[test]
fn synced_suggested_vignette_builder_uses_the_project_library() {
    let f = Fixture::new();
    package_source(&f);
    let _repository = builder_repository(&f);
    f.success(&f.project, &["lock"]);
    f.success(&f.project, &["sync", "--no-install-project"]);
    let lock = f.lock_bytes();
    let output = f.success(&f.project, &["dist", "build"]);
    let html =
        archive_file(&archive_path(&f), "fixturepkg/inst/doc/using.html").unwrap_or_else(|| {
            panic!(
                "built vignette missing from {:?}\n{}",
                archive_paths(&archive_path(&f)),
                diagnostic(&output)
            )
        });
    assert!(html.contains(&f.library().to_string_lossy().replace('\\', "/")));
    assert!(html.contains("1.0.0"));
    assert_eq!(lock, f.lock_bytes());
    assert_no_dist_staging(&f);
    f.close();
}

#[test]
fn missing_suggested_vignette_builder_fails_before_build_starts() {
    let f = Fixture::new();
    package_source(&f);
    let _repository = builder_repository(&f);
    f.success(&f.project, &["lock"]);
    f.success(&f.project, &["sync", "--no-install-project"]);
    fs::remove_dir_all(f.library().join("fixturebuilder")).unwrap();
    let marker = f.root.join("build-started");
    vignette(
        &f,
        &format!("writeLines('started', '{}')", marker.display()),
    );
    let output = f.rpx(&f.project, &["dist", "build"]);
    assert!(!output.status.success(), "{}", diagnostic(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("rpx::dist::library_out_of_sync"),
        "{}",
        diagnostic(&output)
    );
    assert!(
        !marker.exists(),
        "R CMD build started with missing Suggests"
    );
    assert!(!archive_path(&f).exists());
    assert_no_dist_staging(&f);
    f.close();
}

#[test]
fn wrong_installed_version_blocks_build_without_repairing_library() {
    let f = Fixture::new();
    package_source(&f);
    let _repository = builder_repository(&f);
    f.success(&f.project, &["lock"]);
    f.success(&f.project, &["sync", "--no-install-project"]);
    let metadata = f.library().join("fixturebuilder/DESCRIPTION");
    let original = fs::read_to_string(&metadata).unwrap();
    fs::write(
        &metadata,
        original.replace("Version: 1.0.0", "Version: 0.0.1"),
    )
    .unwrap();
    let marker = f.root.join("build-started");
    vignette(
        &f,
        &format!("writeLines('started', '{}')", marker.display()),
    );
    let output = f.rpx(&f.project, &["dist", "build"]);
    assert!(!output.status.success(), "{}", diagnostic(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("rpx::dist::library_out_of_sync"),
        "{}",
        diagnostic(&output)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("1.0.0"),
        "{}",
        diagnostic(&output)
    );
    assert!(
        !marker.exists(),
        "R CMD build started with wrong dependency version"
    );
    assert!(
        fs::read_to_string(metadata)
            .unwrap()
            .contains("Version: 0.0.1")
    );
    assert!(!archive_path(&f).exists());
    assert_no_dist_staging(&f);
    f.close();
}

#[test]
fn stale_lockfile_blocks_build_without_relocking_or_syncing() {
    let f = Fixture::new();
    package_source(&f);
    f.success(&f.project, &["lock"]);
    f.success(&f.project, &["sync"]);
    f.set_field("Imports", "grid");
    let lock = f.lock_bytes();
    let library_before = snapshot(&f.data);
    f.failure(
        &f.project,
        &["dist", "build"],
        "rpx::project::requirements_changed",
    );
    assert_eq!(lock, f.lock_bytes());
    assert_eq!(library_before, snapshot(&f.data));
    assert!(!archive_path(&f).exists());
    assert_no_dist_staging(&f);
    f.close();
}

#[test]
fn root_package_need_not_be_installed_to_build() {
    let f = Fixture::new();
    package_source(&f);
    f.success(&f.project, &["lock"]);
    f.success(&f.project, &["sync", "--no-install-project"]);
    assert!(!f.library().join(PACKAGE).exists());
    f.success(&f.project, &["dist", "build"]);
    assert!(archive_path(&f).is_file());
    assert!(!f.library().join(PACKAGE).exists());
    assert_no_dist_staging(&f);
    f.close();
}

#[test]
fn existing_dist_contents_are_not_bundled_into_the_archive() {
    let f = Fixture::new();
    package_source(&f);
    f.success(&f.project, &["lock"]);
    f.success(&f.project, &["sync"]);
    fs::create_dir(f.project.join("dist")).unwrap();
    fs::write(f.project.join("dist/old-archive.tar.gz"), "old").unwrap();
    f.success(&f.project, &["dist", "build"]);
    let paths = archive_paths(&archive_path(&f));
    assert!(
        paths
            .iter()
            .all(|path| !path.starts_with("fixturepkg/dist/"))
    );
    assert_no_dist_staging(&f);
    f.close();
}

#[test]
fn in_tree_output_requires_the_dist_build_ignore_rule() {
    let f = Fixture::new();
    package_source(&f);
    f.success(&f.project, &["lock"]);
    f.success(&f.project, &["sync"]);
    fs::remove_file(f.project.join(".Rbuildignore")).unwrap();
    let output = f.rpx(&f.project, &["dist", "build"]);
    assert!(!output.status.success(), "{}", diagnostic(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("^dist$"),
        "{}",
        diagnostic(&output)
    );
    assert!(!archive_path(&f).exists());
    assert_no_dist_staging(&f);
    f.close();
}

#[test]
fn other_in_tree_output_directories_are_rejected() {
    let f = Fixture::new();
    package_source(&f);
    f.success(&f.project, &["lock"]);
    f.success(&f.project, &["sync"]);
    let output_dir = f.project.join("other-build-dir");
    let output = f
        .rpx_command(&f.project)
        .args(["dist", "build", "--output-dir"])
        .arg(&output_dir)
        .output()
        .unwrap();
    assert!(!output.status.success(), "{}", diagnostic(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("inside the package"),
        "{}",
        diagnostic(&output)
    );
    assert!(
        !output_dir
            .join(format!("{PACKAGE}_{VERSION}.tar.gz"))
            .exists()
    );
    assert!(!archive_path(&f).exists());
    assert_no_dist_staging(&f);
    f.close();
}

#[test]
fn explicit_external_output_directory_does_not_require_an_ignore_rule() {
    let f = Fixture::new();
    package_source(&f);
    f.success(&f.project, &["lock"]);
    f.success(&f.project, &["sync"]);
    fs::remove_file(f.project.join(".Rbuildignore")).unwrap();
    let output_dir = f.root.join("external-output");
    let output = f
        .rpx_command(&f.project)
        .args(["dist", "build", "--output-dir"])
        .arg(&output_dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", diagnostic(&output));
    assert!(
        output_dir
            .join(format!("{PACKAGE}_{VERSION}.tar.gz"))
            .is_file()
    );
    assert!(!archive_path(&f).exists());
    assert_no_dist_staging(&f);
    f.close();
}
