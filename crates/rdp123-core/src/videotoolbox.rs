//! H.264 decoding with VideoToolbox.
//!
//! Implements `ironrdp_egfx`'s `H264Decoder` on top of a
//! `VTDecompressionSession`, which uses the Mac's hardware decoder when one is
//! available. If a decode fails, the session is rebuilt from the stored
//! SPS/PPS and the frame retried once. If VideoToolbox can't handle the
//! stream at all, the decoder switches to OpenH264 for the rest of the
//! connection.
//!
//! One SPS and one PPS are kept, the latest of each, which covers servers that
//! use a single parameter set of each kind (GNOME Remote Desktop does).
//!
//! VideoToolbox is asked for its native output format rather than a specific
//! pixel format, so it never applies a range conversion of its own. MS-RDPEGFX
//! AVC420 carries full-range BT.709, and servers such as GNOME Remote Desktop
//! 50 don't signal that in the stream; letting VideoToolbox convert would
//! treat the samples as limited range and wash out the picture.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use ironrdp_egfx::decode::{
    DecodedFrame, DecoderError, DecoderResult, H264Decoder, OpenH264Decoder,
};
use objc2_core_foundation::{
    kCFAllocatorNull, CFBoolean, CFDictionary, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
};
use objc2_core_video::{
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVImageBuffer,
    CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{
    kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
    kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder, VTDecodeFrameFlags,
    VTDecodeInfoFlags, VTDecompressionSession, VTSessionCopyProperty,
};

const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;

/// A decoded picture handed from the decode callback to `decode_sample`.
struct Picture(CFRetained<CVImageBuffer>);

// SAFETY: CoreVideo pixel buffers are reference-counted CoreFoundation
// objects that may be passed between threads; only one thread uses it at a
// time here (the callback stores it, `decode_sample` takes it after).
unsafe impl Send for Picture {}

/// Where the decode callback leaves its picture or error status. Shared with a
/// block VideoToolbox may copy and release on its own thread, so it's `Arc`.
type DecodeOutput = Arc<Mutex<Option<Result<Picture, i32>>>>;

/// Why VideoToolbox didn't produce a frame; each case is handled differently.
enum Failure {
    /// The access unit held no picture (or the decoder dropped it). Nothing
    /// is wrong with VideoToolbox.
    NoPicture(DecoderError),
    /// The decode itself failed, for example because the system invalidated
    /// the session. Rebuilding the session may fix it.
    Decode(DecoderError),
    /// VideoToolbox can't handle this stream at all.
    Unusable(DecoderError),
}

/// H.264 decoder backed by VideoToolbox, with OpenH264 as the fallback.
pub struct VideoToolboxDecoder {
    session: Option<CFRetained<VTDecompressionSession>>,
    format: Option<CFRetained<CMFormatDescription>>,
    sps: Vec<u8>,
    pps: Vec<u8>,
    /// Slice NAL units of the current frame, each with a 4-byte BE length.
    sample: Vec<u8>,
    fallback: Option<OpenH264Decoder>,
}

// SAFETY: VideoToolbox sessions, format descriptions and the pixel buffers
// they return are CoreFoundation objects that may be used from any thread;
// the decoder is only ever driven from one thread at a time (`&mut self`).
unsafe impl Send for VideoToolboxDecoder {}

impl VideoToolboxDecoder {
    pub fn new() -> Self {
        Self {
            session: None,
            format: None,
            sps: Vec::new(),
            pps: Vec::new(),
            sample: Vec::new(),
            fallback: None,
        }
    }

    /// Store the access unit's SPS/PPS and collect its other NAL units into
    /// `self.sample`. Returns whether the parameter sets changed.
    fn ingest(&mut self, data: &[u8]) -> DecoderResult<bool> {
        self.sample.clear();
        let mut parameter_sets_changed = false;
        for nal in nal_units(data) {
            let Some(&header) = nal.first() else { continue };
            match header & 0x1f {
                NAL_SPS => {
                    if self.sps != nal {
                        self.sps = nal.to_vec();
                        parameter_sets_changed = true;
                    }
                }
                NAL_PPS => {
                    if self.pps != nal {
                        self.pps = nal.to_vec();
                        parameter_sets_changed = true;
                    }
                }
                NAL_AUD => {}
                _ => {
                    let len = u32::try_from(nal.len())
                        .map_err(|_| DecoderError::msg("NAL unit too large"))?;
                    self.sample.extend_from_slice(&len.to_be_bytes());
                    self.sample.extend_from_slice(nal);
                }
            }
        }

        Ok(parameter_sets_changed)
    }

    /// Decode `self.sample` with the open session.
    fn decode_sample(&mut self) -> Result<DecodedFrame, Failure> {
        let session = self.session.as_ref().expect("session is open");
        let format = self.format.as_ref().expect("session is open");

        // The block buffer borrows `self.sample` (kCFAllocatorNull: CoreMedia
        // never frees it). Decoding is synchronous, so the borrow ends before
        // this function returns.
        let mut block: *mut CMBlockBuffer = ptr::null_mut();
        let status = unsafe {
            CMBlockBuffer::create_with_memory_block(
                None,
                self.sample.as_mut_ptr().cast::<c_void>(),
                self.sample.len(),
                kCFAllocatorNull,
                ptr::null(),
                0,
                self.sample.len(),
                0,
                NonNull::from(&mut block),
            )
        };
        let block = retained(status, block, "CMBlockBufferCreateWithMemoryBlock")
            .map_err(Failure::Decode)?;

        let sample_size = self.sample.len();
        let mut sample: *mut CMSampleBuffer = ptr::null_mut();
        let status = unsafe {
            CMSampleBuffer::create_ready(
                None,
                Some(&block),
                Some(format),
                1,
                0,
                ptr::null(),
                1,
                &sample_size,
                NonNull::from(&mut sample),
            )
        };
        let sample =
            retained(status, sample, "CMSampleBufferCreateReady").map_err(Failure::Decode)?;

        let output: DecodeOutput = Arc::new(Mutex::new(None));
        let handler_output = Arc::clone(&output);
        let handler = RcBlock::new(
            move |status: i32,
                  _flags: VTDecodeInfoFlags,
                  image: *mut CVImageBuffer,
                  _pts: CMTime,
                  _duration: CMTime| {
                let result = match NonNull::new(image) {
                    // SAFETY: VideoToolbox passes a valid image buffer it
                    // owns; retaining it keeps it alive past the handler.
                    Some(image) if status == 0 => Ok(Picture(unsafe { CFRetained::retain(image) })),
                    _ => Err(status),
                };
                *handler_output.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
            },
        );

        let mut info = VTDecodeInfoFlags(0);
        let status = unsafe {
            session.decode_frame_with_output_handler(
                &sample,
                VTDecodeFrameFlags(0),
                &mut info,
                RcBlock::as_ptr(&handler),
            )
        };
        if status != 0 {
            return Err(Failure::Decode(DecoderError::msg(format!(
                "VTDecompressionSessionDecodeFrame failed: {status}"
            ))));
        }
        // Synchronous decode calls the handler before returning; this is a
        // no-op then, and a safeguard otherwise.
        unsafe { session.wait_for_asynchronous_frames() };

        let result = output.lock().unwrap_or_else(|e| e.into_inner()).take();
        let image = match result {
            Some(Ok(Picture(image))) => image,
            // Status 0 without an image: the decoder dropped the frame.
            Some(Err(0)) | None => {
                return Err(Failure::NoPicture(DecoderError::msg(
                    "VideoToolbox returned no picture",
                )))
            }
            Some(Err(status)) => {
                return Err(Failure::Decode(DecoderError::msg(format!(
                    "VideoToolbox decode error: {status}"
                ))))
            }
        };
        nv12_to_rgba(&image).map_err(Failure::Unusable)
    }

    /// Give up on VideoToolbox for the rest of the connection. OpenH264 is
    /// handed the stored SPS/PPS ahead of this access unit, since the server
    /// only resends them with the next keyframe.
    fn switch_to_openh264(
        &mut self,
        data: &[u8],
        cause: &DecoderError,
    ) -> DecoderResult<DecodedFrame> {
        tracing::warn!(
            "egfx: VideoToolbox can't decode this stream ({cause}); switching to OpenH264"
        );
        let mut primed = Vec::new();
        for nal in [&self.sps[..], &self.pps[..]]
            .into_iter()
            .filter(|set| !set.is_empty())
            .chain(nal_units(data))
        {
            primed.extend_from_slice(&[0, 0, 0, 1]);
            primed.extend_from_slice(nal);
        }
        self.close_session();
        let fallback = self.fallback.insert(OpenH264Decoder::new()?);
        fallback.decode(&primed)
    }

    fn open_session(&mut self) -> DecoderResult<()> {
        if self.sps.is_empty() || self.pps.is_empty() {
            return Err(DecoderError::msg("no SPS/PPS received yet"));
        }

        let sets = [
            NonNull::new(self.sps.as_ptr().cast_mut()).expect("non-empty SPS"),
            NonNull::new(self.pps.as_ptr().cast_mut()).expect("non-empty PPS"),
        ];
        let sizes = [self.sps.len(), self.pps.len()];
        let mut format: *const CMFormatDescription = ptr::null();
        let status = unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                None,
                sets.len(),
                NonNull::from(&sets[0]),
                NonNull::from(&sizes[0]),
                4,
                NonNull::from(&mut format),
            )
        };
        let format = retained(
            status,
            format.cast_mut(),
            "CMVideoFormatDescriptionCreateFromH264ParameterSets",
        )?;

        if let Some(session) = &self.session {
            if unsafe { session.can_accept_format_description(&format) } {
                self.format = Some(format);
                return Ok(());
            }
            unsafe { session.invalidate() };
            self.session = None;
        }

        let key: &CFString =
            unsafe { kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder };
        let spec =
            CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[CFBoolean::new(true)]);

        let mut session: *mut VTDecompressionSession = ptr::null_mut();
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                &format,
                Some(spec.as_ref()),
                None,
                ptr::null(),
                NonNull::from(&mut session),
            )
        };
        let session = retained(status, session, "VTDecompressionSessionCreate")?;

        tracing::info!(
            hardware = uses_hardware(&session),
            "egfx: H.264 decoding with VideoToolbox"
        );
        self.session = Some(session);
        self.format = Some(format);
        Ok(())
    }

    /// Tear down the session but keep the parameter sets, so it can be rebuilt.
    fn drop_session(&mut self) {
        if let Some(session) = self.session.take() {
            unsafe { session.invalidate() };
        }
        self.format = None;
    }

    fn close_session(&mut self) {
        self.drop_session();
        self.sps.clear();
        self.pps.clear();
    }
}

