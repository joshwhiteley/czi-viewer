use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use czi_core::{LocalFileSource, RandomAccessSource, SourceError};

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const SEGMENT_BYTES: usize = 4 * 1024;
const CONCURRENT_READERS: usize = 4;

struct TestFile {
    path: PathBuf,
}

impl TestFile {
    fn new(bytes: &[u8]) -> Self {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "czi-local-source-{}-{sequence}.bin",
            std::process::id()
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("create test source");
        file.write_all(bytes).expect("write test source");
        file.sync_all().expect("sync test source");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[test]
fn positioned_local_reads_keep_separate_offsets_concurrent_and_stable() {
    let bytes = (0..SEGMENT_BYTES * CONCURRENT_READERS)
        .map(|index| u8::try_from((index * 31 + index / SEGMENT_BYTES) % 251).expect("test byte"))
        .collect::<Vec<_>>();
    let file = TestFile::new(&bytes);
    let source = Arc::new(LocalFileSource::open(file.path()).expect("local source"));
    let captured = source.info();
    let expected = Arc::<[u8]>::from(bytes.clone());
    let barrier = Arc::new(Barrier::new(CONCURRENT_READERS + 1));

    let readers = (0..CONCURRENT_READERS)
        .map(|reader| {
            let source = Arc::clone(&source);
            let expected = Arc::clone(&expected);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                for iteration in 0..128 {
                    let within_segment = (iteration * 17) % (SEGMENT_BYTES - 257);
                    let start = reader * SEGMENT_BYTES + within_segment;
                    let mut actual = [0_u8; 257];
                    source
                        .read_at(u64::try_from(start).expect("offset"), &mut actual)
                        .expect("positioned read");
                    assert_eq!(actual.as_slice(), &expected[start..start + actual.len()]);
                }
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    for reader in readers {
        reader.join().expect("reader thread");
    }

    assert_eq!(source.info(), captured, "captured metadata changed");
    assert_eq!(fs::read(file.path()).expect("source contents"), bytes);
}

#[test]
fn positioned_local_reads_preserve_bounds_partial_eof_and_captured_metadata() {
    let bytes = (0_u8..8).collect::<Vec<_>>();
    let file = TestFile::new(&bytes);
    let source = LocalFileSource::open(file.path()).expect("local source");
    let captured = source.info();

    let mut empty = [];
    source
        .read_at(captured.length, &mut empty)
        .expect("empty read at EOF");
    assert!(matches!(
        source.read_at(captured.length + 1, &mut empty),
        Err(SourceError::OutOfBounds { .. })
    ));
    let mut overflow = [0_u8; 1];
    assert!(matches!(
        source.read_at(u64::MAX, &mut overflow),
        Err(SourceError::RangeOverflow { .. })
    ));

    OpenOptions::new()
        .write(true)
        .open(file.path())
        .expect("open truncation handle")
        .set_len(3)
        .expect("truncate source after capture");
    let mut partial = [0_u8; 8];
    let error = source
        .read_at(0, &mut partial)
        .expect_err("captured range now ends early");
    match error {
        SourceError::Io(error) => assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof),
        other => panic!("expected premature EOF, got {other:?}"),
    }
    assert_eq!(&partial[..3], &bytes[..3], "short-read prefix changed");
    assert_eq!(source.info(), captured, "captured metadata changed");
}
