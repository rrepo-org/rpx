# archive-stream

Internal, non-publishable utility for reading a regular file from a gzip-compressed
tar stream. Independent of HTTP and package metadata; accepts a Tokio `AsyncRead`.

`tar_gz_entry(reader, path)` returns `io::Result<Option<Vec<u8>>>`. It compares exact
paths (ignoring `.` components), skips non-regular entries, and buffers only the
matching entry plus decoder buffers. Nothing is extracted to disk.

Reading stops at the end of the first matching entry, so the remaining tar data
and gzip checksum are not validated. Missing entries return `None`; input and
archive errors encountered during the search propagate as I/O errors.

## Compatibility tests

`cargo test -p archive-stream` runs offline regressions. Remote integration tests
are ignored by default; explicitly run them with:

```sh
cargo test -p archive-stream --test remote -- --ignored --test-threads=3
```

The suite streams 13 version-pinned CRAN archives and two commit-pinned GitHub
archives directly over HTTPS, without saving or committing fixtures. Each case
checks a DESCRIPTION SHA-256 (independently obtained with Python's `tarfile`),
package/version, and a missing-path search through the entire tar. BH and radix
also check early return using the number of compressed bytes delivered to the
reader. Radix additionally checks an exact long ustar path and its payload hash.

CRAN cases: R6 2.5.1, BH 1.84.0-0, Matrix 1.6-5, data.table 1.14.8,
ggplot2 0.9.1 (late DESCRIPTION), plyr 0.1.1, Rcpp 0.8.0, digest 0.2.0,
StanHeaders 2.21.0-7, rstan 2.21.2, stringi 1.7.12, RcppEigen 0.3.3.9.3,
and sf 0.9-8. These inspected archives use ordinary GNU headers.

GitHub cases cover global PAX metadata before DESCRIPTION:
- [kwbGompitz issue #6](https://github.com/KWB-R/kwbGompitz/issues/6),
  commit `ac68de7461ec6c49d316943ed0a623a4c0339e44`.
- [remotes issue #84](https://github.com/r-lib/remotes/issues/84), radix
  (repository renamed distill), commit `c3dba83d7ee336bcc71bdf3133b2d91fc3c6cad1`.
  This also contains long paths represented using ustar prefix/name fields.

Network/status errors fail these opt-in tests; they are not silently skipped.
The suite makes multiple requests per archive and transfers roughly 100 MB.
Hashes pin selected file contents rather than GitHub's generated gzip encoding.
Missing-path searches validate the tar encountered, not necessarily the gzip
trailer. Corrupt/truncated streams are covered by local generated regressions.
