//! PICT decoder.
//!
//! Clean-room implementation of Inside Macintosh: Imaging With QuickDraw
//! (Apple, 1994). The relevant chapters are:
//!
//! * §A-3 ("Picture opcodes") — the opcode table for v1 (8-bit) and
//!   v2 (16-bit, word-aligned) streams.
//! * §A-5 ("The PackBits procedure") — RLE algorithm.
//! * §4 ("Color QuickDraw and PixMaps") — `PixMap` record layout.
//! * §2 ("QuickDraw Drawing") — `Region` and drawing-verb semantics.
//!
//! Round 2 walks the v2 opcode stream, stepping a small drawing-state
//! machine ([`crate::state::PictState`]) and folding every drawing
//! command (line / rect / round-rect / oval / arc / poly / region /
//! raster) onto an in-crate software-rasteriser canvas
//! ([`crate::raster::Canvas`]). When the walk terminates (`OpEndPic`,
//! end-of-stream, or an unsupported opcode) the canvas — sized to
//! `picFrame` and pre-filled with the background colour (paper) — is
//! returned as the [`PictImage`]. PICTs that contain no drawing
//! commands at all return [`PictError::NoRaster`] — unless they carry
//! a QuickTime payload (`$8200` / `$8201`), in which case the
//! captured payload *is* the picture's content and the untouched
//! background canvas is returned alongside it (round 435).
//!
//! v1 PICTs (8-bit opcodes) have basic raster + line/rect/region
//! support — same drawing-state model, smaller opcode roster.
//!
//! ## DirectBitsRect packType
//!
//! * `0` (none) and `1` (uncompressed): per-row pixel data laid out
//!   verbatim, `rowBytes` bytes long.
//! * `2`: same uncompressed data but with the per-pixel pad byte
//!   removed (32-bit pixels become 24 bytes per row × 3 byte
//!   stride).
//! * `3`: per-row 16-bit-pixel PackBits compression, where the unit
//!   replicated by run packets is a `u16` rather than a `u8`.
//! * `4`: per-row component-separated PackBits (R plane, G plane, B
//!   plane, optionally A plane) compression at 8-bit byte units.
//!
//! Byte-count prefix per row (1 byte if `rowBytes < 250`, else
//! 2 bytes) is shared between packTypes 3 and 4.

use crate::error::{PictError, Result};
use crate::header::{Fixed, PictHeader};
use crate::image::{PictComment, PictImage, PictPixelFormat};
use crate::opcodes::*;
use crate::packbits;
use crate::raster::{
    fill_arc, fill_oval_pattern_mode, fill_oval_pix_pattern, fill_polygon_pattern_mode,
    fill_polygon_pix_pattern, fill_rect, fill_rect_pattern_mode, fill_rect_pix_pattern,
    fill_round_rect_pattern_mode, fill_round_rect_pix_pattern, frame_arc,
    frame_arc_pattern_thick_mode, frame_arc_pix_pattern_thick, frame_oval_pattern_thick_mode,
    frame_oval_pix_pattern_thick, frame_oval_thick, frame_polygon,
    frame_polygon_pattern_thick_mode, frame_polygon_pix_pattern_thick,
    frame_rect_pattern_thick_mode, frame_rect_pix_pattern_thick, frame_round_rect,
    frame_round_rect_pattern_thick_mode, frame_round_rect_pix_pattern_thick, invert_arc,
    invert_oval, invert_polygon, invert_round_rect, line_pattern_thick_mode,
    line_pix_pattern_thick, line_thick as draw_line_thick, stamp_region_pen_cell_mode,
    stamp_region_pen_cell_pix, Canvas, PatternMode, SourceMode,
};
use crate::reader::Reader;
use crate::region::{parse_region, Region};
use crate::state::{
    Pattern, PictFontName, PictGlyphState, PictLineJustify, PictState, PixPattern, RectI32, Rgba,
    TextRatio,
};

/// Decode a complete PICT byte stream into a single rasterised
/// [`PictImage`].
///
/// Accepts both forms produced by real-world generators:
///
/// * Raw PICT body — the 10-byte v1/v2 picture record header is at
///   offset 0.
/// * 512-byte launch-stub prefix + PICT body — Apple's pre-OS-X file-
///   manager habit. Detected by checking that offset 512 looks like a
///   plausible picture record (picSize then picFrame then the version
///   sentinel at +10) and the 0..512 prefix doesn't.
///
/// Returns [`PictError::NoRaster`] if the opcode stream terminates
/// (`OpEndPic`) or runs out of bytes without producing any drawing
/// or raster output.
/// Decode-allocation budget (round 401 hostile-input hardening).
///
/// Every buffer whose size is derived from attacker-controlled length
/// fields (`picFrame`, PixMap `bounds` × `rowBytes`, …) is checked
/// against this budget before allocation: 256 MiB, comfortably above
/// any real QuickDraw-era picture (an 8192 × 8192 RGBA canvas) while
/// keeping a hostile 12-byte header from demanding a multi-gigabyte
/// allocation. Exceeding it returns [`PictError::InvalidData`].
pub const MAX_RASTER_BYTES: usize = 1 << 28;

/// Checked `rows × bytes_per_row` buffer sizing against
/// [`MAX_RASTER_BYTES`]. `what` names the buffer in the error.
fn checked_raster_len(rows: usize, bytes_per_row: usize, what: &str) -> Result<usize> {
    match rows.checked_mul(bytes_per_row) {
        Some(total) if total <= MAX_RASTER_BYTES => Ok(total),
        _ => Err(PictError::invalid(format!(
            "{what} of {rows} × {bytes_per_row} bytes exceeds the {MAX_RASTER_BYTES}-byte decode budget"
        ))),
    }
}

/// Width / height of an on-disk `(top, left, bottom, right)` rectangle,
/// computed in i32 so the maximum i16 span (−32768..32767 = 65535)
/// cannot overflow. Negative spans clamp to 0.
fn rect_dims(bounds: (i16, i16, i16, i16)) -> (u32, u32) {
    let w = (bounds.3 as i32 - bounds.1 as i32).max(0) as u32;
    let h = (bounds.2 as i32 - bounds.0 as i32).max(0) as u32;
    (w, h)
}

pub fn parse_pict(bytes: &[u8]) -> Result<PictImage> {
    let body_offset = detect_body_offset(bytes)?;
    let body = &bytes[body_offset..];

    let mut r = Reader::new(body);
    let _pic_size = r.read_u16()?;
    let pic_frame = r.read_rect()?;
    let pic_frame = RectI32::from_be(pic_frame.0, pic_frame.1, pic_frame.2, pic_frame.3);

    let (version, header) = detect_version(&mut r)?;

    // Initial canvas sized to the picture frame, pre-filled white
    // (QuickDraw "paper"). The drawing-state machine adjusts the
    // origin so we always plot in canvas-local 0..width, 0..height
    // coordinates.
    let width = (pic_frame.right - pic_frame.left).max(0) as u32;
    let height = (pic_frame.bottom - pic_frame.top).max(0) as u32;
    if width == 0 || height == 0 {
        return Err(PictError::invalid(format!(
            "picFrame degenerate: {:?}",
            (
                pic_frame.top,
                pic_frame.left,
                pic_frame.bottom,
                pic_frame.right
            )
        )));
    }
    // Hostile-input hardening: a 12-byte header can claim a 65535 ×
    // 65535 frame (~17 GB of RGBA canvas). Refuse before allocating.
    checked_raster_len(height as usize, width as usize * 4, "picFrame canvas")?;
    let canvas = Canvas::new(width, height, Rgba::WHITE);
    let state = PictState {
        // Origin shifts so picFrame.top/left maps to canvas (0, 0).
        origin: (pic_frame.left, pic_frame.top),
        ..PictState::default()
    };

    let mut img = match version {
        PictVersion::V1 => parse_v1_opcodes(&mut r, pic_frame, canvas, state)?,
        PictVersion::V2 => parse_v2_opcodes(&mut r, pic_frame, canvas, state)?,
    };
    img.header = header;
    Ok(img)
}

/// Advance the QuickDraw text-drawing pen by a `(dh, dv)` delta from the
/// `DHText` / `DVText` / `DHDVText` opcodes (§A-3 Table A-2).
///
/// The deltas are relative to the position the previous text opcode left
/// in [`crate::state::PictTextState::text_pen`]. With no prior `LongText`
/// the running pen starts at the graphics origin `(0, 0)`. Also bumps the
/// text-op counter so callers can see how many text-glyph opcodes a
/// picture carried even without a font rasteriser. Round 295.
fn advance_text_pen(state: &mut PictState, dh: i32, dv: i32) {
    let (h, v) = state.text_state.text_pen.unwrap_or((0, 0));
    state.text_state.text_pen = Some((h + dh, v + dv));
    state.text_state.text_op_count += 1;
}

