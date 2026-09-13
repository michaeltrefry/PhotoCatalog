//! Complete byte admission precedes request decoding or any helper filesystem
//! action. The outer pipe carries exact UTF-8 documents, not a parsed JSON AST.
use super::protocol::{Guard, ParentFrame, read_frame};
use anyhow::{Result, ensure};
use std::{io::Read, time::Instant};

pub(crate) const INPUT_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const TEXT_CHUNK: usize = 16 * 1024;
#[derive(Debug)]
pub(crate) struct Input {
    pub guard: Guard,
    pub digest: String,
    pub text: String,
}
pub(crate) fn digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "migration digest encoding"
    );
    Ok(())
}
pub(crate) fn receive(input: &mut impl Read, until: Instant) -> Result<Input> {
    ensure!(Instant::now() < until, "migration input deadline");
    let ParentFrame::Begin {
        guard,
        request_blake3,
        bytes,
    } = read_frame(input)?
    else {
        anyhow::bail!("migration Begin required before input");
    };
    guard.validate()?;
    digest(&request_blake3)?;
    let maximum = usize::try_from(bytes.0)?;
    ensure!(maximum <= INPUT_BYTES, "migration input byte admission");
    let mut text = String::with_capacity(maximum);
    // The parent encodes 16KiB chunks, shortened only at UTF-8 boundaries.
    // This separate frame bound prevents zero/single-byte message flooding.
    let mut frames = 0usize;
    loop {
        ensure!(Instant::now() < until, "migration input deadline");
        frames += 1;
        ensure!(
            frames <= INPUT_BYTES / (TEXT_CHUNK - 3) + 2,
            "migration input frame admission"
        );
        match read_frame(input)? {
            ParentFrame::Input {
                guard: next,
                offset,
                text: chunk,
            } => {
                ensure!(
                    next == guard && offset.0 == text.len() as u64,
                    "migration input guard/offset differs"
                );
                ensure!(
                    !chunk.is_empty()
                        && chunk.len() <= TEXT_CHUNK
                        && chunk.len() <= maximum - text.len(),
                    "migration input chunk byte admission"
                );
                text.push_str(&chunk);
            }
            ParentFrame::FinishInput {
                guard: next,
                blake3,
            } => {
                ensure!(
                    next == guard && blake3 == request_blake3 && text.len() == maximum,
                    "migration input completion differs"
                );
                // At most 32MiB, retained exactly once. No parsed documents or
                // filesystem-derived allocations exist during this hash.
                let mut hash = blake3::Hasher::new();
                for chunk in text.as_bytes().chunks(128 * 1024) {
                    ensure!(Instant::now() < until, "migration input hash deadline");
                    hash.update(chunk);
                }
                ensure!(
                    hash.finalize().to_hex().as_str() == request_blake3,
                    "migration input digest differs"
                );
                return Ok(Input {
                    guard,
                    digest: request_blake3,
                    text,
                });
            }
            ParentFrame::Cancel { guard: next } => {
                ensure!(next == guard, "stale migration input cancellation");
                anyhow::bail!("migration input canceled before execution");
            }
            _ => anyhow::bail!("unexpected migration input frame"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{application::U64, lightroom_migration_worker::protocol::write_frame};
    use std::time::Duration;
    fn guard() -> Guard {
        Guard {
            session: "s".into(),
            generation: "g".into(),
            operation: "o".into(),
        }
    }
    fn encoded(text: &str) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let hash = blake3::hash(text.as_bytes()).to_hex().to_string();
        write_frame(
            &mut bytes,
            &ParentFrame::Begin {
                guard: guard(),
                request_blake3: hash.clone(),
                bytes: U64(text.len() as u64),
            },
        )?;
        if !text.is_empty() {
            write_frame(
                &mut bytes,
                &ParentFrame::Input {
                    guard: guard(),
                    offset: U64(0),
                    text: text.into(),
                },
            )?;
        }
        write_frame(
            &mut bytes,
            &ParentFrame::FinishInput {
                guard: guard(),
                blake3: hash,
            },
        )?;
        Ok(bytes)
    }
    #[test]
    fn raw_authority_is_exact_and_truncation_never_admits() -> Result<()> {
        let raw = " \n{\"id\":9007199254740993,\"id\":1e99,\"__proto__\":\"é🦀\"}\n ";
        let bytes = encoded(raw)?;
        let admitted = receive(
            &mut bytes.as_slice(),
            Instant::now() + Duration::from_secs(1),
        )?;
        assert_eq!(admitted.text, raw);
        assert_eq!(admitted.guard, guard());
        assert_eq!(
            admitted.digest,
            blake3::hash(raw.as_bytes()).to_hex().as_str()
        );
        for length in [0, 3, bytes.len() - 1] {
            assert!(
                receive(
                    &mut &bytes[..length],
                    Instant::now() + Duration::from_secs(1)
                )
                .is_err()
            );
        }
        Ok(())
    }
    #[test]
    fn oversized_begin_and_cancel_fail_before_action_decode() -> Result<()> {
        let mut bytes = Vec::new();
        write_frame(
            &mut bytes,
            &ParentFrame::Begin {
                guard: guard(),
                request_blake3: "a".repeat(64),
                bytes: U64(INPUT_BYTES as u64 + 1),
            },
        )?;
        assert!(
            receive(
                &mut bytes.as_slice(),
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap_err()
            .to_string()
            .contains("byte admission")
        );
        bytes.clear();
        write_frame(
            &mut bytes,
            &ParentFrame::Begin {
                guard: guard(),
                request_blake3: "a".repeat(64),
                bytes: U64(10),
            },
        )?;
        write_frame(&mut bytes, &ParentFrame::Cancel { guard: guard() })?;
        assert!(
            receive(
                &mut bytes.as_slice(),
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap_err()
            .to_string()
            .contains("canceled")
        );
        Ok(())
    }
}
