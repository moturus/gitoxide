use std::{fs::File, io, path::Path};

#[cfg(target_os = "motor")]
pub(crate) const MAX_CHAIN_BYTES: usize = 32 * 1024;
#[cfg(target_os = "motor")]
const MAX_GRAPH_BYTES: usize = 16 * 1024 * 1024;
#[cfg(target_os = "motor")]
const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;
#[cfg(target_os = "motor")]
const MAX_FILES: usize = 256;

pub(crate) struct Reader {
    max_file_bytes: usize,
    remaining_bytes: usize,
    remaining_files: usize,
}

impl Reader {
    fn new(max_file_bytes: usize, max_total_bytes: usize, max_files: usize) -> Self {
        Self {
            max_file_bytes,
            remaining_bytes: max_total_bytes,
            remaining_files: max_files,
        }
    }

    #[cfg(target_os = "motor")]
    pub(crate) fn for_graph() -> Self {
        Self::new(MAX_GRAPH_BYTES, MAX_TOTAL_BYTES, MAX_FILES)
    }

    #[cfg(target_os = "motor")]
    pub(crate) fn for_file() -> Self {
        Self::new(MAX_GRAPH_BYTES, MAX_GRAPH_BYTES, 1)
    }

    pub(crate) fn read(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        if self.remaining_files == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "commit-graph file count exceeds the limit",
            ));
        }
        let file = File::open(path)?;
        let data = gix_features::fs::read_to_end_bounded(&file, self.max_file_bytes.min(self.remaining_bytes))?;
        self.remaining_files -= 1;
        self.remaining_bytes -= data.len();
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use std::{io, path::Path};

    use super::Reader;

    #[test]
    fn reader_enforces_retained_byte_and_file_limits() -> io::Result<()> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/generated-archives/single_commit.tar");
        let file_len = usize::try_from(path.metadata()?.len()).expect("fixture size fits in memory");
        let total_len = file_len.checked_mul(2).expect("fixture total fits in memory");

        let mut reader = Reader::new(file_len, total_len, 3);
        let retained = [reader.read(&path)?, reader.read(&path)?];
        assert_eq!(retained.iter().map(Vec::len).sum::<usize>(), total_len);
        assert_eq!(
            reader
                .read(&path)
                .expect_err("aggregate byte limit is exhausted")
                .kind(),
            io::ErrorKind::InvalidData
        );

        let mut reader = Reader::new(file_len, total_len, 1);
        let _retained = reader.read(&path)?;
        assert_eq!(
            reader.read(&path).expect_err("file count is exhausted").kind(),
            io::ErrorKind::InvalidData
        );
        Ok(())
    }
}