/// Rasterise a text-glyph opcode's string onto the canvas at the current
/// text pen, then advance the pen rightward by the drawn width so the next
/// `DH/DV/DHDVText` opcode on the same line continues where this left off.
///
/// QuickDraw text-drawing geometry (Imaging With QuickDraw, book pages
/// 2-13/2-34): the baseline sits at the pen location; `txSize` is the cell
/// height in pixels; `fgColor` is the ink; the text source mode
/// (`srcOr` / `srcXor` / `srcBic`) controls how glyph pixels combine with
/// the canvas. `chExtra` (`$0016`) widens every character and `spExtra`
/// (`$0006`) every space. The glyph artwork comes from the crate's
/// built-in clean-room ASCII face ([`crate::font`]) — PICT carries no font
/// data, so this is the in-tree spec-grounded stand-in for the system
/// font, not a reproduction of any particular Mac font.
fn render_text(canvas: &mut Canvas, state: &mut PictState, text: &[u8]) {
    let (pen_h, pen_v) = state.text_state.text_pen.unwrap_or((0, 0));
    let (cx, cy) = to_canvas(state, pen_h, pen_v);
    let tx_size = state.text_state.tx_size as i32;
    let ch_extra = state.text_state.ch_extra as i32;
    // spExtra is a Fixed (16.16) average extra space width; the integer
    // part is what the pen actually advances by per space.
    let sp_extra = state.text_state.sp_extra.0 >> 16;
    // `TxRatio` (`$0010`): numer.h/denom.h is the horizontal glyph-cell
    // scale, numer.v/denom.v the vertical (Imaging With QuickDraw book
    // page 12-13). The §A-3 fresh-GrafPort default is 1/1 on both axes,
    // which reduces to the plain `txSize / DESIGN_EM` scale.
    let ratio = state.text_state.tx_ratio;
    let scale = crate::font::TextScale {
        tx_size,
        numer_h: ratio.numer_h as i32,
        denom_h: ratio.denom_h as i32,
        numer_v: ratio.numer_v as i32,
        denom_v: ratio.denom_v as i32,
    };
    // `lineJustify` (`$002D`) intercharacter spacing — the Script Manager
    // "extra character width" added to every glyph's advance (§A-3
    // footnote `†`). Stored as a Fixed; the pen advances by its integer
    // part. Absent when no `lineJustify` record has been seen.
    let inter_char = state
        .text_state
        .line_justify
        .map(|lj| lj.inter_char_spacing.0 >> 16)
        .unwrap_or(0);
    // Text uses only the Boolean source modes (book page 2-34). The
    // resolved SourceMode falls back to srcOr — the visible default — when
    // the stream's txMode is srcCopy (0), because a srcCopy text draw would
    // paint an opaque white box behind every glyph, which is never what a
    // picture intends for inline text on an existing canvas.
    //
    // `grayishTextOr = 49` is text-only and resolved here, before the
    // shared raster resolver would fold it: on a colour destination it
    // "draws with a blend of the foreground and background colors"
    // (Inside Macintosh Volume VI, page 17-17), i.e. the glyph ink is
    // the fg/bg average composited like srcOr. See
    // [`crate::raster::GRAYISH_TEXT_OR_MODE`].
    let bg_key = state.bg;
    let base_word = state.text_state.tx_mode & !crate::raster::SourceMode::DITHER_COPY;
    let (ink, mode) = if base_word == crate::raster::GRAYISH_TEXT_OR_MODE {
        (
            state.fg.blend_half(state.bg),
            crate::raster::SourceMode::SrcOr,
        )
    } else {
        let mode = match state.text_state.tx_source_mode(bg_key) {
            crate::raster::SourceMode::SrcCopy => crate::raster::SourceMode::SrcOr,
            other => other,
        };
        (state.fg, mode)
    };
    // `txFace` style synthesis (bold / italic / underline / outline /
    // shadow / condense / extend) per Inside Macintosh Volume I pages
    // I-151/I-152 with the I-226 screen characterization-table amounts —
    // see [`crate::font::StyleParams`].
    let face = state.text_state.tx_face;
    let advanced = crate::font::draw_text(
        canvas, text, cx, cy, scale, ch_extra, sp_extra, inter_char, face, ink, state.bg, mode,
    );
    // Move the running text pen by the drawn width (in picture-frame
    // coords, which equals canvas advance since x-scale is 1:1).
    // Saturating: `advanced` can approach i32 range under hostile
    // txSize / TxRatio words (round 407 hardening).
    state.text_state.text_pen = Some((pen_h.saturating_add(advanced), pen_v));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PictVersion {
    V1,
    V2,
}

fn detect_body_offset(bytes: &[u8]) -> Result<usize> {
    if looks_like_picture_record(bytes) {
        return Ok(0);
    }
    if bytes.len() >= 512 + 12 && looks_like_picture_record(&bytes[512..]) {
        return Ok(512);
    }
    Err(PictError::invalid(
        "no PICT picture record at offset 0 or 512",
    ))
}

fn looks_like_picture_record(bytes: &[u8]) -> bool {
    if bytes.len() < 12 {
        return false;
    }
    if &bytes[10..14.min(bytes.len())] == [0x00, 0x11, 0x02, 0xFF].as_slice() {
        return true;
    }
    if bytes[10] == 0x11 && bytes[11] == 0x01 {
        return true;
    }
    false
}

fn detect_version(r: &mut Reader<'_>) -> Result<(PictVersion, Option<PictHeader>)> {
    // The version stanza is the first thing after the 10-byte
    // picture-record header. v2 emits the 2-byte opcode 0x0011
    // followed by the 2-byte 0x02FF v2 sentinel and the headerOp
    // stanza. v1 emits the 1-byte opcode 0x11 followed by the
    // 1-byte version 0x01 — so reading 2 bytes gives 0x1101.
    let v_word = r.read_u16()?;
    if v_word == 0x0011 {
        let next = r.read_u16()?;
        if next == 0x02FF {
            let header_op = r.read_u16()?;
            if header_op != OP_HEADER_OP {
                return Err(PictError::invalid(format!(
                    "expected headerOp 0x0C00 after v2 sentinel, got 0x{header_op:04X}"
                )));
            }
            // Decode the 24-byte payload per §A-3 / Listings A-5+A-6.
            // Pre-existing zero-pad headers (our own pre-r217 encoder
            // emitted `[0u8; 24]`, which is neither the FFFE nor FFFF
            // version marker) are tolerated by stepping past them so
            // older PICTs still decode.
            let header = match PictHeader::parse(r) {
                Ok(h) => Some(h),
                Err(_) => {
                    // Already consumed the 2-byte version word inside
                    // PictHeader::parse — back up and skip 24 bytes
                    // total to preserve the §A-3 24-byte payload
                    // contract.
                    r.pos -= 2;
                    r.skip(24)?;
                    None
                }
            };
            return Ok((PictVersion::V2, header));
        }
        // Some pre-v2 generators pad the version opcode out to 2 bytes
        // (`0x0011`) followed by a 1-byte version `0x01` then v1
        // opcodes. The `next` word's high byte is then 0x01 — we
        // already consumed it as part of the second read, so the
        // first v1 opcode is in the low byte.
        if (next >> 8) == 0x01 {
            // We have to back up one byte: the low byte of `next` is
            // the first v1 opcode.
            r.pos -= 1;
            return Ok((PictVersion::V1, None));
        }
        return Err(PictError::invalid(format!(
            "unrecognised version stanza after 0x0011: 0x{next:04X}"
        )));
    }
    if v_word == 0x1101 {
        // Canonical v1 form: 1-byte opcode 0x11 then 1-byte version
        // 0x01. Both bytes consumed; the next byte is the first v1
        // opcode.
        return Ok((PictVersion::V1, None));
    }
    Err(PictError::invalid(format!(
        "expected version opcode 0x0011 or 0x1101, got 0x{v_word:04X}"
    )))
}

/// Translate a picture-frame coordinate to canvas-local coordinates.
fn to_canvas(state: &PictState, x: i32, y: i32) -> (i32, i32) {
    (x - state.origin.0, y - state.origin.1)
}

/// Translate a picture-frame rectangle to canvas-local coords.
fn rect_to_canvas(state: &PictState, rect: RectI32) -> (i32, i32, i32, i32) {
    let (x0, y0) = to_canvas(state, rect.left, rect.top);
    let (x1, y1) = to_canvas(state, rect.right, rect.bottom);
    (y0, x0, y1, x1) // (top, left, bottom, right) for raster fns
}

/// Walk a v2 opcode stream, fold every command into the canvas, and
/// return the result.
fn parse_v2_opcodes(
    r: &mut Reader<'_>,
    pic_frame: RectI32,
    mut canvas: Canvas,
    mut state: PictState,
) -> Result<PictImage> {
    while !r.at_eof() {
        r.align_word()?;
        if r.at_eof() {
            break;
        }
        let opcode = r.read_u16()?;
        if !dispatch_v2_opcode(r, opcode, &pic_frame, &mut canvas, &mut state)? {
            break; // OpEndPic
        }
    }
    finalise_canvas(canvas, &state)
}

/// One v2 opcode dispatch. Returns `Ok(false)` only on `OpEndPic` —
/// the caller breaks out of the walk.
fn dispatch_v2_opcode(
    r: &mut Reader<'_>,
    opcode: u16,
    pic_frame: &RectI32,
    canvas: &mut Canvas,
    state: &mut PictState,
) -> Result<bool> {
    match opcode {
        OP_NOP => Ok(true),
        OP_OP_END_PIC => Ok(false),
        OP_CLIP_RGN => {
            // Clip region: parse + materialise into a canvas-local
            // boolean mask. Subsequent drawing / raster ops are gated
            // through `Canvas::put` / `span` / `blit`, all of which
            // consult `canvas.clip`. (Round 42.)
            let rgn = parse_region(r)?;
            install_clip_region(canvas, state, &rgn);
            Ok(true)
        }
        OP_FRAME_RGN => {
            let rgn = parse_region(r)?;
            paint_region_outline(canvas, state, &rgn);
            Ok(true)
        }
        OP_PAINT_RGN => {
            let rgn = parse_region(r)?;
            if let Some(pp) = &state.pen_pix_pat {
                paint_region_pix_pattern(canvas, state, &rgn, pp);
            } else {
                paint_region_pattern(canvas, state, &rgn, state.pen_pat, state.fg, state.bg);
            }
            Ok(true)
        }
        OP_FILL_RGN => {
            let rgn = parse_region(r)?;
            if let Some(pp) = &state.fill_pix_pat {
                paint_region_pix_pattern(canvas, state, &rgn, pp);
            } else {
                paint_region_pattern(canvas, state, &rgn, state.fill_pat, state.fg, state.bg);
            }
            Ok(true)
        }
        OP_ERASE_RGN => {
            let rgn = parse_region(r)?;
            if let Some(pp) = &state.back_pix_pat {
                paint_region_pix_pattern(canvas, state, &rgn, pp);
            } else {
                // Erase: stipple on-bits map to *background*, off-bits to
                // foreground — the BkPat inversion convention from Inside
                // Macintosh §A-3.
                paint_region_pattern(canvas, state, &rgn, state.back_pat, state.bg, state.fg);
            }
            Ok(true)
        }
        OP_INVERT_RGN => {
            let rgn = parse_region(r)?;
            invert_region(canvas, state, &rgn);
            Ok(true)
        }
        // Drawing-state opcodes that change `state` (we keep a running
        // copy so subsequent geometry uses the right colour / pen).
        OP_RGB_FG_COL => {
            let (rr, gg, bb) = read_rgb16(r)?;
            state.fg = Rgba::from_rgb16(rr, gg, bb);
            Ok(true)
        }
        OP_RGB_BK_COL => {
            let (rr, gg, bb) = read_rgb16(r)?;
            state.bg = Rgba::from_rgb16(rr, gg, bb);
            Ok(true)
        }
        OP_FG_COLOR => {
            let code = r.read_u32()?;
            state.fg = Rgba::from_pascal_colour(code);
            Ok(true)
        }
        OP_BG_COLOR => {
            let code = r.read_u32()?;
            state.bg = Rgba::from_pascal_colour(code);
            Ok(true)
        }
        OP_PN_SIZE => {
            let v = r.read_i16()?;
            let h = r.read_i16()?;
            state.pen_size = (h as i32, v as i32);
            Ok(true)
        }
        OP_OV_SIZE => {
            let v = r.read_i16()?;
            let h = r.read_i16()?;
            state.oval_size = (h as i32, v as i32);
            Ok(true)
        }
        OP_ORIGIN => {
            // Origin ($000C): dh, dv — the delta applied to the
            // picture's coordinate origin. Per the SetOrigin semantics
            // in Inside Macintosh: Imaging With QuickDraw §2 "Basic
            // QuickDraw" (book pages 2-23 f.), the origin coordinates
            // are the local coordinates assigned to the port
            // rectangle's upper-left corner: *increasing* them means a
            // shape drawn at unchanged coordinates lands closer to (or
            // past) that corner, i.e. moves up / left on the canvas.
            // `to_canvas` computes `x - state.origin.0`, so the delta
            // is added to the running origin. (Round 401 fixes the
            // inverted sign used through round 397.)
            let dh = r.read_i16()?;
            let dv = r.read_i16()?;
            state.origin.0 += dh as i32;
            state.origin.1 += dv as i32;
            Ok(true)
        }
        OP_PN_PAT => {
            // PnPat (0x0009): 8-byte monochrome 8×8 pattern, applied by
            // frame / paint verbs. Inside Macintosh §A-3. Clears the
            // colour `pen_pix_pat` slot — a subsequent mono PnPat
            // overrides any previously-set PnPixPat (classic QuickDraw
            // "most recent pattern wins" semantics).
            let bytes = r.read_bytes(8)?;
            let mut p = [0u8; 8];
            p.copy_from_slice(bytes);
            state.pen_pat = Pattern(p);
            state.pen_pix_pat = None;
            Ok(true)
        }
        OP_BK_PAT => {
            // BkPat (0x0002): background pattern, used by erase verbs.
            let bytes = r.read_bytes(8)?;
            let mut p = [0u8; 8];
            p.copy_from_slice(bytes);
            state.back_pat = Pattern(p);
            state.back_pix_pat = None;
            Ok(true)
        }
        OP_FILL_PAT => {
            // FillPat (0x000A): fill pattern, used by fill verbs
            // (low-nibble 4).
            let bytes = r.read_bytes(8)?;
            let mut p = [0u8; 8];
            p.copy_from_slice(bytes);
            state.fill_pat = Pattern(p);
            state.fill_pix_pat = None;
            Ok(true)
        }
        // Pen-position-affecting ops.
        OP_LINE => {
            let pt0_v = r.read_i16()? as i32;
            let pt0_h = r.read_i16()? as i32;
            let pt1_v = r.read_i16()? as i32;
            let pt1_h = r.read_i16()? as i32;
            let (x0, y0) = to_canvas(state, pt0_h, pt0_v);
            let (x1, y1) = to_canvas(state, pt1_h, pt1_v);
            draw_line_pen(canvas, state, x0, y0, x1, y1);
            state.pen = (pt1_h, pt1_v);
            Ok(true)
        }
        OP_LINE_FROM => {
            let pt_v = r.read_i16()? as i32;
            let pt_h = r.read_i16()? as i32;
            let (x0, y0) = to_canvas(state, state.pen.0, state.pen.1);
            let (x1, y1) = to_canvas(state, pt_h, pt_v);
            draw_line_pen(canvas, state, x0, y0, x1, y1);
            state.pen = (pt_h, pt_v);
            Ok(true)
        }
        OP_SHORT_LINE => {
            let pt_v = r.read_i16()? as i32;
            let pt_h = r.read_i16()? as i32;
            let dh = r.read_u8()? as i8 as i32;
            let dv = r.read_u8()? as i8 as i32;
            let nx = pt_h + dh;
            let ny = pt_v + dv;
            let (x0, y0) = to_canvas(state, pt_h, pt_v);
            let (x1, y1) = to_canvas(state, nx, ny);
            draw_line_pen(canvas, state, x0, y0, x1, y1);
            state.pen = (nx, ny);
            Ok(true)
        }
        OP_SHORT_LINE_FROM => {
            let dh = r.read_u8()? as i8 as i32;
            let dv = r.read_u8()? as i8 as i32;
            let nx = state.pen.0 + dh;
            let ny = state.pen.1 + dv;
            let (x0, y0) = to_canvas(state, state.pen.0, state.pen.1);
            let (x1, y1) = to_canvas(state, nx, ny);
            draw_line_pen(canvas, state, x0, y0, x1, y1);
            state.pen = (nx, ny);
            Ok(true)
        }
        // Rect verbs.
        OP_FRAME_RECT | OP_PAINT_RECT | OP_ERASE_RECT | OP_INVERT_RECT | OP_FILL_RECT => {
            let rect = read_rect_op(r)?;
            state.last_rect = Some(rect);
            apply_rect_verb(canvas, state, opcode, rect);
            Ok(true)
        }
        OP_FRAME_SAME_RECT | OP_PAINT_SAME_RECT | OP_ERASE_SAME_RECT | OP_INVERT_SAME_RECT
        | OP_FILL_SAME_RECT => {
            if let Some(rect) = state.last_rect {
                apply_rect_verb(canvas, state, opcode - 8, rect);
            }
            Ok(true)
        }
        OP_FRAME_RRECT | OP_PAINT_RRECT | OP_ERASE_RRECT | OP_INVERT_RRECT | OP_FILL_RRECT => {
            let rect = read_rect_op(r)?;
            state.last_rrect = Some(rect);
            apply_rrect_verb(canvas, state, opcode, rect);
            Ok(true)
        }
        OP_FRAME_SAME_RRECT | OP_PAINT_SAME_RRECT | OP_ERASE_SAME_RRECT | OP_INVERT_SAME_RRECT
        | OP_FILL_SAME_RRECT => {
            if let Some(rect) = state.last_rrect {
                apply_rrect_verb(canvas, state, opcode - 8, rect);
            }
            Ok(true)
        }
        OP_FRAME_OVAL | OP_PAINT_OVAL | OP_ERASE_OVAL | OP_INVERT_OVAL | OP_FILL_OVAL => {
            let rect = read_rect_op(r)?;
            state.last_oval = Some(rect);
            apply_oval_verb(canvas, state, opcode, rect);
            Ok(true)
        }
        OP_FRAME_SAME_OVAL | OP_PAINT_SAME_OVAL | OP_ERASE_SAME_OVAL | OP_INVERT_SAME_OVAL
        | OP_FILL_SAME_OVAL => {
            if let Some(rect) = state.last_oval {
                apply_oval_verb(canvas, state, opcode - 8, rect);
            }
            Ok(true)
        }
        OP_FRAME_ARC | OP_PAINT_ARC | OP_ERASE_ARC | OP_INVERT_ARC | OP_FILL_ARC => {
            let rect = read_rect_op(r)?;
            let start = r.read_i16()? as i32;
            let arc = r.read_i16()? as i32;
            state.last_arc_rect = Some(rect);
            apply_arc_verb(canvas, state, opcode, rect, start, arc);
            Ok(true)
        }
        OP_FRAME_SAME_ARC | OP_PAINT_SAME_ARC | OP_ERASE_SAME_ARC | OP_INVERT_SAME_ARC
        | OP_FILL_SAME_ARC => {
            let start = r.read_i16()? as i32;
            let arc = r.read_i16()? as i32;
            if let Some(rect) = state.last_arc_rect {
                apply_arc_verb(canvas, state, opcode - 8, rect, start, arc);
            }
            Ok(true)
        }
        // Polygon verbs: 2-byte size, 8-byte bounding rect, then
        // (size - 10)/4 (h, v) pairs.
        OP_FRAME_POLY | OP_PAINT_POLY | OP_ERASE_POLY | OP_INVERT_POLY | OP_FILL_POLY => {
            let poly_size = r.read_u16()? as usize;
            if poly_size < 10 {
                return Err(PictError::invalid(format!(
                    "polygon size {poly_size} smaller than the 10-byte header"
                )));
            }
            let _bbox = r.read_rect()?;
            let n_verts = (poly_size - 10) / 4;
            let mut verts = Vec::with_capacity(n_verts);
            for _ in 0..n_verts {
                let v = r.read_i16()? as i32;
                let h = r.read_i16()? as i32;
                let (cx, cy) = to_canvas(state, h, v);
                verts.push((cx, cy));
            }
            apply_poly_verb(canvas, state, opcode, &verts);
            Ok(true)
        }
        OP_LONG_TEXT => {
            // LongText ($0028): txLoc (Point), count, text. No font
            // rasteriser, so glyph bytes are walked past — but the
            // explicit text-pen origin IS recorded. Inside Macintosh:
            // Imaging With QuickDraw, "About Basic QuickDraw" (page 2-13):
            // text baseline sits at the pen location, and `txLoc` is the
            // absolute Point that establishes it. Point order on disk is
            // (v, h); the crate's pen tuple is (h, v).
            let v = r.read_i16()? as i32;
            let h = r.read_i16()? as i32;
            let n = r.read_u8()? as usize;
            let text = r.read_bytes(n)?.to_vec();
            state.text_state.text_pen = Some((h, v));
            state.text_state.text_op_count += 1;
            render_text(canvas, state, &text);
            Ok(true)
        }
        OP_DH_TEXT => {
            // DHText ($0029): dh (0..255 unsigned), count, text. Advances
            // the running text pen rightward by `dh` relative to the
            // position the previous text opcode left. With no prior
            // LongText the pen advances from the graphics origin (0, 0).
            let dh = r.read_u8()? as i32;
            let n = r.read_u8()? as usize;
            let text = r.read_bytes(n)?.to_vec();
            advance_text_pen(state, dh, 0);
            render_text(canvas, state, &text);
            Ok(true)
        }
        OP_DV_TEXT => {
            // DVText ($002A): dv (0..255 unsigned), count, text. Advances
            // the running text pen downward by `dv`.
            let dv = r.read_u8()? as i32;
            let n = r.read_u8()? as usize;
            let text = r.read_bytes(n)?.to_vec();
            advance_text_pen(state, 0, dv);
            render_text(canvas, state, &text);
            Ok(true)
        }
        OP_DHDV_TEXT => {
            // DHDVText ($002B): dh, dv (each 0..255 unsigned), count,
            // text. Advances the running text pen by both deltas.
            let dh = r.read_u8()? as i32;
            let dv = r.read_u8()? as i32;
            let n = r.read_u8()? as usize;
            let text = r.read_bytes(n)?.to_vec();
            advance_text_pen(state, dh, dv);
            render_text(canvas, state, &text);
            Ok(true)
        }
        OP_FONT_NAME => {
            // §A-3 Table A-2 footnote `*`: the `fontName` payload begins
            // with a `dataLength` word that **includes itself**, so the
            // bytes-after-length = `dataLength - 2` and the total
            // additional-data column matches the table's `5 + nameLen`.
            // Round 236 promotes the walk-past path to a structured
            // [`PictFontName`] capture into `state.text_state.font_name`.
            let n = r.read_u16()? as usize;
            if n < 5 {
                // dataLength must cover at least itself (2) + oldFontID
                // (2) + nameLen (1) = 5 bytes minimum.
                return Err(PictError::invalid(format!(
                    "fontName dataLength {n} smaller than the 5-byte minimum"
                )));
            }
            let old_font_id = r.read_i16()?;
            let name_len = r.read_u8()? as usize;
            // Bytes already consumed since the length word: oldFontID
            // (2) + nameLen (1) = 3. Remaining = n - 2 - 3 = n - 5.
            let remaining = n.saturating_sub(5);
            if name_len > remaining {
                return Err(PictError::invalid(format!(
                    "fontName nameLength {name_len} exceeds remaining {remaining} bytes",
                )));
            }
            let name = r.read_bytes(name_len)?.to_vec();
            // Skip any padding the producer left at the tail (per the
            // spec footnote the table cell column is `5 + nameLen`, so
            // remaining-after-name should be zero, but a producer that
            // padded for word-alignment inside the record is tolerated).
            r.skip(remaining - name_len)?;
            state.text_state.font_name = Some(PictFontName::new(old_font_id, name));
            Ok(true)
        }
        OP_LINE_JUSTIFY => {
            // §A-3 Table A-2 footnote `†`: `dataLength` is the number of
            // bytes **after** the length word — should "always be 8"
            // (the two Fixed-32 fields). Round 236 captures the
            // intercharacter-spacing + total-extra-width pair into
            // `state.text_state.line_justify`.
            let n = r.read_u16()? as usize;
            if n < 8 {
                return Err(PictError::invalid(format!(
                    "lineJustify dataLength {n} smaller than the 8-byte payload",
                )));
            }
            let inter = Fixed(r.read_u32()? as i32);
            let extra = Fixed(r.read_u32()? as i32);
            // Tolerate trailing bytes the producer left beyond the
            // 8-byte minimum (§A-3 fixes the field but a v2 stream may
            // pad).
            r.skip(n - 8)?;
            state.text_state.line_justify = Some(PictLineJustify {
                inter_char_spacing: inter,
                total_extra: extra,
            });
            Ok(true)
        }
        OP_GLYPH_STATE => {
            // §A-3 Table A-2 row `$002E`: `dataLength` word + four
            // 1-byte Boolean flags. The Additional-data column says 8 —
            // i.e. dataLength = 6 with two pad bytes, or dataLength = 8
            // tolerating the producer's choice. Round 236 captures the
            // four Boolean fields into `state.text_state.glyph_state`.
            let n = r.read_u16()? as usize;
            if n < 4 {
                return Err(PictError::invalid(format!(
                    "glyphState dataLength {n} smaller than the 4-byte payload",
                )));
            }
            let outline_preferred = r.read_u8()? != 0;
            let preserve_glyph = r.read_u8()? != 0;
            let fractional_widths = r.read_u8()? != 0;
            let scaling_disabled = r.read_u8()? != 0;
            r.skip(n - 4)?;
            state.text_state.glyph_state = Some(PictGlyphState {
                outline_preferred,
                preserve_glyph,
                fractional_widths,
                scaling_disabled,
            });
            Ok(true)
        }
        OP_BK_PIX_PAT | OP_PN_PIX_PAT | OP_FILL_PIX_PAT => {
            // PixPat — multi-colour 8×8 pixel pattern. Inside Macintosh
            // §A-3 Listing A-1: `patType` (word), `Pat1Data` (8-byte
            // monochrome fallback), then either a `ditherPat` payload
            // (patType=2) or a `colourPixmap` payload (patType=1):
            // PixMap (sans baseAddr) + ColorTable + PixData.
            //
            // The monochrome `Pat1Data` is always stored in the
            // corresponding mono slot so that callers / code paths
            // which consult only the monochrome `pen_pat` / `back_pat` /
            // `fill_pat` (e.g. `paint_region_pattern`) still pick up a
            // reasonable fallback. The colour `*_pix_pat` slot is set
            // additionally for `patType=1` payloads.
            let (pat1, colour) = decode_pix_pat(r)?;
            match opcode {
                OP_PN_PIX_PAT => {
                    state.pen_pat = pat1;
                    state.pen_pix_pat = colour;
                }
                OP_BK_PIX_PAT => {
                    state.back_pat = pat1;
                    state.back_pix_pat = colour;
                }
                OP_FILL_PIX_PAT => {
                    state.fill_pat = pat1;
                    state.fill_pix_pat = colour;
                }
                _ => unreachable!("opcode filtered above"),
            }
            Ok(true)
        }
        // §A-3 Table A-2 text / pen / transfer-mode / highlight state
        // opcodes. Round 230: payload bytes are now captured into
        // `state.text_state` instead of stepped past silently, so the
        // producer's declared text shape + arithmetic-transfer-mode
        // op-colour can be recovered after the walk.
        OP_TX_FONT => {
            state.text_state.tx_font = r.read_i16()?;
            Ok(true)
        }
        OP_TX_FACE => {
            state.text_state.tx_face = crate::state::PictTextFace::from(r.read_u8()?);
            Ok(true)
        }
        OP_TX_MODE => {
            state.text_state.tx_mode = r.read_i16()?;
            Ok(true)
        }
        OP_SP_EXTRA => {
            state.text_state.sp_extra = Fixed(r.read_u32()? as i32);
            Ok(true)
        }
        OP_PN_MODE => {
            state.text_state.pn_mode = r.read_i16()?;
            Ok(true)
        }
        OP_TX_SIZE => {
            state.text_state.tx_size = r.read_i16()?;
            Ok(true)
        }
        OP_TX_RATIO => {
            // `TxRatio` = numerator (Point) + denominator (Point);
            // each `Point` is `(v: i16, h: i16)` on disk per §A-3
            // Table A-1.
            let numer_v = r.read_i16()?;
            let numer_h = r.read_i16()?;
            let denom_v = r.read_i16()?;
            let denom_h = r.read_i16()?;
            state.text_state.tx_ratio = TextRatio {
                numer_v,
                numer_h,
                denom_v,
                denom_h,
            };
            Ok(true)
        }
        OP_PN_LOC_HFRAC => {
            state.text_state.pn_loc_h_frac = r.read_i16()?;
            Ok(true)
        }
        OP_CH_EXTRA => {
            state.text_state.ch_extra = r.read_i16()?;
            Ok(true)
        }
        OP_HILITE_MODE => {
            state.text_state.hilite_mode_flag = true;
            Ok(true)
        }
        OP_HILITE_COLOR => {
            let (rr, gg, bb) = read_rgb16(r)?;
            state.text_state.hilite_color = Some(Rgba::from_rgb16(rr, gg, bb));
            state.text_state.hilite_default = false;
            Ok(true)
        }
        OP_DEF_HILITE => {
            state.text_state.hilite_default = true;
            state.text_state.hilite_color = None;
            Ok(true)
        }
        OP_OP_COLOR => {
            let (rr, gg, bb) = read_rgb16(r)?;
            state.text_state.op_color = Some(Rgba::from_rgb16(rr, gg, bb));
            Ok(true)
        }
        OP_SHORT_COMMENT => {
            let kind = r.read_u16()?;
            state.comments.push(PictComment::short(kind));
            Ok(true)
        }
        OP_LONG_COMMENT => {
            let kind = r.read_u16()?;
            let n = r.read_u16()? as usize;
            let data = r.read_bytes(n)?.to_vec();
            state.comments.push(PictComment::long(kind, data));
            Ok(true)
        }
        OP_PACK_BITS_RECT => {
            let (img, dst) = decode_pack_bits_rect(r)?;
            blit_subimage(canvas, state, &img, &dst);
            Ok(true)
        }
        OP_DIRECT_BITS_RECT => {
            let (img, dst) = decode_direct_bits_rect(r)?;
            blit_subimage(canvas, state, &img, &dst);
            Ok(true)
        }
        OP_PACK_BITS_RGN => {
            let (img, dst, rgn) = decode_pack_bits_rgn(r)?;
            blit_subimage_with_rgn(canvas, state, &img, &dst, Some(&rgn));
            Ok(true)
        }
        OP_DIRECT_BITS_RGN => {
            let (img, dst, rgn) = decode_direct_bits_rgn(r)?;
            blit_subimage_with_rgn(canvas, state, &img, &dst, Some(&rgn));
            Ok(true)
        }
        OP_BITS_RECT | OP_BITS_RGN => {
            // v2 BitsRect / BitsRgn — uncompressed BitMap (no
            // PackBits per row). Layout is identical to PackBitsRect
            // but with raw row data for each scan line. Round 42:
            // BitsRgn's embedded region is honoured as a transient
            // mask for this blit only.
            let (img, dst, rgn) = decode_bits_rect_v2(r, opcode == OP_BITS_RGN)?;
            blit_subimage_with_rgn(canvas, state, &img, &dst, rgn.as_ref());
            Ok(true)
        }
        OP_COMPRESSED_QUICKTIME | OP_UNCOMPRESSED_QUICKTIME => {
            // Embedded QuickTime image. Per §A-3 Table A-2 the payload
            // is `Data length (Long)` followed by `data length` bytes
            // (total additional data = `4 + data length` — the length
            // word excludes itself; round 401 fixes the previous
            // self-inclusive reading, which under-walked conforming
            // streams by 4 bytes). The bytes are captured verbatim
            // into `state.quicktime`, and — round 435 — the interior
            // is additionally parsed into the typed
            // `quicktime::QuickTimePayload` per Inside Macintosh:
            // QuickTime (1993) Tables 3-1 / 3-2.
            //
            // Degradation policy (page 3-26: the `Size` field must be
            // honoured even by a reader that cannot decode the
            // payload — a machine without QuickTime "ignores the new
            // opcodes"): an interior that fails the typed parse, or a
            // `$8201` whose embedded subopcode pixel data fails to
            // decode, keeps the verbatim capture with `image = None`
            // and leaves the canvas untouched instead of failing the
            // picture.
            let data_length = r.read_u32()? as usize;
            let data = r.read_bytes(data_length)?.to_vec();
            let compressed = opcode == OP_COMPRESSED_QUICKTIME;
            let image = if compressed {
                // `$8200`: the compressed image data is a CODEC-tag
                // boundary — the FourCC in the ImageDescription names
                // the decompressor, so no pixels land on the canvas
                // here. With the `registry` feature the caller routes
                // `image_description.codec` through oxideav-core's
                // resolver (`registry::resolve_quicktime_codec`).
                crate::quicktime::parse_compressed_quicktime(&data)
                    .ok()
                    .map(crate::quicktime::QuickTimePayload::Compressed)
            } else {
                // `$8201`: the wrapper embeds one ordinary `$98`–`$9B`
                // pixel-data subopcode whose bytes sit wholly inside
                // the `Size` window — re-enter the normal raster
                // dispatch on it and blit the result.
                crate::quicktime::parse_uncompressed_quicktime(&data)
                    .ok()
                    .and_then(|u| {
                        let mut sub = Reader::new(&u.sub_data);
                        let blitted = match u.subopcode {
                            OP_PACK_BITS_RECT => decode_pack_bits_rect(&mut sub)
                                .map(|(img, dst)| blit_subimage(canvas, state, &img, &dst)),
                            OP_PACK_BITS_RGN => {
                                decode_pack_bits_rgn(&mut sub).map(|(img, dst, rgn)| {
                                    blit_subimage_with_rgn(canvas, state, &img, &dst, Some(&rgn))
                                })
                            }
                            OP_DIRECT_BITS_RECT => decode_direct_bits_rect(&mut sub)
                                .map(|(img, dst)| blit_subimage(canvas, state, &img, &dst)),
                            OP_DIRECT_BITS_RGN => {
                                decode_direct_bits_rgn(&mut sub).map(|(img, dst, rgn)| {
                                    blit_subimage_with_rgn(canvas, state, &img, &dst, Some(&rgn))
                                })
                            }
                            // parse_uncompressed_quicktime guarantees
                            // the $98–$9B range.
                            _ => unreachable!("subopcode range enforced by the parser"),
                        };
                        blitted
                            .ok()
                            .map(|()| crate::quicktime::QuickTimePayload::Uncompressed(u))
                    })
            };
            state.quicktime.push(crate::image::PictQuickTime {
                compressed,
                data,
                image,
            });
            Ok(true)
        }
        _ => {
            // Fixed-size opcodes table covers everything from §A-3
            // we don't have a dedicated arm for.
            if let Some(n) = fixed_operand_size(opcode) {
                r.skip(n)?;
                Ok(true)
            } else if let Some(skip) = reserved_v2_payload_size(opcode) {
                // §A-3 reserved opcodes: known payload size, walked
                // past without dispatch so the rest of the picture
                // can render. Truncation inside the reserved payload
                // is still surfaced as `InvalidData` via `Reader::
                // read_*` / `skip`.
                skip_reserved_v2_payload(r, skip)?;
                Ok(true)
            } else {
                Err(PictError::unsupported(format!(
                    "unknown / unsupported v2 opcode 0x{opcode:04X} at offset {} \
                     (frame={pic_frame:?})",
                    r.pos - 2
                )))
            }
        }
    }
}

/// Walk past a §A-3 reserved v2 opcode's payload. Mirrors the
/// payload-shape spelled out by [`ReservedV2Skip`].
fn skip_reserved_v2_payload(r: &mut Reader<'_>, skip: ReservedV2Skip) -> Result<()> {
    match skip {
        ReservedV2Skip::Fixed(n) => r.skip(n),
        ReservedV2Skip::U16Prefixed => {
            let n = r.read_u16()? as usize;
            r.skip(n)
        }
        ReservedV2Skip::U32Prefixed => {
            let n = r.read_u32()? as usize;
            r.skip(n)
        }
        ReservedV2Skip::PolygonSized => {
            let n = r.read_u16()? as usize;
            if n < 2 {
                return Err(PictError::invalid(format!(
                    "reserved poly size {n} smaller than the 2-byte size word"
                )));
            }
            r.skip(n - 2)
        }
        ReservedV2Skip::RegionSized => {
            let n = r.read_u16()? as usize;
            if n < 2 {
                return Err(PictError::invalid(format!(
                    "reserved rgn size {n} smaller than the 2-byte size word"
                )));
            }
            r.skip(n - 2)
        }
    }
}

/// Static operand size for opcodes whose payload is a fixed number of
/// bytes per Inside Macintosh §A-3. Returning `None` means the opcode
/// has variable-size operands — those have their own match arm in
/// [`dispatch_v2_opcode`].
///
/// As of round 230 every §A-3 state-mutating opcode in this list has
/// been promoted to a dedicated match arm in `dispatch_v2_opcode` so
/// the table is effectively empty — kept as a private function (still
/// returning `Option<usize>`) for future per-opcode fixed-skip arms
/// that don't update [`PictState`].
fn fixed_operand_size(_opcode: u16) -> Option<usize> {
    None
}

fn read_rect_op(r: &mut Reader<'_>) -> Result<RectI32> {
    let (top, left, bottom, right) = r.read_rect()?;
    Ok(RectI32::from_be(top, left, bottom, right))
}

fn read_rgb16(r: &mut Reader<'_>) -> Result<(u16, u16, u16)> {
    let r_ = r.read_u16()?;
    let g_ = r.read_u16()?;
    let b_ = r.read_u16()?;
    Ok((r_, g_, b_))
}

/// Resolve the active pattern transfer mode from the drawing state.
///
/// Folds the §A-3 `PnMode` code together with the §4 colour context the
/// arithmetic transfer modes (`blend = 32` … `adMin = 39`) need — the
/// declared `OpColor` (`state.text_state.op_color`, defaulting per-mode
/// when absent) and the active background colour (`state.bg`, the
/// transparent-mode key). The §4 highlighting mode (`hilite = 50`)
/// additionally folds in the active `HiliteColor`
/// (`state.text_state.hilite_color`). Boolean pattern modes (`8..=15`)
/// ignore the colour context and resolve exactly as before.
#[inline]
fn pattern_mode(state: &PictState) -> PatternMode {
    PatternMode::from_pn_mode_with(
        state.text_state.pn_mode,
        state.text_state.op_color,
        state.bg,
        state.text_state.hilite_color,
    )
}

/// Draw a line honouring the current pen size, pen pattern (mono or
/// colour pixmap), and pen pattern mode. Inside Macintosh: Imaging With
/// QuickDraw §3 "QuickDraw Drawing Reference" (book page 3-81): the
/// `Line` / `LineTo` procedures draw "using the size, pattern, and
/// pattern mode of the graphics pen." The default solid 1×1 `patCopy`
/// `fgColor` pen keeps the historical thin-`fgColor` stroke bit-for-bit.
fn draw_line_pen(canvas: &mut Canvas, state: &PictState, x0: i32, y0: i32, x1: i32, y1: i32) {
    let (ph, pv) = state.pen_size;
    let mode = pattern_mode(state);
    if let Some(pp) = &state.pen_pix_pat {
        line_pix_pattern_thick(canvas, x0, y0, x1, y1, ph, pv, pp);
    } else if mode.is_pat_copy() && state.pen_pat.is_solid_fg() {
        draw_line_thick(canvas, x0, y0, x1, y1, ph, pv, state.fg);
    } else {
        line_pattern_thick_mode(
            canvas,
            x0,
            y0,
            x1,
            y1,
            ph,
            pv,
            state.pen_pat,
            state.fg,
            state.bg,
            mode,
        );
    }
}

/// Apply a `frame|paint|erase|invert|fill Rect` opcode (`opcode` ∈
/// `0x30..=0x34`) to the canvas. Inside Macintosh §2 / §A-3 ties each
/// verb to a distinct pattern slot:
///
/// * `frame` / `paint` use the **pen pattern** (`PnPat` / `PnPixPat`).
/// * `erase` uses the **background pattern** (`BkPat` / `BkPixPat`),
///   inverted for the monochrome path — on-bits select the background
///   colour, off-bits select the foreground.
/// * `fill` uses the **fill pattern** (`FillPat` / `FillPixPat`).
/// * `invert` ignores patterns entirely.
///
/// When a colour `*_pix_pat` is set, every cell renders the resolved
/// per-cell RGBA directly from the 8×8 grid (fg/bg are ignored).
fn apply_rect_verb(canvas: &mut Canvas, state: &PictState, opcode: u16, rect: RectI32) {
    let (top, left, bottom, right) = rect_to_canvas(state, rect);
    let (ph, pv) = state.pen_size;
    let mode = pattern_mode(state);
    match opcode & 0x000F {
        0 => {
            if let Some(pp) = &state.pen_pix_pat {
                frame_rect_pix_pattern_thick(canvas, top, left, bottom, right, ph, pv, pp);
            } else {
                frame_rect_pattern_thick_mode(
                    canvas,
                    top,
                    left,
                    bottom,
                    right,
                    ph,
                    pv,
                    state.pen_pat,
                    state.fg,
                    state.bg,
                    mode,
                );
            }
        }
        1 => {
            if let Some(pp) = &state.pen_pix_pat {
                fill_rect_pix_pattern(canvas, top, left, bottom, right, pp);
            } else {
                fill_rect_pattern_mode(
                    canvas,
                    top,
                    left,
                    bottom,
                    right,
                    state.pen_pat,
                    state.fg,
                    state.bg,
                    mode,
                );
            }
        }
        2 => {
            if let Some(pp) = &state.back_pix_pat {
                fill_rect_pix_pattern(canvas, top, left, bottom, right, pp);
            } else {
                fill_rect_pattern_mode(
                    canvas,
                    top,
                    left,
                    bottom,
                    right,
                    state.back_pat,
                    state.bg,
                    state.fg,
                    mode,
                );
            }
        }
        3 => invert_rect(canvas, top, left, bottom, right),
        4 => {
            if let Some(pp) = &state.fill_pix_pat {
                fill_rect_pix_pattern(canvas, top, left, bottom, right, pp);
            } else {
                fill_rect_pattern_mode(
                    canvas,
                    top,
                    left,
                    bottom,
                    right,
                    state.fill_pat,
                    state.fg,
                    state.bg,
                    mode,
                );
            }
        }
        _ => {}
    }
}

fn apply_rrect_verb(canvas: &mut Canvas, state: &PictState, opcode: u16, rect: RectI32) {
    let (top, left, bottom, right) = rect_to_canvas(state, rect);
    let (ow, oh) = state.oval_size;
    let (ph, pv) = state.pen_size;
    let mode = pattern_mode(state);
    match opcode & 0x000F {
        // frameRRect: honour the current pen size + pen pattern + pen
        // pattern mode (Inside Macintosh: Imaging With QuickDraw,
        // "Framing Shapes", book page 3-13 — FrameRoundRect draws its
        // outline "using the size, pattern, and pattern mode of the
        // graphics pen"). The default solid 1×1 `patCopy` `fgColor` pen
        // keeps the historical thin-outline path bit-for-bit.
        0 => {
            if let Some(pp) = &state.pen_pix_pat {
                frame_round_rect_pix_pattern_thick(
                    canvas, top, left, bottom, right, ow, oh, ph, pv, pp,
                );
            } else if ph <= 1 && pv <= 1 && mode.is_pat_copy() && state.pen_pat.is_solid_fg() {
                frame_round_rect(canvas, top, left, bottom, right, ow, oh, state.fg);
            } else {
                frame_round_rect_pattern_thick_mode(
                    canvas,
                    top,
                    left,
                    bottom,
                    right,
                    ow,
                    oh,
                    ph,
                    pv,
                    state.pen_pat,
                    state.fg,
                    state.bg,
                    mode,
                );
            }
        }
        1 => {
            if let Some(pp) = &state.pen_pix_pat {
                fill_round_rect_pix_pattern(canvas, top, left, bottom, right, ow, oh, pp);
            } else {
                fill_round_rect_pattern_mode(
                    canvas,
                    top,
                    left,
                    bottom,
                    right,
                    ow,
                    oh,
                    state.pen_pat,
                    state.fg,
                    state.bg,
                    mode,
                );
            }
        }
        2 => {
            if let Some(pp) = &state.back_pix_pat {
                fill_round_rect_pix_pattern(canvas, top, left, bottom, right, ow, oh, pp);
            } else {
                fill_round_rect_pattern_mode(
                    canvas,
                    top,
                    left,
                    bottom,
                    right,
                    ow,
                    oh,
                    state.back_pat,
                    state.bg,
                    state.fg,
                    mode,
                );
            }
        }
        3 => {
            // §3-44 InvertRoundRect / §A-3 Table A-2 `$0043`: channel-
            // wise NOT every pixel of the rounded-rect interior. Round
            // 252 swaps the round-2 frame-only placeholder for the spec
            // contract — round-trip (invert twice) restores the canvas.
            invert_round_rect(canvas, top, left, bottom, right, ow, oh)
        }
        4 => {
            if let Some(pp) = &state.fill_pix_pat {
                fill_round_rect_pix_pattern(canvas, top, left, bottom, right, ow, oh, pp);
            } else {
                fill_round_rect_pattern_mode(
                    canvas,
                    top,
                    left,
                    bottom,
                    right,
                    ow,
                    oh,
                    state.fill_pat,
                    state.fg,
                    state.bg,
                    mode,
                );
            }
        }
        _ => {}
    }
}

fn apply_oval_verb(canvas: &mut Canvas, state: &PictState, opcode: u16, rect: RectI32) {
    let (top, left, bottom, right) = rect_to_canvas(state, rect);
    let (ph, pv) = state.pen_size;
    let mode = pattern_mode(state);
    match opcode & 0x000F {
        // frameOval: honour the current pen size + pen pattern + pen
        // pattern mode (book page 3-13 "Framing Shapes"). The thick-pen
        // path was already honoured; round 372 adds the pen pattern /
        // pattern mode. A solid 1×1 `patCopy` `fgColor` pen keeps the
        // historical outline bit-for-bit.
        0 => {
            if let Some(pp) = &state.pen_pix_pat {
                frame_oval_pix_pattern_thick(canvas, top, left, bottom, right, ph, pv, pp);
            } else if mode.is_pat_copy() && state.pen_pat.is_solid_fg() {
                frame_oval_thick(canvas, top, left, bottom, right, ph, pv, state.fg);
            } else {
                frame_oval_pattern_thick_mode(
                    canvas,
                    top,
                    left,
                    bottom,
                    right,
                    ph,
                    pv,
                    state.pen_pat,
                    state.fg,
                    state.bg,
                    mode,
                );
            }
        }
        1 => {
            if let Some(pp) = &state.pen_pix_pat {
                fill_oval_pix_pattern(canvas, top, left, bottom, right, pp);
            } else {
                fill_oval_pattern_mode(
                    canvas,
                    top,
                    left,
                    bottom,
                    right,
                    state.pen_pat,
                    state.fg,
                    state.bg,
                    mode,
                );
            }
        }
        2 => {
            if let Some(pp) = &state.back_pix_pat {
                fill_oval_pix_pattern(canvas, top, left, bottom, right, pp);
            } else {
                fill_oval_pattern_mode(
                    canvas,
                    top,
                    left,
                    bottom,
                    right,
                    state.back_pat,
                    state.bg,
                    state.fg,
                    mode,
                );
            }
        }
        3 => {
            // §3-44 InvertOval / §A-3 Table A-2 `$0053`: channel-wise
            // NOT every pixel of the ellipse interior. Round 252
            // swaps the round-2 frame-only placeholder.
            invert_oval(canvas, top, left, bottom, right);
        }
        4 => {
            if let Some(pp) = &state.fill_pix_pat {
                fill_oval_pix_pattern(canvas, top, left, bottom, right, pp);
            } else {
                fill_oval_pattern_mode(
                    canvas,
                    top,
                    left,
                    bottom,
                    right,
                    state.fill_pat,
                    state.fg,
                    state.bg,
                    mode,
                );
            }
        }
        _ => {}
    }
}

fn apply_arc_verb(
    canvas: &mut Canvas,
    state: &PictState,
    opcode: u16,
    rect: RectI32,
    start: i32,
    arc: i32,
) {
    let (top, left, bottom, right) = rect_to_canvas(state, rect);
    let (ph, pv) = state.pen_size;
    let mode = pattern_mode(state);
    match opcode & 0x000F {
        // frameArc: honour the current pen size + pen pattern + pen
        // pattern mode (book page 3-13 "Framing Shapes" — FrameArc draws
        // its outline "using the size, pattern, and pattern mode of the
        // graphics pen"). The solid 1×1 `patCopy` `fgColor` pen keeps the
        // historical thin arc outline bit-for-bit.
        0 => {
            if let Some(pp) = &state.pen_pix_pat {
                frame_arc_pix_pattern_thick(
                    canvas, top, left, bottom, right, start, arc, ph, pv, pp,
                );
            } else if ph <= 1 && pv <= 1 && mode.is_pat_copy() && state.pen_pat.is_solid_fg() {
                frame_arc(canvas, top, left, bottom, right, start, arc, state.fg);
            } else {
                frame_arc_pattern_thick_mode(
                    canvas,
                    top,
                    left,
                    bottom,
                    right,
                    start,
                    arc,
                    ph,
                    pv,
                    state.pen_pat,
                    state.fg,
                    state.bg,
                    mode,
                );
            }
        }
        1 => fill_arc(canvas, top, left, bottom, right, start, arc, state.fg),
        2 => fill_arc(canvas, top, left, bottom, right, start, arc, state.bg),
        // §3-44 InvertArc / §A-3 Table A-2 `$0063`: channel-wise NOT
        // every pixel of the arc wedge interior. Round 252 swaps the
        // round-2 frame-only placeholder.
        3 => invert_arc(canvas, top, left, bottom, right, start, arc),
        4 => fill_arc(canvas, top, left, bottom, right, start, arc, state.fg),
        _ => {}
    }
}

fn apply_poly_verb(canvas: &mut Canvas, state: &PictState, opcode: u16, verts: &[(i32, i32)]) {
    let mode = pattern_mode(state);
    let (ph, pv) = state.pen_size;
    match opcode & 0x000F {
        // framePoly: honour the current pen size + pen pattern + pattern
        // mode. Inside Macintosh: Imaging With QuickDraw, "QuickDraw
        // Drawing Reference" (book page 3-81): the outline is drawn
        // "using the current graphics port's pen pattern, pattern mode,
        // and size", and the pen hangs below and to the right of each
        // boundary point. A solid pen with the default 1×1 size keeps the
        // historical thin-outline path.
        0 => {
            if let Some(pp) = &state.pen_pix_pat {
                frame_polygon_pix_pattern_thick(canvas, verts, ph, pv, pp);
            } else if ph <= 1 && pv <= 1 && mode.is_pat_copy() && state.pen_pat.is_solid_fg() {
                frame_polygon(canvas, verts, state.fg);
            } else {
                frame_polygon_pattern_thick_mode(
                    canvas,
                    verts,
                    ph,
                    pv,
                    state.pen_pat,
                    state.fg,
                    state.bg,
                    mode,
                );
            }
        }
        1 => {
            if let Some(pp) = &state.pen_pix_pat {
                fill_polygon_pix_pattern(canvas, verts, pp);
            } else {
                fill_polygon_pattern_mode(canvas, verts, state.pen_pat, state.fg, state.bg, mode);
            }
        }
        2 => {
            if let Some(pp) = &state.back_pix_pat {
                fill_polygon_pix_pattern(canvas, verts, pp);
            } else {
                fill_polygon_pattern_mode(canvas, verts, state.back_pat, state.bg, state.fg, mode);
            }
        }
        // §3-44 InvertPoly / §A-3 Table A-2 `$0073`: channel-wise NOT
        // every pixel of the even-odd polygon interior. Round 252 swaps
        // the round-2 frame-only placeholder.
        3 => invert_polygon(canvas, verts),
        4 => {
            if let Some(pp) = &state.fill_pix_pat {
                fill_polygon_pix_pattern(canvas, verts, pp);
            } else {
                fill_polygon_pattern_mode(canvas, verts, state.fill_pat, state.fg, state.bg, mode);
            }
        }
        _ => {}
    }
}

/// Trivial XOR invert of an RGBA rectangle. Each channel is bit-
/// inverted; alpha is preserved.
fn invert_rect(canvas: &mut Canvas, top: i32, left: i32, bottom: i32, right: i32) {
    if right <= left || bottom <= top {
        return;
    }
    for y in top..bottom {
        if y < 0 || (y as u32) >= canvas.height {
            continue;
        }
        for x in left..right {
            if x < 0 || (x as u32) >= canvas.width {
                continue;
            }
            let off = ((y as u32 * canvas.width + x as u32) * 4) as usize;
            canvas.data[off] = !canvas.data[off];
            canvas.data[off + 1] = !canvas.data[off + 1];
            canvas.data[off + 2] = !canvas.data[off + 2];
        }
    }
    canvas.dirty = true;
}

/// Paint a region's interior with `pat` between `fg` (on bits) and
/// `bg` (off bits) under the §3-44 pattern transfer mode `mode`.
/// `mode = PatCopy` (Inside Macintosh's `PnMode` default) hits the
/// solid-collapse fast paths from round 8; any other mode goes through
/// the per-pixel read-modify-write path so the §3-44 Boolean op
/// applies at every cell.
fn paint_region_pattern(
    canvas: &mut Canvas,
    state: &PictState,
    rgn: &Region,
    pat: Pattern,
    fg: Rgba,
    bg: Rgba,
) {
    let bb_w = rgn.width().max(0);
    let bb_h = rgn.height().max(0);
    if bb_w == 0 || bb_h == 0 {
        return;
    }
    let mode = pattern_mode(state);
    let solid = if mode.is_pat_copy() {
        if pat.is_solid_fg() {
            Some(fg)
        } else if pat.is_solid_bg() {
            Some(bg)
        } else {
            None
        }
    } else {
        None
    };
    if rgn.mask.is_none() {
        // Pure rectangular region — equivalent to fill_rect on the
        // bbox.
        let (top, left, bottom, right) = rect_to_canvas(state, rgn.bbox);
        if let Some(c) = solid {
            fill_rect(canvas, top, left, bottom, right, c);
        } else {
            fill_rect_pattern_mode(canvas, top, left, bottom, right, pat, fg, bg, mode);
        }
        return;
    }
    for y in rgn.bbox.top..rgn.bbox.bottom {
        for x in rgn.bbox.left..rgn.bbox.right {
            if rgn.contains(x, y) {
                let (cx, cy) = to_canvas(state, x, y);
                if let Some(c) = solid {
                    canvas.put(cx, cy, c);
                } else {
                    crate::raster::plot_region_cell_mode(canvas, cx, cy, pat, fg, bg, mode);
                }
            }
        }
    }
}

/// Paint a region's interior using a colour pix-pattern. Each pixel
/// inside the region takes its colour straight from the 8×8 tile.
fn paint_region_pix_pattern(canvas: &mut Canvas, state: &PictState, rgn: &Region, pp: &PixPattern) {
    let bb_w = rgn.width().max(0);
    let bb_h = rgn.height().max(0);
    if bb_w == 0 || bb_h == 0 {
        return;
    }
    if rgn.mask.is_none() {
        let (top, left, bottom, right) = rect_to_canvas(state, rgn.bbox);
        fill_rect_pix_pattern(canvas, top, left, bottom, right, pp);
        return;
    }
    for y in rgn.bbox.top..rgn.bbox.bottom {
        for x in rgn.bbox.left..rgn.bbox.right {
            if rgn.contains(x, y) {
                let (cx, cy) = to_canvas(state, x, y);
                canvas.put(cx, cy, pp.sample(cx, cy));
            }
        }
    }
}

/// Paint a region's outline (edges between inside / outside) with
/// the foreground colour.
/// `FrameRgn` — draw just the region's outline, honouring the graphics
/// pen. Inside Macintosh: Imaging With QuickDraw, "Framing Shapes" (book
/// page 3-13): *"Using the … FrameRgn procedure to frame a shape draws
/// just its outline, using the size, pattern, and pattern mode of the
/// graphics pen."* Like the round-333 `FramePoly` treatment, the outline
/// now consults `PnSize` (pen hangs below-and-right, book page 2-31),
/// the pen pattern (`PnPat` / `PnPixPat`), and the pen pattern mode
/// (`PnMode`), instead of the previous fixed 1-pixel `fgColor` trace.
fn paint_region_outline(canvas: &mut Canvas, state: &PictState, rgn: &Region) {
    let bb_w = rgn.width().max(0);
    let bb_h = rgn.height().max(0);
    if bb_w == 0 || bb_h == 0 {
        return;
    }
    let (ph, pv) = state.pen_size;
    let mode = pattern_mode(state);
    if rgn.mask.is_none() {
        // Rectangular region: the outline is exactly the bbox frame, so
        // reuse the pen-aware rect-frame primitives.
        let (top, left, bottom, right) = rect_to_canvas(state, rgn.bbox);
        if let Some(pp) = &state.pen_pix_pat {
            frame_rect_pix_pattern_thick(canvas, top, left, bottom, right, ph, pv, pp);
        } else {
            frame_rect_pattern_thick_mode(
                canvas,
                top,
                left,
                bottom,
                right,
                ph,
                pv,
                state.pen_pat,
                state.fg,
                state.bg,
                mode,
            );
        }
        return;
    }
    // Non-rectangular region: a boundary cell is a region-interior pixel
    // with at least one 4-neighbour outside the region. Stamp the pen at
    // each boundary cell (pen hangs below-and-right), honouring the pen
    // pattern + mode.
    for y in rgn.bbox.top..rgn.bbox.bottom {
        for x in rgn.bbox.left..rgn.bbox.right {
            if !rgn.contains(x, y) {
                continue;
            }
            let n_outside = !rgn.contains(x, y - 1)
                || !rgn.contains(x, y + 1)
                || !rgn.contains(x - 1, y)
                || !rgn.contains(x + 1, y);
            if !n_outside {
                continue;
            }
            let (cx, cy) = to_canvas(state, x, y);
            if let Some(pp) = &state.pen_pix_pat {
                stamp_region_pen_cell_pix(canvas, cx, cy, ph, pv, pp);
            } else {
                stamp_region_pen_cell_mode(
                    canvas,
                    cx,
                    cy,
                    ph,
                    pv,
                    state.pen_pat,
                    state.fg,
                    state.bg,
                    mode,
                );
            }
        }
    }
}

/// Materialise the supplied region into a canvas-local boolean clip
/// mask (`true` = pixel is inside the clip, drawing allowed). For a
/// purely rectangular region we install a pre-filled rectangle inside
/// the canvas-local bbox. For an inversion-encoded region we copy the
/// `Region`'s mask cell-by-cell into the canvas-local frame.
///
/// PICT `ClipRgn` semantics (Inside Macintosh: Imaging With QuickDraw
/// §2 "QuickDraw Drawing"): every subsequent drawing primitive is
/// painted only where the clip region permits. The mask survives
/// across opcodes until the next `ClipRgn` opcode.
fn install_clip_region(canvas: &mut Canvas, state: &PictState, rgn: &Region) {
    let cw = canvas.width as i32;
    let ch = canvas.height as i32;
    if cw <= 0 || ch <= 0 {
        return;
    }
    let mut mask = vec![false; (cw as usize) * (ch as usize)];
    // Translate region bbox into canvas-local coords once.
    // `to_canvas` returns (cx, cy).
    let (left, top) = to_canvas(state, rgn.bbox.left, rgn.bbox.top);
    let (right, bottom) = to_canvas(state, rgn.bbox.right, rgn.bbox.bottom);
    if right <= left || bottom <= top {
        canvas.clip = Some(mask);
        return;
    }
    let lo_x = left.max(0);
    let hi_x = right.min(cw);
    let lo_y = top.max(0);
    let hi_y = bottom.min(ch);
    match &rgn.mask {
        None => {
            for y in lo_y..hi_y {
                for x in lo_x..hi_x {
                    mask[(y * cw + x) as usize] = true;
                }
            }
        }
        Some(rgn_mask) => {
            let rgn_w = rgn.width().max(0);
            for y in lo_y..hi_y {
                for x in lo_x..hi_x {
                    // Convert canvas-local (x, y) back to picture-frame
                    // coords via state.origin, then to region-local
                    // coords via the rgn bbox.
                    let pic_x = x + state.origin.0;
                    let pic_y = y + state.origin.1;
                    let lx = pic_x - rgn.bbox.left;
                    let ly = pic_y - rgn.bbox.top;
                    if lx < 0 || ly < 0 || lx >= rgn_w {
                        continue;
                    }
                    let idx = (ly * rgn_w + lx) as usize;
                    if idx < rgn_mask.len() && rgn_mask[idx] {
                        mask[(y * cw + x) as usize] = true;
                    }
                }
            }
        }
    }
    canvas.clip = Some(mask);
}

fn invert_region(canvas: &mut Canvas, state: &PictState, rgn: &Region) {
    for y in rgn.bbox.top..rgn.bbox.bottom {
        for x in rgn.bbox.left..rgn.bbox.right {
            if !rgn.contains(x, y) {
                continue;
            }
            let (cx, cy) = to_canvas(state, x, y);
            if cx < 0 || cy < 0 || (cx as u32) >= canvas.width || (cy as u32) >= canvas.height {
                continue;
            }
            let off = ((cy as u32 * canvas.width + cx as u32) * 4) as usize;
            canvas.data[off] = !canvas.data[off];
            canvas.data[off + 1] = !canvas.data[off + 1];
            canvas.data[off + 2] = !canvas.data[off + 2];
            canvas.dirty = true;
        }
    }
}

/// One RGBA sub-image with its destination rectangle. Returned by
/// the various `decode_*_rect` raster opcode handlers so the canvas
/// blit happens in one place.
struct RasterSub {
    width: u32,
    height: u32,
    data: Vec<u8>,
    /// The record's on-disk `mode` (transfer mode) word — §A-3
    /// Listings A-2 / A-3 place it between `dstRect` and the pixel
    /// data on every raster opcode. Resolved against the active
    /// foreground / background / `OpColor` state at blit time.
    mode: u16,
}

fn blit_subimage(canvas: &mut Canvas, state: &PictState, img: &RasterSub, dst: &RectI32) {
    let (top, left, bottom, right) = rect_to_canvas(state, *dst);
    // §3-113 / §4 Table 4-1: the record's transfer-mode word governs
    // how source pixels combine with the destination. The §4
    // arithmetic band (32..=39) picks up the declared `OpColor`
    // (per-§4-40 defaults when absent) and the background colour as
    // the transparent-mode key — the same colour context the round-273
    // pattern path resolves. `srcCopy` under the fresh-GrafPort
    // black-fg / white-bg state is the §4-34 identity and takes the
    // raw-copy fast path inside `blit_mode`.
    let mode = SourceMode::from_mode_word(
        img.mode as i16,
        state.text_state.op_color,
        state.bg,
        state.text_state.hilite_color,
    );
    canvas.blit_mode(
        &img.data, img.width, img.height, top, left, bottom, right, mode, state.fg, state.bg,
    );
}

/// Blit with a transient region clip honoured for this opcode only.
/// `rgn` is the BitsRgn / PackBitsRgn / DirectBitsRgn embedded region
/// (per Inside Macintosh: Imaging With QuickDraw §A-3 — bitmap is
/// painted only where the region permits). The region intersects with
/// any pre-existing `ClipRgn` mask; both are restored afterwards.
fn blit_subimage_with_rgn(
    canvas: &mut Canvas,
    state: &PictState,
    img: &RasterSub,
    dst: &RectI32,
    rgn: Option<&Region>,
) {
    let Some(rgn) = rgn else {
        blit_subimage(canvas, state, img, dst);
        return;
    };
    // Stash any pre-existing clip, build the intersection of the
    // pre-existing clip and the per-blit region, install it as the
    // active clip, blit, then restore.
    let prev_clip = canvas.clip.take();
    let cw = canvas.width as i32;
    let ch = canvas.height as i32;
    let mut transient = vec![false; (cw as usize) * (ch as usize)];
    if cw > 0 && ch > 0 {
        for y in 0..ch {
            for x in 0..cw {
                let pic_x = x + state.origin.0;
                let pic_y = y + state.origin.1;
                let inside_rgn = rgn.contains(pic_x, pic_y);
                let inside_prev = match &prev_clip {
                    None => true,
                    Some(mask) => mask[(y * cw + x) as usize],
                };
                if inside_rgn && inside_prev {
                    transient[(y * cw + x) as usize] = true;
                }
            }
        }
    }
    canvas.clip = Some(transient);
    blit_subimage(canvas, state, img, dst);
    canvas.clip = prev_clip;
}

/// Crop a decoded `bounds`-sized RGBA source buffer down to the
/// `srcRect` sub-rectangle before it is blitted to `dstRect`.
///
/// Inside Macintosh: Imaging With QuickDraw §A-3 Listings A-2 / A-3
/// lay every raster opcode out as `PixMap`/`bounds`, then `srcRect`,
/// then `dstRect`. `CopyBits` semantics (the routine these opcodes
/// replay) copy the *`srcRect` sub-rectangle of the source pixel map*
/// and scale it onto `dstRect`; the decoded pixel buffer covers the
/// full `bounds`, of which `srcRect` selects the part actually drawn.
///
/// `bounds` and `src` are `(top, left, bottom, right)` in the shared
/// source coordinate space. The decoded `data` buffer is `bounds`-sized
/// (`width × height` where `width = bounds.right − bounds.left`). This
/// returns the `srcRect ∩ bounds` sub-image so the downstream scaling
/// blit maps `srcRect`, not `bounds`, onto `dstRect`.
///
/// When `srcRect ⊇ bounds` (the overwhelmingly common case where a
/// QuickDraw emitter sets `srcRect == bounds`) the intersection equals
/// `bounds` and the buffer is returned unchanged. A degenerate or
/// non-overlapping `srcRect` (which a well-formed PICT never emits)
/// falls back to the full buffer so a malformed record never silently
/// drops an otherwise-valid blit.
fn crop_to_src_rect(
    data: Vec<u8>,
    bounds: (i16, i16, i16, i16),
    src: (i16, i16, i16, i16),
) -> (Vec<u8>, u32, u32) {
    let b_top = bounds.0 as i32;
    let b_left = bounds.1 as i32;
    let b_bottom = bounds.2 as i32;
    let b_right = bounds.3 as i32;
    let bw = (b_right - b_left).max(0) as u32;
    let bh = (b_bottom - b_top).max(0) as u32;

    // Intersect srcRect with bounds, both in source coordinates.
    let s_top = (src.0 as i32).max(b_top);
    let s_left = (src.1 as i32).max(b_left);
    let s_bottom = (src.2 as i32).min(b_bottom);
    let s_right = (src.3 as i32).min(b_right);
    let cw = (s_right - s_left).max(0) as u32;
    let ch = (s_bottom - s_top).max(0) as u32;

    // No-op when srcRect already covers the whole bounds, or when the
    // intersection is empty / degenerate (malformed record) — return
    // the full buffer unchanged.
    if (cw == bw && ch == bh) || cw == 0 || ch == 0 {
        return (data, bw, bh);
    }

    // Offset of the cropped window inside the bounds-origin buffer.
    let ox = (s_left - b_left) as usize;
    let oy = (s_top - b_top) as usize;
    let bw_us = bw as usize;
    let cw_us = cw as usize;
    let mut out = vec![0u8; cw_us * (ch as usize) * 4];
    for y in 0..ch as usize {
        let src_row = ((oy + y) * bw_us + ox) * 4;
        let dst_row = y * cw_us * 4;
        out[dst_row..dst_row + cw_us * 4].copy_from_slice(&data[src_row..src_row + cw_us * 4]);
    }
    (out, cw, ch)
}

/// Final canvas → PictImage. Returns NoRaster if nothing was drawn
/// **and** no QuickTime payload was captured: a picture whose entire
/// content is a `CompressedQuickTime $8200` opcode (a JPEG-in-PICT
/// wrapper emitted with nothing but a clip and the payload — the
/// common shape for QuickTime-era files) is not empty; its image is
/// the embedded payload, which the caller routes to the codec named
/// by the `ImageDescription` FourCC (round 435). The canvas comes
/// back as the untouched background in that case.
fn finalise_canvas(canvas: Canvas, state: &PictState) -> Result<PictImage> {
    if !canvas.dirty && state.quicktime.is_empty() {
        return Err(PictError::NoRaster);
    }
    Ok(PictImage {
        width: canvas.width,
        height: canvas.height,
        pixel_format: PictPixelFormat::Rgba,
        data: canvas.data,
        pts: None,
        header: None,
        comments: state.comments.clone(),
        quicktime: state.quicktime.clone(),
        text_state: state.text_state.clone(),
    })
}

// ---------------------------------------------------------------------------
// Raster opcode decoders. Each returns `(RasterSub, dstRect)` so the
// canvas blit happens in one place.
// ---------------------------------------------------------------------------

/// `PackBitsRect` (`0x0098`).
///
/// Two on-disk layouts share this opcode (Inside Macintosh §A-3 footnote
/// `§` and Listing A-2):
///
/// * **BitMap** (`rowBytes` high bit clear) — 1-bit-per-pixel monochrome
///   `rowBytes(2) + bounds(8) + srcRect(8) + dstRect(8) + mode(2)` plus
///   per-row data (raw if `rowBytes < 8`, else `byteCount`-prefixed
///   PackBits). Round 1 default.
/// * **PixMap** (`rowBytes` high bit set) — indexed 1/2/4/8-bit pixels
///   resolved against an embedded `ColorTable`. Layout
///   `PixMap(46) + ColorTable + srcRect(8) + dstRect(8) + mode(2)` plus
///   per-row PixData (raw if `rowBytes < 8`, else PackBits at the
///   `rowBytes`-byte width). Round 186 (this opcode).
fn decode_pack_bits_rect(r: &mut Reader<'_>) -> Result<(RasterSub, RectI32)> {
    let row_bytes_raw = r.read_u16()?;
    if row_bytes_raw & 0x8000 != 0 {
        // Indexed PixMap variant. Note the high-bit reading is the only
        // way to disambiguate the two record families at this offset —
        // §A-3 footnote `§` ("The first word following the opcode is
        // rowBytes. If the high bit of rowBytes is set, then it is a
        // pixel map …").
        return decode_indexed_pixmap_payload(r, row_bytes_raw, /* packed= */ true, false)
            .map(|(s, d, _)| (s, d));
    }
    let row_bytes = row_bytes_raw as usize;
    let bounds = r.read_rect()?;
    let src_rect = r.read_rect()?;
    let dst_rect = r.read_rect()?;
    let mode = r.read_u16()?;

    let (width, height) = checked_bitmap_dims(bounds, row_bytes, 1)?;

    let mut bitmap = vec![0u8; row_bytes * height as usize];
    if row_bytes < 8 {
        for y in 0..height as usize {
            let row = r.read_bytes(row_bytes)?;
            bitmap[y * row_bytes..(y + 1) * row_bytes].copy_from_slice(row);
        }
    } else {
        for y in 0..height as usize {
            let _byte_count = if row_bytes > 250 {
                r.read_u16()? as usize
            } else {
                r.read_u8()? as usize
            };
            let dst = &mut bitmap[y * row_bytes..(y + 1) * row_bytes];
            packbits::decode_into(r, dst)?;
        }
    }

    let rgba = expand_1bpp_to_rgba(&bitmap, width, height, row_bytes);
    let (rgba, width, height) = crop_to_src_rect(rgba, bounds, src_rect);
    Ok((
        RasterSub {
            mode,
            width,
            height,
            data: rgba,
        },
        RectI32::from_be(dst_rect.0, dst_rect.1, dst_rect.2, dst_rect.3),
    ))
}

/// Shared bounds / rowBytes validation for the BitMap / indexed-PixMap
/// decode paths (round 401 hostile-input hardening). Returns the
/// `(width, height)` of `bounds` after checking that
///
/// * a row physically fits its declared pixels
///   (`width × bits_per_pixel ≤ row_bytes × 8`), and
/// * both the packed buffer (`row_bytes × height`) and the expanded
///   RGBA buffer (`width × height × 4`) fit the
///   [`MAX_RASTER_BYTES`] decode budget.
fn checked_bitmap_dims(
    bounds: (i16, i16, i16, i16),
    row_bytes: usize,
    bits_per_pixel: usize,
) -> Result<(u32, u32)> {
    let (width, height) = rect_dims(bounds);
    if (width as usize) * bits_per_pixel > row_bytes * 8 {
        return Err(PictError::invalid(format!(
            "bounds width {width} × {bits_per_pixel} bpp does not fit rowBytes {row_bytes}"
        )));
    }
    checked_raster_len(height as usize, row_bytes, "pixel-data buffer")?;
    checked_raster_len(height as usize, width as usize * 4, "RGBA buffer")?;
    Ok((width, height))
}

fn expand_1bpp_to_rgba(bitmap: &[u8], width: u32, height: u32, row_bytes: usize) -> Vec<u8> {
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let byte = bitmap[y * row_bytes + (x >> 3)];
            let bit = (byte >> (7 - (x & 7))) & 1;
            let v = if bit == 1 { 0x00 } else { 0xFF };
            let off = (y * width as usize + x) * 4;
            rgba[off] = v;
            rgba[off + 1] = v;
            rgba[off + 2] = v;
            rgba[off + 3] = 0xFF;
        }
    }
    rgba
}

