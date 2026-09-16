//! Checked target-layout expressions. Constants describe accepted wire grammar
//! and pinned Rust 1.98 collection algorithms, not an allocator/RSS guarantee.
use anyhow::{Context, Result};
use std::mem::{align_of, size_of};

pub(crate) fn add(a: usize, b: usize) -> Result<usize> {
    a.checked_add(b)
        .context("migration allocation sum overflow")
}
pub(crate) fn mul(a: usize, b: usize) -> Result<usize> {
    a.checked_mul(b)
        .context("migration allocation product overflow")
}
fn ceiling_product(bytes: usize, numerator: usize, denominator: usize) -> Result<usize> {
    // Divide first: an otherwise representable result need not multiply the
    // whole input by the coefficient. Every intermediate remains checked.
    add(
        mul(bytes / denominator, numerator)?,
        mul(bytes % denominator, numerator)?.div_ceil(denominator),
    )
}
fn maximum_ratio(bytes: usize, ratios: &[(usize, usize)]) -> Result<usize> {
    let mut maximum = 0;
    for &(numerator, denominator) in ratios {
        maximum = maximum.max(ceiling_product(bytes, numerator, denominator)?);
    }
    Ok(maximum)
}

/// Five-field leaf plus repr(C) internal's twelve pointers, allowing every
/// field/final padding boundary. Option<NonNull<_>> has pointer-size niche.
pub(crate) fn tree_node<K, V>() -> Result<usize> {
    tree_node_layout(
        std::alloc::Layout::new::<K>(),
        std::alloc::Layout::new::<V>(),
    )
}
pub(crate) fn tree_node_layout(
    key: std::alloc::Layout,
    value: std::alloc::Layout,
) -> Result<usize> {
    let pointer = size_of::<usize>();
    let alignment = align_of::<usize>()
        .max(align_of::<u16>())
        .max(key.align())
        .max(value.align());
    let leaf = add(
        add(pointer, 4)?,
        add(
            mul(11, add(key.size(), value.size())?)?,
            mul(5, alignment - 1)?,
        )?,
    )?;
    add(leaf, add(mul(12, pointer)?, mul(2, alignment - 1)?)?)
}
/// Settled occupancy plus an entire insertion split cascade. Empty trees have
/// no allocation. Multiple trees must be charged individually, including roots.
pub(crate) fn tree<K, V>(entries: usize) -> Result<usize> {
    tree_layout(
        entries,
        std::alloc::Layout::new::<K>(),
        std::alloc::Layout::new::<V>(),
    )
}
pub(crate) fn tree_layout(
    entries: usize,
    key: std::alloc::Layout,
    value: std::alloc::Layout,
) -> Result<usize> {
    if entries == 0 {
        return Ok(0);
    }
    let settled = add(1, (entries - 1) / 5)?;
    let mut minimum = 1usize;
    let mut height = 0usize;
    while let Some(next) = minimum.checked_mul(6).and_then(|n| n.checked_add(5)) {
        if next > entries {
            break;
        }
        minimum = next;
        height = add(height, 1)?;
    }
    mul(
        add(settled, add(height, 2)?)?,
        tree_node_layout(key, value)?,
    )
}

/// Pinned RawVec minimum and doubling envelope, including a partial final item.
/// An exact clone can use less; callers may retain this larger allowance.
pub(crate) fn vector_layout(entries: usize, item: std::alloc::Layout) -> Result<usize> {
    if entries == 0 {
        return Ok(0);
    }
    mul(add(mul(2, entries)?, 8)?, item.size())
}
pub(crate) fn vector<T>(entries: usize) -> Result<usize> {
    vector_layout(entries, std::alloc::Layout::new::<T>())
}

pub(crate) fn manifest_dynamic() -> Result<usize> {
    manifest_dynamic_within(crate::lightroom::MANIFEST_BYTES)
}
pub(crate) fn manifest_dynamic_within(encoded: usize) -> Result<usize> {
    use crate::{
        lightroom::{
            Issue,
            capture::{Artifact, Entry},
        },
        storage_volume::NativePath,
    };
    // Four nonempty arrays lose at most four separator bytes. The admitted
    // footprint uses M+4, not an unadjusted M floor.
    maximum_ratio(
        add(crate::lightroom::MANIFEST_BYTES.min(encoded), 4)?,
        &[
            (size_of::<Artifact>(), 57),
            (size_of::<Entry>(), 49),
            (size_of::<NativePath>(), 17),
            (size_of::<Issue>(), 11),
            (2, 1),
        ],
    )
}

/// Complete Source seal dynamic storage. Selected plus excluded is a single
/// 16,384-member allowance; the separate supplement roster has the same count.
/// The byte/unit floor admits at most M+1 owned payload bytes. Inline InputSeal
/// storage belongs to its actual caller and is deliberately not another Box.
pub(crate) fn seal_dynamic() -> Result<usize> {
    use crate::lightroom::migration_source::{SelectedCapture, SupplementPin};
    let selected = size_of::<SelectedCapture>().max(size_of::<String>());
    add(
        add(crate::lightroom::MANIFEST_BYTES, 1)?,
        mul(16_384, add(selected, size_of::<SupplementPin>())?)?,
    )
}

