//! Complete byte admission precedes request decoding or any helper filesystem
//! action. The outer pipe carries exact UTF-8 documents, not a parsed JSON AST.
use super::protocol::{Guard, InputRole, ParentFrame, read_frame};
use crate::application::U64;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{io::Read, time::Instant};

/// Legacy single-request transport. New production workers use admitted parts
/// below; this bound remains unchanged for existing process fixtures.
pub(crate) const INPUT_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const TEXT_CHUNK: usize = 16 * 1024;
pub(crate) const OPERATION_BYTES: usize = crate::lightroom::MANIFEST_BYTES;
pub(crate) const CLI_DOCUMENT_BYTES: usize = crate::lightroom::MANIFEST_BYTES;
pub(crate) const EXECUTION_AUTHORIZATION_BYTES: usize = super::authority::DOCUMENT_BYTES;
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

impl InputRole {
    pub(crate) fn maximum_bytes(self) -> usize {
        match self {
            Self::Operation => OPERATION_BYTES,
            Self::ExecutionAuthorization => EXECUTION_AUTHORIZATION_BYTES,
            Self::Seal
            | Self::Approval
            | Self::Policy
            | Self::RepairRequest
            | Self::SupplementRequests => CLI_DOCUMENT_BYTES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartDescriptor {
    pub role: InputRole,
    pub bytes: U64,
    pub blake3: String,
}
impl PartDescriptor {
    fn validate(&self) -> Result<usize> {
        digest(&self.blake3)?;
        let bytes = usize::try_from(self.bytes.0)?;
        ensure!(
            bytes <= self.role.maximum_bytes(),
            "migration input part byte admission"
        );
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DocumentSet {
    None,
    Run,
    RunWithAuthorization,
    Repair,
    PrepareSupplements,
}
impl DocumentSet {
    fn roles(self) -> &'static [InputRole] {
        use InputRole::*;
        match self {
            Self::None => &[],
            Self::Run => &[Seal, Approval, Policy],
            Self::RunWithAuthorization => &[Seal, Approval, Policy, ExecutionAuthorization],
            Self::Repair => &[Seal, Approval, RepairRequest],
            Self::PrepareSupplements => &[SupplementRequests],
        }
    }
    pub(crate) fn validate(self, parts: &[PartDescriptor]) -> Result<usize> {
        ensure!(
            parts.len() == self.roles().len(),
            "migration input document roster differs"
        );
        parts
            .iter()
            .zip(self.roles())
            .try_fold(0usize, |total, (part, role)| {
                ensure!(part.role == *role, "migration input document order differs");
                total
                    .checked_add(part.validate()?)
                    .context("migration input document aggregate overflow")
            })
    }
}

#[derive(Debug)]
pub(crate) struct AdmittedPart<G> {
    text: String,
    digest: [u8; 64],
    // Last: requested-storage admission outlives the String it covers.
    _admission: G,
}
impl<G> AdmittedPart<G> {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }
    pub(crate) fn digest(&self) -> &str {
        std::str::from_utf8(&self.digest).expect("validated ASCII digest")
    }
}

enum StreamFrame {
    Chunk {
        guard: Guard,
        role: Option<InputRole>,
        offset: U64,
        text: String,
    },
    Finish {
        guard: Guard,
        role: Option<InputRole>,
        blake3: String,
    },
    Cancel(Guard),
    Unexpected,
}
fn stream_frame(frame: ParentFrame) -> StreamFrame {
    match frame {
        ParentFrame::Input {
            guard,
            offset,
            text,
        } => StreamFrame::Chunk {
            guard,
            role: None,
            offset,
            text,
        },
        ParentFrame::Part {
            guard,
            role,
            offset,
            text,
        } => StreamFrame::Chunk {
            guard,
            role: Some(role),
            offset,
            text,
        },
        ParentFrame::FinishInput { guard, blake3 } => StreamFrame::Finish {
            guard,
            role: None,
            blake3,
        },
        ParentFrame::FinishPart {
            guard,
            role,
            blake3,
        } => StreamFrame::Finish {
            guard,
            role: Some(role),
            blake3,
        },
        ParentFrame::Cancel { guard } => StreamFrame::Cancel(guard),
        _ => StreamFrame::Unexpected,
    }
}

fn receive_stream_admitted<G>(
    input: &mut impl Read,
    expected_guard: &Guard,
    expected_role: Option<InputRole>,
    maximum: usize,
    fixed_digest: [u8; 64],
    until: Instant,
    admit: impl FnOnce(usize) -> Result<G>,
) -> Result<AdmittedPart<G>> {
    let admission = admit(maximum)?;
    let mut text = String::with_capacity(maximum);
    // The sender uses 16KiB chunks, shortened by at most three bytes at a UTF-8
    // boundary. This prevents zero/single-byte frame flooding.
    let mut frames = 0usize;
    loop {
        ensure!(Instant::now() < until, "migration input deadline");
        frames = frames
            .checked_add(1)
            .context("migration input frame count overflow")?;
        ensure!(
            frames <= maximum / (TEXT_CHUNK - 3) + 2,
            "migration input frame admission"
        );
        match stream_frame(read_frame(input)?) {
            StreamFrame::Chunk {
                guard,
                role,
                offset,
                text: chunk,
            } => {
                ensure!(
                    guard == *expected_guard
                        && role == expected_role
                        && offset.0 == text.len() as u64,
                    if expected_role.is_some() {
                        "migration input part guard/role/offset differs"
                    } else {
                        "migration input guard/offset differs"
                    }
                );
                ensure!(
                    !chunk.is_empty()
                        && chunk.len() <= TEXT_CHUNK
                        && chunk.len() <= maximum - text.len(),
                    if expected_role.is_some() {
                        "migration input part chunk byte admission"
                    } else {
                        "migration input chunk byte admission"
                    }
                );
                text.push_str(&chunk);
            }
            StreamFrame::Finish {
                guard,
                role,
                blake3,
            } => {
                ensure!(
                    guard == *expected_guard
                        && role == expected_role
                        && blake3.as_bytes() == fixed_digest
                        && text.len() == maximum,
                    if expected_role.is_some() {
                        "migration input part completion differs"
                    } else {
                        "migration input completion differs"
                    }
                );
                let mut hash = blake3::Hasher::new();
                for chunk in text.as_bytes().chunks(128 * 1024) {
                    ensure!(Instant::now() < until, "migration input hash deadline");
                    hash.update(chunk);
                }
                ensure!(
                    hash.finalize().to_hex().as_bytes() == fixed_digest,
                    if expected_role.is_some() {
                        "migration input part digest differs"
                    } else {
                        "migration input digest differs"
                    }
                );
                return Ok(AdmittedPart {
                    text,
                    digest: fixed_digest,
                    _admission: admission,
                });
            }
            StreamFrame::Cancel(guard) => {
                ensure!(
                    guard == *expected_guard,
                    "stale migration input cancellation"
                );
                anyhow::bail!("migration input canceled before execution");
            }
            StreamFrame::Unexpected => anyhow::bail!(if expected_role.is_some() {
                "unexpected migration input part frame"
            } else {
                "unexpected migration input frame"
            }),
        }
    }
}

/// Receive one already-described part. The admission callback runs after the
/// bounded BeginPart frame is validated and before the retained String requests
/// its declared capacity. Callers keep every returned part alive through the
/// operation whose typed values borrow or clone its contents.
pub(crate) fn receive_part_admitted<G>(
    input: &mut impl Read,
    expected_guard: &Guard,
    expected: &PartDescriptor,
    until: Instant,
    admit: impl FnOnce(usize) -> Result<G>,
) -> Result<AdmittedPart<G>> {
    ensure!(Instant::now() < until, "migration input deadline");
    let ParentFrame::BeginPart {
        guard,
        role,
        blake3,
        bytes,
    } = read_frame(input)?
    else {
        anyhow::bail!("migration BeginPart required before document input");
    };
    ensure!(
        guard == *expected_guard && role == expected.role,
        "migration input part guard/role differs"
    );
    digest(&blake3)?;
    let maximum = usize::try_from(bytes.0)?;
    ensure!(
        maximum == expected.validate()? && blake3 == expected.blake3,
        "migration input part descriptor differs"
    );
    let mut fixed_digest = [0u8; 64];
    fixed_digest.copy_from_slice(blake3.as_bytes());
    drop(blake3);
    receive_stream_admitted(
        input,
        expected_guard,
        Some(expected.role),
        maximum,
        fixed_digest,
        until,
        admit,
    )
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
    let mut fixed_digest = [0u8; 64];
    fixed_digest.copy_from_slice(request_blake3.as_bytes());
    let received = receive_stream_admitted(
        input,
        &guard,
        None,
        maximum,
        fixed_digest,
        until,
        |_| Ok(()),
    )?;
    Ok(Input {
        guard,
        digest: request_blake3,
        text: received.text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{application::U64, lightroom_migration_worker::protocol::write_frame};
    use std::{io::Cursor, sync::Arc, time::Duration};
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
    fn descriptor(role: InputRole, text: &str) -> PartDescriptor {
        PartDescriptor {
            role,
            bytes: U64(text.len() as u64),
            blake3: blake3::hash(text.as_bytes()).to_hex().to_string(),
        }
    }
    fn encoded_part(role: InputRole, text: &str) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let hash = blake3::hash(text.as_bytes()).to_hex().to_string();
        write_frame(
            &mut bytes,
            &ParentFrame::BeginPart {
                guard: guard(),
                role,
                blake3: hash.clone(),
                bytes: U64(text.len() as u64),
            },
        )?;
        let mut offset = 0usize;
        while offset < text.len() {
            let mut end = (offset + TEXT_CHUNK).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            write_frame(
                &mut bytes,
                &ParentFrame::Part {
                    guard: guard(),
                    role,
                    offset: U64(offset as u64),
                    text: text[offset..end].into(),
                },
            )?;
            offset = end;
        }
        write_frame(
            &mut bytes,
            &ParentFrame::FinishPart {
                guard: guard(),
                role,
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

    #[test]
    fn lm_transport_batch1_multipart_limits_preserve_three_cli_documents_and_raw_authorization()
    -> Result<()> {
        let digest = "a".repeat(64);
        let part = |role, bytes| PartDescriptor {
            role,
            bytes: U64(bytes as u64),
            blake3: digest.clone(),
        };
        let run = [
            part(InputRole::Seal, CLI_DOCUMENT_BYTES),
            part(InputRole::Approval, CLI_DOCUMENT_BYTES),
            part(InputRole::Policy, CLI_DOCUMENT_BYTES),
            part(
                InputRole::ExecutionAuthorization,
                EXECUTION_AUTHORIZATION_BYTES,
            ),
        ];
        assert_eq!(
            DocumentSet::RunWithAuthorization.validate(&run)?,
            3 * CLI_DOCUMENT_BYTES + EXECUTION_AUTHORIZATION_BYTES
        );
        assert_eq!(DocumentSet::None.validate(&[])?, 0);
        assert_eq!(
            DocumentSet::Run.validate(&run[..3])?,
            3 * CLI_DOCUMENT_BYTES
        );
        assert_eq!(
            DocumentSet::Repair.validate(&[
                part(InputRole::Seal, CLI_DOCUMENT_BYTES),
                part(InputRole::Approval, CLI_DOCUMENT_BYTES),
                part(InputRole::RepairRequest, CLI_DOCUMENT_BYTES),
            ])?,
            3 * CLI_DOCUMENT_BYTES
        );
        assert_eq!(
            DocumentSet::PrepareSupplements
                .validate(&[part(InputRole::SupplementRequests, CLI_DOCUMENT_BYTES,)])?,
            CLI_DOCUMENT_BYTES
        );
        assert_eq!(
            InputRole::Operation.maximum_bytes(),
            crate::lightroom::MANIFEST_BYTES
        );
        assert!(DocumentSet::Run.validate(&run).is_err());
        let mut oversized = run.clone();
        oversized[0].bytes.0 += 1;
        assert!(
            DocumentSet::RunWithAuthorization
                .validate(&oversized)
                .unwrap_err()
                .to_string()
                .contains("part byte admission")
        );
        let mut reordered = run.clone();
        reordered.swap(0, 1);
        assert!(
            DocumentSet::RunWithAuthorization
                .validate(&reordered)
                .unwrap_err()
                .to_string()
                .contains("order")
        );
        Ok(())
    }

    #[test]
    fn lm_transport_batch1_multipart_exact_limits_and_raw_bytes_are_admitted_before_retained_copy()
    -> Result<()> {
        let prefix = " \n{\"protocol\":1,\"authorization\":\"\\u0061\\/b\"}\n";
        let mut raw = String::with_capacity(EXECUTION_AUTHORIZATION_BYTES);
        raw.push_str(prefix);
        raw.extend(std::iter::repeat_n(
            ' ',
            EXECUTION_AUTHORIZATION_BYTES - prefix.len(),
        ));
        let expected = descriptor(InputRole::ExecutionAuthorization, &raw);
        let encoded = encoded_part(InputRole::ExecutionAuthorization, &raw)?;
        let admitted_bytes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = admitted_bytes.clone();
        let part = receive_part_admitted(
            &mut encoded.as_slice(),
            &guard(),
            &expected,
            Instant::now() + Duration::from_secs(20),
            move |bytes| {
                observed.store(bytes, std::sync::atomic::Ordering::Release);
                Ok(admitted_bytes)
            },
        )?;
        assert_eq!(
            part.text().as_bytes(),
            raw.as_bytes(),
            "whitespace and alternate escapes changed"
        );
        assert_eq!(part.digest(), expected.blake3);
        assert_eq!(
            part._admission.load(std::sync::atomic::Ordering::Acquire),
            EXECUTION_AUTHORIZATION_BYTES
        );
        drop(part);
        drop(encoded);
        drop(raw);

        let raw = "x".repeat(CLI_DOCUMENT_BYTES);
        let expected = descriptor(InputRole::Policy, &raw);
        let encoded = encoded_part(InputRole::Policy, &raw)?;
        let part = receive_part_admitted(
            &mut encoded.as_slice(),
            &guard(),
            &expected,
            Instant::now() + Duration::from_secs(20),
            |bytes| {
                assert_eq!(bytes, CLI_DOCUMENT_BYTES);
                Ok(())
            },
        )?;
        assert_eq!(part.text().as_bytes(), raw.as_bytes());
        assert_eq!(part.digest(), expected.blake3);
        Ok(())
    }

    #[test]
    fn lm_transport_batch1_multipart_denial_stops_before_first_content_frame() -> Result<()> {
        let raw = "{}";
        let expected = descriptor(InputRole::Policy, raw);
        let begin = {
            let mut bytes = Vec::new();
            write_frame(
                &mut bytes,
                &ParentFrame::BeginPart {
                    guard: guard(),
                    role: expected.role,
                    blake3: expected.blake3.clone(),
                    bytes: expected.bytes,
                },
            )?;
            bytes
        };
        let encoded = encoded_part(InputRole::Policy, raw)?;
        let mut cursor = Cursor::new(encoded);
        let error = receive_part_admitted(
            &mut cursor,
            &guard(),
            &expected,
            Instant::now() + Duration::from_secs(1),
            |_| -> Result<()> { anyhow::bail!("injected admission refusal") },
        )
        .unwrap_err();
        assert!(error.to_string().contains("injected admission refusal"));
        assert_eq!(cursor.position(), begin.len() as u64);
        Ok(())
    }

    #[test]
    fn lm_transport_batch1_multipart_wrong_digest_truncation_and_legacy_crossing_are_rejected()
    -> Result<()> {
        let raw = "{\"qualified\":true}";
        let mut expected = descriptor(InputRole::Approval, raw);
        let encoded = encoded_part(InputRole::Approval, raw)?;
        expected.blake3 = "b".repeat(64);
        let admitted = std::cell::Cell::new(false);
        assert!(
            receive_part_admitted(
                &mut encoded.as_slice(),
                &guard(),
                &expected,
                Instant::now() + Duration::from_secs(1),
                |_| {
                    admitted.set(true);
                    Ok(())
                }
            )
            .is_err()
        );
        assert!(!admitted.get());

        let expected = descriptor(InputRole::Approval, raw);
        let mut truncated = encoded_part(InputRole::Approval, raw)?;
        truncated.pop();
        assert!(
            receive_part_admitted(
                &mut truncated.as_slice(),
                &guard(),
                &expected,
                Instant::now() + Duration::from_secs(1),
                |_| Ok(())
            )
            .is_err()
        );

        let only_part = encoded_part(InputRole::Approval, raw)?;
        assert!(
            receive(
                &mut only_part.as_slice(),
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap_err()
            .to_string()
            .contains("Begin required")
        );
        Ok(())
    }
}
