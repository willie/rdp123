//! EGFX (RDP 8 Graphics Pipeline) surface compositor.
//!
//! `ironrdp-egfx` handles the channel protocol (ZGFX bulk decompression,
//! capability negotiation, frame acks, H.264 decoding); this module keeps the
//! actual pixels. It maintains one BGRA buffer per server surface, applies
//! bitmap updates, solid fills, surface/cache copies, and flushes mapped
//! regions into the shared framebuffer the UI paints from.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ironrdp::graphics::clearcodec::ClearCodecDecoder;
use ironrdp::graphics::progressive::ProgressiveDecoder;
use ironrdp::graphics::rdp6::BitmapStreamDecoder;
use ironrdp::pdu::codecs::rfx::progressive::{
    decode_progressive_stream, ProgressiveBlock, ProgressiveTile,
};
use ironrdp::pdu::geometry::{ExclusiveRectangle, Rectangle as _};
use ironrdp_egfx::client::{BitmapUpdate, GraphicsPipelineHandler, Surface};
use ironrdp_egfx::pdu::{
    CacheToSurfacePdu, CapabilitiesV107Flags, CapabilitiesV81Flags, CapabilitiesV8Flags,
    CapabilitySet, Codec1Type, Color, EvictCacheEntryPdu, GfxPdu, MapSurfaceToScaledOutputPdu,
    SolidFillPdu, SurfaceToCachePdu, SurfaceToSurfacePdu, WireToSurface1Pdu, WireToSurface2Pdu,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::framebuffer::SharedFramebuffer;

const BPP: usize = 4;
/// RemoteFX Progressive tile edge length.
const TILE: usize = 64;

/// Notifications from the compositor to the session loop (which owns the
/// event callback to the UI).
#[derive(Debug, Clone, Copy)]
pub enum GfxEvent {
    /// Mapped output pixels changed; the UI should repaint.
    Updated,
    /// The server reset the output size.
    Resized { width: u16, height: u16 },
}

/// Variant name of an EGFX PDU for rate-limited "ignored" logging.
fn pdu_name(pdu: &GfxPdu) -> &'static str {
    match pdu {
        GfxPdu::WireToSurface1(_) => "WireToSurface1",
        GfxPdu::MapSurfaceToWindow(_) => "MapSurfaceToWindow",
        GfxPdu::MapSurfaceToScaledWindow(_) => "MapSurfaceToScaledWindow",
        GfxPdu::CacheImportReply(_) => "CacheImportReply",
        _ => "unrecognized",
    }
}

/// One server-side surface: a BGRA pixel buffer plus its output mapping.
struct SurfaceBuf {
    width: u16,
    height: u16,
    /// Top-left position on the output, when mapped.
    origin: Option<(u32, u32)>,
    /// Tightly packed BGRA, `width * height * 4` bytes.
    pixels: Vec<u8>,
}

impl SurfaceBuf {
    fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            origin: None,
            pixels: vec![0; usize::from(width) * usize::from(height) * BPP],
        }
    }

    /// Clamp an exclusive rectangle to this surface.
    fn clamp(&self, rect: &ExclusiveRectangle) -> (usize, usize, usize, usize) {
        let left = usize::from(rect.left.min(self.width));
        let top = usize::from(rect.top.min(self.height));
        let right = usize::from(rect.right.min(self.width)).max(left);
        let bottom = usize::from(rect.bottom.min(self.height)).max(top);
        (left, top, right, bottom)
    }

    /// Copy a rectangle out into a tight buffer.
    fn read_rect(&self, left: usize, top: usize, right: usize, bottom: usize) -> Vec<u8> {
        let w = right - left;
        let mut out = Vec::with_capacity(w * (bottom - top) * BPP);
        let stride = usize::from(self.width) * BPP;
        for y in top..bottom {
            let start = y * stride + left * BPP;
            out.extend_from_slice(&self.pixels[start..start + w * BPP]);
        }
        out
    }

    /// Write a tight buffer of `w x h` pixels at (left, top), clipped.
    fn write_rect(&mut self, src: &[u8], left: usize, top: usize, w: usize, h: usize) {
        let sw = usize::from(self.width);
        let sh = usize::from(self.height);
        if left >= sw || top >= sh {
            return;
        }
        let copy_w = w.min(sw - left);
        let copy_h = h.min(sh - top);
        let stride = sw * BPP;
        for row in 0..copy_h {
            let src_start = row * w * BPP;
            let dst_start = (top + row) * stride + left * BPP;
            self.pixels[dst_start..dst_start + copy_w * BPP]
                .copy_from_slice(&src[src_start..src_start + copy_w * BPP]);
        }
    }
}

/// The EGFX handler: composites surfaces into the shared framebuffer.
pub struct GfxHandler {
    framebuffer: Arc<SharedFramebuffer>,
    events: UnboundedSender<GfxEvent>,
    surfaces: HashMap<u16, SurfaceBuf>,
    /// Last surfaces retained across RESET_GRAPHICS so a same-sized recreated
    /// surface does not replace the visible desktop with black while the
    /// server sends only incremental updates.
    retired_surfaces: HashMap<u16, SurfaceBuf>,
    cache: HashMap<u16, (u16, u16, Vec<u8>)>,
    progressive: ProgressiveDecoder,
    /// ClearCodec regions (UI, text, icons). The decoder is stateful: glyph
    /// and V-bar caches persist across frames. Windows encodes most non-video
    /// screen regions with it, so dropping these updates leaves stale tiles.
    clearcodec: ClearCodecDecoder,
    /// RDP 6.0 planar codec regions (top-down in EGFX, per the reference
    /// implementation's vFlip=FALSE).
    planar: BitmapStreamDecoder,
    clearcodec_regions: u64,
    clearcodec_failures: u64,
    planar_regions: u64,
    /// Codec names already reported as unhandled (warn once per codec).
    warned_unhandled: HashSet<String>,
    /// Codecs already reported as decoded by `ironrdp-egfx` (log once each).
    decoded_codecs: Vec<Codec1Type>,
    warned_progressive: bool,
    /// RemoteFX Progressive frames decoded OK vs. failed. Logged (first
    /// success, first failure, then periodic totals) so a live capture shows
    /// whether decoding works at all and how often it fails.
    progressive_frames: u64,
    progressive_failures: u64,
    progressive_detail_logs: u8,
    /// `RDP123_DEBUG_DUMP=<dir>`: periodically write the framebuffer as a PPM
    /// image so rendering artifacts can be inspected offline.
    dump_dir: Option<std::path::PathBuf>,
    flush_count: std::cell::Cell<u64>,
    /// `RDP123_EGFX_CAPTURE=<file>`: append every WireToSurface2 progressive
    /// stream (length-prefixed) for offline replay and analysis.
    capture: Option<std::fs::File>,
    /// Offer the V10.7 capability set (AVC444).
    avc444: bool,
}

