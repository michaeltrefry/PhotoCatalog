//! Bounded scheduling decisions. A reservation is released only after the owner
//! reports that a worker exited, including when cancellation was requested.
use anyhow::{Result, ensure};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Background,
    Foreground,
}
#[derive(Debug, Clone, Copy)]
pub struct SchedulerLimits {
    pub requests: usize,
    pub workers: usize,
    pub working_bytes: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Consumer(pub u64);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerOutcome {
    Succeeded,
    Failed,
    Stopped,
}
#[derive(Debug)]
pub struct WorkLease {
    pub id: u64,
    pub key: String,
    pub working_bytes: u64,
    pub canceled: Arc<AtomicBool>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerUsage {
    pub consumers: usize,
    pub queued: usize,
    pub active: usize,
    pub reserved_bytes: u64,
}
#[derive(Debug)]
pub struct Completion {
    pub consumers: Vec<Consumer>,
    pub outcome: WorkerOutcome,
    pub requeued: bool,
}
struct Job {
    key: String,
    cost: u64,
    consumers: HashMap<Consumer, Priority>,
    running: bool,
    canceled: Arc<AtomicBool>,
    preempted: bool,
    lease: Option<u64>,
}
impl Job {
    fn priority(&self) -> Priority {
        self.consumers
            .values()
            .copied()
            .max()
            .unwrap_or(Priority::Background)
    }
}
/// The service actor owns this scheduler; no background worker can mutate desired
/// catalog identities or release its own reservation before returning/exiting.
pub struct PreviewScheduler {
    limits: SchedulerLimits,
    next: u64,
    jobs: HashMap<u64, Job>,
    keys: HashMap<String, u64>,
    consumers: HashMap<Consumer, u64>,
    reserved: u64,
}
impl PreviewScheduler {
    pub fn new(limits: SchedulerLimits) -> Result<Self> {
        ensure!(
            limits.requests > 0
                && limits.requests <= 100_000
                && limits.workers > 0
                && limits.workers <= 16
                && limits.working_bytes > 0,
            "invalid scheduler limits"
        );
        Ok(Self {
            limits,
            next: 1,
            jobs: HashMap::new(),
            keys: HashMap::new(),
            consumers: HashMap::new(),
            reserved: 0,
        })
    }
    fn serial(&mut self) -> Result<u64> {
        let value = self.next;
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("request identity overflow"))?;
        Ok(value)
    }
    pub fn request(&mut self, key: String, cost: u64, priority: Priority) -> Result<Consumer> {
        ensure!(
            key.len() == 64 && key.bytes().all(|x| x.is_ascii_hexdigit()),
            "invalid preview key"
        );
        ensure!(
            cost > 0 && cost <= self.limits.working_bytes,
            "request exceeds worker allowance"
        );
        ensure!(
            self.consumers.len() < self.limits.requests,
            "preview request queue full"
        );
        let consumer = Consumer(self.serial()?);
        let existing = self.keys.get(&key).copied().filter(|id| {
            !self.jobs[id].canceled.load(Ordering::Acquire) || self.jobs[id].preempted
        });
        let id = if let Some(id) = existing {
            let job = self.jobs.get_mut(&id).unwrap();
            ensure!(
                job.cost == cost,
                "same preview key has conflicting resource cost"
            );
            job.consumers.insert(consumer, priority);
            id
        } else {
            let id = self.serial()?;
            self.keys.insert(key.clone(), id);
            self.jobs.insert(
                id,
                Job {
                    key,
                    cost,
                    consumers: HashMap::from([(consumer, priority)]),
                    running: false,
                    canceled: Arc::new(AtomicBool::new(false)),
                    preempted: false,
                    lease: None,
                },
            );
            id
        };
        self.consumers.insert(consumer, id);
        if priority == Priority::Foreground {
            self.preempt_background();
        }
        Ok(consumer)
    }
    fn preempt_background(&mut self) {
        // Native calls can be uninterruptible. The owner observes this token,
        // terminates an isolated background worker and calls finished after wait.
        let active = self.jobs.values().filter(|job| job.running).count();
        let foreground = self
            .jobs
            .iter()
            .filter(|(_, job)| !job.running && job.priority() == Priority::Foreground)
            .min_by_key(|(id, _)| **id)
            .map(|(_, job)| job.cost);
        if foreground.is_some_and(|cost| {
            active >= self.limits.workers || cost > self.limits.working_bytes - self.reserved
        }) && let Some((_, job)) = self
            .jobs
            .iter_mut()
            .filter(|(_, job)| job.running && job.priority() == Priority::Background)
            .min_by_key(|(id, _)| **id)
        {
            job.preempted = true;
            job.canceled.store(true, Ordering::Release);
        }
    }
    pub fn cancel(&mut self, consumer: Consumer) -> bool {
        let Some(id) = self.consumers.remove(&consumer) else {
            return false;
        };
        let job = self.jobs.get_mut(&id).unwrap();
        job.consumers.remove(&consumer);
        if job.consumers.is_empty() {
            job.canceled.store(true, Ordering::Release);
            if !job.running {
                self.remove(id);
            }
        }
        true
    }
    fn remove(&mut self, id: u64) -> Job {
        let job = self.jobs.remove(&id).unwrap();
        if self.keys.get(&job.key) == Some(&id) {
            self.keys.remove(&job.key);
        }
        for consumer in job.consumers.keys() {
            self.consumers.remove(consumer);
        }
        job
    }
    pub fn next_ready(&mut self) -> Result<Option<WorkLease>> {
        self.preempt_background();
        if self.jobs.values().filter(|job| job.running).count() >= self.limits.workers {
            return Ok(None);
        }
        let Some(id) = self
            .jobs
            .iter()
            .filter(|(_, job)| !job.running && !job.canceled.load(Ordering::Acquire))
            .min_by_key(|(id, job)| (std::cmp::Reverse(job.priority()), **id))
            .map(|(id, _)| *id)
        else {
            return Ok(None);
        };
        if self.jobs[&id].cost > self.limits.working_bytes - self.reserved {
            return Ok(None);
        }
        // Every admission gets a new identity, including a preempted job's retry.
        // A delayed exit notification from its earlier process cannot release
        // the new process's reservation.
        let lease = self.serial()?;
        let job = self.jobs.get_mut(&id).unwrap();
        self.reserved += job.cost;
        job.running = true;
        job.lease = Some(lease);
        Ok(Some(WorkLease {
            id: lease,
            key: job.key.clone(),
            working_bytes: job.cost,
            canceled: job.canceled.clone(),
        }))
    }
    /// Only after join/wait confirms the worker released native allocations.
    pub fn finished(&mut self, lease: u64, outcome: WorkerOutcome) -> Result<Completion> {
        let id = self
            .jobs
            .iter()
            .find_map(|(id, job)| (job.lease == Some(lease)).then_some(*id))
            .ok_or_else(|| anyhow::anyhow!("unknown or stale worker completion"))?;
        let job = self
            .jobs
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown worker completion"))?;
        ensure!(job.running, "queued work cannot complete");
        job.lease = None;
        self.reserved -= job.cost;
        if job.preempted && !job.consumers.is_empty() {
            job.running = false;
            job.preempted = false;
            job.canceled = Arc::new(AtomicBool::new(false));
            return Ok(Completion {
                consumers: Vec::new(),
                outcome: WorkerOutcome::Stopped,
                requeued: true,
            });
        }
        let job = self.remove(id);
        let outcome = if job.canceled.load(Ordering::Acquire) {
            WorkerOutcome::Stopped
        } else {
            outcome
        };
        Ok(Completion {
            consumers: job.consumers.keys().copied().collect(),
            outcome,
            requeued: false,
        })
    }
    pub fn usage(&self) -> SchedulerUsage {
        let active = self.jobs.values().filter(|job| job.running).count();
        SchedulerUsage {
            consumers: self.consumers.len(),
            queued: self.jobs.len() - active,
            active,
            reserved_bytes: self.reserved,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(n: u8) -> String {
        format!("{n:064x}")
    }
    fn scheduler() -> PreviewScheduler {
        PreviewScheduler::new(SchedulerLimits {
            requests: 3,
            workers: 1,
            working_bytes: 100,
        })
        .unwrap()
    }
    #[test]
    fn foreground_preemption_keeps_reservation_until_worker_exit() {
        let mut queue = scheduler();
        let background = queue.request(key(1), 80, Priority::Background).unwrap();
        let first = queue.next_ready().unwrap().unwrap();
        let foreground = queue.request(key(2), 60, Priority::Foreground).unwrap();
        assert!(first.canceled.load(Ordering::Acquire));
        assert!(queue.next_ready().unwrap().is_none());
        assert_eq!(queue.usage().reserved_bytes, 80);
        assert!(
            queue
                .finished(first.id, WorkerOutcome::Succeeded)
                .unwrap()
                .requeued
        );
        let visible = queue.next_ready().unwrap().unwrap();
        assert_eq!(visible.key, key(2));
        assert_eq!(
            queue
                .finished(visible.id, WorkerOutcome::Succeeded)
                .unwrap()
                .consumers,
            vec![foreground]
        );
        let resumed = queue.next_ready().unwrap().unwrap();
        assert_eq!(resumed.key, key(1));
        assert_ne!(resumed.id, first.id);
        assert!(queue.finished(first.id, WorkerOutcome::Succeeded).is_err());
        assert_eq!(queue.usage().reserved_bytes, 80);
        assert!(!resumed.canceled.load(Ordering::Acquire));
        assert_eq!(
            queue
                .finished(resumed.id, WorkerOutcome::Succeeded)
                .unwrap()
                .consumers,
            vec![background]
        );
        assert_eq!(queue.usage().reserved_bytes, 0);
    }
    #[test]
    fn duplicates_promote_and_cancel_only_after_last_consumer() {
        let mut queue = scheduler();
        let a = queue.request(key(1), 80, Priority::Background).unwrap();
        let b = queue.request(key(1), 80, Priority::Foreground).unwrap();
        assert_eq!(queue.usage().queued, 1);
        let lease = queue.next_ready().unwrap().unwrap();
        assert!(queue.cancel(a));
        assert!(!lease.canceled.load(Ordering::Acquire));
        assert!(queue.cancel(b));
        assert!(lease.canceled.load(Ordering::Acquire));
        assert_eq!(queue.usage().reserved_bytes, 80);
        assert_eq!(
            queue
                .finished(lease.id, WorkerOutcome::Succeeded)
                .unwrap()
                .outcome,
            WorkerOutcome::Stopped
        );
        assert!(queue.finished(lease.id, WorkerOutcome::Succeeded).is_err());
    }
    #[test]
    fn queue_memory_and_duplicate_consumers_are_bounded() {
        let mut queue = scheduler();
        for _ in 0..3 {
            queue.request(key(1), 80, Priority::Background).unwrap();
        }
        assert!(queue.request(key(1), 80, Priority::Foreground).is_err());
        assert!(queue.request(key(2), 101, Priority::Foreground).is_err());
        assert_eq!(queue.usage().consumers, 3);
        assert_eq!(queue.usage().queued, 1);
    }
}
