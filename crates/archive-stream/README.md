# archive-stream

Internal, non-publishable utility for reading a regular file from a gzip-compressed
tar stream. Independent of HTTP and package metadata; accepts a Tokio `AsyncRead`.

`tar_gz_entry(reader, path)` returns `io::Result<Option<Vec<u8>>>`. It compares exact
paths (ignoring `.` components), skips non-regular entries, and buffers only the
matching entry plus decoder buffers. Nothing is extracted to disk.

Reading stops at the end of the first matching entry, so the remaining tar data
and gzip checksum are not validated. Missing entries return `None`; input and
archive errors encountered during the search propagate as I/O errors.
