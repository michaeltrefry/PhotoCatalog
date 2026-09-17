use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const MAX_ANCHORS: usize = 3_002;
pub const MAX_IMPORT_BINDINGS: usize = 4;
pub const MAX_IMPORT_TIMELINE: usize = 1_202;
pub const CLOCK: &str = "macos_mach_absolute_ns";

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct NativeAnchor {
    pub run_id: String,
    pub anchor_id: u32,
    pub session_id: String,
    pub native_pid: u32,
    pub clock: String,
    pub monotonic_ns: String,
}

// This is the same absolute clock/timebase conversion used by CPython 3.12 on macOS.
// No wall time, per-process Instant origin, or floating-point nanoseconds.
#[cfg(target_os = "macos")]
pub fn monotonic_ns() -> Result<u64, String> {
    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_absolute_time() -> u64;
        fn mach_timebase_info(info: *mut Timebase) -> i32;
    }
    let mut info = Timebase { numer: 0, denom: 0 };
    // SAFETY: the kernel writes one initialized-layout Timebase; mach_absolute_time takes no pointers.
    let (result, ticks) = unsafe { (mach_timebase_info(&mut info), mach_absolute_time()) };
    if result != 0 || info.numer == 0 || info.denom == 0 {
        return Err("Mach timebase unavailable".into());
    }
    u64::try_from(u128::from(ticks) * u128::from(info.numer) / u128::from(info.denom))
        .map_err(|_| "Mach clock overflow".into())
}

#[cfg(not(target_os = "macos"))]
pub fn monotonic_ns() -> Result<u64, String> {
    Err("This measurement clock requires macOS".into())
}

pub fn anchor(run_id: &str, id: u32, session: &str) -> Result<NativeAnchor, String> {
    Ok(NativeAnchor {
        run_id: run_id.into(),
        anchor_id: id,
        session_id: session.into(),
        native_pid: std::process::id(),
        clock: CLOCK.into(),
        monotonic_ns: monotonic_ns()?.to_string(),
    })
}

#[derive(Deserialize, Serialize)]
pub struct Anchor {
    anchor_id: u32,
    send_event: u32,
    receive_event: Option<u32>,
    native: Option<NativeAnchor>,
    error: Option<String>,
}
#[derive(Deserialize, Serialize)]
pub struct SampleEvents {
    pub ordinal: u32,
    start_event: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    durable_event: Option<u32>,
    end_event: Option<u32>,
}
#[derive(Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Profile {
    ImportV1,
}
#[derive(Deserialize, Serialize)]
struct ImportBinding {
    key: u8,
    id: String,
    source_blake3: String,
}
#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ImportPhase {
    Discovering,
    Draining,
    Complete,
    CancelRequested,
    Canceled,
    Failed,
}
#[derive(Deserialize, Serialize)]
struct ImportTimeline {
    request_event: u32,
    event: u32,
    binding: u8,
    phase: ImportPhase,
    imported: String,
    unchanged: String,
    failed: String,
    skipped: String,
    metadata_updated: String,
    metadata_warnings: String,
    awaiting_resources: String,
    pending_previews: u32,
}
#[derive(Deserialize, Serialize)]
struct ImportEvidence {
    bindings: Vec<ImportBinding>,
    timeline: Vec<ImportTimeline>,
    overflowed: u32,
}
#[derive(Deserialize, Serialize)]
pub struct Alignment {
    model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<Profile>,
    interval_ms: u32,
    duration_ms: u32,
    anchors: Vec<Anchor>,
    sample_events: Vec<SampleEvents>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    import_evidence: Option<ImportEvidence>,
    stop_reason: String,
}

impl Alignment {
    pub fn import_ids(&self) -> Option<HashSet<&str>> {
        self.import_evidence.as_ref().map(|imports| {
            imports
                .bindings
                .iter()
                .map(|binding| binding.id.as_str())
                .collect()
        })
    }