impl Default for VideoToolboxDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for VideoToolboxDecoder {
    fn drop(&mut self) {
        self.close_session();
    }
}

impl H264Decoder for VideoToolboxDecoder {
    fn decode(&mut self, data: &[u8]) -> DecoderResult<DecodedFrame> {
        if let Some(fallback) = &mut self.fallback {
            return fallback.decode(data);
        }

        let parameter_sets_changed = self.ingest(data)?;
        if self.sps.is_empty() || self.pps.is_empty() {
            return Err(DecoderError::msg("no SPS/PPS received yet"));
        }
        if parameter_sets_changed || self.session.is_none() {
            if let Err(e) = self.open_session() {
                return self.switch_to_openh264(data, &e);
            }
        }
        if self.sample.is_empty() {
            return Err(DecoderError::msg("no slice data in frame"));
        }

        match self.decode_sample() {
            Ok(frame) => Ok(frame),
            Err(Failure::NoPicture(e)) => Err(e),
            Err(Failure::Unusable(e)) => self.switch_to_openh264(data, &e),
            Err(Failure::Decode(e)) => {
                // The system can invalidate a session (sleep/wake, GPU change);
                // rebuild it from the stored parameter sets and retry once.
                tracing::warn!("egfx: VideoToolbox decode failed ({e}); rebuilding the session");
                self.drop_session();
                if let Err(e) = self.open_session() {
                    return self.switch_to_openh264(data, &e);
                }
                match self.decode_sample() {
                    Ok(frame) => Ok(frame),
                    Err(Failure::Unusable(e)) => self.switch_to_openh264(data, &e),
                    Err(Failure::NoPicture(e) | Failure::Decode(e)) => Err(e),
                }
            }
        }
    }

