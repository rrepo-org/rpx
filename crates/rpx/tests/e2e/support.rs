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
        self.command(&self.binary, cwd)
            .args(args)
            .output()
            .expect("rpx should start")
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
        self.success(project, &["run", "Rscript", "--vanilla", "-e", script]);
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