impl GfxHandler {
    pub fn new(
        framebuffer: Arc<SharedFramebuffer>,
        events: UnboundedSender<GfxEvent>,
        avc444: bool,
    ) -> Self {
        let dump_dir = std::env::var_os("RDP123_DEBUG_DUMP").map(std::path::PathBuf::from);
        if let Some(dir) = &dump_dir {
            match std::fs::create_dir_all(dir) {
                Ok(()) => {
                    tracing::info!("egfx: dumping framebuffer snapshots to {}", dir.display())
                }
                Err(e) => tracing::warn!("egfx: cannot create dump dir {}: {e}", dir.display()),
            }
        }
        let capture = std::env::var_os("RDP123_EGFX_CAPTURE").and_then(|path| {
            match std::fs::File::create(&path) {
                Ok(file) => {
                    tracing::info!(
                        "egfx: capturing progressive streams to {}",
                        std::path::Path::new(&path).display()
                    );
                    Some(file)
                }
                Err(e) => {
                    tracing::warn!(
                        "egfx: cannot create capture file {}: {e}",
                        std::path::Path::new(&path).display()
                    );
                    None
                }
            }
        });
        Self {
            framebuffer,
            events,
            surfaces: HashMap::new(),
            retired_surfaces: HashMap::new(),
            cache: HashMap::new(),
            progressive: ProgressiveDecoder::new(),
            clearcodec: ClearCodecDecoder::new(),
            planar: BitmapStreamDecoder::default(),
            clearcodec_regions: 0,
            clearcodec_failures: 0,
            planar_regions: 0,
            warned_unhandled: HashSet::new(),
            decoded_codecs: Vec::new(),
            warned_progressive: false,
            progressive_frames: 0,
            progressive_failures: 0,
            progressive_detail_logs: 0,
            dump_dir,
            flush_count: std::cell::Cell::new(0),
            capture,
            avc444,
        }
    }

    /// Write the current framebuffer as a PPM snapshot (rotating 8 slots) so
    /// visual artifacts can be inspected offline. Runs only with
    /// `RDP123_DEBUG_DUMP` set; roughly one snapshot every 64 flushes (~2 s).
    fn maybe_dump_framebuffer(&self) {
        let Some(dir) = &self.dump_dir else {
            return;
        };
        let count = self.flush_count.get();
        self.flush_count.set(count + 1);
        if !count.is_multiple_of(64) {
            return;
        }
        let slot = (count / 64) % 8;
        let path = dir.join(format!("fb-{slot}.ppm"));
        let written = self.framebuffer.with_pixels(|pixels, width, height| {
            let mut out = Vec::with_capacity(pixels.len() / 4 * 3 + 32);
            out.extend_from_slice(format!("P6\n{width} {height}\n255\n").as_bytes());
            for px in pixels.as_chunks::<4>().0 {
                out.extend_from_slice(&[px[2], px[1], px[0]]);
            }
            std::fs::write(&path, out)
        });
        if let Some(Err(e)) = written {
            tracing::warn!("egfx: framebuffer dump failed: {e}");
        }
    }

    fn create_surface_buffer(
        &mut self,
        surface_id: u16,
        width: u16,
        height: u16,
        origin: Option<(u32, u32)>,
    ) {
        let max = crate::profile::MAX_REMOTE_DIMENSION;
        if width > max || height > max {
            tracing::warn!("egfx: refusing {width}x{height} surface {surface_id}");
            return;
        }
        let mut buf = match self.retired_surfaces.remove(&surface_id) {
            Some(old) if old.width == width && old.height == height => {
                tracing::debug!(
                    "egfx: retained pixels for recreated surface {surface_id} ({width}x{height})"
                );
                old
            }
            _ => SurfaceBuf::new(width, height),
        };
        buf.origin = origin;
        self.surfaces.insert(surface_id, buf);
    }

    /// Write decoded RGBA pixels into a surface at `rect` and flush.
    fn apply_bitmap(
        &mut self,
        surface_id: u16,
        rect: &ExclusiveRectangle,
        rgba: &[u8],
        data_width: u16,
        data_height: u16,
    ) {
        let Some(surface) = self.surfaces.get_mut(&surface_id) else {
            return;
        };
        let w = usize::from(data_width.min(rect.width()));
        let h = usize::from(data_height.min(rect.height()));
        if w == 0 || h == 0 {
            return;
        }

        // RGBA (decoder output) -> BGRA (framebuffer layout), row by row so a
        // decoder buffer wider than the destination is cropped correctly.
        let src_stride = usize::from(data_width) * BPP;
        if rgba.len() < (h - 1) * src_stride + w * BPP {
            tracing::warn!(
                "egfx: dropping {data_width}x{data_height} bitmap with only {} bytes",
                rgba.len()
            );
            return;
        }
        let mut bgra = vec![0u8; w * h * BPP];
        for row in 0..h {
            let src_row = &rgba[row * src_stride..row * src_stride + w * BPP];
            let dst_row = &mut bgra[row * w * BPP..(row + 1) * w * BPP];
            for (dst, src) in dst_row
                .as_chunks_mut::<BPP>()
                .0
                .iter_mut()
                .zip(src_row.as_chunks::<BPP>().0.iter())
            {
                dst[0] = src[2];
                dst[1] = src[1];
                dst[2] = src[0];
                dst[3] = 0xFF;
            }
        }

        let left = usize::from(rect.left);
        let top = usize::from(rect.top);
        surface.write_rect(&bgra, left, top, w, h);
        self.flush(surface_id, left, top, left + w, top + h);
    }

