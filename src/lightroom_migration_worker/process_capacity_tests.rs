use super::*;
use crate::capacity_probes as probe;
use std::time::Instant;
const HELPER: &str = "lightroom_migration_worker::process::capacity_tests::capacity_pipe_helper";
const KEY: &str = "PHOTOCATALOG_CAPACITY_PIPE_HELPER";
#[test]
fn capacity_pipe_helper() -> Result<()> {
    if std::env::var_os(KEY).is_none() {
        return Ok(());
    }
    let baseline = probe::begin();
    let mut input = std::io::stdin();
    let mut output = std::io::stderr();
    while let Some(frame) = super::super::protocol::read_frame_optional::<Vec<u8>>(&mut input)? {
        if frame.is_empty() {
            break;
        }
        super::super::protocol::write_frame(&mut output, &frame)?;
    }
    probe::report("pipe-helper", baseline);
    std::process::exit(0);
}
#[test]
fn capacity_bounded_pipe_queues_keep_cancel_and_reap() -> Result<()> {
    let baseline = probe::begin();
    let stop = Arc::new(Stop::default());
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["--exact", HELPER, "--nocapture"])
        .env(KEY, "1");
    let mut process = Process::<Vec<u8>>::spawn_test_command(command, stop.clone())?;
    let pid = process.pid();
    let frame = vec![255; 16 * 1024];
    let mut accepted = 0;
    let mut full = 0;
    // Hold output consumption. The actual one-slot channels and blocked pipe
    // workers retain their current frames; every rejected send remains caller-owned.
    for _ in 0..256 {
        if process.try_send(frame.clone())?.is_some() {
            full += 1;
        } else {
            accepted += 1;
        }
    }
    assert!(accepted > 0 && full > 0);
    let until = Instant::now() + Duration::from_secs(10);
    let mut received = 0;
    while received < accepted {
        ensure!(Instant::now() < until, "probe response deadline");
        match process.try_receive()? {
            Output::Frame(bytes) => {
                assert_eq!(bytes, frame);
                received += 1;
            }
            Output::Pending => thread::sleep(Duration::from_millis(2)),
            Output::End => anyhow::bail!("early helper exit"),
        }
    }
    // Observe Full again after draining the first burst. Preserve the exact
    // returned caller-owned frame and cancel immediately in that branch. The
    // consumer may advance concurrently; this proves ordering after observed
    // backpressure, not a claim that the queue cannot change afterward.
    let mut canceled_frame = None;
    for _ in 0..256 {
        if let Some(unsent) = process.try_send(frame.clone())? {
            assert_eq!(unsent, frame);
            stop.cancel();
            canceled_frame = Some(unsent);
            break;
        }
    }
    ensure!(
        canceled_frame.is_some(),
        "no Full send immediately before cancel"
    );
    process.terminate();
    assert!(process.try_reap()?.is_some());
    #[cfg(unix)]
    {
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    println!(
        "CAPACITY_PIPE pid={pid} accepted={accepted} full={full} received={received} reaped=true cancel_after_observed_full=true input_slots=1 output_slots=1"
    );
    probe::report("bounded-pipes", baseline);
    Ok(())
}
