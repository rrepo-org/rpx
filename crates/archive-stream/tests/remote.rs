//! Opt-in compatibility checks against version/commit-pinned public archives.
//! Run: cargo test -p archive-stream --test remote -- --ignored --test-threads=3
//! Expected payload hashes were independently computed with Python's tarfile.
use archive_stream::tar_gz_entry;
use futures_util::TryStreamExt;
use sha2::{Digest, Sha256};
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::io::StreamReader;

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent("archive-stream-compatibility-tests")
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(180))
        .build()?)
}

async fn lookup(
    client: &reqwest::Client,
    url: &str,
    path: &str,
) -> Result<(Option<Vec<u8>>, usize)> {
    let response = client.get(url).send().await?.error_for_status()?;
    let received = Arc::new(AtomicUsize::new(0));
    let counter = received.clone();
    let stream = response
        .bytes_stream()
        .inspect_ok(move |chunk| {
            counter.fetch_add(chunk.len(), Ordering::Relaxed);
        })
        .map_err(io::Error::other);
    let result = tar_gz_entry(StreamReader::new(stream), path).await?;
    Ok((result, received.load(Ordering::Relaxed)))
}

async fn check(
    url: &str,
    root: &str,
    package: &str,
    version: &str,
    hash: &str,
    early: bool,
) -> Result {
    let client = client()?;
    let (body, received) = lookup(&client, url, &format!("{root}/DESCRIPTION")).await?;
    let body = body.expect("DESCRIPTION must exist");
    assert_eq!(format!("{:x}", Sha256::digest(&body)), hash, "{url}");
    let text = std::str::from_utf8(&body)?;
    for field in [format!("Package: {package}"), format!("Version: {version}")] {
        assert!(
            text.lines().any(|line| line == field),
            "missing {field} in {url}"
        );
    }
    // Count HTTP body chunks handed to the reader, including read-ahead. This
    // deliberately generous bound is far below these 14–26 MB archives.
    if early {
        assert!(
            received < 1_000_000,
            "consumed {received} bytes before returning from {url}"
        );
    }
    // A separate streaming request must traverse all members, including those
    // after DESCRIPTION. HTTP failures fail the test rather than skip it.
    let (missing, _) = lookup(
        &client,
        url,
        &format!("{root}/__archive_stream_missing_8d125c__"),
    )
    .await?;
    assert_eq!(missing, None, "{url}");
    Ok(())
}

macro_rules! cran {
    ($name:ident, $package:literal, $version:literal, $hash:literal, $early:literal) => {
        #[tokio::test]
        #[ignore = "downloads pinned remote CRAN archives"]
        async fn $name() -> Result {
            check(
                concat!(
                    "https://cran.r-project.org/src/contrib/Archive/",
                    $package,
                    "/",
                    $package,
                    "_",
                    $version,
                    ".tar.gz"
                ),
                $package,
                $package,
                $version,
                $hash,
                $early,
            )
            .await
        }
    };
}