    /// Push a surface region to the framebuffer if the surface is mapped.
    fn flush(&self, surface_id: u16, left: usize, top: usize, right: usize, bottom: usize) {
        let Some(surface) = self.surfaces.get(&surface_id) else {
            return;
        };
        let Some((ox, oy)) = surface.origin else {
            return;
        };
        let right = right.min(usize::from(surface.width));
        let bottom = bottom.min(usize::from(surface.height));
        if left >= right || top >= bottom {
            return;
        }
        let tight = surface.read_rect(left, top, right, bottom);
        self.framebuffer.write_rect(
            &tight,
            ox + left as u32,
            oy + top as u32,
            (right - left) as u32,
            (bottom - top) as u32,
        );
        self.maybe_dump_framebuffer();
        let _ = self.events.send(GfxEvent::Updated);
    }

    /// Append one kind-prefixed record to the capture file (see
    /// `examples/egfx_replay.rs` for the record catalogue). The capture
    /// covers every compositing operation so a session can be replayed
    /// through the real `GfxHandler` offline.
    fn capture_record(&mut self, kind: u8, body: &[u8]) {
        let Some(file) = &mut self.capture else {
            return;
        };
        use std::io::Write as _;
        let write = file.write_all(&[kind]).and_then(|()| file.write_all(body));
        if let Err(e) = write {
            tracing::warn!("egfx: stream capture failed: {e}");
            self.capture = None;
        }
    }

    /// Capture a WireToSurface1 record (0x01): surface u16, codec u8,
    /// left/top/right/bottom u16, len u32, payload.
    fn capture_wire1(&mut self, pdu: &WireToSurface1Pdu) {
        if self.capture.is_none() {
            return;
        }
        let r = &pdu.destination_rectangle;
        let mut body = Vec::with_capacity(15 + pdu.bitmap_data.len());
        body.extend_from_slice(&pdu.surface_id.to_le_bytes());
        body.push(pdu.codec_id as u8);
        for v in [r.left, r.top, r.right, r.bottom] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        body.extend_from_slice(&(pdu.bitmap_data.len() as u32).to_le_bytes());
        body.extend_from_slice(&pdu.bitmap_data);
        self.capture_record(0x01, &body);
    }

    /// Persist a ClearCodec stream that failed to decode (first 8, requires
    /// `RDP123_DEBUG_DUMP`): 2 bytes width + 2 bytes height + payload.
    fn dump_failed_clearcodec(&self, width: u16, height: u16, data: &[u8]) {
        let Some(dir) = &self.dump_dir else {
            return;
        };
        if self.clearcodec_failures > 8 {
            return;
        }
        let path = dir.join(format!("clearcodec-fail-{}.bin", self.clearcodec_failures));
        let mut out = Vec::with_capacity(data.len() + 4);
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(data);
        if let Err(e) = std::fs::write(&path, out) {
            tracing::warn!("egfx: failed-stream dump failed: {e}");
        }
    }

    /// Read the current content of `rect` from a surface as tight BGRA, or
    /// `None` when the rectangle is not fully inside the surface.
    fn read_surface_rect(&self, surface_id: u16, rect: &ExclusiveRectangle) -> Option<Vec<u8>> {
        let surface = self.surfaces.get(&surface_id)?;
        let (left, top, right, bottom) = surface.clamp(rect);
        if left != usize::from(rect.left)
            || top != usize::from(rect.top)
            || right != usize::from(rect.right)
            || bottom != usize::from(rect.bottom)
            || left >= right
            || top >= bottom
        {
            return None;
        }
        Some(surface.read_rect(left, top, right, bottom))
    }

    /// Write already-BGRA pixels (tightly packed at the rectangle's width)
    /// into a surface at `rect` and flush the region.
    fn apply_bgra(&mut self, surface_id: u16, rect: &ExclusiveRectangle, bgra: &[u8]) {
        let Some(surface) = self.surfaces.get_mut(&surface_id) else {
            return;
        };
        let data_w = usize::from(rect.width());
        let (left, top, right, bottom) = surface.clamp(rect);
        let (w, h) = (right - left, bottom - top);
        if w == 0 || h == 0 || bgra.len() < data_w * h * BPP {
            return;
        }
        if data_w == w {
            surface.write_rect(&bgra[..w * h * BPP], left, top, w, h);
        } else {
            // Clip each row to the part of the rectangle inside the surface.
            let mut tight = vec![0u8; w * h * BPP];
            for row in 0..h {
                tight[row * w * BPP..(row + 1) * w * BPP]
                    .copy_from_slice(&bgra[row * data_w * BPP..row * data_w * BPP + w * BPP]);
            }
            surface.write_rect(&tight, left, top, w, h);
        }
        self.flush(surface_id, left, top, right, bottom);
    }

    /// Decode and apply a `WireToSurface1` region the EGFX client itself does
    /// not decode. Windows mixes codecs per region: RemoteFX Progressive and
    /// H.264 arrive via the handled paths, but UI/text regions typically come
    /// as ClearCodec (and occasionally the RDP 6.0 planar codec). Ignoring
    /// them leaves permanently stale rectangles on screen.
    fn apply_wire_to_surface1(&mut self, pdu: &WireToSurface1Pdu) {
        self.capture_wire1(pdu);
        let rect = &pdu.destination_rectangle;
        let (w, h) = (rect.width(), rect.height());
        if w == 0 || h == 0 {
            return;
        }
        match pdu.codec_id {
            Codec1Type::ClearCodec => {
                // ClearCodec layers composite onto the existing surface
                // content: uncovered pixels must stay unchanged, so the
                // current rectangle content is the compositing base.
                let base = self.read_surface_rect(pdu.surface_id, rect);
                match self
                    .clearcodec
                    .decode_with_base(&pdu.bitmap_data, w, h, base.as_deref())
                {
                    Ok(bgra) => {
                        if self.clearcodec_regions == 0 {
                            tracing::info!("egfx: ClearCodec regions active");
                        }
                        self.clearcodec_regions += 1;
                        self.apply_bgra(pdu.surface_id, rect, &bgra);
                    }
                    Err(e) => {
                        self.clearcodec_failures += 1;
                        if self.clearcodec_failures <= 4 {
                            tracing::warn!(
                                "egfx: ClearCodec decode failed ({e}); {}x{h_}px region stays stale \
                                 (failure {} so far, {} ok)",
                                w,
                                self.clearcodec_failures,
                                self.clearcodec_regions,
                                h_ = h,
                            );
                        }
                        self.dump_failed_clearcodec(w, h, &pdu.bitmap_data);
                    }
                }
            }
            Codec1Type::Planar => {
                let mut rgb = Vec::with_capacity(usize::from(w) * usize::from(h) * 3);
                match self.planar.decode_bitmap_stream_to_rgb24(
                    &pdu.bitmap_data,
                    &mut rgb,
                    usize::from(w),
                    usize::from(h),
                ) {
                    Ok(()) => {
                        if self.planar_regions == 0 {
                            tracing::info!("egfx: planar regions active");
                        }
                        self.planar_regions += 1;
                        let mut bgra = vec![0u8; usize::from(w) * usize::from(h) * BPP];
                        for (dst, src) in bgra
                            .as_chunks_mut::<BPP>()
                            .0
                            .iter_mut()
                            .zip(rgb.as_chunks::<3>().0.iter())
                        {
                            dst[0] = src[2];
                            dst[1] = src[1];
                            dst[2] = src[0];
                            dst[3] = 0xFF;
                        }
                        self.apply_bgra(pdu.surface_id, rect, &bgra);
                    }
                    Err(e) => {
                        if self.warned_unhandled.insert("Planar-error".to_owned()) {
                            tracing::warn!(
                                "egfx: planar decode failed ({e}); regions may stay stale"
                            );
                        }
                    }
                }
            }
            other => {
                let key = format!("{other:?}");
                if self.warned_unhandled.insert(key) {
                    tracing::warn!(
                        "egfx: no decoder for codec {other:?}; its regions will not update"
                    );
                }
            }
        }
    }

