//! Round-372 integration test: an indexed PixMap `ColorTable` is
//! resolved by each `ColorSpec`'s `value` field (the pixel index it maps
//! to), not by the entry's position in the `ctTable` array.
//!
//! Inside Macintosh: Imaging With QuickDraw §4 ("Color QuickDraw
//! Reference", book page 4-55): each `ColorSpec` carries *"the pixel
//! value assigned … for the color specified in the rgb field"*, and for
//! indexed devices *"the pixel value is an index number."* The pixel
//! index in the PixData therefore selects the `ColorSpec` whose `value`
//! equals that index. Real PICT colour tables usually store entries in
//! sequential `value` order (0, 1, 2, …) — in which case value-keying is
//! identical to position-keying — but a table whose `value` fields are
//! permuted (legal per §4) was previously mis-coloured because the
//! decoder mapped purely by array position. This test hand-assembles a
//! 4-bpp `PackBitsRect` whose colour table lists its entries in
//! *reverse* `value` order and asserts the pixels resolve correctly.
//!
//! A device color table (`ctFlags` high bit set) has the opposite
//! convention: `ColorSpec.value` is private device data and array
//! position supplies the pixel index. A second test pins that path with
//! all `value` fields zero, matching a real-world PICT that previously
//! collapsed every color into palette slot zero. See Inside Macintosh:
//! Imaging With QuickDraw §4, book pages 4-104 and 4-120, and Apple's
//! *develop* Issue 1, "Palette Manager" ("Drawing With Palette Colors").

use oxideav_pict::parse_pict;

const COLOR_TABLE_DEVICE_FLAG: u16 = 0x8000;

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn put_i16(out: &mut Vec<u8>, v: i16) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_pict_v2_prefix(out: &mut Vec<u8>, width: i16, height: i16) {
    put_u16(out, 0);
    put_i16(out, 0);
    put_i16(out, 0);
    put_i16(out, height);
    put_i16(out, width);
    put_u16(out, 0x0011);
    put_u16(out, 0x02FF);
    put_u16(out, 0x0C00);
    out.extend_from_slice(&[0u8; 24]);
}

fn put_indexed_pixmap_header(out: &mut Vec<u8>, row_bytes: u16, width: i16, height: i16, ps: u16) {
    put_u16(out, row_bytes | 0x8000);
    put_i16(out, 0);
    put_i16(out, 0);
    put_i16(out, height);
    put_i16(out, width);
    put_u16(out, 0); // pmVersion
    put_u16(out, 0); // packType
    put_u32(out, 0); // packSize
    put_u32(out, 72 << 16);
    put_u32(out, 72 << 16);
    put_u16(out, 0); // pixelType
    put_u16(out, ps);
    put_u16(out, 1); // cmpCount
    put_u16(out, ps); // cmpSize
    put_u32(out, 0);
    put_u32(out, 0);
    put_u32(out, 0);
}

/// A 4-entry `ColorTable` whose `(value, rgb)` pairs are listed in
/// reverse value order: value 3 first, then 2, 1, 0.
fn put_reversed_color_table(out: &mut Vec<u8>) {
    // Logical mapping: idx 0=black, 1=red, 2=green, 3=blue.
    let mapping: [(u16, [u8; 3]); 4] = [
        (3, [0x00, 0x00, 0xFF]), // blue
        (2, [0x00, 0xFF, 0x00]), // green
        (1, [0xFF, 0x00, 0x00]), // red
        (0, [0x00, 0x00, 0x00]), // black
    ];
    put_u32(out, 0xDEADBEEF); // ctSeed
    put_i16(out, 0); // ctFlags (pixel-map table)
    put_i16(out, (mapping.len() as i16) - 1); // ctSize
    for (value, [r, g, b]) in mapping {
        put_u16(out, value);
        put_u16(out, u16::from_be_bytes([r, r]));
        put_u16(out, u16::from_be_bytes([g, g]));
        put_u16(out, u16::from_be_bytes([b, b]));
    }
}

fn put_device_color_table(out: &mut Vec<u8>) {
    let colors: [[u8; 3]; 4] = [
        [0x00, 0x00, 0x00], // index 0 — black
        [0xFF, 0x00, 0x00], // index 1 — red
        [0x00, 0xFF, 0x00], // index 2 — green
        [0x00, 0x00, 0xFF], // index 3 — blue
    ];
    put_u32(out, 0xDEADBEEF); // ctSeed
    put_u16(out, COLOR_TABLE_DEVICE_FLAG); // ctFlags (device table)
    put_i16(out, (colors.len() as i16) - 1); // ctSize
    for [r, g, b] in colors {
        put_u16(out, 0); // device-private value; not the pixel index
        put_u16(out, u16::from_be_bytes([r, r]));
        put_u16(out, u16::from_be_bytes([g, g]));
        put_u16(out, u16::from_be_bytes([b, b]));
    }
}

fn pack_4bpp(indices: &[u8], row_bytes: usize) -> Vec<u8> {
    let mut out = vec![0u8; row_bytes];
    for (x, idx) in indices.iter().enumerate() {
        let byte_off = x >> 1;
        if (x & 1) == 0 {
            out[byte_off] |= (idx & 0x0F) << 4;
        } else {
            out[byte_off] |= idx & 0x0F;
        }
    }
    out
}

