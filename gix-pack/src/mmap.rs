use std::path::Path;

#[cfg(any(target_os = "motor", test))]
use std::{
    io::{self, Read},
    ops::Deref,
    sync::atomic::{AtomicUsize, Ordering},
};

#[cfg(target_os = "motor")]
const MAX_FILE_BYTES: usize = 128 * 1024 * 1024;
#[cfg(target_os = "motor")]
const MAX_LIVE_BYTES: usize = 256 * 1024 * 1024;

#[cfg(target_os = "motor")]
static LIVE_BYTES: Budget = Budget::new(MAX_LIVE_BYTES);

/// An owned, memory-backed pack file with a reservation against the native live-buffer limit.
#[cfg(any(target_os = "motor", test))]
#[derive(Debug)]
pub struct MMap {
    // Fields drop in declaration order, freeing the bytes before returning their reservation.
    data: Vec<u8>,
    _reservation: Reservation,
}

#[cfg(any(target_os = "motor", test))]
impl Deref for MMap {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

#[cfg(any(target_os = "motor", test))]
#[derive(Debug)]
struct Budget {
    used: AtomicUsize,
    limit: usize,
}

#[cfg(any(target_os = "motor", test))]
impl Budget {
    const fn new(limit: usize) -> Self {
        Self {
            used: AtomicUsize::new(0),
            limit,
        }
    }

    fn reserve(&'static self, bytes: usize) -> io::Result<Reservation> {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let next = used.checked_add(bytes).ok_or_else(live_limit_error)?;
            if next > self.limit {
                return Err(live_limit_error());
            }
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(Reservation { budget: self, bytes }),
                Err(current) => used = current,
            }
        }
    }

    #[cfg(test)]
    fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
}

#[cfg(any(target_os = "motor", test))]
#[derive(Debug)]
struct Reservation {
    budget: &'static Budget,
    bytes: usize,
}

#[cfg(any(target_os = "motor", test))]
impl Drop for Reservation {
    fn drop(&mut self) {
        let previous = self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
        debug_assert!(previous >= self.bytes);
    }
}

#[cfg(any(target_os = "motor", test))]
fn live_limit_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::OutOfMemory,
        "native pack buffers exceed the live-byte limit",
    )
}

#[cfg(any(target_os = "motor", test))]
fn read_with_limits<R: Read>(
    mut reader: R,
    len: usize,
    file_limit: usize,
    budget: &'static Budget,
) -> io::Result<MMap> {
    if len > file_limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "native pack file exceeds the per-file byte limit",
        ));
    }
    let reservation = budget.reserve(len)?;
    let mut data = Vec::new();
    data.try_reserve_exact(len)
        .map_err(|source| io::Error::new(io::ErrorKind::OutOfMemory, source))?;
    data.resize(len, 0);
    reader.read_exact(&mut data)?;
    if reader.read(&mut [0])? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "native pack file grew while it was read",
        ));
    }
    Ok(MMap {
        data,
        _reservation: reservation,
    })
}

#[cfg(target_os = "motor")]
pub fn read_only(path: &Path) -> std::io::Result<crate::MMap> {
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "native pack input is not a regular file",
        ));
    }
    let len = usize::try_from(metadata.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "native pack file length does not fit in memory",
        )
    })?;
    read_with_limits(file, len, MAX_FILE_BYTES, &LIVE_BYTES)
}

#[cfg(not(target_os = "motor"))]
pub fn read_only(path: &Path) -> std::io::Result<crate::MMap> {
    let file = std::fs::File::open(path)?;
    // SAFETY: we have to take the risk of somebody changing the file underneath. Git never writes into the same file.
    #[expect(unsafe_code)]
    unsafe {
        memmap2::MmapOptions::new().map_copy_read_only(&file)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor};

    use super::{Budget, read_with_limits};

    #[test]
    fn native_reads_enforce_file_and_live_limits_and_release_reservations() -> io::Result<()> {
        static BUDGET: Budget = Budget::new(16);
        assert_eq!(BUDGET.used(), 0);

        for len in [7, 8] {
            let map = read_with_limits(Cursor::new(vec![0; len]), len, 8, &BUDGET)?;
            assert_eq!(map.len(), len);
            drop(map);
            assert_eq!(BUDGET.used(), 0);
        }
        assert_eq!(
            read_with_limits(Cursor::new(vec![0; 9]), 9, 8, &BUDGET)
                .expect_err("a file above the per-file limit must be rejected")
                .kind(),
            io::ErrorKind::InvalidData
        );

        let first = read_with_limits(Cursor::new(vec![0; 8]), 8, 8, &BUDGET)?;
        let second = read_with_limits(Cursor::new(vec![0; 8]), 8, 8, &BUDGET)?;
        assert_eq!(BUDGET.used(), 16);
        assert_eq!(
            read_with_limits(Cursor::new(vec![0]), 1, 8, &BUDGET)
                .expect_err("the aggregate live-buffer limit must be enforced")
                .kind(),
            io::ErrorKind::OutOfMemory
        );
        drop(first);
        let replacement = read_with_limits(Cursor::new(vec![0]), 1, 8, &BUDGET)?;
        drop((second, replacement));
        assert_eq!(BUDGET.used(), 0);

        assert_eq!(
            read_with_limits(Cursor::new(vec![0; 7]), 8, 8, &BUDGET)
                .expect_err("a short read must fail")
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(BUDGET.used(), 0);
        assert_eq!(
            read_with_limits(Cursor::new(vec![0; 9]), 8, 8, &BUDGET)
                .expect_err("growth after metadata inspection must fail")
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(BUDGET.used(), 0);
        Ok(())
    }
}
