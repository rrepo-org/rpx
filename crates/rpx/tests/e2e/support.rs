use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

pub struct Fixture {
    directory: tempfile::TempDir,
    pub root: PathBuf,
    pub project: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    temp: PathBuf,
    binary: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("rpx-e2e-")
            .tempdir()
            .unwrap();
        // R's Windows path handling does not reliably accept verbatim (\\?\) paths.
        #[cfg(windows)]
        let root = directory.path().to_path_buf();
        #[cfg(not(windows))]
        let root = directory.path().canonicalize().unwrap();
        let project = root.join("sample-package");
        let data = root.join("data");
        let cache = root.join("cache");
        let temp = root.join("tmp");
        for path in [&project, &data, &cache, &temp] {
            fs::create_dir(path).unwrap();
        }
        // Resolve at runtime so nextest archives remain relocatable.
        let binary = std::env::var_os("NEXTEST_BIN_EXE_rpx")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_rpx")));
        Self {
            directory,
            root,
            project,
            data,
            cache,
            temp,
            binary,
        }
    }

    pub fn command(&self, program: impl AsRef<OsStr>, cwd: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(cwd)
            .env("RPX_DATA_DIR", &self.data)
            .env("RPX_CACHE_DIR", &self.cache)
            .env("TMPDIR", &self.temp)
            .env("TMP", &self.temp)
            .env("TEMP", &self.temp);
        command
    }

    pub fn rpx(&self, cwd: &Path, args: &[&str]) -> Output {
        self.rpx_command(cwd)
            .args(args)
            .output()
            .expect("rpx should start")
    }

    pub fn rpx_command(&self, cwd: &Path) -> Command {
        self.command(&self.binary, cwd)
    }

    pub fn package(&self) {
        fs::write(self.project.join("DESCRIPTION"), "Package: fixturepkg\nVersion: 0.1.0\nTitle: Fixture Package\nDescription: A package for native end to end tests.\nLicense: GPL-3\nAuthor: Test Author\nMaintainer: Test Author <test@example.com>\n").unwrap();
        fs::write(self.project.join("NAMESPACE"), "").unwrap();
    }

    pub fn locked_project(&self) {
        self.package();
        self.set_field("Config/rpx/type", "project");
        self.success(&self.project, &["lock"]);
    }

    pub fn set_field(&self, name: &str, value: &str) {
        let path = self.project.join("DESCRIPTION");
        let text = fs::read_to_string(&path).unwrap();
        let mut skip = false;
        let mut lines = Vec::new();
        for line in text.lines() {
            if !line.starts_with(char::is_whitespace) {
                skip = line.starts_with(&format!("{name}:"));
            }
            if !skip {
                lines.push(line.to_owned());
            }
        }
        if !value.is_empty() {
            lines.push(format!("{name}: {value}"));
        }
        fs::write(path, format!("{}\n", lines.join("\n"))).unwrap();
    }

    pub fn lock_bytes(&self) -> Vec<u8> {
        fs::read(self.project.join("rpx.lock")).unwrap()
    }

    pub fn lock(&self) -> serde_json::Value {
        serde_json::from_slice(&self.lock_bytes()).unwrap()
    }

    pub fn write_lock(&self, lock: &serde_json::Value) {
        fs::write(
            self.project.join("rpx.lock"),
            serde_json::to_vec_pretty(lock).unwrap(),
        )
        .unwrap();
    }

    pub fn library(&self) -> PathBuf {
        let mut roots = fs::read_dir(self.data.join("libraries")).unwrap();
        let path = roots.next().unwrap().unwrap().path().join("library");
        assert!(roots.next().is_none(), "expected one project library");
        path
    }

    pub fn assert_package(&self, package: &str, present: bool) {
        self.r_assert(&self.project, &format!(
            "installed <- rownames(installed.packages(lib.loc = .libPaths()[1L])); stopifnot(('{package}' %in% installed) == {}); {}",
            if present { "TRUE" } else { "FALSE" },
            if present { format!("library('{package}', lib.loc = .libPaths()[1L])") } else { String::new() }
        ));
    }

    pub fn install_extra(&self) {
        let source = self.root.join("extra");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("DESCRIPTION"), "Package: extrapkg\nVersion: 1.0.0\nTitle: Extra Package\nDescription: An extra package fixture.\nLicense: GPL-3\nAuthor: Test Author\nMaintainer: Test Author <test@example.com>\n").unwrap();
        fs::write(source.join("NAMESPACE"), "").unwrap();
        let output = self
            .command("R", &self.root)
            .args([
                "CMD",
                "INSTALL",
                "--no-docs",
                "--no-help",
                "--no-demo",
                "-l",
            ])
            .arg(self.library())
            .arg(source)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", diagnostic(&output));
    }

    pub fn assert_run_blocked(&self, code: &str) {
        let marker = self.project.join("command-started");
        assert!(!marker.exists());
        self.failure(
            &self.project,
            &[
                "run",
                "Rscript",
                "--vanilla",
                "-e",
                "writeLines('started', 'command-started')",
            ],
            code,
        );
        assert!(!marker.exists(), "rejected command executed");
    }

    pub fn assert_no_staging(&self) {
        for root in [&self.project, &self.data, &self.cache] {
            for path in snapshot(root).keys() {
                for component in path.components() {
                    let name = component.as_os_str().to_string_lossy();
                    assert!(
                        !name.starts_with("00LOCK")
                            && !name.starts_with(".rpx-build-")
                            && !name.starts_with(".rpx-artifact-")
                            && !name.contains(".rpx-stage-"),
                        "leftover transaction: {path:?}"
                    );
                }
            }
        }
        for path in [
            self.cache.join("installer/v1/.build"),
            self.cache.join("installer/v1/locks"),
        ] {
            if path.exists() {
                assert_eq!(
                    fs::read_dir(&path).unwrap().count(),
                    0,
                    "leftover state: {path:?}"
                );
            }
        }
    }

    pub fn success(&self, cwd: &Path, args: &[&str]) -> Output {
        let output = self.rpx(cwd, args);
        assert!(
            output.status.success(),
            "rpx {args:?}: {}",
            diagnostic(&output)
        );
        output
    }

    pub fn failure(&self, cwd: &Path, args: &[&str], code: &str) {
        let output = self.rpx(cwd, args);
        assert!(
            !output.status.success(),
            "rpx {args:?} unexpectedly succeeded"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(code),
            "{}",
            diagnostic(&output)
        );
    }

    pub fn r_assert(&self, project: &Path, script: &str) {
        // Windows Rscript does not reliably handle multiline expressions via -e.
        let script_path = tempfile::Builder::new()
            .prefix("assert-")
            .suffix(".R")
            .tempfile_in(&self.temp)
            .unwrap()
            .into_temp_path();
        fs::write(&script_path, script).unwrap();
        let output = self
            .rpx_command(project)
            .args(["run", "Rscript", "--vanilla"])
            .arg(&script_path)
            .output()
            .expect("rpx should start");
        script_path
            .close()
            .expect("assertion script should be removed");
        assert!(
            output.status.success(),
            "R assertion failed:\n{script}\n{}",
            diagnostic(&output)
        );
    }

    pub fn assert_state_empty(&self) {
        for path in [&self.data, &self.cache] {
            assert_eq!(
                fs::read_dir(path).unwrap().count(),
                0,
                "unexpected state at {path:?}"
            );
        }
    }

    pub fn close(self) {
        self.directory
            .close()
            .expect("fixture should be fully removable");
    }
}

