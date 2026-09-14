//! Borrowed opening envelope. No paths are opened until all complete authority
//! bytes, typed fields and bounded rosters have passed admission.
use super::transport::{AUTHORITY_BYTES, Authority, RawLimits, SqlLimits};
use crate::{
    lightroom::migration_source::seal_json, lightroom_migration_worker::identity::FileKey,
};
use anyhow::{Result, ensure};
use serde::{
    Deserialize,
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use serde_json::value::RawValue;
use std::fmt;

#[derive(Deserialize)]
enum Mode {
    Sql,
    Artifact,
}
struct Envelope<'a> {
    mode: &'a RawValue,
    body: &'a RawValue,
    sequence: bool,
    buffered: bool,
}
impl<'de: 'a, 'a> Deserialize<'de> for Envelope<'a> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Envelope<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("source authority")
            }
            fn visit_seq<S: SeqAccess<'de>>(
                self,
                mut s: S,
            ) -> std::result::Result<Self::Value, S::Error> {
                let mode = s
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let body = s
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                if s.next_element::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::invalid_length(3, &self));
                }
                Ok(Envelope {
                    mode,
                    body,
                    sequence: true,
                    buffered: false,
                })
            }
            fn visit_map<M: MapAccess<'de>>(
                self,
                mut m: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                #[derive(Deserialize)]
                #[serde(field_identifier)]
                enum K {
                    #[serde(rename = "mode")]
                    Mode,
                    #[serde(rename = "authority")]
                    Authority,
                }
                let (mut mode, mut body, mut buffered) = (None, None, false);
                while let Some(k) = m.next_key()? {
                    match k {
                        K::Mode => {
                            if mode.is_some() {
                                return Err(de::Error::duplicate_field("mode"));
                            }
                            mode = Some(m.next_value()?);
                        }
                        K::Authority => {
                            if body.is_some() {
                                return Err(de::Error::duplicate_field("authority"));
                            }
                            buffered = mode.is_none();
                            body = Some(m.next_value()?);
                        }
                    }
                }
                Ok(Envelope {
                    mode: mode.ok_or_else(|| de::Error::missing_field("mode"))?,
                    body: body.ok_or_else(|| de::Error::missing_field("authority"))?,
                    sequence: false,
                    buffered,
                })
            }
        }
        d.deserialize_struct("Authority", &["mode", "authority"], V)
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sql<'a> {
    #[serde(borrow)]
    seal: &'a RawValue,
    limits: SqlLimits,
    #[serde(borrow)]
    protected: &'a RawValue,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact<'a> {
    #[serde(borrow)]
    descriptor: &'a RawValue,
    limits: RawLimits,
    #[serde(borrow)]
    protected: &'a RawValue,
}
// The old adjacent-tag decoder buffered content-before-mode through Content.
// Validate that scalar/recursion grammar without retaining Content's full tree.
// This is not serde_json::Value: its special RawValue key has no meaning here.
#[derive(Clone, Copy)]
struct Content<'a> {
    depth: usize,
    stop: &'a dyn Fn() -> bool,
}
impl<'de> DeserializeSeed<'de> for Content<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> std::result::Result<(), D::Error> {
        if (self.stop)() {
            return Err(de::Error::custom("source authority canceled"));
        }
        d.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Content<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("buffered JSON scalar grammar")
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: de::Error>(self) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_seq<S: SeqAccess<'de>>(self, mut s: S) -> std::result::Result<(), S::Error> {
        if self.depth >= 127 {
            return Err(de::Error::custom("recursion limit exceeded"));
        }
        while s
            .next_element_seed(Self {
                depth: self.depth + 1,
                ..self
            })?
            .is_some()
        {}
        Ok(())
    }
    fn visit_map<M: MapAccess<'de>>(self, mut m: M) -> std::result::Result<(), M::Error> {
        if self.depth >= 127 {
            return Err(de::Error::custom("recursion limit exceeded"));
        }
        while m
            .next_key_seed(Self {
                depth: self.depth + 1,
                ..self
            })?
            .is_some()
        {
            m.next_value_seed(Self {
                depth: self.depth + 1,
                ..self
            })?;
        }
        Ok(())
    }
}
fn protected(raw: &RawValue, stop: &dyn Fn() -> bool) -> Result<Vec<FileKey>> {
    seal_json::roster(raw, 4096, stop, |_| Ok(()))
}
pub(super) fn decode(bytes: &[u8], stop: &dyn Fn() -> bool) -> Result<Authority> {
    ensure!(
        bytes.len() <= AUTHORITY_BYTES && !stop(),
        "source authority admission/canceled"
    );
    let e: Envelope<'_> = serde_json::from_slice(bytes)?;
    let mode = if e.sequence {
        match serde_json::from_str::<String>(e.mode.get())?.as_str() {
            "Sql" => Mode::Sql,
            "Artifact" => Mode::Artifact,
            _ => anyhow::bail!("unknown source authority mode"),
        }
    } else {
        serde_json::from_str(e.mode.get())?
    };
    if e.buffered {
        let mut d = serde_json::Deserializer::from_str(e.body.get());
        Content { depth: 1, stop }.deserialize(&mut d)?;
        d.end()?;
    }
    ensure!(
        e.body.get().trim_start().starts_with('{'),
        "authority struct variant requires a map"
    );
    let out = match mode {
        Mode::Sql => {
            let s: Sql<'_> = serde_json::from_str(e.body.get())?;
            let seal =
                seal_json::decode_mode(s.seal.get().as_bytes(), AUTHORITY_BYTES, stop, e.buffered)?;
            // ContentDeserializer does not implement deserialize_u128, even for
            // a small Some timestamp. Preserve that prior rejection explicitly.
            ensure!(
                !e.buffered
                    || (seal.identity.modified_ns.is_none()
                        && seal
                            .supplements
                            .iter()
                            .all(|s| s.source_revision.modified_unix_ns.is_none())),
                "buffered source authority cannot deserialize u128"
            );
            Authority::Sql {
                seal,
                limits: s.limits,
                protected: protected(s.protected, stop)?,
            }
        }
        Mode::Artifact => {
            let a: Artifact<'_> = serde_json::from_str(e.body.get())?;
            let descriptor = crate::catalog_migration::artifacts::descriptor_json::decode_mode(
                a.descriptor.get().as_bytes(),
                AUTHORITY_BYTES,
                stop,
                e.buffered,
            )?;
            ensure!(
                !e.buffered
                    || (descriptor
                        .request
                        .mapping
                        .copy_identity
                        .modified_ns
                        .is_none()
                        && descriptor.artifact.revision.modified_ns.is_none()),
                "buffered source authority cannot deserialize u128"
            );
            Authority::Artifact {
                descriptor,
                limits: a.limits,
                protected: protected(a.protected, stop)?,
            }
        }
    };
    ensure!(!stop(), "source authority canceled");
    Ok(out)
}
#[cfg(test)]
mod tests;