    fn reset(&mut self) {
        self.close_session();
        if let Some(fallback) = &mut self.fallback {
            fallback.reset();
        }
    }
}

/// Take ownership of a CoreFoundation object returned through an out
/// parameter, or turn the status into an error.
fn retained<T: objc2_core_foundation::Type>(
    status: i32,
    object: *mut T,
    call: &str,
) -> DecoderResult<CFRetained<T>> {
    match NonNull::new(object) {
        // SAFETY: CoreFoundation "Create" functions return a +1 reference.
        Some(object) if status == 0 => Ok(unsafe { CFRetained::from_raw(object) }),
        _ => Err(DecoderError::msg(format!("{call} failed: {status}"))),
    }
}

fn uses_hardware(session: &VTDecompressionSession) -> bool {
    let mut value: *const CFType = ptr::null();
    let key: &CFString =
        unsafe { kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder };
    let status = unsafe {
        VTSessionCopyProperty(
            session.as_ref(),
            key,
            None,
            (&mut value as *mut *const CFType).cast::<c_void>(),
        )
    };
    let Some(value) = NonNull::new(value.cast_mut()) else {
        return false;
    };
    // SAFETY: "Copy" returns a +1 reference.
    let value = unsafe { CFRetained::from_raw(value) };
    status == 0
        && value
            .downcast_ref::<CFBoolean>()
            .is_some_and(CFBoolean::as_bool)
}

