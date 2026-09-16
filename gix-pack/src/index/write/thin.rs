use std::sync::atomic::{AtomicBool, Ordering};

use crate::data::{Offset, entry::Header};

use super::Error;

#[derive(Clone, Copy)]
pub(super) struct Received {
    pub id: gix_hash::ObjectId,
    pub offset: Offset,
    pub header: Header,
}

pub(super) fn ensure_unique<'a>(ids: impl IntoIterator<Item = &'a gix_hash::ObjectId>) -> Result<(), Error> {
    let mut previous = None;
    for id in ids {
        if previous == Some(id) {
            return Err(Error::DuplicateObject { object_id: *id });
        }
        previous = Some(id);
    }
    Ok(())
}

/// Return lookup candidates which aren't already represented by a received object.
///
/// Candidates present in the input must have an original dependency chain ending in a full
/// received object or a genuinely external candidate. Otherwise a virtual lookup root merely
/// hides a cycle which would remain unresolved in the published pack.
/// `received` must be sorted by unique object ID.
pub(super) fn missing_bases(
    received: &[Received],
    mut candidates: Vec<gix_hash::ObjectId>,
    should_interrupt: &AtomicBool,
) -> Result<Vec<gix_hash::ObjectId>, Error> {
    let mut by_offset = indices(received.len())?;
    by_offset.sort_unstable_by_key(|&index| received[index].offset);
    candidates.sort_unstable();

    let mut missing = Vec::new();
    missing.try_reserve_exact(candidates.len())?;
    let mut received_candidates = Vec::new();
    received_candidates.try_reserve_exact(candidates.len())?;
    for candidate in candidates {
        match received.binary_search_by_key(&candidate, |item| item.id) {
            Ok(index) => received_candidates.push(index),
            Err(_) => missing.push(candidate),
        }
    }

    let mut state = Vec::new();
    state.try_reserve_exact(received.len())?;
    state.resize(received.len(), State::Unknown);
    let mut path = Vec::new();
    path.try_reserve(received.len())?;
    for start in received_candidates {
        if should_interrupt.load(Ordering::Relaxed) {
            return Err(crate::cache::delta::traverse::Error::Interrupted.into());
        }
        path.clear();
        let mut current = start;
        loop {
            if should_interrupt.load(Ordering::Relaxed) {
                return Err(crate::cache::delta::traverse::Error::Interrupted.into());
            }
            match state[current] {
                State::Rooted => break,
                State::Visiting => {
                    return Err(Error::UnrootedDeltaChain {
                        object_id: received[current].id,
                    });
                }
                State::Unknown => {
                    state[current] = State::Visiting;
                    path.push(current);
                }
            }
            let item = received[current];
            let dependency = match item.header {
                Header::Tree | Header::Blob | Header::Commit | Header::Tag => None,
                Header::OfsDelta { base_distance } => Header::verified_base_pack_offset(item.offset, base_distance)
                    .and_then(|offset| {
                        by_offset
                            .binary_search_by_key(&offset, |&index| received[index].offset)
                            .ok()
                            .map(|position| by_offset[position])
                    })
                    .map(Some)
                    .ok_or(Error::UnrootedDeltaChain { object_id: item.id })?,
                Header::RefDelta { base_id } => match received.binary_search_by_key(&base_id, |item| item.id) {
                    Ok(index) => Some(index),
                    Err(_) if missing.binary_search(&base_id).is_ok() => None,
                    Err(_) => return Err(Error::UnrootedDeltaChain { object_id: item.id }),
                },
            };
            match dependency {
                Some(index) => current = index,
                None => break,
            }
        }
        for index in path.drain(..) {
            state[index] = State::Rooted;
        }
    }
    Ok(missing)
}

fn indices(len: usize) -> Result<Vec<usize>, Error> {
    let mut out = Vec::new();
    out.try_reserve_exact(len)?;
    out.extend(0..len);
    Ok(out)
}

#[derive(Clone, Copy)]
enum State {
    Unknown,
    Visiting,
    Rooted,
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::{Received, ensure_unique, missing_bases};
    use crate::{data::entry::Header, index::write::Error};

    #[test]
    fn candidates_are_subtracted_and_must_retain_a_real_root() {
        let hash = gix_hash::Kind::Sha1;
        let id = |data| gix_object::compute_hash(hash, gix_object::Kind::Blob, data).expect("hash object");
        let (a, b, c, external) = (id(b"A"), id(b"B"), id(b"C"), id(b"external"));
        let mut valid = [
            Received {
                id: a,
                offset: 12,
                header: Header::Blob,
            },
            Received {
                id: b,
                offset: 20,
                header: Header::RefDelta { base_id: a },
            },
            Received {
                id: c,
                offset: 30,
                header: Header::OfsDelta { base_distance: 10 },
            },
        ];
        valid.sort_by_key(|item| item.id);
        assert_eq!(
            missing_bases(&valid, vec![a, b, c, external], &AtomicBool::new(false))
                .expect("all received candidates are rooted"),
            [external]
        );

        let self_reference = [Received {
            id: a,
            offset: 12,
            header: Header::RefDelta { base_id: a },
        }];
        assert!(matches!(
            missing_bases(&self_reference, vec![a], &AtomicBool::new(false)),
            Err(Error::UnrootedDeltaChain { object_id }) if object_id == a
        ));

        let duplicate = [a, a];
        assert!(matches!(
            ensure_unique(&duplicate),
            Err(Error::DuplicateObject { object_id }) if object_id == a
        ));
    }
}