cran!(
    r6,
    "R6",
    "2.5.1",
    "aaf1db44cb1b1ee0d4cb688f060dfc8e2a526c22431c26f092e07571e42fe96a",
    false
);
cran!(
    bh_early_return,
    "BH",
    "1.84.0-0",
    "d3917233262608bea3e52af0e17ce35ff423f67c456b92f59e78dda78f944b93",
    true
);
cran!(
    matrix,
    "Matrix",
    "1.6-5",
    "7abff0cd2ccd88d055aa2e32844faeda74e08f68579c2d85828ae55a6cf14570",
    false
);
cran!(
    data_table,
    "data.table",
    "1.14.8",
    "d0fe993274226475732e55ed528d83e8bd2ad93bfbfa42ed31b93745537c3550",
    false
);
// DESCRIPTION follows about 2 MB of uncompressed headers and payloads.
cran!(
    ggplot2_late_description,
    "ggplot2",
    "0.9.1",
    "71f96f62888f825c0a4f5daf331daef85ac7a8a56fbdc4b6f307ed5c963a1391",
    false
);
cran!(
    plyr,
    "plyr",
    "0.1.1",
    "7b8872a441bd8c2ea200998b077ab6d7bfaba8f6bd6ff8e75c7064a411664b17",
    false
);
cran!(
    rcpp,
    "Rcpp",
    "0.8.0",
    "d1dff6835abd0fabf609f99669c5c7d052aab863ec90dcf08db0cd7e6216ff07",
    false
);
cran!(
    digest_historical,
    "digest",
    "0.2.0",
    "da4fddc67b814b76440ca993f908bd1006a2b3bee5dd8e0aca4a683d57de80ee",
    false
);
cran!(
    stan_headers,
    "StanHeaders",
    "2.21.0-7",
    "e63626c54b0f91db7db3a06c1f0a142e57a34be562d39f6eb98193a1af882830",
    false
);
cran!(
    rstan,
    "rstan",
    "2.21.2",
    "12895f44ff050866f2e903bc882744dfc9da1744be5eaaba6aa11b35a666b9f8",
    false
);
cran!(
    stringi,
    "stringi",
    "1.7.12",
    "d926a2da22e466375d4b39f08e6d9015aeca4035d9578b7331386af6a28c72cd",
    false
);
cran!(
    rcpp_eigen,
    "RcppEigen",
    "0.3.3.9.3",
    "7c2a299d7050f1b90cd02dd5a0a2a1c7ce6c04ac542413c1f5a2c1d8af074bbe",
    false
);
cran!(
    sf,
    "sf",
    "0.9-8",
    "848ac00a8cb62b1e97dc379dbc6259afec4fbd752d29ac5169e656686e291423",
    false
);

// https://github.com/KWB-R/kwbGompitz/issues/6: global PAX warning.
#[tokio::test]
#[ignore = "downloads pinned remote GitHub archive"]
async fn kwb_gompitz_global_pax() -> Result {
    check(
        "https://codeload.github.com/KWB-R/kwbGompitz/tar.gz/ac68de7461ec6c49d316943ed0a623a4c0339e44",
        "kwbGompitz-ac68de7461ec6c49d316943ed0a623a4c0339e44",
        "kwbGompitz", "0.6.4.9000",
        "2a987d5f9a3ec0b64616baff5767ad3473ad6d93a4834ef095f4fa7fe48c7650", false,
    ).await
}

// https://github.com/r-lib/remotes/issues/84: radix's historical long paths.
// The repository was renamed distill; the package at this commit is radix.
const DISTILL_URL: &str =
    "https://codeload.github.com/rstudio/distill/tar.gz/c3dba83d7ee336bcc71bdf3133b2d91fc3c6cad1";
const DISTILL_ROOT: &str = "distill-c3dba83d7ee336bcc71bdf3133b2d91fc3c6cad1";

#[tokio::test]
#[ignore = "downloads pinned remote GitHub archive"]
async fn radix_global_pax_and_long_ustar_path() -> Result {
    check(
        DISTILL_URL,
        DISTILL_ROOT,
        "radix",
        "0.5.0.9000",
        "56a528e382e7287a8a6b6ad948f6d66e93738f3eef64904a07e98ca0669d0574",
        true,
    )
    .await?;
    let path = format!(
        "{DISTILL_ROOT}/examples/websites/reproducible-finance-with-r/_posts/2017-03-07-quandl-and-forecasting/quandl-and-forecasting_files/dygraphs-binding-1.1.1.4/lib/timezone/moment-timezone-with-data.js"
    );
    let (body, _) = lookup(&client()?, DISTILL_URL, &path).await?;
    let body = body.expect("long ustar path must resolve exactly");
    assert_eq!(
        format!("{:x}", Sha256::digest(body)),
        "299ede9ea08aad5540621eb8c2807afc45472384f9d1d47b6db2afb151910925"
    );
    Ok(())
}
