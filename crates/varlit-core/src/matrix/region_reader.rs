use super::{Error, MatrixIo, Result, Source};
use crate::matrix_generation::View;
use std::cell::OnceCell;
use std::fs::File;
use std::sync::Arc;
#[cfg(test)]
use std::sync::Weak;

/// One reader's lazily opened private Windows file object. No thread-local
/// registry or shared handle list: this owner moves with its reader and is
/// Send + !Sync. A clone starts with its own empty handle cache.
#[derive(Debug, Default)]
pub struct MatrixReadPool {
    handle: OnceCell<Option<Arc<File>>>,
}

impl Clone for MatrixReadPool {
    fn clone(&self) -> Self {
        Self::new()
    }
}

impl MatrixReadPool {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn private_handle_count(&self) -> usize {
        usize::from(self.handle.get().is_some_and(Option::is_some))
    }

    #[cfg(test)]
    pub(crate) fn first_private_handle_weak(&self) -> Option<Weak<File>> {
        self.handle
            .get()
            .and_then(Option::as_ref)
            .map(Arc::downgrade)
    }

    #[cfg(windows)]
    fn reopen(file: &File) -> Option<Arc<File>> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, ReOpenFile,
        };

        // FILE_SHARE_DELETE matters: std opens with it, and without it here
        // a cached private handle would block deletion of a file the owner
        // has closed.
        let raw = unsafe {
            ReOpenFile(
                file.as_raw_handle() as _,
                FILE_GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                0,
            )
        };
        if raw.is_null() || raw == INVALID_HANDLE_VALUE {
            return None;
        }
        // SAFETY: `ReOpenFile` returned a handle owned by this call and not
        // aliased anywhere else; `File` takes ownership and closes it.
        let handle = Arc::new(unsafe { File::from_raw_handle(raw as RawHandle) });
        Some(handle)
    }

    #[cfg(not(windows))]
    fn reopen(_file: &File) -> Option<Arc<File>> {
        None
    }

    fn private_handle(&self, file: &File) -> Option<&Arc<File>> {
        self.handle.get_or_init(|| Self::reopen(file)).as_ref()
    }
}

/// A positional reader over one open matrix file.
///
/// `Copy`, because it is a shared borrow and callers pass it down several
/// levels. A borrowed reader carrying a local handle cache cannot cross threads.
#[derive(Clone, Copy, Debug)]
pub struct MatrixRegionReader<'a> {
    file: &'a File,
    pool: Option<&'a MatrixReadPool>,
    view: Option<&'a View>,
    #[cfg(feature = "mmap")]
    mapped: Option<&'a [u8]>,
}

impl<'a> MatrixRegionReader<'a> {
    pub(crate) fn new(file: &'a impl MatrixIo) -> Self {
        Self {
            file: file.raw(),
            pool: None,
            view: file.paged(),
            #[cfg(feature = "mmap")]
            mapped: None,
        }
    }

    /// The same reader, plus the private-handle pool of the file it reads.
    ///
    /// Reads issued through this are correct byte-for-byte whether or not
    /// the pool yields a handle; the pool only decides *which* file object
    /// carries them, and therefore whether concurrent readers convoy.
    #[cfg(test)]
    pub(crate) fn with_pool(file: &'a File, pool: &'a MatrixReadPool) -> Self {
        Self {
            file,
            pool: Some(pool),
            view: None,
            #[cfg(feature = "mmap")]
            mapped: None,
        }
    }

    pub(crate) fn with_source(source: &'a Source, pool: &'a MatrixReadPool) -> Self {
        Self {
            file: source.raw(),
            pool: Some(pool),
            view: source.paged(),
            #[cfg(feature = "mmap")]
            mapped: None,
        }
    }

    pub(crate) fn with_io(file: &'a impl MatrixIo, pool: &'a MatrixReadPool) -> Self {
        Self {
            file: file.raw(),
            pool: Some(pool),
            view: file.paged(),
            #[cfg(feature = "mmap")]
            mapped: None,
        }
    }