/// v2 `BitsRect` (`0x0090`) — same opcode-record family as
/// `PackBitsRect`. `BitsRgn` (`0x0091`) adds a clipping region after the
/// rects; round 42 honours it as a transient mask for this blit
/// (returned alongside the raster + destination rect).
///
/// As with `PackBitsRect`, the high bit of `rowBytes` selects between
/// 1-bpp BitMap (round 1) and indexed PixMap (round 186) layouts. Per
/// §A-3 footnote `¶` the BitMap variant is restricted to
/// `rowBytes < 8` (the un-packed CopyBits path); when `rowBytes ≥ 8`
/// QuickDraw emitters always pick the packed `PackBitsRect` family.
fn decode_bits_rect_v2(
    r: &mut Reader<'_>,
    with_region: bool,
) -> Result<(RasterSub, RectI32, Option<Region>)> {
    let row_bytes_raw = r.read_u16()?;
    if row_bytes_raw & 0x8000 != 0 {
        // Indexed PixMap variant — same record layout as PackBitsRect /
        // PackBitsRgn but every row is raw.
        return decode_indexed_pixmap_payload(
            r,
            row_bytes_raw,
            /* packed= */ false,
            with_region,
        );
    }
    let row_bytes = row_bytes_raw as usize;
    let bounds = r.read_rect()?;
    let src_rect = r.read_rect()?;
    let dst_rect = r.read_rect()?;
    let mode = r.read_u16()?;
    let rgn = if with_region {
        Some(parse_region(r)?)
    } else {
        None
    };

    let (width, height) = checked_bitmap_dims(bounds, row_bytes, 1)?;

    let mut bitmap = vec![0u8; row_bytes * height as usize];
    for y in 0..height as usize {
        let row = r.read_bytes(row_bytes)?;
        bitmap[y * row_bytes..(y + 1) * row_bytes].copy_from_slice(row);
    }
    let rgba = expand_1bpp_to_rgba(&bitmap, width, height, row_bytes);
    let (rgba, width, height) = crop_to_src_rect(rgba, bounds, src_rect);
    Ok((
        RasterSub {
            mode,
            width,
            height,
            data: rgba,
        },
        RectI32::from_be(dst_rect.0, dst_rect.1, dst_rect.2, dst_rect.3),
        rgn,
    ))
}

