use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};

const MAX_SAMPLES: usize = 512;
const MAX_RECEIPT_BYTES: usize = 2 * 1024 * 1024;
const MAX_DURATION_US: u64 = 30 * 60 * 1_000_000;
const MAX_STARTED_US: u64 = 24 * 60 * 60 * 1_000_000;
const MAX_THUMBNAIL_EVENTS: u32 = 1_000_000;
const MAX_SCROLL_FRAMES: usize = 2_048;
const SCROLL_DURATION_US: u64 = 5_000_000;
const MAX_SCROLL_DURATION_US: u64 = 10_000_000;
const MAX_SCROLL_EDGE_GAP_US: u64 = 100_000;
const MAX_PREVIEW_DIAGNOSTICS: usize = 128;
const MAX_PREVIEW_DIAGNOSTIC_BYTES: usize = 16 * 1024;
const TIMING_TOLERANCE_MS: f64 = 0.001;

struct Target {
    run_id: String,
    receipt_path: PathBuf,
    finalized: bool,
    preview_diagnostics: usize,
    clock_session: String,
    clock_anchors: Vec<crate::measurement_clock::NativeAnchor>,
}

pub struct State(Mutex<Option<Target>>);

impl State {
    pub fn new(run_id: Option<String>, cache_root: PathBuf) -> Self {
        Self(Mutex::new(run_id.map(|run_id| {
            Target {
                receipt_path: cache_root
                    .join("s12-measurements")
                    .join(format!("{run_id}.json")),
                run_id,
                finalized: false,
                preview_diagnostics: 0,
                clock_session: uuid::Uuid::new_v4().to_string(),
                clock_anchors: Vec::new(),
            }
        })))
    }

    fn config(&self) -> Result<Config, String> {
        let target = self.0.lock().map_err(|_| "Measurement state unavailable")?;
        Ok(Config {
            enabled: target.is_some(),
            run_id: target.as_ref().map(|target| target.run_id.clone()),
            max_samples: MAX_SAMPLES,
        })
    }

    fn finish(&self, receipt: Receipt) -> Result<String, String> {
        let mut target = self.0.lock().map_err(|_| "Measurement state unavailable")?;
        let target = target.as_mut().ok_or("S12 measurement is not enabled")?;
        validate(&receipt, &target.run_id)?;
        if let Some(alignment) = &receipt.clock_alignment {
            let durable: HashSet<_> = receipt
                .samples
                .iter()
                .filter_map(|sample| sample.durable_us.map(|_| sample.ordinal))
                .collect();
            let import_durable: HashSet<_> = receipt
                .samples
                .iter()
                .filter_map(|sample| {
                    (sample.durable_us.is_some() && sample.import_id.is_some())
                        .then_some(sample.ordinal)
                })
                .collect();
            alignment.validate(
                &receipt
                    .samples
                    .iter()
                    .map(|sample| sample.ordinal)
                    .collect(),
                &durable,
                &import_durable,
                Some(&target.clock_anchors),
            )?;
        }
        let bytes = serde_json::to_vec(&receipt).map_err(|_| "Measurement receipt is invalid")?;
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err("Measurement receipt exceeds its byte limit".into());
        }
        if target.finalized {
            return exact_existing_receipt(&target.receipt_path, &bytes);
        }
        let parent = target
            .receipt_path
            .parent()
            .ok_or("Measurement receipt path is invalid")?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("Create measurement directory: {error}"))?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target.receipt_path);
        let mut file = match file {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let path = exact_existing_receipt(&target.receipt_path, &bytes)?;
                target.finalized = true;
                return Ok(path);
            }
            Err(error) => return Err(format!("Create measurement receipt: {error}")),
        };
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("Write measurement receipt: {error}"))?;
        target.finalized = true;
        Ok(target.receipt_path.to_string_lossy().into_owned())
    }

    fn preview_diagnostic(&self, diagnostic: FrontendPreviewDiagnostic) -> Result<bool, String> {
        diagnostic.validate()?;
        let mut target = self.0.lock().map_err(|_| "Measurement state unavailable")?;
        let target = target.as_mut().ok_or("S12 measurement is not enabled")?;
        if target.preview_diagnostics >= MAX_PREVIEW_DIAGNOSTICS {
            return Ok(false);
        }
        let line = serde_json::to_string(&PreviewDiagnosticLine {
            run_id: &target.run_id,
            diagnostic: &diagnostic,
        })
        .map_err(|_| "Preview diagnostic is invalid")?;
        if line.len() > MAX_PREVIEW_DIAGNOSTIC_BYTES {
            return Err("Preview diagnostic exceeds its byte limit".into());
        }
        eprintln!("S12_PREVIEW_DIAGNOSTIC {line}");
        target.preview_diagnostics += 1;
        Ok(true)
    }
}

#[derive(Deserialize, Serialize)]
pub struct FrontendPreviewDiagnostic {
    ticket: String,
    admission_command_ms: f64,
    ready_observed_ms: f64,
    blob_invoke_ms: f64,
    object_url_ms: f64,
    polls: u16,
    native: photocatalog::application::PreviewDiagnostic,
}

#[derive(Serialize)]
struct PreviewDiagnosticLine<'a> {
    run_id: &'a str,
    diagnostic: &'a FrontendPreviewDiagnostic,
}