    pub(crate) fn bitmap_read(self) -> super::BitmapRead<'a> {
        #[cfg(feature = "mmap")]
        if let Some(bytes) = self.mapped {
            return super::BitmapRead::Mapped(bytes);
        }
        self.view
            .map_or(super::BitmapRead::Stored, super::BitmapRead::View)
    }

    pub(crate) fn with_backing(
        source: &'a Source,
        pool: &'a MatrixReadPool,
        read: super::BitmapRead<'a>,
    ) -> Self {
        let mut reader = Self::with_source(source, pool);
        match read {
            super::BitmapRead::Stored => {}
            super::BitmapRead::View(view) => reader.view = Some(view),
            #[cfg(feature = "mmap")]
            super::BitmapRead::Mapped(bytes) => {
                reader.view = None;
                reader.mapped = Some(bytes);
            }
        }
        reader
    }

    /// Fills `buffer` from `offset`.
    ///
    /// Short reads are retried and `Interrupted` is retried, matching
    /// `SnapshotFile::read_exact_at`; a zero-length read at end of data is
    /// `UnexpectedEof`, which is what the previous `read_exact` reported.
    ///
    /// Every read uses an absolute offset. Unix `pread` leaves the cursor
    /// unchanged; Windows `seek_read` may move it but never consults it.
    /// A supplied handle cache belongs to this reader and reopens the same
    /// file object read-only on Windows, without resolving its path again.
    pub(crate) fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> Result<()> {
        // One count per logical read, not per retry: the subject is whether
        // a read was *issued* while a bitmap page-store lock was held.
        super::count_matrix_region_read(self.pool.is_some());
        #[cfg(feature = "mmap")]
        if let Some(bytes) = self.mapped {
            let start = usize::try_from(offset).map_err(|_| Error::UnexpectedEof)?;
            let end = start
                .checked_add(buffer.len())
                .ok_or(Error::UnexpectedEof)?;
            buffer.copy_from_slice(bytes.get(start..end).ok_or(Error::UnexpectedEof)?);
            return Ok(());
        }
        if let Some(view) = self.view {
            return view.read_exact_at(offset, buffer).map_err(Error::Io);
        }
        let private = self.pool.and_then(|pool| pool.private_handle(self.file));
        let file: &File = private.map(Arc::as_ref).unwrap_or(self.file);
        let mut consumed = 0usize;
        while consumed < buffer.len() {
            let delta =
                u64::try_from(consumed).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
            let current = offset
                .checked_add(delta)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "matrix read offset",
                })?;
            match crate::snapshot::read_at(file, &mut buffer[consumed..], current) {
                Ok(0) => return Err(Error::UnexpectedEof),
                Ok(read) => consumed += read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{MatrixReadPool, MatrixRegionReader};
    use std::io::Write;

    /// The bytes must not depend on which file object carried the read, and
    /// a thread must reuse its private handle rather than opening one per
    /// read.
    #[test]
    fn a_pooled_read_returns_the_same_bytes_as_a_direct_one() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = directory.path().join("region");
        let contents: Vec<u8> = (0..4096u32).map(|byte| byte as u8).collect();
        {
            let mut file = std::fs::File::create(&path).expect("create");
            file.write_all(&contents).expect("write");
        }
        let file = std::fs::File::open(&path).expect("open");
        let pool = MatrixReadPool::new();

        let mut direct = [0u8; 64];
        MatrixRegionReader::new(&file)
            .read_exact_at(1000, &mut direct)
            .expect("direct read");
        let mut pooled = [0u8; 64];
        let reader = MatrixRegionReader::with_pool(&file, &pool);
        for _ in 0..16 {
            reader
                .read_exact_at(1000, &mut pooled)
                .expect("pooled read");
            assert_eq!(direct, pooled);
        }
        assert_eq!(&direct[..], &contents[1000..1064]);

        // One private handle for this thread and its sixteen reads — the
        // cold path runs once per thread per file, never per read. On
        // platforms where `pread` needs no private handle the pool
        // deliberately holds none and the reads above went through the
        // shared handle, which is why this is not asserted as `> 0`.
        let handles = pool.private_handle_count();
        assert!(
            handles <= 1,
            "expected at most one private handle for one thread, found {handles}"
        );
        #[cfg(windows)]
        assert_eq!(handles, 1, "windows must hand this thread its own handle");
    }

    /// Two threads with independent reader pools get their own file
    /// objects and their own correct bytes.
    #[test]
    fn two_threads_get_two_private_handles_and_the_right_bytes() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = directory.path().join("region");
        let contents: Vec<u8> = (0..8192u32).map(|byte| (byte / 7) as u8).collect();
        {
            let mut file = std::fs::File::create(&path).expect("create");
            file.write_all(&contents).expect("write");
        }
        let file = std::fs::File::open(&path).expect("open");
        let file = &file;
        let contents = &contents;

        std::thread::scope(|scope| {
            for thread in 0..2u64 {
                let pool = MatrixReadPool::new();
                scope.spawn(move || {
                    let reader = MatrixRegionReader::with_pool(file, &pool);
                    for step in 0..64u64 {
                        let offset = (thread * 512 + step * 13) % 4096;
                        let mut bytes = [0u8; 32];
                        reader
                            .read_exact_at(offset, &mut bytes)
                            .expect("threaded read");
                        let start = offset as usize;
                        assert_eq!(&bytes[..], &contents[start..start + 32]);
                    }
                    #[cfg(windows)]
                    assert_eq!(pool.private_handle_count(), 1);
                });
            }
        });
    }

    /// Dropping the reader-owned cache closes its private handle.
    #[test]
    fn dropping_the_pool_releases_the_private_handles() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = directory.path().join("region");
        std::fs::write(&path, vec![9u8; 512]).expect("write");
        let file = std::fs::File::open(&path).expect("open");

        let weak = {
            let pool = MatrixReadPool::new();
            let mut bytes = [0u8; 16];
            MatrixRegionReader::with_pool(&file, &pool)
                .read_exact_at(0, &mut bytes)
                .expect("pooled read");
            assert_eq!(bytes, [9u8; 16]);
            pool.first_private_handle_weak()
        };
        assert!(
            weak.is_none_or(|handle| handle.upgrade().is_none()),
            "a private handle outlived the pool that owns it"
        );
    }
}
