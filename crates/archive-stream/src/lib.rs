//! Read individual regular files from streaming archives without extracting to disk.
use async_compression::tokio::bufread::{GzipDecoder, XzDecoder};
use std::{io, path::Path};
use tar_codec::{Archive, Member, MemberPayload, TarArchive};
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};

/// Read the first regular or contiguous file whose path equals `path` from a gzip/XZ tar stream.
/// Compression is detected from the input signature, independently of filenames.
/// Only the selected file and decoder buffers are held in memory. Stops at the
/// end of that entry's padded tar block: the remainder (including its gzip checksum)
/// is not validated. No archive paths are written to the filesystem.
pub async fn tar_entry(
    mut reader: impl AsyncRead + Unpin,
    path: impl AsRef<Path>,
) -> io::Result<Option<Vec<u8>>> {
    let mut signature = [0; 6];
    reader.read_exact(&mut signature).await?;
    // Replay the signature without seeking or buffering the archive. read_exact
    // handles network readers that return fewer than six bytes per read.
    let reader = BufReader::new(signature.as_slice().chain(reader));
    if signature.starts_with(b"\x1f\x8b") {
        read_tar_entry(GzipDecoder::new(reader), path).await
    } else if signature == *b"\xfd7zXZ\0" {
        let mut decoder = XzDecoder::new(reader);
        decoder.multiple_members(true);
        read_tar_entry(decoder, path).await
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported archive compression (expected gzip or XZ)",
        ))
    }
}

/// Compatibility name for [`tar_entry`]; now accepts both gzip and XZ.
pub use tar_entry as tar_gz_entry;

async fn read_tar_entry(
    reader: impl AsyncRead + Unpin,
    path: impl AsRef<Path>,
) -> io::Result<Option<Vec<u8>>> {
    let mut members = TarArchive::new(reader).members();
    let path = path.as_ref();
    while let Some(member) = members.next().await.map_err(decode_error)? {
        // Advancing automatically drains the previous member using bounded buffers.
        if let Member::File {
            metadata,
            mut payload,
            ..
        } = member
            && Path::new(&metadata.path)
                .components()
                .filter(|part| *part != std::path::Component::CurDir)
                .eq(path
                    .components()
                    .filter(|part| *part != std::path::Component::CurDir))
        {
            let mut bytes = Vec::new();
            // Keep the first chunk as the result; small files need no extra copy.
            if payload
                .next_chunk(&mut bytes, 64 * 1024)
                .await
                .map_err(decode_error)?
            {
                let mut chunk = Vec::new();
                while payload
                    .next_chunk(&mut chunk, 64 * 1024)
                    .await
                    .map_err(decode_error)?
                {
                    bytes.extend_from_slice(&chunk);
                }
            }
            return Ok(Some(bytes));
        }
    }
    Ok(None)
}

