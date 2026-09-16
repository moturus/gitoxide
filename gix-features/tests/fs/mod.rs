use std::{fs::File, io, path::Path};

#[test]
fn bounded_file_reads_enforce_the_supplied_ceiling() -> io::Result<()> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let expected = std::fs::read(&path)?;

    for limit in [expected.len(), expected.len() + 1] {
        let file = File::open(&path)?;
        let actual = gix_features::fs::read_to_end_bounded(&file, limit)?;
        assert_eq!(actual, expected);
    }

    let file = File::open(path)?;
    let error = gix_features::fs::read_to_end_bounded(&file, expected.len() - 1)
        .expect_err("a regular file above the caller's limit must be rejected");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    Ok(())
}