pub fn diagnostic(output: &Output) -> String {
    format!(
        "{}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

// Include directory entries as well as bytes, so unexpected empty directories are detected.
pub fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if entry.file_type().unwrap().is_dir() {
                result.insert(relative, None);
                visit(root, &path, result);
            } else {
                result.insert(relative, Some(fs::read(path).unwrap()));
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

pub struct RepositoryFixture {
    server: mockito::ServerGuard,
    mocks: Vec<mockito::Mock>,
}

impl RepositoryFixture {
    pub fn new(index: &str) -> Self {
        let mut server = mockito::Server::new();
        let mocks = vec![
            server.mock("GET", "/packages").with_status(404).create(),
            server
                .mock("GET", "/src/contrib/Archive/")
                .with_status(404)
                .create(),
            server
                .mock("GET", "/src/contrib/PACKAGES")
                .with_status(200)
                .with_body(index)
                .create(),
        ];
        Self { server, mocks }
    }

    pub fn url(&self) -> String {
        self.server.url()
    }

    pub fn serve_package(&mut self, name: &str, source: &Path) {
        let compressed = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(compressed);
        archive.append_dir_all(name, source).unwrap();
        let bytes = archive.into_inner().unwrap().finish().unwrap();
        self.mocks.push(
            self.server
                .mock("GET", format!("/src/contrib/{name}_1.0.0.tar.gz").as_str())
                .with_status(200)
                .with_body(bytes)
                .create(),
        );
    }
}
