use super::*;
use std::cell::Cell;

struct Counted<'a> {
    job: &'a SavedJob,
    passes: &'a Cell<usize>,
}
impl Serialize for Counted<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        self.passes.set(self.passes.get() + 1);
        self.job.serialize(serializer)
    }
}

#[test]
fn saved_job_count_rejects_before_destination_pass_and_preserves_exact_retry() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (catalog, mut previews, asset, source) = recovery_tests::setup(root.path());
    let mut job = recovery_tests::import_job(&catalog, &previews, &asset, &source);
    job.state = JobState::Failed(String::new());
    let fixed = serde_json::to_string(&job)?.len();
    job.state = JobState::Failed("a".repeat(64 * 1024 - fixed));
    let passes = Cell::new(0);
    let encoded = super::super::store::encoded_descriptor(
        &Counted {
            job: &job,
            passes: &passes,
        },
        "saved job serialization changed",
    )?;
    assert_eq!(passes.get(), 2);
    assert_eq!(encoded.len(), 64 * 1024);
    assert_eq!(encoded, serde_json::to_string(&job)?);
    if let JobState::Failed(message) = &mut job.state {
        message.push('a');
    }
    passes.set(0);
    assert!(
        super::super::store::encoded_descriptor(
            &Counted {
                job: &job,
                passes: &passes
            },
            "saved job serialization changed"
        )
        .is_err()
    );
    assert_eq!(
        passes.get(),
        1,
        "oversized input must not start destination serialization"
    );
    job.state = JobState::Failed("\0".repeat((64 * 1024 - fixed) / 6 + 1));
    passes.set(0);
    assert!(
        super::super::store::encoded_descriptor(
            &Counted {
                job: &job,
                passes: &passes
            },
            "saved job serialization changed"
        )
        .is_err()
    );
    assert_eq!(
        passes.get(),
        1,
        "escaping expansion must be counted before allocation"
    );
    job.state = JobState::Queued;
    passes.set(0);
    let retry = super::super::store::encoded_descriptor(
        &Counted {
            job: &job,
            passes: &passes,
        },
        "saved job serialization changed",
    )?;
    assert_eq!(passes.get(), 2);
    assert_eq!(retry, serde_json::to_string(&job)?);
    previews.try_shutdown()?;
    Ok(())
}

#[test]
fn failure_prefix_preserves_unicode_character_policy_and_resource_category() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (catalog, mut previews, asset, source) = recovery_tests::setup(root.path());
    let job = recovery_tests::import_job(&catalog, &previews, &asset, &source);
    let error = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::StorageFull))
        .context("🦀".repeat(5000));
    let expected = format!("{error:#}").chars().take(4096).collect::<String>();
    let prefix = failure_message(&error);
    assert_eq!(prefix, expected);
    assert_eq!(prefix.len(), 4 * 4096);
    assert!(prefix.capacity() <= 4 * 4096);
    assert!(error.downcast_ref::<std::io::Error>().is_some());
    let completion = previews.record_failure(&"a".repeat(64), &job, error)?;
    match completion {
        ServiceCompletion::NeedsResources(message) => assert_eq!(message, expected),
        other => panic!("resource category changed: {other:?}"),
    }
    let retained = previews.jobs(0, 1)?;
    assert!(
        matches!(&retained[0].state, JobState::NeedsResources(message) if message == &expected)
    );
    previews.try_shutdown()?;
    Ok(())
}