    /// Replay hook: create a surface as if `on_surface_created` ran
    /// (`ironrdp_egfx::client::Surface` is non-exhaustive, so the offline
    /// replay tool cannot construct one itself).
    pub fn replay_surface_created(
        &mut self,
        surface_id: u16,
        width: u16,
        height: u16,
        origin: Option<(u32, u32)>,
    ) {
        self.create_surface_buffer(surface_id, width, height, origin);
    }

    /// Replay hook: apply an already-decoded RGBA bitmap region as if
    /// `on_bitmap_updated` ran (`BitmapUpdate` is non-exhaustive).
    pub fn replay_bitmap_updated(
        &mut self,
        surface_id: u16,
        rect: &ExclusiveRectangle,
        rgba: &[u8],
        data_width: u16,
        data_height: u16,
    ) {
        self.apply_bitmap(surface_id, rect, rgba, data_width, data_height);
    }

    /// Map a surface to an output origin and paint it whole. Shared by the
    /// plain (`MapSurfaceToOutput`) and scaled (`MapSurfaceToScaledOutput`)
    /// paths — without a stored origin, `flush` is a no-op and the surface
    /// never reaches the framebuffer (the black-screen bug on HiDPI sessions,
    /// which map via the scaled PDU).
    fn map_surface(&mut self, surface_id: u16, origin_x: u32, origin_y: u32) {
        if let Some(surface) = self.surfaces.get_mut(&surface_id) {
            surface.origin = Some((origin_x, origin_y));
            let (w, h) = (usize::from(surface.width), usize::from(surface.height));
            self.flush(surface_id, 0, 0, w, h);
        }
    }
}