/// `PackBitsRgn` (`0x0099`) — same as PackBitsRect plus a Region
/// clipping path inserted just before the per-row pixel data. The
/// BitMap-vs-PixMap split (`rowBytes` high bit) mirrors `PackBitsRect`.
fn decode_pack_bits_rgn(r: &mut Reader<'_>) -> Result<(RasterSub, RectI32, Region)> {
    let row_bytes_raw = r.read_u16()?;
    if row_bytes_raw & 0x8000 != 0 {
        let (img, dst, rgn) = decode_indexed_pixmap_payload(
            r,
            row_bytes_raw,
            /* packed= */ true,
            /* with_region= */ true,
        )?;
        // with_region=true guarantees rgn is Some; unwrap is safe.
        let rgn = rgn.ok_or_else(|| {
            PictError::invalid("PackBitsRgn indexed PixMap missing region payload")
        })?;
        return Ok((img, dst, rgn));
    }
    let row_bytes = row_bytes_raw as usize;
    let bounds = r.read_rect()?;
    let src_rect = r.read_rect()?;
    let dst_rect = r.read_rect()?;
    let mode = r.read_u16()?;
    let rgn = parse_region(r)?;

    let (width, height) = checked_bitmap_dims(bounds, row_bytes, 1)?;
    let mut bitmap = vec![0u8; row_bytes * height as usize];
    if row_bytes < 8 {
        for y in 0..height as usize {
            let row = r.read_bytes(row_bytes)?;
            bitmap[y * row_bytes..(y + 1) * row_bytes].copy_from_slice(row);
        }
    } else {
        for y in 0..height as usize {
            let _byte_count = if row_bytes > 250 {
                r.read_u16()? as usize
            } else {
                r.read_u8()? as usize
            };
            let dst = &mut bitmap[y * row_bytes..(y + 1) * row_bytes];
            packbits::decode_into(r, dst)?;
        }
    }

    let rgba = expand_1bpp_to_rgba(&bitmap, width, height, row_bytes);
    let (rgba, width, height) = crop_to_src_rect(rgba, bounds, src_rect);
    Ok((
        RasterSub {
            mode,
            width,
            height,
            data: rgba,
        },
        RectI32::from_be(dst_rect.0, dst_rect.1, dst_rect.2, dst_rect.3),
        rgn,
    ))
}

