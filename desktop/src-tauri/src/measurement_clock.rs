use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const MAX_ANCHORS: usize = 3_002;
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
    end_event: Option<u32>,
}
#[derive(Deserialize, Serialize)]
pub struct Alignment {
    model: String,
    interval_ms: u32,
    duration_ms: u32,
    anchors: Vec<Anchor>,
    sample_events: Vec<SampleEvents>,
    stop_reason: String,
}

impl Alignment {
    pub fn validate(
        &self,
        ordinals: &HashSet<u32>,
        issued: Option<&[NativeAnchor]>,
    ) -> Result<(), String> {
        let bad = || "Invalid causal measurement clock evidence".to_owned();
        if self.model != "causal_native_brackets_v1"
            || self.interval_ms != 100
            || self.duration_ms != 300_000
            || self.anchors.len() > MAX_ANCHORS
            || self.sample_events.len() != ordinals.len()
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
        }
        if events
            .iter()
            .any(|event| *event as usize > MAX_ANCHORS * 2 + 512 * 2)
        {
            return Err(bad());
        }
        Ok(())
    }
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
        alignment
            .validate(&ordinals, Some(std::slice::from_ref(&native)))
            .unwrap();
        assert!(alignment.validate(&ordinals, Some(&[])).is_err());
        alignment.sample_events[0].end_event = Some(2);
        assert!(alignment.validate(&ordinals, None).is_err());
        alignment.sample_events[0].end_event = None;
        assert!(alignment.validate(&ordinals, None).is_err());
        alignment.sample_events[0].end_event = Some(4);
        alignment.anchors[0].native.as_mut().unwrap().native_pid = 43;
        assert!(alignment.validate(&ordinals, Some(&[native])).is_err());
    }
}
