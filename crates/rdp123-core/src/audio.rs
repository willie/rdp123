//! Remote audio playback: bridges the RDPSND channel to the default output
//! device via cpal (CoreAudio on macOS).
//!
//! The player owns the output stream on the session thread; the RDPSND handler
//! only holds a sender, so PCM blocks flow handler → bounded queue → audio
//! callback. Exactly ONE format is advertised — 16-bit PCM stereo at 44.1 kHz,
//! the canonical RDP audio format every server offers — regardless of the
//! output device: the callback resamples 44.1 kHz to the device rate itself,
//! so negotiation never depends on what rates the device accepts.

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ironrdp::core::{Decode as _, Encode, EncodeResult, ReadCursor, WriteCursor};
use ironrdp::dvc::{DvcClientProcessor, DvcMessage, DvcProcessor};
use ironrdp::pdu::{decode_err, PduResult};
use ironrdp::rdpsnd::client::RdpsndClientHandler;
use ironrdp::rdpsnd::pdu::{
    AudioFormat, AudioFormatFlags, ClientAudioFormatPdu, ClientAudioOutputPdu, PitchPdu,
    QualityMode, QualityModePdu, ServerAudioFormatPdu, ServerAudioOutputPdu, TrainingConfirmPdu,
    Version, VolumePdu, WaveConfirmPdu, WaveFormat,
};

/// The canonical RDP PCM rate. Advertising only this maximizes the chance the
/// server's format list intersects ours (an empty intersection means silence).
const SOURCE_RATE: u32 = 44_100;
const CHANNELS: u16 = 2;
/// Bytes per source frame: stereo, 16-bit.
const FRAME_BYTES: usize = 4;

/// Pending PCM blocks between the session loop and the audio callback.
const QUEUE_BLOCKS: usize = 64;

/// Cap the callback-side buffer at ~500 ms so a hiccup can't build up latency.
const MAX_BUFFERED_MS: usize = 500;

/// Owns the output stream. Lives on the session thread for the whole session;
/// dropping it stops playback.
pub struct AudioPlayer {
    _stream: cpal::Stream,
    tx: SyncSender<Vec<u8>>,
    format: AudioFormat,
}

impl AudioPlayer {
    /// Open the default output device at its native rate; the callback
    /// resamples from the advertised 44.1 kHz.
    pub fn start() -> Result<Self> {
        let device = cpal::default_host()
            .default_output_device()
            .context("no audio output device")?;
        let out_rate = device
            .default_output_config()
            .context("querying the audio output configuration")?
            .sample_rate();

        let (tx, rx) = sync_channel::<Vec<u8>>(QUEUE_BLOCKS);
        let stream = device
            .build_output_stream(
                cpal::StreamConfig {
                    channels: CHANNELS,
                    sample_rate: out_rate,
                    buffer_size: cpal::BufferSize::Default,
                },
                pcm_callback(rx, out_rate),
                |e| tracing::warn!("audio output error: {e}"),
                None,
            )
            .context("opening the audio output stream")?;
        stream.play().context("starting the audio output stream")?;

        let block_align = CHANNELS * 2;
        let format = AudioFormat {
            format: WaveFormat::PCM,
            n_channels: CHANNELS,
            n_samples_per_sec: SOURCE_RATE,
            n_avg_bytes_per_sec: SOURCE_RATE * u32::from(block_align),
            n_block_align: block_align,
            bits_per_sample: 16,
            data: None,
        };
        tracing::info!(
            "audio output ready: device {out_rate} Hz, advertising PCM {SOURCE_RATE} Hz stereo"
        );
        Ok(Self {
            _stream: stream,
            tx,
            format,
        })
    }

    /// A fresh RDPSND handler for one connect attempt.
    pub fn handler(&self) -> RdpsndBackend {
        RdpsndBackend {
            formats: vec![self.format.clone()],
            tx: self.tx.clone(),
            negotiated: Cell::new(false),
            got_audio: false,
        }
    }
}

/// The audio callback: drain queued PCM blocks, linearly resample from the
/// 44.1 kHz source to the device rate, and pad underruns with silence.
fn pcm_callback(
    rx: Receiver<Vec<u8>>,
    out_rate: u32,
) -> impl FnMut(&mut [f32], &cpal::OutputCallbackInfo) + Send + 'static {
    let mut resampler = Resampler::new(out_rate);
    move |out, _| {
        while let Ok(block) = rx.try_recv() {
            resampler.push(&block);
        }
        resampler.fill(out);
    }
}

/// Streaming linear resampler from `SOURCE_RATE` to the device rate.
struct Resampler {
    buffered: VecDeque<u8>,
    max_buffered: usize,
    /// Source position advance per output frame.
    step: f64,
    /// Fractional position between `current` and `next` (0.0..1.0).
    position: f64,
    current: [f32; 2],
    next: [f32; 2],
}

