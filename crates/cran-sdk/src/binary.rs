use super::*;
use target_lexicon::{Architecture, OperatingSystem, Triple};

#[derive(Debug, Error)]
#[error("unsupported binary target or R version")]
pub struct RoutingError;

#[derive(Debug, Error)]
pub enum BinaryError {
    #[error(transparent)]
    Routing(#[from] RoutingError),
    #[error(transparent)]
    Request(#[from] reqwest_middleware::Error),
}

#[derive(Debug, Error)]
pub enum BinaryPackagesError {
    #[error(transparent)]
    Routing(#[from] RoutingError),
    #[error(transparent)]
    Packages(#[from] PackagesError),
}

// Routing is shared by the binary artifact and its PACKAGES index.
fn directory(target: &Triple, r: &Version) -> Result<(Vec<String>, &'static str), RoutingError> {
    let series = format!("{}.{}", r.major(), r.minor());
    let version = (r.major(), r.minor());
    match (target.operating_system, target.architecture) {
        (OperatingSystem::Windows, Architecture::X86_64) if version >= (3, 0) => Ok((
            vec!["bin".into(), "windows".into(), "contrib".into(), series],
            "zip",
        )),
        (OperatingSystem::Darwin(_) | OperatingSystem::MacOSX(_), arch) => {
            let platform = match arch {
                Architecture::Aarch64(_) if version >= (4, 6) => Some("sonoma-arm64"),
                Architecture::Aarch64(_) if version >= (4, 1) => Some("big-sur-arm64"),
                Architecture::X86_64 if version >= (4, 3) => Some("big-sur-x86_64"),
                Architecture::X86_64 if version >= (4, 0) => None,
                Architecture::X86_64 if version >= (3, 4) => Some("el-capitan"),
                _ => return Err(RoutingError),
            };
            Ok((
                [
                    Some("bin"),
                    Some("macosx"),
                    platform,
                    Some("contrib"),
                    Some(&series),
                ]
                .into_iter()
                .flatten()
                .map(str::to_owned)
                .collect(),
                "tgz",
            ))
        }
        _ => Err(RoutingError),
    }
}

impl Repository {
    /// Return an unconsumed Windows/macOS binary response for the target R installation.
    #[tracing::instrument(name = "cran.binary", skip_all, fields(package))]
    pub async fn binary(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
        version: impl AsRef<str>,
        target: &Triple,
        r: &Version,
    ) -> Result<reqwest::Response, BinaryError> {
        let (mut segments, suffix) = directory(target, r)?;
        segments.push(format!("{package}_{}.{suffix}", version.as_ref()));
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .expect("validated repository URL")
            .extend(segments);
        Ok(client.get(url).send().await?)
    }

    #[tracing::instrument(name = "cran.binary_packages", skip_all)]
    pub async fn binary_packages(
        &self,
        client: &ClientWithMiddleware,
        target: &Triple,
        r: &Version,
    ) -> Result<Packages, BinaryPackagesError> {
        let (mut segments, _) = directory(target, r)?;
        segments.push("PACKAGES".into());
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .expect("validated repository URL")
            .extend(segments);
        Ok(self.packages_at(client, url).await?)
    }
}
