//! Decoded pixels retain their memory reservation until the final consumer drops
//! them. Evicting an LRU entry alone cannot make externally held pixels free.
use super::{Codec, PreparedRgb, decode, encoded_dimensions};
use anyhow::{Result, ensure};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct ByteBudget(Arc<Mutex<BudgetState>>);
struct BudgetState {
    limit: u64,
    used: u64,
}
pub struct ByteReservation {
    budget: ByteBudget,
    bytes: u64,
}
impl ByteBudget {
    pub fn new(limit: u64) -> Result<Self> {
        ensure!(limit > 0, "zero memory allowance");
        Ok(Self(Arc::new(Mutex::new(BudgetState { limit, used: 0 }))))
    }
    pub fn used(&self) -> u64 {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).used
    }
    pub fn try_reserve(&self, bytes: u64) -> Option<ByteReservation> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if bytes > state.limit - state.used {
            return None;
        }
        state.used += bytes;
        Some(ByteReservation {
            budget: self.clone(),
            bytes,
        })
    }
}
impl Drop for ByteReservation {
    fn drop(&mut self) {
        self.budget.0.lock().unwrap_or_else(|e| e.into_inner()).used -= self.bytes;
    }
}
/// Admission pressure is not corrupt cache data and must never invalidate the
/// only retained offline thumbnail.
#[derive(Debug)]
pub struct DecodedBudgetExceeded;
impl std::fmt::Display for DecodedBudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "decoded allowance retained by visible consumers; release an old page before decoding",
        )
    }
}
impl std::error::Error for DecodedBudgetExceeded {}
pub struct RetainedPixels {
    pixels: PreparedRgb,
    _reservation: ByteReservation,
}
impl RetainedPixels {
    pub fn pixels(&self) -> &PreparedRgb {
        &self.pixels
    }
}
pub struct DecodedCache {
    budget: ByteBudget,
    cache_limit: u64,
    max_entries: usize,
    cached_bytes: u64,
    clock: u64,
    entries: HashMap<String, (Arc<RetainedPixels>, u64)>,
}
impl DecodedCache {
    /// total_live_bytes includes both cached and caller-held pages. A caller that
    /// keeps old pages can exhaust admission, but cannot silently bypass it.
    pub fn new(cache_bytes: u64, total_live_bytes: u64, max_entries: usize) -> Result<Self> {
        ensure!(
            max_entries > 0 && max_entries <= 100_000,
            "decoded entry limit"
        );
        ensure!(
            cache_bytes <= total_live_bytes,
            "LRU exceeds total decoded allowance"
        );
        Ok(Self {
            budget: ByteBudget::new(total_live_bytes)?,
            cache_limit: cache_bytes,
            max_entries,
            cached_bytes: 0,
            clock: 0,
            entries: HashMap::new(),
        })
    }
    pub fn live_bytes(&self) -> u64 {
        self.budget.used()
    }
    pub fn cached_bytes(&self) -> u64 {
        self.cached_bytes
    }
    fn tick(&mut self) -> Result<u64> {
        self.clock = self
            .clock
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("LRU clock overflow"))?;
        Ok(self.clock)
    }
    pub fn get(&mut self, key: &str) -> Result<Option<Arc<RetainedPixels>>> {
        let tick = self.tick()?;
        Ok(self.entries.get_mut(key).map(|(pixels, touched)| {
            *touched = tick;
            pixels.clone()
        }))
    }
    fn evict_one(&mut self) -> bool {
        let key = self
            .entries
            .iter()
            .min_by_key(|(_, (_, tick))| *tick)
            .map(|(key, _)| key.clone());
        if let Some(key) = key {
            let (pixels, _) = self.entries.remove(&key).unwrap();
            self.cached_bytes -= pixels.pixels.byte_len() as u64;
            true
        } else {
            false
        }
    }
    pub fn clear(&mut self) {
        self.entries.clear();
        self.cached_bytes = 0;
    }
    /// The manifest supplies exact dimensions, checked again after decoding.
    /// Worker admission separately covers codec scratch and encoded buffers.
    pub fn decode(
        &mut self,
        key: String,
        encoded: &[u8],
        codec: Codec,
        width: u32,
        height: u32,
    ) -> Result<Arc<RetainedPixels>> {
        ensure!(!key.is_empty() && key.len() <= 64, "decoded key length");
        if let Some(pixels) = self.get(&key)? {
            return Ok(pixels);
        }
        ensure!(
            width > 0 && height > 0 && width <= 8192 && height <= 8192,
            "invalid decoded dimensions"
        );
        ensure!(
            encoded_dimensions(encoded, codec)? == (width, height),
            "cached header dimensions mismatch before pixel allocation"
        );
        let bytes = u64::from(width) * u64::from(height) * 3;
        let reservation = loop {
            if let Some(reservation) = self.budget.try_reserve(bytes) {
                break reservation;
            }
            if !self.evict_one() {
                return Err(DecodedBudgetExceeded.into());
            }
        };
        let pixels = decode(encoded, codec)?;
        ensure!(
            pixels.width() == width && pixels.height() == height,
            "cached dimensions mismatch"
        );
        let retained = Arc::new(RetainedPixels {
            pixels,
            _reservation: reservation,
        });
        if bytes <= self.cache_limit {
            while self.cached_bytes > self.cache_limit - bytes
                || self.entries.len() >= self.max_entries
            {
                if !self.evict_one() {
                    break;
                }
            }
            let tick = self.tick()?;
            self.cached_bytes += bytes;
            self.entries.insert(key, (retained.clone(), tick));
        }
        Ok(retained)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::{CodecSettings, encode};
    #[test]
    fn external_references_keep_memory_charged_after_lru_eviction() {
        let rgb = PreparedRgb::new(2, 2, vec![128; 12]).unwrap();
        let encoded = encode(
            &rgb,
            CodecSettings {
                codec: Codec::Jpeg,
                quality: 65,
            },
            None,
        )
        .unwrap();
        let mut cache = DecodedCache::new(12, 24, 2).unwrap();
        let first = cache
            .decode("a".into(), &encoded, Codec::Jpeg, 2, 2)
            .unwrap();
        let second = cache
            .decode("b".into(), &encoded, Codec::Jpeg, 2, 2)
            .unwrap();
        assert_eq!(cache.cached_bytes(), 12);
        assert_eq!(cache.live_bytes(), 24);
        assert!(
            cache
                .decode("c".into(), &encoded, Codec::Jpeg, 2, 2)
                .is_err()
        );
        assert_eq!(cache.cached_bytes(), 0);
        assert_eq!(cache.live_bytes(), 24);
        drop(first);
        let third = cache
            .decode("c".into(), &encoded, Codec::Jpeg, 2, 2)
            .unwrap();
        cache.clear();
        drop(second);
        drop(third);
        assert_eq!(cache.live_bytes(), 0);
    }
    #[test]
    fn mismatched_header_is_rejected_before_pixel_reservation_for_every_codec() {
        let rgb = PreparedRgb::new(64, 32, vec![128; 64 * 32 * 3]).unwrap();
        for codec in [Codec::Jpeg, Codec::Webp, Codec::Avif] {
            let encoded = encode(&rgb, CodecSettings { codec, quality: 80 }, None).unwrap();
            let mut cache = DecodedCache::new(3, 3, 1).unwrap();
            let error = cache
                .decode("mismatch".into(), &encoded, codec, 1, 1)
                .err()
                .unwrap();
            assert!(error.to_string().contains("before pixel allocation"));
            assert_eq!(cache.live_bytes(), 0);
            assert_eq!(cache.cached_bytes(), 0);
        }
    }
    #[test]
    fn failed_decode_releases_reserved_memory() {
        let mut cache = DecodedCache::new(12, 12, 2).unwrap();
        assert!(
            cache
                .decode("bad".into(), b"bad", Codec::Jpeg, 2, 2)
                .is_err()
        );
        assert_eq!(cache.live_bytes(), 0);
    }
}