fn decode_error(error: tar_codec::DecodeError) -> io::Error {
    // Keep transport error kinds (e.g. ConnectionReset) through framing wrappers.
    let mut source: &dyn std::error::Error = &error;
    loop {
        if let Some(io) = source.downcast_ref::<io::Error>() {
            return io::Error::new(io.kind(), error);
        }
        match source.source() {
            Some(next) => source = next,
            None => return io::Error::new(io::ErrorKind::InvalidData, error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
    };
    use tokio::io::ReadBuf;

    fn archive(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::none(),
        ));
        for (path, kind, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, path, *body).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    struct Chunks {
        bytes: Vec<u8>,
        read: Arc<AtomicUsize>,
        fail_at: usize,
    }
    impl AsyncRead for Chunks {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            output: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let pos = self.read.load(Ordering::Relaxed);
            if pos >= self.fail_at {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "broken input",
                )));
            }
            let len = 13
                .min(output.remaining())
                .min(self.bytes.len() - pos)
                .min(self.fail_at - pos);
            output.put_slice(&self.bytes[pos..pos + len]);
            self.read.fetch_add(len, Ordering::Relaxed);
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn reads_exact_regular_file_and_stops_before_unrelated_payload() {
        let bytes = archive(&[
            ("other/DESCRIPTION", tar::EntryType::Regular, b"wrong"),
            ("pkg/nested/DESCRIPTION", tar::EntryType::Regular, b"wrong"),
            ("pkg/DESCRIPTION", tar::EntryType::Symlink, b""),
            ("./pkg/DESCRIPTION", tar::EntryType::Regular, b"correct"),
            ("pkg/large", tar::EntryType::Regular, &vec![42; 1_000_000]),
        ]);
        let read = Arc::new(AtomicUsize::new(0));
        let reader = Chunks {
            bytes,
            read: read.clone(),
            fail_at: 8192,
        };
        assert_eq!(
            tar_gz_entry(reader, "pkg/DESCRIPTION").await.unwrap(),
            Some(b"correct".to_vec())
        );
        assert!(read.load(Ordering::Relaxed) < 8192);
    }

    #[tokio::test]
    async fn collects_empty_single_and_multiple_chunk_payloads() {
        for size in [0, 1, 64 * 1024, 64 * 1024 + 1, 150_000] {
            let body: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let bytes = archive(&[("pkg/DESCRIPTION", tar::EntryType::Regular, &body)]);
            assert_eq!(
                tar_entry(bytes.as_slice(), "pkg/DESCRIPTION")
                    .await
                    .unwrap(),
                Some(body),
                "payload size {size}"
            );
        }
    }

    #[tokio::test]
    async fn reads_xz_compressed_tgz_like_macos_r_46_binaries() {
        // RRE-98: macOS binary downloads named .tgz can contain XZ, not gzip.
        // The reader receives bytes, so compression must not depend on a suffix.
        let gzip = archive(&[(
            "curl/DESCRIPTION",
            tar::EntryType::Regular,
            b"Package: curl\nVersion: 8.1.0\n",
        )]);
        let mut plain = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::GzDecoder::new(gzip.as_slice()),
            &mut plain,
        )
        .unwrap();
        let mut encoder = xz2::write::XzEncoder::new(Vec::new(), 1);
        std::io::Write::write_all(&mut encoder, &plain).unwrap();
        let xz = encoder.finish().unwrap();
        assert!(xz.starts_with(b"\xfd7zXZ\0"));
        assert!(tar_entry(&xz[..20], "curl/DESCRIPTION").await.is_err());
        let mut broken_tail = xz.clone();
        let last = broken_tail.len() - 1;
        broken_tail[last] ^= 0xff;
        assert_eq!(
            tar_entry(broken_tail.as_slice(), "curl/DESCRIPTION")
                .await
                .unwrap(),
            Some(b"Package: curl\nVersion: 8.1.0\n".to_vec())
        );
        for bytes in [gzip, xz] {
            let reader = tokio_util::io::StreamReader::new(futures_util::stream::iter(
                bytes.chunks(1).map(Ok::<_, io::Error>),
            ));
            assert_eq!(
                tar_gz_entry(reader, "curl/DESCRIPTION").await.unwrap(),
                Some(b"Package: curl\nVersion: 8.1.0\n".to_vec())
            );
            assert_eq!(
                tar_gz_entry(bytes.as_slice(), "curl/missing")
                    .await
                    .unwrap(),
                None
            );
        }
    }

    #[tokio::test]
    async fn rejects_unsupported_and_truncated_compression_signatures() {
        for signature in [b"\x1f\x8b".as_slice(), b"\xfd7zXZ\0".as_slice()] {
            for end in 0..signature.len() {
                assert_eq!(
                    tar_entry(&signature[..end], "entry")
                        .await
                        .unwrap_err()
                        .kind(),
                    io::ErrorKind::UnexpectedEof
                );
            }
        }
        assert_eq!(
            tar_entry(b"not gzip".as_slice(), "entry")
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn reads_contiguous_files() {
        // tar-codec projects regular and contiguous entries to Member::File.
        let bytes = archive(&[("pkg/DESCRIPTION", tar::EntryType::Continuous, b"body")]);
        assert_eq!(
            tar_gz_entry(bytes.as_slice(), "pkg/DESCRIPTION")
                .await
                .unwrap(),
            Some(b"body".to_vec())
        );
    }

    #[tokio::test]
    async fn requires_target_padding_but_not_archive_end_markers() {
        let bytes = archive(&[("pkg/DESCRIPTION", tar::EntryType::Regular, b"body")]);
        let mut plain = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::GzDecoder::new(bytes.as_slice()),
            &mut plain,
        )
        .unwrap();
        for (end, succeeds) in [(512 + 4, false), (1024, true)] {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::none());
            std::io::Write::write_all(&mut encoder, &plain[..end]).unwrap();
            let truncated = encoder.finish().unwrap();
            let result = tar_gz_entry(truncated.as_slice(), "pkg/DESCRIPTION").await;
            if succeeds {
                assert_eq!(result.unwrap(), Some(b"body".to_vec()));
                // A missing-entry search must reach proper tar end markers.
                assert!(tar_gz_entry(truncated.as_slice(), "missing").await.is_err());
            } else {
                assert!(result.is_err());
            }
        }
    }

    #[tokio::test]
    async fn skips_large_payload_and_returns_first_duplicate() {
        let bytes = archive(&[
            ("pkg/large", tar::EntryType::Regular, &vec![42; 2_000_000]),
            ("pkg/DESCRIPTION", tar::EntryType::Regular, b"first"),
            ("pkg/DESCRIPTION", tar::EntryType::Regular, b"second"),
        ]);
        assert_eq!(
            tar_gz_entry(bytes.as_slice(), "./pkg/./DESCRIPTION")
                .await
                .unwrap(),
            Some(b"first".to_vec())
        );
    }

    #[tokio::test]
    async fn rejects_truncated_target_but_accepts_complete_target_before_broken_tail() {
        let bytes = archive(&[
            (
                "pkg/DESCRIPTION",
                tar::EntryType::Regular,
                &vec![42; 32_000],
            ),
            ("pkg/tail", tar::EntryType::Regular, &vec![42; 32_000]),
        ]);
        // Stored DEFLATE keeps these cuts well inside the selected payload and
        // the following payload, respectively. No reliance on compression ratio.
        assert!(
            tar_gz_entry(&bytes[..4096], "pkg/DESCRIPTION")
                .await
                .is_err()
        );
        assert_eq!(
            tar_gz_entry(&bytes[..40_000], "pkg/DESCRIPTION")
                .await
                .unwrap(),
            Some(vec![42; 32_000])
        );
        assert!(tar_gz_entry(&bytes[..40_000], "pkg/missing").await.is_err());
    }

    #[tokio::test]
    async fn does_not_require_gzip_checksum_after_target() {
        let mut bytes = archive(&[
            ("pkg/DESCRIPTION", tar::EntryType::Regular, b"correct"),
            ("pkg/tail", tar::EntryType::Regular, &vec![42; 32_000]),
        ]);
        let checksum = bytes.len() - 8;
        bytes[checksum] ^= 0xff;
        assert_eq!(
            tar_gz_entry(bytes.as_slice(), "pkg/DESCRIPTION")
                .await
                .unwrap(),
            Some(b"correct".to_vec())
        );
    }

    #[tokio::test]
    async fn distinguishes_missing_invalid_and_broken_streams() {
        let bytes = archive(&[("pkg/other", tar::EntryType::Regular, b"body")]);
        assert_eq!(
            tar_gz_entry(bytes.as_slice(), "pkg/DESCRIPTION")
                .await
                .unwrap(),
            None
        );
        assert!(tar_gz_entry(b"not gzip".as_slice(), "entry").await.is_err());
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::none());
        std::io::Write::write_all(&mut encoder, &[42; 512]).unwrap();
        let corrupt_tar = encoder.finish().unwrap();
        assert!(tar_gz_entry(corrupt_tar.as_slice(), "entry").await.is_err());
        let reader = Chunks {
            bytes,
            read: Arc::new(AtomicUsize::new(0)),
            fail_at: 20,
        };
        assert_eq!(
            tar_gz_entry(reader, "pkg/DESCRIPTION")
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::ConnectionReset
        );
    }
}
