//! Inline-image plumbing for the reader. Builds a `ratatui_image::Picker`
//! at startup, decodes `cid:` / `data:` payloads into a `SlicedProtocol`
//! sized for terminal cells, and hands them back to `ui/reader.rs` for
//! rendering. The `SlicedProtocol` is render-only (no `&mut` needed at
//! draw time), so the reader-pane draw path stays on `&App`.
//!
//! Privacy stance (DESIGN.md invariant 4): nothing in this module reaches
//! the network. `cid:` bytes come from the message's MIME parts, `data:`
//! is inline base64. Anything else (http/https/file/…) is the caller's
//! responsibility to skip.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::IsTerminal;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ratatui::layout::Size;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::sliced::SlicedProtocol;

use crate::config::{Images, ImagesProtocol};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ImageKey {
    Cid(String),
    /// `data:` URIs are keyed by a hash of the full URI so two identical
    /// inline payloads share a single decoded entry.
    Data(u64),
}

pub struct ResolvedImage {
    pub protocol: SlicedProtocol,
    pub width_cells: u16,
    pub height_cells: u16,
}

/// Construct a `Picker` honoring `[images].protocol` and the current tty
/// state. Returns `None` when images are disabled (`protocol = "off"`) or
/// stdio isn't a terminal — both cases force the reader into placeholder
/// mode. On a failed `Auto` probe falls back to halfblocks with a
/// diagnostic in `warning`.
pub fn build_picker(cfg: &Images) -> (Option<Picker>, Option<String>) {
    // Silent on success; surface only when the user might wonder why an
    // image isn't rendering. `Off` and non-tty are deliberate states and
    // self-explanatory from the config / invocation, so they stay quiet
    // too — only a failed Auto probe (silent terminal degradation) is
    // worth a status-row note.
    if matches!(cfg.protocol, ImagesProtocol::Off) {
        return (None, None);
    }
    if !std::io::stdout().is_terminal() {
        return (None, None);
    }
    match cfg.protocol {
        ImagesProtocol::Off => (None, None),
        ImagesProtocol::Auto => match Picker::from_query_stdio() {
            Ok(p) => (Some(p), None),
            Err(e) => (
                Some(Picker::halfblocks()),
                Some(format!(
                    "images: probe failed, using halfblocks fallback: {e}"
                )),
            ),
        },
        explicit => {
            let mut p = Picker::halfblocks();
            p.set_protocol_type(map_protocol(explicit));
            (Some(p), None)
        }
    }
}

fn map_protocol(p: ImagesProtocol) -> ProtocolType {
    match p {
        ImagesProtocol::Kitty => ProtocolType::Kitty,
        ImagesProtocol::Iterm => ProtocolType::Iterm2,
        ImagesProtocol::Sixel => ProtocolType::Sixel,
        ImagesProtocol::Halfblocks | ImagesProtocol::Auto | ImagesProtocol::Off => {
            ProtocolType::Halfblocks
        }
    }
}

/// Decode the raw bytes of a `cid:` part or `data:` URI into a
/// [`ResolvedImage`] sized for the current terminal. `max_h_cells` caps
/// the cell height (preserving aspect ratio) so a single huge image
/// can't dominate the reader pane.
pub fn decode(picker: &Picker, bytes: &[u8], max_h_cells: u16) -> Result<ResolvedImage> {
    let dyn_img = image::load_from_memory(bytes).context("decoding image bytes")?;
    let natural = ratatui_image::Resize::natural_size(&dyn_img, picker.font_size());
    let size = cap_height(natural, max_h_cells);
    if size.width == 0 || size.height == 0 {
        bail!("image collapsed to zero cells");
    }
    let protocol = SlicedProtocol::new(picker, dyn_img, Some(size))
        .map_err(|e| anyhow!("building sliced protocol: {e:?}"))?;
    Ok(ResolvedImage {
        protocol,
        width_cells: size.width,
        height_cells: size.height,
    })
}