/// Shared indexed-PixMap payload reader for `BitsRect 0x0090`,
/// `BitsRgn 0x0091`, `PackBitsRect 0x0098` and `PackBitsRgn 0x0099` —
/// all four opcodes share Listing A-2 / A-3 layouts that begin with a
/// PixMap (sans baseAddr — the baseAddr placeholder is exclusive to
/// `DirectBitsRect 0x009A` / `DirectBitsRgn 0x009B` per §A-3 footnote
/// `§`) and an embedded `ColorTable`.
///
/// The caller has already consumed the `rowBytes` word and passes it in
/// as `row_bytes_raw` (high bit set; the BitMap path falls through in
/// the dedicated decoders above).
///
/// `packed=true` selects the per-row `byteCount`-prefixed PackBits path
/// from `PackBitsRect 0x0098` / `PackBitsRgn 0x0099`. `packed=false`
/// selects the raw-row path from `BitsRect 0x0090` / `BitsRgn 0x0091`.
/// In both cases, the BitMap variant restricts itself to `rowBytes < 8`
/// per §A-3 footnote `¶` — *"This data is unpacked. These opcodes can be
/// used only when rowBytes is less than 8."* The indexed PixMap variant
/// lifts that restriction.
///
/// `with_region=true` consumes a `Region` record after the `mode` word
/// (the `0x0091` / `0x0099` family). Returned as `Some` so the caller's
/// blit can apply the per-blit clip mask; `None` for the rectangle-only
/// opcodes.
///
/// Layout per Inside Macintosh §A-3 Listing A-2 / A-3:
///
/// ```text
/// PixMap (rest of, after rowBytes — 44 bytes):
///     bounds(8) + pmVersion(2) + packType(2) + packSize(4) +
///     hRes(4)  + vRes(4)       + pixelType(2) + pixelSize(2) +
///     cmpCount(2) + cmpSize(2) + planeBytes(4) + pmTable(4) +
///     pmReserved(4)
/// ColorTable:
///     ctSeed(4) + ctFlags(2) + ctSize(2) + (ctSize+1) × ColorSpec(8)
///     where ColorSpec = value(2) + red(2) + green(2) + blue(2)
/// srcRect(8) + dstRect(8) + mode(2)
/// [maskRgn — only for `with_region=true` opcodes 0x0091 / 0x0099]
/// PixData (per §A-3 "PixData"):
///     IF rowBytes < 8 OR NOT packed: data unpacked, rowBytes * height bytes
///     ELSE: per-row PackBits at the rowBytes-byte width, byteCount-prefixed
/// ```
///
/// The decoded indexed pixels are resolved against the embedded
/// `ColorTable` and surfaced as RGBA. Pixel sizes 1 / 2 / 4 / 8 are
/// honoured per §4 ("Color QuickDraw and PixMaps"); other sizes return
/// `PictError::unsupported`. Out-of-range palette indices fall back to
/// black (the QuickDraw convention for unassigned colour entries on a
/// truncated `ColorTable`).
fn decode_indexed_pixmap_payload(
    r: &mut Reader<'_>,
    row_bytes_raw: u16,
    packed: bool,
    with_region: bool,
) -> Result<(RasterSub, RectI32, Option<Region>)> {
    // PixMap header — the `rowBytes` word was already consumed by the
    // caller (it is the high-bit dispatch we just performed). The
    // remaining 44 bytes match the §A-3 listing in `decode_pix_pat`.
    let row_bytes = (row_bytes_raw & 0x3FFF) as usize;
    let bounds = r.read_rect()?;
    let _pm_version = r.read_u16()?;
    let _pack_type_field = r.read_u16()?;
    let _pack_size = r.read_u32()?;
    let _h_res = r.read_u32()?;
    let _v_res = r.read_u32()?;
    let _pixel_type = r.read_u16()?;
    let pixel_size = r.read_u16()?;
    let _cmp_count = r.read_u16()?;
    let _cmp_size = r.read_u16()?;
    let _plane_bytes = r.read_u32()?;
    let _pm_table = r.read_u32()?;
    let _pm_reserved = r.read_u32()?;

    // ColorTable (ctSeed + ctFlags + ctSize + ColorSpec[]). Each
    // ColorSpec carries the pixel value (index) it maps to, so the
    // palette is keyed by that value, not by array position — see
    // `read_color_table_value_keyed`.
    let palette = read_color_table_value_keyed(r, "indexed PixMap")?;

    let src_rect = r.read_rect()?;
    let dst_rect = r.read_rect()?;
    let mode = r.read_u16()?;
    let rgn = if with_region {
        Some(parse_region(r)?)
    } else {
        None
    };

    if !matches!(pixel_size, 1 | 2 | 4 | 8) {
        return Err(PictError::unsupported(format!(
            "indexed PixMap pixelSize {pixel_size} (expected 1/2/4/8)"
        )));
    }
    let (width, height) = checked_bitmap_dims(bounds, row_bytes, pixel_size as usize)?;

    // PixData: raw rows when `rowBytes < 8` (§A-3 "PixData") or when the
    // caller is a `BitsRect` / `BitsRgn` (unpacked opcodes; `packed=false`).
    // Otherwise per-row PackBits at the rowBytes-byte width.
    let mut pix_data = vec![0u8; row_bytes * height as usize];
    if row_bytes < 8 || !packed {
        if row_bytes > 0 && height > 0 {
            let raw = r.read_bytes(row_bytes * height as usize)?;
            pix_data.copy_from_slice(raw);
        }
    } else {
        for y in 0..height as usize {
            let _byte_count = if row_bytes > 250 {
                r.read_u16()? as usize
            } else {
                r.read_u8()? as usize
            };
            let dst = &mut pix_data[y * row_bytes..(y + 1) * row_bytes];
            packbits::decode_into(r, dst)?;
        }
    }

    let rgba = resolve_indexed_pixmap(&pix_data, width, height, row_bytes, pixel_size, &palette)?;
    let (rgba, width, height) = crop_to_src_rect(rgba, bounds, src_rect);
    Ok((
        RasterSub {
            mode,
            width,
            height,
            data: rgba,
        },
        RectI32::from_be(dst_rect.0, dst_rect.1, dst_rect.2, dst_rect.3),
        rgn,
    ))
}