impl Resampler {
    fn new(out_rate: u32) -> Self {
        Self {
            buffered: VecDeque::new(),
            max_buffered: SOURCE_RATE as usize * FRAME_BYTES * MAX_BUFFERED_MS / 1000,
            step: f64::from(SOURCE_RATE) / f64::from(out_rate),
            // Start past 1.0 so the first output frame loads real data.
            position: 1.0,
            current: [0.0; 2],
            next: [0.0; 2],
        }
    }

    fn push(&mut self, block: &[u8]) {
        self.buffered.extend(block);
        // Drop the oldest audio if the queue outgrew the latency cap.
        if self.buffered.len() > self.max_buffered {
            self.buffered
                .drain(..self.buffered.len() - self.max_buffered);
        }
    }

    fn fill(&mut self, out: &mut [f32]) {
        for frame in out.chunks_mut(usize::from(CHANNELS)) {
            while self.position >= 1.0 {
                self.position -= 1.0;
                self.current = self.next;
                self.next = self.pop_frame().unwrap_or([0.0, 0.0]);
            }
            let t = self.position as f32;
            frame[0] = self.current[0] + (self.next[0] - self.current[0]) * t;
            if let Some(right) = frame.get_mut(1) {
                *right = self.current[1] + (self.next[1] - self.current[1]) * t;
            }
            self.position += self.step;
        }
    }

    /// Pop one stereo 16-bit frame, or `None` on underrun.
    fn pop_frame(&mut self) -> Option<[f32; 2]> {
        if self.buffered.len() < FRAME_BYTES {
            return None;
        }
        let mut bytes = [0u8; FRAME_BYTES];
        for byte in &mut bytes {
            *byte = self.buffered.pop_front()?;
        }
        Some([
            f32::from(i16::from_le_bytes([bytes[0], bytes[1]])) / 32768.0,
            f32::from(i16::from_le_bytes([bytes[2], bytes[3]])) / 32768.0,
        ])
    }
}

/// RDPSND channel handler: forwards PCM to the player's queue.
#[derive(Debug)]
pub struct RdpsndBackend {
    formats: Vec<AudioFormat>,
    tx: SyncSender<Vec<u8>>,
    /// Set when the server asked for our formats — proves the channel is up.
    negotiated: Cell<bool>,
    got_audio: bool,
}

impl RdpsndClientHandler for RdpsndBackend {
    fn get_formats(&self) -> &[AudioFormat] {
        if !self.negotiated.replace(true) {
            tracing::info!("rdpsnd: negotiating audio (advertising PCM {SOURCE_RATE} Hz stereo)");
        }
        &self.formats
    }

    fn wave(&mut self, format_no: usize, _ts: u32, data: Cow<'_, [u8]>) {
        // Only one format is ever advertised, so all wave data is our PCM
        // format regardless of how the server indexes `format_no`.
        if !self.got_audio {
            self.got_audio = true;
            tracing::info!("rdpsnd: receiving remote audio (format_no {format_no})");
        }
        // Dropping on a full queue trades a glitch for bounded memory/latency.
        let _ = self.tx.try_send(data.into_owned());
    }

    fn set_volume(&mut self, volume: VolumePdu) {
        // Deliberately not applied: honoring a zero/low session volume would
        // silence playback invisibly. The local system volume is in charge.
        tracing::debug!(?volume, "ignoring server volume");
    }

    fn set_pitch(&mut self, _pitch: PitchPdu) {}

    fn close(&mut self) {
        tracing::info!("rdpsnd: server closed the audio stream");
    }
}

// ---------------------------------------------------------------------------
// MS-RDPEA over the dynamic channel. Windows 7+ servers prefer the dynamic
// transport when the client supports DVC (which we do, for Display Control);
// without this listener such servers never use the static channel and the
// session stays silent. Same PDUs, same state machine as the static path.
// ---------------------------------------------------------------------------

const AUDIO_DVC_NAME: &str = "AUDIO_PLAYBACK_DVC";

/// Newtype so rdpsnd PDUs can be sent on a dynamic channel.
struct AudioDvcPdu(ClientAudioOutputPdu);

impl Encode for AudioDvcPdu {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        self.0.encode(dst)
    }
    fn name(&self) -> &'static str {
        self.0.name()
    }
    fn size(&self) -> usize {
        self.0.size()
    }
}

impl ironrdp::dvc::DvcEncode for AudioDvcPdu {}

