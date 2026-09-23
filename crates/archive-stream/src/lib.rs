//! Read individual regular files from streaming archives without extracting to disk.
use async_compression::tokio::bufread::GzipDecoder;
use futures_util::TryStreamExt;
use std::{io, path::Path};
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};

/// Read the first regular file whose path equals `path` from a gzip tar stream.
/// Only the selected file and decoder buffers are held in memory. Stops at the
/// end of that entry: the remainder of the archive (including its gzip checksum)
/// is not validated. No archive paths are written to the filesystem.
pub async fn tar_gz_entry(
    reader: impl AsyncRead + Unpin,
    path: impl AsRef<Path>,
) -> io::Result<Option<Vec<u8>>> {
    let mut archive = tokio_tar::Archive::new(GzipDecoder::new(BufReader::new(reader)));
    let path = path.as_ref();
    let entries = archive.entries()?.try_filter_map(|mut entry| async move {
        if entry.header().entry_type().is_file()
            && entry
                .path()?
                .components()
                .filter(|part| *part != std::path::Component::CurDir)
                .eq(path
                    .components()
                    .filter(|part| *part != std::path::Component::CurDir))
        {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).await?;
            Ok(Some(bytes))
        } else {
            Ok(None)
        }
    });
    futures_util::pin_mut!(entries);
    entries.try_next().await
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