impl GraphicsPipelineHandler for GfxHandler {
    /// V10.7 lets the server use AVC444, which the vendored `ironrdp-egfx`
    /// decodes in its v2 layout (the one GNOME Remote Desktop sends); the
    /// client drops the set if the H.264 decoder can't return YUV420 planes.
    /// V8.1 keeps RemoteFX Progressive, ClearCodec, planar and AVC420; V8 is
    /// the no-AVC fallback. With AVC444 turned off, V10.7 is not offered.
    fn capabilities(&self) -> Vec<CapabilitySet> {
        let mut caps = Vec::with_capacity(3);
        if self.avc444 {
            caps.push(CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::SMALL_CACHE,
            });
        }
        caps.push(CapabilitySet::V8_1 {
            flags: CapabilitiesV81Flags::AVC420_ENABLED | CapabilitiesV81Flags::SMALL_CACHE,
        });
        caps.push(CapabilitySet::V8 {
            flags: CapabilitiesV8Flags::SMALL_CACHE,
        });
        caps
    }

    fn on_capabilities_confirmed(&mut self, caps: &CapabilitySet) {
        tracing::info!("egfx: server confirmed capability set {caps:?}");
    }

    /// EGFX PDUs the client does not process itself; WireToSurface1 regions
    /// in codecs beyond H.264 land here and are decoded by this compositor.
    fn on_unhandled_pdu(&mut self, pdu: &GfxPdu) {
        match pdu {
            GfxPdu::WireToSurface1(w) => self.apply_wire_to_surface1(w),
            other => {
                let key = pdu_name(other).to_owned();
                if self.warned_unhandled.insert(key) {
                    tracing::debug!("egfx: ignoring PDU {}", pdu_name(other));
                }
            }
        }
    }

    fn on_reset_graphics(&mut self, width: u32, height: u32) {
        tracing::info!("egfx: reset graphics {width}x{height}");
        if self.capture.is_some() {
            // Record 0x07: w u32, h u32.
            let mut body = Vec::with_capacity(8);
            body.extend_from_slice(&width.to_le_bytes());
            body.extend_from_slice(&height.to_le_bytes());
            self.capture_record(0x07, &body);
        }
        self.retired_surfaces
            .extend(std::mem::take(&mut self.surfaces));
        // The bitmap cache is NOT cleared: it is tied to the channel
        // lifetime, not to RESET_GRAPHICS. Windows sends a reset ~1 s after
        // connect (the resize handshake) and keeps referencing slots filled
        // before it — clearing here silently dropped 527 of 2503 cache
        // copies in a captured session, leaving large never-painted regions.
        // RDPGFX_RESET_GRAPHICS resets output/surface state, not encoding
        // contexts. Windows may continue an existing Progressive context
        // without sending another RFX_PROGRESSIVE_CONTEXT block.
        self.warned_progressive = false;
        self.progressive_frames = 0;
        self.progressive_failures = 0;
        self.progressive_detail_logs = 0;
        let width = u16::try_from(width).unwrap_or(u16::MAX);
        let height = u16::try_from(height).unwrap_or(u16::MAX);
        self.framebuffer.resize(width, height);
        let _ = self.events.send(GfxEvent::Resized { width, height });
    }

    fn on_surface_created(&mut self, surface: &Surface) {
        tracing::debug!(
            "egfx: surface {} created {}x{} mapped={}",
            surface.id,
            surface.width,
            surface.height,
            surface.is_mapped
        );
        let origin = surface
            .is_mapped
            .then_some((surface.output_origin_x, surface.output_origin_y));
        if self.capture.is_some() {
            // Record 0x08: id u16, w u16, h u16, mapped u8, ox u32, oy u32.
            let (ox, oy) = origin.unwrap_or((0, 0));
            let mut body = Vec::with_capacity(15);
            body.extend_from_slice(&surface.id.to_le_bytes());
            body.extend_from_slice(&surface.width.to_le_bytes());
            body.extend_from_slice(&surface.height.to_le_bytes());
            body.push(u8::from(origin.is_some()));
            body.extend_from_slice(&ox.to_le_bytes());
            body.extend_from_slice(&oy.to_le_bytes());
            self.capture_record(0x08, &body);
        }
        self.create_surface_buffer(surface.id, surface.width, surface.height, origin);
    }

    fn on_surface_deleted(&mut self, surface_id: u16) {
        if self.capture.is_some() {
            // Record 0x09: id u16.
            self.capture_record(0x09, &surface_id.to_le_bytes());
        }
        if let Some(surface) = self.surfaces.remove(&surface_id) {
            self.retired_surfaces.insert(surface_id, surface);
        }
    }

    fn on_surface_mapped(&mut self, surface_id: u16, origin_x: u32, origin_y: u32) {
        tracing::debug!("egfx: surface {surface_id} mapped to output ({origin_x},{origin_y})");
        if self.capture.is_some() {
            // Record 0x0A: id u16, ox u32, oy u32.
            let mut body = Vec::with_capacity(10);
            body.extend_from_slice(&surface_id.to_le_bytes());
            body.extend_from_slice(&origin_x.to_le_bytes());
            body.extend_from_slice(&origin_y.to_le_bytes());
            self.capture_record(0x0A, &body);
        }
        self.map_surface(surface_id, origin_x, origin_y);
    }

    /// HiDPI / DPI-scaled sessions map surfaces with this PDU instead of
    /// `MapSurfaceToOutput`. Windows uses it whenever the client requests a
    /// scaled desktop, so without handling it the surface is never given an
    /// output origin and nothing paints (black screen). We composite at 1:1;
    /// for a single fit-to-window monitor the target size equals the surface
    /// size, so no resampling is needed.
    fn on_map_surface_to_scaled_output(&mut self, pdu: &MapSurfaceToScaledOutputPdu) {
        let surface_size = self
            .surfaces
            .get(&pdu.surface_id)
            .map(|s| (u32::from(s.width), u32::from(s.height)));
        let scaled =
            surface_size.is_some_and(|(sw, sh)| sw != pdu.target_width || sh != pdu.target_height);
        tracing::debug!(
            "egfx: surface {} mapped to scaled output ({},{}) target {}x{}{}",
            pdu.surface_id,
            pdu.output_origin_x,
            pdu.output_origin_y,
            pdu.target_width,
            pdu.target_height,
            if scaled { " (compositing 1:1)" } else { "" }
        );
        if self.capture.is_some() {
            let mut body = Vec::with_capacity(10);
            body.extend_from_slice(&pdu.surface_id.to_le_bytes());
            body.extend_from_slice(&pdu.output_origin_x.to_le_bytes());
            body.extend_from_slice(&pdu.output_origin_y.to_le_bytes());
            self.capture_record(0x0A, &body);
        }
        self.map_surface(pdu.surface_id, pdu.output_origin_x, pdu.output_origin_y);
    }

    fn on_bitmap_updated(&mut self, update: &BitmapUpdate) {
        // Empty data means the codec was skipped (e.g. no decoder).
        if update.data.is_empty() {
            return;
        }
        if !self.decoded_codecs.contains(&update.codec_id) {
            self.decoded_codecs.push(update.codec_id);
            tracing::info!("egfx: {:?} regions active", update.codec_id);
        }
        if self.capture.is_some() {
            // Record 0x0B: surface u16, rect u16 x4, data_w u16, data_h u16,
            // len u32, RGBA data (already decoded, e.g. H.264 output).
            let r = &update.destination_rectangle;
            let mut body = Vec::with_capacity(18 + update.data.len());
            body.extend_from_slice(&update.surface_id.to_le_bytes());
            for v in [r.left, r.top, r.right, r.bottom] {
                body.extend_from_slice(&v.to_le_bytes());
            }
            body.extend_from_slice(&update.width.to_le_bytes());
            body.extend_from_slice(&update.height.to_le_bytes());
            body.extend_from_slice(&(update.data.len() as u32).to_le_bytes());
            body.extend_from_slice(&update.data);
            self.capture_record(0x0B, &body);
        }
        self.apply_bitmap(
            update.surface_id,
            &update.destination_rectangle,
            &update.data,
            update.width,
            update.height,
        );
    }

    fn on_solid_fill(&mut self, pdu: &SolidFillPdu) {
        if self.capture.is_some() {
            // Record 0x03: surface u16, b/g/r u8, count u16, rects u16 x4 each.
            let mut body = Vec::with_capacity(7 + pdu.rectangles.len() * 8);
            body.extend_from_slice(&pdu.surface_id.to_le_bytes());
            body.extend_from_slice(&[pdu.fill_pixel.b, pdu.fill_pixel.g, pdu.fill_pixel.r]);
            body.extend_from_slice(&(pdu.rectangles.len() as u16).to_le_bytes());
            for rect in &pdu.rectangles {
                for v in [rect.left, rect.top, rect.right, rect.bottom] {
                    body.extend_from_slice(&v.to_le_bytes());
                }
            }
            self.capture_record(0x03, &body);
        }
        let Color { b, g, r, .. } = pdu.fill_pixel;
        let Some(surface) = self.surfaces.get_mut(&pdu.surface_id) else {
            return;
        };
        let stride = usize::from(surface.width) * BPP;
        let mut bounds: Option<(usize, usize, usize, usize)> = None;
        for rect in &pdu.rectangles {
            let (left, top, right, bottom) = surface.clamp(rect);
            for y in top..bottom {
                let row = &mut surface.pixels[y * stride + left * BPP..y * stride + right * BPP];
                for px in row.as_chunks_mut::<BPP>().0 {
                    px[0] = b;
                    px[1] = g;
                    px[2] = r;
                    px[3] = 0xFF;
                }
            }
            bounds = Some(match bounds {
                None => (left, top, right, bottom),
                Some((l, t, r2, b2)) => (l.min(left), t.min(top), r2.max(right), b2.max(bottom)),
            });
        }
        if let Some((l, t, r2, b2)) = bounds {
            self.flush(pdu.surface_id, l, t, r2, b2);
        }
    }

    fn on_surface_to_surface(&mut self, pdu: &SurfaceToSurfacePdu) {
        if self.capture.is_some() {
            // Record 0x04: src u16, dst u16, rect u16 x4, count u16, points u16 x2 each.
            let r = &pdu.source_rectangle;
            let mut body = Vec::with_capacity(14 + pdu.destination_points.len() * 4);
            body.extend_from_slice(&pdu.source_surface_id.to_le_bytes());
            body.extend_from_slice(&pdu.destination_surface_id.to_le_bytes());
            for v in [r.left, r.top, r.right, r.bottom] {
                body.extend_from_slice(&v.to_le_bytes());
            }
            body.extend_from_slice(&(pdu.destination_points.len() as u16).to_le_bytes());
            for p in &pdu.destination_points {
                body.extend_from_slice(&p.x.to_le_bytes());
                body.extend_from_slice(&p.y.to_le_bytes());
            }
            self.capture_record(0x04, &body);
        }
        let Some(src) = self.surfaces.get(&pdu.source_surface_id) else {
            return;
        };
        let (left, top, right, bottom) = src.clamp(&pdu.source_rectangle);
        if left >= right || top >= bottom {
            return;
        }
        // Buffer the source region so overlapping same-surface copies are safe.
        let tight = src.read_rect(left, top, right, bottom);
        let (w, h) = (right - left, bottom - top);

        let points: Vec<(usize, usize)> = pdu
            .destination_points
            .iter()
            .map(|p| (usize::from(p.x), usize::from(p.y)))
            .collect();
        {
            let Some(dst) = self.surfaces.get_mut(&pdu.destination_surface_id) else {
                return;
            };
            for &(dx, dy) in &points {
                dst.write_rect(&tight, dx, dy, w, h);
            }
        }
        for &(dx, dy) in &points {
            self.flush(pdu.destination_surface_id, dx, dy, dx + w, dy + h);
        }
    }

    fn on_surface_to_cache(&mut self, pdu: &SurfaceToCachePdu) {
        if self.capture.is_some() {
            // Record 0x05: surface u16, slot u16, rect u16 x4.
            let r = &pdu.source_rectangle;
            let mut body = Vec::with_capacity(12);
            body.extend_from_slice(&pdu.surface_id.to_le_bytes());
            body.extend_from_slice(&pdu.cache_slot.to_le_bytes());
            for v in [r.left, r.top, r.right, r.bottom] {
                body.extend_from_slice(&v.to_le_bytes());
            }
            self.capture_record(0x05, &body);
        }
        let Some(surface) = self.surfaces.get(&pdu.surface_id) else {
            return;
        };
        let (left, top, right, bottom) = surface.clamp(&pdu.source_rectangle);
        if left >= right || top >= bottom {
            return;
        }
        let tight = surface.read_rect(left, top, right, bottom);
        self.cache.insert(
            pdu.cache_slot,
            ((right - left) as u16, (bottom - top) as u16, tight),
        );
    }

    fn on_cache_to_surface(&mut self, pdu: &CacheToSurfacePdu) {
        if self.capture.is_some() {
            // Record 0x06: slot u16, surface u16, count u16, points u16 x2 each.
            let mut body = Vec::with_capacity(6 + pdu.destination_points.len() * 4);
            body.extend_from_slice(&pdu.cache_slot.to_le_bytes());
            body.extend_from_slice(&pdu.surface_id.to_le_bytes());
            body.extend_from_slice(&(pdu.destination_points.len() as u16).to_le_bytes());
            for p in &pdu.destination_points {
                body.extend_from_slice(&p.x.to_le_bytes());
                body.extend_from_slice(&p.y.to_le_bytes());
            }
            self.capture_record(0x06, &body);
        }
        let Some((w, h, tight)) = self.cache.get(&pdu.cache_slot).cloned() else {
            return;
        };
        let (w, h) = (usize::from(w), usize::from(h));
        if !self.surfaces.contains_key(&pdu.surface_id) {
            return;
        }
        for point in &pdu.destination_points {
            let (dx, dy) = (usize::from(point.x), usize::from(point.y));
            if let Some(dst) = self.surfaces.get_mut(&pdu.surface_id) {
                dst.write_rect(&tight, dx, dy, w, h);
            }
            self.flush(pdu.surface_id, dx, dy, dx + w, dy + h);
        }
    }

    fn on_evict_cache_entry(&mut self, pdu: &EvictCacheEntryPdu) {
        if self.capture.is_some() {
            // Record 0x0C: cache slot u16.
            self.capture_record(0x0C, &pdu.cache_slot.to_le_bytes());
        }
        self.cache.remove(&pdu.cache_slot);
    }

    fn on_wire_to_surface2(&mut self, pdu: &WireToSurface2Pdu) {
        let Some((sw, sh)) = self
            .surfaces
            .get(&pdu.surface_id)
            .map(|s| (s.width, s.height))
        else {
            return;
        };

        // The details require a full parse of the stream, so only pay for it
        // when debug logging is actually enabled.
        if self.progressive_detail_logs < 12 && tracing::enabled!(tracing::Level::DEBUG) {
            log_progressive_stream_details(pdu.codec_context_id, pdu.surface_id, &pdu.bitmap_data);
            self.progressive_detail_logs += 1;
        }

        if self.capture.is_some() {
            // Record 0x02: surface u16, context u32, sw/sh u16, len u32, data.
            let mut body = Vec::with_capacity(14 + pdu.bitmap_data.len());
            body.extend_from_slice(&pdu.surface_id.to_le_bytes());
            body.extend_from_slice(&pdu.codec_context_id.to_le_bytes());
            body.extend_from_slice(&sw.to_le_bytes());
            body.extend_from_slice(&sh.to_le_bytes());
            body.extend_from_slice(&(pdu.bitmap_data.len() as u32).to_le_bytes());
            body.extend_from_slice(&pdu.bitmap_data);
            self.capture_record(0x02, &body);
        }

        let surface_key = u32::from(pdu.surface_id);
        let tiles = match self
            .progressive
            .decode_bitmap(surface_key, sw, sh, &pdu.bitmap_data)
        {
            Ok(tiles) => tiles,
            Err(e) => {
                self.progressive_failures += 1;
                if !self.warned_progressive {
                    self.warned_progressive = true;
                    tracing::warn!(
                        "egfx: RFX Progressive decode failed ({e}); some regions may not update"
                    );
                }
                if self.progressive_failures.is_multiple_of(120) {
                    tracing::warn!(
                        "egfx: progressive frames {} ok / {} failed",
                        self.progressive_frames,
                        self.progressive_failures
                    );
                }
                return;
            }
        };

        if self.progressive_frames == 0 {
            tracing::info!(
                "egfx: RemoteFX Progressive decoding ({} tiles in first frame)",
                tiles.len()
            );
        }
        self.progressive_frames += 1;
        if self.progressive_frames.is_multiple_of(300) {
            tracing::debug!(
                "egfx: progressive frames {} ok / {} failed",
                self.progressive_frames,
                self.progressive_failures
            );
        }

        let mut bounds: Option<(usize, usize, usize, usize)> = None;
        {
            let Some(surface) = self.surfaces.get_mut(&pdu.surface_id) else {
                return;
            };
            for tile in &tiles {
                let ox = usize::from(tile.x_idx) * TILE;
                let oy = usize::from(tile.y_idx) * TILE;
                for rect in &tile.updated_rects {
                    let left = usize::from(rect.left).max(ox);
                    let top = usize::from(rect.top).max(oy);
                    let right = usize::from(rect.right).min(ox + TILE).min(usize::from(sw));
                    let bottom = usize::from(rect.bottom).min(oy + TILE).min(usize::from(sh));
                    if left >= right || top >= bottom {
                        continue;
                    }

                    let src_x = left - ox;
                    let src_y = top - oy;
                    let w = right - left;
                    let h = bottom - top;
                    let mut bgra = vec![0u8; w * h * BPP];
                    for row in 0..h {
                        let src_start = ((src_y + row) * TILE + src_x) * BPP;
                        let src = &tile.pixels[src_start..src_start + w * BPP];
                        let dst = &mut bgra[row * w * BPP..(row + 1) * w * BPP];
                        for (d, s) in dst
                            .as_chunks_mut::<BPP>()
                            .0
                            .iter_mut()
                            .zip(src.as_chunks::<BPP>().0.iter())
                        {
                            d[0] = s[2];
                            d[1] = s[1];
                            d[2] = s[0];
                            d[3] = 0xFF;
                        }
                    }
                    surface.write_rect(&bgra, left, top, w, h);
                    bounds = Some(match bounds {
                        None => (left, top, right, bottom),
                        Some((l, t, r, b)) => {
                            (l.min(left), t.min(top), r.max(right), b.max(bottom))
                        }
                    });
                }
            }
        }
        if let Some((l, t, r, b)) = bounds {
            self.flush(pdu.surface_id, l, t, r, b);
        }
    }
}

