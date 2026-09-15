//! File timestamps for gitoxide on Motor OS.

#![deny(missing_docs, unsafe_code)]

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

/// A filesystem timestamp represented relative to the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileTime {
    seconds: i64,
    nanoseconds: u32,
}

impl FileTime {
    /// Return the current system time as a filesystem timestamp.
    pub fn now() -> Self {
        Self::from_system_time(SystemTime::now())
    }

    /// Return the modification time reported by filesystem metadata.
    pub fn from_last_modification_time(metadata: &fs::Metadata) -> Self {
        Self::from_system_time(
            metadata
                .modified()
                .expect("Motor OS metadata always provides a modification time"),
        )
    }

    /// Return whole seconds relative to the Unix epoch.
    pub const fn unix_seconds(&self) -> i64 {
        self.seconds
    }

    /// Return nanoseconds forward from the whole-second value.
    pub const fn nanoseconds(&self) -> u32 {
        self.nanoseconds
    }

    fn from_system_time(time: SystemTime) -> Self {
        match time.duration_since(UNIX_EPOCH) {
            Ok(duration) => Self {
                seconds: i64::try_from(duration.as_secs()).expect("SystemTime seconds fit in i64"),
                nanoseconds: duration.subsec_nanos(),
            },
            Err(error) => {
                let duration = error.duration();
                let seconds = i64::try_from(duration.as_secs()).expect("SystemTime seconds fit in i64");
                if duration.subsec_nanos() == 0 {
                    Self {
                        seconds: -seconds,
                        nanoseconds: 0,
                    }
                } else {
                    Self {
                        seconds: -seconds - 1,
                        nanoseconds: 1_000_000_000 - duration.subsec_nanos(),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{FileTime, UNIX_EPOCH};

    #[test]
    fn system_times_are_normalized_and_ordered() {
        let cases = [
            (UNIX_EPOCH - Duration::new(1, 1), -2, 999_999_999),
            (UNIX_EPOCH - Duration::from_secs(1), -1, 0),
            (UNIX_EPOCH - Duration::from_nanos(1), -1, 999_999_999),
            (UNIX_EPOCH, 0, 0),
            (UNIX_EPOCH + Duration::new(1, 42), 1, 42),
        ];
        let actual: Vec<_> = cases
            .iter()
            .map(|(time, seconds, nanoseconds)| {
                let timestamp = FileTime::from_system_time(*time);
                assert_eq!(timestamp.unix_seconds(), *seconds);
                assert_eq!(timestamp.nanoseconds(), *nanoseconds);
                timestamp
            })
            .collect();

        assert!(actual.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
