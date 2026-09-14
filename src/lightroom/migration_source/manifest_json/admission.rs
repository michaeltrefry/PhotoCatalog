//! Allocation admission for the private Source view. No per-member span vector.
use anyhow::{Result, ensure};
use serde::{
    Deserialize,
    de::{self, DeserializeSeed, SeqAccess, Visitor},
};
use serde_json::value::RawValue;
use std::{fmt, marker::PhantomData};

#[derive(Default)]
pub(super) struct Footprint {
    floor: usize,
}
impl Footprint {
    pub(super) fn member(&mut self, bytes: usize) -> Result<()> {
        self.floor = self
            .floor
            .checked_add(bytes)
            .ok_or_else(|| anyhow::anyhow!("source footprint overflow"))?;
        self.check()
    }
    pub(super) fn check(&self) -> Result<()> {
        ensure!(
            self.floor <= crate::lightroom::MANIFEST_BYTES + 4,
            "source manifest representation exceeds retained byte limit"
        );
        Ok(())
    }
    pub(super) fn strings(&mut self, values: &[&str]) -> Result<()> {
        for value in values {
            self.member(value.len())?;
        }
        Ok(())
    }
    pub(super) fn path(&mut self, count: usize, _width: usize) -> Result<()> {
        self.member(
            count
                .checked_mul(2)
                .ok_or_else(|| anyhow::anyhow!("source unit count overflow"))?
                .saturating_sub(1),
        )
    }
}

pub(crate) fn array<'a>(
    raw: &'a RawValue,
    stop: &dyn Fn() -> bool,
    mut visit: impl FnMut(&'a RawValue) -> Result<()>,
) -> Result<usize> {
    struct Walk<'a, 'b, F> {
        stop: &'b dyn Fn() -> bool,
        visit: &'b mut F,
        lifetime: PhantomData<&'a ()>,
    }
    impl<'de, F: FnMut(&'de RawValue) -> Result<()>> Visitor<'de> for Walk<'de, '_, F> {
        type Value = usize;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("source array")
        }
        fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> std::result::Result<usize, S::Error> {
            let mut n = 0usize;
            loop {
                if (self.stop)() {
                    return Err(de::Error::custom("source decoding canceled"));
                }
                let Some(raw) = seq.next_element::<&RawValue>()? else {
                    break;
                };
                (self.visit)(raw).map_err(de::Error::custom)?;
                n = n
                    .checked_add(1)
                    .ok_or_else(|| de::Error::custom("source member count overflow"))?;
            }
            Ok(n)
        }
    }
    let mut d = serde_json::Deserializer::from_str(raw.get());
    let n = serde::Deserializer::deserialize_seq(
        &mut d,
        Walk {
            stop,
            visit: &mut visit,
            lifetime: PhantomData,
        },
    )?;
    d.end()?;
    Ok(n)
}

pub(crate) fn units<'a, T: Deserialize<'a>>(
    raw: &'a RawValue,
    stop: &dyn Fn() -> bool,
    retain: Option<usize>,
) -> Result<(usize, Vec<T>)> {
    units_bounded(raw, stop, retain, usize::MAX)
}

pub(crate) fn units_bounded<'a, T: Deserialize<'a>>(
    raw: &'a RawValue,
    stop: &dyn Fn() -> bool,
    retain: Option<usize>,
    maximum: usize,
) -> Result<(usize, Vec<T>)> {
    struct Units<'a, T> {
        stop: &'a dyn Fn() -> bool,
        retain: Option<usize>,
        maximum: usize,
        marker: PhantomData<T>,
    }
    impl<'de, T: Deserialize<'de>> DeserializeSeed<'de> for Units<'_, T> {
        type Value = (usize, Vec<T>);
        fn deserialize<D: serde::Deserializer<'de>>(
            self,
            d: D,
        ) -> std::result::Result<Self::Value, D::Error> {
            d.deserialize_seq(self)
        }
    }
    impl<'de, T: Deserialize<'de>> Visitor<'de> for Units<'_, T> {
        type Value = (usize, Vec<T>);
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("native units")
        }
        fn visit_seq<S: SeqAccess<'de>>(
            self,
            mut seq: S,
        ) -> std::result::Result<Self::Value, S::Error> {
            let mut out = Vec::new();
            if let Some(n) = self.retain {
                out.try_reserve_exact(n).map_err(de::Error::custom)?;
            }
            let mut count = 0usize;
            loop {
                if (self.stop)() {
                    return Err(de::Error::custom("source decoding canceled"));
                }
                let Some(unit) = seq.next_element::<T>()? else {
                    break;
                };
                count = count
                    .checked_add(1)
                    .ok_or_else(|| de::Error::custom("source unit count overflow"))?;
                if count > self.maximum {
                    return Err(de::Error::custom("source unit count admission"));
                }
                if let Some(n) = self.retain {
                    if count > n {
                        return Err(de::Error::custom("source unit count changed"));
                    }
                    out.push(unit);
                }
            }
            if self.retain.is_some_and(|n| n != count) {
                return Err(de::Error::custom("source unit count changed"));
            }
            Ok((count, out))
        }
    }
    let mut d = serde_json::Deserializer::from_str(raw.get());
    let result = Units {
        stop,
        retain,
        maximum,
        marker: PhantomData,
    }
    .deserialize(&mut d)?;
    d.end()?;
    Ok(result)
}
