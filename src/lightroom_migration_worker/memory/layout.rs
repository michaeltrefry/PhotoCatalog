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
    let pointer = size_of::<usize>();
    let alignment = align_of::<usize>()
        .max(align_of::<u16>())
        .max(align_of::<K>())
        .max(align_of::<V>());
    let leaf = add(
        add(pointer, 4)?,
        add(
            mul(11, add(size_of::<K>(), size_of::<V>())?)?,
            mul(5, alignment - 1)?,
        )?,
    )?;
    add(leaf, add(mul(12, pointer)?, mul(2, alignment - 1)?)?)
}
/// Settled occupancy plus an entire insertion split cascade. Empty trees have
/// no allocation. Multiple trees must be charged individually, including roots.
pub(crate) fn tree<K, V>(entries: usize) -> Result<usize> {
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
    mul(add(settled, add(height, 2)?)?, tree_node::<K, V>()?)
}

pub(crate) fn manifest_dynamic() -> Result<usize> {
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
        add(crate::lightroom::MANIFEST_BYTES, 4)?,
        &[
            (size_of::<Artifact>(), 57),
            (size_of::<Entry>(), 49),
            (size_of::<NativePath>(), 17),
            (size_of::<Issue>(), 11),
            (2, 1),
        ],
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
