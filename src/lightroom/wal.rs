//! Validate retained WAL framing before SQLite is allowed to recover a private
//! working copy. Trailing uncommitted frames are retained and reported explicitly.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{fs::File, io::Read, path::Path};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalReport {
    pub bytes: u64,
    pub page_size: u32,
    pub valid_frames: u64,
    pub last_commit_frame: u64,
    pub uncommitted_frames: u64,
    pub trailing_bytes: u64,
    pub stale_frames: u64,
}
fn be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().unwrap())
}
fn checksum(bytes: &[u8], little: bool, mut sum: (u32, u32)) -> (u32, u32) {
    for pair in bytes.as_chunks::<8>().0 {
        let word = |b: &[u8]| {
            if little {
                u32::from_le_bytes(b.try_into().unwrap())
            } else {
                be(b)
            }
        };
        sum.0 = sum.0.wrapping_add(word(&pair[..4])).wrapping_add(sum.1);
        sum.1 = sum.1.wrapping_add(word(&pair[4..])).wrapping_add(sum.0);
    }
    sum
}
pub fn validate(path: &Path) -> Result<WalReport> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut result = WalReport {
        bytes: length,
        page_size: 0,
        valid_frames: 0,
        last_commit_frame: 0,
        uncommitted_frames: 0,
        trailing_bytes: 0,
        stale_frames: 0,
    };
    if length == 0 {
        return Ok(result);
    }
    ensure!(length >= 32, "truncated WAL header");
    let mut header = [0u8; 32];
    file.read_exact(&mut header)?;
    let magic = be(&header[..4]);
    ensure!(
        matches!(magic, 0x377f0682 | 0x377f0683),
        "unknown WAL magic"
    );
    ensure!(be(&header[4..8]) == 3_007_000, "unsupported WAL format");
    let page = be(&header[8..12]);
    ensure!(
        (512..=65536).contains(&page) && page.is_power_of_two(),
        "invalid WAL page size"
    );
    result.page_size = page;
    let little = magic == 0x377f0682;
    let mut sum = checksum(&header[..24], little, (0, 0));
    ensure!(
        sum == (be(&header[24..28]), be(&header[28..32])),
        "WAL header checksum mismatch"
    );
    let frame_bytes = u64::from(page) + 24;
    let frames = (length - 32) / frame_bytes;
    result.trailing_bytes = (length - 32) % frame_bytes;
    let mut frame = vec![0u8; frame_bytes as usize];
    let mut stale = false;
    for _ in 0..frames {
        file.read_exact(&mut frame)?;
        if frame[8..16] != header[16..24] {
            stale = true;
        }
        if stale {
            result.stale_frames += 1;
            continue;
        }
        ensure!(be(&frame[..4]) > 0, "invalid WAL page number");
        sum = checksum(&frame[..8], little, sum);
        sum = checksum(&frame[24..], little, sum);
        ensure!(
            sum == (be(&frame[16..20]), be(&frame[20..24])),
            "WAL frame checksum mismatch"
        );
        result.valid_frames += 1;
        if be(&frame[4..8]) > 0 {
            result.last_commit_frame = result.valid_frames;
        }
    }
    result.uncommitted_frames = result.valid_frames - result.last_commit_frame;
    // Torn tails are evidence of an interrupted writer, not automatically a
    // complete capture. The caller must report them and withhold consistency.
    Ok(result)
}
