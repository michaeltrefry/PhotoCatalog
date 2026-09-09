#[path = "../src/xmp_packets.rs"]
mod xmp_packets;
use std::io::Write;
use xmp_packets::{
    Container, Inspection, Limits, Status, Transformation, inspect, inspect_sidecar,
};

fn inspect_bytes(bytes: &[u8], limits: &Limits) -> Inspection {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("雪 photo.bin");
    std::fs::write(&path, bytes).unwrap();
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();
    let result = inspect(&path, limits).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes, "source bytes changed");
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        before
    );
    assert_eq!(
        std::fs::read_dir(directory.path()).unwrap().count(),
        1,
        "source directory changed"
    );
    if bytes.len() as u64 <= limits.max_source_bytes {
        assert_eq!(
            result.revision.blake3,
            blake3::hash(bytes).to_hex().to_string()
        );
    }
    for packet in &result.packets {
        let reconstructed: Vec<u8> = packet
            .ranges
            .iter()
            .flat_map(|range| {
                bytes[range.offset as usize..(range.offset + range.length) as usize]
                    .iter()
                    .copied()
            })
            .collect();
        assert_eq!(
            reconstructed, packet.bytes,
            "carrier does not match its source ranges"
        );
        assert_eq!(
            packet.blake3,
            blake3::hash(&packet.bytes).to_hex().to_string()
        );
    }
    result
}
fn run(bytes: &[u8]) -> Inspection {
    inspect_bytes(bytes, &Limits::default())
}
fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0xff, marker];
    bytes.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}