/// Iterate over the NAL units of an access unit, given either as an Annex B
/// byte stream (start codes, as MS-RDPEGFX specifies) or as 4-byte BE
/// length-prefixed units.
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    if is_length_prefixed(data) {
        let mut offset = 0;
        while offset + 4 <= data.len() {
            let len = u32::from_be_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize;
            units.push(&data[offset + 4..offset + 4 + len]);
            offset += 4 + len;
        }
        return units;
    }

    // Annex B: split on 00 00 01; a preceding 00 (4-byte start code) and any
    // trailing zero bytes belong to the separator, not the NAL unit.
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    for (n, &start) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(data.len(), |&next| next - 3);
        while end > start && data[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            units.push(&data[start..end]);
        }
    }
    units
}

/// Same check as `ironrdp_egfx`'s decoder: a chain of nonzero 4-byte BE
/// lengths that lands exactly on the end of the buffer.
fn is_length_prefixed(data: &[u8]) -> bool {
    let mut offset = 0usize;
    loop {
        if offset == data.len() {
            return !data.is_empty();
        }
        let Some(len_bytes) = data.get(offset..offset + 4) else {
            return false;
        };
        let len =
            u32::from_be_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]) as usize;
        match (offset + 4).checked_add(len) {
            Some(next) if len > 0 && next <= data.len() => offset = next,
            _ => return false,
        }
    }
}