const COLOR_TABLE_DEVICE_FLAG: u16 = 0x8000;

/// Read a `ColorTable` record (already past the PixMap header) and
/// return a **value-keyed** palette: a 256-entry `Vec<Rgba>` where slot
/// `i` holds the RGB of the `ColorSpec` whose `value` field equals `i`.
///
/// Inside Macintosh: Imaging With QuickDraw §4 ("Color QuickDraw
/// Reference", book page 4-55): each `ColorSpec` carries *"the pixel
/// value assigned … for the color specified in the rgb field"*, and for
/// indexed devices *"the pixel value is an index number."* The pixel
/// index in the PixData therefore selects the ColorSpec **whose `value`
/// equals that index**, not the N-th array slot. Real PICT colour tables
/// usually store entries in sequential `value` order (0, 1, 2, …), in
/// which case value-keying is identical to position-keying — but a table
/// with a non-sequential `value` field (legal per §4) is now mapped
/// correctly instead of being mis-coloured by position.
///
/// The table is sized to 256 (the maximum index an 8-bpp indexed PixMap
/// can reference) and BLACK-filled, so an index with no matching
/// `value` resolves to black — the documented QuickDraw fallback for an
/// empty `ctTable` slot. A `value` outside `0..=255` is ignored (it can
/// never be referenced by a ≤8-bpp PixData index).
///
/// Inside Macintosh: Imaging With QuickDraw identifies `$0000` as a
/// pixel-map color table and `$8000` as an indexed-device color table
/// (§4, book page 4-104; the structure summary repeats "high bit: 0 =
/// PixMap; 1 = device" on page 4-120). Apple's *develop* Issue 1,
/// "Palette Manager", makes the indexing rule explicit: device tables
/// are sequential, so `ColorSpec` array position supplies the pixel
/// value. Other tables use the explicit `ColorSpec.value` mapping.
fn read_color_table_value_keyed(r: &mut Reader<'_>, context: &str) -> Result<Vec<Rgba>> {
    let _ct_seed = r.read_u32()?;
    let ct_flags = r.read_u16()?;
    let ct_size = r.read_i16()?;
    if !(0..=255).contains(&ct_size) {
        return Err(PictError::invalid(format!(
            "{context} ColorTable ctSize out of range: {ct_size}"
        )));
    }
    let n_entries = (ct_size as usize) + 1;
    let mut palette = vec![Rgba::BLACK; 256];
    for index in 0..n_entries {
        let value = r.read_u16()?;
        let value = if ct_flags & COLOR_TABLE_DEVICE_FLAG != 0 {
            index as u16
        } else {
            value
        };
        let r16 = r.read_u16()?;
        let g16 = r.read_u16()?;
        let b16 = r.read_u16()?;
        if (value as usize) < palette.len() {
            palette[value as usize] = Rgba::from_rgb16(r16, g16, b16);
        }
    }
    Ok(palette)
}

/// Resolve an indexed PixData buffer into a `width × height` RGBA
/// surface against `palette`. Pixel sizes 1 / 2 / 4 / 8 are honoured.
/// Out-of-range indices (e.g. a PixData entry larger than
/// `palette.len()`) map to black (`Rgba::BLACK`) — the documented
/// QuickDraw fallback for an empty palette slot per §4 ("Color
/// QuickDraw and PixMaps" — *"Empty entries in the ctTable array are
/// drawn as black"*).
fn resolve_indexed_pixmap(
    pix_data: &[u8],
    width: u32,
    height: u32,
    row_bytes: usize,
    pixel_size: u16,
    palette: &[Rgba],
) -> Result<Vec<u8>> {
    let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let idx = read_indexed_pixel(pix_data, x, y, row_bytes, pixel_size)?;
            let c = if (idx as usize) < palette.len() {
                palette[idx as usize]
            } else {
                Rgba::BLACK
            };
            let off = (y * width as usize + x) * 4;
            rgba[off] = c.r;
            rgba[off + 1] = c.g;
            rgba[off + 2] = c.b;
            rgba[off + 3] = c.a;
        }
    }
    Ok(rgba)
}

/// `DirectBitsRgn` (`0x009B`) — same as DirectBitsRect plus a Region
/// clipping path inserted just before the per-row pixel data.
fn decode_direct_bits_rgn(r: &mut Reader<'_>) -> Result<(RasterSub, RectI32, Region)> {
    let header = read_pixmap_header(r)?;
    let src_rect = r.read_rect()?;
    let dst_rect = r.read_rect()?;
    let mode = r.read_u16()?;
    let rgn = parse_region(r)?;
    let (rgba, dst) = decode_direct_bits_pixels(r, &header, dst_rect)?;
    let (rgba, width, height) = crop_to_src_rect(rgba, header.bounds, src_rect);
    Ok((
        RasterSub {
            mode,
            width,
            height,
            data: rgba,
        },
        dst,
        rgn,
    ))
}

/// PixMap header sub-fields read from a DirectBitsRect / DirectBitsRgn
/// opcode body.
struct PixMapHeader {
    row_bytes: usize,
    width: u32,
    height: u32,
    pack_type: u16,
    pixel_size: u16,
    cmp_count: u16,
    cmp_size: u16,
    /// The PixMap `bounds` rectangle `(top, left, bottom, right)` in the
    /// source coordinate space, retained so the DirectBits decoders can
    /// crop the decoded buffer to `srcRect` (Listing A-2 / A-3).
    bounds: (i16, i16, i16, i16),
}

/// Read the DirectBits[Rect|Rgn] PixMap header: baseAddr, rowBytes,
/// bounds, pmVersion, packType, packSize, hRes, vRes, pixelType,
/// pixelSize, cmpCount, cmpSize, planeBytes, pmTable, pmReserved.
fn read_pixmap_header(r: &mut Reader<'_>) -> Result<PixMapHeader> {
    let _base_addr = r.read_u32()?;
    let row_bytes_raw = r.read_u16()?;
    if row_bytes_raw & 0x8000 == 0 {
        return Err(PictError::invalid(
            "DirectBitsRect rowBytes top bit is clear (looks like a BitMap, not a PixMap)",
        ));
    }
    let row_bytes = (row_bytes_raw & 0x3FFF) as usize;
    let bounds = r.read_rect()?;
    let _pm_version = r.read_u16()?;
    let pack_type = r.read_u16()?;
    let _pack_size = r.read_u32()?;
    let _h_res = r.read_u32()?;
    let _v_res = r.read_u32()?;
    let _pixel_type = r.read_u16()?;
    let pixel_size = r.read_u16()?;
    let cmp_count = r.read_u16()?;
    let cmp_size = r.read_u16()?;
    let _plane_bytes = r.read_u32()?;
    let _pm_table = r.read_u32()?;
    let _pm_reserved = r.read_u32()?;

    let (width, height) = rect_dims(bounds);
    // Hostile-input hardening (round 401): the decode buffers
    // (`width × height × 4` RGBA plus any packed intermediates) must
    // fit the MAX_RASTER_BYTES budget. The per-row `rowBytes`-vs-width
    // fit is checked in the raw decoders that index into a
    // `rowBytes`-sized row — it can't live here because packType 2
    // legitimately carries 3 bytes/pixel rows (the pad byte is
    // dropped) while the raw 32-bit form carries 4.
    checked_raster_len(
        height as usize,
        row_bytes.max(width as usize * 4),
        "DirectBits buffer",
    )?;

    Ok(PixMapHeader {
        row_bytes,
        width,
        height,
        pack_type,
        pixel_size,
        cmp_count,
        cmp_size,
        bounds,
    })
}

/// `DirectBitsRect` (`0x009A`).
fn decode_direct_bits_rect(r: &mut Reader<'_>) -> Result<(RasterSub, RectI32)> {
    let header = read_pixmap_header(r)?;
    let src_rect = r.read_rect()?;
    let dst_rect = r.read_rect()?;
    let mode = r.read_u16()?;
    let (rgba, dst) = decode_direct_bits_pixels(r, &header, dst_rect)?;
    let (rgba, width, height) = crop_to_src_rect(rgba, header.bounds, src_rect);
    Ok((
        RasterSub {
            mode,
            width,
            height,
            data: rgba,
        },
        dst,
    ))
}

/// Decode the per-row pixel data of a DirectBits[Rect|Rgn] opcode
/// into a `width × height` RGBA buffer. Honours packType 1 (raw),
/// 2 (24-byte interleaved BGR for 32-bit), 3 (16-bit PackBits) and
/// 4 (component-separated PackBits).
///
/// `packType = 0` selects **default packing** per §A-3 page A-16. The
/// PixData pseudocode forces *unpacked* data whenever `rowBytes < 8`
/// regardless of `packType`; above that threshold the documented
/// default for a `pixelSize` of 16 is **type 3** (per-scanline 16-bit
/// PackBits) and for a `pixelSize` of 32 is **type 4** (per-scanline
/// component-separated PackBits). We resolve `packType = 0` to the
/// matching concrete decoder before dispatch so a default-packed
/// `DirectBits` emitter (`packType = 0`, `rowBytes ≥ 8`) decodes
/// correctly instead of being mis-read as raw pixel rows.
fn decode_direct_bits_pixels(
    r: &mut Reader<'_>,
    h: &PixMapHeader,
    dst_rect: (i16, i16, i16, i16),
) -> Result<(Vec<u8>, RectI32)> {
    let dst = RectI32::from_be(dst_rect.0, dst_rect.1, dst_rect.2, dst_rect.3);
    // Sizing in usize — `read_pixmap_header` already budget-checked
    // this buffer, but the multiply must not wrap u32 either.
    let mut rgba = vec![0u8; h.width as usize * h.height as usize * 4];
    // §A-3 page A-16: resolve the default-packing alias before match.
    // `rowBytes < 8` keeps data unpacked (raw) for either pixel size.
    let pack_type = match (h.pack_type, h.pixel_size, h.row_bytes < 8) {
        (0, 16, false) => 3,
        (0, 32, false) => 4,
        (0, _, true) => 1,
        (pt, _, _) => pt,
    };
    match (pack_type, h.pixel_size) {
        (1, 16) => decode_dbr_16bpp_raw(r, h, &mut rgba)?,
        (1, 32) => decode_dbr_32bpp_raw(r, h, &mut rgba)?,
        (2, 32) => decode_dbr_32bpp_packtype2(r, h, &mut rgba)?,
        (3, 16) => decode_dbr_16bpp_packbits(r, h, &mut rgba)?,
        (4, 32) => decode_dbr_32bpp_planar_packbits(r, h, &mut rgba)?,
        _ => {
            return Err(PictError::unsupported(format!(
                "DirectBitsRect packType={} pixelSize={} cmpCount={} cmpSize={}",
                h.pack_type, h.pixel_size, h.cmp_count, h.cmp_size
            )));
        }
    }
    Ok((rgba, dst))
}

/// packType 1, pixelSize 16: A1R5G5B5 packed BE u16 per pixel.
fn decode_dbr_16bpp_raw(r: &mut Reader<'_>, h: &PixMapHeader, rgba: &mut [u8]) -> Result<()> {
    if !(h.cmp_count == 3 && h.cmp_size == 5) {
        return Err(PictError::unsupported(format!(
            "DirectBitsRect 16bpp expects cmpCount=3 cmpSize=5, got {}/{}",
            h.cmp_count, h.cmp_size
        )));
    }
    // Round 401: a raw row is indexed at 2 bytes per pixel, so it must
    // physically fit the declared bounds width.
    if h.width as usize * 2 > h.row_bytes {
        return Err(PictError::invalid(format!(
            "DirectBits 16bpp raw bounds width {} does not fit rowBytes {}",
            h.width, h.row_bytes
        )));
    }
    for y in 0..h.height as usize {
        let row = r.read_bytes(h.row_bytes)?;
        write_16bpp_row(row, h.width as usize, &mut rgba[y * h.width as usize * 4..]);
    }
    Ok(())
}

fn write_16bpp_row(row: &[u8], width: usize, rgba_row: &mut [u8]) {
    for x in 0..width {
        let p = u16::from_be_bytes([row[x * 2], row[x * 2 + 1]]);
        let r5 = ((p >> 10) & 0x1F) as u8;
        let g5 = ((p >> 5) & 0x1F) as u8;
        let b5 = (p & 0x1F) as u8;
        let off = x * 4;
        rgba_row[off] = (r5 << 3) | (r5 >> 2);
        rgba_row[off + 1] = (g5 << 3) | (g5 >> 2);
        rgba_row[off + 2] = (b5 << 3) | (b5 >> 2);
        rgba_row[off + 3] = 0xFF;
    }
}

