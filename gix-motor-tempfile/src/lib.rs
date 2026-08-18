//! Named temporary files for gitoxide on Motor OS.

#![deny(missing_docs, unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions, Permissions};
use std::hash::{BuildHasher, Hash, Hasher};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_NAME: AtomicU64 = AtomicU64::new(0);

/// Configures creation of a named temporary file.
pub struct Builder {
    prefix: OsString,
    suffix: OsString,
    random_len: usize,
    permissions: Option<Permissions>,
}

impl Builder {
    /// Create a builder with the same naming defaults used by `tempfile`.
    pub fn new() -> Self {
        Self {
            prefix: ".tmp".into(),
            suffix: OsString::new(),
            random_len: 6,
            permissions: None,
        }
    }

    /// Set the fixed part before the generated name.
    pub fn prefix(&mut self, value: impl AsRef<OsStr>) -> &mut Self {
        self.prefix = value.as_ref().to_owned();
        self
    }

    /// Set the fixed part after the generated name.
    pub fn suffix(&mut self, value: impl AsRef<OsStr>) -> &mut Self {
        self.suffix = value.as_ref().to_owned();
        self
    }

    /// Set the number of generated ASCII characters.
    pub fn rand_bytes(&mut self, value: usize) -> &mut Self {
        self.random_len = value;
        self
    }

    /// Set the permissions applied immediately after creation.
    pub fn permissions(&mut self, value: Permissions) -> &mut Self {
        self.permissions = Some(value);
        self
    }

    /// Atomically create a named temporary file inside `directory`.
    pub fn tempfile_in(&self, directory: impl AsRef<Path>) -> io::Result<NamedTempFile> {
        let attempts = if self.random_len == 0 { 1 } else { 128 };
        for attempt in 0..attempts {
            let mut name = self.prefix.clone();
            name.push(random_component(self.random_len, attempt));
            name.push(&self.suffix);
            let path = directory.as_ref().join(name);
            match OpenOptions::new().read(true).write(true).create_new(true).open(&path) {
                Ok(file) => {
                    if let Some(permissions) = &self.permissions
                        && let Err(error) = file.set_permissions(permissions.clone())
                    {
                        drop(file);
                        let _ = fs::remove_file(&path);
                        return Err(error);
                    }
                    return Ok(NamedTempFile {
                        file,
                        path: TempPath { path },
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists && attempt + 1 < attempts => {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique temporary filename",
        ))
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

/// An open file which removes its name when dropped unless it is persisted.
pub struct NamedTempFile {
    file: File,
    path: TempPath,
}

impl NamedTempFile {
    /// Create a named temporary file in `directory`.
    pub fn new_in(directory: impl AsRef<Path>) -> io::Result<Self> {
        Builder::new().tempfile_in(directory)
    }

    /// Return the temporary path.
    pub fn path(&self) -> &Path {
        &self.path.path
    }

    /// Return the underlying file.
    pub fn as_file(&self) -> &File {
        &self.file
    }

    /// Return the underlying file mutably.
    pub fn as_file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    /// Convert into a path which retains automatic cleanup.
    pub fn into_temp_path(self) -> TempPath {
        self.path
    }

    /// Convert into the open file and its cleanup path.
    pub fn into_parts(self) -> (File, TempPath) {
        (self.file, self.path)
    }

    /// Persist the file at `destination`, atomically replacing it if present.
    pub fn persist(self, destination: impl AsRef<Path>) -> Result<File, PersistError> {
        let Self { file, path } = self;
        match path.persist(destination) {
            Ok(()) => Ok(file),
            Err(error) => Err(PersistError {
                error: error.error,
                file: Self { file, path: error.path },
            }),
        }
    }
}

impl fmt::Debug for NamedTempFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("NamedTempFile").field(&self.path).finish()
    }
}

impl Read for NamedTempFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

impl Write for NamedTempFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.file.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for NamedTempFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}

/// A temporary path which removes its file when dropped unless persisted.
#[derive(Debug)]
pub struct TempPath {
    path: PathBuf,
}

impl TempPath {
    /// Return an owned copy of the temporary path.
    pub fn to_path_buf(&self) -> PathBuf {
        self.path.clone()
    }

    /// Persist the file at `destination`, atomically replacing it if present.
    pub fn persist(mut self, destination: impl AsRef<Path>) -> Result<(), PathPersistError> {
        match fs::rename(&self.path, destination) {
            Ok(()) => {
                self.path = PathBuf::new();
                Ok(())
            }
            Err(error) => Err(PathPersistError { error, path: self }),
        }
    }
}

impl AsRef<Path> for TempPath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl std::ops::Deref for TempPath {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// An error which retains the open temporary file after persistence fails.
#[derive(Debug)]
pub struct PersistError {
    /// The filesystem error.
    pub error: io::Error,
    /// The temporary file which was not persisted.
    pub file: NamedTempFile,
}

impl fmt::Display for PersistError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "failed to persist temporary file: {}", self.error)
    }
}

impl std::error::Error for PersistError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// An error which retains the cleanup path after persistence fails.
#[derive(Debug)]
pub struct PathPersistError {
    /// The filesystem error.
    pub error: io::Error,
    /// The temporary path which was not persisted.
    pub path: TempPath,
}

fn random_component(length: usize, attempt: usize) -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    std::process::id().hash(&mut hasher);
    NEXT_NAME.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
        .hash(&mut hasher);
    attempt.hash(&mut hasher);
    let mut random = hasher.finish();
    (0..length)
        .map(|_| {
            let byte = ALPHABET[(random % ALPHABET.len() as u64) as usize];
            random = random.rotate_left(7) ^ 0x9e3779b97f4a7c15;
            char::from(byte)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_cleans_and_persists_named_files() -> Result<(), Box<dyn std::error::Error>> {
        let root = std::env::temp_dir().join(random_component(24, 0));
        fs::create_dir(&root)?;
        let temporary = NamedTempFile::new_in(&root)?;
        let temporary_path = temporary.path().to_owned();
        drop(temporary);
        assert!(!temporary_path.exists(), "drop must remove the temporary name");

        let mut temporary = NamedTempFile::new_in(&root)?;
        temporary.write_all(b"complete")?;
        let destination = root.join("destination");
        drop(temporary.persist(&destination)?);
        assert_eq!(fs::read(&destination)?, b"complete");
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn zero_random_bytes_create_the_exact_lock_name() -> Result<(), Box<dyn std::error::Error>> {
        let root = std::env::temp_dir().join(random_component(24, 0));
        fs::create_dir(&root)?;
        let mut builder = Builder::new();
        builder.prefix("shallow").suffix(".lock").rand_bytes(0);
        let first = builder.tempfile_in(&root)?;
        assert_eq!(first.path(), root.join("shallow.lock"));
        let error = builder
            .tempfile_in(&root)
            .expect_err("an existing lock must prevent another writer");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        drop(first);
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