fn jpeg(segments: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = vec![0xff, 0xd8];
    for segment in segments {
        bytes.extend_from_slice(segment);
    }
    bytes.extend_from_slice(&[0xff, 0xd9]);
    bytes
}
fn main_packet(xml: &[u8]) -> Vec<u8> {
    let mut bytes = b"http://ns.adobe.com/xap/1.0/\0".to_vec();
    bytes.extend_from_slice(xml);
    segment(0xe1, &bytes)
}
fn extended(guid: &str, total: u32, offset: u32, fragment: &[u8]) -> Vec<u8> {
    let mut bytes = b"http://ns.adobe.com/xmp/extension/\0".to_vec();
    bytes.extend_from_slice(guid.as_bytes());
    bytes.extend_from_slice(&total.to_be_bytes());
    bytes.extend_from_slice(&offset.to_be_bytes());
    bytes.extend_from_slice(fragment);
    segment(0xe1, &bytes)
}
#[test]
fn jpeg_retains_duplicate_main_packets_and_post_scan_metadata() {
    let xml = b"\xff\xfe<\0x\0/\0>\0";
    let bytes = jpeg(&[
        main_packet(xml),
        segment(0xda, &[]),
        vec![1, 0xff, 0, 2, 0xff, 0xd3, 3],
        main_packet(xml),
    ]);
    let result = run(&bytes);
    assert_eq!(result.status, Status::Complete);
    assert_eq!(result.packets.len(), 2);
    assert_eq!(result.parse_inputs.len(), 2);
    assert!(result.parse_inputs.iter().all(|p| p.bytes == xml));
    assert!(
        result
            .packets
            .iter()
            .all(|p| p.container == Container::JpegMain)
    );
}
#[test]
fn extended_jpeg_validates_order_and_digest_without_erasing_fragments() {
    let xml = b"<unknown:array>opaque</unknown:array>";
    let guid = format!("{:X}", md5::compute(xml));
    let result = run(&jpeg(&[
        main_packet(b"main"),
        extended(&guid, xml.len() as u32, 8, &xml[8..]),
        extended(&guid, xml.len() as u32, 0, &xml[..8]),
    ]));
    assert_eq!(result.status, Status::Complete);
    assert_eq!(result.packets.len(), 3);
    assert_eq!(result.parse_inputs[1].bytes, xml);
    assert_eq!(result.parse_inputs[1].packet_indices, [2, 1]);
    assert_eq!(
        result.parse_inputs[1].transformation,
        Transformation::JpegExtendedReassembled
    );
    assert_eq!(result.packets[1].attributes["guid"], guid);
}
#[test]
fn extended_jpeg_rejects_gap_overlap_duplicate_bad_guid_and_inconsistent_length() {
    let xml = b"abcdef";
    let guid = format!("{:X}", md5::compute(xml));
    let cases = [
        vec![extended(&guid, 6, 0, b"abc"), extended(&guid, 6, 4, b"ef")],
        vec![
            extended(&guid, 6, 0, b"abcd"),
            extended(&guid, 6, 3, b"def"),
        ],
        vec![extended(&guid, 6, 0, xml), extended(&guid, 6, 0, xml)],
        vec![extended("00000000000000000000000000000000", 6, 0, xml)],
        vec![extended("XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX", 6, 0, xml)],
        vec![extended(&guid, 6, 0, b"abc"), extended(&guid, 7, 3, b"def")],
        vec![extended(&guid, u32::MAX, u32::MAX, b"x")],
    ];
    for segments in cases {
        let result = run(&jpeg(&segments));
        assert_eq!(result.status, Status::Malformed);
        assert_eq!(result.packets.len(), segments.len());
        assert!(result.parse_inputs.is_empty());
    }
}
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
fn png_chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut bytes = (payload.len() as u32).to_be_bytes().to_vec();
    bytes.extend_from_slice(kind);
    bytes.extend_from_slice(payload);
    let crc = crc32(&bytes[4..]);
    bytes.extend_from_slice(&crc.to_be_bytes());
    bytes
}
fn png(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    for chunk in chunks {
        bytes.extend_from_slice(chunk);
    }
    bytes.extend_from_slice(&png_chunk(b"IEND", &[]));
    bytes
}
fn itxt(xml: &[u8], compressed: bool) -> Vec<u8> {
    let mut bytes = b"XML:com.adobe.xmp\0".to_vec();
    bytes.extend_from_slice(&[u8::from(compressed), 0, 0, 0]);
    if compressed {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(xml).unwrap();
        bytes.extend_from_slice(&encoder.finish().unwrap());
    } else {
        bytes.extend_from_slice(xml);
    }
    png_chunk(b"iTXt", &bytes)
}
#[test]
fn png_retains_all_carriers_and_decompresses_separately() {
    let xml = b"\0raw\xffunknown\0";
    let result = run(&png(&[
        itxt(xml, false),
        itxt(xml, true),
        png_chunk(b"iTXt", b"not-XML:com.adobe.xmp\0\0\0\0\0ignored"),
    ]));
    assert_eq!(result.status, Status::Complete);
    assert_eq!(result.packets.len(), 2);
    assert_eq!(result.parse_inputs.len(), 2);
    assert!(result.parse_inputs.iter().all(|p| p.bytes == xml));
    assert_eq!(
        result.parse_inputs[1].transformation,
        Transformation::ZlibDecompressed
    );
    assert_ne!(result.packets[0].bytes, result.packets[1].bytes);
}
#[test]
fn png_bad_crc_or_compression_retains_carrier_without_parse_input() {
    let mut bad_crc = itxt(b"x", false);
    *bad_crc.last_mut().unwrap() ^= 1;
    for chunk in [
        bad_crc,
        png_chunk(b"iTXt", b"XML:com.adobe.xmp\0\x01\0\0\0not-zlib"),
        png_chunk(b"iTXt", b"XML:com.adobe.xmp\0\0"),
    ] {
        let result = run(&png(&[chunk]));
        assert_eq!(result.status, Status::Malformed);
        assert_eq!(result.packets.len(), 1);
        assert!(result.parse_inputs.is_empty());
    }
}
#[test]
fn compressed_png_resource_limit_is_explicit() {
    let result = inspect_bytes(
        &png(&[itxt(&vec![b'x'; 4096], true)]),
        &Limits {
            max_packet_bytes: 256,
            ..Limits::default()
        },
    );
    assert_eq!(result.status, Status::ResourceLimit);
    assert_eq!(result.packets.len(), 1);
    assert!(result.parse_inputs.is_empty());
}
fn tiff_two() -> Vec<u8> {
    let mut bytes = b"II*\0\x08\0\0\0".to_vec();
    for (next, value) in [(26u32, *b"a\0\xffb"), (0u32, *b"c\0\xfed")] {
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&700u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&value);
        bytes.extend_from_slice(&next.to_le_bytes());
    }
    bytes
}
#[test]
fn tiff_cr2_dng_and_raw_family_directories_retain_inline_bytes() {
    for magic in [42u16, 85, 0x4f52, 0x5352] {
        let mut bytes = tiff_two();
        bytes[2..4].copy_from_slice(&magic.to_le_bytes());
        let result = run(&bytes);
        assert_eq!(result.status, Status::Complete);
        assert_eq!(result.packets.len(), 2);
        assert_eq!(result.parse_inputs[0].bytes, b"a\0\xffb");
    }
}
#[test]
fn tiff_cycle_truncation_and_directory_limit_are_explicit() {
    let mut cycle = tiff_two();
    cycle[40..44].copy_from_slice(&8u32.to_le_bytes());
    let result = run(&cycle);
    assert_eq!(result.status, Status::Malformed);
    assert_eq!(result.packets.len(), 2);
    let result = run(&tiff_two()[..42]);
    assert_eq!(result.status, Status::Malformed);
    assert_eq!(result.packets.len(), 1);
    let result = inspect_bytes(
        &tiff_two(),
        &Limits {
            max_entries: 1,
            ..Limits::default()
        },
    );
    assert_eq!(result.status, Status::ResourceLimit);
}
#[test]
fn big_endian_bigtiff_and_subifd_are_read_without_pixels() {
    let mut bytes = b"MM\0+\0\x08\0\0".to_vec();
    bytes.extend_from_slice(&16u64.to_be_bytes());
    bytes.extend_from_slice(&1u64.to_be_bytes());
    bytes.extend_from_slice(&700u16.to_be_bytes());
    bytes.extend_from_slice(&7u16.to_be_bytes());
    bytes.extend_from_slice(&8u64.to_be_bytes());
    bytes.extend_from_slice(b"raw\0\xff\xfe!!");
    bytes.extend_from_slice(&0u64.to_be_bytes());
    let result = run(&bytes);
    assert_eq!(result.status, Status::Complete);
    assert_eq!(result.parse_inputs[0].bytes, b"raw\0\xff\xfe!!");
    let mut bytes = tiff_two();
    bytes[10..12].copy_from_slice(&330u16.to_le_bytes());
    bytes[12..14].copy_from_slice(&4u16.to_le_bytes());
    bytes[14..18].copy_from_slice(&1u32.to_le_bytes());
    bytes[18..22].copy_from_slice(&26u32.to_le_bytes());
    bytes[22..26].fill(0);
    let result = run(&bytes);
    assert_eq!(result.status, Status::Complete);
    assert_eq!(result.packets.len(), 1);
}
#[test]
fn tiff_truncated_payload_retains_available_bytes() {
    let mut bytes = tiff_two()[..26].to_vec();
    bytes[14..18].copy_from_slice(&100u32.to_le_bytes());
    bytes[18..22].copy_from_slice(&26u32.to_le_bytes());
    bytes[22..26].fill(0);
    bytes.extend_from_slice(b"partial");
    let result = run(&bytes);
    assert_eq!(result.status, Status::Malformed);
    assert_eq!(result.packets[0].bytes, b"partial");
    assert!(result.parse_inputs.is_empty());
}
fn webp(chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
    let mut bytes = b"RIFF\0\0\0\0WEBP".to_vec();
    for (kind, payload) in chunks {
        bytes.extend_from_slice(*kind);
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
        if payload.len() % 2 != 0 {
            bytes.push(0);
        }
    }
    let length = bytes.len() as u32 - 8;
    bytes[4..8].copy_from_slice(&length.to_le_bytes());
    bytes
}
fn psd(payloads: &[&[u8]]) -> Vec<u8> {
    let mut resources = Vec::new();
    for payload in payloads {
        resources.extend_from_slice(b"8BIM");
        resources.extend_from_slice(&1060u16.to_be_bytes());
        resources.extend_from_slice(&[0, 0]);
        resources.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        resources.extend_from_slice(payload);
        if payload.len() % 2 != 0 {
            resources.push(0);
        }
    }
    let mut bytes = vec![0; 26];
    bytes[..4].copy_from_slice(b"8BPS");
    bytes[4..6].copy_from_slice(&1u16.to_be_bytes());
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes.extend_from_slice(&(resources.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&resources);
    bytes
}
#[test]
fn webp_and_psd_retain_multiple_packets_in_source_order() {
    for bytes in [
        webp(&[(b"XMP ", b"one"), (b"XMP ", b"two!")]),
        psd(&[b"one", b"two!"]),
    ] {
        let result = run(&bytes);
        assert_eq!(result.status, Status::Complete);
        assert_eq!(
            result
                .parse_inputs
                .iter()
                .map(|p| p.bytes.as_slice())
                .collect::<Vec<_>>(),
            [b"one".as_slice(), b"two!".as_slice()]
        );
    }
}
fn bmff_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut bytes = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    bytes.extend_from_slice(kind);
    bytes.extend_from_slice(payload);
    bytes
}
fn avif(xmls: &[&[u8]], method: u16) -> Vec<u8> {
    avif_encoded(xmls, method, b"")
}
fn avif_encoded(xmls: &[&[u8]], method: u16, encoding: &[u8]) -> Vec<u8> {
    let mut iinf = vec![0, 0, 0, 0];
    iinf.extend_from_slice(&(xmls.len() as u16).to_be_bytes());
    let mut iloc = vec![1, 0, 0, 0, 0x44, 0];
    iloc.extend_from_slice(&(xmls.len() as u16).to_be_bytes());
    let mut idat = Vec::new();
    let mut iref = vec![0, 0, 0, 0];
    for (index, xml) in xmls.iter().enumerate() {
        let id = index as u16 + 1;
        let mut infe = vec![2, 0, 0, 0];
        infe.extend_from_slice(&id.to_be_bytes());
        infe.extend_from_slice(&0u16.to_be_bytes());
        infe.extend_from_slice(b"mimeXMP\0application/rdf+xml\0");
        infe.extend_from_slice(encoding);
        infe.push(0);
        iinf.extend_from_slice(&bmff_box(b"infe", &infe));
        iloc.extend_from_slice(&id.to_be_bytes());
        iloc.extend_from_slice(&method.to_be_bytes());
        iloc.extend_from_slice(&0u16.to_be_bytes());
        iloc.extend_from_slice(&2u16.to_be_bytes());
        let split = xml.len() / 2;
        for part in [&xml[..split], &xml[split..]] {
            iloc.extend_from_slice(&(idat.len() as u32).to_be_bytes());
            iloc.extend_from_slice(&(part.len() as u32).to_be_bytes());
            idat.extend_from_slice(part);
        }
        let mut cdsc = id.to_be_bytes().to_vec();
        cdsc.extend_from_slice(&1u16.to_be_bytes());
        cdsc.extend_from_slice(&42u16.to_be_bytes());
        iref.extend_from_slice(&bmff_box(b"cdsc", &cdsc));
    }
    let mut meta = vec![0, 0, 0, 0];
    meta.extend_from_slice(&bmff_box(b"iinf", &iinf));
    meta.extend_from_slice(&bmff_box(b"iloc", &iloc));
    meta.extend_from_slice(&bmff_box(b"iref", &iref));
    meta.extend_from_slice(&bmff_box(b"idat", &idat));
    let mut bytes = bmff_box(b"ftyp", b"avif\0\0\0\0avif");
    bytes.extend_from_slice(&bmff_box(b"meta", &meta));
    bytes
}
#[test]
fn avif_retains_all_items_extents_and_associations_before_pixel_decode() {
    let result = run(&avif(&[b"one opaque XML", b"two \xff\xfe\0"], 1));
    assert_eq!(result.status, Status::Complete);
    assert_eq!(result.packets.len(), 4);
    assert_eq!(result.parse_inputs.len(), 2);
    assert_eq!(result.parse_inputs[0].bytes, b"one opaque XML");
    assert_eq!(result.parse_inputs[1].bytes, b"two \xff\xfe\0");
    assert_eq!(result.packets[0].attributes["cdsc_to_item_ids"], "42");
    assert_eq!(result.packets[2].attributes["item_id"], "2");
}
#[test]
fn avif_unsupported_construction_is_never_absent() {
    let result = run(&avif(&[b"opaque XML"], 2));
    assert_eq!(result.status, Status::Unsupported);
    assert!(result.parse_inputs.is_empty());
    assert!(!result.issues.is_empty());
}
#[test]
fn cr3_uuid_is_retained_through_known_nested_boxes() {
    let mut uuid = vec![
        0xbe, 0x7a, 0xcf, 0xcb, 0x97, 0xa9, 0x42, 0xe8, 0x9c, 0x71, 0x99, 0x94, 0x91, 0xe3, 0xaf,
        0xac,
    ];
    uuid.extend_from_slice(b"<raw/>");
    let mut bytes = bmff_box(b"ftyp", b"crx \0\0\0\0crx ");
    bytes.extend_from_slice(&bmff_box(b"moov", &bmff_box(b"uuid", &uuid)));
    let result = run(&bytes);
    assert_eq!(result.status, Status::Complete);
    assert_eq!(result.parse_inputs[0].bytes, b"<raw/>");
}
#[test]
fn malformed_box_sizes_overflow_and_truncation_are_explicit() {
    let mut huge = bmff_box(b"ftyp", b"avif\0\0\0\0");
    huge.extend_from_slice(&1u32.to_be_bytes());
    huge.extend_from_slice(b"meta");
    huge.extend_from_slice(&u64::MAX.to_be_bytes());
    for bytes in [
        huge,
        avif(&[b"opaque XML"], 1)[..55].to_vec(),
        b"\0\0\0\x04ftyp".to_vec(),
    ] {
        let result = run(&bytes);
        assert_eq!(result.status, Status::Malformed);
        assert!(!result.issues.is_empty());
    }
}
#[test]
fn absent_unsupported_resource_limits_and_raw_sidecar_are_distinct() {
    assert_eq!(run(&jpeg(&[])).status, Status::Absent);
    assert_eq!(run(b"BMunknown BMP convention").status, Status::Unsupported);
    assert_eq!(run(b"unknown").status, Status::Unsupported);
    assert_eq!(
        inspect_bytes(
            &jpeg(&[]),
            &Limits {
                max_source_bytes: 1,
                ..Limits::default()
            }
        )
        .status,
        Status::ResourceLimit
    );
    assert_eq!(
        inspect_bytes(
            &jpeg(&[main_packet(b"one"), main_packet(b"two")]),
            &Limits {
                max_packets: 1,
                ..Limits::default()
            }
        )
        .status,
        Status::ResourceLimit
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("selected.xmp");
    let bytes = b"\xff\xfe<\0?\0x\0m\0l\0 \0?\0>\0unknown\xff";
    std::fs::write(&path, bytes).unwrap();
    let result = inspect_sidecar(&path, &Limits::default()).unwrap();
    assert_eq!(result.status, Status::Complete);
    assert_eq!(result.packets[0].bytes, bytes);
    assert_eq!(result.parse_inputs[0].bytes, bytes);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}
#[test]
fn every_prefix_of_representative_containers_is_safe_and_source_preserving() {
    for bytes in [
        jpeg(&[main_packet(b"opaque")]),
        png(&[itxt(b"opaque", true)]),
        tiff_two(),
        webp(&[(b"XMP ", b"opaque")]),
        psd(&[b"opaque"]),
        avif(&[b"opaque"], 1),
    ] {
        for end in 0..bytes.len() {
            let _ = run(&bytes[..end]);
        }
    }
}

#[test]
fn truncated_known_carriers_retain_available_source_bytes() {
    for bytes in [
        jpeg(&[main_packet(b"partial metadata")]),
        png(&[itxt(b"partial metadata", false)]),
        webp(&[(b"XMP ", b"partial metadata")]),
        psd(&[b"partial metadata"]),
    ] {
        let full = run(&bytes);
        let packet = &full.packets[0];
        let end = (packet.ranges[0].offset + packet.ranges[0].length - 3) as usize;
        let result = run(&bytes[..end]);
        assert_eq!(result.status, Status::Malformed);
        assert_eq!(result.packets.len(), 1);
        assert!(result.parse_inputs.is_empty());
        assert_eq!(result.packets[0].attributes["incomplete"], "true");
    }
}
#[test]
fn metadata_and_total_retention_budgets_are_enforced() {
    let bytes = jpeg(&[main_packet(b"first"), main_packet(b"second")]);
    for limits in [
        Limits {
            max_metadata_read_bytes: 10,
            ..Limits::default()
        },
        Limits {
            max_retained_bytes: 40,
            ..Limits::default()
        },
        Limits {
            max_parse_bytes: 6,
            ..Limits::default()
        },
    ] {
        assert_eq!(inspect_bytes(&bytes, &limits).status, Status::ResourceLimit);
    }
}
#[cfg(unix)]
#[test]
fn source_symlinks_are_rejected_for_photos_and_sidecars() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("target");
    let link = directory.path().join("link");
    std::fs::write(&target, jpeg(&[main_packet(b"original")])).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert_eq!(
        inspect(&link, &Limits::default()).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(
        inspect_sidecar(&link, &Limits::default())
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        jpeg(&[main_packet(b"original")])
    );
}

#[test]
fn avif_file_extents_use_absolute_offsets_and_missing_locations_fail() {
    let mut bytes = avif(&[b"opaque XML"], 0);
    let data = bytes.windows(4).position(|w| w == b"idat").unwrap() + 4;
    let iloc = bytes.windows(4).position(|w| w == b"iloc").unwrap() + 4;
    // FullBox + widths/count + item ID/method/reference/extent count.
    for field in [iloc + 16, iloc + 24] {
        let relative = u32::from_be_bytes(bytes[field..field + 4].try_into().unwrap());
        bytes[field..field + 4].copy_from_slice(&(relative + data as u32).to_be_bytes());
    }
    let result = run(&bytes);
    assert_eq!(result.status, Status::Complete);
    assert_eq!(result.parse_inputs[0].bytes, b"opaque XML");
    bytes[iloc - 4..iloc].copy_from_slice(b"free");
    let result = run(&bytes);
    assert_eq!(result.status, Status::Malformed);
    assert!(result.parse_inputs.is_empty());
}
#[test]
fn avif_duplicate_item_ids_and_truncated_extents_are_explicit() {
    let mut bytes = avif(&[b"first XML", b"second XML"], 1);
    let infe: Vec<usize> = bytes
        .windows(4)
        .enumerate()
        .filter(|(_, w)| *w == b"infe")
        .map(|(i, _)| i + 4)
        .collect();
    bytes[infe[1] + 4..infe[1] + 6].copy_from_slice(&1u16.to_be_bytes());
    assert_eq!(run(&bytes).status, Status::Malformed);
    let mut bytes = avif(&[b"opaque XML"], 1);
    let iloc = bytes.windows(4).position(|w| w == b"iloc").unwrap() + 4;
    bytes[iloc + 20..iloc + 24].copy_from_slice(&u32::MAX.to_be_bytes());
    let result = run(&bytes);
    assert_eq!(result.status, Status::Malformed);
    assert!(result.parse_inputs.is_empty());
}
#[test]
fn tiff_value_multiplication_overflow_and_bmff_depth_limit_are_explicit() {
    let mut bytes = b"II+\0\x08\0\0\0".to_vec();
    bytes.extend_from_slice(&16u64.to_le_bytes());
    bytes.extend_from_slice(&1u64.to_le_bytes());
    bytes.extend_from_slice(&700u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(&u64::MAX.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    assert_eq!(run(&bytes).status, Status::Malformed);
    let mut bytes = bmff_box(b"ftyp", b"avif\0\0\0\0avif");
    let mut nested = bmff_box(b"free", &[]);
    for _ in 0..5 {
        nested = bmff_box(b"moov", &nested);
    }
    bytes.extend_from_slice(&nested);
    assert_eq!(
        inspect_bytes(
            &bytes,
            &Limits {
                max_depth: 2,
                ..Limits::default()
            }
        )
        .status,
        Status::ResourceLimit
    );
}

#[test]
fn avif_content_encoding_is_derived_and_unknown_encoding_retains_extents() {
    let xml = b"<opaque>gzip RDF</opaque>";
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(xml).unwrap();
    let compressed = encoder.finish().unwrap();
    let result = run(&avif_encoded(&[&compressed], 1, b"gzip"));
    assert_eq!(result.status, Status::Complete);
    assert_eq!(result.parse_inputs[0].bytes, xml);
    assert_eq!(
        result.parse_inputs[0].transformation,
        Transformation::GzipDecompressed
    );
    let result = run(&avif_encoded(&[xml], 1, b"unknown"));
    assert_eq!(result.status, Status::Unsupported);
    assert_eq!(result.packets.len(), 2);
    assert!(result.parse_inputs.is_empty());
}
#[test]
fn raf_declared_jpeg_is_inspected_and_proprietary_coverage_is_explicit() {
    let embedded = jpeg(&[main_packet(b"RAF XMP")]);
    let mut bytes = vec![0; 108];
    bytes[..16].copy_from_slice(b"FUJIFILMCCD-RAW ");
    bytes[84..88].copy_from_slice(&108u32.to_be_bytes());
    bytes[88..92].copy_from_slice(&(embedded.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&embedded);
    let result = run(&bytes);
    assert_eq!(result.status, Status::Unsupported);
    assert_eq!(result.parse_inputs[0].bytes, b"RAF XMP");
    assert!(!result.issues.is_empty());
}