/// packType 1, pixelSize 32: 4 bytes per pixel. cmpCount=3 means
/// 0xFF R G B; cmpCount=4 means A R G B.
fn decode_dbr_32bpp_raw(r: &mut Reader<'_>, h: &PixMapHeader, rgba: &mut [u8]) -> Result<()> {
    if !(h.cmp_size == 8 && (h.cmp_count == 3 || h.cmp_count == 4)) {
        return Err(PictError::unsupported(format!(
            "DirectBitsRect 32bpp expects cmpSize=8 cmpCount=3|4, got {}/{}",
            h.cmp_count, h.cmp_size
        )));
    }
    // Round 401: a raw row is indexed at 4 bytes per pixel, so it must
    // physically fit the declared bounds width.
    if h.width as usize * 4 > h.row_bytes {
        return Err(PictError::invalid(format!(
            "DirectBits 32bpp raw bounds width {} does not fit rowBytes {}",
            h.width, h.row_bytes
        )));
    }
    for y in 0..h.height as usize {
        let row = r.read_bytes(h.row_bytes)?;
        write_32bpp_row(
            row,
            h.width as usize,
            h.cmp_count,
            &mut rgba[y * h.width as usize * 4..],
        );
    }
    Ok(())
}

fn write_32bpp_row(row: &[u8], width: usize, cmp_count: u16, rgba_row: &mut [u8]) {
    for x in 0..width {
        let off_in = x * 4;
        let off_out = x * 4;
        let (a, rr, gg, bb) = if cmp_count == 4 {
            (
                row[off_in],
                row[off_in + 1],
                row[off_in + 2],
                row[off_in + 3],
            )
        } else {
            (0xFF, row[off_in + 1], row[off_in + 2], row[off_in + 3])
        };
        rgba_row[off_out] = rr;
        rgba_row[off_out + 1] = gg;
        rgba_row[off_out + 2] = bb;
        rgba_row[off_out + 3] = a;
    }
}

/// packType 2, pixelSize 32: 3 bytes per pixel (R G B), no pad byte
/// — the only difference vs packType 1 is that the alpha / fill byte
/// is omitted. cmpCount=3, cmpSize=8.
fn decode_dbr_32bpp_packtype2(r: &mut Reader<'_>, h: &PixMapHeader, rgba: &mut [u8]) -> Result<()> {
    if !(h.cmp_size == 8 && h.cmp_count == 3) {
        return Err(PictError::unsupported(format!(
            "DirectBitsRect packType=2 expects cmpCount=3 cmpSize=8, got {}/{}",
            h.cmp_count, h.cmp_size
        )));
    }
    let stride = (h.width as usize) * 3;
    for y in 0..h.height as usize {
        let row = r.read_bytes(stride)?;
        for x in 0..h.width as usize {
            let off_in = x * 3;
            let off_out = (y * h.width as usize + x) * 4;
            rgba[off_out] = row[off_in];
            rgba[off_out + 1] = row[off_in + 1];
            rgba[off_out + 2] = row[off_in + 2];
            rgba[off_out + 3] = 0xFF;
        }
    }
    Ok(())
}

/// packType 3, pixelSize 16: PackBits-compressed per row, where the
/// byte unit replicated by the encoder is a `u16` (so a "run of 5"
/// produces 5 u16 pixels, 10 bytes). The per-row byteCount prefix is
/// 1 byte if `rowBytes < 250` and 2 bytes otherwise.
fn decode_dbr_16bpp_packbits(r: &mut Reader<'_>, h: &PixMapHeader, rgba: &mut [u8]) -> Result<()> {
    if !(h.cmp_count == 3 && h.cmp_size == 5) {
        return Err(PictError::unsupported(format!(
            "DirectBitsRect packType=3 16bpp expects cmpCount=3 cmpSize=5, got {}/{}",
            h.cmp_count, h.cmp_size
        )));
    }
    let row_pixels = h.width as usize;
    let mut row_buf = vec![0u8; row_pixels * 2];
    for y in 0..h.height as usize {
        let _byte_count = if h.row_bytes > 250 {
            r.read_u16()? as usize
        } else {
            r.read_u8()? as usize
        };
        decode_packbits_u16_into(r, &mut row_buf)?;
        write_16bpp_row(&row_buf, row_pixels, &mut rgba[y * row_pixels * 4..]);
    }
    Ok(())
}

/// PackBits where the replicated unit is a `u16` (big-endian). Used
/// by DirectBitsRect packType 3.
fn decode_packbits_u16_into(src: &mut Reader<'_>, out: &mut [u8]) -> Result<()> {
    let expected_pixels = out.len() / 2;
    let mut written = 0usize;
    while written < expected_pixels {
        let flag = src.read_u8()? as i8;
        if flag >= 0 {
            // Raw packet: copy next (flag + 1) u16 pixels verbatim.
            let n = flag as usize + 1;
            if written + n > expected_pixels {
                return Err(PictError::invalid(format!(
                    "PackBits16 raw packet overruns row: {written} + {n} > {expected_pixels}"
                )));
            }
            let bytes = src.read_bytes(n * 2)?;
            out[written * 2..(written + n) * 2].copy_from_slice(bytes);
            written += n;
        } else if flag == -128 {
            continue;
        } else {
            // Run packet: replicate next u16 (1 - flag) times.
            let n = (1 - flag as i32) as usize;
            if written + n > expected_pixels {
                return Err(PictError::invalid(format!(
                    "PackBits16 run packet overruns row: {written} + {n} > {expected_pixels}"
                )));
            }
            let lo = src.read_u8()?;
            let hi = src.read_u8()?;
            for px in 0..n {
                out[(written + px) * 2] = lo;
                out[(written + px) * 2 + 1] = hi;
            }
            written += n;
        }
    }
    Ok(())
}

