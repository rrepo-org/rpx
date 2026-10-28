# archive-stream

Internal, non-publishable utility for reading a file from a gzip- or XZ-compressed
tar stream. Independent of HTTP and package metadata; accepts a Tokio `AsyncRead`.

`tar_entry(reader, path)` returns `io::Result<Option<Vec<u8>>>`. It compares exact
paths (ignoring `.` components), selects regular or contiguous files, and buffers only the
matching entry plus decoder buffers. Nothing is extracted to disk.

Reading stops at the end of the first matching entry's padded tar block, so the remaining tar data
and gzip checksum are not validated. Missing entries return `None`; input and
archive errors encountered during the search propagate as I/O errors.

Compression is detected from the first six bytes and replayed into the decoder;
filenames and HTTP content types are not used. This handles the RRE-98 case of
XZ-compressed macOS R 4.6 binaries named `.tgz`. Unknown compression is rejected
as `InvalidData`, and truncated signatures as `UnexpectedEof`. XZ decoding accepts
concatenated streams. `tar_gz_entry` remains a compatibility alias for `tar_entry`.
Local tests use independently XZ-encoded tar data, one-byte input chunks, missing
entries, truncated input, and a broken XZ footer after the selected file.

`async-compression` is pinned to 0.4.19 to share the `xz2`/`lzma-sys` backend
already used by `r-package-installer` 0.2. Newer versions use `liblzma-sys`,
which conflicts with `lzma-sys` because both declare the native `lzma` link.
Upgrade these dependencies together when the installer changes backends.

## Decoder compatibility

Decoding uses `tar-codec` pinned to `0.0.15`, with its default policy, over the
async gzip/XZ decoders. The upstream crate is early-stage; upgrades should
run both the local regressions and the opt-in remote suite below.

Compared with the previous `astral-tokio-tar` 0.7 reader:
- Both regular and contiguous entries are returned as files by the new API.
  Symlinks and hard links are still excluded; the first matching file wins.
- The selected payload must include its final 512-byte tar block's padding.
  No later member, tar end marker, or gzip checksum is required after a match.
- A missing-entry search requires the two zero tar end blocks; bare EOF or a
  single end block is now an error.
- Member paths and link targets must be UTF-8, including on skipped entries.
  Legacy V7 archives and mixed GNU/PAX framing families are unsupported.
- Metadata parsing is stricter. Unknown PAX vendor records, duplicate PAX keys,
  and global path/size overrides are rejected by default. Metadata extension
  limits remain enabled. Ordinary global PAX comments from GitHub are accepted.

Structural/policy failures become `InvalidData`; underlying I/O error kinds are
preserved. These are intentional compatibility differences, not a claim of full
equivalence with the previous parser. All 15 pinned remote cases pass with the
default policy. The selected payload is accumulated in a Vec; skipped payloads
are drained incrementally by the decoder when advancing to the next member.

`astral-tokio-tar` is removed. `futures-util` is now only a test dependency of
this crate, and `tar` remains for fixture generation. `tar-codec` also brings
`tar-framing` and `archive-trait` (including the latter's filesystem dependencies),
even though this helper never extracts to disk.

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
