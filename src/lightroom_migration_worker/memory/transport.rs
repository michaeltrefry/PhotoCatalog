//! Closed managed transport owners, excluding Source result/Authority assembly
//! and core graphs. G holds this reservation until LM and both Sources, pipe
//! threads, relay payloads and broker have all drained. A frame limit bounds
//! malformed typed input before semantic validation; it is not a decoded cap.
use super::layout::{add, content_containers, mul, vector};
use crate::{lightroom::plan::Cell, lightroom_migration_worker::protocol::FRAME_BYTES};
use anyhow::Result;

#[derive(Debug)]
pub(crate) struct Payloads {
    pub(crate) typed: usize,
    pub(crate) parser: usize,
    pub(crate) encoded: usize,
    pub(crate) diagnostics: usize,
}
impl Payloads {
    pub(crate) fn total(&self) -> Result<usize> {
        add(
            add(self.typed, self.parser)?,
            add(self.encoded, self.diagnostics)?,
        )
    }
}

/// One flat frame can own String payloads and u8/u16 native or opaque units.
/// Input bytes bound their total units; direct/buffered Vec growth contributes
/// at most four bytes per input byte plus minima, and strings one more. Only
/// Source Request's Page cursor additionally owns a Vec<Cell>.
fn flat_graph() -> Result<usize> {
    add(mul(5, FRAME_BYTES)?, 32)
}
fn request_graph() -> Result<usize> {
    add(flat_graph()?, vector::<Cell>(FRAME_BYTES)?)
}

/// Four encoded owners per Process: ordinary queued, urgent queued, writer
/// current, and an attempted send. Each framed Vec owns at most F+4 (or the
/// smaller minimum8). bounded_json's growing scratch contributes another2F.
fn process_encoded() -> Result<usize> {
    add(mul(6, FRAME_BYTES)?, 16)
}

pub(crate) fn payloads(managed_sources: bool) -> Result<Payloads> {
    let flat = flat_graph()?;
    // Main transport: Parent frames held by G data/control + LM input/listener
    // (4); Child frames in G reader/queue/consumer + two publishing owners (5).
    let mut typed = mul(9, flat)?;
    // Parent/Child each have at most two buffered enum vocabulary layers.
    let mut layers = 4;
    let mut decoders = 2;
    let mut encoded = add(process_encoded()?, mul(2, FRAME_BYTES)?)?;
    let mut diagnostic_owners = 2;
    if managed_sources {
        // Requests: two G pending/current slots, six Source listener/pending/
        // executing owners, and LM's current Request plus Read clone. Replies:
        // four G queued/reader-current + broker-current + two LM slots + LM
        // consumer and listener-current (9), plus each Source executor-local
        // Reply while stdout serializes it (2). Broker Command queued2/current/
        // G-held(4), Event queued2/pending/urgent2/G-data-control(7).
        typed = add(typed, add(mul(10, request_graph()?)?, mul(22, flat)?)?)?;
        // Request decoders: G broker1 + Source listeners2, four vocabulary
        // layers each. Reply decoders: G readers2 + LM relay listener1, one
        // layer each. All can coexist with the two outer-frame decoders.
        layers += 15;
        decoders += 6;
        // Two additional Processes and Source stdout serializers. G and LM
        // each retain incoming/outgoing frames for both roles (eight F+4).
        // LM can rebuild one retry while its Sending persists; its scratch
        // and the broker's outgoing serializer coexist with those owners.
        encoded = add(
            encoded,
            mul(2, add(process_encoded()?, mul(2, FRAME_BYTES)?)?)?,
        )?;
        encoded = add(encoded, mul(8, add(FRAME_BYTES, 4)?)?)?;
        encoded = add(encoded, add(mul(5, FRAME_BYTES)?, 4)?)?;
        // Two Source Controls retain full listener formatting, and two LM
        // slots may retain bounded failure details. These are additional to
        // errors under construction at each concurrently active decoder.
        diagnostic_owners += 4;
    }
    // Source frame enum errors use the same bounded lexical alphabet as the
    // reviewed error family: raw/scratch/custom-format overlap <=32F+8F.
    // Content container backing is separate, including unknown fields and
    // body-before-tag representations; owning subtrees move across layers.
    let parser = add(
        content_containers(FRAME_BYTES, layers)?,
        mul(decoders, mul(40, FRAME_BYTES)?)?,
    )?;
    // Full listener error strings and a formatting/copy replacement retain at
    // most the same conservative error-family allowance per independent owner.
    // OS process/pipe errors and runtime backtraces remain the explicit runtime
    // baseline, not a claim that an OS diagnostic fits a Source frame.
    let diagnostics = mul(diagnostic_owners, mul(40, FRAME_BYTES)?)?;
    Ok(Payloads {
        typed,
        parser,
        encoded,
        diagnostics,
    })
}
