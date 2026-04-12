use std::io::{self, Cursor, Read, Write};
use fuse_backend_rs::api::filesystem::{ZeroCopyReader, ZeroCopyWriter};
use fuse_backend_rs::common::file_traits::FileReadWriteVolatile;

/// Captures data written by S3Fs::read().
pub struct TestWriter {
    pub buf: Vec<u8>,
}

impl TestWriter {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }
}

impl Write for TestWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl ZeroCopyWriter for TestWriter {
    fn write_from(
        &mut self,
        _f: &mut dyn FileReadWriteVolatile,
        _count: usize,
        _off: u64,
    ) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "not used in tests"))
    }

    fn available_bytes(&self) -> usize {
        usize::MAX
    }
}

/// Provides data to S3Fs::write().
pub struct TestReader {
    inner: Cursor<Vec<u8>>,
}

impl TestReader {
    pub fn new(data: Vec<u8>) -> Self {
        Self { inner: Cursor::new(data) }
    }
}

impl Read for TestReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl ZeroCopyReader for TestReader {
    fn read_to(
        &mut self,
        _f: &mut dyn FileReadWriteVolatile,
        _count: usize,
        _off: u64,
    ) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "not used in tests"))
    }
}
