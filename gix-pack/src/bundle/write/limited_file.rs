use std::io;

/// Tracks a newly created file from offset zero, inside BufWriter so accounting never forces a flush.
#[derive(Debug)]
pub(super) struct LimitedFile<W> {
    inner: W,
    position: u64,
    max_extent: Option<u64>,
}

impl<W> LimitedFile<W> {
    pub(super) fn for_target(inner: W) -> Self {
        #[cfg(target_os = "motor")]
        let max_extent = Some(128 * 1024 * 1024);
        #[cfg(not(target_os = "motor"))]
        let max_extent = None;
        Self::new(inner, max_extent)
    }

    fn new(inner: W, max_extent: Option<u64>) -> Self {
        Self {
            inner,
            position: 0,
            max_extent,
        }
    }

    /// Only for path lookup; reads, writes and seeks must go through this wrapper.
    pub(super) fn inner_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    pub(super) fn into_inner(self) -> W {
        self.inner
    }

    fn next_position(&self, bytes: usize) -> io::Result<u64> {
        self.position
            .checked_add(u64::try_from(bytes).map_err(|_| io::Error::other("file position overflowed"))?)
            .ok_or_else(|| io::Error::other("file position overflowed"))
    }
}

impl<W: io::Read> io::Read for LimitedFile<W> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.position = self.next_position(read)?;
        Ok(read)
    }
}

impl<W: io::Write> io::Write for LimitedFile<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let requested_end = self.next_position(buf.len())?;
        if self.max_extent.is_some_and(|limit| requested_end > limit) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "temporary pack exceeds its extent limit",
            ));
        }
        let written = self.inner.write(buf)?;
        self.position = self.next_position(written)?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<W: io::Seek> io::Seek for LimitedFile<W> {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        let position = self.inner.seek(pos)?;
        self.position = position;
        Ok(position)
    }
}

impl LimitedFile<gix_tempfile::Handle<gix_tempfile::handle::Writable>> {
    pub(super) fn truncate_and_seek(&mut self, len: u64) -> io::Result<()> {
        self.inner.with_mut(|file| file.as_file_mut().set_len(len))??;
        io::Seek::seek(self, io::SeekFrom::Start(len)).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Read, Seek, Write},
        sync::{Arc, atomic::AtomicBool},
    };

    use gix_tempfile::{AutoRemove, ContainingDirectory};

    use super::LimitedFile;
    use crate::{
        bundle::write::{Options, complete_pack, types::SharedTempFile},
        data,
    };

    #[test]
    fn writes_respect_extent_and_track_seek_and_read_positions() -> gix_testtools::Result {
        let mut file = LimitedFile::new(io::Cursor::new(Vec::new()), Some(4));
        file.write_all(b"abcd")?;
        assert_eq!(file.write(&[])?, 0, "empty writes remain no-ops at the limit");
        let error = file.write(b"e").expect_err("growth past the extent is rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(file.inner.get_ref(), b"abcd", "rejection happens before writing");

        file.seek(io::SeekFrom::Start(1))?;
        file.write_all(b"Z")?;
        file.rewind()?;
        let mut prefix = [0; 2];
        file.read_exact(&mut prefix)?;
        assert_eq!(&prefix, b"aZ");
        file.write_all(b"XY")?;
        assert_eq!(
            file.inner.get_ref(),
            b"aZXY",
            "writes follow the position updated by reads"
        );

        let dir = gix_testtools::tempfile::TempDir::new()?;
        let handle = gix_tempfile::new(dir.path(), ContainingDirectory::Exists, AutoRemove::Tempfile)?;
        let writer: SharedTempFile = Arc::new(parking_lot::Mutex::new(io::BufWriter::new(LimitedFile::new(
            handle,
            Some(31),
        ))));
        writer.lock().write_all(&data::header::encode(data::Version::V2, 0))?;
        let mut prepared = crate::index::Prepared {
            items: Vec::new(),
            missing_bases: Vec::new(),
            pack_hash: None,
            num_objects: 0,
            entries_end: 12,
        };
        let options = Options {
            iteration_mode: data::input::Mode::Restore,
            ..Options::default()
        };
        let error = complete_pack::<gix_object::find::Never>(
            &mut prepared,
            &writer,
            None,
            data::Version::V2,
            &options,
            &AtomicBool::new(false),
        )
        .expect_err("the empty pack trailer crosses the tiny extent");
        assert!(matches!(
            error,
            crate::bundle::write::Error::Io(err) if err.kind() == io::ErrorKind::InvalidData
        ));
        drop(writer);
        assert!(
            std::fs::read_dir(dir.path())?.next().is_none(),
            "the owned temporary file is removed after failure"
        );
        Ok(())
    }
}