impl FrontendPreviewDiagnostic {
    fn validate(&self) -> Result<(), String> {
        uuid::Uuid::parse_str(&self.ticket).map_err(|_| "Invalid preview diagnostic ticket")?;
        if self.polls > 10_000
            || [
                self.admission_command_ms,
                self.ready_observed_ms,
                self.blob_invoke_ms,
                self.object_url_ms,
            ]
            .into_iter()
            .any(|value| !value.is_finite() || !(0.0..=1_800_000.0).contains(&value))
        {
            return Err("Preview diagnostic timing is invalid".into());
        }
        for digest in [
            self.native.expected_key_digest.as_deref(),
            self.native.selected_key_digest.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if digest.len() != 64 || !digest.bytes().all(|value| value.is_ascii_hexdigit()) {
                return Err("Preview diagnostic digest is invalid".into());
            }
        }
        let expected = self.native.expected_key_digest.as_deref();
        let selected = self.native.selected_key_digest.as_deref();
        if match (expected, selected) {
            (Some(expected), Some(selected)) => {
                self.native.current_key_matches_selected != Some(expected == selected)
            }
            _ => self.native.current_key_matches_selected.is_some(),
        } {
            return Err("Preview diagnostic key comparison is invalid".into());
        }
        if let Some(delivery) = &self.native.delivery
            && (delivery.total_ms + TIMING_TOLERANCE_MS < delivery.ready_for_transfer_ms
                || delivery.total_ms + TIMING_TOLERANCE_MS < delivery.transfer_ms)
        {
            return Err("Preview diagnostic delivery timing is inconsistent".into());
        }
        let mut native_times = Vec::new();
        if let Some(value) = self.native.original_render_ms {
            native_times.push(value);
        }
        if let Some(value) = self.native.ready_ms {
            native_times.push(value);
        }
        let mut reads = Vec::new();
        if let Some(read) = &self.native.retained_read {
            reads.push(read);
        }
        if let Some(delivery) = &self.native.delivery {
            native_times.extend([
                delivery.ready_for_transfer_ms,
                delivery.transfer_ms,
                delivery.total_ms,
            ]);
            reads.push(&delivery.retained_read);
        }
        for read in reads {
            if !matches!(
                read.outcome.as_str(),
                "ready" | "missing" | "stale" | "failed"
            ) {
                return Err("Preview diagnostic read outcome is invalid".into());
            }
            native_times.extend([
                read.queue_ms,
                read.owner_read_ms,
                read.catalog_identity_ms,
                read.store_read_checksum_ms,
                read.header_decode_ms,
                read.total_ms,
            ]);
            if read.decoded_hits > 1 || read.decoded_misses > 1 {
                return Err("Preview diagnostic decode count is invalid".into());
            }
            if [
                read.catalog_identity_ms,
                read.store_read_checksum_ms,
                read.header_decode_ms,
            ]
            .into_iter()
            .any(|phase| read.total_ms + TIMING_TOLERANCE_MS < phase)
            {
                return Err("Preview diagnostic read timing is inconsistent".into());
            }
        }
        if native_times
            .into_iter()
            .any(|value| !value.is_finite() || !(0.0..=1_800_000.0).contains(&value))
        {
            return Err("Preview diagnostic native timing is invalid".into());
        }
        let bytes = serde_json::to_vec(self).map_err(|_| "Preview diagnostic is invalid")?;
        if bytes.len() > MAX_PREVIEW_DIAGNOSTIC_BYTES {
            return Err("Preview diagnostic exceeds its byte limit".into());
        }
        Ok(())
    }
}

fn exact_existing_receipt(path: &Path, expected: &[u8]) -> Result<String, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Inspect existing measurement receipt: {error}"))?;
    if metadata.len() > MAX_RECEIPT_BYTES as u64 {
        return Err("Existing measurement receipt exceeds its byte limit".into());
    }
    let existing =
        fs::read(path).map_err(|error| format!("Read existing measurement receipt: {error}"))?;
    if existing != expected {
        return Err("Existing measurement receipt does not match this run".into());
    }
    Ok(path.to_string_lossy().into_owned())
}

