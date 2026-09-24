//! Offline replay of a captured EGFX session through the real compositor.
//!
//! Feed it a file produced by running the app with `RDP123_EGFX_CAPTURE=<file>`;
//! every captured compositing operation (progressive, ClearCodec/planar,
//! solid fills, surface/cache copies, resets, surface lifecycle) is replayed
//! through the actual `GfxHandler`, and the shared framebuffer is written out
//! as PPM images. What this produces is — bit for bit — what the live client
//! would have painted.
//!
//! Usage:
//!   cargo run -p rdp123-core --example egfx_replay -- <capture> [out-dir] [--every N]
//!   cargo run -p rdp123-core --example egfx_replay -- --clear <clearcodec-fail.bin>

use std::io::Read;

use ironrdp::graphics::clearcodec::ClearCodecDecoder;
use ironrdp::pdu::geometry::ExclusiveRectangle;
use ironrdp_egfx::client::GraphicsPipelineHandler as _;
use ironrdp_egfx::pdu::{
    CacheToSurfacePdu, Codec1Type, Codec2Type, Color, EvictCacheEntryPdu, GfxPdu, PixelFormat,
    Point, SolidFillPdu, SurfaceToCachePdu, SurfaceToSurfacePdu, WireToSurface1Pdu,
    WireToSurface2Pdu,
};
use rdp123_core::gfx::GfxHandler;
use rdp123_core::SharedFramebuffer;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(first) = args.next() else {
        eprintln!("usage: egfx_replay <capture> [out-dir] [--every N] | --clear <fail-bin>");
        std::process::exit(2);
    };

    if first == "--clear" {
        let path = args.next().expect("--clear needs a file");
        replay_failed_clearcodec(&path);
        return;
    }

    let mut out_dir = std::path::PathBuf::from(".");
    let mut every: u32 = 0;
    let mut probes: Vec<(u32, u32)> = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--every" {
            every = args
                .next()
                .and_then(|v| v.parse().ok())
                .expect("--every needs a number");
        } else if arg == "--probe" {
            let spec = args.next().expect("--probe needs x,y");
            let (x, y) = spec.split_once(',').expect("--probe format: x,y");
            probes.push((x.parse().expect("probe x"), y.parse().expect("probe y")));
        } else {
            out_dir = std::path::PathBuf::from(arg);
        }
    }
    std::fs::create_dir_all(&out_dir).expect("create out dir");

    let mut data = Vec::new();
    std::fs::File::open(&first)
        .expect("open capture")
        .read_to_end(&mut data)
        .expect("read capture");

    let framebuffer = SharedFramebuffer::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut handler = GfxHandler::new(framebuffer.clone(), tx, true);

    let mut r = Reader {
        data: &data,
        off: 0,
    };
    let mut record = 0u32;
    let mut probe_colors: Vec<Option<(u8, u8, u8)>> = Vec::new();
    while r.remaining() > 0 {
        let kind = r.u8();
        match kind {
            0x01 => {
                let surface_id = r.u16();
                let codec = r.u8();
                let rect = r.rect();
                let len = r.u32() as usize;
                let payload = r.bytes(len).to_vec();
                let codec_id = match codec {
                    0x0 => Codec1Type::Uncompressed,
                    0x3 => Codec1Type::RemoteFx,
                    0x8 => Codec1Type::ClearCodec,
                    0xa => Codec1Type::Planar,
                    0xc => Codec1Type::Alpha,
                    other => {
                        println!("record {record}: unknown codec1 0x{other:x}, skipping");
                        record += 1;
                        continue;
                    }
                };
                println!(
                    "record {record}: W2S1 {codec_id:?} surface {surface_id} rect {:?} {len} bytes",
                    (rect.left, rect.top, rect.right, rect.bottom)
                );
                handler.on_unhandled_pdu(&GfxPdu::WireToSurface1(WireToSurface1Pdu {
                    surface_id,
                    codec_id,
                    pixel_format: PixelFormat::XRgb,
                    destination_rectangle: rect,
                    bitmap_data: payload,
                }));
            }
            0x02 => {
                let surface_id = r.u16();
                let context = r.u32();
                let _sw = r.u16();
                let _sh = r.u16();
                let len = r.u32() as usize;
                let payload = r.bytes(len).to_vec();
                println!(
                    "record {record}: W2S2 surface {surface_id} context {context} {len} bytes"
                );
                handler.on_wire_to_surface2(&WireToSurface2Pdu {
                    surface_id,
                    codec_id: Codec2Type::RemoteFxProgressive,
                    codec_context_id: context,
                    pixel_format: PixelFormat::XRgb,
                    bitmap_data: payload,
                });
            }
            0x03 => {
                let surface_id = r.u16();
                let (b, g, red) = (r.u8(), r.u8(), r.u8());
                let count = r.u16();
                let rectangles: Vec<_> = (0..count).map(|_| r.rect()).collect();
                println!("record {record}: SolidFill surface {surface_id} {count} rects rgb({red},{g},{b})");
                handler.on_solid_fill(&SolidFillPdu {
                    surface_id,
                    fill_pixel: Color {
                        b,
                        g,
                        r: red,
                        xa: 0xFF,
                    },
                    rectangles,
                });
            }
            0x04 => {
                let source_surface_id = r.u16();
                let destination_surface_id = r.u16();
                let source_rectangle = r.rect();
                let count = r.u16();
                let destination_points: Vec<_> = (0..count)
                    .map(|_| Point {
                        x: r.u16(),
                        y: r.u16(),
                    })
                    .collect();
                println!(
                    "record {record}: SurfaceToSurface {source_surface_id}->{destination_surface_id} src {:?} -> {:?}",
                    (
                        source_rectangle.left,
                        source_rectangle.top,
                        source_rectangle.right,
                        source_rectangle.bottom
                    ),
                    destination_points
                        .iter()
                        .map(|p| (p.x, p.y))
                        .collect::<Vec<_>>()
                );
                handler.on_surface_to_surface(&SurfaceToSurfacePdu {
                    source_surface_id,
                    destination_surface_id,
                    source_rectangle,
                    destination_points,
                });
            }
            0x05 => {
                let surface_id = r.u16();
                let cache_slot = r.u16();
                let source_rectangle = r.rect();
                println!("record {record}: SurfaceToCache surface {surface_id} slot {cache_slot}");
                handler.on_surface_to_cache(&SurfaceToCachePdu {
                    surface_id,
                    cache_key: 0,
                    cache_slot,
                    source_rectangle,
                });
            }
            0x06 => {
                let cache_slot = r.u16();
                let surface_id = r.u16();
                let count = r.u16();
                let destination_points: Vec<_> = (0..count)
                    .map(|_| Point {
                        x: r.u16(),
                        y: r.u16(),
                    })
                    .collect();
                println!(
                    "record {record}: CacheToSurface slot {cache_slot} -> surface {surface_id} {:?}",
                    destination_points
                        .iter()
                        .map(|p| (p.x, p.y))
                        .collect::<Vec<_>>()
                );
                handler.on_cache_to_surface(&CacheToSurfacePdu {
                    cache_slot,
                    surface_id,
                    destination_points,
                });
            }
            0x07 => {
                let (w, h) = (r.u32(), r.u32());
                println!("record {record}: ResetGraphics {w}x{h}");
                handler.on_reset_graphics(w, h);
            }
            0x08 => {
                let id = r.u16();
                let (w, h) = (r.u16(), r.u16());
                let mapped = r.u8() != 0;
                let (ox, oy) = (r.u32(), r.u32());
                println!("record {record}: SurfaceCreated {id} {w}x{h} mapped={mapped}");
                handler.replay_surface_created(id, w, h, mapped.then_some((ox, oy)));
            }
            0x09 => {
                let id = r.u16();
                println!("record {record}: SurfaceDeleted {id}");
                handler.on_surface_deleted(id);
            }
            0x0A => {
                let id = r.u16();
                let (ox, oy) = (r.u32(), r.u32());
                println!("record {record}: SurfaceMapped {id} at ({ox},{oy})");
                handler.on_surface_mapped(id, ox, oy);
            }
            0x0B => {
                let surface_id = r.u16();
                let rect = r.rect();
                let (dw, dh) = (r.u16(), r.u16());
                let len = r.u32() as usize;
                let payload = r.bytes(len).to_vec();
                println!(
                    "record {record}: BitmapUpdated surface {surface_id} {dw}x{dh} {len} bytes"
                );
                handler.replay_bitmap_updated(surface_id, &rect, &payload, dw, dh);
            }
            0x0C => {
                let cache_slot = r.u16();
                println!("record {record}: EvictCacheEntry slot {cache_slot}");
                handler.on_evict_cache_entry(&EvictCacheEntryPdu { cache_slot });
            }
            other => {
                eprintln!(
                    "record {record}: unknown kind 0x{other:02x} at offset {}; stopping",
                    r.off
                );
                break;
            }
        }
        record += 1;
        if every > 0 && record.is_multiple_of(every) {
            write_framebuffer(
                &framebuffer,
                &out_dir.join(format!("replay-{record:04}.ppm")),
            );
        }
        if !probes.is_empty() {
            let colors: Vec<Option<(u8, u8, u8)>> = probes
                .iter()
                .map(|&(x, y)| {
                    framebuffer
                        .with_pixels(|pixels, width, _| {
                            let idx = (y as usize * width as usize + x as usize) * 4;
                            pixels.get(idx..idx + 3).map(|p| (p[2], p[1], p[0]))
                        })
                        .flatten()
                })
                .collect();
            if colors != probe_colors {
                for (i, (&(x, y), c)) in probes.iter().zip(&colors).enumerate() {
                    if probe_colors.get(i) != Some(c) {
                        println!("  PROBE ({x},{y}) -> {c:?} after record {}", record - 1);
                    }
                }
                probe_colors = colors;
            }
        }
    }

    // Drain events (unused, but keeps the channel from backing up conceptually).
    while rx.try_recv().is_ok() {}

    write_framebuffer(&framebuffer, &out_dir.join("replay-final.ppm"));
}