/// The three borrowed-key validation sets coexist. Each tree expression includes
/// its own insertion cascade, conservatively above the shared single-cascade
/// observation; these nodes do not own another copy of the borrowed strings.
pub(crate) fn seal_validation() -> Result<usize> {
    add(
        mul(2, tree::<&String, ()>(16_384)?)?,
        tree::<(&String, &String, &String), ()>(16_384)?,
    )
}

/// Source::admit retains both owned capture partitions during selected Manifest
/// validation. The SQL roster can reach its rejected 16,385th entry; expected
/// selected plus excluded is at most 16,384. Incremental insert avoids collect's
/// Vec/sort storage. Include one current candidate, even if it is a duplicate.
pub(crate) fn capture_partitions() -> Result<usize> {
    add(
        add(tree::<String, ()>(16_385)?, tree::<String, ()>(16_384)?)?,
        mul(64, add(add(16_385, 16_384)?, 1)?)?,
    )
}

/// Retained record graphs use a field map per record, not one aggregate map.
/// An arbitrary JSON map entry needs a string key, colon, nonempty value and
/// separator: at least five bytes. Allow the missing final separator per record.
/// This lower bound does not depend on the particular Field/Cell enum spelling.
pub(crate) fn record_dynamic(bytes: usize, records: usize) -> Result<usize> {
    use crate::lightroom::{migration_source::Field, plan::Cell};
    let fields = add(bytes, records)? / 5;
    // A nonempty per-record tree contributes its own root. Using all records
    // here also safely covers empty maps without relying on a measured shape.
    let settled = add(fields / 5, records)?;
    // At most one insertion is active; tree() supplies an entire conservative
    // cascade even if all entries happen to belong to that one map.
    let cascade = if fields == 0 {
        0
    } else {
        tree::<String, Field>(fields)?
            .checked_sub(mul(
                add(1, (fields - 1) / 5)?,
                tree_node::<String, Field>()?,
            )?)
            .context("retained record tree cascade subtraction")?
    };
    let nodes = add(mul(settled, tree_node::<String, Field>()?)?, cascade)?;
    // Each key array value consumes at least one byte plus a separator;
    // allow its missing final separator per record. The earlier finite graph
    // proof permits t <= bytes. Keep minima/growth terms explicit.
    add(
        add(
            mul(10, bytes)?,
            mul(
                add(mul(2, add(bytes, records)? / 2)?, mul(8, records)?)?,
                size_of::<Cell>(),
            )?,
        )?,
        nodes,
    )
}

pub(crate) fn saved_policy_dynamic(bytes: usize) -> Result<usize> {
    use crate::catalog_migration::importer::{ArtifactInput, SupplementInput};
    let artifact = size_of::<ArtifactInput>();
    let supplement = size_of::<SupplementInput>();
    // Direct public Policy grammar: 55*a+22*s<=bytes. Canonical post-hash
    // 212/73 counts must not be used for malformed/pre-hash allocation peaks.
    // 32*a accounts the two native unit-vector growth minima per artifact.
    let entries = maximum_ratio(
        bytes,
        &[(add(mul(2, artifact)?, 32)?, 55), (mul(2, supplement)?, 22)],
    )?;
    add(
        add(mul(5, bytes)?, entries)?,
        mul(8, add(artifact, supplement)?)?,
    )
}

/// Root-independent generic Content/container allowance. Owning Content moves
/// subtrees; `layers` conservatively charges vocabulary boundaries, not observed
/// copies. The caller must add its raw input, scratch, typed result and roots.
pub(crate) fn content_containers(bytes: usize, layers: usize) -> Result<usize> {
    mul(
        mul(
            mul(4, size_of::<serde::__private229::de::Content<'static>>())?,
            bytes,
        )?,
        layers,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn portable_layout_bounds_use_target_types_and_checked_extrema() -> Result<()> {
        assert_eq!(tree::<String, ()>(0)?, 0);
        assert!(tree::<String, ()>(12)? >= mul(3, tree_node::<String, ()>()?)?);
        assert!(tree::<String, ()>(usize::MAX).is_err());
        assert!(saved_policy_dynamic(usize::MAX).is_err());
        let raw = 8 * 1024 * 1024;
        let policy = saved_policy_dynamic(raw)?;
        assert!(policy >= 5 * raw);
        let manifest = manifest_dynamic()?;
        let issues = (crate::lightroom::MANIFEST_BYTES + 4) / 11;
        assert!(manifest >= issues * size_of::<crate::lightroom::Issue>());
        println!(
            "ADMISSION_LAYOUT manifest_dynamic={manifest} saved_policy_dynamic={policy} content_size={}",
            size_of::<serde::__private229::de::Content<'static>>()
        );
        Ok(())
    }
    #[test]
    fn phase_extrema_do_not_overflow_multiplication_before_division() -> Result<()> {
        assert_eq!(ceiling_product(usize::MAX, 1, 1)?, usize::MAX);
        assert_eq!(ceiling_product(usize::MAX, 2, 2)?, usize::MAX);
        assert!(ceiling_product(usize::MAX, 2, 1).is_err());
        assert_eq!(maximum_ratio(57, &[(5, 11), (2, 3)])?, 38);
        Ok(())
    }
}