    pub fn validate(
        &self,
        ordinals: &HashSet<u32>,
        durable_ordinals: &HashSet<u32>,
        import_durable_ordinals: &HashSet<u32>,
        issued: Option<&[NativeAnchor]>,
    ) -> Result<(), String> {
        let bad = || "Invalid causal measurement clock evidence".to_owned();
        let import_profile = matches!(self.profile, Some(Profile::ImportV1));
        let (interval, duration) = if import_profile {
            (200, 600_000)
        } else {
            (100, 300_000)
        };
        if self.model != "causal_native_brackets_v1"
            || self.interval_ms != interval
            || self.duration_ms != duration
            || self.anchors.len() > MAX_ANCHORS
            || self.sample_events.len() != ordinals.len()
            || import_profile != self.import_evidence.is_some()
            || !import_durable_ordinals.is_subset(durable_ordinals)
            || (!import_profile && !import_durable_ordinals.is_empty())
            || ![
                "not_started",
                "duration_elapsed",
                "anchor_limit",
                "anchor_error",
                "finalized",
            ]
            .contains(&self.stop_reason.as_str())
        {
            return Err(bad());
        }
        let mut events = HashSet::new();
        let mut samples = HashSet::new();
        let mut previous_receive = 0;
        let mut previous_native = 0_u64;
        let mut session = None;
        for (index, item) in self.anchors.iter().enumerate() {
            if item.anchor_id as usize != index + 1
                || item.send_event <= previous_receive
                || !events.insert(item.send_event)
            {
                return Err(bad());
            }
            if let Some(receive) = item.receive_event {
                if receive <= item.send_event || !events.insert(receive) {
                    return Err(bad());
                }
                previous_receive = receive;
            } else if index + 1 != self.anchors.len()
                || item.native.is_some()
                || item.error.as_deref() != Some("incomplete")
            {
                return Err(bad());
            }
            if item.error.is_some() && index + 1 != self.anchors.len() {
                return Err(bad());
            }
            if item.error.as_ref().is_some_and(|error| error.len() > 1024) {
                return Err(bad());
            }
            if let Some(native) = &item.native {
                let ns = native.monotonic_ns.parse::<u64>().map_err(|_| bad())?;
                if item.error.is_some()
                    || item.receive_event.is_none()
                    || native.anchor_id != item.anchor_id
                    || native.clock != CLOCK
                    || native.native_pid == 0
                    || uuid::Uuid::parse_str(&native.session_id).is_err()
                    || native.run_id.len() > 64
                    || native.monotonic_ns.len() > 20
                    || !native
                        .monotonic_ns
                        .bytes()
                        .all(|value| value.is_ascii_digit())
                    || ns < previous_native
                    || session.as_ref().is_some_and(|value| {
                        value != &(&native.session_id, native.native_pid, &native.run_id)
                    })
                {
                    return Err(bad());
                }
                if let Some(issued) = issued
                    && issued.get(index) != Some(native)
                {
                    return Err(bad());
                }
                session = Some((&native.session_id, native.native_pid, &native.run_id));
                previous_native = ns;
            } else if item.error.is_none() {
                return Err(bad());
            }
        }
        for sample in &self.sample_events {
            if !ordinals.contains(&sample.ordinal)
                || !samples.insert(sample.ordinal)
                || sample.start_event == 0
                || !events.insert(sample.start_event)
            {
                return Err(bad());
            }
            let end = sample.end_event.ok_or_else(bad)?;
            if end <= sample.start_event || !events.insert(end) {
                return Err(bad());
            }
            if let Some(durable) = sample.durable_event
                && (durable <= sample.start_event
                    || durable > end
                    || !durable_ordinals.contains(&sample.ordinal)
                    || !events.insert(durable))
            {
                return Err(bad());
            }
            if import_profile
                && import_durable_ordinals.contains(&sample.ordinal)
                && sample.durable_event.is_none()
            {
                return Err(bad());
            }
        }
        if let Some(imports) = &self.import_evidence {
            if imports.bindings.is_empty()
                || imports.bindings.len() > MAX_IMPORT_BINDINGS
                || imports.timeline.is_empty()
                || imports.timeline.len() > MAX_IMPORT_TIMELINE
            {
                return Err(bad());
            }
            let mut ids = HashSet::new();
            let mut keys = HashSet::new();
            for (index, binding) in imports.bindings.iter().enumerate() {
                if binding.key as usize != index + 1
                    || !keys.insert(binding.key)
                    || !ids.insert(&binding.id)
                    || uuid::Uuid::parse_str(&binding.id).is_err()
                    || !valid_digest(&binding.source_blake3)
                {
                    return Err(bad());
                }
            }
            let mut used = HashSet::new();
            let mut prior_event = 0;
            let mut prior_counts = std::collections::HashMap::<u8, [u64; 7]>::new();
            let mut prior_phases = std::collections::HashMap::<u8, ImportPhase>::new();
            for row in &imports.timeline {
                let counts = [
                    &row.imported,
                    &row.unchanged,
                    &row.failed,
                    &row.skipped,
                    &row.metadata_updated,
                    &row.metadata_warnings,
                    &row.awaiting_resources,
                ]
                .map(|value| canonical_u64(value).ok_or_else(bad))
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;
                let counts: [u64; 7] = counts.try_into().map_err(|_| bad())?;
                if !keys.contains(&row.binding)
                    || row.request_event <= prior_event
                    || row.event <= row.request_event
                    || !events.insert(row.request_event)
                    || !events.insert(row.event)
                    || prior_counts.get(&row.binding).is_some_and(|prior| {
                        prior[..6]
                            .iter()
                            .zip(counts[..6].iter())
                            .any(|(left, right)| left > right)
                    })
                    || prior_phases
                        .get(&row.binding)
                        .is_some_and(|prior| !valid_phase_transition(*prior, row.phase))
                {
                    return Err(bad());
                }
                prior_event = row.event;
                used.insert(row.binding);
                prior_counts.insert(row.binding, counts);
                prior_phases.insert(row.binding, row.phase);
                let _ = row.pending_previews;
            }
            let _ = imports.overflowed;
            if used != keys {
                return Err(bad());
            }
        }
        if events
            .iter()
            .any(|event| *event as usize > MAX_ANCHORS * 2 + 512 * 3 + MAX_IMPORT_TIMELINE * 2)
        {
            return Err(bad());
        }
        Ok(())
    }
}