#[derive(Serialize)]
pub struct Config {
    enabled: bool,
    run_id: Option<String>,
    max_samples: usize,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Cull,
    Edit,
    Browse,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Complete,
    BackendError,
    PresentationMismatch,
    Superseded,
    Canceled,
    Incomplete,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PresentationModel {
    TwoAnimationFrames,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextModel {
    LastObservedStatusAtStart,
}

#[derive(Deserialize, Serialize)]
pub struct Sample {
    kind: Kind,
    ordinal: u32,
    started_us: u64,
    during_import: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    import_id: Option<String>,
    during_export: bool,
    outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    durable_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    presentation_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    search_response_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_thumbnail_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    visible_complete_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_rows: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    visible_count: Option<u16>,
}

#[derive(Deserialize, Serialize)]
pub struct Receipt {
    protocol: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    clock_alignment: Option<crate::measurement_clock::Alignment>,
    presentation_model: PresentationModel,
    context_model: ContextModel,
    run_id: String,
    time_origin_ms: f64,
    overflowed: u32,
    thumbnail_diagnostics: ThumbnailDiagnostics,
    samples: Vec<Sample>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scroll_capture: Option<ScrollCapture>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollOutcome {
    Complete,
    Incomplete,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollReason {
    DurationElapsed,
    ManualStop,
    Finalized,
    Unmounted,
    TargetChanged,
    FrameLimit,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollFrameModel {
    RequestAnimationFrameTimestampScrollPosition,
}

#[derive(Deserialize, Serialize)]
pub struct ScrollSnapshot {
    identity: u32,
    scroll_top_px: u32,
    scroll_left_px: u32,
    viewport_width_px: u32,
    viewport_height_px: u32,
    scroll_width_px: u32,
    scroll_height_px: u32,
}

#[derive(Deserialize, Serialize)]
pub struct ScrollCapture {
    frame_model: ScrollFrameModel,
    outcome: ScrollOutcome,
    reason: ScrollReason,
    started_us: u64,
    ended_us: u64,
    target_initial: ScrollSnapshot,
    target_final: Option<ScrollSnapshot>,
    frames: Vec<(u64, u32, u32)>,
}

#[derive(Default, Deserialize, Serialize)]
pub struct ThumbnailDiagnostics {
    attempts: u32,
    decode_completed: u32,
    decode_failed: u32,
    source_changed: u32,
    disconnected: u32,
    incomplete: u32,
    zero_size: u32,
    nonvisible: u32,
    accepted: u32,
    roster_tiles: u32,
    pending_expected: u32,
}

pub fn run_id() -> Result<Option<String>, String> {
    parse_run_id(std::env::args_os().skip(1))
}

fn parse_run_id(arguments: impl IntoIterator<Item = OsString>) -> Result<Option<String>, String> {
    let mut run_id = None;
    for argument in arguments {
        let argument = argument.to_string_lossy();
        let Some(value) = argument.strip_prefix("--s12-measure=") else {
            continue;
        };
        if run_id.is_some() {
            return Err("S12 measurement may be enabled only once".into());
        }
        if value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .all(|value| value.is_ascii_alphanumeric() || value == b'-' || value == b'_')
        {
            return Err("Invalid S12 measurement run identifier".into());
        }
        run_id = Some(value.to_owned());
    }
    Ok(run_id)
}

#[tauri::command]
pub fn catalog_measurement_clock_anchor(
    state: tauri::State<'_, State>,
    run_id: String,
    anchor_id: u32,
) -> Result<crate::measurement_clock::NativeAnchor, String> {
    let mut state = state
        .0
        .lock()
        .map_err(|_| "Measurement state unavailable")?;
    let target = state.as_mut().ok_or("S12 measurement is not enabled")?;
    if target.finalized
        || target.run_id != run_id
        || target.clock_anchors.len() >= crate::measurement_clock::MAX_ANCHORS
        || anchor_id as usize != target.clock_anchors.len() + 1
    {
        return Err("Clock anchor request is invalid".into());
    }
    let anchor = crate::measurement_clock::anchor(&run_id, anchor_id, &target.clock_session)?;
    target.clock_anchors.push(anchor.clone());
    Ok(anchor)
}

#[tauri::command]
pub fn catalog_measurement_config(state: tauri::State<'_, State>) -> Result<Config, String> {
    state.config()
}

#[tauri::command]
pub fn catalog_measurement_finish(
    state: tauri::State<'_, State>,
    receipt: Receipt,
) -> Result<String, String> {
    state.finish(receipt)
}

#[tauri::command]
pub fn catalog_measurement_preview_diagnostic(
    state: tauri::State<'_, State>,
    diagnostic: FrontendPreviewDiagnostic,
) -> Result<bool, String> {
    state.preview_diagnostic(diagnostic)
}

fn validate(receipt: &Receipt, expected_run_id: &str) -> Result<(), String> {
    if !matches!(receipt.protocol, 1 | 2)
        || (receipt.protocol == 2) != receipt.clock_alignment.is_some()
        || !matches!(
            receipt.presentation_model,
            PresentationModel::TwoAnimationFrames
        )
        || !matches!(
            receipt.context_model,
            ContextModel::LastObservedStatusAtStart
        )
        || receipt.run_id != expected_run_id
        || !receipt.time_origin_ms.is_finite()
        || receipt.time_origin_ms < 0.0
        || receipt.time_origin_ms > 10_000_000_000_000.0
        || receipt.samples.len() > MAX_SAMPLES
        || [
            receipt.thumbnail_diagnostics.attempts,
            receipt.thumbnail_diagnostics.decode_completed,
            receipt.thumbnail_diagnostics.decode_failed,
            receipt.thumbnail_diagnostics.source_changed,
            receipt.thumbnail_diagnostics.disconnected,
            receipt.thumbnail_diagnostics.incomplete,
            receipt.thumbnail_diagnostics.zero_size,
            receipt.thumbnail_diagnostics.nonvisible,
            receipt.thumbnail_diagnostics.accepted,
            receipt.thumbnail_diagnostics.roster_tiles,
            receipt.thumbnail_diagnostics.pending_expected,
        ]
        .into_iter()
        .any(|value| value > MAX_THUMBNAIL_EVENTS)
    {
        return Err("Measurement receipt header is invalid".into());
    }
    let mut ordinals = HashSet::with_capacity(receipt.samples.len());
    for sample in &receipt.samples {
        if sample.ordinal == 0
            || sample.ordinal as usize > MAX_SAMPLES
            || sample.started_us > MAX_STARTED_US
            || !ordinals.insert(sample.ordinal)
            || sample
                .import_id
                .as_ref()
                .is_some_and(|value| !sample.during_import || uuid::Uuid::parse_str(value).is_err())
        {
            return Err("Measurement receipt ordinals are invalid".into());
        }
        let durations = [
            sample.durable_us,
            sample.presentation_us,
            sample.search_response_us,
            sample.first_thumbnail_us,
            sample.visible_complete_us,
        ];
        if durations
            .into_iter()
            .flatten()
            .any(|value| value > MAX_DURATION_US)
        {
            return Err("Measurement duration exceeds its limit".into());
        }
        if sample.page_rows.is_some_and(|value| value > 100)
            || sample.visible_count.is_some_and(|value| value > 100)
        {
            return Err("Measurement count exceeds its limit".into());
        }
        let valid = match sample.kind {
            Kind::Cull | Kind::Edit => {
                sample.search_response_us.is_none()
                    && sample.first_thumbnail_us.is_none()
                    && sample.visible_complete_us.is_none()
                    && sample.page_rows.is_none()
                    && sample.visible_count.is_none()
                    && match sample.outcome {
                        Outcome::Complete | Outcome::PresentationMismatch => {
                            matches!(
                                (sample.durable_us, sample.presentation_us),
                                (Some(durable), Some(presentation)) if durable <= presentation
                            )
                        }
                        Outcome::Superseded => {
                            matches!(sample.kind, Kind::Edit)
                                && sample.durable_us.is_none()
                                && sample.presentation_us.is_none()
                        }
                        Outcome::BackendError | Outcome::Canceled => {
                            sample.durable_us.is_none() && sample.presentation_us.is_none()
                        }
                        Outcome::Incomplete => sample.presentation_us.is_none(),
                    }
            }
            Kind::Browse => {
                sample.durable_us.is_none()
                    && sample.presentation_us.is_none()
                    && !matches!(
                        sample.outcome,
                        Outcome::PresentationMismatch | Outcome::Superseded
                    )
                    && (if matches!(sample.outcome, Outcome::Complete) {
                        matches!(sample.page_rows, Some(1..=100))
                            && matches!(sample.visible_count, Some(1..=100))
                            && sample.visible_count <= sample.page_rows
                            && matches!(
                                (
                                    sample.search_response_us,
                                    sample.first_thumbnail_us,
                                    sample.visible_complete_us,
                                ),
                                (Some(response), Some(first), Some(complete))
                                    if response <= first && first <= complete
                            )
                    } else {
                        true
                    })
            }
        };
        if !valid {
            return Err("Measurement sample fields are invalid".into());
        }
    }
    if let Some(alignment) = &receipt.clock_alignment {
        let durable = receipt
            .samples
            .iter()
            .filter_map(|sample| sample.durable_us.map(|_| sample.ordinal))
            .collect();
        let import_durable = receipt
            .samples
            .iter()
            .filter_map(|sample| {
                (sample.durable_us.is_some() && sample.import_id.is_some())
                    .then_some(sample.ordinal)
            })
            .collect();
        alignment.validate(&ordinals, &durable, &import_durable, None)?;
        if let Some(ids) = alignment.import_ids()
            && receipt.samples.iter().any(|sample| {
                sample.during_import
                    != sample
                        .import_id
                        .as_deref()
                        .is_some_and(|id| ids.contains(id))
            })
        {
            return Err("Measurement import identity is invalid".into());
        }
        if alignment.import_ids().is_none()
            && receipt
                .samples
                .iter()
                .any(|sample| sample.import_id.is_some())
        {
            return Err("Measurement import identity is invalid".into());
        }
    } else if receipt
        .samples
        .iter()
        .any(|sample| sample.import_id.is_some())
    {
        return Err("Measurement import identity is invalid".into());
    }
    if let Some(capture) = &receipt.scroll_capture {
        let duration = capture
            .ended_us
            .checked_sub(capture.started_us)
            .ok_or("Scroll capture bounds are invalid")?;
        if capture.started_us > MAX_STARTED_US
            || duration > MAX_DURATION_US
            || capture.frames.len() > MAX_SCROLL_FRAMES
            || !valid_scroll_snapshot(&capture.target_initial)
            || !scrollable_snapshot(&capture.target_initial)
            || capture
                .target_final
                .as_ref()
                .is_some_and(|value| !valid_scroll_snapshot(value))
            || capture.frames.windows(2).any(|pair| pair[0].0 > pair[1].0)
            || capture.frames.iter().any(|(timestamp, _, _)| {
                *timestamp < capture.started_us.saturating_sub(MAX_SCROLL_EDGE_GAP_US)
                    || *timestamp > capture.ended_us
            })
        {
            return Err("Scroll capture fields are invalid".into());
        }
        let valid_outcome = match capture.outcome {
            ScrollOutcome::Complete => {
                matches!(capture.reason, ScrollReason::DurationElapsed)
                    && (SCROLL_DURATION_US..=MAX_SCROLL_DURATION_US).contains(&duration)
                    && capture.frames.len() >= 2
                    && capture.frames[0].0.abs_diff(capture.started_us) <= MAX_SCROLL_EDGE_GAP_US
                    && capture.ended_us - capture.frames.last().unwrap().0 <= MAX_SCROLL_EDGE_GAP_US
                    && capture.target_final.as_ref().is_some_and(|final_target| {
                        final_target.identity == capture.target_initial.identity
                    })
            }
            ScrollOutcome::Incomplete => true,
        };
        if !valid_outcome {
            return Err("Scroll capture outcome is invalid".into());
        }
    }
    Ok(())
}

fn valid_scroll_snapshot(value: &ScrollSnapshot) -> bool {
    value.identity > 0
        && value.viewport_width_px > 0
        && value.viewport_height_px > 0
        && value.scroll_width_px >= value.viewport_width_px
        && value.scroll_height_px >= value.viewport_height_px
        && value.scroll_left_px <= value.scroll_width_px
        && value.scroll_top_px <= value.scroll_height_px
}

fn scrollable_snapshot(value: &ScrollSnapshot) -> bool {
    value.scroll_width_px > value.viewport_width_px
        || value.scroll_height_px > value.viewport_height_px
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview_diagnostic() -> FrontendPreviewDiagnostic {
        let read = photocatalog::application::PreviewReadDiagnostic {
            outcome: "ready".into(),
            queue_ms: 1.0,
            owner_read_ms: 2.0,
            catalog_identity_ms: 0.1,
            store_read_checksum_ms: 0.5,
            header_decode_ms: 1.4,
            total_ms: 2.0,
            decoded_hits: 0,
            decoded_misses: 1,
        };
        FrontendPreviewDiagnostic {
            ticket: "00000000-0000-4000-8000-000000000001".into(),
            admission_command_ms: 1.0,
            ready_observed_ms: 3.0,
            blob_invoke_ms: 2.0,
            object_url_ms: 0.1,
            polls: 1,
            native: photocatalog::application::PreviewDiagnostic {
                route: photocatalog::application::PreviewRoute::Retained,
                expected_key_digest: Some("a".repeat(64)),
                selected_key_digest: Some("a".repeat(64)),
                current_key_matches_selected: Some(true),
                retained_read: Some(read.clone()),
                original_render_ms: None,
                ready_ms: Some(3.0),
                delivery: Some(photocatalog::application::PreviewDeliveryDiagnostic {
                    ready_for_transfer_ms: 1.0,
                    retained_read: read,
                    transfer_ms: 1.0,
                    total_ms: 2.0,
                }),
            },
        }
    }

    #[test]
    fn preview_diagnostic_requires_bounded_consistent_identity_and_timing() {
        let mut diagnostic = preview_diagnostic();
        diagnostic.validate().unwrap();
        assert!(serde_json::to_vec(&diagnostic).unwrap().len() < MAX_PREVIEW_DIAGNOSTIC_BYTES);

        diagnostic.native.current_key_matches_selected = Some(false);
        assert!(diagnostic.validate().is_err());
        diagnostic.native.current_key_matches_selected = Some(true);
        diagnostic.native.selected_key_digest = None;
        assert!(diagnostic.validate().is_err());
        diagnostic.native.selected_key_digest = Some("a".repeat(64));
        diagnostic.native.delivery.as_mut().unwrap().total_ms = 0.5;
        assert!(diagnostic.validate().is_err());
        diagnostic.native.delivery.as_mut().unwrap().total_ms = 2.0;
        diagnostic.native.retained_read.as_mut().unwrap().total_ms = 0.05;
        assert!(diagnostic.validate().is_err());
        diagnostic.native.retained_read.as_mut().unwrap().total_ms = 2.0;
        diagnostic
            .native
            .retained_read
            .as_mut()
            .unwrap()
            .decoded_misses = 2;
        assert!(diagnostic.validate().is_err());
        diagnostic
            .native
            .retained_read
            .as_mut()
            .unwrap()
            .decoded_misses = 1;
        diagnostic.ready_observed_ms = f64::INFINITY;
        assert!(diagnostic.validate().is_err());
    }

    #[test]
    fn run_identifier_is_explicit_and_path_safe() {
        assert_eq!(
            parse_run_id([OsString::from("--s12-measure=gui_v7-01")]).unwrap(),
            Some("gui_v7-01".into())
        );
        assert!(parse_run_id([OsString::from("--s12-measure=../escape")]).is_err());
        assert!(
            parse_run_id([
                OsString::from("--s12-measure=first"),
                OsString::from("--s12-measure=second"),
            ])
            .is_err()
        );
    }

    #[test]
    fn receipt_requires_kind_specific_bounded_fields() {
        let mut receipt = Receipt {
            protocol: 1,
            clock_alignment: None,
            presentation_model: PresentationModel::TwoAnimationFrames,
            context_model: ContextModel::LastObservedStatusAtStart,
            run_id: "run".into(),
            time_origin_ms: 42.0,
            overflowed: 0,
            thumbnail_diagnostics: ThumbnailDiagnostics::default(),
            samples: vec![Sample {
                kind: Kind::Cull,
                ordinal: 1,
                started_us: 2_000,
                during_import: true,
                import_id: None,
                during_export: true,
                outcome: Outcome::Complete,
                durable_us: Some(4_000),
                presentation_us: Some(16_000),
                search_response_us: None,
                first_thumbnail_us: None,
                visible_complete_us: None,
                page_rows: None,
                visible_count: None,
            }],
            scroll_capture: None,
        };
        validate(&receipt, "run").unwrap();
        receipt.samples[0].presentation_us = Some(3_999);
        assert!(validate(&receipt, "run").is_err());
        receipt.samples[0].presentation_us = Some(16_000);
        receipt.samples[0].started_us = MAX_STARTED_US + 1;
        assert!(validate(&receipt, "run").is_err());
        receipt.samples[0].started_us = 2_000;
        receipt.samples[0].search_response_us = Some(1);
        assert!(validate(&receipt, "run").is_err());

        receipt.samples[0] = Sample {
            kind: Kind::Browse,
            ordinal: 1,
            started_us: 2_000,
            during_import: false,
            import_id: None,
            during_export: false,
            outcome: Outcome::Complete,
            durable_us: None,
            presentation_us: None,
            search_response_us: Some(3_000),
            first_thumbnail_us: Some(8_000),
            visible_complete_us: Some(17_000),
            page_rows: Some(100),
            visible_count: Some(12),
        };
        validate(&receipt, "run").unwrap();
        receipt.samples[0].page_rows = Some(10);
        assert!(validate(&receipt, "run").is_err());
        receipt.samples[0].page_rows = Some(100);
        receipt.samples[0].visible_complete_us = Some(7_000);
        assert!(validate(&receipt, "run").is_err());
    }

    #[test]
    fn maximum_receipt_fits_the_persisted_byte_bound() {
        let mut receipt = Receipt {
            protocol: 1,
            clock_alignment: None,
            presentation_model: PresentationModel::TwoAnimationFrames,
            context_model: ContextModel::LastObservedStatusAtStart,
            run_id: "x".repeat(64),
            time_origin_ms: 10_000_000_000_000.0,
            overflowed: u32::MAX,
            thumbnail_diagnostics: ThumbnailDiagnostics {
                attempts: MAX_THUMBNAIL_EVENTS,
                decode_completed: MAX_THUMBNAIL_EVENTS,
                decode_failed: MAX_THUMBNAIL_EVENTS,
                source_changed: MAX_THUMBNAIL_EVENTS,
                disconnected: MAX_THUMBNAIL_EVENTS,
                incomplete: MAX_THUMBNAIL_EVENTS,
                zero_size: MAX_THUMBNAIL_EVENTS,
                nonvisible: MAX_THUMBNAIL_EVENTS,
                accepted: MAX_THUMBNAIL_EVENTS,
                roster_tiles: MAX_THUMBNAIL_EVENTS,
                pending_expected: MAX_THUMBNAIL_EVENTS,
            },
            samples: (1..=MAX_SAMPLES)
                .map(|ordinal| Sample {
                    kind: Kind::Browse,
                    ordinal: ordinal as u32,
                    started_us: MAX_STARTED_US,
                    during_import: true,
                    import_id: None,
                    during_export: true,
                    outcome: Outcome::Complete,
                    durable_us: None,
                    presentation_us: None,
                    search_response_us: Some(1_800_000_000),
                    first_thumbnail_us: Some(1_800_000_000),
                    visible_complete_us: Some(1_800_000_000),
                    page_rows: Some(100),
                    visible_count: Some(100),
                })
                .collect(),
            scroll_capture: Some(ScrollCapture {
                frame_model: ScrollFrameModel::RequestAnimationFrameTimestampScrollPosition,
                outcome: ScrollOutcome::Complete,
                reason: ScrollReason::DurationElapsed,
                started_us: MAX_STARTED_US,
                ended_us: MAX_STARTED_US + SCROLL_DURATION_US,
                target_initial: ScrollSnapshot {
                    identity: 1,
                    scroll_top_px: 0,
                    scroll_left_px: 0,
                    viewport_width_px: u32::MAX - 1,
                    viewport_height_px: u32::MAX - 1,
                    scroll_width_px: u32::MAX,
                    scroll_height_px: u32::MAX,
                },
                target_final: Some(ScrollSnapshot {
                    identity: 1,
                    scroll_top_px: u32::MAX,
                    scroll_left_px: u32::MAX,
                    viewport_width_px: u32::MAX - 1,
                    viewport_height_px: u32::MAX - 1,
                    scroll_width_px: u32::MAX,
                    scroll_height_px: u32::MAX,
                }),
                frames: (0..MAX_SCROLL_FRAMES)
                    .map(|offset| {
                        (
                            MAX_STARTED_US
                                + (offset as u64 * SCROLL_DURATION_US / MAX_SCROLL_FRAMES as u64),
                            u32::MAX,
                            u32::MAX,
                        )
                    })
                    .collect(),
            }),
        };
        validate(&receipt, &receipt.run_id).unwrap();
        assert!(serde_json::to_vec(&receipt).unwrap().len() <= MAX_RECEIPT_BYTES);
        let count = crate::measurement_clock::MAX_ANCHORS;
        receipt.protocol = 2;
        receipt.clock_alignment = Some(serde_json::from_value(serde_json::json!({
            "model": "causal_native_brackets_v1", "interval_ms": 100, "duration_ms": 300000,
            "stop_reason": "anchor_limit",
            "anchors": (0..count).map(|index| serde_json::json!({
                "anchor_id": index + 1, "send_event": index * 2 + 1, "receive_event": index * 2 + 2,
                "error": null, "native": { "anchor_id": index + 1, "run_id": receipt.run_id,
                    "session_id": "ffffffff-ffff-ffff-ffff-ffffffffffff", "native_pid": u32::MAX,
                    "clock": crate::measurement_clock::CLOCK, "monotonic_ns": u64::MAX.to_string() }
            })).collect::<Vec<_>>(),
            "sample_events": (0..MAX_SAMPLES).map(|index| serde_json::json!({
                "ordinal": index + 1, "start_event": count * 2 + index * 2 + 1,
                "end_event": count * 2 + index * 2 + 2
            })).collect::<Vec<_>>()
        })).unwrap());
        validate(&receipt, &receipt.run_id).unwrap();
        assert!(serde_json::to_vec(&receipt).unwrap().len() <= MAX_RECEIPT_BYTES);
    }

    #[test]
    fn scroll_capture_requires_bounded_monotonic_callback_timestamps() {
        let mut receipt = Receipt {
            protocol: 1,
            clock_alignment: None,
            presentation_model: PresentationModel::TwoAnimationFrames,
            context_model: ContextModel::LastObservedStatusAtStart,
            run_id: "scroll".into(),
            time_origin_ms: 42.0,
            overflowed: 0,
            thumbnail_diagnostics: ThumbnailDiagnostics::default(),
            samples: Vec::new(),
            scroll_capture: Some(ScrollCapture {
                frame_model: ScrollFrameModel::RequestAnimationFrameTimestampScrollPosition,
                outcome: ScrollOutcome::Complete,
                reason: ScrollReason::DurationElapsed,
                started_us: 1_000,
                ended_us: 5_001_000,
                target_initial: ScrollSnapshot {
                    identity: 1,
                    scroll_top_px: 0,
                    scroll_left_px: 0,
                    viewport_width_px: 800,
                    viewport_height_px: 600,
                    scroll_width_px: 800,
                    scroll_height_px: 2400,
                },
                target_final: Some(ScrollSnapshot {
                    identity: 1,
                    scroll_top_px: 1800,
                    scroll_left_px: 0,
                    viewport_width_px: 800,
                    viewport_height_px: 600,
                    scroll_width_px: 800,
                    scroll_height_px: 2400,
                }),
                frames: vec![(999, 0, 0), (5_000_000, 1800, 0)],
            }),
        };
        validate(&receipt, "scroll").unwrap();
        receipt.scroll_capture.as_mut().unwrap().frames = vec![(5_000_000, 0, 0), (1_010, 0, 0)];
        assert!(validate(&receipt, "scroll").is_err());
        receipt.scroll_capture.as_mut().unwrap().frames = vec![(1_010, 0, 0), (5_001_001, 0, 0)];
        assert!(validate(&receipt, "scroll").is_err());
        let capture = receipt.scroll_capture.as_mut().unwrap();
        capture.outcome = ScrollOutcome::Incomplete;
        capture.ended_us = 12_001_000;
        capture.frames = vec![(1_010, 0, 0), (12_000_000, 0, 0)];
        validate(&receipt, "scroll").unwrap();
    }

    fn import_receipt() -> Receipt {
        let id = "00000000-0000-4000-8000-000000000001";
        Receipt {
            protocol: 2,
            clock_alignment: Some(serde_json::from_value(serde_json::json!({
                "model":"causal_native_brackets_v1", "profile":"import_v1", "interval_ms":200,
                "duration_ms":600000, "stop_reason":"finalized",
                "anchors":[{"anchor_id":1,"send_event":1,"receive_event":2,"native":{
                    "run_id":"import-run","anchor_id":1,"session_id":"ffffffff-ffff-ffff-ffff-ffffffffffff",
                    "native_pid":42,"clock":crate::measurement_clock::CLOCK,"monotonic_ns":"123"
                },"error":null}],
                "sample_events":[{"ordinal":1,"start_event":5,"durable_event":6,"end_event":7}],
                "import_evidence":{"bindings":[{"key":1,"id":id,"source_blake3":"a".repeat(64)}],
                    "timeline":[
                        {"request_event":3,"event":4,"binding":1,"phase":"discovering","imported":"0","unchanged":"0","failed":"0","skipped":"0","metadata_updated":"0","metadata_warnings":"0","awaiting_resources":"0","pending_previews":0},
                        {"request_event":8,"event":9,"binding":1,"phase":"complete","imported":"1","unchanged":"0","failed":"0","skipped":"0","metadata_updated":"1","metadata_warnings":"0","awaiting_resources":"0","pending_previews":0}
                    ],"overflowed":0}
            })).unwrap()),
            presentation_model: PresentationModel::TwoAnimationFrames,
            context_model: ContextModel::LastObservedStatusAtStart,
            run_id: "import-run".into(),
            time_origin_ms: 42.0,
            overflowed: 0,
            thumbnail_diagnostics: ThumbnailDiagnostics::default(),
            samples: vec![Sample {
                kind: Kind::Cull,
                ordinal: 1,
                started_us: 1,
                during_import: true,
                import_id: Some(id.into()),
                during_export: false,
                outcome: Outcome::Complete,
                durable_us: Some(2),
                presentation_us: Some(3),
                search_response_us: None,
                first_thumbnail_us: None,
                visible_complete_us: None,
                page_rows: None,
                visible_count: None,
            }],
            scroll_capture: None,
        }
    }

    #[test]
    fn import_receipt_requires_bound_identity() {
        let mut receipt = import_receipt();
        validate(&receipt, "import-run").unwrap();
        receipt.samples[0].import_id = Some("00000000-0000-4000-8000-000000000002".into());
        assert!(validate(&receipt, "import-run").is_err());
        receipt = import_receipt();
        receipt.clock_alignment = None;
        assert!(validate(&receipt, "import-run").is_err());
    }

    #[test]
    fn import_receipt_accepts_a_durable_idle_warmup_without_a_causal_event() {
        let mut receipt = import_receipt();
        let mut alignment = serde_json::to_value(receipt.clock_alignment.take().unwrap()).unwrap();
        alignment["sample_events"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"ordinal":2,"start_event":10,"end_event":11}));
        receipt.clock_alignment = Some(serde_json::from_value(alignment.clone()).unwrap());
        receipt.samples.push(Sample {
            kind: Kind::Edit,
            ordinal: 2,
            started_us: 1,
            during_import: false,
            import_id: None,
            during_export: false,
            outcome: Outcome::Complete,
            durable_us: Some(2),
            presentation_us: Some(3),
            search_response_us: None,
            first_thumbnail_us: None,
            visible_complete_us: None,
            page_rows: None,
            visible_count: None,
        });
        validate(&receipt, "import-run").unwrap();
        alignment["sample_events"][1]["durable_event"] = serde_json::json!(12);
        alignment["sample_events"][1]["end_event"] = serde_json::json!(13);
        receipt.clock_alignment = Some(serde_json::from_value(alignment).unwrap());
        validate(&receipt, "import-run").unwrap();
        receipt.samples[1].durable_us = None;
        receipt.samples[1].presentation_us = None;
        receipt.samples[1].outcome = Outcome::BackendError;
        assert!(validate(&receipt, "import-run").is_err());
    }

    #[test]
    fn maximum_import_receipt_fits_the_persisted_bound() {
        let id = "00000000-0000-4000-8000-000000000001";
        let anchors = crate::measurement_clock::MAX_ANCHORS;
        let timeline_start = anchors * 2 + 1;
        let samples_start = timeline_start + crate::measurement_clock::MAX_IMPORT_TIMELINE * 2;
        let mut receipt = Receipt {
            protocol: 2,
            clock_alignment: Some(serde_json::from_value(serde_json::json!({
                "model":"causal_native_brackets_v1", "profile":"import_v1", "interval_ms":200,
                "duration_ms":600000, "stop_reason":"anchor_limit",
                "anchors":(0..anchors).map(|index| serde_json::json!({
                    "anchor_id":index+1,"send_event":index*2+1,"receive_event":index*2+2,"error":null,
                    "native":{"run_id":"max-import","anchor_id":index+1,"session_id":"ffffffff-ffff-ffff-ffff-ffffffffffff",
                        "native_pid":u32::MAX,"clock":crate::measurement_clock::CLOCK,"monotonic_ns":u64::MAX.to_string()}
                })).collect::<Vec<_>>(),
                "sample_events":(0..MAX_SAMPLES).map(|index| serde_json::json!({
                    "ordinal":index+1,"start_event":samples_start+index*3,"durable_event":samples_start+index*3+1,"end_event":samples_start+index*3+2
                })).collect::<Vec<_>>(),
                "import_evidence":{"bindings":(1..=crate::measurement_clock::MAX_IMPORT_BINDINGS).map(|key| serde_json::json!({
                    "key":key,"id":format!("00000000-0000-4000-8000-{key:012}"),"source_blake3":format!("{key:x}").repeat(64)
                })).collect::<Vec<_>>(),
                    "timeline":(0..crate::measurement_clock::MAX_IMPORT_TIMELINE).map(|index| serde_json::json!({
                        "request_event":timeline_start+index*2,"event":timeline_start+index*2+1,
                        "binding":if index<crate::measurement_clock::MAX_IMPORT_BINDINGS{index+1}else{1},
                        "phase":if index+1==crate::measurement_clock::MAX_IMPORT_TIMELINE{"complete"}else{"discovering"},
                        "imported":u64::MAX.to_string(),"unchanged":u64::MAX.to_string(),"failed":u64::MAX.to_string(),"skipped":u64::MAX.to_string(),
                        "metadata_updated":u64::MAX.to_string(),"metadata_warnings":u64::MAX.to_string(),"awaiting_resources":u64::MAX.to_string(),"pending_previews":u32::MAX
                    })).collect::<Vec<_>>(),"overflowed":u32::MAX}
            })).unwrap()),
            presentation_model: PresentationModel::TwoAnimationFrames,
            context_model: ContextModel::LastObservedStatusAtStart,
            run_id: "max-import".into(),
            time_origin_ms: 42.0,
            overflowed: 0,
            thumbnail_diagnostics: ThumbnailDiagnostics::default(),
            samples: (1..=MAX_SAMPLES as u32).map(|ordinal| Sample {
                kind: Kind::Edit, ordinal, started_us: MAX_STARTED_US, during_import: true,
                import_id: Some(id.into()), during_export: false, outcome: Outcome::Complete,
                durable_us: Some(MAX_DURATION_US), presentation_us: Some(MAX_DURATION_US),
                search_response_us: None, first_thumbnail_us: None, visible_complete_us: None,
                page_rows: None, visible_count: None,
            }).collect(),
            scroll_capture: None,
        };
        validate(&receipt, "max-import").unwrap();
        let bytes = serde_json::to_vec(&receipt).unwrap();
        assert!(
            bytes.len() <= MAX_RECEIPT_BYTES,
            "maximum import receipt is {} bytes",
            bytes.len()
        );
        let mut alignment = serde_json::to_value(receipt.clock_alignment.take().unwrap()).unwrap();
        alignment["import_evidence"]["bindings"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"key":2,"id":id,"source_blake3":"e".repeat(64)}));
        receipt.clock_alignment = Some(serde_json::from_value(alignment).unwrap());
        assert!(validate(&receipt, "max-import").is_err());
    }

    #[test]
    fn finalize_retries_exactly_without_replacing_an_existing_receipt() {
        fn pending_alignment() -> crate::measurement_clock::Alignment {
            serde_json::from_value(serde_json::json!({"model":"causal_native_brackets_v1", "interval_ms":100,
                "duration_ms":300000, "stop_reason":"finalized", "sample_events":[],
                "anchors":[{"anchor_id":1,"send_event":1,"receive_event":null,"native":null,"error":"incomplete"}]})).unwrap()
        }
        let cache_root = std::env::temp_dir().join(format!(
            "photocatalog-s12-measurement-{}",
            uuid::Uuid::new_v4()
        ));
        let state = State::new(Some("persist-once".into()), cache_root.clone());
        let receipt = Receipt {
            protocol: 2,
            clock_alignment: Some(pending_alignment()),
            presentation_model: PresentationModel::TwoAnimationFrames,
            context_model: ContextModel::LastObservedStatusAtStart,
            run_id: "persist-once".into(),
            time_origin_ms: 42.0,
            overflowed: 0,
            thumbnail_diagnostics: ThumbnailDiagnostics::default(),
            samples: Vec::new(),
            scroll_capture: None,
        };
        let path = PathBuf::from(state.finish(receipt).unwrap());
        let stored: Receipt = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        validate(&stored, "persist-once").unwrap();
        // A command can have been issued even though its frontend reply missed the frozen receipt.
        state
            .0
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .clock_anchors
            .push(crate::measurement_clock::NativeAnchor {
                run_id: "persist-once".into(),
                anchor_id: 1,
                session_id: uuid::Uuid::new_v4().to_string(),
                native_pid: 1,
                clock: crate::measurement_clock::CLOCK.into(),
                monotonic_ns: "123".into(),
            });
        let exact_retry = state
            .finish(Receipt {
                protocol: 2,
                clock_alignment: Some(pending_alignment()),
                presentation_model: PresentationModel::TwoAnimationFrames,
                context_model: ContextModel::LastObservedStatusAtStart,
                run_id: "persist-once".into(),
                time_origin_ms: 42.0,
                overflowed: 0,
                thumbnail_diagnostics: ThumbnailDiagnostics::default(),
                samples: Vec::new(),
                scroll_capture: None,
            })
            .unwrap();
        assert_eq!(PathBuf::from(exact_retry), path);
        assert!(
            state
                .finish(Receipt {
                    protocol: 2,
                    clock_alignment: Some(pending_alignment()),
                    presentation_model: PresentationModel::TwoAnimationFrames,
                    context_model: ContextModel::LastObservedStatusAtStart,
                    run_id: "persist-once".into(),
                    time_origin_ms: 43.0,
                    overflowed: 0,
                    thumbnail_diagnostics: ThumbnailDiagnostics::default(),
                    samples: Vec::new(),
                    scroll_capture: None,
                })
                .is_err()
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            serde_json::to_vec(&stored).unwrap()
        );
        assert!(path.exists());
        fs::remove_dir_all(cache_root).unwrap();
    }
}