fn log_progressive_stream_details(codec_context_id: u32, surface_id: u16, data: &[u8]) {
    let Ok(blocks) = decode_progressive_stream(data) else {
        return;
    };
    let mut regions = 0usize;
    let mut context_blocks = 0usize;
    let mut simple = 0usize;
    let mut first = 0usize;
    let mut upgrade = 0usize;
    let mut difference = 0usize;
    let mut reduce_extrapolate = false;

    for block in blocks {
        match block {
            ProgressiveBlock::Context(_) => context_blocks += 1,
            ProgressiveBlock::Region(region) => {
                regions += 1;
                reduce_extrapolate |= region.uses_reduce_extrapolate();
                for tile in region.tiles {
                    match tile {
                        ProgressiveTile::Simple(tile) => {
                            simple += 1;
                            difference += usize::from(tile.flags & 0x01 != 0);
                        }
                        ProgressiveTile::First(tile) => {
                            first += 1;
                            difference += usize::from(tile.flags & 0x01 != 0);
                        }
                        ProgressiveTile::Upgrade(_) => upgrade += 1,
                    }
                }
            }
            _ => {}
        }
    }

    tracing::debug!(
        "egfx: progressive stream surface={surface_id} context={codec_context_id} \
         regions={regions} context_blocks={context_blocks} simple={simple} first={first} \
         upgrade={upgrade} difference={difference} reduce_extrapolate={reduce_extrapolate}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    fn handler_with_surface(w: u16, h: u16) -> GfxHandler {
        let (tx, _rx) = unbounded_channel();
        // Keep the receiver alive is unnecessary: send errors are ignored.
        let mut handler = GfxHandler::new(SharedFramebuffer::new(), tx, true);
        handler.surfaces.insert(0, SurfaceBuf::new(w, h));
        handler
    }

    fn rect(left: u16, top: u16, right: u16, bottom: u16) -> ExclusiveRectangle {
        ExclusiveRectangle {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn avc444_off_drops_v10_7() {
        let (tx, _rx) = unbounded_channel();
        let on = GfxHandler::new(SharedFramebuffer::new(), tx.clone(), true).capabilities();
        let off = GfxHandler::new(SharedFramebuffer::new(), tx, false).capabilities();
        assert!(matches!(on[0], CapabilitySet::V10_7 { .. }));
        assert_eq!(off.len(), on.len() - 1);
        assert!(!off.iter().any(|c| matches!(c, CapabilitySet::V10_7 { .. })));
        assert!(matches!(off[0], CapabilitySet::V8_1 { .. }));
    }

    #[test]
    fn bitmap_update_swizzles_rgba_to_bgra() {
        let mut handler = handler_with_surface(4, 4);
        let rgba = [0x11, 0x22, 0x33, 0xFF].repeat(4);
        handler.apply_bitmap(0, &rect(1, 1, 3, 3), &rgba, 2, 2);
        let surface = &handler.surfaces[&0];
        let px = &surface.pixels[(4 + 1) * 4..(4 + 1) * 4 + 4]; // (1,1)
        assert_eq!(px, &[0x33, 0x22, 0x11, 0xFF]); // BGRA
    }

    #[test]
    fn solid_fill_writes_color() {
        let mut handler = handler_with_surface(4, 4);
        let pdu = SolidFillPdu {
            surface_id: 0,
            fill_pixel: Color {
                b: 1,
                g: 2,
                r: 3,
                xa: 0,
            },
            rectangles: vec![rect(0, 0, 4, 1)],
        };
        handler.on_solid_fill(&pdu);
        let surface = &handler.surfaces[&0];
        assert_eq!(&surface.pixels[0..4], &[1, 2, 3, 0xFF]);
        // Second row untouched.
        assert_eq!(&surface.pixels[16..20], &[0, 0, 0, 0]);
    }

    #[test]
    fn surface_copy_handles_overlap() {
        let mut handler = handler_with_surface(4, 1);
        // Pixels: [A B C D]; copy [A B] onto [B C].
        for (i, v) in [10u8, 20, 30, 40].iter().enumerate() {
            let p = &mut handler.surfaces.get_mut(&0).unwrap().pixels[i * 4..i * 4 + 4];
            p.copy_from_slice(&[*v; 4]);
        }
        let pdu = SurfaceToSurfacePdu {
            source_surface_id: 0,
            destination_surface_id: 0,
            source_rectangle: rect(0, 0, 2, 1),
            destination_points: vec![ironrdp_egfx::pdu::Point { x: 1, y: 0 }],
        };
        handler.on_surface_to_surface(&pdu);
        let px = &handler.surfaces[&0].pixels;
        assert_eq!(px[0], 10); // A
        assert_eq!(px[4], 10); // A (copied)
        assert_eq!(px[8], 20); // B (copied)
        assert_eq!(px[12], 40); // D untouched
    }

    #[test]
    fn cache_to_surface_past_edge_is_clipped() {
        let mut handler = handler_with_surface(4, 4);
        handler.surfaces.get_mut(&0).unwrap().origin = Some((0, 0));
        handler.cache.insert(1, (2, 2, vec![9; 2 * 2 * BPP]));
        let pdu = CacheToSurfacePdu {
            cache_slot: 1,
            surface_id: 0,
            destination_points: vec![ironrdp_egfx::pdu::Point { x: 3, y: 3 }],
        };
        handler.on_cache_to_surface(&pdu);
        let px = &handler.surfaces[&0].pixels;
        assert_eq!(&px[15 * BPP..16 * BPP], &[9; BPP]); // (3,3)
    }

    #[test]
    fn surface_to_surface_past_edge_is_clipped() {
        let mut handler = handler_with_surface(4, 4);
        handler.surfaces.get_mut(&0).unwrap().origin = Some((0, 0));
        let pdu = SurfaceToSurfacePdu {
            source_surface_id: 0,
            destination_surface_id: 0,
            source_rectangle: rect(0, 0, 2, 2),
            destination_points: vec![ironrdp_egfx::pdu::Point { x: 3, y: 3 }],
        };
        handler.on_surface_to_surface(&pdu);
    }

    #[test]
    fn bitmap_shorter_than_its_size_is_ignored() {
        let mut handler = handler_with_surface(4, 4);
        let rgba = [0x11, 0x22, 0x33, 0xFF].repeat(4); // one row of a 4x4 update
        handler.apply_bitmap(0, &rect(0, 0, 4, 4), &rgba, 4, 4);
        assert!(handler.surfaces[&0].pixels.iter().all(|&value| value == 0));
    }

    #[test]
    fn oversized_surface_is_not_allocated() {
        let (tx, _rx) = unbounded_channel();
        let mut handler = GfxHandler::new(SharedFramebuffer::new(), tx, true);
        handler.create_surface_buffer(0, u16::MAX, u16::MAX, None);
        assert!(!handler.surfaces.contains_key(&0));
    }

    #[test]
    fn evict_cache_entry_removes_stale_slot() {
        let mut handler = handler_with_surface(4, 4);
        handler.cache.insert(7, (1, 1, vec![1, 2, 3, 4]));

        handler.on_evict_cache_entry(&EvictCacheEntryPdu { cache_slot: 7 });

        assert!(!handler.cache.contains_key(&7));
    }

    #[test]
    fn reset_graphics_discards_stale_compositor_state() {
        let mut handler = handler_with_surface(4, 4);
        handler.surfaces.get_mut(&0).unwrap().pixels[0] = 77;
        handler.cache.insert(1, (1, 1, vec![0; BPP]));
        handler.warned_progressive = true;
        handler.progressive_frames = 5;
        handler.progressive_failures = 2;

        handler.on_reset_graphics(8, 6);

        assert!(handler.surfaces.is_empty());
        assert_eq!(handler.retired_surfaces[&0].pixels[0], 77);
        // The bitmap cache survives RESET_GRAPHICS: it is tied to the channel
        // lifetime, and Windows keeps referencing pre-reset slots (the full
        // post-reset repaint is driven from them).
        assert_eq!(handler.cache.len(), 1);
        assert!(!handler.warned_progressive);
        assert_eq!(handler.progressive_frames, 0);
        assert_eq!(handler.progressive_failures, 0);
        assert_eq!(handler.framebuffer.dimensions(), (8, 6));
    }

    #[test]
    fn recreated_same_size_surface_retains_pixels() {
        let mut handler = handler_with_surface(4, 4);
        handler.surfaces.get_mut(&0).unwrap().pixels[0..4].copy_from_slice(&[1, 2, 3, 4]);

        handler.on_reset_graphics(4, 4);
        handler.create_surface_buffer(0, 4, 4, None);

        assert_eq!(&handler.surfaces[&0].pixels[0..4], &[1, 2, 3, 4]);
    }

    #[test]
    fn recreated_different_size_surface_starts_empty() {
        let mut handler = handler_with_surface(4, 4);
        handler.surfaces.get_mut(&0).unwrap().pixels[0] = 77;

        handler.on_reset_graphics(8, 8);
        handler.create_surface_buffer(0, 8, 8, None);

        assert!(handler.surfaces[&0].pixels.iter().all(|&value| value == 0));
    }
}