#[test]
fn indexed_color_table_resolves_by_value_not_position() {
    let width: i16 = 4;
    let height: i16 = 1;
    let row_bytes: u16 = 2; // 4 px × 4 bpp = 2 bytes (< 8 → raw rows)

    let mut bytes: Vec<u8> = Vec::new();
    put_pict_v2_prefix(&mut bytes, width, height);
    put_u16(&mut bytes, 0x0098); // PackBitsRect
    put_indexed_pixmap_header(&mut bytes, row_bytes, width, height, 4);
    put_reversed_color_table(&mut bytes);
    // srcRect + dstRect (identity) + mode.
    for _ in 0..2 {
        put_i16(&mut bytes, 0);
        put_i16(&mut bytes, 0);
        put_i16(&mut bytes, height);
        put_i16(&mut bytes, width);
    }
    put_u16(&mut bytes, 0); // mode

    // PixData (raw, rowBytes < 8): indices [0, 1, 2, 3].
    bytes.extend_from_slice(&pack_4bpp(&[0, 1, 2, 3], row_bytes as usize));
    if bytes.len() % 2 != 0 {
        bytes.push(0);
    }
    put_u16(&mut bytes, 0x00FF); // OpEndPic

    let img = parse_pict(&bytes).expect("decode indexed PackBitsRect");
    let p = |x: u32| {
        let off = (x * 4) as usize;
        [img.data[off], img.data[off + 1], img.data[off + 2]]
    };
    // Despite the reversed table order, each index resolves to the RGB
    // whose ColorSpec.value matches it.
    assert_eq!(p(0), [0x00, 0x00, 0x00], "idx 0 → black");
    assert_eq!(p(1), [0xFF, 0x00, 0x00], "idx 1 → red");
    assert_eq!(p(2), [0x00, 0xFF, 0x00], "idx 2 → green");
    assert_eq!(p(3), [0x00, 0x00, 0xFF], "idx 3 → blue");
}

#[test]
fn indexed_color_table_unmatched_value_is_black() {
    // A sparse table that defines only value 1 (red). Index 0 has no
    // matching ColorSpec → black (the §4 empty-slot fallback).
    let width: i16 = 2;
    let height: i16 = 1;
    let row_bytes: u16 = 1; // 2 px × 4 bpp = 1 byte (< 8 → raw)

    let mut bytes: Vec<u8> = Vec::new();
    put_pict_v2_prefix(&mut bytes, width, height);
    put_u16(&mut bytes, 0x0098);
    put_indexed_pixmap_header(&mut bytes, row_bytes, width, height, 4);
    // One-entry table: value 1 → red.
    put_u32(&mut bytes, 0xDEADBEEF);
    put_i16(&mut bytes, 0); // ctFlags
    put_i16(&mut bytes, 0); // ctSize = 0 → one entry
    put_u16(&mut bytes, 1); // value = 1
    put_u16(&mut bytes, 0xFFFF);
    put_u16(&mut bytes, 0x0000);
    put_u16(&mut bytes, 0x0000);
    for _ in 0..2 {
        put_i16(&mut bytes, 0);
        put_i16(&mut bytes, 0);
        put_i16(&mut bytes, height);
        put_i16(&mut bytes, width);
    }
    put_u16(&mut bytes, 0);
    bytes.extend_from_slice(&pack_4bpp(&[0, 1], row_bytes as usize));
    if bytes.len() % 2 != 0 {
        bytes.push(0);
    }
    put_u16(&mut bytes, 0x00FF);

    let img = parse_pict(&bytes).expect("decode sparse-table indexed PackBitsRect");
    let p = |x: u32| {
        let off = (x * 4) as usize;
        [img.data[off], img.data[off + 1], img.data[off + 2]]
    };
    assert_eq!(p(0), [0x00, 0x00, 0x00], "unmatched idx 0 → black");
    assert_eq!(p(1), [0xFF, 0x00, 0x00], "idx 1 → red");
}

#[test]
fn device_color_table_resolves_by_position_not_value() {
    let width: i16 = 4;
    let height: i16 = 1;
    let row_bytes: u16 = 2;

    let mut bytes: Vec<u8> = Vec::new();
    put_pict_v2_prefix(&mut bytes, width, height);
    put_u16(&mut bytes, 0x0098); // PackBitsRect
    put_indexed_pixmap_header(&mut bytes, row_bytes, width, height, 4);
    put_device_color_table(&mut bytes);
    for _ in 0..2 {
        put_i16(&mut bytes, 0);
        put_i16(&mut bytes, 0);
        put_i16(&mut bytes, height);
        put_i16(&mut bytes, width);
    }
    put_u16(&mut bytes, 0); // mode
    bytes.extend_from_slice(&pack_4bpp(&[0, 1, 2, 3], row_bytes as usize));
    if bytes.len() % 2 != 0 {
        bytes.push(0);
    }
    put_u16(&mut bytes, 0x00FF);

    let img = parse_pict(&bytes).expect("decode device-table indexed PackBitsRect");
    let p = |x: u32| {
        let off = (x * 4) as usize;
        [img.data[off], img.data[off + 1], img.data[off + 2]]
    };
    assert_eq!(p(0), [0x00, 0x00, 0x00], "position 0 → black");
    assert_eq!(p(1), [0xFF, 0x00, 0x00], "position 1 → red");
    assert_eq!(p(2), [0x00, 0xFF, 0x00], "position 2 → green");
    assert_eq!(p(3), [0x00, 0x00, 0xFF], "position 3 → blue");
}