fn write_framebuffer(framebuffer: &SharedFramebuffer, path: &std::path::Path) {
    let Some((pixels, width, height)) = framebuffer.snapshot() else {
        eprintln!("framebuffer empty; nothing to write");
        return;
    };
    let mut out = Vec::with_capacity(pixels.len() / 4 * 3 + 32);
    out.extend_from_slice(format!("P6\n{width} {height}\n255\n").as_bytes());
    for px in pixels.as_chunks::<4>().0 {
        out.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    std::fs::write(path, out).expect("write ppm");
    println!("wrote {}", path.display());
}

/// Decode one dumped failing ClearCodec stream (from `RDP123_DEBUG_DUMP`):
/// 2 bytes width + 2 bytes height + payload.
fn replay_failed_clearcodec(path: &str) {
    let mut data = Vec::new();
    std::fs::File::open(path)
        .expect("open fail-bin")
        .read_to_end(&mut data)
        .expect("read fail-bin");
    assert!(data.len() >= 4, "file too short");
    let w = u16::from_le_bytes([data[0], data[1]]);
    let h = u16::from_le_bytes([data[2], data[3]]);
    let payload = &data[4..];
    println!("stream: {w}x{h}, {} payload bytes", payload.len());
    println!("head: {:02x?}", &payload[..payload.len().min(48)]);
    let mut decoder = ClearCodecDecoder::new();
    match decoder.decode(payload, w, h) {
        Ok(bgra) => println!("decode OK ({} bytes BGRA)", bgra.len()),
        Err(e) => println!("DECODE ERROR: {e}"),
    }
}

struct Reader<'a> {
    data: &'a [u8],
    off: usize,
}

impl Reader<'_> {
    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.off)
    }
    fn u8(&mut self) -> u8 {
        let v = self.data[self.off];
        self.off += 1;
        v
    }
    fn u16(&mut self) -> u16 {
        let v = u16::from_le_bytes([self.data[self.off], self.data[self.off + 1]]);
        self.off += 2;
        v
    }
    fn u32(&mut self) -> u32 {
        let v = u32::from_le_bytes([
            self.data[self.off],
            self.data[self.off + 1],
            self.data[self.off + 2],
            self.data[self.off + 3],
        ]);
        self.off += 4;
        v
    }
    fn rect(&mut self) -> ExclusiveRectangle {
        ExclusiveRectangle {
            left: self.u16(),
            top: self.u16(),
            right: self.u16(),
            bottom: self.u16(),
        }
    }
    fn bytes(&mut self, len: usize) -> &[u8] {
        let v = &self.data[self.off..self.off + len];
        self.off += len;
        v
    }
}