fn valid_phase_transition(from: ImportPhase, to: ImportPhase) -> bool {
    match from {
        ImportPhase::Discovering => matches!(
            to,
            ImportPhase::Discovering
                | ImportPhase::Draining
                | ImportPhase::Complete
                | ImportPhase::CancelRequested
                | ImportPhase::Canceled
                | ImportPhase::Failed
        ),
        ImportPhase::Draining => matches!(
            to,
            ImportPhase::Draining
                | ImportPhase::Complete
                | ImportPhase::CancelRequested
                | ImportPhase::Canceled
                | ImportPhase::Failed
        ),
        ImportPhase::CancelRequested => matches!(
            to,
            ImportPhase::CancelRequested | ImportPhase::Canceled | ImportPhase::Failed
        ),
        ImportPhase::Complete => matches!(to, ImportPhase::Complete),
        ImportPhase::Canceled => matches!(to, ImportPhase::Canceled),
        ImportPhase::Failed => matches!(to, ImportPhase::Failed),
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|value| value.is_ascii_digit() || (b'a'..=b'f').contains(&value))
}

fn canonical_u64(value: &str) -> Option<u64> {
    let parsed = value.parse::<u64>().ok()?;
    (parsed.to_string() == value).then_some(parsed)
}

/// Explicit opt-in, bounded stdin/stdout diagnostic; it never starts a GUI or catalog.
pub fn diagnostic() -> Result<(), String> {
    use std::io::{BufRead, Write};
    let session = uuid::Uuid::new_v4().to_string();
    let mut input = std::io::stdin().lock();
    for id in 1..=16 {
        let mut line = Vec::new();
        // read_until would permit unbounded input; fixed-size token reads do not.
        use std::io::Read;
        let count = (&mut input)
            .take(7)
            .read_until(b'\n', &mut line)
            .map_err(|e| e.to_string())?;
        if count == 0 {
            return Ok(());
        }
        if line != b"anchor\n" {
            return Err("Expected bounded anchor token".into());
        }
        let value = anchor("conformance", id, &session)?;
        println!(
            "{}",
            serde_json::to_string(&value).map_err(|e| e.to_string())?
        );
        std::io::stdout().flush().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_binds_issued_anchors_and_rejects_reordered_or_missing_events() {
        let native = NativeAnchor {
            run_id: "run".into(),
            anchor_id: 1,
            session_id: "ffffffff-ffff-ffff-ffff-ffffffffffff".into(),
            native_pid: 42,
            clock: CLOCK.into(),
            monotonic_ns: "9007199254741001".into(),
        };
        let mut alignment: Alignment = serde_json::from_value(serde_json::json!({
            "model":"causal_native_brackets_v1", "interval_ms":100,"duration_ms":300000,"stop_reason":"finalized",
            "anchors":[{"anchor_id":1,"send_event":1,"receive_event":2,"native":native,"error":null}],
            "sample_events":[{"ordinal":1,"start_event":3,"end_event":4}]
        })).unwrap();
        let ordinals = HashSet::from([1]);
        let durable = HashSet::new();
        alignment
            .validate(
                &ordinals,
                &durable,
                &durable,
                Some(std::slice::from_ref(&native)),
            )
            .unwrap();
        let encoded = serde_json::to_value(&alignment).unwrap();
        assert!(encoded.get("profile").is_none());
        assert!(encoded.get("import_evidence").is_none());
        assert!(encoded["sample_events"][0].get("durable_event").is_none());
        assert!(
            alignment
                .validate(&ordinals, &durable, &durable, Some(&[]))
                .is_err()
        );
        alignment.sample_events[0].end_event = Some(2);
        assert!(
            alignment
                .validate(&ordinals, &durable, &durable, None)
                .is_err()
        );
        alignment.sample_events[0].end_event = None;
        assert!(
            alignment
                .validate(&ordinals, &durable, &durable, None)
                .is_err()
        );
        alignment.sample_events[0].end_event = Some(4);
        alignment.anchors[0].native.as_mut().unwrap().native_pid = 43;
        assert!(
            alignment
                .validate(&ordinals, &durable, &durable, Some(&[native]))
                .is_err()
        );
    }

    #[test]
    fn import_profile_rejects_missing_durable_event_noncanonical_counts_and_phase_regression() {
        let mut value = serde_json::json!({
            "model":"causal_native_brackets_v1", "profile":"import_v1", "interval_ms":200,
            "duration_ms":600000, "stop_reason":"finalized", "anchors":[],
            "sample_events":[{"ordinal":1,"start_event":3,"durable_event":4,"end_event":5}],
            "import_evidence":{"bindings":[{"key":1,"id":"00000000-0000-4000-8000-000000000001","source_blake3":"a".repeat(64)}],
                "timeline":[
                    {"request_event":1,"event":2,"binding":1,"phase":"discovering","imported":"0","unchanged":"0","failed":"0","skipped":"0","metadata_updated":"0","metadata_warnings":"0","awaiting_resources":"2","pending_previews":0},
                    {"request_event":6,"event":7,"binding":1,"phase":"complete","imported":"1","unchanged":"0","failed":"0","skipped":"0","metadata_updated":"1","metadata_warnings":"0","awaiting_resources":"0","pending_previews":0}
                ],"overflowed":0}
        });
        let ordinals = HashSet::from([1]);
        let durable = HashSet::from([1]);
        let alignment: Alignment = serde_json::from_value(value.clone()).unwrap();
        alignment
            .validate(&ordinals, &durable, &durable, None)
            .unwrap();
        value["sample_events"][0]["durable_event"] = serde_json::Value::Null;
        let alignment: Alignment = serde_json::from_value(value.clone()).unwrap();
        assert!(
            alignment
                .validate(&ordinals, &durable, &durable, None)
                .is_err()
        );
        value["sample_events"][0]["durable_event"] = serde_json::json!(4);
        value["import_evidence"]["timeline"][1]["imported"] = serde_json::json!("01");
        let alignment: Alignment = serde_json::from_value(value.clone()).unwrap();
        assert!(
            alignment
                .validate(&ordinals, &durable, &durable, None)
                .is_err()
        );
        value["import_evidence"]["timeline"][1]["imported"] = serde_json::json!("1");
        value["import_evidence"]["timeline"][0]["imported"] = serde_json::json!("2");
        let alignment: Alignment = serde_json::from_value(value.clone()).unwrap();
        assert!(
            alignment
                .validate(&ordinals, &durable, &durable, None)
                .is_err()
        );
        value["import_evidence"]["timeline"][0]["imported"] = serde_json::json!("0");
        value["import_evidence"]["timeline"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "request_event":8,"event":9,"binding":1,"phase":"discovering","imported":"1","unchanged":"0","failed":"0","skipped":"0",
                "metadata_updated":"1","metadata_warnings":"0","awaiting_resources":"0","pending_previews":0
            }));
        let alignment: Alignment = serde_json::from_value(value).unwrap();
        assert!(
            alignment
                .validate(&ordinals, &durable, &durable, None)
                .is_err()
        );
    }
}
