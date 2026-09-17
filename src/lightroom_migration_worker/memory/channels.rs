//! Pinned Rust 1.98 bounded-channel backing. Typed payloads and application Arc
//! wrappers are separate owners. All coefficients use the current target ABI.
use super::layout::{add, mul};
use anyhow::{Context, Result};
use std::{
    alloc::Layout,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicPtr, AtomicUsize},
    },
};

fn layout<T>() -> Layout {
    Layout::new::<T>()
}
fn array(value: Layout, count: usize) -> Result<Layout> {
    Layout::from_size_align(mul(value.pad_to_align().size(), count)?, value.align())
        .context("migration channel array layout")
}
/// A Rust struct may reorder fields. Charge every internal/final padding
/// boundary instead of assuming a private field order or casting private types.
fn structure(fields: &[Layout], explicit_align: usize) -> Result<Layout> {
    let alignment = fields
        .iter()
        .map(Layout::align)
        .max()
        .unwrap_or(1)
        .max(explicit_align);
    let size = fields
        .iter()
        .try_fold(0, |sum, field| add(sum, field.size()))?;
    let size = add(size, mul(fields.len(), alignment - 1)?)?;
    Ok(Layout::from_size_align(size, alignment)
        .context("migration channel struct layout")?
        .pad_to_align())
}
pub(crate) fn arc(value: Layout) -> Result<usize> {
    // ArcInner is repr(C,align(2)): strong, weak, data. Extend uses actual ABI
    // padding; the supplied private-data Layout may itself be a proven upper.
    let (counts, _) = layout::<AtomicUsize>().extend(layout::<AtomicUsize>())?;
    let (whole, _) = counts.extend(value)?;
    Ok(whole.align_to(2)?.pad_to_align().size())
}
fn mutex(value: Layout) -> Result<Layout> {
    // sys::Mutex and poison::Flag each fit in Mutex<()>; UnsafeCell has T layout.
    structure(&[layout::<Mutex<()>>(), layout::<Mutex<()>>(), value], 1)
}
#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "powerpc64"
))]
const CACHE_ALIGNMENT: usize = 128;
#[cfg(any(
    target_arch = "arm",
    target_arch = "mips",
    target_arch = "mips32r6",
    target_arch = "mips64",
    target_arch = "mips64r6"
))]
const CACHE_ALIGNMENT: usize = 32;
#[cfg(target_arch = "s390x")]
const CACHE_ALIGNMENT: usize = 256;
#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "powerpc64",
    target_arch = "arm",
    target_arch = "mips",
    target_arch = "mips32r6",
    target_arch = "mips64",
    target_arch = "mips64r6",
    target_arch = "s390x"
)))]
const CACHE_ALIGNMENT: usize = 64;

pub(crate) fn bounded(capacity: usize, item: Layout) -> Result<usize> {
    let slot = structure(&[layout::<AtomicUsize>(), item], 1)?;
    let buffer = array(slot, capacity)?;
    let waker = structure(&[layout::<Vec<()>>(), layout::<Vec<()>>()], 1)?;
    let sync_waker = structure(&[mutex(waker)?, layout::<AtomicBool>()], 1)?;
    let cache = layout::<AtomicUsize>()
        .align_to(CACHE_ALIGNMENT)?
        .pad_to_align();
    let channel = structure(
        &[
            cache,
            cache,
            layout::<Box<[()]>>(),
            layout::<usize>(),
            layout::<usize>(),
            layout::<usize>(),
            sync_waker,
            sync_waker,
        ],
        CACHE_ALIGNMENT,
    )?;
    let counter = structure(
        &[
            layout::<AtomicUsize>(),
            layout::<AtomicUsize>(),
            layout::<AtomicBool>(),
            channel,
        ],
        1,
    )?;
    add(buffer.size(), counter.size())
}
fn selector() -> Result<Layout> {
    structure(
        &[layout::<usize>(), layout::<*mut ()>(), layout::<Arc<()>>()],
        1,
    )
}
fn context() -> Result<Layout> {
    structure(
        &[
            layout::<AtomicUsize>(),
            layout::<AtomicPtr<()>>(),
            layout::<std::thread::Thread>(),
            layout::<usize>(),
        ],
        1,
    )
}
/// One blocking channel operation retains a four-entry selector vector and
/// one shared wait context while the thread is parked.
pub(crate) fn blocking_waiter() -> Result<usize> {
    add(mul(4, selector()?.size())?, arc(context()?)?)
}
/// Two registering sides per Process: ordinary input recv_timeout and output
/// blocking send. Each has one waiter and retains the first four-entry Vec.
pub(crate) fn process<T>() -> Result<usize> {
    let channels = add(
        mul(2, bounded(1, layout::<Vec<u8>>())?)?,
        bounded(1, layout::<anyhow::Result<Option<T>>>())?,
    )?;
    add(
        add(channels, mul(2, blocking_waiter()?)?)?,
        pthread_mutexes(12)?,
    )
}
pub(crate) fn broker<Command, Event>() -> Result<usize> {
    add(
        add(
            bounded(2, layout::<Command>())?,
            bounded(2, layout::<Event>())?,
        )?,
        pthread_mutexes(8)?,
    )
}

pub(crate) fn pthread_mutexes(users: usize) -> Result<usize> {
    #[cfg(target_vendor = "apple")]
    {
        mul(users, std::mem::size_of::<libc::pthread_mutex_t>())
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        let _ = users;
        Ok(0)
    }
}
pub(crate) fn pthread_condvars(users: usize) -> Result<usize> {
    #[cfg(target_vendor = "apple")]
    {
        mul(users, std::mem::size_of::<libc::pthread_cond_t>())
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        let _ = users;
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn current_target_channel_backing_checks_all_arithmetic() -> Result<()> {
        let one = bounded(1, Layout::new::<Vec<u8>>())?;
        let two = bounded(2, Layout::new::<Vec<u8>>())?;
        let waiter = blocking_waiter()?;
        assert!(two > one);
        assert!(one >= std::mem::size_of::<Vec<u8>>());
        assert!(waiter >= 4 * std::mem::size_of::<Arc<()>>());
        assert!(process::<crate::lightroom_migration_worker::protocol::ChildFrame>()? > 2 * one);
        assert!(bounded(usize::MAX, Layout::new::<Vec<u8>>()).is_err());
        assert!(
            arc(Layout::new::<u128>())?
                >= std::mem::size_of::<u128>() + 2 * std::mem::size_of::<usize>()
        );
        Ok(())
    }
}