fn cap_height(natural: Size, max_h: u16) -> Size {
    if max_h == 0 || natural.height <= max_h || natural.height == 0 {
        return natural;
    }
    let width = ((natural.width as u32 * max_h as u32 + natural.height as u32 / 2)
        / natural.height as u32)
        .max(1) as u16;
    Size::new(width, max_h)
}

/// Decode a `data:image/...;base64,...` URI to its raw bytes. Returns
/// `None` for any other shape (non-image MIME types, non-base64
/// encodings, malformed URIs) so the reader falls back to a placeholder.
pub fn parse_data_uri(uri: &str) -> Option<Vec<u8>> {
    let rest = uri.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    if !meta.starts_with("image/") {
        return None;
    }
    if !meta.contains(";base64") {
        return None;
    }
    STANDARD.decode(payload.trim().as_bytes()).ok()
}

pub fn data_uri_key(uri: &str) -> u64 {
    let mut h = DefaultHasher::new();
    uri.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WELCOME_PNG: &[u8] = include_bytes!("../../dev/fixtures/welcome.png");

    fn test_picker() -> Picker {
        // Halfblocks-only Picker: no stdio probe, deterministic for tests.
        Picker::halfblocks()
    }

    #[test]
    fn decode_real_png_yields_protocol() {
        let picker = test_picker();
        let resolved = decode(&picker, WELCOME_PNG, 24).expect("welcome.png decodes");
        assert!(resolved.width_cells > 0);
        assert!(resolved.height_cells > 0);
        assert!(resolved.height_cells <= 24);
    }

    #[test]
    fn decode_caps_height() {
        let picker = test_picker();
        let resolved = decode(&picker, WELCOME_PNG, 4).expect("welcome.png decodes");
        assert!(resolved.height_cells <= 4, "{}", resolved.height_cells);
        assert!(resolved.width_cells > 0);
    }

    #[test]
    fn decode_bogus_bytes_errors() {
        let picker = test_picker();
        let result = decode(&picker, b"hello-png-bytes", 24);
        match result {
            Ok(_) => panic!("expected decode failure on bogus bytes"),
            Err(e) => assert!(
                e.to_string().contains("decoding image bytes"),
                "unexpected error: {e:#}"
            ),
        }
    }

    #[test]
    fn parse_data_uri_roundtrip() {
        let encoded = STANDARD.encode(WELCOME_PNG);
        let uri = format!("data:image/png;base64,{encoded}");
        let decoded = parse_data_uri(&uri).expect("data uri decodes");
        assert_eq!(decoded, WELCOME_PNG);
    }

    #[test]
    fn parse_data_uri_rejects_unsupported() {
        // Plain text payload.
        assert!(parse_data_uri("data:text/plain;base64,aGVsbG8=").is_none());
        // No base64 marker.
        assert!(parse_data_uri("data:image/png,raw").is_none());
        // Not a data URI at all.
        assert!(parse_data_uri("https://example.com/p.png").is_none());
        // Missing comma.
        assert!(parse_data_uri("data:image/png;base64").is_none());
    }

    #[test]
    fn m0p11_fixture_decodes_against_real_png() {
        use std::path::Path;
        let path = Path::new("dev/maildir/cur/1779200000.M0P11.epost-dev:2,S");
        let body = crate::mail::parse::read_body(path).expect("read body");
        assert!(body.html.is_some(), "html body present");
        let cid_bytes = body
            .cid_parts
            .get("welcome@epost")
            .expect("welcome@epost cid present");
        assert_eq!(
            &cid_bytes[..4],
            b"\x89PNG",
            "expected PNG header, got {:?}",
            &cid_bytes[..8.min(cid_bytes.len())]
        );
        let picker = test_picker();
        let resolved = decode(&picker, cid_bytes, 24).expect("decode should succeed");
        assert!(resolved.width_cells > 0);
        assert!(resolved.height_cells > 0);
    }

    #[test]
    fn build_picker_off_returns_none() {
        let cfg = Images {
            protocol: ImagesProtocol::Off,
            max_height_cells: 24,
        };
        let (picker, warn) = build_picker(&cfg);
        assert!(picker.is_none());
        assert!(warn.is_none(), "off mode should be silent, got {warn:?}");
    }
}