/// packType 4, pixelSize 32: per-row component-separated PackBits.
/// The decoder expects N planes per row (N = cmpCount = 3 or 4),
/// each plane PackBits-compressed at u8 unit size, stored
/// concatenated. After decoding all planes we interleave them into
/// RGBA.
fn decode_dbr_32bpp_planar_packbits(
    r: &mut Reader<'_>,
    h: &PixMapHeader,
    rgba: &mut [u8],
) -> Result<()> {
    if !(h.cmp_size == 8 && (h.cmp_count == 3 || h.cmp_count == 4)) {
        return Err(PictError::unsupported(format!(
            "DirectBitsRect packType=4 32bpp expects cmpCount=3|4 cmpSize=8, got {}/{}",
            h.cmp_count, h.cmp_size
        )));
    }
    let n_planes = h.cmp_count as usize;
    let row_pixels = h.width as usize;
    let plane_bytes = row_pixels;
    let mut plane_buf = vec![0u8; plane_bytes * n_planes];
    for y in 0..h.height as usize {
        let _byte_count = if h.row_bytes > 250 {
            r.read_u16()? as usize
        } else {
            r.read_u8()? as usize
        };
        // The per-row byteCount covers ALL planes together; PackBits
        // packets reset between planes since we know each plane is
        // exactly `row_pixels` bytes long.
        for p in 0..n_planes {
            packbits::decode_into(r, &mut plane_buf[p * plane_bytes..(p + 1) * plane_bytes])?;
        }
        // Interleave: plane order is R, G, B (cmpCount=3) or A, R,
        // G, B (cmpCount=4).
        for x in 0..row_pixels {
            let off_out = (y * row_pixels + x) * 4;
            if n_planes == 4 {
                let a = plane_buf[x];
                let rr = plane_buf[plane_bytes + x];
                let gg = plane_buf[2 * plane_bytes + x];
                let bb = plane_buf[3 * plane_bytes + x];
                rgba[off_out] = rr;
                rgba[off_out + 1] = gg;
                rgba[off_out + 2] = bb;
                rgba[off_out + 3] = a;
            } else {
                let rr = plane_buf[x];
                let gg = plane_buf[plane_bytes + x];
                let bb = plane_buf[2 * plane_bytes + x];
                rgba[off_out] = rr;
                rgba[off_out + 1] = gg;
                rgba[off_out + 2] = bb;
                rgba[off_out + 3] = 0xFF;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// PixPat decoder (BkPixPat / PnPixPat / FillPixPat opcode payload).
// ---------------------------------------------------------------------------

/// Pattern-type constants per Inside Macintosh §A-3 Listing A-1.
const PAT_TYPE_COLOUR_PIXMAP: u16 = 1;
const PAT_TYPE_DITHER: u16 = 2;

/// Decode a PixPat record payload (the bytes that follow a `BkPixPat`
/// `0x0012`, `PnPixPat` `0x0013` or `FillPixPat` `0x0014` opcode word).
///
/// Returns `(Pat1Data, Option<PixPattern>)`:
///
/// * The 8-byte monochrome `Pat1Data` field is always extracted —
///   classic QuickDraw uses it as a fall-through when the colour
///   pixel-pattern can't be honoured.
/// * `Some(PixPattern)` is returned for both colour sub-types:
///   * `patType=1` — colour-pixmap payload successfully resolved
///     against its embedded `ColorTable`.
///   * `patType=2` — dither sub-type, expanded via
///     [`PixPattern::from_dither_rgb`] (round 95): the on-disk record
///     carries a single `RGBColor` that Color QuickDraw's `MakeRGBPat`
///     expands at draw time. Our true-colour canvas renders the
///     target RGB exactly at every cell — see
///     [`PixPattern::from_dither_rgb`] for the spec citation.
/// * `None` is returned only when the colour pixmap's tile bounds
///   aren't 8×8 (non-standard PixMap dimensions fall back to the
///   `Pat1Data` monochrome stipple).
///
/// Layout per Inside Macintosh: Imaging With QuickDraw §A-3 Listing A-1:
///
/// ```text
/// PatType:    word                      (2 bytes; 1 = colour pixmap, 2 = dither)
/// Pat1Data:   Pattern                   (8 bytes — monochrome fallback)
///
/// IF PatType = 1 (colour pixmap):
///   PixMap:     PixMap (sans baseAddr)  (rowBytes, bounds, pmVersion, packType, packSize,
///                                        hRes, vRes, pixelType, pixelSize, cmpCount, cmpSize,
///                                        planeBytes, pmTable, pmReserved = 46 bytes)
///   ColorTable: ColorTable              (8 bytes header + 8 bytes per ColorSpec entry,
///                                        ctSize is the count minus 1)
///   PixData:    PixData                 (per-row packed/raw bytes per §A-3)
///
/// IF PatType = 2 (dither):
///   RGB:        RGBColor                (6 bytes — desired R/G/B; tile is computed at draw
///                                        time by the QuickDraw dither engine. Round 91 stops
///                                        short of implementing this; the bytes are skipped
///                                        and the caller falls back to Pat1Data.)
/// ```
///
/// The `PixMap` is the same record laid out in DirectBitsRect — minus
/// the 4-byte `baseAddr` placeholder. Inside Macintosh's Listing A-2
/// (BitsRect / PackBitsRect) uses the same "sans-baseAddr" convention
/// for embedded PixMaps. (DirectBitsRect / DirectBitsRgn explicitly
/// retain the placeholder because they pre-date the convention; see
/// §A-3 "Opcodes $009A (DirectBitsRect) and $009B (DirectBitsRgn) …
/// store the baseAddr field … set to $000000FF".)
fn decode_pix_pat(r: &mut Reader<'_>) -> Result<(Pattern, Option<PixPattern>)> {
    let pat_type = r.read_u16()?;
    // Pat1Data — always present regardless of sub-type.
    let pat1_bytes = r.read_bytes(8)?;
    let mut pat1 = [0u8; 8];
    pat1.copy_from_slice(pat1_bytes);
    let pat1 = Pattern(pat1);

    match pat_type {
        PAT_TYPE_COLOUR_PIXMAP => {
            // PixMap (sans baseAddr) — 46 bytes.
            let row_bytes_raw = r.read_u16()?;
            // Top bit of rowBytes flags PixMap vs BitMap. PixPat always
            // expects PixMap; reject BitMap-flag PixPat as malformed.
            if row_bytes_raw & 0x8000 == 0 && row_bytes_raw > 0 {
                return Err(PictError::invalid(format!(
                    "PixPat PixMap rowBytes={row_bytes_raw:#06X} top bit clear (BitMap?)"
                )));
            }
            let row_bytes = (row_bytes_raw & 0x3FFF) as usize;
            let bounds = r.read_rect()?;
            let _pm_version = r.read_u16()?;
            let _pack_type = r.read_u16()?;
            let _pack_size = r.read_u32()?;
            let _h_res = r.read_u32()?;
            let _v_res = r.read_u32()?;
            let _pixel_type = r.read_u16()?;
            let pixel_size = r.read_u16()?;
            let _cmp_count = r.read_u16()?;
            let _cmp_size = r.read_u16()?;
            let _plane_bytes = r.read_u32()?;
            let _pm_table = r.read_u32()?;
            let _pm_reserved = r.read_u32()?;

            // PixPat tiles are power-of-2 sided (checked below), but
            // the dims / rowBytes / budget still need the hostile-
            // input validation before `pix_data` is sized from them.
            if !matches!(pixel_size, 1 | 2 | 4 | 8) {
                return Err(PictError::unsupported(format!(
                    "PixPat pixelSize {pixel_size} (expected 1/2/4/8)"
                )));
            }
            let (width, height) = checked_bitmap_dims(bounds, row_bytes, pixel_size as usize)?;
            let (width, height) = (width as usize, height as usize);

            // ColorTable.
            let ct_seed = r.read_u32()?;
            let _ct_flags = r.read_i16()?;
            let ct_size = r.read_i16()?; // entries = ct_size + 1
            if !(0..=255).contains(&ct_size) {
                return Err(PictError::invalid(format!(
                    "PixPat ColorTable ctSize out of range: {ct_size}"
                )));
            }
            let n_entries = (ct_size as usize) + 1;
            let mut palette: Vec<Rgba> = Vec::with_capacity(n_entries);
            for _ in 0..n_entries {
                let _value = r.read_u16()?;
                let r16 = r.read_u16()?;
                let g16 = r.read_u16()?;
                let b16 = r.read_u16()?;
                palette.push(Rgba::from_rgb16(r16, g16, b16));
            }

            // PixData — per Inside Macintosh §A-3 ("PixData"):
            //   IF rowBytes < 8: data unpacked, rowBytes * height bytes.
            //   ELSE: per-row PackBits with byteCount prefix (1 byte if
            //         rowBytes <= 250, else 2 bytes).
            let mut pix_data: Vec<u8> = vec![0u8; row_bytes * height];
            if row_bytes < 8 {
                let raw = r.read_bytes(row_bytes * height)?;
                pix_data.copy_from_slice(raw);
            } else {
                for y in 0..height {
                    let _bc = if row_bytes > 250 {
                        r.read_u16()? as usize
                    } else {
                        r.read_u8()? as usize
                    };
                    let dst = &mut pix_data[y * row_bytes..(y + 1) * row_bytes];
                    crate::packbits::decode_into(r, dst)?;
                }
            }

            // Resolve indexed-pixel PixData against the palette into a
            // `width`×`height` RGBA grid. Inside Macintosh §3 (book page
            // 3-40) — *"A pixel pattern … can be of any width and height
            // that's a power of 2."* — so we honour any power-of-2 tile,
            // not just the universal 8×8 case (round 91). A degenerate
            // (zero-dimension) or non-power-of-2 tile, or a tile too large
            // to back with the PixData we read, falls back to the Pat1Data
            // monochrome interpretation (`None`).
            if width == 0
                || height == 0
                || !width.is_power_of_two()
                || !height.is_power_of_two()
            {
                return Ok((pat1, None));
            }

            let mut pixels = vec![Rgba::BLACK; width * height];
            for y in 0..height {
                for x in 0..width {
                    let idx = read_indexed_pixel(&pix_data, x, y, row_bytes, pixel_size)?;
                    pixels[y * width + x] = if (idx as usize) < palette.len() {
                        palette[idx as usize]
                    } else {
                        // Out-of-range index → fall back to Pat1's
                        // foreground/background interpretation for that
                        // cell. Choose foreground (the "ink") since the
                        // colour PixPat normally describes the same
                        // visual texture as Pat1Data.
                        if pat1.sample(x as i32, y as i32) {
                            Rgba::BLACK
                        } else {
                            Rgba::WHITE
                        }
                    };
                }
            }
            let _ = ct_seed; // unused; retained in read order.

            Ok((
                pat1,
                Some(PixPattern::new(width as u16, height as u16, pixels, pat1)),
            ))
        }
        PAT_TYPE_DITHER => {
            // Dither sub-type — `RGBColor` (6 bytes: r16, g16, b16)
            // follows. Inside Macintosh §A-3 Listing A-1.
            //
            // Per Inside Macintosh §4 ("Color QuickDraw" → "Pixel
            // Patterns"), the on-disk record carries **only** the
            // target colour; the 8×8 tile itself is computed at draw
            // time by `MakeRGBPat` against the active `GDevice`
            // palette — *"For an RGB pixel pattern, the RGBColor
            // record that you specify to the MakeRGBPat procedure
            // defines the image; there is no image data."* — and
            // the §4.90 MakeRGBPat description states *"this
            // implementation opted for a fast pattern selection
            // rather than the best possible pattern selection"*,
            // confirming the bit-pattern is implementation-defined.
            //
            // Our rasteriser draws to a true-colour RGBA canvas
            // (no indexed `GDevice` in the loop), so the spec
            // contract — *"approximates the color you specify in
            // the myColor parameter"* — reduces to "emit the target
            // RGB at every cell." This satisfies both the §4
            // colour-approximation requirement (zero approximation
            // error on a 24-bit canvas) and the §A-3 luminance
            // guarantee (*"QuickDraw draws pixel patterns created
            // with the MakeRGBPat procedure as bit patterns having
            // approximately the same luminance as the pixel
            // patterns"*) by construction.
            let r16 = r.read_u16()?;
            let g16 = r.read_u16()?;
            let b16 = r.read_u16()?;
            let rgb = Rgba::from_rgb16(r16, g16, b16);
            Ok((pat1, Some(PixPattern::from_dither_rgb(rgb, pat1))))
        }
        other => Err(PictError::unsupported(format!(
            "PixPat patType={other} (only 1=colourPixmap, 2=ditherPat are documented in IM §A-3 Listing A-1)"
        ))),
    }
}

/// Read one indexed pixel from PixData at column `x`, row `y`.
///
/// Supports the four indexed pixelSize values Inside Macintosh §4
/// enumerates: 1, 2, 4, 8 bits per pixel. The bit order within a byte
/// is MSB-first per QuickDraw convention.
fn read_indexed_pixel(
    pix_data: &[u8],
    x: usize,
    y: usize,
    row_bytes: usize,
    pixel_size: u16,
) -> Result<u8> {
    let row = &pix_data[y * row_bytes..(y + 1) * row_bytes];
    match pixel_size {
        1 => Ok((row[x >> 3] >> (7 - (x & 7))) & 0x01),
        2 => Ok((row[x >> 2] >> ((3 - (x & 3)) * 2)) & 0x03),
        4 => Ok((row[x >> 1] >> ((1 - (x & 1)) * 4)) & 0x0F),
        8 => Ok(row[x]),
        other => Err(PictError::unsupported(format!(
            "PixPat indexed pixelSize={other} (expected 1/2/4/8)"
        ))),
    }
}

// ---------------------------------------------------------------------------
// PICT v1 opcode walker.
// ---------------------------------------------------------------------------

/// PICT v1 opcodes (per Inside Macintosh §A-3) — 8 bits each, NO
/// word alignment between opcodes. The opcode set is a strict subset
/// of v2's: same numbering for the shape verbs, but raster opcodes
/// only go up to 0x99 and the opcode operand sizes are unchanged.
fn parse_v1_opcodes(
    r: &mut Reader<'_>,
    pic_frame: RectI32,
    mut canvas: Canvas,
    mut state: PictState,
) -> Result<PictImage> {
    while !r.at_eof() {
        let opcode = r.read_u8()? as u16;
        if !dispatch_v1_opcode(r, opcode, &pic_frame, &mut canvas, &mut state)? {
            break;
        }
    }
    finalise_canvas(canvas, &state)
}

fn dispatch_v1_opcode(
    r: &mut Reader<'_>,
    opcode: u16,
    pic_frame: &RectI32,
    canvas: &mut Canvas,
    state: &mut PictState,
) -> Result<bool> {
    // v1 opcodes share v2's numbering for the small shape / state ops
    // (one byte instead of two, no word alignment). Raster ops differ.
    match opcode {
        0x00 => Ok(true),
        0xFF => Ok(false), // OpEndPic in v1 is one byte 0xFF
        0x01 => {
            let rgn = parse_region(r)?;
            install_clip_region(canvas, state, &rgn);
            Ok(true)
        }
        0x02 => {
            // BkPat (v1 opcode 0x02): 8-byte background pattern.
            let bytes = r.read_bytes(8)?;
            let mut p = [0u8; 8];
            p.copy_from_slice(bytes);
            state.back_pat = Pattern(p);
            state.back_pix_pat = None;
            Ok(true)
        }
        0x09 => {
            // PnPat (v1 opcode 0x09): 8-byte pen pattern.
            let bytes = r.read_bytes(8)?;
            let mut p = [0u8; 8];
            p.copy_from_slice(bytes);
            state.pen_pat = Pattern(p);
            state.pen_pix_pat = None;
            Ok(true)
        }
        0x0A => {
            // FillPat (v1 opcode 0x0A): 8-byte fill pattern.
            let bytes = r.read_bytes(8)?;
            let mut p = [0u8; 8];
            p.copy_from_slice(bytes);
            state.fill_pat = Pattern(p);
            state.fill_pix_pat = None;
            Ok(true)
        }
        // §A-3 Table A-3 text / pen / font state opcodes — round 230
        // promotes these from "walk past the payload" to "capture into
        // `state.text_state`" so v1 PICTs surface the producer's
        // declared text shape just like v2 ones do.
        0x03 => {
            // TxFont (Integer)
            state.text_state.tx_font = r.read_i16()?;
            Ok(true)
        }
        0x04 => {
            // TxFace (0..255)
            state.text_state.tx_face = crate::state::PictTextFace::from(r.read_u8()?);
            Ok(true)
        }
        0x05 => {
            // TxMode (Integer)
            state.text_state.tx_mode = r.read_i16()?;
            Ok(true)
        }
        0x06 => {
            // SpExtra (Fixed)
            state.text_state.sp_extra = Fixed(r.read_u32()? as i32);
            Ok(true)
        }
        0x07 => {
            let v = r.read_i16()?;
            let h = r.read_i16()?;
            state.pen_size = (h as i32, v as i32);
            Ok(true)
        }
        0x08 => {
            // PnMode (Integer)
            state.text_state.pn_mode = r.read_i16()?;
            Ok(true)
        }
        0x0B => {
            let v = r.read_i16()?;
            let h = r.read_i16()?;
            state.oval_size = (h as i32, v as i32);
            Ok(true)
        }
        0x0C => {
            // Origin: same SetOrigin-delta semantics as the v2 arm —
            // positive deltas move subsequent shapes up / left (Inside
            // Macintosh: Imaging With QuickDraw §2, book pages 2-23 f.;
            // round 401 sign fix).
            let dh = r.read_i16()?;
            let dv = r.read_i16()?;
            state.origin.0 += dh as i32;
            state.origin.1 += dv as i32;
            Ok(true)
        }
        0x0D => {
            // TxSize (Integer)
            state.text_state.tx_size = r.read_i16()?;
            Ok(true)
        }
        0x0E => {
            let code = r.read_u32()?;
            state.fg = Rgba::from_pascal_colour(code);
            Ok(true)
        }
        0x0F => {
            let code = r.read_u32()?;
            state.bg = Rgba::from_pascal_colour(code);
            Ok(true)
        }
        0x10 => {
            // TxRatio: numerator (Point) + denominator (Point) = 8 bytes
            let numer_v = r.read_i16()?;
            let numer_h = r.read_i16()?;
            let denom_v = r.read_i16()?;
            let denom_h = r.read_i16()?;
            state.text_state.tx_ratio = TextRatio {
                numer_v,
                numer_h,
                denom_v,
                denom_h,
            };
            Ok(true)
        }
        0x20 => {
            let pt0_v = r.read_i16()? as i32;
            let pt0_h = r.read_i16()? as i32;
            let pt1_v = r.read_i16()? as i32;
            let pt1_h = r.read_i16()? as i32;
            let (x0, y0) = to_canvas(state, pt0_h, pt0_v);
            let (x1, y1) = to_canvas(state, pt1_h, pt1_v);
            draw_line_pen(canvas, state, x0, y0, x1, y1);
            state.pen = (pt1_h, pt1_v);
            Ok(true)
        }
        0x21 => {
            let pt_v = r.read_i16()? as i32;
            let pt_h = r.read_i16()? as i32;
            let (x0, y0) = to_canvas(state, state.pen.0, state.pen.1);
            let (x1, y1) = to_canvas(state, pt_h, pt_v);
            draw_line_pen(canvas, state, x0, y0, x1, y1);
            state.pen = (pt_h, pt_v);
            Ok(true)
        }
        0x22 => {
            let pt_v = r.read_i16()? as i32;
            let pt_h = r.read_i16()? as i32;
            let dh = r.read_u8()? as i8 as i32;
            let dv = r.read_u8()? as i8 as i32;
            let nx = pt_h + dh;
            let ny = pt_v + dv;
            let (x0, y0) = to_canvas(state, pt_h, pt_v);
            let (x1, y1) = to_canvas(state, nx, ny);
            draw_line_pen(canvas, state, x0, y0, x1, y1);
            state.pen = (nx, ny);
            Ok(true)
        }
        0x23 => {
            let dh = r.read_u8()? as i8 as i32;
            let dv = r.read_u8()? as i8 as i32;
            let nx = state.pen.0 + dh;
            let ny = state.pen.1 + dv;
            let (x0, y0) = to_canvas(state, state.pen.0, state.pen.1);
            let (x1, y1) = to_canvas(state, nx, ny);
            draw_line_pen(canvas, state, x0, y0, x1, y1);
            state.pen = (nx, ny);
            Ok(true)
        }
        // §A-3 Table A-3 text opcodes — same pen-tracking + rasteriser as
        // v2. The glyph bytes are drawn through the crate's built-in
        // clean-room ASCII face ([`crate::font`]); PICT carries no font
        // data so this is the spec-grounded stand-in for the system font.
        0x28 => {
            // LongText: txLoc (Point=v,h) + count (byte) + text. Point
            // order on disk is (v, h); the crate's pen tuple is (h, v).
            let v = r.read_i16()? as i32;
            let h = r.read_i16()? as i32;
            let n = r.read_u8()? as usize;
            let text = r.read_bytes(n)?.to_vec();
            state.text_state.text_pen = Some((h, v));
            state.text_state.text_op_count += 1;
            render_text(canvas, state, &text);
            Ok(true)
        }
        0x29 => {
            // DHText: dh (byte, 0..255) + count (byte) + text.
            let dh = r.read_u8()? as i32;
            let n = r.read_u8()? as usize;
            let text = r.read_bytes(n)?.to_vec();
            advance_text_pen(state, dh, 0);
            render_text(canvas, state, &text);
            Ok(true)
        }
        0x2A => {
            // DVText: dv (byte, 0..255) + count (byte) + text.
            let dv = r.read_u8()? as i32;
            let n = r.read_u8()? as usize;
            let text = r.read_bytes(n)?.to_vec();
            advance_text_pen(state, 0, dv);
            render_text(canvas, state, &text);
            Ok(true)
        }
        0x2B => {
            // DHDVText: dh (byte) + dv (byte) + count (byte) + text.
            let dh = r.read_u8()? as i32;
            let dv = r.read_u8()? as i32;
            let n = r.read_u8()? as usize;
            let text = r.read_bytes(n)?.to_vec();
            advance_text_pen(state, dh, dv);
            render_text(canvas, state, &text);
            Ok(true)
        }
        0x30..=0x34 | 0x40..=0x44 | 0x50..=0x54 => {
            let rect = read_rect_op(r)?;
            match opcode {
                0x30..=0x34 => {
                    state.last_rect = Some(rect);
                    apply_rect_verb(canvas, state, opcode, rect);
                }
                0x40..=0x44 => {
                    state.last_rrect = Some(rect);
                    apply_rrect_verb(canvas, state, opcode, rect);
                }
                _ => {
                    state.last_oval = Some(rect);
                    apply_oval_verb(canvas, state, opcode, rect);
                }
            }
            Ok(true)
        }
        // §A-3 Table A-3 *Same* shape opcodes — no operand payload; reuse
        // the corresponding last-* rect from the drawing-state machine.
        // Verb-nibble mapping: low nibble of opcode - 8 = base verb (0x38
        // → frame, 0x39 → paint, 0x3A → erase, 0x3B → invert, 0x3C →
        // fill). When the matching last-* slot is empty (no prior verb
        // of that family has executed), §A-3 leaves the behaviour
        // implementation-defined; we silently do nothing — matching
        // QuickDraw's "no previous shape to repeat" no-op semantics.
        0x38..=0x3C => {
            if let Some(rect) = state.last_rect {
                apply_rect_verb(canvas, state, opcode - 8, rect);
            }
            Ok(true)
        }
        0x48..=0x4C => {
            if let Some(rect) = state.last_rrect {
                apply_rrect_verb(canvas, state, opcode - 8, rect);
            }
            Ok(true)
        }
        0x58..=0x5C => {
            if let Some(rect) = state.last_oval {
                apply_oval_verb(canvas, state, opcode - 8, rect);
            }
            Ok(true)
        }
        0x60..=0x64 => {
            let rect = read_rect_op(r)?;
            let start = r.read_i16()? as i32;
            let arc = r.read_i16()? as i32;
            state.last_arc_rect = Some(rect);
            apply_arc_verb(canvas, state, opcode, rect, start, arc);
            Ok(true)
        }
        0x68..=0x6C => {
            // frameSameArc..fillSameArc: 4-byte payload = start + arc;
            // the rect is taken from the last-arc-rect state slot.
            let start = r.read_i16()? as i32;
            let arc = r.read_i16()? as i32;
            if let Some(rect) = state.last_arc_rect {
                apply_arc_verb(canvas, state, opcode - 8, rect, start, arc);
            }
            Ok(true)
        }
        // §A-3 Table A-3 lists frameSamePoly..fillSamePoly (0x78..0x7C)
        // and frameSameRgn..fillSameRgn (0x88..0x8C) as "(Not yet
        // implemented)" with a 0-byte payload. QuickDraw itself never
        // emits these; we accept them as no-ops so a private-extension
        // PICT carrying one keeps decoding.
        0x78..=0x7C | 0x88..=0x8C => Ok(true),
        0x70..=0x74 => {
            let poly_size = r.read_u16()? as usize;
            if poly_size < 10 {
                return Err(PictError::invalid(format!(
                    "v1 polygon size {poly_size} smaller than 10-byte header"
                )));
            }
            let _bbox = r.read_rect()?;
            let n_verts = (poly_size - 10) / 4;
            let mut verts = Vec::with_capacity(n_verts);
            for _ in 0..n_verts {
                let v = r.read_i16()? as i32;
                let h = r.read_i16()? as i32;
                let (cx, cy) = to_canvas(state, h, v);
                verts.push((cx, cy));
            }
            apply_poly_verb(canvas, state, opcode, &verts);
            Ok(true)
        }
        0x80..=0x84 => {
            let rgn = parse_region(r)?;
            match opcode & 0x0F {
                0 => paint_region_outline(canvas, state, &rgn),
                1 => paint_region_pattern(canvas, state, &rgn, state.pen_pat, state.fg, state.bg),
                4 => paint_region_pattern(canvas, state, &rgn, state.fill_pat, state.fg, state.bg),
                2 => paint_region_pattern(canvas, state, &rgn, state.back_pat, state.bg, state.fg),
                3 => invert_region(canvas, state, &rgn),
                _ => {}
            }
            Ok(true)
        }
        0x90 => {
            let (img, dst, _rgn) = decode_bits_rect_v2(r, false)?;
            blit_subimage(canvas, state, &img, &dst);
            Ok(true)
        }
        0x91 => {
            let (img, dst, rgn) = decode_bits_rect_v2(r, true)?;
            blit_subimage_with_rgn(canvas, state, &img, &dst, rgn.as_ref());
            Ok(true)
        }
        0x98 => {
            let (img, dst) = decode_pack_bits_rect(r)?;
            blit_subimage(canvas, state, &img, &dst);
            Ok(true)
        }
        0x99 => {
            let (img, dst, rgn) = decode_pack_bits_rgn(r)?;
            blit_subimage_with_rgn(canvas, state, &img, &dst, Some(&rgn));
            Ok(true)
        }
        // v1 DirectBitsRect / DirectBitsRgn — same PixMap header layout
        // as their v2 counterparts (0x009A / 0x009B). QuickDraw emitted
        // these for 16- and 32-bit direct colour in v1 picture files.
        0x9A => {
            let (img, dst) = decode_direct_bits_rect(r)?;
            blit_subimage(canvas, state, &img, &dst);
            Ok(true)
        }
        0x9B => {
            let (img, dst, rgn) = decode_direct_bits_rgn(r)?;
            blit_subimage_with_rgn(canvas, state, &img, &dst, Some(&rgn));
            Ok(true)
        }
        0xA0 => {
            let kind = r.read_u16()?;
            state.comments.push(PictComment::short(kind));
            Ok(true)
        }
        0xA1 => {
            let kind = r.read_u16()?;
            let n = r.read_u16()? as usize;
            let data = r.read_bytes(n)?.to_vec();
            state.comments.push(PictComment::long(kind, data));
            Ok(true)
        }
        _ => Err(PictError::unsupported(format!(
            "unknown / unsupported v1 opcode 0x{opcode:02X} at offset {} (frame={pic_frame:?})",
            r.pos - 1
        ))),
    }
}

// ---------------------------------------------------------------------------
// Registry-feature trait surface.
// ---------------------------------------------------------------------------

#[cfg(feature = "registry")]
use oxideav_core::Decoder;
#[cfg(feature = "registry")]
use oxideav_core::{CodecId, CodecParameters, Frame, Packet, VideoFrame, VideoPlane};

/// Factory registered with the codec registry. Consumes one packet
/// per whole PICT file and produces one frame.
#[cfg(feature = "registry")]
pub fn make_decoder(_params: &CodecParameters) -> oxideav_core::Result<Box<dyn Decoder>> {
    Ok(Box::new(PictDecoder {
        codec_id: CodecId::new(crate::CODEC_ID_STR),
        pending: None,
        eof: false,
    }))
}

#[cfg(feature = "registry")]
struct PictDecoder {
    codec_id: CodecId,
    pending: Option<VideoFrame>,
    eof: bool,
}

#[cfg(feature = "registry")]
impl Decoder for PictDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }
    fn send_packet(&mut self, packet: &Packet) -> oxideav_core::Result<()> {
        let image = parse_pict(&packet.data)?;
        self.pending = Some(image_to_video_frame(image));
        Ok(())
    }
    fn receive_frame(&mut self) -> oxideav_core::Result<Frame> {
        match self.pending.take() {
            Some(f) => Ok(Frame::Video(f)),
            None => {
                if self.eof {
                    Err(oxideav_core::Error::Eof)
                } else {
                    Err(oxideav_core::Error::NeedMore)
                }
            }
        }
    }
    fn flush(&mut self) -> oxideav_core::Result<()> {
        self.eof = true;
        Ok(())
    }
}

#[cfg(feature = "registry")]
fn image_to_video_frame(image: PictImage) -> VideoFrame {
    let stride = image.stride();
    VideoFrame {
        pts: image.pts,
        planes: vec![VideoPlane {
            stride,
            data: image.data,
        }],
    }
}