fn dvc_msg(pdu: ClientAudioOutputPdu) -> DvcMessage {
    Box::new(AudioDvcPdu(pdu))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DvcAudioState {
    Start,
    WaitingForTraining,
    Ready,
    Stop,
}

/// Audio playback over the dynamic channel, feeding the same [`RdpsndBackend`].
#[derive(Debug)]
pub struct RdpsndDvcChannel {
    backend: RdpsndBackend,
    state: DvcAudioState,
    server_format: Option<ServerAudioFormatPdu>,
}

impl RdpsndDvcChannel {
    pub fn new(backend: RdpsndBackend) -> Self {
        Self {
            backend,
            state: DvcAudioState::Start,
            server_format: None,
        }
    }

    fn version(&self) -> Version {
        self.server_format
            .as_ref()
            .map(|f| f.version)
            .unwrap_or(Version::V8)
    }

    /// Reply to a server format list: our single PCM format if the server
    /// offers it too, plus the quality mode for v6+.
    fn negotiate(&mut self, af: ServerAudioFormatPdu) -> Vec<DvcMessage> {
        let ours = self.backend.get_formats().to_vec();
        let formats: Vec<AudioFormat> = ours
            .into_iter()
            .filter(|f| af.formats.contains(f))
            .collect();
        if formats.is_empty() {
            tracing::warn!(
                "rdpsnd(dvc): server offers no PCM {SOURCE_RATE} Hz stereo; no audio possible"
            );
        }
        self.server_format = Some(af);
        self.state = DvcAudioState::WaitingForTraining;

        let mut messages = vec![dvc_msg(ClientAudioOutputPdu::AudioFormat(
            ClientAudioFormatPdu {
                version: self.version(),
                flags: AudioFormatFlags::ALIVE,
                formats,
                volume_left: 0xFFFF,
                volume_right: 0xFFFF,
                pitch: 0x0001_0000,
                dgram_port: 0,
            },
        ))];
        if self.version() >= Version::V6 {
            messages.push(dvc_msg(ClientAudioOutputPdu::QualityMode(QualityModePdu {
                quality_mode: QualityMode::High,
            })));
        }
        messages
    }
}

impl ironrdp::core::AsAny for RdpsndDvcChannel {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl DvcProcessor for RdpsndDvcChannel {
    fn channel_name(&self) -> &str {
        AUDIO_DVC_NAME
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        tracing::info!("rdpsnd: server opened the dynamic audio channel");
        Ok(Vec::new())
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        let pdu = ServerAudioOutputPdu::decode(&mut ReadCursor::new(payload))
            .map_err(|e| decode_err!(e))?;

        match self.state {
            DvcAudioState::Start => {
                if let ServerAudioOutputPdu::AudioFormat(af) = pdu {
                    Ok(self.negotiate(af))
                } else {
                    tracing::warn!("rdpsnd(dvc): unexpected PDU before format negotiation");
                    self.state = DvcAudioState::Stop;
                    Ok(Vec::new())
                }
            }
            DvcAudioState::WaitingForTraining => match pdu {
                ServerAudioOutputPdu::Training(t) => {
                    self.state = DvcAudioState::Ready;
                    Ok(vec![training_confirm(&t)])
                }
                // Some servers skip training and start streaming directly.
                pdu @ ServerAudioOutputPdu::Wave2(_) => {
                    self.state = DvcAudioState::Ready;
                    Ok(self.streaming(pdu))
                }
                _ => {
                    tracing::warn!("rdpsnd(dvc): unexpected PDU while waiting for training");
                    self.state = DvcAudioState::Stop;
                    Ok(Vec::new())
                }
            },
            DvcAudioState::Ready => Ok(self.streaming(pdu)),
            DvcAudioState::Stop => Ok(Vec::new()),
        }
    }
}

impl RdpsndDvcChannel {
    /// Handle a PDU in the streaming (Ready) state.
    fn streaming(&mut self, pdu: ServerAudioOutputPdu<'_>) -> Vec<DvcMessage> {
        match pdu {
            ServerAudioOutputPdu::Wave2(wave) => {
                self.backend
                    .wave(usize::from(wave.format_no), wave.audio_timestamp, wave.data);
                let confirm = WaveConfirmPdu {
                    timestamp: wave.timestamp,
                    block_no: wave.block_no,
                };
                vec![dvc_msg(ClientAudioOutputPdu::WaveConfirm(confirm))]
            }
            ServerAudioOutputPdu::Volume(v) => {
                self.backend.set_volume(v);
                Vec::new()
            }
            ServerAudioOutputPdu::Pitch(p) => {
                self.backend.set_pitch(p);
                Vec::new()
            }
            ServerAudioOutputPdu::Close => {
                self.backend.close();
                Vec::new()
            }
            ServerAudioOutputPdu::Training(t) => vec![training_confirm(&t)],
            ServerAudioOutputPdu::AudioFormat(af) => {
                self.backend.close();
                self.negotiate(af)
            }
            _ => {
                tracing::debug!("rdpsnd(dvc): ignoring unsupported PDU");
                Vec::new()
            }
        }
    }
}

/// Bytes of the RDPSND message header: msgType, bPad, BodySize.
const SNDPROLOG_SIZE: usize = 4;

/// The confirm must echo the server's wPackSize. IronRDP reads wPackSize as
/// the size of the whole PDU and keeps only the bytes after the header and
/// the timestamp/size fields, so add those back.
fn training_confirm(t: &ironrdp::rdpsnd::pdu::TrainingPdu) -> DvcMessage {
    let pack_size = if t.data.is_empty() {
        0
    } else {
        t.size() + SNDPROLOG_SIZE
    };
    dvc_msg(ClientAudioOutputPdu::TrainingConfirm(TrainingConfirmPdu {
        timestamp: t.timestamp,
        pack_size: u16::try_from(pack_size).unwrap_or(0),
    }))
}

impl DvcClientProcessor for RdpsndDvcChannel {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "opens the real default audio output device"]
    fn audio_player_advertises_the_canonical_rdp_format() {
        let player = AudioPlayer::start().expect("audio output should start");
        let handler = player.handler();
        let formats = handler.get_formats();
        assert_eq!(formats.len(), 1);
        assert_eq!(formats[0].format, WaveFormat::PCM);
        assert_eq!(formats[0].bits_per_sample, 16);
        assert_eq!(formats[0].n_channels, 2);
        // The advertised rate must be the canonical RDP rate, NOT the device
        // rate — negotiation must never depend on the local hardware.
        assert_eq!(formats[0].n_samples_per_sec, 44_100);
    }

    #[test]
    fn training_confirm_echoes_the_server_pack_size() {
        // GNOME Remote Desktop (FreeRDP server): wPackSize 1024 followed by
        // 1024 bytes, and it ignores a confirm that does not echo 1024.
        let (tx, _rx) = sync_channel(1);
        let mut channel = RdpsndDvcChannel::new(RdpsndBackend {
            formats: Vec::new(),
            tx,
            negotiated: Cell::new(false),
            got_audio: false,
        });
        channel.state = DvcAudioState::WaitingForTraining;
        let mut training = vec![0x06, 0x00]; // SNDC_TRAINING, bPad
        training.extend_from_slice(&(4u16 + 1024).to_le_bytes()); // BodySize
        training.extend_from_slice(&0u16.to_le_bytes()); // wTimeStamp
        training.extend_from_slice(&1024u16.to_le_bytes()); // wPackSize
        training.extend_from_slice(&[0; 1024]);

        let replies = channel.process(5, &training).unwrap();

        let bytes = ironrdp::core::encode_vec(replies[0].as_ref()).unwrap();
        let reply = ClientAudioOutputPdu::decode(&mut ReadCursor::new(&bytes)).unwrap();
        let ClientAudioOutputPdu::TrainingConfirm(confirm) = reply else {
            panic!("expected a training confirm, got {reply:?}");
        };
        assert_eq!(confirm.pack_size, 1024);
    }

    fn s16(v: f32) -> [u8; 2] {
        ((v * 32768.0) as i16).to_le_bytes()
    }

    #[test]
    fn resampler_passthrough_at_equal_rates() {
        // step == 1.0: after priming, output must reproduce the input samples.
        let mut r = Resampler::new(SOURCE_RATE);
        let mut block = Vec::new();
        for v in [0.5f32, -0.5, 0.25, -0.25] {
            block.extend_from_slice(&s16(v));
        }
        r.push(&block);
        let mut out = [0.0f32; 6];
        r.fill(&mut out);
        // Frame 0 primes from silence; frames 1 and 2 carry the input.
        assert!((out[2] - 0.5).abs() < 0.01, "left sample: {}", out[2]);
        assert!((out[3] + 0.5).abs() < 0.01, "right sample: {}", out[3]);
        assert!((out[4] - 0.25).abs() < 0.01, "left sample 2: {}", out[4]);
    }

    #[test]
    fn resampler_upsamples_44100_to_48000() {
        // 48 kHz out from 44.1 kHz in: 160 output frames need ~147 input
        // frames; with 200 input frames available the output is fully fed.
        let mut r = Resampler::new(48_000);
        let mut block = Vec::new();
        for _ in 0..200 {
            block.extend_from_slice(&s16(0.5));
            block.extend_from_slice(&s16(0.5));
        }
        r.push(&block);
        let mut out = [0.0f32; 320];
        r.fill(&mut out);
        // After the priming frame, a constant signal must stay constant.
        for (i, sample) in out.iter().enumerate().skip(4) {
            assert!((sample - 0.5).abs() < 0.01, "sample {i}: {sample}");
        }
    }

    #[test]
    fn resampler_underrun_is_silence() {
        let mut r = Resampler::new(48_000);
        let mut out = [1.0f32; 32];
        r.fill(&mut out);
        assert!(out.iter().all(|s| *s == 0.0));
    }
}
