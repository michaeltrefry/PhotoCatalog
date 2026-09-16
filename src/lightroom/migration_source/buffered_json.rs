//! Small grammar adapters for Serde Content-backed authority only. Ordinary
//! direct JSON never inherits Content's numeric identifiers or empty-map units.
use anyhow::Result;
use serde::{
    Deserialize,
    de::{self, DeserializeOwned, MapAccess, Visitor},
};
use serde_json::value::RawValue;
use std::{fmt, marker::PhantomData};

pub(crate) fn unit_enum<T: DeserializeOwned>(raw: &RawValue, buffered: bool) -> Result<T> {
    if !buffered || !raw.get().trim_start().starts_with('{') {
        return Ok(serde_json::from_str(raw.get())?);
    }
    struct Unit;
    impl<'de> Deserialize<'de> for Unit {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
            struct U;
            impl<'de> Visitor<'de> for U {
                type Value = Unit;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("unit or empty Content map")
                }
                fn visit_unit<E: de::Error>(self) -> std::result::Result<Unit, E> {
                    Ok(Unit)
                }
                fn visit_map<M: MapAccess<'de>>(
                    self,
                    mut m: M,
                ) -> std::result::Result<Unit, M::Error> {
                    if m.next_key::<de::IgnoredAny>()?.is_some() {
                        return Err(de::Error::custom("unit map is not empty"));
                    }
                    Ok(Unit)
                }
            }
            d.deserialize_any(U)
        }
    }
    struct E<T>(PhantomData<T>);
    impl<'de, T: DeserializeOwned> Visitor<'de> for E<T> {
        type Value = T;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("one unit variant")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut m: M) -> std::result::Result<T, M::Error> {
            let name = m
                .next_key::<String>()?
                .ok_or_else(|| de::Error::custom("empty enum map"))?;
            let out = T::deserialize(de::value::StrDeserializer::<M::Error>::new(&name))?;
            m.next_value::<Unit>()?;
            if m.next_key::<de::IgnoredAny>()?.is_some() {
                return Err(de::Error::custom("enum map has multiple variants"));
            }
            Ok(out)
        }
    }
    let mut d = serde_json::Deserializer::from_str(raw.get());
    let out = serde::Deserializer::deserialize_map(&mut d, E::<T>(PhantomData))?;
    d.end()?;
    Ok(out)
}
/// Content::U64 can be a variant identifier; JSON's direct identifier reader
/// accepts only strings. Only the two NativePath indices are admitted here.
pub(crate) fn path_identifier(raw: &RawValue) -> Result<usize> {
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = usize;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("NativePath variant identifier")
        }
        fn visit_str<E: de::Error>(self, s: &str) -> std::result::Result<usize, E> {
            match s {
                "UnixBytes" => Ok(0),
                "WindowsWide" => Ok(1),
                _ => Err(E::custom("unknown NativePath variant")),
            }
        }
        fn visit_u64<E: de::Error>(self, n: u64) -> std::result::Result<usize, E> {
            match n {
                0 => Ok(0),
                1 => Ok(1),
                _ => Err(E::custom("NativePath variant index out of range")),
            }
        }
    }
    let mut d = serde_json::Deserializer::from_str(raw.get());
    let out = serde::Deserializer::deserialize_any(&mut d, V)?;
    d.end()?;
    Ok(out)
}