/// Convert a bi-planar 4:2:0 pixel buffer to RGBA, reading the samples as
/// full-range BT.709 whatever range the buffer is labelled with.
fn nv12_to_rgba(image: &CVImageBuffer) -> DecoderResult<DecodedFrame> {
    let format = CVPixelBufferGetPixelFormatType(image);
    if format != kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
        && format != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
    {
        return Err(DecoderError::msg(format!(
            "unexpected VideoToolbox pixel format {:?}",
            format.to_be_bytes().map(char::from)
        )));
    }

    let width = CVPixelBufferGetWidth(image);
    let height = CVPixelBufferGetHeight(image);
    let (w32, h32) = (
        u32::try_from(width).map_err(|_| DecoderError::msg("frame too wide"))?,
        u32::try_from(height).map_err(|_| DecoderError::msg("frame too tall"))?,
    );

    let status = unsafe { CVPixelBufferLockBaseAddress(image, CVPixelBufferLockFlags::ReadOnly) };
    if status != 0 {
        return Err(DecoderError::msg(format!(
            "CVPixelBufferLockBaseAddress failed: {status}"
        )));
    }
    let result = (|| {
        let y_stride = CVPixelBufferGetBytesPerRowOfPlane(image, 0);
        let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(image, 1);
        let y_ptr = CVPixelBufferGetBaseAddressOfPlane(image, 0).cast::<u8>();
        let uv_ptr = CVPixelBufferGetBaseAddressOfPlane(image, 1).cast::<u8>();
        if y_ptr.is_null() || uv_ptr.is_null() {
            return Err(DecoderError::msg("pixel buffer has no base address"));
        }
        // SAFETY: the buffer is locked; each plane holds `stride * rows` bytes.
        let y_plane = unsafe { std::slice::from_raw_parts(y_ptr, y_stride * height) };
        let uv_plane =
            unsafe { std::slice::from_raw_parts(uv_ptr, uv_stride * height.div_ceil(2)) };

        let mut rgba = vec![0u8; width * height * 4];
        yuv::yuv_nv12_to_rgba(
            &yuv::YuvBiPlanarImage {
                y_plane,
                y_stride: y_stride as u32,
                uv_plane,
                uv_stride: uv_stride as u32,
                width: w32,
                height: h32,
            },
            &mut rgba,
            w32 * 4,
            yuv::YuvRange::Full,
            yuv::YuvStandardMatrix::Bt709,
            yuv::YuvConversionMode::Balanced,
        )
        .map_err(|e| DecoderError::new("failed to convert NV12 to RGBA", e))?;
        Ok(DecodedFrame::new(rgba, w32, h32))
    })();
    unsafe { CVPixelBufferUnlockBaseAddress(image, CVPixelBufferLockFlags::ReadOnly) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 320;
    const H: usize = 240;

    /// Encode `count` frames of a moving colour gradient as Annex B H.264.
    fn encode_frames(count: usize) -> Vec<Vec<u8>> {
        let mut encoder = openh264::encoder::Encoder::new().expect("encoder");
        (0..count)
            .map(|n| {
                let mut rgba = vec![0u8; W * H * 4];
                for y in 0..H {
                    for x in 0..W {
                        let i = (y * W + x) * 4;
                        rgba[i] = ((x + n * 7) % 256) as u8;
                        rgba[i + 1] = ((y + n * 3) % 256) as u8;
                        rgba[i + 2] = ((x + y) / 2 % 256) as u8;
                        rgba[i + 3] = 0xff;
                    }
                }
                let rgba = openh264::formats::RgbaSliceU8::new(&rgba, (W, H));
                let yuv = openh264::formats::YUVBuffer::from_rgb_source(rgba);
                encoder.encode(&yuv).expect("encode").to_vec()
            })
            .collect()
    }

    fn max_difference(a: &DecodedFrame, b: &DecodedFrame) -> u8 {
        assert_eq!((a.width(), a.height()), (b.width(), b.height()));
        a.data()
            .iter()
            .zip(b.data())
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn matches_openh264_output() {
        // H.264 decoding is bit-exact, and both paths convert the same YUV
        // as full-range BT.709, so the RGBA must agree to within rounding.
        let mut vt = VideoToolboxDecoder::new();
        let mut reference = OpenH264Decoder::new().expect("OpenH264");
        for (n, frame) in encode_frames(6).iter().enumerate() {
            let got = vt.decode(frame).expect("VideoToolbox decode");
            let want = reference.decode(frame).expect("OpenH264 decode");
            assert!(vt.fallback.is_none(), "frame {n} fell back to OpenH264");
            let diff = max_difference(&got, &want);
            assert!(diff <= 2, "frame {n}: max channel difference {diff}");
        }
    }

    #[test]
    fn decodes_frame_with_access_unit_delimiter() {
        // GNOME Remote Desktop starts every frame with an access unit delimiter.
        let mut frame = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x10];
        frame.extend_from_slice(&encode_frames(1)[0]);
        let mut vt = VideoToolboxDecoder::new();
        let got = vt.decode(&frame).expect("decode");
        assert!(vt.fallback.is_none());
        assert_eq!((got.width(), got.height()), (W as u32, H as u32));
    }

    #[test]
    fn decodes_length_prefixed_input() {
        let annex_b = &encode_frames(1)[0];
        let mut prefixed = Vec::new();
        for nal in nal_units(annex_b) {
            prefixed.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            prefixed.extend_from_slice(nal);
        }
        assert!(is_length_prefixed(&prefixed));
        let mut vt = VideoToolboxDecoder::new();
        vt.decode(&prefixed).expect("decode");
        assert!(vt.fallback.is_none());
    }

    #[test]
    fn parameter_sets_without_a_picture_keep_videotoolbox() {
        // An access unit carrying only SPS/PPS has no picture. That's an
        // error for this frame, not a reason to give up on VideoToolbox.
        let frames = encode_frames(2);
        let parameter_sets: Vec<u8> = nal_units(&frames[0])
            .into_iter()
            .filter(|nal| matches!(nal[0] & 0x1f, NAL_SPS | NAL_PPS))
            .flat_map(|nal| [&[0, 0, 0, 1][..], nal].concat())
            .collect();
        let mut vt = VideoToolboxDecoder::new();
        assert!(vt.decode(&parameter_sets).is_err());
        assert!(vt.fallback.is_none());
        vt.decode(&frames[0])
            .expect("decode after parameter-set-only unit");
        vt.decode(&frames[1]).expect("decode P-frame");
        assert!(vt.fallback.is_none());
    }

    #[test]
    fn matches_openh264_at_1080p() {
        // 1080 isn't a multiple of 16, so the stream is cropped; the size is
        // also large enough that VideoToolbox picks its hardware decoder.
        let (w, h) = (1920usize, 1080usize);
        let mut encoder = openh264::encoder::Encoder::new().expect("encoder");
        let mut rgba = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                rgba[i] = (x % 256) as u8;
                rgba[i + 1] = (y % 256) as u8;
                rgba[i + 2] = ((x ^ y) % 256) as u8;
                rgba[i + 3] = 0xff;
            }
        }
        let yuv = openh264::formats::YUVBuffer::from_rgb_source(
            openh264::formats::RgbaSliceU8::new(&rgba, (w, h)),
        );
        let frame = encoder.encode(&yuv).expect("encode").to_vec();

        let mut vt = VideoToolboxDecoder::new();
        let got = vt.decode(&frame).expect("VideoToolbox decode");
        let want = OpenH264Decoder::new()
            .expect("OpenH264")
            .decode(&frame)
            .expect("decode");
        assert!(vt.fallback.is_none());
        assert_eq!((got.width(), got.height()), (w as u32, h as u32));
        let diff = max_difference(&got, &want);
        assert!(diff <= 2, "max channel difference {diff}");
    }

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let data = [
            0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x68, 0xBB, 0, 0, 0, 1, 0x65, 0xCC,
        ];
        let units = nal_units(&data);
        assert_eq!(
            units,
            vec![&[0x67, 0xAA][..], &[0x68, 0xBB][..], &[0x65, 0xCC][..]]
        );
    }
}
