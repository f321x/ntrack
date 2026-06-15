//! QR code decoding from a grayscale (luminance) image buffer.
//!
//! The camera capture for scanning lives on the Android side (framework
//! Camera2, no third-party dependency); each frame's luminance plane is handed
//! to [`decode_luma`], which decodes it here in pure Rust via [`rqrr`]. Keeping
//! the decode off-device-testable means the whole scan pipeline (generate →
//! rasterize → decode) can be exercised by the unit tests below.

/// Decode the first QR code found in a grayscale image.
///
/// `luma` is row-major 8-bit luminance, `stride` bytes per row (`>= width`,
/// matching Android's `Image.Plane.rowStride`). Returns the decoded UTF-8
/// payload, or `None` if no QR code is found or the buffer is too small.
pub fn decode_luma(width: usize, height: usize, stride: usize, luma: &[u8]) -> Option<String> {
    if width == 0 || height == 0 || stride < width {
        return None;
    }
    // Guard the closure's indexing so a short/garbled buffer can never panic.
    if luma.len() < (height - 1).checked_mul(stride)?.checked_add(width)? {
        return None;
    }
    let mut img =
        rqrr::PreparedImage::prepare_from_greyscale(width, height, |x, y| luma[y * stride + x]);
    img.detect_grids()
        .into_iter()
        .find_map(|grid| grid.decode().ok().map(|(_meta, content)| content))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rasterize a string into a QR image: each module scaled to `scale`×`scale`
    /// pixels with a `quiet`-module light border. Dark module → 0, light → 255.
    /// Returns `(width, height, luma)`.
    fn render_qr(text: &str, scale: usize, quiet: usize) -> (usize, usize, Vec<u8>) {
        let code = qrcode::QrCode::new(text.as_bytes()).unwrap();
        let modules = code.width();
        let dim = (modules + quiet * 2) * scale;
        let mut luma = vec![255u8; dim * dim];
        let colors = code.to_colors();
        for my in 0..modules {
            for mx in 0..modules {
                if colors[my * modules + mx] == qrcode::Color::Dark {
                    let px0 = (mx + quiet) * scale;
                    let py0 = (my + quiet) * scale;
                    for py in py0..py0 + scale {
                        for px in px0..px0 + scale {
                            luma[py * dim + px] = 0;
                        }
                    }
                }
            }
        }
        (dim, dim, luma)
    }

    #[test]
    fn decodes_a_generated_qr() {
        let text = "ntrack://join?n=Family&k=nsec1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqkx2tza";
        let (w, h, luma) = render_qr(text, 6, 4);
        assert_eq!(decode_luma(w, h, w, &luma).as_deref(), Some(text));
    }

    #[test]
    fn decodes_with_padded_stride() {
        // Emulate a camera plane whose rowStride exceeds the width.
        let text = "ntrack://join?k=npub1xxxx";
        let (w, h, tight) = render_qr(text, 6, 4);
        let stride = w + 17;
        let mut padded = vec![255u8; stride * h];
        for y in 0..h {
            padded[y * stride..y * stride + w].copy_from_slice(&tight[y * w..(y + 1) * w]);
        }
        assert_eq!(decode_luma(w, h, stride, &padded).as_deref(), Some(text));
    }

    #[test]
    fn full_share_to_scan_pipeline() {
        // End-to-end in pure Rust: sharing builds an invite URI, the QR is
        // generated and decoded, and scanning parses it back to the same name
        // and key — even with a unicode group name (the URI percent-encodes it,
        // so the QR payload stays ASCII).
        let k = crate::keys::generate();
        let nsec = crate::keys::nsec(&k);
        let invite = crate::invite::build_invite("Família 🇧🇷", nsec.expose(), &[]);
        let (w, h, luma) = render_qr(&invite, 6, 4);

        let scanned = decode_luma(w, h, w, &luma).expect("decode");
        assert_eq!(scanned, invite);

        let parsed = crate::invite::parse_shared(&scanned).expect("parse");
        assert_eq!(parsed.name.as_deref(), Some("Família 🇧🇷"));
        assert_eq!(parsed.key, nsec.expose());
    }

    #[test]
    fn full_pipeline_with_relays() {
        // Mirror the production share path (the controller threads share.relays
        // into build_invite): a relay-bearing invite is the larger real-world QR
        // payload (name + nsec + up to 3 r= params). Exercise it end-to-end.
        let k = crate::keys::generate();
        let nsec = crate::keys::nsec(&k);
        let relays = vec![
            "wss://relay.damus.io".to_string(),
            "wss://nos.lol".to_string(),
            "wss://relay.snort.social".to_string(),
        ];
        let invite = crate::invite::build_invite("Família 🇧🇷", nsec.expose(), &relays);
        let (w, h, luma) = render_qr(&invite, 6, 4);

        let scanned = decode_luma(w, h, w, &luma).expect("decode");
        assert_eq!(scanned, invite);

        let parsed = crate::invite::parse_shared(&scanned).expect("parse");
        assert_eq!(parsed.name.as_deref(), Some("Família 🇧🇷"));
        assert_eq!(parsed.key, nsec.expose());
        assert_eq!(parsed.relays, relays);
    }

    #[test]
    fn blank_image_yields_none() {
        let blank = vec![255u8; 200 * 200];
        assert!(decode_luma(200, 200, 200, &blank).is_none());
    }

    #[test]
    fn undersized_buffer_yields_none_without_panicking() {
        let small = vec![0u8; 10];
        assert!(decode_luma(200, 200, 200, &small).is_none());
    }
}
